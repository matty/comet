//! CodexHarness integration tests against the fake app server in
//! `tests/fixtures/fake_codex.rs` (no real `codex` binary involved).

use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::{path::Path, process::Command};

use futures::StreamExt;
use tokio::sync::{mpsc, oneshot};

use comet_harness::{
    CancellationToken, CodexHarness, Harness, HarnessError, RunControls, SteerMessage,
};
use comet_proto::{
    AgentEvent, ApprovalDecision, ApprovalRequest, DiagnosticSeverity, DoneStatus, FileOperation,
    HarnessId, NoticeKind, NoticeSeverity, ReasoningLevel, RunRequest, RuntimeMode, SandboxLevel,
    ToolCall, UserInputAnswer,
};

/// The `fake-codex` bin target, built by cargo alongside this test.
fn fixture_path() -> &'static str {
    env!("CARGO_BIN_EXE_fake-codex")
}

fn init_stall_fixture_path() -> &'static str {
    env!("CARGO_BIN_EXE_fake-codex-init-stall")
}

fn init_crash_fixture_path() -> &'static str {
    env!("CARGO_BIN_EXE_fake-codex-init-crash")
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
        harness: None,
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

fn git(cwd: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap_or_else(|error| panic!("git {args:?} starts: {error}"));
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn linked_worktree(branch: &str) -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let temp = tempfile::tempdir().expect("temporary repository");
    let checkout = temp.path().join("checkout");
    std::fs::create_dir(&checkout).expect("checkout directory");
    git(&checkout, &["init"]);
    git(&checkout, &["config", "user.email", "test@example.com"]);
    git(&checkout, &["config", "user.name", "Comet test"]);
    std::fs::write(checkout.join("seed"), "seed\n").expect("seed file");
    git(&checkout, &["add", "seed"]);
    git(&checkout, &["commit", "-m", "seed"]);
    let worktree = temp.path().join("linked-worktree");
    git(
        &checkout,
        &[
            "worktree",
            "add",
            "-b",
            branch,
            worktree.to_str().expect("worktree path"),
        ],
    );
    (temp, checkout, worktree)
}

fn done_error(events: &[AgentEvent]) -> String {
    events
        .iter()
        .find_map(|event| match event {
            AgentEvent::Done {
                error: Some(error), ..
            } => Some(error.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no policy observation: {events:#?}"))
}

/// Break caught: omitting the escalation notice or changing either preserved
/// policy makes a real linked worktree run misrepresent its effective access.
#[tokio::test]
async fn slashed_linked_worktree_escalation_is_visible_and_preserves_policy() {
    let (_temp, _checkout, worktree) = linked_worktree("feature/visible-sandbox");

    let (controls, _steer, _token) = controls("Yes");
    let mut req = request("scenario:echo-policy");
    req.cwd = worktree.display().to_string();
    req.runtime_mode = RuntimeMode::AutoAcceptEdits;
    let events = run_to_end(&harness(), req, controls).await;

    assert!(matches!(
        events.first(),
        Some(AgentEvent::SessionStarted { .. })
    ));
    assert_eq!(
        events.get(1),
        Some(&AgentEvent::Notice {
            kind: NoticeKind::Info,
            severity: NoticeSeverity::Warning,
            summary: "Sandbox access widened".into(),
            detail: Some(
                "This run can write anywhere on this machine, outside the workspace. Use a branch name without a slash to keep workspace-only write access.".into(),
            ),
            key: Some("codex-sandbox-escalated".into()),
        }),
        "the widening notice must follow SessionStarted: {events:#?}"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AgentEvent::Notice { .. }))
            .count(),
        1,
        "the widening notice must be emitted exactly once: {events:#?}"
    );
    assert!(
        done_error(&events).contains(
            "thread=on-request turn=on-request sandbox=danger-full-access policy=dangerFullAccess"
        ),
        "the sandbox must widen without changing runtime-mode-derived policy: {events:#?}"
    );
}

/// Break caught: widening outside the slash-named linked-worktree case would
/// silently grant broader access to an ordinary checkout or request.
#[tokio::test]
async fn sandbox_escalation_is_absent_outside_the_linked_slash_branch_case() {
    let (_temp, checkout, slashed) = linked_worktree("feature/visible-sandbox");
    for (name, cwd, sandbox) in [
        ("main checkout", checkout, SandboxLevel::WorkspaceWrite),
        ("other sandbox", slashed, SandboxLevel::ReadOnly),
    ] {
        let (run_controls, _steer, _token) = controls("Yes");
        let mut req = request("scenario:echo-policy");
        req.cwd = cwd.display().to_string();
        req.runtime_mode = RuntimeMode::AutoAcceptEdits;
        req.sandbox = sandbox;
        let events = run_to_end(&harness(), req, run_controls).await;
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, AgentEvent::Notice { .. })),
            "{name} must not announce a sandbox escalation: {events:#?}"
        );
        let expected = match sandbox {
            SandboxLevel::WorkspaceWrite => "workspace-write policy=workspaceWrite",
            SandboxLevel::ReadOnly => "read-only policy=readOnly",
            SandboxLevel::DangerFullAccess => unreachable!(),
        };
        assert!(
            done_error(&events).contains(expected),
            "{name} must preserve its requested sandbox: {events:#?}"
        );
    }

    let non_worktree = tempfile::tempdir().expect("non-worktree directory");
    let (run_controls, _steer, _token) = controls("Yes");
    let mut req = request("scenario:echo-policy");
    req.cwd = non_worktree.path().display().to_string();
    req.runtime_mode = RuntimeMode::AutoAcceptEdits;
    let events = run_to_end(&harness(), req, run_controls).await;
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::Notice { .. })),
        "a non-worktree directory must not announce a sandbox escalation: {events:#?}"
    );
    assert!(
        done_error(&events).contains("workspace-write policy=workspaceWrite"),
        "a non-worktree directory must retain workspace-write: {events:#?}"
    );

    let (_temp, _checkout, plain) = linked_worktree("plain-branch");
    let (run_controls, _steer, _token) = controls("Yes");
    let mut req = request("scenario:echo-policy");
    req.cwd = plain.display().to_string();
    req.runtime_mode = RuntimeMode::AutoAcceptEdits;
    let events = run_to_end(&harness(), req, run_controls).await;
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::Notice { .. })),
        "a plain branch must not announce a sandbox escalation: {events:#?}"
    );
    assert!(
        done_error(&events).contains("workspace-write policy=workspaceWrite"),
        "a plain branch must retain workspace-write: {events:#?}"
    );
}

