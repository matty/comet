# D10 — the bound stops at the registry

**CLOSED 2026-08-30, by the cheap mitigation this page names.** `log_budget`
(`crates/harness/src/lib.rs`) gives each discriminator the first five occurrences at full
fidelity — which is what makes a new frame diagnosable — and logs only a rising count after
that. The payload is the half that is both unbounded and sensitive, so it is the half that
stops. Every drop site goes through it: Claude's `classify_unclaimed` and its unparseable-line
arm, and all three JSON-RPC reader arms, which Codex and both ACP agents share.

The key table is capped at 64, matching the registry's own cap on purpose — the two answer the
same question about the same stream, and a budget tracking more keys than the registry would be
bounding the cheap half. A discriminator past the cap is still logged, just never with its
payload: losing the payload is what the cap is for, while losing the FACT would hide the drift
the diagnostic exists to raise.

**What this does not bound**, stated so nobody reads the close as wider than it is: the journal
append and the broadcast per diagnostic are untouched. The registry still caps distinct
discriminators, so neither grows without limit in the way the log did, but neither is sampled
either.

The original note follows.


The registry caps at 64 distinct discriminators per harness with saturating
counts, so *memory* is bounded. Nothing else on the path is. Every drop site
warn-logs the full frame or raw line, and every diagnostic is journaled and
broadcast.

Take the scenario the feature exists for: a future codex renames
`item/commandExecution/outputDelta`. It moves from Ignored (free) to Unknown, and
every output chunk becomes a warn-level log line **carrying raw command stdout**,
plus a journal append, plus a broadcast — indefinitely. The registry row
saturating at one entry does nothing to slow the producers. The count stays
correct; the log and the journal do not stay bounded.

Cheap mitigation: log at full fidelity for the first N occurrences of a
discriminator and sample thereafter. The registry already knows the count and is
the natural gate. At minimum this should be a recorded, accepted risk rather than
an unexamined one.
