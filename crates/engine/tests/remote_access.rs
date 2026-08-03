use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine as _;
use futures::stream::BoxStream;
use futures::{StreamExt, stream};

use chrono::Utc;
use comet_engine::{
    EngineCore, HarnessRegistry, LanServerStatus, RemoteConfigStore, RemoteRpcService,
    remote_method_allowed,
};
use comet_harness::{Harness, HarnessError, RunControls};
use comet_identity::DeviceIdentity;
use comet_proto::{
    AgentEvent, Device, HarnessId, Model, PROTOCOL_VERSION, ReasoningLevel, RemoteConnectionState,
    RemoteEndpoint, RemoteEntry, RunRequest, ServerId, SteeringMode, TrustedClient,
};
use comet_rpc::{RpcError, RpcReply, RpcService, TlsIdentity, connect_lan_rpc, methods};
use tokio::net::{TcpListener, TcpStream};

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

struct EngineFixture {
    _dir: tempfile::TempDir,
    core: EngineCore,
    lan_addr: std::net::SocketAddr,
}

impl EngineFixture {
    async fn start() -> Self {
        let probe = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let lan_addr = probe.local_addr().unwrap();
        drop(probe);
        let dir = tempfile::tempdir().unwrap();
        RemoteConfigStore::open(dir.path())
            .unwrap()
            .set_lan_settings(comet_proto::LanSettings {
                enabled: false,
                bind: lan_addr,
            })
            .unwrap();
        let core = EngineCore::assemble(
            dir.path(),
            Arc::new(HarnessRegistry::new()),
            HarnessId::Mock,
            None,
        )
        .unwrap();
        Self {
            _dir: dir,
            core,
            lan_addr,
        }
    }

    async fn local_call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, RpcError> {
        match self.core.rpc_service().handle(method, params).await? {
            RpcReply::Value(value) => Ok(value),
            RpcReply::Stream(_) => panic!("expected value reply"),
        }
    }

    async fn enable_lan(&self) -> Result<serde_json::Value, RpcError> {
        self.local_call(
            methods::SET_LAN_SETTINGS,
            serde_json::json!({"enabled": true, "bind": self.lan_addr}),
        )
        .await
    }

    async fn wait_for_status(&self, wanted: fn(&LanServerStatus) -> bool) {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if wanted(&self.core.lan_status()) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("LAN status transition");
    }
}

#[tokio::test]
async fn listener_is_closed_by_default_and_rebinds_after_enable() {
    let fixture = EngineFixture::start().await;
    assert!(TcpStream::connect(fixture.lan_addr).await.is_err());
    fixture.enable_lan().await.unwrap();
    fixture
        .wait_for_status(|status| matches!(status, LanServerStatus::Listening { .. }))
        .await;
    assert!(TcpStream::connect(fixture.lan_addr).await.is_ok());
    fixture.core.shutdown().await;
    assert!(TcpStream::connect(fixture.lan_addr).await.is_err());
}

#[tokio::test]
async fn bind_failure_does_not_stop_local_rpc() {
    let fixture = EngineFixture::start().await;
    let occupied = TcpListener::bind(fixture.lan_addr).await.unwrap();
    fixture.enable_lan().await.unwrap();
    fixture
        .wait_for_status(|status| matches!(status, LanServerStatus::BindFailed { .. }))
        .await;
    assert!(
        fixture
            .local_call(methods::LOCAL_DEVICE, serde_json::json!({}))
            .await
            .is_ok()
    );
    drop(occupied);
    fixture.core.shutdown().await;
}

#[tokio::test]
async fn changing_the_bind_address_replaces_the_listener() {
    let fixture = EngineFixture::start().await;
    fixture.enable_lan().await.unwrap();
    fixture
        .wait_for_status(|status| matches!(status, LanServerStatus::Listening { .. }))
        .await;
    let probe = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let replacement = probe.local_addr().unwrap();
    drop(probe);
    fixture
        .local_call(
            methods::SET_LAN_SETTINGS,
            serde_json::json!({"enabled": true, "bind": replacement}),
        )
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if matches!(
                fixture.core.lan_status(),
                LanServerStatus::Listening { bind } if bind == replacement
            ) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("listener rebinds");
    assert!(TcpStream::connect(fixture.lan_addr).await.is_err());
    assert!(TcpStream::connect(replacement).await.is_ok());
    fixture.core.shutdown().await;
}

