//! The ACP core driven end to end against the `fake-acp` fixture.
//!
//! `acp_fixture.rs` proves the fixture speaks the protocol; this proves Comet's
//! side of it. The split matters — a hardening test written against an
//! unverified fixture passes for the wrong reason.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use comet_harness::acp::session::{AcpSession, SettleSignal, Timeouts};
use comet_harness::{CancellationToken, HarnessError, RunControls, SteerMessage};
use comet_proto::{
    AgentEvent, ApprovalDecision, ApprovalRequest, DiagnosticSeverity, DoneStatus, HarnessId,
    RunRequest, RuntimeMode,
};
use futures::StreamExt;
use futures::stream::BoxStream;
use tokio::sync::{mpsc, oneshot};

/// Short enough that a hung loop fails the suite instead of stalling it, long
/// enough that nothing here is a speed measurement.
///
/// `prompt_stall` is deliberately generous (10s): most of this suite's modes
/// (`starve`, `ignore-cancel`) are silent by design and settle through a
/// DIFFERENT mechanism (cancel, or the drain on a later reply) well inside
/// that window — a short default here would race those tests against a bound
/// meant for a different scenario. `a_stalled_prompt_ends_in_a_bounded_error`
/// overrides it to something worth waiting for in a test.
const TEST_TIMEOUTS: Timeouts = Timeouts {
    handshake: Duration::from_secs(10),
    cancel_grace: Duration::from_millis(750),
    kill_grace: Duration::from_millis(250),
    prompt_stall: Duration::from_secs(10),
};

fn controls() -> (RunControls, mpsc::Sender<SteerMessage>, CancellationToken) {
    let (steer_tx, steer_rx) = mpsc::channel(4);
    let token = CancellationToken::new();
    let controls = RunControls {
        request_input: Box::new(|_| oneshot::channel().1),
        request_approval: Box::new(|_| oneshot::channel().1),
        steering: steer_rx,
        interrupt: token.clone(),
    };
    (controls, steer_tx, token)
}

/// Same as [`controls`], with a caller-supplied approval bridge — every
/// approval test below needs to see what request reached the bridge, or to
/// control what it answers, or both.
fn controls_with_approval(
    request_approval: impl Fn(ApprovalRequest) -> oneshot::Receiver<ApprovalDecision>
    + Send
    + Sync
    + 'static,
) -> (RunControls, mpsc::Sender<SteerMessage>, CancellationToken) {
    let (steer_tx, steer_rx) = mpsc::channel(4);
    let token = CancellationToken::new();
    let controls = RunControls {
        request_input: Box::new(|_| oneshot::channel().1),
        request_approval: Box::new(request_approval),
        steering: steer_rx,
        interrupt: token.clone(),
    };
    (controls, steer_tx, token)
}

fn request(prompt: &str) -> RunRequest {
    RunRequest {
        prompt: prompt.into(),
        cwd: ".".into(),
        model: Some("fake-model".into()),
        ..RunRequest::for_session(RuntimeMode::default())
    }
}

/// This suite drives the generic turn loop's mechanics (settle, cancel,
/// refusal, drop-reply, exit) against `HarnessId::Mock`, never a real
/// provider's usage shape — `normalize::usage` and `grok::usage` are both
/// `pub(crate)` and unreachable from here besides. `session::run` takes a
/// per-agent reader since PR1 (`crates/harness/src/acp/session.rs`'s
/// `UsageReader`); Mock has no reader of its own, so this always answers
/// `None`, which is the honest "no usage" case every real reader already
/// treats as such.
fn no_usage(_: &serde_json::Value, _: Option<u64>) -> Option<AgentEvent> {
    None
}

/// This suite drives the generic turn loop, not the model/effort/resume
/// wiring PR7 adds (that is `acp_run_fidelity.rs`) — every `open*` helper
/// below passes a bare request through `AcpSession::open`, so nothing here
/// ever has a selection to apply.
fn no_config(_: &RunRequest, _: &str) -> Vec<(&'static str, serde_json::Value)> {
    Vec::new()
}

/// Grok's own [`SettleSignal`] (D122), mirrored here rather than reused:
/// `grok::SETTLE_SIGNAL` is `pub(crate)` inside `comet-harness` and
/// unreachable from this integration-test crate — same reason `test_usage`
/// mirrors `grok::usage` below instead of naming it. `fake-acp` speaks Grok's
/// exact wire shape for this notification (`fake_acp.rs`'s own
/// `PROMPT_COMPLETE_METHOD`), so every field here matches `grok::SETTLE_SIGNAL`
/// exactly: the method name, both promptId paths, and the 250ms bound several
/// of this suite's own tests measure their timing against.
fn settle_signal() -> SettleSignal {
    SettleSignal {
        method: "_x.ai/session/prompt_complete",
        notification_prompt_id: |params| params["promptId"].as_str(),
        reply_prompt_id: |result| result["_meta"]["promptId"].as_str(),
        reply_bound: Duration::from_millis(250),
    }
}

/// A `cancel_grace` long enough that giving up cannot be what ends a run
/// inside `drain`'s own limit.
///
/// **This is load-bearing, not padding.** With the short grace, the
/// agent-settles-the-cancel test passed even when `session/cancel` was never
/// sent — the give-up path produced the same `Interrupted`. Only a grace that
/// outlives the drain makes a real settle the sole way to finish in time.
const LONG_CANCEL: Timeouts = Timeouts {
    cancel_grace: Duration::from_secs(120),
    ..TEST_TIMEOUTS
};

async fn open(no_steering: bool) -> AcpSession {
    open_with(no_steering, TEST_TIMEOUTS).await
}

