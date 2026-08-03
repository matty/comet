use chrono::{DateTime, Utc};
use comet_proto::{LanSettings, RemoteConnectionState, RemoteEntry, TrustedClient};
use comet_rpc::{RpcClient, TlsIdentity, methods, pair_client};
use data_encoding::BASE32_NOPAD;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::Path;
use zeroize::Zeroizing;

use gpui::{
    AnyElement, Context, Entity, SharedString, Subscription, Task, Window, div, prelude::*, px,
};

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::state::AppState;
use crate::theme::Theme;

pub const PAIRING_WARNING: &str = "Pairing grants trusted devices full authority to read locally hosted chats, run agents, control terminals, and change repositories on this computer.";

pub struct PairingSecret {
    secret: SecretText,
    pub expires_at: DateTime<Utc>,
}

impl PairingSecret {
    pub fn new(secret: String, expires_at: DateTime<Utc>) -> Self {
        Self {
            secret: SecretText(Zeroizing::new(secret)),
            expires_at,
        }
    }

    pub fn expose(&self) -> &str {
        &self.secret.0
    }
}

impl fmt::Debug for PairingSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PairingSecret")
            .field("secret", &"[REDACTED]")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

struct SecretText(Zeroizing<String>);

impl fmt::Debug for SecretText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

impl PartialEq<&str> for SecretText {
    fn eq(&self, other: &&str) -> bool {
        self.0.as_str() == *other
    }
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

pub struct RemoteSettingsState {
    pub lan: LanSettings,
    pub listener: ListenerStatus,
    pub pairing: Option<PairingSecret>,
    pub trusted_clients: Vec<TrustedClient>,
    pub remotes: Vec<RemoteEntry>,
    pub pairing_error: Option<String>,
    pub add_error: Option<String>,
    pub partial_success: Option<PartialPairing>,
    pub listener_error: Option<String>,
    pub remotes_watch: WatchRecovery,
    pub trusted_watch: WatchRecovery,
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
            partial_success: None,
            listener_error: None,
            remotes_watch: WatchRecovery::default(),
            trusted_watch: WatchRecovery::default(),
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
        self.partial_success = None;
        self.add_generation
    }

    pub fn finish_add_request(&mut self, generation: u64, result: Result<(), AddRemoteError>) {
        if generation != self.add_generation {
            return;
        }
        match result {
            Ok(()) => {}
            Err(AddRemoteError::PartialSuccess {
                remote,
                recovery,
                source,
            }) => {
                self.partial_success = Some(PartialPairing {
                    remote,
                    recovery,
                    source,
                });
            }
            Err(error) => self.add_error = Some(error.to_string()),
        }
    }
}

impl fmt::Debug for RemoteSettingsState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteSettingsState")
            .field("lan", &self.lan)
            .field("listener", &self.listener)
            .field("pairing", &self.pairing)
            .field("trusted_clients", &self.trusted_clients)
            .field("remotes", &self.remotes)
            .field("pairing_error", &self.pairing_error)
            .field("add_error", &self.add_error)
            .field("partial_success", &self.partial_success)
            .field("listener_error", &self.listener_error)
            .finish_non_exhaustive()
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

pub fn decode_pairing_secret(encoded: &str) -> Result<SecretBytes, String> {
    let compact = Zeroizing::new(
        encoded
            .chars()
            .filter(|character| !character.is_whitespace() && *character != '-')
            .flat_map(char::to_uppercase)
            .collect::<String>(),
    );
    let bytes = Zeroizing::new(
        BASE32_NOPAD
            .decode(compact.as_bytes())
            .map_err(|_| "pairing secret must be grouped Base32 text".to_string())?,
    );
    let bytes: [u8; 16] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| "pairing secret must encode exactly 128 bits".to_string())?;
    Ok(SecretBytes(Zeroizing::new(bytes)))
}

pub struct AddRemoteRequest {
    pub endpoint: String,
    pub name: String,
    secret: SecretBytes,
}

impl AddRemoteRequest {
    pub fn new(endpoint: impl Into<String>, name: impl Into<String>, secret: [u8; 16]) -> Self {
        Self {
            endpoint: endpoint.into(),
            name: name.into(),
            secret: SecretBytes(Zeroizing::new(secret)),
        }
    }

    fn from_secret(
        endpoint: impl Into<String>,
        name: impl Into<String>,
        secret: SecretBytes,
    ) -> Self {
        Self {
            endpoint: endpoint.into(),
            name: name.into(),
            secret,
        }
    }
}

