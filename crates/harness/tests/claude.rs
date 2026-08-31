//! ClaudeHarness integration tests against the fake CLI in
//! `tests/fixtures/fake_claude.rs` (no real `claude` binary involved).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use tokio::sync::{mpsc, oneshot};

use comet_harness::{
    CancellationToken, ClaudeHarness, Harness, HarnessError, RunControls, SteerMessage,
};
use comet_proto::{
    AgentEvent, ApprovalDecision, ApprovalRequest, ChecklistStatus, DiagnosticSeverity, DoneStatus,
    FileOperation, HarnessId, NoticeKind, NoticeSeverity, RunRequest, RuntimeMode, SubagentStatus,
    ToolCall, UserInputAnswer, UserInputQuestion,
};

/// The `fake-claude` bin target, built by cargo alongside this test.
fn fixture_path() -> &'static str {
    env!("CARGO_BIN_EXE_fake-claude")
}

fn harness() -> ClaudeHarness {
    ClaudeHarness::new().with_executable(fixture_path())
}

fn request(prompt: &str) -> RunRequest {
    RunRequest {
        prompt: prompt.into(),
        ..RunRequest::for_session(RuntimeMode::default())
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
    harness: &ClaudeHarness,
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

/// Controls whose `request_approval` hands each request to the test and waits
/// on a caller-supplied decision, rather than `controls()`'s always-drops
/// approver. Left as a sibling rather than a change to `controls()` — eight
/// existing tests depend on that helper's never-approves behaviour.
fn controls_with_approver(
    approver: impl Fn(ApprovalRequest) -> oneshot::Receiver<ApprovalDecision> + Send + Sync + 'static,
) -> (RunControls, mpsc::Sender<SteerMessage>, CancellationToken) {
    let (steer_tx, steer_rx) = mpsc::channel(8);
    let token = CancellationToken::new();
    let controls = RunControls {
        request_input: Box::new(|_| {
            let (_tx, rx) = oneshot::channel();
            rx
        }),
        request_approval: Box::new(approver),
        steering: steer_rx,
        interrupt: token.clone(),
    };
    (controls, steer_tx, token)
}

/// Concatenates every `AgentEvent::TextDelta`'s text, in event order.
fn text_of(events: &[AgentEvent]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn happy_path_normalizes_events_and_filters_subagents() {
    let (controls, _steer, _token) = controls("A");
    let events = run_to_end(&harness(), request("scenario:happy"), controls).await;

    // One SessionStarted despite the re-emitted init frame.
    let starts: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::SessionStarted {
                harness,
                model,
                tools,
                session_id,
                ..
            } => Some((harness, model, tools, session_id)),
            _ => None,
        })
        .collect();
    assert_eq!(starts.len(), 1, "init must be deduped: {events:?}");
    let (h, model, tools, session_id) = starts[0];
    assert_eq!(*h, HarnessId::ClaudeCode);
    assert_eq!(model, "claude-fable-5");
    assert_eq!(tools, &vec!["Bash".to_string(), "Read".to_string()]);
    assert_eq!(session_id, "sess-1");

    assert!(events.contains(&AgentEvent::ReasoningDelta {
        text: "pondering".into()
    }));
    assert!(events.contains(&AgentEvent::TextDelta {
        text: "Hello".into()
    }));

    // Subagent frames (parent_tool_use_id set) are filtered out entirely.
    assert!(
        !events.iter().any(|e| matches!(
            e,
            AgentEvent::TextDelta { text } if text.contains("SUBAGENT")
        )),
        "subagent delta leaked: {events:?}"
    );
    assert!(
        !events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolCall { id, .. } | AgentEvent::ToolResult { id, .. } if id == "sub-tool"
        )),
        "subagent tool frames leaked: {events:?}"
    );

    // But the coordinator's OWN view of the subagent — the four claimed
    // system/task_* frames, none of which carry a parent_tool_use_id — must
    // reach the event stream through the real spawn path, not just through
    // normalize.rs's unit tests.
    assert!(events.contains(&AgentEvent::SubagentStarted {
        task_id: "sub-1-task".into(),
        tool_use_id: "sub-1".into(),
        agent_type: "general-purpose".into(),
        description: "Read README and report first heading".into(),
        prompt: Some(
            "Read the README.md file in the current directory and report what the first heading is."
                .into()
        ),
    }));
    // task_progress: a live Running reading.
    assert!(events.contains(&AgentEvent::SubagentUpdated {
        task_id: "sub-1-task".into(),
        status: SubagentStatus::Running,
        activity: Some("Reading README.md".into()),
        summary: None,
        total_tokens: Some(19_215),
        duration_ms: Some(2_906),
        tool_uses: Some(1),
    }));
    // task_updated: status only, and task_notification: adds the answer and
    // usage the status-only reading was missing — both terminal, but the
    // notification adds detail, so both survive the material-transition
    // filter as distinct events.
    assert!(events.contains(&AgentEvent::SubagentUpdated {
        task_id: "sub-1-task".into(),
        status: SubagentStatus::Completed,
        activity: None,
        summary: None,
        total_tokens: None,
        duration_ms: None,
        tool_uses: None,
    }));
    assert!(events.contains(&AgentEvent::SubagentUpdated {
        task_id: "sub-1-task".into(),
        status: SubagentStatus::Completed,
        activity: None,
        summary: Some("Sandbox".into()),
        total_tokens: Some(20_044),
        duration_ms: Some(4_906),
        tool_uses: Some(1),
    }));
    // The resumed invocation (same task_id, new tool_use_id "sub-2") reaches
    // the stream with its OWN summary — proving `normalize.rs:505`'s
    // `subagent_progress.remove(&f.task_id)` on the second `task_started`
    // actually runs through a real spawn. Without it this terminal reading
    // would be compared against the first invocation's already-terminal one
    // (summary "Sandbox", both `Some`) and dropped as adding nothing new.
    assert!(events.contains(&AgentEvent::SubagentUpdated {
        task_id: "sub-1-task".into(),
        status: SubagentStatus::Completed,
        activity: None,
        summary: Some("The first heading is **Sandbox**.".into()),
        total_tokens: Some(19_111),
        duration_ms: Some(2_186),
        tool_uses: Some(0),
    }));

    // Typed tool decoding: Bash -> Exec, mcp__server__tool -> Mcp.
    assert!(events.contains(&AgentEvent::ToolCall {
        id: "tool-1".into(),
        call: ToolCall::Exec {
            command: "ls -la".into()
        },
    }));
    assert!(events.contains(&AgentEvent::ToolCall {
        id: "tool-2".into(),
        call: ToolCall::Mcp {
            server: "linear".into(),
            tool: "search".into(),
            input: Some(serde_json::json!({"q": "bug"})),
        },
    }));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::AssistantMessageCompleted { .. }))
    );
    assert!(events.contains(&AgentEvent::ToolResult {
        id: "tool-1".into(),
        is_error: false,
        diff: None,
        diff_ref: None,
        diff_stats: None,
    }));
    assert!(events.contains(&AgentEvent::ToolResult {
        id: "tool-2".into(),
        is_error: true,
        diff: None,
        diff_ref: None,
        diff_stats: None,
    }));

    // Informational rate-limit frames stay quiet.
    assert!(!events.iter().any(|e| matches!(e, AgentEvent::Error { .. })));

    // 10 uncached + 34,932 cache-read + 75 cache-written. Asserting the sum
    // rather than `input_tokens` is the point: the CLI reports the prompt in
    // three parts and only their total is the occupancy.
    assert!(events.contains(&AgentEvent::Usage {
        prompt_tokens: 35_017,
        output_tokens: 20,
        context_window: Some(200_000),
    }));
    assert_eq!(
        events.last(),
        Some(&AgentEvent::Done {
            status: DoneStatus::Completed,
            result: Some("done!".into()),
            error: None,
            session_id: Some("sess-1".into()),
        })
    );
}

