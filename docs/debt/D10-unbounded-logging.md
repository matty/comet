# D10 — the bound stops at the registry

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
