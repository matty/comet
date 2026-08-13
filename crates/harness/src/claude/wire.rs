//! Claude CLI stream-json wire frames (stdout JSONL + stdin lines).
//!
//! Tolerant by construction: every field defaults, and unclaimed frame types
//! split three ways — [`Frame::Ignored`] (recognized, deliberately dropped)
//! or [`Frame::Unknown`] (a diagnostic) — so a newer CLI never breaks parsing
//! and nothing vanishes silently (spec: docs/research/harness.md).
//!
//! Corpus claims: `claude-routine-frame-ignore-list`,
//! `claude-approval-wire-fields`, `claude-attachment-block-order`,
//! `claude-attachment-block-order-test`, and
//! `claude-tool-use-result-present`.

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
    ("system/task_started", "4.2"),
    ("system/task_progress", "4.2"),
    ("system/task_updated", "4.2"),
    ("system/task_notification", "4.2"),
    ("system/commands_changed", "2.4"),
    ("system/permission_denied", "phase-1"),
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
    /// that does not appear here at all. Landed alone, with only its decode
    /// test reading it; slice 4.2 task 3 is the first production consumer,
    /// normalizing it per tool kind.
    #[serde(default)]
    #[allow(dead_code)]
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
    use crate::capture::selected_payload;

    fn corpus_payload(claim_id: &str) -> String {
        selected_payload(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus"),
            claim_id,
        )
        .expect("reviewed corpus frame")
    }

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

    #[test]
    fn user_line_with_images_is_blocks_then_text() {
        let captured = corpus_payload("claude-attachment-block-order-test");
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
        ] {
            match parse_frame(raw).expect("parses") {
                Frame::Ignored(r) => assert_eq!(r, reason, "{raw}"),
                other => panic!("{raw} should be Ignored({reason}), got {other:?}"),
            }
        }
    }

    #[test]
    fn a_can_use_tool_request_decodes_the_fields_a_card_needs() {
        let line = corpus_payload("claude-approval-wire-fields");
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

    #[test]
    fn a_user_frame_decodes_the_captured_tool_use_result() {
        // Literal captured payload, per AGENTS.md: the SDK's typings declare
        // camelCase toolUseResult, but the wire sends snake_case, and a
        // hand-composed fixture would not catch the wrong spelling.
        let line = corpus_payload("claude-tool-use-result-present");
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
