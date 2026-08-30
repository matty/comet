//! SpacesSync — owner-side upkeep of space rows (git presence) plus the
//! orphan-chat repair sweep.
//!
//! A space is a synced (device, folder) pair; the folder need NOT be a git
//! repo. This service watches the workspace `spaces` rows owned by THIS device
//! and keeps their `gitDetected`/`checkoutId` stamps truthful:
//!
//! - recheck on boot / when a space row is first observed;
//! - a non-recursive `notify` watcher on the space folder — `.git` appearing or
//!   vanishing (git init / de-git) kicks a recheck;
//! - a slow 2-minute repair tick (native watchers coalesce/drop events).
//!
//! Stamps are written ONLY on change, so steady state never grows the oplog.
//! Remote devices read `space.git_detected` straight from the doc — branch
//! pickers and the diff sidebar gate on it with zero RPCs.
//!
//! The repair tick also runs the orphan sweep: a chat created concurrently
//! with a `deleteSpace` on another device can sync in after the cascade ran,
//! leaving a dangling `spaceId`. The HOST device deletes its own such chats
//! (writer discipline — we never touch other devices' rows).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use tokio::sync::{mpsc, watch};

use comet_proto::Space;

use crate::repos::Repos;
use crate::workspace_host::WorkspaceHost;

/// Trailing debounce after a filesystem event burst.
const WATCH_DEBOUNCE: Duration = Duration::from_millis(500);
/// Slow repair pass: recheck every owned space + orphan sweep.
const REPAIR_INTERVAL: Duration = Duration::from_secs(120);

struct SpaceEntry {
    path: PathBuf,
    kick_tx: mpsc::UnboundedSender<()>,
    /// Keeps the folder watcher alive; dropped on entry close. Filled
    /// asynchronously — watch registration blocks, so [`reconcile`] builds it
    /// off the runtime and attaches it here once ready.
    folder_watch: Mutex<Option<notify::RecommendedWatcher>>,
}

impl SpaceEntry {
    /// Stop this entry's folder watcher, if one is armed, and confirm —
    /// bounded by `timeout` — that its OS directory handle has actually been
    /// released before returning (D103).
    ///
    /// `notify`'s own `Drop`/`unwatch` on Windows only *post* a stop request
    /// to the watcher's single background thread and return immediately
    /// (`notify-7.0.0/src/windows.rs`: `ReadDirectoryChangesWatcher::drop`
    /// and `unwatch_inner` both just `tx.send(...)` then return) — so
    /// nothing built on that alone can tell a caller the handle is actually
    /// gone, which is exactly the gap this method closes.
    ///
    /// That background thread's own handling of an unwatch *is* synchronous
    /// internally: `remove_watch`/`stop_watch` (`windows.rs:230-255`)
    /// cancels the pending I/O and blocks the watcher's own thread on its
    /// completion semaphore before it looks at the next queued action. The
    /// fence below is built entirely from `notify`'s public API: `unwatch`
    /// queues that cancel-and-wait as one action on the watcher's single
    /// consumer channel, and the immediately following `configure` queues a
    /// second action that thread can only reach once the first is done —
    /// its ack (which `Watcher::configure`'s public contract already blocks
    /// on) is the sentinel. Both calls run on the SAME calling thread here,
    /// in program order, so they land on that channel in that order. This is
    /// a real fence, not a fixed sleep or a guess — see the test module for
    /// a reproduction that fails without it and passes with it.
    ///
    /// Returns `true` once confirmed (including "nothing was ever
    /// watching"). Returns `false` if `timeout` elapses first: the stop
    /// request has still been sent and the watcher still finishes dropping
    /// in the background — so this is not a failure to stop, only a failure
    /// to confirm *in time*. A caller that must know now (e.g. one about to
    /// delete the directory) should treat `false` as "cannot proceed yet,"
    /// never retry unboundedly (`.agents/rules/user-facing-errors.md`'s
    /// bounded-wait rule applies even though this isn't a user-facing
    /// surface).
    async fn stop_watch(&self, timeout: Duration) -> bool {
        let Some(mut watcher) = lock(&self.folder_watch).take() else {
            return true; // never armed (or already stopped) — nothing to confirm
        };
        let path = self.path.clone();
        let task = tokio::task::spawn_blocking(move || {
            use notify::Watcher as _;
            let _ = watcher.unwatch(&path);
            let _ = watcher.configure(notify::Config::default());
            // `watcher` drops here; its own `Action::Stop` has nothing left
            // registered to tear down, since `unwatch` above already did.
        });
        matches!(tokio::time::timeout(timeout, task).await, Ok(Ok(())))
    }
}

