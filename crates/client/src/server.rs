use std::sync::Arc;

use comet_doc::SessionMessageEntry;
use comet_proto::{Chat, Device, RemoteConnectionState, ServerHello, ServerRef, Session, Space};
use comet_rpc::{RpcClient, RpcError, methods};
use tokio::sync::{mpsc, watch};

use crate::{FederationEvent, ServerState};

pub(crate) enum SupervisorCommand {
    Call(&'static str, serde_json::Value),
    WatchTranscript(Option<String>),
    Reconnect,
    Shutdown,
}

enum ResourceUpdate {
    Devices(Vec<Device>),
    Spaces(Vec<Space>),
    Chats(Vec<Chat>),
    Sessions(Vec<Session>),
}

async fn subscribe_resource<T>(
    client: Arc<RpcClient>,
    method: &'static str,
    updates: mpsc::UnboundedSender<ResourceUpdate>,
    map: fn(Vec<T>) -> ResourceUpdate,
) -> Result<(), RpcError>
where
    T: serde::de::DeserializeOwned + Send + 'static,
{
    let mut stream = client.subscribe(method, serde_json::Value::Null).await?;
    tokio::spawn(async move {
        while let Some(value) = stream.recv().await {
            match serde_json::from_value::<Vec<T>>(value) {
                Ok(value) => {
                    if updates.send(map(value)).is_err() {
                        return;
                    }
                }
                Err(error) => {
                    tracing::warn!(%method, %error, "federation: invalid resource snapshot");
                    return;
                }
            }
        }
    });
    Ok(())
}

pub(crate) async fn supervise_connected(
    client: RpcClient,
    hello: ServerHello,
    display_name: String,
    events: mpsc::UnboundedSender<FederationEvent>,
    commands: &mut mpsc::UnboundedReceiver<SupervisorCommand>,
) -> Result<(), RpcError> {
    let client = Arc::new(client);
    let (updates_tx, mut updates_rx) = mpsc::unbounded_channel();
    subscribe_resource(
        client.clone(),
        methods::WATCH_DEVICES,
        updates_tx.clone(),
        ResourceUpdate::Devices,
    )
    .await?;
    subscribe_resource(
        client.clone(),
        methods::WATCH_SPACES,
        updates_tx.clone(),
        ResourceUpdate::Spaces,
    )
    .await?;
    subscribe_resource(
        client.clone(),
        methods::WATCH_CHATS,
        updates_tx.clone(),
        ResourceUpdate::Chats,
    )
    .await?;
    subscribe_resource(
        client.clone(),
        methods::WATCH_SESSIONS,
        updates_tx,
        ResourceUpdate::Sessions,
    )
    .await?;

    let mut state = ServerState::empty(
        hello.server_id.clone(),
        display_name,
        RemoteConnectionState::Online,
    );
    let _ = events.send(FederationEvent::ServerChanged(state.clone()));
    let (transcript_tx, transcript_rx) = watch::channel::<Option<ServerRef>>(None);

    loop {
        tokio::select! {
            update = updates_rx.recv() => match update {
                Some(ResourceUpdate::Devices(value)) => state.devices = value,
                Some(ResourceUpdate::Spaces(value)) => state.spaces = value,
                Some(ResourceUpdate::Chats(value)) => state.chats = value,
                Some(ResourceUpdate::Sessions(value)) => state.sessions = value,
                None => return Err(RpcError::Closed),
            },
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
                Some(SupervisorCommand::WatchTranscript(chat_id)) => {
                    let chat = chat_id.map(|id| ServerRef::new(hello.server_id.clone(), id));
                    transcript_tx.send_replace(chat.clone());
                    if let Some(chat) = chat {
                        let client = client.clone();
                        let events = events.clone();
                        let mut selection = transcript_rx.clone();
                        tokio::spawn(async move {
                            let params = serde_json::json!({"chatId": chat.local_id()});
                            let Ok(mut stream) = client.subscribe(methods::WATCH_DOC_MESSAGES, params).await else { return; };
                            loop {
                                tokio::select! {
                                    value = stream.recv() => match value {
                                        Some(value) => match serde_json::from_value::<Vec<SessionMessageEntry>>(value) {
                                            Ok(entries) => { let _ = events.send(FederationEvent::Transcript { chat: chat.clone(), entries }); }
                                            Err(_) => return,
                                        },
                                        None => return,
                                    },
                                    changed = selection.changed() => {
                                        if changed.is_err() || selection.borrow().as_ref() != Some(&chat) { return; }
                                    }
                                }
                            }
                        });
                    }
                    continue;
                }
                Some(SupervisorCommand::Reconnect) => return Err(RpcError::Closed),
                Some(SupervisorCommand::Shutdown) | None => return Ok(()),
            }
        }
        let _ = events.send(FederationEvent::ServerChanged(state.clone()));
    }
}
