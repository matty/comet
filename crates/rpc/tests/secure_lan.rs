use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use comet_identity::DeviceIdentity;
use comet_proto::ServerId;
use comet_rpc::{
    LanConnectError, LanPairingState, PairingLimiter, PairingSession, PairingTranscript, RpcError,
    RpcReply, RpcService, TlsIdentity, accept_lan_rpc, connect_lan_rpc, pair_client, serve_pairing,
};
use data_encoding::BASE32_NOPAD;
use futures::{SinkExt, StreamExt};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, Error as TlsError, SignatureScheme};
use tokio_rustls::TlsConnector;
use tokio_tungstenite::tungstenite::Message as WsMessage;

#[test]
fn pairing_confirmation_binds_both_keys_and_nonces() {
    let secret = [7_u8; 16];
    let transcript = PairingTranscript::new(&[1, 2, 3], &[4, 5, 6], [1; 32], [2; 32]);
    let tag = transcript.confirm_client(&secret);

    assert!(transcript.verify_client(&secret, &tag));
    assert!(
        !PairingTranscript::new(&[9, 2, 3], &[4, 5, 6], [1; 32], [2; 32])
            .verify_client(&secret, &tag)
    );
    assert!(
        !PairingTranscript::new(&[1, 2, 3], &[9, 5, 6], [1; 32], [2; 32])
            .verify_client(&secret, &tag)
    );
    assert!(
        !PairingTranscript::new(&[1, 2, 3], &[4, 5, 6], [9; 32], [2; 32])
            .verify_client(&secret, &tag)
    );
    assert!(
        !PairingTranscript::new(&[1, 2, 3], &[4, 5, 6], [1; 32], [9; 32])
            .verify_client(&secret, &tag)
    );
}

#[test]
fn pairing_session_expires_and_is_consumed_on_first_success() {
    let now = Instant::now();
    let transcript = PairingTranscript::new(&[1], &[2], [3; 32], [4; 32]);
    let mut session = PairingSession::new_at(now);
    let tag = transcript.confirm_client(session.secret());

    assert!(session.verify_client(&transcript, &tag, now + Duration::from_secs(299)));
    assert!(!session.verify_client(&transcript, &tag, now + Duration::from_secs(299)));

    let mut expired = PairingSession::new_at(now);
    let expired_tag = transcript.confirm_client(expired.secret());
    assert!(!expired.verify_client(&transcript, &expired_tag, now + Duration::from_secs(300)));
    assert_eq!(expired.secret(), &[0; 16]);
}

#[test]
fn pairing_secret_is_grouped_base32_for_display() {
    let session = PairingSession::new();
    let displayed = session.encoded_secret();
    assert!(!displayed.contains(['0', '1', '8', '9']));
    let compact = displayed.replace('-', "");
    assert_eq!(
        BASE32_NOPAD.decode(compact.as_bytes()).unwrap(),
        session.secret()
    );
}

#[tokio::test]
async fn pairing_state_drops_the_secret_at_its_deadline_without_an_attempt() {
    let session = Arc::new(std::sync::Mutex::new(Some(PairingSession::new_at(
        Instant::now() - Duration::from_millis(4 * 60 * 1000 + 59_900),
    ))));
    let _state = LanPairingState::new(session.clone(), Arc::new(|_, _| Ok(())));

    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(session.lock().unwrap().is_none());
}

#[tokio::test]
async fn old_expiry_task_does_not_remove_a_replacement_pairing_session() {
    let session = Arc::new(std::sync::Mutex::new(Some(PairingSession::new_at(
        Instant::now() - Duration::from_millis(4 * 60 * 1000 + 59_900),
    ))));
    let _state = LanPairingState::new(session.clone(), Arc::new(|_, _| Ok(())));
    *session.lock().unwrap() = Some(PairingSession::new_at(
        Instant::now() - Duration::from_millis(4 * 60 * 1000 + 59_800),
    ));

    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        session
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|session| session.is_active(Instant::now()))
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(session.lock().unwrap().is_none());
}

#[test]
fn pairing_limiter_allows_five_failures_per_source_per_minute() {
    let now = Instant::now();
    let source: IpAddr = "192.168.1.9".parse().unwrap();
    let other: IpAddr = "192.168.1.10".parse().unwrap();
    let mut limiter = PairingLimiter::default();

    for _ in 0..5 {
        assert!(limiter.record_failure(source, now).is_allowed());
    }
    assert!(limiter.record_failure(source, now).is_limited());
    assert!(limiter.record_failure(other, now).is_allowed());
    assert!(
        limiter
            .record_failure(source, now + Duration::from_secs(60))
            .is_allowed()
    );
}

struct Echo;

#[derive(Debug)]
struct TestAnyServerCertificate;

