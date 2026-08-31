//! The app shell (comet `__root.tsx`): sidebar column + main panel + optional
//! right "Changes" pane, plus the boot splash and the connection gate.
//!
//! Layout is comet's: collapsible drag-resizable sidebar (208–400px, default
//! 256) with a 200ms ease-out width transition; main panel with an h-11 header,
//! content outlet, and a reserved h-6 status strip so later content never
//! shifts; right pane scaffold (360px floor, default 520), hidden by default.
//! Widths/collapsed state persist to `ui-settings.json` (debounced).
//!
//! Resize handles use gpui's drag-and-drop pattern (an `on_drag` with an empty
//! ghost view + `on_drag_move::<Marker>` on the root), the same idiom as Zed's
//! dock. Double-clicking a handle resets that pane to its default width.

use std::path::PathBuf;
use std::time::Duration;

use chrono::Utc;
use gpui::{
    Action, AnyElement, App, Context, Empty, Entity, Focusable as _, IntoElement, KeyBinding,
    Keystroke, ModifiersChangedEvent, MouseButton, MouseDownEvent, MouseUpEvent, Pixels, Point,
    Render, SharedString, Subscription, Task, Window, WindowControlArea, actions, div, prelude::*,
    px,
};

use comet_proto::ServerId;
use comet_rpc::methods;
use gpui_tokio::Tokio;

use crate::changes::Changes;
use crate::composer::{Composer, ComposerEvent, ComposerInput, ComposerInputEvent};
use crate::icons::{self, icon};
use crate::loaders;
use crate::motion::{self, AnimationExt as _, MotionSpec, RESIZE, SPLASH_OUT};
use crate::popover::{self, Loadable};
use crate::rail;
use crate::remotes::RemoteConnectionsPage;
use crate::settings::accounts::AccountsPage;
use crate::settings::appearance::AppearancePage;
use crate::settings::archived::ArchivedPage;
use crate::settings::devices::DevicesPage;
use crate::settings::shortcuts::{ShortcutsEvent, ShortcutsPage};
use crate::settings::{
    JUMP_SLOTS, KeymapConfig, RIGHT_PANE_DEFAULT, RIGHT_PANE_MIN, SAVE_DEBOUNCE_MS,
    SIDEBAR_DEFAULT, SIDEBAR_MAX, SIDEBAR_MIN, ShortcutId, TERMINAL_DEFAULT_HEIGHT, UiSettings,
    jump_hints_visible, platform_combo,
};
use crate::state::{
    AppState, ConnectionStatus, EngineBootConfig, GatePhase, Indicator, SidebarScope,
    format_time_ago,
};
use crate::terminal::panel::{TerminalPanel, ToggleTerminal, clamp_terminal_height};
use crate::theme::Theme;
use crate::transcript::{self, Transcript};

mod search;
mod spaces;
mod tabs;

use spaces::{AddSpaceFlow, RenameSpaceDialog};

actions!(
    shell,
    [
        ToggleSidebar,
        ToggleChanges,
        AddSpacePalette,
        FocusSearch,
        NewSession,
        NextSession,
        PrevSession,
        ArchiveSession
    ]
);

/// Open the session at `slot` (zero-based) of the sidebar's active list. One
/// action carrying the slot, rather than nine near-identical action types.
#[derive(Clone, PartialEq, Action)]
#[action(namespace = shell, no_json)]
pub struct JumpSession(pub usize);

// ---------------------------------------------------------------------------
// Traffic-light-aware titlebar layout (feature-inventory §1.1)
// ---------------------------------------------------------------------------

/// Where the top-left window-control cluster starts, in px from the window's
/// left edge (comet window-controls.tsx: `left: fullscreen ? 12 : 88`). The
/// frameless hiddenInset chrome puts the macOS traffic lights at {14,15};
/// fullscreen hides them and the cluster reclaims the inset.
fn titlebar_cluster_start(fullscreen: bool) -> f32 {
    if fullscreen { 12.0 } else { 88.0 }
}

/// Width of the spacer ahead of the control cluster for a strip that already
/// carries `container_pad` px of its own left padding. macOS only — on
/// Linux/Windows there are no traffic lights and the cluster hugs the edge.
pub fn titlebar_spacer_width(is_macos: bool, fullscreen: bool, container_pad: f32) -> f32 {
    if !is_macos {
        return 0.0;
    }
    (titlebar_cluster_start(fullscreen) - container_pad).max(0.0)
}

/// Width of the persistent top-left button cluster itself (sidebar toggle +
/// back/forward: three 24px buttons, 2px gaps).
pub const CLUSTER_BUTTONS_WIDTH: f32 = 24.0 * 3.0 + 2.0 * 2.0;

/// Where the cluster's first button starts, from the window's left edge.
fn cluster_buttons_start(is_macos: bool, fullscreen: bool) -> f32 {
    if is_macos {
        titlebar_cluster_start(fullscreen)
    } else {
        10.0
    }
}

/// Left clearance a full-bleed header (collapsed sidebar) needs so its content
/// starts past the overlay cluster, given the header's own `container_pad`.
pub fn cluster_clearance(is_macos: bool, fullscreen: bool, container_pad: f32) -> f32 {
    (cluster_buttons_start(is_macos, fullscreen) + CLUSTER_BUTTONS_WIDTH + 8.0 - container_pad)
        .max(0.0)
}

pub const WINDOWS_CAPTION_BUTTON_WIDTH: f32 = 46.0;
pub const WINDOWS_CAPTION_CLUSTER_WIDTH: f32 = WINDOWS_CAPTION_BUTTON_WIDTH * 3.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(any(test, target_os = "windows")), allow(dead_code))]
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

#[cfg_attr(not(any(test, target_os = "windows")), allow(dead_code))]
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

#[cfg_attr(not(any(test, target_os = "windows")), allow(dead_code))]
fn windows_caption_font_for_build(build: u32) -> &'static str {
    if build >= 22_000 {
        "Segoe Fluent Icons"
    } else {
        "Segoe MDL2 Assets"
    }
}

/// Which identity-rebuild stamp, if any, still needs announcing.
///
/// Split out of the strip so the rule is testable without a window: `Some`
/// only when the engine reported a rebuild the user has not already dismissed.
/// Keyed on the stamp itself rather than a bool, so a SECOND rebuild — a rare
/// but real thing on a machine that crashes mid-write twice — announces again
/// instead of being swallowed by the first dismissal.
fn identity_notice_stamp<'a>(
    reported: Option<&'a str>,
    dismissed: Option<&str>,
) -> Option<&'a str> {
    let stamp = reported?;
    (dismissed != Some(stamp)).then_some(stamp)
}

/// (Re-)apply the whole app keymap: clears every binding, restores the composer
/// map, then binds the customizable shortcuts from `keymap` (feature-inventory
/// §1.4). Invalid persisted combos fall back to that shortcut's default.
fn valid_or_default(combo: &str, fallback: &str) -> String {
    let candidate = platform_combo(combo);
    match Keystroke::parse(&candidate) {
        Ok(parsed) if is_emittable_key(&parsed.key) => candidate,
        // Parsed, and names a key no keyboard will ever produce. Falling back
        // is what this function's name has always promised; before D95 it
        // returned the combo and the shortcut simply never fired.
        Ok(parsed) => {
            tracing::warn!(
                %combo,
                key = %parsed.key,
                "shortcut names a key no platform emits; using default"
            );
            platform_combo(fallback)
        }
        Err(_) => {
            tracing::warn!(%combo, "unparseable shortcut combo; using default");
            platform_combo(fallback)
        }
    }
}

/// Every named key gpui's platform layers actually emit, read off the pinned
/// fork rev (`ac135eb`) rather than guessed: `gpui_windows/src/events.rs`,
/// `gpui_macos/src/events.rs` and `gpui_linux/src/linux/platform.rs`. `menu`
/// and `back`/`forward` are Windows/Linux-only and stay listed anyway — a
/// keymap file is portable and binding one on macOS is a dead shortcut, not a
/// corrupt config.
///
/// **`Keystroke::parse` is not a validity check, which is what D95 was.** It
/// rejects an unknown *modifier* and nothing else, so `ctrl-nosuchkey`, `zzz`,
/// `ctrl-` and `""` all parse — the last two with an EMPTY key. A hand-edited
/// `ui-settings.json` therefore bound a shortcut that could never fire, with
/// no warning and no fallback, across nine `jumpSession` slots since #105.
const NAMED_KEYS: &[&str] = &[
    "backspace",
    "delete",
    "down",
    "end",
    "enter",
    "escape",
    "forward",
    "back",
    "home",
    "insert",
    "left",
    "menu",
    "pagedown",
    "pageup",
    "right",
    "space",
    "tab",
    "up",
];

/// The five names `Keystroke::parse` mints when a combo is a bare modifier
/// (`shift`, `ctrl`, …): it moves the modifier into the key position rather
/// than leaving the key empty, so these are real, bindable and must not be
/// rejected as unknown names.
const MODIFIER_KEYS: &[&str] = &["shift", "control", "alt", "platform", "function"];

/// Whether a parsed keystroke's key is one a platform can actually deliver.
///
/// **A single character is accepted without further question**, whatever it is:
/// `a`, `7`, `[`, `/` and a dead `-` are all legitimate on some layout, and
/// this guard is not in the business of deciding which keyboard someone owns.
/// What it catches is the shape a typo actually takes — a multi-character name
/// nothing emits, and the empty key two parseable combos produce.
fn is_emittable_key(key: &str) -> bool {
    if key.chars().count() == 1 {
        return true;
    }
    if NAMED_KEYS.contains(&key) || MODIFIER_KEYS.contains(&key) {
        return true;
    }
    // f1..f35, as a range rather than 35 literals — macOS emits the whole
    // span, Windows stops at f24, and a keymap naming f30 on Windows is a dead
    // shortcut rather than a corrupt file, exactly like `menu` on macOS.
    key.strip_prefix('f')
        .and_then(|number| number.parse::<u8>().ok())
        .is_some_and(|number| (1..=35).contains(&number))
}

fn shell_key_bindings(keymap: &KeymapConfig) -> Vec<KeyBinding> {
    let mut bindings = vec![
        KeyBinding::new(
            &valid_or_default(&keymap.toggle_sidebar, "mod-s"),
            ToggleSidebar,
            None,
        ),
        KeyBinding::new(
            &valid_or_default(&keymap.toggle_changes, "mod-b"),
            ToggleChanges,
            None,
        ),
        KeyBinding::new(
            &valid_or_default(&keymap.toggle_terminal, "mod-j"),
            ToggleTerminal,
            None,
        ),
        KeyBinding::new(
            &valid_or_default(&keymap.focus_search, "mod-p"),
            FocusSearch,
            None,
        ),
        KeyBinding::new(
            &valid_or_default(&keymap.new_session, "mod-n"),
            NewSession,
            None,
        ),
        KeyBinding::new(
            &valid_or_default(&keymap.next_session, "ctrl-tab"),
            NextSession,
            None,
        ),
        KeyBinding::new(
            &valid_or_default(&keymap.prev_session, "ctrl-shift-tab"),
            PrevSession,
            None,
        ),
        KeyBinding::new(
            &valid_or_default(&keymap.archive_session, "mod-shift-a"),
            ArchiveSession,
            None,
        ),
        // Fixed: ⌘K summons the add-space palette (the ⌘K chip in its search
        // bar); pressing it again dismisses.
        KeyBinding::new(&platform_combo("mod-k"), AddSpacePalette, None),
    ];
    // ⌘1..⌘9 open the sidebar's first nine rows. A slot left unbound (an empty
    // combo in a hand-edited file) binds nothing rather than falling back —
    // the user cleared it on purpose.
    bindings.extend((0..JUMP_SLOTS).filter_map(|slot| {
        let id = ShortcutId::JumpSession(slot);
        let combo = keymap.get(id);
        if combo.is_empty() {
            return None;
        }
        Some(KeyBinding::new(
            &valid_or_default(combo, id.default_combo()),
            JumpSession(slot),
            None,
        ))
    }));
    bindings
}

pub fn apply_keymap(cx: &mut App, keymap: &KeymapConfig) {
    cx.clear_key_bindings();
    crate::composer::init(cx);
    // Fixed app-level shortcuts (⌘Q quit, ⌘W close, ⌘M minimize, ⌘H hide) —
    // these back the native menu key equivalents and must survive keymap
    // re-application.
    crate::app_menus::bind_keys(cx);
    cx.bind_keys(shell_key_bindings(keymap));
}

/// The settings sections (feature-inventory §1.5 routes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsSection {
    Devices,
    RemoteConnections,
    Agents,
    Appearance,
    Shortcuts,
    Archived,
}

impl SettingsSection {
    pub const ALL: [SettingsSection; 6] = [
        SettingsSection::Devices,
        SettingsSection::RemoteConnections,
        SettingsSection::Agents,
        SettingsSection::Appearance,
        SettingsSection::Shortcuts,
        SettingsSection::Archived,
    ];

    /// Sidebar + header label (comet settings-sidebar.tsx SECTIONS / __root.tsx
    /// `settingsTitle` — the same strings in both places).
    pub fn label(self) -> &'static str {
        match self {
            SettingsSection::Devices => "Devices",
            SettingsSection::RemoteConnections => "Remote",
            SettingsSection::Agents => "Accounts",
            SettingsSection::Appearance => "Appearance",
            SettingsSection::Shortcuts => "Shortcuts",
            SettingsSection::Archived => "Archived sessions",
        }
    }
}

const BOTTOM_SETTINGS_SECTION: SettingsSection = SettingsSection::Devices;

/// What the main outlet shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Chat,
    Settings(SettingsSection),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShellFocusFallback {
    Composer,
    ShellRoot,
}

fn shell_focus_fallback(
    route: Route,
    has_focused_node: bool,
    shell_root_is_focused: bool,
) -> Option<ShellFocusFallback> {
    match route {
        Route::Chat if !has_focused_node || shell_root_is_focused => {
            Some(ShellFocusFallback::Composer)
        }
        Route::Chat => None,
        Route::Settings(_) if !has_focused_node => Some(ShellFocusFallback::ShellRoot),
        Route::Settings(_) => None,
    }
}

/// Per-chat panel open flags (comet parity: `sessionPanels` — the terminal and
/// changes panels open *per session*, in memory only; heights and every other
/// persisted setting stay global). New/unknown chats default to closed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ChatPanels {
    pub terminal_open: bool,
    pub changes_open: bool,
}

/// The session-scoped panel map. Keys are chat ids; the new-chat canvas uses
/// the empty key. Not persisted — a fresh app starts with everything closed.
#[derive(Debug, Default)]
pub struct SessionPanels {
    map: std::collections::HashMap<String, ChatPanels>,
}

impl SessionPanels {
    pub fn get(&self, key: &str) -> ChatPanels {
        self.map.get(key).copied().unwrap_or_default()
    }

    /// Flip the terminal flag for `key`; returns the new value.
    pub fn toggle_terminal(&mut self, key: &str) -> bool {
        let entry = self.map.entry(key.to_string()).or_default();
        entry.terminal_open = !entry.terminal_open;
        entry.terminal_open
    }

    /// Flip the changes flag for `key`; returns the new value.
    pub fn toggle_changes(&mut self, key: &str) -> bool {
        let entry = self.map.entry(key.to_string()).or_default();
        entry.changes_open = !entry.changes_open;
        entry.changes_open
    }
}

/// One route-history entry (comet parity: the renderer's TanStack memory
/// history — every route the user visited, browser-style).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavEntry {
    /// A chat route; the id of the selected chat ("" = the new-chat canvas).
    Chat(String),
    Settings(SettingsSection),
}

/// Browser-style navigation history for the titlebar back/forward buttons
/// (comet window-controls.tsx semantics): every route change pushes an entry;
/// Back/Forward walk the stack without changing it; pushing while behind the
/// tip truncates the entries ahead (a new branch, exactly like a browser).
#[derive(Debug)]
pub struct NavHistory {
    entries: Vec<NavEntry>,
    index: usize,
}

impl NavHistory {
    pub fn new(initial: NavEntry) -> Self {
        Self {
            entries: vec![initial],
            index: 0,
        }
    }

    pub fn current(&self) -> &NavEntry {
        &self.entries[self.index]
    }

    /// Record a route change. Re-navigating to the current route is a no-op
    /// (selecting the already-selected chat never happened as a navigation);
    /// otherwise any forward branch is truncated and the entry appended.
    pub fn push(&mut self, entry: NavEntry) {
        if *self.current() == entry {
            return;
        }
        self.entries.truncate(self.index + 1);
        self.entries.push(entry);
        self.index += 1;
    }

    /// Swap the current entry in place without growing the stack — the native
    /// equivalent of a `replace: true` navigation (comet's boot redirect from
    /// `/` into the last-used chat leaves no dead Back target behind).
    pub fn replace(&mut self, entry: NavEntry) {
        self.entries[self.index] = entry;
    }

    pub fn can_back(&self) -> bool {
        self.index > 0
    }

    /// Memory history keeps every entry, so "behind the last entry" is exactly
    /// "can go forward" (comet window-controls.tsx).
    pub fn can_forward(&self) -> bool {
        self.index + 1 < self.entries.len()
    }

    pub fn back(&mut self) -> Option<NavEntry> {
        if !self.can_back() {
            return None;
        }
        self.index -= 1;
        Some(self.current().clone())
    }

