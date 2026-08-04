# LAN-Only Direct RPC Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remove Comet's hosted runtime dependency and let desktop and TUI connect directly, securely, and non-transitively to explicitly configured Comet engines on the local network.

**Architecture:** Each engine remains authoritative for its own local store and exposes a new opt-in TLS LAN listener through a default-deny RPC wrapper. A shared `comet-client` crate maintains direct local/remote connections for desktop and TUI; it never asks one engine to proxy another. Existing cloud auth, room sync, device relays, and R2 runtime paths are removed only after local migration, secure transport, and both clients are verified.

**Tech Stack:** Rust 2024, Tokio, rustls 0.23/TLS 1.3, tokio-rustls, rcgen Ed25519 certificates, HMAC-SHA-256 pairing confirmation, serde JSON RPC, Loro local documents, SQLite, GPUI, Ratatui, TypeScript/Cloudflare Worker for release distribution only.

## Global Constraints

- Desktop and TUI are the only client surfaces in this implementation; do not modify the iOS app beyond documenting that it is unsupported.
- Remotes are configured manually as `hostname-or-ip:port`; do not add mDNS, Bonjour, broadcast, or registry discovery.
- Remote listening is opt-in and disabled by default.
- Connections are direct and non-transitive: A-to-B and B-to-C must never give A visibility of or access to C.
- Keep localhost IPC and LAN RPC as separate listeners and separate service objects.
- Never bind the unrestricted `EngineRpc` service to a LAN interface.
- Each engine is authoritative only for its own spaces, chats, sessions, repositories, terminals, uploads, and agents.
- Do not persist or reconcile remote content; retain only the configured server entry while it is offline.
- Use a persistent Ed25519 identity, TLS 1.3 mutual authentication, a pinned SHA-256 public-key fingerprint, and a single-use 128-bit pairing secret that authenticates the full handshake transcript with HMAC-SHA-256.
- The first release has one paired-client permission level with full operational control through the restricted LAN allowlist.
- Keep installer, release artifact, and update delivery separate under `COMET_RELEASES_URL`; local and LAN operation must not depend on that endpoint.
- Preserve legacy stores untouched during migration and remove `session.json` only after the new local store is verified and marked complete.
- Follow test-driven development and commit after every task.

## Planned File Structure

### Shared protocol and client federation

- `crates/proto/src/remote.rs`: serializable server IDs, endpoint/configuration rows, connection states, protocol/capability handshake types, and scoped resource references.
- `crates/client/src/lib.rs`: public federation API shared by GPUI and TUI.
- `crates/client/src/manager.rs`: one supervisor per configured direct connection and aggregate event routing.
- `crates/client/src/server.rs`: subscriptions and commands for one authoritative engine.
- `crates/client/src/model.rs`: server-scoped snapshots and events; raw entity IDs remain local inside each server bucket.
- `crates/client/tests/non_transitive.rs`: A/B/C direct-connection behavior.

### Secure transport

- `crates/identity/src/lib.rs`: small shared persistent-identity crate used by the engine and both viewport clients.
- `crates/identity/src/permissions.rs`: platform-specific private-file permissions.
- `crates/rpc/src/tls.rs`: pinned TLS client/server configuration and TLS WebSocket connect/accept.
- `crates/rpc/src/pairing.rs`: transcript construction, HMAC confirmation, and `/pair` handshake frames.
- `crates/rpc/tests/secure_lan.rs`: identity pinning, pairing, revocation, and malformed-frame tests.

### Engine authority and configuration

- `crates/engine/src/local_store.rs`: legacy-profile selection and transactional local-authoritative migration.
- `crates/engine/src/remote_config.rs`: atomic LAN settings, outbound registry, trusted-client allowlist, and watch streams.
- `crates/engine/src/remote_rpc.rs`: default-deny LAN RPC wrapper and local-ownership validation.
- `crates/engine/src/lan_server.rs`: opt-in listener lifecycle, TLS accept loop, pairing-session lifecycle, and revocation disconnects.
- `crates/engine/tests/local_store_migration.rs`: migration fixtures and rollback/idempotence cases.
- `crates/engine/tests/remote_access.rs`: listener/admin/allowlist integration tests.

### Product surfaces and cleanup

- `crates/ui/src/remotes.rs`: desktop Remote Connections settings UI.
- `crates/ui/src/state.rs`: server-bucketed GPUI state and scoped selection/routing.
- `crates/tui/src/link.rs`: replace the single-engine supervisor with shared federation events/commands.
- `crates/tui/src/app.rs` and `crates/tui/src/render.rs`: server grouping and offline rows.
- `apps/comet/src/remote_cli.rs`: `comet remote add|list|remove|listen|pair|clients|revoke` administration.
- `apps/comet/src/main.rs`: local-only startup, CLI removal/addition, and release URL configuration.
- `edge/src/index.ts`, `edge/src/env.ts`, and `edge/wrangler.jsonc`: distribution-only Worker.

---

### Task 1: Add remote protocol types and method names

**Files:**
- Create: `crates/proto/src/remote.rs`
- Modify: `crates/proto/src/lib.rs`
- Modify: `crates/rpc/src/lib.rs`
- Test: `crates/proto/src/remote.rs`

**Interfaces:**
- Produces: `ServerId`, `ServerRef`, `RemoteEndpoint`, `RemoteEntry`, `RemoteConnectionState`, `LanSettings`, `TrustedClient`, `ServerHello`, `PROTOCOL_VERSION`, and localhost-only remote administration method constants.
- Consumes: existing `serde`, `chrono`, and RPC envelope conventions.

- [ ] **Step 1: Write failing protocol round-trip and validation tests**

```rust
#[test]
fn endpoint_requires_host_and_nonzero_port() {
    assert!(RemoteEndpoint::parse("host.local:27655").is_ok());
    assert!(RemoteEndpoint::parse("192.168.1.20:27655").is_ok());
    assert!(RemoteEndpoint::parse("host.local:0").is_err());
    assert!(RemoteEndpoint::parse("https://host.local:27655").is_err());
}

#[test]
fn server_refs_do_not_collide() {
    let a = ServerRef::new(ServerId::new("sha256:a"), "chat-1");
    let b = ServerRef::new(ServerId::new("sha256:b"), "chat-1");
    assert_ne!(a, b);
    assert_eq!(a.local_id(), "chat-1");
}

#[test]
fn connection_state_wire_names_are_stable() {
    assert_eq!(
        serde_json::to_value(RemoteConnectionState::IdentityChanged).unwrap(),
        serde_json::json!("identityChanged")
    );
}
```

- [ ] **Step 2: Run the focused tests and confirm they fail because the types do not exist**

Run: `cargo test -p comet-proto remote -- --nocapture`

Expected: compilation fails with unresolved `RemoteEndpoint`, `ServerRef`, and `RemoteConnectionState`.

- [ ] **Step 3: Implement the protocol types and parser**

```rust
pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ServerId(String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerRef {
    pub server_id: ServerId,
    pub local_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteEndpoint {
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteEntry {
    pub server_id: ServerId,
    pub endpoint: RemoteEndpoint,
    pub name: String,
    pub pinned_spki_sha256: String,
    pub protocol_version: u32,
    pub last_state: RemoteConnectionState,
    pub created_at: DateTime<Utc>,
    pub last_connected_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LanSettings {
    pub enabled: bool,
    pub bind: std::net::SocketAddr,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrustedClient {
    pub server_id: ServerId,
    pub name: String,
    pub pinned_spki_sha256: String,
    pub paired_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RemoteConnectionState {
    Connecting,
    Online,
    Offline,
    Unreachable { message: String },
    IdentityChanged,
    IncompatibleVersion { remote: u32 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerHello {
    pub protocol_version: u32,
    pub server_id: ServerId,
    pub device_id: String,
    pub name: String,
    pub capabilities: Vec<String>,
}
```

Add RPC constants `SERVER_HELLO`, `WATCH_REMOTES`, `PUT_REMOTE`, `REMOVE_REMOTE`, `REPORT_REMOTE_STATUS`, `GET_LAN_SETTINGS`, `SET_LAN_SETTINGS`, `BEGIN_PAIRING`, `WATCH_TRUSTED_CLIENTS`, and `REVOKE_TRUSTED_CLIENT` to `comet_rpc::methods`.

