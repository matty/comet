# D82 — detached chat-id work lacks exact generation binding

**Status:** open, unowned, and pre-existing. This is deliberately outside Task 9's bounded
repair and does not invalidate that delta's bounded PASS.

## Auto-titling

Auto-titling is spawned with only `chat_id`, a harness, an old prompt, and a working directory.
It carries no lifecycle token, document identity, or run handle. If it is delayed until after
delete, purge, and same-id recreation, its first workspace read can see the untitled successor,
start a provider with the old prompt, and rename the successor chat and worktree branch. If it
read the old row before deletion, its post-provider read can instead see the successor and make
the same durable writes there.

The paths are `crates/engine/src/sessions.rs:797-804`, `:2330-2336`, and
`crates/engine/src/titles.rs:60-71` and `:81-140`.

## In-flight command drain

The command drain has the same generation hole through a different retained value. It holds an
old `Arc<ChatDocHandle>`, but final purge removes only the mapped handle and does not wait for an
in-flight drain. A drain descheduled after its one-time host check can resume after same-id
revival. Its `Run`, dead-run `Steer`, and orphan-input fallbacks call ordinary
`SessionsEngine::dispatch` by `chat_id`, with no exact-handle or current-generation validation.
An old command can therefore write or provider-start against the successor.

The lifecycle paths include `crates/engine/src/doc_host.rs:897-920`, `:967-1033`, `:1059-1119`,
and `:1174-1195`.

## Why Task 9 did not repair it

Neither auto-titling nor command drain machinery was changed by the bounded terminal-handoff and
rejected-resume repair. The review found the common missing property, exact generation binding
for detached chat-id work, but no current-delta failure. Keeping the issue together prevents an
ad hoc title-only patch from leaving old drain dispatches behind.

## What closing it requires

A later lifecycle slice should bind both kinds of detached work to a current document or handle
generation before provider start and before every durable effect or ordinary dispatch. The
validation must be repeated after awaited provider work and after a drain resumes, not only when
the task is first queued.
