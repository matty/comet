//! SessionsEngine — per-chat agent runs: dispatch, steering, interrupts, input bridging,
//! journal + broadcast fan-out, and 120ms coalesced doc streaming.
//!
//! Pragmatic port of comet's `sessions.ts` (spec: feature-inventory §3.2):
//! - every `AgentEvent` is (a) appended to the on-disk run journal, (b) broadcast to
//!   in-process subscribers, (c) folded via `fold_event_into_parts` and diffed into the
//!   chat's `SessionDoc` through `SegmentWriter` on a coalesced `STREAM_COMMIT_MS` timer;
//! - the user message entry is pushed to the doc immediately on dispatch (id = the
//!   command's client-minted message id, so optimistic echoes never flicker);
//! - a `Steered` event splits the assistant entry at the exact boundary;
//! - recovery (interrupt or a stale journal at boot) stamps the streaming entry `aborted`.
//!
//! Scope notes: sessions are keyed by chat id (one live run per chat). Comet's pulse
//! loop is ported as the 15s liveness heartbeat in `drive_run`; its stall watchdog is
//! deliberately NOT ported (rejected in review — agents may legitimately wait on
//! something for far longer than any timeout, and a live child IS the working signal).
//! Every dying path must instead carry its own visible error (child crash with stderr,
//! spawn failure, stream error, engine-restart recovery).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use chrono::{DateTime, Utc};
use futures::StreamExt;
use tokio::sync::{broadcast, mpsc, oneshot, watch};

use comet_doc::{
    DocError, MessagePart, MessageRole, MessageStatus, STREAM_COMMIT_MS, SegmentWriter, SessionDoc,
    fold_event_into_parts, sanitize_tool_call,
};
use comet_harness::{CancellationToken, Harness, RunControls, SteerMessage};
use comet_proto::{
    AgentEvent, ApprovalDecision, ApprovalRequest, DoneStatus, HarnessId, RunRequest, RuntimeMode,
    Session, SessionStatus, SubagentStatus, ToolDiff, UserInputAnswer, UserInputQuestion,
};
use comet_sync::{PutToolDiffOutcome, ToolDiffLimit};

use crate::doc_host::{ChatDocHandle, DocHost};
use crate::registry::HarnessRegistry;
use crate::run_journal::{JournalError, RunJournal};
use crate::{EngineError, Presence, WaitKind, due_for_expiry, new_id, now_ms, unattended_note};

/// One journaled event: the durable seq plus the event, as broadcast to subscribers.
#[derive(Debug, Clone)]
pub struct JournaledEvent {
    pub seq: u64,
    pub event: AgentEvent,
}

/// Outcome of a steer attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SteerOutcome {
    /// Delivered into the live run's steering mailbox.
    Accepted,
    /// No live steerable run — the caller should dispatch the prompt as a new turn.
    NotSteerable,
}

/// A deleted chat cannot retire its durable session state until its prior run
/// has actually left the run map.
#[derive(Debug, thiserror::Error)]
pub(crate) enum SessionCleanupError {
    #[error("chat still has a live run")]
    LiveRun,
    #[error(transparent)]
    Journal(#[from] JournalError),
    #[error(transparent)]
    Workspace(#[from] comet_doc::DocError),
}

/// A parked input question: the resolver, plus when it parked.
pub(crate) struct PendingInput {
    pub(crate) resolver: oneshot::Sender<Vec<UserInputAnswer>>,
    pub(crate) parked_at: DateTime<Utc>,
}

type PendingInputs = Arc<Mutex<HashMap<String, PendingInput>>>;

/// A parked approval: the resolver, plus what would have to match for a later
/// identical request to be auto-allowed. `None` = never allowlistable.
pub(crate) struct PendingApproval {
    pub(crate) signature: Option<String>,
    pub(crate) resolver: oneshot::Sender<ApprovalDecision>,
    /// When this wait started. The unattended sweeper needs it; nothing else
    /// reads it. Wall clock, because a sleeping laptop has really been waiting.
    pub(crate) parked_at: DateTime<Utc>,
}

pub(crate) type PendingApprovals = Arc<Mutex<HashMap<String, PendingApproval>>>;
/// Ids this engine minted. The authority record the `ApprovalRequested` guard
/// checks — separate from "still open", because an auto-allowed request is
/// resolved before the guard ever sees its event.
pub(crate) type MintedApprovals = Arc<Mutex<HashSet<String>>>;
/// Signatures the user allowed for the rest of this run.
pub(crate) type SessionAllowlist = Arc<Mutex<HashSet<String>>>;

/// Park an approval where `respond_approval` will find it — unless the user
/// already allowed this exact action for the session, in which case the
/// resolver comes back unparked for the caller to answer itself.
///
/// **`pending` is taken first and held across the read of `session_allowed`,
/// and `respond_approval` takes the same two in the same order and holds
/// `pending` across its write.** That single ordering is the whole point of
/// this function existing. The check used to run under `session_allowed`
/// alone, release it, and only then take `pending` — so a grant landing in
/// between wrote its signature and swept a `pending` this request had not
/// reached yet, leaving it open. The user was asked for a click a moment after
/// saying "don't ask again". It failed closed (asked rather than allowed), so
/// it was never a permission hole, only a promise the UI visibly broke.
fn park_unless_session_allows(
    pending: &PendingApprovals,
    session_allowed: &SessionAllowlist,
    request_id: &str,
    signature: Option<String>,
    resolver: oneshot::Sender<ApprovalDecision>,
) -> Option<oneshot::Sender<ApprovalDecision>> {
    let mut pending = lock(pending);
    if signature
        .as_deref()
        .is_some_and(|sig| lock(session_allowed).contains(sig))
    {
        return Some(resolver);
    }
    pending.insert(
        request_id.to_string(),
        PendingApproval {
            signature,
            resolver,
            parked_at: Utc::now(),
        },
    );
    None
}

/// A harness-native session id plus the cwd it was created under. Harness
/// session stores are cwd-scoped (claude keys conversations by project
/// directory — comet sessions.ts:563 "harness session stores are keyed by
/// cwd"), so resume is only injected for runs launched from the same cwd.
#[derive(Debug, Clone)]
struct HarnessSessionRef {
    session_id: String,
    cwd: String,
}

struct RunHandle {
    run_id: String,
    steerable: bool,
    /// Whether this run's provider can deny an approval and interrupt the turn
    /// as one atomic provider decision.
    supports_approval_interrupt: bool,
    steer_tx: mpsc::Sender<SteerMessage>,
    /// Harness-level cancellation (protocol interrupt + child teardown).
    interrupt_token: CancellationToken,
    /// Engine-level cancel: arms the run task's grace deadline so a harness that
    /// ignores its token can never strand the run.
    cancel: watch::Sender<Option<DoneStatus>>,
    engine_tx: mpsc::UnboundedSender<AgentEvent>,
    pending_inputs: PendingInputs,
    pending_approvals: PendingApprovals,
    minted_approvals: MintedApprovals,
    session_allowed: SessionAllowlist,
}

impl RunHandle {
    /// The earliest still-parked wait, and which kind it is. Derived from the
    /// maps rather than cached in a field: a cached "blocked" flag is one more
    /// thing to keep in sync, and the sync bug is silent.
    ///
    /// Approvals win ties — the note names the wait with a permission
    /// consequence.
    fn blocked_since(&self) -> Option<(DateTime<Utc>, WaitKind)> {
        let approval = lock(&self.pending_approvals)
            .values()
            .map(|p| p.parked_at)
            .min();
        let input = lock(&self.pending_inputs)
            .values()
            .map(|p| p.parked_at)
            .min();
        match (approval, input) {
            (Some(a), Some(i)) if i < a => Some((i, WaitKind::Answer)),
            (Some(a), _) => Some((a, WaitKind::Approval)),
            (None, Some(i)) => Some((i, WaitKind::Answer)),
            (None, None) => None,
        }
    }
}

/// A parked wait as re-read immediately before its unattended expiry, rather
/// than as the collect pass saw it.
struct LiveWait {
    /// Which run this is *now*. `dispatch_inner` inserts a replacement handle
    /// under the same chat id when an interrupt did not settle inside its
    /// bounded wait, so the chat id alone does not identify a run.
    run_id: String,
    parked_at: DateTime<Utc>,
    kind: WaitKind,
    engine_tx: mpsc::UnboundedSender<AgentEvent>,
}

/// May the run collected as due still be expired? Every `false` means DO NOT
/// expire — this is the fail-closed half of the unattended sweep.
///
/// `interrupt` bounded-waits up to 5s per run, so the gap between the collect
/// pass and this run's turn is tens of seconds when several chats are parked.
/// That is long enough for three separate things to invalidate the decision,
/// and all three have to be ruled out:
///
/// - The user answered this card while an earlier run was settling. `live` is
///   `None` (no handle, or a handle with nothing parked), so a run that is now
///   actively progressing is not killed.
/// - A steer or a re-dispatch replaced the handle. The run ids differ, and
///   interrupting by chat id would hit the successor.
/// - A client attached and left again. Re-reading only *whether* the engine is
///   unattended is not enough: that read is `Some` again at a LATER instant,
///   which restarts the window rather than proving nobody came back. Six chats
///   past a 24h deadline, a client that connects at second 2 and quits at
///   second 20, and a decision at second 25 would otherwise expire runs whose
///   deadline is now a day away. So the deadline is recomputed, not assumed.
///
/// Reusing the sweep's `now` rather than reading the clock again is deliberate:
/// a stale (earlier) `now` can only make `due_for_expiry` say no.
fn still_expirable(
    collected_run_id: &str,
    live: Option<(&str, DateTime<Utc>)>,
    unattended_since: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    bound: std::time::Duration,
) -> bool {
    let Some((run_id, parked_at)) = live else {
        return false;
    };
    run_id == collected_run_id && due_for_expiry(parked_at, unattended_since, now, bound)
}

impl Drop for RunHandle {
    /// A handle leaves `runs` exactly when its run stops being answerable:
    /// `remove_run` at the end of the run task, or a replacement insert in
    /// `dispatch_inner` when an interrupt did not settle inside its bounded
    /// wait. Either way nothing can answer what this run parked —
    /// `respond_input` and `respond_approval` both look a run up by chat id
    /// and from here on find the successor — so the drain belongs to the
    /// handle's lifetime rather than to one of the paths that end it. The
    /// previous version sat below `remove_run`'s run-id guard and so skipped
    /// exactly the replaced-handle case it was written for.
    ///
    /// The bridge closures hold these maps through an `Arc` of their own, so
    /// dropping the handle is not otherwise what releases them: the harness's
    /// spawned task would await a reply that can never arrive (and, for
    /// claude, leave the CLI blocked on a tool call). A question resolves to
    /// no answers — what `interrupt()` sends — and an approval's resolver is
    /// dropped, which every consumer reads as NOT approved.
    fn drop(&mut self) {
        let parked: Vec<_> = lock(&self.pending_inputs)
            .drain()
            .map(|(_, p)| p.resolver)
            .collect();
        for tx in parked {
            let _ = tx.send(Vec::new());
        }
        // D15: forget the ids as well as the resolvers. `minted_approvals` is
        // what `drive_run`'s authority guard checks, so an `ApprovalRequested`
        // still in flight when this drain runs would otherwise pass the guard
        // and fold a card nothing can answer — its resolver is already gone.
        let drained: Vec<(String, PendingApproval)> =
            lock(&self.pending_approvals).drain().collect();
        forget_minted(&self.minted_approvals, drained.iter().map(|(id, _)| id));
        drop(drained);
    }
}

/// Drop `ids` from a run's minted set, so a late `ApprovalRequested` for one of
/// them is rejected by `drive_run`'s authority guard rather than folded as an
/// open card (D15).
///
/// **Only for ids drained WITHOUT an answer.** A session-grant sweep resolves
/// what it removes — the card is answered and the transcript says so — and must
/// keep its id minted so its own `ApprovalRequested` still passes the guard.
fn forget_minted<'a>(minted: &MintedApprovals, ids: impl Iterator<Item = &'a String>) {
    let mut minted = lock(minted);
    for id in ids {
        minted.remove(id);
    }
}

struct Inner {
    device_id: String,
    journal: Arc<RunJournal>,
    registry: Arc<HarnessRegistry>,
    doc_host: OnceLock<DocHost>,
    /// chat_id → current routable run.
    runs: Mutex<HashMap<String, RunHandle>>,
    /// Every provider task that can still publish under a chat id, including a
    /// task whose routing handle was displaced by a bounded replacement.
    run_owners: Mutex<HashMap<String, HashSet<String>>>,
    /// Wakes token-owned delete finalizers when any ownership pin changes.
    /// The carried value is never read; only the wakeup itself matters.
    run_owners_changed: watch::Sender<()>,
    /// Cancels ownership waits during engine teardown without certifying purge.
    shutdown: CancellationToken,
    /// chat_id → broadcast hub (retained across runs so subscribers survive turns).
    hubs: Mutex<HashMap<String, broadcast::Sender<JournaledEvent>>>,
    statuses: Mutex<HashMap<String, Session>>,
    sessions_tx: watch::Sender<Vec<Session>>,
    /// Last dispatched request per chat — the steer→new-turn fallback re-derives its
    /// run config from this (chat config rows land with the workspace doc in M4).
    last_requests: Mutex<HashMap<String, RunRequest>>,
    /// Harness-native session ids per chat (resume continuity across turns) —
    /// the live-process cache over the durable copy on the workspace chat row
    /// (comet kept the same pair on `chats.harness_session_id`). An empty
    /// session id is the "do not resume" tombstone after a rejected resume.
    harness_sessions: Mutex<HashMap<String, HarnessSessionRef>>,
    /// Auto-titler for untitled chats (wired at engine assembly; absent in bare tests).
    titles: OnceLock<crate::titles::TitleGenerator>,
    #[cfg(test)]
    run_registration_pause: Mutex<Option<RunRegistrationPause>>,
    #[cfg(test)]
    terminal_handoff_pause: Mutex<Option<TerminalHandoffPause>>,
    #[cfg(test)]
    resume_retry_pause: Mutex<Option<ResumeRetryPause>>,
}

#[cfg(test)]
struct RunRegistrationPause {
    chat_id: String,
    reached: oneshot::Sender<Arc<ChatDocHandle>>,
    release: oneshot::Receiver<()>,
}

#[cfg(test)]
struct TerminalHandoffPause {
    chat_id: String,
    reached: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
    settled: oneshot::Sender<()>,
}

#[cfg(test)]
struct ResumeRetryPause {
    chat_id: String,
    reached: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Clone)]
pub struct SessionsEngine {
    inner: Arc<Inner>,
}

enum DispatchGeneration {
    RetryDoc(Arc<SessionDoc>),
    CommandHandle(Arc<ChatDocHandle>),
}

impl SessionsEngine {
    pub fn new(
        device_id: String,
        journal: Arc<RunJournal>,
        registry: Arc<HarnessRegistry>,
    ) -> Self {
        let (sessions_tx, _) = watch::channel(Vec::new());
        let (run_owners_changed, _) = watch::channel(());
        Self {
            inner: Arc::new(Inner {
                device_id,
                journal,
                registry,
                doc_host: OnceLock::new(),
                runs: Mutex::new(HashMap::new()),
                run_owners: Mutex::new(HashMap::new()),
                run_owners_changed,
                shutdown: CancellationToken::new(),
                hubs: Mutex::new(HashMap::new()),
                statuses: Mutex::new(HashMap::new()),
                sessions_tx,
                last_requests: Mutex::new(HashMap::new()),
                harness_sessions: Mutex::new(HashMap::new()),
                titles: OnceLock::new(),
                #[cfg(test)]
                run_registration_pause: Mutex::new(None),
                #[cfg(test)]
                terminal_handoff_pause: Mutex::new(None),
                #[cfg(test)]
                resume_retry_pause: Mutex::new(None),
            }),
        }
    }

    /// Wire the doc host (called once at engine assembly; the two services are mutually
    /// referential by design — sessions stream into docs, docs execute commands here).
    pub fn set_doc_host(&self, host: DocHost) {
        let _ = self.inner.doc_host.set(host);
    }

