# Task 10 report: desktop remote settings

## Implementation

- Added a first-class **Remote connections** desktop settings section with an opt-in listener toggle, bind-address and port inputs, authoritative listener status, and exact bind-failure display.
- Added `BEGIN_PAIRING` controls with the full-authority warning, an expiring secret, trusted-client watches, and revoke actions. Pairing secrets clear on expiry, replacement, and successful trust-list growth.
- Added configured-remote watches and manual `hostname-or-ip:port` + secret pairing, friendly names, identity and connection status, rename, explicit reconnect, and remove actions.
- Added generation-gated reducer state for concurrent pairing and add requests. Stale completions cannot replace a newer secret or surface an obsolete error.
- Added `FederatedClient::reconnect`, which targets the named remote supervisor without routing a local administrative RPC over LAN.

## Security boundary

- Every remote-registry, listener, pairing-session, and trust mutation uses `AppState.engine().client()`, the raw trusted-local IPC client. The settings page never invokes these methods through `selected_client` or a remote federation request.
- `InstallationRemotePairer` loads `DeviceIdentity` privately from the installation data directory, converts it to `TlsIdentity`, calls Task 4 `pair_client`, and returns only the public server ID and pinned fingerprint.
- The GPUI entities contain endpoint, friendly name, pairing-secret text, status, and errors. Private-key bytes never enter widget or reducer state and are not serialized into `RemoteEntry`.
- A missing installation data directory fails visibly instead of loading or creating identity material relative to the process working directory.

## TDD evidence

The initial focused run failed compilation on the absent reducer, warning, status formatting, and secret decoder. After the minimal reducer went green, a controller test failed compilation on the absent pairer/local-admin boundary, and the settings-section and explicit-reconnect tests likewise failed before their production APIs existed.

Final focused results:

- `cargo test -p comet-ui remotes -- --nocapture`: 7 passed, 0 failed.
- `cargo test -p comet-ui remote_connections_is_a_first_class_settings_section -- --nocapture`: 1 passed, 0 failed.
- `cargo test -p comet-ui explicit_remote_reconnect_targets -- --nocapture`: 1 passed, 0 failed.

Coverage includes default-off listening, warning content, pairing-secret lifecycle, stale generations, identity/bind error copy, grouped Base32 parsing, public-only persisted pairing output, settings navigation, and exact reconnect targeting.

## Verification

- `cargo test -p comet-ui --lib`: 336 passed, 0 failed.
- `cargo fmt --all -- --check`: passed.
- `git diff --check`: passed.
- Strict clippy was attempted with `cargo clippy -p comet-ui --all-targets -- -D warnings` and again with `--no-deps`. The first is blocked by a pre-existing `comet-update` `collapsible_if`; the second reaches `comet-ui` and is blocked by pre-existing warnings in `sound.rs`, `composer.rs`, markdown, pickers, shell, transcript, and other baseline files. The only warning in Task 10 code was fixed (`cloned_ref_to_slice_refs`).

## Self-review

- Listener status polls the localhost status RPC because bind success/failure is asynchronous; a bind failure leaves local operation intact and appears on the settings page.
- Remote and trusted-client rows come from their localhost watch streams, so offline, unreachable, identity-changed, protocol mismatch, rename, revoke, and remove updates reconcile authoritatively.
- Initial trusted-client hydration cannot be mistaken for a newly paired client, and pairing cannot begin until that baseline is loaded.
- Pairing and add task replacement is cancellation-safe and also generation-gated; authoritative watch streams reconcile server-side operations that completed near cancellation.

## Review fix round 1

- A successful TLS pairing followed by a failed local `PUT_REMOTE` is now a distinct partial-success result. It exposes only public remote details and directs the user to revoke this device on the remote computer before starting a fresh pairing session. The controller performs exactly one persistence attempt and never presents a simple retry.
- Rename is now a dedicated local-only `RENAME_REMOTE` RPC backed by an atomic locked `rename_remote` store mutation. It sends only server ID and name, preserving connection state, protocol version, timestamps, endpoint, and pin changed concurrently.
- Remove and revoke now require an explicit target-and-consequence confirmation. Cancel emits no action; confirm emits a server-ID-qualified action.
- Rename, remove, and revoke use independent, server-qualified operation tracking. Duplicate operations are bounded without cancellation or error-state collisions between unrelated mutations.
- Remote and trusted-client watches now reconnect after subscription failure, malformed data, or stream close. They expose stale/offline state, retain last-known data, and use capped exponential backoff from 250 ms through 4 seconds. The page owns all watch tasks, so navigation/drop cancels the loops and their stream receivers.
- Pairing text and decoded bytes are zeroized. Secret-bearing state and requests no longer derive `Clone` or revealing `Debug`; custom debug output is redacted. Secrets clear on replacement, RPC failure, expiry, successful pairing, add submission outcomes, and page drop.
- Endpoint parsing accepts bracketed IPv6 and rejects ambiguous bare IPv6 and hosts containing whitespace.

### Fix-round TDD evidence

The initial UI regression test failed compilation on 16 intentionally absent contracts for partial success, redacted secret construction, destructive confirmation, qualified operation tracking, and watch recovery. The protocol test then failed with `endpoint must be host:port` for bracketed IPv6. Store and RPC tests failed compilation on absent `rename_remote` and `RENAME_REMOTE`. Each production path was added only after its focused failure.

Focused results after the fixes:

