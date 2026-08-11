//! CodexHarness integration tests against the fake app server in
//! `tests/fixtures/fake_codex.rs` (no real `codex` binary involved).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use tokio::sync::{mpsc, oneshot};

use comet_harness::{
    CancellationToken, CodexHarness, Harness, HarnessError, RunControls, SteerMessage,
};
use comet_proto::{
    AgentEvent, ApprovalDecision, ApprovalRequest, DiagnosticSeverity, DoneStatus, FileOperation,
    HarnessId, NoticeKind, NoticeSeverity, ReasoningLevel, RunRequest, RuntimeMode, SandboxLevel,
    TodoItem, ToolCall, UserInputAnswer,
};

/// The `fake-codex` bin target, built by cargo alongside this test.
fn fixture_path() -> &'static str {
    env!("CARGO_BIN_EXE_fake-codex")
}

fn harness() -> CodexHarness {
    CodexHarness::new()
        .with_executable(fixture_path())
        .with_codex_home(logged_in_home())
}

/// A `CODEX_HOME` that looks logged in, created once and alive for the whole
/// test process.
///
/// Discovery refuses to ask a logged-out CLI — it answers with a hardcoded
/// list that does not match the account — and neither CI nor a fresh checkout
/// has a real `~/.codex/auth.json`. Without this every discovery test would
/// pass here and assert nothing there.
fn logged_in_home() -> &'static std::path::Path {
    static HOME: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    HOME.get_or_init(|| {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("auth.json"), "{}").expect("auth.json");
        dir
    })
    .path()
}

fn request(prompt: &str) -> RunRequest {
    RunRequest {
        prompt: prompt.into(),
        model: Some("gpt-5.6-sol".into()),
        reasoning: Some(ReasoningLevel::Ultra),
        model_options: serde_json::Map::new(),
        cwd: String::new(),
        // Deliberately mismatched, unlike the Claude fixture: the Codex
        // adapter reads `sandbox` directly (Claude does not), so exercising
        // `FullAccess` against a `WorkspaceWrite` sandbox is a real,
        // distinct case here, not fixture drift.
        runtime_mode: RuntimeMode::FullAccess,
        sandbox: SandboxLevel::WorkspaceWrite,
        attachments: Vec::new(),
        resume: None,
    }
}

/// Controls whose `request_input` answers every question with `answer_label`.
fn controls(
    answer_label: &'static str,
) -> (RunControls, mpsc::Sender<SteerMessage>, CancellationToken) {
    let (steer_tx, steer_rx) = mpsc::channel(8);
    let token = CancellationToken::new();
    let controls = RunControls {
        request_input: Box::new(move |questions| {
            let (tx, rx) = oneshot::channel();
            let answers: Vec<UserInputAnswer> = questions
                .iter()
                .map(|q| UserInputAnswer {
                    question_id: q.id.clone(),
                    labels: vec![answer_label.into()],
                })
                .collect();
            let _ = tx.send(answers);
            rx
        }),
        // No decision source in this fixture: the dropped sender resolves the
        // receiver to an error, which a run must treat as not approved. Never
        // default a fixture to Allow — that is how a permission defect ships
        // looking correct.
        request_approval: Box::new(|_approval: comet_proto::ApprovalRequest| {
            let (_tx, rx) = oneshot::channel::<comet_proto::ApprovalDecision>();
            rx
        }),
        steering: steer_rx,
        interrupt: token.clone(),
    };
    (controls, steer_tx, token)
}

async fn run_to_end(
    harness: &CodexHarness,
    req: RunRequest,
    controls: RunControls,
) -> Vec<AgentEvent> {
    let stream = harness.run(req, controls).await.expect("run starts");
    tokio::time::timeout(
        Duration::from_secs(10),
        stream.map(|r| r.expect("stream event")).collect::<Vec<_>>(),
    )
    .await
    .expect("run finished in time")
}

