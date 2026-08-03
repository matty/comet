use std::path::Path;
use std::time::Duration;

use anyhow::{Context, bail};
use chrono::{DateTime, Utc};
use clap::Subcommand;
use comet_engine::{InstanceLock, RemoteConfigStore};
use comet_proto::{
    LanSettings, RemoteConnectionState, RemoteEndpoint, RemoteEntry, ServerId, TrustedClient,
};
use comet_rpc::{RpcClient, TlsIdentity, connect_ws, methods, pair_client_zeroizing};
use data_encoding::{BASE32_NOPAD, HEXLOWER};
use serde::Deserialize;
use zeroize::Zeroizing;

#[derive(Debug, Subcommand)]
pub enum RemoteCommand {
    /// Pair this Comet with another instance at HOST:PORT.
    Add {
        endpoint: String,
        #[arg(long)]
        name: Option<String>,
    },
    /// List directly configured Comet instances.
    List,
    /// Remove a configured remote by stable server id.
    Remove { server_id: String },
    /// Enable or disable incoming LAN connections.
    Listen {
        #[arg(long, conflicts_with = "disable")]
        enable: bool,
        #[arg(long, conflicts_with = "enable")]
        disable: bool,
        #[arg(long)]
        bind: Option<std::net::SocketAddr>,
    },
    /// Start a five-minute pairing session for another Comet instance.
    Pair,
    /// List clients trusted to connect to this instance.
    Clients,
    /// Revoke a trusted client by stable server id.
    Revoke { server_id: String },
}

pub async fn run(command: RemoteCommand, data_dir: &Path, ipc_port: u16) -> anyhow::Result<()> {
    match command {
        RemoteCommand::Add { endpoint, name } => add(data_dir, ipc_port, &endpoint, name).await,
        RemoteCommand::List => {
            let remotes = read_remotes(data_dir, ipc_port).await?;
            print!("{}", render_remote_rows(&remotes));
            Ok(())
        }
        RemoteCommand::Remove { server_id } => {
            remove(data_dir, ipc_port, parse_server_id(&server_id)?).await
        }
        RemoteCommand::Listen {
            enable,
            disable,
            bind,
        } => listen(data_dir, ipc_port, enable, disable, bind).await,
        RemoteCommand::Pair => pair(data_dir, ipc_port).await,
        RemoteCommand::Clients => {
            let clients = read_clients(data_dir, ipc_port).await?;
            print!("{}", render_clients(&clients));
            Ok(())
        }
        RemoteCommand::Revoke { server_id } => {
            revoke(data_dir, ipc_port, parse_server_id(&server_id)?).await
        }
    }
}

pub async fn status(data_dir: &Path, ipc_port: u16) -> anyhow::Result<()> {
    println!("Data dir: {}", data_dir.display());
    if let Some(client) = local_client(ipc_port).await {
        println!("Engine:   running");
        println!("IPC:      listening on 127.0.0.1:{ipc_port}");
        let lan = client
            .call(methods::GET_LAN_SETTINGS, serde_json::Value::Null)
            .await?;
        println!("LAN:      {}", render_lan_value(&lan));
        let remotes: Vec<RemoteEntry> = watch_first(&client, methods::WATCH_REMOTES).await?;
        let clients: Vec<TrustedClient> =
            watch_first(&client, methods::WATCH_TRUSTED_CLIENTS).await?;
        println!("Clients:  {} paired", clients.len());
        print!("{}", render_remote_rows(&remotes));
        return Ok(());
    }

    if let Some(pid) = InstanceLock::holder(data_dir) {
        println!("Engine:   running (pid {pid})");
        println!("IPC:      unavailable on 127.0.0.1:{ipc_port}");
        println!("LAN:      unavailable while the running engine IPC is unreachable");
        return Ok(());
    }
    let _lock = acquire_offline_lock(data_dir)?;
    let store = RemoteConfigStore::open(data_dir)?;
    println!("Engine:   not running");
    println!("IPC:      not listening on 127.0.0.1:{ipc_port}");
    let settings = store.lan_settings();
    println!(
        "LAN:      engine offline (configured {}; bind {})",
        if settings.enabled {
            "enabled"
        } else {
            "disabled"
        },
        settings.bind
    );
    let clients = store.watch_trusted_clients().borrow().clone();
    println!("Clients:  {} paired", clients.len());
    let mut remotes = store.watch_remotes().borrow().clone();
    mark_offline(&mut remotes);
    print!("{}", render_remote_rows(&remotes));
    Ok(())
}

