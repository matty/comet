use std::fmt::Debug;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use comet_identity::DeviceIdentity;
use comet_proto::ServerId;
use data_encoding::HEXLOWER;
use futures::{SinkExt, StreamExt};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, DistinguishedName, Error as TlsError,
    ServerConfig, SignatureScheme,
};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::net::{TcpStream, ToSocketAddrs};
use tokio::sync::mpsc;
use tokio_rustls::{TlsAcceptor, TlsConnector};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::http::{Response as HttpResponse, StatusCode};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use x509_parser::parse_x509_certificate;
use zeroize::Zeroizing;

use crate::server::{ConnectionGuard, serve_websocket_guarded};
use crate::{PairingAttempt, PairingSession, PairingTranscript, RpcClient, RpcService};

pub const MAX_LAN_TEXT_FRAME_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone)]
pub struct TlsIdentity {
    certificate: Vec<u8>,
    private_key: Arc<Zeroizing<Vec<u8>>>,
    fingerprint: [u8; 32],
    server_id: ServerId,
}

impl TlsIdentity {
    pub fn from_device_identity(identity: &DeviceIdentity) -> Result<Self, TlsIdentityError> {
        Self::from_der(identity.certificate_der(), identity.private_key_der())
    }

    pub fn from_der(certificate: &[u8], private_key: &[u8]) -> Result<Self, TlsIdentityError> {
        let fingerprint = certificate_fingerprint(certificate)?;
        Ok(Self {
            certificate: certificate.to_vec(),
            private_key: Arc::new(Zeroizing::new(private_key.to_vec())),
            fingerprint,
            server_id: ServerId::new(format!("sha256:{}", HEXLOWER.encode(&fingerprint))),
        })
    }

    pub fn server_id(&self) -> &ServerId {
        &self.server_id
    }

    pub fn pinned_server(&self) -> PinnedServer {
        PinnedServer {
            server_id: self.server_id.clone(),
            spki_sha256: self.fingerprint,
        }
    }

    pub fn certificate_der(&self) -> &[u8] {
        &self.certificate
    }

    fn certificate_chain(&self) -> Vec<CertificateDer<'static>> {
        vec![CertificateDer::from(self.certificate.clone())]
    }

    fn private_key(&self) -> PrivateKeyDer<'static> {
        PrivatePkcs8KeyDer::from(self.private_key.as_ref().as_slice().to_vec()).into()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TlsIdentityError {
    #[error("invalid identity certificate: {0}")]
    InvalidCertificate(String),
}

#[derive(Debug, Clone)]
pub struct PinnedServer {
    server_id: ServerId,
    spki_sha256: [u8; 32],
}

impl PinnedServer {
    pub fn server_id(&self) -> &ServerId {
        &self.server_id
    }

