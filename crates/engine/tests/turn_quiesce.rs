//! Turn-quiesce watchdog: a harness that never sends `Done` must not strand
//! the session Working forever.
//!
//! The failure shape (diagnosed upstream on 2026-08-12, and harness-agnostic):
//! the agent finishes and streams its answer, but the adapter never settles
//! the prompt, so no turn-end reaches the engine. Nothing else catches it —
//! the liveness heartbeat keeps the row fresh *by design*, and the 30-minute
//! idle reaper is gated on `idle_since.is_some()`, which a missing `Done`
//! never sets. The badge spins until someone hits Stop.
//!
//! The watchdog parks such a turn exactly as a `Done` would, and never ends
//! the run — so a false trip costs a status dip, not content.

use std::sync::Arc;
use std::sync::Once;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;

use comet_doc::{
    MessageRole, MessageStatus, SessionCommandEntry, SessionCommandPayload, SessionCommandStatus,
    SessionDoc, SessionMessageEntry,
};
use comet_engine::{EngineCore, HarnessRegistry};
use comet_harness::{Harness, HarnessError, RunControls};
use comet_proto::{
    AgentEvent, HarnessCapabilities, HarnessId, ModelCatalog, ReasoningLevel, RunRequest,
    RuntimeMode, SandboxLevel, SessionStatus, SteeringMode,
};

const CHAT: &str = "chat-quiesce";
const VIEWER: &str = "viewer-device";

/// Watchdog window for every test in this file. Set once, before any engine
/// exists to read it — the knob is process-global.
const QUIESCE_MS: u64 = 300;

fn init_quiesce_env() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // SAFETY: called before any engine (and so any reader of the var) is
        // assembled in this test process; every test here shares the value.
        unsafe { std::env::set_var("COMET_TURN_QUIESCE_MS", QUIESCE_MS.to_string()) };
    });
}

/// Streams its script, then holds the stream OPEN and silent forever — the
/// lost-`Done` shape. Deliberately not `MockHarness`: that one ends its
/// stream after the script, and a closed stream is a different bug (the
/// engine turns it into an errored `Done`, which already had a path).
struct SilentAfterScript {
    script: Vec<AgentEvent>,
}

#[async_trait]
impl Harness for SilentAfterScript {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }
    fn display_name(&self) -> &str {
        "SilentAfterScript"
    }
    fn capabilities(&self) -> HarnessCapabilities {
        HarnessCapabilities {
            // Steerable: the watchdog only arms on a persistent session, which
            // is the only kind that parks instead of ending.
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
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<AgentEvent, HarnessError>>(16);
        let script = self.script.clone();
        tokio::spawn(async move {
            for event in script {
                if tx.send(Ok(event)).await.is_err() {
                    return;
                }
            }
            // Hold the sender forever: the stream stays open, and no further
            // event — least of all a `Done` — ever arrives.
            std::future::pending::<()>().await;
            drop(tx);
        });
        Ok(futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|event| (event, rx))
        })
        .boxed())
    }
}

fn answered_but_never_settled() -> Vec<AgentEvent> {
    vec![
        AgentEvent::SessionStarted {
            harness: HarnessId::Mock,
            model: "mock-1".into(),
            tools: vec![],
            cwd: "/tmp".into(),
            session_id: "hs-1".into(),
            assistant_message_id: "a-1".into(),
            runtime_mode: RuntimeMode::default(),
        },
        AgentEvent::TextDelta {
            text: "Build finished successfully".into(),
        },
    ]
}

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

fn registry_with(harness: Arc<dyn Harness>) -> Arc<HarnessRegistry> {
    let registry = HarnessRegistry::new();
    registry.register(harness);
    Arc::new(registry)
}

fn assemble(dir: &std::path::Path, harness: Arc<dyn Harness>) -> EngineCore {
    EngineCore::assemble(dir, registry_with(harness), HarnessId::Mock, None)
        .expect("engine core assembles")
}

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

fn entries(core: &EngineCore) -> Vec<SessionMessageEntry> {
    core.doc_host
        .open(CHAT)
        .expect("open chat")
        .doc()
        .read_entries()
        .expect("read entries")
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_turn_whose_done_never_arrives_parks_instead_of_spinning_forever() {
    init_quiesce_env();
    let dir = tempfile::tempdir().unwrap();
    let core = assemble(
        dir.path(),
        Arc::new(SilentAfterScript {
            script: answered_but_never_settled(),
        }),
    );
    let handle = core.doc_host.open(CHAT).unwrap();
    queue_as_viewer(
        handle.doc(),
        "cmd-quiesce-1",
        SessionCommandPayload::Run {
            request: run_request("build it"),
            message_id: "m-1".into(),
        },
    );

    // The answer streams and the session goes Working, as it would normally.
    wait_for(
        || {
            core.sessions
                .watch_sessions()
                .borrow()
                .first()
                .is_some_and(|s| s.status == SessionStatus::Working)
        },
        "session to start Working",
    )
    .await;

    // No `Done` is coming. Before the watchdog this spun forever; now the
    // silence past the window parks the turn.
    wait_for(
        || {
            core.sessions
                .watch_sessions()
                .borrow()
                .first()
                .is_some_and(|s| s.status == SessionStatus::Idle)
        },
        "session to park on quiesce",
    )
    .await;

    // Parked like a real `Done`: the segment is finalized, not left streaming,
    // and it kept the text the agent actually sent.
    let assistant: Vec<_> = entries(&core)
        .into_iter()
        .filter(|e| e.role == MessageRole::Assistant)
        .collect();
    assert_eq!(assistant.len(), 1, "one assistant entry");
    assert_eq!(
        assistant[0].status,
        Some(MessageStatus::Complete),
        "quiesced turn is finalized Complete, not left streaming"
    );
    assert!(
        format!("{:?}", assistant[0].parts).contains("Build finished successfully"),
        "the streamed answer survives the park: {:?}",
        assistant[0].parts
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_watchdog_never_ends_the_run() {
    init_quiesce_env();
    let dir = tempfile::tempdir().unwrap();
    let core = assemble(
        dir.path(),
        Arc::new(SilentAfterScript {
            script: answered_but_never_settled(),
        }),
    );
    let handle = core.doc_host.open(CHAT).unwrap();
    queue_as_viewer(
        handle.doc(),
        "cmd-quiesce-2",
        SessionCommandPayload::Run {
            request: run_request("build it"),
            message_id: "m-1".into(),
        },
    );

    wait_for(
        || {
            core.sessions
                .watch_sessions()
                .borrow()
                .first()
                .is_some_and(|s| s.status == SessionStatus::Idle)
        },
        "session to park on quiesce",
    )
    .await;

    // The distinction that makes this safe rather than the stall timeout the
    // design comment rejects: the session is still THERE, parked and warm,
    // with no error stamped on it. A watchdog that ended the run would leave
    // no session row at all.
    let sessions = core.sessions.watch_sessions();
    let parked = sessions.borrow().first().cloned();
    assert!(
        parked.is_some(),
        "the run survives the park — the watchdog never ends it"
    );
    assert!(
        entries(&core)
            .iter()
            .all(|e| e.status != Some(MessageStatus::Aborted)),
        "nothing is stamped aborted by a quiesce park"
    );
}