#[tokio::test]
async fn happy_path_maps_deltas_items_usage_and_done() {
    let (controls, _steer, _token) = controls("Yes");
    let mut req = request("scenario:happy");
    req.cwd = "/tmp".into();
    req.model_options.insert(
        "serviceTier".into(),
        serde_json::Value::String("fast".into()),
    );
    let events = run_to_end(&harness(), req, controls).await;

    // SessionStarted from thread/start's thread id.
    let starts: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::SessionStarted {
                harness,
                model,
                cwd,
                session_id,
                ..
            } => Some((harness, model, cwd, session_id)),
            _ => None,
        })
        .collect();
    assert_eq!(starts.len(), 1, "{events:?}");
    let (h, model, cwd, session_id) = starts[0];
    assert_eq!(*h, HarnessId::Codex);
    assert_eq!(model, "gpt-5.6-sol");
    assert_eq!(cwd, "/tmp");
    assert_eq!(session_id, "th-1");

    // Deltas — both wire spellings accepted.
    assert!(events.contains(&AgentEvent::TextDelta {
        text: "Hello".into()
    }));
    assert!(events.contains(&AgentEvent::ReasoningDelta {
        text: "thinking hard".into()
    }));
    assert!(events.contains(&AgentEvent::ReasoningDelta {
        text: "summary".into()
    }));

    // commandExecution: ToolCall at started only, exit code 1 => error result.
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, AgentEvent::ToolCall { id, .. } if id == "c1"))
            .count(),
        1
    );
    assert!(events.contains(&AgentEvent::ToolCall {
        id: "c1".into(),
        call: ToolCall::Exec {
            command: "ls -la".into()
        },
    }));
    assert!(events.contains(&AgentEvent::ToolResult {
        id: "c1".into(),
        is_error: true
    }));

    // fileChange (single add): WriteFile, refreshed at completion.
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(
                e,
                AgentEvent::ToolCall {
                    id,
                    call: ToolCall::WriteFile { path, content: None }
                } if id == "f1" && path == "/tmp/new.rs"
            ))
            .count(),
        2,
        "started + completion-refresh: {events:?}"
    );
    assert!(events.contains(&AgentEvent::ToolResult {
        id: "f1".into(),
        is_error: false
    }));

    // mcpToolCall with failed status.
    assert!(events.contains(&AgentEvent::ToolCall {
        id: "mcp1".into(),
        call: ToolCall::Mcp {
            server: "linear".into(),
            tool: "search".into(),
            input: Some(serde_json::json!({"q": "bug"})),
        },
    }));
    assert!(events.contains(&AgentEvent::ToolResult {
        id: "mcp1".into(),
        is_error: true
    }));

    // webSearch lifecycle.
    assert!(events.contains(&AgentEvent::ToolCall {
        id: "w1".into(),
        call: ToolCall::WebSearch {
            query: "rust".into()
        },
    }));
    assert!(events.contains(&AgentEvent::ToolResult {
        id: "w1".into(),
        is_error: false
    }));

    // Completion-only todoList still opens and closes the lifecycle.
    assert!(events.contains(&AgentEvent::ToolCall {
        id: "td1".into(),
        call: ToolCall::Todo {
            items: vec![
                TodoItem {
                    text: "a".into(),
                    done: true
                },
                TodoItem {
                    text: "b".into(),
                    done: false
                },
            ]
        },
    }));
    assert!(events.contains(&AgentEvent::ToolResult {
        id: "td1".into(),
        is_error: false
    }));

    // Streamed agentMessage must not re-emit its completed text…
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta { text } if text == "Hello world")),
        "streamed message text re-emitted: {events:?}"
    );
    // …but a never-streamed one falls back to the completed text.
    assert!(events.contains(&AgentEvent::TextDelta {
        text: "unstreamed tail".into()
    }));
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, AgentEvent::AssistantMessageCompleted { .. }))
            .count(),
        2
    );

    // Usage rides just before the terminal Done.
    let usage_pos = events
        .iter()
        .position(|e| {
            matches!(
                e,
                AgentEvent::Usage {
                    input_tokens: 42,
                    output_tokens: 7
                }
            )
        })
        .expect("usage emitted");
    let done_pos = events
        .iter()
        .position(|e| matches!(e, AgentEvent::Done { .. }))
        .expect("done emitted");
    assert!(usage_pos < done_pos);
    assert_eq!(
        events.last(),
        Some(&AgentEvent::Done {
            status: DoneStatus::Completed,
            result: None,
            error: None,
            session_id: Some("th-1".into()),
        })
    );

    // fake_codex's happy stream includes `some/unknownNotification`
    // (fixtures/fake_codex.rs:219) — since 0b.2 it surfaces as exactly one
    // diagnostic, and it precedes Done, so the positional assertions above
    // (usage before done, done last) still hold.
    let diag_pos = events
        .iter()
        .position(|e| {
            matches!(
                e,
                AgentEvent::Diagnostic {
                    discriminator,
                    severity: DiagnosticSeverity::Unknown,
                    ..
                } if discriminator == "some/unknownNotification"
            )
        })
        .expect("unknown notification surfaced as a diagnostic");
    assert!(diag_pos < done_pos);
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, AgentEvent::Diagnostic { .. }))
            .count(),
        1
    );
}

/// `Auto` is the only mode that hands approval review to the provider, so it
/// is the only one whose reviewer value the other scenarios cannot pin.
#[tokio::test]
async fn auto_mode_sends_the_provider_as_the_approvals_reviewer() {
    let (controls, _steer, _token) = controls("Yes");
    let mut req = request("scenario:auto-reviewer");
    req.runtime_mode = RuntimeMode::Auto;
    let events = run_to_end(&harness(), req, controls).await;

    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::Done {
                status: DoneStatus::Completed,
                ..
            }
        )),
        "{events:?}"
    );
}

