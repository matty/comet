//! The right-pane "Changes" content (feature-inventory §1.11): a unified-diff
//! viewer over `WatchCheckoutDiffs`.
//!
//! - pure patch parser: `diff --git` sections → file/hunk/line/notice rows,
//!   with add/delete/rename/binary detection and per-file counts;
//! - resolution: the shown diff matches the selected chat by `checkout_id`
//!   first, then by device+cwd, then cwd alone;
//! - states: *preparing* (no diff yet), *clean* (empty patch), *list*; a watch
//!   error shows a banner while the last content stays;
//! - virtualized with gpui `list()` at LINE granularity — every file header,
//!   hunk header, and diff line is its own row (the flat model Zed's editor
//!   uses for its project diff: only the visible slice materializes, and a
//!   collapsed file's body rows are removed from the list outright, not
//!   hidden); each section collapses with a 180 ms height tween on a
//!   clipped stand-in row (analytic heights, capped to what the clip can
//!   reveal) and a 200 ms chevron transition;
//! - syntax highlight reuses the markdown tokenizer per diff line, computed
//!   time-sliced on the background executor and applied as paint-only run
//!   colors (layout never changes).

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    AnyElement, App, Context, Entity, ListAlignment, ListState, SharedString, Subscription, Task,
    Window, div, font, list, prelude::*, px,
};

use comet_proto::{Chat, CheckoutDiff};
use comet_rpc::methods;

use crate::markdown::highlight::{Lang, LineCarry, Token, tokenize_line};
use crate::markdown::render;
use crate::motion::{self, AnimationExt as _, CHEVRON, COLLAPSE};
use crate::state::{AppState, ServerClient};
use crate::theme::Theme;

// ---------------------------------------------------------------------------
// Layout numbers (analytic — they drive the fold tween)
// ---------------------------------------------------------------------------

pub const FILE_HEADER_HEIGHT: f32 = 36.0;
pub const HUNK_HEADER_HEIGHT: f32 = 28.0;
pub const DIFF_LINE_HEIGHT: f32 = 21.0;
pub const NOTICE_HEIGHT: f32 = 24.0;
pub const BODY_BOTTOM_PAD: f32 = 8.0;
/// Gutter width per line-number column.
pub const GUTTER_WIDTH: f32 = 36.0;
/// The +/−/· marker column between the gutters and the code.
pub const MARKER_WIDTH: f32 = 28.0;
/// Width of the coloured accent bar on the left edge of +/− rows.
pub const ACCENT_BAR_WIDTH: f32 = 3.0;
const DIFF_TEXT_SIZE: f32 = 12.0;

// ---------------------------------------------------------------------------
// Patch model + parser (pure)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Add,
    Del,
    /// `\ No newline at end of file` and friends.
    Meta,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DiffLine {
    pub kind: LineKind,
    pub old_no: Option<u32>,
    pub new_no: Option<u32>,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Hunk {
    pub header: String,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Added,
    Deleted,
    Modified,
    Renamed,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FileDiff {
    /// Display path (the post-change side).
    pub path: String,
    /// Pre-rename path, when different.
    pub old_path: Option<String>,
    pub status: FileStatus,
    pub binary: bool,
    /// Parser-collected notices (mode changes etc.).
    pub notices: Vec<String>,
    pub hunks: Vec<Hunk>,
    pub additions: u32,
    pub deletions: u32,
    /// Largest line number on either side — sizes the gutters analytically
    /// (a fixed column overflowed past 4 digits; user report).
    pub max_line: u32,
}

impl FileDiff {
    fn new(path: String, old_path: Option<String>) -> Self {
        Self {
            path,
            old_path,
            status: FileStatus::Modified,
            binary: false,
            notices: Vec::new(),
            hunks: Vec::new(),
            additions: 0,
            deletions: 0,
            max_line: 0,
        }
    }
}

/// Width of one line-number gutter column, fitted to the file's largest
/// line number: 11px mono ≈ 6.6px per digit, the 8px right pad, and a 6px
/// left gap so the number never abuts the accent bar (at 4 digits the old
/// formula left 1.6px — visually touching; user report). Never narrower
/// than the classic 36px column.
pub fn gutter_width(file: &FileDiff) -> f32 {
    let digits = file.max_line.max(1).ilog10() + 1;
    (digits as f32 * 6.6 + 8.0 + 6.0).max(GUTTER_WIDTH)
}

fn strip_git_prefix(path: &str) -> &str {
    path.strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path)
}

/// Split the tail of a `diff --git a/… b/…` line into (old, new) paths.
/// Quoted paths (spaces/unicode) are handled; for unquoted paths with spaces
/// the split favors the last ` b/` separator, which is git's own convention.
fn parse_git_paths(rest: &str) -> (String, String) {
    fn unquote(s: &str) -> String {
        let trimmed = s.trim();
        if trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"') {
            trimmed[1..trimmed.len() - 1]
                .replace("\\\"", "\"")
                .replace("\\\\", "\\")
        } else {
            trimmed.to_string()
        }
    }
    if let Some(pos) = rest.rfind(" b/").or_else(|| rest.rfind(" \"b/")) {
        let old = unquote(&rest[..pos]);
        let new = unquote(&rest[pos + 1..]);
        (
            strip_git_prefix(&old).to_string(),
            strip_git_prefix(&new).to_string(),
        )
    } else {
        let p = strip_git_prefix(&unquote(rest)).to_string();
        (p.clone(), p)
    }
}

/// Parse one `@@ -a[,b] +c[,d] @@ …` header into starting line numbers.
fn parse_hunk_header(line: &str) -> Option<(u32, u32)> {
    let rest = line.strip_prefix("@@")?;
    let minus = rest.find('-')?;
    let after_minus = &rest[minus + 1..];
    let old: u32 = after_minus
        .split(|c: char| c == ',' || c.is_whitespace())
        .next()?
        .parse()
        .ok()?;
    let plus = rest.find('+')?;
    let after_plus = &rest[plus + 1..];
    let new: u32 = after_plus
        .split(|c: char| c == ',' || c.is_whitespace())
        .next()?
        .parse()
        .ok()?;
    Some((old, new))
}