struct SpacesSyncInner {
    repos: Repos,
    workspace: WorkspaceHost,
    device_id: String,
    entries: Mutex<HashMap<String, Arc<SpaceEntry>>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Clone)]
pub struct SpacesSync {
    inner: Arc<SpacesSyncInner>,
}

impl SpacesSync {
    /// Build and start the sync loop: follows the workspace spaces watch and
    /// runs the repair tick. Requires a tokio runtime.
    pub fn start(repos: Repos, workspace: WorkspaceHost, device_id: &str) -> Self {
        let sync = Self {
            inner: Arc::new(SpacesSyncInner {
                repos,
                workspace: workspace.clone(),
                device_id: device_id.to_string(),
                entries: Mutex::new(HashMap::new()),
            }),
        };
        tokio::spawn(spaces_task(
            Arc::downgrade(&sync.inner),
            workspace.watch_spaces(),
        ));
        sync
    }

    /// Reconcile + recheck now (tests / opportunistic callers).
    pub async fn reconcile_now(&self) {
        let spaces = self.inner.workspace.watch_spaces().borrow().clone();
        reconcile(&self.inner, &spaces);
        for entry in lock(&self.inner.entries).values() {
            let _ = entry.kick_tx.send(());
        }
    }

    /// Stop watching `space_id`'s folder (if this device owns an entry for it
    /// and a watcher is armed) and confirm — bounded by `timeout` — that its
    /// OS directory handle has actually been released before returning. See
    /// [`SpaceEntry::stop_watch`] for the mechanism and what a `false`
    /// return means (D103).
    pub async fn stop_watch(&self, space_id: &str, timeout: Duration) -> bool {
        let entry = lock(&self.inner.entries).get(space_id).cloned();
        match entry {
            Some(entry) => entry.stop_watch(timeout).await,
            None => true,
        }
    }
}

/// (Re)build the entry set for the spaces THIS device owns.
fn reconcile(inner: &Arc<SpacesSyncInner>, spaces: &[Space]) {
    let owned: HashMap<&str, &Space> = spaces
        .iter()
        .filter(|s| s.device_id == inner.device_id)
        .map(|s| (s.id.as_str(), s))
        .collect();

    let mut entries = lock(&inner.entries);
    entries.retain(|id, _| owned.contains_key(id.as_str()));
    for (id, space) in owned {
        if entries.contains_key(id) {
            continue; // deviceId/path are immutable — nothing to refresh
        }
        let (kick_tx, kick_rx) = mpsc::unbounded_channel();
        let entry = Arc::new(SpaceEntry {
            path: PathBuf::from(&space.path),
            kick_tx: kick_tx.clone(),
            folder_watch: Mutex::new(None),
        });
        entries.insert(id.to_string(), entry.clone());
        tokio::spawn(entry_task(
            Arc::downgrade(inner),
            id.to_string(),
            Arc::downgrade(&entry),
            kick_rx,
        ));
        let _ = kick_tx.send(()); // initial check (boot / first observed)

        // Non-recursive watcher on the space folder: `.git` appearing/vanishing
        // among the direct children is exactly the signal we need. Watch
        // failures are fine — the repair tick still converges. Built off the
        // runtime: watch registration blocks, and reconcile runs on the
        // spaces-watch task.
        let weak = Arc::downgrade(&entry);
        tokio::task::spawn_blocking(move || {
            let Some(entry) = weak.upgrade() else {
                return; // entry removed before the watcher was ready
            };
            let tx = entry.kick_tx.clone();
            let result =
                notify::recommended_watcher(move |event: Result<notify::Event, notify::Error>| {
                    let Ok(event) = event else { return };
                    if event
                        .paths
                        .iter()
                        .any(|p| p.file_name().is_some_and(|n| n == ".git"))
                    {
                        let _ = tx.send(());
                    }
                });
            match result {
                Ok(mut watcher) => {
                    use notify::Watcher as _;
                    match watcher.watch(&entry.path, notify::RecursiveMode::NonRecursive) {
                        Ok(()) => {
                            *lock(&entry.folder_watch) = Some(watcher);
                            // Close the check-to-attach gap: a `.git` change
                            // while unwatched gets caught by this recheck.
                            let _ = entry.kick_tx.send(());
                        }
                        Err(err) => {
                            tracing::debug!(path = %entry.path.display(), error = %err, "spaces: watch failed");
                        }
                    }
                }
                Err(err) => {
                    tracing::debug!(error = %err, "spaces: watcher create failed");
                }
            }
        });
    }
}

