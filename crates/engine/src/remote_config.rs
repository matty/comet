use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use comet_proto::{LanSettings, RemoteConnectionState, RemoteEntry, ServerId, TrustedClient};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::EngineError;

const CONFIG_VERSION: u32 = 1;
const CONFIG_FILE: &str = "remote-access.json";

#[derive(Debug, thiserror::Error)]
enum PersistError {
    #[error("serialization failed: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("atomic write failed: {0}")]
    Atomic(#[from] comet_identity::AtomicWriteError),
}

impl PersistError {
    fn committed(&self) -> bool {
        matches!(self, Self::Atomic(error) if error.committed())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteConfigDocument {
    version: u32,
    lan_settings: LanSettings,
    remotes: Vec<RemoteEntry>,
    trusted_clients: Vec<TrustedClient>,
}

impl Default for RemoteConfigDocument {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            lan_settings: LanSettings {
                enabled: false,
                bind: "0.0.0.0:27655"
                    .parse()
                    .expect("the default LAN address is valid"),
            },
            remotes: Vec::new(),
            trusted_clients: Vec::new(),
        }
    }
}

struct RemoteConfigInner {
    path: PathBuf,
    document: Mutex<RemoteConfigDocument>,
    lan_settings_tx: watch::Sender<LanSettings>,
    remotes_tx: watch::Sender<Vec<RemoteEntry>>,
    trusted_clients_tx: watch::Sender<Vec<TrustedClient>>,
}

#[derive(Clone)]
pub struct RemoteConfigStore {
    inner: Arc<RemoteConfigInner>,
}

impl RemoteConfigStore {
    pub fn open(data_dir: &Path) -> Result<Self, EngineError> {
        std::fs::create_dir_all(data_dir)?;
        let path = data_dir.join(CONFIG_FILE);
        let document = match comet_identity::read_private_file(&path) {
            Ok(bytes) => serde_json::from_slice::<RemoteConfigDocument>(&bytes)
                .map_err(|error| EngineError::Other(format!("remote config: {error}")))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                RemoteConfigDocument::default()
            }
            Err(error) => return Err(error.into()),
        };
        if document.version != CONFIG_VERSION {
            return Err(EngineError::Other(format!(
                "unsupported remote config version {}",
                document.version
            )));
        }
        let (lan_settings_tx, _) = watch::channel(document.lan_settings.clone());
        let (remotes_tx, _) = watch::channel(document.remotes.clone());
        let (trusted_clients_tx, _) = watch::channel(document.trusted_clients.clone());
        Ok(Self {
            inner: Arc::new(RemoteConfigInner {
                path,
                document: Mutex::new(document),
                lan_settings_tx,
                remotes_tx,
                trusted_clients_tx,
            }),
        })
    }

    pub fn lan_settings(&self) -> LanSettings {
        self.inner
            .document
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .lan_settings
            .clone()
    }

    pub fn watch_lan_settings(&self) -> watch::Receiver<LanSettings> {
        self.inner.lan_settings_tx.subscribe()
    }

    pub fn watch_remotes(&self) -> watch::Receiver<Vec<RemoteEntry>> {
        self.inner.remotes_tx.subscribe()
    }

    pub fn watch_trusted_clients(&self) -> watch::Receiver<Vec<TrustedClient>> {
        self.inner.trusted_clients_tx.subscribe()
    }

    pub fn set_lan_settings(&self, settings: LanSettings) -> Result<(), EngineError> {
        self.update(
            |document| document.lan_settings = settings,
            |document| {
                self.inner
                    .lan_settings_tx
                    .send_replace(document.lan_settings.clone());
            },
        )
    }

    pub fn put_remote(&self, remote: RemoteEntry) -> Result<(), EngineError> {
        self.update(
            |document| {
                if let Some(existing) = document
                    .remotes
                    .iter_mut()
                    .find(|entry| entry.server_id == remote.server_id)
                {
                    *existing = remote;
                } else {
                    document.remotes.push(remote);
                }
            },
            |document| {
                self.inner.remotes_tx.send_replace(document.remotes.clone());
            },
        )
    }

