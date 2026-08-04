use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;
use futures::StreamExt;
use serde::Deserialize;

use comet_rpc::{RpcError, RpcReply, RpcService, methods, parse_params};

use crate::EngineRpc;

pub fn remote_method_allowed(method: &str) -> bool {
    matches!(
        method,
        methods::SERVER_HELLO
            | methods::LOCAL_DEVICE
            | methods::LIST_HARNESSES
            | methods::LIST_MODELS
            | methods::QUEUE_COMMAND
            | methods::WATCH_DOC_MESSAGES
            | methods::WATCH_CHATS
            | methods::WATCH_DEVICES
            | methods::WATCH_SPACES
            | methods::WATCH_SESSIONS
            | methods::MUTATE
            | methods::LIST_REPOS
            | methods::ADD_REPO
            | methods::CLONE_REPO
            | methods::CREATE_REPO
            | methods::LIST_BRANCHES
            | methods::LIST_REFS
            | methods::SWITCH_REF
            | methods::LIST_FOLDERS
            | methods::SEARCH_FILES
            | methods::CREATE_WORKTREE
            | methods::DELETE_WORKTREE
            | methods::OPEN_TERMINAL
            | methods::SUBSCRIBE_TERMINAL
            | methods::WRITE_TERMINAL
            | methods::RESIZE_TERMINAL
            | methods::CLOSE_TERMINAL
            | methods::WATCH_CHECKOUT_DIFFS
            | methods::LIST_AGENT_ACCOUNTS
            | methods::ACTIVATE_AGENT_ACCOUNT
            | methods::UPLOAD_CHUNK
            | methods::UPLOAD_COMMIT
            | methods::READ_ATTACHMENT_CHUNK
    )
}

pub struct RemoteRpcService {
    inner: Arc<EngineRpc>,
    local_device_id: String,
    terminal_chats: Arc<Mutex<HashMap<String, String>>>,
}

