use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::{StreamExt, stream};

use comet_engine::{EngineCore, HarnessRegistry, RemoteRpcService, remote_method_allowed};
use comet_harness::{Harness, HarnessError, RunControls};
use comet_proto::{AgentEvent, Device, HarnessId, Model, ReasoningLevel, RunRequest, SteeringMode};
use comet_rpc::{RpcError, RpcReply, RpcService, methods};

struct EmptyHarness;

#[async_trait]
impl Harness for EmptyHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }

    fn display_name(&self) -> &str {
        "Empty"
    }

    fn supports_steering(&self) -> bool {
        false
    }

    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::TurnBoundary
    }

    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[]
    }

    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        Ok(Vec::new())
    }

    async fn run(
        &self,
        _request: RunRequest,
        _controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        Ok(stream::empty().boxed())
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    core: EngineCore,
    service: RemoteRpcService,
}

fn fixture_remote_service(device_id: &str) -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("device-id"), device_id).expect("device id");
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(EmptyHarness));
    let core = EngineCore::assemble(dir.path(), Arc::new(registry), HarnessId::Mock, None)
        .expect("engine assembles");
    let service = RemoteRpcService::new(core.rpc_service(), device_id);
    Fixture {
        _dir: dir,
        core,
        service,
    }
}

async fn first_nonempty_stream_item(reply: RpcReply) -> serde_json::Value {
    let RpcReply::Stream(mut stream) = reply else {
        panic!("expected stream reply");
    };
    tokio::time::timeout(std::time::Duration::from_secs(2), async move {
        loop {
            let value = stream.next().await.expect("stream remains open");
            if value.as_array().is_some_and(|rows| !rows.is_empty()) {
                return value;
            }
        }
    })
    .await
    .expect("filtered workspace row published")
}

fn rpc_error(result: Result<RpcReply, RpcError>) -> RpcError {
    match result {
        Ok(_) => panic!("expected RPC failure"),
        Err(error) => error,
    }
}

#[test]
fn administrative_and_proxy_methods_are_denied() {
    for method in [
        methods::WATCH_REMOTES,
        methods::PUT_REMOTE,
        methods::REMOVE_REMOTE,
        methods::REPORT_REMOTE_STATUS,
        methods::GET_LAN_SETTINGS,
        methods::SET_LAN_SETTINGS,
        methods::BEGIN_PAIRING,
        methods::WATCH_TRUSTED_CLIENTS,
        methods::REVOKE_TRUSTED_CLIENT,
        methods::APPLY_UPDATE,
        methods::START_AGENT_LOGIN,
        methods::COMPLETE_AGENT_LOGIN,
        methods::FORGET_AGENT_ACCOUNT,
        methods::POLL_AGENT_LOGIN,
        methods::CANCEL_AGENT_LOGIN,
        methods::UPDATE_STATUS,
    ] {
        assert!(
            !remote_method_allowed(method),
            "{method} escaped the LAN denylist"
        );
    }
}

#[test]
fn operational_surface_is_explicitly_allowed() {
    for method in [
        methods::SERVER_HELLO,
        methods::LOCAL_DEVICE,
        methods::LIST_HARNESSES,
        methods::LIST_MODELS,
        methods::QUEUE_COMMAND,
        methods::WATCH_DOC_MESSAGES,
        methods::WATCH_CHATS,
        methods::WATCH_DEVICES,
        methods::WATCH_SPACES,
        methods::WATCH_SESSIONS,
        methods::MUTATE,
        methods::LIST_REPOS,
        methods::ADD_REPO,
        methods::CLONE_REPO,
        methods::CREATE_REPO,
        methods::LIST_BRANCHES,
        methods::LIST_REFS,
        methods::SWITCH_REF,
        methods::LIST_FOLDERS,
        methods::CREATE_WORKTREE,
        methods::DELETE_WORKTREE,
        methods::OPEN_TERMINAL,
        methods::SUBSCRIBE_TERMINAL,
        methods::WRITE_TERMINAL,
        methods::RESIZE_TERMINAL,
        methods::CLOSE_TERMINAL,
        methods::WATCH_CHECKOUT_DIFFS,
        methods::LIST_AGENT_ACCOUNTS,
        methods::ACTIVATE_AGENT_ACCOUNT,
        methods::UPLOAD_CHUNK,
        methods::UPLOAD_COMMIT,
        methods::READ_ATTACHMENT_CHUNK,
    ] {
        assert!(remote_method_allowed(method), "{method} was not allowed");
    }
    assert!(!remote_method_allowed("FutureOperationalMethod"));
}

#[tokio::test]
async fn denied_method_is_reported_as_unknown() {
    let fixture = fixture_remote_service("device-b");
    let err = rpc_error(
        fixture
            .service
            .handle(methods::GET_LAN_SETTINGS, serde_json::json!({}))
            .await,
    );
    assert!(matches!(err, RpcError::UnknownMethod(ref name) if name == methods::GET_LAN_SETTINGS));
}

#[tokio::test]
async fn target_device_cannot_name_another_machine() {
    let fixture = fixture_remote_service("device-b");
    let err = rpc_error(
        fixture
            .service
            .handle(
                methods::LIST_REFS,
                serde_json::json!({"repoPath":"/repo","targetDeviceId":"device-c"}),
            )
            .await,
    );
    assert!(
        err.to_string()
            .contains("targetDeviceId must match device-b")
    );
}