- [ ] **Step 4: Run protocol tests and RPC crate tests**

Run: `cargo test -p comet-proto remote && cargo test -p comet-rpc --lib`

Expected: all tests pass.

- [ ] **Step 5: Commit the protocol boundary**

```bash
git add crates/proto/src/remote.rs crates/proto/src/lib.rs crates/rpc/src/lib.rs
git commit -m "feat: define direct remote protocol"
```

### Task 2: Build the conservative local-store migration

**Scope correction:** Cloud-to-local migration is no longer a product path.
There is no manual profile selector or recovery command. The implementation
steps below are retained as historical context for code already landed, but
Task 12 must audit that legacy selection/copy/marker code for removal while
preserving a valid local-store startup path.

**Files:**
- Create: `crates/engine/src/local_store.rs`
- Create: `crates/engine/tests/local_store_migration.rs`
- Modify: `crates/engine/src/lib.rs`
- Modify: `crates/doc/src/workspace.rs`
- Modify: `crates/sync/src/store.rs`

**Interfaces:**
- Produces: `prepare_local_store(data_dir: &Path, selected: Option<&LegacyProfile>) -> Result<LocalStore, EngineError>` and `WorkspaceDoc::owned_by(device_id: &str) -> Result<WorkspaceDoc, DocError>`.
- Produces: `LocalStore { root: PathBuf, migrated_from: Option<PathBuf> }`.
- Consumes: the legacy `session.json` shape, `device-id`, `DocsStore`, `RunJournal`, and `WORKSPACE_DOC_ID`.

- [ ] **Step 1: Write migration tests for profile selection and foreign-row filtering**

```rust
#[test]
fn session_selects_legacy_profile_and_filters_other_devices() {
    let fixture = LegacyFixture::new();
    fixture.write_session("org-a", "user-a");
    fixture.write_workspace(&[
        space("space-local", "device-local"),
        space("space-foreign", "device-foreign"),
    ]);

    let local = prepare_local_store(fixture.root(), None).unwrap();
    let state = fixture.read_migrated_workspace(&local);
    assert_eq!(state.spaces.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), ["space-local"]);
    assert!(fixture.legacy_store().exists());
    assert!(!fixture.root().join("session.json").exists());
}

#[test]
fn ambiguous_profiles_fail_without_creating_marker() {
    let fixture = LegacyFixture::with_profiles(&[("org-a", "user-a"), ("org-b", "user-b")]);
    let err = prepare_local_store(fixture.root(), None).unwrap_err().to_string();
    assert!(err.contains("comet migrate --from"));
    assert!(!fixture.root().join("local-store/migration-complete.json").exists());
}

#[test]
fn explicit_profile_resolves_ambiguity_without_merging() {
    let fixture = LegacyFixture::with_profiles(&[("org-a", "user-a"), ("org-b", "user-b")]);
    let selected = LegacyProfile { org_id: "org-b".into(), user_id: "user-b".into() };
    let local = prepare_local_store(fixture.root(), Some(&selected)).unwrap();
    assert_eq!(local.migrated_from.as_deref(), Some(fixture.profile("org-b", "user-b").as_path()));
    assert!(fixture.profile("org-a", "user-a").exists());
}

#[test]
fn completed_migration_is_idempotent() {
    let fixture = LegacyFixture::single_profile("org-a", "user-a");
    let first = prepare_local_store(fixture.root(), None).unwrap();
    let second = prepare_local_store(fixture.root(), None).unwrap();
    assert_eq!(first.root, second.root);
}
```

- [ ] **Step 2: Run the migration test and verify failure**

Run: `cargo test -p comet-engine --test local_store_migration -- --nocapture`

Expected: compilation fails because `local_store` and the migration helpers are absent.

- [ ] **Step 3: Add store enumeration/copy primitives and owned workspace projection**

Add these focused APIs rather than exposing SQLite connections:

```rust
impl DocsStore {
    pub fn snapshot_ids(&self) -> Result<Vec<String>, StoreError>;
    pub fn copy_snapshots_to(&self, destination: &DocsStore) -> Result<(), StoreError>;
}

impl WorkspaceDoc {
    pub fn owned_by(&self, device_id: &str) -> Result<Self, DocError> {
        let state = self.read_all()?;
        let owned_spaces: std::collections::HashSet<_> = state.spaces.iter()
            .filter(|space| space.device_id == device_id)
            .map(|space| space.id.clone())
            .collect();
        let mut out = WorkspaceDoc::new();
        for device in state.devices.iter().filter(|row| row.id == device_id) {
            out.upsert_device(device)?;
        }
        for space in state.spaces.iter().filter(|row| row.device_id == device_id) {
            out.upsert_space(space)?;
        }
        for chat in state.chats.iter().filter(|row| {
            row.device_id == device_id && row.space_id.as_ref().is_none_or(|id| owned_spaces.contains(id))
        }) {
            out.upsert_chat(chat)?;
        }
        for session in state.sessions.iter().filter(|row| row.device_id == device_id) {
            out.upsert_session(session)?;
        }
        Ok(out)
    }
}
```

- [ ] **Step 4: Implement staged migration and atomic marker semantics**

Use `local-store.staging`, copy the selected profile's snapshots and journals, replace the workspace snapshot with `owned_by(device_id)`, open/verify the staged store, rename it to `local-store`, atomically write `migration-complete.json`, then remove `session.json`. Repositories, uploads, agent-account configuration, and `device-id` already live at the data-directory root and remain in place. Never mutate or rename a directory under `orgs/`.

The public result must be:

```rust
#[derive(Debug, Clone)]
pub struct LocalStore {
    pub root: PathBuf,
    pub migrated_from: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyProfile {
    pub org_id: String,
    pub user_id: String,
}

pub fn prepare_local_store(
    data_dir: &Path,
    selected: Option<&LegacyProfile>,
) -> Result<LocalStore, EngineError>;
```

- [ ] **Step 5: Run migration and existing workspace/store tests**

Run: `cargo test -p comet-engine --test local_store_migration && cargo test -p comet-doc workspace && cargo test -p comet-sync store`

Expected: all tests pass; the ambiguous-profile test contains the exact recovery command.

- [ ] **Step 6: Commit the migration**

```bash
git add crates/engine/src/local_store.rs crates/engine/tests/local_store_migration.rs crates/engine/src/lib.rs crates/doc/src/workspace.rs crates/sync/src/store.rs
git commit -m "feat: migrate cloud profiles to local authority"
```

### Task 3: Add persistent device identity and atomic remote configuration

**Files:**
- Create: `crates/identity/Cargo.toml`
- Create: `crates/identity/src/lib.rs`
- Create: `crates/identity/src/permissions.rs`
- Create: `crates/engine/src/remote_config.rs`
- Modify: `crates/engine/src/lib.rs`
- Modify: `crates/engine/Cargo.toml`
- Modify: `Cargo.toml`
- Test: `crates/identity/src/lib.rs`
- Test: `crates/engine/src/remote_config.rs`

**Interfaces:**
- Produces: `DeviceIdentity::load_or_create`, `RemoteConfigStore::open`, `watch_remotes`, `watch_trusted_clients`, `lan_settings`, `put_remote`, `remove_remote`, `trust_client`, and `revoke_client`.
- Consumes: Task 1 protocol types and the engine data directory.

- [ ] **Step 1: Write failing identity/config tests**