    pub fn forward(&mut self) -> Option<NavEntry> {
        if !self.can_forward() {
            return None;
        }
        self.index += 1;
        Some(self.current().clone())
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Sidebar resort glide (feature-inventory §1.6): 260ms
/// `cubic-bezier(0.22,1,0.36,1)` per-row translate, the View Transitions
/// equivalent.
pub const RESORT: MotionSpec = MotionSpec::new(260, motion::EASE_RESORT);

/// FLIP diff for a keyed list: given the previously rendered order and the new
/// order (key + row height), return each surviving key's paint-only start
/// offset `old_y - new_y` (only keys whose position actually moved). `gap` is
/// the flex gap between rows. Pure — drives the sidebar resort glide.
fn resort_offsets(
    old: &[(String, f32)],
    new: &[(String, f32)],
    gap: f32,
) -> std::collections::HashMap<String, f32> {
    let mut old_y = std::collections::HashMap::new();
    let mut y = 0.0_f32;
    for (key, height) in old {
        old_y.insert(key.as_str(), y);
        y += height + gap;
    }
    let mut offsets = std::collections::HashMap::new();
    let mut y = 0.0_f32;
    for (key, height) in new {
        if let Some(prev) = old_y.get(key.as_str()) {
            let dy = prev - y;
            if dy.abs() > 0.5 {
                offsets.insert(key.clone(), dy);
            }
        }
        y += height + gap;
    }
    offsets
}

/// What line 2 of a session row carries, which is also what decides its height.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum RowScope {
    /// Listing every space: a space line sits above the title.
    All {
        space: SharedString,
        device: SharedString,
        host_offline: bool,
    },
    /// Listing one space: the space line is not rendered.
    One,
}

/// Session-row vertical metrics. [`Shell::render_chat_row`] lays out with
/// exactly these and [`chat_row_height`] sums them, so the predicted height
/// and the painted one cannot drift apart.
///
/// Every line is given an EXPLICIT height. Line 2 (branch + harness mark) used
/// to take its height from its children and so collapsed to 13 with no branch
/// and to 0 with neither a branch nor a harness (a chat whose config frame
/// hasn't landed yet), making a mixed list 60/59/46 px tall against a
/// `chat_row_height` that always claimed 60 — every resort then glided the
/// survivors to the wrong y.
mod chat_row {
    pub(super) const PY: f32 = 5.0;
    /// Line 0, `RowScope::All` only: space + device, then the trailing group
    /// (time-ago, + throbber alongside it while Working) flush right. This is
    /// the row's TOP line in this scope, so the trailing group lives here —
    /// the 13px throbber fits inside this 14px line without stretching it.
    pub(super) const SPACE_LINE: f32 = 14.0;
    /// Gap under line 0.
    pub(super) const SPACE_LINE_MB: f32 = 2.0;
    /// Line 1: title, full width in `RowScope::All` (line 0 above carries the
    /// trailing group there). In `RowScope::One`, which renders no line 0,
    /// this IS the row's top line, so it keeps the trailing group instead —
    /// title + time-ago (+ throbber, alongside the time, while Working). No
    /// status rail — that was removed.
    pub(super) const TITLE_LINE: f32 = 18.0;
    /// Gap above line 2.
    pub(super) const META_LINE_MT: f32 = 2.0;
    /// Line 2: branch + harness mark. Fixed regardless of what it carries.
    pub(super) const META_LINE: f32 = 14.0;
}

/// Session-row height. Uniform *within* a scope, which is what the §1.6 resort
/// FLIP diff requires — `resort_offsets` must be fed the same value the render
/// pass used, or surviving rows glide to the wrong y on a scope switch.
pub(crate) fn chat_row_height(scope: &RowScope) -> f32 {
    let body = chat_row::TITLE_LINE + chat_row::META_LINE_MT + chat_row::META_LINE;
    let lead = match scope {
        RowScope::All { .. } => chat_row::SPACE_LINE + chat_row::SPACE_LINE_MB,
        RowScope::One => 0.0,
    };
    chat_row::PY + lead + body + chat_row::PY
}

/// Flex gap between sidebar list items.
const SIDEBAR_LIST_GAP: f32 = 2.0;

/// Ramp height of the glass sidebar's scroll-edge fade (the gpui
/// [`gpui::EdgeFade`] scope — per-primitive, so text fades per glyph).
const SIDEBAR_GLASS_FADE_BAND: f32 = 32.0;

/// Drag marker for the sidebar resize handle.
struct SidebarResize;
/// Drag marker for the right-pane resize handle.
struct RightPaneResize;
/// Drag marker for the terminal-panel height handle.
struct TerminalResize;

/// Invisible drag ghost — resize drags render nothing at the cursor.
struct DragGhost;

impl Render for DragGhost {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

/// A oneshot width tween (200ms ease-out), driven MANUALLY from render via
/// [`Shell::eval_tween`] — never through a `with_animation` wrapper. gpui keys
/// an animation element's start time by its full global element-id path, so a
/// wrapper that mounts/remounts (route swap, or an ancestor animation keyed by
/// a fresh epoch) silently REPLAYS the tween from t=0. Manual evaluation keeps
/// the element tree's shape constant: a finished or stale tween is exactly the
/// steady state, no matter how the tree around it remounts (round-6 §1–3).
#[derive(Debug, Clone, Copy)]
struct WidthTween {
    from: f32,
    to: f32,
    started: std::time::Instant,
}

impl WidthTween {
    fn new(from: f32, to: f32) -> Self {
        Self {
            from,
            to,
            started: std::time::Instant::now(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SplashPhase {
    Visible,
    FadingOut,
    Gone,
}

/// The chat-row Rename dialog.
struct RenameChatDialog {
    chat_id: comet_proto::ServerRef,
    input: Entity<ComposerInput>,
    /// Focus the input on the dialog's first paint (opened without window access).
    focus_pending: bool,
    _events: Subscription,
}

/// In-app update lifecycle (macOS bundle installs; see `render_update_strip`).
enum UpdateFlow {
    Idle,
    Downloading,
    /// Staged bundle ready to swap in — one click restarts into it.
    Ready(PathBuf),
    /// Carries no message on purpose. The failure is an anyhow chain from
    /// `comet_update` ("staging failed: io error: Access is denied. (os error
    /// 5)"), and this renders in a narrow sidebar chip — it was neither
    /// readable there nor useful to a user. The chain is logged at the point
    /// of failure instead; the chip offers the retry, which is the only action
    /// available either way.
    Failed,
}

pub struct Shell {
    state: Entity<AppState>,
    transcript: Entity<Transcript>,
    composer: Entity<Composer>,
    /// Blocked-turn clock: `(key, first seen)` for the approval or tool call
    /// the status strip is reporting. UI-local by construction — no part
    /// carries a start timestamp, so a restart mid-wait under-reports rather
    /// than inventing a start this client never observed.
    blocked_stamp: Option<(SharedString, std::time::Instant)>,
    /// The pinned sidebar search field ("SidebarSearch" context: navigation
    /// keys stay unbound so ↑↓/⏎ bubble to the sidebar frame). Search is
    /// transient — never persisted, never restored on boot.
    search_input: Entity<ComposerInput>,
    /// Keyboard highlight into the flat (spaces-then-sessions) results list —
    /// reset to 0 on every edit ([`search::highlight_target`] maps it to the
    /// row it actually selects).
    search_active: usize,
    /// `true` for the one frame after a search result is opened; the render
    /// pass consumes it to hand focus back to the composer (the dialogs'
    /// `focus_pending` idiom — these handlers run without window access).
    composer_focus_pending: bool,
    /// External file drag hovering the conversation column — shows the
    /// "Drop images to attach" veil over the whole chat area; a drop stages
    /// the files in the composer.
    file_drag_active: bool,
    /// Lazy panes: no entity (and no RPC) until first opened.
    terminal: Option<Entity<TerminalPanel>>,
    changes: Option<Entity<Changes>>,
    /// Chat outlet vs settings pages.
    route: Route,
    /// Route history behind the titlebar back/forward buttons (§ nav history).
    nav: NavHistory,
    devices_page: Option<Entity<DevicesPage>>,
    remote_connections_page: Option<Entity<RemoteConnectionsPage>>,
    archived_page: Option<Entity<ArchivedPage>>,
    appearance_page: Option<Entity<AppearancePage>>,
    shortcuts_page: Option<Entity<ShortcutsPage>>,
    accounts_page: Option<Entity<AccountsPage>>,
    shortcuts_sub: Option<Subscription>,
    /// Session-row context menu: (chat id, window position).
    chat_menu: Option<(comet_proto::ServerRef, Point<Pixels>)>,
    rename_dialog: Option<RenameChatDialog>,
    /// Chat id awaiting delete confirmation.
    delete_confirm: Option<comet_proto::ServerRef>,
    /// Space-row context menu: (space id, window position).
    space_menu: Option<(String, Point<Pixels>)>,
    rename_space_dialog: Option<RenameSpaceDialog>,
    /// Space id awaiting delete confirmation (hard delete + session cascade).
    delete_space_confirm: Option<String>,
    /// The add-space palette (⌘K-style; device tabs + folder search), `Some`
    /// while open.
    add_space: Option<AddSpaceFlow>,
    /// Last selected chat per space (in-memory, like [`SessionPanels`]) — a
    /// space switch lands back on the tab you left.
    space_last_chat: std::collections::HashMap<String, String>,
    /// Session tab currently hovered (close button appears on hover).
    tab_hover: Option<String>,
    /// Session-tab drag-reorder in flight (see `tabs::TabDragState`).
    tab_drag: Option<tabs::TabDragState>,
    /// Space-row drag-reorder in flight (see `spaces::SpaceDragState`).
    space_drag: Option<spaces::SpaceDragState>,
    /// Scroll position of the session tab region (drives the edge fades and
    /// the drop-index math under horizontal overflow).
    tabs_scroll: gpui::ScrollHandle,
    /// Chat id last auto-scrolled into view — scroll-to-selected fires once per
    /// selection change, not every frame (which would fight manual scrolling).
    tabs_scrolled_to: Option<String>,
    /// Scroll position of the sidebar lists region (drives its edge fades).
    sidebar_scroll: gpui::ScrollHandle,
    /// `settings.last_space_id` applied once after the first spaces frame.
    space_boot_applied: bool,
    /// Last seen session status per chat — the chime trigger compares against
    /// it (a row's FIRST appearance never chimes, so boot stays silent).
    sound_prev: std::collections::HashMap<String, comet_proto::SessionStatus>,
    /// Inline sidebar error strip (mutation failures); click dismisses.
    sidebar_notice: Option<SharedString>,
    /// Design prototype: `COMET_SLOW_TOAST_DEMO` pins the slow-request toast
    /// open so it can be reviewed in the app without waiting for something to
    /// actually hang. `=1` shows the cancellable card, `=quiet` the one a
    /// background revalidation raises — which has no Cancel, so its only way out
    /// is to unset the variable. Not a shipping code path.
    ///
    /// `Some(cancellable)`.
    slow_request_demo: Option<bool>,
    /// `Some` while the space-scope dropdown panel is open: `Switch` when
    /// the trigger itself was clicked (picking what the sidebar is scoped
    /// to), `PickForNewSession` when the Sessions `+` on `All spaces` is
    /// asking which space a new session should land in
    /// (`NewSessionTarget::Pick`).
    space_dropdown_open: Option<DropdownMode>,
    /// Keyboard highlight within the open panel's NAVIGABLE rows — an index
    /// into that reachable subsequence, not the raw item list (offline rows
    /// in `PickForNewSession` are skipped by both mouse and keyboard).
    space_dropdown_highlight: Option<usize>,
    /// Puts the open panel on the keyboard dispatch path (`AddSpaceFlow`'s
    /// `track_focus` idiom) so ↑↓/Enter/Esc reach it instead of whatever was
    /// focused before it opened.
    space_dropdown_focus: gpui::FocusHandle,
    /// Shell-level keyboard shortcuts need a focus-chain target when Settings
    /// has no focused control.
    shell_focus: gpui::FocusHandle,
    /// `true` for the one frame after the panel opens — the render pass
    /// consumes it to call `window.focus`.
    space_dropdown_focus_pending: bool,
    /// Outside-click dismissal instant — suppresses the trigger click that
    /// follows the same mouse-down from instantly reopening the panel
    /// (`settings::accounts`'s `device_menu_dismissed_at` idiom: the panel's
    /// `on_mouse_down_out` is a capture-phase mouse-DOWN listener, so a click
    /// on the trigger itself fires it first, then the trigger's own
    /// mouse-up `on_click` toggles the (now-closed) state straight back
    /// open).
    space_dropdown_dismissed_at: Option<std::time::Instant>,
    /// Scroll position of the dropdown panel's space-row region (capped +
    /// scrollable since the round-1 height cap). Both the drag-reorder
    /// math and keyboard scroll-into-view need this — `AddSpaceFlow`'s
    /// `list_scroll` is the precedent.
    space_panel_scroll: gpui::ScrollHandle,
    /// Local lifecycle of an in-app update (macOS bundle swap) — the engine's
    /// UpdateStatus stream says WHETHER one exists; this says how far the
    /// download/stage of it has come in this process.
    update_flow: UpdateFlow,
    update_task: Option<Task<()>>,
    /// Version whose update strip the user dismissed (advisory installs only —
    /// a newer release shows the strip again).
    update_dismissed: Option<String>,
    /// How this binary was installed — decides the strip's click behavior.
    /// Cached: `detect_install` stats `current_exe` and this renders per frame.
    install: comet_update::InstallKind,
    mutate_task: Option<Task<()>>,
    /// Kept for the failed-gate "Retry" action.
    boot: EngineBootConfig,
    data_dir: PathBuf,
    settings: UiSettings,
    /// Session-scoped panel open flags (terminal / changes per chat; §1.10-1.11
    /// parity — heights stay in [`UiSettings`]).
    panels: SessionPanels,
    /// Authoritative owner of the raw-id shell caches below. Switching servers
    /// clears them before an equal local id can inherit another server's UI.
    transient_server: Option<ServerId>,
    /// The panel key of the chat currently shown ("" = new-chat canvas).
    active_chat: String,
    /// Last rendered sidebar order (key + estimated height) — the FLIP baseline
    /// for the §1.6 resort glide.
    sidebar_prev_order: Vec<(String, f32)>,
    /// Per-key paint offsets of the resort in flight, keyed elements restart on
    /// `resort_epoch` bumps.
    sidebar_resort: std::collections::HashMap<String, f32>,
    /// Keys that just appeared in a live list (fade in, no glide).
    sidebar_new_keys: std::collections::HashSet<String>,
    resort_epoch: usize,
    /// Dev/testing knobs (`COMET_OPEN_DIALOG`, `COMET_FORCE_GATE`) — see
    /// [`Shell::new`].
    debug_dialog: Option<String>,
    debug_gate: Option<GatePhase>,
    sidebar_tween: Option<WidthTween>,
    right_tween: Option<WidthTween>,
    /// Latest viewport width, used to keep a persisted right-pane width inside
    /// the physical space available after the left sidebar.
    viewport_width: f32,
    terminal_tween: Option<WidthTween>,
    /// Last observed `window.is_fullscreen()` (`None` before first paint) —
    /// flips key the traffic-light inset tween.
    fullscreen: Option<bool>,
    /// 200ms ease-out tween of the cluster start on fullscreen toggles.
    titlebar_tween: Option<WidthTween>,
    /// Armed by mouse-down on a titlebar strip; the next mouse-move hands the
    /// drag to the compositor (zed's platform-titlebar pattern).
    titlebar_should_move: bool,
    /// Clears the height tween once it completes (so a closed panel unmounts).
    terminal_tween_task: Option<Task<()>>,
    /// Height-drag anchor: (pointer y, height) at mouse-down on the handle.
    terminal_drag_anchor: Option<(f32, f32)>,
    /// `motion::reduced_motion` snapshot, refreshed at the top of each render
    /// pass so [`Shell::eval_tween`] (called from `&self` render helpers) can
    /// snap without a `cx`.
    reduced_motion: bool,
    /// Set by [`Shell::eval_tween`] when any tween is mid-flight this frame;
    /// render schedules the next animation frame off it.
    motion_active: std::cell::Cell<bool>,
    splash: SplashPhase,
    splash_task: Option<Task<()>>,
    save_task: Option<Task<()>>,
    /// Focus fallback (registered on first paint — [`Shell::new`] has no
    /// window): keyboard shortcuts dispatch through the window focus chain, so
    /// with nothing focused they go dead. Initial focus lands on the composer
    /// and focus lost with no successor routes back there.
    focus_sub: Option<Subscription>,
    /// Clears the jump hints when the window deactivates: a Cmd+Tab away
    /// swallows the key-up, so without this the chips stay on screen for good.
    activation_sub: Option<Subscription>,
    /// The jump-hint overlay: true while the held modifiers exactly match a
    /// jump shortcut, which swaps the first nine sidebar rows' time-ago for
    /// their key-cap chip (t3code's `showJumpHints`). Frame-transient — window
    /// deactivation clears it, so a chip cannot stick after an app switch
    /// swallows the key-up.
    pub(super) jump_hints: bool,
    /// 1s heartbeat re-rendering the working indicator (elapsed + flavour word).
    _ticker: Task<()>,
    _state_observation: Subscription,
    _composer_events: Subscription,
    _search_events: Subscription,
}

/// The space-scope dropdown panel's mode (`Shell::space_dropdown_open`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DropdownMode {
    /// Clicking the trigger: pick a scope.
    Switch,
    /// Clicking the Sessions `+` on `All spaces`: pick where the new session
    /// goes.
    PickForNewSession,
}

/// Where a sidebar-initiated new session should go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NewSessionTarget {
    /// Create here, no prompt.
    Space(String),
    /// Ask — a session belongs to exactly one space and `All spaces` is not one.
    Pick(Vec<String>),
    /// Nowhere to put it yet.
    AddSpaceFirst,
}

/// `spaces` is the space ids in display order.
///
/// Host reachability is deliberately NOT an input: an offline space is still
/// a legitimate destination (the picker lists it, dimmed, and
/// `dropdown_navigable_positions` decides what can be committed there), and
/// with every host offline the answer is still the picker rather than a
/// silent no-op. The parameter used to carry an `online` flag that nothing
/// here read.
pub(crate) fn new_session_target(scope: &SidebarScope, spaces: &[String]) -> NewSessionTarget {
    if let Some(id) = scope.space_id() {
        return NewSessionTarget::Space(id.to_string());
    }
    match spaces {
        [] => NewSessionTarget::AddSpaceFirst,
        [only] => NewSessionTarget::Space(only.clone()),
        many => NewSessionTarget::Pick(many.to_vec()),
    }
}

/// History entry for a global New Session action that leaves Settings.
///
/// A direct target ends on the blank canvas immediately. Picker and add-space
/// flows still show the previously active chat behind their cancellable UI, so
/// cancelling must leave history pointing at that visible Chat route. Chat-
/// origin actions need no entry: their selection changes already drive the
/// normal history observer.
fn new_session_nav_entry(
    origin: Route,
    target: &NewSessionTarget,
    active_chat: &str,
) -> Option<NavEntry> {
    if !matches!(origin, Route::Settings(_)) {
        return None;
    }
    Some(match target {
        NewSessionTarget::Space(_) => NavEntry::Chat(String::new()),
        NewSessionTarget::Pick(_) | NewSessionTarget::AddSpaceFirst => {
            NavEntry::Chat(active_chat.to_string())
        }
    })
}

impl Shell {
    pub fn new(state: Entity<AppState>, boot: EngineBootConfig, cx: &mut Context<Self>) -> Self {
        let observation = cx.observe(&state, |this: &mut Shell, state, cx| {
            this.on_state_changed(&state, cx);
            cx.notify();
        });
        let transcript = cx.new(|cx| Transcript::new(state.clone(), cx));
        let composer = cx.new(|cx| Composer::new(state.clone(), cx));
        // Own-send re-engages the stick-to-bottom pin with a smooth scroll.
        let composer_events = cx.subscribe(&composer, {
            let transcript = transcript.clone();
            move |_this: &mut Shell, _, event: &ComposerEvent, cx| match event {
                ComposerEvent::Sent { .. } => {
                    transcript.update(cx, |t, cx| t.on_own_send(cx));
                }
            }
        });
        // Working-indicator heartbeat: notify once a second while a session is
        // live so elapsed time and the flavour word stay fresh.
        let ticker = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let alive = this.update(cx, |shell: &mut Shell, cx| {
                    let live = {
                        let s = shell.state.read(cx);
                        s.selected_chat_id()
                            .is_some_and(|id| s.indicator_for(id, Utc::now()) != Indicator::None)
                    };
                    if live {
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        });
        // "SidebarSearch" context: navigation keys stay unbound so ↑↓/⏎ bubble
        // to the sidebar frame instead of moving the caret — same reason the
        // add-space palette uses its own context.
        let search_input = cx.new(|cx| ComposerInput::with_context("Search", "SidebarSearch", cx));
        let search_events = cx.subscribe(&search_input, |this: &mut Shell, _, event, cx| {
            if matches!(event, ComposerInputEvent::Edited) {
                this.search_active = 0;
                // Results mode swaps out `render_spaces_section` wholesale, so
                // the first character typed UNMOUNTS the scope trigger and its
                // panel. An unmounted panel cannot close itself; leaving the
                // flag set reopens it — mode and all — the moment the query is
                // cleared. Closing at the gate covers every way text can arrive
                // (⌘P then typing, paste, an IME commit).
                this.close_space_dropdown();
                cx.notify();
            }
        });
        let data_dir = boot.data_dir.clone();
        let settings = UiSettings::load(&data_dir);
        // Bind the customizable shortcuts from the persisted keymap.
        apply_keymap(cx, &settings.keymap);
        // Dev/testing knob: `COMET_OPEN_ROUTE=settings[/<section>]` boots
        // straight into a settings section — these pages have no deep link and
        // synthetic input can't reach them on headless compositors.
        let route = match std::env::var("COMET_OPEN_ROUTE").ok().as_deref() {
            Some("settings") | Some("settings/devices") => {
                Route::Settings(SettingsSection::Devices)
            }
            Some("settings/agents") => Route::Settings(SettingsSection::Agents),
            Some("settings/remotes") => Route::Settings(SettingsSection::RemoteConnections),
            Some("settings/appearance") => Route::Settings(SettingsSection::Appearance),
            Some("settings/shortcuts") => Route::Settings(SettingsSection::Shortcuts),
            Some("settings/archived") => Route::Settings(SettingsSection::Archived),
            // `new` pins the new-chat canvas (suppresses boot auto-select).
            Some("new") => {
                state.update(cx, |s, _| s.auto_selected = true);
                Route::Chat
            }
            _ => Route::Chat,
        };
        // More capture knobs of the same kind: `COMET_OPEN_DIALOG=rename|delete`
        // opens that dialog for the first chat once chats land; `=model` pops
        // the combined harness/model menu once the shell is Ready;
        // `COMET_FORCE_GATE=failed` renders that gate regardless of connection
        // state (display-only — for styling passes).
        let debug_dialog = std::env::var("COMET_OPEN_DIALOG").ok();
        let debug_gate = match std::env::var("COMET_FORCE_GATE").ok().as_deref() {
            Some("failed") => Some(GatePhase::Failed(
                "Could not reach the comet engine on port 27901".into(),
            )),
            _ => None,
        };
        let nav = NavHistory::new(match route {
            Route::Chat => NavEntry::Chat(String::new()),
            Route::Settings(section) => NavEntry::Settings(section),
        });
        Self {
            state,
            transcript,
            composer,
            blocked_stamp: None,
            search_input,
            search_active: 0,
            composer_focus_pending: false,
            file_drag_active: false,
            terminal: None,
            changes: None,
            route,
            nav,
            devices_page: None,
            remote_connections_page: None,
            archived_page: None,
            appearance_page: None,
            shortcuts_page: None,
            accounts_page: None,
            shortcuts_sub: None,
            chat_menu: None,
            rename_dialog: None,
            delete_confirm: None,
            space_menu: None,
            rename_space_dialog: None,
            delete_space_confirm: None,
            add_space: None,
            space_last_chat: std::collections::HashMap::new(),
            tab_hover: None,
            tab_drag: None,
            space_drag: None,
            tabs_scroll: gpui::ScrollHandle::new(),
            tabs_scrolled_to: None,
            sidebar_scroll: gpui::ScrollHandle::new(),
            space_boot_applied: false,
            sound_prev: std::collections::HashMap::new(),
            sidebar_notice: None,
            slow_request_demo: match std::env::var("COMET_SLOW_TOAST_DEMO").ok().as_deref() {
                None | Some("") => None,
                Some("quiet") => Some(false),
                Some(_) => Some(true),
            },
            space_dropdown_open: None,
            space_dropdown_highlight: None,
            space_dropdown_focus: cx.focus_handle(),
            shell_focus: cx.focus_handle(),
            space_dropdown_focus_pending: false,
            space_dropdown_dismissed_at: None,
            space_panel_scroll: gpui::ScrollHandle::new(),
            update_flow: UpdateFlow::Idle,
            update_task: None,
            update_dismissed: None,
            install: comet_update::detect_install(),
            mutate_task: None,
            boot,
            data_dir,
            settings,
            panels: SessionPanels::default(),
            transient_server: None,
            active_chat: String::new(),
            sidebar_prev_order: Vec::new(),
            sidebar_resort: std::collections::HashMap::new(),
            sidebar_new_keys: std::collections::HashSet::new(),
            resort_epoch: 0,
            debug_dialog,
            debug_gate,
            sidebar_tween: None,
            right_tween: None,
            viewport_width: 0.0,
            terminal_tween: None,
            fullscreen: None,
            titlebar_tween: None,
            titlebar_should_move: false,
            terminal_tween_task: None,
            terminal_drag_anchor: None,
            reduced_motion: false,
            motion_active: std::cell::Cell::new(false),
            splash: SplashPhase::Visible,
            splash_task: None,
            save_task: None,
            focus_sub: None,
            activation_sub: None,
            jump_hints: false,
            _ticker: ticker,
            _state_observation: observation,
            _composer_events: composer_events,
            _search_events: search_events,
        }
    }

    // ---- splash ----

    fn on_state_changed(&mut self, state: &Entity<AppState>, cx: &mut Context<Self>) {
        let selected_server = state.read(cx).selected_server_id().cloned();
        if selected_server != self.transient_server {
            self.transient_server = selected_server;
            self.chat_menu = None;
            self.rename_dialog = None;
            self.delete_confirm = None;
            self.space_menu = None;
            self.rename_space_dialog = None;
            self.delete_space_confirm = None;
            self.add_space = None;
            self.space_last_chat.clear();
            self.tab_hover = None;
            self.tab_drag = None;
            self.space_drag = None;
            self.tabs_scrolled_to = None;
            self.sound_prev.clear();
            self.panels = SessionPanels::default();
            self.active_chat.clear();
            self.nav = NavHistory::new(NavEntry::Chat(String::new()));
            // The query is transient and scoped to whatever server bucket it
            // was typed against — a raw chat/space id from the old bucket
            // could otherwise collide with an unrelated row on the new one.
            self.search_input
                .update(cx, |input, cx| input.set_text("", cx));
            self.search_active = 0;
        }
        // Capture knob: the add-space palette needs only the device registry.
        if self.debug_dialog.as_deref() == Some("add-space") && !state.read(cx).devices.is_empty() {
            self.debug_dialog = None;
            self.open_add_space(cx);
        }
        // Capture knob: pop the requested dialog once chats have landed.
        if let Some(which) = self.debug_dialog.clone()
            && let Some(first) = state.read(cx).chats.first().map(|c| c.id.clone())
        {
            let owner = comet_proto::ServerRef::new(
                state
                    .read(cx)
                    .selected_server_id()
                    .cloned()
                    .expect("chat rows require a server bucket"),
                first,
            );
            self.debug_dialog = None;
            match which.as_str() {
                "rename" => self.open_rename_chat(owner, cx),
                "delete" => {
                    self.delete_confirm = Some(owner);
                }
                _ => {}
            }
        }
        // Session chimes (herdr semantics, `sound::sound_for_transition`): a
        // question rings whenever a session flips to AwaitingInput, a
        // completion rings on the Working→Idle edge — for ANY session on any
        // device. A row's first appearance only seeds the baseline, so boot
        // (restored rows) and fresh sends stay silent.
        //
        // STALENESS-GATED like the dot (`effective_indicator`), for the same
        // reason: raw row statuses include the past. A dead turn's Working row
        // (host killed mid-run, Idle write lost to a wedged room) seeded
        // prev=Working here, and the moment the old Idle finally synced in —
        // typically piggybacked on the round-trip of a fresh send — the chime
        // heard a phantom Working→Idle and rang "done" on send (user report
        // 2026-07-31). The dot never showed that ghost; the chime must judge
        // by the identical clock.
        {
            let now = Utc::now();
            let sessions: Vec<(String, comet_proto::SessionStatus)> = state
                .read(cx)
                .sessions
                .iter()
                .map(|s| {
                    use comet_proto::view::Indicator;
                    let status = match comet_proto::view::effective_indicator(Some(s), now) {
                        Indicator::Working => comet_proto::SessionStatus::Working,
                        Indicator::AwaitingInput => comet_proto::SessionStatus::AwaitingInput,
                        Indicator::Errored => comet_proto::SessionStatus::Errored,
                        Indicator::None => comet_proto::SessionStatus::Idle,
                    };
                    (s.chat_id.clone(), status)
                })
                .collect();
            for (chat_id, status) in sessions {
                let prev = self.sound_prev.insert(chat_id, status);
                if let Some(prev) = prev
                    && self.settings.sound_enabled
                    && let Some(sound) = crate::sound::sound_for_transition(prev, status)
                {
                    crate::sound::play(sound);
                }
            }
        }
        // Boot: restore the last selected space once the first spaces frame
        // lands (a still-existing row wins over the auto-selected first one;
        // the boot-auto-selected chat's own space wins over both — selecting a
        // chat implies its space, which `select_chat` already applied).
        if !self.space_boot_applied && !state.read(cx).spaces.is_empty() {
            self.space_boot_applied = true;
            if state.read(cx).selected_chat.is_none()
                && let Some(last) = self.settings.last_space_id.clone()
                && state.read(cx).space_row(&last).is_some()
            {
                state.update(cx, |s, cx| s.select_space(Some(last), cx));
            }
            // Restore the sidebar scope. A scope naming a space that no longer
            // exists silently becomes All — `heal_sidebar_scope` would do it on
            // the next frame anyway, but doing it here avoids one wrong render.
            if let Some(scoped) = self.settings.sidebar_scope_space.clone() {
                state.update(cx, |s, _| {
                    s.sidebar_scope = if s.space_row(&scoped).is_some() {
                        crate::state::SidebarScope::Space(scoped)
                    } else {
                        crate::state::SidebarScope::All
                    };
                });
            }
        }
        // The persisted scope mirrors the live one. `activate_space` and
        // `activate_all_spaces` write it on the way in, but `heal_sidebar_scope`
        // resets the scope behind their backs whenever the projection drops the
        // scoped space (deleted elsewhere, or a server switch replacing
        // `spaces` wholesale) — without this the healed-away scope came back
        // on the next launch. Gated on the boot restore having run, or this
        // would erase the stored scope before it is applied.
        if self.space_boot_applied {
            let live = state.read(cx).sidebar_scope.space_id().map(str::to_string);
            if live != self.settings.sidebar_scope_space {
                self.settings.sidebar_scope_space = live;
                self.schedule_save(cx);
            }
        }
        // Track the per-space last chat + persist the selected space.
        {
            let (selected_space, selected_chat, chat_space) = {
                let s = state.read(cx);
                let chat_space = s.selected_chat_row().and_then(|c| c.space_id.clone());
                (
                    s.selected_space.clone().map(|id| id.local_id),
                    s.selected_chat.clone().map(|id| id.local_id),
                    chat_space,
                )
            };
            if let (Some(space), Some(chat)) = (chat_space, selected_chat) {
                self.space_last_chat.insert(space, chat);
            }
            if selected_space != self.settings.last_space_id && selected_space.is_some() {
                self.settings.last_space_id = selected_space;
                self.schedule_save(cx);
            }
        }
        // Chat switch: restore THAT chat's panel state (per-session open flags;
        // snap, no tween — the panels belong to the destination chat).
        let selected = state
            .read(cx)
            .selected_chat_id()
            .unwrap_or_default()
            .to_string();
        if selected != self.active_chat {
            self.active_chat = selected;
            // Route history: a chat switch is a navigation. The very first
            // selection off the untouched boot canvas REPLACES that entry —
            // comet's `/` route redirected into the last-used chat, leaving no
            // dead Back target. Walking history lands here too, but the
            // destination already equals `current()`, so the push dedups.
            if matches!(self.route, Route::Chat) {
                let entry = NavEntry::Chat(self.active_chat.clone());
                if self.nav.len() == 1 && *self.nav.current() == NavEntry::Chat(String::new()) {
                    self.nav.replace(entry);
                } else {
                    self.nav.push(entry);
                }
            }
            self.right_tween = None;
            self.terminal_tween = None;
            // The blocked-turn clock is keyed only on the approval/tool id, not
            // on the chat it belongs to — a stamp surviving the switch would be
            // the clock's only OVER-report path (a same-keyed wait on the new
            // chat inheriting a start time from the old one) in a design that
            // otherwise only ever under-reports. Theoretical today (approval
            // ids are UUIDs; tool keys are provider call ids), but free to close.
            self.blocked_stamp = None;
            let panels = self.panels.get(&self.panel_key(cx));
            if let Some(panel) = self.terminal.clone() {
                panel.update(cx, |panel, cx| panel.set_open(panels.terminal_open, cx));
            }
            if panels.changes_open {
                let changes = self.changes_pane(cx);
                changes.update(cx, |changes, cx| changes.ensure_watch(cx));
            }
        }
        match state.read(cx).connection {
            ConnectionStatus::Ready => {
                if self.splash == SplashPhase::Visible {
                    self.splash = SplashPhase::FadingOut;
                    self.splash_task = Some(cx.spawn(async move |this, cx| {
                        cx.background_executor()
                            .timer(SPLASH_OUT.total() + Duration::from_millis(30))
                            .await;
                        this.update(cx, |shell, cx| {
                            shell.splash = SplashPhase::Gone;
                            cx.notify();
                        })
                        .ok();
                    }));
                }
            }
            // Reveal the gate card immediately; the splash never returns mid-session.
            ConnectionStatus::Failed(_) => self.splash = SplashPhase::Gone,
            ConnectionStatus::Connecting => {}
        }
    }

    // ---- layout state ----

    fn sidebar_target(&self) -> f32 {
        if self.settings.sidebar_collapsed {
            0.0
        } else {
            self.settings.sidebar_width
        }
    }

    /// Does the selected space's folder have git? Owner-stamped and synced —
    /// gates the Changes pane, its toggle, and Cmd-B with zero RPCs.
    fn space_git_detected(&self, cx: &App) -> bool {
        self.state.read(cx).selected_space_git()
    }

    /// The current chat's changes-pane flag (per-session, in-memory), gated on
    /// the space having git at all: a stale per-chat open flag must not reopen
    /// the pane after switching into a non-git space.
    /// The per-session panel key. The new-chat canvas (no selection) keys per
    /// SPACE — one shared "" key made a canvas toggle read as global state
    /// (user report).
    fn panel_key(&self, cx: &App) -> String {
        if self.active_chat.is_empty() {
            let space = self
                .state
                .read(cx)
                .selected_space_id()
                .map(str::to_string)
                .unwrap_or_default();
            format!("space-canvas:{space}")
        } else {
            self.active_chat.clone()
        }
    }

    fn right_pane_open(&self, cx: &App) -> bool {
        self.panels.get(&self.panel_key(cx)).changes_open && self.space_git_detected(cx)
    }

    /// The current chat's terminal flag (per-session, in-memory).
    fn terminal_open(&self, cx: &App) -> bool {
        self.panels.get(&self.panel_key(cx)).terminal_open
    }

    fn right_target(&self, cx: &App) -> f32 {
        if self.right_pane_open(cx) {
            let available = if self.viewport_width > 0.0 {
                (self.viewport_width - self.sidebar_target()).max(RIGHT_PANE_MIN)
            } else {
                self.settings.right_pane_width
            };
            self.settings.right_pane_width.min(available)
        } else {
            0.0
        }
    }

    fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        let from = self.sidebar_target();
        self.settings.sidebar_collapsed = !self.settings.sidebar_collapsed;
        self.sidebar_tween = Some(WidthTween::new(from, self.sidebar_target()));
        self.schedule_save(cx);
        cx.notify();
    }

    /// Roll the sidebar open if it is collapsed, on the same [`WidthTween`]
    /// path [`Self::toggle_sidebar`] uses. Anything that focuses or reveals
    /// something *inside* the column has to call this first: a collapsed
    /// sidebar is still MOUNTED (`pane_container` clips a full-width child
    /// inside `w(0) + overflow_hidden`), so focus lands on a typable but
    /// invisible field and silently steals keystrokes from the composer.
    fn reveal_sidebar(&mut self, cx: &mut Context<Self>) {
        if !self.settings.sidebar_collapsed {
            return;
        }
        let from = self.sidebar_target();
        self.settings.sidebar_collapsed = false;
        self.sidebar_tween = Some(WidthTween::new(from, self.sidebar_target()));
        self.schedule_save(cx);
    }

    /// Transient, sidebar-local UI that must not survive the column being
    /// re-rendered from scratch: the scope dropdown's open flag/highlight.
    ///
    /// The panel is a CHILD of the scope trigger, so anything that stops
    /// rendering `render_spaces_section` (entering search-results mode,
    /// leaving the Chat route) unmounts it — and an unmounted panel can never
    /// run its own `on_mouse_down_out`/Escape teardown. Left set, the flag
    /// reopens the panel the moment the section comes back, in whatever mode
    /// it was in ("New session in…" included).
    fn close_space_dropdown(&mut self) {
        self.space_dropdown_open = None;
        self.space_dropdown_highlight = None;
        self.space_dropdown_focus_pending = false;
    }

    /// The single funnel for route changes. A route swap replaces the whole
    /// sidebar column (Settings renders `render_settings_nav` instead of
    /// `render_chat_sidebar`), so it takes both pieces of transient sidebar
    /// state with it: a stuck dropdown flag, and a stale query that would
    /// otherwise still be filtering the list on the way back from Settings.
    fn set_route(&mut self, route: Route, cx: &mut Context<Self>) {
        if self.route == route {
            return;
        }
        self.route = route;
        self.close_space_dropdown();
        if !self.search_query(cx).is_empty() {
            self.clear_search(cx);
        }
    }

    /// ⌘P (`FocusSearch`): focus the pinned sidebar search field from
    /// anywhere in chat mode.
    ///
    /// Rolls the sidebar open first — the field is otherwise focused while
    /// clipped to zero width. Closes the scope dropdown too: this is a bare
    /// `window.focus` with no mouse-down anywhere, so the panel's
    /// `on_mouse_down_out` never fires, and typing one character would then
    /// unmount the panel (search-results mode) with the flag still set.
    fn focus_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.reveal_sidebar(cx);
        self.close_space_dropdown();
        let handle = self.search_input.focus_handle(cx);
        window.focus(&handle, cx);
        cx.notify();
    }

    /// Escape clears the sidebar query from OUTSIDE the sidebar. The full
    /// ↑↓/⏎/esc handler ([`Self::search_key`]) is mounted on the sidebar
    /// column and only sees keys dispatched through it, so once focus moves
    /// away (clicking the transcript hands it to the composer via
    /// `on_focus_lost`) the clear button was the only way out.
    ///
    /// Deliberately narrow: Escape only, never ↑↓/⏎ — those belong to whatever
    /// is focused (Enter in the composer sends). It also stands down while a
    /// shell dialog, menu, or palette is up, since Escape is theirs; the inner
    /// handler having already cleared the query makes this a no-op on the
    /// bubble up from the field itself.
    fn search_escape_key(&mut self, event: &gpui::KeyDownEvent, cx: &mut Context<Self>) {
        if !matches!(self.route, Route::Chat) || self.overlay_open() {
            return;
        }
        if self.search_query(cx).is_empty() {
            return;
        }
        let key = popover::classify_key(
            event.keystroke.key.as_str(),
            event.keystroke.modifiers.platform,
            event.keystroke.modifiers.control,
        );
        if key == popover::MenuKey::Escape {
            self.clear_search(cx);
        }
    }

    /// Is a shell-owned dialog, context menu, or palette on screen? Escape
    /// belongs to it, not to the sidebar query.
    fn overlay_open(&self) -> bool {
        self.chat_menu.is_some()
            || self.rename_dialog.is_some()
            || self.delete_confirm.is_some()
            || self.space_menu.is_some()
            || self.rename_space_dialog.is_some()
            || self.delete_space_confirm.is_some()
            || self.add_space.is_some()
            || self.space_dropdown_open.is_some()
    }

    fn toggle_right_pane(&mut self, cx: &mut Context<Self>) {
        // No git in this space → no diff pane, Cmd-B goes dead.
        if !self.space_git_detected(cx) {
            return;
        }
        let from = self.right_target(cx);
        let key = self.panel_key(cx);
        let open = self.panels.toggle_changes(&key);
        self.right_tween = Some(WidthTween::new(from, self.right_target(cx)));
        if open {
            // Lazy: the Changes entity (and its WatchCheckoutDiffs) exists only
            // once the pane has been opened.
            let changes = self.changes_pane(cx);
            changes.update(cx, |changes, cx| changes.ensure_watch(cx));
        }
        cx.notify();
    }

    fn changes_pane(&mut self, cx: &mut Context<Self>) -> Entity<Changes> {
        if let Some(changes) = &self.changes {
            return changes.clone();
        }
        let changes = cx.new(|cx| Changes::new(self.state.clone(), cx));
        self.changes = Some(changes.clone());
        changes
    }

    fn terminal_panel(&mut self, cx: &mut Context<Self>) -> Entity<TerminalPanel> {
        if let Some(terminal) = &self.terminal {
            return terminal.clone();
        }
        let terminal = cx.new(|cx| TerminalPanel::new(self.state.clone(), cx));
        self.terminal = Some(terminal.clone());
        terminal
    }

    fn terminal_target(&self, cx: &App) -> f32 {
        if self.terminal_open(cx) {
            self.settings.terminal_height
        } else {
            0.0
        }
    }

    /// Cmd/Ctrl+J and the header button (feature-inventory §1.10). Height
    /// animates 200 ms; closing detaches (PTYs stay alive), opening restores.
    /// The flag is per chat (comet `sessionPanels`).
    fn toggle_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let from = self.terminal_target(cx);
        let key = self.panel_key(cx);
        let open = self.panels.toggle_terminal(&key);
        self.terminal_tween = Some(WidthTween::new(from, self.terminal_target(cx)));
        let panel = self.terminal_panel(cx);
        panel.update(cx, |panel, cx| panel.set_open(open, cx));
        if open {
            // Opening lands keyboard focus IN the shell — typing goes straight
            // to the prompt, no click needed (comet terminal-panel.tsx: the
            // visible+active effect calls `terminal.focus()` on every open).
            // The handle is focusable before the panel's first paint; once the
            // terminal body mounts with `track_focus` it receives the keys.
            window.focus(&panel.read(cx).focus_handle(), cx);
        } else {
            // Hiding the panel removes the (likely focused) terminal view;
            // with nothing focused, window key bindings stop dispatching, so
            // hand focus to the composer. (Cmd+J is a pure toggle — a second
            // press closes even while the terminal is focused, as in comet's
            // `useHotkey(toggleShortcut, ... setOpenScoped(!open))`.)
            window.focus(&self.composer.focus_handle(cx), cx);
        }
        self.terminal_tween_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(RESIZE.total().mul_f32(motion::speed_scale()) + Duration::from_millis(30))
                .await;
            this.update(cx, |shell, cx| {
                shell.terminal_tween = None;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn on_terminal_drag(
        &mut self,
        event: &gpui::DragMoveEvent<TerminalResize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((anchor_y, anchor_h)) = self.terminal_drag_anchor else {
            return;
        };
        let dy = anchor_y - f32::from(event.event.position.y);
        let viewport_h = f32::from(window.viewport_size().height);
        self.settings.terminal_height = clamp_terminal_height(anchor_h + dy, viewport_h);
        self.terminal_tween = None; // live drag tracks the pointer
        self.schedule_save(cx);
        cx.notify();
    }

    fn on_sidebar_drag(
        &mut self,
        event: &gpui::DragMoveEvent<SidebarResize>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let x = f32::from(event.event.position.x);
        self.settings.sidebar_width = x.clamp(SIDEBAR_MIN, SIDEBAR_MAX);
        self.settings.sidebar_collapsed = false;
        self.sidebar_tween = None; // live drag tracks the pointer directly
        self.schedule_save(cx);
        cx.notify();
    }

    fn on_right_pane_drag(
        &mut self,
        event: &gpui::DragMoveEvent<RightPaneResize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let viewport = f32::from(window.viewport_size().width);
        let width = viewport - f32::from(event.event.position.x);
        // No arbitrary percentage or pixel ceiling: the pane can consume all
        // space to the right of the left sidebar. The chat flexes down to zero.
        let max = (viewport - self.sidebar_target()).max(RIGHT_PANE_MIN);
        self.settings.right_pane_width = width.clamp(RIGHT_PANE_MIN, max);
        self.right_tween = None;
        self.schedule_save(cx);
        cx.notify();
    }

    /// Debounced settings write: waits [`SAVE_DEBOUNCE_MS`], then persists the
    /// latest snapshot on the background executor. Re-scheduling drops (cancels)
    /// the previous timer.
    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        let dir = self.data_dir.clone();
        self.save_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(SAVE_DEBOUNCE_MS))
                .await;
            // Re-stamp the appearance from the global before writing. The View
            // menu changes it through `appearance::set_mode`, which never touches
            // this shell's in-memory copy — without this, the next pane resize
            // would quietly write the boot-time appearance back over the user's
            // choice.
            let Ok(snapshot) = this.update(cx, |shell, cx| {
                shell.settings.appearance = crate::appearance::mode(cx);
                shell.settings.clone()
            }) else {
                return;
            };
            cx.background_executor()
                .spawn(async move {
                    if let Err(err) = snapshot.save(&dir) {
                        tracing::warn!(error = %err, "failed to persist ui settings");
                    }
                })
                .await;
        }));
    }

