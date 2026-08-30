//! UI settings persisted to a small JSON file in the data dir — pane widths and
//! collapse flags (comet persisted the same set in localStorage).
//!
//! Loaded once at boot; saved debounced by the shell ([`SAVE_DEBOUNCE_MS`]).
//! Corrupt or missing files fall back to defaults; loaded values are clamped so a
//! hand-edited file can't wedge the layout.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub mod accounts;
pub mod appearance;
pub mod archived;
pub mod composer;
pub mod devices;
pub mod shortcuts;
pub mod widgets;

/// Sidebar drag-resize bounds (px).
pub const SIDEBAR_MIN: f32 = 208.0;
pub const SIDEBAR_MAX: f32 = 400.0;
pub const SIDEBAR_DEFAULT: f32 = 256.0;

/// Right ("Changes") pane drag-resize floor and default (px). Its runtime
/// maximum is the window space remaining after the left sidebar.
pub const RIGHT_PANE_MIN: f32 = 360.0;
pub const RIGHT_PANE_DEFAULT: f32 = 520.0;

/// Terminal panel height bounds: 160px … 55% of the viewport (§1.10). The
/// viewport-relative cap applies at runtime; the absolute cap here only heals
/// hand-edited files.
pub const TERMINAL_MIN_HEIGHT: f32 = 160.0;
pub const TERMINAL_MAX_VH: f32 = 0.55;
pub const TERMINAL_ABS_MAX_HEIGHT: f32 = 2000.0;
pub const TERMINAL_DEFAULT_HEIGHT: f32 = 280.0;

/// Debounce for settings writes after a drag/toggle.
pub const SAVE_DEBOUNCE_MS: u64 = 400;

const FILE_NAME: &str = "ui-settings.json";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct UiSettings {
    pub sidebar_width: f32,
    pub sidebar_collapsed: bool,
    /// Legacy: the grouped-by-project toggle predates spaces (which group by
    /// folder inherently). Kept for file compatibility; no longer read.
    pub sidebar_grouped: bool,
    /// The last selected space — restored on boot when the row still exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_space_id: Option<String>,
    /// Manual session-tab order per space (drag-reorder; device-local).
    /// Missing chats are skipped; new chats append in creation order.
    #[serde(skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub tab_order: std::collections::HashMap<String, Vec<String>>,
    /// Manual sidebar space order (drag-reorder; device-local). Missing spaces
    /// are skipped; new spaces append in creation order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub space_order: Vec<String>,
    /// Sidebar session scope (device-local). `None` = All spaces, the default;
    /// `Some(id)` = scoped to that space. Healed to `None` on load when the
    /// space no longer exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sidebar_scope_space: Option<String>,
    /// Session notification chimes (done / awaiting-input). `COMET_DISABLE_SOUND`
    /// overrides.
    pub sound_enabled: bool,
    /// The identity-rebuild stamp the user has already dismissed (D96).
    ///
    /// Persisted rather than in-memory like the update strip's dismissal,
    /// because the two states differ in how long they last: an update goes
    /// away when you install it, while a rebuilt identity is permanent — the
    /// engine reports the same stamp on every launch forever, so a per-boot
    /// dismissal would nag for the life of the installation. Keyed on the
    /// stamp itself so a SECOND rebuild is announced again rather than being
    /// swallowed by the first dismissal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dismissed_identity_rebuild: Option<String>,
    pub right_pane_width: f32,
    /// Legacy: panel *open* flags are session-scoped in-memory state now
    /// (`shell::SessionPanels`, comet `sessionPanels` parity). Kept for file
    /// compatibility; no longer read or written by the shell.
    pub right_pane_open: bool,
    pub terminal_height: f32,
    /// Legacy — see [`Self::right_pane_open`].
    pub terminal_open: bool,
    /// Customizable shortcut combos (feature-inventory §1.4).
    pub keymap: KeymapConfig,
    /// Light/dark preference. Defaults to following the OS.
    pub appearance: crate::appearance::AppearanceMode,
}