/// `normalize.rs`'s `SubagentStatus::Failed`/`Cancelled` arms were written by
/// hand — no capture has ever recorded a subagent ending any way but
/// `"completed"` (see `tests/corpus/claude/2.1.229/subagent`) — so nothing
/// had run the `Failed` arm through a real spawn until this fixture existed.
/// Fixture: `scenario:subagent-failed` in `fixtures/fake_claude.rs`.
#[tokio::test]
async fn subagent_terminal_failure_reaches_the_event_stream() {
    let (controls, _steer, _token) = controls("A");
    let events = run_to_end(&harness(), request("scenario:subagent-failed"), controls).await;

    assert!(events.contains(&AgentEvent::SubagentStarted {
        task_id: "sub-1-task".into(),
        tool_use_id: "sub-1".into(),
        agent_type: "general-purpose".into(),
        description: "Run the release check".into(),
        prompt: Some("Run scripts/check.sh and report the result.".into()),
    }));
    // task_updated: status only, no summary/usage yet.
    assert!(events.contains(&AgentEvent::SubagentUpdated {
        task_id: "sub-1-task".into(),
        status: SubagentStatus::Failed,
        activity: None,
        summary: None,
        total_tokens: None,
        duration_ms: None,
        tool_uses: None,
    }));
    // task_notification: adds the summary and usage the status-only reading
    // was missing, so it survives the material-transition filter too.
    assert!(events.contains(&AgentEvent::SubagentUpdated {
        task_id: "sub-1-task".into(),
        status: SubagentStatus::Failed,
        activity: None,
        summary: Some("check.sh exited 1".into()),
        total_tokens: Some(8_120),
        duration_ms: Some(1_830),
        tool_uses: Some(1),
    }));
}

/// The task-tool decode crossing a real spawn: fake executable → harness →
/// event stream. The unit tests in `normalize.rs` drive frames straight into
/// the normalizer, so nothing had checked that the `tool_use_id` join survives
/// a real session's interleaving until this fixture existed.
///
/// Fixture: `scenario:checklist` in `fixtures/fake_claude.rs`, shaped from
/// `tests/corpus/claude/2.1.229/checklist`.
#[tokio::test]
async fn task_tool_mutations_reach_the_event_stream_as_checklist_changes() {
    let (controls, _steer, _token) = controls("A");
    let events = run_to_end(&harness(), request("scenario:checklist"), controls).await;

    // A create: the id comes off the RESULT, the subject off either side.
    assert!(
        events.contains(&AgentEvent::ChecklistItemChanged {
            item_id: "1".into(),
            text: Some("Alpha step".into()),
            active_form: None,
            status: ChecklistStatus::Pending,
        }),
        "{events:?}"
    );
    // An update: activeForm off the INPUT, destination off the result.
    assert!(
        events.contains(&AgentEvent::ChecklistItemChanged {
            item_id: "1".into(),
            text: None,
            active_form: Some("Working the first step".into()),
            status: ChecklistStatus::InProgress,
        }),
        "{events:?}"
    );
    // A completion carries no activeForm at all.
    assert!(
        events.contains(&AgentEvent::ChecklistItemChanged {
            item_id: "1".into(),
            text: None,
            active_form: None,
            status: ChecklistStatus::Completed,
        }),
        "{events:?}"
    );
    // Task 9 was never created in this session — the resumed-run shape. It
    // must still reach the stream, with no subject anywhere.
    assert!(
        events.contains(&AgentEvent::ChecklistItemChanged {
            item_id: "9".into(),
            text: None,
            active_form: Some("Working an inherited step".into()),
            status: ChecklistStatus::InProgress,
        }),
        "{events:?}"
    );
    // The task calls themselves stay ordinary tool chips.
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolCall {
                call: comet_proto::ToolCall::Unknown { name, .. },
                ..
            } if name == "TaskCreate"
        )),
        "{events:?}"
    );
}