/// Per-space task: trailing-debounce kicks, then recheck git presence.
async fn entry_task(
    inner: Weak<SpacesSyncInner>,
    space_id: String,
    entry: Weak<SpaceEntry>,
    mut kick_rx: mpsc::UnboundedReceiver<()>,
) {
    while kick_rx.recv().await.is_some() {
        loop {
            match tokio::time::timeout(WATCH_DEBOUNCE, kick_rx.recv()).await {
                Ok(Some(())) => continue,
                Ok(None) => return, // entry closed mid-burst
                Err(_) => break,
            }
        }
        let (Some(inner), Some(entry)) = (inner.upgrade(), entry.upgrade()) else {
            return;
        };
        check_space(&inner, &space_id, &entry.path).await;
    }
}

/// Probe git presence and stamp the row — write only on change.
async fn check_space(inner: &Arc<SpacesSyncInner>, space_id: &str, path: &Path) {
    let detected = inner.repos.is_repo(path).await;
    let checkout_id = if detected {
        match inner.repos.checkout_identity(path).await {
            Ok(identity) => Some(identity.id),
            Err(err) => {
                tracing::debug!(space = %space_id, error = %err, "spaces: checkout identity failed");
                None
            }
        }
    } else {
        None
    };
    let current = match inner.workspace.read_spaces() {
        Ok(spaces) => spaces.into_iter().find(|s| s.id == space_id),
        Err(err) => {
            tracing::warn!(space = %space_id, error = %err, "spaces: row read failed");
            return;
        }
    };
    let Some(current) = current else {
        return; // deleted while checking
    };
    if current.git_detected == detected && current.checkout_id == checkout_id {
        return; // unchanged — no oplog growth
    }
    match inner
        .workspace
        .set_space_git(space_id, detected, checkout_id.as_deref())
    {
        Ok(_) => {
            tracing::info!(space = %space_id, git = detected, "space git presence updated");
        }
        Err(err) => {
            tracing::warn!(space = %space_id, error = %err, "spaces: git stamp failed");
        }
    }
}

/// Host-side repair: delete OUR chats whose `spaceId` dangles (create-vs-delete
/// race). Chats hosted by other devices are left alone.
fn sweep_orphans(inner: &Arc<SpacesSyncInner>) {
    let spaces = inner.workspace.watch_spaces().borrow().clone();
    let live: std::collections::HashSet<&str> = spaces.iter().map(|s| s.id.as_str()).collect();
    let chats = inner.workspace.watch_chats().borrow().clone();
    for chat in chats {
        if chat.device_id != inner.device_id {
            continue;
        }
        let Some(space_id) = chat.space_id.as_deref() else {
            continue;
        };
        if live.contains(space_id) {
            continue;
        }
        tracing::info!(chat = %chat.id, space = %space_id, "deleting orphaned chat (space gone)");
        if let Err(err) = inner.workspace.delete_chat(&chat.id) {
            tracing::warn!(chat = %chat.id, error = %err, "spaces: orphan delete failed");
        }
    }
}

