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
use comet_harness::discovery::{DiscoveredModel, Discovery, DiscoveryFailure};
use comet_harness::mock::MockHarness;
use comet_harness::{Harness, HarnessError, RunControls};
use comet_proto::{
    AgentEvent, ApprovalDecision, ApprovalRequest, CatalogSource, DiagnosticSeverity, DoneStatus,
    FileOperation, HarnessCapabilities, HarnessId, ModelCatalog, NoticeKind, NoticeSeverity,
    ReasoningLevel, RunRequest, RuntimeMode, SandboxLevel, SessionStatus, SteeringMode,
    SubagentStatus, ToolCall,
};
use comet_rpc::RpcService;
use comet_sync::DocsStore;

const CHAT: &str = "chat-e2e";
const VIEWER: &str = "viewer-device";

fn run_request(prompt: &str) -> RunRequest {
    RunRequest {
        prompt: prompt.into(),
        harness: None,
        model: None,
        reasoning: None,
        model_options: Default::default(),
        cwd: "/tmp".into(),
        runtime_mode: RuntimeMode::default(),
        sandbox: SandboxLevel::WorkspaceWrite,
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
            runtime_mode: comet_proto::RuntimeMode::default(),
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
            diff: None,
            diff_ref: None,
            diff_stats: None,
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
            runtime_modes: Vec::new(),
            ..HarnessCapabilities::default()
        }
    }
    async fn models(&self) -> Result<ModelCatalog, HarnessError> {
        Ok(ModelCatalog::built_in(vec![]))
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

/// Every Text and Error part's message, across every entry, joined — for
/// substring assertions about what the transcript says without caring which
/// entry it landed in. Shared by `entries_text` and `entries_text_now` so
/// the join logic can't drift between the panicking and tolerant readers.
fn join_text(entries: &[SessionMessageEntry]) -> String {
    entries
        .iter()
        .flat_map(|e| e.parts.iter())
        .filter_map(|p| match p {
            MessagePart::Text { text, .. } => Some(text.as_str()),
            MessagePart::Error { message, .. } => Some(message.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn entries_text(core: &EngineCore) -> String {
    join_text(&entries(core))
}

/// Tolerant counterpart to `entries_text`, for a hot-polling predicate — see
/// `entries_now`'s comment for why a mid-write snapshot must not panic here.
fn entries_text_now(core: &EngineCore) -> String {
    join_text(&entries_now(core))
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
        Arc::new(MockHarness::with_script(mock_script())),
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
                    runtime_mode: comet_proto::RuntimeMode::default(),
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
                    runtime_mode: comet_proto::RuntimeMode::default(),
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
                    runtime_mode: comet_proto::RuntimeMode::default(),
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
        Arc::new(MockHarness::with_script(mock_script())),
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

/// A mode change applies to the next dispatch, including one that takes the
/// remembered-request fallback. The remembered request is stamped at the
/// previous dispatch and never updated, so without the overlay a user who
/// tightens a chat to `approval-required` and then steers would still run
/// under the looser mode the last turn used — the divergence `docs/debt/README.md` D11
/// records, and it runs in the permissive direction.
#[tokio::test]
async fn a_tightened_mode_reaches_a_steer_turned_run() {
    let dir = tempfile::tempdir().unwrap();
    let core = assemble(
        dir.path(),
        Arc::new(MockHarness::with_script(mock_script())),
    );
    let config = |mode: RuntimeMode| comet_proto::ChatConfig {
        harness: HarnessId::Mock,
        model: None,
        reasoning: None,
        model_options: Default::default(),
        sandbox: mode.sandbox(),
        runtime_mode: mode,
    };
    core.workspace
        .create_space("space-mode", &core.device_id, "/tmp", None, false)
        .expect("create space row");
    core.workspace
        .create_chat(
            CHAT,
            "space-mode",
            Some(config(RuntimeMode::AutoAcceptEdits)),
            Some("/tmp".into()),
        )
        .expect("create chat row");

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
        || core.sessions.session_status(CHAT).map(|s| s.status) == Some(SessionStatus::Idle),
        "first run settled",
    )
    .await;
    assert_eq!(
        core.sessions.last_request(CHAT).map(|r| r.runtime_mode),
        Some(RuntimeMode::AutoAcceptEdits),
        "the first turn stamped the mode it ran under"
    );

    // The user tightens the chat between turns.
    core.workspace
        .set_chat_config(CHAT, &config(RuntimeMode::ApprovalRequired))
        .expect("tighten the mode");

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

    let dispatched = core.sessions.last_request(CHAT).expect("a second dispatch");
    assert_eq!(dispatched.runtime_mode, RuntimeMode::ApprovalRequired);
    // The sandbox is re-derived rather than carried over, so the request's two
    // permission fields cannot be dispatched disagreeing.
    assert_eq!(dispatched.sandbox, SandboxLevel::ReadOnly);
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
        Arc::new(MockHarness::with_script(mock_script())),
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
        Arc::new(MockHarness::with_script(mock_script())),
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
        Arc::new(MockHarness::with_script(mock_script())),
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
    assert_eq!(models["models"][0]["id"], "mock-1");

    // ListCommands answers an OBJECT, not a bare array. `ListModels` shipped as
    // an array in 0.1 and had to be reshaped in 2.1, which broke the picker at
    // run time while every test stayed green (AGENTS.md, "Changing what an RPC
    // method answers with") — this untyped assertion is one of the consumers
    // that rule is about.
    let commands = client
        .call(
            comet_rpc::methods::LIST_COMMANDS,
            serde_json::json!({"harness": "mock", "cwd": "/tmp"}),
        )
        .await
        .unwrap();
    assert!(
        commands["commands"].is_array(),
        "expected {{commands: []}}, got {commands}"
    );
    // A harness with no command surface answers an empty list rather than
    // failing: the mock has none, and neither does Codex.
    assert_eq!(commands["commands"].as_array().unwrap().len(), 0);

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
        async fn models(&self) -> Result<ModelCatalog, HarnessError> {
            Ok(ModelCatalog::built_in(vec![]))
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
        async fn models(&self) -> Result<ModelCatalog, HarnessError> {
            Ok(ModelCatalog::built_in(vec![]))
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
        async fn models(&self) -> Result<ModelCatalog, HarnessError> {
            Ok(ModelCatalog::built_in(vec![]))
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
        async fn models(&self) -> Result<ModelCatalog, HarnessError> {
            Ok(ModelCatalog::built_in(vec![]))
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
            runtime_modes: Vec::new(),
            ..HarnessCapabilities::default()
        }
    }
    async fn models(&self) -> Result<ModelCatalog, HarnessError> {
        Ok(ModelCatalog::built_in(vec![]))
    }
    async fn run(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        self.seen.lock().unwrap().push(request.clone());
        MockHarness::with_script(self.script.clone())
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
    // `rename_chat` also stamps `titleManual`, so this skips the titler via
    // the manual-rename lock, not the plain "already has a title" check —
    // fine for this test's purpose, but not coverage of that other property
    // (`comet_doc::workspace`'s own tests own it).
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
        harness: None,
        model: Some("haiku".into()),
        reasoning: None,
        model_options: Default::default(),
        cwd: cwd.to_string_lossy().to_string(),
        runtime_mode: RuntimeMode::default(),
        sandbox: SandboxLevel::WorkspaceWrite,
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

/// Slice 4.3's evidence, since nothing renders: drive a REAL Claude run that
/// uses the task tools, then read back both the persisted document and the run
/// journal. Same standard 1.4 and 4.2 were held to.
///
/// The unit tests fold events straight into a parts vector and the harness
/// tests stop at the event stream; neither proves a checklist survives the
/// engine's own write path into a real Loro document.
///
/// Ignored by default: needs an installed, authenticated `claude` CLI and
/// spends real tokens (haiku, one short turn).
/// Run with: `cargo test -p comet-engine --test e2e -- --ignored`
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed+authenticated claude CLI; spends tokens"]
async fn real_claude_task_tools_persist_a_checklist() {
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
    core.workspace
        .create_space("space-checklist", &core.device_id, "/tmp", None, false)
        .expect("create space row");
    core.workspace
        .create_chat(CHAT, "space-checklist", None, Some("/tmp".into()))
        .expect("create chat row");
    // Pre-title the chat so the auto-titler does not spend a second model
    // call. `rename_chat` also stamps `titleManual`, so this skips the
    // titler via the manual-rename lock, not the plain "already has a title"
    // check — see the note at this file's other `rename_chat(CHAT, ...)`
    // pre-title call.
    core.workspace
        .rename_chat(CHAT, "Pre-titled")
        .expect("rename chat");

    // The same instruction shape the capture scenario uses, and for the same
    // reason: the task tools are DEFERRED on at least one machine, so a prompt
    // that does not name them can produce a run with no checklist at all.
    let request = RunRequest {
        prompt: concat!(
            "Use ToolSearch exactly once with input ",
            r#"{"query":"select:TaskCreate,TaskUpdate","max_results":5}. "#,
            "Then use TaskCreate exactly twice, first with input ",
            r#"{"subject":"Alpha step","description":"The first step"} "#,
            "and then with input ",
            r#"{"subject":"Beta step","description":"The second step"}. "#,
            "Then use TaskUpdate exactly once with input ",
            r#"{"taskId":"1","status":"in_progress","activeForm":"Working the first step"}. "#,
            "Then use TaskUpdate exactly once with input ",
            r#"{"taskId":"1","status":"completed"}. "#,
            "Do nothing else, and reply with the single word planned."
        )
        .into(),
        harness: None,
        model: Some("haiku".into()),
        reasoning: None,
        model_options: Default::default(),
        cwd: cwd.to_string_lossy().to_string(),
        runtime_mode: RuntimeMode::default(),
        sandbox: SandboxLevel::WorkspaceWrite,
        attachments: Vec::new(),
        resume: None,
    };
    core.doc_host
        .queue_command(
            CHAT,
            SessionCommandPayload::Run {
                request,
                message_id: "msg-checklist-1".into(),
            },
        )
        .expect("queue real checklist run");
    wait_for_within_secs(
        || {
            entries_now(&core).iter().any(|e| {
                e.role == MessageRole::Assistant && e.status == Some(MessageStatus::Complete)
            })
        },
        "real claude checklist turn",
        180,
    )
    .await;

    // 1. The PERSISTED DOCUMENT holds exactly one checklist, with the first
    //    item carried all the way to completed and its subject intact through
    //    a status-only final update.
    let checklists: Vec<_> = entries(&core)
        .iter()
        .flat_map(|e| e.parts.iter())
        .filter_map(|p| match p {
            MessagePart::Checklist { items, .. } => Some(items.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        checklists.len(),
        1,
        "one checklist per run, not one per publication: {checklists:?}"
    );
    let items = &checklists[0];
    assert!(
        items.len() >= 2,
        "expected the two created tasks: {items:?}"
    );
    let first = items
        .iter()
        .find(|i| i.id == "1")
        .unwrap_or_else(|| panic!("task 1 missing from {items:?}"));
    assert_eq!(
        first.status,
        comet_proto::ChecklistStatus::Completed,
        "{items:?}"
    );
    assert_eq!(
        first.text.as_deref(),
        Some("Alpha step"),
        "the status-only completion must not blank the subject: {items:?}"
    );
    assert_eq!(
        first.active_form.as_deref(),
        Some("Working the first step"),
        "nor the activeForm the in_progress frame set: {items:?}"
    );

    // 2. The RUN JOURNAL recorded the mutations themselves, which is what a
    //    replay or a LAN peer reconstructs the card from.
    let journal = dir
        .join("local-store")
        .join("journals")
        .join(format!("{CHAT}.jsonl"));
    let raw = std::fs::read_to_string(&journal).expect("journal file");
    let changes = raw
        .lines()
        .filter(|l| l.contains("\"checklistItemChanged\""))
        .count();
    assert!(
        changes >= 3,
        "expected at least two creates and one update in {}: got {changes}",
        journal.display()
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
        runtime_mode: comet_proto::RuntimeMode::default(),
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
    let core = assemble(dir.path(), Arc::new(MockHarness::with_script(script)));
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
// ---------------------------------------------------------------------------
// Approvals: request → doc part → decision → harness, plus every terminal path.
// ---------------------------------------------------------------------------

/// A harness that asks permission once and reports the answer it got, so the
/// closing text is proof the decision crossed the oneshot rather than merely
/// having been stamped into the doc.
struct ApprovingHarness;

#[async_trait]
impl Harness for ApprovingHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }
    fn display_name(&self) -> &str {
        "Approving"
    }
    fn capabilities(&self) -> HarnessCapabilities {
        HarnessCapabilities::default()
    }
    async fn models(&self) -> Result<ModelCatalog, HarnessError> {
        Ok(ModelCatalog::built_in(vec![]))
    }
    async fn run(
        &self,
        _request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let request_approval = controls.request_approval;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
        tokio::spawn(async move {
            let decision = request_approval(ApprovalRequest::FileChange {
                path: "src/reconcile.rs".into(),
                operation: FileOperation::Modify,
                added_lines: 24,
                removed_lines: 6,
            })
            .await;
            let closing = match decision {
                Ok(ApprovalDecision::Allow) | Ok(ApprovalDecision::AllowForSession) => {
                    "applied the edit"
                }
                Ok(ApprovalDecision::Deny { .. }) => "left the file untouched",
                Ok(ApprovalDecision::Expired) | Err(_) => "stopped without the edit",
            };
            let _ = tx.send(AgentEvent::TextDelta {
                text: closing.into(),
            });
            let _ = tx.send(AgentEvent::Done {
                status: DoneStatus::Completed,
                result: None,
                error: None,
                session_id: None,
            });
        });
        Ok(futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|event| (Ok(event), rx))
        })
        .boxed())
    }
}

/// Drive a run to a pending approval and hand back the request id off the doc.
async fn drive_to_open_approval(
    core: &EngineCore,
    handle: &comet_engine::ChatDocHandle,
    cmd: &str,
) -> String {
    queue_as_viewer(
        handle.doc(),
        cmd,
        SessionCommandPayload::Run {
            request: run_request("edit it"),
            message_id: "m-1".into(),
        },
    );
    wait_for(
        || {
            core.sessions.session_status(CHAT).map(|s| s.status)
                == Some(SessionStatus::AwaitingInput)
        },
        "awaiting approval",
    )
    .await;
    wait_for(
        || {
            entries_now(core).iter().any(|e| {
                e.parts
                    .iter()
                    .any(|p| matches!(p, MessagePart::Approval { decision: None, .. }))
            })
        },
        "open approval part in doc",
    )
    .await;
    entries_now(core)
        .iter()
        .find_map(|e| {
            e.parts.iter().find_map(|p| match p {
                MessagePart::Approval { request_id, .. } => Some(request_id.clone()),
                _ => None,
            })
        })
        .expect("approval part carries a request id")
}

/// Asks for the same file change twice and reports what it was told each time.
/// The second request is the one under test: after AllowForSession the user
/// must not be asked again, and the run must still be told "allowed".
struct TwiceAskingHarness;

#[async_trait]
impl Harness for TwiceAskingHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }
    fn display_name(&self) -> &str {
        "TwiceAsking"
    }
    fn capabilities(&self) -> HarnessCapabilities {
        HarnessCapabilities::default()
    }
    async fn models(&self) -> Result<ModelCatalog, HarnessError> {
        Ok(ModelCatalog::built_in(vec![]))
    }
    async fn run(
        &self,
        _request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let request_approval = controls.request_approval;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
        tokio::spawn(async move {
            let ask = || {
                request_approval(ApprovalRequest::FileChange {
                    path: "src/reconcile.rs".into(),
                    operation: FileOperation::Modify,
                    added_lines: 24,
                    removed_lines: 6,
                })
            };
            let first = ask().await;
            let second = ask().await;
            let word = |d: &Result<ApprovalDecision, _>| match d {
                Ok(ApprovalDecision::Allow) | Ok(ApprovalDecision::AllowForSession) => "allowed",
                Ok(ApprovalDecision::Deny { .. }) => "denied",
                _ => "unanswered",
            };
            let _ = tx.send(AgentEvent::TextDelta {
                text: format!("{} then {}", word(&first), word(&second)),
            });
            let _ = tx.send(AgentEvent::Done {
                status: DoneStatus::Completed,
                result: None,
                error: None,
                session_id: None,
            });
        });
        Ok(futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|event| (Ok(event), rx))
        })
        .boxed())
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn allow_for_session_answers_the_next_identical_request_without_asking() {
    let dir = tempfile::tempdir().unwrap();
    let core = assemble(dir.path(), Arc::new(TwiceAskingHarness));
    let handle = core.doc_host.open(CHAT).unwrap();

    // Drive to the first open approval and answer it AllowForSession.
    let request_id = drive_to_open_approval(&core, &handle, "cmd-1").await;
    queue_as_viewer(
        handle.doc(),
        "cmd-2",
        SessionCommandPayload::RespondApproval {
            request_id: request_id.clone(),
            decision: ApprovalDecision::AllowForSession,
        },
    );

    // The run must finish without a second question reaching the user.
    wait_for(
        || {
            entries_now(&core).iter().any(|e| {
                e.parts.iter().any(|p| {
                    matches!(p, MessagePart::Text { text, .. } if text.contains("allowed then allowed"))
                })
            })
        },
        "the second identical request answers itself",
    )
    .await;

    // Both are VISIBLE: two approval parts, both carrying a decision. An
    // auto-allowed action the user cannot see is the failure 0b exists to stop.
    let approvals: Vec<_> = entries_now(&core)
        .iter()
        .flat_map(|e| e.parts.iter())
        .filter_map(|p| match p {
            MessagePart::Approval { decision, .. } => Some(decision.clone()),
            _ => None,
        })
        .collect();
    // …and each says WHICH allow it was. Both read "Allowed for this session":
    // the first because that is what the user clicked, the second because a
    // card reading "Allowed" for an action the user never saw would be a false
    // record — and the record is the only reason the second card exists.
    assert_eq!(
        approvals,
        vec![
            Some(ApprovalDecision::AllowForSession),
            Some(ApprovalDecision::AllowForSession),
        ],
        "the user's grant and the auto-allow made under it, in that order"
    );
}

/// Asks permission and then takes a steer instead of an answer — what a user
/// typing over an open card produces, and what claude emits the moment a steer
/// line is queued. Reports what the parked request finally resolved to, so a
/// resolver left parked shows up as a report that never arrives.
struct SteeredWhileAskingHarness;

#[async_trait]
impl Harness for SteeredWhileAskingHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }
    fn display_name(&self) -> &str {
        "SteeredWhileAsking"
    }
    fn capabilities(&self) -> HarnessCapabilities {
        HarnessCapabilities {
            supports_steering: true,
            steering_mode: SteeringMode::StepBoundary,
            reasoning_levels: vec![ReasoningLevel::Medium],
            runtime_modes: Vec::new(),
            ..HarnessCapabilities::default()
        }
    }
    async fn models(&self) -> Result<ModelCatalog, HarnessError> {
        Ok(ModelCatalog::built_in(vec![]))
    }
    async fn run(
        &self,
        _request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let request_approval = controls.request_approval;
        let mut steering = controls.steering;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
        tokio::spawn(async move {
            let pending = request_approval(ApprovalRequest::FileChange {
                path: "src/reconcile.rs".into(),
                operation: FileOperation::Modify,
                added_lines: 24,
                removed_lines: 6,
            });
            let Some(steer) = steering.recv().await else {
                return;
            };
            let _ = tx.send(AgentEvent::Steered {
                assistant_message_id: None,
                next_assistant_message_id: steer.message_id.map(|id| format!("a-{id}")),
            });
            let word = match pending.await {
                Ok(ApprovalDecision::Allow) | Ok(ApprovalDecision::AllowForSession) => "allowed",
                Ok(ApprovalDecision::Deny { .. }) => "denied",
                Ok(ApprovalDecision::Expired) | Err(_) => "abandoned",
            };
            let _ = tx.send(AgentEvent::TextDelta {
                text: format!("steered, edit {word}"),
            });
            let _ = tx.send(AgentEvent::Done {
                status: DoneStatus::Completed,
                result: None,
                error: None,
                session_id: None,
            });
        });
        Ok(futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|event| (Ok(event), rx))
        })
        .boxed())
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_steer_over_an_open_approval_terminates_it_rather_than_stranding_it() {
    // A steer finishes the assistant entry the card lives in. Nothing can reach
    // a part once it has left the accumulator — not a later decision, not the
    // Done-time sweep, not the decision row — so the card would read "waiting"
    // for the life of the chat while the CLI stayed blocked on the tool call.
    let dir = tempfile::tempdir().unwrap();
    let core = assemble(dir.path(), Arc::new(SteeredWhileAskingHarness));
    let handle = core.doc_host.open(CHAT).unwrap();
    let request_id = drive_to_open_approval(&core, &handle, "cmd-run-steered").await;

    // The user types instead of answering: a routed dispatch steers the live run.
    queue_as_viewer(
        handle.doc(),
        "cmd-steer-1",
        SessionCommandPayload::Run {
            request: run_request("actually, do this instead"),
            message_id: "m-steer".into(),
        },
    );

    // Only reachable if the resolver was released: the harness reports nothing
    // until its parked request resolves one way or the other.
    wait_for(
        || {
            entries_now(&core).iter().any(|e| {
                e.parts.iter().any(
                    |p| matches!(p, MessagePart::Text { text, .. } if text == "steered, edit abandoned"),
                )
            })
        },
        "the steered-over request to resolve",
    )
    .await;

    // And the transcript says so, in the entry the steer finished.
    wait_for(
        || {
            entries_now(&core).iter().any(|e| {
                e.parts.iter().any(|p| {
                    matches!(
                        p,
                        MessagePart::Approval {
                            request_id: rid,
                            decision: Some(ApprovalDecision::Expired),
                            ..
                        } if *rid == request_id
                    )
                })
            })
        },
        "the steered-over card to be stamped expired",
    )
    .await;
}

/// A harness whose child keeps running across a step-boundary steer — the
/// real Claude behavior (`crates/harness/src/claude/mod.rs`'s steer arm queues
/// a stdin line and emits `Steered`, nothing more; only the separate,
/// mutually-exclusive abort arm signals interrupt). It never sends a
/// `SubagentUpdated` for the task it starts, which is the point: a live
/// subagent's fate is unknown at a steer boundary, unlike an approval, whose
/// resolver the sweep itself drops.
struct SteeredWhileSubagentRunningHarness;

#[async_trait]
impl Harness for SteeredWhileSubagentRunningHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }
    fn display_name(&self) -> &str {
        "SteeredWhileSubagentRunning"
    }
    fn capabilities(&self) -> HarnessCapabilities {
        HarnessCapabilities {
            supports_steering: true,
            steering_mode: SteeringMode::StepBoundary,
            reasoning_levels: vec![ReasoningLevel::Medium],
            runtime_modes: Vec::new(),
            ..HarnessCapabilities::default()
        }
    }
    async fn models(&self) -> Result<ModelCatalog, HarnessError> {
        Ok(ModelCatalog::built_in(vec![]))
    }
    async fn run(
        &self,
        _request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let mut steering = controls.steering;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
        tokio::spawn(async move {
            let _ = tx.send(AgentEvent::SubagentStarted {
                task_id: "t1".into(),
                tool_use_id: "tu1".into(),
                agent_type: "general-purpose".into(),
                description: "Read README and report first heading".into(),
                prompt: None,
            });
            let Some(steer) = steering.recv().await else {
                return;
            };
            let _ = tx.send(AgentEvent::Steered {
                assistant_message_id: None,
                next_assistant_message_id: steer.message_id.map(|id| format!("a-{id}")),
            });
            let _ = tx.send(AgentEvent::TextDelta {
                text: "steered".into(),
            });
            let _ = tx.send(AgentEvent::Done {
                status: DoneStatus::Completed,
                result: None,
                error: None,
                session_id: None,
            });
        });
        Ok(futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|event| (Ok(event), rx))
        })
        .boxed())
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_steer_over_a_running_subagent_does_not_stamp_it_cancelled() {
    // The regression this covers: a steer is not a run end, so it must not
    // claim to know a still-running child's fate. Only `expire_open_approvals`
    // runs at the `Steered` boundary in `drive_run`; `cancel_running_subagents`
    // runs only at `Done`, which this harness never reaches while the subagent
    // is still open.
    let dir = tempfile::tempdir().unwrap();
    let core = assemble(dir.path(), Arc::new(SteeredWhileSubagentRunningHarness));
    let handle = core.doc_host.open(CHAT).unwrap();

    queue_as_viewer(
        handle.doc(),
        "cmd-run-subagent",
        SessionCommandPayload::Run {
            request: run_request("delegate this"),
            message_id: "m-user".into(),
        },
    );

    wait_for(
        || {
            entries_now(&core).iter().any(|e| {
                e.parts
                    .iter()
                    .any(|p| matches!(p, MessagePart::Subagent { .. }))
            })
        },
        "the subagent card to land",
    )
    .await;

    // The user types instead of waiting: a routed dispatch steers the live run.
    queue_as_viewer(
        handle.doc(),
        "cmd-steer-1",
        SessionCommandPayload::Run {
            request: run_request("actually, do this instead"),
            message_id: "m-steer".into(),
        },
    );

    wait_for(
        || entries_text_now(&core).contains("steered"),
        "the post-steer turn to complete",
    )
    .await;

    let subagent_status = entries_now(&core)
        .iter()
        .flat_map(|e| e.parts.iter())
        .find_map(|p| match p {
            MessagePart::Subagent { status, .. } => Some(*status),
            _ => None,
        })
        .expect("subagent card still present");
    assert_eq!(
        subagent_status,
        SubagentStatus::Running,
        "a steer must not claim to know a still-running child's fate"
    );
}

