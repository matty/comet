use std::sync::Arc;

use comet_doc::SessionMessageEntry;
use comet_proto::{Chat, Device, RemoteConnectionState, ServerHello, ServerRef, Session, Space};
use comet_rpc::{RpcClient, RpcError, methods};
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
    transcript: &mut Option<(ServerRef, mpsc::UnboundedReceiver<serde_json::Value>)>,
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
) -> Result<(ServerRef, mpsc::UnboundedReceiver<serde_json::Value>), RpcError> {
    let chat = ServerRef::new(server_id.clone(), chat_id);
    let receiver = client
        .subscribe(
            methods::WATCH_DOC_MESSAGES,
            serde_json::json!({"chatId": chat_id}),
        )
        .await?;
    Ok((chat, receiver))
}

pub(crate) async fn supervise_connected(
    client: Arc<RpcClient>,
    hello: ServerHello,
    display_name: String,
    events: mpsc::UnboundedSender<FederationEvent>,
    commands: &mut mpsc::UnboundedReceiver<SupervisorCommand>,
    selected_chat: &mut Option<String>,
) -> Result<ConnectedExit, RpcError> {
    let mut devices = client
        .subscribe(methods::WATCH_DEVICES, serde_json::Value::Null)
        .await?;
    let mut spaces = client
        .subscribe(methods::WATCH_SPACES, serde_json::Value::Null)
        .await?;
    let mut chats = client
        .subscribe(methods::WATCH_CHATS, serde_json::Value::Null)
        .await?;
    let mut sessions = client
        .subscribe(methods::WATCH_SESSIONS, serde_json::Value::Null)
        .await?;
    let mut transcript = match selected_chat.as_deref() {
        Some(chat_id) => Some(subscribe_transcript(&client, &hello.server_id, chat_id).await?),
        None => None,
    };

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
            command = commands.recv() => match command {
                Some(SupervisorCommand::Call(method, params)) => {
                    if let Err(error) = client.call(method, params).await {
                        let _ = events.send(FederationEvent::Notice {
                            server_id: hello.server_id.clone(),
                            message: error.to_string(),
                        });
                    }
                    continue;
                }
                Some(SupervisorCommand::WatchTranscript { chat_id, acknowledged }) => {
                    if let Some((old, _)) = transcript.take() {
                        let _ = events.send(FederationEvent::Transcript { chat: old, entries: Vec::new() });
                    }
                    *selected_chat = chat_id;
                    transcript = match selected_chat.as_deref() {
                        Some(chat_id) => Some(subscribe_transcript(&client, &hello.server_id, chat_id).await?),
                        None => None,
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