/// Break caught: dropping the setup timeout leaves this collection waiting on
/// the fixture's live stdout forever. Collecting to EOF also proves the child
/// was reaped rather than merely sending `Done` before cleanup.
#[tokio::test]
async fn codex_startup_timeout_is_terminal_private_and_reaped() {
    let (controls, _steer, _token) = controls("Yes");
    let harness = CodexHarness::new()
        .with_executable(init_stall_fixture_path())
        .with_startup_timeout(Duration::from_millis(50))
        .with_graces(Duration::from_millis(10), Duration::from_millis(10));
    let stream = harness
        .run(request("scenario:happy"), controls)
        .await
        .expect("stall fixture starts");
    let events = tokio::time::timeout(
        Duration::from_secs(2),
        stream
            .map(|event| event.expect("normalized event"))
            .collect::<Vec<_>>(),
    )
    .await
    .expect("startup timeout must close the stream after reaping the child");

    assert_eq!(
        events,
        vec![AgentEvent::Done {
            status: DoneStatus::Errored,
            result: None,
            error: Some(
                "Codex didn't finish starting. Open Codex in a terminal to sign in, then try again."
                    .into(),
            ),
            session_id: None,
        }]
    );
    assert!(
        !format!("{events:?}").contains("TASK81_INIT_STALL_PRIVATE_DIAGNOSTIC"),
        "owner-local stderr leaked into the transcript event: {events:?}"
    );
}

/// Break caught: forwarding the JSON-RPC error or stderr tail makes the
/// transcript expose process detail instead of Comet's actionable copy.
#[tokio::test]
async fn codex_startup_failure_keeps_process_detail_out_of_the_transcript() {
    let (controls, _steer, _token) = controls("Yes");
    let harness = CodexHarness::new()
        .with_executable(init_crash_fixture_path())
        .with_startup_timeout(Duration::from_secs(1))
        .with_graces(Duration::from_millis(10), Duration::from_millis(10));
    let stream = harness
        .run(request("scenario:happy"), controls)
        .await
        .expect("crash fixture starts");
    let events = tokio::time::timeout(
        Duration::from_secs(2),
        stream
            .map(|event| event.expect("normalized event"))
            .collect::<Vec<_>>(),
    )
    .await
    .expect("startup failure must close the stream after reaping the child");

    assert_eq!(
        events,
        vec![AgentEvent::Done {
            status: DoneStatus::Errored,
            result: None,
            error: Some(
                "Codex couldn't start. Check that Codex is signed in, then try again.".into(),
            ),
            session_id: None,
        }]
    );
    let visible = format!("{events:?}");
    assert!(!visible.contains("TASK81_INIT_CRASH_PRIVATE_DIAGNOSTIC"));
    assert!(!visible.contains("73"), "exit code leaked: {visible}");
}

/// Break caught: retaining the startup timer after `SessionStarted` turns a
/// legitimate long-running turn into a false startup failure.
///
/// The budget must cover process spawn, not just the handshake, so it cannot be
/// tight: Windows spawn alone is 10-30ms and a 25ms budget failed 2-in-12 here
/// (D89). What proves the property is the observation window exceeding the
/// startup timeout, not the timeout being small.
#[tokio::test]
async fn codex_startup_timeout_never_applies_to_an_active_turn() {
    let (controls, _steer, token) = controls("Yes");
    let mut stream = CodexHarness::new()
        .with_executable(fixture_path())
        .with_startup_timeout(Duration::from_millis(500))
        .run(request("scenario:interrupt"), controls)
        .await
        .expect("run starts");

    let started = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("startup completes")
        .expect("stream stays open")
        .expect("normalized event");
    assert!(matches!(started, AgentEvent::SessionStarted { .. }));

    let deadline = tokio::time::Instant::now() + Duration::from_millis(2000);
    loop {
        match tokio::time::timeout_at(deadline, stream.next()).await {
            Err(_) => break,
            Ok(Some(Ok(event))) => assert!(
                !matches!(event, AgentEvent::Done { .. }),
                "startup deadline survived into the active turn: {event:?}"
            ),
            Ok(Some(Err(error))) => panic!("active turn failed: {error}"),
            Ok(None) => panic!("active turn ended before explicit interruption"),
        }
    }

    token.cancel();
    let tail = tokio::time::timeout(
        Duration::from_secs(2),
        stream
            .map(|event| event.expect("normalized event"))
            .collect::<Vec<_>>(),
    )
    .await
    .expect("explicit interruption completes");
    assert!(tail.iter().any(|event| matches!(
        event,
        AgentEvent::Done {
            status: DoneStatus::Interrupted,
            ..
        }
    )));
    assert!(!tail.iter().any(|event| matches!(
        event,
        AgentEvent::Done {
            status: DoneStatus::Errored,
            ..
        }
    )));
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
        is_error: true,
        diff: None,
        diff_ref: None,
        diff_stats: None,
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
        is_error: false,
        diff: None,
        diff_ref: None,
        diff_stats: None,
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
        is_error: true,
        diff: None,
        diff_ref: None,
        diff_stats: None,
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
        is_error: false,
        diff: None,
        diff_ref: None,
        diff_stats: None,
    }));

    // No `todoList` assertion: the fixture no longer sends one, because no
    // supported codex-cli does. Codex's plan reaches the checklist through
    // `turn/plan/updated`, covered by `plan_tests` in
    // `crates/harness/src/codex/normalize.rs`.

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
                    prompt_tokens: 42,
                    output_tokens: 7,
                    context_window: Some(258_400),
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

