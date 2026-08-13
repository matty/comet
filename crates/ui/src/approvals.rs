//! The blocked-turn surface: what the current turn is waiting on, and what the
//! user can do about it.
//!
//! Two waits share one mechanism because they are the same problem
//! (`.agents/rules/user-facing-errors.md` rule 2): an approval is a
//! DELIBERATE indefinite wait, a tool call with no result is an ACCIDENTAL
//! one — a tool call that never returns has no timeout and no recovery — and
//! neither is bounded by anything today. Both resolve into a named state with
//! an escape hatch rather than an auto-kill — a long build is legitimately
//! long, and auto-denying it would turn a slow success into a fabricated
//! failure.
//!
//! Everything here is pure over the transcript, so the three surfaces that
//! render it (transcript card, composer decision row, status strip) cannot
//! disagree about what is pending.

use std::time::Instant;

use comet_doc::{MessagePart, MessageRole, SessionMessageEntry};
use comet_proto::view::tool_chip_content;
use comet_proto::{ApprovalDecision, ApprovalRequest};
use gpui::SharedString;

/// How long a tool call may run with no result before the strip names it.
///
/// A single 12-minute run motivated adding this state and was never
/// reproduced — so this is a threshold chosen to be well clear of ordinary
/// work (a build, a test run, a large search all legitimately exceed it,
/// which is why the state is informational and offers Stop rather than
/// acting on its own), NOT a value tuned to that single observation.
///
/// This 60s is deliberately GREATER than `SESSION_STALE_MS` (45s,
/// `crates/proto/src/view.rs`) — the blocked strip only renders under
/// `Indicator::Working`/`AwaitingInput`, and the stale gate turns both to
/// `None` past 45s. The line is reachable at all only because `drive_run`'s
/// 15s liveness heartbeat calls `touch_session` unconditionally while the
/// harness stream is open (`crates/engine/src/sessions.rs`, the
/// `live_heartbeat` tick), keeping the session fresh independent of whether
/// the tool call itself is producing events. Make that heartbeat conditional
/// on stream activity — a plausible future optimisation — and this line
/// silently becomes unreachable, which is exactly the state it exists to name.
pub const HUNG_TOOL_AFTER_SECS: i64 = 60;

/// The unresolved approval the decision row should serve: an approval part
/// with no decision on the LAST ASSISTANT entry.
///
/// The scoping rule is `composer::pending_input_request`'s, reused rather than
/// reinvented — see the forensics in its doc comment. What is deliberately NOT
/// reused is the latch: a decided approval, including the host-stamped
/// `Expired`, is not pending. An approval answers a JSON-RPC request id owned
/// by a process that may have exited, and there is no dead-run fallback for it.
pub fn pending_approval(transcript: &[SessionMessageEntry]) -> Option<(String, ApprovalRequest)> {
    transcript
        .iter()
        .rev()
        .find(|entry| entry.role == MessageRole::Assistant)
        .and_then(|entry| {
            entry.parts.iter().find_map(|part| match part {
                MessagePart::Approval {
                    request_id,
                    approval,
                    decision: None,
                    ..
                } => Some((request_id.clone(), approval.clone())),
                _ => None,
            })
        })
}

/// The decision recorded for `request_id` anywhere in the transcript — what
/// releases the decision row, and what the card renders as its terminal state.
pub fn approval_decision(
    transcript: &[SessionMessageEntry],
    request_id: &str,
) -> Option<ApprovalDecision> {
    transcript.iter().rev().find_map(|entry| {
        entry.parts.iter().find_map(|part| match part {
            MessagePart::Approval {
                request_id: rid,
                decision: Some(decision),
                ..
            } if rid == request_id => Some(decision.clone()),
            _ => None,
        })
    })
}

/// Whether `request_id` is still an open approval with no decision recorded
/// anywhere in the transcript — the composer's 2s safety net asks this before
/// un-hiding the decision row.
///
/// Written out by hand rather than trusted to a fixture (the
/// `.agents/rules/optional-wire-fields.md` trap in miniature): the case this
/// guards — a decision that queued and was then REJECTED by the host because
/// the run's resolver was already gone — produces a transcript no other test
/// here constructs, so it earns its own.
pub fn approval_still_unanswered(transcript: &[SessionMessageEntry], request_id: &str) -> bool {
    pending_approval(transcript).is_some_and(|(id, _)| id == request_id)
        && approval_decision(transcript, request_id).is_none()
}

