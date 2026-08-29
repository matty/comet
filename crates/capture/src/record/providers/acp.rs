use serde_json::{Value, json};

use crate::Provider;
use crate::record::provider::CaptureProvider;
use crate::record::scenarios::ScenarioInput;
use crate::record::session::Session;

/// Holds the JSON-RPC id counter. Same reason as Codex's: the handshake
/// spends the first id and a scenario body keeps drawing more.
pub(in crate::record) struct AcpProvider {
    next_id: u64,
}

impl AcpProvider {
    pub(in crate::record) fn new() -> Self {
        Self { next_id: 1 }
    }

    pub(in crate::record) fn next_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }
}

/// A JSON-RPC 2.0 request line. Identical in shape to Codex's — both wires are
/// newline-framed JSON-RPC 2.0, which is why `jsonrpc.rs` is shared — but kept
/// as its own function because the two providers' `next_id` counters are
/// separate and a shared helper would invite passing the wrong one.
pub(in crate::record) fn rpc_request(id: u64, method: &str, params: Value) -> String {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string()
}

/// The `initialize` params the handshake sends — **production's**, so the
/// corpus records the handshake Comet really performs rather than one the
/// recorder invented. See [`comet_harness::acp::initialize_params`].
pub(in crate::record) use comet_harness::acp::initialize_params;

impl CaptureProvider for AcpProvider {
    const NAME: &'static str = "acp";

    fn provider() -> Provider {
        Provider::Acp
    }

    fn frame(line: &str) -> Option<Value> {
        serde_json::from_str(line).ok()
    }

    /// `initialize` and await the reply. **There is no `initialized`
    /// notification** — that is Codex's app-server, not ACP, and sending one
    /// here would put a frame on the wire no agent asked for. Verified against
    /// both adapters: each answers `initialize` and is immediately ready for
    /// `session/new`.
    ///
    /// Client capabilities are declined (no `fs`, no `terminal`), matching what
    /// Comet's own harness can honor: the engine owns file access, and handing
    /// an agent a filesystem channel it could use behind the engine's back is
    /// the opposite of this repository's authority model.
    async fn handshake(session: &mut Session<Self>, _input: &ScenarioInput) -> anyhow::Result<()> {
        let id = session.provider.next_id();
        session
            .send(&rpc_request(id, "initialize", initialize_params()))
            .await?;
        session
            .wait_for("JSON-RPC reply", |frame| {
                (frame["id"].as_u64() == Some(id)).then_some(())
            })
            .await
    }

    /// ACP ends a turn with the **response** to `session/prompt`, which carries
    /// a `stopReason` — not with a notification the way Codex ends one with
    /// `turn/completed`. Anything with a `result.stopReason` is therefore
    /// terminal regardless of which id it answers; the session loop already
    /// matches ids, and a second prompt cannot be in flight because a turn is
    /// awaited to completion before the next one is sent.
    fn turn_complete(frame: &Value) -> bool {
        frame["result"]["stopReason"].is_string()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// Break caught: reading turn-end off a `method` the way Codex does leaves
    /// `wait_for_turn_end` hanging forever on ACP, whose terminal frame is a
    /// response and carries no method at all.
    #[test]
    fn turn_complete_reads_the_prompt_response_not_a_notification() {
        for reason in ["end_turn", "cancelled", "refusal", "max_tokens"] {
            assert!(
                AcpProvider::turn_complete(&json!({"id": 7, "result": {"stopReason": reason}})),
                "{reason} must be terminal"
            );
        }
        // A notification never ends an ACP turn, however terminal it looks.
        assert!(!AcpProvider::turn_complete(
            &json!({"method": "session/update"})
        ));
        assert!(!AcpProvider::turn_complete(
            &json!({"method": "turn/completed"})
        ));
        // A reply that is not the prompt's reply has no stopReason.
        assert!(!AcpProvider::turn_complete(
            &json!({"id": 1, "result": {"sessionId": "s-1"}})
        ));
        // A non-string stopReason is not a stop reason.
        assert!(!AcpProvider::turn_complete(
            &json!({"id": 7, "result": {"stopReason": null}})
        ));
    }

    #[test]
    fn next_id_is_monotonic_starting_at_one() {
        let mut provider = AcpProvider::new();
        assert_eq!(provider.next_id(), 1);
        assert_eq!(provider.next_id(), 2);
        assert_eq!(provider.next_id(), 3);
    }

    /// Break caught: the handshake advertising a client capability Comet cannot
    /// honor. Asserts on `initialize_params()` itself — the value the handshake
    /// sends — because the previous version of this test rebuilt the shape
    /// locally and passed while the real request said `terminal: true`.
    #[test]
    fn initialize_params_decline_fs_and_terminal() {
        let params = initialize_params();
        assert_eq!(params["protocolVersion"], 1);
        assert_eq!(params["clientCapabilities"]["terminal"], false);
        assert_eq!(params["clientCapabilities"]["fs"]["readTextFile"], false);
        assert_eq!(params["clientCapabilities"]["fs"]["writeTextFile"], false);
    }

    /// ACP has no `initialized` notification; sending one would put a frame on
    /// the wire no agent asked for, and it would be recorded as evidence that
    /// Comet speaks a protocol it does not.
    #[test]
    fn the_handshake_request_is_initialize_and_nothing_else() {
        let line = rpc_request(1, "initialize", initialize_params());
        let parsed: Value = serde_json::from_str(&line).expect("request is JSON");
        assert_eq!(parsed["method"], "initialize");
        assert_eq!(parsed["jsonrpc"], "2.0");
        assert_eq!(parsed["id"], 1);
    }
}