    #[cfg(test)]
    pub(crate) fn pause_next_run_registration_for_test(
        &self,
        chat_id: &str,
    ) -> (oneshot::Receiver<Arc<ChatDocHandle>>, oneshot::Sender<()>) {
        let (reached_tx, reached_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let mut slot = lock(&self.inner.run_registration_pause);
        assert!(slot.is_none(), "only one registration pause may be armed");
        *slot = Some(RunRegistrationPause {
            chat_id: chat_id.to_string(),
            reached: reached_tx,
            release: release_rx,
        });
        (reached_rx, release_tx)
    }

    #[cfg(test)]
    async fn pause_run_registration_if_requested(
        &self,
        chat_id: &str,
        handle: &Arc<ChatDocHandle>,
    ) {
        let pause = {
            let mut slot = lock(&self.inner.run_registration_pause);
            if slot.as_ref().is_some_and(|pause| pause.chat_id == chat_id) {
                slot.take()
            } else {
                None
            }
        };
        let Some(pause) = pause else { return };
        let _ = pause.reached.send(handle.clone());
        let _ = pause.release.await;
    }

    #[cfg(test)]
    pub(crate) fn pause_next_terminal_handoff_for_test(
        &self,
        chat_id: &str,
    ) -> (
        oneshot::Receiver<()>,
        oneshot::Sender<()>,
        oneshot::Receiver<()>,
    ) {
        let (reached_tx, reached_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let (settled_tx, settled_rx) = oneshot::channel();
        let mut slot = lock(&self.inner.terminal_handoff_pause);
        assert!(
            slot.is_none(),
            "only one terminal handoff pause may be armed"
        );
        *slot = Some(TerminalHandoffPause {
            chat_id: chat_id.to_string(),
            reached: reached_tx,
            release: release_rx,
            settled: settled_tx,
        });
        (reached_rx, release_tx, settled_rx)
    }

    #[cfg(test)]
    pub(crate) fn pause_next_resume_retry_for_test(
        &self,
        chat_id: &str,
    ) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (reached_tx, reached_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let mut slot = lock(&self.inner.resume_retry_pause);
        assert!(slot.is_none(), "only one resume retry pause may be armed");
        *slot = Some(ResumeRetryPause {
            chat_id: chat_id.to_string(),
            reached: reached_tx,
            release: release_rx,
        });
        (reached_rx, release_tx)
    }

    #[cfg(test)]
    async fn pause_resume_retry_if_requested(&self, chat_id: &str) {
        let pause = {
            let mut slot = lock(&self.inner.resume_retry_pause);
            if slot.as_ref().is_some_and(|pause| pause.chat_id == chat_id) {
                slot.take()
            } else {
                None
            }
        };
        let Some(pause) = pause else { return };
        let _ = pause.reached.send(());
        let _ = pause.release.await;
    }

    /// Wire the chat auto-titler (called once at engine assembly). After each
    /// completed exchange the run task fires it for still-untitled chats.
    pub fn set_titles(&self, titles: crate::titles::TitleGenerator) {
        let _ = self.inner.titles.set(titles);
    }

    fn doc_host(&self) -> Result<&DocHost, EngineError> {
        self.inner
            .doc_host
            .get()
            .ok_or_else(|| EngineError::Other("doc host not wired into sessions engine".into()))
    }

    fn doc_handle(&self, chat_id: &str) -> Result<Arc<ChatDocHandle>, EngineError> {
        self.doc_host()?.open(chat_id)
    }

    /// Status watch: the full session list, re-sent on every transition.
    pub fn watch_sessions(&self) -> watch::Receiver<Vec<Session>> {
        self.inner.sessions_tx.subscribe()
    }

    pub fn session_status(&self, chat_id: &str) -> Option<Session> {
        lock(&self.inner.statuses).get(chat_id).cloned()
    }

    /// Any run currently working or blocked on input — the auto-updater's
    /// "don't restart from under a session" gate.
    pub fn any_active(&self) -> bool {
        lock(&self.inner.statuses).values().any(|s| {
            matches!(
                s.status,
                comet_proto::SessionStatus::Working | comet_proto::SessionStatus::AwaitingInput
            )
        })
    }

    /// The last request dispatched for a chat (steer→new-turn fallback).
    ///
    /// This carries the mode (and sandbox) of the *previous* turn, not the
    /// chat row's current one — it is stamped at dispatch and never touched
    /// again. Since 1.8 a user can change the mode mid-chat, so every caller
    /// that dispatches from this overlays the row's current mode first with
    /// [`DocHost::apply_chat_row_runtime_mode`]; without that the divergence
    /// runs in the permissive direction (`docs/debt/README.md` D11).
    pub fn last_request(&self, chat_id: &str) -> Option<RunRequest> {
        lock(&self.inner.last_requests).get(chat_id).cloned()
    }

    /// Subscribe to a chat's live event stream: returns the journal replay after
    /// `after_seq` plus a live receiver. Subscribe-then-replay ordering means overlap
    /// (dedupe by seq) rather than gaps.
    pub fn subscribe(
        &self,
        chat_id: &str,
        after_seq: u64,
    ) -> Result<(Vec<JournaledEvent>, broadcast::Receiver<JournaledEvent>), EngineError> {
        let rx = {
            let mut hubs = lock(&self.inner.hubs);
            hubs.entry(chat_id.to_string())
                .or_insert_with(|| broadcast::channel(1024).0)
                .subscribe()
        };
        let replay = self
            .inner
            .journal
            .replay(chat_id, after_seq)?
            .into_iter()
            .map(|(seq, event)| JournaledEvent { seq, event })
            .collect();
        Ok((replay, rx))
    }

    /// Start (or route) a run for `chat_id`.
    ///
    /// - The user message entry is written to the doc immediately (id = `message_id`).
    /// - A live steerable run receives the prompt as its next turn via the mailbox
    ///   (comet's persistent-session routing); otherwise any live run is interrupted
    ///   first — never two runtimes driving one chat.
    pub async fn dispatch(
        &self,
        chat_id: &str,
        harness_id: HarnessId,
        request: RunRequest,
        message_id: Option<String>,
    ) -> Result<String, EngineError> {
        self.dispatch_with(chat_id, harness_id, request, message_id, true, None)
            .await
    }

    pub(crate) async fn dispatch_for_generation(
        &self,
        generation: &Arc<ChatDocHandle>,
        harness_id: HarnessId,
        request: RunRequest,
        message_id: Option<String>,
    ) -> Result<String, EngineError> {
        self.dispatch_with(
            generation.chat_id(),
            harness_id,
            request,
            message_id,
            true,
            Some(DispatchGeneration::CommandHandle(generation.clone())),
        )
        .await
    }

    /// [`Self::dispatch`] with resume injection controllable: the failed-resume
    /// retry re-dispatches with `inject_resume = false` so a session id the
    /// harness just rejected can never be re-injected from the journal.
    /// Boxed future: `drive_run` re-enters this for that retry, and the
    /// erasure breaks the opaque-type cycle the recursion would otherwise form.
    fn dispatch_with<'a>(
        &'a self,
        chat_id: &'a str,
        harness_id: HarnessId,
        request: RunRequest,
        message_id: Option<String>,
        inject_resume: bool,
        expected_generation: Option<DispatchGeneration>,
    ) -> futures::future::BoxFuture<'a, Result<String, EngineError>> {
        Box::pin(self.dispatch_inner(
            chat_id,
            harness_id,
            request,
            message_id,
            inject_resume,
            expected_generation,
        ))
    }

