# Two sessions, not one

`manifest.json` and `events.jsonl` are the corpus pair — raw provider wire frames from a
`drive_claude_plan.py` capture, driven directly against `claude.exe` (see `manifest.json`'s
`command` block). No test asserts against these frames — the corpus's capability sheet
(`docs/providers/claude-2.1.229.md`) is what enumerates them, which records what fields and
value vocabulary are present and nothing about the actual content behind them. They are
archived evidence: richer than a
single-agent capture and kept so a later slice has something to read (a resumed `task_started`
sharing one `task_id`, `task_progress`, patch-only `task_updated`, `task_notification` with
usage, and `background_tasks_changed` populated-then-empty).

This directory used to hold two more files — `read-back-run-journal.jsonl` and
`read-back-doc-snapshot.json` — evidence from a **separate session**: the live run driven
through Comet's actual `comet headless` engine (real `RunJournal` + real persisted doc, `haiku`
model, same CLI version) that slice 4.2 task 9's report asserted its two read-backs against.
They were hand-sanitized to their own standard before the allowlist existed, sat outside
`sanitize_dir`'s remit, and — unlike `events.jsonl` above — still published full assistant and
reasoning prose, a roughly thirty-five-entry tool roster, and `agentType: "general-purpose"`
verbatim. `docs/debt/D75-read-back-files-outside-the-allowlist.md` deleted both rather than
building a second enforced sanitizer for evidence nothing still reads; see that page for what
they held and why deletion was the closure chosen over sanitizing them.