impl Default for UiSettings {
    fn default() -> Self {
        Self {
            sidebar_width: SIDEBAR_DEFAULT,
            sidebar_collapsed: false,
            sidebar_grouped: false,
            last_space_id: None,
            tab_order: std::collections::HashMap::new(),
            space_order: Vec::new(),
            sidebar_scope_space: None,
            sound_enabled: true,
            dismissed_identity_rebuild: None,
            right_pane_width: RIGHT_PANE_DEFAULT,
            right_pane_open: false,
            terminal_height: TERMINAL_DEFAULT_HEIGHT,
            terminal_open: false,
            keymap: KeymapConfig::default(),
            appearance: crate::appearance::AppearanceMode::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// Keymap (customizable shortcuts, §1.4)
// ---------------------------------------------------------------------------

/// How many sidebar rows the jump shortcuts reach (t3code's
/// `THREAD_JUMP_KEYBINDING_COMMANDS`, nine slots).
pub const JUMP_SLOTS: usize = 9;

/// Default combo per jump slot, and the label the shortcuts table shows.
const JUMP_DEFAULTS: [&str; JUMP_SLOTS] = [
    "mod-1", "mod-2", "mod-3", "mod-4", "mod-5", "mod-6", "mod-7", "mod-8", "mod-9",
];
const JUMP_LABELS: [&str; JUMP_SLOTS] = [
    "Jump to session 1",
    "Jump to session 2",
    "Jump to session 3",
    "Jump to session 4",
    "Jump to session 5",
    "Jump to session 6",
    "Jump to session 7",
    "Jump to session 8",
    "Jump to session 9",
];

/// The rebindable app shortcuts. `JumpSession(slot)` is zero-based; a slot at
/// or past [`JUMP_SLOTS`] has no combo and no label, so it reads as unbound
/// rather than panicking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ShortcutId {
    ToggleSidebar,
    ToggleChanges,
    ToggleTerminal,
    FocusSearch,
    NewSession,
    NextSession,
    PrevSession,
    ArchiveSession,
    JumpSession(usize),
}

impl ShortcutId {
    pub const ALL: [ShortcutId; 8 + JUMP_SLOTS] = [
        ShortcutId::ToggleSidebar,
        ShortcutId::ToggleChanges,
        ShortcutId::ToggleTerminal,
        ShortcutId::FocusSearch,
        ShortcutId::NewSession,
        ShortcutId::NextSession,
        ShortcutId::PrevSession,
        ShortcutId::ArchiveSession,
        ShortcutId::JumpSession(0),
        ShortcutId::JumpSession(1),
        ShortcutId::JumpSession(2),
        ShortcutId::JumpSession(3),
        ShortcutId::JumpSession(4),
        ShortcutId::JumpSession(5),
        ShortcutId::JumpSession(6),
        ShortcutId::JumpSession(7),
        ShortcutId::JumpSession(8),
    ];

    /// Row label (comet lib/shortcuts.ts `SHORTCUT_DEFINITIONS`, verbatim).
    pub fn label(self) -> &'static str {
        match self {
            ShortcutId::ToggleSidebar => "Toggle left sidebar",
            ShortcutId::ToggleChanges => "Toggle right sidebar",
            ShortcutId::ToggleTerminal => "Toggle terminal",
            ShortcutId::FocusSearch => "Focus sidebar search",
            ShortcutId::NewSession => "New session",
            ShortcutId::NextSession => "Next session",
            ShortcutId::PrevSession => "Previous session",
            ShortcutId::ArchiveSession => "Archive session",
            ShortcutId::JumpSession(slot) => JUMP_LABELS.get(slot).copied().unwrap_or(""),
        }
    }

    pub fn default_combo(self) -> &'static str {
        match self {
            ShortcutId::ToggleSidebar => "mod-s",
            ShortcutId::ToggleChanges => "mod-b",
            ShortcutId::ToggleTerminal => "mod-j",
            ShortcutId::FocusSearch => "mod-p",
            ShortcutId::NewSession => "mod-n",
            // Deliberately ctrl-, not mod-: on macOS "mod" becomes cmd and
            // cmd-tab is the OS application switcher, which never reaches us.
            ShortcutId::NextSession => "ctrl-tab",
            ShortcutId::PrevSession => "ctrl-shift-tab",
            // Mod+A is the composer's Select all, so archiving takes the
            // shifted combo.
            ShortcutId::ArchiveSession => "mod-shift-a",
            ShortcutId::JumpSession(slot) => JUMP_DEFAULTS.get(slot).copied().unwrap_or(""),
        }
    }

    /// The sidebar row this id jumps to, if it is a jump shortcut.
    pub fn jump_slot(self) -> Option<usize> {
        match self {
            ShortcutId::JumpSession(slot) if slot < JUMP_SLOTS => Some(slot),
            _ => None,
        }
    }
}

/// Persisted shortcut combos. Stored platform-neutral ("mod-s"); translated to
/// "cmd-s"/"ctrl-s" at bind time by [`platform_combo`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct KeymapConfig {
    pub toggle_sidebar: String,
    pub toggle_changes: String,
    pub toggle_terminal: String,
    pub focus_search: String,
    pub new_session: String,
    pub next_session: String,
    pub prev_session: String,
    pub archive_session: String,
    /// One combo per jump slot, in slot order. A list rather than nine fields:
    /// [`UiSettings::load`] discards the WHOLE file on a parse error, so a
    /// fixed-length array would let one malformed entry reset every unrelated
    /// setting. [`Self::heal_jump_slots`] restores the length instead.
    pub jump_session: Vec<String>,
}

