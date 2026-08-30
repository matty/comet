//! EngineRpc — the engine-side `RpcService`: sessions + docs + the workspace-doc
//! entity surface.
//!
//! Methods (feature-inventory §2):
//! - `ListHarnesses` → `[HarnessDescriptor]`
//! - `ListModels {harness, force}` → `{models, source}`
//! - `QueueCommand {chatId, command}` → `{commandId}` (durable doc command)
//! - `WatchDocMessages {chatId}` → stream of joined `SessionMessageEntry[]`,
//!   re-emitted on every doc change
//! - `WatchChats` / `WatchDevices` → streams of the workspace doc's entity rows
//! - `WatchSessions` → stream of `Session[]`: this engine's live statuses merged with
//!   remote devices' workspace session rows
//! - `Mutate {op, …}` → `{ok}` — workspace entity mutations (createChat, renameChat,
//!   setChatArchived, deleteChat, renameDevice, markChatSeen)
//! - `LocalDevice` → `{deviceId}` — this engine's identity (never forwarded)
//! - Repos (§3.5): `ListRepos`, `AddRepo {path}`, `CloneRepo {url}`,
//!   `CreateRepo {name}`, `ListBranches {repoPath}` (default branch first),
//!   `ListFolders {path?}`, `CreateWorktree {repoPath, branch}`, `DeleteWorktree
//!   {repoPath, worktreePath}`; `WatchCheckoutDiffs` → stream of `CheckoutDiff[]`
//! - Terminals (§3.4): `OpenTerminal {chatId, cols, rows}` → `TerminalSession`,
//!   `SubscribeTerminal {terminalId, afterSeq?}` → stream of `TerminalEvent`
//!   (replay then live tail), `WriteTerminal {terminalId, data}`, `ResizeTerminal`,
//!   `CloseTerminal`. M5 is single-user local: per-user owner checks land with
//!   real multi-account auth in M6.
//! - Agent accounts (§3.7): `ListAgentAccounts {forceUsage?}` →
//!   `AgentAccountsSnapshot`, `ActivateAgentAccount`/`ForgetAgentAccount`
//!   `{harness, accountId}` → snapshot, `StartAgentLogin {harness}` →
//!   `{loginId, url, mode}`, `CompleteAgentLogin {loginId, code}` → snapshot,
//!   `PollAgentLogin {loginId}`, `CancelAgentLogin {loginId}`.
//! - Uploads (§3.7): `UploadChunk {uploadId, data, seq?}`,
//!   `UploadCommit {uploadId, fileName}` → `{path}`,
//!   `ReadAttachmentChunk {path, offset}` → `{name, mimeType, data, nextOffset,
//!   done}` (path-jailed to the uploads dir + workspace-known chat cwds).
//!
use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use tokio::sync::watch;

use comet_doc::{MessagePart, SessionCommandPayload};
use comet_proto::{
    ChatConfig, DiagnosticSeverity, HarnessId, LanSettings, ReadToolDiffReply,
    RemoteConnectionState, RemoteEntry, ServerHello, ServerId, ToolCall,
};
use comet_rpc::{RpcError, RpcReply, RpcService, methods, parse_params};

use crate::agent_accounts::AgentAccounts;
use crate::diff_sync::CheckoutDiffSync;
use crate::doc_host::{DocHost, PurgeCleanupOutcome, PurgeFinishOutcome, PurgeToken};
use crate::registry::HarnessRegistry;
use crate::repos::{Repos, home_dir};
use crate::sessions::SessionsEngine;
use crate::terminals::Terminals;
use crate::uploads::Uploads;
use crate::workspace_host::{DocMutationGate, WorkspaceHost};
use crate::{LanServerHandle, RemoteConfigStore};