#[tokio::test]
async fn ask_user_question_round_trips_through_the_control_channel() {
    // The questions must reach the ENGINE's input bridge (`request_input`) —
    // and the harness must NOT emit its own `InputRequested`/`InputResolved`
    // twins: the bridge owns that lifecycle (it mints the request id the
    // resolver is parked under; a harness-emitted copy folded an unanswerable
    // duplicate chip into the doc).
    let asked: Arc<Mutex<Vec<UserInputQuestion>>> = Arc::new(Mutex::new(Vec::new()));
    let approved: Arc<Mutex<Vec<comet_proto::ApprovalRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let (steer_tx, steer_rx) = mpsc::channel(8);
    let _steer = steer_tx;
    let token = CancellationToken::new();
    let seen = asked.clone();
    let seen_approvals = approved.clone();
    let controls = RunControls {
        request_input: Box::new(move |questions| {
            seen.lock().unwrap().extend(questions.iter().cloned());
            let (tx, rx) = oneshot::channel();
            let answers: Vec<UserInputAnswer> = questions
                .iter()
                .map(|q| UserInputAnswer {
                    question_id: q.id.clone(),
                    labels: vec!["B".into()],
                })
                .collect();
            let _ = tx.send(answers);
            rx
        }),
        // Records what it was asked, mirroring `asked` above, and answers
        // Allow only for the expected Bash request — anything else drops the
        // sender (not approved), same shape as the shared `controls()`
        // helper. A call site that bypasses this bridge (e.g. reverting to
        // the old `request_approval: _request_approval` plus an unconditional
        // allow) records zero approvals here and fails the assertions below.
        request_approval: Box::new(move |approval: comet_proto::ApprovalRequest| {
            seen_approvals.lock().unwrap().push(approval.clone());
            let (tx, rx) = oneshot::channel::<comet_proto::ApprovalDecision>();
            if approval
                == (comet_proto::ApprovalRequest::Command {
                    command: "ls".into(),
                    cwd: None,
                })
            {
                let _ = tx.send(comet_proto::ApprovalDecision::Allow);
            }
            rx
        }),
        steering: steer_rx,
        interrupt: token.clone(),
    };
    let events = run_to_end(&harness(), request("scenario:askuser"), controls).await;

    let asked = asked.lock().unwrap();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].header, "Choice");
    assert_eq!(asked[0].question, "Pick one");
    assert_eq!(asked[0].options, vec!["A".to_string(), "B".to_string()]);
    assert!(
        !events.iter().any(|e| matches!(
            e,
            AgentEvent::InputRequested { .. } | AgentEvent::InputResolved { .. }
        )),
        "harness must not emit input lifecycle events itself: {events:?}"
    );

    // Proves the approval bridge was actually consulted for the plain Bash
    // can_use_tool, not bypassed: a bypassed bridge records zero approvals
    // and this fails.
    let approved = approved.lock().unwrap();
    assert_eq!(approved.len(), 1, "expected exactly one approval request");
    assert_eq!(
        approved[0],
        comet_proto::ApprovalRequest::Command {
            command: "ls".into(),
            cwd: None,
        }
    );

    // "answered" proves both control round-trips: the plain Bash can_use_tool
    // was approved via `request_approval` AND the answers reached the CLI as
    // updatedInput.answers keyed by question text.
    assert_eq!(
        events.last(),
        Some(&AgentEvent::Done {
            status: DoneStatus::Completed,
            result: Some("answered".into()),
            error: None,
            session_id: Some("sess-ask".into()),
        })
    );
}

#[tokio::test]
async fn a_run_asks_before_writing_and_tells_the_cli_what_the_user_said() {
    let (asked_tx, asked_rx) = std::sync::mpsc::channel();
    let (controls, _steer, _token) = controls_with_approver(move |req: ApprovalRequest| {
        let (tx, rx) = oneshot::channel();
        asked_tx.send(req).unwrap();
        let _ = tx.send(ApprovalDecision::Deny {
            message: "not that path".into(),
        });
        rx
    });
    let events = run_to_end(&harness(), request("scenario:approval"), controls).await;

    // The card the user would have seen, built from the real frame shape. The
    // path is absolute and under a directory that does not exist, so the
    // adapter's real existence check (not a test double) resolves the Write to
    // a create. Kept in step with `WRITE_TARGET_JSON` in fixtures/fake_claude.rs.
    let write_target = if cfg!(windows) {
        r"C:\comet-fake-fixture\a.txt"
    } else {
        "/comet-fake-fixture/a.txt"
    };
    assert_eq!(
        asked_rx.recv().unwrap(),
        ApprovalRequest::FileChange {
            path: write_target.into(),
            operation: FileOperation::Create,
            added_lines: 1,
            removed_lines: 0,
        }
    );
    let text = text_of(&events);
    assert!(
        text.contains("told: deny"),
        "the CLI must hear the denial, got {text:?}"
    );
}

#[tokio::test]
async fn a_run_whose_approver_is_gone_denies_rather_than_writing() {
    // `controls()`'s approver drops its sender. That must reach the CLI as a
    // denial — not as silence, and never as an allow.
    let (controls, _steer, _token) = controls("A");
    let events = run_to_end(&harness(), request("scenario:approval"), controls).await;
    let text = text_of(&events);
    assert!(text.contains("told: deny"), "got {text:?}");
}

#[tokio::test]
async fn steering_lines_are_written_to_stdin_mid_run() {
    let (controls, steer, _token) = controls("A");
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
        .expect("Steered emitted");
    assert!(steered.0.is_some() && steered.1.is_some());
    assert_ne!(steered.0, steered.1);

    // The fake CLI echoes the steer line's content back as a delta.
    assert!(events.contains(&AgentEvent::TextDelta {
        text: "steered:redirect please".into()
    }));
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Done {
            status: DoneStatus::Completed,
            ..
        })
    ));
}

