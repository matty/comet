//! Sidebar search: one field over both spaces and sessions.
//!
//! Pure matching lives here as a free function with unit tests (the `rail.rs`
//! convention); rendering is an `impl Shell` extension below it.
//!
//! Search deliberately ignores [`SidebarScope`] — it always reads the whole
//! projected set. A scoped list is a convenience, never a wall.
//!
//! Session result rows go through the SAME [`super::Shell::render_chat_row`]
//! the normal Sessions list uses, built with `RowScope::All` (space+device
//! line always on top, regardless of the sidebar's current scope — a hit
//! elsewhere is useless if it can't say which one). An earlier version of
//! this file rebuilt that row from scratch to get per-span tinting, and
//! within a day of landing had four properties (the working spinner, the
//! hover text-brighten, the selected shadow, the right-click context menu)
//! silently missing relative to the real row — a review caught it. Tinting
//! is instead threaded INTO `render_chat_row` via its `highlight_query`
//! parameter, so there is exactly one place that draws a session row and it
//! cannot drift out of sync with itself. [`styled_line`] is the tinting
//! primitive both that row and the space row (below, which has no shared
//! production row to reuse — the normal list shows spaces through the scope
//! trigger/dropdown, not a flat row list) build their tinted spans with.

use comet_proto::{Chat, Space};

use super::*;

/// Matching spaces and chats, by id, in the input's order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct SearchResults {
    pub spaces: Vec<String>,
    pub chats: Vec<String>,
}

impl SearchResults {
    pub fn is_empty(&self) -> bool {
        self.spaces.is_empty() && self.chats.is_empty()
    }
}

/// `None` = not searching (blank query). `Some(empty)` = searching, nothing
/// matched — a different state, and a different thing to draw.
///
/// Matches, case-insensitively, on: the space's display name, its path, the
/// session title, the branch, and the owning device's name. The path is
/// included because `display_name()` falls back to the folder basename, so a
/// space named "api" at `~/work/acme/api` must still be findable by "acme".
pub(super) fn filter(
    query: &str,
    spaces: &[Space],
    chats: &[Chat],
    device_name: &dyn Fn(&str) -> Option<String>,
) -> Option<SearchResults> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return None;
    }
    let hit = |hay: &str| hay.to_lowercase().contains(&needle);

    let matching_spaces: Vec<String> = spaces
        .iter()
        .filter(|s| hit(s.display_name()) || hit(&s.path))
        .map(|s| s.id.clone())
        .collect();

    let matching_chats: Vec<String> = chats
        .iter()
        .filter(|c| !c.archived)
        // A chat whose `space_id` doesn't resolve to a live space is invisible
        // to the normal sidebar too (`AppState::overview_chats`'s own
        // `space_row(id).is_some()` guard, state.rs) — search must agree, or
        // it can hand back a hit the sidebar has nowhere to render (space name
        // "?", and selecting it lands the app in a state the normal list
        // can't represent).
        .filter(|c| {
            c.space_id
                .as_deref()
                .is_some_and(|id| spaces.iter().any(|s| s.id == id))
        })
        .filter(|c| {
            let title_hit = c.title.as_deref().is_some_and(hit);
            let branch_hit = c.branch.as_deref().is_some_and(hit);
            let device_hit = device_name(&c.device_id).as_deref().is_some_and(hit);
            let space_hit = c.space_id.as_deref().is_some_and(|id| {
                spaces
                    .iter()
                    .find(|s| s.id == id)
                    .is_some_and(|s| hit(s.display_name()) || hit(&s.path))
            });
            title_hit || branch_hit || device_hit || space_hit
        })
        .map(|c| c.id.clone())
        .collect();

    Some(SearchResults {
        spaces: matching_spaces,
        chats: matching_chats,
    })
}

/// Split `text` into `(before, matched, after)` for the first
/// case-insensitive occurrence of `needle`. `None` when it does not occur.
///
/// Slices on the byte offset the lowercase search found, which is only sound
/// because the offsets agree for the ASCII the match is anchored on; guard the
/// boundaries so a multi-byte title can never panic.
pub(super) fn match_run(text: &str, needle: &str) -> Option<(String, String, String)> {
    if needle.is_empty() {
        return None;
    }
    let start = text.to_lowercase().find(&needle.to_lowercase())?;
    let end = start + needle.len();
    if !text.is_char_boundary(start) || !text.is_char_boundary(end) {
        return None;
    }
    Some((
        text[..start].to_string(),
        text[start..end].to_string(),
        text[end..].to_string(),
    ))
}

