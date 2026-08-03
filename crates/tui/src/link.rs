//! TUI bridge to the shared client-side federation.
//!
//! Daemon attachment remains TUI-owned so detaching never stops the trusted
//! local engine. Once attached, all resource streams and calls are owned by
//! [`comet_client::Federation`], including the local server.

use std::time::Duration;

use comet_client::{Federation, FederationCommand, FederationEvent};
use comet_doc::SessionMessageEntry;
use comet_proto::view::ConnectionStatus;
use comet_proto::{AuthState, ServerId, ServerRef};
use comet_rpc::methods;
use tokio::sync::mpsc;

use crate::daemon::{Attachment, DaemonConfig};

#[derive(Debug)]
pub enum Update {
    Connection(ConnectionStatus),
    Attached(Attachment),
    Federation(FederationEvent),
    // Retained for the existing auth gate until the cloud-removal task.
    Auth(Box<AuthState>),
    Devices(Vec<comet_proto::Device>),
    Spaces(Vec<comet_proto::Space>),
    Chats(Vec<comet_proto::Chat>),
    Sessions(Vec<comet_proto::Session>),
    Transcript {
        chat_id: String,
        entries: Vec<SessionMessageEntry>,
    },
    LocalDevice(String),
    Models(Vec<comet_proto::Model>),
    FederatedModels {
        server_id: ServerId,
        request_id: String,
        models: Vec<comet_proto::Model>,
    },
    Refs(Vec<comet_proto::RepoRef>),
    FederatedRefs {
        server_id: ServerId,
        request_id: String,
        refs: Vec<comet_proto::RepoRef>,
    },
    SessionStarted {
        chat_id: String,
    },
    FederatedSessionStarted {
        chat: ServerRef,
        request_id: String,
    },
    Notice(String),
    SendFailed {
        chat_id: String,
        message_id: String,
        error: String,
    },
    FederatedSendFailed {
        chat: ServerRef,
        message_id: String,
        error: String,
    },
    FederatedRequestFailed {
        server_id: ServerId,
        request_id: String,
        message: String,
    },
    FederatedStartFailed {
        chat: ServerRef,
        request_id: String,
        message_id: String,
        error: String,
    },
}

impl From<FederationEvent> for Update {
    fn from(event: FederationEvent) -> Self {
        Self::Federation(event)
    }
}

#[derive(Debug)]
pub enum Command {
    WatchTranscript(Option<ServerRef>),
    Call {
        server_id: ServerId,
        method: &'static str,
        params: serde_json::Value,
        context: &'static str,
    },
    Send {
        server_id: ServerId,
        chat_id: String,
        message_id: String,
        params: serde_json::Value,
    },
    ListModels {
        server_id: ServerId,
        request_id: String,
        harness: comet_proto::HarnessId,
    },
    ListRefs {
        server_id: ServerId,
        request_id: String,
        repo_path: String,
    },
    StartSession(Box<StartSession>),
    Reconnect(ServerId),
    Shutdown,
}

#[derive(Debug)]
pub struct StartSession {
    pub server_id: ServerId,
    pub request_id: String,
    pub chat_id: String,
    pub space_id: String,
    pub repo_path: String,
    pub plan: comet_proto::view::CheckoutPlan,
    pub config: Option<serde_json::Value>,
    pub message_id: String,
    pub command: serde_json::Value,
}

pub struct EngineLink {
    pub updates: mpsc::UnboundedReceiver<Update>,
    pub commands: mpsc::UnboundedSender<Command>,
    supervisor: tokio::task::JoinHandle<()>,
}

impl EngineLink {
    pub fn send(&self, command: Command) {
        let _ = self.commands.send(command);
    }
}

impl Drop for EngineLink {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Shutdown);
        self.supervisor.abort();
    }
}

const BACKOFF_MS: [u64; 6] = [200, 400, 800, 1_600, 3_200, 5_000];

pub fn spawn(config: DaemonConfig) -> EngineLink {
    let (update_tx, updates) = mpsc::unbounded_channel();
    let (commands, command_rx) = mpsc::unbounded_channel();
    let supervisor = tokio::spawn(supervise(config, update_tx, command_rx));
    EngineLink {
        updates,
        commands,
        supervisor,
    }
}

