//! Claude CLI stream-json wire frames (stdout JSONL + stdin lines).
//!
//! Tolerant by construction: every field defaults, and unclaimed frame types
//! split three ways — [`Frame::Ignored`] (recognized, deliberately dropped)
//! or [`Frame::Unknown`] (a diagnostic) — so a newer CLI never breaks parsing
//! and nothing vanishes silently (spec: docs/research/harness.md).
//!
//! Reviewed evidence lives in `tests/corpus`, addressed by scenario and frame
//! sequence.

use serde::Deserialize;
use serde_json::{Value, json};

/// One parsed stdout line.
#[derive(Debug)]
pub(crate) enum Frame {
    System(SystemFrame),
    /// An allowlisted `system` notice subtype (see [`NOTICE_SUBTYPES`]).
    SystemNotice(SystemNoticeFrame),
    StreamEvent(StreamEventFrame),
    Assistant(MessageFrame),
    User(MessageFrame),
    /// One of the four claimed `system/task_*` subtypes (subagent lifecycle):
    /// `task_started`, `task_progress`, `task_updated`, `task_notification`.
    /// One tolerant struct across all four — see [`SubagentTaskFrame`].
    SubagentTask(SubagentTaskFrame),
    RateLimit(RateLimitFrame),
    Result(ResultFrame),
    ControlRequest(ControlRequestFrame),
    /// Recognized and deliberately dropped — on [`IGNORED_FRAMES`], with a
    /// one-word reason naming the owner ("4.2") or the nature ("transient").
    Ignored(&'static str),
    /// On neither the claimed nor the ignored list: slice 0b.2's diagnostic.
    Unknown {
        discriminator: String,
    },
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct SystemFrame {
    #[serde(default)]
    pub subtype: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub session_id: String,
}

/// The `system` subtypes claimed as notices. An explicit allowlist on
/// purpose: everything else falls to the Ignored/Unknown split in
/// [`classify_unclaimed`] — a subtype nobody claimed must surface as a
/// diagnostic, not vanish.
pub(crate) const NOTICE_SUBTYPES: &[&str] = &[
    "compact_boundary",
    "model_refusal_fallback",
    "api_retry",
    "informational",
    "notification",
];

/// The `system` subtypes claimed as subagent lifecycle frames (slice 4.2).
/// Same allowlist pattern as [`NOTICE_SUBTYPES`]: an explicit list so a
/// subtype nobody claimed still falls to the Ignored/Unknown split rather
/// than vanishing.
pub(crate) const SUBAGENT_TASK_SUBTYPES: &[&str] = &[
    "task_started",
    "task_progress",
    "task_updated",
    "task_notification",
];

/// Frames Comet recognizes and deliberately drops — the middle tier of the
/// Claimed / Ignored / Unknown classification. Reasons: a slice number
/// (e.g. `"4.2"`, `"2.4"`, `"phase-1"`) names a roadmap slice that will
/// later claim the entry and move it out of this table to Claimed, stopping
/// its count — it is a maintenance obligation, not a fact about the frame,
/// and reading only this repository will not resolve which slice that is;
/// any other reason names why no surface wants the frame at all. ★ = confirmed firing
/// in the reviewed healthy-run corpus; the rest are named by sdk.d.ts 0.3.195.
/// Deliberately NOT here, so a diagnostic fires:
/// local_command_output, model_refusal_no_fallback, mirror_error,
/// elicitation_complete, files_persisted, plugin_install, worker_shutting_down.
pub(crate) const IGNORED_FRAMES: &[(&str, &str)] = &[
    // top-level frame types
    // Still ignored on a RUN stream; the discovery session reads its own
    // (claude/discovery.rs), and nothing on a run replies to us.
    ("control_response", "reply-channel"),
    ("control_cancel_request", "control-plumbing"),
    ("keep_alive", "transport-ping"),
    ("tool_progress", "liveness"),
    ("tool_use_summary", "cosmetic"),
    ("prompt_suggestion", "opt-in"),
    ("auth_status", "auth-transient"),
    // system subtypes
    ("system/status", "transient"), // ★ one per API request
    ("system/thinking_tokens", "heartbeat"),
    ("system/session_state_changed", "turn-state"),
    ("system/hook_started", "4.2"), // ★ one per session with hooks
    ("system/hook_progress", "4.2"),
    ("system/hook_response", "4.2"), // ★
    ("system/commands_changed", "2.4"),
    ("system/permission_denied", "phase-1"),
    // Carries the live background-task set, but every task_type this frame
    // has been observed to carry (local_agent) is also fully derivable from
    // the task_* events the normalizer already emits, so no surface needs
    // this frame's copy of it.
    ("system/background_tasks_changed", "redundant"), // ★ twice per subagent run
    // Memory-feature chatter; fires routinely for memory-enabled users. No
    // planned slice owns a memory surface — the reason names that product
    // fact, not a deferral; whoever builds one claims this entry.
    ("system/memory_recall", "memory"),
];

pub(crate) fn ignored_reason(discriminator: &str) -> Option<&'static str> {
    IGNORED_FRAMES
        .iter()
        .find(|(name, _)| *name == discriminator)
        .map(|(_, reason)| *reason)
}

/// Route a frame no claimed arm took: Ignored if allowlisted, else an Unknown
/// that 0b.2 records. The full frame is warn-logged HERE — the drop site —
/// and stays local to the host; the discriminator is the only thing that
/// travels (sanitized again inside `diagnostic()`).
fn classify_unclaimed(discriminator: String, value: &Value) -> Frame {
    if let Some(reason) = ignored_reason(&discriminator) {
        return Frame::Ignored(reason);
    }
    tracing::warn!(
        target: "comet_harness::claude",
        frame = %value,
        "unrecognized frame (recorded as a diagnostic)"
    );
    Frame::Unknown { discriminator }
}

/// One tolerant struct for every allowlisted notice subtype — every field
/// defaults, consistent with this module's "tolerant by construction" header.
/// Only the fields the emitters read are declared.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct SystemNoticeFrame {
    #[serde(default)]
    pub subtype: String,
    // compact_boundary
    #[serde(default)]
    pub compact_metadata: CompactMetadata,
    // model_refusal_fallback
    #[serde(default)]
    pub original_model: String,
    #[serde(default)]
    pub fallback_model: String,
    // api_retry
    #[serde(default)]
    pub attempt: u64,
    #[serde(default)]
    pub max_retries: u64,
    #[serde(default)]
    pub retry_delay_ms: u64,
    // informational
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub level: String,
    #[serde(default)]
    pub tool_use_id: Option<String>,
    // notification
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub priority: String,
}