#[tokio::test]
async fn codex_file_change_without_complete_sources_carries_no_diff() {
    let (controls, _steer, _token) = controls("Yes");
    let mut request = request("scenario:happy");
    request.cwd = "/tmp".into();
    request.model_options.insert(
        "serviceTier".into(),
        serde_json::Value::String("fast".into()),
    );
    let events = run_to_end(&harness(), request, controls).await;
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::ToolResult {
            id,
            diff: None,
            diff_ref: None,
            diff_stats: None,
            ..
        } if id == "f1"
    )));
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

/// The rejected-steer response resolves outside the ordered notification
/// queue. A late old-turn delta therefore must be dropped before `Steered`
/// opens the fallback assistant entry.
#[tokio::test]
async fn rejected_steer_drops_an_orphan_queued_before_the_follow_up() {
    let (controls, steer, _token) = controls("Yes");
    steer
        .send(SteerMessage {
            prompt: "redirect please".into(),
            message_id: None,
        })
        .await
        .expect("steer queued");
    let events = run_to_end(&harness(), request("scenario:steer-race-orphan"), controls).await;

    let steered_pos = events
        .iter()
        .position(|event| matches!(event, AgentEvent::Steered { .. }))
        .expect("fallback Steered event: {events:?}");
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::TextDelta { text } if text == "orphaned")),
        "old-turn content must not appear before or after Steered: {events:?}"
    );
    let post_steer_deltas: Vec<_> = events[steered_pos + 1..]
        .iter()
        .filter_map(|event| match event {
            AgentEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        post_steer_deltas,
        vec!["fallback"],
        "only follow-up content may enter the assistant entry after Steered: {events:?}"
    );
}

#[tokio::test]
async fn second_steer_targets_the_first_follow_up_before_its_started_notice() {
    let (controls, steer, _token) = controls("Yes");
    steer
        .send(SteerMessage {
            prompt: "redirect please".into(),
            message_id: None,
        })
        .await
        .expect("first steer queued");
    let mut stream = harness()
        .run(request("scenario:steer-race-second-steer"), controls)
        .await
        .expect("run starts");
    let events = tokio::time::timeout(Duration::from_secs(10), async move {
        let mut events = Vec::new();
        let mut second_sent = false;
        while let Some(event) = stream.next().await {
            let event = event.expect("stream event");
            if !second_sent && matches!(event, AgentEvent::Done { .. }) {
                steer
                    .send(SteerMessage {
                        prompt: "redirect again".into(),
                        message_id: None,
                    })
                    .await
                    .expect("second steer queued after first completion");
                second_sent = true;
            }
            events.push(event);
        }
        events
    })
    .await
    .expect("run finishes");

    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AgentEvent::Steered { .. }))
            .count(),
        2,
        "fallback then native second steer: {events:?}"
    );
    assert!(events.contains(&AgentEvent::TextDelta {
        text: "second-steer".into()
    }));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::TextDelta { text } if text == "orphaned")),
        "the first follow-up owns no old-turn content: {events:?}"
    );
}

#[tokio::test]
async fn a_follow_up_without_started_still_emits_steered_and_finishes() {
    let (controls, steer, _token) = controls("Yes");
    steer
        .send(SteerMessage {
            prompt: "redirect please".into(),
            message_id: None,
        })
        .await
        .expect("steer queued");
    let events = run_to_end(
        &harness(),
        request("scenario:steer-race-missing-start"),
        controls,
    )
    .await;

    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AgentEvent::Steered { .. }))
            .count(),
        1,
        "the follow-up response owns its lifecycle even without turn/started: {events:?}"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AgentEvent::Done { .. }))
            .count(),
        2,
        "both turns terminate without waiting for turn/started: {events:?}"
    );
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