/// What the current turn is waiting on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockedOn {
    /// A decision only a person can give. Bounded by the user, not by a timer.
    Approval { key: SharedString },
    /// A tool call that has produced no result. `label`/`detail` are the same
    /// pair the transcript's tool chip shows, so the strip and the chip name
    /// the call identically.
    Tool {
        key: SharedString,
        label: &'static str,
        detail: SharedString,
    },
}

/// The blocked state of the last assistant entry, if any.
///
/// An open approval outranks an unresolved call: with a real provider they are
/// the same action — the call has no result BECAUSE the approval is open — and
/// naming the call would report the symptom while hiding the thing the user
/// can actually answer.
pub fn blocked_on(transcript: &[SessionMessageEntry]) -> Option<BlockedOn> {
    let entry = transcript
        .iter()
        .rev()
        .find(|entry| entry.role == MessageRole::Assistant)?;
    if let Some((request_id, _)) = pending_approval(transcript) {
        return Some(BlockedOn::Approval {
            key: request_id.into(),
        });
    }
    entry.parts.iter().rev().find_map(|part| match part {
        MessagePart::Tool {
            id,
            call,
            resolved: false,
            ..
        } => {
            let (label, detail) = tool_chip_content(call);
            Some(BlockedOn::Tool {
                key: id.clone().into(),
                label,
                detail: detail.into(),
            })
        }
        _ => None,
    })
}

/// The status strip's blocked line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockedLine {
    pub text: SharedString,
    pub elapsed_secs: i64,
    /// Whether to offer Stop. True for both waits, and the reason differs
    /// between them: on a hung call Stop is the only exit, while on an approval
    /// Deny ends the APPROVAL and Stop ends the TURN. Those are different
    /// exits, not two ways of saying the same thing — denying leaves the agent
    /// running and free to try something else.
    ///
    /// Suppressing it here made a pending approval the one live run in the app
    /// that could not be interrupted: the decision row replaces the composer,
    /// so the send button's own Stop is off screen for exactly as long as the
    /// wait lasts.
    pub stoppable: bool,
}

