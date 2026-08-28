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
const TEST_TIMEOUTS: Timeouts = Timeouts {
    handshake: Duration::from_secs(10),
    cancel_grace: Duration::from_millis(750),
    kill_grace: Duration::from_millis(250),
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
    let stream =
        comet_harness::acp::session::run(session, HarnessId::Mock, request("hello"), controls);
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
    let stream =
        comet_harness::acp::session::run(session, HarnessId::Mock, request("first"), controls);

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
    );
    token.cancel();

    let events = drain(stream).await;
    match only_done(&events) {
        AgentEvent::Done { status, .. } => assert_eq!(*status, DoneStatus::Interrupted),
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
