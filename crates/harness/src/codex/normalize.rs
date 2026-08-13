//! Codex app-server notification/item → [`AgentEvent`] mapping, ported from
//! codex.ts's `mapItem`/notification switch.
//!
//! Tolerant by construction: both field spellings the app server has shipped
//! (`delta`/`textDelta`, `exitCode`/`exit_code`, camelCase/snake_case item
//! types) are accepted. Unknown item types no longer map to nothing — an
//! item type inside an otherwise-claimed notification that Comet does not
//! understand becomes an Unknown diagnostic (see `map_item`'s `other` arm),
//! counted and journaled rather than dropped silently.
//!
//! Corpus claim `codex-routine-notification-ignore-list` pins the reviewed
//! healthy-run methods in the ignored tier.

use comet_proto::{
    AgentEvent, ChecklistItem, ChecklistStatus, NoticeKind, NoticeSeverity, ToolCall,
};
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
/// model request (held by the session loop, emitted before `Done`).
///
/// **`last`, never `total`.** The upstream struct documents `total` as
/// cumulative across every turn on the thread; captured live it passed 41% of
/// the window after three trivial turns, so drawing it against the window would
/// report a nearly-full context on a nearly-empty conversation.
///
/// `inputTokens` is already cache-inclusive here — `totalTokens` equals
/// `inputTokens + outputTokens`, so `cachedInputTokens` is a subset of it, not
/// a sibling. That is the opposite of Claude's convention, which is why the
/// two adapters converge on one meaning rather than passing their own through.
///
/// `modelContextWindow` is `Option<i64>` in the upstream protocol (with a
/// standing TODO to stop being one). Absent is "not said": the caller draws
/// nothing rather than assuming a size.
pub(crate) fn usage_event(params: &Value) -> Option<AgentEvent> {
    let usage = field(params, &["tokenUsage", "token_usage"])?;
    let last = usage.get("last")?;
    let count = |keys: &[&str]| {
        field(last, keys)
            .and_then(Value::as_u64)
            .unwrap_or_default()
    };
    Some(AgentEvent::Usage {
        prompt_tokens: count(&["inputTokens", "input_tokens"]),
        output_tokens: count(&["outputTokens", "output_tokens"]),
        context_window: field(usage, &["modelContextWindow", "model_context_window"])
            .and_then(Value::as_u64),
    })
}

/// Codex's plan statuses are already the spelling `ChecklistStatus` uses.
/// Mapped explicitly anyway so an unrecognized value lands on `Unknown` here
/// rather than relying on the proto type's `#[serde(other)]`, which only
/// applies when a value is DESERIALIZED — these arrive as a `&str` read out of
/// an untyped `Value` and never pass through serde at all.
fn checklist_status_from_codex(raw: &str) -> ChecklistStatus {
    match raw {
        "pending" => ChecklistStatus::Pending,
        "inProgress" | "in_progress" => ChecklistStatus::InProgress,
        "completed" => ChecklistStatus::Completed,
        _ => ChecklistStatus::Unknown,
    }
}