    fn retry_engine(&mut self, cx: &mut Context<Self>) {
        AppState::bootstrap(self.state.clone(), self.boot.clone(), cx);
    }

    // ---- routes / settings ----

    fn open_settings(&mut self, section: SettingsSection, cx: &mut Context<Self>) {
        self.set_route(Route::Settings(section), cx);
        self.nav.push(NavEntry::Settings(section));
        self.chat_menu = None;
        cx.notify();
    }

    fn close_settings(&mut self, cx: &mut Context<Self>) {
        self.set_route(Route::Chat, cx);
        self.nav.push(NavEntry::Chat(self.active_chat.clone()));
        cx.notify();
    }

    // ---- back/forward (route history) ----

    fn navigate_back(&mut self, cx: &mut Context<Self>) {
        if let Some(entry) = self.nav.back() {
            self.apply_nav(entry, cx);
        }
    }

    fn navigate_forward(&mut self, cx: &mut Context<Self>) {
        if let Some(entry) = self.nav.forward() {
            self.apply_nav(entry, cx);
        }
    }

    /// Land on a history entry WITHOUT recording a new one: the stack already
    /// points at `entry` (back/forward moved the index); the selection change
    /// this triggers dedups against `current()` in [`Self::on_state_changed`].
    fn apply_nav(&mut self, entry: NavEntry, cx: &mut Context<Self>) {
        match entry {
            NavEntry::Chat(chat_id) => {
                self.set_route(Route::Chat, cx);
                let target = (!chat_id.is_empty()).then_some(chat_id);
                if self.state.read(cx).selected_chat_id() != target.as_deref() {
                    self.state.update(cx, |s, cx| s.select_chat(target, cx));
                }
            }
            NavEntry::Settings(section) => {
                self.set_route(Route::Settings(section), cx);
            }
        }
        self.chat_menu = None;
        cx.notify();
    }

