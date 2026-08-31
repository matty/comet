//! comet-engine — the headless backend: sessions engine, doc host + command executor,
//! run journal + crash recovery, and the IPC RPC server.
//!
//! Spec: ARCHITECTURE.md §5 and docs/research/feature-inventory.md §3. M2 surface:
//! sessions + docs + commands + minimal IPC. Terminals, repos/diffs, uploads, auth,
//! agent accounts, and the device-room host land in later milestones.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use comet_identity::DeviceIdentity;
pub use comet_proto::HarnessId;

mod approvals;
mod unattended;

pub mod agent_accounts;
pub mod diff_sync;
pub mod doc_host;
pub mod instance_lock;
pub mod lan_server;
pub mod registry;
pub mod remote_config;
pub mod remote_rpc;
pub mod repos;
pub mod rpc;
pub mod run_journal;
pub mod sessions;
pub mod spaces;
pub mod store;
pub mod terminals;
pub mod titles;
pub mod uploads;
pub mod workspace_host;

pub use agent_accounts::{AgentAccounts, AgentAccountsConfig};
pub use diff_sync::{
    CheckoutDiffSync, DiffFileTextPair, DiffSnapshot, capture_diff, read_diff_file_text,
    working_diff_base,
};
pub use doc_host::{ChatDocHandle, DocHost, DocHostConfig};
pub use instance_lock::InstanceLock;
pub use lan_server::{LanServer, LanServerHandle, LanServerStatus};
pub use registry::{HarnessDescriptor, HarnessRegistry, default_registry};
pub use remote_config::RemoteConfigStore;
pub use remote_rpc::{RemoteRpcService, remote_method_allowed};
pub use repos::{CheckoutIdentity, Repos, worktree_branch_from_title};
pub use rpc::{EngineRpc, LocalRpcService};
pub use run_journal::{JournalError, RunJournal};
pub use sessions::{JournaledEvent, SessionsEngine, SteerOutcome};
pub use spaces::SpacesSync;
pub use store::{DocsStore, PutToolDiffOutcome, StoreError, ToolDiffLimit};
pub use terminals::Terminals;
pub use titles::TitleGenerator;
pub use unattended::{
    DEFAULT_UNATTENDED_TIMEOUT_SECS, Presence, PresenceLease, RESUME_CLAUSE, UnattendedBound,
    WaitKind, due_for_expiry, humanize_bound, sweep_interval, unattended_note,
    unattended_timeout_from_env,
};
pub use uploads::{AttachmentChunk, Uploads};
pub use workspace_host::{WORKSPACE_DOC_ID, WorkspaceHost, WorkspaceHostConfig};

