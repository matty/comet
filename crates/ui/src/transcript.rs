//! The conversation view: virtualized transcript with block-granularity rows,
//! stick-to-bottom, tool-group folding, and streaming markdown.
//!
//! Row model (docs/research/mugen-pretext.md §3):
//! - one row per BLOCK: user message = one bubble row; assistant messages split
//!   into one row per markdown top-level block, plus consecutive-tool groups and
//!   input/error chips;
//! - stable row ids `{msgId}#{partId}.{blockIx}` / `{msgId}#g{groupIx}` — LIVE
//!   (streaming) entries split per block exactly like completed ones (the list
//!   virtualizes them, so a fading live reply re-renders only its visible tail
//!   each frame — flat cost in the reply length); on completion each block row
//!   keeps its id, so row identity is continuous and nothing flickers;
//! - rows are cached per entry keyed by a content fingerprint — only changed
//!   messages rebuild (the anti-"streaming stutter" trick);
//! - row-set changes diff by (id, version) into one minimal `splice`.
//!
//! Stick-to-bottom is a velocity spring (mugen §1e, the same shape as
//! stackblitz's use-stick-to-bottom): while pinned, a per-frame stepper glides
//! the viewport toward the list end with a feed-forward term tracking the
//! smoothed target growth, so 120ms doc commits read as a continuous glide
//! instead of per-commit snaps. The pin breaks only on user input (the list's
//! scroll handler fires exclusively from its wheel/touch path) and re-engages
//! inside the 70px band; own-send re-engages with the same glide.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::rc::Rc;
use std::sync::{
    Arc, Weak,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, BorderStyle, ClipboardItem, Context, Entity, ListAlignment, ListScrollEvent,
    ListState, ObjectFit, SharedString, StyledImage as _, StyledText, Subscription, Task, TextRun,
    Window, canvas, div, img, list, prelude::*, px, quad,
};

use comet_doc::{MessagePart, MessageRole, MessageStatus, SessionMessageEntry};
use comet_proto::{
    ChecklistStatus, NoticeSeverity, ReadToolDiffReply, ServerId, ServerRef, SubagentStatus,
    ToolCall, ToolDiff, ToolDiffStat,
};
use comet_rpc::methods;

use crate::markdown::parser::{Block, BlockTree, IncrementalParser, parse_full};
use crate::markdown::render::{self, RenderCache, RenderOptions};
use crate::markdown::veil::RowVeil;
use crate::motion::{self, AnimationExt as _, RESIZE};
use crate::state::AppState;
use crate::syntax_cache::{DocumentHighlightKey, SyntaxHighlightCache};
use crate::theme::Theme;
use comet_syntax::LanguageId as Lang;

// ---------------------------------------------------------------------------
// Constants (mugen ports)
// ---------------------------------------------------------------------------

/// Re-engage the bottom pin when the user returns within this many px of the end.
pub const STICK_THRESHOLD_PX: f32 = 70.0;
/// List overdraw beyond the viewport.
pub const OVERDRAW_PX: f32 = 320.0;
/// Show the scroll-to-bottom button beyond this distance from the end.
pub const SCROLL_BUTTON_THRESHOLD_PX: f32 = 320.0;
/// Vertical gap opening a new turn (new message entry).
pub const GAP_TURN: f32 = 14.0;
/// Vertical gap between blocks within a turn.
pub const GAP_BLOCK: f32 = 8.0;
/// Transcript column max width (comet 46rem).
pub const MAX_CONTENT_WIDTH: f32 = 736.0;
/// Tool chip row height / gap — analytic, so fold heights need no measurement.
/// A row is the guide rail + a 30px chip card centered in it (comet
/// tool-chip.tsx: `TOOL_CHIP_HEIGHT = 38`, card `h-[30px]`); rows stack with no
/// gap so the rail reads continuous.
pub const CHIP_HEIGHT: f32 = 38.0;
pub const CHIP_GAP: f32 = 0.0;
pub const CHIP_CARD_HEIGHT: f32 = 30.0;
const CHIPS_TOP_PAD: f32 = 2.0;
/// The diff detail wrapper's top padding. It participates in every open
/// detail height, including loading and terminal unavailable states.
const TOOL_DIFF_DETAIL_TOP_PAD: f32 = 2.0;
const TOOL_DIFF_STATUS_HEIGHT: f32 = 20.0;
/// A sidecar read may not occupy the federated generic RPC lane indefinitely.
const TOOL_DIFF_FETCH_TIMEOUT: Duration = Duration::from_secs(8);
/// How long a user fold toggle keeps its height tween armed: the RESIZE
/// spec's 200ms plus margin. Past this the fold renders statically — an armed
/// tween replays on remount, i.e. on every scroll-back-into-view.
const FOLD_TWEEN_WINDOW: std::time::Duration = std::time::Duration::from_millis(400);
/// User-bubble attachment thumbnails (user-attachments.tsx): 112×80 thumbs in
/// a FIXED-height strip (load-state flips never shift the virtualizer).
pub const ATT_THUMB_W: f32 = 112.0;
pub const ATT_THUMB_H: f32 = 80.0;
pub const ATT_STRIP_H: f32 = ATT_THUMB_H + 10.0;

// ---------------------------------------------------------------------------
// Stick-to-bottom spring (mugen §1e — same constants as its DEFAULT_SPRING,
// which follows the shape of stackblitz/use-stick-to-bottom)
// ---------------------------------------------------------------------------

/// Retains velocity frame-to-frame (higher = more glide).
pub const SPRING_DAMPING: f32 = 0.7;
/// Pull toward the target (higher = snappier).
pub const SPRING_STIFFNESS: f32 = 0.05;
/// Inertia (higher = slower to start/stop).
pub const SPRING_MASS: f32 = 1.25;
/// Reference frame for the fixed-timestep integration (60fps).
pub const SPRING_FRAME_MS: f32 = 1000.0 / 60.0;
/// Cap on simulated frames per tick — a hitch catches up instead of teleporting.
pub const SPRING_MAX_CATCHUP_FRAMES: f32 = 8.0;
/// EMA rate for the feed-forward target-growth estimate.
pub const SPRING_GROWTH_EMA: f32 = 0.12;
/// While streaming, chase up to this many px above the true bottom (keeps the
/// growing tail visible instead of hugging a moving edge).
pub const SPRING_CHASE_MAX_LEAD: f32 = 32.0;
/// Treat as exactly pinned within this distance of the bottom.
pub const AT_BOTTOM_PX: f32 = 2.0;
/// Keep the spring loop warm this long after landing, so a streaming pause
/// resumes at cruise instead of re-accelerating from zero.
pub const SPRING_SETTLE_GRACE_MS: u64 = 500;
/// Teleport when farther than this many viewports from the end; glide the rest.
pub const GLIDE_MAX_VIEWPORTS: f32 = 2.5;

/// Pure stick-to-bottom spring stepper — the mugen `tick()` integration:
/// velocity relaxes toward `(damping·v + stiffness·diff)/mass` per 60fps
/// sub-frame, position advances by `v + target_vel` where `target_vel` is a
/// feed-forward EMA of target growth px/frame, and the chase point sits up to
/// [`SPRING_CHASE_MAX_LEAD`] px above the true bottom proportional to growth.
#[derive(Debug, Clone, Copy)]
pub struct StickSpring {
    /// Spring velocity, px per 60fps frame.
    velocity: f32,
    /// Feed-forward: smoothed target growth, px per 60fps frame.
    target_vel: f32,
    /// Target observed at the previous tick (`None` = fresh/parked).
    last_target: Option<f32>,
}

impl Default for StickSpring {
    fn default() -> Self {
        Self::new()
    }
}

impl StickSpring {
    pub fn new() -> Self {
        Self {
            velocity: 0.0,
            target_vel: 0.0,
            last_target: None,
        }
    }

    /// Park the spring (drops all state; the next tick starts cold).
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Residual motion below mugen's settle thresholds (`v < .05 && targetVel
    /// < .05`)?
    pub fn is_idle(&self) -> bool {
        self.velocity < 0.05 && self.target_vel < 0.05
    }

    #[cfg(test)]
    pub(crate) fn target_vel(&self) -> f32 {
        self.target_vel
    }

    /// Advance one tick. `pos`/`target` are scroll offsets in px (larger =
    /// closer to the bottom); `frames` is elapsed time in 60fps frames
    /// (clamped by the caller to [`SPRING_MAX_CATCHUP_FRAMES`]). Returns the
    /// new position: never overshoots `target`, monotone while approaching,
    /// and snaps exactly once within 0.5px.
    pub fn step(&mut self, mut pos: f32, target: f32, mut frames: f32) -> f32 {
        let grew = self.last_target.map_or(0.0, |last| target - last);
        self.last_target = Some(target);
        if grew < -1.0 {
            // Target shrank (row collapse/removal) — growth estimate is stale.
            self.target_vel = 0.0;
        } else {
            let observed = grew.max(0.0) / frames.max(0.25);
            self.target_vel += SPRING_GROWTH_EMA * (observed - self.target_vel);
        }
        let chase = target - (self.target_vel * 9.0).min(SPRING_CHASE_MAX_LEAD);
        let mut v = self.velocity;
        while frames > 0.0 {
            let h = frames.min(1.0);
            frames -= h;
            let diff = (chase - pos).max(0.0);
            v += h * ((SPRING_DAMPING * v + SPRING_STIFFNESS * diff) / SPRING_MASS - v);
            pos = (pos + (v + self.target_vel) * h).min(target);
        }
        self.velocity = v;
        if target - pos <= 0.5 { target } else { pos }
    }
}

// ---------------------------------------------------------------------------
// Row model (pure)
// ---------------------------------------------------------------------------

/// One tool invocation inside a group row.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolItem {
    pub id: SharedString,
    pub call: ToolCall,
    pub is_error: bool,
    pub resolved: bool,
    pub diff_ref: Option<SharedString>,
    pub diff_stats: Option<Arc<Vec<ToolDiffStat>>>,
}

/// A source-pair request is scoped to its authoritative chat and immutable
/// sidecar reference, so a delayed result cannot overwrite a newer row.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ToolDiffFetchKey {
    owner: ServerRef,
    part_id: String,
    diff_ref: String,
}

enum ToolDiffFetchState {
    Loading {
        generation: u64,
    },
    Ready {
        diff: Arc<ToolDiff>,
        file: Arc<crate::changes::FileDiff>,
    },
    Unavailable,
}

/// Internal reasons a sidecar cannot become a visible detail. The UI always
/// presents one safe message, while tracing retains the diagnostic category.
#[derive(Debug, PartialEq, Eq)]
enum ToolDiffValidationFailure {
    NotAvailable,
    ChecksumCalculation { error: String },
    ChecksumMismatch { expected: String, actual: String },
    SourceMismatch,
}

fn tool_item_from_part(part: &MessagePart) -> Option<ToolItem> {
    let MessagePart::Tool {
        id,
        call,
        is_error,
        resolved,
        diff_ref,
        diff_stats,
    } = part
    else {
        return None;
    };
    Some(ToolItem {
        id: id.clone().into(),
        call: call.clone(),
        is_error: *is_error,
        resolved: *resolved,
        diff_ref: diff_ref.clone().map(Into::into),
        diff_stats: diff_stats.clone().map(Arc::new),
    })
}

fn tool_diff_reply_is_current(
    selected_owner: Option<&ServerRef>,
    states: &HashMap<ToolDiffFetchKey, ToolDiffFetchState>,
    key: &ToolDiffFetchKey,
    generation: u64,
) -> bool {
    selected_owner == Some(&key.owner)
        && matches!(
            states.get(key),
            Some(ToolDiffFetchState::Loading {
                generation: current,
            }) if *current == generation
        )
}

/// A retained Ready or Unavailable result is terminal for this immutable
/// sidecar reference, so reopening the detail reuses it rather than starting
/// a second request.
fn tool_diff_fetch_needs_start(
    fetches: &HashMap<ToolDiffFetchKey, ToolDiffFetchState>,
    key: &ToolDiffFetchKey,
) -> bool {
    !fetches.contains_key(key)
}

/// Complete one sidecar request at the non-GPUI boundary shared by the task
/// callback and focused tests. A task removes only its own generation; then a
/// late owner/key/generation result leaves the current fetch state unchanged.
fn complete_tool_diff_fetch<T>(
    selected_owner: Option<&ServerRef>,
    fetches: &mut HashMap<ToolDiffFetchKey, ToolDiffFetchState>,
    tasks: &mut HashMap<ToolDiffFetchKey, (u64, T)>,
    key: ToolDiffFetchKey,
    generation: u64,
    resolved: Option<(Arc<ToolDiff>, Arc<crate::changes::FileDiff>)>,
) -> bool {
    if tasks
        .get(&key)
        .is_some_and(|(task_generation, _)| *task_generation == generation)
    {
        tasks.remove(&key);
    }
    if !tool_diff_reply_is_current(selected_owner, fetches, &key, generation) {
        return false;
    }
    let state = match resolved {
        Some((diff, file)) => ToolDiffFetchState::Ready { diff, file },
        None => ToolDiffFetchState::Unavailable,
    };
    fetches.insert(key, state);
    true
}

/// Omit precisely the one line terminator that `similar::TextDiff::from_lines`
/// leaves attached to each change value. This preserves other trailing text,
/// including an intentional second newline, while keeping visible rows in the
/// same form as complete-source validation.
fn without_line_terminator(value: &str) -> &str {
    value
        .strip_suffix("\r\n")
        .or_else(|| value.strip_suffix('\n'))
        .or_else(|| value.strip_suffix('\r'))
        .unwrap_or(value)
}

/// Unified diff represents an empty range at the preceding line rather than
/// after it: a new file starts at `-0,0`, while a deleted file starts at
/// `+0,0`.
fn unified_hunk_range(range: &Range<usize>) -> (usize, usize) {
    let len = range.len();
    let start = if len == 0 {
        range.start
    } else {
        range.start + 1
    };
    (start, len)
}

fn validate_tool_diff_reference(
    expected: &str,
    actual: serde_json::Result<String>,
) -> Result<(), ToolDiffValidationFailure> {
    let actual = actual.map_err(|error| ToolDiffValidationFailure::ChecksumCalculation {
        error: error.to_string(),
    })?;
    if actual == expected {
        Ok(())
    } else {
        Err(ToolDiffValidationFailure::ChecksumMismatch {
            expected: expected.to_owned(),
            actual,
        })
    }
}

fn validate_tool_diff_sources(
    diff: &ToolDiff,
    file: &crate::changes::FileDiff,
) -> Result<(), ToolDiffValidationFailure> {
    crate::changes::sources_match_diff(file, diff.old_text.as_deref(), Some(&diff.new_text))
        .then_some(())
        .ok_or(ToolDiffValidationFailure::SourceMismatch)
}

/// Convert the complete pair into the changes pane's neutral row model. The
/// line numbers come from `similar` rather than byte offsets, keeping Unicode
/// source text and the displayed diff in the same coordinate system.
fn diff_to_file(diff: &ToolDiff) -> crate::changes::FileDiff {
    use crate::changes::{DiffLine, FileDiff, FileStatus, Hunk, LineKind};

    let old = diff.old_text.as_deref().unwrap_or("");
    let text_diff = similar::TextDiff::from_lines(old, &diff.new_text);
    let mut hunks = Vec::new();
    let (mut additions, mut deletions) = (0u32, 0u32);
    let mut max_line = 0u32;
    for group in text_diff.grouped_ops(3) {
        let (Some(first), Some(last)) = (group.first(), group.last()) else {
            continue;
        };
        let old_range = first.old_range().start..last.old_range().end;
        let new_range = first.new_range().start..last.new_range().end;
        let (old_start, old_len) = unified_hunk_range(&old_range);
        let (new_start, new_len) = unified_hunk_range(&new_range);
        let header = format!(
            "@@ -{},{} +{},{} @@",
            old_start, old_len, new_start, new_len,
        );
        let mut lines = Vec::new();
        for op in &group {
            for change in text_diff.iter_changes(op) {
                let kind = match change.tag() {
                    similar::ChangeTag::Delete => {
                        deletions += 1;
                        LineKind::Del
                    }
                    similar::ChangeTag::Insert => {
                        additions += 1;
                        LineKind::Add
                    }
                    similar::ChangeTag::Equal => LineKind::Context,
                };
                let old_no = change.old_index().map(|line| line as u32 + 1);
                let new_no = change.new_index().map(|line| line as u32 + 1);
                max_line = max_line.max(old_no.unwrap_or(0)).max(new_no.unwrap_or(0));
                lines.push(DiffLine {
                    kind,
                    old_no,
                    new_no,
                    text: without_line_terminator(change.value()).to_owned(),
                });
                if change.missing_newline() {
                    lines.push(DiffLine {
                        kind: LineKind::Meta,
                        old_no: None,
                        new_no: None,
                        text: "\\ No newline at end of file".into(),
                    });
                }
            }
        }
        hunks.push(Hunk { header, lines });
    }
    FileDiff {
        path: diff.path.clone(),
        old_path: None,
        status: if diff.old_text.is_none() {
            FileStatus::Added
        } else if diff.new_text.is_empty() {
            FileStatus::Deleted
        } else {
            FileStatus::Modified
        },
        binary: false,
        notices: Vec::new(),
        hunks,
        additions,
        deletions,
        max_line,
    }
}

fn validate_tool_diff_reply(
    expected_ref: &str,
    reply: ReadToolDiffReply,
) -> Result<(ToolDiff, crate::changes::FileDiff), ToolDiffValidationFailure> {
    let diff = match reply {
        ReadToolDiffReply::Available { diff } => diff,
        ReadToolDiffReply::NotAvailable => return Err(ToolDiffValidationFailure::NotAvailable),
    };
    validate_tool_diff_reference(expected_ref, diff.diff_ref())?;
    let file = diff_to_file(&diff);
    validate_tool_diff_sources(&diff, &file)?;
    Ok((diff, file))
}

fn log_tool_diff_validation_failure(key: &ToolDiffFetchKey, failure: &ToolDiffValidationFailure) {
    match failure {
        ToolDiffValidationFailure::NotAvailable => {
            tracing::warn!(
                owner = ?key.owner,
                part_id = %key.part_id,
                category = "not_available",
                "tool diff sidecar is not available"
            );
        }
        ToolDiffValidationFailure::ChecksumCalculation { error } => {
            tracing::warn!(
                owner = ?key.owner,
                part_id = %key.part_id,
                category = "checksum_calculation",
                error = %error,
                "tool diff checksum calculation failed"
            );
        }
        ToolDiffValidationFailure::ChecksumMismatch { expected, actual } => {
            tracing::warn!(
                owner = ?key.owner,
                part_id = %key.part_id,
                category = "checksum_mismatch",
                expected_ref = %expected,
                actual_ref = %actual,
                "tool diff checksum did not match the transcript reference"
            );
        }
        ToolDiffValidationFailure::SourceMismatch => {
            tracing::warn!(
                owner = ?key.owner,
                part_id = %key.part_id,
                category = "source_mismatch",
                "tool diff complete sources did not match the rendered rows"
            );
        }
    }
}

/// The fixed analytic height of an open per-tool detail, including its wrapper
/// padding. A missing fetch state paints the same loading row as `Loading`.
fn tool_diff_detail_height(state: Option<&ToolDiffFetchState>) -> f32 {
    TOOL_DIFF_DETAIL_TOP_PAD
        + match state {
            Some(ToolDiffFetchState::Ready { file, .. }) => crate::changes::body_height(file),
            Some(ToolDiffFetchState::Loading { .. })
            | Some(ToolDiffFetchState::Unavailable)
            | None => TOOL_DIFF_STATUS_HEIGHT,
        }
}

/// The approval card's paint discriminator — the ONLY thing that may vary by
/// decision (`.agents/rules/gpui-ui.md`: layout constants never depend on
/// which color is painted). Carries the decision KIND, not the decision
/// itself, because `Allow` and `AllowForSession` are both the user saying
/// yes and paint identically; only `approval_card`'s match arm needs to know
/// which of the four looks to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalPaint {
    /// No decision recorded yet.
    Open,
    /// `Allow` or `AllowForSession`.
    Allowed,
    /// A choice the user made, not a failure — never painted `danger`/red.
    Denied,
    /// Host-stamped because the run ended with this still open.
    Expired,
}

impl ApprovalPaint {
    fn of(decision: Option<&comet_proto::ApprovalDecision>) -> Self {
        use comet_proto::ApprovalDecision;
        match decision {
            None => ApprovalPaint::Open,
            Some(ApprovalDecision::Allow) | Some(ApprovalDecision::AllowForSession) => {
                ApprovalPaint::Allowed
            }
            Some(ApprovalDecision::Deny { .. }) => ApprovalPaint::Denied,
            Some(ApprovalDecision::Expired) => ApprovalPaint::Expired,
        }
    }
}

/// The subagent card's paint discriminator, on `ApprovalPaint`'s precedent and
/// for its reason: the only thing that may vary by state is colour and glyph,
/// never a layout number (`.agents/rules/gpui-ui.md`).
///
/// `LastSeenRunning` is NOT a `SubagentStatus` — no such status exists, and
/// inventing one would reach `PROTOCOL_VERSION`. It is the reading a card gets
/// when its own status still says `Running` while the entry around it has
/// finished; see [`subagent_row_state`] for why that happens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentPaint {
    Running,
    Completed,
    Failed,
    Cancelled,
    LastSeenRunning,
}

#[derive(Clone)]
pub enum RowKind {
    User {
        /// Visible prompt (attachment-ref trailer already stripped). When the
        /// prompt carries file mentions this is the *projected* display text —
        /// chip labels in place of the raw Markdown links.
        text: SharedString,
        /// File-mention chips over `text`, in display-byte terms. Computed
        /// once per entry change in [`rows_for_entry`] (rows are cached by
        /// fingerprint), never per frame. Empty for ordinary prompts.
        mentions: Arc<Vec<crate::composer::SentMentionSpan>>,
        /// Image refs parsed out of the message text (message-attachments.ts):
        /// thumbnails load from the owning device via ReadAttachmentChunk.
        attachments: Arc<Vec<crate::attachments::UserImageAttachment>>,
        /// Context the prompt folded in as text, lifted back out by `badges`.
        badges: Arc<Vec<crate::badges::MessageBadge>>,
        /// Optimistic echo not yet confirmed by a doc frame.
        pending: bool,
    },
    /// One top-level markdown block of a completed message.
    Markdown {
        tree: Arc<BlockTree>,
        block_ix: usize,
    },
    /// One top-level block of a STREAMING message. Split per block like
    /// completed rows (only the tail blocks' versions change per commit, so
    /// the settled prefix is never respliced or re-rendered); rendered with
    /// the fade veil.
    LiveMarkdown {
        tree: Arc<BlockTree>,
        block_ix: usize,
    },
    ToolGroup {
        tools: Arc<Vec<ToolItem>>,
        auto_open: bool,
    },
    InputChip {
        /// First question's header (chat-view.tsx `InputChip`: the resolved
        /// chip shows it; unresolved shows "Awaiting your answer…" — which
        /// stays TRUE even across a run death: the composer keeps the panel
        /// up until the user answers, and the engine delivers a dead run's
        /// answer as a resumed turn).
        header: SharedString,
        resolved: bool,
    },
    /// An approval the provider is blocked on. Like `InputChip` this is
    /// PASSIVE — the decision controls live in the composer — but it is a card
    /// rather than a chip because the thing being asked (a command, a path and
    /// its counts) does not survive sharing one 34px line with a label.
    ApprovalCard {
        /// Comet's name for the action ("Run a command", "Edit a file").
        label: &'static str,
        /// The one-line body: the command, the path + counts, the server/tool.
        detail: SharedString,
        /// The terminal caption, once decided. `None` while open.
        state: Option<SharedString>,
        /// Paint-only discriminator for the decision (or its absence).
        /// Changes colour and icon, never layout.
        paint: ApprovalPaint,
    },
    ErrorChip {
        message: SharedString,
    },
    /// D14's residual: the reason the user typed when denying the
    /// `ApprovalCard` directly above it. Its own row rather than a field ON
    /// the card — the card's 56px is fixed in every state precisely so a
    /// decision landing cannot reflow the transcript (`approval_card`'s own
    /// doc comment), and a note can be arbitrarily long. A sibling row keeps
    /// that invariant intact while still keeping the user's own words durably
    /// in the transcript, where the composer's decision row only ever held
    /// them transiently.
    DenyNote {
        message: SharedString,
    },
    NoticeChip {
        summary: SharedString,
        /// Hover tooltip; already suppressed when it would duplicate `summary`.
        detail: Option<SharedString>,
        severity: NoticeSeverity,
        occurrences: u32,
    },
    /// One delegated agent. Like `ApprovalCard` this is PASSIVE — a subagent
    /// asks the user for nothing, so nothing here reaches the composer.
    ///
    /// Every field is resolved in [`rows_for_entry`], never at paint time:
    /// the render arm picks colours and lays out, and makes no decisions about
    /// what the card is allowed to say.
    SubagentCard {
        /// Straight off the wire ("Explore", "general-purpose"); never looked
        /// up in the discovery handshake's `agents` catalogue (D31).
        agent_type: SharedString,
        description: SharedString,
        /// The state word in the top-right ("running", "last seen running").
        status_caption: SharedString,
        paint: SubagentPaint,
        /// The live line, and `Some` ONLY while the card is genuinely live.
        /// Two independent rules blank it — see [`subagent_row_state`].
        activity: Option<SharedString>,
        /// The child's answer, on completion. Folds to two lines.
        summary: Option<SharedString>,
        /// The quiet explanatory line a terminal state carries instead of a
        /// live one. Comet copy, never provider text.
        caption: Option<SharedString>,
        /// Pre-joined "20,115 tokens · 4.9s · 4 tools", omitting every counter
        /// the provider did not report. `None` when it reported none of them.
        counters: Option<SharedString>,
    },
    /// The plan the agent published for this run (slice 4.4).
    ///
    /// One card per RUN, sitting where the plan was first published and moving
    /// as its steps move. It is not pinned and does not follow the viewport:
    /// every message starts a fresh run and therefore a fresh card, so the only
    /// window in which the plan is out of sight is inside one long turn.
    ChecklistCard {
        /// Codex's one-line rationale for the latest change. Claude sends none
        /// and none is synthesized for it, so `None` here is ordinary.
        explanation: Option<SharedString>,
        done: usize,
        steps: Arc<Vec<ChecklistRow>>,
    },
}

/// One drawn step of a plan.
#[derive(Clone, PartialEq)]
pub struct ChecklistRow {
    pub label: SharedString,
    /// True when the provider named this step neither way and the label is
    /// Comet's placeholder — the row is drawn quieter, and it is NOT an error
    /// state. See [`checklist_label`].
    pub unnamed: bool,
    pub status: ChecklistStatus,
}

/// A step's visible label.
///
/// `text` is the step as the agent phrased it; `active_form` is its
/// present-participle twin ("Counting lines" vs "Count the lines"), and on a
/// resumed run it is often the ONLY human-readable text an item has — a
/// resumed Claude process restates nothing, so a step can be first sighted by
/// a bare status change carrying neither. That last case is real, not
/// defensive: `COMET_MOCK_CHECKLIST` emits it deliberately, and a card that
/// assumed every row has a subject would draw a blank line.
fn checklist_label(item: &comet_proto::ChecklistItem) -> (SharedString, bool) {
    match item
        .text
        .as_deref()
        .filter(|t| !t.trim().is_empty())
        .or_else(|| item.active_form.as_deref().filter(|t| !t.trim().is_empty()))
    {
        Some(label) => (SharedString::from(single_line(label)), false),
        None => (SharedString::from("Unnamed step"), true),
    }
}

/// A transcript row: stable id + content version (diff key) + block payload.
#[derive(Clone)]
pub struct Row {
    pub id: SharedString,
    pub version: u64,
    /// First row of its message entry (gets the turn gap).
    pub turn_start: bool,
    pub kind: RowKind,
    /// The owning message entry — hover anywhere on the entry's rows reveals
    /// its timestamp strip (comet chat-view.tsx `group`/`group-hover`).
    pub entry_id: SharedString,
    /// Epoch-ms for the 16px hover-timestamp strip UNDER this row: set on the
    /// LAST row of a completed entry (user rows always; assistant rows only
    /// once streaming ends — "the turn isn't at a time yet", chat-view.tsx).
    pub timestamp: Option<i64>,
}