    /// Re-dispatch a rejected engine-injected resume only if it still belongs
    /// to the same document generation that rejected it.
    fn dispatch_retry_for_generation<'a>(
        &'a self,
        chat_id: &'a str,
        harness_id: HarnessId,
        request: RunRequest,
        message_id: String,
        expected_doc: Arc<SessionDoc>,
    ) -> futures::future::BoxFuture<'a, Result<String, EngineError>> {
        self.dispatch_with(
            chat_id,
            harness_id,
            request,
            Some(message_id),
            false,
            Some(DispatchGeneration::RetryDoc(expected_doc)),
        )
    }

    async fn dispatch_inner(
        &self,
        chat_id: &str,
        harness_id: HarnessId,
        mut request: RunRequest,
        message_id: Option<String>,
        inject_resume: bool,
        expected_generation: Option<DispatchGeneration>,
    ) -> Result<String, EngineError> {
        let expected_handle = match expected_generation.as_ref() {
            Some(DispatchGeneration::CommandHandle(handle)) => Some(handle),
            _ => None,
        };
        let routed = if !matches!(
            expected_generation.as_ref(),
            Some(DispatchGeneration::RetryDoc(_))
        ) {
            let route = || {
                lock(&self.inner.runs)
                    .get(chat_id)
                    .map(|h| (h.run_id.clone(), h.steerable, h.steer_tx.clone()))
            };
            match expected_handle {
                Some(handle) => self
                    .doc_host()?
                    .with_current_handle(handle, route)
                    .ok_or(EngineError::ChatCleanupPendingRetry)?,
                None => route(),
            }
        } else {
            None
        };
        if let Some((run_id, steerable, steer_tx)) = routed {
            let message = SteerMessage {
                prompt: request.prompt.clone(),
                message_id: message_id.clone(),
            };
            if steerable && steer_tx.try_send(message).is_ok() {
                let user_id = message_id.unwrap_or_else(new_id);
                if let Some(handle) = expected_handle {
                    let Some(written) = self.doc_host()?.with_current_handle(handle, || {
                        handle.write_user_message(&user_id, &request.prompt, now_ms())
                    }) else {
                        return Err(EngineError::ChatCleanupPendingRetry);
                    };
                    written?;
                } else {
                    let handle = self.doc_handle(chat_id)?;
                    handle.write_user_message(&user_id, &request.prompt, now_ms())?;
                }
                // Working BEFORE the lastMessageAt bump: both ride the
                // workspace doc from this one peer, so causal order makes it
                // impossible for an observer to hold [new message, old status]
                // — that gap read as unseen-with-no-live-run = a phantom
                // "completed" flash on every remote send (2026-07-31).
                self.set_status(chat_id, SessionStatus::Working, false);
                self.inner.note_message(chat_id, &request.prompt);
                return Ok(run_id);
            }
            // Mailbox closed (runtime mid-teardown / non-steering harness): replace it.
            if expected_handle.is_some() {
                self.interrupt_run(chat_id, &run_id).await?;
            } else {
                self.interrupt(chat_id).await?;
            }
        }

        let harness = self.inner.registry.resolve(harness_id)?;
        let handle = match expected_handle {
            Some(handle) => handle.clone(),
            None => self.doc_handle(chat_id)?,
        };
        let user_id = message_id.unwrap_or_else(new_id);

        // Engine-owned resume (comet sessions.ts:736 — every dispatch read the
        // chat's stored harness session): callers always send `resume: None`;
        // the engine threads the chat's prior harness session back in so a new
        // process (app restart) continues the same harness conversation.
        let mut resume_injected = false;
        if request.resume.is_none() && inject_resume {
            request.resume = self.inner.resume_for(chat_id, &request.cwd);
            resume_injected = request.resume.is_some();
        }
        let run_id = new_id();
        let (steer_tx, steer_rx) = mpsc::channel::<SteerMessage>(32);
        let (cancel_tx, cancel_rx) = watch::channel(None);
        let (engine_tx, engine_rx) = mpsc::unbounded_channel::<AgentEvent>();
        let pending_inputs: PendingInputs = Arc::new(Mutex::new(HashMap::new()));

        // Input bridge: the harness asks questions; we mint the request id, park the
        // resolver for `respond_input`, and surface the event through the run pipeline.
        let request_input = {
            let pending = pending_inputs.clone();
            let engine_tx = engine_tx.clone();
            Box::new(move |questions: Vec<UserInputQuestion>| {
                let (tx, rx) = oneshot::channel();
                let request_id = new_id();
                lock(&pending).insert(
                    request_id.clone(),
                    PendingInput {
                        resolver: tx,
                        parked_at: Utc::now(),
                    },
                );
                if engine_tx
                    .send(AgentEvent::InputRequested {
                        request_id: request_id.clone(),
                        questions,
                    })
                    .is_err()
                    && let Some(parked) = lock(&pending).remove(&request_id)
                {
                    // D16, and the exact twin of the approval arm below: the
                    // run task is gone, so no panel will ever be shown for
                    // this question and no drain will ever reach it. Unpark it
                    // here or the harness awaits a reply that cannot arrive —
                    // for Claude, with the CLI blocked on a tool call.
                    //
                    // Resolved to NO ANSWERS rather than dropped, which is the
                    // one place this differs from the approval arm and is not
                    // a stylistic choice: `interrupt()` and `RunHandle::drop`
                    // both send `Vec::new()` for a parked question, and a
                    // dropped input resolver is a different signal to the
                    // consumer than an empty one.
                    let _ = parked.resolver.send(Vec::new());
                }
                rx
            })
        };
        let pending_approvals: PendingApprovals = Arc::new(Mutex::new(HashMap::new()));
        let minted_approvals: MintedApprovals = Arc::new(Mutex::new(HashSet::new()));
        let session_allowed: SessionAllowlist = Arc::new(Mutex::new(HashSet::new()));

        // Approval bridge: same shape as the input bridge above — mint the id,
        // park the resolver, then emit. Parking before emitting is what makes
        // a legitimate id always resolvable by the time the event is seen.
        //
        // A request the user already allowed for this session resolves itself,
        // but is still emitted and then immediately resolved: an auto-allowed
        // action MUST stay visible in the transcript. `blocked_on` keys on open
        // approvals, so a card resolved on arrival never blocks the composer.
        // The id is recorded in `minted_approvals` either way — that, not
        // `pending_approvals`, is what the drive_run guard checks, because this
        // request is already answered by the time the guard runs.
        let request_approval = {
            let pending = pending_approvals.clone();
            let minted = minted_approvals.clone();
            let engine_tx = engine_tx.clone();
            let session_allowed = session_allowed.clone();
            Box::new(move |approval: ApprovalRequest| {
                let (tx, rx) = oneshot::channel();
                let request_id = new_id();
                let signature = crate::approvals::approval_signature(&approval);
                lock(&minted).insert(request_id.clone());
                // Check-and-park is one critical section; see the function.
                let pre_allowed = park_unless_session_allows(
                    &pending,
                    &session_allowed,
                    &request_id,
                    signature,
                    tx,
                );
                if let Some(tx) = pre_allowed {
                    let _ = engine_tx.send(AgentEvent::ApprovalRequested {
                        request_id: request_id.clone(),
                        approval,
                    });
                    // `AllowForSession`, not `Allow`: the user never saw this
                    // one. A card reading "Allowed" for an action Comet
                    // allowed under an earlier grant is a false record, and
                    // the record is the whole reason the card is emitted.
                    // "Allowed for this session" is the true statement, and
                    // it is the label both halves of the grant then carry.
                    // Nothing branches on the distinction: the claude adapter
                    // maps both to `allow_response`, the UI paints both
                    // `ApprovalPaint::Allowed`, and the allowlist is written
                    // only by `respond_approval` — never from this event.
                    let _ = tx.send(ApprovalDecision::AllowForSession);
                    let _ = engine_tx.send(AgentEvent::ApprovalResolved {
                        request_id,
                        decision: ApprovalDecision::AllowForSession,
                    });
                    return rx;
                }
                if engine_tx
                    .send(AgentEvent::ApprovalRequested {
                        request_id: request_id.clone(),
                        approval,
                    })
                    .is_err()
                {
                    // The run task is gone (its handle may already have been
                    // replaced under this chat id), so no card will ever be
                    // shown for this request and no drain will ever reach it.
                    // Fail closed: dropping the resolver resolves the receiver
                    // to `Err`, which every consumer reads as not approved.
                    drop(lock(&pending).remove(&request_id));
                }
                rx
            })
        };
        let interrupt_token = CancellationToken::new();
        let controls = RunControls {
            request_input,
            request_approval,
            steering: steer_rx,
            interrupt: interrupt_token.clone(),
        };

        let run_handle = RunHandle {
            run_id: run_id.clone(),
            steerable: harness.capabilities().supports_steering,
            supports_approval_interrupt: harness.capabilities().supports_approval_interrupt,
            steer_tx,
            interrupt_token,
            cancel: cancel_tx,
            engine_tx,
            pending_inputs,
            pending_approvals,
            minted_approvals,
            session_allowed,
        };

        #[cfg(test)]
        self.pause_run_registration_if_requested(chat_id, &handle)
            .await;

        let registration = self
            .doc_host()?
            .register_run_if_current(&handle, |run_doc| {
                if let Some(DispatchGeneration::RetryDoc(expected)) = expected_generation.as_ref() {
                    if !Arc::ptr_eq(expected, &run_doc) {
                        return Err(EngineError::ChatCleanupPendingRetry);
                    }
                    if lock(&self.inner.runs).contains_key(chat_id) {
                        return Err(EngineError::ChatCleanupPendingRetry);
                    }
                }

                handle.write_user_message(&user_id, &request.prompt, now_ms())?;
                let displaced = self.inner.register_run(chat_id, run_handle);
                Ok((displaced, run_doc))
            })?;
        let (displaced, run_doc) = registration?;
        // A handle we replaced is a run nothing can answer any more: the
        // interrupt above waits only 5s for the old run to settle and inserts
        // regardless. Dropped explicitly, and outside the lock, so its parked
        // questions and approvals are released here rather than at whatever
        // statement boundary the temporary happened to end on
        // (`RunHandle::drop`).
        drop(displaced);
        lock(&self.inner.last_requests).insert(chat_id.to_string(), request.clone());
        self.set_status(chat_id, SessionStatus::Working, true);
        // AFTER Working (same causal-order guarantee as the steer path): the
        // lastMessageAt bump must never be observable ahead of the live run.
        self.inner.note_message(chat_id, &request.prompt);

        // Name the chat NOW, off the first prompt — not after the first
        // exchange completes ("called New session for a long time for no
        // reason"; the titler only needs the prompt and skips titled chats;
        // the Done-time call below stays as the retry for a failed
        // generation). `_upfront` (not `maybe_generate`): for a harness that
        // reports its own title (`titles::harness_self_titles`), this
        // dispatch is skipped entirely rather than racing the agent's own
        // answer — see `TitleGenerator::maybe_generate_upfront`'s doc for
        // the policy.
        if let Some(titles) = self.inner.titles.get() {
            titles.maybe_generate_upfront(
                handle.clone(),
                harness_id,
                &request.prompt,
                &request.cwd,
            );
        }

        tokio::spawn(drive_run(
            self.inner.clone(),
            chat_id.to_string(),
            run_id.clone(),
            harness,
            request,
            handle,
            run_doc,
            controls,
            engine_rx,
            cancel_rx,
            RunResumeState {
                user_message_id: user_id,
                resume_injected,
            },
        ));
        Ok(run_id)
    }

    /// Push a steer prompt into the live run's mailbox. `NotSteerable` when no live
    /// steerable run exists — the caller (command executor) dispatches a new turn.
    pub async fn steer(
        &self,
        chat_id: &str,
        prompt: &str,
        message_id: Option<String>,
    ) -> Result<SteerOutcome, EngineError> {
        let target = lock(&self.inner.runs)
            .get(chat_id)
            .filter(|h| h.steerable)
            .map(|h| h.steer_tx.clone());
        let Some(steer_tx) = target else {
            return Ok(SteerOutcome::NotSteerable);
        };
        let message = SteerMessage {
            prompt: prompt.to_string(),
            message_id: message_id.clone(),
        };
        if steer_tx.try_send(message).is_err() {
            return Ok(SteerOutcome::NotSteerable);
        }
        // Accepted: the steer prompt becomes a user entry immediately (client-minted id).
        let user_id = message_id.unwrap_or_else(new_id);
        let handle = self.doc_handle(chat_id)?;
        handle.write_user_message(&user_id, prompt, now_ms())?;
        self.inner.note_message(chat_id, prompt);
        Ok(SteerOutcome::Accepted)
    }

    /// Interrupt the live run, if any. The run settles with a synthetic
    /// `Done{interrupted}` and its streaming entry stamped `aborted`; this waits
    /// (bounded) for that settlement so callers observe a consistent doc.
    pub async fn interrupt(&self, chat_id: &str) -> Result<bool, EngineError> {
        self.interrupt_matching(chat_id, None, DoneStatus::Interrupted)
            .await
    }

    async fn interrupt_run(&self, chat_id: &str, run_id: &str) -> Result<bool, EngineError> {
        self.interrupt_matching(chat_id, Some(run_id), DoneStatus::Interrupted)
            .await
    }

    pub(crate) async fn interrupt_for_generation(
        &self,
        generation: &Arc<ChatDocHandle>,
    ) -> Result<bool, EngineError> {
        let run_id = self
            .doc_host()?
            .with_current_handle(generation, || {
                lock(&self.inner.runs)
                    .get(generation.chat_id())
                    .map(|handle| handle.run_id.clone())
            })
            .ok_or(EngineError::ChatCleanupPendingRetry)?;
        match run_id {
            Some(run_id) => self.interrupt_run(generation.chat_id(), &run_id).await,
            None => Ok(false),
        }
    }

    async fn interrupt_with_status(
        &self,
        chat_id: &str,
        terminal_status: DoneStatus,
    ) -> Result<bool, EngineError> {
        self.interrupt_matching(chat_id, None, terminal_status)
            .await
    }

    /// The one implementation. `expected_run_id` bounds the interrupt to an exact
    /// run (D82); `terminal_status` is the reason the settle records (D26).
    async fn interrupt_matching(
        &self,
        chat_id: &str,
        expected_run_id: Option<&str>,
        terminal_status: DoneStatus,
    ) -> Result<bool, EngineError> {
        let target = lock(&self.inner.runs)
            .get(chat_id)
            .filter(|handle| expected_run_id.is_none_or(|expected| handle.run_id == expected))
            .map(|h| {
                (
                    h.run_id.clone(),
                    h.interrupt_token.clone(),
                    h.cancel.clone(),
                    h.pending_inputs.clone(),
                    h.pending_approvals.clone(),
                    h.minted_approvals.clone(),
                )
            });
        let Some((run_id, token, cancel, pending_inputs, pending_approvals, minted_approvals)) =
            target
        else {
            return Ok(false);
        };
        // Unpark any blocked question FIRST (mirrors comet: harness teardown can await a
        // parked question callback — a run stuck on a question would deadlock the stop).
        let parked: Vec<_> = lock(&pending_inputs)
            .drain()
            .map(|(_, p)| p.resolver)
            .collect();
        for tx in parked {
            let _ = tx.send(Vec::new());
        }
        // Same reason for a parked approval. Dropping the senders resolves the
        // receivers to an error, which a run must treat as not approved — the
        // same signal every non-answering caller of this bridge produces.
        let drained: Vec<(String, PendingApproval)> = lock(&pending_approvals).drain().collect();
        // …and forget the ids, for the reason `RunHandle::drop` does (D15).
        forget_minted(&minted_approvals, drained.iter().map(|(id, _)| id));
        drop(drained);
        // Harness-level interrupt (protocol + child teardown) …
        token.cancel();
        // … plus the engine-side grace deadline in the run task, so a harness that
        // ignores its token still settles with a synthesized Done{interrupted}.
        cancel.send_if_modified(|status| {
            if status.is_none() {
                *status = Some(terminal_status);
                true
            } else {
                false
            }
        });
        // Bounded settle wait (the run task appends Done + stamps `aborted`).
        for _ in 0..500 {
            if !self.is_live(chat_id, &run_id) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        Ok(true)
    }

    /// Whether a provider run still owns this chat id. Reconciliation checks
    /// this under DocHost's lifecycle gate as a defensive fallback for a run
    /// whose warm-handle map entry was already retired.
    pub(crate) fn has_live_run(&self, chat_id: &str) -> bool {
        self.inner.has_run_owner(chat_id)
    }

    /// Wait for every run registered before a token-owned final purge to stop
    /// writing under this id. The caller's purge lifecycle prevents a newer
    /// generation from registering while this wait is active.
    pub(crate) async fn wait_for_no_live_runs(&self, chat_id: &str) -> bool {
        // Subscribe before the first check so the final release cannot land in
        // a check-then-subscribe gap and leave this waiting forever.
        let mut owners_changed = self.inner.run_owners_changed.subscribe();
        loop {
            if !self.has_live_run(chat_id) {
                return true;
            }
            if self.inner.shutdown.is_cancelled() {
                return false;
            }
            tokio::select! {
                changed = owners_changed.changed() => {
                    if changed.is_err() {
                        return false;
                    }
                }
                () = self.inner.shutdown.cancelled() => {
                    return false;
                }
            }
        }
    }

    /// Retire every session-owned remnant of a deleted chat. Callers hold the
    /// lifecycle gate, so a new generation cannot register between the live-run
    /// check and the durable journal discard.
    pub(crate) fn cleanup_deleted_chat(&self, chat_id: &str) -> Result<(), SessionCleanupError> {
        if self.has_live_run(chat_id) {
            return Err(SessionCleanupError::LiveRun);
        }
        self.inner.journal.discard(chat_id)?;
        if let Some(workspace) = self.inner.workspace() {
            workspace.delete_session(chat_id)?;
        }

        lock(&self.inner.harness_sessions).remove(chat_id);
        lock(&self.inner.last_requests).remove(chat_id);
        let sessions = {
            let mut statuses = lock(&self.inner.statuses);
            if statuses.remove(chat_id).is_none() {
                None
            } else {
                let mut sessions: Vec<_> = statuses.values().cloned().collect();
                sessions.sort_by(|a, b| a.chat_id.cmp(&b.chat_id));
                Some(sessions)
            }
        };
        if let Some(sessions) = sessions {
            self.inner.sessions_tx.send_replace(sessions);
        }
        lock(&self.inner.hubs).remove(chat_id);
        Ok(())
    }

    /// The `runs` entry for `chat_id`, re-read for the expiry decision. `None`
    /// when no handle exists any more or when nothing is parked on it.
    ///
    /// Split out so the read is one short critical section: the caller must not
    /// be holding `runs` when it awaits `interrupt`.
    fn live_wait(&self, chat_id: &str) -> Option<LiveWait> {
        lock(&self.inner.runs).get(chat_id).and_then(|handle| {
            let (parked_at, kind) = handle.blocked_since()?;
            Some(LiveWait {
                run_id: handle.run_id.clone(),
                parked_at,
                kind,
                engine_tx: handle.engine_tx.clone(),
            })
        })
    }

    /// End every turn whose parked wait nobody can answer any more. Returns how
    /// many were ended.
    ///
    /// `now` and `bound` are parameters, not reads of the clock and config, so
    /// a test can drive one sweep instead of waiting out a 24-hour bound. This
    /// is the same shape as `session_status` and `interrupt` taking their
    /// inputs explicitly rather than reaching for ambient state.
    ///
    /// Two passes, and the split matters: the first collects candidates under
    /// the `runs` lock, the second re-reads each one and only then interrupts.
    /// Everything the collect learned is treated as stale by the time the
    /// interrupt runs, because `interrupt` bounded-waits up to 5s per run, so a
    /// sweep over several parked chats can spend tens of seconds inside this
    /// loop. See [`still_expirable`] for what the re-read has to prove.
    pub async fn expire_unattended(
        &self,
        presence: &Presence,
        now: DateTime<Utc>,
        bound: std::time::Duration,
    ) -> usize {
        let unattended_since = presence.unattended_since();
        // Answerable: no bound applies, and no lock needs taking.
        if unattended_since.is_none() {
            return 0;
        }

        // Collect first, act second. `interrupt` is async and takes the same
        // locks, so holding `runs` across it would deadlock. Only the identity
        // survives the lock release — the facts the decision rests on are all
        // re-read below.
        let due: Vec<(String, String)> = lock(&self.inner.runs)
            .iter()
            .filter_map(|(chat_id, handle)| {
                let (parked_at, _kind) = handle.blocked_since()?;
                due_for_expiry(parked_at, unattended_since, now, bound)
                    .then(|| (chat_id.clone(), handle.run_id.clone()))
            })
            .collect();

        let mut ended = 0;
        for (chat_id, collected_run_id) in due {
            // Re-read the run and the stretch, then decide. Both reads release
            // their locks before the await below.
            let live = self.live_wait(&chat_id);
            let still_due = still_expirable(
                &collected_run_id,
                live.as_ref().map(|l| (l.run_id.as_str(), l.parked_at)),
                presence.unattended_since(),
                now,
                bound,
            );
            let Some(live) = live.filter(|_| still_due) else {
                tracing::debug!(
                    chat = %chat_id,
                    run = %collected_run_id,
                    "skipping an unattended expiry that stopped being due mid-sweep"
                );
                continue;
            };
            // Note BEFORE interrupt: this folds into the live entry via
            // `engine_tx`, and after the interrupt the entry is finished and
            // nothing can add to it — the same failure `expire_open_approvals`'
            // doc comment warns about. `kind` comes from the re-read too, so
            // the wording names the wait that is open now.
            let _ = live.engine_tx.send(AgentEvent::Error {
                message: unattended_note(bound, live.kind),
            });
            match self
                .interrupt_with_status(&chat_id, DoneStatus::Expired)
                .await
            {
                Ok(true) => {
                    ended += 1;
                    tracing::info!(
                        chat = %chat_id,
                        bound_secs = bound.as_secs(),
                        "ended a turn no connected client could answer"
                    );
                }
                Ok(false) => {}
                Err(err) => tracing::warn!(
                    chat = %chat_id,
                    error = %err,
                    "unattended expiry could not settle the run"
                ),
            }
        }
        ended
    }

    /// Resolve a pending `request_input` question set. Returns `false` when no such
    /// request is pending (unknown id, or the run already settled).
    pub fn respond_input(
        &self,
        chat_id: &str,
        request_id: &str,
        answers: Vec<UserInputAnswer>,
    ) -> Result<bool, EngineError> {
        let target = lock(&self.inner.runs)
            .get(chat_id)
            .map(|h| (h.pending_inputs.clone(), h.engine_tx.clone()));
        let Some((pending, engine_tx)) = target else {
            return Ok(false);
        };
        let Some(pending_input) = lock(&pending).remove(request_id) else {
            return Ok(false);
        };
        let _ = pending_input.resolver.send(answers);
        let _ = engine_tx.send(AgentEvent::InputResolved {
            request_id: request_id.to_string(),
        });
        Ok(true)
    }

    /// Resolve a pending approval. Returns `false` when no such request is
    /// pending — an unknown id, or a run that already settled.
    pub fn respond_approval(
        &self,
        chat_id: &str,
        request_id: &str,
        decision: ApprovalDecision,
    ) -> Result<bool, EngineError> {
        let target = lock(&self.inner.runs).get(chat_id).map(|h| {
            (
                h.pending_approvals.clone(),
                h.session_allowed.clone(),
                h.engine_tx.clone(),
                h.supports_approval_interrupt,
            )
        });
        let Some((pending, session_allowed, engine_tx, supports_approval_interrupt)) = target
        else {
            return Ok(false);
        };
        if matches!(decision, ApprovalDecision::DenyAndInterrupt { .. })
            && !supports_approval_interrupt
        {
            tracing::warn!(
                chat_id,
                request_id,
                "approval interrupt rejected: active harness does not support it"
            );
            return Ok(false);
        }
        // One critical section, and `pending` is held across the write to
        // `session_allowed` — the same lock order `park_unless_session_allows`
        // takes, and for the reason documented there: a request being minted
        // must land either wholly before this grant (and be swept below) or
        // wholly after it (and read the signature back). Nothing is sent from
        // in here; the resolvers travel out and are answered below.
        let (parked, also_granted) = {
            let mut pending = lock(&pending);
            let Some(parked) = pending.remove(request_id) else {
                return Ok(false);
            };
            // "Allow for this session" on an action Comet could not identify
            // (`signature: None`) allows THIS call only — no rule to write.
            let mut also_granted: Vec<(String, oneshot::Sender<ApprovalDecision>)> = Vec::new();
            if decision == ApprovalDecision::AllowForSession
                && let Some(signature) = parked.signature.as_deref()
            {
                lock(&session_allowed).insert(signature.to_string());
                // The grant applies to identical requests ALREADY waiting, not
                // just to ones minted after it: claude batches parallel tool
                // calls, so two copies of the same action can be parked at once
                // and the pre-allow check at mint time ran before either.
                // Leaving the second one open asks for a click one second after
                // the user said not to ask again.
                let same: Vec<String> = pending
                    .iter()
                    .filter(|(_, p)| p.signature.as_deref() == Some(signature))
                    .map(|(id, _)| id.clone())
                    .collect();
                for id in same {
                    if let Some(p) = pending.remove(&id) {
                        also_granted.push((id, p.resolver));
                    }
                }
            }
            (parked, also_granted)
        };
        let _ = parked.resolver.send(decision.clone());
        let _ = engine_tx.send(AgentEvent::ApprovalResolved {
            request_id: request_id.to_string(),
            decision,
        });
        // Same shape as the mint-time pre-allowed path, including the event:
        // an action Comet allowed on the user's behalf stays visible.
        for (id, resolver) in also_granted {
            let _ = resolver.send(ApprovalDecision::AllowForSession);
            let _ = engine_tx.send(AgentEvent::ApprovalResolved {
                request_id: id,
                decision: ApprovalDecision::AllowForSession,
            });
        }
        Ok(true)
    }

    /// Boot recovery: for every journal whose last event is not `Done` (a run died
    /// mid-stream), stamp this device's abandoned `streaming` doc entries `aborted`
    /// with a VISIBLE "Run interrupted by engine restart" error part, close the
    /// journal with a synthetic `Done{interrupted}` — and then PICK THE RUN BACK
    /// UP: a fresh crashed turn with revival budget left is re-dispatched against
    /// the remembered harness session (comet: "not just eulogized";
    /// `MAX_AUTO_RESUME` = 3 consecutive revivals, fresh = crashed < 12h ago).
    pub fn recover_stale(&self) -> Result<usize, EngineError> {
        const MAX_AUTO_RESUME: u32 = 3;
        const RESUME_FRESH_MS: i64 = 12 * 60 * 60 * 1000;

        let stale = self.inner.journal.stale_sessions()?;
        let mut recovered = 0usize;
        for chat_id in stale {
            if lock(&self.inner.runs).contains_key(&chat_id) {
                continue; // a live run owns this journal
            }
            let handle = self.doc_handle(&chat_id)?;
            // Harness continuity first: the crashed run's session id may only
            // exist in the journal (the debounced workspace-row write may
            // never have landed) — remember it so the revived run resumes the
            // same harness conversation (comet recoverDraft, sessions.ts:538).
            if let Some((session_id, cwd, _)) = self.inner.journal_harness_session(&chat_id) {
                self.inner
                    .remember_harness_session(&chat_id, &session_id, &cwd);
            }
            // The revival prompt: the last user message (idempotent re-dispatch
            // under the SAME id — `write_user_message` dedupes by id, so the
            // transcript never shows a duplicate).
            let prompt = handle.doc().read_entries().ok().and_then(|entries| {
                entries
                    .iter()
                    .rev()
                    .find(|e| e.role == MessageRole::User)
                    .and_then(|e| {
                        e.parts.iter().find_map(|p| match p {
                            MessagePart::Text { text, .. } => Some((e.id.clone(), text.clone())),
                            _ => None,
                        })
                    })
            });
            let attempts = self.inner.journal.resume_attempts(&chat_id);
            let fresh = handle
                .doc()
                .read_entries()
                .ok()
                .and_then(|entries| {
                    entries
                        .iter()
                        .rev()
                        .find(|e| e.status == Some(MessageStatus::Streaming))
                        .map(|e| now_ms() - e.created_at < RESUME_FRESH_MS)
                })
                .unwrap_or(false);
            let will_resume = fresh && prompt.is_some() && attempts < MAX_AUTO_RESUME;

            let note = if will_resume {
                "Run interrupted by engine restart — resuming"
            } else {
                "Run interrupted by engine restart"
            };
            let done = AgentEvent::Done {
                status: DoneStatus::Interrupted,
                result: None,
                error: Some(note.into()),
                session_id: None,
            };
            self.inner.publish(&chat_id, &done);
            let stamped = handle.mark_abandoned_streams(note)?.len();
            self.set_status(&chat_id, SessionStatus::Idle, false);
            tracing::info!(chat = %chat_id, stamped, will_resume, attempts, "recovered stale session journal");
            recovered += 1;

            if !will_resume {
                continue;
            }
            let attempt = self.inner.journal.note_resume_attempt(&chat_id);
            let (user_id, prompt_text) = prompt.expect("gated by will_resume");
            let sessions = self.clone();
            tokio::spawn(async move {
                let Some(host) = sessions.inner.doc_host.get().cloned() else {
                    return;
                };
                let request = sessions
                    .last_request(&chat_id)
                    .or_else(|| host.request_from_chat_row(&chat_id, &prompt_text))
                    // Last resort: the journal's own cwd and mode (comet's
                    // draft config) — a crash can predate the debounced
                    // workspace-row write, and the chat a new run belongs to
                    // takes its mode from the composer draft rather than from
                    // a stored row. Resuming under the default instead of the
                    // recorded mode would write where the user asked to be
                    // asked.
                    .or_else(|| {
                        let (_, cwd, runtime_mode) =
                            sessions.inner.journal_harness_session(&chat_id)?;
                        Some(RunRequest {
                            cwd,
                            ..RunRequest::for_session(runtime_mode)
                        })
                    });
                let Some(mut request) = request else {
                    tracing::warn!(chat = %chat_id, "auto-resume skipped: no run config");
                    return;
                };
                request.prompt = prompt_text;
                // The remembered request predates any mode change the user made
                // while the run was dead; the chat row is the authority.
                host.apply_chat_row_runtime_mode(&chat_id, &mut request);
                request.resume = None; // dispatch re-injects the remembered session
                request.attachments = Vec::new();
                let harness_id = host.harness_for_request(&chat_id, &request);
                match sessions
                    .dispatch(&chat_id, harness_id, request, Some(user_id))
                    .await
                {
                    Ok(_) => {
                        tracing::info!(chat = %chat_id, attempt, "auto-resumed crashed run")
                    }
                    Err(err) => {
                        tracing::warn!(chat = %chat_id, error = %err, "auto-resume dispatch failed")
                    }
                }
            });
        }
        Ok(recovered)
    }

    /// Graceful shutdown: interrupt every live run so streaming entries settle.
    pub async fn shutdown(&self) {
        self.inner.shutdown.cancel();
        let chats: Vec<String> = lock(&self.inner.runs).keys().cloned().collect();
        for chat_id in chats {
            if let Err(err) = self.interrupt(&chat_id).await {
                tracing::warn!(chat = %chat_id, error = %err, "shutdown interrupt failed");
            }
        }
    }

    fn is_live(&self, chat_id: &str, run_id: &str) -> bool {
        lock(&self.inner.runs)
            .get(chat_id)
            .is_some_and(|h| h.run_id == run_id)
    }

    fn set_status(&self, chat_id: &str, status: SessionStatus, fresh_start: bool) {
        self.inner.set_status(chat_id, status, fresh_start);
    }
}

impl Inner {
    /// Install the current routing handle and independently pin the provider
    /// task in the lifecycle census. The caller holds DocHost's purge gate.
    fn register_run(&self, chat_id: &str, run_handle: RunHandle) -> Option<RunHandle> {
        let run_id = run_handle.run_id.clone();
        let inserted = lock(&self.run_owners)
            .entry(chat_id.to_string())
            .or_default()
            .insert(run_id);
        if inserted {
            let _ = self.run_owners_changed.send(());
        }
        lock(&self.runs).insert(chat_id.to_string(), run_handle)
    }

    fn has_run_owner(&self, chat_id: &str) -> bool {
        lock(&self.run_owners)
            .get(chat_id)
            .is_some_and(|owners| !owners.is_empty())
    }

    fn release_run_owner(&self, chat_id: &str, run_id: &str) {
        let removed = {
            let mut owners = lock(&self.run_owners);
            let removed = owners
                .get_mut(chat_id)
                .is_some_and(|owners| owners.remove(run_id));
            if owners.get(chat_id).is_some_and(HashSet::is_empty) {
                owners.remove(chat_id);
            }
            removed
        };
        if removed {
            let _ = self.run_owners_changed.send(());
        }
    }

    fn retire_run(&self, chat_id: &str, run_id: &str) {
        self.remove_run(chat_id, run_id);
        self.release_run_owner(chat_id, run_id);
    }

    #[cfg(test)]
    async fn pause_terminal_handoff_if_requested(
        &self,
        chat_id: &str,
    ) -> Option<oneshot::Sender<()>> {
        let pause = {
            let mut slot = lock(&self.terminal_handoff_pause);
            if slot.as_ref().is_some_and(|pause| pause.chat_id == chat_id) {
                slot.take()
            } else {
                None
            }
        };
        let pause = pause?;
        let _ = pause.reached.send(());
        let _ = pause.release.await;
        Some(pause.settled)
    }

    /// Keep this run's ownership pin through the terminal local/workspace
    /// status writes. A delete finalizer treats its absence as proof that this
    /// generation cannot write again, and may otherwise admit a same-id chat
    /// between removal and the terminal upsert.
    async fn finish_run(&self, chat_id: &str, run_id: &str, final_status: SessionStatus) {
        self.set_status(chat_id, final_status, false);
        self.retire_run(chat_id, run_id);
        #[cfg(test)]
        if let Some(settled) = self.pause_terminal_handoff_if_requested(chat_id).await {
            let _ = settled.send(());
        }
    }