pub struct SecretBytes(Zeroizing<[u8; 16]>);

impl SecretBytes {
    fn expose(&self) -> &[u8; 16] {
        &self.0
    }
}

impl fmt::Debug for SecretBytes {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

impl PartialEq<[u8; 16]> for SecretBytes {
    fn eq(&self, other: &[u8; 16]) -> bool {
        self.0.as_ref() == other
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinnedRemote {
    pub server_id: comet_proto::ServerId,
    pub pinned_spki_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicRemoteDetails {
    pub server_id: comet_proto::ServerId,
    pub endpoint: comet_proto::RemoteEndpoint,
    pub name: String,
    pub pinned_spki_sha256: String,
}

pub const PARTIAL_PAIRING_RECOVERY: &str = "Pairing succeeded on the remote, but this computer could not save it. On the remote computer, revoke this device, then start a fresh pairing session and add it again.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartialPairing {
    pub remote: PublicRemoteDetails,
    pub recovery: &'static str,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddRemoteError {
    Pairing(String),
    PartialSuccess {
        remote: PublicRemoteDetails,
        recovery: &'static str,
        source: String,
    },
}

impl fmt::Display for AddRemoteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pairing(error) => formatter.write_str(error),
            Self::PartialSuccess {
                recovery, source, ..
            } => {
                write!(formatter, "{recovery} Save error: {source}")
            }
        }
    }
}

impl From<String> for AddRemoteError {
    fn from(error: String) -> Self {
        Self::Pairing(error)
    }
}

