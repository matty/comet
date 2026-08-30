# Verify

Run the project's gate and report the real output. Never claim a change works, builds, or
passes without having run these and read the result.

`.github/workflows/ci.yml` runs the same commands on every PR, with `cargo fmt --all --
--check` instead of the rewriting form, and tests run with `--profile ci`, which adds one
retry — a timeout that passes on retry reports as flaky rather than failing the job, so a green
CI run does not guarantee the local `--profile default` run (no retries) would also be green.
Run them locally first — CI is the backstop, not the first check.

## Always (Rust)

```bash
cargo fmt --all
cargo clippy --workspace --all-targets
cargo nextest run --workspace
```

- `cargo fmt --all` rewrites files; run it first so clippy and tests see formatted code.
  In a check-only context use `cargo fmt --all -- --check`.
- `--all-targets` matters: it covers the integration suites under `crates/*/tests/`, which
  plain `cargo clippy` skips.
- Tests run under `cargo-nextest` (`cargo install cargo-nextest --locked` if it isn't on
  PATH), not plain `cargo test`: `.config/nextest.toml` kills and names a test still running
  at 180s instead of letting it block the run forever — a hung test must be reported, not
  waited on. `.config/nextest.toml` does not change what plain `cargo test` does, so
  `cargo test` still reproduces a hang if you need to demonstrate one. **nextest runs no
  doctests** — the workspace has none runnable today, but if you add a `rust`-fenced doc
  example later, `cargo nextest run` will not run it; `cargo test --workspace --doc` is the
  check.
- Clippy is not `-D warnings` — the workspace carries 26 pre-existing warnings (mostly
  `cloned_ref_to_slice_refs` in `comet-ui`/`comet-engine` and doc-list indentation). Compare
  against that baseline: don't add new ones, and fix any in code you touched.
- The first build after a `gpui` change is slow — gpui and its deps build at `opt-level = 2`
  even in dev (`[profile.dev.package."*"]`). Don't interpret a long build as a hang.

## A failure that matches a known-flaky target still has to be measured

Never write off a failure as "the known flake" from its name or its assertion
text. Both have been wrong here. Establish it with two cheap checks, in this
order:

1. **Can your diff even reach the failing path?** Read the diff against the test.
   A change the test never executes settles the question outright — faster and
   more conclusive than any amount of re-running.
2. **If it can, run the target several times on your commit *and* on the merge
   base.** A single failure is not signal, and neither is a single pass. Compare
   rates, not one run against the other.

Two traps this exists for, both real:

- **A recorded flake rate can be badly out of date.** One target documented as
  failing ~1-in-3 was measured at 5-of-6, which reads as a fresh regression.
- **A target can pass under full-workspace parallelism and fail in isolation.**
  The extra latency lets a racing write land. So `cargo test --workspace` looks
  clean, a targeted re-run fails, and the re-run looks like the break.

When a measurement disagrees with what is recorded, correct the record and keep
the wrong version with a note on why it misled. A wrong explanation that sounds
right is what costs the next person their afternoon — the failure's symptom
routinely names the wrong culprit.

## Writing a test that waits

**Wait on the condition; sleep only to prove absence.** A test that sleeps past
a deadline puts its own correctness in the margin — `sleep(400ms)` against a
300ms grace has 100ms of slack, and under load an orphan stamp landing late
leaves the next pass short of the grace, so the test fails about code that is
right. Poll the condition with a generous deadline instead: the deadline only
ever bounds a FAILURE, so it costs nothing when the code works, and there is no
margin to get wrong. `diff_sync_churn.rs`'s `wait_for_eviction` and
`wait_chat_state` are the shape.

Two things that keep a poll honest:

- **Keep the negative half.** "Eventually gone" passes just as happily against
  a grace of zero, so assert first that one pass BEFORE the deadline does not
  remove the entry. That half is what makes the wait a test of the grace rather
  than of the loop.
- **A fixed sleep is still right for proving a NON-event** ("nothing was
  re-published"), because there is no condition to wait for. Accept that it
  proves absence only within the window it waits, and say so where it sits.

Three rows in this family are already recorded — `docs/debt/README.md`'s D89,
D126 and D129 — each a budget sized on the machine that wrote it. The pattern
is worth naming: an idle developer machine is the least representative one this
code runs on.

## When the change touches `edge/`

```bash
cd edge
npm install   # node_modules is not checked in; tsc/vitest are local devDependencies
npm run typecheck
npm test
```

## When the change touches `scripts/*.py`

```bash
python -m unittest discover -s scripts/tests
```

Run this from the repo root. Do not add `-t .` — `scripts/tests/` has no `__init__.py`, so
setting the top-level directory makes discovery fail with `Start directory is not importable`.

## Reporting

State what you ran and what happened. If something failed, quote the failing output rather
than summarizing it. If you skipped a step, say which and why. A change that needed a
Windows-specific path (harness CLI resolution, terminal exit, diff capture) is not verified
until it has been exercised on Windows.