/// Parse a unified git patch into file sections. Tolerant: unknown header
/// lines are skipped, truncated hunks keep what parsed so far.
pub fn parse_patch(patch: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    let mut in_hunk = false;
    let mut old_no: u32 = 0;
    let mut new_no: u32 = 0;

    for raw in patch.lines() {
        if let Some(rest) = raw.strip_prefix("diff --git ") {
            let (old, new) = parse_git_paths(rest);
            let old_path = (old != new).then_some(old);
            files.push(FileDiff::new(new, old_path));
            in_hunk = false;
            continue;
        }
        let Some(file) = files.last_mut() else {
            continue;
        };

        if raw.starts_with("@@") {
            if let Some((o, n)) = parse_hunk_header(raw) {
                old_no = o;
                new_no = n;
                file.hunks.push(Hunk {
                    header: raw.to_string(),
                    lines: Vec::new(),
                });
                in_hunk = true;
            }
            continue;
        }

        if in_hunk {
            let mut chars = raw.chars();
            let marker = chars.next();
            let body: String = chars.collect();
            let line = match marker {
                Some('+') => {
                    file.additions += 1;
                    let l = DiffLine {
                        kind: LineKind::Add,
                        old_no: None,
                        new_no: Some(new_no),
                        text: body,
                    };
                    new_no += 1;
                    Some(l)
                }
                Some('-') => {
                    file.deletions += 1;
                    let l = DiffLine {
                        kind: LineKind::Del,
                        old_no: Some(old_no),
                        new_no: None,
                        text: body,
                    };
                    old_no += 1;
                    Some(l)
                }
                Some(' ') | None => {
                    let l = DiffLine {
                        kind: LineKind::Context,
                        old_no: Some(old_no),
                        new_no: Some(new_no),
                        text: body,
                    };
                    old_no += 1;
                    new_no += 1;
                    Some(l)
                }
                Some('\\') => Some(DiffLine {
                    kind: LineKind::Meta,
                    old_no: None,
                    new_no: None,
                    text: raw.trim_start_matches('\\').trim().to_string(),
                }),
                _ => {
                    // A non-hunk line ends the hunk; reprocess as a header.
                    in_hunk = false;
                    None
                }
            };
            if let Some(line) = line
                && let Some(hunk) = file.hunks.last_mut()
            {
                file.max_line = file
                    .max_line
                    .max(line.old_no.unwrap_or(0))
                    .max(line.new_no.unwrap_or(0));
                hunk.lines.push(line);
                continue;
            }
            if in_hunk {
                continue;
            }
        }

        // File header territory.
        if raw.starts_with("new file mode") {
            file.status = FileStatus::Added;
        } else if raw.starts_with("deleted file mode") {
            file.status = FileStatus::Deleted;
        } else if let Some(from) = raw.strip_prefix("rename from ") {
            file.status = FileStatus::Renamed;
            file.old_path = Some(from.trim().to_string());
        } else if let Some(to) = raw.strip_prefix("rename to ") {
            file.status = FileStatus::Renamed;
            file.path = to.trim().to_string();
        } else if raw.starts_with("Binary files") || raw.starts_with("GIT binary patch") {
            file.binary = true;
        } else if let Some(mode) = raw.strip_prefix("new mode ") {
            file.notices
                .push(format!("Mode changed to {}", mode.trim()));
        } else if let Some(new) = raw.strip_prefix("+++ ") {
            let new = new.trim();
            if new == "/dev/null" {
                file.status = FileStatus::Deleted;
            } else if file.old_path.is_none() {
                file.path = strip_git_prefix(new).to_string();
            }
        } else if let Some(old) = raw.strip_prefix("--- ")
            && old.trim() == "/dev/null"
        {
            file.status = FileStatus::Added;
        }
        // "index …", "similarity index …", "old mode …" etc.: skipped.
    }
    files
}

/// Derived per-file notice rows (new/deleted/renamed/binary + parser notices).
pub fn file_notices(file: &FileDiff) -> Vec<String> {
    let mut notices = Vec::new();
    match file.status {
        FileStatus::Added => notices.push("New file".to_string()),
        FileStatus::Deleted => notices.push("Deleted file".to_string()),
        FileStatus::Renamed => {
            let from = file.old_path.as_deref().unwrap_or("?");
            notices.push(format!("Renamed from {from}"));
        }
        FileStatus::Modified => {}
    }
    if file.binary {
        notices.push("Binary file — contents not shown".to_string());
    }
    notices.extend(file.notices.iter().cloned());
    notices
}

/// Cap a file's hunks at `max_lines` total diff lines, appending a notice
/// when lines were dropped. The transcript renders a tool diff as ONE
/// stacked element inside its row, so an unbounded diff (a fetched
/// full-diff blob, a whole-file rewrite) would otherwise build tens of
/// thousands of elements every frame it is visible.
pub fn truncate_file_lines(file: &mut FileDiff, max_lines: usize) {
    let total: usize = file.hunks.iter().map(|h| h.lines.len()).sum();
    if total <= max_lines {
        return;
    }
    let mut budget = max_lines;
    file.hunks.retain_mut(|hunk| {
        if budget == 0 {
            return false;
        }
        if hunk.lines.len() > budget {
            hunk.lines.truncate(budget);
        }
        budget -= hunk.lines.len();
        true
    });
    file.notices.push(format!(
        "Diff truncated — showing first {max_lines} of {total} lines"
    ));
    // The gutter fits what actually renders.
    file.max_line = file
        .hunks
        .iter()
        .flat_map(|h| &h.lines)
        .map(|l| l.old_no.unwrap_or(0).max(l.new_no.unwrap_or(0)))
        .max()
        .unwrap_or(0);
}

/// Analytic expanded-body height — drives the 180 ms fold tween without
/// measurement.
pub fn body_height(file: &FileDiff) -> f32 {
    let notices = file_notices(file).len() as f32 * NOTICE_HEIGHT;
    let hunks = file.hunks.len() as f32 * HUNK_HEADER_HEIGHT;
    let lines: usize = file.hunks.iter().map(|h| h.lines.len()).sum();
    notices + hunks + lines as f32 * DIFF_LINE_HEIGHT + BODY_BOTTOM_PAD
}

// ---------------------------------------------------------------------------
// Resolution + states (pure)
// ---------------------------------------------------------------------------

/// The diff shown for a chat: `checkout_id` match first, then device+cwd,
/// then cwd alone (§1.11).
pub fn resolve_diff<'a>(diffs: &'a [CheckoutDiff], chat: &Chat) -> Option<&'a CheckoutDiff> {
    if let Some(checkout_id) = chat.checkout_id.as_deref()
        && let Some(diff) = diffs.iter().find(|d| d.checkout_id == checkout_id)
    {
        return Some(diff);
    }
    let cwd = chat.cwd.as_deref()?;
    diffs
        .iter()
        .find(|d| d.device_id == chat.device_id && d.cwd == cwd)
        .or_else(|| diffs.iter().find(|d| d.cwd == cwd))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffPhase {
    /// No diff for this checkout yet.
    Preparing,
    /// Diff arrived and it's empty — working tree clean.
    Clean,
    List,
}

pub fn diff_phase(resolved: Option<&CheckoutDiff>) -> DiffPhase {
    match resolved {
        None => DiffPhase::Preparing,
        Some(diff) if diff.patch.trim().is_empty() && diff.files.is_empty() => DiffPhase::Clean,
        Some(_) => DiffPhase::List,
    }
}

/// Header label: "N Uncommitted change(s)".
pub fn uncommitted_label(count: usize) -> String {
    if count == 1 {
        "1 Uncommitted change".to_string()
    } else {
        format!("{count} Uncommitted changes")
    }
}

/// Fold a `WatchCheckoutDiffs` frame into the diff set. Accepts either a full
/// list (replace) or a single `CheckoutDiff` (upsert by checkout id) — the
/// contract streams `CheckoutDiff` items, but list frames cost nothing to
/// support. Returns whether anything changed.
pub fn apply_diff_frame(diffs: &mut Vec<CheckoutDiff>, value: serde_json::Value) -> bool {
    if let Ok(all) = serde_json::from_value::<Vec<CheckoutDiff>>(value.clone()) {
        if *diffs != all {
            *diffs = all;
            return true;
        }
        return false;
    }
    match serde_json::from_value::<CheckoutDiff>(value) {
        Ok(one) => {
            if let Some(existing) = diffs.iter_mut().find(|d| d.checkout_id == one.checkout_id) {
                if *existing == one {
                    return false;
                }
                *existing = one;
            } else {
                diffs.push(one);
            }
            true
        }
        Err(err) => {
            tracing::warn!(error = %err, "changes: dropping malformed diff frame");
            false
        }
    }
}

/// Language for a file path's extension (drives per-line highlighting).
pub fn lang_for_path(path: &str) -> Option<Lang> {
    comet_syntax::language_for_path(path)
}

fn hash64(parts: &[&str]) -> u64 {
    let mut hasher = std::hash::DefaultHasher::new();
    for p in parts {
        p.hash(&mut hasher);
    }
    hasher.finish()
}

// ---------------------------------------------------------------------------
// Entity
// ---------------------------------------------------------------------------

struct ParsedDiff {
    /// `checkout_id:checksum` — identity of the parsed content.
    key: String,
    truncated: bool,
    additions: u32,
    deletions: u32,
    file_count: usize,
    files: Arc<Vec<FileDiff>>,
}

// ---------------------------------------------------------------------------
// Row model — the diff flattened to line granularity (pure)
// ---------------------------------------------------------------------------

