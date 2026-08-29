# D91 — a Claude capture inherits the capturer's own configuration, and nothing records that it did

**Status:** partly closed. The mechanism landed — `--claude-config-dir` sets `CLAUDE_CONFIG_DIR`
for every Claude launch, the manifest records it, and `model-discovery` refuses to run without an
empty one — and `claude/2.1.241/{model-discovery,command-discovery}` are the first isolated
captures in the corpus. What stays open is the archive that predates it: `2.1.228`, `2.1.229` and
`2.1.233` are ambient and cannot be re-recorded, because the installed CLI has moved to 2.1.241.
See "What landed" below for the part that is not merely deferred but *unresolvable* by this flag —
the run scenarios.

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
`crates/capture/src/record/scenarios.rs`, mirroring `--codex-home` /
`needs_empty_codex_home` exactly. That pairing already exists for one provider; this is the
missing half, not a new mechanism.

## What landed, and the measurement that shaped it

The original sketch above — the flag plus `needs_empty_claude_config` on *the Claude rows* — is
half right. Re-measuring on **2.1.241** from a disposable cwd, token-free, three configuration
states rather than two:

| scenario | config home | events | commands | models | `account` |
| --- | --- | --- | --- | --- | --- |
| `model-discovery` (`--bare`) | ambient | 2 | 43 | **5** | `tokenSource: none` |
| `model-discovery` | empty | 2 | 42 | 4 | `tokenSource: none` |
| `model-discovery` | credentials only | 2 | 42 | 4 | `tokenSource: none` |
| `command-discovery` | ambient | **4** | **68** | **5** | real account |
| `command-discovery` | empty | 2 | 42 | 4 | `tokenSource: none` |
| `command-discovery` | credentials only | 2 | **46** | 4 | real account |

Three things follow, and only the first was anticipated.

**`--bare` does not isolate.** The production doc comment on `DISCOVERY_ARGS` is right that
`--bare` skips hooks, plugin sync and CLAUDE.md, and that was read as making model discovery
immune. It is not: ambient bare discovery answers **five** models where an isolated one answers
four. The extra is `claude-fable-5[1m]`, configured on the capturer's machine — and
`crates/capture/tests/corpus/claude/2.1.233/model-discovery/events.jsonl` publishes it, with
`default` resolving to `claude-opus-5[1m]`. The corpus has been asserting a model list that no
clean install produces, on the exact reply the model picker decodes.

**An empty home is not a neutral choice for every row.** It also logs the CLI out. For
`model-discovery` that costs nothing, because `--bare` never reads credentials — the empty and
credentials-only rows above are identical. For `command-discovery` it is the difference between
Claude's command surface (46) and its logged-out one (42, `tokenSource: "none"`), which is a
different observation wearing the same scenario name. So the requirement is set on
`model-discovery` alone, and the others take a home seeded with `.credentials.json` —
a home the recorder cannot validate, and therefore cannot enforce. Codex's precedent is the same
shape: `needs_empty_codex_home` belongs to the deliberately logged-out row, not to every row.

**A count moves without either the CLI or the config moving.** Two `command-discovery` captures
minutes apart, same version and same seeded home, answered 46 and 48; the extra `extra-usage` and
`usage-credits` are account state held on the server. This is the sharper form of the caveat
[D86](closed.md) already carries: even holding version, machine and config fixed, a `commands` or
`tools` count is a floor, never a number to diff.

## What is still open

- **The run scenarios have a mechanism but no evidence.** `--claude-config-dir` reaches every
  Claude launch, run rows included, but no run capture has been recorded through it — so
  `tools: 62` in the 2.1.233 sheet stands uncorrected, and the credentials-seeded procedure is
  proven for discovery only. This needs a token-spending capture session.
- **The pre-flag corpus cannot be repaired.** `2.1.228`, `2.1.229` and `2.1.233` were all recorded
  ambient, and their CLI versions are no longer installed. Their sheets print
  `env: (none set)`, which is true of what the recorder set and misleading about what the CLI
  read; the 2.1.241 sheet's `env: CLAUDE_CONFIG_DIR=<CLAUDE_CONFIG_DIR>` is what distinguishes
  them. Compare an isolated sheet against an ambient one and the contamination is a third axis on
  top of the two [D85](closed.md) and D86 already name.
- **Nothing forces isolation on the rows that cannot require it.** A capturer who omits
  `--claude-config-dir` for `command-discovery` or a run scenario gets an ambient capture, and only
  the manifest's missing variable records that they did.

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
cargo run -p comet-harness --bin comet-provider-capture -- claude command-discovery --claude-config-dir <EMPTY_DIR> --cwd <DISPOSABLE> --raw-root .comet-provider-captures\raw\isolated --timeout-seconds 60
```

Compare the `commands` array length in each raw `capture.json`. Add a third run against a home
holding only a copy of `.credentials.json` to separate "isolated" from "logged out" — without it
the two collapse, which is the mistake the first draft of this page made.

The runs behind the table above are gitignored under `.comet-provider-captures/` and not worth
preserving; the promoted `claude/2.1.241` pair is the durable evidence.
