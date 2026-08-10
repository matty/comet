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
    AgentEvent, DiagnosticSeverity, DoneStatus, HarnessId, NoticeKind, NoticeSeverity, RunRequest,
    RuntimeMode, ToolCall, UserInputAnswer, UserInputQuestion,
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
        is_error: false
    }));
    assert!(events.contains(&AgentEvent::ToolResult {
        id: "tool-2".into(),
        is_error: true
    }));

    // Informational rate-limit frames stay quiet.
    assert!(!events.iter().any(|e| matches!(e, AgentEvent::Error { .. })));

    assert!(events.contains(&AgentEvent::Usage {
        input_tokens: 10,
        output_tokens: 20
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

#[tokio::test]
async fn ask_user_question_round_trips_through_the_control_channel() {
    // The questions must reach the ENGINE's input bridge (`request_input`) —
    // and the harness must NOT emit its own `InputRequested`/`InputResolved`
    // twins: the bridge owns that lifecycle (it mints the request id the
    // resolver is parked under; a harness-emitted copy folded an unanswerable
    // duplicate chip into the doc).
    let asked: Arc<Mutex<Vec<UserInputQuestion>>> = Arc::new(Mutex::new(Vec::new()));
    let (steer_tx, steer_rx) = mpsc::channel(8);
    let _steer = steer_tx;
    let token = CancellationToken::new();
    let seen = asked.clone();
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
        // Plain tools now round-trip through `request_approval` (this task);
        // this fixture answers Allow so the run proceeds to the
        // AskUserQuestion step it actually exercises. The deny/unanswered
        // paths for this bridge are covered exhaustively by
        // `claude::control_request_tests`.
        request_approval: Box::new(|_approval: comet_proto::ApprovalRequest| {
            let (tx, rx) = oneshot::channel::<comet_proto::ApprovalDecision>();
            let _ = tx.send(comet_proto::ApprovalDecision::Allow);
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
    // The Ignored tier (status, thinking_tokens, the hook pair,
    // tool_progress — all capture-confirmed routine) produced nothing, no
    // Error fired, and the run still ends cleanly.
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
