# Verify

Run the project's gate and report the real output. Never claim a change works, builds, or
passes without having run these and read the result.

`.github/workflows/ci.yml` runs the same commands on every PR, with `cargo fmt --all --
--check` instead of the rewriting form. Run them locally first — CI is the backstop, not the
first check.

## Always (Rust)

```bash
cargo fmt --all
cargo clippy --workspace --all-targets
cargo test --workspace
```

- `cargo fmt --all` rewrites files; run it first so clippy and tests see formatted code.
  In a check-only context use `cargo fmt --all -- --check`.
- `--all-targets` matters: it covers the integration suites under `crates/*/tests/`, which
  plain `cargo clippy` skips.
- Clippy is not `-D warnings` — the workspace carries ~24 pre-existing warnings (mostly
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
