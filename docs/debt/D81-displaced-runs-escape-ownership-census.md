# D81 — displaced runs escape the ownership census after bounded interruption

**Status:** merged [PR #220](https://github.com/matty/comet/pull/220). This was deliberately
outside Task 9's bounded repair and did not invalidate that delta's bounded PASS.

## What happens

Normal dispatch bounded-waits for an older run. When the wait does not settle it, replacement
registration inserts the new run in the one `runs[chat_id]` slot and drops the displaced
`RunHandle`. The older `drive_run` task is neither joined nor represented in a separate ownership
registry.

This can occur while provider setup is awaiting `harness.run`, or when a closed or full steering
mailbox causes normal routing to fall through before the task has settled. The replacement can
then finish, so `has_live_run` sees no run because it consults only the visible slot. Final purge
removes the handle, retires the journal, status, and sidecars, and permits the chat id to be
recreated without observing the displaced task.

## Why the hidden task still matters

If the old task later returns a startup error, it appends `Error` and `Done` before `finish_run`.
`finish_run` publishes terminal status before its run-id-guarded removal. A late stream can also
reach sidecar, journal, harness-session, message-activity, and status writes. Once a successor
generation is revived and the lifecycle gate is clear, those effects are keyed by `chat_id`, so
they can land on the successor.

The relevant paths are `crates/engine/src/sessions.rs:599-627`, `:767-790`, `:857-906`,
`:1313-1315`, `:1372-1400`, and `:1867-1889`, with the purge census at
`crates/engine/src/doc_host.rs:897-920`.

## Why Task 9 did not repair it

The approved lifecycle table explicitly leaves the row "old task may later settle" unchanged.
Task 9 repaired terminal handoff for the mapped run and rejected-resume admission for a captured
document generation. It neither changes displaced runs nor claims to own all background task
lifecycle accounting. Treating this as a repair regression would extend the bounded task into a
new ownership model without design or coverage.

## What closing it requires

A later lifecycle slice must choose one durable rule: keep every displaced task in the ownership
census until it settles, or make each late effect fail unless it proves the current document
generation and run identity. Either direction must cover provider startup, streamed effects, and
final purge/recreation interleavings.

## Resolution

The repair chose the ownership-census rule without changing routing semantics. `runs` remains the
one current handle used for steering, input, approvals, and interruption. A separate
`run_owners[chat_id]` set records every exact run id from lifecycle-gated registration until that
run finishes all of its terminal writes. Replacing a route still drops the old handle immediately
to drain questions and approvals nobody can answer, but that drop no longer erases the old task's
ownership pin. A stale finish releases only its own id and cannot remove the successor route.

DeleteChat and DeleteSpace finalizers interrupt the current route as before, then wait on an
ownership-change watch before final purge. They subscribe before checking the set, so the last
release cannot be missed. The purge lifecycle keeps same-id registration closed for the duration,
and engine shutdown cancels the wait without treating cancellation as proof that cleanup is safe.
DeleteSpace fans interruption out to every affected chat before waiting on any one chat's census,
so a displaced owner cannot leave later routes running. An exact-run-id drop guard owns retirement
for the detached task itself, including provider panics and cancellation; its stale drop cannot
remove a replacement route.

`rpc::tests::displaced_startup_keeps_same_id_reuse_closed_until_its_late_effects_finish` exercises
the production path: an old provider startup ignores interruption past the five-second replacement
bound, its successor settles, deletion and same-id recreation remain blocked, and the old startup's
late `Error`/`Done` are finally purged before reuse. Session unit tests cover both finish orders,
the stale-finish route guard, the final-owner wakeup, and shutdown cancellation. Additional RPC
regressions cover a two-chat DeleteSpace cancellation fan-out and panic-unwind retirement followed
by deletion and same-id reuse.