/// `AutoAcceptEdits` is declared in [`CodexHarness::capabilities`] but no
/// other test ever sets it. It maps to the same `"user"` reviewer `happy`'s
/// thread-line assertions already pin for `FullAccess`
/// (`fixtures/fake_codex.rs:167`), so reusing `happy` — with the same `cwd`
/// and `serviceTier` the assertions require — is enough to prove the mapping
/// reaches the wire without a dedicated scenario.
#[tokio::test]
async fn auto_accept_edits_reaches_the_wire_as_user() {
    let (controls, _steer, _token) = controls("Yes");
    let mut req = request("scenario:happy");
    req.runtime_mode = RuntimeMode::AutoAcceptEdits;
    req.cwd = "/tmp".into();
    req.model_options.insert(
        "serviceTier".into(),
        serde_json::Value::String("fast".into()),
    );
    let events = run_to_end(&harness(), req, controls).await;

    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::Done {
                status: DoneStatus::Completed,
                ..
            }
        )),
        "{events:?}"
    );
}

#[tokio::test]
async fn steering_uses_turn_steer_with_expected_turn_id() {
    let (controls, steer, _token) = controls("Yes");
    steer
        .send(SteerMessage {
            prompt: "redirect please".into(),
            message_id: None,
        })
        .await
        .expect("steer queued");
    let events = run_to_end(&harness(), request("scenario:steer"), controls).await;

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
        .expect("Steered emitted: {events:?}");
    assert!(steered.0.is_some() && steered.1.is_some());
    assert_ne!(steered.0, steered.1);

    // The fake only emits this delta after verifying expectedTurnId + text.
    assert!(events.contains(&AgentEvent::TextDelta {
        text: "steered".into()
    }));
    assert_eq!(
        events.last(),
        Some(&AgentEvent::Done {
            status: DoneStatus::Completed,
            result: None,
            error: None,
            session_id: Some("th-1".into()),
        })
    );
}

#[tokio::test]
async fn rejected_steer_falls_back_to_a_follow_up_turn() {
    let (controls, steer, _token) = controls("Yes");
    steer
        .send(SteerMessage {
            prompt: "redirect please".into(),
            message_id: None,
        })
        .await
        .expect("steer queued");
    let events = run_to_end(&harness(), request("scenario:steer-race"), controls).await;

    // Two turns: the raced one completes, then the fallback carries the steer.
    let dones: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Done { status, .. } => Some(*status),
            _ => None,
        })
        .collect();
    assert_eq!(
        dones,
        vec![DoneStatus::Completed, DoneStatus::Completed],
        "{events:?}"
    );
    let steered_pos = events
        .iter()
        .position(|e| matches!(e, AgentEvent::Steered { .. }))
        .expect("Steered emitted on fallback");
    let first_done_pos = events
        .iter()
        .position(|e| matches!(e, AgentEvent::Done { .. }))
        .expect("first done");
    assert!(
        first_done_pos < steered_pos,
        "fallback turn starts after the raced turn ends: {events:?}"
    );
    // Only emitted by the fake when the fallback turn/start carried the text.
    assert!(events.contains(&AgentEvent::TextDelta {
        text: "fallback".into()
    }));
}

#[tokio::test]
async fn approvals_reach_the_approval_bridge_not_the_input_bridge() {
    // Approvals must reach the ENGINE's approval bridge (`request_approval`).
    // They used to be synthesized into a yes/no question on the INPUT bridge,
    // which surfaced a permission decision as a generic prompt; that route is
    // gone. The harness must still emit no input lifecycle events of its own —
    // the bridge owns that lifecycle and mints the id the resolver parks under.
    let asked: Arc<Mutex<Vec<ApprovalRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let (steer_tx, steer_rx) = mpsc::channel(8);
    let _steer = steer_tx;
    let token = CancellationToken::new();
    let seen = asked.clone();
    let controls = RunControls {
        // Nothing on this run asks a question; an answer here would be a
        // failure, so the fixture provides none.
        request_input: Box::new(|_questions| {
            let (_tx, rx) = oneshot::channel::<Vec<UserInputAnswer>>();
            rx
        }),
        request_approval: Box::new(move |approval: ApprovalRequest| {
            seen.lock().unwrap().push(approval);
            let (tx, rx) = oneshot::channel();
            let _ = tx.send(ApprovalDecision::Allow);
            rx
        }),
        steering: steer_rx,
        interrupt: token.clone(),
    };
    let mut req = request("scenario:approve");
    req.runtime_mode = RuntimeMode::ApprovalRequired;
    let events = run_to_end(&harness(), req, controls).await;

    let asked = asked.lock().unwrap();
    assert_eq!(asked.len(), 3, "{events:?}");
    // The parsed action, not the launcher invocation wrapped around it.
    assert_eq!(
        asked[0],
        ApprovalRequest::Command {
            command: "rm -rf /tmp/x".into(),
            cwd: Some("/tmp".into()),
        }
    );
    // The path came from the item, because the request itself carries none.
    assert_eq!(
        asked[1],
        ApprovalRequest::FileChange {
            path: "/tmp/a.rs".into(),
            operation: FileOperation::Modify,
            added_lines: 1,
            removed_lines: 0,
        }
    );
    // The join missed: vague, and un-allowlistable, rather than a wrong path.
    assert_eq!(
        asked[2],
        ApprovalRequest::Unknown {
            summary: "Change a file".into()
        }
    );
    assert!(
        !events.iter().any(|e| matches!(
            e,
            AgentEvent::InputRequested { .. } | AgentEvent::InputResolved { .. }
        )),
        "harness must not emit input lifecycle events itself: {events:?}"
    );

    // The fake only completes the turn after seeing every accept decision.
    assert_eq!(
        events.last(),
        Some(&AgentEvent::Done {
            status: DoneStatus::Completed,
            result: None,
            error: None,
            session_id: Some("th-1".into()),
        })
    );
}

