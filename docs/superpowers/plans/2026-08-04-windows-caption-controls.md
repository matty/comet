# Windows Caption Controls Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add native-behaving minimize, maximize/restore, and close buttons to Comet's frameless Windows title bar without changing macOS or Linux behavior.

**Architecture:** Keep the existing app-owned title bar and add one Windows-only, absolute top-right overlay whose three buttons expose GPUI native `WindowControlArea` hit-test regions. Pure helpers own caption sequence, right-side clearance, and font selection so platform decisions are unit-testable without rendering a window; the existing chat and settings title-bar rows consume the clearance.

**Tech Stack:** Rust 2024, GPUI at the workspace-pinned Zed revision, `windows` 0.61 for `RtlGetVersion`, Cargo unit tests.

## Global Constraints

- Preserve the frameless transparent title bar on all platforms.
- Render Windows controls in this exact order: minimize, maximize or restore, close.
- Each Windows caption button is exactly 46 pixels wide and fills the existing 44-pixel title-bar height.
- Use `WindowControlArea::Min`, `WindowControlArea::Max`, and `WindowControlArea::Close`; do not replace them with application click handlers.
- Use Segoe Fluent Icons for Windows build 22000 or newer and Segoe MDL2 Assets for older Windows.
- Use glyphs `\u{e921}` minimize, `\u{e922}` maximize, `\u{e923}` restore, and `\u{e8bb}` close.
- Close hover is Windows red RGB (232, 17, 32) with white text; other controls use the current theme's glass hover and text colors.
- Hide the Windows cluster and its title-bar clearance in fullscreen.
- macOS traffic lights, spacer animation, and title-bar layout remain unchanged.
- Linux title-bar behavior remains unchanged.
- Do not add Linux client-side caption controls or change system-menu and resize-border behavior.

---

### Task 1: Windows caption controls and title-bar integration

**Files:**
- Modify: `Cargo.toml`
- Modify: `crates/ui/Cargo.toml`
- Modify: `crates/ui/src/shell.rs`
- Modify: `crates/ui/src/shell/tabs.rs`
- Test: `crates/ui/src/shell.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `Theme::TITLEBAR_HEIGHT`, `Theme::SPACE_LG`, `Theme::glass_hover`, `Window::is_maximized`, `WindowControlArea`, and the shell root's live `&mut Window`.
- Produces: `WINDOWS_CAPTION_BUTTON_WIDTH: f32`, `WINDOWS_CAPTION_CLUSTER_WIDTH: f32`, `windows_caption_clearance(is_windows: bool, fullscreen: bool) -> f32`, `windows_caption_buttons(is_maximized: bool) -> [WindowsCaptionButton; 3]`, `windows_caption_font_for_build(build: u32) -> &'static str`, and `Shell::render_windows_caption_controls(&self, window: &Window, cx: &App) -> Option<AnyElement>`.

- [ ] **Step 1: Write failing pure-behavior tests**

Append these cases to `crates/ui/src/shell.rs`'s existing test module. They name the production helpers that must make the tests pass:

```rust
#[test]
fn windows_caption_clearance_is_platform_and_fullscreen_aware() {
    assert_eq!(windows_caption_clearance(true, false), 46.0 * 3.0);
    assert_eq!(windows_caption_clearance(true, true), 0.0);
    assert_eq!(windows_caption_clearance(false, false), 0.0);
    assert_eq!(windows_caption_clearance(false, true), 0.0);
}

#[test]
fn windows_caption_sequence_tracks_maximized_state() {
    assert_eq!(
        windows_caption_buttons(false),
        [
            WindowsCaptionButton::Minimize,
            WindowsCaptionButton::Maximize,
            WindowsCaptionButton::Close,
        ]
    );
    assert_eq!(
        windows_caption_buttons(true),
        [
            WindowsCaptionButton::Minimize,
            WindowsCaptionButton::Restore,
            WindowsCaptionButton::Close,
        ]
    );
}

#[test]
fn windows_caption_font_changes_at_windows_11_build() {
    assert_eq!(windows_caption_font_for_build(21_999), "Segoe MDL2 Assets");
    assert_eq!(windows_caption_font_for_build(22_000), "Segoe Fluent Icons");
}
```

- [ ] **Step 2: Run the focused tests and verify RED**

Run:

```powershell
cargo test -p comet-ui shell::tests::windows_caption -- --nocapture
```

Expected: compilation fails because `windows_caption_clearance`, `windows_caption_buttons`, `WindowsCaptionButton`, and `windows_caption_font_for_build` do not exist.

- [ ] **Step 3: Add the pure caption model and layout helpers**

Near the existing traffic-light-aware title-bar helpers in `crates/ui/src/shell.rs`, add the constants, enum, and pure functions. Keep them available on every target so the unit tests exercise platform selection directly:

