use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use comet_doc::SessionMessageEntry;
use comet_identity::DeviceIdentity;
use comet_proto::{PROTOCOL_VERSION, RemoteConnectionState, RemoteEntry, ServerHello, ServerId};
use comet_rpc::{LanConnectError, PinnedServer, RpcClient, TlsIdentity, connect_lan_rpc, methods};
use futures::future::BoxFuture;
use tokio::sync::{mpsc, watch};

use crate::server::{SupervisorCommand, supervise_connected};
use crate::{FederationCommand, FederationEvent, ServerState};

#[derive(Debug, thiserror::Error)]
pub enum RemoteConnectError {
    #[error("identity changed")]
    IdentityChanged,
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
                    .map_err(|error| RemoteConnectError::Transport(error.to_string()))?;
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
    task: tokio::task::JoinHandle<()>,
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
            task,
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
        self.task.abort();
    }
}

struct Supervisor {
    entry: Option<RemoteEntry>,
    commands: mpsc::UnboundedSender<SupervisorCommand>,
    task: tokio::task::JoinHandle<()>,
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
        local_id.clone(),
        local_name,
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

    loop {
        tokio::select! {
            snapshot = registry.recv() => match snapshot {
                Some(value) => match serde_json::from_value::<Vec<RemoteEntry>>(value) {
                    Ok(entries) => reconcile(&mut supervisors, &local_id, entries, local.clone(), tls.clone(), connector.clone(), events.clone()),
                    Err(error) => { let _ = events.send(FederationEvent::Notice { server_id: local_id.clone(), message: format!("invalid remote registry: {error}") }); }
                },
                None => break,
            },
            command = commands.recv() => match command {
                Some(FederationCommand::Call { server_id, method, params }) => {
                    if let Some(supervisor) = supervisors.get(&server_id) { let _ = supervisor.commands.send(SupervisorCommand::Call(method, params)); }
                }
                Some(FederationCommand::WatchTranscript(chat)) => {
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
        let _ = supervisor.commands.send(SupervisorCommand::Shutdown);
        supervisor.task.abort();
    }
}

fn spawn_local_resources(
    local: Arc<RpcClient>,
    id: ServerId,
    name: String,
    events: mpsc::UnboundedSender<FederationEvent>,
    mut commands: mpsc::UnboundedReceiver<SupervisorCommand>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut state = ServerState::empty(id.clone(), name, RemoteConnectionState::Online);
        let (transcript_tx, transcript_rx) = watch::channel::<Option<String>>(None);
        let Ok(mut devices) = local
            .subscribe(methods::WATCH_DEVICES, serde_json::Value::Null)
            .await
        else {
            return;
        };
        let Ok(mut spaces) = local
            .subscribe(methods::WATCH_SPACES, serde_json::Value::Null)
            .await
        else {
            return;
        };
        let Ok(mut chats) = local
            .subscribe(methods::WATCH_CHATS, serde_json::Value::Null)
            .await
        else {
            return;
        };
        let Ok(mut sessions) = local
            .subscribe(methods::WATCH_SESSIONS, serde_json::Value::Null)
            .await
        else {
            return;
        };
        let _ = events.send(FederationEvent::ServerChanged(state.clone()));
        loop {
            tokio::select! {
                value = devices.recv() => match value.and_then(|v| serde_json::from_value(v).ok()) { Some(v) => state.devices = v, None => break },
                value = spaces.recv() => match value.and_then(|v| serde_json::from_value(v).ok()) { Some(v) => state.spaces = v, None => break },
                value = chats.recv() => match value.and_then(|v| serde_json::from_value(v).ok()) { Some(v) => state.chats = v, None => break },
                value = sessions.recv() => match value.and_then(|v| serde_json::from_value(v).ok()) { Some(v) => state.sessions = v, None => break },
                command = commands.recv() => match command {
                    Some(SupervisorCommand::Call(method, params)) => {
                        if let Err(error) = local.call(method, params).await {
                            let _ = events.send(FederationEvent::Notice { server_id: id.clone(), message: error.to_string() });
                        }
                        continue;
                    }
                    Some(SupervisorCommand::WatchTranscript(chat_id)) => {
                        transcript_tx.send_replace(chat_id.clone());
                        if let Some(chat_id) = chat_id {
                            let local = local.clone();
                            let events = events.clone();
                            let server_id = id.clone();
                            let mut selection = transcript_rx.clone();
                            tokio::spawn(async move {
                                let params = serde_json::json!({"chatId": chat_id});
                                let Ok(mut stream) = local.subscribe(methods::WATCH_DOC_MESSAGES, params).await else { return; };
                                loop {
                                    tokio::select! {
                                        value = stream.recv() => match value {
                                            Some(value) => match serde_json::from_value::<Vec<SessionMessageEntry>>(value) {
                                                Ok(entries) => {
                                                    let _ = events.send(FederationEvent::Transcript {
                                                        chat: comet_proto::ServerRef::new(server_id.clone(), chat_id.clone()),
                                                        entries,
                                                    });
                                                }
                                                Err(_) => return,
                                            },
                                            None => return,
                                        },
                                        changed = selection.changed() => {
                                            if changed.is_err() || selection.borrow().as_ref() != Some(&chat_id) { return; }
                                        }
                                    }
                                }
                            });
                        }
                        continue;
                    }
                    Some(SupervisorCommand::Reconnect) => continue,
                    Some(SupervisorCommand::Shutdown) | None => break,
                },
            }
            let _ = events.send(FederationEvent::ServerChanged(state.clone()));
        }
    })
}

fn reconcile(
    supervisors: &mut HashMap<ServerId, Supervisor>,
    local_id: &ServerId,
    entries: Vec<RemoteEntry>,
    local: Arc<RpcClient>,
    tls: Arc<TlsIdentity>,
    connector: Arc<dyn RemoteConnector>,
    events: mpsc::UnboundedSender<FederationEvent>,
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
        if let Some(supervisor) = supervisors.remove(&id) {
            supervisor.task.abort();
        }
        let _ = events.send(FederationEvent::ServerRemoved(id));
    }
    for entry in entries {
        let id = entry.server_id.clone();
        if supervisors.get(&id).is_some_and(|current| {
            current
                .entry
                .as_ref()
                .is_some_and(|current| same_connection(current, &entry))
        }) {
            continue;
        }
        if let Some(old) = supervisors.remove(&id) {
            old.task.abort();
        } else {
            let _ = events.send(FederationEvent::ServerChanged(ServerState::offline(
                id.clone(),
                entry.name.clone(),
            )));
        }
        let (tx, rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(supervise_remote(
            entry.clone(),
            local.clone(),
            tls.clone(),
            connector.clone(),
            events.clone(),
            rx,
        ));
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
                if !wait_reconnect(&mut commands).await {
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
                let entropy = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|duration| duration.subsec_nanos())
                    .unwrap_or(0);
                tokio::select! { _ = tokio::time::sleep(jittered_backoff(delay, entropy)) => {}, command = commands.recv() => if !matches!(command, Some(SupervisorCommand::Reconnect)) { return; } }
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
            if !wait_reconnect(&mut commands).await {
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
            if !wait_reconnect(&mut commands).await {
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
        if supervise_connected(
            client,
            hello,
            entry.name.clone(),
            events.clone(),
            &mut commands,
        )
        .await
        .is_ok()
        {
            return;
        }
        publish_failure(&entry, RemoteConnectionState::Offline, &local, &events).await;
    }
}

fn jittered_backoff(base: Duration, entropy: u32) -> Duration {
    let percentage = 80 + (u64::from(entropy) * 40 / u64::from(u32::MAX));
    Duration::from_millis((base.as_millis() as u64 * percentage / 100).max(1))
        .min(Duration::from_secs(5))
}

async fn wait_reconnect(commands: &mut mpsc::UnboundedReceiver<SupervisorCommand>) -> bool {
    loop {
        match commands.recv().await {
            Some(SupervisorCommand::Reconnect) => return true,
            Some(SupervisorCommand::Shutdown) | None => return false,
            _ => {}
        }
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
}
