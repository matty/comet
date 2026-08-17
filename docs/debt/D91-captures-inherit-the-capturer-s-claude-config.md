# D91 — a Claude capture inherits the capturer's own configuration, and nothing records that it did

**Status:** open. The recorder already solves this for Codex and was never given the Claude half.

## What happens

`comet-provider-capture claude <scenario>` spawns the CLI with the operator's ambient
environment. Claude Code reads `~/.claude` regardless of `--cwd`, so every capture carries
whatever skills, plugins, MCP servers, custom commands and hooks that machine happens to have.

Measured on 2026-08-17, `claude fresh-text` against 2.1.233 from a disposable directory on a
developer machine: **62 tools, 37 skills, 68 slash commands, 3 plugins, 2 MCP servers**, plus a
`SessionStart` hook that fired mid-capture and wrote 7.6 KB of its own output into the recording
as two extra frames (`system`/`hook_started`, `system`/`hook_response`).

None of those values reach the committed archive — they are unlisted, so the allowlist redacts
them. **The counts do.** `tools` array length is exactly what
[D86](closed.md) added to the capability sheet so that roster change would be visible.

## Why that is a problem

`docs/providers/claude-2.1.228.md` reads `tools: 29`. The 2.1.233 capture above would render
`tools: 62`. A reader running the version-change report
(`git diff --no-index docs/providers/claude-2.1.228.md docs/providers/claude-2.1.233.md`) sees
**+33 tools** and reads it as the CLI growing. Most of it is one machine's plugin roster.

This lands on the exact signal D86 was closed to create, so the sheet is not merely silent here —
it is confidently wrong in the direction of "the provider changed".

It is the same confound [D85](closed.md) closed on a different axis. D85's was *different
scenarios* compared across versions; this is *the same scenario on a different machine*. Holding
the scenario fixed, which is what a re-record achieves, does not hold this fixed.

## The fix, measured rather than assumed

`CLAUDE_CONFIG_DIR` pointed at an empty directory isolates it. Two `command-discovery` captures
on 2.1.233, token-free, identical but for that variable:

| | commands |
| --- | --- |
| ambient config | 68 |
| `CLAUDE_CONFIG_DIR=<empty>` | 42 |

The 26 that disappear are all the operator's (`superpowers:*`, `grilling`, `find-skills`,
`skill-creator:*`, `handoff`, `ultrareview`, …). **Nothing appears only in the isolated run** — a
strict subset, so 42 is the CLI's own surface, and it matches the built-in count
[D40](README.md) recorded independently. The two hook frames are gone as well.

So the shape is `--claude-config-dir` on `comet-provider-capture` plus a
`needs_empty_claude_config` requirement on the Claude rows in
`crates/harness/src/capture/record/scenarios.rs`, mirroring `--codex-home` /
`needs_empty_codex_home` exactly. That pairing already exists for one provider; this is the
missing half, not a new mechanism.

## Why not `--safe-mode`

Claude Code 2.1.233 has a `--safe-mode` flag that disables the same set (CLAUDE.md, skills,
plugins, hooks, MCP servers, custom commands and agents, output styles, and more). It is the
wrong tool here.

**It changes the argv.** Stage 4 made "a scenario builds its wire lines only through production
helpers" a hard rule, so that a capture is evidence of what Comet actually spawns rather than of
what a test thought to send. A flag production never passes breaks that property for every row it
is applied to. `CLAUDE_CONFIG_DIR` isolates through the environment and leaves argv identical to
production's, which is why the Codex side was built that way too.

## What this says about the corpus that already exists

Nothing isolated `claude/2.1.228` or `claude/2.1.229` either, so their `tools: 29` / `35` / `59`
figures carry an unknown amount of capturer configuration. **The archive cannot answer how much.**
The sheet's Scenarios section prints `configured environment: (none set)` for those rows, which is
true of the environment the recorder *set* and actively misleading about the environment the CLI
*read*.

Two consequences worth separating:

- **The existing sheets are not a clean baseline.** Re-recording 2.1.228's scenarios under
  isolation is what would make the 228 ↔ 233 comparison mean anything, and that is a re-capture of
  an old CLI version, which may no longer be installable.
- **A capture's manifest should say which configuration home it ran against**, so a future reader
  can tell an isolated capture from an ambient one without knowing who ran it. Today the two are
  indistinguishable in the archive.

## Related

- **D86** (closed) — added the `tools: N` line this contaminates.
- **D85** (closed) — the same class of confound on the scenario axis.
- **D70** — no version-coverage policy; this is the second precondition for the version-change
  report actually working, alongside a comparable scenario set.
- **D40** — the non-bare discovery spawn runs the user's `SessionStart` hooks. That is a
  production concern there and shows up here as recorded frames.
- **D50** — PATH order decides which CLI install answers. Same family: the capture records an
  environment nobody declared.

## Reproducing

The A/B above is token-free and takes about a minute:

```powershell
cargo run -p comet-harness --bin comet-provider-capture -- claude command-discovery --cwd <DISPOSABLE> --raw-root .comet-provider-captures\raw\ambient --timeout-seconds 60
$env:CLAUDE_CONFIG_DIR = "<EMPTY_DIR>"
cargo run -p comet-harness --bin comet-provider-capture -- claude command-discovery --cwd <DISPOSABLE> --raw-root .comet-provider-captures\raw\isolated --timeout-seconds 60
$env:CLAUDE_CONFIG_DIR = $null
```

Compare the `commands` array length in each raw `capture.json`. The original run's artifacts are
under `.worktrees/claude-2-1-233-corpus/.comet-provider-captures/` — gitignored, and not worth
preserving given the above.
