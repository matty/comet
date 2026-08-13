use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{anyhow, bail};
use serde_json::{Value, json};

#[cfg(test)]
use super::common::APPROVAL_MARKER_CONTENT;
#[cfg(windows)]
use super::common::file_identity;
use super::common::{
    APPROVAL_MARKER_ADD_DIFF, APPROVAL_MARKER_NAME, CODEX_APPROVAL_COMMAND, FileIdentity,
};
#[cfg(all(test, windows))]
use super::common::{canonical_protected_roots, select_trusted_powershell};

#[derive(Default)]
pub(in crate::capture) struct CodexApprovalState {
    thread_id: Option<String>,
    turn_id: Option<String>,
    request_ids: BTreeSet<u64>,
    item_ids: BTreeSet<String>,
    pub(in crate::capture) command_items: Vec<Value>,
    pub(in crate::capture) command_completions: BTreeSet<String>,
    pub(in crate::capture) stage: u8,
    pub(in crate::capture) file_change_approvals: u8,
    file_items: BTreeMap<String, Value>,
    pub(in crate::capture) routine_items: BTreeMap<String, Value>,
}

pub(in crate::capture) fn observe_codex_approval_routine_item(
    value: &Value,
    method: &str,
    state: &mut CodexApprovalState,
) -> anyhow::Result<()> {
    require_exact_keys(
        value,
        &["method", "params", "emittedAtMs"],
        "routine item notification",
    )?;
    if value["emittedAtMs"].as_u64().is_none() {
        bail!("Codex routine item notification had no numeric emission time.");
    }
    validate_codex_approval_context(value, state)?;
    require_exact_keys(
        &value["params"],
        &[
            "item",
            "threadId",
            "turnId",
            if method == "item/started" {
                "startedAtMs"
            } else {
                "completedAtMs"
            },
        ],
        "routine item event",
    )?;
    let item = &value["params"]["item"];
    let kind = item["type"].as_str().unwrap_or_default();
    match kind {
        "userMessage" => require_keys_with_optional(
            item,
            &["type", "id", "content"],
            &["clientId"],
            "routine item",
        )?,
        "reasoning" => {
            require_exact_keys(item, &["type", "id", "summary", "content"], "routine item")?
        }
        "agentMessage" => require_exact_keys(
            item,
            &["type", "id", "text", "phase", "memoryCitation"],
            "routine item",
        )?,
        _ => bail!("Codex approval capture observed an unreviewed item type."),
    }
    let id = item["id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| anyhow!("Codex approval routine item had no nonempty identifier."))?;
    match kind {
        "userMessage" => {
            let invalid_client_id = item
                .get("clientId")
                .is_some_and(|client_id| !client_id.is_null() && !client_id.is_string());
            if invalid_client_id || !item["content"].is_array() {
                bail!("Codex approval user-message item had an unexpected value shape.");
            }
        }
        "reasoning" => {
            if !item["summary"].is_array() || !item["content"].is_array() {
                bail!("Codex approval reasoning item had an unexpected value shape.");
            }
        }
        "agentMessage" => {
            if item["text"].as_str().is_none()
                || item["phase"] != "commentary"
                || !item["memoryCitation"].is_null()
            {
                bail!("Codex approval agent-message item had an unexpected value shape.");
            }
        }
        _ => unreachable!("kind was matched above"),
    }
    if method == "item/started" {
        if (kind == "reasoning"
            && item["summary"]
                .as_array()
                .is_none_or(|summary| !summary.is_empty()))
            || (kind == "agentMessage" && item["text"] != "")
        {
            bail!("Codex approval routine item start was not in its reviewed initial state.");
        }
        if state
            .routine_items
            .insert(id.to_owned(), item.clone())
            .is_some()
        {
            bail!("Codex approval routine item repeated its start.");
        }
    } else {
        let Some(started) = state.routine_items.remove(id) else {
            bail!("Codex approval routine item completion did not join its start.");
        };
        let preserved = match kind {
            "userMessage" => {
                started["type"] == item["type"]
                    && started["id"] == item["id"]
                    && started["content"] == item["content"]
            }
            "reasoning" => {
                started["type"] == item["type"]
                    && started["id"] == item["id"]
                    && started["content"] == item["content"]
                    && item["summary"]
                        .as_array()
                        .is_some_and(|summary| !summary.is_empty())
            }
            "agentMessage" => {
                started["type"] == item["type"]
                    && started["id"] == item["id"]
                    && started["phase"] == item["phase"]
                    && started["memoryCitation"] == item["memoryCitation"]
                    && item["text"].as_str().is_some_and(|text| !text.is_empty())
            }
            _ => unreachable!("kind was matched above"),
        };
        if !preserved {
            bail!("Codex approval routine item completion changed a reviewed invariant.");
        }
    }
    Ok(())
}

fn validate_codex_approval_context(
    value: &Value,
    state: &CodexApprovalState,
) -> anyhow::Result<()> {
    let thread_id = value["params"]["threadId"]
        .as_str()
        .filter(|id| !id.is_empty());
    let turn_id = value["params"]["turnId"]
        .as_str()
        .filter(|id| !id.is_empty());
    if thread_id != state.thread_id.as_deref() || turn_id != state.turn_id.as_deref() {
        bail!("Codex approval event changed the active thread or turn identifier.");
    }
    Ok(())
}

pub(in crate::capture) fn validate_codex_approval_lifecycle(
    value: &Value,
    state: &CodexApprovalState,
) -> anyhow::Result<()> {
    require_exact_keys(
        value,
        &["method", "params", "emittedAtMs"],
        "turn lifecycle notification",
    )?;
    if value["emittedAtMs"].as_u64().is_none() {
        bail!("Codex turn lifecycle had no numeric emission time.");
    }
    require_exact_keys(&value["params"], &["threadId", "turn"], "turn lifecycle")?;
    let turn = &value["params"]["turn"];
    require_exact_keys(
        turn,
        &[
            "id",
            "items",
            "itemsView",
            "status",
            "error",
            "startedAt",
            "completedAt",
            "durationMs",
        ],
        "turn lifecycle value",
    )?;
    if value["params"]["threadId"].as_str() != state.thread_id.as_deref()
        || turn["id"].as_str() != state.turn_id.as_deref()
    {
        bail!("Codex approval lifecycle changed the active thread or turn identifier.");
    }
    if !turn["items"].is_array()
        || turn["itemsView"] != "summary"
        || turn["status"] != "completed"
        || !turn["error"].is_null()
        || turn["startedAt"].as_u64().is_none()
        || turn["completedAt"].as_u64().is_none()
        || turn["durationMs"].as_u64().is_none()
    {
        bail!("Codex approval terminal lifecycle did not match the reviewed completed shape.");
    }
    Ok(())
}

pub(in crate::capture) fn observe_codex_approval_turn_started(
    value: &Value,
    expected_thread_id: &str,
    state: &mut CodexApprovalState,
) -> anyhow::Result<String> {
    if state.thread_id.is_some() || state.turn_id.is_some() {
        bail!("Codex approval capture repeated turn/started.");
    }
    require_exact_keys(
        value,
        &["method", "params", "emittedAtMs"],
        "turn lifecycle notification",
    )?;
    if value["emittedAtMs"].as_u64().is_none() {
        bail!("Codex turn lifecycle had no numeric emission time.");
    }
    require_exact_keys(&value["params"], &["threadId", "turn"], "turn lifecycle")?;
    let turn = &value["params"]["turn"];
    require_exact_keys(
        turn,
        &[
            "id",
            "items",
            "itemsView",
            "status",
            "error",
            "startedAt",
            "completedAt",
            "durationMs",
        ],
        "turn lifecycle value",
    )?;
    let thread_id = value["params"]["threadId"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| anyhow!("Codex approval turn had no thread identifier."))?;
    let turn_id = turn["id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| anyhow!("Codex approval turn had no nonempty turn identifier."))?;
    if thread_id != expected_thread_id
        || turn["items"]
            .as_array()
            .is_none_or(|items| !items.is_empty())
        || turn["itemsView"] != "notLoaded"
        || turn["status"] != "inProgress"
        || !turn["error"].is_null()
        || turn["startedAt"].as_u64().is_none()
        || !turn["completedAt"].is_null()
        || !turn["durationMs"].is_null()
    {
        bail!("Codex approval turn/started did not match the reviewed initial shape.");
    }
    state.thread_id = Some(thread_id.to_owned());
    state.turn_id = Some(turn_id.to_owned());
    Ok(turn_id.to_owned())
}

#[derive(Default)]
pub(in crate::capture) struct CodexOnRequestState {
    request_ids: BTreeSet<u64>,
    failed_item_id: Option<String>,
    pub(in crate::capture) stage: u8,
}

fn codex_approval_ids(
    value: &Value,
    request_ids: &mut BTreeSet<u64>,
    item_ids: &mut BTreeSet<String>,
) -> anyhow::Result<(u64, String)> {
    let request_id = value["id"].as_u64().ok_or_else(|| {
        anyhow!("Codex approval request had no valid numeric request identifier.")
    })?;
    if !request_ids.insert(request_id) {
        bail!("Codex approval request repeated a request identifier.");
    }
    let item_id = value["params"]["itemId"]
        .as_str()
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| anyhow!("Codex approval request had no nonempty item identifier."))?
        .to_owned();
    if !item_ids.insert(item_id.clone()) {
        bail!("Codex approval request repeated an item identifier.");
    }
    Ok((request_id, item_id))
}

fn validate_command_action(params: &Value, expected_command: &str) -> anyhow::Result<()> {
    let actions = params["commandActions"]
        .as_array()
        .ok_or_else(|| anyhow!("Codex command approval request had no bounded command action."))?;
    let expected = json!({"type": "unknown", "command": expected_command});
    if actions.len() != 1 || actions[0] != expected {
        bail!("Codex command approval request used an unexpected command action.");
    }
    Ok(())
}

fn validate_codex_raw_launcher(raw: &str, trusted: &FileIdentity) -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        let Some(rest) = raw.strip_prefix('"') else {
            bail!("Codex command execution used an unexpected raw launcher.");
        };
        let Some((path, suffix)) = rest.split_once('"') else {
            bail!("Codex command execution used an unexpected raw launcher.");
        };
        if suffix != " -Command 'echo capture'" {
            bail!("Codex command execution used an unexpected raw launcher.");
        }
        let observed = file_identity(Path::new(path))?;
        if &observed != trusted {
            bail!("Codex command execution used an untrusted launcher identity.");
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = (raw, trusted);
        bail!(
            "Codex approval capture has no observed safe Unix launcher contract. Review real evidence and update the design before retrying."
        );
    }
}

