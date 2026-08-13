//! Frame → [`AgentEvent`] normalization, ported from claude.ts's `normalize`
//! (init dedupe, subagent filtering, tool decoding, error-code mapping).

use std::collections::HashMap;

use comet_proto::{
    AgentEvent, DiagnosticSeverity, DoneStatus, HarnessId, NoticeKind, NoticeSeverity, RuntimeMode,
    SubagentStatus, TodoItem, ToolCall,
};
use serde_json::Value;

use super::wire::{ContentBlock, Frame, SubagentTaskFrame, SystemNoticeFrame};

/// Human-readable text for the CLI's assistant-level error codes. These arrive
/// as a terse `error` field on an `assistant` frame — usually with NO text
/// content and NOT as a `result` error — so a usage-limited or otherwise failed
/// turn looks like the agent simply never replied unless we surface it.
fn assistant_error_text(code: &str) -> String {
    match code {
        "authentication_failed" => "Authentication failed — sign in to Claude again.".into(),
        "oauth_org_not_allowed" => "This organization isn't allowed to use Claude here.".into(),
        "billing_error" => "Billing error — check your Claude plan or payment method.".into(),
        "rate_limit" => "Claude usage limit reached — try again after the limit resets.".into(),
        "overloaded" => "Claude is overloaded right now — try again shortly.".into(),
        "invalid_request" => "The request was rejected as invalid.".into(),
        "model_not_found" => "The selected model isn't available.".into(),
        "server_error" => "Claude had a server error — try again.".into(),
        "max_output_tokens" => "The reply hit the maximum output length.".into(),
        "unknown" => "Claude returned an unspecified error.".into(),
        other => format!("Claude error: {other}"),
    }
}

/// A `result` frame's usage → a context-occupancy reading, or `None` when the
/// turn made no model request at all.
///
/// **The prompt size is the sum of the three input figures**, not
/// `input_tokens`: that field is the cache-exclusive remainder, and Anthropic's
/// prompt-caching documentation states the prompt is
/// `input + cache_creation + cache_read`. Captured live, a turn whose real
/// prompt was ~35,000 tokens reported `input_tokens: 10`.
///
/// **A zero prompt means no request happened** — a slash command such as
/// `/context` or `/compact` runs locally and reports `0/0/0`. Emitting that as
/// a reading would paint an empty gauge over a session with 35k in it, so the
/// turn produces no event and the last real reading stands.
fn usage_event(
    usage: &super::wire::UsageBody,
    model_usage: &std::collections::BTreeMap<String, super::wire::ModelUsageBody>,
) -> Option<AgentEvent> {
    let prompt_tokens = usage
        .input_tokens
        .saturating_add(usage.cache_read_input_tokens)
        .saturating_add(usage.cache_creation_input_tokens);
    if prompt_tokens == 0 {
        return None;
    }
    Some(AgentEvent::Usage {
        prompt_tokens,
        output_tokens: usage.output_tokens,
        context_window: agreed_context_window(model_usage),
    })
}

/// The window every model in the breakdown agrees on, or `None`.
///
/// `modelUsage` is keyed by resolved model id and a turn that ran subagents
/// carries several. Picking one of two disagreeing windows would draw a gauge
/// against a limit that isn't the one being consumed, so this declines instead
/// — the same rule 3.2's `compare_versions` follows, and for the same reason:
/// both guesses are wrong in a way the user acts on.
fn agreed_context_window(
    model_usage: &std::collections::BTreeMap<String, super::wire::ModelUsageBody>,
) -> Option<u64> {
    // Every entry, not every entry that published one: filtering the silent
    // models out would answer with the vocal model's window and draw the
    // conversation against a limit only part of it is consuming.
    let mut windows = model_usage.values().map(|m| m.context_window);
    let first = windows.next()??;
    windows.all(|w| w == Some(first)).then_some(first)
}

/// Which claude.ai usage window a `rate_limit_event` refers to.
fn rate_window_label(kind: &str) -> &'static str {
    match kind {
        "five_hour" => "5-hour",
        "seven_day" | "seven_day_overage_included" => "weekly",
        "seven_day_opus" => "weekly (Opus)",
        "seven_day_sonnet" => "weekly (Sonnet)",
        "overage" => "overage",
        _ => "usage",
    }
}

/// Fallback wording for a `result` error whose `errors` array is empty, so the
/// turn never ends with a blank (and therefore invisible) error.
fn result_error_text(subtype: &str) -> &'static str {
    match subtype {
        "error_max_turns" => "The run hit the maximum number of turns.",
        "error_max_budget_usd" => "The run hit its cost budget.",
        "error_max_structured_output_retries" => "The run exhausted its structured-output retries.",
        _ => "The run ended with an error.",
    }
}

/// The CLI seeds `result.errors` with internal `[ede_diagnostic]` breadcrumbs
/// for its error_during_execution telemetry ("turn aborted (…) stop_reason=…",
/// "result_type=… last_content_type=… stop_reason=…"). They're diagnostics
/// about the CLI's own turn accounting, not user-relevant errors — surfacing
/// them verbatim put raw `[ede_diagnostic] result_type=user …` boxes in the
/// transcript. They're debug-logged and dropped instead.
fn is_internal_diagnostic(message: &str) -> bool {
    message.contains("[ede_diagnostic]")
}

fn str_field(input: &Value, key: &str) -> String {
    input.get(key).and_then(Value::as_str).unwrap_or("").into()
}

fn opt_str_field(input: &Value, key: &str) -> Option<String> {
    input.get(key).and_then(Value::as_str).map(str::to_owned)
}

/// Claude's `status` string on `task_notification` / `task_updated.patch`, to
/// [`SubagentStatus`]. The capture never observed anything but `"completed"`
/// — `Failed` and `Cancelled` are written by hand per
/// `.agents/rules/optional-wire-fields.md`. Anything unrecognized (including
/// a genuinely in-progress subagent, which reports no `status` field at all)
/// degrades to `Running` rather than panicking: an unknown status is not
/// evidence the subagent stopped.
fn subagent_status(raw: &str) -> SubagentStatus {
    match raw {
        "completed" => SubagentStatus::Completed,
        "failed" => SubagentStatus::Failed,
        "cancelled" => SubagentStatus::Cancelled,
        _ => SubagentStatus::Running,
    }
}

/// The material fields of one `SubagentUpdated` reading, compared per
/// `task_id` to dedupe repeated `task_progress` ticks whose content did not
/// move. Deliberately NOT a merged/accumulated state: each frame's own
/// reported fields are compared as-is against the last-EMITTED reading, so a
/// `task_updated` (status only) naturally differs from a preceding
/// `task_progress` (status + activity + usage) even though it reports fewer
/// fields — that is a real transition, not filter noise. This is a dedupe at
/// the normalize boundary, not a state machine: it never looks further back
/// than the immediately preceding emission for the same `task_id`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SubagentSnapshot {
    status: SubagentStatus,
    activity: Option<String>,
    summary: Option<String>,
    total_tokens: Option<u64>,
    tool_uses: Option<u32>,
    duration_ms: Option<u64>,
}