/// Absolute hover-timestamp label, e.g. "Jul 1, 3:45 PM" — the exact
/// `formatTimestamp` shape (utils.ts: short month, numeric day, hour,
/// 2-digit minutes, no leading zero on the hour). Pure over an explicit
/// timezone so tests don't depend on the host's local time.
pub fn format_timestamp<Tz: chrono::TimeZone>(ms: i64, tz: &Tz) -> String
where
    Tz::Offset: std::fmt::Display,
{
    match chrono::DateTime::from_timestamp_millis(ms) {
        Some(utc) => utc
            .with_timezone(tz)
            .format("%b %-d, %-I:%M %p")
            .to_string(),
        None => String::new(),
    }
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x1_0000_01b3);
    }
    hash
}

fn tool_fingerprint(tools: &[ToolItem], auto_open: bool) -> u64 {
    let mut acc = Vec::with_capacity(tools.len() * 8 + 1);
    for t in tools {
        let (label, detail) = tool_chip_content(&t.call);
        acc.extend_from_slice(t.id.as_bytes());
        acc.extend_from_slice(label.as_bytes());
        acc.extend_from_slice(detail.as_bytes());
        acc.push(t.is_error as u8 | (t.resolved as u8) << 1);
        if let Some(diff_ref) = &t.diff_ref {
            acc.extend_from_slice(diff_ref.as_bytes());
        }
        if let Some(stats) = &t.diff_stats {
            for stat in stats.iter() {
                acc.extend_from_slice(stat.path.as_bytes());
                acc.extend_from_slice(&stat.additions.to_le_bytes());
                acc.extend_from_slice(&stat.deletions.to_le_bytes());
            }
        }
    }
    acc.push(auto_open as u8);
    fnv1a(&acc)
}

/// Build the block rows of one (already continuation-joined) entry.
///
/// `parse` maps `(part_key, text)` to a block tree — the entity supplies
/// incremental parsers for live parts and a cache for complete ones; tests pass
/// a plain `parse_full`.
/// Thousands separators without pulling in a formatting crate — the only
/// grouped number on either new card.
fn grouped(n: u64) -> String {
    let raw = n.to_string();
    let mut out = String::with_capacity(raw.len() + raw.len() / 3);
    for (i, c) in raw.chars().enumerate() {
        if i > 0 && (raw.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A subagent's elapsed time, at the precision a person reads at a glance.
fn subagent_duration(ms: u64) -> String {
    if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{}m {}s", ms / 60_000, (ms % 60_000) / 1000)
    }
}

/// The counters line, omitting every counter the provider did not report.
///
/// `None` is "not reported yet", never zero (`AgentEvent::SubagentUpdated`'s
/// own doc) — so an absent counter is DROPPED rather than printed as `0`. A
/// card reading `0 tokens` for an agent that simply never reported would be a
/// lie the `Option` was chosen to prevent. Returns `None` when nothing at all
/// was reported, so the row carries no empty line.
fn subagent_counters(
    total_tokens: Option<u64>,
    duration_ms: Option<u64>,
    tool_uses: Option<u32>,
) -> Option<SharedString> {
    let mut parts: Vec<String> = Vec::with_capacity(3);
    if let Some(tokens) = total_tokens {
        parts.push(format!("{} tokens", grouped(tokens)));
    }
    if let Some(ms) = duration_ms {
        parts.push(subagent_duration(ms));
    }
    if let Some(uses) = tool_uses {
        parts.push(format!(
            "{uses} {}",
            if uses == 1 { "tool" } else { "tools" }
        ));
    }
    (!parts.is_empty()).then(|| SharedString::from(parts.join(" · ")))
}

/// What a subagent card is allowed to say, given its own status and whether
/// the entry around it has finished.
///
/// Returns `(paint, status caption, live activity, quiet caption)`.
///
/// **Two independent rules blank the activity line, and neither subsumes the
/// other.**
///
/// 1. A TERMINAL status blanks it (D53). The `SubagentUpdated` fold overwrites
///    `activity` only when the new reading carries one, so a `task_updated` or
///    `task_notification` reporting no activity leaves the last live line
///    standing in the part. Rendering `activity` whenever it is `Some` would
///    print "Reading normalize.rs" under a finished agent forever.
/// 2. A FINISHED ENTRY blanks it even while the status still says `Running`
///    (D57). Send a new message while an agent is working and Comet passes it
///    to the CLI without stopping that agent; the accumulator is cleared on
///    `Steered`, so every later update for that `task_id` is dropped and the
///    part is frozen at its last reading. The card reports where it froze —
///    `last seen running` — and never an outcome: `cancelled` would assert
///    something nobody observed, `completed` would be a guess.
fn subagent_row_state(
    status: SubagentStatus,
    activity: Option<&str>,
    entry_finished: bool,
) -> (
    SubagentPaint,
    &'static str,
    Option<SharedString>,
    Option<SharedString>,
) {
    match status {
        SubagentStatus::Running if entry_finished => (
            SubagentPaint::LastSeenRunning,
            "last seen running",
            None,
            // Covers BOTH routes into this state, which the card cannot tell
            // apart and should not try to: a steer (D57), and a turn that
            // completed cleanly while the agent was still working — Claude's
            // `Agent` tool is not synchronous with its parent's turn. Naming
            // the steer specifically, as this line first did, was wrong the
            // moment a real run took the second route.
            Some("Still running when the turn ended. Comet never saw how it finished.".into()),
        ),
        SubagentStatus::Running => (
            SubagentPaint::Running,
            "running",
            activity.map(|a| SharedString::from(single_line(a))),
            None,
        ),
        SubagentStatus::Completed => (SubagentPaint::Completed, "completed", None, None),
        SubagentStatus::Failed => (
            SubagentPaint::Failed,
            "failed",
            None,
            Some("The agent stopped before it reported anything back.".into()),
        ),
        SubagentStatus::Cancelled => (SubagentPaint::Cancelled, "cancelled", None, None),
    }
}

/// True for the contentless chip Claude's `Agent` call renders as today.
///
/// The call has no decode arm and falls through to `ToolCall::Unknown`
/// (`comet_harness::claude::normalize`), and `sanitize_tool_call` strips
/// `Unknown`'s input before the part enters the document — so the name is
/// genuinely all that survives, and all there is to match on. See the
/// suppression comment in [`rows_for_entry`].
fn is_agent_spawn_chip(part: &MessagePart) -> bool {
    matches!(
        part,
        MessagePart::Tool {
            call: ToolCall::Unknown { name, .. },
            ..
        } if name == "Agent"
    )
}

/// D14's residual: the text (if any) that earns its own `DenyNote` row beside
/// a decided `ApprovalCard`. `Some` only for `Deny`, and for whatever message
/// it carries — including the composer's own "the user declined this action"
/// default when no note was typed. That default is still exactly what was
/// sent to the model, so showing it is the honest answer; guessing which
/// messages count as "a real note" would need a second source of truth this
/// part does not carry.
fn deny_note_text(decision: Option<&comet_proto::ApprovalDecision>) -> Option<SharedString> {
    match decision {
        Some(comet_proto::ApprovalDecision::Deny { message }) => {
            Some(SharedString::from(single_line(message)))
        }
        _ => None,
    }
}

pub fn rows_for_entry(
    entry: &SessionMessageEntry,
    pending: bool,
    parse: &mut dyn FnMut(&str, &str) -> Arc<BlockTree>,
) -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::new();
    let streaming = entry.status == Some(MessageStatus::Streaming);
    let entry_id: SharedString = entry.id.clone().into();

    if entry.role == MessageRole::User {
        let raw: String = entry
            .parts
            .iter()
            .filter_map(|p| match p {
                MessagePart::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        // Attachment refs ride the plain text (the `withAttachments`
        // transport); split them back out for the thumbnail strip.
        let parsed = crate::attachments::parse_user_message_images(&raw);
        // File mentions render as chips here too, not just in the composer.
        // The projection is pure over the text, so the raw-length row version
        // below stays a valid cache/diff key.
        // Lifted before the mention projection, so a comment body's own
        // Markdown never lands in the bubble.
        let (body, badges) = crate::badges::split(&parsed.text);
        let (text, mentions) = match crate::composer::sent_mention_display(&body) {
            Some((display, spans)) => (display, spans),
            None => (body, Vec::new()),
        };
        return vec![Row {
            id: entry.id.clone().into(),
            version: (raw.len() as u64) << 1 | pending as u64,
            turn_start: true,
            kind: RowKind::User {
                text: text.into(),
                mentions: Arc::new(mentions),
                attachments: Arc::new(parsed.attachments),
                badges: Arc::new(badges),
                pending,
            },
            entry_id,
            // User rows always carry the strip (chat-view.tsx: whenever
            // `createdAt` exists — the optimistic echo included).
            timestamp: Some(entry.created_at),
        }];
    }

    // Assistant/system: split parts into block rows, folding consecutive tools.
    let last_part_ix = entry.parts.len().saturating_sub(1);
    let mut group_ix = 0usize;
    let mut pending_group: Vec<ToolItem> = Vec::new();
    let mut group_last_part_ix = 0usize;

    let flush_group =
        |rows: &mut Vec<Row>, group: &mut Vec<ToolItem>, group_ix: &mut usize, last_ix: usize| {
            if group.is_empty() {
                return;
            }
            let tools = std::mem::take(group);
            let auto_open = streaming && last_ix == last_part_ix;
            rows.push(Row {
                id: format!("{}#g{}", entry.id, group_ix).into(),
                version: tool_fingerprint(&tools, auto_open),
                turn_start: false,
                kind: RowKind::ToolGroup {
                    tools: Arc::new(tools),
                    auto_open,
                },
                entry_id: entry.id.clone().into(),
                timestamp: None,
            });
            *group_ix += 1;
        };

    // A delegation would otherwise draw TWICE: once as the contentless `Agent`
    // tool chip, and once as the subagent card below. The two cannot be joined
    // — `tool_use_id` is dropped when the part is built (`doc::parts`'s
    // `SubagentStarted` arm) and `sanitize_tool_call` strips the chip's input
    // before it reaches the document, so the persisted chip is the bare string
    // "Agent".
    //
    // So pair them POSITIONALLY and entry-locally: suppress as many `Agent`
    // chips as this entry has cards, in fold order. Claude emits the `Agent`
    // tool_use before its `task_started`, so in practice this pairs exactly.
    // It fails OPEN by construction — the budget is bounded by the card count,
    // so a delegation that never produced a card keeps its chip rather than
    // vanishing. `split_parts` can also strand a chip and its card in separate
    // continuation entries, which under-suppresses; that is the harmless
    // direction and is left alone.
    let mut agent_chip_budget = entry
        .parts
        .iter()
        .filter(|p| matches!(p, MessagePart::Subagent { .. }))
        .count();

    for (part_ix, part) in entry.parts.iter().enumerate() {
        if agent_chip_budget > 0 && is_agent_spawn_chip(part) {
            agent_chip_budget -= 1;
            continue;
        }
        if let Some(tool) = tool_item_from_part(part) {
            pending_group.push(tool);
            group_last_part_ix = part_ix;
            continue;
        }
        flush_group(
            &mut rows,
            &mut pending_group,
            &mut group_ix,
            group_last_part_ix,
        );
        match part {
            MessagePart::Text { id: part_id, text } => {
                if text.trim().is_empty() {
                    continue;
                }
                let key = format!("{}#{}", entry.id, part_id);
                let tree = parse(&key, text);
                // Live and completed parts split identically — one row
                // per top-level block, same ids, so the live→complete
                // handoff never changes row identity. The version is a
                // content hash of the block's bytes (LSB = streaming),
                // so a commit only splices rows whose bytes actually
                // changed — the settled prefix of a live reply is
                // untouched (and its render caches stay valid).
                for block_ix in 0..tree.blocks.len() {
                    let range = &tree.blocks[block_ix].range;
                    let end = range.end.min(text.len());
                    let bytes = text
                        .as_bytes()
                        .get(range.start.min(end)..end)
                        .unwrap_or_default();
                    let version = (fnv1a(bytes) << 1) | streaming as u64;
                    rows.push(Row {
                        id: format!("{key}.{block_ix}").into(),
                        version,
                        turn_start: false,
                        entry_id: entry_id.clone(),
                        timestamp: None,
                        kind: if streaming {
                            RowKind::LiveMarkdown {
                                tree: tree.clone(),
                                block_ix,
                            }
                        } else {
                            RowKind::Markdown {
                                tree: tree.clone(),
                                block_ix,
                            }
                        },
                    });
                }
            }
            MessagePart::Input {
                id: part_id,
                questions,
                resolved,
                ..
            } => {
                // Model-generated header onto the one-line chip.
                let header: SharedString = single_line(
                    &questions
                        .first()
                        .map(|q| q.header.clone())
                        .unwrap_or_else(|| "Question".to_string()),
                )
                .into();
                rows.push(Row {
                    id: format!("{}#{}", entry.id, part_id).into(),
                    version: fnv1a(header.as_bytes()) << 1 | *resolved as u64,
                    turn_start: false,
                    kind: RowKind::InputChip {
                        header,
                        resolved: *resolved,
                    },
                    entry_id: entry_id.clone(),
                    timestamp: None,
                });
            }
            MessagePart::Error {
                id: part_id,
                message,
            } => {
                rows.push(Row {
                    id: format!("{}#{}", entry.id, part_id).into(),
                    version: message.len() as u64,
                    turn_start: false,
                    kind: RowKind::ErrorChip {
                        // Harness-generated; the chip is one line.
                        message: single_line(message).into(),
                    },
                    entry_id: entry_id.clone(),
                    timestamp: None,
                });
            }
            // `tool_item_from_part` handles every tool before this arm.
            MessagePart::Tool { .. } => unreachable!("tool part was not adapted"),
            MessagePart::Approval {
                id: part_id,
                approval,
                decision,
                ..
            } => {
                let (label, detail) = comet_proto::view::approval_chip_content(approval);
                let state = decision
                    .as_ref()
                    .map(|d| SharedString::from(comet_proto::view::approval_decision_label(d)));
                let paint = ApprovalPaint::of(decision.as_ref());
                let mut fp = detail.as_bytes().to_vec();
                fp.extend_from_slice(label.as_bytes());
                if let Some(state) = &state {
                    fp.extend_from_slice(state.as_bytes());
                }
                rows.push(Row {
                    id: format!("{}#{}", entry.id, part_id).into(),
                    // The decision folds into the version: a card that
                    // resolves must repaint even though nothing else in
                    // the entry changed.
                    version: fnv1a(&fp),
                    turn_start: false,
                    kind: RowKind::ApprovalCard {
                        label,
                        detail: detail.into(),
                        state,
                        paint,
                    },
                    entry_id: entry_id.clone(),
                    timestamp: None,
                });
                // D14's residual: the deny note is a sibling row, never a
                // field on the card above — see `RowKind::DenyNote`.
                if let Some(note) = deny_note_text(decision.as_ref()) {
                    rows.push(Row {
                        id: format!("{}#{}-deny-note", entry.id, part_id).into(),
                        version: fnv1a(note.as_bytes()),
                        turn_start: false,
                        kind: RowKind::DenyNote { message: note },
                        entry_id: entry_id.clone(),
                        timestamp: None,
                    });
                }
            }
            MessagePart::Notice {
                id: part_id,
                severity,
                summary,
                detail,
                occurrences,
                ..
            } => {
                // A detail that restates the summary earns nothing on
                // hover (0.2a's duplicate-copy lesson) — drop it here.
                let detail: Option<SharedString> = detail
                    .as_ref()
                    .filter(|d| d.as_str() != summary.as_str())
                    .map(|d| SharedString::from(single_line(d)));
                let mut fp = summary.as_bytes().to_vec();
                if let Some(d) = &detail {
                    fp.extend_from_slice(d.as_bytes());
                }
                rows.push(Row {
                    id: format!("{}#{}", entry.id, part_id).into(),
                    // Occurrences folds into the version: a collapse
                    // that only bumps the counter must still repaint.
                    version: (fnv1a(&fp) << 1) ^ u64::from(*occurrences),
                    turn_start: false,
                    kind: RowKind::NoticeChip {
                        summary: SharedString::from(single_line(summary)),
                        detail,
                        severity: *severity,
                        occurrences: *occurrences,
                    },
                    entry_id: entry_id.clone(),
                    timestamp: None,
                });
            }
            MessagePart::Subagent {
                id: part_id,
                agent_type,
                description,
                status,
                activity,
                summary,
                total_tokens,
                duration_ms,
                tool_uses,
                ..
            } => {
                // A finished entry is the second of the two rules that blank
                // the live line — see `subagent_row_state`. `None` status is
                // NOT finished: an entry that never recorded one is not
                // evidence the turn ended.
                let entry_finished = matches!(
                    entry.status,
                    Some(MessageStatus::Complete) | Some(MessageStatus::Aborted)
                );
                let (paint, status_caption, activity, caption) =
                    subagent_row_state(*status, activity.as_deref(), entry_finished);
                let counters = subagent_counters(*total_tokens, *duration_ms, *tool_uses);
                // The child's answer is prose and may be long; the card folds
                // it. Kept multi-line here — the fold shows two lines and the
                // expansion wants the paragraphs it actually sent.
                let summary: Option<SharedString> =
                    summary.as_ref().map(|s| SharedString::from(s.clone()));

                let mut fp = agent_type.as_bytes().to_vec();
                fp.extend_from_slice(description.as_bytes());
                fp.extend_from_slice(status_caption.as_bytes());
                if let Some(a) = &activity {
                    fp.extend_from_slice(a.as_bytes());
                }
                if let Some(s) = &summary {
                    fp.extend_from_slice(s.as_bytes());
                }
                if let Some(c) = &counters {
                    fp.extend_from_slice(c.as_bytes());
                }
                rows.push(Row {
                    id: format!("{}#{}", entry.id, part_id).into(),
                    version: fnv1a(&fp),
                    turn_start: false,
                    kind: RowKind::SubagentCard {
                        agent_type: SharedString::from(single_line(agent_type)),
                        description: SharedString::from(single_line(description)),
                        status_caption: status_caption.into(),
                        paint,
                        activity,
                        summary,
                        caption,
                        counters,
                    },
                    entry_id: entry_id.clone(),
                    timestamp: None,
                });
            }
            MessagePart::Checklist {
                id: part_id,
                explanation,
                items,
            } => {
                let steps: Vec<ChecklistRow> = items
                    .iter()
                    .map(|item| {
                        let (label, unnamed) = checklist_label(item);
                        ChecklistRow {
                            label,
                            unnamed,
                            status: item.status,
                        }
                    })
                    .collect();
                let done = steps
                    .iter()
                    .filter(|s| s.status == ChecklistStatus::Completed)
                    .count();

                let mut fp: Vec<u8> = Vec::with_capacity(steps.len() * 16);
                if let Some(explanation) = explanation {
                    fp.extend_from_slice(explanation.as_bytes());
                }
                for step in &steps {
                    fp.extend_from_slice(step.label.as_bytes());
                    fp.push(step.unnamed as u8);
                    fp.push(match step.status {
                        ChecklistStatus::Pending => 0,
                        ChecklistStatus::InProgress => 1,
                        ChecklistStatus::Completed => 2,
                        ChecklistStatus::Unknown => 3,
                    });
                }
                rows.push(Row {
                    id: format!("{}#{}", entry.id, part_id).into(),
                    version: fnv1a(&fp),
                    turn_start: false,
                    kind: RowKind::ChecklistCard {
                        explanation: explanation
                            .as_ref()
                            .map(|e| SharedString::from(single_line(e))),
                        done,
                        steps: Arc::new(steps),
                    },
                    entry_id: entry_id.clone(),
                    timestamp: None,
                });
            }
        }
    }
    flush_group(
        &mut rows,
        &mut pending_group,
        &mut group_ix,
        group_last_part_ix,
    );

    if let Some(first) = rows.first_mut() {
        first.turn_start = true;
    }
    // Timestamp strip under the entry's LAST row once the turn has settled
    // (chat-view.tsx: "No timestamp hover mid-stream"). The version bit keeps
    // the diff key honest for last-row kinds whose own version wouldn't
    // change when streaming flips off (chips).
    if !streaming && let Some(last) = rows.last_mut() {
        last.timestamp = Some(entry.created_at);
        last.version ^= 1 << 62;
    }
    rows
}

/// `COMET_FRAME_STATS=1` logs live-row render-cost percentiles (p50/p95 µs
/// over rolling windows of [`FRAME_STATS_WINDOW`] samples) at `warn` level —
/// the smoothness measurement knob. Off by default; zero cost when off.
fn frame_stats_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED
        .get_or_init(|| std::env::var("COMET_FRAME_STATS").is_ok_and(|v| !v.is_empty() && v != "0"))
}

const FRAME_STATS_WINDOW: usize = 240;

/// `COMET_NO_RENDER_CACHE=1` bypasses the cross-frame flatten cache — the
/// A/B knob for the frame-cost measurement above.
fn render_cache_disabled() -> bool {
    static DISABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *DISABLED.get_or_init(|| {
        std::env::var("COMET_NO_RENDER_CACHE").is_ok_and(|v| !v.is_empty() && v != "0")
    })
}

fn record_live_frame_us(us: u64) {
    thread_local! {
        static SAMPLES: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
    }
    SAMPLES.with(|s| {
        let mut s = s.borrow_mut();
        s.push(us);
        if s.len() >= FRAME_STATS_WINDOW {
            s.sort_unstable();
            let p50 = s[s.len() / 2];
            let p95 = s[s.len() * 95 / 100];
            let max = *s.last().unwrap();
            tracing::warn!(
                n = s.len(),
                p50_us = p50,
                p95_us = p95,
                max_us = max,
                "live-row render cost"
            );
            s.clear();
        }
    });
}

/// How [`parse_for_row`] produced its tree — carries the incremental parser's
/// work counters so callers (and tests) can see that per-append parse work is
/// bounded by the reparsed tail, never the whole accumulated reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseOutcome {
    /// Streaming row: the live [`IncrementalParser`] advanced by one commit.
    Incremental {
        /// Bytes fed through `parse_full` for this commit (the reparse tail).
        parsed_bytes: usize,
        /// Leading top-level blocks left untouched (render caches stay valid).
        stable_prefix_blocks: usize,
    },
    /// Completed row served from the settled tree cache (no parse at all).
    Cached,
    /// Live→complete handoff: the live parser's exact tree was adopted.
    Handoff,
    /// Completed row parsed from scratch.
    Full,
}

/// The transcript's markdown parse wiring, extracted for testability: one call
/// per text part per sync. Streaming parts keep one [`IncrementalParser`] per
/// row key and advance it with the full accumulated text (`set_text` takes the
/// O(tail) append path for the prefix-extensions the doc watch delivers);
/// completed parts hit the settled cache, adopt the live parser's tree on the
/// live→complete flip (flicker-free handoff), or do one full parse.
pub fn parse_for_row(
    streaming: bool,
    key: &str,
    text: &str,
    live_parsers: &mut HashMap<String, IncrementalParser>,
    tree_cache: &mut HashMap<String, (usize, Arc<BlockTree>)>,
) -> (Arc<BlockTree>, ParseOutcome) {
    if streaming {
        let parser = live_parsers.entry(key.to_string()).or_default();
        parser.set_text(text);
        (
            // Display tree: hanging inline markers mended so closers arriving
            // later never reflow painted text (markdown/mend.rs). Completed
            // rows below use the canonical tree — the honest settle.
            Arc::new(parser.display_tree()),
            ParseOutcome::Incremental {
                parsed_bytes: parser.last_parse_bytes(),
                stable_prefix_blocks: parser.stable_prefix_blocks(),
            },
        )
    } else {
        if let Some((len, tree)) = tree_cache.get(key)
            && *len == text.len()
        {
            return (tree.clone(), ParseOutcome::Cached);
        }
        // On the live→complete flip reuse the live parser's tree when
        // the sources match — the split rows then share the exact tree
        // the unsplit row painted, guaranteeing a flicker-free handoff.
        let (tree, outcome) = match live_parsers.remove(key) {
            Some(parser) if parser.source() == text => {
                (Arc::new(parser.tree().clone()), ParseOutcome::Handoff)
            }
            _ => (Arc::new(parse_full(text)), ParseOutcome::Full),
        };
        tree_cache.insert(key.to_string(), (text.len(), tree.clone()));
        (tree, outcome)
    }
}

/// Markdown row ids are `{entry}#{part}.{blockIx}` — the part prefix is
/// everything before the block index.
fn part_prefix(id: &str) -> &str {
    id.rsplit_once('.').map(|(p, _)| p).unwrap_or(id)
}

/// Vertical gap opening `row` given its predecessor: turn gap at turn starts;
/// the markdown block gap between sibling block rows split from the same text
/// part — matching the live row's internal spacing exactly, so the
/// live→split handoff cannot shift a pixel; the block gap otherwise.
pub fn top_gap_for(prev: Option<&Row>, row: &Row) -> f32 {
    if row.turn_start {
        return GAP_TURN;
    }
    let is_md = |k: &RowKind| matches!(k, RowKind::Markdown { .. } | RowKind::LiveMarkdown { .. });
    let same_part_markdown = prev.is_some_and(|p| {
        is_md(&p.kind) && is_md(&row.kind) && part_prefix(&p.id) == part_prefix(&row.id)
    });
    if same_part_markdown {
        render::MD_BLOCK_GAP
    } else {
        GAP_BLOCK
    }
}

/// Minimal splice for a row-set change: `Some((old_range, new_count))`, or
/// `None` when the sets are identical by (id, version).
pub fn diff_rows(old: &[Row], new: &[Row]) -> Option<(Range<usize>, usize)> {
    let eq = |a: &Row, b: &Row| a.id == b.id && a.version == b.version;
    let mut prefix = 0usize;
    let max_prefix = old.len().min(new.len());
    while prefix < max_prefix && eq(&old[prefix], &new[prefix]) {
        prefix += 1;
    }
    if prefix == old.len() && prefix == new.len() {
        return None;
    }
    let mut suffix = 0usize;
    let max_suffix = (old.len() - prefix).min(new.len() - prefix);
    while suffix < max_suffix && eq(&old[old.len() - 1 - suffix], &new[new.len() - 1 - suffix]) {
        suffix += 1;
    }
    Some((prefix..old.len() - suffix, new.len() - suffix - prefix))
}

// ---------------------------------------------------------------------------
// Tool summaries / chips (pure)
// ---------------------------------------------------------------------------

/// The ToolGroup summary line — "Ran 3 commands · edited 2 files".
///
/// The rule lives in `comet_proto::view` so it remains presentation-independent;
/// this only adapts the row model's [`ToolItem`] to it.
pub fn tool_group_summary(tools: &[ToolItem]) -> String {
    let pairs: Vec<(ToolCall, bool)> = tools.iter().map(|t| (t.call.clone(), t.is_error)).collect();
    comet_proto::view::tool_group_summary(&pairs)
}

// `single_line` and the per-kind chip label/detail live in
// `comet_proto::view`: a tool must be named identically on every
// surface, and the one-line collapse is needed for the same reason in both (a
// literal newline breaks gpui's ellipsis logic and would be a cursor move in a
// cell grid).
pub use comet_proto::view::{single_line, tool_chip_content};

/// Analytic expanded-chips height — no measurement needed for the fold tween.
pub fn chips_height(count: usize) -> f32 {
    if count == 0 {
        return 0.0;
    }
    CHIPS_TOP_PAD + count as f32 * CHIP_HEIGHT + (count as f32 - 1.0) * CHIP_GAP
}

// ---------------------------------------------------------------------------
// Working indicator flavour (pure; rendered by the shell strip)
// ---------------------------------------------------------------------------

/// Rotating flavour vocabulary (20 words / 7s, seeded per chat).
pub const FLAVOUR_WORDS: [&str; 20] = [
    "Thinking",
    "Pondering",
    "Scheming",
    "Brewing",
    "Weaving",
    "Tinkering",
    "Musing",
    "Composing",
    "Sifting",
    "Untangling",
    "Distilling",
    "Sketching",
    "Plotting",
    "Riffing",
    "Combobulating",
    "Percolating",
    "Marinating",
    "Noodling",
    "Puzzling",
    "Conjuring",
];
pub const FLAVOUR_ROTATE_SECS: i64 = 7;

/// The flavour word for a seed at an elapsed time.
pub fn flavour_word(seed: u64, elapsed_secs: i64) -> &'static str {
    let step = (elapsed_secs.max(0) / FLAVOUR_ROTATE_SECS) as u64;
    FLAVOUR_WORDS[((seed.wrapping_add(step)) % FLAVOUR_WORDS.len() as u64) as usize]
}