impl Default for KeymapConfig {
    fn default() -> Self {
        Self {
            toggle_sidebar: ShortcutId::ToggleSidebar.default_combo().into(),
            toggle_changes: ShortcutId::ToggleChanges.default_combo().into(),
            toggle_terminal: ShortcutId::ToggleTerminal.default_combo().into(),
            focus_search: ShortcutId::FocusSearch.default_combo().into(),
            new_session: ShortcutId::NewSession.default_combo().into(),
            next_session: ShortcutId::NextSession.default_combo().into(),
            prev_session: ShortcutId::PrevSession.default_combo().into(),
            archive_session: ShortcutId::ArchiveSession.default_combo().into(),
            jump_session: JUMP_DEFAULTS.iter().map(|c| (*c).to_string()).collect(),
        }
    }
}

impl KeymapConfig {
    pub fn get(&self, id: ShortcutId) -> &str {
        match id {
            ShortcutId::ToggleSidebar => &self.toggle_sidebar,
            ShortcutId::ToggleChanges => &self.toggle_changes,
            ShortcutId::ToggleTerminal => &self.toggle_terminal,
            ShortcutId::FocusSearch => &self.focus_search,
            ShortcutId::NewSession => &self.new_session,
            ShortcutId::NextSession => &self.next_session,
            ShortcutId::PrevSession => &self.prev_session,
            ShortcutId::ArchiveSession => &self.archive_session,
            ShortcutId::JumpSession(slot) => self
                .jump_session
                .get(slot)
                .map(String::as_str)
                .unwrap_or(""),
        }
    }

    pub fn set(&mut self, id: ShortcutId, combo: String) {
        match id {
            ShortcutId::ToggleSidebar => self.toggle_sidebar = combo,
            ShortcutId::ToggleChanges => self.toggle_changes = combo,
            ShortcutId::ToggleTerminal => self.toggle_terminal = combo,
            ShortcutId::FocusSearch => self.focus_search = combo,
            ShortcutId::NewSession => self.new_session = combo,
            ShortcutId::NextSession => self.next_session = combo,
            ShortcutId::PrevSession => self.prev_session = combo,
            ShortcutId::ArchiveSession => self.archive_session = combo,
            ShortcutId::JumpSession(slot) => {
                if slot < JUMP_SLOTS {
                    if self.jump_session.len() < JUMP_SLOTS {
                        self.heal_jump_slots();
                    }
                    self.jump_session[slot] = combo;
                }
            }
        }
    }

    pub fn reset(&mut self, id: ShortcutId) {
        self.set(id, id.default_combo().to_string());
    }

    /// Restore the jump list to exactly [`JUMP_SLOTS`] entries: a hand-edited
    /// or older file may carry a short, long or absent list. Surviving entries
    /// keep their slot; missing ones take the default.
    pub fn heal_jump_slots(&mut self) {
        self.jump_session.truncate(JUMP_SLOTS);
        while self.jump_session.len() < JUMP_SLOTS {
            self.jump_session
                .push(JUMP_DEFAULTS[self.jump_session.len()].to_string());
        }
    }
}

/// Build a combo string from a recorded keystroke. The primary modifier
/// (cmd on macOS, ctrl elsewhere — either recorded key maps in) becomes "mod";
/// bare modifier presses record nothing.
pub fn combo_from_keystroke(
    ctrl: bool,
    alt: bool,
    shift: bool,
    cmd: bool,
    key: &str,
) -> Option<String> {
    let key = key.trim().to_lowercase();
    if key.is_empty()
        || matches!(
            key.as_str(),
            "ctrl" | "control" | "alt" | "shift" | "cmd" | "platform" | "fn"
        )
    {
        return None;
    }
    let mut parts: Vec<&str> = Vec::new();
    if ctrl || cmd {
        parts.push("mod");
    }
    if alt {
        parts.push("alt");
    }
    if shift {
        parts.push("shift");
    }
    parts.push(&key);
    Some(parts.join("-"))
}

/// Shortcut ids whose combos collide with another shortcut (conflict detection).
pub fn conflicted_shortcuts(keymap: &KeymapConfig) -> Vec<ShortcutId> {
    ShortcutId::ALL
        .into_iter()
        .filter(|&id| {
            let combo = keymap.get(id);
            !combo.is_empty()
                && ShortcutId::ALL
                    .into_iter()
                    .any(|other| other != id && keymap.get(other) == combo)
        })
        .collect()
}

/// The modifiers a stored combo carries, as `(mod, alt, shift)`. Everything
/// before the final segment is a modifier; the final segment is the key.
pub fn combo_modifiers(combo: &str) -> (bool, bool, bool) {
    let mut parts: Vec<&str> = combo.split('-').collect();
    parts.pop();
    (
        parts.contains(&"mod"),
        parts.contains(&"alt"),
        parts.contains(&"shift"),
    )
}