#[tokio::test]
async fn approval_no_answer_becomes_decline() {
    let (controls, _steer, _token) = controls("No");
    let mut req = request("scenario:decline");
    req.runtime_mode = RuntimeMode::ApprovalRequired;
    let events = run_to_end(&harness(), req, controls).await;

    // The fake only completes the turn after seeing the decline decision.
    assert!(
        matches!(
            events.last(),
            Some(AgentEvent::Done {
                status: DoneStatus::Completed,
                ..
            })
        ),
        "{events:?}"
    );
}

#[tokio::test]
async fn interrupt_sends_turn_interrupt_and_maps_aborted() {
    let (controls, _steer, token) = controls("Yes");
    let mut stream = harness()
        .run(request("scenario:interrupt"), controls)
        .await
        .expect("run starts");

    let events = tokio::time::timeout(Duration::from_secs(10), async move {
        let mut events = Vec::new();
        while let Some(ev) = stream.next().await {
            let ev = ev.expect("stream event");
            if matches!(&ev, AgentEvent::TextDelta { text } if text == "working") {
                token.cancel(); // interrupt mid-turn
            }
            events.push(ev);
        }
        events
    })
    .await
    .expect("interrupt completed in time");

    assert_eq!(
        events.last(),
        Some(&AgentEvent::Done {
            status: DoneStatus::Interrupted,
            result: None,
            error: None,
            session_id: Some("th-1".into()),
        })
    );
}

#[tokio::test]
async fn unresponsive_child_is_reaped_with_interrupted_done() {
    let harness = CodexHarness::new()
        .with_executable(fixture_path())
        .with_graces(Duration::from_millis(100), Duration::from_millis(500));
    let (controls, _steer, token) = controls("Yes");
    let mut stream = harness
        .run(request("scenario:wedge"), controls)
        .await
        .expect("run starts");

    let events = tokio::time::timeout(Duration::from_secs(10), async move {
        let mut events = Vec::new();
        while let Some(ev) = stream.next().await {
            let ev = ev.expect("stream event");
            if matches!(&ev, AgentEvent::TextDelta { text } if text == "working") {
                token.cancel();
            }
            events.push(ev);
        }
        events
    })
    .await
    .expect("escalation completed in time");

    assert_eq!(
        events.last(),
        Some(&AgentEvent::Done {
            status: DoneStatus::Interrupted,
            result: None,
            error: None,
            session_id: Some("th-1".into()),
        })
    );
}

#[tokio::test]
async fn turn_failed_maps_to_errored_done() {
    let (controls, _steer, _token) = controls("Yes");
    let events = run_to_end(&harness(), request("scenario:fail"), controls).await;
    assert_eq!(
        events.last(),
        Some(&AgentEvent::Done {
            status: DoneStatus::Errored,
            result: None,
            error: Some("boom".into()),
            session_id: Some("th-1".into()),
        })
    );
}

#[tokio::test]
async fn resume_falls_back_to_fresh_thread() {
    let (controls, _steer, _token) = controls("Yes");
    let mut req = request("scenario:resumed");
    req.resume = Some("resume-fail".into());
    let events = run_to_end(&harness(), req, controls).await;

    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::SessionStarted { session_id, .. } if session_id == "th-fresh"
        )),
        "fresh thread expected: {events:?}"
    );
    assert_eq!(
        events.last(),
        Some(&AgentEvent::Done {
            status: DoneStatus::Completed,
            result: None,
            error: None,
            session_id: Some("th-fresh".into()),
        })
    );
}

#[tokio::test]
async fn resume_reuses_the_existing_thread() {
    let (controls, _steer, _token) = controls("Yes");
    let mut req = request("scenario:resumed");
    req.resume = Some("resume-ok".into());
    let events = run_to_end(&harness(), req, controls).await;

    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::SessionStarted { session_id, .. } if session_id == "th-resumed"
        )),
        "resumed thread expected: {events:?}"
    );
}

#[tokio::test]
async fn missing_binary_is_not_installed() {
    let harness = CodexHarness::new().with_executable("/nonexistent/codex-nowhere");
    let (controls, _steer, _token) = controls("Yes");
    let err = harness
        .run(request("scenario:happy"), controls)
        .await
        .err()
        .expect("spawn fails");
    assert!(matches!(err, HarnessError::NotInstalled(_)), "{err:?}");
}

