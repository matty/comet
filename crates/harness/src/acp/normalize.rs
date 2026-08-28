//! Wire → `AgentEvent`, and prompt `stopReason` → `DoneStatus`.
//!
//! Pure functions over `serde_json::Value`, deliberately: every decode here is
//! testable against a literal frame the way `pickers.rs`'s `decode_models_reply`
//! is, without standing up a child process. Round-tripping through a Rust type
//! would let a reshaped wire stay green.

use comet_proto::{AgentEvent, DoneStatus};
use serde_json::Value;

/// ACP's terminal `stopReason`, mapped onto Comet's three outcomes.
///
/// **The unknown arm is `Completed`, not `Errored`.** A reason this build has
/// never heard of still describes a turn that *ended*; calling it an error puts
/// a failure in front of the user for the sole reason that Comet is older than
/// the agent. The upstream vocabulary is `end_turn`, `cancelled`, `refusal`,
/// `max_tokens`, `max_turn_requests`.
pub(crate) fn done_status(stop_reason: &str) -> DoneStatus {
    match stop_reason {
        "cancelled" => DoneStatus::Interrupted,
        "refusal" => DoneStatus::Errored,
        _ => DoneStatus::Completed,
    }
}

/// A `session/update` notification's payload → zero or one `AgentEvent`.
///
/// `None` means "nothing Comet renders", which is a real answer and not a
/// failure: ACP carries update kinds this build does not consume, and an
/// unrecognized one must be dropped quietly rather than surfaced or errored.
pub(crate) fn session_update(params: &Value) -> Option<AgentEvent> {
    let update = &params["update"];
    match update["sessionUpdate"].as_str()? {
        "agent_message_chunk" => {
            text_of(&update["content"]).map(|text| AgentEvent::TextDelta { text })
        }
        "agent_thought_chunk" => {
            text_of(&update["content"]).map(|text| AgentEvent::ReasoningDelta { text })
        }
        _ => None,
    }
}

/// A content block's text. ACP blocks are `{type, text}`, but a block whose
/// `type` is not `text` (an image, say) legitimately carries no text — so this
/// returns `Option` rather than an empty string, and an empty string is
/// likewise treated as nothing to render.
fn text_of(content: &Value) -> Option<String> {
    let text = content["text"].as_str()?;
    (!text.is_empty()).then(|| text.to_owned())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// Break caught: mapping an unknown `stopReason` to `Errored`, which shows
    /// the user a failure whose only cause is Comet being older than the agent.
    #[test]
    fn an_unknown_stop_reason_completes_rather_than_errors() {
        assert_eq!(done_status("end_turn"), DoneStatus::Completed);
        assert_eq!(done_status("max_tokens"), DoneStatus::Completed);
        assert_eq!(done_status("a_reason_from_2027"), DoneStatus::Completed);
        assert_eq!(done_status("cancelled"), DoneStatus::Interrupted);
        assert_eq!(done_status("refusal"), DoneStatus::Errored);
    }

    #[test]
    fn message_and_thought_chunks_map_to_their_own_deltas() {
        let msg = json!({"update": {
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": "hello"},
        }});
        assert!(matches!(
            session_update(&msg),
            Some(AgentEvent::TextDelta { text }) if text == "hello"
        ));

        let thought = json!({"update": {
            "sessionUpdate": "agent_thought_chunk",
            "content": {"type": "text", "text": "thinking"},
        }});
        assert!(matches!(
            session_update(&thought),
            Some(AgentEvent::ReasoningDelta { text }) if text == "thinking"
        ));
    }

    /// Break caught: an unrecognized update kind returning a rendered event, or
    /// panicking. Both are wrong — ACP carries kinds this build does not read,
    /// and the honest answer is to drop them.
    #[test]
    fn unreadable_updates_are_dropped_not_surfaced() {
        for payload in [
            json!({"update": {"sessionUpdate": "tool_call", "content": {"text": "x"}}}),
            json!({"update": {"sessionUpdate": "plan"}}),
            // A kind that does not exist at all.
            json!({"update": {"sessionUpdate": "invented_in_2027"}}),
            // No `sessionUpdate` key whatsoever.
            json!({"update": {}}),
            // No `update` key whatsoever.
            json!({}),
        ] {
            assert!(session_update(&payload).is_none(), "must drop: {payload}");
        }
    }

    /// A text block whose text is absent or empty is nothing to render. The
    /// absent case is the one a fixture would never produce, so it is written
    /// here deliberately rather than left to the wire to demonstrate.
    #[test]
    fn a_chunk_with_no_text_yields_no_event() {
        for content in [
            json!({"type": "image", "data": "..."}),
            json!({"type": "text", "text": ""}),
            json!({}),
        ] {
            let frame = json!({"update": {
                "sessionUpdate": "agent_message_chunk",
                "content": content,
            }});
            assert!(session_update(&frame).is_none(), "must drop: {frame}");
        }
    }
}