/// `turn/plan/updated` → a whole-list [`AgentEvent::ChecklistReplaced`].
///
/// The payload is `{threadId, turnId, explanation, plan: [{step, status}]}`,
/// captured 2026-08-13 against codex-cli 0.147.0. Steps carry no id of their
/// own, so they are indexed positionally — safe here and only here, because a
/// snapshot is never matched against previously stored ids.
///
/// `explanation` is Codex-only prose ("Finished the README read and moved to
/// the line count."). Claude has no equivalent and none is invented for it.
///
/// An absent or empty `plan` yields `None` rather than an empty replacement: a
/// notification that says nothing about the plan must not erase one.
pub(crate) fn plan_update_event(params: &Value) -> Option<AgentEvent> {
    let plan = field(params, &["plan"])?.as_array()?;
    let items: Vec<ChecklistItem> = plan
        .iter()
        .enumerate()
        .map(|(index, entry)| ChecklistItem {
            id: index.to_string(),
            text: Some(str_field(entry, &["step"])),
            active_form: None,
            status: checklist_status_from_codex(&str_field(entry, &["status"])),
        })
        .collect();
    if items.is_empty() {
        return None;
    }
    Some(AgentEvent::ChecklistReplaced {
        explanation: field(params, &["explanation"])
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
        items,
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
                    // `kind` is an OBJECT on the wire — `{"type":"add"}`,
                    // `{"type":"update","move_path":null}` in the generated schema.
                    // Reading it only as a string answered
                    // `None` for every change and fell through to "update", so a
                    // file the agent created rendered as an edit. The bare-string
                    // arm is kept for an older peer; unknown kinds still degrade
                    // to "update", like codex.ts.
                    let kind = c
                        .get("kind")
                        .and_then(|k| k.as_str().or_else(|| k.get("type").and_then(Value::as_str)))
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
        // Never observed on 0.147.0 — every item in the 2026-08-13 capture was
        // `agentMessage`, `commandExecution`, `reasoning` or `userMessage`, and
        // Codex's plan travels as `turn/plan/updated` rather than as an item at
        // all. Kept anyway, on `TodoWrite`'s reasoning: Comet does not pin the
        // user's CLI version, and one run is not proof of absence.
        //
        // BOTH phases emit, deliberately. A replacement is idempotent — the
        // second one restates the same list and the fold lands on the same
        // result — whereas gating on `Started` drops the list entirely for a
        // completion-only item, which is exactly what the app server sends for
        // a `todoList` that was never streamed. Losing a plan is worse than
        // folding it twice.
        "todoList" | "todo_list" => {
            let items: Vec<ChecklistItem> = item
                .get("items")
                .and_then(Value::as_array)
                .map(|a| a.as_slice())
                .unwrap_or_default()
                .iter()
                .enumerate()
                .map(|(index, t)| ChecklistItem {
                    // Positional, like the plan snapshot below: this is a
                    // replacement, so an index is never matched against a
                    // previously stored one.
                    id: index.to_string(),
                    text: Some(str_field(t, &["text"])),
                    active_form: None,
                    // The legacy `completed`/`done` boolean is the only status
                    // this item shape was ever seen to carry, and it cannot
                    // express `inProgress`. Absent means absent, not pending.
                    status: match field(t, &["completed", "done"]).and_then(Value::as_bool) {
                        Some(true) => ChecklistStatus::Completed,
                        Some(false) => ChecklistStatus::Pending,
                        None => ChecklistStatus::Unknown,
                    },
                })
                .collect();
            if items.is_empty() {
                return Vec::new();
            }
            vec![AgentEvent::ChecklistReplaced {
                explanation: None,
                items,
            }]
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
    // `turn/plan/updated` is CLAIMED as of slice 4.3 and is deliberately no
    // longer listed — a frame handled by a claimed arm never reaches
    // `classify_unclaimed`, so an entry here would be a lie the next reader has
    // to disprove.
    //
    // These three stay. None has ever been observed firing, on any capture;
    // they are named by the generated schema alone, and one run is not proof of
    // absence. The reason moves from 4.2 to 4.3 because 4.3 is the slice that
    // owns the plan surface and declined them, rather than the slice that
    // deferred them.
    ("item/plan/delta", "4.3"),
    ("hook/started", "4.3"),
    ("hook/completed", "4.3"),
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

#[cfg(test)]
mod plan_tests {
    use super::*;
    use serde_json::json;

    /// The payload verbatim from the 2026-08-13 capture against codex-cli
    /// 0.147.0 — a complete snapshot per change, with a prose explanation and
    /// all three statuses present at once.
    fn captured_plan() -> Value {
        json!({
            "threadId": "019ff893-0000-7000-8000-000000000000",
            "turnId": "019ff893-1111-7000-8000-000000000000",
            "explanation": "Finished the README read and moved to the line count.",
            "plan": [
                {"step": "Read `README.md`.", "status": "completed"},
                {"step": "Count the lines in `notes.txt`.", "status": "inProgress"},
                {"step": "Report both results.", "status": "pending"},
            ],
        })
    }

    #[test]
    fn a_plan_snapshot_becomes_a_whole_list_replacement() {
        let event = plan_update_event(&captured_plan()).expect("the capture carries a plan");
        assert_eq!(
            event,
            AgentEvent::ChecklistReplaced {
                explanation: Some("Finished the README read and moved to the line count.".into()),
                items: vec![
                    ChecklistItem {
                        id: "0".into(),
                        text: Some("Read `README.md`.".into()),
                        active_form: None,
                        status: ChecklistStatus::Completed,
                    },
                    ChecklistItem {
                        id: "1".into(),
                        text: Some("Count the lines in `notes.txt`.".into()),
                        active_form: None,
                        // The state the whole redesign exists for. `TodoItem`
                        // could not hold it.
                        status: ChecklistStatus::InProgress,
                    },
                    ChecklistItem {
                        id: "2".into(),
                        text: Some("Report both results.".into()),
                        active_form: None,
                        status: ChecklistStatus::Pending,
                    },
                ],
            }
        );
    }

    #[test]
    fn a_notification_carrying_no_plan_erases_nothing() {
        // A replacement built from an absent plan would wipe the list. Saying
        // nothing about the plan is not saying the plan is empty.
        assert!(plan_update_event(&json!({"turnId": "t1"})).is_none());
        assert!(plan_update_event(&json!({"turnId": "t1", "plan": []})).is_none());
    }

    #[test]
    fn an_unrecognized_step_status_is_unknown_rather_than_pending() {
        // A status this build has never heard of must not be reported as "not
        // started" — that reads as a step the agent has yet to begin.
        let event = plan_update_event(&json!({
            "plan": [{"step": "Do the thing.", "status": "blocked"}],
        }))
        .expect("a plan is present");
        let AgentEvent::ChecklistReplaced { items, .. } = event else {
            panic!("expected a replacement");
        };
        assert_eq!(items[0].status, ChecklistStatus::Unknown);
    }

    #[test]
    fn an_absent_explanation_stays_absent() {
        // Claude's paths send none and nothing may invent one, so an empty or
        // missing string must not become `Some("")`.
        let event = plan_update_event(&json!({
            "plan": [{"step": "s", "status": "pending"}],
            "explanation": "",
        }))
        .expect("a plan is present");
        let AgentEvent::ChecklistReplaced { explanation, .. } = event else {
            panic!("expected a replacement");
        };
        assert_eq!(explanation, None);
    }
}

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

    // These fixtures spell `kind` as a bare string, which is NOT what
    // codex-cli 0.147.0 sends — see `a_file_changes_kind_is_an_object_on_the_wire`
    // for the live shape. They are kept deliberately, as the coverage for the
    // fallback arms: an older peer's string, and a change with no `kind` at all.
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
    fn a_file_changes_kind_is_an_object_on_the_wire() {
        // The generated schema sends `{"type":"add"}` / `{"type":"update","move_path":null}`,
        // never a bare string. Reading it as a string
        // answered `None` and fell back to "update", so every file the agent
        // CREATED rendered as an edit, and every delete did too.
        let add = map_item(
            Phase::Started,
            &json!({"type": "fileChange", "id": "f1",
                    "changes": [{"path": "/a.rs", "kind": {"type": "add"}, "diff": "hello\n"}]}),
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
            Phase::Started,
            &json!({"type": "fileChange", "id": "f2",
                    "changes": [{"path": "/b.rs", "kind": {"type": "update", "move_path": null}}]}),
        );
        assert_eq!(
            update,
            vec![AgentEvent::ToolCall {
                id: "f2".into(),
                call: ToolCall::EditFile {
                    path: "/b.rs".into(),
                    old_string: None,
                    new_string: None
                },
            }]
        );
        // A delete is a patch, and it is the arm that silently read as an edit.
        let delete = map_item(
            Phase::Started,
            &json!({"type": "fileChange", "id": "f3",
                    "changes": [{"path": "/c.rs", "kind": {"type": "delete"}}]}),
        );
        assert_eq!(
            delete,
            vec![AgentEvent::ToolCall {
                id: "f3".into(),
                call: ToolCall::ApplyPatch {
                    path: Some("/c.rs".into())
                },
            }]
        );
    }

    #[test]
    fn usage_reads_last_snapshot_under_both_spellings() {
        assert_eq!(
            usage_event(
                &json!({"tokenUsage": {"last": {"inputTokens": 42, "outputTokens": 7}, "modelContextWindow": 258400}})
            ),
            Some(AgentEvent::Usage {
                prompt_tokens: 42,
                output_tokens: 7,
                context_window: Some(258_400),
            })
        );
        assert_eq!(
            usage_event(
                &json!({"token_usage": {"last": {"input_tokens": 1, "output_tokens": 2}, "model_context_window": 400}})
            ),
            Some(AgentEvent::Usage {
                prompt_tokens: 1,
                output_tokens: 2,
                context_window: Some(400),
            })
        );
        assert_eq!(usage_event(&json!({})), None);
    }

    /// The literal frame `codex app-server` sent on 2026-08-12, third turn of
    /// `run4-codex-3turns.jsonl`. `total` is nearly six times `last` here, so a
    /// reader that took the wrong one would draw a gauge six times too full.
    #[test]
    fn usage_takes_last_not_total_from_a_captured_frame() {
        let frame = json!({
            "threadId": "019ff7a7-2b9a-7040-b240-25a5d6d4b040",
            "turnId": "019ff7a7-3b2c-7a11-9c02-1f0b6d0c4e77",
            "tokenUsage": {
                "total": {"totalTokens": 105355, "inputTokens": 103919, "cachedInputTokens": 87968,
                          "cacheWriteInputTokens": 0, "outputTokens": 1436, "reasoningOutputTokens": 1189},
                "last": {"totalTokens": 17316, "inputTokens": 17268, "cachedInputTokens": 16256,
                         "cacheWriteInputTokens": 0, "outputTokens": 48, "reasoningOutputTokens": 41},
                "modelContextWindow": 258400
            }
        });
        assert_eq!(
            usage_event(&frame),
            Some(AgentEvent::Usage {
                prompt_tokens: 17_268,
                output_tokens: 48,
                context_window: Some(258_400),
            })
        );
    }

    /// `modelContextWindow` is `Option<i64>` upstream. Every frame in the
    /// capture carried it, so this absent case only ever runs because a test
    /// constructs it — the exact trap `.agents/rules/optional-wire-fields.md`
    /// describes.
    #[test]
    fn usage_without_a_context_window_reports_none() {
        assert_eq!(
            usage_event(&json!({"tokenUsage": {"last": {"inputTokens": 9, "outputTokens": 1}}})),
            Some(AgentEvent::Usage {
                prompt_tokens: 9,
                output_tokens: 1,
                context_window: None,
            })
        );
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
