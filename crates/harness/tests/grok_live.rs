//! One real turn against the installed Grok CLI.
//!
//! **`#[ignore]`d: it spends tokens and needs an authenticated `grok`.** The
//! rest of the suite proves the decode against fixtures; this proves the decode
//! was pointed at the right thing, which no fixture can.
//!
//! ```text
//! cargo test -p comet-harness --test grok_live -- --ignored --nocapture
//! ```
//!
//! `GROK_EXECUTABLE` overrides resolution — needed on a machine where the CLI
//! was installed after the shell started, since its PATH entry is not yet live.

use std::time::Duration;

use comet_harness::acp::grok::GrokHarness;
use comet_harness::{CancellationToken, Harness, RunControls};
use comet_proto::{AgentEvent, DoneStatus, RunRequest, RuntimeMode};
use futures::StreamExt;
use tokio::sync::{mpsc, oneshot};

/// Deliberately trivial and tool-free: this test exists to watch the turn
/// machinery, and every token it spends buys output nobody keeps.
const PROMPT: &str = "Reply with exactly: OK. Do not use any tools.";

#[tokio::test]
#[ignore = "spends tokens against the real Grok CLI; run with --ignored"]
async fn a_real_turn_streams_and_settles() {
    let cwd = std::env::temp_dir().join("comet-grok-live");
    std::fs::create_dir_all(&cwd).expect("disposable cwd");

    let harness = GrokHarness::new();
    let (_steer_tx, steer_rx) = mpsc::channel(1);
    let controls = RunControls {
        request_input: Box::new(|_| oneshot::channel().1),
        request_approval: Box::new(|_| oneshot::channel().1),
        steering: steer_rx,
        interrupt: CancellationToken::new(),
    };

    let request = RunRequest {
        prompt: PROMPT.into(),
        cwd: cwd.to_string_lossy().into_owned(),
        ..RunRequest::for_session(RuntimeMode::default())
    };

    let mut stream = harness
        .run(request, controls)
        .await
        .expect("the agent starts");

    // **Read to the first `Done`, not to stream end.** The session is
    // persistent: once a turn settles it parks on the steering mailbox and the
    // stream stays open, which is correct and is not what this test measures.
    // Collecting to the end hung for the full timeout and read as a harness bug
    // when it was this test holding the steer sender alive.
    //
    // Generous: a real model call on a VM with no GPU. Short enough that a turn
    // which never settles fails instead of hanging the suite.
    let mut events: Vec<AgentEvent> = Vec::new();
    tokio::time::timeout(Duration::from_secs(180), async {
        while let Some(event) = stream.next().await {
            let event = event.expect("no transport error");
            match &event {
                AgentEvent::TextDelta { text } => print!("{text}"),
                AgentEvent::Done { status, error, .. } => {
                    println!("\n[done: {status:?} {error:?}]")
                }
                _ => {}
            }
            let settled = matches!(event, AgentEvent::Done { .. });
            events.push(event);
            if settled {
                break;
            }
        }
    })
    .await
    .expect("the turn settles rather than hanging");

    let text: String = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        text.contains("OK"),
        "the agent's answer reached the stream: {text:?}"
    );

    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::ReasoningDelta { .. })),
        "grok-4.6 is a reasoning model and its thought chunks must map to \
         ReasoningDelta, not to visible text"
    );

    // **The user's own prompt must not come back as assistant text.** Grok
    // echoes it as a `user_message_chunk` on the same stream; mapping that to
    // `TextDelta` would print the question into the answer.
    assert!(
        !text.contains("Do not use any tools"),
        "the echoed user_message_chunk leaked into the transcript: {text:?}"
    );

    let dones: Vec<&AgentEvent> = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::Done { .. }))
        .collect();
    assert_eq!(dones.len(), 1, "exactly one Done: {events:#?}");
    match dones[0] {
        AgentEvent::Done { status, error, .. } => {
            assert_eq!(*status, DoneStatus::Completed);
            assert!(error.is_none(), "a clean turn carries no error: {error:?}");
        }
        other => unreachable!("{other:?}"),
    }
}
