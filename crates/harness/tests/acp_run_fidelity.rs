//! PR7 — run-request fidelity: resume, attachments, model and effort.
//!
//! `RunRequest` carries `resume`, `attachments`, `model` and `reasoning`, and
//! before this PR none of them reached an ACP agent: `GrokHarness::run` and
//! `HermesHarness::run` built the launch command and opened a bare
//! `session/new`, dropping every one of those fields on the floor. These
//! tests drive the REAL harnesses (`GrokHarness`, `HermesHarness`) against
//! `fake-acp`, end to end through `Harness::run`, the same way
//! `tests/grok.rs` does — never by hand-building a JSON-RPC frame and
//! checking the fixture accepts it, which would prove the fixture's
//! contract, not that production code actually sends the frame.
//!
//! **The fixture ECHOES back what it received**, via a dedicated
//! `echo-selection` prompt keyword (`fake_acp.rs`): a `session/set_model`,
//! `session/set_config_option` or `session/load` request mutates state in the
//! fixture's own process, and the NEXT `session/prompt` reports that state
//! back as streamed text. Reading that echo through `AgentEvent::TextDelta`
//! is what proves a real request reached the child process, not merely that
//! some decode would have accepted it if it had — the CODEX_HOME lesson this
//! repository has paid for three times (`AGENTS.md`, "Step 3's fixture
//! rule").

use std::path::PathBuf;
use std::time::Duration;

use comet_harness::acp::grok::GrokHarness;
use comet_harness::acp::hermes::HermesHarness;
use comet_harness::acp::session::Timeouts;
use comet_harness::{CancellationToken, Harness, RunControls};
use comet_proto::{AgentEvent, ReasoningLevel, RunRequest, RuntimeMode};
use futures::StreamExt;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

/// Short enough that a hung run fails the suite instead of stalling it; this
/// suite is about the session-open path, never a slow turn.
const TEST_TIMEOUTS: Timeouts = Timeouts {
    handshake: Duration::from_secs(10),
    cancel_grace: Duration::from_millis(750),
    kill_grace: Duration::from_millis(250),
    prompt_stall: Duration::from_secs(10),
};

fn grok_fixture() -> GrokHarness {
    GrokHarness::new()
        .with_executable(env!("CARGO_BIN_EXE_fake-acp"))
        .with_timeouts(TEST_TIMEOUTS)
}

fn hermes_fixture() -> HermesHarness {
    HermesHarness::new()
        .with_executable(env!("CARGO_BIN_EXE_fake-acp"))
        .with_timeouts(TEST_TIMEOUTS)
}

fn controls() -> RunControls {
    let (_steer_tx, steer_rx) = mpsc::channel(1);
    RunControls {
        request_input: Box::new(|_| oneshot::channel().1),
        request_approval: Box::new(|_| oneshot::channel().1),
        steering: steer_rx,
        interrupt: CancellationToken::new(),
    }
}

/// Run one turn to completion and return every event up to and including
/// `Done`. Fails the test rather than hanging if the run never settles.
async fn run_and_collect(harness: &impl Harness, request: RunRequest) -> Vec<AgentEvent> {
    let mut stream = harness
        .run(request, controls())
        .await
        .expect("the fixture starts");
    tokio::time::timeout(Duration::from_secs(20), async {
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            let event = event.expect("no transport error");
            let settled = matches!(event, AgentEvent::Done { .. });
            events.push(event);
            if settled {
                break;
            }
        }
        events
    })
    .await
    .expect("the turn settles rather than hanging")
}

