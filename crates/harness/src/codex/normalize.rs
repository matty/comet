//! Codex app-server notification/item → [`AgentEvent`] mapping, ported from
//! codex.ts's `mapItem`/notification switch.
//!
//! Tolerant by construction: both field spellings the app server has shipped
//! (`delta`/`textDelta`, `exitCode`/`exit_code`, camelCase/snake_case item
//! types) are accepted. Unknown item types no longer map to nothing — an
//! item type inside an otherwise-claimed notification that Comet does not
//! understand becomes an Unknown diagnostic (see `map_item`'s `other` arm),
//! counted and journaled rather than dropped silently.

use comet_proto::{AgentEvent, NoticeKind, NoticeSeverity, TodoItem, ToolCall};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    Started,
    Completed,
}

fn field<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|k| v.get(*k))
}

fn str_field(v: &Value, keys: &[&str]) -> String {
    field(v, keys)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned()
}

/// Delta text under either spelling the app server has used
/// (`delta` on agentMessage, `textDelta` on some reasoning builds).
pub(crate) fn delta_text(params: &Value) -> Option<String> {
    field(params, &["delta", "textDelta"])
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

pub(crate) fn item_id(params: &Value) -> String {
    str_field(params, &["itemId", "item_id"])
}

/// `params.turn.id` on the turn/* lifecycle notifications.
pub(crate) fn turn_id(params: &Value) -> String {
    params
        .get("turn")
        .map(|t| str_field(t, &["id"]))
        .unwrap_or_default()
}

/// `params.turn.error.message` (turn/completed carries an optional error;
/// turn/failed always should).
pub(crate) fn turn_error_message(params: &Value) -> Option<String> {
    params
        .get("turn")
        .and_then(|t| t.get("error"))
        .filter(|e| !e.is_null())
        .map(|e| {
            let msg = str_field(e, &["message"]);
            if msg.is_empty() { e.to_string() } else { msg }
        })
}

/// `thread/tokenUsage/updated` → a [`AgentEvent::Usage`] snapshot of the LAST
/// turn's tokens (held by the session loop, emitted before `Done`).
pub(crate) fn usage_event(params: &Value) -> Option<AgentEvent> {
    let last = field(params, &["tokenUsage", "token_usage"])?.get("last")?;
    let count = |keys: &[&str]| {
        field(last, keys)
            .and_then(Value::as_u64)
            .unwrap_or_default()
    };
    Some(AgentEvent::Usage {
        input_tokens: count(&["inputTokens", "input_tokens"]),
        output_tokens: count(&["outputTokens", "output_tokens"]),
    })
}

/// Tool-shaped Codex items must always close the lifecycle they open: started
/// opens the ToolCall, completed refreshes its metadata and resolves the same
/// stable id (port of codex.ts `toolLifecycle`).
fn tool_lifecycle(phase: Phase, id: String, call: ToolCall, is_error: bool) -> Vec<AgentEvent> {
    match phase {
        Phase::Started => vec![AgentEvent::ToolCall { id, call }],
        Phase::Completed => vec![
            AgentEvent::ToolCall {
                id: id.clone(),
                call,
            },
            AgentEvent::ToolResult { id, is_error },
        ],
    }
}

/// A `fileChange` item's `changes` array reduced to the typed [`ToolCall`] the
/// UI renders: a lone `add` is a file write, a lone `update` an edit, anything
/// else (deletes, multi-file changes) a patch.
fn file_change_call(changes: &[(String, String)]) -> ToolCall {
    match changes {
        [(path, kind)] if kind == "add" => ToolCall::WriteFile {
            path: path.clone(),
            content: None,
        },
        [(path, kind)] if kind == "update" => ToolCall::EditFile {
            path: path.clone(),
            old_string: None,
            new_string: None,
        },
        [(path, _)] => ToolCall::ApplyPatch {
            path: Some(path.clone()),
        },
        _ => ToolCall::ApplyPatch { path: None },
    }
}

pub(crate) fn item_type(item: &Value) -> &str {
    item.get("type").and_then(Value::as_str).unwrap_or("")
}

/// Map one `item/started` or `item/completed` payload's item to events.
/// `agentMessage` and `reasoning` flow through their delta channels and are
/// handled by the session loop, not here.
pub(crate) fn map_item(phase: Phase, item: &Value) -> Vec<AgentEvent> {
    let id = str_field(item, &["id"]);
    let status = str_field(item, &["status"]);
    match item_type(item) {
        "commandExecution" | "command_execution" => match phase {
            Phase::Started => vec![AgentEvent::ToolCall {
                id,
                call: ToolCall::Exec {
                    command: str_field(item, &["command"]),
                },
            }],
            Phase::Completed => {
                let exit_code = field(item, &["exitCode", "exit_code"])
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                vec![AgentEvent::ToolResult {
                    id,
                    is_error: status == "failed" || exit_code != 0,
                }]
            }
        },
        "fileChange" | "file_change" => {
            let changes: Vec<(String, String)> = item
                .get("changes")
                .and_then(Value::as_array)
                .map(|a| a.as_slice())
                .unwrap_or_default()
                .iter()
                .map(|c| {
                    // Unknown kinds degrade to "update", like codex.ts.
                    let kind = c
                        .get("kind")
                        .and_then(Value::as_str)
                        .filter(|k| matches!(*k, "add" | "delete" | "update"))
                        .unwrap_or("update");
                    (str_field(c, &["path"]), kind.to_owned())
                })
                .collect();
            tool_lifecycle(
                phase,
                id,
                file_change_call(&changes),
                status == "failed" || status == "declined",
            )
        }
        "mcpToolCall" | "mcp_tool_call" => match phase {
            Phase::Started => {
                let input = item.get("arguments").filter(|v| !v.is_null()).cloned();
                vec![AgentEvent::ToolCall {
                    id,
                    call: ToolCall::Mcp {
                        server: str_field(item, &["server"]),
                        tool: str_field(item, &["tool"]),
                        input,
                    },
                }]
            }
            Phase::Completed => vec![AgentEvent::ToolResult {
                id,
                is_error: status == "failed",
            }],
        },
        "webSearch" | "web_search" => tool_lifecycle(
            phase,
            id,
            ToolCall::WebSearch {
                query: str_field(item, &["query"]),
            },
            false,
        ),
        "todoList" | "todo_list" => {
            let items = item
                .get("items")
                .and_then(Value::as_array)
                .map(|a| a.as_slice())
                .unwrap_or_default()
                .iter()
                .map(|t| TodoItem {
                    text: str_field(t, &["text"]),
                    done: field(t, &["completed", "done"]).and_then(Value::as_bool) == Some(true),
                })
                .collect();
            tool_lifecycle(phase, id, ToolCall::Todo { items }, false)
        }
        "error" => vec![AgentEvent::Error {
            message: str_field(item, &["message"]),
        }],
        // userMessage / reasoning flow through delta channels (agentMessage
        // is routed by the session loop before map_item is reached, but both
        // spellings are named here so a refactor can't misread them as
        // unknown).
        "userMessage" | "user_message" | "reasoning" | "agentMessage" | "agent_message" => {
            Vec::new()
        }
        other => {
            // Sink 4: an item type inside a CLAIMED notification that Comet
            // does not understand. `item/<type>` keeps it distinguishable
            // from a method. Fires on started AND completed, so one unknown
            // item counts twice — a frequency signal, not an item count.
            tracing::warn!(
                target: "comet_harness::codex",
                item = %item,
                "unrecognized item type (recorded as a diagnostic)"
            );
            let discriminator = if other.is_empty() {
                "item/untyped".to_string()
            } else {
                format!("item/{other}")
            };
            vec![crate::diagnostic(
                &discriminator,
                comet_proto::DiagnosticSeverity::Unknown,
            )]
        }
    }
}

/// Comet copy for a failed `mcpServer/startupStatus/updated`, derived from the
/// app server's STRUCTURED `failureReason` — never from its `error` string.
///
/// `error` is a raw technical message ("connect ECONNREFUSED 127.0.0.1:3845")
/// and `.agents/rules/user-facing-errors.md` rule 1 is unconditional: the user
/// never sees one. It reaches a hover tooltip, the persisted doc and LAN
/// replay, so it is debug-logged here and dropped. `failureReason` is the
/// schema's own enum (`McpServerStartupFailureReason`); its only variant today
/// is `reauthenticationRequired`, which IS actionable. An unrecognized or
/// absent reason answers `None` — the summary already names the server, and no
/// detail beats invented detail.
fn startup_failure_detail(name: &str, params: &Value, error: &str) -> Option<String> {
    let reason = str_field(params, &["failureReason", "failure_reason"]);
    if !error.is_empty() || !reason.is_empty() {
        tracing::debug!(
            target: "comet_harness::codex",
            "mcp server {name} failed to start (reason={reason:?}, provider error): {error}"
        );
    }
    match reason.as_str() {
        "reauthenticationRequired" => Some("Sign in to this server again to reconnect it.".into()),
        _ => None,
    }
}

/// Stateless notification → notice mapping for the three claimed Codex
/// methods that need no per-session state. `account/rateLimits/updated` goes
/// through [`rate_limit_notice`] instead — it fires continuously and must be
/// threshold-filtered. Unclaimed methods answer `None` and stay with the run
/// loop's tolerated catch-all (slice 0b.2 reads from there).
pub(crate) fn notice_for(method: &str, params: &Value) -> Option<AgentEvent> {
    match method {
        "mcpServer/startupStatus/updated" => {
            let name = str_field(params, &["name"]);
            let status = str_field(params, &["status"]);
            let error = str_field(params, &["error"]);
            match status.as_str() {
                "failed" => Some(AgentEvent::Notice {
                    kind: NoticeKind::McpStatus,
                    severity: NoticeSeverity::Warning,
                    summary: format!("MCP server {name} failed to start"),
                    detail: startup_failure_detail(&name, params, &error),
                    key: Some(format!("mcp:{name}")),
                }),
                "ready" => Some(AgentEvent::Notice {
                    kind: NoticeKind::McpStatus,
                    severity: NoticeSeverity::Info,
                    summary: format!("MCP server {name} is ready"),
                    detail: None,
                    key: Some(format!("mcp:{name}")),
                }),
                // "starting"/"cancelled" are transient churn a user can't act
                // on; the terminal states above are the message.
                _ => None,
            }
        }
        "mcpServer/oauthLogin/completed" => {
            let name = str_field(params, &["name"]);
            let success = params
                .get("success")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let error = str_field(params, &["error"]);
            if !error.is_empty() {
                tracing::debug!(
                    target: "comet_harness::codex",
                    "mcp oauth login failed for {name} (provider error): {error}"
                );
            }
            Some(AgentEvent::Notice {
                kind: NoticeKind::AuthStatus,
                severity: NoticeSeverity::Info,
                summary: if success {
                    format!("Signed in to MCP server {name}")
                } else {
                    format!("Sign-in to MCP server {name} didn't finish")
                },
                // The app server's `error` is a raw transport/OAuth string
                // ("connect ECONNREFUSED …") — it stays in tracing, never in
                // the doc. There is no structured reason field on this
                // notification, so the summary carries the whole message.
                detail: None,
                key: Some(format!("mcp:{name}")),
            })
        }
        "thread/environment/disconnected" => Some(AgentEvent::Notice {
            kind: NoticeKind::McpStatus,
            severity: NoticeSeverity::Warning,
            summary: "Remote environment disconnected".into(),
            detail: None,
            key: Some("environment".into()),
        }),
        _ => None,
    }
}

/// Per-run threshold latch for `account/rateLimits/updated`. Notices fire
/// only on the FIRST crossing of 80% and of 95% — the same thresholds the
/// accounts pane's usage meter recolors at
/// (`crates/ui/src/settings/accounts.rs`, indigo → amber ≥80 → red ≥95).
#[derive(Debug, Default)]
pub(crate) struct RateLimitThresholds {
    crossed_80: bool,
    crossed_95: bool,
}

pub(crate) fn rate_limit_notice(
    params: &Value,
    state: &mut RateLimitThresholds,
) -> Option<AgentEvent> {
    let limits = params.get("rateLimits")?;
    // Sparse rolling update: the fullest window drives the crossing.
    let used = ["primary", "secondary"]
        .iter()
        .filter_map(|w| limits.get(*w))
        .filter_map(|w| w.get("usedPercent"))
        .filter_map(Value::as_i64)
        .max()?;
    let fire = if used >= 95 && !state.crossed_95 {
        state.crossed_95 = true;
        state.crossed_80 = true;
        true
    } else if used >= 80 && !state.crossed_80 {
        state.crossed_80 = true;
        true
    } else {
        false
    };
    fire.then(|| AgentEvent::Notice {
        kind: NoticeKind::RateLimit,
        severity: NoticeSeverity::Warning,
        summary: format!("Codex usage is at {used}% of its limit"),
        detail: None,
        key: Some("rateLimit".into()),
    })
}

/// Notification methods Comet recognizes and deliberately drops — the middle
/// tier of the Claimed / Ignored / Unknown classification. Reasons: a slice
/// number (e.g. `"4.2"`, `"2.4"`, `"phase-1"`) names a roadmap slice that
/// will later claim the entry and move it out of this table; it is a
/// maintenance obligation, not a fact about the notification, and reading
/// only this repository will not resolve which slice that is; any other
/// reason names why no surface wants it. ★ = confirmed firing on a real codex-cli 0.147.0 capture
/// (2026-08-08); the rest are named by the generated schema (70 methods).
/// The hook/* and item/autoApprovalReview/* families are exactly the two
/// members each — the schema has no others, so literal strings, no globs.
/// Deliberately NOT here (notice-material or genuinely unused, so a
/// diagnostic is the honest signal): warning, guardianWarning,
/// deprecationNotice, configWarning, model/rerouted, model/verification,
/// turn/moderationMetadata, the thread archive/delete/goal/realtime families,
/// fs/changed, windowsSandbox/setupCompleted, windows/worldWritableWarning
/// (neither fired even when the Windows sandbox failed).
pub(crate) const IGNORED_NOTIFICATIONS: &[(&str, &str)] = &[
    // owned by a later slice
    ("skills/changed", "2.4"),
    ("turn/plan/updated", "4.2"),
    ("item/plan/delta", "4.2"),
    ("hook/started", "4.2"),
    ("hook/completed", "4.2"),
    ("item/autoApprovalReview/started", "phase-1"),
    ("item/autoApprovalReview/completed", "phase-1"),
    // no user-relevant state
    ("account/updated", "transient"),
    ("thread/status/changed", "transient"), // ★ 2 per session
    ("thread/environment/connected", "baseline"),
    ("thread/started", "redundant"),              // ★ 1 per session
    ("thread/settings/updated", "settings-echo"), // ★ 1 per session
    ("remoteControl/status/changed", "remote-status"), // ★ 1 per session
    ("thread/name/updated", "redundant"),
    ("serverRequest/resolved", "bookkeeping"),
    ("process/exited", "bookkeeping"),
    // high-volume streams comet does not render yet
    ("item/commandExecution/outputDelta", "unrendered"), // ★
    ("command/exec/outputDelta", "unrendered"),
    ("process/outputDelta", "unrendered"),
    ("item/fileChange/patchUpdated", "unrendered"),
    ("turn/diff/updated", "unrendered"),
    ("item/mcpToolCall/progress", "progress"),
    ("item/reasoning/summaryPartAdded", "boundary"), // ★
    // schema-deprecated
    ("item/fileChange/outputDelta", "deprecated"),
    ("thread/compacted", "deprecated"),
];

pub(crate) fn ignored_notification_reason(method: &str) -> Option<&'static str> {
    IGNORED_NOTIFICATIONS
        .iter()
        .find(|(name, _)| *name == method)
        .map(|(_, reason)| *reason)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn delta_accepts_both_spellings() {
        assert_eq!(delta_text(&json!({"delta": "a"})), Some("a".into()));
        assert_eq!(delta_text(&json!({"textDelta": "b"})), Some("b".into()));
        assert_eq!(delta_text(&json!({"delta": ""})), None);
        assert_eq!(delta_text(&json!({})), None);
    }

    #[test]
    fn command_execution_maps_exit_code_to_error() {
        let started = map_item(
            Phase::Started,
            &json!({"type": "commandExecution", "id": "c1", "command": "ls"}),
        );
        assert_eq!(
            started,
            vec![AgentEvent::ToolCall {
                id: "c1".into(),
                call: ToolCall::Exec {
                    command: "ls".into()
                },
            }]
        );
        let completed = map_item(
            Phase::Completed,
            &json!({"type": "command_execution", "id": "c1", "status": "completed", "exit_code": 2}),
        );
        assert_eq!(
            completed,
            vec![AgentEvent::ToolResult {
                id: "c1".into(),
                is_error: true,
            }]
        );
    }

    #[test]
    fn file_change_variants_map_to_typed_calls() {
        let add = map_item(
            Phase::Started,
            &json!({"type": "fileChange", "id": "f1", "changes": [{"path": "/a.rs", "kind": "add"}]}),
        );
        assert_eq!(
            add,
            vec![AgentEvent::ToolCall {
                id: "f1".into(),
                call: ToolCall::WriteFile {
                    path: "/a.rs".into(),
                    content: None
                },
            }]
        );
        let update = map_item(
            Phase::Completed,
            &json!({"type": "fileChange", "id": "f2", "status": "declined",
                    "changes": [{"path": "/b.rs", "kind": "update"}]}),
        );
        assert_eq!(
            update,
            vec![
                AgentEvent::ToolCall {
                    id: "f2".into(),
                    call: ToolCall::EditFile {
                        path: "/b.rs".into(),
                        old_string: None,
                        new_string: None
                    },
                },
                AgentEvent::ToolResult {
                    id: "f2".into(),
                    is_error: true
                },
            ]
        );
        let multi = map_item(
            Phase::Started,
            &json!({"type": "fileChange", "id": "f3",
                    "changes": [{"path": "/a"}, {"path": "/b", "kind": "delete"}]}),
        );
        assert_eq!(
            multi,
            vec![AgentEvent::ToolCall {
                id: "f3".into(),
                call: ToolCall::ApplyPatch { path: None },
            }]
        );
    }

    #[test]
    fn usage_reads_last_snapshot_under_both_spellings() {
        assert_eq!(
            usage_event(&json!({"tokenUsage": {"last": {"inputTokens": 42, "outputTokens": 7}}})),
            Some(AgentEvent::Usage {
                input_tokens: 42,
                output_tokens: 7
            })
        );
        assert_eq!(
            usage_event(&json!({"token_usage": {"last": {"input_tokens": 1, "output_tokens": 2}}})),
            Some(AgentEvent::Usage {
                input_tokens: 1,
                output_tokens: 2
            })
        );
        assert_eq!(usage_event(&json!({})), None);
    }

    #[test]
    fn turn_error_extraction() {
        assert_eq!(
            turn_error_message(&json!({"turn": {"id": "t", "error": {"message": "boom"}}})),
            Some("boom".into())
        );
        assert_eq!(turn_error_message(&json!({"turn": {"id": "t"}})), None);
        assert_eq!(turn_error_message(&json!({"turn": {"error": null}})), None);
    }

    #[test]
    fn mcp_startup_status_maps_terminal_states_only() {
        use comet_proto::{NoticeKind, NoticeSeverity};
        // The app server's raw `error` NEVER becomes user-facing detail
        // (user-facing-errors rule 1) — it is debug-logged and dropped.
        assert_eq!(
            notice_for(
                "mcpServer/startupStatus/updated",
                &json!({"name": "linear", "status": "failed",
                        "error": "connect ECONNREFUSED 127.0.0.1:3845"}),
            ),
            Some(AgentEvent::Notice {
                kind: NoticeKind::McpStatus,
                severity: NoticeSeverity::Warning,
                summary: "MCP server linear failed to start".into(),
                detail: None,
                key: Some("mcp:linear".into()),
            })
        );
        // The structured `failureReason` IS actionable — Comet's own copy for
        // the schema's only variant, with the raw error still dropped.
        assert_eq!(
            notice_for(
                "mcpServer/startupStatus/updated",
                &json!({"name": "linear", "status": "failed",
                        "failureReason": "reauthenticationRequired",
                        "error": "401 Unauthorized"}),
            ),
            Some(AgentEvent::Notice {
                kind: NoticeKind::McpStatus,
                severity: NoticeSeverity::Warning,
                summary: "MCP server linear failed to start".into(),
                detail: Some("Sign in to this server again to reconnect it.".into()),
                key: Some("mcp:linear".into()),
            })
        );
        // A reason this build has never heard of falls back to no detail
        // rather than echoing the wire value at the user.
        assert_eq!(
            notice_for(
                "mcpServer/startupStatus/updated",
                &json!({"name": "linear", "status": "failed", "failureReason": "somethingNew"}),
            ),
            Some(AgentEvent::Notice {
                kind: NoticeKind::McpStatus,
                severity: NoticeSeverity::Warning,
                summary: "MCP server linear failed to start".into(),
                detail: None,
                key: Some("mcp:linear".into()),
            })
        );
        assert_eq!(
            notice_for(
                "mcpServer/startupStatus/updated",
                &json!({"name": "linear", "status": "ready"}),
            ),
            Some(AgentEvent::Notice {
                kind: NoticeKind::McpStatus,
                severity: NoticeSeverity::Info,
                summary: "MCP server linear is ready".into(),
                detail: None,
                key: Some("mcp:linear".into()),
            })
        );
        // Transient churn a user can't act on.
        assert_eq!(
            notice_for(
                "mcpServer/startupStatus/updated",
                &json!({"name": "linear", "status": "starting"}),
            ),
            None
        );
    }

    #[test]
    fn oauth_completed_and_environment_disconnected_map_to_notices() {
        use comet_proto::{NoticeKind, NoticeSeverity};
        assert_eq!(
            notice_for(
                "mcpServer/oauthLogin/completed",
                &json!({"name": "linear", "success": true}),
            ),
            Some(AgentEvent::Notice {
                kind: NoticeKind::AuthStatus,
                severity: NoticeSeverity::Info,
                summary: "Signed in to MCP server linear".into(),
                detail: None,
                key: Some("mcp:linear".into()),
            })
        );
        assert_eq!(
            notice_for(
                "thread/environment/disconnected",
                &json!({"environmentId": "env-1", "threadId": "th-1"}),
            ),
            Some(AgentEvent::Notice {
                kind: NoticeKind::McpStatus,
                severity: NoticeSeverity::Warning,
                summary: "Remote environment disconnected".into(),
                detail: None,
                key: Some("environment".into()),
            })
        );
        // A failed sign-in keeps the raw provider error out of the doc too.
        assert_eq!(
            notice_for(
                "mcpServer/oauthLogin/completed",
                &json!({"name": "linear", "success": false,
                        "error": "connect ECONNREFUSED 127.0.0.1:3845"}),
            ),
            Some(AgentEvent::Notice {
                kind: NoticeKind::AuthStatus,
                severity: NoticeSeverity::Info,
                summary: "Sign-in to MCP server linear didn't finish".into(),
                detail: None,
                key: Some("mcp:linear".into()),
            })
        );
        // Unclaimed methods answer None (they stay with the tolerated arm).
        assert_eq!(notice_for("thread/status/changed", &json!({})), None);
    }

    /// `account/rateLimits/updated` fires continuously; only the FIRST
    /// crossing of 80% and the FIRST crossing of 95% notice. Collapse must
    /// not be doing a filter's job.
    #[test]
    fn rate_limit_notices_fire_on_threshold_crossings_only() {
        let mut state = RateLimitThresholds::default();
        let update = |pct: i64| json!({"rateLimits": {"primary": {"usedPercent": pct}}});

        assert_eq!(rate_limit_notice(&update(50), &mut state), None);
        let first = rate_limit_notice(&update(85), &mut state).expect("80% crossing fires");
        match &first {
            AgentEvent::Notice { summary, .. } => {
                assert_eq!(summary, "Codex usage is at 85% of its limit")
            }
            other => panic!("unexpected {other:?}"),
        }
        // Still inside the 80 band: quiet.
        assert_eq!(rate_limit_notice(&update(90), &mut state), None);
        let second = rate_limit_notice(&update(97), &mut state).expect("95% crossing fires");
        match &second {
            AgentEvent::Notice { summary, .. } => {
                assert_eq!(summary, "Codex usage is at 97% of its limit")
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(rate_limit_notice(&update(99), &mut state), None);

        // A run that starts already over 95 fires exactly once.
        let mut fresh = RateLimitThresholds::default();
        assert!(rate_limit_notice(&update(96), &mut fresh).is_some());
        assert_eq!(rate_limit_notice(&update(97), &mut fresh), None);

        // The larger of primary/secondary drives the crossing.
        let mut both = RateLimitThresholds::default();
        let two = json!({"rateLimits": {"primary": {"usedPercent": 10}, "secondary": {"usedPercent": 82}}});
        assert!(rate_limit_notice(&two, &mut both).is_some());

        // A sparse update with no windows is quiet, not a panic.
        assert_eq!(
            rate_limit_notice(
                &json!({"rateLimits": {}}),
                &mut RateLimitThresholds::default()
            ),
            None
        );
    }

    #[test]
    fn the_notification_ignore_list_names_the_capture_confirmed_methods() {
        // codex-cli 0.147.0 capture (2026-08-08): these fire on a healthy
        // session; one diagnostic from any of them lies on the settings card.
        for method in [
            "thread/started",
            "thread/settings/updated",
            "remoteControl/status/changed",
            "thread/status/changed",
            "item/reasoning/summaryPartAdded",
            "item/commandExecution/outputDelta",
        ] {
            assert!(
                ignored_notification_reason(method).is_some(),
                "{method} fires on a healthy session and must be Ignored"
            );
        }
        // Notice-material or genuinely unused methods stay Unknown on
        // purpose — a diagnostic is the honest signal until a slice claims
        // them.
        for method in [
            "deprecationNotice",
            "model/rerouted",
            "warning",
            "guardianWarning",
        ] {
            assert!(ignored_notification_reason(method).is_none(), "{method}");
        }
    }

    #[test]
    fn unknown_item_types_map_to_an_item_diagnostic() {
        use comet_proto::DiagnosticSeverity;
        // Sink 4: decoded fine, item type not understood → Unknown (never
        // Malformed — that stays reserved for parse failures).
        let events = map_item(
            Phase::Started,
            &json!({"type": "contextCompaction", "id": "cc1", "secret": "do-not-carry"}),
        );
        assert_eq!(
            events,
            vec![AgentEvent::Diagnostic {
                discriminator: "item/contextCompaction".into(),
                severity: DiagnosticSeverity::Unknown,
                code: None,
                summary: "The agent sent a message Comet doesn't recognize.".into(),
            }]
        );
        // The delta-channel types are claimed elsewhere, not unknown.
        assert!(map_item(Phase::Started, &json!({"type": "reasoning", "id": "r1"})).is_empty());
        assert!(
            map_item(
                Phase::Completed,
                &json!({"type": "userMessage", "id": "u1"})
            )
            .is_empty()
        );
        // An untyped item still gets a stable, sanitizer-safe name.
        let events = map_item(Phase::Started, &json!({"id": "x"}));
        assert!(matches!(
            &events[0],
            AgentEvent::Diagnostic { discriminator, .. } if discriminator == "item/untyped"
        ));
    }
}
