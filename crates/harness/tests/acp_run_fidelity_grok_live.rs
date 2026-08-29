//! PR7's Step 8 live check, Grok half: a real image and a real resume.
//!
//! **`#[ignore]`d: it spends tokens and needs an authenticated `grok`.**
//! `acp_run_fidelity.rs` proves the wiring against `fake-acp`; this proves it
//! was pointed at the right thing against the real CLI, which no fixture can
//! — neither can be proven by a fixture (task brief, Step 8).
//!
//! ```text
//! cargo test -p comet-harness --test acp_run_fidelity_grok_live -- --ignored --nocapture
//! ```
//!
//! Hermes has no live counterpart: no LLM provider is configured on this
//! machine and `hermes model`/`hermes setup` refuse to run under a piped
//! subprocess (see `acp/hermes.rs`'s module doc) — that half is BLOCKED, not
//! run here.
//!
//! **BLOCKED on 2026-08-29 by the free quota exhaustion the task brief
//! warned about, confirmed with the exact wire evidence.** A raw JSON-RPC
//! probe of a real `session/prompt` (not via this harness) answered:
//! `{"error": {"code": -32003, "message": "Rate limited", "data": "API error
//! (status 429 Too Many Requests): subscription:free-usage-exhausted: You've
//! used all the included free usage for model grok-4.6 for now. Usage resets
//! over a rolling 24-hour window — tokens (actual/limit):
//! 526238/500000. ..."}}`. Both tests below ran and reached this same
//! rate limit rather than a real answer — `run_to_done` never sees a
//! `TextDelta`, only a `Done` with an empty transcript, because the turn
//! never produced one. **The model/effort wiring itself was still verified
//! live, separately from this file**: a raw JSON-RPC probe (token-free —
//! `session/new` and `session/set_model`, no `session/prompt`) confirmed
//! `session/set_model` validates a real model id
//! (`{"_meta":{"model":{"Ok":"grok-4.6"}}}`) and rejects an unknown one
//! (`-32602 Invalid params: unknown model id`) — see the task report for the
//! full transcript. What is NOT verified live is turn CONTENT: whether the
//! model actually describes an attached image, and whether a resumed session
//! actually recalls earlier context. Re-run this file once the rolling
//! window clears to close that gap; do not delete it in the meantime.

use std::time::Duration;

use comet_harness::acp::grok::GrokHarness;
use comet_harness::{CancellationToken, Harness, RunControls};
use comet_proto::{AgentEvent, ReasoningLevel, RunRequest, RuntimeMode};
use futures::StreamExt;
use tokio::sync::{mpsc, oneshot};

fn controls() -> RunControls {
    let (_steer_tx, steer_rx) = mpsc::channel(1);
    RunControls {
        request_input: Box::new(|_| oneshot::channel().1),
        request_approval: Box::new(|_| oneshot::channel().1),
        steering: steer_rx,
        interrupt: CancellationToken::new(),
    }
}

async fn run_to_done(harness: &GrokHarness, request: RunRequest) -> (String, String) {
    let mut stream = harness
        .run(request, controls())
        .await
        .expect("the agent starts");
    let mut text = String::new();
    let mut session_id = String::new();
    tokio::time::timeout(Duration::from_secs(180), async {
        while let Some(event) = stream.next().await {
            let event = event.expect("no transport error");
            match &event {
                AgentEvent::SessionStarted { session_id: id, .. } => session_id = id.clone(),
                AgentEvent::TextDelta { text: delta } => {
                    print!("{delta}");
                    text.push_str(delta);
                }
                AgentEvent::ReasoningDelta { text: delta } => {
                    print!("[reasoning]{delta}");
                }
                AgentEvent::Done { status, error, .. } => {
                    println!("\n[done: {status:?} {error:?}]")
                }
                other => println!("[other event] {other:?}"),
            }
            if matches!(event, AgentEvent::Done { .. }) {
                break;
            }
        }
    })
    .await
    .expect("the turn settles rather than hanging");
    (session_id, text)
}

/// A 1x1 transparent PNG — the smallest real, decodable image there is. Small
/// enough to cost almost nothing, real enough that a model actually
/// processing it (rather than hallucinating a description) is a fair test.
fn write_tiny_png() -> String {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=")
        .expect("valid base64");
    let path = std::env::temp_dir().join("comet-acp-run-fidelity-grok-live.png");
    std::fs::write(&path, bytes).expect("write tiny png");
    path.to_string_lossy().into_owned()
}

/// A real image reaches Grok as an image content block, not just a path ref
/// in the text. Grok's own `promptCapabilities.image` reads `false` on the
/// captured 2026-08-28 handshake (`grok.rs`'s doc comment) — so this ALSO
/// confirms that reading is still accurate on whatever version is installed
/// today: if Grok now advertises `true`, the image rides the wire and the
/// model can describe it; if it still advertises `false`, only the path ref
/// does, and the model has to reason from the filename alone.
#[tokio::test]
#[ignore = "spends tokens against the real Grok CLI; run with --ignored"]
async fn a_real_image_reaches_grok() {
    let cwd = std::env::temp_dir().join("comet-acp-run-fidelity-grok-live-image");
    std::fs::create_dir_all(&cwd).expect("disposable cwd");
    let image = write_tiny_png();

    let harness = GrokHarness::new();
    let request = RunRequest {
        prompt: "In at most six words, describe the attached image.".into(),
        attachments: vec![image],
        reasoning: Some(ReasoningLevel::Low),
        cwd: cwd.to_string_lossy().into_owned(),
        ..RunRequest::for_session(RuntimeMode::default())
    };

    let (_session_id, text) = run_to_done(&harness, request).await;
    assert!(
        !text.trim().is_empty(),
        "the model answered something about the attachment"
    );
}

/// A real resume: open a session, then reopen it by id and confirm the agent
/// still has the earlier context — the thing no fixture can prove, since
/// `fake-acp` never actually remembers anything across two spawned children.
#[tokio::test]
#[ignore = "spends tokens against the real Grok CLI; run with --ignored"]
async fn a_real_resume_keeps_context() {
    let cwd = std::env::temp_dir().join("comet-acp-run-fidelity-grok-live-resume");
    std::fs::create_dir_all(&cwd).expect("disposable cwd");

    let harness = GrokHarness::new();
    let first = RunRequest {
        prompt: "Remember the word BANANA. Reply with exactly: OK.".into(),
        reasoning: Some(ReasoningLevel::Low),
        cwd: cwd.to_string_lossy().into_owned(),
        ..RunRequest::for_session(RuntimeMode::default())
    };
    let (session_id, _) = run_to_done(&harness, first).await;
    assert!(!session_id.is_empty(), "the first run opened a session");

    let second = RunRequest {
        prompt: "What word did I ask you to remember? Reply with just the word.".into(),
        reasoning: Some(ReasoningLevel::Low),
        resume: Some(session_id.clone()),
        cwd: cwd.to_string_lossy().into_owned(),
        ..RunRequest::for_session(RuntimeMode::default())
    };
    let (resumed_session_id, text) = run_to_done(&harness, second).await;
    assert_eq!(
        resumed_session_id, session_id,
        "session/load must keep the resumed id, not mint a fresh one"
    );
    assert!(
        text.to_uppercase().contains("BANANA"),
        "the agent must still have the earlier turn's context: {text:?}"
    );
}