async fn open_with(no_steering: bool, timeouts: Timeouts) -> AcpSession {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_fake-acp"));
    if no_steering {
        command.env("FAKE_ACP_NO_STEERING", "1");
    }
    AcpSession::open(
        command,
        ".",
        timeouts,
        &RunRequest::for_session(RuntimeMode::default()),
        no_config,
        // This suite never drives a `session/new` failure through
        // `AcpSession::open` (that is `acp_run_fidelity.rs` and the
        // per-agent unit tests in `grok.rs`/`hermes.rs`), so nothing here
        // has a "signed out" shape to recognize -- every open passes the
        // original error through unchanged. A capture-less closure literal,
        // not a named `fn` item: `OpenFailureMapper`'s parameter type
        // (`comet_harness::jsonrpc::RpcFailure`) is `pub(crate)` inside
        // `comet-harness` and unnameable from this integration-test crate,
        // so a named item with an explicit type annotation cannot be
        // written here at all -- only a closure whose parameter type Rust
        // infers from `AcpSession::open`'s own signature.
        |_| None,
    )
    .await
    .expect("the fixture handshakes")
}

/// Drain a run to its end, failing the test rather than hanging if it never
/// produces one.
async fn drain(stream: BoxStream<'static, Result<AgentEvent, HarnessError>>) -> Vec<AgentEvent> {
    tokio::time::timeout(Duration::from_secs(10), stream.collect::<Vec<_>>())
        .await
        .expect("the run ends rather than hanging")
        .into_iter()
        .map(|event| event.expect("no transport error"))
        .collect()
}

fn text_of(events: &[AgentEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// Every `Done` in the stream. There is one per turn that started, so a steered
/// run has two — asserting on the count is how a missing or duplicated turn
/// boundary shows up rather than being averaged away.
fn dones(events: &[AgentEvent]) -> Vec<&AgentEvent> {
    events
        .iter()
        .filter(|e| matches!(e, AgentEvent::Done { .. }))
        .collect()
}

/// The single `Done` of a single-turn run.
fn only_done(events: &[AgentEvent]) -> &AgentEvent {
    let dones = dones(events);
    assert_eq!(
        dones.len(),
        1,
        "a one-turn run ends with exactly one Done: {events:#?}"
    );
    dones[0]
}

/// The whole of task 4: handshake, session, prompt, stream, settle.
#[tokio::test]
async fn a_turn_runs_from_handshake_to_done() {
    let session = open(false).await;
    let session_id = session.session_id().to_owned();
    assert!(session_id.starts_with("fake-session-"), "got {session_id}");

    let (controls, steer, _token) = controls();
    let stream = comet_harness::acp::session::run(
        session,
        HarnessId::Mock,
        request("hello"),
        controls,
        no_usage,
        Some(settle_signal()),
    );
    // One turn only: closing the mailbox is what tells the persistent session
    // no steer is coming. Held open, it would correctly wait for one.
    drop(steer);
    let events = drain(stream).await;

    match &events[0] {
        AgentEvent::SessionStarted {
            harness,
            session_id: started,
            model,
            ..
        } => {
            assert_eq!(*harness, HarnessId::Mock);
            assert_eq!(started, &session_id);
            assert_eq!(model, "fake-model");
        }
        other => panic!("a run opens with SessionStarted, got {other:?}"),
    }

    assert_eq!(text_of(&events), "working done");
    match only_done(&events) {
        AgentEvent::Done { status, error, .. } => {
            assert_eq!(*status, DoneStatus::Completed);
            assert!(error.is_none(), "a clean turn carries no error");
        }
        other => unreachable!("{other:?}"),
    }
}

/// Break caught: mapping ACP's `refusal` onto a success, or onto a crash. It is
/// the agent declining — a real, non-error end the user should see as such.
#[tokio::test]
async fn a_refusal_ends_the_run_without_an_error_message() {
    let (controls, steer, _token) = controls();
    let stream = comet_harness::acp::session::run(
        open(false).await,
        HarnessId::Mock,
        request("please refusal"),
        controls,
        no_usage,
        Some(settle_signal()),
    );
    drop(steer);
    let events = drain(stream).await;

    match only_done(&events) {
        AgentEvent::Done { status, error, .. } => {
            assert_eq!(*status, DoneStatus::Errored);
            assert!(
                error.is_none(),
                "a refusal is the agent's answer, not a crash to explain: {error:?}"
            );
        }
        other => unreachable!("{other:?}"),
    }
}

/// A steer between turns becomes the next `session/prompt` on the SAME session.
/// This is the fallback path, and with no agent advertising the steering
/// extension it is the only path — so it has to be the one that is built.
#[tokio::test]
async fn a_queued_steer_runs_as_the_next_prompt_on_the_same_session() {
    let session = open(true).await;
    let session_id = session.session_id().to_owned();
    assert!(
        !session.agent().supports_steering(),
        "this fixture withholds the extension, so the boundary path is what runs"
    );

    let (controls, steer, _token) = controls();
    let stream = comet_harness::acp::session::run(
        session,
        HarnessId::Mock,
        request("first"),
        controls,
        no_usage,
        Some(settle_signal()),
    );

    steer
        .send(SteerMessage {
            prompt: "second".into(),
            message_id: None,
        })
        .await
        .expect("queue a steer");
    drop(steer);

    let events = drain(stream).await;
    // Break caught (D111): both ids were hardcoded `None`, so a consumer
    // wanting to correlate a steer against the message it cut short had
    // nothing to read. The two must be present AND different — post-steer
    // output folds into a fresh message, which is what makes the two segments
    // distinguishable at all.
    let steered = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::Steered {
                assistant_message_id,
                next_assistant_message_id,
            } => Some((
                assistant_message_id.clone(),
                next_assistant_message_id.clone(),
            )),
            _ => None,
        })
        .unwrap_or_else(|| panic!("the steer is announced: {events:#?}"));
    let (interrupted, next) = steered;
    let interrupted = interrupted.expect("the steer names the message it interrupted");
    let next = next.expect("the steer names the message its answer folds into");
    assert_ne!(
        interrupted, next,
        "post-steer output must fold into a DIFFERENT message"
    );
    // And the id it names is the one the completed message actually carried,
    // not a fresh value invented at the steer.
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::AssistantMessageCompleted { assistant_message_id }
                if *assistant_message_id == interrupted
        )),
        "the interrupted id must match a message that was actually completed: {events:#?}"
    );
    // Two turns ran, so the fixture streamed its text twice.
    assert_eq!(text_of(&events), "working doneworking done");

    // **One Done per turn.** The first turn's boundary is what a UI needs to
    // stop spinning before the steered turn streams over it; a run that
    // reported only at the very end would leave the first turn open.
    let dones = dones(&events);
    assert_eq!(dones.len(), 2, "one Done per turn: {events:#?}");
    for done in dones {
        match done {
            AgentEvent::Done {
                status,
                session_id: ended,
                ..
            } => {
                assert_eq!(*status, DoneStatus::Completed);
                assert_eq!(
                    ended.as_deref(),
                    Some(session_id.as_str()),
                    "a steer continues the session, it does not open a new one"
                );
            }
            other => unreachable!("{other:?}"),
        }
    }
}

