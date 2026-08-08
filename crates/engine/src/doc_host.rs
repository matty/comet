//! DocHost — per-chat `SessionDoc` handles, local snapshot persistence, and the
//! host-only durable command executor.
//!
//! Pragmatic port of comet's `session-docs.ts` + the `main.ts` executor (spec:
//! feature-inventory §3.3, ARCHITECTURE §2 "command plane"):
//! - the doc IS the outbox: commands and user entries commit locally on the authoritative
//!   Comet instance;
//! - on every doc change (local commit or remote import) the handle re-emits the joined
//!   transcript to watchers, drains pending commands, and schedules a snapshot save;
//! - command drain: evaluate via `evaluate_command` (with the DocsStore processed
//!   ledger), mark processed BEFORE execute, execute through the sessions engine, then
//!   write the outcome status back into the doc as the sole outcome writer.
//!
//! Chat ownership is gated on the workspace doc (`chats[chat_id].deviceId`), with
//! claim-on-first-command for unknown chats. Queueing a command for a chat hosted on
//! another device is rejected by the direct-server authority checks.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, Weak};

use tokio::sync::watch;

use comet_doc::{
    COMMAND_DEFAULT_TTL_MS, CommandBasedOn, CommandDisposition, DocError, EvaluationContext,
    MessagePart, MessageRole, MessageStatus, SessionCommandEntry, SessionCommandPayload,
    SessionCommandStatus, SessionDoc, SessionMessageEntry, evaluate_command,
    join_continuation_entries,
};
use comet_proto::{ApprovalDecision, HarnessId, ToolDiff, UserInputAnswer, UserInputQuestion};
use comet_sync::{DocsStore, PutToolDiffOutcome};

use crate::sessions::{SessionCleanupError, SessionsEngine, SteerOutcome};
use crate::workspace_host::WorkspaceHost;
use crate::{EngineError, new_id, now_ms};

/// Debounce window for local snapshot saves after a doc change.
const SNAPSHOT_DEBOUNCE_MS: u64 = 1_000;

/// Warm-doc LRU: how many unwatched, run-less docs stay fully open. Everything
/// beyond this (and beyond [`comet_doc::DOC_LRU_BYTE_BUDGET`]) is evicted
/// oldest-access-first — reopening from the SQLite snapshot measured within
/// ~11ms of a warm doc, so the cap trades no perceptible open latency.
const WARM_DOC_CAP: usize = 12;

/// Resident-memory estimate per compressed snapshot byte. Loro snapshots are
/// columnar+compressed; the in-memory doc plus mirror runs well above the blob
/// size. A rough multiplier is enough here — the budget is a safety ceiling,
/// the count cap does the day-to-day work.
const RESIDENT_BYTES_PER_SNAPSHOT_BYTE: usize = 6;

/// Floor per open doc regardless of content size.
const DOC_RESIDENT_FLOOR_BYTES: usize = 512 * 1024;
#[derive(Debug, Clone)]
pub struct DocHostConfig {
    pub device_id: String,
    /// Harness for doc-command runs on chats without a workspace `config` row.
    pub default_harness: HarnessId,
}

struct DocHostInner {
    store: Arc<DocsStore>,
    config: DocHostConfig,
    sessions: OnceLock<SessionsEngine>,
    workspace: OnceLock<WorkspaceHost>,
    handles: Mutex<HashMap<String, Arc<ChatDocHandle>>>,
    /// A chat id's local lifecycle owner. This gate stays locked through
    /// persistence, reconciliation cleanup, and final purge, so no callback
    /// can cross into a newer generation that reuses the same id.
    purge_gate: Mutex<PurgeGate>,
    #[cfg(test)]
    purges: watch::Sender<u64>,
}

/// Identity for one deletion or reconciliation lifecycle. Tokens are
/// local-only and distinguish delayed callbacks from the current owner of the
/// same caller-supplied chat id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PurgeToken(u64);

#[derive(Debug, Clone, Copy)]
enum StableLifecycle {
    Live,
    Purged { token: PurgeToken },
}

#[derive(Debug, Clone, Copy)]
enum ChatLifecycle {
    Reconciling {
        token: PurgeToken,
    },
    Purging {
        token: PurgeToken,
        rollback: StableLifecycle,
    },
    Purged {
        token: PurgeToken,
    },
}

/// `chats` omits live ids. Keeping the completed token lets a canceled later
/// tombstone restore the exact completed state it replaced, rather than
/// reopening an unrelated generation.
struct PurgeGate {
    next_token: u64,
    chats: HashMap<String, ChatLifecycle>,
}

impl PurgeGate {
    fn next_token(&mut self) -> PurgeToken {
        let token = PurgeToken(self.next_token);
        self.next_token = self
            .next_token
            .checked_add(1)
            .expect("chat lifecycle token counter exhausted");
        token
    }
}

/// The lifecycle a create owns while materializing its workspace row. A
/// completed deletion or fresh-process reconciliation may be claimed only
/// with its exact token; a pre-existing live row has nothing to clear.
#[derive(Debug, Clone, Copy)]
pub(crate) enum CreateAdmission {
    Live,
    Reconcile(PurgeToken),
    Revive(PurgeToken),
}

/// The synchronous cleanup result is intentionally compact: exact SQLite
/// diagnostics stay in tracing, while a caller can avoid claiming durable
/// deletion completed when the background final retry is still needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PurgeCleanupOutcome {
    Cleared,
    PendingRetry,
}

/// A finalizer either certifies the deletion generation as reusable, leaves
/// its token in the non-reusable cleanup-pending state, or finds that a newer
/// lifecycle already owns the id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PurgeFinishOutcome {
    Purged,
    PendingRetry,
    Stale,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Clone)]
pub struct DocHost {
    inner: Arc<DocHostInner>,
}

#[cfg(test)]
/// Install Reconciling with an existing matching token. Tests use a Purging
/// source state to model a process that restarted after tombstoning a row.
pub(crate) fn restore_reconciling_for_test(host: &DocHost, chat_id: &str, token: PurgeToken) {
    let mut gate = lock(&host.inner.purge_gate);
    let Some(current) = gate.chats.get(chat_id).map(|lifecycle| match lifecycle {
        ChatLifecycle::Purged { token } | ChatLifecycle::Purging { token, .. } => *token,
        ChatLifecycle::Reconciling { .. } => panic!("reconciliation is already installed"),
    }) else {
        panic!("a test reconciliation needs a lifecycle token");
    };
    assert_eq!(current, token, "the test must retain its lifecycle token");
    gate.chats
        .insert(chat_id.to_string(), ChatLifecycle::Reconciling { token });
}

/// One open chat doc and its change/persistence plumbing.
pub struct ChatDocHandle {
    chat_id: String,
    device_id: String,
    doc: Arc<SessionDoc>,
    messages_tx: watch::Sender<Vec<SessionMessageEntry>>,
    /// True when the doc changed while nobody watched: the mirror rebuild is
    /// deferred to the next `watch_messages` attach instead of paid per commit.
    mirror_dirty: AtomicBool,
    /// Epoch ms of the last open/watch touch — the LRU eviction key.
    last_access: AtomicI64,
    /// Last known snapshot blob size — the eviction budget estimate's input.
    snapshot_bytes: AtomicUsize,
    /// Doc subscription (drop = unsubscribe) — bumps the change watch on every commit.
    _sub: loro::Subscription,
}

impl ChatDocHandle {
    pub fn chat_id(&self) -> &str {
        &self.chat_id
    }

    pub fn doc(&self) -> &SessionDoc {
        &self.doc
    }

    pub fn doc_arc(&self) -> Arc<SessionDoc> {
        self.doc.clone()
    }

    /// Joined transcript watch — re-sent on every doc change (WatchDocMessages).
    ///
    /// Attach-time refresh: the mirror is only maintained while watched, so a
    /// doc that changed unwatched materializes here, once, instead of on every
    /// commit it sat through in the background.
    pub fn watch_messages(&self) -> watch::Receiver<Vec<SessionMessageEntry>> {
        self.touch();
        // Subscribe BEFORE the dirty check: a commit racing this attach then
        // sees a live receiver and publishes, instead of re-marking dirty
        // after our refresh and leaving the new watcher a cleared mirror.
        let rx = self.messages_tx.subscribe();
        if self.mirror_dirty.load(Ordering::Acquire) {
            self.publish_messages();
        }
        rx
    }

    fn touch(&self) {
        self.last_access.store(now_ms(), Ordering::Relaxed);
    }