    /// Lazily create the entity for a settings section and return it renderable.
    fn settings_outlet(&mut self, section: SettingsSection, cx: &mut Context<Self>) -> AnyElement {
        match section {
            SettingsSection::Devices => {
                if self.devices_page.is_none() {
                    let state = self.state.clone();
                    self.devices_page = Some(cx.new(|cx| DevicesPage::new(state, cx)));
                }
                match &self.devices_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::RemoteConnections => {
                if self.remote_connections_page.is_none() {
                    let state = self.state.clone();
                    self.remote_connections_page =
                        Some(cx.new(|cx| RemoteConnectionsPage::new(state, cx)));
                }
                match &self.remote_connections_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Agents => {
                if self.accounts_page.is_none() {
                    let state = self.state.clone();
                    self.accounts_page = Some(cx.new(|cx| AccountsPage::new(state, cx)));
                }
                match &self.accounts_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Appearance => {
                if self.appearance_page.is_none() {
                    self.appearance_page = Some(cx.new(AppearancePage::new));
                }
                match &self.appearance_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Shortcuts => {
                if self.shortcuts_page.is_none() {
                    let state = self.state.clone();
                    let keymap = self.settings.keymap.clone();
                    let page = cx.new(|cx| ShortcutsPage::new(state, keymap, cx));
                    // Persist + re-apply the keymap whenever the page changes it.
                    self.shortcuts_sub = Some(cx.subscribe(
                        &page,
                        |this: &mut Shell, _, event: &ShortcutsEvent, cx| {
                            let ShortcutsEvent::Changed(keymap) = event;
                            this.settings.keymap = keymap.clone();
                            apply_keymap(cx, keymap);
                            this.schedule_save(cx);
                            cx.notify();
                        },
                    ));
                    self.shortcuts_page = Some(page);
                }
                match &self.shortcuts_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Archived => {
                if self.archived_page.is_none() {
                    let state = self.state.clone();
                    self.archived_page = Some(cx.new(|cx| ArchivedPage::new(state, cx)));
                }
                match &self.archived_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
        }
    }

    // ---- sidebar mutations ----

    /// Fire a Mutate op; failures surface in the sidebar notice strip.
    fn mutate(&mut self, params: serde_json::Value, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).selected_client() else {
            self.sidebar_notice = Some("Engine not connected".into());
            cx.notify();
            return;
        };
        self.mutate_task = Some(cx.spawn(async move |this, cx| {
            if let Err(err) = engine.client().call(methods::MUTATE, params).await {
                this.update(cx, |shell, cx| {
                    shell.sidebar_notice = Some(
                        crate::errors::mutation_failure(crate::errors::Mutating::Document, &err)
                            .into(),
                    );
                    cx.notify();
                })
                .ok();
            }
        }));
    }

    /// Mutate the server that owned a menu/dialog/tab when it was opened.
    /// Selection is deliberately irrelevant: it may have changed while the
    /// confirmation UI was open.
    fn mutate_for(
        &mut self,
        owner: comet_proto::ServerRef,
        params: serde_json::Value,
        cx: &mut Context<Self>,
    ) {
        let engine = match self.state.read(cx).mutation_client_for(&owner) {
            Ok(engine) => engine,
            Err(err) => {
                self.sidebar_notice = Some(
                    crate::errors::mutation_failure(crate::errors::Mutating::OwnerReachable, &err)
                        .into(),
                );
                cx.notify();
                return;
            }
        };
        self.mutate_task = Some(cx.spawn(async move |this, cx| {
            if let Err(err) = engine.client().call(methods::MUTATE, params).await {
                this.update(cx, |shell, cx| {
                    shell.sidebar_notice = Some(
                        crate::errors::mutation_failure(crate::errors::Mutating::Document, &err)
                            .into(),
                    );
                    cx.notify();
                })
                .ok();
            }
        }));
    }

    fn open_rename_chat(&mut self, chat_id: comet_proto::ServerRef, cx: &mut Context<Self>) {
        self.chat_menu = None;
        let current = self
            .state
            .read(cx)
            .servers
            .get(&chat_id.server_id)
            .into_iter()
            .flat_map(|server| &server.chats)
            .find(|c| c.id == chat_id.local_id)
            .and_then(|c| c.title.clone())
            .unwrap_or_default();
        let input = cx.new(|cx| ComposerInput::new("Session title", cx));
        input.update(cx, |input, cx| input.set_text(current, cx));
        let events = cx.subscribe(&input, |this: &mut Shell, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.submit_rename_chat(cx);
            }
        });
        self.rename_dialog = Some(RenameChatDialog {
            chat_id,
            input,
            focus_pending: true,
            _events: events,
        });
        cx.notify();
    }

    fn submit_rename_chat(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.rename_dialog.take() else {
            return;
        };
        let title = dialog.input.read(cx).text().trim().to_string();
        if !title.is_empty() {
            let owner = dialog.chat_id;
            self.mutate_for(
                owner.clone(),
                serde_json::json!({ "op": "renameChat", "chatId": owner.local_id, "title": title }),
                cx,
            );
        }
        cx.notify();
    }

    /// A jump shortcut: open the sidebar row at `slot`. A slot past the end of
    /// a short list does nothing.
    fn jump_to_session(&mut self, slot: usize, cx: &mut Context<Self>) {
        if self.overlay_owns_keyboard(cx) {
            return;
        }
        let Some(chat_id) = self.state.read(cx).jump_target(Utc::now(), slot) else {
            return;
        };
        // A jump routes back to chat, so Settings is not a dead spot — then
        // the same call a click on that sidebar row makes.
        self.set_route(Route::Chat, cx);
        self.state
            .update(cx, |state, cx| state.select_chat(Some(chat_id), cx));
    }

    /// Whether an overlay that owns the keyboard is up — any shell-owned
    /// dialog, context menu or palette ([`Self::overlay_open`]), or a composer
    /// picker popover (harness/model, reasoning, repo, branch…). Session-nav
    /// shortcuts (cycle/jump/archive) go quiet underneath one: gpui runs a
    /// matched binding before any `on_key_down`, so an unguarded jump would
    /// switch sessions UNDER the open surface, stranding it over a session the
    /// user never picked.
    ///
    /// It delegates to `overlay_open` rather than naming the shell surfaces
    /// again. Hand-listing them here is what this fixes: the first version
    /// named only the add-space palette, so with "Delete session?" up, ⌘1
    /// switched session and ⌘⇧A archived one — while the dialog kept the
    /// `ServerRef` it captured and deleted THAT chat on confirm. Nothing
    /// tears a stale dialog down on a jump; `on_state_changed` clears them
    /// only when the selected *server* changes, not the chat.
    pub(super) fn overlay_owns_keyboard(&self, cx: &App) -> bool {
        self.overlay_open() || self.composer.read(cx).pickers().read(cx).is_open()
    }

    /// Track the held modifiers so the sidebar can show its jump hints. Only a
    /// change in visibility repaints — modifier traffic is otherwise constant.
    fn on_modifiers_changed(&mut self, event: &ModifiersChangedEvent, cx: &mut Context<Self>) {
        let mods = &event.modifiers;
        let primary = if cfg!(target_os = "macos") {
            mods.platform
        } else {
            mods.control
        };
        // No hints while an overlay owns the keyboard — the jumps they
        // advertise are suppressed there.
        let visible = matches!(self.route, Route::Chat)
            && !self.overlay_owns_keyboard(cx)
            && jump_hints_visible(&self.settings.keymap, primary, mods.alt, mods.shift);
        self.set_jump_hints(visible, cx);
    }

    pub(super) fn set_jump_hints(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.jump_hints != visible {
            self.jump_hints = visible;
            cx.notify();
        }
    }

    /// The Archive session shortcut. With no chat open, or with an already
    /// archived one, it does nothing — the shortcut archives, it never
    /// unarchives.
    fn archive_selected_chat(&mut self, cx: &mut Context<Self>) {
        if self.overlay_owns_keyboard(cx) {
            return;
        }
        let Some(chat_id) = self.state.read(cx).archivable_selected_chat() else {
            return;
        };
        self.archive_chat(chat_id, cx);
    }

    fn archive_chat(&mut self, chat_id: comet_proto::ServerRef, cx: &mut Context<Self>) {
        self.chat_menu = None;
        self.mutate_for(
            chat_id.clone(),
            serde_json::json!({ "op": "setChatArchived", "chatId": chat_id.local_id, "archived": true }),
            cx,
        );
        cx.notify();
    }

    fn delete_chat(&mut self, chat_id: comet_proto::ServerRef, cx: &mut Context<Self>) {
        self.delete_confirm = None;
        if self.state.read(cx).selected_chat.as_ref() == Some(&chat_id) {
            self.state.update(cx, |s, cx| s.select_chat(None, cx));
        }
        self.composer
            .update(cx, |composer, cx| composer.purge_chat(&chat_id, cx));
        self.mutate_for(
            chat_id.clone(),
            serde_json::json!({ "op": "deleteChat", "chatId": chat_id.local_id }),
            cx,
        );
        cx.notify();
    }

    // ---- render pieces ----

    /// Evaluate a width tween at "now" (manual drive — see [`WidthTween`]).
    /// Mid-flight: eased 200ms lerp, and `motion_active` is flagged so render
    /// schedules the next animation frame. Finished, stale, absent, or under
    /// reduced motion: exactly `target`. Honors `COMET_MOTION_SCALE`.
    fn eval_tween(&self, tween: Option<WidthTween>, target: f32) -> f32 {
        let Some(WidthTween { from, to, started }) = tween else {
            return target;
        };
        if self.reduced_motion {
            return target;
        }
        let total = RESIZE.total().mul_f32(motion::speed_scale());
        let raw = started.elapsed().as_secs_f32() / total.as_secs_f32();
        if raw >= 1.0 {
            return target;
        }
        self.motion_active.set(true);
        motion::lerp(from, to, RESIZE.progress(raw))
    }

    /// Animated width container: tweens 200ms ease-out on collapse/expand, and
    /// clips a fixed-width inner so content never reflows mid-transition.
    fn pane_container(
        &self,
        tween: Option<WidthTween>,
        target: f32,
        inner: AnyElement,
    ) -> AnyElement {
        div()
            .h_full()
            .flex_none()
            .overflow_hidden()
            .w(px(self.eval_tween(tween, target)))
            .child(inner)
            .into_any_element()
    }

    /// The animated spacer clearing the macOS traffic lights ahead of a
    /// titlebar control cluster. Fullscreen toggles tween the cluster start
    /// over 200ms ease-out ([`RESIZE`]; reduced motion snaps).
    /// `None` off macOS — no phantom flex child.
    fn titlebar_spacer(&self, container_pad: f32) -> Option<AnyElement> {
        if !cfg!(target_os = "macos") {
            return None;
        }
        let fullscreen = self.fullscreen.unwrap_or(false);
        // The tween runs in cluster-start coordinates; the spacer is that
        // minus the container's own padding.
        let start = self.eval_tween(self.titlebar_tween, titlebar_cluster_start(fullscreen));
        let width = (start - container_pad).max(0.0);
        Some(div().flex_none().h_full().w(px(width)).into_any_element())
    }

    /// The header's content row with the animated left inset — the native port
    /// of comet __root.tsx `transition-[padding-left] duration-200 ease-out` +
    /// `style={{ paddingLeft: headerInset }}`: on sidebar toggles (and macOS
    /// fullscreen flips) the SAME element's padding tweens, so the title
    /// glides to its new x-position. Route changes SNAP: the tween is killed
    /// by every route transition (comet remounts the keyed header variants —
    /// instant swap, zero horizontal motion).
    /// Where unified-titlebar content (tabs / the settings label) starts: past
    /// the traffic lights + control cluster, riding the fullscreen inset tween.
    pub(super) fn title_bar_content_start(&self) -> f32 {
        let fullscreen = self.fullscreen.unwrap_or(false);
        let is_macos = cfg!(target_os = "macos");
        let cluster = self.eval_tween(
            self.titlebar_tween,
            cluster_buttons_start(is_macos, fullscreen),
        );
        cluster + CLUSTER_BUTTONS_WIDTH + 10.0
    }

    fn windows_caption_clearance(&self) -> f32 {
        windows_caption_clearance(
            cfg!(target_os = "windows"),
            self.fullscreen.unwrap_or(false),
        )
    }

    /// The unified window titlebar: chat → the session tab strip; settings →
    /// the section label. Full-width on the glass shell; the traffic lights
    /// and control cluster overlay its left end.
    fn render_title_bar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        match self.route {
            Route::Chat => self.render_session_tab_strip(cx),
            Route::Settings(_) => {
                let inner = div()
                    .size_full()
                    .flex()
                    .items_center()
                    .pt(px(Theme::TITLEBAR_TOP_PAD))
                    .pl(px(self.title_bar_content_start()))
                    .pr(px(Theme::SPACE_LG + self.windows_caption_clearance()));
                let bar = div().h(px(Theme::TITLEBAR_HEIGHT)).flex_none().child(inner);
                self.titlebar_drag_region("settings-header-titlebar", bar, cx)
                    .into_any_element()
            }
        }
    }

    /// Make a titlebar strip drag the window — zed's platform-titlebar
    /// pattern (comet's `.drag` region): mark it a [`WindowControlArea::Drag`]
    /// (macOS app-owned titlebar), hand the drag to the compositor once the
    /// pointer moves with the button down, and double-click zooms.
    fn titlebar_drag_region(
        &self,
        id: &'static str,
        el: gpui::Div,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        el.id(id)
            .window_control_area(WindowControlArea::Drag)
            .on_mouse_down_out(cx.listener(|this, _, _, _| this.titlebar_should_move = false))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.titlebar_should_move = false),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.titlebar_should_move = true),
            )
            // Hand the drag to the compositor only while the button is
            // actually held (`pressed_button` guard): on macOS
            // `start_window_move` runs AppKit's NATIVE drag session
            // (`performWindowDragWithEvent:`), and AppKit resolves a quick
            // second click inside that session as a titlebar double-click —
            // system zoom — natively, beyond gpui's reach. Without the guard a
            // stale `titlebar_should_move` (armed by a down whose bubble was
            // later stopped) would start that session from a mere hover move
            // between the two clicks of a double-click.
            .on_mouse_move(
                cx.listener(|this, event: &gpui::MouseMoveEvent, window, _| {
                    if this.titlebar_should_move && event.pressed_button == Some(MouseButton::Left)
                    {
                        this.titlebar_should_move = false;
                        window.start_window_move();
                    }
                }),
            )
            .on_click(|event, window, _| {
                if event.click_count() == 2 {
                    if cfg!(target_os = "macos") {
                        // Native titlebar double-click action (zoom/minimize
                        // per system preference).
                        window.titlebar_double_click();
                    } else {
                        window.zoom_window();
                    }
                }
            })
    }

    /// The ONE top-left window-control cluster (sidebar toggle + back/forward —
    /// comet window-controls.tsx): rendered once, in a paint-only overlay layer
    /// pinned at the window's top-left, ABOVE the sidebar and headers. The
    /// sidebar width animates *beneath* it, so the buttons keep their element
    /// identity and never move or remount on collapse/expand; only the
    /// fullscreen traffic-light inset tweens (the animated spacer). The
    /// container has no id/listeners — everything between the buttons falls
    /// through to the titlebar drag strips below.
    fn render_titlebar_cluster(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let can_back = self.nav.can_back();
        let can_forward = self.nav.can_forward();
        div()
            .absolute()
            .top_0()
            .left_0()
            .h(px(Theme::TITLEBAR_HEIGHT))
            .flex()
            .flex_row()
            .items_center()
            .pt(px(Theme::TITLEBAR_TOP_PAD))
            .gap(px(2.0))
            .px(px(10.0))
            .children(self.titlebar_spacer(12.0))
            .child(window_control_button(
                "toggle-sidebar",
                icons::SIDEBAR_MINIMALISTIC_LEFT,
                &theme,
                cx.listener(|this, _, _, cx| this.toggle_sidebar(cx)),
            ))
            .child(nav_history_button(
                "nav-back",
                icons::ARROW_LEFT,
                can_back,
                &theme,
                cx.listener(|this, _, _, cx| this.navigate_back(cx)),
            ))
            .child(nav_history_button(
                "nav-forward",
                icons::ARROW_RIGHT,
                can_forward,
                &theme,
                cx.listener(|this, _, _, cx| this.navigate_forward(cx)),
            ))
            .into_any_element()
    }

    #[cfg(target_os = "windows")]
    fn render_windows_caption_controls(&self, window: &Window, cx: &App) -> Option<AnyElement> {
        if self.fullscreen.unwrap_or(false) {
            return None;
        }

        let theme = Theme::of(cx);
        Some(
            div()
                .absolute()
                .top_0()
                .right_0()
                .h(px(Theme::TITLEBAR_HEIGHT))
                .flex()
                .flex_row()
                .font_family(windows_caption_font())
                .children(
                    windows_caption_buttons(window.is_maximized())
                        .into_iter()
                        .map(|button| render_windows_caption_button(button, theme)),
                )
                .into_any_element(),
        )
    }

    #[cfg(not(target_os = "windows"))]
    fn render_windows_caption_controls(&self, _window: &Window, _cx: &App) -> Option<AnyElement> {
        None
    }

    fn render_sidebar(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let inner: AnyElement = match self.route {
            Route::Settings(section) => self.render_settings_nav(section, &theme, cx),
            Route::Chat => self.render_chat_sidebar(window, &theme, cx),
        };
        let target = self.sidebar_target();
        // Transparent — the sidebar sits directly on the frost shell; the main
        // card's own border provides the separation.
        self.pane_container(
            self.sidebar_tween,
            target,
            div().h_full().child(inner).into_any_element(),
        )
    }

    /// Settings-mode sidebar (comet settings-sidebar.tsx): window-control
    /// strip, "Settings" heading, icon section rows styled like session rows,
    /// and a Back row pinned to the bottom.
    fn render_settings_nav(
        &mut self,
        section: SettingsSection,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let section_icon = |item: SettingsSection| match item {
            SettingsSection::Devices => icons::MONITOR,
            SettingsSection::RemoteConnections => icons::GLOBAL,
            SettingsSection::Agents => icons::KEY_MINIMALISTIC,
            SettingsSection::Appearance => icons::TUNING,
            SettingsSection::Shortcuts => icons::KEYBOARD,
            SettingsSection::Archived => icons::ARCHIVE_MINIMALISTIC,
        };
        // Match the user's dragged sidebar width — the pane container clips to
        // it, so a hardcoded default here left hover washes stopping short of
        // the sidebar's right edge (user-reported). Device identity lives on
        // the Accounts page now — the one surface where the device matters.
        div()
            .w(px(self.settings.sidebar_width))
            .h_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex_1()
                    .px(px(Theme::SPACE_SM))
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .px(px(Theme::SPACE_SM))
                            .pt(px(12.0))
                            .pb(px(4.0))
                            .text_size(px(11.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text_muted.opacity(0.6))
                            .child(SharedString::from("Settings")),
                    )
                    .child(div().flex().flex_col().gap(px(2.0)).children(
                        SettingsSection::ALL.into_iter().map(|item| {
                            let selected = item == section;
                            div()
                                .id(SharedString::from(format!("settings-nav-{}", item.label())))
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(8.0))
                                .rounded(px(8.0))
                                .px(px(Theme::SPACE_SM))
                                .py(px(6.0))
                                .text_size(px(13.0))
                                .when(selected, |el| {
                                    el.bg(crate::theme::wash(0.17))
                                        .font_weight(gpui::FontWeight::MEDIUM)
                                })
                                .text_color(if selected {
                                    theme.text
                                } else {
                                    theme.text_muted
                                })
                                .cursor_pointer()
                                .hover(|s| s.bg(crate::theme::wash(0.11)).text_color(theme.text))
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.open_settings(item, cx)),
                                )
                                .child(
                                    icon(section_icon(item))
                                        .size(px(16.0))
                                        .text_color(theme.text_muted),
                                )
                                .child(SharedString::from(item.label()))
                        }),
                    )),
            )
            // Back pinned to the bottom (comet settings-sidebar.tsx).
            .child(
                div().px(px(Theme::SPACE_SM)).pb(px(12.0)).child(
                    div()
                        .id("settings-back")
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0))
                        .rounded(px(8.0))
                        .px(px(Theme::SPACE_SM))
                        .py(px(6.0))
                        .text_size(px(13.0))
                        .text_color(theme.text_muted)
                        .cursor_pointer()
                        .hover(|s| s.bg(crate::theme::wash(0.11)).text_color(theme.text))
                        .on_click(cx.listener(|this, _, _, cx| this.close_settings(cx)))
                        .child(
                            // AltArrowLeft chevron (comet settings-sidebar.tsx),
                            // not the straight history arrow.
                            icon(icons::ALT_ARROW_LEFT)
                                .size(px(16.0))
                                .text_color(theme.text_muted),
                        )
                        .child(SharedString::from("Back")),
                ),
            )
            .into_any_element()
    }

    /// One session row (comet session-row.tsx): the row's TOP line carries
    /// the trailing time-ago (+ throbber while Working), flush right — line 0
    /// ("folder · space @ device") in `RowScope::All`, or line 1 (the title
    /// line) in `RowScope::One`, which renders no line 0 to attach to. The
    /// title line is otherwise title-only. Right-click always opens the
    /// context menu; `on_click` decides what a plain click does (the normal
    /// list selects the chat in place, sidebar search additionally clears the
    /// query — see `search::Shell::render_search_chat_row`).
    ///
    /// `highlight_query`, when `Some`, tints the first case-insensitive hit in
    /// each of the space/title/branch lines (`search::styled_line`) — `None`
    /// for the normal list, which never tints anything. `keyboard_highlighted`
    /// is the search results' arrow-key cursor: a DIFFERENT visual from
    /// `selected` (the open chat) that can coincide with it, so both get their
    /// own `when`. This is the single row every session list in the sidebar
    /// draws through — an earlier revision had sidebar search build its own
    /// copy for tinting and it silently diverged (missing spinner, hover
    /// brighten, selected shadow, context menu) within a day.
    #[allow(clippy::too_many_arguments)]
    fn render_chat_row(
        &self,
        id: String,
        title: SharedString,
        time_ago: SharedString,
        scope: RowScope,
        branch: Option<SharedString>,
        harness: Option<comet_proto::HarnessId>,
        status: comet_proto::ChatIndicator,
        selected: bool,
        highlight_query: Option<&str>,
        keyboard_highlighted: bool,
        // This row's jump combo while the hint overlay is up. It replaces the
        // time-ago, which every row carries here — the working throbber sits
        // ALONGSIDE the time rather than instead of it, so unlike upstream
        // there is no busy row whose corner would be left empty.
        jump_label: Option<SharedString>,
        on_click: impl Fn(&mut Self, &mut Context<Self>) + 'static,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // No per-row status mark (user decision, 2026-08-06 revision): the
        // leading dot that used to carry Awaiting-input/Errored/Completed-
        // unseen is gone and NOT replaced — those three statuses now read
        // identically to Idle in the row. Working is the one exception: it
        // still animates, alongside (not instead of) the time-ago on the
        // row's TOP line rather than a dedicated rail (see `trailing_group`
        // below).
        let (hover, text) = (theme.glass_hover(), theme.text);
        let selected_wash = crate::theme::glass_selected_bg();
        let subline = theme.text_muted.opacity(0.5);
        let time_tint = theme.text_muted.opacity(0.45);
        let working = status == comet_proto::ChatIndicator::Working;
        // The trailing group — time-ago, plus the throbber while Working —
        // sits on the row's TOP line: line 0 (`RowScope::All`) or line 1
        // (`RowScope::One`, which has no line 0). Built once, up front, and
        // attached to whichever line owns it below, so its `line_height`
        // matches that line without duplicating the throbber/spinner
        // wiring. `gradient_spinner`'s 2×3 `mini_gradient_spinner` grid
        // can't be square, so this is the 3×3 `gradient_spinner` instead,
        // sized 13×13 to match the harness mark on line 2.
        let trailing_line_height = match &scope {
            RowScope::All { .. } => chat_row::SPACE_LINE,
            RowScope::One => chat_row::TITLE_LINE,
        };
        let trailing_group = div()
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.0))
            .child(match jump_label {
                // The jump hint takes the time-ago's place while the modifier
                // is held, wearing the app's keyboard-hint chip — the same
                // borderless pill the picker menus' accelerators use — so the
                // overlay introduces no new style and reflows nothing.
                Some(label) => crate::popover::kbd_hint(theme, &label)
                    .line_height(px(trailing_line_height))
                    .into_any_element(),
                None => div()
                    .flex_none()
                    .text_size(px(11.0))
                    .line_height(px(trailing_line_height))
                    .text_color(time_tint)
                    .child(time_ago)
                    .into_any_element(),
            })
            .when(working, |el| {
                el.child(
                    div()
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(loaders::gradient_spinner(
                            "chat-working",
                            theme,
                            3.25,
                            cx.entity_id(),
                            cx,
                        )),
                )
            })
            .into_any_element();
        let menu_id = comet_proto::ServerRef::new(
            self.state
                .read(cx)
                .selected_server_id()
                .cloned()
                .expect("chat rows require a server bucket"),
            id.clone(),
        );
        // Hover fades over transition-colors (comet session-row.tsx) — both
        // the wash and the title brighten ride the same 150ms blend.
        let fade_key = format!("chat-row-{id}");
        // `selected` (the open chat) and `keyboard_highlighted` (the search
        // results' arrow-key cursor) are different states that can coincide;
        // both get the same wash — a merely-selected row must not visually
        // outrank the keyboard cursor, or arrowing onto the already-open chat
        // reads as "less highlighted" than any other row (round-1 review).
        // Only the SHADOW differs: highlighted draws the inset accent ring,
        // selected (when not also highlighted) keeps the drop-seat shadow.
        let lit = selected || keyboard_highlighted;
        let rest_bg = if lit {
            selected_wash
        } else {
            crate::theme::wash(0.0)
        };
        // A lit row must NOT drift toward the hover wash: in dark the two
        // fills are identical so the blend is a no-op, but light's hover sits
        // below its near-opaque selected fill, and blending toward it visibly
        // dimmed the active row under the pointer (user report).
        let hover_bg = if lit { selected_wash } else { hover };
        let rest_text = if lit { text } else { text.opacity(0.8) };
        let title_color = motion::hover_blend(&fade_key, rest_text, text);
        let sans = gpui::font(theme.font_sans.clone());
        div()
            .id(SharedString::from(format!("chat-{id}")))
            .flex()
            .flex_col()
            .rounded(px(8.0))
            .px(px(Theme::SPACE_SM))
            .py(px(chat_row::PY))
            .text_color(title_color)
            .bg(motion::hover_blend(&fade_key, rest_bg, hover_bg))
            .when(keyboard_highlighted, |el| {
                el.shadow(search::highlight_ring(theme))
            })
            .when(!keyboard_highlighted && selected, |el| {
                el.shadow(crate::theme::glass_selected_shadows())
            })
            .on_hover(motion::hover_listener(fade_key))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| on_click(this, cx)))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    this.chat_menu = Some((menu_id.clone(), event.position));
                    cx.notify();
                }),
            )
            // Line 0 (all-spaces only) and line 1 (title): built together
            // because the trailing group (time-ago + throbber) attaches to
            // whichever of the two is the row's TOP line. `RowScope::All`
            // renders both, with the trailing group flush right on line 0
            // after `@ device`; `RowScope::One` renders no line 0, so the
            // trailing group stays on line 1, next to the title, exactly as
            // it did before line 0 grew one.
            .map(|el| match scope {
                RowScope::All {
                    space,
                    device,
                    host_offline,
                } => el
                    // Line 0: space + device, then the trailing group flush
                    // right. Starts flush at the row's own padding edge, same
                    // as lines 1/2 (no leading dot to clear anymore).
                    .child(
                        div()
                            .w_full()
                            .flex_none()
                            .h(px(chat_row::SPACE_LINE))
                            .mb(px(chat_row::SPACE_LINE_MB))
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(4.0))
                            .text_size(px(11.0))
                            .line_height(px(chat_row::SPACE_LINE))
                            .child(
                                icon(icons::FOLDER)
                                    .size(px(11.0))
                                    .flex_none()
                                    .text_color(theme.text_muted.opacity(0.5)),
                            )
                            .child(div().min_w_0().truncate().child(search::styled_line(
                                &space,
                                highlight_query,
                                theme.text_muted.opacity(0.75),
                                theme.accent,
                                sans.clone(),
                            )))
                            // The device never truncates: which machine a
                            // session runs on cannot be inferred anywhere else.
                            .child(
                                div()
                                    .flex_none()
                                    .text_color(if host_offline {
                                        theme.warning.opacity(0.8)
                                    } else {
                                        theme.text_muted.opacity(0.5)
                                    })
                                    .child(SharedString::from(if host_offline {
                                        format!("@ {device} · offline")
                                    } else {
                                        format!("@ {device}")
                                    })),
                            )
                            .child(div().flex_1())
                            .child(trailing_group),
                    )
                    // Line 1: title only, full width — the trailing group
                    // moved to line 0 above.
                    .child(
                        div()
                            .w_full()
                            .flex_none()
                            .h(px(chat_row::TITLE_LINE))
                            .flex()
                            .flex_row()
                            .items_center()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(13.0))
                                    .line_height(px(chat_row::TITLE_LINE))
                                    .child(search::styled_line(
                                        &title,
                                        highlight_query,
                                        title_color,
                                        theme.accent,
                                        sans.clone(),
                                    )),
                            ),
                    ),
                // Line 1 (no line 0 in this scope): title, then the trailing
                // group, exactly where it has always been here.
                RowScope::One => el.child(
                    div()
                        .w_full()
                        .flex_none()
                        .h(px(chat_row::TITLE_LINE))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(Theme::SPACE_SM))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_size(px(13.0))
                                .line_height(px(chat_row::TITLE_LINE))
                                .child(search::styled_line(
                                    &title,
                                    highlight_query,
                                    title_color,
                                    theme.accent,
                                    sans.clone(),
                                )),
                        )
                        .child(trailing_group),
                ),
            })
            // Line 2: branch on the left, agent mark pinned right. The mark is
            // flex_none so a long branch truncates into it and the right
            // column never breaks.
            //
            // The EXPLICIT height is load-bearing, not cosmetic: this line has
            // no text node of its own, so without it the row is 14/13/0 px
            // shorter depending on whether it carries a branch, only a harness
            // mark, or neither (a chat whose config frame hasn't landed) —
            // three different row heights against one `chat_row_height`, and
            // the resort FLIP glides every survivor to the wrong y.
            .child(
                div()
                    .w_full()
                    .flex_none()
                    .h(px(chat_row::META_LINE))
                    .mt(px(chat_row::META_LINE_MT))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(4.0))
                    .text_size(px(11.0))
                    .line_height(px(chat_row::META_LINE))
                    .text_color(subline)
                    .when_some(branch, |el, branch| {
                        el.child(
                            icon(icons::GIT_BRANCH)
                                .size(px(11.0))
                                .flex_none()
                                .text_color(subline),
                        )
                        .child(div().min_w_0().truncate().child(search::styled_line(
                            &branch,
                            highlight_query,
                            subline,
                            theme.accent,
                            sans.clone(),
                        )))
                    })
                    .child(div().flex_1().min_w(px(8.0)))
                    .when_some(
                        harness.map(crate::pickers::harness_brand_icon),
                        |el, (path, tint)| {
                            el.child(
                                icon(path)
                                    .size(px(13.0))
                                    .flex_none()
                                    .text_color(tint.unwrap_or(theme.text_muted.opacity(0.75))),
                            )
                        },
                    ),
            )
            .into_any_element()
    }

    /// Which sidebar-list edges have hidden overflow (offset from the LAST
    /// frame — the invisible one-frame lag every fade here rides).
    pub(super) fn sidebar_fade_zones(&self) -> (bool, bool) {
        let scrolled = -f32::from(self.sidebar_scroll.offset().y);
        let max_scroll = f32::from(self.sidebar_scroll.max_offset().y);
        (scrolled > 1.0, scrolled < max_scroll - 1.0)
    }

    /// The Sessions-header `+`. Scoped: straight to the new-session canvas for
    /// that space — the same thing the tab-strip `+` does. All spaces: ask
    /// which space first, unless there is only one.
    pub(super) fn start_session_from_sidebar(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> NewSessionTarget {
        let (scope, spaces) = {
            let state = self.state.read(cx);
            let spaces: Vec<String> = state.spaces.iter().map(|s| s.id.clone()).collect();
            (state.sidebar_scope.clone(), spaces)
        };
        let target = new_session_target(&scope, &spaces);
        match &target {
            NewSessionTarget::Space(id) => {
                self.set_route(Route::Chat, cx);
                let id = id.clone();
                self.state.update(cx, |s, cx| {
                    // Mirrors `activate_space`: `select_chat(None)` alone
                    // deliberately leaves `selected_space` untouched (a scope
                    // switch must not move what's open), so without this the
                    // canvas — and a submitted session's device — can stay on
                    // whatever space was previously selected, not the one the
                    // sidebar is scoped to.
                    //
                    // `sidebar_scope` is deliberately NOT written here, for the
                    // same reason [`Self::create_session_in`] (the picker's
                    // commit) doesn't: starting a session somewhere is not a
                    // request to re-scope the column, and the spec's action
                    // table doesn't list the `+` as scope-changing. It used to
                    // be written, which made the `+` on `All spaces` narrow the
                    // sidebar with exactly one space and not with two (the two
                    // arms disagreed), and the narrowing then evaporated on
                    // restart because it bypassed `settings.sidebar_scope_space`.
                    s.select_space(Some(id), cx);
                    s.select_chat(None, cx);
                });
                cx.notify();
            }
            NewSessionTarget::Pick(_) => {
                self.space_dropdown_open = Some(DropdownMode::PickForNewSession);
                self.space_dropdown_highlight = None;
                self.space_dropdown_focus_pending = true;
                cx.notify();
            }
            NewSessionTarget::AddSpaceFirst => self.open_add_space(cx),
        }
        target
    }

    /// The picker's commit path. Deliberately does not touch `sidebar_scope`:
    /// starting a session somewhere is not a request to re-scope the column.
    /// [`Self::start_session_from_sidebar`]'s no-prompt arm agrees — the two
    /// are the same action reached two ways and must land identically.
    fn create_session_in(&mut self, space_id: String, cx: &mut Context<Self>) {
        self.set_route(Route::Chat, cx);
        self.state.update(cx, |s, cx| {
            s.select_space(Some(space_id), cx);
            s.select_chat(None, cx);
        });
        self.close_space_dropdown();
        cx.notify();
    }

    /// Chat-mode sidebar (spaces overhaul): window-control strip, the Spaces
    /// section (folder + device rows, add-space), the global Active sessions
    /// list, the notice strip, and the UserMenu (§1.6).
    fn render_chat_sidebar(
        &mut self,
        window: &mut Window,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // Search ignores the current scope — it always reads the whole
        // projected set (`search::filter`'s doc comment). `None` = not
        // searching; `Some(empty)` = searching with no matches, a DIFFERENT
        // state from "not searching" that renders differently below.
        let query = self.search_query(cx).to_string();
        // The FIELD's own chrome keys off the raw (untrimmed) text — NOT
        // `results.is_some()`. `filter` trims, so a whitespace-only query is
        // `None` results; keying the clear-button/hint-chip swap off that
        // left a dead end where a lone space showed the ⌘P hint (implying
        // nothing to clear) yet visibly sat in the field (round-1 review).
        let has_text = !query.is_empty();
        let results = {
            let state = self.state.read(cx);
            search::filter(&query, &state.spaces, &state.chats, &|id| {
                state.device_name(id).map(str::to_string)
            })
        };

        // Overflow edge fades for the lists scroll region — the tab strip's
        // idiom, vertical (offset from the LAST frame; the lag is invisible).
        let (lists_fade_top, lists_fade_bottom) = self.sidebar_fade_zones();
        // Opaque platforms melt overflow into the surface tone with painted
        // gradient overlays. Over GLASS no overlay can work — the backdrop is
        // see-through blur, so tone stacks into a smudge and black reads as a
        // shadow (user reports). Instead the whole scroll region paints inside
        // a [`crate::edge_fade::edge_faded`] scope: a per-primitive gradient at
        // the active overflow edges, so text dissolves per glyph into pure
        // glass.
        let glass = theme.is_glass();
        let sidebar_fade = theme.surface;

        let settings_button = self.render_settings_button(theme, cx);
        let search_field = self.render_search_field(has_text, theme, cx);

        // Results mode swaps the Spaces + Sessions sections wholesale for the
        // matching rows (`search::Shell::render_search_results`) — the resort
        // FLIP bookkeeping below is a live-list concept that doesn't apply to
        // a filtered snapshot, so it's skipped entirely while searching.
        let lists_children: Vec<AnyElement> = if let Some(results) = &results {
            self.render_search_results(results, query.trim(), theme, cx)
        } else {
            // Keyed rows: (stable key, estimated height, element) — the key +
            // height list drives the §1.6 resort FLIP diff below
            // (attention-bucket promotions glide; cleared rows just go).
            let keyed: Vec<(String, f32, AnyElement)> = self.render_active_rows(theme, cx);

            // Resort glide (§1.6 View Transitions parity): when the ORDER of a
            // live list changes (new activity resort, grouping flip), surviving
            // rows glide from their old y to the new one — layout is already at
            // the new position; the offset is a paint-only relative inset
            // animated to 0 over 260ms cubic-bezier(0.22,1,0.36,1). New rows
            // fade in; removals just go (matching the original). First fill and
            // chat switches (which don't reorder) never animate.
            let order: Vec<(String, f32)> = keyed.iter().map(|(k, h, _)| (k.clone(), *h)).collect();
            if self.sidebar_prev_order != order {
                if !self.sidebar_prev_order.is_empty() {
                    let offsets =
                        resort_offsets(&self.sidebar_prev_order, &order, SIDEBAR_LIST_GAP);
                    let prev_keys: std::collections::HashSet<&str> = self
                        .sidebar_prev_order
                        .iter()
                        .map(|(k, _)| k.as_str())
                        .collect();
                    let new_keys: std::collections::HashSet<String> = order
                        .iter()
                        .filter(|(k, _)| !prev_keys.contains(k.as_str()))
                        .map(|(k, _)| k.clone())
                        .collect();
                    if !offsets.is_empty() || !new_keys.is_empty() {
                        self.resort_epoch += 1;
                        self.sidebar_resort = offsets;
                        self.sidebar_new_keys = new_keys;
                    }
                }
                self.sidebar_prev_order = order;
            }
            let epoch = self.resort_epoch;
            let list_items: Vec<AnyElement> = keyed
                .into_iter()
                .map(|(key, _, element)| {
                    if let Some(dy) = self.sidebar_resort.get(&key).copied() {
                        let id = SharedString::from(format!("resort-{epoch}-{key}"));
                        div()
                            .child(element)
                            .with_animation(id, RESORT.animation(), move |el, t| {
                                el.relative().top(px(dy * (1.0 - t)))
                            })
                            .into_any_element()
                    } else if self.sidebar_new_keys.contains(&key) {
                        let id = SharedString::from(format!("row-in-{epoch}-{key}"));
                        motion::fade_quick(id, div().child(element)).into_any_element()
                    } else {
                        element
                    }
                })
                .collect();

            let spaces_section = self.render_spaces_section(window, theme, cx);
            vec![
                spaces_section,
                spaces::section_header(
                    "Sessions",
                    false,
                    theme,
                    Some(spaces::header_plus(
                        "new-session",
                        theme,
                        cx.listener(|this, _, window, cx| {
                            this.start_session_from_sidebar(window, cx);
                        }),
                    )),
                )
                .into_any_element(),
                if !list_items.is_empty() {
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(SIDEBAR_LIST_GAP))
                        .pb(px(Theme::SPACE_SM))
                        .children(list_items)
                        .into_any_element()
                } else {
                    // Scope-aware empty state: names the scoped space when
                    // there is one, and its action fires the same handler as
                    // the Sessions `+` (§6).
                    let scoped_space_name = {
                        let state = self.state.read(cx);
                        state.sidebar_scope.space_id().map(|id| {
                            state
                                .spaces
                                .iter()
                                .find(|s| s.id == id)
                                .map(|s| s.display_name().to_string())
                                .unwrap_or_else(|| id.to_string())
                        })
                    };
                    div()
                        .px(px(Theme::SPACE_SM))
                        .pb(px(Theme::SPACE_SM))
                        .flex()
                        .flex_col()
                        .gap(px(6.0))
                        .child(
                            div()
                                .text_size(px(12.0))
                                .text_color(theme.text_faint)
                                .child(SharedString::from(match scoped_space_name {
                                    Some(name) => format!("Nothing running in {name} yet."),
                                    None => "No sessions on any space yet.".to_string(),
                                })),
                        )
                        .child(
                            div()
                                .id("empty-start-session")
                                .text_size(px(12.0))
                                .text_color(theme.accent)
                                .cursor_pointer()
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.start_session_from_sidebar(window, cx);
                                }))
                                .child(SharedString::from("Start a session →")),
                        )
                        .into_any_element()
                },
            ]
        };

        div()
            .w(px(self.settings.sidebar_width))
            .h_full()
            .flex()
            .flex_col()
            .on_key_down(
                cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| this.search_key(event, cx)),
            )
            // The pinned search field: first child of the column, OUTSIDE the
            // scroll region below, so it never scrolls away. As a result the
            // `SIDEBAR_GLASS_FADE_BAND` top fade now starts below this block,
            // not at the column's top edge.
            .child(search_field)
            // (No titlebar strip: the unified window titlebar spans the whole
            // window above this column.)
            // Spaces + the global Active list share one scroll region. On
            // glass the whole region paints inside an EdgeFade scope — a true
            // per-glyph gradient at active overflow edges.
            .child(crate::edge_fade::edge_faded(
                SIDEBAR_GLASS_FADE_BAND,
                glass && lists_fade_top,
                glass && lists_fade_bottom,
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .id("sidebar-lists")
                            .size_full()
                            .overflow_y_scroll()
                            .track_scroll(&self.sidebar_scroll)
                            .px(px(Theme::SPACE_SM))
                            .flex()
                            .flex_col()
                            .children(lists_children),
                    )
                    .when(lists_fade_top && !glass, |el| {
                        el.child(div().absolute().top_0().left_0().right_0().h(px(24.0)).bg(
                            gpui::linear_gradient(
                                180.0,
                                gpui::linear_color_stop(sidebar_fade, 0.0),
                                gpui::linear_color_stop(sidebar_fade.opacity(0.0), 1.0),
                            ),
                        ))
                    })
                    .when(lists_fade_bottom && !glass, |el| {
                        el.child(
                            div()
                                .absolute()
                                .bottom_0()
                                .left_0()
                                .right_0()
                                .h(px(24.0))
                                .bg(gpui::linear_gradient(
                                    0.0,
                                    gpui::linear_color_stop(sidebar_fade, 0.0),
                                    gpui::linear_color_stop(sidebar_fade.opacity(0.0), 1.0),
                                )),
                        )
                    }),
            ))
            // Identity-rebuild strip, above the update one: it explains why
            // the list the user is looking at reads offline, so it belongs
            // nearer the list than an unrelated release notice.
            .when_some(
                self.render_identity_rebuilt_strip(theme, cx),
                |el, strip| el.child(strip),
            )
            // Update strip (above the settings button; below the lists).
            .when_some(self.render_update_strip(theme, cx), |el, strip| {
                el.child(strip)
            })
            // Inline mutation-failure notice.
            .when_some(self.sidebar_notice.clone(), |el, notice| {
                el.child(
                    div()
                        .id("sidebar-notice")
                        .mx(px(Theme::SPACE_SM))
                        .mb(px(Theme::SPACE_SM))
                        .px(px(Theme::SPACE_SM))
                        .py(px(4.0))
                        .rounded(px(Theme::CONTROL_RADIUS))
                        .border_1()
                        .border_color(theme.danger)
                        .text_size(px(11.0))
                        .text_color(theme.danger)
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.sidebar_notice = None;
                            cx.notify();
                        }))
                        .child(notice),
                )
            })
            .child(
                div()
                    .p(px(Theme::SPACE_SM))
                    .flex_none()
                    .child(settings_button),
            )
            .into_any_element()
    }

    /// Update strip: shown above the settings button whenever the engine's
    /// UpdateStatus stream reports a newer release. On a macOS bundle install
    /// it drives the whole flow — click to download, then click to restart into
    /// the staged bundle. Elsewhere (managed/source installs) it is advisory
    /// (`comet update`); click dismisses it for that version.
    /// The identity-rebuild strip's copy.
    ///
    /// `concat!` rather than a backslash-continued literal: rustfmt rejoins those
    /// onto one line and the continuation indentation becomes a run of real spaces
    /// inside the sentence, invisible in the source. That has now shipped once
    /// (`normalize::session_update_once`'s cap copy) and been caught twice more
    /// while writing this kind of string — `the_notice_copy_has_no_stray_runs_of_spaces`
    /// below is what makes it a test rather than a habit.
    const IDENTITY_REBUILT_NOTICE: &'static str = concat!(
        "Add older spaces again to use them — this machine's identity was rebuilt, ",
        "so spaces created before that show as offline.",
    );

    /// "This machine's identity was rebuilt" — the one thing that explains a
    /// sidebar full of `@ host · offline` spaces (D96).
    ///
    /// **The action leads**, per `.agents/rules/user-facing-errors.md`: adding
    /// the folder again is what makes a space usable, and the cause follows it.
    /// Amber rather than red, because it is a state to resolve rather than
    /// something that failed — the recovery itself succeeded, and refusing to
    /// start was the alternative.
    ///
    /// **It cannot name the affected spaces.** The previous id died with the
    /// zero-byte file, so nothing on this machine still knows which rows
    /// belonged to it; "spaces created before then" is the most precise true
    /// statement available.
    fn render_identity_rebuilt_strip(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let stamp = identity_notice_stamp(
            self.state.read(cx).identity_rebuilt_at.as_deref(),
            self.settings.dismissed_identity_rebuild.as_deref(),
        )?
        .to_owned();
        Some(
            div()
                .id("identity-rebuilt-notice")
                .mx(px(Theme::SPACE_SM))
                .mb(px(Theme::SPACE_SM))
                .px(px(Theme::SPACE_SM))
                .py(px(4.0))
                .rounded(px(Theme::CONTROL_RADIUS))
                .border_1()
                .border_color(theme.warning.opacity(0.5))
                .text_size(px(11.0))
                .text_color(theme.warning)
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.settings.dismissed_identity_rebuild = Some(stamp.clone());
                    this.schedule_save(cx);
                    cx.notify();
                }))
                .child(SharedString::from(Self::IDENTITY_REBUILT_NOTICE))
                .into_any_element(),
        )
    }

    fn render_update_strip(&mut self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let status = self.state.read(cx).update.clone()?;
        if !status.update_available {
            return None;
        }
        let latest = status.latest_version.clone()?;
        if self.update_dismissed.as_deref() == Some(latest.as_str()) {
            return None;
        }
        let mac_app = matches!(self.install, comet_update::InstallKind::MacApp { .. });

        let (label, clickable): (SharedString, bool) = if mac_app {
            match &self.update_flow {
                UpdateFlow::Idle => (format!("Update available — v{latest}").into(), true),
                UpdateFlow::Downloading => (format!("Downloading v{latest}…").into(), false),
                UpdateFlow::Ready(_) => ("Update ready — restart to apply".into(), true),
                UpdateFlow::Failed => ("Update failed — click to retry".into(), true),
            }
        } else {
            (
                format!("Update available — v{latest} · run `comet update`").into(),
                true,
            )
        };
        let failed = matches!(self.update_flow, UpdateFlow::Failed);
        let tone = if failed { theme.danger } else { theme.accent };
        // The chip fill is the sidebar's WHITE wash language, not an accent
        // tint: an indigo fill over the glass composited into a dark slab that
        // blocked the blur (user report) — the accent lives in the icon/text.
        let (chip_bg, chip_bg_hover) = if failed {
            (theme.danger.opacity(0.14), theme.danger.opacity(0.22))
        } else {
            (crate::theme::wash(0.11), crate::theme::wash(0.16))
        };

        let mut strip = div()
            .id("update-strip")
            .mx(px(Theme::SPACE_SM))
            // No bottom margin: the settings-button block below carries its own
            // SPACE_SM padding — doubling it read as a hole (user report).
            .px(px(Theme::SPACE_SM))
            .py(px(6.0))
            .rounded(px(Theme::CONTROL_RADIUS))
            .bg(chip_bg)
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .text_size(px(11.0))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(tone)
            .child(
                icon(if failed {
                    icons::DANGER_TRIANGLE
                } else {
                    icons::RESTART
                })
                .size(px(14.0))
                .text_color(tone),
            )
            .child(div().flex_1().min_w_0().child(label));
        if clickable {
            strip = strip
                .cursor_pointer()
                .hover(move |s| s.bg(chip_bg_hover))
                .on_click(cx.listener(move |this, _, _, cx| this.on_update_strip_click(cx)));
        }
        Some(strip.into_any_element())
    }

    /// Idle → download; Ready → swap + relaunch; Failed → retry; advisory
    /// installs → dismiss for this version.
    fn on_update_strip_click(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.install, comet_update::InstallKind::MacApp { .. }) {
            self.update_dismissed = self
                .state
                .read(cx)
                .update
                .as_ref()
                .and_then(|s| s.latest_version.clone());
            cx.notify();
            return;
        }
        match std::mem::replace(&mut self.update_flow, UpdateFlow::Idle) {
            UpdateFlow::Idle | UpdateFlow::Failed => self.begin_update_download(cx),
            UpdateFlow::Downloading => self.update_flow = UpdateFlow::Downloading,
            UpdateFlow::Ready(staged) => self.apply_staged_update(staged, cx),
        }
    }

    /// Fetch the manifest and stage the new `Comet.app` under the data dir
    /// (tokio — reqwest); the strip flips to "restart to apply" when done.
    fn begin_update_download(&mut self, cx: &mut Context<Self>) {
        let edge_url = self.boot.releases_url.clone();
        let data_dir = self.data_dir.clone();
        self.update_flow = UpdateFlow::Downloading;
        let download = Tokio::spawn(cx, async move {
            let manifest = comet_update::fetch_latest(&edge_url).await?;
            comet_update::stage_mac_app(&edge_url, &manifest, &data_dir).await
        });
        self.update_task = Some(cx.spawn(async move |this, cx| {
            let outcome = match download.await {
                Ok(Ok(staged)) => Ok(staged),
                Ok(Err(err)) => Err(format!("{err:#}")),
                Err(join_err) => Err(join_err.to_string()),
            };
            this.update(cx, |shell, cx| {
                shell.update_flow = match outcome {
                    Ok(staged) => UpdateFlow::Ready(staged),
                    Err(message) => {
                        tracing::warn!(%message, "update download failed");
                        UpdateFlow::Failed
                    }
                };
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Swap the staged bundle over the installed one, arm the detached
    /// relauncher, and quit — the relauncher `open`s the new bundle once this
    /// process (and its engine lock / IPC port) is gone.
    fn apply_staged_update(&mut self, staged: PathBuf, cx: &mut Context<Self>) {
        let comet_update::InstallKind::MacApp { bundle } = self.install.clone() else {
            return;
        };
        match comet_update::apply_mac_app(&staged, &bundle) {
            Ok(()) => {
                comet_update::relaunch_app_after_exit(&bundle);
                cx.quit();
            }
            Err(err) => {
                tracing::error!(error = %err, "update apply failed");
                self.update_flow = UpdateFlow::Failed;
                cx.notify();
            }
        }
    }

    /// Row metrics track the settings sidebar's Back row
    /// ([`Shell::render_settings_nav`]): the two rows occupy the same corner and
    /// swap when settings open, so a taller row or heavier icon here read as a
    /// jump (user-reported).
    fn render_settings_button(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("sidebar-settings")
            .flex_none()
            .rounded(px(8.0))
            .px(px(Theme::SPACE_SM))
            .py(px(6.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .cursor_pointer()
            .hover(|style| style.bg(theme.element_hover))
            .on_click(cx.listener(|this, _, _, cx| this.open_settings(BOTTOM_SETTINGS_SECTION, cx)))
            .child(
                icon(icons::SETTINGS_MINIMALISTIC)
                    .size(px(16.0))
                    .text_color(theme.text_muted),
            )
            .child(
                div()
                    .text_size(px(13.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text)
                    .child("Settings"),
            )
            .into_any_element()
    }

    /// Floating layers owned by the shell: the session context menu and the
    /// rename / delete-confirm dialogs.
    fn render_overlays(
        &mut self,
        viewport: gpui::Size<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let theme = Theme::of(cx).clone();
        let mut overlays: Vec<AnyElement> = Vec::new();

        if let Some((chat_id, position)) = self.chat_menu.clone() {
            let rename_id = chat_id.clone();
            let archive_id = chat_id.clone();
            let delete_id = chat_id.clone();
            let menu =
                popover::popover_card(&theme)
                    .w(px(170.0))
                    .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                        this.chat_menu = None;
                        cx.notify();
                    }))
                    .flex()
                    .flex_col()
                    .child(
                        popover::menu_row(
                            &theme,
                            false,
                            format!("chat-menu-rename-{}", chat_id.local_id),
                        )
                        .id("chat-menu-rename")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.open_rename_chat(rename_id.clone(), cx)
                        }))
                        .child(icon(icons::PEN).size(px(16.0)).text_color(theme.text_muted))
                        .child(SharedString::from("Rename…")),
                    )
                    .child(
                        popover::menu_row(
                            &theme,
                            false,
                            format!("chat-menu-archive-{}", chat_id.local_id),
                        )
                        .id("chat-menu-archive")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.archive_chat(archive_id.clone(), cx)
                        }))
                        .child(
                            icon(icons::ARCHIVE_MINIMALISTIC)
                                .size(px(16.0))
                                .text_color(theme.text_muted),
                        )
                        .child(SharedString::from("Archive")),
                    )
                    .child(popover::menu_separator())
                    .child(
                        popover::menu_row(
                            &theme,
                            false,
                            format!("chat-menu-delete-{}", chat_id.local_id),
                        )
                        .id("chat-menu-delete")
                        .text_color(theme.danger)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.chat_menu = None;
                            this.delete_confirm = Some(delete_id.clone());
                            cx.notify();
                        }))
                        .child(
                            icon(icons::TRASH_BIN_MINIMALISTIC)
                                .size(px(16.0))
                                .text_color(theme.danger),
                        )
                        .child(SharedString::from("Delete…")),
                    )
                    .into_any_element();
            overlays.push(popover::menu_at("chat-context-menu", position, menu));
        }

        if let Some(dialog) = &mut self.rename_dialog {
            if std::mem::take(&mut dialog.focus_pending) {
                window.focus(&dialog.input.focus_handle(cx), cx);
            }
            let input = dialog.input.clone();
            let card = popover::dialog_card(&theme)
                .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, _, cx| {
                    if ev.keystroke.key == "escape" {
                        this.rename_dialog = None;
                        cx.notify();
                    }
                }))
                .child(popover::dialog_title(&theme, "Rename session"))
                .child(
                    div()
                        .mt(px(12.0))
                        .child(popover::dialog_field(input.into_any_element())),
                )
                .child(
                    div()
                        .mt(px(16.0))
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            popover::btn_ghost(&theme, "Cancel", "rename-chat-cancel")
                                .id("rename-chat-cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.rename_dialog = None;
                                    cx.notify();
                                })),
                        )
                        .child(
                            popover::btn_primary(&theme, "Rename")
                                .id("rename-chat-save")
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.submit_rename_chat(cx)),
                                ),
                        ),
                )
                .into_any_element();
            overlays.push(popover::modal("rename-chat-dialog", viewport, card));
        }

        overlays.extend(self.render_space_overlays(viewport, window, cx));
        if let Some(overlay) = self.render_add_space_overlay(viewport, window, cx) {
            overlays.push(overlay);
        }

        if let Some(chat_id) = self.delete_confirm.clone() {
            let title = transcript::single_line(
                &self
                    .state
                    .read(cx)
                    .chats
                    .iter()
                    .find(|c| c.id == chat_id.local_id)
                    .and_then(|c| c.title.clone())
                    .unwrap_or_else(|| "New session".into()),
            );
            let card = popover::dialog_card(&theme)
                .child(popover::dialog_title(&theme, "Delete session?"))
                .child(div().mt(px(6.0)).child(popover::dialog_body(
                    &theme,
                    format!("\u{201C}{title}\u{201D} will be permanently deleted. This can\u{2019}t be undone."),
                )))
                .child(
                    div()
                        .mt(px(16.0))
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            popover::btn_ghost(&theme, "Cancel", "delete-chat-cancel")
                                .id("delete-chat-cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.delete_confirm = None;
                                    cx.notify();
                                })),
                        )
                        .child(
                            popover::btn_danger(&theme, "Delete")
                                .id("delete-chat-confirm")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.delete_chat(chat_id.clone(), cx)
                                })),
                        ),
                )
                .into_any_element();
            overlays.push(popover::modal("delete-chat-dialog", viewport, card));
        }

        overlays
    }

    fn resize_handle<T>(
        &self,
        id: &'static str,
        marker: fn() -> T,
        reset: fn(&mut Shell, &mut Context<Shell>),
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div>
    where
        T: 'static,
    {
        let hover = Theme::of(cx).border_strong;
        div()
            .id(id)
            .w(px(5.0))
            .h_full()
            .flex_none()
            .cursor_col_resize()
            .hover(move |s| s.bg(hover))
            .on_drag(marker(), |_, _point: Point<gpui::Pixels>, _, cx| {
                cx.stop_propagation();
                cx.new(|_| DragGhost)
            })
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseUpEvent, _, cx| {
                    if event.click_count == 2 {
                        reset(this, cx);
                        this.schedule_save(cx);
                        cx.notify();
                    }
                }),
            )
    }

    fn render_main(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme_owned = Theme::of(cx).clone();
        let theme = &theme_owned;
        let theme_bg = theme.bg;
        let (border, text, faint) = (theme.border, theme.text, theme.text_faint);

        // Settings route: just the section outlet — the section label lives in
        // the unified window titlebar now (render_title_bar).
        if let Route::Settings(section) = self.route {
            let outlet = self.settings_outlet(section, cx);
            return div()
                .flex_1()
                .min_w_0()
                .h_full()
                .flex()
                .flex_col()
                .child(div().flex_1().min_h_0().child(outlet))
                .into_any_element();
        }

        let _ = (text, border);
        let has_selection = self.state.read(cx).selected_chat.is_some();
        let has_spaces = !self.state.read(cx).spaces.is_empty();
        let space_name: SharedString = self
            .state
            .read(cx)
            .selected_space_row()
            .map(|s| s.display_name().to_string())
            .unwrap_or_default()
            .into();

        // Content outlet: selected chat → transcript; nothing selected → the
        // "Send a message to start" canvas with a watermark; no spaces at all
        // → the onboarding card. The composer sits below the first two
        // (new-chat mode mints the chat id on first send).
        let outlet: AnyElement = if has_selection {
            self.transcript.clone().into_any_element()
        } else if !has_spaces {
            // Onboarding (first boot / after the destructive wipe): no folders
            // to work in yet — one clear affordance.
            let _ = faint;
            div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .child(motion::fade_in(
                    "no-spaces-canvas",
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .child(
                            icon(icons::COMET_LOGO)
                                .w(px(41.9))
                                .h(px(48.0))
                                .text_color(theme.text.opacity(0.09)),
                        )
                        .child(
                            div()
                                .mt(px(24.0))
                                .text_size(px(16.0))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(theme.text)
                                .child(SharedString::from("Add a space to get started")),
                        )
                        .child(
                            div()
                                .mt(px(6.0))
                                .text_size(px(13.0))
                                .text_color(theme.text_muted.opacity(0.7))
                                .child(SharedString::from(
                                    "A space is a folder on one of your devices.",
                                )),
                        )
                        .child(
                            popover::btn_primary(&theme_owned, "Add a space")
                                .id("onboarding-add-space")
                                .mt(px(20.0))
                                .on_click(cx.listener(|this, _, _, cx| this.open_add_space(cx))),
                        ),
                ))
                .into_any_element()
        } else {
            // New-chat canvas (comet index.tsx): the dim comet mark watermark
            // (`h-12 text-foreground/[0.09]`) over the centered helper line —
            // now naming the space the session will start in.
            let helper: SharedString = if space_name.is_empty() {
                "Send a message to start a new session.".into()
            } else {
                format!("Send a message to start a session in {space_name}.").into()
            };
            div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .child(motion::fade_in(
                    "new-chat-canvas",
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .child(
                            icon(icons::COMET_LOGO)
                                .w(px(41.9))
                                .h(px(48.0))
                                .text_color(theme.text.opacity(0.09)),
                        )
                        .child(
                            div()
                                .mt(px(24.0))
                                .text_size(px(14.0))
                                .text_color(theme.text_muted.opacity(0.6))
                                .child(helper),
                        ),
                ))
                .into_any_element()
        };

        let status = self.render_status_strip(cx);
        // File dropzone over the ENTIRE conversation column (transcript +
        // composer, not just the pill): dragging OS files anywhere across the
        // chat area shows the "Drop images to attach" veil; a drop stages the
        // files in the composer. `has_active_drag` gates the veil so a drag
        // that left the window (FileDrop Exited) can't strand it.
        let file_drag_active = self.file_drag_active && cx.has_active_drag();
        div()
            .id("chat-dropzone")
            .relative()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .on_drag_move::<gpui::ExternalPaths>(cx.listener(
                |this, e: &gpui::DragMoveEvent<gpui::ExternalPaths>, _, cx| {
                    let inside = e.bounds.contains(&e.event.position);
                    if this.file_drag_active != inside {
                        this.file_drag_active = inside;
                        cx.notify();
                    }
                },
            ))
            .on_drop(cx.listener(|this, paths: &gpui::ExternalPaths, _, cx| {
                this.file_drag_active = false;
                let paths = paths.paths().to_vec();
                this.composer
                    .update(cx, |composer, cx| composer.add_paths(paths, cx));
                cx.notify();
            }))
            .child(
                // The conversation fades out at its bottom edge instead of
                // hard-cutting against the composer — a gradient overlay from
                // transparent into the panel background.
                div()
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .child(outlet)
                    .child(
                        div()
                            .absolute()
                            .bottom_0()
                            .left_0()
                            .right(px(10.0))
                            .h(px(Theme::TRANSCRIPT_FADE_BAND))
                            .bg(gpui::linear_gradient(
                                0.0,
                                gpui::linear_color_stop(theme_bg, 0.0),
                                gpui::linear_color_stop(theme_bg.opacity(0.0), 1.0),
                            )),
                    )
                    .children(self.render_jump_to_bottom(cx)),
            )
            // Reserved status strip (h-6) — the WorkingIndicator lives here so
            // the composer below never shifts. Both live INSIDE the
            // conversation region, ABOVE the terminal dock (comet __root.tsx:
            // the terminal panel sits below the whole conversation column).
            .child(status)
            .when(has_spaces, |el| el.child(self.composer.clone()))
            .child(self.render_terminal_container(cx))
            .when(file_drag_active, |el| {
                el.child(
                    div()
                        .absolute()
                        .inset_0()
                        .bg(theme.scrim().opacity(0.4 / 0.6))
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(13.0))
                        .text_color(theme.text)
                        .child("Drop images to attach"),
                )
            })
            .into_any_element()
    }

    /// The "↓ Scroll to bottom" pill (round-9 §3): a LABELED rounded-full
    /// chip — down-arrow glyph + 13px label on a near-opaque raised surface
    /// with a hairline — horizontally centered over the transcript column and
    /// floating a small gap above the composer. It hangs 14px below the
    /// conversation region (through the reserved h-6 status strip, whose
    /// content is left-aligned) so its bottom edge sits ~10px above the pill.
    /// Shown past the transcript's 320px threshold; 180ms fade + 2px rise in.
    fn render_jump_to_bottom(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.transcript.read(cx).jump_button_shown() {
            return None;
        }
        let theme = Theme::of(cx);
        Some(
            div()
                .absolute()
                .bottom(px(-14.0))
                .left_0()
                .right(px(10.0))
                .flex()
                .justify_center()
                .child(motion::dialog_in(
                    "jump-to-bottom",
                    div()
                        .id("jump-to-bottom-btn")
                        .h(px(30.0))
                        .rounded_full()
                        .border_1()
                        .border_color(theme.border)
                        .shadow_md()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .pl(px(11.0))
                        .pr(px(13.0))
                        .cursor_pointer()
                        // Hover must BRIGHTEN the opaque pill, never replace it
                        // with a translucent wash (a 10%-alpha bg here made the
                        // pill go see-through on hover — user-reported), and it
                        // fades over the CSS transition-colors 150ms, not snaps.
                        .bg(motion::hover_blend(
                            "jump-pill",
                            theme.surface_raised,
                            theme.surface_raised_hover,
                        ))
                        .on_hover(motion::hover_listener("jump-pill"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.transcript
                                .update(cx, |transcript, cx| transcript.jump_to_bottom(cx));
                        }))
                        .child(
                            div()
                                .text_size(px(13.0))
                                .text_color(theme.text_muted)
                                .child(SharedString::from("↓")),
                        )
                        .child(
                            div()
                                .text_size(px(13.0))
                                .text_color(theme.text)
                                .child(SharedString::from("Scroll to bottom")),
                        ),
                ))
                .into_any_element(),
        )
    }

    /// Terminal panel dock at the main-column bottom: a 5px height-drag handle
    /// over the panel, the whole container height-animated 200 ms on toggle.
    fn render_terminal_container(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let target = self.terminal_target(cx);
        let tween = self.terminal_tween;
        if target <= 0.0 && tween.is_none() {
            return gpui::Empty.into_any_element();
        }
        // Defensive: an open flag needs its entity (and set_open) even if
        // toggle_terminal never created one.
        if self.terminal_open(cx) && self.terminal.is_none() {
            let panel = self.terminal_panel(cx);
            panel.update(cx, |panel, cx| panel.set_open(true, cx));
        }
        let Some(panel) = self.terminal.clone() else {
            return gpui::Empty.into_any_element();
        };
        let border = Theme::of(cx).border;
        let handle_hover = Theme::of(cx).border_strong;
        let height = self.settings.terminal_height;

        let handle = div()
            .id("terminal-resize")
            .h(px(5.0))
            .w_full()
            .flex_none()
            .cursor_row_resize()
            .hover(move |s| s.bg(handle_hover))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseDownEvent, _, _| {
                    this.terminal_drag_anchor =
                        Some((f32::from(event.position.y), this.settings.terminal_height));
                }),
            )
            .on_drag(TerminalResize, |_, _point: Point<gpui::Pixels>, _, cx| {
                cx.stop_propagation();
                cx.new(|_| DragGhost)
            })
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, event: &MouseUpEvent, _, cx| {
                    if event.click_count == 2 {
                        this.settings.terminal_height = TERMINAL_DEFAULT_HEIGHT;
                        this.schedule_save(cx);
                        cx.notify();
                    }
                }),
            );

        // Fixed-height inner clipped by the animated container: content never
        // reflows mid-transition (same trick as the side panes).
        let inner = div()
            .h(px(height))
            .w_full()
            .flex()
            .flex_col()
            .child(handle)
            .child(div().flex_1().min_h_0().child(panel));

        div()
            .w_full()
            .flex_none()
            .overflow_hidden()
            .border_t_1()
            .border_color(border)
            .h(px(self.eval_tween(tween, target)))
            .child(inner)
            .into_any_element()
    }

    /// Working indicator strip: gradient spinner + rotating flavour word (7s,
    /// seeded per chat) + elapsed, staleness-gated via [`Indicator`]; falls back
    /// to a "Sending…" bridge and then the engine mode line.
    fn render_status_strip(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let now = Utc::now();
        let state = self.state.read(cx);

        // Aligned with the composer column: centered, same max width, small
        // inner gutter (comet's `mx-auto h-6 max-w-3xl px-2`). A closure, not
        // a value: `Div` isn't `Clone` at the pinned gpui rev, and both a
        // normal indicator arm and the blocked-turn line below need their own
        // fresh strip with identical geometry.
        let strip = || {
            div()
                .h(px(Theme::STATUS_STRIP_HEIGHT))
                .flex_none()
                .w_full()
                .max_w(px(768.0))
                .mx_auto()
                .flex()
                .items_center()
                .gap(px(Theme::SPACE_SM))
                .px(px(Theme::SPACE_LG + 8.0))
                .text_size(px(11.0))
        };

        let Some(chat_id) = state.selected_chat.clone().map(|id| id.local_id) else {
            return strip().into_any_element();
        };
        let indicator = state.indicator_for(&chat_id, now);
        let elapsed_secs = state
            .session_for(&chat_id)
            .and_then(|s| s.started_at)
            .map(|t| now.signed_duration_since(t).num_seconds())
            .unwrap_or(0);
        let sending = self.composer.read(cx).is_sending();
        // Last read of `state` (borrowed from `cx`) in this call — everything
        // after this point only touches `self`'s own fields and `cx` mutably,
        // both of which are disjoint from it.
        let blocked = crate::approvals::blocked_on(&state.transcript);

        // Both indicator arms below consult this SAME helper. An approval
        // sets the session to `AwaitingInput` (sessions.rs:1310), so an
        // arm-specific implementation would go silent at exactly the moment
        // the wait begins — see `crate::approvals` for the shared rule this
        // closes (a tool call that never returns has no timeout and no
        // recovery, same as an approval no one has answered).
        let blocked = crate::approvals::blocked_line(
            &mut self.blocked_stamp,
            blocked,
            std::time::Instant::now(),
        );
        let blocked_strip = blocked.map(|line| {
            let stoppable = line.stoppable;
            let elapsed = transcript::format_elapsed(line.elapsed_secs);
            strip()
                .child(loaders::gradient_spinner(
                    "blocked-indicator",
                    &theme,
                    2.5,
                    cx.entity_id(),
                    cx,
                ))
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(px(12.0))
                        .text_color(theme.text_muted)
                        .child(line.text),
                )
                .child(
                    div()
                        .flex_none()
                        .text_color(theme.text_faint)
                        .child(SharedString::from(elapsed)),
                )
                .when(stoppable, |el| {
                    el.child(
                        // The only cancellation Comet has is turn-level: no
                        // provider exposes a per-call cancel, and the command
                        // queue has one `interrupt`. So the line NAMES the
                        // call and Stop ends the turn — the honest
                        // affordance, not a per-call one that doesn't exist.
                        div()
                            .id("blocked-stop")
                            .flex_none()
                            .px(px(6.0))
                            .rounded(px(6.0))
                            .cursor_pointer()
                            .text_color(theme.text_muted)
                            .hover(|s| s.text_color(theme.text))
                            .child(SharedString::from("Stop"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.composer
                                    .update(cx, |composer, cx| composer.interrupt(cx));
                            })),
                    )
                })
                .into_any_element()
        });

        match indicator {
            Indicator::Working => {
                if let Some(blocked) = blocked_strip {
                    return blocked;
                }
                let word =
                    transcript::flavour_word(transcript::flavour_seed(&chat_id), elapsed_secs);
                strip()
                    .child(loaders::gradient_spinner(
                        "working-indicator",
                        &theme,
                        2.5,
                        cx.entity_id(),
                        cx,
                    ))
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(format!("{word}…"))),
                    )
                    .child(
                        div()
                            .text_color(theme.text_faint)
                            .child(SharedString::from(transcript::format_elapsed(elapsed_secs))),
                    )
                    .into_any_element()
            }
            // The QuestionPanel right below IS the awaiting-input surface — a
            // strip caption above it was redundant (user request) — but an
            // APPROVAL wait carries elapsed time the panel does not show, and
            // that is exactly the information a long wait must surface.
            Indicator::AwaitingInput => blocked_strip.unwrap_or_else(|| strip().into_any_element()),
            Indicator::Errored => strip()
                .text_color(theme.danger)
                .child(SharedString::from("Run failed"))
                .into_any_element(),
            Indicator::None if sending => strip()
                .child(loaders::gradient_spinner(
                    "sending-indicator",
                    &theme,
                    2.5,
                    cx.entity_id(),
                    cx,
                ))
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(theme.text_muted)
                        .child(SharedString::from("Sending…")),
                )
                .into_any_element(),
            Indicator::None => strip().into_any_element(),
        }
    }

    /// Right "Changes" pane — hidden by default, drag-resizable; content is the
    /// lazy [`Changes`] diff viewer (created on first open).
    fn render_right_pane(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let bg = theme.bg;
        let content: AnyElement = if self.right_pane_open(cx) {
            let changes = self.changes_pane(cx);
            // Idempotent — also covers a persisted-open pane on boot.
            changes.update(cx, |changes, cx| changes.ensure_watch(cx));
            changes.into_any_element()
        } else {
            gpui::Empty.into_any_element()
        };
        // Its OWN inset card (user request): the conversation card's right
        // gutter is the gap; padding (not margins) keeps the tweened width
        // container clean. The resize grabber lives outside this clipped
        // container, on the seam assembled by the root layout.
        let card = div()
            .size_full()
            .rounded(px(12.0))
            .border_1()
            .border_color(theme.border)
            .bg(bg)
            .overflow_hidden()
            .child(content);
        let target = self.right_target(cx);
        self.pane_container(
            self.right_tween,
            target,
            // Mirrors the conversation card's box exactly: flush under the
            // titlebar (no top pad), 8px bottom/right gutters — the
            // conversation card's own right margin is the 8px gap between the
            // two insets (user-reported height/gap mismatch).
            div()
                .h_full()
                .relative()
                .pb(px(8.0))
                .pr(px(8.0))
                .child(card)
                .into_any_element(),
        )
    }

    fn render_gate_card(&mut self, phase: &GatePhase, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let content: AnyElement = match phase {
            // Backend unreachable: quiet centered copy (comet Gate `Failed`),
            // plus a Retry affordance (the native engine doesn't self-redial).
            GatePhase::Failed(error) => div()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(Theme::SPACE_MD))
                .child(
                    div()
                        .text_size(px(14.0))
                        .text_color(theme.text_muted)
                        .child(SharedString::from(error.clone())),
                )
                .child(
                    div()
                        .id("retry-engine")
                        .px(px(12.0))
                        .py(px(6.0))
                        .rounded(px(8.0))
                        .border_1()
                        .border_color(theme.border)
                        .text_size(px(13.0))
                        .text_color(theme.text)
                        .cursor_pointer()
                        .hover(|s| s.bg(theme.glass_hover()))
                        .on_click(cx.listener(|this, _, _, cx| this.retry_engine(cx)))
                        .child(SharedString::from("Retry")),
                )
                .into_any_element(),
            _ => return Empty.into_any_element(),
        };
        div()
            .size_full()
            .relative()
            .bg(theme.bg)
            .child(grid_backdrop(&theme))
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    // Keyed per phase (comet App.tsx `<div key={phase}
                    // className="animate-in">`): every gate swap replays the
                    // 0.5s entrance instead of mutating one animated element.
                    .child(motion::fade_in(
                        match phase {
                            _ => "gate-card-failed",
                        },
                        div().child(content),
                    )),
            )
            .into_any_element()
    }
}

