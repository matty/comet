//! Provider-process boundary coverage uses the real Codex harness fixture.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use comet_doc::{
    MessagePart, MessageRole, MessageStatus, SessionCommandEntry, SessionCommandPayload,
    SessionCommandStatus, SessionMessageEntry, TranscriptFrame,
};
use comet_engine::{EngineCore, HarnessRegistry, JournaledEvent, RunJournal};
use comet_harness::CodexHarness;
use comet_proto::{
    AgentEvent, ApprovalDecision, DoneStatus, HarnessId, RunRequest, RuntimeMode, SessionStatus,
};
use comet_rpc::RpcClient;

const SPACE: &str = "space-provider-boundary";
const CHAT: &str = "chat-provider-boundary";

struct EngineFixture {
    client: RpcClient,
    core: EngineCore,
    cwd: PathBuf,
    _data_dir: tempfile::TempDir,
    _codex_home: tempfile::TempDir,
    _cwd_dir: tempfile::TempDir,
}

impl EngineFixture {
    fn new() -> Self {
        let data_dir = tempfile::tempdir().expect("create engine data dir");
        let codex_home = tempfile::tempdir().expect("create Codex home");
        let cwd_dir = tempfile::tempdir().expect("create run cwd");
        std::fs::write(codex_home.path().join("auth.json"), "{}").expect("write fake auth");

        let registry = HarnessRegistry::new();
        registry.register(Arc::new(
            CodexHarness::new()
                .with_executable(env!("CARGO_BIN_EXE_engine-fake-codex"))
                .with_codex_home(codex_home.path()),
        ));
        let core =
            EngineCore::assemble(data_dir.path(), Arc::new(registry), HarnessId::Codex, None)
                .expect("assemble engine core");
        let cwd = cwd_dir.path().to_path_buf();
        let cwd_string = cwd.to_string_lossy().into_owned();
        core.workspace
            .create_space(SPACE, &core.device_id, &cwd_string, None, false)
            .expect("create fixture space");
        core.workspace
            .create_chat(CHAT, SPACE, None, Some(cwd_string))
            .expect("create fixture chat");
        assert!(
            core.workspace
                .rename_chat(CHAT, "Provider boundary")
                .expect("rename fixture chat"),
            "fixture chat must be manually titled to suppress auto-titling"
        );

        let client = comet_rpc::memory_client(core.rpc_service());
        Self {
            _data_dir: data_dir,
            _codex_home: codex_home,
            _cwd_dir: cwd_dir,
            core,
            client,
            cwd,
        }
    }
}