/// The one authoritative store directory for this Comet installation, under
/// the engine's data dir.
const LOCAL_STORE_DIR: &str = "local-store";

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("identity: {0}")]
    Identity(#[from] comet_identity::IdentityError),
    #[error("doc: {0}")]
    Doc(#[from] comet_doc::DocError),
    #[error("journal: {0}")]
    Journal(#[from] run_journal::JournalError),
    #[error("store: {0}")]
    Store(#[from] crate::StoreError),
    #[error("harness: {0}")]
    Harness(#[from] comet_harness::HarnessError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// Another engine already owns this data directory.
    ///
    /// Its own variant rather than an `Other` string so a surface can offer
    /// the one action that resolves it. As prose it reached the startup gate
    /// verbatim — a filesystem path, a pid and an env var name, none of which
    /// tell a user what to do.
    #[error("another engine is already running on {data_dir} (pid {pid})")]
    AlreadyRunning { data_dir: String, pid: String },
    /// The workspace tombstone committed, but one local artifact cleanup leg
    /// failed. Diagnostics stay in tracing and finalization will retry.
    #[error("chat cleanup is pending retry")]
    ChatCleanupPendingRetry,
    #[error("{0}")]
    Other(String),
}

/// Epoch millis now — the doc/journal timestamp base.
pub(crate) fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub(crate) fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Data directory (default `~/.comet-native`, dev `~/.comet-native-dev`).
    pub data_dir: PathBuf,
    /// Localhost IPC port for the UI.
    pub ipc_port: u16,
    /// Harness for doc-command runs on chats without a workspace `config` row.
    pub default_harness: HarnessId,
    /// Release metadata/download origin. It is not a runtime authority.
    pub releases_url: String,
    /// How long a wait no client can answer may last before the turn ends.
    /// `COMET_UNATTENDED_TIMEOUT_SECS`, default 24 hours.
    pub unattended_timeout: std::time::Duration,
}

impl EngineConfig {
    pub fn for_test(data_dir: &Path) -> Self {
        Self {
            data_dir: data_dir.to_path_buf(),
            ipc_port: 0,
            default_harness: HarnessId::Mock,
            releases_url: "http://127.0.0.1:1".into(),
            unattended_timeout: std::time::Duration::from_secs(DEFAULT_UNATTENDED_TIMEOUT_SECS),
        }
    }
}

/// The assembled engine core — also constructible without the IPC server for tests
/// and the in-process (headed) mode.
pub struct EngineCore {
    pub sessions: SessionsEngine,
    pub doc_host: DocHost,
    pub workspace: WorkspaceHost,
    pub registry: Arc<HarnessRegistry>,
    pub repos: Repos,
    pub terminals: Terminals,
    pub diff_sync: CheckoutDiffSync,
    pub spaces_sync: SpacesSync,
    pub uploads: Uploads,
    pub agent_accounts: AgentAccounts,
    pub device_id: String,
    /// When this installation's identity was last rebuilt from a zero-byte
    /// `device-id`, if ever — read once at assembly and reported over
    /// `LocalDevice` so the UI can explain why older spaces are offline (D96).
    pub identity_rebuilt_at: Option<String>,
    device_identity: Arc<DeviceIdentity>,
    remote_config: RemoteConfigStore,
    lan_server: LanServerHandle,
    /// Live supervisor count, tracked from boot so a daemon nobody ever
    /// connects to still starts an unattended stretch. The sweeper (a later
    /// slice) is the only reader that turns this into policy.
    presence: Arc<Presence>,
    rpc: std::sync::OnceLock<Arc<EngineRpc>>,
    local_rpc: std::sync::OnceLock<Arc<LocalRpcService>>,
    /// Release checker (attached by [`Engine::assemble_runtime`]) — the
    /// UpdateStatus stream + ApplyUpdate.
    updater: std::sync::Mutex<Option<comet_update::Updater>>,
    /// Exclusive data-dir lock — held for the engine's lifetime (single-instance).
    _instance_lock: InstanceLock,
}

impl EngineCore {
    /// Open stores under `data_dir`, wire sessions ⇄ doc host ⇄ workspace host, and
    /// recover stale journals from a previous crash.
    pub fn assemble(
        data_dir: &Path,
        registry: Arc<HarnessRegistry>,
        default_harness: HarnessId,
        _legacy_offline: Option<()>,
    ) -> Result<Self, EngineError> {
        std::fs::create_dir_all(data_dir)?;
        // Single-instance guard: two engines on one data dir would race the
        // SQLite snapshots + journals. Taken before any store opens or the IPC
        // port binds; held (and kernel-released on crash) for the engine's life.
        let lock = InstanceLock::acquire(data_dir)?;
        let device_id = load_or_create_device_id(data_dir)?;
        let identity_rebuilt_at = identity_rebuilt_at(data_dir);
        let device_identity = DeviceIdentity::load_or_create(data_dir)?;
        let remote_config = RemoteConfigStore::open(data_dir)?;
        let persisted_remotes = remote_config.watch_remotes().borrow().clone();
        for mut remote in persisted_remotes {
            if remote.last_state == comet_proto::RemoteConnectionState::Online {
                remote.last_state = comet_proto::RemoteConnectionState::Offline;
                remote_config.put_remote(remote)?;
            }
        }
        let lan_server = LanServerHandle::new(remote_config.clone(), device_identity.clone());
        // `DocsStore::open` creates the whole path, so the store root is the
        // only thing this needs to name.
        let local_root = data_dir.join(LOCAL_STORE_DIR);
        let store = Arc::new(DocsStore::open(&local_root)?);
        let journal = Arc::new(RunJournal::open(local_root.join("journals"))?);
        let detected_device_name = local_device_name();
        // Probe provider CLIs off the boot path. Fire-and-forget: nothing here
        // waits on it, and a harness stays selectable until its result lands.
        registry.spawn_probes();
        let sessions = SessionsEngine::new(device_id.clone(), journal, registry.clone());
        let doc_host = DocHost::new(
            store.clone(),
            DocHostConfig {
                device_id: device_id.clone(),
                default_harness,
            },
        );
        let workspace = WorkspaceHost::open(
            store,
            WorkspaceHostConfig {
                device_id: device_id.clone(),
                device_name: detected_device_name,
                platform: std::env::consts::OS.to_string(),
            },
        )?;
        doc_host.set_workspace(workspace.clone());
        doc_host.set_sessions(sessions.clone());
        sessions.set_doc_host(doc_host.clone());
        match sessions.recover_stale() {
            Ok(0) => {}
            Ok(recovered) => tracing::info!(recovered, "stale sessions recovered on boot"),
            Err(err) => tracing::error!(error = %err, "stale-session recovery failed"),
        }
        let repos = Repos::new(data_dir, &device_id);
        let terminals = Terminals::new();
        let uploads = Uploads::new(data_dir);
        let agent_accounts = AgentAccounts::new(AgentAccountsConfig::detect(data_dir));
        sessions.set_titles(TitleGenerator::new(
            workspace.clone(),
            doc_host.clone(),
            registry.clone(),
            repos.clone(),
        ));
        let diff_sync = CheckoutDiffSync::start(repos.clone(), workspace.clone(), &device_id);
        let spaces_sync = SpacesSync::start(repos.clone(), workspace.clone(), &device_id);
        Ok(Self {
            sessions,
            doc_host,
            workspace,
            registry,
            repos,
            terminals,
            diff_sync,
            spaces_sync,
            uploads,
            agent_accounts,
            device_id,
            identity_rebuilt_at,
            device_identity,
            remote_config,
            lan_server,
            presence: Presence::new(chrono::Utc::now()),
            rpc: std::sync::OnceLock::new(),
            local_rpc: std::sync::OnceLock::new(),
            updater: std::sync::Mutex::new(None),
            _instance_lock: lock,
        })
    }

    /// Attach the release checker (before building the RPC service).
    pub fn set_updater(&self, updater: comet_update::Updater) {
        *self
            .updater
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(updater);
    }

    pub fn updater(&self) -> Option<comet_update::Updater> {
        self.updater
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn engine_rpc_service(&self) -> Arc<EngineRpc> {
        self.rpc
            .get_or_init(|| {
                let hello = comet_proto::ServerHello {
                    protocol_version: comet_proto::PROTOCOL_VERSION,
                    server_id: self.device_identity.server_id().clone(),
                    device_id: self.device_id.clone(),
                    name: self.workspace.device_name().to_string(),
                    capabilities: vec!["authoritative-rpc".into(), "pairing".into()],
                };
                let mut rpc = EngineRpc::new(
                    self.sessions.clone(),
                    self.doc_host.clone(),
                    self.workspace.clone(),
                    self.registry.clone(),
                    self.repos.clone(),
                    self.terminals.clone(),
                    self.diff_sync.clone(),
                    self.uploads.clone(),
                    self.agent_accounts.clone(),
                    self.presence.clone(),
                )
                .with_server_hello(hello)
                .with_identity_rebuilt_at(self.identity_rebuilt_at.clone());
                if let Some(updater) = self.updater() {
                    rpc = rpc.with_updater(updater);
                }
                Arc::new(rpc)
            })
            .clone()
    }

    /// Authoritative service exposed to explicitly paired direct-LAN clients.
    pub fn remote_rpc_service(&self) -> Arc<EngineRpc> {
        self.engine_rpc_service()
    }

    pub fn rpc_service(&self) -> Arc<LocalRpcService> {
        let engine_rpc = self.engine_rpc_service();
        let local = self
            .local_rpc
            .get_or_init(|| {
                Arc::new(LocalRpcService::new(
                    engine_rpc.clone(),
                    self.remote_config.clone(),
                    self.lan_server.clone(),
                ))
            })
            .clone();
        self.lan_server
            .ensure_started(Arc::new(RemoteRpcService::new(
                engine_rpc,
                self.device_id.clone(),
            )));
        local
    }

    pub fn remote_config(&self) -> &RemoteConfigStore {
        &self.remote_config
    }

    /// Live supervisor count. The unattended sweeper reads it; nothing else
    /// should make policy from it.
    pub fn presence(&self) -> Arc<Presence> {
        self.presence.clone()
    }

    pub fn device_identity(&self) -> &DeviceIdentity {
        &self.device_identity
    }

    pub fn lan_status(&self) -> LanServerStatus {
        self.lan_server.status()
    }

    /// Graceful teardown: settle live runs (streaming entries stamped `aborted`),
    /// kill live PTYs, stamp our workspace `lastSeenAt`, and flush every open doc
    /// snapshot.
    pub async fn shutdown(&self) {
        self.lan_server.shutdown().await;
        self.sessions.shutdown().await;
        self.terminals.shutdown();
        self.agent_accounts.shutdown();
        self.doc_host.flush_all();
        self.workspace.shutdown();
    }
}

pub struct Engine {
    pub config: EngineConfig,
}

pub struct EngineRuntime {
    core: EngineCore,
    /// The unattended sweeper's task. Owned outright: the task owns clones of
    /// `SessionsEngine` and `Presence`, so a detached one keeps sweeping
    /// against shut-down stores, and every recreated embedded engine would
    /// leave another behind.
    ///
    /// `shutdown` consumes `self` to get at it, which is what makes a second
    /// concurrent shutdown impossible to write rather than merely unlikely —
    /// there is no `&self` path left that could observe the handle already
    /// taken and skip the wait.
    sweeper: tokio::task::JoinHandle<()>,
}

impl EngineRuntime {
    pub fn core(&self) -> &EngineCore {
        &self.core
    }

    pub async fn shutdown(self) {
        // Sweeper first, and awaited: `abort` only schedules cancellation, it
        // doesn't stop the task before its next await point, so without the
        // await a sweep already past that point could still call
        // `expire_unattended` against stores `core.shutdown()` is closing.
        // Awaiting the aborted handle guarantees the task has stopped before
        // we proceed.
        self.sweeper.abort();
        let _ = self.sweeper.await;
        self.core.shutdown().await;
    }
}

impl Engine {
    pub fn new(config: EngineConfig) -> Self {
        Self { config }
    }

    pub async fn assemble_runtime(config: &EngineConfig) -> anyhow::Result<EngineRuntime> {
        let core = EngineCore::assemble(
            &config.data_dir,
            Arc::new(default_registry()),
            config.default_harness,
            None,
        )?;
        // The release checker is an independent distribution edge, not runtime authority.
        // installs with COMET_AUTO_UPDATE=1 apply + restart themselves — gated
        // on quiescence so a restart never lands under a live run or open PTY.
        let quiescent: comet_update::QuiescentCheck = {
            let sessions = core.sessions.clone();
            let terminals = core.terminals.clone();
            Arc::new(move || !sessions.any_active() && !terminals.any_open())
        };
        core.set_updater(comet_update::Updater::spawn(
            config.releases_url.clone(),
            Some(quiescent),
        ));

        let sweeper = spawn_unattended_sweeper(
            core.sessions.clone(),
            core.presence(),
            UnattendedBound(config.unattended_timeout),
        );

        tracing::info!(device_id = %core.device_id, "engine core assembled");
        Ok(EngineRuntime { core, sweeper })
    }

    /// Run the local engine and opt-in LAN server until shutdown.
    pub async fn run(self) -> anyhow::Result<()> {
        let config = self.config;
        tracing::info!(data_dir = %config.data_dir.display(), "engine starting");

        let runtime = Self::assemble_runtime(&config).await?;

        // A daemon exists to serve this port, so a bind failure is fatal here —
        // unlike the headed app, which can still work over its in-process
        // transport (see `serve_ipc`).
        let server = serve_ipc(config.ipc_port, runtime.core().rpc_service()).await?;

        shutdown_signal().await?;
        tracing::info!("shutting down");
        server.abort();
        runtime.shutdown().await;
        Ok(())
    }
}

/// Ctrl-C or SIGTERM. systemd/launchd stop (and the auto-updater's service
/// restart) deliver SIGTERM — without catching it the daemon dies mid-write
/// and every stop takes the crash-recovery path instead of the graceful drain.
async fn shutdown_signal() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            _ = sigterm.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await
    }
}

/// Spawn the unattended sweeper: one task, not a timer per wait. Presence
/// edges would otherwise have to cancel and re-arm N timers, and a deadline
/// still has to be per-run for the park-while-disconnected case. At the
/// 24-hour default this wakes once a minute.
///
/// A free function (not inlined into `assemble_runtime`) so a test can spawn
/// it directly against a bare `EngineCore::assemble` core — `assemble_runtime`
/// itself hard-codes `default_registry()`, which has no harness a test can
/// park deterministically without a process-global env var.
///
/// Returns the task rather than detaching it: the loop never ends on its own,
/// and it holds the sessions engine and presence, so only the owner aborting it
/// stops it. [`EngineRuntime::shutdown`] is that owner.
pub fn spawn_unattended_sweeper(
    sessions: SessionsEngine,
    presence: Arc<Presence>,
    bound: UnattendedBound,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(sweep_interval(bound));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            sessions
                .expire_unattended(&presence, chrono::Utc::now(), bound.get())
                .await;
        }
    })
}