#[tokio::test]
async fn interrupt_escalates_to_sigterm_and_ends_with_interrupted_done() {
    let harness = ClaudeHarness::new()
        .with_executable(fixture_path())
        .with_graces(Duration::from_millis(100), Duration::from_millis(500));
    let (controls, _steer, token) = controls("A");
    let mut stream = harness
        .run(request("scenario:interrupt"), controls)
        .await
        .expect("run starts");

    let events = tokio::time::timeout(Duration::from_secs(10), async move {
        let mut events = Vec::new();
        while let Some(ev) = stream.next().await {
            let ev = ev.expect("stream event");
            if matches!(ev, AgentEvent::SessionStarted { .. }) {
                token.cancel(); // interrupt as soon as the session is up
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
            session_id: Some("sess-int".into()),
        })
    );
}

#[tokio::test]
async fn error_codes_map_to_readable_messages() {
    let (controls, _steer, _token) = controls("A");
    let events = run_to_end(&harness(), request("scenario:error"), controls).await;

    let errors: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Error { message } => Some(message.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        errors.contains(&"Claude usage limit reached — try again after the limit resets."),
        "assistant error code not mapped: {errors:?}"
    );
    assert!(
        errors.contains(
            &"Claude 5-hour limit reached — the turn was blocked. Try again after it resets."
        ),
        "rejected rate_limit_event not mapped: {errors:?}"
    );

    // Empty `errors` array on the result falls back to subtype wording.
    assert_eq!(
        events.last(),
        Some(&AgentEvent::Done {
            status: DoneStatus::Errored,
            result: None,
            error: Some("The run hit the maximum number of turns.".into()),
            session_id: Some("sess-err".into()),
        })
    );
}

// ---------------------------------------------------------------------------
// D45: reusable provider lifecycle fault primitives
// (docs/debt/D45-provider-lifecycle-fault-matrix.md)
//
// PR #203 built six of these on `fake_codex.rs`. This is their `fake_claude.rs`
// mirror — Claude's wire (stream-json over stdio, no per-turn ids, no
// handshake distinct from the turn itself) is different enough that several
// primitives change what they prove rather than porting one-for-one. See
// each fixture function's own comment in `tests/fixtures/fake_claude.rs` for
// the exact fault and how its meaning shifted.
// ---------------------------------------------------------------------------

/// D45 primitive: setup-stage stall, changed meaning (see
/// `fake_claude.rs::silent_stall`'s doc comment for why — `ClaudeHarness` has
/// no `startup_timeout` equivalent at all, unlike Codex).
///
/// A run that produces literally zero output must still be recoverable
/// through `RunControls::interrupt` — cancelling before the CLI has said
/// anything, not after (`interrupt_escalates_to_sigterm_and_ends_with_interrupted_done`
/// below only proves recovery once a `SessionStarted` has already arrived).
/// Cancelling immediately, before the child has necessarily even finished
/// spawning, still resolves in one bounded `Done` with nothing else in the
/// stream — proving there is no race where an early frame sneaks out before
/// the escalation kills the child.
///
/// Falsified by temporarily making `silent_stall()` emit a `system`/`init`
/// line before sleeping: the assertion below (`events` is EXACTLY one
/// `Done`) failed with a `SessionStarted` event ahead of it, confirming the
/// assertion is pinned on "nothing was ever heard from the provider", not
/// merely "the run eventually ends interrupted". Restored before committing.
#[tokio::test]
async fn claude_silent_stall_recovers_only_via_manual_cancellation() {
    let harness = ClaudeHarness::new()
        .with_executable(fixture_path())
        .with_graces(Duration::from_millis(100), Duration::from_millis(200));
    let (controls, _steer, token) = controls("A");
    let stream = harness
        .run(request("scenario:silent-stall"), controls)
        .await
        .expect("run starts");
    // Nothing else could ever end this run: no timeout exists to race
    // against, so cancelling immediately (rather than waiting for some
    // in-band signal that will never come) is the only correct way to drive
    // this scenario at all.
    token.cancel();

    let events = tokio::time::timeout(
        Duration::from_secs(10),
        stream.map(|r| r.expect("stream event")).collect::<Vec<_>>(),
    )
    .await
    .expect("cancellation must still bound a totally silent run");

    assert_eq!(
        events,
        vec![AgentEvent::Done {
            status: DoneStatus::Interrupted,
            result: None,
            error: None,
            session_id: None,
        }],
        "a silent provider must produce exactly one terminal event, nothing more: {events:?}"
    );
}

/// D45 primitive: stderr_then_exit, mid-turn. Ported directly — same
/// `StderrTail`/`describe_exit`/`crash_message` machinery Codex's mirror
/// exercises, just reached through Claude's one crash-reporting call site
/// instead of two.
///
/// Falsified by temporarily deleting the `writeln!(stderr, ...)` call in
/// `crash_mid_turn()`: the assertion on `"boom: fake claude crashed
/// mid-turn"` failed because `error` no longer contained it (`crash_message`
/// fell back to its no-stderr form, `"claude exited unexpectedly (exit code
/// 66)"`). Restored before committing.
#[tokio::test]
async fn claude_mid_turn_crash_reports_exit_code_and_a_bounded_stderr_excerpt() {
    // `Incoming::Eof` (this fixture's stdout closing) and the OS actually
    // marking the process exited (what `child.try_wait()` reads right after
    // the main loop breaks) are two independent observations — the same race
    // `crash_mid_turn_settled_error` in codex.rs documents and works around.
    // Bounding by re-running the whole scenario, per
    // `.agents/workflows/verify.md`'s "poll the condition with a generous
    // deadline instead": each attempt spawns an independent process, so a
    // retry is a fresh trial of the same race, not a re-read of stale state.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut attempts = 0u32;
    let error = loop {
        attempts += 1;
        let (controls, _steer, _token) = controls("A");
        let events = run_to_end(&harness(), request("scenario:crash-mid-turn"), controls).await;
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
                 attempts within a 10s deadline (last message: {error})"
            );
            break error;
        }
    };
    assert!(
        error.contains("boom: fake claude crashed mid-turn"),
        "expected the bounded stderr excerpt, got: {error}"
    );
}

