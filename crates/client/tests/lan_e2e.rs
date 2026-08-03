//! Acceptance coverage for direct, explicitly configured LAN remotes.
//!
//! Pairing/TLS failure cases live in `comet-rpc/tests/secure_lan.rs`; the
//! authoritative operational allowlist and resource ownership cases live in
//! `comet-engine/tests/remote_access.rs`. This test exercises the client-level
//! A/B/C topology and the offline-without-cache contract.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use comet_client::{Federation, RemoteConnectError, RemoteConnector, ServerSnapshot};
use comet_proto::{
    Chat, PROTOCOL_VERSION, RemoteConnectionState, RemoteEndpoint, RemoteEntry, ServerHello,
    ServerId,
};
use comet_rpc::{RpcClient, RpcError, RpcReply, RpcService, methods};
use futures::{FutureExt, StreamExt};

fn fingerprint(character: char) -> String {
    std::iter::repeat_n(character, 64).collect()
}

fn server(character: char) -> ServerId {
    ServerId::new(format!("sha256:{}", fingerprint(character)))
}

fn remote(server_id: ServerId, name: &str, character: char) -> RemoteEntry {
    RemoteEntry {
        server_id,
        endpoint: RemoteEndpoint {
            host: format!("{name}.local"),
            port: 27_655,
        },
        name: name.into(),
        pinned_spki_sha256: fingerprint(character),
        protocol_version: PROTOCOL_VERSION,
        last_state: RemoteConnectionState::Offline,
        created_at: chrono::Utc::now(),
        last_connected_at: None,
    }
}

fn chat(id: &str) -> Chat {
    Chat {
        id: id.into(),
        device_id: "device-b".into(),
        title: Some("B-only chat".into()),
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

struct AcceptanceService {
    hello: ServerHello,
    remotes: tokio::sync::watch::Receiver<Vec<RemoteEntry>>,
    chats: Vec<Chat>,
    stop: Option<Arc<tokio::sync::Notify>>,
    calls: Arc<Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl RpcService for AcceptanceService {
    async fn handle(&self, method: &str, _params: serde_json::Value) -> Result<RpcReply, RpcError> {
        self.calls.lock().unwrap().push(method.to_owned());
        match method {
            methods::SERVER_HELLO => RpcReply::value(&self.hello),
            methods::WATCH_REMOTES => {
                let receiver = self.remotes.clone();
                Ok(RpcReply::Stream(
                    futures::stream::unfold((receiver, true), |(mut receiver, first)| async move {
                        if !first && receiver.changed().await.is_err() {
                            return None;
                        }
                        let value = serde_json::to_value(receiver.borrow().clone()).unwrap();
                        Some((value, (receiver, false)))
                    })
                    .boxed(),
                ))
            }
            methods::WATCH_CHATS => {
                self.resource_stream(serde_json::to_value(&self.chats).unwrap())
            }
            methods::WATCH_DEVICES | methods::WATCH_SPACES | methods::WATCH_SESSIONS => {
                self.resource_stream(serde_json::json!([]))
            }
            methods::WATCH_DOC_MESSAGES => Ok(RpcReply::Stream(futures::stream::pending().boxed())),
            methods::LIST_REPOS => RpcReply::value(&serde_json::json!([])),
            methods::REPORT_REMOTE_STATUS => RpcReply::value(&serde_json::json!({"ok": true})),
            other => Err(RpcError::UnknownMethod(other.into())),
        }
    }
}

impl AcceptanceService {
    fn resource_stream(&self, value: serde_json::Value) -> Result<RpcReply, RpcError> {
        let stream = futures::stream::once(async move { value });
        Ok(RpcReply::Stream(match &self.stop {
            Some(stop) => {
                let stop = stop.clone();
                stream
                    .chain(futures::stream::pending().take_until(async move {
                        stop.notified().await;
                    }))
                    .boxed()
            }
            None => stream.chain(futures::stream::pending()).boxed(),
        }))
    }
}

struct Connector(Mutex<HashMap<ServerId, VecDeque<Result<RpcClient, RemoteConnectError>>>>);

impl RemoteConnector for Connector {
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
                .unwrap_or_else(|| Err(RemoteConnectError::Transport("server stopped".into())))
        }
        .boxed()
    }
}

fn service(
    id: ServerId,
    name: &str,
    remotes: tokio::sync::watch::Receiver<Vec<RemoteEntry>>,
    chats: Vec<Chat>,
    stop: Option<Arc<tokio::sync::Notify>>,
    calls: Arc<Mutex<Vec<String>>>,
) -> RpcClient {
    comet_rpc::memory_client(Arc::new(AcceptanceService {
        hello: ServerHello {
            protocol_version: PROTOCOL_VERSION,
            server_id: id,
            device_id: format!("device-{name}"),
            name: name.into(),
            capabilities: Vec::new(),
        },
        remotes,
        chats,
        stop,
        calls,
    }))
}

async fn receive_until(
    federation: &mut Federation,
    snapshot: &mut ServerSnapshot,
    predicate: impl Fn(&ServerSnapshot) -> bool,
) {
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while !predicate(snapshot) {
            snapshot.apply(federation.recv().await.expect("federation remains active"));
        }
    })
    .await
    .expect("expected federation state");
}