/// The sign-in gate's faint grid backdrop (comet styles.css `.bg-grid`):
/// 44px hairlines at white 3.5%, with the radial mask approximated by edge
/// gradients back into the page background (gpui has no mask-image).
fn grid_backdrop(theme: &Theme) -> AnyElement {
    let line = crate::theme::hairline(0.035);
    let bg = theme.bg;
    const STEP: f32 = 44.0;
    const SPAN: f32 = 2640.0;
    let verticals = (1..(SPAN / STEP) as usize).map(|i| {
        div()
            .absolute()
            .left(px(i as f32 * STEP))
            .top_0()
            .bottom_0()
            .w(px(1.0))
            .bg(line)
    });
    let horizontals = (1..((SPAN * 0.75) / STEP) as usize).map(|i| {
        div()
            .absolute()
            .top(px(i as f32 * STEP))
            .left_0()
            .right_0()
            .h(px(1.0))
            .bg(line)
    });
    div()
        .absolute()
        .inset_0()
        .overflow_hidden()
        .children(verticals)
        .children(horizontals)
        // Mask approximation: fade the grid back into the background toward
        // the window edges (the original masks to an ellipse at 50% / 40%).
        .child(
            div()
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .h(px(120.0))
                .bg(gpui::linear_gradient(
                    180.0,
                    gpui::linear_color_stop(bg, 0.0),
                    gpui::linear_color_stop(bg.opacity(0.0), 1.0),
                )),
        )
        .child(
            div()
                .absolute()
                .bottom_0()
                .left_0()
                .right_0()
                .h(px(260.0))
                .bg(gpui::linear_gradient(
                    0.0,
                    gpui::linear_color_stop(bg, 0.0),
                    gpui::linear_color_stop(bg.opacity(0.0), 1.0),
                )),
        )
        .child(
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .left_0()
                .w(px(200.0))
                .bg(gpui::linear_gradient(
                    90.0,
                    gpui::linear_color_stop(bg, 0.0),
                    gpui::linear_color_stop(bg.opacity(0.0), 1.0),
                )),
        )
        .child(
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .right_0()
                .w(px(200.0))
                .bg(gpui::linear_gradient(
                    270.0,
                    gpui::linear_color_stop(bg, 0.0),
                    gpui::linear_color_stop(bg.opacity(0.0), 1.0),
                )),
        )
        .into_any_element()
}

