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
