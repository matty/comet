# D68 — the checklist is scoped per run, so a resumed run's card is partial

**Status:** open, deliberate. Read this before designing the checklist surface (slice 4.4).

## What is true

`MessagePart::Checklist` holds one plan per **run**. The fold writes into the current message's
parts and `SessionStarted` resets the accumulator, so a new run starts with an empty card.

That is native for Codex: `turn/plan/updated` carries `turnId`, its plan belongs to the turn,
and a fresh turn genuinely has a fresh plan.

It is an **imposition** on Claude. Its task list is server-held and session-scoped — the tool's
own description says *"for your current coding session"* — and it outlives the process. Comet
resumes by spawning a new process with `--resume=<session_id>`, and the `Normalizer` is per-run
state.

## What that costs, measured

Recorded 2026-08-13 against Claude Code 2.1.229, promoted as
`crates/capture/tests/corpus/claude/2.1.229/checklist` and `…/checklist-resume`:

- Run A creates three tasks and completes task 1.
- Run B resumes the same session and moves tasks 2 and 3.

**Run B's card holds two items, not three.** Task 1 is invisible in it — completed by a process
whose card belonged to the previous run. And run B's rows start life without subjects, because a
resumed process restates nothing: no `tasks` key on `system/init`, no `TaskList` call, and its
first task frame is a bare `TaskUpdate` for an id it never created.

The fold handles that by creating the item from the update and labelling it from `activeForm`
(see `apply_event`). It is honest, and it is not the same as showing the plan.

## Why it was not solved here

Three options were live. The one taken is the cheapest honest one; the others are recorded so
4.4 does not re-derive them.

1. **Per-run part** (taken). No new state, no migration, and it matches Codex exactly. Costs the
   partial resumed card described above.
2. **Reconcile from `TaskList`.** Rejected on evidence: the model never called `TaskList` on any
   recorded run, so a checklist waiting for one would stay empty for the whole resumed run.
3. **Carry the list across runs as engine state.** Correct for Claude, wrong for Codex without a
   second scope concept, and it needs a migration for documents already written. That is its own
   slice, and it is what this row exists to point at.

## What 4.4 should do about it

**Look at a multi-run chat before committing to a card design.** The question is not whether the
data is right — it is — but whether a card that says less than the agent knows reads as a bug to
a user who scrolled up and saw three items a moment ago.

If it does, the fix is option 3 above, and it is a slice rather than a tweak. Deciding that
against a rendered surface is exactly why 4.2 and 4.3 shipped no UI.

## Related

- **D52** — there is still no way to preview a UI state without producing it for real, which is
  what makes "look at a multi-run chat" more expensive than it sounds.
- **D67** — an item leaving a Claude list has never been observed; if a delete exists, a per-run
  card would show a dropped step until the next snapshot.
