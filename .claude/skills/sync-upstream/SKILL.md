---
name: sync-upstream
description: Pull selected commits from the zeronsh/comet upstream into this fork using scripts/sync-upstream.py, including the conflict and --resume paths.
disable-model-invocation: true
---

Read `.agents/workflows/sync-upstream.md` and follow it exactly. It is the shared,
agent-agnostic procedure; this skill is only the Claude Code entry point to it.

$ARGUMENTS

The helper is interactive — it prompts for a commit selection and a final `y`/`yes`
confirmation. Do not attempt to drive those prompts blindly: run it, show the user the
listed commits, and let them choose. Never cherry-pick from `upstream` by hand as a
workaround; that desynchronizes `.github/upstream-sync.json`.

If a cherry-pick conflicts, resolve it, then `python scripts/sync-upstream.py --resume`.
After the branch exists, run the gate in `.agents/workflows/verify.md` before treating the
sync as done — upstream does not develop on Windows.
