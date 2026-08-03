use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use comet_identity::DeviceIdentity;
use comet_proto::{
    PROTOCOL_VERSION, RemoteConnectionState, RemoteEntry, ServerHello, ServerId, ServerRef,
};
use comet_rpc::{
    LanConnectError, PinnedServer, RpcClient, RpcStream, TlsIdentity, connect_lan_rpc, methods,
};
use futures::future::BoxFuture;
use tokio::sync::mpsc;

use crate::server::{
    ConnectedExit, SupervisorCommand, clear_selected_transcript, supervise_connected,
};
use crate::{FederationCommand, FederationEvent, FederationStream, ServerState};

const INITIAL_RETRY_DELAY: Duration = Duration::from_millis(100);
const STABLE_CONNECTION_DURATION: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RetryWake {
    Timer,
    ExplicitReconnect,
    Shutdown,
}

enum PhaseResult<T> {
    Ready(T),
    Reconnect,
    Shutdown,
}

async fn await_supervisor_phase<T>(
    operation: impl std::future::Future<Output = T>,
    commands: &mut mpsc::UnboundedReceiver<SupervisorCommand>,
    selected_chat: &mut Option<String>,
    server_id: &ServerId,
    events: &mpsc::UnboundedSender<FederationEvent>,
) -> PhaseResult<T> {
    tokio::pin!(operation);
    loop {
        tokio::select! {
            value = &mut operation => return PhaseResult::Ready(value),
            command = commands.recv() => match command {
                Some(SupervisorCommand::Reconnect) => return PhaseResult::Reconnect,
                Some(SupervisorCommand::Shutdown) | None => return PhaseResult::Shutdown,
                Some(command) => handle_offline_command(command, selected_chat, server_id, events),
            }
        }
    }
}

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

    pub async fn new_shared(
        local: Arc<RpcClient>,
        data_dir: impl AsRef<Path>,
    ) -> Result<Self, FederationError> {
        Self::with_connector_shared(local, data_dir, Arc::new(LanConnector)).await
    }

    pub async fn with_connector(
        local: RpcClient,
        data_dir: impl AsRef<Path>,
        connector: Arc<dyn RemoteConnector>,
    ) -> Result<Self, FederationError> {
        Self::with_connector_shared(Arc::new(local), data_dir, connector).await
    }

    async fn with_connector_shared(
        local: Arc<RpcClient>,
        data_dir: impl AsRef<Path>,
        connector: Arc<dyn RemoteConnector>,
    ) -> Result<Self, FederationError> {
        let identity = DeviceIdentity::load_or_create(data_dir.as_ref())?;
        let tls = Arc::new(TlsIdentity::from_device_identity(&identity)?);
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

    pub async fn request(
        &self,
        server_id: ServerId,
        method: &'static str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, comet_rpc::RpcError> {
        let (reply, received) = tokio::sync::oneshot::channel();
        self.send(FederationCommand::Request {
            server_id,
            method,
            params,
            reply,
        })
        .map_err(|_| comet_rpc::RpcError::Closed)?;
        received.await.unwrap_or(Err(comet_rpc::RpcError::Closed))
    }

    pub async fn subscribe(
        &self,
        server_id: ServerId,
        method: &'static str,
        params: serde_json::Value,
    ) -> Result<FederationStream, comet_rpc::RpcError> {
        let (reply, received) = tokio::sync::oneshot::channel();
        self.send(FederationCommand::Subscribe {
            server_id,
            method,
            params,
            reply,
        })
        .map_err(|_| comet_rpc::RpcError::Closed)?;
        received.await.unwrap_or(Err(comet_rpc::RpcError::Closed))
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
    local_hello: ServerHello,
    tls: Arc<TlsIdentity>,
    connector: Arc<dyn RemoteConnector>,
    events: mpsc::UnboundedSender<FederationEvent>,
}

async fn run_manager(
    local: Arc<RpcClient>,
    local_hello: ServerHello,
    mut registry: RpcStream,
    tls: Arc<TlsIdentity>,
    connector: Arc<dyn RemoteConnector>,
    events: mpsc::UnboundedSender<FederationEvent>,
    mut commands: mpsc::UnboundedReceiver<FederationCommand>,
) {
    let mut supervisors = HashMap::<ServerId, Supervisor>::new();
    let (local_tx, local_rx) = mpsc::unbounded_channel();
    let local_id = local_hello.server_id.clone();
    let local_name = local_hello.name.clone();
    let local_hello_for_restart = local_hello.clone();
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
        local_hello: local_hello_for_restart,
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
                    let selected = selected.take();
                    let owned = std::mem::take(&mut supervisors);
                    for (id, supervisor) in owned {
                        stop_supervisor(supervisor).await;
                        if id != local_id { let _ = events.send(FederationEvent::ServerRemoved(id)); }
                    }
                    if let Some(chat) = selected {
                        let _ = events.send(FederationEvent::Transcript { chat, entries: Vec::new() });
                    }
                    let _ = events.send(FederationEvent::ServerChanged(ServerState::offline(local_id.clone(), local_name.clone())));
                    return;
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
                Some(FederationCommand::Request { server_id, method, params, reply }) => {
                    if server_id != local_id && is_local_admin(method) {
                        let _ = reply.send(Err(comet_rpc::RpcError::UnknownMethod(method.into())));
                    } else if let Some(supervisor) = supervisors.get(&server_id) {
                        if let Err(error) = supervisor.commands.send(SupervisorCommand::Request { method, params, reply })
                            && let SupervisorCommand::Request { reply, .. } = error.0
                        {
                            let _ = reply.send(Err(comet_rpc::RpcError::Closed));
                        }
                    } else {
                        let _ = reply.send(Err(comet_rpc::RpcError::Failed("server is offline".into())));
                    }
                }
                Some(FederationCommand::Subscribe { server_id, method, params, reply }) => {
                    if server_id != local_id && is_local_admin(method) {
                        let _ = reply.send(Err(comet_rpc::RpcError::UnknownMethod(method.into())));
                    } else if let Some(supervisor) = supervisors.get(&server_id) {
                        if let Err(error) = supervisor.commands.send(SupervisorCommand::Subscribe { method, params, reply })
                            && let SupervisorCommand::Subscribe { reply, .. } = error.0
                        {
                            let _ = reply.send(Err(comet_rpc::RpcError::Closed));
                        }
                    } else {
                        let _ = reply.send(Err(comet_rpc::RpcError::Failed("server is offline".into())));
                    }
                }
                Some(FederationCommand::WatchTranscript(chat)) => {
                    if let Some(previous) = selected.take() {
                        if let Some(supervisor) = supervisors.get(&previous.server_id) {
                            if !set_supervisor_transcript(supervisor, None).await {
                                restart_unresponsive_owner(
                                    &mut supervisors,
                                    &previous,
                                    &remote_context,
                                )
                                .await;
                            }
                        } else {
                            let _ = events.send(FederationEvent::Transcript { chat: previous, entries: Vec::new() });
                        }
                    }
                    selected = chat;
                    if let Some(chat) = selected.as_ref() && let Some(supervisor) = supervisors.get(&chat.server_id) {
                        queue_supervisor_transcript(supervisor, Some(chat.local_id.clone()));
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
    if let Some(chat) = selected {
        let _ = events.send(FederationEvent::Transcript {
            chat,
            entries: Vec::new(),
        });
    }
}

async fn stop_supervisor(supervisor: Supervisor) {
    let _ = supervisor.commands.send(SupervisorCommand::Shutdown);
    supervisor.task.abort();
    let _ = supervisor.task.await;
}

async fn stop_supervisor_then_clear(
    supervisor: Supervisor,
    chat: Option<ServerRef>,
    events: &mpsc::UnboundedSender<FederationEvent>,
) {
    stop_supervisor(supervisor).await;
    if let Some(chat) = chat {
        let _ = events.send(FederationEvent::Transcript {
            chat,
            entries: Vec::new(),
        });
    }
}

async fn set_supervisor_transcript(supervisor: &Supervisor, chat_id: Option<String>) -> bool {
    let (acknowledged, received) = tokio::sync::oneshot::channel();
    if supervisor
        .commands
        .send(SupervisorCommand::WatchTranscript {
            chat_id,
            acknowledged: Some(acknowledged),
        })
        .is_err()
    {
        return false;
    }
    received.await.is_ok()
}

fn queue_supervisor_transcript(supervisor: &Supervisor, chat_id: Option<String>) -> bool {
    supervisor
        .commands
        .send(SupervisorCommand::WatchTranscript {
            chat_id,
            acknowledged: None,
        })
        .is_ok()
}

async fn restart_unresponsive_owner(
    supervisors: &mut HashMap<ServerId, Supervisor>,
    previous: &ServerRef,
    context: &RemoteContext,
) {
    let Some(old) = supervisors.remove(&previous.server_id) else {
        let _ = context.events.send(FederationEvent::Transcript {
            chat: previous.clone(),
            entries: Vec::new(),
        });
        return;
    };
    let entry = old.entry.clone();
    stop_supervisor_then_clear(old, Some(previous.clone()), &context.events).await;
    let replacement = spawn_owned_supervisor(entry, context);
    supervisors.insert(previous.server_id.clone(), replacement);
}

fn spawn_owned_supervisor(entry: Option<RemoteEntry>, context: &RemoteContext) -> Supervisor {
    let (commands, receiver) = mpsc::unbounded_channel();
    let task = match entry.clone() {
        Some(remote) => tokio::spawn(supervise_remote(
            remote,
            context.local.clone(),
            context.tls.clone(),
            context.connector.clone(),
            context.events.clone(),
            receiver,
        )),
        None => spawn_local_resources(
            context.local.clone(),
            context.local_hello.clone(),
            context.local_hello.name.clone(),
            context.events.clone(),
            receiver,
        ),
    };
    Supervisor {
        entry,
        commands,
        task,
    }
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
        let chat = selected
            .as_ref()
            .is_some_and(|chat| chat.server_id == id)
            .then(|| selected.take())
            .flatten();
        if let Some(supervisor) = supervisors.remove(&id) {
            stop_supervisor_then_clear(supervisor, chat, &context.events).await;
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
            let chat = selected
                .as_ref()
                .filter(|chat| chat.server_id == id)
                .cloned();
            stop_supervisor_then_clear(old, chat, &context.events).await;
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
        supervisors.insert(
            id.clone(),
            Supervisor {
                entry: Some(entry),
                commands: tx,
                task,
            },
        );
        if let Some(chat) = selected.as_ref().filter(|chat| chat.server_id == id)
            && let Some(supervisor) = supervisors.get(&id)
        {
            queue_supervisor_transcript(supervisor, Some(chat.local_id.clone()));
        }
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
    let mut delay = INITIAL_RETRY_DELAY;
    let mut selected_chat = None;
    loop {
        macro_rules! phase {
            ($operation:expr) => {
                match await_supervisor_phase(
                    $operation,
                    &mut commands,
                    &mut selected_chat,
                    &entry.server_id,
                    &events,
                )
                .await
                {
                    PhaseResult::Ready(value) => value,
                    PhaseResult::Reconnect => {
                        delay = INITIAL_RETRY_DELAY;
                        continue;
                    }
                    PhaseResult::Shutdown => return,
                }
            };
        }
        let _ = events.send(FederationEvent::ServerChanged(ServerState::empty(
            entry.server_id.clone(),
            entry.name.clone(),
            RemoteConnectionState::Connecting,
        )));
        let client = match phase!(connector.connect(&entry, &tls)) {
            Ok(client) => client,
            Err(RemoteConnectError::IdentityChanged) => {
                phase!(publish_failure(
                    &entry,
                    RemoteConnectionState::IdentityChanged,
                    &local,
                    &events,
                ));
                let wake =
                    wait_terminal(&mut commands, &mut selected_chat, &entry.server_id, &events)
                        .await;
                if wake == RetryWake::Shutdown {
                    return;
                }
                delay = retry_delay_after_wake(delay, wake);
                continue;
            }
            Err(RemoteConnectError::InvalidConfiguration(message)) => {
                let _ = events.send(FederationEvent::Notice {
                    server_id: entry.server_id.clone(),
                    message,
                });
                phase!(publish_failure(
                    &entry,
                    RemoteConnectionState::IdentityChanged,
                    &local,
                    &events,
                ));
                let wake =
                    wait_terminal(&mut commands, &mut selected_chat, &entry.server_id, &events)
                        .await;
                if wake == RetryWake::Shutdown {
                    return;
                }
                delay = retry_delay_after_wake(delay, wake);
                continue;
            }
            Err(RemoteConnectError::Transport(message)) => {
                phase!(publish_failure(
                    &entry,
                    RemoteConnectionState::Unreachable { message },
                    &local,
                    &events,
                ));
                let wake = wait_transient(
                    delay,
                    &mut commands,
                    &mut selected_chat,
                    &entry.server_id,
                    &events,
                )
                .await;
                if wake == RetryWake::Shutdown {
                    return;
                }
                delay = retry_delay_after_wake(delay, wake);
                continue;
            }
        };
        let hello: ServerHello =
            match phase!(client.call_as(methods::SERVER_HELLO, serde_json::Value::Null,)) {
                Ok(hello) => hello,
                Err(error) => {
                    phase!(publish_failure(
                        &entry,
                        RemoteConnectionState::Unreachable {
                            message: error.to_string(),
                        },
                        &local,
                        &events,
                    ));
                    let wake = wait_transient(
                        delay,
                        &mut commands,
                        &mut selected_chat,
                        &entry.server_id,
                        &events,
                    )
                    .await;
                    if wake == RetryWake::Shutdown {
                        return;
                    }
                    delay = retry_delay_after_wake(delay, wake);
                    continue;
                }
            };
        if hello.server_id != entry.server_id {
            phase!(publish_failure(
                &entry,
                RemoteConnectionState::IdentityChanged,
                &local,
                &events,
            ));
            let wake =
                wait_terminal(&mut commands, &mut selected_chat, &entry.server_id, &events).await;
            if wake == RetryWake::Shutdown {
                return;
            }
            delay = retry_delay_after_wake(delay, wake);
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
            phase!(report(&entry, state, hello.protocol_version, &local));
            let wake =
                wait_terminal(&mut commands, &mut selected_chat, &entry.server_id, &events).await;
            if wake == RetryWake::Shutdown {
                return;
            }
            delay = retry_delay_after_wake(delay, wake);
            continue;
        }
        phase!(report(
            &entry,
            RemoteConnectionState::Online,
            hello.protocol_version,
            &local,
        ));
        let session_started = std::time::Instant::now();
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
                delay = INITIAL_RETRY_DELAY;
                clear_selected_transcript(&entry.server_id, &selected_chat, &events);
                phase!(publish_failure(
                    &entry,
                    RemoteConnectionState::Offline,
                    &local,
                    &events
                ));
                continue;
            }
            Err(_) => {
                delay = retry_delay_after_session(delay, session_started.elapsed());
                clear_selected_transcript(&entry.server_id, &selected_chat, &events);
                phase!(publish_failure(
                    &entry,
                    RemoteConnectionState::Offline,
                    &local,
                    &events
                ));
                let wake = wait_transient(
                    delay,
                    &mut commands,
                    &mut selected_chat,
                    &entry.server_id,
                    &events,
                )
                .await;
                if wake == RetryWake::Shutdown {
                    return;
                }
                delay = retry_delay_after_wake(delay, wake);
            }
        }
    }
}

fn jittered_backoff(base: Duration, entropy: u32) -> Duration {
    let percentage = 80 + (u64::from(entropy) * 40 / u64::from(u32::MAX));
    Duration::from_millis((base.as_millis() as u64 * percentage / 100).max(1))
        .min(Duration::from_secs(5))
}

fn retry_delay_after_session(current: Duration, connected_for: Duration) -> Duration {
    if connected_for >= STABLE_CONNECTION_DURATION {
        INITIAL_RETRY_DELAY
    } else {
        current
    }
}

fn retry_delay_after_wake(current: Duration, wake: RetryWake) -> Duration {
    match wake {
        RetryWake::Timer => (current * 2).min(Duration::from_secs(5)),
        RetryWake::ExplicitReconnect => INITIAL_RETRY_DELAY,
        RetryWake::Shutdown => current,
    }
}

async fn wait_terminal(
    commands: &mut mpsc::UnboundedReceiver<SupervisorCommand>,
    selected_chat: &mut Option<String>,
    server_id: &ServerId,
    events: &mpsc::UnboundedSender<FederationEvent>,
) -> RetryWake {
    loop {
        match commands.recv().await {
            Some(SupervisorCommand::Reconnect) => return RetryWake::ExplicitReconnect,
            Some(SupervisorCommand::Shutdown) | None => return RetryWake::Shutdown,
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
) -> RetryWake {
    let entropy = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.subsec_nanos())
        .unwrap_or(0);
    let sleep = tokio::time::sleep(jittered_backoff(base, entropy));
    tokio::pin!(sleep);
    loop {
        tokio::select! {
            () = &mut sleep => return RetryWake::Timer,
            command = commands.recv() => match command {
                Some(SupervisorCommand::Reconnect) => return RetryWake::ExplicitReconnect,
                Some(SupervisorCommand::Shutdown) | None => return RetryWake::Shutdown,
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
        SupervisorCommand::WatchTranscript {
            chat_id,
            acknowledged,
        } => {
            *selected_chat = chat_id;
            if let Some(acknowledged) = acknowledged {
                let _ = acknowledged.send(());
            }
        }
        SupervisorCommand::Call(method, _) => {
            let _ = events.send(FederationEvent::Notice {
                server_id: server_id.clone(),
                message: format!("cannot call {method} while server is offline"),
            });
        }
        SupervisorCommand::Request { method, reply, .. } => {
            let _ = reply.send(Err(comet_rpc::RpcError::Failed(format!(
                "cannot call {method} while server is offline"
            ))));
        }
        SupervisorCommand::Subscribe { method, reply, .. } => {
            let _ = reply.send(Err(comet_rpc::RpcError::Failed(format!(
                "cannot subscribe to {method} while server is offline"
            ))));
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
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    struct PhaseDrop(Arc<AtomicBool>);
    impl Drop for PhaseDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    struct PhaseService {
        hello: ServerHello,
        stalled_method: Option<&'static str>,
        started: Arc<tokio::sync::Notify>,
        dropped: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl comet_rpc::RpcService for PhaseService {
        async fn handle(
            &self,
            method: &str,
            _params: serde_json::Value,
        ) -> Result<comet_rpc::RpcReply, comet_rpc::RpcError> {
            if self.stalled_method == Some(method) {
                let _guard = PhaseDrop(self.dropped.clone());
                self.started.notify_waiters();
                return futures::future::pending().await;
            }
            match method {
                methods::SERVER_HELLO => comet_rpc::RpcReply::value(&self.hello),
                methods::REPORT_REMOTE_STATUS => {
                    comet_rpc::RpcReply::value(&serde_json::json!({"ok": true}))
                }
                other => Err(comet_rpc::RpcError::UnknownMethod(other.into())),
            }
        }
    }

    struct FirstClientConnector {
        first: std::sync::Mutex<Option<RpcClient>>,
        attempts: Arc<AtomicUsize>,
    }

    impl RemoteConnector for FirstClientConnector {
        fn connect<'a>(
            &'a self,
            _entry: &'a RemoteEntry,
            _identity: &'a TlsIdentity,
        ) -> BoxFuture<'a, Result<RpcClient, RemoteConnectError>> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            let first = self.first.lock().unwrap().take();
            Box::pin(async move {
                match first {
                    Some(client) => Ok(client),
                    None => futures::future::pending().await,
                }
            })
        }
    }

    fn phase_hello() -> ServerHello {
        ServerHello {
            protocol_version: PROTOCOL_VERSION,
            server_id: ServerId::new(
                "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            ),
            device_id: "device-b".into(),
            name: "Build".into(),
            capabilities: Vec::new(),
        }
    }

    fn phase_client(
        stalled_method: Option<&'static str>,
        started: Arc<tokio::sync::Notify>,
        dropped: Arc<AtomicBool>,
    ) -> RpcClient {
        comet_rpc::memory_client(Arc::new(PhaseService {
            hello: phase_hello(),
            stalled_method,
            started,
            dropped,
        }))
    }

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

    #[test]
    fn retry_delay_resets_only_after_stable_connection_window() {
        let accumulated = Duration::from_millis(800);
        assert_eq!(
            retry_delay_after_session(accumulated, STABLE_CONNECTION_DURATION / 2),
            accumulated
        );
        assert_eq!(
            retry_delay_after_session(accumulated, STABLE_CONNECTION_DURATION),
            INITIAL_RETRY_DELAY
        );
    }

    #[test]
    fn explicit_reconnect_resets_near_max_backoff_without_growth() {
        let near_max = Duration::from_secs(4);
        assert_eq!(
            retry_delay_after_wake(near_max, RetryWake::ExplicitReconnect),
            INITIAL_RETRY_DELAY
        );
        assert_eq!(
            retry_delay_after_wake(near_max, RetryWake::Timer),
            Duration::from_secs(5)
        );
    }

    #[tokio::test]
    async fn explicit_reconnect_wakes_transient_and_terminal_waits_promptly() {
        let server_id = ServerId::new(
            "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        );
        let (events, _received) = mpsc::unbounded_channel();

        let (transient_tx, mut transient_rx) = mpsc::unbounded_channel();
        transient_tx.send(SupervisorCommand::Reconnect).unwrap();
        let mut selected = None;
        let transient = tokio::time::timeout(
            Duration::from_millis(20),
            wait_transient(
                Duration::from_secs(5),
                &mut transient_rx,
                &mut selected,
                &server_id,
                &events,
            ),
        )
        .await
        .unwrap();
        assert_eq!(transient, RetryWake::ExplicitReconnect);

        let (terminal_tx, mut terminal_rx) = mpsc::unbounded_channel();
        terminal_tx.send(SupervisorCommand::Reconnect).unwrap();
        let terminal = tokio::time::timeout(
            Duration::from_millis(20),
            wait_terminal(&mut terminal_rx, &mut selected, &server_id, &events),
        )
        .await
        .unwrap();
        assert_eq!(terminal, RetryWake::ExplicitReconnect);
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

    #[tokio::test]
    async fn old_supervisor_is_joined_before_its_transcript_clear() {
        struct FinalEvent(mpsc::UnboundedSender<FederationEvent>, ServerId);
        impl Drop for FinalEvent {
            fn drop(&mut self) {
                let _ = self.0.send(FederationEvent::Notice {
                    server_id: self.1.clone(),
                    message: "old supervisor final event".into(),
                });
            }
        }

        let server_id = ServerId::new(
            "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        );
        let chat = ServerRef::new(server_id.clone(), "old-chat");
        let (events, mut received) = mpsc::unbounded_channel();
        let guard = FinalEvent(events.clone(), server_id);
        let task = tokio::spawn(async move {
            let _guard = guard;
            futures::future::pending::<()>().await;
        });
        let (commands, _receiver) = mpsc::unbounded_channel();

        stop_supervisor_then_clear(
            Supervisor {
                entry: None,
                commands,
                task,
            },
            Some(chat.clone()),
            &events,
        )
        .await;

        assert!(matches!(
            received.recv().await,
            Some(FederationEvent::Notice { .. })
        ));
        assert_eq!(
            received.recv().await,
            Some(FederationEvent::Transcript {
                chat,
                entries: Vec::new(),
            })
        );
        assert!(received.try_recv().is_err());
    }

    #[tokio::test]
    async fn reconnect_interrupts_an_in_flight_transport_connect() {
        struct DropFlag(Arc<AtomicBool>);
        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        struct PendingConnector {
            attempts: Arc<AtomicUsize>,
            started: Arc<tokio::sync::Notify>,
            first_dropped: Arc<AtomicBool>,
        }
        impl RemoteConnector for PendingConnector {
            fn connect<'a>(
                &'a self,
                _entry: &'a RemoteEntry,
                _identity: &'a TlsIdentity,
            ) -> BoxFuture<'a, Result<RpcClient, RemoteConnectError>> {
                let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
                self.started.notify_waiters();
                let dropped = (attempt == 0).then(|| DropFlag(self.first_dropped.clone()));
                Box::pin(async move {
                    let _dropped = dropped;
                    futures::future::pending().await
                })
            }
        }

        let attempts = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(tokio::sync::Notify::new());
        let first_dropped = Arc::new(AtomicBool::new(false));
        let connector = Arc::new(PendingConnector {
            attempts: attempts.clone(),
            started: started.clone(),
            first_dropped: first_dropped.clone(),
        });
        let directory = tempfile::tempdir().unwrap();
        let identity = DeviceIdentity::load_or_create(directory.path()).unwrap();
        let tls = Arc::new(TlsIdentity::from_device_identity(&identity).unwrap());
        struct UnusedService;
        #[async_trait::async_trait]
        impl comet_rpc::RpcService for UnusedService {
            async fn handle(
                &self,
                method: &str,
                _params: serde_json::Value,
            ) -> Result<comet_rpc::RpcReply, comet_rpc::RpcError> {
                Err(comet_rpc::RpcError::UnknownMethod(method.into()))
            }
        }
        let local = Arc::new(comet_rpc::memory_client(Arc::new(UnusedService)));
        let (events, _received) = mpsc::unbounded_channel();
        let (commands, receiver) = mpsc::unbounded_channel();
        let task = tokio::spawn(supervise_remote(
            entry(
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                RemoteConnectionState::Offline,
            ),
            local,
            tls,
            connector,
            events,
            receiver,
        ));
        started.notified().await;
        commands.send(SupervisorCommand::Reconnect).unwrap();

        let result = tokio::time::timeout(Duration::from_millis(100), async {
            while attempts.load(Ordering::SeqCst) < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await;
        task.abort();
        result.expect("reconnect did not interrupt the transport connect");
        assert!(first_dropped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn reconnect_interrupts_server_hello_and_cancels_server_work() {
        let started = Arc::new(tokio::sync::Notify::new());
        let dropped = Arc::new(AtomicBool::new(false));
        let attempts = Arc::new(AtomicUsize::new(0));
        let connector = Arc::new(FirstClientConnector {
            first: std::sync::Mutex::new(Some(phase_client(
                Some(methods::SERVER_HELLO),
                started.clone(),
                dropped.clone(),
            ))),
            attempts: attempts.clone(),
        });
        let directory = tempfile::tempdir().unwrap();
        let identity = DeviceIdentity::load_or_create(directory.path()).unwrap();
        let tls = Arc::new(TlsIdentity::from_device_identity(&identity).unwrap());
        let local = Arc::new(phase_client(
            None,
            Arc::new(tokio::sync::Notify::new()),
            Arc::new(AtomicBool::new(false)),
        ));
        let (events, _received) = mpsc::unbounded_channel();
        let (commands, receiver) = mpsc::unbounded_channel();
        let task = tokio::spawn(supervise_remote(
            entry(
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                RemoteConnectionState::Offline,
            ),
            local,
            tls,
            connector,
            events,
            receiver,
        ));
        started.notified().await;
        commands.send(SupervisorCommand::Reconnect).unwrap();
        let result = tokio::time::timeout(Duration::from_millis(100), async {
            while attempts.load(Ordering::SeqCst) < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await;
        task.abort();
        result.expect("reconnect did not interrupt SERVER_HELLO");
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn reconnect_interrupts_status_report_and_cancels_server_work() {
        let started = Arc::new(tokio::sync::Notify::new());
        let dropped = Arc::new(AtomicBool::new(false));
        let attempts = Arc::new(AtomicUsize::new(0));
        let connector = Arc::new(FirstClientConnector {
            first: std::sync::Mutex::new(Some(phase_client(
                None,
                Arc::new(tokio::sync::Notify::new()),
                Arc::new(AtomicBool::new(false)),
            ))),
            attempts: attempts.clone(),
        });
        let local = Arc::new(phase_client(
            Some(methods::REPORT_REMOTE_STATUS),
            started.clone(),
            dropped.clone(),
        ));
        let directory = tempfile::tempdir().unwrap();
        let identity = DeviceIdentity::load_or_create(directory.path()).unwrap();
        let tls = Arc::new(TlsIdentity::from_device_identity(&identity).unwrap());
        let (events, _received) = mpsc::unbounded_channel();
        let (commands, receiver) = mpsc::unbounded_channel();
        let task = tokio::spawn(supervise_remote(
            entry(
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                RemoteConnectionState::Offline,
            ),
            local,
            tls,
            connector,
            events,
            receiver,
        ));
        started.notified().await;
        commands.send(SupervisorCommand::Reconnect).unwrap();
        let result = tokio::time::timeout(Duration::from_millis(100), async {
            while attempts.load(Ordering::SeqCst) < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await;
        task.abort();
        result.expect("reconnect did not interrupt status reporting");
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn selection_handoff_waits_for_prior_owner_acknowledgement() {
        let server_id = ServerId::new(
            "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        );
        let (events, mut received) = mpsc::unbounded_channel();
        let (commands, mut command_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn({
            let server_id = server_id.clone();
            async move {
                if let Some(SupervisorCommand::WatchTranscript { acknowledged, .. }) =
                    command_rx.recv().await
                {
                    let _ = events.send(FederationEvent::Notice {
                        server_id,
                        message: "prior owner drained".into(),
                    });
                    if let Some(acknowledged) = acknowledged {
                        let _ = acknowledged.send(());
                    }
                }
            }
        });
        let supervisor = Supervisor {
            entry: None,
            commands,
            task,
        };

        set_supervisor_transcript(&supervisor, None).await;
        assert!(matches!(
            received.try_recv(),
            Ok(FederationEvent::Notice { message, .. }) if message == "prior owner drained"
        ));
        stop_supervisor(supervisor).await;
    }

    #[tokio::test]
    async fn offline_reply_call_returns_an_error_to_its_caller() {
        let server_id = ServerId::new("server-b");
        let (events, _received) = mpsc::unbounded_channel();
        let (reply, received) = tokio::sync::oneshot::channel();
        let mut selected = None;
        handle_offline_command(
            SupervisorCommand::Request {
                method: "ListModels",
                params: serde_json::Value::Null,
                reply,
            },
            &mut selected,
            &server_id,
            &events,
        );
        assert!(
            matches!(received.await.unwrap(), Err(comet_rpc::RpcError::Failed(message)) if message.contains("offline"))
        );
    }
}
