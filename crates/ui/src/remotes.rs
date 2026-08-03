use chrono::{DateTime, Utc};
use comet_proto::{LanSettings, RemoteConnectionState, RemoteEntry, TrustedClient};
use comet_rpc::{RpcClient, TlsIdentity, methods, pair_client};
use data_encoding::BASE32_NOPAD;
use serde::Deserialize;
use std::path::Path;

use gpui::{
    AnyElement, Context, Entity, SharedString, Subscription, Task, Window, div, prelude::*, px,
};

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::state::AppState;
use crate::theme::Theme;

pub const PAIRING_WARNING: &str = "Pairing grants trusted devices full authority to read locally hosted chats, run agents, control terminals, and change repositories on this computer.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingSecret {
    pub secret: String,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", tag = "state")]
pub enum ListenerStatus {
    Disabled,
    Listening {
        bind: std::net::SocketAddr,
    },
    BindFailed {
        bind: std::net::SocketAddr,
        error: String,
    },
}

#[derive(Debug, Clone)]
pub struct RemoteSettingsState {
    pub lan: LanSettings,
    pub listener: ListenerStatus,
    pub pairing: Option<PairingSecret>,
    pub trusted_clients: Vec<TrustedClient>,
    pub remotes: Vec<RemoteEntry>,
    pub pairing_error: Option<String>,
    pub add_error: Option<String>,
    pub listener_error: Option<String>,
    pairing_generation: u64,
    add_generation: u64,
}

impl Default for RemoteSettingsState {
    fn default() -> Self {
        Self {
            lan: LanSettings {
                enabled: false,
                bind: "0.0.0.0:27655".parse().expect("valid default LAN bind"),
            },
            listener: ListenerStatus::Disabled,
            pairing: None,
            trusted_clients: Vec::new(),
            remotes: Vec::new(),
            pairing_error: None,
            add_error: None,
            listener_error: None,
            pairing_generation: 0,
            add_generation: 0,
        }
    }
}

impl RemoteSettingsState {
    pub fn begin_pairing_request(&mut self) -> u64 {
        self.pairing_generation = self.pairing_generation.wrapping_add(1);
        self.pairing = None;
        self.pairing_error = None;
        self.pairing_generation
    }

    pub fn finish_pairing_request(
        &mut self,
        generation: u64,
        result: Result<PairingSecret, String>,
    ) {
        if generation != self.pairing_generation {
            return;
        }
        match result {
            Ok(pairing) => self.pairing = Some(pairing),
            Err(error) => self.pairing_error = Some(error),
        }
    }

    pub fn expire_pairing(&mut self, now: DateTime<Utc>) {
        if self
            .pairing
            .as_ref()
            .is_some_and(|pairing| now >= pairing.expires_at)
        {
            self.pairing = None;
        }
    }

    pub fn pairing_succeeded(&mut self) {
        self.pairing = None;
    }

    pub fn begin_add_request(&mut self) -> u64 {
        self.add_generation = self.add_generation.wrapping_add(1);
        self.add_error = None;
        self.add_generation
    }

    pub fn finish_add_request(&mut self, generation: u64, result: Result<(), String>) {
        if generation != self.add_generation {
            return;
        }
        if let Err(error) = result {
            self.add_error = Some(error);
        }
    }
}

pub fn remote_status_label(status: &RemoteConnectionState) -> String {
    match status {
        RemoteConnectionState::Connecting => "Connecting".into(),
        RemoteConnectionState::Online => "Online".into(),
        RemoteConnectionState::Offline => "Offline".into(),
        RemoteConnectionState::Unreachable { message } => format!("Unreachable — {message}"),
        RemoteConnectionState::IdentityChanged => {
            "Identity changed — remove and pair this server again".into()
        }
        RemoteConnectionState::IncompatibleVersion { remote } => {
            format!("Incompatible version ({remote})")
        }
    }
}

pub fn listener_status_label(status: &ListenerStatus) -> String {
    match status {
        ListenerStatus::Disabled => "Not listening".into(),
        ListenerStatus::Listening { bind } => format!("Listening on {bind}"),
        ListenerStatus::BindFailed { bind, error } => {
            format!("Could not listen on {bind}: {error}")
        }
    }
}