    /// Record a context reading against the chat's session row.
    ///
    /// A window-less reading **clears** the row rather than leaving the last
    /// one standing. The stale figure is not merely old, it is wrong: the
    /// prompt has moved on, so the gauge would keep drawing the previous
    /// turn's occupancy against the previous turn's limit. This is reachable
    /// from Comet's own code, not just a provider quirk — a multi-model turn
    /// whose windows disagree declines the window on purpose
    /// (`claude::normalize::agreed_context_window`), and that decline arrives
    /// here as `None`.
    ///
    /// A prompt with no limit still cannot be drawn, so nothing replaces it:
    /// inventing a default limit would be a number the user acts on.
    ///
    /// The reading is deliberately NOT journaled. It is current occupancy, not
    /// history, and replaying a run would otherwise resurrect a figure that
    /// stopped being true the moment the next request went out.
    fn record_context(&self, chat_id: &str, event: &AgentEvent) {
        let AgentEvent::Usage {
            prompt_tokens,
            context_window,
            ..
        } = event
        else {
            return; // not a reading at all — a slash-command turn emits none
        };
        let reading = (*context_window).map(|context_window| comet_proto::ContextUsage {
            prompt_tokens: *prompt_tokens,
            context_window,
        });
        let session = {
            let mut statuses = lock(&self.statuses);
            let Some(entry) = statuses.get_mut(chat_id) else {
                return; // no session row yet: nothing to hang the reading on
            };
            if entry.context == reading {
                return; // nothing changed; skip the broadcast and the doc write
            }
            entry.context = reading;
            let session = entry.clone();
            let mut list: Vec<Session> = statuses.values().cloned().collect();
            list.sort_by(|a, b| a.chat_id.cmp(&b.chat_id));
            self.sessions_tx.send_replace(list);
            session
        };
        if let Some(ws) = self.workspace() {
            ws.record_session(&session);
        }
    }

    /// Journal + broadcast one event (the two unconditional legs of the pipeline).
    fn publish(&self, chat_id: &str, event: &AgentEvent) -> u64 {
        self.record_context(chat_id, event);
        let seq = match self.journal.append(chat_id, event) {
            Ok(seq) => seq,
            Err(err) => {
                tracing::error!(chat = %chat_id, error = %err, "journal append failed");
                0
            }
        };
        if let Some(hub) = lock(&self.hubs).get(chat_id) {
            let _ = hub.send(JournaledEvent {
                seq,
                event: event.clone(),
            });
        }
        seq
    }

    /// Enforce the engine's transcript privacy boundary before this event can
    /// be copied into the journal, broadcast stream, or document fold.
    fn prepare_event(&self, chat_id: &str, event: &mut AgentEvent) {
        let host = self
            .doc_host
            .get()
            .expect("runs require a wired document host before they can start");
        match prepare_event_with(event, |part_id, diff| {
            host.put_tool_diff(chat_id, part_id, diff)
        }) {
            None => {}
            Some(PrepareToolDiffFailure::Rejected(limit)) => {
                tracing::debug!(chat = %chat_id, ?limit, "tool diff sidecar rejected")
            }
            Some(PrepareToolDiffFailure::Store(err)) => {
                tracing::warn!(chat = %chat_id, error = %err, "tool diff sidecar write failed")
            }
        }
    }

    /// Bump the session's freshness on stream activity WITHOUT a status
    /// transition. Long silent-LOOKING stretches (thinking heartbeats, a big
    /// tool input being generated) still carry events — the UI's 45s
    /// staleness gate must not flip "Working" off mid-run. Throttled: a
    /// workspace-doc mirror per delta would be far too chatty.
    fn touch_session(&self, chat_id: &str) {
        const TOUCH_THROTTLE_MS: i64 = 10_000;
        let now = Utc::now();
        let session = {
            let mut statuses = lock(&self.statuses);
            let Some(entry) = statuses.get_mut(chat_id) else {
                return;
            };
            let age = now
                .signed_duration_since(entry.updated_at)
                .num_milliseconds();
            if age < TOUCH_THROTTLE_MS {
                return;
            }
            entry.updated_at = now;
            let session = entry.clone();
            let mut list: Vec<Session> = statuses.values().cloned().collect();
            list.sort_by(|a, b| a.chat_id.cmp(&b.chat_id));
            self.sessions_tx.send_replace(list);
            session
        };
        if let Some(ws) = self.workspace() {
            ws.record_session(&session);
        }
    }

    fn set_status(&self, chat_id: &str, status: SessionStatus, fresh_start: bool) {
        let now = Utc::now();
        let session = {
            let mut statuses = lock(&self.statuses);
            let entry = statuses
                .entry(chat_id.to_string())
                .or_insert_with(|| Session {
                    chat_id: chat_id.to_string(),
                    device_id: self.device_id.clone(),
                    status,
                    started_at: None,
                    updated_at: now,
                    // Unknown until the first model request answers: neither
                    // provider publishes a window before then.
                    context: None,
                });
            entry.status = status;
            entry.updated_at = now;
            if fresh_start {
                entry.started_at = Some(now);
            }
            let session = entry.clone();
            let mut list: Vec<Session> = statuses.values().cloned().collect();
            list.sort_by(|a, b| a.chat_id.cmp(&b.chat_id));
            // send_replace: keep the current value fresh even with no receivers,
            // so late WatchSessions subscribers see the last transition.
            self.sessions_tx.send_replace(list);
            session
        };
        // Mirror the transition into the workspace doc's session-status row so
        // remote devices' sidebars show this run (staleness-checked client-side).
        if let Some(ws) = self.workspace() {
            ws.record_session(&session);
        }
    }

    fn workspace(&self) -> Option<&crate::workspace_host::WorkspaceHost> {
        self.doc_host.get().and_then(|host| host.workspace())
    }

    /// Sidebar freshness: push a message-persist preview into the chat's workspace row.
    fn note_message(&self, chat_id: &str, text: &str) {
        if text.is_empty() {
            return;
        }
        if let Some(ws) = self.workspace() {
            ws.note_message(chat_id, text);
        }
    }

    /// Record the chat's harness-native session id (and its cwd): live-process
    /// cache plus the durable workspace chat row — the row is what survives an
    /// engine restart (comet sessions.ts:1039).
    fn remember_harness_session(&self, chat_id: &str, session_id: &str, cwd: &str) {
        if session_id.is_empty() {
            return;
        }
        lock(&self.harness_sessions).insert(
            chat_id.to_string(),
            HarnessSessionRef {
                session_id: session_id.to_string(),
                cwd: cwd.to_string(),
            },
        );
        if let Some(ws) = self.workspace() {
            ws.set_chat_harness_session(chat_id, session_id, cwd);
        }
    }

    /// A harness rejected the stored session id: tombstone it (empty string on
    /// the row, cleared cache) so no lookup source — including the journal,
    /// which still names the dead id — can re-inject it.
    fn forget_harness_session(&self, chat_id: &str) {
        lock(&self.harness_sessions).insert(
            chat_id.to_string(),
            HarnessSessionRef {
                session_id: String::new(),
                cwd: String::new(),
            },
        );
        if let Some(ws) = self.workspace() {
            ws.set_chat_harness_session(chat_id, "", "");
        }
    }

    /// The session id to resume for a run in `chat_id` launching from `cwd`
    /// (comet sessions.ts:736, looked up on every dispatch):
    /// live-process cache → workspace chat row → journal scan (the crash path
    /// where the debounced row write never landed — SessionStarted/Done events
    /// are journaled per event, flushed immediately). Cwd-gated throughout:
    /// harness session stores are keyed by cwd, so a session created elsewhere
    /// never rides `--resume`. An empty stored id is the explicit tombstone —
    /// no resume, no falling through to staler sources.
    fn resume_for(&self, chat_id: &str, cwd: &str) -> Option<String> {
        let cwd_ok = |session_cwd: &str| session_cwd.is_empty() || session_cwd == cwd;
        if let Some(known) = lock(&self.harness_sessions).get(chat_id).cloned() {
            return (!known.session_id.is_empty() && cwd_ok(&known.cwd))
                .then_some(known.session_id);
        }
        if let Some(ws) = self.workspace()
            && let Some((session_id, session_cwd)) = ws.chat_harness_session(chat_id)
        {
            return (!session_id.is_empty() && cwd_ok(session_cwd.as_deref().unwrap_or("")))
                .then_some(session_id);
        }
        let (session_id, session_cwd, _) = self.journal_harness_session(chat_id)?;
        // Cache the journal hit (memory + row) so later dispatches skip the scan.
        self.remember_harness_session(chat_id, &session_id, &session_cwd);
        cwd_ok(&session_cwd).then_some(session_id)
    }

    /// The last harness session id named anywhere in the chat's journal, with
    /// the cwd and runtime mode of the `SessionStarted` that governs it.
    /// `Done.session_id` inherits both from the most recent `SessionStarted`
    /// (same run).
    fn journal_harness_session(&self, chat_id: &str) -> Option<(String, String, RuntimeMode)> {
        let events = match self.journal.replay(chat_id, 0) {
            Ok(events) => events,
            Err(err) => {
                tracing::warn!(chat = %chat_id, error = %err, "journal scan for harness session failed");
                return None;
            }
        };
        let mut current_cwd = String::new();
        let mut current_mode = RuntimeMode::default();
        let mut found: Option<(String, String, RuntimeMode)> = None;
        for (_, event) in events {
            match event {
                AgentEvent::SessionStarted {
                    session_id,
                    cwd,
                    runtime_mode,
                    ..
                } => {
                    current_cwd = cwd;
                    current_mode = runtime_mode;
                    if !session_id.is_empty() {
                        found = Some((session_id, current_cwd.clone(), current_mode));
                    }
                }
                AgentEvent::Done {
                    session_id: Some(session_id),
                    ..
                } if !session_id.is_empty() => {
                    found = Some((session_id, current_cwd.clone(), current_mode));
                }
                _ => {}
            }
        }
        found
    }

    /// Retire `run_id`'s handle, if it is still the live one for this chat.
    ///
    /// The run-id guard stays: a handle under this chat id that belongs to a
    /// LATER run is not ours to retire, and draining it would kill a card the
    /// user is looking at. Releasing what the retired run parked is
    /// `RunHandle::drop`'s job, which is also what covers the case this guard
    /// declines — a replaced handle, drained by `dispatch_inner`'s insert.
    fn remove_run(&self, chat_id: &str, run_id: &str) {
        let mut runs = lock(&self.runs);
        if !runs.get(chat_id).is_some_and(|h| h.run_id == run_id) {
            return;
        }
        let retired = runs.remove(chat_id);
        drop(runs);
        drop(retired);
    }
}

// ── run task ────────────────────────────────────────────────────────────────

/// The durable sidecar was not available for one tool result. Exact sources
/// have already been removed from the event in every variant.
enum PrepareToolDiffFailure {
    Rejected(ToolDiffLimit),
    Store(comet_sync::StoreError),
}

/// Strip exact Write/Edit call inputs and move exact tool-result sources into
/// the sidecar before an event can reach a journal, subscriber, or document.
fn prepare_event_with<F>(event: &mut AgentEvent, persist: F) -> Option<PrepareToolDiffFailure>
where
    F: FnOnce(&str, &ToolDiff) -> Result<PutToolDiffOutcome, comet_sync::StoreError>,
{
    if let AgentEvent::ToolCall { call, .. } = event {
        match call {
            comet_proto::ToolCall::WriteFile { content, .. } => *content = None,
            comet_proto::ToolCall::EditFile {
                old_string,
                new_string,
                ..
            } => {
                *old_string = None;
                *new_string = None;
            }
            _ => {}
        }
        return None;
    }
    prepare_tool_result_with(event, persist)
}

/// Move exact tool-result sources into the sidecar once any source-bearing
/// ToolCall has already been stripped at the authoritative event boundary.
fn prepare_tool_result_with<F>(event: &mut AgentEvent, persist: F) -> Option<PrepareToolDiffFailure>
where
    F: FnOnce(&str, &ToolDiff) -> Result<PutToolDiffOutcome, comet_sync::StoreError>,
{
    let AgentEvent::ToolResult {
        id,
        diff,
        diff_ref,
        diff_stats,
        ..
    } = event
    else {
        return None;
    };
    let Some(diff) = diff.take() else {
        // Replay of a previously prepared event. Its metadata is already the
        // durable record, so never write it again or clear it as if it failed.
        return None;
    };

    *diff_ref = None;
    *diff_stats = Some(vec![diff.stat()]);
    match persist(id, &diff) {
        Ok(PutToolDiffOutcome::Stored {
            diff_ref: stored, ..
        }) => {
            *diff_ref = Some(stored);
            None
        }
        Ok(PutToolDiffOutcome::Rejected(limit)) => Some(PrepareToolDiffFailure::Rejected(limit)),
        Err(err) => Some(PrepareToolDiffFailure::Store(err)),
    }
}

/// Apply the render-parts privacy policy: strip heavy/sensitive tool inputs before doc
/// entry. Complete Write/Edit sources were already stripped at the engine boundary.
fn render_parts(parts: &[MessagePart]) -> Vec<MessagePart> {
    parts
        .iter()
        .map(|part| match part {
            MessagePart::Tool {
                id,
                call,
                is_error,
                resolved,
                diff_ref,
                diff_stats,
            } => MessagePart::Tool {
                id: id.clone(),
                call: sanitize_tool_call(call),
                is_error: *is_error,
                resolved: *resolved,
                diff_ref: diff_ref.clone(),
                diff_stats: diff_stats.clone(),
            },
            // Approvals carry no heavy field by construction: a file change is
            // path + operation + counts, never the patch. Passed through whole
            // — the catch-all below would do the same, but a kind that DOES
            // carry something heavy has to make that decision here rather than
            // inherit it silently.
            MessagePart::Approval { .. } => part.clone(),
            // `Subagent` DOES carry something heavy — `summary` is the
            // child's full answer — and the decision for it was made
            // (D56), not inherited: `summary` is the product's own
            // displayed content, the same kind as `TextDelta` and
            // `Done.result`, both uncapped end-to-end for the same reason.
            // Capping it here would truncate real content before any
            // surface decides how to present it, so it is passed through
            // whole on purpose. Do not add a cap here.
            //
            // `description` is not that kind of field — it is the Task
            // tool's own label, contracted with the model to be "a short
            // (3-5 word) description of the task" — so it is bounded
            // against a model that ignores that contract at the harness
            // boundary instead (`claude::normalize`'s
            // `SUBAGENT_DESCRIPTION_MAX`, the same place and reason
            // `prompt` is bounded), and reaches this fold already capped.
            MessagePart::Subagent { .. } => part.clone(),
            other => other.clone(),
        })
        .collect()
}