/// Sniff the `Agent` tool's own `tool_use_result` for a subagent record, keyed
/// by `agentId` (== `task_id`). Shape-based rather than name-tracked: `agentId`
/// alongside `status` is specific to an `Agent` result, so an ordinary tool's
/// result (`Bash`'s `stdout`, `Write`'s diff, …) never matches. Returns `None`
/// for anything that isn't one.
fn subagent_result_from_tool_use_result(value: &Value) -> Option<(String, SubagentSnapshot)> {
    let task_id = value.get("agentId").and_then(Value::as_str)?.to_owned();
    let status = value
        .get("status")
        .and_then(Value::as_str)
        .map(subagent_status)
        .unwrap_or(SubagentStatus::Running);
    let summary = value
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .filter(|s| !s.is_empty());
    let total_tokens = value.get("totalTokens").and_then(Value::as_u64);
    let duration_ms = value.get("totalDurationMs").and_then(Value::as_u64);
    let tool_uses = value
        .get("totalToolUseCount")
        .and_then(Value::as_u64)
        .map(|n| n as u32);
    Some((
        task_id,
        SubagentSnapshot {
            status,
            activity: None,
            summary,
            total_tokens,
            tool_uses,
            duration_ms,
        },
    ))
}

/// Decode a Claude `tool_use` block (name + input) into a typed [`ToolCall`].
pub(crate) fn decode_tool_use(name: &str, input: &Value) -> ToolCall {
    match name {
        "Bash" => ToolCall::Exec {
            command: str_field(input, "command"),
        },
        "Read" => ToolCall::ReadFile {
            path: str_field(input, "file_path"),
        },
        "Write" => ToolCall::WriteFile {
            path: str_field(input, "file_path"),
            content: opt_str_field(input, "content"),
        },
        "Edit" => ToolCall::EditFile {
            path: str_field(input, "file_path"),
            old_string: opt_str_field(input, "old_string"),
            new_string: opt_str_field(input, "new_string"),
        },
        "Grep" => ToolCall::Search {
            pattern: str_field(input, "pattern"),
            path: opt_str_field(input, "path"),
        },
        "Glob" => ToolCall::Glob {
            pattern: str_field(input, "pattern"),
        },
        "WebFetch" => ToolCall::WebFetch {
            url: str_field(input, "url"),
            prompt: opt_str_field(input, "prompt"),
        },
        "WebSearch" => ToolCall::WebSearch {
            query: str_field(input, "query"),
        },
        "TodoWrite" => ToolCall::Todo {
            items: input
                .get("todos")
                .and_then(Value::as_array)
                .map(|a| a.as_slice())
                .unwrap_or_default()
                .iter()
                .map(|t| TodoItem {
                    text: str_field(t, "content"),
                    done: t.get("status").and_then(Value::as_str) == Some("completed"),
                })
                .collect(),
        },
        // MCP tools arrive as `mcp__<server>__<tool>`.
        _ => match name.strip_prefix("mcp__").and_then(|r| r.split_once("__")) {
            Some((server, tool)) => ToolCall::Mcp {
                server: server.into(),
                tool: tool.into(),
                input: (!input.is_null()).then(|| input.clone()),
            },
            None => ToolCall::Unknown {
                name: name.into(),
                input: (!input.is_null()).then(|| input.clone()),
            },
        },
    }
}

fn new_message_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Map one allowlisted `system` notice frame to its notice event. Structured
/// kinds carry Comet copy; the passthrough kinds (informational,
/// notification) carry capped provider prose and are claimed by the
/// passthrough emitters. The structured kinds use a per-kind constant
/// collapse key.
fn notice_events(f: &SystemNoticeFrame) -> Vec<AgentEvent> {
    let notice = match f.subtype.as_str() {
        "compact_boundary" => {
            let m = &f.compact_metadata;
            let summary = if m.trigger == "manual" {
                "Context compacted".to_string()
            } else {
                "Context compacted automatically".to_string()
            };
            let detail = match m.post_tokens {
                Some(post) => format!("{} tokens → {}", m.pre_tokens, post),
                None => format!("{} tokens before compaction", m.pre_tokens),
            };
            Some(AgentEvent::Notice {
                kind: NoticeKind::Compaction,
                severity: NoticeSeverity::Info,
                summary,
                detail: Some(detail),
                key: Some("compaction".into()),
            })
        }
        "model_refusal_fallback" => Some(AgentEvent::Notice {
            kind: NoticeKind::ModelRerouted,
            severity: NoticeSeverity::Warning,
            summary: format!("Model changed to {}", f.fallback_model),
            detail: Some(format!(
                "{} refused the request; replies now come from {}.",
                f.original_model, f.fallback_model
            )),
            key: Some("model".into()),
        }),
        "api_retry" => Some(AgentEvent::Notice {
            kind: NoticeKind::Retrying,
            severity: NoticeSeverity::Warning,
            summary: format!("Retrying — attempt {} of {}", f.attempt, f.max_retries),
            detail: Some(format!(
                "Next attempt in {}s.",
                f.retry_delay_ms.div_ceil(1000)
            )),
            key: Some("retry".into()),
        }),
        "informational" => {
            tracing::debug!(
                target: "comet_harness::claude",
                "informational (full text): {}", f.content
            );
            // sdk.d.ts documents `level` as the CLI's own render level —
            // read it rather than inventing a severity.
            let severity = match f.level.as_str() {
                "suggestion" | "warning" => NoticeSeverity::Warning,
                _ => NoticeSeverity::Info,
            };
            Some(AgentEvent::Notice {
                kind: NoticeKind::Info,
                severity,
                summary: crate::cap_prose(&f.content, crate::NOTICE_SUMMARY_MAX),
                detail: (f.content.len() > crate::NOTICE_SUMMARY_MAX)
                    .then(|| crate::cap_prose(&f.content, crate::NOTICE_DETAIL_MAX)),
                key: f.tool_use_id.clone(),
            })
        }
        "notification" => {
            tracing::debug!(
                target: "comet_harness::claude",
                "notification (full text): {}", f.text
            );
            let severity = match f.priority.as_str() {
                "high" | "immediate" => NoticeSeverity::Warning,
                _ => NoticeSeverity::Info,
            };
            Some(AgentEvent::Notice {
                kind: NoticeKind::Info,
                severity,
                summary: crate::cap_prose(&f.text, crate::NOTICE_SUMMARY_MAX),
                detail: (f.text.len() > crate::NOTICE_SUMMARY_MAX)
                    .then(|| crate::cap_prose(&f.text, crate::NOTICE_DETAIL_MAX)),
                key: f.key.clone(),
            })
        }
        _ => None,
    };
    notice.into_iter().collect()
}

/// Per-run normalization state.
///
/// `saw_init` dedupes `system:init` — the CLI re-emits it every time the model
/// is re-invoked WITHIN one session (a background-task notification, a
/// scheduled wakeup), not just at start. Downstream, `SessionStarted` is the
/// fold's run boundary (it resets accumulated parts), so one run ⇒ one
/// `SessionStarted`.
pub(crate) struct Normalizer {
    saw_init: bool,
    /// Rotates at each assistant-frame close and at each steer; SessionStarted
    /// carries the first value so folds can attribute deltas from the start.
    assistant_message_id: String,
    /// Last session id seen (init or result) — used for synthetic Dones.
    pub session_id: Option<String>,
    /// Echoed into `SessionStarted` so the journal records what the run was
    /// launched under. Nothing here acts on it.
    runtime_mode: RuntimeMode,
    /// Last-EMITTED `SubagentUpdated` reading per `task_id` — the
    /// material-transition filter's only state. See [`SubagentSnapshot`].
    subagent_progress: HashMap<String, SubagentSnapshot>,
}