```rust
#[test]
fn identity_is_stable_and_private_key_is_not_serialized_in_config() {
    let dir = tempfile::tempdir().unwrap();
    let first = DeviceIdentity::load_or_create(dir.path()).unwrap();
    let second = DeviceIdentity::load_or_create(dir.path()).unwrap();
    assert_eq!(first.server_id(), second.server_id());
    let config = std::fs::read_to_string(dir.path().join("remote-access.json")).unwrap_or_default();
    assert!(!config.contains(&first.private_key_base64_for_test()));
}

#[test]
fn listening_defaults_off_and_writes_are_atomic() {
    let dir = tempfile::tempdir().unwrap();
    let store = RemoteConfigStore::open(dir.path()).unwrap();
    assert!(!store.lan_settings().enabled);
    store.set_lan_settings(LanSettings { enabled: true, bind: "0.0.0.0:27655".parse().unwrap() }).unwrap();
    assert!(RemoteConfigStore::open(dir.path()).unwrap().lan_settings().enabled);
    assert!(!dir.path().join("remote-access.json.tmp").exists());
}
```

- [ ] **Step 2: Run tests and verify missing-module failures**

Run: `cargo test -p comet-identity && cargo test -p comet-engine remote_config -- --nocapture`

Expected: compilation fails because both modules are absent.

- [ ] **Step 3: Add explicit crypto dependencies and implement identity creation**

Add workspace dependencies `rustls = "0.23"`, `rustls-pki-types = "1"`, `tokio-rustls = "0.26"`, `rcgen = { version = "0.14", features = ["crypto", "pem", "zeroize"] }`, `hmac = "0.12"`, `rand = "0.10"`, `data-encoding = "2"`, `subtle = "2"`, and `zeroize = "1"`. Add a target-specific `windows-sys` dependency with the Win32 security and filesystem features used by `permissions.rs`.

Implement:

```rust
pub struct DeviceIdentity {
    server_id: ServerId,
    certificate_der: Vec<u8>,
    private_key_der: zeroize::Zeroizing<Vec<u8>>,
}

impl DeviceIdentity {
    pub fn load_or_create(data_dir: &Path) -> Result<Arc<Self>, EngineError>;
    pub fn server_id(&self) -> &ServerId;
    pub fn certificate_der(&self) -> &[u8];
    pub fn private_key_der(&self) -> &[u8];
}
```

Write `device-identity.pem` atomically. `permissions.rs` must create it as Unix mode `0600`; on Windows it must install a DACL granting the current user and `SYSTEM` access and denying inherited broad write access. Test the Unix mode directly and put the Windows ACL assertion behind `#[cfg(target_os = "windows")]`.

- [ ] **Step 4: Implement versioned remote configuration and watches**

Persist a single versioned JSON document containing `lanSettings`, `remotes`, and `trustedClients`. Keep active pairing sessions in memory only. Publish full snapshots through `tokio::sync::watch` after successful atomic replacement.

- [ ] **Step 5: Run identity/config tests and inspect permissions**

Run: `cargo test -p comet-identity && cargo test -p comet-engine remote_config`

Expected: all tests pass; on Unix the identity test observes mode `0600`.

- [ ] **Step 6: Commit identity and configuration storage**

```bash
git add Cargo.toml Cargo.lock crates/identity crates/engine/Cargo.toml crates/engine/src/lib.rs crates/engine/src/remote_config.rs
git commit -m "feat: persist remote trust and device identity"
```

### Task 4: Implement authenticated pairing and pinned TLS transport

**Files:**
- Create: `crates/rpc/src/tls.rs`
- Create: `crates/rpc/src/pairing.rs`
- Create: `crates/rpc/tests/secure_lan.rs`
- Modify: `crates/rpc/src/lib.rs`
- Modify: `crates/rpc/src/server.rs`
- Modify: `crates/rpc/Cargo.toml`

**Interfaces:**
- Produces: `TlsIdentity`, `PinnedServer`, `connect_lan_rpc`, `accept_lan_rpc`, `PairingSession`, `pair_client`, and `serve_pairing`.
- Consumes: Task 1 `ServerId`/handshake types and Task 3 DER identity material.

- [ ] **Step 1: Write failing transcript, pin, and revocation tests**

```rust
#[test]
fn pairing_confirmation_binds_both_keys_and_nonces() {
    let transcript = PairingTranscript::new(&server_cert, &client_key, [1; 32], [2; 32]);
    let tag = transcript.confirm_client(&secret);
    assert!(transcript.verify_client(&secret, &tag));
    let substituted = PairingTranscript::new(&other_server_cert, &client_key, [1; 32], [2; 32]);
    assert!(!substituted.verify_client(&secret, &tag));
}

#[tokio::test]
async fn pinned_client_rejects_changed_server_identity() {
    let server = SecureFixture::spawn().await;
    let pin = server.server_id();
    assert!(connect_lan_rpc(server.endpoint(), &client_identity(), &pin).await.is_ok());
    server.rotate_identity().await;
    assert!(matches!(
        connect_lan_rpc(server.endpoint(), &client_identity(), &pin).await,
        Err(LanConnectError::IdentityChanged)
    ));
}

#[test]
fn pairing_limiter_allows_five_failures_per_source_per_minute() {
    let now = Instant::now();
    let mut limiter = PairingLimiter::default();
    for _ in 0..5 {
        assert!(limiter.record_failure("192.168.1.9".parse().unwrap(), now).is_allowed());
    }
    assert!(limiter.record_failure("192.168.1.9".parse().unwrap(), now).is_limited());
    assert!(limiter.record_failure("192.168.1.10".parse().unwrap(), now).is_allowed());
}
```

- [ ] **Step 2: Run secure transport tests and confirm failure**

Run: `cargo test -p comet-rpc --test secure_lan -- --nocapture`

Expected: compilation fails because secure transport APIs do not exist.

- [ ] **Step 3: Implement the exact pairing transcript and constant-time verification**

```rust
const CLIENT_DOMAIN: &[u8] = b"comet-pair-v1/client";
const SERVER_DOMAIN: &[u8] = b"comet-pair-v1/server";

fn confirmation(secret: &[u8; 16], domain: &[u8], transcript: &[u8]) -> [u8; 32] {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("fixed key length");
    mac.update(domain);
    mac.update(transcript);
    mac.finalize().into_bytes().into()
}
```

Construct transcript bytes with length-prefixed server fingerprint, client fingerprint, server nonce, and client nonce. Compare tags using `subtle::ConstantTimeEq`. Pairing secrets expire after five minutes and are consumed after the first successful verification. Add a per-source-IP limiter allowing five failed confirmations in a rolling minute; a sixth attempt closes without evaluating another HMAC. Log source address and outcome, never the secret or confirmation bytes.

- [ ] **Step 4: Implement TLS 1.3 pinned verifiers and WebSocket upgrade paths**

Use rustls custom certificate verifiers that compare the SHA-256 SubjectPublicKeyInfo fingerprint to the configured pin/allowlist. The single-port server requests but does not make a client certificate mandatory at the TLS layer. A `/pair` client omits a TLS client certificate and supplies its public certificate inside the transcript-authenticated pairing frame; a `/rpc` client supplies its identity certificate, and the HTTP upgrade callback rejects the path unless that peer fingerprint is currently allowlisted. The pairing client temporarily accepts B's self-signed server certificate but pins it only after B's HMAC confirmation; normal `/rpc` clients require the saved pin during TLS verification. Set `MAX_LAN_TEXT_FRAME_BYTES` to 8 MiB in the LAN WebSocket configuration; attachment RPC already chunks its 32 MiB file limit into smaller frames. Keep existing plaintext `connect_ws` and `serve_ws_listener` for localhost IPC.

- [ ] **Step 5: Add frame-size and failed-auth tests**

Assert that an unauthenticated `/rpc`, an inactive `/pair`, a malformed pairing frame, and a WebSocket text frame over the configured maximum are closed before `RpcService::handle` is called.

- [ ] **Step 6: Run RPC tests**

Run: `cargo test -p comet-rpc --test secure_lan && cargo test -p comet-rpc --lib`

Expected: all secure and existing localhost transport tests pass.

- [ ] **Step 7: Commit secure transport**

```bash
git add crates/rpc/Cargo.toml crates/rpc/src/lib.rs crates/rpc/src/server.rs crates/rpc/src/tls.rs crates/rpc/src/pairing.rs crates/rpc/tests/secure_lan.rs
git commit -m "feat: add paired TLS LAN transport"
```

### Task 5: Add the default-deny authoritative LAN RPC service

