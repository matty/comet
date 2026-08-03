use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use comet_client::{
    Federation, FederationEvent, RemoteConnectError, RemoteConnector, ServerSnapshot, ServerState,
};
use comet_proto::{
    Chat, PROTOCOL_VERSION, RemoteConnectionState, RemoteEndpoint, RemoteEntry, ServerHello,
    ServerId,
};
use comet_rpc::{RpcClient, RpcError, RpcReply, RpcService, methods};
use futures::{FutureExt, StreamExt};

fn server(value: &str) -> ServerId {
    ServerId::new(format!("sha256:{value}"))
}

fn fingerprint(character: char) -> String {
    std::iter::repeat_n(character, 64).collect()
}

fn secure_server(character: char) -> ServerId {
    ServerId::new(format!("sha256:{}", fingerprint(character)))
}

fn chat(id: &str) -> Chat {
    Chat {
        id: id.into(),
        device_id: "device".into(),
        title: None,
        archived: false,
        cwd: None,
        branch: None,
        checkout_id: None,
        config: None,
        last_message_preview: None,
        last_message_at: None,
        created_at: chrono::Utc::now(),
        harness_session_id: None,
        harness_session_cwd: None,
        space_id: None,
        last_seen_at: None,
    }
}

#[test]
fn equal_raw_chat_ids_remain_in_separate_server_buckets() {
    let mut state = ServerSnapshot::default();
    for id in [server("b"), server("c")] {
        let mut bucket = ServerState::empty(id, "remote", RemoteConnectionState::Online);
        bucket.chats.push(chat("chat-1"));
        state.apply(FederationEvent::ServerChanged(bucket));
    }

    assert_eq!(state.server(&server("b")).unwrap().chats[0].id, "chat-1");
    assert_eq!(state.server(&server("c")).unwrap().chats[0].id, "chat-1");
    assert_ne!(
        state.chat_ref(&server("b"), "chat-1"),
        state.chat_ref(&server("c"), "chat-1")
    );
}

#[test]
fn disconnect_clears_remote_children_but_retains_server_entry() {
    let id = server("b");
    let mut state = ServerSnapshot::default();
    let mut online = ServerState::empty(id.clone(), "Build box", RemoteConnectionState::Online);
    online.chats.push(chat("chat-1"));
    state.apply(FederationEvent::ServerChanged(online));
    state.apply(FederationEvent::ServerChanged(ServerState::offline(
        id.clone(),
        "Build box",
    )));

    let bucket = state.server(&id).unwrap();
    assert_eq!(bucket.connection, RemoteConnectionState::Offline);
    assert!(bucket.chats.is_empty());
}

#[test]
fn removing_direct_registry_entry_removes_only_that_server_bucket() {
    let mut state = ServerSnapshot::default();
    for id in [server("b"), server("c")] {
        state.apply(FederationEvent::ServerChanged(ServerState::empty(
            id,
            "remote",
            RemoteConnectionState::Online,
        )));
    }
    state.apply(FederationEvent::ServerRemoved(server("b")));

    assert!(state.server(&server("b")).is_none());
    assert!(state.server(&server("c")).is_some());
}

struct FixtureService {
    hello: ServerHello,
    remotes: Vec<RemoteEntry>,
    chats: Vec<Chat>,
    calls: Arc<Mutex<Vec<String>>>,
    keep_open: bool,
}

#[async_trait::async_trait]
impl RpcService for FixtureService {
    async fn handle(&self, method: &str, _params: serde_json::Value) -> Result<RpcReply, RpcError> {
        self.calls.lock().unwrap().push(method.to_string());
        let snapshot = match method {
            methods::SERVER_HELLO => return RpcReply::value(&self.hello),
            methods::WATCH_REMOTES => serde_json::to_value(&self.remotes).unwrap(),
            methods::WATCH_CHATS => serde_json::to_value(&self.chats).unwrap(),
            methods::WATCH_DEVICES | methods::WATCH_SPACES | methods::WATCH_SESSIONS => {
                serde_json::json!([])
            }
            methods::WATCH_DOC_MESSAGES => serde_json::json!([]),
            methods::REPORT_REMOTE_STATUS => {
                return RpcReply::value(&serde_json::json!({"ok": true}));
            }
            other => return Err(RpcError::UnknownMethod(other.into())),
        };
        let stream = futures::stream::once(async move { snapshot });
        Ok(RpcReply::Stream(if self.keep_open {
            stream.chain(futures::stream::pending()).boxed()
        } else {
            stream.boxed()
        }))
    }
}

