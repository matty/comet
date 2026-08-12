# D44 — the provider capture provenance is missing

Harness comments cite dated Claude and Codex captures, and selected literal
replies preserve some provider versions and wire shapes. The underlying capture
set is not in this checkout. A maintainer can verify the excerpt a decoder
accepts, but cannot tie it to the full invocation, neighbouring frames, ordering,
omitted fields, platform or sanitization decisions that produced it.

**Why this is debt.** The fake protocols are evidence-driven, but the evidence
cannot be replayed or re-sanitized when a provider changes. A hand-copied excerpt
also hides whether a field was always present, appeared on only one platform, or
was separated from its request by unrelated notifications.

Fix shape: keep a sanitized, versioned corpus of raw JSONL sessions with a small
manifest recording provider version, OS, invocation, scenario and redactions.
Contract tests should consume literal captured frames rather than round-trip
Comet's own wire types. Dynamic request ids and user content need explicit
placeholders; captured assistant prose should not become a brittle assertion.

The corpus is test evidence, not a promise to replay every session byte for byte.
The native fake executables remain responsible for interactive timing, control
round-trips and process lifecycle.

## Partial result, 2026-08-12

The repository now carries a sanitized, versioned corpus and an exact reciprocal
claim index. Every retained capture-derived claim is backed by a promoted literal
frame, and unsupported Codex approval claims were removed after two bounded live
attempts contradicted the proposed capture contract. The operator procedure is
[`docs/testing/provider-captures.md`](../testing/provider-captures.md).

D44 remains open because the plan's all-twelve-scenarios acceptance criterion is
not met. A fresh explicitly authorized residual slice is recording each scenario
independently under the contradiction policy.

## Residual scenarios

- `codex-steer-reply-before-completion` is backed by a successful sanitized
  `codex-cli 0.147.0` capture. The request, matching successful reply, and terminal
  completion are literal frames in the promoted artifact.
- Ordinary approval stopped after its single permitted attempt when the run emitted
  one failed command execution instead of the reviewed multi-command ordering.
  Its `codex-cli 0.147.0` partial remains ignored and cannot satisfy a claim.
- On-request approval stopped when its approval request arrived before the required
  sandbox-failure completion. Its `codex-cli 0.147.0` partial remains ignored and
  cannot satisfy a claim.
- Interruption remains to be attempted within the current authorized residual slice.