/// Serve the typed RPC on the localhost IPC port.
///
/// Both engines call this: the headless daemon, and the headed app's embedded
/// engine. That second case is the point — an embedded engine that keeps the
/// port to itself forces anyone wanting a second viewport (the terminal app) to
/// stop the desktop app, start a daemon, and start it again in the right order.
/// Serving here means any viewport can just attach.
///
/// Localhost only, exactly as before: this widens *which process* can serve the
/// port, not who can reach it.
pub async fn serve_ipc(
    port: u16,
    service: std::sync::Arc<dyn comet_rpc::RpcService>,
) -> std::io::Result<tokio::task::JoinHandle<()>> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    tracing::info!(port, "IPC server listening");
    Ok(tokio::spawn(comet_rpc::serve_ws_listener(
        listener, service,
    )))
}

/// Best-effort human name for this device's registry row (hostname).
fn local_device_name_from(
    getenv: impl Fn(&str) -> Option<String>,
    read_hostname: impl Fn() -> Option<String>,
) -> String {
    ["COMET_DEVICE_NAME", "COMPUTERNAME", "HOSTNAME"]
        .into_iter()
        .filter_map(getenv)
        .chain(read_hostname())
        .map(|value| value.trim().to_string())
        .find(|value| !value.is_empty())
        .unwrap_or_else(|| "unknown-device".to_string())
}