/// Whether the sidebar should show its jump hints for the currently held
/// modifiers (t3code `shouldShowThreadJumpHintsForModifiers`). The held set
/// must match a jump combo EXACTLY, so adding Shift or Alt hides the hints and
/// a chord like Cmd+Shift+4 never flashes the overlay. `primary` is the held
/// "mod" key — cmd on macOS, ctrl elsewhere.
///
/// A jump combo with no modifiers at all never shows hints: it would otherwise
/// match the resting state and pin the overlay open. Pure.
pub fn jump_hints_visible(keymap: &KeymapConfig, primary: bool, alt: bool, shift: bool) -> bool {
    if !(primary || alt || shift) {
        return false;
    }
    ShortcutId::ALL
        .into_iter()
        .filter(|id| id.jump_slot().is_some())
        .any(|id| combo_modifiers(keymap.get(id)) == (primary, alt, shift))
}

/// Translate a stored combo into a bindable keystroke for this platform.
pub fn platform_combo(combo: &str) -> String {
    let primary = if cfg!(target_os = "macos") {
        "cmd"
    } else {
        "ctrl"
    };
    combo
        .split('-')
        .map(|part| if part == "mod" { primary } else { part })
        .collect::<Vec<_>>()
        .join("-")
}

/// Human-readable combo for the shortcuts table ("mod-s" → "Cmd+S"/"Ctrl+S").
pub fn display_combo(combo: &str) -> String {
    display_combo_on(cfg!(target_os = "macos"), combo)
}

/// [`display_combo`] for an explicit platform, so both spellings are testable
/// wherever the suite runs rather than only the host's.
pub fn display_combo_on(mac: bool, combo: &str) -> String {
    combo
        .split('-')
        .map(|part| match part {
            "mod" => {
                if mac {
                    "Cmd".to_string()
                } else {
                    "Ctrl".to_string()
                }
            }
            // macOS spells this key Option, and every native shortcuts list
            // shows "Opt"; only the badge surface uses the ⌥ glyph.
            "alt" => {
                if mac {
                    "Opt".to_string()
                } else {
                    "Alt".to_string()
                }
            }
            "shift" => "Shift".to_string(),
            other => {
                let mut chars = other.chars();
                match chars.next() {
                    Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                    None => String::new(),
                }
            }
        })
        .collect::<Vec<_>>()
        .join("+")
}

/// Compact combo for badge surfaces (the sidebar jump hints): macOS spells
/// the modifiers as their key glyphs in canonical ⌃⌥⇧⌘ order and drops the
/// separators ("⌘1", "⇧⌘A") — the form the model picker's ⌘N chips already
/// use — while other platforms keep the textual [`display_combo`] ("Ctrl+1").
pub fn badge_combo(combo: &str) -> String {
    badge_combo_on(cfg!(target_os = "macos"), combo)
}

/// [`badge_combo`] for an explicit platform.
pub fn badge_combo_on(mac: bool, combo: &str) -> String {
    if !mac {
        return display_combo_on(false, combo);
    }
    let mut parts: Vec<&str> = combo.split('-').collect();
    let key = parts.pop().unwrap_or("");
    let mut out = String::new();
    for glyph in ["ctrl", "alt", "shift", "mod"]
        .iter()
        .zip(['⌃', '⌥', '⇧', '⌘'])
        .filter_map(|(name, glyph)| parts.contains(name).then_some(glyph))
    {
        out.push(glyph);
    }
    let mut chars = key.chars();
    if let Some(first) = chars.next() {
        out.extend(first.to_uppercase());
        out.push_str(chars.as_str());
    }
    out
}

impl UiSettings {
    /// Clamp widths into their legal ranges (also heals NaN to defaults).
    pub fn clamped(mut self) -> Self {
        self.sidebar_width = clamp_or(
            self.sidebar_width,
            SIDEBAR_MIN,
            SIDEBAR_MAX,
            SIDEBAR_DEFAULT,
        );
        // The right pane has no persisted upper bound: its live drag clamps
        // against the current window, which is unavailable while loading.
        self.right_pane_width = min_or(self.right_pane_width, RIGHT_PANE_MIN, RIGHT_PANE_DEFAULT);
        self.terminal_height = clamp_or(
            self.terminal_height,
            TERMINAL_MIN_HEIGHT,
            TERMINAL_ABS_MAX_HEIGHT,
            TERMINAL_DEFAULT_HEIGHT,
        );
        self.keymap.heal_jump_slots();
        self
    }