/// One virtualized list row. The diff is flattened so each visible LINE is
/// its own row (Zed's editor draws exactly the visible line range the same
/// way): scrolling a 10k-line file materializes ~50 line rows per frame, not
/// one 10k-line element, and a collapsed file contributes no body rows at
/// all. Heights are the analytic constants above — no measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffRow {
    FileHeader {
        file: u32,
    },
    Notice {
        file: u32,
        notice: u32,
    },
    HunkHeader {
        file: u32,
        hunk: u32,
    },
    Line {
        file: u32,
        hunk: u32,
        line: u32,
        /// Flat index across the file's hunks — keys into the highlight slot.
        flat: u32,
    },
    /// Trailing pad closing an expanded body ([`BODY_BOTTOM_PAD`]).
    BodyPad {
        file: u32,
    },
    /// A body mid-fold-tween: one height-animated, clipped row standing in
    /// for the whole body. Only the slice that can be revealed is built —
    /// the tween never pays for off-screen lines.
    FoldingBody {
        file: u32,
    },
}

/// Rows an expanded body contributes (notices + hunk headers + lines + pad).
pub fn body_row_count(file: &FileDiff) -> usize {
    let lines: usize = file.hunks.iter().map(|h| h.lines.len()).sum();
    file_notices(file).len() + file.hunks.len() + lines + 1
}

/// The steady-state body rows of one expanded file.
pub fn body_rows(file_ix: u32, file: &FileDiff) -> Vec<DiffRow> {
    let mut rows = Vec::with_capacity(body_row_count(file));
    for notice in 0..file_notices(file).len() {
        rows.push(DiffRow::Notice {
            file: file_ix,
            notice: notice as u32,
        });
    }
    let mut flat = 0u32;
    for (hunk_ix, hunk) in file.hunks.iter().enumerate() {
        rows.push(DiffRow::HunkHeader {
            file: file_ix,
            hunk: hunk_ix as u32,
        });
        for line_ix in 0..hunk.lines.len() {
            rows.push(DiffRow::Line {
                file: file_ix,
                hunk: hunk_ix as u32,
                line: line_ix as u32,
                flat,
            });
            flat += 1;
        }
    }
    rows.push(DiffRow::BodyPad { file: file_ix });
    rows
}

/// Flatten all files into rows + each file's row span (header at
/// `range.start`, body rows after it). `collapsed(ix)` folds a file to just
/// its header.
pub fn flatten_rows(
    files: &[FileDiff],
    mut collapsed: impl FnMut(usize) -> bool,
) -> (Vec<DiffRow>, Vec<std::ops::Range<usize>>) {
    let mut rows = Vec::new();
    let mut ranges = Vec::with_capacity(files.len());
    for (ix, file) in files.iter().enumerate() {
        let start = rows.len();
        rows.push(DiffRow::FileHeader { file: ix as u32 });
        if !collapsed(ix) {
            rows.extend(body_rows(ix as u32, file));
        }
        ranges.push(start..rows.len());
    }
    (rows, ranges)
}

#[derive(Default, Clone, Copy)]
struct FileFold {
    collapsed: bool,
    /// Bumped per toggle — keys the height tween + chevron transition.
    epoch: usize,
    from: f32,
    to: f32,
    /// When the toggle happened: the tweens are armed only briefly after the
    /// click — gpui replays an element's animation on remount, and in the
    /// virtualized list a row scrolling back into view is a remount (the
    /// transcript's tool groups had the same flash; user report).
    toggled_at: Option<std::time::Instant>,
}

/// Tween arming window after a fold toggle (COLLAPSE's 180ms plus margin).
const FOLD_TWEEN_WINDOW: Duration = Duration::from_millis(400);

/// Ceiling on how much body a fold tween's stand-in row materializes. A
/// tween always starts from a clicked (on-screen) header, so the revealable
/// slice is at most one viewport tall — everything past this is clipped or
/// below the fold either way.
const FOLD_TWEEN_MAX_PX: f32 = 2400.0;

impl FileFold {
    fn animating(&self) -> bool {
        self.epoch > 0
            && self
                .toggled_at
                .is_some_and(|at| at.elapsed() < FOLD_TWEEN_WINDOW)
    }
}

struct HighlightSlot {
    fingerprint: u64,
    lines: Option<Arc<Vec<Vec<Token>>>>,
    _task: Option<Task<()>>,
}

async fn yield_now() {
    let mut yielded = false;
    futures::future::poll_fn(move |cx| {
        if yielded {
            std::task::Poll::Ready(())
        } else {
            yielded = true;
            cx.waker().wake_by_ref();
            std::task::Poll::Pending
        }
    })
    .await
}

/// The Changes pane entity. Lazy: no RPC until [`Changes::ensure_watch`] runs
/// (the shell calls it when the pane first opens).
pub struct Changes {
    state: Entity<AppState>,
    diffs: Vec<CheckoutDiff>,
    started: bool,
    error: Option<SharedString>,
    /// Device the running watch targets: `None` = the connected engine itself,
    /// `Some(id)` = a remote chat's host (relay-forwarded). The stream only
    /// carries the TARGET device's checkouts, so a selection change onto a
    /// chat hosted elsewhere tears the watch down and re-subscribes.
    watch_target: Option<comet_proto::ServerId>,
    watch_task: Option<Task<()>>,
    parsed: Option<ParsedDiff>,
    parse_task: Option<Task<()>>,
    folds: HashMap<String, FileFold>,
    highlights: HashMap<String, HighlightSlot>,
    /// The flattened row model the list virtualizes over (line granularity;
    /// collapsed bodies excluded) + each file's row span within it.
    rows: Vec<DiffRow>,
    row_ranges: Vec<std::ops::Range<usize>>,
    /// Sweeps [`DiffRow::FoldingBody`] stand-ins back to steady-state rows
    /// once their tween window elapses.
    fold_settle: Option<Task<()>>,
    list: ListState,
    _observe: Subscription,
}

