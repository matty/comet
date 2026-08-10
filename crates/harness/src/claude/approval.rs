//! Claude's `can_use_tool` request → Comet's `ApprovalRequest`.
//!
//! Tool names and input shapes are from a live capture (2026-08-10, CLI
//! 2.1.226) plus the tool set in sdk.d.ts. An unrecognized tool is NOT an
//! error — it becomes `Unknown` with Comet copy, because the alternative is
//! either dropping the request (the agent hangs) or auto-allowing it.

use comet_proto::{ApprovalRequest, FileOperation};

use super::wire::ControlRequestBody;

// Not yet called from `handle_control_request`: this task is the pure
// decision table only. Wiring it into the control-request path (replacing
// the current auto-allow) is a later task in this slice.
#[allow(dead_code)]
pub(crate) fn approval_request(body: &ControlRequestBody) -> ApprovalRequest {
    let str_field = |key: &str| {
        body.input
            .get(key)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };

    match body.tool_name.as_str() {
        "Bash" => match str_field("command") {
            // The request carries no working directory (captured); reporting
            // one Comet inferred would be an assertion the provider never made.
            Some(command) => ApprovalRequest::Command { command, cwd: None },
            None => unknown(&body.tool_name),
        },
        // No Notebook* arms: the installed sdk.d.ts (0.3.195) does not mention
        // those tools at all, and no capture run produced one. They would read
        // `notebook_path` rather than `file_path` if they exist, so listing
        // them here would be an arm that silently never matches.
        "Read" => match str_field("file_path") {
            Some(path) => ApprovalRequest::FileRead { path },
            None => unknown(&body.tool_name),
        },
        "Write" | "Edit" => {
            let Some(path) = str_field("file_path") else {
                return unknown(&body.tool_name);
            };
            let old = str_field("old_string").unwrap_or_default();
            let new = str_field("new_string").or_else(|| str_field("content"));
            let (operation, added, removed) = match (&new, old.is_empty()) {
                // Write with content and no prior text: a create.
                (Some(text), true) if body.tool_name == "Write" => {
                    (FileOperation::Create, line_count(text), 0)
                }
                (Some(text), _) => (FileOperation::Modify, line_count(text), line_count(&old)),
                // An empty replacement removes text.
                (None, false) => (FileOperation::Modify, 0, line_count(&old)),
                (None, true) => (FileOperation::Unknown, 0, 0),
            };
            ApprovalRequest::FileChange {
                path,
                operation,
                added_lines: added,
                removed_lines: removed,
            }
        }
        name if name.starts_with("mcp__") => {
            // `mcp__<server>__<tool>`; a name missing the tool half still
            // names its server rather than degrading to Unknown. `strip_prefix`
            // removes the guard-matched prefix exactly once — `trim_start_matches`
            // would also eat a literal leading "mcp__" off a server named `mcp`.
            let rest = name.strip_prefix("mcp__").unwrap_or(name);
            if rest.is_empty() {
                // A bare "mcp__" names no server; nothing to put on the card.
                return unknown(name);
            }
            let (server, tool) = rest.split_once("__").unwrap_or((rest, ""));
            ApprovalRequest::Mcp {
                server: server.to_owned(),
                tool: tool.to_owned(),
            }
        }
        name => unknown(name),
    }
}

/// Comet copy naming what was asked. Never the CLI's `description` — see
/// `ApprovalRequest::Unknown`'s contract in comet-proto.
fn unknown(tool_name: &str) -> ApprovalRequest {
    let summary = if tool_name.is_empty() {
        "Run a tool Comet could not identify".to_string()
    } else {
        format!("Run the {tool_name} tool")
    };
    ApprovalRequest::Unknown { summary }
}

