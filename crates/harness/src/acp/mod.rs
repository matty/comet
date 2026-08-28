//! Agent Client Protocol v1 over a child agent's stdio.
//!
//! ACP is newline-framed JSON-RPC 2.0, the same wire shape as the Codex
//! app-server, so this rides the shared [`crate::jsonrpc`] client rather than
//! growing a second framing implementation.
//!
//! **This module is for agents built ground-up on ACP.** Claude and Codex keep
//! their native drivers here, which is where upstream landed after converting
//! everything to ACP and then putting the native drivers back (`60887f79`). The
//! adapter compensations that conversion needed — the Claude cost-frame settle
//! in particular — are deliberately absent: they existed because an *adapter*
//! dropped prompt replies, and are not ACP facts.
//!
//! Wire types are read tolerantly off `serde_json::Value` rather than through
//! generated bindings, matching house style: a field this build does not know
//! must not fail the frame, and `normalize.rs` holds the decisions about what
//! an unreadable one means.

// Landing ahead of its consumer, deliberately. The `Harness` impl that
// constructs these is coupled to a `HarnessId` variant and so to a
// `PROTOCOL_VERSION` bump; splitting that out keeps both changes reviewable.
// What is here is the decode, and every decision in it is pinned by a test
// against the literal wire two real adapters answered on 2026-08-28.
#![allow(dead_code)]

pub mod grok;
pub mod hermes;
pub(crate) mod normalize;
pub mod session;

use serde_json::{Value, json};

/// The ACP protocol version this client speaks. All three recorded agents
/// answered `1` to a `1` request when probed on 2026-08-28 — codex-acp 1.7.0,
/// claude-agent-acp 0.70.0 and grok 1.0.5.
pub const PROTOCOL_VERSION: u64 = 1;

/// The `initialize` params, as one value both the live harness and the capture
/// recorder send.
///
/// **Shared on purpose, the same way `launch.rs` is.** A capture is evidence of
/// what Comet puts on the wire; a recorder with its own hand-copied handshake
/// would be evidence of the recorder. The precedent is `codex::approval`'s
/// `decision_literal`, which `capture/record/scenarios/codex.rs` calls for
/// exactly this reason.
///
/// A free function rather than a literal inline in the handshake, so a test can
/// pin the bytes that actually go out: an earlier version of that test rebuilt
/// the shape itself and passed happily while the real request advertised
/// `terminal: true`.
///
/// **Client capabilities are declined.** The engine owns file access, and
/// handing an agent a filesystem or terminal channel it could use behind the
/// engine's back is the opposite of this repository's authority model.
pub fn initialize_params() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "clientInfo": {
            "name": "comet-native",
            "title": "Comet",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "clientCapabilities": {
            "fs": {"readTextFile": false, "writeTextFile": false},
            "terminal": false,
        },
    })
}

/// The `session/new` params. `mcpServers` is empty and explicit: Comet does not
/// hand an agent MCP servers of its own, and omitting the key entirely is a
/// different statement from declaring none.
pub fn new_session_params(cwd: &str) -> Value {
    json!({"cwd": cwd, "mcpServers": []})
}

/// The `session/prompt` params. ACP carries a prompt as an array of content
/// blocks, never a bare string — a single text block is the whole of what Comet
/// sends today, and attachments would arrive as further blocks.
pub fn prompt_params(session_id: &str, text: &str) -> Value {
    json!({
        "sessionId": session_id,
        "prompt": [{"type": "text", "text": text}],
    })
}

/// What `initialize` told us about the agent.
///
/// Every field is an *observation*, so each is optional in the honest sense:
/// absent means "this agent did not say", never "false". `steering` is the one
/// that matters most — Hermes advertises no steering extension at all, and
/// reading its absence as "steering is off" versus "unknown" is the difference
/// between a correct turn-boundary fallback and a silently dropped steer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentDescription {
    pub name: Option<String>,
    pub version: Option<String>,
    /// `_meta.steering.supported`. `None` when the agent said nothing.
    pub steering: Option<bool>,
    /// `authMethods[].id`. Empty is a real answer — claude-agent-acp returns
    /// `[]` while codex-acp returns two — and is not the same as absent.
    pub auth_methods: Vec<String>,
}