impl Changes {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&state, |this: &mut Self, _, cx| this.sync(cx));
        Self {
            state,
            diffs: Vec::new(),
            started: false,
            error: None,
            watch_target: None,
            watch_task: None,
            parsed: None,
            parse_task: None,
            folds: HashMap::new(),
            highlights: HashMap::new(),
            rows: Vec::new(),
            row_ranges: Vec::new(),
            fold_settle: None,
            // Rows are single lines now — a deep overdraw is cheap and keeps
            // fast wheel flicks from outrunning measurement.
            list: ListState::new(0, ListAlignment::Top, px(1024.0)),
            _observe: observe,
        }
    }

    fn desired_target(&self, cx: &App) -> Option<comet_proto::ServerId> {
        self.state
            .read(cx)
            .selected_chat
            .as_ref()
            .map(|chat| chat.server_id.clone())
    }

    /// Start the `WatchCheckoutDiffs` subscription (idempotent per target).
    /// Retries with a flat 2 s delay if the stream fails or ends; the last
    /// content stays visible under an error banner meanwhile.
    pub fn ensure_watch(&mut self, cx: &mut Context<Self>) {
        let target = self.desired_target(cx);
        if self.started && self.watch_target == target {
            return;
        }
        let Some(engine) = self.state.read(cx).selected_client() else {
            // Engine still booting — retry on the next state change via sync().
            return;
        };
        // Retarget: the old task (and its stream) drop; rows from the previous
        // device would resolve against the wrong checkouts, so clear them.
        if self.started {
            self.diffs.clear();
            self.error = None;
        }
        self.started = true;
        self.watch_target = target.clone();
        self.watch_task = Some(Self::spawn_watch(engine, target, cx));
    }

    fn spawn_watch(
        engine: ServerClient,
        _target: Option<comet_proto::ServerId>,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                let subscribed = engine
                    .client()
                    .subscribe(methods::WATCH_CHECKOUT_DIFFS, serde_json::Value::Null)
                    .await;
                match subscribed {
                    Ok(mut rx) => {
                        while let Some(value) = rx.recv().await {
                            let alive = this.update(cx, |changes, cx| {
                                changes.error = None;
                                if apply_diff_frame(&mut changes.diffs, value) {
                                    changes.sync(cx);
                                    cx.notify();
                                }
                            });
                            if alive.is_err() {
                                return;
                            }
                        }
                        // Stream ended (engine restart / reconnect): banner + retry.
                        if this
                            .update(cx, |changes, cx| {
                                changes.error = Some("Diff stream interrupted — retrying".into());
                                cx.notify();
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                    Err(err) => {
                        if this
                            .update(cx, |changes, cx| {
                                changes.error =
                                    Some(format!("Diff watch unavailable: {err}").into());
                                cx.notify();
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                }
                cx.background_executor().timer(Duration::from_secs(2)).await;
            }
        })
    }

    fn resolved(&self, cx: &App) -> Option<CheckoutDiff> {
        let state = self.state.read(cx);
        let chat = state.selected_chat_row()?;
        resolve_diff(&self.diffs, chat).cloned()
    }

    /// Reconcile parsed content with the currently-resolved diff.
    fn sync(&mut self, cx: &mut Context<Self>) {
        // The watch follows the selected chat's host device (idempotent when
        // the target is unchanged); a boot-deferred attempt retries here too.
        self.ensure_watch(cx);
        let Some(diff) = self.resolved(cx) else {
            if self.parsed.take().is_some() {
                self.rows.clear();
                self.row_ranges.clear();
                self.list.reset(0);
                self.folds.clear();
                self.highlights.clear();
                cx.notify();
            }
            return;
        };
        let key = format!("{}:{}", diff.checkout_id, diff.checksum);
        if self.parsed.as_ref().is_some_and(|p| p.key == key) {
            return;
        }
        // Parse off the render path — patches run to megabytes.
        let patch = diff.patch.clone();
        let truncated = diff.truncated;
        let additions = diff.additions;
        let deletions = diff.deletions;
        let file_count = diff.files.len();
        self.parse_task = Some(cx.spawn(async move |this, cx| {
            let files = cx
                .background_executor()
                .spawn(async move { parse_patch(&patch) })
                .await;
            this.update(cx, |changes, cx| {
                // Late results for a superseded diff are re-checked by key.
                let current = changes
                    .resolved(cx)
                    .map(|d| format!("{}:{}", d.checkout_id, d.checksum));
                if current.as_deref() != Some(key.as_str()) {
                    return;
                }
                let file_count = if file_count > 0 {
                    file_count
                } else {
                    files.len()
                };
                changes.folds.clear();
                changes.highlights.clear();
                let (rows, ranges) = flatten_rows(&files, |_| false);
                // The uniform hint keeps offsets for never-rendered rows
                // sane (most rows ARE lines); real heights land as rows
                // render.
                changes
                    .list
                    .reset_with_uniform_height(rows.len(), px(DIFF_LINE_HEIGHT));
                changes.rows = rows;
                changes.row_ranges = ranges;
                changes.parsed = Some(ParsedDiff {
                    key,
                    truncated,
                    additions,
                    deletions,
                    file_count,
                    files: Arc::new(files),
                });
                cx.notify();
            })
            .ok();
        }));
    }

    /// Swap one file's body rows (everything after its header) for
    /// `new_body`, splicing both the row model and the list state. gpui's
    /// `splice` shifts the logical scroll anchor by the count delta, so
    /// content below the fold stays put.
    fn replace_file_body(&mut self, file_ix: usize, new_body: Vec<DiffRow>) {
        let Some(range) = self.row_ranges.get(file_ix).cloned() else {
            return;
        };
        let body = range.start + 1..range.end;
        let delta = new_body.len() as isize - body.len() as isize;
        self.list.splice(body.clone(), new_body.len());
        self.rows.splice(body, new_body);
        self.row_ranges[file_ix] = range.start..(range.end as isize + delta) as usize;
        for r in &mut self.row_ranges[file_ix + 1..] {
            *r = (r.start as isize + delta) as usize..(r.end as isize + delta) as usize;
        }
    }

    fn toggle_fold(&mut self, file_ix: usize, cx: &mut Context<Self>) {
        let Some(parsed) = &self.parsed else {
            return;
        };
        let Some(file) = parsed.files.get(file_ix) else {
            return;
        };
        let expanded_height = body_height(file);
        let fold = self.folds.entry(file.path.clone()).or_default();
        let currently_collapsed = fold.collapsed;
        fold.from = if currently_collapsed {
            0.0
        } else {
            expanded_height
        };
        fold.to = if currently_collapsed {
            expanded_height
        } else {
            0.0
        };
        fold.collapsed = !currently_collapsed;
        fold.epoch += 1;
        fold.toggled_at = Some(std::time::Instant::now());
        // The body tweens as ONE clipped stand-in row; the settle sweep
        // swaps it for steady rows (all lines, or none) once the window
        // elapses.
        self.replace_file_body(
            file_ix,
            vec![DiffRow::FoldingBody {
                file: file_ix as u32,
            }],
        );
        self.ensure_fold_settle(cx);
    }

    /// Keep a sweep alive while any [`DiffRow::FoldingBody`] stand-ins
    /// remain; each tick settles the ones whose tween window has elapsed.
    fn ensure_fold_settle(&mut self, cx: &mut Context<Self>) {
        if self.fold_settle.is_some() {
            return;
        }
        self.fold_settle = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(FOLD_TWEEN_WINDOW).await;
                let more = this
                    .update(cx, |changes, cx| changes.settle_folds(cx))
                    .unwrap_or(false);
                if !more {
                    break;
                }
            }
            this.update(cx, |changes, _| changes.fold_settle = None)
                .ok();
        }));
    }

    /// Replace every settled folding stand-in with its steady-state rows.
    /// Returns whether any stand-ins are still mid-tween.
    fn settle_folds(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(parsed) = &self.parsed else {
            return false;
        };
        let files = parsed.files.clone();
        let mut pending = false;
        for file_ix in (0..self.row_ranges.len()).rev() {
            let range = &self.row_ranges[file_ix];
            let folding = self.rows.get(range.start + 1)
                == Some(&DiffRow::FoldingBody {
                    file: file_ix as u32,
                });
            if !folding {
                continue;
            }
            let Some(file) = files.get(file_ix) else {
                continue;
            };
            let fold = self.folds.get(&file.path).copied().unwrap_or_default();
            if fold.animating() {
                pending = true;
                continue;
            }
            let body = if fold.collapsed {
                Vec::new()
            } else {
                body_rows(file_ix as u32, file)
            };
            self.replace_file_body(file_ix, body);
        }
        cx.notify();
        pending
    }

    /// Tokens for a file's diff lines (paint-only). Kicks a time-sliced
    /// background tokenize when missing; returns the current best.
    fn request_highlight(
        &mut self,
        file: &FileDiff,
        parsed_key: &str,
        cx: &mut Context<Self>,
    ) -> Option<Arc<Vec<Vec<Token>>>> {
        let lang = lang_for_path(&file.path)?;
        let fingerprint = hash64(&[parsed_key, &file.path]);
        if let Some(slot) = self.highlights.get(&file.path)
            && slot.fingerprint == fingerprint
        {
            return slot.lines.clone();
        }
        let texts: Vec<(LineKind, String)> = file
            .hunks
            .iter()
            .flat_map(|h| h.lines.iter().map(|l| (l.kind, l.text.clone())))
            .collect();
        let path = file.path.clone();
        let task = cx.spawn(async move |this, cx| {
            let lines = cx
                .background_executor()
                .spawn(async move {
                    let mut out = Vec::with_capacity(texts.len());
                    for (ix, (kind, text)) in texts.iter().enumerate() {
                        // Diff lines are fragments — no carry across lines.
                        let tokens = match kind {
                            LineKind::Meta => Vec::new(),
                            _ => tokenize_line(lang, text, LineCarry::None).0,
                        };
                        out.push(tokens);
                        if ix % 128 == 127 {
                            yield_now().await;
                        }
                    }
                    out
                })
                .await;
            this.update(cx, |changes, cx| {
                if let Some(slot) = changes.highlights.get_mut(&path)
                    && slot.fingerprint == fingerprint
                {
                    slot.lines = Some(Arc::new(lines));
                    cx.notify();
                }
            })
            .ok();
        });
        self.highlights.insert(
            file.path.clone(),
            HighlightSlot {
                fingerprint,
                lines: None,
                _task: Some(task),
            },
        );
        None
    }

    // ---- rendering ----

    fn render_row(
        &mut self,
        ix: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(parsed) = &self.parsed else {
            return gpui::Empty.into_any_element();
        };
        let files = parsed.files.clone();
        let parsed_key = parsed.key.clone();
        let Some(row) = self.rows.get(ix).copied() else {
            return gpui::Empty.into_any_element();
        };
        let theme = Theme::of(cx).clone();
        match row {
            DiffRow::FileHeader { file } => {
                let Some(file_diff) = files.get(file as usize) else {
                    return gpui::Empty.into_any_element();
                };
                let fold = self.folds.get(&file_diff.path).copied().unwrap_or_default();
                self.render_file_header(file as usize, file_diff, &fold, &theme, cx)
            }
            DiffRow::Notice { file, notice } => files
                .get(file as usize)
                .and_then(|f| file_notices(f).into_iter().nth(notice as usize))
                .map(|text| notice_row(text, &theme))
                .unwrap_or_else(|| gpui::Empty.into_any_element()),
            DiffRow::HunkHeader { file, hunk } => files
                .get(file as usize)
                .and_then(|f| f.hunks.get(hunk as usize))
                .map(|h| hunk_header_row(&h.header, &theme))
                .unwrap_or_else(|| gpui::Empty.into_any_element()),
            DiffRow::Line {
                file,
                hunk,
                line,
                flat,
            } => {
                let Some(file_diff) = files.get(file as usize) else {
                    return gpui::Empty.into_any_element();
                };
                let highlight = self.request_highlight(file_diff, &parsed_key, cx);
                let Some(line) = file_diff
                    .hunks
                    .get(hunk as usize)
                    .and_then(|h| h.lines.get(line as usize))
                else {
                    return gpui::Empty.into_any_element();
                };
                let tokens = highlight
                    .as_ref()
                    .and_then(|lines| lines.get(flat as usize))
                    .map(|t| t.as_slice())
                    .unwrap_or(&[]);
                diff_line_row(line, tokens, &theme, gutter_width(file_diff))
            }
            DiffRow::BodyPad { .. } => div().w_full().h(px(BODY_BOTTOM_PAD)).into_any_element(),
            DiffRow::FoldingBody { file } => {
                let Some(file_diff) = files.get(file as usize) else {
                    return gpui::Empty.into_any_element();
                };
                let fold = self.folds.get(&file_diff.path).copied().unwrap_or_default();
                let highlight = self.request_highlight(file_diff, &parsed_key, cx);
                let (from, to) = (fold.from, fold.to);
                // Only the revealable slice is built — the tween never pays
                // for lines it cannot show.
                let cap = from.max(to).min(FOLD_TWEEN_MAX_PX);
                let body = render_file_body_upto(file_diff, highlight, &theme, cap);
                let clipped = div().w_full().overflow_hidden().child(body);
                if fold.animating() {
                    clipped
                        .with_animation(
                            SharedString::from(format!("fold-{}-{}", file_diff.path, fold.epoch)),
                            COLLAPSE.animation(),
                            move |el, t| el.h(px(motion::lerp(from, to, t))),
                        )
                        .into_any_element()
                } else {
                    // Post-tween, pre-settle: hold the full target height so
                    // the settle splice swaps rows without any reflow (the
                    // capped slice always covers what the viewport can see —
                    // tweens start from a clicked, on-screen header).
                    clipped.h(px(to)).into_any_element()
                }
            }
        }
    }

    fn render_file_header(
        &mut self,
        ix: usize,
        file: &FileDiff,
        fold: &FileFold,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let collapsed = fold.collapsed;
        let path = file.path.clone();
        let adds = file.additions;
        let dels = file.deletions;

        // Chevron (comet checkout-diff-sidebar): chevron-right closed,
        // chevron-down open; gpui divs have no rotation transform at the
        // pinned rev, so the glyph swap crossfades over the same 200 ms.
        let chevron_icon = if collapsed {
            crate::icons::ALT_ARROW_RIGHT
        } else {
            crate::icons::ALT_ARROW_DOWN
        };
        let chevron = div().flex_none().size(px(14.0)).child(
            crate::icons::icon(chevron_icon)
                .size(px(13.0))
                .text_color(theme.text_muted.opacity(0.7)),
        );
        let chevron: AnyElement = if fold.animating() {
            chevron
                .with_animation(
                    SharedString::from(format!("chev-{path}-{}", fold.epoch)),
                    CHEVRON.animation(),
                    |el, t| el.opacity(0.25 + 0.75 * t),
                )
                .into_any_element()
        } else {
            chevron.into_any_element()
        };

        // Header row: chevron + mono path (one quiet tone) + right-aligned
        // +N / −N counts on a slightly raised wash. The header carries the
        // section separator (the per-file wrapper it used to hang on is
        // gone — rows are flat now).
        div()
            .id(SharedString::from(format!("file-hdr-{ix}")))
            .w_full()
            .h(px(FILE_HEADER_HEIGHT))
            .when(ix > 0, |el| {
                el.border_t_1().border_color(crate::theme::hairline(0.04))
            })
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .px(px(Theme::SPACE_MD))
            .bg(crate::theme::ink(0.025))
            .cursor_pointer()
            .hover(|s| s.bg(crate::theme::ink(0.05)))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.toggle_fold(ix, cx);
                cx.notify();
            }))
            .child(chevron)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font_family(theme.font_mono.clone())
                    .text_size(px(12.0))
                    .text_color(theme.text_dim)
                    .child(SharedString::from(file.path.clone())),
            )
            .when(file.binary, |el| {
                el.child(
                    div()
                        .flex_none()
                        .text_size(px(10.0))
                        .text_color(theme.text_faint)
                        .child(SharedString::from("BIN")),
                )
            })
            .when(adds > 0 || !file.binary, |el| {
                el.child(
                    div()
                        .flex_none()
                        .font_family(theme.font_mono.clone())
                        .text_size(px(11.0))
                        .text_color(add_color(theme))
                        .child(SharedString::from(format!("+{adds}"))),
                )
            })
            .when(dels > 0 || !file.binary, |el| {
                el.child(
                    div()
                        .flex_none()
                        .font_family(theme.font_mono.clone())
                        .text_size(px(11.0))
                        .text_color(del_color(theme))
                        .child(SharedString::from(format!("−{dels}"))),
                )
            })
            .into_any_element()
    }

    fn render_header_strip(&self, theme: &Theme) -> Option<AnyElement> {
        let parsed = self.parsed.as_ref()?;
        Some(
            div()
                .flex_none()
                .h(px(36.0))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.0))
                .px(px(Theme::SPACE_LG))
                .border_b_1()
                .border_color(crate::theme::hairline(0.06))
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(theme.text_muted)
                        .child(SharedString::from(uncommitted_label(parsed.file_count))),
                )
                .child(
                    div()
                        .font_family(theme.font_mono.clone())
                        .text_size(px(11.0))
                        .text_color(add_color(theme))
                        .child(SharedString::from(format!("+{}", parsed.additions))),
                )
                .child(
                    div()
                        .font_family(theme.font_mono.clone())
                        .text_size(px(11.0))
                        .text_color(del_color(theme))
                        .child(SharedString::from(format!("−{}", parsed.deletions))),
                )
                .child(div().flex_1())
                .when(parsed.truncated, |el| {
                    el.child(
                        div()
                            .flex_none()
                            .text_size(px(10.0))
                            .px(px(6.0))
                            .py(px(2.0))
                            .rounded(px(4.0))
                            .bg(theme.warning.opacity(0.08))
                            .text_color(theme.warning.opacity(0.75))
                            .child(SharedString::from("Partial snapshot")),
                    )
                })
                .into_any_element(),
        )
    }
}

