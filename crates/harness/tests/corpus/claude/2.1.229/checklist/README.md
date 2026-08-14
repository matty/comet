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

**`comet-provider-sanitize` does not redact model prose inside tool inputs or results** — a
`TaskCreate` input's `subject` and `description`, and a `TaskUpdate`'s `activeForm`, survive
verbatim, as anyone can confirm by reading `events.jsonl`. It redacts `thinking` blocks and
typed identifiers, and has no rule for these. On a real session those fields would carry
whatever the operator's work is about.

The scenario prompts in `crates/harness/src/capture/checklist.rs` therefore keep every task
subject meaningless. **Do not "improve" them into something descriptive**, and do not re-record
this pair from an ad-hoc prompt. `docs/debt/README.md` **D64** tracks the sanitizer gap; until it
closes, the prompt is the control.

## Claims

Three, all in `../../../index.json`, all consumed by `crates/harness/src/claude/normalize.rs`:

| Claim | Rests on |
| --- | --- |
| `claude-task-create-result-shape` | the create call and its result — the assigned id is on the result and on no input |
| `claude-task-update-status-transition` | the update call and its result — `statusChange` on the result, `activeForm` only on the input |
| `claude-resumed-run-updates-an-uncreated-task` | the resumed init plus the call and result that move task 2 |

`crates/harness/tests/capture_corpus/promoted_evidence.rs` asserts the payloads behind all
three. That test exists because **the structural validator alone does not check them**: pointing
a claim's selector at a neighbouring sequence left `--test capture_corpus` fully green until the
payload assertions were added. A claim without one is decoration.

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