    pub fn spki_sha256(&self) -> &[u8; 32] {
        &self.spki_sha256
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LanConnectError {
    #[error("identity changed")]
    IdentityChanged,
    #[error("LAN transport: {0}")]
    Transport(String),
}

#[derive(Debug, thiserror::Error)]
pub enum LanAcceptError {
    #[error("LAN transport: {0}")]
    Transport(String),
}

pub type ClientAuthorizer = Arc<dyn Fn(&ServerId) -> bool + Send + Sync>;
pub type PairingAuthorizer = Arc<dyn Fn(ServerId, Vec<u8>) -> Result<(), String> + Send + Sync>;

#[derive(Clone)]
pub struct LanPairingState {
    session: Arc<std::sync::Mutex<Option<PairingSession>>>,
    on_paired: PairingAuthorizer,
}

impl LanPairingState {
    pub fn new(
        session: Arc<std::sync::Mutex<Option<PairingSession>>>,
        on_paired: PairingAuthorizer,
    ) -> Self {
        let runtime = tokio::runtime::Handle::try_current()
            .expect("LAN pairing state must be created inside a Tokio runtime");
        let weak_session = Arc::downgrade(&session);
        runtime.spawn(async move {
            loop {
                let Some(session) = weak_session.upgrade() else {
                    break;
                };
                let expiry = session
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .as_ref()
                    .map(|session| (session.expires_at(), session.generation()));
                drop(session);
                let Some((deadline, generation)) = expiry else {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    continue;
                };
                tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
                if let Some(session) = weak_session.upgrade() {
                    let mut session = session
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let now = std::time::Instant::now();
                    let same_expired_session = session.as_ref().is_some_and(|active| {
                        active.generation() == generation && now >= active.expires_at()
                    });
                    if same_expired_session {
                        if let Some(active) = session.as_mut() {
                            active.expire_if_needed(now);
                        }
                        *session = None;
                    }
                }
            }
        });
        Self { session, on_paired }
    }

    fn active_deadline(&self, now: std::time::Instant) -> Option<(std::time::Instant, u64)> {
        let mut session = self
            .session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let session = session.as_mut()?;
        session
            .expire_if_needed(now)
            .then(|| (session.expires_at(), session.generation()))
    }

    fn generation_is_active(&self, generation: u64, now: std::time::Instant) -> bool {
        self.active_deadline(now)
            .is_some_and(|(_, active_generation)| active_generation == generation)
    }
}

#[derive(Clone, Copy)]
enum LanRoute {
    Rpc,
    Pair,
}

#[derive(Debug, thiserror::Error)]
pub enum PairingError {
    #[error("pairing transport: {0}")]
    Transport(String),
    #[error("pairing was rejected")]
    Rejected,
    #[error("server pairing confirmation was invalid")]
    InvalidConfirmation,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PairingServerHello {
    server_nonce: [u8; 32],
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PairingClientHello {
    certificate: Vec<u8>,
    client_nonce: [u8; 32],
    confirmation: [u8; 32],
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PairingServerConfirmation {
    confirmation: [u8; 32],
}

pub async fn connect_lan_rpc<A>(
    endpoint: A,
    identity: &TlsIdentity,
    pin: &PinnedServer,
) -> Result<RpcClient, LanConnectError>
where
    A: ToSocketAddrs,
{
    let mismatch = Arc::new(AtomicBool::new(false));
    let verifier = Arc::new(PinnedServerVerifier {
        expected: pin.spki_sha256,
        mismatch: mismatch.clone(),
        algorithms: supported_algorithms(),
    });
    let config = ClientConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(identity.certificate_chain(), identity.private_key())
        .map_err(|error| LanConnectError::Transport(error.to_string()))?;
    let tcp = TcpStream::connect(endpoint)
        .await
        .map_err(|error| LanConnectError::Transport(error.to_string()))?;
    let endpoint = tcp
        .peer_addr()
        .map_err(|error| LanConnectError::Transport(error.to_string()))?;
    let server_name = ServerName::try_from("comet.local")
        .expect("static server name is valid")
        .to_owned();
    let tls = match TlsConnector::from(Arc::new(config))
        .connect(server_name, tcp)
        .await
    {
        Ok(tls) => tls,
        Err(_) if mismatch.load(Ordering::Relaxed) => return Err(LanConnectError::IdentityChanged),
        Err(error) => return Err(LanConnectError::Transport(error.to_string())),
    };
    let url = format!("wss://{endpoint}/rpc");
    let config = lan_websocket_config();
    let (ws, _) = tokio_tungstenite::client_async_with_config(url, tls, Some(config))
        .await
        .map_err(|error| LanConnectError::Transport(error.to_string()))?;
    Ok(client_from_lan_websocket(ws))
}

fn client_from_lan_websocket<S>(ws: tokio_tungstenite::WebSocketStream<S>) -> RpcClient
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut sink, mut stream) = ws.split();
    let (out_tx, mut out_rx) = mpsc::channel::<String>(256);
    let (in_tx, in_rx) = mpsc::channel::<String>(256);
    tokio::spawn(async move {
        loop {
            tokio::select! {
                frame = out_rx.recv() => match frame {
                    Some(text) => {
                        if sink.send(WsMessage::Text(text)).await.is_err() {
                            break;
                        }
                    }
                    None => {
                        let _ = sink.send(WsMessage::Close(None)).await;
                        break;
                    }
                },
                message = stream.next() => match message {
                    Some(Ok(WsMessage::Text(text))) => {
                        if in_tx.send(text).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(WsMessage::Close(_))) | Some(Err(_)) | None => break,
                    Some(Ok(_)) => {}
                },
            }
        }
    });
    RpcClient::new(out_tx, in_rx)
}

#[allow(clippy::result_large_err)] // tungstenite's handshake callback fixes this response type.
pub async fn accept_lan_rpc(
    stream: TcpStream,
    peer: SocketAddr,
    identity: &TlsIdentity,
    authorizer: ClientAuthorizer,
    pairing: Option<LanPairingState>,
    service: Arc<dyn RpcService>,
) -> Result<(), LanAcceptError> {
    let config = optional_client_server_config(identity)
        .map_err(|error| LanAcceptError::Transport(error.to_string()))?;
    let tls = TlsAcceptor::from(Arc::new(config))
        .accept(stream)
        .await
        .map_err(|error| LanAcceptError::Transport(error.to_string()))?;
    let client_id = tls
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|certificates| certificates.first())
        .and_then(|certificate| certificate_fingerprint(certificate.as_ref()).ok())
        .map(|fingerprint| ServerId::new(format!("sha256:{}", HEXLOWER.encode(&fingerprint))));
    let selected_route = Arc::new(std::sync::Mutex::new(None));
    let rpc_guard = client_id.clone().map(|client_id| {
        let authorizer = authorizer.clone();
        Arc::new(move || authorizer(&client_id)) as ConnectionGuard
    });
    let callback_route = selected_route.clone();
    let callback_pairing = pairing.clone();
    let callback = move |request: &Request, response: Response| {
        if request.uri().path() == "/rpc"
            && client_id
                .as_ref()
                .is_some_and(|client_id| authorizer(client_id))
        {
            tracing::info!(%peer, "lan rpc: authenticated websocket upgrade");
            *callback_route
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(LanRoute::Rpc);
            return Ok(response);
        }
        if request.uri().path() == "/pair"
            && client_id.is_none()
            && callback_pairing.as_ref().is_some_and(|pairing| {
                pairing
                    .session
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .as_mut()
                    .is_some_and(|session| session.expire_if_needed(std::time::Instant::now()))
            })
        {
            *callback_route
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(LanRoute::Pair);
            return Ok(response);
        }
        tracing::warn!(%peer, "lan rpc: rejected websocket upgrade");
        Err(HttpResponse::builder()
            .status(StatusCode::FORBIDDEN)
            .body(Some("forbidden".into()))
            .expect("static rejection response"))
    };
    let ws = tokio_tungstenite::accept_hdr_async_with_config(
        tls,
        callback,
        Some(lan_websocket_config()),
    )
    .await
    .map_err(|error| LanAcceptError::Transport(error.to_string()))?;
    let route = *selected_route
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match route {
        Some(LanRoute::Rpc) => {
            serve_websocket_guarded(ws, service, rpc_guard).await;
            Ok(())
        }
        Some(LanRoute::Pair) => {
            let pairing = pairing.expect("pair route requires pairing state");
            serve_pairing_websocket(ws, peer, identity, pairing)
                .await
                .map_err(|error| LanAcceptError::Transport(error.to_string()))
        }
        None => Err(LanAcceptError::Transport("no LAN route selected".into())),
    }
}

pub async fn pair_client<A>(
    endpoint: A,
    identity: &TlsIdentity,
    secret: [u8; 16],
) -> Result<PinnedServer, PairingError>
where
    A: ToSocketAddrs,
{
    let secret = Zeroizing::new(secret);
    let verifier = Arc::new(UnpinnedPairingVerifier {
        algorithms: supported_algorithms(),
    });
    let config = ClientConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    let tcp = TcpStream::connect(endpoint)
        .await
        .map_err(|error| PairingError::Transport(error.to_string()))?;
    let endpoint = tcp
        .peer_addr()
        .map_err(|error| PairingError::Transport(error.to_string()))?;
    let tls = TlsConnector::from(Arc::new(config))
        .connect(
            ServerName::try_from("comet.local")
                .expect("static server name is valid")
                .to_owned(),
            tcp,
        )
        .await
        .map_err(|error| PairingError::Transport(error.to_string()))?;
    let server_certificate = tls
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|certificates| certificates.first())
        .ok_or_else(|| PairingError::Transport("server sent no certificate".into()))?
        .as_ref()
        .to_vec();
    let server_fingerprint = certificate_fingerprint(&server_certificate)
        .map_err(|error| PairingError::Transport(error.to_string()))?;
    let url = format!("wss://{endpoint}/pair");
    let (mut ws, _) =
        tokio_tungstenite::client_async_with_config(url, tls, Some(lan_websocket_config()))
            .await
            .map_err(|error| PairingError::Transport(error.to_string()))?;
    let server_hello: PairingServerHello = receive_pairing_json(&mut ws).await?;
    let client_nonce = rand::random();
    let transcript = PairingTranscript::new(
        &server_fingerprint,
        &identity.fingerprint,
        server_hello.server_nonce,
        client_nonce,
    );
    let hello = PairingClientHello {
        certificate: identity.certificate.clone(),
        client_nonce,
        confirmation: transcript.confirm_client(&secret),
    };
    send_pairing_json(&mut ws, &hello).await?;
    let response: PairingServerConfirmation = receive_pairing_json(&mut ws).await?;
    if !transcript.verify_server(&secret, &response.confirmation) {
        return Err(PairingError::InvalidConfirmation);
    }
    Ok(PinnedServer {
        server_id: ServerId::new(format!("sha256:{}", HEXLOWER.encode(&server_fingerprint))),
        spki_sha256: server_fingerprint,
    })
}

#[allow(clippy::result_large_err)] // tungstenite's handshake callback fixes this response type.
pub async fn serve_pairing(
    stream: TcpStream,
    peer: SocketAddr,
    identity: &TlsIdentity,
    pairing: LanPairingState,
) -> Result<(), PairingError> {
    let config = optional_client_server_config(identity)
        .map_err(|error| PairingError::Transport(error.to_string()))?;
    let tls = TlsAcceptor::from(Arc::new(config))
        .accept(stream)
        .await
        .map_err(|error| PairingError::Transport(error.to_string()))?;
    let client_certificate_absent = tls.get_ref().1.peer_certificates().is_none();
    let callback_pairing = pairing.clone();
    let callback = move |request: &Request, response: Response| {
        let pairing_active = callback_pairing
            .active_deadline(std::time::Instant::now())
            .is_some();
        if request.uri().path() == "/pair" && client_certificate_absent && pairing_active {
            Ok(response)
        } else {
            Err(HttpResponse::builder()
                .status(StatusCode::FORBIDDEN)
                .body(Some("forbidden".into()))
                .expect("static rejection response"))
        }
    };
    let ws = tokio_tungstenite::accept_hdr_async_with_config(
        tls,
        callback,
        Some(lan_websocket_config()),
    )
    .await
    .map_err(|error| PairingError::Transport(error.to_string()))?;
    serve_pairing_websocket(ws, peer, identity, pairing).await
}

async fn serve_pairing_websocket<S>(
    mut ws: tokio_tungstenite::WebSocketStream<S>,
    peer: SocketAddr,
    identity: &TlsIdentity,
    pairing: LanPairingState,
) -> Result<(), PairingError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let Some((deadline, generation)) = pairing.active_deadline(std::time::Instant::now()) else {
        tracing::warn!(%peer, "lan pairing: rejected inactive session");
        let _ = ws.close(None).await;
        return Err(PairingError::Rejected);
    };
    let server_nonce = rand::random();
    send_pairing_json(&mut ws, &PairingServerHello { server_nonce }).await?;
    let hello_result = {
        let receive = receive_pairing_json(&mut ws);
        futures::pin_mut!(receive);
        let deadline = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline));
        tokio::pin!(deadline);
        let mut active_check = tokio::time::interval(std::time::Duration::from_millis(100));
        loop {
            tokio::select! {
                result = &mut receive => break result,
                _ = &mut deadline => break Err(PairingError::Rejected),
                _ = active_check.tick() => {
                    if !pairing.generation_is_active(generation, std::time::Instant::now()) {
                        break Err(PairingError::Rejected);
                    }
                }
            }
        }
    };
    let hello: PairingClientHello = match hello_result {
        Ok(hello) => hello,
        Err(error) => {
            tracing::warn!(%peer, "lan pairing: rejected malformed frame");
            let _ = ws.close(None).await;
            return Err(error);
        }
    };
    let client_fingerprint =
        certificate_fingerprint(&hello.certificate).map_err(|_| PairingError::Rejected)?;
    let transcript = PairingTranscript::new(
        &identity.fingerprint,
        &client_fingerprint,
        server_nonce,
        hello.client_nonce,
    );
    let attempt = pairing
        .session
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_mut()
        .map(|session| {
            session.verify_from(
                peer.ip(),
                &transcript,
                &hello.confirmation,
                std::time::Instant::now(),
            )
        })
        .unwrap_or(PairingAttempt::Inactive);
    let PairingAttempt::Accepted(server_confirmation) = attempt else {
        tracing::warn!(%peer, outcome = ?attempt, "lan pairing: confirmation rejected");
        let _ = ws.close(None).await;
        return Err(PairingError::Rejected);
    };
    let client_id = ServerId::new(format!("sha256:{}", HEXLOWER.encode(&client_fingerprint)));
    if let Err(error) = (pairing.on_paired)(client_id, hello.certificate) {
        tracing::warn!(%peer, "lan pairing: trust persistence failed");
        let _ = ws.close(None).await;
        return Err(PairingError::Transport(error));
    }
    send_pairing_json(
        &mut ws,
        &PairingServerConfirmation {
            confirmation: server_confirmation,
        },
    )
    .await?;
    tracing::info!(%peer, "lan pairing: paired client");
    let _ = ws.close(None).await;
    Ok(())
}

