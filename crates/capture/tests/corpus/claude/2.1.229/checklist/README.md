# The checklist pair

`checklist` and `checklist-resume` are **one observation in two processes** and only mean
something read together. Both were recorded through `comet-provider-capture` and sanitized by
`comet-provider-sanitize` — unlike the `subagent` scenario beside them, which was hand-sanitized
from a Python rig capture (see its own README, and `docs/debt/D63-…`).

- **`checklist`** — a fresh session creates two tasks and drives the first through
  `pending → in_progress → completed`.
- **`checklist-resume`** — a *second process*, resuming that session's id, moves task **2**,
  which it never created. Its `system/init` restates no task list at all: there is no `tasks`,
  `todos` or `plan` key on it, and no `TaskList` call anywhere in the run.

That second point is the reason the pair exists. Claude's task list is session-scoped and
server-held; Comet's run boundary is not, and a resumed run is told nothing about the list it
inherits. Anything accumulating a checklist per run therefore receives a status change for an
item it has no subject for.

## The subjects are deliberately content-free, and must stay that way

`"Alpha step"`, `"Beta step"`, `"Working the first step"`. That is not incidental phrasing.

**Keep it that way regardless of what the sanitizer does.** Under the earlier blocklist, a
`TaskCreate` input's `subject` and `description`, and a `TaskUpdate`'s `activeForm`, had no
matching redaction rule and survived verbatim — `docs/debt/README.md`'s **D64** named that gap.
The allowlist-sanitizer stage closed the general mechanism: none of `.message.content[].
input.subject`, `.description`, or `.activeForm` is on
`crates/capture/src/allowlist/claude.txt`, so a field nothing on that list names now
defaults to redacted rather than surviving by omission. Confirm it yourself in `events.jsonl`:
sequence 55's `subject` reads `<V215>` and its `description` `<V214>`, and sequence 88's
`activeForm` reads `<V246>` — not the prompt text.

That closure is not a reason to relax the prompt. The scenario prompts in
`claude_checklist_prompt`/`claude_checklist_resume_prompt`
(`crates/capture/src/record/scenarios/claude.rs`) still keep every task subject meaningless, on purpose:
writing something descriptive here would mean trusting the sanitizer to keep catching it forever.
`docs/debt/D73-tool-argument-union-paths.md` (closed at the stage-6 promotion) named the live way
that trust could be misplaced — several tool-argument paths
(`.message.content[].input.status`/`.taskId`/`.type` and their `tool_use_result` siblings) were
allowlisted as a *union* across five known tools, so an unreviewed sixth tool landing content on
one of those paths would not have been caught by anything that reviews paths rather than values.
The seven lines are gone from `claude.txt` now, closing that specific risk, but the general
lesson stands: a synthetic subject has nothing in it worth catching either way, whatever a future
capture lands on a neighboring field.

**Do not "improve" them into something descriptive**, and do not re-record this pair from an
ad-hoc prompt.

## Assertions

Three, all asserted in `crates/harness/src/claude/normalize.rs`, which reads them by sequence:

| Assertion | Sequence(s) | Rests on |
| --- | --- | --- |
| task create result shape | 55/64 | the create call and its result — the assigned id is on the result and on no input |
| task update status transition | 88/93 | the update call and its result — `statusChange` on the result, `activeForm` only on the input |
| resumed run updates an uncreated task | 2/50/55 (`checklist-resume`) | the resumed init plus the call and result that move task 2 |

`crates/capture/tests/capture_corpus/corpus_frames.rs` asserts the raw payloads behind the same
sequences. That file exists because **presence alone does not prove content**: pointing a test's
selector at a neighbouring sequence once left `--test capture_corpus` fully green until the
payload assertions were added. A sequence number without one is decoration.

## The Codex half has no automated live check, deliberately

`real_claude_task_tools_persist_a_checklist` (`crates/engine/tests/e2e.rs`, `#[ignore]`) drives
a real Claude run through the engine and reads the persisted document and run journal back.
**There is no Codex equivalent, and one was written and removed.**

Codex publishes a plan at its own discretion. Two identical runs on 2026-08-14, same prompt,
same CLI (0.147.0): the first published a three-step plan, the second published none within four
minutes. A test asserting one is a coin flip, and a coin-flip test in the ignored set is worse
than none — it trains whoever runs it to disregard a failure.

The successful run is recorded here instead, read back out of the real persisted document:

```text
Checklist { id: "checklist", explanation: None, items: [
  { id: "0", text: "Read README.md",                      status: InProgress },
  { id: "1", text: "Count the lines in notes.txt",        status: Pending    },
  { id: "2", text: "Report both results in one sentence", status: Pending    } ] }
```

One card for the whole snapshot, every step named, and the tri-state carried distinctly. The
decode itself is pinned by `plan_tests` in `crates/harness/src/codex/normalize.rs` against the
capture's verbatim payload, and the fold and persistence paths are provider-agnostic — they are
the same code the Claude live test exercises.

**Worth knowing if you retry it:** that run never *completed*. Codex's sandboxed
`Get-Content README.md` failed twice, it escalated to an approval, and a test that answers no
approvals parks forever. The plan is published well before that point. See debt **D13** for the
Windows sandbox behaviour.
