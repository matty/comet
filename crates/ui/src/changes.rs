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
//! - syntax highlighting paints a tree-sitter excerpt immediately, then
//!   promotes to checksum-bound complete old/new documents when the owning
//!   engine returns them; syntax changes paint only, never layout.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    AnyElement, App, Context, Entity, Focusable as _, ListAlignment, ListState, SharedString,
    Subscription, Task, Window, div, font, list, prelude::*, px,
};

use comet_proto::{Chat, CheckoutDiff};
use comet_rpc::methods;

use crate::comments::{self, CommentSide, DiffComment};
use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::markdown::render;
use crate::motion::{self, AnimationExt as _, CHEVRON, COLLAPSE};
use crate::state::{AppState, ServerClient};
use crate::theme::Theme;
use comet_syntax::LanguageId as Lang;

// ---------------------------------------------------------------------------
// Layout numbers (analytic — they drive the fold tween)
// ---------------------------------------------------------------------------

pub const FILE_HEADER_HEIGHT: f32 = 36.0;
const STICKY_FILE_HEADER_BLUR: f32 = 16.0;
/// Coverage of the theme's content-plane tint over the sticky header blur.
/// Light needs substantially more coverage: dark text is much more vulnerable
/// to rows ghosting through the blur than light text is on a dark tint.
const STICKY_FILE_HEADER_TINT_ALPHA_DARK: f32 = 0.40;
const STICKY_FILE_HEADER_TINT_ALPHA_LIGHT: f32 = 0.85;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SourceSide {
    Old,
    New,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SourceLineRef {
    pub side: SourceSide,
    /// One-based source line number. Conversion to a document index is checked.
    pub line_number: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DiffHighlights {
    pub old: Option<Arc<comet_syntax::HighlightedDocument>>,
    pub new: Option<Arc<comet_syntax::HighlightedDocument>>,
}

impl DiffHighlights {
    pub fn source_ref(&self, line: &DiffLine) -> Option<SourceLineRef> {
        let source_ref = |side, line_number: u32| {
            usize::try_from(line_number)
                .ok()
                .map(|line_number| SourceLineRef { side, line_number })
        };
        match line.kind {
            LineKind::Del => line
                .old_no
                .and_then(|line_number| source_ref(SourceSide::Old, line_number)),
            LineKind::Add => line
                .new_no
                .and_then(|line_number| source_ref(SourceSide::New, line_number)),
            LineKind::Context => line
                .new_no
                .filter(|_| self.new.is_some())
                .and_then(|line_number| source_ref(SourceSide::New, line_number))
                .or_else(|| {
                    line.old_no
                        .and_then(|line_number| source_ref(SourceSide::Old, line_number))
                }),
            LineKind::Meta => None,
        }
    }

    pub fn spans(&self, line: &DiffLine) -> &[comet_syntax::HighlightSpan] {
        let Some(source_ref) = self.source_ref(line) else {
            return &[];
        };
        let Some(line_index) = source_ref.line_number.checked_sub(1) else {
            return &[];
        };
        match source_ref.side {
            SourceSide::Old => self.old.as_deref(),
            SourceSide::New => self.new.as_deref(),
        }
        .and_then(|document| document.lines.get(line_index))
        .map(Vec::as_slice)
        .unwrap_or(&[])
    }
}

/// Reject a complete pair unless every source-backed visible diff line agrees.
/// A rejected pair leaves callers' existing excerpt or plain rendering intact.
#[allow(dead_code)] // The local sidecar task supplies the transcript caller.
pub(crate) fn sources_match_diff(
    file: &FileDiff,
    old_text: Option<&str>,
    new_text: Option<&str>,
) -> bool {
    sources_match_diff_with(file, old_text, new_text, split_source_lines)
}

/// Split exactly as `similar::TextDiff::from_lines` does, but omit each line
/// terminator because visible diff rows omit it too. `str::lines` recognizes
/// LF and CRLF but not lone CR, which would make a valid source pair fail the
/// complete-source preflight.
fn split_source_lines(source: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut line_start = 0;
    let mut chars = source.char_indices().peekable();
    while let Some((at, character)) = chars.next() {
        let mut terminator_end = at + character.len_utf8();
        if character == '\r' {
            if let Some(&(next, '\n')) = chars.peek() {
                chars.next();
                terminator_end = next + '\n'.len_utf8();
            }
        } else if character != '\n' {
            continue;
        }
        lines.push(&source[line_start..at]);
        line_start = terminator_end;
    }
    if line_start < source.len() {
        lines.push(&source[line_start..]);
    }
    lines
}

fn sources_match_diff_with<'a>(
    file: &FileDiff,
    old_text: Option<&'a str>,
    new_text: Option<&'a str>,
    mut split_lines: impl FnMut(&'a str) -> Vec<&'a str>,
) -> bool {
    fn line_at<'a>(lines: Option<&'a [&'a str]>, line_number: u32) -> Option<&'a str> {
        usize::try_from(line_number)
            .ok()
            .and_then(|line_number| line_number.checked_sub(1))
            .and_then(|line_index| lines.and_then(|lines| lines.get(line_index).copied()))
    }
    let old_lines = old_text.map(&mut split_lines);
    let new_lines = new_text.map(split_lines);
    file.hunks
        .iter()
        .flat_map(|hunk| &hunk.lines)
        .all(|line| match line.kind {
            LineKind::Del => {
                line.old_no
                    .and_then(|number| line_at(old_lines.as_deref(), number))
                    == Some(line.text.as_str())
            }
            LineKind::Add => {
                line.new_no
                    .and_then(|number| line_at(new_lines.as_deref(), number))
                    == Some(line.text.as_str())
            }
            LineKind::Context => {
                let selected = new_lines.as_deref().or(old_lines.as_deref());
                let number = if new_lines.is_some() {
                    line.new_no
                } else {
                    line.old_no
                };
                number.and_then(|number| line_at(selected, number)) == Some(line.text.as_str())
            }
            LineKind::Meta => true,
        })
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
    body_height_with(file, &[], None)
}

pub fn body_height_with(
    file: &FileDiff,
    comments: &[DiffComment],
    draft: Option<(CommentSide, u32)>,
) -> f32 {
    body_rows(0, file, comments, draft)
        .iter()
        .map(|row| row.height(comments))
        .sum()
}

/// A deletion only exists in the pre-change file; everything else is cited
/// against the post-change file, which is what the agent edits.
pub fn line_anchor(line: &DiffLine) -> Option<(CommentSide, u32)> {
    match line.kind {
        LineKind::Meta => None,
        LineKind::Del => line.old_no.map(|no| (CommentSide::Old, no)),
        _ => line.new_no.map(|no| (CommentSide::New, no)),
    }
}

pub fn draft_belongs_to(
    owner: &comet_proto::ServerRef,
    selected: Option<&comet_proto::ServerRef>,
) -> bool {
    selected == Some(owner)
}

fn discard_stale_draft<T>(
    draft: &mut Option<T>,
    owner: Option<&comet_proto::ServerRef>,
    selected: Option<&comet_proto::ServerRef>,
) -> bool {
    if owner.is_some_and(|owner| !draft_belongs_to(owner, selected)) {
        *draft = None;
        true
    } else {
        false
    }
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

fn comment_state_key(comments: &[DiffComment], draft: Option<&(String, CommentSide, u32)>) -> u64 {
    let mut parts: Vec<String> = comments.iter().map(|comment| comment.id.clone()).collect();
    if let Some((path, side, line)) = draft {
        parts.push(format!("draft:{path}:{}:{line}", side.tag()));
    }
    let refs: Vec<&str> = parts.iter().map(String::as_str).collect();
    hash64(&refs)
}

fn hash64(parts: &[&str]) -> u64 {
    let mut hasher = std::hash::DefaultHasher::new();
    for p in parts {
        p.hash(&mut hasher);
    }
    hasher.finish()
}

const MAX_EXCERPT_SOURCE_LINES: usize = 200_000;

fn excerpt_side(
    file: &FileDiff,
    side: SourceSide,
    language: Lang,
    path: &str,
) -> Option<Arc<comet_syntax::HighlightedDocument>> {
    let max_line = file
        .hunks
        .iter()
        .flat_map(|hunk| &hunk.lines)
        .filter_map(|line| match side {
            SourceSide::Old => line.old_no,
            SourceSide::New => line.new_no,
        })
        .max()
        .unwrap_or(0) as usize;
    if max_line > MAX_EXCERPT_SOURCE_LINES {
        return None;
    }
    let mut lines = vec![Vec::new(); max_line];
    for hunk in &file.hunks {
        let visible = hunk
            .lines
            .iter()
            .filter_map(|line| {
                let number = match side {
                    SourceSide::Old => line.old_no,
                    SourceSide::New => line.new_no,
                }?;
                (line.kind != LineKind::Meta).then_some((number, line.text.as_str()))
            })
            .collect::<Vec<_>>();
        if visible.is_empty() {
            continue;
        }
        let source = visible
            .iter()
            .map(|(_, text)| *text)
            .collect::<Vec<_>>()
            .join("\n");
        let document = comet_syntax::highlight(comet_syntax::HighlightRequest {
            source: &source,
            path: Some(path),
            fence_tag: None,
        })
        .ok()?;
        for ((number, _), spans) in visible.into_iter().zip(document.lines) {
            lines[number as usize - 1] = spans;
        }
    }
    Some(Arc::new(comet_syntax::HighlightedDocument {
        language,
        lines,
    }))
}

fn excerpt_highlights(file: &FileDiff, language: Lang) -> Option<DiffHighlights> {
    if !comet_syntax::supports_language(language) {
        return None;
    }
    let old = if file.status == FileStatus::Added {
        None
    } else {
        Some(excerpt_side(file, SourceSide::Old, language, &file.path)?)
    };
    let new = if file.status == FileStatus::Deleted {
        None
    } else {
        Some(excerpt_side(file, SourceSide::New, language, &file.path)?)
    };
    Some(DiffHighlights { old, new })
}

fn full_highlights(
    file: &FileDiff,
    language: Lang,
    response: &comet_proto::CheckoutFileDiffText,
) -> Option<DiffHighlights> {
    if response.stale
        || response.binary
        || response.truncated
        || !sources_match_diff(
            file,
            response.old_text.as_deref(),
            response.new_text.as_deref(),
        )
    {
        return None;
    }
    let parse = |source: &str, path: &str| {
        comet_syntax::highlight(comet_syntax::HighlightRequest {
            source,
            path: Some(path),
            fence_tag: None,
        })
        .ok()
        .map(Arc::new)
    };
    let old = match response.old_text.as_deref() {
        Some(source) => Some(parse(source, &file.path)?),
        None => None,
    };
    let new = match response.new_text.as_deref() {
        Some(source) => Some(parse(source, &file.path)?),
        None => None,
    };
    if old.is_none() && new.is_none() && comet_syntax::supports_language(language) {
        return None;
    }
    Some(DiffHighlights { old, new })
}

fn decode_checkout_file_diff_text_reply(
    value: serde_json::Value,
) -> Result<comet_proto::CheckoutFileDiffText, serde_json::Error> {
    serde_json::from_value(value)
}

fn full_highlights_for_fetch(
    file: &FileDiff,
    language: Lang,
    expected_checksum: &str,
    result: Result<comet_proto::CheckoutFileDiffText, comet_rpc::RpcError>,
) -> Option<Arc<DiffHighlights>> {
    let response = match result {
        Ok(response) => response,
        Err(error) => {
            tracing::debug!(?error, "checkout diff source enrichment unavailable");
            return None;
        }
    };
    if response.diff_checksum != expected_checksum {
        tracing::debug!(
            expected_checksum,
            actual_checksum = response.diff_checksum,
            "checkout diff source reply belongs to another snapshot"
        );
        return None;
    }
    full_highlights(file, language, &response).map(Arc::new)
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
    /// `card` indexes the file's own staged-comment slice, in staged order.
    CommentCard {
        file: u32,
        card: u32,
    },
    CommentDraft {
        file: u32,
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

impl DiffRow {
    /// `FoldingBody` is height-animated, so it reports 0 and never lands in a
    /// height sum.
    fn height(self, comments: &[DiffComment]) -> f32 {
        match self {
            DiffRow::FileHeader { .. } => FILE_HEADER_HEIGHT,
            DiffRow::Notice { .. } => NOTICE_HEIGHT,
            DiffRow::HunkHeader { .. } => HUNK_HEADER_HEIGHT,
            DiffRow::Line { .. } => DIFF_LINE_HEIGHT,
            DiffRow::CommentCard { card, .. } => comments
                .get(card as usize)
                .map(|comment| comments::card_height(&comment.body))
                .unwrap_or(0.0),
            DiffRow::CommentDraft { .. } => comments::DRAFT_CARD_HEIGHT,
            DiffRow::BodyPad { .. } => BODY_BOTTOM_PAD,
            DiffRow::FoldingBody { .. } => 0.0,
        }
    }
}

/// Capacity hint only — comment cards are not counted.
pub fn body_row_count(file: &FileDiff) -> usize {
    let lines: usize = file.hunks.iter().map(|h| h.lines.len()).sum();
    file_notices(file).len() + file.hunks.len() + lines + 1
}

pub fn body_rows(
    file_ix: u32,
    file: &FileDiff,
    comments: &[DiffComment],
    draft: Option<(CommentSide, u32)>,
) -> Vec<DiffRow> {
    fn push_cards(
        rows: &mut Vec<DiffRow>,
        file_ix: u32,
        comments: &[DiffComment],
        draft: Option<(CommentSide, u32)>,
        anchors: &[Option<(CommentSide, u32)>],
    ) {
        for anchor in anchors.iter().flatten() {
            for (ix, comment) in comments.iter().enumerate() {
                if comment.anchor() == *anchor {
                    rows.push(DiffRow::CommentCard {
                        file: file_ix,
                        card: ix as u32,
                    });
                }
            }
            if draft == Some(*anchor) {
                rows.push(DiffRow::CommentDraft { file: file_ix });
            }
        }
    }

    let mut rows = Vec::with_capacity(body_row_count(file));
    for notice in 0..file_notices(file).len() {
        rows.push(DiffRow::Notice {
            file: file_ix,
            notice: notice as u32,
        });
    }
    let visible_anchors: Vec<(CommentSide, u32)> = file
        .hunks
        .iter()
        .flat_map(|hunk| &hunk.lines)
        .filter_map(line_anchor)
        .collect();
    let orphan_comments: Vec<u32> = comments
        .iter()
        .enumerate()
        .filter(|(_, comment)| !visible_anchors.contains(&comment.anchor()))
        .map(|(ix, _)| ix as u32)
        .collect();
    rows.extend(
        orphan_comments
            .into_iter()
            .map(|card| DiffRow::CommentCard {
                file: file_ix,
                card,
            }),
    );
    if draft.is_some_and(|anchor| !visible_anchors.contains(&anchor)) {
        rows.push(DiffRow::CommentDraft { file: file_ix });
    }
    let mut hunk_flat = 0u32;
    for (hunk_ix, hunk) in file.hunks.iter().enumerate() {
        rows.push(DiffRow::HunkHeader {
            file: file_ix,
            hunk: hunk_ix as u32,
        });
        for (line_ix, line) in hunk.lines.iter().enumerate() {
            rows.push(DiffRow::Line {
                file: file_ix,
                hunk: hunk_ix as u32,
                line: line_ix as u32,
                flat: hunk_flat + line_ix as u32,
            });
            push_cards(&mut rows, file_ix, comments, draft, &[line_anchor(line)]);
        }
        hunk_flat += hunk.lines.len() as u32;
    }
    rows.push(DiffRow::BodyPad { file: file_ix });
    rows
}

/// Flatten all files into rows + each file's row span (header at
/// `range.start`, body rows after it). `collapsed(ix)` folds a file to just
/// its header. `comments` is the whole staged set; each file takes its own
/// path's slice.
pub fn flatten_rows(
    files: &[FileDiff],
    comments: &[DiffComment],
    draft: Option<(&str, CommentSide, u32)>,
    mut collapsed: impl FnMut(usize) -> bool,
) -> (Vec<DiffRow>, Vec<std::ops::Range<usize>>) {
    let mut rows = Vec::new();
    let mut ranges = Vec::with_capacity(files.len());
    for (ix, file) in files.iter().enumerate() {
        let start = rows.len();
        rows.push(DiffRow::FileHeader { file: ix as u32 });
        if !collapsed(ix) {
            let file_comments: Vec<DiffComment> = comments
                .iter()
                .filter(|comment| comment.path == file.path)
                .cloned()
                .collect();
            let file_draft = draft
                .filter(|(path, _, _)| *path == file.path)
                .map(|(_, side, line)| (side, line));
            rows.extend(body_rows(ix as u32, file, &file_comments, file_draft));
        }
        ranges.push(start..rows.len());
    }
    (rows, ranges)
}

/// The file header that should remain visible for a logical list position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StickyFileHeader {
    file_ix: usize,
    header_row: usize,
    next_header_row: Option<usize>,
}

/// Resolve a sticky file header from the current flattened row ranges.
///
/// This remains independent of the rendered list so folds and diff resets
/// cannot leave a second, stale active-file state behind.
fn sticky_file_header(
    row_ranges: &[std::ops::Range<usize>],
    item_ix: usize,
    offset_in_item: f32,
) -> Option<StickyFileHeader> {
    let file_ix = row_ranges
        .partition_point(|range| range.start <= item_ix)
        .checked_sub(1)?;
    let range = row_ranges.get(file_ix)?;

    // A reset can briefly leave ListState pointing past the replacement
    // model. Treat that frame as having no sticky header.
    if !range.contains(&item_ix) || (item_ix == range.start && offset_in_item <= 0.0) {
        return None;
    }

    Some(StickyFileHeader {
        file_ix,
        header_row: range.start,
        next_header_row: row_ranges.get(file_ix + 1).map(|range| range.start),
    })
}

/// Offset a sticky header upward as the next file header enters its slot.
fn sticky_header_push_offset(next_header_y: Option<f32>) -> f32 {
    next_header_y
        .map(|y| (y - FILE_HEADER_HEIGHT).min(0.0))
        .unwrap_or(0.0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileHeaderPresentation {
    Row,
    Sticky,
}

impl FileHeaderPresentation {
    fn key_prefix(self) -> &'static str {
        match self {
            Self::Row => "file-hdr",
            Self::Sticky => "sticky-file-hdr",
        }
    }

    fn element_id(self, file_ix: usize) -> SharedString {
        let prefix = self.key_prefix();
        SharedString::from(format!("{prefix}-{file_ix}"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct StickyFileHeaderPaint {
    rest_bg: gpui::Hsla,
    hover_bg: gpui::Hsla,
    border: gpui::Hsla,
    frost_tint: Option<gpui::Hsla>,
}

/// Resolve the sticky header from the diff's content plane, not the elevated
/// overlay plane used by menus and popovers.
fn sticky_file_header_paint(theme: &Theme) -> StickyFileHeaderPaint {
    sticky_file_header_paint_for(theme, theme.is_glass())
}

/// The paint decision with the glass question handed in. Split out because
/// [`Theme::is_glass`] is `false` at compile time off macOS ([`Theme::GLASS_ALPHA`]
/// is platform-wide), so a test calling the wrapper could only ever reach the
/// opaque arm on the machines that run the gate.
fn sticky_file_header_paint_for(theme: &Theme, glass: bool) -> StickyFileHeaderPaint {
    if glass {
        let tint_alpha = match theme.appearance {
            crate::theme::Appearance::Dark => STICKY_FILE_HEADER_TINT_ALPHA_DARK,
            crate::theme::Appearance::Light => STICKY_FILE_HEADER_TINT_ALPHA_LIGHT,
        };
        StickyFileHeaderPaint {
            rest_bg: theme.ink(0.025),
            hover_bg: theme.glass_hover(),
            border: theme.border,
            frost_tint: Some(theme.bg.opacity(tint_alpha)),
        }
    } else {
        StickyFileHeaderPaint {
            rest_bg: crate::theme::flatten(theme.ink(0.025), theme.bg),
            hover_bg: crate::theme::flatten(theme.element_hover, theme.bg),
            border: theme.border,
            frost_tint: None,
        }
    }
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
    state: DiffHighlightState,
    _excerpt_task: Option<Task<()>>,
    _fetch_task: Option<Task<()>>,
}

enum DiffHighlightState {
    Pending,
    Ready(Arc<DiffHighlights>),
    Excerpt(Arc<DiffHighlights>),
    Plain,
}

fn promote_highlights_if_current(
    current_fingerprint: u64,
    requested_fingerprint: u64,
    state: &mut DiffHighlightState,
    highlights: Option<Arc<DiffHighlights>>,
) -> bool {
    let Some(highlights) = highlights else {
        return false;
    };
    if current_fingerprint != requested_fingerprint {
        return false;
    }
    *state = DiffHighlightState::Ready(highlights);
    true
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HoverRow {
    path: String,
    side: CommentSide,
    line: u32,
}

struct CommentDraft {
    key: comet_proto::ServerRef,
    path: String,
    /// The file's pre-rename path, when it moved — carried onto the comment so
    /// an `Old`-side citation names the file that line lives in.
    old_path: Option<String>,
    side: CommentSide,
    line: u32,
    input: Entity<ComposerInput>,
    _events: Subscription,
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
    /// The one open comment draft, if any. Only ever one — a second `+` click
    /// moves the card rather than stacking two half-written notes.
    draft: Option<CommentDraft>,
    hover: Option<HoverRow>,
    comment_key: u64,
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
            draft: None,
            hover: None,
            comment_key: 0,
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
        self.discard_stale_draft(cx);
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
            self.sync_comment_rows(cx);
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
                let staged = changes.staged_comments(cx);
                let draft = changes.draft_anchor();
                let (rows, ranges) = flatten_rows(
                    &files,
                    &staged,
                    draft
                        .as_ref()
                        .map(|(path, side, line)| (path.as_str(), *side, *line)),
                    |_| false,
                );
                changes.comment_key = comment_state_key(&staged, draft.as_ref());
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
        // Only splice the rows that moved: `ListState::splice` clamps the
        // scroll anchor to the range start when the anchored row is inside it,
        // so replacing a whole body jumped the pane to the top of the file.
        let (prefix, suffix) = {
            let old = &self.rows[body.clone()];
            let prefix = old
                .iter()
                .zip(&new_body)
                .take_while(|(a, b)| a == b)
                .count();
            let suffix = old[prefix..]
                .iter()
                .rev()
                .zip(new_body[prefix..].iter().rev())
                .take_while(|(a, b)| a == b)
                .count();
            (prefix, suffix)
        };
        if delta == 0 && prefix + suffix >= body.len() {
            return;
        }
        let changed = body.start + prefix..body.end - suffix;
        let mid: Vec<DiffRow> = new_body[prefix..new_body.len() - suffix].to_vec();
        self.list.splice(changed.clone(), mid.len());
        self.rows.splice(changed, mid);
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
        let expanded_height = body_height_with(
            file,
            &self.comments_for(&file.path, cx),
            self.draft_anchor_in(&file.path),
        );
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
                body_rows(
                    file_ix as u32,
                    file,
                    &self.comments_for(&file.path, cx),
                    self.draft_anchor_in(&file.path),
                )
            };
            self.replace_file_body(file_ix, body);
        }
        cx.notify();
        pending
    }

    // ---- diff comments ----

    /// The comments staged for the chat this pane is showing. Cloned because
    /// rendering borrows `self` mutably a moment later.
    fn staged_comments(&self, cx: &App) -> Vec<DiffComment> {
        let state = self.state.read(cx);
        state
            .composer_key()
            .as_ref()
            .map(|key| state.diff_comments(key).to_vec())
            .unwrap_or_default()
    }

    fn comments_for(&self, path: &str, cx: &App) -> Vec<DiffComment> {
        self.staged_comments(cx)
            .into_iter()
            .filter(|comment| comment.path == path)
            .collect()
    }

    fn discard_stale_draft(&mut self, cx: &mut Context<Self>) {
        let selected = self.state.read(cx).selected_chat.clone();
        let owner = self.draft.as_ref().map(|draft| draft.key.clone());
        if discard_stale_draft(&mut self.draft, owner.as_ref(), selected.as_ref()) {
            self.sync_comment_rows(cx);
            cx.notify();
        }
    }

    fn draft_anchor(&self) -> Option<(String, CommentSide, u32)> {
        self.draft
            .as_ref()
            .map(|draft| (draft.path.clone(), draft.side, draft.line))
    }

    fn draft_anchor_in(&self, path: &str) -> Option<(CommentSide, u32)> {
        self.draft
            .as_ref()
            .filter(|draft| draft.path == path)
            .map(|draft| (draft.side, draft.line))
    }

    fn sync_comment_rows(&mut self, cx: &mut Context<Self>) {
        if self.parsed.is_none() {
            return;
        }
        let staged = self.staged_comments(cx);
        let draft = self.draft_anchor();
        let key = comment_state_key(&staged, draft.as_ref());
        if key == self.comment_key {
            return;
        }
        self.comment_key = key;
        let Some(parsed) = &self.parsed else {
            return;
        };
        let files = parsed.files.clone();
        for file_ix in (0..self.row_ranges.len().min(files.len())).rev() {
            let file = &files[file_ix];
            // A mid-tween stand-in is the settle sweep's to replace.
            if self
                .folds
                .get(&file.path)
                .is_some_and(|fold| fold.collapsed)
            {
                continue;
            }
            let range = &self.row_ranges[file_ix];
            if self.rows.get(range.start + 1)
                == Some(&DiffRow::FoldingBody {
                    file: file_ix as u32,
                })
            {
                continue;
            }
            let comments: Vec<DiffComment> = staged
                .iter()
                .filter(|comment| comment.path == file.path)
                .cloned()
                .collect();
            let body = body_rows(
                file_ix as u32,
                file,
                &comments,
                self.draft_anchor_in(&file.path),
            );
            self.replace_file_body(file_ix, body);
        }
        cx.notify();
    }

    fn set_hover(
        &mut self,
        path: &str,
        anchor: Option<(CommentSide, u32)>,
        cx: &mut Context<Self>,
    ) {
        let next = anchor.map(|(side, line)| HoverRow {
            path: path.to_string(),
            side,
            line,
        });
        if next != self.hover {
            self.hover = next;
            cx.notify();
        }
    }

    fn clear_hover_at(&mut self, path: &str, side: CommentSide, line: u32, cx: &mut Context<Self>) {
        if self
            .hover
            .as_ref()
            .is_some_and(|hover| hover.path == path && hover.side == side && hover.line == line)
        {
            self.hover = None;
            cx.notify();
        }
    }

    fn open_draft(
        &mut self,
        path: String,
        old_path: Option<String>,
        side: CommentSide,
        line: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(key) = self.state.read(cx).selected_chat.clone() else {
            self.cancel_draft(cx);
            return;
        };
        // "Composer" context: ⏎ commits the comment, ⇧⏎ adds a line.
        let input = cx.new(|cx| ComposerInput::new("Request a change…", cx));
        let events = cx.subscribe(&input, |this: &mut Self, _, event, cx| match event {
            ComposerInputEvent::Submitted => this.commit_draft(cx),
            ComposerInputEvent::Edited => cx.notify(),
            _ => {}
        });
        let handle = input.read(cx).focus_handle(cx);
        self.draft = Some(CommentDraft {
            key,
            path,
            old_path,
            side,
            line,
            input,
            _events: events,
        });
        window.focus(&handle, cx);
        self.sync_comment_rows(cx);
        cx.notify();
    }

    fn cancel_draft(&mut self, cx: &mut Context<Self>) {
        self.draft = None;
        self.sync_comment_rows(cx);
        cx.notify();
    }

    fn commit_draft(&mut self, cx: &mut Context<Self>) {
        let Some(draft) = self.draft.take() else {
            return;
        };
        let body = draft.input.read(cx).text().trim().to_string();
        if body.is_empty() {
            self.sync_comment_rows(cx);
            cx.notify();
            return;
        }
        let comment =
            DiffComment::new(draft.path, draft.side, draft.line, body).renamed_from(draft.old_path);
        self.state.update(cx, |state, cx| {
            if draft_belongs_to(&draft.key, state.selected_chat.as_ref()) {
                state.add_diff_comment(&draft.key, comment);
            }
            cx.notify();
        });
        self.sync_comment_rows(cx);
        cx.notify();
    }

    fn remove_comment(&mut self, owner: &comet_proto::ServerRef, id: &str, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            state.remove_diff_comment(owner, id);
            cx.notify();
        });
        self.sync_comment_rows(cx);
        cx.notify();
    }

    /// Start excerpt parsing and a lazy full-source fetch for an expanded file.
    fn request_highlight(
        &mut self,
        file: &FileDiff,
        parsed_key: &str,
        cx: &mut Context<Self>,
    ) -> Option<Arc<DiffHighlights>> {
        let lang = comet_syntax::language_for_path(&file.path)?;
        let fingerprint = hash64(&[parsed_key, &file.path]);
        if let Some(slot) = self.highlights.get(&file.path)
            && slot.fingerprint == fingerprint
        {
            return match &slot.state {
                DiffHighlightState::Ready(highlights) | DiffHighlightState::Excerpt(highlights) => {
                    Some(highlights.clone())
                }
                DiffHighlightState::Pending | DiffHighlightState::Plain => None,
            };
        }
        if !comet_syntax::supports_language(lang) {
            self.highlights.insert(
                file.path.clone(),
                HighlightSlot {
                    fingerprint,
                    state: DiffHighlightState::Plain,
                    _excerpt_task: None,
                    _fetch_task: None,
                },
            );
            return None;
        }
        let path = file.path.clone();
        let excerpt_file = file.clone();
        let excerpt_path = path.clone();
        let excerpt_task = cx.spawn(async move |this, cx| {
            let highlights = cx
                .background_executor()
                .spawn(async move { excerpt_highlights(&excerpt_file, lang).map(Arc::new) })
                .await;
            this.update(cx, |changes, cx| {
                if let Some(slot) = changes.highlights.get_mut(&excerpt_path)
                    && slot.fingerprint == fingerprint
                    && matches!(slot.state, DiffHighlightState::Pending)
                {
                    slot.state = match highlights {
                        Some(highlights) => DiffHighlightState::Excerpt(highlights),
                        None => DiffHighlightState::Plain,
                    };
                    cx.notify();
                }
            })
            .ok();
        });

        let active = self
            .resolved(cx)
            .filter(|diff| format!("{}:{}", diff.checkout_id, diff.checksum) == parsed_key);
        let engine = self.state.read(cx).selected_client();
        let fetch_file = file.clone();
        let fetch_path = path.clone();
        let fetch_task = match (active, engine) {
            (Some(diff), Some(engine)) => Some(cx.spawn(async move |this, cx| {
                let expected_checksum = diff.checksum.clone();
                let request = comet_proto::GetCheckoutFileDiffTextRequest {
                    checkout_id: diff.checkout_id,
                    cwd: diff.cwd,
                    path: fetch_path.clone(),
                    diff_checksum: expected_checksum.clone(),
                };
                let params = match serde_json::to_value(request) {
                    Ok(params) => params,
                    Err(error) => {
                        tracing::debug!(?error, "checkout diff source request did not serialize");
                        return;
                    }
                };
                let response = match engine
                    .client()
                    .call(methods::GET_CHECKOUT_FILE_DIFF_TEXT, params)
                    .await
                {
                    Ok(value) => match decode_checkout_file_diff_text_reply(value) {
                        Ok(response) => Ok(response),
                        Err(error) => {
                            tracing::debug!(
                                ?error,
                                "checkout diff source reply did not match its wire contract"
                            );
                            return;
                        }
                    },
                    Err(error) => Err(error),
                };
                let highlights = cx
                    .background_executor()
                    .spawn(async move {
                        full_highlights_for_fetch(&fetch_file, lang, &expected_checksum, response)
                    })
                    .await;
                this.update(cx, |changes, cx| {
                    if let Some(slot) = changes.highlights.get_mut(&fetch_path)
                        && promote_highlights_if_current(
                            slot.fingerprint,
                            fingerprint,
                            &mut slot.state,
                            highlights,
                        )
                    {
                        cx.notify();
                    }
                })
                .ok();
            })),
            _ => None,
        };
        self.highlights.insert(
            file.path.clone(),
            HighlightSlot {
                fingerprint,
                state: DiffHighlightState::Pending,
                _excerpt_task: Some(excerpt_task),
                _fetch_task: fetch_task,
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
                self.render_file_header(
                    file as usize,
                    file_diff,
                    &fold,
                    FileHeaderPresentation::Row,
                    &theme,
                    cx,
                )
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
                flat: _,
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
                let spans = highlight
                    .as_deref()
                    .map(|highlights| highlights.spans(line))
                    .unwrap_or(&[]);
                let gutter_px = gutter_width(file_diff);
                let row = diff_line_row_with_syntax(line, spans, &theme, gutter_px);
                let Some((side, line_no)) = line_anchor(line) else {
                    return row;
                };
                let path = file_diff.path.clone();
                let old_path = file_diff.old_path.clone();
                let hovered = self.hover.as_ref().is_some_and(|hover| {
                    hover.path == path && hover.side == side && hover.line == line_no
                });
                let move_path = path.clone();
                let leave_path = path.clone();
                div()
                    .id(("diff-line", ix))
                    .w_full()
                    .relative()
                    .child(row)
                    .when(hovered, |el| {
                        el.child(positioned_adder(
                            comment_adder_left(side, gutter_px),
                            render_comment_adder(
                                &path,
                                old_path.clone(),
                                side,
                                line_no,
                                &theme,
                                cx,
                            ),
                        ))
                    })
                    .on_mouse_move(cx.listener(move |this, _, _, cx| {
                        this.set_hover(&move_path, Some((side, line_no)), cx);
                    }))
                    .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                        if !*hovered {
                            this.clear_hover_at(&leave_path, side, line_no, cx);
                        }
                    }))
                    .into_any_element()
            }
            DiffRow::CommentCard { file, card } => {
                let Some(file_diff) = files.get(file as usize) else {
                    return gpui::Empty.into_any_element();
                };
                let comments = self.comments_for(&file_diff.path, cx);
                let owner = self.state.read(cx).composer_key();
                match comments.get(card as usize) {
                    Some(comment) => render_comment_card(comment, owner, &theme, cx),
                    None => gpui::Empty.into_any_element(),
                }
            }
            DiffRow::CommentDraft { file } => {
                let Some(file_diff) = files.get(file as usize) else {
                    return gpui::Empty.into_any_element();
                };
                match self
                    .draft
                    .as_ref()
                    .filter(|draft| draft.path == file_diff.path)
                {
                    // Header cites the same path the staged card and the
                    // prompt bullet will.
                    Some(draft) => render_comment_draft(
                        draft_cite_path(draft),
                        draft.line,
                        draft.input.clone(),
                        &theme,
                        cx,
                    ),
                    None => gpui::Empty.into_any_element(),
                }
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
        presentation: FileHeaderPresentation,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let collapsed = fold.collapsed;
        let path = file.path.clone();
        let adds = file.additions;
        let dels = file.deletions;
        let sticky = presentation == FileHeaderPresentation::Sticky;
        // The sticky copy floats over scrolling rows, so it cannot simply
        // reuse the row's translucent ink — diff text would read through it.
        // `sticky_file_header_paint` resolves that from the content plane.
        let sticky_paint = sticky.then(|| sticky_file_header_paint(theme));
        let rest_bg = if let Some(paint) = sticky_paint {
            paint.rest_bg
        } else {
            crate::theme::ink(0.025)
        };
        let hover_bg = if let Some(paint) = sticky_paint {
            paint.hover_bg
        } else {
            crate::theme::ink(0.05)
        };

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
                    SharedString::from(format!(
                        "chev-{}-{path}-{}",
                        presentation.key_prefix(),
                        fold.epoch
                    )),
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
            .id(presentation.element_id(ix))
            .w_full()
            .h(px(FILE_HEADER_HEIGHT))
            .when(
                presentation == FileHeaderPresentation::Row && ix > 0,
                |el| el.border_t_1().border_color(crate::theme::hairline(0.04)),
            )
            .when(sticky, |el| {
                el.border_b_1()
                    .border_color(sticky_paint.expect("sticky paint").border)
                    .shadow_sm()
                    .block_mouse_except_scroll()
            })
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .px(px(Theme::SPACE_MD))
            .bg(rest_bg)
            .cursor_pointer()
            .hover(move |s| s.bg(hover_bg))
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

    /// The floating copy of the active file's header, pinned to the top of the
    /// list while that file's rows scroll under it. It reads the list's own
    /// scroll position rather than caching an "active file", so a fold or a
    /// diff reset cannot strand a second, stale one.
    fn render_sticky_file_header(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let scroll_top = self.list.logical_scroll_top();
        let sticky = sticky_file_header(
            &self.row_ranges,
            scroll_top.item_ix,
            scroll_top.offset_in_item.as_f32(),
        )?;
        debug_assert_eq!(
            self.rows.get(sticky.header_row),
            Some(&DiffRow::FileHeader {
                file: sticky.file_ix as u32,
            })
        );
        let files = self.parsed.as_ref()?.files.clone();
        let file = files.get(sticky.file_ix)?;
        let fold = self.folds.get(&file.path).copied().unwrap_or_default();
        let next_header_y = sticky.next_header_row.and_then(|row| {
            let bounds = self.list.bounds_for_item(row)?;
            let viewport = self.list.viewport_bounds();
            Some((bounds.origin.y - viewport.origin.y).as_f32())
        });
        let top_offset = sticky_header_push_offset(next_header_y);
        let header = self.render_file_header(
            sticky.file_ix,
            file,
            &fold,
            FileHeaderPresentation::Sticky,
            theme,
            cx,
        );
        let paint = sticky_file_header_paint(theme);
        // The sticky floats over diff rows, but it belongs to the same content
        // plane. Tint the blur with `theme.bg`; `glass_overlay` is deliberately
        // reserved for elevated menus/cards and produced the wrong hue here.
        let header = if let Some(tint) = paint.frost_tint {
            div().w_full().bg(tint).child(header).into_any_element()
        } else {
            header
        };
        // Frosted is a pass-through when the resolved surface is opaque.
        let header = crate::frost::frosted(0.0, STICKY_FILE_HEADER_BLUR, header);

        Some(
            div()
                .absolute()
                .top(px(top_offset))
                .left_0()
                .w_full()
                .child(header)
                .into_any_element(),
        )
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

fn diff_line_row_with_syntax(
    line: &DiffLine,
    spans: &[comet_syntax::HighlightSpan],
    theme: &Theme,
    gutter_px: f32,
) -> AnyElement {
    let runs = diff_text_runs(line, spans, theme);
    diff_line_row_with_runs(line, runs, theme, gutter_px)
}

/// Paint-only runs for a complete-source diff line. Plain and highlighted
/// paths share this builder so syntax colors cannot alter mono text geometry.
pub(crate) fn diff_text_runs(
    line: &DiffLine,
    spans: &[comet_syntax::HighlightSpan],
    theme: &Theme,
) -> Vec<gpui::TextRun> {
    let mono = font(theme.font_mono.clone());
    render::runs_for_syntax_line_with_plain(
        &line.text,
        spans,
        &mono,
        theme.text.opacity(0.92),
        theme,
    )
}

fn diff_line_row_with_runs(
    line: &DiffLine,
    runs: Vec<gpui::TextRun>,
    theme: &Theme,
    gutter_px: f32,
) -> AnyElement {
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

pub const COMMENT_ADDER_SIZE: f32 = 16.0;

/// A row carries both gutters side by side, and a deletion numbers in the
/// first.
pub fn comment_adder_left(side: CommentSide, gutter_px: f32) -> f32 {
    let column = match side {
        CommentSide::Old => 0.0,
        CommentSide::New => gutter_px,
    };
    ACCENT_BAR_WIDTH + column + (gutter_px - COMMENT_ADDER_SIZE) / 2.0
}

fn positioned_adder(left: f32, adder: AnyElement) -> gpui::Div {
    div()
        .absolute()
        .left(px(left))
        .top(px(0.0))
        .h_full()
        .flex()
        .items_center()
        .child(adder)
}

fn render_comment_adder(
    path: &str,
    old_path: Option<String>,
    side: CommentSide,
    line: u32,
    theme: &Theme,
    cx: &Context<Changes>,
) -> AnyElement {
    let target = path.to_string();
    div()
        .id(SharedString::from(format!(
            "cmt-add-{path}-{}-{line}",
            side.tag()
        )))
        .size(px(COMMENT_ADDER_SIZE))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(4.0))
        .bg(theme.solid)
        .cursor_pointer()
        .on_click(cx.listener(move |this, _, window, cx| {
            // A second `+` replaces the open draft rather than stacking two
            // half-written notes.
            this.open_draft(target.clone(), old_path.clone(), side, line, window, cx);
        }))
        .child(
            crate::icons::icon(crate::icons::PLUS)
                .size(px(11.0))
                .text_color(theme.on_solid),
        )
        .into_any_element()
}

/// A staged comment parked under its line: mono location, body, and a
/// hover-revealed remove. Deliberately not a thread — it is one note that
/// rides the next prompt and then stops existing.
fn render_comment_card(
    comment: &DiffComment,
    owner: Option<comet_proto::ServerRef>,
    theme: &Theme,
    cx: &Context<Changes>,
) -> AnyElement {
    let group: SharedString = format!("cmt-card-{}", comment.id).into();
    let id = comment.id.clone();
    div()
        .group(group.clone())
        .h(px(comments::card_height(&comment.body)))
        .w_full()
        .flex_none()
        .flex()
        .flex_row()
        .bg(crate::theme::ink(0.05))
        // A bar, not a border: it must match ACCENT_BAR_WIDTH exactly or the
        // card's edge steps in and out of the column.
        .child(comment_accent_bar(theme.solid.opacity(0.35)))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .px(px(Theme::SPACE_LG))
                .py(px(comments::CARD_PAD_V / 2.0))
                .child(
                    div()
                        .h(px(comments::CARD_HEADER_HEIGHT))
                        .flex_none()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0))
                        .child(
                            crate::icons::icon(crate::icons::CHAT_ROUND_LINE)
                                .size(px(12.0))
                                .text_color(theme.text_faint),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .font_family(theme.font_mono.clone())
                                .text_size(px(11.0))
                                .text_color(theme.text_faint)
                                .child(SharedString::from(comment.location())),
                        )
                        .child(
                            div()
                                .id(SharedString::from(format!("cmt-remove-{}", comment.id)))
                                .flex_none()
                                .size(px(16.0))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(px(4.0))
                                .cursor_pointer()
                                .opacity(0.0)
                                .group_hover(group, |s| s.opacity(1.0))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if let Some(owner) = owner.as_ref() {
                                        this.remove_comment(owner, &id, cx);
                                    }
                                }))
                                .child(
                                    crate::icons::icon(crate::icons::CLOSE_CIRCLE)
                                        .size(px(12.0))
                                        .text_color(theme.text_muted),
                                ),
                        ),
                )
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        // Height is analytic, so an over-long body clips
                        // inside the card rather than past the fold height.
                        .overflow_hidden()
                        .text_size(px(12.0))
                        .line_height(px(comments::CARD_LINE_HEIGHT))
                        .text_color(theme.text_dim)
                        .child(SharedString::from(comment.body.clone())),
                ),
        )
        .into_any_element()
}

fn comment_accent_bar(color: gpui::Hsla) -> gpui::Div {
    div().w(px(ACCENT_BAR_WIDTH)).h_full().flex_none().bg(color)
}

/// Mirrors [`DiffComment::cite_path`] for the not-yet-staged note.
fn draft_cite_path(draft: &CommentDraft) -> &str {
    match draft.side {
        CommentSide::Old => draft.old_path.as_deref().unwrap_or(&draft.path),
        CommentSide::New => &draft.path,
    }
}

/// Fixed height, so an open draft never fights the fold tween.
fn render_comment_draft(
    path: &str,
    line: u32,
    input: Entity<ComposerInput>,
    theme: &Theme,
    cx: &Context<Changes>,
) -> AnyElement {
    div()
        .h(px(comments::DRAFT_CARD_HEIGHT))
        .w_full()
        .flex_none()
        .flex()
        .flex_row()
        .bg(crate::theme::ink(0.08))
        .child(comment_accent_bar(theme.solid.opacity(0.7)))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .px(px(Theme::SPACE_LG))
                .py(px(10.0))
                .child(
                    div()
                        .h(px(comments::CARD_HEADER_HEIGHT))
                        .flex_none()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0))
                        .child(
                            crate::icons::icon(crate::icons::CHAT_ROUND_LINE)
                                .size(px(12.0))
                                .text_color(theme.text_faint),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .font_family(theme.font_mono.clone())
                                .text_size(px(11.0))
                                .text_color(theme.text_faint)
                                .child(SharedString::from(format!("{path}:{line}"))),
                        ),
                )
                .child(
                    div()
                        .h(px(46.0))
                        .flex_none()
                        .overflow_hidden()
                        .text_size(px(12.0))
                        .child(input.into_any_element()),
                )
                .child(
                    div()
                        .h(px(28.0))
                        .flex_none()
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_end()
                        .gap(px(6.0))
                        .child(
                            comment_action("cmt-cancel", "Cancel", false, theme)
                                .on_click(cx.listener(|this, _, _, cx| this.cancel_draft(cx))),
                        )
                        .child(
                            comment_action("cmt-commit", "Comment", true, theme)
                                .on_click(cx.listener(|this, _, _, cx| this.commit_draft(cx))),
                        ),
                ),
        )
        .into_any_element()
}