/// Interrupting a turn the agent then settles as `cancelled`.
///
/// Runs under [`LONG_CANCEL`] deliberately: the grace period outlasts the
/// drain, so the only thing that can end this run in time is the agent
/// answering a `session/cancel` Comet actually sent. Under the ordinary grace
/// this test passed with the cancel deleted, which is no test at all.
#[tokio::test]
async fn an_interrupt_the_agent_settles_reports_one_interrupted_done() {
    let (controls, _steer, token) = controls();
    let stream = comet_harness::acp::session::run(
        open_with(false, LONG_CANCEL).await,
        HarnessId::Mock,
        // Streams, then waits: the cancel is what settles it.
        request("please drop-reply"),
        controls,
        no_usage,
        Some(settle_signal()),
    );
    token.cancel();

    let events = drain(stream).await;
    match only_done(&events) {
        AgentEvent::Done { status, .. } => assert_eq!(*status, DoneStatus::Interrupted),
        other => unreachable!("{other:?}"),
    }
}

/// **The bounded-wait rule, made real.** This agent is silent *and* deaf: it
/// never answers the prompt and ignores `session/cancel` too, so the client's
/// own give-up is the only thing that can end the turn. An interrupt the user
/// asked for cannot leave the transcript spinning forever.
///
/// The `ignore-cancel` mode exists because `starve` alone did not test this —
/// the fixture settles a starved prompt when the cancel lands, so this test
/// passed with the give-up deadline pushed a day into the future.
#[tokio::test]
async fn an_interrupt_a_silent_agent_ignores_still_ends_the_run() {
    let (controls, _steer, token) = controls();
    let stream = comet_harness::acp::session::run(
        open(false).await,
        HarnessId::Mock,
        request("please starve ignore-cancel"),
        controls,
        no_usage,
        Some(settle_signal()),
    );
    token.cancel();

    let events = drain(stream).await;
    match only_done(&events) {
        AgentEvent::Done { status, .. } => assert_eq!(*status, DoneStatus::Interrupted),
        other => unreachable!("{other:?}"),
    }
}

/// Break caught: settling only on the RPC response. Upstream reports Grok's
/// `session/prompt` RPC hanging silently after a turn really finished; the
/// notification is what ends the turn there. Measured here at 3ms ahead of the
/// response, so it is early and reliable, not a last resort.
#[tokio::test]
async fn a_turn_settles_on_the_completion_notification_alone() {
    let (controls, steer, _token) = controls();
    let stream = comet_harness::acp::session::run(
        open(false).await,
        HarnessId::Mock,
        // The fixture's `complete-notification-only`: sends the completion
        // notification, then never answers the RPC at all — the
        // response-only settle hangs on this shape forever.
        request("please complete-notification-only"),
        controls,
        no_usage,
        Some(settle_signal()),
    );
    drop(steer);
    let events = tokio::time::timeout(Duration::from_secs(5), async { drain(stream).await })
        .await
        .expect("settles off the notification well inside prompt_stall, not the RPC fallback");

    assert_eq!(text_of(&events), "working");
    match only_done(&events) {
        AgentEvent::Done { status, error, .. } => {
            assert_eq!(*status, DoneStatus::Completed);
            assert!(
                error.is_none(),
                "a clean end_turn carries no error: {error:?}"
            );
        }
        other => unreachable!("{other:?}"),
    }
}

/// **D122's falsification: no [`SettleSignal`] means no recognition at
/// all.** Same fixture mode as the test above
/// (`complete-notification-only`: sends the completion notification, then
/// never answers the RPC), but with `settle: None` — proving the arm is
/// genuinely inert for an agent that supplies no signal, rather than merely
/// unexercised in every other test here. If the shared loop still hardcoded
/// the vendor method name (or gated on a flag this build forgot to unset for
/// `HarnessId::Mock`), this would settle exactly like the test above;
/// instead nothing recognizes the notification at all, so the turn is still
/// waiting on an RPC reply this fixture mode never sends.
#[tokio::test]
async fn with_no_settle_signal_the_vendor_notification_goes_unrecognized() {
    let (controls, steer, _token) = controls();
    let stream = comet_harness::acp::session::run(
        open(false).await,
        HarnessId::Mock,
        request("please complete-notification-only"),
        controls,
        no_usage,
        None,
    );
    drop(steer);
    let result = tokio::time::timeout(Duration::from_millis(500), drain(stream)).await;
    assert!(
        result.is_err(),
        "with no SettleSignal supplied, the vendor notification must not \
         settle the turn -- it should still be waiting on the RPC reply \
         this fixture mode never sends: {result:?}"
    );
}

/// The mirror. An agent that never sends the extension must still settle, or
/// this fix trades one hang for another.
#[tokio::test]
async fn a_turn_settles_on_the_rpc_response_alone() {
    let (controls, steer, _token) = controls();
    let stream = comet_harness::acp::session::run(
        open(false).await,
        HarnessId::Mock,
        // `complete-response-only` is the fixture's plain `end_turn` path —
        // named explicitly here because it is the deliberate mirror of
        // `complete-notification-only`, not because the fixture treats it
        // specially.
        request("please complete-response-only"),
        controls,
        no_usage,
        Some(settle_signal()),
    );
    drop(steer);
    let events = drain(stream).await;

    assert_eq!(text_of(&events), "working done");
    match only_done(&events) {
        AgentEvent::Done { status, error, .. } => {
            assert_eq!(*status, DoneStatus::Completed);
            assert!(
                error.is_none(),
                "a clean end_turn carries no error: {error:?}"
            );
        }
        other => unreachable!("{other:?}"),
    }
}