**Files:**
- Create: `crates/engine/src/remote_rpc.rs`
- Create: `crates/engine/tests/remote_access.rs`
- Modify: `crates/engine/src/lib.rs`
- Modify: `crates/engine/src/rpc.rs`

**Interfaces:**
- Produces: `RemoteRpcService::new(inner, local_device_id)` and `remote_method_allowed(method) -> bool`.
- Consumes: `EngineRpc`, Task 1 methods, and the receiving engine's local `device_id`.

- [ ] **Step 1: Write an exhaustive allowlist test**

```rust
#[test]
fn administrative_and_proxy_methods_are_denied() {
    for method in [
        methods::WATCH_REMOTES,
        methods::PUT_REMOTE,
        methods::REMOVE_REMOTE,
        methods::REPORT_REMOTE_STATUS,
        methods::GET_LAN_SETTINGS,
        methods::SET_LAN_SETTINGS,
        methods::BEGIN_PAIRING,
        methods::WATCH_TRUSTED_CLIENTS,
        methods::REVOKE_TRUSTED_CLIENT,
        methods::APPLY_UPDATE,
        methods::START_AGENT_LOGIN,
        methods::COMPLETE_AGENT_LOGIN,
        methods::FORGET_AGENT_ACCOUNT,
    ] {
        assert!(!remote_method_allowed(method), "{method} escaped the LAN denylist");
    }
}

#[tokio::test]
async fn target_device_cannot_name_another_machine() {
    let service = fixture_remote_service("device-b");
    let err = service.handle(
        methods::LIST_REFS,
        serde_json::json!({"repoPath":"/repo","targetDeviceId":"device-c"})
    ).await.unwrap_err();
    assert!(err.to_string().contains("targetDeviceId must match device-b"));
}

#[tokio::test]
async fn orphaned_foreign_transcript_is_not_remotely_addressable() {
    let service = fixture_with_unindexed_snapshot("foreign-chat", "device-c");
    let err = service.handle(
        methods::WATCH_DOC_MESSAGES,
        serde_json::json!({"chatId":"foreign-chat"})
    ).await.unwrap_err();
    assert!(err.to_string().contains("not owned by this server"));
}
```

- [ ] **Step 2: Run the test and verify failure**

Run: `cargo test -p comet-engine --test remote_access -- --nocapture`

Expected: compilation fails because `RemoteRpcService` is absent.

- [ ] **Step 3: Implement an explicit match-based allowlist**

Allow operational methods only: server hello, local device, harness/model listing, local-authority watch streams, command queueing, approved mutations, repository/worktree/folder operations, terminals, checkout diffs, `LIST_AGENT_ACCOUNTS`, `ACTIVATE_AGENT_ACCOUNT`, upload operations, and attachment reads. Do not use a prefix rule or “all except” rule.

```rust
pub fn remote_method_allowed(method: &str) -> bool {
    matches!(method,
        methods::SERVER_HELLO
        | methods::LOCAL_DEVICE
        | methods::LIST_HARNESSES
        | methods::LIST_MODELS
        | methods::QUEUE_COMMAND
        | methods::WATCH_DOC_MESSAGES
        | methods::WATCH_CHATS
        | methods::WATCH_DEVICES
        | methods::WATCH_SPACES
        | methods::WATCH_SESSIONS
        | methods::MUTATE
        | methods::LIST_REPOS
        | methods::ADD_REPO
        | methods::CLONE_REPO
        | methods::CREATE_REPO
        | methods::LIST_BRANCHES
        | methods::LIST_REFS
        | methods::SWITCH_REF
        | methods::LIST_FOLDERS
        | methods::CREATE_WORKTREE
        | methods::DELETE_WORKTREE
        | methods::OPEN_TERMINAL
        | methods::SUBSCRIBE_TERMINAL
        | methods::WRITE_TERMINAL
        | methods::RESIZE_TERMINAL
        | methods::CLOSE_TERMINAL
        | methods::WATCH_CHECKOUT_DIFFS
        | methods::LIST_AGENT_ACCOUNTS
        | methods::ACTIVATE_AGENT_ACCOUNT
        | methods::UPLOAD_CHUNK
        | methods::UPLOAD_COMMIT
        | methods::READ_ATTACHMENT_CHUNK
    )
}
```

- [ ] **Step 4: Enforce local ownership before delegation**

Reject a foreign `targetDeviceId`. Filter `WATCH_DEVICES` to the local row and filter spaces/chats/sessions/diffs to `device_id == local_device_id`. Before delegating chat-addressed methods—including transcript watch, queue command, terminals, diffs, and attachment access—require an indexed chat row owned by this server; knowledge of an orphaned copied snapshot ID must not grant access. Validate `MUTATE` so create-space uses the local device and chat/space mutations name rows owned by this server. Return `RpcError::UnknownMethod` for denied methods so the LAN surface does not advertise internals.

- [ ] **Step 5: Run engine remote-access tests**

Run: `cargo test -p comet-engine --test remote_access && cargo test -p comet-engine --test device_routing`

Expected: authoritative tests pass and legacy relay tests remain green until the final removal task.

- [ ] **Step 6: Commit the restricted service**

```bash
git add crates/engine/src/lib.rs crates/engine/src/rpc.rs crates/engine/src/remote_rpc.rs crates/engine/tests/remote_access.rs
git commit -m "feat: restrict LAN RPC to local authority"
```

### Task 6: Wire the opt-in listener and localhost administration RPCs

**Files:**
- Create: `crates/engine/src/lan_server.rs`
- Modify: `crates/engine/src/lib.rs`
- Modify: `crates/engine/src/rpc.rs`
- Modify: `crates/engine/tests/remote_access.rs`

**Interfaces:**
- Produces: `LanServer::spawn`, `LanServer::apply_settings`, `LanServerStatus`, and localhost handlers for registry/listener/pairing/trust methods.
- Consumes: Tasks 3–5 identity, configuration, TLS transport, and `RemoteRpcService`.

- [ ] **Step 1: Write listener lifecycle tests**

```rust
#[tokio::test]
async fn listener_is_closed_by_default_and_rebinds_after_enable() {
    let fixture = EngineFixture::start().await;
    assert!(TcpStream::connect(fixture.lan_addr()).await.is_err());
    fixture.local_call(methods::SET_LAN_SETTINGS, json!({
        "enabled": true,
        "bind": fixture.lan_addr()
    })).await.unwrap();
    assert!(TcpStream::connect(fixture.lan_addr()).await.is_ok());
}

#[tokio::test]
async fn bind_failure_does_not_stop_local_rpc() {
    let fixture = EngineFixture::with_occupied_lan_port().await;
    fixture.enable_lan().await.unwrap();
    assert!(fixture.local_call(methods::LOCAL_DEVICE, json!({})).await.is_ok());
    assert!(matches!(fixture.lan_status(), LanServerStatus::BindFailed { .. }));
}
```

- [ ] **Step 2: Run listener tests and verify failure**

Run: `cargo test -p comet-engine --test remote_access -- --nocapture`

Expected: compilation fails because listener/admin handlers are missing.

- [ ] **Step 3: Implement listener supervision without affecting local runtime**

`LanServer` watches `LanSettings`, binds only when enabled, swaps listeners on address changes, reports `Disabled`, `Listening`, or `BindFailed`, and uses bounded accept-task cleanup. A bind error updates status and logs but never returns from `Engine::run`.

- [ ] **Step 4: Add localhost-only admin dispatch**

Extend `EngineRpc` with `RemoteConfigStore` and `LanServerHandle`, then implement the new admin methods. `BEGIN_PAIRING` returns `{ secret, expiresAt }`; `REPORT_REMOTE_STATUS` updates only the named registry row's `last_state`, `protocol_version`, and `last_connected_at`; `REVOKE_TRUSTED_CLIENT` removes the key and asks `LanServer` to close matching active sessions. On store open, normalize a persisted `Online` state to `Offline` because no connection has yet been established. These handlers remain absent from `RemoteRpcService`.

- [ ] **Step 5: Test pairing expiry and live revocation through the engine**

Run: `cargo test -p comet-engine --test remote_access -- --nocapture`