async fn add(
    data_dir: &Path,
    ipc_port: u16,
    endpoint_text: &str,
    name: Option<String>,
) -> anyhow::Result<()> {
    let endpoint = RemoteEndpoint::parse(endpoint_text).map_err(anyhow::Error::msg)?;
    let entered = Zeroizing::new(rpassword::prompt_password("Pairing secret: ")?);
    let secret = decode_pairing_secret(&entered)?;
    let identity = comet_identity::DeviceIdentity::load_or_create(data_dir)?;
    let tls = TlsIdentity::from_device_identity(&identity)?;
    let address = endpoint_address(&endpoint);

    if let Some(client) = local_client(ipc_port).await {
        let pinned = pair_client_zeroizing(address, &tls, secret).await?;
        let entry = paired_entry(endpoint, name, &pinned);
        client
            .call(methods::PUT_REMOTE, serde_json::to_value(&entry)?)
            .await
            .with_context(|| paired_but_not_saved(&entry))?;
        println!("Added {}.", entry.name);
        return Ok(());
    }

    let _lock = acquire_offline_lock(data_dir)?;
    let store = RemoteConfigStore::open(data_dir)?;
    let pinned = pair_client_zeroizing(address, &tls, secret).await?;
    let entry = paired_entry(endpoint, name, &pinned);
    store
        .put_remote(entry.clone())
        .with_context(|| paired_but_not_saved(&entry))?;
    println!("Added {}.", entry.name);
    Ok(())
}

fn paired_entry(
    endpoint: RemoteEndpoint,
    name: Option<String>,
    pinned: &comet_rpc::PinnedServer,
) -> RemoteEntry {
    RemoteEntry {
        server_id: pinned.server_id().clone(),
        name: name
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| endpoint.host.clone()),
        endpoint,
        pinned_spki_sha256: HEXLOWER.encode(pinned.spki_sha256()),
        protocol_version: 0,
        last_state: RemoteConnectionState::Connecting,
        created_at: Utc::now(),
        last_connected_at: None,
    }
}

fn paired_but_not_saved(entry: &RemoteEntry) -> String {
    format!(
        "pairing succeeded with {} ({}) but this computer could not save it; revoke this client on the remote, then pair again",
        entry.name,
        server_id_text(&entry.server_id)
    )
}

async fn remove(data_dir: &Path, ipc_port: u16, server_id: ServerId) -> anyhow::Result<()> {
    if let Some(client) = local_client(ipc_port).await {
        let reply = client
            .call(
                methods::REMOVE_REMOTE,
                serde_json::json!({ "serverId": server_id }),
            )
            .await?;
        print_removed(&reply, "remote")?;
        return Ok(());
    }
    let _lock = acquire_offline_lock(data_dir)?;
    let removed = RemoteConfigStore::open(data_dir)?.remove_remote(&server_id)?;
    if !removed {
        bail!("remote registry row not found");
    }
    println!("Removed remote {}.", server_id_text(&server_id));
    Ok(())
}

