//! M2 end-to-end tests: doc-queued commands → host executor → harness stream →
//! journal + broadcast + folded doc entries, plus interrupt/recovery/idempotence
//! and the RPC surface over the in-memory transport.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;

use comet_doc::{
    MessagePart, MessageRole, MessageStatus, SegmentWriter, SessionCommandEntry,
    SessionCommandPayload, SessionCommandStatus, SessionDoc, SessionMessageEntry,
};
use comet_engine::{EngineCore, HarnessRegistry, RunJournal};
use comet_harness::mock::MockHarness;
use comet_harness::{Harness, HarnessError, RunControls};
use comet_proto::{
    AgentEvent, DiagnosticSeverity, DoneStatus, HarnessCapabilities, HarnessId, Model, NoticeKind,
    NoticeSeverity, ReasoningLevel, RunRequest, SandboxLevel, SessionStatus, SteeringMode,
    ToolCall,
};
use comet_rpc::RpcService;
use comet_sync::DocsStore;

const CHAT: &str = "chat-e2e";
const VIEWER: &str = "viewer-device";

fn run_request(prompt: &str) -> RunRequest {
    RunRequest {
        prompt: prompt.into(),
        model: None,
        reasoning: None,
        model_options: Default::default(),
        cwd: "/tmp".into(),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: true,
        attachments: Vec::new(),
        resume: None,
    }
}

fn done(status: DoneStatus) -> AgentEvent {
    AgentEvent::Done {
        status,
        result: None,
        error: None,
        session_id: Some("hs-1".into()),
    }
}

fn mock_script() -> Vec<AgentEvent> {
    vec![
        AgentEvent::SessionStarted {
            harness: HarnessId::Mock,
            model: "mock-1".into(),
            tools: vec![],
            cwd: "/tmp".into(),
            session_id: "hs-1".into(),
            assistant_message_id: "a-1".into(),
        },
        AgentEvent::TextDelta { text: "Hel".into() },
        AgentEvent::TextDelta { text: "lo".into() },
        AgentEvent::ToolCall {
            id: "tool-1".into(),
            call: ToolCall::WriteFile {
                path: "/tmp/x".into(),
                content: Some("SECRET".into()),
            },
        },
        AgentEvent::ToolResult {
            id: "tool-1".into(),
            is_error: false,
        },
        done(DoneStatus::Completed),
    ]
}

/// Scripted harness with a per-event delay; optionally hangs after the script until its
/// interrupt token cancels, then ends with `Done{interrupted}`.
struct ScriptedHarness {
    script: Vec<AgentEvent>,
    step_delay: Duration,
    hang_until_interrupt: bool,
}

#[async_trait]
impl Harness for ScriptedHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }
    fn display_name(&self) -> &str {
        "Scripted"
    }
    fn capabilities(&self) -> HarnessCapabilities {
        HarnessCapabilities {
            supports_steering: true,
            steering_mode: SteeringMode::StepBoundary,
            reasoning_levels: vec![ReasoningLevel::Medium],
        }
    }
    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        Ok(vec![])
    }
    async fn run(
        &self,
        _request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<AgentEvent, HarnessError>>(16);
        let script = self.script.clone();
        let delay = self.step_delay;
        let hang = self.hang_until_interrupt;
        let token = controls.interrupt.clone();
        tokio::spawn(async move {
            for event in script {
                if tx.send(Ok(event)).await.is_err() {
                    return;
                }
                tokio::time::sleep(delay).await;
            }
            if hang {
                token.cancelled().await;
                let _ = tx.send(Ok(done(DoneStatus::Interrupted))).await;
            }
        });
        Ok(futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|event| (event, rx))
        })
        .boxed())
    }
}

fn registry_with(harness: Arc<dyn Harness>) -> Arc<HarnessRegistry> {
    let registry = HarnessRegistry::new();
    registry.register(harness);
    Arc::new(registry)
}

fn assemble(dir: &std::path::Path, harness: Arc<dyn Harness>) -> EngineCore {
    EngineCore::assemble(dir, registry_with(harness), HarnessId::Mock, None)
        .expect("engine core assembles")
}

/// Queue a command into the chat doc the way a REMOTE viewer device would: an immutable
/// pending entry appended under the viewer's device id (ledger rule 1).
fn queue_as_viewer(doc: &SessionDoc, id: &str, payload: SessionCommandPayload) {
    let now = chrono::Utc::now().timestamp_millis();
    let based_on =
        doc.read_entries()
            .expect("read entries")
            .last()
            .map(|m| comet_doc::CommandBasedOn {
                turn_id: Some(m.id.clone()),
                frontier: None,
            });
    doc.queue_command(&SessionCommandEntry {
        id: id.into(),
        payload,
        issued_by: VIEWER.into(),
        issued_at: now,
        based_on,
        expires_at: None,
        status: SessionCommandStatus::Pending,
        resolution: None,
    })
    .expect("queue command");
}

async fn wait_for<F>(mut predicate: F, what: &str)
where
    F: FnMut() -> bool,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !predicate() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
}

fn entries(core: &EngineCore) -> Vec<SessionMessageEntry> {
    core.doc_host
        .open(CHAT)
        .expect("open chat")
        .doc()
        .read_entries()
        .expect("read entries")
}

/// Tolerant read for hot-polling predicates: a snapshot taken between a
/// segment writer's `push_container` and its field writes deserializes with
/// fields missing — treat that instant as "not yet" instead of panicking.
fn entries_now(core: &EngineCore) -> Vec<SessionMessageEntry> {
    core.doc_host
        .open(CHAT)
        .ok()
        .and_then(|h| h.doc().read_entries().ok())
        .unwrap_or_default()
}

fn command_status(core: &EngineCore, id: &str) -> Option<(SessionCommandStatus, Option<String>)> {
    core.doc_host
        .open(CHAT)
        .expect("open chat")
        .doc()
        .read_commands()
        .expect("read commands")
        .into_iter()
        .find(|c| c.id == id)
        .map(|c| (c.status, c.resolution))
}

#[tokio::test]
async fn queued_run_command_executes_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let core = assemble(
        dir.path(),
        Arc::new(MockHarness {
            script: mock_script(),
        }),
    );
    let handle = core.doc_host.open(CHAT).unwrap();

    // Live event subscription (journal replay + broadcast) before anything runs.
    let (replayed, mut live) = core.sessions.subscribe(CHAT, 0).unwrap();
    assert!(replayed.is_empty());

    // A viewer device queues the run command into the doc.
    queue_as_viewer(
        handle.doc(),
        "cmd-run-1",
        SessionCommandPayload::Run {
            request: run_request("do the thing"),
            message_id: "msg-user-1".into(),
        },
    );

    // The host executor picks it up, runs the harness, and the doc settles.
    wait_for(
        || {
            entries(&core).iter().any(|e| {
                e.role == MessageRole::Assistant && e.status == Some(MessageStatus::Complete)
            })
        },
        "assistant entry to complete",
    )
    .await;

    let all = entries(&core);
    assert_eq!(all.len(), 2, "user + assistant entries, got {all:#?}");
    // User entry carries the command's client-minted message id.
    assert_eq!(all[0].id, "msg-user-1");
    assert_eq!(all[0].role, MessageRole::User);
    assert_eq!(
        all[0].parts,
        vec![MessagePart::Text {
            id: "t0".into(),
            text: "do the thing".into()
        }]
    );
    // Assistant entry: folded parts — merged text, then the resolved tool call with the
    // render-parts privacy policy applied (WriteFile content stripped).
    let assistant = &all[1];
    assert_eq!(assistant.status, Some(MessageStatus::Complete));
    assert_eq!(assistant.parts.len(), 2);
    match &assistant.parts[0] {
        MessagePart::Text { text, .. } => assert_eq!(text, "Hello"),
        other => panic!("unexpected first part {other:?}"),
    }
    match &assistant.parts[1] {
        MessagePart::Tool {
            call,
            resolved,
            is_error,
            ..
        } => {
            assert!(*resolved);
            assert!(!*is_error);
            assert_eq!(
                call,
                &ToolCall::WriteFile {
                    path: "/tmp/x".into(),
                    content: None
                }
            );
        }
        other => panic!("unexpected second part {other:?}"),
    }

    // Command outcome written by the host (sole outcome writer).
    assert_eq!(
        command_status(&core, "cmd-run-1"),
        Some((SessionCommandStatus::Applied, None))
    );

    // Journal replay: the full script in order, terminal Done last.
    let replay = core.sessions.subscribe(CHAT, 0).unwrap().0;
    assert_eq!(replay.len(), mock_script().len());
    assert!(matches!(
        replay.last().map(|j| &j.event),
        Some(AgentEvent::Done {
            status: DoneStatus::Completed,
            ..
        })
    ));
    let seqs: Vec<u64> = replay.iter().map(|j| j.seq).collect();
    assert_eq!(seqs, (1..=mock_script().len() as u64).collect::<Vec<_>>());

    // The live broadcast delivered the same events.
    let mut broadcast_count = 0usize;
    while let Ok(event) = live.try_recv() {
        assert!(event.seq >= 1);
        broadcast_count += 1;
    }
    assert_eq!(broadcast_count, mock_script().len());

    // Final session status: Idle.
    assert_eq!(
        core.sessions.session_status(CHAT).map(|s| s.status),
        Some(SessionStatus::Idle)
    );
}

