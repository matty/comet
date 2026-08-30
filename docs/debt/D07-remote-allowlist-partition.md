# D7 — the remote allowlist partition is conventional, not mechanical

**CLOSED 2026-08-30.** `every_declared_rpc_method_is_allowed_or_denied_on_purpose`
(`crates/engine/tests/remote_access.rs`) reads every `pub const … = "Method"` out of
`comet_rpc`'s source and fails when one is on neither side of the partition — the exact
invisibility below. It also fails on a method listed on BOTH sides, on a listed method
`comet_rpc` no longer declares, and where a list disagrees with what `remote_method_allowed`
actually answers.

The partition was complete when the check landed: 54 declared, 17 denied, 37 allowed. The
count in the note below (51 / 17 / 34) is what it was in 0b.2, kept because the drift between
the two numbers is the point — three methods were added and partitioned correctly by hand, and
nothing would have said so had they not been.

**The declared set is read from source deliberately.** A `methods::ALL` array would move the
problem up one level and be the thing nobody updates; the source scan cannot go stale because
the declarations are what it reads.

The original note follows.

**Still unowned as of 2026-08-09.** The Phase 1 spec assigned this to 1.4, on the premise that
`RespondApproval` would be a new RPC method. It is not — it is a `SessionCommandPayload` queued
through `QUEUE_COMMAND`, which was already allowed, so 1.4 changed neither
`remote_method_allowed()` nor `remote_access.rs`. Nothing has exercised this since 0b.2. The next
slice that genuinely adds an RPC method inherits it; on current plans that is 2.1/2.2 or 3.2,
whichever first adds the `ListHarnesses` push channel.

`crates/engine/tests/remote_access.rs` asserts an exhaustive allow/deny split
over `comet_rpc::methods` — 51 constants, 17 denied, 34 allowed — but only via
two hand-written literal lists. No macro, no `strum`, no build-time scan. A
method added to neither list silently defaults to denied by
`remote_method_allowed()` with **no test failure**. That is fail-closed, so it is
not a security hole, but it is invisible: 0b.2's `ListHarnessDiagnostics` slipped
through exactly this way and was only caught because a reviewer went looking.
A compile-time or test-time exhaustiveness check over the method constants would
close it.
