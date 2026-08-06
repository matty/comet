//! Spaces sidebar: the spaces list (folder + device rows), the global
//! Sessions list, and the add-space palette (⌘K-style: device tabs + filtered
//! folder browser).
//!
//! A space = a synced (device, folder) pair; the sidebar's job is switching
//! between them and surfacing which sessions want attention. Child module of
//! `shell` so it renders straight off `Shell`'s private state.

use super::*;
use crate::motion::TAB_SLIDE;
use crate::pickers::{breadcrumbs, browser_rows, parent_path};
use crate::terminal::panel::{drop_index, reorder_tabs, slide_offset};
use comet_proto::{ChatIndicator, Device, FolderListing, Space};
use gpui::FocusHandle;

/// Space-row slot height for drag drop-index math: py(6)×2 + 17px line ≈ 29,
/// plus the 2px column gap.
const SPACE_ROW_SLOT: f32 = 31.0;

/// Cap on the dropdown panel's space-row region (~8 rows) — every other list
/// popover in this crate caps and scrolls (`pickers.rs`'s branch/checkout
/// lists, `composer.rs`'s file-mention popup); without this, a long space
/// list defeats the whole point of the fixed-height trigger by overflowing
/// the window (`snap_to_window_with_margin` repositions the panel, it does
/// not shrink it).
const PANEL_ROWS_MAX_H: f32 = SPACE_ROW_SLOT * 8.0;

/// Drag-reorder state for the spaces list; `epoch` keys the 150ms slide
/// animation restarts (the session-tab idiom, vertical).
pub(super) struct SpaceDragState {
    from: usize,
    over: usize,
    epoch: usize,
    prev_over: usize,
}

/// Spaces (drag-resolved order) + the per-space device/offline/attention
/// lookups — computed once per frame and shared by the trigger, the dropdown
/// panel, and its keyboard handler so all three agree on the same list.
struct SpacesContext {
    spaces: Vec<Space>,
    device_names: std::collections::HashMap<String, String>,
    offline_devices: std::collections::HashSet<String>,
    attention: std::collections::HashMap<String, ChatIndicator>,
}

#[cfg(test)]
mod federation_projection_tests {
    use super::*;
    use comet_client::ServerState;
    use comet_proto::{RemoteConnectionState, ServerId};

    fn server(id: &str, state: RemoteConnectionState, spaces: &[&str]) -> ServerState {
        let mut server = ServerState::empty(ServerId::new(id), id, state);
        server.spaces = spaces
            .iter()
            .map(|id| Space {
                id: (*id).into(),
                device_id: "device".into(),
                path: format!("/{id}"),
                name: None,
                git_detected: false,
                git_checked_at: None,
                checkout_id: None,
                created_at: Utc::now(),
            })
            .collect();
        server
    }

    #[test]
    fn projection_groups_every_online_server_and_hides_offline_children() {
        let local = server("local", RemoteConnectionState::Online, &["same"]);
        let b = server("b", RemoteConnectionState::Online, &["same"]);
        let c = server("c", RemoteConnectionState::Offline, &["stale"]);
        let servers = std::collections::HashMap::from([
            (local.id.clone(), local),
            (b.id.clone(), b),
            (c.id.clone(), c),
        ]);
        let order = vec![
            ServerId::new("local"),
            ServerId::new("b"),
            ServerId::new("c"),
        ];

        let groups = project_sidebar_servers(&servers, &order);

        assert_eq!(
            groups
                .iter()
                .map(|g| g.server.id.clone())
                .collect::<Vec<_>>(),
            order
        );
        assert_eq!(groups[0].spaces.len(), 1);
        assert_eq!(groups[1].spaces.len(), 1);
        assert!(groups[2].spaces.is_empty());
    }

    fn test_device(id: &str) -> Device {
        Device {
            id: id.into(),
            name: id.into(),
            platform: "macos".into(),
            last_seen_at: None,
            created_at: None,
            version: None,
        }
    }

    #[test]
    fn browse_targets_span_every_online_server() {
        let mut local = server("local", RemoteConnectionState::Online, &[]);
        local.devices = vec![test_device("d1")];
        let mut remote = server("remote", RemoteConnectionState::Online, &[]);
        remote.devices = vec![test_device("d2"), test_device("d3")];
        let servers = std::collections::HashMap::from([
            (local.id.clone(), local),
            (remote.id.clone(), remote),
        ]);
        let order = vec![ServerId::new("local"), ServerId::new("remote")];

        let groups = project_sidebar_servers(&servers, &order);
        let targets = browse_targets(&groups);

        assert_eq!(
            targets
                .iter()
                .map(|t| (t.server_id.clone(), t.device.id.clone()))
                .collect::<Vec<_>>(),
            vec![
                (ServerId::new("local"), "d1".to_string()),
                (ServerId::new("remote"), "d2".to_string()),
                (ServerId::new("remote"), "d3".to_string()),
            ]
        );
    }

    #[test]
    fn browse_targets_exclude_offline_servers() {
        let mut offline = server("nuc", RemoteConnectionState::Offline, &[]);
        offline.devices = vec![test_device("d1")];
        let servers = std::collections::HashMap::from([(offline.id.clone(), offline)]);
        let order = vec![ServerId::new("nuc")];

        let groups = project_sidebar_servers(&servers, &order);

        assert!(browse_targets(&groups).is_empty());
    }

    #[test]
    fn browse_targets_include_a_server_with_zero_spaces() {
        // THE REGRESSION THIS TASK EXISTS FOR: a freshly paired remote with
        // no spaces must still be offered, or it can never receive its
        // first one.
        let mut remote = server("nuc", RemoteConnectionState::Online, &[]);
        remote.devices = vec![test_device("d1")];
        let servers = std::collections::HashMap::from([(remote.id.clone(), remote)]);
        let order = vec![ServerId::new("nuc")];

        let groups = project_sidebar_servers(&servers, &order);
        let targets = browse_targets(&groups);

        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].device.id, "d1");
    }

    #[test]
    fn default_browse_target_prefers_the_local_device() {
        let targets = vec![
            BrowseTarget {
                server_id: ServerId::new("a"),
                server_name: "A".into(),
                device: test_device("d1"),
            },
            BrowseTarget {
                server_id: ServerId::new("b"),
                server_name: "B".into(),
                device: test_device("d2"),
            },
        ];

        let picked = default_browse_target(&targets, Some("d2")).expect("a target");
        assert_eq!(picked.device.id, "d2");
        assert_eq!(picked.server_id, ServerId::new("b"));
    }

    #[test]
    fn default_browse_target_falls_back_to_the_first() {
        let targets = vec![
            BrowseTarget {
                server_id: ServerId::new("a"),
                server_name: "A".into(),
                device: test_device("d1"),
            },
            BrowseTarget {
                server_id: ServerId::new("b"),
                server_name: "B".into(),
                device: test_device("d2"),
            },
        ];

        // No local id at all.
        assert_eq!(
            default_browse_target(&targets, None).map(|t| t.device.id),
            Some("d1".to_string())
        );
        // A local id that names a device not present in the list.
        assert_eq!(
            default_browse_target(&targets, Some("unknown")).map(|t| t.device.id),
            Some("d1".to_string())
        );
    }

    #[test]
    fn default_browse_target_is_none_when_nothing_is_online() {
        assert!(default_browse_target(&[], None).is_none());
        assert!(default_browse_target(&[], Some("d1")).is_none());
    }

    /// Wraps a single `ServerState` in a `SidebarServerGroup` via the real
    /// projection (not a hand-built struct) — `SidebarServerGroup` is
    /// private, so this is the only route into one from a test.
    fn sidebar_group(server: &ServerState) -> SidebarServerGroup {
        let servers = std::collections::HashMap::from([(server.id.clone(), server.clone())]);
        let order = vec![server.id.clone()];
        project_sidebar_servers(&servers, &order)
            .into_iter()
            .next()
            .expect("one server in, one group out")
    }

    #[test]
    fn panel_groups_by_server_only_when_more_than_one_is_configured() {
        let local = server("local", RemoteConnectionState::Online, &["s1"]);
        let groups = vec![sidebar_group(&local)];
        let items = panel_grouped_items(&SidebarScope::All, &groups);
        assert!(
            !items
                .iter()
                .any(|i| matches!(i, PanelItem::ServerHeader { .. })),
            "one server renders flat — a header for a single group is noise"
        );

        let remote = server("nuc", RemoteConnectionState::Offline, &[]);
        let groups = vec![sidebar_group(&local), sidebar_group(&remote)];
        let items = panel_grouped_items(&SidebarScope::All, &groups);
        assert_eq!(items[0], PanelItem::AllSpaces { active: true });
        assert!(matches!(
            items[1],
            PanelItem::ServerHeader { online: true, .. }
        ));
        assert_eq!(
            items[2],
            PanelItem::Space {
                id: "s1".into(),
                active: false
            }
        );
        assert!(
            matches!(items[3], PanelItem::ServerHeader { online: false, .. }),
            "an offline server keeps its header and contributes no children"
        );
        assert_eq!(items.len(), 4);
    }
}

#[derive(Clone)]
struct SidebarServerGroup {
    server: comet_client::ServerState,
    spaces: Vec<Space>,
    chats: Vec<comet_proto::Chat>,
}

/// A machine the add-space palette can browse: the device plus the server
/// that owns it. The server is what every RPC in that flow must be
/// addressed to — the active server is NOT necessarily the one being
/// browsed (Task 8b).
#[derive(Clone, Debug, PartialEq)]
pub(super) struct BrowseTarget {
    pub server_id: comet_proto::ServerId,
    pub server_name: String,
    pub device: Device,
}

/// Every device of every ONLINE server, in server order then device order —
/// the machines a space can actually be created on. Offline servers are
/// excluded entirely: `project_sidebar_servers` does not clear their raw
/// `server.devices`, so this filters on connection state itself rather than
/// trusting an empty list; an RPC to an offline server cannot succeed
/// anyway. Private, not `pub(super)`: `SidebarServerGroup` is a private
/// struct (this module), so a more-visible signature is E0446 — same
/// reasoning as `panel_grouped_items`, below.
fn browse_targets(groups: &[SidebarServerGroup]) -> Vec<BrowseTarget> {
    groups
        .iter()
        .filter(|group| group.server.connection == comet_proto::RemoteConnectionState::Online)
        .flat_map(|group| {
            let server_id = group.server.id.clone();
            let server_name = group.server.name.clone();
            group.server.devices.iter().map(move |device| BrowseTarget {
                server_id: server_id.clone(),
                server_name: server_name.clone(),
                device: device.clone(),
            })
        })
        .collect()
}

/// Where the rail opens: this installation's own device when it is present
/// among the ONLINE targets, else the first target. `None` only when there
/// are no online machines at all — a freshly paired remote with zero spaces
/// still counts (Task 8b, the whole point of this task).
pub(super) fn default_browse_target(
    targets: &[BrowseTarget],
    local_device_id: Option<&str>,
) -> Option<BrowseTarget> {
    targets
        .iter()
        .find(|target| local_device_id.is_some_and(|id| target.device.id == id))
        .or_else(|| targets.first())
        .cloned()
}

fn project_sidebar_servers(
    servers: &std::collections::HashMap<comet_proto::ServerId, comet_client::ServerState>,
    order: &[comet_proto::ServerId],
) -> Vec<SidebarServerGroup> {
    order
        .iter()
        .filter_map(|id| servers.get(id))
        .map(|server| {
            let online = server.connection == comet_proto::RemoteConnectionState::Online;
            SidebarServerGroup {
                server: server.clone(),
                spaces: if online {
                    let mut spaces = server.spaces.clone();
                    spaces.sort_by(|a, b| {
                        a.created_at
                            .cmp(&b.created_at)
                            .then_with(|| a.id.cmp(&b.id))
                    });
                    spaces
                } else {
                    Vec::new()
                },
                chats: if online {
                    let mut chats = server.chats.clone();
                    chats.sort_by(|a, b| {
                        b.last_message_at
                            .cmp(&a.last_message_at)
                            .then_with(|| a.id.cmp(&b.id))
                    });
                    chats
                } else {
                    Vec::new()
                },
            }
        })
        .collect()
}

/// The dragged-row payload (gpui drag-and-drop).
struct SpaceDragPayload {
    from: usize,
    name: SharedString,
}

/// The floating row rendered at the cursor while dragging.
struct SpaceGhost {
    name: SharedString,
}

impl Render for SpaceGhost {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx);
        div()
            .w(px(200.0))
            .h(px(29.0))
            .px(px(Theme::SPACE_SM))
            .flex()
            .items_center()
            .gap(px(Theme::SPACE_SM))
            .rounded(px(8.0))
            .bg(theme.surface_raised)
            .border_1()
            .border_color(theme.border_strong)
            .text_size(px(13.0))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(theme.text)
            .opacity(0.85)
            .child(
                icon(icons::FOLDER)
                    .size(px(16.0))
                    .flex_none()
                    .text_color(theme.text_muted),
            )
            .child(div().truncate().child(self.name.clone()))
    }
}