/// A cleanly completed turn that leaves its child running — Claude's real
/// shape, not a contrived one.
///
/// Replays the event ORDER of a captured 2.1.246 run (2026-08-26, one `Agent`
/// delegation), journal seq in comments. The parent's turn ends `Completed`
/// while the background agent is still working, carrying Claude's own
/// "Agent is running. Waiting for completion notification." as its result, and
/// the child reports its real outcome afterwards.
struct BackgroundSubagentOutlivesTurnHarness;

#[async_trait]
impl Harness for BackgroundSubagentOutlivesTurnHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }
    fn display_name(&self) -> &str {
        "BackgroundSubagentOutlivesTurn"
    }
    fn capabilities(&self) -> HarnessCapabilities {
        HarnessCapabilities {
            supports_steering: true,
            steering_mode: SteeringMode::StepBoundary,
            reasoning_levels: vec![ReasoningLevel::Medium],
            runtime_modes: Vec::new(),
            ..HarnessCapabilities::default()
        }
    }
    async fn models(&self) -> Result<ModelCatalog, HarnessError> {
        Ok(ModelCatalog::built_in(vec![]))
    }
    async fn run(
        &self,
        _request: RunRequest,
        _controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
        tokio::spawn(async move {
            // seq 46
            let _ = tx.send(AgentEvent::SubagentStarted {
                task_id: "t1".into(),
                tool_use_id: "tu1".into(),
                agent_type: "general-purpose".into(),
                description: "List current directory and count entries".into(),
                prompt: None,
            });
            // seq 48
            let _ = tx.send(AgentEvent::SubagentUpdated {
                task_id: "t1".into(),
                status: SubagentStatus::Running,
                activity: None,
                summary: None,
                total_tokens: None,
                duration_ms: None,
                tool_uses: None,
            });
            let _ = tx.send(AgentEvent::TextDelta {
                text: "delegated".into(),
            });
            // seq 57 — the turn ends CLEANLY with the child still live.
            let _ = tx.send(AgentEvent::Done {
                status: DoneStatus::Completed,
                result: Some("Agent is running. Waiting for completion notification.".into()),
                error: None,
                session_id: None,
            });
        });
        Ok(futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|event| (Ok(event), rx))
        })
        .boxed())
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cleanly_completed_turn_does_not_cancel_a_still_running_subagent() {
    // The regression: `cancel_running_subagents` used to run on EVERY `Done`,
    // on the premise that Claude's `Agent` tool is synchronous with its
    // parent's turn. It is not. A real 2.1.246 run completed its turn with the
    // child still working and the child reported `completed` — with its answer
    // and full usage — four events later. The sweep had already stamped it
    // `Cancelled`, which is precisely the manufactured outcome
    // `cancel_running_subagents`'s own doc forbids.
    //
    // `Running` is the honest reading here: the turn ended without Comet
    // seeing how the child finished. What it must NOT be is `Cancelled`.
    let dir = tempfile::tempdir().unwrap();
    let core = assemble(dir.path(), Arc::new(BackgroundSubagentOutlivesTurnHarness));
    let handle = core.doc_host.open(CHAT).unwrap();

    queue_as_viewer(
        handle.doc(),
        "cmd-run-bg-subagent",
        SessionCommandPayload::Run {
            request: run_request("delegate this"),
            message_id: "m-user".into(),
        },
    );

    wait_for(
        || entries_text_now(&core).contains("delegated"),
        "the turn to complete",
    )
    .await;

    let subagent_status = entries_now(&core)
        .iter()
        .flat_map(|e| e.parts.iter())
        .find_map(|p| match p {
            MessagePart::Subagent { status, .. } => Some(*status),
            _ => None,
        })
        .expect("subagent card still present");
    assert_eq!(
        subagent_status,
        SubagentStatus::Running,
        "a cleanly completed turn must not claim to know a background child's fate"
    );
}

