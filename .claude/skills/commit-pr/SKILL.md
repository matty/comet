---
name: commit-pr
description: Branch, commit in this repo's message style, push, and open a squash-merge PR with gh.
disable-model-invocation: true
---

Read `.agents/workflows/commit-pr.md` and follow it exactly. It is the shared,
agent-agnostic procedure; this skill is only the Claude Code entry point to it.

$ARGUMENTS

Key points that are easy to get wrong here: never commit to `main`; run the gate in
`.agents/workflows/verify.md` before committing; do not append `(#N)` to the subject (the
squash merge adds it); the PR title must follow the same conventional-commit style as the
subject because it becomes the squashed commit. Confirm with the user before merging.