/// The persisted assistant text of a folded segment (workspace preview source).
fn folded_text(parts: &[MessagePart]) -> String {
    parts
        .iter()
        .filter_map(|part| match part {
            MessagePart::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn sync_segment<'a>(
    doc: &'a SessionDoc,
    writer: &mut Option<SegmentWriter<'a>>,
    entry_id: &str,
    device_id: &str,
    started_at: i64,
    folded: &[MessagePart],
) -> Result<(), DocError> {
    if folded.is_empty() {
        return Ok(());
    }
    let rendered = render_parts(folded);
    if writer.is_none() {
        *writer = Some(SegmentWriter::begin(doc, entry_id, device_id, started_at)?);
    }
    if let Some(w) = writer.as_mut() {
        w.sync(&rendered)?;
    }
    Ok(())
}

fn finish_segment<'a>(
    doc: &'a SessionDoc,
    writer: Option<SegmentWriter<'a>>,
    entry_id: &str,
    device_id: &str,
    started_at: i64,
    folded: &[MessagePart],
    status: MessageStatus,
) -> Result<(), DocError> {
    let rendered = render_parts(folded);
    match writer {
        Some(w) => w.finish(&rendered, status),
        None if !folded.is_empty() => {
            SegmentWriter::begin(doc, entry_id, device_id, started_at)?.finish(&rendered, status)
        }
        None => Ok(()),
    }
}

/// Terminally resolve every approval this run still has open, both halves:
/// drop the parked resolvers, and stamp the accumulated parts `Expired`.
///
/// Called wherever `folded` is about to be written into a FINISHED entry. Past
/// that point neither half can happen any more — `fold_event_into_parts`'s
/// `ApprovalResolved` arm and the run's own sweeps all walk the live
/// accumulator only, so a card that leaves it undecided reads "waiting" for
/// the life of the chat — while the resolver stays parked and the harness
/// stays blocked on a tool call nobody will answer.
///
/// Dropping the resolver, rather than sending a decision, is deliberate: every
/// consumer reads a dropped resolver as NOT approved, which is the same rule
/// `interrupt()` and `RunHandle::drop` use.
fn expire_open_approvals(inner: &Inner, chat_id: &str, run_id: &str, folded: &mut [MessagePart]) {
    let pending = lock(&inner.runs)
        .get(chat_id)
        .filter(|h| h.run_id == run_id)
        .map(|h| h.pending_approvals.clone());
    if let Some(pending) = pending {
        let parked: Vec<_> = lock(&pending).drain().map(|(_, p)| p.resolver).collect();
        drop(parked);
    }
    for part in folded.iter_mut() {
        if let MessagePart::Approval { decision, .. } = part
            && decision.is_none()
        {
            *decision = Some(ApprovalDecision::Expired);
        }
    }
}

/// Cancel every `Subagent` part still `Running` in `folded`. Called ONLY from
/// the `Done` arm below, and there only when the turn was **cut short**
/// (`Interrupted` or `Errored`) — never on a clean completion, and never from
/// the `Steered` boundary, unlike [`expire_open_approvals`].
///
/// **A cleanly completed turn can leave a live child.** Claude's `Agent` tool
/// is not synchronous with the parent's turn: a real 2.1.246 run ended
/// `Completed` with `result: "Agent is running. Waiting for completion
/// notification."` and the child reported `completed`, with its answer and
/// usage, four events later. Sweeping there manufactured `Cancelled` for an
/// agent that succeeded. The Done arm carries the full journal trace.
///
/// That split is not "two mechanisms for one job": it is one mechanism kept
/// out of a place it would lie. The approval sweep CAUSES the state it
/// records — it drops the parked resolver, and every consumer reads a
/// dropped resolver as NOT approved (`interrupt()`, `RunHandle::drop`), so
/// `Expired` becomes true by the sweep's own act, regardless of boundary.
/// This function performs no analogous act. Steering
/// (`SessionsEngine::steer`, `RunHandle::steer_tx`) only queues a line for
/// the CLI's stdin — the harness's steer arm in `comet_harness::claude`
/// writes it and emits `Steered`, nothing more; the mutually-exclusive abort
/// arm beside it is the one that signals interrupt and escalates
/// SIGTERM→SIGKILL. A subagent running when the user steers: Comet sends
/// nothing that would abort it, and what the CLI process does with it from
/// there is uncaptured — no capture pins the child's real fate, so it most
/// likely keeps running and completes, but that is an inference, not an
/// observation. Stamping it `Cancelled` at that boundary would assert an
/// outcome nobody observed. The pre-steer segment is orphaned either way —
/// `fold_event_into_parts` clears the accumulator on `Steered`, so a
/// `SubagentUpdated` for an unseen `task_id` in the new segment is dropped —
/// but a stale `Running` reading is honest about what is unknown, and a
/// manufactured `Cancelled` is not.
///
/// A `Subagent` part already `Completed`, `Failed` or `Cancelled` is left
/// alone: those are real outcomes, and overwriting one would erase it in
/// favor of a manufactured one.
fn cancel_running_subagents(folded: &mut [MessagePart]) {
    for part in folded.iter_mut() {
        if let MessagePart::Subagent { status, .. } = part
            && *status == SubagentStatus::Running
        {
            *status = SubagentStatus::Cancelled;
        }
    }
}

/// Resume bookkeeping for one run task: which user entry the run answers (so a
/// failed-resume retry re-dispatches idempotently against the same doc entry)
/// and whether `dispatch` injected the resume id itself (only engine-injected
/// resumes are retried fresh — a caller-specified resume fails loudly).
struct RunResumeState {
    user_message_id: String,
    resume_injected: bool,
}

/// Retires the exact lifecycle owner when a detached run task leaves for any
/// reason, including unwinding from a provider panic. `retire_run`'s run-id
/// check prevents an old task's drop from removing a replacement route.
struct RunOwnerRetirementGuard {
    inner: Arc<Inner>,
    chat_id: String,
    run_id: String,
}

impl Drop for RunOwnerRetirementGuard {
    fn drop(&mut self) {
        self.inner.retire_run(&self.chat_id, &self.run_id);
    }
}

#[allow(clippy::too_many_arguments)]
async fn drive_run(
    inner: Arc<Inner>,
    chat_id: String,
    run_id: String,
    harness: Arc<dyn Harness>,
    request: RunRequest,
    generation: Arc<ChatDocHandle>,
    doc: Arc<SessionDoc>,
    controls: RunControls,
    mut engine_rx: mpsc::UnboundedReceiver<AgentEvent>,
    mut cancel_rx: watch::Receiver<Option<DoneStatus>>,
    resume_state: RunResumeState,
) {
    // This must be the first task-owned local: every later operation includes
    // provider code and document/status writes that may unwind. Normal explicit
    // retirement remains idempotent; this guard closes every exceptional path.
    let _owner_retirement = RunOwnerRetirementGuard {
        inner: inner.clone(),
        chat_id: chat_id.clone(),
        run_id: run_id.clone(),
    };
    let device_id = inner.device_id.clone();
    // Captured for post-run auto-titling (the request moves into the harness).
    let harness_id = harness.id();
    let user_prompt = request.prompt.clone();
    let run_cwd = request.cwd.clone();
    // Kept whole for the failed-resume retry (fresh session, same user entry).
    // Option so the retry branch (inside the event loop) can take ownership.
    let mut retry_request = Some(RunRequest {
        resume: None,
        ..request.clone()
    });
    let mut stream = match harness.run(request, controls).await {
        Ok(stream) => stream,
        Err(err) => {
            let message = err.to_string();
            inner.publish(
                &chat_id,
                &AgentEvent::Error {
                    message: message.clone(),
                },
            );
            inner.publish(
                &chat_id,
                &AgentEvent::Done {
                    status: DoneStatus::Errored,
                    result: None,
                    error: Some(message),
                    session_id: None,
                },
            );
            inner
                .finish_run(&chat_id, &run_id, SessionStatus::Errored)
                .await;
            return;
        }
    };

    let doc_ref: &SessionDoc = &doc;
    let mut folded: Vec<MessagePart> = Vec::new();
    let mut entry_id = new_id();
    // `task_id` → the entry that currently holds its `Subagent` card. Lives
    // for exactly the RUN's lifetime (this local, dropped when `drive_run`
    // returns) — the run's process is the only thing that can still hear
    // from the child, so once it ends nothing more will ever arrive to
    // route. Updated every time `SubagentStarted` folds a card into the
    // CURRENT `entry_id`; read when a `SubagentUpdated` names a `task_id`
    // the live `folded` accumulator no longer holds — i.e. its segment
    // already finished and rotated to a new entry (D98: Claude's `Agent`
    // tool is not synchronous with the parent's turn, so a `Done` can finish
    // and park the segment before the child reports its real outcome).
    let mut subagent_locations: HashMap<String, String> = HashMap::new();
    let mut segment_started = now_ms();
    let mut writer: Option<SegmentWriter<'_>> = None;
    let mut dirty = false;
    let mut flush_at = tokio::time::Instant::now();
    // Set when the engine interrupts the run: the harness gets this long to end its own
    // stream (its token was cancelled); past it, a terminal Done is synthesized.
    let mut interrupt_deadline: Option<tokio::time::Instant> = None;
    let mut interrupted = false;
    let mut saw_session_started = false;
    // Liveness heartbeat: this loop RUNNING is proof the harness stream is
    // open, so freshness must not depend on events arriving. Silent stretches
    // are normal and UNBOUNDED — a long tool call, redacted thinking, an
    // agent waiting on an external process, a question parked for an hour —
    // and each starved the UI's 45s staleness gate in turn (working strip /
    // AwaitingInput dot vanishing mid-run, both user-reported). No stall
    // timeout here by design (a first port was rejected — agents may
    // legitimately be quiet for >10min): a live child means Working, dying
    // paths each carry their own error, and engine death stops these ticks
    // so the gate still catches real crashes. touch_session throttles at 10s.
    let mut live_heartbeat = tokio::time::interval(std::time::Duration::from_secs(15));
    live_heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // PERSISTENT SESSION (comet runsBySession): a completed turn on a
    // steerable harness parks here instead of ending the run — the child and
    // its steering mailbox stay warm, and the next user message (dispatch
    // routes into a live run) starts the next turn with zero respawn/resume
    // latency. `Some(when)` = idle since then; the 30-min reaper below ends
    // a session nobody comes back to (comet SESSION_IDLE_MS).
    const SESSION_IDLE: std::time::Duration = std::time::Duration::from_secs(30 * 60);
    let mut idle_since: Option<tokio::time::Instant> = None;
    let steerable = harness.capabilities().supports_steering;
    // TURN-QUIESCE WATCHDOG. A harness that loses a turn's Done — the adapter
    // never settles the prompt even though the agent finished — strands the
    // session Working forever: the heartbeat above keeps the row fresh by
    // design, and the idle reaper below only arms once `idle_since` is set,
    // which a missing Done never does.
    //
    // This is NOT the stall timeout the comment above rejects, and the
    // difference is the whole point. That one would END a run on silence,
    // which is wrong because agents are legitimately quiet for minutes. This
    // one never ends the run and never errors anything: it parks the turn
    // exactly as a Done would — segment finalized Complete, status Idle,
    // child and mailbox warm — and only when the fold proves nothing is in
    // flight. A false trip therefore costs a status dip, not content.
    //
    // `COMET_TURN_QUIESCE_MS` overrides the window; 0 disables it.
    let quiesce_after: Option<std::time::Duration> = match std::env::var("COMET_TURN_QUIESCE_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        Some(0) => None,
        Some(ms) => Some(std::time::Duration::from_millis(ms)),
        None => Some(std::time::Duration::from_secs(120)),
    };
    let mut last_stream_activity = tokio::time::Instant::now();

    let mut interrupt_status = None;
    let final_status = loop {
        let mut event: AgentEvent = tokio::select! {
            biased;
            changed = cancel_rx.changed(), if !interrupted => {
                let _ = changed;
                interrupted = true;
                interrupt_status = *cancel_rx.borrow_and_update();
                interrupt_deadline = Some(
                    tokio::time::Instant::now() + std::time::Duration::from_secs(3),
                );
                continue;
            }
            _ = tokio::time::sleep_until(
                interrupt_deadline.unwrap_or_else(tokio::time::Instant::now)
            ), if interrupt_deadline.is_some() => AgentEvent::Done {
                status: interrupt_status.unwrap_or(DoneStatus::Interrupted),
                result: None,
                error: None,
                session_id: None,
            },
            _ = live_heartbeat.tick() => {
                inner.touch_session(&chat_id);
                continue;
            }
            // Turn-quiesce watchdog (see the knob above). Armed only when the
            // fold proves nothing is in flight: an unresolved tool part is a
            // command still running — legitimately silent for minutes, the
            // case the rejected stall timeout got wrong — and an unresolved
            // input part is a question awaiting the user. An EMPTY fold still
            // arms (a boundary that no output ever follows is one of the wedge
            // shapes); it just parks without writing a segment, since an empty
            // finalize would leave a stub entry.
            //
            // Upstream exempts its live-plan chip here, a singleton that never
            // resolves. We have no equivalent: our plan surface is
            // `MessagePart::Checklist`, its own variant with a fixed id, so it
            // never matches the `Tool` arm and needs no exemption.
            _ = tokio::time::sleep_until(
                last_stream_activity + quiesce_after.unwrap_or_default()
            ), if quiesce_after.is_some()
                && idle_since.is_none()
                && !interrupted
                && steerable
                && !folded.iter().any(|p| matches!(
                    p,
                    MessagePart::Tool { resolved: false, .. }
                        | MessagePart::Input { resolved: false, .. }
                )) =>
            {
                tracing::warn!(
                    chat = %chat_id,
                    quiet_ms = quiesce_after.unwrap_or_default().as_millis() as u64,
                    "turn quiesced: stream silent after completed output with no                      turn-end; parking (suspected missing harness Done)"
                );
                if !folded.is_empty() || writer.is_some() {
                    if let Err(err) = finish_segment(
                        doc_ref,
                        writer.take(),
                        &entry_id,
                        &device_id,
                        segment_started,
                        &folded,
                        MessageStatus::Complete,
                    ) {
                        tracing::warn!(
                            chat = %chat_id,
                            error = %err,
                            "quiesce segment finish failed"
                        );
                    }
                    inner.note_message(&chat_id, &folded_text(&folded));
                }
                folded.clear();
                dirty = false;
                entry_id = new_id();
                segment_started = now_ms();
                idle_since = Some(tokio::time::Instant::now());
                inner.set_status(&chat_id, SessionStatus::Idle, false);
                continue;
            }
            // Idle reaper (comet SESSION_IDLE_MS): a parked persistent session
            // nobody returned to in 30 minutes releases its child. The turn
            // was finalized at Done, so this end is clean — no aborted stamp.
            _ = tokio::time::sleep_until(
                idle_since.map(|at| at + SESSION_IDLE).unwrap_or_else(tokio::time::Instant::now)
            ), if idle_since.is_some() => {
                tracing::info!(chat = %chat_id, "reaping idle persistent session");
                if let Some(token) = lock(&inner.runs)
                    .get(&chat_id)
                    .filter(|h| h.run_id == run_id)
                    .map(|h| h.interrupt_token.clone())
                {
                    token.cancel();
                }
                break SessionStatus::Idle;
            }
            Some(event) = engine_rx.recv() => event,
            next = stream.next() => match next {
                Some(Ok(event)) => event,
                Some(Err(err)) => AgentEvent::Done {
                    status: DoneStatus::Errored,
                    result: None,
                    error: Some(err.to_string()),
                    session_id: None,
                },
                None if interrupted => AgentEvent::Done {
                    status: interrupt_status.unwrap_or(DoneStatus::Interrupted),
                    result: None,
                    error: None,
                    session_id: None,
                },
                // Stream end while PARKED idle: a per-turn adapter closing
                // after its final Done — a clean end, not a crash (the turn
                // was already finalized). Persistent adapters keep the
                // stream open and never hit this.
                None if idle_since.is_some() => break SessionStatus::Idle,
                None => AgentEvent::Done {
                    status: DoneStatus::Errored,
                    result: None,
                    error: Some("harness stream ended without Done".into()),
                    session_id: None,
                },
            },
            _ = tokio::time::sleep_until(flush_at), if dirty => {
                // Coalesced STREAM_COMMIT_MS tick: one doc commit per window.
                if let Err(err) = sync_segment(
                    doc_ref, &mut writer, &entry_id, &device_id, segment_started, &folded,
                ) {
                    tracing::warn!(chat = %chat_id, error = %err, "segment sync failed");
                }
                dirty = false;
                continue;
            }
        };

        if let AgentEvent::Done { status, .. } = &mut event
            && let Some(interrupt_status) = interrupt_status
        {
            *status = interrupt_status;
        }

        // Any stream activity proves the run is alive — keep the session's
        // freshness inside the UI's 45s staleness window (throttled), and push
        // the quiesce watchdog's window out.
        inner.touch_session(&chat_id);
        last_stream_activity = tokio::time::Instant::now();
        // Empty reasoning deltas are PURE heartbeats: redacted thinking and
        // tool-input-generation windows stream them with no text, and a
        // persistent session emits them between turns too. They fold to
        // nothing, so journaling/publishing them is only noise (hundreds per
        // long turn observed) — the touch above already did their job.
        //
        // Dropped HERE, before any state transition, and not below the
        // turn-start block: a heartbeat is not turn-start either. Counting one
        // as a turn wedged a parked session the same way a notice used to (see
        // the note below) — with the extra sting that `parked_notice` reads
        // `idle_since`, so a heartbeat clearing it silently disarmed that fix.
        if matches!(&event, AgentEvent::ReasoningDelta { text } if text.is_empty()) {
            continue;
        }
        // This is the authoritative privacy boundary. Every later path either
        // publishes the event, folds it, or both, so no clone can retain exact
        // sources beyond this point.
        inner.prepare_event(&chat_id, &mut event);
        // A Diagnostic is bookkeeping about the protocol, not turn content.
        // It can arrive OUTSIDE a turn (a persistent session's unknown
        // notification while parked) and must not be mistaken for turn-start
        // — the same wedge the between-turns notice and the empty heartbeat
        // each hit. It folds to no part, so its whole life is: count into
        // the per-boot registry, journal, move on.
        if let AgentEvent::Diagnostic {
            discriminator,
            severity,
            ..
        } = &event
        {
            inner
                .registry
                .record_diagnostic(harness_id, discriminator, *severity);
            inner.publish(&chat_id, &event);
            continue;
        }
        // Notice is not the only event class that can arrive OUTSIDE a turn —
        // the empty-heartbeat ReasoningDelta and the Diagnostic handled just
        // above both can too, each disposed of before reaching here for
        // exactly this reason. A Notice (an MCP server dropping, a
        // rate-limit warning, an environment disconnect while the session
        // sits parked) is NOT turn-start either. Counting it as one wedged
        // the session: `idle_since` cleared → the reaper's select arm (gated
        // on `idle_since.is_some()`) is disabled and the child is never
        // released, the status flips to Working, and the chip opens a
        // streaming entry that no `Done` is ever coming to finish. This
        // comment is scar tissue from two prior regressions in this exact
        // loop — a between-turns notice, and (later) an empty reasoning
        // heartbeat, each of which flipped a parked session to Working
        // forever and disarmed the idle reaper. Treat "can arrive outside a
        // turn" as the default question for any new event class, not the
        // exception. Handled below by writing it as its own finished entry,
        // still parked.
        let parked_notice = idle_since.is_some() && matches!(&event, AgentEvent::Notice { .. });
        // `SessionTitled` is the same "can arrive outside a turn" class as
        // `Notice`: a steerable session (Grok supports steering) PARKS
        // between turns, and the agent may re-push `session_info_update`
        // with a revised title while parked, same as it does mid-turn. It
        // does NOT get `Notice`'s "write it as its own finished entry"
        // treatment below — it folds to no transcript part at all
        // (`doc::parts::fold_event_into_parts`'s no-op arm), so that would
        // only write an empty segment. It just must not count as turn-start.
        let parked_title =
            idle_since.is_some() && matches!(&event, AgentEvent::SessionTitled { .. });
        // D98's own case, and the SAME "can arrive outside a turn" class as
        // `Notice`/`SessionTitled`: the whole reason a late `SubagentUpdated`
        // needs routing back into a finished entry is that it arrives AFTER
        // the turn that started it already parked. Counting it as turn-start
        // would retrigger exactly the wedge the comment above describes —
        // `idle_since` cleared, the idle reaper disarmed, status flipped to
        // Working for a background child's own report, nothing left to ever
        // park it again. `SubagentStarted` never legitimately opens a fresh
        // turn either, so it gets the same exemption for symmetry, though no
        // capture has shown it arriving while parked. Neither variant folds
        // to a visible part while `folded` is empty (the just-parked state),
        // so — like `parked_title`, unlike `parked_notice` — this needs no
        // "write it as its own finished entry" handling below.
        let parked_subagent = idle_since.is_some()
            && matches!(
                &event,
                AgentEvent::SubagentStarted { .. } | AgentEvent::SubagentUpdated { .. }
            );
        // First event after parking idle = the next turn beginning (a routed
        // dispatch steered in): the session is Working again.
        if !parked_notice && !parked_title && !parked_subagent && idle_since.take().is_some() {
            inner.set_status(&chat_id, SessionStatus::Working, true);
        }

        // Failed-resume fallback: an engine-injected `--resume` naming a session
        // the harness no longer knows dies before ever starting (claude exits
        // without an init frame; codex falls back internally via thread/start).
        // Signature: errored Done, no SessionStarted, nothing streamed. Retry
        // ONCE as a fresh session against the same user entry — tombstone the
        // dead id first so no lookup source (journal included) re-injects it.
        if resume_state.resume_injected
            && !saw_session_started
            && folded.is_empty()
            && !interrupted
            && matches!(
                &event,
                AgentEvent::Done {
                    status: DoneStatus::Errored,
                    ..
                }
            )
            && let Some(retry) = retry_request.take()
        {
            tracing::warn!(
                chat = %chat_id,
                "harness rejected injected resume id; retrying as a fresh session"
            );
            inner.forget_harness_session(&chat_id);
            inner.retire_run(&chat_id, &run_id);
            let engine = SessionsEngine {
                inner: inner.clone(),
            };
            let chat = chat_id.clone();
            let message_id = resume_state.user_message_id.clone();
            let retry_doc = doc.clone();
            #[cfg(test)]
            engine.pause_resume_retry_if_requested(&chat).await;
            tokio::spawn(async move {
                // The retry starts fresh and must still bind to the document
                // generation that owned the rejected resume.
                if let Err(err) = engine
                    .dispatch_retry_for_generation(&chat, harness_id, retry, message_id, retry_doc)
                    .await
                {
                    tracing::debug!(chat = %chat, error = %err, "fresh-session retry dispatch refused");
                }
            });
            return;
        }

        // A steer boundary splits the assistant entry exactly where the fold resets.
        if let AgentEvent::Steered {
            next_assistant_message_id,
            ..
        } = &event
        {
            inner.publish(&chat_id, &event);
            // A steer abandons whatever the previous turn was waiting on, and
            // this is the last moment either half of that can be recorded: the
            // segment below is FINISHED, so a card left open in it can never be
            // stamped by a later decision, by the Done-time sweep, or offered
            // by the decision row again. Saying the tool call was dropped is
            // the honest answer, and it releases the harness.
            //
            // Deliberately approvals-only: a subagent still `Running` here is
            // NOT stamped `Cancelled` — see `cancel_running_subagents`'s doc
            // for why a steer must not claim to know a child's fate it never
            // observed.
            expire_open_approvals(&inner, &chat_id, &run_id, &mut folded);
            if let Err(err) = finish_segment(
                doc_ref,
                writer.take(),
                &entry_id,
                &device_id,
                segment_started,
                &folded,
                MessageStatus::Complete,
            ) {
                tracing::warn!(chat = %chat_id, error = %err, "segment finish failed");
            }
            inner.note_message(&chat_id, &folded_text(&folded));
            folded.clear();
            dirty = false;
            entry_id = next_assistant_message_id.clone().unwrap_or_else(new_id);
            segment_started = now_ms();
            continue;
        }

        match &event {
            AgentEvent::SessionStarted {
                session_id, cwd, ..
            } => {
                saw_session_started = true;
                // The event's own cwd (where the harness actually created the
                // session) scopes the stored id, not the request's.
                inner.remember_harness_session(&chat_id, session_id, cwd);
            }
            AgentEvent::Done {
                session_id: Some(session_id),
                ..
            } => {
                inner.remember_harness_session(&chat_id, session_id, &run_cwd);
            }
            AgentEvent::InputRequested { request_id, .. } => {
                // The engine's input bridge is the sole authority on input
                // requests: it mints the id and parks the resolver BEFORE
                // emitting the event, so a legitimate id is always pending
                // here. A harness emitting its own copy (a different id no
                // resolver knows) would fold an unanswerable twin chip into
                // the doc — and answering the twin would never resume the
                // run. Drop such events.
                let pending = lock(&inner.runs)
                    .get(&chat_id)
                    .map(|h| h.pending_inputs.clone());
                let known = pending.is_some_and(|p| lock(&p).contains_key(request_id));
                if !known {
                    tracing::warn!(
                        chat = %chat_id,
                        request = %request_id,
                        "dropping harness-emitted InputRequested (unknown id; \
                         the engine input bridge owns this lifecycle)"
                    );
                    continue;
                }
                inner.set_status(&chat_id, SessionStatus::AwaitingInput, false);
            }
            AgentEvent::InputResolved { .. } => {
                inner.set_status(&chat_id, SessionStatus::Working, false);
            }
            AgentEvent::ApprovalRequested { request_id, .. } => {
                // Same authority rule as input requests: the host mints the id
                // before emitting, so a legitimate id is always recorded here.
                // An adapter emitting its own copy would fold an unanswerable
                // card into the doc. Checked against `minted_approvals`, not
                // `pending_approvals`: a session-auto-allowed request is
                // already resolved (and removed from `pending`) by the time
                // this guard runs, so `pending` would wrongly reject it.
                let minted = lock(&inner.runs)
                    .get(&chat_id)
                    .map(|h| h.minted_approvals.clone());
                let known = minted.is_some_and(|m| lock(&m).contains(request_id));
                if !known {
                    tracing::warn!(
                        chat = %chat_id,
                        request = %request_id,
                        "dropping harness-emitted ApprovalRequested (unknown id; \
                         the engine approval bridge owns this lifecycle)"
                    );
                    continue;
                }
                // Reusing AwaitingInput rather than adding a variant: it
                // crosses RPC inside Session, and every consumer encodes
                // "blocked on the user", which is true of both.
                inner.set_status(&chat_id, SessionStatus::AwaitingInput, false);
            }
            AgentEvent::ApprovalResolved { .. } => {
                // Only once NOTHING is still waiting on the user. Approvals
                // arrive in batches (claude asks for parallel tool calls at
                // once, and a session-auto-allowed one resolves the instant
                // it is minted), so an unconditional step back to Working
                // would report a run as running while a card is still open.
                let pending = lock(&inner.runs)
                    .get(&chat_id)
                    .filter(|h| h.run_id == run_id)
                    .map(|h| h.pending_approvals.clone());
                let still_waiting = pending.is_some_and(|p| !lock(&p).is_empty());
                if !still_waiting {
                    inner.set_status(&chat_id, SessionStatus::Working, false);
                }
            }
            AgentEvent::SessionTitled { title } => {
                // Fire-and-forget; every guard (manual-rename lock,
                // first-writer-wins) lives in `WorkspaceHost::rename_chat_auto`,
                // which `TitleGenerator::apply_agent_title` writes through —
                // see that method's own doc.
                if let Some(titles) = inner.titles.get() {
                    titles.apply_agent_title(generation.clone(), title);
                }
            }
            _ => {}
        }

        inner.publish(&chat_id, &event);

        // Defensive rule from comet: a mid-run SessionStarted re-emission (Claude SDK
        // background re-invocations) must not wipe the segment being written.
        let skip_fold = matches!(&event, AgentEvent::SessionStarted { .. }) && !folded.is_empty();
        if !skip_fold {
            fold_event_into_parts(&mut folded, &event);
        }

        // D98: keep `subagent_locations` current, and route a `SubagentUpdated`
        // the live fold above just dropped back to the entry that actually
        // holds its card. `SubagentStarted` always records/refreshes the
        // CURRENT `entry_id` — matching exactly where the fold just placed
        // (or refreshed) the part, including a `SendMessage` resume that
        // starts a fresh card under a new `entry_id`. A `SubagentUpdated` is
        // only a routing candidate when `folded` — the live accumulator this
        // segment hasn't flushed and cleared yet — no longer has a matching
        // part: while the segment is still open, the ordinary fold above
        // already applied it in place, and it will reach the doc through the
        // normal commit/finish path like everything else in `folded`.
        match &event {
            AgentEvent::SubagentStarted { task_id, .. } => {
                subagent_locations.insert(task_id.clone(), entry_id.clone());
            }
            AgentEvent::SubagentUpdated {
                task_id,
                status,
                activity,
                summary,
                total_tokens,
                duration_ms,
                tool_uses,
            } => {
                let live = folded.iter().any(
                    |p| matches!(p, MessagePart::Subagent { task_id: tid, .. } if tid == task_id),
                );
                if !live && let Some(target_entry) = subagent_locations.get(task_id) {
                    match doc_ref.apply_late_subagent_update(
                        target_entry,
                        task_id,
                        *status,
                        activity.as_deref(),
                        summary.as_deref(),
                        *total_tokens,
                        *duration_ms,
                        *tool_uses,
                    ) {
                        Ok(true) => tracing::debug!(
                            chat = %chat_id,
                            task_id = %task_id,
                            entry = %target_entry,
                            "routed a late SubagentUpdated to its already-finished entry"
                        ),
                        Ok(false) => tracing::warn!(
                            chat = %chat_id,
                            task_id = %task_id,
                            entry = %target_entry,
                            "late SubagentUpdated named an entry with no matching subagent part"
                        ),
                        Err(err) => tracing::warn!(
                            chat = %chat_id,
                            task_id = %task_id,
                            error = %err,
                            "late SubagentUpdated write failed"
                        ),
                    }
                }
            }
            _ => {}
        }

        // Between-turns notice: write it NOW as a complete entry and stay
        // parked. The spec's decision that such a notice is written stands
        // (holding it until the next turn would lose exactly the MCP-failure
        // case this exists for), but nothing else will finalize it — no `Done`
        // is coming while parked, and the normal flush tick would leave a
        // `streaming` entry spinning in the UI forever. One notice per entry is
        // the cost: parking already cleared the accumulator, so a repeat can't
        // collapse into a trailing part across the gap.
        if parked_notice {
            if let Err(err) = finish_segment(
                doc_ref,
                writer.take(),
                &entry_id,
                &device_id,
                segment_started,
                &folded,
                MessageStatus::Complete,
            ) {
                tracing::warn!(chat = %chat_id, error = %err, "parked notice segment finish failed");
            }
            folded.clear();
            dirty = false;
            entry_id = new_id();
            segment_started = now_ms();
            continue;
        }

        if let AgentEvent::Done { status, .. } = &event {
            let message_status = match status {
                DoneStatus::Interrupted | DoneStatus::Expired => MessageStatus::Aborted,
                DoneStatus::Completed | DoneStatus::Errored => MessageStatus::Complete,
            };
            // No dangling chips: a run that ends for ANY reason (completed,
            // errored, interrupted) terminally resolves its input parts — an
            // unresolved question must not outlive the run that asked it
            // (its resolver died with the run; an answer could never land).
            for part in folded.iter_mut() {
                if let MessagePart::Input { resolved, .. } = part {
                    *resolved = true;
                }
            }
            // Same rule for approvals, and the same reason — plus the drain,
            // which the stamp alone left undone: a steerable harness PARKS
            // after a completed turn (below), so its resolvers would outlive
            // the transcript's "Expired" by up to the 30-minute idle reap,
            // and `respond_approval` would keep answering `true` for one of
            // them the whole time.
            expire_open_approvals(&inner, &chat_id, &run_id, &mut folded);
            // A subagent still `Running` at `Done` is stamped `Cancelled`
            // ONLY when the turn was cut short. A cleanly completed turn must
            // not claim to know a background child's fate.
            //
            // This arm used to sweep on every `Done`, on the premise —
            // written here — that "Claude's `Task` tool call is synchronous
            // from the parent's own turn". **That is false, and a real run
            // disproves it.** Captured 2026-08-26 against Claude Code
            // 2.1.246, one `Agent` delegation, journal seq in parentheses:
            //
            //   subagentStarted (46) → running (48) → **done, COMPLETED (57)**
            //   with `result: "Agent is running. Waiting for completion
            //   notification."` → running + usage (58) → completed (61) →
            //   completed with the answer and full usage (62)
            //
            // The turn ends cleanly with the child still live, and the child
            // reports its real outcome four events later. Sweeping at (57)
            // manufactured `Cancelled` for an agent that completed — the
            // exact act this function's own doc says must never happen,
            // performed at the one boundary that was believed safe.
            //
            // `Interrupted` and `Errored` keep the sweep: there the turn
            // genuinely was cut short, and the sweep is what stops a card
            // spinning forever.
            //
            // The rest of the old comment still holds and is why this is only
            // half a fix. This segment (`folded`/`entry_id`) never gets
            // another fold: a steerable harness that PARKS keeps the same
            // `run_id`, but its next turn folds into a NEW, cleared
            // accumulator, so (61) and (62) above are dropped as updates for
            // a `task_id` this accumulator never saw. The card is left
            // honestly `Running` rather than falsely `Cancelled`, but the
            // summary and usage it earned are still lost. Routing a late
            // reading back to a finished segment is its own change — see
            // `docs/debt/README.md`.
            if *status != DoneStatus::Completed {
                cancel_running_subagents(&mut folded);
            }
            // A Done landing on a PARKED session with nothing streamed (the
            // idle reaper's or an interrupt's own teardown) has no entry to
            // finalize — writing one would leave an empty aborted stub.
            let nothing_streamed = writer.is_none() && folded.is_empty();
            if !nothing_streamed {
                if let Err(err) = finish_segment(
                    doc_ref,
                    writer.take(),
                    &entry_id,
                    &device_id,
                    segment_started,
                    &folded,
                    message_status,
                ) {
                    tracing::warn!(chat = %chat_id, error = %err, "final segment finish failed");
                }
                inner.note_message(&chat_id, &folded_text(&folded));
            }
            if *status == DoneStatus::Completed {
                // A cleanly completed turn resets the auto-resume revival
                // budget: only consecutive crash-revive-crash cycles spend it.
                inner.journal.clear_resume_attempts(&chat_id);
            }
            // Exchange completed on an untitled chat → name it (fire-and-forget;
            // interrupted/errored turns never trigger naming).
            if *status == DoneStatus::Completed
                && let Some(titles) = inner.titles.get()
            {
                titles.maybe_generate(generation.clone(), harness_id, &user_prompt, &run_cwd);
            }
            // PERSISTENT SESSION: a cleanly completed turn on a steerable
            // harness PARKS instead of ending — child + mailbox stay warm for
            // the next routed dispatch; per-turn state resets for it.
            if *status == DoneStatus::Completed && steerable && !interrupted {
                folded.clear();
                dirty = false;
                entry_id = new_id();
                segment_started = now_ms();
                // Resume-retry is strictly a first-turn concern.
                saw_session_started = true;
                idle_since = Some(tokio::time::Instant::now());
                inner.set_status(&chat_id, SessionStatus::Idle, false);
                continue;
            }
            break match status {
                DoneStatus::Errored => SessionStatus::Errored,
                _ => SessionStatus::Idle,
            };
        }

        if !folded.is_empty() && !dirty {
            dirty = true;
            flush_at =
                tokio::time::Instant::now() + std::time::Duration::from_millis(STREAM_COMMIT_MS);
        }
    };

    // Closed BEFORE the run is announced finished: the approval bridge fails
    // closed on a send error, so an approval minted in the moment between the
    // loop ending and this task's frame being dropped must find the channel
    // already shut rather than park into a receiver nobody polls.
    drop(engine_rx);
    inner.finish_run(&chat_id, &run_id, final_status).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DocHostConfig;
    use comet_sync::DocsStore;

    fn engine(dir: &std::path::Path) -> SessionsEngine {
        let journal = Arc::new(RunJournal::open(dir).expect("journal opens"));
        SessionsEngine::new("dev-a".into(), journal, Arc::new(HarnessRegistry::new()))
    }

    #[test]
    fn deleted_chat_cleanup_waits_for_a_live_run_then_retires_every_session_remnant() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = engine(dir.path());
        let mut watch = sessions.watch_sessions();
        sessions
            .inner
            .journal
            .append("chat-1", &AgentEvent::TextDelta { text: "old".into() })
            .unwrap();
        sessions.inner.journal.note_resume_attempt("chat-1");
        let (_replay, _live) = sessions.subscribe("chat-1", 0).unwrap();
        lock(&sessions.inner.harness_sessions).insert(
            "chat-1".into(),
            HarnessSessionRef {
                session_id: "old-session".into(),
                cwd: "/tmp".into(),
            },
        );
        lock(&sessions.inner.last_requests).insert(
            "chat-1".into(),
            RunRequest {
                cwd: "/tmp".into(),
                ..RunRequest::for_session(RuntimeMode::default())
            },
        );
        sessions.set_status("chat-1", SessionStatus::Working, true);
        sessions.inner.register_run(
            "chat-1",
            bare_handle(
                "run-1",
                Arc::new(Mutex::new(HashMap::new())),
                Arc::new(Mutex::new(HashMap::new())),
            ),
        );

        assert!(matches!(
            sessions.cleanup_deleted_chat("chat-1"),
            Err(SessionCleanupError::LiveRun)
        ));
        assert!(sessions.has_live_run("chat-1"));
        assert!(
            !sessions
                .inner
                .journal
                .replay("chat-1", 0)
                .unwrap()
                .is_empty()
        );
        assert_eq!(sessions.inner.journal.resume_attempts("chat-1"), 1);
        assert!(lock(&sessions.inner.harness_sessions).contains_key("chat-1"));
        assert!(lock(&sessions.inner.last_requests).contains_key("chat-1"));
        assert!(lock(&sessions.inner.statuses).contains_key("chat-1"));
        assert!(lock(&sessions.inner.hubs).contains_key("chat-1"));

        sessions.inner.retire_run("chat-1", "run-1");
        sessions.cleanup_deleted_chat("chat-1").unwrap();

        assert!(
            sessions
                .inner
                .journal
                .replay("chat-1", 0)
                .unwrap()
                .is_empty()
        );
        assert_eq!(sessions.inner.journal.resume_attempts("chat-1"), 0);
        assert!(!lock(&sessions.inner.harness_sessions).contains_key("chat-1"));
        assert!(!lock(&sessions.inner.last_requests).contains_key("chat-1"));
        assert!(!lock(&sessions.inner.statuses).contains_key("chat-1"));
        assert!(!lock(&sessions.inner.hubs).contains_key("chat-1"));
        assert!(watch.borrow_and_update().is_empty());
        sessions.cleanup_deleted_chat("chat-1").unwrap();
    }

    /// A finite harness script lets the privacy test exercise the actual run
    /// pipeline, including publish, journal, fold, and document rendering.
    struct ToolDiffHarness {
        script: Vec<AgentEvent>,
    }

    #[async_trait::async_trait]
    impl Harness for ToolDiffHarness {
        fn id(&self) -> HarnessId {
            HarnessId::Mock
        }

        fn display_name(&self) -> &str {
            "Tool diff script"
        }

        fn capabilities(&self) -> comet_proto::HarnessCapabilities {
            comet_proto::HarnessCapabilities::default()
        }

        async fn models(&self) -> Result<comet_proto::ModelCatalog, comet_harness::HarnessError> {
            Ok(comet_proto::ModelCatalog::built_in(Vec::new()))
        }

        async fn run(
            &self,
            _request: RunRequest,
            _controls: RunControls,
        ) -> Result<
            futures::stream::BoxStream<'static, Result<AgentEvent, comet_harness::HarnessError>>,
            comet_harness::HarnessError,
        > {
            Ok(Box::pin(futures::stream::iter(
                self.script
                    .clone()
                    .into_iter()
                    .map(Ok::<_, comet_harness::HarnessError>),
            )))
        }
    }

    #[test]
    fn render_parts_passes_an_approval_through_unchanged() {
        // The privacy policy strips heavy tool inputs; an approval has none to
        // strip, and this pins that rather than leaving it to a catch-all.
        let part = MessagePart::Approval {
            id: "ap-r1".into(),
            request_id: "r1".into(),
            approval: comet_proto::ApprovalRequest::FileChange {
                path: "src/main.rs".into(),
                operation: comet_proto::FileOperation::Modify,
                added_lines: 12,
                removed_lines: 3,
            },
            decision: None,
        };
        assert_eq!(render_parts(std::slice::from_ref(&part)), vec![part]);
    }

    /// Removing preparation or moving it after publish leaks the source pair
    /// into the live event and run journal. The sidecar is the sole allowed
    /// durable owner of the exact source pair.
    #[tokio::test]
    async fn tool_result_is_stripped_before_journal_and_fold() {
        const WRITE_POISON_NEW: &str = "TASK9_WRITE_POISON_NEW_SOURCE";
        const EDIT_POISON_OLD: &str = "TASK9_EDIT_POISON_OLD_SOURCE";
        const EDIT_POISON_NEW: &str = "TASK9_EDIT_POISON_NEW_SOURCE";

        let write_diff = comet_proto::ToolDiff {
            path: "src/lib.rs".into(),
            old_text: None,
            new_text: WRITE_POISON_NEW.into(),
        };
        let edit_diff = comet_proto::ToolDiff {
            path: "src/edit.rs".into(),
            old_text: Some(EDIT_POISON_OLD.into()),
            new_text: EDIT_POISON_NEW.into(),
        };
        let write_ref = write_diff.diff_ref().unwrap();
        let write_stats = vec![write_diff.stat()];
        let edit_ref = edit_diff.diff_ref().unwrap();
        let edit_stats = vec![edit_diff.stat()];
        let poisons = [WRITE_POISON_NEW, EDIT_POISON_OLD, EDIT_POISON_NEW];
        let script = vec![
            AgentEvent::SessionStarted {
                harness: HarnessId::Mock,
                model: "test".into(),
                tools: Vec::new(),
                cwd: "/tmp".into(),
                session_id: "session-1".into(),
                assistant_message_id: "assistant-1".into(),
                runtime_mode: comet_proto::RuntimeMode::default(),
            },
            AgentEvent::ToolCall {
                id: "write-tool".into(),
                call: comet_proto::ToolCall::WriteFile {
                    path: "src/lib.rs".into(),
                    content: Some(WRITE_POISON_NEW.into()),
                },
            },
            AgentEvent::ToolResult {
                id: "write-tool".into(),
                is_error: false,
                diff: Some(write_diff.clone()),
                // A provider/replay must never make a stale ref survive a
                // new source pair; Stored replaces it with the sidecar ref.
                diff_ref: Some("v1:stale".into()),
                diff_stats: None,
            },
            AgentEvent::ToolCall {
                id: "edit-tool".into(),
                call: comet_proto::ToolCall::EditFile {
                    path: "src/edit.rs".into(),
                    old_string: Some(EDIT_POISON_OLD.into()),
                    new_string: Some(EDIT_POISON_NEW.into()),
                },
            },
            AgentEvent::ToolResult {
                id: "edit-tool".into(),
                is_error: false,
                diff: Some(edit_diff.clone()),
                diff_ref: Some("v1:stale".into()),
                diff_stats: None,
            },
            AgentEvent::Done {
                status: DoneStatus::Completed,
                result: None,
                error: None,
                session_id: Some("session-1".into()),
            },
        ];
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(DocsStore::open(dir.path().join("store")).unwrap());
        let host = DocHost::new(
            store.clone(),
            DocHostConfig {
                device_id: "dev-a".into(),
                default_harness: HarnessId::Mock,
            },
        );
        let registry = Arc::new(HarnessRegistry::new());
        registry.register(Arc::new(ToolDiffHarness { script }));
        let journal = Arc::new(RunJournal::open(dir.path().join("journals")).unwrap());
        let sessions = SessionsEngine::new("dev-a".into(), journal.clone(), registry);
        sessions.set_doc_host(host.clone());
        host.set_sessions(sessions.clone());

        let (replay_before_run, mut live) = sessions.subscribe("chat-1", 0).unwrap();
        assert!(replay_before_run.is_empty());
        sessions
            .dispatch(
                "chat-1",
                HarnessId::Mock,
                RunRequest {
                    prompt: "change it".into(),
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

        let mut published = Vec::new();
        loop {
            let event = tokio::time::timeout(std::time::Duration::from_secs(1), live.recv())
                .await
                .expect("run publishes every scripted event")
                .expect("run broadcast remains open")
                .event;
            let is_done = matches!(event, AgentEvent::Done { .. });
            published.push(event);
            if is_done {
                break;
            }
        }
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while sessions
                .session_status("chat-1")
                .is_some_and(|session| session.status != SessionStatus::Idle)
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("run finishes its document fold");

        for event in &published {
            let published_json = serde_json::to_string(event).unwrap();
            for poison in poisons {
                assert!(
                    !published_json.contains(poison),
                    "published event leaked exact source {poison}: {published_json}"
                );
            }
        }

        let published_result = published
            .iter()
            .find(|event| matches!(event, AgentEvent::ToolResult { id, .. } if id == "write-tool"))
            .expect("write result published");
        let AgentEvent::ToolResult {
            diff,
            diff_ref,
            diff_stats,
            ..
        } = published_result
        else {
            unreachable!();
        };
        assert!(
            diff.is_none(),
            "published event must not retain exact sources"
        );
        assert_eq!(diff_ref.as_deref(), Some(write_ref.as_str()));
        assert_eq!(diff_stats.as_deref(), Some(write_stats.as_slice()));

        let AgentEvent::ToolResult {
            diff,
            diff_ref,
            diff_stats,
            ..
        } = published
            .iter()
            .find(|event| matches!(event, AgentEvent::ToolResult { id, .. } if id == "edit-tool"))
            .expect("whole-file Edit result published")
        else {
            unreachable!();
        };
        assert!(
            diff.is_none(),
            "published Edit must not retain exact sources"
        );
        assert_eq!(diff_ref.as_deref(), Some(edit_ref.as_str()));
        assert_eq!(diff_stats.as_deref(), Some(edit_stats.as_slice()));

        let replay = journal.replay("chat-1", 0).unwrap();
        let replayed_result = replay
            .iter()
            .map(|(_, event)| event)
            .find(|event| matches!(event, AgentEvent::ToolResult { id, .. } if id == "write-tool"))
            .expect("write result journaled");
        let AgentEvent::ToolResult {
            diff,
            diff_ref,
            diff_stats,
            ..
        } = replayed_result
        else {
            unreachable!();
        };
        assert!(
            diff.is_none(),
            "journal replay must not retain exact sources"
        );
        assert_eq!(diff_ref.as_deref(), Some(write_ref.as_str()));
        assert_eq!(diff_stats.as_deref(), Some(write_stats.as_slice()));
        let replayed_json = serde_json::to_string(replayed_result).unwrap();
        for poison in poisons {
            assert!(
                !replayed_json.contains(poison),
                "journal replay leaked exact source {poison}: {replayed_json}"
            );
        }
        let journal_text = std::fs::read_to_string(dir.path().join("journals/chat-1.jsonl"))
            .expect("journal file exists");
        for poison in poisons {
            assert!(
                !journal_text.contains(poison),
                "journal leaked exact source {poison}: {journal_text}"
            );
        }

        assert_eq!(
            store
                .read_tool_diff("chat-1", "write-tool", &write_ref)
                .unwrap(),
            Some(write_diff.clone()),
            "the sidecar retains the exact source pair"
        );
        assert_eq!(
            store
                .read_tool_diff("chat-1", "edit-tool", &edit_ref)
                .unwrap(),
            Some(edit_diff.clone()),
            "the sidecar retains the exact whole-file Edit pair"
        );

        let handle = host.open("chat-1").unwrap();
        let entries = handle.doc().read_entries().unwrap();
        let folded_tool = entries
            .iter()
            .flat_map(|entry| &entry.parts)
            .find(|part| matches!(part, MessagePart::Tool { id, .. } if id == "write-tool"))
            .expect("tool result folds into the document");
        let MessagePart::Tool {
            diff_ref,
            diff_stats,
            ..
        } = folded_tool
        else {
            unreachable!();
        };
        assert_eq!(diff_ref.as_deref(), Some(write_ref.as_str()));
        assert_eq!(diff_stats.as_deref(), Some(write_stats.as_slice()));
        let folded_json = serde_json::to_string(&entries).unwrap();
        for poison in poisons {
            assert!(
                !folded_json.contains(poison),
                "folded document leaked exact source {poison}: {folded_json}"
            );
        }

        let snapshot = handle.doc().export_snapshot().unwrap();
        let snapshot_text = String::from_utf8_lossy(&snapshot);
        for poison in poisons {
            assert!(
                !snapshot_text.contains(poison),
                "document snapshot leaked exact source {poison}: {snapshot_text}"
            );
        }
        let restored_raw = loro::LoroDoc::new();
        restored_raw.import(&snapshot).unwrap();
        let restored = SessionDoc::from_doc(restored_raw);
        let restored_tool = restored
            .read_entries()
            .unwrap()
            .into_iter()
            .flat_map(|entry| entry.parts)
            .find(|part| matches!(part, MessagePart::Tool { id, .. } if id == "write-tool"))
            .expect("tool part survives snapshot read");
        let MessagePart::Tool {
            diff_ref,
            diff_stats,
            ..
        } = restored_tool
        else {
            unreachable!();
        };
        assert_eq!(diff_ref.as_deref(), Some(write_ref.as_str()));
        assert_eq!(diff_stats.as_deref(), Some(write_stats.as_slice()));
    }

    /// A failed or rejected sidecar write has useful compact statistics but
    /// must never publish a stale or unavailable exact-source reference.
    #[test]
    fn sidecar_store_failure_keeps_stats_without_a_reference() {
        let source = comet_proto::ToolDiff {
            path: "src/lib.rs".into(),
            old_text: Some("before\n".into()),
            new_text: "after\n".into(),
        };
        let expected_stats = vec![source.stat()];
        let event_with_source = || AgentEvent::ToolResult {
            id: "tool-1".into(),
            is_error: false,
            diff: Some(source.clone()),
            diff_ref: Some("v1:stale".into()),
            diff_stats: None,
        };
        let assert_stripped = |event: &AgentEvent| {
            let AgentEvent::ToolResult {
                diff,
                diff_ref,
                diff_stats,
                ..
            } = event
            else {
                unreachable!();
            };
            assert!(
                diff.is_none(),
                "exact source must be cleared before persistence"
            );
            assert!(
                diff_ref.is_none(),
                "a failed write must clear a stale reference"
            );
            assert_eq!(diff_stats.as_deref(), Some(expected_stats.as_slice()));
        };

        let mut store_failure = event_with_source();
        let failure = prepare_tool_result_with(&mut store_failure, |_part_id, _diff| {
            Err(comet_sync::StoreError::Sqlite(
                rusqlite::Error::InvalidQuery,
            ))
        });
        assert!(matches!(
            failure,
            Some(PrepareToolDiffFailure::Store(
                comet_sync::StoreError::Sqlite(_)
            ))
        ));
        assert_stripped(&store_failure);

        let mut quota_failure = event_with_source();
        let failure = prepare_tool_result_with(&mut quota_failure, |_part_id, _diff| {
            Err(comet_sync::StoreError::ToolDiffQuota)
        });
        assert!(matches!(
            failure,
            Some(PrepareToolDiffFailure::Store(
                comet_sync::StoreError::ToolDiffQuota
            ))
        ));
        assert_stripped(&quota_failure);

        let mut rejected = event_with_source();
        let failure = prepare_tool_result_with(&mut rejected, |_part_id, _diff| {
            Ok(comet_sync::PutToolDiffOutcome::Rejected(
                comet_sync::ToolDiffLimit::Path,
            ))
        });
        assert!(matches!(
            failure,
            Some(PrepareToolDiffFailure::Rejected(
                comet_sync::ToolDiffLimit::Path
            ))
        ));
        assert_stripped(&rejected);

        let mut replay = AgentEvent::ToolResult {
            id: "tool-1".into(),
            is_error: false,
            diff: None,
            diff_ref: Some("v1:already-stored".into()),
            diff_stats: Some(expected_stats.clone()),
        };
        let result = prepare_tool_result_with(&mut replay, |_part_id, _diff| {
            panic!("an already-stripped replay must not be persisted again")
        });
        assert!(result.is_none());
        assert!(matches!(
            replay,
            AgentEvent::ToolResult {
                diff: None,
                diff_ref: Some(ref diff_ref),
                diff_stats: Some(ref diff_stats),
                ..
            } if diff_ref == "v1:already-stored" && diff_stats == &expected_stats
        ));
    }

    /// Break caught (D15): the drain took the resolver and left the id minted,
    /// so an `ApprovalRequested` still in flight when a run ends passed
    /// `drive_run`'s authority guard — which reads `minted_approvals`, not
    /// `pending_approvals` — and folded a card whose buttons could only ever
    /// return `Ok(false)`.
    ///
    /// Asserts the guard's own input rather than the card, because the guard is
    /// the thing that changed: after the drain the id must no longer be known.
    #[test]
    fn a_drained_approval_is_forgotten_so_a_late_request_cannot_fold_a_card() {
        let parked = parked_run_handle("run-1");
        let minted = parked.minted.clone();
        assert!(
            lock(&minted).contains("run-1-approval"),
            "the fixture has to start from a minted id, or this proves nothing"
        );

        let ParkedRun {
            handle,
            approval,
            question,
            ..
        } = parked;
        drop(handle);

        assert!(
            !lock(&minted).contains("run-1-approval"),
            "a drained approval must stop being an id the authority guard accepts"
        );
        // And the drain still does what it did before: the resolver is gone
        // (every consumer reads that as not approved) and the question is
        // answered with no answers.
        drop(approval);
        drop(question);
    }

    /// The other direction, and the reason `forget_minted` is not just
    /// `minted.clear()`: a session grant sweeps parked approvals and ANSWERS
    /// them, so their ids must stay minted — their own `ApprovalRequested` may
    /// still be in flight and has every right to fold a card, which the grant
    /// then resolves.
    #[test]
    fn forgetting_one_drained_id_leaves_every_other_minted_id_alone() {
        let minted: MintedApprovals = Arc::new(Mutex::new(HashSet::new()));
        {
            let mut set = lock(&minted);
            set.insert("drained".to_string());
            set.insert("answered".to_string());
        }

        forget_minted(&minted, [&"drained".to_string()].into_iter());

        let set = lock(&minted);
        assert!(!set.contains("drained"));
        assert!(
            set.contains("answered"),
            "an id answered by a grant sweep must stay minted"
        );
    }

    /// A run with one question and one approval parked.
    struct ParkedRun {
        handle: RunHandle,
        approval: oneshot::Receiver<ApprovalDecision>,
        question: oneshot::Receiver<Vec<UserInputAnswer>>,
        /// The same set the handle holds, kept so a test can read it after the
        /// handle is dropped — D15 is about what the drain leaves behind.
        minted: MintedApprovals,
        /// The `Arc`s the bridge closures hold — the reason a parked resolver
        /// outlives its handle at all. Without them here the maps would die
        /// with the handle and every assertion below would pass against a
        /// `Drop` that did nothing.
        _bridges: (PendingInputs, PendingApprovals),
    }

    fn parked_run_handle(run_id: &str) -> ParkedRun {
        let pending_approvals: PendingApprovals = Arc::new(Mutex::new(HashMap::new()));
        let pending_inputs: PendingInputs = Arc::new(Mutex::new(HashMap::new()));
        let (resolver, approval_rx) = oneshot::channel();
        let (answers, input_rx) = oneshot::channel();
        lock(&pending_approvals).insert(
            format!("{run_id}-approval"),
            PendingApproval {
                signature: None,
                resolver,
                parked_at: Utc::now(),
            },
        );
        lock(&pending_inputs).insert(
            format!("{run_id}-question"),
            PendingInput {
                resolver: answers,
                parked_at: Utc::now(),
            },
        );
        let minted: MintedApprovals = Arc::new(Mutex::new(HashSet::new()));
        lock(&minted).insert(format!("{run_id}-approval"));
        let (steer_tx, _steer_rx) = mpsc::channel(1);
        let (cancel, _cancel_rx) = watch::channel(None);
        let (engine_tx, _engine_rx) = mpsc::unbounded_channel();
        ParkedRun {
            handle: RunHandle {
                run_id: run_id.to_string(),
                steerable: false,
                supports_approval_interrupt: false,
                steer_tx,
                interrupt_token: CancellationToken::new(),
                cancel,
                engine_tx,
                pending_inputs: pending_inputs.clone(),
                pending_approvals: pending_approvals.clone(),
                minted_approvals: minted.clone(),
                session_allowed: Arc::new(Mutex::new(HashSet::new())),
            },
            approval: approval_rx,
            question: input_rx,
            minted,
            _bridges: (pending_inputs, pending_approvals),
        }
    }

    #[test]
    fn a_grant_that_lands_mid_mint_is_read_rather_than_missed() {
        // The window this lock order closes. The bridge used to read
        // `session_allowed`, release it, and only then take `pending`; a grant
        // slipping into that gap wrote its signature and swept a `pending` the
        // new request had not reached yet, so the request stayed open and the
        // user was asked for a click a moment after saying "don't ask again".
        //
        // Deterministic because this thread stands in for `respond_approval`'s
        // critical section: it holds `pending` — the lock `respond_approval`
        // now holds across its grant — and writes the signature while the
        // minting thread is running. With the fix the minting thread is still
        // waiting on `pending` and reads the signature after it; without it,
        // the minting thread has already read `session_allowed` and parks
        // regardless.
        //
        // The sleep is a head start for the WRONG behavior, never the right
        // one: a machine too loaded to get through those few instructions in
        // 50ms loses this coverage rather than gaining a flake.
        let pending: PendingApprovals = Arc::new(Mutex::new(HashMap::new()));
        let session_allowed: SessionAllowlist = Arc::new(Mutex::new(HashSet::new()));
        let signature = "sig-1".to_string();
        let (resolver, _rx) = oneshot::channel::<ApprovalDecision>();

        let granting = lock(&pending);
        let started = Arc::new(std::sync::Barrier::new(2));
        let minting = std::thread::spawn({
            let pending = pending.clone();
            let session_allowed = session_allowed.clone();
            let signature = signature.clone();
            let started = started.clone();
            move || {
                started.wait();
                park_unless_session_allows(
                    &pending,
                    &session_allowed,
                    "req-1",
                    Some(signature),
                    resolver,
                )
                .is_some()
            }
        });
        started.wait();
        std::thread::sleep(std::time::Duration::from_millis(50));
        lock(&session_allowed).insert(signature);
        drop(granting);

        assert!(
            minting.join().unwrap(),
            "a request minted around a session grant must read the grant, not park behind it"
        );
        assert!(
            lock(&pending).is_empty(),
            "and must leave nothing parked for the user to click"
        );
    }

    #[test]
    fn a_replaced_run_releases_what_nobody_can_answer_any_more() {
        // `dispatch_inner` waits 5s for an interrupt to settle and then inserts
        // regardless, so a live run's handle can be replaced. From that moment
        // `respond_approval` and `respond_input` find the successor, and the
        // old run's `remove_run` declines the handle it no longer owns — its
        // parked resolvers used to be leaked, leaving the harness (for claude,
        // a CLI blocked on a tool call) awaiting a reply forever.
        let dir = tempfile::tempdir().unwrap();
        let sessions = engine(dir.path());
        let mut old = parked_run_handle("run-1");
        let mut new = parked_run_handle("run-2");

        sessions.inner.register_run("chat-1", old.handle);
        let displaced = sessions.inner.register_run("chat-1", new.handle);
        drop(displaced);

        assert!(
            matches!(
                old.approval.try_recv(),
                Err(oneshot::error::TryRecvError::Closed)
            ),
            "a dropped resolver is how every consumer reads NOT approved"
        );
        assert_eq!(
            old.question.try_recv(),
            Ok(Vec::new()),
            "a parked question resolves to no answers, as interrupt() sends"
        );

        // The successor's open approval is NOT collateral: `remove_run` for the
        // old run id must leave the handle it declines to retire untouched.
        sessions.inner.retire_run("chat-1", "run-1");
        assert!(matches!(
            new.approval.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert!(lock(&sessions.inner.runs).contains_key("chat-1"));

        sessions.inner.retire_run("chat-1", "run-2");
        assert!(matches!(
            new.approval.try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        ));
        assert!(!sessions.has_live_run("chat-1"));
    }

    #[test]
    fn a_settled_replacement_does_not_hide_its_displaced_predecessor_from_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = engine(dir.path());
        let old = bare_handle(
            "run-1",
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(HashMap::new())),
        );
        let new = bare_handle(
            "run-2",
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(HashMap::new())),
        );

        sessions.inner.register_run("chat-1", old);
        drop(sessions.inner.register_run("chat-1", new));
        sessions.inner.retire_run("chat-1", "run-2");

        assert!(
            !lock(&sessions.inner.runs).contains_key("chat-1"),
            "the settled successor no longer owns the routing slot"
        );
        assert!(
            sessions.has_live_run("chat-1"),
            "the displaced predecessor remains in the ownership census"
        );
        assert!(matches!(
            sessions.cleanup_deleted_chat("chat-1"),
            Err(SessionCleanupError::LiveRun)
        ));

        sessions.inner.retire_run("chat-1", "run-1");
        assert!(!sessions.has_live_run("chat-1"));
    }

    #[tokio::test]
    async fn final_owner_release_wakes_a_waiter_registered_before_the_check() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = engine(dir.path());
        sessions.inner.register_run(
            "chat-1",
            bare_handle(
                "run-1",
                Arc::new(Mutex::new(HashMap::new())),
                Arc::new(Mutex::new(HashMap::new())),
            ),
        );
        let waiter = tokio::spawn({
            let sessions = sessions.clone();
            async move { sessions.wait_for_no_live_runs("chat-1").await }
        });
        tokio::task::yield_now().await;

        sessions.inner.retire_run("chat-1", "run-1");

        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
                .await
                .expect("final owner release wakes the waiter")
                .expect("waiter task joins")
        );
    }

    #[tokio::test]
    async fn engine_shutdown_cancels_an_owner_wait_without_certifying_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = engine(dir.path());
        sessions.inner.register_run(
            "chat-1",
            bare_handle(
                "run-1",
                Arc::new(Mutex::new(HashMap::new())),
                Arc::new(Mutex::new(HashMap::new())),
            ),
        );
        sessions.inner.remove_run("chat-1", "run-1");
        let waiter = tokio::spawn({
            let sessions = sessions.clone();
            async move { sessions.wait_for_no_live_runs("chat-1").await }
        });
        tokio::task::yield_now().await;

        sessions.shutdown().await;

        assert!(
            !tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
                .await
                .expect("shutdown wakes the owner waiter")
                .expect("waiter task joins"),
            "shutdown cancellation is not evidence that cleanup is safe"
        );
        assert!(sessions.has_live_run("chat-1"));
        sessions.inner.retire_run("chat-1", "run-1");
    }

    /// A reading is only worth keeping when the provider named the window it
    /// is measured against, and it must not resurrect after the row is gone.
    #[test]
    fn context_is_recorded_only_when_the_window_is_known() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = engine(dir.path());
        let context_of = |chat: &str| {
            lock(&sessions.inner.statuses)
                .get(chat)
                .and_then(|s| s.context)
        };

        // No session row yet: nothing to hang a reading on, and inventing one
        // would put a gauge on a chat that has never run.
        sessions.inner.record_context(
            "chat-1",
            &AgentEvent::Usage {
                prompt_tokens: 10,
                output_tokens: 1,
                context_window: Some(200_000),
            },
        );
        assert_eq!(context_of("chat-1"), None);

        sessions
            .inner
            .set_status("chat-1", SessionStatus::Working, true);

        // A window-less reading is undrawable, so it is dropped rather than
        // stored against an invented limit.
        sessions.inner.record_context(
            "chat-1",
            &AgentEvent::Usage {
                prompt_tokens: 17_268,
                output_tokens: 48,
                context_window: None,
            },
        );
        assert_eq!(context_of("chat-1"), None);

        sessions.inner.record_context(
            "chat-1",
            &AgentEvent::Usage {
                prompt_tokens: 35_017,
                output_tokens: 26,
                context_window: Some(200_000),
            },
        );
        assert_eq!(
            context_of("chat-1"),
            Some(comet_proto::ContextUsage {
                prompt_tokens: 35_017,
                context_window: 200_000,
            })
        );

        // A later status transition must not wipe it: occupancy outlives the
        // turn that measured it, and the next turn is what replaces it.
        sessions
            .inner
            .set_status("chat-1", SessionStatus::Idle, false);
        assert!(context_of("chat-1").is_some());

        // A window-less reading AFTER a good one clears it. Leaving the old
        // pair standing would draw the previous turn's occupancy against the
        // previous turn's limit while the prompt has already moved on — and
        // this is reachable from Comet's own decline path, not just a
        // provider quirk (PR #52 review, Macroscope).
        sessions.inner.record_context(
            "chat-1",
            &AgentEvent::Usage {
                prompt_tokens: 120_000,
                output_tokens: 12,
                context_window: None,
            },
        );
        assert_eq!(context_of("chat-1"), None);
    }

    fn session_started(cwd: &str, runtime_mode: RuntimeMode) -> AgentEvent {
        AgentEvent::SessionStarted {
            harness: HarnessId::Mock,
            model: "m".into(),
            tools: Vec::new(),
            cwd: cwd.into(),
            session_id: "harness-session-1".into(),
            assistant_message_id: "a1".into(),
            runtime_mode,
        }
    }

    #[test]
    fn journal_recovery_carries_the_mode_the_run_was_launched_under() {
        // The journal is the only durable record of a run whose chat row never
        // landed, and such a chat takes its mode from the composer draft — so
        // resuming under the default would drop a mode the user chose.
        let dir = tempfile::tempdir().unwrap();
        let sessions = engine(dir.path());
        sessions
            .inner
            .journal
            .append(
                "chat-1",
                &session_started("/tmp/repo", RuntimeMode::ApprovalRequired),
            )
            .expect("append");

        let (session_id, cwd, mode) = sessions
            .inner
            .journal_harness_session("chat-1")
            .expect("journal names a session");
        assert_eq!(session_id, "harness-session-1");
        assert_eq!(cwd, "/tmp/repo");
        assert_eq!(mode, RuntimeMode::ApprovalRequired);
    }

    #[test]
    fn a_journal_written_before_the_mode_existed_recovers_the_default() {
        // Absent on the wire is not "unknown": those runs ran under the default.
        let dir = tempfile::tempdir().unwrap();
        let sessions = engine(dir.path());
        let old = serde_json::json!({
            "type": "sessionStarted",
            "harness": "mock",
            "model": "m",
            "cwd": "/tmp/repo",
            "sessionId": "harness-session-1",
            "assistantMessageId": "a1"
        });
        let event: AgentEvent = serde_json::from_value(old).expect("old wire decodes");
        sessions
            .inner
            .journal
            .append("chat-1", &event)
            .expect("append");

        let (_, _, mode) = sessions
            .inner
            .journal_harness_session("chat-1")
            .expect("journal names a session");
        assert_eq!(mode, RuntimeMode::AutoAcceptEdits);
    }

    /// A bare `RunHandle` with nothing wired to a run task — `blocked_since`
    /// only ever reads the two pending maps, so this is cheaper than driving a
    /// harness through an approval or a question just to inspect timestamps.
    fn bare_handle(
        run_id: &str,
        pending_approvals: PendingApprovals,
        pending_inputs: PendingInputs,
    ) -> RunHandle {
        let (steer_tx, _steer_rx) = mpsc::channel(1);
        let (cancel, _cancel_rx) = watch::channel(None);
        let (engine_tx, _engine_rx) = mpsc::unbounded_channel();
        RunHandle {
            run_id: run_id.into(),
            steerable: false,
            supports_approval_interrupt: false,
            steer_tx,
            interrupt_token: CancellationToken::new(),
            cancel,
            engine_tx,
            pending_inputs,
            pending_approvals,
            minted_approvals: Arc::new(Mutex::new(HashSet::new())),
            session_allowed: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// A forged LAN command must not make an unsupported provider receive a
    /// semantic promise it cannot keep. Returning false leaves the resolver in
    /// the map, so the composer's existing safety net restores the decision
    /// row instead of retiring it forever.
    #[test]
    fn unsupported_deny_and_interrupt_leaves_the_approval_pending() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = engine(dir.path());
        let pending_approvals: PendingApprovals = Arc::new(Mutex::new(HashMap::new()));
        let (resolver, mut approval_rx) = oneshot::channel();
        lock(&pending_approvals).insert(
            "approval-1".into(),
            PendingApproval {
                signature: None,
                resolver,
                parked_at: Utc::now(),
            },
        );
        let pending_inputs: PendingInputs = Arc::new(Mutex::new(HashMap::new()));
        let (steer_tx, _steer_rx) = mpsc::channel(1);
        let (cancel, _cancel_rx) = watch::channel(None);
        let (engine_tx, mut engine_rx) = mpsc::unbounded_channel();
        lock(&sessions.inner.runs).insert(
            "chat-1".into(),
            RunHandle {
                run_id: "run-1".into(),
                steerable: false,
                supports_approval_interrupt: false,
                steer_tx,
                interrupt_token: CancellationToken::new(),
                cancel,
                engine_tx,
                pending_inputs,
                pending_approvals: pending_approvals.clone(),
                minted_approvals: Arc::new(Mutex::new(HashSet::new())),
                session_allowed: Arc::new(Mutex::new(HashSet::new())),
            },
        );

        assert!(
            !sessions
                .respond_approval(
                    "chat-1",
                    "approval-1",
                    ApprovalDecision::DenyAndInterrupt {
                        message: "stop this turn".into(),
                    },
                )
                .unwrap()
        );
        assert!(lock(&pending_approvals).contains_key("approval-1"));
        assert!(matches!(
            approval_rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        assert!(matches!(
            engine_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
    }

    /// `blocked_since` untested by any e2e test: the `None` case (nothing
    /// parked), which side wins when the two waits are NOT tied (the earlier
    /// one, regardless of kind), and the tie itself (approvals win). An
    /// integration test can't force an exact tie — two `Utc::now()` calls
    /// never land on the same instant — so this builds the maps directly with
    /// fixed timestamps instead.
    #[test]
    fn blocked_since_reports_the_earliest_wait_and_approvals_win_ties() {
        fn t(secs: i64) -> DateTime<Utc> {
            DateTime::from_timestamp(1_800_000_000 + secs, 0).unwrap()
        }

        let pending_approvals: PendingApprovals = Arc::new(Mutex::new(HashMap::new()));
        let pending_inputs: PendingInputs = Arc::new(Mutex::new(HashMap::new()));
        let handle = bare_handle("r1", pending_approvals.clone(), pending_inputs.clone());

        assert_eq!(handle.blocked_since(), None, "nothing parked, not blocked");

        let (answers, _rx) = oneshot::channel();
        lock(&pending_inputs).insert(
            "q1".into(),
            PendingInput {
                resolver: answers,
                parked_at: t(10),
            },
        );
        assert_eq!(
            handle.blocked_since(),
            Some((t(10), WaitKind::Answer)),
            "a lone question is the wait"
        );

        // An approval parked at the exact same instant: approvals win the tie.
        let (resolver, _rx) = oneshot::channel();
        lock(&pending_approvals).insert(
            "a1".into(),
            PendingApproval {
                signature: None,
                resolver,
                parked_at: t(10),
            },
        );
        assert_eq!(
            handle.blocked_since(),
            Some((t(10), WaitKind::Approval)),
            "tied instants: the approval wins"
        );

        // A LATER question must not override the earlier approval.
        let (later_answers, _rx) = oneshot::channel();
        lock(&pending_inputs).insert(
            "q2".into(),
            PendingInput {
                resolver: later_answers,
                parked_at: t(20),
            },
        );
        assert_eq!(
            handle.blocked_since(),
            Some((t(10), WaitKind::Approval)),
            "the earliest wait wins, not the newest approval"
        );

        // An EARLIER question must win over a later approval — this is not a
        // tie, so kind never overrides recency.
        lock(&pending_approvals).clear();
        lock(&pending_inputs).clear();
        let (resolver2, _rx) = oneshot::channel();
        lock(&pending_approvals).insert(
            "a2".into(),
            PendingApproval {
                signature: None,
                resolver: resolver2,
                parked_at: t(30),
            },
        );
        let (earlier_answers, _rx) = oneshot::channel();
        lock(&pending_inputs).insert(
            "q3".into(),
            PendingInput {
                resolver: earlier_answers,
                parked_at: t(5),
            },
        );
        assert_eq!(
            handle.blocked_since(),
            Some((t(5), WaitKind::Answer)),
            "the earlier question beats the later approval outside a tie"
        );
    }

    // -----------------------------------------------------------------------
    // `cancel_running_subagents` — a part this run's own event stream can
    // never touch again reads "running" for the life of the chat unless a
    // run-end sweep stamps it. And `expire_open_approvals` vs
    // `cancel_running_subagents` at the two boundaries that finish a segment:
    // a steer expires approvals (the sweep causes that state, by dropping the
    // resolver) but must NOT cancel a subagent (the sweep would only be
    // guessing at a state it did not cause — see `cancel_running_subagents`'s
    // own doc comment).
    // -----------------------------------------------------------------------

    fn subagent_part(id: &str, task_id: &str, status: SubagentStatus) -> MessagePart {
        MessagePart::Subagent {
            id: id.into(),
            task_id: task_id.into(),
            agent_type: "general-purpose".into(),
            description: "Read README and report first heading".into(),
            status,
            activity: None,
            summary: None,
            total_tokens: None,
            duration_ms: None,
            tool_uses: None,
        }
    }

    fn open_approval(id: &str, request_id: &str) -> MessagePart {
        MessagePart::Approval {
            id: id.into(),
            request_id: request_id.into(),
            approval: comet_proto::ApprovalRequest::FileRead {
                path: "a.rs".into(),
            },
            decision: None,
        }
    }

    /// A run that truly ends (`Done`) while a subagent is still `Running`
    /// must not leave that part `Running` — nothing will ever fold another
    /// `SubagentUpdated` into this segment.
    #[test]
    fn a_run_that_ends_mid_subagent_leaves_no_running_part() {
        let mut folded = vec![subagent_part("sub-1", "t1", SubagentStatus::Running)];

        cancel_running_subagents(&mut folded);

        match &folded[0] {
            MessagePart::Subagent { status, .. } => {
                assert_eq!(*status, SubagentStatus::Cancelled);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    /// A run that ends after the subagent already reached a terminal status
    /// must not stamp over it — `Completed` is a real outcome, not an open
    /// wait, and the sweep must not manufacture a different one.
    #[test]
    fn a_run_that_ends_after_the_subagent_completed_does_not_overwrite_it() {
        let mut folded = vec![subagent_part("sub-1", "t1", SubagentStatus::Completed)];

        cancel_running_subagents(&mut folded);

        match &folded[0] {
            MessagePart::Subagent { status, .. } => {
                assert_eq!(*status, SubagentStatus::Completed);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    /// The `Done` boundary runs both sweeps: an open approval and a running
    /// subagent in the SAME segment must both reach a terminal status.
    #[test]
    fn a_run_that_ends_resolves_both_an_open_approval_and_a_running_subagent() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = engine(dir.path());
        let mut folded = vec![
            open_approval("ap-r1", "r1"),
            subagent_part("sub-1", "t1", SubagentStatus::Running),
        ];

        // Mirrors exactly what the `Done` arm in `drive_run` calls, in order.
        expire_open_approvals(&sessions.inner, "chat-1", "run-1", &mut folded);
        cancel_running_subagents(&mut folded);

        assert!(matches!(
            &folded[0],
            MessagePart::Approval {
                decision: Some(ApprovalDecision::Expired),
                ..
            }
        ));
        assert!(matches!(
            &folded[1],
            MessagePart::Subagent {
                status: SubagentStatus::Cancelled,
                ..
            }
        ));
    }

    /// The `Steered` boundary must expire an open approval (the sweep causes
    /// that state) but must NOT cancel a running subagent (the sweep would
    /// only be labeling a state nobody observed) — this is the regression
    /// case for treating the two as one mechanism. This test calls
    /// `expire_open_approvals` DIRECTLY on a hand-built `folded`; it never
    /// drives the `Steered` arm's own dispatch path, so it would stay green
    /// even if that arm gained a `cancel_running_subagents` call alongside
    /// `expire_open_approvals`. Only
    /// `crates/engine/tests/e2e.rs:2625-2686`'s
    /// `a_steer_over_a_running_subagent_does_not_stamp_it_cancelled` actually
    /// pins the arm calling `expire_open_approvals` alone — see D57.
    #[test]
    fn a_steer_boundary_expires_approvals_but_leaves_a_running_subagent_alone() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = engine(dir.path());
        let mut folded = vec![
            open_approval("ap-r1", "r1"),
            subagent_part("sub-1", "t1", SubagentStatus::Running),
        ];

        expire_open_approvals(&sessions.inner, "chat-1", "run-1", &mut folded);

        assert!(matches!(
            &folded[0],
            MessagePart::Approval {
                decision: Some(ApprovalDecision::Expired),
                ..
            }
        ));
        assert!(
            matches!(
                &folded[1],
                MessagePart::Subagent {
                    status: SubagentStatus::Running,
                    ..
                }
            ),
            "a steer must not claim to know a still-running child's fate"
        );
    }

    // -----------------------------------------------------------------------
    // The unattended sweep's fail-closed re-check. `expire_unattended` cannot
    // be paused between its collect pass and an individual interrupt without a
    // test-only hook, so the per-run decision is a pure function and these
    // drive it directly with the facts each interleaving would produce.
    // -----------------------------------------------------------------------

    mod still_expirable {
        use super::super::still_expirable;
        use chrono::{DateTime, Utc};
        use std::time::Duration;

        fn t(secs: i64) -> DateTime<Utc> {
            DateTime::from_timestamp(1_800_000_000 + secs, 0).unwrap()
        }

        const BOUND: Duration = Duration::from_secs(86_400);

        /// The baseline the others are variations on: nothing changed between
        /// the collect and the interrupt, so the expiry proceeds.
        #[test]
        fn an_unchanged_wait_is_still_expirable() {
            assert!(still_expirable(
                "run-1",
                Some(("run-1", t(0))),
                Some(t(0)),
                t(86_500),
                BOUND
            ));
        }

        /// A supervisor attached in the gap. The wait is answerable again, so
        /// nothing may end the turn however long it has been parked.
        #[test]
        fn a_supervisor_attaching_mid_sweep_cancels_the_expiry() {
            assert!(!still_expirable(
                "run-1",
                Some(("run-1", t(0))),
                None,
                t(86_500),
                BOUND
            ));
        }

        /// The sharp one. A client that connects and quits again leaves
        /// `unattended_since` as `Some` at a LATER instant, which is a FRESH
        /// window, not a continuation of the expired one. Re-reading only
        /// whether the engine is unattended would read this as "still nobody
        /// here" and expire a run whose deadline is now a day away.
        #[test]
        fn a_reconnect_and_a_second_disconnect_restarts_the_window() {
            // Parked and unattended since t(0); the 24h deadline passed at
            // t(86_400). A client arrives at t(86_402) and quits at t(86_420).
            assert!(!still_expirable(
                "run-1",
                Some(("run-1", t(0))),
                Some(t(86_420)),
                t(86_425),
                BOUND
            ));
        }

        /// The handle was replaced (a steer, or a re-dispatch over a run whose
        /// interrupt did not settle in time). Interrupting by chat id alone
        /// would kill the successor, which nobody judged due.
        #[test]
        fn a_replaced_run_is_not_interrupted_in_its_predecessors_name() {
            assert!(!still_expirable(
                "run-1",
                Some(("run-2", t(0))),
                Some(t(0)),
                t(86_500),
                BOUND
            ));
        }

        /// The user answered the card while an earlier due run was settling, so
        /// this run is now actively progressing. `live_wait` returns `None`
        /// both for a vanished handle and for one with nothing parked.
        #[test]
        fn a_wait_that_was_answered_mid_sweep_is_left_alone() {
            assert!(!still_expirable(
                "run-1",
                None,
                Some(t(0)),
                t(86_500),
                BOUND
            ));
        }

        /// A run that re-parked after the collect gets its own full window
        /// measured from the new park, not the old one.
        #[test]
        fn a_freshly_re_parked_wait_is_not_yet_due() {
            assert!(!still_expirable(
                "run-1",
                Some(("run-1", t(86_400))),
                Some(t(0)),
                t(86_500),
                BOUND
            ));
        }
    }
}