fn comment_action(
    id: &'static str,
    label: &'static str,
    primary: bool,
    theme: &Theme,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .h(px(22.0))
        .px(px(10.0))
        .flex()
        .items_center()
        .rounded(px(6.0))
        .text_size(px(11.0))
        .font_weight(gpui::FontWeight::MEDIUM)
        .cursor_pointer()
        .when(primary, |el| el.bg(theme.solid).text_color(theme.on_solid))
        .when(!primary, |el| {
            el.text_color(motion::hover_blend(id, theme.text_muted, theme.text))
                .bg(motion::hover_blend(
                    id,
                    gpui::transparent_black(),
                    theme.element_hover,
                ))
                .on_hover(motion::hover_listener(id))
        })
        .child(SharedString::from(label))
}

/// Render a complete-source diff without changing its row geometry. This is
/// kept separate from the virtualized checkout view, whose excerpt highlighter
/// is still its best available result until the source-pair RPC lands.
pub(crate) fn render_file_body_with_syntax(
    file: &FileDiff,
    highlights: Option<Arc<DiffHighlights>>,
    theme: &Theme,
) -> AnyElement {
    let mut children: Vec<AnyElement> = Vec::new();
    let gutter_px = gutter_width(file);
    for notice in file_notices(file) {
        children.push(notice_row(notice, theme));
    }
    for hunk in &file.hunks {
        children.push(hunk_header_row(&hunk.header, theme));
        for line in &hunk.lines {
            let spans = highlights
                .as_deref()
                .map(|highlights| highlights.spans(line))
                .unwrap_or(&[]);
            children.push(diff_line_row_with_syntax(line, spans, theme, gutter_px));
        }
    }
    div()
        .flex()
        .flex_col()
        .pb(px(BODY_BOTTOM_PAD))
        .children(children)
        .into_any_element()
}

