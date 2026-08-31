//! `HermesHarness` identity, capability, and provider-isolation contract.

use std::time::Duration;

use comet_harness::acp::hermes::HermesHarness;
use comet_harness::acp::session::Timeouts;
use comet_harness::{CancellationToken, Harness, RunControls};
use comet_proto::{AgentEvent, HarnessId, RunRequest, RuntimeMode, SteeringMode};
use futures::StreamExt;
use tokio::sync::{mpsc, oneshot};

/// The registry's lazy descriptor names `HermesHarness::capabilities()`, so the
/// catalog entry shown before first use must equal what the trait reports after
/// the slot resolves. A drift here shows as a picker row that changes when the
/// user clicks it.
#[test]
fn identity_and_capabilities_do_not_drift() {
    let harness = HermesHarness::new();
    assert_eq!(harness.id(), HarnessId::Hermes);
    assert_eq!(harness.display_name(), "Hermes");
    assert_eq!(harness.capabilities(), HermesHarness::capabilities());
}

/// Break caught: declaring an effort ladder Hermes does not offer. An empty
/// ladder is the honest answer and the traits picker is built to render it;
/// a populated one puts choices on screen that the run silently discards.
#[test]
fn the_effort_ladder_is_empty_because_hermes_has_none() {
    assert!(
        HermesHarness::capabilities().reasoning_levels.is_empty(),
        "Hermes advertises no effort config; a ladder here is a promise the run breaks"
    );
}

/// Break caught: reading an absent `_meta.steering` as StepBoundary. Hermes
/// sends no steering extension, so a steer must be delivered as the next prompt
/// on the same session. Declaring StepBoundary loses the steer silently.
#[test]
fn steering_falls_back_to_the_turn_boundary() {
    let caps = HermesHarness::capabilities();
    assert!(caps.supports_steering);
    assert_eq!(caps.steering_mode, SteeringMode::TurnBoundary);
}

/// Break caught: installing Grok's vendor observer in the shared ACP path,
/// which would make Hermes interpret `_x.ai/session_notification` frames it
/// never advertised as its own subagents.
#[tokio::test]
async fn grok_subagent_extensions_remain_inert_for_hermes() {
    let harness = HermesHarness::new()
        .with_executable(env!("CARGO_BIN_EXE_fake-acp"))
        .with_timeouts(Timeouts {
            handshake: Duration::from_secs(10),
            cancel_grace: Duration::from_millis(750),
            kill_grace: Duration::from_millis(250),
            prompt_stall: Duration::from_secs(10),
        });
    let (steer_tx, steer_rx) = mpsc::channel(1);
    let controls = RunControls {
        request_input: Box::new(|_| oneshot::channel().1),
        request_approval: Box::new(|_| oneshot::channel().1),
        steering: steer_rx,
        interrupt: CancellationToken::new(),
    };
    let request = RunRequest {
        prompt: "grok-subagent-late".into(),
        cwd: std::env::temp_dir().to_string_lossy().into_owned(),
        ..RunRequest::for_session(RuntimeMode::default())
    };
    let stream = harness
        .run(request, controls)
        .await
        .expect("fixture starts");
    drop(steer_tx);
    let events: Vec<AgentEvent> = tokio::time::timeout(
        Duration::from_secs(20),
        stream
            .map(|event| event.expect("no transport error"))
            .collect(),
    )
    .await
    .expect("the parent turn settles");

    assert!(
        !events.iter().any(|event| matches!(
            event,
            AgentEvent::SubagentStarted { .. } | AgentEvent::SubagentUpdated { .. }
        )),
        "Grok vendor lifecycle must remain invisible to Hermes: {events:#?}"
    );
}
