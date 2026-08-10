# D5 — a hung tool call has no floor

### Closed by slice 1.5 — and "closed" means something narrower than it sounds

**Read this before assuming the hang is gone.** The wait is now **named and actionable**. It is
**not bounded**. A tool call can still run forever, and 1.5 deliberately did not add a timeout: a
long build, a long test run and a large search are all legitimately long, and auto-killing one
would convert a slow success into a fabricated failure. What can no longer happen is a turn
sitting there with **nothing on screen saying what it is waiting on or how to get out**.

What ships (`crates/ui/src/approvals.rs`, `crates/ui/src/shell.rs`): after
`HUNG_TOOL_AFTER_SECS = 60` with no result, the status strip replaces the ordinary working
indicator with the call named by its own chip label and detail — `Still waiting on Run · pwsh
-NoProfile -Command "…"` — its own elapsed, and a Stop. The clock is keyed on the tool id, so a
second call starts its own wait, and it is cleared when nothing is blocked.

**Two limits, both deliberate and both stated rather than discovered:**

- **Stop is turn-level.** Comet has no per-call cancellation and neither provider exposes one, so
  the line names the *specific* call and Stop ends the *turn*. The spec's "a cancel tied to that
  specific call" is answered, not implemented literally.
- **The elapsed clock is UI-local.** No part carries a start timestamp, so a client that restarts
  mid-wait **under-reports**. That is the safe direction: the line only appears once a wait is
  long, and it can never claim a wait was longer than it was.

**D5 was never reproduced on `main`** — the repo's own rule, and the thing this entry always
lacked. The original 12-minute observation was against a live Codex session and three controlled
re-runs failed to trigger it. That gap is now filled from the other side: **`COMET_MOCK_HANG=1`
is the reproducer**, and the first one that exists, because every fake in this repo answers. It
emits a tool call and then holds the stream open forever with no result and no `Done`.

Verified on screen 2026-08-09 (`sdd/2026-08-09-slice-1-5-approval-ui/shots/12*`, `13*`): ordinary
indicator at strip-elapsed 57s, the named line with Stop at 1m 2s — so the 60s crossing is
confirmed from both sides — and Stop at 1m 21s cleared the strip, collapsed the tool group and
returned the composer's send button.

**Latent, named but not observed:** `shell.rs`'s `Indicator::None` arm returns a bare strip, and
`effective_indicator` degrades a `Working` session to `None` after `SESSION_STALE_MS` = 45s — 15s
before the hung line is due to appear. Both hang runs kept counting past 45s, so the engine does
heartbeat a hung session; nothing tests the interaction.

The original entry follows, unchanged, because the forensics are still the only record of the
live failure.

---


**Observed live on 2026-08-09**, during 0b.2's real-CLI check, on a branch build
of `feature/frame-diagnostics`. Not reproduced from a test — this is the failure
the fakes cannot produce, because the fakes always answer.

**What happened.** A Codex session ran an ordinary read-only prompt. Journal
`34052aca` records eight events and then stops:

```
sessionStarted (codex, gpt-5.3-codex-spark)
notice ×4        (mcpStatus: codex_apps ready, node_repl ready)
textDelta        ("I'll quickly check the AGENTS instructions and then list…")
assistantMessageCompleted
toolCall         exec: pwsh.exe -Command "Get-Content …\.agents\AGENTS.md"
```

No `toolResult`. No `done`. Twelve minutes later `codex.exe` was still burning
~83% of a core, with two children: a `node_repl.exe` MCP server and the
long-lived `pwsh.exe` that is codex's *own* command-safety AST parser (by design
— its embedded script says it stays alive to amortize PowerShell startup). The
Claude session started in the same run completed normally, so this is not a
machine-wide condition.

**Whose bug the hang is: probably not Comet's.** Codex had not produced a result
and was the process spinning, which points at the CLI's Windows command-safety
layer.

**Narrowed by three controlled runs on the branch build (2026-08-09), none of
which reproduced it:**

| cwd | sandbox | result |
| --- | --- | --- |
| small scratch dir (2 files) | `danger-full-access` | completed, fast |
| small scratch dir (2 files) | `workspace-write` | completed, 35s |
| `C:\Users\coding` (home) | `workspace-write` | **wedged 12+ min** (the observed failure) |

So the sandbox mode is **not** the trigger — `workspace-write` completes fine on
a small tree. The remaining variable is the **cwd**: the wedged session had the
user's entire home directory as its workspace, and died on a tool call reading
`C:\Users\coding\.agents\AGENTS.md`. A sandbox preparing a writable scope over a
whole home directory is the obvious suspect, unconfirmed. Reproducing it costs
~12 minutes and a pinned core, so it was not attempted again.

Note this also means the 0b.2 zero-diagnostic check passed under *both* Codex
sandbox modes — the hang is orthogonal to the classification work.

The earlier `CreateProcessAsUserW failed: -1073283067` seen during the 0b.2
capture is a *different* symptom (fails fast, surfaces as an item-level error)
and should not be conflated with this one.

**Why it is a Comet debt anyway, and the actual point.** `.agents/rules/user-facing-errors.md`
rule 2 says no waiting state may last forever — every skeleton needs a reply, a
timeout, or a bounded retry that gives up into something actionable. A tool call
has none. The session sits Working indefinitely with no elapsed indicator, no
cancel affordance tied to the stuck call, and nothing that eventually converts
the silence into a named state the user can act on. Whether the CLI or the
sandbox is at fault does not change that: **the provider is untrusted for
liveness, and Comet is the thing the user is looking at.**

Note the shape is the mirror image of what 0b.2 just built. 0b.2 makes a frame
Comet *received but did not understand* visible. This is a frame that never
arrived at all — the same blindness, from the other side, and the diagnostics
registry cannot see it because nothing was dropped.

**Candidate answers, none decided.** A per-tool-call elapsed threshold that
surfaces "this has been running N minutes" with a cancel; reusing the
slow-request toast machinery already in `crates/ui/src/toast.rs`, which exists
for exactly this and currently covers loads but not turns; or a harness-level
watchdog that turns a silent exec into a visible `Error` after a bound. The
first is the smallest honest fix. Whichever is chosen, support for a sandbox
that cannot spawn its shell is a separate question from not hanging.

**Before acting on this, reproduce it on `main`** — the repo's own rule, and the
one thing this entry does not have.
