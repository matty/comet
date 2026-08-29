# D87 — the stage-7 extraction trigger has fired

**Status: open, REOPENED 2026-08-29.** This page is the PR10 (`docs/acp-closeout`) re-check the
row itself names as one of two things that would reopen it. **A first pass on 2026-08-29 got
this wrong** — it concluded neither trigger had fired and restated the wait. A review of that
pass caught it: trigger 1 had already fired, on evidence sitting in this same tree. This page
records the corrected finding. Read `docs/debt/README.md`'s D87 row first.

## What the row asked to be re-checked

D87 names two independent triggers, either of which reopens stage 7 of the capture-module
simplification design (extracting `capture/` into a separate `comet-capture` crate that
`comet-harness` only dev-depends on, so production physically cannot reach it):

1. **A third harness arrives that Comet captures but does not yet run** — the crate's other
   stated purpose.
2. **Evidence that something on the runtime path reaches into `capture/`.**

## Trigger 1 — FIRED. `codex-acp` and `claude-agent-acp` are the claimant

The first pass reasoned about Grok and Hermes only, found both are run *and* captured, and
concluded "no claimant." That reasoning never checked PR2's own promoted corpus, which already
holds the claimant:

- **`docs/testing/supported-provider-versions.md:12-22`**: "codex-acp 1.7.0 and claude-agent-acp
  0.70.0 are both promoted... and Comet registers no `Harness` for either:
  `comet_proto::agent::HarnessId` has no `CodexAcp`/`ClaudeAgentAcp` variant."
- **`crates/proto/src/agent.rs:9-20`**: `HarnessId` lists `ClaudeCode, Codex, Cursor, Grok,
  Hermes, Mock` — confirmed directly, no `CodexAcp` or `ClaudeAgentAcp` variant exists.
- **`crates/harness/src/capture/record/scenarios/acp.rs:221-227`**, the recorder's own comment on
  why `run_launch` hard-delegates to Grok: "codex-acp and claude-agent-acp never reach this
  function... neither has a production `Harness` to register one against."
- **`crates/harness/src/capture/record/scenarios/acp.rs:32-91` (`adapter_launch`, `adapter_entry`,
  `npm_global_root`), `:151` (`codex_acp_launch`), `:158` (`claude_acp_launch`)**:
  `codex_acp_launch`, `claude_acp_launch` — the launch these two rows actually use — are built
  **entirely inside** `capture/`. They call no `crate::acp::{grok,hermes}` production code at
  all, because there is none to call.
- **Corpus and sheets exist and are real**: `crates/harness/tests/corpus/{codex-acp,claude-agent-acp}/`
  and `docs/providers/{codex-acp-1.7.0,claude-agent-acp-0.70.0}.md` (per D102's own notes on the
  promotion).

This is the row's "captures but does not yet run" case, exactly, and not a stretch of it: a real
harness-shaped citizen — its own corpus directory, its own capability sheet, its own scenario
rows and launch code — with no `Harness` trait implementation and no plan to ever get one, per
`supported-provider-versions.md`'s own framing ("exist in the corpus purely as the ACP
protocol's own two-speaker comparison points").

**The design that introduced ACP capture anticipated exactly this and said so at the time.**
`specs/2026-08-27-acp-harness-design.md` section 5 ("Capture first — and D87 reopens"), written
before PR2 added `codex-acp`/`claude-agent-acp` to the corpus: "This slice **is** that trigger,
and it names pi explicitly... D87's stage-7 question must be answered, not silently
re-deferred... A third harness changes the inputs; the slice either restates the wait with the
new facts or reopens it. Either is fine. Leaving the row untouched is not." That design named pi
as the example claimant; PR2 supplied two more, concretely, in the corpus itself.

**pi remains not a claimant** — `HarnessId::Pi` still does not exist and pi is out of scope for
this plan (D106) — but pi was never the only candidate the trigger's wording covers, and the
first pass's error was treating it as the only one worth checking.

## Trigger 2 — corrected, not overturned: nothing on the runtime path reaches in

The first pass's grep was not exhaustive and its conclusion sentence overstated what the (partial)
grep showed. The complete picture, grepping every `capture::`/`capture:` reference under
`crates/harness/src/` outside `capture/` itself:

```
crates/harness/src/claude/commands.rs:149    use crate::capture::corpus_frame;   -- #[cfg(test)]
crates/harness/src/claude/discovery.rs:309   use crate::capture::corpus_frame;   -- #[cfg(test)]
crates/harness/src/claude/wire.rs:573        use crate::capture::corpus_frame;   -- #[cfg(test)]
crates/harness/src/codex/discovery.rs:398    use crate::capture::corpus_frame;   -- #[cfg(test)]
crates/harness/src/bin/comet-provider-capture.rs:4   use comet_harness::capture::...   -- NOT test
crates/harness/src/bin/comet-provider-sanitize.rs:3  use comet_harness::capture::...   -- NOT test
```

The four `claude`/`codex` hits are `#[cfg(test)]` fixture reads — the intended direction. The two
`bin/` hits are genuinely non-test code that reaches into `capture/` — but neither binary ships
on `comet.exe`'s runtime path: they are separate `[[bin]]` targets (`comet-provider-capture`,
`comet-provider-sanitize`), operator tooling for taking and sanitizing captures, never linked
into or invoked by the desktop app. Trigger 2's own wording is "the runtime path," not "any
non-test code," and that distinction is exactly what keeps it from firing here — the row's own
text already gets this right (it says "nothing on the runtime path calls them"); this page's
first pass did not, and claimed a stronger, false thing ("no production, non-test code path
calls into `capture/`" — the two `bin/` targets are a counterexample to that literal sentence,
even though they do not reach `comet.exe`).

## The cost side, corrected

`crates/harness/src/lib.rs`'s `pub mod capture;` is still bare, at line 165 (the original row
cited 135; the file grew above it during the ACP work — same declaration, corrected line
number). The pre-existing Claude/Codex `pub(crate)` list from the 2026-08-16 premise check is
unchanged and remains the dominant cost of extraction: roughly fifteen items across
`claude::{run_launch, wire, load_image_blocks, discovery, commands}`,
`codex::{run_launch, normalize_run_request, turn_start_params, thread_start_params,
thread_resume_params, turn_steer_params, turn_interrupt_params, discovery}`, `launch`,
`home_dir`, `shell_env::system_path`.

**What the ACP slice actually added to that list is smaller than a first read suggests.**
Checked directly against `crates/harness/src/acp/mod.rs`:

- `initialize_params` (`:56`) — already `pub`.
- `new_session_params` (`:74`) — already `pub`.
- `prompt_params` (`:87`) — `pub(crate)`. Would need widening.

And of the two agent-specific `run_launch` builders:

- `acp::grok::run_launch` (`grok.rs:135`, `pub(crate)`) — actually called from
  `capture/record/scenarios/acp.rs:246`. Would need widening.
- `acp::hermes::run_launch` (`hermes.rs:105`, `pub(crate)`) — named only in a doc comment
  (`scenarios/acp.rs:233`, "no capture scenario row yet"). Not currently reached by any capture
  code, so it does not belong in this row's cost count until a Hermes scenario row is built.
  (Hermes has no capture at all today — see D110, and the correction below.)

**Net ACP-driven growth: two items** (`grok::run_launch`, `prompt_params`), not the five the
first pass counted. A materially cheaper marginal addition to an already-large pre-existing list.

## One more error in the first pass's page, corrected here

The first pass's page said Hermes was "promoted" into the corpus alongside the two ACP-adapter
agents. **False.** Hermes has no corpus directory (`crates/harness/tests/corpus/hermes/` does
not exist), no scenario row in `SCENARIOS`, and no capability sheet — only two raw, unsanitized
`.jsonl` captures outside the repository (`C:\dev\superpowers\comet\captures\...`). D110 states
this correctly; this page's earlier draft did not, and is corrected here rather than left
standing.

## Decision: reopen stage 7

Trigger 1 fired. `codex-acp` and `claude-agent-acp` are a real claimant for capture's "captures
but does not yet run" purpose, not a hypothetical one, and their entire launch/scenario/corpus
machinery lives inside `capture/` with zero benefit to production — unlike Grok's and Hermes'
capture code, which at least documents a harness Comet genuinely ships and runs, theirs can
never earn its place in `comet.exe` by that argument, because no `Harness` will ever exist for
them. That is precisely the accumulation stage 7 exists to stop before it becomes permanent by
default.

Weighed against the corrected, smaller marginal cost (two items now, against the already-large
pre-existing list — the marginal ACP cost was never the dominant term either way), the balance
favors extraction. The row is marked **reopened**, not merely re-affirmed waiting.