/// A size-6 icon button for the titlebar strip (comet window-controls.tsx:
/// `grid size-6 place-items-center rounded-md text-muted-foreground`).
fn window_control_button(
    id: &'static str,
    icon_path: &'static str,
    theme: &Theme,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let muted = theme.text_muted;
    let fade_key = format!("window-control-{id}");
    div()
        .id(id)
        .size(px(24.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.0))
        .cursor_pointer()
        // comet window-controls.tsx: `transition-colors` — the wash fades.
        .bg(motion::hover_blend(
            &fade_key,
            theme.glass_hover().opacity(0.0),
            theme.glass_hover(),
        ))
        .on_hover(motion::hover_listener(fade_key))
        // Buttons in/over a titlebar drag strip must be EXCLUDED from the
        // strip's event surface entirely. `.occlude()` (gpui
        // `HitboxBehavior::BlockMouse`) makes the window hit-test STOP at the
        // button, so every `is_hovered`-guarded strip listener — the
        // mouse-down that arms the drag, the mouse-move that hands AppKit a
        // native drag session (`performWindowDragWithEvent:`, whose second
        // quick click zooms NATIVELY on macOS), and the `click_count == 2`
        // zoom handler — never fires with the pointer over a button. It also
        // removes the button's rect from the native Drag control-area
        // hit-test on Windows/Linux. The click-level stop_propagation is
        // zed's ButtonLike belt on top. Double-click on EMPTY strip space
        // still zooms — nothing occludes it there.
        .occlude()
        .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
        .on_click(move |event, window, cx| {
            cx.stop_propagation();
            on_click(event, window, cx)
        })
        .child(icon(icon_path).size(px(16.0)).text_color(muted))
}