/// Exactly once. Both signals land on a healthy turn 3ms apart; emitting two
/// `Done`s puts two terminal states in one transcript.
///
/// **Asserts on SPEED, not merely on the final count.** The fixture's
/// `complete-both` sends the notification immediately and delays its RPC
/// reply by 600ms — comfortably past `POST_NOTIFICATION_REPLY_BOUND`'s
/// 250ms (`acp/session.rs`), so the client's own harvest of that reply
/// always times out rather than receiving it. Settling near the BOUND
/// rather than near the reply's full 600ms delay is only possible via the
/// notification arm, so this fails if the dedup/fast-path is removed even
/// though the slower RPC-reply fallback would still, eventually, produce
/// exactly one `Done` on its own — the failure mode the brief calls "passing
/// for the wrong reason".
#[tokio::test]
async fn both_signals_arriving_produce_exactly_one_done() {
    let (controls, steer, _token) = controls();
    let stream = comet_harness::acp::session::run(
        open(false).await,
        HarnessId::Mock,
        request("please complete-both"),
        controls,
        no_usage,
        Some(settle_signal()),
    );
    // Times each `Done` from when it is OBSERVED, not from stream close:
    // `drain`'s end-to-end time also includes `shutdown_child` reaping the
    // fixture process afterward (on Windows, a no-op SIGTERM means that reap
    // waits out its own grace period regardless of how fast the turn
    // settled), which would swamp the budget below on teardown alone and
    // prove nothing about which signal ended the turn.
    let started = tokio::time::Instant::now();
    let mut events = Vec::new();
    let mut done_at = Vec::new();
    let mut stream = stream;
    let mut steer = Some(steer);
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = stream.next().await {
            let event = event.expect("no transport error");
            if matches!(event, AgentEvent::Done { .. }) {
                done_at.push(started.elapsed());
                // After the FIRST turn settles, queue a second one whose
                // fixture mode (`replay-stale`) replays turn 1's already-
                // consumed promptId as a bogus early completion of turn 2 —
                // the cross-turn staleness the consumed-id dedup exists to
                // reject. Sent here, not before the loop starts, so it lands
                // once the session is actually between turns. Taken (not
                // cloned) so the mailbox closes right after — one steer only.
                if let Some(steer) = steer.take() {
                    steer
                        .send(SteerMessage {
                            prompt: "second replay-stale".into(),
                            message_id: None,
                        })
                        .await
                        .expect("queue the second turn");
                }
            }
            events.push(event);
        }
    })
    .await
    .expect("the run ends rather than hanging");

    assert_eq!(
        done_at.len(),
        2,
        "two completion signals on turn 1, plus a stale replay racing turn 2, \
         must still produce exactly one Done per turn: {events:#?}"
    );

    // Turn 1: settled off the notification, bounded by
    // `POST_NOTIFICATION_REPLY_BOUND` (250ms) rather than by the fixture's
    // 600ms reply delay. Measured 254-266ms across five runs on this
    // machine (2026-08-29, the harvest bound landing in this same fix wave);
    // 500ms gives ample margin above the bound for scheduler jitter while
    // staying well under the 600ms delay it must not be waiting on.
    assert!(
        done_at[0] < Duration::from_millis(500),
        "turn 1 Done arrived at {:?} — the fixture's RPC reply is delayed \
         600ms and POST_NOTIFICATION_REPLY_BOUND is 250ms, so anything this \
         close to the reply's delay means the notification arm (plus its \
         bounded harvest) did not settle the turn and the slow fallback did \
         instead",
        done_at[0]
    );

    // Turn 2: `fake_acp` is single-threaded and blocks in `complete-both`'s
    // own 600ms sleep before it can even READ turn 2's `session/prompt`, so
    // the earliest the stale replay can reach the client is ~600ms after
    // turn 1's prompt was sent (not after turn 1's Done, which now arrives
    // earlier, bound-limited at ~250ms). If the dedup is missing, that
    // stale replay settles turn 2 immediately on arrival — measured
    // turn2_gap ~601-602ms across three runs (dedup temporarily disabled to
    // measure this, then restored; falsifying only the dedup, same
    // procedure `a test you have not seen fail is not evidence` requires).
    // If the dedup is present, it is ignored and turn 2 waits for its OWN
    // real completion instead — one more 200ms sleep past the stale
    // replay's arrival, plus its own 250ms harvest bound (turn 2's real
    // completion never gets an RPC reply either) — measured turn2_gap
    // ~792-808ms across five runs. 700ms sits between the two clusters with
    // ~90-100ms margin either side, so it is the assertion that actually
    // discriminates a missing dedup from a present one; a lower threshold
    // would pass in BOTH cases and prove nothing — the OLD 300ms threshold,
    // measured against the OLD 200ms fixture delay and the pre-harvest
    // near-zero turn-1 latency, no longer discriminates now that both
    // clusters shifted upward by the harvest bound plus the larger fixture
    // delay it required. The text_of/status assertions below are
    // corroborating, not load-bearing on their own for THIS distinction —
    // falsifying just the dedup (leaving the notification arm and its
    // method/session match intact) settles turn 2 as Errored off the stale
    // "refusal" before "second" ever streams, which is what they catch;
    // this gap assertion is what catches it even if that stale content
    // happened to look otherwise plausible.
    let turn2_gap = done_at[1] - done_at[0];
    assert!(
        turn2_gap > Duration::from_millis(700),
        "turn 2 settled only {turn2_gap:?} after turn 1 — the replayed, \
         already-consumed promptId from turn 1 must not be read as \
         completing turn 2; this fails when the consumed-id dedup is \
         removed even though the notification-recognition arm is untouched"
    );

    assert_eq!(
        text_of(&events),
        "workingworkingsecond",
        "both turns stream their generic \"working\" chunk; the stale replay \
         must never surface as content of its own, and turn 2's real \
         \"second\" chunk must still stream: {events:#?}"
    );
    for (i, event) in dones(&events).into_iter().enumerate() {
        match event {
            AgentEvent::Done { status, error, .. } => {
                assert_eq!(
                    *status,
                    DoneStatus::Completed,
                    "turn {i}: the stale replay's stopReason is \"refusal\" — a \
                     Completed status here proves it was rejected: {events:#?}"
                );
                assert!(
                    error.is_none(),
                    "a clean end_turn carries no error: {error:?}"
                );
            }
            other => unreachable!("{other:?}"),
        }
    }
}