#[tokio::test]
async fn session_status_transitions_idle_working_idle() {
    let dir = tempfile::tempdir().unwrap();
    let core = assemble(
        dir.path(),
        Arc::new(ScriptedHarness {
            script: mock_script(),
            step_delay: Duration::from_millis(40),
            hang_until_interrupt: false,
        }),
    );
    let mut watch = core.sessions.watch_sessions();
    assert!(watch.borrow().is_empty(), "no sessions before dispatch");

    let handle = core.doc_host.open(CHAT).unwrap();
    queue_as_viewer(
        handle.doc(),
        "cmd-run-status",
        SessionCommandPayload::Run {
            request: run_request("go"),
            message_id: "m-1".into(),
        },
    );

    let mut seen = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let status = tokio::time::timeout_at(deadline, watch.changed())
            .await
            .expect("status change before timeout")
            .map(|_| watch.borrow().first().map(|s| s.status))
            .expect("watch alive");
        if let Some(status) = status {
            if seen.last() != Some(&status) {
                seen.push(status);
            }
            if status == SessionStatus::Idle {
                break;
            }
        }
    }
    assert_eq!(seen, vec![SessionStatus::Working, SessionStatus::Idle]);
}

/// REGRESSION: a notice is the one event that can arrive OUTSIDE a turn, and
/// it must not be mistaken for turn-start. Counting it as one cleared
/// `idle_since` — which both flipped the parked session to Working and
/// disabled the 30-minute reaper's select arm (it is gated on
/// `idle_since.is_some()`), so the child was never released — and then opened
/// a `streaming` assistant entry holding just the chip that no `Done` was ever
/// coming to finish: a chat spinning forever with no terminal state.
#[tokio::test]
async fn a_notice_while_parked_leaves_the_session_parked_and_the_entry_finished() {
    let dir = tempfile::tempdir().unwrap();
    let core = assemble(
        dir.path(),
        Arc::new(ScriptedHarness {
            script: vec![
                AgentEvent::SessionStarted {
                    harness: HarnessId::Mock,
                    model: "mock-1".into(),
                    tools: vec![],
                    cwd: "/tmp".into(),
                    session_id: "hs-1".into(),
                    assistant_message_id: "a-1".into(),
                },
                AgentEvent::TextDelta {
                    text: "first turn".into(),
                },
                // Steerable + completed ⇒ the run PARKS here instead of ending.
                done(DoneStatus::Completed),
                // …and the provider keeps talking while nobody is asking.
                AgentEvent::Notice {
                    kind: NoticeKind::McpStatus,
                    severity: NoticeSeverity::Warning,
                    summary: "MCP server linear failed to start".into(),
                    detail: None,
                    key: Some("mcp:linear".into()),
                },
            ],
            step_delay: Duration::from_millis(20),
            // Persistent-session shape: the stream stays open past the park.
            hang_until_interrupt: true,
        }),
    );
    let handle = core.doc_host.open(CHAT).unwrap();
    queue_as_viewer(
        handle.doc(),
        "cmd-run-parked-notice",
        SessionCommandPayload::Run {
            request: run_request("go"),
            message_id: "m-1".into(),
        },
    );

    let has_notice = |e: &SessionMessageEntry| {
        e.parts
            .iter()
            .any(|p| matches!(p, MessagePart::Notice { .. }))
    };
    wait_for(
        || entries_now(&core).iter().any(has_notice),
        "the between-turns notice to reach the doc",
    )
    .await;
    // Give any (buggy) status flip or streaming-entry write time to land.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // (a) + (b): the session is still PARKED. Working is the visible half of
    // the wedge; the reaper gate that would have leaked the child is the same
    // `idle_since` this status is derived from, and nothing else resets it.
    assert_eq!(
        core.sessions.session_status(CHAT).map(|s| s.status),
        Some(SessionStatus::Idle),
        "a between-turns notice must not restart the turn"
    );

    // (c) the notice landed in its own FINISHED entry — no chip left spinning.
    let all = entries(&core);
    let notice_entry = all.iter().find(|e| has_notice(e)).expect("notice entry");
    assert_eq!(notice_entry.role, MessageRole::Assistant);
    assert_eq!(notice_entry.status, Some(MessageStatus::Complete));
    assert!(
        all.iter()
            .all(|e| e.status != Some(MessageStatus::Streaming)),
        "no entry may be left streaming, got {all:#?}"
    );
    // The parked turn's own entry stayed separate and complete.
    assert!(all.iter().any(|e| {
        e.status == Some(MessageStatus::Complete)
            && e.parts
                .iter()
                .any(|p| matches!(p, MessagePart::Text { text, .. } if text == "first turn"))
    }));
}

/// 0b.2: a Diagnostic is bookkeeping, not turn content — a persistent Codex
/// session's unknown notification can arrive while PARKED, and it must not be
/// mistaken for turn-start (the same wedge the between-turns notice and the
/// empty heartbeat each hit: `idle_since` cleared → Working forever, reaper
/// disarmed). It also folds to NO doc part: only the registry hears it.
#[tokio::test]
async fn a_diagnostic_while_parked_is_counted_and_leaves_the_session_parked() {
    let dir = tempfile::tempdir().unwrap();
    let core = assemble(
        dir.path(),
        Arc::new(ScriptedHarness {
            script: vec![
                AgentEvent::SessionStarted {
                    harness: HarnessId::Mock,
                    model: "mock-1".into(),
                    tools: vec![],
                    cwd: "/tmp".into(),
                    session_id: "hs-1".into(),
                    assistant_message_id: "a-1".into(),
                },
                AgentEvent::TextDelta {
                    text: "first turn".into(),
                },
                // Steerable + completed ⇒ the run PARKS here…
                done(DoneStatus::Completed),
                // …then the provider ships a frame this build never heard of.
                AgentEvent::Diagnostic {
                    discriminator: "thread/checkpoint/created".into(),
                    severity: DiagnosticSeverity::Unknown,
                    code: None,
                    summary: "The agent sent a message Comet doesn't recognize.".into(),
                },
            ],
            step_delay: Duration::from_millis(20),
            hang_until_interrupt: true,
        }),
    );
    let handle = core.doc_host.open(CHAT).unwrap();
    queue_as_viewer(
        handle.doc(),
        "cmd-run-parked-diagnostic",
        SessionCommandPayload::Run {
            request: run_request("go"),
            message_id: "m-1".into(),
        },
    );

    wait_for(
        || !core.registry.diagnostics().is_empty(),
        "the diagnostic to reach the registry",
    )
    .await;
    // Give any (buggy) status flip or doc write time to land.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Counted, keyed (harness, discriminator).
    let report = core.registry.diagnostics();
    assert_eq!(report.len(), 1);
    assert_eq!(report[0].harness, HarnessId::Mock);
    assert_eq!(report[0].entries.len(), 1);
    assert_eq!(
        report[0].entries[0].discriminator,
        "thread/checkpoint/created"
    );
    assert_eq!(report[0].entries[0].count, 1);

    // Still PARKED: the diagnostic is not turn-start.
    assert_eq!(
        core.sessions.session_status(CHAT).map(|s| s.status),
        Some(SessionStatus::Idle),
        "a between-turns diagnostic must not restart the turn"
    );
    // And it never became a doc part: every entry holds only text, none is
    // left streaming.
    let all = entries(&core);
    assert!(
        all.iter().all(|e| e
            .parts
            .iter()
            .all(|p| matches!(p, MessagePart::Text { .. }))),
        "a diagnostic must never become a doc part: {all:#?}"
    );
    assert!(
        all.iter()
            .all(|e| e.status != Some(MessageStatus::Streaming)),
        "no entry may be left streaming, got {all:#?}"
    );
}