/// Spaces-watch follower + repair tick. Weak handles so dropping the service
/// tears the loop down.
async fn spaces_task(inner: Weak<SpacesSyncInner>, mut spaces_rx: watch::Receiver<Vec<Space>>) {
    let mut repair = tokio::time::interval(REPAIR_INTERVAL);
    repair.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    repair.tick().await; // consume the immediate first tick
    {
        let Some(inner) = inner.upgrade() else { return };
        let spaces = spaces_rx.borrow().clone();
        reconcile(&inner, &spaces);
    }
    loop {
        tokio::select! {
            changed = spaces_rx.changed() => {
                if changed.is_err() {
                    break;
                }
                let Some(inner) = inner.upgrade() else { break };
                let spaces = spaces_rx.borrow_and_update().clone();
                reconcile(&inner, &spaces);
            }
            _ = repair.tick() => {
                let Some(inner) = inner.upgrade() else { break };
                let spaces = spaces_rx.borrow().clone();
                reconcile(&inner, &spaces);
                for entry in lock(&inner.entries).values() {
                    let _ = entry.kick_tx.send(());
                }
                sweep_orphans(&inner);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace_host::WorkspaceHostConfig;
    use comet_sync::DocsStore;

    fn workspace_host(dir: &Path) -> WorkspaceHost {
        let store = Arc::new(DocsStore::open(dir.join("local-store")).unwrap());
        WorkspaceHost::open(
            store,
            WorkspaceHostConfig {
                device_id: "dev-a".to_string(),
                device_name: "test-device".to_string(),
                platform: "test".to_string(),
            },
        )
        .unwrap()
    }

    /// Poll until `space_id` has an entry (created synchronously inside
    /// [`reconcile`], but reconcile itself can run on a delay via the
    /// spaces-watch task) and its folder watcher has attached (filled
    /// asynchronously off the runtime — see [`SpaceEntry::folder_watch`]).
    async fn wait_for_armed_entry(sync: &SpacesSync, space_id: &str) -> Arc<SpaceEntry> {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(entry) = lock(&sync.inner.entries).get(space_id).cloned()
                    && lock(&entry.folder_watch).is_some()
                {
                    return entry;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("space entry's folder watcher must attach within 5s")
    }

    /// D103's actual bug — a rare Windows kernel-level race between a
    /// deleting thread's `NtClose` and the watcher's own re-arming thread —
    /// needed WinDbg plus a dedicated stress campaign to catch even once
    /// (`docs/debt/D101-rpc-tests-watcher-delete-race.md`), and a plain
    /// "drop the watcher, then remove_dir_all" probe against an otherwise
    /// idle directory did not reproduce it in 50 tries in this repo's own
    /// investigation of this task (Windows tolerates deleting a path with an
    /// open handle when that handle was opened with `FILE_SHARE_DELETE`,
    /// which `notify`'s own `CreateFileW` call does — see
    /// `notify-7.0.0/src/windows.rs`'s `add_watch`, and which Rust's own
    /// `std::fs::File::create` also sets by default on Windows, confirmed by
    /// probing it directly — so a bare `File::create` cannot stand in for a
    /// held resource either). So this test does not try to force that exact
    /// kernel race. Instead it proves the same *structural* claim
    /// deterministically, using a resource this test fully controls: an
    /// event-handler callback that, on any filesystem event, opens a file
    /// in this same watched directory with `share_mode` explicitly excluding
    /// `FILE_SHARE_DELETE`, holds it briefly, then releases it — standing in
    /// for "work `SpaceEntry`'s own watcher thread has not finished yet."
    /// `notify`'s callback runs synchronously on the watcher's single
    /// background thread, so while it is inside that callback, no other
    /// action queued on that thread's channel (including
    /// `Action::Unwatch`/`Action::Stop`) can be serviced. A stop that only
    /// posts a request (`notify`'s own `Drop`/`unwatch`) returns before the
    /// callback releases the lock; a stop that genuinely fences through the
    /// same single-consumer queue cannot.
    ///
    /// Windows-only (`#[cfg(windows)]`): the property it exploits
    /// (`share_mode` excluding `FILE_SHARE_DELETE`) is a Windows-specific
    /// API and has no Linux equivalent, so this does not run — and would
    /// not prove anything if it did — on `ci.yml`'s `ubuntu-24.04` runner.
    /// Linux's own `notify` backend (`inotify.rs`'s `unwatch_inner`) already
    /// blocks its *caller* until the removal is acked, unlike Windows, so
    /// D103's underlying hazard is Windows-specific to begin with; see
    /// `spaces_sync_stop_watch_confirms_real_entry` below for the
    /// cross-platform test of the production wiring.
    #[cfg(windows)]
    #[tokio::test]
    async fn stop_watch_confirms_before_directory_is_removable() {
        use notify::Watcher as _;
        use std::os::windows::fs::OpenOptionsExt;

        const FILE_SHARE_READ: u32 = 0x1;
        const FILE_SHARE_WRITE: u32 = 0x2;

        let dir = tempfile::tempdir().unwrap();
        let locked_path = dir.path().join("locked-by-watcher-thread.txt");

        let locked_path_cb = locked_path.clone();
        let mut watcher =
            notify::recommended_watcher(move |event: Result<notify::Event, notify::Error>| {
                if event.is_ok() {
                    // No FILE_SHARE_DELETE: while this handle is open,
                    // deleting `locked_path` fails outright, no race needed.
                    let opened = std::fs::OpenOptions::new()
                        .write(true)
                        .create(true)
                        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
                        .open(&locked_path_cb);
                    if let Ok(file) = opened {
                        std::thread::sleep(Duration::from_millis(250));
                        drop(file);
                    }
                }
            })
            .unwrap();
        watcher
            .watch(dir.path(), notify::RecursiveMode::NonRecursive)
            .unwrap();

        // Fire an event and give it a moment to actually reach the
        // watcher's background thread — generous slack before the property
        // under test starts, not part of what's being measured.
        std::fs::write(dir.path().join("trigger.txt"), b"x").unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let entry = SpaceEntry {
            path: dir.path().to_path_buf(),
            kick_tx: mpsc::unbounded_channel().0,
            folder_watch: Mutex::new(Some(watcher)),
        };

        let confirmed = entry.stop_watch(Duration::from_secs(5)).await;
        assert!(confirmed, "stop_watch must confirm within its bound");

        // The property under test: once `stop_watch` has returned, the
        // watcher's background thread must be done with whatever it was
        // doing when the stop was requested — so `locked_path`, opened
        // without FILE_SHARE_DELETE inside that callback, must already be
        // releasable.
        std::fs::remove_file(&locked_path).expect(
            "file the watcher's thread held must be removable right after a confirmed stop",
        );
    }

    /// Wires the property above through the public surface: `SpacesSync`'s
    /// entry map and the pass-through `stop_watch` method, using a real
    /// space and the production event handler (not the adversarial one
    /// above — this just checks the plumbing, not the race).
    #[tokio::test]
    async fn spaces_sync_stop_watch_confirms_real_entry() {
        let data_dir = tempfile::tempdir().unwrap();
        let workspace = workspace_host(data_dir.path());
        let repos = Repos::new(data_dir.path(), "dev-a");
        let sync = SpacesSync::start(repos, workspace.clone(), "dev-a");

        let space_dir = tempfile::tempdir().unwrap();
        let space_path = space_dir.path().to_path_buf();
        workspace
            .create_space(
                "space-1",
                "dev-a",
                space_path.to_string_lossy().as_ref(),
                None,
                false,
            )
            .unwrap();

        let entry = wait_for_armed_entry(&sync, "space-1").await;
        assert_eq!(entry.path, space_path);

        assert!(sync.stop_watch("space-1", Duration::from_secs(5)).await);
        // Idempotent: nothing left armed the second time.
        assert!(sync.stop_watch("space-1", Duration::from_secs(5)).await);
        // Unknown space id: nothing to stop, trivially confirmed.
        assert!(
            sync.stop_watch("no-such-space", Duration::from_secs(5))
                .await
        );
    }
}
