//! Frame → [`AgentEvent`] normalization, ported from claude.ts's `normalize`
//! (init dedupe, subagent filtering, tool decoding, error-code mapping).

use comet_proto::{
    AgentEvent, DiagnosticSeverity, DoneStatus, HarnessId, NoticeKind, NoticeSeverity, TodoItem,
    ToolCall,
};
use serde_json::Value;

use super::wire::{ContentBlock, Frame, SystemNoticeFrame};

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
}

impl Normalizer {
    pub fn new() -> Self {
        Self {
            saw_init: false,
            assistant_message_id: new_message_id(),
            session_id: None,
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
                f.message
                    .blocks()
                    .filter(|b: &ContentBlock| b.kind == "tool_result")
                    .map(|b| AgentEvent::ToolResult {
                        id: b.tool_use_id.clone(),
                        is_error: b.is_error.unwrap_or(false),
                    })
                    .collect()
            }

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
                let usage = AgentEvent::Usage {
                    input_tokens: f.usage.input_tokens,
                    output_tokens: f.usage.output_tokens,
                };
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
                vec![usage, done]
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

    fn result_done(raw: &str) -> AgentEvent {
        let frame = crate::claude::wire::parse_frame(raw).expect("frame parses");
        let events = Normalizer::new().normalize(frame, false);
        assert_eq!(events.len(), 2, "usage + done");
        events.into_iter().nth(1).expect("done event")
    }

    #[test]
    fn stream_deltas_map_to_text_reasoning_and_heartbeats() {
        let normalize = |raw: &str| {
            let frame = crate::claude::wire::parse_frame(raw).expect("frame parses");
            Normalizer::new().normalize(frame, false)
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
        let events = Normalizer::new().normalize(frame, false);
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
        let events = Normalizer::new().normalize(frame, false);
        assert!(matches!(&events[0], AgentEvent::Error { .. }), "{events:?}");
        // allowed: still quiet.
        let frame = crate::claude::wire::parse_frame(
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed"}}"#,
        )
        .unwrap();
        assert!(Normalizer::new().normalize(frame, false).is_empty());
    }

    #[test]
    fn ignored_frames_are_silent_and_unknown_frames_become_diagnostics() {
        use comet_proto::DiagnosticSeverity;
        let normalize = |raw: &str| {
            let frame = crate::claude::wire::parse_frame(raw).expect("frame parses");
            Normalizer::new().normalize(frame, false)
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
}