#[cfg(target_os = "windows")]
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

#[cfg(target_os = "windows")]
impl WindowsCaptionButton {
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
}

#[cfg(target_os = "windows")]
fn render_windows_caption_button(button: WindowsCaptionButton, theme: &Theme) -> AnyElement {
    let (hover_bg, hover_fg, active_bg, active_fg) = match button {
        WindowsCaptionButton::Close => {
            let color: gpui::Hsla = gpui::Rgba {
                r: 232.0 / 255.0,
                g: 17.0 / 255.0,
                b: 32.0 / 255.0,
                a: 1.0,
            }
            .into();
            (
                color,
                gpui::white(),
                color.opacity(0.8),
                gpui::white().opacity(0.8),
            )
        }
        _ => (
            theme.glass_hover(),
            theme.text,
            theme.glass_hover().opacity(0.8),
            theme.text,
        ),
    };

    div()
        .id(button.id())
        .w(px(WINDOWS_CAPTION_BUTTON_WIDTH))
        .h(px(Theme::TITLEBAR_HEIGHT))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(10.0))
        .text_color(theme.text)
        .hover(|style| style.bg(hover_bg).text_color(hover_fg))
        .active(|style| style.bg(active_bg).text_color(active_fg))
        .occlude()
        .window_control_area(button.control_area())
        .child(button.glyph())
        .into_any_element()
}