async fn supervise(
    config: DaemonConfig,
    updates: mpsc::UnboundedSender<Update>,
    mut commands: mpsc::UnboundedReceiver<Command>,
) {
    let mut attempt = 0usize;
    loop {
        if updates
            .send(Update::Connection(ConnectionStatus::Connecting))
            .is_err()
        {
            return;
        }
        match crate::daemon::connect(&config).await {
            Ok(connection) => {
                attempt = 0;
                let _ = updates.send(Update::Attached(connection.attachment));
                match Federation::new(connection.client, &config.data_dir).await {
                    Ok(mut federation) => {
                        let _ = updates.send(Update::Connection(ConnectionStatus::Ready));
                        if run_federation(&mut federation, &updates, &mut commands).await {
                            return;
                        }
                        let _ = updates.send(Update::Connection(ConnectionStatus::Failed(
                            "engine connection lost".into(),
                        )));
                    }
                    Err(error) => {
                        let _ = updates.send(Update::Connection(ConnectionStatus::Failed(
                            format!("{error:#}"),
                        )));
                    }
                }
            }
            Err(error) => {
                if updates
                    .send(Update::Connection(ConnectionStatus::Failed(format!(
                        "{error:#}"
                    ))))
                    .is_err()
                {
                    return;
                }
            }
        }
        let delay = Duration::from_millis(BACKOFF_MS[attempt.min(BACKOFF_MS.len() - 1)]);
        attempt = attempt.saturating_add(1);
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            command = commands.recv() => match command {
                None | Some(Command::Shutdown) => return,
                Some(Command::Reconnect(_)) => attempt = 0,
                Some(_) => {}
            }
        }
    }
}

/// Returns true only when the viewport intentionally shuts down.
async fn run_federation(
    federation: &mut Federation,
    updates: &mpsc::UnboundedSender<Update>,
    commands: &mut mpsc::UnboundedReceiver<Command>,
) -> bool {
    loop {
        tokio::select! {
            biased;
            command = commands.recv() => match command {
                None | Some(Command::Shutdown) => {
                    let _ = federation.send(FederationCommand::Shutdown);
                    return true;
                }
                Some(command) => forward(command, federation, updates),
            },
            event = federation.recv() => match event {
                Some(event) => if updates.send(Update::Federation(event)).is_err() { return true; },
                None => return false,
            }
        }
    }
}

fn forward(command: Command, federation: &Federation, updates: &mpsc::UnboundedSender<Update>) {
    let send = |command| {
        let _ = federation.send(command);
    };
    match command {
        Command::WatchTranscript(chat) => send(FederationCommand::WatchTranscript(chat)),
        Command::Call {
            server_id,
            method,
            params,
            ..
        } => {
            send(FederationCommand::Call {
                server_id,
                method,
                params,
            });
        }
        Command::Send {
            server_id,
            chat_id,
            message_id,
            params,
        } => {
            let commands = federation.command_sender();
            let updates = updates.clone();
            tokio::spawn(async move {
                if let Err(error) =
                    request(&commands, server_id.clone(), methods::QUEUE_COMMAND, params).await
                {
                    let _ = updates.send(Update::FederatedSendFailed {
                        chat: ServerRef::new(server_id, chat_id),
                        message_id,
                        error: error.to_string(),
                    });
                }
            });
        }
        Command::ListModels {
            server_id,
            request_id,
            harness,
        } => {
            let commands = federation.command_sender();
            let updates = updates.clone();
            tokio::spawn(async move {
                match request(
                    &commands,
                    server_id.clone(),
                    methods::LIST_MODELS,
                    serde_json::json!({ "harness": harness }),
                )
                .await
                .and_then(|value| {
                    serde_json::from_value(value)
                        .map_err(|error| comet_rpc::RpcError::Failed(error.to_string()))
                }) {
                    Ok(models) => {
                        let _ = updates.send(Update::FederatedModels {
                            server_id,
                            request_id,
                            models,
                        });
                    }
                    Err(error) => {
                        let _ = updates.send(Update::FederatedRequestFailed {
                            server_id,
                            request_id,
                            message: format!("Couldn't list models: {error}"),
                        });
                    }
                }
            });
        }
        Command::ListRefs {
            server_id,
            request_id,
            repo_path,
        } => {
            let commands = federation.command_sender();
            let updates = updates.clone();
            tokio::spawn(async move {
                match request(
                    &commands,
                    server_id.clone(),
                    methods::LIST_REFS,
                    serde_json::json!({ "repoPath": repo_path }),
                )
                .await
                .and_then(|value| {
                    serde_json::from_value(value)
                        .map_err(|error| comet_rpc::RpcError::Failed(error.to_string()))
                }) {
                    Ok(refs) => {
                        let _ = updates.send(Update::FederatedRefs {
                            server_id,
                            request_id,
                            refs,
                        });
                    }
                    Err(error) => {
                        let _ = updates.send(Update::FederatedRequestFailed {
                            server_id,
                            request_id,
                            message: format!("Couldn't list refs: {error}"),
                        });
                    }
                }
            });
        }
        Command::Reconnect(server_id) => send(FederationCommand::Reconnect(server_id)),
        Command::StartSession(start) => {
            let commands = federation.command_sender();
            let updates = updates.clone();
            let failed_chat = ServerRef::new(start.server_id.clone(), start.chat_id.clone());
            let failed_request = start.request_id.clone();
            let failed_message = start.message_id.clone();
            tokio::spawn(async move {
                if let Err(error) = start_session(&commands, &updates, *start).await {
                    let _ = updates.send(Update::FederatedStartFailed {
                        chat: failed_chat,
                        request_id: failed_request,
                        message_id: failed_message,
                        error: error.to_string(),
                    });
                }
            });
        }
        Command::Shutdown => {}
    }
}

