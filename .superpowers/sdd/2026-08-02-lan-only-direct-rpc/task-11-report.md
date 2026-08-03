# Task 11 report — LAN remote CLI and local status

## Result

- Added `comet remote add|list|remove|listen|pair|clients|revoke`.
- Removed the `login` / `logout` commands and auth CLI module. `comet migrate` is also rejected; cloud-to-local migration is not a CLI or supported product path.
- Replaced account-oriented status with local data-dir, engine/IPC, LAN listener, paired-client, and direct-remote status.
- Active engines are administered through local RPC. Offline reads and mutations acquire `InstanceLock`; an owned lock with unavailable IPC fails closed rather than editing configuration behind the engine.
- Added Windows `InstanceLock` exclusivity using file-sharing rules so the same safety contract holds on Windows.
- Daemon capture no longer includes Edge, token, org, WorkOS, or callback variables. `COMET_RELEASES_URL` remains as a separate distribution-only override, and `comet update` uses it.

## TDD evidence

The initial parser RED failed on the absent `remote_cli` module and `Remote` variant. Focused behavior REDs then covered the grouped-secret decoder, remote output, stable-ID handling, and active/offline engine safety. The daemon capture test failed because `COMET_RELEASES_URL` was absent and online-service variables were still captured. A final audit RED showed that offline status could report a persisted stale `Online` state; offline snapshots now normalize `Online` and `Connecting` to `Offline`. A later scope-correction regression requires `comet migrate` to be rejected.

The active/offline safety work exposed that `InstanceLock` did not lock on Windows. The existing exclusivity/holder/reacquire tests were expanded to Windows before the Windows file-sharing implementation was applied.

## Safety audit

- Pairing secrets are never CLI arguments. `remote add` reads from the terminal with `rpassword`, moves the returned allocation into `Zeroizing<String>`, decodes into `Zeroizing<[u8; 16]>`, and passes that to the hardened pairing client.
- `remote pair` intentionally prints the one-time server-side secret and expiry. Its deserialized string allocation is immediately moved into a zeroizing owner before output; no secret-bearing type derives `Debug`, and no secret is logged.
- Remove and revoke accept stable server IDs, not names, so duplicate display names cannot select the wrong row.
- On Windows, the holder shares reads (for the PID probe) but denies a second writer. Closing or process exit releases the kernel-owned share lock. The PID text may remain after exit, but `holder` tests lock availability first, so stale metadata is ignored; drop/reacquire is covered by tests.

## Verification

- `cargo fmt --all -- --check` — passed.
- `cargo test -p comet` — 18 passed before the scope correction. The corrected parser test could not reach execution because the degraded MSVC linker stalled; source-level checks confirm the command variant and dispatch are absent and the rejection regression is the only remaining app reference.
- `cargo test -p comet-engine --test remote_access` — 20 passed.
- `cargo test -p comet-engine instance_lock::tests` — 2 passed on Windows.
- `cargo clippy -p comet --bin comet --no-deps -- -D warnings` — passed for the Task 11 binary. Dependency warnings are capped.
- Full-dependency strict Clippy remains blocked by the pre-existing `clippy::collapsible_if` in `crates/update/src/lib.rs:543`.
- `comet remote --help` smoke — all seven commands present.
- Offline `comet status` smoke — prints only local engine/IPC, LAN listener, client count, and direct remotes; no Edge, WorkOS, user, or organization data.
- `git diff --check` — passed.

## Residual risks

- Pairing and live local-RPC administration were verified against the existing hardened RPC/remote-access integration suite, but not with two physical machines in this task.
- Non-Unix/non-Windows targets retain the pre-existing best-effort lock behavior; supported desktop/service targets (Windows, macOS, Linux) now have real exclusivity.

## Review fix round 1

- Every successful IPC connection now calls `ServerHello` and compares its `serverId` with the already-existing identity in the selected data directory before granting status or administrative authority. A mismatched engine is an error, including for destructive commands. If the selected directory has no identity, an occupied IPC port fails closed without creating one; only genuinely absent IPC proceeds to the offline-lock path.
- Added `DeviceIdentity::load_existing`, which never creates a directory or identity file, plus identity and live WebSocket regressions. The destructive mismatch regression serves engine A on the selected port, selects data directory B, attempts removal, verifies the authority error, and verifies B remains unchanged.
- Offline add setup is now represented by `OfflineRemoteAdmin::open`: it acquires `InstanceLock` before creating/loading `DeviceIdentity` or opening remote configuration. The refusal regression holds the lock and verifies neither `device-identity.pem` nor `remote-access.json` is created.
- Pairing secret decoding now checks the decoded length and copies directly from the zeroizing decoded vector into a pre-zeroized `[u8; 16]`; no plain fixed-size secret array is constructed. The decoder test statically requires `Zeroizing<[u8; 16]>`.

Fix-round verification before the scope correction: `cargo test -p comet` 18/18, `cargo test -p comet-identity` 8/8, remote-access 20/20, instance-lock 2/2, focused strict Clippy passed, format and diff checks passed.

Subsequent full package reruns hit MSVC `LNK1318 Unexpected PDB error; LIMIT`; clearing only the Comet package build artifacts and retrying single-job then stalled in the linker until the five-minute command timeout. No test failed in either linker attempt.

## Scope correction: no migration CLI

- Deleted `apps/comet/src/migration_cli.rs`, the parser variant and dispatch, and all migration CLI tests. `comet migrate` is explicitly rejected by the parser regression.
- Preserved LAN remote commands, `status`, and removal of `login` / `logout` unchanged.
- No dependency was migration-CLI-only after removal: `comet-engine` remains the engine/runtime dependency and `tempfile` remains required by remote CLI safety tests.
- Task 12 removal candidates with no non-migration production callers are: profile selection/copy/marker and `LegacyProfile` / `migrated_from` in `crates/engine/src/local_store.rs`; `crates/engine/tests/local_store_migration.rs`; `DocsStore::snapshot_ids` / `copy_snapshots_to`; and `WorkspaceDoc::owned_by`. `prepare_local_store` currently has only migration-test callers on this branch, but Task 12 must trace startup wiring and retain or replace the local-store initializer rather than deleting it blindly.
- Scope-correction verification: format and diff checks passed; source scans found no migration module, command variant, or dispatch, and found only the intentional parser rejection reference. `cargo test` and `cargo check --tests` were both stopped after bounded waits in the already-recorded degraded MSVC PDB/link environment.
