# Remote Settings Hardening Round 2 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make desktop remote add durable across page lifecycle, recover watches across engine replacement, present secrets without persistent plaintext render state, and strictly validate endpoints.

**Architecture:** `AppState` owns the single-flight add coordinator and its task/result, while each remote settings page owns only its current views and retrying watch tasks. A dedicated masked input owns remote secret text in zeroizing storage. Protocol endpoint parsing validates IP literals and DNS labels before persistence or connection.

**Tech Stack:** Rust, GPUI entities/tasks, Tokio test synchronization, `zeroize`, Comet local RPC, serde, `std::net`.

## Global Constraints

- Strict RED→GREEN TDD for every behavior change.
- Never route local administration through a remote client.
- Never log or render pairing-secret plaintext by default.
- Page drop cancels watch loops but does not cancel an app-owned add operation.
- Do not edit the shared SDD ledger.

---

### Task 1: Durable app-owned remote add

**Files:**
- Modify: `crates/ui/src/remotes.rs`
- Modify: `crates/ui/src/state.rs`
- Test: `crates/ui/src/remotes.rs`

**Interfaces:**
- Produces: `RemoteAddCoordinator::{begin, finish, state}` with one-flight semantics and a public terminal result.
- Produces: an `AppState` task slot that owns `pair_and_persist_remote` independently of `RemoteConnectionsPage`.

- [ ] **Step 1: Write failing coordinator lifecycle tests**

Add a gated fake pairer/admin test that begins one operation, rejects a second, releases pairing, drops the page-level observer, and asserts exactly one `PUT_REMOTE` plus a retrievable `PartialSuccess` containing revoke/fresh-pairing recovery.

- [ ] **Step 2: Run the focused test and confirm missing coordinator/app ownership failure**

Run: `cargo test -p comet-ui durable_remote_add -- --nocapture`

- [ ] **Step 3: Implement the minimal coordinator and app-owned task**

The page calls an `AppState` method that performs:

```rust
if !self.remote_add.begin() { return Err(AddAlreadyInProgress); }
self.remote_add_task = Some(cx.spawn(async move |app, cx| {
    let result = pair_and_persist_remote(...).await;
    app.update(cx, |state, cx| state.remote_add.finish(result, cx)).ok();
}));
```

Store only public terminal details/errors. Log partial recovery without secret fields. Let a recreated page read coordinator state.

- [ ] **Step 4: Run focused tests to green**

Run: `cargo test -p comet-ui durable_remote_add -- --nocapture`

---

### Task 2: Stable destructive confirmation identity

**Files:**
- Modify/Test: `crates/ui/src/remotes.rs`

**Interfaces:**
- Produces: confirmation copy containing friendly name, stable `ServerId`, and consequence.

- [ ] **Step 1: Extend the duplicate-name confirmation test and run RED**

Create two revoke confirmations with the same name and different IDs; assert copy differs and each contains its ID.

Run: `cargo test -p comet-ui duplicate_names_have_distinct_revoke_confirmations -- --nocapture`

- [ ] **Step 2: Add stable ID to remove/revoke copy and run GREEN**

Run the same command and the existing cancel/confirm test.

---

### Task 3: Current-client watch retry and valid-frame backoff

**Files:**
- Modify/Test: `crates/ui/src/remotes.rs`

**Interfaces:**
- Produces: a retry loop that resolves `AppState.engine().client()` on every subscribe attempt.
- Produces: `WatchRecovery::valid_snapshot()` as the only backoff reset point.

- [ ] **Step 1: Write failing provider-swap and backoff sequence tests**

Use a provider fake returning client A then client B and assert retry uses B. Assert close/malformed cycles yield 250, 500, 1000, 2000, 4000, 4000 ms and subscribe success does not reset; valid snapshot resets to 250 ms.

- [ ] **Step 2: Run focused tests and confirm RED**

Run: `cargo test -p comet-ui watch_ -- --nocapture`

- [ ] **Step 3: Reacquire the client inside each loop and move reset to decoded frames**

Do not retain `EngineHandle` or `RpcClient` outside one subscribe/stream attempt. Keep task handles in the page vector.

- [ ] **Step 4: Run focused tests to green**

Run: `cargo test -p comet-ui watch_ -- --nocapture`

---

### Task 4: Zeroizing masked secret input and server-secret copy

**Files:**
- Modify/Test: `crates/ui/src/remotes.rs`
- Modify if needed: `crates/ui/src/composer.rs` only for reusable public key actions, never secret storage.

**Interfaces:**
- Produces: `SecretInput` with `Zeroizing<String>`, masked render text, direct replace/paste, `take_secret`, `clear`, redacted `Debug`, and zeroizing drop.
- Produces: server pairing copy action with non-secret bounded status and explicit system-clipboard warning.

- [ ] **Step 1: Write failing secret-model tests**

Assert debug and masked render omit plaintext, edits/paste modify the zeroizing buffer, `clear`/take empty it, and copy uses a short-lived zeroizing source while bounded status contains no secret.

- [ ] **Step 2: Run focused tests and confirm RED**

Run: `cargo test -p comet-ui secret_ -- --nocapture`

- [ ] **Step 3: Implement `SecretInput` and replace the composer field**

Implement only single-line input/selection behavior needed for ASCII Base32 secrets. Framework render strings contain bullets and grouping only. Add `Copy Secret`, expiration, full-authority warning, and clipboard persistence disclosure; no reveal state.

- [ ] **Step 4: Narrow array copying at the pairing boundary**

Move decoded bytes into `Zeroizing<[u8; 16]>` and expose a borrowed array until Task 4 `pair_client` consumes its zeroizing parameter.

- [ ] **Step 5: Run secret and remotes tests to green**

Run: `cargo test -p comet-ui remotes -- --nocapture`

---

### Task 5: Strict endpoint validation

**Files:**
- Modify/Test: `crates/proto/src/remote.rs`

**Interfaces:**
- Produces: `RemoteEndpoint::parse` accepting valid IPv4, DNS names, and bracketed `Ipv6Addr`, while rejecting malformed labels and ambiguous IPv6.

- [ ] **Step 1: Add failing malformed endpoint matrix**

Cover invalid bracketed IPv6, underscores, empty/overlong labels, leading/trailing hyphens, overlong host, malformed IPv4-like input, whitespace, and bare IPv6; retain valid localhost/DNS/IPv4/bracketed IPv6 cases.

- [ ] **Step 2: Run endpoint tests and confirm RED**

Run: `cargo test -p comet-proto endpoint -- --nocapture`

- [ ] **Step 3: Implement IP/DNS validation and run GREEN**

Use `Ipv6Addr::from_str`, `IpAddr::from_str`, total/label limits, ASCII label characters, and alphanumeric label edges.

---

### Task 6: Verification and report

**Files:**
- Modify: `.superpowers/sdd/2026-08-02-lan-only-direct-rpc/task-10-report.md`

- [ ] **Step 1: Run affected suites**

Run protocol, RPC, client, engine, UI library tests and engine remote-access integration tests.

- [ ] **Step 2: Run quality gates**

Run `cargo fmt --all -- --check`, `git diff --check`, and strict affected-package clippy; record any pre-existing blocker precisely.

- [ ] **Step 3: Audit requirements and update the report**

Document RED diagnostics, focused totals, lifecycle ownership, clipboard disclosure, process-exit residual risk, and final gates.

- [ ] **Step 4: Commit**

Commit all Task 10 round-2 implementation/report changes without modifying the ledger.
