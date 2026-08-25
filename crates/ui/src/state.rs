//! App state: the engine connection, entity lists, and the selected chat's
//! transcript — one gpui [`Entity`] the whole shell renders from.
//!
//! ## EngineHandle
//! The UI talks the same typed RPC whether the engine is in-process or a separate
//! daemon (ARCHITECTURE §1). [`EngineHandle::bootstrap`] probes the localhost IPC
//! port, mirroring comet: if an engine is listening it connects over WebSocket
//! ([`RemoteEngine`]); otherwise it embeds one via [`EngineCore::assemble`] and an
//! in-memory RPC transport ([`InProcessEngine`]) — same envelopes, same dispatch.
//!
//! ## Async bridging
//! `bootstrap` runs on tokio via `gpui_tokio::Tokio::spawn`. Once an [`RpcClient`]
//! exists, its `call`/`subscribe` futures are runtime-agnostic (tokio channels),
//! so subscription pumps run on gpui's own executor via `cx.spawn` and fold each
//! frame into the entity with `this.update(...)` + `cx.notify()`.
//!
//! Pure logic (sort order, staleness, gate phase) lives in free functions with
//! unit tests; rendering reads them.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use gpui::{App, Context, Entity, Task};
use gpui_tokio::Tokio;
use serde::de::DeserializeOwned;

use comet_client::{Federation, FederationCommand, FederationEvent, ServerState};
use comet_doc::{SessionMessageEntry, TranscriptDesync, TranscriptFrame};
use comet_engine::{Engine, EngineConfig, EngineRuntime};
use comet_proto::{
    Chat, ChatIndicator, Device, HarnessId, RemoteConnectionState, ServerId, ServerRef, Session,
    Space,
};
use comet_rpc::{RpcClient, RpcError, RpcService, connect_ws, memory_client, methods};

use crate::comments::DiffComment;
use crate::errors;
use crate::remotes::{
    AddRemoteRequest, InstallationRemotePairer, RemoteAddCoordinator, RemoteAddState,
    run_remote_add_operation,
};

// ---------------------------------------------------------------------------
// Engine handle
// ---------------------------------------------------------------------------

/// Everything needed to reach (or start) an engine.
#[derive(Debug, Clone)]
pub struct EngineBootConfig {
    /// Data directory for the embedded engine (`~/.comet-native`).
    pub data_dir: PathBuf,
    /// Localhost IPC port to probe / serve.
    pub ipc_port: u16,
    /// Release metadata/download origin; independent from runtime authority.
    pub releases_url: String,
    /// Harness for doc-command runs until per-chat config lands (M4).
    pub default_harness: HarnessId,
}

/// How this UI reached its engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineMode {
    /// Engine embedded in this process (in-memory RPC transport).
    InProcess,
    /// Connected to a separate daemon over localhost WebSocket.
    Remote { url: String },
}

/// One of the two ways to own an engine connection. Both end at an [`RpcClient`]
/// speaking the identical protocol — the trait only differs in provenance and
/// teardown.
#[async_trait]
trait EngineBackend: Send + Sync {
    fn client(&self) -> Arc<RpcClient>;
    fn mode(&self) -> EngineMode;
    /// Graceful teardown (drains runs / flushes docs for the in-process engine).
    async fn shutdown(&self);
}

/// Embedded engine: owns the [`EngineCore`] and an in-memory RPC loop.
struct InProcessEngine {
    runtime: Arc<tokio::sync::Mutex<Option<EngineRuntime>>>,
    /// Serves this engine to other viewports over the IPC port. `None` when the
    /// port was already taken — the window still works over its own transport.
    ipc_task: Option<tokio::task::JoinHandle<()>>,
    client: Arc<RpcClient>,
}

#[async_trait]
impl EngineBackend for InProcessEngine {
    fn client(&self) -> Arc<RpcClient> {
        self.client.clone()
    }
    fn mode(&self) -> EngineMode {
        EngineMode::InProcess
    }
    async fn shutdown(&self) {
        // Stop accepting first: a viewport must not connect midway through the
        // drain and queue work against stores that are closing.
        if let Some(ipc) = &self.ipc_task {
            ipc.abort();
        }
        if let Some(runtime) = self.runtime.lock().await.take() {
            runtime.shutdown().await;
        }
    }
}

/// External daemon over `ws://127.0.0.1:{port}`.
struct RemoteEngine {
    client: Arc<RpcClient>,
    url: String,
}

#[async_trait]
impl EngineBackend for RemoteEngine {
    fn client(&self) -> Arc<RpcClient> {
        self.client.clone()
    }
    fn mode(&self) -> EngineMode {
        EngineMode::Remote {
            url: self.url.clone(),
        }
    }
    async fn shutdown(&self) {
        // The daemon outlives this viewport; nothing to tear down.
    }
}

/// Cheaply clonable handle to whichever backend won the probe.
#[derive(Clone)]
pub struct EngineHandle {
    inner: Arc<dyn EngineBackend>,
}

/// Cloneable command side of the client federation. UI effects use this for
/// every resource operation; raw object ids remain inside `params` while the
/// authoritative server id travels out-of-band.
#[derive(Clone)]
pub struct FederatedClient {
    commands: tokio::sync::mpsc::UnboundedSender<FederationCommand>,
}

#[derive(Clone)]
pub struct ServerClient {
    federation: FederatedClient,
    server_id: ServerId,
}

impl ServerClient {
    pub fn client(&self) -> &Self {
        self
    }

    pub async fn call(
        &self,
        method: &'static str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, RpcError> {
        self.federation
            .request(self.server_id.clone(), method, params)
            .await
    }

    pub async fn call_as<T: serde::de::DeserializeOwned>(
        &self,
        method: &'static str,
        params: serde_json::Value,
    ) -> Result<T, RpcError> {
        self.federation
            .request_as(self.server_id.clone(), method, params)
            .await
    }

    pub fn server_id(&self) -> &ServerId {
        &self.server_id
    }

    pub async fn subscribe(
        &self,
        method: &'static str,
        params: serde_json::Value,
    ) -> Result<comet_client::FederationStream, RpcError> {
        self.federation
            .subscribe(self.server_id.clone(), method, params)
            .await
    }
}

impl FederatedClient {
    pub fn reconnect(&self, server_id: ServerId) {
        let _ = self.commands.send(FederationCommand::Reconnect(server_id));
    }

    pub fn call(&self, server_id: ServerId, method: &'static str, params: serde_json::Value) {
        let _ = self.commands.send(FederationCommand::Call {
            server_id,
            method,
            params,
        });
    }

    pub async fn request(
        &self,
        server_id: ServerId,
        method: &'static str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, RpcError> {
        let (reply, received) = tokio::sync::oneshot::channel();
        self.commands
            .send(FederationCommand::Request {
                server_id,
                method,
                params,
                reply,
            })
            .map_err(|_| RpcError::Closed)?;
        received.await.unwrap_or(Err(RpcError::Closed))
    }

    pub async fn request_as<T: serde::de::DeserializeOwned>(
        &self,
        server_id: ServerId,
        method: &'static str,
        params: serde_json::Value,
    ) -> Result<T, RpcError> {
        serde_json::from_value(self.request(server_id, method, params).await?)
            .map_err(|error| RpcError::Failed(error.to_string()))
    }

    pub async fn subscribe(
        &self,
        server_id: ServerId,
        method: &'static str,
        params: serde_json::Value,
    ) -> Result<comet_client::FederationStream, RpcError> {
        let (reply, received) = tokio::sync::oneshot::channel();
        self.commands
            .send(FederationCommand::Subscribe {
                server_id,
                method,
                params,
                reply,
            })
            .map_err(|_| RpcError::Closed)?;
        received.await.unwrap_or(Err(RpcError::Closed))
    }

    fn watch_transcript(&self, chat: Option<ServerRef>) {
        let _ = self.commands.send(FederationCommand::WatchTranscript(chat));
    }
}

impl EngineHandle {
    /// Probe the IPC port and connect (daemon listening) or embed (nothing there).
    /// Must run on the tokio runtime (`Tokio::spawn`): both transports spawn
    /// tokio tasks.
    pub async fn bootstrap(config: EngineBootConfig) -> anyhow::Result<EngineHandle> {
        let url = format!("ws://127.0.0.1:{}", config.ipc_port);
        let probe = tokio::time::timeout(
            std::time::Duration::from_millis(750),
            tokio::net::TcpStream::connect(("127.0.0.1", config.ipc_port)),
        )
        .await;
        if matches!(probe, Ok(Ok(_))) {
            tracing::info!(%url, "engine daemon detected; connecting");
            match connect_ws(&url).await {
                Ok(client) => {
                    return Ok(EngineHandle {
                        inner: Arc::new(RemoteEngine {
                            client: Arc::new(client),
                            url,
                        }),
                    });
                }
                // Something is on the port but it is not an engine (or it is
                // wedged). Fall through and embed: a stranger holding 27654
                // should cost other viewports, not this window.
                Err(err) => tracing::warn!(%url, error = %err, "not an engine; embedding instead"),
            }
        }

        tracing::info!(data_dir = %config.data_dir.display(), "no daemon on port; embedding engine");
        let engine_config = EngineConfig {
            data_dir: config.data_dir,
            ipc_port: config.ipc_port,
            default_harness: config.default_harness,
            releases_url: config.releases_url,
            // `EngineBootConfig` has no timeout knob of its own; reading the
            // env var here rather than hard-coding the default is what lets
            // an operator's override reach the embedded (headed) engine too.
            unattended_timeout: comet_engine::unattended_timeout_from_env(),
        };
        let engine_runtime = Engine::assemble_runtime(&engine_config).await?;
        let service: Arc<dyn RpcService> = engine_runtime.core().rpc_service();
        let client = memory_client(service.clone());

        // Serve the same service on the IPC port so local administration
        // clients can attach to this window's engine with no setup.
        // Best-effort — losing the bind race with another engine costs other
        // local clients, not this one.
        let ipc_task = match comet_engine::serve_ipc(engine_config.ipc_port, service).await {
            Ok(task) => Some(task),
            Err(err) => {
                tracing::warn!(
                    port = engine_config.ipc_port,
                    error = %err,
                    "IPC port unavailable; other viewports cannot attach to this window"
                );
                None
            }
        };
        let runtime = Arc::new(tokio::sync::Mutex::new(Some(engine_runtime)));
        Ok(EngineHandle {
            inner: Arc::new(InProcessEngine {
                runtime,
                ipc_task,
                client: Arc::new(client),
            }),
        })
    }

    pub fn client(&self) -> Arc<RpcClient> {
        self.inner.client()
    }

    pub fn mode(&self) -> EngineMode {
        self.inner.mode()
    }

