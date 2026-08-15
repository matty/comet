# D75 — the subagent read-back files carry a different, unenforced redaction standard

**Not a defect in the allowlist or in `sanitize_dir`** — both do exactly what they are scoped to
do. Found during Task 6 (documentation) of the allowlist-sanitizer stage, while sweeping
`tests/corpus/` for anything the new machinery does not actually reach.

## What is in the corpus, and what reached it

`crates/harness/tests/corpus/claude/2.1.229/subagent/` holds two different kinds of evidence, and
its own `README.md` already says so: `manifest.json` + `events.jsonl` are raw provider wire frames
from a `comet-provider-capture` run, sanitized by `comet-provider-sanitize` like every other
scenario in the corpus. `read-back-doc-snapshot.json` and `read-back-run-journal.jsonl` are
evidence from a **separate session** — a live run driven through Comet's real `comet headless`
engine, read back out of the persisted document and run journal — hand-sanitized by an earlier
slice (4.2 task 9) to its own standard, before this stage's allowlist existed. `allowlist_property.rs`
says so explicitly in its own doc comment: these two files are outside `sanitize_dir`'s remit, and
its total property does not walk them.

That was true before this stage and stays true after it: nothing this stage built enforces
anything about these two files. They are not provider wire frames, so `sanitize_dir` was never
going to touch them, and the total property inspects `events.jsonl` files by design.

## What the read-back files actually publish

Read directly, `read-back-run-journal.jsonl` and `read-back-doc-snapshot.json` carry, verbatim:

- Full assistant and reasoning prose from the subagent's real answer, not a placeholder.
- A roster of roughly thirty-five tool names available to that run.
- `agentType: "general-purpose"` — one entry from the exact subagent-roster shape this stage
  removed from `events.jsonl` everywhere else (`.mcp_servers[].name`, `.tools[]`, and the same
  family of installed-tooling identity Task 1 excluded).

## Why this is not the leak this stage exists to close

The stage's own motivating incident — the recording user's OS build, installed plugin names and
versions, subagent roster, MCP connector identity — was about identity: whose machine, whose
plugins, whose agents, whose servers. Nothing in these two files is machine, user, path, plugin,
or MCP identity. The earlier hand-sanitizing pass did substitute cwd/home, and replaced session,
task, tool, message, and device ids with reused placeholders, and dropped `mcp__*` entries from the
tools list. What survives is model-authored prose and a **generic** built-in agent-type name, not
anyone's configuration.

## Why it is still worth a row

`tests/corpus/` now holds two redaction standards side by side, and a reader has no way to tell
which standard covers a given file without already knowing this page or the `subagent/README.md`
paragraph that names the split. Only one of the two standards — the allowlist — is machine-checked.
The other is a one-time, by-hand pass from before this stage's vocabulary existed, and nothing
would fail if it drifted, was copied elsewhere, or was extended with a third hand-sanitized file
under a third ad-hoc standard.

## Two ways to close it, not yet chosen between

1. **Bring the read-backs under the sanitizer.** Give `RunJournal` and persisted-doc snapshots
   their own `sanitize_dir`-shaped pass (or extend the existing one to a second evidence shape) and
   an allowlist of its own, so `allowlist_property.rs` — or a sibling property built the same way —
   can cover them too. The larger cost: these are not provider wire frames, so the walk `sanitize_dir`
   does today (JSON-RPC frame by frame, `.payload` as the root) does not apply as-is; it would need
   its own root shape for a `RunJournal` entry and a persisted `MessagePart` tree.
2. **Delete them if the evidence is no longer needed.** `subagent/README.md` says they exist "so a
   later slice has something to read" — if no slice still depends on reading model prose or the
   tool roster out of these two files specifically, removing them closes the gap by removing the
   file, not by building a second sanitizer.

Whichever is chosen, it is a judgment call about whether the evidence is worth the cost of a second
enforced standard, not a same-task fix — which is why it is filed rather than picked here.
