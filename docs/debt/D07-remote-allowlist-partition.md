# D7 — the remote allowlist partition is conventional, not mechanical

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