/// The add-space palette (a command-K surface, summoned by ⌘K): search bar
/// across the top, folder browser on the left, a Devices rail on the right,
/// kbd-hint footer. One surface — picking a device in the rail rebrowses in
/// place, no step wizard.
pub(super) struct AddSpaceFlow {
    /// The machine currently browsed (the highlighted rail row) — a device
    /// AND its owning server, since the rail spans every online server, not
    /// only the active one (Task 8b).
    device: Option<BrowseTarget>,
    /// Filter input; Enter descends into the highlighted folder.
    search: Entity<ComposerInput>,
    browser: Loadable<FolderListing>,
    /// Requested browser path (`None` = the device's default, i.e. home).
    browser_path: Option<String>,
    /// The device's home (the path a `None` browse resolved to) — breadcrumbs
    /// fold everything up to here into the device-name crumb.
    home: Option<String>,
    /// Best-effort git seed for the CURRENT browser path (known when we
    /// descended through an entry whose `is_repo` we saw; the owning device's
    /// SpacesSync re-verifies either way).
    browser_repo: bool,
    /// Keyboard highlight within the FILTERED folder rows.
    active: usize,
    submit_busy: bool,
    error: Option<SharedString>,
    /// Tracked on the card (`track_focus`) — puts the card on the keyboard
    /// dispatch path so ↑↓/⌫/esc reach `add_space_key` while the search input
    /// holds focus (the structure every working picker uses).
    focus: FocusHandle,
    /// Folder-list scroll — keyboard navigation keeps the highlighted row in
    /// view (`scroll_to_item`).
    list_scroll: gpui::ScrollHandle,
    focus_pending: bool,
    load_task: Option<Task<()>>,
    submit_task: Option<Task<()>>,
    _search_events: Subscription,
}

/// The space-row Rename dialog (same shape as [`RenameChatDialog`]).
pub(super) struct RenameSpaceDialog {
    pub space_id: String,
    pub input: Entity<ComposerInput>,
    pub focus_pending: bool,
    pub _events: Subscription,
}

/// Dot color for a chat's display status (tab dots + Sessions rows).
pub(super) fn status_dot_color(status: ChatIndicator, theme: &Theme) -> gpui::Hsla {
    match status {
        // Pink, not amber — the harsh yellow read as a warning; running is
        // routine (user request).
        ChatIndicator::Working => {
            theme.busy.opacity(0.85) // pink-400
        }
        // Blue: "asking you a question" must read differently from "busy
        // working" at a glance.
        ChatIndicator::AwaitingInput => theme.accent.opacity(0.9),
        ChatIndicator::Errored => theme.danger,
        // Green: finished-but-unseen reads as "ready for you".
        ChatIndicator::Completed => {
            theme.success.opacity(0.9) // emerald-400
        }
        ChatIndicator::Idle => crate::theme::ink(0.14),
    }
}

/// The 20×20 `+` that trails a section header. Both sections use this —
/// they must be visually identical.
pub(super) fn header_plus(
    id: &'static str,
    theme: &Theme,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .size(px(20.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(5.0))
        .cursor_pointer()
        .bg(motion::hover_blend(
            id,
            crate::theme::wash(0.0),
            crate::theme::wash(0.14),
        ))
        .on_hover(motion::hover_listener(id))
        .on_click(on_click)
        .child(
            icon(icons::PLUS)
                .size(px(14.0))
                .text_color(theme.text_muted),
        )
}

/// One entry in the scope-dropdown panel, in display order: `All spaces`
/// pinned first (when shown), then — only when 2+ servers are configured —
/// a `ServerHeader` per server with that server's spaces nested underneath.
/// A single server renders flat: no headers, just the spaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PanelItem {
    AllSpaces {
        active: bool,
    },
    Space {
        id: String,
        active: bool,
    },
    ServerHeader {
        server_id: comet_proto::ServerId,
        name: String,
        online: bool,
    },
}

/// Panel contents in display order: `All spaces` pinned first, then the spaces
/// in their (already drag-resolved) order. The flat-list half of the panel's
/// logic; `panel_grouped_items` delegates here for `groups.len() <= 1` so
/// there is exactly one place that turns a space list into `PanelItem`s.
pub(super) fn panel_items(scope: &SidebarScope, spaces: &[Space]) -> Vec<PanelItem> {
    let mut items = vec![PanelItem::AllSpaces {
        active: matches!(scope, SidebarScope::All),
    }];
    items.extend(spaces.iter().map(|s| PanelItem::Space {
        id: s.id.clone(),
        active: scope.space_id() == Some(s.id.as_str()),
    }));
    items
}

/// Panel contents when servers are involved. One server renders flat (no
/// header — delegates to [`panel_items`]); two or more get a header each,
/// mirroring today's `grouped_mode`. Offline servers keep their header and
/// contribute no children — `project_sidebar_servers` already empties their
/// `spaces`.
///
/// Private, not `pub(super)`: `SidebarServerGroup` is a private struct
/// (this module), so a more-visible signature is E0446. Every caller is in
/// this module.
fn panel_grouped_items(scope: &SidebarScope, groups: &[SidebarServerGroup]) -> Vec<PanelItem> {
    if groups.len() <= 1 {
        let spaces = groups.first().map(|g| g.spaces.as_slice()).unwrap_or(&[]);
        return panel_items(scope, spaces);
    }
    let mut items = vec![PanelItem::AllSpaces {
        active: matches!(scope, SidebarScope::All),
    }];
    for group in groups {
        items.push(PanelItem::ServerHeader {
            server_id: group.server.id.clone(),
            name: group.server.name.clone(),
            online: group.server.connection == comet_proto::RemoteConnectionState::Online,
        });
        items.extend(group.spaces.iter().map(|s| PanelItem::Space {
            id: s.id.clone(),
            active: scope.space_id() == Some(s.id.as_str()),
        }));
    }
    items
}

/// `1` when the panel leads with `All spaces` (Switch — it occupies logical
/// position 0), `0` in `PickForNewSession` (that item is skipped entirely,
/// so position 0 is the first space).
pub(super) fn dropdown_offset(mode: DropdownMode) -> usize {
    if mode == DropdownMode::Switch { 1 } else { 0 }
}

/// Positions (within the logical `[All spaces?, space0, space1, ...]` list)
/// that keyboard nav and clicks may land on — offline spaces are unreachable
/// in `PickForNewSession` (an unclickable row must not be keyboard-reachable
/// either).
///
/// `host_offline[i]` is space `i`'s pre-resolved offline flag. Round 1's
/// shape (`spaces: &[Space]` + a device-id `HashSet`) breaks once a second
/// server exists: two servers can register the same local device id, so a
/// merged set would falsely mark a same-named device on another server
/// offline (`spaces.rs` grouped rendering resolves each space's device
/// against its OWNING server's own device list, never a merged one — this
/// mirrors that). Server headers never occupy a position at all — they are
/// not represented in this slice, so nothing extra is needed to exclude them
/// from keyboard nav; adding a third `PanelItem` kind does not change this
/// function's shape. Pure so the off-by-one risk here is actually testable —
/// this crate has no gpui render-test harness.
pub(super) fn dropdown_navigable_positions(
    mode: DropdownMode,
    host_offline: &[bool],
) -> Vec<usize> {
    let offset = dropdown_offset(mode);
    let mut positions = Vec::new();
    if mode == DropdownMode::Switch {
        positions.push(0);
    }
    for (ix, offline) in host_offline.iter().enumerate() {
        if mode == DropdownMode::Switch || !offline {
            positions.push(offset + ix);
        }
    }
    positions
}

/// Maps a navigable logical position back to a `spaces` index — `None` when
/// the position is the `All spaces` row (only reachable at position 0, and
/// only in `Switch` mode).
pub(super) fn dropdown_position_to_space_index(mode: DropdownMode, pos: usize) -> Option<usize> {
    if mode == DropdownMode::Switch && pos == 0 {
        None
    } else {
        Some(pos - dropdown_offset(mode))
    }
}

/// One space row's resolved display + click data for the panel — device
/// name, offline flag, and attention, resolved against whichever server
/// actually owns the space. Grouped rows are not always the active server's
/// spaces, but `ServerState` (`comet_client::ServerState`) carries live
/// `devices`/`sessions` for every configured server, not just the active
/// one, so a non-active server's offline/attention state is knowable without
/// switching to it first.
struct PanelSpaceEntry {
    server_id: comet_proto::ServerId,
    space: Space,
    device_name: String,
    host_offline: bool,
    attention: Option<ChatIndicator>,
}

/// One rendered row of the panel's scrollable region: either a
/// non-interactive server header or a space (carrying its resolved display
/// data). `panel_rows` is the only place that decides which spaces got a
/// header — everything downstream (nav positions, click handlers, rendering)
/// walks this list instead of re-deriving grouping.
enum PanelRow {
    Header {
        name: String,
        connection: comet_proto::RemoteConnectionState,
    },
    Space(PanelSpaceEntry),
}

/// Aggregate attention (the most urgent of Working/AwaitingInput) per space
/// for one server group — the per-group analogue of `spaces_context`'s own
/// attention loop (`Shell::spaces_context`, below), scoped to a group that
/// may not be the active server. `group.chats` and `group.server.sessions`
/// are that server's own projection (`project_sidebar_servers`), so this
/// works for any online group, active or not.
fn group_space_attention(
    group: &SidebarServerGroup,
    now: chrono::DateTime<Utc>,
) -> std::collections::HashMap<String, ChatIndicator> {
    let mut attention: std::collections::HashMap<String, ChatIndicator> =
        std::collections::HashMap::new();
    for chat in group.chats.iter().filter(|c| !c.archived) {
        let session = group.server.sessions.iter().find(|s| s.chat_id == chat.id);
        let status = crate::state::display_status(chat, session, now);
        if !matches!(
            status,
            ChatIndicator::Working | ChatIndicator::AwaitingInput
        ) {
            continue;
        }
        let Some(space_id) = chat.space_id.clone() else {
            continue;
        };
        attention
            .entry(space_id)
            .and_modify(|held| {
                if crate::state::attention_rank(status) < crate::state::attention_rank(*held) {
                    *held = status;
                }
            })
            .or_insert(status);
    }
    attention
}

/// The panel's row plan, in display order. Flat (0 or 1 servers) uses `ctx`
/// — the DRAG-REORDER-aware order (`resolve_tab_order`, `Shell::
/// spaces_context`) — because `SidebarServerGroup::spaces`
/// (`project_sidebar_servers`) is sorted only by creation and is NOT
/// reorder-aware; switching the flat case over to it would silently drop the
/// existing drag-reorder feature. Grouped (2+) walks every group instead,
/// since `ctx` only ever holds the ACTIVE server's spaces — the whole point
/// of grouping is showing every configured server's spaces at once.
fn panel_rows(
    ctx: &SpacesContext,
    server_groups: &[SidebarServerGroup],
    active_server: &comet_proto::ServerId,
    local_device_id: Option<&str>,
    now: chrono::DateTime<Utc>,
) -> Vec<PanelRow> {
    if server_groups.len() > 1 {
        let mut rows = Vec::new();
        for group in server_groups {
            rows.push(PanelRow::Header {
                name: group.server.name.clone(),
                connection: group.server.connection.clone(),
            });
            let attention = group_space_attention(group, now);
            for space in &group.spaces {
                let device = group
                    .server
                    .devices
                    .iter()
                    .find(|d| d.id == space.device_id);
                // Mirrors `AppState::device_online`: the local device is
                // trivially online, an unknown device gets the benefit of
                // the doubt.
                let device_online = local_device_id == Some(space.device_id.as_str())
                    || device
                        .map(|d| crate::settings::devices::device_online(d.last_seen_at, now))
                        .unwrap_or(true);
                rows.push(PanelRow::Space(PanelSpaceEntry {
                    server_id: group.server.id.clone(),
                    device_name: device
                        .map(|d| d.name.clone())
                        .unwrap_or_else(|| "Unknown device".to_string()),
                    host_offline: !device_online,
                    attention: attention.get(&space.id).copied(),
                    space: space.clone(),
                }));
            }
        }
        rows
    } else {
        ctx.spaces
            .iter()
            .map(|space| {
                PanelRow::Space(PanelSpaceEntry {
                    server_id: active_server.clone(),
                    device_name: ctx
                        .device_names
                        .get(&space.device_id)
                        .cloned()
                        .unwrap_or_else(|| "Unknown device".to_string()),
                    host_offline: ctx.offline_devices.contains(&space.device_id),
                    attention: ctx.attention.get(&space.id).copied(),
                    space: space.clone(),
                })
            })
            .collect()
    }
}

/// Drop-index math (`drop_index`, below) runs in CONTENT coordinates, but
/// `DragMoveEvent::bounds` is the scroll VIEWPORT — fixed on screen,
/// invariant under scrolling, because gpui applies the scroll offset to
/// children only (`with_element_offset`). `scroll_offset_y` is
/// `ScrollHandle::offset().y`, which is `<= 0` once scrolled down; content-
/// space y is `bounds.top() - offset.y` (the relation gpui itself states in
/// `ScrollHandle::top_item`), so the viewport-relative `position_y -
/// bounds_top` needs `- scroll_offset_y` added back to land in content
/// space. `shell::tabs`'s horizontal drag (`tabs.rs:534-536`,
/// `scroll_for_drag.offset().x`) is the exact same fix on the other axis.
pub(super) fn content_rel_y(position_y: f32, bounds_top: f32, scroll_offset_y: f32) -> f32 {
    position_y - bounds_top - scroll_offset_y
}

#[cfg(test)]
mod panel_tests {
    use super::*;

