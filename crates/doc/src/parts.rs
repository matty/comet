//! Message parts: the event fold, the render-only privacy policy, and continuation splitting.
//!
//! Ports of `packages/control/src/parts.ts` (fold) and
//! `packages/session-doc/src/{render-parts,messages}.ts`.

use serde::{Deserialize, Serialize};

use comet_proto::{
    AgentEvent, ApprovalDecision, ApprovalRequest, ChecklistItem, NoticeKind, NoticeSeverity,
    SubagentStatus, ToolCall, ToolDiffStat, UserInputQuestion,
};

use crate::constants::MSG_INLINE_MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MessageStatus {
    Streaming,
    Complete,
    Aborted,
}

/// Serde default for `MessagePart::Notice::occurrences` — a payload written
/// before collapse existed represents a single occurrence.
fn one() -> u32 {
    1
}

/// The id of the single [`MessagePart::Checklist`] in a run.
///
/// Fixed rather than generated, for the same reason `Subagent` derives its id
/// from `task_id`: the fold must be replay-safe. A generated id would make
/// re-folding the same event stream produce a part the previous fold did not
/// have, and there is exactly one checklist per run for it to collide with.
const CHECKLIST_PART_ID: &str = "checklist";

/// One rendered part of an assistant message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum MessagePart {
    Text {
        id: String,
        text: String,
    },
    #[serde(rename_all = "camelCase")]
    Tool {
        id: String,
        call: ToolCall,
        #[serde(default)]
        is_error: bool,
        /// True once a ToolResult arrived.
        #[serde(default)]
        resolved: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        diff_ref: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        diff_stats: Option<Vec<ToolDiffStat>>,
    },
    #[serde(rename_all = "camelCase")]
    Input {
        id: String,
        request_id: String,
        questions: Vec<UserInputQuestion>,
        #[serde(default)]
        resolved: bool,
    },
    /// An approval the provider is blocked on. `decision: None` is the open
    /// state; the host is the sole writer of the resolved state, including the
    /// `Expired` it stamps when a pending approval's run ends.
    #[serde(rename_all = "camelCase")]
    Approval {
        id: String,
        request_id: String,
        approval: ApprovalRequest,
        #[serde(default)]
        decision: Option<ApprovalDecision>,
    },
    Error {
        id: String,
        message: String,
    },
    /// A provider notice (compaction, model reroute, retry, MCP status…).
    /// Positional: "the context was compacted HERE" is the whole message.
    #[serde(rename_all = "camelCase")]
    Notice {
        id: String,
        /// Stored as `noticeKind` on the wire — `kind` is the part-type tag.
        #[serde(rename = "noticeKind")]
        kind: NoticeKind,
        severity: NoticeSeverity,
        summary: String,
        #[serde(default)]
        detail: Option<String>,
        /// Collapse key — from the wire where the provider gives us one.
        #[serde(default)]
        key: Option<String>,
        /// 1 for a single occurrence; >1 after collapse.
        #[serde(default = "one")]
        occurrences: u32,
    },
    /// A subagent this run delegated to (Claude-only; see
    /// `AgentEvent::SubagentStarted`'s own doc for why). One card per
    /// `task_id` — a `SendMessage`-resumed agent's second `task_started`
    /// refreshes this part in place rather than adding a second card,
    /// matching how `SubagentUpdated` itself is applied.
    #[serde(rename_all = "camelCase")]
    Subagent {
        id: String,
        /// The DURABLE identity and the only field a `SubagentUpdated`
        /// matches against — never `tool_use_id`, which changes across a
        /// resume (see `AgentEvent::SubagentStarted`'s own doc).
        task_id: String,
        agent_type: String,
        description: String,
        status: SubagentStatus,
        /// The live activity line. `None` until the first `task_progress`.
        #[serde(default)]
        activity: Option<String>,
        /// The child's answer, on completion only.
        #[serde(default)]
        summary: Option<String>,
        /// `None` is "not reported yet", never zero — see `total_tokens` on
        /// `AgentEvent::SubagentUpdated`.
        #[serde(default)]
        total_tokens: Option<u64>,
        #[serde(default)]
        duration_ms: Option<u64>,
        #[serde(default)]
        tool_uses: Option<u32>,
    },
    /// The plan the agent published for this run, accumulated in place.
    ///
    /// **One per run, not one per publication.** Both providers restate rather
    /// than append — Codex sends a complete snapshot per change and Claude's
    /// `TaskCreate`/`TaskUpdate` move one item of a server-held list — so a
    /// second card would be a second copy of the same plan, not new
    /// information.
    ///
    /// Its scope is the RUN, which is native for Codex (its plan is turn-scoped)
    /// and an imposition on Claude (whose list is session-scoped and outlives
    /// the process). That asymmetry is deliberate and is what makes the
    /// unknown-id rule in `apply_event` necessary; see it for the evidence.
    #[serde(rename_all = "camelCase")]
    Checklist {
        id: String,
        /// Codex's one-line rationale for the latest change. Claude sends none
        /// and none is synthesized for it, so `None` here is ordinary.
        #[serde(default)]
        explanation: Option<String>,
        items: Vec<ChecklistItem>,
    },
}

impl MessagePart {
    pub fn id(&self) -> &str {
        match self {
            MessagePart::Text { id, .. }
            | MessagePart::Tool { id, .. }
            | MessagePart::Input { id, .. }
            | MessagePart::Approval { id, .. }
            | MessagePart::Error { id, .. }
            | MessagePart::Notice { id, .. }
            | MessagePart::Subagent { id, .. }
            | MessagePart::Checklist { id, .. } => id,
        }
    }

    pub fn byte_len(&self) -> usize {
        match self {
            MessagePart::Text { text, .. } => text.len(),
            MessagePart::Tool {
                call,
                diff_ref,
                diff_stats,
                ..
            } => {
                serde_json::to_vec(call).map_or(0, |v| v.len())
                    + diff_ref
                        .as_ref()
                        .and_then(|value| serde_json::to_vec(value).ok())
                        .map_or(0, |value| value.len())
                    + diff_stats
                        .as_ref()
                        .and_then(|value| serde_json::to_vec(value).ok())
                        .map_or(0, |value| value.len())
            }
            MessagePart::Input { questions, .. } => {
                serde_json::to_vec(questions).map_or(0, |v| v.len())
            }
            MessagePart::Approval { approval, .. } => {
                serde_json::to_vec(approval).map_or(0, |v| v.len())
            }
            MessagePart::Error { message, .. } => message.len(),
            MessagePart::Notice {
                summary, detail, ..
            } => summary.len() + detail.as_deref().map_or(0, str::len),
            MessagePart::Subagent {
                agent_type,
                description,
                activity,
                summary,
                ..
            } => {
                agent_type.len()
                    + description.len()
                    + activity.as_deref().map_or(0, str::len)
                    + summary.as_deref().map_or(0, str::len)
            }
            MessagePart::Checklist {
                explanation, items, ..
            } => {
                explanation.as_deref().map_or(0, str::len)
                    + items
                        .iter()
                        .map(|i| {
                            i.text.as_deref().map_or(0, str::len)
                                + i.active_form.as_deref().map_or(0, str::len)
                        })
                        .sum::<usize>()
            }
        }
    }
}