/// The curated list 2.1 pinned, now reached through the built-in path: a CLI
/// that cannot answer leaves it exactly as it was, which is what makes a failed
/// discovery harmless.
#[tokio::test]
async fn the_curated_catalog_survives_a_failed_discovery() {
    let missing = CodexHarness::new()
        .with_executable("/nonexistent/codex-nowhere")
        .with_codex_home(logged_in_home());
    let catalog = missing.models().await.expect("models");
    assert_eq!(
        catalog.source,
        comet_proto::CatalogSource::BuiltIn,
        "a CLI that cannot answer must not be reported as a live list"
    );
    assert!(!catalog.models.is_empty());
    let models = catalog.models;
    assert_eq!(models.len(), 7);
    assert_eq!(models[0].id, "gpt-5.6-sol");
    assert!(models[0].reasoning_levels.contains(&ReasoningLevel::Ultra));
    assert!(
        models
            .iter()
            .all(|m| m.options.iter().any(|o| o.id == "serviceTier"))
    );

    // models() requires a resolvable binary… but with_executable trusts the
    // caller's path, so only the default resolution can report NotInstalled —
    // exercise the harness identity surface instead.
    assert_eq!(missing.id(), HarnessId::Codex);
    // "Codex" — comet composer/defaults.ts HARNESS_LABEL (and the registry's
    // lazy descriptor must stay in lockstep).
    assert_eq!(missing.display_name(), "Codex");
    assert_eq!(missing.capabilities().reasoning_levels.len(), 7);
    // The trait impl must hand back the associated declaration verbatim —
    // that identity is what lets the registry's lazy descriptor name it.
    assert_eq!(missing.capabilities(), CodexHarness::capabilities());
}

#[tokio::test]
async fn claimed_notifications_surface_as_notices() {
    let (controls, _steer, _token) = controls("Yes");
    let events = run_to_end(&harness(), request("scenario:notices"), controls).await;

    let notices: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Notice {
                kind,
                severity,
                summary,
                key,
                ..
            } => Some((*kind, *severity, summary.clone(), key.clone())),
            _ => None,
        })
        .collect();

    assert!(
        notices.contains(&(
            NoticeKind::McpStatus,
            NoticeSeverity::Warning,
            "MCP server linear failed to start".to_string(),
            Some("mcp:linear".to_string()),
        )),
        "{notices:?}"
    );
    assert!(
        notices.contains(&(
            NoticeKind::AuthStatus,
            NoticeSeverity::Info,
            "Signed in to MCP server linear".to_string(),
            Some("mcp:linear".to_string()),
        )),
        "{notices:?}"
    );
    assert!(
        notices.contains(&(
            NoticeKind::McpStatus,
            NoticeSeverity::Warning,
            "Remote environment disconnected".to_string(),
            Some("environment".to_string()),
        )),
        "{notices:?}"
    );
    // Threshold filter: four rolling rate-limit updates produce exactly two
    // notices — the first 80% crossing and the first 95% crossing.
    let rate: Vec<&String> = notices
        .iter()
        .filter(|(k, ..)| *k == NoticeKind::RateLimit)
        .map(|(_, _, s, _)| s)
        .collect();
    assert_eq!(rate.len(), 2, "{notices:?}");
    assert_eq!(rate[0], "Codex usage is at 85% of its limit");
    assert_eq!(rate[1], "Codex usage is at 97% of its limit");

    // "starting" produced nothing: failed + oauth + env + 2 rate = 5 total.
    assert_eq!(notices.len(), 5, "{notices:?}");

    // End to end, no notice's detail carries a raw provider error string
    // (user-facing-errors rule 1) — the failed startup shows Comet's own copy
    // derived from the structured `failureReason` instead.
    let details: Vec<Option<String>> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Notice { detail, .. } => Some(detail.clone()),
            _ => None,
        })
        .collect();
    assert!(
        !details
            .iter()
            .flatten()
            .any(|d| d.contains("ECONNREFUSED") || d.contains("127.0.0.1")),
        "{details:?}"
    );
    assert!(
        details.contains(&Some(
            "Sign in to this server again to reconnect it.".to_string()
        )),
        "{details:?}"
    );
    assert!(!events.iter().any(|e| matches!(e, AgentEvent::Error { .. })));
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Done {
            status: DoneStatus::Completed,
            ..
        })
    ));
}