    /// Write a complete user message entry, idempotent by id (the client-minted message
    /// id — a re-executed command or optimistic echo never duplicates the entry).
    pub fn write_user_message(
        &self,
        message_id: &str,
        text: &str,
        created_at: i64,
    ) -> Result<(), DocError> {
        if self.doc.read_entries()?.iter().any(|e| e.id == message_id) {
            return Ok(());
        }
        self.doc.push_message(&SessionMessageEntry {
            id: message_id.to_string(),
            role: MessageRole::User,
            parts: vec![MessagePart::Text {
                id: "t0".into(),
                text: text.to_string(),
            }],
            created_at,
            device_id: self.device_id.clone(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
        })
    }

    /// Recovery sweep: stamp this device's abandoned `streaming` entries `aborted`, appending
    /// `note` as a visible error part so the transcript says WHY the turn
    /// ended (comet folded "Run interrupted by backend restart" the same
    /// way). Returns the stamped entries' `(id, created_at)` — recovery uses
    /// them for the resume-freshness check.
    pub fn mark_abandoned_streams(&self, note: &str) -> Result<Vec<(String, i64)>, DocError> {
        let mut stamped = Vec::new();
        for entry in self.doc.read_entries()? {
            if entry.role == MessageRole::Assistant
                && entry.status == Some(MessageStatus::Streaming)
                && entry.device_id == self.device_id
                && self
                    .doc
                    .set_message_status(&entry.id, MessageStatus::Aborted)?
            {
                let part_id = format!("{}-recovery", entry.id);
                if let Err(err) = self.doc.append_error_part(&entry.id, &part_id, note) {
                    tracing::warn!(chat = %self.chat_id, error = %err, "recovery note append failed");
                }
                // The crash path: the process died while blocked on an
                // approval, so the run loop never reached its terminal
                // stamp and this entry is the only record left.
                match self.doc.expire_open_approvals(&entry.id) {
                    Ok(0) => {}
                    Ok(n) => tracing::info!(chat = %self.chat_id, entry = %entry.id,
                        expired = n, "expired approvals left open by a dead run"),
                    Err(err) => tracing::warn!(chat = %self.chat_id, entry = %entry.id,
                        error = %err, "expiring open approvals failed"),
                }
                // Same crash path, for a subagent: the process died before
                // `sessions::cancel_running_subagents` ever ran, so a part
                // still `Running` here would otherwise persist that way
                // forever, in this doc and in every LAN peer's replica.
                match self.doc.cancel_running_subagents(&entry.id) {
                    Ok(0) => {}
                    Ok(n) => tracing::info!(chat = %self.chat_id, entry = %entry.id,
                        cancelled = n, "cancelled subagents left running by a dead run"),
                    Err(err) => tracing::warn!(chat = %self.chat_id, entry = %entry.id,
                        error = %err, "cancelling running subagents failed"),
                }
                stamped.push((entry.id.clone(), entry.created_at));
            }
        }
        if !stamped.is_empty() {
            self.publish_messages();
        }
        Ok(stamped)
    }

    fn publish_messages(&self) {
        self.mirror_dirty.store(false, Ordering::Release);
        match self.doc.read_entries() {
            Ok(entries) => {
                let joined = join_continuation_entries(entries);
                // send_replace: update the watch even with no subscribers yet, so a
                // late subscriber's first borrow sees the current transcript.
                self.messages_tx.send_replace(joined);
            }
            Err(err) => {
                tracing::warn!(chat = %self.chat_id, error = %err, "transcript read failed");
            }
        }
    }

    /// Per-commit publish path: unwatched docs just mark the mirror dirty —
    /// rebuilding a full transcript nobody reads was a per-tick cost on every
    /// open doc (and kept a second transcript copy hot).
    fn publish_messages_if_watched(&self) {
        if self.messages_tx.receiver_count() == 0 {
            self.mirror_dirty.store(true, Ordering::Release);
            // Shrink the stale mirror: watch_messages rebuilds on attach.
            self.messages_tx.send_replace(Vec::new());
        } else {
            self.publish_messages();
        }
    }

    /// Rough resident cost for the LRU budget.
    fn resident_estimate(&self) -> usize {
        (self.snapshot_bytes.load(Ordering::Relaxed) * RESIDENT_BYTES_PER_SNAPSHOT_BYTE)
            .max(DOC_RESIDENT_FLOOR_BYTES)
    }
}

impl DocHost {
    pub fn new(store: Arc<DocsStore>, config: DocHostConfig) -> Self {
        #[cfg(test)]
        let (purges, _) = watch::channel(0);
        Self {
            inner: Arc::new(DocHostInner {
                store,
                config,
                sessions: OnceLock::new(),
                workspace: OnceLock::new(),
                handles: Mutex::new(HashMap::new()),
                purge_gate: Mutex::new(PurgeGate {
                    next_token: 1,
                    chats: HashMap::new(),
                }),
                #[cfg(test)]
                purges,
            }),
        }
    }

    /// Wire the sessions engine (engine assembly; see `SessionsEngine::set_doc_host`).
    pub fn set_sessions(&self, sessions: SessionsEngine) {
        let _ = self.inner.sessions.set(sessions);
        // Commands may already be pending in warm-opened docs.
        let handles: Vec<_> = lock(&self.inner.handles).values().cloned().collect();
        for handle in handles {
            let host = self.clone();
            tokio::spawn(async move { host.drain_commands(&handle).await });
        }
    }

    /// Wire the workspace host (engine assembly) — the source of chat-ownership rows.
    pub fn set_workspace(&self, workspace: WorkspaceHost) {
        let _ = self.inner.workspace.set(workspace);
    }

    /// The workspace host, once wired (tests may assemble a DocHost without one).
    pub fn workspace(&self) -> Option<&WorkspaceHost> {
        self.inner.workspace.get()
    }

    pub fn device_id(&self) -> &str {
        &self.inner.config.device_id
    }

    /// Store one exact tool-result source pair outside the transcript document.
    pub(crate) fn put_tool_diff(
        &self,
        chat_id: &str,
        part_id: &str,
        diff: &ToolDiff,
    ) -> Result<PutToolDiffOutcome, comet_sync::StoreError> {
        let gate = lock(&self.inner.purge_gate);
        if matches!(
            gate.chats.get(chat_id),
            Some(
                ChatLifecycle::Reconciling { .. }
                    | ChatLifecycle::Purging { .. }
                    | ChatLifecycle::Purged { .. }
            )
        ) {
            return Err(comet_sync::StoreError::ToolDiffPurged);
        }
        // Keep the lifecycle gate held through the write. A concurrent
        // begin-purge waits for this write and then deletes it, while a later
        // write sees the purging/purged state and is refused.
        let outcome = self.inner.store.put_tool_diff(chat_id, part_id, diff);
        drop(gate);
        outcome
    }

    /// Atomically start one deletion lifecycle. A duplicate delete while its
    /// predecessor still settles receives no token and cannot steal cleanup.
    pub(crate) fn begin_purge(&self, chat_id: &str) -> Option<PurgeToken> {
        let mut gate = lock(&self.inner.purge_gate);
        if matches!(
            gate.chats.get(chat_id),
            Some(ChatLifecycle::Reconciling { .. } | ChatLifecycle::Purging { .. })
        ) {
            return None;
        }
        let rollback = match gate.chats.get(chat_id) {
            Some(ChatLifecycle::Purged { token }) => StableLifecycle::Purged { token: *token },
            Some(ChatLifecycle::Reconciling { .. } | ChatLifecycle::Purging { .. }) => {
                unreachable!("non-stable lifecycle returned above")
            }
            None => StableLifecycle::Live,
        };
        let token = gate.next_token();
        gate.chats.insert(
            chat_id.to_string(),
            ChatLifecycle::Purging { token, rollback },
        );
        Some(token)
    }

    /// Roll back only the matching workspace-tombstone attempt. A stale
    /// callback may not reopen a newer purging generation.
    pub(crate) fn cancel_purge(&self, chat_id: &str, token: PurgeToken) -> bool {
        let mut gate = lock(&self.inner.purge_gate);
        let Some(ChatLifecycle::Purging {
            token: current,
            rollback,
        }) = gate.chats.get(chat_id).copied()
        else {
            return false;
        };
        if current != token {
            return false;
        }
        match rollback {
            StableLifecycle::Live => {
                gate.chats.remove(chat_id);
            }
            StableLifecycle::Purged { token } => {
                gate.chats
                    .insert(chat_id.to_string(), ChatLifecycle::Purged { token });
            }
        }
        true
    }

    fn has_previous_generation_owner(&self, chat_id: &str) -> bool {
        if lock(&self.inner.handles).contains_key(chat_id) {
            return true;
        }
        self.inner
            .sessions
            .get()
            .is_some_and(|sessions| sessions.has_live_run(chat_id))
    }

    /// Refuse same-id reuse while a previous generation is still settling.
    /// Fresh-process cleanup installs a token before touching artifacts, and a
    /// completed purge already has one; the caller must present that exact
    /// admission after workspace create succeeds before the id becomes live.
    pub(crate) fn admit_create(
        &self,
        chat_id: &str,
        workspace_row_exists: bool,
    ) -> Result<CreateAdmission, ()> {
        let mut gate = lock(&self.inner.purge_gate);
        let lifecycle = gate.chats.get(chat_id).copied();
        let needs_owner_free = matches!(
            lifecycle,
            Some(ChatLifecycle::Purged { .. } | ChatLifecycle::Reconciling { .. })
        ) || (lifecycle.is_none() && !workspace_row_exists);
        if needs_owner_free && self.has_previous_generation_owner(chat_id) {
            return Err(());
        }

        match lifecycle {
            Some(ChatLifecycle::Purging { .. }) => Err(()),
            Some(ChatLifecycle::Purged { token }) => Ok(CreateAdmission::Revive(token)),
            Some(ChatLifecycle::Reconciling { token }) => {
                match self.cleanup_old_generation(chat_id) {
                    PurgeCleanupOutcome::Cleared => Ok(CreateAdmission::Reconcile(token)),
                    PurgeCleanupOutcome::PendingRetry => Err(()),
                }
            }
            None if workspace_row_exists => Ok(CreateAdmission::Live),
            None => {
                let token = gate.next_token();
                gate.chats
                    .insert(chat_id.to_string(), ChatLifecycle::Reconciling { token });
                match self.cleanup_old_generation(chat_id) {
                    PurgeCleanupOutcome::Cleared => Ok(CreateAdmission::Reconcile(token)),
                    PurgeCleanupOutcome::PendingRetry => Err(()),
                }
            }
        }
    }