fn validate_marker_change(item: &Value, cwd: &Path) -> anyhow::Result<()> {
    require_exact_keys(
        item,
        &["type", "id", "changes", "status"],
        "file-change item",
    )?;
    if item["type"] != "fileChange" || item["status"] != "inProgress" {
        bail!("Codex file-change approval request did not join an active file change.");
    }
    let changes = item["changes"]
        .as_array()
        .ok_or_else(|| anyhow!("Codex file-change approval request had no bounded change."))?;
    if changes.len() != 1 {
        bail!("Codex file-change approval request had an unexpected number of changes.");
    }
    let change = &changes[0];
    require_exact_keys(change, &["path", "kind", "diff"], "file-change add")?;
    let expected_path = cwd.join(APPROVAL_MARKER_NAME);
    if change["path"].as_str() != Some(expected_path.to_string_lossy().as_ref())
        || change["kind"] != json!({"type": "add"})
        || change["diff"] != APPROVAL_MARKER_ADD_DIFF
    {
        bail!("Codex file-change approval request did not match the exact bounded marker add.");
    }
    let canonical_cwd = std::fs::canonicalize(cwd)
        .map_err(|_| anyhow!("Codex file-change approval request cwd could not be validated."))?;
    let canonical_parent = expected_path
        .parent()
        .and_then(|parent| std::fs::canonicalize(parent).ok())
        .ok_or_else(|| {
            anyhow!("Codex file-change approval request marker parent could not be validated.")
        })?;
    if canonical_parent != canonical_cwd {
        bail!("Codex file-change approval request escaped the configured cwd.");
    }
    Ok(())
}