/// D45 primitive: partial_line — "stdout closes halfway through a frame".
/// Ported directly: `tokio::io::Lines` behaves the same as Codex's
/// `BufReader::lines()` on a delimiter-less tail at EOF.
///
/// Falsified by temporarily completing the truncated JSON in
/// `partial_frame_then_exit()` (closing every brace and adding the trailing
/// newline `write!` deliberately omits): the diagnostics assertion failed
/// with `left: [] right: [("unparseable", Malformed)]` — a real, valid
/// `TextDelta` reached the stream instead of a diagnostic, and the run ended
/// `Errored` (exit code 0 is still "unexpected" for a run with no `Done`
/// frame) rather than through the diagnostic path this test exists to pin.
/// Restored before committing.
#[tokio::test]
async fn claude_partial_stdout_frame_is_malformed_not_silently_dropped() {
    let (controls, _steer, _token) = controls("A");
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

/// D45 primitive: "stdin breaks while Comet writes a decision" — a provider
/// that raises a `can_use_tool` request and never sticks around to see it
/// answered. Ported directly onto Claude's control-channel shape.
///
/// Falsified by temporarily typo-ing the fixture's own dispatch tag
/// (`"scenario:die-after-aproval"`, one `p`, in `fake_claude.rs`'s `main`),
/// so the run falls through to the fixture's generic "unknown scenario"
/// branch instead of `die_after_approval()`: `assert_ne!(error, "unknown
/// scenario", ...)` fired with `left: "unknown scenario" right: "unknown
/// scenario"`, proving that check is load-bearing rather than vacuous.
/// Restored before committing.
#[tokio::test]
async fn claude_provider_dying_before_an_approval_decision_still_ends_bounded() {
    let (controls, _steer, _token) = controls_with_approver(|_req: ApprovalRequest| {
        let (tx, rx) = oneshot::channel();
        let _ = tx.send(ApprovalDecision::Allow);
        rx
    });
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

/// D45 primitive: duplicate, changed meaning (see
/// `fake_claude.rs::duplicate_completion`'s doc comment). This documents
/// CURRENT behavior, not an aspiration — `run_session`'s main loop only ever
/// checks `interrupted` before breaking on a `Done`, never whether a `Done`
/// was already sent, so a second `result` frame produces a second `Done`.
/// Fixing it needs a change in `crates/harness/src/claude/mod.rs`, out of
/// scope for a tests-only slice; see the PR description for the proposed
/// debt row.
///
/// Falsified by temporarily deleting the second `emit(r#"{"type":"result"..`
/// call in `duplicate_completion()`: `done_count` read 1, and the
/// `assert_eq!(done_count, 2, ...)` failed with that count, confirming the
/// assertion is pinned on the actual duplicate rather than on any completion
/// at all. Restored before committing.
#[tokio::test]
async fn claude_duplicate_result_frame_is_not_deduplicated_today() {
    let (controls, _steer, _token) = controls("A");
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
        done_count, 2,
        "known gap (see this test's doc comment): expected two Done events \
         for a duplicated result frame, got {done_count} in {events:?}"
    );
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
/// provider's own successful `result` beats the interrupt to the wire's
/// meaning, arriving after the harness already committed to
/// `interrupted = true`.
///
/// Falsified by temporarily asserting `DoneStatus::Completed` here instead
/// of `Interrupted`: failed with `left: Some(Done { status: Interrupted,
/// .. }) right: Some(Done { status: Completed, .. })` — the fixture's own
/// late `result: success` really does resolve through the shared
/// `interrupted` flag rather than reporting what the frame itself claims.
/// Restored before committing.
#[tokio::test]
async fn claude_completion_delayed_past_cancellation_still_reports_interrupted() {
    let harness = ClaudeHarness::new()
        .with_executable(fixture_path())
        // Comfortably longer than the fixture's own 100ms delay, so the kill
        // escalation this deliberately races against never fires.
        .with_graces(Duration::from_secs(2), Duration::from_secs(2));
    let (controls, _steer, token) = controls("A");
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
            session_id: Some("sess-late".into()),
        }),
        "a late-arriving completion must not overrule an already-committed \
         interrupt: {events:?}"
    );
}

// ---------------------------------------------------------------------------
// D43: the real process-launch contract (docs/debt/D43-fake-provider-launch-contract.md)
// ---------------------------------------------------------------------------

/// D43's headline finding: "Claude's fake accepts every argument except an
/// invalid `--permission-mode`." `tests/fixtures/fake_claude.rs`'s
/// `check_run_flags` now fails loudly, before any scenario dispatch, if a
/// run's argv is missing the streaming transport, `--verbose`,
/// `--include-partial-messages`, or the stdio permission channel — so this
/// (and every other run-scenario test in this file) is proof that a real
/// `scenario:happy` invocation still carries the full floor. Falsified by
/// temporarily removing `--verbose` from `claude::run_launch` and rerunning
/// this test: it failed with
/// `claude exited unexpectedly (exit code 1): fake-claude: run argv is
/// missing required flag(s) ["--verbose"]; got [...]` — see the PR
/// description for the full quoted line. Restored before committing.
#[tokio::test]
async fn a_run_carries_the_full_streaming_and_control_argv_floor() {
    let (controls, _steer, _token) = controls("A");
    let events = run_to_end(&harness(), request("scenario:happy"), controls).await;
    assert!(
        matches!(
            events.last(),
            Some(AgentEvent::Done {
                status: DoneStatus::Completed,
                ..
            })
        ),
        "the fixture's own argv floor check must pass for a real run_launch: {events:?}"
    );
}