impl From<&str> for AddRemoteError {
    fn from(error: &str) -> Self {
        Self::Pairing(error.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum OperationKind {
    Rename,
    Remove,
    Revoke,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OperationKey {
    kind: OperationKind,
    server_id: comet_proto::ServerId,
}

impl OperationKey {
    pub fn rename(server_id: comet_proto::ServerId) -> Self {
        Self {
            kind: OperationKind::Rename,
            server_id,
        }
    }

    pub fn remove(server_id: comet_proto::ServerId) -> Self {
        Self {
            kind: OperationKind::Remove,
            server_id,
        }
    }

    pub fn revoke(server_id: comet_proto::ServerId) -> Self {
        Self {
            kind: OperationKind::Revoke,
            server_id,
        }
    }
}

#[derive(Debug, Default)]
pub struct OperationTracker {
    pending: HashSet<OperationKey>,
    errors: HashMap<OperationKey, String>,
}

impl OperationTracker {
    pub fn begin(&mut self, key: OperationKey) -> bool {
        if !self.pending.insert(key.clone()) {
            return false;
        }
        self.errors.remove(&key);
        true
    }

    pub fn finish(&mut self, key: OperationKey, result: Result<(), String>) {
        self.pending.remove(&key);
        if let Err(error) = result {
            self.errors.insert(key, error);
        }
    }

    pub fn is_pending(&self, key: &OperationKey) -> bool {
        self.pending.contains(key)
    }

    pub fn error(&self, key: &OperationKey) -> Option<&str> {
        self.errors.get(key).map(String::as_str)
    }

    fn first_error(&self) -> Option<String> {
        self.errors.values().next().cloned()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DestructiveAction {
    RemoveRemote { server_id: comet_proto::ServerId },
    RevokeClient { server_id: comet_proto::ServerId },
}

#[derive(Debug, Default)]
pub struct DestructiveConfirmation {
    pending: Option<(DestructiveAction, String)>,
    confirmed: bool,
}

impl DestructiveConfirmation {
    pub fn request_remove(&mut self, server_id: comet_proto::ServerId, name: &str, endpoint: &str) {
        self.pending = Some((
            DestructiveAction::RemoveRemote { server_id },
            format!(
                "Remove {name} ({endpoint}) from this computer? This does not revoke this device on the remote computer."
            ),
        ));
        self.confirmed = false;
    }

    pub fn request_revoke(&mut self, server_id: comet_proto::ServerId, name: &str) {
        self.pending = Some((
            DestructiveAction::RevokeClient { server_id },
            format!(
                "Revoke {name}? Its active connection will close and future connections will be rejected."
            ),
        ));
        self.confirmed = false;
    }

    pub fn copy(&self) -> Option<&str> {
        self.pending.as_ref().map(|(_, copy)| copy.as_str())
    }

    pub fn confirm(&mut self) {
        self.confirmed = self.pending.is_some();
    }

    pub fn cancel(&mut self) {
        self.pending = None;
        self.confirmed = false;
    }

    pub fn take_confirmed(&mut self) -> Option<DestructiveAction> {
        if !self.confirmed {
            return None;
        }
        self.confirmed = false;
        self.pending.take().map(|(action, _)| action)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchHealth {
    Loading,
    Live,
    Stale { error: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchRecovery {
    pub health: WatchHealth,
    failures: u32,
}

impl Default for WatchRecovery {
    fn default() -> Self {
        Self {
            health: WatchHealth::Loading,
            failures: 0,
        }
    }
}

impl WatchRecovery {
    pub fn connected(&mut self) {
        self.health = WatchHealth::Live;
        self.failures = 0;
    }

    pub fn disconnected(&mut self, error: impl Into<String>) {
        self.health = WatchHealth::Stale {
            error: error.into(),
        };
        self.failures = self.failures.saturating_add(1);
    }

    pub fn retry_delay(&self) -> std::time::Duration {
        let exponent = self.failures.saturating_sub(1).min(4);
        std::time::Duration::from_millis(250 * (1_u64 << exponent))
    }
}

fn watch_status_message(label: &str, watch: &WatchRecovery) -> Option<String> {
    match &watch.health {
        WatchHealth::Stale { error } => Some(format!(
            "{label} is offline; showing last known data and reconnecting: {error}"
        )),
        WatchHealth::Loading | WatchHealth::Live => None,
    }
}

#[async_trait::async_trait]
pub trait RemotePairer: Send + Sync {
    async fn pair(
        &self,
        data_dir: &Path,
        endpoint: &comet_proto::RemoteEndpoint,
        secret: &[u8; 16],
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
        secret: &[u8; 16],
    ) -> Result<PinnedRemote, String> {
        // The private key is loaded only in this controller and is never part
        // of AddRemoteRequest, RemoteSettingsState, or any GPUI entity.
        let identity = comet_identity::DeviceIdentity::load_or_create(data_dir)
            .map_err(|error| error.to_string())?;
        let tls =
            TlsIdentity::from_device_identity(&identity).map_err(|error| error.to_string())?;
        let address = if endpoint.host.contains(':') {
            format!("[{}]:{}", endpoint.host, endpoint.port)
        } else {
            format!("{}:{}", endpoint.host, endpoint.port)
        };
        let pinned = pair_client(address, &tls, *secret)
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
) -> Result<RemoteEntry, AddRemoteError>
where
    P: RemotePairer + ?Sized,
    A: LocalRemoteAdmin + ?Sized,
{
    let endpoint =
        comet_proto::RemoteEndpoint::parse(&request.endpoint).map_err(AddRemoteError::Pairing)?;
    let pinned = pairer
        .pair(data_dir, &endpoint, request.secret.expose())
        .await
        .map_err(AddRemoteError::Pairing)?;
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
    if let Err(source) = local_admin.put_remote(&entry).await {
        return Err(AddRemoteError::PartialSuccess {
            remote: PublicRemoteDetails {
                server_id: entry.server_id.clone(),
                endpoint: entry.endpoint.clone(),
                name: entry.name.clone(),
                pinned_spki_sha256: entry.pinned_spki_sha256.clone(),
            },
            recovery: PARTIAL_PAIRING_RECOVERY,
            source,
        });
    }
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
    server_id: comet_proto::ServerId,
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
    confirmation: DestructiveConfirmation,
    operations: OperationTracker,
    trusted_loaded: bool,
    listener_task: Option<Task<()>>,
    pairing_task: Option<Task<()>>,
    add_task: Option<Task<()>>,
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
            confirmation: DestructiveConfirmation::default(),
            operations: OperationTracker::default(),
            trusted_loaded: false,
            listener_task: None,
            pairing_task: None,
            add_task: None,
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
            loop {
                let mut disconnected = match remote_client
                    .subscribe(methods::WATCH_REMOTES, serde_json::Value::Null)
                    .await
                {
                    Ok(mut stream) => {
                        if this
                            .update(cx, |page, cx| {
                                page.model.remotes_watch.connected();
                                cx.notify();
                            })
                            .is_err()
                        {
                            return;
                        }
                        let mut reason = "Remote registry stream closed".to_string();
                        while let Some(value) = stream.recv().await {
                            match serde_json::from_value::<Vec<RemoteEntry>>(value) {
                                Ok(remotes) => {
                                    if this
                                        .update(cx, |page, cx| {
                                            page.model.remotes = remotes;
                                            cx.notify();
                                        })
                                        .is_err()
                                    {
                                        return;
                                    }
                                }
                                Err(error) => {
                                    reason = format!("Invalid remote registry snapshot: {error}");
                                    break;
                                }
                            }
                        }
                        reason
                    }
                    Err(error) => error.to_string(),
                };
                let delay = match this.update(cx, |page, cx| {
                    page.model
                        .remotes_watch
                        .disconnected(std::mem::take(&mut disconnected));
                    cx.notify();
                    page.model.remotes_watch.retry_delay()
                }) {
                    Ok(delay) => delay,
                    Err(_) => return,
                };
                cx.background_executor().timer(delay).await;
            }
        }));

        self._watch_tasks.push(cx.spawn(async move |this, cx| {
            loop {
                let mut disconnected = match local
                    .subscribe(methods::WATCH_TRUSTED_CLIENTS, serde_json::Value::Null)
                    .await
                {
                    Ok(mut stream) => {
                        if this
                            .update(cx, |page, cx| {
                                page.model.trusted_watch.connected();
                                cx.notify();
                            })
                            .is_err()
                        {
                            return;
                        }
                        let mut reason = "Trusted-client stream closed".to_string();
                        while let Some(value) = stream.recv().await {
                            match serde_json::from_value::<Vec<TrustedClient>>(value) {
                                Ok(clients) => {
                                    if this
                                        .update(cx, |page, cx| {
                                            let paired = page.trusted_loaded
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
                                            cx.notify();
                                        })
                                        .is_err()
                                    {
                                        return;
                                    }
                                }
                                Err(error) => {
                                    reason = format!("Invalid trusted-client snapshot: {error}");
                                    break;
                                }
                            }
                        }
                        reason
                    }
                    Err(error) => error.to_string(),
                };
                let delay = match this.update(cx, |page, cx| {
                    page.trusted_loaded = false;
                    page.model
                        .trusted_watch
                        .disconnected(std::mem::take(&mut disconnected));
                    cx.notify();
                    page.model.trusted_watch.retry_delay()
                }) {
                    Ok(delay) => delay,
                    Err(_) => return,
                };
                cx.background_executor().timer(delay).await;
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
                .map(|reply| PairingSecret::new(reply.secret, reply.expires_at))
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
        let endpoint = self.endpoint_input.read(cx).text().to_string();
        let name = self.name_input.read(cx).text().trim().to_string();
        let decoded = decode_pairing_secret(self.secret_input.read(cx).text());
        self.secret_input
            .update(cx, |input, cx| input.set_text("", cx));
        let secret = match decoded {
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
        self.add_task = Some(cx.spawn(async move |this, cx| {
            let result = pair_and_persist_remote(
                &InstallationRemotePairer,
                local.as_ref(),
                &data_dir,
                AddRemoteRequest::from_secret(endpoint, name, secret),
            )
            .await
            .map(|_| ());
            this.update(cx, |page, cx| {
                page.model.finish_add_request(generation, result);
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
            server_id: entry.server_id,
            input,
            _events: events,
        });
        cx.notify();
    }

    fn submit_rename(&mut self, cx: &mut Context<Self>) {
        let Some(rename) = self.rename.take() else {
            return;
        };
        let name = rename.input.read(cx).text().trim().to_string();
        if name.is_empty() {
            self.model.add_error = Some("Remote name cannot be empty".into());
            cx.notify();
            return;
        }
        let Some(local) = self.local_client(cx) else {
            return;
        };
        let server_id = rename.server_id;
        let operation = OperationKey::rename(server_id.clone());
        if !self.operations.begin(operation.clone()) {
            return;
        }
        cx.spawn(async move |this, cx| {
            let result = local
                .call(
                    methods::RENAME_REMOTE,
                    serde_json::json!({"serverId": server_id, "name": name}),
                )
                .await
                .map(|_| ())
                .map_err(|error| error.to_string());
            this.update(cx, |page, cx| {
                page.operations.finish(operation, result);
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn request_remove(&mut self, entry: &RemoteEntry, cx: &mut Context<Self>) {
        self.confirmation.request_remove(
            entry.server_id.clone(),
            &entry.name,
            &format!("{}:{}", entry.endpoint.host, entry.endpoint.port),
        );
        cx.notify();
    }

    fn request_revoke(&mut self, client: &TrustedClient, cx: &mut Context<Self>) {
        self.confirmation
            .request_revoke(client.server_id.clone(), &client.name);
        cx.notify();
    }

    fn cancel_destructive(&mut self, cx: &mut Context<Self>) {
        self.confirmation.cancel();
        cx.notify();
    }

    fn confirm_destructive(&mut self, cx: &mut Context<Self>) {
        self.confirmation.confirm();
        let Some(action) = self.confirmation.take_confirmed() else {
            return;
        };
        match action {
            DestructiveAction::RemoveRemote { server_id } => self.remove_remote(server_id, cx),
            DestructiveAction::RevokeClient { server_id } => self.revoke_client(server_id, cx),
        }
    }

    fn remove_remote(&mut self, server_id: comet_proto::ServerId, cx: &mut Context<Self>) {
        let Some(local) = self.local_client(cx) else {
            return;
        };
        let operation = OperationKey::remove(server_id.clone());
        if !self.operations.begin(operation.clone()) {
            return;
        }
        cx.spawn(async move |this, cx| {
            let result = local
                .call(
                    methods::REMOVE_REMOTE,
                    serde_json::json!({"serverId": server_id}),
                )
                .await
                .map(|_| ())
                .map_err(|error| error.to_string());
            this.update(cx, |page, cx| {
                page.operations.finish(operation, result);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn revoke_client(&mut self, server_id: comet_proto::ServerId, cx: &mut Context<Self>) {
        let Some(local) = self.local_client(cx) else {
            return;
        };
        let operation = OperationKey::revoke(server_id.clone());
        if !self.operations.begin(operation.clone()) {
            return;
        }
        cx.spawn(async move |this, cx| {
            let result = local
                .call(
                    methods::REVOKE_TRUSTED_CLIENT,
                    serde_json::json!({"serverId": server_id}),
                )
                .await
                .map(|_| ())
                .map_err(|error| error.to_string());
            this.update(cx, |page, cx| {
                page.operations.finish(operation, result);
                cx.notify();
            })
            .ok();
        })
        .detach();
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
        let pairing = self.model.pairing.as_ref();
        let remote_rows: Vec<AnyElement> = self
            .model
            .remotes
            .clone()
            .into_iter()
            .enumerate()
            .map(|(ix, entry)| {
                let rename_entry = entry.clone();
                let reconnect_id = entry.server_id.clone();
                let remove_entry = entry.clone();
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
                                this.request_remove(&remove_entry, cx)
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
                let revoke_client = client.clone();
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
                                this.request_revoke(&revoke_client, cx)
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
                        .child(SharedString::from(pairing.expose().to_string())),
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
        let confirmation = self.confirmation.copy().map(str::to_string).map(|copy| {
            widgets::section_card(&theme)
                .child(
                    div()
                        .px(px(20.0))
                        .pt(px(14.0))
                        .child(widgets::warning_strip(copy)),
                )
                .child(
                    widgets::card_row(&theme, false)
                        .child(div().flex_1())
                        .child(
                            action_button(&theme, "Cancel")
                                .id("destructive-cancel")
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.cancel_destructive(cx)),
                                ),
                        )
                        .child(
                            action_button(&theme, "Confirm")
                                .id("destructive-confirm")
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.confirm_destructive(cx)),
                                ),
                        ),
                )
                .into_any_element()
        });
        let partial_success = self.model.partial_success.clone().map(|partial| {
            let endpoint = format!(
                "{}:{}",
                partial.remote.endpoint.host, partial.remote.endpoint.port
            );
            widgets::warning_strip(format!(
                "{} ({endpoint}, pin {}). {} Save error: {}",
                partial.remote.name,
                partial.remote.pinned_spki_sha256,
                partial.recovery,
                partial.source
            ))
            .into_any_element()
        });
        let operation_error = self.operations.first_error();
        let trusted_watch_error =
            watch_status_message("Trusted clients", &self.model.trusted_watch);
        let remotes_watch_error =
            watch_status_message("Remote registry", &self.model.remotes_watch);

        div().id("remote-connections-page").size_full().overflow_y_scroll().child(
            widgets::page_column()
                .child(widgets::page_header(&theme, "Remote connections", None))
                .child(widgets::page_subtitle(&theme, "Listen on your LAN and connect directly to explicitly configured Comet servers."))
                .when_some(self.model.listener_error.clone(), |el, error| el.child(widgets::error_strip(error)))
                .child(listener_card)
                .when_some(self.model.pairing_error.clone(), |el, error| el.child(widgets::error_strip(error)))
                .child(pairing_card)
                .when_some(trusted_watch_error, |el, error| el.child(widgets::error_strip(error)))
                .child(trusted_card)
                .when_some(self.model.add_error.clone(), |el, error| el.child(widgets::error_strip(error)))
                .when_some(operation_error, |el, error| el.child(widgets::error_strip(error)))
                .when_some(partial_success, |el, partial| el.child(partial))
                .child(add_card)
                .when_some(rename, |el, rename| el.child(rename))
                .when_some(confirmation, |el, confirmation| el.child(confirmation))
                .when_some(remotes_watch_error, |el, error| el.child(widgets::error_strip(error)))
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
            Ok(PairingSecret::new(
                "AAAA-BBBB".into(),
                now + TimeDelta::minutes(5),
            )),
        );
        assert_eq!(state.pairing.as_ref().unwrap().secret, "AAAA-BBBB");

        let second = state.begin_pairing_request();
        assert!(state.pairing.is_none(), "replacement clears the old secret");
        state.finish_pairing_request(
            second,
            Ok(PairingSecret::new(
                "CCCC-DDDD".into(),
                now + TimeDelta::seconds(1),
            )),
        );
        state.expire_pairing(now + TimeDelta::seconds(2));
        assert!(state.pairing.is_none());

        let third = state.begin_pairing_request();
        state.finish_pairing_request(
            third,
            Ok(PairingSecret::new(
                "EEEE-FFFF".into(),
                now + TimeDelta::minutes(5),
            )),
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
            Ok(PairingSecret::new(
                "STALE".into(),
                now + TimeDelta::minutes(5),
            )),
        );
        assert!(state.pairing.is_none());
        state.finish_pairing_request(
            current_pairing,
            Ok(PairingSecret::new(
                "CURRENT".into(),
                now + TimeDelta::minutes(5),
            )),
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
        let decoded = decode_pairing_secret("AEBA-GBAF-AYDQ-QCIK-BMGA-2DQP-CA").unwrap();
        assert_eq!(
            decoded.expose(),
            &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]
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
            secret: &[u8; 16],
        ) -> Result<PinnedRemote, String> {
            assert_eq!(endpoint.host, "buildbox.local");
            assert_eq!(secret, &[1; 16]);
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
            AddRemoteRequest::new("buildbox.local:27655", "Build box", [1; 16]),
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

    struct FailingLocalAdmin {
        calls: Arc<Mutex<usize>>,
    }

    #[async_trait::async_trait]
    impl LocalRemoteAdmin for FailingLocalAdmin {
        async fn put_remote(&self, _entry: &comet_proto::RemoteEntry) -> Result<(), String> {
            *self.calls.lock().unwrap() += 1;
            Err("disk full".into())
        }
    }

    #[tokio::test]
    async fn pair_success_then_store_failure_is_partial_success_without_retry() {
        let calls = Arc::new(Mutex::new(0));
        let error = pair_and_persist_remote(
            &FakePairer,
            &FailingLocalAdmin {
                calls: calls.clone(),
            },
            std::path::Path::new("private-installation-dir"),
            AddRemoteRequest::new("buildbox.local:27655", "Build box", [1; 16]),
        )
        .await
        .unwrap_err();

        let AddRemoteError::PartialSuccess {
            remote,
            recovery,
            source,
        } = error
        else {
            panic!("expected partial success");
        };
        assert_eq!(
            *calls.lock().unwrap(),
            1,
            "controller never retries PUT_REMOTE"
        );
        assert_eq!(remote.name, "Build box");
        assert!(source.contains("disk full"));
        assert!(recovery.contains("revoke this device"));
        assert!(recovery.contains("start a fresh pairing session"));
        assert!(!recovery.to_lowercase().contains("retry"));
    }

    #[test]
    fn partial_success_is_preserved_in_ui_state_with_public_recovery_details() {
        let mut state = RemoteSettingsState::default();
        let generation = state.begin_add_request();
        let remote = PublicRemoteDetails {
            server_id: comet_proto::ServerId::new("sha256:remote"),
            endpoint: comet_proto::RemoteEndpoint::parse("buildbox.local:27655").unwrap(),
            name: "Build box".into(),
            pinned_spki_sha256: "ab".repeat(32),
        };
        state.finish_add_request(
            generation,
            Err(AddRemoteError::PartialSuccess {
                remote: remote.clone(),
                recovery: PARTIAL_PAIRING_RECOVERY,
                source: "disk full".into(),
            }),
        );
        let partial = state
            .partial_success
            .as_ref()
            .expect("visible recovery state");
        assert_eq!(partial.remote, remote);
        assert!(partial.recovery.contains("revoke this device"));
        assert!(state.add_error.is_none());
    }

    #[test]
    fn pairing_secret_debug_is_redacted_and_cleanup_removes_it() {
        let now = Utc::now();
        let mut state = RemoteSettingsState::default();
        let generation = state.begin_pairing_request();
        state.finish_pairing_request(
            generation,
            Ok(PairingSecret::new(
                "TOPS-ECRE-T123".into(),
                now + TimeDelta::seconds(1),
            )),
        );
        assert!(!format!("{:?}", state).contains("TOPS-ECRE-T123"));
        state.expire_pairing(now + TimeDelta::seconds(2));
        assert!(state.pairing.is_none());
    }

    #[test]
    fn destructive_confirmation_cancel_emits_no_action_and_confirm_is_qualified() {
        let mut confirmation = DestructiveConfirmation::default();
        let server_id = comet_proto::ServerId::new("sha256:remote");
        confirmation.request_remove(server_id.clone(), "Build box", "buildbox.local:27655");
        assert!(confirmation.take_confirmed().is_none());
        assert!(confirmation.copy().unwrap().contains("Build box"));
        assert!(
            confirmation
                .copy()
                .unwrap()
                .contains("buildbox.local:27655")
        );
        confirmation.cancel();
        assert!(confirmation.take_confirmed().is_none());

        confirmation.request_revoke(server_id.clone(), "Laptop");
        confirmation.confirm();
        assert_eq!(
            confirmation.take_confirmed(),
            Some(DestructiveAction::RevokeClient { server_id })
        );
    }

    #[test]
    fn independent_operations_do_not_cancel_or_overwrite_each_other() {
        let mut operations = OperationTracker::default();
        let rename = OperationKey::rename(comet_proto::ServerId::new("sha256:a"));
        let remove = OperationKey::remove(comet_proto::ServerId::new("sha256:b"));
        let revoke = OperationKey::revoke(comet_proto::ServerId::new("sha256:c"));
        assert!(operations.begin(rename.clone()));
        assert!(operations.begin(remove.clone()));
        assert!(operations.begin(revoke.clone()));
        assert!(
            !operations.begin(rename.clone()),
            "same operation is bounded"
        );

        operations.finish(remove.clone(), Err("remove failed".into()));
        operations.finish(rename.clone(), Ok(()));
        operations.finish(revoke.clone(), Err("revoke failed".into()));
        assert!(!operations.is_pending(&rename));
        assert_eq!(operations.error(&remove), Some("remove failed"));
        assert_eq!(operations.error(&revoke), Some("revoke failed"));
    }

    #[test]
    fn watch_close_marks_stale_and_recovery_resets_bounded_backoff() {
        let mut watch = WatchRecovery::default();
        watch.connected();
        watch.disconnected("stream closed");
        assert!(matches!(watch.health, WatchHealth::Stale { .. }));
        assert_eq!(watch.retry_delay(), std::time::Duration::from_millis(250));
        watch.disconnected("closed again");
        assert_eq!(watch.retry_delay(), std::time::Duration::from_millis(500));
        watch.connected();
        assert_eq!(watch.health, WatchHealth::Live);
        assert_eq!(watch.retry_delay(), std::time::Duration::from_millis(250));
    }

    #[test]
    fn stale_watch_message_explains_cached_state_and_recovery() {
        let mut watch = WatchRecovery::default();
        watch.connected();
        watch.disconnected("connection reset");

        let message =
            watch_status_message("Remote registry", &watch).expect("stale watches are visible");
        assert!(message.contains("last known data"));
        assert!(message.contains("reconnecting"));
        assert!(message.contains("connection reset"));
        assert!(watch_status_message("Remote registry", &WatchRecovery::default()).is_none());
    }
}