impl Normalizer {
    pub fn new(runtime_mode: RuntimeMode) -> Self {
        Self {
            saw_init: false,
            assistant_message_id: new_message_id(),
            session_id: None,
            runtime_mode,
            subagent_progress: HashMap::new(),
        }
    }

    /// Emit a `SubagentUpdated` for `task_id`, unless `snapshot` is identical
    /// to the last one emitted for the same id — the material-transition
    /// filter. Also the mechanism behind the `tool_use_result` fallback:
    /// when `task_notification` already reported this exact terminal state,
    /// the fallback's candidate snapshot matches and is silently absorbed
    /// rather than needing a second, separate "did we already resolve this"
    /// check.
    fn emit_subagent_update(
        &mut self,
        task_id: String,
        snapshot: SubagentSnapshot,
    ) -> Vec<AgentEvent> {
        if self.subagent_progress.get(&task_id) == Some(&snapshot) {
            return Vec::new();
        }
        let event = AgentEvent::SubagentUpdated {
            task_id: task_id.clone(),
            status: snapshot.status,
            activity: snapshot.activity.clone(),
            summary: snapshot.summary.clone(),
            total_tokens: snapshot.total_tokens,
            duration_ms: snapshot.duration_ms,
            tool_uses: snapshot.tool_uses,
        };
        self.subagent_progress.insert(task_id, snapshot);
        vec![event]
    }

    /// The four claimed `system/task_*` subtypes → `SubagentStarted` /
    /// `SubagentUpdated`. See the module-level table in the task 3 brief for
    /// which subtype feeds which fields.
    fn normalize_subagent_task(&mut self, f: SubagentTaskFrame) -> Vec<AgentEvent> {
        match f.subtype.as_str() {
            "task_started" => vec![AgentEvent::SubagentStarted {
                task_id: f.task_id,
                tool_use_id: f.tool_use_id.unwrap_or_default(),
                agent_type: f.subagent_type.unwrap_or_default(),
                description: f.description.unwrap_or_default(),
                prompt: f
                    .prompt
                    .map(|p| crate::cap_prose(&p, crate::SUBAGENT_PROMPT_MAX)),
            }],
            "task_progress" => {
                let usage = f.usage.unwrap_or_default();
                self.emit_subagent_update(
                    f.task_id,
                    SubagentSnapshot {
                        status: SubagentStatus::Running,
                        activity: f.description,
                        summary: None,
                        total_tokens: usage.total_tokens,
                        tool_uses: usage.tool_uses,
                        duration_ms: usage.duration_ms,
                    },
                )
            }
            "task_updated" => {
                let status = f
                    .patch
                    .and_then(|p| p.status)
                    .as_deref()
                    .map(subagent_status)
                    .unwrap_or(SubagentStatus::Running);
                self.emit_subagent_update(
                    f.task_id,
                    SubagentSnapshot {
                        status,
                        activity: None,
                        summary: None,
                        total_tokens: None,
                        tool_uses: None,
                        duration_ms: None,
                    },
                )
            }
            "task_notification" => {
                let usage = f.usage.unwrap_or_default();
                let status = f
                    .status
                    .as_deref()
                    .map(subagent_status)
                    .unwrap_or(SubagentStatus::Running);
                self.emit_subagent_update(
                    f.task_id,
                    SubagentSnapshot {
                        status,
                        activity: None,
                        summary: f.summary,
                        total_tokens: usage.total_tokens,
                        tool_uses: usage.tool_uses,
                        duration_ms: usage.duration_ms,
                    },
                )
            }
            // Unreachable in production: `parse_frame` routes only the four
            // subtypes above into `Frame::SubagentTask`. Not a panic — a
            // wire fact changing underneath this match must degrade quietly
            // rather than crash a run.
            _ => Vec::new(),
        }
    }

    /// Rotate the assistant message id for a steer boundary; returns
    /// (previous, next) for the `Steered` event.
    pub fn rotate_for_steer(&mut self) -> (String, String) {
        let prev = std::mem::replace(&mut self.assistant_message_id, new_message_id());
        (prev, self.assistant_message_id.clone())
    }