async fn request(
    commands: &mpsc::UnboundedSender<FederationCommand>,
    server_id: ServerId,
    method: &'static str,
    params: serde_json::Value,
) -> Result<serde_json::Value, comet_rpc::RpcError> {
    let (reply, received) = tokio::sync::oneshot::channel();
    commands
        .send(FederationCommand::Request {
            server_id,
            method,
            params,
            reply,
        })
        .map_err(|_| comet_rpc::RpcError::Closed)?;
    received.await.unwrap_or(Err(comet_rpc::RpcError::Closed))
}

async fn start_session(
    commands: &mpsc::UnboundedSender<FederationCommand>,
    updates: &mpsc::UnboundedSender<Update>,
    start: StartSession,
) -> Result<(), comet_rpc::RpcError> {
    use comet_proto::view::CheckoutPlan;
    let (mut cwd, branch) = match &start.plan {
        CheckoutPlan::CurrentCheckout { branch } => (None, branch.clone()),
        CheckoutPlan::ReuseWorktree { path, branch } => (Some(path.clone()), Some(branch.clone())),
        CheckoutPlan::NewWorktree { base } => (None, base.clone()),
    };
    if let CheckoutPlan::NewWorktree { base: Some(base) } = &start.plan {
        let value = request(
            commands,
            start.server_id.clone(),
            methods::CREATE_WORKTREE,
            serde_json::json!({ "repoPath": start.repo_path, "branch": base }),
        )
        .await?;
        cwd = Some(
            serde_json::from_value::<comet_proto::Worktree>(value)
                .map_err(|error| comet_rpc::RpcError::Failed(error.to_string()))?
                .path,
        );
    }
    let mut mutate = serde_json::json!({ "op": "createChat", "chatId": start.chat_id, "spaceId": start.space_id });
    if let Some(object) = mutate.as_object_mut() {
        if let Some(cwd) = &cwd {
            object.insert("cwd".into(), serde_json::Value::String(cwd.clone()));
        }
        if let Some(branch) = &branch {
            object.insert("branch".into(), serde_json::Value::String(branch.clone()));
        }
        if let Some(config) = start.config {
            object.insert("config".into(), config);
        }
    }
    request(commands, start.server_id.clone(), methods::MUTATE, mutate).await?;
    let mut command = start.command;
    if let (Some(cwd), Some(request)) = (
        &cwd,
        command
            .get_mut("request")
            .and_then(|value| value.as_object_mut()),
    ) {
        request.insert("cwd".into(), serde_json::Value::String(cwd.clone()));
    }
    let queue = request(
        commands,
        start.server_id.clone(),
        methods::QUEUE_COMMAND,
        serde_json::json!({ "chatId": start.chat_id, "command": command }),
    )
    .await;
    match queue {
        Ok(_) => {
            let _ = updates.send(Update::FederatedSessionStarted {
                chat: ServerRef::new(start.server_id, start.chat_id),
                request_id: start.request_id,
            });
            Ok(())
        }
        Err(error) => Err(error),
    }
}

