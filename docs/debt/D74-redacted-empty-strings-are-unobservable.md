# D74 — a redacted field's empty case can never again be demonstrated from corpus evidence

**Not a defect in the allowlist redactor — a structural consequence of redacting a whole path by
default.** Found in Task 5 of the allowlist-sanitizer stage, while re-sanitizing the corpus broke
`crates/harness/src/claude/commands.rs::an_empty_argument_hint_is_absent_not_blank`.

## The mechanism

`crates/harness/src/capture/sanitize.rs`'s `Redactor::sanitize_scalar` redacts every `String`
scalar on a path that is not on the provider's allowlist, with no exception for an empty string.
An empty string and a populated one are redacted identically — both become a non-empty
placeholder token (`<V22>`, `<SESSION_1>`, …), because a placeholder is never itself empty.

This is invisible for almost every field: Comet's decoders read *content*, and content that was
never observably empty on the wire loses nothing by becoming an opaque token instead of a
specific one. But a field whose **emptiness is itself the meaningful signal** — not "what does it
say" but "did the provider send anything at all" — loses exactly the bit that mattered. Once such
a field's path is excluded from the allowlist, the corpus can no longer demonstrate that the
provider ever sends the empty case, because every archived occurrence, empty or not, now reads as
"present."

## The worked example

`.response.response.commands[].argumentHint` (Claude's slash-command discovery reply) is not on
`claude.txt` — excluded as part of the whole `commands[]` family (installed-tooling identity, Task
1 of this stage). The real CLI sends `argumentHint: ""` for a command that takes no arguments;
`crates/harness/src/claude/commands.rs::non_empty()` exists specifically to fold that into `None`
so the UI doesn't render an empty hint slot. Before this stage, `an_empty_argument_hint_is_absent_
not_blank` read that empty string straight off the corpus and asserted `non_empty()` turned it into
`None`. After the re-pass, every archived `argumentHint` is a placeholder like `<V22>` —
`non_empty(Some("<V22>"))` is `Some(_)`, never `None` — so the test no longer has any evidence in
the corpus that the empty case exists at all, let alone that the decoder handles it.

## The fix taken here, and its limit

Task 5 moved the test off corpus evidence entirely: it is now a hand-written fixture asserting
directly against an inline JSON literal, testing `non_empty()` — Comet's own decoder — rather than
what the provider sends. That is legitimate (the function's contract does not depend on which CLI
version produced the empty string), but it means the corpus can no longer *prove* the provider
still behaves this way. If Claude Code ever stopped sending `argumentHint: ""` and started omitting
the key instead, nothing here would notice — the inline fixture only proves Comet's own code
reacts correctly to an empty string if handed one, not that one is still handed.

## What to watch for

Any future field where "absent" and "present-but-empty" are a decoded distinction — the same
shape as `argumentHint`, or the `description`/`content` fields decision 2 of Task 1 already
redacts — inherits this same blind spot the moment its path is excluded from the allowlist. There
is no general fix: the allowlist's whole point is to withhold content by default, and an empty
string is content. The only mitigation available at review time is to notice, when excluding a
path, whether its **absence-vs-emptiness** distinction is one Comet's own decode logic depends on,
and if so, to give that decode its own hand-written fixture test up front rather than discovering
the gap the next time the corpus is regenerated.