async fn listen(
    data_dir: &Path,
    ipc_port: u16,
    enable: bool,
    disable: bool,
    bind: Option<std::net::SocketAddr>,
) -> anyhow::Result<()> {
    if let Some(client) = local_client(ipc_port).await {
        let current = client
            .call(methods::GET_LAN_SETTINGS, serde_json::Value::Null)
            .await?;
        if !enable && !disable && bind.is_none() {
            println!("{}", render_lan_value(&current));
            return Ok(());
        }
        let mut settings: LanSettings = serde_json::from_value(current["settings"].clone())?;
        if enable {
            settings.enabled = true;
        }
        if disable {
            settings.enabled = false;
        }
        if let Some(bind) = bind {
            settings.bind = bind;
        }
        client
            .call(methods::SET_LAN_SETTINGS, serde_json::to_value(settings)?)
            .await?;
        println!("Remote connections updated.");
        return Ok(());
    }
    let _lock = acquire_offline_lock(data_dir)?;
    let store = RemoteConfigStore::open(data_dir)?;
    let mut settings = store.lan_settings();
    if !enable && !disable && bind.is_none() {
        println!(
            "Configured {} on {} (engine offline).",
            if settings.enabled {
                "enabled"
            } else {
                "disabled"
            },
            settings.bind
        );
        return Ok(());
    }
    if enable {
        settings.enabled = true;
    }
    if disable {
        settings.enabled = false;
    }
    if let Some(bind) = bind {
        settings.bind = bind;
    }
    store.set_lan_settings(settings)?;
    println!("Remote connections updated; start Comet to apply the listener setting.");
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BeginPairingReply {
    secret: String,
    expires_at: DateTime<Utc>,
}

async fn pair(_data_dir: &Path, ipc_port: u16) -> anyhow::Result<()> {
    let client = local_client(ipc_port).await.ok_or_else(|| {
        anyhow::anyhow!(
            "pairing requires a running local Comet engine with remote connections enabled"
        )
    })?;
    let lan = client
        .call(methods::GET_LAN_SETTINGS, serde_json::Value::Null)
        .await?;
    if lan["settings"]["enabled"] != serde_json::Value::Bool(true)
        || lan["status"]["state"] != serde_json::Value::String("listening".into())
    {
        bail!("remote connections are not listening; run `comet remote listen --enable` first");
    }
    let reply: BeginPairingReply = client
        .call_as(methods::BEGIN_PAIRING, serde_json::Value::Null)
        .await?;
    let secret = Zeroizing::new(reply.secret);
    println!("Pairing secret: {}", secret.as_str());
    println!("Expires: {}", reply.expires_at.to_rfc3339());
    println!(
        "Anyone with this one-time secret can control this Comet instance; share it only with the intended device."
    );
    Ok(())
}

async fn revoke(data_dir: &Path, ipc_port: u16, server_id: ServerId) -> anyhow::Result<()> {
    if let Some(client) = local_client(ipc_port).await {
        let reply = client
            .call(
                methods::REVOKE_TRUSTED_CLIENT,
                serde_json::json!({ "serverId": server_id }),
            )
            .await?;
        print_removed(&reply, "trusted client")?;
        return Ok(());
    }
    let _lock = acquire_offline_lock(data_dir)?;
    let removed = RemoteConfigStore::open(data_dir)?.revoke_client(&server_id)?;
    if !removed {
        bail!("trusted client not found");
    }
    println!("Revoked trusted client {}.", server_id_text(&server_id));
    Ok(())
}

fn print_removed(reply: &serde_json::Value, kind: &str) -> anyhow::Result<()> {
    if reply["removed"] != serde_json::Value::Bool(true) {
        bail!("{kind} not found");
    }
    println!("Removed {kind}.");
    Ok(())
}

async fn read_remotes(data_dir: &Path, ipc_port: u16) -> anyhow::Result<Vec<RemoteEntry>> {
    if let Some(client) = local_client(ipc_port).await {
        return watch_first(&client, methods::WATCH_REMOTES).await;
    }
    let _lock = acquire_offline_lock(data_dir)?;
    let mut remotes = RemoteConfigStore::open(data_dir)?
        .watch_remotes()
        .borrow()
        .clone();
    mark_offline(&mut remotes);
    Ok(remotes)
}

async fn read_clients(data_dir: &Path, ipc_port: u16) -> anyhow::Result<Vec<TrustedClient>> {
    if let Some(client) = local_client(ipc_port).await {
        return watch_first(&client, methods::WATCH_TRUSTED_CLIENTS).await;
    }
    let _lock = acquire_offline_lock(data_dir)?;
    Ok(RemoteConfigStore::open(data_dir)?
        .watch_trusted_clients()
        .borrow()
        .clone())
}

async fn watch_first<T: serde::de::DeserializeOwned>(
    client: &RpcClient,
    method: &str,
) -> anyhow::Result<T> {
    let mut stream = client.subscribe(method, serde_json::Value::Null).await?;
    let value = stream
        .recv()
        .await
        .ok_or_else(|| anyhow::anyhow!("{method} closed before its initial snapshot"))?;
    Ok(serde_json::from_value(value)?)
}

async fn local_client(ipc_port: u16) -> Option<RpcClient> {
    tokio::time::timeout(
        Duration::from_millis(750),
        connect_ws(&format!("ws://127.0.0.1:{ipc_port}")),
    )
    .await
    .ok()
    .and_then(Result::ok)
}

fn acquire_offline_lock(data_dir: &Path) -> anyhow::Result<InstanceLock> {
    std::fs::create_dir_all(data_dir)?;
    InstanceLock::acquire(data_dir).map_err(|error| {
        anyhow::anyhow!("the Comet engine is running but local RPC is unavailable; refusing an offline configuration access: {error}")
    })
}

fn decode_pairing_secret(encoded: &str) -> anyhow::Result<Zeroizing<[u8; 16]>> {
    let compact = Zeroizing::new(
        encoded
            .chars()
            .filter(|character| !character.is_whitespace() && *character != '-')
            .flat_map(char::to_uppercase)
            .collect::<String>(),
    );
    let decoded = Zeroizing::new(
        BASE32_NOPAD
            .decode(compact.as_bytes())
            .map_err(|_| anyhow::anyhow!("pairing secret must be grouped Base32 text"))?,
    );
    let bytes: [u8; 16] = decoded
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("pairing secret must encode exactly 128 bits"))?;
    Ok(Zeroizing::new(bytes))
}

fn parse_server_id(value: &str) -> anyhow::Result<ServerId> {
    let value = value.trim();
    if value.is_empty() {
        bail!("a stable server id is required");
    }
    Ok(ServerId::new(value))
}

fn server_id_text(server_id: &ServerId) -> String {
    serde_json::to_value(server_id)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "<invalid-server-id>".into())
}