fn codex_request(fixture: &EngineFixture, prompt: &str, mode: RuntimeMode) -> RunRequest {
    RunRequest {
        prompt: prompt.into(),
        harness: Some(HarnessId::Codex),
        model: Some("gpt-5.6-sol".into()),
        cwd: fixture.cwd.to_string_lossy().into_owned(),
        ..RunRequest::for_session(mode)
    }
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

fn commands(fixture: &EngineFixture) -> Vec<SessionCommandEntry> {
    fixture
        .core
        .doc_host
        .open(CHAT)
        .expect("open fixture chat")
        .doc()
        .read_commands()
        .expect("read fixture commands")
}

fn journal(fixture: &EngineFixture) -> Vec<JournaledEvent> {
    fixture
        .core
        .sessions
        .subscribe(CHAT, 0)
        .expect("subscribe to fixture journal")
        .0
}

fn apply_message_frame(entries: &mut Vec<SessionMessageEntry>, frame: TranscriptFrame) {
    comet_doc::apply_transcript_frame(entries, frame).expect("apply transcript frame");
}

#[tokio::test]
async fn fixture_wires_codex_request_manual_title_and_rpc_harness_list() {
    let fixture = EngineFixture::new();
    let request = codex_request(&fixture, "fixture smoke", RuntimeMode::ApprovalRequired);

    assert_eq!(request.harness, Some(HarnessId::Codex));
    assert_eq!(request.cwd, fixture.cwd.to_string_lossy());
    assert_eq!(
        fixture
            .core
            .workspace
            .doc()
            .chat(CHAT)
            .expect("read fixture chat")
            .expect("fixture chat row")
            .title,
        Some("Provider boundary".into())
    );
    let harnesses = fixture
        .client
        .call(comet_rpc::methods::LIST_HARNESSES, serde_json::Value::Null)
        .await
        .expect("list fixture harnesses");
    assert_eq!(harnesses[0]["id"], "codex");
}

#[tokio::test]
async fn fake_codex_model_discovery_and_command_endpoint_cross_engine_rpc() {
    let fixture = EngineFixture::new();

    // Keep this untyped: the picker decodes this literal RPC reply, so a Rust
    // round trip would miss a producer/consumer shape drift.
    let models = fixture
        .client
        .call(
            comet_rpc::methods::LIST_MODELS,
            serde_json::json!({"harness": "codex"}),
        )
        .await
        .expect("discover fake Codex models");
    assert_eq!(models["source"], "live");
    assert!(
        models["models"]
            .as_array()
            .expect("models array on the wire")
            .iter()
            .any(|model| model["id"] == "gpt-5.7-nova"),
        "fake-only model must cross the harness and engine RPC boundary: {models}"
    );
    let home_label = models["models"]
        .as_array()
        .expect("models array on the wire")
        .iter()
        .find(|model| model["id"] == "codex-home-echo")
        .expect("Codex home echo model on the wire")["label"]
        .as_str()
        .expect("Codex home echo label");
    assert_eq!(
        Path::new(home_label)
            .canonicalize()
            .expect("canonicalize child Codex home"),
        fixture
            ._codex_home
            .path()
            .canonicalize()
            .expect("canonicalize fixture Codex home")
    );

    let commands = fixture
        .client
        .call(
            comet_rpc::methods::LIST_COMMANDS,
            serde_json::json!({"harness": "codex", "cwd": fixture.cwd}),
        )
        .await
        .expect("list fake Codex commands");
    assert_eq!(commands["commands"], serde_json::json!([]));
}

#[tokio::test]
async fn fake_codex_rejected_resume_falls_back_to_a_fresh_durable_session() {
    let fixture = EngineFixture::new();
    let mut request = codex_request(&fixture, "scenario:resumed", RuntimeMode::ApprovalRequired);
    request.resume = Some("resume-fail".into());
    let queued = fixture
        .client
        .call(
            comet_rpc::methods::QUEUE_COMMAND,
            serde_json::json!({
                "chatId": CHAT,
                "command": SessionCommandPayload::Run {
                    request,
                    message_id: "m-resume-fallback".into(),
                },
            }),
        )
        .await
        .expect("queue resume command through RPC");
    let command_id = queued["commandId"].as_str().expect("queued command id");

    wait_for(
        || {
            commands(&fixture).iter().any(|command| {
                command.id == command_id && command.status == SessionCommandStatus::Applied
            })
        },
        "resume command applied",
    )
    .await;
    wait_for(
        || {
            fixture
                .core
                .sessions
                .session_status(CHAT)
                .map(|session| session.status)
                == Some(SessionStatus::Idle)
        },
        "resumed session idle",
    )
    .await;

    let replay = journal(&fixture);
    assert!(
        replay.iter().any(|entry| {
            matches!(&entry.event, AgentEvent::SessionStarted { session_id, .. } if session_id == "th-fresh")
        }),
        "the native Codex fallback must durably start th-fresh: {replay:?}"
    );
    assert!(matches!(
        replay.last().map(|entry| &entry.event),
        Some(AgentEvent::Done {
            status: DoneStatus::Completed,
            session_id: Some(session_id),
            ..
        }) if session_id == "th-fresh"
    ));
    assert_eq!(
        fixture.core.workspace.chat_harness_session(CHAT),
        Some((
            "th-fresh".into(),
            Some(fixture.cwd.to_string_lossy().into_owned()),
        ))
    );
}

#[tokio::test]
async fn fake_codex_cancelled_approval_round_trips_via_rpc_and_aborts_durably() {
    let fixture = EngineFixture::new();
    let mut messages = fixture
        .client
        .subscribe(
            comet_rpc::methods::WATCH_DOC_MESSAGES,
            serde_json::json!({"chatId": CHAT}),
        )
        .await
        .expect("watch fixture messages through RPC");
    let reset: TranscriptFrame = serde_json::from_value(
        tokio::time::timeout(Duration::from_secs(10), messages.recv())
            .await
            .expect("message reset before timeout")
            .expect("message stream reset"),
    )
    .expect("deserialize message reset");
    assert!(matches!(reset, TranscriptFrame::Reset { .. }));
    let mut materialized = Vec::new();
    apply_message_frame(&mut materialized, reset);

    let queued = fixture
        .client
        .call(
            comet_rpc::methods::QUEUE_COMMAND,
            serde_json::json!({
                "chatId": CHAT,
                "command": SessionCommandPayload::Run {
                    request: codex_request(&fixture, "scenario:cancel-approval", RuntimeMode::ApprovalRequired),
                    message_id: "m-cancel-approval".into(),
                },
            }),
        )
        .await
        .expect("queue cancellation run through RPC");
    let run_command_id = queued["commandId"]
        .as_str()
        .expect("queued run command id")
        .to_owned();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let request_id = loop {
        let frame: TranscriptFrame = serde_json::from_value(
            tokio::time::timeout_at(deadline, messages.recv())
                .await
                .expect("unresolved approval before timeout")
                .expect("message stream remains open"),
        )
        .expect("deserialize approval frame");
        apply_message_frame(&mut materialized, frame);
        if let Some(request_id) =
            materialized
                .iter()
                .flat_map(|entry| &entry.parts)
                .find_map(|part| match part {
                    MessagePart::Approval {
                        request_id,
                        decision: None,
                        ..
                    } => Some(request_id.clone()),
                    _ => None,
                })
        {
            break request_id;
        }
    };

    let queued = fixture
        .client
        .call(
            comet_rpc::methods::QUEUE_COMMAND,
            serde_json::json!({
                "chatId": CHAT,
                "command": SessionCommandPayload::RespondApproval {
                    request_id: request_id.clone(),
                    decision: ApprovalDecision::DenyAndInterrupt {
                        message: "stop before touching that file".into(),
                    },
                },
            }),
        )
        .await
        .expect("queue cancellation approval through RPC");
    let approval_command_id = queued["commandId"]
        .as_str()
        .expect("queued approval command id")
        .to_owned();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let frame: TranscriptFrame = serde_json::from_value(
            tokio::time::timeout_at(deadline, messages.recv())
                .await
                .expect("aborted approval before timeout")
                .expect("message stream remains open"),
        )
        .expect("deserialize terminal frame");
        apply_message_frame(&mut materialized, frame);
        if materialized.iter().any(|entry| {
            entry.role == MessageRole::Assistant
                && entry.status == Some(MessageStatus::Aborted)
                && entry.parts.iter().any(|part| {
                    matches!(
                        part,
                        MessagePart::Approval {
                            request_id: resolved_id,
                            decision: Some(ApprovalDecision::DenyAndInterrupt { message }),
                            ..
                        } if resolved_id == &request_id && message == "stop before touching that file"
                    )
                })
        }) {
            break;
        }
    }

    wait_for(
        || {
            let entries = commands(&fixture);
            entries.iter().any(|entry| {
                entry.id == run_command_id && entry.status == SessionCommandStatus::Applied
            }) && entries.iter().any(|entry| {
                entry.id == approval_command_id && entry.status == SessionCommandStatus::Applied
            }) && fixture
                .core
                .sessions
                .session_status(CHAT)
                .map(|session| session.status)
                == Some(SessionStatus::Idle)
        },
        "both cancellation commands applied and session idle",
    )
    .await;

    let replay = journal(&fixture);
    assert!(matches!(
        replay.last().map(|entry| &entry.event),
        Some(AgentEvent::Done {
            status: DoneStatus::Interrupted,
            ..
        })
    ));
    assert!(
        RunJournal::open(fixture._data_dir.path().join("local-store/journals"))
            .expect("open fixture journal")
            .stale_sessions()
            .expect("scan fixture journal")
            .is_empty(),
        "interrupted approval must leave no stale session"
    );
}