/// The same shape, but the turn is CUT SHORT. This is the case the sweep
/// exists for, and the half that narrowing it must not disable.
struct ErroredTurnWithRunningSubagentHarness;

#[async_trait]
impl Harness for ErroredTurnWithRunningSubagentHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }
    fn display_name(&self) -> &str {
        "ErroredTurnWithRunningSubagent"
    }
    fn capabilities(&self) -> HarnessCapabilities {
        HarnessCapabilities {
            reasoning_levels: vec![ReasoningLevel::Medium],
            runtime_modes: Vec::new(),
            ..HarnessCapabilities::default()
        }
    }
    async fn models(&self) -> Result<ModelCatalog, HarnessError> {
        Ok(ModelCatalog::built_in(vec![]))
    }
    async fn run(
        &self,
        _request: RunRequest,
        _controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
        tokio::spawn(async move {
            let _ = tx.send(AgentEvent::SubagentStarted {
                task_id: "t1".into(),
                tool_use_id: "tu1".into(),
                agent_type: "general-purpose".into(),
                description: "List current directory and count entries".into(),
                prompt: None,
            });
            let _ = tx.send(AgentEvent::TextDelta {
                text: "delegated".into(),
            });
            let _ = tx.send(AgentEvent::Done {
                status: DoneStatus::Errored,
                result: None,
                error: Some("the CLI died".into()),
                session_id: None,
            });
        });
        Ok(futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|event| (Ok(event), rx))
        })
        .boxed())
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cut_short_turn_still_cancels_a_running_subagent() {
    // The other side of the narrowing above. A turn that errored or was
    // interrupted really did end everything under it, and a card left
    // `Running` there would spin forever with nothing able to settle it.
    // Without this test, narrowing the sweep to non-`Completed` could be
    // narrowed all the way to nothing and the suite would stay green.
    let dir = tempfile::tempdir().unwrap();
    let core = assemble(dir.path(), Arc::new(ErroredTurnWithRunningSubagentHarness));
    let handle = core.doc_host.open(CHAT).unwrap();

    queue_as_viewer(
        handle.doc(),
        "cmd-run-errored-subagent",
        SessionCommandPayload::Run {
            request: run_request("delegate this"),
            message_id: "m-user".into(),
        },
    );

    wait_for(
        || {
            entries_now(&core).iter().any(|e| {
                e.parts.iter().any(|p| {
                    matches!(
                        p,
                        MessagePart::Subagent {
                            status: SubagentStatus::Cancelled,
                            ..
                        }
                    )
                })
            })
        },
        "the cut-short turn to cancel its running subagent",
    )
    .await;
}