/// Render `text` as a single line, tinting the first case-insensitive hit of
/// `query` (if any) in `accent` — the rest stays `base`.
///
/// This renders through ONE [`gpui::StyledText`] carrying multiple
/// [`gpui::TextRun`]s, not a `flex_row` of sibling text elements. That
/// distinction matters: `gpui`'s `.truncate()` (`overflow_hidden` +
/// `whitespace_nowrap` + `text_ellipsis`) operates on a single text node's
/// layout as a whole. Three sibling divs are each their own layout box, so a
/// long line built that way ellipsized EACH span independently (`Make the
/// … fade dis…` instead of one clean cut) — round-1 review caught this
/// before it shipped. A single styled-run text node truncates exactly like
/// the plain, untinted string it replaces.
pub(super) fn styled_line(
    text: &str,
    query: Option<&str>,
    base: gpui::Hsla,
    accent: gpui::Hsla,
    font: gpui::Font,
) -> AnyElement {
    let hit = query.and_then(|q| match_run(text, q));
    let run = |len: usize, color: gpui::Hsla, font: gpui::Font| gpui::TextRun {
        len,
        font,
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let runs = match hit {
        Some((before, matched, after)) => {
            let mut runs = Vec::with_capacity(3);
            if !before.is_empty() {
                runs.push(run(before.len(), base, font.clone()));
            }
            runs.push(run(matched.len(), accent, font.clone()));
            if !after.is_empty() {
                runs.push(run(after.len(), base, font));
            }
            runs
        }
        None => vec![run(text.len(), base, font)],
    };
    gpui::StyledText::new(text.to_string())
        .with_runs(runs)
        .into_any_element()
}

/// One openable result. `Chat` selects a session in place (never re-scopes
/// the sidebar); `Space` switches the sidebar's scope via `activate_space`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SearchHit {
    Chat(String),
    Space(String),
}

/// Which result group a flat keyboard-highlight index falls into, and its
/// index within that group. Spaces render first, sessions second — the same
/// order [`filter`] returns and [`Shell::render_search_results`] draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HighlightTarget {
    Space(usize),
    Chat(usize),
}

/// Maps a flat highlight index into the group it selects, given the two
/// group sizes. Pulled out and unit-tested on its own: this exact kind of
/// off-by-one has bitten this sidebar twice before (index arithmetic in the
/// drag-drop math, and again adding scroll to a list container), so the flat
/// index → (group, offset) mapping gets the same treatment rather than being
/// inlined into the keydown handler and the render loop separately, where the
/// two copies could quietly drift apart.
pub(super) fn highlight_target(
    active: usize,
    spaces_len: usize,
    chats_len: usize,
) -> Option<HighlightTarget> {
    if active < spaces_len {
        Some(HighlightTarget::Space(active))
    } else if active < spaces_len + chats_len {
        Some(HighlightTarget::Chat(active - spaces_len))
    } else {
        None
    }
}

/// The inset accent ring for the keyboard-highlighted result row (spec §4):
/// `box_shadow` with `inset: true` rather than [`crate::theme::glass_selected_shadows`]'s
/// drop-seat treatment — a result row's highlight is a cursor, not a selection.
pub(super) fn highlight_ring(theme: &Theme) -> Vec<gpui::BoxShadow> {
    vec![gpui::BoxShadow {
        color: theme.accent.opacity(0.45),
        offset: gpui::point(px(0.0), px(0.0)),
        blur_radius: px(0.0),
        spread_radius: px(1.0),
        inset: true,
    }]
}

/// Trailing count on a results-mode section header — [`spaces::section_header`]
/// with no `+` (results never offer one) plus this appended as its second
/// (and last) child, taking the slot `justify_between` normally gives the
/// `+` button.
fn search_count_chip(n: usize, theme: &Theme) -> AnyElement {
    div()
        .flex_none()
        .text_size(px(11.0))
        .text_color(theme.text_muted.opacity(0.4))
        .child(SharedString::from(n.to_string()))
        .into_any_element()
}

