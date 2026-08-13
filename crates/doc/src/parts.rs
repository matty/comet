//! Message parts: the event fold, the render-only privacy policy, and continuation splitting.
//!
//! Ports of `packages/control/src/parts.ts` (fold) and
//! `packages/session-doc/src/{render-parts,messages}.ts`.

use serde::{Deserialize, Serialize};

use comet_proto::{
    AgentEvent, ApprovalDecision, ApprovalRequest, NoticeKind, NoticeSeverity, SubagentStatus,
    ToolCall, UserInputQuestion,
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
            | MessagePart::Subagent { id, .. } => id,
        }
    }

    pub fn byte_len(&self) -> usize {
        match self {
            MessagePart::Text { text, .. } => text.len(),
            MessagePart::Tool { call, .. } => serde_json::to_vec(call).map_or(0, |v| v.len()),
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
                });
            }
        }
        AgentEvent::ToolResult { id, is_error } => {
            for p in out.iter_mut() {
                if let MessagePart::Tool {
                    id: pid,
                    is_error: e,
                    resolved,
                    ..
                } = p
                    && pid == id
                {
                    *e = *is_error;
                    *resolved = true;
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
        AgentEvent::AssistantMessageCompleted { .. }
        | AgentEvent::Usage { .. }
        | AgentEvent::Diagnostic { .. } => {}
    }
}

/// Render-only privacy policy — strip heavy/sensitive tool inputs before a call enters the doc.
///
/// Keeps: command / path / pattern / url / query / todo items / server+tool names.
/// Drops: WriteFile content, EditFile old/new strings, WebFetch prompt, Mcp/Unknown input.
/// Full inputs remain only in the host's local run journal. Idempotent.
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
}