/// Fold one agent event into a parts accumulator, in place.
///
/// In place because the fold runs once per streamed event: rebuilding the
/// accumulator each time made long turns O(n²) in allocations.
///
/// Semantics from comet `foldEventIntoParts`:
/// - `SessionStarted` / `Steered` reset the accumulator (turn boundary — makes replay safe).
/// - `TextDelta` appends to the trailing text part, or starts a new one if the trail is not text
///   (a tool call in between breaks the text block).
/// - `ToolCall` appends, or refreshes in place when the id already exists (SDK retry idempotence).
/// - `ToolResult` marks the matching tool part resolved / errored in place.
/// - `InputRequested` appends an input part; `InputResolved` marks it resolved.
/// - `ApprovalRequested` appends an approval part; `ApprovalResolved` stamps its decision.
/// - `Error` and `Done{error}` become visible error parts.
pub fn fold_event_into_parts(out: &mut Vec<MessagePart>, event: &AgentEvent) {
    match event {
        AgentEvent::SessionStarted { .. } | AgentEvent::Steered { .. } => {
            out.clear();
        }
        AgentEvent::TextDelta { text } => {
            if let Some(MessagePart::Text { text: tail, .. }) = out.last_mut() {
                tail.push_str(text);
            } else {
                let id = format!("t{}", out.len());
                out.push(MessagePart::Text {
                    id,
                    text: text.clone(),
                });
            }
        }
        AgentEvent::ReasoningDelta { .. } => {
            // Reasoning is not rendered as a transcript part (matches comet).
        }
        AgentEvent::ToolCall { id, call } => {
            if let Some(existing) = out.iter_mut().find_map(|p| match p {
                MessagePart::Tool {
                    id: pid, call: c, ..
                } if pid == id => Some(c),
                _ => None,
            }) {
                *existing = call.clone();
            } else {
                out.push(MessagePart::Tool {
                    id: id.clone(),
                    call: call.clone(),
                    is_error: false,
                    resolved: false,
                    diff_ref: None,
                    diff_stats: None,
                });
            }
        }
        AgentEvent::ToolResult {
            id,
            is_error,
            diff_ref,
            diff_stats,
            ..
        } => {
            for p in out.iter_mut() {
                if let MessagePart::Tool {
                    id: pid,
                    is_error: e,
                    resolved,
                    diff_ref: part_diff_ref,
                    diff_stats: part_diff_stats,
                    ..
                } = p
                    && pid == id
                {
                    *e = *is_error;
                    *resolved = true;
                    *part_diff_ref = diff_ref.clone();
                    *part_diff_stats = diff_stats.clone();
                }
            }
        }
        AgentEvent::InputRequested {
            request_id,
            questions,
        } => {
            let id = format!("in-{request_id}");
            if !out.iter().any(|p| p.id() == id) {
                out.push(MessagePart::Input {
                    id,
                    request_id: request_id.clone(),
                    questions: questions.clone(),
                    resolved: false,
                });
            }
        }
        AgentEvent::InputResolved { request_id } => {
            for p in out.iter_mut() {
                if let MessagePart::Input {
                    request_id: rid,
                    resolved,
                    ..
                } = p
                    && rid == request_id
                {
                    *resolved = true;
                }
            }
        }
        AgentEvent::ApprovalRequested {
            request_id,
            approval,
        } => {
            let id = format!("ap-{request_id}");
            if !out.iter().any(|p| p.id() == id) {
                out.push(MessagePart::Approval {
                    id,
                    request_id: request_id.clone(),
                    approval: approval.clone(),
                    decision: None,
                });
            }
        }
        AgentEvent::ApprovalResolved {
            request_id,
            decision,
        } => {
            for p in out.iter_mut() {
                if let MessagePart::Approval {
                    request_id: rid,
                    decision: slot,
                    ..
                } = p
                    && rid == request_id
                {
                    *slot = Some(decision.clone());
                }
            }
        }
        AgentEvent::Error { message } => {
            let id = format!("e{}", out.len());
            out.push(MessagePart::Error {
                id,
                message: message.clone(),
            });
        }
        AgentEvent::Done { error, .. } => {
            if let Some(message) = error {
                let id = format!("e{}", out.len());
                out.push(MessagePart::Error {
                    id,
                    message: message.clone(),
                });
            }
        }
        AgentEvent::Notice {
            kind,
            severity,
            summary,
            detail,
            key,
        } => {
            // Collapse: only when the TRAILING part is a Notice with the same
            // kind and the same PRESENT key — occurrences increments and the
            // text follows the newest event (severity and detail refresh in
            // the same spirit). A notice recurring after other content gets
            // its own part: its position is what makes it meaningful.
            //
            // `key` is the provider's own dedupe id, carried verbatim where it
            // gives us one (`informational.tool_use_id`, `notification.key`)
            // and a per-kind constant for the structured kinds. Two absent keys
            // are the ABSENCE of evidence that two notices are the same thing,
            // not evidence that they are — and `tool_use_id` is documented as
            // scoping a message to one tool use, so an ordinary informational
            // message simply has none. Collapsing on `None == None` merged two
            // unrelated CLI messages and let the second overwrite the first's
            // text, in a persisted transcript that replays over the LAN.
            if let Some(MessagePart::Notice {
                kind: prev_kind,
                severity: prev_severity,
                summary: prev_summary,
                detail: prev_detail,
                key: prev_key,
                occurrences,
                ..
            }) = out.last_mut()
                && *prev_kind == *kind
                && prev_key.is_some()
                && *prev_key == *key
            {
                *occurrences = occurrences.saturating_add(1);
                *prev_severity = *severity;
                *prev_summary = summary.clone();
                *prev_detail = detail.clone();
            } else {
                let id = format!("n{}", out.len());
                out.push(MessagePart::Notice {
                    id,
                    kind: *kind,
                    severity: *severity,
                    summary: summary.clone(),
                    detail: detail.clone(),
                    key: key.clone(),
                    occurrences: 1,
                });
            }
        }
        AgentEvent::SubagentStarted {
            task_id,
            agent_type,
            description,
            // `tool_use_id` is kept on the event for the journal and a later
            // slice's child-transcript render, never as a match key here —
            // see `task_id`'s own doc on `MessagePart::Subagent`. `prompt` is
            // capped provider prose that must not enter the persisted
            // transcript, the same policy `sanitize_tool_call` applies to a
            // tool's write content.
            ..
        } => {
            let mut found = false;
            for p in out.iter_mut() {
                if let MessagePart::Subagent {
                    task_id: tid,
                    agent_type: a,
                    description: d,
                    status,
                    activity,
                    summary,
                    total_tokens,
                    duration_ms,
                    tool_uses,
                    ..
                } = p
                    && tid == task_id
                {
                    found = true;
                    // A `SendMessage`-resumed agent fires a second
                    // `task_started` for the SAME task_id under a NEW
                    // `tool_use_id` (Task 3's `normalize.rs`,
                    // `subagent_progress.remove(&f.task_id)` on this same
                    // event). Refresh the identity fields from the new
                    // invocation and reset the progress fields to their
                    // just-started values — carrying the prior run's
                    // terminal reading forward would show a finished card
                    // the instant the resumed run starts.
                    *a = agent_type.clone();
                    *d = description.clone();
                    *status = SubagentStatus::Running;
                    *activity = None;
                    *summary = None;
                    *total_tokens = None;
                    *duration_ms = None;
                    *tool_uses = None;
                }
            }
            if !found {
                out.push(MessagePart::Subagent {
                    id: format!("sub-{task_id}"),
                    task_id: task_id.clone(),
                    agent_type: agent_type.clone(),
                    description: description.clone(),
                    status: SubagentStatus::Running,
                    activity: None,
                    summary: None,
                    total_tokens: None,
                    duration_ms: None,
                    tool_uses: None,
                });
            }
        }
        AgentEvent::SubagentUpdated {
            task_id,
            status: new_status,
            activity: new_activity,
            summary: new_summary,
            total_tokens: new_total_tokens,
            duration_ms: new_duration_ms,
            tool_uses: new_tool_uses,
        } => {
            // Matched by `task_id`, never `tool_use_id`: a resumed
            // invocation's updates must land on the SAME card that
            // `SubagentStarted` refreshed above, not spawn a second one keyed
            // on the new tool call. An update for a `task_id` this
            // accumulator never saw (a lost `task_started`) is dropped, not
            // appended — a headless card with no description or agent_type
            // would be worse than no card.
            for p in out.iter_mut() {
                if let MessagePart::Subagent {
                    task_id: tid,
                    status,
                    activity,
                    summary,
                    total_tokens,
                    duration_ms,
                    tool_uses,
                    ..
                } = p
                    && tid == task_id
                {
                    *status = *new_status;
                    // DECISION (constraint 6): `task_updated` carries a
                    // PARTIAL patch (Task 3) — an absent field means "this
                    // frame said nothing about it", never "clear what an
                    // earlier frame reported". Assigning `None` straight
                    // through would silently discard a token count the
                    // instant a later status-only frame arrived, on every
                    // real run — see `.agents/rules/optional-wire-fields.md`.
                    if new_activity.is_some() {
                        *activity = new_activity.clone();
                    }
                    if new_summary.is_some() {
                        *summary = new_summary.clone();
                    }
                    if new_total_tokens.is_some() {
                        *total_tokens = *new_total_tokens;
                    }
                    if new_duration_ms.is_some() {
                        *duration_ms = *new_duration_ms;
                    }
                    if new_tool_uses.is_some() {
                        *tool_uses = *new_tool_uses;
                    }
                }
            }
        }
        // `SessionTitled` is chat-row metadata (`comet_engine::titles`
        // applies it to the `Chat.title` field, not the transcript), the
        // same class as `Usage` — never a transcript part.
        AgentEvent::AssistantMessageCompleted { .. }
        | AgentEvent::Usage { .. }
        | AgentEvent::Diagnostic { .. }
        | AgentEvent::SessionTitled { .. } => {}
        AgentEvent::ChecklistReplaced { explanation, items } => {
            // REPLACE, never merge. An item the snapshot omits is gone: Codex
            // drops a step from its plan and the card must drop it too.
            // Upserting here instead would accumulate every step the plan ever
            // held, which reads as a plan that only ever grows.
            if let Some(MessagePart::Checklist {
                explanation: e,
                items: existing,
                ..
            }) = out
                .iter_mut()
                .find(|p| matches!(p, MessagePart::Checklist { .. }))
            {
                *existing = items.clone();
                // Assigned UNCONDITIONALLY, including to `None`. The
                // explanation belongs to the snapshot that carried it: it
                // reads as the rationale for the change on screen beside it,
                // so keeping the previous one alive next to a newer plan
                // states a reason the agent did not give for it.
                //
                // A run is one provider, so there is no Claude replacement to
                // blank a Codex explanation — and if there were, clearing is
                // still the honest answer. Every observed `turn/plan/updated`
                // carried one (4 of 4, `run4-codex-plan.jsonl`), so this is
                // the unobserved case written by hand per
                // `.agents/rules/optional-wire-fields.md`.
                *e = explanation.clone();
            } else {
                out.push(MessagePart::Checklist {
                    id: CHECKLIST_PART_ID.to_owned(),
                    explanation: explanation.clone(),
                    items: items.clone(),
                });
            }
        }
        AgentEvent::ChecklistItemChanged {
            item_id,
            text,
            active_form,
            status,
        } => {
            let Some(MessagePart::Checklist { items, .. }) = out
                .iter_mut()
                .find(|p| matches!(p, MessagePart::Checklist { .. }))
            else {
                // No checklist yet: the first mutation of a run creates one.
                out.push(MessagePart::Checklist {
                    id: CHECKLIST_PART_ID.to_owned(),
                    explanation: None,
                    items: vec![ChecklistItem {
                        id: item_id.clone(),
                        text: text.clone(),
                        active_form: active_form.clone(),
                        status: *status,
                    }],
                });
                return;
            };
            if let Some(item) = items.iter_mut().find(|i| &i.id == item_id) {
                item.status = *status;
                // PARTIAL patch: absent means "this frame said nothing about
                // it", never "clear what an earlier frame reported". A
                // `TaskUpdate` carries no subject at all, and a `completed`
                // transition carries no `activeForm` either — assigning
                // straight through would blank both on the last update of
                // every item. Same trap `SubagentUpdated` documents above.
                if text.is_some() {
                    item.text = text.clone();
                }
                if active_form.is_some() {
                    item.active_form = active_form.clone();
                }
            } else {
                // CREATE on an unknown id, rather than dropping it.
                //
                // A resumed Claude process is told nothing about the list it
                // inherits — no `tasks` key on `system/init`, no `TaskList`
                // call — and its first task frame updates an id the previous
                // process created. Dropping would leave the whole resumed run
                // with an empty checklist, because nothing ever reconciles it.
                // Captured 2026-08-13 against 2.1.229:
                // `captures/2026-08-13-plan-todo-subagent.md` §7, and the
                // corpus pair at `claude/2.1.229/checklist{,-resume}`.
                //
                // `text` stays None when the update carries no subject, which
                // is what a `completed`-only first sighting produces. None
                // means "this run never saw the subject", NOT "the step has no
                // subject" — a reader that renders it as an empty line has
                // read it wrong.
                items.push(ChecklistItem {
                    id: item_id.clone(),
                    text: text.clone(),
                    active_form: active_form.clone(),
                    status: *status,
                });
            }
        }
    }
}