/// One result group's rows, in the SAME gapped column the normal Sessions
/// list wraps its rows in (`SIDEBAR_LIST_GAP`). Results used to be flat
/// children of the gapless scroll container, so identical rows sat flush
/// against each other here and 2px apart three keystrokes earlier.
fn result_rows(rows: Vec<AnyElement>) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(px(super::SIDEBAR_LIST_GAP))
        .children(rows)
        .into_any_element()
}

/// The `↑↓ move · ↵ open · esc clear` legend under the results list.
fn render_search_footer(theme: &Theme) -> AnyElement {
    div()
        .flex_none()
        .px(px(Theme::SPACE_SM))
        .pt(px(4.0))
        .pb(px(Theme::SPACE_SM))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(10.0))
        .text_size(px(10.5))
        .text_color(theme.text_muted.opacity(0.45))
        .child(SharedString::from("↑↓ move"))
        .child(SharedString::from("↵ open"))
        .child(SharedString::from("esc clear"))
        .into_any_element()
}

impl Shell {
    /// The query as currently typed, untrimmed (callers trim before matching,
    /// same as [`filter`]).
    pub(super) fn search_query<'a>(&self, cx: &'a App) -> &'a str {
        self.search_input.read(cx).text()
    }

    /// Clear the query and the keyboard highlight. Deliberately never touches
    /// `sidebar_scope` — the caller decides that separately, see
    /// [`Self::open_search_result`].
    pub(super) fn clear_search(&mut self, cx: &mut Context<Self>) {
        self.search_input
            .update(cx, |input, cx| input.set_text("", cx));
        self.search_active = 0;
        cx.notify();
    }

    /// Search is a lens, not a mode: opening a session never re-scopes the
    /// column. Only picking a *space* result changes the scope.
    ///
    /// Focus follows the result out of the field: without this it stayed in
    /// the (now cleared) search input, so the first keystroke aimed at the
    /// session just opened went to search instead. Every other way of
    /// selecting a session leaves you in the composer; this one now does too.
    /// The move itself is deferred one frame — see `composer_focus_pending`,
    /// this path has no `Window`.
    pub(super) fn open_search_result(&mut self, result: SearchHit, cx: &mut Context<Self>) {
        match result {
            SearchHit::Chat(id) => {
                self.state.update(cx, |s, cx| s.select_chat(Some(id), cx));
            }
            SearchHit::Space(id) => self.activate_space(id, cx),
        }
        self.clear_search(cx);
        self.composer_focus_pending = true;
    }

    /// Bubbled ↑↓/⏎/esc from the focused search input — the `"SidebarSearch"`
    /// context leaves them unbound (see [`crate::composer::init`]) so they
    /// reach here instead of moving the caret, the same shape as
    /// `AddSpaceFlow::add_space_key`.
    ///
    /// Escape is checked against the RAW (untrimmed) text, before anything
    /// else: `filter` trims, so a whitespace-only query is `None` results —
    /// gating Escape on "has results" left a dead end where a lone space
    /// showed the clear button but the only way to actually clear it was
    /// Backspace (round-1 review). ↑↓/Enter still need real results and stay
    /// gated on the trimmed query.
    pub(super) fn search_key(&mut self, event: &gpui::KeyDownEvent, cx: &mut Context<Self>) {
        let raw = self.search_query(cx).to_string();
        if raw.is_empty() {
            return;
        }
        let key = popover::classify_key(
            event.keystroke.key.as_str(),
            event.keystroke.modifiers.platform,
            event.keystroke.modifiers.control,
        );
        if key == popover::MenuKey::Escape {
            self.clear_search(cx);
            return;
        }
        let query = raw.trim().to_string();
        if query.is_empty() {
            return;
        }
        let results = {
            let state = self.state.read(cx);
            filter(&query, &state.spaces, &state.chats, &|id| {
                state.device_name(id).map(str::to_string)
            })
        };
        let Some(results) = results else { return };
        let total = results.spaces.len() + results.chats.len();
        match key {
            popover::MenuKey::Up | popover::MenuKey::Down => {
                let delta = if key == popover::MenuKey::Up { -1 } else { 1 };
                self.search_active =
                    popover::menu_step(Some(self.search_active), total, delta).unwrap_or(0);
                cx.notify();
            }
            popover::MenuKey::Enter => {
                if let Some(target) = highlight_target(
                    self.search_active,
                    results.spaces.len(),
                    results.chats.len(),
                ) {
                    let hit = match target {
                        HighlightTarget::Space(i) => SearchHit::Space(results.spaces[i].clone()),
                        HighlightTarget::Chat(i) => SearchHit::Chat(results.chats[i].clone()),
                    };
                    self.open_search_result(hit, cx);
                }
            }
            _ => {}
        }
    }

    /// The pinned search field: rendered as the FIRST child of the sidebar
    /// column, outside the scroll region, so it can never scroll away. As a
    /// consequence `SIDEBAR_GLASS_FADE_BAND`'s top fade now starts below this
    /// block rather than at the column's top edge.
    ///
    /// `has_text` drives this field's own chrome and is deliberately the RAW
    /// (untrimmed) query being non-empty, not "has results": a whitespace-only
    /// query has no results (`filter` trims), but the field still needs to
    /// show the clear button rather than the ⌘P hint, and `search_key` clears
    /// it on Escape the same as any other non-empty text (round-1 review — the
    /// two were conflated and a lone space was a dead end).
    pub(super) fn render_search_field(
        &mut self,
        has_text: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .id("sidebar-search")
            .flex_none()
            .mx(px(Theme::SPACE_SM))
            .mt(px(Theme::SPACE_SM))
            .mb(px(6.0))
            .h(px(28.0))
            .px(px(Theme::SPACE_SM))
            .rounded(px(Theme::CONTROL_RADIUS))
            .border_1()
            .border_color(if has_text {
                theme.accent.opacity(0.55)
            } else {
                theme.border
            })
            .bg(if has_text {
                crate::theme::wash(0.055)
            } else {
                crate::theme::wash(0.03)
            })
            .flex()
            .flex_row()
            .items_center()
            .gap(px(7.0))
            .child(
                icon(icons::MAGNIFER)
                    .size(px(14.0))
                    .flex_none()
                    .text_color(if has_text {
                        theme.accent
                    } else {
                        theme.text_faint
                    }),
            )
            .child(div().flex_1().min_w_0().child(self.search_input.clone()))
            .when(!has_text, |el| {
                el.child(
                    div()
                        .flex_none()
                        .text_size(px(9.5))
                        .text_color(theme.text_muted.opacity(0.5))
                        .child(SharedString::from(crate::settings::display_combo(
                            &self.settings.keymap.focus_search,
                        ))),
                )
            })
            .when(has_text, |el| {
                el.child(
                    div()
                        .id("search-clear")
                        .flex_none()
                        .size(px(16.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(4.0))
                        .cursor_pointer()
                        .hover(|s| s.bg(crate::theme::wash(0.12)))
                        .on_click(cx.listener(|this, _, _, cx| this.clear_search(cx)))
                        .child(
                            icon(icons::CLOSE)
                                .size(px(11.0))
                                .text_color(theme.text_muted),
                        ),
                )
            })
            .into_any_element()
    }

    /// The results-mode content of the sidebar's scroll region: `Spaces` and
    /// `Sessions` headers (counted, no `+`) each followed by their matching
    /// rows, or the empty-state line when neither group has anything, plus
    /// the keyboard-hint footer. Replaces `render_spaces_section` + the
    /// normal Sessions list wholesale.
    ///
    /// A group with zero matches skips its header entirely (round-1 review:
    /// a query matching only sessions used to still show a dangling
    /// "Spaces 0"). Session rows render through the SAME [`Self::render_chat_row`]
    /// the normal list uses (see the module doc) rather than a parallel
    /// builder, so the working spinner, hover text-brighten, selected shadow,
    /// and right-click context menu can't drift out of sync with it again.
    pub(super) fn render_search_results(
        &mut self,
        results: &SearchResults,
        query: &str,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        if results.is_empty() {
            return vec![
                div()
                    .px(px(Theme::SPACE_SM))
                    .pt(px(6.0))
                    .pb(px(Theme::SPACE_SM))
                    .text_size(px(12.0))
                    .text_color(theme.text_faint)
                    .child(SharedString::from(format!(
                        "No spaces or sessions match \"{query}\"."
                    )))
                    .into_any_element(),
                render_search_footer(theme),
            ];
        }

        let mut children: Vec<AnyElement> = Vec::new();

        if !results.spaces.is_empty() {
            children.push(
                spaces::section_header("Spaces", true, theme, None)
                    .child(search_count_chip(results.spaces.len(), theme))
                    .into_any_element(),
            );
            let mut rows: Vec<AnyElement> = Vec::new();
            for (i, id) in results.spaces.iter().enumerate() {
                let space = self.state.read(cx).space_row(id).cloned();
                let Some(space) = space else { continue };
                let highlighted = highlight_target(
                    self.search_active,
                    results.spaces.len(),
                    results.chats.len(),
                ) == Some(HighlightTarget::Space(i));
                rows.push(self.render_search_space_row(&space, query, highlighted, theme, cx));
            }
            children.push(result_rows(rows));
        }

        if !results.chats.is_empty() {
            // "First" (tighter top padding) whenever Spaces didn't render —
            // Sessions is then visually the top of the list, same as the
            // normal (non-search) list tucks its first section up.
            let sessions_first = results.spaces.is_empty();
            children.push(
                spaces::section_header("Sessions", sessions_first, theme, None)
                    .child(search_count_chip(results.chats.len(), theme))
                    .into_any_element(),
            );
            let mut rows: Vec<AnyElement> = Vec::new();
            for (i, id) in results.chats.iter().enumerate() {
                let chat = self
                    .state
                    .read(cx)
                    .chats
                    .iter()
                    .find(|c| &c.id == id)
                    .cloned();
                let Some(chat) = chat else { continue };
                let highlighted = highlight_target(
                    self.search_active,
                    results.spaces.len(),
                    results.chats.len(),
                ) == Some(HighlightTarget::Chat(i));
                rows.push(self.render_search_chat_row(&chat, query, highlighted, theme, cx));
            }
            children.push(result_rows(rows));
        }

        children.push(render_search_footer(theme));
        children
    }

    /// One space result row: folder icon, tinted display name. Always opens
    /// via [`Self::open_search_result`], which is the only path that switches
    /// `sidebar_scope`. No shared production row to reuse here (unlike
    /// sessions) — the normal sidebar shows spaces through the scope
    /// trigger/dropdown, not a flat row list — so this stays a small,
    /// self-contained builder.
    fn render_search_space_row(
        &mut self,
        space: &Space,
        query: &str,
        highlighted: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = space.id.clone();
        let click_id = id.clone();
        let name = space.display_name().to_string();
        let fade_key = format!("search-space-{id}");
        let mut font = gpui::font(theme.font_sans.clone());
        font.weight = gpui::FontWeight::MEDIUM;
        let title = styled_line(
            &name,
            Some(query),
            theme.text.opacity(0.85),
            theme.accent,
            font,
        );

        div()
            .id(SharedString::from(format!("search-space-{id}")))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(Theme::SPACE_SM))
            .rounded(px(8.0))
            .px(px(Theme::SPACE_SM))
            .py(px(6.0))
            .bg(motion::hover_blend(
                &fade_key,
                crate::theme::wash(0.0),
                theme.glass_hover(),
            ))
            .when(highlighted, |el| {
                el.bg(crate::theme::glass_selected_bg())
                    .shadow(highlight_ring(theme))
            })
            .on_hover(motion::hover_listener(fade_key))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                this.open_search_result(SearchHit::Space(click_id.clone()), cx);
            }))
            .child(
                icon(icons::FOLDER)
                    .size(px(14.0))
                    .flex_none()
                    .text_color(theme.text_muted),
            )
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_size(px(13.0))
                    .line_height(px(18.0))
                    .child(title),
            )
            .into_any_element()
    }

    /// One session result row. Builds the exact same arguments
    /// `render_active_rows` builds for the normal list — `RowScope::All`
    /// (always, regardless of the sidebar's current scope: a hit elsewhere is
    /// useless if it can't say which one), the same status/time/branch/harness
    /// projection — and hands them to the shared [`Self::render_chat_row`]
    /// with `highlight_query` set and a click handler that opens through
    /// [`Self::open_search_result`] (clears the query) instead of selecting
    /// directly.
    fn render_search_chat_row(
        &mut self,
        chat: &Chat,
        query: &str,
        highlighted: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let now = Utc::now();
        let (status, scope, selected) = {
            let state = self.state.read(cx);
            let status = state.display_status_for(chat, now);
            let space_name = state
                .space_for_chat(chat)
                .map(|s| s.display_name().to_string())
                .unwrap_or_else(|| "?".to_string());
            let device = state
                .device_name(&chat.device_id)
                .unwrap_or("Unknown device")
                .to_string();
            let host_offline = !state.device_online(&chat.device_id, now);
            let selected = state.selected_chat_id() == Some(chat.id.as_str());
            let scope = super::RowScope::All {
                space: space_name.into(),
                device: device.into(),
                host_offline,
            };
            (status, scope, selected)
        };
        let time_ago: SharedString =
            format_time_ago(chat.last_message_at.unwrap_or(chat.created_at), now).into();
        let title: SharedString =
            transcript::single_line(&chat.title.clone().unwrap_or_else(|| "New session".into()))
                .into();
        let branch = chat
            .branch
            .as_deref()
            .map(str::trim)
            .filter(|b| !b.is_empty())
            .map(SharedString::from);
        let harness = chat.config.as_ref().map(|c| c.harness);
        let click_id = chat.id.clone();

        self.render_chat_row(
            chat.id.clone(),
            title,
            time_ago,
            scope,
            branch,
            harness,
            status,
            selected,
            Some(query),
            highlighted,
            // Search results are their own list — the jump slots number the
            // sidebar's active rows, not these.
            None,
            move |this: &mut Shell, cx: &mut Context<Shell>| {
                this.open_search_result(SearchHit::Chat(click_id.clone()), cx);
            },
            theme,
            cx,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn spaces() -> Vec<Space> {
        vec![
            Space {
                id: "comet-id".into(),
                device_id: "d2".into(),
                path: "/home/m/comet".into(),
                name: None,
                git_detected: true,
                git_checked_at: None,
                checkout_id: None,
                created_at: Utc::now(),
            },
            Space {
                id: "fade-lab-id".into(),
                device_id: "d1".into(),
                path: "/home/m/work/acme/fade-lab".into(),
                name: None,
                git_detected: true,
                git_checked_at: None,
                checkout_id: None,
                created_at: Utc::now(),
            },
        ]
    }

    fn chats() -> Vec<Chat> {
        vec![
            Chat {
                id: "c-fade".into(),
                device_id: "d2".into(),
                title: Some("Fade exploration".into()),
                archived: false,
                cwd: Some("/home/m/work/acme/fade-lab".into()),
                branch: Some("main".into()),
                checkout_id: None,
                config: None,
                last_message_preview: None,
                last_message_at: Some(Utc::now()),
                created_at: Utc::now(),
                harness_session_id: None,
                harness_session_cwd: None,
                space_id: Some("fade-lab-id".into()),
                last_seen_at: None,
            },
            Chat {
                id: "c-tabs".into(),
                device_id: "d1".into(),
                title: Some("Chat about the sidebar".into()),
                archived: false,
                cwd: Some("/home/m/comet".into()),
                branch: Some("tab-drag-followup".into()),
                checkout_id: None,
                config: None,
                last_message_preview: None,
                last_message_at: Some(Utc::now()),
                created_at: Utc::now(),
                harness_session_id: None,
                harness_session_cwd: None,
                space_id: Some("comet-id".into()),
                last_seen_at: None,
            },
        ]
    }

    fn archived_chat_titled(title: &str) -> Chat {
        Chat {
            id: "c-archived".into(),
            device_id: "d3".into(),
            title: Some(title.into()),
            archived: true,
            cwd: None,
            branch: None,
            checkout_id: None,
            config: None,
            last_message_preview: None,
            last_message_at: None,
            created_at: Utc::now(),
            harness_session_id: None,
            harness_session_cwd: None,
            space_id: None,
            last_seen_at: None,
        }
    }

    fn devices(id: &str) -> Option<String> {
        (id == "d1").then(|| "mac-studio".to_string())
    }

    #[test]
    fn empty_query_is_not_a_search() {
        assert!(filter("", &spaces(), &chats(), &devices).is_none());
        assert!(filter("   ", &spaces(), &chats(), &devices).is_none());
    }

    #[test]
    fn matches_space_name_and_session_title() {
        let r = filter("fade", &spaces(), &chats(), &devices).unwrap();
        assert_eq!(r.spaces, ["fade-lab-id"]);
        assert!(r.chats.contains(&"c-fade".to_string()));
    }

    #[test]
    fn matches_the_space_path_not_just_its_display_name() {
        // "fade-lab" lives at /home/m/work/acme/fade-lab.
        let r = filter("acme", &spaces(), &chats(), &devices).unwrap();
        assert_eq!(
            r.spaces,
            ["fade-lab-id"],
            "display_name falls back to the basename, so the path has to match too"
        );
        // c-fade's own title ("Fade exploration"), branch ("main") and device
        // (d2, unmapped) all miss "acme" — it can only land here through the
        // owning-space clause (space_id -> fade-lab-id -> path match).
        assert_eq!(
            r.chats,
            ["c-fade"],
            "a session must surface when its owning space matches, not just its own fields"
        );
    }

    #[test]
    fn matches_branch_and_device() {
        let r = filter("tab-drag", &spaces(), &chats(), &devices).unwrap();
        assert_eq!(r.chats, ["c-tabs"]);
        let r = filter("mac-studio", &spaces(), &chats(), &devices).unwrap();
        assert_eq!(r.chats, ["c-tabs"]);
    }

    #[test]
    fn matching_is_case_insensitive() {
        let lower = filter("fade", &spaces(), &chats(), &devices).unwrap();
        let upper = filter("FaDe", &spaces(), &chats(), &devices).unwrap();
        assert_eq!(lower.chats, upper.chats);
    }

    #[test]
    fn archived_chats_are_never_returned() {
        let mut all = chats();
        all.push(archived_chat_titled("fade something"));
        let r = filter("fade", &spaces(), &all, &devices).unwrap();
        assert!(!r.chats.iter().any(|id| id == "c-archived"));
    }

    /// `overview_chats` (the normal sidebar list) only shows a chat when its
    /// `space_id` resolves to a live space row; search must apply the same
    /// guard or it can hand back a hit the sidebar has nowhere to render.
    #[test]
    fn chats_with_a_dangling_space_id_are_never_returned() {
        let mut all = chats();
        all.push(Chat {
            id: "c-dangling".into(),
            device_id: "d1".into(),
            title: Some("fade orphan".into()),
            archived: false,
            cwd: None,
            branch: None,
            checkout_id: None,
            config: None,
            last_message_preview: None,
            last_message_at: None,
            created_at: Utc::now(),
            harness_session_id: None,
            harness_session_cwd: None,
            space_id: Some("deleted-space-id".into()),
            last_seen_at: None,
        });
        let r = filter("fade", &spaces(), &all, &devices).unwrap();
        assert!(
            !r.chats.iter().any(|id| id == "c-dangling"),
            "a chat whose space no longer exists must not surface, even on a title hit"
        );
    }

    #[test]
    fn no_matches_is_a_search_with_empty_groups() {
        let r = filter("wingleeio", &spaces(), &chats(), &devices).unwrap();
        assert!(r.spaces.is_empty() && r.chats.is_empty());
        assert!(r.is_empty());
    }

    #[test]
    fn match_run_splits_around_the_hit() {
        assert_eq!(
            match_run("Make the edge fade dissolve", "fade"),
            Some(("Make the edge ".into(), "fade".into(), " dissolve".into()))
        );
        assert_eq!(
            match_run("Make the edge FADE dissolve", "fade"),
            Some(("Make the edge ".into(), "FADE".into(), " dissolve".into())),
            "the run keeps the original casing, not the query's"
        );
        assert_eq!(match_run("no hit here", "fade"), None);
        assert_eq!(match_run("anything", ""), None);
        // Multi-byte text must never panic or slice mid-character.
        assert_eq!(
            match_run("héllo wörld", "wör").map(|r| r.1),
            Some("wör".into())
        );
        assert!(match_run("héllo", "é").is_some());
    }

    #[test]
    fn highlight_target_spaces_come_before_chats() {
        // 2 spaces, 3 chats: indices 0-1 are spaces, 2-4 are chats, 5+ is out
        // of range (e.g. a stale highlight after the result set shrank).
        assert_eq!(highlight_target(0, 2, 3), Some(HighlightTarget::Space(0)));
        assert_eq!(highlight_target(1, 2, 3), Some(HighlightTarget::Space(1)));
        assert_eq!(highlight_target(2, 2, 3), Some(HighlightTarget::Chat(0)));
        assert_eq!(highlight_target(4, 2, 3), Some(HighlightTarget::Chat(2)));
        assert_eq!(highlight_target(5, 2, 3), None);
    }

    #[test]
    fn highlight_target_handles_an_empty_group() {
        assert_eq!(highlight_target(0, 0, 1), Some(HighlightTarget::Chat(0)));
        assert_eq!(highlight_target(0, 1, 0), Some(HighlightTarget::Space(0)));
        assert_eq!(highlight_target(0, 0, 0), None);
    }
}
