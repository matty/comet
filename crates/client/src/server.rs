use std::collections::VecDeque;
use std::sync::Arc;

use comet_doc::SessionMessageEntry;
use comet_proto::{Chat, Device, RemoteConnectionState, ServerHello, ServerRef, Session, Space};
use comet_rpc::{RpcClient, RpcError, RpcStream, methods};
use futures::{StreamExt as _, future::BoxFuture, stream::FuturesUnordered};
use tokio::sync::mpsc;

use crate::{FederationEvent, FederationStream, ServerState};

pub(crate) enum SupervisorCommand {
    Call(&'static str, serde_json::Value),
    Request {
        method: &'static str,
        params: serde_json::Value,
        reply: tokio::sync::oneshot::Sender<Result<serde_json::Value, RpcError>>,
    },
    Subscribe {
        method: &'static str,
        params: serde_json::Value,
        reply: tokio::sync::oneshot::Sender<Result<FederationStream, RpcError>>,
    },
    WatchTranscript {
        chat_id: Option<String>,
        acknowledged: Option<tokio::sync::oneshot::Sender<()>>,
    },
    Reconnect,
    Shutdown,
}

pub(crate) enum ConnectedExit {
    Reconnect,
    Shutdown,
}

const MAX_QUEUED_CALLS: usize = 32;
const MAX_PENDING_SUBSCRIPTIONS: usize = 32;
const SUBSCRIPTION_BUFFER: usize = 16;
struct GenericCall {
    method: &'static str,
    params: serde_json::Value,
    reply: Option<tokio::sync::oneshot::Sender<Result<serde_json::Value, RpcError>>>,
}

struct ActiveCall {
    future: BoxFuture<'static, Result<serde_json::Value, RpcError>>,
    reply: Option<tokio::sync::oneshot::Sender<Result<serde_json::Value, RpcError>>>,
}

enum SubscriptionSetupResult {
    Canceled,
    Completed {
        reply: tokio::sync::oneshot::Sender<Result<FederationStream, RpcError>>,
        result: Result<RpcStream, RpcError>,
    },
}

type SubscriptionSetup = BoxFuture<'static, SubscriptionSetupResult>;

async fn subscription_setup_next(
    setups: &mut FuturesUnordered<SubscriptionSetup>,
) -> SubscriptionSetupResult {
    if setups.is_empty() {
        futures::future::pending().await
    } else {
        setups
            .next()
            .await
            .expect("non-empty subscription setup set")
    }
}

async fn subscription_forwarder_next(
    tasks: &mut tokio::task::JoinSet<()>,
) -> Option<Result<(), tokio::task::JoinError>> {
    if tasks.is_empty() {
        futures::future::pending().await
    } else {
        tasks.join_next().await
    }
}

async fn stop_subscription_forwarders(tasks: &mut tokio::task::JoinSet<()>) {
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
}

fn start_call(client: Arc<RpcClient>, call: GenericCall) -> ActiveCall {
    ActiveCall {
        future: Box::pin(async move { client.call(call.method, call.params).await }),
        reply: call.reply,
    }
}

async fn active_call_next(active: &mut Option<ActiveCall>) -> Result<serde_json::Value, RpcError> {
    match active {
        Some(call) => call.future.as_mut().await,
        None => futures::future::pending().await,
    }
}

fn decode<T: serde::de::DeserializeOwned>(
    method: &'static str,
    value: Option<serde_json::Value>,
) -> Result<Vec<T>, RpcError> {
    let value = value.ok_or(RpcError::Closed)?;
    serde_json::from_value(value)
        .map_err(|error| RpcError::Failed(format!("invalid {method} snapshot: {error}")))
}

async fn transcript_next(
    transcript: &mut Option<(ServerRef, RpcStream)>,
) -> Option<serde_json::Value> {
    match transcript {
        Some((_, receiver)) => receiver.recv().await,
        None => futures::future::pending().await,
    }
}

async fn subscribe_transcript(
    client: &RpcClient,
    server_id: &comet_proto::ServerId,
    chat_id: &str,
) -> Result<(ServerRef, RpcStream), RpcError> {
    let chat = ServerRef::new(server_id.clone(), chat_id);
    let receiver = client
        .subscribe(
            methods::WATCH_DOC_MESSAGES,
            serde_json::json!({"chatId": chat_id}),
        )
        .await?;
    Ok((chat, receiver))
}

enum Phase<T> {
    Ready(T),
    Exit(ConnectedExit),
}

async fn await_initial<T>(
    operation: impl std::future::Future<Output = Result<T, RpcError>>,
    commands: &mut mpsc::UnboundedReceiver<SupervisorCommand>,
    selected_chat: &mut Option<String>,
    server_id: &comet_proto::ServerId,
    events: &mpsc::UnboundedSender<FederationEvent>,
) -> Result<Phase<T>, RpcError> {
    tokio::pin!(operation);
    loop {
        tokio::select! {
            result = &mut operation => return result.map(Phase::Ready),
            command = commands.recv() => match command {
                Some(SupervisorCommand::Reconnect) => return Ok(Phase::Exit(ConnectedExit::Reconnect)),
                Some(SupervisorCommand::Shutdown) | None => return Ok(Phase::Exit(ConnectedExit::Shutdown)),
                Some(SupervisorCommand::WatchTranscript { chat_id, acknowledged }) => {
                    *selected_chat = chat_id;
                    if let Some(acknowledged) = acknowledged { let _ = acknowledged.send(()); }
                }
                Some(SupervisorCommand::Call(method, _)) => {
                    let _ = events.send(FederationEvent::Notice {
                        server_id: server_id.clone(),
                        message: format!("cannot call {method} while server is connecting"),
                    });
                }
                Some(SupervisorCommand::Request { method, reply, .. }) => {
                    let _ = reply.send(Err(RpcError::Failed(format!("cannot call {method} while server is connecting"))));
                }
                Some(SupervisorCommand::Subscribe { method, reply, .. }) => {
                    let _ = reply.send(Err(RpcError::Failed(format!("cannot subscribe to {method} while server is connecting"))));
                }
            }
        }
    }
}

async fn replace_transcript(
    client: &RpcClient,
    server_id: &comet_proto::ServerId,
    commands: &mut mpsc::UnboundedReceiver<SupervisorCommand>,
    selected_chat: &mut Option<String>,
    events: &mpsc::UnboundedSender<FederationEvent>,
) -> Result<Phase<Option<(ServerRef, RpcStream)>>, RpcError> {
    loop {
        let Some(chat_id) = selected_chat.clone() else {
            return Ok(Phase::Ready(None));
        };
        let operation = subscribe_transcript(client, server_id, &chat_id);
        tokio::pin!(operation);
        loop {
            tokio::select! {
                result = &mut operation => return result.map(|stream| Phase::Ready(Some(stream))),
                command = commands.recv() => match command {
                    Some(SupervisorCommand::Reconnect) => return Ok(Phase::Exit(ConnectedExit::Reconnect)),
                    Some(SupervisorCommand::Shutdown) | None => return Ok(Phase::Exit(ConnectedExit::Shutdown)),
                    Some(SupervisorCommand::WatchTranscript { chat_id, acknowledged }) => {
                        *selected_chat = chat_id;
                        if let Some(acknowledged) = acknowledged { let _ = acknowledged.send(()); }
                        break;
                    }
                    Some(SupervisorCommand::Call(method, _)) => {
                        let _ = events.send(FederationEvent::Notice {
                            server_id: server_id.clone(),
                            message: format!("cannot call {method} while transcript subscription is changing"),
                        });
                    }
                    Some(SupervisorCommand::Request { method, reply, .. }) => {
                        let _ = reply.send(Err(RpcError::Failed(format!("cannot call {method} while transcript subscription is changing"))));
                    }
                    Some(SupervisorCommand::Subscribe { method, reply, .. }) => {
                        let _ = reply.send(Err(RpcError::Failed(format!("cannot subscribe to {method} while transcript subscription is changing"))));
                    }
                }
            }
        }
    }
}

pub(crate) async fn supervise_connected(
    client: Arc<RpcClient>,
    hello: ServerHello,
    display_name: String,
    events: mpsc::UnboundedSender<FederationEvent>,
    commands: &mut mpsc::UnboundedReceiver<SupervisorCommand>,
    selected_chat: &mut Option<String>,
) -> Result<ConnectedExit, RpcError> {
    macro_rules! initial {
        ($operation:expr) => {
            match await_initial(
                $operation,
                commands,
                selected_chat,
                &hello.server_id,
                &events,
            )
            .await?
            {
                Phase::Ready(value) => value,
                Phase::Exit(exit) => return Ok(exit),
            }
        };
    }
    let mut devices = initial!(client.subscribe(methods::WATCH_DEVICES, serde_json::Value::Null));
    let mut spaces = initial!(client.subscribe(methods::WATCH_SPACES, serde_json::Value::Null));
    let mut chats = initial!(client.subscribe(methods::WATCH_CHATS, serde_json::Value::Null));
    let mut sessions = initial!(client.subscribe(methods::WATCH_SESSIONS, serde_json::Value::Null));
    let mut transcript =
        match replace_transcript(&client, &hello.server_id, commands, selected_chat, &events)
            .await?
        {
            Phase::Ready(transcript) => transcript,
            Phase::Exit(exit) => return Ok(exit),
        };
    let mut active_call = None;
    let mut queued_calls = VecDeque::<GenericCall>::new();
    let mut subscriptions = tokio::task::JoinSet::<()>::new();
    let mut subscription_setups = FuturesUnordered::<SubscriptionSetup>::new();

    let mut state = ServerState::empty(
        hello.server_id.clone(),
        display_name,
        RemoteConnectionState::Online,
    );
    let _ = events.send(FederationEvent::ServerChanged(state.clone()));

    loop {
        tokio::select! {
            value = devices.recv() => state.devices = decode::<Device>(methods::WATCH_DEVICES, value)?,
            value = spaces.recv() => state.spaces = decode::<Space>(methods::WATCH_SPACES, value)?,
            value = chats.recv() => state.chats = decode::<Chat>(methods::WATCH_CHATS, value)?,
            value = sessions.recv() => state.sessions = decode::<Session>(methods::WATCH_SESSIONS, value)?,
            value = transcript_next(&mut transcript) => {
                let Some((chat, _)) = transcript.as_ref() else { continue; };
                let entries = decode::<SessionMessageEntry>(methods::WATCH_DOC_MESSAGES, value)?;
                let _ = events.send(FederationEvent::Transcript { chat: chat.clone(), entries });
                continue;
            }
            result = active_call_next(&mut active_call) => {
                let finished = active_call.take().expect("active call completed");
                if let Some(reply) = finished.reply {
                    let _ = reply.send(result);
                } else if let Err(error) = result {
                    let _ = events.send(FederationEvent::Notice {
                        server_id: hello.server_id.clone(),
                        message: error.to_string(),
                    });
                }
                active_call = queued_calls
                    .pop_front()
                    .map(|call| start_call(client.clone(), call));
                continue;
            }
            setup = subscription_setup_next(&mut subscription_setups) => {
                let SubscriptionSetupResult::Completed { reply, result } = setup else {
                    continue;
                };
                match result {
                    Ok(mut source) => {
                        let (send, receive) = mpsc::channel(SUBSCRIPTION_BUFFER);
                        if reply.is_closed() {
                            continue;
                        }
                        subscriptions.spawn(async move {
                            loop {
                                tokio::select! {
                                    biased;
                                    _ = send.closed() => break,
                                    value = source.recv() => match value {
                                        Some(value) => {
                                            if send.send(value).await.is_err() { break; }
                                        }
                                        None => break,
                                    }
                                }
                            }
                        });
                        let _ = reply.send(Ok(receive));
                    }
                    Err(error) => { let _ = reply.send(Err(error)); }
                }
                continue;
            }
            _ = subscription_forwarder_next(&mut subscriptions) => {
                continue;
            }
            command = commands.recv() => match command {
                Some(SupervisorCommand::Call(method, params)) => {
                    let call = GenericCall { method, params, reply: None };
                    if active_call.is_none() {
                        active_call = Some(start_call(client.clone(), call));
                    } else if queued_calls.len() < MAX_QUEUED_CALLS {
                        queued_calls.push_back(call);
                    } else {
                        let _ = events.send(FederationEvent::Notice {
                            server_id: hello.server_id.clone(),
                            message: format!(
                                "cannot call {method}: generic RPC queue is full ({MAX_QUEUED_CALLS})"
                            ),
                        });
                    }
                    continue;
                }
                Some(SupervisorCommand::Request { method, params, reply }) => {
                    let call = GenericCall { method, params, reply: Some(reply) };
                    if active_call.is_none() {
                        active_call = Some(start_call(client.clone(), call));
                    } else if queued_calls.len() < MAX_QUEUED_CALLS {
                        queued_calls.push_back(call);
                    } else if let Some(reply) = call.reply {
                        let _ = reply.send(Err(RpcError::Failed(format!(
                            "cannot call {method}: generic RPC queue is full ({MAX_QUEUED_CALLS})"
                        ))));
                    }
                    continue;
                }
                Some(SupervisorCommand::Subscribe { method, params, reply }) => {
                    if subscription_setups.len() >= MAX_PENDING_SUBSCRIPTIONS {
                        let _ = reply.send(Err(RpcError::Failed(format!(
                            "cannot subscribe to {method}: subscription setup queue is full ({MAX_PENDING_SUBSCRIPTIONS})"
                        ))));
                    } else {
                        let setup_client = client.clone();
                        subscription_setups.push(Box::pin(async move {
                            let mut reply = reply;
                            tokio::select! {
                                biased;
                                _ = reply.closed() => SubscriptionSetupResult::Canceled,
                                result = setup_client.subscribe(method, params) => {
                                    SubscriptionSetupResult::Completed { reply, result }
                                }
                            }
                        }));
                    }
                    continue;
                }
                Some(SupervisorCommand::WatchTranscript { chat_id, acknowledged }) => {
                    if let Some((old, _)) = transcript.take() {
                        let _ = events.send(FederationEvent::Transcript { chat: old, entries: Vec::new() });
                    }
                    *selected_chat = chat_id;
                    transcript = match replace_transcript(
                        &client,
                        &hello.server_id,
                        commands,
                        selected_chat,
                        &events,
                    )
                    .await? {
                        Phase::Ready(transcript) => transcript,
                        Phase::Exit(exit) => return Ok(exit),
                    };
                    if let Some(acknowledged) = acknowledged { let _ = acknowledged.send(()); }
                    continue;
                }
                Some(SupervisorCommand::Reconnect) => {
                    cancel_calls(&mut active_call, &mut queued_calls);
                    stop_subscription_forwarders(&mut subscriptions).await;
                    return Ok(ConnectedExit::Reconnect)
                },
                Some(SupervisorCommand::Shutdown) | None => {
                    cancel_calls(&mut active_call, &mut queued_calls);
                    stop_subscription_forwarders(&mut subscriptions).await;
                    return Ok(ConnectedExit::Shutdown)
                },
            }
        }
        let _ = events.send(FederationEvent::ServerChanged(state.clone()));
    }
}

fn cancel_calls(active: &mut Option<ActiveCall>, queued: &mut VecDeque<GenericCall>) {
    if let Some(mut call) = active.take()
        && let Some(reply) = call.reply.take()
    {
        let _ = reply.send(Err(RpcError::Closed));
    }
    for mut call in queued.drain(..) {
        if let Some(reply) = call.reply.take() {
            let _ = reply.send(Err(RpcError::Closed));
        }
    }
}

pub(crate) fn clear_selected_transcript(
    server_id: &comet_proto::ServerId,
    selected_chat: &Option<String>,
    events: &mpsc::UnboundedSender<FederationEvent>,
) {
    if let Some(chat_id) = selected_chat {
        let _ = events.send(FederationEvent::Transcript {
            chat: ServerRef::new(server_id.clone(), chat_id),
            entries: Vec::new(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use comet_proto::{PROTOCOL_VERSION, ServerId};
    use comet_rpc::{RpcReply, RpcService};
    use futures::StreamExt;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    type SupervisorTask = tokio::task::JoinHandle<Result<ConnectedExit, RpcError>>;
    type StalledFixture = (
        SupervisorTask,
        mpsc::UnboundedSender<SupervisorCommand>,
        mpsc::UnboundedReceiver<FederationEvent>,
        Arc<tokio::sync::Notify>,
        Arc<AtomicBool>,
    );
    type OrderedFixture = (
        SupervisorTask,
        mpsc::UnboundedSender<SupervisorCommand>,
        mpsc::UnboundedReceiver<FederationEvent>,
        Arc<tokio::sync::Notify>,
        Arc<tokio::sync::Notify>,
        Arc<tokio::sync::Notify>,
    );

    struct DropFlag(Arc<AtomicBool>);
    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    struct StalledService {
        stalled_method: &'static str,
        started: Arc<tokio::sync::Notify>,
        dropped: Arc<AtomicBool>,
    }

    struct OrderedCallService {
        first_started: Arc<tokio::sync::Notify>,
        release_first: Arc<tokio::sync::Notify>,
        second_started: Arc<tokio::sync::Notify>,
    }

    struct QuietSubscriptionService {
        dropped: Arc<AtomicUsize>,
    }

    struct CountDrop(Arc<AtomicUsize>);

    impl Drop for CountDrop {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[async_trait::async_trait]
    impl RpcService for QuietSubscriptionService {
        async fn handle(
            &self,
            method: &str,
            _params: serde_json::Value,
        ) -> Result<RpcReply, RpcError> {
            match method {
                methods::WATCH_DEVICES
                | methods::WATCH_SPACES
                | methods::WATCH_CHATS
                | methods::WATCH_SESSIONS => Ok(RpcReply::Stream(
                    futures::stream::once(async { serde_json::json!([]) })
                        .chain(futures::stream::pending())
                        .boxed(),
                )),
                "QuietEvents" => {
                    let guard = CountDrop(self.dropped.clone());
                    Ok(RpcReply::Stream(
                        futures::stream::unfold(guard, |guard| async move {
                            futures::future::pending::<()>().await;
                            Some((serde_json::Value::Null, guard))
                        })
                        .boxed(),
                    ))
                }
                other => Err(RpcError::UnknownMethod(other.into())),
            }
        }
    }

    #[async_trait::async_trait]
    impl RpcService for OrderedCallService {
        async fn handle(
            &self,
            method: &str,
            _params: serde_json::Value,
        ) -> Result<RpcReply, RpcError> {
            match method {
                methods::WATCH_DEVICES
                | methods::WATCH_SPACES
                | methods::WATCH_CHATS
                | methods::WATCH_SESSIONS => Ok(RpcReply::Stream(
                    futures::stream::once(async { serde_json::json!([]) })
                        .chain(futures::stream::pending())
                        .boxed(),
                )),
                "First" => {
                    self.first_started.notify_one();
                    self.release_first.notified().await;
                    RpcReply::value(&true)
                }
                "Second" | "Queued" => {
                    self.second_started.notify_one();
                    RpcReply::value(&true)
                }
                "Fail" => Err(RpcError::Failed("ordered failure".into())),
                "Events" => Ok(RpcReply::Stream(
                    futures::stream::once(async { serde_json::json!({"seq": 1}) })
                        .chain(futures::stream::pending())
                        .boxed(),
                )),
                "FastEvents" => Ok(RpcReply::Stream(
                    futures::stream::iter((0..1_000).map(|seq| serde_json::json!(seq)))
                        .chain(futures::stream::pending())
                        .boxed(),
                )),
                "FailStream" => Err(RpcError::Failed("stream failure".into())),
                other => Err(RpcError::UnknownMethod(other.into())),
            }
        }
    }

    #[async_trait::async_trait]
    impl RpcService for StalledService {
        async fn handle(
            &self,
            method: &str,
            _params: serde_json::Value,
        ) -> Result<RpcReply, RpcError> {
            if method == self.stalled_method {
                let _guard = DropFlag(self.dropped.clone());
                self.started.notify_waiters();
                return futures::future::pending().await;
            }
            match method {
                methods::WATCH_DEVICES
                | methods::WATCH_SPACES
                | methods::WATCH_CHATS
                | methods::WATCH_SESSIONS
                | methods::WATCH_DOC_MESSAGES => Ok(RpcReply::Stream(
                    futures::stream::once(async { serde_json::json!([]) })
                        .chain(futures::stream::pending())
                        .boxed(),
                )),
                other => Err(RpcError::UnknownMethod(other.into())),
            }
        }
    }

    fn hello() -> ServerHello {
        ServerHello {
            protocol_version: PROTOCOL_VERSION,
            server_id: ServerId::new(
                "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            ),
            device_id: "device-b".into(),
            name: "B".into(),
            capabilities: Vec::new(),
        }
    }

    fn spawn_stalled(method: &'static str) -> StalledFixture {
        let started = Arc::new(tokio::sync::Notify::new());
        let dropped = Arc::new(AtomicBool::new(false));
        let client = Arc::new(comet_rpc::memory_client(Arc::new(StalledService {
            stalled_method: method,
            started: started.clone(),
            dropped: dropped.clone(),
        })));
        let (commands, mut command_rx) = mpsc::unbounded_channel();
        let (events, event_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            let mut selected = None;
            supervise_connected(
                client,
                hello(),
                "B".into(),
                events,
                &mut command_rx,
                &mut selected,
            )
            .await
        });
        (task, commands, event_rx, started, dropped)
    }

    fn spawn_with_client(
        client: RpcClient,
    ) -> (
        SupervisorTask,
        mpsc::UnboundedSender<SupervisorCommand>,
        mpsc::UnboundedReceiver<FederationEvent>,
    ) {
        let (commands, mut command_rx) = mpsc::unbounded_channel();
        let (events, event_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            let mut selected = None;
            supervise_connected(
                Arc::new(client),
                hello(),
                "B".into(),
                events,
                &mut command_rx,
                &mut selected,
            )
            .await
        });
        (task, commands, event_rx)
    }

    fn spawn_ordered() -> OrderedFixture {
        let first_started = Arc::new(tokio::sync::Notify::new());
        let release_first = Arc::new(tokio::sync::Notify::new());
        let second_started = Arc::new(tokio::sync::Notify::new());
        let client = comet_rpc::memory_client(Arc::new(OrderedCallService {
            first_started: first_started.clone(),
            release_first: release_first.clone(),
            second_started: second_started.clone(),
        }));
        let (task, commands, events) = spawn_with_client(client);
        (
            task,
            commands,
            events,
            first_started,
            release_first,
            second_started,
        )
    }

    fn backpressured_client(capacity: usize, prefill: usize) -> RpcClient {
        let (out, outbound) = mpsc::channel(capacity);
        for _ in 0..prefill {
            out.try_send("occupied".into()).unwrap();
        }
        let (inbound_sender, inbound) = mpsc::channel(1);
        tokio::spawn(async move {
            let _channels = (outbound, inbound_sender);
            futures::future::pending::<()>().await;
        });
        RpcClient::new(out, inbound)
    }

    fn controllable_transport() -> (RpcClient, mpsc::Sender<String>, mpsc::Receiver<String>) {
        let (out, outbound) = mpsc::channel(1);
        let control = out.clone();
        let (inbound_sender, inbound) = mpsc::channel(1);
        tokio::spawn(async move {
            let _inbound_sender = inbound_sender;
            futures::future::pending::<()>().await;
        });
        (RpcClient::new(out, inbound), control, outbound)
    }

    async fn wait_online(events: &mut mpsc::UnboundedReceiver<FederationEvent>) {
        loop {
            if matches!(events.recv().await, Some(FederationEvent::ServerChanged(state)) if state.connection == RemoteConnectionState::Online)
            {
                return;
            }
        }
    }

    #[tokio::test]
    async fn reconnect_interrupts_an_initial_resource_subscription() {
        let (task, commands, _events) = spawn_with_client(backpressured_client(1, 1));
        tokio::task::yield_now().await;
        commands.send(SupervisorCommand::Reconnect).unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_millis(100), task).await;
        assert!(
            matches!(result, Ok(Ok(Ok(ConnectedExit::Reconnect)))),
            "reconnect did not interrupt the initial subscription"
        );
    }

    #[tokio::test]
    async fn reconnect_interrupts_a_connected_generic_rpc() {
        let (task, commands, mut events, started, dropped) = spawn_stalled("Block");
        wait_online(&mut events).await;
        commands
            .send(SupervisorCommand::Call("Block", serde_json::Value::Null))
            .unwrap();
        started.notified().await;
        commands.send(SupervisorCommand::Reconnect).unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_millis(100), task).await;
        assert!(
            matches!(result, Ok(Ok(Ok(ConnectedExit::Reconnect)))),
            "reconnect did not interrupt the generic RPC"
        );
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn shutdown_interrupts_a_transcript_subscription() {
        let (task, commands, mut events) = spawn_with_client(backpressured_client(4, 0));
        wait_online(&mut events).await;
        commands
            .send(SupervisorCommand::WatchTranscript {
                chat_id: Some("chat-1".into()),
                acknowledged: None,
            })
            .unwrap();
        tokio::task::yield_now().await;
        commands.send(SupervisorCommand::Shutdown).unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_millis(100), task).await;
        assert!(
            matches!(result, Ok(Ok(Ok(ConnectedExit::Shutdown)))),
            "shutdown did not interrupt transcript subscription"
        );
    }

    #[tokio::test]
    async fn generic_calls_preserve_fifo_execution_order() {
        let (task, commands, mut events, first_started, release_first, second_started) =
            spawn_ordered();
        wait_online(&mut events).await;
        commands
            .send(SupervisorCommand::Call("First", serde_json::Value::Null))
            .unwrap();
        commands
            .send(SupervisorCommand::Call("Second", serde_json::Value::Null))
            .unwrap();
        first_started.notified().await;
        let second_was_early = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            second_started.notified(),
        )
        .await
        .is_ok();
        release_first.notify_one();
        if !second_was_early {
            second_started.notified().await;
        }
        commands.send(SupervisorCommand::Shutdown).unwrap();
        let _ = task.await;
        assert!(
            !second_was_early,
            "second call started before first completed"
        );
    }

    #[tokio::test]
    async fn generic_call_queue_overflow_emits_a_notice() {
        let (task, commands, mut events, first_started, release_first, _second_started) =
            spawn_ordered();
        wait_online(&mut events).await;
        commands
            .send(SupervisorCommand::Call("First", serde_json::Value::Null))
            .unwrap();
        first_started.notified().await;
        for _ in 0..65 {
            commands
                .send(SupervisorCommand::Call("Queued", serde_json::Value::Null))
                .unwrap();
        }
        let overflow = tokio::time::timeout(std::time::Duration::from_millis(100), async {
            loop {
                if matches!(events.recv().await, Some(FederationEvent::Notice { message, .. }) if message.contains("queue is full")) {
                    return;
                }
            }
        })
        .await;
        release_first.notify_one();
        commands.send(SupervisorCommand::Shutdown).unwrap();
        let _ = task.await;
        overflow.expect("unbounded generic call queue emitted no overflow notice");
    }

    #[tokio::test]
    async fn reconnect_cancels_the_active_call_and_discards_its_fifo_queue() {
        let (task, commands, mut events, first_started, _release_first, second_started) =
            spawn_ordered();
        wait_online(&mut events).await;
        commands
            .send(SupervisorCommand::Call("First", serde_json::Value::Null))
            .unwrap();
        commands
            .send(SupervisorCommand::Call("Second", serde_json::Value::Null))
            .unwrap();
        first_started.notified().await;
        commands.send(SupervisorCommand::Reconnect).unwrap();
        assert!(matches!(task.await, Ok(Ok(ConnectedExit::Reconnect))));
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(50),
                second_started.notified()
            )
            .await
            .is_err(),
            "queued call reached the server after reconnect"
        );
    }