fn require_exact_keys(value: &Value, expected: &[&str], label: &str) -> anyhow::Result<()> {
    let Some(object) = value.as_object() else {
        bail!("Codex {label} was not an object.");
    };
    let actual: BTreeSet<_> = object.keys().map(String::as_str).collect();
    let expected: BTreeSet<_> = expected.iter().copied().collect();
    if actual != expected {
        bail!("Codex {label} had unexpected or missing fields.");
    }
    Ok(())
}

fn require_keys_with_optional(
    value: &Value,
    required: &[&str],
    optional: &[&str],
    label: &str,
) -> anyhow::Result<()> {
    let Some(object) = value.as_object() else {
        bail!("Codex {label} was not an object.");
    };
    let actual: BTreeSet<_> = object.keys().map(String::as_str).collect();
    let required: BTreeSet<_> = required.iter().copied().collect();
    let allowed: BTreeSet<_> = required
        .iter()
        .copied()
        .chain(optional.iter().copied())
        .collect();
    if !required.is_subset(&actual) || !actual.is_subset(&allowed) {
        bail!("Codex {label} had unexpected or missing fields.");
    }
    Ok(())
}

pub(in crate::capture) fn validate_codex_approval_command_event(
    value: &Value,
    method: &str,
    cwd: &Path,
    trusted_powershell: &FileIdentity,
    state: &mut CodexApprovalState,
) -> anyhow::Result<()> {
    require_exact_keys(
        value,
        &["method", "params", "emittedAtMs"],
        "command notification",
    )?;
    if value["emittedAtMs"].as_u64().is_none() {
        bail!("Codex command notification had no numeric emission time.");
    }
    validate_codex_approval_context(value, state)?;
    require_exact_keys(
        &value["params"],
        &[
            "item",
            "threadId",
            "turnId",
            if method == "item/started" {
                "startedAtMs"
            } else {
                "completedAtMs"
            },
        ],
        "command event",
    )?;
    let item = &value["params"]["item"];
    require_exact_keys(
        item,
        &[
            "type",
            "id",
            "pluginId",
            "scriptPath",
            "command",
            "cwd",
            "processId",
            "source",
            "status",
            "commandActions",
            "aggregatedOutput",
            "exitCode",
            "durationMs",
        ],
        "command item",
    )?;
    let id = item["id"]
        .as_str()
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| anyhow!("Codex command execution had no nonempty item identifier."))?;
    if item["type"] != "commandExecution"
        || item["cwd"].as_str() != Some(cwd.to_string_lossy().as_ref())
    {
        bail!("Codex command execution did not match the bounded cwd and item type.");
    }
    validate_command_action(item, CODEX_APPROVAL_COMMAND)?;
    let raw = item["command"]
        .as_str()
        .ok_or_else(|| anyhow!("Codex command execution had no raw launcher."))?;
    validate_codex_raw_launcher(raw, trusted_powershell)?;

    let start_at = match state.stage {
        0..=2 => Some(state.stage as usize),
        5 => Some(3),
        7 => Some(4),
        _ => None,
    };
    if method == "item/started" {
        let Some(index) = start_at else {
            bail!("Codex command executions did not match the reviewed ordering.");
        };
        if item["status"] != "inProgress"
            || item.get("exitCode").is_some_and(|code| !code.is_null())
            || state
                .command_items
                .iter()
                .any(|existing| existing["id"].as_str() == Some(id))
        {
            bail!("Codex command execution start was duplicated or malformed.");
        }
        if state.command_items.len() != index {
            bail!("Codex command executions did not match the reviewed ordering.");
        }
        state.command_items.push(item.clone());
        state.stage += 1;
        return Ok(());
    }

    let expected_index = match state.stage {
        3 => 2,
        4 => 0,
        6 => 3,
        8 => 4,
        _ => bail!("Codex command completions did not match the reviewed ordering."),
    };
    let started = state.command_items.get(expected_index).ok_or_else(|| {
        anyhow!("Codex command completion did not join a preceding bounded start.")
    })?;
    if started["id"].as_str() != Some(id)
        || item["status"] != "failed"
        || item["exitCode"].as_i64() != Some(-1)
        || started["command"] != item["command"]
        || started["cwd"] != item["cwd"]
        || started["commandActions"] != item["commandActions"]
        || !state.command_completions.insert(id.to_owned())
    {
        bail!("Codex command completion did not match its exact bounded start.");
    }
    state.stage += 1;
    Ok(())
}

pub(in crate::capture) fn observe_codex_approval_file_item(
    value: &Value,
    cwd: &Path,
    state: &mut CodexApprovalState,
) -> anyhow::Result<()> {
    require_exact_keys(
        value,
        &["method", "params", "emittedAtMs"],
        "file-change notification",
    )?;
    if value["emittedAtMs"].as_u64().is_none() {
        bail!("Codex file-change notification had no numeric emission time.");
    }
    validate_codex_approval_context(value, state)?;
    require_exact_keys(
        &value["params"],
        &["item", "threadId", "turnId", "startedAtMs"],
        "file-change start",
    )?;
    if state.stage != 9 || !state.file_items.is_empty() {
        bail!("Codex file change did not match the reviewed ordering.");
    }
    let item = &value["params"]["item"];
    validate_marker_change(item, cwd)?;
    let item_id = item["id"]
        .as_str()
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| anyhow!("Codex file-change item preceding approval had no identifier."))?
        .to_owned();
    state.file_items.insert(item_id, item.clone());
    state.stage = 10;
    Ok(())
}