/// Asks permission AFTER its run has ended. That is the state a run whose
/// handle was replaced reaches — its `ApprovalRequested` is dropped by the
/// authority guard (which reads whatever handle now owns the chat) and no
/// drain can reach the resolver — so the bridge itself has to fail closed.
struct LateAskingHarness {
    ask: Arc<tokio::sync::Notify>,
    reported: tokio::sync::mpsc::UnboundedSender<&'static str>,
}

#[async_trait]
impl Harness for LateAskingHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }
    fn display_name(&self) -> &str {
        "LateAsking"
    }
    fn capabilities(&self) -> HarnessCapabilities {
        HarnessCapabilities::default()
    }
    async fn models(&self) -> Result<ModelCatalog, HarnessError> {
        Ok(ModelCatalog::built_in(vec![]))
    }
    async fn run(
        &self,
        _request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let request_approval = controls.request_approval;
        let ask = self.ask.clone();
        let reported = self.reported.clone();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
        tokio::spawn(async move {
            let _ = tx.send(AgentEvent::Done {
                status: DoneStatus::Completed,
                result: None,
                error: None,
                session_id: None,
            });
            ask.notified().await;
            let word = match request_approval(ApprovalRequest::Command {
                command: "rm -rf /".into(),
                cwd: None,
            })
            .await
            {
                Ok(ApprovalDecision::Allow) | Ok(ApprovalDecision::AllowForSession) => "allowed",
                Ok(ApprovalDecision::Deny { .. }) => "denied",
                Ok(ApprovalDecision::Expired) | Err(_) => "not approved",
            };
            let _ = reported.send(word);
        });
        Ok(futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|event| (Ok(event), rx))
        })
        .boxed())
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_approval_the_engine_can_no_longer_show_is_refused_not_parked() {
    let dir = tempfile::tempdir().unwrap();
    let ask = Arc::new(tokio::sync::Notify::new());
    let (reported, mut reports) = tokio::sync::mpsc::unbounded_channel();
    let core = assemble(
        dir.path(),
        Arc::new(LateAskingHarness {
            ask: ask.clone(),
            reported,
        }),
    );
    let handle = core.doc_host.open(CHAT).unwrap();
    queue_as_viewer(
        handle.doc(),
        "cmd-run-late",
        SessionCommandPayload::Run {
            request: run_request("go"),
            message_id: "m-1".into(),
        },
    );
    wait_for(
        || core.sessions.session_status(CHAT).map(|s| s.status) == Some(SessionStatus::Idle),
        "the run to finish",
    )
    .await;

    // The run task is gone, so this request can never become a card. Parking
    // its resolver would leave the caller (for claude: a CLI blocked on a tool
    // call) waiting on an answer nobody is left to give.
    ask.notify_one();
    let answer = tokio::time::timeout(Duration::from_secs(5), reports.recv())
        .await
        .expect("a request nothing can show must still be answered, not parked forever");
    assert_eq!(answer, Some("not approved"));
}

/// Asks for the SAME action twice with both requests in flight — claude's
/// parallel tool calls. The mint-time pre-allow check ran before either was
/// answered, so a grant only helps the twin if it also sweeps what is already
/// parked.
struct BatchAskingHarness;

