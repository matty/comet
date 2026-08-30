//! A Codex `requestApproval` server request → Comet's [`ApprovalRequest`], and
//! a user's [`ApprovalDecision`] → the decision literal the app-server takes.
//!
//! Shapes follow the generated schema in `t3code/packages/effect-codex-app-server`. An
//! unrecognized method is NOT an error — it becomes `Unknown` with Comet copy,
//! because the alternatives are dropping the request (the turn wedges) or
//! accepting it unasked.

use comet_proto::{ApprovalDecision, ApprovalRequest, FileOperation};
use serde_json::{Value, json};

pub(crate) const COMMAND_APPROVAL: &str = "item/commandExecution/requestApproval";
pub(crate) const FILE_CHANGE_APPROVAL: &str = "item/fileChange/requestApproval";
/// A third approval method in `ServerRequest` that no capture run produced.
/// Its params are filesystem/network permission overlays, which fit no
/// `ApprovalRequest` variant — see [`approval_request`]'s permissions arm.
pub(crate) const PERMISSIONS_APPROVAL: &str = "item/permissions/requestApproval";

/// `changes` is the `changes` array the adapter recorded for this request's
/// `itemId`, or `None` when it never saw one.
///
/// It is passed in rather than looked up so this stays a pure function of its
/// inputs. **A file-change request carries no path, no diff and no `changes`**
/// (captured; the schema agrees) — only ids and timestamps. The detail arrives
/// on the `item/started` that precedes it, which is why the join exists at all.
pub(crate) fn approval_request(
    method: &str,
    params: &Value,
    changes: Option<&Value>,
) -> ApprovalRequest {
    match method {
        COMMAND_APPROVAL => match command_text(params) {
            Some(command) => ApprovalRequest::Command {
                command,
                // Written by hand, per `.agents/rules/optional-wire-fields.md`:
                // an absent `cwd` must stay absent rather than becoming "", which
                // `approval_signature` treats as a different action.
                cwd: params.get("cwd").and_then(Value::as_str).map(str::to_owned),
            },
            None => unknown_command(),
        },
        FILE_CHANGE_APPROVAL => file_change(changes),
        PERMISSIONS_APPROVAL => ApprovalRequest::Unknown {
            // Written blind: no capture run produced one of these, so the copy
            // is derived from the schema (filesystem + network overlays) rather
            // than from a frame. `Unknown` is not allowlistable, so
            // Allow-for-session correctly degrades to allowing this one call.
            summary: "Grant Codex additional permissions".to_owned(),
        },
        _ => ApprovalRequest::Unknown {
            summary: "Take an action Comet could not identify".to_owned(),
        },
    }
}