/// Green for additions — sampled from the reference diff (soft emerald).
fn add_color(theme: &Theme) -> gpui::Hsla {
    theme.diff_add // emerald-400
}

/// Red for deletions — softer than the theme danger, per the reference diff.
fn del_color(theme: &Theme) -> gpui::Hsla {
    theme.diff_del // red-400
}

/// Diff syntax palette — since round 9 the transcript's code blocks share the
/// same soft hues, so this simply delegates to [`render::token_color`].
fn diff_token_color(class: crate::markdown::highlight::TokenClass, theme: &Theme) -> gpui::Hsla {
    render::token_color(class, theme)
}

/// One notice row ("New file", "Binary file — contents not shown", …).
fn notice_row(notice: String, theme: &Theme) -> AnyElement {
    div()
        .h(px(NOTICE_HEIGHT))
        .w_full()
        .flex_none()
        .flex()
        .items_center()
        .px(px(Theme::SPACE_LG))
        .text_size(px(11.0))
        .text_color(theme.text_faint)
        .child(SharedString::from(notice))
        .into_any_element()
}

/// One `@@ … @@` hunk-header row on the bluish-grey wash.
fn hunk_header_row(header: &str, theme: &Theme) -> AnyElement {
    div()
        .h(px(HUNK_HEADER_HEIGHT))
        .w_full()
        .flex_none()
        .flex()
        .items_center()
        .px(px(Theme::SPACE_LG))
        .bg(theme.diff_hunk_bg)
        .font_family(theme.font_mono.clone())
        .text_size(px(11.0))
        .text_color(theme.text_faint)
        .child(SharedString::from(header.to_string()))
        .into_any_element()
}