    pub fn remove_remote(&self, server_id: &ServerId) -> Result<bool, EngineError> {
        let mut removed = false;
        self.update(
            |document| {
                let original_len = document.remotes.len();
                document
                    .remotes
                    .retain(|entry| &entry.server_id != server_id);
                removed = document.remotes.len() != original_len;
            },
            |document| {
                self.inner.remotes_tx.send_replace(document.remotes.clone());
            },
        )?;
        Ok(removed)
    }

    pub fn update_remote_status(
        &self,
        server_id: &ServerId,
        last_state: RemoteConnectionState,
        protocol_version: u32,
        last_connected_at: Option<DateTime<Utc>>,
    ) -> Result<bool, EngineError> {
        let mut found = false;
        self.update(
            |document| {
                if let Some(remote) = document
                    .remotes
                    .iter_mut()
                    .find(|remote| &remote.server_id == server_id)
                {
                    remote.last_state = last_state;
                    remote.protocol_version = protocol_version;
                    remote.last_connected_at = last_connected_at;
                    found = true;
                }
            },
            |document| {
                self.inner.remotes_tx.send_replace(document.remotes.clone());
            },
        )?;
        Ok(found)
    }

    pub fn trust_client(&self, client: TrustedClient) -> Result<(), EngineError> {
        self.update(
            |document| {
                if let Some(existing) = document
                    .trusted_clients
                    .iter_mut()
                    .find(|entry| entry.server_id == client.server_id)
                {
                    *existing = client;
                } else {
                    document.trusted_clients.push(client);
                }
            },
            |document| {
                self.inner
                    .trusted_clients_tx
                    .send_replace(document.trusted_clients.clone());
            },
        )
    }

    pub fn revoke_client(&self, server_id: &ServerId) -> Result<bool, EngineError> {
        let mut removed = false;
        self.update(
            |document| {
                let original_len = document.trusted_clients.len();
                document
                    .trusted_clients
                    .retain(|entry| &entry.server_id != server_id);
                removed = document.trusted_clients.len() != original_len;
            },
            |document| {
                self.inner
                    .trusted_clients_tx
                    .send_replace(document.trusted_clients.clone());
            },
        )?;
        Ok(removed)
    }

    fn update(
        &self,
        mutate: impl FnOnce(&mut RemoteConfigDocument),
        publish: impl FnOnce(&RemoteConfigDocument),
    ) -> Result<(), EngineError> {
        let mut current = self
            .inner
            .document
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut next = current.clone();
        mutate(&mut next);
        let committed_error = match persist_atomic(&self.inner.path, &next) {
            Ok(()) => None,
            Err(error) if error.committed() => Some(error),
            Err(error) => {
                return Err(EngineError::Other(format!(
                    "remote config atomic write: {error}"
                )));
            }
        };
        *current = next;
        publish(&current);
        match committed_error {
            Some(error) => Err(EngineError::Other(format!(
                "remote config replaced but directory sync failed: {error}"
            ))),
            None => Ok(()),
        }
    }
}