    /// Normalize one stdout frame into 0+ unified events. `interrupted` folds
    /// a post-interrupt `result` into `Done { status: Interrupted }`.
    pub fn normalize(&mut self, frame: Frame, interrupted: bool) -> Vec<AgentEvent> {
        match frame {
            Frame::System(f) => {
                if f.subtype != "init" || self.saw_init {
                    return Vec::new();
                }
                self.saw_init = true;
                self.session_id = Some(f.session_id.clone());
                vec![AgentEvent::SessionStarted {
                    harness: HarnessId::ClaudeCode,
                    model: f.model,
                    tools: f.tools,
                    cwd: f.cwd,
                    session_id: f.session_id,
                    assistant_message_id: self.assistant_message_id.clone(),
                    runtime_mode: self.runtime_mode,
                }]
            }

            Frame::SystemNotice(f) => notice_events(&f),

            // Frames with `parent_tool_use_id` set belong to a SUBAGENT's
            // nested transcript; a background Task runs concurrently with the
            // parent's text stream, so folding them in would split a contiguous
            // text block around a phantom tool call. Only null-parent frames
            // are this turn's own content.
            Frame::StreamEvent(f) => {
                if f.parent_tool_use_id.is_some() || f.event.kind != "content_block_delta" {
                    return Vec::new();
                }
                match f.event.delta.kind.as_str() {
                    "text_delta" => vec![AgentEvent::TextDelta {
                        text: f.event.delta.text,
                    }],
                    "thinking_delta" => vec![AgentEvent::ReasoningDelta {
                        text: f.event.delta.thinking,
                    }],
                    // A big tool input (a 90-line Write) streams as a long run
                    // of input_json_delta frames with nothing else — minutes of
                    // apparent silence that reads as a stalled run. Surface
                    // them as empty reasoning deltas: the engine treats those
                    // as pure liveness heartbeats (never journaled/rendered).
                    "input_json_delta" => vec![AgentEvent::ReasoningDelta {
                        text: String::new(),
                    }],
                    _ => Vec::new(),
                }
            }

            Frame::Assistant(f) => {
                if f.parent_tool_use_id.is_some() {
                    return Vec::new();
                }
                let mut out: Vec<AgentEvent> = f
                    .message
                    .blocks()
                    .filter(|b: &ContentBlock| b.kind == "tool_use")
                    .map(|b| AgentEvent::ToolCall {
                        id: b.id.clone(),
                        call: decode_tool_use(&b.name, &b.input),
                    })
                    .collect();
                // A failed turn (usage limit, billing, auth, overloaded, …)
                // carries a terse `error` code here — often with empty content
                // and no `result` error — so surface it visibly.
                if let Some(code) = &f.error {
                    out.push(AgentEvent::Error {
                        message: assistant_error_text(code),
                    });
                }
                // The enclosing assistant frame closes the streamed message
                // item; rotate so post-boundary deltas get a fresh id.
                let (prev, _next) = self.rotate_for_steer();
                out.push(AgentEvent::AssistantMessageCompleted {
                    assistant_message_id: prev,
                });
                out
            }

            Frame::User(f) => {
                if f.parent_tool_use_id.is_some() {
                    return Vec::new();
                }
                let mut out: Vec<AgentEvent> = f
                    .message
                    .blocks()
                    .filter(|b: &ContentBlock| b.kind == "tool_result")
                    .map(|b| AgentEvent::ToolResult {
                        id: b.tool_use_id.clone(),
                        is_error: b.is_error.unwrap_or(false),
                    })
                    .collect();
                // The `Agent` tool's own result carries the whole subagent
                // record on one frame — a fallback that resolves the card
                // even if `task_notification` never arrived. Routed through
                // the same dedupe as the live stream, so it is a no-op when
                // that stream already reported this exact terminal state.
                let fallback = f
                    .tool_use_result
                    .as_ref()
                    .and_then(subagent_result_from_tool_use_result);
                if let Some((task_id, snapshot)) = fallback {
                    out.extend(self.emit_subagent_update(task_id, snapshot));
                }
                out
            }

            Frame::SubagentTask(f) => self.normalize_subagent_task(f),

            // A claude.ai plan window. `rejected` blocks the turn and stays an
            // Error (deliberately unchanged this slice). `allowed_warning` is
            // the provider telling us the window is nearly spent — a state to
            // resolve, not a failure: a Warning notice. `allowed` stays quiet.
            Frame::RateLimit(f) => {
                let window =
                    rate_window_label(f.rate_limit_info.rate_limit_type.as_deref().unwrap_or(""));
                match f.rate_limit_info.status.as_str() {
                    "rejected" => vec![AgentEvent::Error {
                        message: format!(
                            "Claude {window} limit reached — the turn was blocked. Try again after it resets."
                        ),
                    }],
                    "allowed_warning" => vec![AgentEvent::Notice {
                        kind: NoticeKind::RateLimit,
                        severity: NoticeSeverity::Warning,
                        summary: format!("Approaching the Claude {window} usage limit"),
                        detail: None,
                        key: Some("rateLimit".into()),
                    }],
                    _ => Vec::new(),
                }
            }

            Frame::Result(f) => {
                if let Some(id) = &f.session_id {
                    self.session_id = Some(id.clone());
                }
                let usage = usage_event(&f.usage, &f.model_usage);
                let done = if f.subtype == "success" {
                    AgentEvent::Done {
                        status: if interrupted {
                            DoneStatus::Interrupted
                        } else {
                            DoneStatus::Completed
                        },
                        result: f.result,
                        error: None,
                        session_id: f.session_id,
                    }
                } else {
                    // Split the CLI's internal `[ede_diagnostic]` breadcrumbs
                    // off the real errors: diagnostics are debug-logged, never
                    // surfaced as transcript error parts.
                    let (diagnostics, errors): (Vec<String>, Vec<String>) = f
                        .errors
                        .iter()
                        .map(|e| match e {
                            Value::String(s) => s.clone(),
                            other => other.to_string(),
                        })
                        .partition(|m| is_internal_diagnostic(m));
                    for diagnostic in &diagnostics {
                        tracing::debug!(
                            target: "comet_harness::claude",
                            "internal CLI diagnostic (not surfaced): {diagnostic}"
                        );
                    }
                    let error = if !errors.is_empty() {
                        // Real user-relevant errors — surface verbatim.
                        Some(errors.join("; "))
                    } else {
                        match f.subtype.as_str() {
                            // Known run-failure subtypes stay visible with
                            // their mapped human wording (never blank — a
                            // blank error folds to no part and the failed
                            // turn reads as a silent non-reply).
                            "error_max_turns"
                            | "error_max_budget_usd"
                            | "error_max_structured_output_retries" => {
                                Some(result_error_text(&f.subtype).to_owned())
                            }
                            // Diagnostic-only ends (the CLI's turn-accounting
                            // telemetry, typically `error_during_execution`
                            // after an abort): nothing user-relevant to show.
                            _ if !diagnostics.is_empty() => None,
                            _ => Some(result_error_text(&f.subtype).to_owned()),
                        }
                    };
                    AgentEvent::Done {
                        status: if interrupted {
                            DoneStatus::Interrupted
                        } else {
                            DoneStatus::Errored
                        },
                        result: None,
                        error,
                        session_id: f.session_id,
                    }
                };
                match usage {
                    Some(usage) => vec![usage, done],
                    None => vec![done],
                }
            }

            // Recognized, deliberately dropped — the middle tier. Nothing to
            // emit; the reason names the owner.
            Frame::Ignored(reason) => {
                tracing::trace!(target: "comet_harness::claude", reason, "ignored frame");
                Vec::new()
            }

            // On neither list: still dropped — now counted. The full frame
            // was warn-logged at the drop site (parse_frame).
            Frame::Unknown { discriminator } => {
                vec![crate::diagnostic(
                    &discriminator,
                    DiagnosticSeverity::Unknown,
                )]
            }

            // Control frames are handled by the run loop, not normalized.
            Frame::ControlRequest(_) => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn decodes_typed_tools() {
        assert_eq!(
            decode_tool_use("Bash", &json!({"command": "ls -la"})),
            ToolCall::Exec {
                command: "ls -la".into()
            }
        );
        assert_eq!(
            decode_tool_use(
                "Edit",
                &json!({"file_path": "/a", "old_string": "x", "new_string": "y"})
            ),
            ToolCall::EditFile {
                path: "/a".into(),
                old_string: Some("x".into()),
                new_string: Some("y".into())
            }
        );
        assert_eq!(
            decode_tool_use(
                "TodoWrite",
                &json!({"todos": [{"content": "t", "status": "completed"}]})
            ),
            ToolCall::Todo {
                items: vec![TodoItem {
                    text: "t".into(),
                    done: true
                }]
            }
        );
        assert_eq!(
            decode_tool_use("mcp__linear__search", &json!({"q": "bug"})),
            ToolCall::Mcp {
                server: "linear".into(),
                tool: "search".into(),
                input: Some(json!({"q": "bug"}))
            }
        );
        assert!(matches!(
            decode_tool_use("Mystery", &json!({})),
            ToolCall::Unknown { .. }
        ));
    }

    /// A `result` frame emits `[usage, done]` when the turn made a model
    /// request and `[done]` alone when it made none, so take the last event
    /// rather than a fixed index.
    fn result_done(raw: &str) -> AgentEvent {
        let frame = crate::claude::wire::parse_frame(raw).expect("frame parses");
        let events = Normalizer::new(RuntimeMode::default()).normalize(frame, false);
        events.into_iter().next_back().expect("done event")
    }

    fn result_usage(raw: &str) -> Option<AgentEvent> {
        let frame = crate::claude::wire::parse_frame(raw).expect("frame parses");
        Normalizer::new(RuntimeMode::default())
            .normalize(frame, false)
            .into_iter()
            .find(|e| matches!(e, AgentEvent::Usage { .. }))
    }

    /// The literal frame Claude Code 2.1.228 sent on 2026-08-12, third turn of
    /// `run1-claude-3turns.jsonl`. Asserting against the raw JSON rather than a
    /// constructed `UsageBody` is the point: `input_tokens` alone is 10 here,
    /// and a test that round-tripped through the Rust type would stay green
    /// through exactly the bug this slice fixes.
    #[test]
    fn usage_sums_the_cache_fields_from_a_captured_frame() {
        let raw = r#"{"type":"result","subtype":"success","result":"DONE","errors":[],
            "usage":{"input_tokens":10,"output_tokens":26,"cache_read_input_tokens":34932,
                     "cache_creation_input_tokens":75},
            "modelUsage":{"claude-haiku-4-5-20251001":{"inputTokens":38,"outputTokens":326,
                          "contextWindow":200000,"maxOutputTokens":32000}}}"#;
        assert_eq!(
            result_usage(raw),
            Some(AgentEvent::Usage {
                prompt_tokens: 35_017,
                output_tokens: 26,
                context_window: Some(200_000),
            })
        );
    }

    /// `/context` and `/compact` run locally and report zeroes. Emitting that
    /// as a reading would blank a gauge over a session with 35k in it.
    #[test]
    fn a_slash_command_turn_reports_no_usage() {
        let raw = r#"{"type":"result","subtype":"success","result":"Context Usage","errors":[],
            "usage":{"input_tokens":0,"output_tokens":0,"cache_read_input_tokens":0,
                     "cache_creation_input_tokens":0}}"#;
        assert_eq!(result_usage(raw), None);
    }

