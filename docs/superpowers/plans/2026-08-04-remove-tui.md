# Full-Screen TUI Removal Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remove the full-screen terminal viewport using upstream commit `7b52ce1` while preserving Comet's headed app, direct-LAN behavior, and ordinary CLI commands.

**Architecture:** Treat upstream `7b52ce1` as the source of truth for the deletion. Add a CLI regression guard first, then cherry-pick and resolve only fork-divergence conflicts so the TUI crate, binary, subcommand, dependencies, scripts, and current documentation disappear without reverting LAN-specific code.

**Tech Stack:** Rust 2024, Cargo workspace, Clap CLI, Python `unittest`, Git cherry-pick.

## Global Constraints

- Preserve the headed app and `comet headless`, `status`, `remote`, `daemon`, and `update` commands.
- Preserve direct-LAN client, identity, RPC, engine, and desktop UI behavior.
- Keep `comet_proto::view` and `comet_proto::motion` as shared pure derivations.
- Leave historical documents under `docs/superpowers/plans` and `docs/superpowers/specs` unchanged.
- Do not add replacement terminal-interface functionality.

---

### Task 1: Guard the CLI removal boundary

**Files:**
- Modify: `apps/comet/src/main.rs`
- Test: `apps/comet/src/main.rs`

**Interfaces:**
- Consumes: Clap's existing private `Cli` parser and `Command` enum.
- Produces: A regression test proving `comet tui` is not accepted after removal.

- [ ] **Step 1: Add the failing CLI regression test**

Add this test to the existing `mod tests` in `apps/comet/src/main.rs`:

```rust
#[test]
fn rejects_removed_tui_subcommand() {
    assert!(Cli::try_parse_from(["comet", "tui"]).is_err());
}
```

- [ ] **Step 2: Run the test and verify it fails before removal**

Run `cargo test -p comet tests::rejects_removed_tui_subcommand -- --exact`.

Expected: FAIL because the current parser accepts `tui` and returns `Command::Tui`.

- [ ] **Step 3: Commit the red regression guard**

```powershell
git add apps/comet/src/main.rs
git commit -m "test: guard removal of TUI command"
```

### Task 2: Cherry-pick the upstream TUI removal

**Files:**
- Delete: `apps/tui/**`
- Delete: `crates/tui/**`
- Delete: `scripts/frame_png.py`
- Delete: `scripts/tui-screenshots.py`
- Delete: `scripts/tui-smoke.py`
- Delete: `scripts/tui_capture.py`
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `apps/comet/Cargo.toml`
- Modify: `apps/comet/src/main.rs`
- Modify: `README.md`
- Modify: `ARCHITECTURE.md`
- Modify: `docs/memory-plan.md`
- Modify: `crates/engine/src/doc_host.rs`
- Modify: `crates/proto/src/motion.rs`
- Modify: `crates/proto/src/view.rs`
- Modify: `crates/ui/src/motion.rs`
- Modify: `crates/ui/src/state.rs`

**Interfaces:**
- Consumes: Upstream commit `7b52ce1f70b3dddf13756358c4dc1f9d810a0bad` and the regression test from Task 1.
- Produces: A Cargo workspace and `comet` CLI with no TUI surface and all non-TUI commands intact.

- [ ] **Step 1: Cherry-pick the upstream commit**

Run `git cherry-pick 7b52ce1f70b3dddf13756358c4dc1f9d810a0bad`.

Expected: either a clean cherry-pick or conflicts only in files changed by both the LAN fork and the upstream removal.

- [ ] **Step 2: Resolve conflicts to preserve both deletion and LAN behavior**

For each conflicted file, retain these exact outcomes:

- `Cargo.toml`: remove `crates/tui`, `apps/tui`, `comet-tui`, Ratatui, and TUI-only dependencies; retain `crates/client`, `crates/identity`, LAN dependencies, and GPUI revision `ac135eb227c0784e4bb8f09ae3c3ff6bffd8a827`.
- `apps/comet/src/main.rs`: remove `Command::Tui`, its dispatch arm, and the TUI tracing exception; retain `Headless`, `Status`, `Remote`, `Daemon`, `Update`, the headed default, LAN configuration, and `rejects_removed_tui_subcommand`.
- `apps/comet/Cargo.toml`: remove only `comet-tui`; retain LAN/client/identity dependencies.
- Current documentation: remove claims that TUI is supported while retaining direct-LAN architecture and CLI documentation.
- Source comments: remove TUI-specific wording while retaining shared `proto::view` and `proto::motion` code.

After resolving, run:

```powershell
git add -A
git cherry-pick --continue
```

- [ ] **Step 3: Validate the lockfile**

Run `cargo metadata --no-deps --format-version 1 > $null`.

If Cargo reports a lockfile error, run:

```powershell
cargo generate-lockfile
git add Cargo.lock
git commit --amend --no-edit
```

- [ ] **Step 4: Verify the CLI regression guard is green**

Run `cargo test -p comet tests::rejects_removed_tui_subcommand -- --exact`.

Expected: PASS.

### Task 3: Verify the remaining product surface

**Files:**
- Test: `scripts/tests/test_sync_upstream.py`
- Test: remaining Rust workspace

**Interfaces:**
- Consumes: The TUI-free workspace from Task 2.
- Produces: Evidence that the current product surface builds and that no live TUI references remain.

- [ ] **Step 1: Check live references and workspace metadata**

Run:

```powershell
rg -n -i "comet[- ]?tui|comet tui|apps/tui|crates/tui|tui-smoke|tui_capture|tui-screenshots|frame_png" Cargo.toml Cargo.lock README.md ARCHITECTURE.md apps crates scripts docs/memory-plan.md
cargo metadata --no-deps --format-version 1 > $null
```

Expected: `rg` returns no live references; Cargo metadata exits 0. Historical references under `docs/superpowers` are intentionally outside this scan.

- [ ] **Step 2: Verify CLI help**

Run `cargo run -p comet -- --help`.

Expected: help lists `headless`, `status`, `remote`, `daemon`, and `update`; it does not list `tui`.

- [ ] **Step 3: Run formatting and helper tests**

```powershell
cargo fmt --all -- --check
python -m unittest discover -s scripts/tests -v
```

Expected: both commands exit 0; the helper suite reports 37 passing tests.

- [ ] **Step 4: Run the remaining Rust workspace tests**

Run `cargo test --workspace`.

Expected: all tests unrelated to the already identified Windows path-normalization defect pass. If `repos_round_trip_add_branches_worktrees` still fails because Git emits `/` while the test expects `\`, report it separately and run the directly affected packages to completion.

- [ ] **Step 5: Inspect the final commit and worktree**

```powershell
git diff --check
git status --short --branch
git log --oneline -5
```

Expected: no diff errors, clean worktree, and history contains the CLI guard plus the cherry-picked upstream removal.