/// A stable per-chat seed.
pub fn flavour_seed(chat_id: &str) -> u64 {
    fnv1a(chat_id.as_bytes())
}

/// "1m 32s"-style elapsed formatting.
pub fn format_elapsed(secs: i64) -> String {
    let secs = secs.max(0);
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m {}s", secs / 60, secs % 60)
    }
}

// ---------------------------------------------------------------------------
// Highlight store (background, time-sliced, paint-only)
// ---------------------------------------------------------------------------

struct HighlightEntry {
    key: DocumentHighlightKey,
    document: Option<Weak<comet_syntax::HighlightedDocument>>,
    cancellation: Arc<AtomicUsize>,
    _task: Option<Task<()>>,
}

impl Drop for HighlightEntry {
    fn drop(&mut self) {
        self.cancellation.store(1, Ordering::Relaxed);
    }
}

/// Cache of tokenized code blocks keyed by `(row id, block ix)`. Tokenization
/// runs on the background executor, time-sliced; results apply as paint-only
/// run colors when they land.
#[derive(Default)]
struct HighlightStore {
    entries: HashMap<(SharedString, usize), HighlightEntry>,
    cache: SyntaxHighlightCache,
}

impl HighlightStore {
    fn remember_cached_document(
        &mut self,
        slot_key: (SharedString, usize),
        document_key: DocumentHighlightKey,
        document: &Arc<comet_syntax::HighlightedDocument>,
    ) {
        self.entries.insert(
            slot_key,
            HighlightEntry {
                key: document_key,
                document: Some(Arc::downgrade(document)),
                cancellation: Arc::new(AtomicUsize::new(0)),
                _task: None,
            },
        );
    }

    /// Current tokens if ready; kicks a background tokenize when stale/missing.
    fn request(
        &mut self,
        row_id: SharedString,
        block_ix: usize,
        lang: Lang,
        code: &str,
        cx: &mut Context<Transcript>,
    ) -> Option<Arc<comet_syntax::HighlightedDocument>> {
        let slot_key = (row_id.clone(), block_ix);
        let document_key = DocumentHighlightKey::new(lang, code);
        if let Some(entry) = self.entries.get(&slot_key)
            && entry.key == document_key
        {
            let document = entry.document.as_ref()?;
            if let Some(document) = document.upgrade() {
                return Some(document);
            }
        }
        self.entries.remove(&slot_key);
        if let Some(document) = self.cache.get(&document_key) {
            self.remember_cached_document(slot_key, document_key, &document);
            return Some(document);
        }
        let code = code.to_string();
        let source_bytes = code.len();
        let cancellation = Arc::new(AtomicUsize::new(0));
        let background_cancellation = cancellation.clone();
        let apply_cancellation = cancellation.clone();
        let task = cx.spawn(async move |this, cx| {
            let started = Instant::now();
            let document = cx
                .background_executor()
                .spawn(async move { highlight_document(lang, &code, &background_cancellation) })
                .await;
            this.update(cx, |transcript, cx| {
                let is_current =
                    transcript
                        .highlights
                        .entries
                        .get(&slot_key)
                        .is_some_and(|entry| {
                            entry.key == document_key
                                && Arc::ptr_eq(&entry.cancellation, &apply_cancellation)
                        });
                if apply_cancellation.load(Ordering::Relaxed) == 0
                    && is_current
                    && let Some(document) = document
                {
                    let document = Arc::new(document);
                    let retained = transcript
                        .highlights
                        .cache
                        .insert(document_key, document.clone());
                    if let Some(entry) = transcript.highlights.entries.get_mut(&slot_key) {
                        tracing::debug!(
                            language = ?lang,
                            source_bytes,
                            spans = document.lines.iter().map(Vec::len).sum::<usize>(),
                            elapsed_us = started.elapsed().as_micros() as u64,
                            "syntax highlight ready"
                        );
                        entry.document = retained.then(|| Arc::downgrade(&document));
                        cx.notify();
                    }
                }
            })
            .ok();
        });
        self.entries.insert(
            (row_id, block_ix),
            HighlightEntry {
                key: document_key,
                document: None,
                cancellation,
                _task: Some(task),
            },
        );
        None
    }

    fn clear(&mut self) {
        self.entries.clear();
    }

    /// Dropping an entry cooperatively cancels any background parse it owns.
    fn retain_slots(&mut self, slots: &HashSet<(SharedString, usize)>) {
        self.entries.retain(|slot, _| slots.contains(slot));
    }
}

fn highlight_document(
    lang: Lang,
    code: &str,
    cancellation: &AtomicUsize,
) -> Option<comet_syntax::HighlightedDocument> {
    comet_syntax::highlight_with_limits(
        comet_syntax::HighlightRequest {
            source: code,
            path: None,
            fence_tag: Some(language_alias(lang)),
        },
        comet_syntax::HighlightLimits::default(),
        Some(cancellation),
    )
    .ok()
}

fn language_alias(language: Lang) -> &'static str {
    match language {
        Lang::Rust => "rust",
        Lang::JavaScript => "javascript",
        Lang::Jsx => "jsx",
        Lang::TypeScript => "typescript",
        Lang::Tsx => "tsx",
        Lang::Python => "python",
        Lang::Go => "go",
        Lang::Json => "json",
        Lang::Jsonc => "jsonc",
        Lang::Bash => "bash",
        Lang::Toml => "toml",
        Lang::Markdown => "markdown",
        Lang::Html => "html",
        Lang::Css => "css",
        Lang::Yaml => "yaml",
        Lang::C => "c",
        Lang::Cpp => "cpp",
        Lang::CSharp => "csharp",
        Lang::Java => "java",
        Lang::Kotlin => "kotlin",
        Lang::Swift => "swift",
        Lang::Ruby => "ruby",
        Lang::Php => "php",
        Lang::Sql => "sql",
        Lang::Lua => "lua",
        Lang::Dockerfile => "dockerfile",
        Lang::Nix => "nix",
        Lang::Make => "make",
    }
}

// ---------------------------------------------------------------------------
// Transcript entity
// ---------------------------------------------------------------------------

struct CachedRows {
    fingerprint: u64,
    rows: Vec<Row>,
}

#[derive(Default, Clone, Copy)]
struct FoldState {
    /// User pin (click); `None` follows the auto-open rule.
    open: Option<bool>,
    /// Bumped per toggle — keys the 200ms height tween.
    epoch: usize,
    /// Height at the moment of the toggle (the tween's start). The destination
    /// is always the *current* target height, so content growth after a toggle
    /// snaps instead of replaying a stale tween.
    from: f32,
    /// When the toggle happened. The tween is armed only for a short window
    /// after the click: gpui replays an element's animation on REMOUNT, and a
    /// virtualized row scrolling back into view is a remount — an armed-forever
    /// tween made every once-collapsed group flash open→closed on each
    /// reappearance (user report).
    toggled_at: Option<Instant>,
}

pub struct Transcript {
    state: Entity<AppState>,
    list: ListState,
    rows: Vec<Row>,
    chat_id: Option<ServerRef>,
    row_cache: HashMap<String, CachedRows>,
    live_parsers: HashMap<String, IncrementalParser>,
    tree_cache: HashMap<String, (usize, Arc<BlockTree>)>,
    folds: HashMap<SharedString, FoldState>,
    /// Streaming fade veils, one per live markdown row (dropped on completion).
    veils: HashMap<SharedString, Rc<RefCell<RowVeil>>>,
    /// Live rows present in the transcript's REPLAY after (re)attaching to a
    /// chat: their veils are created pre-seeded, so text that was already
    /// streamed before the switch never fades in — only appends after it do
    /// (mugen's `FadePainter.attach` baseline; user report: switching back to
    /// a streaming session dissolved the entire reply).
    veil_baseline: std::collections::HashSet<SharedString>,
    /// Armed at attach, disarmed on the first sync whose transcript is
    /// non-empty: the baseline must be captured from the doc REPLAY frame,
    /// not the attach-time sync — selection clears the transcript and the
    /// replay lands async, so capturing at attach seeded nothing and the
    /// still-streaming reply faded in whole on every session switch (user
    /// report, round 2).
    veil_attach_pending: bool,
    /// Cross-frame flatten/shape-input cache (see [`RenderCache`]): fade
    /// frames reuse settled blocks' text+runs; the incremental parser's stable
    /// boundary invalidates only the live tail per commit.
    render_cache: Rc<RefCell<RenderCache>>,
    highlights: HighlightStore,
    tool_diff_fetches: HashMap<ToolDiffFetchKey, ToolDiffFetchState>,
    tool_diff_tasks: HashMap<ToolDiffFetchKey, (u64, Task<()>)>,
    tool_diff_generation: u64,
    show_jump_button: bool,
    /// Distance from the bottom at the last observation (wheel event or spring
    /// tick) — restick and escape are direction-aware
    /// (see [`Transcript::should_restick`]).
    last_scroll_distance: f32,
    /// The stick-to-bottom pin. Broken only by user input (wheel/touch up);
    /// re-engaged inside the 70px band, on own-send, and on the jump button.
    pinned: bool,
    spring: StickSpring,
    /// Wall-clock of the previous spring tick (`None` = parked).
    spring_last_tick: Option<Instant>,
    /// When the spring last landed on the bottom (settle-grace bookkeeping).
    spring_settled_at: Option<Instant>,
    /// A doc commit / wake happened before layout measured it — run at least
    /// one spring tick even though the pre-layout distance still reads 0.
    spring_kick: bool,
    /// One `on_next_frame` callback in flight at most.
    spring_scheduled: bool,
    scroll_anim: Option<Task<()>>,
    /// MessageRail width gate (set by the shell from the container width).
    rail_enabled: bool,
    /// Hovered rail tick (grows + shows the preview card).
    rail_hover: Option<usize>,
    /// `(row id, entry id)` under the pointer — reveals the entry's timestamp
    /// strip (comet chat-view.tsx `group-hover`; the rows report hover
    /// themselves). Keyed by ROW so a row→row move within one entry can't
    /// clear the reveal when the old row's leave event arrives after the new
    /// row's enter (enter/leave order across rows is not guaranteed).
    hovered_entry: Option<(SharedString, SharedString)>,
    /// Code block showing "Copied" feedback: `(row id, block ix)`, cleared by
    /// the companion task after ~1.2s.
    copied_code: Option<(SharedString, usize)>,
    copied_clear: Option<Task<()>>,
    /// Transcript attachment being viewed full-size (click a user thumbnail).
    attachment_preview: Option<crate::attachments::PreviewImage>,
    /// In-flight ReadAttachmentChunk loads, keyed `(deviceId, path)` — one per
    /// source; results land in the global attachment cache.
    attachment_loads: HashMap<(ServerId, String, String), Task<()>>,
    /// Scheduled retry wake-ups for errored sources (the 2s→15s ladder).
    attachment_retries: HashMap<(ServerId, String, String), Task<()>>,
    _observe: Subscription,
}