/// A native approval `cancel` is not a clean completion. Unlike the Stop
/// control, it does not trip Comet's interrupt token; the adapter has to read
/// Codex's own `turn.status: "interrupted"` from the terminal notification.
#[tokio::test]
async fn approval_cancel_maps_the_provider_interrupted_status() {
    let (steer_tx, steer_rx) = mpsc::channel(1);
    drop(steer_tx);
    let controls = RunControls {
        request_input: Box::new(|_questions| {
            let (_tx, rx) = oneshot::channel::<Vec<UserInputAnswer>>();
            rx
        }),
        request_approval: Box::new(|_approval| {
            let (tx, rx) = oneshot::channel();
            let _ = tx.send(ApprovalDecision::DenyAndInterrupt {
                message: "stop this turn".into(),
            });
            rx
        }),
        steering: steer_rx,
        interrupt: CancellationToken::new(),
    };
    let mut req = request("scenario:cancel-approval");
    req.runtime_mode = RuntimeMode::ApprovalRequired;
    let events = run_to_end(&harness(), req, controls).await;

    assert_eq!(
        events.last(),
        Some(&AgentEvent::Done {
            status: DoneStatus::Interrupted,
            result: None,
            error: None,
            session_id: Some("th-1".into()),
        }),
        "the provider's interrupted terminal must not become a clean completion: {events:?}"
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

/// Whether a pid still names a live process. Existence-only: neither branch
/// sends a real signal, and both treat "cannot even ask" as "gone" — the
/// conservative direction for a test that means to prove absence.
#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    // SAFETY: signal 0 is `kill(2)`'s documented existence probe — it delivers
    // nothing, it only reports whether the pid (or, negative, the process
    // group) could be signaled.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[cfg(windows)]
fn process_is_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };
    // SAFETY: plain read-only handle open plus a zero-timeout wait, both
    // documented, non-mutating queries; the handle is closed on every path.
    unsafe {
        let handle = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
        if handle.is_null() {
            return false;
        }
        let status = WaitForSingleObject(handle, 0);
        CloseHandle(handle);
        status == WAIT_TIMEOUT
    }
}

/// Poll rather than sleep-then-check: reaping a process tree can race the
/// escalation task's own teardown, and a bare sleep either flakes under load
/// or (D89) has its budget quietly outgrown by one. This gives a real
/// failure message instead of a timeout with no evidence attached.
async fn wait_for_process_exit(pid: u32, timeout: Duration) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if !process_is_alive(pid) {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "pid {pid} was still alive after {}ms",
                timeout.as_millis()
            ));
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// D46: the direct fake exiting proves nothing about a grandchild the
/// provider itself spawned — `unresponsive_child_is_reaped_with_interrupted_done`
/// above only ever had one process to reap. This fixture (`wedge_with_child`
/// in `fake_codex.rs`) spawns a real OS grandchild, records both pids to a
/// file, then hangs exactly like `wedge`. After cancellation, both pids must
/// be gone — not just the harness's own Done event, which today fires the
/// moment the direct child is reaped regardless of what it left behind.
#[tokio::test]
async fn cancellation_reaps_a_provider_owned_grandchild() {
    let harness = CodexHarness::new()
        .with_executable(fixture_path())
        .with_graces(Duration::from_millis(100), Duration::from_millis(500));
    let (controls, _steer, token) = controls("Yes");

    let pid_file = std::env::temp_dir().join(format!(
        "comet-d46-grandchild-pids-{}-{}.txt",
        std::process::id(),
        uuid_ish()
    ));
    let _ = std::fs::remove_file(&pid_file);

    let mut stream = harness
        .run(
            request(&format!("scenario:wedge-with-child|{}", pid_file.display())),
            controls,
        )
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
        }),
        "{events:?}"
    );

    // Written before the fixture ever emits the "working" delta that
    // triggers cancellation above, so it is guaranteed present by now.
    let recorded = std::fs::read_to_string(&pid_file).expect("fixture recorded both pids");
    let _ = std::fs::remove_file(&pid_file);
    let mut pids = recorded.lines();
    let child_pid: u32 = pids
        .next()
        .expect("direct child pid line")
        .trim()
        .parse()
        .expect("direct child pid parses");
    let grandchild_pid: u32 = pids
        .next()
        .expect("grandchild pid line")
        .trim()
        .parse()
        .expect("grandchild pid parses");

    wait_for_process_exit(child_pid, Duration::from_secs(5))
        .await
        .unwrap_or_else(|msg| panic!("direct child (fixture) {msg}"));

    // The actual claim this row exists to prove: a provider-owned descendant
    // does not outlive the cancelled run just because the direct child did.
    wait_for_process_exit(grandchild_pid, Duration::from_secs(5))
        .await
        .unwrap_or_else(|msg| {
            panic!(
                "provider-owned grandchild (pid {grandchild_pid}) {msg} — \
                 cancellation reaped the fixture but leaked its child"
            )
        });
}

/// A cheap, dependency-free per-process disambiguator for the pid-file path —
/// parallel `cargo nextest` runs already separate by `std::process::id()`,
/// this only needs to also separate repeated runs within one process (a
/// retried test).
fn uuid_ish() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed)
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

// ---------------------------------------------------------------------------
// D45: reusable provider lifecycle fault primitives
// (docs/debt/D45-provider-lifecycle-fault-matrix.md)
// ---------------------------------------------------------------------------

fn thread_setup_stall_fixture_path() -> &'static str {
    env!("CARGO_BIN_EXE_fake-codex-thread-setup-stall")
}

