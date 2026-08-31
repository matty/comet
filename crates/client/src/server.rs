use std::collections::VecDeque;
use std::sync::Arc;

use comet_doc::{SessionMessageEntry, TranscriptFrame};
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

impl GenericCall {
    /// A `Request` whose caller stopped waiting must not occupy the generic
    /// lane. Fire-and-forget `Call`s intentionally have no receiver.
    fn receiver_is_closed(&self) -> bool {
        self.reply.as_ref().is_some_and(|reply| reply.is_closed())
    }
}

struct ActiveCall {
    future: BoxFuture<'static, Result<serde_json::Value, RpcError>>,
    reply: Option<tokio::sync::oneshot::Sender<Result<serde_json::Value, RpcError>>>,
}

enum ActiveCallEvent {
    Completed(Result<serde_json::Value, RpcError>),
    Canceled,
}

enum TranscriptReplacementWait<T> {
    Ready(T),
    Restart,
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

async fn active_call_next(active: &mut Option<ActiveCall>) -> ActiveCallEvent {
    match active {
        Some(call) => {
            if let Some(reply) = call.reply.as_mut() {
                tokio::select! {
                    result = call.future.as_mut() => ActiveCallEvent::Completed(result),
                    _ = reply.closed() => ActiveCallEvent::Canceled,
                }
            } else {
                ActiveCallEvent::Completed(call.future.as_mut().await)
            }
        }
        None => futures::future::pending().await,
    }
}

/// Start the first FIFO call whose requester still awaits a reply. Closed
/// `Request`s are work the caller explicitly no longer needs; plain `Call`s
/// always remain eligible.
fn promote_queued_call(
    client: Arc<RpcClient>,
    queued_calls: &mut VecDeque<GenericCall>,
) -> Option<ActiveCall> {
    while let Some(call) = queued_calls.pop_front() {
        if call.receiver_is_closed() {
            continue;
        }
        return Some(start_call(client, call));
    }
    None
}

/// Keep FIFO order for runnable work while releasing bounded admission slots
/// as soon as their Request callers stop waiting. Plain Calls have no reply
/// receiver and are therefore always retained.
fn prune_closed_queued_calls(queued_calls: &mut VecDeque<GenericCall>) {
    queued_calls.retain(|call| !call.receiver_is_closed());
}

fn finish_active_call(
    event: ActiveCallEvent,
    active_call: &mut Option<ActiveCall>,
    queued_calls: &mut VecDeque<GenericCall>,
    client: &Arc<RpcClient>,
    server_id: &comet_proto::ServerId,
    events: &mpsc::UnboundedSender<FederationEvent>,
) {
    let finished = active_call.take().expect("active call resolved");
    match event {
        ActiveCallEvent::Completed(result) => {
            if let Some(reply) = finished.reply {
                let _ = reply.send(result);
            } else if let Err(error) = result {
                let _ = events.send(FederationEvent::Notice {
                    server_id: server_id.clone(),
                    message: error.to_string(),
                });
            }
        }
        ActiveCallEvent::Canceled => {
            // The federation oneshot receiver closed (UI timeout, owner
            // change, or row removal). Dropping the live future triggers
            // RpcClient's PendingGuard cancellation before the next queued
            // request enters the generic lane.
            drop(finished);
        }
    }
    *active_call = promote_queued_call(client.clone(), queued_calls);
}

fn decode<T: serde::de::DeserializeOwned>(
    method: &'static str,
    value: Option<serde_json::Value>,
) -> Result<Vec<T>, RpcError> {
    let value = value.ok_or(RpcError::Closed)?;
    serde_json::from_value(value)
        .map_err(|error| RpcError::Failed(format!("invalid {method} snapshot: {error}")))
}

fn apply_transcript_value(
    entries: &mut Vec<SessionMessageEntry>,
    value: Option<serde_json::Value>,
) -> Result<(), RpcError> {
    let value = value.ok_or(RpcError::Closed)?;
    let frame: TranscriptFrame = serde_json::from_value(value)
        .map_err(|error| RpcError::Failed(format!("invalid WatchDocMessages frame: {error}")))?;
    comet_doc::apply_transcript_frame(entries, frame)
        .map_err(|error| RpcError::Failed(error.to_string()))
}

struct TranscriptSubscription {
    chat: ServerRef,
    receiver: RpcStream,
    entries: Vec<SessionMessageEntry>,
}

async fn transcript_next(
    transcript: &mut Option<TranscriptSubscription>,
) -> Option<serde_json::Value> {
    match transcript {
        Some(transcript) => transcript.receiver.recv().await,
        None => futures::future::pending().await,
    }
}

async fn subscribe_transcript(
    client: &RpcClient,
    server_id: &comet_proto::ServerId,
    chat_id: &str,
) -> Result<TranscriptSubscription, RpcError> {
    let chat = ServerRef::new(server_id.clone(), chat_id);
    let receiver = client
        .subscribe(
            methods::WATCH_DOC_MESSAGES,
            serde_json::json!({"chatId": chat_id}),
        )
        .await?;
    Ok(TranscriptSubscription {
        chat,
        receiver,
        entries: Vec::new(),
    })
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
) -> Result<Phase<Option<TranscriptSubscription>>, RpcError> {
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

struct ConnectedTranscriptReplacement<'a> {
    client: Arc<RpcClient>,
    server_id: &'a comet_proto::ServerId,
    commands: &'a mut mpsc::UnboundedReceiver<SupervisorCommand>,
    selected_chat: &'a mut Option<String>,
    events: &'a mpsc::UnboundedSender<FederationEvent>,
    active_call: &'a mut Option<ActiveCall>,
    queued_calls: &'a mut VecDeque<GenericCall>,
}

/// Wait for one connected transcript replacement while keeping the generic
/// lane cancelable. Both a WatchTranscript change and a desynced stream use
/// this wait, so an owner-driven caller drop cannot be hidden behind either
/// subscription setup.
async fn wait_for_transcript_replacement<T>(
    operation: impl std::future::Future<Output = Result<T, RpcError>>,
    replacement: &mut ConnectedTranscriptReplacement<'_>,
) -> Result<Phase<TranscriptReplacementWait<T>>, RpcError> {
    tokio::pin!(operation);
    loop {
        tokio::select! {
            result = &mut operation => return result.map(|value| Phase::Ready(TranscriptReplacementWait::Ready(value))),
            event = active_call_next(replacement.active_call) => {
                finish_active_call(
                    event,
                    replacement.active_call,
                    replacement.queued_calls,
                    &replacement.client,
                    replacement.server_id,
                    replacement.events,
                );
            }
            command = replacement.commands.recv() => match command {
                Some(SupervisorCommand::Reconnect) => return Ok(Phase::Exit(ConnectedExit::Reconnect)),
                Some(SupervisorCommand::Shutdown) | None => return Ok(Phase::Exit(ConnectedExit::Shutdown)),
                Some(SupervisorCommand::WatchTranscript { chat_id, acknowledged }) => {
                    *replacement.selected_chat = chat_id;
                    if let Some(acknowledged) = acknowledged { let _ = acknowledged.send(()); }
                    return Ok(Phase::Ready(TranscriptReplacementWait::Restart));
                }
                Some(SupervisorCommand::Call(method, _)) => {
                    let _ = replacement.events.send(FederationEvent::Notice {
                        server_id: replacement.server_id.clone(),
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

async fn replace_transcript_while_connected(
    client: &Arc<RpcClient>,
    server_id: &comet_proto::ServerId,
    commands: &mut mpsc::UnboundedReceiver<SupervisorCommand>,
    selected_chat: &mut Option<String>,
    events: &mpsc::UnboundedSender<FederationEvent>,
    active_call: &mut Option<ActiveCall>,
    queued_calls: &mut VecDeque<GenericCall>,
) -> Result<Phase<Option<TranscriptSubscription>>, RpcError> {
    let mut replacement = ConnectedTranscriptReplacement {
        client: client.clone(),
        server_id,
        commands,
        selected_chat,
        events,
        active_call,
        queued_calls,
    };
    loop {
        let Some(chat_id) = replacement.selected_chat.clone() else {
            return Ok(Phase::Ready(None));
        };
        let operation_client = replacement.client.clone();
        let operation = subscribe_transcript(&operation_client, replacement.server_id, &chat_id);
        match wait_for_transcript_replacement(operation, &mut replacement).await? {
            Phase::Ready(TranscriptReplacementWait::Ready(transcript)) => {
                return Ok(Phase::Ready(Some(transcript)));
            }
            Phase::Ready(TranscriptReplacementWait::Restart) => continue,
            Phase::Exit(exit) => return Ok(Phase::Exit(exit)),
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
                let Some(active) = transcript.as_mut() else { continue; };
                if let Err(error) = apply_transcript_value(&mut active.entries, value) {
                    tracing::warn!(chat = ?active.chat, %error, "resubscribing desynced transcript");
                    transcript = match replace_transcript_while_connected(
                        &client,
                        &hello.server_id,
                        commands,
                        selected_chat,
                        &events,
                        &mut active_call,
                        &mut queued_calls,
                    ).await? {
                        Phase::Ready(transcript) => transcript,
                        Phase::Exit(exit) => return Ok(exit),
                    };
                    continue;
                }
                let _ = events.send(FederationEvent::Transcript {
                    chat: active.chat.clone(),
                    entries: active.entries.clone(),
                });
                continue;
            }
            event = active_call_next(&mut active_call) => {
                finish_active_call(
                    event,
                    &mut active_call,
                    &mut queued_calls,
                    &client,
                    &hello.server_id,
                    &events,
                );
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
                    } else {
                        prune_closed_queued_calls(&mut queued_calls);
                        if queued_calls.len() < MAX_QUEUED_CALLS {
                            queued_calls.push_back(call);
                        } else {
                            let _ = events.send(FederationEvent::Notice {
                                server_id: hello.server_id.clone(),
                                message: format!(
                                    "cannot call {method}: generic RPC queue is full ({MAX_QUEUED_CALLS})"
                                ),
                            });
                        }
                    }
                    continue;
                }
                Some(SupervisorCommand::Request { method, params, reply }) => {
                    let call = GenericCall { method, params, reply: Some(reply) };
                    if call.receiver_is_closed() {
                        continue;
                    } else if active_call.is_none() {
                        active_call = Some(start_call(client.clone(), call));
                    } else {
                        prune_closed_queued_calls(&mut queued_calls);
                        if queued_calls.len() < MAX_QUEUED_CALLS {
                            queued_calls.push_back(call);
                        } else if let Some(reply) = call.reply {
                            let _ = reply.send(Err(RpcError::Failed(format!(
                                "cannot call {method}: generic RPC queue is full ({MAX_QUEUED_CALLS})"
                            ))));
                        }
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
                    if let Some(old) = transcript.take() {
                        let _ = events.send(FederationEvent::Transcript { chat: old.chat, entries: Vec::new() });
                    }
                    *selected_chat = chat_id;
                    transcript = match replace_transcript_while_connected(
                        &client,
                        &hello.server_id,
                        commands,
                        selected_chat,
                        &events,
                        &mut active_call,
                        &mut queued_calls,
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
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::task::{Context, Poll};

    #[test]
    fn transcript_wire_frames_materialize_in_the_server_copy() {
        let entry = SessionMessageEntry {
            id: "message-1".into(),
            role: comet_doc::MessageRole::Assistant,
            parts: vec![comet_doc::MessagePart::Text {
                id: "text-1".into(),
                text: "hello".into(),
            }],
            created_at: 0,
            device_id: "device-a".into(),
            status: None,
            continuation_of: None,
        };
        let value = serde_json::to_value(comet_doc::TranscriptFrame::reset(&[entry.clone()]))
            .expect("serialize reset frame");
        let mut entries = Vec::new();

        apply_transcript_value(&mut entries, Some(value)).expect("apply reset frame");

        assert_eq!(entries, vec![entry]);
    }

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

    struct FirstPollPending {
        first_polled: Arc<tokio::sync::Notify>,
        polled: Arc<AtomicBool>,
    }

    impl Future for FirstPollPending {
        type Output = Result<(), RpcError>;

        fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
            if !self.polled.swap(true, Ordering::SeqCst) {
                self.first_polled.notify_one();
            }
            Poll::Pending
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

    struct CancellationService {
        block_started: Arc<tokio::sync::Notify>,
        block_dropped: Arc<AtomicBool>,
        first_started: Arc<tokio::sync::Notify>,
        release_first: Arc<tokio::sync::Notify>,
        queued_started: Arc<AtomicUsize>,
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
    impl RpcService for CancellationService {
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
                "Block" => {
                    let guard = DropFlag(self.block_dropped.clone());
                    self.block_started.notify_one();
                    futures::future::pending::<()>().await;
                    #[allow(unreachable_code)]
                    {
                        drop(guard);
                        unreachable!()
                    }
                }
                "First" => {
                    self.first_started.notify_one();
                    self.release_first.notified().await;
                    RpcReply::value(&true)
                }
                "Queued" => {
                    self.queued_started.fetch_add(1, Ordering::SeqCst);
                    RpcReply::value(&true)
                }
                "Next" => RpcReply::value(&true),
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

    fn spawn_cancellation_service() -> (
        SupervisorTask,
        mpsc::UnboundedSender<SupervisorCommand>,
        mpsc::UnboundedReceiver<FederationEvent>,
        Arc<CancellationService>,
    ) {
        let service = Arc::new(CancellationService {
            block_started: Arc::new(tokio::sync::Notify::new()),
            block_dropped: Arc::new(AtomicBool::new(false)),
            first_started: Arc::new(tokio::sync::Notify::new()),
            release_first: Arc::new(tokio::sync::Notify::new()),
            queued_started: Arc::new(AtomicUsize::new(0)),
        });
        let client = comet_rpc::memory_client(service.clone());
        let (task, commands, events) = spawn_with_client(client);
        (task, commands, events, service)
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

    /// Drains the client hello before handing back the transport: every
    /// `RpcClient` now writes it as frame zero, and callers here assert on
    /// exact frame counts/positions for the requests that follow it.
    async fn controllable_transport() -> (RpcClient, mpsc::Sender<String>, mpsc::Receiver<String>) {
        let (out, mut outbound) = mpsc::channel(1);
        let control = out.clone();
        let (inbound_sender, inbound) = mpsc::channel(1);
        tokio::spawn(async move {
            let _inbound_sender = inbound_sender;
            futures::future::pending::<()>().await;
        });
        let client = RpcClient::new(out, inbound);
        outbound.recv().await.expect("client hello");
        (client, control, outbound)
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
        // Capacity 5, not 4: the client hello now occupies the first slot, so
        // 5 is what exactly holds it plus the four startup watch requests —
        // leaving the transcript subscription below as the one that blocks.
        let (task, commands, mut events) = spawn_with_client(backpressured_client(5, 0));
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
    async fn closing_an_active_request_cancels_server_work_and_promotes_the_next_request() {
        let (task, commands, mut events, service) = spawn_cancellation_service();
        wait_online(&mut events).await;

        let (reply, received) = tokio::sync::oneshot::channel();
        commands
            .send(SupervisorCommand::Request {
                method: "Block",
                params: serde_json::Value::Null,
                reply,
            })
            .unwrap();
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            service.block_started.notified(),
        )
        .await
        .expect("active request never reached the server");
        drop(received);

        let (reply, next) = tokio::sync::oneshot::channel();
        commands
            .send(SupervisorCommand::Request {
                method: "Next",
                params: serde_json::Value::Null,
                reply,
            })
            .unwrap();

        tokio::time::timeout(std::time::Duration::from_millis(100), async {
            while !service.block_dropped.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("closing the federation request did not cancel server work");
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_millis(100), next)
                .await
                .expect("next request remained behind the cancelled request")
                .expect("next request reply sender closed")
                .expect("next request failed"),
            serde_json::json!(true)
        );

        commands.send(SupervisorCommand::Shutdown).unwrap();
        assert!(matches!(task.await, Ok(Ok(ConnectedExit::Shutdown))));
    }

    #[tokio::test]
    async fn active_request_cancels_and_promotes_while_transcript_replacement_waits() {
        let service = Arc::new(CancellationService {
            block_started: Arc::new(tokio::sync::Notify::new()),
            block_dropped: Arc::new(AtomicBool::new(false)),
            first_started: Arc::new(tokio::sync::Notify::new()),
            release_first: Arc::new(tokio::sync::Notify::new()),
            queued_started: Arc::new(AtomicUsize::new(0)),
        });
        let client = Arc::new(comet_rpc::memory_client(service.clone()));
        let (active_reply, active_receiver) = tokio::sync::oneshot::channel();
        let mut active_call = Some(start_call(
            client.clone(),
            GenericCall {
                method: "Block",
                params: serde_json::Value::Null,
                reply: Some(active_reply),
            },
        ));
        let (next_reply, next_receiver) = tokio::sync::oneshot::channel();
        let mut queued_calls = VecDeque::from([GenericCall {
            method: "Next",
            params: serde_json::Value::Null,
            reply: Some(next_reply),
        }]);
        let (command_tx, mut command_rx) = mpsc::unbounded_channel();
        let (events, _event_rx) = mpsc::unbounded_channel();
        let mut selected_chat = Some("chat-1".into());
        let server_id = hello().server_id;
        let replacement_first_polled = Arc::new(tokio::sync::Notify::new());
        let replacement_polled = Arc::new(AtomicBool::new(false));

        tokio::time::timeout(std::time::Duration::from_millis(100), async {
            tokio::select! {
                _ = active_call_next(&mut active_call) => panic!("blocking active request completed"),
                _ = service.block_started.notified() => {}
            }
        })
        .await
        .expect("active request never reached the server");
        assert!(
            !service.block_dropped.load(Ordering::SeqCst),
            "server work ended before the transcript replacement began"
        );

        let replacement_first_polled_for_task = replacement_first_polled.clone();
        let replacement_polled_for_task = replacement_polled.clone();
        let replacement = tokio::spawn(async move {
            let mut context = ConnectedTranscriptReplacement {
                client,
                server_id: &server_id,
                commands: &mut command_rx,
                selected_chat: &mut selected_chat,
                events: &events,
                active_call: &mut active_call,
                queued_calls: &mut queued_calls,
            };
            wait_for_transcript_replacement(
                FirstPollPending {
                    first_polled: replacement_first_polled_for_task,
                    polled: replacement_polled_for_task,
                },
                &mut context,
            )
            .await
        });
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            replacement_first_polled.notified(),
        )
        .await
        .expect("transcript replacement did not begin waiting");
        assert!(
            replacement_polled.load(Ordering::SeqCst),
            "transcript replacement was not pending before the federation receiver closed"
        );
        assert!(
            !service.block_dropped.load(Ordering::SeqCst),
            "server work ended before the federation receiver closed"
        );
        drop(active_receiver);

        tokio::time::timeout(std::time::Duration::from_millis(100), async {
            while !service.block_dropped.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("closed active request survived the stalled transcript replacement");
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_millis(100), next_receiver)
                .await
                .expect("queued request waited for the transcript replacement")
                .expect("next request reply sender closed")
                .expect("next request failed"),
            serde_json::json!(true)
        );

        command_tx.send(SupervisorCommand::Shutdown).unwrap();
        assert!(matches!(
            replacement.await,
            Ok(Ok(Phase::Exit(ConnectedExit::Shutdown)))
        ));
    }

    #[tokio::test]
    async fn closing_a_queued_request_discards_it_before_starting_the_next_request() {
        let (task, commands, mut events, service) = spawn_cancellation_service();
        wait_online(&mut events).await;

        let (reply, first) = tokio::sync::oneshot::channel();
        commands
            .send(SupervisorCommand::Request {
                method: "First",
                params: serde_json::Value::Null,
                reply,
            })
            .unwrap();
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            service.first_started.notified(),
        )
        .await
        .expect("first request never reached the server");

        let (reply, queued) = tokio::sync::oneshot::channel();
        commands
            .send(SupervisorCommand::Request {
                method: "Queued",
                params: serde_json::Value::Null,
                reply,
            })
            .unwrap();
        drop(queued);

        let (reply, next) = tokio::sync::oneshot::channel();
        commands
            .send(SupervisorCommand::Request {
                method: "Next",
                params: serde_json::Value::Null,
                reply,
            })
            .unwrap();
        service.release_first.notify_one();

        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_millis(100), next)
                .await
                .expect("next request remained behind a closed queued request")
                .expect("next request reply sender closed")
                .expect("next request failed"),
            serde_json::json!(true)
        );
        assert_eq!(
            service.queued_started.load(Ordering::SeqCst),
            0,
            "closed queued request still reached the server"
        );
        assert_eq!(first.await.unwrap().unwrap(), serde_json::json!(true));

        commands.send(SupervisorCommand::Shutdown).unwrap();
        assert!(matches!(task.await, Ok(Ok(ConnectedExit::Shutdown))));
    }

    #[tokio::test]
    async fn closed_queued_requests_do_not_consume_admission_capacity() {
        let (task, commands, mut events, service) = spawn_cancellation_service();
        wait_online(&mut events).await;

        let (reply, first) = tokio::sync::oneshot::channel();
        commands
            .send(SupervisorCommand::Request {
                method: "First",
                params: serde_json::Value::Null,
                reply,
            })
            .unwrap();
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            service.first_started.notified(),
        )
        .await
        .expect("first request never reached the server");

        let mut canceled_receivers = Vec::new();
        for _ in 0..MAX_QUEUED_CALLS {
            let (reply, dropped) = tokio::sync::oneshot::channel();
            commands
                .send(SupervisorCommand::Request {
                    method: "Queued",
                    params: serde_json::Value::Null,
                    reply,
                })
                .unwrap();
            canceled_receivers.push(dropped);
        }
        let (acknowledged, ack) = tokio::sync::oneshot::channel();
        commands
            .send(SupervisorCommand::WatchTranscript {
                chat_id: None,
                acknowledged: Some(acknowledged),
            })
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_millis(100), ack)
            .await
            .expect("canceled requests were not admitted before the fence")
            .expect("transcript fence acknowledgment was dropped");
        drop(canceled_receivers);
        let (reply, next) = tokio::sync::oneshot::channel();
        commands
            .send(SupervisorCommand::Request {
                method: "Next",
                params: serde_json::Value::Null,
                reply,
            })
            .unwrap();
        let (acknowledged, next_admitted) = tokio::sync::oneshot::channel();
        commands
            .send(SupervisorCommand::WatchTranscript {
                chat_id: None,
                acknowledged: Some(acknowledged),
            })
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_millis(100), next_admitted)
            .await
            .expect("live request was not admitted before the release fence")
            .expect("live-request admission fence acknowledgment was dropped");
        service.release_first.notify_one();

        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_millis(100), next)
                .await
                .expect("live request was not admitted after canceled queued requests")
                .expect("live request reply sender closed")
                .expect("live request was rejected despite canceled queue entries"),
            serde_json::json!(true)
        );
        assert_eq!(
            service.queued_started.load(Ordering::SeqCst),
            0,
            "closed queued request reached the server"
        );
        assert_eq!(first.await.unwrap().unwrap(), serde_json::json!(true));

        commands.send(SupervisorCommand::Shutdown).unwrap();
        assert!(matches!(task.await, Ok(Ok(ConnectedExit::Shutdown))));
    }

    #[tokio::test]
    async fn promoting_queued_requests_skips_a_closed_receiver_before_its_rpc_starts() {
        let (client, _transport, mut outbound) = controllable_transport().await;
        let (reply, closed) = tokio::sync::oneshot::channel();
        drop(closed);
        let (next_reply, _next) = tokio::sync::oneshot::channel();
        let mut queued = VecDeque::from([
            GenericCall {
                method: "Queued",
                params: serde_json::Value::Null,
                reply: Some(reply),
            },
            GenericCall {
                method: "Next",
                params: serde_json::Value::Null,
                reply: Some(next_reply),
            },
        ]);

        let mut active = Some(
            promote_queued_call(Arc::new(client), &mut queued)
                .expect("next live queued request was not promoted"),
        );
        let task = tokio::spawn(async move { active_call_next(&mut active).await });
        let frame = tokio::time::timeout(std::time::Duration::from_millis(100), outbound.recv())
            .await
            .expect("promoted request did not reach the transport")
            .expect("transport closed before the promoted request");
        let frame: serde_json::Value = serde_json::from_str(&frame).expect("RPC frame is JSON");
        assert_eq!(frame["method"], "Next");
        task.abort();
        let _ = task.await;
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
        let (client, transport, mut outbound) = controllable_transport().await;
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
        let (client, transport, mut outbound) = controllable_transport().await;
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
        let (client, transport, mut outbound) = controllable_transport().await;
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