/// D114: the completion notification's session-match guard
/// (`acp/session.rs`'s `SettleSignal` arm) is checked BEFORE the
/// promptId staleness check, and is real, load-bearing logic in its own
/// right — a foreign `sessionId` naming a different session is not evidence
/// about this turn, whatever its promptId says. Nothing before this test
/// drove that comparison down a mismatching path; `both_signals_arriving_
/// produce_exactly_one_done` above exercises the promptId dedup with a
/// SESSION-matching replay, which is a different half of the same guard.
#[tokio::test]
async fn a_foreign_session_completion_notification_is_not_evidence_about_this_turn() {
    let (controls, steer, _token) = controls();
    let stream = comet_harness::acp::session::run(
        open(false).await,
        HarnessId::Mock,
        // The fixture's `complete-foreign-session`: an immediate completion
        // notification naming a DIFFERENT session, with `stopReason:
        // "refusal"` and no promptId — nothing for the promptId dedup to
        // catch on its own — followed after a delay by this turn's own real
        // content and its own correctly-scoped completion.
        request("please complete-foreign-session"),
        controls,
        no_usage,
        Some(settle_signal()),
    );
    drop(steer);
    let events = drain(stream).await;

    assert_eq!(
        text_of(&events),
        "workingsecond",
        "the foreign notification must never settle this turn — \"second\" \
         (streamed only after the real, correctly-scoped completion fires) \
         must still appear: {events:#?}"
    );
    match only_done(&events) {
        AgentEvent::Done { status, error, .. } => {
            assert_eq!(
                *status,
                DoneStatus::Completed,
                "the foreign notification's stopReason is \"refusal\" \
                 (DoneStatus::Errored) — a Completed status here proves it \
                 was rejected on the sessionId mismatch rather than settling \
                 this turn: {events:#?}"
            );
            assert!(
                error.is_none(),
                "a clean end_turn carries no error: {error:?}"
            );
        }
        other => unreachable!("{other:?}"),
    }
}

/// Grok's own reader, mirrored here rather than reused: `grok::usage` is
/// `pub(crate)` inside `comet-harness` and unreachable from this integration-
/// test crate (`no_usage`'s own doc comment above explains why every other
/// test in this file passes that instead). Same field names, same shape —
/// `_meta.inputTokens`/`outputTokens`, `None` when the reply carries neither.
fn test_usage(result: &serde_json::Value, context_window: Option<u64>) -> Option<AgentEvent> {
    let meta = &result["_meta"];
    let prompt_tokens = meta["inputTokens"].as_u64();
    let output_tokens = meta["outputTokens"].as_u64();
    if prompt_tokens.is_none() && output_tokens.is_none() {
        return None;
    }
    Some(AgentEvent::Usage {
        prompt_tokens: prompt_tokens.unwrap_or(0),
        output_tokens: output_tokens.unwrap_or(0),
        context_window,
    })
}

/// **Finding 1, pinned.** Grok's usage numbers live ONLY in the `session/
/// prompt` RPC reply's `_meta` (this suite's `test_usage`, mirroring
/// `grok::usage`) — never in [`comet_harness::acp::session`]'s completion
/// notification, which settles first on every healthy turn (`session.rs`'s
/// own measurement: the notification consistently beats the reply by about
/// 3ms). Before the fix this notification-settle arm returned the instant it
/// saw the notification, dropping the still in-flight reply future — and
/// with it, Grok's only copy of the turn's usage — without ever reading it.
///
/// `complete-both-usage`'s reply lands a few ms after its notification,
/// comfortably inside `POST_NOTIFICATION_REPLY_BOUND`'s 30ms, so a working
/// harvest recovers it well within the bound; removing the harvest (leaving
/// only `usage_reader(&params, ...)` on the notification's own empty
/// `params`) must fail this — the notification never carries usage at all.
#[tokio::test]
async fn a_notification_settled_turn_still_reports_usage_from_the_reply() {
    let (controls, steer, _token) = controls();
    let stream = comet_harness::acp::session::run(
        open(false).await,
        HarnessId::Mock,
        request("please complete-both-usage"),
        controls,
        test_usage,
        Some(settle_signal()),
    );
    drop(steer);
    let events = drain(stream).await;

    let usage: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::Usage { .. }))
        .collect();
    assert_eq!(
        usage.len(),
        1,
        "the reply's usage must reach the stream exactly once: {events:#?}"
    );
    match usage[0] {
        AgentEvent::Usage {
            prompt_tokens,
            output_tokens,
            ..
        } => {
            assert_eq!(*prompt_tokens, 111, "the fixture's own inputTokens");
            assert_eq!(*output_tokens, 22, "the fixture's own outputTokens");
        }
        other => unreachable!("{other:?}"),
    }
    match only_done(&events) {
        AgentEvent::Done { status, error, .. } => {
            assert_eq!(*status, DoneStatus::Completed);
            assert!(error.is_none());
        }
        other => unreachable!("{other:?}"),
    }
}

