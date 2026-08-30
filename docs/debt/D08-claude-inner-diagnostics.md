# D8 — Claude's inner catch-alls reach none of the three tiers

**CLOSED 2026-08-30, with the fix shape this page ends on.** `INNER_IGNORED` is the small named
list (structural `content_block_start`/`stop`, the message framing, `ping`, the opaque
`signature_delta`, and the buffered `text`/`thinking` blocks whose streamed halves are already
claimed), and everything else produces a `delta/<kind>`, `stream_event/<kind>` or `block/<kind>`
diagnostic through `Normalizer::inner_diagnostic`.

**Once per RUN, not once per frame**, which is the one thing this page did not say and the
implementation has to: these arrive inside streamed deltas, so an unrecognized delta kind on a
long tool input would otherwise journal and broadcast one event per chunk. The set is capped at
32 distinct kinds for the reason `acp::normalize`'s `MAX_TRACKED_UPDATE_KINDS` gives — the key
is provider-chosen.

The original note follows.


0b.2 gave Codex an **inner** sink: `map_item`'s `other =>` arm turns an
unrecognized item type inside the *claimed* `item/started`/`item/completed`
notifications into an `item/<type>` diagnostic. Claude has no equivalent.
`claude/normalize.rs`'s `stream_event` arm drops an unrecognized `delta.kind`,
and its assistant-content handling keeps only `tool_use` and drops every other
block kind — both inside frames the classifier calls fully Claimed, so neither
produces a diagnostic, a warn, or anything at all.

This matters because delta kinds are exactly where Anthropic has shipped change:
`thinking_delta`, `signature_delta`, `input_json_delta`, `citations_delta` all
arrived over time. A new one is invisible — which is the precise failure 0b.2
exists to remove, surviving inside the slice that removed it.

**It is a plan gap, not an implementation defect.** The plan enumerated five
sinks and gave Claude only the outer three. Worth noting the branch's own
comments now read as though coverage is complete, which is why this entry
exists rather than silence.

Fix shape: the same treatment Codex got — a small named Ignored list (the
structural `content_block_start`/`stop` and `signature_delta` belong on it) plus
a `stream_event/<deltaKind>` and `block/<kind>` diagnostic for the rest.