/// REGRESSION: an EMPTY reasoning delta is a pure heartbeat — persistent
/// sessions stream them between turns — but the run loop cleared `idle_since`
/// before the filter that drops them, so a heartbeat wedged a parked session
/// exactly as a notice used to: Working forever with no `Done` coming, and the
/// 30-minute reaper's select arm (gated on `idle_since.is_some()`) disabled, so
/// the child was never released.
///
/// The notice that follows is both the delivery marker (the harness emits in
/// order, so seeing it proves the heartbeat was consumed) and the sharper half
/// of the assertion: `parked_notice` is itself derived from `idle_since`, so a
/// heartbeat that clears it silently disarms the fix that keeps a between-turns
/// notice from restarting the turn.
#[tokio::test]
async fn an_empty_reasoning_heartbeat_while_parked_leaves_the_session_parked() {
    let dir = tempfile::tempdir().unwrap();
    let core = assemble(
        dir.path(),
        Arc::new(ScriptedHarness {
            script: vec![
                AgentEvent::SessionStarted {
                    harness: HarnessId::Mock,
                    model: "mock-1".into(),
                    tools: vec![],
                    cwd: "/tmp".into(),
                    session_id: "hs-1".into(),
                    assistant_message_id: "a-1".into(),
                },
                AgentEvent::TextDelta {
                    text: "first turn".into(),
                },
                // Steerable + completed ⇒ the run PARKS here instead of ending.
                done(DoneStatus::Completed),
                // The heartbeat under test: no text, so it folds to nothing and
                // never reaches the doc. Nothing but the status can observe it.
                AgentEvent::ReasoningDelta {
                    text: String::new(),
                },
                AgentEvent::Notice {
                    kind: NoticeKind::McpStatus,
                    severity: NoticeSeverity::Warning,
                    summary: "MCP server linear failed to start".into(),
                    detail: None,
                    key: Some("mcp:linear".into()),
                },
            ],
            step_delay: Duration::from_millis(20),
            // Persistent-session shape: the stream stays open past the park.
            hang_until_interrupt: true,
        }),
    );
    let handle = core.doc_host.open(CHAT).unwrap();
    queue_as_viewer(
        handle.doc(),
        "cmd-run-parked-heartbeat",
        SessionCommandPayload::Run {
            request: run_request("go"),
            message_id: "m-1".into(),
        },
    );

    let has_notice = |e: &SessionMessageEntry| {
        e.parts
            .iter()
            .any(|p| matches!(p, MessagePart::Notice { .. }))
    };
    wait_for(
        || entries_now(&core).iter().any(has_notice),
        "the post-heartbeat notice to reach the doc",
    )
    .await;
    // Give any (buggy) status flip or streaming-entry write time to land.
    tokio::time::sleep(Duration::from_millis(200)).await;

    assert_eq!(
        core.sessions.session_status(CHAT).map(|s| s.status),
        Some(SessionStatus::Idle),
        "an empty reasoning heartbeat must not restart a parked turn"
    );
    // …and the notice still took the parked path, which only holds while the
    // heartbeat left `idle_since` alone.
    let all = entries(&core);
    let notice_entry = all.iter().find(|e| has_notice(e)).expect("notice entry");
    assert_eq!(notice_entry.status, Some(MessageStatus::Complete));
    assert!(
        all.iter()
            .all(|e| e.status != Some(MessageStatus::Streaming)),
        "no entry may be left streaming, got {all:#?}"
    );
}

#[tokio::test]
async fn interrupt_stamps_streaming_entry_aborted() {
    let dir = tempfile::tempdir().unwrap();
    let core = assemble(
        dir.path(),
        Arc::new(ScriptedHarness {
            script: vec![AgentEvent::TextDelta {
                text: "partial output".into(),
            }],
            step_delay: Duration::from_millis(5),
            hang_until_interrupt: true,
        }),
    );
    let handle = core.doc_host.open(CHAT).unwrap();
    queue_as_viewer(
        handle.doc(),
        "cmd-run-hang",
        SessionCommandPayload::Run {
            request: run_request("hang"),
            message_id: "m-1".into(),
        },
    );

    // Wait until the streaming entry is visibly in the doc, then interrupt via a
    // viewer-queued durable command (based_on = the streaming entry = current turn).
    wait_for(
        || {
            entries(&core)
                .iter()
                .any(|e| e.status == Some(MessageStatus::Streaming))
        },
        "streaming entry",
    )
    .await;
    queue_as_viewer(
        handle.doc(),
        "cmd-int-1",
        SessionCommandPayload::Interrupt {},
    );

    wait_for(
        || {
            entries(&core)
                .iter()
                .any(|e| e.status == Some(MessageStatus::Aborted))
        },
        "aborted stamp",
    )
    .await;

    let all = entries(&core);
    let assistant = all
        .iter()
        .find(|e| e.role == MessageRole::Assistant)
        .unwrap();
    assert_eq!(assistant.status, Some(MessageStatus::Aborted));
    match &assistant.parts[0] {
        MessagePart::Text { text, .. } => assert_eq!(text, "partial output"),
        other => panic!("unexpected part {other:?}"),
    }
    wait_for(
        || command_status(&core, "cmd-int-1") == Some((SessionCommandStatus::Applied, None)),
        "interrupt command outcome",
    )
    .await;
    assert_eq!(
        command_status(&core, "cmd-int-1"),
        Some((SessionCommandStatus::Applied, None))
    );
    // Journal closed with a Done — nothing left to recover.
    let journal = RunJournal::open(dir.path().join("local-store/journals")).unwrap();
    assert!(journal.stale_sessions().unwrap().is_empty());
    assert_eq!(
        core.sessions.session_status(CHAT).map(|s| s.status),
        Some(SessionStatus::Idle)
    );
}

#[tokio::test]
async fn steer_with_no_live_run_falls_back_to_new_turn() {
    let dir = tempfile::tempdir().unwrap();
    let core = assemble(
        dir.path(),
        Arc::new(MockHarness {
            script: mock_script(),
        }),
    );
    let handle = core.doc_host.open(CHAT).unwrap();

    queue_as_viewer(
        handle.doc(),
        "cmd-run-1",
        SessionCommandPayload::Run {
            request: run_request("first"),
            message_id: "m-1".into(),
        },
    );
    wait_for(
        || {
            matches!(
                command_status(&core, "cmd-run-1"),
                Some((SessionCommandStatus::Applied, _))
            )
        },
        "first run applied",
    )
    .await;
    wait_for(
        || core.sessions.session_status(CHAT).map(|s| s.status) == Some(SessionStatus::Idle),
        "first run settled",
    )
    .await;

    // No live run anymore (mock finishes instantly): a steer command must fall back to
    // dispatch-as-next-turn, per comet's executor.
    queue_as_viewer(
        handle.doc(),
        "cmd-steer-1",
        SessionCommandPayload::Steer {
            prompt: "also do this".into(),
            message_id: Some("m-2".into()),
        },
    );
    wait_for(
        || {
            matches!(
                command_status(&core, "cmd-steer-1"),
                Some((SessionCommandStatus::Applied, Some(_)))
            )
        },
        "steer fallback applied",
    )
    .await;
    let (status, resolution) = command_status(&core, "cmd-steer-1").unwrap();
    assert_eq!(status, SessionCommandStatus::Applied);
    assert_eq!(resolution.as_deref(), Some("queued as new turn"));

    wait_for(
        || {
            entries(&core)
                .iter()
                .filter(|e| {
                    e.role == MessageRole::Assistant && e.status == Some(MessageStatus::Complete)
                })
                .count()
                == 2
        },
        "second assistant entry",
    )
    .await;
    // The steer prompt became a user entry with its client-minted id.
    assert!(
        entries(&core)
            .iter()
            .any(|e| e.id == "m-2" && e.role == MessageRole::User)
    );
}

