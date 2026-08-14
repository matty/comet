//! Frame → [`AgentEvent`] normalization, ported from claude.ts's `normalize`
//! (init dedupe, subagent filtering, tool decoding, error-code mapping).

use std::collections::HashMap;

use comet_proto::{
    AgentEvent, ChecklistItem, ChecklistStatus, DiagnosticSeverity, DoneStatus, HarnessId,
    NoticeKind, NoticeSeverity, RuntimeMode, SubagentStatus, ToolCall,
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
/// `.agents/rules/optional-wire-fields.md`. `None` means the string matched
/// none of the three known values — distinct from the field being absent
/// entirely, which callers represent as `raw: None` in
/// [`Normalizer::status_or_carry_forward`], the function that decides what an
/// unrecognized or absent reading degrades to.
fn subagent_status(raw: &str) -> Option<SubagentStatus> {
    match raw {
        "completed" => Some(SubagentStatus::Completed),
        "failed" => Some(SubagentStatus::Failed),
        "cancelled" => Some(SubagentStatus::Cancelled),
        _ => None,
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

/// Whether a status is a resting state — nothing further changes it. Used
/// only to decide whether a second terminal reading should reopen a
/// resolved card; not exported, since nothing outside this dedupe cares.
fn is_terminal(status: SubagentStatus) -> bool {
    matches!(
        status,
        SubagentStatus::Completed | SubagentStatus::Failed | SubagentStatus::Cancelled
    )
}

/// Whether `candidate` reports something `prior` left blank. The terminal
/// guard uses this to tell a truly redundant second terminal reading (the
/// `tool_use_result` fallback repeating a notification's own numbers — every
/// field already populated on both sides) from a second terminal reading
/// that's the FIRST to fill a field in (`task_updated` reports status alone;
/// the `task_notification` one frame later is what actually carries the
/// answer and usage). Only "prior was `None`, candidate is `Some`" counts —
/// two different non-`None` numbers on the same field is the redundant case,
/// not a fill-in. This checks only ADDITION, not disagreement: a populated
/// `Failed` arriving after a populated `Completed` also returns `false` here
/// (nothing is `None` → `Some`) and is dropped by the guard — a contradicted
/// status is swallowed the same as a genuinely redundant repeat, inherited
/// from "first terminal reading wins" and unchanged by this function.
fn adds_new_detail(prior: &SubagentSnapshot, candidate: &SubagentSnapshot) -> bool {
    (candidate.activity.is_some() && prior.activity.is_none())
        || (candidate.summary.is_some() && prior.summary.is_none())
        || (candidate.total_tokens.is_some() && prior.total_tokens.is_none())
        || (candidate.tool_uses.is_some() && prior.tool_uses.is_none())
        || (candidate.duration_ms.is_some() && prior.duration_ms.is_none())
}

/// The subagent fields recoverable from the `Agent` tool's own
/// `tool_use_result`, with `status` still the RAW wire string. Resolving it
/// to a [`SubagentStatus`] needs `&self` (to carry forward a prior reading
/// when this particular frame reports none), which a free function doesn't
/// have — see [`Normalizer::status_or_carry_forward`].
struct SubagentToolResult {
    task_id: String,
    status: Option<String>,
    summary: Option<String>,
    total_tokens: Option<u64>,
    tool_uses: Option<u32>,
    duration_ms: Option<u64>,
}

/// Sniff the `Agent` tool's own `tool_use_result` for a subagent record, keyed
/// by `agentId` (== `task_id`). Shape-based rather than name-tracked: `agentId`
/// alone is specific to an `Agent` result, so an ordinary tool's result
/// (`Bash`'s `stdout`, `Write`'s diff, …) never matches. Returns `None` for
/// anything that isn't one.
fn subagent_result_from_tool_use_result(value: &Value) -> Option<SubagentToolResult> {
    let task_id = value.get("agentId").and_then(Value::as_str)?.to_owned();
    let status = value
        .get("status")
        .and_then(Value::as_str)
        .map(str::to_owned);
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
    // `n as u32` truncates rather than erroring on a value above `u32::MAX`
    // (e.g. `4294967296` silently becomes `0`) — a wrong count presented as a
    // real one. `try_from` treats an out-of-range value as absent instead.
    let tool_uses = value
        .get("totalToolUseCount")
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok());
    Some(SubagentToolResult {
        task_id,
        status,
        summary,
        total_tokens,
        tool_uses,
        duration_ms,
    })
}

/// Which task tool a held call was, so its result is read the right way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TaskCallKind {
    Create,
    Update,
    List,
}

impl TaskCallKind {
    /// `TodoWrite` is deliberately absent. It does not exist on any supported
    /// Claude Code — not on 2.1.229 and not on 2.1.228, the floor recorded in
    /// `docs/testing/supported-provider-versions.md`. Decoding a tool no
    /// supported CLI can send is a path that ships never having been
    /// constructed, which is the exact failure
    /// `.agents/rules/optional-wire-fields.md` exists to prevent.
    ///
    /// This says nothing about `ToolCall::Todo`, which survives for a
    /// different reason entirely — documents written before this branch, on
    /// any CLI. See that variant's own doc comment.
    fn from_tool_name(name: &str) -> Option<Self> {
        match name {
            "TaskCreate" => Some(Self::Create),
            "TaskUpdate" => Some(Self::Update),
            "TaskList" => Some(Self::List),
            _ => None,
        }
    }
}

/// Claude's task statuses are snake_case on the wire (`in_progress`); the
/// proto type is camelCase. Mapped by hand rather than through serde because
/// the two spellings genuinely differ and a rename attribute would hide that
/// from anyone reading either side.
fn checklist_status_from_claude(raw: &str) -> ChecklistStatus {
    match raw {
        "pending" => ChecklistStatus::Pending,
        "in_progress" => ChecklistStatus::InProgress,
        "completed" => ChecklistStatus::Completed,
        _ => ChecklistStatus::Unknown,
    }
}

/// An id the CLI may send as a JSON string or a JSON number — `TaskCreate`
/// answers `{"task":{"id":"1"}}` but nothing promises the quoting is stable,
/// and a number silently read as absent would orphan every later update.
fn id_field(value: &Value, key: &str) -> Option<String> {
    match value.get(key) {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    }
}

/// Turn a held `Task*` call plus its result into a checklist event.
///
/// **Both halves are required and neither is redundant.** `activeForm` and the
/// requested `status` appear ONLY on the tool input; `TaskCreate`'s assigned id
/// and `TaskUpdate`'s confirmed `statusChange` appear ONLY on the result.
/// Captured 2026-08-13 (`captures/2026-08-13-plan-todo-subagent.md`, §1 and §7)
/// against Claude Code 2.1.229.
fn checklist_event_from_task_call(
    kind: TaskCallKind,
    input: &Value,
    result: &Value,
) -> Option<AgentEvent> {
    match kind {
        TaskCallKind::Create => {
            let task = result.get("task")?;
            let item_id = id_field(task, "id")?;
            Some(AgentEvent::ChecklistItemChanged {
                item_id,
                // The result echoes the subject; the input is the fallback for
                // a build that stops echoing it.
                text: opt_str_field(task, "subject").or_else(|| opt_str_field(input, "subject")),
                active_form: opt_str_field(input, "activeForm"),
                // A creation reports no status of its own. `pending` is not a
                // guess: the first `TaskUpdate` on a fresh task reported
                // `statusChange.from == "pending"` on every observed run.
                status: opt_str_field(input, "status")
                    .as_deref()
                    .map_or(ChecklistStatus::Pending, checklist_status_from_claude),
            })
        }
        TaskCallKind::Update => {
            let item_id = id_field(result, "taskId").or_else(|| id_field(input, "taskId"))?;
            // The result's confirmed transition wins over the input's request:
            // the input is what was asked for, the result is what happened.
            let status = result
                .get("statusChange")
                .and_then(|c| opt_str_field(c, "to"))
                .or_else(|| opt_str_field(input, "status"))
                .as_deref()
                .map(checklist_status_from_claude)?;
            Some(AgentEvent::ChecklistItemChanged {
                item_id,
                // An update never carries a subject. This is the field the
                // fold may have to live without on a resumed run — see
                // `ChecklistItem::text`.
                text: None,
                active_form: opt_str_field(input, "activeForm"),
                status,
            })
        }
        TaskCallKind::List => {
            let tasks = result.get("tasks")?.as_array()?;
            let items: Vec<ChecklistItem> = tasks
                .iter()
                .filter_map(|t| {
                    Some(ChecklistItem {
                        id: id_field(t, "id")?,
                        text: opt_str_field(t, "subject"),
                        active_form: opt_str_field(t, "activeForm"),
                        status: opt_str_field(t, "status")
                            .as_deref()
                            .map_or(ChecklistStatus::Unknown, checklist_status_from_claude),
                    })
                })
                .collect();
            // **The element shape here is UNOBSERVED.** The capture recorded
            // that `TaskList` answers `{"tasks":[…]}` and never recorded what
            // is inside one, because the model never called it. So this
            // refuses to emit unless every element yielded an id: a
            // replacement built from a shape that turned out wrong would wipe
            // a correctly accumulated list, which is the one failure here that
            // destroys good data rather than merely missing some. Dropping the
            // reconciliation instead is recoverable — the next mutation still
            // lands. Confirm the shape before relaxing this.
            (!items.is_empty() && items.len() == tasks.len()).then_some(
                AgentEvent::ChecklistReplaced {
                    explanation: None,
                    items,
                },
            )
        }
    }
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
        // `TodoWrite` and the `Task*` family are DELIBERATELY absent here and
        // fall through to `Unknown`, which is how they render as ordinary tool
        // chips. The list they carry is its own event
        // (`AgentEvent::ChecklistReplaced` / `ChecklistItemChanged`, emitted
        // from `checklist_event_from_task_call`), not a `ToolCall` variant —
        // the same split t3code arrived at, and the reason `ToolCall::Todo` is
        // no longer constructed anywhere. See that variant's own doc comment
        // for why it still exists.
        //
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
    /// `Task*` calls whose result has not arrived yet, keyed by `tool_use_id`.
    ///
    /// Held because neither frame carries the whole picture: `activeForm` and
    /// the requested status are only on the tool INPUT, while the assigned id
    /// and the confirmed transition are only on the RESULT. Entries are
    /// removed on consumption, so the map is bounded by the calls in flight —
    /// a call whose result never arrives (an interrupted turn) leaks one small
    /// entry for the life of the run, which is why this is per-run state and
    /// not per-session.
    pending_task_calls: HashMap<String, (TaskCallKind, Value)>,
}

impl Normalizer {
    pub fn new(runtime_mode: RuntimeMode) -> Self {
        Self {
            saw_init: false,
            assistant_message_id: new_message_id(),
            session_id: None,
            runtime_mode,
            subagent_progress: HashMap::new(),
            pending_task_calls: HashMap::new(),
        }
    }

    /// The status for a frame that may not report one, or may report a
    /// string this build doesn't recognize: the previously known status for
    /// this `task_id` when we have one, `Running` otherwise (the only sane
    /// default for a `task_id` normalize has never seen). Never `Running` as
    /// a blind fallback — a status-less OR unrecognized-status frame arriving
    /// after a terminal reading must not resurrect a finished card, because
    /// "unknown" is not evidence the subagent restarted. This is exactly
    /// `.agents/rules/optional-wire-fields.md` for the absent case; the
    /// unrecognized case gets the same treatment for the same reason — the
    /// CLI ships often and a future status spelling ("succeeded",
    /// "timed_out", …) is the likelier trigger, not an exotic race. A
    /// NON-terminal stored reading (or no stored reading at all) still
    /// degrades an unrecognized string to `Running`, matching how a first,
    /// never-seen reading has always been handled — only a stored terminal
    /// reading is protected from being reopened.
    fn status_or_carry_forward(&self, task_id: &str, raw: Option<&str>) -> SubagentStatus {
        let stored = self
            .subagent_progress
            .get(task_id)
            .map(|snapshot| snapshot.status);
        match raw {
            Some(s) => match subagent_status(s) {
                Some(status) => status,
                None => match stored {
                    Some(status) if is_terminal(status) => status,
                    _ => SubagentStatus::Running,
                },
            },
            None => stored.unwrap_or(SubagentStatus::Running),
        }
    }

    /// Emit a `SubagentUpdated` for `task_id`, unless either: (a) `snapshot`
    /// is identical to the last one emitted for the same id — the
    /// material-transition filter — or (b) BOTH the stored reading and the
    /// new one are terminal AND the new one adds no detail the stored one was
    /// missing (see [`adds_new_detail`]). (b) is what makes the
    /// `tool_use_result` fallback safe without dropping the wire's own
    /// progression: `task_updated` reports status alone and `task_notification`
    /// one frame later is what actually carries the answer and usage — both
    /// terminal, but the second one adds detail, so it must still surface. A
    /// truly redundant terminal reading (the fallback repeating a
    /// notification's own numbers, where every field is already populated on
    /// both sides) adds nothing and is dropped. A CONTRADICTED terminal
    /// reading — e.g. a populated `Failed` arriving after a populated
    /// `Completed` — is dropped the same way, for the same reason: nothing in
    /// the candidate is `Some` where the stored reading was `None`, so
    /// `adds_new_detail` reports no addition either. First terminal reading
    /// wins even when the second one disagrees, not only when it repeats.
    fn emit_subagent_update(
        &mut self,
        task_id: String,
        snapshot: SubagentSnapshot,
    ) -> Vec<AgentEvent> {
        if let Some(prior) = self.subagent_progress.get(&task_id) {
            if prior == &snapshot {
                return Vec::new();
            }
            if is_terminal(prior.status)
                && is_terminal(snapshot.status)
                && !adds_new_detail(prior, &snapshot)
            {
                return Vec::new();
            }
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
            "task_started" => {
                // A `SendMessage`-resumed agent fires a second `task_started`
                // for the SAME task_id. Clear any stored reading from the
                // PRIOR invocation first, so the new run's own terminal
                // reading is compared against nothing — never against the
                // last run's, which would otherwise look like a redundant
                // terminal repeat and go silent.
                self.subagent_progress.remove(&f.task_id);
                if let Some(p) = &f.prompt {
                    tracing::debug!(
                        target: "comet_harness::claude",
                        "subagent prompt (full text): {}", p
                    );
                }
                vec![AgentEvent::SubagentStarted {
                    task_id: f.task_id,
                    tool_use_id: f.tool_use_id.unwrap_or_default(),
                    agent_type: f.subagent_type.unwrap_or_default(),
                    description: f.description.unwrap_or_default(),
                    prompt: f
                        .prompt
                        .map(|p| crate::cap_prose(&p, crate::SUBAGENT_PROMPT_MAX)),
                }]
            }
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
                let raw_status = f.patch.and_then(|p| p.status);
                let status = self.status_or_carry_forward(&f.task_id, raw_status.as_deref());
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
                let status = self.status_or_carry_forward(&f.task_id, f.status.as_deref());
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
                // Hold any `Task*` call until its result lands. The chip these
                // calls render is unaffected — they stay ordinary tool calls,
                // which is the split t3code arrived at: the plan is its own
                // event and the calls driving it are not special-cased away.
                for b in f.message.blocks() {
                    if b.kind == "tool_use"
                        && let Some(kind) = TaskCallKind::from_tool_name(&b.name)
                    {
                        self.pending_task_calls
                            .insert(b.id.clone(), (kind, b.input.clone()));
                    }
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
                // Join the `tool_result` back to the `Task*` call it answers.
                //
                // **`tool_use_result` is a SIBLING of `message`, not a field of
                // the block**, so it describes the frame and cannot be
                // attributed to one block among several. Every recorded frame
                // carries exactly one `tool_result` (19 of 19 across the
                // corpus), but nothing on the wire promises that — and if a
                // build ever batches two, applying one result to both would
                // silently stamp one call with the other's id and status.
                //
                // So a batched frame correlates NOTHING. The pending entries
                // stay pending rather than being consumed against a result
                // that may not be theirs: a missing mutation is recoverable
                // (the next frame for that task still lands) while a wrong one
                // is not. If this ever fires in practice the fix is upstream —
                // the frame would need per-block results to be decodable at
                // all.
                let tool_results = f
                    .message
                    .blocks()
                    .filter(|b: &ContentBlock| b.kind == "tool_result")
                    .count();
                for b in f.message.blocks().filter(|_| tool_results == 1) {
                    if b.kind != "tool_result" {
                        continue;
                    }
                    let Some((kind, input)) = self.pending_task_calls.remove(&b.tool_use_id) else {
                        continue;
                    };
                    // An errored call changed nothing, so it must not move the
                    // checklist. The prose the model sees says so; the typed
                    // result is absent or negative.
                    if b.is_error.unwrap_or(false) {
                        continue;
                    }
                    if let Some(result) = f.tool_use_result.as_ref()
                        && let Some(ev) = checklist_event_from_task_call(kind, &input, result)
                    {
                        out.push(ev);
                    }
                }
                // The `Agent` tool's own result carries the whole subagent
                // record on one frame — a fallback that resolves the card
                // even if `task_notification` never arrived. Routed through
                // the same dedupe as the live stream: on real captures its
                // numbers differ slightly from the notification's (a
                // task-tracking total vs. a model-turn total), so it is the
                // terminal-vs-terminal guard in `emit_subagent_update` — not
                // snapshot equality — that keeps this a no-op when the
                // notification already resolved the card.
                let fallback = f
                    .tool_use_result
                    .as_ref()
                    .and_then(subagent_result_from_tool_use_result);
                if let Some(result) = fallback {
                    let status =
                        self.status_or_carry_forward(&result.task_id, result.status.as_deref());
                    let snapshot = SubagentSnapshot {
                        status,
                        activity: None,
                        summary: result.summary,
                        total_tokens: result.total_tokens,
                        tool_uses: result.tool_uses,
                        duration_ms: result.duration_ms,
                    };
                    out.extend(self.emit_subagent_update(result.task_id, snapshot));
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
        // `TodoWrite` no longer decodes to `ToolCall::Todo` — the list it
        // carries is its own event now, and the call itself is an ordinary
        // chip like `TaskCreate` beside it. Nothing in this crate constructs
        // `ToolCall::Todo` any more; the variant survives only so historical
        // documents still decode (see its doc comment).
        let todo_write = decode_tool_use(
            "TodoWrite",
            &json!({"todos": [{"content": "t", "status": "completed"}]}),
        );
        assert!(
            matches!(&todo_write, ToolCall::Unknown { name, .. } if name == "TodoWrite"),
            "{todo_write:?}"
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

    /// A `task_updated` patch moving only `end_time` (no `status` key at
    /// all) must NOT regress a card the notification already marked
    /// `Completed` back to `Running`. `wire.rs`'s own doc comment on
    /// `SubagentPatch::status` says the absent case must decode absent,
    /// never collapse to a value — this is the normalizer honoring that one
    /// layer up, per `.agents/rules/optional-wire-fields.md`.
    #[test]
    fn a_status_less_task_updated_carries_forward_the_prior_status() {
        let mut normalizer = Normalizer::new(RuntimeMode::default());

        let notification = r#"{"type":"system","subtype":"task_notification","task_id":"t1","status":"completed","summary":"done"}"#;
        let frame = crate::claude::wire::parse_frame(notification).unwrap();
        let first = normalizer.normalize(frame, false);
        assert!(matches!(
            &first[0],
            AgentEvent::SubagentUpdated {
                status: SubagentStatus::Completed,
                ..
            }
        ));

        // A patch that moves only end_time — status absent, not "running".
        let status_less_update =
            r#"{"type":"system","subtype":"task_updated","task_id":"t1","patch":{"end_time":123}}"#;
        let frame = crate::claude::wire::parse_frame(status_less_update).unwrap();
        let second = normalizer.normalize(frame, false);
        // Carrying forward Completed means the candidate snapshot is
        // terminal-vs-terminal against the stored one, so nothing emits —
        // which is itself the point: the card must stay Completed, not
        // silently become Running.
        assert!(second.is_empty(), "{second:?}");
    }

    /// The same carry-forward, with no prior state at all: a `task_id`
    /// normalize has never seen defaults to `Running`, the only sane answer.
    #[test]
    fn a_status_less_task_updated_with_no_prior_state_defaults_to_running() {
        let raw =
            r#"{"type":"system","subtype":"task_updated","task_id":"t1","patch":{"end_time":123}}"#;
        assert!(matches!(
            &normalize_one(raw)[0],
            AgentEvent::SubagentUpdated {
                status: SubagentStatus::Running,
                ..
            }
        ));
    }

    /// A `task_notification` with no `status` field must carry forward too —
    /// same rule, different subtype.
    #[test]
    fn a_status_less_task_notification_carries_forward_the_prior_status() {
        // Starting from Running would not distinguish carry-forward from the
        // old `unwrap_or(Running)` bug — both give the same answer from that
        // starting point. Start from a TERMINAL prior reading instead, so
        // the two implementations diverge: carry-forward stays Completed
        // (and the terminal-vs-terminal guard then silences the repeat);
        // `unwrap_or(Running)` would regress to Running and emit it.
        let mut normalizer = Normalizer::new(RuntimeMode::default());

        let notification = r#"{"type":"system","subtype":"task_notification","task_id":"t1","status":"completed","summary":"done"}"#;
        let frame = crate::claude::wire::parse_frame(notification).unwrap();
        let first = normalizer.normalize(frame, false);
        assert!(matches!(
            &first[0],
            AgentEvent::SubagentUpdated {
                status: SubagentStatus::Completed,
                ..
            }
        ));

        // A second notification reporting no status at all.
        let status_less_notification =
            r#"{"type":"system","subtype":"task_notification","task_id":"t1","summary":"partial"}"#;
        let frame = crate::claude::wire::parse_frame(status_less_notification).unwrap();
        let second = normalizer.normalize(frame, false);
        assert!(
            second.is_empty(),
            "a status-less notification after Completed must carry Completed forward \
             (and the terminal guard then silences it), not regress to Running: {second:?}"
        );
    }

    /// A status-less `Agent` tool `tool_use_result` must carry forward too —
    /// `Running` here would leave a genuinely finished card spinning forever,
    /// since this frame only exists because the agent's turn ended.
    #[test]
    fn a_status_less_tool_use_result_carries_forward_the_prior_status() {
        let mut normalizer = Normalizer::new(RuntimeMode::default());

        let notification = r#"{"type":"system","subtype":"task_notification","task_id":"a6d1ae6c4fec0efe9","status":"completed","summary":"done"}"#;
        let frame = crate::claude::wire::parse_frame(notification).unwrap();
        let first = normalizer.normalize(frame, false);
        assert!(matches!(
            &first[0],
            AgentEvent::SubagentUpdated {
                status: SubagentStatus::Completed,
                ..
            }
        ));

        // No "status" key on this tool_use_result at all.
        let tool_result = r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_1","type":"tool_result","content":[{"type":"text","text":"Sandbox"}]}]},"parent_tool_use_id":null,"tool_use_result":{"agentId":"a6d1ae6c4fec0efe9","agentType":"general-purpose","content":[{"type":"text","text":"Sandbox"}],"totalDurationMs":1,"totalTokens":1,"totalToolUseCount":1}}"#;
        let frame = crate::claude::wire::parse_frame(tool_result).unwrap();
        let second = normalizer.normalize(frame, false);
        // This is NOT absorbed: the stored reading is {Completed,
        // summary: Some("done"), tokens/tool_uses/duration None} and this
        // frame supplies Some for all three usage fields, so
        // `adds_new_detail` is true and a SubagentUpdated DOES emit — the
        // point being tested is only that its status is the carried-forward
        // Completed, never a regression to Running. Pin the status exactly
        // (not merely "not Running") so a future change to the mechanism
        // can't silently satisfy a weaker assertion again.
        match second
            .iter()
            .find(|e| matches!(e, AgentEvent::SubagentUpdated { .. }))
        {
            Some(AgentEvent::SubagentUpdated { status, .. }) => {
                assert_eq!(*status, SubagentStatus::Completed, "{second:?}");
            }
            _ => {
                panic!("expected a SubagentUpdated carrying the carried-forward status: {second:?}")
            }
        }
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

    /// The mirror of the test above, with a stored TERMINAL reading in
    /// place: an unrecognized status string is not evidence a genuinely
    /// running subagent started, and it is also not evidence a finished one
    /// restarted — but before this fix `subagent_status` collapsed both to
    /// `Running` unconditionally, so a future CLI spelling ("succeeded",
    /// "timed_out", …) arriving after `Completed` reopened the card forever.
    /// `usage.total_tokens` is new detail the first reading never reported,
    /// so the terminal-vs-terminal guard does not silence this update and
    /// the emitted status is directly observable.
    #[test]
    fn an_unrecognized_status_after_a_terminal_reading_carries_it_forward_instead_of_reopening() {
        let mut normalizer = Normalizer::new(RuntimeMode::default());

        let notification = r#"{"type":"system","subtype":"task_notification","task_id":"t1","status":"completed","summary":"done"}"#;
        let frame = crate::claude::wire::parse_frame(notification).unwrap();
        let first = normalizer.normalize(frame, false);
        assert!(matches!(
            &first[0],
            AgentEvent::SubagentUpdated {
                status: SubagentStatus::Completed,
                ..
            }
        ));

        let unrecognized = r#"{"type":"system","subtype":"task_notification","task_id":"t1","status":"succeeded","summary":"done","usage":{"total_tokens":42}}"#;
        let frame = crate::claude::wire::parse_frame(unrecognized).unwrap();
        let second = normalizer.normalize(frame, false);
        match second.first() {
            Some(AgentEvent::SubagentUpdated { status, .. }) => {
                assert_eq!(
                    *status,
                    SubagentStatus::Completed,
                    "an unrecognized status after a terminal reading must carry the terminal \
                     status forward, not reopen the card as Running: {second:?}"
                );
            }
            other => panic!("expected a SubagentUpdated: {other:?}"),
        }
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
    /// event. This guards against redundant identical ticks, not against a
    /// coordinator's fan-out — a `workflow_progress` array on `task_progress`
    /// carries per-member updates this module never decodes in the first
    /// place, which is what actually keeps one coordinator tick at one
    /// `SubagentUpdated`. See `docs/debt/README.md`'s D55.
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

    /// The CAPTURED order (run2-claude-subagent.jsonl lines 106-107):
    /// `task_updated{patch.status:"completed"}` arrives BEFORE
    /// `task_notification{status:"completed", summary, usage}`. Both are
    /// terminal readings for the same `task_id`, and the notification is the
    /// ONLY one of the two carrying the answer and usage — a status-only
    /// terminal reading arriving first must not cause the terminal guard to
    /// drop the one that actually fills the card in.
    #[test]
    fn wire_order_task_updated_before_notification_still_surfaces_the_answer() {
        let mut normalizer = Normalizer::new(RuntimeMode::default());

        let progress = r#"{"type":"system","subtype":"task_progress","task_id":"a6d1ae6c4fec0efe9","tool_use_id":"toolu_01M553SNnGHZ1j4whxE9zWq9","description":"Reading README.md","usage":{"total_tokens":19215,"tool_uses":1,"duration_ms":2906}}"#;
        normalizer.normalize(crate::claude::wire::parse_frame(progress).unwrap(), false);

        // Sequence 106: task_updated, status only.
        let task_updated = r#"{"type":"system","subtype":"task_updated","task_id":"a6d1ae6c4fec0efe9","patch":{"status":"completed","end_time":1786581776304}}"#;
        let after_updated = normalizer.normalize(
            crate::claude::wire::parse_frame(task_updated).unwrap(),
            false,
        );
        assert_eq!(
            after_updated,
            vec![AgentEvent::SubagentUpdated {
                task_id: "a6d1ae6c4fec0efe9".into(),
                status: SubagentStatus::Completed,
                activity: None,
                summary: None,
                total_tokens: None,
                duration_ms: None,
                tool_uses: None,
            }],
            "the status-only terminal reading itself must still surface"
        );

        // Sequence 107: task_notification, the frame carrying the answer.
        let notification = r#"{"type":"system","subtype":"task_notification","task_id":"a6d1ae6c4fec0efe9","tool_use_id":"toolu_01M553SNnGHZ1j4whxE9zWq9","status":"completed","output_file":"C:\\tmp\\out","summary":"Sandbox","usage":{"total_tokens":20044,"tool_uses":1,"duration_ms":4906}}"#;
        let after_notification = normalizer.normalize(
            crate::claude::wire::parse_frame(notification).unwrap(),
            false,
        );
        assert_eq!(
            after_notification,
            vec![AgentEvent::SubagentUpdated {
                task_id: "a6d1ae6c4fec0efe9".into(),
                status: SubagentStatus::Completed,
                activity: None,
                summary: Some("Sandbox".into()),
                total_tokens: Some(20044),
                duration_ms: Some(4906),
                tool_uses: Some(1),
            }],
            "the notification must not be dropped just because a status-only \
             terminal reading arrived first"
        );
    }

    /// The full literal resume sequence (run2-claude-subagent.jsonl lines
    /// 98, 103, 106, 107, 150, 168, 169 — `background_tasks_changed` at 149
    /// omitted, it's a different task's territory): first invocation runs to
    /// completion, then a second `task_started` resumes the SAME `task_id`
    /// under a NEW `tool_use_id`. The second run's own terminal reading must
    /// not go silent just because the first run already left a terminal
    /// reading stored for this `task_id`.
    #[test]
    fn a_resumed_agent_end_to_end_surfaces_the_second_runs_answer() {
        let mut normalizer = Normalizer::new(RuntimeMode::default());
        let frames = [
            // First invocation.
            r#"{"type":"system","subtype":"task_started","task_id":"a6d1ae6c4fec0efe9","tool_use_id":"toolu_01M553SNnGHZ1j4whxE9zWq9","description":"Read README and report first heading","subagent_type":"general-purpose","task_type":"local_agent","prompt":"Read the README.md file in the current directory and report what the first heading is. Just state the heading text, nothing else."}"#,
            r#"{"type":"system","subtype":"task_progress","task_id":"a6d1ae6c4fec0efe9","tool_use_id":"toolu_01M553SNnGHZ1j4whxE9zWq9","description":"Reading README.md","usage":{"total_tokens":19215,"tool_uses":1,"duration_ms":2906}}"#,
            r#"{"type":"system","subtype":"task_updated","task_id":"a6d1ae6c4fec0efe9","patch":{"status":"completed","end_time":1786581776304}}"#,
            r#"{"type":"system","subtype":"task_notification","task_id":"a6d1ae6c4fec0efe9","tool_use_id":"toolu_01M553SNnGHZ1j4whxE9zWq9","status":"completed","output_file":"C:\\tmp\\out","summary":"Sandbox","usage":{"total_tokens":20044,"tool_uses":1,"duration_ms":4906}}"#,
            // Resumed: new tool_use_id, same task_id.
            r#"{"type":"system","subtype":"task_started","task_id":"a6d1ae6c4fec0efe9","tool_use_id":"toolu_017ABUEwC8qtu28pSawot4BQ","description":"Read README and report first heading","subagent_type":"general-purpose","task_type":"local_agent","prompt":"What was the first heading you found?"}"#,
            r#"{"type":"system","subtype":"task_updated","task_id":"a6d1ae6c4fec0efe9","patch":{"status":"completed","end_time":1786581781670}}"#,
            r#"{"type":"system","subtype":"task_notification","task_id":"a6d1ae6c4fec0efe9","tool_use_id":"toolu_017ABUEwC8qtu28pSawot4BQ","status":"completed","output_file":"C:\\tmp\\out2","summary":"The first heading is **Sandbox**.","usage":{"total_tokens":19111,"tool_uses":0,"duration_ms":2186}}"#,
        ];
        let mut events = Vec::new();
        for raw in frames {
            let frame = crate::claude::wire::parse_frame(raw).unwrap();
            events.extend(normalizer.normalize(frame, false));
        }
        let second_run_answer = events.iter().any(|e| {
            matches!(
                e,
                AgentEvent::SubagentUpdated { summary: Some(s), .. }
                    if s == "The first heading is **Sandbox**."
            )
        });
        assert!(
            second_run_answer,
            "the resumed run's own terminal notification must not be silently \
             dropped against the first run's stored terminal reading: {events:?}"
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

    /// `n as u32` truncates rather than erroring, so a `totalToolUseCount`
    /// above `u32::MAX` used to silently become a small wrong number instead
    /// of a missing one (`4294967296 as u32 == 0`). The checked conversion
    /// must read it as absent instead.
    #[test]
    fn an_out_of_range_tool_use_count_is_read_as_absent_not_wrapped() {
        let raw = r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_01M553SNnGHZ1j4whxE9zWq9","type":"tool_result","content":[{"type":"text","text":"Sandbox"}]}]},"parent_tool_use_id":null,"tool_use_result":{"status":"completed","agentId":"a6d1ae6c4fec0efe9","agentType":"general-purpose","content":[{"type":"text","text":"Sandbox"}],"resolvedModel":"claude-haiku-4-5-20251001","totalDurationMs":4907,"totalTokens":20115,"totalToolUseCount":4294967296}}"#;
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
                tool_uses: None,
            }
        );
    }

    /// The fallback must not reopen a card `task_notification` already
    /// resolved — but NOT via snapshot equality: on the real capture the
    /// notification's task-tracking usage (`total_tokens: 20044,
    /// duration_ms: 4906`, sequence 107 of run2-claude-subagent.jsonl) and
    /// the Agent tool's own model-turn usage (`totalTokens: 20115,
    /// totalDurationMs: 4907`, the very next frame) genuinely differ, so the
    /// two snapshots are never `==`. It's the terminal-vs-terminal guard in
    /// `emit_subagent_update` that suppresses this, which is why both
    /// literal captured numbers are used here unaltered rather than a
    /// synthetic pair chosen to match.
    #[test]
    fn agent_tool_use_result_does_not_reopen_a_card_the_notification_already_resolved() {
        let notification = r#"{"type":"system","subtype":"task_notification","task_id":"a6d1ae6c4fec0efe9","tool_use_id":"toolu_01M553SNnGHZ1j4whxE9zWq9","status":"completed","summary":"Sandbox","usage":{"total_tokens":20044,"tool_uses":1,"duration_ms":4906}}"#;
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
            "a second terminal reading, even with different numbers, must not reopen the card: {second:?}"
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

    // ---- checklist (slice 4.3, task 3) ----
    //
    // The wire evidence behind these tests is promoted corpus material, not
    // prose in this file — read at test time from `tests/corpus` by scenario
    // and frame sequence:
    //
    // - `TaskCreate`'s result carries the assigned id and subject; the id is
    //   on no tool input.
    // - `TaskUpdate`'s result carries an explicit `statusChange {from,to}`
    //   while `activeForm` is only ever on the input, so neither frame alone
    //   describes the change.
    // - A resumed process restates nothing at init and its first task frame
    //   updates an id it never created.
    //
    // The JSON literals below are copied from those same captures (Claude Code
    // 2.1.229), per `AGENTS.md`'s rule that a decode's test points at the
    // literal the provider sends rather than round-tripping through the Rust
    // type — a round trip stays green through exactly the failure that matters.

    /// Drive an assistant `tool_use` and its `user` result through one
    /// normalizer, the way the CLI actually sends them.
    fn drive(normalizer: &mut Normalizer, raws: &[impl AsRef<str>]) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        for raw in raws {
            let frame = crate::claude::wire::parse_frame(raw.as_ref()).unwrap();
            out.extend(normalizer.normalize(frame, false));
        }
        out
    }

    /// The reviewed frames a test rests on, in the order it names them.
    ///
    /// Read from the corpus rather than pasted here so a re-recording cannot
    /// leave this file asserting a shape the provider no longer sends. That
    /// drift is silent: hand-copied literals keep passing forever.
    fn corpus_run(scenario: &str, sequences: &[u64]) -> Vec<String> {
        sequences
            .iter()
            .map(|sequence| crate::capture::corpus_frame(scenario, *sequence).payload)
            .collect()
    }

    const CHECKLIST: &str = "claude/2.1.229/checklist";
    const CHECKLIST_RESUME: &str = "claude/2.1.229/checklist-resume";

    /// `TaskCreate`'s `tool_use_result` carries the assigned task id and its
    /// subject; the id appears nowhere on the tool input, so a decode reading
    /// only the input cannot key the item.
    #[test]
    fn task_create_yields_a_pending_item_with_its_assigned_id() {
        let frames = corpus_run(CHECKLIST, &[55, 64]);
        let mut n = Normalizer::new(RuntimeMode::default());
        let events = drive(&mut n, &frames);
        let changed: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::ChecklistItemChanged { .. }))
            .collect();
        assert_eq!(
            changed,
            vec![&AgentEvent::ChecklistItemChanged {
                // The id is on the RESULT only; the input never knows it.
                item_id: "1".into(),
                text: Some("Alpha step".into()),
                active_form: None,
                status: ChecklistStatus::Pending,
            }],
            "{events:?}"
        );
    }

    /// `TaskCreate`'s `tool_use_result` carries the assigned task id and its
    /// subject; the id appears nowhere on the tool input, so a decode reading
    /// only the input cannot key the item.
    #[test]
    fn a_task_create_still_renders_an_ordinary_tool_chip() {
        // The plan is its own event; the calls driving it stay ordinary tool
        // calls. Nothing here may swallow the chip.
        let frames = corpus_run(CHECKLIST, &[55, 64]);
        let mut n = Normalizer::new(RuntimeMode::default());
        let events = drive(&mut n, &frames[..1]);
        assert!(
            events.iter().any(|e| matches!(
                e,
                AgentEvent::ToolCall {
                    call: ToolCall::Unknown { name, .. },
                    ..
                } if name == "TaskCreate"
            )),
            "{events:?}"
        );
    }

    /// A resumed Claude process restates no task list at init and its first
    /// task frame updates an id it never created, so a per-run accumulator
    /// receives a status change for an unknown item.
    #[test]
    fn a_resumed_runs_update_for_an_unseen_task_carries_its_active_form() {
        // This test's three frames are the resumed run's init (which restates
        // no list), the update call, and its result. Task 2 was created by the
        // PREVIOUS process, so this normalizer has never seen its subject —
        // `text` is None and `activeForm` is the only readable label. The
        // ordinary two-turn case, not an exotic one.
        let frames = corpus_run(CHECKLIST_RESUME, &[2, 50, 55]);
        let mut n = Normalizer::new(RuntimeMode::default());
        let events = drive(&mut n, &frames);
        assert!(
            events.contains(&AgentEvent::ChecklistItemChanged {
                item_id: "2".into(),
                text: None,
                active_form: Some("Working the second step".into()),
                status: ChecklistStatus::InProgress,
            }),
            "{events:?}"
        );
    }

    #[test]
    fn a_completion_update_carries_no_active_form_and_that_is_fine() {
        // The `completed` transition sends neither subject nor activeForm. An
        // item whose FIRST sighting is this has a status and no text of any
        // kind — the case `ChecklistItem::text` is optional for.
        let call = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_0144zTWo88YYXGVKLQotEqpu","name":"TaskUpdate","input":{"taskId":"2","status":"completed"}}]},"parent_tool_use_id":null}"#;
        let result = r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_0144zTWo88YYXGVKLQotEqpu","type":"tool_result","content":"Updated task #2 status"}]},"parent_tool_use_id":null,"tool_use_result":{"success":true,"taskId":"2","updatedFields":["status"],"statusChange":{"from":"in_progress","to":"completed"}}}"#;
        let mut n = Normalizer::new(RuntimeMode::default());
        let events = drive(&mut n, &[call, result]);
        assert!(
            events.contains(&AgentEvent::ChecklistItemChanged {
                item_id: "2".into(),
                text: None,
                active_form: None,
                status: ChecklistStatus::Completed,
            }),
            "{events:?}"
        );
    }

    /// `TaskUpdate`'s `tool_use_result` reports an explicit `statusChange
    /// {from,to}`, while `activeForm` appears only on the tool input, so
    /// neither frame alone describes the change.
    #[test]
    fn the_results_confirmed_transition_beats_the_inputs_request() {
        // This test's two frames: an `in_progress` request whose result
        // confirms the same transition, with `activeForm` present on the input
        // and absent from the result. The decode must read the destination
        // from the result — the input is what was asked for, the result is
        // what happened.
        let frames = corpus_run(CHECKLIST, &[88, 93]);
        let mut n = Normalizer::new(RuntimeMode::default());
        let events = drive(&mut n, &frames);
        assert!(
            events.contains(&AgentEvent::ChecklistItemChanged {
                item_id: "1".into(),
                text: None,
                active_form: Some("Working the first step".into()),
                status: ChecklistStatus::InProgress,
            }),
            "{events:?}"
        );
    }

    #[test]
    fn an_update_reporting_no_status_anywhere_emits_nothing() {
        // An absent transition is not a transition. Defaulting to Pending here
        // would silently reset an item the user watched reach completed.
        let call = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"tu2","name":"TaskUpdate","input":{"taskId":"3","activeForm":"Reporting results"}}]},"parent_tool_use_id":null}"#;
        let result = r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"tu2","type":"tool_result","content":"Updated task #3 activeForm"}]},"parent_tool_use_id":null,"tool_use_result":{"success":true,"taskId":"3","updatedFields":["activeForm"]}}"#;
        let mut n = Normalizer::new(RuntimeMode::default());
        let events = drive(&mut n, &[call, result]);
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::ChecklistItemChanged { .. })),
            "{events:?}"
        );
    }

    #[test]
    fn an_errored_task_call_moves_nothing() {
        let call = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"tu3","name":"TaskUpdate","input":{"taskId":"4","status":"completed"}}]},"parent_tool_use_id":null}"#;
        let result = r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"tu3","type":"tool_result","is_error":true,"content":"No task #4"}]},"parent_tool_use_id":null,"tool_use_result":{"success":false,"taskId":"4","statusChange":{"from":"pending","to":"completed"}}}"#;
        let mut n = Normalizer::new(RuntimeMode::default());
        let events = drive(&mut n, &[call, result]);
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::ChecklistItemChanged { .. })),
            "{events:?}"
        );
    }

    #[test]
    fn an_unrelated_tools_result_emits_no_checklist_event() {
        // A `Read` result flowing through the same frame shape. Nothing about
        // it may look like a task mutation.
        let raw = r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_01CBBqQr95WuVk1MNSokZmod","type":"tool_result","content":"1\talpha\n"}]},"parent_tool_use_id":null,"tool_use_result":{"type":"text","file":{"filePath":"notes.txt"}}}"#;
        let events = normalize_one(raw);
        assert!(
            !events.iter().any(|e| matches!(
                e,
                AgentEvent::ChecklistItemChanged { .. } | AgentEvent::ChecklistReplaced { .. }
            )),
            "{events:?}"
        );
    }

    #[test]
    fn task_list_refuses_to_replace_from_an_unrecognized_element_shape() {
        // The element shape is UNOBSERVED — the model never called `TaskList`
        // on any recorded run. A replacement built from a wrong guess would
        // wipe a correctly accumulated list, so a shape that yields no ids
        // must emit nothing at all rather than an empty replacement.
        let call = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"tu4","name":"TaskList","input":{}}]},"parent_tool_use_id":null}"#;
        let result = r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"tu4","type":"tool_result","content":"3 tasks"}]},"parent_tool_use_id":null,"tool_use_result":{"tasks":[{"identifier":"1","title":"Read README.md"},{"identifier":"2","title":"Count lines"}]}}"#;
        let mut n = Normalizer::new(RuntimeMode::default());
        let events = drive(&mut n, &[call, result]);
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::ChecklistReplaced { .. })),
            "a shape whose elements yield no id must not produce a replacement: {events:?}"
        );
    }

    #[test]
    fn task_list_replaces_when_every_element_decodes() {
        let call = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"tu5","name":"TaskList","input":{}}]},"parent_tool_use_id":null}"#;
        let result = r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"tu5","type":"tool_result","content":"2 tasks"}]},"parent_tool_use_id":null,"tool_use_result":{"tasks":[{"id":"1","subject":"Read README.md","status":"completed"},{"id":"2","subject":"Count lines","status":"in_progress"}]}}"#;
        let mut n = Normalizer::new(RuntimeMode::default());
        let events = drive(&mut n, &[call, result]);
        assert!(
            events.contains(&AgentEvent::ChecklistReplaced {
                explanation: None,
                items: vec![
                    ChecklistItem {
                        id: "1".into(),
                        text: Some("Read README.md".into()),
                        active_form: None,
                        status: ChecklistStatus::Completed,
                    },
                    ChecklistItem {
                        id: "2".into(),
                        text: Some("Count lines".into()),
                        active_form: None,
                        status: ChecklistStatus::InProgress,
                    },
                ],
            }),
            "{events:?}"
        );
    }

    #[test]
    fn todo_write_publishes_no_checklist_on_a_supported_cli() {
        // `TodoWrite` exists on NO supported Claude Code — absent from 2.1.229
        // and from 2.1.228, the floor in
        // `docs/testing/supported-provider-versions.md`. Decoding it would be
        // a path that ships never having been constructed.
        //
        // If it ever reappears this is the test that will fail, which is the
        // point: it is a statement about supported versions, not about the
        // tool being impossible.
        let call = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"tw1","name":"TodoWrite","input":{"todos":[{"content":"Read the file","status":"completed"},{"content":"Count the lines","status":"in_progress"}]}}]},"parent_tool_use_id":null}"#;
        let result = r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"tw1","type":"tool_result","content":"Todos updated"}]},"parent_tool_use_id":null,"tool_use_result":{"ok":true}}"#;
        let mut n = Normalizer::new(RuntimeMode::default());
        let events = drive(&mut n, &[call, result]);
        assert!(
            !events.iter().any(|e| matches!(
                e,
                AgentEvent::ChecklistReplaced { .. } | AgentEvent::ChecklistItemChanged { .. }
            )),
            "{events:?}"
        );
        // It still renders, as an ordinary unknown-tool chip — the call is not
        // swallowed, only its list is not interpreted.
        assert!(
            events.iter().any(|e| matches!(
                e,
                AgentEvent::ToolCall {
                    call: ToolCall::Unknown { name, .. },
                    ..
                } if name == "TodoWrite"
            )),
            "{events:?}"
        );
    }

    /// `tool_use_result` describes the FRAME, so a frame carrying two
    /// `tool_result` blocks cannot say which of them it belongs to. Applying
    /// it to both would stamp one call with the other's id and status — the
    /// silent mis-attribution, which is strictly worse than the missing
    /// mutation that dropping produces.
    ///
    /// Never observed: 19 of 19 recorded frames carry exactly one block. This
    /// is the unobserved case written by hand, per
    /// `.agents/rules/optional-wire-fields.md`.
    #[test]
    fn a_frame_batching_two_tool_results_correlates_neither() {
        let calls = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"tb1","name":"TaskUpdate","input":{"taskId":"1","status":"completed"}},{"type":"tool_use","id":"tb2","name":"TaskUpdate","input":{"taskId":"2","status":"completed"}}]},"parent_tool_use_id":null}"#;
        let batched = r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"tb1","type":"tool_result","content":"ok"},{"tool_use_id":"tb2","type":"tool_result","content":"ok"}]},"parent_tool_use_id":null,"tool_use_result":{"success":true,"taskId":"1","statusChange":{"from":"in_progress","to":"completed"}}}"#;
        let mut n = Normalizer::new(RuntimeMode::default());
        let events = drive(&mut n, &[calls, batched]);
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::ChecklistItemChanged { .. })),
            "a batched frame must correlate nothing rather than guess: {events:?}"
        );
        // Both tool_results still reach the stream as ordinary results — only
        // the checklist correlation is withheld.
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, AgentEvent::ToolResult { .. }))
                .count(),
            2,
            "{events:?}"
        );
    }

    #[test]
    fn a_numeric_task_id_decodes_rather_than_orphaning_the_item() {
        // The capture showed `"1"` quoted. Nothing promises that stays true,
        // and a number read as absent would drop every later update silently.
        let call = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"tu6","name":"TaskCreate","input":{"subject":"Read README.md"}}]},"parent_tool_use_id":null}"#;
        let result = r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"tu6","type":"tool_result","content":"created"}]},"parent_tool_use_id":null,"tool_use_result":{"task":{"id":7,"subject":"Read README.md"}}}"#;
        let mut n = Normalizer::new(RuntimeMode::default());
        let events = drive(&mut n, &[call, result]);
        assert!(
            events.iter().any(|e| matches!(
                e,
                AgentEvent::ChecklistItemChanged { item_id, .. } if item_id == "7"
            )),
            "{events:?}"
        );
    }
}
