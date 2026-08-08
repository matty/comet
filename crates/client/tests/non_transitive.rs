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
    closing_method: Option<&'static str>,
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
            methods::WATCH_DOC_MESSAGES => {
                serde_json::to_value(comet_doc::TranscriptFrame::reset(&[])).unwrap()
            }
            methods::REPORT_REMOTE_STATUS => {
                return RpcReply::value(&serde_json::json!({"ok": true}));
            }
            other => return Err(RpcError::UnknownMethod(other.into())),
        };
        let stream = futures::stream::once(async move { snapshot });
        Ok(RpcReply::Stream(
            if self.keep_open && self.closing_method != Some(method) {
                stream.chain(futures::stream::pending()).boxed()
            } else {
                stream.boxed()
            },
        ))
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

struct FailHelloService;

#[async_trait::async_trait]
impl RpcService for FailHelloService {
    async fn handle(&self, method: &str, _params: serde_json::Value) -> Result<RpcReply, RpcError> {
        Err(RpcError::Failed(format!("{method} unavailable")))
    }
}

struct DisconnectAfterTranscriptService {
    hello: ServerHello,
    transcript_started: Arc<tokio::sync::Notify>,
    calls: Arc<Mutex<Vec<String>>>,
}

struct BlockingCallService {
    hello: ServerHello,
    block_started: Arc<tokio::sync::Notify>,
    calls: Arc<Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl RpcService for BlockingCallService {
    async fn handle(&self, method: &str, _params: serde_json::Value) -> Result<RpcReply, RpcError> {
        self.calls.lock().unwrap().push(method.to_string());
        match method {
            methods::SERVER_HELLO => RpcReply::value(&self.hello),
            methods::WATCH_DEVICES
            | methods::WATCH_SPACES
            | methods::WATCH_CHATS
            | methods::WATCH_SESSIONS => Ok(RpcReply::Stream(
                futures::stream::once(async { serde_json::json!([]) })
                    .chain(futures::stream::pending())
                    .boxed(),
            )),
            methods::WATCH_DOC_MESSAGES => Ok(RpcReply::Stream(
                futures::stream::once(async {
                    serde_json::to_value(comet_doc::TranscriptFrame::reset(&[])).unwrap()
                })
                .chain(futures::stream::pending())
                .boxed(),
            )),
            "Block" => {
                self.block_started.notify_waiters();
                futures::future::pending().await
            }
            other => Err(RpcError::UnknownMethod(other.into())),
        }
    }
}

#[async_trait::async_trait]
impl RpcService for DisconnectAfterTranscriptService {
    async fn handle(&self, method: &str, _params: serde_json::Value) -> Result<RpcReply, RpcError> {
        self.calls.lock().unwrap().push(method.to_string());
        match method {
            methods::SERVER_HELLO => RpcReply::value(&self.hello),
            methods::WATCH_CHATS => {
                let notify = self.transcript_started.clone();
                Ok(RpcReply::Stream(
                    futures::stream::once(async { serde_json::json!([]) })
                        .chain(
                            futures::stream::pending::<serde_json::Value>()
                                .take_until(async move { notify.notified().await }),
                        )
                        .boxed(),
                ))
            }
            methods::WATCH_DEVICES | methods::WATCH_SPACES | methods::WATCH_SESSIONS => {
                Ok(RpcReply::Stream(
                    futures::stream::once(async { serde_json::json!([]) })
                        .chain(futures::stream::pending())
                        .boxed(),
                ))
            }
            methods::WATCH_DOC_MESSAGES => {
                self.transcript_started.notify_waiters();
                Ok(RpcReply::Stream(futures::stream::pending().boxed()))
            }
            other => Err(RpcError::UnknownMethod(other.into())),
        }
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
        closing_method: None,
    }))
}