/// D43's other headline gaps: "[nothing proves] `--model`, effort, settings
/// or `--resume`", and the run path is "weaker than command discovery's
/// child-side cwd echo" (which only ever proved discovery, never a run).
/// This drives a real request carrying all four plus a non-default `cwd`,
/// and has the fixture (`launch_record` in `fake_claude.rs`) record the argv
/// and working directory the OS actually handed the child — the shape #197
/// established: the fake records what it received, and the test asserts
/// against that record instead of trusting Comet's own `Command` was built
/// right.
///
/// Falsified one property at a time by commenting out the corresponding
/// block in `claude::run_launch` and rerunning: each removal failed only its
/// own assertion below (quoted in the PR description), never the others,
/// before being restored.
#[tokio::test]
async fn model_effort_settings_resume_and_the_real_child_cwd_reach_the_child() {
    let record_dir = std::env::temp_dir().join("comet-claude-launch-record");
    std::fs::create_dir_all(&record_dir).expect("record dir");
    let record_path = record_dir.join("record.json");
    let _ = std::fs::remove_file(&record_path);

    let cwd_dir = std::env::temp_dir().join("comet-claude-launch-cwd-probe");
    std::fs::create_dir_all(&cwd_dir).expect("probe dir");

    let mut req = request(&format!("scenario:launch-record|{}", record_path.display()));
    req.model = Some("claude-test-model".into());
    req.reasoning = Some(comet_proto::ReasoningLevel::Medium);
    req.resume = Some("resume-token-1".into());
    req.cwd = cwd_dir.display().to_string();
    req.model_options
        .insert("fastMode".into(), serde_json::Value::Bool(true));

    let (controls, _steer, _token) = controls("A");
    let events = run_to_end(&harness(), req, controls).await;
    assert!(
        matches!(
            events.last(),
            Some(AgentEvent::Done {
                status: DoneStatus::Completed,
                ..
            })
        ),
        "launch_record must end the run cleanly: {events:?}"
    );

    let raw = std::fs::read_to_string(&record_path).expect("the fixture must write a record");
    let record: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON record");
    let args: Vec<String> = record["args"]
        .as_array()
        .expect("args array")
        .iter()
        .map(|v| v.as_str().expect("string arg").to_string())
        .collect();

    let value_of = |flag: &str| -> Option<&str> {
        args.iter()
            .position(|a| a == flag)
            .and_then(|at| args.get(at + 1))
            .map(String::as_str)
    };

    assert_eq!(
        value_of("--model"),
        Some("claude-test-model"),
        "argv: {args:?}"
    );
    assert_eq!(value_of("--effort"), Some("medium"), "argv: {args:?}");
    assert!(
        args.iter().any(|a| a == "--resume=resume-token-1"),
        "argv missing --resume=resume-token-1: {args:?}"
    );
    let settings = value_of("--settings").unwrap_or_else(|| panic!("argv: {args:?}"));
    assert!(
        settings.contains(r#""fastMode":true"#),
        "settings argv missing fastMode: {settings}"
    );
    assert_eq!(
        record["cwd"].as_str(),
        Some(cwd_dir.display().to_string().as_str()),
        "the real child must start in the requested directory, not merely report it on the wire"
    );
}

#[tokio::test]
async fn missing_binary_is_not_installed() {
    let harness = ClaudeHarness::new().with_executable("/nonexistent/claude-nowhere");
    let (controls, _steer, _token) = controls("A");
    let err = harness
        .run(request("scenario:happy"), controls)
        .await
        .err()
        .expect("spawn fails");
    assert!(matches!(err, HarnessError::NotInstalled(_)), "{err:?}");
}

#[tokio::test]
async fn notice_frames_surface_as_notice_events() {
    let (controls, _steer, _token) = controls("A");
    let events = run_to_end(&harness(), request("scenario:notices"), controls).await;

    let notices: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Notice {
                kind,
                severity,
                summary,
                detail,
                key,
            } => Some((
                *kind,
                *severity,
                summary.clone(),
                detail.clone(),
                key.clone(),
            )),
            _ => None,
        })
        .collect();

    assert!(
        notices.contains(&(
            NoticeKind::Compaction,
            NoticeSeverity::Info,
            "Context compacted automatically".to_string(),
            Some("68000 tokens → 12000".to_string()),
            Some("compaction".to_string()),
        )),
        "{notices:?}"
    );
    assert!(
        notices.contains(&(
            NoticeKind::ModelRerouted,
            NoticeSeverity::Warning,
            "Model changed to claude-haiku-4-5".to_string(),
            Some(
                "claude-fable-5 refused the request; replies now come from claude-haiku-4-5."
                    .to_string()
            ),
            Some("model".to_string()),
        )),
        "{notices:?}"
    );
    // Both retry frames surface — collapse is the doc fold's job, not the
    // adapter's.
    let retries: Vec<&String> = notices
        .iter()
        .filter(|(k, ..)| *k == NoticeKind::Retrying)
        .map(|(_, _, s, ..)| s)
        .collect();
    assert_eq!(retries.len(), 2, "{notices:?}");
    assert_eq!(retries[0], "Retrying — attempt 1 of 3");
    assert_eq!(retries[1], "Retrying — attempt 2 of 3");
    // Passthrough kinds: severity read from the wire, key carried verbatim.
    assert!(
        notices.contains(&(
            NoticeKind::Info,
            NoticeSeverity::Warning,
            "Consider running /doctor to fix your settings.".to_string(),
            None,
            None,
        )),
        "{notices:?}"
    );
    assert!(
        notices.contains(&(
            NoticeKind::Info,
            NoticeSeverity::Info,
            "You have used half of your weekly limit.".to_string(),
            None,
            Some("usage-warning".to_string()),
        )),
        "{notices:?}"
    );
    assert!(
        notices.contains(&(
            NoticeKind::RateLimit,
            NoticeSeverity::Warning,
            "Approaching the Claude 5-hour usage limit".to_string(),
            None,
            Some("rateLimit".to_string()),
        )),
        "{notices:?}"
    );

    // The unclaimed someFutureSubtype now surfaces as a diagnostic — 0b.2's
    // interlock with 0b.1's parse_frame allowlist — rather than vanishing.
    let diagnostics: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Diagnostic {
                discriminator,
                severity,
                ..
            } => Some((discriminator.as_str(), *severity)),
            _ => None,
        })
        .collect();
    assert_eq!(
        diagnostics,
        vec![("system/someFutureSubtype", DiagnosticSeverity::Unknown)]
    );

    // Exactly the scripted emitters fired — compaction, reroute, retry x2,
    // informational, notification, rate limit — and the unclaimed future
    // subtype produced neither a notice nor an error.
    assert_eq!(notices.len(), 7, "{notices:?}");
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
async fn unclaimed_frames_surface_as_diagnostics_and_ignored_frames_stay_silent() {
    let (controls, _steer, _token) = controls("A");
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
            ("unparseable".to_string(), DiagnosticSeverity::Malformed),
            (
                "system/someFutureSubtype".to_string(),
                DiagnosticSeverity::Unknown
            ),
            ("mystery_frame".to_string(), DiagnosticSeverity::Unknown),
            (
                "control_request/request_user_dialog".to_string(),
                DiagnosticSeverity::Unknown
            ),
        ],
        "{events:?}"
    );
    // Redaction is structural: no provider text travels on any diagnostic.
    for e in &events {
        if let AgentEvent::Diagnostic { summary, .. } = e {
            assert!(!summary.contains("do-not-carry"), "{summary}");
        }
    }
    // The capture-confirmed Ignored tier (status, the hook pair, and
    // background_tasks_changed) produced nothing, no Error fired, and the
    // run still ends cleanly.
    assert!(!events.iter().any(|e| matches!(e, AgentEvent::Error { .. })));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta { text } if text == "ok"))
    );
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Done {
            status: DoneStatus::Completed,
            ..
        })
    ));
}