fn local_device_name() -> String {
    local_device_name_from(
        |key| std::env::var(key).ok(),
        || std::fs::read_to_string("/etc/hostname").ok(),
    )
}

/// How long to keep re-reading after losing a publish race, or after failing to
/// displace an empty file because another process holds it open.
///
/// **A wall-clock budget, not an attempt count (D126).** It was 24 attempts at
/// a flat 3ms — about 72ms — sized against real contention (Windows `rename`
/// and `remove_file` fail with `ERROR_ACCESS_DENIED` while any other handle is
/// open, and 8 concurrent openers reproduced it) but sized on an UNLOADED
/// machine. Under a saturated one the same 24 slices buy far less progress,
/// and exhausting them fails startup outright. That is D89's mechanism exactly:
/// a budget that held when idle, missed under parallel builds.
///
/// Five seconds sounds enormous for a startup path and is not, because of when
/// it is spent: only while repairing a corrupt identity, only under contention,
/// and only in place of refusing to start at all. The common path never sleeps
/// once.
const DEVICE_ID_RECOVERY_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);

/// The first backoff, and the step the schedule doubles from.
const DEVICE_ID_RETRY_PAUSE: std::time::Duration = std::time::Duration::from_millis(3);

/// The ceiling a doubled backoff stops at, so a long budget stays responsive
/// rather than sleeping through the window in which a winner publishes.
const DEVICE_ID_RETRY_PAUSE_CAP: std::time::Duration = std::time::Duration::from_millis(50);