async fn send_pairing_json<S, T>(
    ws: &mut tokio_tungstenite::WebSocketStream<S>,
    value: &T,
) -> Result<(), PairingError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    T: serde::Serialize,
{
    let text =
        serde_json::to_string(value).map_err(|error| PairingError::Transport(error.to_string()))?;
    ws.send(WsMessage::Text(text))
        .await
        .map_err(|error| PairingError::Transport(error.to_string()))
}

async fn receive_pairing_json<S, T>(
    ws: &mut tokio_tungstenite::WebSocketStream<S>,
) -> Result<T, PairingError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    T: serde::de::DeserializeOwned,
{
    match ws.next().await {
        Some(Ok(WsMessage::Text(text))) => {
            serde_json::from_str(&text).map_err(|_| PairingError::Rejected)
        }
        Some(Ok(_)) => Err(PairingError::Rejected),
        Some(Err(error)) => Err(PairingError::Transport(error.to_string())),
        None => Err(PairingError::Rejected),
    }
}

fn lan_websocket_config() -> WebSocketConfig {
    WebSocketConfig {
        max_message_size: Some(MAX_LAN_TEXT_FRAME_BYTES),
        max_frame_size: Some(MAX_LAN_TEXT_FRAME_BYTES),
        ..Default::default()
    }
}

