//! ACP's `session/request_permission` <-> Comet's [`ApprovalRequest`] /
//! [`ApprovalDecision`].
//!
//! **Neither recorded agent has ever been captured sending this method.**
//! Grok 1.0.5, probed live 2026-08-29 under Comet's exact launch
//! (`grok::GROK_ARGS`, `initialize_params()`) with a prompt that writes a file
//! outside the working directory — the trigger this crate's own task brief
//! names — tracked the write as an internal `pending_interaction` of kind
//! `"permission"` (a vendor `_x.ai/session_notification`) and resolved it
//! itself without ever putting `session/request_permission` on the wire; see
//! `grok::capabilities`'s doc comment for the full account. Hermes cannot open
//! a session on this machine at all (a Python 3.14 stdlib removal breaks its
//! installed 0.15.2), so there is no live Hermes evidence either.
//!
//! What this module decodes against is therefore the ACP spec's own hypothesis
//! (`{sessionId, toolCall, options: [{optionId, name, kind}]}`, `kind` in the
//! four the spec names, answered `{outcome: {outcome: "selected", optionId}}`
//! or `{outcome: {outcome: "cancelled"}}`), corroborated two ways: Hermes'
//! *installed* Python source (`acp_adapter/permissions.py`,
//! `acp_adapter/edit_approval.py` — read, not captured, and weaker evidence
//! than a capture) builds exactly this shape for both a dangerous-command
//! approval and a file-edit approval; and Grok's own real `tool_call` /
//! `tool_call_update` frames for the same file write (captured live
//! 2026-08-29) carry a `diff` content block (`path`/`oldText`/`newText`) in
//! the identical shape ACP's `ToolCallContent::Diff` names, which is the same
//! `toolCall` a `session/request_permission` for that edit would have carried
//! had Grok escalated it. Two independent, non-hypothetical sources agreeing
//! on the same wire shape is the strongest evidence available without a real
//! capture — labelled as such everywhere it is relied on.
//!
//! Because of that gap, this only decodes the two shapes evidenced above:
//! a `diff` content block (`FileChange`) and an `execute`-kind call with a
//! structured `rawInput.command` (`Command`, Hermes'
//! `_build_permission_tool_call`). `FileRead` and `Mcp` are not evidenced by
//! either source, so a request this build cannot place in one of the first
//! two shapes decodes as `Unknown` — never a guess at a shape nobody has
//! demonstrated.

use comet_proto::{ApprovalDecision, ApprovalRequest, FileOperation, ToolDiff};
use serde_json::{Value, json};

/// The JSON-RPC method name, exported so `acp::session` can match on it and
/// so a diagnostic raised for a malformed request can discriminate on the
/// same literal.
pub(crate) const REQUEST_PERMISSION_METHOD: &str = "session/request_permission";

const ALLOW_ONCE: &str = "allow_once";
const ALLOW_ALWAYS: &str = "allow_always";
const REJECT_ONCE: &str = "reject_once";
const REJECT_ALWAYS: &str = "reject_always";

/// Whether `options` names at least one of the four kinds the ACP spec
/// defines.
///
/// `false` is a protocol-drift signal — a vocabulary this build has never
/// seen at all — never merely "the option this decision wants is missing".
/// The two are different failures: the second still lets some OTHER decision
/// through honestly (see [`outcome_for`]), while the first means nothing this
/// request offers can be trusted.
pub(crate) fn has_recognized_kind(options: &[Value]) -> bool {
    options.iter().any(|option| {
        matches!(
            option["kind"].as_str(),
            Some(ALLOW_ONCE | ALLOW_ALWAYS | REJECT_ONCE | REJECT_ALWAYS)
        )
    })
}

