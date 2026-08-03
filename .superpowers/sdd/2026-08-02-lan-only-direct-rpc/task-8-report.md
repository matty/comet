# Task 8 report: TUI federation

## Implementation

- Commit: `7b7ab7c2b4c40fa0bd5177bd6b52cb5160a589bd`
- Baseline: `b68e0740d14def81e761dec71f8bbbcde31dc5e2`
- Replaced the TUI's single-engine subscription supervisor with `comet_client::Federation` after the existing trusted-local daemon attach/spawn path.
- Added per-server state in local-first federation registry order, persistent connection-status server rows, online child grouping, and immediate child removal for offline servers.
- Qualified transcript selection with `ServerRef` and all RPC effects with `server_id`; duplicate raw IDs therefore route through the selected authoritative server.
- Removed TUI `targetDeviceId` construction while retaining detach/shutdown ownership semantics.

## RED

Added `offline_remote_stays_visible_without_cached_children` and `duplicate_remote_chat_ids_route_to_selected_server` before production edits.

Command:

```text
cargo test -p comet-tui --test render remote -- --nocapture
```

Result: exit 1. The pre-federation TUI failed compilation at its obsolete single-engine `RpcStream` boundary (`expected UnboundedReceiver<Value>, found RpcStream`), confirming the Task 7 client boundary had not been consumed.

## GREEN / verification

- Focused federation render tests: 2 passed, 0 failed.
- `cargo test -p comet-tui`: 82 unit tests and 68 render tests passed; 1 intentional frame-dump test ignored; doc tests passed.
- `cargo fmt --all -- --check`: passed.
- `git diff --check`: passed before commit.
- `rg -n "targetDeviceId|target_device" crates/tui/src crates/tui/tests`: no matches.
- `python scripts/tui-smoke.py`: blocked before TUI startup on this Windows worker because `scripts/tui_capture.py` imports the Unix-only `fcntl` module (`ModuleNotFoundError`).

## Self-review

- Verified daemon attachment remains outside federation construction and `EngineLink::Drop` still requests shutdown only from the client supervisor; it does not stop the detached daemon.
- Verified every `Command::Call`, send, reconnect, model/ref request, and draft-session effect carries a server ID; transcript watches carry `ServerRef`.
- Verified server headers follow `ServerSnapshot` order, online resources are grouped below their authoritative server, and any non-online state suppresses children.
- Verified row activation first switches the selected server bucket, so colliding raw space/chat IDs cannot route through the previously active server.

## Risks / follow-up

- The current Task 7 `FederationCommand::Call` API is fire-and-forget. Existing model/ref picker calls can be routed correctly but cannot return typed reply payloads to the TUI through that API yet.
- Draft creation that requires a newly-created worktree path cannot preserve the old dependent reply chain through fire-and-forget federation calls; current-checkout and already-materialized paths remain routable. A reply-bearing federation command is needed to restore that workflow fully without bypassing federation.
- The smoke harness itself needs a Windows implementation or must run on Unix CI; the requested command cannot execute in this environment.
