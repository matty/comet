# Windows Caption Controls Design

## Goal

Keep Comet's frameless desktop window while restoring the standard Windows minimize, maximize/restore, and close affordances. macOS must retain its existing inset traffic lights and title-bar layout. Linux behavior is outside this change and remains unchanged.

## Root Cause

`open_main_window` configures a transparent, app-owned title bar on all desktop platforms. On Windows this removes the system-drawn caption area. Comet's custom title bar currently replaces only the left-side application controls and relies on macOS traffic lights for window controls, so Windows has no replacement caption buttons.

## Design

Add a Windows-only caption-control cluster to the right edge of the existing unified title bar. The cluster contains minimize, maximize or restore depending on current window state, and close, in that order.

Each button uses GPUI's `WindowControlArea::Min`, `WindowControlArea::Max`, or `WindowControlArea::Close` instead of application click handlers. GPUI maps those regions to Windows non-client hit-test results, allowing Windows to perform the action and preserve native behavior such as maximize/restore transitions and the Windows 11 Snap Layouts affordance on the maximize button.

The controls use Windows caption glyphs from Segoe Fluent Icons on Windows 11 and Segoe MDL2 Assets on older Windows. Each button is 46 pixels wide and fills the existing 44-pixel title-bar height. Minimize and maximize/restore use the current theme's text and glass hover colors. Close uses the standard Windows red hover background with white foreground. The maximize glyph changes to restore whenever `Window::is_maximized()` is true.

The cluster is an absolute top-right overlay above the custom drag strip. Its button elements occlude the underlying strip, so caption clicks cannot also initiate a window drag or title-bar double-click. The cluster is omitted while fullscreen, matching the absence of caption controls in Windows fullscreen applications.

## Platform Isolation

Windows-specific rendering is gated with `cfg(target_os = "windows")`. The existing macOS traffic-light spacer, fullscreen inset animation, and app-owned title-bar behavior are not changed. Non-Windows builds receive no new font lookup or caption-control elements.

The title-bar content receives right-side clearance equal to the Windows control-cluster width when the cluster is visible. This prevents session tabs or the settings title from rendering underneath the caption buttons. macOS and Linux retain their current right padding.

## Components

- A small platform-selection/layout helper reports whether custom Windows caption controls should be present and how much right clearance they require.
- A Windows-only caption-cluster renderer selects maximize versus restore from live window state and emits the three native hit-test regions.
- The unified title bar applies the returned right clearance to both chat and settings variants.
- The shell root overlays the Windows caption cluster alongside the existing left title-bar application-control cluster.

## Behavior and Failure Handling

There is no asynchronous state or recoverable runtime error. The operating system owns caption actions after hit testing. If the preferred Windows 11 font is unavailable, the implementation selects the older Windows caption font based on the OS build, following the pinned GPUI/Zed reference implementation.

## Testing

Unit tests cover:

- Windows receives the full caption-cluster right clearance outside fullscreen.
- Windows receives no caption-cluster clearance in fullscreen.
- macOS and Linux receive no Windows caption-cluster clearance.
- maximized state selects the restore control while normal state selects maximize.

Run the focused `comet-ui` unit tests, the complete `comet-ui` test suite, formatting checks, and a Windows-target `cargo check` when the target toolchain is available. Existing macOS behavior remains protected by the current traffic-light and cluster-layout tests.

## Non-Goals

- Replacing the frameless window with a system title bar.
- Changing macOS traffic-light positioning or behavior.
- Adding Linux client-side window controls.
- Customizing Windows system-menu or resize-border behavior.