#[tokio::test]
async fn processed_commands_are_skipped_on_redelivery() {
    let dir = tempfile::tempdir().unwrap();

    // Simulate a crash AFTER mark-processed but BEFORE execute/outcome: the ledger has
    // the id, the doc still says pending.
    {
        let store = DocsStore::open(dir.path().join("local-store")).unwrap();
        assert!(store.mark_processed("cmd-crashed").unwrap());
    }

    let core = assemble(
        dir.path(),
        Arc::new(MockHarness {
            script: mock_script(),
        }),
    );
    let handle = core.doc_host.open(CHAT).unwrap();
    queue_as_viewer(
        handle.doc(),
        "cmd-crashed",
        SessionCommandPayload::Run {
            request: run_request("never again"),
            message_id: "m-x".into(),
        },
    );

    // Give the drain a moment: the command must be SKIPPED — no user entry, no run.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        entries(&core).is_empty(),
        "skipped command must not execute"
    );
    assert_eq!(
        command_status(&core, "cmd-crashed"),
        Some((SessionCommandStatus::Pending, None)),
        "skip leaves the entry pending without an outcome"
    );
    assert!(core.sessions.session_status(CHAT).is_none());

    // Direct ledger-evaluation check: re-evaluating a processed command = Skip.
    let store = DocsStore::open(dir.path().join("local-store")).unwrap();
    let commands = handle.doc().read_commands().unwrap();
    let entry = commands.iter().find(|c| c.id == "cmd-crashed").unwrap();
    let is_processed = |id: &str| store.is_processed(id).unwrap_or(false);
    let never_past = |_: &str| false;
    let verdict = comet_doc::evaluate_command(
        entry,
        &comet_doc::EvaluationContext {
            is_processed: &is_processed,
            now_ms: chrono::Utc::now().timestamp_millis(),
            entries: &commands,
            current_turn_id: None,
            turn_is_past: &never_past,
        },
    );
    assert_eq!(verdict, comet_doc::CommandDisposition::Skip);
}

#[tokio::test]
async fn recover_stale_journal_stamps_aborted_on_boot() {
    let dir = tempfile::tempdir().unwrap();
    let device_id = "dev-host-fixed";
    std::fs::create_dir_all(dir.path()).unwrap();
    std::fs::write(dir.path().join("device-id"), device_id).unwrap();

    // Craft the crash state: a journal without a terminal Done + a doc snapshot whose
    // assistant entry is still `streaming`.
    {
        let journal = RunJournal::open(dir.path().join("local-store/journals")).unwrap();
        journal
            .append(
                CHAT,
                &AgentEvent::TextDelta {
                    text: "doomed".into(),
                },
            )
            .unwrap();

        let doc = SessionDoc::init(CHAT).unwrap();
        doc.push_message(&SessionMessageEntry {
            id: "m-user".into(),
            role: MessageRole::User,
            parts: vec![MessagePart::Text {
                id: "t0".into(),
                text: "hi".into(),
            }],
            created_at: 1,
            device_id: device_id.into(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
        })
        .unwrap();
        let mut writer = SegmentWriter::begin(&doc, "m-assist", device_id, 2).unwrap();
        writer
            .sync(&[MessagePart::Text {
                id: "t0".into(),
                text: "doomed".into(),
            }])
            .unwrap();
        // No finish — the "process" dies here with the entry still streaming.
        let store = DocsStore::open(dir.path().join("local-store")).unwrap();
        store
            .save_snapshot(CHAT, &doc.export_snapshot().unwrap())
            .unwrap();
    }

    // Boot: EngineCore::assemble runs recover_stale.
    let core = assemble(
        dir.path(),
        Arc::new(MockHarness {
            script: mock_script(),
        }),
    );
    assert_eq!(core.device_id, device_id);

    let all = entries(&core);
    let assistant = all.iter().find(|e| e.id == "m-assist").unwrap();
    assert_eq!(assistant.status, Some(MessageStatus::Aborted));
    match &assistant.parts[0] {
        MessagePart::Text { text, .. } => assert_eq!(text, "doomed"),
        other => panic!("unexpected part {other:?}"),
    }

    // Journal closed with a synthetic Done{interrupted}; no longer stale.
    let journal = RunJournal::open(dir.path().join("local-store/journals")).unwrap();
    assert!(journal.stale_sessions().unwrap().is_empty());
    let (_, last) = journal.last_event(CHAT).unwrap().unwrap();
    assert!(matches!(
        last,
        AgentEvent::Done {
            status: DoneStatus::Interrupted,
            ..
        }
    ));
    assert_eq!(
        core.sessions.session_status(CHAT).map(|s| s.status),
        Some(SessionStatus::Idle)
    );
}

#[tokio::test]
async fn rpc_surface_over_in_memory_transport() {
    let dir = tempfile::tempdir().unwrap();
    let core = assemble(
        dir.path(),
        Arc::new(MockHarness {
            script: mock_script(),
        }),
    );
    let client = comet_rpc::memory_client(core.rpc_service());

    // ListHarnesses + ListModels.
    let harnesses = client
        .call(comet_rpc::methods::LIST_HARNESSES, serde_json::Value::Null)
        .await
        .unwrap();
    assert_eq!(harnesses[0]["id"], "mock");
    let models = client
        .call(
            comet_rpc::methods::LIST_MODELS,
            serde_json::json!({"harness": "mock"}),
        )
        .await
        .unwrap();
    assert_eq!(models[0]["id"], "mock-1");

    // WatchSessions + WatchDocMessages streams.
    let mut sessions_stream = client
        .subscribe(comet_rpc::methods::WATCH_SESSIONS, serde_json::Value::Null)
        .await
        .unwrap();
    let first_sessions = tokio::time::timeout(Duration::from_secs(5), sessions_stream.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first_sessions, serde_json::json!([]));

    let mut messages_stream = client
        .subscribe(
            comet_rpc::methods::WATCH_DOC_MESSAGES,
            serde_json::json!({"chatId": CHAT}),
        )
        .await
        .unwrap();
    let initial = tokio::time::timeout(Duration::from_secs(5), messages_stream.recv())
        .await
        .unwrap()
        .unwrap();
    // Delta protocol: the stream opens with a full reset frame.
    assert_eq!(initial, serde_json::json!({ "reset": [] }));

    // QueueCommand (as this device's composer would over IPC).
    let command = serde_json::to_value(SessionCommandPayload::Run {
        request: run_request("via rpc"),
        message_id: "m-rpc-1".into(),
    })
    .unwrap();
    let queued = client
        .call(
            comet_rpc::methods::QUEUE_COMMAND,
            serde_json::json!({"chatId": CHAT, "command": command}),
        )
        .await
        .unwrap();
    assert!(queued["commandId"].is_string());

    // The doc-messages stream emits delta frames until the transcript settles:
    // user entry + completed assistant entry with the folded parts. Applying
    // each frame client-side mirrors what both viewports do.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut materialized: Vec<SessionMessageEntry> = vec![];
    let settled = loop {
        let item = tokio::time::timeout_at(deadline, messages_stream.recv())
            .await
            .expect("doc messages before timeout")
            .expect("stream alive");
        let frame: comet_doc::TranscriptFrame = serde_json::from_value(item).unwrap();
        comet_doc::apply_transcript_frame(&mut materialized, frame).unwrap();
        if materialized.len() == 2 && materialized[1].status == Some(MessageStatus::Complete) {
            break materialized;
        }
    };
    assert_eq!(settled[0].id, "m-rpc-1");
    assert_eq!(settled[0].role, MessageRole::User);
    match &settled[1].parts[0] {
        MessagePart::Text { text, .. } => assert_eq!(text, "Hello"),
        other => panic!("unexpected part {other:?}"),
    }

    // WatchSessions eventually reports the settled Idle session.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let item = tokio::time::timeout_at(deadline, sessions_stream.recv())
            .await
            .expect("session update before timeout")
            .expect("stream alive");
        let list: Vec<serde_json::Value> = serde_json::from_value(item).unwrap();
        if list.first().and_then(|s| s["status"].as_str()) == Some("idle") {
            break;
        }
    }
}