#[tokio::test]
async fn direct_connections_are_operational_and_non_transitive() {
    let a_id = server('a');
    let b_id = server('b');
    let c_id = server('c');
    let b_entry = remote(b_id.clone(), "b", 'b');
    let c_entry = remote(c_id.clone(), "c", 'c');

    let (a_remotes_tx, a_remotes_rx) = tokio::sync::watch::channel(vec![b_entry.clone()]);
    let (_b_remotes_tx, b_remotes_rx) = tokio::sync::watch::channel(vec![c_entry.clone()]);
    let (_c_remotes_tx, c_remotes_rx) = tokio::sync::watch::channel(Vec::new());
    let b_stop = Arc::new(tokio::sync::Notify::new());
    let b_calls = Arc::new(Mutex::new(Vec::new()));

    let local = service(
        a_id.clone(),
        "a",
        a_remotes_rx,
        Vec::new(),
        None,
        Arc::new(Mutex::new(Vec::new())),
    );
    let b = service(
        b_id.clone(),
        "b",
        b_remotes_rx,
        vec![chat("chat-b")],
        Some(b_stop.clone()),
        b_calls.clone(),
    );
    let c = service(
        c_id.clone(),
        "c",
        c_remotes_rx,
        Vec::new(),
        None,
        Arc::new(Mutex::new(Vec::new())),
    );
    let connector = Arc::new(Connector(Mutex::new(HashMap::from([
        (b_id.clone(), VecDeque::from([Ok(b)])),
        (c_id.clone(), VecDeque::from([Ok(c)])),
    ]))));
    let data_dir = tempfile::tempdir().unwrap();
    let mut federation = Federation::with_connector(local, data_dir.path(), connector)
        .await
        .unwrap();
    let mut snapshot = ServerSnapshot::default();

    receive_until(&mut federation, &mut snapshot, |state| {
        state.server(&b_id).is_some_and(|server| {
            server.connection == RemoteConnectionState::Online && !server.chats.is_empty()
        })
    })
    .await;
    assert!(
        snapshot.server(&c_id).is_none(),
        "B's remote C must not leak to A"
    );
    federation
        .request(b_id.clone(), methods::LIST_REPOS, serde_json::json!({}))
        .await
        .expect("direct operational call to B");
    assert!(
        b_calls
            .lock()
            .unwrap()
            .iter()
            .any(|call| call == methods::LIST_REPOS)
    );
    assert!(
        !b_calls
            .lock()
            .unwrap()
            .iter()
            .any(|call| call == methods::WATCH_REMOTES)
    );

    a_remotes_tx.send(vec![b_entry, c_entry]).unwrap();
    receive_until(&mut federation, &mut snapshot, |state| {
        state
            .server(&c_id)
            .is_some_and(|server| server.connection == RemoteConnectionState::Online)
    })
    .await;

    b_stop.notify_waiters();
    receive_until(&mut federation, &mut snapshot, |state| {
        state.server(&b_id).is_some_and(|server| {
            matches!(
                server.connection,
                RemoteConnectionState::Offline | RemoteConnectionState::Unreachable { .. }
            ) && server.spaces.is_empty()
                && server.chats.is_empty()
                && server.sessions.is_empty()
        })
    })
    .await;
}