#[tokio::test]
async fn orphaned_foreign_transcript_is_not_remotely_addressable() {
    let fixture = fixture_remote_service("device-b");
    fixture
        .core
        .doc_host
        .open("foreign-chat")
        .expect("create an unindexed session document");

    let err = rpc_error(
        fixture
            .service
            .handle(
                methods::WATCH_DOC_MESSAGES,
                serde_json::json!({"chatId":"foreign-chat"}),
            )
            .await,
    );
    assert!(err.to_string().contains("not owned by this server"));
}

#[tokio::test]
async fn workspace_watches_expose_only_receiver_owned_rows() {
    let fixture = fixture_remote_service("device-b");
    fixture
        .core
        .workspace
        .doc()
        .upsert_device(&Device {
            id: "device-c".into(),
            name: "foreign".into(),
            platform: "test".into(),
            last_seen_at: None,
            created_at: None,
            version: None,
        })
        .unwrap();
    fixture
        .core
        .workspace
        .create_space("local-space", "device-b", "/local", None, false)
        .unwrap();
    fixture
        .core
        .workspace
        .create_space("foreign-space", "device-c", "/foreign", None, false)
        .unwrap();
    fixture
        .core
        .workspace
        .create_chat("local-chat", "local-space", None, None)
        .unwrap();
    fixture
        .core
        .workspace
        .create_chat("foreign-chat", "foreign-space", None, None)
        .unwrap();

    let mut raw_devices = fixture.core.workspace.watch_devices();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if raw_devices.borrow().iter().any(|row| row.id == "device-c") {
                break;
            }
            raw_devices
                .changed()
                .await
                .expect("device watch remains open");
        }
    })
    .await
    .expect("foreign device reaches underlying workspace watch");

    let spaces = first_nonempty_stream_item(
        fixture
            .service
            .handle(methods::WATCH_SPACES, serde_json::json!({}))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(spaces.as_array().unwrap().len(), 1);
    assert_eq!(spaces[0]["id"], "local-space");

    let chats = first_nonempty_stream_item(
        fixture
            .service
            .handle(methods::WATCH_CHATS, serde_json::json!({}))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(chats.as_array().unwrap().len(), 1);
    assert_eq!(chats[0]["id"], "local-chat");

    let devices = first_nonempty_stream_item(
        fixture
            .service
            .handle(methods::WATCH_DEVICES, serde_json::json!({}))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(devices.as_array().unwrap().len(), 1);
    assert_eq!(devices[0]["id"], "device-b");
}

#[tokio::test]
async fn terminal_ids_not_opened_through_this_authoritative_service_are_denied() {
    let fixture = fixture_remote_service("device-b");
    let err = rpc_error(
        fixture
            .service
            .handle(
                methods::WRITE_TERMINAL,
                serde_json::json!({"terminalId":"ipc-terminal", "data":""}),
            )
            .await,
    );
    assert!(err.to_string().contains("not owned by this server"));
}

#[tokio::test]
async fn attachment_path_must_belong_to_the_named_local_chat() {
    let fixture = fixture_remote_service("device-b");
    let local_dir = fixture._dir.path().join("local-chat");
    let foreign_dir = fixture._dir.path().join("foreign-chat");
    std::fs::create_dir_all(&local_dir).unwrap();
    std::fs::create_dir_all(&foreign_dir).unwrap();
    let foreign_image = foreign_dir.join("private.png");
    std::fs::write(&foreign_image, b"not-really-a-png").unwrap();
    fixture
        .core
        .workspace
        .create_space(
            "local-space",
            "device-b",
            &local_dir.to_string_lossy(),
            None,
            false,
        )
        .unwrap();
    fixture
        .core
        .workspace
        .create_space(
            "foreign-space",
            "device-c",
            &foreign_dir.to_string_lossy(),
            None,
            false,
        )
        .unwrap();
    fixture
        .core
        .workspace
        .create_chat("local-chat", "local-space", None, None)
        .unwrap();
    fixture
        .core
        .workspace
        .create_chat("foreign-chat", "foreign-space", None, None)
        .unwrap();

    let err = rpc_error(
        fixture
            .service
            .handle(
                methods::READ_ATTACHMENT_CHUNK,
                serde_json::json!({
                    "chatId":"local-chat", "path":foreign_image, "offset":0
                }),
            )
            .await,
    );
    assert!(err.to_string().contains("not owned by this server"));
}

#[tokio::test]
async fn remote_mutations_cannot_create_or_change_foreign_rows() {
    let fixture = fixture_remote_service("device-b");
    fixture
        .core
        .workspace
        .create_space("local-space", "device-b", "/local", None, false)
        .unwrap();
    fixture
        .core
        .workspace
        .create_chat("local-chat", "local-space", None, None)
        .unwrap();
    fixture
        .core
        .workspace
        .create_space("foreign-space", "device-c", "/foreign", None, false)
        .unwrap();

    for params in [
        serde_json::json!({
            "op":"createSpace", "spaceId":"bad", "deviceId":"device-c", "path":"/bad"
        }),
        serde_json::json!({
            "op":"createChat", "chatId":"bad-chat", "spaceId":"foreign-space"
        }),
        serde_json::json!({
            "op":"renameSpace", "spaceId":"foreign-space", "name":"stolen"
        }),
        serde_json::json!({
            "op":"renameDevice", "deviceId":"device-c", "name":"stolen"
        }),
        serde_json::json!({
            "op":"renameDevice", "deviceId":"device-b", "name":"remote rename"
        }),
        serde_json::json!({
            "op":"setChatHost", "chatId":"local-chat", "deviceId":"device-b"
        }),
    ] {
        let err = rpc_error(fixture.service.handle(methods::MUTATE, params).await);
        assert!(
            err.to_string().contains("not owned by this server")
                || err.to_string().contains("must match device-b")
                || err.to_string().contains("not allowed over LAN")
        );
    }
}