    /// One model publishing a window and another staying silent is not
    /// agreement. Filtering the silent one out answers with the vocal model's
    /// limit, which the rest of the turn is not measured against.
    #[test]
    fn a_silent_model_is_not_agreement() {
        let raw = r#"{"type":"result","subtype":"success","result":"ok","errors":[],
            "usage":{"input_tokens":5,"output_tokens":1,"cache_read_input_tokens":100,
                     "cache_creation_input_tokens":0},
            "modelUsage":{"claude-haiku-4-5-20251001":{"contextWindow":200000},
                          "claude-opus-4-8":{"inputTokens":7}}}"#;
        assert!(matches!(
            result_usage(raw),
            Some(AgentEvent::Usage {
                context_window: None,
                ..
            })
        ));
    }

    /// Two models in one turn with different windows: there is no honest
    /// single limit to draw against, so decline rather than pick one.
    #[test]
    fn disagreeing_windows_decline_rather_than_guess() {
        let raw = r#"{"type":"result","subtype":"success","result":"ok","errors":[],
            "usage":{"input_tokens":5,"output_tokens":1,"cache_read_input_tokens":100,
                     "cache_creation_input_tokens":0},
            "modelUsage":{"claude-haiku-4-5-20251001":{"contextWindow":200000},
                          "claude-opus-4-8":{"contextWindow":1000000}}}"#;
        assert!(matches!(
            result_usage(raw),
            Some(AgentEvent::Usage {
                context_window: None,
                ..
            })
        ));
    }

    #[test]
    fn stream_deltas_map_to_text_reasoning_and_heartbeats() {
        let normalize = |raw: &str| {
            let frame = crate::claude::wire::parse_frame(raw).expect("frame parses");
            Normalizer::new(RuntimeMode::default()).normalize(frame, false)
        };
        // Real thinking text streams as a reasoning delta.
        let ev = normalize(
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"hmm"}}}"#,
        );
        assert_eq!(ev, vec![AgentEvent::ReasoningDelta { text: "hmm".into() }]);
        // Redacted thinking (estimated_tokens only) yields the empty
        // heartbeat shape the engine filters.
        let ev = normalize(
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"","estimated_tokens":50}}}"#,
        );
        assert_eq!(
            ev,
            vec![AgentEvent::ReasoningDelta {
                text: String::new()
            }]
        );
        // A tool input being generated (input_json_delta) is a liveness
        // heartbeat, not silence — minutes of a big Write must not read as
        // a stalled run.
        let ev = normalize(
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"input_json_delta","partial_json":"{\"file_"}}}"#,
        );
        assert_eq!(
            ev,
            vec![AgentEvent::ReasoningDelta {
                text: String::new()
            }]
        );
        // Signature deltas stay dropped.
        let ev = normalize(
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"signature_delta","signature":"abc"}}}"#,
        );
        assert!(ev.is_empty());
    }

    #[test]
    fn ede_diagnostics_never_surface_as_errors() {
        // The CLI's internal turn-accounting breadcrumbs must not become
        // transcript error parts (they showed up as raw red boxes).
        let done = result_done(
            r#"{"type":"result","subtype":"error_during_execution","errors":["[ede_diagnostic] result_type=user last_content_type=n/a stop_reason=null"]}"#,
        );
        match done {
            AgentEvent::Done { status, error, .. } => {
                assert_eq!(status, DoneStatus::Errored);
                assert_eq!(error, None, "diagnostic-only failure surfaces no text");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn real_errors_survive_diagnostic_filtering() {
        let done = result_done(
            r#"{"type":"result","subtype":"error_during_execution","errors":["[ede_diagnostic] turn aborted (x) stop_reason=null","Something real broke"]}"#,
        );
        match done {
            AgentEvent::Done { error, .. } => {
                assert_eq!(error.as_deref(), Some("Something real broke"));
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn known_failure_subtypes_keep_mapped_wording() {
        // A known run-failure subtype stays visible with human wording even
        // when its errors array is all diagnostics (or empty).
        let done = result_done(
            r#"{"type":"result","subtype":"error_max_turns","errors":["[ede_diagnostic] turn aborted (max) stop_reason=null"]}"#,
        );
        match done {
            AgentEvent::Done { error, .. } => {
                assert_eq!(
                    error.as_deref(),
                    Some("The run hit the maximum number of turns.")
                );
            }
            other => panic!("unexpected event: {other:?}"),
        }
        let done = result_done(r#"{"type":"result","subtype":"error_max_turns","errors":[]}"#);
        match done {
            AgentEvent::Done { error, .. } => {
                assert_eq!(
                    error.as_deref(),
                    Some("The run hit the maximum number of turns.")
                );
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    fn notice_of(raw: &str) -> AgentEvent {
        let frame = crate::claude::wire::parse_frame(raw).expect("frame parses");
        let events = Normalizer::new(RuntimeMode::default()).normalize(frame, false);
        assert_eq!(events.len(), 1, "{events:?}");
        events.into_iter().next().unwrap()
    }

    #[test]
    fn structured_system_subtypes_map_to_notices() {
        use comet_proto::{NoticeKind, NoticeSeverity};

        assert_eq!(
            notice_of(
                r#"{"type":"system","subtype":"compact_boundary","compact_metadata":{"trigger":"auto","pre_tokens":68000,"post_tokens":12000}}"#
            ),
            AgentEvent::Notice {
                kind: NoticeKind::Compaction,
                severity: NoticeSeverity::Info,
                summary: "Context compacted automatically".into(),
                detail: Some("68000 tokens → 12000".into()),
                key: Some("compaction".into()),
            }
        );
        assert_eq!(
            notice_of(
                r#"{"type":"system","subtype":"model_refusal_fallback","original_model":"claude-fable-5","fallback_model":"claude-haiku-4-5","direction":"sticky","content":"x"}"#
            ),
            AgentEvent::Notice {
                kind: NoticeKind::ModelRerouted,
                severity: NoticeSeverity::Warning,
                summary: "Model changed to claude-haiku-4-5".into(),
                detail: Some(
                    "claude-fable-5 refused the request; replies now come from claude-haiku-4-5."
                        .into()
                ),
                key: Some("model".into()),
            }
        );
        assert_eq!(
            notice_of(
                r#"{"type":"system","subtype":"api_retry","attempt":2,"max_retries":3,"retry_delay_ms":4000,"error_status":529}"#
            ),
            AgentEvent::Notice {
                kind: NoticeKind::Retrying,
                severity: NoticeSeverity::Warning,
                summary: "Retrying — attempt 2 of 3".into(),
                detail: Some("Next attempt in 4s.".into()),
                key: Some("retry".into()),
            }
        );
        // Manual compaction reads differently and a missing post_tokens
        // degrades the detail rather than lying.
        assert_eq!(
            notice_of(
                r#"{"type":"system","subtype":"compact_boundary","compact_metadata":{"trigger":"manual","pre_tokens":500}}"#
            ),
            AgentEvent::Notice {
                kind: NoticeKind::Compaction,
                severity: NoticeSeverity::Info,
                summary: "Context compacted".into(),
                detail: Some("500 tokens before compaction".into()),
                key: Some("compaction".into()),
            }
        );
    }

    #[test]
    fn passthrough_subtypes_carry_capped_provider_prose() {
        use comet_proto::{NoticeKind, NoticeSeverity};

        // informational: severity from `level`, key from `tool_use_id`.
        assert_eq!(
            notice_of(
                r#"{"type":"system","subtype":"informational","content":"Consider running /doctor.","level":"suggestion","tool_use_id":"tu-1"}"#
            ),
            AgentEvent::Notice {
                kind: NoticeKind::Info,
                severity: NoticeSeverity::Warning,
                summary: "Consider running /doctor.".into(),
                detail: None,
                key: Some("tu-1".into()),
            }
        );
        // info/notice levels stay quiet-severity.
        match notice_of(
            r#"{"type":"system","subtype":"informational","content":"x","level":"notice"}"#,
        ) {
            AgentEvent::Notice { severity, .. } => {
                assert_eq!(severity, NoticeSeverity::Info)
            }
            other => panic!("unexpected {other:?}"),
        }
        // notification: severity from `priority`, key from `key`.
        assert_eq!(
            notice_of(
                r#"{"type":"system","subtype":"notification","key":"usage-warning","text":"Half of the weekly limit is used.","priority":"immediate"}"#
            ),
            AgentEvent::Notice {
                kind: NoticeKind::Info,
                severity: NoticeSeverity::Warning,
                summary: "Half of the weekly limit is used.".into(),
                detail: None,
                key: Some("usage-warning".into()),
            }
        );
    }

    /// The 480-byte detail budget is what keeps unbounded provider prose out
    /// of a persisted, LAN-replayed doc, and this is its only guard — so the
    /// input must EXCEED it (600 bytes) and the length must be asserted
    /// exactly. A shorter input makes `cap_prose` a no-op and the assertion
    /// vacuous: it would pass just as happily if detail were capped at 160.
    #[test]
    fn oversized_provider_prose_is_capped_with_full_text_in_detail() {
        let long = "x".repeat(600);
        let raw = format!(
            r#"{{"type":"system","subtype":"informational","content":"{long}","level":"info"}}"#
        );
        match notice_of(&raw) {
            AgentEvent::Notice {
                summary, detail, ..
            } => {
                assert_eq!(summary.len(), 160 + '…'.len_utf8());
                assert!(summary.ends_with('…'));
                let detail = detail.expect("overflow keeps a longer detail");
                assert_eq!(detail.len(), 480 + '…'.len_utf8());
                assert!(detail.ends_with('…'));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn rate_limit_allowed_warning_becomes_a_notice_and_rejected_stays_an_error() {
        use comet_proto::{NoticeKind, NoticeSeverity};
        // allowed_warning: a notice, not an error.
        assert_eq!(
            notice_of(
                r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","rateLimitType":"five_hour"}}"#
            ),
            AgentEvent::Notice {
                kind: NoticeKind::RateLimit,
                severity: NoticeSeverity::Warning,
                summary: "Approaching the Claude 5-hour usage limit".into(),
                detail: None,
                key: Some("rateLimit".into()),
            }
        );
        // rejected: deliberately UNCHANGED — it blocks the turn, it is a
        // failure, and reclassifying it would be a behaviour change this
        // slice has no reason to make.
        let frame = crate::claude::wire::parse_frame(
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","rateLimitType":"five_hour"}}"#,
        )
        .unwrap();
        let events = Normalizer::new(RuntimeMode::default()).normalize(frame, false);
        assert!(matches!(&events[0], AgentEvent::Error { .. }), "{events:?}");
        // allowed: still quiet.
        let frame = crate::claude::wire::parse_frame(
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed"}}"#,
        )
        .unwrap();
        assert!(
            Normalizer::new(RuntimeMode::default())
                .normalize(frame, false)
                .is_empty()
        );
    }

    #[test]
    fn ignored_frames_are_silent_and_unknown_frames_become_diagnostics() {
        use comet_proto::DiagnosticSeverity;
        let normalize = |raw: &str| {
            let frame = crate::claude::wire::parse_frame(raw).expect("frame parses");
            Normalizer::new(RuntimeMode::default()).normalize(frame, false)
        };
        // Ignored tier: recognized, deliberately dropped — routine on every
        // healthy session, must produce NOTHING.
        assert!(
            normalize(r#"{"type":"system","subtype":"status","status":"requesting"}"#).is_empty()
        );
        assert!(
            normalize(r#"{"type":"system","subtype":"thinking_tokens","tokens":9}"#).is_empty()
        );
        assert!(normalize(r#"{"type":"tool_progress","tool_use_id":"t1"}"#).is_empty());
        // Unknown tier: exactly one diagnostic — discriminator named, severity
        // Unknown, summary Comet-authored (the payload never travels).
        let events =
            normalize(r#"{"type":"system","subtype":"someFutureSubtype","secret":"do-not-carry"}"#);
        assert_eq!(
            events,
            vec![AgentEvent::Diagnostic {
                discriminator: "system/someFutureSubtype".into(),
                severity: DiagnosticSeverity::Unknown,
                code: None,
                summary: "The agent sent a message Comet doesn't recognize.".into(),
            }]
        );
        // A hostile type string never travels: the sanitizer collapses it.
        let events = normalize(r#"{"type":"two words"}"#);
        assert!(matches!(
            &events[0],
            AgentEvent::Diagnostic { discriminator, .. } if discriminator == "malformed"
        ));
    }

    // ---- subagent task frames (slice 4.2 task 3) ----
    // Literal shapes throughout: captures/2026-08-13-plan-todo-subagent/
    // run2-claude-subagent.jsonl, per AGENTS.md's rule that wire-pinning
    // tests point at the JSON the provider actually sends.

    use comet_proto::SubagentStatus;

    fn normalize_one(raw: &str) -> Vec<AgentEvent> {
        let frame = crate::claude::wire::parse_frame(raw).expect("frame parses");
        Normalizer::new(RuntimeMode::default()).normalize(frame, false)
    }

    #[test]
    fn task_started_emits_subagent_started() {
        let raw = r#"{"type":"system","subtype":"task_started","task_id":"a6d1ae6c4fec0efe9","tool_use_id":"toolu_01M553SNnGHZ1j4whxE9zWq9","description":"Read README and report first heading","subagent_type":"general-purpose","task_type":"local_agent","prompt":"Read the README.md file in the current directory and report what the first heading is. Just state the heading text, nothing else."}"#;
        assert_eq!(
            normalize_one(raw),
            vec![AgentEvent::SubagentStarted {
                task_id: "a6d1ae6c4fec0efe9".into(),
                tool_use_id: "toolu_01M553SNnGHZ1j4whxE9zWq9".into(),
                agent_type: "general-purpose".into(),
                description: "Read README and report first heading".into(),
                prompt: Some(
                    "Read the README.md file in the current directory and report what the first heading is. Just state the heading text, nothing else."
                        .into()
                ),
            }]
        );
    }

    /// The prompt is a privacy decision, not a display one — capped at the
    /// harness boundary (per the slice's own decision doc) because it is
    /// unbounded and the transcript replays over the LAN. `SUBAGENT_PROMPT_MAX`
    /// is 480 bytes; the input has to exceed that for the assertion to be
    /// non-vacuous.
    #[test]
    fn an_oversized_prompt_is_capped_at_the_harness_boundary() {
        let long_prompt = "x".repeat(600);
        let raw = format!(
            r#"{{"type":"system","subtype":"task_started","task_id":"t1","tool_use_id":"tu1","description":"d","subagent_type":"general-purpose","prompt":"{long_prompt}"}}"#
        );
        match &normalize_one(&raw)[0] {
            AgentEvent::SubagentStarted { prompt, .. } => {
                let prompt = prompt.as_ref().expect("prompt present");
                assert_eq!(prompt.len(), crate::SUBAGENT_PROMPT_MAX + '…'.len_utf8());
                assert!(prompt.ends_with('…'));
            }
            other => panic!("expected SubagentStarted, got {other:?}"),
        }
    }

    #[test]
    fn task_progress_emits_a_running_update_with_activity_and_usage() {
        let raw = r#"{"type":"system","subtype":"task_progress","task_id":"a6d1ae6c4fec0efe9","tool_use_id":"toolu_01M553SNnGHZ1j4whxE9zWq9","description":"Reading README.md","subagent_type":"general-purpose","usage":{"total_tokens":19215,"tool_uses":1,"duration_ms":2906},"last_tool_name":"Read"}"#;
        assert_eq!(
            normalize_one(raw),
            vec![AgentEvent::SubagentUpdated {
                task_id: "a6d1ae6c4fec0efe9".into(),
                status: SubagentStatus::Running,
                activity: Some("Reading README.md".into()),
                summary: None,
                total_tokens: Some(19215),
                duration_ms: Some(2906),
                tool_uses: Some(1),
            }]
        );
    }

    /// The absent case, written by hand per
    /// `.agents/rules/optional-wire-fields.md`: nothing on the wire promises
    /// `task_progress` always carries a `usage` block, and `None` here must
    /// mean "not reported", never collapse to a reported zero.
    #[test]
    fn task_progress_with_no_usage_block_reports_none_not_zero() {
        let raw = r#"{"type":"system","subtype":"task_progress","task_id":"t1","tool_use_id":"tu1","description":"Starting up"}"#;
        assert_eq!(
            normalize_one(raw),
            vec![AgentEvent::SubagentUpdated {
                task_id: "t1".into(),
                status: SubagentStatus::Running,
                activity: Some("Starting up".into()),
                summary: None,
                total_tokens: None,
                duration_ms: None,
                tool_uses: None,
            }]
        );
    }

    #[test]
    fn task_updated_partial_patch_leaves_summary_and_usage_none() {
        let raw = r#"{"type":"system","subtype":"task_updated","task_id":"a6d1ae6c4fec0efe9","patch":{"status":"completed","end_time":1786581776304}}"#;
        assert_eq!(
            normalize_one(raw),
            vec![AgentEvent::SubagentUpdated {
                task_id: "a6d1ae6c4fec0efe9".into(),
                status: SubagentStatus::Completed,
                activity: None,
                summary: None,
                total_tokens: None,
                duration_ms: None,
                tool_uses: None,
            }]
        );
    }

    #[test]
    fn task_notification_emits_a_terminal_update_with_summary_and_usage() {
        let raw = r#"{"type":"system","subtype":"task_notification","task_id":"a6d1ae6c4fec0efe9","tool_use_id":"toolu_01M553SNnGHZ1j4whxE9zWq9","status":"completed","output_file":"C:\\tmp\\out","summary":"Sandbox","usage":{"total_tokens":20044,"tool_uses":1,"duration_ms":4906}}"#;
        assert_eq!(
            normalize_one(raw),
            vec![AgentEvent::SubagentUpdated {
                task_id: "a6d1ae6c4fec0efe9".into(),
                status: SubagentStatus::Completed,
                activity: None,
                summary: Some("Sandbox".into()),
                total_tokens: Some(20044),
                duration_ms: Some(4906),
                tool_uses: Some(1),
            }]
        );
    }

    /// A `SendMessage`-resumed agent fires a second `task_started` for the
    /// SAME `task_id` under a NEW `tool_use_id`. Keying the emitted event on
    /// `task_id` (not `tool_use_id`) is what keeps this one card — the
    /// falsification probe below breaks exactly this.
    #[test]
    fn a_resumed_agent_produces_two_started_events_sharing_one_task_id() {
        let first = r#"{"type":"system","subtype":"task_started","task_id":"a6d1ae6c4fec0efe9","tool_use_id":"toolu_01M553SNnGHZ1j4whxE9zWq9","description":"Read README and report first heading","subagent_type":"general-purpose","task_type":"local_agent","prompt":"Read the README.md file in the current directory and report what the first heading is. Just state the heading text, nothing else."}"#;
        let second = r#"{"type":"system","subtype":"task_started","task_id":"a6d1ae6c4fec0efe9","tool_use_id":"toolu_017ABUEwC8qtu28pSawot4BQ","description":"Read README and report first heading","subagent_type":"general-purpose","task_type":"local_agent","prompt":"What was the first heading you found?"}"#;

        let mut normalizer = Normalizer::new(RuntimeMode::default());
        let f1 = crate::claude::wire::parse_frame(first).unwrap();
        let f2 = crate::claude::wire::parse_frame(second).unwrap();
        let mut events = normalizer.normalize(f1, false);
        events.extend(normalizer.normalize(f2, false));

        assert_eq!(events.len(), 2, "{events:?}");
        let (task_ids, tool_use_ids): (Vec<&str>, Vec<&str>) = events
            .iter()
            .map(|e| match e {
                AgentEvent::SubagentStarted {
                    task_id,
                    tool_use_id,
                    ..
                } => (task_id.as_str(), tool_use_id.as_str()),
                other => panic!("expected SubagentStarted, got {other:?}"),
            })
            .unzip();
        assert_eq!(task_ids[0], task_ids[1], "both must share one task_id");
        assert_ne!(
            tool_use_ids[0], tool_use_ids[1],
            "the resumed invocation uses a new tool_use_id"
        );
    }

    #[test]
    fn an_unknown_status_string_maps_to_running_not_a_panic() {
        let raw = r#"{"type":"system","subtype":"task_notification","task_id":"t1","status":"sleeping","summary":"x"}"#;
        let events = normalize_one(raw);
        assert!(matches!(
            &events[0],
            AgentEvent::SubagentUpdated {
                status: SubagentStatus::Running,
                ..
            }
        ));
    }

    /// Unobserved on the wire (the capture only ever saw `"completed"`), so
    /// written by hand per `.agents/rules/optional-wire-fields.md`: a path
    /// whose only real input is the happy fixture ships never having been
    /// constructed.
    #[test]
    fn failed_and_cancelled_statuses_map_correctly() {
        let failed = r#"{"type":"system","subtype":"task_notification","task_id":"t1","status":"failed","summary":"x"}"#;
        assert!(matches!(
            &normalize_one(failed)[0],
            AgentEvent::SubagentUpdated {
                status: SubagentStatus::Failed,
                ..
            }
        ));
        let cancelled = r#"{"type":"system","subtype":"task_notification","task_id":"t1","status":"cancelled","summary":"x"}"#;
        assert!(matches!(
            &normalize_one(cancelled)[0],
            AgentEvent::SubagentUpdated {
                status: SubagentStatus::Cancelled,
                ..
            }
        ));
        // task_updated's patch.status is the same string space.
        let patch_failed = r#"{"type":"system","subtype":"task_updated","task_id":"t1","patch":{"status":"failed"}}"#;
        assert!(matches!(
            &normalize_one(patch_failed)[0],
            AgentEvent::SubagentUpdated {
                status: SubagentStatus::Failed,
                ..
            }
        ));
    }

    /// The material-transition filter: repeated `task_progress` ticks whose
    /// activity and usage figures did not move must not each mint a new
    /// event — this is the guard against the fan-out a coordinator's
    /// `workflow_progress` array would otherwise cause (out of scope here;
    /// Comet never decodes that array — see the task brief).
    #[test]
    fn repeated_identical_task_progress_frames_collapse_to_one_update() {
        let raw = r#"{"type":"system","subtype":"task_progress","task_id":"t1","tool_use_id":"tu1","description":"Reading README.md","usage":{"total_tokens":100,"tool_uses":1,"duration_ms":500}}"#;
        let mut normalizer = Normalizer::new(RuntimeMode::default());
        let frame = crate::claude::wire::parse_frame(raw).unwrap();
        let first = normalizer.normalize(frame, false);
        assert_eq!(first.len(), 1, "{first:?}");

        // An exact repeat: nothing material moved, so nothing emits.
        let frame = crate::claude::wire::parse_frame(raw).unwrap();
        let second = normalizer.normalize(frame, false);
        assert!(second.is_empty(), "{second:?}");

        // A real tick: the activity line and usage moved, so this emits.
        let moved = r#"{"type":"system","subtype":"task_progress","task_id":"t1","tool_use_id":"tu1","description":"Writing report.md","usage":{"total_tokens":250,"tool_uses":2,"duration_ms":1200}}"#;
        let frame = crate::claude::wire::parse_frame(moved).unwrap();
        let third = normalizer.normalize(frame, false);
        assert_eq!(
            third,
            vec![AgentEvent::SubagentUpdated {
                task_id: "t1".into(),
                status: SubagentStatus::Running,
                activity: Some("Writing report.md".into()),
                summary: None,
                total_tokens: Some(250),
                duration_ms: Some(1200),
                tool_uses: Some(2),
            }]
        );
    }

    /// The `Agent` tool's own `tool_use_result` carries the whole record on
    /// one frame — a fallback that resolves the card even if
    /// `task_notification` never arrived. Literal shape:
    /// run2-claude-subagent.jsonl sequence with `parent_tool_use_id: null`.
    #[test]
    fn agent_tool_use_result_resolves_the_card_with_no_notification() {
        let raw = r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_01M553SNnGHZ1j4whxE9zWq9","type":"tool_result","content":[{"type":"text","text":"Sandbox"}]}]},"parent_tool_use_id":null,"tool_use_result":{"status":"completed","agentId":"a6d1ae6c4fec0efe9","agentType":"general-purpose","content":[{"type":"text","text":"Sandbox"}],"resolvedModel":"claude-haiku-4-5-20251001","totalDurationMs":4907,"totalTokens":20115,"totalToolUseCount":1}}"#;
        let events = normalize_one(raw);
        let subagent_event = events
            .iter()
            .find(|e| matches!(e, AgentEvent::SubagentUpdated { .. }))
            .expect("a SubagentUpdated fallback");
        assert_eq!(
            subagent_event,
            &AgentEvent::SubagentUpdated {
                task_id: "a6d1ae6c4fec0efe9".into(),
                status: SubagentStatus::Completed,
                activity: None,
                summary: Some("Sandbox".into()),
                total_tokens: Some(20115),
                duration_ms: Some(4907),
                tool_uses: Some(1),
            }
        );
    }

    /// The fallback must not double up when `task_notification` already
    /// resolved the same terminal state — same dedupe path as the
    /// material-transition filter above, not a separate mechanism.
    #[test]
    fn agent_tool_use_result_is_suppressed_when_notification_already_matched() {
        let notification = r#"{"type":"system","subtype":"task_notification","task_id":"a6d1ae6c4fec0efe9","tool_use_id":"toolu_01M553SNnGHZ1j4whxE9zWq9","status":"completed","summary":"Sandbox","usage":{"total_tokens":20115,"tool_uses":1,"duration_ms":4907}}"#;
        let tool_result = r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_01M553SNnGHZ1j4whxE9zWq9","type":"tool_result","content":[{"type":"text","text":"Sandbox"}]}]},"parent_tool_use_id":null,"tool_use_result":{"status":"completed","agentId":"a6d1ae6c4fec0efe9","agentType":"general-purpose","content":[{"type":"text","text":"Sandbox"}],"resolvedModel":"claude-haiku-4-5-20251001","totalDurationMs":4907,"totalTokens":20115,"totalToolUseCount":1}}"#;

        let mut normalizer = Normalizer::new(RuntimeMode::default());
        let frame = crate::claude::wire::parse_frame(notification).unwrap();
        let first = normalizer.normalize(frame, false);
        assert_eq!(first.len(), 1, "{first:?}");

        let frame = crate::claude::wire::parse_frame(tool_result).unwrap();
        let second = normalizer.normalize(frame, false);
        assert!(
            !second
                .iter()
                .any(|e| matches!(e, AgentEvent::SubagentUpdated { .. })),
            "identical terminal state must not re-emit: {second:?}"
        );
    }

    /// An ordinary tool result (no `agentId`) must not be mistaken for a
    /// subagent record.
    #[test]
    fn a_non_agent_tool_use_result_emits_no_subagent_event() {
        let raw = r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"t1","type":"tool_result","content":[{"type":"text","text":"ok"}]}]},"parent_tool_use_id":null,"tool_use_result":{"stdout":"capture","interrupted":false}}"#;
        let events = normalize_one(raw);
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::SubagentUpdated { .. })),
            "{events:?}"
        );
    }
}
