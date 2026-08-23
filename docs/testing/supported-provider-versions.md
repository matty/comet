# Supported provider versions

Comet drives provider CLIs it does not ship. This file records the **oldest version each adapter
is written against**, so a decode can be deleted when no supported version can produce it
instead of being carried forever on "someone might have an older one".

| Provider | Floor | Corpus evidence |
| --- | --- | --- |
| Claude Code | **2.1.228** | `crates/harness/tests/corpus/claude/2.1.228/`, `…/2.1.229/`, `…/2.1.233/`, `…/2.1.241/` |
| codex-cli | **0.147.0** | `crates/harness/tests/corpus/codex/0.147.0/` |

## What the floor means

**A tool or frame no supported version emits does not get a decode.** Writing one produces a
path that ships never having been constructed, which is the failure
[`.agents/rules/optional-wire-fields.md`](../../.agents/rules/optional-wire-fields.md) exists to
prevent — its fixtures always supply the field, and the live case never runs.

Two removals made under this rule, both in slice 4.3:

- **Claude `TodoWrite`.** Absent from 2.1.229 (the CLI's own `ToolSearch` denies the name) and
  absent from 2.1.228 (35 tools in `system/init`, `TodoWrite` not among them, probed
  2026-08-14). The replacement is `TaskCreate` / `TaskUpdate` / `TaskList`.
- **Codex `todoList` items.** Never observed on 0.147.0; the plan arrives as
  `turn/plan/updated`, not as an item. It now falls through to an Unknown diagnostic, so if one
  ever appears we hear about it.

## What the floor does NOT mean

**It says nothing about persisted documents.** A user's existing transcripts hold parts written
by any earlier build, and those must keep decoding whatever CLI they now run. `ToolCall::Todo`
survives for exactly that reason and is unaffected by anything here — see its own doc comment
in `crates/proto/src/agent.rs`, and the test that pins it
(`a_persisted_todo_tool_part_still_decodes`).

Provider-version support and document-format support are different axes. Conflating them is how
a cleanup blanks somebody's history.

## Raising the floor

Raising it is a deliberate change, not a consequence of upgrading a local CLI:

1. Record a capture at the new version through `comet-provider-capture` and promote it
   (`provider-captures.md`).
2. Update the table above **and say what became removable**, with the evidence that no supported
   version emits it.
3. Leave the old version's corpus entries in place unless they are actively wrong. They are the
   record of what that version did, and an assertion resting on them stays valid.

The corpus is what makes this safe: it holds real frames per version, so "does any supported
version still send X" is a question with an answer on disk rather than a guess.

**This is not enforced by anything.** The floor is prose, and nothing fails when the installed
CLI drifts below it or when a decode outlives its last emitting version — see `docs/debt/`
**D69** and **D70**.