#[tokio::test]
async fn respond_input_resolves_pending_question() {
    // Harness that asks a question through RunControls and echoes the answer.
    struct AskingHarness;
    #[async_trait]
    impl Harness for AskingHarness {
        fn id(&self) -> HarnessId {
            HarnessId::Mock
        }
        fn display_name(&self) -> &str {
            "Asking"
        }
        fn capabilities(&self) -> HarnessCapabilities {
            HarnessCapabilities::default()
        }
        async fn models(&self) -> Result<Vec<Model>, HarnessError> {
            Ok(vec![])
        }
        async fn run(
            &self,
            _request: RunRequest,
            controls: RunControls,
        ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
            let (tx, rx) = tokio::sync::mpsc::channel::<Result<AgentEvent, HarnessError>>(16);
            tokio::spawn(async move {
                let answers = (controls.request_input)(vec![comet_proto::UserInputQuestion {
                    id: "q1".into(),
                    header: "Pick".into(),
                    question: "Which one?".into(),
                    options: vec!["a".into(), "b".into()],
                    multi_select: false,
                }])
                .await
                .unwrap_or_default();
                let picked = answers
                    .first()
                    .and_then(|a| a.labels.first().cloned())
                    .unwrap_or_else(|| "none".into());
                let _ = tx
                    .send(Ok(AgentEvent::TextDelta {
                        text: format!("picked {picked}"),
                    }))
                    .await;
                let _ = tx.send(Ok(done(DoneStatus::Completed))).await;
            });
            Ok(futures::stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|event| (event, rx))
            })
            .boxed())
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let core = assemble(dir.path(), Arc::new(AskingHarness));
    let handle = core.doc_host.open(CHAT).unwrap();
    queue_as_viewer(
        handle.doc(),
        "cmd-run-ask",
        SessionCommandPayload::Run {
            request: run_request("ask me"),
            message_id: "m-1".into(),
        },
    );

    // The input request surfaces: status AwaitingInput + an unresolved input part.
    wait_for(
        || {
            core.sessions.session_status(CHAT).map(|s| s.status)
                == Some(SessionStatus::AwaitingInput)
        },
        "awaiting input",
    )
    .await;
    wait_for(
        || {
            entries(&core).iter().any(|e| {
                e.parts.iter().any(|p| {
                    matches!(
                        p,
                        MessagePart::Input {
                            resolved: false,
                            ..
                        }
                    )
                })
            })
        },
        "input part in doc",
    )
    .await;

    // A viewer answers through the durable command queue.
    let request_id = entries(&core)
        .iter()
        .find_map(|e| {
            e.parts.iter().find_map(|p| match p {
                MessagePart::Input { request_id, .. } => Some(request_id.clone()),
                _ => None,
            })
        })
        .unwrap();
    queue_as_viewer(
        handle.doc(),
        "cmd-answer-1",
        SessionCommandPayload::RespondInput {
            request_id,
            answers: vec![comet_proto::UserInputAnswer {
                question_id: "q1".into(),
                labels: vec!["b".into()],
            }],
        },
    );

    wait_for(
        || {
            entries_now(&core).iter().any(|e| {
                e.status == Some(MessageStatus::Complete)
                    && e.parts
                        .iter()
                        .any(|p| matches!(p, MessagePart::Text { text, .. } if text == "picked b"))
            })
        },
        "answered turn to complete",
    )
    .await;
    assert_eq!(
        command_status(&core, "cmd-answer-1"),
        Some((SessionCommandStatus::Applied, None))
    );
    // The input part is marked resolved in the doc.
    assert!(entries(&core).iter().any(|e| {
        e.parts
            .iter()
            .any(|p| matches!(p, MessagePart::Input { resolved: true, .. }))
    }));
    // The run task writes the Complete entry BEFORE settling the status row —
    // wait for the transition instead of asserting the instant in between.
    wait_for(
        || core.sessions.session_status(CHAT).map(|s| s.status) == Some(SessionStatus::Idle),
        "session to settle idle",
    )
    .await;
}

/// Resilience: a RespondInput whose id matches no pending request is REJECTED
/// with a resolution (never silently dropped), the question stays live (the
/// panel persists), and a subsequent correct answer still resumes the run —
/// a wrong answer can never brick the session.
#[tokio::test(flavor = "multi_thread")]
async fn wrong_id_respond_is_rejected_and_correct_answer_still_resumes() {
    struct AskingHarness;
    #[async_trait]
    impl Harness for AskingHarness {
        fn id(&self) -> HarnessId {
            HarnessId::Mock
        }
        fn display_name(&self) -> &str {
            "Asking"
        }
        fn capabilities(&self) -> HarnessCapabilities {
            HarnessCapabilities::default()
        }
        async fn models(&self) -> Result<Vec<Model>, HarnessError> {
            Ok(vec![])
        }
        async fn run(
            &self,
            _request: RunRequest,
            controls: RunControls,
        ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
            let (tx, rx) = tokio::sync::mpsc::channel::<Result<AgentEvent, HarnessError>>(16);
            tokio::spawn(async move {
                let answers = (controls.request_input)(vec![comet_proto::UserInputQuestion {
                    id: "q1".into(),
                    header: "Pick".into(),
                    question: "Which one?".into(),
                    options: vec!["a".into(), "b".into()],
                    multi_select: false,
                }])
                .await
                .unwrap_or_default();
                let picked = answers
                    .first()
                    .and_then(|a| a.labels.first().cloned())
                    .unwrap_or_else(|| "none".into());
                let _ = tx
                    .send(Ok(AgentEvent::TextDelta {
                        text: format!("picked {picked}"),
                    }))
                    .await;
                let _ = tx.send(Ok(done(DoneStatus::Completed))).await;
            });
            Ok(futures::stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|event| (event, rx))
            })
            .boxed())
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let core = assemble(dir.path(), Arc::new(AskingHarness));
    let handle = core.doc_host.open(CHAT).unwrap();
    queue_as_viewer(
        handle.doc(),
        "cmd-run-wrong",
        SessionCommandPayload::Run {
            request: run_request("ask me"),
            message_id: "m-1".into(),
        },
    );
    wait_for(
        || {
            core.sessions.session_status(CHAT).map(|s| s.status)
                == Some(SessionStatus::AwaitingInput)
        },
        "awaiting input",
    )
    .await;
    wait_for(
        || {
            entries_now(&core).iter().any(|e| {
                e.parts.iter().any(|p| {
                    matches!(
                        p,
                        MessagePart::Input {
                            resolved: false,
                            ..
                        }
                    )
                })
            })
        },
        "input part in doc",
    )
    .await;

    // A wrong-id answer: rejected with a resolution, question still live.
    queue_as_viewer(
        handle.doc(),
        "cmd-answer-bogus",
        SessionCommandPayload::RespondInput {
            request_id: "bogus-id".into(),
            answers: vec![comet_proto::UserInputAnswer {
                question_id: "q1".into(),
                labels: vec!["a".into()],
            }],
        },
    );
    wait_for(
        || {
            command_status(&core, "cmd-answer-bogus")
                .is_some_and(|(s, _)| s != SessionCommandStatus::Pending)
        },
        "bogus answer processed",
    )
    .await;
    assert_eq!(
        command_status(&core, "cmd-answer-bogus"),
        Some((
            SessionCommandStatus::Rejected,
            Some("no pending input request".into())
        ))
    );
    // The run is still waiting and the part is still unresolved — the
    // QuestionPanel keeps presenting the real request.
    assert_eq!(
        core.sessions.session_status(CHAT).map(|s| s.status),
        Some(SessionStatus::AwaitingInput)
    );
    let request_id = entries(&core)
        .iter()
        .find_map(|e| {
            e.parts.iter().find_map(|p| match p {
                MessagePart::Input {
                    request_id,
                    resolved: false,
                    ..
                } => Some(request_id.clone()),
                _ => None,
            })
        })
        .expect("question still live after rejected answer");

    // The correct answer still resumes and completes the run.
    queue_as_viewer(
        handle.doc(),
        "cmd-answer-right",
        SessionCommandPayload::RespondInput {
            request_id,
            answers: vec![comet_proto::UserInputAnswer {
                question_id: "q1".into(),
                labels: vec!["b".into()],
            }],
        },
    );
    wait_for(
        || {
            entries_now(&core).iter().any(|e| {
                e.status == Some(MessageStatus::Complete)
                    && e.parts
                        .iter()
                        .any(|p| matches!(p, MessagePart::Text { text, .. } if text == "picked b"))
            })
        },
        "answered turn to complete",
    )
    .await;
    assert_eq!(
        core.sessions.session_status(CHAT).map(|s| s.status),
        Some(SessionStatus::Idle)
    );
}