/// Both initialize paths must drain the piped stderr before it fills. The fake
/// writes one MiB there before reading stdin, so either undrained path wedges.
#[tokio::test]
async fn initialize_discovery_drains_stderr_for_models_and_commands() {
    let harness = harness();
    let cwd = std::env::temp_dir().join("comet-noisy-command-discovery");
    std::fs::create_dir_all(&cwd).expect("command cwd");

    tokio::time::timeout(Duration::from_secs(5), async {
        harness.models().await.expect("models");
        harness
            .commands(&cwd.display().to_string())
            .await
            .expect("commands");
    })
    .await
    .expect("a full stderr pipe must not block either initialize path");
}

/// The slice's deliverable: a real spawn, a real handshake round-trip, and a
/// merged catalog that says it is live. The fixture emits the captured
/// initialize reply itself, `claude/2.1.228/model-discovery` frame 2, loaded
/// byte-for-byte rather than a hand-typed stand-in — this decodes the
/// provider's real seven-model list, not an author's guess at its shape.
#[tokio::test]
async fn models_come_back_live_and_merged() {
    let catalog = harness().models().await.expect("models");
    assert_eq!(catalog.source, comet_proto::CatalogSource::Live);
    let ids: Vec<&str> = catalog.models.iter().map(|m| m.id.as_str()).collect();
    assert!(
        ids.contains(&"claude-sonnet-5"),
        "the live `sonnet` merged onto its curated row, got {ids:?}"
    );
    assert!(
        !ids.contains(&"sonnet"),
        "an alias must not become its own row, got {ids:?}"
    );
    assert!(
        ids.contains(&"claude-opus-4-8"),
        "a curated model the CLI did not list is still kept, got {ids:?}"
    );
}

/// The `/` menu's spawn, proven from the child's side.
///
/// The fixture reports its own working directory and whether `--bare` reached
/// it, as two command entries. Nothing else can establish either: a decode test
/// reads bytes this process wrote, so it would pass just as happily if the
/// adapter spawned in the wrong directory or with the model discovery's
/// arguments. Slice 2.3 shipped exactly that bug — a login check reading one
/// `CODEX_HOME` while the child used another — with every test green.
#[tokio::test]
async fn commands_are_read_from_the_requested_directory_without_bare() {
    let dir = std::env::temp_dir().join("comet-command-cwd-probe");
    std::fs::create_dir_all(&dir).expect("probe dir");
    let commands = harness()
        .commands(&dir.display().to_string())
        .await
        .expect("commands");

    let cwd_echo = commands
        .iter()
        .find(|c| c.name == "cwd-echo")
        .expect("the fixture echoes its cwd");
    assert_eq!(
        cwd_echo.description.as_deref(),
        Some(dir.display().to_string().as_str()),
        "the child must be started in the directory the caller asked about"
    );

    let bare_echo = commands
        .iter()
        .find(|c| c.name == "bare-echo")
        .expect("the fixture echoes its arguments");
    assert_eq!(
        bare_echo.description.as_deref(),
        Some("false"),
        "--bare skips user and project skill discovery, which is the whole list (D32)"
    );
}

/// Aliases survive the trip and empty hints do not. The composer matches on
/// aliases without listing them, so losing them here silently costs `/cr` with
/// no test noticing.
#[tokio::test]
async fn command_aliases_and_hints_survive_the_round_trip() {
    let commands = harness()
        .commands(&std::env::temp_dir().display().to_string())
        .await
        .expect("commands");
    let review = commands
        .iter()
        .find(|c| c.name == "review")
        .expect("review command");
    assert_eq!(review.aliases, vec!["cr".to_string()]);
    assert_eq!(review.argument_hint.as_deref(), Some("[--fix]"));
    let cwd_echo = commands.iter().find(|c| c.name == "cwd-echo").unwrap();
    assert_eq!(
        cwd_echo.argument_hint, None,
        "the CLI's empty-string hint must not reach the menu as a blank slot"
    );
}

/// A CLI that cannot be spawned must not answer "this agent has no commands".
/// An empty list is a real answer — a directory with nothing in it — so the
/// failure has to stay distinguishable all the way to the caller.
#[tokio::test]
async fn a_missing_cli_fails_rather_than_reporting_no_commands() {
    let harness = ClaudeHarness::new().with_executable("/nonexistent/claude-nowhere");
    let answer = harness
        .commands(&std::env::temp_dir().display().to_string())
        .await;
    assert!(
        answer.is_err(),
        "a failed read must not degrade to an empty menu, got {answer:?}"
    );
}