Expected: all cases pass, including immediate active-connection closure after revocation.

- [ ] **Step 6: Commit listener and administration**

```bash
git add crates/engine/src/lan_server.rs crates/engine/src/lib.rs crates/engine/src/rpc.rs crates/engine/tests/remote_access.rs
git commit -m "feat: expose opt-in authoritative LAN server"
```

### Task 7: Create the shared direct-connection federation crate

**Files:**
- Create: `crates/client/Cargo.toml`
- Create: `crates/client/src/lib.rs`
- Create: `crates/client/src/model.rs`
- Create: `crates/client/src/server.rs`
- Create: `crates/client/src/manager.rs`
- Create: `crates/client/tests/non_transitive.rs`
- Modify: `Cargo.toml`

**Interfaces:**
- Produces: `Federation`, `FederationEvent`, `FederationCommand`, `ServerState`, and `ServerSnapshot`.
- Consumes: a trusted local `RpcClient`, data-directory identity, remote registry watch, and `connect_lan_rpc`.

- [ ] **Step 1: Write failing server-bucketing and non-transitive tests**

```rust
#[tokio::test]
async fn equal_raw_chat_ids_remain_in_separate_server_buckets() {
    let fixture = FederationFixture::with_servers([("b", "chat-1"), ("c", "chat-1")]).await;
    let state = fixture.wait_ready().await;
    assert_eq!(state.server(&server("b")).unwrap().chats[0].id, "chat-1");
    assert_eq!(state.server(&server("c")).unwrap().chats[0].id, "chat-1");
    assert_ne!(
        state.chat_ref(&server("b"), "chat-1"),
        state.chat_ref(&server("c"), "chat-1")
    );
}

#[tokio::test]
async fn b_does_not_publish_its_configured_c_to_a() {
    let fixture = ThreeServerFixture::a_to_b_and_b_to_c().await;
    let a = fixture.a_federation().wait_ready().await;
    assert!(a.server(&fixture.b_id()).is_some());
    assert!(a.server(&fixture.c_id()).is_none());
}
```

- [ ] **Step 2: Run the new crate tests and verify failure**

Run: `cargo test -p comet-client --test non_transitive -- --nocapture`

Expected: Cargo reports that package `comet-client` does not exist.

- [ ] **Step 3: Implement server-scoped model and event API**

Create `crates/client/Cargo.toml` with dependencies on `comet-proto`, `comet-doc`, `comet-identity`, `comet-rpc`, Tokio, futures, serde, serde_json, anyhow, thiserror, and tracing; do not depend on `comet-engine` or either UI crate.

```rust
pub struct ServerState {
    pub id: ServerId,
    pub name: String,
    pub connection: RemoteConnectionState,
    pub devices: Vec<Device>,
    pub spaces: Vec<Space>,
    pub chats: Vec<Chat>,
    pub sessions: Vec<Session>,
}

pub enum FederationEvent {
    ServerChanged(ServerState),
    ServerRemoved(ServerId),
    Transcript { chat: ServerRef, entries: Vec<SessionMessageEntry> },
    Notice { server_id: ServerId, message: String },
}

pub enum FederationCommand {
    Call { server_id: ServerId, method: &'static str, params: serde_json::Value },
    WatchTranscript(Option<ServerRef>),
    Reconnect(ServerId),
    Shutdown,
}
```

- [ ] **Step 4: Implement one supervisor per direct registry entry**

The manager always adds the local engine as its own server bucket, watches `WATCH_REMOTES` only on local IPC, and spawns remote supervisors only for entries in that direct registry snapshot. A remote supervisor subscribes to B's authoritative RPC streams but never calls `WATCH_REMOTES` on B. It reports successful/error state back to the local engine with localhost-only `REPORT_REMOTE_STATUS`. On disconnect it emits an empty B `ServerState` with offline status.

- [ ] **Step 5: Run crate tests including disconnect cleanup**

Run: `cargo test -p comet-client -- --nocapture`

Expected: A/B/C, duplicate-ID, reconnect, identity-changed, incompatible-version, and offline-clearing tests pass.

- [ ] **Step 6: Commit client federation**

```bash
git add Cargo.toml Cargo.lock crates/client
git commit -m "feat: federate direct engine connections in clients"
```

### Task 8: Move TUI to the shared federation model

**Files:**
- Modify: `crates/tui/Cargo.toml`
- Modify: `crates/tui/src/link.rs`
- Modify: `crates/tui/src/app.rs`
- Modify: `crates/tui/src/render.rs`
- Modify: `crates/tui/src/lib.rs`
- Modify: `crates/tui/tests/render.rs`
- Modify: `crates/tui/tests/frame_dump.rs`

**Interfaces:**
- Consumes: Task 7 `FederationEvent`, `FederationCommand`, `ServerState`, and `ServerRef`.
- Produces: TUI selection and commands that always carry `server_id`; renders persistent offline remote rows.

- [ ] **Step 1: Add failing TUI state/render tests**

```rust
#[test]
fn offline_remote_stays_visible_without_cached_children() {
    let mut app = App::fixture();
    app.apply(FederationEvent::ServerChanged(ServerState::offline(server("b"), "Build box")));
    let screen = render_to_string(&app, 100, 30);
    assert!(screen.contains("Build box"));
    assert!(screen.contains("Offline"));
    assert!(!screen.contains("B's cached chat"));
}

#[test]
fn duplicate_chat_ids_route_to_selected_server() {
    let mut app = two_server_app_with_chat_id("chat-1");
    app.select_chat(ServerRef::new(server("c"), "chat-1"));
    assert_eq!(app.selected_chat().unwrap().server_id, server("c"));
}
```

- [ ] **Step 2: Run TUI render tests and verify failure**

Run: `cargo test -p comet-tui --test render remote -- --nocapture`

Expected: compilation fails because TUI state is still single-engine.

- [ ] **Step 3: Replace `EngineLink`'s single session supervisor with `Federation`**

Keep daemon attach/spawn for the trusted local engine. After attachment, construct `Federation` with the local client and data directory. Translate existing TUI commands into `FederationCommand` using the selected `ServerRef`; remove `targetDeviceId` injection.

- [ ] **Step 4: Bucket application state by server and render server headers**

Change selection fields from raw chat/space IDs to `ServerRef`. Render local first, then configured remotes in registry order. An offline remote renders one disabled server row and no stale spaces/chats.

- [ ] **Step 5: Run all TUI tests and smoke script**

Run: `cargo test -p comet-tui && python scripts/tui-smoke.py`

Expected: all tests pass and detach behavior remains unchanged.

- [ ] **Step 6: Commit TUI federation**

```bash
git add crates/tui/Cargo.toml crates/tui/src/link.rs crates/tui/src/app.rs crates/tui/src/render.rs crates/tui/src/lib.rs crates/tui/tests/render.rs crates/tui/tests/frame_dump.rs
git commit -m "feat: show direct remote engines in TUI"
```

### Task 9: Move GPUI application state to server buckets

**Files:**
- Modify: `crates/ui/Cargo.toml`
- Modify: `crates/ui/src/state.rs`
- Modify: `crates/ui/src/shell.rs`
- Modify: `crates/ui/src/shell/spaces.rs`
- Modify: `crates/ui/src/shell/tabs.rs`
- Modify: `crates/ui/src/composer.rs`
- Modify: `crates/ui/src/attachments.rs`
- Modify: `crates/ui/src/pickers.rs`
- Modify: `crates/ui/src/terminal/panel.rs`

**Interfaces:**
- Consumes: Task 7 federation model.
- Produces: GPUI state keyed by `ServerId` and selected resources keyed by `ServerRef`.

- [ ] **Step 1: Add failing GPUI state tests for scoped selection and disconnect**