/// Lines a block of text spans. A trailing newline does not begin a line.
fn line_count(text: &str) -> u32 {
    if text.is_empty() {
        return 0;
    }
    text.strip_suffix('\n').unwrap_or(text).split('\n').count() as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use comet_proto::{ApprovalRequest, FileOperation};
    use serde_json::json;

    fn body(tool: &str, input: serde_json::Value) -> ControlRequestBody {
        ControlRequestBody {
            subtype: "can_use_tool".into(),
            tool_name: tool.into(),
            input,
            description: Some("provider prose that must not reach the card".into()),
        }
    }

    #[test]
    fn write_to_a_new_file_is_a_create() {
        let got = approval_request(&body(
            "Write",
            json!({"file_path": "a.txt", "content": "hi\n"}),
        ));
        assert_eq!(
            got,
            ApprovalRequest::FileChange {
                path: "a.txt".into(),
                operation: FileOperation::Create,
                added_lines: 1,
                removed_lines: 0,
            }
        );
    }

    #[test]
    fn edit_is_a_modify_and_counts_the_replacement() {
        let got = approval_request(&body(
            "Edit",
            json!({"file_path": "a.txt", "old_string": "one\ntwo\n", "new_string": "one\n"}),
        ));
        assert_eq!(
            got,
            ApprovalRequest::FileChange {
                path: "a.txt".into(),
                operation: FileOperation::Modify,
                added_lines: 1,
                removed_lines: 2,
            }
        );
    }

    #[test]
    fn bash_is_a_command_and_carries_no_directory_it_was_not_given() {
        // The CLI does not report a cwd on the request (captured). Inventing
        // one — the run's cwd, say — would put a directory on the card that
        // the provider never asserted.
        let got = approval_request(&body("Bash", json!({"command": "cargo test"})));
        assert_eq!(
            got,
            ApprovalRequest::Command {
                command: "cargo test".into(),
                cwd: None,
            }
        );
    }

    #[test]
    fn read_is_a_file_read() {
        let got = approval_request(&body("Read", json!({"file_path": "/etc/hosts"})));
        assert_eq!(
            got,
            ApprovalRequest::FileRead {
                path: "/etc/hosts".into()
            }
        );
    }

    #[test]
    fn an_mcp_tool_splits_its_server_from_its_tool() {
        // sdk.d.ts names MCP tools `mcp__<server>__<tool>`.
        let got = approval_request(&body("mcp__linear__create_issue", json!({})));
        assert_eq!(
            got,
            ApprovalRequest::Mcp {
                server: "linear".into(),
                tool: "create_issue".into(),
            }
        );
    }

    #[test]
    fn an_unrecognized_tool_gets_comet_copy_naming_the_tool() {
        // ApprovalRequest::Unknown's contract: "summary is Comet copy, never
        // provider prose". 1.5's rendered check flagged that this promise had
        // no adapter behind it; this is the adapter.
        let got = approval_request(&body("WebFetch", json!({"url": "https://example.com"})));
        assert_eq!(
            got,
            ApprovalRequest::Unknown {
                summary: "Run the WebFetch tool".into(),
            }
        );
        let ApprovalRequest::Unknown { summary } = got else {
            unreachable!()
        };
        assert!(
            !summary.contains("provider prose"),
            "the CLI's description must not reach the card"
        );
    }

    #[test]
    fn a_tool_with_no_name_is_still_answerable() {
        // Absent case: `tool_name` defaults to "" on a malformed frame. The
        // card must still say something, because the run is blocked until it
        // is answered.
        let got = approval_request(&body("", json!({})));
        assert_eq!(
            got,
            ApprovalRequest::Unknown {
                summary: "Run a tool Comet could not identify".into(),
            }
        );
    }

    #[test]
    fn a_write_with_no_path_does_not_pretend_to_know_one() {
        let got = approval_request(&body("Write", json!({"content": "hi"})));
        assert_eq!(
            got,
            ApprovalRequest::Unknown {
                summary: "Run the Write tool".into(),
            }
        );
    }

    #[test]
    fn an_edit_that_empties_the_text_is_a_pure_deletion() {
        // (None, false) arm: new_string present but empty is filtered out by
        // str_field, so this lands on the "no replacement text" branch with a
        // non-empty old_string — a Modify that only removes lines.
        let got = approval_request(&body(
            "Edit",
            json!({"file_path": "a.txt", "old_string": "one\ntwo\n", "new_string": ""}),
        ));
        assert_eq!(
            got,
            ApprovalRequest::FileChange {
                path: "a.txt".into(),
                operation: FileOperation::Modify,
                added_lines: 0,
                removed_lines: 2,
            }
        );
    }

    #[test]
    fn an_edit_with_neither_string_populated_is_an_unknown_operation() {
        // (None, true) arm: no old_string and no new replacement text at all.
        let got = approval_request(&body("Edit", json!({"file_path": "a.txt"})));
        assert_eq!(
            got,
            ApprovalRequest::FileChange {
                path: "a.txt".into(),
                operation: FileOperation::Unknown,
                added_lines: 0,
                removed_lines: 0,
            }
        );
    }

    #[test]
    fn an_mcp_tool_missing_its_tool_half_still_names_the_server() {
        let got = approval_request(&body("mcp__linear", json!({})));
        assert_eq!(
            got,
            ApprovalRequest::Mcp {
                server: "linear".into(),
                tool: "".into(),
            }
        );
    }

    #[test]
    fn a_bare_mcp_prefix_names_no_server_and_is_unknown() {
        let got = approval_request(&body("mcp__", json!({})));
        assert_eq!(
            got,
            ApprovalRequest::Unknown {
                summary: "Run the mcp__ tool".into(),
            }
        );
    }

    #[test]
    fn an_mcp_server_literally_named_mcp_keeps_both_halves() {
        // strip_prefix removes the guard-matched "mcp__" exactly once; the
        // old trim_start_matches would also eat the server's own "mcp__".
        let got = approval_request(&body("mcp__mcp__foo", json!({})));
        assert_eq!(
            got,
            ApprovalRequest::Mcp {
                server: "mcp".into(),
                tool: "foo".into(),
            }
        );
    }

    #[test]
    fn line_count_counts_lines_not_trailing_newlines() {
        assert_eq!(line_count(""), 0);
        assert_eq!(line_count("hi\n"), 1);
        assert_eq!(line_count("\n"), 1);
        assert_eq!(line_count("\n\n"), 2);
        // A trailing newline does not begin a line (singular) — only the
        // last one is stripped, so two blank lines still count.
        assert_eq!(line_count("a\n\n\n"), 3);
    }
}
