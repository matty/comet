//! Provider-process boundary coverage uses the real Codex harness fixture.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use comet_doc::{SessionCommandEntry, SessionMessageEntry, TranscriptFrame};
use comet_engine::{EngineCore, HarnessRegistry, JournaledEvent};
use comet_harness::CodexHarness;
use comet_proto::{HarnessId, RunRequest, RuntimeMode};
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

// Tasks 2–4 consume this once their provider scenarios are added.
#[allow(dead_code)]
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

// Tasks 2–4 consume this once their provider scenarios are added.
#[allow(dead_code)]
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

// Tasks 2–4 consume this once their provider scenarios are added.
#[allow(dead_code)]
fn journal(fixture: &EngineFixture) -> Vec<JournaledEvent> {
    fixture
        .core
        .sessions
        .subscribe(CHAT, 0)
        .expect("subscribe to fixture journal")
        .0
}

// Tasks 2–4 consume this once their provider scenarios are added.
#[allow(dead_code)]
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
