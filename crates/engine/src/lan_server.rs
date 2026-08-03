use std::collections::HashSet;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use chrono::{DateTime, Utc};
use comet_identity::DeviceIdentity;
use comet_proto::{LanSettings, ServerId, TrustedClient};
use comet_rpc::{
    ClientAuthorizer, LanPairingState, PairingAuthorizer, PairingSession, RpcService, TlsIdentity,
    accept_lan_rpc,
};
use serde::Serialize;
use tokio::sync::watch;

use crate::{EngineError, RemoteConfigStore};

const PAIRING_LIFETIME: chrono::Duration = chrono::Duration::minutes(5);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", tag = "state")]
pub enum LanServerStatus {
    Disabled,
    Listening {
        bind: std::net::SocketAddr,
    },
    BindFailed {
        bind: std::net::SocketAddr,
        error: String,
    },
}

#[derive(Clone)]
pub struct LanServerHandle {
    store: RemoteConfigStore,
    identity: Arc<DeviceIdentity>,
    status_tx: watch::Sender<LanServerStatus>,
    pairing: Arc<Mutex<Option<PairingSession>>>,
    explicitly_revoked: Arc<Mutex<HashSet<ServerId>>>,
    started: Arc<std::sync::atomic::AtomicBool>,
    shutdown_tx: watch::Sender<bool>,
    stopped_tx: watch::Sender<bool>,
}

impl LanServerHandle {
    pub(crate) fn new(store: RemoteConfigStore, identity: Arc<DeviceIdentity>) -> Self {
        let (status_tx, _) = watch::channel(LanServerStatus::Disabled);
        let (shutdown_tx, _) = watch::channel(false);
        let (stopped_tx, _) = watch::channel(true);
        Self {
            store,
            identity,
            status_tx,
            pairing: Arc::new(Mutex::new(None)),
            explicitly_revoked: Arc::new(Mutex::new(HashSet::new())),
            started: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            shutdown_tx,
            stopped_tx,
        }
    }

    pub fn status(&self) -> LanServerStatus {
        self.status_tx.borrow().clone()
    }

    pub fn watch_status(&self) -> watch::Receiver<LanServerStatus> {
        self.status_tx.subscribe()
    }

    pub fn apply_settings(&self, settings: LanSettings) -> Result<(), EngineError> {
        self.store.set_lan_settings(settings)
    }

    pub fn begin_pairing(&self) -> (String, DateTime<Utc>) {
        let session = PairingSession::new();
        let secret = session.encoded_secret();
        let expires_at = Utc::now() + PAIRING_LIFETIME;
        *self.pairing.lock().unwrap_or_else(PoisonError::into_inner) = Some(session);
        (secret, expires_at)
    }

    pub fn close_client(&self, server_id: &ServerId) {
        self.explicitly_revoked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(server_id.clone());
    }

    pub(crate) fn ensure_started(&self, service: Arc<dyn RpcService>) {
        use std::sync::atomic::Ordering;
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        if self.started.swap(true, Ordering::AcqRel) {
            return;
        }
        self.stopped_tx.send_replace(false);
        LanServer::spawn(self.clone(), service);
    }

    pub async fn shutdown(&self) {
        if !self.started.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        let mut stopped = self.stopped_tx.subscribe();
        self.shutdown_tx.send_replace(true);
        let wait = async {
            loop {
                let is_stopped = *stopped.borrow_and_update();
                if is_stopped {
                    break;
                }
                if stopped.changed().await.is_err() {
                    break;
                }
            }
        };
        let _ = tokio::time::timeout(CLEANUP_TIMEOUT + CLEANUP_TIMEOUT, wait).await;
    }
}

pub struct LanServer;