```rust
#[test]
fn removing_remote_snapshot_heals_selection_to_local() {
    let mut state = AppState::fixture_with_servers([local_server(), remote_server("b")]);
    state.select_chat(ServerRef::new(server("b"), "chat-1"));
    state.apply_federation(FederationEvent::ServerChanged(ServerState::offline(server("b"), "B")));
    assert_eq!(state.selected_server(), state.local_server_id());
    assert!(state.selected_chat().is_none());
}

#[test]
fn action_routes_through_selected_server() {
    let state = AppState::fixture_with_servers([server_with_chat("b", "chat-1")]);
    let command = state.queue_command_for(ServerRef::new(server("b"), "chat-1"), run_payload());
    assert_eq!(command.server_id, server("b"));
    assert_eq!(command.params["chatId"], "chat-1");
}
```

- [ ] **Step 2: Run focused UI state tests and verify failure**

Run: `cargo test -p comet-ui state::tests::remote -- --nocapture`

Expected: compilation fails because `AppState` has one flat engine snapshot.

- [ ] **Step 3: Bootstrap `Federation` after local engine bootstrap**

Retain `EngineHandle` only for local attach/embed ownership and shutdown. Replace the five direct watch pumps in `AppState::attach` with a federation event pump. Store server rows in `HashMap<ServerId, ServerState>` plus a separate `Vec<ServerId>` containing local-first registry order, and change selected chat/space/session identities to `ServerRef`.

- [ ] **Step 4: Route every operation through the selected server**

Update composer sends, transcript watches, repo/ref pickers, worktree operations, terminals, diffs, and attachment upload/read calls to carry `server_id` separately while keeping raw IDs in RPC params. Remove all UI-side `targetDeviceId` construction.

- [ ] **Step 5: Render authoritative server grouping and offline rows**

Update sidebar and tab projections to iterate server buckets. Local appears first. Remote names come from `RemoteEntry`; offline/unreachable/identity/incompatible status appears on the server header and no stale children render.

- [ ] **Step 6: Run UI state/render unit tests**

Run: `cargo test -p comet-ui --lib`

Expected: all tests pass, including duplicate raw IDs across servers and routing assertions.

- [ ] **Step 7: Commit GPUI federation**

```bash
git add crates/ui/Cargo.toml crates/ui/src/state.rs crates/ui/src/shell.rs crates/ui/src/shell/spaces.rs crates/ui/src/shell/tabs.rs crates/ui/src/composer.rs crates/ui/src/attachments.rs crates/ui/src/pickers.rs crates/ui/src/terminal/panel.rs
git commit -m "feat: scope desktop state by direct server"
```

### Task 10: Add desktop remote settings and pairing controls

**Files:**
- Create: `crates/ui/src/remotes.rs`
- Modify: `crates/ui/src/lib.rs`
- Modify: `crates/ui/src/settings.rs`
- Modify: `crates/ui/src/shell.rs`
- Modify: `crates/ui/src/icons.rs`
- Test: `crates/ui/src/remotes.rs`

**Interfaces:**
- Consumes: localhost admin RPCs from Task 6 and `pair_client` from Task 4.
- Produces: opt-in listener controls, pair-secret presentation, trusted-client revocation, and outbound remote add/rename/remove UI.

- [ ] **Step 1: Write reducer tests for listener, pairing warning, and identity mismatch**

```rust
#[test]
fn enabling_listener_requires_explicit_user_action() {
    let state = RemoteSettingsState::default();
    assert!(!state.lan.enabled);
    assert!(state.pairing_secret.is_none());
}

#[test]
fn pairing_warning_describes_granted_authority() {
    assert!(PAIRING_WARNING.contains("run agents"));
    assert!(PAIRING_WARNING.contains("control terminals"));
    assert!(PAIRING_WARNING.contains("trusted devices"));
}
```

- [ ] **Step 2: Run focused UI tests and verify failure**

Run: `cargo test -p comet-ui remotes -- --nocapture`

Expected: compilation fails because `remotes.rs` is absent.

- [ ] **Step 3: Implement Remote Connections settings state and local RPC calls**

Add controls for enabled/bind/port, begin pairing, trusted clients and revoke, configured remotes and their statuses, add by endpoint+secret, rename, reconnect, and remove. Display pairing expiration and clear the secret when it expires or succeeds.

- [ ] **Step 4: Implement pairing/add flow without exposing private key to widgets**

The async controller loads the installation identity from the data directory, calls `pair_client`, then sends the resulting `RemoteEntry` to localhost `PUT_REMOTE`. Widgets receive only endpoint, friendly name, secret text, and success/error state.

- [ ] **Step 5: Run UI tests**

Run: `cargo test -p comet-ui remotes && cargo test -p comet-ui --lib`

Expected: all tests pass and no rendered state contains private key bytes.

- [ ] **Step 6: Commit desktop settings**

```bash
git add crates/ui/src/remotes.rs crates/ui/src/lib.rs crates/ui/src/settings.rs crates/ui/src/shell.rs crates/ui/src/icons.rs
git commit -m "feat: manage LAN remotes from desktop"
```

### Task 11: Add remote CLI and replace auth-oriented status

**Scope correction:** There is no cloud-to-local migration command or manually
selected profile path. `comet migrate` must be rejected. The earlier migration
CLI steps below are superseded; internal startup compatibility is left intact
for a focused Task 12 removal audit.

**Files:**
- Create: `apps/comet/src/remote_cli.rs`
- Modify: `apps/comet/src/main.rs`
- Delete: `apps/comet/src/auth_cli.rs`
- Modify: `apps/comet/src/daemon.rs`
- Modify: `apps/comet/Cargo.toml`
- Test: `apps/comet/src/remote_cli.rs`

**Interfaces:**
- Produces: `comet remote add|list|remove|listen|pair|clients|revoke` and local/LAN-oriented `comet status`.
- Consumes: local admin RPC when the engine is running; otherwise the same identity/config types under an exclusive engine lock.

- [ ] **Step 1: Write CLI parser and output tests**

```rust
#[test]
fn login_and_logout_are_not_commands() {
    assert!(Cli::try_parse_from(["comet", "login"]).is_err());
    assert!(Cli::try_parse_from(["comet", "logout"]).is_err());
}

#[test]
fn parses_manual_remote_endpoint() {
    let cli = Cli::try_parse_from(["comet", "remote", "add", "buildbox.local:27655"]).unwrap();
    assert!(matches!(cli.command, Some(Command::Remote { command: RemoteCommand::Add { .. } })));
}

#[test]
fn migrate_is_not_a_command() {
    assert!(Cli::try_parse_from(["comet", "migrate"]).is_err());
}

#[test]
fn parses_server_side_pairing_session() {
    let cli = Cli::try_parse_from(["comet", "remote", "pair"]).unwrap();
    assert!(matches!(cli.command, Some(Command::Remote { command: RemoteCommand::Pair })));
}
```

- [ ] **Step 2: Run application tests and verify failure**

Run: `cargo test -p comet --bin comet -- --nocapture`

Expected: compilation fails because `RemoteCommand` is absent.

- [ ] **Step 3: Implement remote subcommands and safe active-engine behavior**

Add `comet-identity`, `comet-proto`, `comet-rpc`, and `rpassword = "7"` to `apps/comet/Cargo.toml`. When local IPC is live, administer through local RPC so the running engine publishes watches immediately. When it is not live, acquire `InstanceLock`, update configuration directly, and run the same pairing client. `comet remote pair` requires a running local engine/listener, calls `BEGIN_PAIRING`, and prints the grouped secret and expiry. `comet remote add` reads that secret with `rpassword::prompt_password`; never accept it as a positional argument that lands in shell history.

- [ ] **Step 4: Remove auth commands and rewrite status**

`comet status` prints data directory, engine/IPC state, LAN listener state/address/error, paired-client count, and each configured remote's name/endpoint/status. It does not print Edge, WorkOS, user, or organization fields.

Do not implement or advertise a migration command. `comet migrate` is rejected
by the parser. Preserve existing internal local-store startup behavior in this
task; report legacy migration candidates for removal in Task 12.

- [ ] **Step 5: Remove obsolete daemon environment capture**

Delete `COMET_EDGE_URL`, `COMET_EDGE_TOKEN`, `COMET_ORG_ID`, `COMET_WORKOS_CLIENT_ID`, `COMET_WORKOS_API_BASE`, and `COMET_CALLBACK_PORT` from `CAPTURED_ENV`. Add `COMET_RELEASES_URL` and retain local engine/listener settings that remain meaningful.