#[tokio::test]
async fn unclaimed_notifications_items_and_requests_surface_as_diagnostics() {
    let (controls, _steer, _token) = controls("Yes");
    let events = run_to_end(&harness(), request("scenario:diagnostics"), controls).await;

    let diagnostics: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Diagnostic {
                discriminator,
                severity,
                ..
            } => Some((discriminator.clone(), *severity)),
            _ => None,
        })
        .collect();
    assert_eq!(
        diagnostics,
        vec![
            // sink 5: a non-JSON line, then a JSON frame with neither
            // `method` nor `id` — both the Malformed sentinel.
            ("unparseable".to_string(), DiagnosticSeverity::Malformed),
            ("unparseable".to_string(), DiagnosticSeverity::Malformed),
            // sink 2: an unknown notification method, verbatim.
            (
                "thread/checkpoint/created".to_string(),
                DiagnosticSeverity::Unknown
            ),
            // sink 4: an unknown item type — started AND completed each count.
            (
                "item/contextCompaction".to_string(),
                DiagnosticSeverity::Unknown
            ),
            (
                "item/contextCompaction".to_string(),
                DiagnosticSeverity::Unknown
            ),
            // an unknown server→client request: answered -32601, then counted.
            (
                "some/unknownRequest".to_string(),
                DiagnosticSeverity::Unknown
            ),
        ],
        "{events:?}"
    );
    // Redaction is structural: no provider text on any diagnostic.
    for e in &events {
        if let AgentEvent::Diagnostic { summary, .. } = e {
            assert!(!summary.contains("do-not-carry"), "{summary}");
        }
    }
    // The Ignored tier (thread/settings/updated, remoteControl/status/changed,
    // thread/status/changed, item/reasoning/summaryPartAdded — all
    // capture-confirmed routine) produced nothing; the run ends cleanly.
    assert!(!events.iter().any(|e| matches!(e, AgentEvent::Error { .. })));
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Done {
            status: DoneStatus::Completed,
            ..
        })
    ));
}

/// All four, now that the policy is derived from the mode and an approval it
/// raises reaches the user. `ApprovalRequired` and `Auto` were withheld while
/// the policy was pinned at `"never"`, because a declared mode the adapter
/// cannot keep is a promise the run breaks.
#[test]
fn every_runtime_mode_is_declared_once_the_policy_is_derived() {
    let caps = CodexHarness::capabilities();
    assert_eq!(
        caps.runtime_modes,
        vec![
            RuntimeMode::ApprovalRequired,
            RuntimeMode::AutoAcceptEdits,
            RuntimeMode::Auto,
            RuntimeMode::FullAccess,
        ]
    );
}

