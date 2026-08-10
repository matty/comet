# D26 — `DoneStatus` cannot tell an unattended expiry from a user's Stop

**Deliberate, per the 1.9 spec (§8), not an oversight — recorded here because a
future reader of `interrupt()` will otherwise assume the two are already
distinguishable.**

`Sessions::expire_unattended` (`crates/engine/src/sessions.rs:667`) ends a
parked turn by calling the same `interrupt()` a user's Stop button calls
(`:615`). Both paths settle the run with `DoneStatus::Interrupted`
(`crates/proto/src/agent.rs:680`) — there is no third variant for "nobody was
there to answer." The only place the two are told apart is the transcript: the
sweeper writes an `AgentEvent::Error` with `unattended_note()`'s text
*immediately before* calling `interrupt`, so a reader has to find that message
and infer the cause from prose rather than from the status enum.

**Why this slice didn't add the variant.** `DoneStatus` crosses RPC inside
`AgentEvent::Done`, which — like the `ApprovalRequested`/`ApprovalResolved`
question 1.4 answered — makes a new variant a `PROTOCOL_VERSION` bump (see
`PROGRESS.md`'s "Wire compatibility" note on why `AgentEvent` variants
themselves don't cross RPC but the enums they carry, like `DoneStatus`, do).
That is a larger, more considered change than this slice's scope, and the
spec named it out of scope explicitly rather than by omission.

**How to apply.** Add `DoneStatus::Expired` (or similar) the next time
`PROTOCOL_VERSION` bumps for an unrelated reason — bundling it avoids a bump
whose only justification is this one enum variant. Any code that currently
matches `DoneStatus::Interrupted` for "the user stopped it" needs an audit
pass first: some of those call sites may currently rely on Interrupted
meaning "not the model's fault," which an unattended expiry also satisfies, so
adding the variant is not purely additive to read sites even though it is
additive to the wire.
