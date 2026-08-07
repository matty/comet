# AGENTS.md

Guidance for any coding agent working in this repository. This is the source of truth;
`CLAUDE.md` points here so the two can't drift.

Comet is a local-first native desktop app (gpui) that runs Claude Code and Codex sessions on
this machine or on explicitly paired machines on the LAN. There is no account, hosted control
plane, or cloud sync. Read `ARCHITECTURE.md` for the trust and process boundaries and
`docs/PARITY.md` for the implemented feature surface when a change touches either.

## Workspace

Rust 2024 workspace; the toolchain is pinned by `rust-toolchain.toml` (stable, with
`rustfmt` and `clippy`). `crates/*` are libraries — `proto` (wire types), `rpc`
(localhost + pinned-TLS transport, pairing), `engine` (storage and authoritative
operations), `client` (remote supervision), `ui` (gpui surface), plus `harness`, `doc`,
`sync`, `identity`, `update`. `apps/comet` is the only binary.

Two non-Rust trees, neither on the runtime path:

- `edge/` — Cloudflare Worker serving installer/release artifacts. npm + wrangler + vitest.
- `scripts/` — bash packaging scripts and `sync-upstream.py`, with Python `unittest`
  suites in `scripts/tests/`.

`apps/ios` is out of scope for this release; don't change it unless asked.

## Verify before claiming done

Run all three and report the actual output — never claim a change works without it:

```bash
cargo fmt --all
cargo clippy --workspace --all-targets
cargo test --workspace
```

Also run, when the change touches them:

```bash
cd edge && npm install && npm run typecheck && npm test   # edge/
python -m unittest discover -s scripts/tests              # scripts/*.py
```

`.github/workflows/ci.yml` runs the same commands on every PR (with `fmt --check`).
`.github/workflows/release.yml` is separate and only builds nightly releases. Run the gate
locally first — don't use CI as the first place a change is checked.

Clippy is not yet `-D warnings`: the workspace carries ~24 pre-existing warnings. Don't add
new ones, and prefer fixing any you touch.

Full procedure: `.agents/workflows/verify.md`.

## The gpui fork rev is load-bearing

`gpui` comes from `wingleeio/zed` at a pinned rev, not from crates.io. Comet depends on
fork-only changes: `Window::with_edge_fade`, `Window::paint_backdrop_blur`,
`ImageSource::evict`, a line-wrap fix, and renderer GPU-memory bounds. The crates.io `gpui`
release does not have them and will not compile.

Never bump the rev to "get something newer". Bumping means rebasing the fork's
`comet/line-wrap-closing-punctuation` branch onto the new upstream and re-verifying every
commit listed in the `Cargo.toml` comment above the `gpui` dependency — keep that comment
current when the rev changes.

## Upstream sync

`upstream` is `zeronsh/comet`. Never cherry-pick from it by hand — that desynchronizes the
`.github/upstream-sync.json` ledger. Always use `python scripts/sync-upstream.py`
(`--resume` after resolving a conflict). Full procedure:
`.agents/workflows/sync-upstream.md`.

## Windows is the primary dev machine

Prefer PowerShell; use bash only for genuinely POSIX scripts. `scripts/*.sh`
(`package-linux.sh`, `package-macos.sh`, `e2e-smoke.sh`) do not run here — they are
CI/platform scripts. Windows path and process-lifetime behavior has bitten this repo
repeatedly (see the `fix: ... on Windows` commits); when touching harness CLI resolution,
terminal exit, or diff capture, check the Windows path explicitly.

Parallel branches live in `.worktrees/` *inside* the repo and are gitignored. Files under
`.worktrees/` belong to another branch — never edit them while working in the main checkout.

## Git

Never commit to `main`. Branch as `fix/…`, `feature/…`, `ci/…`, `test/…`, or `docs/…`, then
open a PR with `gh`. PRs are squash-merged, so the squashed subject carries `(#N)`.
Commit subjects are conventional and lowercase with an optional scope:
`fix(ci): preserve Windows dependency version`, `test(engine): cover concurrent multi-client LAN access`.
Full procedure: `.agents/workflows/commit-pr.md`.

## crates/ui conventions

Read `.agents/rules/gpui-ui.md` before writing UI code. In short: layout constants are plain
numbers and never depend on which color is painted; every color comes from a `Theme` token;
light mode is designed, not inverted; anything caching paint across frames keys on
`theme_generation()`.

## What the user is allowed to see when something fails

Read `.agents/rules/user-facing-errors.md` before writing any surface that can fail or wait.
Two hard rules: the user never sees a raw technical error (no `err.to_string()` on screen),
and no waiting state can last forever — every skeleton needs a reply, a timeout, or a bounded
retry that gives up into something actionable. Failures split into a short `summary` and an
actionable `hint`, with the diagnostic detail left in `tracing`.

## Shared procedures

Agent-agnostic procedures live in `.agents/`. Read the relevant one before starting that kind
of task:

| Task | File |
| --- | --- |
| Verify a change before calling it done | `.agents/workflows/verify.md` |
| Pull selected commits from `upstream` | `.agents/workflows/sync-upstream.md` |
| Commit, push, open a PR | `.agents/workflows/commit-pr.md` |
| Write UI code in `crates/ui` | `.agents/rules/gpui-ui.md` |
| Write a surface that can fail or wait | `.agents/rules/user-facing-errors.md` |

Claude Code exposes these as the slash commands `/verify`, `/sync-upstream`, and
`/commit-pr` via thin wrappers in `.claude/skills/`, which read the files above rather than
copying them. Other agents should read the files directly.

## Automated formatting

`python scripts/format-file.py <path>` formats one edited file (`rustfmt` for `.rs`,
`ruff`/`black` for `.py` when installed). It also accepts a Claude Code hook payload on
stdin, which is how the `PostToolUse` hook in `.claude/settings.json` invokes it after every
Write/Edit. Agents without a hook mechanism should call it with an explicit path after
editing, or run `cargo fmt --all` before committing. It is a no-op for other file types and
always exits 0.
