# D73 — the allowlist has seven paths that are really a tool-argument union, not a field each

**Not a defect in the Task 2 allowlist redactor — a residual of choosing a path-based allowlist
at all.** Filed by the Task 2 review (2026-08-15) rather than fixed, because closing it today
breaks a stage-1 corpus assertion and the specific risk below — an *unreviewed* tool landing on
one of these paths — is prospective: every tool the committed corpus exercises through them was
part of what got sampled before the paths were approved.

## The risk, precisely

`.message.content[].input.*` and `.tool_use_result.*` are not one field each — they are whichever
fields the **currently invoked tool** happens to define. This is not hypothetical: verified
directly against the committed corpus (`PSObject`-walked, not grepped, to avoid matching an
unrelated same-named field elsewhere in the payload), these seven paths already carry values from
**five different tools**, not one:

```
.message.content[].input.status     "completed" | "in_progress"          — TaskUpdate
.message.content[].input.taskId     "1" | "2"                            — TaskCreate / TaskUpdate
.message.content[].input.type       "message"                            — SendMessage
.tool_use_result.matches[]          ["TaskCreate"] | ["TaskCreate","TaskUpdate"] | ["TaskUpdate"] — ToolSearch
.tool_use_result.status             "completed"                          — TaskCreate / TaskUpdate
.tool_use_result.type               "text" | "create"                    — Read | TaskCreate
.tool_use_result.updatedFields[]    ["status"] | ["activeForm","status"] — TaskUpdate
```

`observed-fields.json` independently shows the same pattern one level up: `.tool_use_result` is a
union across tools sharing one path prefix (`.filePath`, `.stdout`, `.structuredPatch`, `.query`,
`.prompt` all appear there, one tool's field at a time). Each of the seven lines above was reviewed
against the values these five tools produce. But the path itself does not belong to any one of
them — it belongs to whichever tool is in flight, and nothing stops a **sixth** tool, including a
third party's MCP tool whose argument schema Comet does not control and has never captured, from
landing on the same already-approved path. The five tools above are the reviewed, bounded case;
an MCP tool's arguments are the unbounded one. A future capture that exercises a genuinely new
tool puts that tool's field **at the same path**, and the allowlist, being path-keyed, has no way
to notice the field's *meaning* — and its *reviewer* — changed underneath it.

## Why the surface gate cannot catch this

`crates/harness/tests/capture_corpus/surface_map.rs` (the "new-field gate",
`docs/testing/provider-captures.md`) fails a promotion when the corpus shows a *path* it has
never seen before. That is exactly the wrong granularity for this risk: the seven paths above
already exist in `observed-fields.json`. A new capture that puts an MCP tool's `input.status`
value there is not a new path — it is the *same* path with different meaning, which the gate was
never built to distinguish. The gate's blind spot ("it reports fields that are present; a
capability no capture ever exercised cannot appear in it at all" — `AGENTS.md`, "What the
providers send") is exactly this: the field is present today, under a narrower meaning than the
path actually carries.

## Why the `mcp__` value rule doesn't help either

Task 2's `is_mcp_tool_identity` check (`crates/harness/src/capture/sanitize.rs:668`) runs inside
`sanitize_scalar`, which every allowlisted scalar at every path passes through — it is not scoped
to the tool-name family. The real gap is narrower: `is_mcp_tool_identity` only matches a value that
*starts with the literal `mcp__` prefix*, which is how an MCP tool's own name looks
(`mcp__<server>__<tool>`), not how its **argument content** looks. An MCP tool's `input.status`
value would read `"completed"`, not `"mcp__…"`, so the prefix check has nothing to catch there —
argument content carries no such marker at all, regardless of which scalar it rides on. An MCP
tool's `input.status`, `input.taskId`, or `tool_use_result.matches[]`-shaped field would sail
through unredacted, unrelated to whether the invoking tool's name got caught.

## Why the six-line fix (dropping them) was not taken here

Removing the seven lines from `claude.txt` would default every one of them to redacted, closing
the risk immediately — but two of `crates/harness/tests/capture_corpus/corpus_frames.rs`'s
stage-1 corpus-frame tests read a literal value off one of these seven paths in the **sanitized**
archive: `task_update_splits_status_change_and_active_form_across_two_frames`
(`corpus_frames.rs:118`) asserts `input["taskId"] == "1"`, and
`a_resumed_run_updates_a_task_it_never_created` (`corpus_frames.rs:149`) asserts
`call["message"]["content"][0]["input"]["taskId"] == "2"` — both read
`.message.content[].input.taskId`, one of the seven. Redacting that path would turn each literal
into a placeholder and break both assertions — a real regression in already-promoted,
already-relied-on tests, for a risk (an *unreviewed* tool landing on one of these paths) that has
not happened in any committed capture. That is why this is a deferred decision, not a same-task
fix.

## What has to happen before it can stay deferred any longer

**This must be settled before the next capture is promoted to the corpus.** A new capture is
exactly the event that could exercise a sixth, unreviewed tool through one of these paths, and
nothing today would stop it from being promoted with that tool's argument schema riding an
already-approved line. Two candidate resolutions, not yet chosen between:

1. **Drop the seven lines and accept the stage-1 assertion breaking.** The clean fix — the paths
   go back to default-redacted like everything else nobody has separately reviewed — at the cost
   of rewriting `task_create_puts_the_assigned_id_only_on_the_result` and any sibling assertion
   that reads a literal value off one of these paths in the sanitized archive.
2. **Gate them on the sibling `.name`/`.message.content[].name` being a built-in tool.** Keeps
   the stage-1 assertions working, but turns the allowlist from purely path-based into
   path-plus-sibling-value for this one family — the same shape of exception `is_mcp_tool_identity`
   already is, just checking the opposite condition (allow only when the tool name is *not*
   `mcp__`-prefixed, rather than redact only when it is).

Whichever resolution is chosen, it is a decision to make with a real capture in hand, at the point
the next capture is promoted, not a guess made now against a corpus that has never exercised the
case.
