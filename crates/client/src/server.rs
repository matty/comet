use std::sync::Arc;

use comet_doc::SessionMessageEntry;
use comet_proto::{Chat, Device, RemoteConnectionState, ServerHello, ServerRef, Session, Space};
use comet_rpc::{RpcClient, RpcError, RpcStream, methods};
use futures::future::BoxFuture;
use futures::stream::{FuturesUnordered, StreamExt};
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
    let mut calls: FuturesUnordered<BoxFuture<'static, Result<(), RpcError>>> =
        FuturesUnordered::new();

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
            result = calls.next(), if !calls.is_empty() => {
                if let Some(Err(error)) = result {
                    let _ = events.send(FederationEvent::Notice {
                        server_id: hello.server_id.clone(),
                        message: error.to_string(),
                    });
                }
                continue;
            }
            command = commands.recv() => match command {
                Some(SupervisorCommand::Call(method, params)) => {
                    let client = client.clone();
                    calls.push(Box::pin(async move { client.call(method, params).await.map(|_| ()) }));
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
}
