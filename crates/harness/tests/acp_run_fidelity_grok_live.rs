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

mod support;

use std::time::Duration;

use comet_harness::acp::grok::{GROK_ARGS, GrokHarness, resolve_grok_executable};
use comet_harness::acp::session::{AcpSession, Timeouts};
use comet_harness::{CancellationToken, Harness, RunControls};
use comet_proto::{AgentEvent, ReasoningLevel, RunRequest, RuntimeMode};
use support::ApprovalWatch;
use tokio::sync::{mpsc, oneshot};

/// Token-free: `initialize` + `session/new`, no `session/prompt`. Answers
/// whether THIS installed Grok build actually advertises
/// `promptCapabilities.image` right now — the assumption
/// `a_run_with_an_attachment_completes_when_grok_lacks_image_support`'s own
/// doc comment names and depends on staying `false`.
///
/// Reads the raw `initialize` reply directly (`AgentDescription::
/// from_initialize` is private to `crate::acp`, unreachable from an external
/// integration-test crate like this one) — the same path a decode ultimately
/// reads, just without the production type in between.
async fn grok_supports_image_attachments() -> bool {
    let exe = resolve_grok_executable().expect("grok resolves on this machine");
    let mut command = tokio::process::Command::new(&exe);
    command.args(GROK_ARGS);
    let discovered = AcpSession::open_for_discovery(
        command,
        &std::env::temp_dir().to_string_lossy(),
        Timeouts::default(),
    )
    .await
    .expect("the handshake answers");
    discovered.initialized["agentCapabilities"]["promptCapabilities"]["image"].as_bool()
        == Some(true)
}

fn controls() -> (RunControls, ApprovalWatch) {
    let (_steer_tx, steer_rx) = mpsc::channel(1);
    let (request_approval, approval_watch) = support::recording_decliner();
    (
        RunControls {
            request_input: Box::new(|_| oneshot::channel().1),
            request_approval,
            steering: steer_rx,
            interrupt: CancellationToken::new(),
        },
        approval_watch,
    )
}

async fn run_to_done(harness: &GrokHarness, request: RunRequest) -> (String, String) {
    let (controls, approval_watch) = controls();
    let stream = harness
        .run(request, controls)
        .await
        .expect("the agent starts");
    let mut text = String::new();
    let mut session_id = String::new();
    // If the run never settles BECAUSE it escalated to an approval nothing
    // here answers, `settle_or_report` fails immediately naming it rather
    // than waiting out this whole budget with no explanation (D71 (2)).
    support::settle_or_report(
        stream,
        Duration::from_secs(180),
        &approval_watch,
        |event| match event {
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
        },
    )
    .await;
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

/// **What this test does NOT prove, stated plainly: it does not confirm the
/// model describes an attached image.** Grok's own `promptCapabilities.image`
/// reads `false` on the captured 2026-08-28 handshake (`grok.rs`'s doc
/// comment), so `session.rs`'s own capability gate never attaches an image
/// content block for this agent — `crate::claude::load_image_blocks` is
/// never even called. `session.agent().supports_image_attachments()` is
/// asserted directly below for exactly that reason: if a future Grok build
/// ever answers `true` here, THIS test would go on passing for the wrong
/// reason (an attachment silently never sent) unless something notices the
/// capability flipped. Task brief Step 8 asks to "confirm the model
/// describes it" — that assertion needs an image-CAPABLE live agent, and
/// none is available today: Hermes cannot open a session at all on this
/// machine (this module's header), and Grok's own answer is `false`. What
/// this test actually verifies is narrower and still real: a run with a
/// staged attachment completes normally against an agent that cannot use it,
/// rather than erroring or hanging on the unsupported field.
#[tokio::test]
#[ignore = "spends tokens against the real Grok CLI; run with --ignored"]
async fn a_run_with_an_attachment_completes_when_grok_lacks_image_support() {
    assert!(
        !grok_supports_image_attachments().await,
        "this test's whole premise is that promptCapabilities.image reads false on this \
         install; it now reads true, so this test is no longer exercising the unsupported \
         path it claims to -- content-description coverage belongs in a NEW test gated on \
         this same capability, not silently folded into this one"
    );

    let cwd = std::env::temp_dir().join("comet-acp-run-fidelity-grok-live-image");
    std::fs::create_dir_all(&cwd).expect("disposable cwd");
    let image = write_tiny_png();

    let harness = GrokHarness::new();
    let request = RunRequest {
        prompt: "Reply with exactly: OK.".into(),
        attachments: vec![image],
        reasoning: Some(ReasoningLevel::Low),
        cwd: cwd.to_string_lossy().into_owned(),
        ..RunRequest::for_session(RuntimeMode::default())
    };

    let (_session_id, text) = run_to_done(&harness, request).await;
    assert!(
        text.to_uppercase().contains("OK"),
        "the run completes normally with an attachment staged, even though this agent \
         cannot use it: {text:?}"
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
