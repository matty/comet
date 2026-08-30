//! One real turn against the installed Hermes CLI.
//!
//! **`#[ignore]`d: it spends tokens and needs an authenticated `hermes`.** The
//! rest of the suite proves the decode against fixtures and captured literals;
//! this proves the decode was pointed at the right thing, which no fixture can.
//!
//! ```text
//! cargo test -p comet-harness --test hermes_live -- --ignored --nocapture
//! ```
//!
//! `HERMES_EXECUTABLE` overrides resolution — needed on a machine where the
//! CLI was installed after the shell started, since its PATH entry is not yet
//! live.
//!
//! **BLOCKED on this machine as of PR1 (2026-08-28).** No LLM provider is
//! configured for Hermes here (`hermes model` / `hermes setup` are
//! interactive-terminal-only and refuse to run under a piped subprocess, so
//! the OAuth/API-key flow could not be driven from an agent session), and
//! Hermes' `session/new` — unlike Grok's — requires a configured provider
//! before it will even open a session: it eagerly constructs the LLM client
//! and fails with `-32603 "No LLM provider configured"` otherwise. A second
//! attempt with a fabricated `OPENROUTER_API_KEY` got past that check and
//! crashed instead on an unrelated Hermes-side bug (`module 'collections' has
//! no attribute 'MutableSet'`, a Python 3.10+ stdlib removal this
//! hermes-agent 0.15.2 install has not patched around) — not a Comet defect
//! either way. These tests are written and kept `#[ignore]`d rather than
//! deleted; whoever next has an authenticated Hermes should run them for real
//! and report the timing/`Done` status this file's own doc comments ask for.

mod support;

use std::time::Duration;

use comet_harness::acp::hermes::HermesHarness;
use comet_harness::{CancellationToken, Harness, RunControls};
use comet_proto::{AgentEvent, DoneStatus, RunRequest, RuntimeMode};
use tokio::sync::{mpsc, oneshot};

/// Deliberately trivial and tool-free: this test exists to watch the turn
/// machinery, and every token it spends buys output nobody keeps.
const PROMPT: &str = "Reply with exactly: OK. Do not use any tools.";

#[tokio::test]
#[ignore = "spends tokens against the real Hermes CLI; run with --ignored"]
async fn a_real_turn_streams_and_settles() {
    let cwd = std::env::temp_dir().join("comet-hermes-live");
    std::fs::create_dir_all(&cwd).expect("disposable cwd");

    let harness = HermesHarness::new();
    let (_steer_tx, steer_rx) = mpsc::channel(1);
    let (request_approval, approval_watch) = support::recording_decliner();
    let controls = RunControls {
        request_input: Box::new(|_| oneshot::channel().1),
        request_approval,
        steering: steer_rx,
        interrupt: CancellationToken::new(),
    };

    let request = RunRequest {
        prompt: PROMPT.into(),
        cwd: cwd.to_string_lossy().into_owned(),
        ..RunRequest::for_session(RuntimeMode::default())
    };

    let stream = harness
        .run(request, controls)
        .await
        .expect("the agent starts");

    // **Read to the first `Done`, not to stream end.** The session is
    // persistent: once a turn settles it parks on the steering mailbox and the
    // stream stays open, which is correct and is not what this test measures.
    // Collecting to the end hangs for the full timeout and reads as a harness
    // bug when it is this test holding the steer sender alive — the exact trap
    // `grok_live.rs` documents.
    //
    // Generous: a real model call on a VM with no GPU. Short enough that a turn
    // which never settles fails instead of hanging the suite. If it never
    // settles BECAUSE the run escalated to an approval nothing here answers,
    // `settle_or_report` fails immediately naming it rather than waiting out
    // this whole budget with no explanation (D71 (2)).
    let events =
        support::settle_or_report(stream, Duration::from_secs(180), &approval_watch, |event| {
            match event {
                AgentEvent::TextDelta { text } => print!("{text}"),
                AgentEvent::Done { status, error, .. } => {
                    println!("\n[done: {status:?} {error:?}]")
                }
                _ => {}
            }
        })
        .await;

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

    // **The user's own prompt must not come back as assistant text.** Every
    // recorded ACP agent echoes it as a `user_message_chunk` on the same
    // stream; mapping that to `TextDelta` would print the question into the
    // answer.
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

/// **The path the model picker actually runs, against the real CLI.**
///
/// The fixture-free unit tests cover the decode and this test's sibling
/// covers `run()`; neither covers `models()` against real Hermes. Unlike
/// Grok's version of this test, this is NOT expected to be token-free on an
/// unconfigured install: Hermes' `session/new` requires a provider before it
/// will open a session at all (see this file's header), so on an
/// unconfigured machine this exercises the BUILT-IN fallback path and its own
/// timing bound instead of a live discovery answer.
#[tokio::test]
#[ignore = "spawns the real Hermes CLI; run with --ignored"]
async fn discovery_answers_within_its_own_timeouts() {
    let harness = HermesHarness::new();

    // Deliberately longer than the harness's own handshake bound, so a failure
    // here means the bound did not fire — which is a different bug from a slow
    // agent, and the one worth telling apart.
    let started = std::time::Instant::now();
    let catalog = tokio::time::timeout(Duration::from_secs(90), harness.models())
        .await
        .expect("models() answers within its own timeouts rather than hanging")
        .expect("an installed CLI resolves");
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_secs(15),
        "discovery took {elapsed:?} — a timeout fired instead of the agent answering"
    );
    println!("discovery took {elapsed:?}");

    println!("source={:?}", catalog.source);
    for m in &catalog.models {
        println!("  {} — {} {:?}", m.id, m.label, m.reasoning_levels);
    }
    assert!(!catalog.models.is_empty());
}

