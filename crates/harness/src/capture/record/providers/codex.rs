use std::path::Path;

use anyhow::anyhow;
use serde_json::{Value, json};

use crate::capture::Provider;
use crate::capture::record::provider::CaptureProvider;
use crate::capture::record::scenarios::ScenarioInput;
use crate::capture::record::session::Session;
use crate::launch::LaunchDescriptor;

/// Codex's `initialized` notification, verbatim from `recording.rs`'s
/// `CODEX_INITIALIZED_LINE`. Sent once, right after the `initialize` reply,
/// before anything scenario-specific.
const CODEX_INITIALIZED_LINE: &str = r#"{"jsonrpc":"2.0","method":"initialized"}"#;

/// Holds the JSON-RPC id counter: every Codex request on the wire needs a
/// fresh, monotonic id, and a scenario body (paging `model/list`, starting a
/// thread, starting a turn) keeps drawing more of them after the handshake
/// spends the first one.
pub(in crate::capture::record) struct CodexProvider {
    next_id: u64,
}

impl CodexProvider {
    pub(in crate::capture::record) fn new() -> Self {
        Self { next_id: 1 }
    }

    pub(in crate::capture::record) fn next_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }
}

/// A JSON-RPC 2.0 request line, shared by the handshake and every scenario
/// body that talks to Codex's app-server.
pub(in crate::capture::record) fn rpc_request(id: u64, method: &str, params: Value) -> String {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string()
}

impl CaptureProvider for CodexProvider {
    const NAME: &'static str = "codex";

    fn provider() -> Provider {
        Provider::Codex
    }

    fn launch(input: &ScenarioInput, executable: &Path) -> anyhow::Result<LaunchDescriptor> {
        let home = input
            .codex_home
            .clone()
            .or_else(crate::codex::discovery::codex_home)
            .ok_or_else(|| {
                anyhow!("Codex home could not be found. Pass --codex-home and try again.")
            })?;
        let home = super::super::session::absolute_from_parent(home)?;
        let cwd = input.cwd.clone().unwrap_or_else(std::env::temp_dir);
        Ok(crate::codex::discovery::discovery_launch(
            executable, &home, &cwd,
        ))
    }

    fn frame(line: &str) -> Option<Value> {
        serde_json::from_str(line).ok()
    }

    async fn handshake(session: &mut Session<Self>, _input: &ScenarioInput) -> anyhow::Result<()> {
        let id = session.provider.next_id();
        let params = json!({
            "clientInfo": {
                "name": "comet-native",
                "title": "Comet",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "capabilities": {"experimentalApi": true},
        });
        session.send(&rpc_request(id, "initialize", params)).await?;
        session
            .wait_for("JSON-RPC reply", |frame| {
                (frame["id"].as_u64() == Some(id)).then_some(())
            })
            .await?;
        session.send(CODEX_INITIALIZED_LINE).await
    }

    fn turn_complete(frame: &Value) -> bool {
        matches!(
            frame["method"].as_str(),
            Some("turn/completed" | "turn/failed" | "turn/aborted")
        )
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// Break caught: `turn_complete` drops one of the three terminal turn
    /// notifications, which would make `wait_for_turn_end` hang on a failed
    /// or aborted turn.
    #[test]
    fn turn_complete_is_exactly_the_three_terminal_turn_notifications() {
        for method in ["turn/completed", "turn/failed", "turn/aborted"] {
            assert!(
                CodexProvider::turn_complete(&json!({"method": method})),
                "{method} must be terminal"
            );
        }
        assert!(!CodexProvider::turn_complete(
            &json!({"method": "turn/started"})
        ));
        assert!(!CodexProvider::turn_complete(
            &json!({"method": "item/started"})
        ));
    }

    #[test]
    fn next_id_is_monotonic_starting_at_one() {
        let mut provider = CodexProvider::new();
        assert_eq!(provider.next_id(), 1);
        assert_eq!(provider.next_id(), 2);
        assert_eq!(provider.next_id(), 3);
    }
}