// Kept public through Update's transcript payload type documentation.
#[allow(dead_code)]
fn _typed_transcript(_: Vec<SessionMessageEntry>) {}

#[cfg(test)]
mod tests {
    use super::*;
    use comet_proto::view::CheckoutPlan;
    use std::sync::{Arc, Mutex};

    async fn run_start(
        plan: CheckoutPlan,
        fail_mutate: bool,
    ) -> (
        Result<(), comet_rpc::RpcError>,
        Vec<(&'static str, serde_json::Value)>,
        Vec<Update>,
    ) {
        let (commands, mut receiver) = mpsc::unbounded_channel();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let recorded = calls.clone();
        tokio::spawn(async move {
            while let Some(FederationCommand::Request {
                method,
                params,
                reply,
                ..
            }) = receiver.recv().await
            {
                recorded.lock().unwrap().push((method, params));
                let result = if method == methods::MUTATE && fail_mutate {
                    Err(comet_rpc::RpcError::Failed("mutate failed".into()))
                } else if method == methods::CREATE_WORKTREE {
                    Ok(
                        serde_json::json!({"repoPath":"/repo","path":"/worktree","branch":"feature","name":"feature","checkoutId":null}),
                    )
                } else {
                    Ok(serde_json::Value::Null)
                };
                let _ = reply.send(result);
            }
        });
        let (updates, mut update_rx) = mpsc::unbounded_channel();
        let result = start_session(
            &commands,
            &updates,
            StartSession {
                server_id: ServerId::new("server-b"),
                request_id: "request-1".into(),
                chat_id: "chat-1".into(),
                space_id: "space-1".into(),
                repo_path: "/repo".into(),
                plan,
                config: None,
                message_id: "message-1".into(),
                command: serde_json::json!({"request":{}}),
            },
        )
        .await;
        drop(commands);
        let mut emitted = Vec::new();
        while let Ok(update) = update_rx.try_recv() {
            emitted.push(update);
        }
        let calls = calls.lock().unwrap().clone();
        (result, calls, emitted)
    }

    #[tokio::test]
    async fn start_session_preserves_each_checkout_plan_and_call_order() {
        let (_, current, _) = run_start(
            CheckoutPlan::CurrentCheckout {
                branch: Some("main".into()),
            },
            false,
        )
        .await;
        assert_eq!(
            current.iter().map(|call| call.0).collect::<Vec<_>>(),
            [methods::MUTATE, methods::QUEUE_COMMAND]
        );
        assert_eq!(current[0].1["branch"], "main");

        let (_, reuse, _) = run_start(
            CheckoutPlan::ReuseWorktree {
                path: "/existing".into(),
                branch: "topic".into(),
            },
            false,
        )
        .await;
        assert_eq!(
            reuse.iter().map(|call| call.0).collect::<Vec<_>>(),
            [methods::MUTATE, methods::QUEUE_COMMAND]
        );
        assert_eq!(reuse[0].1["cwd"], "/existing");
        assert_eq!(reuse[0].1["branch"], "topic");

        let (_, created, _) = run_start(
            CheckoutPlan::NewWorktree {
                base: Some("main".into()),
            },
            false,
        )
        .await;
        assert_eq!(
            created.iter().map(|call| call.0).collect::<Vec<_>>(),
            [
                methods::CREATE_WORKTREE,
                methods::MUTATE,
                methods::QUEUE_COMMAND
            ]
        );
        assert_eq!(created[1].1["cwd"], "/worktree");
    }

    #[tokio::test]
    async fn failed_create_chat_never_queues_or_emits_started() {
        let (result, calls, updates) =
            run_start(CheckoutPlan::CurrentCheckout { branch: None }, true).await;
        assert!(result.is_err());
        assert_eq!(
            calls.iter().map(|call| call.0).collect::<Vec<_>>(),
            [methods::MUTATE]
        );
        assert!(
            !updates
                .iter()
                .any(|update| matches!(update, Update::SessionStarted { .. }))
        );
    }
}