    fn test_space(path: &str) -> Space {
        Space {
            id: "test".into(),
            device_id: "device".into(),
            path: path.into(),
            name: None,
            git_detected: false,
            git_checked_at: None,
            checkout_id: None,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn panel_lists_all_spaces_first_and_marks_the_active_one() {
        let spaces = vec![
            Space {
                id: "s1".into(),
                ..test_space("/a")
            },
            Space {
                id: "s2".into(),
                ..test_space("/b")
            },
        ];

        let items = panel_items(&SidebarScope::All, &spaces);
        assert_eq!(items[0], PanelItem::AllSpaces { active: true });
        assert_eq!(
            items[1],
            PanelItem::Space {
                id: "s1".into(),
                active: false
            }
        );
        assert_eq!(
            items[2],
            PanelItem::Space {
                id: "s2".into(),
                active: false
            }
        );

        let items = panel_items(&SidebarScope::Space("s2".into()), &spaces);
        assert_eq!(items[0], PanelItem::AllSpaces { active: false });
        assert_eq!(
            items[2],
            PanelItem::Space {
                id: "s2".into(),
                active: true
            }
        );
    }

    #[test]
    fn panel_with_no_spaces_still_offers_all_spaces() {
        let items = panel_items(&SidebarScope::All, &[]);
        assert_eq!(items, vec![PanelItem::AllSpaces { active: true }]);
    }

    #[test]
    fn navigable_positions_switch_reserves_slot_zero_for_all_spaces() {
        assert_eq!(
            dropdown_navigable_positions(DropdownMode::Switch, &[false, false]),
            vec![0, 1, 2]
        );
    }

    #[test]
    fn navigable_positions_switch_includes_offline_spaces() {
        // Switching scope to an offline space is still meaningful (you land
        // on it and see it's offline) — only the picker excludes them.
        assert_eq!(
            dropdown_navigable_positions(DropdownMode::Switch, &[true]),
            vec![0, 1]
        );
    }

    #[test]
    fn navigable_positions_switch_empty_spaces_is_just_all_spaces() {
        assert_eq!(
            dropdown_navigable_positions(DropdownMode::Switch, &[]),
            vec![0]
        );
    }

    #[test]
    fn navigable_positions_pick_has_no_all_spaces_slot_and_skips_offline() {
        assert_eq!(
            dropdown_navigable_positions(DropdownMode::PickForNewSession, &[false, true, false]),
            vec![0, 2]
        );
    }

    #[test]
    fn navigable_positions_pick_all_offline_is_empty() {
        assert!(
            dropdown_navigable_positions(DropdownMode::PickForNewSession, &[true, true]).is_empty()
        );
    }

    #[test]
    fn navigable_positions_pick_empty_spaces_is_empty() {
        assert!(dropdown_navigable_positions(DropdownMode::PickForNewSession, &[]).is_empty());
    }

    #[test]
    fn position_to_space_index_switch_zero_is_all_spaces() {
        assert_eq!(
            dropdown_position_to_space_index(DropdownMode::Switch, 0),
            None
        );
        assert_eq!(
            dropdown_position_to_space_index(DropdownMode::Switch, 1),
            Some(0)
        );
        assert_eq!(
            dropdown_position_to_space_index(DropdownMode::Switch, 3),
            Some(2)
        );
    }

    #[test]
    fn position_to_space_index_pick_has_no_all_spaces_slot() {
        assert_eq!(
            dropdown_position_to_space_index(DropdownMode::PickForNewSession, 0),
            Some(0)
        );
        assert_eq!(
            dropdown_position_to_space_index(DropdownMode::PickForNewSession, 2),
            Some(2)
        );
    }

    #[test]
    fn content_rel_y_at_zero_scroll_matches_pre_scroll_behaviour() {
        // Unscrolled: offset().y is 0, so this must reduce to the plain
        // `position.y - bounds.top()` the round-1 code used (and the
        // `drop_index` unit tests already pin against fixed pixel values).
        assert_eq!(content_rel_y(150.0, 20.0, 0.0), 130.0);
        assert_eq!(content_rel_y(20.0, 20.0, 0.0), 0.0);
        assert_eq!(content_rel_y(0.0, 20.0, 0.0), -20.0);
    }

    #[test]
    fn content_rel_y_scrolled_down_adds_back_the_scrolled_off_distance() {
        // Scrolled down by 3 rows (93px @ SPACE_ROW_SLOT): gpui reports that
        // as offset().y == -93.0 (content moved up under a fixed viewport).
        // A cursor sitting at the very top of the (now-scrolled) viewport is
        // really over content row 3, not row 0.
        let scrolled = -(SPACE_ROW_SLOT * 3.0);
        assert_eq!(content_rel_y(20.0, 20.0, scrolled), SPACE_ROW_SLOT * 3.0);
        assert_eq!(
            content_rel_y(20.0 + SPACE_ROW_SLOT, 20.0, scrolled),
            SPACE_ROW_SLOT * 4.0
        );
    }

    #[test]
    fn content_rel_y_feeds_drop_index_to_the_scrolled_row_not_the_clamped_top() {
        // The regression scenario itself: 12 spaces, scrolled down 4 rows,
        // cursor near the viewport top. Viewport-relative math would clamp
        // this into the visible top rows (the round-2 bug); content-relative
        // math lands on the actual row under the cursor.
        let count = 12;
        let scrolled = -(SPACE_ROW_SLOT * 4.0);
        let viewport_relative = 5.0; // just below the viewport's own top edge
        let rel_y = content_rel_y(20.0 + viewport_relative, 20.0, scrolled);
        assert_eq!(drop_index(rel_y, SPACE_ROW_SLOT, count), 4);
    }
}

/// A section header: label left, optional `+` right. `first` tucks the
/// block up under whatever sits above it.
pub(super) fn section_header(
    label: &'static str,
    first: bool,
    theme: &Theme,
    plus: Option<gpui::Stateful<gpui::Div>>,
) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .px(px(Theme::SPACE_SM))
        .pt(px(if first { 6.0 } else { 12.0 }))
        .pb(px(4.0))
        .child(
            div()
                .text_size(px(11.0))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(theme.text_muted.opacity(0.55))
                .child(SharedString::from(label)),
        )
        .when_some(plus, |el, plus| el.child(plus))
}

/// A grouped-panel server header: uppercase name (tracked, `font_mono`,
/// matching `popover::menu_heading`'s treatment) left, connection status
/// trailing. Non-interactive — servers switch via picking one of their
/// spaces, not by clicking the header (spec §7). The status copy is the
/// same match the old always-expanded server header built (round 1 of this
/// redesign, before the panel absorbed it).
fn render_panel_server_header(
    name: &str,
    connection: &comet_proto::RemoteConnectionState,
    theme: &Theme,
) -> gpui::Div {
    let status = match connection {
        comet_proto::RemoteConnectionState::Connecting => "Connecting".to_string(),
        comet_proto::RemoteConnectionState::Online => "Online".to_string(),
        comet_proto::RemoteConnectionState::Offline => "Offline".to_string(),
        comet_proto::RemoteConnectionState::Unreachable { message } => {
            format!("Unreachable · {message}")
        }
        comet_proto::RemoteConnectionState::IdentityChanged => "Identity changed".to_string(),
        comet_proto::RemoteConnectionState::IncompatibleVersion { remote } => {
            format!("Incompatible v{remote}")
        }
    };
    div()
        .px(px(7.0))
        .pt(px(6.0))
        .pb(px(2.0))
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(Theme::SPACE_SM))
        .text_size(px(9.5))
        .font_family(theme.font_mono.clone())
        .text_color(theme.text_muted.opacity(0.4))
        .child(
            div()
                .min_w_0()
                .truncate()
                .child(SharedString::from(popover::tracked_upper(name))),
        )
        .child(div().flex_none().child(SharedString::from(status)))
}

impl Shell {
    // ---- space switching ----

    /// Land in a space: remembered tab if alive, else the most recent chat in
    /// the space, else the new-session canvas. Persists `last_space_id`.
    pub(super) fn activate_space(&mut self, space_id: String, cx: &mut Context<Self>) {
        self.route = Route::Chat;
        self.state.update(cx, |s, cx| {
            s.select_space(Some(space_id.clone()), cx);
        });
        let target = {
            let state = self.state.read(cx);
            let in_space = |id: &str| {
                state
                    .visible_chats()
                    .any(|c| c.id == id && c.space_id.as_deref() == Some(space_id.as_str()))
            };
            self.space_last_chat
                .get(&space_id)
                .filter(|id| in_space(id))
                .cloned()
                .or_else(|| {
                    // `visible_chats` is recency-sorted — first match is the
                    // most recent chat of the space.
                    state
                        .visible_chats()
                        .find(|c| c.space_id.as_deref() == Some(space_id.as_str()))
                        .map(|c| c.id.clone())
                })
        };
        self.state.update(cx, |s, cx| s.select_chat(target, cx));
        self.state.update(cx, |s, _| {
            s.sidebar_scope = crate::state::SidebarScope::Space(space_id.clone());
        });
        self.settings.sidebar_scope_space = Some(space_id.clone());
        self.settings.last_space_id = Some(space_id);
        self.schedule_save(cx);
        cx.notify();
    }

    /// Widen the sidebar to every space. Deliberately does **not** touch
    /// `selected_space` or `selected_chat`: changing the scope never changes
    /// what is open.
    pub(super) fn activate_all_spaces(&mut self, cx: &mut Context<Self>) {
        self.state.update(cx, |s, _| {
            s.sidebar_scope = crate::state::SidebarScope::All;
        });
        self.settings.sidebar_scope_space = None;
        self.schedule_save(cx);
        cx.notify();
    }

    /// The active server, defaulting to the configured order's head when no
    /// `ServerChanged` has projected one yet — mirrors `AppState::
    /// current_server_id`'s own fallback (`state.rs`), which is private to
    /// that module.
    fn active_server_id(&self, cx: &Context<Self>) -> comet_proto::ServerId {
        let state = self.state.read(cx);
        state
            .selected_server_id()
            .cloned()
            .or_else(|| state.server_order.first().cloned())
            .unwrap_or_else(|| comet_proto::ServerId::new("local"))
    }

    /// Route a panel space pick through the right mutator. A plain
    /// `activate_space`/`create_session_in` qualifies the space id against
    /// whichever server is CURRENTLY active (`AppState::current_server_id`) —
    /// wrong when the row belongs to a different (grouped-panel) server.
    /// Switch first when it does; `select_server_bucket` only actually
    /// changes anything when the target differs from the active server, so
    /// this is a no-op for the common flat-panel case. Does not touch
    /// `space_dropdown_open`/`_highlight` — callers close on their own
    /// existing terms (the row click and the Enter-key handler already
    /// differed on this before this change; preserved as-is).
    fn pick_panel_space(
        &mut self,
        server_id: comet_proto::ServerId,
        space_id: String,
        mode: DropdownMode,
        cx: &mut Context<Self>,
    ) {
        if self.state.read(cx).selected_server_id() != Some(&server_id) {
            self.state.update(cx, |state, cx| {
                state.select_server_bucket(server_id);
                cx.notify();
            });
        }
        match mode {
            DropdownMode::Switch => self.activate_space(space_id, cx),
            DropdownMode::PickForNewSession => self.create_session_in(space_id, cx),
        }
    }

    // ---- sidebar sections ----

    /// Spaces (drag-resolved order) + the per-space device/offline/attention
    /// lookups — shared by the trigger, the dropdown panel, and its keyboard
    /// handler so all three agree on the same list.
    fn spaces_context(&self, cx: &Context<Self>) -> SpacesContext {
        let now = Utc::now();
        let state = self.state.read(cx);
        let spaces = state.spaces.clone();
        let device_names = spaces
            .iter()
            .map(|s| {
                (
                    s.device_id.clone(),
                    state
                        .device_name(&s.device_id)
                        .unwrap_or("Unknown device")
                        .to_string(),
                )
            })
            .collect();
        // Host-presence (the revived "Remote" signal): a remote space whose
        // device heartbeat lapsed shows offline — a host outage, not slow sync.
        let offline_devices = spaces
            .iter()
            .map(|s| s.device_id.clone())
            .filter(|id| !state.device_online(id, now))
            .collect();
        // Spaces with a live/awaiting session get an aggregate dot (the
        // most urgent member status wins) so the attention signal survives
        // even with the Sessions list scrolled off.
        let mut attention: std::collections::HashMap<String, ChatIndicator> =
            std::collections::HashMap::new();
        for chat in state.visible_chats() {
            let status = state.display_status_for(chat, now);
            if !matches!(
                status,
                ChatIndicator::Working | ChatIndicator::AwaitingInput
            ) {
                continue;
            }
            let Some(space_id) = chat.space_id.clone() else {
                continue;
            };
            attention
                .entry(space_id)
                .and_modify(|held| {
                    if crate::state::attention_rank(status) < crate::state::attention_rank(*held) {
                        *held = status;
                    }
                })
                .or_insert(status);
        }
        // Manual (drag) order overrides the synced creation order — device-
        // local, resolved exactly like the session-tab order.
        let spaces: Vec<Space> = {
            let created: Vec<String> = spaces.iter().map(|s| s.id.clone()).collect();
            let order = super::tabs::resolve_tab_order(&created, &self.settings.space_order);
            let mut by_id: std::collections::HashMap<String, Space> =
                spaces.into_iter().map(|s| (s.id.clone(), s)).collect();
            order.iter().filter_map(|id| by_id.remove(id)).collect()
        };
        SpacesContext {
            spaces,
            device_names,
            offline_devices,
            attention,
        }
    }

    /// The `(server_groups, active_server, ctx)` triple `panel_rows` needs —
    /// factored out so the keyboard handler (`space_dropdown_key`, which runs
    /// on its own key event, outside any render pass) can recompute the same
    /// inputs `render_scope_panel` does.
    fn panel_computation_inputs(
        &self,
        cx: &Context<Self>,
    ) -> (
        Vec<SidebarServerGroup>,
        comet_proto::ServerId,
        SpacesContext,
    ) {
        let server_groups = {
            let state = self.state.read(cx);
            project_sidebar_servers(&state.servers, &state.server_order)
        };
        let active_server = self.active_server_id(cx);
        let ctx = self.spaces_context(cx);
        (server_groups, active_server, ctx)
    }