fn optional_client_server_config(identity: &TlsIdentity) -> Result<ServerConfig, TlsError> {
    ServerConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
        .with_client_cert_verifier(Arc::new(OptionalClientVerifier {
            hints: Vec::new(),
            algorithms: supported_algorithms(),
        }))
        .with_single_cert(identity.certificate_chain(), identity.private_key())
}

fn certificate_fingerprint(certificate_der: &[u8]) -> Result<[u8; 32], TlsIdentityError> {
    let (remainder, certificate) = parse_x509_certificate(certificate_der)
        .map_err(|error| TlsIdentityError::InvalidCertificate(error.to_string()))?;
    if !remainder.is_empty() {
        return Err(TlsIdentityError::InvalidCertificate(
            "trailing certificate data".into(),
        ));
    }
    Ok(Sha256::digest(certificate.public_key().raw).into())
}

fn supported_algorithms() -> WebPkiSupportedAlgorithms {
    CryptoProvider::get_default()
        .cloned()
        .unwrap_or_else(|| Arc::new(rustls::crypto::aws_lc_rs::default_provider()))
        .signature_verification_algorithms
}

fn verify_signature(
    message: &[u8],
    cert: &CertificateDer<'_>,
    dss: &DigitallySignedStruct,
    algorithms: &WebPkiSupportedAlgorithms,
) -> Result<HandshakeSignatureValid, TlsError> {
    verify_tls13_signature(message, cert, dss, algorithms)
}