/// Drive the blocked-turn clock and produce the line, or `None` while there is
/// nothing to report.
///
/// `stamp` is the caller's `(key, first_seen)` cell. The clock is UI-LOCAL: no
/// part carries a start timestamp, so a client that restarts mid-wait
/// under-reports rather than inventing a start it never observed. Under-report
/// is the safe direction — the line only appears once a wait is long, and it
/// never claims a wait was longer than it was.
pub fn blocked_line(
    stamp: &mut Option<(SharedString, Instant)>,
    blocked: Option<BlockedOn>,
    now: Instant,
) -> Option<BlockedLine> {
    let Some(blocked) = blocked else {
        // A settled turn must not leave a stamp behind, or the next call to
        // the same tool id would inherit this one's age.
        *stamp = None;
        return None;
    };
    let key = match &blocked {
        BlockedOn::Approval { key } => key.clone(),
        BlockedOn::Tool { key, .. } => key.clone(),
    };
    let since = match stamp {
        Some((seen, since)) if *seen == key => *since,
        _ => {
            *stamp = Some((key.clone(), now));
            now
        }
    };
    let elapsed_secs = now.duration_since(since).as_secs() as i64;
    match blocked {
        // A wait for a person is blocked from the first frame: there is no
        // threshold at which it becomes true.
        BlockedOn::Approval { .. } => Some(BlockedLine {
            text: "Waiting for approval".into(),
            elapsed_secs,
            stoppable: true,
        }),
        BlockedOn::Tool { label, detail, .. } => {
            if elapsed_secs < HUNG_TOOL_AFTER_SECS {
                return None;
            }
            Some(BlockedLine {
                // "Still waiting on Run · pwsh …" rather than "Running pwsh …":
                // the chip's own label/detail pair reads correctly for all
                // eleven ToolCall kinds, where a hand-written verb only reads
                // correctly for Exec.
                text: format!("Still waiting on {label} · {detail}").into(),
                elapsed_secs,
                stoppable: true,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use comet_doc::MessageStatus;
    use comet_proto::{FileOperation, ToolCall};

    fn approval_part(request_id: &str, decision: Option<ApprovalDecision>) -> MessagePart {
        MessagePart::Approval {
            id: format!("ap-{request_id}"),
            request_id: request_id.into(),
            approval: ApprovalRequest::FileChange {
                path: "src/reconcile.rs".into(),
                operation: FileOperation::Modify,
                added_lines: 24,
                removed_lines: 6,
            },
            decision,
        }
    }

    fn tool_part(id: &str, resolved: bool) -> MessagePart {
        MessagePart::Tool {
            id: id.into(),
            call: ToolCall::Exec {
                command: "pwsh -NoProfile -Command \"Get-ChildItem\"".into(),
            },
            is_error: false,
            resolved,
            diff_ref: None,
            diff_stats: None,
        }
    }

    fn assistant(id: &str, status: MessageStatus, parts: Vec<MessagePart>) -> SessionMessageEntry {
        SessionMessageEntry {
            id: id.into(),
            role: MessageRole::Assistant,
            parts,
            created_at: 0,
            device_id: "d".into(),
            status: Some(status),
            continuation_of: None,
        }
    }

    fn user(id: &str) -> SessionMessageEntry {
        SessionMessageEntry {
            id: id.into(),
            role: MessageRole::User,
            parts: vec![MessagePart::Text {
                id: "t".into(),
                text: "steer".into(),
            }],
            created_at: 1,
            device_id: "d".into(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
        }
    }

    #[test]
    fn an_undecided_approval_on_the_last_assistant_entry_is_pending() {
        let t = vec![assistant(
            "m1",
            MessageStatus::Streaming,
            vec![approval_part("r1", None)],
        )];
        assert_eq!(pending_approval(&t).map(|(id, _)| id), Some("r1".into()));
        assert!(pending_approval(&[]).is_none());
    }

    /// The scoping rule reused verbatim from `pending_input_request`: a steer
    /// prompt appends a USER entry BEHIND the streaming assistant entry, and a
    /// last-entry read made the question panel vanish exactly when the user
    /// typed (composer.rs:378-389 forensics).
    #[test]
    fn a_user_entry_appended_behind_the_stream_does_not_hide_the_approval() {
        let t = vec![
            assistant(
                "m1",
                MessageStatus::Streaming,
                vec![approval_part("r1", None)],
            ),
            user("u1"),
        ];
        assert_eq!(pending_approval(&t).map(|(id, _)| id), Some("r1".into()));
    }

    /// A newer assistant entry supersedes an unanswered approval, same as a
    /// question.
    #[test]
    fn a_newer_assistant_entry_supersedes_an_unanswered_approval() {
        let t = vec![
            assistant(
                "m1",
                MessageStatus::Aborted,
                vec![approval_part("r1", None)],
            ),
            assistant(
                "m2",
                MessageStatus::Complete,
                vec![MessagePart::Text {
                    id: "t2".into(),
                    text: "moved on".into(),
                }],
            ),
        ];
        assert!(pending_approval(&t).is_none());
    }

    /// **The one place approvals must not copy the question latch.** A decided
    /// approval — INCLUDING the host-stamped `Expired` — is not pending. A
    /// question stays answerable after its run dies because `RespondInput` has
    /// a dead-run fallback; an approval answers a JSON-RPC id owned by a
    /// process that has exited, so an Allow button here would do nothing,
    /// silently.
    #[test]
    fn any_decision_including_expired_ends_the_pending_state() {
        for decision in [
            ApprovalDecision::Allow,
            ApprovalDecision::AllowForSession,
            ApprovalDecision::Deny {
                message: "no".into(),
            },
            ApprovalDecision::Expired,
        ] {
            let t = vec![assistant(
                "m1",
                MessageStatus::Complete,
                vec![approval_part("r1", Some(decision.clone()))],
            )];
            assert!(
                pending_approval(&t).is_none(),
                "a decided approval is not answerable: {decision:?}"
            );
            assert_eq!(approval_decision(&t, "r1"), Some(decision));
        }
    }

    #[test]
    fn a_decision_is_read_from_any_entry_not_just_the_last() {
        let t = vec![
            assistant(
                "m1",
                MessageStatus::Complete,
                vec![approval_part("r1", Some(ApprovalDecision::Allow))],
            ),
            assistant("m2", MessageStatus::Streaming, vec![]),
        ];
        assert_eq!(approval_decision(&t, "r1"), Some(ApprovalDecision::Allow));
        assert_eq!(approval_decision(&t, "other"), None);
    }

    /// The composer's 2s safety net asks this before un-hiding the decision
    /// row — it must say yes only while the exact request is still both the
    /// pending one AND carries no recorded decision.
    #[test]
    fn approval_still_unanswered_is_true_only_while_genuinely_open() {
        let still_pending = vec![assistant(
            "m1",
            MessageStatus::Streaming,
            vec![approval_part("r1", None)],
        )];
        assert!(approval_still_unanswered(&still_pending, "r1"));

        let decided_since = vec![assistant(
            "m1",
            MessageStatus::Complete,
            vec![approval_part("r1", Some(ApprovalDecision::Allow))],
        )];
        assert!(!approval_still_unanswered(&decided_since, "r1"));

        let superseded = vec![
            assistant(
                "m1",
                MessageStatus::Aborted,
                vec![approval_part("r1", None)],
            ),
            assistant(
                "m2",
                MessageStatus::Complete,
                vec![MessagePart::Text {
                    id: "t2".into(),
                    text: "moved on".into(),
                }],
            ),
        ];
        assert!(!approval_still_unanswered(&superseded, "r1"));

        assert!(!approval_still_unanswered(&still_pending, "other-request"));
    }

    #[test]
    fn an_open_approval_outranks_an_unresolved_tool_call() {
        // With a real provider these are the SAME action: the tool has no
        // result BECAUSE the approval is open. Reporting the tool would name
        // the symptom and hide the thing the user can actually answer.
        let t = vec![assistant(
            "m1",
            MessageStatus::Streaming,
            vec![tool_part("t1", false), approval_part("r1", None)],
        )];
        assert!(matches!(blocked_on(&t), Some(BlockedOn::Approval { .. })));
    }

    /// Same rule as above, parts in the OPPOSITE order. `blocked_on` checks
    /// `pending_approval` (which scans the WHOLE entry) before it falls back
    /// to a reverse scan for an unresolved tool part — an implementation that
    /// picked whichever part came last (or first) in the vec, rather than
    /// genuinely preferring the approval, would pass the `[tool, approval]`
    /// fixture above by accident and only fail here.
    #[test]
    fn an_open_approval_outranks_an_unresolved_tool_call_in_either_part_order() {
        let t = vec![assistant(
            "m1",
            MessageStatus::Streaming,
            vec![approval_part("r1", None), tool_part("t1", false)],
        )];
        assert!(matches!(blocked_on(&t), Some(BlockedOn::Approval { .. })));
    }

    #[test]
    fn an_unresolved_tool_call_is_the_blocked_state_when_nothing_is_pending() {
        let t = vec![assistant(
            "m1",
            MessageStatus::Streaming,
            vec![tool_part("t1", true), tool_part("t2", false)],
        )];
        let Some(BlockedOn::Tool { key, label, .. }) = blocked_on(&t) else {
            panic!("expected the unresolved call to be the blocked state");
        };
        assert_eq!(
            key.as_ref(),
            "t2",
            "the LAST unresolved call, not the first"
        );
        assert_eq!(label, "Run");
    }

    #[test]
    fn a_turn_with_every_call_resolved_is_not_blocked() {
        let t = vec![assistant(
            "m1",
            MessageStatus::Streaming,
            vec![tool_part("t1", true)],
        )];
        assert!(blocked_on(&t).is_none());
    }

    /// The `None` path a `.find` over an absent assistant entry takes — an
    /// empty transcript (no messages sent yet) and a transcript that only has
    /// a user entry (nothing has been answered by the assistant at all). Both
    /// have to fall out of `blocked_on`'s leading `?` cleanly rather than
    /// panicking on a `.rev().find()` that never matches
    /// (`.agents/rules/optional-wire-fields.md`: write the absent case
    /// yourself, or it ships never having been constructed).
    #[test]
    fn blocked_on_is_none_with_no_assistant_entry() {
        assert!(blocked_on(&[]).is_none(), "an empty transcript");
        assert!(
            blocked_on(&[user("u1")]).is_none(),
            "a user-only transcript, before any assistant reply"
        );
    }

    /// An approval reports the moment it appears — a wait for a person is
    /// blocked by definition. A tool call does not: a long build is
    /// legitimately long, and the line must not accuse it of hanging.
    #[test]
    fn only_the_tool_arm_waits_for_the_threshold() {
        let t0 = Instant::now();
        let mut stamp = None;
        let approval = Some(BlockedOn::Approval { key: "r1".into() });
        let line = blocked_line(&mut stamp, approval, t0).expect("approvals report immediately");
        assert_eq!(line.elapsed_secs, 0);
        assert!(line.text.contains("Waiting for approval"), "{}", line.text);
        assert!(
            line.stoppable,
            "Deny ends the approval; Stop ends the turn. The decision row \
             replaces the composer, so this line carries the only Stop on \
             screen while the wait lasts."
        );

        let mut stamp = None;
        let tool = || {
            Some(BlockedOn::Tool {
                key: "t1".into(),
                label: "Run",
                detail: "pwsh -NoProfile".into(),
            })
        };
        assert!(
            blocked_line(&mut stamp, tool(), t0).is_none(),
            "a fresh call is not a hung call"
        );
        let later = t0 + std::time::Duration::from_secs(HUNG_TOOL_AFTER_SECS as u64 + 12);
        let line = blocked_line(&mut stamp, tool(), later).expect("past the threshold");
        assert_eq!(line.elapsed_secs, HUNG_TOOL_AFTER_SECS + 12);
        assert!(line.text.contains("pwsh -NoProfile"), "{}", line.text);
        assert!(line.stoppable, "the wait has to give the user a way out");
    }

    /// The clock is keyed on the call, not on the turn: a second call must
    /// start its own wait rather than inherit the first one's age.
    #[test]
    fn the_clock_restarts_when_the_blocked_thing_changes() {
        let t0 = Instant::now();
        let past = t0 + std::time::Duration::from_secs(HUNG_TOOL_AFTER_SECS as u64 + 1);
        let mut stamp = None;
        let call = |key: &str| {
            Some(BlockedOn::Tool {
                key: key.into(),
                label: "Run",
                detail: "x".into(),
            })
        };
        assert!(blocked_line(&mut stamp, call("t1"), t0).is_none());
        assert!(blocked_line(&mut stamp, call("t1"), past).is_some());
        assert!(
            blocked_line(&mut stamp, call("t2"), past).is_none(),
            "a new call inherits nothing from the old one's clock"
        );
    }

    #[test]
    fn nothing_blocked_clears_the_clock() {
        let t0 = Instant::now();
        let mut stamp = Some((SharedString::from("t1"), t0));
        assert!(blocked_line(&mut stamp, None, t0).is_none());
        assert!(
            stamp.is_none(),
            "a settled turn must not leave a stamp behind"
        );
    }

    /// The two indicator arms of the status strip must read the SAME helper.
    /// The regression this guards is a strip that reports an approval wait
    /// only while the session is `Working`: an approval sets the session to
    /// `AwaitingInput` (sessions.rs:1310), so an arm-specific implementation
    /// would go silent at exactly the moment the wait begins.
    #[test]
    fn an_approval_wait_reports_under_the_awaiting_input_indicator_too() {
        let t = vec![assistant(
            "m1",
            MessageStatus::Streaming,
            vec![approval_part("r1", None)],
        )];
        let mut stamp = None;
        let line = blocked_line(&mut stamp, blocked_on(&t), Instant::now())
            .expect("an open approval is a blocked turn regardless of indicator");
        assert!(line.stoppable);
        assert!(line.text.contains("Waiting for approval"));
    }
}