fn service_with_closing_stream(
    id: ServerId,
    name: &str,
    remotes: Vec<RemoteEntry>,
    calls: Arc<Mutex<Vec<String>>>,
    closing_method: &'static str,
) -> RpcClient {
    comet_rpc::memory_client(Arc::new(FixtureService {
        hello: ServerHello {
            protocol_version: PROTOCOL_VERSION,
            server_id: id,
            device_id: format!("{name}-device"),
            name: name.into(),
            capabilities: Vec::new(),
        },
        remotes,
        chats: vec![chat("chat-1")],
        calls,
        keep_open: true,
        closing_method: Some(closing_method),
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
async fn protocol_7_peer_is_rejected_before_it_can_ignore_the_harness_choice() {
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
        7,
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
    let incompatible_state = RemoteConnectionState::IncompatibleVersion { remote: 7 };

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

#[tokio::test]
async fn transcript_selection_during_backoff_does_not_stop_reconnect() {
    let a_id = secure_server('a');
    let b_id = secure_server('b');
    let local = service(
        a_id,
        "A",
        vec![remote(b_id.clone(), "B", 'b')],
        Vec::new(),
        Arc::new(Mutex::new(Vec::new())),
    );
    let b_calls = Arc::new(Mutex::new(Vec::new()));
    let b = service(b_id.clone(), "B", Vec::new(), Vec::new(), b_calls.clone());
    let connector = Arc::new(FixtureConnector(Mutex::new(HashMap::from([(
        b_id.clone(),
        VecDeque::from([Err(RemoteConnectError::Transport("not yet".into())), Ok(b)]),
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
        &RemoteConnectionState::Unreachable {
            message: "not yet".into(),
        },
    )
    .await;

    federation
        .send(comet_client::FederationCommand::WatchTranscript(Some(
            comet_client::ServerRef::new(b_id.clone(), "chat-1"),
        )))
        .unwrap();
    wait_for_state(
        &mut federation,
        &mut snapshot,
        &b_id,
        &RemoteConnectionState::Online,
    )
    .await;
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while !b_calls
            .lock()
            .unwrap()
            .iter()
            .any(|method| method == methods::WATCH_DOC_MESSAGES)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn one_required_remote_stream_ending_clears_the_bucket() {
    let a_id = secure_server('a');
    let b_id = secure_server('b');
    let local = service(
        a_id,
        "A",
        vec![remote(b_id.clone(), "B", 'b')],
        Vec::new(),
        Arc::new(Mutex::new(Vec::new())),
    );
    let b = service_with_closing_stream(
        b_id.clone(),
        "B",
        Vec::new(),
        Arc::new(Mutex::new(Vec::new())),
        methods::WATCH_CHATS,
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
        &RemoteConnectionState::Online,
    )
    .await;
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
async fn local_required_stream_ending_publishes_empty_offline_bucket() {
    let a_id = secure_server('a');
    let local = service_with_closing_stream(
        a_id.clone(),
        "A",
        Vec::new(),
        Arc::new(Mutex::new(Vec::new())),
        methods::WATCH_CHATS,
    );
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
        &RemoteConnectionState::Offline,
    )
    .await;
    assert!(snapshot.server(&a_id).unwrap().chats.is_empty());
}

#[tokio::test]
async fn registry_entry_colliding_with_local_id_is_ignored() {
    let a_id = secure_server('a');
    let local = service(
        a_id.clone(),
        "A",
        vec![remote(a_id.clone(), "Imposter", 'a')],
        Vec::new(),
        Arc::new(Mutex::new(Vec::new())),
    );
    let imposter = service(
        a_id.clone(),
        "Imposter",
        Vec::new(),
        Vec::new(),
        Arc::new(Mutex::new(Vec::new())),
    );
    let connector = Arc::new(FixtureConnector(Mutex::new(HashMap::from([(
        a_id.clone(),
        VecDeque::from([Ok(imposter)]),
    )]))));
    let inspect = connector.clone();
    let data_dir = tempfile::tempdir().unwrap();
    let _federation = Federation::with_connector(local, data_dir.path(), connector)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    assert_eq!(inspect.0.lock().unwrap().get(&a_id).unwrap().len(), 1);
}

#[tokio::test]
async fn generic_remote_call_rejects_local_admin_methods_client_side() {
    let a_id = secure_server('a');
    let b_id = secure_server('b');
    let local = service(
        a_id,
        "A",
        vec![remote(b_id.clone(), "B", 'b')],
        Vec::new(),
        Arc::new(Mutex::new(Vec::new())),
    );
    let b_calls = Arc::new(Mutex::new(Vec::new()));
    let b = service(b_id.clone(), "B", Vec::new(), Vec::new(), b_calls.clone());
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
        &RemoteConnectionState::Online,
    )
    .await;

    federation
        .send(comet_client::FederationCommand::Call {
            server_id: b_id,
            method: methods::WATCH_REMOTES,
            params: serde_json::Value::Null,
        })
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(
        !b_calls
            .lock()
            .unwrap()
            .iter()
            .any(|method| method == methods::WATCH_REMOTES)
    );
}

#[tokio::test]
async fn failed_server_hello_uses_backoff_before_redial() {
    let a_id = secure_server('a');
    let b_id = secure_server('b');
    let local = service(
        a_id,
        "A",
        vec![remote(b_id.clone(), "B", 'b')],
        Vec::new(),
        Arc::new(Mutex::new(Vec::new())),
    );
    let connector = Arc::new(FixtureConnector(Mutex::new(HashMap::from([(
        b_id.clone(),
        VecDeque::from([
            Ok(comet_rpc::memory_client(Arc::new(FailHelloService))),
            Ok(comet_rpc::memory_client(Arc::new(FailHelloService))),
        ]),
    )]))));
    let inspect = connector.clone();
    let data_dir = tempfile::tempdir().unwrap();
    let _federation = Federation::with_connector(local, data_dir.path(), connector)
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(40)).await;
    assert_eq!(inspect.0.lock().unwrap().get(&b_id).unwrap().len(), 1);
}

#[tokio::test]
async fn registry_closure_clears_remote_buckets_and_marks_local_offline() {
    let a_id = secure_server('a');
    let b_id = secure_server('b');
    let local = service_with(
        a_id.clone(),
        "A",
        vec![remote(b_id.clone(), "B", 'b')],
        Vec::new(),
        Arc::new(Mutex::new(Vec::new())),
        PROTOCOL_VERSION,
        false,
    );
    let connector = Arc::new(FixtureConnector(Mutex::new(HashMap::from([(
        b_id.clone(),
        VecDeque::from([Err(RemoteConnectError::IdentityChanged)]),
    )]))));
    let data_dir = tempfile::tempdir().unwrap();
    let mut federation = Federation::with_connector(local, data_dir.path(), connector)
        .await
        .unwrap();
    let mut saw_local_offline = false;
    let mut saw_remote_removed = false;

    while let Some(event) = federation.recv().await {
        match event {
            FederationEvent::ServerChanged(server) if server.id == a_id => {
                saw_local_offline |= server.connection == RemoteConnectionState::Offline;
            }
            FederationEvent::ServerRemoved(id) if id == b_id => saw_remote_removed = true,
            _ => {}
        }
    }
    assert!(saw_local_offline);
    assert!(saw_remote_removed);
}

#[tokio::test]
async fn disconnect_clears_selected_transcript_and_reconnect_resubscribes() {
    let a_id = secure_server('a');
    let b_id = secure_server('b');
    let local = service(
        a_id,
        "A",
        vec![remote(b_id.clone(), "B", 'b')],
        Vec::new(),
        Arc::new(Mutex::new(Vec::new())),
    );
    let first_calls = Arc::new(Mutex::new(Vec::new()));
    let first = comet_rpc::memory_client(Arc::new(DisconnectAfterTranscriptService {
        hello: ServerHello {
            protocol_version: PROTOCOL_VERSION,
            server_id: b_id.clone(),
            device_id: "b-device".into(),
            name: "B".into(),
            capabilities: Vec::new(),
        },
        transcript_started: Arc::new(tokio::sync::Notify::new()),
        calls: first_calls,
    }));
    let second_calls = Arc::new(Mutex::new(Vec::new()));
    let second = service(
        b_id.clone(),
        "B",
        Vec::new(),
        Vec::new(),
        second_calls.clone(),
    );
    let connector = Arc::new(FixtureConnector(Mutex::new(HashMap::from([(
        b_id.clone(),
        VecDeque::from([Ok(first), Ok(second)]),
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
        &RemoteConnectionState::Online,
    )
    .await;
    let selected = comet_client::ServerRef::new(b_id.clone(), "chat-1");
    federation
        .send(comet_client::FederationCommand::WatchTranscript(Some(
            selected.clone(),
        )))
        .unwrap();

    let mut saw_clear_before_offline = false;
    let mut saw_offline = false;
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            match federation.recv().await.unwrap() {
                FederationEvent::Transcript { chat, entries }
                    if chat == selected && entries.is_empty() && !saw_offline =>
                {
                    saw_clear_before_offline = true
                }
                FederationEvent::ServerChanged(server)
                    if server.id == b_id && server.connection == RemoteConnectionState::Offline =>
                {
                    assert!(
                        saw_clear_before_offline,
                        "transcript was not cleared before offline state"
                    );
                    saw_offline = true;
                }
                FederationEvent::ServerChanged(server)
                    if server.id == b_id
                        && server.connection == RemoteConnectionState::Online
                        && saw_offline =>
                {
                    return;
                }
                _ => {}
            }
        }
    })
    .await
    .unwrap();

    assert!(saw_clear_before_offline);
    assert!(
        second_calls
            .lock()
            .unwrap()
            .iter()
            .any(|method| method == methods::WATCH_DOC_MESSAGES)
    );
}

#[tokio::test]
async fn immediately_ending_resource_stream_uses_backoff_before_redial() {
    let a_id = secure_server('a');
    let b_id = secure_server('b');
    let local = service(
        a_id,
        "A",
        vec![remote(b_id.clone(), "B", 'b')],
        Vec::new(),
        Arc::new(Mutex::new(Vec::new())),
    );
    let first = service_with_closing_stream(
        b_id.clone(),
        "B",
        Vec::new(),
        Arc::new(Mutex::new(Vec::new())),
        methods::WATCH_CHATS,
    );
    let second = service_with_closing_stream(
        b_id.clone(),
        "B",
        Vec::new(),
        Arc::new(Mutex::new(Vec::new())),
        methods::WATCH_CHATS,
    );
    let connector = Arc::new(FixtureConnector(Mutex::new(HashMap::from([(
        b_id.clone(),
        VecDeque::from([Ok(first), Ok(second)]),
    )]))));
    let inspect = connector.clone();
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

    tokio::time::sleep(std::time::Duration::from_millis(40)).await;
    assert_eq!(inspect.0.lock().unwrap().get(&b_id).unwrap().len(), 1);
}

#[tokio::test]
async fn invalid_persisted_pin_is_terminal_until_registry_replacement_or_reconnect() {
    let a_id = secure_server('a');
    let b_id = secure_server('b');
    let local = service(
        a_id,
        "A",
        vec![remote(b_id.clone(), "B", 'b')],
        Vec::new(),
        Arc::new(Mutex::new(Vec::new())),
    );
    let b = service(
        b_id.clone(),
        "B",
        Vec::new(),
        Vec::new(),
        Arc::new(Mutex::new(Vec::new())),
    );
    let connector = Arc::new(FixtureConnector(Mutex::new(HashMap::from([(
        b_id.clone(),
        VecDeque::from([
            Err(RemoteConnectError::InvalidConfiguration("bad pin".into())),
            Ok(b),
        ]),
    )]))));
    let inspect = connector.clone();
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
    assert_eq!(inspect.0.lock().unwrap().get(&b_id).unwrap().len(), 1);
}

#[tokio::test]
async fn repeated_short_sessions_accumulate_exponential_backoff() {
    let a_id = secure_server('a');
    let b_id = secure_server('b');
    let local = service(
        a_id,
        "A",
        vec![remote(b_id.clone(), "B", 'b')],
        Vec::new(),
        Arc::new(Mutex::new(Vec::new())),
    );
    let mut attempts = VecDeque::new();
    for _ in 0..4 {
        attempts.push_back(Ok(service_with_closing_stream(
            b_id.clone(),
            "B",
            Vec::new(),
            Arc::new(Mutex::new(Vec::new())),
            methods::WATCH_CHATS,
        )));
    }
    let connector = Arc::new(FixtureConnector(Mutex::new(HashMap::from([(
        b_id.clone(),
        attempts,
    )]))));
    let inspect = connector.clone();
    let data_dir = tempfile::tempdir().unwrap();
    let _federation = Federation::with_connector(local, data_dir.path(), connector)
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(450)).await;
    assert_eq!(
        inspect.0.lock().unwrap().get(&b_id).unwrap().len(),
        1,
        "fourth short-lived connection was dialed without accumulated backoff"
    );
}

#[tokio::test]
async fn selection_handoff_clears_old_transcript_before_new_subscription_event() {
    let a_id = secure_server('a');
    let b_id = secure_server('b');
    let c_id = secure_server('c');
    let local = service(
        a_id,
        "A",
        vec![
            remote(b_id.clone(), "B", 'b'),
            remote(c_id.clone(), "C", 'c'),
        ],
        Vec::new(),
        Arc::new(Mutex::new(Vec::new())),
    );
    let b_calls = Arc::new(Mutex::new(Vec::new()));
    let c_calls = Arc::new(Mutex::new(Vec::new()));
    let connector = Arc::new(FixtureConnector(Mutex::new(HashMap::from([
        (
            b_id.clone(),
            VecDeque::from([Ok(service(
                b_id.clone(),
                "B",
                Vec::new(),
                Vec::new(),
                b_calls.clone(),
            ))]),
        ),
        (
            c_id.clone(),
            VecDeque::from([Ok(service(
                c_id.clone(),
                "C",
                Vec::new(),
                Vec::new(),
                c_calls.clone(),
            ))]),
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
        &RemoteConnectionState::Online,
    )
    .await;
    wait_for_state(
        &mut federation,
        &mut snapshot,
        &c_id,
        &RemoteConnectionState::Online,
    )
    .await;
    let b_chat = comet_client::ServerRef::new(b_id.clone(), "chat-1");
    let c_chat = comet_client::ServerRef::new(c_id.clone(), "chat-1");
    federation
        .send(comet_client::FederationCommand::WatchTranscript(Some(
            b_chat.clone(),
        )))
        .unwrap();
    loop {
        if matches!(federation.recv().await, Some(FederationEvent::Transcript { chat, .. }) if chat == b_chat)
        {
            break;
        }
    }

    federation
        .send(comet_client::FederationCommand::WatchTranscript(Some(
            c_chat.clone(),
        )))
        .unwrap();
    let mut transcript_events = Vec::new();
    while transcript_events.len() < 2 {
        if let Some(event @ FederationEvent::Transcript { .. }) = federation.recv().await {
            transcript_events.push(event);
        }
    }
    let first = transcript_events.remove(0);
    let second = transcript_events.remove(0);
    assert!(
        matches!(first, FederationEvent::Transcript { chat, entries } if chat == b_chat && entries.is_empty())
    );
    assert!(matches!(second, FederationEvent::Transcript { chat, .. } if chat == c_chat));
    assert!(
        b_calls
            .lock()
            .unwrap()
            .iter()
            .any(|method| method == methods::WATCH_DOC_MESSAGES)
    );
    assert!(
        c_calls
            .lock()
            .unwrap()
            .iter()
            .any(|method| method == methods::WATCH_DOC_MESSAGES)
    );
}

#[tokio::test]
async fn shutdown_clears_selected_transcript_after_supervisors_stop() {
    let a_id = secure_server('a');
    let calls = Arc::new(Mutex::new(Vec::new()));
    let local = service(a_id.clone(), "A", Vec::new(), Vec::new(), calls);
    let connector = Arc::new(FixtureConnector(Mutex::new(HashMap::new())));
    let data_dir = tempfile::tempdir().unwrap();
    let mut federation = Federation::with_connector(local, data_dir.path(), connector)
        .await
        .unwrap();
    let selected = comet_client::ServerRef::new(a_id.clone(), "chat-1");
    federation
        .send(comet_client::FederationCommand::WatchTranscript(Some(
            selected.clone(),
        )))
        .unwrap();
    loop {
        if matches!(federation.recv().await, Some(FederationEvent::Transcript { chat, .. }) if chat == selected)
        {
            break;
        }
    }

    federation
        .send(comet_client::FederationCommand::Shutdown)
        .unwrap();
    let mut saw_clear = false;
    while let Some(event) = federation.recv().await {
        if matches!(event, FederationEvent::Transcript { chat, entries } if chat == selected && entries.is_empty())
        {
            saw_clear = true;
        }
    }
    assert!(saw_clear);
}

#[tokio::test]
async fn stalled_prior_owner_cannot_block_selection_handoff_or_shutdown() {
    let a_id = secure_server('a');
    let b_id = secure_server('b');
    let c_id = secure_server('c');
    let local = service(
        a_id,
        "A",
        vec![
            remote(b_id.clone(), "B", 'b'),
            remote(c_id.clone(), "C", 'c'),
        ],
        Vec::new(),
        Arc::new(Mutex::new(Vec::new())),
    );
    let block_started = Arc::new(tokio::sync::Notify::new());
    let remote_client = comet_rpc::memory_client(Arc::new(BlockingCallService {
        hello: ServerHello {
            protocol_version: PROTOCOL_VERSION,
            server_id: b_id.clone(),
            device_id: "b-device".into(),
            name: "B".into(),
            capabilities: Vec::new(),
        },
        block_started: block_started.clone(),
        calls: Arc::new(Mutex::new(Vec::new())),
    }));
    let c_calls = Arc::new(Mutex::new(Vec::new()));
    let c_block_started = Arc::new(tokio::sync::Notify::new());
    let c = comet_rpc::memory_client(Arc::new(BlockingCallService {
        hello: ServerHello {
            protocol_version: PROTOCOL_VERSION,
            server_id: c_id.clone(),
            device_id: "c-device".into(),
            name: "C".into(),
            capabilities: Vec::new(),
        },
        block_started: c_block_started.clone(),
        calls: c_calls.clone(),
    }));
    let connector = Arc::new(FixtureConnector(Mutex::new(HashMap::from([
        (b_id.clone(), VecDeque::from([Ok(remote_client)])),
        (c_id.clone(), VecDeque::from([Ok(c)])),
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
        &RemoteConnectionState::Online,
    )
    .await;
    wait_for_state(
        &mut federation,
        &mut snapshot,
        &c_id,
        &RemoteConnectionState::Online,
    )
    .await;
    let selected = comet_client::ServerRef::new(b_id.clone(), "chat-1");
    federation
        .send(comet_client::FederationCommand::WatchTranscript(Some(
            selected.clone(),
        )))
        .unwrap();
    loop {
        if matches!(federation.recv().await, Some(FederationEvent::Transcript { chat, .. }) if chat == selected)
        {
            break;
        }
    }
    federation
        .send(comet_client::FederationCommand::Call {
            server_id: b_id,
            method: "Block",
            params: serde_json::Value::Null,
        })
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), block_started.notified())
        .await
        .unwrap();

    let new_selected = comet_client::ServerRef::new(c_id.clone(), "chat-1");
    federation
        .send(comet_client::FederationCommand::WatchTranscript(Some(
            new_selected.clone(),
        )))
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_millis(500), async {
        while !c_calls
            .lock()
            .unwrap()
            .iter()
            .any(|method| method == methods::WATCH_DOC_MESSAGES)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("stalled prior owner blocked the new transcript subscription");
    let mut saw_old_clear = false;
    loop {
        match federation.recv().await.unwrap() {
            FederationEvent::Transcript { chat, entries }
                if chat == selected && entries.is_empty() =>
            {
                saw_old_clear = true
            }
            FederationEvent::Transcript { chat, .. } if chat == new_selected => break,
            _ => {}
        }
    }
    federation
        .send(comet_client::FederationCommand::Call {
            server_id: c_id,
            method: "Block",
            params: serde_json::Value::Null,
        })
        .unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        c_block_started.notified(),
    )
    .await
    .unwrap();
    federation
        .send(comet_client::FederationCommand::WatchTranscript(None))
        .unwrap();
    federation
        .send(comet_client::FederationCommand::Shutdown)
        .unwrap();
    let mut saw_new_clear = false;
    tokio::time::timeout(std::time::Duration::from_millis(500), async {
        while let Some(event) = federation.recv().await {
            if matches!(event, FederationEvent::Transcript { chat, entries } if chat == new_selected && entries.is_empty()) {
                saw_new_clear = true;
            }
        }
    })
    .await
    .expect("stalled transcript owner blocked manager shutdown");
    assert!(saw_old_clear);
    assert!(saw_new_clear);
}