impl Transcript {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        // FollowMode stays Normal: the tail pin is ours (a per-frame spring),
        // not the list's per-layout hard snap.
        let list = ListState::new(0, ListAlignment::Bottom, px(OVERDRAW_PX));
        let weak = cx.weak_entity();
        list.set_scroll_handler(move |event: &ListScrollEvent, _window, cx| {
            weak.update(cx, |this: &mut Transcript, cx| {
                this.handle_scroll(event, cx)
            })
            .ok();
        });
        let observe = cx.observe(&state, |this: &mut Self, _, cx| this.sync(cx));
        let mut this = Self {
            state,
            list,
            rows: Vec::new(),
            chat_id: None,
            row_cache: HashMap::new(),
            live_parsers: HashMap::new(),
            tree_cache: HashMap::new(),
            folds: HashMap::new(),
            veils: HashMap::new(),
            veil_baseline: std::collections::HashSet::new(),
            veil_attach_pending: true,
            render_cache: Rc::new(RefCell::new(RenderCache::default())),
            highlights: HighlightStore::default(),
            tool_diff_fetches: HashMap::new(),
            tool_diff_tasks: HashMap::new(),
            tool_diff_generation: 0,
            show_jump_button: false,
            last_scroll_distance: 0.0,
            pinned: true,
            spring: StickSpring::new(),
            spring_last_tick: None,
            spring_settled_at: None,
            spring_kick: false,
            spring_scheduled: false,
            scroll_anim: None,
            rail_enabled: true,
            rail_hover: None,
            hovered_entry: None,
            copied_code: None,
            copied_clear: None,
            attachment_preview: None,
            attachment_loads: HashMap::new(),
            attachment_retries: HashMap::new(),
            _observe: observe,
        };
        this.sync(cx);
        this
    }

    // ---- rail plumbing (rendering lives in crate::rail) ----

    /// Shell-driven width gate: the rail hides below 48rem of container width.
    pub fn set_rail_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.rail_enabled != enabled {
            self.rail_enabled = enabled;
            cx.notify();
        }
    }

    pub(crate) fn rail_enabled(&self) -> bool {
        self.rail_enabled
    }

    pub(crate) fn rail_hover(&self) -> Option<usize> {
        self.rail_hover
    }

    pub(crate) fn set_rail_hover(&mut self, hover: Option<usize>) {
        self.rail_hover = hover;
    }

    pub(crate) fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub(crate) fn list_state(&self) -> &ListState {
        &self.list
    }

    pub(crate) fn state_entity(&self) -> &Entity<AppState> {
        &self.state
    }

    /// Replace the transcript's scroll animation task (rail click / jump).
    pub(crate) fn set_scroll_task(&mut self, task: Task<()>) {
        self.pinned = false;
        self.scroll_anim = Some(task);
    }

    pub(crate) fn distance_from_bottom(&self) -> f32 {
        let max = f32::from(self.list.max_offset_for_scrollbar().y);
        let cur = f32::from(self.list.scroll_px_offset_for_scrollbar().y);
        (max + cur).max(0.0)
    }

    /// Whether a user scroll should re-engage the bottom pin: inside the 70px
    /// stick band *and* moving toward the bottom. Direction matters — a small
    /// wheel-up notch near the bottom stays inside the band, and re-sticking
    /// on it would snap the view straight back, making the pin unbreakable.
    pub fn should_restick(distance: f32, previous_distance: f32) -> bool {
        distance <= STICK_THRESHOLD_PX && distance < previous_distance
    }

    fn handle_scroll(&mut self, _event: &ListScrollEvent, cx: &mut Context<Self>) {
        // The list invokes this handler ONLY from its wheel/touch input path
        // (programmatic scroll_by/scroll_to never re-enter it), while holding
        // its internal RefCell borrow — reading the ListState back
        // synchronously panics with "already mutably borrowed". Defer to the
        // end of the effect cycle, after the list has released its borrow.
        let this = cx.weak_entity();
        cx.defer(move |cx| {
            this.update(cx, |this: &mut Transcript, cx| {
                let distance = this.distance_from_bottom();
                let previous = this.last_scroll_distance;
                this.last_scroll_distance = distance;
                if distance > previous + 1.0 && distance > AT_BOTTOM_PX {
                    // User input moving away from the bottom breaks the pin.
                    // Content growth never lands here — it doesn't fire the
                    // scroll handler (mugen §1e: interrupt from input, not
                    // scrollbar position).
                    this.pinned = false;
                    this.spring.reset();
                    this.spring_last_tick = None;
                } else if distance <= AT_BOTTOM_PX || Self::should_restick(distance, previous) {
                    // Returning toward the bottom inside the 70px band (or
                    // arriving at it) re-engages the pin with a glide.
                    if !this.pinned {
                        this.pinned = true;
                        this.wake_spring();
                    }
                }
                let show = distance > SCROLL_BUTTON_THRESHOLD_PX && !this.pinned;
                if show != this.show_jump_button {
                    this.show_jump_button = show;
                }
                cx.notify();
            })
            .ok();
        });
    }

    /// Own-send re-engage: glide to the end, then stay pinned.
    pub fn on_own_send(&mut self, cx: &mut Context<Self>) {
        self.engage_pin(cx);
    }

    /// Whether the transcript is currently pinned to the bottom.
    pub fn is_pinned(&self) -> bool {
        self.pinned
    }

    /// Whether the shell should float the "Scroll to bottom" pill (scrolled
    /// more than [`SCROLL_BUTTON_THRESHOLD_PX`] off the end, unpinned).
    pub fn jump_button_shown(&self) -> bool {
        self.show_jump_button
    }

    /// The scroll-to-bottom pill's click: glide back to the end and re-pin.
    pub fn jump_to_bottom(&mut self, cx: &mut Context<Self>) {
        self.engage_pin(cx);
    }

    /// Re-engage the bottom pin with a glide. Long jumps teleport to within
    /// [`GLIDE_MAX_VIEWPORTS`] of the end first (mugen `springToBottom`);
    /// reduced motion snaps.
    fn engage_pin(&mut self, cx: &mut Context<Self>) {
        self.pinned = true;
        self.show_jump_button = false;
        if motion::reduced_motion(cx) {
            self.list.scroll_to_end();
            cx.notify();
            return;
        }
        let viewport = f32::from(self.list.viewport_bounds().size.height);
        let distance = self.distance_from_bottom();
        let glide_max = GLIDE_MAX_VIEWPORTS * viewport;
        if viewport > 0.0 && distance > glide_max {
            self.list.scroll_by(px(distance - glide_max));
        }
        self.wake_spring();
        cx.notify();
    }

    /// Arm the per-frame spring driver — `render` schedules the next frame
    /// while [`Self::spring_should_run`].
    fn wake_spring(&mut self) {
        self.spring_settled_at = None;
        self.spring_kick = true;
    }

    /// Whether the spring loop needs another frame: off the bottom, carrying
    /// residual motion, or inside the post-landing settle grace.
    fn spring_should_run(&self) -> bool {
        self.spring_kick
            || self.distance_from_bottom() > 0.5
            || !self.spring.is_idle()
            || self.spring_settled_at.is_some()
    }

    /// Whether the scroll offset is in a bottom-glued representation (`None`
    /// or anchored past the end) — states where the next layout hard-snaps to
    /// the new end instead of holding a pixel position.
    pub(crate) fn is_glued(&self) -> bool {
        self.list.logical_scroll_top().item_ix >= self.rows.len()
    }

    /// One spring frame: observe target growth, step the stepper, apply the
    /// delta, park after the settle grace. Runs from `window.on_next_frame`,
    /// i.e. after layout — measurements are fresh.
    fn step_spring(&mut self, cx: &mut Context<Self>) {
        self.spring_kick = false;
        if !self.pinned {
            self.spring_last_tick = None;
            return;
        }
        let now = Instant::now();
        let frames = match self.spring_last_tick {
            Some(last) => (now.duration_since(last).as_secs_f32() * 1000.0 / SPRING_FRAME_MS)
                .min(SPRING_MAX_CATCHUP_FRAMES),
            None => 1.0,
        };
        self.spring_last_tick = Some(now);

        let target = f32::from(self.list.max_offset_for_scrollbar().y);
        let mut distance = self.distance_from_bottom();
        // Long jumps (chat switch mid-history, huge pastes) teleport first.
        let viewport = f32::from(self.list.viewport_bounds().size.height);
        let glide_max = GLIDE_MAX_VIEWPORTS * viewport;
        if viewport > 0.0 && distance > glide_max {
            self.list.scroll_by(px(distance - glide_max));
            distance = glide_max;
        }
        let pos = target - distance;
        let next = self.spring.step(pos, target, frames);
        if next > pos {
            self.list.scroll_by(px(next - pos));
        }
        self.last_scroll_distance = (target - next).max(0.0);

        if target - next <= 0.5 {
            let settled = *self.spring_settled_at.get_or_insert(now);
            if now.duration_since(settled) >= Duration::from_millis(SPRING_SETTLE_GRACE_MS)
                && self.spring.is_idle()
            {
                // Park: stop scheduling frames until the next wake.
                self.spring.reset();
                self.spring_last_tick = None;
                self.spring_settled_at = None;
                return;
            }
        } else {
            self.spring_settled_at = None;
        }
        cx.notify();
    }

    /// Rebuild rows from app state; splice minimal ranges into the list.
    fn sync(&mut self, cx: &mut Context<Self>) {
        let (selected, entries, echoes) = {
            let s = self.state.read(cx);
            (
                s.selected_chat.clone(),
                s.transcript.clone(),
                s.pending_echoes().to_vec(),
            )
        };

        let attached = transcript_owner_changed(self.chat_id.as_ref(), selected.as_ref());
        if attached {
            self.chat_id = selected;
            self.rows.clear();
            self.row_cache.clear();
            self.live_parsers.clear();
            self.tree_cache.clear();
            self.folds.clear();
            self.veils.clear();
            self.render_cache.borrow_mut().clear();
            self.highlights.clear();
            self.tool_diff_fetches.clear();
            self.tool_diff_tasks.clear();
            self.list.reset(0);
            self.pinned = true;
            self.spring.reset();
            self.spring_last_tick = None;
            self.spring_settled_at = None;
            self.spring_kick = false;
            self.show_jump_button = false;
            self.attachment_loads.clear();
            self.attachment_retries.clear();
            self.attachment_preview = None;
        }

        let mut new_rows: Vec<Row> = Vec::new();
        for entry in &entries {
            new_rows.extend(self.rows_for(entry, false));
        }
        for echo in &echoes {
            new_rows.extend(self.rows_for(echo, true));
        }

        if !attached {
            let mut live_fetches = HashSet::new();
            let mut live_highlights: HashSet<(SharedString, usize)> = new_rows
                .iter()
                .filter_map(|row| match &row.kind {
                    RowKind::Markdown { tree, block_ix }
                    | RowKind::LiveMarkdown { tree, block_ix }
                        if matches!(
                            tree.blocks.get(*block_ix).map(|top| &top.block),
                            Some(Block::CodeBlock { .. })
                        ) =>
                    {
                        Some((row.id.clone(), *block_ix))
                    }
                    _ => None,
                })
                .collect();
            let mut live_folds: HashSet<SharedString> =
                new_rows.iter().map(|row| row.id.clone()).collect();
            if let Some(owner) = &self.chat_id {
                for row in &new_rows {
                    let RowKind::ToolGroup { tools, .. } = &row.kind else {
                        continue;
                    };
                    for (tool_ix, tool) in tools.iter().enumerate() {
                        let Some(diff_ref) = &tool.diff_ref else {
                            continue;
                        };
                        live_fetches.insert(ToolDiffFetchKey {
                            owner: owner.clone(),
                            part_id: tool.id.to_string(),
                            diff_ref: diff_ref.to_string(),
                        });
                        live_folds.insert(format!("{}#tool-{tool_ix}", row.id).into());
                        live_highlights.insert((row.id.clone(), tool_ix * 2));
                        live_highlights.insert((row.id.clone(), tool_ix * 2 + 1));
                    }
                }
            }
            self.tool_diff_fetches
                .retain(|key, _| live_fetches.contains(key));
            self.tool_diff_tasks
                .retain(|key, _| live_fetches.contains(key));
            self.highlights.retain_slots(&live_highlights);
            self.folds.retain(|id, _| live_folds.contains(id));
        }

        // Text already streamed before this (re)attach is the veil BASELINE:
        // its rows' veils seed instead of fading (render creates them from
        // this set), so only post-switch appends animate. Captured from the
        // first NON-EMPTY transcript after attach — the replay frame — never
        // the attach-time sync, whose transcript is still empty (selection
        // clears it; the doc watch refills it async).
        if attached {
            self.veil_baseline.clear();
            self.veil_attach_pending = true;
        }
        if self.veil_attach_pending && !entries.is_empty() {
            self.veil_attach_pending = false;
            self.veil_baseline = new_rows
                .iter()
                .filter(|r| matches!(r.kind, RowKind::LiveMarkdown { .. }))
                .map(|r| r.id.clone())
                .collect();
        }

        // Veils live exactly as long as their live row — drop them on the
        // live→complete flip (any mid-fade chunk snaps to full, matching the
        // row's version splice).
        self.veils.retain(|id, _| {
            new_rows
                .iter()
                .any(|r| &r.id == id && matches!(r.kind, RowKind::LiveMarkdown { .. }))
        });
        self.veil_baseline.retain(|id| {
            new_rows
                .iter()
                .any(|r| &r.id == id && matches!(r.kind, RowKind::LiveMarkdown { .. }))
        });

        let was_empty = self.rows.is_empty();
        match diff_rows(&self.rows, &new_rows) {
            None => {
                self.rows = new_rows;
                return;
            }
            Some((old_range, count)) => {
                // Any replaced row's cached flatten results are stale — and
                // because live replies splice only the rows whose content hash
                // changed (the tail), this is O(changed rows) per commit, never
                // O(reply).
                for row in &self.rows[old_range.clone()] {
                    self.render_cache.borrow_mut().invalidate_row(&row.id);
                }
                if old_range.len() == count {
                    // In-place content change, same row count — notably the
                    // live→complete flip, where EVERY row of the streamed
                    // message changes version (streaming bit, tool auto_open,
                    // timestamp bit) with identical ids. `splice` would reset
                    // those items to hint-less Unmeasured (heights read 0
                    // until the next paint) and, when the viewport-top item is
                    // inside the range, clobber the scroll anchor to the range
                    // start — the end-of-turn up/down jump the spring then has
                    // to walk back. `remeasure_items` keeps old sizes as hints
                    // and holds the anchor across the remeasure.
                    self.list.remeasure_items(old_range);
                } else {
                    self.list.splice(old_range, count);
                }
            }
        }
        self.rows = new_rows;
        if self.pinned {
            if motion::reduced_motion(cx) || was_empty {
                // First fill (chat open) lands at the bottom instantly
                // (mugen initialScroll:'bottom'); reduced motion always snaps.
                self.list.scroll_to_end();
            } else if self.is_glued() {
                // A glued offset (`None` / anchored past the end) makes the
                // upcoming layout hard-snap to the new end — the per-commit
                // stutter. Materialize a pixel anchor a hair above the bottom
                // so layout holds position and the spring glides the growth.
                self.list.scroll_by(px(-0.75));
            }
            self.spring_kick = true;
        }
        cx.notify();
    }

    /// Cached row build for one entry (streaming entries bypass the cache).
    fn rows_for(&mut self, entry: &SessionMessageEntry, pending: bool) -> Vec<Row> {
        let streaming = entry.status == Some(MessageStatus::Streaming);
        let fingerprint = entry_fingerprint(entry, pending);
        if !streaming
            && let Some(cached) = self.row_cache.get(&entry.id)
            && cached.fingerprint == fingerprint
        {
            return cached.rows.clone();
        }

        let live_parsers = &mut self.live_parsers;
        let tree_cache = &mut self.tree_cache;
        let mut parse = |key: &str, text: &str| -> Arc<BlockTree> {
            // Render-cache invalidation rides on the row diff in `sync` (only
            // rows whose content hash changed are spliced — the reparsed tail).
            parse_for_row(streaming, key, text, live_parsers, tree_cache).0
        };
        let rows = rows_for_entry(entry, pending, &mut parse);

        if !streaming {
            self.row_cache.insert(
                entry.id.clone(),
                CachedRows {
                    fingerprint,
                    rows: rows.clone(),
                },
            );
        }
        rows
    }

    /// One delegated agent (slice 4.4). Passive — a subagent asks the user for
    /// nothing, so unlike an approval this puts no controls in the composer.
    ///
    /// Every decision about what the card may SAY was made in `rows_for_entry`
    /// (see `subagent_row_state`); this picks colours and lays out. The only
    /// state it owns is whether the summary is folded, which rides the same
    /// `folds` map tool groups use and is therefore garbage-collected with the
    /// row like any other.
    #[allow(clippy::too_many_arguments)]
    fn render_subagent_card(
        &mut self,
        row_id: &SharedString,
        agent_type: &SharedString,
        description: &SharedString,
        status_caption: &SharedString,
        paint: SubagentPaint,
        activity: Option<&SharedString>,
        summary: Option<&SharedString>,
        caption: Option<&SharedString>,
        counters: Option<&SharedString>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (tint, icon_path) = match paint {
            SubagentPaint::Running => (theme.text_muted, crate::icons::MAGNIFER),
            SubagentPaint::Completed => (theme.success_muted, crate::icons::CHECK),
            SubagentPaint::Failed => (theme.danger, crate::icons::CLOSE_CIRCLE),
            // A plain cross, not the filled STOP square: cancellation is a
            // quiet outcome and the solid glyph read as the heaviest thing on
            // a muted card.
            SubagentPaint::Cancelled => (theme.text_muted, crate::icons::CLOSE),
            SubagentPaint::LastSeenRunning => (theme.warning_muted, crate::icons::DANGER_TRIANGLE),
        };
        let tile = tint.opacity(0.12);
        let border = match paint {
            SubagentPaint::Running | SubagentPaint::Cancelled => crate::theme::hairline(0.08),
            _ => tint.opacity(0.16),
        };
        let wash = match paint {
            SubagentPaint::Running | SubagentPaint::Cancelled => crate::theme::ink(0.045),
            _ => tint.opacity(0.05),
        };

        let summary_open = summary.is_some()
            && self
                .folds
                .get(row_id)
                .is_some_and(|fold| fold.open.unwrap_or(false));
        let toggle_id = row_id.clone();

        // The live dot breathes on the same 2.4s spec as every other pulse in
        // the app. It is drawn ONLY when `activity` survived both blanking
        // rules, so a finished card can never animate.
        let live_opacity = activity.is_some().then(|| {
            0.35 + 0.5
                * motion::pulse_wave(motion::pulse_delta(
                    &motion::COMET_PULSE,
                    cx.entity_id(),
                    cx,
                ))
        });

        div()
            .py(px(4.0))
            .w_full()
            .child(
                card_frame(border, wash, None)
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(Theme::SPACE_SM))
                            .child(card_tile(icon_path, tile, tint))
                            .child(
                                div()
                                    .flex_none()
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(tint)
                                    .child(agent_type.clone()),
                            )
                            .child(div().flex_1())
                            .child(
                                div()
                                    .flex_none()
                                    .min_w_0()
                                    .truncate()
                                    .text_color(theme.text_muted)
                                    .child(status_caption.clone()),
                            ),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .w_full()
                            .truncate()
                            .text_color(theme.text.opacity(0.85))
                            .child(description.clone()),
                    )
                    .children(activity.map(|activity| {
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(6.0))
                            .child(
                                div()
                                    .flex_none()
                                    .size(px(5.0))
                                    .rounded(px(2.5))
                                    .bg(theme.accent)
                                    .opacity(live_opacity.unwrap_or(1.0)),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_color(theme.text_muted)
                                    .child(activity.clone()),
                            )
                    }))
                    .children(caption.map(|caption| {
                        div()
                            .min_w_0()
                            .w_full()
                            .text_color(theme.text_muted)
                            .child(caption.clone())
                    }))
                    .children(summary.map(|summary| {
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(Theme::SPACE_XS))
                            .child(div().h(px(1.0)).w_full().bg(crate::theme::hairline(0.08)))
                            .child({
                                let body = div()
                                    .min_w_0()
                                    .w_full()
                                    .text_color(theme.text.opacity(0.85))
                                    .child(summary.clone());
                                // Folded height is two lines at this size, in
                                // layout numbers only — never a palette value.
                                if summary_open {
                                    body
                                } else {
                                    body.max_h(px(36.0)).overflow_hidden()
                                }
                            })
                            .child(
                                div()
                                    .id(SharedString::from(format!("{row_id}#sum")))
                                    .cursor_pointer()
                                    .text_color(theme.text_muted)
                                    .child(SharedString::from(if summary_open {
                                        "Show less"
                                    } else {
                                        "Show more"
                                    }))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        let fold = this.folds.entry(toggle_id.clone()).or_default();
                                        fold.open = Some(!fold.open.unwrap_or(false));
                                        fold.epoch += 1;
                                        cx.notify();
                                    })),
                            )
                    }))
                    .children(counters.map(|counters| {
                        div()
                            .min_w_0()
                            .w_full()
                            .truncate()
                            .text_color(theme.text_faint)
                            .child(counters.clone())
                    })),
            )
            .into_any_element()
    }

    /// The plan card (slice 4.4). Passive and non-interactive: a plan is a
    /// report, and every step's state is already on the row.
    ///
    /// Shares the card frame with the approval and subagent cards. Step glyphs
    /// are DRAWN rather than iconography — the embedded Solar set has no
    /// half-filled circle, and a ring plus a centred dot is a layout, so it
    /// needs no new asset and stays palette-independent.
    fn render_checklist_card(
        &mut self,
        explanation: Option<&SharedString>,
        done: usize,
        steps: &Arc<Vec<ChecklistRow>>,
        theme: &Theme,
    ) -> AnyElement {
        let total = steps.len();
        div()
            .py(px(4.0))
            .w_full()
            .child(
                card_frame(crate::theme::hairline(0.08), crate::theme::ink(0.045), None)
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(Theme::SPACE_SM))
                            .child(card_tile(
                                crate::icons::CHECKLIST,
                                crate::theme::ink(0.09),
                                theme.text_muted,
                            ))
                            .child(
                                div()
                                    .flex_none()
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(theme.text_muted)
                                    .child(SharedString::from("Plan")),
                            )
                            .child(div().flex_1())
                            .child(
                                div()
                                    .flex_none()
                                    .text_color(theme.text_muted)
                                    .child(SharedString::from(format!("{done} of {total} done"))),
                            ),
                    )
                    .children(explanation.map(|explanation| {
                        div()
                            .min_w_0()
                            .w_full()
                            .text_color(theme.text_muted)
                            .child(explanation.clone())
                    }))
                    .child(div().h(px(1.0)).w_full().bg(crate::theme::hairline(0.08)))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(3.0))
                            .children(steps.iter().map(|step| {
                                let label_color = match step.status {
                                    ChecklistStatus::Completed => theme.text_muted,
                                    _ => theme.text.opacity(0.85),
                                };
                                // Filled discs, not hollow rings: an outlined
                                // circle reads as a radio button offering a
                                // choice, which a plan step is not. The disc
                                // grows through the states — small faint dot,
                                // full accent, full success with the tick
                                // knocked out in the plane behind the card — so
                                // the column still separates them without
                                // leaning on hue alone. The 12px box is fixed
                                // across all three, so the column never shifts.
                                let glyph = div()
                                    .flex_none()
                                    .size(px(12.0))
                                    .mt(px(3.0))
                                    .flex()
                                    .items_center()
                                    .justify_center();
                                let glyph = match step.status {
                                    ChecklistStatus::Completed => {
                                        glyph.rounded(px(6.0)).bg(theme.success).child(
                                            crate::icons::icon(crate::icons::CHECK)
                                                .size(px(8.0))
                                                .text_color(theme.bg),
                                        )
                                    }
                                    ChecklistStatus::InProgress => {
                                        glyph.rounded(px(6.0)).bg(theme.accent)
                                    }
                                    ChecklistStatus::Pending | ChecklistStatus::Unknown => glyph
                                        .child(
                                            div()
                                                .size(px(6.0))
                                                .rounded(px(3.0))
                                                .bg(theme.text_faint),
                                        ),
                                };
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_start()
                                    .gap(px(Theme::SPACE_SM))
                                    .child(glyph)
                                    .child(
                                        div()
                                            .min_w_0()
                                            .flex_1()
                                            .text_color(if step.unnamed {
                                                theme.text_faint
                                            } else {
                                                label_color
                                            })
                                            .child(step.label.clone()),
                                    )
                            })),
                    ),
            )
            .into_any_element()
    }

    fn toggle_fold(&mut self, row_id: SharedString, current_height: f32, auto_open: bool) {
        let entry = self.folds.entry(row_id).or_default();
        let currently_open = entry.open.unwrap_or(auto_open);
        entry.from = if currently_open { current_height } else { 0.0 };
        entry.open = Some(!currently_open);
        entry.epoch += 1;
        entry.toggled_at = Some(Instant::now());
    }

    fn toggle_tool_diff_detail(&mut self, detail_id: SharedString) {
        let entry = self.folds.entry(detail_id).or_default();
        entry.open = Some(!entry.open.unwrap_or(false));
        entry.epoch += 1;
        entry.toggled_at = Some(Instant::now());
    }

    fn begin_tool_diff_fetch(&mut self, key: ToolDiffFetchKey, cx: &mut Context<Self>) {
        if !tool_diff_fetch_needs_start(&self.tool_diff_fetches, &key) {
            return;
        }
        self.tool_diff_generation = self.tool_diff_generation.wrapping_add(1);
        let generation = self.tool_diff_generation;
        self.tool_diff_fetches
            .insert(key.clone(), ToolDiffFetchState::Loading { generation });

        let client = self.state.read(cx).client_for(&key.owner);
        let Some(client) = client else {
            tracing::warn!(owner = ?key.owner, "tool diff fetch has no client for chat owner");
            self.tool_diff_fetches
                .insert(key, ToolDiffFetchState::Unavailable);
            cx.notify();
            return;
        };

        let request_key = key.clone();
        let task = cx.spawn(async move |this, cx| {
            let call = client.call_as::<ReadToolDiffReply>(
                methods::READ_TOOL_DIFF,
                serde_json::json!({
                    "chatId": request_key.owner.local_id.clone(),
                    "partId": request_key.part_id.clone(),
                    "diffRef": request_key.diff_ref.clone(),
                }),
            );
            let timer = cx.background_executor().timer(TOOL_DIFF_FETCH_TIMEOUT);
            futures::pin_mut!(call);
            let resolved = match futures::future::select(call, timer).await {
                futures::future::Either::Left((Ok(reply), _)) => {
                    match validate_tool_diff_reply(&request_key.diff_ref, reply) {
                        Ok((diff, mut file)) => {
                            // One transcript row paints this inline. Bound element count while
                            // keeping the complete sources for background highlighting.
                            crate::changes::truncate_file_lines(&mut file, 400);
                            Some((Arc::new(diff), Arc::new(file)))
                        }
                        Err(failure) => {
                            log_tool_diff_validation_failure(&request_key, &failure);
                            None
                        }
                    }
                }
                futures::future::Either::Left((Err(error), _)) => {
                    tracing::warn!(
                        owner = ?request_key.owner,
                        part_id = %request_key.part_id,
                        error = %error,
                        "tool diff request failed"
                    );
                    None
                }
                futures::future::Either::Right(_) => {
                    tracing::warn!(
                        owner = ?request_key.owner,
                        part_id = %request_key.part_id,
                        timeout_secs = TOOL_DIFF_FETCH_TIMEOUT.as_secs(),
                        "tool diff request timed out"
                    );
                    None
                }
            };
            this.update(cx, |transcript, cx| {
                if complete_tool_diff_fetch(
                    transcript.chat_id.as_ref(),
                    &mut transcript.tool_diff_fetches,
                    &mut transcript.tool_diff_tasks,
                    request_key.clone(),
                    generation,
                    resolved,
                ) {
                    cx.notify();
                }
            })
            .ok();
        });
        self.tool_diff_tasks.insert(key, (generation, task));
    }

    fn tool_diff_highlights_for(
        &mut self,
        row_id: &SharedString,
        tool_ix: usize,
        diff: &ToolDiff,
        cx: &mut Context<Self>,
    ) -> Option<Arc<crate::changes::DiffHighlights>> {
        let lang = comet_syntax::language_for_path(&diff.path)?;
        let old = diff.old_text.as_deref().and_then(|source| {
            self.highlights
                .request(row_id.clone(), tool_ix * 2, lang, source, cx)
        });
        let new =
            self.highlights
                .request(row_id.clone(), tool_ix * 2 + 1, lang, &diff.new_text, cx);
        (old.is_some() || new.is_some())
            .then(|| Arc::new(crate::changes::DiffHighlights { old, new }))
    }

    // ---- attachment read-back (user-attachments.tsx + transcript cache) ----

    /// Devices that may own a user message's attachment files: the chat's host
    /// device (uploads targeted it) plus this device (comet's
    /// `uniqueIds([attachmentDeviceId, m.device_id])`).
    fn attachment_device_ids(&self, cx: &Context<Self>) -> Vec<String> {
        let state = self.state.read(cx);
        let mut ids = Vec::new();
        if let Some(chat) = state.selected_chat_row() {
            ids.push(chat.device_id.clone());
        }
        if let Some(local) = state.local_device_id.clone()
            && !ids.contains(&local)
        {
            ids.push(local);
        }
        ids
    }

    /// Effective load state for one attachment across its candidate devices:
    /// first Loaded source wins; otherwise loads are (re)claimed and the
    /// snapshot degrades Loading → Error with a scheduled retry wake-up.
    fn attachment_state(
        &mut self,
        device_ids: &[String],
        path: &str,
        cx: &mut Context<Self>,
    ) -> crate::attachments::AttachmentSnapshot {
        use crate::attachments::{AttachmentSnapshot, attachment_snapshot, begin_load};
        let Some(server_id) = self.state.read(cx).selected_server_id().cloned() else {
            return AttachmentSnapshot::Error {
                retry_in: Duration::MAX,
            };
        };
        for dev in device_ids {
            if let AttachmentSnapshot::Loaded(image) = attachment_snapshot(&server_id, dev, path) {
                return AttachmentSnapshot::Loaded(image);
            }
        }
        let mut any_loading = false;
        let mut min_retry: Option<Duration> = None;
        for dev in device_ids {
            if begin_load(&server_id, dev, path) {
                let generation = crate::attachments::attachment_generation(&server_id);
                self.spawn_attachment_load(
                    server_id.clone(),
                    dev.clone(),
                    path.to_string(),
                    generation,
                    cx,
                );
            }
            match attachment_snapshot(&server_id, dev, path) {
                AttachmentSnapshot::Loaded(image) => return AttachmentSnapshot::Loaded(image),
                AttachmentSnapshot::Loading => any_loading = true,
                AttachmentSnapshot::Error { retry_in } => {
                    min_retry = Some(min_retry.map_or(retry_in, |m| m.min(retry_in)));
                }
            }
        }
        if any_loading {
            return AttachmentSnapshot::Loading;
        }
        match min_retry {
            Some(retry_in) => {
                if let Some(dev) = device_ids.first() {
                    self.schedule_attachment_retry(
                        (server_id, dev.clone(), path.to_string()),
                        retry_in,
                        cx,
                    );
                }
                AttachmentSnapshot::Error { retry_in }
            }
            // No candidate devices at all — the "unavailable" thumb, no retry.
            None => AttachmentSnapshot::Error {
                retry_in: Duration::MAX,
            },
        }
    }

    fn spawn_attachment_load(
        &mut self,
        server_id: ServerId,
        device_id: String,
        path: String,
        generation: u64,
        cx: &mut Context<Self>,
    ) {
        use crate::attachments::{
            read_attachment_image, store_error_for_generation, store_loaded_for_generation,
        };
        let owner = ServerRef::new(server_id.clone(), "attachment");
        let Some(engine) = self.state.read(cx).client_for(&owner) else {
            store_error_for_generation(&server_id, &device_id, &path, generation);
            return;
        };
        let key = (server_id.clone(), device_id.clone(), path.clone());
        let task = cx.spawn(async move |this, cx| {
            match read_attachment_image(&engine, cx.background_executor(), &path).await {
                Some(loaded) => store_loaded_for_generation(
                    &server_id,
                    &device_id,
                    &path,
                    loaded.name.into(),
                    loaded.image,
                    generation,
                ),
                None => store_error_for_generation(&server_id, &device_id, &path, generation),
            }
            this.update(cx, |transcript, cx| {
                transcript.attachment_loads.remove(&(
                    server_id.clone(),
                    device_id.clone(),
                    path.clone(),
                ));
                cx.notify();
            })
            .ok();
        });
        self.attachment_loads.insert(key, task);
    }

    /// One wake-up per errored source: after the backoff elapses, a notify
    /// re-renders the thumb, whose `begin_load` then claims the retry.
    fn schedule_attachment_retry(
        &mut self,
        key: (ServerId, String, String),
        delay: Duration,
        cx: &mut Context<Self>,
    ) {
        if delay == Duration::MAX || self.attachment_retries.contains_key(&key) {
            return;
        }
        let wake = key.clone();
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(delay + Duration::from_millis(60))
                .await;
            this.update(cx, |transcript, cx| {
                transcript.attachment_retries.remove(&wake);
                cx.notify();
            })
            .ok();
        });
        self.attachment_retries.insert(key, task);
    }

    /// The right-aligned thumbnail strip above a user bubble.
    fn render_user_attachments(
        &mut self,
        row_id: &SharedString,
        atts: &[crate::attachments::UserImageAttachment],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use crate::attachments::AttachmentSnapshot;
        let device_ids = self.attachment_device_ids(cx);
        let mut strip = div()
            .w_full()
            .h(px(ATT_STRIP_H))
            .flex()
            .flex_row()
            .justify_end()
            .items_start()
            .gap(px(8.0))
            .overflow_hidden()
            .px(px(4.0))
            .pt(px(4.0));
        for (aix, att) in atts.iter().enumerate() {
            let state = self.attachment_state(&device_ids, &att.path, cx);
            let frame = div()
                .flex_none()
                .w(px(ATT_THUMB_W))
                .h(px(ATT_THUMB_H))
                .rounded(px(8.0))
                .overflow_hidden();
            let thumb: AnyElement = match state {
                AttachmentSnapshot::Loaded(image) => {
                    let preview = crate::attachments::PreviewImage {
                        name: image.name.clone(),
                        image: image.image.clone(),
                    };
                    frame
                        .id(SharedString::from(format!("{row_id}#att{aix}")))
                        .border_1()
                        .border_color(crate::theme::hairline(0.11))
                        .bg(crate::theme::ink(0.035))
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.attachment_preview = Some(preview.clone());
                            cx.notify();
                        }))
                        .child(
                            img(image.image.clone())
                                .size_full()
                                .object_fit(ObjectFit::Cover),
                        )
                        .into_any_element()
                }
                // Errored/unavailable: the dashed "missing" thumb.
                AttachmentSnapshot::Error { .. } => frame
                    .border_1()
                    .border_dashed()
                    .border_color(crate::theme::hairline(0.14))
                    .bg(crate::theme::ink(0.025))
                    .into_any_element(),
                // Loading: the pulsing skeleton (same wash as popover skeletons).
                AttachmentSnapshot::Loading => frame
                    .border_1()
                    .border_color(crate::theme::hairline(0.08))
                    .bg(crate::theme::ink(0.055))
                    .opacity(
                        0.35 + 0.4
                            * motion::pulse_wave(motion::pulse_delta(
                                &motion::COMET_PULSE,
                                cx.entity_id(),
                                cx,
                            )),
                    )
                    .into_any_element(),
            };
            strip = strip.child(thumb);
        }
        strip.into_any_element()
    }

    // ---- rendering ----

    fn render_row(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(row) = self.rows.get(ix).cloned() else {
            return gpui::Empty.into_any_element();
        };
        let theme = Theme::of(cx).clone();
        let top_gap = if ix == 0 {
            GAP_TURN + 10.0
        } else {
            top_gap_for(ix.checked_sub(1).and_then(|i| self.rows.get(i)), &row)
        };
        // The last row must clear the shell's bottom fade band, or the
        // timestamp strip (the row's lowest content) renders half-faded
        // when the transcript is scrolled to the bottom.
        let bottom_pad = if ix + 1 == self.rows.len() {
            Theme::TRANSCRIPT_FADE_BAND + 8.0
        } else {
            0.0
        };

        let inner: AnyElement = match &row.kind {
            RowKind::User {
                text,
                mentions,
                attachments,
                badges,
                pending,
            } => {
                let attachments = attachments.clone();
                let badges = badges.clone();
                let text = text.clone();
                let mentions = mentions.clone();
                let pending = *pending;
                // Attachment thumbnails ride ABOVE the bubble, right-aligned
                // (chat-view.tsx RowView: UserAttachmentStrip then the text
                // HStack); image-only sends show no bubble at all.
                let mut column = div().w_full().flex().flex_col();
                if !attachments.is_empty() {
                    column = column.child(self.render_user_attachments(&row.id, &attachments, cx));
                }
                if !badges.is_empty() {
                    column = column.child(
                        div()
                            .w_full()
                            .flex()
                            .flex_row()
                            .flex_wrap()
                            .justify_end()
                            .items_center()
                            .gap(px(6.0))
                            .pb(px(6.0))
                            .children(badges.iter().enumerate().map(|(bix, badge)| {
                                crate::badges::render(
                                    SharedString::from(format!("{}#badge{bix}", row.id)),
                                    badge,
                                    &theme,
                                )
                            })),
                    );
                }
                if !text.is_empty() {
                    // `min_w_0` is load-bearing: gpui text answers min/max-content
                    // probes with its UNWRAPPED width, so without it the bubble's
                    // automatic min-size is the full single-line width — the flex
                    // item can't shrink, `justify_end` pushes the overflow off the
                    // left edge, and long prompts render as one clipped line
                    // instead of wrapping inside the 80% column cap.
                    column = column.child(
                        div().w_full().flex().justify_end().child(
                            div()
                                .min_w_0()
                                .max_w(px(MAX_CONTENT_WIDTH * 0.8))
                                .bg(theme.surface_raised)
                                .rounded(px(Theme::BUBBLE_RADIUS))
                                .px(px(16.0))
                                .py(px(10.0))
                                .text_size(px(14.0))
                                .line_height(px(22.0))
                                .text_color(theme.text)
                                .when(pending, |el| el.opacity(0.65))
                                .child(user_bubble_text(&row.id, text, mentions, &theme)),
                        ),
                    );
                }
                column.into_any_element()
            }
            RowKind::Markdown { tree, block_ix } => {
                let opts = RenderOptions {
                    row_key: row.id.clone(),
                    veil: None,
                    cache: (!render_cache_disabled()).then(|| self.render_cache.clone()),
                    now: Instant::now(),
                    copy: Some(self.copy_ui_for(&row.id, cx)),
                };
                let highlight = self.code_highlight_for(&row.id, tree, Some(*block_ix), cx);
                let Some(top) = tree.blocks.get(*block_ix) else {
                    return gpui::Empty.into_any_element();
                };
                render::render_block(
                    &top.block,
                    *block_ix,
                    *block_ix,
                    &opts,
                    &theme,
                    window,
                    highlight
                        .get(block_ix)
                        .and_then(|o| o.as_deref())
                        .map(|v| v.lines.as_slice()),
                )
            }
            RowKind::LiveMarkdown { tree, block_ix } => {
                // Per-appended-chunk fade veil (opacity only — layout commits
                // instantly). Reduced motion renders with no veil at all.
                // Baseline rows (text already streamed when the transcript
                // attached) start seeded: the existing reply must not fade in
                // on a session switch — only fresh appends animate.
                let veil = (!motion::reduced_motion(cx)).then(|| {
                    self.veils
                        .entry(row.id.clone())
                        .or_insert_with(|| {
                            if self.veil_baseline.contains(&row.id) {
                                Rc::new(RefCell::new(RowVeil::seeded()))
                            } else {
                                Rc::default()
                            }
                        })
                        .clone()
                });
                let opts = RenderOptions {
                    row_key: row.id.clone(),
                    veil: veil.clone(),
                    cache: (!render_cache_disabled()).then(|| self.render_cache.clone()),
                    now: Instant::now(),
                    copy: Some(self.copy_ui_for(&row.id, cx)),
                };
                let highlight = self.code_highlight_for(&row.id, tree, Some(*block_ix), cx);
                let Some(top) = tree.blocks.get(*block_ix) else {
                    return gpui::Empty.into_any_element();
                };
                let timer = frame_stats_enabled().then(Instant::now);
                let el = render::render_block(
                    &top.block,
                    *block_ix,
                    *block_ix,
                    &opts,
                    &theme,
                    window,
                    highlight
                        .get(block_ix)
                        .and_then(|o| o.as_deref())
                        .map(|v| v.lines.as_slice()),
                );
                if let Some(start) = timer {
                    record_live_frame_us(start.elapsed().as_micros() as u64);
                }
                // The attach pass for this row is done (every element rendered
                // above seeded its baseline synchronously): elements appearing
                // from the NEXT pass on are newly streamed and fade normally.
                if let Some(veil) = &veil {
                    veil.borrow_mut().finish_seeding();
                }
                // Drive the veil clock: while any chunk is still dissolving,
                // repaint next frame (self-limiting — one callback per frame).
                if veil.is_some_and(|v| v.borrow().is_fading()) {
                    let id = cx.entity_id();
                    window.on_next_frame(move |_, cx| cx.notify(id));
                }
                el
            }
            RowKind::ToolGroup { tools, auto_open } => {
                self.render_tool_group(&row.id, tools, *auto_open, &theme, cx)
            }
            RowKind::InputChip { header, resolved } => {
                input_chip(header.clone(), *resolved, &theme)
            }
            RowKind::ApprovalCard {
                label,
                detail,
                state,
                paint,
            } => approval_card(label, detail.clone(), state.clone(), *paint, &theme),
            RowKind::DenyNote { message } => deny_note_chip(
                format!("{}-deny-note", row.id).into(),
                message.clone(),
                &theme,
            ),
            RowKind::ErrorChip { message } => error_chip(message.clone(), &theme),
            RowKind::SubagentCard {
                agent_type,
                description,
                status_caption,
                paint,
                activity,
                summary,
                caption,
                counters,
            } => self.render_subagent_card(
                &row.id,
                agent_type,
                description,
                status_caption,
                *paint,
                activity.as_ref(),
                summary.as_ref(),
                caption.as_ref(),
                counters.as_ref(),
                &theme,
                cx,
            ),
            RowKind::ChecklistCard {
                explanation,
                done,
                steps,
            } => self.render_checklist_card(explanation.as_ref(), *done, steps, &theme),
            RowKind::NoticeChip {
                summary,
                detail,
                severity,
                occurrences,
            } => notice_chip(
                format!("{}-notice", row.id).into(),
                summary.clone(),
                detail.clone(),
                *severity,
                *occurrences,
                &theme,
            ),
        };

        // Hover-revealed timestamp strip (comet chat-view.tsx `Timestamp`):
        // a RESERVED 16px lane under the entry's last row — the label only
        // flips opacity, so revealing it never shifts the virtualizer's
        // layout. User entries align end (under the bubble), assistant start.
        let is_user_row = matches!(row.kind, RowKind::User { .. });
        let hovered = self
            .hovered_entry
            .as_ref()
            .is_some_and(|(_, entry)| entry == &row.entry_id);
        // Vertical breathing room from the source: assistant text blocks sit
        // in a `VStack padding={4}` (chat-view.tsx:183), so the strip starts
        // 4px below the message text — the native markdown column has no such
        // bottom padding, so the strip carries it as top inset (grown into the
        // reserved height: reveal still never shifts layout). User rows are
        // flush: the Timestamp follows the bubble HStack directly (VStack gap
        // defaults to 0 in mugen), the label's centering inside the 16px lane
        // is all the gap the original has.
        let strip = row.timestamp.map(|ms| {
            div()
                .h(px(if is_user_row { 16.0 } else { 20.0 }))
                .when(!is_user_row, |el| el.pt(px(4.0)))
                .w_full()
                .flex()
                .items_center()
                // No horizontal inset: the original's `px-1` netted out flush
                // because its message text was inset by the same amount (group
                // padding 4 + inner VStack 4 = 8 = group 4 + px-1 4). Here the
                // markdown text / user bubble sit AT the content column edges,
                // so the label must too — assistant label's left edge on the
                // text's first-character x, user label's right edge on the
                // bubble's right edge (user-reported 4px drift).
                .when(is_user_row, |el| el.justify_end())
                .when(hovered, |el| {
                    el.child(motion::fade_quick(
                        SharedString::from(format!("ts-{}", row.id)),
                        div()
                            .text_size(px(11.0))
                            .text_color(theme.text_muted.opacity(0.55))
                            .child(SharedString::from(format_timestamp(ms, &chrono::Local))),
                    ))
                })
        });
        let entry_id = row.entry_id.clone();
        let row_id = row.id.clone();
        div()
            .id(row.id.clone())
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                if *hovered {
                    let next = Some((row_id.clone(), entry_id.clone()));
                    if this.hovered_entry != next {
                        let entry_changed = this
                            .hovered_entry
                            .as_ref()
                            .is_none_or(|(_, entry)| entry != &entry_id);
                        this.hovered_entry = next;
                        if entry_changed {
                            cx.notify();
                        }
                    }
                } else if this
                    .hovered_entry
                    .as_ref()
                    .is_some_and(|(row, _)| row == &row_id)
                {
                    // Only the row that OWNS the current reveal may clear it —
                    // a stale leave from an earlier row must not blank the
                    // strip the newly entered row just lit.
                    this.hovered_entry = None;
                    cx.notify();
                }
            }))
            .w_full()
            .flex()
            .justify_center()
            .pt(px(top_gap))
            .pb(px(bottom_pad))
            // Wide gutters (comet `px-4 @3xl:px-12`) around the 46rem column.
            .px(px(48.0))
            .child(
                div()
                    .w_full()
                    .max_w(px(MAX_CONTENT_WIDTH))
                    .min_w_0()
                    .child(inner)
                    .children(strip),
            )
            .into_any_element()
    }

    /// Copy-button wiring for one row's code blocks ([`render::CopyUi`]):
    /// click writes the block's code to the clipboard and shows a transient
    /// "Copied" check on that block for ~1.2s (overlay — no layout shift).
    fn copy_ui_for(&self, row_id: &SharedString, cx: &mut Context<Self>) -> render::CopyUi {
        let copied_ix = self
            .copied_code
            .as_ref()
            .filter(|(id, _)| id == row_id)
            .map(|(_, ix)| *ix);
        let row_key = row_id.clone();
        let entity = cx.weak_entity();
        let handler: Rc<dyn Fn(usize, SharedString, &mut Window, &mut gpui::App)> =
            Rc::new(move |ix, code, _window, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(code.to_string()));
                let row_key = row_key.clone();
                entity
                    .update(cx, |this, cx| {
                        this.copied_code = Some((row_key, ix));
                        this.copied_clear = Some(cx.spawn(async move |this, cx| {
                            cx.background_executor()
                                .timer(Duration::from_millis(1200))
                                .await;
                            this.update(cx, |this, cx| {
                                this.copied_code = None;
                                this.copied_clear = None;
                                cx.notify();
                            })
                            .ok();
                        }));
                        cx.notify();
                    })
                    .ok();
            });
        render::CopyUi { handler, copied_ix }
    }

    /// Request highlights for the code blocks of a tree. `only` limits to one
    /// block index (split rows); `None` covers the whole tree (live rows).
    fn code_highlight_for(
        &mut self,
        row_id: &SharedString,
        tree: &Arc<BlockTree>,
        only: Option<usize>,
        cx: &mut Context<Self>,
    ) -> HashMap<usize, Option<Arc<comet_syntax::HighlightedDocument>>> {
        let mut out = HashMap::new();
        for (ix, top) in tree.blocks.iter().enumerate() {
            if only.is_some_and(|o| o != ix) {
                continue;
            }
            if let Block::CodeBlock { language, code } = &top.block
                && let Some(lang) = language
                    .as_deref()
                    .and_then(comet_syntax::language_for_alias)
            {
                out.insert(
                    ix,
                    self.highlights.request(row_id.clone(), ix, lang, code, cx),
                );
            }
        }
        out
    }

    fn render_tool_group(
        &mut self,
        row_id: &SharedString,
        tools: &Arc<Vec<ToolItem>>,
        auto_open: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let fold = self.folds.get(row_id).copied().unwrap_or_default();
        let open = fold.open.unwrap_or(auto_open);
        let mut target = chips_height(tools.len());
        if open {
            for (tool_ix, tool) in tools.iter().enumerate() {
                let detail_id: SharedString = format!("{row_id}#tool-{tool_ix}").into();
                let stat_rows = tool.diff_stats.as_deref().map_or(0, Vec::len);
                if stat_rows > 0 {
                    target += 20.0 * stat_rows as f32;
                } else if tool.diff_ref.is_some() {
                    target += 20.0;
                }
                if !self
                    .folds
                    .get(&detail_id)
                    .is_some_and(|state| state.open.unwrap_or(false))
                {
                    continue;
                }
                let key =
                    self.chat_id
                        .as_ref()
                        .zip(tool.diff_ref.as_ref())
                        .map(|(owner, diff_ref)| ToolDiffFetchKey {
                            owner: owner.clone(),
                            part_id: tool.id.to_string(),
                            diff_ref: diff_ref.to_string(),
                        });
                target += tool_diff_detail_height(
                    key.as_ref().and_then(|key| self.tool_diff_fetches.get(key)),
                );
            }
        } else {
            target = 0.0;
        }
        let summary = tool_group_summary(tools);

        let toggle_id = row_id.clone();
        let current_height = target;
        // Header (comet tool-group.tsx): a small chevron tile centered over the
        // chips' guide rail, then the quiet 12px summary.
        let header = div()
            .id(SharedString::from(format!("{row_id}-hdr")))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .px(px(4.0))
            .h(px(26.0))
            .cursor_pointer()
            .text_size(px(12.0))
            // Quiet even when children failed: agents routinely have failed
            // probes mid-work, and a red HEADER read as "this whole step
            // broke" (user report). Failures still show on the individual
            // chips (destructive tint, comet tool-chip.tsx) and in the
            // summary's "· N failed" count.
            .text_color(theme.text_muted)
            .hover(|s| s.text_color(theme.text))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.toggle_fold(toggle_id.clone(), current_height, auto_open);
                cx.notify();
            }))
            .child(
                div()
                    .size(px(18.0))
                    .flex_none()
                    .rounded(px(5.0))
                    .bg(crate::theme::ink(0.06))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(10.0))
                    .text_color(theme.text_muted.opacity(0.7))
                    .child(SharedString::from(if open { "▾" } else { "▸" })),
            )
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .child(SharedString::from(summary)),
            );

        let mut tool_rows: Vec<AnyElement> = Vec::new();
        for (tool_ix, tool) in tools.iter().enumerate() {
            tool_rows.push(tool_chip(tool, theme));
            let detail_id: SharedString = format!("{row_id}#tool-{tool_ix}").into();
            let detail_open = self
                .folds
                .get(&detail_id)
                .is_some_and(|state| state.open.unwrap_or(false));
            let key = self
                .chat_id
                .as_ref()
                .zip(tool.diff_ref.as_ref())
                .map(|(owner, diff_ref)| ToolDiffFetchKey {
                    owner: owner.clone(),
                    part_id: tool.id.to_string(),
                    diff_ref: diff_ref.to_string(),
                });
            if detail_open && let Some(key) = key.clone() {
                self.begin_tool_diff_fetch(key, cx);
            }

            let mut stats: Vec<AnyElement> = Vec::new();
            if let Some(diff_stats) = &tool.diff_stats {
                for (stat_ix, stat) in diff_stats.iter().enumerate() {
                    let label: SharedString =
                        format!("{} · +{} −{}", stat.path, stat.additions, stat.deletions).into();
                    let row = div()
                        .id(SharedString::from(format!("{detail_id}-stat-{stat_ix}")))
                        .h(px(20.0))
                        .ml(px(37.0))
                        .flex()
                        .items_center()
                        .text_size(px(11.0))
                        .text_color(theme.text_faint)
                        .child(label);
                    let row = if tool.diff_ref.is_some() {
                        let toggle_id = detail_id.clone();
                        row.cursor_pointer()
                            .hover(|element| element.text_color(theme.text_muted))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.toggle_tool_diff_detail(toggle_id.clone());
                                cx.notify();
                            }))
                    } else {
                        row
                    };
                    stats.push(row.into_any_element());
                }
            } else if tool.diff_ref.is_some() {
                let toggle_id = detail_id.clone();
                stats.push(
                    div()
                        .id(SharedString::from(format!("{detail_id}-open")))
                        .h(px(20.0))
                        .ml(px(37.0))
                        .flex()
                        .items_center()
                        .cursor_pointer()
                        .text_size(px(11.0))
                        .text_color(theme.text_faint)
                        .hover(|element| element.text_color(theme.text_muted))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.toggle_tool_diff_detail(toggle_id.clone());
                            cx.notify();
                        }))
                        .child(SharedString::from("View diff"))
                        .into_any_element(),
                );
            }
            tool_rows.extend(stats);

            if !detail_open {
                continue;
            }
            let ready = key.as_ref().and_then(|key| {
                self.tool_diff_fetches
                    .get(key)
                    .and_then(|state| match state {
                        ToolDiffFetchState::Ready { diff, file, .. } => {
                            Some((diff.clone(), file.clone()))
                        }
                        _ => None,
                    })
            });
            let detail: AnyElement = match ready {
                Some((diff, file)) => crate::changes::render_file_body_with_syntax(
                    &file,
                    self.tool_diff_highlights_for(row_id, tool_ix, &diff, cx),
                    theme,
                ),
                None if key.as_ref().is_some_and(|key| {
                    matches!(
                        self.tool_diff_fetches.get(key),
                        Some(ToolDiffFetchState::Loading { .. })
                    )
                }) =>
                {
                    div()
                        .h(px(TOOL_DIFF_STATUS_HEIGHT))
                        .flex()
                        .items_center()
                        .text_size(px(11.0))
                        .text_color(theme.text_faint)
                        .child(SharedString::from("Loading diff…"))
                        .into_any_element()
                }
                _ => div()
                    .h(px(TOOL_DIFF_STATUS_HEIGHT))
                    .flex()
                    .items_center()
                    .text_size(px(11.0))
                    .text_color(theme.text_faint)
                    .child(SharedString::from("Diff details unavailable"))
                    .into_any_element(),
            };
            tool_rows.push(
                div()
                    .ml(px(37.0))
                    .pt(px(TOOL_DIFF_DETAIL_TOP_PAD))
                    .overflow_hidden()
                    .child(detail)
                    .into_any_element(),
            );
        }

        let chips = div()
            .pt(px(CHIPS_TOP_PAD))
            .flex()
            .flex_col()
            .gap(px(CHIP_GAP))
            .children(tool_rows);

        // Fold body: 200ms committed-height tween on a USER toggle only — and
        // only within a short window of the click. Auto-open (streaming) and
        // content growth never tween, and a SETTLED fold renders at its static
        // height: leaving the tween armed replayed it on every remount, which
        // in a virtualized list means every scroll-back-into-view (only `open`
        // toggles animate — composes with the stick spring).
        let animating = fold.epoch > 0
            && fold
                .toggled_at
                .is_some_and(|at| at.elapsed() < FOLD_TWEEN_WINDOW);
        let body: AnyElement = if animating {
            let from = fold.from;
            div()
                .overflow_hidden()
                .child(chips)
                .with_animation(
                    SharedString::from(format!("{row_id}-fold{}", fold.epoch)),
                    RESIZE.animation(),
                    move |el, t| el.h(px(motion::lerp(from, target, t))),
                )
                .into_any_element()
        } else {
            div()
                .overflow_hidden()
                .h(px(target))
                .child(chips)
                .into_any_element()
        };

        div()
            .flex()
            .flex_col()
            .child(header)
            .child(body)
            .into_any_element()
    }
}