- [ ] **Step 6: Run CLI/daemon tests**

Run: `cargo test -p comet --bin comet`

Expected: all parser, status, daemon rendering, and remote CLI tests pass.

- [ ] **Step 7: Commit CLI cutover**

```bash
git add apps/comet/Cargo.toml apps/comet/src/main.rs apps/comet/src/remote_cli.rs apps/comet/src/daemon.rs
git rm apps/comet/src/auth_cli.rs
git commit -m "feat: replace account CLI with LAN remotes"
```

### Task 12: Cut the engine and UI over to local-only authority

**Files:**
- Modify: `crates/engine/src/local_store.rs`
- Delete: `crates/engine/tests/local_store_migration.rs`
- Delete: `crates/engine/src/auth.rs`
- Delete: `crates/engine/tests/auth.rs`
- Delete: `crates/engine/tests/device_routing.rs`
- Modify: `crates/engine/src/lib.rs`
- Modify: `crates/engine/src/rpc.rs`
- Modify: `crates/engine/src/doc_host.rs`
- Modify: `crates/engine/src/workspace_host.rs`
- Modify: `crates/engine/src/diff_sync.rs`
- Modify: `crates/engine/src/uploads.rs`
- Modify: `crates/engine/Cargo.toml`
- Modify: `crates/rpc/src/lib.rs`
- Delete: `crates/rpc/src/device_room.rs`
- Delete: `crates/rpc/tests/device_room.rs`
- Modify: `crates/rpc/Cargo.toml`
- Modify: `crates/sync/src/lib.rs`
- Modify: `crates/sync/src/store.rs`
- Modify: `crates/sync/Cargo.toml`
- Delete: `crates/sync/src/room.rs`
- Delete: `crates/sync/src/room/`
- Delete: `crates/sync/src/wake.rs`
- Delete: `crates/sync/tests/edge_convergence.rs`
- Modify: `crates/proto/src/entities.rs`
- Modify: `crates/proto/src/view.rs`
- Modify: `crates/doc/src/workspace.rs`
- Modify: `crates/ui/src/state.rs`
- Modify: `crates/ui/src/shell.rs`
- Modify: `crates/tui/src/link.rs`

**Interfaces:**
- Consumes: Tasks 3–6 remote runtime and the existing `comet-update` release checker. It does not consume Task 2's migration API.
- Produces: an `EngineConfig` containing only local data, IPC, LAN, harness, and release settings; startup has no account gate, cloud-profile selection, copy, marker, or recovery-command path.
- Removes: `LegacyProfile`, `migrated_from`, `prepare_local_store`, migration staging/marker/profile-selection code, migration-only tests, and migration-only `DocsStore` / `WorkspaceDoc` helpers when the initial usage trace confirms no independent caller.

- [ ] **Step 1: Add a failing no-runtime-cloud engine test**

```rust
#[tokio::test]
async fn fresh_engine_starts_without_account_or_runtime_edge() {
    let dir = tempfile::tempdir().unwrap();
    let config = EngineConfig::for_test(dir.path());
    let runtime = Engine::assemble_runtime(&config).await.unwrap();
    let local = runtime.core().rpc_service().handle(methods::LOCAL_DEVICE, json!({})).await;
    assert!(local.is_ok());
}
```

Also add a source/config regression test that fails if runtime code references `/auth/`, `/workspace/`, `/session/`, `/device/`, `COMET_EDGE_`, or WorkOS.

Add a second source/API regression that fails if normal startup or user-facing
errors reference `comet migrate`, `LegacyProfile`, `migrated_from`,
`prepare_local_store`, or the legacy recovery command. This is a deletion
boundary: do not replace the removed command with differently worded migration
guidance.

- [ ] **Step 2: Run focused tests and verify they fail under the auth gate**

Run: `cargo test -p comet-engine no_runtime_cloud -- --nocapture`

Expected: the new API/signature is missing or startup still requires auth configuration.

- [ ] **Step 3: Trace startup and replace migration with local-only initialization**

First trace every production and test caller of `prepare_local_store`,
`LegacyProfile`, `migrated_from`, `DocsStore::copy_snapshots_to`,
`DocsStore::snapshot_ids`, and `WorkspaceDoc::owned_by`. Confirm which APIs are
migration-only before deleting them. Then remove legacy session/profile
selection, copying/filtering, staging, marker, and recovery-guidance logic.

Replace `prepare_local_store` with a narrowly named local-only initializer, or
open the fixed local root directly in assembly if the trace shows no separate
abstraction is useful. The initializer may create/open an empty local store; it
must never inspect `session.json` or `orgs/`, copy cloud-cached rows, expose a
manual selector, or emit `comet migrate` guidance.

Replace identity-scoped `assemble_with_identity` with:

```rust
pub struct EngineConfig {
    pub data_dir: PathBuf,
    pub ipc_port: u16,
    pub default_harness: HarnessId,
    pub releases_url: String,
}

pub fn initialize_local_store(data_dir: &Path) -> Result<PathBuf, EngineError> {
    let root = data_dir.join("local-store");
    DocsStore::open(&root)?;
    Ok(root)
}

pub async fn assemble_runtime(config: &EngineConfig) -> anyhow::Result<EngineRuntime> {
    let local_root = initialize_local_store(&config.data_dir)?;
    let core = EngineCore::assemble_local(&config.data_dir, &local_root, config.default_harness)?;
    let lan = LanServer::spawn(core.remote_rpc_service(), core.remote_access()).await;
    Ok(EngineRuntime::new(core, lan))
}
```

The exact helper name is not normative; direct `DocsStore::open` is preferred
if a wrapper adds no invariant. Keep updater initialization independent and
non-fatal when the release endpoint is unreachable. `COMET_RELEASES_URL` and
release downloads remain separate from runtime/cloud removal and must not be
removed in this task.

- [ ] **Step 4: Remove cloud transports and relay forwarding**

Remove `EdgeConfig` from `DocHostConfig`, `WorkspaceHostConfig`, `Uploads`, and diff sync. Do not start room joins, host relays, link caches, refresh loops, or cloud nudges. Delete `targetDeviceId` forwarding from `EngineRpc`; Task 7 routes calls through the correct direct client instead.

- [ ] **Step 5: Remove auth/org protocol and UI gates**

Delete auth RPC method constants, `AuthState`, organization parsing, sign-in gate, and org gate. Simplify `GatePhase` to boot/ready/failure based only on connection state. Desktop embedded startup assembles immediately; TUI no longer subscribes to `AUTH_STATUS`.

- [ ] **Step 6: Remove relay modules and obsolete dependencies**

Delete `device_room` exports/tests. Reduce `comet-sync` to `DocsStore`: delete `room`, `wake`, and the live edge convergence test, then remove its Loro WebSocket/Tungstenite dependencies. Retain `reqwest` in `comet-engine` because `agent_accounts.rs` uses it for Claude/Codex account and usage APIs; remove the cloud-runtime HTTP fields and calls from `doc_host.rs`, `workspace_host.rs`, `diff_sync.rs`, and `uploads.rs`.

Delete the legacy local-store migration integration test. Remove
`DocsStore::copy_snapshots_to`, migration-only snapshot enumeration, and
`WorkspaceDoc::owned_by` only after the Step 3 usage trace confirms they have no
non-migration caller. Remove migration marker/staging constants and stored
session selector types. No replacement error may instruct users to run `comet
migrate`.

- [ ] **Step 7: Run core workspace tests**

Run: `cargo test -p comet-proto && cargo test -p comet-rpc && cargo test -p comet-engine && cargo test -p comet-client && cargo test -p comet-tui && cargo test -p comet-ui --lib`

Expected: all tests pass with no login/org/relay test surface.

Also run:

```bash
rg -n "comet migrate|LegacyProfile|migrated_from|prepare_local_store|RECOVERY_COMMAND" crates apps/comet
```

Expected: no runtime, API, test, or user-facing migration path remains.

- [ ] **Step 8: Commit runtime-cloud removal**