/// The `echo-selection` reply's own JSON, parsed out of the one `TextDelta`
/// that carries it (see `fake_acp.rs::handle_prompt`'s `echo-selection`
/// branch) — `{"model": ..., "config": {..}, "load": ..., "images": N}`.
fn echoed_selection(events: &[AgentEvent]) -> Value {
    events
        .iter()
        .find_map(|event| match event {
            AgentEvent::TextDelta { text } => serde_json::from_str::<Value>(text).ok(),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no echo-selection reply found in {events:#?}"))
}

fn session_started_id(events: &[AgentEvent]) -> String {
    events
        .iter()
        .find_map(|event| match event {
            AgentEvent::SessionStarted { session_id, .. } => Some(session_id.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no SessionStarted event found in {events:#?}"))
}

/// A small file with a `.png` extension so `crate::claude::image_media_type`
/// recognizes it by extension regardless of content — the same detection
/// path `load_image_blocks` uses in production.
fn write_temp_png(name: &str) -> String {
    let path: PathBuf = std::env::temp_dir().join(format!("comet-acp-run-fidelity-{name}.png"));
    std::fs::write(&path, b"not a real png, but the extension is enough").expect("write temp png");
    path.to_string_lossy().into_owned()
}

// ---------------------------------------------------------------------------
// Step 3: model and effort selection.
// ---------------------------------------------------------------------------

/// Break caught: dropping `RunRequest.model`. The picker's selection has to
/// reach the agent or the turn silently runs on the agent's default, which
/// reads to the user as the picker not working.
///
/// **Grok's own wire mechanism, corrected from a first design.** A raw
/// JSON-RPC probe against the real CLI (2026-08-29) found that
/// `session/set_config_option` — inferred from the ACP org's reference SDK
/// schema, since Grok's session config LOOKS like exactly that shape — is not
/// registered at all (`-32601 Method not found`). `session/set_model` (the
/// ACP spec's own dedicated method) is what actually works; see
/// `grok::config_requests`'s doc comment for the full probe evidence.
#[tokio::test]
async fn the_requested_model_reaches_the_agent() {
    let request = RunRequest {
        prompt: "echo-selection".into(),
        model: Some("fake-mini".into()),
        cwd: std::env::temp_dir().to_string_lossy().into_owned(),
        ..RunRequest::for_session(RuntimeMode::default())
    };
    let events = run_and_collect(&grok_fixture(), request).await;
    let echo = echoed_selection(&events);
    assert_eq!(
        echo["model"], "fake-mini",
        "Grok's own session/set_model must carry the selection: {echo}"
    );
}

/// The same selection, over Hermes' own `session/set_model` call — the SAME
/// ACP method Grok's own turns out to use (see the test above), verifying
/// both harnesses independently rather than only the one the brief names
/// verbatim.
#[tokio::test]
async fn the_requested_model_reaches_the_agent_over_hermes_set_model() {
    let request = RunRequest {
        prompt: "echo-selection".into(),
        model: Some("openai:gpt-5.4-mini".into()),
        cwd: std::env::temp_dir().to_string_lossy().into_owned(),
        ..RunRequest::for_session(RuntimeMode::default())
    };
    let events = run_and_collect(&hermes_fixture(), request).await;
    let echo = echoed_selection(&events);
    assert_eq!(
        echo["model"], "openai:gpt-5.4-mini",
        "Hermes' own dedicated session/set_model must carry the id: {echo}"
    );
}

/// Break caught: sending Grok's effort through a config-option setter that
/// reads `category: "mode"` as a PERMISSION mode. Upstream's own adapter
/// makes exactly this mistake.
///
/// **What the live probe actually found, and why this test now asserts
/// silence rather than a `configId`.** The plan's premise was that Grok
/// exposes some working setter for its `category: "mode"` effort ladder —
/// its own session config carries `selected` on every row, which does look
/// like a live setting. A raw JSON-RPC probe against grok 1.0.5
/// (2026-08-29) found no such setter: `session/set_config_option` (the
/// generic ACP method whose shape matches Grok's flat option rows) answers
/// `-32601 Method not found`, and `session/set_mode` (the ACP spec's own
/// approval-style mode setter) answers `{}` for EVERY `modeId` tried —
/// including a deliberately invalid one — which means it validates nothing
/// and is not evidence of a real effect. Sending an effort selection through
/// either would be the exact "silently ignored" trap the task brief warns
/// against, so `grok::config_requests` sends neither, and this test's
/// surviving, evidence-backed assertion is that no config call ever fires
/// for effort — not that one fires with the correct category. See
/// `grok.rs`'s own module doc ("No effort setter was found among the
/// methods tried") for the full probe transcript.
#[tokio::test]
async fn the_effort_selection_uses_the_effort_category_not_the_permission_one() {
    let request = RunRequest {
        prompt: "echo-selection".into(),
        reasoning: Some(ReasoningLevel::High),
        cwd: std::env::temp_dir().to_string_lossy().into_owned(),
        ..RunRequest::for_session(RuntimeMode::default())
    };
    let events = run_and_collect(&grok_fixture(), request).await;
    let echo = echoed_selection(&events);
    let config = echo["config"].as_object().expect("config is an object");
    assert!(
        config.is_empty(),
        "no working setter exists for Grok's effort ladder; nothing must be sent, and \
         nothing must ever land under a permission-mode-shaped key either: {echo}"
    );
}

/// An agent with no effort ladder must not be sent one. Hermes advertises
/// none; a selection sent anyway is either an error or silently ignored, and
/// both are worse than not sending it.
#[tokio::test]
async fn an_agent_without_an_effort_ladder_is_sent_no_effort() {
    let request = RunRequest {
        prompt: "echo-selection".into(),
        // Set defensively even though the picker should never offer an
        // effort choice for an agent with an empty ladder
        // (`HermesHarness::capabilities().reasoning_levels`).
        reasoning: Some(ReasoningLevel::High),
        cwd: std::env::temp_dir().to_string_lossy().into_owned(),
        ..RunRequest::for_session(RuntimeMode::default())
    };
    let events = run_and_collect(&hermes_fixture(), request).await;
    let echo = echoed_selection(&events);
    let config = echo["config"].as_object().expect("config is an object");
    assert!(
        config.is_empty(),
        "Hermes has no effort ladder; no config-option call of ANY kind must fire for \
         effort -- config being non-empty here means some setter landed, whichever one it \
         was: {echo}"
    );
}

// ---------------------------------------------------------------------------
// Step 5: resume, gated on `loadSession`.
// ---------------------------------------------------------------------------

/// `session/load` only where the agent advertised `loadSession`. Sending it
/// blind produces a protocol error the user sees for a feature they did not
/// ask for.
///
/// **Two independent signals, not one.** `SessionStarted.session_id` proves
/// the CLIENT took the load branch (its value is `resume_id.to_owned()` from
/// `session.rs`'s own code, not anything read off the wire) — but on its own
/// that is not proof a `session/load` FRAME ever reached the child; a stubbed
/// `open_or_resume` that skipped the RPC entirely and just returned the
/// requested id would pass that assertion too. `echo-selection` closes that
/// gap: `fake_acp.rs` only ever sets `last_load_session_id` inside its
/// `session/load` REQUEST HANDLER (`main`'s `"session/load" =>` arm), so
/// `echo["load"]` coming back non-null is evidence the fixture actually
/// received the frame, the same standard every other assertion in this file
/// holds itself to.
#[tokio::test]
async fn resume_loads_the_session_when_the_agent_advertises_it() {
    let request = RunRequest {
        prompt: "echo-selection".into(),
        resume: Some("prior-session-42".into()),
        cwd: std::env::temp_dir().to_string_lossy().into_owned(),
        ..RunRequest::for_session(RuntimeMode::default())
    };
    // `fake-acp` advertises `loadSession: true` by default (matching both
    // real agents' captured replies) — see `initialize_result`.
    let events = run_and_collect(&grok_fixture(), request).await;
    let started = session_started_id(&events);
    assert_eq!(
        started, "prior-session-42",
        "session/load must be used — its reply carries no sessionId of its own, so the \
         session id stays the one the client asked to resume. session/new, by contrast, \
         always mints a fresh `fake-session-N`: {started}"
    );
    let echo = echoed_selection(&events);
    assert_eq!(
        echo["load"], "prior-session-42",
        "the fixture's OWN session/load request handler must have actually run — this is \
         what the SessionStarted id alone cannot prove: {echo}"
    );
}

/// Break caught: falling back to a NEW session when load fails. A resumed
/// chat that silently starts empty loses the user's context with no signal —
/// the confident-wrong-answer shape this repo has hit repeatedly.
#[tokio::test]
async fn a_failed_load_reports_rather_than_starting_fresh() {
    let request = RunRequest {
        prompt: "hello".into(),
        // `fake-acp` answers a JSON-RPC error for any sessionId containing
        // "reject-load" — see `main`'s `session/load` arm.
        resume: Some("reject-load-1".into()),
        cwd: std::env::temp_dir().to_string_lossy().into_owned(),
        ..RunRequest::for_session(RuntimeMode::default())
    };
    let result = grok_fixture().run(request, controls()).await;
    assert!(
        result.is_err(),
        "a failed session/load must be reported as an error, never silently open a fresh session"
    );
}

/// Resume requested, but this agent never advertised `loadSession` at all.
/// Sending `session/load` anyway would be a protocol error for a feature the
/// user never asked for — the correct behavior is a plain new session, the
/// same as if `resume` had never been set.
#[tokio::test]
async fn resume_is_ignored_by_an_agent_that_never_advertised_load_session() {
    // SAFETY: nextest runs each test in its own process, so this does not
    // race the default-`true` behavior other tests in this binary rely on
    // (the same reasoning `tests/grok.rs::against_fixture` already documents
    // for `FAKE_ACP_SESSION_CONFIG`).
    unsafe { std::env::set_var("FAKE_ACP_NO_LOAD_SESSION", "1") };
    let request = RunRequest {
        prompt: "hello".into(),
        resume: Some("prior-session-99".into()),
        cwd: std::env::temp_dir().to_string_lossy().into_owned(),
        ..RunRequest::for_session(RuntimeMode::default())
    };
    let events = run_and_collect(&grok_fixture(), request).await;
    let started = session_started_id(&events);
    assert!(
        started.starts_with("fake-session-"),
        "an agent that never advertised loadSession must fall back to session/new, not error \
         and not send session/load: {started}"
    );
}

// ---------------------------------------------------------------------------
// Step 6: attachments, gated on `promptCapabilities.image`.
// ---------------------------------------------------------------------------

/// An agent that did not advertise image support gets the text block only.
/// The paths also ride the prompt text as `Attached images (local files …)`
/// refs (added upstream, in `crates/ui/src/attachments.rs`, before the
/// request ever reaches this harness), so the model is not left unaware of
/// them.
#[tokio::test]
async fn attachments_are_omitted_for_an_agent_without_image_support() {
    let image = write_temp_png("no-image-support");
    let request = RunRequest {
        prompt: "echo-selection".into(),
        attachments: vec![image],
        cwd: std::env::temp_dir().to_string_lossy().into_owned(),
        ..RunRequest::for_session(RuntimeMode::default())
    };
    // `fake-acp` answers `promptCapabilities.image: false` by default,
    // matching Grok's real captured reply.
    let events = run_and_collect(&grok_fixture(), request).await;
    let echo = echoed_selection(&events);
    assert_eq!(
        echo["images"], 0,
        "an agent without promptCapabilities.image must receive the text block only: {echo}"
    );
}

/// The mirror case: an agent that DOES advertise `promptCapabilities.image`
/// must actually receive the staged attachment as an image content block.
#[tokio::test]
async fn attachments_ride_the_prompt_when_the_agent_supports_images() {
    // SAFETY: see `resume_is_ignored_by_an_agent_that_never_advertised_load_session`.
    unsafe { std::env::set_var("FAKE_ACP_IMAGE_CAPABLE", "1") };
    let image = write_temp_png("with-image-support");
    let request = RunRequest {
        prompt: "echo-selection".into(),
        attachments: vec![image],
        cwd: std::env::temp_dir().to_string_lossy().into_owned(),
        ..RunRequest::for_session(RuntimeMode::default())
    };
    let events = run_and_collect(&grok_fixture(), request).await;
    let echo = echoed_selection(&events);
    assert_eq!(
        echo["images"], 1,
        "an agent that advertises promptCapabilities.image must receive the staged attachment: {echo}"
    );
}
