//! Shared plumbing for the live-provider e2e tests (`*_live.rs`): all of them
//! are `#[ignore]`d — they need a real, authenticated CLI and are never run by
//! CI — so nothing here is exercised outside a deliberate `--ignored` run.
//!
//! `approval_diagnostic.rs`, in this same `tests/` directory, drives this
//! module directly with an in-process double and is NOT `#[ignore]`d: it is
//! the evidence that [`settle_or_report`] actually does what this module
//! claims, without needing a real CLI.
//!
//! D71 (2): "unanswered approvals park to the timeout." The live tests never
//! answer an approval — a real evaluator is not sitting at the keyboard for
//! an automated run — but the old idiom for saying so,
//! `request_approval: Box::new(|_| oneshot::channel().1)`, dropped the sender
//! in the same statement, so the run declined the approval immediately and
//! moved on. That was never the hang. The hang came after: when a run then
//! stalled for an unrelated reason (an escalation the harness could not shed
//! cleanly, a retry loop, a wedge in the child process), every live test's
//! `tokio::time::timeout(...).expect("the turn settles rather than hanging")`
//! fired with a message that named nothing — the fact that an approval had
//! been raised and auto-declined earlier in the run was already gone by the
//! time the deadline arrived. [`recording_decliner`] keeps a record instead of
//! throwing it away, and [`settle_or_report`] fails the instant that record is
//! written rather than waiting out the rest of the budget.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use comet_harness::HarnessError;
use comet_proto::{AgentEvent, ApprovalDecision, ApprovalRequest};
use futures::StreamExt;
use futures::stream::BoxStream;
use tokio::sync::{Notify, oneshot};

/// The most recent approval a live run raised that this test's `RunControls`
/// declined without a real answer. Read it after a failure to say what the
/// run was asking for; [`settle_or_report`] does this for you.
type SeenApproval = Arc<Mutex<Option<ApprovalRequest>>>;

/// Watches for the harness escalating to a permission decision this test's
/// `RunControls` never really answers. Pair with [`settle_or_report`], which
/// fails fast the moment [`recording_decliner`]'s responder fires rather than
/// waiting out the read loop's own timeout with no explanation.
pub struct ApprovalWatch {
    seen: SeenApproval,
    notify: Arc<Notify>,
}

/// A `RunControls::request_approval` responder for a live test.
///
/// Behaviorally identical to the old `Box::new(|_| oneshot::channel().1)`
/// idiom — the sender is dropped in the same statement, so the receiver
/// resolves to `Err` and every harness maps that to `ApprovalDecision::Expired`
/// (see e.g. `acp::session::handle_permission_request`'s doc comment) — a live
/// test has no human to answer for real and must not grant itself filesystem
/// or command access on its own authority. The only change is that the
/// request is recorded, and a waiter is woken, before the sender is dropped.
pub fn recording_decliner() -> (
    Box<dyn Fn(ApprovalRequest) -> oneshot::Receiver<ApprovalDecision> + Send + Sync>,
    ApprovalWatch,
) {
    let seen: SeenApproval = Arc::new(Mutex::new(None));
    let notify = Arc::new(Notify::new());
    let seen_for_closure = Arc::clone(&seen);
    let notify_for_closure = Arc::clone(&notify);
    let responder = move |request: ApprovalRequest| {
        *seen_for_closure.lock().expect("not poisoned") = Some(request);
        notify_for_closure.notify_one();
        oneshot::channel().1
    };
    (Box::new(responder), ApprovalWatch { seen, notify })
}

/// Read a run's event stream to its first `Done`, calling `on_event` with
/// each event as it arrives (a live test uses this for its own `print!`s and
/// bookkeeping — this helper only owns the loop and the deadline, not what a
/// caller wants to do with an event).
///
/// **Fails fast, and names what happened.** Three ways out:
/// - the stream produces `Done`: returns the collected events normally.
/// - `watch` fires before `Done` does: panics immediately (does not wait out
///   the rest of `budget`) naming the approval the run raised — the
///   diagnostic D71 (2) says was hand-written once for a single run, then
///   deleted along with the test that needed it.
/// - `budget` elapses with neither of the above: panics with the same bare
///   message the old direct `tokio::time::timeout(...)` calls used, so a hang
///   this helper cannot explain still fails instead of blocking the suite.
pub async fn settle_or_report<F>(
    mut stream: BoxStream<'static, Result<AgentEvent, HarnessError>>,
    budget: Duration,
    watch: &ApprovalWatch,
    mut on_event: F,
) -> Vec<AgentEvent>
where
    F: FnMut(&AgentEvent),
{
    let mut events: Vec<AgentEvent> = Vec::new();
    // Pinned once, outside the loop: a fresh `tokio::time::sleep(budget)` per
    // iteration would restart the clock on every event and turn an ABSOLUTE
    // budget into an idle timeout, which is not what `budget` documents or
    // what the tests this replaces relied on.
    let deadline = tokio::time::sleep(budget);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            biased;

            () = watch.notify.notified() => {
                let pending = watch.seen.lock().expect("not poisoned").clone();
                match pending {
                    Some(request) => {
                        let (label, detail) = comet_proto::view::approval_chip_content(&request);
                        panic!(
                            "the run escalated to an approval this test's RunControls declines \
                             by design (no human answers a live test): {label} ({detail:?}). \
                             Failing here rather than waiting out the {budget:?} budget for the \
                             turn to settle -- D71 (2). Collected before this point: {events:#?}"
                        );
                    }
                    // The notify fired but the record it should have carried
                    // is gone -- can't happen from this module's own
                    // responder, which writes the record before notifying,
                    // but panic descriptively rather than silently looping
                    // forever if some other caller wires the Notify up wrong.
                    None => panic!(
                        "an approval watch fired with no recorded request: {events:#?}"
                    ),
                }
            }

            event = stream.next() => {
                match event {
                    Some(event) => {
                        let event = event.expect("no transport error");
                        on_event(&event);
                        let settled = matches!(event, AgentEvent::Done { .. });
                        events.push(event);
                        if settled {
                            return events;
                        }
                    }
                    None => panic!(
                        "the stream ended before a Done event, with no approval seen either: \
                         {events:#?}"
                    ),
                }
            }

            () = &mut deadline => {
                panic!(
                    "the turn settles rather than hanging (budget {budget:?} elapsed, no \
                     approval request seen either): {events:#?}"
                );
            }
        }
    }
}
