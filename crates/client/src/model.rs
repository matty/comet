use std::collections::HashMap;

use comet_doc::SessionMessageEntry;
use comet_proto::{Chat, Device, RemoteConnectionState, ServerId, ServerRef, Session, Space};

pub type FederationStream = tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>;

#[derive(Debug, Clone, PartialEq)]
pub struct ServerState {
    pub id: ServerId,
    pub name: String,
    pub connection: RemoteConnectionState,
    pub devices: Vec<Device>,
    pub spaces: Vec<Space>,
    pub chats: Vec<Chat>,
    pub sessions: Vec<Session>,
}

impl ServerState {
    pub fn empty(id: ServerId, name: impl Into<String>, connection: RemoteConnectionState) -> Self {
        Self {
            id,
            name: name.into(),
            connection,
            devices: Vec::new(),
            spaces: Vec::new(),
            chats: Vec::new(),
            sessions: Vec::new(),
        }
    }

    pub fn offline(id: ServerId, name: impl Into<String>) -> Self {
        Self::empty(id, name, RemoteConnectionState::Offline)
    }

    pub fn chat_ref(&self, local_id: &str) -> Option<ServerRef> {
        self.chats
            .iter()
            .any(|chat| chat.id == local_id)
            .then(|| ServerRef::new(self.id.clone(), local_id))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum FederationEvent {
    ServerChanged(ServerState),
    ServerRemoved(ServerId),
    Transcript {
        chat: ServerRef,
        entries: Vec<SessionMessageEntry>,
    },
    Notice {
        server_id: ServerId,
        message: String,
    },
}

pub enum FederationCommand {
    Call {
        server_id: ServerId,
        method: &'static str,
        params: serde_json::Value,
    },
    Request {
        server_id: ServerId,
        method: &'static str,
        params: serde_json::Value,
        reply: tokio::sync::oneshot::Sender<Result<serde_json::Value, comet_rpc::RpcError>>,
    },
    Subscribe {
        server_id: ServerId,
        method: &'static str,
        params: serde_json::Value,
        reply: tokio::sync::oneshot::Sender<Result<FederationStream, comet_rpc::RpcError>>,
    },
    WatchTranscript(Option<ServerRef>),
    Reconnect(ServerId),
    Shutdown,
}

impl std::fmt::Debug for FederationCommand {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Call {
                server_id,
                method,
                params,
            } => formatter
                .debug_struct("Call")
                .field("server_id", server_id)
                .field("method", method)
                .field("params", params)
                .finish(),
            Self::Request {
                server_id,
                method,
                params,
                ..
            } => formatter
                .debug_struct("Request")
                .field("server_id", server_id)
                .field("method", method)
                .field("params", params)
                .finish_non_exhaustive(),
            Self::Subscribe {
                server_id,
                method,
                params,
                ..
            } => formatter
                .debug_struct("Subscribe")
                .field("server_id", server_id)
                .field("method", method)
                .field("params", params)
                .finish_non_exhaustive(),
            Self::WatchTranscript(chat) => formatter
                .debug_tuple("WatchTranscript")
                .field(chat)
                .finish(),
            Self::Reconnect(server_id) => {
                formatter.debug_tuple("Reconnect").field(server_id).finish()
            }
            Self::Shutdown => formatter.write_str("Shutdown"),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ServerSnapshot {
    servers: HashMap<ServerId, ServerState>,
    order: Vec<ServerId>,
}

impl ServerSnapshot {
    pub fn apply(&mut self, event: FederationEvent) {
        match event {
            FederationEvent::ServerChanged(server) => {
                if !self.servers.contains_key(&server.id) {
                    self.order.push(server.id.clone());
                }
                self.servers.insert(server.id.clone(), server);
            }
            FederationEvent::ServerRemoved(server_id) => {
                self.servers.remove(&server_id);
                self.order.retain(|id| id != &server_id);
            }
            FederationEvent::Transcript { .. } | FederationEvent::Notice { .. } => {}
        }
    }

    pub fn server(&self, id: &ServerId) -> Option<&ServerState> {
        self.servers.get(id)
    }

    pub fn servers(&self) -> impl Iterator<Item = &ServerState> {
        self.order.iter().filter_map(|id| self.servers.get(id))
    }

    pub fn chat_ref(&self, server_id: &ServerId, local_id: &str) -> Option<ServerRef> {
        self.server(server_id)?.chat_ref(local_id)
    }
}
