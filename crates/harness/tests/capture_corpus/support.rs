use std::path::{Path, PathBuf};

use serde_json::Value;

pub(super) fn staging_dir(root: &Path, name: &str) -> PathBuf {
    root.join(".comet-provider-captures")
        .join("staging")
        .join(name)
}

pub(super) fn write_raw_capture(root: &Path, name: &str, events: &[&str]) -> PathBuf {
    let raw = root.join(".comet-provider-captures").join("raw").join(name);
    std::fs::create_dir_all(&raw).unwrap();
    let event_values: Vec<Value> = events
        .iter()
        .enumerate()
        .map(|(index, payload)| {
            serde_json::json!({
                "sequence": index + 1,
                "channel": if index == events.len() - 1 { "stderr" } else { "stdout" },
                "payload": payload,
            })
        })
        .collect();
    let literal_capture = serde_json::json!({
        "directory": "ignored-raw-directory",
        "provider": "claude",
        "cli_version": "2.1.0 (Claude Code)",
        "captured_at_unix_ms": 1786464000123i64,
        "scenario": "model-discovery",
        "purpose": "capture Claude's token-free model initialize reply",
        "platform": {"os": "windows", "arch": "x86_64"},
        "redaction_roots": {"cwd": null, "repo": null, "home": null, "temp": null},
        "command": {
            "program": "claude",
            "args": ["--print"],
            "cwd": null,
            "configured_env": {},
            "stdin": "Piped",
            "stdout": "Piped",
            "stderr": "Piped",
            "kill_on_drop": true,
            "creation_flags": 0
        },
        "events": event_values,
        "exit_code": 0
    });
    std::fs::write(
        raw.join("capture.json"),
        serde_json::to_vec_pretty(&literal_capture).unwrap(),
    )
    .unwrap();
    raw
}

pub(super) fn sanitized_payloads(events_bytes: &[u8]) -> Vec<Value> {
    std::str::from_utf8(events_bytes)
        .unwrap()
        .lines()
        .map(|line| {
            let event: Value = serde_json::from_str(line).unwrap();
            serde_json::from_str(event["payload"].as_str().unwrap()).unwrap()
        })
        .collect()
}

pub(super) fn claim_payloads(corpus_root: &Path, claim_id: &str) -> Vec<(String, Value)> {
    let index: Value =
        serde_json::from_slice(&std::fs::read(corpus_root.join("index.json")).unwrap()).unwrap();
    let claim = index["claims"]
        .as_array()
        .unwrap()
        .iter()
        .find(|claim| claim["id"] == claim_id)
        .unwrap_or_else(|| panic!("missing residual claim {claim_id}"));
    let mut payloads = Vec::new();
    for evidence in claim["evidence"].as_array().unwrap() {
        let manifest = evidence["manifest"].as_str().unwrap();
        let events_path = corpus_root.join(manifest).with_file_name("events.jsonl");
        let events: Vec<Value> = std::fs::read_to_string(events_path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        for frame in evidence["frames"].as_array().unwrap() {
            let sequence = frame["sequence"].as_u64().unwrap();
            let event = events
                .iter()
                .find(|event| event["sequence"] == sequence)
                .unwrap_or_else(|| panic!("{claim_id}: missing frame {sequence}"));
            let payload = serde_json::from_str(event["payload"].as_str().unwrap()).unwrap();
            payloads.push((event["channel"].as_str().unwrap().to_owned(), payload));
        }
    }
    payloads
}

pub(super) fn write_valid_literal_corpus(root: &Path) {
    let scenario = root.join("claude/2.1.227/model-discovery");
    std::fs::create_dir_all(&scenario).unwrap();
    std::fs::write(
        root.join("index.json"),
        r#"{
  "schema_version": 1,
  "claims": [
    {
      "id": "claude-model-reply",
      "consumer": "crates/harness/src/claude/discovery.rs:the_captured_reply_decodes_onto_curated_ids",
      "evidence": [{
        "manifest": "claude/2.1.227/model-discovery/manifest.json",
        "frames": [{"sequence": 2, "channel": "stdout"}]
      }],
      "fact": "The initialize response nests the provider model list twice."
    }
  ]
}
"#,
    )
    .unwrap();
    std::fs::write(
        scenario.join("manifest.json"),
        r#"{
  "schema_version": 1,
  "source": "capture.json",
  "provider": "claude",
  "cli_version": "2.1.227 (Claude Code)",
  "normalized_cli_version": "2.1.227 (Claude Code)",
  "captured_at_unix_ms": 1786464000123,
  "scenario": "model-discovery",
  "purpose": "capture Claude's token-free model initialize reply",
  "platform": {"os": "windows", "arch": "x86_64"},
  "command": {
    "program": "claude",
    "args": ["--print", "--input-format", "stream-json"],
    "cwd": "<TEMP>",
    "configured_env": {},
    "stdin": "Piped",
    "stdout": "Piped",
    "stderr": "Piped",
    "kill_on_drop": true,
    "creation_flags": 134217728
  },
  "channels": ["stdin", "stdout", "stderr"],
  "exit_code": 0,
  "placeholders": [
    {"placeholder": "<TEMP>", "kind": "temp_path"},
    {"placeholder": "<CLAUDE_REQUEST_ID_1>", "kind": "claude_request_id"}
  ],
  "redaction_counts": {"claude_request_id": 2, "temp_path": 1},
  "consumers": [
    "crates/harness/src/claude/discovery.rs:the_captured_reply_decodes_onto_curated_ids"
  ]
}
"#,
    )
    .unwrap();
    std::fs::write(
        scenario.join("events.jsonl"),
        concat!(
            "{\"sequence\":1,\"channel\":\"stdin\",\"payload\":\"{\\\"type\\\":\\\"control_request\\\",\\\"request_id\\\":\\\"<CLAUDE_REQUEST_ID_1>\\\"}\"}\n",
            "{\"sequence\":2,\"channel\":\"stdout\",\"payload\":\"{\\\"type\\\":\\\"control_response\\\",\\\"request_id\\\":\\\"<CLAUDE_REQUEST_ID_1>\\\",\\\"models\\\":[{\\\"value\\\":\\\"sonnet\\\"}]}\"}\n",
            "{\"sequence\":3,\"channel\":\"stderr\",\"payload\":\"safe diagnostic\"}\n"
        ),
    )
    .unwrap();
}

pub(super) fn overwrite(root: &Path, relative: &str, contents: &str) {
    std::fs::write(root.join(relative), contents).unwrap();
}
