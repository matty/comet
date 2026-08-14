# Two sessions, not one

`manifest.json` and `events.jsonl` are the corpus pair — raw provider wire frames from a
`drive_claude_plan.py` capture, driven directly against `claude.exe` (see `manifest.json`'s
`command` block). No test reads these frames. They are archived evidence: richer than a
single-agent capture and kept so a later slice has something to read (a resumed `task_started`
sharing one `task_id`, `task_progress`, patch-only `task_updated`, `task_notification` with
usage, and `background_tasks_changed` populated-then-empty).

`read-back-run-journal.jsonl` and `read-back-doc-snapshot.json` are **evidence from a separate
session** — the live run driven through Comet's actual `comet headless` engine (real
`RunJournal` + real persisted doc, `haiku` model, same CLI version) that slice 4.2 task 9's
report asserts its two read-backs against: a `SubagentStarted` and a terminal `SubagentUpdated`
sharing one `task_id` with non-`None` `totalTokens`, and a persisted document holding exactly one
terminal `Subagent` part with no `prompt` field. That run used a different `task_id` and
different token totals than the capture above — the two are not the same session, and are not
meant to be read as one. Both are sanitized to the same standard (cwd/home substituted,
session/task/tool/message/device ids replaced with reused placeholders, `mcp__*` tool entries
dropped from the tools list); neither follows the corpus manifest schema in
`docs/testing/provider-captures.md`, since they are not provider-wire evidence.
