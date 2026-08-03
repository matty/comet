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
    Refs(Vec<comet_proto::RepoRef>),
    SessionStarted {
        chat_id: String,
    },
    Notice(String),
    SendFailed {
        chat_id: String,
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
        harness: comet_proto::HarnessId,
    },
    ListRefs {
        server_id: ServerId,
        repo_path: String,
    },
    StartSession(Box<StartSession>),
    Reconnect(ServerId),
    Shutdown,
}

#[derive(Debug)]
pub struct StartSession {
    pub server_id: ServerId,
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
            server_id, params, ..
        } => {
            send(FederationCommand::Call {
                server_id,
                method: methods::QUEUE_COMMAND,
                params,
            });
        }
        Command::ListModels { server_id, harness } => send(FederationCommand::Call {
            server_id,
            method: methods::LIST_MODELS,
            params: serde_json::json!({ "harness": harness }),
        }),
        Command::ListRefs {
            server_id,
            repo_path,
        } => send(FederationCommand::Call {
            server_id,
            method: methods::LIST_REFS,
            params: serde_json::json!({ "repoPath": repo_path }),
        }),
        Command::Reconnect(server_id) => send(FederationCommand::Reconnect(server_id)),
        Command::StartSession(start) => {
            // Calls remain explicitly server-qualified. Worktree creation that
            // needs a returned path is left to a later richer federation RPC;
            // current-checkout and existing-worktree drafts preserve behavior.
            let chat = ServerRef::new(start.server_id.clone(), start.chat_id.clone());
            let mut params = serde_json::json!({
                "op": "createChat", "chatId": start.chat_id, "spaceId": start.space_id,
            });
            if let Some(config) = start.config
                && let Some(object) = params.as_object_mut()
            {
                object.insert("config".into(), config);
            }
            send(FederationCommand::Call {
                server_id: start.server_id.clone(),
                method: methods::MUTATE,
                params,
            });
            send(FederationCommand::Call {
                server_id: start.server_id,
                method: methods::QUEUE_COMMAND,
                params: serde_json::json!({ "chatId": start.chat_id, "command": start.command }),
            });
            let _ = updates.send(Update::SessionStarted {
                chat_id: chat.local_id,
            });
        }
        Command::Shutdown => {}
    }
}

// Kept public through Update's transcript payload type documentation.
#[allow(dead_code)]
fn _typed_transcript(_: Vec<SessionMessageEntry>) {}
