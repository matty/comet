//! Sidebar search: one field over both spaces and sessions.
//!
//! Pure matching lives here as a free function with unit tests (the `rail.rs`
//! convention); rendering is an `impl Shell` extension below it.
//!
//! Search deliberately ignores [`SidebarScope`] — it always reads the whole
//! projected set. A scoped list is a convenience, never a wall.
//!
//! Result rows deliberately do NOT go through [`super::Shell::render_chat_row`]
//! / [`super::RowScope`]: that shared row needs plain `SharedString` title and
//! branch text, but a search hit has to tint the matched run wherever it
//! landed (title, branch, or the space name), which means building three
//! colored spans instead of one string. Rather than widen a well-tested,
//! shared type to carry element children (which would also force `RowScope`
//! off `Clone`/`PartialEq`, both load-bearing elsewhere), results render
//! through a parallel, self-contained set of row builders below that
//! reproduce the SAME shape `RowScope::All` draws — space+device line always
//! on top, regardless of the sidebar's current scope — just with tinting.

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
/// `query` (if any) in `accent` — the rest stays `base`. Falls back to a
/// plain line when there is no match (a hit that landed on a DIFFERENT line
/// of the same row, e.g. the branch, must not force every other line to tint
/// nothing).
fn tinted_line(text: &str, query: &str, base: gpui::Hsla, accent: gpui::Hsla) -> AnyElement {
    match match_run(text, query) {
        Some((before, matched, after)) => div()
            .flex()
            .flex_row()
            .min_w_0()
            .text_color(base)
            .child(SharedString::from(before))
            .child(
                div()
                    .flex_none()
                    .text_color(accent)
                    .child(SharedString::from(matched)),
            )
            .child(SharedString::from(after))
            .into_any_element(),
        None => div()
            .text_color(base)
            .child(SharedString::from(text.to_string()))
            .into_any_element(),
    }
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
fn highlight_ring(theme: &Theme) -> Vec<gpui::BoxShadow> {
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
    pub(super) fn open_search_result(&mut self, result: SearchHit, cx: &mut Context<Self>) {
        match result {
            SearchHit::Chat(id) => {
                self.state.update(cx, |s, cx| s.select_chat(Some(id), cx));
            }
            SearchHit::Space(id) => self.activate_space(id, cx),
        }
        self.clear_search(cx);
    }

    /// Bubbled ↑↓/⏎/esc from the focused search input — the `"SidebarSearch"`
    /// context leaves them unbound (see [`crate::composer::init`]) so they
    /// reach here instead of moving the caret, the same shape as
    /// `AddSpaceFlow::add_space_key`.
    pub(super) fn search_key(&mut self, event: &gpui::KeyDownEvent, cx: &mut Context<Self>) {
        let query = self.search_query(cx).trim().to_string();
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
        let key = popover::classify_key(
            event.keystroke.key.as_str(),
            event.keystroke.modifiers.platform,
            event.keystroke.modifiers.control,
        );
        let total = results.spaces.len() + results.chats.len();
        match key {
            popover::MenuKey::Escape => self.clear_search(cx),
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
    pub(super) fn render_search_field(
        &mut self,
        searching: bool,
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
            .border_color(if searching {
                theme.accent.opacity(0.55)
            } else {
                theme.border
            })
            .bg(if searching {
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
                    .text_color(if searching {
                        theme.accent
                    } else {
                        theme.text_faint
                    }),
            )
            .child(div().flex_1().min_w_0().child(self.search_input.clone()))
            .when(!searching, |el| {
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
            .when(searching, |el| {
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
    /// normal Sessions list wholesale — see the module doc for why these rows
    /// don't go through `render_chat_row`.
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

        children.push(
            spaces::section_header("Spaces", true, theme, None)
                .child(search_count_chip(results.spaces.len(), theme))
                .into_any_element(),
        );
        for (i, id) in results.spaces.iter().enumerate() {
            let space = self.state.read(cx).space_row(id).cloned();
            let Some(space) = space else { continue };
            let highlighted = highlight_target(
                self.search_active,
                results.spaces.len(),
                results.chats.len(),
            ) == Some(HighlightTarget::Space(i));
            children.push(self.render_search_space_row(&space, query, highlighted, theme, cx));
        }

        children.push(
            spaces::section_header("Sessions", false, theme, None)
                .child(search_count_chip(results.chats.len(), theme))
                .into_any_element(),
        );
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
            children.push(self.render_search_chat_row(&chat, query, highlighted, theme, cx));
        }

        children.push(render_search_footer(theme));
        children
    }

    /// One space result row: folder icon, tinted display name. Always opens
    /// via [`Self::open_search_result`], which is the only path that switches
    /// `sidebar_scope`.
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
        let title = tinted_line(&name, query, theme.text.opacity(0.85), theme.accent);

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
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .child(title),
            )
            .into_any_element()
    }

    /// One session result row — the `RowScope::All` shape (space+device line,
    /// title+time, branch+harness), rebuilt here rather than through
    /// `render_chat_row` so title/branch/space can each tint their matched
    /// run. Always shows the owning space, regardless of the sidebar's
    /// current scope.
    fn render_search_chat_row(
        &mut self,
        chat: &Chat,
        query: &str,
        highlighted: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let now = Utc::now();
        let (status, space_name, device, host_offline, selected) = {
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
            (status, space_name, device, host_offline, selected)
        };
        let time_ago: SharedString =
            format_time_ago(chat.last_message_at.unwrap_or(chat.created_at), now).into();
        let title_text =
            transcript::single_line(&chat.title.clone().unwrap_or_else(|| "New session".into()));
        let branch_text = chat
            .branch
            .as_deref()
            .map(str::trim)
            .filter(|b| !b.is_empty());

        let id = chat.id.clone();
        let click_id = id.clone();
        let fade_key = format!("search-chat-{id}");
        let dot_color = spaces::status_dot_color(status, theme);
        let selected_wash = crate::theme::glass_selected_bg();
        let rest_bg = if selected {
            selected_wash
        } else {
            crate::theme::wash(0.0)
        };
        let hover_bg = if selected {
            selected_wash
        } else {
            theme.glass_hover()
        };
        let rest_text = if selected {
            theme.text
        } else {
            theme.text.opacity(0.8)
        };

        let title_el = tinted_line(&title_text, query, rest_text, theme.accent);
        let space_el = tinted_line(
            &space_name,
            query,
            theme.text_muted.opacity(0.75),
            theme.accent,
        );

        let harness = chat.config.as_ref().map(|c| c.harness);

        div()
            .id(SharedString::from(format!("search-chat-{id}")))
            .flex()
            .flex_col()
            .rounded(px(8.0))
            .px(px(Theme::SPACE_SM))
            .py(px(5.0))
            .bg(motion::hover_blend(&fade_key, rest_bg, hover_bg))
            .when(highlighted, |el| el.shadow(highlight_ring(theme)))
            .on_hover(motion::hover_listener(fade_key))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                this.open_search_result(SearchHit::Chat(click_id.clone()), cx);
            }))
            // Line 0: the owning space — ALWAYS shown here (the `RowScope::All`
            // line), regardless of the sidebar's current scope: a hit in
            // another space is useless if it can't say which one.
            .child(
                div()
                    .w_full()
                    .mb(px(2.0))
                    .pl(px(14.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(4.0))
                    .text_size(px(11.0))
                    .line_height(px(14.0))
                    .child(
                        icon(icons::FOLDER)
                            .size(px(11.0))
                            .flex_none()
                            .text_color(theme.text_muted.opacity(0.5)),
                    )
                    .child(div().min_w_0().truncate().child(space_el))
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
                    ),
            )
            // Line 1: status dot, title, time-ago.
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(Theme::SPACE_SM))
                    .child(div().size(px(6.0)).rounded_full().flex_none().bg(dot_color))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(13.0))
                            .line_height(px(18.0))
                            .child(title_el),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(11.0))
                            .line_height(px(18.0))
                            .text_color(theme.text_muted.opacity(0.45))
                            .child(time_ago),
                    ),
            )
            // Line 2: branch, agent mark pinned right.
            .child(
                div()
                    .w_full()
                    .mt(px(2.0))
                    .pl(px(14.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(4.0))
                    .text_size(px(11.0))
                    .line_height(px(14.0))
                    .when_some(branch_text, |el, branch| {
                        el.child(
                            icon(icons::GIT_BRANCH)
                                .size(px(11.0))
                                .flex_none()
                                .text_color(theme.text_muted.opacity(0.5)),
                        )
                        .child(div().min_w_0().truncate().child(tinted_line(
                            branch,
                            query,
                            theme.text_muted.opacity(0.5),
                            theme.accent,
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