/// Two directories are two answers. The cache keys on the directory precisely
/// because a project's skills belong to it, and serving them elsewhere is a
/// wrong answer rather than a missing one.
#[tokio::test]
async fn each_directory_gets_its_own_command_list() {
    let harness = harness();
    let a = std::env::temp_dir().join("comet-cmd-a");
    let b = std::env::temp_dir().join("comet-cmd-b");
    std::fs::create_dir_all(&a).expect("a");
    std::fs::create_dir_all(&b).expect("b");

    let from_a = harness.commands(&a.display().to_string()).await.expect("a");
    let from_b = harness.commands(&b.display().to_string()).await.expect("b");
    let echo = |list: &[comet_proto::AgentCommand]| {
        list.iter()
            .find(|c| c.name == "cwd-echo")
            .and_then(|c| c.description.clone())
            .expect("cwd echo")
    };
    assert_eq!(echo(&from_a), a.display().to_string());
    assert_eq!(
        echo(&from_b),
        b.display().to_string(),
        "the second directory must not be served the first one's cached answer"
    );
}

/// An answer we cannot read is the one failure that means a provider changed
/// its protocol under us, and it must survive as `Unparseable` so the engine
/// raises its `Diagnostic` (`crates/engine/src/rpc.rs:1010`).
#[tokio::test]
async fn an_unreadable_answer_is_reported_as_drift() {
    let harness =
        ClaudeHarness::new().with_executable(env!("CARGO_BIN_EXE_fake-claude-bad-discovery"));
    let catalog = harness.models().await.expect("still answers");
    assert_eq!(
        catalog.source,
        comet_proto::CatalogSource::BuiltIn,
        "a broken handshake still serves the curated list"
    );
    assert_eq!(
        harness.take_unreported_discovery_failure(),
        Some(comet_harness::discovery::DiscoveryFailure::Unparseable)
    );
}

/// A CLI that cannot be spawned is ordinary, not drift — otherwise every
/// machine without Claude installed would report a protocol failure on boot.
#[tokio::test]
async fn a_missing_cli_is_not_drift() {
    let harness = ClaudeHarness::new().with_executable("/nonexistent/claude-nowhere");
    let catalog = harness.models().await.expect("curated list still answers");
    assert_eq!(catalog.source, comet_proto::CatalogSource::BuiltIn);
    assert_eq!(
        harness.take_unreported_discovery_failure(),
        Some(comet_harness::discovery::DiscoveryFailure::Unreachable)
    );
}

/// The live check, against the real CLI rather than the fixture: the handshake
/// answers, the merge lands on curated ids, and no alias or duplicate reaches
/// the picker. Ignored by default — it needs an installed, authenticated
/// `claude` — but it spends no tokens, because a discovery session never runs
/// a turn.
/// Run with: `cargo test -p comet-harness --test claude -- --ignored`
#[tokio::test]
#[ignore = "requires installed+authenticated claude CLI (spends no tokens)"]
async fn live_cli_discovery_lands_on_curated_ids() {
    let catalog = ClaudeHarness::new().models().await.expect("models");
    let ids: Vec<&str> = catalog.models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(
        catalog.source,
        comet_proto::CatalogSource::Live,
        "the real handshake answered, got {ids:?}"
    );
    assert_eq!(
        ids,
        vec![
            "claude-fable-5",
            "claude-opus-5",
            "claude-opus-4-8",
            "claude-opus-4-7",
            "claude-sonnet-5",
            "claude-haiku-4-5",
        ],
        "six curated rows: no `sonnet`, no `opus[1m]`, no `default`"
    );
}

/// The live check for the `/` menu: the real CLI, in this repository, answers
/// with the project's own skills. Ignored by default; spends no tokens, because
/// this session never runs a turn either.
///
/// It asserts a project-scoped command specifically, because that is what the
/// non-bare spawn buys: with `--bare` the same call answers with built-ins only
/// and every other assertion here would still pass. The non-bare handshake
/// includes project-scoped commands the bare handshake omits, per
/// `claude/2.1.228/model-discovery` frame 2 (bare) and
/// `claude/2.1.228/command-discovery` frame 5 (non-bare).
/// Run with: `cargo test -p comet-harness --test claude -- --ignored`
#[tokio::test]
#[ignore = "requires installed+authenticated claude CLI (spends no tokens)"]
async fn live_cli_commands_include_this_repositorys_own_skills() {
    let repo = env!("CARGO_MANIFEST_DIR")
        .rsplit_once("crates")
        .map(|(root, _)| root.to_owned())
        .expect("crate lives under the repo root");
    let commands = ClaudeHarness::new()
        .commands(&repo)
        .await
        .expect("commands");
    let names: Vec<&str> = commands.iter().map(|c| c.name.as_str()).collect();
    assert!(
        names.contains(&"verify"),
        "comet's own project skill must be in the list, got {} commands: {names:?}",
        names.len()
    );
    assert!(
        commands.iter().any(|c| c.argument_hint.is_some()),
        "at least one command carries an argument hint"
    );
}

/// The cache belongs to the CLI it asked, not to the harness value. Pointing a
/// harness at a different executable and re-asking must re-run discovery —
/// otherwise the second CLI's answer is whatever the first one said. Latent
/// today (production never calls `with_executable`, and every test builds the
/// harness in one chain), but the builder is public and the failure would be
/// silent.
#[tokio::test]
async fn changing_the_executable_re_runs_discovery() {
    let harness = harness();
    assert_eq!(
        harness.models().await.expect("first").source,
        comet_proto::CatalogSource::Live,
        "the good fixture answers"
    );

    let harness = harness.with_executable(env!("CARGO_BIN_EXE_fake-claude-bad-discovery"));
    assert_eq!(
        harness.models().await.expect("second").source,
        comet_proto::CatalogSource::BuiltIn,
        "the new executable is asked, not the old answer replayed"
    );
}
