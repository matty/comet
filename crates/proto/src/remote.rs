use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

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
        let (host, port) = value
            .split_once(':')
            .ok_or_else(|| "endpoint must be host:port".to_string())?;
        if host.is_empty() || port.contains(':') {
            return Err("endpoint must be host:port".to_string());
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
