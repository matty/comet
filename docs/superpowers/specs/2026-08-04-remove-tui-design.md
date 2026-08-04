# Remove the full-screen TUI

## Goal

Remove Comet's full-screen terminal viewport while preserving the headed app,
headless engine, direct-LAN functionality, and ordinary `comet` CLI commands.

## Source of truth

Use upstream commit `7b52ce1` (`Remove the TUI: crates/tui, apps/tui, comet tui
subcommand, scripts, docs`) as the removal patch. Resolve conflicts only where
this fork has diverged from upstream; do not reimplement the deletion from
scratch or broaden it into unrelated cleanup.

## Removal boundary

The change removes:

- the `crates/tui` library and its tests;
- the standalone `apps/tui` development binary;
- the `comet tui` subcommand and `comet-tui` dependency;
- TUI-only workspace members and Ratatui dependencies;
- TUI capture, screenshot, smoke-test, and frame-rendering scripts;
- current README, architecture, memory-plan, and source-comment claims that the
  TUI is a supported viewport.

The shared pure `comet_proto::view` and `comet_proto::motion` derivations stay.
Historical implementation plans remain unchanged as records of past work.

## Preserved behavior

The following remain available and must continue to compile and parse:

- the headed app (`comet` with no subcommand);
- `comet headless`;
- `comet status`;
- `comet remote ...`;
- `comet daemon ...`;
- `comet update`.

The fork's direct-LAN client, identity, RPC, engine, and desktop UI components
must not be removed or reverted while resolving the cherry-pick.

## Verification

After the cherry-pick and conflict resolution:

1. Confirm no workspace member or live source/documentation reference requires
   `crates/tui`, `apps/tui`, `comet-tui`, or `comet tui`.
2. Confirm the ordinary CLI help contains the preserved commands and no `tui`
   command.
3. Run formatting checks, upstream-sync helper tests, and the remaining Rust
   workspace tests, reporting any unrelated pre-existing failures separately.
4. Confirm the worktree is clean and the removal is represented by a commit
   derived by cherry-picking upstream `7b52ce1`.
