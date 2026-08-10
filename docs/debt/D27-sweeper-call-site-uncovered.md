# D27 — the sweeper's three call-site arguments are unasserted

**Controller ruling during 1.9's Task 5 review: do not reopen.** The gap is
real, understood, and left open on purpose because closing it costs more than
it buys — recorded here so a later reader doesn't reopen it expecting a quick
fix.

`Engine::assemble_runtime` (`crates/engine/src/lib.rs:380-384`) spawns the
sweeper with three argument expressions:

```rust
spawn_unattended_sweeper(
    core.sessions.clone(),
    core.presence(),
    config.unattended_timeout,
);
```

No test exercises this call site directly — every e2e test builds
`EngineCore::assemble` and calls `spawn_unattended_sweeper` itself (or calls
`expire_unattended` directly), bypassing `assemble_runtime` entirely. A first
review round caught the sharper version of this gap — the spawn could be
deleted outright and the whole workspace suite would still pass — and that was
fixed: `the_spawned_sweeper_expires_a_parked_approval_on_its_own` now proves
the spawn exists and fires.

**What that test still cannot catch: an argument transposition at this call
site**, specifically swapping `config.unattended_timeout` for
`sweep_interval(config.unattended_timeout)` (passing the wake-up cadence where
the deadline bound belongs, or vice versa). Both orderings expire a parked
approval well inside the fixed test's assertion window — `sweep_interval`
returns a *smaller* duration than its input across the whole clamped range
`[250ms, 60s]`, so a bound of `B` and a bound of `sweep_interval(B)` both fire
within the same observable few seconds at test scale. Distinguishing them
needs an assertion of the shape "has NOT expired at T, HAS expired at T+ε,"
which is exactly the flake trap the real-wall-clock sweeper design (`chrono`,
not `Instant`, see `unattended.rs`'s module doc) exists to avoid encoding into
a test.

**How to apply, if it ever needs closing.** The safer fix is not a tighter
test — it's removing the transposition as a possible mistake, e.g. by giving
`spawn_unattended_sweeper`'s third parameter a distinct type from
`Duration` (a newtype `UnattendedBound(Duration)`) so `sweep_interval`'s
`Duration` return can't type-check in that position. That closes the gap
without a timing-sensitive test.
