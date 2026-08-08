# Optional fields arriving from a provider

One rule:

**When a field is optional on the wire, write its absent case yourself.** A plan's
fixtures are written from the shape the provider documents, so they supply the
field every time. The `None` path then ships having never once been constructed —
not in a test, not in a fake CLI, not in review.

This is not the same concern as decode tolerance. A missing field decoding
cleanly to `None` via `#[serde(default)]` is the easy half and is usually already
right. The rule is about what the code *does* with that `None` afterwards.

## The trap: absent read as a value

`None` is the absence of evidence. Code downstream tends to treat it as evidence:

```rust
// WRONG — two notices with no key at all "match", so unrelated
// messages collapse into one and the newer overwrites the older.
if prev.key == next.key { collapse(prev, next) }

// RIGHT — an absent key is not proof that two things are the same thing.
if prev.key.is_some() && prev.key == next.key { collapse(prev, next) }
```

Equality is where this bites hardest, because `None == None` is `true` and reads
as correct at the call site. The same shape appears wherever a `None` silently
picks a branch: dedup keys, cache keys, grouping, "did this change" comparisons,
`unwrap_or_default()` feeding a decision rather than a display.

Ask, at every use of an optional field: **if this is absent, is the answer
"unknown" or is it a value?** If it is "unknown", the code must say so.

## Worked example — notice collapse (0b.1, PR #26)

`AgentEvent::Notice` carries `key: Option<String>`, used to fold a repeated
notice into one chip with an occurrence counter. Eight of the ten emitters
supply a constant key. **Two pass an `Option` straight through from the wire** —
Claude's `informational` (`tool_use_id`) and `notification` (`key`) — and
`sdk.d.ts` documents `tool_use_id` as scoping a message to a single tool use, so
an ordinary informational message carries none.

The result: two unrelated CLI messages merged into one chip, the second
overwriting the first's summary and detail, in a transcript that is **persisted
and replayed to every LAN peer**.

Three per-task reviews and a whole-branch review missed it. Every test and
fixture in the slice supplied a key, so nothing in the slice ever built the
frame that breaks it. The emitters were two files from the fold that compared
them, and neither file was wrong on its own.

## What to do

- **Write the `None` fixture first**, before the happy path — if the field is
  `Option` on the wire, the absent case is not an edge, it is a shape the
  provider will send.
- **Confirm the test is non-vacuous.** Run it against the unfixed code and watch
  it fail. A test that passes either way pins nothing.
- **Read the consumer, not the producer.** The bug lives at the point that
  compares or branches on the value, which is routinely in another crate from
  the emitter that made it optional.
- **Check the provider's own typings for what absent means.** `sdk.d.ts` and
  `schema.gen.ts` say when a field is scoped or conditional; a rendered docs
  page has already been wrong about this repo's providers more than once.
