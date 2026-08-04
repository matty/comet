# Merge Main into LAN-Only Branch Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Integrate `origin/main` v0.1.15 into `plan/lan-only-networking` while retaining all upstream product/performance work and preserving the LAN-only, direct, non-transitive authority model.

**Architecture:** Merge `origin/main` once and port overlapping upstream features into the branch's direct federation abstractions instead of restoring the former cloud/device-room forwarding. Resolve build/bootstrap first, then transport and engine semantics, then Desktop/TUI state, and regenerate the lockfile only after manifests and source compile. The merge produces one reviewed merge commit because Git cannot commit partial conflict groups while the index remains unmerged.

**Tech Stack:** Rust 2024, Tokio, GPUI, Cargo workspaces, Git merge, existing Comet engine/RPC/client/UI/TUI tests.

## Global Constraints

- Keep the branch's local authoritative engine, opt-in mutually authenticated LAN listener, hostname/IP+port remotes, and explicit non-transitive federation.
- Do not restore WorkOS, account gates, `targetDeviceId` relay forwarding, `DeviceRoom`, sync rooms, cloud migration, or hosted runtime dependencies.
- Retain upstream v0.1.15 features: native word editing, light mode, file mentions/search, document/transcript memory bounds, and shared loader clock.
- Every remote-sensitive cache, mutation, search, transcript, and attachment path remains qualified by `ServerId`/`ServerRef`; no selected-server fallback.
- Preserve cancellation semantics introduced by the LAN branch while adding upstream bounded/backpressured stream behavior.
- Keep release distribution separate and pinned to `matty/comet`.
- Resolve `Cargo.lock` by regeneration after all manifest conflicts are resolved.

---

### Task 1: Merge baseline, manifests, bootstrap, and deliberate deletions

**Files:**
- Modify: `Cargo.toml`
- Regenerate: `Cargo.lock`
- Modify: `apps/comet/src/main.rs`
- Delete/keep deleted: `crates/rpc/src/device_room.rs`, `crates/sync/src/room.rs`
- Inspect auto-merges: `apps/comet/Cargo.toml`, `apps/tui/Cargo.toml`, `crates/engine/Cargo.toml`, `crates/rpc/Cargo.toml`, `crates/sync/Cargo.toml`, `crates/ui/Cargo.toml`

**Interfaces:**
- Consumes: `origin/main` at v0.1.15 and current `plan/lan-only-networking` HEAD.
- Produces: one uncommitted merge index with version/dependency/bootstrap policy resolved and cloud room files still deleted.

- [ ] **Step 1: Stop only the running worktree binary and begin the merge**

```powershell
$exe = 'C:\dev\comet\.worktrees\lan-only-networking\target\debug\comet.exe'
Get-Process comet -ErrorAction SilentlyContinue |
  Where-Object Path -eq $exe |
  Stop-Process
git fetch origin main
git merge --no-commit --no-ff origin/main
```

Expected: Git reports the known 15 conflict paths and leaves the worktree in a merge state.

- [ ] **Step 2: Resolve workspace metadata as a union**

`Cargo.toml` must contain:

```toml
[workspace.package]
version = "0.1.15"

[workspace.dependencies]
mimalloc = "0.1"
ignore = "0.4"
# Retain all existing comet-client/comet-identity and TLS/pairing dependencies.
```

Retain the LAN branch's workspace members and all main members. Do not restore deleted cloud-only package dependencies.

- [ ] **Step 3: Resolve the headed binary bootstrap**

Keep the branch's `Command::{Headless,Status,Remote,Daemon,Update,Tui}` and local-only `main`. Add only main's allocator:

```rust
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;
```

Do not retain `DEFAULT_EDGE_URL`, WorkOS helpers, login/logout commands, organization gates, or edge bearer configuration.

- [ ] **Step 4: Keep cloud room transports deleted**

```powershell
git rm -- crates/rpc/src/device_room.rs crates/sync/src/room.rs
```

Confirm no surviving module declaration or dependency requires them:

```powershell
rg -n "device_room|DeviceRoom|sync::room|RoomClient|targetDeviceId" crates apps
```

Expected: only intentional negative-regression/documentation references, not runtime imports or modules.

- [ ] **Step 5: Stage resolved baseline files without committing**

```powershell
git add Cargo.toml apps/comet/src/main.rs apps/comet/Cargo.toml apps/tui/Cargo.toml \
  crates/engine/Cargo.toml crates/rpc/Cargo.toml crates/sync/Cargo.toml crates/ui/Cargo.toml
git status --short
```