/// Build the expanded body rows that start above `max_px`; the fold tween's
/// stand-in never materializes lines its clip cannot reveal.
fn render_file_body_upto(
    file: &FileDiff,
    highlight: Option<Arc<DiffHighlights>>,
    theme: &Theme,
    max_px: f32,
) -> AnyElement {
    let mut children: Vec<AnyElement> = Vec::new();
    let mut y = 0.0f32;
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
                let spans = highlight
                    .as_deref()
                    .map(|highlights| highlights.spans(line))
                    .unwrap_or(&[]);
                children.push(diff_line_row_with_syntax(line, spans, theme, gutter_px));
                y += DIFF_LINE_HEIGHT;
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
                    let sticky_header = self.render_sticky_file_header(&theme, cx);
                    div()
                        .flex_1()
                        .min_h_0()
                        .flex()
                        .flex_col()
                        .children(self.render_header_strip(&theme))
                        .child(
                            div()
                                .relative()
                                .flex_1()
                                .min_h_0()
                                .overflow_hidden()
                                .child(
                                    list(self.list.clone(), cx.processor(Self::render_row))
                                        .size_full()
                                        .with_sizing_behavior(gpui::ListSizingBehavior::Auto),
                                )
                                .when_some(sticky_header, |el, header| el.child(header)),
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

    fn line(kind: LineKind, old_no: Option<u32>, new_no: Option<u32>) -> DiffLine {
        DiffLine {
            kind,
            old_no,
            new_no,
            text: "line".into(),
        }
    }

    #[test]
    fn diff_line_comment_anchors_use_the_source_side() {
        assert_eq!(
            line_anchor(&line(LineKind::Del, Some(7), None)),
            Some((CommentSide::Old, 7))
        );
        assert_eq!(
            line_anchor(&line(LineKind::Add, None, Some(8))),
            Some((CommentSide::New, 8))
        );
        assert_eq!(
            line_anchor(&line(LineKind::Context, Some(8), Some(9))),
            Some((CommentSide::New, 9))
        );
        assert_eq!(line_anchor(&line(LineKind::Meta, None, None)), None);
    }

    #[test]
    fn renamed_file_comments_cite_the_path_for_their_side() {
        let old = DiffComment::new("src/new.rs", CommentSide::Old, 7, "old")
            .renamed_from(Some("src/old.rs"));
        let new = DiffComment::new("src/new.rs", CommentSide::New, 8, "new")
            .renamed_from(Some("src/old.rs"));

        assert!(
            comments::with_comments("review", &[old, new])
                .ends_with("- src/old.rs:7 (L): old\n- src/new.rs:8 (R): new")
        );
    }

    fn commented_file() -> FileDiff {
        parse_patch(
            "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -7 +7 @@\n-old\n+new\n",
        )
        .remove(0)
    }

    #[test]
    fn comment_cards_and_draft_follow_their_anchor() {
        let file = commented_file();
        let comment = DiffComment::new("src/lib.rs", CommentSide::Old, 7, "replace this");
        let rows = body_rows(
            0,
            &file,
            std::slice::from_ref(&comment),
            Some((CommentSide::New, 7)),
        );

        assert!(matches!(
            rows[1],
            DiffRow::Line {
                line: 0,
                flat: 0,
                ..
            }
        ));
        assert_eq!(rows[2], DiffRow::CommentCard { file: 0, card: 0 });
        assert!(matches!(
            rows[3],
            DiffRow::Line {
                line: 1,
                flat: 1,
                ..
            }
        ));
        assert_eq!(rows[4], DiffRow::CommentDraft { file: 0 });
    }

    #[test]
    fn comment_rows_contribute_to_analytic_body_height() {
        let file = commented_file();
        let comment = DiffComment::new("src/lib.rs", CommentSide::Old, 7, "replace this");

        assert_eq!(
            body_height_with(
                &file,
                std::slice::from_ref(&comment),
                Some((CommentSide::New, 7)),
            ),
            body_height(&file) + comments::card_height(&comment.body) + comments::DRAFT_CARD_HEIGHT
        );
    }

    #[test]
    fn draft_is_discarded_when_its_server_ref_changes() {
        let owner =
            comet_proto::ServerRef::new(comet_proto::ServerId::new("sha256:a"), "same-chat");
        let other =
            comet_proto::ServerRef::new(comet_proto::ServerId::new("sha256:b"), "same-chat");

        assert!(draft_belongs_to(&owner, Some(&owner)));
        assert!(!draft_belongs_to(&owner, Some(&other)));
        assert!(!draft_belongs_to(&owner, None));

        let mut draft = Some("unsent");
        assert!(discard_stale_draft(&mut draft, Some(&owner), Some(&other)));
        assert!(draft.is_none());
    }

    #[test]
    fn comments_without_a_visible_anchor_remain_removable() {
        let file = commented_file();
        let comment = DiffComment::new("src/lib.rs", CommentSide::Old, 99, "still here");
        let rows = body_rows(0, &file, std::slice::from_ref(&comment), None);

        assert_eq!(rows[0], DiffRow::CommentCard { file: 0, card: 0 });
    }

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
        let (rows, ranges) = flatten_rows(&files, &[], None, |_| false);
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
        let (rows, ranges) = flatten_rows(&files, &[], None, |ix| ix == 0);
        assert_eq!(ranges[0].len(), 1);
        assert_eq!(rows[ranges[1].start], DiffRow::FileHeader { file: 1 });

        // Notices lead the body: the added file carries "New file".
        let added_rows = &rows[ranges[1].clone()];
        assert_eq!(added_rows[1], DiffRow::Notice { file: 1, notice: 0 });
    }

    #[test]
    fn sticky_header_tracks_the_logical_top_row() {
        let ranges = vec![0..4, 4..5, 5..10];

        assert_eq!(sticky_file_header(&[], 0, 0.0), None);
        assert_eq!(sticky_file_header(&ranges, 0, 0.0), None);
        assert_eq!(
            sticky_file_header(&ranges, 0, 0.5),
            Some(StickyFileHeader {
                file_ix: 0,
                header_row: 0,
                next_header_row: Some(4),
            })
        );
        assert_eq!(
            sticky_file_header(&ranges, 2, 0.0),
            Some(StickyFileHeader {
                file_ix: 0,
                header_row: 0,
                next_header_row: Some(4),
            })
        );

        // Landing exactly on a new header hands ownership to that file; its
        // real row remains visible until it starts crossing the viewport.
        assert_eq!(sticky_file_header(&ranges, 4, 0.0), None);
        assert_eq!(
            sticky_file_header(&ranges, 4, 1.0),
            Some(StickyFileHeader {
                file_ix: 1,
                header_row: 4,
                next_header_row: Some(5),
            })
        );
        assert_eq!(sticky_file_header(&ranges, 5, 0.0), None);
        assert_eq!(
            sticky_file_header(&ranges, 8, 0.0),
            Some(StickyFileHeader {
                file_ix: 2,
                header_row: 5,
                next_header_row: None,
            })
        );
        assert_eq!(sticky_file_header(&ranges, 10, 0.0), None);
    }

    #[test]
    fn sticky_header_is_pushed_by_the_next_file() {
        assert_eq!(sticky_header_push_offset(None), 0.0);
        assert_eq!(sticky_header_push_offset(Some(80.0)), 0.0);
        assert_eq!(sticky_header_push_offset(Some(FILE_HEADER_HEIGHT)), 0.0);
        assert_eq!(sticky_header_push_offset(Some(24.0)), -12.0);
        assert_eq!(sticky_header_push_offset(Some(0.0)), -FILE_HEADER_HEIGHT);
    }

    /// The sticky header belongs to the diff's CONTENT plane. Borrowing
    /// `glass_overlay` — the elevated plane menus and cards paint on — is the
    /// bug this pins: it produced the wrong hue behind the blur.
    ///
    /// Upstream's version of this test drives `Theme::for_selection` over named
    /// variants, which this fork has no accent/surface selection API for; both
    /// arms are exercised through `sticky_file_header_paint_for` instead, since
    /// `is_glass()` is compile-time `false` wherever the gate runs.
    #[test]
    fn sticky_header_paints_from_the_content_plane_in_both_appearances() {
        for theme in [Theme::dark(), Theme::light()] {
            let opaque = sticky_file_header_paint_for(&theme, false);
            assert_eq!(opaque.frost_tint, None);
            assert_eq!(
                opaque.rest_bg,
                crate::theme::flatten(theme.ink(0.025), theme.bg),
                "opaque rest wash must be flattened, not translucent"
            );
            assert_eq!(
                opaque.hover_bg,
                crate::theme::flatten(theme.element_hover, theme.bg)
            );
            assert_eq!(opaque.border, theme.border);
            assert_eq!(
                opaque.rest_bg.a, 1.0,
                "a floating header cannot be see-through"
            );
            assert_eq!(opaque.hover_bg.a, 1.0);

            let glass = sticky_file_header_paint_for(&theme, true);
            let expected_alpha = match theme.appearance {
                crate::theme::Appearance::Dark => STICKY_FILE_HEADER_TINT_ALPHA_DARK,
                crate::theme::Appearance::Light => STICKY_FILE_HEADER_TINT_ALPHA_LIGHT,
            };
            let tint = glass.frost_tint.expect("glass tints the blur");
            assert_eq!(tint, theme.bg.opacity(expected_alpha));
            // The negative check only bites in dark. This fork's LIGHT theme
            // paints `bg` and `surface_overlay` as the same pure white, and its
            // tint coverage matches `glass_overlay`'s light coverage, so the two
            // planes are numerically identical there — indistinguishable by
            // value, however different in provenance.
            if theme.appearance == crate::theme::Appearance::Dark {
                assert_ne!(
                    tint,
                    theme.glass_overlay(),
                    "the sticky header must not borrow the elevated overlay plane"
                );
            }
            assert_eq!(glass.hover_bg, theme.glass_hover());
            assert_eq!(glass.border, theme.border);
        }

        // Light needs the heavier tint: dark glyphs ghost through a blur that
        // light glyphs on a dark tint survive.
        assert!(STICKY_FILE_HEADER_TINT_ALPHA_LIGHT > STICKY_FILE_HEADER_TINT_ALPHA_DARK);
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
    fn full_diff_highlights_map_old_new_and_context_by_source_line() {
        let old_source = "fn old() {\n    let value = 1;\n}\n";
        let new_source = "fn new() {\n    let value = 2;\n}\n";
        let parse = |source| {
            Arc::new(
                comet_syntax::highlight(comet_syntax::HighlightRequest {
                    source,
                    path: Some("src/lib.rs"),
                    fence_tag: None,
                })
                .unwrap(),
            )
        };
        let highlights = DiffHighlights {
            old: Some(parse(old_source)),
            new: Some(parse(new_source)),
        };
        let deleted = DiffLine {
            kind: LineKind::Del,
            old_no: Some(1),
            new_no: None,
            text: "fn old() {".into(),
        };
        let added = DiffLine {
            kind: LineKind::Add,
            old_no: None,
            new_no: Some(1),
            text: "fn new() {".into(),
        };
        let context = DiffLine {
            kind: LineKind::Context,
            old_no: Some(2),
            new_no: Some(2),
            text: "    let value = 2;".into(),
        };

        assert_eq!(
            highlights.source_ref(&deleted),
            Some(SourceLineRef {
                side: SourceSide::Old,
                line_number: 1
            })
        );
        assert_eq!(
            highlights.source_ref(&added),
            Some(SourceLineRef {
                side: SourceSide::New,
                line_number: 1
            })
        );
        assert_eq!(
            highlights.source_ref(&context),
            Some(SourceLineRef {
                side: SourceSide::New,
                line_number: 2
            })
        );
        assert!(
            highlights
                .spans(&deleted)
                .iter()
                .any(|span| span.kind == comet_syntax::HighlightKind::Function)
        );
        assert!(
            highlights
                .spans(&added)
                .iter()
                .any(|span| span.kind == comet_syntax::HighlightKind::Function)
        );

        let old_only = DiffHighlights {
            old: highlights.old.clone(),
            new: None,
        };
        assert_eq!(
            old_only.source_ref(&context),
            Some(SourceLineRef {
                side: SourceSide::Old,
                line_number: 2
            })
        );
        let invalid = DiffLine {
            kind: LineKind::Add,
            old_no: None,
            new_no: Some(0),
            text: String::new(),
        };
        assert!(highlights.spans(&invalid).is_empty());
    }

    #[test]
    fn tool_diff_complete_sources_are_rejected_on_line_mismatch() {
        let file = FileDiff {
            path: "src/lib.rs".into(),
            old_path: None,
            status: FileStatus::Modified,
            binary: false,
            notices: Vec::new(),
            hunks: vec![Hunk {
                header: "@@ -1,2 +1,2 @@".into(),
                lines: vec![
                    DiffLine {
                        kind: LineKind::Del,
                        old_no: Some(1),
                        new_no: None,
                        text: "fn old() {}".into(),
                    },
                    DiffLine {
                        kind: LineKind::Add,
                        old_no: None,
                        new_no: Some(1),
                        text: "fn new() {}".into(),
                    },
                    DiffLine {
                        kind: LineKind::Context,
                        old_no: Some(2),
                        new_no: Some(2),
                        text: "let shared = true;".into(),
                    },
                ],
            }],
            additions: 1,
            deletions: 1,
            max_line: 2,
        };
        assert!(sources_match_diff(
            &file,
            Some("fn old() {}\nlet shared = true;\n"),
            Some("fn new() {}\nlet shared = true;\n")
        ));
        assert!(!sources_match_diff(
            &file,
            Some("fn stale() {}\nlet shared = true;\n"),
            Some("fn new() {}\nlet shared = true;\n")
        ));
    }

    #[test]
    fn source_validation_splits_each_complete_source_once_for_many_high_lines() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let source = (1..=12_000)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let lines = (11_001..=12_000)
            .map(|line| DiffLine {
                kind: LineKind::Context,
                old_no: Some(line),
                new_no: Some(line),
                text: format!("line {line}"),
            })
            .collect();
        let file = FileDiff {
            path: "src/large.rs".into(),
            old_path: None,
            status: FileStatus::Modified,
            binary: false,
            notices: Vec::new(),
            hunks: vec![Hunk {
                header: "@@ -11001,1000 +11001,1000 @@".into(),
                lines,
            }],
            additions: 0,
            deletions: 0,
            max_line: 12_000,
        };
        let split_calls = AtomicUsize::new(0);

        assert!(sources_match_diff_with(
            &file,
            Some(&source),
            Some(&source),
            |text| {
                split_calls.fetch_add(1, Ordering::Relaxed);
                text.lines().collect()
            },
        ));
        assert_eq!(split_calls.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn excerpt_parses_old_and_new_hunks_as_separate_documents() {
        let file = FileDiff {
            path: "src/lib.rs".into(),
            old_path: None,
            status: FileStatus::Modified,
            binary: false,
            notices: vec![],
            hunks: vec![Hunk {
                header: "@@ -1,3 +1,3 @@".into(),
                lines: vec![
                    DiffLine {
                        kind: LineKind::Context,
                        old_no: Some(1),
                        new_no: Some(1),
                        text: "/* start".into(),
                    },
                    DiffLine {
                        kind: LineKind::Del,
                        old_no: Some(2),
                        new_no: None,
                        text: "old body".into(),
                    },
                    DiffLine {
                        kind: LineKind::Add,
                        old_no: None,
                        new_no: Some(2),
                        text: "new body".into(),
                    },
                    DiffLine {
                        kind: LineKind::Context,
                        old_no: Some(3),
                        new_no: Some(3),
                        text: "end */".into(),
                    },
                ],
            }],
            additions: 1,
            deletions: 1,
            max_line: 3,
        };
        let highlights = excerpt_highlights(&file, Lang::Rust).expect("excerpt");
        let deleted = &file.hunks[0].lines[1];
        let added = &file.hunks[0].lines[2];
        assert!(
            highlights
                .spans(deleted)
                .iter()
                .any(|span| span.kind == comet_syntax::HighlightKind::Comment)
        );
        assert!(
            highlights
                .spans(added)
                .iter()
                .any(|span| span.kind == comet_syntax::HighlightKind::Comment)
        );
    }

    #[test]
    fn checkout_file_diff_text_reply_decodes_literal_camel_case_json() {
        let decoded = decode_checkout_file_diff_text_reply(serde_json::json!({
            "diffChecksum": "sum-1",
            "oldText": "let old = 1;\n",
            "newText": "let new = 2;\n",
            "oldContentHash": "sha256-old",
            "newContentHash": "sha256-new",
            "binary": false,
            "truncated": false,
            "stale": false
        }))
        .expect("literal engine reply");

        assert_eq!(decoded.diff_checksum, "sum-1");
        assert_eq!(decoded.old_text.as_deref(), Some("let old = 1;\n"));
        assert_eq!(decoded.new_text.as_deref(), Some("let new = 2;\n"));
        assert_eq!(decoded.old_content_hash.as_deref(), Some("sha256-old"));
        assert_eq!(decoded.new_content_hash.as_deref(), Some("sha256-new"));
        assert!(!decoded.binary);
        assert!(!decoded.truncated);
        assert!(!decoded.stale);
    }

    #[test]
    fn excerpt_promotes_to_complete_only_for_current_snapshot() {
        let excerpt = Arc::new(DiffHighlights::default());
        let complete = Arc::new(DiffHighlights::default());
        let mut state = DiffHighlightState::Excerpt(excerpt);

        assert!(promote_highlights_if_current(
            41,
            41,
            &mut state,
            Some(complete.clone())
        ));
        assert!(matches!(
            state,
            DiffHighlightState::Ready(ref installed) if Arc::ptr_eq(installed, &complete)
        ));
    }

    fn source_pair_file() -> FileDiff {
        FileDiff {
            path: "src/lib.rs".into(),
            old_path: None,
            status: FileStatus::Modified,
            binary: false,
            notices: vec![],
            hunks: vec![Hunk {
                header: "@@ -1 +1 @@".into(),
                lines: vec![
                    DiffLine {
                        kind: LineKind::Del,
                        old_no: Some(1),
                        new_no: None,
                        text: "let old = 1;".into(),
                    },
                    DiffLine {
                        kind: LineKind::Add,
                        old_no: None,
                        new_no: Some(1),
                        text: "let new = 2;".into(),
                    },
                ],
            }],
            additions: 1,
            deletions: 1,
            max_line: 1,
        }
    }

    fn source_pair_reply() -> comet_proto::CheckoutFileDiffText {
        comet_proto::CheckoutFileDiffText {
            diff_checksum: "sum-1".into(),
            old_text: Some("let old = 1;\n".into()),
            new_text: Some("let new = 2;\n".into()),
            old_content_hash: Some("sha256-old".into()),
            new_content_hash: Some("sha256-new".into()),
            binary: false,
            truncated: false,
            stale: false,
        }
    }

    #[test]
    fn binary_truncated_stale_and_unknown_method_keep_excerpt() {
        let file = source_pair_file();
        let excerpt = Arc::new(excerpt_highlights(&file, Lang::Rust).expect("excerpt"));
        let mut stale = source_pair_reply();
        stale.stale = true;
        let mut binary = source_pair_reply();
        binary.binary = true;
        let mut truncated = source_pair_reply();
        truncated.truncated = true;
        let mut absent = source_pair_reply();
        absent.old_text = None;
        absent.new_text = None;

        for fetch in [
            Ok(stale),
            Ok(binary),
            Ok(truncated),
            Ok(absent),
            Err(comet_rpc::RpcError::UnknownMethod(
                methods::GET_CHECKOUT_FILE_DIFF_TEXT.into(),
            )),
        ] {
            let candidate = full_highlights_for_fetch(&file, Lang::Rust, "sum-1", fetch);
            let mut state = DiffHighlightState::Excerpt(excerpt.clone());
            assert!(!promote_highlights_if_current(
                17, 17, &mut state, candidate
            ));
            assert!(matches!(
                state,
                DiffHighlightState::Excerpt(ref retained) if Arc::ptr_eq(retained, &excerpt)
            ));
        }
    }

    #[test]
    fn late_highlight_result_does_not_replace_newer_diff_state() {
        let file = source_pair_file();
        let mut old_reply = source_pair_reply();
        old_reply.diff_checksum = "sum-old".into();
        assert!(
            full_highlights_for_fetch(&file, Lang::Rust, "sum-new", Ok(old_reply)).is_none(),
            "the reply checksum is checked before parsing"
        );

        let current = Arc::new(DiffHighlights::default());
        let late = Arc::new(DiffHighlights::default());
        let mut state = DiffHighlightState::Excerpt(current.clone());
        let old_fingerprint = hash64(&["checkout:sum-old", "src/lib.rs"]);
        let current_fingerprint = hash64(&["checkout:sum-new", "src/lib.rs"]);

        assert!(!promote_highlights_if_current(
            current_fingerprint,
            old_fingerprint,
            &mut state,
            Some(late)
        ));
        assert!(matches!(
            state,
            DiffHighlightState::Excerpt(ref retained) if Arc::ptr_eq(retained, &current)
        ));
    }

    #[test]
    fn full_diff_sources_cover_added_deleted_renamed_and_context_lines() {
        let added = FileDiff {
            path: "src/added.rs".into(),
            old_path: None,
            status: FileStatus::Added,
            binary: false,
            notices: vec![],
            hunks: vec![Hunk {
                header: "@@ -0,0 +1 @@".into(),
                lines: vec![DiffLine {
                    kind: LineKind::Add,
                    old_no: None,
                    new_no: Some(1),
                    text: "fn added() {}".into(),
                }],
            }],
            additions: 1,
            deletions: 0,
            max_line: 1,
        };
        let added_reply = comet_proto::CheckoutFileDiffText {
            diff_checksum: "added".into(),
            old_text: None,
            new_text: Some("fn added() {}\n".into()),
            old_content_hash: None,
            new_content_hash: Some("new".into()),
            binary: false,
            truncated: false,
            stale: false,
        };
        let added_highlights = full_highlights(&added, Lang::Rust, &added_reply).expect("added");
        assert!(added_highlights.old.is_none());
        assert!(added_highlights.new.is_some());
        assert!(!added_highlights.spans(&added.hunks[0].lines[0]).is_empty());

        let deleted = FileDiff {
            path: "src/deleted.rs".into(),
            old_path: None,
            status: FileStatus::Deleted,
            binary: false,
            notices: vec![],
            hunks: vec![Hunk {
                header: "@@ -1 +0,0 @@".into(),
                lines: vec![DiffLine {
                    kind: LineKind::Del,
                    old_no: Some(1),
                    new_no: None,
                    text: "fn deleted() {}".into(),
                }],
            }],
            additions: 0,
            deletions: 1,
            max_line: 1,
        };
        let deleted_reply = comet_proto::CheckoutFileDiffText {
            diff_checksum: "deleted".into(),
            old_text: Some("fn deleted() {}\n".into()),
            new_text: None,
            old_content_hash: Some("old".into()),
            new_content_hash: None,
            binary: false,
            truncated: false,
            stale: false,
        };
        let deleted_highlights =
            full_highlights(&deleted, Lang::Rust, &deleted_reply).expect("deleted");
        assert!(deleted_highlights.old.is_some());
        assert!(deleted_highlights.new.is_none());
        assert!(
            !deleted_highlights
                .spans(&deleted.hunks[0].lines[0])
                .is_empty()
        );

        let renamed = FileDiff {
            path: "src/renamed.rs".into(),
            old_path: Some("legacy.txt".into()),
            status: FileStatus::Renamed,
            binary: false,
            notices: vec![],
            hunks: vec![Hunk {
                header: "@@ -1,2 +1,2 @@".into(),
                lines: vec![
                    DiffLine {
                        kind: LineKind::Del,
                        old_no: Some(1),
                        new_no: None,
                        text: "fn old() {}".into(),
                    },
                    DiffLine {
                        kind: LineKind::Add,
                        old_no: None,
                        new_no: Some(1),
                        text: "fn new() {}".into(),
                    },
                    DiffLine {
                        kind: LineKind::Context,
                        old_no: Some(2),
                        new_no: Some(2),
                        text: "const SHARED: bool = true;".into(),
                    },
                ],
            }],
            additions: 1,
            deletions: 1,
            max_line: 2,
        };
        let renamed_reply = comet_proto::CheckoutFileDiffText {
            diff_checksum: "renamed".into(),
            old_text: Some("fn old() {}\nconst SHARED: bool = true;\n".into()),
            new_text: Some("fn new() {}\nconst SHARED: bool = true;\n".into()),
            old_content_hash: Some("old".into()),
            new_content_hash: Some("new".into()),
            binary: false,
            truncated: false,
            stale: false,
        };
        let renamed_highlights =
            full_highlights(&renamed, Lang::Rust, &renamed_reply).expect("renamed");
        assert_eq!(
            renamed_highlights.old.as_ref().unwrap().language,
            Lang::Rust,
            "the final path selects the language for both sides"
        );
        assert_eq!(
            renamed_highlights.new.as_ref().unwrap().language,
            Lang::Rust
        );
        for line in &renamed.hunks[0].lines {
            assert!(
                !renamed_highlights.spans(line).is_empty(),
                "missing spans for {line:?}"
            );
        }
    }

    #[test]
    fn mismatched_full_sources_are_rejected_atomically() {
        let file = FileDiff {
            path: "src/lib.rs".into(),
            old_path: None,
            status: FileStatus::Modified,
            binary: false,
            notices: vec![],
            hunks: vec![Hunk {
                header: "@@ -1 +1 @@".into(),
                lines: vec![
                    DiffLine {
                        kind: LineKind::Del,
                        old_no: Some(1),
                        new_no: None,
                        text: "let old = 1;".into(),
                    },
                    DiffLine {
                        kind: LineKind::Add,
                        old_no: None,
                        new_no: Some(1),
                        text: "let new = 2;".into(),
                    },
                ],
            }],
            additions: 1,
            deletions: 1,
            max_line: 1,
        };
        let response = comet_proto::CheckoutFileDiffText {
            diff_checksum: "sum".into(),
            old_text: Some("let old = 1;\n".into()),
            new_text: Some("different snapshot\n".into()),
            old_content_hash: None,
            new_content_hash: None,
            binary: false,
            truncated: false,
            stale: false,
        };
        assert!(!sources_match_diff(
            &file,
            response.old_text.as_deref(),
            response.new_text.as_deref()
        ));
        assert!(full_highlights(&file, Lang::Rust, &response).is_none());
    }
}
