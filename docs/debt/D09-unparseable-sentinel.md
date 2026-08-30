# D9 — one sentinel for two different failures

**CLOSED 2026-08-30, on both sides, exactly as this page proposed.**

Claude: `parse_frame` returns `FrameParseError { kind, error }`. `kind` is `None` for a line
that was never JSON — which keeps the bare `unparseable` — and `Some(type)` when the line
parsed and only the typed decode failed, giving `unparseable/system`, `unparseable/result` and
so on. The type is free at the failure site, as this page says: the generic parse already
succeeded.

Codex and ACP: `Incoming::Malformed(MalformedKind)` carries the distinction the reader always
had. `NotJson` keeps the bare sentinel; `NotAnObject` and `NotAMessage` name themselves. Both
harnesses read it through one `discriminator()`, so the vocabulary is fixed strings and cannot
grow with provider text.

**One thing this page assumed that turned out not to hold, worth knowing before extending it:**
an unrecognized `type` never reaches Claude's parse-failure path at all. It falls to
`classify_unclaimed` and becomes `Frame::Unknown`, so the only kinds that can appear after
`unparseable/` are the handful `parse_frame` names. The discriminator is still run through
`sanitize_discriminator` as defence, and `an_unrecognized_frame_type_becomes_unknown_rather_than_a_parse_failure`
is the record that nothing today can exercise it.

The original note follows.


Claude's `parse_frame` returns a single `Err` for both "this line is not JSON"
and "this line is valid JSON with a known `type`, but the typed decode failed".
`#[serde(default)]` covers an absent field, not a field whose *type* changed —
so a `result` frame whose `errors` becomes an object lands as a bare
`unparseable` row. An operator reads `unparseable ×412` and learns nothing about
which frame; worse, for a `result` frame that same failure means no `Done` ever
fires, which is the original missing-output symptom the slice was built to
explain.

The information is free: the line already parsed as JSON, so `type` is in hand.
Emit `unparseable/result` when the generic parse succeeded and only the typed
decode failed; keep the bare sentinel for genuinely unparseable input. Codex
already distinguishes its two cases at the source (non-JSON vs. neither-method-
nor-id) and then discards the distinction into the same sentinel — same fix.