    pub async fn shutdown(&self) {
        self.inner.shutdown().await;
    }
}

// ---------------------------------------------------------------------------
// Pure state + reducers
// ---------------------------------------------------------------------------

// The frontend-agnostic derivations (sort orders, staleness gating, sidebar
// grouping, the boot gate, relative times) live in `comet_proto::view`, pure
// and with their own test suite. Re-exported here because every call site in
// this crate reads them as `state::…`.
pub use comet_proto::view::{
    ChatGroup, ConnectionStatus, GatePhase, Indicator, SESSION_STALE_MS, attention_rank,
    chat_location, display_status, effective_indicator, format_time_ago, gate_phase, group_chats,
    project_label, sort_active, sort_chats, sort_spaces, sort_tabs,
};

// AppState entity
// ---------------------------------------------------------------------------

/// Which sessions the sidebar lists.
///
/// Deliberately separate from [`AppState::selected_space`], which drives the
/// main area and tab strip. `selected_space` cannot express "all": a spaces
/// frame forces it to `Some(first)` when it is `None` (`apply_spaces`), and
/// selecting a chat sets it from that chat (`select_chat`) — so overloading it
/// would silently drop the user out of the all-spaces view on any click.
///
/// Changing the scope never changes what is open.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SidebarScope {
    /// Every space on the active server — the historical behaviour, and the
    /// default.
    #[default]
    All,
    /// One space, by local id.
    Space(String),
}

impl SidebarScope {
    pub fn space_id(&self) -> Option<&str> {
        match self {
            SidebarScope::All => None,
            SidebarScope::Space(id) => Some(id.as_str()),
        }
    }
}

/// Root application state. Reducer methods (`apply_*`, [`Self::session_for`], …)
/// are plain `&mut self` functions so tests construct the struct directly; gpui
/// glue ([`Self::bootstrap`], [`Self::select_chat`]) layers subscriptions on top.
pub struct AppState {
    pub connection: ConnectionStatus,
    pub devices: Vec<Device>,
    /// Sorted (see [`sort_spaces`]).
    pub spaces: Vec<Space>,
    /// Sorted (see [`sort_chats`]); includes archived rows — views filter.
    pub chats: Vec<Chat>,
    pub sessions: Vec<Session>,
    /// Authoritative per-server snapshots in federation registry order. The
    /// flat vectors above are only the active server's render projection.
    pub servers: HashMap<ServerId, ServerState>,
    pub server_order: Vec<ServerId>,
    active_server: Option<ServerId>,
    /// The space whose tabs fill the main area. Healed by [`Self::apply_spaces`]
    /// when the row vanishes; selecting a chat implies its space.
    pub selected_space: Option<ServerRef>,
    /// Sidebar session scope — see [`SidebarScope`]. Independent of
    /// `selected_space`.
    pub sidebar_scope: SidebarScope,
    pub selected_chat: Option<ServerRef>,
    /// Boot auto-select happened (or a manual selection superseded it).
    pub auto_selected: bool,
    /// Joined transcript of the selected chat (continuations folded engine-side).
    pub transcript: Vec<SessionMessageEntry>,
    /// Optimistic user echoes per chat id, shown until the doc frame carrying
    /// the same message id arrives (client-minted ids make dedup exact).
    echoes: HashMap<ServerRef, Vec<SessionMessageEntry>>,
    /// Diff comments staged per composer key. The changes pane writes them,
    /// the composer reads them for its chip and folds them into the next
    /// prompt — AppState is the only thing both views already share.
    diff_comments: HashMap<ServerRef, Vec<DiffComment>>,
    /// This engine's device id (best-effort `LocalDevice` probe; `None` until
    /// the engine serves it — views degrade gracefully).
    pub local_device_id: Option<String>,
    /// Latest `UpdateStatus` frame — drives the sidebar update strip.
    pub update: Option<comet_update::UpdateStatus>,
    /// Data directory (`ui-settings.json`, `composer-defaults.json`); set at
    /// bootstrap so child views can persist small preference files.
    pub data_dir: Option<PathBuf>,
    engine: Option<EngineHandle>,
    federation: Option<FederatedClient>,
    watch_tasks: Vec<Task<()>>,
    remote_add: RemoteAddCoordinator,
    remote_add_task: Option<Task<()>>,
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

impl AppState {
    pub fn new() -> Self {
        Self {
            connection: ConnectionStatus::Connecting,
            devices: Vec::new(),
            spaces: Vec::new(),
            chats: Vec::new(),
            sessions: Vec::new(),
            servers: HashMap::new(),
            server_order: Vec::new(),
            active_server: None,
            selected_space: None,
            sidebar_scope: SidebarScope::All,
            selected_chat: None,
            transcript: Vec::new(),
            echoes: HashMap::new(),
            diff_comments: HashMap::new(),
            local_device_id: None,
            update: None,
            data_dir: None,
            engine: None,
            federation: None,
            watch_tasks: Vec::new(),
            remote_add: RemoteAddCoordinator::default(),
            remote_add_task: None,
            auto_selected: false,
        }
    }

    // ---- diff comments (pure) ----

    /// The selected, fully-qualified owner of any draft or staged comment.
    /// New-chat canvases deliberately have no owner and cannot stage comments.
    pub fn composer_key(&self) -> Option<ServerRef> {
        self.selected_chat.clone()
    }