/// How long to wait before attempt `attempt` (0-based) tries again.
///
/// Exponential from [`DEVICE_ID_RETRY_PAUSE`] to [`DEVICE_ID_RETRY_PAUSE_CAP`].
/// Backing off matters more than it looks: every repairer sleeping the same
/// flat 3ms is a lock-step convoy that keeps colliding, which is the shape that
/// exhausted the old fixed count.
fn device_id_retry_pause(attempt: u32) -> std::time::Duration {
    DEVICE_ID_RETRY_PAUSE
        .saturating_mul(2u32.saturating_pow(attempt.min(16)))
        .min(DEVICE_ID_RETRY_PAUSE_CAP)
}

/// Stable per-installation device id, persisted at `{data_dir}/device-id`.
///
/// Reads it, minting one on first run and
/// RECOVERING a zero-byte file rather than refusing to start.
///
/// The empty case is not hypothetical. Builds before the temp-file publish
/// (`13cd956f`) wrote the id with `std::fs::write`, which creates, truncates,
/// then writes: a crash or a full disk between the truncate and the write
/// leaves nothing behind. Treating that as a hard error stranded the whole
/// installation permanently — the engine refused to start and no amount of
/// restarting helped, because nothing ever rewrote the file.
///
/// A fresh id is safe to mint here: this is the CRDT authorship string, not
/// the pairing identity. That is `device-identity.pem` in `comet-identity`,
/// a different file, so recovering does not invalidate a paired LAN peer.
/// The cost is that entries written before the crash keep the old author id.
fn load_or_create_device_id(data_dir: &Path) -> Result<String, EngineError> {
    std::fs::create_dir_all(data_dir)?;
    let path = data_dir.join("device-id");
    let started = std::time::Instant::now();
    let mut attempt = 0u32;
    // Whether THIS process saw the file empty. Recorded even when another
    // repairer wins the publish and we take their id: the machine's identity
    // was rebuilt either way, and that is what the user needs told (D96).
    let mut rebuilt = false;
    while started.elapsed() < DEVICE_ID_RECOVERY_BUDGET {
        let pause = device_id_retry_pause(attempt);
        attempt += 1;
        match std::fs::read_to_string(&path) {
            Ok(id) if !id.trim().is_empty() => {
                if rebuilt {
                    record_identity_rebuilt(data_dir);
                }
                return Ok(id.trim().to_string());
            }
            // Empty: displace it so the create-if-absent publish below has a
            // clear path. Renaming rather than deleting keeps the failure
            // recoverable if we die here — the next start finds no file and
            // mints one, which is the same outcome by a different route.
            Ok(_) => {
                rebuilt = true;
                if !displace_empty_device_id(data_dir, &path)? {
                    // Another repairer holds the file open. Back off and
                    // re-read: they are most likely about to publish, and
                    // taking their result is the whole point.
                    std::thread::sleep(pause);
                    continue;
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }

        let id = new_id();
        let temp_path = data_dir.join(format!(
            ".device-id.tmp-{}-{}",
            std::process::id(),
            new_id()
        ));
        let write_result = (|| -> Result<(), EngineError> {
            let mut temp = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp_path)?;
            temp.write_all(id.as_bytes())?;
            temp.sync_all()?;
            Ok(())
        })();
        if let Err(err) = write_result {
            let _ = std::fs::remove_file(&temp_path);
            return Err(err);
        }

        // Publish only fully-written bytes and never replace another process's
        // winner. A hard link is the portable create-if-absent primitive already
        // used for local-profile identity; losers re-read on the next pass.
        let publish_result = std::fs::hard_link(&temp_path, &path);
        let _ = std::fs::remove_file(&temp_path);
        match publish_result {
            Ok(()) => {
                if rebuilt {
                    record_identity_rebuilt(data_dir);
                }
                return Ok(id);
            }
            // Someone published first, or the empty file is still there with a
            // repairer mid-flight. Re-read on the next pass.
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::PermissionDenied
                ) =>
            {
                std::thread::sleep(pause);
                continue;
            }
            Err(err) => return Err(err.into()),
        }
    }
    Err(EngineError::Other(format!(
        "could not establish a device identity at {} ({attempt} attempts over {:?})",
        path.display(),
        started.elapsed()
    )))
}