```bash
git add Cargo.toml Cargo.lock crates/proto crates/rpc crates/sync crates/engine crates/ui crates/tui
git commit -m "refactor: remove hosted runtime dependency"
```

### Task 13: Separate update distribution and reduce the edge Worker

**Files:**
- Modify: `crates/update/src/lib.rs`
- Modify: `apps/comet/src/update_cli.rs`
- Modify: `apps/comet/src/main.rs`
- Modify: `edge/src/index.ts`
- Modify: `edge/src/env.ts`
- Create: `edge/src/distribution.test.ts`
- Modify: `edge/wrangler.jsonc`
- Delete: `edge/src/auth-routes.ts`
- Delete: `edge/src/auth.ts`
- Delete: `edge/src/blobs.ts`
- Delete: `edge/src/device-room.ts`
- Delete: `edge/src/session-room.ts`
- Delete: `edge/src/workos.ts`
- Delete: `edge/src/session-doc/`
- Delete: `edge/scripts/compat-check.mjs`
- Delete: `edge/scripts/device-frame.mjs`
- Delete: `edge/scripts/fold-check.mjs`
- Delete: `edge/scripts/reset-check.mjs`
- Delete: `edge/scripts/smoke.mjs`
- Delete: `edge/src/device-frame.test.ts`
- Delete: `edge/src/device-host-liveness.test.ts`
- Modify: `edge/package.json`
- Modify: `.github/workflows/release.yml`
- Modify: `.github/workflows/deploy.yml`

**Interfaces:**
- Produces: `releases_url_from_env()` and a distribution Worker serving only `/install.sh` and `/releases/*`.
- Consumes: existing manifest/checksum update flow.

- [ ] **Step 1: Write failing release-URL independence tests**

```rust
#[test]
fn releases_url_is_independent_of_removed_edge_variables() {
    let env = TestEnv::new()
        .set("COMET_RELEASES_URL", "https://downloads.example")
        .set("COMET_EDGE_URL", "https://must-not-be-read.example");
    assert_eq!(releases_url_from(&env), "https://downloads.example");
}
```

Add a Worker test asserting `/health`, `/auth/exchange`, `/workspace/x/ws`, `/device/x/ws`, and `/attachments/<hash>` return `404`, while `/install.sh` and `/releases/manifest.json` remain routable.

- [ ] **Step 2: Run Rust and edge tests and verify failure**

Run: `cargo test -p comet-update && npm test --prefix edge`

Expected: the new URL resolver is absent and runtime routes still exist.

- [ ] **Step 3: Rename update configuration and make checks non-critical**

Use `DEFAULT_RELEASES_URL = "https://comet.zeron.sh"` and `COMET_RELEASES_URL`. Ensure background update failure changes only `UpdateStatus` and cannot fail engine assembly or LAN operation.

- [ ] **Step 4: Reduce the Worker to distribution routes**

Keep the release R2 binding and installer module only. Remove Durable Object classes/bindings, WorkOS secrets/JWKS, runtime attachment binding, migrations, compatibility/smoke scripts, and runtime route imports. Remove `jose`, `loro-crdt`, `loro-protocol`, `loro-adaptors`, and `loro-websocket` from `edge/package.json` and refresh `package-lock.json`. Update the release workflow to publish to the retained release bucket and distribution endpoint.

- [ ] **Step 5: Run distribution and update tests**

Run: `cargo test -p comet-update && npm test --prefix edge && npm run typecheck --prefix edge`

Expected: update tests pass; only installer/release Worker tests and types remain.

- [ ] **Step 6: Commit distribution separation**

```bash
git add .github/workflows/release.yml .github/workflows/deploy.yml crates/update/src/lib.rs apps/comet/src/update_cli.rs apps/comet/src/main.rs edge
git commit -m "refactor: keep edge for release distribution only"
```

### Task 14: End-to-end verification, documentation, and dead-code audit

**Files:**
- Create: `crates/client/tests/lan_e2e.rs`
- Modify: `README.md`
- Modify: `ARCHITECTURE.md`
- Modify: `docs/PARITY.md`
- Modify: `edge/src/install.sh`
- Modify: `scripts/tui-smoke.py`

**Interfaces:**
- Consumes: the completed local/LAN runtime.
- Produces: executable A/B/C acceptance coverage and user/operator documentation.

- [ ] **Step 1: Add the full A/B/C acceptance test**

```rust
#[tokio::test]
async fn direct_connections_are_operational_and_non_transitive() {
    let mut net = LanTestNetwork::start_three().await;
    net.pair("a", "b").await.unwrap();
    net.pair("b", "c").await.unwrap();

    let a = net.federation("a").await;
    assert!(a.wait_server(net.id("b")).await.is_online());
    assert!(a.server(net.id("c")).is_none());
    assert!(a.call(net.id("b"), methods::LIST_REPOS, json!({})).await.is_ok());

    net.pair("a", "c").await.unwrap();
    assert!(a.wait_server(net.id("c")).await.is_online());

    net.stop("b").await;
    let b = a.wait_server(net.id("b")).await;
    assert!(matches!(b.connection, RemoteConnectionState::Offline | RemoteConnectionState::Unreachable { .. }));
    assert!(b.spaces.is_empty() && b.chats.is_empty() && b.sessions.is_empty());
}
```

- [ ] **Step 2: Extend end-to-end coverage for sensitive operations and failures**

Add cases for agent command queueing with the mock harness, terminal open/write/resize/close, repository/ref/diff listing, chunked attachment upload/read, wrong and expired pairing secrets, identity rotation, protocol mismatch, listener port conflict, malformed frames, and active revocation.

- [ ] **Step 3: Run the end-to-end and security suites**

Run: `cargo test -p comet-client --test lan_e2e -- --nocapture && cargo test -p comet-rpc --test secure_lan && cargo test -p comet-engine --test remote_access`

Expected: all tests pass; A cannot observe C until explicitly paired.

- [ ] **Step 4: Rewrite user and architecture documentation**

Document local authority, manual endpoint configuration, listener opt-in, pairing/revocation, the power granted to trusted devices, server grouping, offline-without-cache behavior, non-transitivity, firewall considerations, removed login commands, the absence of any cloud-to-local migration command/path, and optional release internet access. Remove instructions that say runtime sync or control requires WorkOS/Cloudflare.

- [ ] **Step 5: Audit for forbidden hosted-runtime remnants**

Run:

```bash
rg -n "WorkOS|COMET_EDGE|AUTH_STATUS|SIGN_IN|LIST_ORGS|DeviceRoom|HostRelay|LinkCache|/auth/|/workspace/|/session/|/device/|R2 attachments" Cargo.toml crates apps/comet README.md ARCHITECTURE.md docs/PARITY.md
```

Expected: no production runtime matches. Matches are allowed only in migration comments/tests that parse the obsolete `session.json`, the approved design/plan documents, and iOS files explicitly outside scope.

- [ ] **Step 6: Run formatting, lint, Rust workspace tests, edge tests, and TUI smoke**

Run:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
npm test --prefix edge
npm run typecheck --prefix edge
python scripts/tui-smoke.py
```

Expected: every command exits `0`.

- [ ] **Step 7: Perform manual two-machine verification**

On B, enable the LAN listener and begin pairing. On A, add B by hostname/IP and port, enter the secret, start a mock-harness session and terminal on B, then stop B. Confirm A continues locally, B stays listed offline with no cached children, and restarting B reconnects. Configure B-to-C and confirm A never shows C until A pairs with C itself.

- [ ] **Step 8: Commit acceptance coverage and documentation**

```bash
git add README.md ARCHITECTURE.md docs/PARITY.md edge/src/install.sh scripts/tui-smoke.py crates/client/tests/lan_e2e.rs
git commit -m "docs: document LAN-only Comet operation"
```

## Final Verification Gate

Before declaring implementation complete, run the `superpowers:verification-before-completion` skill and repeat the full commands from Task 14 Step 6. Record the exact passing command output in the handoff. Confirm the worktree is clean except for explicitly user-owned changes and confirm the git log contains one focused commit per task.