impl LanServer {
    pub fn spawn(handle: LanServerHandle, service: Arc<dyn RpcService>) {
        tokio::spawn(async move {
            let tls = match TlsIdentity::from_device_identity(&handle.identity) {
                Ok(tls) => tls,
                Err(error) => {
                    tracing::error!(error = %error, "LAN identity unavailable");
                    return;
                }
            };
            let trusted = handle.store.watch_trusted_clients();
            let revoked = handle.explicitly_revoked.clone();
            let authorizer: ClientAuthorizer = Arc::new(move |server_id| {
                !revoked
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .contains(server_id)
                    && trusted
                        .borrow()
                        .iter()
                        .any(|client| &client.server_id == server_id)
            });
            let paired_store = handle.store.clone();
            let paired_revoked = handle.explicitly_revoked.clone();
            let on_paired: PairingAuthorizer = Arc::new(move |server_id, _certificate| {
                paired_revoked
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&server_id);
                let pin = serde_json::to_value(&server_id)
                    .ok()
                    .and_then(|value| value.as_str().map(ToOwned::to_owned))
                    .unwrap_or_default();
                paired_store
                    .trust_client(TrustedClient {
                        server_id,
                        name: "Paired client".into(),
                        pinned_spki_sha256: pin,
                        paired_at: Utc::now(),
                    })
                    .map_err(|error| error.to_string())
            });
            let pairing = LanPairingState::new(handle.pairing.clone(), on_paired);
            supervise(
                handle.store.watch_lan_settings(),
                handle.shutdown_tx.subscribe(),
                handle.status_tx.clone(),
                tls,
                authorizer,
                pairing,
                service,
            )
            .await;
            handle.status_tx.send_replace(LanServerStatus::Disabled);
            handle.stopped_tx.send_replace(true);
        });
    }
}

async fn supervise(
    mut settings_rx: watch::Receiver<LanSettings>,
    mut shutdown_rx: watch::Receiver<bool>,
    status_tx: watch::Sender<LanServerStatus>,
    identity: TlsIdentity,
    authorizer: ClientAuthorizer,
    pairing: LanPairingState,
    service: Arc<dyn RpcService>,
) {
    let mut accept_task: Option<tokio::task::JoinHandle<()>> = None;
    loop {
        if *shutdown_rx.borrow() {
            break;
        }
        if let Some(task) = accept_task.take() {
            task.abort();
            let _ = tokio::time::timeout(CLEANUP_TIMEOUT, task).await;
        }
        let settings = settings_rx.borrow().clone();
        if !settings.enabled {
            status_tx.send_replace(LanServerStatus::Disabled);
        } else {
            match tokio::net::TcpListener::bind(settings.bind).await {
                Ok(listener) => {
                    let bind = listener.local_addr().unwrap_or(settings.bind);
                    status_tx.send_replace(LanServerStatus::Listening { bind });
                    let identity = identity.clone();
                    let authorizer = authorizer.clone();
                    let pairing = pairing.clone();
                    let service = service.clone();
                    accept_task = Some(tokio::spawn(async move {
                        let mut connections = tokio::task::JoinSet::new();
                        loop {
                            tokio::select! {
                                accepted = listener.accept() => match accepted {
                                    Ok((stream, peer)) => {
                                        let identity = identity.clone();
                                        let authorizer = authorizer.clone();
                                        let pairing = pairing.clone();
                                        let service = service.clone();
                                        connections.spawn(async move {
                                            if let Err(error) = accept_lan_rpc(
                                                stream,
                                                peer,
                                                &identity,
                                                authorizer,
                                                Some(pairing),
                                                service,
                                            ).await {
                                                tracing::debug!(%peer, error = %error, "LAN connection ended");
                                            }
                                        });
                                    }
                                    Err(error) => {
                                        tracing::warn!(error = %error, "LAN accept failed");
                                        tokio::time::sleep(Duration::from_millis(100)).await;
                                    }
                                },
                                Some(_) = connections.join_next(), if !connections.is_empty() => {}
                            }
                        }
                    }));
                }
                Err(error) => {
                    tracing::warn!(bind = %settings.bind, error = %error, "LAN listener bind failed");
                    status_tx.send_replace(LanServerStatus::BindFailed {
                        bind: settings.bind,
                        error: error.to_string(),
                    });
                }
            }
        }
        tokio::select! {
            changed = settings_rx.changed() => {
                if changed.is_err() {
                    break;
                }
            }
            changed = shutdown_rx.changed() => {
                if changed.is_err() || *shutdown_rx.borrow_and_update() {
                    break;
                }
            }
        }
    }
    if let Some(task) = accept_task {
        task.abort();
        let _ = tokio::time::timeout(CLEANUP_TIMEOUT, task).await;
    }
}