fn persist_atomic(path: &Path, document: &RemoteConfigDocument) -> Result<(), PersistError> {
    let mut bytes = serde_json::to_vec_pretty(document)?;
    bytes.push(b'\n');
    comet_identity::write_private_file_atomic(path, &bytes)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use comet_proto::{
        LanSettings, PROTOCOL_VERSION, RemoteConnectionState, RemoteEndpoint, RemoteEntry,
        ServerId, TrustedClient,
    };

    #[test]
    fn listening_defaults_off_and_writes_are_atomic() {
        let dir = tempfile::tempdir().unwrap();
        let store = RemoteConfigStore::open(dir.path()).unwrap();
        assert!(!store.lan_settings().enabled);
        store
            .set_lan_settings(LanSettings {
                enabled: true,
                bind: "0.0.0.0:27655".parse().unwrap(),
            })
            .unwrap();
        assert!(
            RemoteConfigStore::open(dir.path())
                .unwrap()
                .lan_settings()
                .enabled
        );
        assert!(!dir.path().join("remote-access.json.tmp").exists());
    }

    #[test]
    fn mutations_persist_and_publish_full_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let store = RemoteConfigStore::open(dir.path()).unwrap();
        let remotes = store.watch_remotes();
        let clients = store.watch_trusted_clients();
        let remote = RemoteEntry {
            server_id: ServerId::new("sha256:remote"),
            endpoint: RemoteEndpoint::parse("buildbox.local:27655").unwrap(),
            name: "Build box".into(),
            pinned_spki_sha256: "remote-pin".into(),
            protocol_version: PROTOCOL_VERSION,
            last_state: RemoteConnectionState::Offline,
            created_at: Utc::now(),
            last_connected_at: None,
        };
        let client = TrustedClient {
            server_id: ServerId::new("sha256:client"),
            name: "Laptop".into(),
            pinned_spki_sha256: "client-pin".into(),
            paired_at: Utc::now(),
        };

        store.put_remote(remote.clone()).unwrap();
        store.trust_client(client.clone()).unwrap();
        assert_eq!(remotes.borrow().as_slice(), &[remote.clone()]);
        assert_eq!(clients.borrow().as_slice(), &[client.clone()]);

        let reopened = RemoteConfigStore::open(dir.path()).unwrap();
        assert_eq!(reopened.watch_remotes().borrow().as_slice(), &[remote]);
        assert_eq!(
            reopened.watch_trusted_clients().borrow().as_slice(),
            &[client]
        );

        assert!(
            store
                .remove_remote(&ServerId::new("sha256:remote"))
                .unwrap()
        );
        assert!(
            store
                .revoke_client(&ServerId::new("sha256:client"))
                .unwrap()
        );
        assert!(remotes.borrow().is_empty());
        assert!(clients.borrow().is_empty());
    }

    #[test]
    fn failed_replacement_does_not_publish_a_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let store = RemoteConfigStore::open(dir.path()).unwrap();
        let settings = store.watch_lan_settings();
        std::fs::create_dir(dir.path().join("remote-access.json")).unwrap();

        assert!(
            store
                .set_lan_settings(LanSettings {
                    enabled: true,
                    bind: "0.0.0.0:27655".parse().unwrap(),
                })
                .is_err()
        );
        assert!(!settings.borrow().enabled);
    }

    #[test]
    fn concurrent_writes_leave_watch_disk_and_memory_consistent() {
        let dir = tempfile::tempdir().unwrap();
        let store = RemoteConfigStore::open(dir.path()).unwrap();
        let settings = store.watch_lan_settings();
        let barrier = Arc::new(std::sync::Barrier::new(24));
        let handles: Vec<_> = (0..24)
            .map(|index| {
                let store = store.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    store
                        .set_lan_settings(LanSettings {
                            enabled: index % 2 == 0,
                            bind: format!("127.0.0.1:{}", 27655 + index).parse().unwrap(),
                        })
                        .unwrap();
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }

        let persisted = RemoteConfigStore::open(dir.path()).unwrap().lan_settings();
        assert_eq!(*settings.borrow(), store.lan_settings());
        assert_eq!(persisted, store.lan_settings());
    }

    #[test]
    fn concurrent_metadata_edit_survives_named_status_update() {
        let dir = tempfile::tempdir().unwrap();
        let store = RemoteConfigStore::open(dir.path()).unwrap();
        let original = RemoteEntry {
            server_id: ServerId::new("sha256:remote"),
            endpoint: RemoteEndpoint::parse("old.local:27655").unwrap(),
            name: "Old name".into(),
            pinned_spki_sha256: "old-pin".into(),
            protocol_version: 0,
            last_state: RemoteConnectionState::Connecting,
            created_at: Utc::now(),
            last_connected_at: None,
        };
        store.put_remote(original.clone()).unwrap();

        let start = Arc::new(std::sync::Barrier::new(3));
        let metadata_done = Arc::new(std::sync::Barrier::new(2));
        let editor = {
            let store = store.clone();
            let start = start.clone();
            let metadata_done = metadata_done.clone();
            std::thread::spawn(move || {
                start.wait();
                let mut edited = original;
                edited.endpoint = RemoteEndpoint::parse("new.local:30000").unwrap();
                edited.name = "New name".into();
                edited.pinned_spki_sha256 = "new-pin".into();
                store.put_remote(edited).unwrap();
                metadata_done.wait();
            })
        };
        let reporter = {
            let store = store.clone();
            let start = start.clone();
            let metadata_done = metadata_done.clone();
            std::thread::spawn(move || {
                start.wait();
                metadata_done.wait();
                store
                    .update_remote_status(
                        &ServerId::new("sha256:remote"),
                        RemoteConnectionState::Online,
                        PROTOCOL_VERSION,
                        Some(Utc::now()),
                    )
                    .unwrap();
            })
        };
        start.wait();
        editor.join().unwrap();
        reporter.join().unwrap();

        let row = store.watch_remotes().borrow()[0].clone();
        assert_eq!(
            row.endpoint,
            RemoteEndpoint::parse("new.local:30000").unwrap()
        );
        assert_eq!(row.name, "New name");
        assert_eq!(row.pinned_spki_sha256, "new-pin");
        assert_eq!(row.last_state, RemoteConnectionState::Online);
        assert_eq!(row.protocol_version, PROTOCOL_VERSION);
        assert!(row.last_connected_at.is_some());
    }

    #[cfg(unix)]
    #[test]
    fn remote_config_is_owner_read_write_only() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let store = RemoteConfigStore::open(dir.path()).unwrap();
        store
            .set_lan_settings(LanSettings {
                enabled: true,
                bind: "0.0.0.0:27655".parse().unwrap(),
            })
            .unwrap();
        let mode = std::fs::metadata(dir.path().join("remote-access.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[cfg(windows)]
    #[test]
    fn remote_config_has_a_protected_private_acl() {
        use std::os::windows::ffi::OsStrExt;
        use std::ptr::null_mut;
        use windows_sys::Win32::Security::{
            DACL_SECURITY_INFORMATION, GetFileSecurityW, GetSecurityDescriptorControl,
            SE_DACL_PROTECTED,
        };

        let dir = tempfile::tempdir().unwrap();
        let store = RemoteConfigStore::open(dir.path()).unwrap();
        store
            .set_lan_settings(LanSettings {
                enabled: true,
                bind: "0.0.0.0:27655".parse().unwrap(),
            })
            .unwrap();
        let path: Vec<u16> = dir
            .path()
            .join("remote-access.json")
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        let mut required = 0;
        unsafe {
            GetFileSecurityW(
                path.as_ptr(),
                DACL_SECURITY_INFORMATION,
                null_mut(),
                0,
                &mut required,
            )
        };
        let mut descriptor = vec![0u8; required as usize];
        assert_ne!(
            unsafe {
                GetFileSecurityW(
                    path.as_ptr(),
                    DACL_SECURITY_INFORMATION,
                    descriptor.as_mut_ptr().cast(),
                    required,
                    &mut required,
                )
            },
            0
        );
        let mut control = 0;
        let mut revision = 0;
        assert_ne!(
            unsafe {
                GetSecurityDescriptorControl(
                    descriptor.as_mut_ptr().cast(),
                    &mut control,
                    &mut revision,
                )
            },
            0
        );
        assert_ne!(control & SE_DACL_PROTECTED, 0);
    }

    #[cfg(windows)]
    #[test]
    fn remote_config_open_fails_closed_when_existing_acl_is_broad() {
        let dir = tempfile::tempdir().unwrap();
        let store = RemoteConfigStore::open(dir.path()).unwrap();
        store
            .set_lan_settings(LanSettings {
                enabled: true,
                bind: "0.0.0.0:27655".parse().unwrap(),
            })
            .unwrap();
        let path = dir.path().join("remote-access.json");
        let status = std::process::Command::new("icacls")
            .arg(&path)
            .args(["/grant", "*S-1-1-0:(F)"])
            .status()
            .unwrap();
        assert!(status.success());

        assert!(RemoteConfigStore::open(dir.path()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn remote_config_open_fails_closed_when_existing_mode_is_broad() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let store = RemoteConfigStore::open(dir.path()).unwrap();
        store
            .set_lan_settings(LanSettings {
                enabled: true,
                bind: "0.0.0.0:27655".parse().unwrap(),
            })
            .unwrap();
        let path = dir.path().join("remote-access.json");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();

        assert!(RemoteConfigStore::open(dir.path()).is_err());
    }
}