const FILE_SEARCH_RPC_TIMEOUT: Duration = Duration::from_secs(6);
const FILE_SEARCH_FEATURED_PATHS: usize = 32;
const PURGE_FINAL_RETRIES: usize = 3;
const PURGE_FINAL_RETRY_DELAY: Duration = Duration::from_millis(25);

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChatParams {
    chat_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListModelsParams {
    harness: HarnessId,
    /// Set by the picker's Retry row. A new FIELD on an existing method, so
    /// it stays additive and needs no version bump of its own — an older
    /// peer simply never asks for a refresh.
    #[serde(default)]
    force: bool,
}

/// The `/` menu's parameters. `cwd` is required and has no default: the answer
/// is directory-scoped, and a missing directory silently answering for some
/// other one is the failure this method exists to avoid.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListCommandsParams {
    harness: HarnessId,
    cwd: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct QueueCommandParams {
    chat_id: String,
    command: SessionCommandPayload,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepoPathParams {
    /// `repoPath` per §3.5 (the §2.1 shorthand `repo` is accepted as an alias).
    #[serde(alias = "repo")]
    repo_path: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SwitchRefParams {
    /// The checkout to switch — a session's cwd (main folder or worktree).
    repo_path: String,
    ref_name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateWorktreeParams {
    #[serde(alias = "repo")]
    repo_path: String,
    branch: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeleteWorktreeParams {
    #[serde(alias = "repo")]
    repo_path: String,
    #[serde(alias = "path")]
    worktree_path: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListFoldersParams {
    #[serde(default)]
    path: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileSearchParams {
    query: String,
    #[serde(default)]
    chat_id: Option<String>,
    #[serde(default)]
    space_id: Option<String>,
    /// Existing linked worktree selected for a new chat. The engine accepts it
    /// only after verifying it against the space repository's worktree list.
    #[serde(default)]
    path: Option<String>,
}

fn tool_file_path(call: &ToolCall) -> Option<&str> {
    match call {
        ToolCall::ReadFile { path }
        | ToolCall::WriteFile { path, .. }
        | ToolCall::EditFile { path, .. } => Some(path),
        ToolCall::ApplyPatch { path } | ToolCall::Search { path, .. } => path.as_deref(),
        ToolCall::Exec { .. }
        | ToolCall::Glob { .. }
        | ToolCall::WebFetch { .. }
        | ToolCall::WebSearch { .. }
        | ToolCall::Todo { .. }
        | ToolCall::Mcp { .. }
        | ToolCall::Unknown { .. } => None,
    }
}

/// Couple the workspace row and its local persistence-admission lifecycle
/// without widening the RPC protocol. A tokened admission remains owned across
/// the injected create and is claimed only after it succeeds; the optional
/// branch write then runs against the live row.
fn create_chat_with_lifecycle<Create, SetBranch>(
    doc_host: &DocHost,
    chat_id: &str,
    workspace_row_exists: bool,
    create: Create,
    set_branch: SetBranch,
) -> Result<(), crate::EngineError>
where
    Create: FnOnce() -> Result<(), crate::EngineError>,
    SetBranch: FnOnce() -> Result<(), crate::EngineError>,
{
    let admission = doc_host
        .admit_create(chat_id, workspace_row_exists)
        .map_err(|()| crate::EngineError::ChatCleanupPendingRetry)?;
    create()?;
    if !doc_host.revive_created_chat(chat_id, admission) {
        return Err(crate::EngineError::Other(
            "chat lifecycle changed while creating it".into(),
        ));
    }
    set_branch()
}

/// Complete the durable pass before acknowledging a delete. Store diagnostics
/// remain in `DocHost` tracing; this typed outcome tells the mutation caller
/// that the background finalizer still owes a retry.
fn initial_purge_cleanup(
    doc_host: &DocHost,
    chat_purges: &[(String, PurgeToken)],
) -> Result<(), crate::EngineError> {
    let mut retry_needed = false;
    for (chat_id, token) in chat_purges {
        match doc_host.cleanup_purging_chat(chat_id, *token) {
            Some(PurgeCleanupOutcome::Cleared) => {}
            Some(PurgeCleanupOutcome::PendingRetry) => retry_needed = true,
            None => {
                tracing::warn!(chat = %chat_id, "initial cleanup skipped by a stale purge token");
                retry_needed = true;
            }
        }
    }
    if retry_needed {
        Err(crate::EngineError::ChatCleanupPendingRetry)
    } else {
        Ok(())
    }
}

async fn run_bounded_purge_retries<Finish>(
    mut finish: Finish,
    retry_delay: Duration,
    max_retries: usize,
) -> PurgeFinishOutcome
where
    Finish: FnMut() -> PurgeFinishOutcome,
{
    let mut outcome = finish();
    for _ in 0..max_retries {
        if outcome != PurgeFinishOutcome::PendingRetry {
            break;
        }
        if !retry_delay.is_zero() {
            tokio::time::sleep(retry_delay).await;
        }
        outcome = finish();
    }
    outcome
}

async fn finish_purge_with_retries(doc_host: DocHost, chat_id: String, token: PurgeToken) {
    match run_bounded_purge_retries(
        || doc_host.finish_purge(&chat_id, token),
        PURGE_FINAL_RETRY_DELAY,
        PURGE_FINAL_RETRIES,
    )
    .await
    {
        PurgeFinishOutcome::Purged => {}
        PurgeFinishOutcome::PendingRetry => {
            tracing::warn!(chat = %chat_id, "chat cleanup retries exhausted");
        }
        PurgeFinishOutcome::Stale => {
            tracing::debug!(chat = %chat_id, "chat cleanup retry skipped by a stale purge token");
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OpenTerminalParams {
    chat_id: String,
    cols: u16,
    rows: u16,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TerminalIdParams {
    terminal_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubscribeTerminalParams {
    terminal_id: String,
    #[serde(default)]
    after_seq: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WriteTerminalParams {
    terminal_id: String,
    /// Base64 input bytes (plain UTF-8 accepted leniently).
    data: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResizeTerminalParams {
    terminal_id: String,
    cols: u16,
    rows: u16,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListAgentAccountsParams {
    #[serde(default)]
    force_usage: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentAccountParams {
    harness: HarnessId,
    account_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StartAgentLoginParams {
    harness: HarnessId,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LoginIdParams {
    login_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CompleteAgentLoginParams {
    login_id: String,
    code: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UploadChunkParams {
    upload_id: String,
    /// Base64 payload chunk.
    data: String,
    #[serde(default)]
    seq: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UploadCommitParams {
    upload_id: String,
    file_name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReadAttachmentChunkParams {
    path: String,
    #[serde(default)]
    offset: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReadToolDiffParams {
    chat_id: String,
    part_id: String,
    diff_ref: String,
}

/// The Mutate surface (feature-inventory §2 DataRpc), tagged by `op`.
#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase")]
enum MutateParams {
    #[serde(rename_all = "camelCase")]
    CreateChat {
        chat_id: String,
        /// The space the chat is created in — fixes host device + base cwd.
        space_id: String,
        #[serde(default)]
        config: Option<ChatConfig>,
        /// The picked ref, named on the row from the first frame (the footer
        /// read "Select ref" until the diff reconciler stamped it).
        #[serde(default)]
        branch: Option<String>,
        /// Cwd override (isolated-worktree path); default = the space's folder.
        #[serde(default)]
        cwd: Option<String>,
    },
    /// Create a space (device + folder pair). Idempotent by id; a live
    /// duplicate `(deviceId, path)` no-ops. `gitDetected` is seeded from the
    /// picker's FolderEntry — the owning device's SpacesSync re-verifies.
    #[serde(rename_all = "camelCase")]
    CreateSpace {
        space_id: String,
        device_id: String,
        path: String,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        git_detected: bool,
    },
    /// LWW display-name set; `name: None` clears back to basename(path).
    #[serde(rename_all = "camelCase")]
    RenameSpace {
        space_id: String,
        #[serde(default)]
        name: Option<String>,
    },
    /// Hard delete: cascades to every chat (and session row) in the space.
    /// Live runs hosted here are interrupted best-effort.
    #[serde(rename_all = "camelCase")]
    DeleteSpace { space_id: String },
    #[serde(rename_all = "camelCase")]
    RenameChat { chat_id: String, title: String },
    /// Set the chat's checkout branch label — the sidebar's
    /// "project · branch" sub-line.
    #[serde(rename_all = "camelCase")]
    SetChatBranch { chat_id: String, branch: String },
    /// Retarget a chat onto another folder — mid-session switch to an
    /// EXISTING worktree (the picked ref's checkout). Next run starts a
    /// fresh harness conversation there (resume is cwd-scoped).
    #[serde(rename_all = "camelCase")]
    SetChatCwd { chat_id: String, cwd: String },
    /// Backdate a chat's activity timestamps (epoch ms) — the sidebar's
    /// relative-time column. Used by tooling/seeds; the doc fold sets these on
    /// real message traffic.
    #[serde(rename_all = "camelCase")]
    SetChatActivity {
        chat_id: String,
        #[serde(default)]
        last_message_at: Option<i64>,
        #[serde(default)]
        created_at: Option<i64>,
    },
    /// Re-home a chat to another device (tooling/seeds; device migration later).
    #[serde(rename_all = "camelCase")]
    SetChatHost { chat_id: String, device_id: String },
    #[serde(rename_all = "camelCase")]
    SetChatArchived { chat_id: String, archived: bool },
    /// Full-config replace on the chat row (comet `SetChatConfig`): the
    /// composer's mid-session model / reasoning / options changes, LWW-synced
    /// so they survive restarts and reach every device.
    #[serde(rename_all = "camelCase")]
    SetChatConfig { chat_id: String, config: ChatConfig },
    /// Tombstone: removes the chats-map row; the session doc remains.
    #[serde(rename_all = "camelCase")]
    DeleteChat { chat_id: String },
    #[serde(rename_all = "camelCase")]
    RenameDevice { device_id: String, name: String },
    /// Synced seen marker (LWW + monotonic guard): clears the "completed"
    /// badge on every device. `at` is epoch ms; default = now.
    #[serde(rename_all = "camelCase")]
    MarkChatSeen {
        chat_id: String,
        #[serde(default)]
        at: Option<i64>,
    },
}

pub struct EngineRpc {
    sessions: SessionsEngine,
    doc_host: DocHost,
    workspace: WorkspaceHost,
    registry: std::sync::Arc<HarnessRegistry>,
    repos: Repos,
    terminals: Terminals,
    diff_sync: CheckoutDiffSync,
    uploads: Uploads,
    agent_accounts: AgentAccounts,
    updater: Option<comet_update::Updater>,
    server_hello: Option<ServerHello>,
    mutation_authority: DocMutationGate,
    presence: std::sync::Arc<crate::Presence>,
    /// When this installation's identity was last rebuilt from a zero-byte
    /// `device-id`, if ever (D96) — reported on `LocalDevice`.
    identity_rebuilt_at: Option<String>,
}

#[derive(Clone)]
pub struct LocalRpcService {
    inner: std::sync::Arc<EngineRpc>,
    store: RemoteConfigStore,
    lan: LanServerHandle,
}

impl EngineRpc {
    #[allow(clippy::too_many_arguments)] // engine assembly seam, not a public API
    pub fn new(
        sessions: SessionsEngine,
        doc_host: DocHost,
        workspace: WorkspaceHost,
        registry: std::sync::Arc<HarnessRegistry>,
        repos: Repos,
        terminals: Terminals,
        diff_sync: CheckoutDiffSync,
        uploads: Uploads,
        agent_accounts: AgentAccounts,
        presence: std::sync::Arc<crate::Presence>,
    ) -> Self {
        let mutation_authority = workspace.mutation_gate();
        Self {
            sessions,
            doc_host,
            workspace,
            registry,
            repos,
            terminals,
            diff_sync,
            uploads,
            agent_accounts,
            updater: None,
            server_hello: None,
            mutation_authority,
            presence,
            identity_rebuilt_at: None,
        }
    }

    /// Attach the identity-rebuilt stamp read at assembly (D96).
    ///
    /// A builder rather than a `new` argument for the same reason
    /// [`Self::with_updater`] is one: this is boot-time context the RPC layer
    /// reports and never owns, and `new` already carries ten arguments.
    pub fn with_identity_rebuilt_at(mut self, stamp: Option<String>) -> Self {
        self.identity_rebuilt_at = stamp;
        self
    }

    /// Attach the release checker (UpdateStatus stream + ApplyUpdate).
    pub fn with_updater(mut self, updater: comet_update::Updater) -> Self {
        self.updater = Some(updater);
        self
    }

    pub fn with_server_hello(mut self, hello: ServerHello) -> Self {
        self.server_hello = Some(hello);
        self
    }

    fn server_hello(&self) -> Result<&ServerHello, RpcError> {
        self.server_hello
            .as_ref()
            .ok_or_else(|| RpcError::Failed("server identity unavailable".into()))
    }

    #[cfg(test)]
    pub(crate) fn shares_mutation_authority(&self, other: &Self) -> bool {
        self.mutation_authority.ptr_eq(&other.mutation_authority)
    }

    fn updater(&self) -> Result<&comet_update::Updater, RpcError> {
        self.updater
            .as_ref()
            .ok_or_else(|| RpcError::Failed("updates unavailable".into()))
    }

    /// Resolve a mention-search root from synced workspace rows. A client may
    /// name an existing linked worktree for a new chat, but it is verified
    /// against the space repository before any filesystem walk begins.
    async fn file_search_root(&self, p: &FileSearchParams) -> Result<std::path::PathBuf, RpcError> {
        let local_device = self.doc_host.device_id();
        match (&p.chat_id, &p.space_id) {
            (Some(_), Some(_)) | (None, None) => Err(RpcError::BadParams(
                "SearchFiles needs exactly one of chatId or spaceId".into(),
            )),
            (Some(chat_id), None) => {
                if p.path.is_some() {
                    return Err(RpcError::BadParams(
                        "SearchFiles path applies only to a space".into(),
                    ));
                }
                let chat = self
                    .workspace
                    .doc()
                    .chat(chat_id)
                    .map_err(|e| RpcError::Failed(e.to_string()))?
                    .ok_or_else(|| RpcError::Failed("chat not found".into()))?;
                if chat.device_id != local_device {
                    return Err(RpcError::Failed("chat belongs to another device".into()));
                }
                let cwd = chat
                    .cwd
                    .map(std::path::PathBuf::from)
                    .ok_or_else(|| RpcError::Failed("chat has no workspace folder".into()))?;
                let space_id = chat
                    .space_id
                    .ok_or_else(|| RpcError::Failed("chat has no workspace space".into()))?;
                let space = self
                    .workspace
                    .doc()
                    .space(&space_id)
                    .map_err(|e| RpcError::Failed(e.to_string()))?
                    .ok_or_else(|| RpcError::Failed("chat workspace space not found".into()))?;
                if space.device_id != local_device {
                    return Err(RpcError::Failed(
                        "chat space belongs to another device".into(),
                    ));
                }
                if let Some(cwd) = self
                    .repos
                    .workspace_checkout(std::path::Path::new(&space.path), &cwd)
                    .await
                {
                    Ok(cwd)
                } else {
                    Err(RpcError::Failed(
                        "chat folder is not a workspace checkout".into(),
                    ))
                }
            }
            (None, Some(space_id)) => {
                let space = self
                    .workspace
                    .doc()
                    .space(space_id)
                    .map_err(|e| RpcError::Failed(e.to_string()))?
                    .ok_or_else(|| RpcError::Failed("space not found".into()))?;
                if space.device_id != local_device {
                    return Err(RpcError::Failed("space belongs to another device".into()));
                }
                let space_path = std::path::PathBuf::from(&space.path);
                let requested = p
                    .path
                    .as_deref()
                    .map_or_else(|| space_path.clone(), std::path::PathBuf::from);
                if let Some(requested) =
                    self.repos.workspace_checkout(&space_path, &requested).await
                {
                    Ok(requested)
                } else {
                    Err(RpcError::BadParams(
                        "SearchFiles path is not a workspace checkout".into(),
                    ))
                }
            }
        }
    }

    /// Most-recent-first paths the current chat actually touched, followed by
    /// files still changed in its checkout. The search worker validates and
    /// normalizes them against the resolved root before using them as ranking
    /// hints, so stale or out-of-workspace tool paths simply disappear.
    fn featured_file_paths(&self, chat_id: &str) -> Vec<String> {
        let mut paths = Vec::new();
        let mut seen = HashSet::new();
        if let Ok(handle) = self.doc_host.open(chat_id)
            && let Ok(entries) = handle.doc().read_entries()
        {
            for entry in entries.into_iter().rev() {
                for part in entry.parts.into_iter().rev() {
                    if let MessagePart::Tool { call, .. } = part
                        && let Some(path) = tool_file_path(&call)
                        && !path.trim().is_empty()
                        && seen.insert(path.to_string())
                    {
                        paths.push(path.to_string());
                        if paths.len() == FILE_SEARCH_FEATURED_PATHS {
                            break;
                        }
                    }
                }
                if paths.len() == FILE_SEARCH_FEATURED_PATHS {
                    break;
                }
            }
        }

        if let Ok(Some(chat)) = self.workspace.doc().chat(chat_id) {
            let diffs = self.diff_sync.watch_diffs().borrow().clone();
            let diff = chat
                .checkout_id
                .as_deref()
                .and_then(|id| diffs.iter().find(|diff| diff.checkout_id == id))
                .or_else(|| {
                    chat.cwd
                        .as_deref()
                        .and_then(|cwd| diffs.iter().find(|diff| diff.cwd == cwd))
                });
            if let Some(diff) = diff {
                for file in &diff.files {
                    if paths.len() == FILE_SEARCH_FEATURED_PATHS {
                        break;
                    }
                    if seen.insert(file.path.clone()) {
                        paths.push(file.path.clone());
                    }
                }
            }
        }
        paths
    }

    fn mutate(&self, params: MutateParams) -> Result<(), RpcError> {
        let failed = |e: crate::EngineError| RpcError::Failed(e.to_string());
        match params {
            MutateParams::CreateChat {
                chat_id,
                space_id,
                config,
                branch,
                cwd,
            } => {
                let workspace_row_exists = self
                    .workspace
                    .doc()
                    .chat(&chat_id)
                    .map_err(crate::EngineError::from)
                    .map_err(failed)?
                    .is_some();
                create_chat_with_lifecycle(
                    &self.doc_host,
                    &chat_id,
                    workspace_row_exists,
                    || {
                        self.workspace
                            .create_chat(&chat_id, &space_id, config, cwd)
                            .map(drop)
                    },
                    || match branch.as_deref().filter(|branch| !branch.is_empty()) {
                        Some(branch) => self.workspace.set_chat_branch(&chat_id, branch).map(drop),
                        None => Ok(()),
                    },
                )
                .map_err(failed)
            }
            MutateParams::CreateSpace {
                space_id,
                device_id,
                path,
                name,
                git_detected,
            } => self
                .workspace
                .create_space(&space_id, &device_id, &path, name, git_detected)
                .map_err(failed),
            MutateParams::RenameSpace { space_id, name } => self
                .workspace
                .rename_space(&space_id, name.as_deref())
                .map_err(failed)
                .map(drop),
            MutateParams::DeleteSpace { space_id } => {
                // Read and mark the complete cascade while the shared mutation
                // authority still holds the workspace rows. That closes the
                // crash window between the durable tombstone and admission.
                let pre_delete_ids: Vec<String> = self
                    .workspace
                    .doc()
                    .read_chats()
                    .map_err(crate::EngineError::from)
                    .map_err(failed)?
                    .into_iter()
                    .filter(|chat| chat.space_id.as_deref() == Some(&space_id))
                    .map(|chat| chat.id)
                    .collect();
                let mut pre_delete_tokens: HashMap<String, PurgeToken> = HashMap::new();
                for chat_id in &pre_delete_ids {
                    let Some(token) = self.doc_host.begin_purge(chat_id) else {
                        for (marked_chat_id, marked_token) in pre_delete_tokens {
                            self.doc_host.cancel_purge(&marked_chat_id, marked_token);
                        }
                        return Err(failed(crate::EngineError::ChatCleanupPendingRetry));
                    };
                    pre_delete_tokens.insert(chat_id.clone(), token);
                }
                let deleted = match self.workspace.delete_space(&space_id) {
                    Ok(deleted) => deleted,
                    Err(err) => {
                        for (chat_id, token) in pre_delete_tokens {
                            self.doc_host.cancel_purge(&chat_id, token);
                        }
                        return Err(failed(err));
                    }
                };

                // The mutation authority makes an exact match normal. Reconcile
                // defensively if a concurrent document import changed the set:
                // never leave a returned id unmarked, and reopen any id the
                // cascade did not actually remove.
                let expected: HashSet<_> = pre_delete_ids.into_iter().collect();
                let returned: HashSet<_> = deleted.chat_ids.iter().cloned().collect();
                let mut reconciliation_needs_retry = expected != returned;
                if reconciliation_needs_retry {
                    tracing::warn!(space = %space_id, "deleteSpace chat set changed while tombstoning");
                }
                let mut chat_purges = Vec::with_capacity(deleted.chat_ids.len());
                for chat_id in deleted.chat_ids {
                    match pre_delete_tokens.remove(&chat_id) {
                        Some(token) => chat_purges.push((chat_id, token)),
                        None => match self.doc_host.begin_purge(&chat_id) {
                            Some(token) => chat_purges.push((chat_id, token)),
                            None => {
                                tracing::warn!(chat = %chat_id, "deleteSpace could not own a returned purge lifecycle");
                                reconciliation_needs_retry = true;
                            }
                        },
                    }
                }
                for (chat_id, token) in pre_delete_tokens {
                    self.doc_host.cancel_purge(&chat_id, token);
                }
                let initial_cleanup = initial_purge_cleanup(&self.doc_host, &chat_purges);
                let sessions = self.sessions.clone();
                let doc_host = self.doc_host.clone();
                tokio::spawn(async move {
                    for (chat_id, token) in chat_purges {
                        if let Err(err) = sessions.interrupt(&chat_id).await {
                            tracing::debug!(chat = %chat_id, error = %err, "deleteSpace interrupt skipped");
                        }
                        finish_purge_with_retries(doc_host.clone(), chat_id, token).await;
                    }
                });
                if reconciliation_needs_retry {
                    Err(failed(crate::EngineError::ChatCleanupPendingRetry))
                } else {
                    initial_cleanup.map_err(failed)
                }
            }
            MutateParams::RenameChat { chat_id, title } => self
                .workspace
                .rename_chat(&chat_id, &title)
                .map_err(failed)
                .map(drop),
            MutateParams::SetChatBranch { chat_id, branch } => self
                .workspace
                .set_chat_branch(&chat_id, &branch)
                .map_err(failed)
                .map(drop),
            MutateParams::SetChatCwd { chat_id, cwd } => self
                .workspace
                .set_chat_cwd(&chat_id, &cwd)
                .map_err(failed)
                .map(drop),
            MutateParams::SetChatActivity {
                chat_id,
                last_message_at,
                created_at,
            } => self
                .workspace
                .set_chat_activity(&chat_id, last_message_at, created_at)
                .map_err(failed)
                .map(drop),
            MutateParams::SetChatHost { chat_id, device_id } => self
                .workspace
                .set_chat_host(&chat_id, &device_id)
                .map_err(failed)
                .map(drop),
            MutateParams::SetChatArchived { chat_id, archived } => self
                .workspace
                .set_chat_archived(&chat_id, archived)
                .map_err(failed)
                .map(drop),
            MutateParams::SetChatConfig { chat_id, config } => self
                .workspace
                .set_chat_config(&chat_id, &config)
                .map_err(failed)
                .map(drop),
            MutateParams::DeleteChat { chat_id } => {
                let Some(token) = self.doc_host.begin_purge(&chat_id) else {
                    return Err(failed(crate::EngineError::ChatCleanupPendingRetry));
                };
                if let Err(err) = self.workspace.delete_chat(&chat_id) {
                    self.doc_host.cancel_purge(&chat_id, token);
                    return Err(failed(err));
                }
                let initial_cleanup =
                    initial_purge_cleanup(&self.doc_host, &[(chat_id.clone(), token)]);
                let sessions = self.sessions.clone();
                let doc_host = self.doc_host.clone();
                tokio::spawn(async move {
                    if let Err(err) = sessions.interrupt(&chat_id).await {
                        tracing::debug!(chat = %chat_id, error = %err, "deleteChat interrupt skipped");
                    }
                    finish_purge_with_retries(doc_host, chat_id, token).await;
                });
                initial_cleanup.map_err(failed)
            }
            MutateParams::RenameDevice { device_id, name } => self
                .workspace
                .rename_device(&device_id, &name)
                .map_err(failed)
                .map(drop),
            MutateParams::MarkChatSeen { chat_id, at } => {
                let at = at
                    .and_then(chrono::DateTime::<chrono::Utc>::from_timestamp_millis)
                    .unwrap_or_else(chrono::Utc::now);
                self.workspace
                    .mark_chat_seen(&chat_id, at)
                    .map_err(failed)
                    .map(drop)
            }
        }
    }

    pub(crate) fn owns_remote_chat(&self, chat_id: &str, local_device_id: &str) -> bool {
        matches!(
            self.workspace.doc().chat(chat_id),
            Ok(Some(chat)) if chat.device_id == local_device_id
        )
    }

    pub(crate) fn read_remote_attachment(
        &self,
        chat_id: &str,
        path: &str,
        offset: u64,
        local_device_id: &str,
    ) -> Result<RpcReply, RpcError> {
        self.mutation_authority.run(|| {
            let chat = self
                .workspace
                .doc()
                .chat(chat_id)
                .map_err(|error| RpcError::Failed(error.to_string()))?
                .filter(|chat| chat.device_id == local_device_id)
                .ok_or_else(|| {
                    RpcError::Failed(format!("chat {chat_id} is not owned by this server"))
                })?;
            let roots: Vec<std::path::PathBuf> = chat.cwd.into_iter().map(Into::into).collect();
            let chunk = self
                .uploads
                .read_chunk(path, offset, &roots)
                .map_err(|error| RpcError::Failed(error.to_string()))?;
            RpcReply::value(&chunk)
        })
    }

    fn owns_remote_space(&self, space_id: &str, local_device_id: &str) -> bool {
        matches!(
            self.workspace.doc().space(space_id),
            Ok(Some(space)) if space.device_id == local_device_id
        )
    }

    fn validate_remote_mutation(
        &self,
        params: serde_json::Value,
        local_device_id: &str,
    ) -> Result<serde_json::Value, RpcError> {
        let mutation: MutateParams = parse_params(params.clone())?;
        let ownership_error = |kind: &str, id: &str| {
            RpcError::Failed(format!("{kind} {id} is not owned by this server"))
        };
        match &mutation {
            MutateParams::CreateSpace {
                space_id,
                device_id,
                ..
            } => {
                if device_id != local_device_id {
                    return Err(RpcError::BadParams(format!(
                        "deviceId must match {local_device_id}"
                    )));
                }
                if matches!(self.workspace.doc().space(space_id), Ok(Some(space)) if space.device_id != local_device_id)
                {
                    return Err(ownership_error("space", space_id));
                }
            }
            MutateParams::CreateChat {
                chat_id, space_id, ..
            } => {
                if !self.owns_remote_space(space_id, local_device_id) {
                    return Err(ownership_error("space", space_id));
                }
                if matches!(self.workspace.doc().chat(chat_id), Ok(Some(chat)) if chat.device_id != local_device_id)
                {
                    return Err(ownership_error("chat", chat_id));
                }
            }
            MutateParams::RenameSpace { space_id, .. } | MutateParams::DeleteSpace { space_id } => {
                if !self.owns_remote_space(space_id, local_device_id) {
                    return Err(ownership_error("space", space_id));
                }
            }
            MutateParams::RenameChat { chat_id, .. }
            | MutateParams::SetChatBranch { chat_id, .. }
            | MutateParams::SetChatCwd { chat_id, .. }
            | MutateParams::SetChatActivity { chat_id, .. }
            | MutateParams::SetChatArchived { chat_id, .. }
            | MutateParams::SetChatConfig { chat_id, .. }
            | MutateParams::DeleteChat { chat_id }
            | MutateParams::MarkChatSeen { chat_id, .. } => {
                if !self.owns_remote_chat(chat_id, local_device_id) {
                    return Err(ownership_error("chat", chat_id));
                }
            }
            MutateParams::SetChatHost { .. } => {
                return Err(RpcError::BadParams(
                    "setChatHost is not allowed over LAN".into(),
                ));
            }
            MutateParams::RenameDevice { .. } => {
                return Err(RpcError::BadParams(
                    "renameDevice is not allowed over LAN".into(),
                ));
            }
        }
        Ok(params)
    }

    pub(crate) fn handle_remote_mutation(
        &self,
        params: serde_json::Value,
        local_device_id: &str,
    ) -> Result<RpcReply, RpcError> {
        self.mutation_authority.run(|| {
            let params = self.validate_remote_mutation(params, local_device_id)?;
            let mutation: MutateParams = parse_params(params)?;
            self.mutate(mutation)?;
            RpcReply::value(&serde_json::json!({ "ok": true }))
        })
    }

    async fn checkout_file_diff_text_with_after_read<AfterRead, AfterReadFuture>(
        &self,
        request: comet_proto::GetCheckoutFileDiffTextRequest,
        after_read: AfterRead,
    ) -> Result<comet_proto::CheckoutFileDiffText, RpcError>
    where
        AfterRead: FnOnce() -> AfterReadFuture + Send,
        AfterReadFuture: std::future::Future<Output = ()> + Send,
    {
        let identity = Box::pin(
            self.repos
                .checkout_identity(std::path::Path::new(&request.cwd)),
        )
        .await
        .map_err(|error| RpcError::Failed(error.to_string()))?;
        if identity.id != request.checkout_id {
            return Err(RpcError::Failed("checkoutId does not match cwd".into()));
        }

        let stale = || comet_proto::CheckoutFileDiffText {
            diff_checksum: request.diff_checksum.clone(),
            old_text: None,
            new_text: None,
            old_content_hash: None,
            new_content_hash: None,
            binary: false,
            truncated: false,
            stale: true,
        };
        let root = identity.root.as_path();
        let snapshot = Box::pin(crate::diff_sync::capture_diff(&self.repos, root))
            .await
            .map_err(|error| RpcError::Failed(error.to_string()))?;
        if snapshot.checksum != request.diff_checksum {
            return Ok(stale());
        }
        let file = snapshot
            .files
            .iter()
            .find(|file| file.path == request.path)
            .ok_or_else(|| RpcError::Failed("path is not part of diff snapshot".into()))?;
        let base = Box::pin(crate::diff_sync::working_diff_base(root))
            .await
            .map_err(|error| RpcError::Failed(error.to_string()))?;
        let pair = Box::pin(crate::diff_sync::read_diff_file_text(root, &base, file))
            .await
            .map_err(|error| RpcError::Failed(error.to_string()))?;

        after_read().await;

        let current = Box::pin(crate::diff_sync::capture_diff(&self.repos, root))
            .await
            .map_err(|error| RpcError::Failed(error.to_string()))?;
        if current.checksum != request.diff_checksum {
            return Ok(stale());
        }
        Ok(comet_proto::CheckoutFileDiffText {
            diff_checksum: request.diff_checksum,
            old_text: pair.old_text,
            new_text: pair.new_text,
            old_content_hash: pair.old_content_hash,
            new_content_hash: pair.new_content_hash,
            binary: pair.binary,
            truncated: pair.truncated,
            stale: false,
        })
    }
}

/// A watch receiver as a stream: current value first, then every change.
fn watch_stream<T>(rx: watch::Receiver<T>) -> BoxStream<'static, serde_json::Value>
where
    T: serde::Serialize + Clone + Send + Sync + 'static,
{
    futures::stream::unfold((rx, false), |(mut rx, emitted)| async move {
        if emitted {
            rx.changed().await.ok()?;
        }
        let value = {
            let borrowed = rx.borrow_and_update();
            serde_json::to_value(&*borrowed).ok()?
        };
        Some((value, (rx, true)))
    })
    .boxed()
}

/// The transcript watch as delta frames (`comet_doc::transcript_delta`): a
/// full `reset` first, then only changed entries per commit — the whole-Vec
/// serialization here was the per-tick cost that scaled with transcript size.
fn doc_messages_stream(
    rx: watch::Receiver<Vec<comet_doc::SessionMessageEntry>>,
) -> BoxStream<'static, serde_json::Value> {
    use comet_doc::transcript_delta::{TranscriptFrame, diff_transcript};
    futures::stream::unfold(
        (rx, None::<Vec<comet_doc::SessionMessageEntry>>),
        |(mut rx, mut prev)| async move {
            loop {
                if prev.is_some() {
                    rx.changed().await.ok()?;
                }
                let current: Vec<_> = rx.borrow_and_update().clone();
                let frame = match prev.as_deref() {
                    None => TranscriptFrame::reset(&current),
                    Some(prev) => diff_transcript(prev, &current),
                };
                prev = Some(current);
                // No-op commits (a second watcher attaching, command-only
                // changes) produce empty deltas — skip the frame entirely.
                if frame.is_empty_delta() {
                    continue;
                }
                let value = serde_json::to_value(&frame).ok()?;
                return Some((value, (rx, prev)));
            }
        },
    )
    .boxed()
}

impl LocalRpcService {
    pub fn new(
        inner: std::sync::Arc<EngineRpc>,
        store: RemoteConfigStore,
        lan: LanServerHandle,
    ) -> Self {
        Self { inner, store, lan }
    }

    #[cfg(test)]
    pub(crate) fn shares_mutation_authority(&self, other: &Self) -> bool {
        self.inner.shares_mutation_authority(&other.inner)
    }
}

#[async_trait]
impl RpcService for LocalRpcService {
    async fn handle(&self, method: &str, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        match method {
            methods::WATCH_REMOTES => {
                Ok(RpcReply::Stream(watch_stream(self.store.watch_remotes())))
            }
            methods::PUT_REMOTE => {
                let remote: RemoteEntry = parse_params(params)?;
                self.store
                    .put_remote(remote)
                    .map_err(|error| RpcError::Failed(error.to_string()))?;
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            methods::RENAME_REMOTE => {
                #[derive(Deserialize)]
                #[serde(rename_all = "camelCase")]
                struct P {
                    server_id: ServerId,
                    name: String,
                }
                let p: P = parse_params(params)?;
                if p.name.trim().is_empty() {
                    return Err(RpcError::BadParams("remote name cannot be empty".into()));
                }
                let renamed = self
                    .store
                    .rename_remote(&p.server_id, p.name.trim())
                    .map_err(|error| RpcError::Failed(error.to_string()))?;
                if !renamed {
                    return Err(RpcError::Failed("remote registry row not found".into()));
                }
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            methods::REMOVE_REMOTE => {
                #[derive(Deserialize)]
                #[serde(rename_all = "camelCase")]
                struct P {
                    server_id: ServerId,
                }
                let p: P = parse_params(params)?;
                let removed = self
                    .store
                    .remove_remote(&p.server_id)
                    .map_err(|error| RpcError::Failed(error.to_string()))?;
                RpcReply::value(&serde_json::json!({ "removed": removed }))
            }
            methods::REPORT_REMOTE_STATUS => {
                #[derive(Deserialize)]
                #[serde(rename_all = "camelCase")]
                struct P {
                    server_id: ServerId,
                    last_state: RemoteConnectionState,
                    protocol_version: u32,
                    last_connected_at: Option<chrono::DateTime<chrono::Utc>>,
                }
                let p: P = parse_params(params)?;
                let found = self
                    .store
                    .update_remote_status(
                        &p.server_id,
                        p.last_state,
                        p.protocol_version,
                        p.last_connected_at,
                    )
                    .map_err(|error| RpcError::Failed(error.to_string()))?;
                if !found {
                    return Err(RpcError::Failed("remote registry row not found".into()));
                }
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            methods::GET_LAN_SETTINGS => RpcReply::value(&serde_json::json!({
                "settings": self.store.lan_settings(),
                "status": self.lan.status(),
            })),
            methods::SET_LAN_SETTINGS => {
                let settings: LanSettings = parse_params(params)?;
                self.lan
                    .apply_settings(settings)
                    .map_err(|error| RpcError::Failed(error.to_string()))?;
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            methods::BEGIN_PAIRING => {
                let (secret, expires_at) = self.lan.begin_pairing();
                RpcReply::value(&serde_json::json!({
                    "secret": secret,
                    "expiresAt": expires_at,
                }))
            }
            methods::WATCH_TRUSTED_CLIENTS => Ok(RpcReply::Stream(watch_stream(
                self.store.watch_trusted_clients(),
            ))),
            methods::REVOKE_TRUSTED_CLIENT => {
                #[derive(Deserialize)]
                #[serde(rename_all = "camelCase")]
                struct P {
                    server_id: ServerId,
                }
                let p: P = parse_params(params)?;
                let removed = self
                    .store
                    .revoke_client(&p.server_id)
                    .map_err(|error| RpcError::Failed(error.to_string()))?;
                self.lan.close_client(&p.server_id);
                RpcReply::value(&serde_json::json!({ "removed": removed }))
            }
            _ => self.inner.handle(method, params).await,
        }
    }

    /// Forwarded, not defaulted: `rpc_service()` hands out this wrapper, so a
    /// defaulted `None` here would silently drop every embedded-UI client
    /// from the presence count.
    fn attached(&self) -> Option<comet_rpc::ConnectionLease> {
        self.inner.attached()
    }
}

#[async_trait]
impl RpcService for EngineRpc {
    async fn handle(&self, method: &str, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        match method {
            methods::SERVER_HELLO => RpcReply::value(self.server_hello()?),
            methods::LIST_HARNESSES => RpcReply::value(&self.registry.descriptors()),
            methods::LIST_HARNESS_DIAGNOSTICS => RpcReply::value(&self.registry.diagnostics()),
            methods::LIST_MODELS => {
                let p: ListModelsParams = parse_params(params)?;
                let harness = self
                    .registry
                    .resolve(p.harness)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                if p.force {
                    harness.clear_discovery();
                }
                let catalog = harness
                    .models()
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                // Only an unreadable answer is drift. An absent CLI is
                // ordinary and stays out of the diagnostics surface, or the
                // card reads a figure on every healthy boot.
                //
                // Taking rather than peeking: the failure stays cached for the
                // whole boot, so peeking would record one unreadable answer
                // again on every picker open, inflating the count and
                // refreshing `last_seen_ms` as if the provider kept failing.
                if harness.take_unreported_discovery_failure()
                    == Some(comet_harness::discovery::DiscoveryFailure::Unparseable)
                {
                    self.registry.record_diagnostic(
                        p.harness,
                        "discovery/unreadable",
                        DiagnosticSeverity::Malformed,
                    );
                }
                RpcReply::value(&catalog)
            }
            methods::LIST_COMMANDS => {
                let p: ListCommandsParams = parse_params(params)?;
                let harness = self
                    .registry
                    .resolve(p.harness)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                let commands = harness
                    .commands(&p.cwd)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                // Answered as an object rather than a bare array, unlike the
                // shape `ListModels` shipped and had to reshape in 2.1: a
                // top-level array leaves nowhere to add a field without a
                // whole-value decode change, and that reshape is what broke the
                // picker at run time with every test green.
                RpcReply::value(&serde_json::json!({ "commands": commands }))
            }
            methods::QUEUE_COMMAND => {
                let p: QueueCommandParams = parse_params(params)?;
                let command_id = self
                    .doc_host
                    .queue_command(&p.chat_id, p.command)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&serde_json::json!({ "commandId": command_id }))
            }
            methods::WATCH_DOC_MESSAGES => {
                let p: ChatParams = parse_params(params)?;
                let handle = self
                    .doc_host
                    .open(&p.chat_id)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                Ok(RpcReply::Stream(doc_messages_stream(
                    handle.watch_messages(),
                )))
            }
            methods::WATCH_CHATS => {
                Ok(RpcReply::Stream(watch_stream(self.workspace.watch_chats())))
            }
            methods::WATCH_DEVICES => Ok(RpcReply::Stream(watch_stream(
                self.workspace.watch_devices(),
            ))),
            methods::WATCH_SPACES => Ok(RpcReply::Stream(watch_stream(
                self.workspace.watch_spaces(),
            ))),
            methods::WATCH_SESSIONS => {
                // Local live statuses merged with remote devices' workspace rows.
                let merged = self
                    .workspace
                    .merged_sessions_watch(self.sessions.watch_sessions());
                Ok(RpcReply::Stream(watch_stream(merged)))
            }
            methods::LOCAL_DEVICE => {
                // `identityRebuiltAt` is additive and absent when nothing was
                // rebuilt, so it needs no `PROTOCOL_VERSION` bump: a peer that
                // drops the key shows no notice, which is what every build
                // before this one did. Read that constant's own doc comment
                // before assuming the same of the next field — the test is
                // whether an older peer would ACT on the absence, and here
                // there is nothing to act on.
                RpcReply::value(&serde_json::json!({
                    "deviceId": self.doc_host.device_id(),
                    "identityRebuiltAt": self.identity_rebuilt_at,

                }))
            }
            methods::UPDATE_STATUS => Ok(RpcReply::Stream(watch_stream(self.updater()?.watch()))),
            methods::APPLY_UPDATE => {
                let version = self
                    .updater()?
                    .apply()
                    .await
                    .map_err(|e| RpcError::Failed(format!("{e:#}")))?;
                RpcReply::value(&serde_json::json!({ "ok": true, "version": version }))
            }
            methods::MUTATE => {
                let p: MutateParams = parse_params(params)?;
                self.mutation_authority.run(|| self.mutate(p))?;
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            methods::WATCH_CHECKOUT_DIFFS => {
                Ok(RpcReply::Stream(watch_stream(self.diff_sync.watch_diffs())))
            }
            methods::GET_CHECKOUT_FILE_DIFF_TEXT => {
                // The source-pair path contains several large nested async
                // futures. Keep that state off every unrelated RPC's worker
                // stack; this is the same bounded-dispatch mechanism as the
                // selected upstream stack follow-up (`1ef4aca`).
                Box::pin(async move {
                    let request: comet_proto::GetCheckoutFileDiffTextRequest =
                        parse_params(params)?;
                    let reply =
                        Box::pin(self.checkout_file_diff_text_with_after_read(request, || {
                            std::future::ready(())
                        }))
                        .await?;
                    RpcReply::value(&reply)
                })
                .await
            }
            methods::LIST_REPOS => RpcReply::value(&self.repos.list().await),
            methods::ADD_REPO => {
                #[derive(Deserialize)]
                struct P {
                    path: String,
                }
                let p: P = parse_params(params)?;
                let repo = self
                    .repos
                    .add(&p.path)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&repo)
            }
            methods::CLONE_REPO => {
                #[derive(Deserialize)]
                struct P {
                    url: String,
                }
                let p: P = parse_params(params)?;
                let repo = self
                    .repos
                    .clone_repo(&p.url)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&repo)
            }
            methods::CREATE_REPO => {
                #[derive(Deserialize)]
                struct P {
                    name: String,
                }
                let p: P = parse_params(params)?;
                let repo = self
                    .repos
                    .create(&p.name)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&repo)
            }
            methods::LIST_BRANCHES => {
                let p: RepoPathParams = parse_params(params)?;
                let branches = self
                    .repos
                    .branches(std::path::Path::new(&p.repo_path))
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&branches)
            }
            methods::LIST_REFS => {
                let p: RepoPathParams = parse_params(params)?;
                let refs = self
                    .repos
                    .refs(std::path::Path::new(&p.repo_path))
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&refs)
            }
            methods::SWITCH_REF => {
                let p: SwitchRefParams = parse_params(params)?;
                let branch = self
                    .repos
                    .switch_ref(std::path::Path::new(&p.repo_path), &p.ref_name)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&serde_json::json!({ "branch": branch }))
            }
            methods::LIST_FOLDERS => {
                let p: ListFoldersParams = parse_params(params)?;
                let listing = self
                    .repos
                    .list_folders(p.path)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&listing)
            }
            methods::SEARCH_FILES => {
                let p: FileSearchParams = parse_params(params)?;
                if p.query.chars().count() > 256 {
                    return Err(RpcError::BadParams(
                        "SearchFiles query must not exceed 256 characters".into(),
                    ));
                }
                let matches = tokio::time::timeout(FILE_SEARCH_RPC_TIMEOUT, async {
                    let root = self.file_search_root(&p).await?;
                    let featured_paths = p
                        .chat_id
                        .as_deref()
                        .filter(|_| p.query.is_empty())
                        .map(|chat_id| self.featured_file_paths(chat_id))
                        .unwrap_or_default();
                    self.repos
                        .search_files(root, p.query, featured_paths)
                        .await
                        .map_err(|e| RpcError::Failed(e.to_string()))
                })
                .await
                .map_err(|_| RpcError::Failed("file search timed out".into()))??;
                RpcReply::value(&matches)
            }
            methods::CREATE_WORKTREE => {
                let p: CreateWorktreeParams = parse_params(params)?;
                let worktree = self
                    .repos
                    .create_worktree(std::path::Path::new(&p.repo_path), &p.branch)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&worktree)
            }
            methods::DELETE_WORKTREE => {
                let p: DeleteWorktreeParams = parse_params(params)?;
                self.repos
                    .delete_worktree(
                        std::path::Path::new(&p.repo_path),
                        std::path::Path::new(&p.worktree_path),
                    )
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            methods::OPEN_TERMINAL => {
                let p: OpenTerminalParams = parse_params(params)?;
                // The terminal runs in the chat's checkout; a chat with no cwd (or
                // no row yet) gets the home directory.
                let cwd = self
                    .workspace
                    .doc()
                    .chat(&p.chat_id)
                    .ok()
                    .flatten()
                    .and_then(|chat| chat.cwd)
                    .unwrap_or_else(|| home_dir().to_string_lossy().to_string());
                let session = self
                    .terminals
                    .open(&cwd, p.cols, p.rows)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&session)
            }
            methods::SUBSCRIBE_TERMINAL => {
                let p: SubscribeTerminalParams = parse_params(params)?;
                let rx = self
                    .terminals
                    .subscribe(&p.terminal_id, p.after_seq)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                let stream = futures::stream::unfold(rx, |mut rx| async move {
                    let event = rx.recv().await?;
                    let value = serde_json::to_value(&event).ok()?;
                    Some((value, rx))
                });
                Ok(RpcReply::Stream(stream.boxed()))
            }
            methods::WRITE_TERMINAL => {
                let p: WriteTerminalParams = parse_params(params)?;
                self.terminals
                    .write(&p.terminal_id, &p.data)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            methods::RESIZE_TERMINAL => {
                let p: ResizeTerminalParams = parse_params(params)?;
                self.terminals
                    .resize(&p.terminal_id, p.cols, p.rows)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            methods::CLOSE_TERMINAL => {
                let p: TerminalIdParams = parse_params(params)?;
                self.terminals
                    .close(&p.terminal_id)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            methods::LIST_AGENT_ACCOUNTS => {
                let p: ListAgentAccountsParams = parse_params(params)?;
                let snapshot = self
                    .agent_accounts
                    .list(p.force_usage.unwrap_or(false))
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&snapshot)
            }
            methods::ACTIVATE_AGENT_ACCOUNT => {
                let p: AgentAccountParams = parse_params(params)?;
                let snapshot = self
                    .agent_accounts
                    .activate(p.harness, &p.account_id)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&snapshot)
            }
            methods::FORGET_AGENT_ACCOUNT => {
                let p: AgentAccountParams = parse_params(params)?;
                let snapshot = self
                    .agent_accounts
                    .forget(p.harness, &p.account_id)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&snapshot)
            }
            methods::START_AGENT_LOGIN => {
                let p: StartAgentLoginParams = parse_params(params)?;
                let start = self
                    .agent_accounts
                    .start_login(p.harness)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&start)
            }
            methods::COMPLETE_AGENT_LOGIN => {
                let p: CompleteAgentLoginParams = parse_params(params)?;
                let snapshot = self
                    .agent_accounts
                    .complete_login(&p.login_id, &p.code)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&snapshot)
            }
            methods::POLL_AGENT_LOGIN => {
                let p: LoginIdParams = parse_params(params)?;
                let poll = self
                    .agent_accounts
                    .poll_login(&p.login_id)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&poll)
            }
            methods::CANCEL_AGENT_LOGIN => {
                let p: LoginIdParams = parse_params(params)?;
                self.agent_accounts.cancel_login(&p.login_id);
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            methods::UPLOAD_CHUNK => {
                let p: UploadChunkParams = parse_params(params)?;
                self.uploads
                    .append(&p.upload_id, &p.data, p.seq)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            methods::UPLOAD_COMMIT => {
                let p: UploadCommitParams = parse_params(params)?;
                let path = self
                    .uploads
                    .commit(&p.upload_id, &p.file_name)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&serde_json::json!({ "path": path }))
            }
            methods::READ_ATTACHMENT_CHUNK => {
                let p: ReadAttachmentChunkParams = parse_params(params)?;
                // Path jail: the uploads dir plus every workspace-known chat cwd.
                let roots: Vec<std::path::PathBuf> = self
                    .workspace
                    .doc()
                    .read_chats()
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|chat| chat.cwd)
                    .map(std::path::PathBuf::from)
                    .collect();
                let chunk = self
                    .uploads
                    .read_chunk(&p.path, p.offset, &roots)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&chunk)
            }
            methods::READ_TOOL_DIFF => {
                let p: ReadToolDiffParams = parse_params(params)?;
                let chat = self
                    .workspace
                    .doc()
                    .chat(&p.chat_id)
                    .map_err(|error| RpcError::Failed(error.to_string()))?
                    .filter(|chat| chat.device_id == self.doc_host.device_id())
                    .ok_or_else(|| RpcError::Failed("chat is not hosted by this device".into()))?;
                let reply = match self
                    .doc_host
                    .read_tool_diff(&chat.id, &p.part_id, &p.diff_ref)
                    .map_err(|error| RpcError::Failed(error.to_string()))?
                {
                    Some(diff) => ReadToolDiffReply::Available { diff },
                    None => ReadToolDiffReply::NotAvailable,
                };
                RpcReply::value(&reply)
            }
            other => Err(RpcError::UnknownMethod(other.to_string())),
        }
    }

    /// Every connection counts as a supervisor, and not every connection is a
    /// watching human: `comet status` and the `comet remote …` subcommands open
    /// a real IPC socket, so each one clears `unattended_since` and stamps a
    /// fresh stretch on exit. A monitoring cron polling often enough can
    /// therefore keep a parked approval alive indefinitely. Fails in the safe
    /// direction (nothing expires early) and telling the two apart is a design
    /// change, not a fix here — see `docs/debt/D29-administrative-clients-count-as-supervisors.md`.
    fn attached(&self) -> Option<comet_rpc::ConnectionLease> {
        Some(Box::new(self.presence.attach()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use tokio::sync::oneshot;

    use crate::doc_host::CreateAdmission;
    use comet_sync::{DocsStore, PutToolDiffOutcome};

    /// Emits a tool call, then waits for the test to allow a deliberately
    /// straggling result even after its run has been interrupted.
    struct LateToolResultHarness {
        release: Mutex<Option<oneshot::Receiver<()>>>,
    }

    #[async_trait::async_trait]
    impl comet_harness::Harness for LateToolResultHarness {
        fn id(&self) -> HarnessId {
            HarnessId::Mock
        }

        fn display_name(&self) -> &str {
            "Late tool result"
        }

        fn capabilities(&self) -> comet_proto::HarnessCapabilities {
            comet_proto::HarnessCapabilities::default()
        }

        async fn models(&self) -> Result<comet_proto::ModelCatalog, comet_harness::HarnessError> {
            Ok(comet_proto::ModelCatalog::built_in(Vec::new()))
        }

        async fn run(
            &self,
            request: comet_proto::RunRequest,
            _controls: comet_harness::RunControls,
        ) -> Result<
            futures::stream::BoxStream<
                'static,
                Result<comet_proto::AgentEvent, comet_harness::HarnessError>,
            >,
            comet_harness::HarnessError,
        > {
            if request.prompt != "change it" {
                return Ok(Box::pin(futures::stream::iter([Ok(
                    comet_proto::AgentEvent::Done {
                        status: comet_proto::DoneStatus::Completed,
                        result: Some("title".into()),
                        error: None,
                        session_id: None,
                    },
                )])));
            }
            let release = self
                .release
                .lock()
                .unwrap()
                .take()
                .expect("one scripted run");
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            tokio::spawn(async move {
                let _ = tx.send(comet_proto::AgentEvent::ToolCall {
                    id: "late-tool".into(),
                    call: ToolCall::EditFile {
                        path: "src/lib.rs".into(),
                        old_string: None,
                        new_string: None,
                    },
                });
                let _ = release.await;
                let _ = tx.send(comet_proto::AgentEvent::ToolResult {
                    id: "late-tool".into(),
                    is_error: false,
                    diff: Some(comet_proto::ToolDiff {
                        path: "src/lib.rs".into(),
                        old_text: Some("TASK6_DELETE_POISON_OLD".into()),
                        new_text: "TASK6_DELETE_POISON_NEW".into(),
                    }),
                    diff_ref: Some("v1:stale".into()),
                    diff_stats: None,
                });
                let _ = tx.send(comet_proto::AgentEvent::Done {
                    status: comet_proto::DoneStatus::Completed,
                    result: None,
                    error: None,
                    session_id: None,
                });
            });
            Ok(Box::pin(futures::stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|event| (Ok(event), rx))
            })))
        }
    }

    /// The old generation ends only after DeleteChat has begun, and reports a
    /// terminal error so a stale final status is distinguishable from the
    /// replacement's successful Idle row.
    struct TerminalHandoffHarness {
        old_release: Mutex<Option<oneshot::Receiver<()>>>,
    }

    #[async_trait::async_trait]
    impl comet_harness::Harness for TerminalHandoffHarness {
        fn id(&self) -> HarnessId {
            HarnessId::Mock
        }

        fn display_name(&self) -> &str {
            "Terminal handoff"
        }

        fn capabilities(&self) -> comet_proto::HarnessCapabilities {
            comet_proto::HarnessCapabilities::default()
        }

        async fn models(&self) -> Result<comet_proto::ModelCatalog, comet_harness::HarnessError> {
            Ok(comet_proto::ModelCatalog::built_in(Vec::new()))
        }

        async fn run(
            &self,
            request: comet_proto::RunRequest,
            _controls: comet_harness::RunControls,
        ) -> Result<
            futures::stream::BoxStream<
                'static,
                Result<comet_proto::AgentEvent, comet_harness::HarnessError>,
            >,
            comet_harness::HarnessError,
        > {
            if request.prompt == "old generation" {
                let release = self
                    .old_release
                    .lock()
                    .unwrap()
                    .take()
                    .expect("one old-generation run");
                return Ok(Box::pin(futures::stream::once(async move {
                    let _ = release.await;
                    Ok(comet_proto::AgentEvent::Done {
                        status: comet_proto::DoneStatus::Errored,
                        result: None,
                        error: Some("old generation terminal error".into()),
                        session_id: None,
                    })
                })));
            }

            Ok(Box::pin(futures::stream::iter([Ok(
                comet_proto::AgentEvent::Done {
                    status: comet_proto::DoneStatus::Completed,
                    result: None,
                    error: None,
                    session_id: None,
                },
            )])))
        }
    }

    /// The old generation's provider startup does not finish until DeleteChat
    /// begins, then fails before producing a stream.
    struct StartupErrorTerminalHandoffHarness {
        old_release: Mutex<Option<oneshot::Receiver<()>>>,
    }

    #[async_trait::async_trait]
    impl comet_harness::Harness for StartupErrorTerminalHandoffHarness {
        fn id(&self) -> HarnessId {
            HarnessId::Mock
        }

        fn display_name(&self) -> &str {
            "Startup error terminal handoff"
        }

        fn capabilities(&self) -> comet_proto::HarnessCapabilities {
            comet_proto::HarnessCapabilities::default()
        }

        async fn models(&self) -> Result<comet_proto::ModelCatalog, comet_harness::HarnessError> {
            Ok(comet_proto::ModelCatalog::built_in(Vec::new()))
        }

        async fn run(
            &self,
            request: comet_proto::RunRequest,
            _controls: comet_harness::RunControls,
        ) -> Result<
            futures::stream::BoxStream<
                'static,
                Result<comet_proto::AgentEvent, comet_harness::HarnessError>,
            >,
            comet_harness::HarnessError,
        > {
            if request.prompt == "old generation" {
                let release = self
                    .old_release
                    .lock()
                    .unwrap()
                    .take()
                    .expect("one old-generation run");
                let _ = release.await;
                return Err(comet_harness::HarnessError::Protocol(
                    "startup failure for terminal-handoff test".into(),
                ));
            }

            Ok(Box::pin(futures::stream::iter([Ok(
                comet_proto::AgentEvent::Done {
                    status: comet_proto::DoneStatus::Completed,
                    result: None,
                    error: None,
                    session_id: None,
                },
            )])))
        }
    }

    type RequestLog = Arc<Mutex<Vec<comet_proto::RunRequest>>>;

    /// Records the engine-owned resume injected into every harness request and
    /// advances its native session id on each completed run.
    struct ResumeRecordingHarness {
        requests: RequestLog,
        next_session: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl comet_harness::Harness for ResumeRecordingHarness {
        fn id(&self) -> HarnessId {
            HarnessId::Mock
        }

        fn display_name(&self) -> &str {
            "Resume recording"
        }

        fn capabilities(&self) -> comet_proto::HarnessCapabilities {
            comet_proto::HarnessCapabilities::default()
        }

        async fn models(&self) -> Result<comet_proto::ModelCatalog, comet_harness::HarnessError> {
            Ok(comet_proto::ModelCatalog::built_in(Vec::new()))
        }

        async fn run(
            &self,
            request: comet_proto::RunRequest,
            _controls: comet_harness::RunControls,
        ) -> Result<
            futures::stream::BoxStream<
                'static,
                Result<comet_proto::AgentEvent, comet_harness::HarnessError>,
            >,
            comet_harness::HarnessError,
        > {
            self.requests.lock().unwrap().push(request.clone());
            let session_id = format!(
                "recorded-session-{}",
                self.next_session.fetch_add(1, Ordering::SeqCst) + 1
            );
            Ok(Box::pin(futures::stream::iter([
                Ok(comet_proto::AgentEvent::SessionStarted {
                    harness: HarnessId::Mock,
                    model: "mock".into(),
                    tools: Vec::new(),
                    cwd: request.cwd,
                    session_id: session_id.clone(),
                    assistant_message_id: "assistant-1".into(),
                    runtime_mode: comet_proto::RuntimeMode::default(),
                }),
                Ok(comet_proto::AgentEvent::Done {
                    status: comet_proto::DoneStatus::Completed,
                    result: None,
                    error: None,
                    session_id: Some(session_id),
                }),
            ])))
        }
    }

    /// Records every request, but makes exactly one engine-injected resume end
    /// before SessionStarted so the engine exercises its fresh-session retry.
    struct RejectedResumeRecordingHarness {
        requests: RequestLog,
        next_session: AtomicUsize,
        rejected_resume: AtomicBool,
    }

    #[async_trait::async_trait]
    impl comet_harness::Harness for RejectedResumeRecordingHarness {
        fn id(&self) -> HarnessId {
            HarnessId::Mock
        }

        fn display_name(&self) -> &str {
            "Rejected resume recording"
        }

        fn capabilities(&self) -> comet_proto::HarnessCapabilities {
            comet_proto::HarnessCapabilities {
                supports_steering: true,
                ..Default::default()
            }
        }

        async fn models(&self) -> Result<comet_proto::ModelCatalog, comet_harness::HarnessError> {
            Ok(comet_proto::ModelCatalog::built_in(Vec::new()))
        }

        async fn run(
            &self,
            request: comet_proto::RunRequest,
            _controls: comet_harness::RunControls,
        ) -> Result<
            futures::stream::BoxStream<
                'static,
                Result<comet_proto::AgentEvent, comet_harness::HarnessError>,
            >,
            comet_harness::HarnessError,
        > {
            self.requests.lock().unwrap().push(request.clone());
            if request.resume.is_some() && !self.rejected_resume.swap(true, Ordering::SeqCst) {
                return Ok(Box::pin(futures::stream::iter([Ok(
                    comet_proto::AgentEvent::Done {
                        status: comet_proto::DoneStatus::Errored,
                        result: None,
                        error: Some("injected resume rejected".into()),
                        session_id: None,
                    },
                )])));
            }

            let session_id = format!(
                "rejected-resume-session-{}",
                self.next_session.fetch_add(1, Ordering::SeqCst) + 1
            );
            if request.prompt == "replacement prompt" {
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                tx.send(comet_proto::AgentEvent::SessionStarted {
                    harness: HarnessId::Mock,
                    model: "mock".into(),
                    tools: Vec::new(),
                    cwd: request.cwd,
                    session_id: session_id.clone(),
                    assistant_message_id: "assistant-1".into(),
                    runtime_mode: comet_proto::RuntimeMode::default(),
                })
                .unwrap();
                tx.send(comet_proto::AgentEvent::Done {
                    status: comet_proto::DoneStatus::Completed,
                    result: None,
                    error: None,
                    session_id: Some(session_id),
                })
                .unwrap();
                let keepalive = tx.clone();
                return Ok(Box::pin(futures::stream::unfold(
                    (rx, _controls.steering, keepalive),
                    |(mut rx, steering, keepalive)| async move {
                        rx.recv()
                            .await
                            .map(|event| (Ok(event), (rx, steering, keepalive)))
                    },
                )));
            }
            Ok(Box::pin(futures::stream::iter([
                Ok(comet_proto::AgentEvent::SessionStarted {
                    harness: HarnessId::Mock,
                    model: "mock".into(),
                    tools: Vec::new(),
                    cwd: request.cwd,
                    session_id: session_id.clone(),
                    assistant_message_id: "assistant-1".into(),
                    runtime_mode: comet_proto::RuntimeMode::default(),
                }),
                Ok(comet_proto::AgentEvent::Done {
                    status: comet_proto::DoneStatus::Completed,
                    result: None,
                    error: None,
                    session_id: Some(session_id),
                }),
            ])))
        }
    }

    struct RegistrationProbeHarness {
        started: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl comet_harness::Harness for RegistrationProbeHarness {
        fn id(&self) -> HarnessId {
            HarnessId::Mock
        }

        fn display_name(&self) -> &str {
            "Registration probe"
        }

        fn capabilities(&self) -> comet_proto::HarnessCapabilities {
            comet_proto::HarnessCapabilities::default()
        }

        async fn models(&self) -> Result<comet_proto::ModelCatalog, comet_harness::HarnessError> {
            Ok(comet_proto::ModelCatalog::built_in(Vec::new()))
        }

        async fn run(
            &self,
            _request: comet_proto::RunRequest,
            _controls: comet_harness::RunControls,
        ) -> Result<
            futures::stream::BoxStream<
                'static,
                Result<comet_proto::AgentEvent, comet_harness::HarnessError>,
            >,
            comet_harness::HarnessError,
        > {
            self.started.store(true, Ordering::SeqCst);
            let diff = comet_proto::ToolDiff {
                path: "src/old-registration.rs".into(),
                old_text: Some("OLD_REGISTRATION_SOURCE".into()),
                new_text: "NEW_REGISTRATION_SOURCE".into(),
            };
            Ok(Box::pin(futures::stream::iter([
                Ok(comet_proto::AgentEvent::ToolCall {
                    id: "old-registration-tool".into(),
                    call: ToolCall::EditFile {
                        path: "src/old-registration.rs".into(),
                        old_string: None,
                        new_string: None,
                    },
                }),
                Ok(comet_proto::AgentEvent::ToolResult {
                    id: "old-registration-tool".into(),
                    is_error: false,
                    diff: Some(diff),
                    diff_ref: Some("v1:stale".into()),
                    diff_stats: None,
                }),
                Ok(comet_proto::AgentEvent::Done {
                    status: comet_proto::DoneStatus::Completed,
                    result: None,
                    error: None,
                    session_id: None,
                }),
            ])))
        }
    }

    fn registration_request(dir: &std::path::Path) -> comet_proto::RunRequest {
        comet_proto::RunRequest {
            prompt: "register old generation".into(),
            harness: None,
            model: None,
            reasoning: None,
            model_options: Default::default(),
            cwd: unwatched_space_root(dir).to_string_lossy().to_string(),
            runtime_mode: comet_proto::RuntimeMode::default(),
            sandbox: comet_proto::SandboxLevel::WorkspaceWrite,
            attachments: Vec::new(),
            resume: None,
        }
    }

    /// D101: a space's folder must never be a real, existing directory in this
    /// module. `SpacesSync` (`crate::spaces`) arms a non-recursive `notify`
    /// filesystem watcher directly on `Space::path` — and every test here backs
    /// its `EngineCore` with the same `tempfile::TempDir` it registers as that
    /// path, so the watcher ends up watching the exact tree `TempDir::drop()`
    /// recursively deletes at the end of the test.
    ///
    /// That used to deadlock intermittently on Windows: `notify` 7.0's
    /// `ReadDirectoryChangesWatcher` only *posts* a stop request from `Drop`/
    /// `unwatch` and never waits for its background thread to act on it (see
    /// `notify-7.0.0/src/windows.rs`, `Drop for ReadDirectoryChangesWatcher` and
    /// `stop_watch`) — so a delete racing a slow-to-stop watcher on the same
    /// directory could starve the stop forever behind a stream of change events
    /// the delete itself was generating. There is no synchronous, awaitable stop
    /// this crate exposes to build a teardown-ordering fix on.
    ///
    /// `notify::Watcher::watch` fails fast, entirely inside `notify` and before
    /// any OS handle is opened, when the path is neither a file nor a directory
    /// (`windows.rs`'s `watch_inner`) — so a space folder that is never created
    /// keeps every test's watcher permanently unarmed: no `ReadDirectoryChangesW`
    /// call is ever issued, and the race has nothing to race. `create_space`
    /// itself does no filesystem validation (`workspace_host.rs`), so this is
    /// safe: it is a doc-row string, and none of this module's tests read a
    /// space's folder back off disk.
    fn unwatched_space_root(dir: &std::path::Path) -> std::path::PathBuf {
        let root = dir.join("space-root-not-on-disk");
        // Enforces the invariant this whole module leans on: if anything ever
        // creates this path, notify::Watcher::watch stops failing fast and the
        // D101 race is back. A debug_assert turns that into a loud test
        // failure at the call site instead of a silent, timing-dependent hang
        // someone has to rediscover with a debugger.
        debug_assert!(
            !root.exists(),
            "unwatched_space_root must stay unwatchable: {} exists",
            root.display()
        );
        root
    }

    fn engine_core(dir: &std::path::Path) -> crate::EngineCore {
        let registry = Arc::new(crate::registry::HarnessRegistry::new());
        registry.register(Arc::new(comet_harness::mock::MockHarness::new()));
        crate::EngineCore::assemble(dir, registry, HarnessId::Mock, None)
            .expect("engine core assembles")
    }

    async fn init_source_pair_repo(root: &std::path::Path) {
        std::fs::create_dir_all(root).expect("repo directory");
        for args in [
            vec!["init", "-b", "main"],
            vec!["add", "."],
            vec!["commit", "-m", "initial"],
        ] {
            if args[0] == "add" {
                std::fs::write(root.join("a.txt"), "one\ntwo\n").expect("tracked source");
            }
            let output = tokio::process::Command::new("git")
                .args(&args)
                .current_dir(root)
                .env("GIT_AUTHOR_NAME", "test")
                .env("GIT_AUTHOR_EMAIL", "test@test")
                .env("GIT_COMMITTER_NAME", "test")
                .env("GIT_COMMITTER_EMAIL", "test@test")
                .output()
                .await
                .expect("git command");
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    #[tokio::test]
    async fn checkout_file_diff_text_rpc_returns_stale_after_read_hook() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        init_source_pair_repo(&repo).await;
        std::fs::write(repo.join("a.txt"), "one\nfirst\n").expect("first edit");

        let core = engine_core(&dir.path().join("data"));
        let identity = core
            .repos
            .checkout_identity(&repo)
            .await
            .expect("checkout identity");
        let snapshot = crate::diff_sync::capture_diff(&core.repos, &repo)
            .await
            .expect("diff snapshot");
        let request = comet_proto::GetCheckoutFileDiffTextRequest {
            checkout_id: identity.id,
            cwd: repo.to_string_lossy().into_owned(),
            path: "a.txt".into(),
            diff_checksum: snapshot.checksum.clone(),
        };
        let changed_path = repo.join("a.txt");
        let reply = core
            .remote_rpc_service()
            .checkout_file_diff_text_with_after_read(request, move || async move {
                std::fs::write(changed_path, "one\nsecond\n").expect("post-read edit");
            })
            .await
            .expect("stale reply");

        assert_eq!(reply.diff_checksum, snapshot.checksum);
        assert!(reply.stale);
        assert_eq!(reply.old_text, None);
        assert_eq!(reply.new_text, None);
        assert_eq!(reply.old_content_hash, None);
        assert_eq!(reply.new_content_hash, None);
        core.shutdown().await;
    }

    fn recording_core(dir: &std::path::Path, requests: RequestLog) -> crate::EngineCore {
        let registry = Arc::new(crate::registry::HarnessRegistry::new());
        registry.register(Arc::new(ResumeRecordingHarness {
            requests,
            next_session: AtomicUsize::new(0),
        }));
        crate::EngineCore::assemble(dir, registry, HarnessId::Mock, None)
            .expect("engine core assembles")
    }

    fn rejected_resume_core(dir: &std::path::Path, requests: RequestLog) -> crate::EngineCore {
        let registry = Arc::new(crate::registry::HarnessRegistry::new());
        registry.register(Arc::new(RejectedResumeRecordingHarness {
            requests,
            next_session: AtomicUsize::new(0),
            rejected_resume: AtomicBool::new(false),
        }));
        crate::EngineCore::assemble(dir, registry, HarnessId::Mock, None)
            .expect("engine core assembles")
    }

    fn recording_request(dir: &std::path::Path, prompt: &str) -> comet_proto::RunRequest {
        comet_proto::RunRequest {
            prompt: prompt.into(),
            harness: None,
            model: None,
            reasoning: None,
            model_options: Default::default(),
            cwd: unwatched_space_root(dir).to_string_lossy().to_string(),
            runtime_mode: comet_proto::RuntimeMode::default(),
            sandbox: comet_proto::SandboxLevel::WorkspaceWrite,
            attachments: Vec::new(),
            resume: None,
        }
    }

    async fn wait_for_run_completion(core: &crate::EngineCore, chat_id: &str) {
        tokio::time::timeout(Duration::from_secs(1), async {
            while core.sessions.has_live_run(chat_id) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("recording harness run finishes");
    }

    async fn wait_for_run_to_retire(core: &crate::EngineCore, chat_id: &str) {
        tokio::time::timeout(Duration::from_secs(1), async {
            while core.sessions.has_live_run(chat_id) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the released test run retires");
    }

    async fn wait_for_idle(core: &crate::EngineCore, chat_id: &str) {
        tokio::time::timeout(Duration::from_secs(1), async {
            while !matches!(
                core.sessions.session_status(chat_id),
                Some(comet_proto::Session {
                    status: comet_proto::SessionStatus::Idle,
                    ..
                })
            ) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("persistent replacement reaches Idle");
    }

    fn stored_message_ids(store: &DocsStore, chat_id: &str) -> Vec<String> {
        let bytes = store
            .load_snapshot(chat_id)
            .unwrap()
            .expect("snapshot exists");
        let raw = loro::LoroDoc::new();
        raw.import(&bytes).unwrap();
        comet_doc::SessionDoc::from_doc(raw)
            .read_entries()
            .unwrap()
            .into_iter()
            .map(|entry| entry.id)
            .collect()
    }

    #[tokio::test]
    async fn bounded_final_cleanup_retries_until_cleared_or_exhausted() {
        let attempts = Cell::new(0usize);
        let cleared = run_bounded_purge_retries(
            || {
                attempts.set(attempts.get() + 1);
                if attempts.get() == 3 {
                    PurgeFinishOutcome::Purged
                } else {
                    PurgeFinishOutcome::PendingRetry
                }
            },
            Duration::ZERO,
            3,
        )
        .await;
        assert_eq!(cleared, PurgeFinishOutcome::Purged);
        assert_eq!(attempts.get(), 3);

        attempts.set(0);
        let exhausted = run_bounded_purge_retries(
            || {
                attempts.set(attempts.get() + 1);
                PurgeFinishOutcome::PendingRetry
            },
            Duration::ZERO,
            3,
        )
        .await;
        assert_eq!(exhausted, PurgeFinishOutcome::PendingRetry);
        assert_eq!(attempts.get(), 4, "one final pass plus three retries");
    }

    /// The UI's Switch/Forget calls send `{id, accountId, harness}` (+ optional
    /// `targetDeviceId`); the extra fields must be tolerated, `accountId` wins.
    #[test]
    fn agent_account_params_accept_ui_shape() {
        let p: AgentAccountParams = parse_params(serde_json::json!({
            "id": "acct-1",
            "accountId": "acct-1",
            "harness": "claude-code",
            "targetDeviceId": "dev-2",
        }))
        .expect("ui param shape");
        assert_eq!(p.account_id, "acct-1");
        assert_eq!(p.harness, HarnessId::ClaudeCode);
    }
    #[test]
    fn tool_file_paths_keep_workspace_activity_only() {
        assert_eq!(
            tool_file_path(&ToolCall::EditFile {
                path: "src/main.rs".into(),
                old_string: None,
                new_string: None,
            }),
            Some("src/main.rs")
        );
        assert_eq!(
            tool_file_path(&ToolCall::Exec {
                command: "cargo test".into(),
            }),
            None
        );
    }

    /// DeleteChat reaches DocHost's cleanup path, which owns both independent
    /// local artifacts. The workspace tombstone alone must not retain either.
    #[tokio::test]
    async fn purge_chat_deletes_snapshot_and_tool_diffs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("device-id"), "dev-a").unwrap();
        let core = engine_core(dir.path());
        core.workspace
            .create_space(
                "space-1",
                "dev-a",
                unwatched_space_root(dir.path()).to_string_lossy().as_ref(),
                None,
                false,
            )
            .unwrap();
        core.workspace
            .create_chat("chat-1", "space-1", None, None)
            .unwrap();

        let store = DocsStore::open(dir.path().join("local-store")).unwrap();
        store.save_snapshot("chat-1", b"snapshot").unwrap();
        let diff = comet_proto::ToolDiff {
            path: "src/lib.rs".into(),
            old_text: Some("before".into()),
            new_text: "after".into(),
        };
        let PutToolDiffOutcome::Stored { diff_ref, .. } = core
            .doc_host
            .put_tool_diff("chat-1", "tool-1", &diff)
            .unwrap()
        else {
            panic!("small test sidecar must be stored");
        };
        let mut purge_done = core.doc_host.watch_purges();

        core.remote_rpc_service()
            .mutate(MutateParams::DeleteChat {
                chat_id: "chat-1".into(),
            })
            .unwrap();

        tokio::time::timeout(Duration::from_secs(1), purge_done.changed())
            .await
            .expect("final purge completes")
            .expect("purge watch stays connected");

        assert_eq!(store.load_snapshot("chat-1").unwrap(), None);
        assert_eq!(
            core.doc_host
                .read_tool_diff("chat-1", "tool-1", &diff_ref)
                .unwrap(),
            None,
            "purging a chat deletes its exact-source sidecars"
        );
    }

    #[tokio::test]
    async fn same_process_same_id_recreation_starts_without_the_deleted_harness_resume() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("device-id"), "dev-a").unwrap();
        let requests: RequestLog = Arc::new(Mutex::new(Vec::new()));
        let core = recording_core(dir.path(), requests.clone());
        core.workspace
            .create_space(
                "space-1",
                "dev-a",
                unwatched_space_root(dir.path()).to_string_lossy().as_ref(),
                None,
                false,
            )
            .unwrap();
        core.workspace
            .create_chat("chat-1", "space-1", None, None)
            .unwrap();
        core.workspace.rename_chat("chat-1", "Old chat").unwrap();

        core.sessions
            .dispatch(
                "chat-1",
                HarnessId::Mock,
                recording_request(dir.path(), "old generation"),
                Some("old-user".into()),
            )
            .await
            .unwrap();
        wait_for_run_completion(&core, "chat-1").await;
        let journal = crate::RunJournal::open(dir.path().join("local-store/journals")).unwrap();
        journal.note_resume_attempt("chat-1");

        let mut purge_done = core.doc_host.watch_purges();
        core.remote_rpc_service()
            .mutate(MutateParams::DeleteChat {
                chat_id: "chat-1".into(),
            })
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), purge_done.changed())
            .await
            .expect("final DeleteChat purge settles")
            .expect("purge watch remains connected");

        assert!(
            journal.replay("chat-1", 0).unwrap().is_empty(),
            "final DeleteChat purge retires the old run journal"
        );
        assert_eq!(
            journal.resume_attempts("chat-1"),
            0,
            "final DeleteChat purge retires the old resume counter"
        );

        core.remote_rpc_service()
            .mutate(MutateParams::CreateChat {
                chat_id: "chat-1".into(),
                space_id: "space-1".into(),
                config: None,
                branch: None,
                cwd: None,
            })
            .unwrap();
        core.workspace.rename_chat("chat-1", "New chat").unwrap();
        core.sessions
            .dispatch(
                "chat-1",
                HarnessId::Mock,
                recording_request(dir.path(), "new generation"),
                Some("new-user".into()),
            )
            .await
            .unwrap();
        wait_for_run_completion(&core, "chat-1").await;

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[1].resume, None,
            "the first run in a recreated chat starts fresh"
        );
    }

    /// A rejected resume belongs to the document generation that launched it.
    /// Reusing a chat id after DeleteChat must not let its retry write or run
    /// against the new document generation.
    #[tokio::test]
    async fn rejected_resume_retry_cannot_cross_a_recreated_chat_generation() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("device-id"), "dev-a").unwrap();
        let requests: RequestLog = Arc::new(Mutex::new(Vec::new()));
        let core = rejected_resume_core(dir.path(), requests.clone());
        core.workspace
            .create_space(
                "space-1",
                "dev-a",
                unwatched_space_root(dir.path()).to_string_lossy().as_ref(),
                None,
                false,
            )
            .unwrap();
        core.workspace
            .create_chat("chat-1", "space-1", None, None)
            .unwrap();

        // Seed the old generation with the session id which the next request
        // has injected, then rejected, by the recording harness.
        core.sessions
            .dispatch(
                "chat-1",
                HarnessId::Mock,
                recording_request(dir.path(), "seed old generation"),
                Some("seed-user".into()),
            )
            .await
            .unwrap();
        wait_for_run_completion(&core, "chat-1").await;

        let (retry_reached, retry_release) =
            core.sessions.pause_next_resume_retry_for_test("chat-1");
        core.sessions
            .dispatch(
                "chat-1",
                HarnessId::Mock,
                recording_request(dir.path(), "old prompt must stay old"),
                Some("old-user".into()),
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), retry_reached)
            .await
            .expect("rejected resume reaches the retry pause")
            .expect("retry pause remains armed");

        let mut purge_done = core.doc_host.watch_purges();
        core.remote_rpc_service()
            .mutate(MutateParams::DeleteChat {
                chat_id: "chat-1".into(),
            })
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), purge_done.changed())
            .await
            .expect("old generation final purge settles")
            .expect("purge watch remains connected");
        core.remote_rpc_service()
            .mutate(MutateParams::CreateChat {
                chat_id: "chat-1".into(),
                space_id: "space-1".into(),
                config: None,
                branch: None,
                cwd: None,
            })
            .unwrap();

        core.sessions
            .dispatch(
                "chat-1",
                HarnessId::Mock,
                recording_request(dir.path(), "replacement prompt"),
                Some("replacement-user".into()),
            )
            .await
            .unwrap();
        wait_for_idle(&core, "chat-1").await;

        let replacement_entries_before = core
            .doc_host
            .open("chat-1")
            .unwrap()
            .doc()
            .read_entries()
            .unwrap();
        let journal = crate::RunJournal::open(dir.path().join("local-store/journals")).unwrap();
        let replacement_journal_before = journal.replay("chat-1", 0).unwrap();
        let replacement_last_request = core.sessions.last_request("chat-1");
        let replacement_workspace_status = core
            .workspace
            .doc()
            .read_sessions()
            .unwrap()
            .into_iter()
            .find(|session| session.chat_id == "chat-1");
        let replacement_workspace_chat = core.workspace.doc().chat("chat-1").unwrap();
        let replacement_diff = comet_proto::ToolDiff {
            path: "src/replacement.rs".into(),
            old_text: Some("replacement old".into()),
            new_text: "replacement new".into(),
        };
        let PutToolDiffOutcome::Stored { diff_ref, .. } = core
            .doc_host
            .put_tool_diff("chat-1", "replacement-tool", &replacement_diff)
            .unwrap()
        else {
            panic!("replacement sidecar is stored before stale retry release");
        };
        let replacement_sidecar_before = core
            .doc_host
            .read_tool_diff("chat-1", "replacement-tool", &diff_ref)
            .unwrap();

        retry_release.send(()).unwrap();
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }

        let replacement_entries_after = core
            .doc_host
            .open("chat-1")
            .unwrap()
            .doc()
            .read_entries()
            .unwrap();
        let replacement_journal_after = journal.replay("chat-1", 0).unwrap();
        let workspace_status = core
            .workspace
            .doc()
            .read_sessions()
            .unwrap()
            .into_iter()
            .find(|session| session.chat_id == "chat-1")
            .map(|session| session.status);
        let replacement_workspace_chat_after = core.workspace.doc().chat("chat-1").unwrap();
        let replacement_sidecar_after = core
            .doc_host
            .read_tool_diff("chat-1", "replacement-tool", &diff_ref)
            .unwrap();
        let requests = requests.lock().unwrap();
        let recorded_old_prompt_count = requests
            .iter()
            .filter(|request| request.prompt == "old prompt must stay old")
            .count();
        let recorded_replacement_prompt_count = requests
            .iter()
            .filter(|request| request.prompt == "replacement prompt")
            .count();

        assert_eq!(replacement_entries_after, replacement_entries_before);
        assert_eq!(replacement_journal_after, replacement_journal_before);
        assert_eq!(
            core.sessions.last_request("chat-1"),
            replacement_last_request
        );
        assert_eq!(
            core.sessions.session_status("chat-1").unwrap().status,
            comet_proto::SessionStatus::Idle
        );
        assert_eq!(workspace_status, Some(comet_proto::SessionStatus::Idle));
        assert_eq!(
            replacement_workspace_chat_after, replacement_workspace_chat,
            "the refused retry cannot touch the replacement workspace row"
        );
        assert_eq!(
            replacement_sidecar_after, replacement_sidecar_before,
            "the refused retry cannot change replacement exact sources"
        );
        assert_eq!(
            replacement_workspace_status.map(|session| session.status),
            Some(comet_proto::SessionStatus::Idle)
        );
        assert_eq!(recorded_old_prompt_count, 1);
        assert_eq!(recorded_replacement_prompt_count, 1);
    }

    /// The generation match alone is insufficient: the retry cannot displace
    /// a newer live run that owns that same document generation.
    #[tokio::test]
    async fn rejected_resume_retry_cannot_displace_a_live_same_generation_run() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("device-id"), "dev-a").unwrap();
        let requests: RequestLog = Arc::new(Mutex::new(Vec::new()));
        let core = rejected_resume_core(dir.path(), requests.clone());
        core.workspace
            .create_space(
                "space-1",
                "dev-a",
                unwatched_space_root(dir.path()).to_string_lossy().as_ref(),
                None,
                false,
            )
            .unwrap();
        core.workspace
            .create_chat("chat-1", "space-1", None, None)
            .unwrap();
        core.sessions
            .dispatch(
                "chat-1",
                HarnessId::Mock,
                recording_request(dir.path(), "seed same generation"),
                Some("seed-user".into()),
            )
            .await
            .unwrap();
        wait_for_run_completion(&core, "chat-1").await;

        let (retry_reached, retry_release) =
            core.sessions.pause_next_resume_retry_for_test("chat-1");
        core.sessions
            .dispatch(
                "chat-1",
                HarnessId::Mock,
                recording_request(dir.path(), "old prompt must not displace"),
                Some("old-user".into()),
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), retry_reached)
            .await
            .expect("rejected resume reaches the retry pause")
            .expect("retry pause remains armed");

        core.sessions
            .dispatch(
                "chat-1",
                HarnessId::Mock,
                recording_request(dir.path(), "replacement prompt"),
                Some("replacement-user".into()),
            )
            .await
            .unwrap();
        wait_for_idle(&core, "chat-1").await;
        let replacement_entries_before = core
            .doc_host
            .open("chat-1")
            .unwrap()
            .doc()
            .read_entries()
            .unwrap();
        let replacement_last_request = core.sessions.last_request("chat-1");

        retry_release.send(()).unwrap();
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }

        let replacement_entries_after = core
            .doc_host
            .open("chat-1")
            .unwrap()
            .doc()
            .read_entries()
            .unwrap();
        let requests = requests.lock().unwrap();
        let recorded_old_prompt_count = requests
            .iter()
            .filter(|request| request.prompt == "old prompt must not displace")
            .count();
        let recorded_replacement_prompt_count = requests
            .iter()
            .filter(|request| request.prompt == "replacement prompt")
            .count();

        assert_eq!(replacement_entries_after, replacement_entries_before);
        assert_eq!(
            core.sessions.last_request("chat-1"),
            replacement_last_request
        );
        assert_eq!(
            core.sessions.session_status("chat-1").unwrap().status,
            comet_proto::SessionStatus::Idle
        );
        assert_eq!(recorded_old_prompt_count, 1);
        assert_eq!(recorded_replacement_prompt_count, 1);
    }

    /// A recreated document without a successor run still rejects the retry:
    /// generation identity, not merely live-run ownership, is the admission.
    #[tokio::test]
    async fn rejected_resume_retry_cannot_register_against_an_idle_successor_generation() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("device-id"), "dev-a").unwrap();
        let requests: RequestLog = Arc::new(Mutex::new(Vec::new()));
        let core = rejected_resume_core(dir.path(), requests.clone());
        core.workspace
            .create_space(
                "space-1",
                "dev-a",
                unwatched_space_root(dir.path()).to_string_lossy().as_ref(),
                None,
                false,
            )
            .unwrap();
        core.workspace
            .create_chat("chat-1", "space-1", None, None)
            .unwrap();
        core.sessions
            .dispatch(
                "chat-1",
                HarnessId::Mock,
                recording_request(dir.path(), "seed idle successor"),
                Some("seed-user".into()),
            )
            .await
            .unwrap();
        wait_for_run_completion(&core, "chat-1").await;

        let (retry_reached, retry_release) =
            core.sessions.pause_next_resume_retry_for_test("chat-1");
        core.sessions
            .dispatch(
                "chat-1",
                HarnessId::Mock,
                recording_request(dir.path(), "old prompt must not register"),
                Some("old-user".into()),
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), retry_reached)
            .await
            .expect("rejected resume reaches the retry pause")
            .expect("retry pause remains armed");

        let mut purge_done = core.doc_host.watch_purges();
        core.remote_rpc_service()
            .mutate(MutateParams::DeleteChat {
                chat_id: "chat-1".into(),
            })
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), purge_done.changed())
            .await
            .expect("old generation final purge settles")
            .expect("purge watch remains connected");
        core.remote_rpc_service()
            .mutate(MutateParams::CreateChat {
                chat_id: "chat-1".into(),
                space_id: "space-1".into(),
                config: None,
                branch: None,
                cwd: None,
            })
            .unwrap();

        core.sessions
            .dispatch(
                "chat-1",
                HarnessId::Mock,
                recording_request(dir.path(), "replacement fully retires"),
                Some("replacement-user".into()),
            )
            .await
            .unwrap();
        wait_for_run_completion(&core, "chat-1").await;
        let replacement_entries_before = core
            .doc_host
            .open("chat-1")
            .unwrap()
            .doc()
            .read_entries()
            .unwrap();
        let replacement_last_request = core.sessions.last_request("chat-1");

        retry_release.send(()).unwrap();
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }

        let replacement_entries_after = core
            .doc_host
            .open("chat-1")
            .unwrap()
            .doc()
            .read_entries()
            .unwrap();
        let requests = requests.lock().unwrap();
        let recorded_old_prompt_count = requests
            .iter()
            .filter(|request| request.prompt == "old prompt must not register")
            .count();
        let recorded_replacement_prompt_count = requests
            .iter()
            .filter(|request| request.prompt == "replacement fully retires")
            .count();

        assert_eq!(replacement_entries_after, replacement_entries_before);
        assert_eq!(
            core.sessions.last_request("chat-1"),
            replacement_last_request
        );
        assert_eq!(
            core.sessions.session_status("chat-1").unwrap().status,
            comet_proto::SessionStatus::Idle
        );
        assert_eq!(recorded_old_prompt_count, 1);
        assert_eq!(recorded_replacement_prompt_count, 1);
    }

    #[tokio::test]
    async fn restarted_same_id_reconciliation_retires_the_old_journal_before_dispatch() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("device-id"), "dev-a").unwrap();
        let requests: RequestLog = Arc::new(Mutex::new(Vec::new()));
        {
            let core = recording_core(dir.path(), requests.clone());
            core.workspace
                .create_space(
                    "space-1",
                    "dev-a",
                    unwatched_space_root(dir.path()).to_string_lossy().as_ref(),
                    None,
                    false,
                )
                .unwrap();
            core.workspace
                .create_chat("chat-1", "space-1", None, None)
                .unwrap();
            core.workspace.rename_chat("chat-1", "Old chat").unwrap();
            core.sessions
                .dispatch(
                    "chat-1",
                    HarnessId::Mock,
                    recording_request(dir.path(), "old generation"),
                    Some("old-user".into()),
                )
                .await
                .unwrap();
            wait_for_run_completion(&core, "chat-1").await;
            let orphan = tokio::time::timeout(Duration::from_secs(1), async {
                loop {
                    if let Some(session) = core
                        .workspace
                        .doc()
                        .read_sessions()
                        .unwrap()
                        .iter()
                        .find(|session| session.chat_id == "chat-1")
                        .cloned()
                    {
                        break session;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("old generation writes a workspace session row");
            core.workspace.delete_chat("chat-1").unwrap();
            core.workspace.record_session(&orphan);
            core.shutdown().await;
        }

        let core = recording_core(dir.path(), requests.clone());
        core.remote_rpc_service()
            .mutate(MutateParams::CreateChat {
                chat_id: "chat-1".into(),
                space_id: "space-1".into(),
                config: None,
                branch: None,
                cwd: None,
            })
            .unwrap();
        let journal = crate::RunJournal::open(dir.path().join("local-store/journals")).unwrap();
        assert!(
            journal.replay("chat-1", 0).unwrap().is_empty(),
            "reconciliation discards the deleted generation's journal before create"
        );
        assert!(
            core.workspace.doc().read_sessions().unwrap().is_empty(),
            "reconciliation retires an orphan old-generation workspace session row"
        );
        core.workspace.rename_chat("chat-1", "New chat").unwrap();
        core.sessions
            .dispatch(
                "chat-1",
                HarnessId::Mock,
                recording_request(dir.path(), "new generation"),
                Some("new-user".into()),
            )
            .await
            .unwrap();
        wait_for_run_completion(&core, "chat-1").await;

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[1].resume, None,
            "a fresh process cannot recover the deleted generation's session id"
        );
    }

    #[tokio::test]
    async fn stale_finalizer_cannot_retire_a_new_generation_session_or_journal() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("device-id"), "dev-a").unwrap();
        let requests: RequestLog = Arc::new(Mutex::new(Vec::new()));
        let core = recording_core(dir.path(), requests.clone());
        core.workspace
            .create_space(
                "space-1",
                "dev-a",
                unwatched_space_root(dir.path()).to_string_lossy().as_ref(),
                None,
                false,
            )
            .unwrap();
        core.workspace
            .create_chat("chat-1", "space-1", None, None)
            .unwrap();
        core.workspace.rename_chat("chat-1", "Old chat").unwrap();
        core.sessions
            .dispatch(
                "chat-1",
                HarnessId::Mock,
                recording_request(dir.path(), "old generation"),
                Some("old-user".into()),
            )
            .await
            .unwrap();
        wait_for_run_completion(&core, "chat-1").await;

        core.workspace.delete_chat("chat-1").unwrap();
        let stale = core.doc_host.begin_purge("chat-1").unwrap();
        assert_eq!(
            core.doc_host.finish_purge("chat-1", stale),
            PurgeFinishOutcome::Purged
        );
        let admission = core.doc_host.admit_create("chat-1", false).unwrap();
        core.workspace
            .create_chat("chat-1", "space-1", None, None)
            .unwrap();
        assert!(core.doc_host.revive_created_chat("chat-1", admission));
        core.workspace.rename_chat("chat-1", "New chat").unwrap();
        core.sessions
            .dispatch(
                "chat-1",
                HarnessId::Mock,
                recording_request(dir.path(), "new generation"),
                Some("new-user".into()),
            )
            .await
            .unwrap();
        wait_for_run_completion(&core, "chat-1").await;

        let replacement_session = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Some(session) = core
                    .workspace
                    .doc()
                    .read_sessions()
                    .unwrap()
                    .iter()
                    .find(|session| session.chat_id == "chat-1")
                    .cloned()
                {
                    break session;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("replacement generation writes a workspace session row");

        assert_eq!(
            core.doc_host.finish_purge("chat-1", stale),
            PurgeFinishOutcome::Stale
        );
        assert_eq!(
            core.workspace
                .doc()
                .read_sessions()
                .unwrap()
                .iter()
                .find(|session| session.chat_id == "chat-1"),
            Some(&replacement_session),
            "a stale finalizer cannot delete the replacement workspace session row"
        );
        core.sessions
            .dispatch(
                "chat-1",
                HarnessId::Mock,
                recording_request(dir.path(), "third generation turn"),
                Some("third-user".into()),
            )
            .await
            .unwrap();
        wait_for_run_completion(&core, "chat-1").await;

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert_eq!(
            requests[2].resume.as_deref(),
            Some("recorded-session-2"),
            "the stale old finalizer cannot clear the new generation cache or journal"
        );
        assert!(
            crate::RunJournal::open(dir.path().join("local-store/journals"))
                .unwrap()
                .replay("chat-1", 0)
                .unwrap()
                .iter()
                .any(|(_, event)| matches!(event, comet_proto::AgentEvent::SessionStarted { session_id, .. } if session_id == "recorded-session-2")),
            "the new generation journal survives the stale callback"
        );
    }

    /// Deletion marks the sidecar boundary before the workspace tombstone,
    /// then a late producer is allowed to finish so the final purge must see
    /// no exact sources left behind.
    #[tokio::test]
    async fn deleting_a_running_chat_refuses_a_late_tool_diff() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("device-id"), "dev-a").unwrap();
        let (release_tx, release_rx) = oneshot::channel();
        let registry = Arc::new(crate::registry::HarnessRegistry::new());
        registry.register(Arc::new(LateToolResultHarness {
            release: Mutex::new(Some(release_rx)),
        }));
        let core = crate::EngineCore::assemble(dir.path(), registry, HarnessId::Mock, None)
            .expect("engine core assembles");
        core.workspace
            .create_space(
                "space-1",
                "dev-a",
                unwatched_space_root(dir.path()).to_string_lossy().as_ref(),
                None,
                false,
            )
            .unwrap();
        core.workspace
            .create_chat("chat-1", "space-1", None, None)
            .unwrap();

        let pre_delete_diff = comet_proto::ToolDiff {
            path: "src/pre-delete.rs".into(),
            old_text: Some("before delete".into()),
            new_text: "after delete".into(),
        };
        let PutToolDiffOutcome::Stored {
            diff_ref: pre_delete_ref,
            ..
        } = core
            .doc_host
            .put_tool_diff("chat-1", "pre-delete-tool", &pre_delete_diff)
            .unwrap()
        else {
            panic!("pre-delete sidecar must be stored");
        };
        let mut purge_done = core.doc_host.watch_purges();
        let local_sessions = core.sessions.watch_sessions();

        let (_replay, mut live) = core.sessions.subscribe("chat-1", 0).unwrap();
        core.sessions
            .dispatch(
                "chat-1",
                HarnessId::Mock,
                comet_proto::RunRequest {
                    prompt: "change it".into(),
                    harness: None,
                    model: None,
                    reasoning: None,
                    model_options: Default::default(),
                    cwd: unwatched_space_root(dir.path())
                        .to_string_lossy()
                        .to_string(),
                    runtime_mode: comet_proto::RuntimeMode::default(),
                    sandbox: comet_proto::SandboxLevel::WorkspaceWrite,
                    attachments: Vec::new(),
                    resume: None,
                },
                Some("user-1".into()),
            )
            .await
            .unwrap();

        loop {
            let event = tokio::time::timeout(Duration::from_secs(1), live.recv())
                .await
                .expect("tool call arrives before deletion")
                .expect("live stream stays open")
                .event;
            if matches!(event, comet_proto::AgentEvent::ToolCall { .. }) {
                break;
            }
        }

        core.remote_rpc_service()
            .mutate(MutateParams::DeleteChat {
                chat_id: "chat-1".into(),
            })
            .unwrap();

        let blocked_diff = comet_proto::ToolDiff {
            path: "src/blocked.rs".into(),
            old_text: Some("before".into()),
            new_text: "after".into(),
        };
        assert!(matches!(
            core.doc_host
                .put_tool_diff("chat-1", "blocked-tool", &blocked_diff),
            Err(comet_sync::StoreError::ToolDiffPurged)
        ));

        // Bypass the façade to pin final cleanup independently: a writer that
        // passed the gate just before deletion must be removed after settle.
        let straggler_diff = comet_proto::ToolDiff {
            path: "src/straggler.rs".into(),
            old_text: Some("before straggler".into()),
            new_text: "after straggler".into(),
        };
        let straggler_ref = straggler_diff.diff_ref().unwrap();
        let store = DocsStore::open(dir.path().join("local-store")).unwrap();
        assert!(matches!(
            store.put_tool_diff("chat-1", "straggler-tool", &straggler_diff),
            Ok(PutToolDiffOutcome::Stored { .. })
        ));

        release_tx.send(()).unwrap();
        let late_diff = comet_proto::ToolDiff {
            path: "src/lib.rs".into(),
            old_text: Some("TASK6_DELETE_POISON_OLD".into()),
            new_text: "TASK6_DELETE_POISON_NEW".into(),
        };
        let late_stats = vec![late_diff.stat()];
        let late_ref = late_diff.diff_ref().unwrap();
        let mut saw_late_result = false;
        loop {
            let event = tokio::time::timeout(Duration::from_secs(1), live.recv())
                .await
                .expect("late result and terminal event arrive")
                .expect("live stream stays open")
                .event;
            if let comet_proto::AgentEvent::ToolResult {
                diff,
                diff_ref,
                diff_stats,
                ..
            } = &event
            {
                saw_late_result = true;
                assert!(diff.is_none(), "late event must not retain exact sources");
                assert!(
                    diff_ref.is_none(),
                    "late event must not retain a stale reference"
                );
                assert_eq!(diff_stats.as_deref(), Some(late_stats.as_slice()));
            }
            if matches!(event, comet_proto::AgentEvent::Done { .. }) {
                break;
            }
        }
        assert!(saw_late_result, "the straggling ToolResult was prepared");

        tokio::time::timeout(Duration::from_secs(1), purge_done.changed())
            .await
            .expect("final purge completes after the run settles")
            .expect("purge watch stays connected");
        assert!(
            local_sessions.borrow().is_empty(),
            "final cleanup removes the retired run from the local session watch"
        );
        assert!(
            core.workspace.doc().read_sessions().unwrap().is_empty(),
            "terminal status cannot recreate a workspace row after DeleteChat"
        );
        assert_eq!(
            store
                .read_tool_diff("chat-1", "pre-delete-tool", &pre_delete_ref)
                .unwrap(),
            None,
            "final purge removes sidecars that predate deletion"
        );
        assert_eq!(
            store
                .read_tool_diff("chat-1", "late-tool", &late_ref)
                .unwrap(),
            None,
            "final purge leaves no late-result sidecar"
        );
        assert_eq!(
            store
                .read_tool_diff("chat-1", "straggler-tool", &straggler_ref)
                .unwrap(),
            None,
            "final purge removes the straggler sidecar"
        );
    }

    #[tokio::test]
    async fn terminal_handoff_cannot_overwrite_a_recreated_chat_session_row() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("device-id"), "dev-a").unwrap();
        let (old_release, old_wait) = oneshot::channel();
        let registry = Arc::new(crate::registry::HarnessRegistry::new());
        registry.register(Arc::new(TerminalHandoffHarness {
            old_release: Mutex::new(Some(old_wait)),
        }));
        let core = crate::EngineCore::assemble(dir.path(), registry, HarnessId::Mock, None)
            .expect("engine core assembles");
        core.workspace
            .create_space(
                "space-1",
                "dev-a",
                unwatched_space_root(dir.path()).to_string_lossy().as_ref(),
                None,
                false,
            )
            .unwrap();
        core.workspace
            .create_chat("chat-1", "space-1", None, None)
            .unwrap();
        let (handoff_reached, handoff_release, handoff_settled) =
            core.sessions.pause_next_terminal_handoff_for_test("chat-1");

        core.sessions
            .dispatch(
                "chat-1",
                HarnessId::Mock,
                recording_request(dir.path(), "old generation"),
                Some("old-user".into()),
            )
            .await
            .unwrap();
        let mut purge_done = core.doc_host.watch_purges();
        core.remote_rpc_service()
            .mutate(MutateParams::DeleteChat {
                chat_id: "chat-1".into(),
            })
            .unwrap();
        old_release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), handoff_reached)
            .await
            .expect("old terminal task reaches the handoff")
            .expect("handoff remains armed");
        tokio::time::timeout(Duration::from_secs(1), purge_done.changed())
            .await
            .expect("the released run pin lets final purge settle")
            .expect("purge watch remains connected");

        core.remote_rpc_service()
            .mutate(MutateParams::CreateChat {
                chat_id: "chat-1".into(),
                space_id: "space-1".into(),
                config: None,
                branch: None,
                cwd: None,
            })
            .unwrap();
        core.sessions
            .dispatch(
                "chat-1",
                HarnessId::Mock,
                recording_request(dir.path(), "replacement"),
                Some("new-user".into()),
            )
            .await
            .unwrap();
        wait_for_run_completion(&core, "chat-1").await;
        tokio::time::timeout(Duration::from_secs(1), async {
            while !matches!(
                core.sessions.session_status("chat-1"),
                Some(comet_proto::Session {
                    status: comet_proto::SessionStatus::Idle,
                    ..
                })
            ) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("replacement terminal status lands before the old task resumes");

        handoff_release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), handoff_settled)
            .await
            .expect("old terminal handoff settles")
            .expect("old task acknowledges the handoff");
        assert!(matches!(
            core.sessions.session_status("chat-1"),
            Some(comet_proto::Session {
                status: comet_proto::SessionStatus::Idle,
                ..
            })
        ));
        assert!(matches!(
            core.workspace
                .doc()
                .read_sessions()
                .unwrap()
                .iter()
                .find(|session| session.chat_id == "chat-1"),
            Some(comet_proto::Session {
                status: comet_proto::SessionStatus::Idle,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn startup_error_terminal_handoff_cannot_overwrite_a_recreated_chat_session_row() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("device-id"), "dev-a").unwrap();
        let (old_release, old_wait) = oneshot::channel();
        let registry = Arc::new(crate::registry::HarnessRegistry::new());
        registry.register(Arc::new(StartupErrorTerminalHandoffHarness {
            old_release: Mutex::new(Some(old_wait)),
        }));
        let core = crate::EngineCore::assemble(dir.path(), registry, HarnessId::Mock, None)
            .expect("engine core assembles");
        core.workspace
            .create_space(
                "space-1",
                "dev-a",
                unwatched_space_root(dir.path()).to_string_lossy().as_ref(),
                None,
                false,
            )
            .unwrap();
        core.workspace
            .create_chat("chat-1", "space-1", None, None)
            .unwrap();
        let (handoff_reached, handoff_release, handoff_settled) =
            core.sessions.pause_next_terminal_handoff_for_test("chat-1");

        core.sessions
            .dispatch(
                "chat-1",
                HarnessId::Mock,
                recording_request(dir.path(), "old generation"),
                Some("old-user".into()),
            )
            .await
            .unwrap();
        let mut purge_done = core.doc_host.watch_purges();
        core.remote_rpc_service()
            .mutate(MutateParams::DeleteChat {
                chat_id: "chat-1".into(),
            })
            .unwrap();
        old_release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), handoff_reached)
            .await
            .expect("old terminal task reaches the handoff")
            .expect("handoff remains armed");
        tokio::time::timeout(Duration::from_secs(1), purge_done.changed())
            .await
            .expect("the released run pin lets final purge settle")
            .expect("purge watch remains connected");

        core.remote_rpc_service()
            .mutate(MutateParams::CreateChat {
                chat_id: "chat-1".into(),
                space_id: "space-1".into(),
                config: None,
                branch: None,
                cwd: None,
            })
            .unwrap();
        core.sessions
            .dispatch(
                "chat-1",
                HarnessId::Mock,
                recording_request(dir.path(), "replacement"),
                Some("new-user".into()),
            )
            .await
            .unwrap();
        wait_for_run_completion(&core, "chat-1").await;
        tokio::time::timeout(Duration::from_secs(1), async {
            while !matches!(
                core.sessions.session_status("chat-1"),
                Some(comet_proto::Session {
                    status: comet_proto::SessionStatus::Idle,
                    ..
                })
            ) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("replacement terminal status lands before the old task resumes");

        handoff_release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), handoff_settled)
            .await
            .expect("old terminal handoff settles")
            .expect("old task acknowledges the handoff");
        assert!(matches!(
            core.sessions.session_status("chat-1"),
            Some(comet_proto::Session {
                status: comet_proto::SessionStatus::Idle,
                ..
            })
        ));
        assert!(matches!(
            core.workspace
                .doc()
                .read_sessions()
                .unwrap()
                .iter()
                .find(|session| session.chat_id == "chat-1"),
            Some(comet_proto::Session {
                status: comet_proto::SessionStatus::Idle,
                ..
            })
        ));
    }

    /// A successful DeleteChat response is a durable privacy boundary: if the
    /// process exits while the interrupted run is still blocked, reopening the
    /// local store must not recover this generation's exact sources.
    #[tokio::test]
    async fn delete_chat_cleans_exact_sources_before_the_blocked_finalizer() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("device-id"), "dev-a").unwrap();
        let (release_tx, release_rx) = oneshot::channel();
        let registry = Arc::new(crate::registry::HarnessRegistry::new());
        registry.register(Arc::new(LateToolResultHarness {
            release: Mutex::new(Some(release_rx)),
        }));
        let core = crate::EngineCore::assemble(dir.path(), registry, HarnessId::Mock, None)
            .expect("engine core assembles");
        core.workspace
            .create_space(
                "space-1",
                "dev-a",
                unwatched_space_root(dir.path()).to_string_lossy().as_ref(),
                None,
                false,
            )
            .unwrap();
        core.workspace
            .create_chat("chat-1", "space-1", None, None)
            .unwrap();

        let (_replay, mut live) = core.sessions.subscribe("chat-1", 0).unwrap();
        core.sessions
            .dispatch(
                "chat-1",
                HarnessId::Mock,
                comet_proto::RunRequest {
                    prompt: "change it".into(),
                    harness: None,
                    model: None,
                    reasoning: None,
                    model_options: Default::default(),
                    cwd: unwatched_space_root(dir.path())
                        .to_string_lossy()
                        .to_string(),
                    runtime_mode: comet_proto::RuntimeMode::default(),
                    sandbox: comet_proto::SandboxLevel::WorkspaceWrite,
                    attachments: Vec::new(),
                    resume: None,
                },
                Some("user-1".into()),
            )
            .await
            .unwrap();
        loop {
            let event = tokio::time::timeout(Duration::from_secs(1), live.recv())
                .await
                .expect("tool call arrives before deletion")
                .expect("live stream stays open")
                .event;
            if matches!(event, comet_proto::AgentEvent::ToolCall { .. }) {
                break;
            }
        }

        let store = DocsStore::open(dir.path().join("local-store")).unwrap();
        store
            .save_snapshot("chat-1", b"TASK6_RESTART_SNAPSHOT")
            .unwrap();
        let diff = comet_proto::ToolDiff {
            path: "src/restart-window.rs".into(),
            old_text: Some("TASK6_RESTART_OLD".into()),
            new_text: "TASK6_RESTART_NEW".into(),
        };
        let PutToolDiffOutcome::Stored { diff_ref, .. } = core
            .doc_host
            .put_tool_diff("chat-1", "restart-window-tool", &diff)
            .unwrap()
        else {
            panic!("restart-window sidecar is stored before deletion");
        };
        let pending_handle = core.doc_host.open("chat-1").unwrap();
        pending_handle
            .write_user_message(
                "pending-snapshot-message",
                "TASK6_PENDING_DEBOUNCE_SNAPSHOT",
                1,
            )
            .unwrap();
        tokio::task::yield_now().await;
        let mut purge_done = core.doc_host.watch_purges();

        core.remote_rpc_service()
            .mutate(MutateParams::DeleteChat {
                chat_id: "chat-1".into(),
            })
            .expect("delete reports success before the blocked run settles");

        // Both shutdown and the already-scheduled debounce still see the old
        // open handle while the finalizer is blocked. Neither may recreate the
        // deleted generation's snapshot after the synchronous cleanup pass.
        core.doc_host.flush_all();
        tokio::time::sleep(Duration::from_millis(1_100)).await;

        drop(store);
        let restarted_store = DocsStore::open(dir.path().join("local-store")).unwrap();
        assert_eq!(
            restarted_store.load_snapshot("chat-1").unwrap(),
            None,
            "the pre-delete snapshot is gone before the finalizer is released"
        );
        assert_eq!(
            restarted_store
                .read_tool_diff("chat-1", "restart-window-tool", &diff_ref)
                .unwrap(),
            None,
            "the pre-delete exact sidecar is gone before restart"
        );

        release_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), purge_done.changed())
            .await
            .expect("final retry completes after the run settles")
            .expect("purge watch stays connected");
    }

    /// A restarted engine has no in-memory purge token. Before reusing a
    /// workspace-absent id, CreateChat must scrub any orphan left by an older
    /// process, while an idempotent create of a live row keeps its artifacts.
    #[tokio::test]
    async fn restarted_same_id_create_scrubs_orphans_before_reuse() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("device-id"), "dev-a").unwrap();
        let orphan_diff = comet_proto::ToolDiff {
            path: "src/orphaned-generation.rs".into(),
            old_text: Some("TASK6_ORPHAN_OLD".into()),
            new_text: "TASK6_ORPHAN_NEW".into(),
        };
        let orphan_ref = {
            let store = DocsStore::open(dir.path().join("local-store")).unwrap();
            store
                .save_snapshot("chat-1", b"TASK6_ORPHAN_SNAPSHOT")
                .unwrap();
            let PutToolDiffOutcome::Stored { diff_ref, .. } = store
                .put_tool_diff("chat-1", "orphan-tool", &orphan_diff)
                .unwrap()
            else {
                panic!("the prior process stores an orphan sidecar");
            };
            diff_ref
        };

        let core = engine_core(dir.path());
        core.workspace
            .create_space(
                "space-1",
                "dev-a",
                unwatched_space_root(dir.path()).to_string_lossy().as_ref(),
                None,
                false,
            )
            .unwrap();
        let rpc = core.remote_rpc_service();
        rpc.mutate(MutateParams::CreateChat {
            chat_id: "chat-1".into(),
            space_id: "space-1".into(),
            config: None,
            branch: None,
            cwd: None,
        })
        .expect("a clean generation may claim the reused id");

        let store = DocsStore::open(dir.path().join("local-store")).unwrap();
        assert_eq!(
            store.load_snapshot("chat-1").unwrap(),
            None,
            "the new generation cannot inherit an orphan snapshot"
        );
        assert_eq!(
            store
                .read_tool_diff("chat-1", "orphan-tool", &orphan_ref)
                .unwrap(),
            None,
            "the new generation cannot inherit an orphan exact-source sidecar"
        );

        store
            .save_snapshot("chat-1", b"TASK6_LIVE_GENERATION_SNAPSHOT")
            .unwrap();
        let live_diff = comet_proto::ToolDiff {
            path: "src/live-generation.rs".into(),
            old_text: Some("before".into()),
            new_text: "after".into(),
        };
        let PutToolDiffOutcome::Stored { diff_ref, .. } = core
            .doc_host
            .put_tool_diff("chat-1", "live-tool", &live_diff)
            .unwrap()
        else {
            panic!("the live generation stores its sidecar");
        };

        rpc.mutate(MutateParams::CreateChat {
            chat_id: "chat-1".into(),
            space_id: "space-1".into(),
            config: None,
            branch: None,
            cwd: None,
        })
        .expect("an idempotent create keeps the live generation");
        assert_eq!(
            store.load_snapshot("chat-1").unwrap(),
            Some(b"TASK6_LIVE_GENERATION_SNAPSHOT".to_vec())
        );
        assert_eq!(
            store
                .read_tool_diff("chat-1", "live-tool", &diff_ref)
                .unwrap(),
            Some(live_diff)
        );
    }

    #[tokio::test]
    async fn late_dispatch_cannot_register_after_same_id_recreation() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("device-id"), "dev-a").unwrap();
        let started = Arc::new(AtomicBool::new(false));
        let registry = Arc::new(crate::registry::HarnessRegistry::new());
        registry.register(Arc::new(RegistrationProbeHarness {
            started: started.clone(),
        }));
        let core = crate::EngineCore::assemble(dir.path(), registry, HarnessId::Mock, None)
            .expect("engine core assembles");
        core.workspace
            .create_space(
                "space-1",
                "dev-a",
                unwatched_space_root(dir.path()).to_string_lossy().as_ref(),
                None,
                false,
            )
            .unwrap();
        core.workspace
            .create_chat("chat-1", "space-1", None, None)
            .unwrap();

        let (stale_rx, release_registration) =
            core.sessions.pause_next_run_registration_for_test("chat-1");
        let sessions = core.sessions.clone();
        let request = registration_request(dir.path());
        let dispatch = tokio::spawn(async move {
            sessions
                .dispatch(
                    "chat-1",
                    HarnessId::Mock,
                    request,
                    Some("old-user-message".into()),
                )
                .await
        });
        let stale_handle = tokio::time::timeout(Duration::from_secs(1), stale_rx)
            .await
            .expect("dispatch reaches registration")
            .expect("registration pause sends the old handle");

        let mut purge_done = core.doc_host.watch_purges();
        let rpc = core.remote_rpc_service();
        rpc.mutate(MutateParams::DeleteChat {
            chat_id: "chat-1".into(),
        })
        .expect("the unregistered dispatch does not delay deletion");
        tokio::time::timeout(Duration::from_secs(1), purge_done.changed())
            .await
            .expect("final purge completes")
            .expect("purge watch stays connected");
        rpc.mutate(MutateParams::CreateChat {
            chat_id: "chat-1".into(),
            space_id: "space-1".into(),
            config: None,
            branch: None,
            cwd: None,
        })
        .expect("the cleaned id is intentionally reused");
        let current_handle = core.doc_host.open("chat-1").unwrap();
        assert!(!Arc::ptr_eq(&stale_handle, &current_handle));
        release_registration.send(()).unwrap();

        let result = dispatch.await.expect("dispatch task joins");
        assert!(matches!(
            result,
            Err(crate::EngineError::ChatCleanupPendingRetry)
        ));
        assert!(!core.sessions.has_live_run("chat-1"));
        assert!(!started.load(Ordering::SeqCst));
        assert!(core.sessions.last_request("chat-1").is_none());

        let old_diff = comet_proto::ToolDiff {
            path: "src/old-registration.rs".into(),
            old_text: Some("OLD_REGISTRATION_SOURCE".into()),
            new_text: "NEW_REGISTRATION_SOURCE".into(),
        };
        let old_ref = old_diff.diff_ref().unwrap();
        let store = DocsStore::open(dir.path().join("local-store")).unwrap();
        assert_eq!(
            store
                .read_tool_diff("chat-1", "old-registration-tool", &old_ref)
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn run_registration_refuses_a_purging_chat_before_run_map_insertion() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("device-id"), "dev-a").unwrap();
        let started = Arc::new(AtomicBool::new(false));
        let registry = Arc::new(crate::registry::HarnessRegistry::new());
        registry.register(Arc::new(RegistrationProbeHarness {
            started: started.clone(),
        }));
        let core = crate::EngineCore::assemble(dir.path(), registry, HarnessId::Mock, None)
            .expect("engine core assembles");
        core.workspace
            .create_space(
                "space-1",
                "dev-a",
                unwatched_space_root(dir.path()).to_string_lossy().as_ref(),
                None,
                false,
            )
            .unwrap();
        core.workspace
            .create_chat("chat-1", "space-1", None, None)
            .unwrap();

        let (stale_rx, release_registration) =
            core.sessions.pause_next_run_registration_for_test("chat-1");
        let sessions = core.sessions.clone();
        let request = registration_request(dir.path());
        let dispatch = tokio::spawn(async move {
            sessions
                .dispatch(
                    "chat-1",
                    HarnessId::Mock,
                    request,
                    Some("old-user-message".into()),
                )
                .await
        });
        let _stale_handle = tokio::time::timeout(Duration::from_secs(1), stale_rx)
            .await
            .expect("dispatch reaches registration")
            .expect("registration pause sends the old handle");

        let purge = core.doc_host.begin_purge("chat-1").unwrap();
        release_registration.send(()).unwrap();

        let result = dispatch.await.expect("dispatch task joins");
        assert!(matches!(
            result,
            Err(crate::EngineError::ChatCleanupPendingRetry)
        ));
        assert!(!core.sessions.has_live_run("chat-1"));
        assert!(!started.load(Ordering::SeqCst));
        assert!(core.sessions.last_request("chat-1").is_none());
        assert_eq!(
            core.doc_host.finish_purge("chat-1", purge),
            crate::doc_host::PurgeFinishOutcome::Purged
        );
    }

    /// A token-owned purge must not let a delayed old run admit a new generation.
    #[tokio::test]
    async fn workspace_absent_live_run_without_handle_refuses_reconciliation() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("device-id"), "dev-a").unwrap();
        let (release_tx, release_rx) = oneshot::channel();
        let registry = Arc::new(crate::registry::HarnessRegistry::new());
        registry.register(Arc::new(LateToolResultHarness {
            release: Mutex::new(Some(release_rx)),
        }));
        let core = crate::EngineCore::assemble(dir.path(), registry, HarnessId::Mock, None)
            .expect("engine core assembles");
        core.sessions
            .dispatch(
                "chat-1",
                HarnessId::Mock,
                comet_proto::RunRequest {
                    prompt: "change it".into(),
                    harness: None,
                    model: None,
                    reasoning: None,
                    model_options: Default::default(),
                    cwd: unwatched_space_root(dir.path())
                        .to_string_lossy()
                        .to_string(),
                    runtime_mode: comet_proto::RuntimeMode::default(),
                    sandbox: comet_proto::SandboxLevel::WorkspaceWrite,
                    attachments: Vec::new(),
                    resume: None,
                },
                Some("user-1".into()),
            )
            .await
            .unwrap();

        // A final purge may not retire session state while the delayed run is
        // still live; its matching token must remain Purging for a retry.
        let purge = core.doc_host.begin_purge("chat-1").unwrap();
        assert_eq!(
            core.doc_host.finish_purge("chat-1", purge),
            PurgeFinishOutcome::PendingRetry
        );
        assert!(
            core.doc_host.admit_create("chat-1", false).is_err(),
            "a Purging lifecycle does not override a still-live run owner"
        );

        release_tx.send(()).unwrap();
        wait_for_run_to_retire(&core, "chat-1").await;

        assert_eq!(
            core.doc_host.finish_purge("chat-1", purge),
            PurgeFinishOutcome::Purged
        );

        let CreateAdmission::Revive(admitted) = core
            .doc_host
            .admit_create("chat-1", false)
            .expect("the retired run permits the clean Purged token")
        else {
            panic!("expected a Purged-token revival");
        };
        assert_eq!(admitted, purge);
        assert!(
            core.doc_host
                .revive_created_chat("chat-1", CreateAdmission::Revive(admitted))
        );
    }

    /// A reconciliation retry must preserve artifacts until its old run retires.
    #[tokio::test]
    async fn reconciling_retry_refuses_a_live_run_before_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("device-id"), "dev-a").unwrap();
        let (release_tx, release_rx) = oneshot::channel();
        let registry = Arc::new(crate::registry::HarnessRegistry::new());
        registry.register(Arc::new(LateToolResultHarness {
            release: Mutex::new(Some(release_rx)),
        }));
        let core = crate::EngineCore::assemble(dir.path(), registry, HarnessId::Mock, None)
            .expect("engine core assembles");
        core.sessions
            .dispatch(
                "chat-1",
                HarnessId::Mock,
                comet_proto::RunRequest {
                    prompt: "change it".into(),
                    harness: None,
                    model: None,
                    reasoning: None,
                    model_options: Default::default(),
                    cwd: unwatched_space_root(dir.path())
                        .to_string_lossy()
                        .to_string(),
                    runtime_mode: comet_proto::RuntimeMode::default(),
                    sandbox: comet_proto::SandboxLevel::WorkspaceWrite,
                    attachments: Vec::new(),
                    resume: None,
                },
                Some("user-1".into()),
            )
            .await
            .unwrap();

        let purge = core.doc_host.begin_purge("chat-1").unwrap();
        assert_eq!(
            core.doc_host.finish_purge("chat-1", purge),
            PurgeFinishOutcome::PendingRetry
        );
        // This is the fresh-process lifecycle shape the test needs: a
        // reconciliation token can coexist with an old run only until
        // admission observes the owner and refuses cleanup.
        crate::doc_host::restore_reconciling_for_test(&core.doc_host, "chat-1", purge);

        let store = DocsStore::open(dir.path().join("local-store")).unwrap();
        store
            .save_snapshot("chat-1", b"TASK6_RECONCILING_OWNER_SNAPSHOT")
            .unwrap();
        let diff = comet_proto::ToolDiff {
            path: "src/reconciling-owner.rs".into(),
            old_text: Some("TASK6_RECONCILING_OWNER_OLD".into()),
            new_text: "TASK6_RECONCILING_OWNER_NEW".into(),
        };
        let PutToolDiffOutcome::Stored { diff_ref, .. } = store
            .put_tool_diff("chat-1", "reconciling-owner-tool", &diff)
            .unwrap()
        else {
            panic!("seeded sidecar is stored");
        };

        assert!(core.doc_host.admit_create("chat-1", false).is_err());
        assert_eq!(
            store.load_snapshot("chat-1").unwrap(),
            Some(b"TASK6_RECONCILING_OWNER_SNAPSHOT".to_vec())
        );
        assert_eq!(
            store
                .read_tool_diff("chat-1", "reconciling-owner-tool", &diff_ref)
                .unwrap(),
            Some(diff.clone())
        );

        release_tx.send(()).unwrap();
        wait_for_run_to_retire(&core, "chat-1").await;
        let CreateAdmission::Reconcile(admitted) = core
            .doc_host
            .admit_create("chat-1", false)
            .expect("retirement permits reconciliation cleanup")
        else {
            panic!("expected the existing reconciliation token");
        };
        assert_eq!(admitted, purge);
        assert_eq!(store.load_snapshot("chat-1").unwrap(), None);
        assert_eq!(
            store
                .read_tool_diff("chat-1", "reconciling-owner-tool", &diff_ref)
                .unwrap(),
            None
        );
    }

    /// An optimistic retry of a live row belongs to its current generation.
    #[tokio::test]
    async fn live_row_create_stays_idempotent_with_an_active_run_and_handle() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("device-id"), "dev-a").unwrap();
        let (release_tx, release_rx) = oneshot::channel();
        let registry = Arc::new(crate::registry::HarnessRegistry::new());
        registry.register(Arc::new(LateToolResultHarness {
            release: Mutex::new(Some(release_rx)),
        }));
        let core = crate::EngineCore::assemble(dir.path(), registry, HarnessId::Mock, None)
            .expect("engine core assembles");
        core.workspace
            .create_space(
                "space-1",
                "dev-a",
                unwatched_space_root(dir.path()).to_string_lossy().as_ref(),
                None,
                false,
            )
            .unwrap();
        core.workspace
            .create_chat("chat-1", "space-1", None, None)
            .unwrap();
        core.sessions
            .dispatch(
                "chat-1",
                HarnessId::Mock,
                comet_proto::RunRequest {
                    prompt: "change it".into(),
                    harness: None,
                    model: None,
                    reasoning: None,
                    model_options: Default::default(),
                    cwd: unwatched_space_root(dir.path())
                        .to_string_lossy()
                        .to_string(),
                    runtime_mode: comet_proto::RuntimeMode::default(),
                    sandbox: comet_proto::SandboxLevel::WorkspaceWrite,
                    attachments: Vec::new(),
                    resume: None,
                },
                Some("user-1".into()),
            )
            .await
            .unwrap();

        assert!(core.sessions.has_live_run("chat-1"));
        core.remote_rpc_service()
            .mutate(MutateParams::CreateChat {
                chat_id: "chat-1".into(),
                space_id: "space-1".into(),
                config: None,
                branch: None,
                cwd: None,
            })
            .expect("an optimistic retry of the live row remains idempotent");
        assert!(core.doc_host.open("chat-1").is_ok());

        release_tx.send(()).unwrap();
        wait_for_run_to_retire(&core, "chat-1").await;
    }

    /// Reconciliation owns the id from cleanup through the injected workspace
    /// create. All production refill paths race inside that closure and must be
    /// refused until its exact token is claimed.
    #[tokio::test]
    async fn reconciliation_blocks_open_snapshot_and_sidecar_refill_until_create_claim() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("device-id"), "dev-a").unwrap();
        let core = engine_core(dir.path());
        core.workspace
            .create_space(
                "space-1",
                "dev-a",
                unwatched_space_root(dir.path()).to_string_lossy().as_ref(),
                None,
                false,
            )
            .unwrap();
        let stale_handle = core.doc_host.open("chat-1").unwrap();
        assert!(
            crate::doc_host::detach_handle_for_reconciliation_test(&core.doc_host, "chat-1")
                .is_some()
        );
        let store = DocsStore::open(dir.path().join("local-store")).unwrap();
        store
            .save_snapshot("chat-1", b"TASK6_RECONCILE_ORPHAN_SNAPSHOT")
            .unwrap();
        let orphan_diff = comet_proto::ToolDiff {
            path: "src/reconcile-orphan.rs".into(),
            old_text: Some("TASK6_RECONCILE_ORPHAN_OLD".into()),
            new_text: "TASK6_RECONCILE_ORPHAN_NEW".into(),
        };
        let PutToolDiffOutcome::Stored {
            diff_ref: orphan_ref,
            ..
        } = store
            .put_tool_diff("chat-1", "orphan-tool", &orphan_diff)
            .unwrap()
        else {
            panic!("the old generation seeds its exact sidecar");
        };
        let refill_diff = comet_proto::ToolDiff {
            path: "src/reconcile-refill.rs".into(),
            old_text: Some("TASK6_RECONCILE_REFILL_OLD".into()),
            new_text: "TASK6_RECONCILE_REFILL_NEW".into(),
        };

        create_chat_with_lifecycle(
            &core.doc_host,
            "chat-1",
            false,
            || {
                stale_handle
                    .write_user_message("stale-message", "TASK6_RECONCILE_REFILL_DOC", 1)
                    .unwrap();
                let open_host = core.doc_host.clone();
                let save_host = core.doc_host.clone();
                let sidecar_host = core.doc_host.clone();
                let runtime = tokio::runtime::Handle::current();
                let (open_result, sidecar_result) = std::thread::scope(|scope| {
                    let open = scope.spawn(move || {
                        let _runtime = runtime.enter();
                        open_host.open("chat-1")
                    });
                    let save = scope.spawn(|| {
                        crate::doc_host::save_snapshot_for_reconciliation_test(
                            &save_host,
                            &stale_handle,
                        );
                    });
                    let sidecar = scope.spawn(|| {
                        sidecar_host.put_tool_diff("chat-1", "refill-tool", &refill_diff)
                    });
                    save.join().unwrap();
                    (open.join().unwrap(), sidecar.join().unwrap())
                });
                assert!(matches!(
                    open_result,
                    Err(crate::EngineError::ChatCleanupPendingRetry)
                ));
                assert!(matches!(
                    sidecar_result,
                    Err(comet_sync::StoreError::ToolDiffPurged)
                ));
                assert_eq!(store.load_snapshot("chat-1").unwrap(), None);
                assert_eq!(
                    store
                        .read_tool_diff("chat-1", "orphan-tool", &orphan_ref)
                        .unwrap(),
                    None
                );
                core.workspace
                    .create_chat("chat-1", "space-1", None, None)
                    .map(drop)
            },
            || Ok(()),
        )
        .expect("the matching reconciliation token admits the clean generation");
        let current_handle = core.doc_host.open("chat-1").unwrap();
        crate::doc_host::save_snapshot_for_reconciliation_test(&core.doc_host, &stale_handle);
        tokio::time::sleep(Duration::from_millis(1_100)).await;
        assert_eq!(
            store.load_snapshot("chat-1").unwrap(),
            None,
            "forced and debounced saves from the detached handle stay rejected after claim"
        );

        current_handle
            .write_user_message("current-flush", "CURRENT_FLUSH", 2)
            .unwrap();
        core.doc_host.flush_all();
        let ids = stored_message_ids(&store, "chat-1");
        assert!(ids.iter().any(|id| id == "current-flush"));
        assert!(!ids.iter().any(|id| id == "stale-message"));

        current_handle
            .write_user_message("current-debounce", "CURRENT_DEBOUNCE", 3)
            .unwrap();
        tokio::time::sleep(Duration::from_millis(1_100)).await;
        let ids = stored_message_ids(&store, "chat-1");
        assert!(ids.iter().any(|id| id == "current-flush"));
        assert!(ids.iter().any(|id| id == "current-debounce"));
        assert!(!ids.iter().any(|id| id == "stale-message"));

        assert!(matches!(
            core.doc_host
                .put_tool_diff("chat-1", "new-tool", &refill_diff),
            Ok(PutToolDiffOutcome::Stored { .. })
        ));
    }

    /// A failed workspace create cannot drop reconciliation admission. The same
    /// token remains non-writable and a later retry cleans and claims it.
    #[tokio::test]
    async fn create_failure_keeps_reconciliation_closed_until_clean_retry() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("device-id"), "dev-a").unwrap();
        let core = engine_core(dir.path());
        core.workspace
            .create_space(
                "space-1",
                "dev-a",
                unwatched_space_root(dir.path()).to_string_lossy().as_ref(),
                None,
                false,
            )
            .unwrap();
        let store = DocsStore::open(dir.path().join("local-store")).unwrap();
        store
            .save_snapshot("chat-1", b"TASK6_FAILED_CREATE_ORPHAN")
            .unwrap();
        let old_diff = comet_proto::ToolDiff {
            path: "src/failed-create-old.rs".into(),
            old_text: Some("TASK6_FAILED_CREATE_OLD".into()),
            new_text: "TASK6_FAILED_CREATE_NEW".into(),
        };
        let PutToolDiffOutcome::Stored { diff_ref, .. } = store
            .put_tool_diff("chat-1", "old-tool", &old_diff)
            .unwrap()
        else {
            panic!("the failed create starts with an orphan sidecar");
        };

        let error = create_chat_with_lifecycle(
            &core.doc_host,
            "chat-1",
            false,
            || Err(crate::EngineError::Other("injected create failure".into())),
            || Ok(()),
        )
        .expect_err("the injected workspace create fails");
        assert!(matches!(error, crate::EngineError::Other(_)));
        assert!(matches!(
            core.doc_host.open("chat-1"),
            Err(crate::EngineError::ChatCleanupPendingRetry)
        ));
        assert!(matches!(
            core.doc_host
                .put_tool_diff("chat-1", "blocked-tool", &old_diff),
            Err(comet_sync::StoreError::ToolDiffPurged)
        ));

        create_chat_with_lifecycle(
            &core.doc_host,
            "chat-1",
            false,
            || {
                core.workspace
                    .create_chat("chat-1", "space-1", None, None)
                    .map(drop)
            },
            || Ok(()),
        )
        .expect("the later retry claims the same clean reconciliation");
        assert_eq!(store.load_snapshot("chat-1").unwrap(), None);
        assert_eq!(
            store
                .read_tool_diff("chat-1", "old-tool", &diff_ref)
                .unwrap(),
            None
        );
        assert!(core.doc_host.open("chat-1").is_ok());
        assert!(matches!(
            core.doc_host.put_tool_diff("chat-1", "new-tool", &old_diff),
            Ok(PutToolDiffOutcome::Stored { .. })
        ));
    }

    /// The first interrupted chat can hold DeleteSpace's sequential finalizer
    /// indefinitely. A later chat's artifacts must already be absent from a
    /// reopened store when the successful space deletion returns.
    #[tokio::test]
    async fn delete_space_cleans_later_chat_before_the_blocked_finalizer() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("device-id"), "dev-a").unwrap();
        let (release_tx, release_rx) = oneshot::channel();
        let registry = Arc::new(crate::registry::HarnessRegistry::new());
        registry.register(Arc::new(LateToolResultHarness {
            release: Mutex::new(Some(release_rx)),
        }));
        let core = crate::EngineCore::assemble(dir.path(), registry, HarnessId::Mock, None)
            .expect("engine core assembles");
        core.workspace
            .create_space(
                "space-1",
                "dev-a",
                unwatched_space_root(dir.path()).to_string_lossy().as_ref(),
                None,
                false,
            )
            .unwrap();
        for chat_id in ["chat-a", "chat-b"] {
            core.workspace
                .create_chat(chat_id, "space-1", None, None)
                .unwrap();
        }

        let (_replay, mut live) = core.sessions.subscribe("chat-a", 0).unwrap();
        core.sessions
            .dispatch(
                "chat-a",
                HarnessId::Mock,
                comet_proto::RunRequest {
                    prompt: "change it".into(),
                    harness: None,
                    model: None,
                    reasoning: None,
                    model_options: Default::default(),
                    cwd: unwatched_space_root(dir.path())
                        .to_string_lossy()
                        .to_string(),
                    runtime_mode: comet_proto::RuntimeMode::default(),
                    sandbox: comet_proto::SandboxLevel::WorkspaceWrite,
                    attachments: Vec::new(),
                    resume: None,
                },
                Some("user-a".into()),
            )
            .await
            .unwrap();
        loop {
            let event = tokio::time::timeout(Duration::from_secs(1), live.recv())
                .await
                .expect("tool call arrives before deletion")
                .expect("live stream stays open")
                .event;
            if matches!(event, comet_proto::AgentEvent::ToolCall { .. }) {
                break;
            }
        }

        let store = DocsStore::open(dir.path().join("local-store")).unwrap();
        store
            .save_snapshot("chat-b", b"TASK6_SPACE_RESTART_SNAPSHOT")
            .unwrap();
        let diff = comet_proto::ToolDiff {
            path: "src/later-space-chat.rs".into(),
            old_text: Some("TASK6_SPACE_RESTART_OLD".into()),
            new_text: "TASK6_SPACE_RESTART_NEW".into(),
        };
        let PutToolDiffOutcome::Stored { diff_ref, .. } = core
            .doc_host
            .put_tool_diff("chat-b", "later-space-tool", &diff)
            .unwrap()
        else {
            panic!("later space chat sidecar is stored before deletion");
        };
        let mut purge_done = core.doc_host.watch_purges();

        core.remote_rpc_service()
            .mutate(MutateParams::DeleteSpace {
                space_id: "space-1".into(),
            })
            .expect("space deletion returns before the first run settles");

        drop(store);
        let restarted_store = DocsStore::open(dir.path().join("local-store")).unwrap();
        assert_eq!(
            restarted_store.load_snapshot("chat-b").unwrap(),
            None,
            "the later chat snapshot is gone before the blocked first finalizer"
        );
        assert_eq!(
            restarted_store
                .read_tool_diff("chat-b", "later-space-tool", &diff_ref)
                .unwrap(),
            None,
            "the later chat exact sidecar is gone before restart"
        );

        release_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while *purge_done.borrow() < 2 {
                purge_done
                    .changed()
                    .await
                    .expect("purge watch stays connected");
            }
        })
        .await
        .expect("both final retries complete after the run settles");
    }

    /// Removing the admission mark while an old run still settles lets that
    /// run write into, then delete, a new chat with the same id.
    #[tokio::test]
    async fn deleting_a_running_chat_defers_same_id_reuse_until_final_purge() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("device-id"), "dev-a").unwrap();
        let (release_tx, release_rx) = oneshot::channel();
        let registry = Arc::new(crate::registry::HarnessRegistry::new());
        registry.register(Arc::new(LateToolResultHarness {
            release: Mutex::new(Some(release_rx)),
        }));
        let core = crate::EngineCore::assemble(dir.path(), registry, HarnessId::Mock, None)
            .expect("engine core assembles");
        core.workspace
            .create_space(
                "space-1",
                "dev-a",
                unwatched_space_root(dir.path()).to_string_lossy().as_ref(),
                None,
                false,
            )
            .unwrap();
        core.workspace
            .create_chat("chat-1", "space-1", None, None)
            .unwrap();

        let (_replay, mut live) = core.sessions.subscribe("chat-1", 0).unwrap();
        core.sessions
            .dispatch(
                "chat-1",
                HarnessId::Mock,
                comet_proto::RunRequest {
                    prompt: "change it".into(),
                    harness: None,
                    model: None,
                    reasoning: None,
                    model_options: Default::default(),
                    cwd: unwatched_space_root(dir.path())
                        .to_string_lossy()
                        .to_string(),
                    runtime_mode: comet_proto::RuntimeMode::default(),
                    sandbox: comet_proto::SandboxLevel::WorkspaceWrite,
                    attachments: Vec::new(),
                    resume: None,
                },
                Some("user-1".into()),
            )
            .await
            .unwrap();
        loop {
            let event = tokio::time::timeout(Duration::from_secs(1), live.recv())
                .await
                .expect("tool call arrives before deletion")
                .expect("live stream stays open")
                .event;
            if matches!(event, comet_proto::AgentEvent::ToolCall { .. }) {
                break;
            }
        }

        let mut purge_done = core.doc_host.watch_purges();
        let rpc = core.remote_rpc_service();
        rpc.mutate(MutateParams::DeleteChat {
            chat_id: "chat-1".into(),
        })
        .unwrap();
        assert!(
            rpc.mutate(MutateParams::CreateChat {
                chat_id: "chat-1".into(),
                space_id: "space-1".into(),
                config: None,
                branch: None,
                cwd: None,
            })
            .is_err(),
            "a chat id stays unavailable until its old purge has completed"
        );

        release_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), purge_done.changed())
            .await
            .expect("old run settles and purges")
            .expect("purge watch stays connected");

        rpc.mutate(MutateParams::CreateChat {
            chat_id: "chat-1".into(),
            space_id: "space-1".into(),
            config: None,
            branch: None,
            cwd: None,
        })
        .expect("same id is reusable after final purge");
        let store = DocsStore::open(dir.path().join("local-store")).unwrap();
        store
            .save_snapshot("chat-1", b"new generation snapshot")
            .unwrap();
        let new_diff = comet_proto::ToolDiff {
            path: "src/new-generation.rs".into(),
            old_text: Some("before".into()),
            new_text: "after".into(),
        };
        let PutToolDiffOutcome::Stored { diff_ref, .. } = core
            .doc_host
            .put_tool_diff("chat-1", "new-generation-tool", &new_diff)
            .unwrap()
        else {
            panic!("new generation sidecar is admitted");
        };
        assert_eq!(
            store.load_snapshot("chat-1").unwrap(),
            Some(b"new generation snapshot".to_vec())
        );
        assert_eq!(
            core.doc_host
                .read_tool_diff("chat-1", "new-generation-tool", &diff_ref)
                .unwrap(),
            Some(new_diff)
        );
    }

    /// DeleteSpace marks every chat before its sequential teardown, so a
    /// later id cannot be reused while an earlier blocked run delays it.
    #[tokio::test]
    async fn delete_space_keeps_later_chat_purging_until_its_turn() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("device-id"), "dev-a").unwrap();
        let (release_tx, release_rx) = oneshot::channel();
        let registry = Arc::new(crate::registry::HarnessRegistry::new());
        registry.register(Arc::new(LateToolResultHarness {
            release: Mutex::new(Some(release_rx)),
        }));
        let core = crate::EngineCore::assemble(dir.path(), registry, HarnessId::Mock, None)
            .expect("engine core assembles");
        core.workspace
            .create_space(
                "space-1",
                "dev-a",
                unwatched_space_root(dir.path()).to_string_lossy().as_ref(),
                None,
                false,
            )
            .unwrap();
        for chat_id in ["chat-a", "chat-b"] {
            core.workspace
                .create_chat(chat_id, "space-1", None, None)
                .unwrap();
        }

        let (_replay, mut live) = core.sessions.subscribe("chat-a", 0).unwrap();
        core.sessions
            .dispatch(
                "chat-a",
                HarnessId::Mock,
                comet_proto::RunRequest {
                    prompt: "change it".into(),
                    harness: None,
                    model: None,
                    reasoning: None,
                    model_options: Default::default(),
                    cwd: unwatched_space_root(dir.path())
                        .to_string_lossy()
                        .to_string(),
                    runtime_mode: comet_proto::RuntimeMode::default(),
                    sandbox: comet_proto::SandboxLevel::WorkspaceWrite,
                    attachments: Vec::new(),
                    resume: None,
                },
                Some("user-a".into()),
            )
            .await
            .unwrap();
        loop {
            let event = tokio::time::timeout(Duration::from_secs(1), live.recv())
                .await
                .expect("tool call arrives before space deletion")
                .expect("live stream stays open")
                .event;
            if matches!(event, comet_proto::AgentEvent::ToolCall { .. }) {
                break;
            }
        }

        let mut purge_done = core.doc_host.watch_purges();
        let rpc = core.remote_rpc_service();
        rpc.mutate(MutateParams::DeleteSpace {
            space_id: "space-1".into(),
        })
        .unwrap();
        rpc.mutate(MutateParams::CreateSpace {
            space_id: "space-1".into(),
            device_id: "dev-a".into(),
            path: unwatched_space_root(dir.path())
                .to_string_lossy()
                .to_string(),
            name: None,
            git_detected: false,
        })
        .unwrap();
        assert!(
            rpc.mutate(MutateParams::CreateChat {
                chat_id: "chat-b".into(),
                space_id: "space-1".into(),
                config: None,
                branch: None,
                cwd: None,
            })
            .is_err(),
            "the later chat stays purging while the first run settles"
        );

        release_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while *purge_done.borrow() < 2 {
                purge_done
                    .changed()
                    .await
                    .expect("purge watch stays connected");
            }
        })
        .await
        .expect("both space chats finish their final purge");
        rpc.mutate(MutateParams::CreateChat {
            chat_id: "chat-b".into(),
            space_id: "space-1".into(),
            config: None,
            branch: None,
            cwd: None,
        })
        .expect("later chat is reusable after its own final purge");
    }

    /// A branch write is optional follow-up work. Once the chat row exists,
    /// lifecycle admission must be restored even if that follow-up fails, or
    /// the materialized row becomes permanently unable to retain sidecars.
    #[tokio::test]
    async fn branch_failure_after_create_restores_lifecycle_admission() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("device-id"), "dev-a").unwrap();
        let registry = Arc::new(crate::registry::HarnessRegistry::new());
        registry.register(Arc::new(comet_harness::mock::MockHarness::new()));
        let core = crate::EngineCore::assemble(dir.path(), registry, HarnessId::Mock, None)
            .expect("engine core assembles");
        core.workspace
            .create_space(
                "space-1",
                "dev-a",
                unwatched_space_root(dir.path()).to_string_lossy().as_ref(),
                None,
                false,
            )
            .unwrap();
        let purge = core
            .doc_host
            .begin_purge("chat-1")
            .expect("the old generation begins purging");
        assert_eq!(
            core.doc_host.finish_purge("chat-1", purge),
            PurgeFinishOutcome::Purged
        );

        let error = create_chat_with_lifecycle(
            &core.doc_host,
            "chat-1",
            false,
            || {
                core.workspace
                    .create_chat("chat-1", "space-1", None, None)
                    .map(drop)
            },
            || Err(crate::EngineError::Other("branch write failed".into())),
        )
        .expect_err("the injected follow-up branch write fails");
        assert!(matches!(error, crate::EngineError::Other(_)));
        assert!(
            core.workspace.doc().chat("chat-1").unwrap().is_some(),
            "the create already materialized the workspace row"
        );

        let diff = comet_proto::ToolDiff {
            path: "src/admitted-after-branch-error.rs".into(),
            old_text: Some("before".into()),
            new_text: "after".into(),
        };
        assert!(matches!(
            core.doc_host
                .put_tool_diff("chat-1", "branch-error-tool", &diff),
            Ok(PutToolDiffOutcome::Stored { .. })
        ));
    }

    /// A harness registered under an arbitrary [`HarnessId`], streaming a
    /// fixed script — `id` is a field rather than fixed like
    /// `comet_harness::mock::MockHarness`'s, which is what lets a test stand
    /// this in for Grok (an ACP agent, out of reach in this crate's tests)
    /// while still going through the real `SessionsEngine::dispatch` →
    /// `drive_run` pipeline.
    struct ScriptedHarness {
        id: HarnessId,
        /// Declared, not inferred from `id`: the engine gates its upfront
        /// titling run on [`comet_proto::HarnessCapabilities::self_titles`],
        /// so a stand-in for a self-titling agent has to answer that
        /// capability the way the real one does. A stub that derived it from
        /// its own `id` would satisfy the gate however the gate was wired.
        self_titles: bool,
        script: Vec<comet_proto::AgentEvent>,
    }

    #[async_trait::async_trait]
    impl comet_harness::Harness for ScriptedHarness {
        fn id(&self) -> HarnessId {
            self.id
        }
        fn display_name(&self) -> &str {
            "Scripted"
        }
        fn capabilities(&self) -> comet_proto::HarnessCapabilities {
            comet_proto::HarnessCapabilities {
                self_titles: self.self_titles,
                ..Default::default()
            }
        }
        async fn models(&self) -> Result<comet_proto::ModelCatalog, comet_harness::HarnessError> {
            Ok(comet_proto::ModelCatalog::built_in(Vec::new()))
        }
        async fn run(
            &self,
            _request: comet_proto::RunRequest,
            _controls: comet_harness::RunControls,
        ) -> Result<
            futures::stream::BoxStream<
                'static,
                Result<comet_proto::AgentEvent, comet_harness::HarnessError>,
            >,
            comet_harness::HarnessError,
        > {
            let events: Vec<_> = self.script.iter().cloned().map(Ok).collect();
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    /// End-to-end proof for `agent-authored-title` (PR9), through the real
    /// dispatch → `drive_run` → journal/doc pipeline, not a direct call into
    /// `titles::TitleGenerator`: a chat dispatched to a self-titling harness
    /// (`HarnessId::Grok`, stood in for by [`ScriptedHarness`] since Grok
    /// itself cannot run in this crate's tests) is named from
    /// `AgentEvent::SessionTitled` mid-stream, WITHOUT the request-start
    /// titling dispatch ever running — proven by registering NO harness
    /// under `HarnessId::Mock` at all, so a model-based titling run dispatched
    /// by mistake would fail loudly (`HarnessError::NotInstalled`) rather
    /// than silently racing and possibly winning.
    #[tokio::test]
    async fn a_grok_chat_is_named_by_its_own_session_titled_event() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Arc::new(crate::registry::HarnessRegistry::new());
        registry.register(Arc::new(ScriptedHarness {
            id: HarnessId::Grok,
            self_titles: true,
            script: vec![
                comet_proto::AgentEvent::SessionStarted {
                    harness: HarnessId::Grok,
                    model: "grok-code-fast".into(),
                    tools: Vec::new(),
                    cwd: "/tmp".into(),
                    session_id: "session-1".into(),
                    assistant_message_id: "assistant-1".into(),
                    runtime_mode: comet_proto::RuntimeMode::default(),
                },
                comet_proto::AgentEvent::TextDelta {
                    text: "Listing the directory now.".into(),
                },
                comet_proto::AgentEvent::SessionTitled {
                    title: "List Directory Files".into(),
                },
                comet_proto::AgentEvent::Done {
                    status: comet_proto::DoneStatus::Completed,
                    result: None,
                    error: None,
                    session_id: None,
                },
            ],
        }));
        let core = crate::EngineCore::assemble(dir.path(), registry, HarnessId::Grok, None)
            .expect("engine core assembles");
        core.workspace
            .create_space(
                "space-1",
                "dev-a",
                dir.path().to_str().unwrap(),
                None,
                false,
            )
            .unwrap();
        core.workspace
            .create_chat("chat-1", "space-1", None, None)
            .unwrap();

        let (_replay, mut live) = core.sessions.subscribe("chat-1", 0).unwrap();
        core.sessions
            .dispatch(
                "chat-1",
                HarnessId::Grok,
                comet_proto::RunRequest {
                    prompt: "List directory files then say DONE".into(),
                    harness: None,
                    model: None,
                    reasoning: None,
                    model_options: Default::default(),
                    cwd: "/tmp".into(),
                    runtime_mode: comet_proto::RuntimeMode::default(),
                    sandbox: comet_proto::SandboxLevel::WorkspaceWrite,
                    attachments: Vec::new(),
                    resume: None,
                },
                Some("user-1".into()),
            )
            .await
            .unwrap();

        loop {
            let event = tokio::time::timeout(Duration::from_secs(2), live.recv())
                .await
                .expect("run settles")
                .expect("live stream stays open")
                .event;
            if matches!(event, comet_proto::AgentEvent::Done { .. }) {
                break;
            }
        }

        // `apply_agent_title` and the turn-end fallback are both
        // fire-and-forget: give them a bounded window to land.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if core
                .workspace
                .doc()
                .chat("chat-1")
                .unwrap()
                .unwrap()
                .title
                .is_some()
            {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the chat was never named"
            );
            tokio::task::yield_now().await;
        }
        assert_eq!(
            core.workspace
                .doc()
                .chat("chat-1")
                .unwrap()
                .unwrap()
                .title
                .as_deref(),
            Some("List Directory Files"),
            "must be Grok's own title, not a fallback/model-generated one — no other \
             harness was even registered, so a fallback title would prove the upfront \
             dispatch was NOT actually skipped"
        );

        core.shutdown().await;
    }
}