impl ServerCertVerifier for TestAnyServerCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        Err(TlsError::General("TLS 1.2 disabled".into()))
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
}

async fn open_unauthenticated_ws(
    endpoint: SocketAddr,
    path: &str,
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>,
    tokio_tungstenite::tungstenite::Error,
> {
    let config = ClientConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(TestAnyServerCertificate))
        .with_no_client_auth();
    let tcp = tokio::net::TcpStream::connect(endpoint).await.unwrap();
    let tls = TlsConnector::from(Arc::new(config))
        .connect(ServerName::try_from("comet.local").unwrap().to_owned(), tcp)
        .await
        .unwrap();
    tokio_tungstenite::client_async_with_config(format!("wss://{endpoint}{path}"), tls, None)
        .await
        .map(|(ws, _)| ws)
}

#[async_trait]
impl RpcService for Echo {
    async fn handle(&self, _method: &str, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        Ok(RpcReply::Value(params))
    }
}

fn identity() -> TlsIdentity {
    let directory = tempfile::tempdir().unwrap();
    let identity = DeviceIdentity::load_or_create(directory.path()).unwrap();
    TlsIdentity::from_device_identity(&identity).unwrap()
}

async fn spawn_server(
    identity: TlsIdentity,
    trusted: Arc<RwLock<HashSet<ServerId>>>,
    service: Arc<dyn RpcService>,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        loop {
            let (stream, peer) = listener.accept().await.unwrap();
            let identity = identity.clone();
            let trusted = trusted.clone();
            let service = service.clone();
            tokio::spawn(async move {
                let is_trusted = Arc::new(move |server_id: &ServerId| {
                    trusted.read().unwrap().contains(server_id)
                });
                let _ = accept_lan_rpc(stream, peer, &identity, is_trusted, None, service).await;
            });
        }
    });
    (endpoint, task)
}

#[tokio::test]
async fn pinned_client_rejects_changed_server_identity() {
    let server_identity = identity();
    let client_identity = identity();
    let trusted = Arc::new(RwLock::new(HashSet::from([client_identity
        .server_id()
        .clone()])));
    let (endpoint, task) = spawn_server(server_identity.clone(), trusted, Arc::new(Echo)).await;

    let endpoint_text = endpoint.to_string();
    let client = connect_lan_rpc(
        endpoint_text.as_str(),
        &client_identity,
        &server_identity.pinned_server(),
    )
    .await
    .unwrap();
    assert_eq!(
        client
            .call("Echo", serde_json::json!("hello"))
            .await
            .unwrap(),
        serde_json::json!("hello")
    );
    let wrong_pin = identity().pinned_server();
    assert!(matches!(
        connect_lan_rpc(endpoint, &client_identity, &wrong_pin).await,
        Err(LanConnectError::IdentityChanged)
    ));
    task.abort();
}

#[tokio::test]
async fn rpc_upgrade_rechecks_the_current_client_allowlist() {
    let server_identity = identity();
    let client_identity = identity();
    let trusted = Arc::new(RwLock::new(HashSet::from([client_identity
        .server_id()
        .clone()])));
    let service = Arc::new(CountingService(AtomicUsize::new(0)));
    let (endpoint, task) =
        spawn_server(server_identity.clone(), trusted.clone(), service.clone()).await;
    let pin = server_identity.pinned_server();

    let client = connect_lan_rpc(endpoint, &client_identity, &pin)
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(
            Duration::from_secs(2),
            client.call("BeforeRevoke", serde_json::Value::Null),
        )
        .await
        .unwrap()
        .is_ok()
    );
    assert_eq!(service.0.load(Ordering::Relaxed), 1);
    trusted.write().unwrap().clear();
    assert!(matches!(
        tokio::time::timeout(
            Duration::from_secs(2),
            client.call("AfterRevoke", serde_json::Value::Null),
        )
        .await,
        Ok(Err(RpcError::Closed))
    ));
    assert_eq!(service.0.load(Ordering::Relaxed), 1);
    assert!(
        connect_lan_rpc(endpoint, &client_identity, &pin)
            .await
            .is_err()
    );
    task.abort();
}

struct CountingService(AtomicUsize);

#[async_trait]
impl RpcService for CountingService {
    async fn handle(
        &self,
        _method: &str,
        _params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        self.0.fetch_add(1, Ordering::Relaxed);
        Ok(RpcReply::Value(serde_json::Value::Bool(true)))
    }
}

