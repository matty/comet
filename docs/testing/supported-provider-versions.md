# Supported provider versions

Comet drives provider CLIs it does not ship. This file records the **oldest version each adapter
is written against**, so a decode can be deleted when no supported version can produce it
instead of being carried forever on "someone might have an older one".

| Provider | Floor | Corpus evidence |
| --- | --- | --- |
| Claude Code | **2.1.228** | `crates/capture/tests/corpus/claude/2.1.228/`, `…/2.1.229/`, `…/2.1.233/`, `…/2.1.241/` |
| codex-cli | **0.147.0** | `crates/capture/tests/corpus/codex/0.147.0/` |
| Grok | **1.0.5** | `crates/capture/tests/corpus/grok/1.0.5/` |

**Grok's row is the whole evidence, not the oldest of several.** 1.0.5 is the only version any
Grok capture has ever been taken at, so the floor states what the adapter is written against
rather than a version it was deliberately raised to. **Hermes still has no row**: it is a
registered `Harness` like Grok, but nothing has ever run it here (a Python 3.14 install failure,
`docs/debt/` D104), so there is no corpus directory to point at and a floor would be a guess —
D110's remaining half.

**A promoted corpus does not always earn a floor row.** codex-acp 1.7.0 and claude-agent-acp
0.70.0 are both promoted (`crates/capture/tests/corpus/{codex-acp,claude-agent-acp}/`) with no
row above. This table is about *adapters* -- CLIs a Comet `Harness` actually drives and
therefore might one day drop a decode for -- and Comet registers no `Harness` for either:
`comet_proto::agent::HarnessId` has no `CodexAcp`/`ClaudeAgentAcp` variant, only `ClaudeCode`,
`Codex`, `Cursor`, `Grok`, `Hermes` and `Mock`. codex-acp and claude-agent-acp exist in the
corpus purely as the ACP protocol's own two-speaker comparison points for the agents Comet
does drive over ACP (`crates/harness/src/acp/{grok,hermes}.rs`). "No supported version can
produce it" has no floor to be measured against for a CLI nothing decodes. If either ever
gains a real `Harness`, add its row then, from the same evidence already sitting in the
corpus.

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

## What is enforced, and what is still prose

**The table and the corpus can no longer disagree.**
`crates/capture/tests/capture_corpus/version_floor.rs` fails when a floor cites a corpus
directory that holds no promoted scenario, and when a promoted provider Comet actually drives has
no floor row and is not listed as deliberately floorless. That closes the clause of D69 about the
floor not being tied to its evidence.

**A decode outliving its last emitting version is still only partly caught.**
`acp_decode_coverage.rs` does it for ACP `sessionUpdate` kinds and `codex_method_coverage.rs` for
Codex JSON-RPC methods, both in both directions — a decoded name with no capture behind it, and a
captured name nobody has ruled on. Those are the two surfaces whose vocabulary the corpus records
(`.params.update.sessionUpdate` and `.method`).

**Tool names are the gap, and Codex's half is capture-blocked**: no promoted scenario exercises a
Codex tool call at all, so there is no dotted path to declare as a discriminator and nothing to
compare a decode against. Claude's `.message.content[].name` and `.event.content_block.name` are
declared and could carry the same lint today.

**Nothing checks the installed CLI against the floor**, and nothing states a coverage or
retirement policy for corpus versions — **D70**.