    /// Turn a successfully materialized row live. Conditional token claims
    /// prevent a stale create completion from reopening any newer lifecycle.
    pub(crate) fn revive_created_chat(&self, chat_id: &str, admission: CreateAdmission) -> bool {
        let mut gate = lock(&self.inner.purge_gate);
        match admission {
            CreateAdmission::Live => !matches!(
                gate.chats.get(chat_id),
                Some(
                    ChatLifecycle::Reconciling { .. }
                        | ChatLifecycle::Purging { .. }
                        | ChatLifecycle::Purged { .. }
                )
            ),
            CreateAdmission::Reconcile(token) => {
                if matches!(
                    gate.chats.get(chat_id),
                    Some(ChatLifecycle::Reconciling { token: current }) if *current == token
                ) {
                    gate.chats.remove(chat_id);
                    true
                } else {
                    false
                }
            }
            CreateAdmission::Revive(token) => {
                if matches!(
                    gate.chats.get(chat_id),
                    Some(ChatLifecycle::Purged { token: current }) if *current == token
                ) {
                    gate.chats.remove(chat_id);
                    true
                } else {
                    false
                }
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn watch_purges(&self) -> watch::Receiver<u64> {
        self.inner.purges.subscribe()
    }

    /// Read an exact tool-result source pair when its durable reference still matches.
    #[allow(dead_code)] // Consumed by the following read-only RPC task.
    pub(crate) fn read_tool_diff(
        &self,
        chat_id: &str,
        part_id: &str,
        diff_ref: &str,
    ) -> Result<Option<ToolDiff>, comet_sync::StoreError> {
        self.inner.store.read_tool_diff(chat_id, part_id, diff_ref)
    }

    /// Open (or return) the chat's doc handle, load its local snapshot (or
    /// initialize it), and start the change-driven persistence task.
    pub fn open(&self, chat_id: &str) -> Result<Arc<ChatDocHandle>, EngineError> {
        // Lifecycle gate first, then handles/store. Reconciliation keeps this
        // guard reserved through workspace creation, so an open cannot load an
        // old snapshot or insert a handle into the cleaned interval.
        let gate = lock(&self.inner.purge_gate);
        if gate.chats.contains_key(chat_id) {
            return Err(EngineError::ChatCleanupPendingRetry);
        }
        if let Some(handle) = lock(&self.inner.handles).get(chat_id) {
            handle.touch();
            return Ok(handle.clone());
        }
        let mut snapshot_len = 0usize;
        let doc = match self.inner.store.load_snapshot(chat_id)? {
            Some(bytes) => {
                snapshot_len = bytes.len();
                let raw = loro::LoroDoc::new();
                raw.import(&bytes)
                    .map_err(|e| EngineError::Other(format!("snapshot import failed: {e}")))?;
                SessionDoc::from_doc(raw)
            }
            None => SessionDoc::init(chat_id)?,
        };
        let doc = Arc::new(doc);

        let (changed_tx, changed_rx) = watch::channel(0u64);
        let sub = doc.doc().subscribe_root(Arc::new(move |_diff| {
            changed_tx.send_modify(|v| *v = v.wrapping_add(1));
        }));
        // The mirror starts dirty and empty: many opens (command queueing,
        // drains, nudges) never watch the transcript, and the first
        // watch_messages attach materializes it on demand.
        let (messages_tx, _) = watch::channel(Vec::new());

        let handle = Arc::new(ChatDocHandle {
            chat_id: chat_id.to_string(),
            device_id: self.inner.config.device_id.clone(),
            doc: doc.clone(),
            messages_tx,
            mirror_dirty: AtomicBool::new(true),
            last_access: AtomicI64::new(now_ms()),
            snapshot_bytes: AtomicUsize::new(snapshot_len),
            _sub: sub,
        });
        {
            let mut handles = lock(&self.inner.handles);
            if let Some(existing) = handles.get(chat_id) {
                return Ok(existing.clone()); // racing open — keep the first
            }
            handles.insert(chat_id.to_string(), handle.clone());
        }
        drop(gate);

        tokio::spawn(chat_task(self.clone(), Arc::downgrade(&handle), changed_rx));
        self.evict_over_budget();
        Ok(handle)
    }

    pub(crate) fn register_run_if_current<T>(
        &self,
        handle: &Arc<ChatDocHandle>,
        register: impl FnOnce(Arc<SessionDoc>) -> T,
    ) -> Result<T, EngineError> {
        let gate = lock(&self.inner.purge_gate);
        if gate.chats.contains_key(&handle.chat_id) {
            return Err(EngineError::ChatCleanupPendingRetry);
        }
        let current = lock(&self.inner.handles)
            .get(&handle.chat_id)
            .is_some_and(|current| Arc::ptr_eq(current, handle));
        if !current {
            return Err(EngineError::ChatCleanupPendingRetry);
        }
        let run_doc = handle.doc_arc();
        let registered = register(run_doc);
        drop(gate);
        Ok(registered)
    }

    /// LRU eviction: while the warm set exceeds [`WARM_DOC_CAP`] or the
    /// resident estimate exceeds `DOC_LRU_BYTE_BUDGET`, close the
    /// least-recently-touched unpinned docs. Pinned (never evicted):
    /// - watched docs (`messages_tx` has receivers — a UI transcript);
    /// - docs with a live writer (`Arc<SessionDoc>` held outside the handle —
    ///   a run streaming into it);
    /// - host-side docs with pending commands (the executor owes them work).
    ///
    /// Eviction flushes a final snapshot, so reopen loses nothing; missed
    /// direct peers will observe the current authoritative snapshot on reopen.
    fn evict_over_budget(&self) {
        let mut by_age: Vec<(i64, Arc<ChatDocHandle>)> = {
            let handles = lock(&self.inner.handles);
            handles
                .values()
                .map(|handle| (handle.last_access.load(Ordering::Relaxed), handle.clone()))
                .collect()
        };
        by_age.sort_unstable_by_key(|(age, _)| *age);
        for (_, candidate) in by_age {
            let (count, estimate) = {
                let handles = lock(&self.inner.handles);
                (
                    handles.len(),
                    handles
                        .values()
                        .map(|h| h.resident_estimate())
                        .sum::<usize>(),
                )
            };
            if count <= WARM_DOC_CAP && estimate <= comet_doc::DOC_LRU_BYTE_BUDGET {
                return;
            }
            if self.evict_if_current(&candidate) {
                tracing::debug!(chat = %candidate.chat_id, "doc evicted (LRU)");
            }
        }
    }

    fn evict_if_current(&self, candidate: &Arc<ChatDocHandle>) -> bool {
        let gate = lock(&self.inner.purge_gate);
        if gate.chats.contains_key(&candidate.chat_id) {
            return false;
        }
        let current = lock(&self.inner.handles).get(&candidate.chat_id).cloned();
        let Some(current) = current.filter(|current| Arc::ptr_eq(current, candidate)) else {
            return false;
        };
        if Arc::strong_count(&current) > 3 || self.pinned(&current) {
            return false;
        }
        if !self.save_snapshot_under_gate(&gate, &current, || {}) {
            return false;
        }
        let removed = {
            let mut handles = lock(&self.inner.handles);
            match handles.get(&current.chat_id) {
                Some(mapped) if Arc::ptr_eq(mapped, &current) => {
                    handles.remove(&current.chat_id);
                    true
                }
                _ => false,
            }
        };
        drop(gate);
        removed
    }

    fn pinned(&self, handle: &Arc<ChatDocHandle>) -> bool {
        if handle.messages_tx.receiver_count() > 0 {
            return true;
        }
        // The handle itself holds one doc ref; more means a live writer.
        if Arc::strong_count(&handle.doc) > 1 {
            return true;
        }
        if self.is_host(&handle.chat_id) {
            let is_processed = |id: &str| self.inner.store.is_processed(id).unwrap_or(false);
            match handle.doc.read_commands() {
                Ok(commands) => commands
                    .iter()
                    .any(|c| c.status == SessionCommandStatus::Pending && !is_processed(&c.id)),
                // Unreadable ledger: keep the doc, never evict blind.
                Err(_) => true,
            }
        } else {
            false
        }
    }

    /// Attempt both durable cleanup legs only while the matching lifecycle is
    /// still purging. It deliberately leaves that state unchanged so callers
    /// can make an initial synchronous pass and a final post-interrupt retry.
    /// A stale token is a no-op.
    pub(crate) fn cleanup_purging_chat(
        &self,
        chat_id: &str,
        token: PurgeToken,
    ) -> Option<PurgeCleanupOutcome> {
        self.cleanup_purging_chat_with(
            chat_id,
            token,
            |chat_id| self.inner.store.delete_snapshot(chat_id),
            |chat_id| self.inner.store.delete_tool_diffs(chat_id),
        )
    }

    fn cleanup_purging_chat_with<DeleteSnapshot, DeleteToolDiffs>(
        &self,
        chat_id: &str,
        token: PurgeToken,
        delete_snapshot: DeleteSnapshot,
        delete_tool_diffs: DeleteToolDiffs,
    ) -> Option<PurgeCleanupOutcome>
    where
        DeleteSnapshot: FnOnce(&str) -> Result<(), comet_sync::StoreError>,
        DeleteToolDiffs: FnOnce(&str) -> Result<(), comet_sync::StoreError>,
    {
        let gate = lock(&self.inner.purge_gate);
        self.cleanup_purging_chat_locked_with(
            &gate,
            chat_id,
            token,
            delete_snapshot,
            delete_tool_diffs,
        )
    }

    fn cleanup_purging_chat_locked_with<DeleteSnapshot, DeleteToolDiffs>(
        &self,
        gate: &PurgeGate,
        chat_id: &str,
        token: PurgeToken,
        delete_snapshot: DeleteSnapshot,
        delete_tool_diffs: DeleteToolDiffs,
    ) -> Option<PurgeCleanupOutcome>
    where
        DeleteSnapshot: FnOnce(&str) -> Result<(), comet_sync::StoreError>,
        DeleteToolDiffs: FnOnce(&str) -> Result<(), comet_sync::StoreError>,
    {
        if !matches!(
            gate.chats.get(chat_id),
            Some(ChatLifecycle::Purging { token: current, .. }) if *current == token
        ) {
            return None;
        }
        Some(self.cleanup_chat_artifacts_with(chat_id, delete_snapshot, delete_tool_diffs))
    }

    /// Retire all durable state owned by an old generation. This is only used
    /// by final token-owned purge and fresh-process reconciliation; the initial
    /// delete pass deliberately leaves the journal alone while a run settles.
    fn cleanup_old_generation(&self, chat_id: &str) -> PurgeCleanupOutcome {
        self.cleanup_old_generation_with(
            chat_id,
            |chat_id| self.inner.store.delete_snapshot(chat_id),
            |chat_id| self.inner.store.delete_tool_diffs(chat_id),
            |chat_id| self.cleanup_deleted_session(chat_id),
        )
    }

    fn cleanup_old_generation_with<DeleteSnapshot, DeleteToolDiffs, RetireSession>(
        &self,
        chat_id: &str,
        delete_snapshot: DeleteSnapshot,
        delete_tool_diffs: DeleteToolDiffs,
        retire_session: RetireSession,
    ) -> PurgeCleanupOutcome
    where
        DeleteSnapshot: FnOnce(&str) -> Result<(), comet_sync::StoreError>,
        DeleteToolDiffs: FnOnce(&str) -> Result<(), comet_sync::StoreError>,
        RetireSession: FnOnce(&str) -> Result<(), SessionCleanupError>,
    {
        // The caller holds the purge gate. Retire the session first so the
        // lock order stays gate -> session/run maps -> journal; artifacts do
        // not participate in that lock hierarchy and are still attempted if
        // session retirement needs a retry.
        let mut retry_needed = false;
        if let Err(err) = retire_session(chat_id) {
            tracing::warn!(chat = %chat_id, error = %err, "session retirement failed");
            retry_needed = true;
        }
        if self.cleanup_chat_artifacts_with(chat_id, delete_snapshot, delete_tool_diffs)
            == PurgeCleanupOutcome::PendingRetry
        {
            retry_needed = true;
        }
        if retry_needed {
            PurgeCleanupOutcome::PendingRetry
        } else {
            PurgeCleanupOutcome::Cleared
        }
    }

    fn cleanup_deleted_session(&self, chat_id: &str) -> Result<(), SessionCleanupError> {
        self.inner
            .sessions
            .get()
            .map_or(Ok(()), |sessions| sessions.cleanup_deleted_chat(chat_id))
    }

    fn cleanup_chat_artifacts_with<DeleteSnapshot, DeleteToolDiffs>(
        &self,
        chat_id: &str,
        delete_snapshot: DeleteSnapshot,
        delete_tool_diffs: DeleteToolDiffs,
    ) -> PurgeCleanupOutcome
    where
        DeleteSnapshot: FnOnce(&str) -> Result<(), comet_sync::StoreError>,
        DeleteToolDiffs: FnOnce(&str) -> Result<(), comet_sync::StoreError>,
    {
        let mut retry_needed = false;
        if let Err(err) = delete_snapshot(chat_id) {
            tracing::warn!(chat = %chat_id, error = %err, "snapshot delete failed");
            retry_needed = true;
        }
        if let Err(err) = delete_tool_diffs(chat_id) {
            tracing::warn!(chat = %chat_id, error = %err, "tool diff sidecar delete failed");
            retry_needed = true;
        }
        if retry_needed {
            PurgeCleanupOutcome::PendingRetry
        } else {
            PurgeCleanupOutcome::Cleared
        }
    }

    /// Finish only the matching deletion lifecycle and leave the id in its
    /// completed `Purged` state. Cleanup is retried under the same lifecycle
    /// gate, so a stale finalizer can neither erase nor re-mark a newer
    /// generation.
    pub(crate) fn finish_purge(&self, chat_id: &str, token: PurgeToken) -> PurgeFinishOutcome {
        self.finish_purge_with(
            chat_id,
            token,
            |chat_id| self.inner.store.delete_snapshot(chat_id),
            |chat_id| self.inner.store.delete_tool_diffs(chat_id),
            |chat_id| self.cleanup_deleted_session(chat_id),
        )
    }

    fn finish_purge_with<DeleteSnapshot, DeleteToolDiffs, RetireSession>(
        &self,
        chat_id: &str,
        token: PurgeToken,
        delete_snapshot: DeleteSnapshot,
        delete_tool_diffs: DeleteToolDiffs,
        retire_session: RetireSession,
    ) -> PurgeFinishOutcome
    where
        DeleteSnapshot: FnOnce(&str) -> Result<(), comet_sync::StoreError>,
        DeleteToolDiffs: FnOnce(&str) -> Result<(), comet_sync::StoreError>,
        RetireSession: FnOnce(&str) -> Result<(), SessionCleanupError>,
    {
        let mut gate = lock(&self.inner.purge_gate);
        if !matches!(
            gate.chats.get(chat_id),
            Some(ChatLifecycle::Purging { token: current, .. }) if *current == token
        ) {
            return PurgeFinishOutcome::Stale;
        }
        let removed = lock(&self.inner.handles).remove(chat_id);
        drop(removed);
        let cleanup = self.cleanup_old_generation_with(
            chat_id,
            delete_snapshot,
            delete_tool_diffs,
            retire_session,
        );
        if cleanup == PurgeCleanupOutcome::PendingRetry {
            return PurgeFinishOutcome::PendingRetry;
        }
        gate.chats
            .insert(chat_id.to_string(), ChatLifecycle::Purged { token });
        #[cfg(test)]
        self.inner.purges.send_modify(|generation| *generation += 1);
        drop(gate);
        PurgeFinishOutcome::Purged
    }

    /// Composer path: append an immutable pending command entry (rule 1). Durable by
    /// construction — the change subscription kicks the drain, so a local host executes
    /// immediately and an offline doc simply holds the entry until it syncs.
    pub fn queue_command(
        &self,
        chat_id: &str,
        payload: SessionCommandPayload,
    ) -> Result<String, EngineError> {
        let handle = self.open(chat_id)?;
        let id = new_id();
        let now = now_ms();
        let based_on = handle.doc.read_entries()?.last().map(|m| CommandBasedOn {
            turn_id: Some(m.id.clone()),
            frontier: None,
        });
        handle.doc.queue_command(&SessionCommandEntry {
            id: id.clone(),
            payload,
            issued_by: self.inner.config.device_id.clone(),
            issued_at: now,
            based_on,
            expires_at: Some(now + COMMAND_DEFAULT_TTL_MS),
            status: SessionCommandStatus::Pending,
            resolution: None,
        })?;
        Ok(id)
    }

    /// §2.2 writer discipline: we host a chat iff its workspace row's `deviceId` is
    /// ours; a chat with no row is claimable (claim-on-first-command). Without a
    /// wired workspace host (bare-DocHost tests) every open chat is ours — M2's
    /// behavior, now the degenerate case.
    fn is_host(&self, chat_id: &str) -> bool {
        self.workspace().is_none_or(|ws| ws.is_host(chat_id))
    }

    /// Chat-config harness when the workspace row carries one, else the default.
    pub(crate) fn harness_for(&self, chat_id: &str) -> HarnessId {
        self.workspace()
            .and_then(|ws| ws.chat_config(chat_id))
            .map(|config| config.harness)
            .unwrap_or(self.inner.config.default_harness)
    }

    /// Resolve the provider selected for this command. The explicit request
    /// value rides the command plane and therefore survives a missing chat
    /// row; older persisted requests fall back to the row and engine default.
    pub(crate) fn harness_for_request(
        &self,
        chat_id: &str,
        request: &comet_proto::RunRequest,
    ) -> HarnessId {
        request.harness.unwrap_or_else(|| self.harness_for(chat_id))
    }

    /// Drain pending commands (host-only): evaluate → mark processed BEFORE execute →
    /// execute → write the outcome as the sole outcome writer.
    pub async fn drain_commands(&self, handle: &Arc<ChatDocHandle>) {
        let Some(sessions) = self.inner.sessions.get() else {
            return; // executor not wired yet; the set_sessions kick re-drains
        };
        if !self.is_host(&handle.chat_id) {
            return;
        }
        // Entries this pass decided to leave alone (processed dedupe hits).
        let mut skipped: HashSet<String> = HashSet::new();
        loop {
            let commands = match handle.doc.read_commands() {
                Ok(commands) => commands,
                Err(err) => {
                    tracing::warn!(chat = %handle.chat_id, error = %err, "command read failed");
                    return;
                }
            };
            let is_processed = |id: &str| self.inner.store.is_processed(id).unwrap_or(false);
            let Some(entry) = commands
                .iter()
                .find(|c| {
                    c.status == SessionCommandStatus::Pending
                        && !skipped.contains(&c.id)
                        && !is_processed(&c.id)
                })
                .cloned()
            else {
                return;
            };
            let messages = handle.doc.read_entries().unwrap_or_default();
            let current_turn_id = messages.last().map(|m| m.id.clone());
            let turn_is_past = |turn_id: &str| messages.iter().any(|m| m.id == turn_id);
            let disposition = evaluate_command(
                &entry,
                &EvaluationContext {
                    is_processed: &is_processed,
                    now_ms: now_ms(),
                    entries: &commands,
                    current_turn_id: current_turn_id.as_deref(),
                    turn_is_past: &turn_is_past,
                },
            );
            // Mark BEFORE executing: a crash mid-execution must never double-run a
            // command whose side effect may already have happened.
            if let Err(err) = self.inner.store.mark_processed(&entry.id) {
                tracing::error!(chat = %handle.chat_id, error = %err, "processed-ledger write failed; halting drain");
                return;
            }
            match disposition {
                CommandDisposition::Skip => {
                    skipped.insert(entry.id.clone());
                }
                CommandDisposition::Expired => {
                    self.resolve_command(handle, &entry.id, SessionCommandStatus::Expired, None);
                }
                CommandDisposition::Superseded => {
                    self.resolve_command(handle, &entry.id, SessionCommandStatus::Superseded, None);
                }
                CommandDisposition::Execute => {
                    let (status, resolution) = match self.execute(sessions, handle, &entry).await {
                        Ok(outcome) => outcome,
                        Err(err) => (SessionCommandStatus::Rejected, Some(err.to_string())),
                    };
                    self.resolve_command(handle, &entry.id, status, resolution.as_deref());
                }
            }
        }
    }

    /// Host-only outcome write (ledger rule 2).
    fn resolve_command(
        &self,
        handle: &ChatDocHandle,
        command_id: &str,
        status: SessionCommandStatus,
        resolution: Option<&str>,
    ) {
        if let Err(err) = handle
            .doc
            .set_command_status(command_id, status, resolution)
        {
            tracing::warn!(
                chat = %handle.chat_id,
                command = %command_id,
                error = %err,
                "command outcome write failed"
            );
        }
    }

    async fn execute(
        &self,
        sessions: &SessionsEngine,
        handle: &Arc<ChatDocHandle>,
        entry: &SessionCommandEntry,
    ) -> Result<(SessionCommandStatus, Option<String>), EngineError> {
        let chat_id = &handle.chat_id;
        match &entry.payload {
            SessionCommandPayload::Run {
                request,
                message_id,
            } => {
                // Claim-on-first-command: a run for a chat with no workspace row
                // creates the row under our device id (we are about to host it).
                if let Some(ws) = self.workspace() {
                    ws.claim_chat(chat_id, Some(&request.cwd))?;
                }
                let harness = self.harness_for_request(chat_id, request);
                // The owner persists what it is about to dispatch so the row
                // remains truthful even when CreateChat failed or a command-
                // only client reached the claim fallback.
                if let Some(ws) = self.workspace()
                    && ws.chat_config(chat_id).is_none()
                {
                    let config = comet_proto::ChatConfig {
                        harness,
                        model: request.model.clone(),
                        reasoning: request.reasoning,
                        model_options: request.model_options.clone(),
                        sandbox: request.sandbox,
                        runtime_mode: request.runtime_mode,
                    };
                    if let Err(err) = ws.set_chat_config(chat_id, &config) {
                        tracing::warn!(chat = %chat_id, error = %err, "run-config backfill failed");
                    }
                }
                sessions
                    .dispatch(chat_id, harness, request.clone(), Some(message_id.clone()))
                    .await?;
                Ok((SessionCommandStatus::Applied, None))
            }
            SessionCommandPayload::Steer { prompt, message_id } => {
                match sessions.steer(chat_id, prompt, message_id.clone()).await? {
                    SteerOutcome::Accepted => Ok((SessionCommandStatus::Applied, None)),
                    SteerOutcome::NotSteerable => {
                        // No live steerable run: the durable command still delivers —
                        // run it as the next turn (comet's fallback, executor-side).
                        // After an engine restart `last_request` is empty too, so
                        // rebuild the run config from the chat's workspace row
                        // (comet derived dispatch config from the chat row the
                        // same way — sessions.ts:601-620); dispatch's engine-owned
                        // resume then reattaches the prior harness conversation.
                        let request = sessions
                            .last_request(chat_id)
                            .or_else(|| self.request_from_chat_row(chat_id, prompt));
                        let Some(mut request) = request else {
                            return Ok((
                                SessionCommandStatus::Rejected,
                                Some("no live run and no prior run config".into()),
                            ));
                        };
                        request.prompt = prompt.clone();
                        // The remembered request carries the previous turn's
                        // mode; this dispatch runs under the one the chat names
                        // now.
                        self.apply_chat_row_runtime_mode(chat_id, &mut request);
                        request.resume = None; // dispatch re-derives the harness session
                        // A reused config must not re-inline the PREVIOUS
                        // turn's images; this steer's own refs (if any) already
                        // ride the prompt text.
                        request.attachments = Vec::new();
                        let harness = self.harness_for_request(chat_id, &request);
                        sessions
                            .dispatch(chat_id, harness, request, message_id.clone())
                            .await?;
                        Ok((
                            SessionCommandStatus::Applied,
                            Some("queued as new turn".into()),
                        ))
                    }
                }
            }
            SessionCommandPayload::Interrupt {} => {
                sessions.interrupt(chat_id).await?;
                Ok((SessionCommandStatus::Applied, None))
            }
            SessionCommandPayload::RespondInput {
                request_id,
                answers,
            } => {
                if sessions.respond_input(chat_id, request_id, answers.clone())? {
                    return Ok((SessionCommandStatus::Applied, None));
                }
                // No live resolver. Only a request id the doc shows as an
                // OPEN question on a SETTLED entry gets the orphan fallback:
                // a mismatched or already-resolved id is a stale/buggy answer
                // and must still reject, and a still-streaming entry's
                // question belongs to the live run (a just-consumed resolver
                // racing a second answer must not spawn a duplicate turn).
                let questions = handle.doc.read_entries().ok().and_then(|entries| {
                    entries
                        .iter()
                        .rev()
                        .filter(|e| e.status != Some(MessageStatus::Streaming))
                        .find_map(|e| {
                            e.parts.iter().find_map(|p| match p {
                                MessagePart::Input {
                                    request_id: rid,
                                    questions,
                                    resolved: false,
                                    ..
                                } if rid == request_id => Some(questions.clone()),
                                _ => None,
                            })
                        })
                });
                let Some(questions) = questions else {
                    return Ok((
                        SessionCommandStatus::Rejected,
                        Some("no pending input request".into()),
                    ));
                };
                // The run died under the question (engine restart, crash).
                // The question is still open in the doc and the command is
                // durable, so honor it anyway — stamp the part resolved and
                // deliver the answers as the next (resumed) turn, the same
                // fallback a dead-run steer takes. The question UI stays up
                // until the user answers (user requirement); this is what
                // makes that answer still WORK.
                let request = sessions
                    .last_request(chat_id)
                    .or_else(|| self.request_from_chat_row(chat_id, ""));
                let Some(mut request) = request else {
                    return Ok((
                        SessionCommandStatus::Rejected,
                        Some("no pending input request and no prior run config".into()),
                    ));
                };
                request.prompt = respond_input_prompt(&questions, answers);
                // As above: the answer runs under the chat's current mode, not
                // the one the dead run happened to be started with.
                self.apply_chat_row_runtime_mode(chat_id, &mut request);
                request.resume = None; // dispatch re-derives the harness session
                request.attachments = Vec::new();
                if let Err(err) = handle.doc.resolve_input(request_id) {
                    tracing::warn!(chat = %chat_id, request = %request_id, error = %err,
                        "orphaned input resolve failed");
                }
                let harness = self.harness_for_request(chat_id, &request);
                sessions.dispatch(chat_id, harness, request, None).await?;
                Ok((
                    SessionCommandStatus::Applied,
                    Some("answered as new turn".into()),
                ))
            }
            SessionCommandPayload::RespondApproval {
                request_id,
                decision,
            } => {
                // `Expired` is stamped by the host when a run ends; it is not a
                // choice a client may make. Accepting it off the wire would let
                // any paired device mark a live approval expired, which every
                // peer would then render as answered and disabled.
                if matches!(decision, ApprovalDecision::Expired) {
                    return Ok((
                        SessionCommandStatus::Rejected,
                        Some("Expired isn't a decision that can be sent.".into()),
                    ));
                }
                if sessions.respond_approval(chat_id, request_id, decision.clone())? {
                    return Ok((SessionCommandStatus::Applied, None));
                }
                // No live resolver, and no fallback is possible: the decision
                // answers a request id owned by a process that has exited.
                // Refuse with a reason rather than dropping it silently.
                tracing::debug!(
                    chat = %chat_id,
                    request = %request_id,
                    "respond-approval had no live resolver"
                );
                Ok((
                    SessionCommandStatus::Rejected,
                    Some("This approval is no longer waiting for an answer.".into()),
                ))
            }
        }
    }

    /// A steer-turned-run with no in-process `last_request` (engine restarted
    /// since the last turn): rebuild the run config from the chat's workspace
    /// row — cwd from the row, model/reasoning/options from its config
    /// (composer defaults otherwise), and the runtime mode from its config,
    /// with the sandbox derived from that mode rather than stored separately.
    /// `None` without a workspace host or row.
    // (Also the RespondInput dead-run fallback's config source.)
    pub(crate) fn request_from_chat_row(
        &self,
        chat_id: &str,
        prompt: &str,
    ) -> Option<comet_proto::RunRequest> {
        let workspace = self.workspace()?;
        let chat = match workspace.doc().chat(chat_id) {
            Ok(chat) => chat?,
            Err(err) => {
                tracing::warn!(chat = %chat_id, error = %err, "workspace chat read failed");
                return None;
            }
        };
        let config = chat.config;
        let runtime_mode = config.as_ref().map(|c| c.runtime_mode).unwrap_or_default();
        Some(comet_proto::RunRequest {
            prompt: prompt.to_string(),
            harness: config.as_ref().map(|c| c.harness),
            model: config.as_ref().and_then(|c| c.model.clone()),
            reasoning: config.as_ref().and_then(|c| c.reasoning),
            model_options: config
                .as_ref()
                .map(|c| c.model_options.clone())
                .unwrap_or_default(),
            cwd: chat.cwd.unwrap_or_default(),
            ..comet_proto::RunRequest::for_session(runtime_mode)
        })
    }

    /// Overlay the chat row's *current* runtime mode onto a request rebuilt
    /// from the remembered one, and re-derive the sandbox from it.
    ///
    /// `last_request` is stamped at dispatch and never touched again, so it
    /// carries the mode of the previous turn. A mode change applies to the next
    /// dispatch — there is no mid-process change to make, since the provider
    /// was spawned with its permission mode — and these fallback paths *are* a
    /// next dispatch. Without this the divergence runs in the permissive
    /// direction: a user tightens a chat to `approval-required`, steers, and
    /// the steered run still writes unattended (`DEBT.md` D11).
    ///
    /// The sandbox is derived here rather than carried over, because the two
    /// must not be left disagreeing — the same rule `apply_owned_fields`
    /// follows in the UI. Titling's never-ask/read-only pairing is unaffected:
    /// it builds its own request and never takes this path.
    pub(crate) fn apply_chat_row_runtime_mode(
        &self,
        chat_id: &str,
        request: &mut comet_proto::RunRequest,
    ) {
        let Some(mode) = self.chat_row_runtime_mode(chat_id) else {
            return; // no workspace or no row: the remembered mode is all there is
        };
        request.runtime_mode = mode;
        request.sandbox = mode.sandbox();
    }

    /// The mode the chat row currently names, if the row exists and carries a
    /// config. `None` is "unknown", never "the default".
    fn chat_row_runtime_mode(&self, chat_id: &str) -> Option<comet_proto::RuntimeMode> {
        let workspace = self.workspace()?;
        match workspace.doc().chat(chat_id) {
            Ok(chat) => chat?.config.map(|c| c.runtime_mode),
            Err(err) => {
                tracing::warn!(chat = %chat_id, error = %err, "workspace chat read failed");
                None
            }
        }
    }

    fn save_snapshot(&self, handle: &Arc<ChatDocHandle>) {
        self.save_snapshot_with_probe(handle, || {});
    }

    fn save_snapshot_with_probe<Probe>(&self, handle: &Arc<ChatDocHandle>, probe: Probe)
    where
        Probe: FnOnce(),
    {
        let gate = lock(&self.inner.purge_gate);
        let _ = self.save_snapshot_under_gate(&gate, handle, probe);
        drop(gate);
    }

    fn save_snapshot_under_gate<Probe>(
        &self,
        gate: &PurgeGate,
        handle: &Arc<ChatDocHandle>,
        probe: Probe,
    ) -> bool
    where
        Probe: FnOnce(),
    {
        if gate.chats.contains_key(&handle.chat_id) {
            return false;
        }
        let current = lock(&self.inner.handles)
            .get(&handle.chat_id)
            .is_some_and(|current| Arc::ptr_eq(current, handle));
        if !current {
            return false;
        }
        probe();
        self.persist_snapshot(handle);
        true
    }

    fn persist_snapshot(&self, handle: &ChatDocHandle) {
        match handle.doc.export_snapshot() {
            Ok(bytes) => {
                handle.snapshot_bytes.store(bytes.len(), Ordering::Relaxed);
                if let Err(err) = self.inner.store.save_snapshot(&handle.chat_id, &bytes) {
                    tracing::warn!(chat = %handle.chat_id, error = %err, "snapshot save failed");
                }
            }
            Err(err) => {
                tracing::warn!(chat = %handle.chat_id, error = %err, "snapshot export failed");
            }
        }
    }

    /// Persist every open doc now (shutdown path; bypasses the debounce).
    pub fn flush_all(&self) {
        let handles: Vec<_> = lock(&self.inner.handles).values().cloned().collect();
        for handle in handles {
            self.save_snapshot(&handle);
        }
    }
}

/// The resumed-turn prompt for answers to a question whose run died: each
/// answer paired with its question text so the reattached conversation reads
/// naturally. Pure.
pub fn respond_input_prompt(
    questions: &[UserInputQuestion],
    answers: &[UserInputAnswer],
) -> String {
    let mut lines = vec!["Answering your earlier question:".to_string()];
    for answer in answers {
        let picked = answer.labels.join(", ");
        let question = questions
            .iter()
            .find(|q| q.id == answer.question_id)
            .map(|q| q.question.trim())
            .filter(|q| !q.is_empty());
        match question {
            Some(question) => lines.push(format!("{question} — {picked}")),
            None => lines.push(picked),
        }
    }
    lines.join("\n")
}

/// Per-chat background task: reacts to doc changes (local commits and remote imports)
/// by re-publishing the transcript watch, draining commands, and debouncing snapshots.
/// Holds only a weak handle so a dropped host tears the task down.
async fn chat_task(host: DocHost, weak: Weak<ChatDocHandle>, mut changed_rx: watch::Receiver<u64>) {
    // Initial pass: the snapshot may already carry pending commands. The
    // mirror stays lazy — it materializes on the first watch attach.
    {
        let Some(handle) = weak.upgrade() else { return };
        host.drain_commands(&handle).await;
    }
    let mut save_deadline: Option<tokio::time::Instant> = None;
    loop {
        let sleep_until = save_deadline.unwrap_or_else(tokio::time::Instant::now);
        tokio::select! {
            changed = changed_rx.changed() => {
                if changed.is_err() {
                    break; // doc handle (and its change sender) is gone
                }
                let Some(handle) = weak.upgrade() else { break };
                handle.publish_messages_if_watched();
                host.drain_commands(&handle).await;
                if save_deadline.is_none() {
                    save_deadline = Some(
                        tokio::time::Instant::now()
                            + std::time::Duration::from_millis(SNAPSHOT_DEBOUNCE_MS),
                    );
                }
            }
            _ = tokio::time::sleep_until(sleep_until), if save_deadline.is_some() => {
                save_deadline = None;
                let Some(handle) = weak.upgrade() else { break };
                host.save_snapshot(&handle);
                // Post-quiesce eviction pass: sizes just refreshed.
                host.evict_over_budget();
            }
        }
    }
}

#[cfg(test)]
pub(crate) fn detach_handle_for_reconciliation_test(
    host: &DocHost,
    chat_id: &str,
) -> Option<Arc<ChatDocHandle>> {
    lock(&host.inner.handles).remove(chat_id)
}

#[cfg(test)]
pub(crate) fn save_snapshot_for_reconciliation_test(host: &DocHost, handle: &Arc<ChatDocHandle>) {
    host.save_snapshot(handle);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use comet_proto::{ChatConfig, HarnessId, RuntimeMode, SandboxLevel, SubagentStatus};

    // These tests only exercise the workspace-row read, never a dispatch, so
    // the stock mock harness (empty script) is enough to satisfy assembly.
    fn registry() -> Arc<crate::registry::HarnessRegistry> {
        let registry = crate::registry::HarnessRegistry::new();
        registry.register(Arc::new(comet_harness::mock::MockHarness::new()));
        Arc::new(registry)
    }

    /// Assemble an engine core over a tempdir (offline, fixed device id) — the
    /// same pattern `crates/engine/tests/workspace_sync.rs` uses for its own
    /// fixture, copied here because `request_from_chat_row` is crate-private.
    fn assemble_core(dir: &std::path::Path, device_id: &str) -> crate::EngineCore {
        std::fs::create_dir_all(dir).expect("create data dir");
        std::fs::write(dir.join("device-id"), device_id).expect("write device id");
        crate::EngineCore::assemble(dir, registry(), HarnessId::Mock, None)
            .expect("engine core assembles")
    }

    #[tokio::test]
    async fn stored_runtime_mode_reaches_the_next_run_request() {
        // The chat row is what a run reads when it has no remembered request:
        // resume, and the dead-run fallback. The mode a user chose has to
        // survive that read, and the sandbox has to follow the mode rather
        // than stay at whatever the row was created with.
        let dir = tempfile::tempdir().unwrap();
        let core = assemble_core(dir.path(), "dev-a");

        core.workspace
            .create_space("space-1", "dev-a", "/tmp/cfg", None, false)
            .expect("create space");
        core.workspace
            .create_chat(
                "chat-1",
                "space-1",
                Some(ChatConfig {
                    harness: HarnessId::ClaudeCode,
                    model: None,
                    reasoning: None,
                    model_options: Default::default(),
                    sandbox: SandboxLevel::WorkspaceWrite,
                    runtime_mode: RuntimeMode::ApprovalRequired,
                }),
                None,
            )
            .expect("create chat");

        let request = core
            .doc_host
            .request_from_chat_row("chat-1", "hello")
            .expect("chat row exists");
        assert_eq!(request.runtime_mode, RuntimeMode::ApprovalRequired);
        assert_eq!(request.sandbox, SandboxLevel::ReadOnly);
    }

    #[tokio::test]
    async fn a_chat_row_without_a_config_gets_the_default_mode() {
        // Every chat that predates this field. Absent is not "unknown" — it
        // is the mode those chats were already running under.
        let dir = tempfile::tempdir().unwrap();
        let core = assemble_core(dir.path(), "dev-a");

        core.workspace
            .create_space("space-1", "dev-a", "/tmp/cfg", None, false)
            .expect("create space");
        core.workspace
            .create_chat("chat-1", "space-1", None, None)
            .expect("create chat");

        let request = core
            .doc_host
            .request_from_chat_row("chat-1", "hello")
            .expect("chat row exists");
        assert_eq!(request.runtime_mode, RuntimeMode::AutoAcceptEdits);
        assert_eq!(request.sandbox, SandboxLevel::WorkspaceWrite);
    }

    #[tokio::test]
    async fn snapshot_save_holds_the_lifecycle_gate_through_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let core = assemble_core(dir.path(), "dev-a");
        let handle = core.doc_host.open("chat-1").unwrap();
        handle.write_user_message("m1", "current", 1).unwrap();

        core.doc_host.save_snapshot_with_probe(&handle, || {
            assert!(
                core.doc_host.inner.purge_gate.try_lock().is_err(),
                "the lifecycle gate must remain held until persistence completes"
            );
        });
    }

    #[tokio::test]
    async fn run_registration_holds_the_lifecycle_gate_through_insertion() {
        let dir = tempfile::tempdir().unwrap();
        let core = assemble_core(dir.path(), "dev-a");
        let handle = core.doc_host.open("chat-1").unwrap();
        core.doc_host
            .register_run_if_current(&handle, |_doc| {
                assert!(
                    core.doc_host.inner.purge_gate.try_lock().is_err(),
                    "run insertion must complete before lifecycle admission can proceed"
                );
            })
            .unwrap();
    }

    #[tokio::test]
    async fn eviction_saves_the_current_handle_before_removal() {
        let dir = tempfile::tempdir().unwrap();
        let core = assemble_core(dir.path(), "dev-a");
        let handle = core.doc_host.open("chat-1").unwrap();
        handle
            .write_user_message("before-evict", "persist me", 1)
            .unwrap();

        assert!(core.doc_host.evict_if_current(&handle));
        let reopened = core.doc_host.open("chat-1").unwrap();
        assert!(!Arc::ptr_eq(&handle, &reopened));
        assert!(
            reopened
                .doc()
                .read_entries()
                .unwrap()
                .iter()
                .any(|entry| entry.id == "before-evict")
        );
    }

    #[tokio::test]
    async fn eviction_defers_across_open_to_watcher_and_run_pin_transitions() {
        let dir = tempfile::tempdir().unwrap();
        let core = assemble_core(dir.path(), "dev-a");
        let handle = core.doc_host.open("chat-1").unwrap();
        handle
            .write_user_message("before-watch", "persist me", 1)
            .unwrap();
        let candidate = handle.clone();

        assert!(!core.doc_host.evict_if_current(&candidate));
        let receiver = handle.watch_messages();
        drop(handle);
        assert!(!core.doc_host.evict_if_current(&candidate));
        drop(receiver);

        let run_handle = candidate.clone();
        assert!(!core.doc_host.evict_if_current(&candidate));
        let run_doc = run_handle.doc_arc();
        drop(run_handle);
        assert!(!core.doc_host.evict_if_current(&candidate));
        drop(run_doc);

        assert!(core.doc_host.evict_if_current(&candidate));
        let reopened = core.doc_host.open("chat-1").unwrap();
        assert!(!Arc::ptr_eq(&candidate, &reopened));
        assert!(
            reopened
                .doc()
                .read_entries()
                .unwrap()
                .iter()
                .any(|entry| entry.id == "before-watch")
        );
    }

    #[tokio::test]
    async fn a_stale_eviction_candidate_cannot_remove_or_overwrite_its_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let core = assemble_core(dir.path(), "dev-a");
        let stale = core.doc_host.open("chat-1").unwrap();
        stale
            .write_user_message("stale-eviction", "old", 1)
            .unwrap();
        assert!(detach_handle_for_reconciliation_test(&core.doc_host, "chat-1").is_some());

        let replacement = core.doc_host.open("chat-1").unwrap();
        replacement
            .write_user_message("replacement", "new", 2)
            .unwrap();
        assert!(!core.doc_host.evict_if_current(&stale));
        assert!(Arc::ptr_eq(
            &replacement,
            &core.doc_host.open("chat-1").unwrap()
        ));

        core.doc_host.flush_all();
        tokio::time::sleep(Duration::from_millis(1_100)).await;
        let entries = replacement.doc().read_entries().unwrap();
        assert!(entries.iter().any(|entry| entry.id == "replacement"));
    }

    #[tokio::test]
    async fn a_runtime_mode_change_through_set_chat_config_reaches_the_next_run_request() {
        // The mutate path (comet's setChatConfig RPC), not just create-time
        // config — a user flipping the mode on an existing chat must have it
        // honoured too.
        let dir = tempfile::tempdir().unwrap();
        let core = assemble_core(dir.path(), "dev-a");

        core.workspace
            .create_space("space-1", "dev-a", "/tmp/cfg", None, false)
            .expect("create space");
        core.workspace
            .create_chat(
                "chat-1",
                "space-1",
                Some(ChatConfig {
                    harness: HarnessId::ClaudeCode,
                    model: None,
                    reasoning: None,
                    model_options: Default::default(),
                    sandbox: SandboxLevel::WorkspaceWrite,
                    runtime_mode: RuntimeMode::AutoAcceptEdits,
                }),
                None,
            )
            .expect("create chat");
        let updated = core
            .workspace
            .set_chat_config(
                "chat-1",
                &ChatConfig {
                    harness: HarnessId::ClaudeCode,
                    model: None,
                    reasoning: None,
                    model_options: Default::default(),
                    sandbox: SandboxLevel::WorkspaceWrite,
                    runtime_mode: RuntimeMode::FullAccess,
                },
            )
            .expect("update chat config");
        assert!(updated, "chat row must exist to update");

        let request = core
            .doc_host
            .request_from_chat_row("chat-1", "hello")
            .expect("chat row exists");
        assert_eq!(request.runtime_mode, RuntimeMode::FullAccess);
        assert_eq!(request.sandbox, SandboxLevel::DangerFullAccess);
    }

    /// The subagent analogue of the crash path an interrupt can never reach:
    /// the process is killed while a child subagent is mid-run, so
    /// `sessions::cancel_running_subagents` never gets to run and this
    /// entry's `streaming` status is the only trace left. Exercises the real
    /// `mark_abandoned_streams` wiring (not just the `SessionDoc` method it
    /// calls), and reads the result back off the persisted doc.
    #[tokio::test]
    async fn mark_abandoned_streams_cancels_a_running_subagent_left_by_a_dead_run() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(DocsStore::open(dir.path().join("local-store")).unwrap());
        let host = DocHost::new(
            store,
            DocHostConfig {
                device_id: "dev-a".into(),
                default_harness: HarnessId::Mock,
            },
        );
        let handle = host.open("chat-1").unwrap();
        handle
            .doc()
            .push_message(&SessionMessageEntry {
                id: "m1".into(),
                role: MessageRole::Assistant,
                parts: vec![MessagePart::Subagent {
                    id: "sub-1".into(),
                    task_id: "t1".into(),
                    agent_type: "general-purpose".into(),
                    description: "Read README and report first heading".into(),
                    status: SubagentStatus::Running,
                    activity: None,
                    summary: None,
                    total_tokens: None,
                    duration_ms: None,
                    tool_uses: None,
                }],
                created_at: 1,
                device_id: "dev-a".into(),
                status: Some(MessageStatus::Streaming),
                continuation_of: None,
            })
            .unwrap();

        handle.mark_abandoned_streams("backend restarted").unwrap();

        let entries = handle.doc().read_entries().unwrap();
        let subagent = entries[0]
            .parts
            .iter()
            .find(|p| matches!(p, MessagePart::Subagent { .. }))
            .expect("subagent part survives the sweep");
        assert!(matches!(
            subagent,
            MessagePart::Subagent {
                status: SubagentStatus::Cancelled,
                ..
            }
        ));
    }