/// What to put on the card, and therefore what a session grant is keyed on.
///
/// **Deliberately the parsed action, not the raw `command`.** The schema gives
/// the parsed action as the stable user-facing operation while the raw field is
/// a launcher invocation. Session grants key on the operation rather than
/// process-launch details, matching `approval_signature`'s exclusion of other
/// volatile fields.
///
/// More than one parsed action falls back to the raw command: joining them
/// would assert a sequencing relationship the wire does not state, and the raw
/// text is at least verbatim true.
fn command_text(params: &Value) -> Option<String> {
    let actions = params
        .get("commandActions")
        .and_then(Value::as_array)
        .map(|a| a.as_slice())
        .unwrap_or_default();
    if let [only] = actions
        && let Some(text) = only
            .get("command")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    {
        return Some(text.to_owned());
    }
    params
        .get("command")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Reduce a `fileChange` item's `changes` to the little the card can render,
/// at the moment the item is announced.
///
/// The adapter holds the result until the approval request that needs it
/// arrives, so **what it holds must not scale with what the agent is
/// changing.** Only two things are ever read back: a single change's path,
/// operation and line counts, or — for a multi-file change, which
/// `ApprovalRequest::FileChange` cannot render — how many files there were.
/// Every `diff` is consumed into counts here and dropped, so a large patch
/// costs the same to remember as a one-line edit.
pub(crate) fn summarize_changes(changes: &Value) -> Value {
    let Some([change]) = changes.as_array().map(|c| c.as_slice()) else {
        let count = changes.as_array().map(|c| c.len()).unwrap_or(0);
        return json!({ "count": count });
    };
    let operation = change_kind(change);
    let diff = change.get("diff").and_then(Value::as_str).unwrap_or("");
    let (added, removed) = match operation {
        // An add's `diff` is the raw content of the new file, not a patch.
        FileOperation::Create => (line_count(diff), 0),
        FileOperation::Delete => (0, line_count(diff)),
        // An update's is a unified diff.
        _ => unified_diff_counts(diff),
    };
    json!({
        "path": change.get("path").and_then(Value::as_str).unwrap_or(""),
        "operation": operation,
        "addedLines": added,
        "removedLines": removed,
    })
}

/// Takes the reduced value [`summarize_changes`] produced, not the raw wire
/// array — see there for why the raw payload is not kept.
fn file_change(summary: Option<&Value>) -> ApprovalRequest {
    let Some(summary) = summary else {
        // The join missed: the request named an `itemId` whose `item/started`
        // the adapter never saw. Say so vaguely rather than rendering a
        // `FileChange` with an empty path, which would read as a change to a
        // file called "".
        return ApprovalRequest::Unknown {
            summary: "Change a file".to_owned(),
        };
    };
    // `FileChange` names ONE path. A multi-file change has no honest rendering
    // in it, and `Unknown` is the un-allowlistable variant, which is the
    // conservative answer for a change Comet cannot state in full. The count is
    // all `summarize_changes` kept for that case.
    if let Some(count) = summary.get("count").and_then(Value::as_u64) {
        return ApprovalRequest::Unknown {
            summary: if count == 0 {
                "Change a file".to_owned()
            } else {
                format!("Change {count} files")
            },
        };
    }
    let Some(path) = summary
        .get("path")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return ApprovalRequest::Unknown {
            summary: "Change a file".to_owned(),
        };
    };
    ApprovalRequest::FileChange {
        path: path.to_owned(),
        operation: serde_json::from_value(summary.get("operation").cloned().unwrap_or(Value::Null))
            .unwrap_or(FileOperation::Unknown),
        added_lines: u32_field(summary, "addedLines"),
        removed_lines: u32_field(summary, "removedLines"),
    }
}

fn u32_field(value: &Value, key: &str) -> u32 {
    value
        .get(key)
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(u32::MAX as u64) as u32
}

/// `kind` is an object on the wire (`{"type":"add"}`); the bare-string arm is
/// kept for an older peer. Same tolerance as `normalize::map_item`.
fn change_kind(change: &Value) -> FileOperation {
    let kind = change
        .get("kind")
        .and_then(|k| k.as_str().or_else(|| k.get("type").and_then(Value::as_str)));
    match kind {
        Some("add") => FileOperation::Create,
        Some("update") => FileOperation::Modify,
        Some("delete") => FileOperation::Delete,
        // Being vague about the verb beats naming the wrong one — the same
        // trade `claude/approval.rs` makes for a relative path.
        _ => FileOperation::Unknown,
    }
}

fn line_count(text: &str) -> u32 {
    if text.is_empty() {
        return 0;
    }
    text.lines().count() as u32
}