/// Resilience: interrupting a run that is BLOCKED on a question unparks the
/// harness immediately (the pending resolver is failed with empty answers),
/// the entry settles `aborted`, the chip flips terminal (never dangles
/// unresolved), and the next run works — a blocked question can never brick
/// the session.
#[tokio::test(flavor = "multi_thread")]
async fn interrupt_unblocks_a_run_awaiting_input() {
    struct BlockingHarness;
    #[async_trait]
    impl Harness for BlockingHarness {
        fn id(&self) -> HarnessId {
            HarnessId::Mock
        }
        fn display_name(&self) -> &str {
            "Blocking"
        }
        fn capabilities(&self) -> HarnessCapabilities {
            HarnessCapabilities::default()
        }
        async fn models(&self) -> Result<Vec<Model>, HarnessError> {
            Ok(vec![])
        }
        async fn run(
            &self,
            request: RunRequest,
            controls: RunControls,
        ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
            let (tx, rx) = tokio::sync::mpsc::channel::<Result<AgentEvent, HarnessError>>(16);
            if request.prompt == "second run" {
                // The post-interrupt turn: completes immediately.
                tokio::spawn(async move {
                    let _ = tx
                        .send(Ok(AgentEvent::TextDelta {
                            text: "second done".into(),
                        }))
                        .await;
                    let _ = tx.send(Ok(done(DoneStatus::Completed))).await;
                });
            } else {
                let interrupt = controls.interrupt.clone();
                tokio::spawn(async move {
                    // Blocks on the question; an interrupt fails the resolver
                    // (empty answers) and cancels the token — like a real CLI
                    // being torn down, the stream then ends WITHOUT a Done.
                    let _ = (controls.request_input)(vec![comet_proto::UserInputQuestion {
                        id: "q1".into(),
                        header: "Pick".into(),
                        question: "Which one?".into(),
                        options: vec!["a".into(), "b".into()],
                        multi_select: false,
                    }])
                    .await;
                    interrupt.cancelled().await;
                    drop(tx);
                });
            }
            Ok(futures::stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|event| (event, rx))
            })
            .boxed())
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let core = assemble(dir.path(), Arc::new(BlockingHarness));
    let handle = core.doc_host.open(CHAT).unwrap();
    queue_as_viewer(
        handle.doc(),
        "cmd-run-block",
        SessionCommandPayload::Run {
            request: run_request("ask and block"),
            message_id: "m-1".into(),
        },
    );
    wait_for(
        || {
            core.sessions.session_status(CHAT).map(|s| s.status)
                == Some(SessionStatus::AwaitingInput)
        },
        "awaiting input",
    )
    .await;
    wait_for(
        || {
            entries_now(&core).iter().any(|e| {
                e.parts.iter().any(|p| {
                    matches!(
                        p,
                        MessagePart::Input {
                            resolved: false,
                            ..
                        }
                    )
                })
            })
        },
        "input part in doc",
    )
    .await;

    // Interrupt while blocked: settles promptly (well under the 3s grace —
    // the unparked resolver lets the harness wind down on its own).
    let start = std::time::Instant::now();
    core.sessions.interrupt(CHAT).await.unwrap();
    assert!(
        start.elapsed() < std::time::Duration::from_secs(3),
        "interrupt settled via the unparked resolver, not the grace timeout"
    );
    wait_for(
        || {
            entries_now(&core)
                .iter()
                .any(|e| e.status == Some(MessageStatus::Aborted))
        },
        "entry stamped aborted",
    )
    .await;
    // The chip is terminal — no dangling unresolved question survives the run.
    assert!(entries(&core).iter().all(|e| {
        e.parts.iter().all(|p| {
            !matches!(
                p,
                MessagePart::Input {
                    resolved: false,
                    ..
                }
            )
        })
    }));

    // And the session is usable: the next run completes.
    queue_as_viewer(
        handle.doc(),
        "cmd-run-second",
        SessionCommandPayload::Run {
            request: run_request("second run"),
            message_id: "m-2".into(),
        },
    );
    wait_for(
        || {
            entries_now(&core).iter().any(|e| {
                e.status == Some(MessageStatus::Complete)
                    && e.parts.iter().any(
                        |p| matches!(p, MessagePart::Text { text, .. } if text == "second done"),
                    )
            })
        },
        "second run to complete",
    )
    .await;
}

