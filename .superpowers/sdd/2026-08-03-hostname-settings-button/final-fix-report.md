# Final fix report: hostname and Settings button

## Status

Both Important final-review findings are resolved.

## Engine consistency fix

- `EngineCore::assemble` resolves the detected hostname once and passes it into
  `WorkspaceHost::open`.
- `WorkspaceHost::open` records the effective startup name after applying the
  generated-sentinel repair/custom-name preservation rule.
- `ServerHello` now uses that effective workspace startup name instead of
  invoking the hostname resolver independently.
- Remote-access regressions cover a fresh generated name and a custom name
  persisted across restart; both assert equality between the local workspace
  device row and `ServerHello.name`.

## Missing coverage added

- Hostname-file fallback after empty environment candidates.
- Final `unknown-device` fallback after all candidates are missing/blank.
- A UI source-boundary test isolates production source before `#[cfg(test)]`,
  confirms the rendered sidebar contains the bottom Settings button, checks the
  actual `this.open_settings(BOTTOM_SETTINGS_SECTION, cx)` listener token and
  Devices target, and verifies obsolete account-menu state/rendering tokens are
  absent without matching its own assertions.

## TDD evidence

Before production changes:

```text
cargo test -p comet-engine --test remote_access preserved_custom_workspace_name_matches_server_hello_after_restart -- --nocapture
```

Failed as intended: the persisted workspace row was `Rendering workstation`
while `ServerHello.name` was the independently resolved `DESKTOP-AEAIL4K`.

The hostname fallback tests and UI source-boundary test passed immediately
because they add required coverage for already-correct behavior; no production
change was needed for those paths.

After the minimal engine change:

```text
cargo test -p comet-engine --test remote_access workspace_name_matches_server_hello -- --nocapture
```

Result: 2 passed, 0 failed.

## Final verification

- `cargo test -p comet-engine --lib` — 39 passed, 0 failed.
- `cargo test -p comet-engine --test remote_access` — 22 passed, 0 failed.
- `cargo test -p comet-ui --lib` — 345 passed, 0 failed.
- `cargo fmt --all` — completed successfully before the suites.
- `git diff --check` — clean apart from Git's informational LF-to-CRLF warnings.

## Scope and concerns

- Changes are limited to engine naming assembly, the remote regression tests,
  the sidebar boundary test, and this report.
- No stash was inspected, applied, dropped, or changed.
- The UI suite continues to emit the two pre-existing dead-code warnings in
  `app_menus.rs` and `sound.rs`; there are no new warnings or blockers.