    /// Load from `{data_dir}/ui-settings.json`; defaults on any failure.
    pub fn load(data_dir: &Path) -> Self {
        match std::fs::read_to_string(Self::path(data_dir)) {
            Ok(text) => match serde_json::from_str::<UiSettings>(&text) {
                Ok(settings) => settings.clamped(),
                Err(err) => {
                    tracing::warn!(error = %err, "ui-settings corrupt; using defaults");
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }

    /// Write atomically (temp file + rename) so a crash mid-write never corrupts.
    pub fn save(&self, data_dir: &Path) -> io::Result<()> {
        std::fs::create_dir_all(data_dir)?;
        let path = Self::path(data_dir);
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &path)
    }

    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join(FILE_NAME)
    }
}

fn clamp_or(value: f32, min: f32, max: f32, default: f32) -> f32 {
    if value.is_finite() {
        value.clamp(min, max)
    } else {
        default
    }
}

fn min_or(value: f32, min: f32, default: f32) -> f32 {
    if value.is_finite() {
        value.max(min)
    } else {
        default
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let settings = UiSettings {
            sidebar_width: 300.0,
            sidebar_collapsed: true,
            sidebar_grouped: true,
            last_space_id: Some("space-1".into()),
            tab_order: std::collections::HashMap::from([(
                "space-1".to_string(),
                vec!["b".to_string(), "a".to_string()],
            )]),
            space_order: vec!["space-2".to_string(), "space-1".to_string()],
            sidebar_scope_space: Some("space-1".to_string()),
            sound_enabled: false,
            dismissed_identity_rebuild: Some("2026-08-30T09:00:00Z".to_string()),
            right_pane_width: 700.0,
            right_pane_open: true,
            terminal_height: 320.0,
            terminal_open: true,
            keymap: KeymapConfig {
                toggle_sidebar: "mod-shift-s".into(),
                ..KeymapConfig::default()
            },
            appearance: crate::appearance::AppearanceMode::Light,
        };
        settings.save(dir.path()).unwrap();
        assert_eq!(UiSettings::load(dir.path()), settings);
    }

    /// A settings file written before light mode existed has no `appearance`
    /// key; it must load as "follow the OS" rather than failing the whole parse
    /// and resetting every other preference to defaults.
    #[test]
    fn settings_without_appearance_default_to_system() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            UiSettings::path(dir.path()),
            r#"{"sidebarWidth": 300, "soundEnabled": false}"#,
        )
        .unwrap();
        let loaded = UiSettings::load(dir.path());
        assert_eq!(loaded.appearance, crate::appearance::AppearanceMode::System);
        assert_eq!(loaded.sidebar_width, 300.0);
        assert!(!loaded.sound_enabled, "other keys still parse");
    }

    #[test]
    fn sidebar_scope_defaults_to_all_and_survives_old_files() {
        // A settings file written before this field existed.
        let legacy = r#"{"sidebarWidth":256,"sidebarCollapsed":false}"#;
        let loaded: UiSettings = serde_json::from_str(legacy).expect("legacy file still parses");
        assert_eq!(
            loaded.sidebar_scope_space, None,
            "absent field means All spaces"
        );

        let scoped = UiSettings {
            sidebar_scope_space: Some("space-1".into()),
            ..UiSettings::default()
        };
        let json = serde_json::to_string(&scoped).unwrap();
        assert!(json.contains("sidebarScopeSpace"));
        let round: UiSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(round.sidebar_scope_space, Some("space-1".into()));

        // All spaces writes nothing.
        let json = serde_json::to_string(&UiSettings::default()).unwrap();
        assert!(!json.contains("sidebarScopeSpace"));
    }

    #[test]
    fn missing_and_corrupt_files_yield_defaults() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(UiSettings::load(dir.path()), UiSettings::default());
        std::fs::write(UiSettings::path(dir.path()), "{not json").unwrap();
        assert_eq!(UiSettings::load(dir.path()), UiSettings::default());
    }

    #[test]
    fn loaded_values_are_clamped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            UiSettings::path(dir.path()),
            r#"{"sidebarWidth": 10000, "rightPaneWidth": 1}"#,
        )
        .unwrap();
        let loaded = UiSettings::load(dir.path());
        assert_eq!(loaded.sidebar_width, SIDEBAR_MAX);
        assert_eq!(loaded.right_pane_width, RIGHT_PANE_MIN);
    }

    #[test]
    fn large_right_pane_width_is_preserved() {
        let loaded = UiSettings {
            right_pane_width: 2400.0,
            ..Default::default()
        }
        .clamped();
        assert_eq!(loaded.right_pane_width, 2400.0);
    }

    #[test]
    fn nan_heals_to_default() {
        let healed = UiSettings {
            sidebar_width: f32::NAN,
            ..Default::default()
        }
        .clamped();
        assert_eq!(healed.sidebar_width, SIDEBAR_DEFAULT);
    }

    #[test]
    fn defaults_match_comet() {
        let d = UiSettings::default();
        assert_eq!(d.sidebar_width, 256.0);
        assert_eq!(d.right_pane_width, 520.0);
        assert_eq!(d.terminal_height, 280.0);
        assert!(!d.sidebar_collapsed && !d.right_pane_open && !d.terminal_open);
    }

    /// `Keystroke::parse` only rejects an unknown *modifier* — it accepts
    /// "", "ctrl-" and even "ctrl-nosuchkey", so `is_ok()` alone would pass
    /// against a typo in the key and bind a keystroke that can never fire.
    /// Assert the parsed shape instead: the modifier we asked for, and the
    /// exact key name.
    #[test]
    fn session_cycling_combos_parse_to_the_keystroke_they_name() {
        let next = gpui::Keystroke::parse(&platform_combo(ShortcutId::NextSession.default_combo()))
            .expect("next-session combo parses");
        assert_eq!(next.key, "tab");
        assert!(next.modifiers.control);
        assert!(!next.modifiers.shift);

        let prev = gpui::Keystroke::parse(&platform_combo(ShortcutId::PrevSession.default_combo()))
            .expect("prev-session combo parses");
        assert_eq!(prev.key, "tab");
        assert!(prev.modifiers.control);
        assert!(prev.modifiers.shift);
    }
    /// Cycling must not be reachable through the OS application switcher's
    /// keys: on macOS `mod-` becomes `cmd-`, and cmd-tab never reaches us.
    #[test]
    fn session_cycling_defaults_to_control_not_the_platform_modifier() {
        assert_eq!(ShortcutId::NextSession.default_combo(), "ctrl-tab");
        assert_eq!(ShortcutId::PrevSession.default_combo(), "ctrl-shift-tab");
        assert!(!ShortcutId::NextSession.default_combo().contains("mod-"));
        assert!(!ShortcutId::PrevSession.default_combo().contains("mod-"));
    }

    #[test]
    fn keymap_defaults_and_reset() {
        let mut keymap = KeymapConfig::default();
        assert_eq!(keymap.get(ShortcutId::ToggleSidebar), "mod-s");
        assert_eq!(keymap.get(ShortcutId::ToggleChanges), "mod-b");
        assert_eq!(keymap.get(ShortcutId::ToggleTerminal), "mod-j");
        assert_eq!(keymap.get(ShortcutId::ArchiveSession), "mod-shift-a");
        keymap.set(ShortcutId::ToggleSidebar, "mod-shift-x".into());
        assert_eq!(keymap.get(ShortcutId::ToggleSidebar), "mod-shift-x");
        keymap.reset(ShortcutId::ToggleSidebar);
        assert_eq!(keymap.get(ShortcutId::ToggleSidebar), "mod-s");
        keymap.set(ShortcutId::ArchiveSession, "mod-shift-y".into());
        assert_eq!(keymap.get(ShortcutId::ArchiveSession), "mod-shift-y");
        keymap.reset(ShortcutId::ArchiveSession);
        assert_eq!(keymap.get(ShortcutId::ArchiveSession), "mod-shift-a");
    }

    #[test]
    fn jump_slots_get_set_and_reset() {
        let mut keymap = KeymapConfig::default();
        assert_eq!(keymap.get(ShortcutId::JumpSession(0)), "mod-1");
        assert_eq!(keymap.get(ShortcutId::JumpSession(8)), "mod-9");
        // Past the last slot there is no shortcut, not a panic.
        assert_eq!(keymap.get(ShortcutId::JumpSession(9)), "");
        assert_eq!(ShortcutId::JumpSession(9).jump_slot(), None);
        assert_eq!(ShortcutId::JumpSession(0).jump_slot(), Some(0));
        assert_eq!(ShortcutId::ArchiveSession.jump_slot(), None);

        keymap.set(ShortcutId::JumpSession(2), "mod-alt-3".into());
        assert_eq!(keymap.get(ShortcutId::JumpSession(2)), "mod-alt-3");
        keymap.reset(ShortcutId::JumpSession(2));
        assert_eq!(keymap.get(ShortcutId::JumpSession(2)), "mod-3");
        // A write past the last slot is dropped, and grows nothing.
        keymap.set(ShortcutId::JumpSession(9), "mod-0".into());
        assert_eq!(keymap.jump_session.len(), JUMP_SLOTS);
    }

    #[test]
    fn short_or_long_jump_lists_heal_to_the_slot_count() {
        // Short: surviving entries keep their slot, the rest take defaults.
        let mut keymap = KeymapConfig {
            jump_session: vec!["mod-alt-1".into()],
            ..KeymapConfig::default()
        };
        keymap.heal_jump_slots();
        assert_eq!(keymap.jump_session.len(), JUMP_SLOTS);
        assert_eq!(keymap.get(ShortcutId::JumpSession(0)), "mod-alt-1");
        assert_eq!(keymap.get(ShortcutId::JumpSession(1)), "mod-2");

        // Long: the tail is dropped.
        let mut keymap = KeymapConfig {
            jump_session: (0..20).map(|i| format!("mod-{i}")).collect(),
            ..KeymapConfig::default()
        };
        keymap.heal_jump_slots();
        assert_eq!(keymap.jump_session.len(), JUMP_SLOTS);

        // Absent: the whole list comes back.
        let mut keymap = KeymapConfig {
            jump_session: Vec::new(),
            ..KeymapConfig::default()
        };
        keymap.heal_jump_slots();
        assert_eq!(keymap.jump_session, KeymapConfig::default().jump_session);
    }

    #[test]
    fn a_malformed_jump_list_heals_without_losing_other_settings() {
        // Healing happens on load, so an odd jumpSession must not cost the
        // user their sidebar width or their other combos.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            UiSettings::path(dir.path()),
            r#"{"sidebarWidth": 300, "keymap": {"toggleSidebar": "mod-shift-x", "jumpSession": ["mod-alt-1", "mod-alt-2"]}}"#,
        )
        .unwrap();
        let loaded = UiSettings::load(dir.path());
        assert_eq!(loaded.sidebar_width, 300.0);
        assert_eq!(loaded.keymap.get(ShortcutId::ToggleSidebar), "mod-shift-x");
        assert_eq!(loaded.keymap.get(ShortcutId::JumpSession(0)), "mod-alt-1");
        assert_eq!(loaded.keymap.get(ShortcutId::JumpSession(8)), "mod-9");
        assert_eq!(loaded.keymap.jump_session.len(), JUMP_SLOTS);
    }

    #[test]
    fn jump_hints_need_an_exact_modifier_match() {
        let keymap = KeymapConfig::default();
        // Mod alone matches mod-1..9.
        assert!(jump_hints_visible(&keymap, true, false, false));
        // Extra modifiers are a different chord (Cmd+Shift+4 screenshots).
        assert!(!jump_hints_visible(&keymap, true, false, true));
        assert!(!jump_hints_visible(&keymap, true, true, false));
        // Nothing held, nothing shown.
        assert!(!jump_hints_visible(&keymap, false, false, false));
        // Alt alone is not a jump modifier by default.
        assert!(!jump_hints_visible(&keymap, false, true, false));

        // Rebinding moves the trigger with it.
        let mut rebound = KeymapConfig::default();
        for slot in 0..JUMP_SLOTS {
            rebound.set(ShortcutId::JumpSession(slot), format!("mod-alt-{slot}"));
        }
        assert!(!jump_hints_visible(&rebound, true, false, false));
        assert!(jump_hints_visible(&rebound, true, true, false));

        // A jump combo with no modifiers must not pin the overlay open.
        let mut bare = KeymapConfig::default();
        bare.set(ShortcutId::JumpSession(0), "f5".into());
        assert!(!jump_hints_visible(&bare, false, false, false));
    }

    #[test]
    fn badge_combos_use_mac_glyphs_and_textual_form_elsewhere() {
        // macOS: glyphs in canonical ⌃⌥⇧⌘ order, no separators — the model
        // picker's ⌘N chip form.
        assert_eq!(badge_combo_on(true, "mod-2"), "⌘2");
        assert_eq!(badge_combo_on(true, "mod-shift-a"), "⇧⌘A");
        assert_eq!(badge_combo_on(true, "mod-alt-3"), "⌥⌘3");
        // A literal ctrl segment (the macOS recorder's spelling) is ⌃, so a
        // ctrl-rebound slot's badge is truthful too.
        assert_eq!(badge_combo_on(true, "ctrl-tab"), "⌃Tab");
        // Elsewhere the textual form stands.
        assert_eq!(badge_combo_on(false, "mod-2"), "Ctrl+2");
        assert_eq!(badge_combo_on(false, "mod-shift-a"), "Ctrl+Shift+A");
    }

    #[test]
    fn combo_modifiers_reads_the_stored_form() {
        assert_eq!(combo_modifiers("mod-1"), (true, false, false));
        assert_eq!(combo_modifiers("mod-alt-shift-k"), (true, true, true));
        assert_eq!(combo_modifiers("f5"), (false, false, false));
        assert_eq!(combo_modifiers("shift-tab"), (false, false, true));
        // The FINAL segment is always the key, even when it is spelled like a
        // modifier: a hand-edited file carrying a bare "mod" must read as no
        // modifiers, or `jump_hints_visible` would pin the overlay open for
        // as long as the primary key is held.
        assert_eq!(combo_modifiers("mod"), (false, false, false));
    }

    #[test]
    fn every_shortcut_default_is_unique_and_bindable() {
        // A new shortcut must not ship in conflict with an existing one, and
        // its default must parse on this platform.
        assert!(conflicted_shortcuts(&KeymapConfig::default()).is_empty());
        for id in ShortcutId::ALL {
            assert!(
                gpui::Keystroke::parse(&platform_combo(id.default_combo())).is_ok(),
                "{id:?} default combo does not parse"
            );
        }
    }

    #[test]
    fn a_keymap_missing_newer_shortcuts_keeps_its_customizations() {
        // Upgrade path: a file written before archiving existed carries the
        // user's rebinds and defaults only the new row.
        let keymap: KeymapConfig = serde_json::from_str(r#"{"toggleSidebar": "mod-shift-x"}"#)
            .expect("older keymap loads");
        assert_eq!(keymap.get(ShortcutId::ToggleSidebar), "mod-shift-x");
        assert_eq!(keymap.get(ShortcutId::ArchiveSession), "mod-shift-a");
    }

    #[test]
    fn new_session_is_a_defaulted_rebindable_shortcut() {
        let legacy: KeymapConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(legacy.get(ShortcutId::NewSession), "mod-n");
        assert!(ShortcutId::ALL.contains(&ShortcutId::FocusSearch));
        assert!(ShortcutId::ALL.contains(&ShortcutId::NewSession));

        let mut changed = legacy;
        changed.set(ShortcutId::NewSession, "mod-shift-n".into());
        assert_eq!(changed.get(ShortcutId::NewSession), "mod-shift-n");
        changed.reset(ShortcutId::NewSession);
        assert_eq!(changed.get(ShortcutId::NewSession), "mod-n");
    }

    #[test]
    fn combo_recording() {
        // Primary modifier (ctrl or cmd) normalizes to "mod".
        assert_eq!(
            combo_from_keystroke(true, false, false, false, "s"),
            Some("mod-s".into())
        );
        assert_eq!(
            combo_from_keystroke(false, false, false, true, "s"),
            Some("mod-s".into())
        );
        assert_eq!(
            combo_from_keystroke(true, true, true, false, "K"),
            Some("mod-alt-shift-k".into())
        );
        // Plain keys record without modifiers (Esc is filtered by the caller).
        assert_eq!(
            combo_from_keystroke(false, false, false, false, "f5"),
            Some("f5".into())
        );
        // Bare modifier presses record nothing.
        assert_eq!(
            combo_from_keystroke(true, false, false, false, "ctrl"),
            None
        );
        assert_eq!(
            combo_from_keystroke(false, false, true, false, "shift"),
            None
        );
        assert_eq!(combo_from_keystroke(false, false, false, false, ""), None);
    }

    #[test]
    fn conflict_detection() {
        let mut keymap = KeymapConfig::default();
        assert!(conflicted_shortcuts(&keymap).is_empty());
        keymap.set(ShortcutId::ToggleChanges, "mod-s".into());
        let conflicts = conflicted_shortcuts(&keymap);
        assert!(conflicts.contains(&ShortcutId::ToggleSidebar));
        assert!(conflicts.contains(&ShortcutId::ToggleChanges));
        assert!(!conflicts.contains(&ShortcutId::ToggleTerminal));
        keymap.reset(ShortcutId::ToggleChanges);
        assert!(conflicted_shortcuts(&keymap).is_empty());
    }

    #[test]
    fn combo_translation() {
        let primary = if cfg!(target_os = "macos") {
            "cmd"
        } else {
            "ctrl"
        };
        assert_eq!(platform_combo("mod-s"), format!("{primary}-s"));
        assert_eq!(platform_combo("alt-f4"), "alt-f4");
        let display_primary = if cfg!(target_os = "macos") {
            "Cmd"
        } else {
            "Ctrl"
        };
        assert_eq!(
            display_combo("mod-shift-s"),
            format!("{display_primary}+Shift+S")
        );
        assert_eq!(display_combo("f5"), "F5");
        // Both spellings assert here rather than only the host's: the alt key
        // is Option on macOS and Alt everywhere else, and the suite runs on
        // Linux in CI.
        assert_eq!(display_combo_on(true, "mod-alt-up"), "Cmd+Opt+Up");
        assert_eq!(display_combo_on(false, "mod-alt-up"), "Ctrl+Alt+Up");
    }

    #[test]
    fn keymap_survives_old_settings_files() {
        // Files written before the keymap existed load with defaults.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(UiSettings::path(dir.path()), r#"{"sidebarWidth": 300}"#).unwrap();
        let loaded = UiSettings::load(dir.path());
        assert_eq!(loaded.keymap, KeymapConfig::default());
        assert!(!loaded.sidebar_grouped);
    }

    #[test]
    fn terminal_height_clamps_on_load() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(UiSettings::path(dir.path()), r#"{"terminalHeight": 5}"#).unwrap();
        assert_eq!(
            UiSettings::load(dir.path()).terminal_height,
            TERMINAL_MIN_HEIGHT
        );
        std::fs::write(UiSettings::path(dir.path()), r#"{"terminalHeight": 99999}"#).unwrap();
        assert_eq!(
            UiSettings::load(dir.path()).terminal_height,
            TERMINAL_ABS_MAX_HEIGHT
        );
    }
}