/// Marker for "this installation's device identity was rebuilt", written
/// beside the id it replaced.
///
/// **A file rather than a boot-scoped flag, because the consequence is not
/// boot-scoped** (D96). Spaces are keyed by `deviceId` and that field is
/// immutable by design, so every space created under the old id renders
/// `@ host · offline` and cannot dispatch — for good, not for one session.
/// A notice that appeared once and vanished would leave the same silence
/// behind the second time the user looked.
///
/// It cannot say WHICH spaces: the old id died with the zero-byte file, so
/// nothing on this machine still knows it. The honest statement is that
/// identity was rebuilt and older spaces belong to what came before.
const DEVICE_ID_REBUILT_MARKER: &str = "device-id.rebuilt";

/// When this installation's device identity was last rebuilt, if it ever was.
///
/// RFC 3339, read straight from [`DEVICE_ID_REBUILT_MARKER`]. Unreadable,
/// missing or empty all answer `None` — this drives a notice, so failing to
/// read it must never fail anything else.
pub fn identity_rebuilt_at(data_dir: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(data_dir.join(DEVICE_ID_REBUILT_MARKER)).ok()?;
    let stamp = raw.trim();
    (!stamp.is_empty()).then(|| stamp.to_owned())
}

/// Record that recovery happened, best-effort.
///
/// A failure here loses the explanation, never the recovery: the engine has a
/// working identity by this point and refusing to start over an unwritable
/// marker would reintroduce exactly the stranded installation #96 fixed.
fn record_identity_rebuilt(data_dir: &Path) {
    let stamp = chrono::Utc::now().to_rfc3339();
    let path = data_dir.join(DEVICE_ID_REBUILT_MARKER);
    if let Err(err) = std::fs::write(&path, &stamp) {
        tracing::warn!(
            path = %path.display(),
            error = %err,
            "device identity was rebuilt, but the marker could not be written;              the user will not be told why older spaces are offline"
        );
        return;
    }
    tracing::warn!(
        stamp,
        "device identity was rebuilt from an empty device-id; spaces created          under the previous identity will read as offline"
    );
}

/// Move a zero-byte `device-id` aside so the create-if-absent publish can run.
///
/// The only production caller reaches this after `EngineCore::assemble` holds
/// `InstanceLock`, so two live engines cannot race recovery. The bounded
/// recovery remains necessary after a crashed engine releases that lock.
fn displace_empty_device_id(data_dir: &Path, path: &Path) -> Result<bool, EngineError> {
    let aside = data_dir.join(format!(".device-id.empty-{}", new_id()));
    match std::fs::rename(path, &aside) {
        Ok(()) => {
            let _ = std::fs::remove_file(&aside);
            Ok(true)
        }
        // Another process displaced it first; the path is clear either way.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(true),
        // Windows refuses to rename a file another handle has open
        // (ERROR_ACCESS_DENIED). That is contention, not corruption, so the
        // caller backs off and re-reads instead of failing the startup.
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => Ok(false),
        Err(err) => Err(err.into()),
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn a_second_assembly_cannot_rebuild_device_id_while_the_first_holds_the_instance_lock() {
        let dir = tempfile::tempdir().unwrap();
        let first = EngineCore::assemble(
            dir.path(),
            Arc::new(HarnessRegistry::new()),
            HarnessId::Mock,
            None,
        )
        .expect("first engine assembles and retains InstanceLock");

        let path = dir.path().join("device-id");
        assert!(!first.device_id.trim().is_empty());
        assert_eq!(identity_rebuilt_at(dir.path()), None);
        std::fs::write(&path, "").expect("manufacture the legacy-corrupt file");

        let Err(error) = EngineCore::assemble(
            dir.path(),
            Arc::new(HarnessRegistry::new()),
            HarnessId::Mock,
            None,
        ) else {
            panic!("second assembly must be rejected before device-id recovery");
        };
        assert!(matches!(error, EngineError::AlreadyRunning { .. }));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
        assert_eq!(identity_rebuilt_at(dir.path()), None);
    }

    /// A zero-byte `device-id` is what a pre-`13cd956f` crash leaves behind.
    /// Erroring on it stranded the installation with no way out, so the whole
    /// point of the recovery is that this returns an id at all.
    #[test]
    fn an_empty_device_id_is_recovered_rather_than_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("device-id");
        std::fs::write(&path, "").unwrap();

        let id = load_or_create_device_id(dir.path()).expect("empty file recovers");

        assert!(!id.trim().is_empty(), "a real id is minted");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap().trim(),
            id,
            "the recovered id is persisted, so the next start is stable"
        );
    }

    /// Whitespace-only counts as empty for the same reason a zero-byte file
    /// does — a partial write is not an identity.
    #[test]
    fn a_whitespace_only_device_id_is_recovered_too() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("device-id"),
            "   