#[tokio::test]
async fn oversized_lan_text_frame_is_closed_before_dispatch() {
    let server_identity = identity();
    let client_identity = identity();
    let trusted = Arc::new(RwLock::new(HashSet::from([client_identity
        .server_id()
        .clone()])));
    let service = Arc::new(CountingService(AtomicUsize::new(0)));
    let (endpoint, task) = spawn_server(server_identity.clone(), trusted, service.clone()).await;
    let client = connect_lan_rpc(endpoint, &client_identity, &server_identity.pinned_server())
        .await
        .unwrap();

    let result = client
        .call(
            "TooLarge",
            serde_json::Value::String("x".repeat(comet_rpc::MAX_LAN_TEXT_FRAME_BYTES + 1)),
        )
        .await;
    assert!(matches!(result, Err(RpcError::Closed)));
    assert_eq!(service.0.load(Ordering::Relaxed), 0);
    task.abort();
}

#[tokio::test]
async fn unauthenticated_rpc_is_rejected_before_dispatch() {
    let server_identity = identity();
    let service = Arc::new(CountingService(AtomicUsize::new(0)));
    let (endpoint, task) = spawn_server(
        server_identity,
        Arc::new(RwLock::new(HashSet::new())),
        service.clone(),
    )
    .await;

    assert!(open_unauthenticated_ws(endpoint, "/rpc").await.is_err());
    assert_eq!(service.0.load(Ordering::Relaxed), 0);
    task.abort();
}

#[tokio::test]
async fn malformed_pairing_frame_is_closed_without_rpc_dispatch() {
    let server_identity = identity();
    let session = Arc::new(std::sync::Mutex::new(Some(PairingSession::new())));
    let service = Arc::new(CountingService(AtomicUsize::new(0)));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = listener.local_addr().unwrap();
    let server = server_identity.clone();
    let pairing = LanPairingState::new(session, Arc::new(|_, _| Ok(())));
    let server_service = service.clone();
    let task = tokio::spawn(async move {
        let (stream, peer) = listener.accept().await.unwrap();
        let _ = accept_lan_rpc(
            stream,
            peer,
            &server,
            Arc::new(|_| false),
            Some(pairing),
            server_service,
        )
        .await;
    });
    let mut ws = open_unauthenticated_ws(endpoint, "/pair").await.unwrap();
    assert!(matches!(ws.next().await, Some(Ok(WsMessage::Text(_)))));
    ws.send(WsMessage::Text("{".into())).await.unwrap();
    assert!(!matches!(ws.next().await, Some(Ok(WsMessage::Text(_)))));
    assert_eq!(service.0.load(Ordering::Relaxed), 0);
    task.abort();
}

#[tokio::test]
async fn standalone_inactive_pairing_is_rejected_before_upgrade() {
    let server_identity = identity();
    let now = Instant::now();
    let session = Arc::new(std::sync::Mutex::new(Some(PairingSession::new_at(now))));
    let transcript = PairingTranscript::new(&[1], &[2], [3; 32], [4; 32]);
    let tag = transcript.confirm_client(session.lock().unwrap().as_ref().unwrap().secret());
    assert!(!session.lock().unwrap().as_mut().unwrap().verify_client(
        &transcript,
        &tag,
        now + Duration::from_secs(300),
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (stream, peer) = listener.accept().await.unwrap();
        let _ = serve_pairing(
            stream,
            peer,
            &server_identity,
            session,
            Arc::new(|_, _| Ok(())),
        )
        .await;
    });

    assert!(open_unauthenticated_ws(endpoint, "/pair").await.is_err());
    task.abort();
}

#[tokio::test]
async fn pairing_exchange_pins_both_identities_and_consumes_the_session() {
    let server_identity = identity();
    let client_identity = identity();
    let session = Arc::new(std::sync::Mutex::new(Some(PairingSession::new())));
    let secret = *session.lock().unwrap().as_ref().unwrap().secret();
    let trusted = Arc::new(RwLock::new(HashSet::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = listener.local_addr().unwrap();
    let server = server_identity.clone();
    let server_session = session.clone();
    let server_trusted = trusted.clone();
    let task = tokio::spawn(async move {
        let pairing = LanPairingState::new(
            server_session,
            Arc::new(move |server_id, _certificate| {
                server_trusted.write().unwrap().insert(server_id);
                Ok(())
            }),
        );
        loop {
            let (stream, peer) = listener.accept().await.unwrap();
            let identity = server.clone();
            let pairing = pairing.clone();
            tokio::spawn(async move {
                let deny_rpc = Arc::new(|_: &ServerId| false);
                let _ = accept_lan_rpc(
                    stream,
                    peer,
                    &identity,
                    deny_rpc,
                    Some(pairing),
                    Arc::new(Echo),
                )
                .await;
            });
        }
    });

    let pin = pair_client(endpoint, &client_identity, secret)
        .await
        .unwrap();
    assert_eq!(pin.server_id(), server_identity.server_id());
    assert!(
        trusted
            .read()
            .unwrap()
            .contains(client_identity.server_id())
    );
    assert!(
        pair_client(endpoint, &client_identity, secret)
            .await
            .is_err()
    );
    task.abort();
}