fn tls13_supported_schemes(algorithms: &WebPkiSupportedAlgorithms) -> Vec<SignatureScheme> {
    algorithms
        .supported_schemes()
        .into_iter()
        .filter(|scheme| {
            !matches!(
                scheme,
                SignatureScheme::RSA_PKCS1_SHA1
                    | SignatureScheme::RSA_PKCS1_SHA256
                    | SignatureScheme::RSA_PKCS1_SHA384
                    | SignatureScheme::RSA_PKCS1_SHA512
                    | SignatureScheme::ECDSA_SHA1_Legacy
            )
        })
        .collect()
}

#[derive(Debug)]
struct PinnedServerVerifier {
    expected: [u8; 32],
    mismatch: Arc<AtomicBool>,
    algorithms: WebPkiSupportedAlgorithms,
}

#[derive(Debug)]
struct UnpinnedPairingVerifier {
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for UnpinnedPairingVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        certificate_fingerprint(end_entity.as_ref())
            .map_err(|_| TlsError::InvalidCertificate(CertificateError::BadEncoding))?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        Err(TlsError::General("TLS 1.2 is disabled".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        tls13_supported_schemes(&self.algorithms)
    }
}

impl ServerCertVerifier for PinnedServerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        let actual = certificate_fingerprint(end_entity.as_ref())
            .map_err(|_| TlsError::InvalidCertificate(CertificateError::BadEncoding))?;
        if !bool::from(actual.ct_eq(&self.expected)) {
            self.mismatch.store(true, Ordering::Relaxed);
            return Err(TlsError::InvalidCertificate(
                CertificateError::ApplicationVerificationFailure,
            ));
        }
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        Err(TlsError::General("TLS 1.2 is disabled".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        tls13_supported_schemes(&self.algorithms)
    }
}

#[derive(Debug)]
struct OptionalClientVerifier {
    hints: Vec<DistinguishedName>,
    algorithms: WebPkiSupportedAlgorithms,
}

impl ClientCertVerifier for OptionalClientVerifier {
    fn client_auth_mandatory(&self) -> bool {
        false
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &self.hints
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, TlsError> {
        certificate_fingerprint(end_entity.as_ref())
            .map_err(|_| TlsError::InvalidCertificate(CertificateError::BadEncoding))?;
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        Err(TlsError::General("TLS 1.2 is disabled".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        tls13_supported_schemes(&self.algorithms)
    }
}

#[cfg(test)]
mod tls13_scheme_tests {
    use super::*;

    #[test]
    fn tls13_verifiers_do_not_advertise_rsa_pkcs1_schemes() {
        let verifier = UnpinnedPairingVerifier {
            algorithms: supported_algorithms(),
        };
        let schemes = verifier.supported_verify_schemes();

        assert!(!schemes.contains(&SignatureScheme::RSA_PKCS1_SHA1));
        assert!(!schemes.contains(&SignatureScheme::RSA_PKCS1_SHA256));
        assert!(!schemes.contains(&SignatureScheme::RSA_PKCS1_SHA384));
        assert!(!schemes.contains(&SignatureScheme::RSA_PKCS1_SHA512));
        assert!(!schemes.contains(&SignatureScheme::ECDSA_SHA1_Legacy));
        assert!(schemes.contains(&SignatureScheme::ED25519));
    }
}