fn transcript_owner_changed(previous: Option<&ServerRef>, selected: Option<&ServerRef>) -> bool {
    previous != selected
}

/// A sent message's text with its file-mention chips. The same recipe as the
/// markdown renderer's inline code (`flat_text_element`): chip ranges shape in
/// the mono font at `code_text` violet, [`StyledText`] supplies wrapped glyph
/// geometry through its layout handle, and a canvas paints the rounded
/// `code_wash` *beneath* the glyphs — so chips wrap, clip, and scroll exactly
/// like the text they decorate.
///
/// Per-frame cost while an assistant message streams below: shaping hits
/// gpui's line-layout cache (identical text + runs ⇒ reuse) and the underlay
/// repaints O(chips) quads — no layout work, no re-projection (spans were
/// computed once in [`rows_for_entry`]).
/// The user bubble's text: runs split at mention-chip boundaries (one plain
/// run when there are none), with the same selection machinery as rendered
/// markdown — the element registers into the frame's document-ordered
/// registry, so drags select, span into adjacent rows, and Cmd+C copies.
fn user_bubble_text(
    row_id: &SharedString,
    text: SharedString,
    mentions: Arc<Vec<crate::composer::SentMentionSpan>>,
    theme: &Theme,
) -> AnyElement {
    // Split runs at chip boundaries (spans are in order): body text keeps the
    // sans font, chips read as inline code. Size/line-height flow from the
    // bubble's div like every text child.
    let body_run = |len: usize| TextRun {
        len,
        font: gpui::font(theme.font_sans.clone()),
        color: theme.text,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let chip_run = |len: usize| TextRun {
        len,
        font: gpui::font(theme.font_mono.clone()),
        color: theme.code_text,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let mut runs = Vec::with_capacity(mentions.len() * 2 + 1);
    let mut at = 0;
    for span in mentions.iter() {
        if at < span.range.start {
            runs.push(body_run(span.range.start - at));
        }
        runs.push(chip_run(span.range.len()));
        at = span.range.end;
    }
    if at < text.len() {
        runs.push(body_run(text.len() - at));
    }
    let styled = StyledText::new(text.clone()).with_runs(runs);
    let layout = styled.layout().clone();
    let wash = theme.code_wash;
    let sel_key: std::sync::Arc<str> = format!("{row_id}:u").into();
    let sel_theme = theme.clone();
    let underlay = canvas(
        |_, _, _| (),
        move |_, _, window, _| {
            for span in mentions.iter() {
                for rect in render::range_rects(&layout, &span.range, 0.0, 2.0) {
                    window.paint_quad(quad(
                        rect,
                        px(5.0),
                        wash,
                        px(0.0),
                        gpui::transparent_black(),
                        BorderStyle::default(),
                    ));
                }
            }
            render::paint_text_selection(window, &sel_key, &text, &layout, &sel_theme);
        },
    )
    .absolute()
    .size_full();
    div()
        .relative()
        .child(underlay)
        .child(styled)
        .into_any_element()
}

/// D14's residual, rendered: the note the user typed when denying the
/// `ApprovalCard` above it (`RowKind::DenyNote`). Same 34px chip shape as
/// [`error_chip`]/[`notice_chip`] — a card was the wrong shell here, because
/// the approval card's 56px must stay fixed across every decision
/// (`approval_card`'s doc comment) and a note has no length bound.
///
/// Neutral tones, not danger red: a denial is the user's own choice, not a
/// failure (`.agents/rules/user-facing-errors.md`, and `approval_card`'s own
/// comment makes the identical call for the card itself). A long note
/// truncates on the row and carries the full text on hover, mirroring
/// `notice_chip`'s tooltip rather than inventing a second pattern.
fn deny_note_chip(chip_id: SharedString, message: SharedString, theme: &Theme) -> AnyElement {
    let tooltip_text = message.clone();
    let row = div()
        .id(chip_id)
        .h(px(34.0))
        .w_full()
        .flex()
        .items_center()
        .gap(px(8.0))
        .overflow_hidden()
        .rounded(px(10.0))
        .border_1()
        .border_color(crate::theme::hairline(0.08))
        .bg(crate::theme::ink(0.045))
        .px(px(8.0))
        .text_size(px(12.0))
        .child(
            div()
                .flex_none()
                .size(px(20.0))
                .rounded(px(6.0))
                .bg(crate::theme::ink(0.09))
                .flex()
                .items_center()
                .justify_center()
                .child(
                    crate::icons::icon(crate::icons::CHAT_ROUND_LINE)
                        .size(px(12.0))
                        .text_color(theme.text_muted),
                ),
        )
        .child(
            div()
                .flex_none()
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(theme.text_muted)
                .child(SharedString::from("Deny note")),
        )
        .child(
            div()
                .min_w_0()
                .flex_1()
                .truncate()
                .text_color(theme.text.opacity(0.8))
                .child(message),
        )
        .tooltip(move |_, cx| {
            let detail = tooltip_text.clone();
            cx.new(|_| NoticeDetailTooltip { detail }).into()
        });
    div().py(px(4.0)).w_full().child(row).into_any_element()
}

/// The transcript ErrorChip — an exact port of comet chat-view.tsx
/// `ErrorChip`: a 34px row (`rounded-[10px] border border-red-400/[0.16]
/// bg-red-400/[0.05] px-2 text-[12px]`) with a 20px red-washed tile holding a
/// 12px DangerTriangle (`bg-red-400/[0.12] text-red-300/80`), a medium
/// "Error" label, then the human message truncating at `text-foreground/80` —
/// a subtle red-tinted wash, never a bare red-stroke box.
fn error_chip(message: SharedString, theme: &Theme) -> AnyElement {
    let red_300 = theme.danger_muted; // tailwind red-300
    let danger = theme.danger; // red-400
    div()
        .py(px(4.0))
        .w_full()
        .child(
            div()
                .h(px(34.0))
                .w_full()
                .flex()
                .items_center()
                .gap(px(8.0))
                .overflow_hidden()
                .rounded(px(10.0))
                .border_1()
                .border_color(danger.opacity(0.16))
                .bg(danger.opacity(0.05))
                .px(px(8.0))
                .text_size(px(12.0))
                .child(
                    div()
                        .flex_none()
                        .size(px(20.0))
                        .rounded(px(6.0))
                        .bg(danger.opacity(0.12))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            crate::icons::icon(crate::icons::DANGER_TRIANGLE)
                                .size(px(12.0))
                                .text_color(red_300.opacity(0.8)),
                        ),
                )
                .child(
                    div()
                        .flex_none()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(red_300.opacity(0.8))
                        .child(SharedString::from("Error")),
                )
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .truncate()
                        .text_color(theme.text.opacity(0.8))
                        .child(message),
                ),
        )
        .into_any_element()
}

/// A passive one-line chip marking a question the agent asked — the
/// interactive controls live in the composer (chat-view.tsx `InputChip`):
/// 34px row, `rounded-[10px] border-white/[0.08] bg-white/[0.045] px-2
/// text-[12px]`, a 20px `bg-white/[0.09]` icon tile with a 12px
/// ChatRoundLine, the medium "Question" label, then the truncating value —
/// the first question's header once resolved, "Awaiting your answer…" while
/// pending. Neutral tones throughout; resolution never recolors the chip.
fn input_chip(header: SharedString, resolved: bool, theme: &Theme) -> AnyElement {
    let value: SharedString = if resolved {
        header
    } else {
        "Awaiting your answer…".into()
    };
    div()
        .py(px(4.0))
        .w_full()
        .child(
            div()
                .h(px(34.0))
                .w_full()
                .flex()
                .items_center()
                .gap(px(8.0))
                .overflow_hidden()
                .rounded(px(10.0))
                .border_1()
                .border_color(crate::theme::hairline(0.08))
                .bg(crate::theme::ink(0.045))
                .px(px(8.0))
                .text_size(px(12.0))
                .child(
                    div()
                        .flex_none()
                        .size(px(20.0))
                        .rounded(px(6.0))
                        .bg(crate::theme::ink(0.09))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            crate::icons::icon(crate::icons::CHAT_ROUND_LINE)
                                .size(px(12.0))
                                .text_color(theme.text_muted),
                        ),
                )
                .child(
                    div()
                        .flex_none()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text_muted)
                        .child(SharedString::from("Question")),
                )
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .truncate()
                        .text_color(theme.text.opacity(0.9))
                        .child(value),
                ),
        )
        .into_any_element()
}

/// The transcript notice chip — a quiet sibling of [`error_chip`] for
/// provider notices (compaction, model reroute, retry, MCP status). Amber
/// `warning_muted` for a state to resolve, muted neutrals for information —
/// never `danger`, these are not failures. Layout constants are identical for
/// both severities: only paint differs (.agents/rules/gpui-ui.md). No
/// `opacity()` on the summary text — it is text to read (0.2a's contrast
/// lesson); the wash/border alphas are paint on color tokens, matching
/// [`error_chip`]'s idiom. `detail` renders as a hover tooltip; the caller
/// already suppressed it when it would duplicate the summary.
fn notice_chip(
    chip_id: SharedString,
    summary: SharedString,
    detail: Option<SharedString>,
    severity: NoticeSeverity,
    occurrences: u32,
    theme: &Theme,
) -> AnyElement {
    let warning = severity == NoticeSeverity::Warning;
    let (border, wash, tile, tint) = if warning {
        (
            theme.warning_muted.opacity(0.16),
            theme.warning_muted.opacity(0.05),
            theme.warning_muted.opacity(0.12),
            theme.warning_muted,
        )
    } else {
        (
            crate::theme::hairline(0.08),
            crate::theme::ink(0.045),
            crate::theme::ink(0.09),
            theme.text_muted,
        )
    };
    let icon_path = if warning {
        crate::icons::DANGER_TRIANGLE
    } else {
        crate::icons::INFO_CIRCLE
    };
    let mut row = div()
        .id(chip_id)
        .h(px(34.0))
        .w_full()
        .flex()
        .items_center()
        .gap(px(8.0))
        .overflow_hidden()
        .rounded(px(10.0))
        .border_1()
        .border_color(border)
        .bg(wash)
        .px(px(8.0))
        .text_size(px(12.0))
        .child(
            div()
                .flex_none()
                .size(px(20.0))
                .rounded(px(6.0))
                .bg(tile)
                .flex()
                .items_center()
                .justify_center()
                .child(
                    crate::icons::icon(icon_path)
                        .size(px(12.0))
                        .text_color(tint),
                ),
        )
        .child(
            div()
                .flex_none()
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(tint)
                .child(SharedString::from("Notice")),
        )
        .child(
            div()
                .min_w_0()
                .flex_1()
                .truncate()
                .text_color(theme.text)
                .child(summary),
        );
    if occurrences > 1 {
        row = row.child(
            div()
                .flex_none()
                .text_color(theme.text_muted)
                .child(SharedString::from(format!("×{occurrences}"))),
        );
    }
    if let Some(detail) = detail {
        row = row.tooltip(move |_, cx| {
            let detail = detail.clone();
            cx.new(|_| NoticeDetailTooltip { detail }).into()
        });
    }
    div().py(px(4.0)).w_full().child(row).into_any_element()
}

/// The transcript's approval card — what the provider asked permission to do.
///
/// Two lines, **56px in every kind and every state**: layout never varies with
/// the decision, so an approval resolving cannot reflow the transcript under
/// the user's scroll position (`.agents/rules/gpui-ui.md`). Height, padding,
/// icon size and tile size are the SAME literals in every arm below — only
/// the `match` on [`ApprovalPaint`] may vary between them.
///
/// Open and `Denied` both read in neutral tones — a denial is a choice the
/// user made, not a failure, so `danger`/red is wrong for it
/// (`.agents/rules/user-facing-errors.md`); `Allowed` reads `success_muted`,
/// `Expired` reads amber (`warning_muted`) because it is the one state the
/// user did not choose. Icon plus tint/caption-color is what has to carry the
/// distinction on its own — this project has shipped a card that painted
/// every decision identically twice before.
///
/// Passive by construction, like [`input_chip`]: the decision controls live in
/// the composer, so there is no control here to disable when the approval is
/// no longer answerable.
/// The transcript's card frame — the shell every card here shares.
///
/// Extracted rather than repeated because the `MessagePart::Subagent` no-op
/// arm warned about exactly this before either later card existed: designing
/// them apart is how a transcript ends up with two unrelated card idioms.
/// Three callers of one helper cannot drift; three hand-built cards will.
///
/// `fixed_height` is `Some` only for the approval card, whose one-line body
/// has always been 56px. The others grow with their content — the transcript
/// is a measured `list`, not a uniform one, so variable height costs nothing.
fn card_frame(border: gpui::Hsla, wash: gpui::Hsla, fixed_height: Option<f32>) -> gpui::Div {
    let base = div()
        .w_full()
        .flex()
        .flex_col()
        .gap(px(Theme::SPACE_XS))
        .overflow_hidden()
        .rounded(px(Theme::PANEL_RADIUS))
        .border_1()
        .border_color(border)
        .bg(wash)
        .px(px(Theme::SPACE_SM))
        .text_size(px(12.0));
    match fixed_height {
        Some(h) => base.h(px(h)).justify_center(),
        None => base.py(px(Theme::SPACE_SM)),
    }
}

/// The 20px icon tile every card's header row opens with.
fn card_tile(icon_path: &'static str, bg: gpui::Hsla, tint: gpui::Hsla) -> gpui::Div {
    div()
        .flex_none()
        .size(px(20.0))
        .rounded(px(Theme::CONTROL_RADIUS))
        .bg(bg)
        .flex()
        .items_center()
        .justify_center()
        .child(
            crate::icons::icon(icon_path)
                .size(px(12.0))
                .text_color(tint),
        )
}

fn approval_card(
    label: &'static str,
    detail: SharedString,
    state: Option<SharedString>,
    paint: ApprovalPaint,
    theme: &Theme,
) -> AnyElement {
    let (border, wash, tile, tint, icon_path, caption_color) = match paint {
        ApprovalPaint::Allowed => (
            theme.success_muted.opacity(0.16),
            theme.success_muted.opacity(0.05),
            theme.success_muted.opacity(0.12),
            theme.success_muted,
            crate::icons::CHECK,
            theme.text_muted,
        ),
        ApprovalPaint::Denied => (
            crate::theme::hairline(0.08),
            crate::theme::ink(0.045),
            crate::theme::ink(0.09),
            theme.text_muted,
            crate::icons::CLOSE_CIRCLE,
            // The one caption that departs from the muted tone every other
            // state uses — a refusal has to read differently from an
            // approval at a glance, not just carry a different word.
            theme.text,
        ),
        ApprovalPaint::Expired => (
            theme.warning_muted.opacity(0.16),
            theme.warning_muted.opacity(0.05),
            theme.warning_muted.opacity(0.12),
            theme.warning_muted,
            crate::icons::KEY_MINIMALISTIC,
            theme.text_muted,
        ),
        ApprovalPaint::Open => (
            crate::theme::hairline(0.08),
            crate::theme::ink(0.045),
            crate::theme::ink(0.09),
            theme.text_muted,
            crate::icons::KEY_MINIMALISTIC,
            theme.text_muted,
        ),
    };
    div()
        .py(px(4.0))
        .w_full()
        .child(
            card_frame(border, wash, Some(56.0))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(Theme::SPACE_SM))
                        .child(card_tile(icon_path, tile, tint))
                        .child(
                            div()
                                .flex_none()
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(tint)
                                .child(SharedString::from(label)),
                        )
                        .child(div().flex_1())
                        .children(state.map(|state| {
                            div()
                                .flex_none()
                                .min_w_0()
                                .truncate()
                                .text_color(caption_color)
                                .child(state)
                        })),
                )
                .child(
                    div()
                        .min_w_0()
                        .w_full()
                        .truncate()
                        .text_color(theme.text)
                        .child(detail),
                ),
        )
        .into_any_element()
}

/// Hover card for a notice's `detail` line (same frame as the harness-rail
/// tooltip in `pickers.rs`).
struct NoticeDetailTooltip {
    detail: SharedString,
}

impl gpui::Render for NoticeDetailTooltip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx);
        motion::fade_quick(
            SharedString::from("notice-detail-tooltip"),
            div()
                .max_w(px(360.0))
                .px(px(8.0))
                .py(px(6.0))
                .rounded(px(5.0))
                .border_1()
                .border_color(theme.border_strong)
                .bg(theme.surface_raised)
                .text_size(px(11.0))
                .text_color(theme.text_muted)
                .child(self.detail.clone()),
        )
    }
}

/// A small glyph standing in for the tool's icon (comet uses an icon set; a
/// quiet monochrome character keeps the tile without shipping SVGs).
/// The glyph for a tool call (comet tool-chip.tsx `toolIcon`, Solar set).
fn tool_icon_path(call: &ToolCall) -> &'static str {
    match call {
        ToolCall::Exec { .. } => crate::icons::COMMAND,
        ToolCall::ReadFile { .. } | ToolCall::ApplyPatch { .. } => crate::icons::DOCUMENT,
        ToolCall::WriteFile { .. } => crate::icons::DOCUMENT_ADD,
        ToolCall::EditFile { .. } => crate::icons::PEN,
        ToolCall::Search { .. } => crate::icons::MAGNIFER,
        ToolCall::Glob { .. } => crate::icons::FOLDER_WITH_FILES,
        ToolCall::WebFetch { .. } | ToolCall::WebSearch { .. } => crate::icons::GLOBAL,
        ToolCall::Todo { .. } => crate::icons::CHECKLIST,
        ToolCall::Mcp { .. } | ToolCall::Unknown { .. } => crate::icons::WIDGET,
    }
}