    /// `(space_row_positions, host_offline)` for the current panel — the
    /// ↑↓ handler's half of `panel_rows`: which RENDERED row each space is
    /// (headers shift it in grouped mode — same translation `render_scope_
    /// panel` uses to land on the scoped space when the panel opens) and
    /// whether each space is reachable.
    fn panel_nav_lookup(&self, cx: &Context<Self>) -> (Vec<usize>, Vec<bool>) {
        let (server_groups, active_server, ctx) = self.panel_computation_inputs(cx);
        let now = Utc::now();
        let local_device_id = self.state.read(cx).local_device_id.clone();
        let rows = panel_rows(
            &ctx,
            &server_groups,
            &active_server,
            local_device_id.as_deref(),
            now,
        );
        let mut space_row_positions = Vec::new();
        let mut host_offline = Vec::new();
        for (row_ix, row) in rows.iter().enumerate() {
            if let PanelRow::Space(entry) = row {
                space_row_positions.push(row_ix);
                host_offline.push(entry.host_offline);
            }
        }
        (space_row_positions, host_offline)
    }

    /// The "Spaces" section: tracked header + add button, then the scope
    /// trigger (Task 7 — replaces the old row-per-space list with a single
    /// 28px row + dropdown panel, so the section's height no longer grows
    /// with the space count).
    pub(super) fn render_spaces_section(
        &mut self,
        window: &mut Window,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // A drag that ended off-list (no drop event) must not strand the
        // sibling slide offsets.
        if self.space_drag.is_some() && !cx.has_active_drag() {
            self.space_drag = None;
        }
        let server_groups = {
            let state = self.state.read(cx);
            project_sidebar_servers(&state.servers, &state.server_order)
        };
        let active_server = self.active_server_id(cx);
        let ctx = self.spaces_context(cx);

        let header = section_header(
            "Spaces",
            true,
            theme,
            Some(header_plus(
                "add-space",
                theme,
                cx.listener(|this, _, _, cx| this.open_add_space(cx)),
            )),
        );

        // The old per-server header+space(+chat) tree that rendered whenever
        // 2+ servers were configured is gone — the scope trigger/panel below
        // now absorbs it (`PanelItem::ServerHeader`, `panel_grouped_items`)
        // for BOTH space counts, so there is exactly one Spaces UI regardless
        // of how many servers are configured. This is also the fix for the
        // round-1-deferred bug: that gate (`!grouped_mode`) used to skip the
        // trigger/panel entirely with 2+ servers, so the Sessions `+` (which
        // opens the panel in `PickForNewSession` mode) rendered nothing and
        // left `space_dropdown_open` stuck set.
        div()
            .flex()
            .flex_col()
            .child(header)
            .child(self.render_scope_trigger(
                window,
                theme,
                &ctx,
                &server_groups,
                &active_server,
                cx,
            ))
            .into_any_element()
    }

    /// The scope trigger: one row reading "All spaces" or the space the
    /// sidebar is scoped to (Task 7's fixed-height replacement for the old
    /// per-space row list). Opens/closes [`Self::render_scope_panel`].
    #[allow(clippy::too_many_arguments)]
    fn render_scope_trigger(
        &mut self,
        window: &mut Window,
        theme: &Theme,
        ctx: &SpacesContext,
        server_groups: &[SidebarServerGroup],
        active_server: &comet_proto::ServerId,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let scope = self.state.read(cx).sidebar_scope.clone();
        let mode = self.space_dropdown_open;
        let open = mode.is_some();
        let active = scope
            .space_id()
            .and_then(|id| ctx.spaces.iter().find(|s| s.id == id));
        // Step 7: the same total feeds both the trigger (on `All`) and the
        // panel's `All spaces` row.
        let session_total = self.state.read(cx).visible_chats().count();
        // §7: with several servers configured, `comet @ mac-studio` on two
        // different machines is ambiguous — a scoped trigger disambiguates
        // with the server name, and `All spaces` swaps the (now
        // active-server-scoped, no longer global) session count for the
        // server name outright.
        let grouped = server_groups.len() > 1;
        let active_server_name = grouped
            .then(|| {
                server_groups
                    .iter()
                    .find(|g| &g.server.id == active_server)
                    .map(|g| g.server.name.clone())
            })
            .flatten();
        // Task 9: aggregate attention across every space when the scope is
        // `All` (mirrors the per-space dot in the panel below). Placeholder
        // until Task 9 wires the real aggregate.
        let scope_attention: Option<ChatIndicator> = None;

        let mut trigger = div()
            .id("space-scope")
            .flex()
            .flex_row()
            .items_center()
            .gap(px(Theme::SPACE_SM))
            .rounded(px(8.0))
            .px(px(Theme::SPACE_SM))
            .py(px(5.0))
            .text_size(px(13.0))
            .line_height(px(18.0))
            .font_weight(gpui::FontWeight::MEDIUM)
            .cursor_pointer()
            // A resting wash so this reads as a control, not a selected row.
            .bg(if open {
                crate::theme::glass_selected_bg()
            } else {
                motion::hover_blend(
                    "space-scope",
                    crate::theme::wash(0.055),
                    crate::theme::wash(0.085),
                )
            })
            .when(open, |el| el.shadow(crate::theme::glass_selected_shadows()))
            .on_hover(motion::hover_listener("space-scope"))
            .on_click(cx.listener(|this, _, _, cx| {
                // `on_mouse_down_out` on the panel fires on the mouse-DOWN of
                // this same click (capture phase) when the panel is open —
                // it closes the panel first, then this `on_click` (mouse-up)
                // would otherwise see `None` and reopen it. Suppress that
                // one reopen (`settings::accounts`'s `device_menu_dismissed_at`).
                let just_dismissed = this
                    .space_dropdown_dismissed_at
                    .is_some_and(|at| at.elapsed() < Duration::from_millis(400));
                this.space_dropdown_open = match this.space_dropdown_open {
                    Some(_) => None,
                    None if just_dismissed => None,
                    None => Some(DropdownMode::Switch),
                };
                this.space_dropdown_dismissed_at = None;
                this.space_dropdown_highlight = None;
                this.space_dropdown_focus_pending = this.space_dropdown_open.is_some();
                cx.notify();
            }))
            .child(
                div()
                    .size(px(6.0))
                    .rounded_full()
                    .flex_none()
                    .bg(scope_attention
                        .map(|s| status_dot_color(s, theme))
                        .unwrap_or_else(|| crate::theme::ink(0.14))),
            )
            .child(
                icon(if active.is_some() {
                    icons::FOLDER
                } else {
                    icons::LIST
                })
                .size(px(16.0))
                .flex_none()
                .text_color(theme.text_muted),
            )
            .child(
                div().min_w_0().truncate().child(SharedString::from(
                    active
                        .map(|s| s.display_name().to_string())
                        .unwrap_or_else(|| "All spaces".to_string()),
                )),
            )
            .child(div().flex_1())
            // Trailing slot: the device when scoped, the session total on All.
            // A scoped trigger goes amber when its host is offline, matching
            // the space row it replaced.
            .child({
                let host_offline =
                    active.is_some_and(|s| ctx.offline_devices.contains(&s.device_id));
                div()
                    .flex_none()
                    .min_w_0()
                    .truncate()
                    .text_size(px(11.0))
                    .font_weight(gpui::FontWeight::NORMAL)
                    .text_color(if host_offline {
                        theme.warning.opacity(0.8)
                    } else {
                        theme.text_muted.opacity(0.55)
                    })
                    .child(SharedString::from(match active {
                        Some(s) => {
                            let device = ctx
                                .device_names
                                .get(&s.device_id)
                                .cloned()
                                .unwrap_or_default();
                            match (host_offline, &active_server_name) {
                                (true, Some(server)) => format!("@ {device} · {server} · offline"),
                                (true, None) => format!("@ {device} · offline"),
                                (false, Some(server)) => format!("@ {device} · {server}"),
                                (false, None) => format!("@ {device}"),
                            }
                        }
                        None => active_server_name
                            .clone()
                            .unwrap_or_else(|| session_total.to_string()),
                    }))
            })
            .child(
                icon(icons::ALT_ARROW_DOWN)
                    .size(px(13.0))
                    .flex_none()
                    .text_color(theme.text_muted.opacity(0.5)),
            );

        if let Some(mode) = mode {
            let panel = self.render_scope_panel(
                window,
                theme,
                mode,
                ctx,
                server_groups,
                active_server,
                session_total,
                cx,
            );
            trigger = trigger.child(panel);
        }

        trigger.into_any_element()
    }