impl AgentDescription {
    fn from_initialize(result: &Value) -> Self {
        Self {
            name: result["agentInfo"]["name"].as_str().map(str::to_owned),
            version: result["agentInfo"]["version"].as_str().map(str::to_owned),
            steering: result["_meta"]["steering"]["supported"].as_bool(),
            auth_methods: result["authMethods"]
                .as_array()
                .map(|methods| {
                    methods
                        .iter()
                        .filter_map(|m| m["id"].as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default(),
        }
    }

    /// Whether a steer can ride the agent's own extension.
    ///
    /// **Absent means no**, and that is a decision rather than a default: an
    /// agent that never advertised the extension has no handler for it, so
    /// sending one loses the steer silently. Falling back to a turn boundary is
    /// slower and correct.
    pub fn supports_steering(&self) -> bool {
        self.steering == Some(true)
    }
}

// The live session loop (`session/new`, `session/prompt`, the update stream)
// lands with the `Harness` impl that gives it a caller. Putting it here first
// would be a module nothing constructs -- dead code the compiler is right to
// warn about, and untestable besides: `CARGO_BIN_EXE_*` reaches integration
// tests only, and this module is `pub(crate)` on purpose.

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// Break caught: reading an absent `_meta.steering` as `false`. The two are
    /// different — Hermes advertises nothing at all — and only one of them is
    /// safe to act on without a turn-boundary fallback.
    #[test]
    fn absent_steering_is_unknown_not_false() {
        let silent = AgentDescription::from_initialize(&json!({"agentInfo": {"name": "hermes"}}));
        assert_eq!(
            silent.steering, None,
            "absent must not decode as Some(false)"
        );
        assert!(
            !silent.supports_steering(),
            "unknown must not enable steering"
        );

        let off = AgentDescription::from_initialize(
            &json!({"_meta": {"steering": {"supported": false}}}),
        );
        assert_eq!(off.steering, Some(false));
        assert!(!off.supports_steering());

        let on =
            AgentDescription::from_initialize(&json!({"_meta": {"steering": {"supported": true}}}));
        assert_eq!(on.steering, Some(true));
        assert!(on.supports_steering());
    }

    /// An empty `authMethods` is a real answer (claude-agent-acp sends `[]`),
    /// and must not be confused with the key being missing.
    #[test]
    fn auth_methods_distinguish_empty_from_absent() {
        let empty = AgentDescription::from_initialize(&json!({"authMethods": []}));
        assert!(empty.auth_methods.is_empty());

        let absent = AgentDescription::from_initialize(&json!({}));
        assert!(absent.auth_methods.is_empty());

        let two = AgentDescription::from_initialize(&json!({
            "authMethods": [{"id": "api-key"}, {"id": "chat-gpt"}],
        }));
        assert_eq!(two.auth_methods, vec!["api-key", "chat-gpt"]);
    }

    /// The real codex-acp `initialize` reply, captured 2026-08-28. Pinned
    /// against the literal wire rather than a Rust round-trip: a reshaped reply
    /// must fail here, which a type-mediated test would not catch.
    #[test]
    fn the_captured_codex_acp_initialize_reply_decodes() {
        let result = json!({
            "protocolVersion": 1,
            "agentInfo": {
                "name": "@agentclientprotocol/codex-acp",
                "title": "Codex",
                "version": "1.7.0",
            },
            "agentCapabilities": {
                "auth": {"logout": {}},
                "loadSession": true,
                "promptCapabilities": {"embeddedContext": true, "image": true},
                "sessionCapabilities": {"resume": {}, "list": {}, "subagents": {}},
            },
            "authMethods": [
                {"id": "api-key", "name": "API Key"},
                {"id": "chat-gpt", "name": "ChatGPT"},
            ],
            "_meta": {"steering": {"supported": true}},
        });
        let agent = AgentDescription::from_initialize(&result);
        assert_eq!(
            agent.name.as_deref(),
            Some("@agentclientprotocol/codex-acp")
        );
        assert_eq!(agent.version.as_deref(), Some("1.7.0"));
        assert!(agent.supports_steering());
        assert_eq!(agent.auth_methods, vec!["api-key", "chat-gpt"]);
    }
}