**What "reopened" means here, concretely, and what it does not mean**: this PR does not perform
the `comet-capture` crate split — that is a Rust change, and this branch is docs-only by its own
scope (the task brief is explicit: "no Rust should need to change"). Reopening records the
corrected decision so the next slice picks up stage 7 as active work rather than reading D87 as
settled-by-decision-to-wait, which is what a first, wrong pass at this page would have shipped.

## Stage 7 executed, 2026-08-29

`crates/harness/src/capture/` moved to a new workspace member, `crates/capture` (package
`comet-capture`), `git mv`'d wholesale — sources, `allowlist/*.txt`, the two `comet-provider-*`
bins, `tests/capture_corpus.rs` + `tests/capture_corpus/`, and `tests/corpus/`. `comet-harness`
depends on `comet-capture` only as a **dev-dependency**; `comet-capture` depends on
`comet-harness` normally, to build launches from its production types. Cargo's dev-dependency
cycle allowance is what makes this direction sound, and it does build — `cargo test -p
comet-harness --no-run` links `comet-capture` for the crate's own `#[cfg(test)]` unit tests
(five sites: `claude::{commands,discovery,wire}`, `codex::discovery`, and
`claude::normalize` — this page's trigger-2 grep at the top of this file missed the fifth,
`claude/normalize.rs:2269`, which is why the count here differs from the four cited above).

**One thing that grep did not anticipate: `[[bin]]` targets never get dev-dependencies, in any
Cargo build mode.** `tests/fixtures/fake_claude.rs` compiles as a `[[bin]]` (spawned via
`CARGO_BIN_EXE_fake-claude` by harness's own integration tests), and it read one corpus frame
through `comet_harness::capture::corpus_frame`. Pointing that call at `comet_capture::corpus_frame`
after the move does not compile — `cargo test -p comet-harness --no-run` failed with
`E0433: cannot find module or crate comet_capture in this scope` — because `comet-harness`'s
dev-dependency on `comet-capture` is invisible to a `[[bin]]` target regardless of the invoking
command (`cargo check --all-targets`, `cargo test --no-run`, `cargo build --profile test`, all
fail the same way). `corpus::frame`'s own doc comment ("Kept separate from `frame` so the reader
can move to its own crate later while this path stays anchored to `comet-harness`") anticipated
exactly this: `fake_claude.rs` now reads its one frame with a small local reimplementation of
the same manifest/`events.jsonl` convention, rather than depending on the crate.

Visibility promoted from `pub(crate)` to `pub` on `comet-harness`, the full list actually needed
to compile (checked directly against the source, not re-derived from the pre-move estimate
above): `launch::LaunchDescriptor` (+ its `command()` method), `home_dir`, `resolve_cli`,
`all_known_dirs`, `KnownDir`; `acp::prompt_params`, `acp::grok::run_launch`;
`claude::{run_launch, load_image_blocks}`, the `claude::{commands, discovery, wire}` module
declarations, `claude::commands::command_discovery_launch`,
`claude::discovery::model_discovery_launch`, and five `claude::wire` items
(`ImageBlock`, `user_message_line_with_images`, `control_response_line`, `allow_response`,
`deny_response`); `codex::{run_launch, normalize_run_request, thread_start_params,
thread_resume_params, turn_start_params, turn_steer_params, turn_interrupt_params}`, the
`codex::{approval, discovery}` module declarations, `codex::discovery::{codex_home,
discovery_launch}`, and `codex::approval::decision_literal`. `acp::hermes::run_launch` did
**not** need promotion — capture names it only in a doc comment, never calls it (matching this
page's D110 note above).

Full workspace gate after the move: `cargo fmt --all`, `cargo clippy --workspace --all-targets`
(zero warnings attributed to `crates/harness` or `crates/capture`, same 26-location baseline
elsewhere), `cargo nextest run --workspace` (2027 run + 17 skipped, matching `origin/main`'s
total exactly — confirmed against a disposable comparison worktree — so the move dropped no
test). `cargo tree -p comet -e normal` lists `comet-harness` but never `comet-capture`, and
`crates/harness/src/lib.rs` no longer declares `pub mod capture;` — the negative this stage
exists to prove. The capability-sheet golden test was exercised in both directions from its new
home: it failed first (the sheets' committed text still named `crates/harness/src/capture/...`
after `surface.rs`'s own doc-comment path moved to `crates/capture/src/surface.rs`), and
`$env:COMET_UPDATE_SHEETS = "1"; cargo test -p comet-capture --test capture_corpus` regenerated
all seven sheets and restored green.