#[tokio::test]
async fn server_hello_is_stable_and_pairing_returns_an_expiry() {
    let fixture = EngineFixture::start().await;
    let first = fixture
        .local_call(methods::SERVER_HELLO, serde_json::json!({}))
        .await
        .unwrap();
    let second = fixture
        .local_call(methods::SERVER_HELLO, serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(first, second);
    assert_eq!(first["protocolVersion"], PROTOCOL_VERSION);
    assert_eq!(first["deviceId"], fixture.core.device_id);

    let before = Utc::now();
    let pairing = fixture
        .local_call(methods::BEGIN_PAIRING, serde_json::json!({}))
        .await
        .unwrap();
    assert!(pairing["secret"].as_str().unwrap().contains('-'));
    let expires_at = pairing["expiresAt"]
        .as_str()
        .unwrap()
        .parse::<chrono::DateTime<Utc>>()
        .unwrap();
    assert!(expires_at > before);
    assert!(expires_at <= before + chrono::Duration::minutes(6));
    fixture.core.shutdown().await;
}

#[tokio::test]
async fn remote_status_updates_only_the_named_rows_connection_fields() {
    let fixture = EngineFixture::start().await;
    let original = RemoteEntry {
        server_id: ServerId::new("sha256:remote"),
        endpoint: RemoteEndpoint::parse("host.local:27655").unwrap(),
        name: "Build box".into(),
        pinned_spki_sha256: "pin".into(),
        protocol_version: 0,
        last_state: RemoteConnectionState::Connecting,
        created_at: Utc::now(),
        last_connected_at: None,
    };
    fixture
        .local_call(
            methods::PUT_REMOTE,
            serde_json::to_value(&original).unwrap(),
        )
        .await
        .unwrap();
    let connected_at = Utc::now();
    fixture
        .local_call(
            methods::REPORT_REMOTE_STATUS,
            serde_json::json!({
                "serverId": original.server_id,
                "lastState": "online",
                "protocolVersion": PROTOCOL_VERSION,
                "lastConnectedAt": connected_at,
                "name": "must not replace the row",
                "pinnedSpkiSha256": "must-not-change"
            }),
        )
        .await
        .unwrap();
    let rows = fixture.core.remote_config().watch_remotes();
    let row = &rows.borrow()[0];
    assert_eq!(row.name, original.name);
    assert_eq!(row.endpoint, original.endpoint);
    assert_eq!(row.pinned_spki_sha256, original.pinned_spki_sha256);
    assert_eq!(row.last_state, RemoteConnectionState::Online);
    assert_eq!(row.protocol_version, PROTOCOL_VERSION);
    assert_eq!(row.last_connected_at, Some(connected_at));
    fixture.core.shutdown().await;
}

#[tokio::test]
async fn persisted_online_remote_is_offline_when_engine_opens() {
    let dir = tempfile::tempdir().unwrap();
    let store = RemoteConfigStore::open(dir.path()).unwrap();
    store
        .put_remote(RemoteEntry {
            server_id: ServerId::new("sha256:remote"),
            endpoint: RemoteEndpoint::parse("host.local:27655").unwrap(),
            name: "Build box".into(),
            pinned_spki_sha256: "pin".into(),
            protocol_version: PROTOCOL_VERSION,
            last_state: RemoteConnectionState::Online,
            created_at: Utc::now(),
            last_connected_at: Some(Utc::now()),
        })
        .unwrap();
    drop(store);
    let core = EngineCore::assemble(
        dir.path(),
        Arc::new(HarnessRegistry::new()),
        HarnessId::Mock,
        None,
    )
    .unwrap();
    assert_eq!(
        core.remote_config().watch_remotes().borrow()[0].last_state,
        RemoteConnectionState::Offline
    );
    core.shutdown().await;
}

#[tokio::test]
async fn revoking_a_trusted_client_closes_its_active_connection() {
    let fixture = EngineFixture::start().await;
    let client_dir = tempfile::tempdir().unwrap();
    let client_identity = DeviceIdentity::load_or_create(client_dir.path()).unwrap();
    let client_tls = TlsIdentity::from_device_identity(&client_identity).unwrap();
    fixture
        .core
        .remote_config()
        .trust_client(TrustedClient {
            server_id: client_tls.server_id().clone(),
            name: "Test client".into(),
            pinned_spki_sha256: serde_json::to_value(client_tls.server_id())
                .unwrap()
                .as_str()
                .unwrap()
                .to_string(),
            paired_at: Utc::now(),
        })
        .unwrap();
    fixture.enable_lan().await.unwrap();
    fixture
        .wait_for_status(|status| matches!(status, LanServerStatus::Listening { .. }))
        .await;
    let server_tls = TlsIdentity::from_device_identity(fixture.core.device_identity()).unwrap();
    let client = connect_lan_rpc(fixture.lan_addr, &client_tls, &server_tls.pinned_server())
        .await
        .unwrap();
    assert!(
        client
            .call(methods::SERVER_HELLO, serde_json::json!({}))
            .await
            .is_ok()
    );
    fixture
        .local_call(
            methods::REVOKE_TRUSTED_CLIENT,
            serde_json::json!({"serverId": client_tls.server_id()}),
        )
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if client
                .call(methods::SERVER_HELLO, serde_json::json!({}))
                .await
                .is_err()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("revocation closes the active connection");
    fixture.core.shutdown().await;
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
async fn transcript_watch_ends_when_chat_loses_local_ownership() {
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
    let handle = fixture.core.doc_host.open("local-chat").unwrap();
    let RpcReply::Stream(mut stream) = fixture
        .service
        .handle(
            methods::WATCH_DOC_MESSAGES,
            serde_json::json!({"chatId":"local-chat"}),
        )
        .await
        .unwrap()
    else {
        panic!("expected transcript stream");
    };
    assert!(stream.next().await.unwrap().as_array().unwrap().is_empty());

    fixture.core.workspace.delete_chat("local-chat").unwrap();
    handle
        .write_user_message("after-delete", "must not escape", 1)
        .unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
            .await
            .expect("ownership loss closes transcript stream")
            .is_none()
    );
}

#[tokio::test]
async fn terminal_watch_ends_when_its_chat_loses_local_ownership() {
    let fixture = fixture_remote_service("device-b");
    let cwd = fixture._dir.path().join("terminal-cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    fixture
        .core
        .workspace
        .create_space(
            "local-space",
            "device-b",
            &cwd.to_string_lossy(),
            None,
            false,
        )
        .unwrap();
    fixture
        .core
        .workspace
        .create_chat("local-chat", "local-space", None, None)
        .unwrap();
    let RpcReply::Value(session) = fixture
        .service
        .handle(
            methods::OPEN_TERMINAL,
            serde_json::json!({"chatId":"local-chat", "cols":80, "rows":24}),
        )
        .await
        .unwrap()
    else {
        panic!("expected terminal session");
    };
    let terminal_id = session["id"].as_str().unwrap();
    let RpcReply::Stream(mut stream) = fixture
        .service
        .handle(
            methods::SUBSCRIBE_TERMINAL,
            serde_json::json!({"terminalId":terminal_id}),
        )
        .await
        .unwrap()
    else {
        panic!("expected terminal stream");
    };
    fixture
        .core
        .terminals
        .write(
            terminal_id,
            &base64::engine::general_purpose::STANDARD.encode("echo before-loss\r\n"),
        )
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
        .await
        .expect("initial terminal output")
        .expect("stream initially open");

    fixture.core.workspace.delete_chat("local-chat").unwrap();
    fixture
        .core
        .terminals
        .write(
            terminal_id,
            &base64::engine::general_purpose::STANDARD.encode("echo after-loss\r\n"),
        )
        .unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
            .await
            .expect("ownership loss closes terminal stream")
            .is_none()
    );
    fixture.core.terminals.close(terminal_id).unwrap();
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
    assert!(err.to_string().contains("outside the upload cache"));
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
