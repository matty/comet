---
name: verify
description: Run Comet's full verification gate (cargo fmt, clippy, workspace tests, plus the edge and Python suites when relevant) and report the real output. Use before claiming a change works, builds, or passes, and before opening a PR.
---

Read `.agents/workflows/verify.md` and follow it exactly. It is the shared, agent-agnostic
definition of this project's gate; this skill is only the Claude Code entry point to it.

Determine which optional sections apply from `git status --short` / `git diff --name-only`
against the merge base with `main`:

- any `*.rs`, `Cargo.toml`, or `Cargo.lock` change → the Rust gate (always run it anyway)
- any `edge/**` change → the edge typecheck and tests
- any `scripts/**/*.py` change → the Python unittest suite

Run the commands yourself, read the output, and report what actually happened — including
failures, quoted rather than paraphrased. If you skip a step, say which and why.