    #[tokio::test]
    async fn failed_generic_call_emits_notice_then_promotes_the_fifo_head() {
        let (task, commands, mut events, _first_started, _release_first, second_started) =
            spawn_ordered();
        wait_online(&mut events).await;
        commands
            .send(SupervisorCommand::Call("Fail", serde_json::Value::Null))
            .unwrap();
        commands
            .send(SupervisorCommand::Call("Second", serde_json::Value::Null))
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_millis(100), async {
            loop {
                if matches!(events.recv().await, Some(FederationEvent::Notice { message, .. }) if message.contains("ordered failure")) {
                    break;
                }
            }
        })
        .await
        .expect("failed generic call emitted no Notice");
        second_started.notified().await;
        commands.send(SupervisorCommand::Shutdown).unwrap();
        let _ = task.await;
    }

    #[tokio::test]
    async fn reply_call_returns_the_server_value() {
        let (task, commands, mut events, _first_started, _release_first, second_started) =
            spawn_ordered();
        wait_online(&mut events).await;
        let (reply, received) = tokio::sync::oneshot::channel();
        commands
            .send(SupervisorCommand::Request {
                method: "Second",
                params: serde_json::Value::Null,
                reply,
            })
            .unwrap();
        second_started.notified().await;
        assert_eq!(received.await.unwrap().unwrap(), serde_json::json!(true));
        commands.send(SupervisorCommand::Shutdown).unwrap();
        let _ = task.await;
    }

    #[tokio::test]
    async fn reply_call_returns_the_server_error() {
        let (task, commands, mut events, ..) = spawn_ordered();
        wait_online(&mut events).await;
        let (reply, received) = tokio::sync::oneshot::channel();
        commands
            .send(SupervisorCommand::Request {
                method: "Fail",
                params: serde_json::Value::Null,
                reply,
            })
            .unwrap();
        assert!(
            matches!(received.await.unwrap(), Err(RpcError::Failed(message)) if message == "ordered failure")
        );
        commands.send(SupervisorCommand::Shutdown).unwrap();
        let _ = task.await;
    }

    #[tokio::test]
    async fn reconnect_cancels_a_reply_call() {
        let (task, commands, mut events, started, _dropped) = spawn_stalled("Block");
        wait_online(&mut events).await;
        let (reply, received) = tokio::sync::oneshot::channel();
        commands
            .send(SupervisorCommand::Request {
                method: "Block",
                params: serde_json::Value::Null,
                reply,
            })
            .unwrap();
        started.notified().await;
        commands.send(SupervisorCommand::Reconnect).unwrap();
        assert!(matches!(received.await.unwrap(), Err(RpcError::Closed)));
        assert!(matches!(task.await, Ok(Ok(ConnectedExit::Reconnect))));
    }

    #[tokio::test]
    async fn reply_call_queue_overflow_returns_an_error() {
        let (task, commands, mut events, first_started, release_first, _) = spawn_ordered();
        wait_online(&mut events).await;
        commands
            .send(SupervisorCommand::Call("First", serde_json::Value::Null))
            .unwrap();
        first_started.notified().await;
        let mut receivers = Vec::new();
        for _ in 0..=MAX_QUEUED_CALLS {
            let (reply, received) = tokio::sync::oneshot::channel();
            commands
                .send(SupervisorCommand::Request {
                    method: "Queued",
                    params: serde_json::Value::Null,
                    reply,
                })
                .unwrap();
            receivers.push(received);
        }
        assert!(
            matches!(receivers.pop().unwrap().await.unwrap(), Err(RpcError::Failed(message)) if message.contains("queue is full"))
        );
        release_first.notify_one();
        commands.send(SupervisorCommand::Shutdown).unwrap();
        let _ = task.await;
    }

    #[tokio::test]
    async fn subscribe_returns_the_server_stream_and_errors() {
        let (task, commands, mut events, ..) = spawn_ordered();
        wait_online(&mut events).await;
        let (reply, received) = tokio::sync::oneshot::channel();
        commands
            .send(SupervisorCommand::Subscribe {
                method: "Events",
                params: serde_json::Value::Null,
                reply,
            })
            .unwrap();
        let mut stream = received.await.unwrap().unwrap();
        assert_eq!(stream.recv().await, Some(serde_json::json!({"seq": 1})));

        let (reply, received) = tokio::sync::oneshot::channel();
        commands
            .send(SupervisorCommand::Subscribe {
                method: "FailStream",
                params: serde_json::Value::Null,
                reply,
            })
            .unwrap();
        let mut failed = received.await.unwrap().unwrap();
        assert_eq!(failed.recv().await, None);
        commands.send(SupervisorCommand::Shutdown).unwrap();
        let _ = task.await;
    }

    #[tokio::test]
    async fn reconnect_closes_a_returned_subscription() {
        let (task, commands, mut events, ..) = spawn_ordered();
        wait_online(&mut events).await;
        let (reply, received) = tokio::sync::oneshot::channel();
        commands
            .send(SupervisorCommand::Subscribe {
                method: "Events",
                params: serde_json::Value::Null,
                reply,
            })
            .unwrap();
        let mut stream = received.await.unwrap().unwrap();
        assert!(stream.recv().await.is_some());
        commands.send(SupervisorCommand::Reconnect).unwrap();
        assert!(matches!(task.await, Ok(Ok(ConnectedExit::Reconnect))));
        assert_eq!(stream.recv().await, None);
    }

    #[tokio::test]
    async fn reconnect_interrupts_a_stalled_subscription_handshake() {
        let (client, transport, mut outbound) = controllable_transport();
        let (task, commands, mut events) = spawn_with_client(client);
        for _ in 0..4 {
            outbound.recv().await.expect("initial watch request");
        }
        wait_online(&mut events).await;
        transport.try_send("occupied".into()).unwrap();
        let (reply, _received) = tokio::sync::oneshot::channel();
        commands
            .send(SupervisorCommand::Subscribe {
                method: "SlowEvents",
                params: serde_json::Value::Null,
                reply,
            })
            .unwrap();
        tokio::task::yield_now().await;

        commands.send(SupervisorCommand::Reconnect).unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_millis(100), task).await;
        assert!(
            matches!(result, Ok(Ok(Ok(ConnectedExit::Reconnect)))),
            "stalled subscription handshake blocked reconnect"
        );
    }

    #[tokio::test]
    async fn dropping_a_quiet_subscription_receiver_cancels_server_work() {
        let dropped = Arc::new(AtomicUsize::new(0));
        let client = comet_rpc::memory_client(Arc::new(QuietSubscriptionService {
            dropped: dropped.clone(),
        }));
        let (task, commands, mut events) = spawn_with_client(client);
        wait_online(&mut events).await;
        let (reply, received) = tokio::sync::oneshot::channel();
        commands
            .send(SupervisorCommand::Subscribe {
                method: "QuietEvents",
                params: serde_json::Value::Null,
                reply,
            })
            .unwrap();
        let stream = received.await.unwrap().unwrap();
        drop(stream);

        tokio::time::timeout(std::time::Duration::from_millis(100), async {
            while dropped.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("quiet server stream survived receiver drop");
        commands.send(SupervisorCommand::Shutdown).unwrap();
        let _ = task.await;
    }

    #[tokio::test]
    async fn fast_subscription_source_cannot_overfill_the_public_receiver() {
        let (task, commands, mut events, ..) = spawn_ordered();
        wait_online(&mut events).await;
        let (reply, received) = tokio::sync::oneshot::channel();
        commands
            .send(SupervisorCommand::Subscribe {
                method: "FastEvents",
                params: serde_json::Value::Null,
                reply,
            })
            .unwrap();
        let stream = received.await.unwrap().unwrap();
        tokio::task::yield_now().await;
        assert!(
            stream.len() <= 16,
            "public subscription buffer grew unbounded"
        );
        drop(stream);
        commands.send(SupervisorCommand::Shutdown).unwrap();
        let _ = task.await;
    }

    #[tokio::test]
    async fn completed_subscription_forwarders_do_not_accumulate() {
        let dropped = Arc::new(AtomicUsize::new(0));
        let client = comet_rpc::memory_client(Arc::new(QuietSubscriptionService {
            dropped: dropped.clone(),
        }));
        let (task, commands, mut events) = spawn_with_client(client);
        wait_online(&mut events).await;
        for _ in 0..32 {
            let (reply, received) = tokio::sync::oneshot::channel();
            commands
                .send(SupervisorCommand::Subscribe {
                    method: "QuietEvents",
                    params: serde_json::Value::Null,
                    reply,
                })
                .unwrap();
            drop(received.await.unwrap().unwrap());
        }
        tokio::time::timeout(std::time::Duration::from_millis(200), async {
            while dropped.load(Ordering::SeqCst) != 32 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("completed forwarders retained their source streams");
        commands.send(SupervisorCommand::Shutdown).unwrap();
        let _ = task.await;
    }

    #[tokio::test]
    async fn dropping_a_pending_subscription_reply_cancels_its_transport_send() {
        let (client, transport, mut outbound) = controllable_transport();
        let (task, commands, mut events) = spawn_with_client(client);
        for _ in 0..4 {
            outbound.recv().await.expect("initial watch request");
        }
        wait_online(&mut events).await;
        transport.try_send("occupied".into()).unwrap();

        let (reply, received) = tokio::sync::oneshot::channel();
        commands
            .send(SupervisorCommand::Subscribe {
                method: "CanceledEvents",
                params: serde_json::Value::Null,
                reply,
            })
            .unwrap();
        tokio::task::yield_now().await;
        drop(received);
        tokio::task::yield_now().await;

        assert_eq!(outbound.recv().await.as_deref(), Some("occupied"));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), outbound.recv())
                .await
                .is_err(),
            "canceled subscription handshake still reached the transport"
        );
        commands.send(SupervisorCommand::Shutdown).unwrap();
        let _ = task.await;
    }

    #[tokio::test]
    async fn pending_subscription_setup_overflow_returns_an_error() {
        let (client, transport, mut outbound) = controllable_transport();
        let (task, commands, mut events) = spawn_with_client(client);
        for _ in 0..4 {
            outbound.recv().await.expect("initial watch request");
        }
        wait_online(&mut events).await;
        transport.try_send("occupied".into()).unwrap();

        let mut receivers = Vec::new();
        for _ in 0..=32 {
            let (reply, received) = tokio::sync::oneshot::channel();
            commands
                .send(SupervisorCommand::Subscribe {
                    method: "QueuedEvents",
                    params: serde_json::Value::Null,
                    reply,
                })
                .unwrap();
            receivers.push(received);
        }
        let overflow = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            receivers.pop().unwrap(),
        )
        .await
        .expect("subscription setup overflow did not reply")
        .unwrap();
        assert!(
            matches!(overflow, Err(RpcError::Failed(message)) if message.contains("queue is full"))
        );
        commands.send(SupervisorCommand::Shutdown).unwrap();
        let _ = task.await;
    }
}