Expected: those paths are no longer unmerged; `Cargo.lock` and subsystem source conflicts remain.

---

### Task 2: Integrate engine, RPC, client, and TUI transport semantics

**Files:**
- Modify: `crates/engine/src/doc_host.rs`
- Modify: `crates/engine/src/rpc.rs`
- Modify: `crates/rpc/src/client.rs`
- Modify: `crates/tui/src/link.rs`
- Inspect auto-merges: `crates/doc/src/transcript_delta.rs`, `crates/doc/src/lib.rs`, `crates/rpc/src/lib.rs`, `crates/tui/src/app.rs`
- Test: `crates/rpc/tests/cancellation.rs`, `crates/rpc/tests/secure_lan.rs`, `crates/engine/tests/remote_access.rs`, `crates/client/tests/non_transitive.rs`

**Interfaces:**
- Consumes: LAN branch `RpcStream`, `Federation`, `ServerRef`, `LocalRpcService`, `RemoteRpcService` and upstream `TranscriptFrame`, `SearchFiles`, LRU/backpressure behavior.
- Produces: cancellation-safe bounded streams, direct-server file search/transcript deltas, and memory-bounded document hosting without cloud rooms.

- [ ] **Step 1: Port document LRU and lazy transcript mirrors without room state**

Bring over upstream `WARM_DOC_CAP`, snapshot/resident accounting, dirty mirrors, watcher-aware publishing, purge, and eviction. `ChatDocHandle` remains cloud-free:

```rust
pub struct ChatDocHandle {
    chat_id: String,
    device_id: String,
    doc: Arc<SessionDoc>,
    messages_tx: watch::Sender<TranscriptFrame>,
    mirror_dirty: AtomicBool,
    last_access: AtomicI64,
    snapshot_bytes: AtomicUsize,
    _sub: loro::Subscription,
}
```

Do not add `RoomClient`, room joins, link caches, or edge nudges. Adapt the exact sender type to the upstream `transcript_delta` API present after auto-merge.

- [ ] **Step 2: Combine file search and transcript deltas with LAN authority**

In `EngineRpc`, retain branch LAN types/methods and add upstream `FileSearchParams`, `tool_file_path`, `file_search_root`, `SEARCH_FILES`, and delta watch construction. File search must reject a workspace row not owned by this authoritative engine. Keep:

```rust
pub(crate) fn owns_remote_chat(&self, chat_id: &str, local_device_id: &str) -> bool
```

Delete/omit upstream `forwardable`, `targetDeviceId`, and `LinkCache` forwarding. Add `SEARCH_FILES` to the explicit remote allowlist only when its ownership checks pass through the authoritative engine.

- [ ] **Step 3: Combine bounded streams with writer-owned cancellation**

Keep `RpcStream` and its drop-triggered cancellation guard. Replace its internal stream channel with a bounded sender/receiver and ensure the reader task never holds the pending-map mutex across `.await`:

```rust
let tx = match shared.lock().get(&id) {
    Some(Pending::Stream(tx)) => Some(tx.clone()),
    _ => None,
};
let dead = match tx {
    Some(tx) => tx.send(item).await.is_err(),
    None => false,
};
```

Cancellation must remain ordered through the writer and must not block `Drop`.

- [ ] **Step 4: Adapt TUI transcript deltas to federation**

Keep `run_federation` and `FederationCommand` routing. Apply upstream `TranscriptFrame` handling to the selected qualified `ServerRef`; a stale frame from another owner cannot replace the current transcript. Reconnect remains per direct server and never discovers B's C.

- [ ] **Step 5: Stage transport resolutions and run focused tests**

```powershell
git add crates/engine/src/doc_host.rs crates/engine/src/rpc.rs crates/rpc/src/client.rs crates/tui/src/link.rs
cargo test -p comet-rpc --test cancellation
cargo test -p comet-rpc --test secure_lan
cargo test -p comet-engine --test remote_access
cargo test -p comet-client --test non_transitive
cargo test -p comet-tui --lib
```

Expected: cancellation, TLS/allowlist, ownership, non-transitivity, and TUI delta tests pass.

---

### Task 3: Integrate Desktop light mode, file mentions, caches, and federation