struct FixtureConnector(Mutex<HashMap<ServerId, VecDeque<Result<RpcClient, RemoteConnectError>>>>);

impl RemoteConnector for FixtureConnector {
    fn connect<'a>(
        &'a self,
        entry: &'a RemoteEntry,
        _identity: &'a comet_rpc::TlsIdentity,
    ) -> futures::future::BoxFuture<'a, Result<RpcClient, RemoteConnectError>> {
        async move {
            self.0
                .lock()
                .unwrap()
                .get_mut(&entry.server_id)
                .and_then(VecDeque::pop_front)
                .ok_or_else(|| RemoteConnectError::Transport("no fixture client".into()))
                .and_then(|result| result)
        }
        .boxed()
    }
}

fn remote(server_id: ServerId, name: &str, character: char) -> RemoteEntry {
    RemoteEntry {
        server_id,
        endpoint: RemoteEndpoint {
            host: "fixture".into(),
            port: 1,
        },
        name: name.into(),
        pinned_spki_sha256: fingerprint(character),
        protocol_version: PROTOCOL_VERSION,
        last_state: RemoteConnectionState::Offline,
        created_at: chrono::Utc::now(),
        last_connected_at: None,
    }
}

fn service(
    id: ServerId,
    name: &str,
    remotes: Vec<RemoteEntry>,
    chats: Vec<Chat>,
    calls: Arc<Mutex<Vec<String>>>,
) -> RpcClient {
    service_with(id, name, remotes, chats, calls, PROTOCOL_VERSION, true)
}

fn service_with(
    id: ServerId,
    name: &str,
    remotes: Vec<RemoteEntry>,
    chats: Vec<Chat>,
    calls: Arc<Mutex<Vec<String>>>,
    protocol_version: u32,
    keep_open: bool,
) -> RpcClient {
    comet_rpc::memory_client(Arc::new(FixtureService {
        hello: ServerHello {
            protocol_version,
            server_id: id,
            device_id: format!("{name}-device"),
            name: name.into(),
            capabilities: Vec::new(),
        },
        remotes,
        chats,
        calls,
        keep_open,
    }))
}

#[tokio::test]
async fn remote_supervisor_never_watches_remote_registry_or_discovers_c_through_b() {
    let a_id = secure_server('a');
    let b_id = secure_server('b');
    let c_id = secure_server('c');
    let b_entry = remote(b_id.clone(), "B", 'b');
    let c_entry = remote(c_id.clone(), "C", 'c');
    let local_calls = Arc::new(Mutex::new(Vec::new()));
    let b_calls = Arc::new(Mutex::new(Vec::new()));
    let local = service(a_id.clone(), "A", vec![b_entry], Vec::new(), local_calls);
    let b = service(
        b_id.clone(),
        "B",
        vec![c_entry],
        vec![chat("chat-b")],
        b_calls.clone(),
    );
    let connector = Arc::new(FixtureConnector(Mutex::new(HashMap::from([(
        b_id.clone(),
        VecDeque::from([Ok(b)]),
    )]))));
    let data_dir = tempfile::tempdir().unwrap();
    let mut federation = Federation::with_connector(local, data_dir.path(), connector)
        .await
        .unwrap();
    let mut snapshot = ServerSnapshot::default();

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let event = federation.recv().await.unwrap();
            snapshot.apply(event);
            if snapshot.server(&b_id).is_some_and(|server| {
                server.connection == RemoteConnectionState::Online && !server.chats.is_empty()
            }) {
                break;
            }
        }
    })
    .await
    .unwrap();

    assert!(snapshot.server(&a_id).is_some());
    assert!(snapshot.server(&b_id).is_some());
    assert!(snapshot.server(&c_id).is_none());
    assert!(
        !b_calls
            .lock()
            .unwrap()
            .iter()
            .any(|method| method == methods::WATCH_REMOTES)
    );
}