- `cargo test -p comet-ui remotes -- --nocapture`: 13 passed before the final stale-visibility case; the resulting full UI library suite contains 14 remote-settings tests.
- `cargo test -p comet-ui stale_watch_message_explains_cached_state_and_recovery -- --nocapture`: 1 passed.
- `cargo test -p comet-proto endpoint -- --nocapture`: 2 passed.
- `cargo test -p comet-client every_local_administration_method_is_blocked_from_generic_remote_calls -- --nocapture`: 1 passed.
- `cargo test -p comet-engine --test remote_access -- --nocapture`: 20 passed.

### Fix-round verification

- `cargo test -p comet-proto -p comet-rpc -p comet-client -p comet-engine -p comet-ui --lib`: 446 passed, 0 failed (36 client + 36 engine + 16 protocol + 15 RPC + 343 UI).
- `cargo test -p comet-engine --test remote_access -- --nocapture`: 20 passed, 0 failed.
- `cargo fmt --all -- --check`: passed after formatting.
- `git diff --check`: passed.
- Strict multi-package clippy remains blocked before affected code by the pre-existing `crates/update/src/lib.rs:543` `collapsible_if`. `cargo clippy -p comet-ui --lib --no-deps -- -D warnings` reaches the UI and reports 15 pre-existing baseline findings in other files; it reports none in `remotes.rs`.

## Review fix round 2

- Remote add is now a single-flight operation owned by `AppState`, not by the settings page. The app-owned task runs pairing and local persistence as one future, and its `RemoteAddCoordinator` retains a public terminal result for a newly attached page. Dropping the page after remote trust succeeds cannot cancel `PUT_REMOTE`; a persistence failure remains retrievable as `PartialSuccess` and is logged with public server/endpoint/error fields only.
- A second add is rejected while the coordinator is in flight. App/runtime shutdown remains the explicit residual risk: process exit can end a task after remote trust but before local persistence, while ordinary page navigation cannot.
- Remove and revoke confirmations now include the stable server ID as well as the friendly name (and endpoint for remove), so duplicate friendly names are distinguishable.
- Remote and trusted-client watch loops resolve `AppState.local_rpc_client()` before every subscribe attempt. They do not retain an old `EngineHandle` or client between attempts, so engine replacement is observed on retry. Page-owned task handles still cancel subscriptions and retry loops on page drop.
- Subscribe success no longer resets watch backoff. Close, subscribe failure, and malformed-frame cycles accumulate 250, 500, 1000, 2000, and 4000 ms delays capped at 4000 ms. Only a valid decoded snapshot marks the watch live and resets the sequence.
- Remote pairing text no longer enters `ComposerInput`. A dedicated single-line `SecretInput` stores plaintext only in `Zeroizing<String>`, mutates it directly for edit/paste, exposes redacted `Debug`, explicitly clears it on submission, and renders only grouped bullets. Server pairing secrets remain zeroizing and masked; `Copy secret` uses a short-lived zeroizing source, keeps no plaintext copy status, expires that status after ten seconds, and tells the user that the value remains in the system clipboard until replaced. The framework clipboard necessarily owns the copied plaintext; no stronger zeroization claim is made.
- The RPC pairing boundary now accepts `Zeroizing<[u8; 16]>` through `pair_client_zeroizing`; the legacy array entry point wraps immediately for compatibility. The UI creates the protected copy directly at that narrow boundary.
- Endpoint parsing validates bracketed contents as `Ipv6Addr`, accepts valid IPv4 literals, and enforces DNS total/label length, character, and edge rules. Malformed numeric IPv4-like hosts, underscores, empty labels, edge hyphens, whitespace, ambiguous bare IPv6, and invalid bracketed IPv6 are rejected.

### Round-2 TDD evidence

- Endpoint RED: `[not-ipv6]:27655` was accepted before bracket validation; the malformed IPv6/DNS matrix now passes.
- UI RED: the regression batch failed compilation on absent `RemoteAddCoordinator`, `run_remote_add_operation`, `SecretInputModel`, and valid-snapshot watch methods. The minimal contracts and production integrations were then added.
- Engine replacement fixture initially failed outside a Tokio reactor; converting it to an async test made the transport fixture valid, after which it proved that a replaced engine yields a different current local client.
- The durable-add test gates local persistence: it waits until pairing returned and `PUT_REMOTE` began, drops the page-level observer, releases the failing PUT, then verifies one persistence attempt and retrievable revoke/fresh-pairing recovery.

Focused results:

- `cargo test -p comet-ui remotes -- --nocapture`: 17 passed, 0 failed.
- `cargo test -p comet-proto endpoint -- --nocapture`: 3 passed, 0 failed.
- `cargo test -p comet-ui local_watch_client_provider_observes_engine_replacement -- --nocapture`: 1 passed, 0 failed.
- `cargo test -p comet-ui durable_remote_add -- --nocapture`: 1 passed, 0 failed.

### Round-2 verification

- `cargo test -p comet-proto -p comet-rpc -p comet-client -p comet-engine -p comet-ui --lib`: 451 passed, 0 failed (36 client + 36 engine + 17 protocol + 15 RPC + 347 UI).
- `cargo test -p comet-engine --test remote_access -- --nocapture`: 20 passed, 0 failed.
- `cargo clippy -p comet-proto -p comet-rpc --lib -- -D warnings`: passed.
- `cargo clippy -p comet-ui --lib --no-deps -- -D warnings`: remains blocked by 15 pre-existing findings outside `remotes.rs` and `state.rs`; after fixing the one new lazy-evaluation finding, the rerun reports no Task 10 finding.
- Formatting and diff checks are run again immediately before commit.