**Files:**
- Modify: `crates/ui/src/attachments.rs`
- Modify: `crates/ui/src/changes.rs`
- Modify: `crates/ui/src/composer.rs`
- Modify: `crates/ui/src/shell.rs`
- Modify: `crates/ui/src/state.rs`
- Modify: `crates/ui/src/transcript.rs`
- Inspect auto-merges: `crates/ui/src/appearance.rs`, `crates/ui/src/settings/appearance.rs`, `crates/ui/src/theme.rs`, `crates/ui/src/loaders.rs`, `crates/ui/src/motion.rs`, `crates/ui/src/settings.rs`
- Test: inline UI unit tests and `cargo test -p comet-ui --lib`

**Interfaces:**
- Consumes: branch `ServerClient`, `ServerRef`, `Federation`; upstream Appearance/light theme, `FileSearchMatch`, mention projection, image LRU, `TranscriptFrame`.
- Produces: server-qualified upstream UI features with no account gate or cloud-routing fallback.

- [ ] **Step 1: Merge attachment LRU with server-qualified invalidation**

The cache key remains:

```rust
(ServerId, String /* device */, String /* path */)
```

Retain branch offline generations and `mark_server_offline` behavior. Add upstream byte-budget/LRU eviction and gpui decoded-image eviction. A late load from an old generation must not repopulate the cache.

- [ ] **Step 2: Port file mentions through the owning server**

Keep composer drafts/attachments keyed by `Option<ServerRef>`. Add upstream mention state and projection, but call `SEARCH_FILES` through the explicit owner/current `ServerClient`; results carry the same server qualification as the draft/chat. Never fall back to a newly selected server if the original owner disconnects.

- [ ] **Step 3: Merge shell settings and mutation behavior**

`SettingsSection::ALL` includes both:

```rust
SettingsSection::RemoteConnections,
SettingsSection::Appearance,
```

Keep the branch's direct bottom Settings button, no avatar/account popover, no login/org gates, and `mutate_for(ServerRef, ...)`. Incorporate upstream composer purge before owner-qualified deletion and light-mode theme calls. Keep the label `Remote`.

- [ ] **Step 4: Apply transcript deltas per server-qualified chat**

In `AppState`, store/apply transcript frames only for the captured `ServerRef`. A desync resubscribes that owner's client. Combine upstream mention-chip transcript rendering and theme/memory changes with branch `transcript_owner_changed` invalidation.

- [ ] **Step 5: Stage UI resolutions and run UI tests**

```powershell
git add crates/ui/src/attachments.rs crates/ui/src/changes.rs crates/ui/src/composer.rs \
  crates/ui/src/shell.rs crates/ui/src/state.rs crates/ui/src/transcript.rs
cargo test -p comet-ui --lib
```

Expected: light/dark appearance, file mentions, server-qualified attachments/drafts/transcripts, settings routing, and owner-bound mutations pass.

---

### Task 4: Regenerate, verify, commit, and update the PR

**Files:**
- Regenerate: `Cargo.lock`
- Include: every clean auto-merge from `origin/main`
- Update remote branch: `origin/plan/lan-only-networking`

**Interfaces:**
- Consumes: fully resolved merge index.
- Produces: one merge commit whose first parent is the LAN branch and second parent is `origin/main`, with PR #1 mergeable.

- [ ] **Step 1: Regenerate the lockfile and clear all unmerged paths**

```powershell
cargo generate-lockfile
git add Cargo.lock
git diff --name-only --diff-filter=U
```

Expected: the unmerged-path command prints nothing.

- [ ] **Step 2: Run format and affected package gates**

```powershell
cargo fmt --all -- --check
cargo test -p comet-proto
cargo test -p comet-rpc
cargo test -p comet-client
cargo test -p comet-engine --lib
cargo test -p comet-engine --test remote_access
cargo test -p comet-tui --lib
cargo test -p comet-ui --lib
cargo test -p comet-update
cargo test -p comet --bin comet
git diff --check
```

Expected: all focused gates pass. Record any bounded Windows linker limitation without claiming workspace-wide green.

- [ ] **Step 3: Audit forbidden cloud runtime remnants**

```powershell
rg -n "COMET_EDGE_|WORKOS|targetDeviceId|DeviceRoom|SessionRoom|comet login|comet migrate" crates apps
```

Expected: only intentional negative regressions/provider-account documentation; no Comet hosted authority path.

- [ ] **Step 4: Commit the merge and push**

```powershell
git commit -m "Merge main into LAN-only networking"
git push origin plan/lan-only-networking
gh pr view 1 --json mergeable,mergeStateStatus,url
```

Expected: PR #1 reports a non-conflicting merge state.