/// Regression (the "nothing happened after I answered" bug): a harness that
/// emits its OWN `InputRequested` (keyed by its internal id — Claude's
/// control-request id) *and* asks through `RunControls::request_input` used to
/// fold TWO input parts into the doc. The UI answers the LAST unresolved part;
/// the harness-emitted twin's id was unknown to `respond_input`'s pending map,
/// so the RespondInput doc command was rejected and the run never resumed.
/// The engine now drops harness-emitted `InputRequested` events (the input
/// bridge is the sole authority), so exactly one — answerable — part folds.
#[tokio::test(flavor = "multi_thread")]
async fn harness_emitted_input_twin_is_dropped_and_answer_resumes() {
    struct DoubleEmitHarness;
    #[async_trait]
    impl Harness for DoubleEmitHarness {
        fn id(&self) -> HarnessId {
            HarnessId::Mock
        }
        fn display_name(&self) -> &str {
            "DoubleEmit"
        }
        fn capabilities(&self) -> HarnessCapabilities {
            HarnessCapabilities::default()
        }
        async fn models(&self) -> Result<Vec<Model>, HarnessError> {
            Ok(vec![])
        }
        async fn run(
            &self,
            _request: RunRequest,
            controls: RunControls,
        ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
            let (tx, rx) = tokio::sync::mpsc::channel::<Result<AgentEvent, HarnessError>>(16);
            tokio::spawn(async move {
                let question = comet_proto::UserInputQuestion {
                    id: "q1".into(),
                    header: "Pick".into(),
                    question: "Which one?".into(),
                    options: vec!["a".into(), "b".into()],
                    multi_select: false,
                };
                // The pre-fix Claude/Codex shape: surface the question under
                // the harness's own id BEFORE asking through the bridge.
                let _ = tx
                    .send(Ok(AgentEvent::InputRequested {
                        request_id: "claude-ctrl-1".into(),
                        questions: vec![question.clone()],
                    }))
                    .await;
                let answers = (controls.request_input)(vec![question])
                    .await
                    .unwrap_or_default();
                let picked = answers
                    .first()
                    .and_then(|a| a.labels.first().cloned())
                    .unwrap_or_else(|| "none".into());
                let _ = tx
                    .send(Ok(AgentEvent::TextDelta {
                        text: format!("picked {picked}"),
                    }))
                    .await;
                let _ = tx.send(Ok(done(DoneStatus::Completed))).await;
            });
            Ok(futures::stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|event| (event, rx))
            })
            .boxed())
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let core = assemble(dir.path(), Arc::new(DoubleEmitHarness));
    let handle = core.doc_host.open(CHAT).unwrap();
    queue_as_viewer(
        handle.doc(),
        "cmd-run-twin",
        SessionCommandPayload::Run {
            request: run_request("ask me twice"),
            message_id: "m-1".into(),
        },
    );

    wait_for(
        || {
            core.sessions.session_status(CHAT).map(|s| s.status)
                == Some(SessionStatus::AwaitingInput)
        },
        "awaiting input",
    )
    .await;
    wait_for(
        || {
            entries_now(&core).iter().any(|e| {
                e.parts.iter().any(|p| {
                    matches!(
                        p,
                        MessagePart::Input {
                            resolved: false,
                            ..
                        }
                    )
                })
            })
        },
        "input part in doc",
    )
    .await;

    // Exactly ONE input part folded, and not under the harness's own id.
    let input_ids: Vec<String> = entries(&core)
        .iter()
        .flat_map(|e| {
            e.parts.iter().filter_map(|p| match p {
                MessagePart::Input { request_id, .. } => Some(request_id.clone()),
                _ => None,
            })
        })
        .collect();
    assert_eq!(input_ids.len(), 1, "one chip, not a twin: {input_ids:?}");
    assert_ne!(input_ids[0], "claude-ctrl-1");

    // Answer the LAST unresolved part — exactly what the QuestionPanel does.
    let request_id = entries(&core)
        .iter()
        .rev()
        .find_map(|e| {
            e.parts.iter().rev().find_map(|p| match p {
                MessagePart::Input {
                    request_id,
                    resolved: false,
                    ..
                } => Some(request_id.clone()),
                _ => None,
            })
        })
        .unwrap();
    queue_as_viewer(
        handle.doc(),
        "cmd-answer-twin",
        SessionCommandPayload::RespondInput {
            request_id,
            answers: vec![comet_proto::UserInputAnswer {
                question_id: "q1".into(),
                labels: vec!["a".into()],
            }],
        },
    );

    // The run resumes and completes; the chip flips to resolved.
    wait_for(
        || {
            entries_now(&core).iter().any(|e| {
                e.status == Some(MessageStatus::Complete)
                    && e.parts
                        .iter()
                        .any(|p| matches!(p, MessagePart::Text { text, .. } if text == "picked a"))
            })
        },
        "answered turn to complete",
    )
    .await;
    assert_eq!(
        command_status(&core, "cmd-answer-twin"),
        Some((SessionCommandStatus::Applied, None))
    );
    assert!(entries(&core).iter().any(|e| {
        e.parts
            .iter()
            .any(|p| matches!(p, MessagePart::Input { resolved: true, .. }))
    }));
    // The run task writes the Complete entry BEFORE settling the status row —
    // wait for the transition instead of asserting the instant in between.
    wait_for(
        || core.sessions.session_status(CHAT).map(|s| s.status) == Some(SessionStatus::Idle),
        "session to settle idle",
    )
    .await;
}

// ---------------------------------------------------------------------------
// Attachments (round 17): chunked upload → durable path → Run carrying both
// the prompt-embedded refs (the persisted transport) and the staged paths.
// ---------------------------------------------------------------------------

/// Delegates to a scripted mock but records every RunRequest the engine hands
/// over (the chat run AND the auto-title run share the harness) — proves
/// `attachments` survives doc-queue → executor → harness.
struct CapturingHarness {
    script: Vec<AgentEvent>,
    seen: Arc<std::sync::Mutex<Vec<RunRequest>>>,
}

#[async_trait]
impl Harness for CapturingHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }
    fn display_name(&self) -> &str {
        "Capturing"
    }
    fn capabilities(&self) -> HarnessCapabilities {
        HarnessCapabilities {
            supports_steering: true,
            steering_mode: SteeringMode::StepBoundary,
            reasoning_levels: vec![ReasoningLevel::Medium],
        }
    }
    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        Ok(vec![])
    }
    async fn run(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        self.seen.lock().unwrap().push(request.clone());
        MockHarness {
            script: self.script.clone(),
        }
        .run(request, controls)
        .await
    }
}

#[tokio::test]
async fn attachment_upload_then_run_threads_refs_and_paths() {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD;
    let dir = tempfile::tempdir().unwrap();
    let seen: Arc<std::sync::Mutex<Vec<RunRequest>>> = Default::default();
    let core = assemble(
        dir.path(),
        Arc::new(CapturingHarness {
            script: mock_script(),
            seen: seen.clone(),
        }),
    );
    let client = comet_rpc::memory_client(core.rpc_service());

    // Chunked upload exactly as the composer sends it: base64 split across
    // positional UploadChunk slots, then UploadCommit → the durable path.
    let payload: Vec<u8> = (0..=255u8).cycle().take(9_001).collect();
    let encoded = b64.encode(&payload);
    let (first, second) = encoded.split_at(encoded.len() / 2);
    for (seq, data) in [(0, first), (1, second)] {
        client
            .call(
                comet_rpc::methods::UPLOAD_CHUNK,
                serde_json::json!({ "uploadId": "e2e-att", "seq": seq, "data": data }),
            )
            .await
            .expect("UploadChunk");
    }
    let committed = client
        .call(
            comet_rpc::methods::UPLOAD_COMMIT,
            serde_json::json!({ "uploadId": "e2e-att", "fileName": "red.png" }),
        )
        .await
        .expect("UploadCommit");
    let path = committed["path"].as_str().expect("path").to_string();
    assert_eq!(
        std::fs::read(&path).expect("durable upload file"),
        payload,
        "committed file holds the exact reassembled bytes"
    );

    // Run with the comet `withAttachments` transport: refs embedded in the
    // prompt text (this is what persists), paths on the additive field.
    let prompt = format!(
        "what color is this?\n\nAttached images (local files — open them to view):\n- {path}"
    );
    let mut request = run_request(&prompt);
    request.attachments = vec![path.clone()];
    let handle = core.doc_host.open(CHAT).unwrap();
    queue_as_viewer(
        handle.doc(),
        "cmd-att-1",
        SessionCommandPayload::Run {
            request,
            message_id: "msg-att-1".into(),
        },
    );
    wait_for(
        || {
            entries_now(&core).iter().any(|e| {
                e.role == MessageRole::Assistant && e.status == Some(MessageStatus::Complete)
            })
        },
        "assistant entry to complete",
    )
    .await;

    // Doc user entry: the message text carries the refs verbatim (render-back
    // parses them into thumbnails).
    let all = entries(&core);
    assert_eq!(all[0].id, "msg-att-1");
    assert_eq!(all[0].role, MessageRole::User);
    match &all[0].parts[0] {
        MessagePart::Text { text, .. } => {
            assert!(text.contains("Attached images (local files"));
            assert!(text.contains(&path));
        }
        other => panic!("unexpected user part {other:?}"),
    }

    // The harness saw the staged paths on the request itself (the chat run —
    // NOT the auto-title run, which fires at dispatch now, embeds the user
    // prompt in its wrapper, and legitimately carries no attachments).
    let requests = seen.lock().unwrap().clone();
    let chat_run = requests
        .iter()
        .find(|r| r.prompt.contains("what color is this?") && !r.prompt.contains("word title"))
        .expect("chat run reached the harness");
    assert_eq!(chat_run.attachments, vec![path.clone()]);
    assert!(chat_run.prompt.contains(&path));

    // Read-back over the same RPC surface the transcript uses.
    let chunk = client
        .call(
            comet_rpc::methods::READ_ATTACHMENT_CHUNK,
            serde_json::json!({ "path": path, "offset": 0 }),
        )
        .await
        .expect("ReadAttachmentChunk");
    assert_eq!(chunk["mimeType"], "image/png");
    assert_eq!(chunk["name"], "e2e-att-red.png");
}