/// One tolerant struct across all four claimed `system/task_*` subtypes,
/// consistent with this module's "every field defaults" convention — the
/// normalizer reads only the fields each subtype actually carries. Literal
/// shapes: captures/2026-08-13-plan-todo-subagent/run2-claude-subagent.jsonl.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct SubagentTaskFrame {
    #[serde(default)]
    pub subtype: String,
    #[serde(default)]
    pub task_id: String,
    /// The parent `Agent` tool_use id. Present on `task_started`,
    /// `task_progress`, `task_notification`; absent on `task_updated`.
    #[serde(default)]
    pub tool_use_id: Option<String>,
    /// `task_started`'s description, OR `task_progress`'s LIVE activity line
    /// — the same key moves ("Read README…" → "Reading README.md").
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub subagent_type: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
    /// `task_notification`'s top-level terminal status. `task_updated`'s
    /// status lives one level down, in [`patch`](Self::patch).
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub usage: Option<SubagentUsage>,
    /// `task_updated`'s PARTIAL patch — only the field the CLI actually
    /// changed, so an absent `status` here must decode absent, never as a
    /// collapsed default.
    #[serde(default)]
    pub patch: Option<SubagentPatch>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct SubagentUsage {
    /// `None` is "not reported yet", never zero — mirrors
    /// [`comet_proto::AgentEvent::SubagentUpdated`]'s own field.
    #[serde(default)]
    pub total_tokens: Option<u64>,
    #[serde(default)]
    pub tool_uses: Option<u32>,
    #[serde(default)]
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct SubagentPatch {
    #[serde(default)]
    pub status: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct CompactMetadata {
    #[serde(default)]
    pub trigger: String,
    #[serde(default)]
    pub pre_tokens: u64,
    #[serde(default)]
    pub post_tokens: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct StreamEventFrame {
    #[serde(default)]
    pub parent_tool_use_id: Option<String>,
    #[serde(default)]
    pub event: StreamEventBody,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct StreamEventBody {
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub delta: Delta,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct Delta {
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub thinking: String,
}

/// An `assistant` or `user` frame (an Anthropic API message envelope).
#[derive(Debug, Default, Deserialize)]
pub(crate) struct MessageFrame {
    #[serde(default)]
    pub parent_tool_use_id: Option<String>,
    #[serde(default)]
    pub message: MessageBody,
    /// Terse assistant-level error code (`rate_limit`, `billing_error`, …).
    #[serde(default)]
    pub error: Option<String>,
    /// On `user` frames: the tool's own result, already typed per tool
    /// (`Bash` gets stdout/stderr, `Write` gets a diff, …). **Snake_case on
    /// the wire** — the SDK's typings declare a camelCase `toolUseResult`
    /// that does not appear here at all. Consumed by `normalize.rs`'s
    /// `subagent_result_from_tool_use_result`: the `Agent` tool's own result
    /// carries the whole subagent record on one frame, a fallback for a run
    /// that produced no `task_notification`.
    #[serde(default)]
    pub tool_use_result: Option<Value>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct MessageBody {
    #[serde(default)]
    pub role: String,
    /// Either a plain string or an array of content blocks.
    #[serde(default)]
    pub content: Value,
}

impl MessageBody {
    pub fn blocks(&self) -> impl Iterator<Item = ContentBlock> + '_ {
        self.content
            .as_array()
            .map(|a| a.as_slice())
            .unwrap_or_default()
            .iter()
            .filter_map(|b| serde_json::from_value(b.clone()).ok())
    }
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct ContentBlock {
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub input: Value,
    #[serde(default)]
    pub tool_use_id: String,
    #[serde(default)]
    pub is_error: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct RateLimitFrame {
    #[serde(default)]
    pub rate_limit_info: RateLimitInfo,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct RateLimitInfo {
    #[serde(default)]
    pub status: String,
    #[serde(rename = "rateLimitType", default)]
    pub rate_limit_type: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct ResultFrame {
    #[serde(default)]
    pub subtype: String,
    #[serde(default)]
    pub result: Option<String>,
    #[serde(default)]
    pub errors: Vec<Value>,
    #[serde(default)]
    pub usage: UsageBody,
    #[serde(default, rename = "modelUsage")]
    pub model_usage: std::collections::BTreeMap<String, ModelUsageBody>,
    #[serde(default)]
    pub session_id: Option<String>,
}

/// `input_tokens` here is the CACHE-EXCLUSIVE remainder — Anthropic's own
/// prompt-caching docs state the prompt size is the sum of all three. Reading
/// `input_tokens` alone reports single digits for a 35,000-token prompt.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct UsageBody {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
}

/// One `modelUsage` entry. The map is keyed by resolved model id and carries
/// the only context-window figure the CLI publishes; the field is on the wire
/// but in no documented field list, so treat its absence as ordinary.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ModelUsageBody {
    #[serde(default)]
    pub context_window: Option<u64>,
}

/// A CLI→client control request (`can_use_tool` is the one we act on).
#[derive(Debug, Default, Deserialize)]
pub(crate) struct ControlRequestFrame {
    #[serde(default)]
    pub request_id: String,
    #[serde(default)]
    pub request: ControlRequestBody,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) struct ControlRequestBody {
    #[serde(default)]
    pub subtype: String,
    #[serde(default)]
    pub tool_name: String,
    #[serde(default)]
    pub input: Value,
    #[serde(default)]
    pub tool_use_id: String,
    /// The CLI's own one-line rendering of the request ("hello.txt", "Write
    /// \"one\" to a.txt"). Provider prose.
    ///
    /// Decoded for exactly one consumer, and it is a test:
    /// `ApprovalRequest::Unknown` promises its summary is Comet copy and
    /// never provider prose, and the contract test in `claude/approval.rs`
    /// asserts this string does not reach the card. No production path reads
    /// it. Do not "use" it.
    #[serde(default)]
    #[allow(dead_code)]
    pub description: Option<String>,
}

/// Parse one stdout JSONL line. `Err` = not JSON; unclaimed types classify
/// via [`classify_unclaimed`].
pub(crate) fn parse_frame(line: &str) -> Result<Frame, serde_json::Error> {
    let value: Value = serde_json::from_str(line)?;
    let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
    let frame = match kind {
        "system" => {
            let subtype = value
                .get("subtype")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            match subtype.as_str() {
                "init" => Frame::System(serde_json::from_value(value)?),
                s if NOTICE_SUBTYPES.contains(&s) => {
                    Frame::SystemNotice(serde_json::from_value(value)?)
                }
                s if SUBAGENT_TASK_SUBTYPES.contains(&s) => {
                    Frame::SubagentTask(serde_json::from_value(value)?)
                }
                s => {
                    let discriminator = format!("system/{s}");
                    classify_unclaimed(discriminator, &value)
                }
            }
        }
        "stream_event" => Frame::StreamEvent(serde_json::from_value(value)?),
        "assistant" => Frame::Assistant(serde_json::from_value(value)?),
        "user" => Frame::User(serde_json::from_value(value)?),
        "rate_limit_event" => Frame::RateLimit(serde_json::from_value(value)?),
        "result" => Frame::Result(serde_json::from_value(value)?),
        "control_request" => Frame::ControlRequest(serde_json::from_value(value)?),
        kind => classify_unclaimed(kind.to_owned(), &value),
    };
    Ok(frame)
}

/// A stdin user turn: `{"type":"user","message":{...},"parent_tool_use_id":null}`.
/// Steering = another such line mid-run (consumed at a step boundary).
pub(crate) fn user_message_line(text: &str) -> String {
    json!({
        "type": "user",
        "message": { "role": "user", "content": text },
        "parent_tool_use_id": null,
    })
    .to_string()
}

/// One inline image for a stdin user turn (Anthropic base64 image source).
pub(crate) struct ImageBlock {
    /// One of the API-supported media types (png/jpeg/gif/webp).
    pub media_type: String,
    /// Raw base64 (no data-URL prefix).
    pub data: String,
}

/// A stdin user turn whose content is an array of blocks: the attached images
/// first, then the text — the standard Anthropic image+text message shape
/// (verified against the real CLI: `--input-format stream-json` accepts image
/// content blocks in user frames). Empty `images` degrades to the plain line.
pub(crate) fn user_message_line_with_images(text: &str, images: &[ImageBlock]) -> String {
    if images.is_empty() {
        return user_message_line(text);
    }
    let mut blocks: Vec<Value> = images
        .iter()
        .map(|img| {
            json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": img.media_type,
                    "data": img.data,
                },
            })
        })
        .collect();
    blocks.push(json!({ "type": "text", "text": text }));
    json!({
        "type": "user",
        "message": { "role": "user", "content": blocks },
        "parent_tool_use_id": null,
    })
    .to_string()
}

/// Success reply to a CLI control request (`can_use_tool` allow/deny payloads).
pub(crate) fn control_response_line(request_id: &str, response: Value) -> String {
    json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": request_id,
            "response": response,
        },
    })
    .to_string()
}

/// `can_use_tool` deny payload. `message` is what the model is told, and is
/// the user's own note when they wrote one.
///
/// No `updatedPermissions` is sent on either arm: Comet owns session grants
/// and must not persist them into provider or repository configuration. See
/// `comet_engine::approvals`.
pub(crate) fn deny_response(message: String) -> Value {
    json!({ "behavior": "deny", "message": message })
}

/// `can_use_tool` allow payload with the (possibly updated) tool input.
pub(crate) fn allow_response(updated_input: Value) -> Value {
    json!({ "behavior": "allow", "updatedInput": updated_input })
}

/// The reply sdk.d.ts requires for a dialog kind the host does not recognize.
/// Leaving it unanswered leaves the CLI waiting on a reply that never comes.
/// This shape is derived from the SDK typings and remains unverified against
/// a reproducible live frame.
pub(crate) fn cancelled_response() -> Value {
    json!({ "behavior": "cancelled" })
}

/// Client→CLI interrupt control request.
pub(crate) fn interrupt_request_line(request_id: &str) -> String {
    json!({
        "type": "control_request",
        "request_id": request_id,
        "request": { "subtype": "interrupt" },
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::corpus_frame;

    #[test]
    fn parses_known_and_unknown_frames() {
        let init = r#"{"type":"system","subtype":"init","model":"m","tools":["Bash"],"cwd":"/x","session_id":"s1"}"#;
        match parse_frame(init).expect("parses") {
            Frame::System(f) => {
                assert_eq!(f.subtype, "init");
                assert_eq!(f.session_id, "s1");
            }
            other => panic!("unexpected frame: {other:?}"),
        }
        assert!(matches!(
            parse_frame(r#"{"type":"mystery_frame"}"#).expect("parses"),
            Frame::Unknown { discriminator } if discriminator == "mystery_frame"
        ));
        assert!(parse_frame("not json").is_err());
    }

    #[test]
    fn user_line_shape_matches_protocol() {
        let line = user_message_line("hi");
        let v: Value = serde_json::from_str(&line).expect("json");
        assert_eq!(v["type"], "user");
        assert_eq!(v["message"]["content"], "hi");
        assert!(v["parent_tool_use_id"].is_null());
    }

    /// The literal attachment user frame orders image blocks before text.
    #[test]
    fn user_line_with_images_is_blocks_then_text() {
        let captured = corpus_frame("claude/2.1.228/attachment", 1).payload;
        let captured: Value = serde_json::from_str(&captured).expect("captured stdin JSON");
        let captured_content = captured["message"]["content"]
            .as_array()
            .expect("captured block content");
        assert_eq!(captured_content.len(), 2);
        assert_eq!(captured_content[0]["type"], "image");
        assert_eq!(captured_content[1]["type"], "text");

        let line = user_message_line_with_images(
            "what is this?",
            &[ImageBlock {
                media_type: "image/png".into(),
                data: "QUJD".into(),
            }],
        );
        let v: Value = serde_json::from_str(&line).expect("json");
        assert_eq!(v["type"], "user");
        let content = v["message"]["content"].as_array().expect("array content");
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "image");
        assert_eq!(content[0]["source"]["type"], "base64");
        assert_eq!(content[0]["source"]["media_type"], "image/png");
        assert_eq!(content[0]["source"]["data"], "QUJD");
        assert_eq!(content[1]["type"], "text");
        assert_eq!(content[1]["text"], "what is this?");
        // No images ⇒ identical to the plain string line.
        assert_eq!(
            user_message_line_with_images("hi", &[]),
            user_message_line("hi")
        );
    }

    #[test]
    fn system_subtypes_split_init_notice_ignored_and_unknown() {
        // init keeps its dedicated SystemFrame (SessionStarted decoding
        // untouched).
        assert!(matches!(
            parse_frame(r#"{"type":"system","subtype":"init","session_id":"s"}"#).unwrap(),
            Frame::System(_)
        ));
        // Allowlisted notice subtypes become SystemNotice.
        match parse_frame(
            r#"{"type":"system","subtype":"compact_boundary","compact_metadata":{"trigger":"auto","pre_tokens":68000,"post_tokens":12000}}"#,
        )
        .unwrap()
        {
            Frame::SystemNotice(f) => {
                assert_eq!(f.subtype, "compact_boundary");
                assert_eq!(f.compact_metadata.trigger, "auto");
                assert_eq!(f.compact_metadata.pre_tokens, 68000);
                assert_eq!(f.compact_metadata.post_tokens, Some(12000));
            }
            other => panic!("unexpected frame: {other:?}"),
        }
        // Everything else splits three ways — the 0b.2 classification: a
        // subtype nobody claimed is Unknown (a diagnostic)…
        assert!(matches!(
            parse_frame(r#"{"type":"system","subtype":"someFutureSubtype"}"#).unwrap(),
            Frame::Unknown { discriminator } if discriminator == "system/someFutureSubtype"
        ));
        // …while a recognized-and-deliberately-dropped one is Ignored, its
        // reason naming the owner.
        assert!(matches!(
            parse_frame(r#"{"type":"system","subtype":"status","status":"compacting"}"#).unwrap(),
            Frame::Ignored("transient")
        ));
    }

    /// The frames in the reviewed healthy-run corpus. One diagnostic from any of these puts a lie on the
    /// settings card — "hidden when zero" is only honest with this tier.
    #[test]
    fn the_ignore_list_covers_every_capture_confirmed_routine_frame() {
        for (raw, reason) in [
            (
                r#"{"type":"system","subtype":"status","status":"requesting"}"#,
                "transient",
            ),
            (
                r#"{"type":"system","subtype":"hook_started","hook":"SessionStart"}"#,
                "4.2",
            ),
            (
                r#"{"type":"system","subtype":"hook_response","hook":"SessionStart"}"#,
                "4.2",
            ),
            (
                r#"{"type":"system","subtype":"background_tasks_changed","tasks":[]}"#,
                "redundant",
            ),
        ] {
            match parse_frame(raw).expect("parses") {
                Frame::Ignored(r) => assert_eq!(r, reason, "{raw}"),
                other => panic!("{raw} should be Ignored({reason}), got {other:?}"),
            }
        }
    }

    /// A `can_use_tool` request carries request id, tool, input, description,
    /// and additional ignored fields.
    #[test]
    fn a_can_use_tool_request_decodes_the_fields_a_card_needs() {
        let line = corpus_frame("claude/2.1.228/approval", 102).payload;
        let Frame::ControlRequest(req) = parse_frame(&line).unwrap() else {
            panic!("expected a control request");
        };
        assert_eq!(req.request.subtype, "can_use_tool");
        assert_eq!(req.request.tool_name, "Write");
        assert_eq!(req.request.tool_use_id, "<TOOL_USE_ID_4>");
        assert_eq!(
            req.request.description.as_deref(),
            Some("capture-marker.txt")
        );
        // permission_suggestions and display_name are present on this captured frame and
        // deliberately undecoded. Nothing
        // reads them, and serde ignores unknown keys — so the frame parses either
        // way and declaring them would buy availability nothing consumes.
    }

    #[test]
    fn a_request_that_reports_no_description_is_not_an_error() {
        // The absent case, written by hand, per .agents/rules/optional-wire-fields.md.
        // `None` means "the CLI sent no description", NOT "the empty description" —
        // the distinction Task 3's fallback copy depends on.
        let line = r#"{"type":"control_request","request_id":"cc1cb7a8","request":{
            "subtype":"can_use_tool","tool_name":"Write",
            "input":{"file_path":"C:\\work\\hello.txt","content":"hi\n"}}}"#;
        let Frame::ControlRequest(req) = parse_frame(line).unwrap() else {
            panic!("expected a control request");
        };
        assert_eq!(req.request.description, None);

        // ...and an empty one is a different answer, not the same one.
        let empty = r#"{"type":"control_request","request_id":"x","request":{
            "subtype":"can_use_tool","tool_name":"Write","input":{},"description":""}}"#;
        let Frame::ControlRequest(req) = parse_frame(empty).unwrap() else {
            panic!("expected a control request");
        };
        assert_eq!(req.request.description.as_deref(), Some(""));
    }

    /// A captured Bash `tool_result` user frame carries a snake_case
    /// `tool_use_result` key with the tool's typed result.
    #[test]
    fn a_user_frame_decodes_the_captured_tool_use_result() {
        // Literal captured payload, per AGENTS.md: the SDK's typings declare
        // camelCase toolUseResult, but the wire sends snake_case, and a
        // hand-composed fixture would not catch the wrong spelling.
        let line = corpus_frame("claude/2.1.228/approval", 56).payload;
        let Frame::User(frame) = parse_frame(&line).unwrap() else {
            panic!("expected a user frame");
        };
        let result = frame.tool_use_result.expect("captured frame has a result");
        assert_eq!(result["stdout"], "capture");
        assert_eq!(result["interrupted"], false);
    }

    #[test]
    fn a_user_frame_without_tool_use_result_decodes_to_none() {
        // The absent case, written by hand, per .agents/rules/optional-wire-fields.md.
        let line =
            r#"{"type":"user","message":{"role":"user","content":"hi"},"parent_tool_use_id":null}"#;
        let Frame::User(frame) = parse_frame(line).unwrap() else {
            panic!("expected a user frame");
        };
        assert_eq!(frame.tool_use_result, None);
    }

    #[test]
    fn subagent_task_subtypes_decode_into_subagent_task_not_ignored() {
        // The four frames slice 4.2 task 3 claims. Literal captured shape:
        // captures/2026-08-13-plan-todo-subagent/run2-claude-subagent.jsonl.
        let started = r#"{"type":"system","subtype":"task_started","task_id":"a6d1ae6c4fec0efe9","tool_use_id":"toolu_01M553SNnGHZ1j4whxE9zWq9","description":"Read README and report first heading","subagent_type":"general-purpose","task_type":"local_agent","prompt":"Read the README.md file in the current directory and report what the first heading is. Just state the heading text, nothing else."}"#;
        match parse_frame(started).unwrap() {
            Frame::SubagentTask(f) => {
                assert_eq!(f.subtype, "task_started");
                assert_eq!(f.task_id, "a6d1ae6c4fec0efe9");
                assert_eq!(
                    f.tool_use_id.as_deref(),
                    Some("toolu_01M553SNnGHZ1j4whxE9zWq9")
                );
                assert_eq!(f.subagent_type.as_deref(), Some("general-purpose"));
                assert_eq!(
                    f.description.as_deref(),
                    Some("Read README and report first heading")
                );
                assert!(f.prompt.is_some());
            }
            other => panic!("expected SubagentTask, got {other:?}"),
        }

        let progress = r#"{"type":"system","subtype":"task_progress","task_id":"a6d1ae6c4fec0efe9","tool_use_id":"toolu_01M553SNnGHZ1j4whxE9zWq9","description":"Reading README.md","subagent_type":"general-purpose","usage":{"total_tokens":19215,"tool_uses":1,"duration_ms":2906},"last_tool_name":"Read"}"#;
        match parse_frame(progress).unwrap() {
            Frame::SubagentTask(f) => {
                assert_eq!(f.subtype, "task_progress");
                assert_eq!(f.description.as_deref(), Some("Reading README.md"));
                let usage = f.usage.expect("usage present");
                assert_eq!(usage.total_tokens, Some(19215));
                assert_eq!(usage.tool_uses, Some(1));
                assert_eq!(usage.duration_ms, Some(2906));
            }
            other => panic!("expected SubagentTask, got {other:?}"),
        }

        let updated = r#"{"type":"system","subtype":"task_updated","task_id":"a6d1ae6c4fec0efe9","patch":{"status":"completed","end_time":1786581776304}}"#;
        match parse_frame(updated).unwrap() {
            Frame::SubagentTask(f) => {
                assert_eq!(f.subtype, "task_updated");
                assert_eq!(
                    f.patch.expect("patch present").status.as_deref(),
                    Some("completed")
                );
            }
            other => panic!("expected SubagentTask, got {other:?}"),
        }

        let notification = r#"{"type":"system","subtype":"task_notification","task_id":"a6d1ae6c4fec0efe9","tool_use_id":"toolu_01M553SNnGHZ1j4whxE9zWq9","status":"completed","output_file":"C:\\tmp\\out","summary":"Sandbox","usage":{"total_tokens":20044,"tool_uses":1,"duration_ms":4906}}"#;
        match parse_frame(notification).unwrap() {
            Frame::SubagentTask(f) => {
                assert_eq!(f.subtype, "task_notification");
                assert_eq!(f.status.as_deref(), Some("completed"));
                assert_eq!(f.summary.as_deref(), Some("Sandbox"));
                let usage = f.usage.expect("usage present");
                assert_eq!(usage.total_tokens, Some(20044));
            }
            other => panic!("expected SubagentTask, got {other:?}"),
        }
    }

    /// Deleting these from `IGNORED_FRAMES` is the point of task 3: a claimed
    /// arm must never leave a stale reason lying about a frame it now handles.
    #[test]
    fn subagent_task_subtypes_are_no_longer_on_the_ignored_list() {
        for subtype in [
            "task_started",
            "task_progress",
            "task_updated",
            "task_notification",
        ] {
            assert_eq!(
                ignored_reason(&format!("system/{subtype}")),
                None,
                "system/{subtype} must be claimed, not ignored"
            );
        }
    }

    #[test]
    fn a_task_updated_partial_patch_leaves_everything_else_absent() {
        // The patch is PARTIAL — only status moved. No other field is present
        // at all, and the frame-level fields not set by task_updated
        // (description, summary, usage) must decode to None, not to a
        // collapsed default.
        let line = r#"{"type":"system","subtype":"task_updated","task_id":"t1","patch":{"status":"completed"}}"#;
        let Frame::SubagentTask(f) = parse_frame(line).unwrap() else {
            panic!("expected SubagentTask");
        };
        assert_eq!(f.description, None);
        assert_eq!(f.summary, None);
        assert!(f.usage.is_none());
        assert_eq!(f.tool_use_id, None);
    }

    #[test]
    fn an_unknown_control_subtype_still_decodes() {
        // Sink 3's input: the frame must decode so the subtype can be reported and
        // answered, even though nothing understands it.
        let line = r#"{"type":"control_request","request_id":"x","request":{"subtype":"request_user_dialog"}}"#;
        let Frame::ControlRequest(req) = parse_frame(line).unwrap() else {
            panic!("expected a control request");
        };
        assert_eq!(req.request.subtype, "request_user_dialog");
        assert_eq!(req.request.tool_name, "");
        assert_eq!(req.request.description, None);
    }
}
