# D9 — one sentinel for two different failures

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