async fn wait_for_state(
    federation: &mut Federation,
    snapshot: &mut ServerSnapshot,
    id: &ServerId,
    expected: &RemoteConnectionState,
) {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            snapshot.apply(federation.recv().await.unwrap());
            if snapshot
                .server(id)
                .is_some_and(|server| &server.connection == expected)
            {
                return;
            }
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn identity_change_is_terminal_until_explicit_reconnect() {
    let a_id = secure_server('a');
    let b_id = secure_server('b');
    let b_entry = remote(b_id.clone(), "B", 'b');
    let calls = Arc::new(Mutex::new(Vec::new()));
    let local = service(a_id, "A", vec![b_entry], Vec::new(), calls.clone());
    let b = service(
        b_id.clone(),
        "B",
        Vec::new(),
        Vec::new(),
        Arc::new(Mutex::new(Vec::new())),
    );
    let connector = Arc::new(FixtureConnector(Mutex::new(HashMap::from([(
        b_id.clone(),
        VecDeque::from([Err(RemoteConnectError::IdentityChanged), Ok(b)]),
    )]))));
    let data_dir = tempfile::tempdir().unwrap();
    let mut federation = Federation::with_connector(local, data_dir.path(), connector)
        .await
        .unwrap();
    let mut snapshot = ServerSnapshot::default();

    wait_for_state(
        &mut federation,
        &mut snapshot,
        &b_id,
        &RemoteConnectionState::IdentityChanged,
    )
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert_eq!(
        snapshot.server(&b_id).unwrap().connection,
        RemoteConnectionState::IdentityChanged
    );
    federation
        .send(comet_client::FederationCommand::Reconnect(b_id.clone()))
        .unwrap();
    wait_for_state(
        &mut federation,
        &mut snapshot,
        &b_id,
        &RemoteConnectionState::Online,
    )
    .await;
    assert!(
        calls
            .lock()
            .unwrap()
            .iter()
            .any(|method| method == methods::REPORT_REMOTE_STATUS)
    );
}

#[tokio::test]
async fn incompatible_version_reports_remote_version_and_waits_for_reconnect() {
    let a_id = secure_server('a');
    let b_id = secure_server('b');
    let b_entry = remote(b_id.clone(), "B", 'b');
    let local = service(
        a_id,
        "A",
        vec![b_entry],
        Vec::new(),
        Arc::new(Mutex::new(Vec::new())),
    );
    let incompatible = service_with(
        b_id.clone(),
        "B",
        Vec::new(),
        Vec::new(),
        Arc::new(Mutex::new(Vec::new())),
        99,
        true,
    );
    let compatible = service(
        b_id.clone(),
        "B",
        Vec::new(),
        Vec::new(),
        Arc::new(Mutex::new(Vec::new())),
    );
    let connector = Arc::new(FixtureConnector(Mutex::new(HashMap::from([(
        b_id.clone(),
        VecDeque::from([Ok(incompatible), Ok(compatible)]),
    )]))));
    let data_dir = tempfile::tempdir().unwrap();
    let mut federation = Federation::with_connector(local, data_dir.path(), connector)
        .await
        .unwrap();
    let mut snapshot = ServerSnapshot::default();
    let incompatible_state = RemoteConnectionState::IncompatibleVersion { remote: 99 };

    wait_for_state(&mut federation, &mut snapshot, &b_id, &incompatible_state).await;
    federation
        .send(comet_client::FederationCommand::Reconnect(b_id.clone()))
        .unwrap();
    wait_for_state(
        &mut federation,
        &mut snapshot,
        &b_id,
        &RemoteConnectionState::Online,
    )
    .await;
}

#[tokio::test]
async fn disconnect_emits_empty_offline_bucket_before_reconnect() {
    let a_id = secure_server('a');
    let b_id = secure_server('b');
    let b_entry = remote(b_id.clone(), "B", 'b');
    let local = service(
        a_id,
        "A",
        vec![b_entry],
        Vec::new(),
        Arc::new(Mutex::new(Vec::new())),
    );
    let b = service_with(
        b_id.clone(),
        "B",
        Vec::new(),
        vec![chat("remote-chat")],
        Arc::new(Mutex::new(Vec::new())),
        PROTOCOL_VERSION,
        false,
    );
    let connector = Arc::new(FixtureConnector(Mutex::new(HashMap::from([(
        b_id.clone(),
        VecDeque::from([Ok(b)]),
    )]))));
    let data_dir = tempfile::tempdir().unwrap();
    let mut federation = Federation::with_connector(local, data_dir.path(), connector)
        .await
        .unwrap();
    let mut snapshot = ServerSnapshot::default();

    wait_for_state(
        &mut federation,
        &mut snapshot,
        &b_id,
        &RemoteConnectionState::Offline,
    )
    .await;
    assert!(snapshot.server(&b_id).unwrap().chats.is_empty());
}

#[tokio::test]
async fn snapshot_order_is_local_then_direct_registry_order() {
    let a_id = secure_server('a');
    let b_id = secure_server('b');
    let c_id = secure_server('c');
    let local = service(
        a_id.clone(),
        "A",
        vec![
            remote(b_id.clone(), "B", 'b'),
            remote(c_id.clone(), "C", 'c'),
        ],
        Vec::new(),
        Arc::new(Mutex::new(Vec::new())),
    );
    let connector = Arc::new(FixtureConnector(Mutex::new(HashMap::from([
        (
            b_id.clone(),
            VecDeque::from([Err(RemoteConnectError::IdentityChanged)]),
        ),
        (
            c_id.clone(),
            VecDeque::from([Err(RemoteConnectError::IdentityChanged)]),
        ),
    ]))));
    let data_dir = tempfile::tempdir().unwrap();
    let mut federation = Federation::with_connector(local, data_dir.path(), connector)
        .await
        .unwrap();
    let mut snapshot = ServerSnapshot::default();
    wait_for_state(
        &mut federation,
        &mut snapshot,
        &b_id,
        &RemoteConnectionState::IdentityChanged,
    )
    .await;
    wait_for_state(
        &mut federation,
        &mut snapshot,
        &c_id,
        &RemoteConnectionState::IdentityChanged,
    )
    .await;

    let order: Vec<_> = snapshot.servers().map(|server| server.id.clone()).collect();
    assert_eq!(order, vec![a_id, b_id, c_id]);
}

#[tokio::test]
async fn local_transcript_watch_uses_trusted_local_connection() {
    let a_id = secure_server('a');
    let calls = Arc::new(Mutex::new(Vec::new()));
    let local = service(a_id.clone(), "A", Vec::new(), Vec::new(), calls.clone());
    let connector = Arc::new(FixtureConnector(Mutex::new(HashMap::new())));
    let data_dir = tempfile::tempdir().unwrap();
    let mut federation = Federation::with_connector(local, data_dir.path(), connector)
        .await
        .unwrap();
    let mut snapshot = ServerSnapshot::default();
    wait_for_state(
        &mut federation,
        &mut snapshot,
        &a_id,
        &RemoteConnectionState::Online,
    )
    .await;

    federation
        .send(comet_client::FederationCommand::WatchTranscript(Some(
            comet_client::ServerRef::new(a_id, "chat-local"),
        )))
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            if calls
                .lock()
                .unwrap()
                .iter()
                .any(|method| method == methods::WATCH_DOC_MESSAGES)
            {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
