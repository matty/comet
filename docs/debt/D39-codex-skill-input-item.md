# D39 — Codex's command surface, and why 2.4 skipped it

**The ruling, 2026-08-11, during slice 2.4's capture: Codex is out of the `/`
menu for that slice, deliberately.** Recorded because the reasoning is a wire
design, not a preference, and because the next reader will otherwise repeat the
capture to find it.

## Codex does not parse slash commands at all

Claude's CLI expands `/name` itself: sent as a plain `stream-json` user message
it comes back expanded, and an unrecognized one is rejected locally in 20ms with
no model call. **Codex's app-server does neither.** A `turn/start` whose input
text is `/xyzzy-not-a-real-command` echoes it verbatim into the `userMessage`
item and hands it to the model, which reasons for eight seconds and replies that
it is "not a recognized command here" — a full turn, real tokens, for a menu
selection.

So a `/` menu that inserts text on Codex would be a confident wrong answer, the
shape this phase has now paid for three times (`--bare`, the logged-out model
list, the two-sided `CODEX_HOME`).

## What it has instead, and it is better

`skills/list` — **0.36s, no thread required**, 17 entries on this machine.
Params are `{cwds?: string[], forceReload?: bool}`; `cwds` is an array, so one
call covers several directories, and it defaults to the app-server's own. Each
entry carries `{name, description, shortDescription, path, scope, enabled,
interface{displayName, …}}` with `scope` one of `user | repo | system | admin`.

And the invocation channel is **not text**. `turn/start`'s input union has

```
{ "type": "skill", "name": "<name>", "path": "<path from skills/list>" }
```

beside `{"type":"mention", name, path}` for files (`schema.gen.ts:11414`,
`SkillUserInput`). Driven live, the server accepted it, echoed it in the
`userMessage`, and the model could describe the skill it had been handed. That
is what `skills/list`'s `path` is for.

Evidence and raw JSONL: `captures/2026-08-11-slash-command-expansion.md`
(runs `codex1`, `codex4`, `codex5`) in the artifacts directory.

## Why it is not built here

A skill pick is not text, so it cannot ride the prompt string. It has to survive
composer draft → proto → engine → harness as a structured reference, which is:

- a new part or field on whatever carries a queued prompt, hence a
  `PROTOCOL_VERSION` question (read the constant's own doc comment, not this
  page);
- a draft model in the composer that can hold non-text elements, which is
  adjacent to **D25**'s ruling about that surface being full; and
- a second consumer immediately: Comet sends `@` file mentions as plain text
  today, and Codex's `{"type":"mention"}` item is sitting unused for exactly
  them.

One design, two consumers, its own rendered check. Folding it into the slice
whose job was to make Phase 2 visible would have meant shipping a wire change
nobody specified.

## How to apply

The owner is the slice that takes on structured composer input. When it lands,
**D41** (no command-cache refresh inside a boot) is worth closing alongside it:
Codex publishes a `skills/changed` notification, which is the trigger that side
is missing, and Claude has no equivalent.
