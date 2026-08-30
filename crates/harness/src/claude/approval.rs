//! Claude's `can_use_tool` request → Comet's `ApprovalRequest`.
//!
//! Tool names and input shapes come from `claude/2.1.228/approval` frame 102:
//! `can_use_tool` requests use the captured tool names and input fields, and
//! captured `Write` requests carry an absolute `file_path` and no cwd. Plus the
//! tool set in sdk.d.ts. An unrecognized tool is NOT an
//! error — it becomes `Unknown` with Comet copy, because the alternative is
//! either dropping the request (the agent hangs) or auto-allowing it.

use comet_proto::{ApprovalRequest, FileOperation};

use super::wire::ControlRequestBody;

/// `file_exists` answers whether a path already holds a file. It is injected so
/// this stays a pure function of its inputs: the real call site passes
/// `Path::exists`, the tests pass a closure that states the case under test.
pub(crate) fn approval_request(
    body: &ControlRequestBody,
    file_exists: impl Fn(&str) -> bool,
) -> ApprovalRequest {
    // Two readings, because emptiness means different things per field. For a
    // path or a command an empty string carries no more information than an
    // absent one, so both degrade to `Unknown`. For the CONTENT of a write it
    // is a value: `{"file_path":"x","content":""}` is claude creating an empty
    // file, and reading that "" as absence renders it "+0 −0" with no
    // operation at all instead of a create.
    let raw_field = |key: &str| {
        body.input
            .get(key)
            .and_then(|v| v.as_str())
            .map(str::to_owned)
    };
    let str_field = |key: &str| raw_field(key).filter(|s| !s.is_empty());

    match body.tool_name.as_str() {
        "Bash" => match str_field("command") {
            // This wire schema has no working-directory field. The adapter
            // must not invent one from unrelated run state.
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
            let old = raw_field("old_string").unwrap_or_default();
            let new = raw_field("new_string").or_else(|| raw_field("content"));
            let (operation, added, removed) = match (&new, old.is_empty()) {
                // A Write carries content and never an `old_string` — including
                // when it lands on a file that already exists, which it
                // overwrites. So the missing `old_string` is not evidence of a
                // new file, and the filesystem is the only thing that knows.
                (Some(text), true) if body.tool_name == "Write" => {
                    (write_operation(&path, &file_exists), line_count(text), 0)
                }
                // Reachable only for Edit (the Write case above already
                // claimed every `old.is_empty()` input for that tool). An
                // empty `new_string` alone reads as a real deletion — see the
                // `(Some(text), _)` arm below — but only because `old_string`
                // names the text being deleted. With `old_string` absent (or
                // itself empty; both read as `old.is_empty()`) there is
                // nothing to delete either: no target and no replacement, so
                // this is unreadable input, not a zero-line edit. Read
                // literally it used to produce FileChange{Modify, +0, -0} — a
                // signature indistinguishable from a REAL modify to the same
                // path, since approval_signature deliberately drops line
                // counts (a genuine repeat edit must still match). Allowing
                // that no-op card for the session silently auto-allowed every
                // later real edit to the file (D17). Same treatment as
                // `(None, true)` below, its sibling with an absent
                // `new_string` key instead of an empty one.
                (Some(text), true) if text.is_empty() => (FileOperation::Unknown, 0, 0),
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

/// What a `Write` to `path` actually does: overwrite what is there, or make a
/// file that is not. The adapter runs on the machine holding the file, and the
/// captured `file_path` is absolute, so this is knowable rather than guessable.
fn write_operation(path: &str, file_exists: &impl Fn(&str) -> bool) -> FileOperation {
    if !std::path::Path::new(path).is_absolute() {
        // A relative path has nothing to resolve against here: the request
        // carries no working directory (captured), so an existence check would
        // answer for whatever directory this process happens to sit in. Say
        // "Change a file" instead — the same trade `FileOperation::Unknown`
        // already makes in comet-proto, where being vague about the verb beats
        // naming the wrong one.
        return FileOperation::Unknown;
    }
    if file_exists(path) {
        FileOperation::Modify
    } else {
        FileOperation::Create
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
            tool_use_id: String::new(),
            description: Some("provider prose that must not reach the card".into()),
        }
    }

    /// An absolute path on the platform the test runs on. A leading `/` is NOT
    /// absolute on Windows (no drive prefix), so a hardcoded POSIX path would
    /// take the relative branch there and test something else.
    fn abs(name: &str) -> String {
        if cfg!(windows) {
            format!("C:\\tmp\\{name}")
        } else {
            format!("/tmp/{name}")
        }
    }

    /// The filesystem answer, stated per test rather than read off this machine.
    fn nothing_exists(_: &str) -> bool {
        false
    }
    fn everything_exists(_: &str) -> bool {
        true
    }

    #[test]
    fn write_to_a_new_file_is_a_create() {
        let path = abs("a.txt");
        let got = approval_request(
            &body("Write", json!({"file_path": path, "content": "hi\n"})),
            nothing_exists,
        );
        assert_eq!(
            got,
            ApprovalRequest::FileChange {
                path,
                operation: FileOperation::Create,
                added_lines: 1,
                removed_lines: 0,
            }
        );
    }

    #[test]
    fn write_over_an_existing_file_is_a_modify() {
        // Claude's Write OVERWRITES; it never sends `old_string`, so the card
        // used to call every Write a create. "Create a file" for a destructive
        // overwrite is a false statement on the one surface whose job is
        // telling the user what they are about to authorize.
        let path = abs("a.txt");
        let got = approval_request(
            &body("Write", json!({"file_path": path, "content": "hi\n"})),
            everything_exists,
        );
        assert_eq!(
            got,
            ApprovalRequest::FileChange {
                path,
                operation: FileOperation::Modify,
                added_lines: 1,
                removed_lines: 0,
            }
        );
    }

    #[test]
    fn a_write_to_a_relative_path_does_not_guess_the_operation() {
        // Nothing to resolve it against — the request carries no cwd — so the
        // existence check would answer for this process's directory. The card
        // says "Change a file" rather than picking a verb by coin flip. It is
        // also never allowlistable: FileOperation::Unknown has no signature.
        let got = approval_request(
            &body("Write", json!({"file_path": "a.txt", "content": "hi\n"})),
            |_| panic!("a relative path must not be probed on the filesystem"),
        );
        assert_eq!(
            got,
            ApprovalRequest::FileChange {
                path: "a.txt".into(),
                operation: FileOperation::Unknown,
                added_lines: 1,
                removed_lines: 0,
            }
        );
    }

    #[test]
    fn the_path_the_check_is_asked_about_is_the_path_on_the_card() {
        let path = abs("a.txt");
        let seen = std::cell::RefCell::new(Vec::new());
        let got = approval_request(
            &body("Write", json!({"file_path": path, "content": "hi\n"})),
            |p| {
                seen.borrow_mut().push(p.to_owned());
                false
            },
        );
        assert_eq!(seen.into_inner(), vec![path.clone()]);
        assert!(matches!(got, ApprovalRequest::FileChange { path: p, .. } if p == path));
    }

    #[test]
    fn writing_an_empty_file_is_still_a_create() {
        // `content: ""` is claude creating an empty file — a shape it emits.
        // Read as absence it fell through to `FileOperation::Unknown`, so the
        // card said "Change a file · +0 −0" for a plain create.
        let path = abs("a.txt");
        let got = approval_request(
            &body("Write", json!({"file_path": path, "content": ""})),
            nothing_exists,
        );
        assert_eq!(
            got,
            ApprovalRequest::FileChange {
                path,
                operation: FileOperation::Create,
                added_lines: 0,
                removed_lines: 0,
            }
        );
    }

    #[test]
    fn edit_is_a_modify_and_counts_the_replacement() {
        let got = approval_request(
            &body(
                "Edit",
                json!({"file_path": "a.txt", "old_string": "one\ntwo\n", "new_string": "one\n"}),
            ),
            nothing_exists,
        );
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
    fn bash_command_schema_does_not_invent_a_directory() {
        // Keep the adapter limited to fields present in ControlRequestBody;
        // run state is not evidence about the command's working directory.
        let got = approval_request(
            &body("Bash", json!({"command": "cargo test"})),
            nothing_exists,
        );
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
        let got = approval_request(
            &body("Read", json!({"file_path": "/etc/hosts"})),
            nothing_exists,
        );
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
        let got = approval_request(
            &body("mcp__linear__create_issue", json!({})),
            nothing_exists,
        );
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
        let got = approval_request(
            &body("WebFetch", json!({"url": "https://example.com"})),
            nothing_exists,
        );
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
        let got = approval_request(&body("", json!({})), nothing_exists);
        assert_eq!(
            got,
            ApprovalRequest::Unknown {
                summary: "Run a tool Comet could not identify".into(),
            }
        );
    }

    #[test]
    fn a_write_with_no_path_does_not_pretend_to_know_one() {
        let got = approval_request(&body("Write", json!({"content": "hi"})), nothing_exists);
        assert_eq!(
            got,
            ApprovalRequest::Unknown {
                summary: "Run the Write tool".into(),
            }
        );
    }

    #[test]
    fn an_edit_that_empties_the_text_is_a_pure_deletion() {
        // An empty `new_string` is a value — "replace it with nothing" — so it
        // is read as present and counted as zero added lines against the old
        // string's two. (The `(None, false)` arm, no `new_string` key at all,
        // is the same answer by a different route.)
        let got = approval_request(
            &body(
                "Edit",
                json!({"file_path": "a.txt", "old_string": "one\ntwo\n", "new_string": ""}),
            ),
            nothing_exists,
        );
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
        let got = approval_request(&body("Edit", json!({"file_path": "a.txt"})), nothing_exists);
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
    fn an_edit_with_an_empty_new_string_and_no_old_string_is_also_unknown() {
        // D17: this used to fall into the general `(Some(text), _) => Modify`
        // arm — an empty `new_string` normally means "replace with nothing"
        // (a real deletion, see the pure-deletion test below), but that
        // reading only holds when `old_string` names something to delete.
        // With no `old_string` at all, nothing was specified in either
        // direction: not a deletion, just unreadable input. The old behavior
        // produced FileChange{Modify, +0, -0} — a signature indistinguishable
        // from a REAL modify to the same path, because approval_signature
        // deliberately drops line counts. Allowing this no-op card for the
        // session would silently auto-allow every later real edit to the file.
        let got = approval_request(
            &body("Edit", json!({"file_path": "a.txt", "new_string": ""})),
            nothing_exists,
        );
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
    fn an_edit_with_an_explicitly_empty_old_string_and_empty_new_string_is_unknown_too() {
        // Same defect, the other door: an `old_string` key present but empty
        // reads identically to an absent one (`old.is_empty()`), so it must
        // route the same way as the absent case above.
        let got = approval_request(
            &body(
                "Edit",
                json!({"file_path": "a.txt", "old_string": "", "new_string": ""}),
            ),
            nothing_exists,
        );
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
        let got = approval_request(&body("mcp__linear", json!({})), nothing_exists);
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
        let got = approval_request(&body("mcp__", json!({})), nothing_exists);
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
        let got = approval_request(&body("mcp__mcp__foo", json!({})), nothing_exists);
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