/// One tool chip row: a guide rail on the left (continuous across stacked
/// chips — the rail spans the row's full height) threading the chips to their
/// group toggle, then the chip card (comet tool-chip.tsx).
fn tool_chip(tool: &ToolItem, theme: &Theme) -> AnyElement {
    let (label, detail) = tool_chip_content(&tool.call);
    let tint = if tool.is_error {
        theme.danger
    } else {
        theme.text_muted
    };
    div()
        .h(px(CHIP_HEIGHT))
        .w_full()
        .flex_none()
        .flex()
        .flex_row()
        .items_center()
        // Guide rail: hairline centered under the header's chevron tile.
        .child(
            div()
                .ml(px(12.0))
                .h_full()
                .w(px(1.0))
                .flex_none()
                .bg(crate::theme::ink(0.08)),
        )
        .child(
            div()
                .ml(px(12.0))
                .h(px(CHIP_CARD_HEIGHT))
                .min_w_0()
                .flex_1()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .overflow_hidden()
                .rounded(px(9.0))
                .border_1()
                .border_color(crate::theme::hairline(0.07))
                .bg(crate::theme::ink(0.03))
                .px(px(8.0))
                .text_size(px(12.0))
                .child(
                    // Icon tile (`size-[18px] rounded-[5px] bg-white/[0.08]`,
                    // icon size-3).
                    div()
                        .size(px(18.0))
                        .flex_none()
                        .rounded(px(5.0))
                        .bg(crate::theme::ink(0.08))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            crate::icons::icon(tool_icon_path(&tool.call))
                                .size(px(12.0))
                                .text_color(theme.text_muted),
                        ),
                )
                .child(
                    div()
                        .flex_none()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(tint)
                        .child(SharedString::from(label)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(if tool.is_error {
                            theme.danger
                        } else {
                            theme.text.opacity(0.85)
                        })
                        .child(SharedString::from(detail)),
                ),
        )
        .into_any_element()
}

fn entry_fingerprint(entry: &SessionMessageEntry, pending: bool) -> u64 {
    let mut acc: Vec<u8> = Vec::with_capacity(entry.parts.len() * 8 + 16);
    acc.extend_from_slice(entry.id.as_bytes());
    acc.push(match entry.status {
        None => 0,
        Some(MessageStatus::Streaming) => 1,
        Some(MessageStatus::Complete) => 2,
        Some(MessageStatus::Aborted) => 3,
    });
    acc.push(pending as u8);
    for part in &entry.parts {
        acc.extend_from_slice(part.id().as_bytes());
        acc.extend_from_slice(&(part.byte_len() as u64).to_le_bytes());
        if let MessagePart::Tool {
            is_error,
            resolved,
            diff_ref,
            diff_stats,
            ..
        } = part
        {
            acc.push(*is_error as u8 | (*resolved as u8) << 1);
            if let Some(diff_ref) = diff_ref {
                acc.extend_from_slice(diff_ref.as_bytes());
            }
            if let Some(diff_stats) = diff_stats {
                for stat in diff_stats {
                    acc.extend_from_slice(stat.path.as_bytes());
                    acc.extend_from_slice(&stat.additions.to_le_bytes());
                    acc.extend_from_slice(&stat.deletions.to_le_bytes());
                }
            }
        }
        if let MessagePart::Input { resolved, .. } = part {
            acc.push(0x10 | *resolved as u8);
        }
        // `byte_len` sees `description`/`activity`/`summary` text but is
        // blind to these four fields — a `Running` -> `Completed` transition
        // with no length change in the text fields would otherwise produce a
        // byte-identical fingerprint and the row cache would miss the
        // invalidation (mirrors the `Tool`/`Input` arms above, same reason).
        if let MessagePart::Subagent {
            status,
            total_tokens,
            duration_ms,
            tool_uses,
            activity,
            summary,
            ..
        } = part
        {
            acc.push(match status {
                SubagentStatus::Running => 0,
                SubagentStatus::Completed => 1,
                SubagentStatus::Failed => 2,
                SubagentStatus::Cancelled => 3,
            });
            acc.push(total_tokens.is_some() as u8);
            acc.extend_from_slice(&total_tokens.unwrap_or(0).to_le_bytes());
            acc.push(duration_ms.is_some() as u8);
            acc.extend_from_slice(&duration_ms.unwrap_or(0).to_le_bytes());
            acc.push(tool_uses.is_some() as u8);
            acc.extend_from_slice(&tool_uses.unwrap_or(0).to_le_bytes());
            // `byte_len` sums text LENGTHS, so two activity lines of equal
            // length ("Reading a.md" -> "Reading b.md") fingerprint
            // identically and the live line freezes on screen while the agent
            // works. Fold the CONTENT, not the length. Unreachable before
            // this slice, because nothing drew either field.
            acc.extend_from_slice(activity.as_deref().unwrap_or_default().as_bytes());
            acc.extend_from_slice(summary.as_deref().unwrap_or_default().as_bytes());
        }
        // Same blindness, sharper here: a checklist's ONLY interesting change
        // is usually a status moving with no text change at all
        // (`pending` -> `inProgress` -> `completed` on a fixed list), which
        // `byte_len` cannot see because it sums text lengths. Fingerprint the
        // item count and every status so the row cache invalidates on exactly
        // the transitions the card exists to show.
        if let MessagePart::Checklist { items, .. } = part {
            acc.push(0x20);
            acc.extend_from_slice(&(items.len() as u64).to_le_bytes());
            for item in items {
                acc.push(match item.status {
                    ChecklistStatus::Pending => 0,
                    ChecklistStatus::InProgress => 1,
                    ChecklistStatus::Completed => 2,
                    ChecklistStatus::Unknown => 3,
                });
                // An item gaining a subject on a later frame is a visible
                // change even when the status did not move — a resumed run's
                // text-less row becoming a named one.
                acc.push(item.text.is_some() as u8);
                acc.push(item.active_form.is_some() as u8);
                // And the CONTENT, for the same reason the `Subagent` arm
                // folds its activity line: `byte_len` sums lengths, so one
                // subject being rewritten to another of equal length is
                // invisible to it and the card would keep the old wording.
                acc.extend_from_slice(item.text.as_deref().unwrap_or_default().as_bytes());
                acc.extend_from_slice(item.active_form.as_deref().unwrap_or_default().as_bytes());
            }
        }
    }
    fnv1a(&acc)
}

impl Render for Transcript {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Release gpui-side decoded copies of any images the attachment LRU
        // evicted since the last frame (no-op when nothing was evicted).
        crate::attachments::flush_evicted(Some(window), cx);
        // Spring driver: one on_next_frame callback at a time; each tick
        // notifies, which re-enters render and schedules the next frame until
        // the spring parks. Reduced motion never schedules (sync snaps).
        if self.pinned
            && !motion::reduced_motion(cx)
            && !self.spring_scheduled
            && self.spring_should_run()
        {
            self.spring_scheduled = true;
            let entity = cx.weak_entity();
            window.on_next_frame(move |_, cx| {
                entity
                    .update(cx, |this: &mut Transcript, cx| {
                        this.spring_scheduled = false;
                        this.step_spring(cx);
                    })
                    .ok();
            });
        }
        let rail = self.render_rail(cx);
        // The scroll-to-bottom pill is rendered by the SHELL (conversation
        // region overlay): it must float just above the composer and paint
        // OVER the bottom fade gradient, which is a later sibling of this
        // outlet — an overlay here would be tinted by the fade.
        let root = div()
            .relative()
            .size_full()
            .min_h_0()
            // FIRST child ⇒ paints first: clears the frame's markdown text-
            // selection registry before any row's text elements re-register
            // (document paint order = selection order; see markdown/render.rs).
            .child(crate::markdown::render::selection_frame_reset())
            .child(
                list(self.list.clone(), cx.processor(Self::render_row))
                    .size_full()
                    .with_sizing_behavior(gpui::ListSizingBehavior::Auto),
            )
            .child(rail);
        // Full-size viewer for a clicked user-bubble thumbnail
        // (AttachmentPreviewDialog: bare lightbox, click closes).
        if let Some(preview) = self.attachment_preview.clone() {
            let weak = cx.weak_entity();
            return root.child(crate::attachments::lightbox(
                window.viewport_size(),
                &preview,
                move |_, cx| {
                    weak.update(cx, |this, cx| {
                        this.attachment_preview = None;
                        cx.notify();
                    })
                    .ok();
                },
            ));
        }
        root
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use comet_doc::MessagePart;

    #[test]
    fn dropping_a_highlight_entry_signals_its_background_work() {
        let cancellation = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let entry = HighlightEntry {
            key: DocumentHighlightKey::new(Lang::Rust, "fn main() {}"),
            document: None,
            cancellation: cancellation.clone(),
            _task: None,
        };
        drop(entry);
        assert_ne!(cancellation.load(std::sync::atomic::Ordering::Relaxed), 0);
    }

    #[test]
    fn background_tree_sitter_highlighting_observes_its_cancellation_token() {
        let cancellation = std::sync::atomic::AtomicUsize::new(1);
        let source = "fn cancelled() {}\n".repeat(2_048);
        assert_eq!(highlight_document(Lang::Rust, &source, &cancellation), None);
    }

    #[test]
    fn background_tree_sitter_highlighting_uses_the_requested_language() {
        let cancellation = std::sync::atomic::AtomicUsize::new(0);
        let document = highlight_document(
            Lang::Python,
            "def answer():\n    return 42\n",
            &cancellation,
        )
        .expect("python highlighting");
        assert_eq!(document.language, Lang::Python);
    }

    #[test]
    fn transcript_entries_do_not_outlive_bounded_cache() {
        let mut store = HighlightStore::default();
        let slot_key = (SharedString::from("row"), 0);
        let first_key = DocumentHighlightKey::new(Lang::Rust, "first");
        let first = Arc::new(comet_syntax::HighlightedDocument {
            language: Lang::Rust,
            lines: Vec::new(),
        });
        assert!(store.cache.insert(first_key, first.clone()));
        store.remember_cached_document(slot_key, first_key, &first);
        let first_weak = Arc::downgrade(&first);
        drop(first);

        for index in 0..128 {
            let key = DocumentHighlightKey::new(Lang::Rust, &format!("document-{index}"));
            store.cache.insert(
                key,
                Arc::new(comet_syntax::HighlightedDocument {
                    language: Lang::Rust,
                    lines: Vec::new(),
                }),
            );
        }

        assert!(store.cache.get(&first_key).is_none());
        assert!(
            first_weak.upgrade().is_none(),
            "a transcript slot kept a document alive after bounded-cache eviction"
        );
    }

    /// `byte_len` sees only `description`/`activity`/`summary` text, so a
    /// `Running` -> `Completed` transition with no length change anywhere in
    /// those fields must still change the fingerprint, the same way the
    /// `Tool`/`Input` arms cover `is_error`/`resolved` — otherwise the row
    /// cache never invalidates and a settled card renders stale.
    #[test]
    fn a_subagent_status_change_with_no_text_length_change_still_changes_the_fingerprint() {
        let running = comet_doc::SessionMessageEntry {
            id: "e1".into(),
            role: comet_doc::MessageRole::Assistant,
            parts: vec![MessagePart::Subagent {
                id: "sub-1".into(),
                task_id: "t1".into(),
                agent_type: "general-purpose".into(),
                description: "same length".into(),
                status: comet_proto::SubagentStatus::Running,
                activity: None,
                summary: None,
                total_tokens: None,
                duration_ms: None,
                tool_uses: None,
            }],
            created_at: 0,
            device_id: "d1".into(),
            status: None,
            continuation_of: None,
        };
        let mut completed = running.clone();
        completed.parts = vec![MessagePart::Subagent {
            id: "sub-1".into(),
            task_id: "t1".into(),
            agent_type: "general-purpose".into(),
            description: "same length".into(),
            status: comet_proto::SubagentStatus::Completed,
            activity: None,
            summary: None,
            total_tokens: Some(42),
            duration_ms: Some(100),
            tool_uses: Some(1),
        }];

        assert_ne!(
            entry_fingerprint(&running, false),
            entry_fingerprint(&completed, false),
            "a Running -> Completed transition with identical part text must \
             still change the fingerprint"
        );
    }

    /// The same blindness one layer down, and the one the live line actually
    /// hits: `byte_len` sums text LENGTHS, so an activity line moving between
    /// two readings of equal length fingerprints identically and the row cache
    /// never rebuilds — the live line freezes on screen while the agent works.
    /// Unreachable before slice 4.4, because nothing drew the field.
    #[test]
    fn an_equal_length_activity_change_still_changes_the_fingerprint() {
        let base = comet_doc::SessionMessageEntry {
            id: "e1".into(),
            role: comet_doc::MessageRole::Assistant,
            parts: vec![MessagePart::Subagent {
                id: "sub-1".into(),
                task_id: "t1".into(),
                agent_type: "Explore".into(),
                description: "Find the retry sites".into(),
                status: SubagentStatus::Running,
                activity: Some("Reading normalize.rs".into()),
                summary: None,
                total_tokens: None,
                duration_ms: None,
                tool_uses: None,
            }],
            created_at: 0,
            device_id: "d1".into(),
            status: Some(MessageStatus::Streaming),
            continuation_of: None,
        };
        let mut moved_on = base.clone();
        moved_on.parts = vec![MessagePart::Subagent {
            id: "sub-1".into(),
            task_id: "t1".into(),
            agent_type: "Explore".into(),
            description: "Find the retry sites".into(),
            status: SubagentStatus::Running,
            // Same byte length as "Reading normalize.rs".
            activity: Some("Reading discovery.rs".into()),
            summary: None,
            total_tokens: None,
            duration_ms: None,
            tool_uses: None,
        }];
        assert_eq!(
            "Reading normalize.rs".len(),
            "Reading discovery.rs".len(),
            "the fixture only tests anything while these are the same length"
        );
        assert_ne!(
            entry_fingerprint(&base, false),
            entry_fingerprint(&moved_on, false),
            "an equal-length activity change must still invalidate the row cache"
        );
    }

    #[test]
    fn equal_raw_chat_ids_on_different_servers_are_a_new_attachment() {
        let b = comet_proto::ServerRef::new(comet_proto::ServerId::new("server-b"), "chat-1");
        let c = comet_proto::ServerRef::new(comet_proto::ServerId::new("server-c"), "chat-1");

        assert!(transcript_owner_changed(Some(&b), Some(&c)));
    }

    // ---- streaming parse wiring (the transcript side, not the parser) ----

    #[test]
    fn live_row_parse_work_is_bounded_per_commit() {
        // Drive the EXACT wiring `rows_for` uses (`parse_for_row`) with the
        // prefix-extending commit snapshots the doc watch delivers, and prove
        // the per-commit parse work stays O(reparsed tail): a full-reparse
        // wiring would feed ~N/2 × final_len bytes through the parser across N
        // commits; the incremental path stays within a small multiple of the
        // final length regardless of N.
        let mut live_parsers = HashMap::new();
        let mut tree_cache = HashMap::new();
        let paragraph = "A paragraph of streaming prose that keeps arriving.\n\n";
        let commits = 120usize;
        let mut text = String::new();
        let mut total_parsed = 0usize;
        for i in 0..commits {
            // Each commit appends ~half a paragraph (crosses block boundaries).
            let chunk = &paragraph[..paragraph.len() / 2];
            text.push_str(if i % 2 == 0 {
                chunk
            } else {
                &paragraph[paragraph.len() / 2..]
            });
            let (tree, outcome) =
                parse_for_row(true, "e1#p1", &text, &mut live_parsers, &mut tree_cache);
            assert!(!tree.blocks.is_empty());
            let ParseOutcome::Incremental {
                parsed_bytes,
                stable_prefix_blocks,
            } = outcome
            else {
                panic!("streaming commit must take the incremental path");
            };
            total_parsed += parsed_bytes;
            // Per commit: never a full reparse once the doc has grown past the
            // tail window (last two complete blocks + the partial trailing
            // one + the delta ≤ 3 paragraphs here).
            assert!(
                parsed_bytes <= 3 * paragraph.len(),
                "commit {i}: parsed {parsed_bytes} bytes — not bounded by the tail window"
            );
            // The stable prefix grows with the doc — settled blocks are never
            // re-touched (this is what keeps render caches valid).
            assert!(stable_prefix_blocks + 2 >= tree.blocks.len().saturating_sub(1));
        }
        // Across the whole stream: work is commits × O(tail), an order of
        // magnitude under the ~commits × len/2 a full-reparse wiring costs.
        let final_len = text.len();
        let full_reparse_cost = commits * final_len / 2;
        assert!(total_parsed <= commits * 3 * paragraph.len());
        assert!(
            total_parsed * 10 < full_reparse_cost,
            "total parsed {total_parsed} vs full-reparse ~{full_reparse_cost}"
        );

        // Live→complete handoff: the completed part adopts the live parser's
        // exact tree without parsing a single byte.
        let (_, outcome) = parse_for_row(false, "e1#p1", &text, &mut live_parsers, &mut tree_cache);
        assert_eq!(outcome, ParseOutcome::Handoff);
        // And the settled cache serves repeats with no work at all.
        let (_, outcome) = parse_for_row(false, "e1#p1", &text, &mut live_parsers, &mut tree_cache);
        assert_eq!(outcome, ParseOutcome::Cached);
    }

    // ---- stick-to-bottom spring ----

    #[test]
    fn spring_converges_to_a_fixed_target() {
        let mut spring = StickSpring::new();
        let target = 400.0;
        let mut pos = 0.0;
        let mut frames = 0;
        while pos < target && frames < 600 {
            pos = spring.step(pos, target, 1.0);
            frames += 1;
        }
        assert_eq!(pos, target, "spring must land exactly on the target");
        assert!(
            frames < 300,
            "400px should converge within 5s of frames, took {frames}"
        );
        // Once landed it stays landed (and idles out).
        for _ in 0..120 {
            pos = spring.step(pos, target, 1.0);
            assert_eq!(pos, target);
        }
        assert!(spring.is_idle(), "no residual motion at rest");
    }

    #[test]
    fn spring_never_overshoots_or_oscillates() {
        let mut spring = StickSpring::new();
        let target = 250.0;
        let mut pos = 0.0;
        let mut last = pos;
        for _ in 0..600 {
            pos = spring.step(pos, target, 1.0);
            assert!(pos <= target, "overshoot: {pos} > {target}");
            assert!(
                pos >= last - 1e-3,
                "oscillation: position moved backwards {last} -> {pos}"
            );
            last = pos;
        }
        assert_eq!(pos, target);
    }

    #[test]
    fn spring_feed_forward_tracks_constant_growth() {
        // Target grows 2px/frame (≈120px/s — a typical stream). After warmup
        // the EMA feed-forward must carry the viewport at the same rate with a
        // bounded, stable lag — a glide, not 0,0,0,Npx steps.
        let growth = 2.0;
        let mut spring = StickSpring::new();
        let mut target = 600.0;
        let mut pos = 600.0;
        let mut deltas: Vec<f32> = Vec::new();
        for frame in 0..400 {
            target += growth;
            let next = spring.step(pos, target, 1.0);
            if frame >= 200 {
                deltas.push(next - pos);
            }
            pos = next;
        }
        // Steady state: per-frame movement ≈ growth rate…
        let mean = deltas.iter().sum::<f32>() / deltas.len() as f32;
        assert!(
            (mean - growth).abs() < 0.2,
            "steady-state speed {mean} should track growth {growth}"
        );
        // …with no stepping (every frame moves, none jumps).
        for d in &deltas {
            assert!(*d > 0.0, "viewport stalled mid-stream");
            assert!(*d < growth * 3.0, "viewport jumped: {d}px in one frame");
        }
        // The EMA growth estimate itself has locked on.
        assert!((spring.target_vel() - growth).abs() < 0.3);
        // Lag stays bounded by the chase lead.
        assert!(target - pos <= SPRING_CHASE_MAX_LEAD + growth);
    }

    #[test]
    fn spring_feed_forward_resets_when_target_shrinks() {
        let mut spring = StickSpring::new();
        let mut pos = 0.0;
        for i in 1..=50 {
            pos = spring.step(pos, 100.0 + i as f32 * 4.0, 1.0);
        }
        assert!(spring.target_vel() > 1.0);
        // A collapse (target shrinks by more than 1px) drops the estimate.
        spring.step(pos.min(120.0), 120.0, 1.0);
        assert_eq!(spring.target_vel(), 0.0);
    }

    #[test]
    fn spring_catchup_frames_glide_instead_of_teleporting() {
        // A 5-frame hitch advances roughly as far as 5 single steps would —
        // sub-stepped, still clamped at the target.
        let target = 300.0;
        let mut a = StickSpring::new();
        let mut pos_a = 0.0;
        for _ in 0..5 {
            pos_a = a.step(pos_a, target, 1.0);
        }
        let mut b = StickSpring::new();
        let pos_b = b.step(0.0, target, 5.0);
        assert!((pos_a - pos_b).abs() < 1.0, "{pos_a} vs {pos_b}");
        assert!(pos_b <= target);
    }

    #[test]
    fn restick_is_direction_aware() {
        // Scrolling away from the bottom never resticks, even inside the band
        // (a 20px wheel notch from the pinned bottom must break the pin).
        assert!(!Transcript::should_restick(20.0, 0.0));
        assert!(!Transcript::should_restick(69.0, 30.0));
        // Returning toward the bottom resticks once inside the 70px band…
        assert!(Transcript::should_restick(69.0, 120.0));
        assert!(Transcript::should_restick(0.0, 30.0));
        // …but not while still outside it.
        assert!(!Transcript::should_restick(200.0, 300.0));
        // No movement — leave the pin alone.
        assert!(!Transcript::should_restick(50.0, 50.0));
    }

    fn parse(_: &str, text: &str) -> Arc<BlockTree> {
        Arc::new(parse_full(text))
    }

    fn assistant(id: &str, status: MessageStatus, parts: Vec<MessagePart>) -> SessionMessageEntry {
        SessionMessageEntry {
            id: id.into(),
            role: MessageRole::Assistant,
            parts,
            created_at: 0,
            device_id: "dev".into(),
            status: Some(status),
            continuation_of: None,
        }
    }

    fn text_part(id: &str, text: &str) -> MessagePart {
        MessagePart::Text {
            id: id.into(),
            text: text.into(),
        }
    }

    fn tool_part(id: &str, command: &str) -> MessagePart {
        MessagePart::Tool {
            id: id.into(),
            call: ToolCall::Exec {
                command: command.into(),
            },
            is_error: false,
            resolved: true,
            diff_ref: None,
            diff_stats: None,
        }
    }

    fn notice_part(id: &str, summary: &str, detail: Option<&str>, occurrences: u32) -> MessagePart {
        MessagePart::Notice {
            id: id.into(),
            kind: comet_proto::NoticeKind::Retrying,
            severity: comet_proto::NoticeSeverity::Warning,
            summary: summary.into(),
            detail: detail.map(str::to_owned),
            key: Some("retry".into()),
            occurrences,
        }
    }