/// Every declared mode reaches the wire as the policy the mapping table names,
/// on **both** `thread/start` and `turn/start` — they are one binding and two
/// sites, and a mode honoured on one but not the other would be silent.
#[tokio::test]
async fn every_runtime_mode_reaches_the_wire_as_its_approval_policy() {
    for (mode, want) in [
        (RuntimeMode::ApprovalRequired, "untrusted"),
        (RuntimeMode::AutoAcceptEdits, "on-request"),
        (RuntimeMode::Auto, "on-request"),
        (RuntimeMode::FullAccess, "never"),
    ] {
        let (controls, _steer, _token) = controls("Yes");
        let mut req = request("scenario:echo-policy");
        req.runtime_mode = mode;
        let events = run_to_end(&harness(), req, controls).await;
        let error = events
            .iter()
            .find_map(|e| match e {
                AgentEvent::Done { error, .. } => error.clone(),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{mode:?}: no Done carrying the observed policy"));
        assert!(
            error.contains(&format!("thread={want} turn={want}")),
            "{mode:?} wanted {want}, wire said {error}"
        );
    }
}

// ---------------------------------------------------------------------------
// Live discovery (slice 2.3)
// ---------------------------------------------------------------------------

/// The slice's deliverable: a real spawn, a real `initialize` + `model/list`
/// round trip, and a merged catalog that says it is live. The fixture answers
/// shaped exactly as codex-cli 0.147.0 did in the 2026-08-11 capture, and pages
/// by default — the real server returns all seven models in one page and would
/// never exercise the loop.
#[tokio::test]
async fn models_come_back_live_and_merged() {
    let catalog = harness().models().await.expect("models");
    assert_eq!(catalog.source, comet_proto::CatalogSource::Live);
    let ids: Vec<&str> = catalog.models.iter().map(|m| m.id.as_str()).collect();
    assert!(
        ids.contains(&"gpt-5.7-nova"),
        "a model only the server knows appears, got {ids:?}"
    );
    assert!(
        ids.contains(&"gpt-5.4-mini"),
        "a curated model the server did not list is still kept, got {ids:?}"
    );
    assert!(
        !ids.contains(&"codex-auto-review"),
        "a hidden model never reaches the picker, got {ids:?}"
    );
    assert_eq!(
        ids.iter().filter(|id| **id == "gpt-5.6-sol").count(),
        1,
        "a matched id is one row, not two, got {ids:?}"
    );
}

/// Paging is only ever exercised by the fixture, so it is worth asserting the
/// loop reassembled every page rather than stopping at the first: the models
/// the fake serves last are the ones a broken loop drops.
#[tokio::test]
async fn every_page_of_the_model_list_is_collected() {
    let catalog = harness().models().await.expect("models");
    let ids: Vec<&str> = catalog.models.iter().map(|m| m.id.as_str()).collect();
    // The fake serves five models two at a time; `gpt-5.7-nova` is fourth and
    // only reachable through two cursor round trips.
    assert!(
        ids.contains(&"gpt-5.7-nova"),
        "the third page never arrived, got {ids:?}"
    );
}

/// The live answer overrides a curated capability, which is the whole point of
/// reading `inputModalities`: `catalog.rs` marks every model image-capable and
/// the server says one of them is not.
#[tokio::test]
async fn a_text_only_model_loses_the_curated_image_flag() {
    let catalog = harness().models().await.expect("models");
    let spark = catalog
        .models
        .iter()
        .find(|m| m.id == "gpt-5.3-codex-spark")
        .expect("curated model present");
    assert!(
        !spark.accepts_images,
        "the server reports text-only; the curated `true` must not win"
    );
    let sol = catalog
        .models
        .iter()
        .find(|m| m.id == "gpt-5.6-sol")
        .expect("curated model present");
    assert!(sol.accepts_images, "an image-capable model is unchanged");
}

/// A model nobody has curated keeps the effort the provider reported for it,
/// `ultra` included — Codex reports it natively and `to_effort` already sends
/// it. It must not acquire Comet's own ultracode/ultrathink.
#[tokio::test]
async fn a_live_only_model_keeps_its_reported_ladder() {
    let catalog = harness().models().await.expect("models");
    let nova = catalog
        .models
        .iter()
        .find(|m| m.id == "gpt-5.7-nova")
        .expect("live-only model present");
    assert!(nova.reasoning_levels.contains(&ReasoningLevel::Ultra));
    assert!(!nova.reasoning_levels.contains(&ReasoningLevel::Ultracode));
    assert!(!nova.reasoning_levels.contains(&ReasoningLevel::Ultrathink));
    assert!(
        nova.accepts_images,
        "the fixture omits inputModalities entirely: absent means images work"
    );
}

/// An answer we cannot read is the one failure that means a provider changed
/// its protocol under us, and it must survive as `Unparseable` so the engine
/// raises its `Diagnostic` (`crates/engine/src/rpc.rs:1010`).
#[tokio::test]
async fn an_unreadable_answer_is_reported_as_drift() {
    let harness = CodexHarness::new()
        .with_executable(env!("CARGO_BIN_EXE_fake-codex-bad-discovery"))
        .with_codex_home(logged_in_home());
    let catalog = harness.models().await.expect("still answers");
    assert_eq!(
        catalog.source,
        comet_proto::CatalogSource::BuiltIn,
        "a broken reply still serves the curated list"
    );
    assert_eq!(
        harness.take_unreported_discovery_failure(),
        Some(comet_harness::discovery::DiscoveryFailure::Unparseable)
    );
}

/// The cursor is opaque and server-chosen, so nothing in the schema stops a
/// server handing back one that never advances. Unbounded, the picker would
/// await a loop that never ends.
///
/// The fixture stops answering after three pages so this cannot pass on the
/// page cap instead: without the did-not-advance guard the loop runs into EOF
/// and reports `Unreachable`, not drift.
#[tokio::test]
async fn a_cursor_that_never_advances_is_drift() {
    let harness = CodexHarness::new()
        .with_executable(env!("CARGO_BIN_EXE_fake-codex-stuck-cursor"))
        .with_codex_home(logged_in_home());
    let catalog = tokio::time::timeout(Duration::from_secs(20), harness.models())
        .await
        .expect("the paging loop terminated")
        .expect("still answers");
    assert_eq!(catalog.source, comet_proto::CatalogSource::BuiltIn);
    assert_eq!(
        harness.take_unreported_discovery_failure(),
        Some(comet_harness::discovery::DiscoveryFailure::Unparseable)
    );
}

/// A server whose cursor keeps advancing can page forever, and the
/// did-not-advance guard cannot see it. The page cap is the only thing that
/// ends this, and the picker is awaiting the answer while it runs.
#[tokio::test]
async fn an_endless_pager_is_stopped_by_the_page_cap() {
    let harness = CodexHarness::new()
        .with_executable(env!("CARGO_BIN_EXE_fake-codex-endless-cursor"))
        .with_codex_home(logged_in_home());
    let catalog = tokio::time::timeout(Duration::from_secs(20), harness.models())
        .await
        .expect("the paging loop terminated")
        .expect("still answers");
    assert_eq!(catalog.source, comet_proto::CatalogSource::BuiltIn);
    assert_eq!(
        harness.take_unreported_discovery_failure(),
        Some(comet_harness::discovery::DiscoveryFailure::Unparseable)
    );
}

/// The home the login check reads `auth.json` from must be the home the CLI is
/// actually given. Left to the ambient environment they can differ, and the
/// check then passes against one account's credentials while the CLI answers
/// from another's — or from its logged-out fallback list — with the result
/// still labelled live.
///
/// The fixture echoes its own `CODEX_HOME` back as a model label, because the
/// child is the only thing that can say which home it got.
#[tokio::test]
async fn the_child_is_given_the_home_the_login_check_validated() {
    let catalog = harness().models().await.expect("models");
    let echo = catalog
        .models
        .iter()
        .find(|m| m.id == "codex-home-echo")
        .expect("the fixture echoes its CODEX_HOME");
    assert_eq!(
        std::path::Path::new(&echo.label),
        logged_in_home(),
        "the CLI was handed a different home than the one that was checked"
    );
}

/// The cursor is the server's string, not ours: 0.147.0 sends a stringified
/// offset but the schema calls it opaque. The fixture issues one containing a
/// quote and a backslash, so a request built by interpolation is malformed
/// JSON on page two — and the models on the later pages silently disappear.
#[tokio::test]
async fn an_opaque_cursor_survives_the_next_request() {
    let catalog = harness().models().await.expect("models");
    let ids: Vec<&str> = catalog.models.iter().map(|m| m.id.as_str()).collect();
    assert!(
        ids.contains(&"gpt-5.7-nova"),
        "a page reached through a quoted cursor is missing, got {ids:?}"
    );
    assert_eq!(
        catalog.source,
        comet_proto::CatalogSource::Live,
        "a malformed follow-up request degrades to the built-in list"
    );
}

/// A logged-out `codex` answers `model/list` **successfully**, with a
/// hardcoded five-model list that does not match the account: it contains a
/// model the account cannot use and misses three it has (capture
/// `2026-08-11-codex-model-list.md`, run 6). Nothing in the envelope says so,
/// so the only defence is not to ask.
///
/// The fixture here is the good one — it would answer `Live`. Getting the
/// built-in list back is the proof that the gate fired before the spawn.
#[tokio::test]
async fn a_logged_out_cli_is_never_asked() {
    let home = tempfile::tempdir().expect("temp dir");
    let harness = harness().with_codex_home(home.path());
    let catalog = harness.models().await.expect("curated list still answers");
    assert_eq!(
        catalog.source,
        comet_proto::CatalogSource::BuiltIn,
        "no auth.json: the built-in list, honestly captioned"
    );
    assert_eq!(
        harness.take_unreported_discovery_failure(),
        Some(comet_harness::discovery::DiscoveryFailure::Unreachable),
        "not being logged in is ordinary, not protocol drift"
    );
}

/// The live check, against the real CLI rather than the fixture: `model/list`
/// answers cold, the ids land on the curated rows, and the one text-only model
/// comes back marked as such. Ignored by default — it needs an installed,
/// logged-in `codex` — but it spends no tokens, because discovery never starts
/// a thread.
/// Run with: `cargo test -p comet-harness --test codex -- --ignored`
#[tokio::test]
#[ignore = "requires installed+logged-in codex CLI (spends no tokens)"]
async fn live_cli_discovery_lands_on_curated_ids() {
    let catalog = CodexHarness::new().models().await.expect("models");
    let ids: Vec<&str> = catalog.models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(
        catalog.source,
        comet_proto::CatalogSource::Live,
        "the real model/list answered, got {ids:?}"
    );
    assert_eq!(
        ids,
        vec![
            "gpt-5.6-sol",
            "gpt-5.6-terra",
            "gpt-5.6-luna",
            "gpt-5.5",
            "gpt-5.4",
            "gpt-5.4-mini",
            "gpt-5.3-codex-spark",
        ],
        "seven curated rows, no duplicates and nothing hidden"
    );
    let spark = catalog
        .models
        .iter()
        .find(|m| m.id == "gpt-5.3-codex-spark")
        .expect("present");
    assert!(
        !spark.accepts_images,
        "the live inputModalities override reached the merged catalog"
    );
}

/// A CLI that cannot be spawned is ordinary, not drift — otherwise every
/// machine without Codex installed would report a protocol failure on boot.
#[tokio::test]
async fn a_missing_cli_is_not_drift() {
    let harness = CodexHarness::new()
        .with_executable("/nonexistent/codex-nowhere")
        .with_codex_home(logged_in_home());
    let catalog = harness.models().await.expect("curated list still answers");
    assert_eq!(catalog.source, comet_proto::CatalogSource::BuiltIn);
    assert_eq!(
        harness.take_unreported_discovery_failure(),
        Some(comet_harness::discovery::DiscoveryFailure::Unreachable)
    );
}

/// The cache belongs to the CLI it asked, not to the harness value. Pointing a
/// harness at a different executable and re-asking must re-run discovery —
/// otherwise the second CLI's answer is whatever the first one said.
#[tokio::test]
async fn changing_the_executable_re_runs_discovery() {
    let harness = harness();
    assert_eq!(
        harness.models().await.expect("first").source,
        comet_proto::CatalogSource::Live,
        "the good fixture answers"
    );

    let harness = harness.with_executable(env!("CARGO_BIN_EXE_fake-codex-bad-discovery"));
    assert_eq!(
        harness.models().await.expect("second").source,
        comet_proto::CatalogSource::BuiltIn,
        "the new executable is asked, not the old answer replayed"
    );
}