/// `codex_startup_timeout_is_terminal_private_and_reaped` (above) proves the
/// timeout covers `initialize`. This proves it also covers `thread/start` —
/// the SECOND await inside `run_session`'s `setup` future — which nothing
/// exercised before this fixture: before it existed, changing the timeout to
/// wrap only the first await would have left every other test in this suite
/// green.
#[tokio::test]
async fn codex_startup_timeout_covers_thread_setup_not_just_initialize() {
    let (controls, _steer, _token) = controls("Yes");
    let harness = CodexHarness::new()
        .with_executable(thread_setup_stall_fixture_path())
        .with_startup_timeout(Duration::from_millis(50))
        .with_graces(Duration::from_millis(10), Duration::from_millis(10));
    let stream = harness
        .run(request("scenario:happy"), controls)
        .await
        .expect("stall fixture starts");
    let events = tokio::time::timeout(
        Duration::from_secs(2),
        stream
            .map(|event| event.expect("normalized event"))
            .collect::<Vec<_>>(),
    )
    .await
    .expect("startup timeout must close the stream after reaping the child");

    assert_eq!(
        events,
        vec![AgentEvent::Done {
            status: DoneStatus::Errored,
            result: None,
            error: Some(
                "Codex didn't finish starting. Open Codex in a terminal to sign in, then try again."
                    .into(),
            ),
            session_id: None,
        }]
    );
    assert!(
        !format!("{events:?}").contains("D45_THREAD_SETUP_STALL_PRIVATE_DIAGNOSTIC"),
        "owner-local stderr leaked into the transcript event: {events:?}"
    );
}

/// Runs `scenario:crash-mid-turn` until `describe_exit`'s status settles to
/// "exit code 66", bounded rather than accepting whichever status the first
/// attempt happens to read.
///
/// `Incoming::Eof` (this fixture's stdout closing) and the OS actually
/// marking the process exited (what `child.try_wait()` reads, right after
/// `Eof`, in `run_session`'s post-loop teardown) are two independent
/// observations. Under `cargo nextest`'s parallel load a single attempt
/// reliably read "still running" in a five-test batch on this machine even
/// though the fixture had already called `exit(66)`; a single isolated run
/// just as reliably read "exit code 66". Per `.agents/workflows/verify.md`'s
/// "poll the condition with a generous deadline instead", this polls by
/// re-running the whole scenario — each run spawns an independent process, so
/// a retry is a fresh trial of the same race, not a re-read of stale state —
/// rather than accepting either outcome as a matter of test-authoring
/// convenience. The deadline only ever bounds a FAILURE: on this machine
/// every observed run has settled to "exit code 66" within the first two
/// attempts.
async fn crash_mid_turn_settled_error() -> String {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut attempts = 0u32;
    loop {
        attempts += 1;
        let (controls, _steer, _token) = controls("Yes");
        let harness = CodexHarness::new().with_executable(fixture_path());
        let events = run_to_end(&harness, request("scenario:crash-mid-turn"), controls).await;
        let error = events
            .iter()
            .find_map(|e| match e {
                AgentEvent::Done {
                    status: DoneStatus::Errored,
                    error,
                    ..
                } => error.clone(),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no Errored Done in {events:?}"));
        let settled = error.contains("exit code 66");
        if settled || tokio::time::Instant::now() >= deadline {
            assert!(
                settled,
                "describe_exit never settled to \"exit code 66\" across {attempts} \
                 attempts within a 10s deadline (last message: {error}) — if this \
                 fires, it is evidence the Eof-vs-try_wait race is worse than a \
                 first-attempt hiccup and worth its own debt row, not a widened \
                 assertion"
            );
            return error;
        }
    }
}

/// D45 primitive: stderr_then_exit, mid-turn (not at startup). Uses a harness
/// with NO `CODEX_HOME` set (via `crash_mid_turn_settled_error`), deliberately
/// unlike `harness()`: with it set, `fake_codex.rs::fill_stderr()` writes
/// ~1MB of undelimited `x` bytes to stderr before the handshake even starts,
/// which would still be sitting in the stderr reader's line buffer (no
/// newline yet) when this scenario's own diagnostic line supplies the first
/// `\n` — `StderrTail`'s 700-byte-per-line cap would then keep the FRONT of
/// that combined line (`x` padding), not this scenario's own text. That
/// collision is an artifact of the OTHER fixture's own discovery-only setup,
/// not a fault this primitive means to express, so this test sidesteps it
/// rather than fold a workaround into it.
#[tokio::test]
async fn codex_mid_turn_crash_reports_exit_code_and_a_bounded_stderr_excerpt() {
    let error = crash_mid_turn_settled_error().await;
    assert!(
        error.contains("boom: fake codex crashed mid-turn"),
        "expected the bounded stderr excerpt, got: {error}"
    );
}

/// D45 primitive: partial_line — "stdout closes halfway through a frame".
#[tokio::test]
async fn codex_partial_stdout_frame_is_malformed_not_silently_dropped() {
    let (controls, _steer, _token) = controls("Yes");
    let events = run_to_end(&harness(), request("scenario:partial-frame"), controls).await;

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
        vec![("unparseable".to_string(), DiagnosticSeverity::Malformed)],
        "{events:?}"
    );
    assert!(
        matches!(
            events.last(),
            Some(AgentEvent::Done {
                status: DoneStatus::Errored,
                ..
            })
        ),
        "{events:?}"
    );
}

/// D45 primitive: "stdin breaks while Comet writes a decision" — an approval
/// request the provider never sticks around to see answered.
#[tokio::test]
async fn codex_provider_dying_before_an_approval_decision_still_ends_bounded() {
    let (controls, _steer, _token) = controls("Yes");
    let events = run_to_end(&harness(), request("scenario:die-after-approval"), controls).await;

    assert!(
        !events.iter().any(|e| matches!(e, AgentEvent::Error { .. })),
        "{events:?}"
    );
    let error = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::Done {
                status: DoneStatus::Errored,
                error,
                ..
            } => error.clone(),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no Errored Done in {events:?}"));
    assert_ne!(
        error, "unknown scenario",
        "the fixture's die-after-approval branch never ran: {events:?}"
    );
    assert!(error.contains("exited unexpectedly"), "{error}");
}

/// D135 regression: a duplicate `turn/completed` notification produces one
/// terminal event, even when an unrelated notification arrives between them.
///
/// Removing the completion arm's `TurnRouter::is_completed` gate must make
/// this receive two `Done` events from the real subprocess fixture.
#[tokio::test]
async fn codex_duplicate_turn_completed_emits_one_done() {
    let (controls, _steer, _token) = controls("Yes");
    let events = run_to_end(
        &harness(),
        request("scenario:duplicate-completion"),
        controls,
    )
    .await;

    let done_count = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::Done { .. }))
        .count();
    assert_eq!(
        done_count, 1,
        "expected one Done event for a duplicated turn/completed, got {done_count} in {events:?}"
    );
    // The terminal result remains a normal completion.
    assert!(
        events.iter().all(|e| !matches!(
            e,
            AgentEvent::Done {
                status: DoneStatus::Errored,
                ..
            }
        )),
        "{events:?}"
    );
}

