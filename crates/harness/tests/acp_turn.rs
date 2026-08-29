//! The ACP core driven end to end against the `fake-acp` fixture.
//!
//! `acp_fixture.rs` proves the fixture speaks the protocol; this proves Comet's
//! side of it. The split matters — a hardening test written against an
//! unverified fixture passes for the wrong reason.

use std::time::Duration;

use comet_harness::acp::session::{AcpSession, Timeouts};
use comet_harness::{CancellationToken, HarnessError, RunControls, SteerMessage};
use comet_proto::{AgentEvent, DoneStatus, HarnessId, RunRequest, RuntimeMode};
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
    AcpSession::open(command, ".", timeouts)
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
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Steered { .. })),
        "the steer is announced: {events:#?}"
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
/// reply by 200ms; settling well under that delay is only possible via the
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
    );
    // Times each `Done` from when it is OBSERVED, not from stream close:
    // `drain`'s end-to-end time also includes `shutdown_child` reaping the
    // fixture process afterward (on Windows, a no-op SIGTERM means that reap
    // waits out its own grace period regardless of how fast the turn
    // settled), which would swamp a 150ms budget on teardown alone and prove
    // nothing about which signal ended the turn.
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

    // Turn 1: settled off the fast notification, well under its RPC reply's
    // 200ms delay.
    assert!(
        done_at[0] < Duration::from_millis(150),
        "turn 1 Done arrived at {:?} — the fixture's RPC reply is delayed \
         200ms, so anything this close to that delay means the notification \
         arm did not settle the turn and the slow fallback did instead",
        done_at[0]
    );

    // Turn 2: the fixture replays turn 1's promptId as a bogus immediate
    // completion, THEN streams "second" and completes for real after a
    // 200ms delay. A settle anywhere near turn 1's Done (i.e. near-zero
    // additional wait) means the stale replay wrongly ended turn 2; only a
    // gap close to 200ms proves it was recognized as stale and ignored.
    let turn2_gap = done_at[1] - done_at[0];
    assert!(
        turn2_gap > Duration::from_millis(120),
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
