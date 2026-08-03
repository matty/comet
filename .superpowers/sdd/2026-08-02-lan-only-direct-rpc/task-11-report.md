# Task 11 report — LAN remote CLI and local status

## Result

- Added `comet remote add|list|remove|listen|pair|clients|revoke`.
- Added explicit `comet migrate --from ORG/USER` and removed the `login` / `logout` commands and auth CLI module.
- Replaced account-oriented status with local data-dir, engine/IPC, LAN listener, paired-client, and direct-remote status.
- Active engines are administered through local RPC. Offline reads and mutations acquire `InstanceLock`; an owned lock with unavailable IPC fails closed rather than editing configuration behind the engine.
- Added Windows `InstanceLock` exclusivity using file-sharing rules so the same safety contract holds on Windows.
- Daemon capture no longer includes Edge, token, org, WorkOS, or callback variables. `COMET_RELEASES_URL` remains as a separate distribution-only override, and `comet update` uses it.

## TDD evidence

The initial parser RED failed on the absent `remote_cli` module and absent `Remote` / `Migrate` variants. After the minimal parser slice passed, focused behavior REDs failed on the absent grouped-secret decoder, remote output and stable-ID handling, migration selector/path validation, and active-engine migration guard. The daemon capture test then failed because `COMET_RELEASES_URL` was absent and online-service variables were still captured. A final audit RED showed that offline status could report a persisted stale `Online` state; offline snapshots now normalize `Online` and `Connecting` to `Offline`.

The migration guard exposed that `InstanceLock` did not lock on Windows. The existing exclusivity/holder/reacquire tests were expanded to Windows and the migration test reproduced the unsafe second acquisition before the Windows file-sharing implementation was applied.

## Safety audit

- Pairing secrets are never CLI arguments. `remote add` reads from the terminal with `rpassword`, moves the returned allocation into `Zeroizing<String>`, decodes into `Zeroizing<[u8; 16]>`, and passes that to the hardened pairing client.
- `remote pair` intentionally prints the one-time server-side secret and expiry. Its deserialized string allocation is immediately moved into a zeroizing owner before output; no secret-bearing type derives `Debug`, and no secret is logged.
- Remove and revoke accept stable server IDs, not names, so duplicate display names cannot select the wrong row.
- Migration accepts exactly two safe segments, requires the exact `data_dir/orgs/ORG/USER` directory, rejects a selected symlink or canonical escape, acquires the engine lock before `prepare_local_store`, and never accepts an arbitrary filesystem path.
- On Windows, the holder shares reads (for the PID probe) but denies a second writer. Closing or process exit releases the kernel-owned share lock. The PID text may remain after exit, but `holder` tests lock availability first, so stale metadata is ignored; drop/reacquire is covered by tests.

## Verification

- `cargo fmt --all -- --check` — passed.
- `cargo test -p comet` — 15 passed.
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