/// D45 primitive: late_reply — "delayed until after cancellation". The
/// provider's own `turn/completed` beats `turn/aborted` to the wire, arriving
/// after the harness already committed to `interrupted = true`.
#[tokio::test]
async fn codex_completion_delayed_past_cancellation_still_reports_interrupted() {
    let harness = CodexHarness::new()
        .with_executable(fixture_path())
        .with_codex_home(logged_in_home())
        // Comfortably longer than the fixture's own 100ms delay, so the kill
        // escalation this deliberately races against never fires.
        .with_graces(Duration::from_secs(2), Duration::from_secs(2));
    let (controls, _steer, token) = controls("Yes");
    let mut stream = harness
        .run(
            request("scenario:late-completion-after-interrupt"),
            controls,
        )
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
    .expect("run settled in time");

    assert_eq!(
        events.last(),
        Some(&AgentEvent::Done {
            status: DoneStatus::Interrupted,
            result: None,
            error: None,
            session_id: Some("th-1".into()),
        }),
        "a late-arriving completion must not overrule an already-committed \
         interrupt: {events:?}"
    );
}

// ---------------------------------------------------------------------------
// D48: a selected slice of the event-order state space
// (docs/debt/D48-provider-state-sequences.md)
//
// D45 (above) supplied reusable fault primitives; this section selects
// orderings not already covered by a named scenario or by one of D45's
// primitives, each tied to one of D48's own invariants (request ids resolve
// at most once, no normal event follows a terminal result, cancellation
// settles, pending approvals fail closed, the child is reaped). This is a
// selection, not an enumeration — see the PR description for orderings that
// were considered and left out, and why.
// ---------------------------------------------------------------------------

/// "The child is reaped" under a THIRD way a run can end, distinct from both
/// a normal `Done` and an explicit `interrupt.cancel()`: the consumer simply
/// drops the event stream mid-turn (the app closing, a dropped engine-side
/// subscription, …). `run_session`'s main `tokio::select!` has exactly one
/// arm for this — `_ = event_tx.closed() => break 'main` — and nothing else
/// in this suite exercises it: every other test either drains to `Done` or
/// cancels the interrupt token first. `shutdown_child`/`tree.terminate()` run
/// unconditionally after the loop, for whatever reason it ended, so this
/// proves that unconditional cleanup actually covers this exit too, not just
/// the two already-tested ones.
///
/// Reuses `wedge-with-child` (D46) rather than plain `wedge`: this needs a
/// pid to poll, and `wedge` records none of its own.
#[tokio::test]
async fn dropping_the_consumer_stream_still_reaps_the_child_and_its_grandchild() {
    let harness = CodexHarness::new()
        .with_executable(fixture_path())
        .with_codex_home(logged_in_home())
        .with_graces(Duration::from_millis(100), Duration::from_millis(300));
    let (controls, _steer, _token) = controls("Yes");

    let pid_file = std::env::temp_dir().join(format!(
        "comet-d48-consumer-drop-pids-{}-{}.txt",
        std::process::id(),
        uuid_ish()
    ));
    let _ = std::fs::remove_file(&pid_file);

    let mut stream = harness
        .run(
            request(&format!("scenario:wedge-with-child|{}", pid_file.display())),
            controls,
        )
        .await
        .expect("run starts");

    // Let the turn actually reach the in-flight state (both pids recorded,
    // per wedge_with_child's own doc comment, written before this delta) —
    // before abandoning the stream.
    loop {
        match tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("delta arrives before the wedge sleep")
        {
            Some(Ok(AgentEvent::TextDelta { text })) if text == "working" => break,
            Some(Ok(_)) => continue,
            other => panic!("unexpected event before drop: {other:?}"),
        }
    }

    // The abrupt-disconnect path: never read a Done, never cancel the token.
    drop(stream);

    let recorded = std::fs::read_to_string(&pid_file).expect("fixture recorded both pids");
    let _ = std::fs::remove_file(&pid_file);
    let mut pids = recorded.lines();
    let child_pid: u32 = pids
        .next()
        .expect("direct child pid line")
        .trim()
        .parse()
        .expect("direct child pid parses");
    let grandchild_pid: u32 = pids
        .next()
        .expect("grandchild pid line")
        .trim()
        .parse()
        .expect("grandchild pid parses");

    wait_for_process_exit(child_pid, Duration::from_secs(5))
        .await
        .unwrap_or_else(|msg| {
            panic!("direct child (fixture) {msg} after the consumer dropped the stream")
        });
    wait_for_process_exit(grandchild_pid, Duration::from_secs(5))
        .await
        .unwrap_or_else(|msg| {
            panic!(
                "provider-owned grandchild (pid {grandchild_pid}) {msg} — \
                 dropping the consumer stream leaked it"
            )
        });
}

