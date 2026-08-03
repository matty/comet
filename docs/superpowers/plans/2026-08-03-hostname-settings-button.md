# Hostname Device Name and Settings Button Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Use the system hostname for generated local-device names and replace the desktop sidebar account menu with a direct labeled Settings button.

**Architecture:** Keep hostname resolution in the engine, where both the workspace device row and `ServerHello` already obtain their name. Make resolution and sentinel repair pure/testable at their boundaries. In the GPUI shell, remove account-menu state and render one direct navigation row targeting Settings > Devices.

**Tech Stack:** Rust, Tokio, GPUI, existing Comet engine/workspace models, Cargo tests.

## Global Constraints

- `COMET_DEVICE_NAME` remains the highest-priority explicit override.
- Windows `COMPUTERNAME` precedes `HOSTNAME`; `/etc/hostname` remains the final system lookup.
- Empty and whitespace-only values are ignored.
- Only empty names, `unknown-default`, and `unknown-device` are repaired; deliberate user names survive restarts.
- The bottom-left row is gear icon + `Settings` and opens Settings > Devices directly.
- Agent-provider accounts remain available in Settings > Accounts.
- Do not change remote, pairing, listener, trust, or updater behavior.

---

### Task 1: Resolve and repair the local device hostname

**Files:**
- Modify: `crates/engine/src/lib.rs:160-175, 230-245, 416-425, 440-end`
- Modify: `crates/engine/src/workspace_host.rs:90-118, end-of-file`
- Test: inline unit tests in `crates/engine/src/lib.rs` and `crates/engine/src/workspace_host.rs`

**Interfaces:**
- Consumes: `WorkspaceHostConfig { device_name: String, .. }`, existing device rows from `WorkspaceDoc`.
- Produces: `local_device_name_from(getenv, read_hostname) -> String` and `startup_device_name(existing, detected) -> String` private helpers used by engine startup.

- [ ] **Step 1: Add failing hostname-resolution tests**

Add table-driven tests to `crates/engine/src/lib.rs` using closures rather than process-wide environment mutation:

```rust
#[test]
fn local_name_prefers_override_then_windows_hostname() {
    let name = local_device_name_from(
        |key| match key {
            "COMET_DEVICE_NAME" => Some("  Lab Override  ".into()),
            "COMPUTERNAME" => Some("BUILD-PC".into()),
            "HOSTNAME" => Some("unix-host".into()),
            _ => None,
        },
        || Some("file-host".into()),
    );
    assert_eq!(name, "Lab Override");

    let windows = local_device_name_from(
        |key| (key == "COMPUTERNAME").then(|| "BUILD-PC".into()),
        || None,
    );
    assert_eq!(windows, "BUILD-PC");
}

#[test]
fn local_name_ignores_empty_values_and_falls_back() {
    let hostname = local_device_name_from(
        |key| match key {
            "COMET_DEVICE_NAME" | "COMPUTERNAME" => Some("   ".into()),
            "HOSTNAME" => Some(" linux-box ".into()),
            _ => None,
        },
        || Some("file-host".into()),
    );
    assert_eq!(hostname, "linux-box");
}
```

- [ ] **Step 2: Run the engine unit tests and observe RED**

Run: `cargo test -p comet-engine --lib local_name -- --nocapture`

Expected: compilation fails because `local_device_name_from` does not exist.

- [ ] **Step 3: Implement the minimal cross-platform resolver**

Replace the inline environment chain with a helper of this shape:

```rust
fn local_device_name_from(
    getenv: impl Fn(&str) -> Option<String>,
    read_hostname: impl Fn() -> Option<String>,
) -> String {
    ["COMET_DEVICE_NAME", "COMPUTERNAME", "HOSTNAME"]
        .into_iter()
        .filter_map(getenv)
        .chain(read_hostname())
        .map(|value| value.trim().to_string())
        .find(|value| !value.is_empty())
        .unwrap_or_else(|| "unknown-device".to_string())
}

fn local_device_name() -> String {
    local_device_name_from(
        |key| std::env::var(key).ok(),
        || std::fs::read_to_string("/etc/hostname").ok(),
    )
}
```

Keep both existing engine call sites using `local_device_name()` so the workspace row and `ServerHello` remain consistent.

- [ ] **Step 4: Add failing sentinel-repair tests**

Add a pure helper and tests in `workspace_host.rs`:

```rust
#[test]
fn startup_name_repairs_generated_sentinels() {
    assert_eq!(startup_device_name(None, "BUILD-PC"), "BUILD-PC");
    assert_eq!(startup_device_name(Some(""), "BUILD-PC"), "BUILD-PC");
    assert_eq!(startup_device_name(Some("unknown-default"), "BUILD-PC"), "BUILD-PC");
    assert_eq!(startup_device_name(Some("unknown-device"), "BUILD-PC"), "BUILD-PC");
}

#[test]
fn startup_name_preserves_deliberate_rename() {
    assert_eq!(startup_device_name(Some("Rendering workstation"), "BUILD-PC"), "Rendering workstation");
}
```

- [ ] **Step 5: Run the sentinel tests and observe RED**

Run: `cargo test -p comet-engine --lib startup_name -- --nocapture`

Expected: compilation fails because `startup_device_name` does not exist.

- [ ] **Step 6: Implement sentinel repair at workspace startup**

Add:

```rust
fn startup_device_name(existing: Option<&str>, detected: &str) -> String {
    match existing.map(str::trim) {
        Some(name) if !name.is_empty()
            && !matches!(name, "unknown-default" | "unknown-device") => name.to_string(),
        _ => detected.to_string(),
    }
}
```

Use it when constructing the boot-time `Device` row:

```rust
name: startup_device_name(existing.as_ref().map(|device| device.name.as_str()), &config.device_name),
```

- [ ] **Step 7: Run focused and affected engine tests**

Run:

```powershell
cargo test -p comet-engine --lib
cargo test -p comet-engine --test remote_access
```

Expected: all tests pass, including hostname precedence and sentinel repair.

- [ ] **Step 8: Commit the hostname behavior**

```powershell
git add crates/engine/src/lib.rs crates/engine/src/workspace_host.rs
git commit -m "fix: name local device from system hostname"
```

---

### Task 2: Replace the account control with a direct Settings row

**Files:**
- Modify: `crates/ui/src/shell.rs:450-470, 630-655, 1880-1910, 2160-2305, test module near 3650-end`
- Test: inline unit test in `crates/ui/src/shell.rs`

**Interfaces:**
- Consumes: `Shell::open_settings(SettingsSection, cx)` and `SettingsSection::Devices`.
- Produces: `BOTTOM_SETTINGS_SECTION: SettingsSection` and `render_settings_button(theme, cx) -> AnyElement`.

- [ ] **Step 1: Add a failing direct-navigation contract test**

Define the intended target at module scope and first add the test:

```rust
#[test]
fn bottom_settings_button_targets_devices() {
    assert_eq!(BOTTOM_SETTINGS_SECTION, SettingsSection::Devices);
    assert_eq!(BOTTOM_SETTINGS_SECTION.label(), "Devices");
}
```

- [ ] **Step 2: Run the focused UI test and observe RED**

Run: `cargo test -p comet-ui --lib bottom_settings_button_targets_devices -- --nocapture`

Expected: compilation fails because `BOTTOM_SETTINGS_SECTION` does not exist.

- [ ] **Step 3: Replace the account menu with a Settings row**

Add:

```rust
const BOTTOM_SETTINGS_SECTION: SettingsSection = SettingsSection::Devices;
```

Remove `user_menu_open`, `user_menu_dismissed_at`, their constructor values,
the `user_line`/`user_email` lookup, and `render_user_menu`. Replace them with a
small renderer using the existing theme and icon primitives:

```rust
fn render_settings_button(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
    div()
        .id("sidebar-settings")
        .flex_none()
        .rounded(px(8.0))
        .px(px(Theme::SPACE_SM))
        .py(px(Theme::SPACE_SM))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(10.0))
        .cursor_pointer()
        .hover(|style| style.bg(theme.element_hover))
        .on_click(cx.listener(|this, _, _, cx| {
            this.open_settings(BOTTOM_SETTINGS_SECTION, cx)
        }))
        .child(icon(icons::SETTINGS_MINIMALISTIC).size(px(18.0)).text_color(theme.text_muted))
        .child(div().text_size(px(13.0)).font_weight(gpui::FontWeight::MEDIUM).text_color(theme.text).child("Settings"))
        .into_any_element()
}
```

Keep it pinned in the same bottom sidebar slot. Accounts remain reachable from
the existing Settings navigation section.

- [ ] **Step 4: Run focused and full UI library tests**

Run:

```powershell
cargo test -p comet-ui --lib bottom_settings_button_targets_devices -- --nocapture
cargo test -p comet-ui --lib
```

Expected: the focused contract and UI library suite pass.

- [ ] **Step 5: Verify obsolete account-menu code is gone**

Run:

```powershell
rg -n "user_menu_open|user_menu_dismissed_at|render_user_menu|user-menu-settings|Sign out|Alpha" crates/ui/src/shell.rs
```

Expected: no matches associated with the bottom sidebar control.

- [ ] **Step 6: Commit the Settings-row behavior**

```powershell
git add crates/ui/src/shell.rs
git commit -m "feat: replace sidebar account menu with settings"
```

---

### Task 3: Build, relaunch, and verify the Windows desktop

**Files:**
- No source files expected beyond Tasks 1-2.

**Interfaces:**
- Consumes: the `comet` binary built from this worktree.
- Produces: a running desktop using `%USERPROFILE%\.comet-native` and the detected Windows hostname.

- [ ] **Step 1: Stop only the desktop process launched from this worktree**

Resolve the current process by executable path before stopping it. Do not stop
an unrelated installed Comet process.

- [ ] **Step 2: Format and build the exact worktree binary**

Run:

```powershell
cargo fmt --all -- --check
cargo build -p comet --bin comet
```

Expected: formatting and build pass.

- [ ] **Step 3: Launch with a Windows-compatible home environment**

Until the separately observed `HOME` resolver bug is fixed, set child-process
`HOME=$env:USERPROFILE` and launch
`target\debug\comet.exe` from this worktree's Cargo target directory.

- [ ] **Step 4: Verify runtime status and visible behavior**

Run the same binary with `status` and confirm:

```text
Engine:   running
LAN:      disabled
```

Visually confirm the local device row displays `%COMPUTERNAME%`, the bottom-left
control is only gear + `Settings`, and it opens Settings > Devices.

- [ ] **Step 5: Record final verification**

Run `git status --short` and `git diff --check`. Expected: clean worktree and no
diff errors.