/// `+`/`-` line counts of a unified diff.
///
/// **File headers are excluded by position, not by prefix.** `--- a/x` and
/// `+++ b/x` can only appear before the first `@@`, so that is what gates them.
/// A prefix test cannot do this job at all: an added line whose content begins
/// with `++` is spelled `+++…` in the diff and is indistinguishable from a
/// header by its opening characters — a trailing space does not separate them
/// either, since `++ foo` is a perfectly ordinary added line. Skipping on
/// prefix therefore drops real changes from the count.
///
/// A diff with no `@@` at all is not a unified diff; rather than report nothing
/// for a shape this has not seen, it falls back to counting every `+`/`-` line.
/// Headerless hunked updates naturally have no preamble, so neither exclusion
/// rule has anything to do in that shape.
fn unified_diff_counts(diff: &str) -> (u32, u32) {
    let mut added = 0;
    let mut removed = 0;
    let mut in_hunk = false;
    for line in diff.lines() {
        if line.starts_with("@@") {
            in_hunk = true;
            continue;
        }
        if !in_hunk {
            continue;
        }
        if line.starts_with('+') {
            added += 1;
        } else if line.starts_with('-') {
            removed += 1;
        }
    }
    if in_hunk {
        return (added, removed);
    }
    let mut added = 0;
    let mut removed = 0;
    for line in diff.lines() {
        if line.starts_with('+') {
            added += 1;
        } else if line.starts_with('-') {
            removed += 1;
        }
    }
    (added, removed)
}

fn unknown_command() -> ApprovalRequest {
    ApprovalRequest::Unknown {
        summary: "Run a command Comet could not read".to_owned(),
    }
}

/// The decision literal the app-server takes for this answer.
///
/// **`acceptForSession` is never produced.** It is real and works, but Codex's
/// session grant is keyed on the path alone and spans the operation kind,
/// which is broader than `approval_signature`; delegating would leave two
/// grant caches with different scopes (`docs/debt/README.md` D20).
pub fn decision_literal(decision: &ApprovalDecision) -> &'static str {
    match decision {
        // Comet's engine owns the session grant and answers the repeat itself,
        // so the wire only ever hears about this one call.
        ApprovalDecision::Allow | ApprovalDecision::AllowForSession => "accept",
        ApprovalDecision::Deny { .. } => "decline",
        ApprovalDecision::DenyAndInterrupt { .. } => "cancel",
        // The user never answered and never will. Not approved.
        ApprovalDecision::Expired => "decline",
    }
}