fn endpoint_address(endpoint: &RemoteEndpoint) -> String {
    if endpoint.host.contains(':') {
        format!("[{}]:{}", endpoint.host, endpoint.port)
    } else {
        format!("{}:{}", endpoint.host, endpoint.port)
    }
}

fn render_remote_rows(remotes: &[RemoteEntry]) -> String {
    if remotes.is_empty() {
        return "Remotes:  none configured\n".into();
    }
    let mut output = String::from("Remotes:\n");
    for remote in remotes {
        output.push_str(&format!(
            "  {}  {}  {}  {}\n",
            remote.name,
            server_id_text(&remote.server_id),
            endpoint_address(&remote.endpoint),
            state_label(&remote.last_state)
        ));
    }
    output
}

fn mark_offline(remotes: &mut [RemoteEntry]) {
    for remote in remotes {
        if matches!(
            remote.last_state,
            RemoteConnectionState::Online | RemoteConnectionState::Connecting
        ) {
            remote.last_state = RemoteConnectionState::Offline;
        }
    }
}

fn state_label(state: &RemoteConnectionState) -> String {
    match state {
        RemoteConnectionState::Connecting => "connecting".into(),
        RemoteConnectionState::Online => "online".into(),
        RemoteConnectionState::Offline => "offline".into(),
        RemoteConnectionState::Unreachable { message } => format!("unreachable ({message})"),
        RemoteConnectionState::IdentityChanged => "identity changed".into(),
        RemoteConnectionState::IncompatibleVersion { remote } => {
            format!("incompatible protocol {remote}")
        }
    }
}

fn render_clients(clients: &[TrustedClient]) -> String {
    if clients.is_empty() {
        return "Trusted clients: none\n".into();
    }
    let mut output = String::from("Trusted clients:\n");
    for client in clients {
        output.push_str(&format!(
            "  {}  {}  paired {}\n",
            client.name,
            server_id_text(&client.server_id),
            client.paired_at.to_rfc3339()
        ));
    }
    output
}

fn render_lan_value(value: &serde_json::Value) -> String {
    match value["status"]["state"].as_str() {
        Some("listening") => format!(
            "listening on {}",
            value["status"]["bind"].as_str().unwrap_or("unknown")
        ),
        Some("bindFailed") => format!(
            "bind failed on {}: {}",
            value["status"]["bind"].as_str().unwrap_or("unknown"),
            value["status"]["error"].as_str().unwrap_or("unknown error")
        ),
        _ if value["settings"]["enabled"] == serde_json::Value::Bool(true) => {
            "enabled, not listening".into()
        }
        _ => "disabled".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use comet_proto::{RemoteConnectionState, RemoteEndpoint, RemoteEntry, ServerId};

    #[test]
    fn pairing_secret_decoder_accepts_grouped_text_without_retaining_the_input() {
        let decoded = decode_pairing_secret("AEBA-GBAF-AYDQ-QCIK-BMGA-2DQP-CA").unwrap();
        assert_eq!(
            decoded.as_ref(),
            &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]
        );
        assert!(decode_pairing_secret("AAAA").is_err());
    }

    #[test]
    fn remote_rows_include_stable_id_endpoint_and_offline_state() {
        let rows = render_remote_rows(&[RemoteEntry {
            server_id: ServerId::new("sha256:remote-a"),
            endpoint: RemoteEndpoint::parse("buildbox.local:27655").unwrap(),
            name: "Build box".into(),
            pinned_spki_sha256: "pin".into(),
            protocol_version: 1,
            last_state: RemoteConnectionState::Offline,
            created_at: Utc::now(),
            last_connected_at: None,
        }]);
        assert!(rows.contains("Build box"));
        assert!(rows.contains("sha256:remote-a"));
        assert!(rows.contains("buildbox.local:27655"));
        assert!(rows.contains("offline"));
    }

    #[test]
    fn remove_and_revoke_require_nonempty_stable_ids() {
        assert!(parse_server_id("").is_err());
        assert!(parse_server_id("sha256:remote-a").is_ok());
    }

    #[test]
    fn an_offline_engine_never_reports_a_stale_remote_as_online() {
        let mut remotes = vec![RemoteEntry {
            server_id: ServerId::new("sha256:remote-a"),
            endpoint: RemoteEndpoint::parse("buildbox.local:27655").unwrap(),
            name: "Build box".into(),
            pinned_spki_sha256: "pin".into(),
            protocol_version: 1,
            last_state: RemoteConnectionState::Online,
            created_at: Utc::now(),
            last_connected_at: Some(Utc::now()),
        }];
        mark_offline(&mut remotes);
        assert_eq!(remotes[0].last_state, RemoteConnectionState::Offline);
    }
}