/// A JSON-RPC response and its own related notifications may arrive in
/// either order (`TurnRouter`'s own doc comment says so,
/// `crates/harness/src/codex/mod.rs`), and `turn_router_never_revives_completed_turns`
/// (same file) already proves `TurnRouter` tolerates it in isolation. What
/// that unit test cannot show is whether the surrounding plumbing does:
/// `start_turn`'s ack is a bare `.await` BEFORE `run_session`'s
/// `tokio::select!` loop even starts, so anything the wire sends earlier
/// sits unread on the `incoming` mpsc channel the whole time. This proves
/// that buffering is transparent end to end: the consumer sees the exact
/// ordered sequence an ack-first happy path would produce — nothing lost,
/// nothing duplicated, nothing reordered. This race is safe by construction,
/// not accidentally so.
#[tokio::test]
async fn turn_events_streamed_ahead_of_their_own_ack_are_not_lost_or_reordered() {
    let (controls, _steer, _token) = controls("Yes");
    let events = run_to_end(&harness(), request("scenario:stream-before-ack"), controls).await;

    assert!(
        matches!(events.first(), Some(AgentEvent::SessionStarted { .. })),
        "{events:?}"
    );
    assert_eq!(
        events.get(1),
        Some(&AgentEvent::TextDelta {
            text: "before".into()
        }),
        "{events:?}"
    );
    assert_eq!(
        events.get(2),
        Some(&AgentEvent::TextDelta {
            text: "-ack".into()
        }),
        "{events:?}"
    );
    assert_eq!(
        events.get(3),
        Some(&AgentEvent::Done {
            status: DoneStatus::Completed,
            result: None,
            error: None,
            session_id: Some("th-1".into()),
        }),
        "{events:?}"
    );
    assert_eq!(events.len(), 4, "extra or missing events: {events:?}");
}

