//! Provider-process boundary coverage uses the real Codex harness fixture.

#![allow(dead_code)] // Later D49 scenarios share this fixture's helpers.

use std::path::PathBuf;
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
    _data_dir: tempfile::TempDir,
    _codex_home: tempfile::TempDir,
    _cwd_dir: tempfile::TempDir,
    core: EngineCore,
    client: RpcClient,
    cwd: PathBuf,
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
async fn fixture_owns_the_codex_home_and_manually_titled_workspace_chat() {
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