/// **D121, pinned.** The notification-settle arm's two existing drains
/// (`drain_buffered` immediately on receiving the notification, then again
/// on the RPC-reply arm) only cover what was buffered BEFORE each of their
/// own settle signals. Neither covers the window this test poses: the
/// fixture's `complete-race-drain` mode pushes a THIRD frame from the reader
/// task strictly between the notification and the reply — i.e. while the
/// harvest is still bounded-polling `reply`, not before it.
///
/// Steered to a plain second turn so a misattributed frame has somewhere
/// else to land: without the post-harvest drain this fix adds, "raced-in"
/// is never drained on turn 1 at all — it sits in `incoming` untouched until
/// turn 2's own PRE-send `drain_buffered` (the "drain any backlog before
/// sending" comment at the top of `drive_turn`) picks it up as leftover
/// backlog, so it turns up glued onto turn 2's text instead of turn 1's.
#[tokio::test]
async fn a_frame_racing_the_post_notification_harvest_lands_on_its_own_turn() {
    let (controls, steer, _token) = controls();
    let mut stream = comet_harness::acp::session::run(
        open(true).await,
        HarnessId::Mock,
        request("please complete-race-drain"),
        controls,
        no_usage,
        Some(settle_signal()),
    );

    let mut events = Vec::new();
    let mut steer = Some(steer);
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = stream.next().await {
            let event = event.expect("no transport error");
            let settled = matches!(event, AgentEvent::Done { .. });
            events.push(event);
            if settled && let Some(steer) = steer.take() {
                steer
                    .send(SteerMessage {
                        prompt: "second".into(),
                        message_id: None,
                    })
                    .await
                    .expect("queue the second turn");
            }
        }
    })
    .await
    .expect("both turns settle rather than hanging");

    let dones = dones(&events);
    assert_eq!(dones.len(), 2, "one Done per turn: {events:#?}");
    let first_done_idx = events
        .iter()
        .position(|e| matches!(e, AgentEvent::Done { .. }))
        .expect("turn 1's Done is in the stream");

    let turn1_text = text_of(&events[..=first_done_idx]);
    let turn2_text = text_of(&events[first_done_idx + 1..]);
    assert_eq!(
        turn1_text, "workingraced-in",
        "the frame that raced the harvest belongs to turn 1, where it was \
         sent — without the post-harvest drain it is missing here entirely: \
         {events:#?}"
    );
    assert_eq!(
        turn2_text, "working done",
        "turn 2 must be clean — a missing post-harvest drain on turn 1 \
         leaves \"raced-in\" for turn 2's own pre-send drain to pick up \
         instead, which would prefix it here: {events:#?}"
    );

    for done in dones {
        match done {
            AgentEvent::Done { status, error, .. } => {
                assert_eq!(*status, DoneStatus::Completed);
                assert!(error.is_none(), "a clean end_turn carries no error");
            }
            other => unreachable!("{other:?}"),
        }
    }
}

/// **Task 5's stall bound, made real.** `silent-after-prompt` poses total wire
/// silence after `session/prompt` — no update, no reply, nothing — which is
/// exactly the shape `Timeouts::prompt_stall` exists to bound. The run must
/// end in an errored `Done` inside the bound, and the message must name an
/// action rather than any protocol detail
/// (`.agents/rules/user-facing-errors.md`).
#[tokio::test]
async fn a_stalled_prompt_ends_in_a_bounded_error() {
    // Short enough that a regression fails the suite in well under a second,
    // long enough that it cannot be confused with scheduling noise.
    const SHORT_STALL: Timeouts = Timeouts {
        prompt_stall: Duration::from_millis(200),
        ..TEST_TIMEOUTS
    };
    let (controls, steer, _token) = controls();
    let stream = comet_harness::acp::session::run(
        open_with(false, SHORT_STALL).await,
        HarnessId::Mock,
        request("please silent-after-prompt"),
        controls,
        no_usage,
        Some(settle_signal()),
    );
    drop(steer);

    let events = tokio::time::timeout(Duration::from_secs(2), async { drain(stream).await })
        .await
        .expect("the stall bound ends the run inside 2s, well past its own 200ms window");

    match only_done(&events) {
        AgentEvent::Done { status, error, .. } => {
            assert_eq!(*status, DoneStatus::Errored);
            let error = error.as_deref().expect("a stall names itself");
            assert!(
                !error.contains("session/prompt")
                    && !error.contains("_x.ai")
                    && !error.contains("jsonrpc"),
                "no raw protocol text on screen: {error}"
            );
            assert!(
                error.to_lowercase().contains("restart"),
                "the message must name an action, not just report a state: {error}"
            );
        }
        other => unreachable!("{other:?}"),
    }
}