/// A titlebar history button (comet window-controls.tsx): enabled it is a
/// normal window-control button; disabled it dims to 35% opacity and ignores
/// the pointer (`disabled:pointer-events-none disabled:opacity-35`).
fn nav_history_button(
    id: &'static str,
    icon_path: &'static str,
    enabled: bool,
    theme: &Theme,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    if !enabled {
        return div()
            .size(px(24.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            // Even disabled it reads as a control — occlude so double-clicks
            // on it don't fall through to the titlebar strip's zoom handler.
            .occlude()
            .child(
                icon(icon_path)
                    .size(px(16.0))
                    .text_color(theme.text_muted.opacity(0.35)),
            )
            .into_any_element();
    }
    window_control_button(id, icon_path, theme, on_click).into_any_element()
}

/// A size-7 icon button for the main-panel header (comet __root.tsx:
/// `grid size-7 place-items-center rounded-md text-muted-foreground`).
fn header_icon_button(
    id: &'static str,
    icon_path: &'static str,
    theme: &Theme,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let muted = theme.text_muted;
    let fade_key = format!("header-icon-{id}");
    div()
        .id(id)
        .size(px(28.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.0))
        .cursor_pointer()
        // comet __root.tsx header buttons: `transition-colors`.
        .bg(motion::hover_blend(
            &fade_key,
            crate::theme::wash(0.0),
            crate::theme::wash(0.11),
        ))
        .on_hover(motion::hover_listener(fade_key))
        // Same occlusion + click-swallowing as [`window_control_button`]: this
        // button sits inside the chat header's titlebar drag region, so its
        // rect must be carved out of the strip's drag/double-click surface.
        .occlude()
        .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
        .on_click(move |event, window, cx| {
            cx.stop_propagation();
            on_click(event, window, cx)
        })
        .child(icon(icon_path).size(px(16.0)).text_color(muted))
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.viewport_width = f32::from(window.viewport_size().width);
        let theme = Theme::of(cx);
        // The shell tone (comet `.frost`): the surface the sidebar sits on and
        // the main panel floats over as an inset rounded card. On macOS the
        // window background is the blurred desktop (lib.rs `Blurred`), so the
        // frost paints translucent — the sidebar and card margins read as
        // glass while the opaque card keeps text off it.
        let (frost, text, font) = (theme.glass(), theme.text, theme.font_sans.clone());
        let gate = self
            .debug_gate
            .clone()
            .unwrap_or_else(|| self.state.read(cx).gate());

        // Fullscreen hides the macOS traffic lights — reflow the control
        // cluster with a 200ms ease-out tween (§1.1). A fullscreen transition
        // resizes the window, which re-renders us, so polling here is exact.
        let fullscreen = window.is_fullscreen();
        if self.fullscreen != Some(fullscreen) {
            if self.fullscreen.is_some() && cfg!(target_os = "macos") {
                self.titlebar_tween = Some(WidthTween::new(
                    titlebar_cluster_start(!fullscreen),
                    titlebar_cluster_start(fullscreen),
                ));
            }
            self.fullscreen = Some(fullscreen);
        }
        // Manual tween drive bookkeeping for this pass (see [`WidthTween`]).
        self.reduced_motion = motion::reduced_motion(cx);
        self.motion_active.set(false);

        // Keyboard shortcuts (mod-s/b/j) dispatch through the window focus
        // chain — with nothing focused they go dead. Land initial focus on the
        // route's fallback, and whenever focus is lost with no successor (e.g.
        // the focused element unmounted), route it back there.
        if self.activation_sub.is_none() {
            self.activation_sub = Some(cx.observe_window_activation(
                window,
                |this: &mut Shell, window, cx| {
                    if !window.is_window_active() {
                        this.set_jump_hints(false, cx);
                    }
                },
            ));
        }

        if self.focus_sub.is_none() {
            self.focus_sub = Some(cx.on_focus_lost(window, |this: &mut Shell, window, cx| {
                match shell_focus_fallback(this.route, false, false) {
                    Some(ShellFocusFallback::Composer) => {
                        window.focus(&this.composer.focus_handle(cx), cx)
                    }
                    Some(ShellFocusFallback::ShellRoot) => window.focus(&this.shell_focus, cx),
                    None => {}
                }
            }));
        }
        let has_focused_node = window.focused(cx).is_some();
        let shell_root_is_focused = self.shell_focus.is_focused(window);
        if matches!(gate, GatePhase::Ready)
            && let Some(fallback) =
                shell_focus_fallback(self.route, has_focused_node, shell_root_is_focused)
        {
            match fallback {
                ShellFocusFallback::Composer => window.focus(&self.composer.focus_handle(cx), cx),
                ShellFocusFallback::ShellRoot => window.focus(&self.shell_focus, cx),
            }
        }
        // Opening a search result hands focus back to the composer, matching
        // what selecting a session anywhere else leaves you with — otherwise
        // focus stays in the search field and the first thing typed at the
        // session you just opened goes to search instead. Deferred to here
        // because those handlers have no `window`.
        if std::mem::take(&mut self.composer_focus_pending) && matches!(self.route, Route::Chat) {
            window.focus(&self.composer.focus_handle(cx), cx);
        }

        let root = div()
            .id("shell-root")
            .track_focus(&self.shell_focus)
            .relative()
            .flex()
            .flex_row()
            .size_full()
            .bg(frost)
            .text_color(text)
            .font_family(font)
            .text_size(px(14.0))
            // Escape clears the sidebar query from anywhere — the column's own
            // `search_key` only sees keys dispatched through the sidebar, and
            // focus leaves it as soon as you click the transcript.
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                this.search_escape_key(event, cx)
            }))
            .on_drag_move(cx.listener(Self::on_sidebar_drag))
            .on_drag_move(cx.listener(Self::on_right_pane_drag))
            .on_drag_move(cx.listener(Self::on_terminal_drag))
            // The panel shortcuts are chat-scoped chrome: in Settings they are
            // no-ops (comet __root.tsx gates the hotkey on `!isSettings`, and
            // the terminal panel is only mounted on session routes). The
            // sidebar toggle stays live everywhere, as in the original.
            .on_action(cx.listener(|this, _: &ToggleTerminal, window, cx| {
                if matches!(this.route, Route::Chat) {
                    this.toggle_terminal(window, cx)
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| this.toggle_sidebar(cx)))
            .on_action(cx.listener(|this, _: &FocusSearch, window, cx| {
                if matches!(this.route, Route::Chat) {
                    this.focus_search(window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &NextSession, _, cx| {
                this.cycle_session(true, cx);
            }))
            .on_action(cx.listener(|this, _: &PrevSession, _, cx| {
                this.cycle_session(false, cx);
            }))
            // Chat-scoped like the panel toggles: Settings has no current
            // session to archive.
            .on_action(cx.listener(|this, _: &ArchiveSession, _, cx| {
                if matches!(this.route, Route::Chat) {
                    this.archive_selected_chat(cx)
                }
            }))
            // A jump routes back to chat itself, so Settings is not a dead
            // spot — the same call a click on that sidebar row makes.
            .on_action(
                cx.listener(|this, jump: &JumpSession, _, cx| this.jump_to_session(jump.0, cx)),
            )
            .on_modifiers_changed(
                cx.listener(|this, event, _, cx| this.on_modifiers_changed(event, cx)),
            )
            .on_action(cx.listener(|this, _: &NewSession, window, cx| {
                let origin = this.route;
                this.set_route(Route::Chat, cx);
                let target = this.start_session_from_sidebar(window, cx);
                if let Some(entry) = new_session_nav_entry(origin, &target, &this.active_chat) {
                    // Direct selection also reaches `on_state_changed`; push
                    // deduplication makes that later observation a no-op.
                    this.nav.push(entry);
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleChanges, _, cx| {
                if matches!(this.route, Route::Chat) {
                    this.toggle_right_pane(cx)
                }
            }))
            .on_action(cx.listener(|this, _: &AddSpacePalette, _, cx| {
                if this.add_space.is_some() {
                    this.add_space = None;
                    cx.notify();
                } else {
                    this.open_add_space(cx);
                }
            }));

        let root = match &gate {
            GatePhase::Ready => {
                // A run finishing while you're LOOKING at the session must not
                // badge "completed" until you leave and return — mark it seen
                // live while the window is active (idempotent guard inside;
                // one extra frame settles it).
                if window.is_window_active() {
                    let unseen_selected = {
                        let s = self.state.read(cx);
                        s.selected_chat_row()
                            .filter(|c| c.unseen())
                            .map(|c| c.id.clone())
                    };
                    if let Some(chat_id) = unseen_selected {
                        self.state
                            .update(cx, |s, cx| s.mark_chat_seen(&chat_id, cx));
                    }
                }
                // Capture knob: `COMET_OPEN_DIALOG=model` pops the combined
                // harness/model menu (needs `window`, so it fires here rather
                // than in `on_state_changed`).
                if self.debug_dialog.as_deref() == Some("model") {
                    self.debug_dialog = None;
                    self.composer
                        .update(cx, |c, cx| c.debug_open_model_menu(window, cx));
                }
                // MessageRail width gate: hide below 48rem of main-panel width.
                let viewport = f32::from(window.viewport_size().width);
                let main_width = viewport - self.sidebar_target() - self.right_target(cx) - 10.0;
                self.transcript.update(cx, |t, cx| {
                    t.set_rail_enabled(rail::rail_visible(main_width), cx)
                });

                let sidebar = self.render_sidebar(window, cx);
                let sidebar_handle = self.resize_handle(
                    "sidebar-resize",
                    || SidebarResize,
                    |shell, _| shell.settings.sidebar_width = SIDEBAR_DEFAULT,
                    cx,
                );
                let main = self.render_main(cx);
                // The Changes pane is chat-scoped chrome: the Settings route
                // never renders it (comet __root.tsx `!isSettings && activeChat`
                // around the diff column) — the per-session open flags stay
                // intact for the return trip.
                let on_chat = matches!(self.route, Route::Chat);
                let right_open = on_chat && self.right_pane_open(cx);
                let right_handle = right_open.then(|| {
                    self.resize_handle(
                        "right-pane-resize",
                        || RightPaneResize,
                        |shell, _| shell.settings.right_pane_width = RIGHT_PANE_DEFAULT,
                        cx,
                    )
                    // A forgiving transparent hit target centered on the
                    // seam; the card's 1px border remains the visual divider.
                    .w(px(12.0))
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left(px(-6.0))
                });
                let right: AnyElement = if on_chat {
                    self.render_right_pane(cx)
                } else {
                    Empty.into_any_element()
                };
                let overlays = self.render_overlays(window.viewport_size(), window, cx);
                // The signature frame: the conversation card and — when the
                // changes pane is open — a SECOND inset card beside it, both
                // rounded hairline-bordered floats on the frost shell (the
                // changes card is built inside `render_right_pane`).
                let theme = Theme::of(cx);
                // Margins, radius, and border-color MELT over the same 200ms
                // ease-out as the sidebar width (comet __root.tsx `<main>`
                // `transition-[margin,border-radius,border-color]`; collapsed
                // is `m-0 rounded-none border-transparent` — the border WIDTH
                // stays, only its color fades, so layout never jumps by the
                // hairline).
                let border_color = theme.border;
                let card = div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_row()
                    .overflow_hidden()
                    .bg(theme.bg)
                    .border_1()
                    .child(main);
                // Manual drive on the SAME clock as the sidebar width tween.
                // Crucially there is no `with_animation` wrapper here: the
                // wrapper's epoch-keyed id used to change every card
                // descendant's global element-id path on each toggle, which
                // reset gpui's per-element animation state and REPLAYED any
                // stale pane/terminal tween from t=0 (the changes pane slid
                // ~100px under the clip mid-toggle — round-6 §2/§3).
                //
                // The inset card persists in EVERY state (user request): top
                // gutter under the unified titlebar, constant left/right/
                // bottom gutters, constant radius + hairline — the 8px left
                // gap holds whether it borders the sidebar or the window edge.
                // No top margin: the titlebar's own internal air (44px bar,
                // 28px tabs) is the gap — an extra gutter read as a hole
                // between the header and the app (user report).
                // The right margin is the window gutter when the changes
                // pane is closed, but the SEAM between the two inset cards
                // when it's open — a full gutter there read double-wide next
                // to the two borders it separates (user report).
                let right_gap = if on_chat && self.right_pane_open(cx) {
                    4.0
                } else {
                    8.0
                };
                let card: AnyElement = card
                    .mb(px(8.0))
                    .mr(px(right_gap))
                    .ml(px(8.0))
                    .rounded(px(12.0))
                    .border_color(border_color)
                    .into_any_element();
                // The whole app page is one keyed `animate-in` entrance (comet
                // App.tsx `<div key={phase} className="animate-in h-full">`):
                // arriving from the splash or any gate fades the page in; the
                // splash-out crossfades over it on boot.
                // The sidebar resize handle FLOATS over the sidebar/card seam
                // (zero layout width, same idiom as the changes-pane grabber)
                // so the sidebar's right gutter stays exactly as wide as its
                // left one — a 5px flex child here read as lopsided spacing.
                let sidebar_seam = div()
                    .w(px(0.0))
                    .h_full()
                    .flex_none()
                    .relative()
                    .child(sidebar_handle.absolute().top_0().bottom_0().left(px(-2.0)));
                // Keep the right resize target outside the pane's
                // overflow-hidden width container. This mirrors the sidebar
                // seam and lets the target straddle both adjacent panes.
                let right_seam: AnyElement = if let Some(handle) = right_handle {
                    div()
                        .w(px(0.0))
                        .h_full()
                        .flex_none()
                        .relative()
                        .child(handle)
                        .into_any_element()
                } else {
                    Empty.into_any_element()
                };
                let title_bar = self.render_title_bar(cx);
                // Sidebar tone: a slightly lighter column behind the sidebar,
                // spanning the FULL window height (under the traffic lights,
                // through the titlebar, down to the bottom edge). Its width
                // rides the same tween as the sidebar, so the tone melts away
                // with the collapse instead of vanishing in a frame.
                let sidebar_now = self.eval_tween(self.sidebar_tween, self.sidebar_target());
                // Hairline on its right edge — full height like the tone,
                // so the sidebar column reads as its own surface.
                let sidebar_tone = div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left_0()
                    .w(px(sidebar_now))
                    .bg(crate::theme::wash(0.05))
                    .border_r_1()
                    .border_color(border_color);
                let page = div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .child(title_bar)
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .flex()
                            .flex_row()
                            .child(sidebar)
                            .child(sidebar_seam)
                            .child(card)
                            .child(right_seam)
                            .child(right),
                    )
                    .child(self.render_titlebar_cluster(cx))
                    .children(self.render_windows_caption_controls(window, cx))
                    .children(overlays);
                root.child(sidebar_tone)
                    .child(motion::fade_in("phase-app", page))
            }
            GatePhase::Loading => root, // splash overlay covers boot
            phase @ GatePhase::Failed(_) => {
                let card = self.render_gate_card(phase, cx);
                root.child(card)
            }
        };

        // A manually-driven tween is mid-flight: keep frames coming (the same
        // scheduling `with_animation` would have requested). Hover color fades
        // ride the same clock; their once-per-frame tick lives here (this is
        // the window's root render — it runs exactly once per frame).
        if self.motion_active.get() | motion::hover_fades_active() {
            window.request_animation_frame();
        }

        // Slow-request toast, above the app and below the splash.
        //
        // Nothing fires an event when a request crosses `SLOW_AFTER` — it is a
        // clock, so while anything is in flight we keep frames coming and look.
        // Requests normally finish in milliseconds, so this costs nothing in
        // the common case, and once the toast is up its spinner drives frames
        // anyway.
        if crate::toast::any_in_flight(cx) {
            window.request_animation_frame();
        }
        // `COMET_SLOW_TOAST_DEMO=1` pins it open with a stand-in request, for
        // reviewing the design without waiting for something to actually hang.
        let slow = crate::toast::slow(cx).or(self.slow_request_demo.map(|cancellable| {
            crate::toast::SlowRequest {
                id: 0,
                what: crate::errors::Loading::Models,
                cancellable,
            }
        }));
        let root = if let Some(slow) = slow {
            let request_id = slow.id;
            let theme = Theme::of(cx).clone();
            let view = cx.entity_id();
            // Absent for a wait that revalidates rows already on screen: there
            // is nothing for a Cancel to change, so none is offered.
            let cancel = slow.cancellable.then(|| {
                crate::toast::cancel_link(&theme).on_click(cx.listener(
                    move |shell, _, _, cx: &mut Context<Self>| {
                        crate::toast::cancel(cx, request_id);
                        shell.slow_request_demo = None;
                        cx.notify();
                    },
                ))
            });
            // The LIVE sidebar width, from the same tween the column itself
            // rides, so the toast tracks a collapse rather than snapping when
            // it lands.
            let left_inset = self.eval_tween(self.sidebar_tween, self.sidebar_target());
            root.child(crate::toast::slow_request_toast(
                &theme,
                crate::toast::waiting_message(slow.what),
                cancel,
                left_inset,
                view,
                cx,
            ))
        } else {
            root
        };

        // Boot splash overlay: visible → crossfades out on Ready → removed.
        match self.splash {
            SplashPhase::Visible => {
                let theme = Theme::of(cx).clone();
                let view = cx.entity_id();
                root.child(loaders::splash_overlay(&theme, false, view, cx))
            }
            SplashPhase::FadingOut => {
                let theme = Theme::of(cx).clone();
                let view = cx.entity_id();
                root.child(loaders::splash_overlay(&theme, true, view, cx))
            }
            SplashPhase::Gone => root,
        }
    }
}

#[cfg(test)]
mod tests {

    /// The rule the strip hangs on, without needing a window to check it.
    #[test]
    fn an_identity_rebuild_announces_once_per_stamp() {
        assert_eq!(identity_notice_stamp(None, None), None, "nothing to say");
        assert_eq!(
            identity_notice_stamp(None, Some("2026-08-30T09:00:00Z")),
            None,
            "a stale dismissal on an engine reporting nothing says nothing"
        );
        assert_eq!(
            identity_notice_stamp(Some("2026-08-30T09:00:00Z"), None),
            Some("2026-08-30T09:00:00Z"),
            "an undismissed rebuild is announced"
        );
        assert_eq!(
            identity_notice_stamp(Some("2026-08-30T09:00:00Z"), Some("2026-08-30T09:00:00Z")),
            None,
            "and stays dismissed across launches — the marker is permanent"
        );
        assert_eq!(
            identity_notice_stamp(Some("2026-09-01T11:00:00Z"), Some("2026-08-30T09:00:00Z")),
            Some("2026-09-01T11:00:00Z"),
            "a SECOND rebuild is its own announcement"
        );
    }

    /// Break caught twice while writing user copy, and shipped once before
    /// that (`normalize::session_update_once`): a backslash-continued string
    /// literal keeps its continuation indentation as REAL SPACES inside the
    /// sentence, and rustfmt rejoins the source so the run is invisible where
    /// it is written. Checks the shape rather than the wording, so rewording
    /// the notice does not need this test updated.
    #[test]
    fn the_identity_notice_copy_has_no_stray_runs_of_spaces() {
        let copy = Shell::IDENTITY_REBUILT_NOTICE;
        assert!(
            !copy.contains("  "),
            "a run of spaces means a continued literal, not a sentence: {copy:?}"
        );
        assert!(
            !copy.chars().any(char::is_control),
            "the strip is one line: {copy:?}"
        );
        assert!(
            copy.starts_with("Add older spaces"),
            "the action leads, per the user-facing-errors rule: {copy:?}"
        );
    }

    /// Break caught (D95): `Keystroke::parse` rejects an unknown MODIFIER and
    /// nothing else, so every combo below used to pass the guard and bind a
    /// shortcut that could never fire — silently, and without the fallback
    /// this function's name promises. #105 widened the exposure to nine
    /// `jumpSession` slots.
    ///
    /// The two empty-key cases are the ones a reader doubts: `ctrl-` splits
    /// into `["ctrl", ""]` and `""` into `[""]`, and `parse` stores the empty
    /// string as the key in both.
    #[test]
    fn a_combo_naming_a_key_no_platform_emits_falls_back_to_the_default() {
        for combo in ["ctrl-nosuchkey", "zzz", "ctrl-", "", "mod-notakey"] {
            assert!(
                Keystroke::parse(&platform_combo(combo)).is_ok(),
                "{combo:?} must still PARSE, or this test is proving something else"
            );
            assert_eq!(
                valid_or_default(combo, "mod-s"),
                platform_combo("mod-s"),
                "{combo:?} names no emittable key and must fall back"
            );
        }
    }

    /// The other direction, so the guard cannot be "reject everything": every
    /// shape a real keymap uses survives untouched.
    #[test]
    fn every_shape_a_real_keymap_uses_survives_the_guard() {
        for combo in [
            "mod-s",
            "mod-shift-a",
            "ctrl-tab",
            "ctrl-shift-tab",
            "mod-1",
            "f5",
            "ctrl-f13",
            "mod-[",
            "alt-pageup",
            "mod-enter",
            "shift-escape",
        ] {
            assert_eq!(
                valid_or_default(combo, "mod-s"),
                platform_combo(combo),
                "{combo:?} is a legitimate binding and must be kept as written"
            );
        }
    }

    /// Every default this file ships has to survive its own guard — a fallback
    /// that is itself rejected would leave the shortcut bound to nothing, and
    /// the guard would be the thing that broke it.
    #[test]
    fn every_shipped_default_combo_passes_the_guard() {
        for id in ShortcutId::ALL {
            let default = id.default_combo();
            if default.is_empty() {
                continue;
            }
            assert_eq!(
                valid_or_default(default, default),
                platform_combo(default),
                "{id:?}'s own default must be emittable"
            );
        }
    }

    use super::*;

    fn space_ids(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|id| (*id).to_string()).collect()
    }

    #[test]
    fn new_session_target_picks_the_scoped_space() {
        assert_eq!(
            new_session_target(&SidebarScope::Space("s2".into()), &space_ids(&["s1", "s2"])),
            NewSessionTarget::Space("s2".into()),
            "a scoped sidebar already answers the question the picker would ask"
        );
    }

    #[test]
    fn jump_slots_bind_mod_digits_and_an_empty_combo_binds_nothing() {
        let bindings = shell_key_bindings(&KeymapConfig::default());
        let jumps: Vec<_> = bindings
            .iter()
            .filter(|binding| binding.action().name() == JumpSession(0).name())
            .collect();
        assert_eq!(jumps.len(), JUMP_SLOTS, "one binding per slot");
        assert_eq!(
            jumps[0]
                .keystrokes()
                .iter()
                .map(|key| key.inner().clone())
                .collect::<Vec<_>>(),
            vec![Keystroke::parse(&platform_combo("mod-1")).unwrap()]
        );

        // A slot cleared in a hand-edited file binds nothing rather than
        // falling back to its default — the user cleared it on purpose.
        let mut cleared = KeymapConfig::default();
        cleared.set(ShortcutId::JumpSession(3), String::new());
        let bindings = shell_key_bindings(&cleared);
        let jumps = bindings
            .iter()
            .filter(|binding| binding.action().name() == JumpSession(0).name())
            .count();
        assert_eq!(jumps, JUMP_SLOTS - 1);
    }

    #[test]
    fn shell_key_bindings_include_new_session() {
        let bindings = shell_key_bindings(&KeymapConfig::default());
        let binding = bindings
            .iter()
            .find(|binding| binding.action().name() == NewSession.name())
            .expect("NewSession binding");
        assert_eq!(
            binding
                .keystrokes()
                .iter()
                .map(|key| key.inner().clone())
                .collect::<Vec<_>>(),
            vec![Keystroke::parse(&platform_combo("mod-n")).unwrap()]
        );
    }

    #[test]
    fn shell_focus_fallback_routes_chat_to_composer_and_settings_to_shell_root() {
        assert_eq!(
            shell_focus_fallback(Route::Chat, false, false),
            Some(ShellFocusFallback::Composer)
        );
        assert_eq!(
            shell_focus_fallback(Route::Chat, true, true),
            Some(ShellFocusFallback::Composer)
        );
        assert_eq!(shell_focus_fallback(Route::Chat, true, false), None);
        for section in SettingsSection::ALL {
            assert_eq!(
                shell_focus_fallback(Route::Settings(section), false, false),
                Some(ShellFocusFallback::ShellRoot)
            );
            assert_eq!(
                shell_focus_fallback(Route::Settings(section), true, true),
                None
            );
            assert_eq!(
                shell_focus_fallback(Route::Settings(section), true, false),
                None
            );
        }
    }

    #[test]
    fn new_session_target_ignores_a_scope_the_space_list_no_longer_contains() {
        // `heal_sidebar_scope` normally clears this, but the two are read from
        // the same frame: the scope wins, and the space id it names is used
        // verbatim rather than falling through to the picker.
        assert_eq!(
            new_session_target(
                &SidebarScope::Space("gone".into()),
                &space_ids(&["s1", "s2"])
            ),
            NewSessionTarget::Space("gone".into())
        );
    }

    #[test]
    fn new_session_target_routes_to_add_space_when_there_are_none() {
        assert_eq!(
            new_session_target(&SidebarScope::All, &[]),
            NewSessionTarget::AddSpaceFirst,
            "a disabled button helps nobody — send them to the thing they need"
        );
    }

    #[test]
    fn new_session_target_skips_a_one_item_picker() {
        assert_eq!(
            new_session_target(&SidebarScope::All, &space_ids(&["s1"])),
            NewSessionTarget::Space("s1".into()),
            "a menu with one entry is a click for nothing"
        );
    }

    #[test]
    fn new_session_target_prompts_across_all_spaces_in_display_order() {
        assert_eq!(
            new_session_target(&SidebarScope::All, &space_ids(&["s3", "s1", "s2"])),
            NewSessionTarget::Pick(vec!["s3".into(), "s1".into(), "s2".into()]),
            "the picker offers every space, in the order the sidebar shows them"
        );
    }

    /// §1.6's FLIP precondition: a session row is a UNIFORM height within a
    /// scope, and `chat_row_height` reports exactly what `render_chat_row`
    /// lays out. Both now read the same `chat_row` metrics, so a row that
    /// carries no branch and no harness mark (config frame not landed) can no
    /// longer come out 14px short of what `resort_offsets` was told.
    #[test]
    fn chat_row_height_is_the_sum_of_the_metrics_the_row_is_built_from() {
        let all = RowScope::All {
            space: "comet".into(),
            device: "mac-studio".into(),
            host_offline: false,
        };
        assert_eq!(chat_row_height(&all), 60.0);
        assert_eq!(chat_row_height(&RowScope::One), 44.0);
        assert_eq!(
            chat_row_height(&all) - chat_row_height(&RowScope::One),
            chat_row::SPACE_LINE + chat_row::SPACE_LINE_MB,
            "the only difference between the scopes is line 0"
        );
    }

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

    #[test]
    fn titlebar_cluster_matches_comet_window_controls() {
        // comet window-controls.tsx: `left: fullscreen ? 12 : 88` — the
        // cluster clears the {14,15} traffic lights, and reclaims the inset
        // when fullscreen hides them.
        assert_eq!(titlebar_cluster_start(false), 88.0);
        assert_eq!(titlebar_cluster_start(true), 12.0);
    }

    #[test]
    fn titlebar_spacer_selects_per_platform_and_fullscreen() {
        // macOS, lights visible: spacer fills up to the 88px cluster start.
        assert_eq!(titlebar_spacer_width(true, false, 10.0), 78.0);
        assert_eq!(titlebar_spacer_width(true, false, 12.0), 76.0);
        assert_eq!(titlebar_spacer_width(true, false, 26.0), 62.0);
        // macOS fullscreen: the inset animates away (clamped at zero when the
        // strip's own padding already exceeds the 12px cluster start).
        assert_eq!(titlebar_spacer_width(true, true, 10.0), 2.0);
        assert_eq!(titlebar_spacer_width(true, true, 26.0), 0.0);
        // Linux / Windows: never any inset.
        assert_eq!(titlebar_spacer_width(false, false, 10.0), 0.0);
        assert_eq!(titlebar_spacer_width(false, true, 10.0), 0.0);
    }

    #[test]
    fn cluster_clearance_clears_the_overlay_buttons() {
        // Linux: buttons at 10..86; a 16px-padded header needs 78 more px to
        // put content at 86 + 8 breathing room.
        assert_eq!(cluster_clearance(false, false, 16.0), 78.0);
        assert_eq!(cluster_clearance(false, false, 10.0), 84.0);
        // macOS: buttons start at the 88px traffic-light cluster start.
        assert_eq!(
            cluster_clearance(true, false, 16.0),
            88.0 + 76.0 + 8.0 - 16.0
        );
        // macOS fullscreen: cluster reclaims the inset (starts at 12).
        assert_eq!(
            cluster_clearance(true, true, 16.0),
            12.0 + 76.0 + 8.0 - 16.0
        );
    }

    // ---- per-session panel flags (§1.10/1.11 parity: comet sessionPanels) ----

    #[test]
    fn session_panels_default_closed_per_chat() {
        let panels = SessionPanels::default();
        assert_eq!(panels.get("a"), ChatPanels::default());
        assert!(!panels.get("a").terminal_open);
        assert!(!panels.get("a").changes_open);
        // The new-chat canvas ("" key) is its own session, also closed.
        assert!(!panels.get("").terminal_open);
    }

    #[test]
    fn session_panels_flags_are_chat_scoped() {
        let mut panels = SessionPanels::default();
        // Opening the terminal in chat A opens it ONLY in chat A.
        assert!(panels.toggle_terminal("a"));
        assert!(panels.get("a").terminal_open);
        assert!(!panels.get("b").terminal_open);
        assert!(!panels.get("").terminal_open);
        // Changes pane in B is independent of A's terminal.
        assert!(panels.toggle_changes("b"));
        assert!(panels.get("b").changes_open);
        assert!(!panels.get("b").terminal_open);
        assert!(!panels.get("a").changes_open);
        // Switching back to A restores A's state untouched.
        assert!(panels.get("a").terminal_open);
        // Toggling off round-trips.
        assert!(!panels.toggle_terminal("a"));
        assert!(!panels.get("a").terminal_open);
    }

    #[test]
    fn session_panels_both_flags_coexist_per_chat() {
        let mut panels = SessionPanels::default();
        panels.toggle_terminal("a");
        panels.toggle_changes("a");
        assert_eq!(
            panels.get("a"),
            ChatPanels {
                terminal_open: true,
                changes_open: true
            }
        );
        assert_eq!(panels.get("b"), ChatPanels::default());
    }

    // ---- sidebar resort FLIP diff (§1.6) ----

    fn keys(list: &[(&str, f32)]) -> Vec<(String, f32)> {
        list.iter().map(|(k, h)| (k.to_string(), *h)).collect()
    }

    fn all_scope() -> RowScope {
        RowScope::All {
            space: "comet".into(),
            device: "mac-studio".into(),
            host_offline: false,
        }
    }

    #[test]
    fn chat_row_height_adds_exactly_the_space_line() {
        // Two lines inside one space: py5 + title 18 + mt2 + branch 14 + py5.
        assert_eq!(chat_row_height(&RowScope::One), 44.0);
        // The all-spaces form is the same row plus the 14px space line and its
        // 2px margin — nothing else may differ, or the two scopes have drifted
        // apart and the row is no longer one design.
        assert_eq!(
            chat_row_height(&all_scope()) - chat_row_height(&RowScope::One),
            16.0
        );
        // The device text does not change the height; it truncates instead.
        assert_eq!(
            chat_row_height(&RowScope::All {
                space: "a-very-long-space-name-indeed".into(),
                device: "a-very-long-device-name".into(),
                host_offline: true,
            }),
            chat_row_height(&all_scope())
        );
    }

    /// The spec's highest-risk item: `CHAT_ROW_HEIGHT` stops being a constant, and
    /// `resort_offsets` must be fed the same heights the render pass used. If a
    /// caller ever passes a stale height, surviving rows glide to the wrong y.
    #[test]
    fn resort_offsets_track_the_scope_height_change() {
        // Same three rows, same order, but the scope narrowed: every row shrank
        // 60 -> 44, so each one after the first has genuinely moved up.
        let wide = keys(&[("a", 60.0), ("b", 60.0), ("c", 60.0)]);
        let narrow = keys(&[("a", 44.0), ("b", 44.0), ("c", 44.0)]);
        let offsets = resort_offsets(&wide, &narrow, SIDEBAR_LIST_GAP);

        assert!(!offsets.contains_key("a"), "the first row does not move");
        assert_eq!(offsets.get("b").copied(), Some(16.0));
        assert_eq!(offsets.get("c").copied(), Some(32.0));

        // And feeding it the OLD height for the new pass produces no offsets at
        // all — the silent-wrong-glide failure mode, asserted so it stays visible.
        assert!(resort_offsets(&wide, &wide, SIDEBAR_LIST_GAP).is_empty());
    }

    #[test]
    fn resort_offsets_empty_when_order_unchanged() {
        let order = keys(&[("a", 29.0), ("b", 29.0), ("c", 45.0)]);
        assert!(resort_offsets(&order, &order, 2.0).is_empty());
    }

    #[test]
    fn resort_offsets_activity_moves_row_to_top() {
        // c (bottom, y=62) jumps to top: c glides down-from-above? No — c's
        // old y is 62, new y is 0 → starts +62 below… offset = old - new = +62,
        // painted at +62 decaying to 0 (a glide UP into place). a and b shift
        // down by c's height + gap (31).
        let old = keys(&[("a", 29.0), ("b", 29.0), ("c", 29.0)]);
        let new = keys(&[("c", 29.0), ("a", 29.0), ("b", 29.0)]);
        let offsets = resort_offsets(&old, &new, 2.0);
        assert_eq!(offsets.get("c"), Some(&62.0));
        assert_eq!(offsets.get("a"), Some(&-31.0));
        assert_eq!(offsets.get("b"), Some(&-31.0));
    }

    #[test]
    fn resort_offsets_respect_heights_and_gap() {
        // Tall row (45px) swaps with a short one (29px).
        let old = keys(&[("tall", 45.0), ("short", 29.0)]);
        let new = keys(&[("short", 29.0), ("tall", 45.0)]);
        let offsets = resort_offsets(&old, &new, 2.0);
        // short: old y 47 → new y 0; tall: old y 0 → new y 31.
        assert_eq!(offsets.get("short"), Some(&47.0));
        assert_eq!(offsets.get("tall"), Some(&-31.0));
    }

    #[test]
    fn resort_offsets_ignore_added_and_removed_keys() {
        let old = keys(&[("a", 29.0), ("gone", 29.0), ("b", 29.0)]);
        let new = keys(&[("new", 29.0), ("a", 29.0), ("b", 29.0)]);
        let offsets = resort_offsets(&old, &new, 2.0);
        // "new" has no old position (fades in instead); "gone" just goes.
        assert!(!offsets.contains_key("new"));
        assert!(!offsets.contains_key("gone"));
        // a: old 0 → new 31 (pushed down by the insert); b: 62 → 62 (gone's
        // slot replaced by "new" of equal height — no move, no entry).
        assert_eq!(offsets.get("a"), Some(&-31.0));
        assert_eq!(offsets.get("b"), None);
    }

    #[test]
    fn resort_glide_spec_matches_original() {
        // §1.6: 260ms cubic-bezier(0.22, 1, 0.36, 1).
        assert_eq!(RESORT.duration_ms, 260);
        assert_eq!(RESORT.curve, motion::EASE_RESORT);
    }

    // ---- navigation history (titlebar back/forward) ----

    fn chat(id: &str) -> NavEntry {
        NavEntry::Chat(id.to_string())
    }

    #[test]
    fn nav_history_starts_with_nothing_to_walk() {
        let nav = NavHistory::new(chat(""));
        assert!(!nav.can_back());
        assert!(!nav.can_forward());
        assert_eq!(*nav.current(), chat(""));
    }

    #[test]
    fn nav_push_then_back_and_forward() {
        let mut nav = NavHistory::new(chat("a"));
        nav.push(chat("b"));
        nav.push(NavEntry::Settings(SettingsSection::Devices));
        assert!(nav.can_back());
        assert!(!nav.can_forward());

        // Back walks toward the oldest entry without dropping anything.
        assert_eq!(
            nav.back(),
            Some(chat("b")),
            "back lands on the previous route"
        );
        assert_eq!(nav.back(), Some(chat("a")));
        assert!(!nav.can_back());
        assert!(nav.can_forward());
        assert_eq!(nav.back(), None, "past the oldest entry is a no-op");

        // Forward retraces the same path.
        assert_eq!(nav.forward(), Some(chat("b")));
        assert_eq!(
            nav.forward(),
            Some(NavEntry::Settings(SettingsSection::Devices))
        );
        assert!(!nav.can_forward());
        assert_eq!(nav.forward(), None);
    }

    #[test]
    fn nav_push_dedups_the_current_route() {
        let mut nav = NavHistory::new(chat("a"));
        nav.push(chat("a"));
        nav.push(chat("a"));
        assert_eq!(nav.len(), 1, "re-selecting the current route never stacks");
        nav.push(NavEntry::Settings(SettingsSection::Agents));
        nav.push(NavEntry::Settings(SettingsSection::Agents));
        assert_eq!(nav.len(), 2);
    }

    #[test]
    fn nav_push_truncates_the_forward_branch() {
        // a → b → c, back to a, then push d: the b/c branch is gone (browser
        // semantics — comet's memory history PUSH truncates entries ahead).
        let mut nav = NavHistory::new(chat("a"));
        nav.push(chat("b"));
        nav.push(chat("c"));
        nav.back();
        nav.back();
        assert_eq!(*nav.current(), chat("a"));
        assert!(nav.can_forward());
        nav.push(chat("d"));
        assert!(!nav.can_forward(), "the old branch is unreachable");
        assert_eq!(nav.len(), 2);
        assert_eq!(nav.back(), Some(chat("a")));
        assert_eq!(nav.forward(), Some(chat("d")));
    }

    #[test]
    fn nav_replace_swaps_in_place() {
        // The boot auto-select replaces the untouched canvas entry, so Back
        // stays disabled after landing in the last-used chat.
        let mut nav = NavHistory::new(chat(""));
        nav.replace(chat("boot"));
        assert_eq!(nav.len(), 1);
        assert_eq!(*nav.current(), chat("boot"));
        assert!(!nav.can_back());
    }

    #[test]
    fn nav_settings_sections_are_distinct_entries() {
        let mut nav = NavHistory::new(chat("a"));
        nav.push(NavEntry::Settings(SettingsSection::Devices));
        nav.push(NavEntry::Settings(SettingsSection::Shortcuts));
        assert_eq!(nav.len(), 3, "section changes are navigations");
        assert_eq!(
            nav.back(),
            Some(NavEntry::Settings(SettingsSection::Devices))
        );
        assert_eq!(nav.back(), Some(chat("a")));
    }

    #[test]
    fn settings_new_session_records_the_chat_route_before_cancel_or_selection() {
        let settings = Route::Settings(SettingsSection::Devices);

        let direct = new_session_nav_entry(
            settings,
            &NewSessionTarget::Space("space-a".into()),
            "existing-chat",
        );
        assert_eq!(direct, Some(chat("")));
        let mut direct_nav = NavHistory::new(chat("existing-chat"));
        direct_nav.push(NavEntry::Settings(SettingsSection::Devices));
        direct_nav.push(direct.expect("direct target records the blank canvas"));
        let after_action = direct_nav.len();
        direct_nav.push(chat(""));
        assert_eq!(
            direct_nav.len(),
            after_action,
            "the later selected-session observer dedups the same blank canvas"
        );
        assert_eq!(
            direct_nav.back(),
            Some(NavEntry::Settings(SettingsSection::Devices))
        );

        for pending in [
            NewSessionTarget::Pick(vec!["space-a".into(), "space-b".into()]),
            NewSessionTarget::AddSpaceFirst,
        ] {
            let mut nav = NavHistory::new(chat("existing-chat"));
            nav.push(NavEntry::Settings(SettingsSection::Devices));
            nav.push(
                new_session_nav_entry(settings, &pending, "existing-chat")
                    .expect("leaving Settings records the visible Chat route"),
            );

            assert_eq!(*nav.current(), chat("existing-chat"));
            assert_eq!(
                nav.back(),
                Some(NavEntry::Settings(SettingsSection::Devices)),
                "cancelling the pending flow leaves Back pointing at Settings"
            );
        }

        assert_eq!(
            new_session_nav_entry(
                Route::Chat,
                &NewSessionTarget::Pick(vec!["space-a".into(), "space-b".into()]),
                "existing-chat",
            ),
            None,
            "Chat-origin actions already have coherent navigation history"
        );
    }

    /// Session-nav shortcuts must go quiet under EVERY shell-owned overlay,
    /// not just the add-space palette. `overlay_owns_keyboard` shipped naming
    /// one of the eight surfaces `overlay_open` already lists, so with
    /// "Delete session?" up, ⌘1 switched session and ⌘⇧A archived one out
    /// from under the dialog — which then deleted the chat it had captured.
    ///
    /// Pinned by source scan for the reason `new_session_action_…` below is:
    /// `overlay_owns_keyboard` takes `&App`, and `crates/ui` has no gpui test
    /// context, so the guard cannot be called here. What matters is not the
    /// boolean but that the two lists CANNOT drift, so that is what is pinned.
    #[test]
    fn the_keyboard_guard_defers_to_the_one_overlay_list() {
        let source = include_str!("shell.rs");
        let production_source = source
            .split_once("#[cfg(test)]")
            .expect("shell test-module boundary")
            .0;

        let guard = production_source
            .split_once("pub(super) fn overlay_owns_keyboard")
            .expect("overlay_owns_keyboard")
            .1
            .split_once("\n    }")
            .expect("end of overlay_owns_keyboard")
            .0;
        assert!(
            guard.contains("self.overlay_open()"),
            "overlay_owns_keyboard must defer to overlay_open, never re-list \
             the shell surfaces: a second hand-written list is what let a jump \
             fire under the delete-confirm dialog"
        );

        // And the list it defers to must still cover the surfaces that make
        // that dangerous — a confirm dialog holds a ServerRef captured when it
        // opened, so a jump underneath it retargets what the user is looking
        // at without retargeting what Enter will do.
        let list = production_source
            .split_once("fn overlay_open")
            .expect("overlay_open")
            .1
            .split_once("\n    }")
            .expect("end of overlay_open")
            .0;
        for field in [
            "chat_menu",
            "rename_dialog",
            "delete_confirm",
            "space_menu",
            "rename_space_dialog",
            "delete_space_confirm",
            "add_space",
            "space_dropdown_open",
        ] {
            assert!(
                list.contains(field),
                "overlay_open must still name `{field}`; session-nav shortcuts \
                 are gated on this list"
            );
        }
    }

    #[test]
    fn new_session_action_wires_the_target_aware_history_entry() {
        let source = include_str!("shell.rs");
        let production_source = source
            .split_once("#[cfg(test)]")
            .expect("shell test-module boundary")
            .0;
        let listener = production_source
            .split_once(".on_action(cx.listener(|this, _: &NewSession")
            .expect("NewSession action listener")
            .1
            .split_once(".on_action(cx.listener(|this, _: &ToggleChanges")
            .expect("next shell action listener")
            .0;

        assert!(
            listener.contains("new_session_nav_entry(origin, &target, &this.active_chat)"),
            "the production listener must compute the Settings-to-Chat history entry"
        );
        assert!(
            listener.contains("this.nav.push(entry)"),
            "the production listener must record the computed history entry"
        );
    }

    #[test]
    fn remote_connections_is_a_first_class_settings_section() {
        assert!(SettingsSection::ALL.contains(&SettingsSection::RemoteConnections));
        assert_eq!(SettingsSection::RemoteConnections.label(), "Remote");
    }

    #[test]
    fn bottom_settings_button_rendering_targets_devices_without_account_menu() {
        assert_eq!(BOTTOM_SETTINGS_SECTION, SettingsSection::Devices);
        assert_eq!(BOTTOM_SETTINGS_SECTION.label(), "Devices");

        let source = include_str!("shell.rs");
        let production_source = source
            .split_once("#[cfg(test)]")
            .expect("shell test-module boundary")
            .0;
        let settings_renderer = production_source
            .split_once("fn render_settings_button")
            .expect("settings button renderer")
            .1
            .split_once("\n    fn ")
            .expect("next shell method")
            .0;
        let listener_token = ["this.", "open_settings", "(BOTTOM_SETTINGS_SECTION, cx)"].concat();

        assert!(
            production_source.contains(".child(settings_button)"),
            "the rendered sidebar must include the bottom Settings button"
        );
        assert!(settings_renderer.contains(".id(\"sidebar-settings\")"));
        assert!(
            settings_renderer.contains(&listener_token),
            "the actual click listener must open the configured settings section"
        );
        for obsolete in [
            "user_menu_open",
            "user_menu_dismissed_at",
            "render_user_menu",
            "user-menu-settings",
            "Sign out",
            "Alpha",
        ] {
            assert!(
                !production_source.contains(obsolete),
                "obsolete account-menu token remains: {obsolete}"
            );
        }
    }
}