pub fn decode_pairing_secret(encoded: &str) -> Result<[u8; 16], String> {
    let compact = encoded
        .chars()
        .filter(|character| !character.is_whitespace() && *character != '-')
        .flat_map(char::to_uppercase)
        .collect::<String>();
    let bytes = BASE32_NOPAD
        .decode(compact.as_bytes())
        .map_err(|_| "pairing secret must be grouped Base32 text".to_string())?;
    bytes
        .try_into()
        .map_err(|_| "pairing secret must encode exactly 128 bits".to_string())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddRemoteRequest {
    pub endpoint: String,
    pub name: String,
    pub secret: [u8; 16],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinnedRemote {
    pub server_id: comet_proto::ServerId,
    pub pinned_spki_sha256: String,
}

#[async_trait::async_trait]
pub trait RemotePairer: Send + Sync {
    async fn pair(
        &self,
        data_dir: &Path,
        endpoint: &comet_proto::RemoteEndpoint,
        secret: [u8; 16],
    ) -> Result<PinnedRemote, String>;
}

#[async_trait::async_trait]
pub trait LocalRemoteAdmin: Send + Sync {
    async fn put_remote(&self, entry: &RemoteEntry) -> Result<(), String>;
}

pub struct InstallationRemotePairer;

#[async_trait::async_trait]
impl RemotePairer for InstallationRemotePairer {
    async fn pair(
        &self,
        data_dir: &Path,
        endpoint: &comet_proto::RemoteEndpoint,
        secret: [u8; 16],
    ) -> Result<PinnedRemote, String> {
        // The private key is loaded only in this controller and is never part
        // of AddRemoteRequest, RemoteSettingsState, or any GPUI entity.
        let identity = comet_identity::DeviceIdentity::load_or_create(data_dir)
            .map_err(|error| error.to_string())?;
        let tls =
            TlsIdentity::from_device_identity(&identity).map_err(|error| error.to_string())?;
        let pinned = pair_client(format!("{}:{}", endpoint.host, endpoint.port), &tls, secret)
            .await
            .map_err(|error| error.to_string())?;
        Ok(PinnedRemote {
            server_id: pinned.server_id().clone(),
            pinned_spki_sha256: data_encoding::HEXLOWER.encode(pinned.spki_sha256()),
        })
    }
}

#[async_trait::async_trait]
impl LocalRemoteAdmin for RpcClient {
    async fn put_remote(&self, entry: &RemoteEntry) -> Result<(), String> {
        self.call(
            methods::PUT_REMOTE,
            serde_json::to_value(entry).expect("remote serializes"),
        )
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
    }
}

pub async fn pair_and_persist_remote<P, A>(
    pairer: &P,
    local_admin: &A,
    data_dir: &Path,
    request: AddRemoteRequest,
) -> Result<RemoteEntry, String>
where
    P: RemotePairer + ?Sized,
    A: LocalRemoteAdmin + ?Sized,
{
    let endpoint = comet_proto::RemoteEndpoint::parse(request.endpoint.trim())?;
    let pinned = pairer.pair(data_dir, &endpoint, request.secret).await?;
    let now = Utc::now();
    let entry = RemoteEntry {
        server_id: pinned.server_id,
        name: if request.name.trim().is_empty() {
            endpoint.host.clone()
        } else {
            request.name.trim().to_string()
        },
        endpoint,
        pinned_spki_sha256: pinned.pinned_spki_sha256,
        protocol_version: 0,
        last_state: RemoteConnectionState::Connecting,
        created_at: now,
        last_connected_at: None,
    };
    local_admin.put_remote(&entry).await?;
    Ok(entry)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LanSnapshot {
    settings: LanSettings,
    status: ListenerStatus,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BeginPairingReply {
    secret: String,
    expires_at: DateTime<Utc>,
}

struct RenameRemote {
    entry: RemoteEntry,
    input: Entity<ComposerInput>,
    _events: Subscription,
}

/// Settings -> Remote connections. Administrative calls deliberately clone
/// `EngineHandle::client()` (trusted localhost IPC), never a selected remote
/// `ServerClient` or a federation request path.
pub struct RemoteConnectionsPage {
    app: Entity<AppState>,
    model: RemoteSettingsState,
    bind_input: Entity<ComposerInput>,
    port_input: Entity<ComposerInput>,
    endpoint_input: Entity<ComposerInput>,
    name_input: Entity<ComposerInput>,
    secret_input: Entity<ComposerInput>,
    rename: Option<RenameRemote>,
    trusted_loaded: bool,
    listener_task: Option<Task<()>>,
    pairing_task: Option<Task<()>>,
    add_task: Option<Task<()>>,
    mutation_task: Option<Task<()>>,
    _watch_tasks: Vec<Task<()>>,
    _input_events: Vec<Subscription>,
    _app_observation: Subscription,
}

impl RemoteConnectionsPage {
    pub fn new(app: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let app_observation = cx.observe(&app, |this: &mut Self, _, cx| {
            if this._watch_tasks.is_empty() {
                this.start_local_watches(cx);
            }
            cx.notify();
        });
        let bind_input = cx.new(|cx| ComposerInput::new("Bind address", cx));
        bind_input.update(cx, |input, cx| input.set_text("0.0.0.0", cx));
        let port_input = cx.new(|cx| ComposerInput::new("Port", cx));
        port_input.update(cx, |input, cx| input.set_text("27655", cx));
        let endpoint_input = cx.new(|cx| ComposerInput::new("hostname-or-ip:port", cx));
        let name_input = cx.new(|cx| ComposerInput::new("Friendly name (optional)", cx));
        let secret_input = cx.new(|cx| ComposerInput::new("Pairing secret", cx));
        let input_events = [
            &bind_input,
            &port_input,
            &endpoint_input,
            &name_input,
            &secret_input,
        ]
        .into_iter()
        .map(|input| {
            cx.subscribe(input, |_: &mut Self, _, event, cx| {
                if matches!(
                    event,
                    ComposerInputEvent::Edited | ComposerInputEvent::Submitted
                ) {
                    cx.notify();
                }
            })
        })
        .collect();

        let mut page = Self {
            app,
            model: RemoteSettingsState::default(),
            bind_input,
            port_input,
            endpoint_input,
            name_input,
            secret_input,
            rename: None,
            trusted_loaded: false,
            listener_task: None,
            pairing_task: None,
            add_task: None,
            mutation_task: None,
            _watch_tasks: Vec::new(),
            _input_events: input_events,
            _app_observation: app_observation,
        };
        page.start_local_watches(cx);
        page
    }

    fn local_client(&self, cx: &Context<Self>) -> Option<std::sync::Arc<RpcClient>> {
        self.app.read(cx).engine().map(|engine| engine.client())
    }

    fn start_local_watches(&mut self, cx: &mut Context<Self>) {
        let Some(local) = self.local_client(cx) else {
            self.model.listener_error = Some("Local engine is not connected".into());
            return;
        };
        let settings_client = local.clone();
        let bind_input = self.bind_input.clone();
        let port_input = self.port_input.clone();
        self._watch_tasks.push(cx.spawn(async move |this, cx| {
            let mut first = true;
            loop {
                let result = settings_client
                    .call_as::<LanSnapshot>(methods::GET_LAN_SETTINGS, serde_json::Value::Null)
                    .await;
                let loaded = result.is_ok();
                if this
                    .update(cx, |page, cx| {
                        match result {
                            Ok(snapshot) => {
                                page.model.lan = snapshot.settings;
                                page.model.listener = snapshot.status;
                                page.model.listener_error = None;
                                if first {
                                    bind_input.update(cx, |input, cx| {
                                        input.set_text(page.model.lan.bind.ip().to_string(), cx)
                                    });
                                    port_input.update(cx, |input, cx| {
                                        input.set_text(page.model.lan.bind.port().to_string(), cx)
                                    });
                                }
                            }
                            Err(error) => page.model.listener_error = Some(error.to_string()),
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
                if loaded {
                    first = false;
                }
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(1))
                    .await;
            }
        }));

        let remote_client = local.clone();
        self._watch_tasks.push(cx.spawn(async move |this, cx| {
            let stream = remote_client
                .subscribe(methods::WATCH_REMOTES, serde_json::Value::Null)
                .await;
            match stream {
                Ok(mut stream) => {
                    while let Some(value) = stream.recv().await {
                        let parsed = serde_json::from_value::<Vec<RemoteEntry>>(value)
                            .map_err(|error| error.to_string());
                        if this
                            .update(cx, |page, cx| {
                                match parsed {
                                    Ok(remotes) => page.model.remotes = remotes,
                                    Err(error) => page.model.add_error = Some(error),
                                }
                                cx.notify();
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                }
                Err(error) => {
                    this.update(cx, |page, cx| {
                        page.model.add_error = Some(error.to_string());
                        cx.notify();
                    })
                    .ok();
                }
            }
        }));

        self._watch_tasks.push(cx.spawn(async move |this, cx| {
            let stream = local
                .subscribe(methods::WATCH_TRUSTED_CLIENTS, serde_json::Value::Null)
                .await;
            match stream {
                Ok(mut stream) => {
                    while let Some(value) = stream.recv().await {
                        let parsed = serde_json::from_value::<Vec<TrustedClient>>(value)
                            .map_err(|error| error.to_string());
                        if this
                            .update(cx, |page, cx| {
                                match parsed {
                                    Ok(clients) => {
                                        let paired =
                                            page.trusted_loaded
                                                && clients.iter().any(|candidate| {
                                                    !page.model.trusted_clients.iter().any(|old| {
                                                        old.server_id == candidate.server_id
                                                    })
                                                });
                                        page.model.trusted_clients = clients;
                                        page.trusted_loaded = true;
                                        if paired {
                                            page.model.pairing_succeeded();
                                        }
                                    }
                                    Err(error) => page.model.pairing_error = Some(error),
                                }
                                cx.notify();
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                }
                Err(error) => {
                    this.update(cx, |page, cx| {
                        page.model.pairing_error = Some(error.to_string());
                        cx.notify();
                    })
                    .ok();
                }
            }
        }));

        self._watch_tasks.push(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(1))
                    .await;
                if this
                    .update(cx, |page, cx| {
                        page.model.expire_pairing(Utc::now());
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
            }
        }));
    }

    fn listener_settings_from_inputs(
        &self,
        enabled: bool,
        cx: &Context<Self>,
    ) -> Result<LanSettings, String> {
        let ip = self
            .bind_input
            .read(cx)
            .text()
            .trim()
            .parse::<std::net::IpAddr>()
            .map_err(|_| "Bind address must be an IPv4 or IPv6 address".to_string())?;
        let port = self
            .port_input
            .read(cx)
            .text()
            .trim()
            .parse::<u16>()
            .map_err(|_| "Port must be a number from 1 to 65535".to_string())?;
        if port == 0 {
            return Err("Port must be a number from 1 to 65535".into());
        }
        Ok(LanSettings {
            enabled,
            bind: std::net::SocketAddr::new(ip, port),
        })
    }

    fn save_listener(&mut self, enabled: bool, cx: &mut Context<Self>) {
        let settings = match self.listener_settings_from_inputs(enabled, cx) {
            Ok(settings) => settings,
            Err(error) => {
                self.model.listener_error = Some(error);
                cx.notify();
                return;
            }
        };
        let Some(local) = self.local_client(cx) else {
            self.model.listener_error = Some("Local engine is not connected".into());
            return;
        };
        self.model.listener_error = None;
        self.listener_task = Some(cx.spawn(async move |this, cx| {
            let result = local
                .call(
                    methods::SET_LAN_SETTINGS,
                    serde_json::to_value(&settings).unwrap(),
                )
                .await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(_) => page.model.lan = settings,
                    Err(error) => page.model.listener_error = Some(error.to_string()),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn begin_pairing(&mut self, cx: &mut Context<Self>) {
        if !self.model.lan.enabled {
            self.model.pairing_error = Some("Enable remote connections before pairing".into());
            cx.notify();
            return;
        }
        if !self.trusted_loaded {
            self.model.pairing_error = Some("Trusted clients are still loading".into());
            cx.notify();
            return;
        }
        let Some(local) = self.local_client(cx) else {
            self.model.pairing_error = Some("Local engine is not connected".into());
            return;
        };
        let generation = self.model.begin_pairing_request();
        self.pairing_task = Some(cx.spawn(async move |this, cx| {
            let result = local
                .call_as::<BeginPairingReply>(methods::BEGIN_PAIRING, serde_json::Value::Null)
                .await
                .map(|reply| PairingSecret {
                    secret: reply.secret,
                    expires_at: reply.expires_at,
                })
                .map_err(|error| error.to_string());
            this.update(cx, |page, cx| {
                page.model.finish_pairing_request(generation, result);
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn add_remote(&mut self, cx: &mut Context<Self>) {
        let endpoint = self.endpoint_input.read(cx).text().trim().to_string();
        let name = self.name_input.read(cx).text().trim().to_string();
        let secret = match decode_pairing_secret(self.secret_input.read(cx).text()) {
            Ok(secret) => secret,
            Err(error) => {
                self.model.add_error = Some(error);
                cx.notify();
                return;
            }
        };
        let Some(local) = self.local_client(cx) else {
            self.model.add_error = Some("Local engine is not connected".into());
            return;
        };
        let Some(data_dir) = self.app.read(cx).data_dir.clone() else {
            self.model.add_error = Some("Installation identity is unavailable".into());
            cx.notify();
            return;
        };
        let generation = self.model.begin_add_request();
        let secret_input = self.secret_input.clone();
        self.add_task = Some(cx.spawn(async move |this, cx| {
            let result = pair_and_persist_remote(
                &InstallationRemotePairer,
                local.as_ref(),
                &data_dir,
                AddRemoteRequest {
                    endpoint,
                    name,
                    secret,
                },
            )
            .await
            .map(|_| ())
            .map_err(|error| error.to_string());
            let succeeded = result.is_ok();
            this.update(cx, |page, cx| {
                page.model.finish_add_request(generation, result);
                if succeeded && generation == page.model.add_generation {
                    secret_input.update(cx, |input, cx| input.set_text("", cx));
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn open_rename(&mut self, entry: RemoteEntry, cx: &mut Context<Self>) {
        let input = cx.new(|cx| ComposerInput::new("Remote name", cx));
        input.update(cx, |input, cx| input.set_text(entry.name.clone(), cx));
        let events = cx.subscribe(&input, |this: &mut Self, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.submit_rename(cx);
            }
        });
        self.rename = Some(RenameRemote {
            entry,
            input,
            _events: events,
        });
        cx.notify();
    }

    fn submit_rename(&mut self, cx: &mut Context<Self>) {
        let Some(mut rename) = self.rename.take() else {
            return;
        };
        let name = rename.input.read(cx).text().trim().to_string();
        if name.is_empty() {
            self.model.add_error = Some("Remote name cannot be empty".into());
            cx.notify();
            return;
        }
        rename.entry.name = name;
        let Some(local) = self.local_client(cx) else {
            return;
        };
        let entry = rename.entry;
        self.mutation_task = Some(cx.spawn(async move |this, cx| {
            let result = local.put_remote(&entry).await;
            this.update(cx, |page, cx| {
                if let Err(error) = result {
                    page.model.add_error = Some(error);
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn remove_remote(&mut self, server_id: comet_proto::ServerId, cx: &mut Context<Self>) {
        let Some(local) = self.local_client(cx) else {
            return;
        };
        self.mutation_task = Some(cx.spawn(async move |this, cx| {
            let result = local
                .call(
                    methods::REMOVE_REMOTE,
                    serde_json::json!({"serverId": server_id}),
                )
                .await;
            this.update(cx, |page, cx| {
                if let Err(error) = result {
                    page.model.add_error = Some(error.to_string());
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn revoke_client(&mut self, server_id: comet_proto::ServerId, cx: &mut Context<Self>) {
        let Some(local) = self.local_client(cx) else {
            return;
        };
        self.mutation_task = Some(cx.spawn(async move |this, cx| {
            let result = local
                .call(
                    methods::REVOKE_TRUSTED_CLIENT,
                    serde_json::json!({"serverId": server_id}),
                )
                .await;
            this.update(cx, |page, cx| {
                if let Err(error) = result {
                    page.model.pairing_error = Some(error.to_string());
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn reconnect(&mut self, server_id: comet_proto::ServerId, cx: &mut Context<Self>) {
        if let Some(federation) = self.app.read(cx).federation() {
            federation.reconnect(server_id);
        } else {
            self.model.add_error = Some("Connection manager is not ready".into());
        }
        cx.notify();
    }
}

fn field(input: Entity<ComposerInput>) -> AnyElement {
    div()
        .min_w(px(120.0))
        .flex_1()
        .child(crate::popover::dialog_field(input.into_any_element()))
        .into_any_element()
}

fn action_button(theme: &Theme, label: impl Into<SharedString>) -> gpui::Div {
    crate::settings::widgets::ghost_action(theme)
        .hover(crate::settings::widgets::ghost_hover)
        .child(label.into())
}

impl gpui::Render for RemoteConnectionsPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        use crate::settings::widgets;
        let theme = Theme::of(cx).clone();
        self.model.expire_pairing(Utc::now());
        let listener_label = listener_status_label(&self.model.listener);
        let listener_enabled = self.model.lan.enabled;
        let pairing = self.model.pairing.clone();
        let remote_rows: Vec<AnyElement> = self
            .model
            .remotes
            .clone()
            .into_iter()
            .enumerate()
            .map(|(ix, entry)| {
                let rename_entry = entry.clone();
                let reconnect_id = entry.server_id.clone();
                let remove_id = entry.server_id.clone();
                widgets::card_row(&theme, ix == 0)
                    .child(widgets::row_tile(&theme, crate::icons::GLOBAL))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(widgets::row_title(&theme, entry.name.clone()))
                            .child(widgets::meta_line(
                                &theme,
                                vec![
                                    div()
                                        .child(SharedString::from(format!(
                                            "{}:{}",
                                            entry.endpoint.host, entry.endpoint.port
                                        )))
                                        .into_any_element(),
                                    div()
                                        .child(SharedString::from(remote_status_label(
                                            &entry.last_state,
                                        )))
                                        .into_any_element(),
                                ],
                            )),
                    )
                    .child(
                        action_button(&theme, "Rename")
                            .id(("remote-rename", ix))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.open_rename(rename_entry.clone(), cx)
                            })),
                    )
                    .child(
                        action_button(&theme, "Reconnect")
                            .id(("remote-reconnect", ix))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.reconnect(reconnect_id.clone(), cx)
                            })),
                    )
                    .child(
                        action_button(&theme, "Remove")
                            .id(("remote-remove", ix))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.remove_remote(remove_id.clone(), cx)
                            })),
                    )
                    .into_any_element()
            })
            .collect();
        let trusted_rows: Vec<AnyElement> = self
            .model
            .trusted_clients
            .clone()
            .into_iter()
            .enumerate()
            .map(|(ix, client)| {
                let revoke_id = client.server_id.clone();
                widgets::card_row(&theme, ix == 0)
                    .child(widgets::row_tile(&theme, crate::icons::MONITOR))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(widgets::row_title(&theme, client.name))
                            .child(widgets::meta_line(
                                &theme,
                                vec![
                                    div()
                                        .child(SharedString::from(format!(
                                            "Paired {}",
                                            client.paired_at.format("%Y-%m-%d %H:%M UTC")
                                        )))
                                        .into_any_element(),
                                ],
                            )),
                    )
                    .child(
                        action_button(&theme, "Revoke")
                            .id(("trusted-revoke", ix))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.revoke_client(revoke_id.clone(), cx)
                            })),
                    )
                    .into_any_element()
            })
            .collect();

        let listener_card = widgets::section_card(&theme)
            .child(
                widgets::card_row(&theme, true)
                    .child(
                        div()
                            .flex_1()
                            .child(widgets::row_title(&theme, "Enable remote connections"))
                            .child(
                                div()
                                    .mt(px(4.0))
                                    .text_size(px(11.5))
                                    .text_color(theme.text_muted)
                                    .child(SharedString::from(listener_label)),
                            ),
                    )
                    .child(
                        action_button(
                            &theme,
                            if listener_enabled {
                                "Disable"
                            } else {
                                "Enable"
                            },
                        )
                        .id("remote-listener-toggle")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.save_listener(!listener_enabled, cx)
                        })),
                    ),
            )
            .child(
                widgets::card_row(&theme, false)
                    .child(field(self.bind_input.clone()))
                    .child(field(self.port_input.clone()))
                    .child(
                        action_button(&theme, "Apply")
                            .id("remote-listener-apply")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.save_listener(listener_enabled, cx)
                            })),
                    ),
            );

        let pairing_details = div()
            .flex_1()
            .child(widgets::row_title(&theme, "Pair a trusted client"))
            .when_some(pairing, |el, pairing| {
                el.child(
                    div()
                        .mt(px(6.0))
                        .font_family(theme.font_mono.clone())
                        .text_size(px(15.0))
                        .child(SharedString::from(pairing.secret)),
                )
                .child(
                    div()
                        .mt(px(3.0))
                        .text_size(px(11.5))
                        .text_color(theme.text_muted)
                        .child(SharedString::from(format!(
                            "Expires {}",
                            pairing.expires_at.format("%H:%M:%S UTC")
                        ))),
                )
            });
        let pairing_card = widgets::section_card(&theme)
            .child(
                div()
                    .px(px(20.0))
                    .pt(px(14.0))
                    .child(widgets::warning_strip(PAIRING_WARNING)),
            )
            .child(
                widgets::card_row(&theme, false)
                    .child(pairing_details)
                    .child(
                        action_button(&theme, "Begin pairing")
                            .id("begin-pairing")
                            .on_click(cx.listener(|this, _, _, cx| this.begin_pairing(cx))),
                    ),
            );

        let add_card = widgets::section_card(&theme)
            .child(
                widgets::card_row(&theme, true)
                    .child(field(self.endpoint_input.clone()))
                    .child(field(self.name_input.clone())),
            )
            .child(
                widgets::card_row(&theme, false)
                    .child(field(self.secret_input.clone()))
                    .child(
                        action_button(&theme, "Add remote")
                            .id("add-remote")
                            .on_click(cx.listener(|this, _, _, cx| this.add_remote(cx))),
                    ),
            );

        let remote_card = if remote_rows.is_empty() {
            widgets::section_card(&theme).child(
                widgets::card_row(&theme, true).child(
                    div()
                        .text_size(px(12.5))
                        .text_color(theme.text_muted)
                        .child("No configured remotes"),
                ),
            )
        } else {
            widgets::section_card(&theme).children(remote_rows)
        };
        let trusted_card = if trusted_rows.is_empty() {
            widgets::section_card(&theme).child(
                widgets::card_row(&theme, true).child(
                    div()
                        .text_size(px(12.5))
                        .text_color(theme.text_muted)
                        .child("No trusted clients"),
                ),
            )
        } else {
            widgets::section_card(&theme).children(trusted_rows)
        };

        let rename = self.rename.as_ref().map(|rename| {
            widgets::section_card(&theme)
                .child(
                    widgets::card_row(&theme, true)
                        .child(field(rename.input.clone()))
                        .child(
                            action_button(&theme, "Save name")
                                .id("remote-rename-save")
                                .on_click(cx.listener(|this, _, _, cx| this.submit_rename(cx))),
                        ),
                )
                .into_any_element()
        });

        div().id("remote-connections-page").size_full().overflow_y_scroll().child(
            widgets::page_column()
                .child(widgets::page_header(&theme, "Remote connections", None))
                .child(widgets::page_subtitle(&theme, "Listen on your LAN and connect directly to explicitly configured Comet servers."))
                .when_some(self.model.listener_error.clone(), |el, error| el.child(widgets::error_strip(error)))
                .child(listener_card)
                .when_some(self.model.pairing_error.clone(), |el, error| el.child(widgets::error_strip(error)))
                .child(pairing_card)
                .child(trusted_card)
                .when_some(self.model.add_error.clone(), |el, error| el.child(widgets::error_strip(error)))
                .child(add_card)
                .when_some(rename, |el, rename| el.child(rename))
                .child(remote_card)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeDelta, Utc};
    use std::sync::{Arc, Mutex};

    #[test]
    fn listener_is_disabled_until_the_user_enables_it() {
        let state = RemoteSettingsState::default();
        assert!(!state.lan.enabled);
        assert_eq!(state.lan.bind.to_string(), "0.0.0.0:27655");
        assert!(state.pairing.is_none());
    }

    #[test]
    fn pairing_warning_names_the_full_authority_granted() {
        assert!(PAIRING_WARNING.contains("run agents"));
        assert!(PAIRING_WARNING.contains("control terminals"));
        assert!(PAIRING_WARNING.contains("trusted devices"));
    }

    #[test]
    fn pairing_secret_is_cleared_on_expiry_success_and_replacement() {
        let now = Utc::now();
        let mut state = RemoteSettingsState::default();
        let first = state.begin_pairing_request();
        state.finish_pairing_request(
            first,
            Ok(PairingSecret {
                secret: "AAAA-BBBB".into(),
                expires_at: now + TimeDelta::minutes(5),
            }),
        );
        assert_eq!(state.pairing.as_ref().unwrap().secret, "AAAA-BBBB");

        let second = state.begin_pairing_request();
        assert!(state.pairing.is_none(), "replacement clears the old secret");
        state.finish_pairing_request(
            second,
            Ok(PairingSecret {
                secret: "CCCC-DDDD".into(),
                expires_at: now + TimeDelta::seconds(1),
            }),
        );
        state.expire_pairing(now + TimeDelta::seconds(2));
        assert!(state.pairing.is_none());

        let third = state.begin_pairing_request();
        state.finish_pairing_request(
            third,
            Ok(PairingSecret {
                secret: "EEEE-FFFF".into(),
                expires_at: now + TimeDelta::minutes(5),
            }),
        );
        state.pairing_succeeded();
        assert!(state.pairing.is_none());
    }

    #[test]
    fn stale_pairing_and_add_results_cannot_replace_newer_requests() {
        let now = Utc::now();
        let mut state = RemoteSettingsState::default();
        let stale_pairing = state.begin_pairing_request();
        let current_pairing = state.begin_pairing_request();
        state.finish_pairing_request(
            stale_pairing,
            Ok(PairingSecret {
                secret: "STALE".into(),
                expires_at: now + TimeDelta::minutes(5),
            }),
        );
        assert!(state.pairing.is_none());
        state.finish_pairing_request(
            current_pairing,
            Ok(PairingSecret {
                secret: "CURRENT".into(),
                expires_at: now + TimeDelta::minutes(5),
            }),
        );
        assert_eq!(state.pairing.as_ref().unwrap().secret, "CURRENT");

        let stale_add = state.begin_add_request();
        let current_add = state.begin_add_request();
        state.finish_add_request(stale_add, Err("old failure".into()));
        assert!(state.add_error.is_none());
        state.finish_add_request(current_add, Err("offline".into()));
        assert_eq!(state.add_error.as_deref(), Some("offline"));
    }

    #[test]
    fn status_copy_keeps_identity_and_bind_failures_visible() {
        assert_eq!(
            remote_status_label(&comet_proto::RemoteConnectionState::IdentityChanged),
            "Identity changed — remove and pair this server again"
        );
        let status = ListenerStatus::BindFailed {
            bind: "0.0.0.0:27655".parse().unwrap(),
            error: "Address already in use".into(),
        };
        assert_eq!(
            listener_status_label(&status),
            "Could not listen on 0.0.0.0:27655: Address already in use"
        );
    }

    #[test]
    fn pairing_secret_parser_accepts_grouped_base32_and_rejects_wrong_length() {
        assert_eq!(
            decode_pairing_secret("AEBA-GBAF-AYDQ-QCIK-BMGA-2DQP-CA"),
            Ok([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16])
        );
        assert!(decode_pairing_secret("AAAA").is_err());
    }

    struct FakePairer;

    #[async_trait::async_trait]
    impl RemotePairer for FakePairer {
        async fn pair(
            &self,
            _data_dir: &std::path::Path,
            endpoint: &comet_proto::RemoteEndpoint,
            secret: [u8; 16],
        ) -> Result<PinnedRemote, String> {
            assert_eq!(endpoint.host, "buildbox.local");
            assert_eq!(secret, [1; 16]);
            Ok(PinnedRemote {
                server_id: comet_proto::ServerId::new(format!("sha256:{}", "ab".repeat(32))),
                pinned_spki_sha256: "ab".repeat(32),
            })
        }
    }

    #[derive(Default)]
    struct FakeLocalAdmin {
        saved: Arc<Mutex<Vec<comet_proto::RemoteEntry>>>,
    }

    #[async_trait::async_trait]
    impl LocalRemoteAdmin for FakeLocalAdmin {
        async fn put_remote(&self, entry: &comet_proto::RemoteEntry) -> Result<(), String> {
            self.saved.lock().unwrap().push(entry.clone());
            Ok(())
        }
    }

    #[tokio::test]
    async fn pairing_controller_persists_only_the_public_remote_entry_locally() {
        let admin = FakeLocalAdmin::default();
        let saved = admin.saved.clone();
        let entry = pair_and_persist_remote(
            &FakePairer,
            &admin,
            std::path::Path::new("private-installation-dir"),
            AddRemoteRequest {
                endpoint: "buildbox.local:27655".into(),
                name: "Build box".into(),
                secret: [1; 16],
            },
        )
        .await
        .unwrap();

        assert_eq!(
            saved.lock().unwrap().as_slice(),
            std::slice::from_ref(&entry)
        );
        assert_eq!(entry.name, "Build box");
        assert_eq!(entry.endpoint.port, 27655);
        let wire = serde_json::to_string(&entry).unwrap();
        assert!(!wire.contains("private-installation-dir"));
        assert!(!wire.contains("privateKey"));
    }
}
