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
pub const PROTOCOL_VERSION: u32 = 3;

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
    fn connection_state_wire_names_are_stable() {
        assert_eq!(
            serde_json::to_value(RemoteConnectionState::IdentityChanged).unwrap(),
            serde_json::json!("identityChanged")
        );
    }
}