/// Render-only privacy policy — strip heavy/sensitive tool inputs before a call enters the doc.
///
/// Keeps: command / path / pattern / url / query / server+tool names.
/// Drops: WriteFile content, EditFile old/new strings, WebFetch prompt, Mcp/Unknown input.
/// Complete file sources are retained only by the engine's bounded sidecar. Idempotent.
pub fn sanitize_tool_call(call: &ToolCall) -> ToolCall {
    match call {
        ToolCall::WriteFile { path, .. } => ToolCall::WriteFile {
            path: path.clone(),
            content: None,
        },
        ToolCall::EditFile { path, .. } => ToolCall::EditFile {
            path: path.clone(),
            old_string: None,
            new_string: None,
        },
        ToolCall::WebFetch { url, .. } => ToolCall::WebFetch {
            url: url.clone(),
            prompt: None,
        },
        ToolCall::Mcp { server, tool, .. } => ToolCall::Mcp {
            server: server.clone(),
            tool: tool.clone(),
            input: None,
        },
        ToolCall::Unknown { name, .. } => ToolCall::Unknown {
            name: name.clone(),
            input: None,
        },
        other => other.clone(),
    }
}

/// Deterministic continuation id: `"{root}#c{n}"`.
pub fn continuation_id(root: &str, index: usize) -> String {
    format!("{root}#c{index}")
}