    #[test]
    fn notice_parts_become_notice_chip_rows() {
        let entry = assistant(
            "m1",
            MessageStatus::Complete,
            vec![
                text_part("t0", "before"),
                notice_part(
                    "n1",
                    "Retrying — attempt 1 of 3",
                    Some("Next attempt in 2s."),
                    1,
                ),
            ],
        );
        let rows = rows_for_entry(&entry, false, &mut parse);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].id.as_ref(), "m1#n1");
        let RowKind::NoticeChip {
            summary,
            detail,
            severity,
            occurrences,
        } = &rows[1].kind
        else {
            panic!("expected NoticeChip, got another row kind");
        };
        assert_eq!(summary.as_ref(), "Retrying — attempt 1 of 3");
        assert_eq!(
            detail.as_ref().map(|d| d.as_ref()),
            Some("Next attempt in 2s.")
        );
        assert_eq!(*severity, comet_proto::NoticeSeverity::Warning);
        assert_eq!(*occurrences, 1);
    }

    /// `Row::version` folds in `occurrences`, or a collapse (which changes
    /// only the counter once the summary settles) would not repaint.
    #[test]
    fn notice_row_version_changes_when_occurrences_bumps() {
        let one = assistant(
            "m1",
            MessageStatus::Complete,
            vec![notice_part("n0", "Retrying — attempt 3 of 3", None, 2)],
        );
        let two = assistant(
            "m1",
            MessageStatus::Complete,
            vec![notice_part("n0", "Retrying — attempt 3 of 3", None, 3)],
        );
        let r1 = rows_for_entry(&one, false, &mut parse);
        let r2 = rows_for_entry(&two, false, &mut parse);
        assert_eq!(r1[0].id, r2[0].id);
        assert_ne!(r1[0].version, r2[0].version);
    }

    // ---- slice 4.4: the subagent card ----

    #[allow(clippy::too_many_arguments)]
    fn subagent_part(
        id: &str,
        agent_type: &str,
        description: &str,
        status: SubagentStatus,
        activity: Option<&str>,
        summary: Option<&str>,
        total_tokens: Option<u64>,
        duration_ms: Option<u64>,
        tool_uses: Option<u32>,
    ) -> MessagePart {
        MessagePart::Subagent {
            id: id.into(),
            task_id: id.into(),
            agent_type: agent_type.into(),
            description: description.into(),
            status,
            activity: activity.map(str::to_owned),
            summary: summary.map(str::to_owned),
            total_tokens,
            duration_ms,
            tool_uses,
        }
    }

    fn agent_chip(id: &str) -> MessagePart {
        MessagePart::Tool {
            id: id.into(),
            // Exactly what reaches the document: `sanitize_tool_call` has
            // already stripped the input, so the name is all there is.
            call: ToolCall::Unknown {
                name: "Agent".into(),
                input: None,
            },
            is_error: false,
            resolved: true,
            diff_ref: None,
            diff_stats: None,
        }
    }

    fn subagent_row(rows: &[Row]) -> (&SharedString, SubagentPaint, Option<&SharedString>) {
        let row = rows
            .iter()
            .find(|r| matches!(r.kind, RowKind::SubagentCard { .. }))
            .expect("expected a SubagentCard row");
        let RowKind::SubagentCard {
            status_caption,
            paint,
            activity,
            ..
        } = &row.kind
        else {
            unreachable!()
        };
        (status_caption, *paint, activity.as_ref())
    }

    /// A live agent's card carries its activity line and says so.
    #[test]
    fn a_running_subagent_in_a_live_entry_draws_its_activity_line() {
        let entry = assistant(
            "m1",
            MessageStatus::Streaming,
            vec![subagent_part(
                "s1",
                "Explore",
                "Find the retry sites",
                SubagentStatus::Running,
                Some("Reading normalize.rs"),
                None,
                None,
                None,
                None,
            )],
        );
        let rows = rows_for_entry(&entry, false, &mut parse);
        let (caption, paint, activity) = subagent_row(&rows);
        assert_eq!(caption.as_ref(), "running");
        assert_eq!(paint, SubagentPaint::Running);
        assert_eq!(
            activity.map(|a| a.as_ref()),
            Some("Reading normalize.rs"),
            "a live card must show what the agent is doing"
        );
    }

    /// D57: send a new message while an agent is working and Comet never
    /// learns the outcome. The card reports where it froze, and stops looking
    /// alive — the entry around it is finished even though its own status is
    /// still `Running`.
    #[test]
    fn a_running_subagent_in_a_finished_entry_reads_last_seen_running() {
        let entry = assistant(
            "m1",
            MessageStatus::Complete,
            vec![subagent_part(
                "s1",
                "Explore",
                "Find the retry sites",
                SubagentStatus::Running,
                Some("Reading normalize.rs"),
                None,
                None,
                None,
                None,
            )],
        );
        let rows = rows_for_entry(&entry, false, &mut parse);
        let (caption, paint, activity) = subagent_row(&rows);
        assert_eq!(caption.as_ref(), "last seen running");
        assert_eq!(paint, SubagentPaint::LastSeenRunning);
        assert!(
            activity.is_none(),
            "a frozen card must not keep a live activity line"
        );
    }

    /// D53: the `SubagentUpdated` fold overwrites `activity` only when the new
    /// reading carries one, so a terminal part can still be holding the last
    /// live line. The fixture sets it DELIBERATELY — one that cleared it could
    /// not fail.
    #[test]
    fn a_completed_subagent_still_carrying_activity_draws_no_activity_line() {
        let entry = assistant(
            "m1",
            MessageStatus::Complete,
            vec![subagent_part(
                "s1",
                "Explore",
                "Find the retry sites",
                SubagentStatus::Completed,
                Some("Reading normalize.rs"),
                Some("Three call sites."),
                Some(20_115),
                Some(4_907),
                Some(4),
            )],
        );
        let rows = rows_for_entry(&entry, false, &mut parse);
        let (caption, _, activity) = subagent_row(&rows);
        assert_eq!(caption.as_ref(), "completed");
        assert!(
            activity.is_none(),
            "a finished card must never draw the stale live line the fold left behind"
        );
    }

    /// `None` is "not reported yet", never zero — so an unreported counter is
    /// dropped rather than printed as `0`.
    #[test]
    fn subagent_counters_omit_every_unreported_field() {
        assert_eq!(
            subagent_counters(Some(20_115), Some(4_907), Some(4))
                .as_ref()
                .map(|c| c.as_ref()),
            Some("20,115 tokens · 4.9s · 4 tools")
        );
        assert_eq!(
            subagent_counters(None, Some(4_907), None)
                .as_ref()
                .map(|c| c.as_ref()),
            Some("4.9s"),
            "an unreported counter must be absent, not zero"
        );
        assert_eq!(
            subagent_counters(Some(1), None, Some(1))
                .as_ref()
                .map(|c| c.as_ref()),
            Some("1 tokens · 1 tool")
        );
        assert!(
            subagent_counters(None, None, None).is_none(),
            "a card whose agent reported nothing carries no counters line at all"
        );
    }

    /// The delegation must not read twice. One card spends one chip.
    #[test]
    fn a_subagent_card_suppresses_one_agent_chip() {
        let entry = assistant(
            "m1",
            MessageStatus::Complete,
            vec![
                agent_chip("t1"),
                subagent_part(
                    "s1",
                    "Explore",
                    "Find the retry sites",
                    SubagentStatus::Completed,
                    None,
                    None,
                    None,
                    None,
                    None,
                ),
            ],
        );
        let rows = rows_for_entry(&entry, false, &mut parse);
        assert!(
            !rows
                .iter()
                .any(|r| matches!(r.kind, RowKind::ToolGroup { .. })),
            "the contentless Agent chip must not survive beside its card"
        );
        assert_eq!(
            rows.iter()
                .filter(|r| matches!(r.kind, RowKind::SubagentCard { .. }))
                .count(),
            1
        );
    }

    /// Suppression is budgeted by the card count, so it fails OPEN: a
    /// delegation that never produced a card keeps its chip rather than
    /// vanishing from the transcript entirely.
    #[test]
    fn an_agent_chip_with_no_card_survives() {
        let entry = assistant(
            "m1",
            MessageStatus::Complete,
            vec![
                agent_chip("t1"),
                agent_chip("t2"),
                subagent_part(
                    "s1",
                    "Explore",
                    "Find the retry sites",
                    SubagentStatus::Completed,
                    None,
                    None,
                    None,
                    None,
                    None,
                ),
            ],
        );
        let rows = rows_for_entry(&entry, false, &mut parse);
        let chips: usize = rows
            .iter()
            .filter_map(|r| match &r.kind {
                RowKind::ToolGroup { tools, .. } => Some(tools.len()),
                _ => None,
            })
            .sum();
        assert_eq!(
            chips, 1,
            "two chips and one card must leave exactly one chip standing"
        );
    }

    /// The match is on the `Agent` name specifically. Another unknown tool in
    /// the same entry is not collateral.
    #[test]
    fn a_non_agent_unknown_tool_is_never_suppressed() {
        let other = MessagePart::Tool {
            id: "t1".into(),
            call: ToolCall::Unknown {
                name: "SomeOtherTool".into(),
                input: None,
            },
            is_error: false,
            resolved: true,
            diff_ref: None,
            diff_stats: None,
        };
        let entry = assistant(
            "m1",
            MessageStatus::Complete,
            vec![
                other,
                subagent_part(
                    "s1",
                    "Explore",
                    "Find the retry sites",
                    SubagentStatus::Completed,
                    None,
                    None,
                    None,
                    None,
                    None,
                ),
            ],
        );
        let rows = rows_for_entry(&entry, false, &mut parse);
        let chips: usize = rows
            .iter()
            .filter_map(|r| match &r.kind {
                RowKind::ToolGroup { tools, .. } => Some(tools.len()),
                _ => None,
            })
            .sum();
        assert_eq!(chips, 1, "only the Agent chip may be suppressed");
    }

    // ---- slice 4.4: the plan card ----

    fn step(
        text: Option<&str>,
        active: Option<&str>,
        status: ChecklistStatus,
    ) -> comet_proto::ChecklistItem {
        comet_proto::ChecklistItem {
            id: "1".into(),
            text: text.map(str::to_owned),
            active_form: active.map(str::to_owned),
            status,
        }
    }

    fn checklist_part(
        explanation: Option<&str>,
        items: Vec<comet_proto::ChecklistItem>,
    ) -> MessagePart {
        MessagePart::Checklist {
            id: "checklist".into(),
            explanation: explanation.map(str::to_owned),
            items,
        }
    }

    fn checklist_row(rows: &[Row]) -> (Option<&SharedString>, usize, &Arc<Vec<ChecklistRow>>) {
        let row = rows
            .iter()
            .find(|r| matches!(r.kind, RowKind::ChecklistCard { .. }))
            .expect("expected a ChecklistCard row");
        let RowKind::ChecklistCard {
            explanation,
            done,
            steps,
        } = &row.kind
        else {
            unreachable!()
        };
        (explanation.as_ref(), *done, steps)
    }

    #[test]
    fn a_checklist_part_becomes_a_card_counting_only_completed_steps() {
        let entry = assistant(
            "m1",
            MessageStatus::Streaming,
            vec![checklist_part(
                Some("Narrowing to the fold first."),
                vec![
                    step(
                        Some("Read the failing test"),
                        None,
                        ChecklistStatus::Completed,
                    ),
                    step(
                        Some("Trace the assertion"),
                        None,
                        ChecklistStatus::Completed,
                    ),
                    step(
                        Some("Fix the fold"),
                        Some("Fixing the fold"),
                        ChecklistStatus::InProgress,
                    ),
                    step(Some("Re-run the suite"), None, ChecklistStatus::Pending),
                ],
            )],
        );
        let rows = rows_for_entry(&entry, false, &mut parse);
        let (explanation, done, steps) = checklist_row(&rows);
        assert_eq!(
            explanation.map(|e| e.as_ref()),
            Some("Narrowing to the fold first.")
        );
        assert_eq!(steps.len(), 4);
        assert_eq!(done, 2, "only Completed steps count toward the tally");
        assert_eq!(steps[2].label.as_ref(), "Fix the fold");
        assert!(steps.iter().all(|s| !s.unnamed));
    }

    /// Claude sends no explanation and none may be synthesized for it, so the
    /// row is simply absent rather than filled with Comet's own prose.
    #[test]
    fn a_claude_plan_carries_no_explanation_row() {
        let entry = assistant(
            "m1",
            MessageStatus::Streaming,
            vec![checklist_part(
                None,
                vec![step(Some("Read the test"), None, ChecklistStatus::Pending)],
            )],
        );
        let rows = rows_for_entry(&entry, false, &mut parse);
        let (explanation, _, _) = checklist_row(&rows);
        assert!(explanation.is_none());
    }

    /// A resumed run restates nothing, so a step can arrive with only its
    /// present-participle form — or with neither. Neither may render blank.
    #[test]
    fn a_step_label_falls_back_through_active_form_to_a_placeholder() {
        let entry = assistant(
            "m1",
            MessageStatus::Streaming,
            vec![checklist_part(
                None,
                vec![
                    step(
                        Some("Count the lines"),
                        Some("Counting the lines"),
                        ChecklistStatus::Pending,
                    ),
                    step(
                        None,
                        Some("Counting the lines"),
                        ChecklistStatus::InProgress,
                    ),
                    step(None, None, ChecklistStatus::Unknown),
                    // An empty string is not a subject either.
                    step(Some("   "), None, ChecklistStatus::Pending),
                ],
            )],
        );
        let rows = rows_for_entry(&entry, false, &mut parse);
        let (_, _, steps) = checklist_row(&rows);
        assert_eq!(steps[0].label.as_ref(), "Count the lines");
        assert!(!steps[0].unnamed);
        assert_eq!(
            steps[1].label.as_ref(),
            "Counting the lines",
            "a step this run never saw the subject of falls back to the active form"
        );
        assert!(!steps[1].unnamed);
        assert_eq!(steps[2].label.as_ref(), "Unnamed step");
        assert!(steps[2].unnamed, "the placeholder row is drawn quieter");
        assert_eq!(
            steps[3].label.as_ref(),
            "Unnamed step",
            "a blank subject is not a subject"
        );
    }

    /// An unrecognized status degrades to `Unknown` at the wire boundary and
    /// must still draw — it is a step like any other, not an error.
    #[test]
    fn an_unknown_step_status_still_draws_a_row() {
        let entry = assistant(
            "m1",
            MessageStatus::Streaming,
            vec![checklist_part(
                None,
                vec![step(Some("Something new"), None, ChecklistStatus::Unknown)],
            )],
        );
        let rows = rows_for_entry(&entry, false, &mut parse);
        let (_, done, steps) = checklist_row(&rows);
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].status, ChecklistStatus::Unknown);
        assert_eq!(done, 0, "an unknown status is not a completion");
    }

    /// The row cache keys on the entry fingerprint, which folds text LENGTHS.
    /// A step rewritten to another subject of the same length must still
    /// repaint, or the card keeps the old wording.
    #[test]
    fn an_equal_length_step_rewrite_still_changes_the_fingerprint() {
        let before = assistant(
            "m1",
            MessageStatus::Streaming,
            vec![checklist_part(
                None,
                vec![step(
                    Some("Read the aaa file"),
                    None,
                    ChecklistStatus::Pending,
                )],
            )],
        );
        let after = assistant(
            "m1",
            MessageStatus::Streaming,
            vec![checklist_part(
                None,
                vec![step(
                    Some("Read the bbb file"),
                    None,
                    ChecklistStatus::Pending,
                )],
            )],
        );
        assert_eq!("Read the aaa file".len(), "Read the bbb file".len());
        assert_ne!(
            entry_fingerprint(&before, false),
            entry_fingerprint(&after, false)
        );
    }

    fn approval_part(id: &str, decision: Option<comet_proto::ApprovalDecision>) -> MessagePart {
        MessagePart::Approval {
            id: id.into(),
            request_id: "r1".into(),
            approval: comet_proto::ApprovalRequest::Command {
                command: "cargo test --workspace".into(),
                cwd: None,
            },
            decision,
        }
    }

    #[test]
    fn an_open_approval_becomes_a_card_row() {
        let entry = assistant(
            "m1",
            MessageStatus::Streaming,
            vec![text_part("t0", "before"), approval_part("ap-r1", None)],
        );
        let rows = rows_for_entry(&entry, true, &mut parse);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].id.as_ref(), "m1#ap-r1");
        let RowKind::ApprovalCard {
            label,
            detail,
            state,
            paint,
        } = &rows[1].kind
        else {
            panic!("expected ApprovalCard, got another row kind");
        };
        assert_eq!(*label, "Run a command");
        assert_eq!(detail.as_ref(), "cargo test --workspace");
        assert_eq!(*state, None, "an open approval has no terminal caption");
        assert_eq!(*paint, ApprovalPaint::Open);
    }

    #[test]
    fn a_decided_approval_carries_its_caption() {
        let entry = assistant(
            "m1",
            MessageStatus::Complete,
            vec![approval_part(
                "ap-r1",
                Some(comet_proto::ApprovalDecision::AllowForSession),
            )],
        );
        let rows = rows_for_entry(&entry, false, &mut parse);
        let RowKind::ApprovalCard { state, paint, .. } = &rows[0].kind else {
            panic!("expected ApprovalCard");
        };
        assert_eq!(
            state.as_ref().map(|s| s.as_ref()),
            Some("Allowed for this session")
        );
        assert_eq!(*paint, ApprovalPaint::Allowed);
    }

    /// D14's residual: the note goes back to the model AND stays in the
    /// user's own transcript, as a sibling row right after the (still fixed
    /// 56px) card — never lost, and never a field growing the card itself.
    #[test]
    fn a_denied_approval_with_a_note_gets_its_own_transcript_row() {
        let entry = assistant(
            "m1",
            MessageStatus::Complete,
            vec![approval_part(
                "ap-r1",
                Some(comet_proto::ApprovalDecision::Deny {
                    message: "not that path, use src/other.rs instead".into(),
                }),
            )],
        );
        let rows = rows_for_entry(&entry, false, &mut parse);
        assert_eq!(rows.len(), 2, "the denied card plus its note row");
        assert!(
            matches!(rows[0].kind, RowKind::ApprovalCard { .. }),
            "the card comes first"
        );
        let RowKind::DenyNote { message } = &rows[1].kind else {
            panic!("expected a DenyNote row right after the denied card");
        };
        assert_eq!(message.as_ref(), "not that path, use src/other.rs instead");
    }

    /// The note row is specific to `Deny` — every other decision (including
    /// no decision at all) must not manufacture one.
    #[test]
    fn only_a_denial_gets_a_deny_note_row() {
        for decision in [
            None,
            Some(comet_proto::ApprovalDecision::Allow),
            Some(comet_proto::ApprovalDecision::AllowForSession),
            Some(comet_proto::ApprovalDecision::Expired),
        ] {
            let entry = assistant(
                "m1",
                MessageStatus::Complete,
                vec![approval_part("ap-r1", decision)],
            );
            let rows = rows_for_entry(&entry, false, &mut parse);
            assert_eq!(rows.len(), 1, "no deny-note row for a non-denial decision");
        }
    }

    /// `Expired` paints differently (amber, a state to resolve) but must NOT
    /// lay out differently — a decision arriving cannot be allowed to reflow
    /// the transcript under the user's scroll position.
    #[test]
    fn an_expired_approval_is_flagged_for_paint_only() {
        let entry = assistant(
            "m1",
            MessageStatus::Aborted,
            vec![approval_part(
                "ap-r1",
                Some(comet_proto::ApprovalDecision::Expired),
            )],
        );
        let rows = rows_for_entry(&entry, false, &mut parse);
        let RowKind::ApprovalCard {
            state,
            paint,
            label,
            detail,
        } = &rows[0].kind
        else {
            panic!("expected ApprovalCard");
        };
        assert_eq!(*paint, ApprovalPaint::Expired);
        assert!(state.as_ref().is_some_and(|s| s.contains("run ended")));
        // Identical to the open row's content fields: only paint may differ.
        assert_eq!(*label, "Run a command");
        assert_eq!(detail.as_ref(), "cargo test --workspace");
    }

    /// The whole point of splitting the paint discriminator out of `expired`:
    /// a decision must change how the card LOOKS (Allowed/Denied/Expired/Open
    /// all pairwise distinct) without changing anything layout-bearing — the
    /// 56px card must not reflow under the user's scroll position when a
    /// decision lands.
    #[test]
    fn the_paint_discriminator_differs_by_decision_while_layout_fields_do_not() {
        let row_for = |decision: Option<comet_proto::ApprovalDecision>| {
            let entry = assistant(
                "m1",
                MessageStatus::Complete,
                vec![approval_part("ap-r1", decision)],
            );
            let rows = rows_for_entry(&entry, false, &mut parse);
            let RowKind::ApprovalCard {
                label,
                detail,
                paint,
                ..
            } = rows[0].kind.clone()
            else {
                panic!("expected ApprovalCard");
            };
            (label, detail, paint)
        };

        let (open_label, open_detail, open_paint) = row_for(None);
        let (allow_label, allow_detail, allow_paint) =
            row_for(Some(comet_proto::ApprovalDecision::Allow));
        let (session_label, session_detail, session_paint) =
            row_for(Some(comet_proto::ApprovalDecision::AllowForSession));
        let (deny_label, deny_detail, deny_paint) =
            row_for(Some(comet_proto::ApprovalDecision::Deny {
                message: "no".into(),
            }));
        let (expired_label, expired_detail, expired_paint) =
            row_for(Some(comet_proto::ApprovalDecision::Expired));

        // Layout-bearing content (what `approval_card` sizes/lays out around)
        // never varies with the decision — only the request does.
        for (label, detail) in [
            (open_label, &open_detail),
            (allow_label, &allow_detail),
            (session_label, &session_detail),
            (deny_label, &deny_detail),
            (expired_label, &expired_detail),
        ] {
            assert_eq!(label, "Run a command");
            assert_eq!(detail.as_ref(), "cargo test --workspace");
        }

        // Allow and AllowForSession are the SAME paint (both "the user said
        // yes") — that is deliberate, not the bug. Everything else must be
        // pairwise distinct, or a refusal is indistinguishable from an
        // approval on a scrolled-back transcript (the defect this guards).
        assert_eq!(allow_paint, session_paint);
        let distinct = [open_paint, allow_paint, deny_paint, expired_paint];
        for i in 0..distinct.len() {
            for j in (i + 1)..distinct.len() {
                assert_ne!(
                    distinct[i], distinct[j],
                    "paint must differ across {:?} vs {:?}",
                    distinct[i], distinct[j]
                );
            }
        }
    }

    /// The decision changes the row's CONTENT, so it has to change the row's
    /// version — a diff key that ignored it would leave a decided approval
    /// painted as open until something else in the entry changed.
    #[test]
    fn the_row_version_changes_when_the_decision_lands() {
        let open = assistant(
            "m1",
            MessageStatus::Streaming,
            vec![approval_part("ap-r1", None)],
        );
        let decided = assistant(
            "m1",
            MessageStatus::Streaming,
            vec![approval_part(
                "ap-r1",
                Some(comet_proto::ApprovalDecision::Allow),
            )],
        );
        let a = rows_for_entry(&open, true, &mut parse);
        let b = rows_for_entry(&decided, true, &mut parse);
        assert_eq!(a[0].id, b[0].id);
        assert_ne!(a[0].version, b[0].version);
    }

    /// An approval between two tool calls breaks the group, like any non-tool
    /// part — the approval is ABOUT the call that follows it, so folding them
    /// into one group would put the card after the action it gates.
    #[test]
    fn an_approval_splits_tool_groups() {
        let entry = assistant(
            "m1",
            MessageStatus::Complete,
            vec![
                tool_part("a", "ls"),
                approval_part("ap-r1", None),
                tool_part("b", "pwd"),
            ],
        );
        let rows = rows_for_entry(&entry, false, &mut parse);
        assert_eq!(rows.len(), 3);
        assert!(matches!(rows[0].kind, RowKind::ToolGroup { .. }));
        assert!(matches!(rows[1].kind, RowKind::ApprovalCard { .. }));
        assert!(matches!(rows[2].kind, RowKind::ToolGroup { .. }));
    }

    /// A detail that restates the summary is suppressed before it reaches the
    /// chip (0.2a's duplicate-copy lesson).
    #[test]
    fn notice_detail_duplicating_summary_is_suppressed() {
        let entry = assistant(
            "m1",
            MessageStatus::Complete,
            vec![notice_part(
                "n0",
                "Context compacted",
                Some("Context compacted"),
                1,
            )],
        );
        let rows = rows_for_entry(&entry, false, &mut parse);
        let RowKind::NoticeChip { detail, .. } = &rows[0].kind else {
            panic!("expected NoticeChip");
        };
        assert_eq!(*detail, None);
    }

    /// A notice between two tool calls breaks the tool group, like any
    /// non-tool part.
    #[test]
    fn a_notice_splits_tool_groups() {
        let entry = assistant(
            "m1",
            MessageStatus::Complete,
            vec![
                tool_part("a", "ls"),
                notice_part("n0", "Context compacted", None, 1),
                tool_part("b", "pwd"),
            ],
        );
        let rows = rows_for_entry(&entry, false, &mut parse);
        assert_eq!(rows.len(), 3);
        assert!(matches!(rows[0].kind, RowKind::ToolGroup { .. }));
        assert!(matches!(rows[1].kind, RowKind::NoticeChip { .. }));
        assert!(matches!(rows[2].kind, RowKind::ToolGroup { .. }));
    }

    const MD: &str = "# Title\n\npara one\n\n```rust\nlet x = 1;\n```";

    #[test]
    fn live_entry_splits_per_block_with_id_continuity() {
        // Live rows split per block exactly like completed ones (the list
        // virtualizes them — the fading tail is the only per-frame work).
        let live = assistant("m1", MessageStatus::Streaming, vec![text_part("t0", MD)]);
        let live_rows = rows_for_entry(&live, false, &mut parse);
        assert_eq!(live_rows.len(), 3, "one live row per top-level block");
        assert!(
            live_rows
                .iter()
                .all(|r| matches!(r.kind, RowKind::LiveMarkdown { .. }))
        );
        assert_eq!(live_rows[0].id.as_ref(), "m1#t0.0");
        assert_eq!(live_rows[2].id.as_ref(), "m1#t0.2");

        let done = assistant("m1", MessageStatus::Complete, vec![text_part("t0", MD)]);
        let done_rows = rows_for_entry(&done, false, &mut parse);
        assert_eq!(done_rows.len(), 3, "three top-level blocks");
        // Every block row keeps its id across the flip — no flicker on handoff.
        for (live, done) in live_rows.iter().zip(&done_rows) {
            assert_eq!(live.id, done.id);
            // The flip changes the version even at identical text (the
            // streaming bit), forcing a splice.
            assert_ne!(live.version, done.version);
        }
        assert!(matches!(
            done_rows[0].kind,
            RowKind::Markdown { block_ix: 0, .. }
        ));
    }

    #[test]
    fn live_commit_changes_only_tail_row_versions() {
        // Streaming commit: appending to the last block leaves every settled
        // block row's (id, version) untouched — the diff splices only the tail.
        let t1 = "para one\n\npara two\n\npara three";
        let t2 = "para one\n\npara two\n\npara three grows here";
        let live1 = assistant("m1", MessageStatus::Streaming, vec![text_part("t0", t1)]);
        let live2 = assistant("m1", MessageStatus::Streaming, vec![text_part("t0", t2)]);
        let r1 = rows_for_entry(&live1, false, &mut parse);
        let r2 = rows_for_entry(&live2, false, &mut parse);
        assert_eq!(r1.len(), 3);
        assert_eq!(r2.len(), 3);
        assert_eq!(r1[0].version, r2[0].version, "settled block untouched");
        assert_eq!(r1[1].version, r2[1].version, "settled block untouched");
        assert_ne!(r1[2].version, r2[2].version, "tail block respliced");
        assert_eq!(diff_rows(&r1, &r2), Some((2..3, 1)));
    }

    #[test]
    fn split_sibling_gaps_match_live_internal_spacing() {
        // The live row spaces its internal blocks by MD_BLOCK_GAP; after the
        // live→split handoff the same boundaries are inter-row gaps. They must
        // be identical or the whole message jumps at completion.
        let done = assistant(
            "m1",
            MessageStatus::Complete,
            vec![
                text_part("t0", MD),
                tool_part("a", "ls"),
                text_part("t1", "tail para"),
            ],
        );
        let rows = rows_for_entry(&done, false, &mut parse);
        // Rows: t0.0, t0.1, t0.2 (three MD blocks), g0, t1.0.
        assert_eq!(rows.len(), 5);
        // Sibling markdown blocks from the same part: md block gap.
        assert_eq!(top_gap_for(Some(&rows[0]), &rows[1]), render::MD_BLOCK_GAP);
        assert_eq!(top_gap_for(Some(&rows[1]), &rows[2]), render::MD_BLOCK_GAP);
        // Markdown → tool group and tool group → next part: block gap.
        assert_eq!(top_gap_for(Some(&rows[2]), &rows[3]), GAP_BLOCK);
        assert_eq!(top_gap_for(Some(&rows[3]), &rows[4]), GAP_BLOCK);
        // Turn starts get the turn gap regardless.
        assert_eq!(top_gap_for(None, &rows[0]), GAP_TURN);
    }

    #[test]
    fn consecutive_tools_fold_into_groups_between_text() {
        let entry = assistant(
            "m2",
            MessageStatus::Complete,
            vec![
                text_part("t0", "before"),
                tool_part("a", "ls"),
                tool_part("b", "pwd"),
                text_part("t1", "after"),
                tool_part("c", "make"),
            ],
        );
        let rows = rows_for_entry(&entry, false, &mut parse);
        let ids: Vec<&str> = rows.iter().map(|r| r.id.as_ref()).collect();
        assert_eq!(ids, ["m2#t0.0", "m2#g0", "m2#t1.0", "m2#g1"]);
        let RowKind::ToolGroup { tools, .. } = &rows[1].kind else {
            panic!("group expected")
        };
        assert_eq!(tools.len(), 2);
        assert!(rows[0].turn_start && !rows[1].turn_start);
    }

    #[test]
    fn trailing_group_auto_opens_only_while_streaming() {
        let parts = vec![text_part("t0", "hi"), tool_part("a", "ls")];
        let streaming = assistant("m3", MessageStatus::Streaming, parts.clone());
        let rows = rows_for_entry(&streaming, false, &mut parse);
        let RowKind::ToolGroup { auto_open, .. } = rows[1].kind else {
            panic!()
        };
        assert!(auto_open, "trailing group opens while streaming");

        let complete = assistant("m3", MessageStatus::Complete, parts);
        let rows = rows_for_entry(&complete, false, &mut parse);
        let RowKind::ToolGroup { auto_open, .. } = rows[1].kind else {
            panic!()
        };
        assert!(!auto_open);

        // A non-trailing group never auto-opens.
        let mid = assistant(
            "m4",
            MessageStatus::Streaming,
            vec![tool_part("a", "ls"), text_part("t0", "hi")],
        );
        let rows = rows_for_entry(&mid, false, &mut parse);
        let RowKind::ToolGroup { auto_open, .. } = rows[0].kind else {
            panic!()
        };
        assert!(!auto_open);
    }

    #[test]
    fn user_rows_and_echo_versions() {
        let mut entry = assistant("u1", MessageStatus::Complete, vec![]);
        entry.role = MessageRole::User;
        entry.status = None;
        entry.parts = vec![text_part("t0", "hello")];
        let confirmed = rows_for_entry(&entry, false, &mut parse);
        let echoed = rows_for_entry(&entry, true, &mut parse);
        assert_eq!(confirmed.len(), 1);
        assert_eq!(confirmed[0].id, echoed[0].id);
        // Pending → confirmed changes the version so the row re-renders.
        assert_ne!(confirmed[0].version, echoed[0].version);
        assert!(matches!(
            &echoed[0].kind,
            RowKind::User { pending: true, .. }
        ));
    }

    #[test]
    fn user_rows_split_attachment_refs_from_text() {
        let content = crate::attachments::with_attachments(
            "what color is this?",
            &["/data/uploads/ab12-red.png".to_string()],
        );
        let mut entry = assistant("u2", MessageStatus::Complete, vec![]);
        entry.role = MessageRole::User;
        entry.status = None;
        entry.parts = vec![text_part("t0", &content)];
        let rows = rows_for_entry(&entry, false, &mut parse);
        assert_eq!(rows.len(), 1);
        let RowKind::User {
            text, attachments, ..
        } = &rows[0].kind
        else {
            panic!("expected a user row");
        };
        assert_eq!(text.as_ref(), "what color is this?");
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].path, "/data/uploads/ab12-red.png");
        assert_eq!(attachments[0].name, "ab12-red.png");

        // Image-only send: no bubble text, refs parsed.
        let only = crate::attachments::with_attachments("", &["/a/p.png".to_string()]);
        entry.parts = vec![text_part("t0", &only)];
        let rows = rows_for_entry(&entry, false, &mut parse);
        let RowKind::User {
            text, attachments, ..
        } = &rows[0].kind
        else {
            panic!("expected a user row");
        };
        assert_eq!(text.as_ref(), "");
        assert_eq!(attachments.len(), 1);
    }

    #[test]
    fn user_row_extracts_attachment_then_comment_badge_without_mutating_entry() {
        let comments = vec![crate::comments::DiffComment::new(
            "src/lib.rs",
            crate::comments::CommentSide::New,
            9,
            "tighten this",
        )];
        let with_comments = crate::comments::with_comments("review", &comments);
        let raw =
            crate::attachments::with_attachments(&with_comments, &[r"C:\tmp\shot.png".to_string()]);
        let mut entry = assistant("u-comments", MessageStatus::Complete, vec![]);
        entry.role = MessageRole::User;
        entry.status = None;
        entry.parts = vec![text_part("t0", &raw)];

        let rows = rows_for_entry(&entry, false, &mut parse);
        let RowKind::User {
            text,
            attachments,
            badges,
            ..
        } = &rows[0].kind
        else {
            panic!("expected a user row");
        };
        assert_eq!(text.as_ref(), "review");
        assert_eq!(attachments.len(), 1);
        assert_eq!(badges.len(), 1);
        assert_eq!(badges[0].label.as_ref(), "1 comment");
        assert_eq!(badges[0].details[0].location.as_ref(), "src/lib.rs:9");
        assert_eq!(badges[0].details[0].tag.as_deref(), Some("R"));
        assert_eq!(badges[0].details[0].body.as_ref(), "tighten this");
        let MessagePart::Text { text: stored, .. } = &entry.parts[0] else {
            panic!("expected stored text");
        };
        assert_eq!(stored, &raw);
        assert!(stored.contains(crate::comments::COMMENT_BLOCK_HEADER));
        assert!(stored.contains("Attached images"));
    }

    /// A sent prompt's file mentions render as chips in the transcript: the
    /// row carries the projected display text plus spans, while ordinary
    /// prompts keep the empty-spans fast path. The row version derives from
    /// the RAW text either way, so projection never perturbs the diff key.
    #[test]
    fn user_rows_project_file_mentions_into_chips() {
        let raw = "look at [composer.rs](comet-file:crates/ui/src/composer.rs) please";
        let mut entry = assistant("u3", MessageStatus::Complete, vec![]);
        entry.role = MessageRole::User;
        entry.status = None;
        entry.parts = vec![text_part("t0", raw)];
        let rows = rows_for_entry(&entry, false, &mut parse);
        let RowKind::User { text, mentions, .. } = &rows[0].kind else {
            panic!("expected a user row");
        };
        assert!(
            !text.contains("comet-file:"),
            "raw link left visible: {text}"
        );
        assert!(text.contains("composer.rs"));
        assert_eq!(mentions.len(), 1);
        assert!(!mentions[0].is_dir);
        assert_eq!(mentions[0].path.as_ref(), "crates/ui/src/composer.rs");
        assert_eq!(&text[mentions[0].range.clone()], {
            let projected: &str = "\u{00A0}@composer.rs\u{00A0}";
            projected
        });
        assert_eq!(rows[0].version, (raw.len() as u64) << 1);

        entry.parts = vec![text_part("t0", "no mentions here")];
        let rows = rows_for_entry(&entry, false, &mut parse);
        let RowKind::User { text, mentions, .. } = &rows[0].kind else {
            panic!("expected a user row");
        };
        assert_eq!(text.as_ref(), "no mentions here");
        assert!(mentions.is_empty());
    }

    #[test]
    fn diff_rows_appends_and_middle_edits() {
        let entry1 = assistant("m1", MessageStatus::Complete, vec![text_part("t0", "one")]);
        let entry2 = assistant("m2", MessageStatus::Complete, vec![text_part("t0", "two")]);
        let r1 = rows_for_entry(&entry1, false, &mut parse);
        let mut both = r1.clone();
        both.extend(rows_for_entry(&entry2, false, &mut parse));

        // Identical → None.
        assert!(diff_rows(&r1, &r1.clone()).is_none());
        // Append → splice at the tail.
        assert_eq!(diff_rows(&r1, &both), Some((1..1, 1)));
        // Removal from the end.
        assert_eq!(diff_rows(&both, &r1), Some((1..2, 0)));

        // Middle content change: only the changed row splices.
        let entry1b = assistant(
            "m1",
            MessageStatus::Complete,
            vec![text_part("t0", "one more")],
        );
        let mut both_b = rows_for_entry(&entry1b, false, &mut parse);
        both_b.extend(rows_for_entry(&entry2, false, &mut parse));
        assert_eq!(diff_rows(&both, &both_b), Some((0..1, 1)));

        // Full reset when everything shifts.
        let r2 = rows_for_entry(&entry2, false, &mut parse);
        assert_eq!(diff_rows(&r1, &r2), Some((0..1, 1)));
    }

    #[test]
    fn diff_handles_live_to_split_growth() {
        let live = assistant("m1", MessageStatus::Streaming, vec![text_part("t0", MD)]);
        let done = assistant("m1", MessageStatus::Complete, vec![text_part("t0", MD)]);
        let live_rows = rows_for_entry(&live, false, &mut parse);
        let done_rows = rows_for_entry(&done, false, &mut parse);
        // Same ids; every version flips its streaming bit → one 3-row splice.
        assert_eq!(diff_rows(&live_rows, &done_rows), Some((0..3, 3)));
    }

    #[test]
    fn tool_group_summaries() {
        let exec = |c: &str| ToolItem {
            id: c.into(),
            call: ToolCall::Exec { command: c.into() },
            is_error: false,
            resolved: true,
            diff_ref: None,
            diff_stats: None,
        };
        let edit = |p: &str| ToolItem {
            id: p.into(),
            call: ToolCall::EditFile {
                path: p.into(),
                old_string: None,
                new_string: None,
            },
            is_error: false,
            resolved: true,
            diff_ref: None,
            diff_stats: None,
        };
        let tools = vec![
            exec("ls"),
            exec("pwd"),
            exec("make"),
            edit("a.rs"),
            edit("b.rs"),
        ];
        assert_eq!(
            tool_group_summary(&tools),
            "Ran 3 commands · edited 2 files"
        );
        // Distinct-path dedupe: editing one file twice counts once.
        let tools = vec![edit("a.rs"), edit("a.rs")];
        assert_eq!(tool_group_summary(&tools), "Edited 1 file");
        // Failures append.
        let mut failing = exec("boom");
        failing.is_error = true;
        assert_eq!(tool_group_summary(&[failing]), "Ran 1 command · 1 failed");
        // Reads / searches / misc.
        let tools = vec![
            ToolItem {
                id: "read".into(),
                call: ToolCall::ReadFile { path: "x".into() },
                is_error: false,
                resolved: true,
                diff_ref: None,
                diff_stats: None,
            },
            ToolItem {
                id: "glob".into(),
                call: ToolCall::Glob {
                    pattern: "*.rs".into(),
                },
                is_error: false,
                resolved: true,
                diff_ref: None,
                diff_stats: None,
            },
            ToolItem {
                id: "web".into(),
                call: ToolCall::WebSearch { query: "q".into() },
                is_error: false,
                resolved: true,
                diff_ref: None,
                diff_stats: None,
            },
        ];
        assert_eq!(tool_group_summary(&tools), "Read 1 file · searched 2 times");
    }

    #[test]
    fn tool_chip_labels_per_kind() {
        assert_eq!(
            tool_chip_content(&ToolCall::Exec {
                command: "cargo test".into()
            }),
            ("Run", "cargo test".to_string())
        );
        assert_eq!(
            tool_chip_content(&ToolCall::Search {
                pattern: "foo".into(),
                path: Some("src".into())
            }),
            ("Search", "foo in src".to_string())
        );
        assert_eq!(
            tool_chip_content(&ToolCall::ApplyPatch { path: None }),
            ("Patch", "workspace".to_string())
        );
        assert_eq!(
            tool_chip_content(&ToolCall::Mcp {
                server: "gh".into(),
                tool: "issues".into(),
                input: None
            }),
            ("MCP", "gh · issues".to_string())
        );
        let todo = ToolCall::Todo {
            items: vec![
                comet_proto::TodoItem {
                    text: "a".into(),
                    done: true,
                },
                comet_proto::TodoItem {
                    text: "b".into(),
                    done: false,
                },
            ],
        };
        assert_eq!(tool_chip_content(&todo), ("Todo", "1/2 done".to_string()));
    }

    #[test]
    fn multiline_command_flattens_to_one_chip_line() {
        // The user's breaker: a multi-line script in a Run chip. The detail
        // must come out as ONE sanitized line — the chip's fixed 30px card
        // then truncates it with an ellipsis like the original's CSS.
        let (label, detail) = tool_chip_content(&ToolCall::Exec {
            command: "set -e\nfixture_in_original=0\n\tgrep -c  \"x\"".into(),
        });
        assert_eq!(label, "Run");
        assert_eq!(detail, "set -e fixture_in_original=0 grep -c \"x\"");
        assert!(!detail.contains('\n'));
        // The chip row height is a constant, independent of content shape.
        assert_eq!(chips_height(1), CHIPS_TOP_PAD + CHIP_HEIGHT);
        // Every detail kind is sanitized (MCP inputs / queries are model text).
        let (_, q) = tool_chip_content(&ToolCall::WebSearch {
            query: "line one\nline two".into(),
        });
        assert_eq!(q, "line one line two");
    }

    #[test]
    fn timestamp_strip_lands_on_the_last_settled_row() {
        use chrono::FixedOffset;
        // Fixed zone (UTC−4): "Jul 1, 3:45 PM" — the exact formatTimestamp
        // shape (short month, numeric day, no leading zero, 2-digit minutes).
        let tz = FixedOffset::west_opt(4 * 3600).unwrap();
        let ms = chrono::DateTime::parse_from_rfc3339("2026-07-01T19:45:00Z")
            .unwrap()
            .timestamp_millis();
        assert_eq!(format_timestamp(ms, &tz), "Jul 1, 3:45 PM");

        // User entries carry the strip on their single row (pending too).
        let user = SessionMessageEntry {
            id: "u1".into(),
            role: MessageRole::User,
            parts: vec![text_part("p1", "hi")],
            created_at: ms,
            device_id: "dev".into(),
            status: None,
            continuation_of: None,
        };
        let rows = rows_for_entry(&user, true, &mut parse);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].timestamp, Some(ms));

        // Assistant entries: strip on the LAST row once settled…
        let done = assistant(
            "a1",
            MessageStatus::Complete,
            vec![text_part("p1", "one\n\ntwo")],
        );
        let rows = rows_for_entry(&done, false, &mut parse);
        assert!(rows.len() >= 2);
        assert_eq!(rows.last().unwrap().timestamp, Some(done.created_at));
        assert!(rows[..rows.len() - 1].iter().all(|r| r.timestamp.is_none()));

        // …but never mid-stream (chat-view.tsx: no hover under a moving reply).
        let live = assistant(
            "a2",
            MessageStatus::Streaming,
            vec![text_part("p1", "streaming…")],
        );
        let rows = rows_for_entry(&live, false, &mut parse);
        assert!(rows.iter().all(|r| r.timestamp.is_none()));
        // Every row knows its entry (the hover group).
        assert!(rows.iter().all(|r| r.entry_id.as_ref() == live.id));
    }

    #[test]
    fn single_line_collapses_all_whitespace_runs() {
        assert_eq!(single_line("a\nb"), "a b");
        assert_eq!(single_line("  a\t\t b \r\n c  "), "a b c");
        assert_eq!(single_line("plain"), "plain");
        assert_eq!(single_line(""), "");
        assert_eq!(single_line("\n\n"), "");
    }

    #[test]
    fn chips_height_is_analytic() {
        assert_eq!(chips_height(0), 0.0);
        assert_eq!(chips_height(1), CHIPS_TOP_PAD + CHIP_HEIGHT);
        assert_eq!(
            chips_height(3),
            CHIPS_TOP_PAD + 3.0 * CHIP_HEIGHT + 2.0 * CHIP_GAP
        );
    }

    #[test]
    fn flavour_words_rotate_every_seven_seconds() {
        let seed = flavour_seed("chat-1");
        assert_eq!(flavour_word(seed, 0), flavour_word(seed, 6));
        assert_ne!(flavour_word(seed, 0), flavour_word(seed, 7));
        // Deterministic per chat; different chats usually differ in phase.
        assert_eq!(flavour_word(seed, 3), flavour_word(seed, 3));
        assert_eq!(format_elapsed(59), "59s");
        assert_eq!(format_elapsed(92), "1m 32s");
        assert_eq!(format_elapsed(-5), "0s");
    }

    #[test]
    fn empty_text_parts_produce_no_rows() {
        let entry = assistant(
            "m9",
            MessageStatus::Streaming,
            vec![text_part("t0", ""), text_part("t1", "   ")],
        );
        assert!(rows_for_entry(&entry, false, &mut parse).is_empty());
    }

    #[test]
    fn rows_keep_tool_part_identity_reference_and_stats() {
        let part = MessagePart::Tool {
            id: "tool-1".into(),
            call: ToolCall::EditFile {
                path: "src/lib.rs".into(),
                old_string: None,
                new_string: None,
            },
            is_error: false,
            resolved: true,
            diff_ref: Some("v1:abc".into()),
            diff_stats: Some(vec![comet_proto::ToolDiffStat {
                path: "src/lib.rs".into(),
                additions: 2,
                deletions: 1,
            }]),
        };
        let item = tool_item_from_part(&part).unwrap();
        assert_eq!(item.id.as_ref(), "tool-1");
        assert_eq!(item.diff_ref.as_deref(), Some("v1:abc"));
        assert_eq!(item.diff_stats.as_deref().unwrap()[0].additions, 2);
    }

    #[test]
    fn a_late_tool_diff_reply_cannot_replace_a_new_chat_or_reference() {
        let owner_a = ServerRef::new(ServerId::new("server-a"), "chat-a");
        let owner_b = ServerRef::new(ServerId::new("server-b"), "chat-b");
        let key = ToolDiffFetchKey {
            owner: owner_a.clone(),
            part_id: "tool-1".into(),
            diff_ref: "v1:old".into(),
        };
        let mut states = HashMap::new();
        states.insert(key.clone(), ToolDiffFetchState::Loading { generation: 7 });
        assert!(tool_diff_reply_is_current(Some(&owner_a), &states, &key, 7));
        assert!(!tool_diff_reply_is_current(
            Some(&owner_b),
            &states,
            &key,
            7
        ));
        assert!(!tool_diff_reply_is_current(
            Some(&owner_a),
            &states,
            &key,
            6
        ));
        let newer = ToolDiffFetchKey {
            diff_ref: "v1:new".into(),
            ..key.clone()
        };
        assert!(!tool_diff_reply_is_current(
            Some(&owner_a),
            &states,
            &newer,
            7
        ));
    }

    #[test]
    fn tool_diff_reference_mismatch_becomes_unavailable() {
        let expected = comet_proto::ToolDiff {
            path: "src/lib.rs".into(),
            old_text: Some("old\n".into()),
            new_text: "new\n".into(),
        };
        let wrong = comet_proto::ToolDiff {
            path: "src/lib.rs".into(),
            old_text: Some("old\n".into()),
            new_text: "different\n".into(),
        };
        assert!(
            validate_tool_diff_reply(
                &expected.diff_ref().unwrap(),
                comet_proto::ReadToolDiffReply::Available { diff: wrong },
            )
            .is_err_and(|failure| matches!(
                failure,
                ToolDiffValidationFailure::ChecksumMismatch { .. }
            ))
        );
    }

    #[test]
    fn tool_diff_validation_preserves_unavailable_checksum_and_source_categories() {
        let diff = comet_proto::ToolDiff {
            path: "src/lib.rs".into(),
            old_text: Some("old\n".into()),
            new_text: "new\n".into(),
        };
        let expected_ref = diff.diff_ref().unwrap();
        assert!(matches!(
            validate_tool_diff_reply(&expected_ref, comet_proto::ReadToolDiffReply::NotAvailable),
            Err(ToolDiffValidationFailure::NotAvailable)
        ));
        assert!(matches!(
            validate_tool_diff_reference(
                &expected_ref,
                Err(serde_json::Error::io(std::io::Error::other(
                    "checksum fixture"
                ))),
            ),
            Err(ToolDiffValidationFailure::ChecksumCalculation { .. })
        ));

        let mut file = diff_to_file(&diff);
        file.hunks[0].lines[0].text = "corrupted".into();
        assert!(matches!(
            validate_tool_diff_sources(&diff, &file),
            Err(ToolDiffValidationFailure::SourceMismatch)
        ));
    }

    #[test]
    fn tool_diff_plain_model_exists_before_highlights_are_ready() {
        let diff = comet_proto::ToolDiff {
            path: "src/lib.rs".into(),
            old_text: Some("fn old() {}\n".into()),
            new_text: "fn new() {}\n".into(),
        };
        let file = diff_to_file(&diff);
        let kinds: Vec<crate::changes::LineKind> =
            file.hunks[0].lines.iter().map(|line| line.kind).collect();
        assert!(kinds.contains(&crate::changes::LineKind::Del));
        assert!(kinds.contains(&crate::changes::LineKind::Add));
        assert_eq!(file.deletions, 1);
        assert_eq!(file.additions, 1);
    }

    #[test]
    fn new_file_stats_match_rendered_rows_for_all_line_endings() {
        for (case, source, additions) in [
            ("empty", "", 0),
            ("LF", "first\nsecond\n", 2),
            ("CRLF", "first\r\nsecond\r\n", 2),
            ("lone CR", "first\rsecond\r", 2),
            ("final newline", "last\n", 1),
        ] {
            let diff = comet_proto::ToolDiff {
                path: "src/new.rs".into(),
                old_text: None,
                new_text: source.into(),
            };
            let file = diff_to_file(&diff);
            let stat = diff.stat();
            assert_eq!(stat.additions, additions, "{case}");
            assert_eq!(stat.deletions, 0, "{case}");
            assert_eq!(stat.additions, u64::from(file.additions), "{case}");
            assert_eq!(stat.deletions, u64::from(file.deletions), "{case}");
        }
    }

    #[test]
    fn tool_diff_crlf_and_cr_sources_validate_without_visible_terminators() {
        for (old, new) in [
            ("old\r\nkeep\r\n", "new\r\nkeep\r\n"),
            ("old\rkeep\r", "new\rkeep\r"),
        ] {
            let diff = comet_proto::ToolDiff {
                path: "src/lib.rs".into(),
                old_text: Some(old.into()),
                new_text: new.into(),
            };
            let file = diff_to_file(&diff);
            let visible: Vec<_> = file
                .hunks
                .iter()
                .flat_map(|hunk| &hunk.lines)
                .filter(|line| line.kind != crate::changes::LineKind::Meta)
                .map(|line| (line.kind, line.text.as_str()))
                .collect();
            assert_eq!(
                visible,
                vec![
                    (crate::changes::LineKind::Del, "old"),
                    (crate::changes::LineKind::Add, "new"),
                    (crate::changes::LineKind::Context, "keep"),
                ]
            );
            assert!(crate::changes::sources_match_diff(
                &file,
                diff.old_text.as_deref(),
                Some(&diff.new_text),
            ));
        }
    }

    #[test]
    fn tool_diff_final_newline_change_has_a_visible_meta_row() {
        let diff = comet_proto::ToolDiff {
            path: "src/lib.rs".into(),
            old_text: Some("line".into()),
            new_text: "line\n".into(),
        };
        let file = diff_to_file(&diff);
        let visible: Vec<_> = file
            .hunks
            .iter()
            .flat_map(|hunk| &hunk.lines)
            .map(|line| (line.kind, line.text.as_str()))
            .collect();
        assert_eq!(
            visible,
            vec![
                (crate::changes::LineKind::Del, "line"),
                (
                    crate::changes::LineKind::Meta,
                    "\\ No newline at end of file"
                ),
                (crate::changes::LineKind::Add, "line"),
            ]
        );
        assert!(crate::changes::sources_match_diff(
            &file,
            diff.old_text.as_deref(),
            Some(&diff.new_text),
        ));
    }

    #[test]
    fn tool_diff_new_and_deleted_files_use_unified_zero_length_headers() {
        let added = comet_proto::ToolDiff {
            path: "src/new.rs".into(),
            old_text: None,
            new_text: "new\n".into(),
        };
        let added_file = diff_to_file(&added);
        assert_eq!(added_file.hunks[0].header, "@@ -0,0 +1,1 @@");
        assert_eq!(
            added_file.hunks[0].lines,
            vec![crate::changes::DiffLine {
                kind: crate::changes::LineKind::Add,
                old_no: None,
                new_no: Some(1),
                text: "new".into(),
            }]
        );
        assert!(crate::changes::sources_match_diff(
            &added_file,
            None,
            Some(&added.new_text),
        ));

        let deleted = comet_proto::ToolDiff {
            path: "src/old.rs".into(),
            old_text: Some("old\n".into()),
            new_text: String::new(),
        };
        let deleted_file = diff_to_file(&deleted);
        assert_eq!(deleted_file.hunks[0].header, "@@ -1,1 +0,0 @@");
        assert_eq!(
            deleted_file.hunks[0].lines,
            vec![crate::changes::DiffLine {
                kind: crate::changes::LineKind::Del,
                old_no: Some(1),
                new_no: None,
                text: "old".into(),
            }]
        );
        assert!(crate::changes::sources_match_diff(
            &deleted_file,
            deleted.old_text.as_deref(),
            Some(&deleted.new_text),
        ));
    }

    #[test]
    fn unicode_source_lines_validate_by_line_not_byte_offset() {
        let diff = comet_proto::ToolDiff {
            path: "src/lib.rs".into(),
            old_text: Some("let café = \"Paris\";\n".into()),
            new_text: "let café = \"東京\";\n".into(),
        };
        let file = diff_to_file(&diff);
        assert!(crate::changes::sources_match_diff(
            &file,
            diff.old_text.as_deref(),
            Some(&diff.new_text),
        ));
    }

    #[test]
    fn syntax_runs_do_not_change_diff_geometry_or_font_weight() {
        let theme = Theme::dark();
        let line = crate::changes::DiffLine {
            kind: crate::changes::LineKind::Add,
            old_no: None,
            new_no: Some(1),
            text: "fn café() {}".into(),
        };
        let plain = crate::changes::diff_text_runs(&line, &[], &theme);
        let highlighted = crate::changes::diff_text_runs(
            &line,
            &[comet_syntax::HighlightSpan {
                range: 0..2,
                kind: comet_syntax::HighlightKind::Keyword,
            }],
            &theme,
        );
        assert_eq!(
            plain.iter().map(|run| run.len).sum::<usize>(),
            line.text.len()
        );
        assert_eq!(
            highlighted.iter().map(|run| run.len).sum::<usize>(),
            line.text.len()
        );
        let mono = gpui::font(theme.font_mono.clone());
        assert!(plain.iter().all(|run| run.font == mono));
        assert!(highlighted.iter().all(|run| run.font == mono));
    }

    #[test]
    fn tool_diff_timeout_completion_removes_its_task_and_becomes_terminal_after_eight_seconds() {
        let key = ToolDiffFetchKey {
            owner: ServerRef::new(ServerId::new("server-a"), "chat-a"),
            part_id: "tool-1".into(),
            diff_ref: "v1:old".into(),
        };
        let mut fetches =
            HashMap::from([(key.clone(), ToolDiffFetchState::Loading { generation: 7 })]);
        let mut tasks = HashMap::from([(key.clone(), (7, ()))]);

        assert!(complete_tool_diff_fetch(
            Some(&key.owner),
            &mut fetches,
            &mut tasks,
            key.clone(),
            7,
            None,
        ));
        assert!(tasks.is_empty());
        assert!(matches!(
            fetches.get(&key),
            Some(ToolDiffFetchState::Unavailable)
        ));
        assert!(!tool_diff_fetch_needs_start(&fetches, &key));
        assert_eq!(TOOL_DIFF_FETCH_TIMEOUT, Duration::from_secs(8));
    }

    #[test]
    fn late_tool_diff_completion_keeps_owner_and_newer_generation_fetches_intact() {
        let key = ToolDiffFetchKey {
            owner: ServerRef::new(ServerId::new("server-a"), "chat-a"),
            part_id: "tool-1".into(),
            diff_ref: "v1:old".into(),
        };
        let mut fetches =
            HashMap::from([(key.clone(), ToolDiffFetchState::Loading { generation: 8 })]);
        let mut tasks = HashMap::from([(key.clone(), (8, ()))]);
        assert!(!complete_tool_diff_fetch(
            Some(&key.owner),
            &mut fetches,
            &mut tasks,
            key.clone(),
            7,
            None,
        ));
        assert!(matches!(
            fetches.get(&key),
            Some(ToolDiffFetchState::Loading { generation: 8 })
        ));
        assert_eq!(tasks.get(&key).map(|(generation, _)| *generation), Some(8));

        let other_owner = ServerRef::new(ServerId::new("server-b"), "chat-b");
        let mut owner_fetches =
            HashMap::from([(key.clone(), ToolDiffFetchState::Loading { generation: 7 })]);
        let mut owner_tasks = HashMap::from([(key.clone(), (7, ()))]);
        assert!(!complete_tool_diff_fetch(
            Some(&other_owner),
            &mut owner_fetches,
            &mut owner_tasks,
            key.clone(),
            7,
            None,
        ));
        assert!(matches!(
            owner_fetches.get(&key),
            Some(ToolDiffFetchState::Loading { generation: 7 })
        ));
        assert!(owner_tasks.is_empty());
    }

    #[test]
    fn ready_tool_diff_completion_is_reused_without_starting_a_second_fetch() {
        let key = ToolDiffFetchKey {
            owner: ServerRef::new(ServerId::new("server-a"), "chat-a"),
            part_id: "tool-1".into(),
            diff_ref: "v1:ready".into(),
        };
        let diff = Arc::new(comet_proto::ToolDiff {
            path: "src/lib.rs".into(),
            old_text: Some("old\n".into()),
            new_text: "new\n".into(),
        });
        let file = Arc::new(diff_to_file(&diff));
        let mut fetches =
            HashMap::from([(key.clone(), ToolDiffFetchState::Loading { generation: 7 })]);
        let mut tasks = HashMap::from([(key.clone(), (7, ()))]);

        assert!(complete_tool_diff_fetch(
            Some(&key.owner),
            &mut fetches,
            &mut tasks,
            key.clone(),
            7,
            Some((diff.clone(), file.clone())),
        ));
        assert!(tasks.is_empty());
        assert!(!tool_diff_fetch_needs_start(&fetches, &key));
        assert!(matches!(
            fetches.get(&key),
            Some(ToolDiffFetchState::Ready { diff: cached, file: cached_file })
                if Arc::ptr_eq(cached, &diff) && Arc::ptr_eq(cached_file, &file)
        ));
    }

    #[test]
    fn tool_diff_detail_height_includes_the_wrapper_padding_for_every_terminal_state() {
        let loading = ToolDiffFetchState::Loading { generation: 1 };
        let unavailable = ToolDiffFetchState::Unavailable;
        let file = Arc::new(crate::changes::FileDiff {
            path: "src/lib.rs".into(),
            old_path: None,
            status: crate::changes::FileStatus::Modified,
            binary: false,
            notices: Vec::new(),
            hunks: vec![crate::changes::Hunk {
                header: "@@ -1,1 +1,1 @@".into(),
                lines: vec![crate::changes::DiffLine {
                    kind: crate::changes::LineKind::Add,
                    old_no: None,
                    new_no: Some(1),
                    text: "new".into(),
                }],
            }],
            additions: 1,
            deletions: 0,
            max_line: 1,
        });
        let ready = ToolDiffFetchState::Ready {
            diff: Arc::new(comet_proto::ToolDiff {
                path: "src/lib.rs".into(),
                old_text: Some("old\n".into()),
                new_text: "new\n".into(),
            }),
            file: file.clone(),
        };

        assert_eq!(
            tool_diff_detail_height(Some(&loading)),
            TOOL_DIFF_STATUS_HEIGHT + 2.0
        );
        assert_eq!(
            tool_diff_detail_height(Some(&unavailable)),
            TOOL_DIFF_STATUS_HEIGHT + 2.0
        );
        assert_eq!(
            tool_diff_detail_height(Some(&ready)),
            crate::changes::body_height(&file) + 2.0
        );
    }
}