#[tokio::test]
async fn codex_orphaned_turn_events_after_done_are_dropped_but_session_notices_survive() {
    let (controls, _steer, _token) = controls("Yes");
    let events = run_to_end(
        &harness(),
        request("scenario:orphan-after-completion"),
        controls,
    )
    .await;

    assert!(
        matches!(events.first(), Some(AgentEvent::SessionStarted { .. })),
        "{events:?}"
    );
    assert_eq!(
        events.get(1),
        Some(&AgentEvent::TextDelta {
            text: "done".into()
        }),
        "{events:?}"
    );
    assert_eq!(
        events.get(2),
        Some(&AgentEvent::Done {
            status: DoneStatus::Completed,
            result: None,
            error: None,
            session_id: Some("th-1".into()),
        }),
        "{events:?}"
    );
    assert_eq!(
        events.get(3),
        Some(&AgentEvent::Notice {
            kind: NoticeKind::RateLimit,
            severity: NoticeSeverity::Warning,
            summary: "Codex usage is at 85% of its limit".into(),
            detail: None,
            key: Some("rateLimit".into()),
        }),
        "the post-completion rate-limit notice is session-scoped: {events:?}"
    );
    assert_eq!(events.len(), 4, "extra or missing events: {events:?}");
    assert!(
        !events.iter().any(|event| matches!(
            event,
            AgentEvent::TextDelta { text } if text == "orphaned"
        )),
        "the orphaned turn delta must not follow Done: {events:?}"
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
            // `method` nor `id`. Both are Malformed and they no longer share
            // one sentinel (D9) — an operator reading `unparseable x412` could
            // not tell a CLI writing log noise to stdout from one whose
            // message shape moved.
            ("unparseable".to_string(), DiagnosticSeverity::Malformed),
            (
                "unparseable/not-a-message".to_string(),
                DiagnosticSeverity::Malformed,
            ),
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
// D43: the real process-launch contract (docs/debt/D43-fake-provider-launch-contract.md)
// ---------------------------------------------------------------------------

/// D43's headline Codex finding: "it never checks that the executable was
/// invoked with `app-server`." `tests/fixtures/fake_codex.rs`'s
/// `check_app_server_arg` now fails loudly, before the handshake, if
/// `"app-server"` is missing from argv — so this (and every other scenario
/// and discovery test in this file) is proof that a real run still carries
/// it. Falsified by temporarily changing `codex::run_launch`'s
/// `args: vec!["app-server".into()]` to `args: vec![]` and rerunning: the
/// fixture exited before ever answering `initialize`, and the run ended
/// `Errored`/`"Codex couldn't start. Check that Codex is signed in, then try
/// again."` — quoted in the PR description. Restored before committing.
#[tokio::test]
async fn a_run_is_launched_as_codex_app_server() {
    let (controls, _steer, _token) = controls("Yes");
    // `resumed` runs no turn/thread param checks of its own — this test is
    // about argv reaching the child at all, not about the parameter
    // contract `happy_path_maps_deltas_items_usage_and_done` already covers.
    let events = run_to_end(&harness(), request("scenario:resumed"), controls).await;
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

/// D43's other Codex finding: "run scenarios inspect the `cwd` field sent
/// over JSON-RPC rather than the process's actual working directory. That is
/// weaker than command discovery's child-side cwd echo." `happy` and
/// `echo_policy` only ever check `thread_start_params`'s `cwd` VALUE, which
/// is `request.cwd` echoed straight back — it cannot disagree with what
/// Comet asked for, because both come from the same field, so neither can
/// catch `LaunchDescriptor::command`'s `current_dir` call being dropped or
/// broken. This instead asks the spawned child for its own real working
/// directory (`cwd_echo` in `fake_codex.rs`).
///
/// Falsified by temporarily deleting the `if let Some(cwd) = &self.cwd {
/// command.current_dir(cwd); }` block from
/// `LaunchDescriptor::command` (`crates/harness/src/launch.rs`) and
/// rerunning: the observed cwd fell back to this test binary's own working
/// directory instead of the requested probe directory — quoted in the PR
/// description. Restored before committing; the JSON-RPC `cwd` value alone
/// would not have caught this, since `thread_start_params` reads the same
/// `request.cwd` field regardless of whether the process spawn honors it.
#[tokio::test]
async fn the_real_child_process_starts_in_the_requested_directory() {
    let dir = std::env::temp_dir().join("comet-codex-launch-cwd-probe");
    std::fs::create_dir_all(&dir).expect("probe dir");
    let (controls, _steer, _token) = controls("Yes");
    let mut req = request("scenario:cwd-echo");
    req.cwd = dir.display().to_string();
    let events = run_to_end(&harness(), req, controls).await;
    let error = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::Done { error, .. } => error.clone(),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no Done carrying the observed cwd: {events:?}"));
    assert_eq!(
        error,
        format!("cwd={}", dir.display()),
        "the child's actual working directory must match the requested one"
    );
}

// ---------------------------------------------------------------------------
// Live discovery (slice 2.3)
// ---------------------------------------------------------------------------

/// Discovery must drain piped stderr before it fills. The fake writes one MiB
/// there before reading stdin, so an undrained command never reaches model/list.
#[tokio::test]
async fn model_discovery_drains_stderr() {
    tokio::time::timeout(Duration::from_secs(5), harness().models())
        .await
        .expect("a full stderr pipe must not block model discovery")
        .expect("models");
}

/// The slice's deliverable: a real spawn, a real `initialize` + `model/list`
/// round trip, and a merged catalog that says it is live. The fixture is a
/// paged form of the captured live reply, `codex/0.147.0/model-discovery`
/// frame 6, and pages by default — the real server returns all seven models
/// in one page and would never exercise the loop.
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

/// D72 (`docs/debt/README.md`): `pickers::default_model` just returns
/// `models.first()`, so whoever leads the merged catalog IS the default the
/// picker offers. The fixture flags `gpt-5.5` `isDefault: true` — a model that
/// is neither the curated flagship nor the first row the fake server serves —
/// so a merged catalog still led by `gpt-5.6-sol` would mean the live answer
/// was decoded and then ignored.
#[tokio::test]
async fn a_live_default_leads_the_merged_catalog_even_when_not_first() {
    let catalog = harness().models().await.expect("models");
    assert_eq!(catalog.source, comet_proto::CatalogSource::Live);
    assert_eq!(
        catalog.models.first().map(|m| m.id.as_str()),
        Some("gpt-5.5"),
        "the live isDefault row must lead the catalog, got {:?}",
        catalog.models.iter().map(|m| &m.id).collect::<Vec<_>>()
    );
    let ids: std::collections::HashSet<&str> =
        catalog.models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(
        ids.len(),
        catalog.models.len(),
        "reordering must not drop or duplicate a row"
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

/// A relative `CODEX_HOME` is real configuration — `env_dir` returns the
/// variable verbatim — and the child runs with its working directory set to the
/// temp directory. Handed on as-is, the parent would check `auth.json` here
/// while the CLI read a directory under the temp dir, and answer from its
/// logged-out fallback list with the result still labelled live.
#[tokio::test]
async fn a_relative_codex_home_reaches_the_child_as_an_absolute_one() {
    // Created inside the crate root, which is where cargo runs the test from,
    // so the relative spelling below is one the parent can resolve and the
    // child cannot.
    let home = tempfile::TempDir::new_in(".").expect("temp dir in crate root");
    std::fs::write(home.path().join("auth.json"), "{}").expect("auth.json");
    let relative = std::path::Path::new(".").join(
        home.path()
            .file_name()
            .expect("the temp dir has a final component"),
    );
    assert!(!relative.is_absolute(), "the fixture needs a relative home");

    let harness = harness().with_codex_home(&relative);
    let catalog = harness.models().await.expect("models");
    let echo = catalog
        .models
        .iter()
        .find(|m| m.id == "codex-home-echo")
        .expect("discovery ran and the fixture echoed its CODEX_HOME");
    let echoed = std::path::Path::new(&echo.label);
    assert!(
        echoed.is_absolute(),
        "the child got a relative home to resolve against its own cwd: {echoed:?}"
    );
    assert_eq!(
        echoed.canonicalize().ok(),
        home.path().canonicalize().ok(),
        "the child's home is not the directory the login check read"
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
/// model the account cannot use and misses three it has. The logged-out
/// success reply, `codex/0.147.0/model-discovery-logged-out` frame 6,
/// motivates skipping `model/list` without auth evidence. Nothing in the
/// envelope says so, so the only defence is not to ask.
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