/// Split an oversized parts list into chunks each under `MSG_INLINE_MAX` bytes.
///
/// Splitting happens at part boundaries; an oversized text part is itself chunked at char
/// boundaries. Returns one Vec per resulting entry — the first keeps the root id, the rest are
/// continuations (`continuation_id(root, i)`), matching `splitMessageEntry` in comet.
pub fn split_parts(parts: &[MessagePart]) -> Vec<Vec<MessagePart>> {
    let mut chunks: Vec<Vec<MessagePart>> = vec![Vec::new()];
    let mut current_bytes = 0usize;

    let push_part = |chunks: &mut Vec<Vec<MessagePart>>, current: &mut usize, part: MessagePart| {
        let len = part.byte_len();
        if *current > 0 && *current + len > MSG_INLINE_MAX {
            chunks.push(Vec::new());
            *current = 0;
        }
        *current += len;
        chunks.last_mut().unwrap().push(part);
    };

    for part in parts {
        match part {
            MessagePart::Text { id, text } if text.len() > MSG_INLINE_MAX => {
                // Chunk oversized text at char boundaries.
                let mut start = 0usize;
                let mut piece = 0usize;
                while start < text.len() {
                    let mut end = (start + MSG_INLINE_MAX).min(text.len());
                    while end < text.len() && !text.is_char_boundary(end) {
                        end -= 1;
                    }
                    // Guard: ensure forward progress on pathological boundaries.
                    if end <= start {
                        end = text.len();
                    }
                    let sub = MessagePart::Text {
                        id: if piece == 0 {
                            id.clone()
                        } else {
                            format!("{id}~{piece}")
                        },
                        text: text[start..end].to_string(),
                    };
                    push_part(&mut chunks, &mut current_bytes, sub);
                    start = end;
                    piece += 1;
                }
            }
            other => push_part(&mut chunks, &mut current_bytes, other.clone()),
        }
    }
    chunks
}