    pub fn diff_comments(&self, key: &ServerRef) -> &[DiffComment] {
        self.diff_comments
            .get(key)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    pub fn add_diff_comment(&mut self, key: &ServerRef, comment: DiffComment) {
        self.diff_comments
            .entry(key.clone())
            .or_default()
            .push(comment);
    }

    pub fn remove_diff_comment(&mut self, key: &ServerRef, id: &str) {
        if let Some(list) = self.diff_comments.get_mut(key) {
            list.retain(|c| c.id != id);
            if list.is_empty() {
                self.diff_comments.remove(key);
            }
        }
    }

    /// Snapshot-and-clear on send (`attachments` does the same): the chip
    /// empties the instant the prompt carrying the comments goes out.
    pub fn take_diff_comments(&mut self, key: &ServerRef) -> Vec<DiffComment> {
        self.diff_comments.remove(key).unwrap_or_default()
    }

    /// Restore a failed send ahead of comments staged while it was in flight.
    /// IDs keep the merge idempotent if a completion path is observed twice.
    pub fn restore_diff_comments(&mut self, key: &ServerRef, mut taken: Vec<DiffComment>) {
        let current = self.diff_comments.remove(key).unwrap_or_default();
        for comment in current {
            if !taken.iter().any(|saved| saved.id == comment.id) {
                taken.push(comment);
            }
        }
        if !taken.is_empty() {
            self.diff_comments.insert(key.clone(), taken);
        }
    }

    /// Drop a deleted chat's stage — its comments could never be sent again.
    pub fn purge_diff_comments(&mut self, key: &ServerRef) {
        self.diff_comments.remove(key);
    }

    // ---- reducers (pure) ----

    /// Fold one federation event into authoritative server buckets. Any
    /// non-online snapshot is already child-free by contract; selected remote
    /// loss heals to the local (first) bucket and cannot retain stale content.
    pub fn apply_federation(&mut self, event: FederationEvent) {
        match event {
            FederationEvent::ServerChanged(mut server) => {
                if server.connection != RemoteConnectionState::Online {
                    crate::attachments::purge_server_attachments(&server.id);
                    server.devices.clear();
                    server.spaces.clear();
                    server.chats.clear();
                    server.sessions.clear();
                } else {
                    crate::attachments::mark_server_attachments_online(&server.id);
                }
                let id = server.id.clone();
                if !self.servers.contains_key(&id) {
                    self.server_order.push(id.clone());
                }
                self.servers.insert(id.clone(), server);
                if self.active_server.is_none() {
                    self.active_server = self.server_order.first().cloned();
                }
                if self.active_server.as_ref() == Some(&id)
                    && self
                        .servers
                        .get(&id)
                        .is_some_and(|s| s.connection != RemoteConnectionState::Online)
                {
                    self.heal_to_local();
                } else if self.active_server.as_ref() == Some(&id) {
                    self.project_active_server();
                    self.heal_resource_selection();
                }
            }
            FederationEvent::ServerRemoved(id) => {
                crate::attachments::purge_server_attachments(&id);
                self.purge_server(&id);
                self.servers.remove(&id);
                self.server_order.retain(|candidate| candidate != &id);
                if self.active_server.as_ref() == Some(&id) {
                    self.heal_to_local();
                }
            }
            FederationEvent::Transcript { chat, entries } => {
                if self.selected_chat.as_ref() == Some(&chat) {
                    self.apply_transcript(entries);
                }
            }
            FederationEvent::Notice { server_id, message } => {
                tracing::warn!(server = ?server_id, %message, "federation notice");
            }
        }
    }

    fn purge_server(&mut self, server_id: &ServerId) {
        self.echoes.retain(|key, _| &key.server_id != server_id);
        if self
            .selected_chat
            .as_ref()
            .is_some_and(|key| &key.server_id == server_id)
        {
            self.clear_transcript_ownership();
        }
        if self
            .selected_space
            .as_ref()
            .is_some_and(|key| &key.server_id == server_id)
        {
            self.selected_space = None;
        }
    }

    fn clear_transcript_ownership(&mut self) {
        if self.selected_chat.is_some()
            && let Some(client) = &self.federation
        {
            client.watch_transcript(None);
        }
        self.selected_chat = None;
        self.transcript.clear();
    }

    fn heal_to_local(&mut self) {
        if let Some(previous) = self.active_server.clone() {
            self.purge_server(&previous);
        }
        self.active_server = self.server_order.first().cloned();
        self.selected_chat = None;
        self.selected_space = None;
        self.transcript.clear();
        self.project_active_server();
    }

    fn project_active_server(&mut self) {
        let Some(server) = self
            .active_server
            .as_ref()
            .and_then(|id| self.servers.get(id))
        else {
            self.devices.clear();
            self.spaces.clear();
            self.chats.clear();
            self.sessions.clear();
            self.sidebar_scope = SidebarScope::All;
            return;
        };
        self.devices = server.devices.clone();
        self.spaces = server.spaces.clone();
        sort_spaces(&mut self.spaces);
        self.chats = server.chats.clone();
        sort_chats(&mut self.chats);
        self.sessions = server.sessions.clone();
        self.heal_sidebar_scope();
    }

    /// A scoped sidebar pointing at a space that is no longer projected falls
    /// back to `All`. Covers both a space deleted elsewhere and a change of
    /// active server (whose projection replaces `self.spaces` wholesale).
    fn heal_sidebar_scope(&mut self) {
        if let SidebarScope::Space(id) = &self.sidebar_scope
            && !self.spaces.iter().any(|s| s.id == *id)
        {
            self.sidebar_scope = SidebarScope::All;
        }
    }

    fn heal_resource_selection(&mut self) {
        if self
            .selected_chat
            .as_ref()
            .is_some_and(|selected| !self.chats.iter().any(|chat| chat.id == selected.local_id))
        {
            self.clear_transcript_ownership();
        }
        if self.selected_space.as_ref().is_some_and(|selected| {
            !self
                .spaces
                .iter()
                .any(|space| space.id == selected.local_id)
        }) {
            self.selected_space = self.spaces.first().and_then(|space| {
                self.active_server
                    .clone()
                    .map(|server| ServerRef::new(server, space.id.clone()))
            });
        }
    }

    pub fn selected_server_id(&self) -> Option<&ServerId> {
        self.active_server.as_ref()
    }

    pub fn call_for(
        &self,
        owner: &ServerRef,
        method: &'static str,
        params: serde_json::Value,
    ) -> FederationCommand {
        FederationCommand::Call {
            server_id: owner.server_id.clone(),
            method,
            params,
        }
    }

    pub fn subscribe_for(
        &self,
        owner: &ServerRef,
        method: &'static str,
        params: serde_json::Value,
        reply: tokio::sync::oneshot::Sender<Result<comet_client::FederationStream, RpcError>>,
    ) -> FederationCommand {
        FederationCommand::Subscribe {
            server_id: owner.server_id.clone(),
            method,
            params,
            reply,
        }
    }

    pub fn selected_chat_id(&self) -> Option<&str> {
        self.selected_chat.as_ref().map(ServerRef::local_id)
    }

    pub fn selected_space_id(&self) -> Option<&str> {
        self.selected_space.as_ref().map(ServerRef::local_id)
    }

    pub fn select_server_chat(&mut self, chat: ServerRef) {
        self.active_server = Some(chat.server_id.clone());
        self.project_active_server();
        self.selected_space = self
            .chats
            .iter()
            .find(|row| row.id == chat.local_id)
            .and_then(|row| row.space_id.clone())
            .map(|space| ServerRef::new(chat.server_id.clone(), space));
        self.selected_chat = Some(chat);
        self.auto_selected = true;
        self.transcript.clear();
    }

    pub fn select_server_bucket(&mut self, server_id: ServerId) {
        if self
            .servers
            .get(&server_id)
            .is_none_or(|server| server.connection != RemoteConnectionState::Online)
        {
            return;
        }
        self.active_server = Some(server_id);
        self.selected_space = None;
        self.clear_transcript_ownership();
        self.project_active_server();
    }

    pub fn apply_chats(&mut self, mut chats: Vec<Chat>) {
        sort_chats(&mut chats);
        self.chats = chats;
        if let Some(selected) = &self.selected_chat
            && !self.chats.iter().any(|c| c.id == selected.local_id)
        {
            // Selected chat vanished (deleted elsewhere): drop selection + transcript.
            self.selected_chat = None;
            self.transcript.clear();
        }
    }

    pub fn apply_sessions(&mut self, sessions: Vec<Session>) {
        self.sessions = sessions;
    }

    pub fn apply_spaces(&mut self, mut spaces: Vec<Space>) {
        sort_spaces(&mut spaces);
        self.spaces = spaces;
        // Heal a vanished selection (space deleted elsewhere): fall back to the
        // first space; its chats died with it, so a matching chat selection is
        // healed by the accompanying chats frame (`apply_chats`).
        if let Some(selected) = &self.selected_space
            && !self.spaces.iter().any(|s| s.id == selected.local_id)
        {
            self.selected_space = self
                .spaces
                .first()
                .map(|s| ServerRef::new(self.current_server_id(), s.id.clone()));
        }
        // First frame with no selection yet: pick the first space so the shell
        // never renders an empty main area while spaces exist.
        if self.selected_space.is_none() {
            self.selected_space = self
                .spaces
                .first()
                .map(|s| ServerRef::new(self.current_server_id(), s.id.clone()));
        }
        self.heal_sidebar_scope();
    }

    /// Optimistic local echo of a `setChatConfig` mutate: stamp the row now so
    /// the chips update on click; the next chats watch frame carries the same
    /// value once the engine applies the LWW write.
    pub fn apply_chat_config(&mut self, chat_id: &str, config: comet_proto::ChatConfig) {
        if let Some(chat) = self.chats.iter_mut().find(|c| c.id == chat_id) {
            chat.config = Some(config);
        }
    }

    pub fn apply_devices(&mut self, devices: Vec<Device>) {
        self.devices = devices;
    }

    pub fn apply_update(&mut self, status: comet_update::UpdateStatus) {
        self.update = Some(status);
    }

    pub fn apply_transcript(&mut self, entries: Vec<SessionMessageEntry>) {
        // Doc frames supersede optimistic echoes carrying the same id.
        if let Some(chat_id) = self.selected_chat.as_ref()
            && let Some(echoes) = self.echoes.get_mut(chat_id)
        {
            echoes.retain(|echo| !entries.iter().any(|e| e.id == echo.id));
        }
        self.transcript = entries;
    }

    /// Apply a `WatchDocMessages` delta frame in place. `Err` = this copy has
    /// diverged; the watch task resubscribes for a fresh reset.
    pub fn apply_transcript_frame(
        &mut self,
        frame: TranscriptFrame,
    ) -> Result<(), TranscriptDesync> {
        comet_doc::apply_transcript_frame(&mut self.transcript, frame)?;
        if let Some(chat_id) = self.selected_chat.as_ref()
            && let Some(echoes) = self.echoes.get_mut(chat_id)
        {
            let transcript = &self.transcript;
            echoes.retain(|echo| !transcript.iter().any(|e| e.id == echo.id));
        }
        Ok(())
    }

    /// Add an optimistic user echo (composer send path).
    pub fn push_echo(&mut self, chat_id: &str, entry: SessionMessageEntry) {
        let key = self
            .selected_chat
            .as_ref()
            .filter(|selected| selected.local_id == chat_id)
            .cloned()
            .or_else(|| {
                self.active_server
                    .clone()
                    .map(|server| ServerRef::new(server, chat_id))
            });
        let Some(key) = key else {
            return;
        };
        self.push_echo_for(key, entry);
    }

    pub fn push_echo_for(&mut self, chat: ServerRef, entry: SessionMessageEntry) {
        let echoes = self.echoes.entry(chat).or_default();
        if !echoes.iter().any(|e| e.id == entry.id) {
            echoes.push(entry);
        }
    }

    /// Drop an echo (send failed — the prompt returns to the draft).
    pub fn remove_echo(&mut self, chat_id: &str, message_id: &str) {
        let key = self
            .selected_chat
            .as_ref()
            .filter(|selected| selected.local_id == chat_id)
            .cloned()
            .or_else(|| {
                self.active_server
                    .clone()
                    .map(|server| ServerRef::new(server, chat_id))
            });
        if let Some(key) = key {
            self.remove_echo_for(&key, message_id);
        }
    }

    pub fn remove_echo_for(&mut self, chat: &ServerRef, message_id: &str) {
        if let Some(echoes) = self.echoes.get_mut(chat) {
            echoes.retain(|e| e.id != message_id);
        }
    }

    /// Unconfirmed echoes for the selected chat, in send order.
    pub fn pending_echoes(&self) -> &[SessionMessageEntry] {
        self.selected_chat
            .as_ref()
            .and_then(|id| self.echoes.get(id))
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    // ---- queries ----

    /// Non-archived chats in sidebar order.
    pub fn visible_chats(&self) -> impl Iterator<Item = &Chat> {
        self.chats.iter().filter(|c| !c.archived)
    }

    pub fn selected_space_row(&self) -> Option<&Space> {
        let id = self.selected_space_id()?;
        self.spaces.iter().find(|s| s.id == id)
    }

    pub fn space_row(&self, space_id: &str) -> Option<&Space> {
        self.spaces.iter().find(|s| s.id == space_id)
    }

    pub fn space_for_chat(&self, chat: &Chat) -> Option<&Space> {
        self.space_row(chat.space_id.as_deref()?)
    }

    /// Non-archived chats of a space in tab (creation) order. Chats with a
    /// dangling/missing `space_id` are invisible by construction.
    pub fn chats_in_space(&self, space_id: &str) -> Vec<&Chat> {
        let mut chats: Vec<&Chat> = self
            .visible_chats()
            .filter(|c| c.space_id.as_deref() == Some(space_id))
            .collect();
        sort_tabs(&mut chats);
        chats
    }

    pub fn device_name(&self, device_id: &str) -> Option<&str> {
        self.devices
            .iter()
            .find(|d| d.id == device_id)
            .map(|d| d.name.as_str())
    }

    /// Host-presence check: is this device's 15s presence heartbeat fresh?
    /// Distinguishes "host offline" (its queued work syncs when it returns)
    /// from slow sync. The local device is trivially online; unknown devices
    /// get the benefit of the doubt (no evidence — don't cry wolf).
    pub fn device_online(&self, device_id: &str, now: DateTime<Utc>) -> bool {
        if self.local_device_id.as_deref() == Some(device_id) {
            return true;
        }
        match self.devices.iter().find(|d| d.id == device_id) {
            Some(d) => crate::settings::devices::device_online(d.last_seen_at, now),
            None => true,
        }
    }

    /// Does the selected space's folder have git? Drives the branch picker and
    /// the diff sidebar (owner-stamped, synced — no RPC).
    pub fn selected_space_git(&self) -> bool {
        self.selected_space_row().is_some_and(|s| s.git_detected)
    }

    /// Full display status for a chat (tab dots, Active list).
    pub fn display_status_for(&self, chat: &Chat, now: DateTime<Utc>) -> ChatIndicator {
        display_status(chat, self.session_for(&chat.id), now)
    }

    /// THE definition of "a session the sidebar can show": non-archived AND
    /// owned by a space that is currently projected. Scope-independent — a
    /// scoped list narrows this set, it never widens it.
    ///
    /// Every count, list, and per-space aggregate in the sidebar goes through
    /// here. Three call sites used to spell the condition out themselves and
    /// two got it wrong (the scope trigger's total and the panel's `All spaces`
    /// row counted bare `visible_chats`, the per-space attention map keyed off
    /// a raw `space_id`), so a chat with a `None`/dangling `space_id` was
    /// counted by the chrome and then never appeared in the list it claimed to
    /// summarise. `search::filter` applies the same rule to its own snapshot
    /// (it matches over `&[Space]`/`&[Chat]` slices, not `AppState`).
    pub fn listed_chats(&self) -> impl Iterator<Item = &Chat> {
        self.visible_chats()
            .filter(|c| self.space_for_chat(c).is_some())
    }

    /// The sidebar's Sessions list: [`Self::listed_chats`] narrowed to the
    /// scoped space (all of them on `All`), idle included, attention-sorted.
    pub fn overview_chats(&self, now: DateTime<Utc>) -> Vec<(ChatIndicator, &Chat)> {
        let scope = self.sidebar_scope.space_id();
        let mut rows: Vec<(ChatIndicator, &Chat)> = self
            .listed_chats()
            .filter(|c| scope.is_none_or(|scoped| c.space_id.as_deref() == Some(scoped)))
            .map(|c| (display_status(c, self.session_for(&c.id), now), c))
            .collect();
        sort_active(&mut rows);
        rows
    }

    /// The chat a jump shortcut opens: the row at `slot` (zero-based) of the
    /// sidebar's active list. A slot past the end of a short list opens
    /// nothing. Pure.
    ///
    /// Counting happens in [`Self::overview_chats`] itself rather than in a
    /// jump-only copy of the list, so the numbering can never drift from the
    /// rows on screen — that function already applies `sidebar_scope`, and it
    /// is the same call the sidebar builds its rows from.
    pub fn jump_target(&self, now: DateTime<Utc>, slot: usize) -> Option<String> {
        self.overview_chats(now)
            .get(slot)
            .map(|(_, chat)| chat.id.clone())
    }

    pub fn session_for(&self, chat_id: &str) -> Option<&Session> {
        self.sessions.iter().find(|s| s.chat_id == chat_id)
    }

    /// Staleness-checked status dot for a chat row.
    pub fn indicator_for(&self, chat_id: &str, now: DateTime<Utc>) -> Indicator {
        effective_indicator(self.session_for(chat_id), now)
    }

    pub fn selected_chat_row(&self) -> Option<&Chat> {
        let id = self.selected_chat_id()?;
        self.chats.iter().find(|c| c.id == id)
    }

    /// The chat the Archive session shortcut acts on: the selected one, unless
    /// it is already archived. The shortcut archives and never unarchives, so
    /// an archived chat is left alone. Pure.
    pub fn archivable_selected_chat(&self) -> Option<ServerRef> {
        if self.selected_chat_row()?.archived {
            return None;
        }
        self.selected_chat.clone()
    }

    pub fn gate(&self) -> GatePhase {
        gate_phase(&self.connection)
    }

    pub fn engine(&self) -> Option<&EngineHandle> {
        self.engine.as_ref()
    }

    pub fn local_rpc_client(&self) -> Option<Arc<RpcClient>> {
        self.engine.as_ref().map(EngineHandle::client)
    }

    pub fn remote_add_state(&self) -> RemoteAddState {
        self.remote_add.state()
    }

    pub fn start_remote_add(
        &mut self,
        request: AddRemoteRequest,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if matches!(self.remote_add.state(), RemoteAddState::InFlight) {
            return Err("A remote is already being added".into());
        }
        let local = self
            .engine()
            .map(|engine| engine.client())
            .ok_or_else(|| "Local engine is not connected".to_string())?;
        let data_dir = self
            .data_dir
            .clone()
            .ok_or_else(|| "Installation identity is unavailable".to_string())?;
        if !self.remote_add.begin() {
            return Err("A remote is already being added".into());
        }
        let coordinator = self.remote_add.clone();
        self.remote_add_task = Some(cx.spawn(async move |this, cx| {
            run_remote_add_operation(
                coordinator,
                InstallationRemotePairer,
                local,
                data_dir,
                request,
            )
            .await;
            this.update(cx, |_, cx| cx.notify()).ok();
        }));
        cx.notify();
        Ok(())
    }

    pub fn federation(&self) -> Option<&FederatedClient> {
        self.federation.as_ref()
    }

    pub fn client_for(&self, owner: &ServerRef) -> Option<ServerClient> {
        Some(ServerClient {
            federation: self.federation.clone()?,
            server_id: owner.server_id.clone(),
        })
    }

    /// Resolve a captured resource owner for a final mutation. Unlike
    /// `selected_client`, this never falls back to whichever server is active,
    /// and it rejects owners that disappeared or went offline after the UI was
    /// opened.
    pub fn mutation_client_for(&self, owner: &ServerRef) -> Result<ServerClient, RpcError> {
        let online = self
            .servers
            .get(&owner.server_id)
            .is_some_and(|server| server.connection == RemoteConnectionState::Online);
        if !online {
            return Err(RpcError::Failed("server is offline".into()));
        }
        self.client_for(owner)
            .ok_or_else(|| RpcError::Failed("engine not connected".into()))
    }

    pub fn selected_client(&self) -> Option<ServerClient> {
        Some(ServerClient {
            federation: self.federation.clone()?,
            server_id: self.current_server_id(),
        })
    }

    // ---- gpui glue ----

    /// Kick off (or retry) the engine bootstrap: probe → connect-or-embed on
    /// tokio, then attach subscriptions. Safe to call again after `Failed`.
    pub fn bootstrap(state: Entity<AppState>, config: EngineBootConfig, cx: &mut App) {
        let data_dir = config.data_dir.clone();
        state.update(cx, |s, cx| {
            s.connection = ConnectionStatus::Connecting;
            s.data_dir = Some(data_dir);
            cx.notify();
        });
        let boot = Tokio::spawn(cx, EngineHandle::bootstrap(config));
        cx.spawn(async move |cx| {
            let outcome = match boot.await {
                Ok(Ok(handle)) => Ok(handle),
                // Translate, never render: the gate showed this chain verbatim
                // — path, pid and env var — until `engine_start_failure`.
                Ok(Err(err)) => Err(errors::engine_start_failure(&err)),
                Err(join_err) => {
                    tracing::error!(error = %join_err, "engine bootstrap task failed");
                    Err("Comet's engine couldn't start.".to_string())
                }
            };
            // NB: at the pinned rev `Entity::update(&mut AsyncApp)` returns the
            // closure's value directly (no Result) — AsyncApp implements
            // AppContext like App does.
            state.update(cx, |s, cx| match outcome {
                Ok(handle) => s.attach_engine(handle, cx),
                Err(message) => {
                    // Already logged with its diagnostic detail at the
                    // translation site; `message` here is user-facing copy.
                    s.connection = ConnectionStatus::Failed(message);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Wire the connected engine: mark Ready and start the standing watches.
    /// Methods the engine doesn't serve yet (chats/devices/auth land with the
    /// workspace doc in M4) fail their subscribe and are skipped gracefully.
    fn attach_engine(&mut self, handle: EngineHandle, cx: &mut Context<Self>) {
        self.connection = ConnectionStatus::Ready;
        self.engine = Some(handle.clone());
        let federation_handle = handle.clone();
        let federation_data_dir = self.data_dir.clone();
        self.watch_tasks = vec![
            spawn_watch(
                cx,
                handle.clone(),
                methods::UPDATE_STATUS,
                AppState::apply_update,
            ),
            spawn_local_device_probe(cx, handle.clone()),
        ];
        if let Some(data_dir) = federation_data_dir {
            let start = Tokio::spawn(cx, async move {
                Federation::new_shared(federation_handle.client(), data_dir).await
            });
            cx.spawn(async move |this, cx| match start.await {
                Ok(Ok(federation)) => {
                    this.update(cx, |state, cx| state.attach_federation(federation, cx))
                        .ok();
                }
                Ok(Err(error)) => tracing::warn!(%error, "federation unavailable"),
                Err(error) => tracing::warn!(%error, "federation startup task failed"),
            })
            .detach();
        }
        cx.notify();
    }

    fn attach_federation(&mut self, mut federation: Federation, cx: &mut Context<Self>) {
        self.federation = Some(FederatedClient {
            commands: federation.command_sender(),
        });
        if let (Some(client), Some(chat)) = (&self.federation, self.selected_chat.clone()) {
            client.watch_transcript(Some(chat));
        }
        self.watch_tasks.push(cx.spawn(async move |this, cx| {
            while let Some(event) = federation.recv().await {
                let alive = this.update(cx, |state, cx| {
                    state.apply_federation(event);
                    if state.selected_chat.is_none() && !state.auto_selected {
                        let first = state.server_order.iter().find_map(|server_id| {
                            let server = state.servers.get(server_id)?;
                            server
                                .chats
                                .iter()
                                .find(|chat| !chat.archived)
                                .map(|chat| ServerRef::new(server_id.clone(), chat.id.clone()))
                        });
                        if let Some(chat) = first {
                            state.select_server_chat(chat.clone());
                            if let Some(client) = &state.federation {
                                client.watch_transcript(Some(chat));
                            }
                        }
                    }
                    cx.notify();
                });
                if alive.is_err() {
                    break;
                }
            }
        }));
    }

    /// Select a chat (or clear). Swaps the per-chat doc-transcript subscription:
    /// dropping the old task drops its stream receiver, which cancels the doc
    /// watch server-side. Selecting a chat also lands in its space and marks it
    /// seen (a global-list click must switch the tab strip too).
    pub fn select_chat(&mut self, chat_id: Option<String>, cx: &mut Context<Self>) {
        let qualified = chat_id
            .as_ref()
            .map(|id| ServerRef::new(self.current_server_id(), id));
        if self.selected_chat == qualified {
            // Re-selecting still clears a fresh "completed" badge.
            if let Some(id) = chat_id {
                self.mark_chat_seen(&id, cx);
            }
            return;
        }
        self.selected_chat = qualified;
        self.auto_selected = true;
        self.transcript.clear();
        if let Some(id) = chat_id.as_deref() {
            // A chat implies its space; `select_chat(None)` (the new-session
            // canvas) stays within the current space.
            if let Some(space_id) = self
                .chats
                .iter()
                .find(|c| c.id == id)
                .and_then(|c| c.space_id.clone())
            {
                self.selected_space = Some(ServerRef::new(self.current_server_id(), space_id));
            }
            self.mark_chat_seen(id, cx);
        }
        if let Some(client) = &self.federation {
            client.watch_transcript(self.selected_chat.clone());
        }
        cx.notify();
    }

    /// Select a space; the caller (shell) decides which chat to land on.
    pub fn select_space(&mut self, space_id: Option<String>, cx: &mut Context<Self>) {
        let qualified = space_id.map(|id| ServerRef::new(self.current_server_id(), id));
        if self.selected_space == qualified {
            return;
        }
        self.selected_space = qualified;
        cx.notify();
    }

    fn current_server_id(&self) -> ServerId {
        self.active_server
            .clone()
            .or_else(|| self.server_order.first().cloned())
            .unwrap_or_else(|| ServerId::new("local"))
    }

    /// Synced seen marker: only fires when the chat is currently unseen
    /// (idempotence — no mutate spam), stamps the local row optimistically so
    /// the LWW round-trip is invisible, and fire-and-forgets the mutate.
    pub fn mark_chat_seen(&mut self, chat_id: &str, cx: &mut Context<Self>) {
        let Some(chat) = self.chats.iter_mut().find(|c| c.id == chat_id) else {
            return;
        };
        if !chat.unseen() {
            return;
        }
        chat.last_seen_at = Some(Utc::now());
        cx.notify();
        let Some(client) = self.federation.clone() else {
            return;
        };
        let server_id = self.current_server_id();
        let chat_id = chat_id.to_string();
        client.call(
            server_id,
            methods::MUTATE,
            serde_json::json!({ "op": "markChatSeen", "chatId": chat_id }),
        );
    }
}

/// Subscribe to a watch method and pump each frame through `apply`. Runs on the
fn spawn_watch<T: DeserializeOwned + 'static>(
    cx: &mut Context<AppState>,
    handle: EngineHandle,
    method: &'static str,
    apply: fn(&mut AppState, T),
) -> Task<()> {
    cx.spawn(async move |this, cx| {
        let mut rx = match handle
            .client()
            .subscribe(method, serde_json::json!({}))
            .await
        {
            Ok(rx) => rx,
            Err(err) => {
                tracing::debug!(method, error = %err, "watch unavailable");
                return;
            }
        };
        while let Some(value) = rx.recv().await {
            let parsed: T = match serde_json::from_value(value) {
                Ok(parsed) => parsed,
                Err(err) => {
                    tracing::warn!(method, error = %err, "dropping malformed watch frame");
                    continue;
                }
            };
            let alive = this.update(cx, |state, cx| {
                apply(state, parsed);
                cx.notify();
            });
            if alive.is_err() {
                break;
            }
        }
    })
}

/// Best-effort `LocalDevice` probe: fills `local_device_id` for the "This
/// device" badge. Engines that don't serve the method leave it `None`.
fn spawn_local_device_probe(cx: &mut Context<AppState>, handle: EngineHandle) -> Task<()> {
    cx.spawn(async move |this, cx| {
        let Ok(value) = handle
            .client()
            .call("LocalDevice", serde_json::json!({}))
            .await
        else {
            tracing::debug!("LocalDevice unavailable; skipping this-device badge");
            return;
        };
        let id = value
            .get("id")
            .or_else(|| value.get("deviceId"))
            .and_then(|v| v.as_str())
            .map(str::to_string);
        if let Some(id) = id {
            this.update(cx, |state, cx| {
                state.local_device_id = Some(id);
                cx.notify();
            })
            .ok();
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;
    use comet_client::{FederationCommand, FederationEvent, ServerState};
    use comet_engine::{EngineCore, default_registry};
    // `SessionStatus` is only needed to build the fixtures below — the module
    // itself derives everything through `comet_proto::view`.
    use comet_proto::SessionStatus;
    use comet_rpc::RpcReply;

    fn server(id: &str) -> comet_proto::ServerId {
        comet_proto::ServerId::new(format!("sha256:{id}"))
    }

    fn staged_comment(id: &str) -> crate::comments::DiffComment {
        let mut comment = crate::comments::DiffComment::new(
            "src/lib.rs",
            crate::comments::CommentSide::New,
            3,
            id,
        );
        comment.id = id.to_string();
        comment
    }

    #[test]
    fn equal_raw_chat_ids_on_different_servers_do_not_share_diff_comments() {
        let a = ServerRef::new(server("a"), "same-chat");
        let b = ServerRef::new(server("b"), "same-chat");
        let mut state = AppState::new();

        state.add_diff_comment(&a, staged_comment("a-1"));

        assert_eq!(
            state
                .diff_comments(&a)
                .iter()
                .map(|comment| comment.id.as_str())
                .collect::<Vec<_>>(),
            vec!["a-1"]
        );
        assert!(state.diff_comments(&b).is_empty());
    }

    #[test]
    fn failed_send_restores_taken_comments_without_losing_newer_staged_comments() {
        let owner = ServerRef::new(server("a"), "chat-1");
        let mut state = AppState::new();
        state.add_diff_comment(&owner, staged_comment("old-1"));
        state.add_diff_comment(&owner, staged_comment("old-2"));

        let taken = state.take_diff_comments(&owner);
        state.add_diff_comment(&owner, staged_comment("new-3"));
        state.restore_diff_comments(&owner, taken);

        assert_eq!(
            state
                .diff_comments(&owner)
                .iter()
                .map(|comment| comment.id.as_str())
                .collect::<Vec<_>>(),
            vec!["old-1", "old-2", "new-3"]
        );
    }

    struct ClosedService;

    #[async_trait]
    impl RpcService for ClosedService {
        async fn handle(&self, _: &str, _: serde_json::Value) -> Result<RpcReply, RpcError> {
            Err(RpcError::Closed)
        }
    }

    #[tokio::test]
    async fn local_watch_client_provider_observes_engine_replacement() {
        let first = Arc::new(memory_client(Arc::new(ClosedService)));
        let second = Arc::new(memory_client(Arc::new(ClosedService)));
        let mut state = AppState::new();
        state.engine = Some(EngineHandle {
            inner: Arc::new(RemoteEngine {
                client: first.clone(),
                url: "ws://first".into(),
            }),
        });
        assert!(Arc::ptr_eq(&state.local_rpc_client().unwrap(), &first));

        state.engine = Some(EngineHandle {
            inner: Arc::new(RemoteEngine {
                client: second.clone(),
                url: "ws://second".into(),
            }),
        });
        assert!(Arc::ptr_eq(&state.local_rpc_client().unwrap(), &second));
    }

    fn remote_chat(id: &str, space_id: &str) -> Chat {
        Chat {
            id: id.into(),
            device_id: "dev".into(),
            title: Some(format!("chat on {space_id}")),
            archived: false,
            cwd: Some("/dev/comet".into()),
            branch: Some("main".into()),
            checkout_id: None,
            config: None,
            last_message_preview: None,
            last_message_at: Some(Utc::now()),
            created_at: Utc::now(),
            harness_session_id: None,
            harness_session_cwd: None,
            space_id: Some(space_id.into()),
            last_seen_at: None,
        }
    }

    #[test]
    fn remote_offline_heals_qualified_selection_to_local() {
        let local = server("local");
        let remote = server("b");
        let (commands, mut received) = tokio::sync::mpsc::unbounded_channel();
        let mut state = AppState::new();
        state.federation = Some(FederatedClient { commands });
        state.apply_federation(FederationEvent::ServerChanged(ServerState::empty(
            local.clone(),
            "This device",
            comet_proto::RemoteConnectionState::Online,
        )));
        let mut b = ServerState::empty(
            remote.clone(),
            "Build box",
            comet_proto::RemoteConnectionState::Online,
        );
        b.chats.push(remote_chat("chat-1", "space-1"));
        state.apply_federation(FederationEvent::ServerChanged(b));
        state.select_server_chat(comet_proto::ServerRef::new(remote.clone(), "chat-1"));

        state.apply_federation(FederationEvent::ServerChanged(ServerState::offline(
            remote,
            "Build box",
        )));

        assert_eq!(state.selected_server_id(), Some(&local));
        assert!(state.selected_chat.is_none());
        assert!(state.transcript.is_empty());
        assert!(matches!(
            received.try_recv(),
            Ok(FederationCommand::WatchTranscript(None))
        ));
    }

    #[test]
    fn duplicate_raw_chat_ids_route_to_the_qualified_server() {
        let mut state = AppState::new();
        for id in ["b", "c"] {
            let mut bucket =
                ServerState::empty(server(id), id, comet_proto::RemoteConnectionState::Online);
            bucket.chats.push(remote_chat("chat-1", "space-1"));
            state.apply_federation(FederationEvent::ServerChanged(bucket));
        }
        let chat = comet_proto::ServerRef::new(server("c"), "chat-1");

        let command = state.call_for(
            &chat,
            methods::QUEUE_COMMAND,
            serde_json::json!({ "chatId": chat.local_id(), "text": "hello" }),
        );

        assert!(matches!(command,
            FederationCommand::Call { server_id, method, params }
                if server_id == server("c")
                    && method == methods::QUEUE_COMMAND
                    && params["chatId"] == "chat-1"
                    && params.get("targetDeviceId").is_none()));
    }

    #[tokio::test]
    async fn owner_bound_mutations_route_to_b_without_changing_selected_a() {
        let mut state = AppState::new();
        for id in ["a", "b"] {
            let mut bucket = ServerState::empty(server(id), id, RemoteConnectionState::Online);
            bucket.chats.push(remote_chat("chat-1", "space-1"));
            state.apply_federation(FederationEvent::ServerChanged(bucket));
        }
        let selected = ServerRef::new(server("a"), "chat-1");
        let owner = ServerRef::new(server("b"), "chat-1");
        state.select_server_chat(selected.clone());
        let (commands, mut received) = tokio::sync::mpsc::unbounded_channel();
        state.federation = Some(FederatedClient { commands });

        for params in [
            serde_json::json!({ "op": "renameChat", "chatId": "chat-1", "title": "B" }),
            serde_json::json!({ "op": "setChatArchived", "chatId": "chat-1", "archived": true }),
            serde_json::json!({ "op": "deleteChat", "chatId": "chat-1" }),
        ] {
            let client = state.mutation_client_for(&owner).unwrap();
            let request = tokio::spawn(async move { client.call(methods::MUTATE, params).await });
            let FederationCommand::Request {
                server_id,
                method,
                params,
                reply,
            } = received.recv().await.unwrap()
            else {
                panic!("expected an owner-bound request");
            };
            assert_eq!(server_id, server("b"));
            assert_eq!(method, methods::MUTATE);
            assert_eq!(params["chatId"], "chat-1");
            reply.send(Ok(serde_json::Value::Null)).unwrap();
            request.await.unwrap().unwrap();
            assert_eq!(state.selected_chat.as_ref(), Some(&selected));
        }
    }

    #[test]
    fn owner_bound_mutations_reject_offline_or_removed_b_without_falling_back_to_a() {
        let mut state = AppState::new();
        for id in ["a", "b"] {
            let mut bucket = ServerState::empty(server(id), id, RemoteConnectionState::Online);
            bucket.chats.push(remote_chat("chat-1", "space-1"));
            state.apply_federation(FederationEvent::ServerChanged(bucket));
        }
        let selected = ServerRef::new(server("a"), "chat-1");
        let owner = ServerRef::new(server("b"), "chat-1");
        state.select_server_chat(selected.clone());
        let (commands, mut received) = tokio::sync::mpsc::unbounded_channel();
        state.federation = Some(FederatedClient { commands });

        state.apply_federation(FederationEvent::ServerChanged(ServerState::offline(
            server("b"),
            "b",
        )));
        assert!(matches!(
            state.mutation_client_for(&owner),
            Err(RpcError::Failed(message)) if message.contains("offline")
        ));
        assert_eq!(state.selected_chat.as_ref(), Some(&selected));
        assert!(received.try_recv().is_err());

        state.apply_federation(FederationEvent::ServerRemoved(server("b")));
        assert!(matches!(
            state.mutation_client_for(&owner),
            Err(RpcError::Failed(message)) if message.contains("offline")
        ));
        assert_eq!(state.selected_chat.as_ref(), Some(&selected));
        assert!(received.try_recv().is_err());
    }

    #[test]
    fn terminal_subscription_command_uses_the_chat_server() {
        let state = AppState::new();
        let chat = comet_proto::ServerRef::new(server("c"), "chat-1");
        let (reply, _received) = tokio::sync::oneshot::channel();
        let command = state.subscribe_for(
            &chat,
            methods::SUBSCRIBE_TERMINAL,
            serde_json::json!({"terminalId": "term-1", "afterSeq": 7}),
            reply,
        );
        assert!(matches!(command,
            FederationCommand::Subscribe { server_id, method, params, .. }
                if server_id == server("c")
                    && method == methods::SUBSCRIBE_TERMINAL
                    && params["terminalId"] == "term-1"
                    && params.get("targetDeviceId").is_none()));
    }

    #[test]
    fn duplicate_chat_transients_are_isolated_and_removal_is_scoped() {
        let mut state = AppState::new();
        for id in ["b", "c"] {
            let mut bucket =
                ServerState::empty(server(id), id, comet_proto::RemoteConnectionState::Online);
            bucket.chats.push(remote_chat("chat-1", "space-1"));
            state.apply_federation(FederationEvent::ServerChanged(bucket));
        }
        let echo = |id: &str| SessionMessageEntry {
            id: id.into(),
            role: comet_doc::MessageRole::User,
            parts: Vec::new(),
            created_at: 0,
            device_id: "local".into(),
            status: None,
            continuation_of: None,
        };
        state.select_server_chat(ServerRef::new(server("b"), "chat-1"));
        state.push_echo("chat-1", echo("from-b"));
        state.select_server_chat(ServerRef::new(server("c"), "chat-1"));
        state.push_echo("chat-1", echo("from-c"));
        assert_eq!(state.pending_echoes()[0].id, "from-c");

        state.apply_federation(FederationEvent::ServerRemoved(server("b")));

        assert_eq!(state.pending_echoes()[0].id, "from-c");
        assert!(state.echoes.keys().all(|key| key.server_id != server("b")));
    }

    #[test]
    fn selecting_a_server_header_clears_transcript_ownership() {
        let (commands, mut received) = tokio::sync::mpsc::unbounded_channel();
        let mut state = AppState::new();
        state.federation = Some(FederatedClient { commands });
        for id in ["a", "b"] {
            let mut bucket = ServerState::empty(server(id), id, RemoteConnectionState::Online);
            bucket.chats.push(remote_chat("chat-1", "space-1"));
            state.apply_federation(FederationEvent::ServerChanged(bucket));
        }
        state.select_server_chat(ServerRef::new(server("b"), "chat-1"));

        state.select_server_bucket(server("a"));

        assert!(matches!(
            received.try_recv(),
            Ok(FederationCommand::WatchTranscript(None))
        ));
        assert!(state.selected_chat.is_none());
    }

    /// A localhost port that was just free (bind :0, read, drop).
    async fn free_port() -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap().port()
    }

    #[tokio::test]
    async fn bootstrap_embeds_engine_when_port_is_free() {
        let dir = tempfile::tempdir().unwrap();
        let handle = EngineHandle::bootstrap(EngineBootConfig {
            data_dir: dir.path().to_path_buf(),
            ipc_port: free_port().await,
            releases_url: "http://127.0.0.1:1".into(),
            default_harness: HarnessId::Mock,
        })
        .await
        .unwrap();
        assert_eq!(handle.mode(), EngineMode::InProcess);
        // Same protocol over the in-memory transport: a real engine answers.
        let harnesses = handle
            .client()
            .call(methods::LIST_HARNESSES, serde_json::json!({}))
            .await
            .unwrap();
        assert!(harnesses.as_array().is_some_and(|h| !h.is_empty()));
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn an_embedded_engine_serves_the_ipc_port_for_other_viewports() {
        // The whole point of embedding-and-serving: a second viewport (the
        // terminal app) can attach to this window's engine with no setup, no
        // separate daemon, and no launch ordering.
        let dir = tempfile::tempdir().unwrap();
        let port = free_port().await;
        let handle = EngineHandle::bootstrap(EngineBootConfig {
            data_dir: dir.path().to_path_buf(),
            ipc_port: port,
            releases_url: "http://127.0.0.1:1".into(),
            default_harness: HarnessId::Mock,
        })
        .await
        .unwrap();
        assert_eq!(handle.mode(), EngineMode::InProcess);

        // Attach the way an external viewport would, and speak the same protocol.
        let attached = connect_ws(&format!("ws://127.0.0.1:{port}"))
            .await
            .expect("a second viewport must be able to attach");
        let harnesses = attached
            .call(methods::LIST_HARNESSES, serde_json::json!({}))
            .await
            .unwrap();
        assert!(harnesses.as_array().is_some_and(|h| !h.is_empty()));

        // Shutting the window down stops accepting, so the next viewport
        // starts its own engine rather than talking to closing stores.
        handle.shutdown().await;
        assert!(
            tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_err(),
            "the port must be released on shutdown"
        );
    }

    #[tokio::test]
    async fn a_stranger_on_the_ipc_port_does_not_wedge_the_window() {
        // The port probe only proves *something* is listening. A process that
        // accepts TCP and never speaks WebSocket used to hang the dial forever;
        // now it times out and we embed instead, losing only the ability to
        // serve other viewports.
        let squatter = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = squatter.local_addr().unwrap().port();
        let dir = tempfile::tempdir().unwrap();
        let handle = EngineHandle::bootstrap(EngineBootConfig {
            data_dir: dir.path().to_path_buf(),
            ipc_port: port,
            releases_url: "http://127.0.0.1:1".into(),
            default_harness: HarnessId::Mock,
        })
        .await
        .expect("a taken port must not fail the boot");
        assert_eq!(handle.mode(), EngineMode::InProcess);
        assert!(
            handle
                .client()
                .call(methods::LIST_HARNESSES, serde_json::json!({}))
                .await
                .is_ok(),
            "the window still works over its own transport"
        );
        handle.shutdown().await;
        drop(squatter);
    }

    #[tokio::test]
    async fn bootstrap_connects_when_daemon_is_listening() {
        // Stand in for `comet headless`: an engine served over the WS IPC port.
        let daemon_dir = tempfile::tempdir().unwrap();
        let core = EngineCore::assemble(
            daemon_dir.path(),
            Arc::new(default_registry()),
            HarnessId::Mock,
            None,
        )
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(comet_rpc::serve_ws_listener(listener, core.rpc_service()));

        let ui_dir = tempfile::tempdir().unwrap();
        let handle = EngineHandle::bootstrap(EngineBootConfig {
            data_dir: ui_dir.path().to_path_buf(),
            ipc_port: port,
            releases_url: "http://127.0.0.1:1".into(),
            default_harness: HarnessId::Mock,
        })
        .await
        .unwrap();
        assert_eq!(
            handle.mode(),
            EngineMode::Remote {
                url: format!("ws://127.0.0.1:{port}")
            }
        );
        let harnesses = handle
            .client()
            .call(methods::LIST_HARNESSES, serde_json::json!({}))
            .await
            .unwrap();
        assert!(harnesses.as_array().is_some_and(|h| !h.is_empty()));
    }

    fn chat(id: &str, created_min: i64, last_msg_min: Option<i64>) -> Chat {
        let base = DateTime::parse_from_rfc3339("2026-07-19T12:00:00Z")
            .unwrap()
            .to_utc();
        Chat {
            id: id.into(),
            device_id: "dev".into(),
            title: None,
            archived: false,
            cwd: None,
            branch: None,
            checkout_id: None,
            config: None,
            last_message_preview: None,
            last_message_at: last_msg_min.map(|m| base + TimeDelta::minutes(m)),
            created_at: base + TimeDelta::minutes(created_min),
            harness_session_id: None,
            harness_session_cwd: None,
            space_id: None,
            last_seen_at: None,
        }
    }

    fn space(id: &str, device_id: &str, path: &str, created_min: i64) -> Space {
        let base = DateTime::parse_from_rfc3339("2026-07-19T12:00:00Z")
            .unwrap()
            .to_utc();
        Space {
            id: id.into(),
            device_id: device_id.into(),
            path: path.into(),
            name: None,
            git_detected: false,
            git_checked_at: None,
            checkout_id: None,
            created_at: base + TimeDelta::minutes(created_min),
        }
    }

    fn session(
        chat_id: &str,
        status: SessionStatus,
        updated_secs_ago: i64,
        now: DateTime<Utc>,
    ) -> Session {
        Session {
            chat_id: chat_id.into(),
            device_id: "dev".into(),
            status,
            started_at: None,
            updated_at: now - TimeDelta::seconds(updated_secs_ago),
            context: None,
        }
    }

    #[test]
    fn chats_sort_by_last_message_desc_with_created_fallback() {
        let mut chats = vec![
            chat("a", 0, Some(10)),
            chat("b", 5, None), // no messages → keys on created_at (+5min)
            chat("c", 1, Some(30)),
            chat("d", 40, None), // created after every message
        ];
        sort_chats(&mut chats);
        let order: Vec<&str> = chats.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(order, ["d", "c", "a", "b"]);
    }

    #[test]
    fn chat_sort_ties_are_deterministic() {
        let mut chats = vec![chat("z", 0, Some(10)), chat("a", 0, Some(10))];
        sort_chats(&mut chats);
        assert_eq!(chats[0].id, "a");
    }

    #[test]
    fn working_indicator_staleness() {
        let now = Utc::now();
        // Fresh working session shows.
        let fresh = session("c", SessionStatus::Working, 10, now);
        assert_eq!(effective_indicator(Some(&fresh), now), Indicator::Working);
        // Stale working session is suppressed — crashed backend, not eternal spinner.
        let stale = session("c", SessionStatus::Working, 46, now);
        assert_eq!(effective_indicator(Some(&stale), now), Indicator::None);
        // Exactly at the boundary still shows (strictly-older-than semantics).
        let edge = session("c", SessionStatus::Working, 45, now);
        assert_eq!(effective_indicator(Some(&edge), now), Indicator::Working);
        // Future timestamps (clock skew) count as fresh.
        let skewed = session("c", SessionStatus::Working, -30, now);
        assert_eq!(effective_indicator(Some(&skewed), now), Indicator::Working);
    }

    #[test]
    fn indicator_kinds() {
        let now = Utc::now();
        assert_eq!(effective_indicator(None, now), Indicator::None);
        let idle = session("c", SessionStatus::Idle, 0, now);
        assert_eq!(effective_indicator(Some(&idle), now), Indicator::None);
        // Errored is not staleness-gated: the error stays visible.
        let errored = session("c", SessionStatus::Errored, 600, now);
        assert_eq!(effective_indicator(Some(&errored), now), Indicator::Errored);
        let awaiting = session("c", SessionStatus::AwaitingInput, 5, now);
        assert_eq!(
            effective_indicator(Some(&awaiting), now),
            Indicator::AwaitingInput
        );
        let awaiting_stale = session("c", SessionStatus::AwaitingInput, 300, now);
        assert_eq!(
            effective_indicator(Some(&awaiting_stale), now),
            Indicator::None
        );
    }

    #[test]
    fn display_status_derivation() {
        let now = Utc::now();
        let mut c = chat("c", 0, Some(10));
        // Live states win regardless of seen.
        let working = session("c", SessionStatus::Working, 5, now);
        assert_eq!(
            display_status(&c, Some(&working), now),
            ChatIndicator::Working
        );
        let awaiting = session("c", SessionStatus::AwaitingInput, 5, now);
        assert_eq!(
            display_status(&c, Some(&awaiting), now),
            ChatIndicator::AwaitingInput
        );
        // Finished + unseen = Completed (no session row at all).
        assert_eq!(display_status(&c, None, now), ChatIndicator::Completed);
        // Idle session + unseen = Completed.
        let idle = session("c", SessionStatus::Idle, 5, now);
        assert_eq!(
            display_status(&c, Some(&idle), now),
            ChatIndicator::Completed
        );
        // Stale working session falls back to the seen check.
        let stale = session("c", SessionStatus::Working, 300, now);
        assert_eq!(
            display_status(&c, Some(&stale), now),
            ChatIndicator::Completed
        );
        // Seen after the last message = Idle.
        c.last_seen_at = c.last_message_at.map(|t| t + TimeDelta::minutes(1));
        assert_eq!(display_status(&c, Some(&idle), now), ChatIndicator::Idle);
        // Errored + unseen = Errored; seen clears it to Idle.
        let errored = session("c", SessionStatus::Errored, 600, now);
        assert_eq!(display_status(&c, Some(&errored), now), ChatIndicator::Idle);
        c.last_seen_at = None;
        assert_eq!(
            display_status(&c, Some(&errored), now),
            ChatIndicator::Errored
        );
        // No messages at all: nothing to see — Idle.
        let fresh = chat("f", 0, None);
        assert_eq!(display_status(&fresh, None, now), ChatIndicator::Idle);
    }

    #[test]
    fn active_list_sorts_by_recency_only_status_never_moves_rows() {
        let a = chat("a", 0, Some(10)); // Completed (older)
        let b = chat("b", 0, Some(20)); // Completed (newer)
        let c = chat("c", 0, Some(5)); // AwaitingInput
        let d = chat("d", 0, Some(1)); // Working
        let mut rows = vec![
            (ChatIndicator::Completed, &a),
            (ChatIndicator::Completed, &b),
            (ChatIndicator::AwaitingInput, &c),
            (ChatIndicator::Working, &d),
        ];
        sort_active(&mut rows);
        let order: Vec<&str> = rows.iter().map(|(_, c)| c.id.as_str()).collect();
        assert_eq!(order, ["b", "a", "c", "d"], "recency desc, status ignored");

        // Opening a completed session (completed → seen → idle) must NOT
        // change its position (user report: rows jumped under the pointer).
        let mut seen = vec![
            (ChatIndicator::Idle, &a),
            (ChatIndicator::Completed, &b),
            (ChatIndicator::AwaitingInput, &c),
            (ChatIndicator::Working, &d),
        ];
        sort_active(&mut seen);
        let order_after: Vec<&str> = seen.iter().map(|(_, c)| c.id.as_str()).collect();
        assert_eq!(order, order_after);
    }

    #[test]
    fn tabs_order_by_creation_not_activity() {
        let a = chat("a", 5, Some(100)); // created later, very active
        let b = chat("b", 1, Some(2));
        let mut tabs = vec![&a, &b];
        sort_tabs(&mut tabs);
        let order: Vec<&str> = tabs.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(order, ["b", "a"]);
    }

    #[test]
    fn apply_spaces_sorts_and_heals_selection() {
        let mut state = AppState::new();
        state.apply_spaces(vec![
            space("s2", "dev", "/b", 2),
            space("s1", "dev", "/a", 1),
        ]);
        let ids: Vec<&str> = state.spaces.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["s1", "s2"]);
        // First frame auto-selects the first space.
        assert_eq!(state.selected_space_id(), Some("s1"));
        state.selected_space = Some(ServerRef::new(state.current_server_id(), "s2"));
        // Vanished selection heals to the first space.
        state.apply_spaces(vec![space("s1", "dev", "/a", 1)]);
        assert_eq!(state.selected_space_id(), Some("s1"));
        // No spaces at all: selection clears.
        state.apply_spaces(vec![]);
        assert_eq!(state.selected_space, None);
    }

    #[test]
    fn overview_chats_defaults_to_every_space() {
        let mut state = AppState::new();
        state.apply_spaces(vec![
            space("s1", "dev", "/a", 1),
            space("s2", "dev", "/b", 2),
        ]);
        let mut a = chat("c1", 1, None);
        a.space_id = Some("s1".into());
        let mut b = chat("c2", 2, None);
        b.space_id = Some("s2".into());
        state.apply_chats(vec![a, b]);

        assert_eq!(state.sidebar_scope, SidebarScope::All);
        let ids: Vec<&str> = state
            .overview_chats(Utc::now())
            .iter()
            .map(|(_, c)| c.id.as_str())
            .collect();
        assert_eq!(ids.len(), 2, "All scope lists every space's chats");
    }

    #[test]
    fn overview_chats_filters_to_the_scoped_space() {
        let mut state = AppState::new();
        state.apply_spaces(vec![
            space("s1", "dev", "/a", 1),
            space("s2", "dev", "/b", 2),
        ]);
        let mut a = chat("c1", 1, None);
        a.space_id = Some("s1".into());
        let mut b = chat("c2", 2, None);
        b.space_id = Some("s2".into());
        state.apply_chats(vec![a, b]);

        state.sidebar_scope = SidebarScope::Space("s1".into());
        let ids: Vec<&str> = state
            .overview_chats(Utc::now())
            .iter()
            .map(|(_, c)| c.id.as_str())
            .collect();
        assert_eq!(ids, ["c1"]);
    }

    #[test]
    fn scope_heals_to_all_when_its_space_vanishes() {
        let mut state = AppState::new();
        state.apply_spaces(vec![
            space("s1", "dev", "/a", 1),
            space("s2", "dev", "/b", 2),
        ]);
        state.sidebar_scope = SidebarScope::Space("s2".into());

        // s2 deleted elsewhere.
        state.apply_spaces(vec![space("s1", "dev", "/a", 1)]);

        assert_eq!(
            state.sidebar_scope,
            SidebarScope::All,
            "a dangling scope falls back to All, not to an arbitrary neighbour"
        );
    }

    #[test]
    fn scope_survives_a_spaces_frame_that_still_contains_it() {
        let mut state = AppState::new();
        state.apply_spaces(vec![
            space("s1", "dev", "/a", 1),
            space("s2", "dev", "/b", 2),
        ]);
        state.sidebar_scope = SidebarScope::Space("s2".into());
        state.apply_spaces(vec![
            space("s1", "dev", "/a", 1),
            space("s2", "dev", "/b", 2),
        ]);
        assert_eq!(state.sidebar_scope, SidebarScope::Space("s2".into()));
    }

    #[test]
    fn scope_space_id_accessor() {
        assert_eq!(SidebarScope::All.space_id(), None);
        assert_eq!(SidebarScope::Space("s1".into()).space_id(), Some("s1"));
    }

    #[test]
    fn chats_in_space_filters_and_orders() {
        let mut state = AppState::new();
        state.apply_spaces(vec![space("s1", "dev", "/a", 1)]);
        let mut in_space_new = chat("new", 5, None);
        in_space_new.space_id = Some("s1".into());
        let mut in_space_old = chat("old", 1, Some(50)); // active but created first
        in_space_old.space_id = Some("s1".into());
        let mut other = chat("other", 2, None);
        other.space_id = Some("s2".into());
        let mut archived = chat("gone", 0, None);
        archived.space_id = Some("s1".into());
        archived.archived = true;
        let dangling = chat("dangling", 3, None); // no space id
        state.apply_chats(vec![in_space_new, in_space_old, other, archived, dangling]);
        let ids: Vec<&str> = state
            .chats_in_space("s1")
            .iter()
            .map(|c| c.id.as_str())
            .collect();
        assert_eq!(ids, ["old", "new"]);
        // The overview shows every live-space chat (idle included) — chats of
        // unknown spaces stay hidden. Completed ("old") outranks idle ("new").
        let now = Utc::now();
        let overview: Vec<&str> = state
            .overview_chats(now)
            .iter()
            .map(|(_, c)| c.id.as_str())
            .collect();
        assert_eq!(overview, ["old", "new"]);
    }

    /// The whole point of [`AppState::listed_chats`]: the sidebar's counts and
    /// its list must be the same set. A chat with no `space_id` (config frame
    /// not landed) or a dangling one (space deleted elsewhere) is in neither.
    #[test]
    fn listed_chats_is_the_same_set_the_sessions_list_shows() {
        let mut state = AppState::new();
        state.apply_spaces(vec![space("s1", "dev", "/a", 1)]);
        let mut live = chat("live", 1, None);
        live.space_id = Some("s1".into());
        let mut dangling = chat("dangling", 2, None);
        dangling.space_id = Some("deleted".into());
        let spaceless = chat("spaceless", 3, None); // space_id: None
        let mut archived = chat("archived", 4, None);
        archived.space_id = Some("s1".into());
        archived.archived = true;
        state.apply_chats(vec![live, dangling, spaceless, archived]);

        let listed: Vec<&str> = state.listed_chats().map(|c| c.id.as_str()).collect();
        assert_eq!(listed, ["live"]);
        // The trigger/panel count is `listed_chats().count()`; on `All` it must
        // equal exactly what the list below it renders.
        assert_eq!(state.sidebar_scope, SidebarScope::All);
        assert_eq!(
            state.listed_chats().count(),
            state.overview_chats(Utc::now()).len(),
            "the count in the chrome and the rows in the list are one set"
        );
    }

    #[test]
    fn jump_slots_count_the_rows_the_sidebar_draws() {
        let now = Utc::now();
        let mut state = AppState::new();
        let mut in_space = chat("a", 0, Some(3));
        in_space.space_id = Some("s1".into());
        let mut other_space = chat("b", 1, Some(2));
        other_space.space_id = Some("s2".into());
        let mut archived = chat("gone", 2, Some(1));
        archived.space_id = Some("s1".into());
        archived.archived = true;
        state.apply_spaces(vec![
            space("s1", "dev", "/tmp/s1", 0),
            space("s2", "dev", "/tmp/s2", 1),
        ]);
        state.apply_chats(vec![in_space, other_space, archived]);

        let order: Vec<&str> = state
            .overview_chats(now)
            .iter()
            .map(|(_, c)| c.id.as_str())
            .collect();
        assert_eq!(state.jump_target(now, 0).as_deref(), Some(order[0]));
        assert_eq!(state.jump_target(now, 1).as_deref(), Some(order[1]));
        // The archived row is not in the active list, so no slot reaches it.
        assert_eq!(order.len(), 2);
        assert_eq!(state.jump_target(now, 2), None);
        assert_eq!(state.jump_target(now, 8), None);

        // A scoped sidebar renumbers: the slots count the visible rows only.
        state.sidebar_scope = SidebarScope::Space("s2".into());
        assert_eq!(state.jump_target(now, 0).as_deref(), Some("b"));
        assert_eq!(state.jump_target(now, 1), None);
    }

    #[test]
    fn archive_shortcut_only_targets_an_open_active_chat() {
        let mut state = AppState::new();
        let mut archived = chat("a", 0, None);
        archived.archived = true;
        state.apply_chats(vec![archived, chat("b", 1, None)]);
        // No chat open: nothing to archive.
        assert_eq!(state.archivable_selected_chat(), None);
        // The open active chat is the target.
        state.selected_chat = Some(ServerRef::new(state.current_server_id(), "b"));
        assert_eq!(
            state.archivable_selected_chat(),
            Some(ServerRef::new(state.current_server_id(), "b"))
        );
        // An already archived chat stays put — the shortcut never unarchives.
        state.selected_chat = Some(ServerRef::new(state.current_server_id(), "a"));
        assert_eq!(state.archivable_selected_chat(), None);
    }

    #[test]
    fn apply_chats_drops_vanished_selection() {
        let mut state = AppState::new();
        state.apply_chats(vec![chat("a", 0, None), chat("b", 1, None)]);
        state.selected_chat = Some(ServerRef::new(state.current_server_id(), "a"));
        state.transcript = vec![];
        state.apply_chats(vec![chat("b", 1, None)]);
        assert_eq!(state.selected_chat, None);
        // Still-present selection survives.
        state.selected_chat = Some(ServerRef::new(state.current_server_id(), "b"));
        state.apply_chats(vec![chat("b", 1, None), chat("c", 2, None)]);
        assert_eq!(state.selected_chat_id(), Some("b"));
    }

    #[test]
    fn apply_chat_config_stamps_the_row() {
        let mut state = AppState::new();
        state.apply_chats(vec![chat("a", 0, None), chat("b", 1, None)]);
        let config = comet_proto::ChatConfig {
            harness: HarnessId::ClaudeCode,
            model: Some("claude-fable-5".into()),
            reasoning: Some(comet_proto::ReasoningLevel::XHigh),
            model_options: serde_json::Map::new(),
            sandbox: comet_proto::SandboxLevel::WorkspaceWrite,
            runtime_mode: comet_proto::RuntimeMode::default(),
        };
        state.apply_chat_config("a", config.clone());
        assert_eq!(
            state.chats.iter().find(|c| c.id == "a").unwrap().config,
            Some(config)
        );
        assert!(
            state
                .chats
                .iter()
                .find(|c| c.id == "b")
                .unwrap()
                .config
                .is_none()
        );
        // Unknown chat: no-op, no panic.
        state.apply_chat_config(
            "missing",
            comet_proto::ChatConfig {
                harness: HarnessId::ClaudeCode,
                model: None,
                reasoning: None,
                model_options: serde_json::Map::new(),
                sandbox: comet_proto::SandboxLevel::WorkspaceWrite,
                runtime_mode: comet_proto::RuntimeMode::default(),
            },
        );
    }

    #[test]
    fn visible_chats_filters_archived() {
        let mut state = AppState::new();
        let mut archived = chat("a", 0, Some(99));
        archived.archived = true;
        state.apply_chats(vec![archived, chat("b", 1, None)]);
        let visible: Vec<&str> = state.visible_chats().map(|c| c.id.as_str()).collect();
        assert_eq!(visible, ["b"]);
    }

    #[test]
    fn echoes_show_until_doc_frame_confirms() {
        let mut state = AppState::new();
        state.selected_chat = Some(ServerRef::new(state.current_server_id(), "c1"));
        let echo = SessionMessageEntry {
            id: "m1".into(),
            role: comet_doc::MessageRole::User,
            parts: vec![],
            created_at: 0,
            device_id: "local".into(),
            status: None,
            continuation_of: None,
        };
        state.push_echo("c1", echo.clone());
        // Duplicate pushes dedupe.
        state.push_echo("c1", echo.clone());
        assert_eq!(state.pending_echoes().len(), 1);
        // Frames without the id keep the echo.
        state.apply_transcript(vec![]);
        assert_eq!(state.pending_echoes().len(), 1);
        // The confirming frame prunes it.
        state.apply_transcript(vec![SessionMessageEntry {
            id: "m1".into(),
            ..echo.clone()
        }]);
        assert!(state.pending_echoes().is_empty());
        // Failure path: explicit removal.
        state.push_echo(
            "c1",
            SessionMessageEntry {
                id: "m2".into(),
                ..echo.clone()
            },
        );
        state.remove_echo("c1", "m2");
        assert!(state.pending_echoes().is_empty());
        // Echoes are per chat.
        state.push_echo(
            "other",
            SessionMessageEntry {
                id: "m3".into(),
                ..echo
            },
        );
        assert!(state.pending_echoes().is_empty());
    }

    #[test]
    fn gate_phases() {
        assert_eq!(
            gate_phase(&ConnectionStatus::Connecting),
            GatePhase::Loading
        );
        assert_eq!(
            gate_phase(&ConnectionStatus::Failed("boom".into())),
            GatePhase::Failed("boom".into())
        );
        assert_eq!(gate_phase(&ConnectionStatus::Ready), GatePhase::Ready);
    }

    fn chat_with_cwd(id: &str, created_min: i64, cwd: Option<&str>) -> Chat {
        let mut c = chat(id, created_min, None);
        c.cwd = cwd.map(str::to_string);
        c
    }

    #[test]
    fn project_labels_from_cwd() {
        assert_eq!(project_label(Some("/home/w/dev/comet")), "comet");
        assert_eq!(project_label(Some("/home/w/dev/comet/")), "comet");
        assert_eq!(project_label(None), "No project");
        assert_eq!(project_label(Some("   ")), "No project");
        assert_eq!(project_label(Some("/")), "/");
    }

    #[test]
    fn grouped_sidebar_preserves_recency_order() {
        // Input is sidebar-sorted (most recent first).
        let chats = [
            chat_with_cwd("a", 9, Some("/dev/comet")),
            chat_with_cwd("b", 8, Some("/dev/zed")),
            chat_with_cwd("c", 7, Some("/dev/comet")),
            chat_with_cwd("d", 6, None),
        ];
        let groups = group_chats(chats.iter());
        let labels: Vec<&str> = groups.iter().map(|g| g.label.as_str()).collect();
        // Groups ordered by their most recent chat; rows keep order.
        assert_eq!(labels, ["comet", "zed", "No project"]);
        let comet_ids: Vec<&str> = groups[0].chats.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(comet_ids, ["a", "c"]);
        assert!(group_chats(std::iter::empty()).is_empty());
    }

    #[test]
    fn relative_times_match_comet_format() {
        let now = Utc::now();
        let ago = |secs: i64| now - chrono::Duration::seconds(secs);
        assert_eq!(format_time_ago(ago(0), now), "now");
        assert_eq!(format_time_ago(ago(59), now), "now");
        assert_eq!(format_time_ago(ago(60), now), "1m");
        assert_eq!(format_time_ago(ago(59 * 60), now), "59m");
        assert_eq!(format_time_ago(ago(60 * 60), now), "1h");
        assert_eq!(format_time_ago(ago(23 * 3600 + 3599), now), "23h");
        assert_eq!(format_time_ago(ago(24 * 3600), now), "1d");
        assert_eq!(format_time_ago(ago(6 * 86400), now), "6d");
        assert_eq!(format_time_ago(ago(7 * 86400), now), "1w");
        assert_eq!(format_time_ago(ago(30 * 86400), now), "4w");
        assert_eq!(format_time_ago(ago(35 * 86400), now), "1mo");
        assert_eq!(format_time_ago(ago(400 * 86400), now), "1y");
        // Clock skew (future timestamps) clamps to "now".
        assert_eq!(
            format_time_ago(now + chrono::Duration::hours(2), now),
            "now"
        );
    }

    #[test]
    fn chat_location_joins_project_and_branch() {
        let mut c = chat_with_cwd("x", 1, Some("/home/w/dev/soccertcg"));
        c.branch = Some("comet/rebalance".into());
        assert_eq!(
            chat_location(&c).as_deref(),
            Some("soccertcg · comet/rebalance")
        );
        c.branch = None;
        assert_eq!(chat_location(&c).as_deref(), Some("soccertcg"));
        c.cwd = None;
        c.branch = Some("main".into());
        assert_eq!(chat_location(&c).as_deref(), Some("main"));
        c.branch = Some("   ".into());
        assert_eq!(chat_location(&c), None);
        c.branch = None;
        assert_eq!(chat_location(&c), None);
    }

    #[test]
    fn explicit_remote_reconnect_targets_the_named_federation_server() {
        let (commands, mut received) = tokio::sync::mpsc::unbounded_channel();
        let client = FederatedClient { commands };
        let wanted = ServerId::new("sha256:remote");
        client.reconnect(wanted.clone());
        assert!(matches!(
            received.try_recv(),
            Ok(FederationCommand::Reconnect(server_id)) if server_id == wanted
        ));
    }
}
