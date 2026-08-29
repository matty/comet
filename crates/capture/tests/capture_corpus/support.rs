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