/// Render-time inverse of splitting: concatenate continuation entries' parts in list order.
pub fn join_continuations(entries: Vec<Vec<MessagePart>>) -> Vec<MessagePart> {
    entries.into_iter().flatten().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_delta(s: &str) -> AgentEvent {
        AgentEvent::TextDelta { text: s.into() }
    }

    #[test]
    fn approval_requested_appends_an_open_part() {
        let mut parts = Vec::new();
        fold_event_into_parts(
            &mut parts,
            &AgentEvent::ApprovalRequested {
                request_id: "r1".into(),
                approval: ApprovalRequest::FileRead {
                    path: "a.rs".into(),
                },
            },
        );
        assert_eq!(parts.len(), 1);
        assert!(matches!(
            &parts[0],
            MessagePart::Approval { id, request_id, decision: None, .. }
                if id == "ap-r1" && request_id == "r1"
        ));
    }

    #[test]
    fn approval_requested_is_idempotent_on_the_same_id() {
        let mut parts = Vec::new();
        let ev = AgentEvent::ApprovalRequested {
            request_id: "r1".into(),
            approval: ApprovalRequest::FileRead {
                path: "a.rs".into(),
            },
        };
        fold_event_into_parts(&mut parts, &ev);
        fold_event_into_parts(&mut parts, &ev);
        assert_eq!(parts.len(), 1);
    }

    #[test]
    fn approval_resolved_stamps_the_matching_part() {
        let mut parts = Vec::new();
        fold_event_into_parts(
            &mut parts,
            &AgentEvent::ApprovalRequested {
                request_id: "r1".into(),
                approval: ApprovalRequest::FileRead {
                    path: "a.rs".into(),
                },
            },
        );
        fold_event_into_parts(
            &mut parts,
            &AgentEvent::ApprovalResolved {
                request_id: "r1".into(),
                decision: ApprovalDecision::Deny {
                    message: "no".into(),
                },
            },
        );
        assert!(matches!(
            &parts[0],
            MessagePart::Approval {
                decision: Some(ApprovalDecision::Deny { message }),
                ..
            } if message == "no"
        ));
    }

    #[test]
    fn approval_resolved_for_an_unknown_id_changes_nothing() {
        // The absent case: a decision arriving for a part this accumulator
        // never saw must not fabricate one.
        let mut parts = Vec::new();
        fold_event_into_parts(
            &mut parts,
            &AgentEvent::ApprovalResolved {
                request_id: "ghost".into(),
                decision: ApprovalDecision::Allow,
            },
        );
        assert!(parts.is_empty());
    }

    #[test]
    fn text_deltas_merge_until_broken_by_tool() {
        let mut parts = Vec::new();
        fold_event_into_parts(&mut parts, &text_delta("Hello "));
        fold_event_into_parts(&mut parts, &text_delta("world"));
        assert_eq!(parts.len(), 1);
        fold_event_into_parts(
            &mut parts,
            &AgentEvent::ToolCall {
                id: "tool-1".into(),
                call: ToolCall::Exec {
                    command: "ls".into(),
                },
            },
        );
        fold_event_into_parts(&mut parts, &text_delta("after"));
        assert_eq!(parts.len(), 3);
        match &parts[2] {
            MessagePart::Text { text, .. } => assert_eq!(text, "after"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn session_started_resets_accumulator() {
        let mut parts = Vec::new();
        fold_event_into_parts(&mut parts, &text_delta("junk"));
        fold_event_into_parts(
            &mut parts,
            &AgentEvent::SessionStarted {
                harness: comet_proto::HarnessId::Mock,
                model: "m".into(),
                tools: vec![],
                cwd: "/".into(),
                session_id: "s".into(),
                assistant_message_id: "a".into(),
                runtime_mode: comet_proto::RuntimeMode::default(),
            },
        );
        assert!(parts.is_empty());
    }

    #[test]
    fn tool_call_refresh_is_idempotent() {
        let call = AgentEvent::ToolCall {
            id: "t".into(),
            call: ToolCall::Exec {
                command: "ls".into(),
            },
        };
        let mut once = Vec::new();
        fold_event_into_parts(&mut once, &call);
        let mut twice = once.clone();
        fold_event_into_parts(&mut twice, &call);
        assert_eq!(once, twice);
    }

    #[test]
    fn tool_result_marks_resolution() {
        let mut parts = Vec::new();
        fold_event_into_parts(
            &mut parts,
            &AgentEvent::ToolCall {
                id: "t".into(),
                call: ToolCall::Exec {
                    command: "ls".into(),
                },
            },
        );
        fold_event_into_parts(
            &mut parts,
            &AgentEvent::ToolResult {
                id: "t".into(),
                is_error: true,
                diff: None,
                diff_ref: None,
                diff_stats: None,
            },
        );
        match &parts[0] {
            MessagePart::Tool {
                is_error, resolved, ..
            } => {
                assert!(*is_error);
                assert!(*resolved);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn tool_result_folds_reference_and_stats_without_exact_sources() {
        let mut parts = Vec::new();
        fold_event_into_parts(
            &mut parts,
            &AgentEvent::ToolCall {
                id: "t".into(),
                call: ToolCall::Exec {
                    command: "ls".into(),
                },
            },
        );
        fold_event_into_parts(
            &mut parts,
            &AgentEvent::ToolResult {
                id: "t".into(),
                is_error: true,
                diff: Some(comet_proto::ToolDiff {
                    path: "src/lib.rs".into(),
                    old_text: Some("SECRET_OLD_SOURCE".into()),
                    new_text: "new source".into(),
                }),
                diff_ref: Some("v1:abc123".into()),
                diff_stats: Some(vec![ToolDiffStat {
                    path: "src/lib.rs".into(),
                    additions: 1,
                    deletions: 1,
                }]),
            },
        );

        let MessagePart::Tool {
            is_error,
            resolved,
            diff_ref,
            diff_stats,
            ..
        } = &parts[0]
        else {
            panic!("unexpected {parts:?}");
        };
        assert!(*is_error);
        assert!(*resolved);
        assert_eq!(diff_ref.as_deref(), Some("v1:abc123"));
        assert_eq!(
            diff_stats.as_deref(),
            Some(
                [ToolDiffStat {
                    path: "src/lib.rs".into(),
                    additions: 1,
                    deletions: 1,
                }]
                .as_slice()
            )
        );
        assert!(
            !serde_json::to_string(&parts)
                .unwrap()
                .contains("SECRET_OLD_SOURCE"),
            "exact diff source leaked into MessagePart JSON"
        );
    }

    #[test]
    fn tool_byte_len_counts_json_encoded_diff_metadata() {
        let part = MessagePart::Tool {
            id: "tool-1".into(),
            call: ToolCall::Exec {
                command: "ls".into(),
            },
            is_error: false,
            resolved: true,
            diff_ref: Some("v1:ref\"\\😀".into()),
            diff_stats: Some(vec![ToolDiffStat {
                path: "src/\"quoted\"\\file.rs".into(),
                additions: 12,
                deletions: 3,
            }]),
        };

        assert_eq!(
            part.byte_len(),
            "{\"kind\":\"exec\",\"command\":\"ls\"}".len()
                + "\"v1:ref\\\"\\\\😀\"".len()
                + "[{\"path\":\"src/\\\"quoted\\\"\\\\file.rs\",\"additions\":12,\"deletions\":3}]"
                    .len(),
        );
    }

    #[test]
    fn sanitize_strips_heavy_inputs_and_is_idempotent() {
        let call = ToolCall::WriteFile {
            path: "/x".into(),
            content: Some("secret".into()),
        };
        let clean = sanitize_tool_call(&call);
        assert_eq!(
            clean,
            ToolCall::WriteFile {
                path: "/x".into(),
                content: None
            }
        );
        assert_eq!(sanitize_tool_call(&clean), clean);
    }

    #[test]
    fn split_and_join_round_trip() {
        let big = "x".repeat(MSG_INLINE_MAX * 2 + 100);
        let parts = vec![
            MessagePart::Text {
                id: "t0".into(),
                text: big.clone(),
            },
            MessagePart::Tool {
                id: "tool-1".into(),
                call: ToolCall::Exec {
                    command: "ls".into(),
                },
                is_error: false,
                resolved: true,
                diff_ref: None,
                diff_stats: None,
            },
        ];
        let chunks = split_parts(&parts);
        assert!(
            chunks.len() >= 3,
            "expected >=3 chunks, got {}",
            chunks.len()
        );
        for chunk in &chunks {
            let bytes: usize = chunk.iter().map(|p| p.byte_len()).sum();
            assert!(bytes <= MSG_INLINE_MAX, "chunk over cap: {bytes}");
        }
        let joined = join_continuations(chunks);
        let text: String = joined
            .iter()
            .filter_map(|p| match p {
                MessagePart::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, big);
        assert!(matches!(joined.last().unwrap(), MessagePart::Tool { .. }));
    }

    #[test]
    fn continuation_ids_are_deterministic() {
        assert_eq!(continuation_id("m1", 1), "m1#c1");
    }

    #[test]
    fn notice_part_serde_defaults_occurrences_to_one() {
        // A payload written before collapse existed (no `occurrences`).
        let json = r#"{"kind":"notice","id":"n0","noticeKind":"retrying","severity":"warning","summary":"Retrying — attempt 1 of 3"}"#;
        let part: MessagePart = serde_json::from_str(json).unwrap();
        match &part {
            MessagePart::Notice {
                occurrences,
                kind,
                severity,
                summary,
                detail,
                key,
                ..
            } => {
                assert_eq!(*occurrences, 1);
                assert_eq!(*kind, comet_proto::NoticeKind::Retrying);
                assert_eq!(*severity, comet_proto::NoticeSeverity::Warning);
                assert_eq!(summary, "Retrying — attempt 1 of 3");
                assert_eq!(*detail, None);
                assert_eq!(*key, None);
            }
            other => panic!("unexpected {other:?}"),
        }
        // Round trip.
        let json = serde_json::to_string(&part).unwrap();
        assert_eq!(serde_json::from_str::<MessagePart>(&json).unwrap(), part);
    }

    #[test]
    fn notice_byte_len_counts_summary_and_detail() {
        let part = MessagePart::Notice {
            id: "n0".into(),
            kind: comet_proto::NoticeKind::Info,
            severity: comet_proto::NoticeSeverity::Info,
            summary: "abcd".into(),
            detail: Some("efgh".into()),
            key: None,
            occurrences: 1,
        };
        assert_eq!(part.byte_len(), 8);
        assert_eq!(part.id(), "n0");
    }

    fn notice(kind: NoticeKind, key: &str, summary: &str) -> AgentEvent {
        AgentEvent::Notice {
            kind,
            severity: NoticeSeverity::Warning,
            summary: summary.into(),
            detail: None,
            key: Some(key.into()),
        }
    }

    /// A notice the provider gave no dedupe id for — what Claude's
    /// `informational` and `notification` emitters produce whenever
    /// `tool_use_id` / `key` are absent from the frame.
    fn keyless_notice(kind: NoticeKind, summary: &str, detail: &str) -> AgentEvent {
        AgentEvent::Notice {
            kind,
            severity: NoticeSeverity::Info,
            summary: summary.into(),
            detail: Some(detail.into()),
            key: None,
        }
    }

    /// Consecutive same-kind-same-key notices collapse into the TRAILING part:
    /// occurrences increments, summary follows the newest text. Five retry
    /// frames paint one chip reading "attempt 5 of 5".
    #[test]
    fn trailing_same_kind_same_key_notices_collapse() {
        let mut parts = Vec::new();
        fold_event_into_parts(
            &mut parts,
            &notice(NoticeKind::Retrying, "retry", "Retrying — attempt 1 of 3"),
        );
        fold_event_into_parts(
            &mut parts,
            &notice(NoticeKind::Retrying, "retry", "Retrying — attempt 2 of 3"),
        );
        assert_eq!(parts.len(), 1);
        match &parts[0] {
            MessagePart::Notice {
                occurrences,
                summary,
                ..
            } => {
                assert_eq!(*occurrences, 2);
                assert_eq!(summary, "Retrying — attempt 2 of 3");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    /// Two notices the provider gave no key for are NOT the same notice.
    /// `informational` carries `tool_use_id` only when the message is scoped to
    /// a tool use, so ordinary CLI messages arrive keyless — collapsing them on
    /// `None == None` merged unrelated text and dropped the first message's
    /// summary and detail from the persisted transcript.
    #[test]
    fn keyless_notices_never_collapse() {
        let mut parts = Vec::new();
        fold_event_into_parts(
            &mut parts,
            &keyless_notice(
                NoticeKind::Info,
                "Consider running /doctor.",
                "first detail",
            ),
        );
        fold_event_into_parts(
            &mut parts,
            &keyless_notice(NoticeKind::Info, "A plugin was disabled.", "second detail"),
        );
        assert_eq!(parts.len(), 2, "{parts:?}");
        match (&parts[0], &parts[1]) {
            (
                MessagePart::Notice {
                    summary: first,
                    detail: first_detail,
                    occurrences: first_count,
                    ..
                },
                MessagePart::Notice {
                    summary: second,
                    occurrences: second_count,
                    ..
                },
            ) => {
                // The first message survives intact — the bug overwrote it.
                assert_eq!(first, "Consider running /doctor.");
                assert_eq!(first_detail.as_deref(), Some("first detail"));
                assert_eq!(second, "A plugin was disabled.");
                assert_eq!(*first_count, 1);
                assert_eq!(*second_count, 1);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    /// A different key does not collapse, even with the same kind.
    #[test]
    fn different_key_notices_do_not_collapse() {
        let mut parts = Vec::new();
        fold_event_into_parts(
            &mut parts,
            &notice(NoticeKind::McpStatus, "mcp:a", "A failed"),
        );
        fold_event_into_parts(
            &mut parts,
            &notice(NoticeKind::McpStatus, "mcp:b", "B failed"),
        );
        assert_eq!(parts.len(), 2);
    }

    /// A different kind does not collapse, even with the same key — the
    /// guard is `kind == kind && key == key`, not key alone.
    #[test]
    fn different_kind_same_key_notices_do_not_collapse() {
        let mut parts = Vec::new();
        fold_event_into_parts(
            &mut parts,
            &notice(NoticeKind::Compaction, "shared", "Compacted"),
        );
        fold_event_into_parts(
            &mut parts,
            &notice(NoticeKind::McpStatus, "shared", "MCP failed"),
        );
        assert_eq!(parts.len(), 2);
    }

    /// Only the TRAILING part collapses: a notice recurring after other
    /// content gets its own chip, because its position is the message.
    #[test]
    fn a_notice_separated_by_text_does_not_collapse() {
        let mut parts = Vec::new();
        fold_event_into_parts(
            &mut parts,
            &notice(NoticeKind::Compaction, "compaction", "Compacted"),
        );
        fold_event_into_parts(&mut parts, &text_delta("more output"));
        fold_event_into_parts(
            &mut parts,
            &notice(NoticeKind::Compaction, "compaction", "Compacted"),
        );
        assert_eq!(parts.len(), 3);
        // Part count alone would still pass with a leaked counter: the new
        // chip must read as a first occurrence, not "×2".
        assert!(
            matches!(&parts[0], MessagePart::Notice { occurrences: 1, .. }),
            "{parts:?}"
        );
        assert!(
            matches!(&parts[2], MessagePart::Notice { occurrences: 1, .. }),
            "{parts:?}"
        );
    }

    /// DECISION (pinned): a notice folding into an empty accumulator produces
    /// a part — i.e. a notice arriving between turns is WRITTEN as a
    /// notice-only entry, not held. The engine's `sync_segment`
    /// (crates/engine/src/sessions.rs:837) lazily begins the entry when the
    /// fold is non-empty, and the `SessionStarted`/`Steered` clear at the top
    /// of this fold is not a loss risk: the engine finalizes a segment at
    /// those boundaries before the clear runs.
    #[test]
    fn notice_into_empty_accumulator_creates_a_part() {
        let mut parts = Vec::new();
        fold_event_into_parts(
            &mut parts,
            &notice(
                NoticeKind::McpStatus,
                "mcp:linear",
                "MCP server linear failed to start",
            ),
        );
        assert_eq!(parts.len(), 1);
        assert!(matches!(
            &parts[0],
            MessagePart::Notice { occurrences: 1, .. }
        ));
    }

    /// Spec verification 3: a Diagnostic is not transcript material — it folds
    /// to NO part, so a doc written during a run full of unknown frames is
    /// byte-identical to one without them.
    #[test]
    fn diagnostics_fold_to_no_part() {
        let mut parts = Vec::new();
        fold_event_into_parts(&mut parts, &text_delta("hello"));
        let before = parts.clone();
        fold_event_into_parts(
            &mut parts,
            &AgentEvent::Diagnostic {
                discriminator: "thread/checkpoint/created".into(),
                severity: comet_proto::DiagnosticSeverity::Unknown,
                code: None,
                summary: "The agent sent a message Comet doesn't recognize.".into(),
            },
        );
        assert_eq!(parts, before);
        // Into an empty accumulator too — nothing opens an entry.
        let mut empty = Vec::new();
        fold_event_into_parts(
            &mut empty,
            &AgentEvent::Diagnostic {
                discriminator: "unparseable".into(),
                severity: comet_proto::DiagnosticSeverity::Malformed,
                code: None,
                summary: "The agent sent a message Comet couldn't read.".into(),
            },
        );
        assert!(empty.is_empty());
    }

    fn subagent_started(task_id: &str, tool_use_id: &str, prompt: Option<&str>) -> AgentEvent {
        AgentEvent::SubagentStarted {
            task_id: task_id.into(),
            tool_use_id: tool_use_id.into(),
            agent_type: "general-purpose".into(),
            description: "Read README and report first heading".into(),
            prompt: prompt.map(str::to_owned),
        }
    }

    fn subagent_updated(
        task_id: &str,
        status: SubagentStatus,
        activity: Option<&str>,
        summary: Option<&str>,
        total_tokens: Option<u64>,
        duration_ms: Option<u64>,
        tool_uses: Option<u32>,
    ) -> AgentEvent {
        AgentEvent::SubagentUpdated {
            task_id: task_id.into(),
            status,
            activity: activity.map(str::to_owned),
            summary: summary.map(str::to_owned),
            total_tokens,
            duration_ms,
            tool_uses,
        }
    }

    /// `SubagentStarted` pushes a card, and a `SubagentUpdated` for the same
    /// `task_id` refreshes it in place rather than adding a second one.
    #[test]
    fn subagent_started_then_updated_collapses_to_one_part() {
        let mut parts = Vec::new();
        fold_event_into_parts(&mut parts, &subagent_started("t1", "tu1", Some("p")));
        fold_event_into_parts(
            &mut parts,
            &subagent_updated(
                "t1",
                SubagentStatus::Running,
                Some("Reading README.md"),
                None,
                Some(100),
                Some(50),
                Some(1),
            ),
        );
        assert_eq!(parts.len(), 1, "{parts:?}");
        match &parts[0] {
            MessagePart::Subagent {
                task_id,
                agent_type,
                description,
                status,
                activity,
                total_tokens,
                duration_ms,
                tool_uses,
                ..
            } => {
                assert_eq!(task_id, "t1");
                assert_eq!(agent_type, "general-purpose");
                assert_eq!(description, "Read README and report first heading");
                assert_eq!(*status, SubagentStatus::Running);
                assert_eq!(activity.as_deref(), Some("Reading README.md"));
                assert_eq!(*total_tokens, Some(100));
                assert_eq!(*duration_ms, Some(50));
                assert_eq!(*tool_uses, Some(1));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    /// A lost `task_started` must not let its `SubagentUpdated` fabricate a
    /// headless card — no description, no agent_type, nothing to show.
    #[test]
    fn subagent_update_for_unknown_task_id_is_dropped() {
        let mut parts = Vec::new();
        fold_event_into_parts(
            &mut parts,
            &subagent_updated(
                "ghost",
                SubagentStatus::Running,
                None,
                None,
                None,
                None,
                None,
            ),
        );
        assert!(parts.is_empty(), "{parts:?}");
    }

    /// Capped provider prose from `SubagentStarted.prompt` must never reach
    /// the persisted transcript — the same policy `sanitize_tool_call`
    /// applies to a tool's write content.
    #[test]
    fn subagent_part_never_carries_the_prompt() {
        let mut parts = Vec::new();
        fold_event_into_parts(
            &mut parts,
            &subagent_started("t1", "tu1", Some("a very specific secret prompt")),
        );
        let json = serde_json::to_string(&parts[0]).unwrap();
        assert!(!json.contains("secret prompt"), "{json}");
        assert!(!json.contains("\"prompt\""), "{json}");
    }

    /// DECISION (constraint 6, pinned): a `SubagentUpdated` whose fields are
    /// `None` leaves the part's existing reading alone — `None` means "this
    /// frame said nothing", never "clear it". A status-only `task_updated`
    /// arriving after a `task_progress` already reported usage must not wipe
    /// that usage back to unknown.
    #[test]
    fn subagent_update_with_none_fields_preserves_prior_values() {
        let mut parts = Vec::new();
        fold_event_into_parts(&mut parts, &subagent_started("t1", "tu1", None));
        fold_event_into_parts(
            &mut parts,
            &subagent_updated(
                "t1",
                SubagentStatus::Running,
                Some("Reading README.md"),
                None,
                Some(19_215),
                Some(2_906),
                Some(1),
            ),
        );
        // A status-only task_updated: every other field None on the wire.
        fold_event_into_parts(
            &mut parts,
            &subagent_updated(
                "t1",
                SubagentStatus::Completed,
                None,
                None,
                None,
                None,
                None,
            ),
        );
        match &parts[0] {
            MessagePart::Subagent {
                status,
                activity,
                total_tokens,
                duration_ms,
                tool_uses,
                ..
            } => {
                assert_eq!(*status, SubagentStatus::Completed);
                assert_eq!(
                    activity.as_deref(),
                    Some("Reading README.md"),
                    "an unreported activity must keep the earlier reading"
                );
                assert_eq!(*total_tokens, Some(19_215));
                assert_eq!(*duration_ms, Some(2_906));
                assert_eq!(*tool_uses, Some(1));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    /// The resume falsification probe (task-3's own resume scenario, replayed
    /// at the fold level): a `SendMessage`-resumed agent fires a SECOND
    /// `SubagentStarted` for the SAME `task_id` under a NEW `tool_use_id`,
    /// followed by that invocation's OWN terminal `SubagentUpdated`. The
    /// whole sequence must still collapse to ONE card — matching by
    /// `tool_use_id` anywhere in this path would leave two.
    #[test]
    fn a_resumed_subagent_collapses_to_one_card() {
        let mut parts = Vec::new();
        // First invocation.
        fold_event_into_parts(
            &mut parts,
            &subagent_started(
                "task-1",
                "tool-a",
                Some("Read the README.md file in the current directory."),
            ),
        );
        fold_event_into_parts(
            &mut parts,
            &subagent_updated(
                "task-1",
                SubagentStatus::Running,
                Some("Reading README.md"),
                None,
                Some(19_215),
                Some(2_906),
                Some(1),
            ),
        );
        fold_event_into_parts(
            &mut parts,
            &subagent_updated(
                "task-1",
                SubagentStatus::Completed,
                None,
                Some("Sandbox"),
                Some(20_044),
                Some(4_906),
                Some(1),
            ),
        );
        assert_eq!(parts.len(), 1, "{parts:?}");

        // Resumed invocation: same task_id, new tool_use_id, new prompt.
        fold_event_into_parts(
            &mut parts,
            &subagent_started(
                "task-1",
                "tool-b",
                Some("What was the first heading you found?"),
            ),
        );
        fold_event_into_parts(
            &mut parts,
            &subagent_updated(
                "task-1",
                SubagentStatus::Completed,
                None,
                Some("The first heading is **Sandbox**."),
                Some(19_111),
                Some(2_186),
                Some(0),
            ),
        );

        assert_eq!(
            parts.len(),
            1,
            "the resumed invocation must land on the SAME card: {parts:?}"
        );
        match &parts[0] {
            MessagePart::Subagent {
                task_id,
                status,
                summary,
                total_tokens,
                duration_ms,
                tool_uses,
                ..
            } => {
                assert_eq!(task_id, "task-1");
                assert_eq!(*status, SubagentStatus::Completed);
                assert_eq!(
                    summary.as_deref(),
                    Some("The first heading is **Sandbox**.")
                );
                assert_eq!(*total_tokens, Some(19_111));
                assert_eq!(*duration_ms, Some(2_186));
                assert_eq!(*tool_uses, Some(0));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    // ---- checklist fold (slice 4.3, task 5) ----

    fn item(id: &str, text: Option<&str>, status: comet_proto::ChecklistStatus) -> ChecklistItem {
        ChecklistItem {
            id: id.into(),
            text: text.map(str::to_owned),
            active_form: None,
            status,
        }
    }

    fn checklist_of(parts: &[MessagePart]) -> &Vec<ChecklistItem> {
        parts
            .iter()
            .find_map(|p| match p {
                MessagePart::Checklist { items, .. } => Some(items),
                _ => None,
            })
            .expect("a checklist part")
    }

    #[test]
    fn a_replacement_drops_the_items_it_omits() {
        // The whole reason `ChecklistReplaced` is its own variant. Codex sends
        // a complete snapshot per change; a step it stops sending is gone from
        // the plan, and merging here would build a plan that only ever grows.
        let mut parts = Vec::new();
        fold_event_into_parts(
            &mut parts,
            &AgentEvent::ChecklistReplaced {
                explanation: None,
                items: vec![
                    item("0", Some("a"), comet_proto::ChecklistStatus::Completed),
                    item("1", Some("b"), comet_proto::ChecklistStatus::InProgress),
                    item("2", Some("c"), comet_proto::ChecklistStatus::Pending),
                ],
            },
        );
        fold_event_into_parts(
            &mut parts,
            &AgentEvent::ChecklistReplaced {
                explanation: None,
                items: vec![
                    item("0", Some("a"), comet_proto::ChecklistStatus::Completed),
                    item("1", Some("b"), comet_proto::ChecklistStatus::Completed),
                ],
            },
        );
        assert_eq!(parts.len(), 1, "one card, not one per publication");
        let items = checklist_of(&parts);
        assert_eq!(items.len(), 2, "the dropped step is gone: {items:?}");
        assert!(items.iter().all(|i| i.text.as_deref() != Some("c")));
    }

    #[test]
    fn a_mutation_upserts_in_place_and_never_adds_a_second_card() {
        let mut parts = Vec::new();
        for status in [
            comet_proto::ChecklistStatus::Pending,
            comet_proto::ChecklistStatus::InProgress,
        ] {
            fold_event_into_parts(
                &mut parts,
                &AgentEvent::ChecklistItemChanged {
                    item_id: "1".into(),
                    text: Some("Read the file".into()),
                    active_form: None,
                    status,
                },
            );
        }
        assert_eq!(parts.len(), 1);
        let items = checklist_of(&parts);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].status, comet_proto::ChecklistStatus::InProgress);
    }

    #[test]
    fn a_status_only_update_does_not_blank_the_subject() {
        // The partial-patch trap. A `completed` transition carries neither
        // subject nor activeForm, so assigning straight through would blank
        // both on the LAST update of every item — i.e. on every finished row,
        // every time.
        let mut parts = Vec::new();
        fold_event_into_parts(
            &mut parts,
            &AgentEvent::ChecklistItemChanged {
                item_id: "1".into(),
                text: Some("Read the file".into()),
                active_form: Some("Reading the file".into()),
                status: comet_proto::ChecklistStatus::InProgress,
            },
        );
        fold_event_into_parts(
            &mut parts,
            &AgentEvent::ChecklistItemChanged {
                item_id: "1".into(),
                text: None,
                active_form: None,
                status: comet_proto::ChecklistStatus::Completed,
            },
        );
        let items = checklist_of(&parts);
        assert_eq!(items[0].status, comet_proto::ChecklistStatus::Completed);
        assert_eq!(items[0].text.as_deref(), Some("Read the file"));
        assert_eq!(items[0].active_form.as_deref(), Some("Reading the file"));
    }

    #[test]
    fn an_update_for_an_unknown_id_creates_the_item_rather_than_vanishing() {
        // What a resumed Claude run does on its very first task frame. The
        // previous process created task 2; this one is told nothing about it
        // (no `tasks` key on init, no `TaskList` call) and updates it blind.
        // Dropping would leave the whole resumed run with an empty checklist,
        // because nothing ever reconciles it. Capture §7, 2026-08-13, 2.1.229.
        let mut parts = Vec::new();
        fold_event_into_parts(
            &mut parts,
            &AgentEvent::ChecklistItemChanged {
                item_id: "2".into(),
                text: None,
                active_form: Some("Working the second step".into()),
                status: comet_proto::ChecklistStatus::InProgress,
            },
        );
        let items = checklist_of(&parts);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "2");
        assert_eq!(
            items[0].text, None,
            "None means this run never saw the subject, not that there is none"
        );
        assert_eq!(
            items[0].active_form.as_deref(),
            Some("Working the second step"),
            "activeForm is the only readable label such a row has"
        );
    }

    #[test]
    fn a_replacement_without_an_explanation_clears_the_previous_one() {
        // The explanation belongs to the snapshot that carried it: it reads as
        // the rationale for the plan shown beside it, so keeping the previous
        // one alive next to newer items states a reason the agent did not give
        // for them.
        //
        // Every observed `turn/plan/updated` carried one (4 of 4,
        // `run4-codex-plan.jsonl`), so this is the unobserved case written by
        // hand per `.agents/rules/optional-wire-fields.md`.
        let mut parts = Vec::new();
        fold_event_into_parts(
            &mut parts,
            &AgentEvent::ChecklistReplaced {
                explanation: Some("Moved to the line count.".into()),
                items: vec![item("0", Some("a"), comet_proto::ChecklistStatus::Pending)],
            },
        );
        fold_event_into_parts(
            &mut parts,
            &AgentEvent::ChecklistReplaced {
                explanation: None,
                items: vec![item(
                    "0",
                    Some("a"),
                    comet_proto::ChecklistStatus::Completed,
                )],
            },
        );
        let MessagePart::Checklist { explanation, .. } = &parts[0] else {
            panic!("expected a checklist");
        };
        assert_eq!(
            explanation, &None,
            "stale rationale must not survive beside a newer plan"
        );
    }

    #[test]
    fn a_run_boundary_clears_the_checklist_with_everything_else() {
        // `SessionStarted` resets the accumulator, which is what makes the
        // part run-scoped rather than chat-scoped. Pinned because the scoping
        // is the slice's most reversible decision (4.4 may want it wider).
        let mut parts = Vec::new();
        fold_event_into_parts(
            &mut parts,
            &AgentEvent::ChecklistReplaced {
                explanation: None,
                items: vec![item("0", Some("a"), comet_proto::ChecklistStatus::Pending)],
            },
        );
        assert_eq!(parts.len(), 1);
        fold_event_into_parts(
            &mut parts,
            &AgentEvent::SessionStarted {
                harness: comet_proto::HarnessId::ClaudeCode,
                model: "haiku".into(),
                tools: Vec::new(),
                cwd: "/tmp".into(),
                session_id: "s1".into(),
                assistant_message_id: "m1".into(),
                runtime_mode: Default::default(),
            },
        );
        assert!(parts.is_empty(), "{parts:?}");
    }

    /// `SessionTitled` is chat-row metadata, not message content — same
    /// class as `Usage`. It must never become a transcript part.
    #[test]
    fn session_titled_is_never_a_transcript_part() {
        let mut parts = Vec::new();
        fold_event_into_parts(
            &mut parts,
            &AgentEvent::SessionTitled {
                title: "Fix Login Flow".into(),
            },
        );
        assert!(parts.is_empty(), "{parts:?}");
    }
}