/// One +/−/context/meta diff line: coloured accent bar, dual line-number
/// gutters (`gutter_px` wide — see [`gutter_width`]), marker column, and
/// paint-only syntax runs.
fn diff_line_row(line: &DiffLine, tokens: &[Token], theme: &Theme, gutter_px: f32) -> AnyElement {
    if line.kind == LineKind::Meta {
        return div()
            .h(px(DIFF_LINE_HEIGHT))
            .w_full()
            .flex_none()
            .flex()
            .items_center()
            .pl(px(ACCENT_BAR_WIDTH + 2.0 * gutter_px + MARKER_WIDTH + 12.0))
            .text_size(px(10.5))
            .text_color(theme.text_faint)
            .italic()
            .child(SharedString::from(line.text.clone()))
            .into_any_element();
    }

    // Row tints sampled from the reference: ~5–6% washes over the pane tone.
    let mut add_bg = add_color(theme);
    add_bg.a = 0.055;
    let mut del_bg = del_color(theme);
    del_bg.a = 0.055;

    let (marker, marker_color, row_bg, accent, number_color) = match line.kind {
        LineKind::Add => (
            "+",
            add_color(theme),
            Some(add_bg),
            Some(add_color(theme).opacity(0.55)),
            add_color(theme).opacity(0.9),
        ),
        LineKind::Del => (
            "−",
            del_color(theme),
            Some(del_bg),
            Some(del_color(theme).opacity(0.55)),
            del_color(theme).opacity(0.9),
        ),
        _ => (
            "·",
            theme.text_faint.opacity(0.5),
            None,
            None,
            theme.text_faint.opacity(0.8),
        ),
    };
    let gutter = |no: Option<u32>, color: gpui::Hsla| {
        div()
            .w(px(gutter_px))
            .flex_none()
            .font_family(theme.font_mono.clone())
            .text_size(px(11.0))
            .text_color(color)
            .flex()
            .justify_end()
            .pr(px(8.0))
            .child(SharedString::from(
                no.map(|n| n.to_string()).unwrap_or_default(),
            ))
    };
    let mono = font(theme.font_mono.clone());
    let runs = render::runs_with_palette(
        &line.text,
        tokens,
        &mono,
        theme.text.opacity(0.92),
        |class| diff_token_color(class, theme),
    );
    div()
        .h(px(DIFF_LINE_HEIGHT))
        .w_full()
        .flex_none()
        .flex()
        .flex_row()
        .items_center()
        .when_some(row_bg, |el, bg| el.bg(bg))
        // Accent bar: solid colour on +/− rows, invisible spacer on
        // context rows so columns always align.
        .child(
            div()
                .w(px(ACCENT_BAR_WIDTH))
                .h_full()
                .flex_none()
                .when_some(accent, |el, color| el.bg(color)),
        )
        .child(gutter(
            line.old_no,
            if line.kind == LineKind::Del {
                number_color
            } else {
                theme.text_faint.opacity(0.8)
            },
        ))
        .child(gutter(
            line.new_no,
            if line.kind == LineKind::Add {
                number_color
            } else {
                theme.text_faint.opacity(0.8)
            },
        ))
        .child(
            div()
                .w(px(MARKER_WIDTH))
                .flex_none()
                .flex()
                .justify_center()
                .text_size(px(DIFF_TEXT_SIZE))
                .text_color(marker_color)
                .font_family(theme.font_mono.clone())
                .child(SharedString::from(marker)),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .pl(px(12.0))
                .font_family(theme.font_mono.clone())
                .text_size(px(DIFF_TEXT_SIZE))
                .whitespace_nowrap()
                .child(gpui::StyledText::new(line.text.clone()).with_runs(runs)),
        )
        .into_any_element()
}

/// Build the expanded body rows that start above `max_px`; the fold tween's
/// stand-in never materializes lines its clip cannot reveal.
fn render_file_body_upto(
    file: &FileDiff,
    highlight: Option<Arc<Vec<Vec<Token>>>>,
    theme: &Theme,
    max_px: f32,
) -> AnyElement {
    let mut children: Vec<AnyElement> = Vec::new();
    let mut y = 0.0f32;
    let mut line_ix = 0usize;
    let gutter_px = gutter_width(file);

    'build: {
        for notice in file_notices(file) {
            if y >= max_px {
                break 'build;
            }
            children.push(notice_row(notice, theme));
            y += NOTICE_HEIGHT;
        }
        for hunk in &file.hunks {
            if y >= max_px {
                break 'build;
            }
            children.push(hunk_header_row(&hunk.header, theme));
            y += HUNK_HEADER_HEIGHT;
            for line in &hunk.lines {
                if y >= max_px {
                    break 'build;
                }
                let tokens = highlight
                    .as_ref()
                    .and_then(|lines| lines.get(line_ix))
                    .map(|t| t.as_slice())
                    .unwrap_or(&[]);
                children.push(diff_line_row(line, tokens, theme, gutter_px));
                y += DIFF_LINE_HEIGHT;
                line_ix += 1;
            }
        }
    }

    div()
        .flex()
        .flex_col()
        .pb(px(BODY_BOTTOM_PAD))
        .children(children)
        .into_any_element()
}

impl Render for Changes {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let resolved = self.resolved(cx);
        // With no session selected (new-chat canvas) there is nothing to
        // prepare — show the quiet empty state, not an endless spinner.
        let phase = if self.state.read(cx).selected_chat_row().is_none() {
            DiffPhase::Clean
        } else {
            diff_phase(resolved.as_ref())
        };
        let error = self.error.clone();

