use std::collections::VecDeque;
use std::sync::Arc;

use comet_doc::SessionMessageEntry;
use comet_proto::{Chat, Device, RemoteConnectionState, ServerHello, ServerRef, Session, Space};
use comet_rpc::{RpcClient, RpcError, RpcStream, methods};
use futures::future::BoxFuture;
use tokio::sync::mpsc;

use crate::{FederationEvent, ServerState};

pub(crate) enum SupervisorCommand {
    Call(&'static str, serde_json::Value),
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
type GenericCall = (&'static str, serde_json::Value);

fn start_call(
    client: Arc<RpcClient>,
    call: GenericCall,
) -> BoxFuture<'static, Result<(), RpcError>> {
    Box::pin(async move { client.call(call.0, call.1).await.map(|_| ()) })
}

async fn active_call_next(
    active: &mut Option<BoxFuture<'static, Result<(), RpcError>>>,
) -> Result<(), RpcError> {
    match active {
        Some(call) => call.await,
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
                if let Err(error) = result {
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
            command = commands.recv() => match command {
                Some(SupervisorCommand::Call(method, params)) => {
                    let call = (method, params);
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
                Some(SupervisorCommand::Reconnect) => return Ok(ConnectedExit::Reconnect),
                Some(SupervisorCommand::Shutdown) | None => return Ok(ConnectedExit::Shutdown),
            }
        }
        let _ = events.send(FederationEvent::ServerChanged(state.clone()));
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
    use std::sync::atomic::{AtomicBool, Ordering};

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
}
