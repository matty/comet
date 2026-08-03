# Task 9 report: GPUI federation

## Implementation

- Commit: this commit (hash reported after commit)
- Baseline: `92ad82f`
- Bootstraps `comet_client::Federation` from the trusted local RPC connection and consumes one federation event pump for server resources and transcripts.
- Stores authoritative `ServerState` buckets in `HashMap<ServerId, ServerState>` with a separate local-first `server_order`; disconnect frames retain the server header while clearing its children, and removal/offline selection heals to the first online local-first bucket.
- Qualifies selected chats/spaces, optimistic echoes, composer drafts/attachments, picker owners, terminal tabs, drag state, and asynchronous completion ownership with `ServerRef` or `ServerId`. Residual raw-id shell caches have an explicit server owner and are cleared atomically on server changes.
- Routes composer, transcript, repo/ref/worktree, terminal, diff, attachment read/upload, archived/device/account, and sidebar mutations through a selected `ServerClient`. Raw resource IDs remain in RPC parameters; the authoritative server travels separately through the federation command.
- Adds a reply-bearing federation subscription path for terminal and diff streams. The supervisor proxies transport streams into supervisor-owned channels so reconnect/shutdown closes returned streams without a retained `RpcClient` pending guard.
- Renders local-first persistent server headers with online, connecting, offline, unreachable, identity-changed, and incompatible-version states. Only the selected online bucket projects children, so non-online servers cannot show stale resources.

## RED evidence

- `remote_offline_heals_qualified_selection_to_local` and `duplicate_raw_chat_ids_route_to_the_qualified_server` were added before the GPUI federation model. The focused build failed because `apply_federation`, qualified selections, `selected_server_id`, and `call_for` did not exist.
- `subscribe_returns_the_server_stream_and_errors`, `reconnect_closes_a_returned_subscription`, and `terminal_subscription_command_uses_the_chat_server` were added after removing an initially premature subscription implementation. They failed to compile because the federation/supervisor subscription commands and adapters did not exist.
- The first subscription implementation exposed the raw RPC stream and the reconnect test hung. Diagnosis showed its pending guard retained the pending sender after transport replacement. The final supervisor-owned proxy receiver fixes that ownership error.
- The first full UI run exposed one echo regression (`echoes_show_until_doc_frame_confirms`): legacy local fixtures had no active bucket. The qualified selected-chat fallback fixed it before the final full run.

## GREEN / verification

- Focused federation state/routing tests: 4 passed, including duplicate raw chat IDs, scoped transient removal, offline healing, and terminal subscription routing.
- Focused client subscription tests: 2 passed, covering server values/errors and reconnect closure.
- `cargo test -p comet-client`: 28 unit tests + 23 integration tests passed; doc tests passed.
- `cargo test -p comet-ui --lib`: 317 passed, 0 failed.
- `cargo clippy -p comet-client --tests -- -D warnings`: passed.
- `cargo fmt --all -- --check`: passed.
- `git diff --check`: passed.
- `rg -n "targetDeviceId|target_device_id" crates/ui crates/client`: only the two negative routing assertions match; production construction has no matches.

## Strict-clippy baseline blocker

`cargo clippy -p comet-ui --tests --no-deps -- -D warnings` reaches `comet-ui` successfully but exits 1 on 15 repository lint findings outside this task's routing changes. Examples include existing dead test/platform helpers in `app_menus.rs` and `sound.rs`, markdown type/doc lints, `NavHistory::len` without `is_empty`, and existing style lints in composer/pickers/accounts. Normal compilation and the complete 317-test UI gate pass.

## Self-review

- Reply calls retain the Task 8 FIFO/error behavior. Subscriptions are intentionally not queued as calls: they return a live stream, are tied to the current supervisor generation, and close on reconnect, shutdown, or receiver drop.
- Transcript frames apply only when their full `ServerRef` equals the current selection. A late send error removes only its qualified echo and restores composer state only if the same owner/generation remains selected.
- Picker async tasks capture both qualified owner and generation; server switches cancel old tasks and stale completions are ignored.
- A server removal purges only that server's echoes/selections and then heals to local-first state. Non-online frames replace the bucket with empty children before any render projection.
- The shell shows all server headers persistently and projects children for the selected online server. This keeps existing GPUI sidebar/tab interaction semantics while making the active authoritative boundary explicit.

## Transitional local control path

`EngineHandle` remains the owner of local attach/embed/shutdown. Auth-status, update-status, the local-device badge probe, and sign-in/org gate calls still use its trusted-local RPC client because those calls must work before federation can complete `ServerHello` under the current authentication gate. All resource operations are federated; removing this bootstrap-only local administration path belongs with the later auth/bootstrap migration.
