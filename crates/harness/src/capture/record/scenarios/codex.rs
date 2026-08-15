use anyhow::bail;
use serde_json::Value;

use crate::capture::record::providers::codex::{CodexProvider, rpc_request};
use crate::capture::record::scenarios::ScenarioInput;
use crate::capture::record::session::Session;

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