    /// A final cleanup pass may certify reuse only when both durable legs
    /// clear. Either leg can fail independently through the initial and final
    /// passes; the sibling must still run, and a later retry must be able to
    /// complete the same token before a clean generation is admitted.
    #[tokio::test]
    async fn either_cleanup_leg_failure_stays_purging_until_retry_clears() {
        for failing_leg in ["snapshot", "sidecar"] {
            let dir = tempfile::tempdir().unwrap();
            let store = Arc::new(DocsStore::open(dir.path().join("local-store")).unwrap());
            let host = DocHost::new(
                store.clone(),
                DocHostConfig {
                    device_id: "dev-a".into(),
                    default_harness: HarnessId::Mock,
                },
            );
            let old_diff = ToolDiff {
                path: format!("src/{failing_leg}-old.rs"),
                old_text: Some("TASK6_RETRY_OLD".into()),
                new_text: "TASK6_RETRY_NEW".into(),
            };
            store
                .save_snapshot("chat-1", b"TASK6_RETRY_OLD_SNAPSHOT")
                .unwrap();
            let PutToolDiffOutcome::Stored {
                diff_ref: old_ref, ..
            } = store
                .put_tool_diff("chat-1", "old-tool", &old_diff)
                .unwrap()
            else {
                panic!("the old generation seeds a sidecar");
            };
            let token = host.begin_purge("chat-1").unwrap();
            let snapshot_attempts = std::cell::Cell::new(0usize);
            let sidecar_attempts = std::cell::Cell::new(0usize);

            let initial = host.cleanup_purging_chat_with(
                "chat-1",
                token,
                |chat_id| {
                    snapshot_attempts.set(snapshot_attempts.get() + 1);
                    if failing_leg == "snapshot" {
                        Err(comet_sync::StoreError::Sqlite(
                            rusqlite::Error::InvalidQuery,
                        ))
                    } else {
                        store.delete_snapshot(chat_id)
                    }
                },
                |chat_id| {
                    sidecar_attempts.set(sidecar_attempts.get() + 1);
                    if failing_leg == "sidecar" {
                        Err(comet_sync::StoreError::Sqlite(
                            rusqlite::Error::InvalidQuery,
                        ))
                    } else {
                        store.delete_tool_diffs(chat_id)
                    }
                },
            );
            assert_eq!(initial, Some(PurgeCleanupOutcome::PendingRetry));

            // Refill only the successful sibling leg. The injected final pass
            // must attempt it again rather than short-circuiting on the same
            // failing leg.
            if failing_leg == "snapshot" {
                store
                    .put_tool_diff("chat-1", "old-tool", &old_diff)
                    .unwrap();
            } else {
                store
                    .save_snapshot("chat-1", b"TASK6_RETRY_OLD_SNAPSHOT")
                    .unwrap();
            }
            let final_attempt = host.finish_purge_with(
                "chat-1",
                token,
                |chat_id| {
                    snapshot_attempts.set(snapshot_attempts.get() + 1);
                    if failing_leg == "snapshot" {
                        Err(comet_sync::StoreError::Sqlite(
                            rusqlite::Error::InvalidQuery,
                        ))
                    } else {
                        store.delete_snapshot(chat_id)
                    }
                },
                |chat_id| {
                    sidecar_attempts.set(sidecar_attempts.get() + 1);
                    if failing_leg == "sidecar" {
                        Err(comet_sync::StoreError::Sqlite(
                            rusqlite::Error::InvalidQuery,
                        ))
                    } else {
                        store.delete_tool_diffs(chat_id)
                    }
                },
                |_chat_id| Ok(()),
            );
            assert_eq!(final_attempt, PurgeFinishOutcome::PendingRetry);
            assert_eq!(snapshot_attempts.get(), 2, "{failing_leg}");
            assert_eq!(sidecar_attempts.get(), 2, "{failing_leg}");
            assert!(
                host.admit_create("chat-1", false).is_err(),
                "{failing_leg}: failed cleanup cannot certify same-id reuse"
            );
            if failing_leg == "snapshot" {
                assert_eq!(
                    store.load_snapshot("chat-1").unwrap(),
                    Some(b"TASK6_RETRY_OLD_SNAPSHOT".to_vec())
                );
                assert_eq!(
                    store
                        .read_tool_diff("chat-1", "old-tool", &old_ref)
                        .unwrap(),
                    None,
                    "the sidecar leg still runs on both failed passes"
                );
            } else {
                assert_eq!(store.load_snapshot("chat-1").unwrap(), None);
                assert_eq!(
                    store
                        .read_tool_diff("chat-1", "old-tool", &old_ref)
                        .unwrap(),
                    Some(old_diff.clone()),
                    "the failing sidecar remains pending"
                );
            }

            assert_eq!(
                host.finish_purge("chat-1", token),
                PurgeFinishOutcome::Purged,
                "{failing_leg}: a later clean retry completes the same token"
            );
            let admission = host
                .admit_create("chat-1", false)
                .expect("Cleared to Purged permits one clean generation");
            assert!(host.revive_created_chat("chat-1", admission));
            assert_eq!(store.load_snapshot("chat-1").unwrap(), None);
            assert_eq!(
                store
                    .read_tool_diff("chat-1", "old-tool", &old_ref)
                    .unwrap(),
                None
            );

            let new_handle = host.open("chat-1").unwrap();
            new_handle
                .write_user_message("new-message", "new generation", 2)
                .unwrap();
            host.flush_all();
            let new_diff = ToolDiff {
                path: "src/new-generation.rs".into(),
                old_text: Some("before".into()),
                new_text: "after".into(),
            };
            assert!(matches!(
                host.put_tool_diff("chat-1", "new-tool", &new_diff),
                Ok(PutToolDiffOutcome::Stored { .. })
            ));
            assert!(store.load_snapshot("chat-1").unwrap().is_some());
        }
    }