#[async_trait]
impl Harness for BatchAskingHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }
    fn display_name(&self) -> &str {
        "BatchAsking"
    }
    fn capabilities(&self) -> HarnessCapabilities {
        HarnessCapabilities::default()
    }
    async fn models(&self) -> Result<ModelCatalog, HarnessError> {
        Ok(ModelCatalog::built_in(vec![]))
    }
    async fn run(
        &self,
        _request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let request_approval = controls.request_approval;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
        tokio::spawn(async move {
            let ask = || {
                request_approval(ApprovalRequest::FileChange {
                    path: "src/reconcile.rs".into(),
                    operation: FileOperation::Modify,
                    added_lines: 24,
                    removed_lines: 6,
                })
            };
            // Both parked BEFORE either is awaited: the bridge parks as it is
            // called, so this is two open cards at once.
            let (first, second) = tokio::join!(ask(), ask());
            let word =
                |d: &Result<ApprovalDecision, tokio::sync::oneshot::error::RecvError>| match d {
                    Ok(ApprovalDecision::Allow) | Ok(ApprovalDecision::AllowForSession) => {
                        "allowed"
                    }
                    Ok(ApprovalDecision::Deny { .. }) => "denied",
                    _ => "unanswered",
                };
            let _ = tx.send(AgentEvent::TextDelta {
                text: format!("{} and {}", word(&first), word(&second)),
            });
            let _ = tx.send(AgentEvent::Done {
                status: DoneStatus::Completed,
                result: None,
                error: None,
                session_id: None,
            });
        });
        Ok(futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|event| (Ok(event), rx))
        })
        .boxed())
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_grant_also_answers_an_identical_request_already_waiting() {
    let dir = tempfile::tempdir().unwrap();
    let core = assemble(dir.path(), Arc::new(BatchAskingHarness));
    let handle = core.doc_host.open(CHAT).unwrap();
    let request_id = drive_to_open_approval(&core, &handle, "cmd-run-batch").await;
    let open_cards = || {
        entries_now(&core)
            .iter()
            .flat_map(|e| e.parts.clone())
            .filter(|p| matches!(p, MessagePart::Approval { decision: None, .. }))
            .count()
    };
    wait_for(|| open_cards() == 2, "both parallel requests to open").await;

    queue_as_viewer(
        handle.doc(),
        "cmd-grant-1",
        SessionCommandPayload::RespondApproval {
            request_id,
            decision: ApprovalDecision::AllowForSession,
        },
    );

    // The twin must not survive the grant: asking again one second after the
    // user said "don't ask again" is the whole defect.
    wait_for(
        || {
            entries_now(&core).iter().any(|e| {
                e.parts.iter().any(
                    |p| matches!(p, MessagePart::Text { text, .. } if text == "allowed and allowed"),
                )
            })
        },
        "the waiting twin to be answered by the grant",
    )
    .await;
    let decisions: Vec<_> = entries_now(&core)
        .iter()
        .flat_map(|e| e.parts.clone())
        .filter_map(|p| match p {
            MessagePart::Approval { decision, .. } => Some(decision),
            _ => None,
        })
        .collect();
    assert_eq!(
        decisions,
        vec![
            Some(ApprovalDecision::AllowForSession),
            Some(ApprovalDecision::AllowForSession),
        ],
        "both cards stamped with the grant that answered them"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_approval_round_trips_from_request_to_decision() {
    let dir = tempfile::tempdir().unwrap();
    let core = assemble(dir.path(), Arc::new(ApprovingHarness));
    let handle = core.doc_host.open(CHAT).unwrap();
    let request_id = drive_to_open_approval(&core, &handle, "cmd-run-approve").await;

    queue_as_viewer(
        handle.doc(),
        "cmd-approve-1",
        SessionCommandPayload::RespondApproval {
            request_id,
            decision: ApprovalDecision::Allow,
        },
    );

    // Only reachable if the decision crossed the oneshot into the harness: a
    // bridge that stamped the doc and never answered would hang here.
    wait_for(
        || {
            entries_now(&core).iter().any(|e| {
                e.parts.iter().any(
                    |p| matches!(p, MessagePart::Text { text, .. } if text == "applied the edit"),
                )
            })
        },
        "approved turn to report the decision",
    )
    .await;
    assert_eq!(
        command_status(&core, "cmd-approve-1"),
        Some((SessionCommandStatus::Applied, None))
    );
    assert!(entries_now(&core).iter().any(|e| {
        e.parts.iter().any(|p| {
            matches!(
                p,
                MessagePart::Approval {
                    decision: Some(ApprovalDecision::Allow),
                    ..
                }
            )
        })
    }));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_denied_approval_reaches_the_harness_with_its_message() {
    // The arm a fixture that silently approves would never exercise.
    let dir = tempfile::tempdir().unwrap();
    let core = assemble(dir.path(), Arc::new(ApprovingHarness));
    let handle = core.doc_host.open(CHAT).unwrap();
    let request_id = drive_to_open_approval(&core, &handle, "cmd-run-deny").await;

    queue_as_viewer(
        handle.doc(),
        "cmd-deny-1",
        SessionCommandPayload::RespondApproval {
            request_id,
            decision: ApprovalDecision::Deny {
                message: "not that file".into(),
            },
        },
    );

    wait_for(
        || {
            entries_now(&core).iter().any(|e| {
                e.parts.iter().any(|p| {
                    matches!(p, MessagePart::Text { text, .. } if text == "left the file untouched")
                })
            })
        },
        "denied turn to report the decision",
    )
    .await;
    assert!(entries_now(&core).iter().any(|e| {
        e.parts.iter().any(|p| {
            matches!(
                p,
                MessagePart::Approval {
                    decision: Some(ApprovalDecision::Deny { message }),
                    ..
                } if message == "not that file"
            )
        })
    }));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_response_to_an_unknown_approval_is_rejected_with_a_reason() {
    let dir = tempfile::tempdir().unwrap();
    let core = assemble(dir.path(), Arc::new(ApprovingHarness));
    let handle = core.doc_host.open(CHAT).unwrap();
    let _request_id = drive_to_open_approval(&core, &handle, "cmd-run-bogus").await;

    queue_as_viewer(
        handle.doc(),
        "cmd-approve-bogus",
        SessionCommandPayload::RespondApproval {
            request_id: "not-a-real-id".into(),
            decision: ApprovalDecision::Allow,
        },
    );

    wait_for(
        || {
            matches!(
                command_status(&core, "cmd-approve-bogus"),
                Some((SessionCommandStatus::Rejected, Some(_)))
            )
        },
        "bogus approval to be rejected",
    )
    .await;
    let (_, resolution) = command_status(&core, "cmd-approve-bogus").unwrap();
    assert_eq!(
        resolution.unwrap(),
        "This approval is no longer waiting for an answer."
    );
    // The real approval is untouched and still answerable.
    assert!(entries_now(&core).iter().any(|e| {
        e.parts
            .iter()
            .any(|p| matches!(p, MessagePart::Approval { decision: None, .. }))
    }));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_supplied_expired_approval_is_rejected() {
    // `Expired` is host-stamped, never client-sent: accepting it off the wire
    // would let any paired device mark a live approval dead.
    let dir = tempfile::tempdir().unwrap();
    let core = assemble(dir.path(), Arc::new(ApprovingHarness));
    let handle = core.doc_host.open(CHAT).unwrap();
    let request_id = drive_to_open_approval(&core, &handle, "cmd-run-expired").await;

    queue_as_viewer(
        handle.doc(),
        "cmd-expire-1",
        SessionCommandPayload::RespondApproval {
            request_id: request_id.clone(),
            decision: ApprovalDecision::Expired,
        },
    );
    wait_for(
        || {
            matches!(
                command_status(&core, "cmd-expire-1"),
                Some((SessionCommandStatus::Rejected, Some(_)))
            )
        },
        "client-sent Expired to be rejected",
    )
    .await;
    let (_, resolution) = command_status(&core, "cmd-expire-1").unwrap();
    assert_eq!(
        resolution.unwrap(),
        "Expired isn't a decision that can be sent."
    );

    // A wrong decision must never brick the approval: the real one still lands.
    queue_as_viewer(
        handle.doc(),
        "cmd-expire-2",
        SessionCommandPayload::RespondApproval {
            request_id,
            decision: ApprovalDecision::Allow,
        },
    );
    wait_for(
        || {
            entries_now(&core).iter().any(|e| {
                e.parts.iter().any(
                    |p| matches!(p, MessagePart::Text { text, .. } if text == "applied the edit"),
                )
            })
        },
        "the correct decision to still land",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_approval_pending_when_the_run_is_interrupted_becomes_expired() {
    // The common way a run ends with an approval open: the user stops it. The
    // part must reach a terminal state in this process, with no restart — the
    // recovery sweep only ever sees entries a crash left `streaming`.
    let dir = tempfile::tempdir().unwrap();
    let core = assemble(dir.path(), Arc::new(ApprovingHarness));
    let handle = core.doc_host.open(CHAT).unwrap();
    let request_id = drive_to_open_approval(&core, &handle, "cmd-run-interrupt").await;

    queue_as_viewer(
        handle.doc(),
        "cmd-interrupt-1",
        SessionCommandPayload::Interrupt {},
    );

    wait_for(
        || {
            entries_now(&core).iter().any(|e| {
                e.parts.iter().any(|p| {
                    matches!(
                        p,
                        MessagePart::Approval {
                            request_id: rid,
                            decision: Some(ApprovalDecision::Expired),
                            ..
                        } if *rid == request_id
                    )
                })
            })
        },
        "interrupted approval to be stamped expired",
    )
    .await;
}
#[tokio::test(flavor = "multi_thread")]
async fn an_approval_pending_when_the_process_dies_becomes_expired() {
    // The path an interrupt CANNOT reach: the process is killed while a run is
    // blocked on an approval, so the run loop never runs its terminal stamp.
    // A graceful shutdown+restart would prove nothing here — `shutdown()`
    // interrupts every live run, so the FIRST process would stamp Expired
    // through the Done arm and recovery would find nothing stale. This
    // manufactures the on-disk state a kill leaves behind instead.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data");
    std::fs::create_dir_all(&dir).unwrap();
    // Pin the device id so the manufactured streaming entry counts as OURS —
    // `mark_abandoned_streams` only sweeps entries this device wrote.
    std::fs::write(dir.join("device-id"), "dev-crash").unwrap();

    {
        let store = DocsStore::open(dir.join("local-store")).unwrap();
        let doc = SessionDoc::init(CHAT).unwrap();
        doc.push_message(&SessionMessageEntry {
            id: "msg-user-1".into(),
            role: MessageRole::User,
            parts: vec![MessagePart::Text {
                id: "t0".into(),
                text: "edit it".into(),
            }],
            created_at: 1,
            device_id: "dev-crash".into(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
        })
        .unwrap();
        doc.push_message(&SessionMessageEntry {
            id: "msg-assistant-1".into(),
            role: MessageRole::Assistant,
            parts: vec![MessagePart::Approval {
                id: "ap-r-crash".into(),
                request_id: "r-crash".into(),
                approval: ApprovalRequest::FileChange {
                    path: "src/reconcile.rs".into(),
                    operation: FileOperation::Modify,
                    added_lines: 24,
                    removed_lines: 6,
                },
                decision: None,
            }],
            created_at: 2,
            device_id: "dev-crash".into(),
            status: Some(MessageStatus::Streaming),
            continuation_of: None,
        })
        .unwrap();
        store
            .save_snapshot(CHAT, &doc.export_snapshot().unwrap())
            .unwrap();

        // A journal whose last event is not `Done`: the run died mid-stream.
        let journal = RunJournal::open(dir.join("local-store/journals")).unwrap();
        journal
            .append(
                CHAT,
                &AgentEvent::SessionStarted {
                    harness: HarnessId::Mock,
                    model: "mock-1".into(),
                    tools: vec![],
                    cwd: "/tmp".into(),
                    session_id: "hs-crash".into(),
                    assistant_message_id: "msg-assistant-1".into(),
                    runtime_mode: RuntimeMode::default(),
                },
            )
            .unwrap();
    }

    let core = assemble(&dir, Arc::new(ApprovingHarness));
    assert_eq!(core.device_id, "dev-crash");

    // Asserted BY REQUEST ID, never by "no open approval exists": boot recovery
    // may revive the crashed turn, and a revived run legitimately opens a NEW
    // approval beside this one.
    wait_for(
        || {
            entries_now(&core).iter().any(|e| {
                e.parts.iter().any(|p| {
                    matches!(
                        p,
                        MessagePart::Approval {
                            request_id: rid,
                            decision: Some(ApprovalDecision::Expired),
                            ..
                        } if rid == "r-crash"
                    )
                })
            })
        },
        "orphaned approval to be stamped expired",
    )
    .await;

    // And a decision arriving afterwards is refused rather than resurrecting it.
    let handle = core.doc_host.open(CHAT).unwrap();
    queue_as_viewer(
        handle.doc(),
        "cmd-late-approve",
        SessionCommandPayload::RespondApproval {
            request_id: "r-crash".into(),
            decision: ApprovalDecision::Allow,
        },
    );
    wait_for(
        || {
            matches!(
                command_status(&core, "cmd-late-approve"),
                Some((SessionCommandStatus::Rejected, Some(_)))
            )
        },
        "late approval response to be refused",
    )
    .await;
    let (_, resolution) = command_status(&core, "cmd-late-approve").unwrap();
    assert_eq!(
        resolution.unwrap(),
        "This approval is no longer waiting for an answer."
    );
}

// ---------------------------------------------------------------------------
// Presence: how many supervisors are attached, across topologies. The
// unattended sweeper (a later slice) is the only reader that turns this into
// a policy decision; these tests only cover the counting.
// ---------------------------------------------------------------------------

/// The embedded-UI topology: `memory_client` is a real connection, so the
/// engine must see the in-process UI as attached. Counting sockets instead
/// would report zero watchers with the UI open on screen and expire runs in
/// front of a present user.
#[tokio::test]
async fn an_in_memory_client_counts_as_an_attached_supervisor() {
    let dir = tempfile::tempdir().unwrap();
    let core = assemble(
        dir.path(),
        Arc::new(MockHarness::with_script(mock_script())),
    );
    assert_eq!(core.presence().attached_count(), 0);
    assert!(core.presence().unattended_since().is_some());

    let client = comet_rpc::memory_client(core.rpc_service());
    wait_for(
        || core.presence().attached_count() == 1,
        "the in-memory client to be counted",
    )
    .await;
    assert_eq!(core.presence().unattended_since(), None);

    drop(client);
    wait_for(
        || core.presence().unattended_since().is_some(),
        "the stretch to restart when the client goes",
    )
    .await;
}

/// `RemoteRpcService` wraps `EngineRpc`; a defaulted `attached()` compiles and
/// silently loses every LAN client. This is the topology `remote_access.rs`
/// exercises for RPC behavior — here it stands in as the "LAN-shaped" client
/// that must still register as an attached supervisor.
#[tokio::test]
async fn the_remote_service_forwards_presence_to_the_engine() {
    let dir = tempfile::tempdir().unwrap();
    let core = assemble(
        dir.path(),
        Arc::new(MockHarness::with_script(mock_script())),
    );
    let remote: Arc<dyn RpcService> = Arc::new(comet_engine::RemoteRpcService::new(
        core.remote_rpc_service(),
        core.device_id.clone(),
    ));
    let client = comet_rpc::memory_client(remote);
    wait_for(
        || core.presence().attached_count() == 1,
        "a LAN-shaped client to be counted",
    )
    .await;
    drop(client);
}

// ---------------------------------------------------------------------------
// The unattended sweeper: a wait no connected client can answer gets ended
// after `bound`, and the transcript says why rather than inventing a
// decision the user never made. `expire_unattended` is called directly with
// an explicit `now` so these tests don't wait out a real bound.
// ---------------------------------------------------------------------------

/// The rule, unattended half: the turn ends, the card reads Expired, the note
/// says why, and NOTHING writes a decision. Auto-denying would tell the model
/// the user refused something the user was never asked.
#[tokio::test]
async fn an_unanswerable_approval_expires_the_turn_without_inventing_a_decision() {
    let dir = tempfile::tempdir().unwrap();
    // No client ever attaches: `assemble` starts the engine unattended from boot.
    let core = assemble(dir.path(), Arc::new(ApprovingHarness));
    let presence = core.presence();
    let handle = core.doc_host.open(CHAT).unwrap();
    drive_to_open_approval(&core, &handle, "cmd-run-unattended-approval").await;

    let ended = core
        .sessions
        .expire_unattended(
            &presence,
            chrono::Utc::now() + chrono::TimeDelta::seconds(1),
            Duration::from_millis(100),
        )
        .await;
    assert_eq!(ended, 1, "the blocked run should have been ended");

    wait_for(
        || {
            entries(&core).iter().any(|e| {
                e.parts.iter().any(|p| {
                    matches!(
                        p,
                        MessagePart::Approval {
                            decision: Some(ApprovalDecision::Expired),
                            ..
                        }
                    )
                })
            })
        },
        "the card to be stamped expired",
    )
    .await;

    let text = entries_text(&core);
    assert!(
        text.contains("nothing was connected to ask"),
        "the transcript must say why the turn ended: {text}"
    );
    assert!(
        !entries(&core)
            .iter()
            .any(|e| e.parts.iter().any(|p| matches!(
                p,
                MessagePart::Approval {
                    decision: Some(ApprovalDecision::Deny { .. }),
                    ..
                }
            ))),
        "an expiry must never be recorded as a denial"
    );
}

/// The rule, attended half — and the test most likely to be broken by a later
/// refactor. A user deliberating over a card did nothing wrong.
#[tokio::test]
async fn an_answerable_approval_never_expires_however_long_it_waits() {
    let dir = tempfile::tempdir().unwrap();
    let core = assemble(dir.path(), Arc::new(ApprovingHarness));
    let presence = core.presence();
    let _client = comet_rpc::memory_client(core.rpc_service());
    wait_for(
        || presence.attached_count() == 1,
        "a supervisor to be attached",
    )
    .await;

    let handle = core.doc_host.open(CHAT).unwrap();
    drive_to_open_approval(&core, &handle, "cmd-run-attended-approval").await;

    let ended = core
        .sessions
        .expire_unattended(
            &presence,
            chrono::Utc::now() + chrono::TimeDelta::days(30),
            Duration::from_millis(1),
        )
        .await;
    assert_eq!(ended, 0, "an answerable wait is never bounded");
    assert_eq!(
        core.sessions.session_status(CHAT).map(|s| s.status),
        Some(SessionStatus::AwaitingInput),
        "and it is still there to answer"
    );
}

/// Reconnecting clears the stretch; a later disconnect starts a fresh one.
#[tokio::test]
async fn reconnecting_resets_the_window() {
    let dir = tempfile::tempdir().unwrap();
    let core = assemble(dir.path(), Arc::new(ApprovingHarness));
    let presence = core.presence();
    let handle = core.doc_host.open(CHAT).unwrap();
    drive_to_open_approval(&core, &handle, "cmd-run-reconnect").await;

    let client = comet_rpc::memory_client(core.rpc_service());
    wait_for(|| presence.attached_count() == 1, "attached").await;
    assert_eq!(
        core.sessions
            .expire_unattended(
                &presence,
                chrono::Utc::now() + chrono::TimeDelta::hours(2),
                Duration::from_secs(60)
            )
            .await,
        0,
        "attended, so not due"
    );

    drop(client);
    wait_for(
        || presence.unattended_since().is_some(),
        "the stretch to restart",
    )
    .await;
    assert_eq!(
        core.sessions
            .expire_unattended(
                &presence,
                chrono::Utc::now() + chrono::TimeDelta::hours(2),
                Duration::from_secs(60)
            )
            .await,
        1,
        "a fresh window elapsed"
    );
}

/// The same rule at the other call site: a parked question wedges a run
/// identically, so it expires identically.
#[tokio::test]
async fn an_unanswerable_input_question_expires_the_turn_too() {
    // Same shape as `respond_input_resolves_pending_question`'s local harness:
    // asks one question and echoes what it was told. Parked here means never
    // answered, so the sweep's `interrupt` (which unparks with empty answers)
    // is what lets it finish at all.
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
        async fn models(&self) -> Result<ModelCatalog, HarnessError> {
            Ok(ModelCatalog::built_in(vec![]))
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
    let presence = core.presence();
    let handle = core.doc_host.open(CHAT).unwrap();
    queue_as_viewer(
        handle.doc(),
        "cmd-run-unattended-question",
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

    let ended = core
        .sessions
        .expire_unattended(
            &presence,
            chrono::Utc::now() + chrono::TimeDelta::seconds(1),
            Duration::from_millis(100),
        )
        .await;
    assert_eq!(ended, 1);

    let text = entries_text(&core);
    assert!(
        text.contains("needed your answer"),
        "a question expiry must name the question, not an approval: {text}"
    );
}

/// A run with nothing parked is not blocked and must never be swept: a tool
/// call in flight is `blocked_since() == None`, so `due_for_expiry` never runs
/// for it regardless of how long it takes.
#[tokio::test]
async fn a_run_that_is_merely_slow_is_not_expired() {
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
                    runtime_mode: comet_proto::RuntimeMode::default(),
                },
                AgentEvent::ToolCall {
                    id: "tool-slow".into(),
                    call: ToolCall::Exec {
                        command: "long-running-command".into(),
                    },
                },
            ],
            step_delay: Duration::from_millis(5),
            hang_until_interrupt: true,
        }),
    );
    let presence = core.presence();
    let handle = core.doc_host.open(CHAT).unwrap();
    queue_as_viewer(
        handle.doc(),
        "cmd-run-slow",
        SessionCommandPayload::Run {
            request: run_request("do something slow"),
            message_id: "m-1".into(),
        },
    );
    wait_for(
        || core.sessions.session_status(CHAT).map(|s| s.status) == Some(SessionStatus::Working),
        "the slow run to start",
    )
    .await;

    let ended = core
        .sessions
        .expire_unattended(
            &presence,
            chrono::Utc::now() + chrono::TimeDelta::days(1),
            Duration::from_millis(1),
        )
        .await;
    assert_eq!(ended, 0, "a working run is not a run waiting on a human");
}

/// Two chats parked past the same deadline must BOTH end in one pass.
///
/// The fail-closed re-check is per run, and the mistake it must not make is
/// abandoning the rest of the list once one run is skipped or settled — a
/// `break` where a `continue` belongs would leave every chat but the first
/// parked forever, and every other test here parks exactly one.
#[tokio::test]
async fn one_sweep_ends_every_parked_chat_not_just_the_first() {
    let dir = tempfile::tempdir().unwrap();
    let core = assemble(dir.path(), Arc::new(ApprovingHarness));
    let presence = core.presence();

    let chats = ["chat-sweep-a", "chat-sweep-b"];
    let handles: Vec<_> = chats
        .iter()
        .map(|chat| core.doc_host.open(chat).unwrap())
        .collect();
    for (chat, handle) in chats.iter().zip(&handles) {
        queue_as_viewer(
            handle.doc(),
            &format!("cmd-{chat}"),
            SessionCommandPayload::Run {
                request: run_request("edit it"),
                message_id: "m-1".into(),
            },
        );
    }
    for chat in chats {
        wait_for(
            || {
                core.sessions.session_status(chat).map(|s| s.status)
                    == Some(SessionStatus::AwaitingInput)
            },
            "both chats to park on an approval",
        )
        .await;
    }

    let ended = core
        .sessions
        .expire_unattended(
            &presence,
            chrono::Utc::now() + chrono::TimeDelta::seconds(1),
            Duration::from_millis(100),
        )
        .await;
    assert_eq!(ended, 2, "every parked chat is judged on its own");
}

/// Every other test above drives `expire_unattended` by hand — proof of the
/// expiry logic, but not of the ticker `Engine::assemble_runtime` actually
/// spawns. This calls `spawn_unattended_sweeper` itself, on a real interval
/// and a real `Utc::now()`, and waits for the card to expire on its own with
/// nothing manually pumping the sweep. `assemble_runtime` can't be driven
/// directly here — it hard-codes `default_registry()`, and the mock harness's
/// parking knob is the process-global `COMET_MOCK_APPROVAL` env var, the same
/// parallel-test race `ScriptedHarness` was chosen over `COMET_MOCK_HANG` to
/// avoid elsewhere in this file — so this builds a core the same way every
/// other test here does and spawns the real function against it.
#[tokio::test]
async fn the_spawned_sweeper_expires_a_parked_approval_on_its_own() {
    let dir = tempfile::tempdir().unwrap();
    let core = assemble(dir.path(), Arc::new(ApprovingHarness));
    // No client ever attaches: unattended from boot, same as the manual-sweep
    // tests above.
    let presence = core.presence();
    let handle = core.doc_host.open(CHAT).unwrap();

    // `sweep_interval` clamps the tick to 250ms even for a much shorter
    // bound, so the wait below just needs to outlast a couple of ticks —
    // the bound itself can stay near-instant.
    // Bound rather than dropped: dropping a `JoinHandle` detaches the task, and
    // this test wants it running for the wait below. `EngineRuntime` keeps the
    // real one so shutdown can abort it.
    let _sweeper = comet_engine::spawn_unattended_sweeper(
        core.sessions.clone(),
        presence,
        Duration::from_millis(100),
    );

    drive_to_open_approval(&core, &handle, "cmd-run-real-sweeper").await;

    wait_for(
        || {
            entries(&core).iter().any(|e| {
                e.parts.iter().any(|p| {
                    matches!(
                        p,
                        MessagePart::Approval {
                            decision: Some(ApprovalDecision::Expired),
                            ..
                        }
                    )
                })
            })
        },
        "the real sweeper to expire the card with nothing pumping it manually",
    )
    .await;
}

// ---------------------------------------------------------------------------
// Retry clears the discovery cell (slice 2.1, task 6)
// ---------------------------------------------------------------------------

/// Fixture harness whose `models()` runs a discovery closure through a real
/// `DiscoveryCache`, counting every time the closure actually executes. The
/// production shape (2.2/2.3) wires a `DiscoveryCache` into `ClaudeHarness`/
/// `CodexHarness`; this fixture exercises the same cache without a real CLI.
struct CountingDiscoveryHarness {
    cache: comet_harness::discovery::DiscoveryCache,
    runs: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait]
impl Harness for CountingDiscoveryHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }
    fn display_name(&self) -> &str {
        "Counting"
    }
    fn capabilities(&self) -> HarnessCapabilities {
        HarnessCapabilities::default()
    }
    async fn models(&self) -> Result<ModelCatalog, HarnessError> {
        let runs = self.runs.clone();
        let discovery = self
            .cache
            .get(|| async move {
                runs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(comet_harness::discovery::Discovery { models: vec![] })
            })
            .await;
        Ok(self.cache.catalog(vec![], discovery))
    }
    async fn run(
        &self,
        _request: RunRequest,
        _controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        unimplemented!("not exercised by the discovery-retry test")
    }
    fn clear_discovery(&self) {
        self.cache.clear();
    }
}

impl CountingDiscoveryHarness {
    fn discovery_runs(&self) -> usize {
        self.runs.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// The RPC-surface handle the discovery-retry test drives `list_models`
/// through — `EngineCore` has no typed `list_models` method of its own (out
/// of this task's scope), so this wraps the same in-memory `RpcClient` the
/// `rpc_surface_over_in_memory_transport` test above uses, adding the
/// `force` field `ListModelsParams` gains in this task.
///
/// Holds the `TempDir` and `EngineCore` alongside the client so nothing the
/// registered `RpcService` depends on is dropped out from under a still-live
/// background connection task.
struct TestEngine {
    _dir: tempfile::TempDir,
    _core: EngineCore,
    client: comet_rpc::RpcClient,
}

impl TestEngine {
    async fn list_models(
        &self,
        harness: HarnessId,
        force: bool,
    ) -> Result<ModelCatalog, comet_rpc::RpcError> {
        self.client
            .call_as(
                comet_rpc::methods::LIST_MODELS,
                serde_json::json!({ "harness": harness, "force": force }),
            )
            .await
    }

    /// The registry `LIST_MODELS` records diagnostics into — the discovery
    /// tests read it directly rather than round-tripping through
    /// `ListHarnessDiagnostics`, since the point is what the engine recorded,
    /// not the RPC surface that later reports it.
    fn registry(&self) -> &HarnessRegistry {
        &self._core.registry
    }
}

async fn engine_with_counting_discovery() -> (TestEngine, Arc<CountingDiscoveryHarness>) {
    let dir = tempfile::tempdir().unwrap();
    let harness = Arc::new(CountingDiscoveryHarness {
        cache: Default::default(),
        runs: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    });
    let core = assemble(dir.path(), harness.clone());
    let client = comet_rpc::memory_client(core.rpc_service());
    (
        TestEngine {
            _dir: dir,
            _core: core,
            client,
        },
        harness,
    )
}

/// A cached failure must survive an ordinary reopen and must NOT survive a
/// Retry. Both halves matter: the first is what stops a broken login
/// spawning a subprocess per picker open, the second is what stops Retry
/// being a button that does nothing.
#[tokio::test]
async fn retry_re_arms_a_cached_discovery_failure() {
    let (engine, harness) = engine_with_counting_discovery().await;

    engine.list_models(HarnessId::Mock, false).await.unwrap();
    engine.list_models(HarnessId::Mock, false).await.unwrap();
    assert_eq!(harness.discovery_runs(), 1, "cached between ordinary calls");

    engine.list_models(HarnessId::Mock, true).await.unwrap();
    assert_eq!(harness.discovery_runs(), 2, "force re-arms the cell");
}

/// Assembles an engine around a given `MockHarness` — the shape both the
/// discovered-model test (task 7) and the drift-diagnostic tests (task 8)
/// need, differing only in how the mock is scripted.
async fn engine_with_mock(harness: MockHarness) -> TestEngine {
    let dir = tempfile::tempdir().unwrap();
    let core = assemble(dir.path(), Arc::new(harness));
    let client = comet_rpc::memory_client(core.rpc_service());
    TestEngine {
        _dir: dir,
        _core: core,
        client,
    }
}

/// Assembles an engine around a `MockHarness` scripted with the given
/// discovery answer — task 7's end-to-end proof that a discovered model
/// reaches the client with no CLI on the machine.
async fn engine_with_mock_discovery(discovery: Discovery) -> TestEngine {
    engine_with_mock(MockHarness::with_discovery(discovery)).await
}

/// The end-to-end shape 2.2 and 2.3 will inherit: a discovered model the
/// curated list has never heard of reaches the client, the curated models
/// survive beside it, and the reply says the list is live.
#[tokio::test]
async fn a_discovered_model_reaches_the_client_and_the_list_reads_live() {
    let engine = engine_with_mock_discovery(Discovery {
        models: vec![DiscoveredModel {
            id: "mock-tomorrow".into(),
            label: "Tomorrow".into(),
            description: None,
            reasoning_levels: vec![ReasoningLevel::High],
            accepts_images: None,
        }],
    })
    .await;

    let catalog = engine.list_models(HarnessId::Mock, false).await.unwrap();
    assert_eq!(catalog.source, CatalogSource::Live);
    let ids: Vec<&str> = catalog.models.iter().map(|m| m.id.as_str()).collect();
    assert!(ids.contains(&"mock-1"), "curated models survive: {ids:?}");
    assert!(
        ids.contains(&"mock-tomorrow"),
        "live model appears: {ids:?}"
    );
    assert!(
        catalog
            .models
            .iter()
            .find(|m| m.id == "mock-tomorrow")
            .unwrap()
            .accepts_images,
        "absent modality means images work"
    );
}

/// A provider that answered nonsense has changed its protocol under us.
/// That is the whole reason 0b.2's channel exists.
#[tokio::test]
async fn an_unreadable_discovery_answer_is_reported_as_drift() {
    let engine = engine_with_mock(MockHarness::with_failing_discovery(
        DiscoveryFailure::Unparseable,
    ))
    .await;

    let catalog = engine.list_models(HarnessId::Mock, false).await.unwrap();
    assert_eq!(catalog.source, CatalogSource::BuiltIn, "list still works");

    let diagnostics = engine.registry().diagnostics();
    let bucket = diagnostics
        .iter()
        .find(|d| d.harness == HarnessId::Mock)
        .expect("a bucket");
    assert!(
        bucket
            .entries
            .iter()
            .any(|e| e.discriminator.contains("discovery")),
        "expected a discovery drift entry, got {:?}",
        bucket.entries
    );
}

/// One unreadable answer is ONE incident, however many times the picker is
/// opened afterwards. The failure is cached for the whole boot, so a
/// re-reporting read would climb the count and refresh `last_seen_ms` on every
/// `ListModels` — rendering a provider that failed once as one failing
/// continuously.
#[tokio::test]
async fn a_cached_drift_failure_is_reported_once_not_once_per_request() {
    let engine = engine_with_mock(MockHarness::with_failing_discovery(
        DiscoveryFailure::Unparseable,
    ))
    .await;

    for _ in 0..5 {
        engine.list_models(HarnessId::Mock, false).await.unwrap();
    }

    let diagnostics = engine.registry().diagnostics();
    let bucket = diagnostics
        .iter()
        .find(|d| d.harness == HarnessId::Mock)
        .expect("a bucket");
    let entry = bucket
        .entries
        .iter()
        .find(|e| e.discriminator.contains("discovery"))
        .expect("a discovery drift entry");
    assert_eq!(
        entry.count, 1,
        "five requests, one cached failure, one incident"
    );
}

/// Retry has to reach the harness, not just the UI's slot. The mock caches a
/// scripted failure like a real adapter would, so a `force` that did not clear
/// it would hand back the same failure and the Retry path could never be
/// exercised end to end.
///
/// This counts incidents as its proxy for "discovery ran again", which only
/// discriminates while a failure is reported once per attempt. Break that and
/// this test passes for the wrong reason — two reads of one cached failure
/// also make two. Verified by falsification: with report-once intact and the
/// mock's `clear_discovery` removed it fails 1 vs 2; with both broken it
/// passes. If reporting ever stops being once-per-attempt, rewrite this to
/// count discovery runs directly.
#[tokio::test]
async fn a_forced_request_re_runs_mock_discovery() {
    let engine = engine_with_mock(MockHarness::with_failing_discovery(
        DiscoveryFailure::Unparseable,
    ))
    .await;

    engine.list_models(HarnessId::Mock, false).await.unwrap();
    engine.list_models(HarnessId::Mock, true).await.unwrap();

    let diagnostics = engine.registry().diagnostics();
    let entry = diagnostics
        .iter()
        .find(|d| d.harness == HarnessId::Mock)
        .and_then(|b| {
            b.entries
                .iter()
                .find(|e| e.discriminator.contains("discovery"))
        })
        .expect("a discovery drift entry");
    assert_eq!(
        entry.count, 2,
        "the forced call re-ran discovery, so its failure is a second incident"
    );
}

/// The ordinary case must stay silent. A machine with no CLI installed
/// would otherwise report protocol drift on every single boot, which is how
/// a diagnostics surface becomes noise nobody reads.
#[tokio::test]
async fn an_unreachable_provider_raises_no_diagnostic() {
    let engine = engine_with_mock(MockHarness::with_failing_discovery(
        DiscoveryFailure::Unreachable,
    ))
    .await;

    engine.list_models(HarnessId::Mock, false).await.unwrap();

    assert!(
        engine.registry().diagnostics().is_empty(),
        "an absent CLI is not a protocol change"
    );
}
