# D87 — the stage-7 extraction wait, reconfirmed against ACP

**Status: open by decision. The wait is restated, not the trigger fired.** This page is the
PR10 (`docs/acp-closeout`) re-check the row itself names as one of two things that would reopen
it. Read `docs/debt/README.md`'s D87 row first for the original finding; this page only records
what changed and what was actually checked on 2026-08-29, after the ACP parity slice (Grok,
Hermes) merged.

## What the row asked to be re-checked

D87's own text names two independent triggers, either of which reopens stage 7 of the
capture-module simplification design (extracting `capture/` into a separate `comet-capture`
crate that `comet-harness` only dev-depends on, so production physically cannot reach it):

1. **A third harness arrives that Comet captures but does not yet run** — the crate's other
   stated purpose, previously with no claimant.
2. **Evidence that something on the runtime path reaches into `capture/`** — it happened once
   before (launch types lived there while production used them), fixed by moving the types out
   in stage 2, not by building the boundary.

The ACP parity plan changed the facts underneath both: Comet now runs two more harnesses over
ACP (Grok, Hermes) and captures both of them into the corpus (Grok blocked at the sanitize step
by D102, not at capture; Hermes and the ACP-adapter agents promoted). The brief's framing —
"Comet both captures *and* runs ACP agents, and there are recordings" — is correct as a
statement of fact. What it does not automatically mean is that either trigger, as the row
actually wrote it, has fired. Checked directly rather than assumed:

## Trigger 1 — checked, not fired

Trigger 1 fires for a harness Comet **captures but does not run**. Grok and Hermes are both
registered, running `Harness` implementations (`crates/harness/src/acp/grok.rs`,
`crates/harness/src/acp/hermes.rs`) that also happen to be captured. Neither ever passed through
a "captured but unrun" phase the way the row anticipated for a hypothetical pi/ACP claimant —
ACP support was built to run first, and capture support rides the same production code path
(see Trigger 2 below), not a separate one. So the specific condition the row names — a harness
with no run path, existing only as a capture target — still has no claimant. pi remains the one
concrete case the row had in mind, and pi is explicitly out of scope for this plan
(`HarnessId::Pi` was not added; see the PR10 debt filing for pi below).

## Trigger 2 — checked, not fired

Grepped every production (non-test) reference to `capture::` in `crates/harness/src/`:

```
crates/harness/src/acp/grok.rs:133    (doc comment only, names capture::record::derive_launch)
crates/harness/src/acp/hermes.rs:104  (doc comment only, same)
crates/harness/src/claude/commands.rs:149   use crate::capture::corpus_frame;   -- inside #[cfg(test)] mod tests
crates/harness/src/claude/discovery.rs:309  use crate::capture::corpus_frame;   -- inside #[cfg(test)] mod tests
crates/harness/src/claude/wire.rs:573       use crate::capture::corpus_frame;   -- inside #[cfg(test)] mod tests
crates/harness/src/codex/discovery.rs:398   use crate::capture::corpus_frame;   -- inside #[cfg(test)] mod tests
```

Every non-comment reference is inside a `#[cfg(test)]` block reading `corpus_frame` fixtures for
assertions — the intended direction (tests reach into `capture/` for fixture data), not the
regression the trigger describes (production reaching into `capture/` for behavior). No
production, non-test code path calls into `capture/`. The direction that matters —
`capture/` reaching into production's `pub(crate)` harness internals — is unchanged in kind and
larger in extent (see below), which is D87's own described normal operation, not the failure
mode Trigger 2 is watching for.

## What did change: the coupling grew

`crates/harness/src/lib.rs`'s `pub mod capture;` is still bare, now at **line 165** (the row
cites line 135; the file grew above it during the ACP work — same declaration, stale line
number, corrected here). `capture/` now additionally reaches, all `pub(crate)`:

- `acp::grok::run_launch` (`crates/harness/src/acp/grok.rs:135`) — wired as
  `capture::record::scenarios::acp`'s launch builder for the `session-discovery-grok`,
  `run-grok` and `steer-grok` rows.
- `acp::hermes::run_launch` (`crates/harness/src/acp/hermes.rs:105`) — same role for Hermes'
  scenario rows.
- `acp::{initialize_params, new_session_params, prompt_params}` (`crates/harness/src/acp/mod.rs`)
  — the ACP param builders every `capture::record::scenarios::acp` request body calls, the same
  role Codex's `turn_start_params`/`thread_start_params`/etc. already played in the row's
  original inventory.

So the extraction cost the row weighed against stage 7 — "promote to `pub` every helper
`capture/` reaches, permanently advertising the production crate's internal spawn and param
builders" — is strictly larger now than when the row was written: two more `run_launch`
functions and three more param builders would need widening, for two more providers. This is
evidence *against* extracting now, not for it.

## Decision

**Restate the wait, with the corrected facts.** Neither reopening trigger has fired. The
design's own reasoning still holds — "a scenario builds its wire lines only through production
helpers" is what makes a capture evidence of what Comet actually sends, and ACP's Grok/Hermes
scenarios are a second, larger proof that the coupling works as intended, not a new case for
breaking it. The cost side (an irreversible `pub` API widening) grew; the benefit side (an actual
claimant for the "captures but doesn't run" case) did not appear. Extracting today would spend a
one-way API commitment to solve a problem neither trigger shows exists yet.

The row in `docs/debt/README.md` is updated in the same change that added this page: the line
citation is corrected, the helper inventory now names the ACP additions, and the state column
records that this is a reconfirmation, not a fresh finding.
