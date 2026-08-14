//! Diff comments: notes pinned to a line of the changes pane, staged on the
//! composer and folded into the next prompt as plain text.
//!
//! [`with_comments`] appends them; [`extract_badge`] reads the same block back
//! out for the transcript. There is no second data model.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CommentSide {
    Old,
    New,
}

impl CommentSide {
    pub fn tag(self) -> &'static str {
        match self {
            Self::Old => "L",
            Self::New => "R",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffComment {
    pub id: String,
    pub path: String,
    /// Pre-change path for a rename. Only old-side anchors cite it; grouping
    /// and new-side anchors continue to use [`Self::path`].
    pub old_path: Option<String>,
    pub side: CommentSide,
    pub line: u32,
    pub body: String,
}

impl DiffComment {
    pub fn new(
        path: impl Into<String>,
        side: CommentSide,
        line: u32,
        body: impl Into<String>,
    ) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            path: path.into(),
            old_path: None,
            side,
            line,
            body: body.into(),
        }
    }

    pub fn renamed_from<T: Into<String>>(mut self, old_path: Option<T>) -> Self {
        self.old_path = old_path.map(Into::into);
        self
    }

    pub fn anchor(&self) -> (CommentSide, u32) {
        (self.side, self.line)
    }

    pub fn cite_path(&self) -> &str {
        match self.side {
            CommentSide::Old => self.old_path.as_deref().unwrap_or(&self.path),
            CommentSide::New => &self.path,
        }
    }

    pub fn location(&self) -> String {
        format!("{}:{}", self.cite_path(), self.line)
    }
}

pub const COMMENT_ONLY_TEXT: &str = "Address the review comments below.";

pub const COMMENT_BLOCK_HEADER: &str = "Comments on the diff (each cites the file and line it belongs to; L = line number in the original file, R = in the changed file):";

fn side_marker(side: CommentSide) -> String {
    format!(" ({}): ", side.tag())
}

pub fn with_comments(text: &str, comments: &[DiffComment]) -> String {
    if comments.is_empty() {
        return text.to_string();
    }
    let bullets: Vec<String> = comments
        .iter()
        .map(|comment| {
            let body = comment.body.trim().replace('\n', "\n  ");
            format!(
                "- {}{}{body}",
                comment.location(),
                side_marker(comment.side)
            )
        })
        .collect();
    let body = if text.is_empty() {
        COMMENT_ONLY_TEXT
    } else {
        text
    };
    format!("{body}\n\n{COMMENT_BLOCK_HEADER}\n{}", bullets.join("\n"))
}

/// [`crate::badges::Extractor`] for the comment block. Matched only as a whole
/// trailing block, so a prompt quoting the header mid-body is left alone.
pub fn extract_badge(text: &str) -> Option<(String, crate::badges::MessageBadge)> {
    let marker = format!("\n\n{COMMENT_BLOCK_HEADER}\n");
    let at = text.rfind(&marker)?;
    let block = &text[at + marker.len()..];
    if block.is_empty()
        || !block
            .lines()
            .all(|line| line.starts_with("- ") || line.starts_with("  "))
    {
        return None;
    }
    let details = parse_bullets(block)?;
    Some((
        text[..at].to_string(),
        crate::badges::MessageBadge {
            icon: crate::icons::CHAT_ROUND_LINE,
            label: chip_label(details.len()).into(),
            details,
        },
    ))
}

fn parse_bullets(block: &str) -> Option<Vec<crate::badges::BadgeDetail>> {
    let mut details: Vec<crate::badges::BadgeDetail> = Vec::new();
    for line in block.lines() {
        let Some(bullet) = line.strip_prefix("- ") else {
            let indented = line.strip_prefix("  ")?;
            let last = details.last_mut()?;
            last.body = format!("{}\n{indented}", last.body).into();
            continue;
        };
        // Earliest marker wins: a body may contain "(L): " itself, and matching
        // that would swallow the body into the location.
        let split = [CommentSide::Old, CommentSide::New]
            .into_iter()
            .filter_map(|side| {
                let marker = side_marker(side);
                bullet.find(&marker).map(|at| (at, marker.len(), side))
            })
            .min_by_key(|(at, _, _)| *at);
        let (at, marker_len, side) = split?;
        details.push(crate::badges::BadgeDetail {
            location: bullet[..at].into(),
            tag: Some(side.tag().into()),
            body: bullet[at + marker_len..].into(),
        });
    }
    (!details.is_empty()).then_some(details)
}