```rust
pub const WINDOWS_CAPTION_BUTTON_WIDTH: f32 = 46.0;
pub const WINDOWS_CAPTION_CLUSTER_WIDTH: f32 = WINDOWS_CAPTION_BUTTON_WIDTH * 3.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WindowsCaptionButton {
    Minimize,
    Maximize,
    Restore,
    Close,
}

pub fn windows_caption_clearance(is_windows: bool, fullscreen: bool) -> f32 {
    if is_windows && !fullscreen {
        WINDOWS_CAPTION_CLUSTER_WIDTH
    } else {
        0.0
    }
}

fn windows_caption_buttons(is_maximized: bool) -> [WindowsCaptionButton; 3] {
    [
        WindowsCaptionButton::Minimize,
        if is_maximized {
            WindowsCaptionButton::Restore
        } else {
            WindowsCaptionButton::Maximize
        },
        WindowsCaptionButton::Close,
    ]
}

fn windows_caption_font_for_build(build: u32) -> &'static str {
    if build >= 22_000 {
        "Segoe Fluent Icons"
    } else {
        "Segoe MDL2 Assets"
    }
}
```

- [ ] **Step 4: Run the focused tests and verify GREEN**

Run:

```powershell
cargo test -p comet-ui shell::tests::windows_caption -- --nocapture
```

Expected: all three focused tests pass.

- [ ] **Step 5: Add the Windows-only version dependency**

Add this workspace dependency to the root `Cargo.toml`:

```toml
[workspace.dependencies.windows]
version = "0.61"
features = ["Wdk_System_SystemServices"]
```

Add this target dependency to `crates/ui/Cargo.toml`:

```toml
[target.'cfg(target_os = "windows")'.dependencies]
windows.workspace = true
```

Do not expose the dependency to non-Windows builds.

- [ ] **Step 6: Implement the Windows caption glyphs and native hit-test regions**

Under `#[cfg(target_os = "windows")]`, add a build-detection helper following the pinned Zed `platform_title_bar` implementation:

```rust
fn windows_caption_font() -> &'static str {
    use windows::Wdk::System::SystemServices::RtlGetVersion;

    let mut version = unsafe { std::mem::zeroed() };
    let status = unsafe { RtlGetVersion(&mut version) };
    let build = if status.is_ok() {
        version.dwBuildNumber
    } else {
        0
    };
    windows_caption_font_for_build(build)
}
```

Give `WindowsCaptionButton` methods returning these exact mappings:

```rust
fn id(self) -> &'static str {
    match self {
        Self::Minimize => "windows-minimize",
        Self::Maximize => "windows-maximize",
        Self::Restore => "windows-restore",
        Self::Close => "windows-close",
    }
}

fn glyph(self) -> &'static str {
    match self {
        Self::Minimize => "\u{e921}",
        Self::Maximize => "\u{e922}",
        Self::Restore => "\u{e923}",
        Self::Close => "\u{e8bb}",
    }
}

fn control_area(self) -> WindowControlArea {
    match self {
        Self::Minimize => WindowControlArea::Min,
        Self::Maximize | Self::Restore => WindowControlArea::Max,
        Self::Close => WindowControlArea::Close,
    }
}
```

Implement a Windows-only button renderer and `Shell::render_windows_caption_controls`. Each button must be `46px` wide, `Theme::TITLEBAR_HEIGHT` high, centered, `10px` text, `.occlude()`, and marked with its native control area. Apply normal/active glass washes for minimize and maximize/restore. Apply RGB `(232/255, 17/255, 32/255)` with white text on close hover and an 80%-opacity version on active. The cluster must be `.absolute().top_0().right_0()`, horizontal, use `windows_caption_font()`, and return `None` whenever `self.fullscreen.unwrap_or(false)` is true.

Provide a non-Windows implementation with the same method signature that always returns `None`; it must not mention the `windows` crate.

- [ ] **Step 7: Reserve title-bar content space and mount the overlay**

Add a `Shell` helper:

```rust
fn windows_caption_clearance(&self) -> f32 {
    windows_caption_clearance(
        cfg!(target_os = "windows"),
        self.fullscreen.unwrap_or(false),
    )
}
```

In the settings title-bar row in `crates/ui/src/shell.rs`, replace the existing right padding with:

```rust
.pr(px(Theme::SPACE_LG + self.windows_caption_clearance()))
```

In the chat title-bar row in `crates/ui/src/shell/tabs.rs`, make the same replacement. In the ready-page shell root, mount the overlay after the left title-bar application-control cluster and before modal overlays:

```rust
.child(self.render_titlebar_cluster(cx))
.children(self.render_windows_caption_controls(window, cx))
.children(overlays)
```

Use `.children(Option<AnyElement>)` so non-Windows and fullscreen builds add no element.

- [ ] **Step 8: Format and verify the implementation**

Run:

```powershell
cargo fmt --all -- --check
cargo test -p comet-ui shell::tests::windows_caption -- --nocapture
cargo test -p comet-ui
cargo check -p comet-ui
```

Expected: formatting check succeeds; focused tests pass; all `comet-ui` tests pass; Windows `comet-ui` compilation succeeds without warnings introduced by this change.

- [ ] **Step 9: Review the diff for platform isolation**

Run:

```powershell
git diff --check
git diff -- Cargo.toml crates/ui/Cargo.toml crates/ui/src/shell.rs crates/ui/src/shell/tabs.rs
```

Confirm the `windows` API is referenced only inside `#[cfg(target_os = "windows")]`, the overlay is omitted in fullscreen, and no macOS traffic-light code changed.

- [ ] **Step 10: Commit the implementation**

Run:

```powershell
git add Cargo.toml Cargo.lock crates/ui/Cargo.toml crates/ui/src/shell.rs crates/ui/src/shell/tabs.rs
git commit -m "fix: add Windows caption controls"
```
