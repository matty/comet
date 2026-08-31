# D82 — detached chat-id work lacks exact generation binding

**Status:** resolved in `fix/d82-detached-generation-binding`, merge pending.

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

## Resolution

The existing mapped `Arc<ChatDocHandle>` is the generation identity; no second counter was added.
Both automatic title sites now carry that exact handle into their detached task. Discovery, every
provider attempt, provider results, the title write, and worktree-branch effects revalidate it;
synchronous workspace writes run while the lifecycle gate remains held.

Command drains likewise stop unless their retained handle is still mapped after every resume and
await. Processed-ledger and command-status writes are gated atomically, workspace backfills and
orphan-input resolution are gated, and any run fallback dispatch registers through the exact
handle rather than reopening by chat id. An exact run id also bounds replacement interruption, so
a stale fallback cannot interrupt a successor.

Three deterministic RPC regressions delete and recreate the same chat id while work is paused:
one resumes after title discovery, one returns from the title provider, and one resumes an old
command drain after its host check. They prove that no stale provider starts, title lands, or old
command reaches the successor, while the existing title and command suites preserve the live
generation paths.