    #[test]
    fn session_retirement_failure_keeps_the_lifecycle_pending_until_a_clean_retry() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(DocsStore::open(dir.path().join("local-store")).unwrap());
        let host = DocHost::new(
            store,
            DocHostConfig {
                device_id: "dev-a".into(),
                default_harness: HarnessId::Mock,
            },
        );
        let token = host.begin_purge("chat-1").unwrap();

        assert_eq!(
            host.finish_purge_with(
                "chat-1",
                token,
                |_chat_id| Ok(()),
                |_chat_id| Ok(()),
                |_chat_id| {
                    Err(crate::sessions::SessionCleanupError::Journal(
                        crate::run_journal::JournalError::Io(std::io::Error::other(
                            "injected journal cleanup failure",
                        )),
                    ))
                },
            ),
            PurgeFinishOutcome::PendingRetry
        );
        assert!(
            host.admit_create("chat-1", false).is_err(),
            "a failed journal retirement keeps same-id recreation closed"
        );

        assert_eq!(
            host.finish_purge("chat-1", token),
            PurgeFinishOutcome::Purged
        );
        let admission = host
            .admit_create("chat-1", false)
            .expect("a clean retirement retry admits the next generation");
        assert!(host.revive_created_chat("chat-1", admission));
    }

    /// A late cleanup from an older delete must not cancel or finish the
    /// generation that replaced it. In particular, its finalizer must not
    /// remove a sidecar written only after the newer generation was admitted.
    #[test]
    fn stale_purge_token_cannot_cancel_or_finalize_a_newer_generation() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(DocsStore::open(dir.path().join("local-store")).unwrap());
        let host = DocHost::new(
            store.clone(),
            DocHostConfig {
                device_id: "dev-a".into(),
                default_harness: HarnessId::Mock,
            },
        );
        let diff = ToolDiff {
            path: "src/new-generation.rs".into(),
            old_text: Some("before".into()),
            new_text: "after".into(),
        };
        store
            .save_snapshot("chat-1", b"newer generation snapshot")
            .unwrap();
        let PutToolDiffOutcome::Stored { diff_ref, .. } = store
            .put_tool_diff("chat-1", "new-generation-tool", &diff)
            .unwrap()
        else {
            panic!("the test seeds a real exact-source sidecar");
        };

