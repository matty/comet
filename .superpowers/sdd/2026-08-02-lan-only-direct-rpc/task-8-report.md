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

- Fix Round 1 added the reply-bearing path needed by model/ref pickers and dependent draft creation; those original implementation risks are resolved below.
- The smoke harness itself needs a Windows implementation or must run on Unix CI; the requested command cannot execute in this environment.

## Fix Round 1

Commit: `35b78eb482c7652789bbf9f2696a86172884372e`

### RED evidence

- Reply success/error tests failed compilation because `SupervisorCommand::Request` did not exist.
- `cargo test -p comet-tui --test render remote_ -- --nocapture` produced two behavioral failures: equal raw chat IDs suppressed the cross-server transcript watch, and a remote cursor action routed through the previously selected server.
- `cargo test -p comet-tui --test render new_session_adopts -- --nocapture` failed because the draft/ref request retained the previous server.
- `cargo test -p comet-tui --test render duplicate_remote_row_cursor -- --nocapture` failed because row rebuild anchoring compared only raw IDs.
- The first full TUI regression run exposed three echo failures after qualification; tracing showed legacy local fixtures and qualified maps used different fallback identities.

### GREEN evidence

- Added a bounded reply-bearing federation request path returning `Value`/`RpcError` through the existing one-active + 32-entry FIFO. Tests cover success, server error, offline error, reconnect cancellation, and overflow.
- Models, refs, sends, and session creation now await federation replies. Adapter tests cover current checkout, reused worktree, new worktree reply-derived cwd, exact ordered calls, and failure stopping before queue/start notification.
- Full `ServerRef` switching now clears stale transcript state and emits the new qualified watch.
- Menu/prompt targets, cursor row actions, new-session cursor adoption, cursor identity, last-visited state, echoes, and asynchronous send failures are server-qualified.
- `cargo test -p comet-client`: 26 unit + 23 integration tests passed.
- `cargo test -p comet-tui`: 84 unit/adapter + 72 render tests passed; one intentional frame dump ignored; doc tests passed.
- `cargo clippy -p comet-client -p comet-tui --tests -- -D warnings`: passed.
- `cargo fmt --all -- --check` and `git diff --check`: passed.
- Smoke was attempted again and remains environment-blocked before launch by Unix-only `fcntl` on Windows.

### Self-review / residual risk

- Request responders are cancelled structurally: reconnect drains active/queued requests with `RpcError::Closed`; shutdown or supervisor loss drops the oneshot and the public adapter maps that to `Closed`.
- TUI adapter tests exercise the actual `FederationCommand::Request` channel boundary and ordered reply handling. They do not launch a real daemon-backed `EngineLink`; the full daemon smoke path remains unavailable on this Windows worker.

## Fix Round 2

Commit: `14e6f7643abde3ae077c165f94a0c5fddb7e4547`

### RED evidence

- `cargo test -p comet-tui --test render action_open_on_duplicate -- --nocapture`: the chat case failed because `open_row` changed `selected_server_id` before calling raw `select_chat`, whose equal raw ID early-returned without a transcript watch.
- Delayed completion/reply tests failed compilation because `StartSession`, `ListModels`, and `ListRefs` had no request generation and the corresponding server-qualified update variants did not exist.
- A stale request-error test failed compilation because model/ref failures still used unqualified `Update::Notice`.

### GREEN changes

- `Action::Open` and click now route chat rows through `select_server_chat` and space rows through `activate_server_space`; no qualified row is split into a server mutation followed by raw selection.
- Drafts and picker/ref requests mint UUID generations. Runtime `SessionStarted`, model, ref, request-failure, and start-failure updates carry both authoritative server/chat and the exact generation.
- The reducer accepts a completion only while its originating pending start, overlay, or draft request is still current. Delayed B results cannot clear or populate a newer C draft/picker, including stale failures.
- Session-start failure removes only its qualified optimistic echo/pending target and never clears another server's draft.

### Verification

- `cargo test -p comet-client`: 26 unit + 23 integration tests passed.
- `cargo test -p comet-tui`: 84 unit/adapter + 77 render tests passed; one intentional frame dump ignored; doc tests passed.
- `cargo clippy -p comet-client -p comet-tui --tests -- -D warnings`: passed.
- `cargo fmt --all -- --check` and `git diff --check`: passed.
- Smoke was attempted again and remains blocked before startup by the Windows worker lacking Unix `fcntl`.

## Fix Round 3

Commit: `cc34afb98890211687d00345c7904a8233a5da77`

### Chosen concurrency behavior

Concurrent starts across server/space contexts are allowed. Each start is keyed by its request UUID and retains its qualified provisional chat, originating space, and optimistic message ID. A reply always cleans its own record; only a reply for the exact live provisional chat may change navigation, pending-chat state, or the composer.

### RED evidence

- `duplicate_space_activation_with_duplicate_target_chat_retargets_qualified_chat` failed because switching from B/`chat-1` to C/`chat-1` changed the server first and then raw-ID selection returned early without `WatchTranscript(C/chat-1)`.
- `current_start_failure_unsubscribes_and_removes_provisional_context` failed because rollback directly cleared `selected_chat` and emitted no `WatchTranscript(None)`.
- `stale_send_failure_cleans_origin_without_overwriting_current_composer` failed because a late B failure restored B's text into C's empty composer.
- `reordered_concurrent_starts_clean_only_their_own_request` failed because the singleton pending start discarded B when C began, so B's later failure was not independently handled.
- The direct-assignment audit added `removing_the_selected_server_unsubscribes_its_transcript`; it failed because server removal directly cleared the raw selection without unsubscribing.

### GREEN changes

- Qualified space activation computes a qualified target from the destination server bucket before mutating selection, then uses the same `select_server_chat` path as qualified chat rows. Keyboard and click activation therefore retarget correctly even when B and C share raw space and chat IDs.
- Pending starts are a request-UUID map containing qualified chat, qualified space, and message ID. Reordered successes/failures remove only their own record and stale completions cannot mutate the live context.
- Current start failure clears the provisional echo and transcript through `select_chat(None)`, returning `WatchTranscript(None)` and restoring the prompt only for the exact live failed chat.
- Federated send failure always removes the qualified origin echo but restores text only when that same qualified chat is currently selected and the composer is empty.
- Removing the selected server now clears transcript data and unsubscribes through the normal selection/effect path; removing the last server also clears the loaded bucket.

### Verification

- Focused Round 3 regressions: 5 passed, 0 failed.
- `cargo test -p comet-client`: 26 unit + 23 integration tests passed.
- `cargo test -p comet-tui`: 84 unit/adapter + 82 render tests passed; one intentional frame dump ignored; doc tests passed.
- `cargo clippy -p comet-client -p comet-tui --tests -- -D warnings`: passed.
- `cargo fmt --all -- --check` and `git diff --check`: passed.
- `python scripts/tui-smoke.py` was attempted and remains blocked before TUI startup on Windows: `scripts/tui_capture.py` imports unavailable Unix-only `fcntl`.
