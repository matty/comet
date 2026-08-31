use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Exact-match gate for LAN pairing (`manager.rs` refuses any peer whose
/// `ServerHello.protocol_version` differs). Bump rule, stated in
/// ARCHITECTURE.md: a new FIELD on an existing wire type stays additive and
/// does not bump; a new VARIANT of an enum that crosses the RPC boundary
/// inside a decoded container (e.g. `MessagePart` in a `TranscriptFrame`)
/// DOES bump, because the container decode is all-or-nothing and the
/// receiver has no tolerant arm. A new RPC method needs no bump — an older
/// peer answers `UnknownMethod`.
///
/// 2: `MessagePart::Notice`.
/// 3: `MessagePart::Approval`.
/// 4: the permission axis became user-selectable (slice 1.8). Not a decode
///    problem — `runtimeMode` was always an additive field — but an older peer
///    ignores the key and runs the turn under *its* default, so a user who
///    picked `approval-required` here would get an unattended write on the
///    other device. The absence of the field is indistinguishable from a
///    deliberate value, so refusing the pairing is the honest failure.
/// 5: `ListModels` answers `{models, source}` instead of a bare `Vec<Model>`.
///    A decode problem, unlike 4: the reply is a whole-value decode, so an
///    older peer fails the array-vs-object shape outright rather than
///    ignoring a key.
/// 6: `MessagePart::Subagent` (subagent attribution, slice 4.2). A decode
///    problem, same shape as 2 and 3: this variant crosses the RPC transcript
///    stream inside `TranscriptFrame`, and `MessagePart`'s decode is
///    all-or-nothing — an older peer fails the WHOLE transcript rather than
///    skipping the one part it does not recognize.
/// 7: `MessagePart::Checklist` (the plan/todo checklist, slice 4.3). Same
///    mechanism as 6 exactly — a new `MessagePart` variant on the transcript
///    stream. Note what does NOT appear in this list: `ToolCall::Todo` stopped
///    being produced in the same slice, and that needed no bump, because
///    ceasing to SEND a variant every peer already decodes breaks nobody. The
///    variant is still decoded, for documents written before the change.
/// 8: `RunRequest.harness` became command-plane intent for claim-on-first-
///    command. The field is additive and old persisted requests safely decode
///    it as absent, but an older LAN peer would silently ignore a user's
///    provider choice and execute under its own default. Same safety class as
///    4: refusing the pairing is more honest than running the wrong provider.
/// 9: `HarnessId::Grok`. A decode problem of the same class as 2, 3, 6 and 7,
///    and the reason is worth stating because `HarnessId` does not look like a
///    transcript type: it rides `HarnessDescriptor.id` in every `ListHarnesses`
///    reply, `Chat.harness` in every workspace row, and `RunRequest.harness`
///    (see 8). Five sibling enums in `agent.rs` carry `#[serde(other)]` and each
///    says why; **`HarnessId` deliberately does not**, so an older peer meeting
///    `"grok"` fails the whole containing value rather than the one field —
///    losing a harness list, or a chat row, not a label.
///
///    Note `HarnessId::Cursor` already exists with no harness behind it. It
///    predates this list and is **not** precedent that variants are free: it has
///    simply never been sent, and a variant nobody emits breaks nobody.
/// 10: `HarnessId::Hermes`. The same decode problem as 9, on the same fields:
///     `HarnessId` still carries no `#[serde(other)]` arm, so an older peer
///     meeting `"hermes"` fails the whole containing value — a harness list,
///     a chat row, or a `RunRequest` — not one field.
/// 11: `ApprovalDecision::DenyAndInterrupt`. This decision crosses the command
///     plane in `SessionCommandPayload::RespondApproval` and the transcript
///     stream inside `MessagePart::Approval`. Both containing values decode
///     all-or-nothing, so an older peer cannot skip the unfamiliar variant.
/// 12: `ApprovalRequest::Mcp.arguments`. The field is additive, but an older
///     client silently ignores both the argument preview and identity while
///     still offering its v11 "Allow for this session" action. The new host
///     would enforce that grant against the exact digest, but the user would
///     have granted an argument set the old client never showed. Refusing the
///     pair preserves informed approval instead of treating exact enforcement
///     as a substitute for a visible request.
/// 13: `DoneStatus::Expired`. The new status crosses inside `AgentEvent::Done`,
///     so an older peer fails the containing event rather than learning why an
///     unattended turn ended. D29's additive client hello rides the same
///     release but does not itself require the bump: a new server defaults a
///     missing hello to supervising, while an old server ignoring an
///     administrative declaration preserves the old safe behavior.
pub const PROTOCOL_VERSION: u32 = 13;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ServerId(String);

impl ServerId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerRef {
    pub server_id: ServerId,
    pub local_id: String,
}

impl ServerRef {
    pub fn new(server_id: ServerId, local_id: impl Into<String>) -> Self {
        Self {
            server_id,
            local_id: local_id.into(),
        }
    }

    pub fn local_id(&self) -> &str {
        &self.local_id
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteEndpoint {
    pub host: String,
    pub port: u16,
}

impl RemoteEndpoint {
    pub fn parse(value: &str) -> Result<Self, String> {
        let (host, port) = if let Some(bracketed) = value.strip_prefix('[') {
            let (host, port) = bracketed
                .split_once("]:")
                .ok_or_else(|| "IPv6 endpoints must be [address]:port".to_string())?;
            if port.contains(':') {
                return Err("IPv6 endpoints must be [address]:port".into());
            }
            host.parse::<std::net::Ipv6Addr>()
                .map_err(|_| "bracketed host must be a valid IPv6 address".to_string())?;
            (host, port)
        } else {
            let (host, port) = value
                .rsplit_once(':')
                .ok_or_else(|| "endpoint must be host:port".to_string())?;
            if host.contains(':') {
                return Err("IPv6 endpoints must be [address]:port".into());
            }
            (host, port)
        };
        if host.is_empty()
            || host.chars().any(char::is_whitespace)
            || host.contains(['/', '[', ']'])
        {
            return Err("endpoint must be host:port".to_string());
        }
        if !value.starts_with('[') {
            validate_unbracketed_host(host)?;
        }
        let port = port
            .parse::<u16>()
            .map_err(|_| "endpoint port must be a number from 1 to 65535".to_string())?;
        if port == 0 {
            return Err("endpoint port must be nonzero".to_string());
        }
        Ok(Self {
            host: host.to_string(),
            port,
        })
    }
}

fn validate_unbracketed_host(host: &str) -> Result<(), String> {
    if host.parse::<std::net::Ipv4Addr>().is_ok() {
        return Ok(());
    }
    if host
        .chars()
        .all(|character| character.is_ascii_digit() || character == '.')
    {
        return Err("endpoint host is not a valid IPv4 address".into());
    }
    if host.len() > 253 {
        return Err("DNS host must be at most 253 characters".into());
    }
    for label in host.split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err("DNS labels must contain 1 to 63 characters".into());
        }
        if !label
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            || !label.as_bytes()[0].is_ascii_alphanumeric()
            || !label.as_bytes()[label.len() - 1].is_ascii_alphanumeric()
        {
            return Err("DNS labels must use alphanumerics with internal hyphens only".into());
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteEntry {
    pub server_id: ServerId,
    pub endpoint: RemoteEndpoint,
    pub name: String,
    pub pinned_spki_sha256: String,
    pub protocol_version: u32,
    pub last_state: RemoteConnectionState,
    pub created_at: DateTime<Utc>,
    pub last_connected_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LanSettings {
    pub enabled: bool,
    pub bind: std::net::SocketAddr,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrustedClient {
    pub server_id: ServerId,
    pub name: String,
    pub pinned_spki_sha256: String,
    pub paired_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RemoteConnectionState {
    Connecting,
    Online,
    Offline,
    Unreachable { message: String },
    IdentityChanged,
    IncompatibleVersion { remote: u32 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerHello {
    pub protocol_version: u32,
    pub server_id: ServerId,
    pub device_id: String,
    pub name: String,
    pub capabilities: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_requires_host_and_nonzero_port() {
        assert!(RemoteEndpoint::parse("host.local:27655").is_ok());
        assert!(RemoteEndpoint::parse("192.168.1.20:27655").is_ok());
        assert!(RemoteEndpoint::parse("host.local:0").is_err());
        assert!(RemoteEndpoint::parse("https://host.local:27655").is_err());
    }

    #[test]
    fn endpoint_accepts_bracketed_ipv6_and_rejects_ambiguous_or_whitespace_hosts() {
        assert_eq!(
            RemoteEndpoint::parse("[fe80::1]:27655").unwrap(),
            RemoteEndpoint {
                host: "fe80::1".into(),
                port: 27655,
            }
        );
        assert!(RemoteEndpoint::parse("fe80::1:27655").is_err());
        assert!(RemoteEndpoint::parse(" buildbox.local:27655").is_err());
        assert!(RemoteEndpoint::parse("build box.local:27655").is_err());
        assert!(RemoteEndpoint::parse("[]:27655").is_err());
    }

    #[test]
    fn endpoint_rejects_malformed_ipv6_and_dns_labels() {
        for invalid in [
            "[not-ipv6]:27655",
            "[12345::1]:27655",
            "bad_name.local:27655",
            "-leading.local:27655",
            "trailing-.local:27655",
            "empty..label:27655",
            "999.2.3.4:27655",
        ] {
            assert!(
                RemoteEndpoint::parse(invalid).is_err(),
                "accepted {invalid}"
            );
        }
        let long_label = "a".repeat(64);
        assert!(RemoteEndpoint::parse(&format!("{long_label}.local:27655")).is_err());
        let long_host = format!("{}.com:27655", "a.".repeat(126));
        assert!(RemoteEndpoint::parse(&long_host).is_err());

        for valid in [
            "localhost:27655",
            "build-box.local:27655",
            "192.168.1.20:27655",
            "[2001:db8::1]:27655",
        ] {
            assert!(RemoteEndpoint::parse(valid).is_ok(), "rejected {valid}");
        }
    }

    #[test]
    fn server_refs_do_not_collide() {
        let a = ServerRef::new(ServerId::new("sha256:a"), "chat-1");
        let b = ServerRef::new(ServerId::new("sha256:b"), "chat-1");
        assert_ne!(a, b);
        assert_eq!(a.local_id(), "chat-1");
    }

    #[test]
    fn expired_done_status_requires_protocol_version_thirteen() {
        assert_eq!(PROTOCOL_VERSION, 13);
    }

    #[test]
    fn connection_state_wire_names_are_stable() {
        assert_eq!(
            serde_json::to_value(RemoteConnectionState::IdentityChanged).unwrap(),
            serde_json::json!("identityChanged")
        );
    }
}
