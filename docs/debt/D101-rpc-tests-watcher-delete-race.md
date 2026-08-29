# D101 — `rpc::tests` hung on a race between a `notify` watcher and its own `TempDir::drop()`

**State:** closed, fixed in PR #128 (`fix/d101-engine-hang`). Not yet merged to `main` at time of
writing; this page and the debt row are part of the same PR that lands the fix. **One caveat before
treating this as a closed loop**: a hang on a test with no watcher exposure under any route this
page names stays unexplained — see "What the first review round got wrong" below and the note at
the end of the Step 5 ruling.

## The question this page answers

`comet-engine`'s `rpc::tests` module hung intermittently on Windows, a different test each time,
sometimes wedging the whole gate. The engine serves concurrent LAN clients by design, so the
first question was whether this was a lock-ordering bug reachable from two concurrent RPC
dispatches — a shipped defect — or a test-only artifact. It turned out to be the latter, but
getting to a fix that is actually correct (not just "passed N runs") took two review rounds and
real debugger evidence at every step. This page is the record so nobody has to re-derive any of
it, and so nobody trusts a future N-run count on this exact bug without first checking the
argument below.

## The mechanism

Captured with WinDbg (`DbgX.Shell.exe -p <pid> -c ".logopen <log>; ~*kv; .logclose; q"`,
non-invasive attach to a live hung process) across four independent hangs. Every one showed the
identical shape: the hung test's own thread blocked inside `tempfile::dir::impl$3::drop` →
`std::fs::remove_dir_all`, stuck in `NtClose`, while a second thread named
`"notify-rs windows loop"` was simultaneously re-arming a `ReadDirectoryChangesW` watch on the
same directory tree (`notify::windows::handle_event` → `start_read`,
`notify-7.0.0/src/windows.rs:300,328`), in response to the very filesystem churn the delete
itself was producing.

The watcher is `crate::spaces::SpacesSync` (`crates/engine/src/spaces.rs:134-152`):
`SpacesSync::reconcile` registers a non-recursive `notify::recommended_watcher` directly on a
`Space`'s `path` field, with no requirement that the path be a git checkout (unlike
`CheckoutDiffSync`, which needs `git rev-parse --show-toplevel` to succeed first and so never
arms for these tests' bare tempdirs — ruled out early and stayed ruled out through both review
rounds).

`rpc::tests`' helpers back every `EngineCore` with a `tempfile::TempDir`, and — before this
fix — fed that **same** directory into `SpacesSync` as a space's watched folder through three
independent, syntactically different call sites (found one at a time, across two rounds of this
task):

1. `WorkspaceHost::create_space(...)` — the 21 test-body calls of the form
   `core.workspace.create_space("space-1", "dev-a", dir.path().to_string_lossy().as_ref(), ...)`.
   This is the bug as it exists on `main`: every one of these tests points a space directly at
   the engine's own data directory.
2. `RunRequest.cwd` — the two shared helpers (`recording_request`, `registration_request`) and 8
   inline `RunRequest` literals all set `cwd: dir.path().to_string_lossy().to_string()`. This
   does **not** independently arm a watcher on `main`: `doc_host.rs`'s claim-on-first-command
   (`ws.claim_chat(chat_id, Some(&request.cwd))`) only reaches `workspace_host.rs`'s
   `space_for_path` — which can auto-create a second space at `cwd` — when the chat row does not
   already exist, and `claim_chat` (`workspace_host.rs:251-254`) returns early whenever it does.
   Every dispatching test calls `create_chat` before it dispatches, so this path is dead on
   `main`. **It matters anyway**: fixing only (1) moves the space's path away from `cwd`, so if
   a later change (or a re-dispatch-after-delete test) ever hits `claim_chat` with no existing
   chat row, `space_for_path`'s `paths_equivalent` match against the (now different) space path
   would fail and it would create a phantom second space at the real `cwd` — arming a second,
   independent watcher on the same tree. Fixing (2) alongside (1) closes that self-inflicted
   opening rather than leaving it for the next person to hit. (An earlier version of this page
   claimed (2) was a live second bug on `main` on its own; it is not — see "What the first review
   round got wrong" below.)
3. `MutateParams::CreateSpace { path: dir.path().to_string_lossy().to_string(), ... }` — one test
   body (`delete_space_keeps_later_chat_purging_until_its_turn`) re-creates `space-1` through the
   production RPC mutate path after deleting it, using the tempdir root directly. This is
   syntactically different from (1) (a struct literal passed to `.mutate(...)`, not a
   `.create_space(...)` method call) and was missed by the first fix's grep-based sweep of
   `create_space(` call sites. **This is not theoretical**: a stress-test campaign built for the
   second review round reproduced this exact test hanging, with the identical
   `TempDir::drop`/`notify-rs windows loop` stack, under completely ordinary execution (no
   injected delay, no `multi_thread` runtime) — this route was caught live, not merely reasoned
   about.

## Why a teardown-ordering fix was rejected, not just deprioritized

The tempting fix is to make `EngineCore::shutdown()` await an explicit `SpacesSync` stop before a
test's `TempDir` is allowed to drop. Read `notify-7.0.0/src/windows.rs` directly (the vendored
source under the cargo registry cache) rather than trusting the crate's public API surface:

