use std::path::Path;

use anyhow::{anyhow, bail};
use serde_json::Value;

use crate::capture::record::providers::codex::{CodexProvider, rpc_request};
use crate::capture::record::scenarios::ScenarioInput;
use crate::capture::record::session::Session;
use crate::launch::LaunchDescriptor;

/// SPAWN for every Codex discovery row (`model-discovery` and its
/// `-neutral-cwd`/`-project-cwd`/`-logged-out` aliases): the same launch,
/// varying only by which `cwd`/`codex_home` the row's `ScenarioInput`
/// carries.
pub(in crate::capture::record) fn model_discovery_launch(
    input: &ScenarioInput,
    executable: &Path,
) -> anyhow::Result<LaunchDescriptor> {
    let home = input
        .codex_home
        .clone()
        .or_else(crate::codex::discovery::codex_home)
        .ok_or_else(|| {
            anyhow!("Codex home could not be found. Pass --codex-home and try again.")
        })?;
    let home = crate::capture::record::session::absolute_from_parent(home)?;
    let cwd = input.cwd.clone().unwrap_or_else(std::env::temp_dir);
    Ok(crate::codex::discovery::discovery_launch(
        executable, &home, &cwd,
    ))
}

/// The cursor-paginated `model/list` loop, run after `record()`'s generic
/// handshake. Every page is recorded — `recording.rs:488-506`'s loop, moved
/// unchanged.
pub(in crate::capture::record) async fn model_discovery(
    session: &mut Session<CodexProvider>,
    _input: &ScenarioInput,
) -> anyhow::Result<()> {
    let mut cursor: Option<String> = None;
    for _ in 0..20_u64 {
        let id = session.provider.next_id();
        session
            .send(&codex_model_list_line(id, cursor.as_deref()))
            .await?;
        let reply = session
            .wait_for("JSON-RPC reply", |value| {
                (value["id"].as_u64() == Some(id)).then(|| value.clone())
            })
            .await?;
        cursor = reply["result"]["nextCursor"].as_str().map(str::to_owned);
        if cursor.is_none() {
            return Ok(());
        }
    }
    bail!("Codex returned too many model pages. Update the CLI or retry the capture later.")
}

fn codex_model_list_line(id: u64, cursor: Option<&str>) -> String {
    let mut params = serde_json::Map::new();
    if let Some(cursor) = cursor {
        params.insert("cursor".into(), cursor.into());
    }
    rpc_request(id, "model/list", Value::Object(params))
}