/// Real-CLI proof of the image pipeline: upload a tiny solid-red PNG through
/// the chunked RPC path, run claude (haiku) with the staged path on
/// `attachments` + the refs in the prompt, and check the reply names the
/// color — it can only know it by SEEING the inline image block (the sandbox
/// prompt forbids opening the file). Ignored by default: needs an installed,
/// authenticated `claude` CLI and spends real tokens.
/// Run with: `cargo test -p comet-engine --test e2e -- --ignored`
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed+authenticated claude CLI; spends tokens"]
async fn real_claude_sees_uploaded_image_inline() {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD;
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data");
    let cwd = tmp.path().join("project");
    std::fs::create_dir_all(&cwd).unwrap();

    let core = EngineCore::assemble(
        &dir,
        Arc::new(comet_engine::default_registry()),
        HarnessId::ClaudeCode,
        None,
    )
    .expect("engine core assembles");
    // Pre-title the chat so the auto-titler doesn't spend a second model call.
    core.workspace
        .create_chat(CHAT, &core.device_id, None, Some("/tmp".into()))
        .expect("create chat row");
    core.workspace
        .rename_chat(CHAT, "Pre-titled")
        .expect("rename chat");

    // 8×8 solid-red PNG, uploaded exactly as the composer does.
    const RED_PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAgAAAAICAIAAABLbSncAAAAEklEQVR4nGP4z8CAB+GTG2wAAJP0GeGuMDBnAAAAAElFTkSuQmCC";
    let client = comet_rpc::memory_client(core.rpc_service());
    client
        .call(
            comet_rpc::methods::UPLOAD_CHUNK,
            serde_json::json!({ "uploadId": "real-img", "seq": 0, "data": RED_PNG_B64 }),
        )
        .await
        .expect("UploadChunk");
    let committed = client
        .call(
            comet_rpc::methods::UPLOAD_COMMIT,
            serde_json::json!({ "uploadId": "real-img", "fileName": "swatch.png" }),
        )
        .await
        .expect("UploadCommit");
    let path = committed["path"].as_str().expect("path").to_string();
    assert_eq!(
        std::fs::read(&path).expect("committed file"),
        b64.decode(RED_PNG_B64).unwrap()
    );

    let prompt = format!(
        "Without running any tools or opening any files, answer from the attached image alone: \
         what solid color is this image? Reply with exactly one lowercase word.\n\n\
         Attached images (local files — open them to view):\n- {path}"
    );
    let request = RunRequest {
        prompt,
        model: Some("haiku".into()),
        reasoning: None,
        model_options: Default::default(),
        cwd: cwd.to_string_lossy().to_string(),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: false,
        attachments: vec![path],
        resume: None,
    };
    core.doc_host
        .queue_command(
            CHAT,
            SessionCommandPayload::Run {
                request,
                message_id: "msg-img-1".into(),
            },
        )
        .expect("queue real image run");
    wait_for_within_secs(
        || {
            entries_now(&core).iter().any(|e| {
                e.role == MessageRole::Assistant && e.status == Some(MessageStatus::Complete)
            })
        },
        "real claude image turn",
        120,
    )
    .await;

    let reply: String = entries(&core)
        .iter()
        .filter(|e| e.role == MessageRole::Assistant)
        .flat_map(|e| e.parts.iter())
        .filter_map(|p| match p {
            MessagePart::Text { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    assert!(
        reply.contains("red"),
        "claude should name the image's color; got: {reply:?}"
    );
    core.shutdown().await;
}

async fn wait_for_within_secs<F>(mut predicate: F, what: &str, secs: u64)
where
    F: FnMut() -> bool,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    while !predicate() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ---------------------------------------------------------------------------
// Liveness heartbeats: empty reasoning deltas keep the session fresh but
// never reach the journal (redacted thinking + tool-input-generation noise).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn empty_reasoning_deltas_are_heartbeats_not_journal_noise() {
    let mut script = vec![AgentEvent::SessionStarted {
        harness: HarnessId::Mock,
        model: "mock-1".into(),
        tools: vec![],
        cwd: "/tmp".into(),
        session_id: "hs-hb".into(),
        assistant_message_id: "a-hb".into(),
    }];
    // A long "silent" stretch: redacted thinking / input_json_delta windows
    // stream as empty reasoning deltas.
    for _ in 0..40 {
        script.push(AgentEvent::ReasoningDelta {
            text: String::new(),
        });
    }
    script.push(AgentEvent::ReasoningDelta {
        text: "planning".into(),
    });
    script.push(AgentEvent::TextDelta {
        text: "done".into(),
    });
    script.push(AgentEvent::Done {
        status: DoneStatus::Completed,
        result: Some("done".into()),
        error: None,
        session_id: None,
    });
    let dir = tempfile::tempdir().unwrap();
    let core = assemble(dir.path(), Arc::new(MockHarness { script }));
    let handle = core.doc_host.open(CHAT).unwrap();
    queue_as_viewer(
        handle.doc(),
        "cmd-hb-1",
        SessionCommandPayload::Run {
            request: run_request("hb"),
            message_id: "msg-hb-1".into(),
        },
    );
    wait_for(
        || {
            entries(&core)
                .iter()
                .any(|e| e.status == Some(MessageStatus::Complete))
        },
        "run completes",
    )
    .await;
    // Journal replay: the 40 empties were filtered; real content survived.
    let replay = core.sessions.subscribe(CHAT, 0).unwrap().0;
    let empties = replay
        .iter()
        .filter(|j| matches!(&j.event, AgentEvent::ReasoningDelta { text } if text.is_empty()))
        .count();
    let nonempty = replay
        .iter()
        .filter(|j| matches!(&j.event, AgentEvent::ReasoningDelta { text } if !text.is_empty()))
        .count();
    assert_eq!(empties, 0, "empty reasoning deltas never reach the journal");
    assert_eq!(nonempty, 1, "real reasoning text is preserved");
    assert!(
        replay
            .iter()
            .any(|j| matches!(&j.event, AgentEvent::TextDelta { text } if text == "done")),
        "text deltas unaffected"
    );
}

/// The diagnostics surface is pull-only: `ListHarnessDiagnostics` answers
/// straight from the registry, like `ListHarnesses` — no push channel, a
/// few-seconds-stale count is harmless.
#[tokio::test]
async fn list_harness_diagnostics_answers_from_the_registry() {
    let dir = tempfile::tempdir().unwrap();
    let core = assemble(
        dir.path(),
        Arc::new(ScriptedHarness {
            script: vec![],
            step_delay: Duration::from_millis(1),
            hang_until_interrupt: false,
        }),
    );
    core.registry.record_diagnostic(
        HarnessId::Mock,
        "thread/checkpoint/created",
        DiagnosticSeverity::Unknown,
    );
    core.registry.record_diagnostic(
        HarnessId::Mock,
        "thread/checkpoint/created",
        DiagnosticSeverity::Unknown,
    );

    let reply = core
        .rpc_service()
        .handle(
            comet_rpc::methods::LIST_HARNESS_DIAGNOSTICS,
            serde_json::Value::Null,
        )
        .await
        .expect("method answers");
    let comet_rpc::RpcReply::Value(value) = reply else {
        panic!("expected a unary reply");
    };
    let report: Vec<comet_engine::registry::HarnessDiagnostics> =
        serde_json::from_value(value).expect("reply decodes");
    assert_eq!(report.len(), 1);
    assert_eq!(report[0].harness, HarnessId::Mock);
    assert_eq!(
        report[0].entries[0].discriminator,
        "thread/checkpoint/created"
    );
    assert_eq!(report[0].entries[0].count, 2);
    assert_eq!(report[0].overflow, 0);
}
