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

pub(crate) mod approval;
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
/// blocks, never a bare string — the text block always leads, and a staged
/// image attachment rides as a further `{"type": "image", ...}` block when the
/// agent advertised `promptCapabilities.image` (gated by the caller, not
/// here: this builder sends whatever `images` it is given).
///
/// `images` reuses `crate::claude::wire::ImageBlock` rather than a second
/// image type — one reading, staging and size-limiting policy
/// (`crate::claude::load_image_blocks`), not a duplicate.
pub(crate) fn prompt_params(
    session_id: &str,
    text: &str,
    images: &[crate::claude::wire::ImageBlock],
) -> Value {
    let mut blocks = vec![json!({"type": "text", "text": text})];
    blocks.extend(
        images.iter().map(
            |image| json!({"type": "image", "data": image.data, "mimeType": image.media_type}),
        ),
    );
    json!({
        "sessionId": session_id,
        "prompt": blocks,
    })
}

/// The `session/load` params — used instead of [`new_session_params`] to
/// resume a harness-native session id, and only when the agent advertised
/// `agentCapabilities.loadSession` (see [`AgentDescription::supports_load_session`]).
/// Shape pinned against the ACP org's own reference schema
/// (`agent-client-protocol` PyPI package, `LoadSessionRequest`:
/// `{cwd, mcpServers, sessionId}`) — a source read, not a capture, since
/// resuming a session was never exercised live on this machine. `mcpServers`
/// is empty and explicit for the same reason [`new_session_params`]'s is.
pub(crate) fn load_session_params(session_id: &str, cwd: &str) -> Value {
    json!({"sessionId": session_id, "cwd": cwd, "mcpServers": []})
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
    /// `agentCapabilities.loadSession`. Unlike `steering`, absent and `false`
    /// collapse to the same behavior here: either way there is no `session/load`
    /// handler on the other end, and sending one is a protocol error the user
    /// sees for a feature they never asked for (PR7's task brief, Step 2) — so
    /// a plain `bool` rather than an `Option<bool>` is the honest shape.
    pub supports_load_session: bool,
    /// `agentCapabilities.promptCapabilities.image`. `None` when the agent said
    /// nothing — kept tri-state (unlike `supports_load_session`) because a
    /// discovered model's own `accepts_images` reads `None` as "images work by
    /// the 2.1 default" (`crates/proto`), and collapsing an unread handshake to
    /// `false` here would misreport an agent that simply didn't say.
    pub image_attachments: Option<bool>,
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
            supports_load_session: result["agentCapabilities"]["loadSession"]
                .as_bool()
                .unwrap_or(false),
            image_attachments: result["agentCapabilities"]["promptCapabilities"]["image"].as_bool(),
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

    /// Whether a staged attachment may ride the prompt as an image content
    /// block. **Absent means no**, the same reasoning as [`Self::supports_load_session`]:
    /// an unadvertised capability has no handler on the other end, and the
    /// path ref in the prompt text (always sent, regardless) is what keeps the
    /// model informed either way.
    pub fn supports_image_attachments(&self) -> bool {
        self.image_attachments == Some(true)
    }
}

// The live session loop (`session/new`, `session/prompt`, the update stream)
// lives in `session.rs`, not here -- it is `pub(crate)` on purpose, and its
// caller is the `Harness` impl each of `grok.rs` and `hermes.rs` gives it
// (`GrokHarness`/`HermesHarness`, both calling `session::run`), not something
// still pending. `CARGO_BIN_EXE_*` reaches integration tests only, so the
// live wire behavior itself is exercised there (`acp_turn.rs` and friends)
// rather than by a unit test in this module.

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
        assert!(
            agent.supports_load_session,
            "codex-acp advertises loadSession: true"
        );
        assert!(
            agent.supports_image_attachments(),
            "codex-acp advertises promptCapabilities.image: true"
        );
    }

    /// Break caught: reading absent `loadSession` as `true` (would send
    /// `session/load` to an agent with no handler, a protocol error the user
    /// sees for a feature they never asked for) or `false` as ambiguous (there
    /// is nothing to preserve by keeping it tri-state, unlike `steering`).
    #[test]
    fn load_session_support_is_a_plain_bool_defaulting_to_false() {
        let absent = AgentDescription::from_initialize(&json!({}));
        assert!(!absent.supports_load_session);

        let off = AgentDescription::from_initialize(
            &json!({"agentCapabilities": {"loadSession": false}}),
        );
        assert!(!off.supports_load_session);

        let on =
            AgentDescription::from_initialize(&json!({"agentCapabilities": {"loadSession": true}}));
        assert!(on.supports_load_session);
    }

    /// **Absent stays unknown, not false**, unlike `loadSession` — a
    /// discovered model's own `accepts_images` reads `None` as "images work by
    /// the 2.1 default" elsewhere, and collapsing an unread handshake to
    /// `false` here would misreport an agent that simply didn't say.
    #[test]
    fn image_attachment_support_stays_tri_state() {
        let silent = AgentDescription::from_initialize(&json!({}));
        assert_eq!(silent.image_attachments, None);
        assert!(!silent.supports_image_attachments());

        let off = AgentDescription::from_initialize(&json!({
            "agentCapabilities": {"promptCapabilities": {"image": false}},
        }));
        assert_eq!(off.image_attachments, Some(false));
        assert!(!off.supports_image_attachments());

        let on = AgentDescription::from_initialize(&json!({
            "agentCapabilities": {"promptCapabilities": {"image": true}},
        }));
        assert_eq!(on.image_attachments, Some(true));
        assert!(on.supports_image_attachments());
    }

    /// `session/load`'s params, pinned against the ACP org's own
    /// `LoadSessionRequest` shape (this module's doc comment on
    /// [`load_session_params`]) — `cwd`, an explicit empty `mcpServers`, and
    /// the id being resumed.
    #[test]
    fn a_load_session_names_the_resumed_id_and_declares_no_mcp_servers() {
        let params = load_session_params("prior-session-1", "/tmp/x");
        assert_eq!(params["sessionId"], "prior-session-1");
        assert_eq!(params["cwd"], "/tmp/x");
        assert_eq!(params["mcpServers"].as_array().map(Vec::len), Some(0));
    }

    /// Break caught: dropping an image attachment on the floor, or sending it
    /// ahead of the text block (ACP has no ordering requirement, but "text
    /// leads" is the invariant every fixture and capture assumes).
    #[test]
    fn a_prompt_with_images_carries_the_text_block_first_then_each_image() {
        let images = vec![
            crate::claude::wire::ImageBlock {
                media_type: "image/png".into(),
                data: "AAA".into(),
            },
            crate::claude::wire::ImageBlock {
                media_type: "image/jpeg".into(),
                data: "BBB".into(),
            },
        ];
        let params = prompt_params("s-1", "look at this", &images);
        let blocks = params["prompt"].as_array().expect("prompt is an array");
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["text"], "look at this");
        assert_eq!(blocks[1]["type"], "image");
        assert_eq!(blocks[1]["mimeType"], "image/png");
        assert_eq!(blocks[1]["data"], "AAA");
        assert_eq!(blocks[2]["type"], "image");
        assert_eq!(blocks[2]["mimeType"], "image/jpeg");
        assert_eq!(blocks[2]["data"], "BBB");
    }
}