",
        )
        .unwrap();
        let id = load_or_create_device_id(dir.path()).expect("whitespace recovers");
        assert!(!id.trim().is_empty());
    }

    /// The recovery must never fire on a healthy file: displacing a good id
    /// would silently re-author every entry written afterwards.
    #[test]
    fn an_existing_device_id_is_never_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("device-id");
        std::fs::write(&path, "device-abc").unwrap();

        assert_eq!(load_or_create_device_id(dir.path()).unwrap(), "device-abc");
        assert_eq!(load_or_create_device_id(dir.path()).unwrap(), "device-abc");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "device-abc");
    }

    /// Recovery leaves no `.device-id.empty-*` litter in the data directory.
    #[test]
    fn recovery_cleans_up_after_itself() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("device-id"), "").unwrap();
        load_or_create_device_id(dir.path()).unwrap();

        let strays: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".device-id."))
            .collect();
        assert!(strays.is_empty(), "left behind: {strays:?}");
    }

    /// Break caught (D126): the retry budget was 24 attempts at a flat 3ms,
    /// sized against real contention but on an UNLOADED machine. One
    /// full-workspace run with four other cargo builds saturating the box
    /// exhausted it, and `concurrent_recovery_agrees_on_one_id` `.unwrap()`s
    /// that error into a panic — D89's mechanism exactly.
    ///
    /// Asserts the property that makes the change a fix rather than a reshuffle:
    /// **the new budget can never buy fewer attempts than the old constant
    /// did.** A count is what got sized wrong, so the replacement is checked
    /// against the count it replaced rather than against a number picked here.
    #[test]
    fn the_recovery_budget_never_buys_fewer_attempts_than_the_fixed_count_did() {
        const OLD_ATTEMPTS: u32 = 24;

        let mut spent = std::time::Duration::ZERO;
        let mut attempts = 0u32;
        while spent < DEVICE_ID_RECOVERY_BUDGET {
            spent += device_id_retry_pause(attempts);
            attempts += 1;
        }

        assert!(
            attempts > OLD_ATTEMPTS,
            "the budget bought {attempts} attempts, no better than the              {OLD_ATTEMPTS} that failed under load"
        );
    }

    /// The backoff itself: exponential from the first pause to the cap, and
    /// capped rather than doubling forever. Both halves matter — a flat pause
    /// is a lock-step convoy that keeps colliding, and an uncapped one sleeps
    /// through the window a winner publishes in.
    #[test]
    fn the_retry_backoff_grows_and_then_stops_growing() {
        assert_eq!(device_id_retry_pause(0), DEVICE_ID_RETRY_PAUSE);
        assert_eq!(device_id_retry_pause(1), DEVICE_ID_RETRY_PAUSE * 2);
        assert_eq!(device_id_retry_pause(2), DEVICE_ID_RETRY_PAUSE * 4);
        assert_eq!(device_id_retry_pause(60), DEVICE_ID_RETRY_PAUSE_CAP);
        for attempt in 0..1_000 {
            assert!(
                device_id_retry_pause(attempt) <= DEVICE_ID_RETRY_PAUSE_CAP,
                "attempt {attempt} slept past the cap"
            );
        }
    }

    /// Break caught (D96): recovery re-homed the machine in silence. Spaces
    /// are keyed by `deviceId` and that field is immutable by design, so every
    /// space made under the old id reads `@ host · offline` for good — and
    /// nothing recorded that recovery had happened, so nothing could say why.
    ///
    /// The marker is the whole mechanism, so both directions are asserted: a
    /// first-run mint must NOT claim an identity was rebuilt, or the notice
    /// fires on every fresh install.
    #[test]
    fn recovering_an_empty_device_id_records_that_identity_was_rebuilt() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path();

        let first = load_or_create_device_id(data).expect("first run mints an id");
        assert_eq!(
            identity_rebuilt_at(data),
            None,
            "a first-run mint is not a rebuild"
        );

        std::fs::write(data.join("device-id"), "").unwrap();
        let second = load_or_create_device_id(data).expect("an empty id is recovered");

        assert_ne!(first, second, "recovery mints a new id");
        let stamp = identity_rebuilt_at(data).expect("recovery has to be recorded");
        assert!(
            chrono::DateTime::parse_from_rfc3339(&stamp).is_ok(),
            "the marker holds an RFC 3339 stamp: {stamp}"
        );
    }

    /// The marker outlives the boot that wrote it, which is the reason it is a
    /// file: the spaces stay offline for good, so an explanation that vanished
    /// with the process would leave the same silence behind next launch.
    #[test]
    fn the_rebuilt_marker_survives_a_later_ordinary_start() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path();

        load_or_create_device_id(data).unwrap();
        std::fs::write(data.join("device-id"), "").unwrap();
        let recovered = load_or_create_device_id(data).unwrap();
        let stamp = identity_rebuilt_at(data).expect("recorded on the recovering start");

        let later = load_or_create_device_id(data).unwrap();
        assert_eq!(later, recovered, "an ordinary start reads the same id");
        assert_eq!(
            identity_rebuilt_at(data).as_deref(),
            Some(stamp.as_str()),
            "and must not clear or restamp the explanation"
        );
    }

    /// The reason recovery publishes through create-if-absent rather than just
    /// writing the file: two engines can cold-start together, and if they
    /// disagreed about the id they would author the same doc under two
    /// identities. Every caller must see the one that actually landed on disk.
    #[test]
    fn concurrent_recovery_agrees_on_one_id() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("device-id"), "").unwrap();
        let root = dir.path().to_path_buf();

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let root = root.clone();
                std::thread::spawn(move || load_or_create_device_id(&root).unwrap())
            })
            .collect();
        let ids: Vec<String> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        let on_disk = std::fs::read_to_string(root.join("device-id")).unwrap();
        let on_disk = on_disk.trim();
        assert!(!on_disk.is_empty(), "an id landed");
        for id in &ids {
            assert_eq!(id, on_disk, "every caller got the id that landed: {ids:?}");
        }
    }

    #[test]
    fn local_name_prefers_override_then_windows_hostname() {
        let name = local_device_name_from(
            |key| match key {
                "COMET_DEVICE_NAME" => Some("  Lab Override  ".into()),
                "COMPUTERNAME" => Some("BUILD-PC".into()),
                "HOSTNAME" => Some("unix-host".into()),
                _ => None,
            },
            || Some("file-host".into()),
        );
        assert_eq!(name, "Lab Override");

        let windows = local_device_name_from(
            |key| (key == "COMPUTERNAME").then(|| "BUILD-PC".into()),
            || None,
        );
        assert_eq!(windows, "BUILD-PC");
    }

    #[test]
    fn local_name_ignores_empty_values_and_falls_back() {
        let hostname = local_device_name_from(
            |key| match key {
                "COMET_DEVICE_NAME" | "COMPUTERNAME" => Some("   ".into()),
                "HOSTNAME" => Some(" linux-box ".into()),
                _ => None,
            },
            || Some("file-host".into()),
        );
        assert_eq!(hostname, "linux-box");
    }

    #[test]
    fn local_name_uses_hostname_file_after_empty_environment() {
        let name = local_device_name_from(|_| Some("   ".into()), || Some(" file-host\n".into()));
        assert_eq!(name, "file-host");
    }

    #[test]
    fn local_name_uses_unknown_device_as_the_final_fallback() {
        let name = local_device_name_from(|_| None, || Some("  \n".into()));
        assert_eq!(name, "unknown-device");
    }

    #[tokio::test]
    async fn rpc_services_from_one_core_share_mutation_authority() {
        let dir = tempfile::tempdir().unwrap();
        let core = EngineCore::assemble(
            dir.path(),
            Arc::new(HarnessRegistry::new()),
            HarnessId::Mock,
            None,
        )
        .unwrap();
        let first = core.rpc_service();
        let second = core.rpc_service();
        assert!(first.shares_mutation_authority(&second));
        core.shutdown().await;
    }

    /// `EngineRuntime::shutdown` must not return until the aborted sweeper
    /// task has actually stopped, not merely been asked to. This core has
    /// nothing live (no LAN server started, no sessions, no terminals), so
    /// every step of `core.shutdown()` resolves on its first poll without
    /// ever yielding to the scheduler — the only way the stand-in sweeper
    /// task below gets polled and dropped at all is the explicit
    /// `sweeper.await` inside `shutdown`. That makes this deterministic
    /// rather than a race: pre-fix (bare `abort()`, no join) the flag is
    /// reliably still unset when `shutdown()` returns; post-fix it is
    /// reliably set, because `JoinHandle::await` only resolves after the
    /// task's drop glue has run.
    #[tokio::test]
    async fn shutdown_waits_for_the_sweeper_to_actually_stop() {
        let dir = tempfile::tempdir().unwrap();
        let core = EngineCore::assemble(
            dir.path(),
            Arc::new(HarnessRegistry::new()),
            HarnessId::Mock,
            None,
        )
        .unwrap();

        let stopped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        struct MarkOnDrop(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for MarkOnDrop {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let guard = MarkOnDrop(stopped.clone());
        let sweeper = tokio::spawn(async move {
            let _guard = guard;
            // Never completes on its own; only abort ends it, and abort only
            // drops it at its next await point, which is right here.
            std::future::pending::<()>().await
        });

        let runtime = EngineRuntime { core, sweeper };
        runtime.shutdown().await;

        assert!(
            stopped.load(std::sync::atomic::Ordering::SeqCst),
            "shutdown() returned before the aborted sweeper task finished dropping"
        );
    }
}