/// Break caught: an agent that dies mid-turn reading as a quiet success. The
/// `Done` has to say it errored AND carry something that explains why.
#[tokio::test]
async fn an_agent_that_exits_mid_turn_errors_with_an_explanation() {
    let (controls, steer, _token) = controls();
    let stream = comet_harness::acp::session::run(
        open(false).await,
        HarnessId::Mock,
        request("please exit-now"),
        controls,
        no_usage,
        Some(settle_signal()),
    );
    drop(steer);
    let events = drain(stream).await;

    match only_done(&events) {
        AgentEvent::Done { status, error, .. } => {
            assert_eq!(*status, DoneStatus::Errored);
            let error = error.as_deref().expect("a crash names itself");
            assert!(
                error.contains("exited unexpectedly"),
                "unexplained crash: {error}"
            );
        }
        other => unreachable!("{other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Approvals: `session/request_permission`, via `fake-acp`'s
// `request-permission[-unrecognized]` modes.
// ---------------------------------------------------------------------------

/// The host mints the request id and owns the lifecycle. An adapter that
/// emitted its own `ApprovalRequested` event would put a card in the doc under
/// an id no resolver knows, and answering it would never unblock the run —
/// which is why the harness calls `controls.request_approval` rather than
/// sending an event. Pinned against the literal frame the agent really sent.
#[tokio::test]
async fn a_permission_request_reaches_the_approval_bridge() {
    let received: Arc<Mutex<Option<ApprovalRequest>>> = Arc::new(Mutex::new(None));
    let received_for_bridge = received.clone();
    let (controls, steer, _token) = controls_with_approval(move |request| {
        *received_for_bridge.lock().unwrap() = Some(request);
        let (tx, rx) = oneshot::channel();
        let _ = tx.send(ApprovalDecision::Allow);
        rx
    });
    let stream = comet_harness::acp::session::run(
        open(false).await,
        HarnessId::Mock,
        request("please request-permission"),
        controls,
        no_usage,
        Some(settle_signal()),
    );
    drop(steer);
    let events = drain(stream).await;

    assert_eq!(
        *received.lock().unwrap(),
        Some(ApprovalRequest::Command {
            command: "rm -rf /tmp/x".into(),
            cwd: None,
        }),
        "the literal request the wire sent must reach the bridge unchanged"
    );
    assert!(
        text_of(&events).contains("chosen:opt-allow-once"),
        "the agent must be told which option was picked: {:?}",
        text_of(&events)
    );
    match only_done(&events) {
        AgentEvent::Done { status, .. } => assert_eq!(*status, DoneStatus::Completed),
        other => unreachable!("{other:?}"),
    }
}

/// Break caught: mapping `Deny` to the wrong option kind. `reject_once` and
/// `reject_always` are different answers and picking the wrong one silently
/// changes a one-time denial into a session-wide one.
#[tokio::test]
async fn each_decision_selects_the_matching_option_kind() {
    async fn run_with(decision: ApprovalDecision) -> Vec<AgentEvent> {
        let (controls, steer, _token) = controls_with_approval(move |_request| {
            let (tx, rx) = oneshot::channel();
            let _ = tx.send(decision.clone());
            rx
        });
        let stream = comet_harness::acp::session::run(
            open(false).await,
            HarnessId::Mock,
            request("please request-permission"),
            controls,
            no_usage,
            Some(settle_signal()),
        );
        drop(steer);
        drain(stream).await
    }

    let allow = run_with(ApprovalDecision::Allow).await;
    assert!(
        text_of(&allow).contains("chosen:opt-allow-once"),
        "Allow must select allow_once: {:?}",
        text_of(&allow)
    );

    let allow_for_session = run_with(ApprovalDecision::AllowForSession).await;
    assert!(
        text_of(&allow_for_session).contains("chosen:opt-allow-always"),
        "AllowForSession must select allow_always: {:?}",
        text_of(&allow_for_session)
    );

    let deny = run_with(ApprovalDecision::Deny {
        message: "no".into(),
    })
    .await;
    assert!(
        text_of(&deny).contains("chosen:opt-reject-once"),
        "Deny must select reject_once: {:?}",
        text_of(&deny)
    );
    assert!(
        !text_of(&deny).contains("opt-reject-always"),
        "Deny must never select reject_always — a one-time denial is not a \
         session-wide one: {:?}",
        text_of(&deny)
    );
}

/// **The bug a review found in this task, posed end to end.** Hermes' real
/// edit-approval shape (`fake-acp`'s `request-permission-edit` mode) offers
/// only `allow_once`/`reject_once` — no `allow_always` at all. Before the
/// fix, `AllowForSession` against this shape answered the agent `cancelled`
/// — denying the edit on the wire — while a cardless test could not see that
/// the engine's own `request_approval` bridge had already recorded "Allowed
/// for this session" for it. This drives the real production path (not just
/// `approval::outcome_for` in isolation) against the real fixture shape, so
/// a regression here fails at the same layer the four brief-mandated tests
/// do.
#[tokio::test]
async fn allow_for_session_narrows_to_allow_once_on_hermes_real_edit_shape() {
    let (controls, steer, _token) = controls_with_approval(|_request| {
        let (tx, rx) = oneshot::channel();
        let _ = tx.send(ApprovalDecision::AllowForSession);
        rx
    });
    let stream = comet_harness::acp::session::run(
        open(false).await,
        HarnessId::Mock,
        request("please request-permission-edit"),
        controls,
        no_usage,
        Some(settle_signal()),
    );
    drop(steer);
    let events = drain(stream).await;

    assert!(
        text_of(&events).contains("chosen:opt-allow-once"),
        "AllowForSession must narrow to allow_once when allow_always is not \
         offered, never cancel and read as a denial: {:?}",
        text_of(&events)
    );
}

/// `ApprovalDecision::Expired` is host-stamped when a run ends with an approval
/// still pending, and is never a decision a client may send. The agent must be
/// answered `cancelled`, not left waiting on a dead channel.
#[tokio::test]
async fn an_expired_approval_cancels_the_agents_request() {
    let (controls, steer, _token) = controls_with_approval(|_request| {
        // The resolver is dropped without ever sending — the run ended (or
        // simply never answered) with this approval still outstanding.
        let (tx, rx) = oneshot::channel::<ApprovalDecision>();
        drop(tx);
        rx
    });
    let stream = comet_harness::acp::session::run(
        open(false).await,
        HarnessId::Mock,
        request("please request-permission"),
        controls,
        no_usage,
        Some(settle_signal()),
    );
    drop(steer);
    let events = drain(stream).await;

    assert!(
        text_of(&events).contains("chosen:cancelled"),
        "a dropped resolver must answer the agent `cancelled` rather than \
         hang or invent an approval: {:?}",
        text_of(&events)
    );
    match only_done(&events) {
        AgentEvent::Done { status, .. } => assert_eq!(*status, DoneStatus::Completed),
        other => unreachable!("{other:?}"),
    }
}

/// An option set that carries none of the four expected kinds must not silently
/// pick the first option. It is a protocol-drift Diagnostic and a denial —
/// guessing here is the difference between asking and not asking.
#[tokio::test]
async fn an_unrecognized_option_set_denies_and_reports_drift() {
    let called = Arc::new(Mutex::new(false));
    let called_from_bridge = called.clone();
    let (controls, steer, _token) = controls_with_approval(move |_request| {
        *called_from_bridge.lock().unwrap() = true;
        let (tx, rx) = oneshot::channel();
        let _ = tx.send(ApprovalDecision::Allow);
        rx
    });
    let stream = comet_harness::acp::session::run(
        open(false).await,
        HarnessId::Mock,
        request("please request-permission-unrecognized"),
        controls,
        no_usage,
        Some(settle_signal()),
    );
    drop(steer);
    let events = drain(stream).await;

    assert!(
        !*called.lock().unwrap(),
        "an options vocabulary with none of the four recognized kinds must \
         never reach the user — guessing which of the four a vendor's own \
         kind names is not asking"
    );
    assert!(
        text_of(&events).contains("chosen:cancelled"),
        "the agent must be answered cancelled, not left hanging: {:?}",
        text_of(&events)
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::Diagnostic { discriminator, severity, .. }
                if discriminator == "session/request_permission"
                    && *severity == DiagnosticSeverity::Unknown
        )),
        "protocol drift must be reported, not silently swallowed: {events:#?}"
    );
}

/// The drift diagnostic is rate-limited per session, the same way
/// `normalize::session_update_once` rate-limits an unrecognized
/// `session/update` kind (both share the same `diagnostics` set) — a vendor
/// that drifts on every turn must report once per session, not once per
/// request.
#[tokio::test]
async fn repeated_protocol_drift_reports_only_once_per_session() {
    let (controls, steer, _token) = controls_with_approval(|_request| {
        let (tx, rx) = oneshot::channel();
        let _ = tx.send(ApprovalDecision::Allow);
        rx
    });
    let stream = comet_harness::acp::session::run(
        open(false).await,
        HarnessId::Mock,
        request("please request-permission-unrecognized"),
        controls,
        no_usage,
        Some(settle_signal()),
    );
    steer
        .send(SteerMessage {
            prompt: "please request-permission-unrecognized".into(),
            message_id: None,
        })
        .await
        .expect("queue a second drifting turn");
    drop(steer);
    let events = drain(stream).await;

    let diagnostics = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                AgentEvent::Diagnostic { discriminator, .. }
                    if discriminator == "session/request_permission"
            )
        })
        .count();
    assert_eq!(
        diagnostics, 1,
        "a vendor that drifts every turn must report once per session, not \
         once per request: {events:#?}"
    );
}

