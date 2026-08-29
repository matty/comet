//! What "the same action, again" means for an approval the user allowed for
//! the rest of the session.
//!
//! Comet owns this, not the provider: capture 2026-08-10 established that
//! Claude's `updatedPermissions` cannot express a per-tool session allow (the
//! rule it suggests persists to `.claude/settings.local.json` on disk AND the
//! next call prompts anyway). Codex's `acceptForSession` lands on this same
//! function in 1.7.

use comet_proto::{ApprovalRequest, FileOperation};
use serde_json::json;

/// A stable identity for "this action", or `None` when the action cannot be
/// identified well enough to allow it again unattended.
///
/// Volatile fields are excluded deliberately: a file change's line counts
/// differ on every edit, so including them would make a session-allow match
/// nothing.
///
/// The fields are encoded as a JSON array rather than joined with a separator.
/// A separator only works if it cannot occur in a field, and `command`, `cwd`,
/// `server` and `tool` are unrestricted strings — a command containing the
/// separator could be spelled to produce another action's signature, and this
/// is a permission boundary, so "unlikely input" is not the standard. JSON
/// escapes the field contents, which makes the encoding injective: distinct
/// field vectors cannot produce the same string.
pub(crate) fn approval_signature(request: &ApprovalRequest) -> Option<String> {
    let fields = match request {
        // `None` serializes as `null` and `Some("")` as `""` — different
        // answers, and they must not collide.
        ApprovalRequest::Command { command, cwd } => json!(["command", command, cwd]),
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
        } => json!(["fileChange", path, format!("{operation:?}")]),
        ApprovalRequest::FileRead { path } => json!(["fileRead", path]),
        // The only variant that names a CAPABILITY rather than an action.
        // `Mcp` carries no arguments — `create_issue` on one project and
        // `create_issue` on another are the same value — so a signature built
        // from it would grant every future call of that tool, whatever it was
        // asked to do. Every other kind's grant is narrow: the same command
        // text, the same path. This one would not be, and the user cannot see
        // the difference either, because the card renders `server · tool` and
        // no arguments.
        //
        // So: allow the call in front of the user, remember nothing. Same
        // treatment as `Unknown` below, for the same reason one field up.
        // Carrying a discriminating digest of the arguments would fix it
        // properly and needs a proto change (`docs/debt/README.md` D19).
        ApprovalRequest::Mcp { .. } => return None,
        ApprovalRequest::Unknown { .. } => return None,
    };
    // Infallible: every value here is a string, a null, or an array of them.
    Some(fields.to_string())
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
        let empty = ApprovalRequest::Command {
            command: "cargo test".into(),
            cwd: Some(String::new()),
        };
        assert_ne!(approval_signature(&here), approval_signature(&there));
        assert_ne!(approval_signature(&here), approval_signature(&nowhere));
        assert_ne!(approval_signature(&nowhere), approval_signature(&empty));
        assert_eq!(
            approval_signature(&here),
            approval_signature(&ApprovalRequest::Command {
                command: "cargo test".into(),
                cwd: Some("/repo".into()),
            })
        );
    }

    #[test]
    fn fields_that_contain_the_old_separator_do_not_collide() {
        // The signature used to join fields with `\u{1f}`, on the assumption
        // that the separator could not occur inside a field. `command` and
        // `cwd` are unrestricted strings, so it can: these two joined to the
        // identical `command\u{1f}a\u{1f}in\u{1f}b\u{1f}in\u{1f}c`. Allowing
        // the first for the session then allowed the second unattended, which
        // is a permission boundary crossed by an input nobody has to be lucky
        // to produce.
        let one = ApprovalRequest::Command {
            command: "a".into(),
            cwd: Some("b\u{1f}in\u{1f}c".into()),
        };
        let two = ApprovalRequest::Command {
            command: "a\u{1f}in\u{1f}b".into(),
            cwd: Some("c".into()),
        };
        assert!(approval_signature(&one).is_some());
        assert_ne!(approval_signature(&one), approval_signature(&two));
    }

    #[test]
    fn a_separator_in_a_path_or_a_server_name_does_not_collide_either() {
        // Same defect, the other allowlistable variants. A path and a file
        // change's path+operation pair are equally unrestricted.
        //
        // `Mcp` used to be the second half of this test and is no longer
        // allowlistable at all, so it cannot carry it: two `None`s are equal
        // and the assertion would pass for the wrong reason.
        let deep = ApprovalRequest::FileRead {
            path: "a\u{1f}b".into(),
        };
        let shallow = ApprovalRequest::FileRead { path: "a".into() };
        assert_ne!(approval_signature(&deep), approval_signature(&shallow));

        let split = ApprovalRequest::FileChange {
            path: "a\u{1f}Modify".into(),
            operation: FileOperation::Create,
            added_lines: 0,
            removed_lines: 0,
        };
        let whole = ApprovalRequest::FileChange {
            path: "a".into(),
            operation: FileOperation::Modify,
            added_lines: 0,
            removed_lines: 0,
        };
        assert!(approval_signature(&split).is_some());
        assert_ne!(approval_signature(&split), approval_signature(&whole));
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
        // Both sides must be allowlistable, or this passes on `Some != None`
        // and proves nothing about the encoding.
        let read = ApprovalRequest::FileRead { path: "x".into() };
        let command = ApprovalRequest::Command {
            command: "x".into(),
            cwd: None,
        };
        assert!(approval_signature(&read).is_some());
        assert!(approval_signature(&command).is_some());
        assert_ne!(approval_signature(&read), approval_signature(&command));
    }

    #[test]
    fn an_mcp_tool_is_never_allowlistable() {
        // `Mcp` names a capability, not an action: it carries no arguments, so
        // `create_issue` against one project and `create_issue` against another
        // are the same value. A signature built from it would turn "allow this
        // issue" into "allow every issue this tool ever creates", which is
        // broader than any other kind's grant and broader than what the card
        // showed the user (it renders `server · tool`, no arguments).
        let one = ApprovalRequest::Mcp {
            server: "linear".into(),
            tool: "create_issue".into(),
        };
        assert_eq!(approval_signature(&one), None);
    }
}
