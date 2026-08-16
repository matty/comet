# D81 — displaced runs escape the ownership census after bounded interruption

**Status:** open, unowned, and pre-existing. This is deliberately outside Task 9's bounded
repair and does not invalidate that delta's bounded PASS.

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
