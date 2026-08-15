use serde_json::Value;

use crate::capture::Provider;
use crate::capture::record::provider::CaptureProvider;
use crate::capture::record::scenarios::ScenarioInput;
use crate::capture::record::session::Session;

/// Claude's control-channel initialize handshake, verbatim from
/// `recording.rs`'s `CLAUDE_INITIALIZE_LINE`. Shared by every Claude
/// discovery scenario: model discovery and command discovery differ only in
/// which launch builder they use (see `scenarios::claude::*_launch`), never
/// in this line.
const CLAUDE_INITIALIZE_LINE: &str = r#"{"type":"control_request","request_id":"comet-discovery-1","request":{"subtype":"initialize"}}"#;

pub(in crate::capture::record) struct ClaudeProvider;

impl CaptureProvider for ClaudeProvider {
    const NAME: &'static str = "claude";

    fn provider() -> Provider {
        Provider::Claude
    }

    fn frame(line: &str) -> Option<Value> {
        serde_json::from_str(line).ok()
    }

    async fn handshake(session: &mut Session<Self>, _input: &ScenarioInput) -> anyhow::Result<()> {
        session.send(CLAUDE_INITIALIZE_LINE).await?;
        session
            .wait_for("initialize reply", |frame| {
                (frame["type"] == "control_response").then_some(())
            })
            .await
    }

    fn turn_complete(frame: &Value) -> bool {
        frame["type"] == "result"
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// Break caught: `turn_complete` stops treating a `result` frame as
    /// terminal, which would make `wait_for_turn_end` hang on any live run.
    #[test]
    fn turn_complete_is_exactly_the_result_frame() {
        assert!(ClaudeProvider::turn_complete(
            &json!({"type": "result", "subtype": "success"})
        ));
        assert!(!ClaudeProvider::turn_complete(
            &json!({"type": "assistant"})
        ));
        assert!(!ClaudeProvider::turn_complete(&json!({"type": "system"})));
    }

    #[test]
    fn frame_parses_json_and_tolerates_garbage() {
        assert_eq!(
            ClaudeProvider::frame(r#"{"type":"system"}"#),
            Some(json!({"type": "system"}))
        );
        assert_eq!(ClaudeProvider::frame("not json"), None);
    }
}