/// [`ApprovalRequest`] from ACP's `toolCall` param. See the module doc for
/// which shapes are evidenced and which fall back to `Unknown`.
pub(crate) fn approval_request(tool_call: &Value) -> ApprovalRequest {
    let content = tool_call["content"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    if let Some(diff) = content.iter().find(|block| block["type"] == "diff") {
        return file_change(tool_call, diff);
    }
    if tool_call["kind"] == "execute" {
        return command(tool_call);
    }
    ApprovalRequest::Unknown {
        summary: "Take an action Comet could not identify".to_owned(),
    }
}

/// A `diff` content block, reduced the way `codex::approval::summarize_changes`
/// reduces Codex's `changes` array — line counts only, never the file's own
/// text held any longer than it takes to count them.
fn file_change(tool_call: &Value, diff: &Value) -> ApprovalRequest {
    let path = diff["path"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or_else(|| tool_call["locations"][0]["path"].as_str())
        .unwrap_or_default();
    if path.is_empty() {
        return ApprovalRequest::Unknown {
            summary: "Change a file".to_owned(),
        };
    }
    // Absent and empty both read as "no prior text", i.e. a new file: ACP
    // gives no separate signal for "this key was never sent" the way Comet's
    // own optional fields do, so there is nothing else to distinguish here.
    let old_text = diff["oldText"].as_str().filter(|s| !s.is_empty());
    let new_text = diff["newText"].as_str().unwrap_or_default();
    let operation = if tool_call["kind"] == "delete" {
        FileOperation::Delete
    } else if old_text.is_none() {
        FileOperation::Create
    } else {
        FileOperation::Modify
    };
    let stat = ToolDiff {
        path: path.to_owned(),
        old_text: old_text.map(str::to_owned),
        new_text: new_text.to_owned(),
    }
    .stat();
    ApprovalRequest::FileChange {
        path: path.to_owned(),
        operation,
        added_lines: u32_clamped(stat.additions),
        removed_lines: u32_clamped(stat.deletions),
    }
}

fn u32_clamped(value: u64) -> u32 {
    value.min(u32::MAX as u64) as u32
}

/// An `execute`-kind call. Hermes' own builder (`_build_permission_tool_call`,
/// `acp_adapter/permissions.py`) puts the raw command at `rawInput.command`;
/// that structured field is preferred over parsing the human-readable
/// `content` text block (`"$ {command}"`), the same trade
/// `codex::approval::command_text` makes for the parsed action over the raw
/// launcher line.
fn command(tool_call: &Value) -> ApprovalRequest {
    match tool_call["rawInput"]["command"]
        .as_str()
        .filter(|s| !s.is_empty())
    {
        Some(command) => ApprovalRequest::Command {
            command: command.to_owned(),
            cwd: tool_call["rawInput"]["cwd"].as_str().map(str::to_owned),
        },
        None => ApprovalRequest::Unknown {
            summary: "Run a command Comet could not read".to_owned(),
        },
    }
}

/// The option `kind` this decision expresses, or `None` for
/// [`ApprovalDecision::Expired`], which never selects an option — see
/// [`outcome_for`].
fn kind_for(decision: &ApprovalDecision) -> Option<&'static str> {
    match decision {
        ApprovalDecision::Allow => Some(ALLOW_ONCE),
        ApprovalDecision::AllowForSession => Some(ALLOW_ALWAYS),
        ApprovalDecision::Deny { .. } => Some(REJECT_ONCE),
        ApprovalDecision::Expired => None,
    }
}

/// The `session/request_permission` JSON-RPC result for one decision.
///
/// **`Expired` always cancels, and so does a decision whose exact kind the
/// options never offered.** Never widen to a kind that WAS offered but is not
/// the one this decision means: `reject_once` picked for a
/// `reject_always`-only offer would silently turn a one-time denial into a
/// session-wide one — the break `each_decision_selects_the_matching_option_kind`
/// exists to catch. `cancelled` is always a true statement about what
/// happened; a `selected` naming the wrong kind never is.
///
/// No branch here attaches a message to a denial: neither `PermissionOption`
/// nor the outcome ACP defines carries a note field, and Grok's and Hermes'
/// own `carries_deny_note: false` (`grok::capabilities`, `hermes::capabilities`)
/// records that this was checked, not assumed (D24).
pub(crate) fn outcome_for(decision: &ApprovalDecision, options: &[Value]) -> Value {
    let cancelled = json!({"outcome": {"outcome": "cancelled"}});
    let Some(kind) = kind_for(decision) else {
        return cancelled;
    };
    match options
        .iter()
        .find(|option| option["kind"].as_str() == Some(kind))
    {
        Some(option) => json!({
            "outcome": {"outcome": "selected", "optionId": option["optionId"]},
        }),
        None => cancelled,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use comet_proto::ApprovalDecision;
    use serde_json::json;

    /// The real Grok `tool_call_update` shape for a file write outside the
    /// cwd, captured live 2026-08-29 (this module's own doc comment) —
    /// trimmed to the fields `approval_request` reads.
    #[test]
    fn a_diff_content_block_is_read_as_a_file_change() {
        let tool_call = json!({
            "toolCallId": "call-1",
            "kind": "edit",
            "content": [{
                "type": "diff",
                "path": "C:\\out\\outside.txt",
                "oldText": "",
                "newText": "hello",
            }],
        });
        assert_eq!(
            approval_request(&tool_call),
            ApprovalRequest::FileChange {
                path: "C:\\out\\outside.txt".into(),
                operation: FileOperation::Create,
                added_lines: 1,
                removed_lines: 0,
            }
        );
    }

    /// Hermes' `_build_permission_tool_call` shape (source-read,
    /// `acp_adapter/permissions.py`): `kind: "execute"`, the command at
    /// `rawInput.command`, never parsed out of the human-readable text block.
    #[test]
    fn an_execute_call_reads_the_structured_raw_command() {
        let tool_call = json!({
            "toolCallId": "perm-check-1",
            "kind": "execute",
            "content": [{"type": "content", "content": {"type": "text", "text": "$ rm -rf /tmp/x"}}],
            "rawInput": {"command": "rm -rf /tmp/x", "description": "delete a directory"},
        });
        assert_eq!(
            approval_request(&tool_call),
            ApprovalRequest::Command {
                command: "rm -rf /tmp/x".into(),
                cwd: None,
            }
        );
    }

    /// A shape neither evidence source has ever produced -- `read`, `move`,
    /// `search`, `think`, `fetch`, an MCP call -- is `Unknown`, never guessed.
    #[test]
    fn an_unevidenced_kind_is_unknown() {
        let tool_call = json!({"toolCallId": "x", "kind": "fetch", "content": []});
        assert_eq!(
            approval_request(&tool_call),
            ApprovalRequest::Unknown {
                summary: "Take an action Comet could not identify".into()
            }
        );
    }

    #[test]
    fn a_recognized_kind_set_needs_only_one_of_the_four() {
        assert!(has_recognized_kind(&[
            json!({"optionId": "a", "kind": "allow_once", "name": "Allow"}),
        ]));
        assert!(!has_recognized_kind(&[
            json!({"optionId": "a", "kind": "vendor_specific", "name": "Do it"}),
        ]));
        assert!(!has_recognized_kind(&[]));
    }

    /// The break `each_decision_selects_the_matching_option_kind` exists to
    /// catch: `Deny` must select `reject_once`, never `reject_always`, when
    /// both are offered.
    #[test]
    fn deny_selects_reject_once_not_reject_always() {
        let options = [
            json!({"optionId": "allow-1", "kind": "allow_once", "name": "Allow once"}),
            json!({"optionId": "allow-2", "kind": "allow_always", "name": "Allow always"}),
            json!({"optionId": "deny-once", "kind": "reject_once", "name": "Deny"}),
            json!({"optionId": "deny-always", "kind": "reject_always", "name": "Deny always"}),
        ];
        let deny = ApprovalDecision::Deny {
            message: "no".into(),
        };
        assert_eq!(
            outcome_for(&deny, &options),
            json!({"outcome": {"outcome": "selected", "optionId": "deny-once"}})
        );
        assert_eq!(
            outcome_for(&ApprovalDecision::Allow, &options),
            json!({"outcome": {"outcome": "selected", "optionId": "allow-1"}})
        );
        assert_eq!(
            outcome_for(&ApprovalDecision::AllowForSession, &options),
            json!({"outcome": {"outcome": "selected", "optionId": "allow-2"}})
        );
    }

    /// `Expired` is host-stamped and never a decision a client may send; the
    /// agent is answered `cancelled`, not left on a dead channel and not told
    /// something was `selected`.
    #[test]
    fn expired_always_cancels() {
        let options = [json!({"optionId": "a", "kind": "allow_once", "name": "Allow"})];
        assert_eq!(
            outcome_for(&ApprovalDecision::Expired, &options),
            json!({"outcome": {"outcome": "cancelled"}})
        );
    }

    /// A decision whose exact kind is not among the options offered cancels
    /// rather than borrowing a different kind that happens to be present.
    #[test]
    fn a_decision_with_no_matching_kind_cancels_rather_than_substitutes() {
        let only_allow_once = [json!({"optionId": "a", "kind": "allow_once", "name": "Allow"})];
        assert_eq!(
            outcome_for(&ApprovalDecision::AllowForSession, &only_allow_once),
            json!({"outcome": {"outcome": "cancelled"}})
        );
    }
}