- `impl Drop for ReadDirectoryChangesWatcher` (line 535) does `self.tx.send(Action::Stop);
  self.wakeup_server();` and returns immediately — it **posts** a stop request, it does not wait
  for the watcher's background thread to act on it.
- `Watcher::unwatch` (`unwatch_inner`, line 491) is the same shape: send, wake, return.
- `ReadDirectoryChangesServer::start` (line 84) spawns that background thread with
  `let _ = thread::Builder::new()....spawn(...)` — the `JoinHandle` is discarded at the call
  site. Nothing anywhere, inside the crate or out, can synchronously confirm the thread has
  exited.

There is no synchronous, awaitable "fully stopped" signal this crate version exposes, to any
caller, with any amount of code in `spaces.rs` or `lib.rs`. An `EngineCore::shutdown()`-based fix
would still be racing a concurrent delete on the same path in the best case — probabilistic, not
structural — and would only cover the two of this module's 28 tests that call `shutdown()`
explicitly, which is a second, independent reason it does not fully close the hole. Both reasons
apply; the API gap is the one that makes the option unsound even if every test called
`shutdown()`.

## The fix, and why it is structural rather than probabilistic

`notify::Watcher::watch` (`windows.rs`'s `watch_inner`) fails fast, entirely inside `notify` and
before any OS handle opens, when the path is neither a file nor a directory:
`if !pb.is_dir() && !pb.is_file() { return Err(...) }`. No `Action::Watch` is ever sent to the
watcher's background thread in that case, so `add_watch`/`CreateFileW`/`ReadDirectoryChangesW`
are never reached and no OS directory handle is ever opened.

`crates/engine/src/rpc.rs` now routes all three call sites above through one helper,
`unwatched_space_root(dir)`, which joins a subpath (`"space-root-not-on-disk"`) that nothing in
the test suite ever creates, and asserts the invariant it depends on:

```rust
fn unwatched_space_root(dir: &std::path::Path) -> std::path::PathBuf {
    let root = dir.join("space-root-not-on-disk");
    debug_assert!(
        !root.exists(),
        "unwatched_space_root must stay unwatchable: {} exists",
        root.display()
    );
    root
}
```

The `debug_assert!` makes the invariant enforceable rather than merely documented: if any future
test (or a future call site nobody grepped for) ever creates this exact path, the assertion fails
loudly in that test's own run instead of silently re-opening a timing-dependent hang for someone
else to rediscover with a debugger. This was added directly in response to the review finding
that route (3) existed — the assertion is the mechanical answer to "how do we know nothing else
does this," not just a comment saying so.

`create_space` (`workspace_host.rs:424`) and `space_for_path` (`workspace_host.rs:268`) both
write a plain doc-row string with zero filesystem validation, and none of `rpc::tests`'
assertions ever read a space's path back off disk — so a nonexistent space path is a fully
supported value, not a workaround.

**Why this is structural, not "less likely to race"**: the race needs an open Windows directory
handle with `FILE_FLAG_OVERLAPPED` I/O pending (`add_watch` in `windows.rs`, `CreateFileW`) that
stays alive because the watcher keeps re-arming it. The existence check that decides whether that
handle ever gets opened runs synchronously, inside `notify`, before any Windows API call — so
when the path does not exist, the precondition the race needs cannot occur, independent of
scheduling, machine load, or how many other tests are running concurrently. This is categorically
different from "await the stop and hope it finishes before the delete," which remains genuinely
racy even in the best case (see the previous section).

## What the first review round got wrong, and what the second stress test found

The first pass at this fix converted only route (1) (`create_space`). Its own falsification
series (20 required runs) hung at run 16/20, on a different test,
`branch_failure_after_create_restores_lifecycle_admission` — which does not dispatch, builds no
`RunRequest`, and had already been converted to the safe path at route (1). That test's own body
has no code path that arms a `SpacesSync` watcher under the mechanism above.

The base report's isolation series (six-times-each, run alone) covered five of the module's
six known victims — every one but this one, which was found only later, during that first fix's
falsification series, and was never run individually. So unlike the other five, there is no 6/6
isolation data for this test either way; the gap in evidence and the gap in explanation are the
same gap.

The investigation that followed reached for route (2) (`claim_chat`/`space_for_path`) as the
explanation and shipped a fix and a debt-row narrative built on it. **That narrative was wrong**,
caught in the second review round: `claim_chat` returns early whenever the chat row already
exists (`workspace_host.rs:252-254`), true of every dispatching test since they all call
`create_chat` first — so route (2) could not have fired on `main`, and did not explain the run-16
hang either, since it requires a dispatch that
`branch_failure_after_create_restores_lifecycle_admission` never makes. Route (2) only became a
*live risk* as a side effect of fixing route (1) alone (see point 2 above) — real, but a
different claim than "a second pre-existing production bug," and the row should never have said
the latter.

A second stress campaign (looping `cargo test -p comet-engine --lib rpc::tests`, capturing a
WinDbg stack **and** a Sysinternals `handle64` open-handle dump the moment any run exceeded 15s)
reproduced route (3) directly — `delete_space_keeps_later_chat_purging_until_its_turn` hung with
the identical stack. That is what actually surfaced route (3): not code reading, a live
reproduction with tooling built for exactly this purpose.

**`branch_failure_after_create_restores_lifecycle_admission`'s original run-16 hang is not
explained by any route found in this task.** All three real-path routes into `SpacesSync` found
across two review rounds are now closed, and stress testing after the complete fix (routes 1-3,
plus the `debug_assert`) did not reproduce a hang on that specific test — see the task report for
the exact run count. But "did not reproduce under N more runs" is not a mechanism, and this page
says so rather than claiming a closed loop it cannot back with a stack. If this test (or any
other with no `SpacesSync` exposure under the mechanism above) hangs again, that is real evidence
a fourth route exists and this page's state should be revisited.

## Step 5 ruling (production reachability)

Unchanged by either review round: this was a test artifact, not a production RPC-dispatch
deadlock. No two RPC dispatches ever contended for anything in this mechanism at any point across
either fix — one test's own background watcher thread raced that same test's own `TempDir`
teardown, and needed OS scheduling contention (or, per route (3)'s direct reproduction, simply
needed to run at all) to lose. Production never recursively deletes a live space's folder —
`DeleteSpace`/`DeleteChat` remove Comet's own internal rows and journals, never the user's
directory on disk — so this mechanism has no production analogue at any of the three routes.

This ruling covers the three routes this page names and traces to a mechanism. It does not cover
the one hang neither review round could explain ("What the first review round got wrong" above)
— that observation has no known mechanism to rule on, production or otherwise, and is not folded
into this ruling.

## The gap this fix deliberately does not close

`SpacesSync` still has no way to synchronously stop watching (see "Why a teardown-ordering fix
was rejected" above) — that is a real, separate hardening gap in `crates/engine/src/spaces.rs`,
independent of whether any test ever hits it. It was not bundled into this fix because it was not
needed to close D101 and a hang fix is the wrong place to redesign a watcher's lifecycle API.
Tracked as its own row — see the Open table in `docs/debt/README.md`.