/// **The wiring line itself, not just a mapper function called in
/// isolation.** `grok.rs`/`hermes.rs` each unit-test their own
/// `map_open_failure` directly, but nothing before this test exercised
/// `AcpSession::open`'s own plumbing -- the line that actually calls the
/// mapper on a real `session/new` failure and returns its result instead of
/// the fallback. Reverting that line to `return Err(fallback)` (ignoring
/// `map_open_failure` entirely) leaves every other test in this suite
/// green, because none of them drives `session/new` to fail at all -- this
/// is the one that would catch it.
///
/// The fixture's `needs-setup` `cwd` trigger (`fake_acp.rs`) answers with
/// Grok's real captured signed-out shape (`grok::map_open_failure`'s own
/// doc comment), so this drives the exact wire text a real signed-out Grok
/// sends, through a real child process, not a hand-built `RpcFailure`.
#[tokio::test]
async fn a_session_new_failure_reaches_the_caller_through_the_open_failure_mapper() {
    let command = tokio::process::Command::new(env!("CARGO_BIN_EXE_fake-acp"));
    let request = RunRequest {
        cwd: "needs-setup".into(),
        ..RunRequest::for_session(RuntimeMode::default())
    };
    let error = AcpSession::open(
        command,
        "needs-setup",
        TEST_TIMEOUTS,
        &request,
        no_config,
        // A minimal mapper matching Grok's real shape -- deliberately not
        // `grok::map_open_failure` itself (that function is `pub(crate)`
        // inside `comet-harness` and unreachable from this integration-test
        // crate), but the same recognition rule, so this test still proves
        // the plumbing carries a REAL match through to the caller.
        |failure| {
            if failure.message.contains("Authentication required") {
                Some(comet_harness::HarnessError::NeedsSetup {
                    summary: "Sign-in required".into(),
                    hint: "Run `grok login` to sign in, then try again.".into(),
                })
            } else {
                None
            }
        },
    )
    .await;

    let error = match error {
        Ok(_) => panic!("a needs-setup session/new must fail, not open a session"),
        Err(error) => error,
    };

    match error {
        comet_harness::HarnessError::NeedsSetup { summary, hint } => {
            assert_eq!(summary, "Sign-in required");
            assert!(hint.contains("grok"), "{hint}");
        }
        other => panic!(
            "expected the mapper's NeedsSetup, got the raw fallback instead \
             (the wiring line did not call the mapper): {other:?}"
        ),
    }
}

/// Break caught (D119): the `config_requests` loop returned
/// `client.request`'s error unchanged, so the provider's own JSON-RPC message
/// travelled to `drive_run`'s sink and onto the screen —
/// `.agents/rules/user-facing-errors.md` RULE 1. Grok's real text for this
/// case is `Invalid params: unknown model id`, which the fixture answers with,
/// so the assertion below is against the exact string that used to escape.
///
/// The `session/new` half of this file's mapper test is deliberately NOT the
/// same path: that one recognizes a signed-out agent before a session exists;
/// this one opens a session successfully and is refused a setting afterwards.
#[tokio::test]
async fn a_refused_session_setting_reports_copy_rather_than_the_agent_s_own_words() {
    let command = tokio::process::Command::new(env!("CARGO_BIN_EXE_fake-acp"));
    let request = RunRequest {
        cwd: "refuse-set-model".into(),
        model: Some("no-such-model".into()),
        ..RunRequest::for_session(RuntimeMode::default())
    };
    let error = AcpSession::open(
        command,
        "refuse-set-model",
        TEST_TIMEOUTS,
        &request,
        |_, session_id| {
            vec![(
                "session/set_model",
                serde_json::json!({"sessionId": session_id, "modelId": "no-such-model"}),
            )]
        },
        |_| None,
    )
    .await;

    let error = match error {
        Ok(_) => panic!("a refused session/set_model must fail the open, not proceed silently"),
        Err(error) => error,
    };

    match &error {
        comet_harness::HarnessError::SettingRefused { summary, hint } => {
            assert_eq!(summary, "Model refused");
            assert!(
                hint.contains("Pick a different model"),
                "the hint must name the action: {hint}"
            );
        }
        other => panic!("expected SettingRefused, got {other:?}"),
    }

    // The rule, asserted directly: nothing the agent wrote, and no wire
    // vocabulary, reaches the string `drive_run` renders.
    let shown = error.to_string();
    for leaked in [
        "Invalid params",
        "unknown model id",
        "session/set_model",
        "harness",
        "-32602",
    ] {
        assert!(
            !shown.contains(leaked),
            "{leaked:?} must not reach the user: {shown}"
        );
    }
}