pub(in crate::capture) fn validate_codex_approval_request(
    value: &Value,
    method: &str,
    cwd: &Path,
    state: &mut CodexApprovalState,
) -> anyhow::Result<()> {
    require_exact_keys(
        value,
        &["id", "method", "params"],
        "file-change approval request",
    )?;
    validate_codex_approval_context(value, state)?;
    require_exact_keys(
        &value["params"],
        &[
            "threadId",
            "turnId",
            "itemId",
            "startedAtMs",
            "reason",
            "grantRoot",
        ],
        "file-change approval request",
    )?;
    if method != "item/fileChange/requestApproval"
        || state.stage != 10
        || state.file_change_approvals != 0
    {
        bail!("Codex approval request had an unexpected method, order, or count.");
    }
    let (request_id, item_id) =
        codex_approval_ids(value, &mut state.request_ids, &mut state.item_ids)?;
    if request_id != 0 {
        bail!("Codex file-change approval request did not use the reviewed numeric identifier.");
    }
    let item = state.file_items.get(&item_id).ok_or_else(|| {
        anyhow!("Codex file-change approval request did not join a preceding item/started.")
    })?;
    validate_marker_change(item, cwd)?;
    state.file_change_approvals = 1;
    state.stage = 11;
    Ok(())
}

pub(in crate::capture) fn validate_on_request_item(
    value: &Value,
    expected_command: &str,
    state: &mut CodexOnRequestState,
) -> anyhow::Result<()> {
    let item = &value["params"]["item"];
    if item["type"] != "commandExecution" || item["command"] != expected_command {
        bail!(
            "Codex on-request approval request command item did not match the exact bounded command."
        );
    }
    let item_id = item["id"]
        .as_str()
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| anyhow!("Codex on-request command item had no nonempty identifier."))?;
    let exit_code = item["exitCode"].as_i64();
    match state.stage {
        0 if item["status"] == "failed" && exit_code.is_some_and(|code| code != 0) => {
            state.failed_item_id = Some(item_id.to_owned());
            state.stage = 1;
        }
        2 if item["status"] == "completed"
            && exit_code == Some(0)
            && state.failed_item_id.as_deref() == Some(item_id) =>
        {
            state.stage = 3;
        }
        _ => bail!("Codex on-request command events arrived out of order or changed identity."),
    }
    Ok(())
}

