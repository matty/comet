# D75 — the subagent read-back files carry a different, unenforced redaction standard

### Closed — option 2 below was chosen: the files are deleted

`read-back-doc-snapshot.json` and `read-back-run-journal.jsonl` are gone. Nothing in `crates/`
loaded them as test data — the only references were this page, `allowlist_property.rs`'s doc
comments (explaining why they were outside the total property's remit), and a paragraph in
`subagent/README.md` — so removing them cost no test coverage. `allowlist_property.rs`'s
carve-out comments now say plainly that the exemption is gone: the property's remit is the whole
corpus's frame evidence, with nothing exempted for being sanitized to a different standard.

The reasoning below — why the files existed, what they published, and why the roster field was
not structurally safe even though this one capture happened to be clean — is kept for whoever
next hand-sanitizes evidence outside `sanitize_dir`'s reach and reaches for the same shortcut.

---

**Not a defect in the allowlist or in `sanitize_dir`** — both do exactly what they are scoped to
do. Found during Task 6 (documentation) of the allowlist-sanitizer stage, while sweeping
`tests/corpus/` for anything the new machinery does not actually reach.

## What is in the corpus, and what reached it

`crates/capture/tests/corpus/claude/2.1.229/subagent/` holds two different kinds of evidence, and
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

## Why this is not the leak this stage exists to close today — but the roster is not structurally safe

The stage's own motivating incident — the recording user's OS build, installed plugin names and
versions, subagent roster, MCP connector identity — was about identity: whose machine, whose
plugins, whose agents, whose servers. The earlier hand-sanitizing pass did substitute cwd/home, and
replaced session, task, tool, message, and device ids with reused placeholders. What survives
besides those is model-authored prose, a **generic** built-in agent-type name (`agentType:
"general-purpose"`), and the roster named in the inventory above: `read-back-run-journal.jsonl`'s
`sessionStarted.tools` publishes all ~35 tool names verbatim.

That roster is not a fixed, generic Comet catalog — `tools` on `SessionStarted` is populated from
the same place the raw provider frame's `.tools[]` is (`crates/harness/src/claude/wire.rs:46`,
plumbed through `normalize.rs:756`): whatever the Claude Code process itself reported at `init`,
including any `mcp__<server>__<tool>` entries an MCP connection would add. In *this* capture it is
verified safe on both counts — none of the 35 names carries an `mcp__` prefix, and this run's raw
frames (`events.jsonl` sequence 4) show `"mcp_servers":[]`, so there was nothing to leak. But that
is a fact about this one recording, not a property the read-back format enforces: a future
subagent capture taken from a session with an MCP server connected would carry that server's
identity in this same field, unredacted, the same way the raw wire frame did before Task 2 closed
it there. So the roster is not machine, user, or path identity, but it is not categorically free of
MCP identity either — it is free of it here, by the coincidence of what this particular run had
connected.

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