    /// The scope-dropdown panel: `All spaces` (Switch only) + one row per
    /// space + `Add space…` (Switch only). Mounted through
    /// [`popover::anchored_menu_below`] — the trigger is a real 28px row (not
    /// an icon button), so it needs the bottom-pinned variant, not
    /// `anchored_menu` (which would cover most of the trigger).
    #[allow(clippy::too_many_arguments)]
    fn render_scope_panel(
        &mut self,
        window: &mut Window,
        theme: &Theme,
        mode: DropdownMode,
        ctx: &SpacesContext,
        server_groups: &[SidebarServerGroup],
        active_server: &comet_proto::ServerId,
        session_total: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // Capture keyboard focus once, the moment the panel opens (the
        // `AddSpaceFlow` idiom) — without this ↑↓/Enter/Esc have nothing to
        // dispatch to.
        let just_opened = std::mem::take(&mut self.space_dropdown_focus_pending);
        if just_opened {
            let handle = self.space_dropdown_focus.clone();
            window.focus(&handle, cx);
        }

        let scope = self.state.read(cx).sidebar_scope.clone();
        let is_pick = mode == DropdownMode::PickForNewSession;
        let grouped = server_groups.len() > 1;
        let now = Utc::now();
        let local_device_id = self.state.read(cx).local_device_id.clone();
        let rows = panel_rows(
            ctx,
            server_groups,
            active_server,
            local_device_id.as_deref(),
            now,
        );
        // The flat "space index" nav/scroll math keys off (`dropdown_navigable_
        // positions`, `space_dropdown_key`) and the RENDERED row index (`rows`,
        // headers included) are different sequences once headers are in play —
        // `space_row_positions[space_ix]` translates one to the other so
        // `scroll_to_item` (which addresses rendered children) never targets
        // the wrong row.
        let mut space_entries: Vec<&PanelSpaceEntry> = Vec::new();
        let mut space_row_positions: Vec<usize> = Vec::new();
        for (row_ix, row) in rows.iter().enumerate() {
            if let PanelRow::Space(entry) = row {
                space_entries.push(entry);
                space_row_positions.push(row_ix);
            }
        }
        // `active` flags, sourced from the same tested function the module's
        // unit tests pin against — NOT `panel_grouped_items` unconditionally:
        // its `groups.len() <= 1` branch walks `group.spaces` (creation
        // order, `project_sidebar_servers`), but `panel_rows`'s flat branch
        // (and `space_entries` above) walks `ctx.spaces` — the DRAG-REORDER-
        // aware order. Those two orders diverge the moment a user drags a row,
        // so mixing them would silently pair the wrong `active` flag with the
        // wrong row. Call `panel_items` directly on `ctx.spaces` instead when
        // flat; `panel_grouped_items` is safe as-is when grouped, since then
        // `panel_rows` ALSO walks `group.spaces` — the two traversals match.
        let items = if grouped {
            panel_grouped_items(&scope, server_groups)
        } else {
            panel_items(&scope, &ctx.spaces)
        };
        let space_active: Vec<bool> = items
            .iter()
            .filter_map(|item| match item {
                PanelItem::Space { active, .. } => Some(*active),
                _ => None,
            })
            .collect();
        // A scoped space below row 8 would otherwise open with its check
        // mark scrolled out of view — land the panel on it immediately.
        if just_opened
            && let Some(space_ix) = scope
                .space_id()
                .and_then(|id| space_entries.iter().position(|e| e.space.id == id))
        {
            self.space_panel_scroll
                .scroll_to_item(space_row_positions[space_ix]);
        }
        let host_offline: Vec<bool> = space_entries.iter().map(|e| e.host_offline).collect();
        let navigable = dropdown_navigable_positions(mode, &host_offline);
        let highlighted_pos = self
            .space_dropdown_highlight
            .and_then(|h| navigable.get(h))
            .copied();
        let all_unreachable = is_pick && !space_entries.is_empty() && navigable.is_empty();

        let mut card = popover::popover_card(theme)
            .w(px(self.settings.sidebar_width - 16.0))
            .track_focus(&self.space_dropdown_focus)
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                this.space_dropdown_key(event, cx)
            }))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.space_dropdown_open = None;
                this.space_dropdown_highlight = None;
                this.space_dropdown_dismissed_at = Some(std::time::Instant::now());
                cx.notify();
            }))
            .flex()
            .flex_col();

        if is_pick {
            // Non-interactive lead — a session cannot go to `All spaces`, so
            // this mode never shows that item.
            card = card.child(
                div()
                    .px(px(7.0))
                    .pt(px(6.0))
                    .pb(px(2.0))
                    .text_size(px(9.5))
                    .text_color(theme.text_muted.opacity(0.4))
                    .child(SharedString::from("New session in…")),
            );
        } else {
            let all_active = matches!(items[0], PanelItem::AllSpaces { active: true });
            let highlighted = highlighted_pos == Some(0);
            card = card.child(
                popover::menu_row_nav(theme, all_active, highlighted, "space-panel-all")
                    .id("space-panel-all")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.activate_all_spaces(cx);
                        this.space_dropdown_open = None;
                        this.space_dropdown_highlight = None;
                        cx.notify();
                    }))
                    .child(
                        icon(icons::LIST)
                            .size(px(16.0))
                            .flex_none()
                            .text_color(theme.text_muted),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(SharedString::from("All spaces")),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(11.0))
                            .text_color(theme.text_muted.opacity(0.55))
                            .child(session_total.to_string()),
                    )
                    .when(all_active, |el| el.child(popover::menu_check(theme))),
            );
        }

        // Two different offsets, easy to conflate: a space's position among
        // RENDERED rows (`rows`, headers included) is not its logical
        // NAVIGATION position — headers occupy a row but never a position,
        // and the navigation position space only reserves slot 0 for
        // `AllSpaces` in `Switch` mode. `space_ix` (incremented only on
        // `PanelRow::Space`) is the latter; `position_offset + space_ix`
        // matches it to `highlighted_pos`.
        let position_offset = dropdown_offset(mode);
        let drag = self
            .space_drag
            .as_ref()
            .map(|d| (d.from, d.over, d.epoch, d.prev_over));
        let mut space_ix = 0usize;
        let row_elements: Vec<AnyElement> = rows
            .iter()
            .map(|row| match row {
                PanelRow::Header { name, connection } => {
                    render_panel_server_header(name, connection, theme).into_any_element()
                }
                PanelRow::Space(entry) => {
                    let ix = space_ix;
                    space_ix += 1;
                    let id = entry.space.id.clone();
                    let active = space_active.get(ix).copied().unwrap_or(false);
                    let unreachable = is_pick && entry.host_offline;
                    let highlighted = highlighted_pos == Some(position_offset + ix);
                    let row = self.render_panel_space_row(
                        ix,
                        &entry.server_id,
                        &entry.space,
                        entry.device_name.clone(),
                        entry.host_offline,
                        active,
                        unreachable,
                        highlighted,
                        entry.attention,
                        mode,
                        !grouped,
                        theme,
                        cx,
                    );
                    // Sliding transform while a sibling is dragged over —
                    // Task 6's finding: the machinery moves over verbatim, no
                    // coordinate adjustment needed for the popover. Grouped
                    // rows never carry a drag (see `render_panel_space_row`'s
                    // `draggable` guard below) — `settings.space_order` has
                    // no notion of which server an id belongs to, so
                    // reordering across server groups is not attempted.
                    if !grouped && mode == DropdownMode::Switch {
                        match drag {
                            Some((from, over, epoch, prev_over)) if ix != from => {
                                let target = slide_offset(ix, from, over) * SPACE_ROW_SLOT;
                                let start = slide_offset(ix, from, prev_over) * SPACE_ROW_SLOT;
                                div()
                                    .relative()
                                    .child(row.with_animation(
                                        SharedString::from(format!(
                                            "space-panel-slide-{id}-{epoch}"
                                        )),
                                        TAB_SLIDE.animation(),
                                        move |el, t| el.top(px(motion::lerp(start, target, t))),
                                    ))
                                    .into_any_element()
                            }
                            // The dragged row renders as an invisible spacer;
                            // the cursor ghost represents it.
                            Some((from, ..)) if ix == from => div()
                                .h(px(SPACE_ROW_SLOT - 2.0))
                                .flex_none()
                                .into_any_element(),
                            _ => row.into_any_element(),
                        }
                    } else {
                        row.into_any_element()
                    }
                }
            })
            .collect();

        // Drag-reorder only makes sense while switching scope on a flat
        // (single-server) panel — the picker is a one-shot "where does this
        // session go" prompt, and grouped rows never originate a drag.
        let rows_container = if !grouped && mode == DropdownMode::Switch {
            // `DragMoveEvent::bounds` is the scroll VIEWPORT, invariant under
            // scrolling — captured before the `move` closure, the
            // `shell::tabs` horizontal-drag idiom (`tabs.rs:499`).
            let scroll_for_drag = self.space_panel_scroll.clone();
            let count = space_entries.len();
            div()
                .id("space-panel-rows")
                .flex()
                .flex_col()
                .gap(px(2.0))
                .max_h(px(PANEL_ROWS_MAX_H))
                .overflow_y_scroll()
                .track_scroll(&self.space_panel_scroll)
                .on_drag_move::<SpaceDragPayload>(cx.listener(
                    move |this, event: &gpui::DragMoveEvent<SpaceDragPayload>, _, cx| {
                        let from = event.drag(cx).from;
                        let rel_y = content_rel_y(
                            f32::from(event.event.position.y),
                            f32::from(event.bounds.top()),
                            f32::from(scroll_for_drag.offset().y),
                        );
                        let over = drop_index(rel_y, SPACE_ROW_SLOT, count);
                        this.update_space_drag_over(from, over, cx);
                    },
                ))
                .on_drop::<SpaceDragPayload>(cx.listener(
                    move |this, payload: &SpaceDragPayload, _, cx| {
                        let to = this
                            .space_drag
                            .as_ref()
                            .map(|d| d.over)
                            .unwrap_or(payload.from);
                        this.commit_space_reorder(payload.from, to, cx);
                    },
                ))
                .children(row_elements)
        } else {
            div()
                .id("space-panel-rows")
                .flex()
                .flex_col()
                .gap(px(2.0))
                .max_h(px(PANEL_ROWS_MAX_H))
                .overflow_y_scroll()
                .track_scroll(&self.space_panel_scroll)
                .children(row_elements)
        };
        card = card.child(rows_container);

        if all_unreachable {
            card = card.child(
                div()
                    .px(px(8.0))
                    .py(px(8.0))
                    .text_size(px(12.0))
                    .text_color(theme.text_faint)
                    .child(SharedString::from("No space is reachable right now.")),
            );
        }

        if !is_pick {
            card = card.child(popover::menu_separator()).child(
                popover::menu_row(theme, false, "space-panel-add")
                    .id("space-panel-add")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.space_dropdown_open = None;
                        this.space_dropdown_highlight = None;
                        this.open_add_space(cx);
                    }))
                    .child(
                        icon(icons::PLUS)
                            .size(px(16.0))
                            .flex_none()
                            .text_color(theme.text_muted),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(SharedString::from("Add space…")),
                    )
                    .child(popover::kbd_hint(
                        theme,
                        &crate::settings::display_combo("mod-k"),
                    )),
            );
        }

        popover::anchored_menu_below("space-scope-panel", card.into_any_element())
    }

    /// Panel keys (bubbling from the focused card): ↑↓ move the highlight
    /// across the navigable rows, ↵ picks, Esc closes without changing the
    /// scope (`AddSpaceFlow::add_space_key`'s shape).
    fn space_dropdown_key(&mut self, event: &gpui::KeyDownEvent, cx: &mut Context<Self>) {
        let Some(mode) = self.space_dropdown_open else {
            return;
        };
        let key = popover::classify_key(
            event.keystroke.key.as_str(),
            event.keystroke.modifiers.platform,
            event.keystroke.modifiers.control,
        );
        match key {
            popover::MenuKey::Escape => {
                self.space_dropdown_open = None;
                self.space_dropdown_highlight = None;
                cx.notify();
            }
            popover::MenuKey::Up | popover::MenuKey::Down => {
                let (space_row_positions, host_offline) = self.panel_nav_lookup(cx);
                let navigable = dropdown_navigable_positions(mode, &host_offline);
                let delta = if key == popover::MenuKey::Up { -1 } else { 1 };
                self.space_dropdown_highlight =
                    popover::menu_step(self.space_dropdown_highlight, navigable.len(), delta);
                // Keep the highlighted row in view — the capped panel can now
                // scroll past it (`AddSpaceFlow::add_space_key`'s
                // `list_scroll.scroll_to_item`). `All spaces` (no space
                // index) sits above the scrollable region, so nothing to do.
                // `space_row_positions` translates a space index to its
                // RENDERED row index (headers shift it in grouped mode) —
                // same translation `render_scope_panel` uses to land on the
                // scoped space when the panel opens.
                if let Some(row_ix) = self
                    .space_dropdown_highlight
                    .and_then(|h| navigable.get(h))
                    .copied()
                    .and_then(|pos| dropdown_position_to_space_index(mode, pos))
                    .and_then(|space_ix| space_row_positions.get(space_ix).copied())
                {
                    self.space_panel_scroll.scroll_to_item(row_ix);
                }
                cx.notify();
            }
            popover::MenuKey::Enter => {
                let (server_groups, active_server, ctx) = self.panel_computation_inputs(cx);
                let now = Utc::now();
                let local_device_id = self.state.read(cx).local_device_id.clone();
                let rows = panel_rows(
                    &ctx,
                    &server_groups,
                    &active_server,
                    local_device_id.as_deref(),
                    now,
                );
                let space_entries: Vec<&PanelSpaceEntry> = rows
                    .iter()
                    .filter_map(|r| match r {
                        PanelRow::Space(e) => Some(e),
                        PanelRow::Header { .. } => None,
                    })
                    .collect();
                let host_offline: Vec<bool> =
                    space_entries.iter().map(|e| e.host_offline).collect();
                let navigable = dropdown_navigable_positions(mode, &host_offline);
                if let Some(pos) = self
                    .space_dropdown_highlight
                    .and_then(|h| navigable.get(h))
                    .copied()
                {
                    match dropdown_position_to_space_index(mode, pos) {
                        None => self.activate_all_spaces(cx),
                        Some(ix) => {
                            if let Some(entry) = space_entries.get(ix) {
                                let server_id = entry.server_id.clone();
                                let space_id = entry.space.id.clone();
                                self.pick_panel_space(server_id, space_id, mode, cx);
                            }
                        }
                    }
                    self.space_dropdown_open = None;
                    self.space_dropdown_highlight = None;
                    cx.notify();
                }
            }
            popover::MenuKey::ModEnter | popover::MenuKey::Backspace | popover::MenuKey::Other => {}
        }
    }

    /// One panel space row: a folder-less row (every row here is already a
    /// space) with a trailing check when `active`, and (`PickForNewSession`)
    /// an offline host rendered unreachable rather than clickable.
    /// `draggable` is `false` for grouped rows: `settings.space_order` has no
    /// notion of which server an id belongs to, so cross-server drag-reorder
    /// is not attempted (only within a single server's flat panel).
    #[allow(clippy::too_many_arguments)]
    fn render_panel_space_row(
        &self,
        ix: usize,
        server_id: &comet_proto::ServerId,
        space: &Space,
        device_name: String,
        host_offline: bool,
        active: bool,
        unreachable: bool,
        highlighted: bool,
        attention: Option<ChatIndicator>,
        mode: DropdownMode,
        draggable: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let id = space.id.clone();
        let name: SharedString = space.display_name().to_string().into();
        let fade_key = format!("space-panel-row-{id}");
        let rest_bg = if active {
            crate::theme::glass_selected_bg()
        } else {
            crate::theme::wash(0.0)
        };
        let rest_text = if active {
            theme.text
        } else {
            theme.text.opacity(0.8)
        };
        let select_id = id.clone();
        let select_server = server_id.clone();
        let menu_id = id.clone();
        let menu_server = server_id.clone();

        let row = div()
            .id(SharedString::from(format!("space-panel-{id}")))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(Theme::SPACE_SM))
            .rounded(px(8.0))
            .px(px(Theme::SPACE_SM))
            .py(px(6.0));

        // The keyboard cursor: a `card_selected_bg()` wash + full text,
        // matching `menu_row_nav`'s contract exactly (`popover.rs`) — NOT the
        // same treatment as `active` (a `glass_selected_shadows()` ring),
        // else arrowing a non-active row painted the identical ring as
        // selection, and highlighting the already-active row painted
        // nothing extra at all. `!active` here mirrors `menu_row_nav`'s own
        // `!selected && highlighted`: an active row never needs a second
        // "this one" indicator.
        let show_highlight = highlighted && !active;

        let row = if unreachable {
            // No hover fill, no click — the row stays listed so the picker
            // shows the whole shape of your setup, just unreachable. (Never
            // highlighted in practice: `dropdown_navigable_positions` skips
            // unreachable rows in `PickForNewSession`.)
            row.opacity(0.45).text_color(rest_text).bg(rest_bg)
        } else {
            let mut row = row
                .text_color(if show_highlight {
                    theme.text
                } else {
                    motion::hover_blend(&fade_key, rest_text, theme.text)
                })
                .bg(if show_highlight {
                    crate::theme::card_selected_bg()
                } else {
                    motion::hover_blend(
                        &fade_key,
                        rest_bg,
                        if active { rest_bg } else { theme.glass_hover() },
                    )
                })
                .when(active, |el| {
                    el.shadow(crate::theme::glass_selected_shadows())
                })
                .on_hover(motion::hover_listener(fade_key))
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.pick_panel_space(select_server.clone(), select_id.clone(), mode, cx);
                    if mode == DropdownMode::Switch {
                        this.space_dropdown_open = None;
                        this.space_dropdown_highlight = None;
                    }
                    cx.notify();
                }))
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                        if this.state.read(cx).selected_server_id() != Some(&menu_server) {
                            this.state.update(cx, |state, cx| {
                                state.select_server_bucket(menu_server.clone());
                                cx.notify();
                            });
                        }
                        this.space_menu = Some((menu_id.clone(), event.position));
                        this.space_dropdown_open = None;
                        this.space_dropdown_highlight = None;
                        cx.notify();
                    }),
                );
            // Drag-reorder only in Switch mode on a flat (single-server)
            // panel — see this function's doc comment.
            if draggable && mode == DropdownMode::Switch {
                row = row.on_drag(
                    SpaceDragPayload {
                        from: ix,
                        name: name.clone(),
                    },
                    |payload, _point, _, cx| {
                        let name = payload.name.clone();
                        cx.stop_propagation();
                        cx.new(|_| SpaceGhost { name })
                    },
                );
            }
            row
        };

        row.child(
            div().size(px(6.0)).rounded_full().flex_none().bg(attention
                .map(|status| status_dot_color(status, theme))
                .unwrap_or_else(|| crate::theme::ink(0.14))),
        )
        .child(
            div()
                .min_w_0()
                .truncate()
                .text_size(px(13.0))
                .line_height(px(17.0))
                .font_weight(gpui::FontWeight::MEDIUM)
                .child(name),
        )
        .child(div().flex_1())
        .child(
            div()
                .flex_none()
                .min_w_0()
                .truncate()
                .text_size(px(12.0))
                .line_height(px(17.0))
                .text_color(if host_offline {
                    theme.warning.opacity(0.8)
                } else {
                    theme.text_muted.opacity(0.6)
                })
                .child(SharedString::from(if host_offline {
                    format!("@ {device_name} · offline")
                } else {
                    format!("@ {device_name}")
                })),
        )
        .when(active, |el| el.child(popover::menu_check(theme)))
    }

    /// Track the drop slot while a space row is dragged over the list (150ms
    /// sibling slides restart per committed `over` change).
    fn update_space_drag_over(&mut self, from: usize, over: usize, cx: &mut Context<Self>) {
        match &mut self.space_drag {
            Some(drag) if drag.from == from => {
                if drag.over != over {
                    drag.prev_over = drag.over;
                    drag.over = over;
                    drag.epoch += 1;
                    cx.notify();
                }
            }
            _ => {
                self.space_drag = Some(SpaceDragState {
                    from,
                    over,
                    epoch: 0,
                    prev_over: from,
                });
                cx.notify();
            }
        }
    }

    /// Commit a drag: persist the new visual order (device-local).
    fn commit_space_reorder(&mut self, from: usize, to: usize, cx: &mut Context<Self>) {
        let created: Vec<String> = self
            .state
            .read(cx)
            .spaces
            .iter()
            .map(|s| s.id.clone())
            .collect();
        let mut order = super::tabs::resolve_tab_order(&created, &self.settings.space_order);
        if from < order.len() {
            reorder_tabs(&mut order, from, to);
            self.settings.space_order = order;
            self.schedule_save(cx);
        }
        self.space_drag = None;
        cx.notify();
    }

    /// The global "Sessions" list: every session across all spaces (idle
    /// included), attention-sorted. Rows are keyed for the FLIP resort glide.
    pub(super) fn render_active_rows(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Vec<(String, f32, AnyElement)> {
        let scoped = self.state.read(cx).sidebar_scope.space_id().is_some();
        let now = Utc::now();
        let rows: Vec<(ChatIndicator, comet_proto::Chat, RowScope, Option<String>)> = {
            let state = self.state.read(cx);
            state
                .overview_chats(now)
                .into_iter()
                .map(|(status, chat)| {
                    let space = state.space_for_chat(chat);
                    let scope = if scoped {
                        RowScope::One
                    } else {
                        RowScope::All {
                            space: space
                                .map(|s| s.display_name().to_string())
                                .unwrap_or_else(|| "?".to_string())
                                .into(),
                            device: state
                                .device_name(&chat.device_id)
                                .unwrap_or("Unknown device")
                                .to_string()
                                .into(),
                            host_offline: !state.device_online(&chat.device_id, now),
                        }
                    };
                    // The branch shows whenever the engine has stamped one —
                    // main-checkout sessions included, not just worktrees.
                    let branch = chat
                        .branch
                        .as_deref()
                        .map(str::trim)
                        .filter(|b| !b.is_empty())
                        .map(str::to_string);
                    (status, chat.clone(), scope, branch)
                })
                .collect()
        };
        let selected = self
            .state
            .read(cx)
            .selected_chat
            .clone()
            .map(|id| id.local_id);
        rows.into_iter()
            .map(|(status, chat, scope, branch)| {
                let time_ago: SharedString =
                    format_time_ago(chat.last_message_at.unwrap_or(chat.created_at), now).into();
                let is_selected = selected.as_deref() == Some(chat.id.as_str());
                let height = super::chat_row_height(&scope);
                let harness = chat.config.as_ref().map(|c| c.harness);
                let element = self.render_chat_row(
                    chat.id.clone(),
                    transcript::single_line(
                        &chat.title.clone().unwrap_or_else(|| "New session".into()),
                    )
                    .into(),
                    time_ago,
                    scope,
                    branch.map(SharedString::from),
                    harness,
                    status,
                    is_selected,
                    theme,
                    cx,
                );
                (format!("c:{}", chat.id), height, element)
            })
            .collect()
    }

    // ---- add-space flow (the ⌘K palette) ----

    pub(super) fn open_add_space(&mut self, cx: &mut Context<Self>) {
        let (targets, local) = {
            let state = self.state.read(cx);
            let groups = project_sidebar_servers(&state.servers, &state.server_order);
            (browse_targets(&groups), state.local_device_id.clone())
        };
        // Land on this installation's own device (else the first online
        // target) — `browse_targets` spans every ONLINE server, so a
        // freshly paired remote with zero spaces is reachable here even
        // though it has no space row of its own yet (Task 8b).
        let device = default_browse_target(&targets, local.as_deref());
        // "PaletteSearch" context: navigation keys stay unbound so ↑↓/←/→/⏎
        // bubble to the palette frame (`add_space_key`) instead of moving the
        // text caret — Enter and ⌘Enter are both handled there.
        let search =
            cx.new(|cx| ComposerInput::with_context("Search folders…", "PaletteSearch", cx));
        let search_events = cx.subscribe(&search, |this: &mut Shell, _, event, cx| {
            if matches!(event, ComposerInputEvent::Edited) {
                if let Some(flow) = this.add_space.as_mut() {
                    flow.active = 0;
                }
                cx.notify();
            }
        });
        let has_device = device.is_some();
        self.add_space = Some(AddSpaceFlow {
            device,
            search,
            browser: Loadable::Idle,
            browser_path: None,
            home: None,
            browser_repo: false,
            active: 0,
            submit_busy: false,
            error: None,
            focus: cx.focus_handle(),
            list_scroll: gpui::ScrollHandle::new(),
            focus_pending: true,
            load_task: None,
            submit_task: None,
            _search_events: search_events,
        });
        if has_device {
            self.load_space_folders(None, cx);
        }
        cx.notify();
    }

    /// Devices-rail click: rebrowse the same palette on another machine
    /// (device + owning server) — the rail spans every online server, not
    /// only the active one (Task 8b).
    fn add_space_pick_target(&mut self, target: BrowseTarget, cx: &mut Context<Self>) {
        let Some(flow) = self.add_space.as_mut() else {
            return;
        };
        if flow
            .device
            .as_ref()
            .is_some_and(|d| d.server_id == target.server_id && d.device.id == target.device.id)
        {
            return;
        }
        flow.device = Some(target);
        flow.browser = Loadable::Idle;
        flow.browser_path = None;
        flow.home = None;
        flow.browser_repo = false;
        flow.active = 0;
        flow.error = None;
        let search = flow.search.clone();
        search.update(cx, |input, cx| input.set_text("", cx));
        self.load_space_folders(None, cx);
        cx.notify();
    }

    /// The current listing's folder rows filtered by the search query
    /// (prefix matches first — `popover::filter_indices`).
    fn add_space_filtered(&self, cx: &App) -> Vec<comet_proto::FolderEntry> {
        let Some(flow) = self.add_space.as_ref() else {
            return Vec::new();
        };
        let Some(listing) = flow.browser.ready() else {
            return Vec::new();
        };
        let dirs = browser_rows(listing);
        let query = flow.search.read(cx).text().to_string();
        let names: Vec<&str> = dirs.iter().map(|e| e.name.as_str()).collect();
        popover::filter_indices(&query, &names)
            .into_iter()
            .map(|ix| dirs[ix].clone())
            .collect()
    }

    /// Descend into the highlighted (filtered) folder; clears the query.
    fn add_space_open_active(&mut self, cx: &mut Context<Self>) {
        let rows = self.add_space_filtered(cx);
        let Some(flow) = self.add_space.as_ref() else {
            return;
        };
        let Some(listing) = flow.browser.ready() else {
            return;
        };
        let Some(entry) = rows.get(flow.active) else {
            return;
        };
        let full = crate::pickers::child_path(&listing.path, &entry.name);
        let is_repo = entry.is_repo;
        let search = flow.search.clone();
        if let Some(flow) = self.add_space.as_mut() {
            flow.browser_repo = is_repo;
        }
        search.update(cx, |input, cx| input.set_text("", cx));
        self.load_space_folders(Some(full), cx);
    }

    /// Descend into a specific folder row (mouse path); clears the query.
    fn add_space_descend(&mut self, full: String, is_repo: bool, cx: &mut Context<Self>) {
        let Some(flow) = self.add_space.as_mut() else {
            return;
        };
        flow.browser_repo = is_repo;
        let search = flow.search.clone();
        search.update(cx, |input, cx| input.set_text("", cx));
        self.load_space_folders(Some(full), cx);
    }

    /// ListFolders on the flow's target device (relay-forwarded when
    /// remote) — resolved via `client_for` against the TARGET's owning
    /// server, which may not be the active one (Task 8b).
    pub(super) fn load_space_folders(&mut self, path: Option<String>, cx: &mut Context<Self>) {
        let Some(target) = self.add_space.as_ref().and_then(|f| f.device.clone()) else {
            return;
        };
        let Some(engine) = self
            .state
            .read(cx)
            .client_for(&comet_proto::ServerRef::new(target.server_id, ""))
        else {
            return;
        };
        let Some(flow) = self.add_space.as_mut() else {
            return;
        };
        let went_home = path.is_none();
        flow.browser_path = path.clone();
        flow.browser = Loadable::Loading;
        flow.active = 0;
        flow.list_scroll.set_offset(gpui::Point::default());
        flow.load_task = Some(cx.spawn(async move |this, cx| {
            let mut params = serde_json::Map::new();
            if let Some(p) = &path {
                params.insert("path".into(), serde_json::Value::String(p.clone()));
            }
            let result = engine
                .client()
                .call(methods::LIST_FOLDERS, serde_json::Value::Object(params))
                .await;
            this.update(cx, |shell, cx| {
                if let Some(flow) = shell.add_space.as_mut() {
                    flow.browser = match result {
                        Ok(value) => match serde_json::from_value::<FolderListing>(value) {
                            Ok(listing) => {
                                // A pathless browse resolved home — remember it
                                // so the breadcrumbs can fold it into the
                                // device crumb.
                                if went_home {
                                    flow.home = Some(listing.path.clone());
                                }
                                Loadable::Ready(listing)
                            }
                            Err(err) => Loadable::Error(err.to_string()),
                        },
                        Err(err) => Loadable::Error(err.to_string()),
                    };
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Create the space for the browser's current folder, on the TARGET's
    /// owning server — never just the active one, which may differ (Task
    /// 8b: the palette must reach any online machine).
    fn submit_add_space(&mut self, cx: &mut Context<Self>) {
        let Some(flow) = self.add_space.as_ref() else {
            return;
        };
        if flow.submit_busy {
            return;
        }
        let Some(target) = flow.device.clone() else {
            return;
        };
        let Some(listing) = flow.browser.ready() else {
            return;
        };
        let path = listing.path.clone();
        let git_detected = flow.browser_repo;
        let server_id = target.server_id.clone();
        let device = target.device.clone();
        let Some(engine) = self
            .state
            .read(cx)
            .client_for(&comet_proto::ServerRef::new(server_id.clone(), ""))
        else {
            return;
        };
        // Same (device, folder) already has a space on the TARGET server →
        // just switch to it. The engine dedupes this case too (a
        // createSpace for a duplicate pair no-ops), so creating would leave
        // the minted id dangling. Scanning `state.spaces` (the ACTIVE
        // server's projection) would miss a duplicate on a remote — scan
        // the owning server's own group instead
        // (`project_sidebar_servers` resolves every configured server's
        // real space list, active or not).
        let existing = {
            let state = self.state.read(cx);
            let groups = project_sidebar_servers(&state.servers, &state.server_order);
            groups
                .iter()
                .find(|g| g.server.id == server_id)
                .and_then(|g| {
                    g.spaces
                        .iter()
                        .find(|s| s.device_id == device.id && s.path == path)
                        .map(|s| s.id.clone())
                })
        };
        if let Some(existing) = existing {
            self.add_space = None;
            // Route through the switch-then-activate sequence
            // (`pick_panel_space`), not a bare `activate_space`: handing a
            // foreign-server id to `activate_space` while a different
            // server is active leaves `sidebar_scope` pointing at a row
            // that doesn't exist (Task 8's review, M1).
            self.pick_panel_space(server_id, existing, DropdownMode::Switch, cx);
            return;
        }
        let Some(flow) = self.add_space.as_mut() else {
            return;
        };
        flow.submit_busy = true;
        flow.error = None;
        let space_id = uuid::Uuid::new_v4().to_string();
        // Optimistic echo: the next watch frame for this server
        // (`FederationEvent::ServerChanged`) replaces its ENTIRE space list
        // wholesale (`apply_federation`, state.rs), so this row is
        // transient by construction — it lives only until that real
        // snapshot lands, same-id or not. Lands in the TARGET server's own
        // bucket — mutating `state.servers` directly (a public field) — so
        // a non-active remote's grouped-panel entry appears immediately
        // too; also mirrored into `state.spaces` when the target IS the
        // active server, matching the instant-appears behaviour the
        // single-server case already had (that projection is only
        // re-derived when the active server actually changes).
        let space = Space {
            id: space_id.clone(),
            device_id: device.id.clone(),
            path: path.clone(),
            name: None,
            git_detected,
            git_checked_at: None,
            checkout_id: None,
            created_at: Utc::now(),
        };
        self.state.update(cx, |s, cx| {
            let is_active = s.selected_server_id() == Some(&server_id);
            if let Some(server) = s.servers.get_mut(&server_id)
                && !server.spaces.iter().any(|existing| existing.id == space.id)
            {
                server.spaces.push(space.clone());
            }
            if is_active && !s.spaces.iter().any(|existing| existing.id == space.id) {
                s.spaces.push(space.clone());
            }
            cx.notify();
        });
        let params = serde_json::json!({
            "op": "createSpace",
            "spaceId": space_id,
            "deviceId": device.id,
            "path": path,
            "gitDetected": git_detected,
        });
        let submit_id = space_id.clone();
        let target_server = server_id.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = engine.client().call(methods::MUTATE, params).await;
            this.update(cx, |shell, cx| {
                match result {
                    Ok(_) => {
                        shell.add_space = None;
                        shell.pick_panel_space(
                            target_server.clone(),
                            submit_id.clone(),
                            DropdownMode::Switch,
                            cx,
                        );
                    }
                    Err(err) => {
                        // Roll the optimistic row back in both places;
                        // surface the error inline.
                        shell.state.update(cx, |s, cx| {
                            if let Some(server) = s.servers.get_mut(&target_server) {
                                server.spaces.retain(|space| space.id != submit_id);
                            }
                            s.spaces.retain(|space| space.id != submit_id);
                            cx.notify();
                        });
                        if let Some(flow) = shell.add_space.as_mut() {
                            flow.submit_busy = false;
                            flow.error = Some(format!("{err}").into());
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        });
        if let Some(flow) = self.add_space.as_mut() {
            flow.submit_task = Some(task);
        }
        cx.notify();
    }

    /// Go up to the parent folder (←, and ⌫ on an empty query).
    fn add_space_go_up(&mut self, cx: &mut Context<Self>) {
        let parent = self
            .add_space
            .as_ref()
            .and_then(|f| f.browser.ready())
            .and_then(|l| parent_path(&l.path));
        if let Some(parent) = parent {
            if let Some(flow) = self.add_space.as_mut() {
                flow.browser_repo = false; // unknown at the parent
            }
            self.load_space_folders(Some(parent), cx);
        }
    }

    /// Palette keys (bubbling from the focused search input) — every legend
    /// maps to a REAL key: ↑↓ navigate, →/⏎ open the highlighted folder,
    /// ← up a level, ⌘⏎ add the OPEN folder, ⌫ (empty query) also goes up,
    /// esc closes.
    fn add_space_key(&mut self, event: &gpui::KeyDownEvent, cx: &mut Context<Self>) {
        // ←/→ act on the FOLDERS, not the text cursor — the palette is a
        // navigator first; queries are short and edited with ⌫.
        match event.keystroke.key.as_str() {
            "right" => {
                self.add_space_open_active(cx);
                return;
            }
            "left" => {
                self.add_space_go_up(cx);
                return;
            }
            _ => {}
        }
        let key = popover::classify_key(
            event.keystroke.key.as_str(),
            event.keystroke.modifiers.platform,
            event.keystroke.modifiers.control,
        );
        match key {
            popover::MenuKey::Escape => {
                self.add_space = None;
                cx.notify();
            }
            popover::MenuKey::Up | popover::MenuKey::Down => {
                let count = self.add_space_filtered(cx).len();
                let delta = if key == popover::MenuKey::Up { -1 } else { 1 };
                if let Some(flow) = self.add_space.as_mut() {
                    flow.active = popover::menu_step(Some(flow.active), count, delta).unwrap_or(0);
                    // Keep the highlighted row in view as the cursor walks
                    // past the viewport (user-reported: the list didn't
                    // follow the keyboard).
                    flow.list_scroll.scroll_to_item(flow.active);
                    cx.notify();
                }
            }
            // ⏎ opens the highlighted folder (an alias for →); the space is
            // added with ⌘⏎ — and the chord acts on the folder OPEN in the
            // breadcrumbs, not the highlight. The highlight auto-rests on the
            // first row, so a chord that took it would add arbitrary
            // subfolders; the usual target (a repo root full of subfolders)
            // is only ever "the folder you're standing in".
            popover::MenuKey::Enter => self.add_space_open_active(cx),
            popover::MenuKey::ModEnter => self.submit_add_space(cx),
            popover::MenuKey::Backspace => {
                let empty = self
                    .add_space
                    .as_ref()
                    .is_some_and(|f| f.search.read(cx).is_empty());
                if empty {
                    self.add_space_go_up(cx);
                }
            }
            popover::MenuKey::Other => {}
        }
    }

    /// The palette card: ⌘K search bar (with the ⌘⏎ add / esc chips) ·
    /// breadcrumbs + folder list beside the devices rail · kbd-hint footer.
    pub(super) fn render_add_space_overlay(
        &mut self,
        viewport: gpui::Size<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let theme = Theme::of(cx).clone();
        {
            let flow = self.add_space.as_mut()?;
            if std::mem::take(&mut flow.focus_pending) {
                let handle = flow.search.focus_handle(cx);
                window.focus(&handle, cx);
            }
        }
        let (
            target,
            search,
            error,
            submit_busy,
            active,
            loading,
            load_error,
            listing,
            focus,
            list_scroll,
            home,
        ) = {
            let flow = self.add_space.as_ref()?;
            (
                flow.device.clone(),
                flow.search.clone(),
                flow.error.clone(),
                flow.submit_busy,
                flow.active,
                matches!(flow.browser, Loadable::Loading | Loadable::Idle),
                flow.browser.error().map(str::to_string),
                flow.browser.ready().cloned(),
                flow.focus.clone(),
                flow.list_scroll.clone(),
                flow.home.clone(),
            )
        };
        // The rail spans every device of every ONLINE server, not just the
        // active one (Task 8b) — `targets` replaces the old
        // `state.devices` (active-server-only) source.
        let (targets, multi_server, local_device_id): (Vec<BrowseTarget>, bool, Option<String>) = {
            let state = self.state.read(cx);
            let groups = project_sidebar_servers(&state.servers, &state.server_order);
            let online_servers = groups
                .iter()
                .filter(|g| g.server.connection == comet_proto::RemoteConnectionState::Online)
                .count();
            (
                browse_targets(&groups),
                online_servers > 1,
                state.local_device_id.clone(),
            )
        };
        let rows = self.add_space_filtered(cx);
        let query_empty = search.read(cx).is_empty();
        let hairline = crate::theme::hairline(0.06);
        let now = Utc::now();
        // (online) per rail row — presence is the same signal the sidebar
        // space rows use, resolved directly against each target's own
        // device record rather than `AppState::device_online` (which only
        // knows the ACTIVE server's devices — the same reasoning
        // `panel_rows`' per-group presence check uses, above).
        let device_presence: Vec<bool> = targets
            .iter()
            .map(|t| {
                local_device_id.as_deref() == Some(t.device.id.as_str())
                    || crate::settings::devices::device_online(t.device.last_seen_at, now)
            })
            .collect();
        let device_name: SharedString = target
            .as_ref()
            .map(|t| t.device.name.clone())
            .unwrap_or_else(|| "This device".to_string())
            .into();

        // A quiet mono key-cap chip ("⌘K" / "esc") for the search bar ends.
        let key_chip = |theme: &Theme| {
            div()
                .h(px(22.0))
                .px(px(6.0))
                .rounded(px(5.0))
                .flex_none()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(2.0))
                .bg(crate::theme::ink(0.05))
                .text_size(px(11.0))
                .font_family(theme.font_mono.clone())
                .text_color(theme.text_muted.opacity(0.7))
        };

        // ── search bar (the ⌘K bar): summon chip · input · "⌘ Enter" add ·
        //    esc. The primary chip leads with the ⌘ glyph, then says "Enter"
        //    in words (user request — the bare return arrow read as noise).
        let submit_chip = popover::btn_primary(&theme, "")
            .id("add-space-submit")
            .h(px(22.0))
            .px(px(8.0))
            .py(px(0.0))
            // Match the key-cap chips beside it (rounded-5) — btn_primary's
            // rounded-8 at this size read as a different component.
            .rounded(px(5.0))
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.0))
            .text_size(px(12.0))
            .when(submit_busy || listing.is_none(), |el| el.opacity(0.6))
            .on_click(cx.listener(|this, _, _, cx| this.submit_add_space(cx)))
            .when(!submit_busy, |el| {
                el.child(
                    icon(icons::COMMAND)
                        .size(px(11.0))
                        .text_color(theme.on_solid.opacity(0.8)),
                )
                .child(SharedString::from("Enter"))
            })
            .when(submit_busy, |el| el.child(SharedString::from("Adding…")));
        // Header and footer sit a shade DEEPER than the body (the shared
        // recessed-band tone) — the bands frame the folder list, which stays
        // on the brighter tint.
        let band = popover::band();
        let input_row = div()
            .h(px(46.0))
            .flex_none()
            .pl(px(12.0))
            .pr(px(10.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .bg(band)
            .border_b_1()
            .border_color(hairline)
            .child(
                key_chip(&theme)
                    .child(
                        icon(icons::COMMAND)
                            .size(px(11.0))
                            .text_color(theme.text_muted.opacity(0.7)),
                    )
                    .child(SharedString::from("K")),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(14.0))
                    .child(search.clone().into_any_element()),
            )
            .child(submit_chip)
            .child(
                key_chip(&theme)
                    .id("add-space-esc")
                    .cursor_pointer()
                    .hover(|s| s.bg(crate::theme::ink(0.09)))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.add_space = None;
                        cx.notify();
                    }))
                    .child(SharedString::from("esc")),
            );

        // ── breadcrumbs ("MacBook Pro / Projects / comet"): the quiet mono
        //    path voice, `/` separators. The device crumb stands in for home —
        //    everything up to the resolved home path folds into it; below
        //    home the full path shows. Ancestors (device crumb included) are
        //    clickable.
        let crumbs: AnyElement = match &listing {
            Some(listing) => {
                let segments = breadcrumbs(&listing.path);
                let last = segments.len().saturating_sub(1);
                // Root "/" chip always folds; the home segments fold too when
                // the browsed path sits at/under home.
                let at_home = home.as_deref() == Some(listing.path.as_str());
                let folded = 1 + home
                    .as_deref()
                    .filter(|h| listing.path == *h || listing.path.starts_with(&format!("{h}/")))
                    .map(|h| h.split('/').filter(|s| !s.is_empty()).count())
                    .unwrap_or(0);
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .px(px(13.0))
                    .pt(px(10.0))
                    .pb(px(2.0))
                    .text_size(px(11.0))
                    .font_family(theme.font_mono.clone())
                    .child({
                        let crumb = div()
                            .id("add-space-crumb-device")
                            .px(px(3.0))
                            .rounded(px(4.0))
                            .child(device_name.clone());
                        if at_home {
                            // Standing at home — the device crumb IS the
                            // current folder.
                            crumb
                                .text_color(theme.text.opacity(0.85))
                                .into_any_element()
                        } else {
                            crumb
                                .text_color(theme.text_muted.opacity(0.55))
                                .cursor_pointer()
                                .hover(|s| s.text_color(theme.text))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if let Some(flow) = this.add_space.as_mut() {
                                        flow.browser_repo = false;
                                    }
                                    this.load_space_folders(None, cx);
                                }))
                                .into_any_element()
                        }
                    })
                    .children(segments.into_iter().enumerate().skip(folded).map(
                        |(ix, (label, full))| {
                            let is_last = ix == last;
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .child(
                                    div()
                                        .text_color(theme.text_faint.opacity(0.7))
                                        .child(SharedString::from("/")),
                                )
                                .child({
                                    let crumb = div()
                                        .id(("add-space-crumb", ix))
                                        .px(px(3.0))
                                        .rounded(px(4.0))
                                        .text_color(if is_last {
                                            theme.text.opacity(0.85)
                                        } else {
                                            theme.text_muted.opacity(0.55)
                                        })
                                        .child(SharedString::from(label));
                                    if is_last {
                                        crumb.into_any_element()
                                    } else {
                                        crumb
                                            .cursor_pointer()
                                            .hover(|s| s.text_color(theme.text))
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                if let Some(flow) = this.add_space.as_mut() {
                                                    flow.browser_repo = false;
                                                }
                                                this.load_space_folders(Some(full.clone()), cx);
                                            }))
                                            .into_any_element()
                                    }
                                })
                        },
                    ))
                    .into_any_element()
            }
            None => div().pt(px(6.0)).into_any_element(),
        };

        // ── folder list ─────────────────────────────────────────────────────
        let base_path = listing.as_ref().map(|l| l.path.clone()).unwrap_or_default();
        let list: AnyElement = if loading {
            div()
                .px(px(8.0))
                .py(px(6.0))
                .child(popover::skeleton_rows(
                    "add-space-skeleton",
                    &theme,
                    6,
                    cx.entity_id(),
                    cx,
                ))
                .into_any_element()
        } else if let Some(message) = load_error {
            let device_line = target
                .as_ref()
                .map(|t| format!("{} didn't respond — is it online?", t.device.name))
                .unwrap_or(message);
            popover::error_row(&theme, &device_line)
                .px(px(14.0))
                .py(px(10.0))
                .child(
                    div()
                        .id("add-space-retry")
                        .px(px(Theme::SPACE_SM))
                        .py(px(3.0))
                        .rounded(px(Theme::CONTROL_RADIUS))
                        .border_1()
                        .border_color(theme.border)
                        .text_color(theme.text)
                        .cursor_pointer()
                        .hover(|s| s.bg(theme.element_hover))
                        .on_click(cx.listener(|this, _, _, cx| {
                            let path = this.add_space.as_ref().and_then(|f| f.browser_path.clone());
                            this.load_space_folders(path, cx);
                        }))
                        .child(SharedString::from("Retry")),
                )
                .into_any_element()
        } else if rows.is_empty() {
            div()
                .px(px(14.0))
                .py(px(16.0))
                .text_size(px(12.5))
                .text_color(theme.text_faint)
                .child(SharedString::from(if query_empty {
                    "No folders here"
                } else {
                    "No folders match"
                }))
                .into_any_element()
        } else {
            // The 6px gutters live on a WRAPPER, outside the scroll viewport:
            // in-content padding/spacers can't do it — the wheel's max offset
            // eats bottom padding, and `scroll_to_item` (keyboard) pins the
            // row's bottom to the viewport edge regardless.
            div()
                .flex_1()
                .min_h_0()
                .py(px(6.0))
                .child(
                    div()
                        .id("add-space-folders")
                        .size_full()
                        .overflow_y_scroll()
                        .track_scroll(&list_scroll)
                        .px(px(8.0))
                        .flex()
                        .flex_col()
                        // The app-wide list rhythm (sidebar rows, menu rows): 2px.
                        .gap(px(2.0))
                        .children(rows.into_iter().enumerate().map(|(ix, entry)| {
                            let name: SharedString = entry.name.clone().into();
                            let full = crate::pickers::child_path(&base_path, &entry.name);
                            let is_repo = entry.is_repo;
                            popover::menu_row_nav(
                                &theme,
                                false,
                                ix == active,
                                format!("add-space-folder-{ix}"),
                            )
                            // The floating-card selection language: the wash
                            // plus the ring-only inset outline.
                            .when(ix == active, |el| {
                                el.shadow(crate::theme::card_selected_shadows())
                            })
                            .id(("add-space-folder", ix))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.add_space_descend(full.clone(), is_repo, cx);
                            }))
                            .child(
                                icon(icons::FOLDER)
                                    .size(px(15.0))
                                    .flex_none()
                                    .text_color(theme.text_muted.opacity(0.8)),
                            )
                            .child(div().flex_1().min_w_0().truncate().child(name))
                            // Repos get a quiet trailing branch glyph — the row
                            // you're usually hunting for announces itself.
                            .when(is_repo, |el| {
                                el.child(
                                    icon(icons::GIT_BRANCH)
                                        .size(px(13.0))
                                        .flex_none()
                                        .text_color(theme.text_muted.opacity(0.5)),
                                )
                            })
                        })),
                )
                .into_any_element()
        };

        // ── devices rail (mock right column): platform glyph + name +
        //    presence dot per row, an info line naming the browsed device.
        //    Rows are the tab recipe (h-28 rounded-8 washes), vertical. When
        //    more than one server is online, a non-interactive server header
        //    (the same tone `render_panel_server_header` uses for the scope
        //    dropdown) precedes that server's devices — a bare device name
        //    is ambiguous once machines from different servers can collide.
        //    A single online server renders flat, same as today.
        let mut last_server: Option<comet_proto::ServerId> = None;
        let device_rows: Vec<AnyElement> = targets
            .iter()
            .enumerate()
            .flat_map(|(ix, t)| {
                let mut group: Vec<AnyElement> = Vec::new();
                if multi_server && last_server.as_ref() != Some(&t.server_id) {
                    group.push(
                        render_panel_server_header(
                            &t.server_name,
                            &comet_proto::RemoteConnectionState::Online,
                            &theme,
                        )
                        .into_any_element(),
                    );
                }
                last_server = Some(t.server_id.clone());
                let is_active = target
                    .as_ref()
                    .is_some_and(|d| d.server_id == t.server_id && d.device.id == t.device.id);
                let online = device_presence.get(ix).copied().unwrap_or(false);
                // The Devices-page platform mapping (settings::devices).
                let platform_icon = match t.device.platform.as_str() {
                    "macos" | "darwin" => icons::LAPTOP,
                    "web" => icons::GLOBAL,
                    "ios" | "android" => icons::SMARTPHONE,
                    _ => icons::MONITOR,
                };
                let name: SharedString = t.device.name.clone().into();
                let pick = t.clone();
                group.push(
                    div()
                        .id(("add-space-device", ix))
                        .h(px(28.0))
                        .px(px(8.0))
                        .rounded(px(8.0))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .text_size(px(12.5))
                        .cursor_pointer()
                        .when(is_active, |el| {
                            // The floating-card selection language: wash +
                            // ring-only inset outline.
                            el.bg(crate::theme::card_selected_bg())
                                .shadow(crate::theme::card_selected_shadows())
                                .text_color(theme.text)
                        })
                        .when(!is_active, |el| {
                            el.text_color(theme.text_muted.opacity(0.7))
                                .hover(|s| s.bg(theme.element_hover))
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.add_space_pick_target(pick.clone(), cx);
                        }))
                        .child(
                            icon(platform_icon)
                                .size(px(14.0))
                                .flex_none()
                                .text_color(theme.text_muted.opacity(0.8)),
                        )
                        .child(div().flex_1().min_w_0().truncate().child(name))
                        .child(
                            div()
                                .size(px(5.0))
                                .rounded_full()
                                .flex_none()
                                .when(online, |el| {
                                    // The Devices-page presence emerald, soft glow
                                    // included.
                                    let emerald = theme.success;
                                    el.bg(emerald.opacity(0.9)).shadow(vec![gpui::BoxShadow {
                                        color: emerald.opacity(0.55),
                                        offset: gpui::point(px(0.0), px(0.0)),
                                        blur_radius: px(6.0),
                                        spread_radius: px(0.0),
                                        inset: false,
                                    }])
                                })
                                .when(!online, |el| el.bg(crate::theme::ink(0.22))),
                        )
                        .into_any_element(),
                );
                group
            })
            .collect();
        let rail = div()
            .w(px(196.0))
            .flex_none()
            .border_l_1()
            .border_color(hairline)
            .px(px(8.0))
            .py(px(8.0))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .child(
                div()
                    .px(px(8.0))
                    .pt(px(2.0))
                    .pb(px(4.0))
                    .text_size(px(11.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text_muted.opacity(0.6))
                    .child(SharedString::from("Devices")),
            )
            .children(device_rows)
            .child(div().h(px(1.0)).mx(px(2.0)).my(px(6.0)).bg(hairline))
            .child(
                div()
                    .px(px(8.0))
                    .flex()
                    .flex_row()
                    .items_start()
                    .gap(px(6.0))
                    .text_size(px(11.0))
                    .line_height(px(15.0))
                    .text_color(theme.text_muted.opacity(0.5))
                    .child(
                        icon(icons::INFO_CIRCLE)
                            .size(px(12.0))
                            .flex_none()
                            .mt(px(1.0))
                            .text_color(theme.text_muted.opacity(0.5)),
                    )
                    .child(div().min_w_0().child(SharedString::from(format!(
                        "Showing folders from {device_name} only"
                    )))),
            );

        // ── body: folder column (crumbs + list) beside the devices rail.
        //    FIXED height — sparse folders, loading skeletons, and device
        //    switches must not resize the card (the list fills and scrolls).
        let body = div()
            .h(px(330.0))
            .flex()
            .flex_row()
            .items_stretch()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(crumbs)
                    .child(list),
            )
            .child(rail);

        // ── footer: the shared key-cap legend voice (popover::key_hint).
        let footer = div()
            .flex_none()
            .bg(band)
            .border_t_1()
            .border_color(hairline)
            .px(px(12.0))
            .py(px(8.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(12.0))
            .child(popover::key_hint_pair(
                &theme,
                icons::ARROW_UP,
                icons::ARROW_DOWN,
                "Navigate",
            ))
            .child(popover::key_hint(&theme, icons::ARROW_LEFT, "Up"))
            .child(popover::key_hint(&theme, icons::ARROW_RIGHT, "Open"))
            .when_some(error, |el, message| {
                el.child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(px(11.0))
                        .text_color(theme.danger)
                        .child(message),
                )
            });

        let card =
            div()
                .id("add-space-palette")
                .w(px(680.0))
                .rounded(px(14.0))
                .border_1()
                .border_color(crate::theme::hairline(0.10))
                // The popover_card glass recipe: a translucent tint over the
                // frosted backdrop blur (`popover::modal` wraps in `frosted`) —
                // an opaque fill here killed the vibrancy every other float has.
                .bg(if theme.is_glass() {
                    theme.glass_overlay()
                } else {
                    theme.surface_overlay
                })
                .shadow_lg()
                .overflow_hidden()
                .flex()
                .flex_col()
                .text_color(theme.text)
                // On the keyboard dispatch path (see `AddSpaceFlow::focus`) — the
                // pickers' proven structure for frame-level keys with a focused
                // child input.
                .track_focus(&focus)
                .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                    this.add_space_key(event, cx)
                }))
                // Clicking the scrim dismisses (user requirement) — same close
                // path as Escape.
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.add_space = None;
                    cx.notify();
                }))
                .child(input_row)
                .child(body)
                .child(footer)
                .into_any_element();
        Some(popover::modal("add-space-dialog", viewport, card))
    }

    // ---- space context menu / rename / delete overlays ----

    pub(super) fn open_rename_space(&mut self, space_id: String, cx: &mut Context<Self>) {
        self.space_menu = None;
        let current = self
            .state
            .read(cx)
            .space_row(&space_id)
            .map(|s| s.display_name().to_string())
            .unwrap_or_default();
        let input = cx.new(|cx| ComposerInput::new("Space name", cx));
        input.update(cx, |input, cx| input.set_text(current, cx));
        let events = cx.subscribe(&input, |this: &mut Shell, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.submit_rename_space(cx);
            }
        });
        self.rename_space_dialog = Some(RenameSpaceDialog {
            space_id,
            input,
            focus_pending: true,
            _events: events,
        });
        cx.notify();
    }

    pub(super) fn submit_rename_space(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.rename_space_dialog.take() else {
            return;
        };
        let name = dialog.input.read(cx).text().trim().to_string();
        if !name.is_empty() {
            self.mutate(
                serde_json::json!({ "op": "renameSpace", "spaceId": dialog.space_id, "name": name }),
                cx,
            );
        }
        cx.notify();
    }

    pub(super) fn delete_space(&mut self, space_id: String, cx: &mut Context<Self>) {
        self.delete_space_confirm = None;
        self.mutate(
            serde_json::json!({ "op": "deleteSpace", "spaceId": space_id }),
            cx,
        );
        cx.notify();
    }

    /// Space context menu + rename dialog + delete confirm (appended to the
    /// shell's overlay list).
    pub(super) fn render_space_overlays(
        &mut self,
        viewport: gpui::Size<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let theme = Theme::of(cx).clone();
        let mut overlays: Vec<AnyElement> = Vec::new();

        if let Some((space_id, position)) = self.space_menu.clone() {
            let rename_id = space_id.clone();
            let delete_id = space_id.clone();
            let menu = popover::popover_card(&theme)
                .w(px(170.0))
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.space_menu = None;
                    cx.notify();
                }))
                .flex()
                .flex_col()
                .child(
                    popover::menu_row(&theme, false, format!("space-menu-rename-{space_id}"))
                        .id("space-menu-rename")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.open_rename_space(rename_id.clone(), cx)
                        }))
                        .child(icon(icons::PEN).size(px(16.0)).text_color(theme.text_muted))
                        .child(SharedString::from("Rename…")),
                )
                .child(popover::menu_separator())
                .child(
                    popover::menu_row(&theme, false, format!("space-menu-delete-{space_id}"))
                        .id("space-menu-delete")
                        .text_color(theme.danger)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.space_menu = None;
                            this.delete_space_confirm = Some(delete_id.clone());
                            cx.notify();
                        }))
                        .child(
                            icon(icons::TRASH_BIN_MINIMALISTIC)
                                .size(px(16.0))
                                .text_color(theme.danger),
                        )
                        .child(SharedString::from("Remove…")),
                )
                .into_any_element();
            overlays.push(popover::menu_at("space-context-menu", position, menu));
        }

        if let Some(dialog) = &mut self.rename_space_dialog {
            if std::mem::take(&mut dialog.focus_pending) {
                window.focus(&dialog.input.focus_handle(cx), cx);
            }
            let input = dialog.input.clone();
            let card = popover::dialog_card(&theme)
                .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, _, cx| {
                    if ev.keystroke.key == "escape" {
                        this.rename_space_dialog = None;
                        cx.notify();
                    }
                }))
                .child(popover::dialog_title(&theme, "Rename space"))
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
                            popover::btn_ghost(&theme, "Cancel", "rename-space-cancel")
                                .id("rename-space-cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.rename_space_dialog = None;
                                    cx.notify();
                                })),
                        )
                        .child(
                            popover::btn_primary(&theme, "Rename")
                                .id("rename-space-save")
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.submit_rename_space(cx)),
                                ),
                        ),
                )
                .into_any_element();
            overlays.push(popover::modal("rename-space-dialog", viewport, card));
        }

        if let Some(space_id) = self.delete_space_confirm.clone() {
            let (name, device, count) = {
                let state = self.state.read(cx);
                let space = state.space_row(&space_id);
                (
                    space
                        .map(|s| s.display_name().to_string())
                        .unwrap_or_else(|| "this space".into()),
                    space
                        .and_then(|s| state.device_name(&s.device_id))
                        .unwrap_or("its device")
                        .to_string(),
                    state.chats_in_space(&space_id).len(),
                )
            };
            let copy = if count == 1 {
                format!(
                    "Removing “{name}” permanently deletes its 1 session on {device}. This can’t be undone."
                )
            } else {
                format!(
                    "Removing “{name}” permanently deletes its {count} sessions on {device}. This can’t be undone."
                )
            };
            let card = popover::dialog_card(&theme)
                .child(popover::dialog_title(&theme, "Remove space?"))
                .child(div().mt(px(6.0)).child(popover::dialog_body(&theme, copy)))
                .child(
                    div()
                        .mt(px(16.0))
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            popover::btn_ghost(&theme, "Cancel", "delete-space-cancel")
                                .id("delete-space-cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.delete_space_confirm = None;
                                    cx.notify();
                                })),
                        )
                        .child(
                            popover::btn_danger(&theme, "Remove")
                                .id("delete-space-confirm")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.delete_space(space_id.clone(), cx)
                                })),
                        ),
                )
                .into_any_element();
            overlays.push(popover::modal("delete-space-dialog", viewport, card));
        }

        overlays
    }
}
