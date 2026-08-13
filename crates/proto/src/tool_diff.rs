use std::fmt::Write;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use similar::ChangeTag;

pub const TOOL_DIFF_PATH_MAX_BYTES: usize = 4_096;
pub const TOOL_DIFF_SOURCE_MAX_BYTES: usize = 1024 * 1024;
pub const TOOL_DIFF_PAYLOAD_MAX_BYTES: usize = 2 * 1024 * 1024 + 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDiff {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_text: Option<String>,
    pub new_text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDiffStat {
    pub path: String,
    pub additions: u64,
    pub deletions: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum ReadToolDiffReply {
    Available { diff: ToolDiff },
    NotAvailable,
}

impl ToolDiff {
    pub fn canonical_bytes(&self) -> serde_json::Result<Vec<u8>> {
        serde_json::to_vec(self)
    }

    pub fn diff_ref(&self) -> serde_json::Result<String> {
        let digest = Sha256::digest(self.canonical_bytes()?);
        let mut reference = String::from("v1:");
        for byte in digest {
            write!(&mut reference, "{byte:02x}").expect("writing to String cannot fail");
        }
        Ok(reference)
    }

    pub fn stat(&self) -> ToolDiffStat {
        let mut additions = 0;
        let mut deletions = 0;

        let old_text = self.old_text.as_deref().unwrap_or("");
        for change in similar::TextDiff::from_lines(old_text, &self.new_text).iter_all_changes() {
            match change.tag() {
                ChangeTag::Insert => additions += 1,
                ChangeTag::Delete => deletions += 1,
                ChangeTag::Equal => {}
            }
        }

        ToolDiffStat {
            path: self.path.clone(),
            additions,
            deletions,
        }
    }

    pub fn fits_inline_limits(&self) -> bool {
        if self.path.len() > TOOL_DIFF_PATH_MAX_BYTES
            || self.new_text.len() > TOOL_DIFF_SOURCE_MAX_BYTES
            || self
                .old_text
                .as_ref()
                .is_some_and(|old_text| old_text.len() > TOOL_DIFF_SOURCE_MAX_BYTES)
        {
            return false;
        }

        match self.canonical_bytes() {
            Ok(bytes) => bytes.len() <= TOOL_DIFF_PAYLOAD_MAX_BYTES,
            Err(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_diff_bytes_reference_and_stats_are_stable() {
        let diff = ToolDiff {
            path: "src/lib.rs".into(),
            old_text: Some("old\n".into()),
            new_text: "new\n".into(),
        };
        assert_eq!(
            diff.canonical_bytes().unwrap(),
            br#"{"path":"src/lib.rs","oldText":"old\n","newText":"new\n"}"#
        );
        assert_eq!(
            diff.diff_ref().unwrap(),
            "v1:6837019f1513cd673d215857abd61dbf00ed9e8eaa9cdbbdca37907482872ad0"
        );
        assert_eq!(
            diff.stat(),
            ToolDiffStat {
                path: "src/lib.rs".into(),
                additions: 1,
                deletions: 1
            }
        );
    }

    #[test]
    fn inline_limits_reject_oversized_values() {
        let mut diff = ToolDiff {
            path: "p".repeat(TOOL_DIFF_PATH_MAX_BYTES),
            old_text: Some("o".repeat(TOOL_DIFF_SOURCE_MAX_BYTES)),
            new_text: "n".repeat(TOOL_DIFF_SOURCE_MAX_BYTES),
        };
        assert!(diff.fits_inline_limits());

        diff.path.push('x');
        assert!(!diff.fits_inline_limits());
    }

    #[test]
    fn source_limits_are_inclusive_for_old_and_new_text() {
        let cases = [
            (
                "old source at limit",
                Some("o".repeat(TOOL_DIFF_SOURCE_MAX_BYTES)),
                String::new(),
                true,
            ),
            (
                "old source one byte over",
                Some("o".repeat(TOOL_DIFF_SOURCE_MAX_BYTES + 1)),
                String::new(),
                false,
            ),
            (
                "new source at limit",
                None,
                "n".repeat(TOOL_DIFF_SOURCE_MAX_BYTES),
                true,
            ),
            (
                "new source one byte over",
                None,
                "n".repeat(TOOL_DIFF_SOURCE_MAX_BYTES + 1),
                false,
            ),
        ];

        for (case, old_text, new_text, expected) in cases {
            assert_eq!(
                ToolDiff {
                    path: "src/lib.rs".into(),
                    old_text,
                    new_text,
                }
                .fits_inline_limits(),
                expected,
                "{case}"
            );
        }
    }

    #[test]
    fn payload_limit_rejects_escape_heavy_sources_within_source_limits() {
        let escape_heavy = "\"".repeat(TOOL_DIFF_SOURCE_MAX_BYTES);
        let diff = ToolDiff {
            path: "src/lib.rs".into(),
            old_text: Some(escape_heavy.clone()),
            new_text: escape_heavy,
        };

        assert!(diff.old_text.as_ref().unwrap().len() <= TOOL_DIFF_SOURCE_MAX_BYTES);
        assert!(diff.new_text.len() <= TOOL_DIFF_SOURCE_MAX_BYTES);
        assert!(diff.canonical_bytes().unwrap().len() > TOOL_DIFF_PAYLOAD_MAX_BYTES);
        assert!(!diff.fits_inline_limits());
    }

    #[test]
    fn path_limit_uses_utf8_bytes_not_character_count() {
        let cases = [
            ("multibyte path at byte limit", "é".repeat(2_048), true),
            (
                "multibyte path one character over byte limit",
                "é".repeat(2_049),
                false,
            ),
        ];

        for (case, path, expected) in cases {
            assert_eq!(
                ToolDiff {
                    path,
                    old_text: None,
                    new_text: String::new(),
                }
                .fits_inline_limits(),
                expected,
                "{case}"
            );
        }
    }

    #[test]
    fn line_stats_preserve_newline_semantics() {
        let cases = [
            ("empty new file", None, "", 0, 0),
            ("blank lines in new file", None, "\n\n", 2, 0),
            ("lone CR lines in new file", None, "first\rsecond\r", 2, 0),
            ("CRLF replacement", Some("before\r\n"), "after\r\n", 1, 1),
            ("trailing final newline only", Some("line"), "line\n", 1, 1),
        ];

        for (case, old_text, new_text, additions, deletions) in cases {
            assert_eq!(
                ToolDiff {
                    path: "src/lib.rs".into(),
                    old_text: old_text.map(str::to_owned),
                    new_text: new_text.into(),
                }
                .stat(),
                ToolDiffStat {
                    path: "src/lib.rs".into(),
                    additions,
                    deletions,
                },
                "{case}"
            );
        }
    }

    #[test]
    fn read_tool_diff_not_available_uses_a_stable_literal() {
        assert_eq!(
            serde_json::to_string(&ReadToolDiffReply::NotAvailable).unwrap(),
            r#"{"status":"notAvailable"}"#
        );
    }
}
