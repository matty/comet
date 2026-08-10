//! What "the same action, again" means for an approval the user allowed for
//! the rest of the session.
//!
//! Comet owns this, not the provider: capture 2026-08-10 established that
//! Claude's `updatedPermissions` cannot express a per-tool session allow (the
//! rule it suggests persists to `.claude/settings.local.json` on disk AND the
//! next call prompts anyway). Codex's `acceptForSession` lands on this same
//! function in 1.7.

use comet_proto::{ApprovalRequest, FileOperation};

/// A stable identity for "this action", or `None` when the action cannot be
/// identified well enough to allow it again unattended.
///
/// Volatile fields are excluded deliberately: a file change's line counts
/// differ on every edit, so including them would make a session-allow match
/// nothing. Fields are joined with `\u{1f}` (unit separator), which cannot
/// occur in a path or a command, so two different actions cannot collide by
/// concatenation.
pub(crate) fn approval_signature(request: &ApprovalRequest) -> Option<String> {
    const SEP: char = '\u{1f}';
    Some(match request {
        ApprovalRequest::Command { command, cwd } => {
            // `None` and `Some("")` are different answers and must not collide.
            let cwd = match cwd {
                Some(dir) => format!("in{SEP}{dir}"),
                None => "nowhere".to_string(),
            };
            format!("command{SEP}{command}{SEP}{cwd}")
        }
        // An operation Comet could not determine is not an identity. Formatting
        // it would make "Comet does not know what this edit does" a stable
        // allowlist key, so a later edit to the same path that is equally
        // unreadable — a different tool, a shape a future provider introduces —
        // would be allowed unattended on the strength of the first one. Same
        // reasoning as `Unknown` below, one field down.
        ApprovalRequest::FileChange {
            operation: FileOperation::Unknown,
            ..
        } => return None,
        ApprovalRequest::FileChange {
            path, operation, ..
        } => format!("fileChange{SEP}{path}{SEP}{operation:?}"),
        ApprovalRequest::FileRead { path } => format!("fileRead{SEP}{path}"),
        ApprovalRequest::Mcp { server, tool } => format!("mcp{SEP}{server}{SEP}{tool}"),
        ApprovalRequest::Unknown { .. } => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use comet_proto::ApprovalRequest;

    #[test]
    fn the_same_edit_to_the_same_file_matches_across_different_line_counts() {
        // Line counts are volatile: the second edit to a file is never the
        // same size as the first. Including them would make a file-change
        // session-allow match nothing, which reads as the button doing nothing.
        let first = ApprovalRequest::FileChange {
            path: "src/main.rs".into(),
            operation: FileOperation::Modify,
            added_lines: 12,
            removed_lines: 3,
        };
        let second = ApprovalRequest::FileChange {
            path: "src/main.rs".into(),
            operation: FileOperation::Modify,
            added_lines: 1,
            removed_lines: 0,
        };
        assert_eq!(approval_signature(&first), approval_signature(&second));
        assert!(approval_signature(&first).is_some());
    }

    #[test]
    fn a_different_path_or_operation_is_a_different_action() {
        let modify = ApprovalRequest::FileChange {
            path: "src/main.rs".into(),
            operation: FileOperation::Modify,
            added_lines: 1,
            removed_lines: 0,
        };
        let other_path = ApprovalRequest::FileChange {
            path: "src/other.rs".into(),
            operation: FileOperation::Modify,
            added_lines: 1,
            removed_lines: 0,
        };
        let delete = ApprovalRequest::FileChange {
            path: "src/main.rs".into(),
            operation: FileOperation::Delete,
            added_lines: 0,
            removed_lines: 40,
        };
        assert_ne!(approval_signature(&modify), approval_signature(&other_path));
        assert_ne!(approval_signature(&modify), approval_signature(&delete));
    }

    #[test]
    fn a_command_is_identified_by_its_text_and_directory() {
        let here = ApprovalRequest::Command {
            command: "cargo test".into(),
            cwd: Some("/repo".into()),
        };
        let there = ApprovalRequest::Command {
            command: "cargo test".into(),
            cwd: Some("/elsewhere".into()),
        };
        // The absent case, written by hand: no reported directory must not
        // collide with a directory that happens to be empty or "/repo".
        let nowhere = ApprovalRequest::Command {
            command: "cargo test".into(),
            cwd: None,
        };
        assert_ne!(approval_signature(&here), approval_signature(&there));
        assert_ne!(approval_signature(&here), approval_signature(&nowhere));
        assert_eq!(
            approval_signature(&here),
            approval_signature(&ApprovalRequest::Command {
                command: "cargo test".into(),
                cwd: Some("/repo".into()),
            })
        );
    }

    #[test]
    fn an_unknown_action_is_never_allowlistable() {
        // Comet does not know what this is, so it cannot scope a rule to it.
        // AllowForSession on an Unknown degrades to allowing this one call.
        let unknown = ApprovalRequest::Unknown {
            summary: "an action Comet does not model".into(),
        };
        assert_eq!(approval_signature(&unknown), None);
    }

    #[test]
    fn an_edit_comet_could_not_read_is_never_allowlistable() {
        // `FileOperation::Unknown` means the adapter could not tell what the
        // change does. A rule keyed on it would allow the NEXT equally
        // unreadable change to the same path without asking.
        let unreadable = ApprovalRequest::FileChange {
            path: "src/main.rs".into(),
            operation: FileOperation::Unknown,
            added_lines: 0,
            removed_lines: 0,
        };
        assert_eq!(approval_signature(&unreadable), None);
    }

    #[test]
    fn kinds_never_collide_with_each_other() {
        let read = ApprovalRequest::FileRead { path: "x".into() };
        let mcp = ApprovalRequest::Mcp {
            server: "x".into(),
            tool: String::new(),
        };
        assert_ne!(approval_signature(&read), approval_signature(&mcp));
    }
}