pub fn chip_label(count: usize) -> String {
    if count == 1 {
        "1 comment".to_string()
    } else {
        format!("{count} comments")
    }
}

pub const CARD_PAD_V: f32 = 20.0;
pub const CARD_HEADER_HEIGHT: f32 = 22.0;
pub const CARD_LINE_HEIGHT: f32 = 18.0;
pub const DRAFT_CARD_HEIGHT: f32 = 116.0;
const CARD_GAP: f32 = 6.0;
const CARD_WRAP_COLUMNS: usize = 64;
const CARD_MAX_LINES: usize = 8;

/// Wraps are guessed, not measured: the changes pane sizes bodies by arithmetic
/// to drive the fold tween, and a measured card would desync it.
pub fn card_body_lines(body: &str) -> usize {
    body.lines()
        .map(|line| line.chars().count().div_ceil(CARD_WRAP_COLUMNS).max(1))
        .sum::<usize>()
        .clamp(1, CARD_MAX_LINES)
}

pub fn card_height(body: &str) -> f32 {
    CARD_PAD_V + CARD_HEADER_HEIGHT + card_body_lines(body) as f32 * CARD_LINE_HEIGHT + CARD_GAP
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comment(path: &str, side: CommentSide, line: u32, body: &str) -> DiffComment {
        DiffComment::new(path, side, line, body)
    }

    #[test]
    fn empty_set_leaves_the_prompt_untouched() {
        assert_eq!(with_comments("ship it", &[]), "ship it");
    }

    #[test]
    fn comments_append_as_located_bullets() {
        let staged = vec![
            comment("src/main.rs", CommentSide::New, 42, "early-return here"),
            comment("src/lib.rs", CommentSide::Old, 7, "why was this dropped?"),
        ];
        assert_eq!(
            with_comments("look at these", &staged),
            "look at these\n\nComments on the diff (each cites the file and line it belongs to; L = line number in the original file, R = in the changed file):\n- src/main.rs:42 (R): early-return here\n- src/lib.rs:7 (L): why was this dropped?"
        );
    }

    #[test]
    fn comment_only_send_gets_a_body() {
        let staged = vec![comment("a.rs", CommentSide::New, 1, "fix")];
        assert!(with_comments("", &staged).starts_with(COMMENT_ONLY_TEXT));
    }

    #[test]
    fn multiline_bodies_indent_under_their_bullet() {
        let staged = vec![comment("a.rs", CommentSide::New, 3, "first\nsecond")];
        assert!(with_comments("x", &staged).contains("- a.rs:3 (R): first\n  second"));
    }

    #[test]
    fn a_complete_trailing_block_extracts_one_badge() {
        let staged = vec![comment("a.rs", CommentSide::New, 3, "first\nsecond")];
        let serialized = with_comments("typed", &staged);
        let (text, badge) = extract_badge(&serialized).expect("valid trailing block");

        assert_eq!(text, "typed");
        assert_eq!(badge.label.as_ref(), "1 comment");
        assert_eq!(badge.details[0].location.as_ref(), "a.rs:3");
        assert_eq!(badge.details[0].tag.as_deref(), Some("R"));
        assert_eq!(badge.details[0].body.as_ref(), "first\nsecond");
    }

    #[test]
    fn malformed_or_non_trailing_blocks_stay_visible() {
        let quoted = format!("before\n\n{COMMENT_BLOCK_HEADER}\nnot a block\nafter");
        assert!(extract_badge(&quoted).is_none());

        let missing_marker =
            format!("typed\n\n{COMMENT_BLOCK_HEADER}\n- a.rs:1 (R): valid\n- malformed");
        assert!(extract_badge(&missing_marker).is_none());

        let leading_continuation =
            format!("typed\n\n{COMMENT_BLOCK_HEADER}\n  orphaned\n- a.rs:1 (R): valid");
        assert!(extract_badge(&leading_continuation).is_none());

        let trailing_prose =
            format!("typed\n\n{COMMENT_BLOCK_HEADER}\n- a.rs:1 (R): valid\nordinary prose");
        assert!(extract_badge(&trailing_prose).is_none());
    }

    #[test]
    fn the_earliest_side_marker_preserves_marker_text_in_the_body() {
        let staged = vec![comment(
            "a.rs",
            CommentSide::New,
            5,
            "see (L): the other one",
        )];
        let (_, badge) = extract_badge(&with_comments("x", &staged)).unwrap();

        assert_eq!(badge.details[0].location.as_ref(), "a.rs:5");
        assert_eq!(badge.details[0].tag.as_deref(), Some("R"));
        assert_eq!(badge.details[0].body.as_ref(), "see (L): the other one");
    }

    #[test]
    fn renamed_locations_round_trip_on_their_source_side() {
        let old =
            comment("src/new.rs", CommentSide::Old, 7, "old").renamed_from(Some("src/old.rs"));
        let new =
            comment("src/new.rs", CommentSide::New, 8, "new").renamed_from(Some("src/old.rs"));
        let (_, badge) = extract_badge(&with_comments("review", &[old, new])).unwrap();

        assert_eq!(badge.details[0].location.as_ref(), "src/old.rs:7");
        assert_eq!(badge.details[1].location.as_ref(), "src/new.rs:8");
    }

    #[test]
    fn renamed_comments_cite_the_path_for_their_side() {
        let old =
            comment("src/new.rs", CommentSide::Old, 7, "old").renamed_from(Some("src/old.rs"));
        let new =
            comment("src/new.rs", CommentSide::New, 8, "new").renamed_from(Some("src/old.rs"));

        assert_eq!(old.cite_path(), "src/old.rs");
        assert_eq!(new.cite_path(), "src/new.rs");
        assert_eq!(
            with_comments("review", &[old, new]),
            "review\n\nComments on the diff (each cites the file and line it belongs to; L = line number in the original file, R = in the changed file):\n- src/old.rs:7 (L): old\n- src/new.rs:8 (R): new"
        );
    }

    #[test]
    fn chip_label_pluralizes() {
        assert_eq!(chip_label(1), "1 comment");
        assert_eq!(chip_label(2), "2 comments");
        assert_eq!(chip_label(0), "0 comments");
    }

    #[test]
    fn a_body_quoting_a_side_marker_survives_the_round_trip() {
        let staged = vec![comment(
            "a.rs",
            CommentSide::New,
            5,
            "see (L): the other one",
        )];
        let (text, badge) = extract_badge(&with_comments("x", &staged)).unwrap();
        assert_eq!(text, "x");
        assert_eq!(badge.details.len(), 1);
        assert_eq!(badge.details[0].location.as_ref(), "a.rs:5");
        assert_eq!(badge.details[0].tag.as_deref(), Some("R"));
        assert_eq!(badge.details[0].body.as_ref(), "see (L): the other one");
    }

    #[test]
    fn card_height_grows_with_body_lines() {
        assert!(card_height("two\nlines") > card_height("one"));
        assert_eq!(card_height(""), card_height("one"));
    }

    #[test]
    fn long_lines_are_charged_for_their_soft_wraps() {
        assert_eq!(card_body_lines("short"), 1);
        assert_eq!(card_body_lines(&"x".repeat(CARD_WRAP_COLUMNS)), 1);
        assert_eq!(card_body_lines(&"x".repeat(CARD_WRAP_COLUMNS + 1)), 2);
        assert_eq!(card_body_lines(&"line\n".repeat(200)), CARD_MAX_LINES);
    }
}
