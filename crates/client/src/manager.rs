use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use comet_identity::DeviceIdentity;
use comet_proto::{
    PROTOCOL_VERSION, RemoteConnectionState, RemoteEntry, ServerHello, ServerId, ServerRef,
};
use comet_rpc::{LanConnectError, PinnedServer, RpcClient, TlsIdentity, connect_lan_rpc, methods};
use futures::future::BoxFuture;
use tokio::sync::mpsc;

use crate::server::{
    ConnectedExit, SupervisorCommand, clear_selected_transcript, supervise_connected,
};
use crate::{FederationCommand, FederationEvent, ServerState};

#[derive(Debug, thiserror::Error)]
pub enum RemoteConnectError {
    #[error("identity changed")]
    IdentityChanged,
    #[error("invalid remote configuration: {0}")]
    InvalidConfiguration(String),
    #[error("transport: {0}")]
    Transport(String),
}

pub trait RemoteConnector: Send + Sync + 'static {
    fn connect<'a>(
        &'a self,
        entry: &'a RemoteEntry,
        identity: &'a TlsIdentity,
    ) -> BoxFuture<'a, Result<RpcClient, RemoteConnectError>>;
}

struct LanConnector;

impl RemoteConnector for LanConnector {
    fn connect<'a>(
        &'a self,
        entry: &'a RemoteEntry,
        identity: &'a TlsIdentity,
    ) -> BoxFuture<'a, Result<RpcClient, RemoteConnectError>> {
        Box::pin(async move {
            let pin =
                PinnedServer::from_spki_sha256(entry.server_id.clone(), &entry.pinned_spki_sha256)
                    .map_err(|error| RemoteConnectError::InvalidConfiguration(error.to_string()))?;
            let endpoint = format!("{}:{}", entry.endpoint.host, entry.endpoint.port);
            connect_lan_rpc(endpoint.as_str(), identity, &pin)
                .await
                .map_err(|error| match error {
                    LanConnectError::IdentityChanged => RemoteConnectError::IdentityChanged,
                    LanConnectError::Transport(message) => RemoteConnectError::Transport(message),
                })
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FederationError {
    #[error("identity: {0}")]
    Identity(#[from] comet_identity::IdentityError),
    #[error("TLS identity: {0}")]
    Tls(#[from] comet_rpc::TlsIdentityError),
    #[error("local RPC: {0}")]
    LocalRpc(#[from] comet_rpc::RpcError),
}

pub struct Federation {
    events: mpsc::UnboundedReceiver<FederationEvent>,
    commands: mpsc::UnboundedSender<FederationCommand>,
    _task: tokio::task::JoinHandle<()>,
}

impl Federation {
    pub async fn new(
        local: RpcClient,
        data_dir: impl AsRef<Path>,
    ) -> Result<Self, FederationError> {
        Self::with_connector(local, data_dir, Arc::new(LanConnector)).await
    }

    pub async fn with_connector(
        local: RpcClient,
        data_dir: impl AsRef<Path>,
        connector: Arc<dyn RemoteConnector>,
    ) -> Result<Self, FederationError> {
        let identity = DeviceIdentity::load_or_create(data_dir.as_ref())?;
        let tls = Arc::new(TlsIdentity::from_device_identity(&identity)?);
        let local = Arc::new(local);
        let hello: ServerHello = local
            .call_as(methods::SERVER_HELLO, serde_json::Value::Null)
            .await?;
        let remotes = local
            .subscribe(methods::WATCH_REMOTES, serde_json::Value::Null)
            .await?;
        let (events_tx, events) = mpsc::unbounded_channel();
        let (commands, commands_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(run_manager(
            local,
            hello,
            remotes,
            tls,
            connector,
            events_tx,
            commands_rx,
        ));
        Ok(Self {
            events,
            commands,
            _task: task,
        })
    }

    pub async fn recv(&mut self) -> Option<FederationEvent> {
        self.events.recv().await
    }

    pub fn command_sender(&self) -> mpsc::UnboundedSender<FederationCommand> {
        self.commands.clone()
    }

    pub fn send(&self, command: FederationCommand) -> Result<(), FederationCommand> {
        self.commands.send(command).map_err(|error| error.0)
    }
}

impl Drop for Federation {
    fn drop(&mut self) {
        let _ = self.commands.send(FederationCommand::Shutdown);
    }
}

struct Supervisor {
    entry: Option<RemoteEntry>,
    commands: mpsc::UnboundedSender<SupervisorCommand>,
    task: tokio::task::JoinHandle<()>,
}

struct RemoteContext {
    local: Arc<RpcClient>,
    tls: Arc<TlsIdentity>,
    connector: Arc<dyn RemoteConnector>,
    events: mpsc::UnboundedSender<FederationEvent>,
}

async fn run_manager(
    local: Arc<RpcClient>,
    local_hello: ServerHello,
    mut registry: mpsc::UnboundedReceiver<serde_json::Value>,
    tls: Arc<TlsIdentity>,
    connector: Arc<dyn RemoteConnector>,
    events: mpsc::UnboundedSender<FederationEvent>,
    mut commands: mpsc::UnboundedReceiver<FederationCommand>,
) {
    let mut supervisors = HashMap::<ServerId, Supervisor>::new();
    let (local_tx, local_rx) = mpsc::unbounded_channel();
    let local_id = local_hello.server_id.clone();
    let local_name = local_hello.name.clone();
    let _ = events.send(FederationEvent::ServerChanged(ServerState::empty(
        local_id.clone(),
        local_name.clone(),
        RemoteConnectionState::Online,
    )));
    let local_task = spawn_local_resources(
        local.clone(),
        local_hello,
        local_name.clone(),
        events.clone(),
        local_rx,
    );
    supervisors.insert(
        local_id.clone(),
        Supervisor {
            entry: None,
            commands: local_tx,
            task: local_task,
        },
    );
    let remote_context = RemoteContext {
        local: local.clone(),
        tls,
        connector,
        events: events.clone(),
    };

    let mut selected: Option<ServerRef> = None;
    loop {
        tokio::select! {
            snapshot = registry.recv() => match snapshot {
                Some(value) => match serde_json::from_value::<Vec<RemoteEntry>>(value) {
                    Ok(entries) => reconcile(&mut supervisors, &local_id, entries, &mut selected, &remote_context).await,
                    Err(error) => { let _ = events.send(FederationEvent::Notice { server_id: local_id.clone(), message: format!("invalid remote registry: {error}") }); }
                },
                None => {
                    if let Some(chat) = selected.take() {
                        let _ = events.send(FederationEvent::Transcript { chat, entries: Vec::new() });
                    }
                    let remote_ids: Vec<_> = supervisors.keys().filter(|id| *id != &local_id).cloned().collect();
                    for id in remote_ids {
                        if let Some(supervisor) = supervisors.remove(&id) { stop_supervisor(supervisor).await; }
                        let _ = events.send(FederationEvent::ServerRemoved(id));
                    }
                    let _ = events.send(FederationEvent::ServerChanged(ServerState::offline(local_id.clone(), local_name.clone())));
                    break;
                },
            },
            command = commands.recv() => match command {
                Some(FederationCommand::Call { server_id, method, params }) => {
                    if server_id != local_id && is_local_admin(method) {
                        let _ = events.send(FederationEvent::Notice { server_id, message: format!("{method} is available only on trusted local IPC") });
                    } else if let Some(supervisor) = supervisors.get(&server_id) {
                        let _ = supervisor.commands.send(SupervisorCommand::Call(method, params));
                    }
                }
                Some(FederationCommand::WatchTranscript(chat)) => {
                    selected = chat.clone();
                    for supervisor in supervisors.values() { let _ = supervisor.commands.send(SupervisorCommand::WatchTranscript(None)); }
                    if let Some(chat) = chat && let Some(supervisor) = supervisors.get(&chat.server_id) {
                        let _ = supervisor.commands.send(SupervisorCommand::WatchTranscript(Some(chat.local_id)));
                    }
                }
                Some(FederationCommand::Reconnect(server_id)) => {
                    if let Some(supervisor) = supervisors.get(&server_id) { let _ = supervisor.commands.send(SupervisorCommand::Reconnect); }
                }
                Some(FederationCommand::Shutdown) | None => break,
            }
        }
    }
    for (_, supervisor) in supervisors {
        stop_supervisor(supervisor).await;
    }
}

async fn stop_supervisor(supervisor: Supervisor) {
    let _ = supervisor.commands.send(SupervisorCommand::Shutdown);
    supervisor.task.abort();
    let _ = supervisor.task.await;
}

fn spawn_local_resources(
    local: Arc<RpcClient>,
    hello: ServerHello,
    name: String,
    events: mpsc::UnboundedSender<FederationEvent>,
    mut commands: mpsc::UnboundedReceiver<SupervisorCommand>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let id = hello.server_id.clone();
        let mut selected_chat = None;
        let _ = supervise_connected(
            local,
            hello,
            name.clone(),
            events.clone(),
            &mut commands,
            &mut selected_chat,
        )
        .await;
        clear_selected_transcript(&id, &selected_chat, &events);
        let _ = events.send(FederationEvent::ServerChanged(ServerState::offline(
            id, name,
        )));
    })
}

fn is_local_admin(method: &str) -> bool {
    matches!(
        method,
        methods::WATCH_REMOTES
            | methods::PUT_REMOTE
            | methods::REMOVE_REMOTE
            | methods::REPORT_REMOTE_STATUS
            | methods::GET_LAN_SETTINGS
            | methods::SET_LAN_SETTINGS
            | methods::BEGIN_PAIRING
            | methods::WATCH_TRUSTED_CLIENTS
            | methods::REVOKE_TRUSTED_CLIENT
    )
}

async fn reconcile(
    supervisors: &mut HashMap<ServerId, Supervisor>,
    local_id: &ServerId,
    entries: Vec<RemoteEntry>,
    selected: &mut Option<ServerRef>,
    context: &RemoteContext,
) {
    let wanted: HashSet<_> = entries
        .iter()
        .map(|entry| entry.server_id.clone())
        .collect();
    let removed: Vec<_> = supervisors
        .keys()
        .filter(|id| *id != local_id && !wanted.contains(*id))
        .cloned()
        .collect();
    for id in removed {
        if selected.as_ref().is_some_and(|chat| chat.server_id == id)
            && let Some(chat) = selected.take()
        {
            let _ = context.events.send(FederationEvent::Transcript {
                chat,
                entries: Vec::new(),
            });
        }
        if let Some(supervisor) = supervisors.remove(&id) {
            stop_supervisor(supervisor).await;
        }
        let _ = context.events.send(FederationEvent::ServerRemoved(id));
    }
    for entry in entries {
        let id = entry.server_id.clone();
        if &id == local_id {
            let _ = context.events.send(FederationEvent::Notice {
                server_id: id,
                message: "remote registry entry collides with the local server identity".into(),
            });
            continue;
        }
        if supervisors.get(&id).is_some_and(|current| {
            current
                .entry
                .as_ref()
                .is_some_and(|current| same_connection(current, &entry))
        }) {
            continue;
        }
        if let Some(old) = supervisors.remove(&id) {
            if let Some(chat) = selected.as_ref().filter(|chat| chat.server_id == id) {
                let _ = context.events.send(FederationEvent::Transcript {
                    chat: chat.clone(),
                    entries: Vec::new(),
                });
            }
            stop_supervisor(old).await;
        } else {
            let _ = context
                .events
                .send(FederationEvent::ServerChanged(ServerState::offline(
                    id.clone(),
                    entry.name.clone(),
                )));
        }
        let (tx, rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(supervise_remote(
            entry.clone(),
            context.local.clone(),
            context.tls.clone(),
            context.connector.clone(),
            context.events.clone(),
            rx,
        ));
        if let Some(chat) = selected.as_ref().filter(|chat| chat.server_id == id) {
            let _ = tx.send(SupervisorCommand::WatchTranscript(Some(
                chat.local_id.clone(),
            )));
        }
        supervisors.insert(
            id,
            Supervisor {
                entry: Some(entry),
                commands: tx,
                task,
            },
        );
    }
}

fn same_connection(left: &RemoteEntry, right: &RemoteEntry) -> bool {
    left.server_id == right.server_id
        && left.endpoint == right.endpoint
        && left.name == right.name
        && left.pinned_spki_sha256 == right.pinned_spki_sha256
}

async fn supervise_remote(
    entry: RemoteEntry,
    local: Arc<RpcClient>,
    tls: Arc<TlsIdentity>,
    connector: Arc<dyn RemoteConnector>,
    events: mpsc::UnboundedSender<FederationEvent>,
    mut commands: mpsc::UnboundedReceiver<SupervisorCommand>,
) {
    let mut delay = Duration::from_millis(100);
    let mut selected_chat = None;
    loop {
        let _ = events.send(FederationEvent::ServerChanged(ServerState::empty(
            entry.server_id.clone(),
            entry.name.clone(),
            RemoteConnectionState::Connecting,
        )));
        let client = match connector.connect(&entry, &tls).await {
            Ok(client) => client,
            Err(RemoteConnectError::IdentityChanged) => {
                publish_failure(
                    &entry,
                    RemoteConnectionState::IdentityChanged,
                    &local,
                    &events,
                )
                .await;
                if !wait_terminal(&mut commands, &mut selected_chat, &entry.server_id, &events)
                    .await
                {
                    return;
                }
                continue;
            }
            Err(RemoteConnectError::InvalidConfiguration(message)) => {
                let _ = events.send(FederationEvent::Notice {
                    server_id: entry.server_id.clone(),
                    message,
                });
                publish_failure(
                    &entry,
                    RemoteConnectionState::IdentityChanged,
                    &local,
                    &events,
                )
                .await;
                if !wait_terminal(&mut commands, &mut selected_chat, &entry.server_id, &events)
                    .await
                {
                    return;
                }
                continue;
            }
            Err(RemoteConnectError::Transport(message)) => {
                publish_failure(
                    &entry,
                    RemoteConnectionState::Unreachable { message },
                    &local,
                    &events,
                )
                .await;
                if !wait_transient(
                    delay,
                    &mut commands,
                    &mut selected_chat,
                    &entry.server_id,
                    &events,
                )
                .await
                {
                    return;
                }
                delay = (delay * 2).min(Duration::from_secs(5));
                continue;
            }
        };
        let hello: ServerHello = match client
            .call_as(methods::SERVER_HELLO, serde_json::Value::Null)
            .await
        {
            Ok(hello) => hello,
            Err(error) => {
                publish_failure(
                    &entry,
                    RemoteConnectionState::Unreachable {
                        message: error.to_string(),
                    },
                    &local,
                    &events,
                )
                .await;
                if !wait_transient(
                    delay,
                    &mut commands,
                    &mut selected_chat,
                    &entry.server_id,
                    &events,
                )
                .await
                {
                    return;
                }
                delay = (delay * 2).min(Duration::from_secs(5));
                continue;
            }
        };
        if hello.server_id != entry.server_id {
            publish_failure(
                &entry,
                RemoteConnectionState::IdentityChanged,
                &local,
                &events,
            )
            .await;
            if !wait_terminal(&mut commands, &mut selected_chat, &entry.server_id, &events).await {
                return;
            }
            continue;
        }
        if hello.protocol_version != PROTOCOL_VERSION {
            let state = RemoteConnectionState::IncompatibleVersion {
                remote: hello.protocol_version,
            };
            let _ = events.send(FederationEvent::ServerChanged(ServerState::empty(
                entry.server_id.clone(),
                entry.name.clone(),
                state.clone(),
            )));
            report(&entry, state, hello.protocol_version, &local).await;
            if !wait_terminal(&mut commands, &mut selected_chat, &entry.server_id, &events).await {
                return;
            }
            continue;
        }
        report(
            &entry,
            RemoteConnectionState::Online,
            hello.protocol_version,
            &local,
        )
        .await;
        delay = Duration::from_millis(100);
        let connected = supervise_connected(
            Arc::new(client),
            hello,
            entry.name.clone(),
            events.clone(),
            &mut commands,
            &mut selected_chat,
        )
        .await;
        match connected {
            Ok(ConnectedExit::Shutdown) => return,
            Ok(ConnectedExit::Reconnect) => {
                clear_selected_transcript(&entry.server_id, &selected_chat, &events);
                publish_failure(&entry, RemoteConnectionState::Offline, &local, &events).await;
                continue;
            }
            Err(_) => {
                clear_selected_transcript(&entry.server_id, &selected_chat, &events);
                publish_failure(&entry, RemoteConnectionState::Offline, &local, &events).await;
                if !wait_transient(
                    delay,
                    &mut commands,
                    &mut selected_chat,
                    &entry.server_id,
                    &events,
                )
                .await
                {
                    return;
                }
                delay = (delay * 2).min(Duration::from_secs(5));
            }
        }
    }
}

fn jittered_backoff(base: Duration, entropy: u32) -> Duration {
    let percentage = 80 + (u64::from(entropy) * 40 / u64::from(u32::MAX));
    Duration::from_millis((base.as_millis() as u64 * percentage / 100).max(1))
        .min(Duration::from_secs(5))
}

async fn wait_terminal(
    commands: &mut mpsc::UnboundedReceiver<SupervisorCommand>,
    selected_chat: &mut Option<String>,
    server_id: &ServerId,
    events: &mpsc::UnboundedSender<FederationEvent>,
) -> bool {
    loop {
        match commands.recv().await {
            Some(SupervisorCommand::Reconnect) => return true,
            Some(SupervisorCommand::Shutdown) | None => return false,
            Some(command) => handle_offline_command(command, selected_chat, server_id, events),
        }
    }
}

async fn wait_transient(
    base: Duration,
    commands: &mut mpsc::UnboundedReceiver<SupervisorCommand>,
    selected_chat: &mut Option<String>,
    server_id: &ServerId,
    events: &mpsc::UnboundedSender<FederationEvent>,
) -> bool {
    let entropy = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.subsec_nanos())
        .unwrap_or(0);
    let sleep = tokio::time::sleep(jittered_backoff(base, entropy));
    tokio::pin!(sleep);
    loop {
        tokio::select! {
            () = &mut sleep => return true,
            command = commands.recv() => match command {
                Some(SupervisorCommand::Reconnect) => return true,
                Some(SupervisorCommand::Shutdown) | None => return false,
                Some(command) => handle_offline_command(command, selected_chat, server_id, events),
            }
        }
    }
}

fn handle_offline_command(
    command: SupervisorCommand,
    selected_chat: &mut Option<String>,
    server_id: &ServerId,
    events: &mpsc::UnboundedSender<FederationEvent>,
) {
    match command {
        SupervisorCommand::WatchTranscript(chat_id) => *selected_chat = chat_id,
        SupervisorCommand::Call(method, _) => {
            let _ = events.send(FederationEvent::Notice {
                server_id: server_id.clone(),
                message: format!("cannot call {method} while server is offline"),
            });
        }
        SupervisorCommand::Reconnect | SupervisorCommand::Shutdown => {}
    }
}

async fn publish_failure(
    entry: &RemoteEntry,
    state: RemoteConnectionState,
    local: &RpcClient,
    events: &mpsc::UnboundedSender<FederationEvent>,
) {
    let _ = events.send(FederationEvent::ServerChanged(ServerState::empty(
        entry.server_id.clone(),
        entry.name.clone(),
        state.clone(),
    )));
    report(entry, state, entry.protocol_version, local).await;
}

async fn report(
    entry: &RemoteEntry,
    state: RemoteConnectionState,
    protocol_version: u32,
    local: &RpcClient,
) {
    let connected = matches!(state, RemoteConnectionState::Online).then(chrono::Utc::now);
    let _ = local
        .call(
            methods::REPORT_REMOTE_STATUS,
            serde_json::json!({
                "serverId": entry.server_id,
                "lastState": state,
                "protocolVersion": protocol_version,
                "lastConnectedAt": connected,
            }),
        )
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use comet_proto::RemoteEndpoint;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn entry(pin: &str, state: RemoteConnectionState) -> RemoteEntry {
        RemoteEntry {
            server_id: ServerId::new(format!("sha256:{pin}")),
            endpoint: RemoteEndpoint {
                host: "build.local".into(),
                port: 27655,
            },
            name: "Build".into(),
            pinned_spki_sha256: pin.into(),
            protocol_version: 1,
            last_state: state,
            created_at: chrono::Utc::now(),
            last_connected_at: None,
        }
    }

    #[test]
    fn status_report_does_not_replace_a_direct_connection_supervisor() {
        let pin = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let offline = entry(pin, RemoteConnectionState::Offline);
        let mut online = offline.clone();
        online.last_state = RemoteConnectionState::Online;
        online.last_connected_at = Some(chrono::Utc::now());

        assert!(same_connection(&offline, &online));
    }

    #[test]
    fn pin_change_replaces_the_direct_connection_supervisor() {
        let old = entry(
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            RemoteConnectionState::Offline,
        );
        let mut changed = old.clone();
        changed.pinned_spki_sha256 =
            "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc".into();

        assert!(!same_connection(&old, &changed));
    }

    #[test]
    fn every_local_administration_method_is_blocked_from_generic_remote_calls() {
        for method in [
            methods::WATCH_REMOTES,
            methods::PUT_REMOTE,
            methods::REMOVE_REMOTE,
            methods::REPORT_REMOTE_STATUS,
            methods::GET_LAN_SETTINGS,
            methods::SET_LAN_SETTINGS,
            methods::BEGIN_PAIRING,
            methods::WATCH_TRUSTED_CLIENTS,
            methods::REVOKE_TRUSTED_CLIENT,
        ] {
            assert!(is_local_admin(method), "{method}");
        }
        assert!(!is_local_admin(methods::SERVER_HELLO));
        assert!(!is_local_admin(methods::WATCH_CHATS));
    }

    #[test]
    fn reconnect_backoff_jitter_stays_within_twenty_percent() {
        assert_eq!(
            jittered_backoff(Duration::from_millis(100), 0),
            Duration::from_millis(80)
        );
        assert_eq!(
            jittered_backoff(Duration::from_millis(100), u32::MAX),
            Duration::from_millis(120)
        );
    }

    #[tokio::test]
    async fn malformed_persisted_pin_is_a_terminal_configuration_error() {
        let pin = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let mut remote = entry(pin, RemoteConnectionState::Offline);
        remote.pinned_spki_sha256 = "NOT-CANONICAL".into();
        let directory = tempfile::tempdir().unwrap();
        let identity = DeviceIdentity::load_or_create(directory.path()).unwrap();
        let tls = TlsIdentity::from_device_identity(&identity).unwrap();

        assert!(matches!(
            LanConnector.connect(&remote, &tls).await,
            Err(RemoteConnectError::InvalidConfiguration(_))
        ));
    }

    #[tokio::test]
    async fn stopping_a_supervisor_joins_and_drops_its_owned_connection_task() {
        struct Dropped(Arc<AtomicBool>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let dropped = Arc::new(AtomicBool::new(false));
        let guard = Dropped(dropped.clone());
        let task = tokio::spawn(async move {
            let _guard = guard;
            futures::future::pending::<()>().await;
        });
        let (commands, _receiver) = mpsc::unbounded_channel();
        stop_supervisor(Supervisor {
            entry: None,
            commands,
            task,
        })
        .await;

        assert!(dropped.load(Ordering::SeqCst));
    }
}