pub(in crate::capture) fn validate_codex_on_request_approval(
    value: &Value,
    method: &str,
    expected_command: &str,
    state: &mut CodexOnRequestState,
) -> anyhow::Result<()> {
    if method != "item/commandExecution/requestApproval" {
        bail!("Codex on-request approval request had an unexpected method.");
    }
    let request_id = value["id"]
        .as_u64()
        .ok_or_else(|| anyhow!("Codex on-request approval request had no valid identifier."))?;
    if !state.request_ids.insert(request_id) {
        bail!("Codex on-request approval request repeated its request identifier.");
    }
    if state.stage != 1 {
        bail!("Codex on-request approval request arrived before the sandbox failure.");
    }
    if value["params"]["itemId"].as_str() != state.failed_item_id.as_deref()
        || value["params"]["command"] != expected_command
    {
        bail!("Codex on-request approval request changed the failed command or item.");
    }
    validate_command_action(&value["params"], expected_command)?;
    let reason = value["params"]["reason"]
        .as_str()
        .or_else(|| value["params"]["failureReason"].as_str())
        .unwrap_or_default();
    if reason.trim().is_empty() {
        bail!("Codex on-request approval request had no sandbox-failure reason.");
    }
    state.stage = 2;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    #[cfg(not(windows))]
    use std::path::Path;

    use comet_proto::{RunRequest, RuntimeMode};
    use serde_json::{Value, json};

    use super::APPROVAL_MARKER_NAME;
    use crate::capture::recording::failed_session_stdin;
    use crate::capture::test_support::{
        channel_payloads, config, contains_response_id, fixture_path, isolated_approval_target,
        isolated_tempdir,
    };
    use crate::capture::{
        CaptureOperation, Channel, CodexCaptureOperation, CodexRunScript, record,
    };

    #[test]
    fn codex_on_request_capture_runs_from_a_temp_checkout() {
        let checkout = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "capture::approval::codex::tests::codex_on_request_requires_ordered_failure_approval_retry_and_marker",
                "--nocapture",
            ])
            .current_dir(checkout.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "on-request capture failed from a temp checkout:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    fn routine_item_event(method: &str, item: Value) -> Value {
        let timing = if method == "item/started" {
            "startedAtMs"
        } else {
            "completedAtMs"
        };
        let mut params = json!({
            "item": item,
            "threadId": "th-1",
            "turnId": "t-1",
        });
        params[timing] = json!(1);
        json!({"method": method, "params": params, "emittedAtMs": 1})
    }

    fn approval_state_for_routine_item() -> super::CodexApprovalState {
        super::CodexApprovalState {
            thread_id: Some("th-1".into()),
            turn_id: Some("t-1".into()),
            ..super::CodexApprovalState::default()
        }
    }

    /// Break caught: Codex's generated `UserMessageThreadItem` schema makes `clientId`
    /// optional and nullable, but the capture driver rejects both wire shapes before approval.
    #[test]
    fn codex_approval_user_message_client_id_matches_generated_schema() {
        for client_id in [None, Some(Value::Null), Some(json!("client-1"))] {
            let mut item = json!({
                "type": "userMessage",
                "id": "u1",
                "content": [{"type": "text", "text": "bounded prompt"}],
            });
            if let Some(client_id) = client_id {
                item["clientId"] = client_id;
            }
            let mut state = approval_state_for_routine_item();
            super::observe_codex_approval_routine_item(
                &routine_item_event("item/started", item.clone()),
                "item/started",
                &mut state,
            )
            .unwrap();
            super::observe_codex_approval_routine_item(
                &routine_item_event("item/completed", item),
                "item/completed",
                &mut state,
            )
            .unwrap();
            assert!(state.routine_items.is_empty());
        }
    }

    /// Break caught: treating absent metadata as an identity value makes an otherwise matching
    /// user-message start/completion pair fail when Codex supplies `clientId` only later.
    #[test]
    fn codex_approval_user_message_client_id_does_not_control_the_item_join() {
        let started = json!({
            "type": "userMessage",
            "id": "u1",
            "content": [{"type": "text", "text": "bounded prompt"}],
        });
        let completed = json!({
            "type": "userMessage",
            "id": "u1",
            "clientId": "client-1",
            "content": [{"type": "text", "text": "bounded prompt"}],
        });
        let mut state = approval_state_for_routine_item();

        super::observe_codex_approval_routine_item(
            &routine_item_event("item/started", started),
            "item/started",
            &mut state,
        )
        .unwrap();
        super::observe_codex_approval_routine_item(
            &routine_item_event("item/completed", completed),
            "item/completed",
            &mut state,
        )
        .unwrap();

        assert!(state.routine_items.is_empty());
    }

    /// Break caught: making `clientId` optional accidentally permits unreviewed JSON kinds or
    /// turns the optional-key allowance into an open-ended item shape.
    #[test]
    fn codex_approval_user_message_rejects_invalid_client_id_and_unknown_fields() {
        for invalid in [json!(true), json!(1), json!([]), json!({})] {
            let item = json!({
                "type": "userMessage",
                "id": "u1",
                "clientId": invalid,
                "content": [],
            });
            let mut state = approval_state_for_routine_item();
            let error = super::observe_codex_approval_routine_item(
                &routine_item_event("item/started", item),
                "item/started",
                &mut state,
            )
            .unwrap_err();
            assert!(
                error.to_string().contains("unexpected value shape"),
                "{error}"
            );
        }

        let item = json!({
            "type": "userMessage",
            "id": "u1",
            "content": [],
            "unreviewed": true,
        });
        let mut state = approval_state_for_routine_item();
        let error = super::observe_codex_approval_routine_item(
            &routine_item_event("item/started", item),
            "item/started",
            &mut state,
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("unexpected or missing fields"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn codex_approval_requires_the_observed_command_phase_and_file_change() {
        let raw = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let request = RunRequest {
            prompt: "scenario:capture-approval".into(),
            cwd: cwd.path().display().to_string(),
            ..RunRequest::for_session(RuntimeMode::ApprovalRequired)
        };
        let result = record(config(
            "codex-approval",
            fixture_path("fake-codex"),
            CaptureOperation::Codex(CodexCaptureOperation::Run {
                request,
                script: CodexRunScript::Approval,
            }),
            raw.path(),
        ))
        .await;
        #[cfg(not(windows))]
        {
            let error = result.expect_err("Unix launcher evidence is not approved yet");
            assert!(
                error.to_string().contains("observed safe Unix launcher"),
                "{error}"
            );
            return;
        }
        #[cfg(windows)]
        {
            let capture = result.unwrap();
            let values: Vec<serde_json::Value> = channel_payloads(&capture, Channel::Stdout)
                .into_iter()
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect();
            assert_eq!(
                values
                    .iter()
                    .filter(|value| value["method"] == "item/started"
                        && value["params"]["item"]["type"] == "commandExecution")
                    .count(),
                5
            );
            assert_eq!(
                values
                    .iter()
                    .filter(|value| value["method"] == "item/completed"
                        && value["params"]["item"]["type"] == "commandExecution")
                    .count(),
                4
            );
            assert!(
                values.iter().any(|value| value["id"] == 0
                    && value["method"] == "item/fileChange/requestApproval")
            );
            assert!(
                channel_payloads(&capture, Channel::Stdin)
                    .iter()
                    .any(|line| {
                        serde_json::from_str::<Value>(line).is_ok_and(|value| {
                            value["id"] == 0 && value["result"]["decision"] == "accept"
                        })
                    })
            );
            assert_eq!(
                std::fs::read_to_string(cwd.path().join(APPROVAL_MARKER_NAME)).unwrap(),
                super::APPROVAL_MARKER_CONTENT
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn codex_approval_launcher_requires_exact_trusted_file_identity_and_grammar() {
        let root = tempfile::tempdir().unwrap();
        let program_files = root.path().join("Program Files");
        let system_root = root.path().join("Windows");
        let trusted_dir = program_files.join("WindowsApps/PowerShell 7.5.2");
        let hostile_dir = root.path().join("capture cwd");
        let prefix_collision = root.path().join("Program Files hostile");
        std::fs::create_dir_all(&trusted_dir).unwrap();
        std::fs::create_dir_all(&hostile_dir).unwrap();
        std::fs::create_dir_all(&system_root).unwrap();
        std::fs::create_dir_all(&prefix_collision).unwrap();
        let trusted = trusted_dir.join("pwsh.exe");
        let hostile = hostile_dir.join("pwsh.exe");
        let collision = prefix_collision.join("pwsh.exe");
        std::fs::write(&trusted, b"trusted").unwrap();
        std::fs::write(&hostile, b"hostile").unwrap();
        std::fs::write(&collision, b"collision").unwrap();
        let system_pwsh = system_root.join("System32/pwsh.exe");
        std::fs::create_dir_all(system_pwsh.parent().unwrap()).unwrap();
        std::fs::write(&system_pwsh, b"system").unwrap();
        let roots = super::canonical_protected_roots([
            Some(program_files.as_path()),
            None,
            Some(system_root.as_path()),
        ])
        .unwrap();
        let identity = super::select_trusted_powershell(
            &[hostile.clone(), collision.clone(), trusted.clone()],
            &roots,
            &[hostile_dir.clone(), root.path().join("raw")],
        )
        .unwrap();
        assert_eq!(identity, super::file_identity(&trusted).unwrap());
        assert!(
            super::select_trusted_powershell(
                &[hostile.clone(), collision],
                &roots,
                &[hostile_dir.clone(), root.path().join("raw")],
            )
            .is_err()
        );
        assert_eq!(
            super::select_trusted_powershell(
                std::slice::from_ref(&system_pwsh),
                &roots,
                std::slice::from_ref(&hostile_dir),
            )
            .unwrap(),
            super::file_identity(&system_pwsh).unwrap()
        );
        let valid = format!(r#""{}" -Command 'echo capture'"#, trusted.display());
        super::validate_codex_raw_launcher(&valid, &identity).unwrap();

        for launcher in [
            format!(r#""{}" -Command 'echo capture'"#, hostile.display()),
            format!(
                r#""{}" -NoProfile -Command 'echo capture'"#,
                trusted.display()
            ),
            format!(
                r#""{}" -Command 'echo capture && whoami'"#,
                trusted.display()
            ),
            format!(
                r#""{}" -Command 'echo capture > marker'"#,
                trusted.display()
            ),
            format!(
                r#""{}" -Command 'echo capture' trailing"#,
                trusted.display()
            ),
            format!(r#"{} -Command 'echo capture'"#, trusted.display()),
        ] {
            assert!(
                super::validate_codex_raw_launcher(&launcher, &identity).is_err(),
                "accepted untrusted launcher: {launcher:?}"
            );
        }

        std::fs::remove_file(&trusted).unwrap();
        std::fs::write(&trusted, b"replacement").unwrap();
        assert!(super::validate_codex_raw_launcher(&valid, &identity).is_err());
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn codex_approval_rejects_missing_or_invalid_rpc_ids_before_accepting() {
        for prompt in [
            "scenario:capture-approval-missing-id",
            "scenario:capture-approval-invalid-id",
        ] {
            let raw = tempfile::tempdir().unwrap();
            let cwd = tempfile::tempdir().unwrap();
            let request = RunRequest {
                prompt: prompt.into(),
                cwd: cwd.path().display().to_string(),
                ..RunRequest::for_session(RuntimeMode::ApprovalRequired)
            };
            let (error, stdin) = failed_session_stdin(config(
                "codex-approval-invalid-id",
                fixture_path("fake-codex"),
                CaptureOperation::Codex(CodexCaptureOperation::Run {
                    request,
                    script: CodexRunScript::Approval,
                }),
                raw.path(),
            ))
            .await;
            assert!(
                stdin.iter().all(|line| !line.contains("\"decision\"")),
                "invalid approval ID received accept: {stdin:?}"
            );
            assert!(error.contains("approval request"), "{error}");
        }
    }

    /// Break caught: JSON-RPC numeric zero is a valid request identifier and production echoes
    /// the exact Value; capture-only validation must not relabel it as missing.
    #[test]
    fn codex_approval_ids_accept_zero_and_reject_non_u64_json_values() {
        let mut request_ids = BTreeSet::new();
        let mut item_ids = BTreeSet::new();
        assert_eq!(
            super::codex_approval_ids(
                &json!({"id": 0, "params": {"itemId": "file-zero"}}),
                &mut request_ids,
                &mut item_ids,
            )
            .unwrap(),
            (0, "file-zero".to_owned())
        );
        assert!(
            super::codex_approval_ids(
                &json!({"id": 0, "params": {"itemId": "file-repeat"}}),
                &mut request_ids,
                &mut item_ids,
            )
            .unwrap_err()
            .to_string()
            .contains("repeated a request identifier")
        );

        for invalid in [
            Value::Null,
            json!("0"),
            json!(-1),
            json!(0.5),
            json!(true),
            json!({}),
            json!([]),
        ] {
            let mut request_ids = BTreeSet::new();
            let mut item_ids = BTreeSet::new();
            let request = json!({"id": invalid, "params": {"itemId": "file"}});
            assert!(
                super::codex_approval_ids(&request, &mut request_ids, &mut item_ids).is_err(),
                "accepted invalid JSON-RPC id: {invalid:?}"
            );
        }
        let mut request_ids = BTreeSet::new();
        let mut item_ids = BTreeSet::new();
        assert!(
            super::codex_approval_ids(
                &json!({"params": {"itemId": "file"}}),
                &mut request_ids,
                &mut item_ids,
            )
            .is_err()
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn codex_approval_rejects_destructive_requests_before_accepting() {
        for (prompt, rejected_id) in [
            ("scenario:capture-approval-destructive-command", 451),
            ("scenario:capture-approval-destructive-file", 464),
        ] {
            let raw = tempfile::tempdir().unwrap();
            let cwd = tempfile::tempdir().unwrap();
            let request = RunRequest {
                prompt: prompt.into(),
                cwd: cwd.path().display().to_string(),
                ..RunRequest::for_session(RuntimeMode::ApprovalRequired)
            };
            let (error, stdin) = failed_session_stdin(config(
                "codex-approval-adversarial",
                fixture_path("fake-codex"),
                CaptureOperation::Codex(CodexCaptureOperation::Run {
                    request,
                    script: CodexRunScript::Approval,
                }),
                raw.path(),
            ))
            .await;
            assert!(
                !contains_response_id(&stdin, rejected_id),
                "unsafe approval {rejected_id} received accept: {stdin:?}"
            );
            assert!(error.contains("approval request"), "{error}");
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn codex_approval_rejects_relevant_activity_after_file_accept() {
        let raw = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let request = RunRequest {
            prompt: "scenario:capture-approval-later-command".into(),
            cwd: cwd.path().display().to_string(),
            ..RunRequest::for_session(RuntimeMode::ApprovalRequired)
        };
        let (error, stdin) = failed_session_stdin(config(
            "codex-approval-later-command",
            fixture_path("fake-codex"),
            CaptureOperation::Codex(CodexCaptureOperation::Run {
                request,
                script: CodexRunScript::Approval,
            }),
            raw.path(),
        ))
        .await;
        assert!(
            contains_response_id(&stdin, 0),
            "bounded file was not accepted"
        );
        assert!(
            error.contains("reviewed ordering"),
            "late command was not contract-named: {error}"
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn codex_approval_rejects_observed_order_deviations_before_reply() {
        for mode in [
            "missing-thread",
            "second-turn-start",
            "wrong-thread",
            "missing-turn",
            "wrong-turn",
            "hidden-action",
            "file-extra-key",
            "file-missing-key",
            "file-missing-thread",
            "file-wrong-thread",
            "file-missing-turn",
            "file-wrong-turn",
            "request-extra-key",
            "request-missing-key",
            "request-missing-thread",
            "request-wrong-thread",
            "request-missing-turn",
            "request-wrong-turn",
            "routine-mismatch",
            "routine-type-change",
            "routine-id-change",
            "routine-completion-without-start",
            "marker-preapproval",
            "cwd-replaced",
            "wrong-order",
            "duplicate-start",
            "wrong-completion-id",
            "wrong-status",
            "wrong-exit",
            "wrong-fields",
            "command-approval",
            "missing-completion",
            "extra-file",
        ] {
            let raw = tempfile::tempdir().unwrap();
            let cwd_root = tempfile::tempdir().unwrap();
            let cwd = cwd_root.path().join("cwd");
            std::fs::create_dir(&cwd).unwrap();
            let request = RunRequest {
                prompt: format!("scenario:capture-approval-deviation:{mode}"),
                cwd: cwd.display().to_string(),
                ..RunRequest::for_session(RuntimeMode::ApprovalRequired)
            };
            let (error, stdin) = failed_session_stdin(config(
                "codex-approval-deviation",
                fixture_path("fake-codex"),
                CaptureOperation::Codex(CodexCaptureOperation::Run {
                    request,
                    script: CodexRunScript::Approval,
                }),
                raw.path(),
            ))
            .await;
            assert!(
                stdin.iter().all(|line| !line.contains("\"decision\"")),
                "{mode} received an approval reply: {stdin:?}"
            );
            assert!(
                error.contains("Codex") || error.contains("codex"),
                "{mode} lacked a safe contract error: {error}"
            );
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn codex_approval_rejects_post_accept_deviations() {
        for mode in [
            "extra-approval",
            "hidden-after",
            "marker-missing",
            "marker-wrong",
            "marker-link",
            "terminal-failed",
            "terminal-mismatch",
            "terminal-missing-thread",
            "terminal-wrong-thread",
            "terminal-missing-turn",
        ] {
            let raw = tempfile::tempdir().unwrap();
            let cwd = tempfile::tempdir().unwrap();
            let request = RunRequest {
                prompt: format!("scenario:capture-approval-deviation:{mode}"),
                cwd: cwd.path().display().to_string(),
                ..RunRequest::for_session(RuntimeMode::ApprovalRequired)
            };
            let (error, stdin) = failed_session_stdin(config(
                "codex-approval-post-accept-deviation",
                fixture_path("fake-codex"),
                CaptureOperation::Codex(CodexCaptureOperation::Run {
                    request,
                    script: CodexRunScript::Approval,
                }),
                raw.path(),
            ))
            .await;
            assert!(
                contains_response_id(&stdin, 0),
                "{mode} missed bounded accept"
            );
            assert!(
                stdin.iter().all(|line| {
                    serde_json::from_str::<Value>(line).map_or(true, |value| {
                        value["id"] != 1 || value["result"]["decision"] != "accept"
                    })
                }),
                "{mode} accepted an extra request"
            );
            assert!(error.contains("Codex"), "{mode} lacked safe error: {error}");
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn codex_approval_rejects_a_post_accept_marker_link() {
        let raw = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let request = RunRequest {
            prompt: "scenario:capture-approval-deviation:marker-link".into(),
            cwd: cwd.path().display().to_string(),
            ..RunRequest::for_session(RuntimeMode::ApprovalRequired)
        };
        let (error, stdin) = failed_session_stdin(config(
            "codex-approval-marker-link",
            fixture_path("fake-codex"),
            CaptureOperation::Codex(CodexCaptureOperation::Run {
                request,
                script: CodexRunScript::Approval,
            }),
            raw.path(),
        ))
        .await;
        assert!(contains_response_id(&stdin, 0));
        assert!(error.contains("regular non-reparse file"), "{error}");
    }

    #[tokio::test]
    async fn codex_on_request_requires_ordered_failure_approval_retry_and_marker() {
        let raw = tempfile::tempdir().unwrap();
        let cwd = isolated_tempdir("comet-onrequest-cwd-");
        let Some(target) = isolated_approval_target("comet-onrequest-target-") else {
            return;
        };
        let request = RunRequest {
            prompt: format!("scenario:capture-onrequest:{}", target.path().display()),
            cwd: cwd.path().display().to_string(),
            ..RunRequest::for_session(RuntimeMode::AutoAcceptEdits)
        };
        let mut config = config(
            "codex-approval-on-request",
            fixture_path("fake-codex"),
            CaptureOperation::Codex(CodexCaptureOperation::Run {
                request,
                script: CodexRunScript::ApprovalOnRequest,
            }),
            raw.path(),
        );
        config.approval_target = Some(target.path().into());
        let capture = record(config).await.unwrap();
        assert_eq!(
            capture.redaction_roots.approval_target.as_deref(),
            Some(target.path().to_string_lossy().as_ref())
        );
    }

    #[tokio::test]
    async fn codex_on_request_echoes_zero_and_rejects_its_duplicate() {
        let raw = tempfile::tempdir().unwrap();
        let cwd = isolated_tempdir("comet-onrequest-zero-cwd-");
        let Some(target) = isolated_approval_target("comet-onrequest-zero-target-") else {
            return;
        };
        let request = RunRequest {
            prompt: format!(
                "scenario:capture-onrequest-zero:{}",
                target.path().display()
            ),
            cwd: cwd.path().display().to_string(),
            ..RunRequest::for_session(RuntimeMode::AutoAcceptEdits)
        };
        let mut capture_config = config(
            "codex-approval-on-request-zero",
            fixture_path("fake-codex"),
            CaptureOperation::Codex(CodexCaptureOperation::Run {
                request,
                script: CodexRunScript::ApprovalOnRequest,
            }),
            raw.path(),
        );
        capture_config.approval_target = Some(target.path().into());
        let (error, stdin) = failed_session_stdin(capture_config).await;
        assert!(
            contains_response_id(&stdin, 0),
            "numeric zero was not echoed: {error}; {stdin:?}"
        );
        assert!(error.contains("repeated its request identifier"), "{error}");
        assert_eq!(
            stdin
                .iter()
                .filter(|line| serde_json::from_str::<Value>(line)
                    .is_ok_and(|value| value["id"] == 0 && value["result"]["decision"] == "accept"))
                .count(),
            1
        );
    }

    #[test]
    fn codex_on_request_ids_reject_every_non_u64_json_shape() {
        for invalid in [
            Value::Null,
            json!("0"),
            json!(-1),
            json!(0.5),
            json!(true),
            json!({}),
            json!([]),
        ] {
            let mut state = super::CodexOnRequestState {
                failed_item_id: Some("item".into()),
                stage: 1,
                ..Default::default()
            };
            let request = json!({"id":invalid,"method":"item/commandExecution/requestApproval","params":{"itemId":"item","command":"safe","commandActions":[{"type":"unknown","command":"safe"}],"reason":"sandbox denied"}});
            assert!(
                super::validate_codex_on_request_approval(
                    &request,
                    "item/commandExecution/requestApproval",
                    "safe",
                    &mut state
                )
                .is_err()
            );
        }
    }

    #[tokio::test]
    async fn codex_on_request_invalid_id_shapes_never_receive_a_reply() {
        for mode in [
            "missing", "null", "string", "negative", "fraction", "bool", "object", "array",
        ] {
            let raw = tempfile::tempdir().unwrap();
            let cwd = isolated_tempdir("comet-onrequest-invalid-cwd-");
            let Some(target) = isolated_approval_target("comet-onrequest-invalid-target-") else {
                return;
            };
            let request = RunRequest {
                prompt: format!(
                    "scenario:capture-onrequest-invalid-{mode}:{}",
                    target.path().display()
                ),
                cwd: cwd.path().display().to_string(),
                ..RunRequest::for_session(RuntimeMode::AutoAcceptEdits)
            };
            let mut capture_config = config(
                "codex-approval-on-request-invalid-id",
                fixture_path("fake-codex"),
                CaptureOperation::Codex(CodexCaptureOperation::Run {
                    request,
                    script: CodexRunScript::ApprovalOnRequest,
                }),
                raw.path(),
            );
            capture_config.approval_target = Some(target.path().into());
            let (error, stdin) = failed_session_stdin(capture_config).await;
            assert!(
                stdin.iter().all(|line| !line.contains("\"decision\"")),
                "{mode} received an approval response: {stdin:?}"
            );
            assert!(error.contains("valid identifier"), "{mode}: {error}");
        }
    }

    #[tokio::test]
    async fn codex_on_request_rejects_approval_before_sandbox_failure() {
        let raw = tempfile::tempdir().unwrap();
        let cwd = isolated_tempdir("comet-onrequest-cwd-");
        let Some(target) = isolated_approval_target("comet-onrequest-target-") else {
            return;
        };
        let request = RunRequest {
            prompt: "scenario:capture-onrequest-out-of-order".into(),
            cwd: cwd.path().display().to_string(),
            ..RunRequest::for_session(RuntimeMode::AutoAcceptEdits)
        };
        let mut config = config(
            "codex-approval-on-request",
            fixture_path("fake-codex"),
            CaptureOperation::Codex(CodexCaptureOperation::Run {
                request,
                script: CodexRunScript::ApprovalOnRequest,
            }),
            raw.path(),
        );
        config.approval_target = Some(target.path().into());
        let error = record(config).await.unwrap_err();
        assert!(error.to_string().contains("before the sandbox failure"));
    }

    #[tokio::test]
    async fn codex_on_request_rejects_a_destructive_command_before_accepting() {
        let raw = tempfile::tempdir().unwrap();
        let cwd = isolated_tempdir("comet-onrequest-cwd-");
        let Some(target) = isolated_approval_target("comet-onrequest-target-") else {
            return;
        };
        let request = RunRequest {
            prompt: "scenario:capture-onrequest-destructive".into(),
            cwd: cwd.path().display().to_string(),
            ..RunRequest::for_session(RuntimeMode::AutoAcceptEdits)
        };
        let mut capture_config = config(
            "codex-approval-on-request-adversarial",
            fixture_path("fake-codex"),
            CaptureOperation::Codex(CodexCaptureOperation::Run {
                request,
                script: CodexRunScript::ApprovalOnRequest,
            }),
            raw.path(),
        );
        capture_config.approval_target = Some(target.path().into());
        let (error, stdin) = failed_session_stdin(capture_config).await;
        assert!(
            !contains_response_id(&stdin, 471),
            "unsafe on-request command received accept: {stdin:?}"
        );
        assert!(error.contains("approval request"), "{error}");
    }

    #[tokio::test]
    async fn codex_on_request_rechecks_target_immediately_before_accepting() {
        for race in ["target", "marker", "identity"] {
            let raw = tempfile::tempdir().unwrap();
            let cwd = isolated_tempdir("comet-onrequest-cwd-");
            let Some(target) = isolated_approval_target("comet-onrequest-target-") else {
                return;
            };
            let prompt = format!(
                "scenario:capture-onrequest-{race}-race:{}",
                target.path().display()
            );
            let request = RunRequest {
                prompt,
                cwd: cwd.path().display().to_string(),
                ..RunRequest::for_session(RuntimeMode::AutoAcceptEdits)
            };
            let mut capture_config = config(
                "codex-approval-on-request-target-race",
                fixture_path("fake-codex"),
                CaptureOperation::Codex(CodexCaptureOperation::Run {
                    request,
                    script: CodexRunScript::ApprovalOnRequest,
                }),
                raw.path(),
            );
            capture_config.approval_target = Some(target.path().into());
            let (error, stdin) = failed_session_stdin(capture_config).await;
            if race == "identity" {
                std::fs::remove_dir(format!("{}.original", target.path().display())).unwrap();
            }
            assert!(
                !contains_response_id(&stdin, 481),
                "raced target received accept: {stdin:?}"
            );
            assert!(error.contains("approval target"), "{error}");
        }
    }
}
