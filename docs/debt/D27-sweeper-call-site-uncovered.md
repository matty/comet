# D27 — the sweeper's production call site is untested

**Half closed 2026-08-30: the transposition is now a type error.** This page's own
"How to apply" recommended a newtype rather than a tighter test, and that is what landed —
`UnattendedBound(Duration)` in `unattended.rs`, taken by `spawn_unattended_sweeper` and by
`sweep_interval`, which still returns a plain `Duration`. Passing the cadence where the bound
belongs no longer type-checks; verified by writing the transposition and watching the build
fail at `lib.rs:424`. Nothing about the timing assertion this page rejected was needed.

**The deletion half stands open, and the ruling below stands with it.** Nothing type-checks a
call that is not there, and reaching it still needs `assemble_runtime` to accept an injected
registry — a production signature widened for test reachability, which is the cost the ruling
declined. Do not read the half-close as licence to reopen that one.

**Controller ruling during 1.9's Task 5 review: do not reopen.** The gap is
real, understood, and left open on purpose because closing it costs more than
it buys — recorded here so a later reader doesn't reopen it expecting a quick
fix.

`Engine::assemble_runtime` (`crates/engine/src/lib.rs:389-393`) spawns the
sweeper with three argument expressions:

```rust
let sweeper = spawn_unattended_sweeper(
    core.sessions.clone(),
    core.presence(),
    config.unattended_timeout,
);
```

**No test exercises this call site, and the earlier version of this page
claimed otherwise.** Every test builds `EngineCore::assemble` and calls
`spawn_unattended_sweeper` itself (or calls `expire_unattended` directly),
bypassing `assemble_runtime` entirely.
`the_spawned_sweeper_expires_a_parked_approval_on_its_own` is the closest
thing, and what it proves is that `spawn_unattended_sweeper` works when
called — not that anything calls it. **Deleting the spawn above would still
leave the whole workspace suite green**, which is the sharp form of this gap and
it is open, not closed. `assemble_runtime` can't be driven from a test as things
stand: it hard-codes `default_registry()`, whose only parkable harness is gated
on the process-global `COMET_MOCK_APPROVAL` env var — the same parallel-test
race `ScriptedHarness` exists to avoid elsewhere in `e2e.rs`.

**The second uncovered mistake is an argument transposition**, specifically
swapping `config.unattended_timeout` for
`sweep_interval(config.unattended_timeout)` (passing the wake-up cadence where
the deadline bound belongs, or vice versa). Both orderings expire a parked
approval well inside any test's assertion window at test scale, because
`sweep_interval` clamps: it is `bound/4` bounded into `[250ms, 60s]`, so it
returns something *smaller* than its input only above 250ms, exactly **equal**
at 250ms, and **larger** below it — `sweep_interval(100ms) == 250ms`, and 100ms
is the bound the covering test actually uses. Either way both durations are
small enough that a bound of `B` and a bound of `sweep_interval(B)` fire within
the same observable moment. Distinguishing them needs an assertion of the shape
"has NOT expired at T, HAS expired at T+ε," which is exactly the flake trap the
real-wall-clock sweeper design (`chrono`, not `Instant`, see `unattended.rs`'s
module doc) exists to avoid encoding into a test.

**How to apply, if it ever needs closing.** The recommended fix is a type-level
guard, not a tighter test: give `spawn_unattended_sweeper`'s third parameter a
distinct type from `Duration` (a newtype `UnattendedBound(Duration)`) so
`sweep_interval`'s `Duration` return can't type-check in that position. The
transposition then stops being a possible mistake, with no timing assertion to
flake.

That leaves the deletion half — nothing type-checks a call that isn't there.
Closing it needs `assemble_runtime` to accept an injected registry so a test can
hand it a parkable harness without the env var, at which point the existing
sweeper test can be pointed at the real assembly path.
