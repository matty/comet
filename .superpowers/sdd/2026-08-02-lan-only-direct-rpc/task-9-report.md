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

## Fix Round 1

### Client subscription lifecycle

- A real `RpcClient` transport test drains the four required initial watches, fills the bounded outbound transport, and then starts a generic subscription. RED: the inline handshake blocked `Reconnect` beyond 100 ms. Subscription setup is now a `FuturesUnordered` operation polled by the supervisor select loop, so reconnect/shutdown/transcript commands continue while delivery is backpressured.
- RED lifecycle tests showed a dropped receiver retained a quiet server stream, a fast source filled an unbounded public receiver, and 32 dropped quiet subscriptions retained 32 source tasks. The public stream is now a bounded `mpsc::Receiver` (capacity 16). Each forwarder selects source input against `Sender::closed`, applies backpressure, owns the `RpcStream`, and lives in a pruned `JoinSet`.
- Reconnect and shutdown abort and join every forwarder. Supervisor removal first allows a 250 ms structured shutdown/join; a deterministic RED test proved the previous send-then-immediate-abort path never completed shutdown, and is now GREEN with abort only as the fallback.
- Manager transcript handoff is now a timeout-bounded future polled alongside registry and command inputs. RED compilation established the nonblocking handoff API; the test proves another manager input wins while the prior owner withholds acknowledgement. The desired selection starts only after the old owner clears or its supervisor is replaced.

### GPUI ownership fixes

- Header selection and centralized offline/removal cleanup emit `WatchTranscript(None)` before clearing qualified UI ownership. Focused RED/GREEN covers both an explicit server-header switch and automatic offline healing.
- Terminal delayed flush resolves `client_for` the captured chat `ServerRef`, never the current selection. Terminal observer cleanup removes only tabs whose server is removed/non-online; dropping those tab entries cancels their open/subscription/retry tasks while preserving equal raw IDs on other servers.
- Transcript attachment identity is the complete `ServerRef`. Equal B/C raw chat IDs now reset rows, caches, parsers, folds, veils, highlights, scroll/spring state, previews, and in-flight attachment tasks.
- Attachment cache, retry, and load-task keys are `(ServerId, deviceId, path)`. Reads use the captured server client, owner switches cancel old entity tasks, and a late B result can populate only B's cache entry. Composer seeding uses the same qualified key.
- Sidebar projection iterates local-first `server_order`, retains every server header, and includes spaces/chats only for online buckets. With multiple servers, each header is immediately followed by its authoritative qualified children, including nonselected online buckets; offline groups render no children. Click/context actions select the server bucket before applying raw resource IDs. The existing detailed drag UI remains for the single-server presentation.

### Fix-round verification

- Focused desktop regressions: 7 passed (header/offline ownership clear, terminal routing/purge, transcript collision, attachment collision, grouped projection).
- Focused client lifecycle regressions: 6 passed (stalled handshake, bounded buffer, quiet receiver cancellation, repeated forwarder pruning, nonblocking manager handoff, structured stop).
- `cargo test -p comet-client`: 34 unit + 23 integration tests passed.
- `cargo test -p comet-ui --lib`: 323 passed, 0 failed.
- `cargo clippy -p comet-client --tests -- -D warnings`: passed.
- `cargo fmt --all -- --check` and `git diff --check`: passed.