        let first = host
            .begin_purge("chat-1")
            .expect("the first delete starts a lifecycle generation");
        assert!(
            host.cancel_purge("chat-1", first),
            "the matching tombstone rollback restores the original generation"
        );
        let second = host
            .begin_purge("chat-1")
            .expect("a later delete receives a distinct generation");
        assert_ne!(first, second);

        assert!(
            !host.cancel_purge("chat-1", first),
            "an old rollback cannot re-open the newer purging generation"
        );
        assert_eq!(
            host.finish_purge("chat-1", first),
            PurgeFinishOutcome::Stale,
            "an old finalizer cannot delete the newer purging generation"
        );
        assert!(
            host.cleanup_purging_chat("chat-1", first).is_none(),
            "an old initial-cleanup callback is also a no-op"
        );
        assert_eq!(
            store.load_snapshot("chat-1").unwrap(),
            Some(b"newer generation snapshot".to_vec())
        );
        assert_eq!(
            store
                .read_tool_diff("chat-1", "new-generation-tool", &diff_ref)
                .unwrap(),
            Some(diff.clone())
        );
        assert!(matches!(
            host.put_tool_diff("chat-1", "new-generation-tool", &diff),
            Err(comet_sync::StoreError::ToolDiffPurged)
        ));

        assert_eq!(
            host.finish_purge("chat-1", second),
            PurgeFinishOutcome::Purged
        );
        let admission = host
            .admit_create("chat-1", false)
            .expect("only the completed generation is reusable");
        assert!(host.revive_created_chat("chat-1", admission));
        assert!(matches!(
            host.put_tool_diff("chat-1", "new-generation-tool", &diff),
            Ok(PutToolDiffOutcome::Stored { .. })
        ));
    }

    /// A workspace row is not the only ownership signal. Claim-on-first-command
    /// deliberately opens a live doc before that row exists, so reconciliation
    /// must refuse it without deleting either durable leg.
    #[tokio::test]
    async fn workspace_absent_live_handle_refuses_reconciliation_without_scrub() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(DocsStore::open(dir.path().join("local-store")).unwrap());
        let host = DocHost::new(
            store.clone(),
            DocHostConfig {
                device_id: "dev-a".into(),
                default_harness: HarnessId::Mock,
            },
        );
        let handle = host.open("chat-1").unwrap();
        handle
            .write_user_message("claimable-message", "claimable generation", 1)
            .unwrap();
        host.flush_all();
        let diff = ToolDiff {
            path: "src/claimable.rs".into(),
            old_text: Some("TASK6_CLAIMABLE_OLD".into()),
            new_text: "TASK6_CLAIMABLE_NEW".into(),
        };
        let PutToolDiffOutcome::Stored { diff_ref, .. } = host
            .put_tool_diff("chat-1", "claimable-tool", &diff)
            .unwrap()
        else {
            panic!("the claimable generation stores its sidecar");
        };

        let admission = host.admit_create("chat-1", false);

        assert!(
            admission.is_err(),
            "a workspace-absent live handle is not an orphan"
        );
        assert!(store.load_snapshot("chat-1").unwrap().is_some());
        assert_eq!(
            store
                .read_tool_diff("chat-1", "claimable-tool", &diff_ref)
                .unwrap(),
            Some(diff.clone()),
            "refused reconciliation must not scrub a legitimate writer"
        );
        let reopened = host.open("chat-1").unwrap();
        assert!(Arc::ptr_eq(&handle, &reopened));
        assert!(matches!(
            host.put_tool_diff("chat-1", "claimable-tool", &diff),
            Ok(PutToolDiffOutcome::Stored { .. })
        ));
    }

    /// Reconciliation callbacks own one token. Once a generation is admitted,
    /// a duplicate callback from it cannot clear the next reservation.
    #[tokio::test]
    async fn stale_reconciliation_callback_cannot_affect_admitted_generation() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(DocsStore::open(dir.path().join("local-store")).unwrap());
        let host = DocHost::new(
            store,
            DocHostConfig {
                device_id: "dev-a".into(),
                default_harness: HarnessId::Mock,
            },
        );
        let first = host
            .admit_create("chat-1", false)
            .expect("the first clean reconciliation is admitted");
        assert!(host.revive_created_chat("chat-1", first));
        let second = host
            .admit_create("chat-1", false)
            .expect("a later reconciliation owns a different generation");

        assert!(
            !host.revive_created_chat("chat-1", first),
            "the stale callback cannot clear the later reservation"
        );
        assert!(matches!(
            host.open("chat-1"),
            Err(crate::EngineError::ChatCleanupPendingRetry)
        ));
        assert!(host.revive_created_chat("chat-1", second));
        assert!(host.open("chat-1").is_ok());
    }
}