/// **A tool-using turn shows its tools**, the same gap `grok_live.rs` was
/// written to catch: a tool that ran and returned with nothing shown in
/// between the announcement and the answer.
///
/// Asserts the transcript ORDER, not just presence: a card that arrives after
/// the answer it produced explains nothing.
#[tokio::test]
#[ignore = "spends tokens against the real Hermes CLI; run with --ignored"]
async fn a_tool_using_turn_shows_its_tools() {
    let cwd = std::env::temp_dir().join("comet-hermes-live-tools");
    std::fs::create_dir_all(&cwd).expect("disposable cwd");
    std::fs::write(
        cwd.join("alpha.txt"),
        "hello alpha
",
    )
    .expect("seed a file");

    let harness = HermesHarness::new();
    let (steer_tx, steer_rx) = mpsc::channel(1);
    let (request_approval, approval_watch) = support::recording_decliner();
    let controls = RunControls {
        request_input: Box::new(|_| oneshot::channel().1),
        request_approval,
        steering: steer_rx,
        interrupt: CancellationToken::new(),
    };

    let request = RunRequest {
        prompt: "Read alpha.txt with your tools, then say DONE.".into(),
        cwd: cwd.to_string_lossy().into_owned(),
        ..RunRequest::for_session(RuntimeMode::default())
    };

    let stream = harness
        .run(request, controls)
        .await
        .expect("the agent starts");
    drop(steer_tx);

    let events =
        support::settle_or_report(stream, Duration::from_secs(180), &approval_watch, |event| {
            match event {
                AgentEvent::ToolCall { id, call } => println!("[tool {id}] {call:?}"),
                AgentEvent::ToolResult { id, is_error, .. } => {
                    println!("[result {id}] is_error={is_error}")
                }
                AgentEvent::TextDelta { text } => print!("{text}"),
                _ => {}
            }
        })
        .await;
    println!();

    let call_at = events
        .iter()
        .position(|e| matches!(e, AgentEvent::ToolCall { .. }))
        .expect("the tool the agent ran is visible in the transcript");
    let result_at = events
        .iter()
        .position(|e| matches!(e, AgentEvent::ToolResult { .. }))
        .expect("and so is its result");
    assert!(
        call_at < result_at,
        "the call is drawn before its result: {events:#?}"
    );

    let done_at = events
        .iter()
        .position(|e| matches!(e, AgentEvent::Done { .. }))
        .expect("the turn settles");
    assert!(
        result_at < done_at,
        "the tool finishes before the turn does: {events:#?}"
    );

    // **The context meter has something to show**, off Hermes' own top-level
    // `usage` block (`normalize::usage`) rather than Grok's `_meta` one.
    let usage_at = events
        .iter()
        .position(|e| matches!(e, AgentEvent::Usage { .. }))
        .expect("the turn reports its tokens");
    assert!(
        usage_at < done_at,
        "the reading lands before the turn is reported finished: {events:#?}"
    );
    match &events[usage_at] {
        AgentEvent::Usage {
            prompt_tokens,
            output_tokens,
            context_window,
        } => {
            println!(
                "[usage] prompt={prompt_tokens} output={output_tokens} window={context_window:?}"
            );
            assert!(*prompt_tokens > 0, "a real turn has a real prompt size");
            assert!(*output_tokens > 0, "and a real answer");
        }
        other => unreachable!("{other:?}"),
    }

    // Exactly one card per call, not one per frame: ACP sends several frames
    // for one call and drawing each would repeat it in the transcript.
    let ids: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolCall { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    let unique: std::collections::HashSet<&str> = ids.iter().copied().collect();
    assert_eq!(ids.len(), unique.len(), "a call was drawn twice: {ids:?}");
}

/// **The `/` menu, against the real CLI.** Hermes' own source
/// (`acp_adapter.server.new_session`) schedules `available_commands_update`
/// on session creation, the same push-before-asked shape Grok uses — but
/// unlike Grok's, Hermes' `session/new` needs a configured provider to get
/// that far at all (see this file's header), so this is not expected to be
/// token-free on every install the way `grok_live`'s equivalent is.
#[tokio::test]
#[ignore = "spawns the real Hermes CLI; run with --ignored"]
async fn the_slash_menu_is_populated_from_the_agent() {
    let cwd = std::env::temp_dir().join("comet-hermes-live-cmds");
    std::fs::create_dir_all(&cwd).expect("disposable cwd");

    let harness = HermesHarness::new();
    let started = std::time::Instant::now();
    let commands = tokio::time::timeout(
        Duration::from_secs(60),
        harness.commands(&cwd.to_string_lossy()),
    )
    .await
    .expect("commands() answers within its own timeouts")
    .expect("an installed CLI resolves");
    let elapsed = started.elapsed();

    println!("{} commands in {elapsed:?}", commands.len());
    for c in commands.iter().take(4) {
        println!(
            "  /{} — {:?} hint={:?}",
            c.name, c.description, c.argument_hint
        );
    }

    assert!(
        commands.iter().all(|c| !c.name.starts_with('/')),
        "names carry no leading slash: the composer adds it"
    );

    // Same bound and the same reason as the model discovery test: this is one
    // handshake, and a timeout firing instead would still "pass" on content.
    assert!(
        elapsed < Duration::from_secs(15),
        "took {elapsed:?} — a timeout fired instead of the agent answering"
    );
}