        let content: AnyElement = match phase {
            DiffPhase::Preparing => div()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(Theme::SPACE_SM))
                .child(crate::loaders::gradient_spinner(
                    "changes-preparing",
                    &theme,
                    3.0,
                    cx.entity_id(),
                    cx,
                ))
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(theme.text_faint)
                        .child(SharedString::from("Preparing diff…")),
                )
                .into_any_element(),
            DiffPhase::Clean => div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(12.0))
                .text_color(theme.text_faint)
                .child(SharedString::from("No uncommitted changes"))
                .into_any_element(),
            DiffPhase::List => {
                if self.parsed.is_some() {
                    div()
                        .flex_1()
                        .min_h_0()
                        .flex()
                        .flex_col()
                        .children(self.render_header_strip(&theme))
                        .child(
                            list(self.list.clone(), cx.processor(Self::render_row))
                                .flex_1()
                                .with_sizing_behavior(gpui::ListSizingBehavior::Auto),
                        )
                        .into_any_element()
                } else {
                    // Diff known, parse still running.
                    div()
                        .flex_1()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(crate::loaders::gradient_spinner(
                            "changes-parsing",
                            &theme,
                            3.0,
                            cx.entity_id(),
                            cx,
                        ))
                        .into_any_element()
                }
            }
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .when_some(error, |el, message| {
                el.child(
                    div()
                        .flex_none()
                        .px(px(Theme::SPACE_MD))
                        .py(px(4.0))
                        .border_b_1()
                        .border_color(theme.border)
                        .text_size(px(11.0))
                        .text_color(theme.warning)
                        .child(message),
                )
            })
            .child(content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    const PATCH: &str = "\
diff --git a/src/main.rs b/src/main.rs
index 111..222 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,4 +1,5 @@ fn main
 fn main() {
-    println!(\"old\");
+    println!(\"new\");
+    let x = 1;
 }
@@ -10,2 +11,2 @@
 // tail
-old_line
+new_line
diff --git a/added.txt b/added.txt
new file mode 100644
--- /dev/null
+++ b/added.txt
@@ -0,0 +1,2 @@
+first
+second
\\ No newline at end of file
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
--- a/gone.txt
+++ /dev/null
@@ -1,1 +0,0 @@
-bye
diff --git a/img.png b/img.png
new file mode 100644
Binary files /dev/null and b/img.png differ
diff --git a/old_name.rs b/new_name.rs
similarity index 90%
rename from old_name.rs
rename to new_name.rs
";

    #[test]
    fn parses_files_hunks_and_lines() {
        let files = parse_patch(PATCH);
        assert_eq!(files.len(), 5);

        let main = &files[0];
        assert_eq!(main.path, "src/main.rs");
        assert_eq!(main.status, FileStatus::Modified);
        assert_eq!(main.hunks.len(), 2);
        assert_eq!(main.additions, 3);
        assert_eq!(main.deletions, 2);
        let h0 = &main.hunks[0];
        assert_eq!(h0.header, "@@ -1,4 +1,5 @@ fn main");
        assert_eq!(h0.lines.len(), 5);
        assert_eq!(h0.lines[0].kind, LineKind::Context);
        assert_eq!(h0.lines[0].old_no, Some(1));
        assert_eq!(h0.lines[0].new_no, Some(1));
        assert_eq!(h0.lines[1].kind, LineKind::Del);
        assert_eq!(h0.lines[1].old_no, Some(2));
        assert_eq!(h0.lines[1].new_no, None);
        assert_eq!(h0.lines[2].kind, LineKind::Add);
        assert_eq!(h0.lines[2].new_no, Some(2));
        assert_eq!(h0.lines[3].kind, LineKind::Add);
        assert_eq!(h0.lines[3].new_no, Some(3));
        // Closing context line: numbering advanced past the add/del block.
        assert_eq!(h0.lines[4].old_no, Some(3));
        assert_eq!(h0.lines[4].new_no, Some(4));
        // Second hunk restarts numbering from its header.
        assert_eq!(main.hunks[1].lines[0].old_no, Some(10));
        assert_eq!(main.hunks[1].lines[0].new_no, Some(11));
    }

    #[test]
    fn detects_new_deleted_binary_and_renamed() {
        let files = parse_patch(PATCH);
        let added = &files[1];
        assert_eq!(added.status, FileStatus::Added);
        assert_eq!(added.additions, 2);
        // The no-newline marker rides as a Meta line.
        let last = added.hunks[0].lines.last().unwrap();
        assert_eq!(last.kind, LineKind::Meta);
        assert!(last.text.contains("No newline"));
        assert!(file_notices(added).iter().any(|n| n == "New file"));

        let deleted = &files[2];
        assert_eq!(deleted.status, FileStatus::Deleted);
        assert_eq!(deleted.deletions, 1);
        assert!(file_notices(deleted).iter().any(|n| n == "Deleted file"));

        let binary = &files[3];
        assert!(binary.binary);
        assert_eq!(binary.status, FileStatus::Added);
        assert!(binary.hunks.is_empty());
        assert!(file_notices(binary).iter().any(|n| n.contains("Binary")));

        let renamed = &files[4];
        assert_eq!(renamed.status, FileStatus::Renamed);
        assert_eq!(renamed.path, "new_name.rs");
        assert_eq!(renamed.old_path.as_deref(), Some("old_name.rs"));
        assert!(
            file_notices(renamed)
                .iter()
                .any(|n| n.contains("old_name.rs"))
        );
    }

    #[test]
    fn empty_and_garbage_patches_parse_to_nothing() {
        assert!(parse_patch("").is_empty());
        assert!(parse_patch("not a diff\nat all\n").is_empty());
        // Truncated mid-hunk: keeps what parsed.
        let files = parse_patch("diff --git a/x b/x\n@@ -1,9 +1,9 @@\n ctx\n+add");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].hunks[0].lines.len(), 2);
        assert_eq!(files[0].additions, 1);
    }

    #[test]
    fn quoted_and_spaced_paths() {
        let (old, new) = parse_git_paths("a/simple.rs b/simple.rs");
        assert_eq!((old.as_str(), new.as_str()), ("simple.rs", "simple.rs"));
        let (old, new) = parse_git_paths("\"a/with space.rs\" \"b/with space.rs\"");
        assert_eq!(old, "with space.rs");
        assert_eq!(new, "with space.rs");
    }

    #[test]
    fn hunk_headers_parse_with_and_without_counts() {
        assert_eq!(parse_hunk_header("@@ -1,4 +2,5 @@"), Some((1, 2)));
        assert_eq!(parse_hunk_header("@@ -7 +9 @@ fn ctx"), Some((7, 9)));
        assert_eq!(parse_hunk_header("@@ garbage"), None);
    }

    #[test]
    fn rows_flatten_to_line_granularity() {
        let files = parse_patch(PATCH);
        let (rows, ranges) = flatten_rows(&files, |_| false);
        assert_eq!(ranges.len(), files.len());
        // Every file's span starts with its header…
        for (ix, range) in ranges.iter().enumerate() {
            assert_eq!(rows[range.start], DiffRow::FileHeader { file: ix as u32 });
            // …and spans exactly header + analytic body rows.
            assert_eq!(range.len(), 1 + body_row_count(&files[ix]));
        }
        // Spans tile the whole row vec.
        assert_eq!(ranges.last().unwrap().end, rows.len());

        // src/main.rs: header, 2 hunk headers, 8 lines, pad.
        let main_rows = &rows[ranges[0].clone()];
        assert_eq!(main_rows.len(), 1 + 2 + 8 + 1);
        assert_eq!(main_rows[1], DiffRow::HunkHeader { file: 0, hunk: 0 });
        // Flat line indices run across hunks (they key the highlight slot).
        let flats: Vec<u32> = main_rows
            .iter()
            .filter_map(|r| match r {
                DiffRow::Line { flat, .. } => Some(*flat),
                _ => None,
            })
            .collect();
        assert_eq!(flats, (0..8).collect::<Vec<u32>>());
        assert_eq!(*main_rows.last().unwrap(), DiffRow::BodyPad { file: 0 });

        // A collapsed file contributes its header row only.
        let (rows, ranges) = flatten_rows(&files, |ix| ix == 0);
        assert_eq!(ranges[0].len(), 1);
        assert_eq!(rows[ranges[1].start], DiffRow::FileHeader { file: 1 });

        // Notices lead the body: the added file carries "New file".
        let added_rows = &rows[ranges[1].clone()];
        assert_eq!(added_rows[1], DiffRow::Notice { file: 1, notice: 0 });
    }

    #[test]
    fn truncate_caps_lines_and_appends_notice() {
        let mut file = parse_patch(PATCH).remove(0); // 2 hunks, 8 lines
        let untouched = file.clone();
        truncate_file_lines(&mut file, 10);
        assert_eq!(file, untouched, "under the cap: untouched");

        truncate_file_lines(&mut file, 6);
        let lines: usize = file.hunks.iter().map(|h| h.lines.len()).sum();
        assert_eq!(lines, 6);
        assert_eq!(file.hunks.len(), 2);
        assert!(
            file_notices(&file)
                .iter()
                .any(|n| n.contains("first 6 of 8 lines"))
        );
        // body_height stays consistent with what actually renders.
        assert_eq!(
            body_height(&file),
            NOTICE_HEIGHT + 2.0 * HUNK_HEADER_HEIGHT + 6.0 * DIFF_LINE_HEIGHT + BODY_BOTTOM_PAD
        );

        // A cap below the first hunk's length drops later hunks entirely.
        let mut file = parse_patch(PATCH).remove(0);
        truncate_file_lines(&mut file, 3);
        assert_eq!(file.hunks.len(), 1);
        assert_eq!(file.hunks[0].lines.len(), 3);
    }

    #[test]
    fn gutters_fit_the_largest_line_number() {
        let files = parse_patch(PATCH);
        // src/main.rs second hunk ends at old 11 / new 12.
        assert_eq!(files[0].max_line, 12);
        assert_eq!(gutter_width(&files[0]), GUTTER_WIDTH);

        // Every digit count keeps ≥6px clear of the accent bar on the left
        // of the number (digits×6.6 + 8px right pad + 6px gap), and the
        // column never shrinks below the classic 36px.
        let mut file = files[0].clone();
        for digits in 1..=7u32 {
            file.max_line = 10u32.pow(digits) - 1;
            let w = gutter_width(&file);
            assert!(w >= GUTTER_WIDTH);
            let left_gap = w - (digits as f32 * 6.6 + 8.0);
            assert!(
                left_gap >= 6.0,
                "{digits} digits: left gap {left_gap} < 6px"
            );
        }
        // 4 digits outgrow the classic column now (the old formula left
        // them 1.6px off the bar — visually touching).
        file.max_line = 9999;
        assert!(gutter_width(&file) > GUTTER_WIDTH);
        file.max_line = 27404;
        assert!(
            gutter_width(&file)
                > gutter_width(&{
                    let mut f = file.clone();
                    f.max_line = 9999;
                    f
                })
        );

        // Truncation refits the gutter to what actually renders: the first
        // 3 lines are ctx(1,1) / del(2,·) / add(·,2) — max line 2.
        let mut file = files[0].clone();
        truncate_file_lines(&mut file, 3);
        assert_eq!(file.max_line, 2);
    }

    #[test]
    fn body_height_is_analytic() {
        let files = parse_patch(PATCH);
        let main = &files[0];
        let lines: usize = main.hunks.iter().map(|h| h.lines.len()).sum();
        assert_eq!(
            body_height(main),
            2.0 * HUNK_HEADER_HEIGHT + lines as f32 * DIFF_LINE_HEIGHT + BODY_BOTTOM_PAD
        );
        // Notices add height (added file: 1 notice + meta line inside hunk).
        let added = &files[1];
        assert_eq!(
            body_height(added),
            NOTICE_HEIGHT + HUNK_HEADER_HEIGHT + 3.0 * DIFF_LINE_HEIGHT + BODY_BOTTOM_PAD
        );
    }

    fn diff(checkout: &str, device: &str, cwd: &str, patch: &str) -> CheckoutDiff {
        CheckoutDiff {
            checkout_id: checkout.into(),
            device_id: device.into(),
            cwd: cwd.into(),
            patch: patch.into(),
            files: Vec::new(),
            additions: 0,
            deletions: 0,
            truncated: false,
            checksum: format!("sum-{}", patch.len()),
            updated_at: Utc::now(),
        }
    }

    fn chat(checkout: Option<&str>, device: &str, cwd: Option<&str>) -> Chat {
        Chat {
            id: "c1".into(),
            device_id: device.into(),
            title: None,
            archived: false,
            cwd: cwd.map(Into::into),
            branch: None,
            checkout_id: checkout.map(Into::into),
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

    #[test]
    fn diff_resolution_prefers_checkout_id_then_cwd() {
        let diffs = vec![
            diff("co-1", "dev-a", "/repo/one", "x"),
            diff("co-2", "dev-b", "/repo/two", "y"),
        ];
        // checkout_id match wins even when cwd points elsewhere.
        let c = chat(Some("co-2"), "dev-a", Some("/repo/one"));
        assert_eq!(resolve_diff(&diffs, &c).unwrap().checkout_id, "co-2");
        // Unknown checkout falls back to device+cwd.
        let c = chat(Some("co-9"), "dev-a", Some("/repo/one"));
        assert_eq!(resolve_diff(&diffs, &c).unwrap().checkout_id, "co-1");
        // Wrong device still matches by cwd alone.
        let c = chat(None, "dev-z", Some("/repo/two"));
        assert_eq!(resolve_diff(&diffs, &c).unwrap().checkout_id, "co-2");
        // Nothing to go on.
        let c = chat(None, "dev-a", None);
        assert!(resolve_diff(&diffs, &c).is_none());
        let c = chat(None, "dev-a", Some("/elsewhere"));
        assert!(resolve_diff(&diffs, &c).is_none());
    }

    #[test]
    fn phases() {
        assert_eq!(diff_phase(None), DiffPhase::Preparing);
        let clean = diff("co", "d", "/w", "  \n");
        assert_eq!(diff_phase(Some(&clean)), DiffPhase::Clean);
        let full = diff("co", "d", "/w", "diff --git a/x b/x\n");
        assert_eq!(diff_phase(Some(&full)), DiffPhase::List);
        // Engine may report files without patch text (truncation edge).
        let mut summarized = diff("co", "d", "/w", "");
        summarized.files.push(comet_proto::DiffFileSummary {
            path: "x".into(),
            old_path: None,
            status: "modified".into(),
            additions: 1,
            deletions: 0,
            binary: false,
        });
        assert_eq!(diff_phase(Some(&summarized)), DiffPhase::List);
    }

    #[test]
    fn header_label_pluralizes() {
        assert_eq!(uncommitted_label(0), "0 Uncommitted changes");
        assert_eq!(uncommitted_label(1), "1 Uncommitted change");
        assert_eq!(uncommitted_label(4), "4 Uncommitted changes");
    }

    #[test]
    fn diff_frames_replace_lists_and_upsert_singles() {
        let mut diffs = Vec::new();
        let one = diff("co-1", "d", "/w", "p1");
        // Single frame inserts.
        assert!(apply_diff_frame(
            &mut diffs,
            serde_json::to_value(&one).unwrap()
        ));
        assert_eq!(diffs.len(), 1);
        // Identical frame is a no-op.
        assert!(!apply_diff_frame(
            &mut diffs,
            serde_json::to_value(&one).unwrap()
        ));
        // Same checkout upserts in place.
        let mut updated = one.clone();
        updated.patch = "p2".into();
        assert!(apply_diff_frame(
            &mut diffs,
            serde_json::to_value(&updated).unwrap()
        ));
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].patch, "p2");
        // List frame replaces wholesale.
        let two = diff("co-2", "d", "/x", "q");
        assert!(apply_diff_frame(
            &mut diffs,
            serde_json::to_value(vec![two.clone()]).unwrap()
        ));
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].checkout_id, "co-2");
        // Malformed frames change nothing.
        assert!(!apply_diff_frame(
            &mut diffs,
            serde_json::json!({"nope": true})
        ));
        assert_eq!(diffs[0].checkout_id, "co-2");
    }

    #[test]
    fn langs_resolve_from_paths() {
        assert_eq!(lang_for_path("src/main.rs"), Some(Lang::Rust));
        assert_eq!(lang_for_path("a/b/app.tsx"), Some(Lang::Tsx));
        assert_eq!(lang_for_path("Cargo.toml"), Some(Lang::Toml));
        assert_eq!(lang_for_path("script.sh"), Some(Lang::Bash));
        assert_eq!(lang_for_path("README"), None);
        assert_eq!(lang_for_path("img.png"), None);
    }
}