/// The complete JSON-RPC result object sent for a Codex approval request.
pub(crate) fn decision_response(decision: &ApprovalDecision) -> Value {
    json!({ "decision": decision_literal(decision) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_command_is_named_by_its_parsed_action_not_its_launcher() {
        let params = json!({
            "itemId": "exec-1",
            "command": "\"C:\\Program Files\\pwsh.exe\" -Command 'echo one > a.txt'",
            "commandActions": [{"type": "unknown", "command": "echo one > a.txt"}],
            "cwd": "C:\\work",
        });
        assert_eq!(
            approval_request(COMMAND_APPROVAL, &params, None),
            ApprovalRequest::Command {
                command: "echo one > a.txt".into(),
                cwd: Some("C:\\work".into()),
            }
        );
    }

    #[test]
    fn the_parsed_action_is_what_makes_a_session_grant_match_again() {
        // Launcher details are transport metadata. The parsed action is the
        // schema-level identity used for a repeatable session grant.
        let first = json!({
            "command": "\"pwsh.exe\" -Command 'echo one > a.txt'",
            "commandActions": [{"type": "unknown", "command": "echo one > a.txt"}],
        });
        let third = json!({
            "command": "\"pwsh.exe\" -NoProfile -Command 'echo one > a.txt'",
            "commandActions": [{"type": "unknown", "command": "echo one > a.txt"}],
        });
        assert_eq!(
            approval_request(COMMAND_APPROVAL, &first, None),
            approval_request(COMMAND_APPROVAL, &third, None)
        );
    }

    #[test]
    fn a_command_falls_back_to_the_raw_text_and_then_to_unknown() {
        let no_actions = json!({"command": "cargo test"});
        assert_eq!(
            approval_request(COMMAND_APPROVAL, &no_actions, None),
            ApprovalRequest::Command {
                command: "cargo test".into(),
                cwd: None,
            }
        );
        // More than one parsed action: the raw text is verbatim true, joining
        // them would not be.
        let multi = json!({
            "command": "a && b",
            "commandActions": [{"command": "a"}, {"command": "b"}],
        });
        assert_eq!(
            approval_request(COMMAND_APPROVAL, &multi, None),
            ApprovalRequest::Command {
                command: "a && b".into(),
                cwd: None,
            }
        );
        assert_eq!(
            approval_request(COMMAND_APPROVAL, &json!({"command": ""}), None),
            ApprovalRequest::Unknown {
                summary: "Run a command Comet could not read".into()
            }
        );
    }

    #[test]
    fn an_absent_cwd_stays_absent() {
        // `None` and `Some("")` are different actions to `approval_signature`,
        // which has its own test for the collision. Do not invent a directory.
        let params = json!({"commandActions": [{"command": "ls"}]});
        assert_eq!(
            approval_request(COMMAND_APPROVAL, &params, None),
            ApprovalRequest::Command {
                command: "ls".into(),
                cwd: None,
            }
        );
    }

    /// End to end over the real wire shape: what the item announces, reduced
    /// the way the adapter reduces it, then mapped.
    #[test]
    fn a_file_change_is_read_off_the_joined_item() {
        let changes = json!([{"path": "/a.rs", "kind": {"type": "add"}, "diff": "one\ntwo\n"}]);
        let summary = summarize_changes(&changes);
        assert_eq!(
            approval_request(FILE_CHANGE_APPROVAL, &json!({}), Some(&summary)),
            ApprovalRequest::FileChange {
                path: "/a.rs".into(),
                operation: FileOperation::Create,
                added_lines: 2,
                removed_lines: 0,
            }
        );
        let update = json!([{"path": "/b.rs", "kind": {"type": "update"},
                             "diff": "@@ -1 +1,2 @@\n one\n+two\n"}]);
        let summary = summarize_changes(&update);
        assert_eq!(
            approval_request(FILE_CHANGE_APPROVAL, &json!({}), Some(&summary)),
            ApprovalRequest::FileChange {
                path: "/b.rs".into(),
                operation: FileOperation::Modify,
                added_lines: 1,
                removed_lines: 0,
            }
        );
    }

    /// What is remembered between the item and its approval must not scale
    /// with the size of the change: capping the number of tracked items is no
    /// bound at all if one item can carry an arbitrarily large payload.
    #[test]
    fn a_summary_keeps_no_diff_however_big_the_change_is() {
        let huge = "@@ -1 +1,20000 @@\n".to_owned() + &"+line\n".repeat(20_000);
        let changes = json!([{"path": "/a.rs", "kind": {"type": "update"}, "diff": huge}]);
        let summary = summarize_changes(&changes);
        assert!(
            summary.to_string().len() < 200,
            "summary retained the payload: {} bytes",
            summary.to_string().len()
        );
        assert_eq!(summary["addedLines"], 20_000);

        // A multi-file change keeps only how many files there were, because
        // that is all the `Unknown` copy can say.
        let many: Vec<Value> = (0..5_000)
            .map(|i| json!({"path": format!("/f{i}.rs"), "kind": {"type": "add"}, "diff": "x\n"}))
            .collect();
        let summary = summarize_changes(&Value::Array(many));
        assert_eq!(summary, json!({"count": 5_000}));
        assert_eq!(
            approval_request(FILE_CHANGE_APPROVAL, &json!({}), Some(&summary)),
            ApprovalRequest::Unknown {
                summary: "Change 5000 files".into()
            }
        );
    }

    /// A prefix test cannot tell a file header from an added line whose content
    /// starts with `++`, with or without a trailing space. Position can: headers
    /// only ever precede the first `@@`.
    #[test]
    fn diff_counts_do_not_drop_lines_that_look_like_headers() {
        let changes = json!([{"path": "/a.rs", "kind": {"type": "update"},
                              "diff": "--- a/a.rs\n+++ b/a.rs\n@@ -1,2 +1,3 @@\n+++ added\n++also\n-- removed\n context\n"}]);
        let summary = summarize_changes(&changes);
        assert_eq!(summary["addedLines"], 2, "real `+` lines were skipped");
        assert_eq!(summary["removedLines"], 1, "a real `-` line was skipped");

        // The headers themselves are still excluded, by position.
        let headers_only = json!([{"path": "/a.rs", "kind": {"type": "update"},
                                   "diff": "--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n+one\n"}]);
        assert_eq!(summarize_changes(&headers_only)["addedLines"], 1);
        assert_eq!(summarize_changes(&headers_only)["removedLines"], 0);
    }

    #[test]
    fn a_file_change_whose_item_was_never_seen_is_unknown() {
        // The absent case, written by hand: the fixtures always announce the
        // item first, so this path would otherwise ship unconstructed.
        assert_eq!(
            approval_request(FILE_CHANGE_APPROVAL, &json!({"itemId": "exec-9"}), None),
            ApprovalRequest::Unknown {
                summary: "Change a file".into()
            }
        );
    }

    #[test]
    fn a_multi_file_change_is_unknown_rather_than_one_of_its_paths() {
        let changes = json!([{"path": "/a.rs", "kind": {"type": "add"}},
                             {"path": "/b.rs", "kind": {"type": "delete"}}]);
        let summary = summarize_changes(&changes);
        assert_eq!(
            approval_request(FILE_CHANGE_APPROVAL, &json!({}), Some(&summary)),
            ApprovalRequest::Unknown {
                summary: "Change 2 files".into()
            }
        );
    }

    #[test]
    fn an_unreadable_kind_is_vague_rather_than_wrong() {
        let changes = json!([{"path": "/a.rs", "kind": {"type": "teleport"}}]);
        let summary = summarize_changes(&changes);
        assert_eq!(
            approval_request(FILE_CHANGE_APPROVAL, &json!({}), Some(&summary)),
            ApprovalRequest::FileChange {
                path: "/a.rs".into(),
                operation: FileOperation::Unknown,
                added_lines: 0,
                removed_lines: 0,
            }
        );
    }

    #[test]
    fn no_summary_is_ever_provider_prose() {
        // `ApprovalRequest::Unknown` promises Comet copy. The live `reason`
        // field is the prose most likely to be reached for; this is the
        // tripwire that says no.
        let prose =
            "Allow running the exact command you requested after the sandbox process failed?";
        for method in [
            PERMISSIONS_APPROVAL,
            "item/somethingNew/requestApproval",
            FILE_CHANGE_APPROVAL,
        ] {
            let params = json!({"reason": prose, "command": ""});
            if let ApprovalRequest::Unknown { summary } = approval_request(method, &params, None) {
                assert!(
                    !summary.contains("sandbox"),
                    "{method} leaked provider prose: {summary}"
                );
            }
        }
    }

    #[test]
    fn no_decision_is_ever_accept_for_session() {
        // `acceptForSession` works but remains deliberately unused
        // (docs/debt/README.md D20): Comet's engine owns the session grant.
        for decision in [
            ApprovalDecision::Allow,
            ApprovalDecision::AllowForSession,
            ApprovalDecision::Deny {
                message: "no".into(),
            },
            ApprovalDecision::DenyAndInterrupt {
                message: "stop".into(),
            },
            ApprovalDecision::Expired,
        ] {
            let literal = decision_literal(&decision);
            assert!(
                matches!(literal, "accept" | "decline" | "cancel"),
                "{decision:?} produced {literal}"
            );
            assert_ne!(literal, "acceptForSession");
        }
        assert_eq!(
            decision_literal(&ApprovalDecision::AllowForSession),
            "accept"
        );
        assert_eq!(decision_literal(&ApprovalDecision::Expired), "decline");
    }

    /// This is the literal JSON-RPC result body sent to Codex app-server, not
    /// a round-trip through Comet's own wire types. Ordinary Deny must keep
    /// allowing the turn to continue; only the new action emits `cancel`.
    #[test]
    fn deny_and_interrupt_has_the_native_cancel_response() {
        assert_eq!(
            decision_response(&ApprovalDecision::Deny {
                message: "try something else".into(),
            }),
            json!({"decision": "decline"})
        );
        assert_eq!(
            decision_response(&ApprovalDecision::DenyAndInterrupt {
                message: "stop this turn".into(),
            }),
            json!({"decision": "cancel"})
        );
    }
}