impl RemoteRpcService {
    pub fn new(inner: Arc<EngineRpc>, local_device_id: impl Into<String>) -> Self {
        Self {
            inner,
            local_device_id: local_device_id.into(),
            terminal_chats: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn terminals(&self) -> MutexGuard<'_, HashMap<String, String>> {
        self.terminal_chats
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn require_local_chat(&self, chat_id: &str) -> Result<(), RpcError> {
        if self.inner.owns_remote_chat(chat_id, &self.local_device_id) {
            Ok(())
        } else {
            Err(RpcError::Failed(format!(
                "chat {chat_id} is not owned by this server"
            )))
        }
    }

    fn require_remote_terminal(&self, terminal_id: &str) -> Result<(), RpcError> {
        let chat_id = self.terminals().get(terminal_id).cloned().ok_or_else(|| {
            RpcError::Failed(format!(
                "terminal {terminal_id} is not owned by this server"
            ))
        })?;
        self.require_local_chat(&chat_id)
    }

    fn filter_stream(
        reply: RpcReply,
        field: &'static str,
        wanted: String,
    ) -> Result<RpcReply, RpcError> {
        let RpcReply::Stream(stream) = reply else {
            return Err(RpcError::Failed("expected stream response".into()));
        };
        Ok(RpcReply::Stream(
            stream
                .filter_map(move |mut value| {
                    let valid = value.as_array_mut().is_some_and(|rows| {
                        rows.retain(|row| {
                            row.get(field).and_then(|v| v.as_str()) == Some(wanted.as_str())
                        });
                        true
                    });
                    futures::future::ready(valid.then_some(value))
                })
                .boxed(),
        ))
    }

    fn guard_chat_stream(
        reply: RpcReply,
        inner: Arc<EngineRpc>,
        local_device_id: String,
        chat_id: String,
    ) -> Result<RpcReply, RpcError> {
        let RpcReply::Stream(stream) = reply else {
            return Err(RpcError::Failed("expected stream response".into()));
        };
        Ok(RpcReply::Stream(
            stream
                .take_while(move |_| {
                    futures::future::ready(inner.owns_remote_chat(&chat_id, &local_device_id))
                })
                .boxed(),
        ))
    }

    fn guard_terminal_stream(
        reply: RpcReply,
        inner: Arc<EngineRpc>,
        local_device_id: String,
        terminal_chats: Arc<Mutex<HashMap<String, String>>>,
        terminal_id: String,
        chat_id: String,
    ) -> Result<RpcReply, RpcError> {
        let RpcReply::Stream(stream) = reply else {
            return Err(RpcError::Failed("expected stream response".into()));
        };
        Ok(RpcReply::Stream(
            stream
                .take_while(move |_| {
                    let mapped = terminal_chats
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .get(&terminal_id)
                        == Some(&chat_id);
                    futures::future::ready(
                        mapped && inner.owns_remote_chat(&chat_id, &local_device_id),
                    )
                })
                .boxed(),
        ))
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChatParams {
    chat_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TerminalParams {
    terminal_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AttachmentParams {
    chat_id: String,
    path: String,
    #[serde(default)]
    offset: u64,
}

#[async_trait]
impl RpcService for RemoteRpcService {
    async fn handle(&self, method: &str, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        if !remote_method_allowed(method) {
            return Err(RpcError::UnknownMethod(method.to_string()));
        }
        if let Some(target) = params
            .get("targetDeviceId")
            .and_then(|value| value.as_str())
            && target != self.local_device_id
        {
            return Err(RpcError::BadParams(format!(
                "targetDeviceId must match {}",
                self.local_device_id
            )));
        }

        let mut opened_chat = None;
        let mut watched_chat = None;
        let mut terminal_id = None;
        let mut terminal_chat = None;
        match method {
            methods::QUEUE_COMMAND | methods::WATCH_DOC_MESSAGES => {
                let parsed: ChatParams = parse_params(params.clone())?;
                self.require_local_chat(&parsed.chat_id)?;
                if method == methods::WATCH_DOC_MESSAGES {
                    watched_chat = Some(parsed.chat_id);
                }
            }
            methods::OPEN_TERMINAL => {
                let parsed: ChatParams = parse_params(params.clone())?;
                self.require_local_chat(&parsed.chat_id)?;
                opened_chat = Some(parsed.chat_id);
            }
            methods::SUBSCRIBE_TERMINAL
            | methods::WRITE_TERMINAL
            | methods::RESIZE_TERMINAL
            | methods::CLOSE_TERMINAL => {
                let parsed: TerminalParams = parse_params(params.clone())?;
                self.require_remote_terminal(&parsed.terminal_id)?;
                terminal_chat = self.terminals().get(&parsed.terminal_id).cloned();
                terminal_id = Some(parsed.terminal_id);
            }
            methods::READ_ATTACHMENT_CHUNK => {
                let parsed: AttachmentParams = parse_params(params.clone())?;
                return self.inner.read_remote_attachment(
                    &parsed.chat_id,
                    &parsed.path,
                    parsed.offset,
                    &self.local_device_id,
                );
            }
            _ => {}
        }

        if method == methods::MUTATE {
            return self
                .inner
                .handle_remote_mutation(params, &self.local_device_id);
        }
        let reply = self.inner.handle(method, params).await?;

        match method {
            methods::WATCH_DOC_MESSAGES => Self::guard_chat_stream(
                reply,
                self.inner.clone(),
                self.local_device_id.clone(),
                watched_chat.expect("chat id parsed before delegation"),
            ),
            methods::SUBSCRIBE_TERMINAL => Self::guard_terminal_stream(
                reply,
                self.inner.clone(),
                self.local_device_id.clone(),
                self.terminal_chats.clone(),
                terminal_id.expect("terminal id parsed before delegation"),
                terminal_chat.expect("terminal ownership checked before delegation"),
            ),
            methods::WATCH_DEVICES => {
                Self::filter_stream(reply, "id", self.local_device_id.clone())
            }
            methods::WATCH_CHATS
            | methods::WATCH_SPACES
            | methods::WATCH_SESSIONS
            | methods::WATCH_CHECKOUT_DIFFS => {
                Self::filter_stream(reply, "deviceId", self.local_device_id.clone())
            }
            methods::OPEN_TERMINAL => {
                if let RpcReply::Value(value) = &reply
                    && let Some(terminal_id) = value.get("id").and_then(|value| value.as_str())
                    && let Some(chat_id) = opened_chat
                {
                    self.terminals().insert(terminal_id.to_string(), chat_id);
                }
                Ok(reply)
            }
            methods::CLOSE_TERMINAL => {
                if let Some(terminal_id) = terminal_id {
                    self.terminals().remove(&terminal_id);
                }
                Ok(reply)
            }
            _ => Ok(reply),
        }
    }
}

#[cfg(test)]
mod tests {
    use futures::{StreamExt, stream};

    use super::*;

    #[tokio::test]
    async fn unexpected_watch_shape_is_not_forwarded() {
        let reply = RpcReply::Stream(
            stream::once(async { serde_json::json!({"secret":"not an array"}) }).boxed(),
        );
        let RpcReply::Stream(mut filtered) =
            RemoteRpcService::filter_stream(reply, "deviceId", "device-b".into()).unwrap()
        else {
            panic!("expected stream");
        };
        assert!(filtered.next().await.is_none());
    }
}
