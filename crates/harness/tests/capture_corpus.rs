use std::path::{Path, PathBuf};

use comet_harness::capture::{
    CorpusError, SanitizationError, sanitize_dir, selected_payload, validate_corpus,
};
use serde_json::Value;

fn staging_dir(root: &Path, name: &str) -> PathBuf {
    root.join(".comet-provider-captures")
        .join("staging")
        .join(name)
}

fn write_raw_capture(root: &Path, name: &str, events: &[&str]) -> PathBuf {
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

fn sanitized_payloads(events_bytes: &[u8]) -> Vec<Value> {
    std::str::from_utf8(events_bytes)
        .unwrap()
        .lines()
        .map(|line| {
            let event: Value = serde_json::from_str(line).unwrap();
            serde_json::from_str(event["payload"].as_str().unwrap()).unwrap()
        })
        .collect()
}

/// Break caught: a promoted artifact cannot be reproduced or audited when sanitization drops the
/// logical scenario, its purpose, or the one capture-time sample recorded with the raw evidence.
#[test]
fn sanitizer_manifest_preserves_structured_capture_provenance() {
    let temp = tempfile::tempdir().unwrap();
    let raw = write_raw_capture(
        temp.path(),
        "capture-provenance",
        &[r#"{"type":"control_response","response":{"subtype":"success"}}"#],
    );
    let report = sanitize_dir(&raw, &staging_dir(temp.path(), "capture-provenance")).unwrap();
    let manifest: Value = serde_json::from_slice(&report.manifest_bytes).unwrap();

    assert_eq!(manifest["captured_at_unix_ms"], 1786464000123i64);
    assert_eq!(manifest["scenario"], "model-discovery");
    assert_eq!(
        manifest["purpose"],
        "capture Claude's token-free model initialize reply"
    );
}

fn write_valid_literal_corpus(root: &Path) {
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

fn overwrite(root: &Path, relative: &str, contents: &str) {
    std::fs::write(root.join(relative), contents).unwrap();
}

/// Break caught: reserializing a selected provider frame through a Comet wire type can silently
/// normalize away omitted fields or change the provider's literal JSON bytes.
#[test]
fn corpus_valid_literal_schema_returns_the_exact_selected_payload() {
    let temp = tempfile::tempdir().unwrap();
    write_valid_literal_corpus(temp.path());

    let errors = validate_corpus(temp.path());
    assert!(errors.is_empty(), "valid corpus returned {errors:#?}");
    assert_eq!(
        selected_payload(temp.path(), "claude-model-reply").unwrap(),
        r#"{"type":"control_response","request_id":"<CLAUDE_REQUEST_ID_1>","models":[{"value":"sonnet"}]}"#
    );
}

/// Break caught: sanitizer-generated path definitions are part of the promoted manifest schema,
/// so corpus validation must accept their exact static placeholder/kind pairs.
#[test]
fn corpus_accepts_sanitizer_generated_static_path_definitions() {
    let temp = tempfile::tempdir().unwrap();
    let raw = write_raw_capture(
        temp.path(),
        "static-placeholder-roundtrip",
        &[r#"{"type":"control_response","response":{"subtype":"success"}}"#],
    );
    let capture_path = raw.join("capture.json");
    let mut capture: Value =
        serde_json::from_slice(&std::fs::read(&capture_path).unwrap()).unwrap();
    let workspace = temp.path().join("private-workspace");
    let workspace = workspace.to_string_lossy().into_owned();
    capture["redaction_roots"]["cwd"] = Value::String(workspace.clone());
    capture["command"]["cwd"] = Value::String(workspace);
    std::fs::write(&capture_path, serde_json::to_vec_pretty(&capture).unwrap()).unwrap();

    let report = sanitize_dir(
        &raw,
        &staging_dir(temp.path(), "static-placeholder-roundtrip"),
    )
    .unwrap();
    let corpus = temp.path().join("corpus");
    let scenario = corpus.join("claude/2.1.0/model-discovery");
    std::fs::create_dir_all(&scenario).unwrap();
    std::fs::write(
        corpus.join("index.json"),
        r#"{
  "schema_version": 1,
  "claims": [{
    "id": "static-placeholder-roundtrip",
    "consumer": "tests:static_placeholder_roundtrip",
    "evidence": [{
      "manifest": "claude/2.1.0/model-discovery/manifest.json",
      "frames": [{"sequence": 1, "channel": "stderr"}]
    }],
    "fact": "Sanitized static path definitions validate after promotion."
  }]
}
"#,
    )
    .unwrap();
    let mut manifest: Value = serde_json::from_slice(&report.manifest_bytes).unwrap();
    manifest["consumers"] = serde_json::json!(["tests:static_placeholder_roundtrip"]);
    std::fs::write(
        scenario.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
    std::fs::write(scenario.join("events.jsonl"), report.events_bytes).unwrap();

    let errors = validate_corpus(&corpus);
    assert!(errors.is_empty(), "roundtrip returned {errors:#?}");

    let cwd_definition = manifest["placeholders"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|definition| definition["placeholder"] == "<CWD>")
        .unwrap();
    cwd_definition["kind"] = Value::String("home_path".into());
    std::fs::write(
        scenario.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
    assert!(matches!(
        validate_corpus(&corpus).as_slice(),
        [CorpusError::PlaceholderKindMismatch {
            placeholder,
            expected_kind,
            actual_kind,
            ..
        }] if placeholder == "<CWD>"
            && expected_kind == "cwd_path"
            && actual_kind == "home_path"
    ));
}

/// Break caught: a static path marker in promoted evidence can outlive its manifest accounting,
/// leaving reviewers unable to tell which path family the sanitizer replaced.
#[test]
fn corpus_requires_a_definition_for_every_used_static_placeholder() {
    let temp = tempfile::tempdir().unwrap();
    write_valid_literal_corpus(temp.path());
    let manifest_path = "claude/2.1.227/model-discovery/manifest.json";
    let manifest = std::fs::read_to_string(temp.path().join(manifest_path))
        .unwrap()
        .replace(
            concat!(
                "    {\"placeholder\": \"<TEMP>\", \"kind\": \"temp_path\"},",
                "\n"
            ),
            "",
        );
    overwrite(temp.path(), manifest_path, &manifest);

    assert!(matches!(
        validate_corpus(temp.path()).as_slice(),
        [CorpusError::MissingPlaceholderDefinition { placeholder, .. }]
            if placeholder == "<TEMP>"
    ));
}

/// Break caught: a static path definition with no matching promoted marker can falsely claim a
/// path was sanitized even though the evidence contains no auditable occurrence.
#[test]
fn corpus_rejects_an_unused_static_placeholder_definition() {
    let temp = tempfile::tempdir().unwrap();
    write_valid_literal_corpus(temp.path());
    let manifest_path = "claude/2.1.227/model-discovery/manifest.json";
    let manifest = std::fs::read_to_string(temp.path().join(manifest_path))
        .unwrap()
        .replace(
            r#"    {"placeholder": "<TEMP>", "kind": "temp_path"},"#,
            concat!(
                r#"    {"placeholder": "<TEMP>", "kind": "temp_path"},"#,
                "\n",
                r#"    {"placeholder": "<HOME>", "kind": "home_path"},"#
            ),
        );
    overwrite(temp.path(), manifest_path, &manifest);

    assert!(matches!(
        validate_corpus(temp.path()).as_slice(),
        [CorpusError::UnusedPlaceholderDefinition { placeholder, .. }]
            if placeholder == "<HOME>"
    ));
}

/// Break caught: accepting a future index or manifest schema under version-one assumptions can
/// misread references while still returning a plausible provider payload.
#[test]
fn corpus_rejects_unknown_index_and_manifest_schema_versions_explicitly() {
    let temp = tempfile::tempdir().unwrap();
    write_valid_literal_corpus(temp.path());
    overwrite(
        temp.path(),
        "index.json",
        r#"{"schema_version":2,"claims":[]}"#,
    );
    assert!(matches!(
        validate_corpus(temp.path()).as_slice(),
        [CorpusError::UnsupportedIndexSchemaVersion { version: 2 }]
    ));

    write_valid_literal_corpus(temp.path());
    let manifest = std::fs::read_to_string(
        temp.path()
            .join("claude/2.1.227/model-discovery/manifest.json"),
    )
    .unwrap()
    .replacen(r#""schema_version": 1"#, r#""schema_version": 2"#, 1)
    .replacen("2.1.227 (Claude Code)", "2.1.227 <UNKNOWN-MARKER>", 1);
    overwrite(
        temp.path(),
        "claude/2.1.227/model-discovery/manifest.json",
        &manifest,
    );
    assert!(matches!(
        validate_corpus(temp.path()).as_slice(),
        [CorpusError::UnsupportedManifestSchemaVersion {
            claim_id,
            version: 2,
            ..
        }] if claim_id == "claude-model-reply"
    ));
}

/// Break caught: scanning serialized JSON misses escaped markers and marker-shaped keys, while a
/// loose numeric suffix accepts placeholder ids the sanitizer can never emit.
#[test]
fn corpus_inspects_decoded_json_values_and_keys_with_exact_placeholder_grammar() {
    let temp = tempfile::tempdir().unwrap();
    let events_path = "claude/2.1.227/model-discovery/events.jsonl";
    for marker in [
        "\\u003cUNKNOWN-MARKER\\u003e",
        "<SESSION_ID_0>",
        "<SESSION_ID_01>",
        "<SESSION_ID_-1>",
        "${SECRET}",
        "{{SECRET}}",
        "[REDACTED]",
    ] {
        write_valid_literal_corpus(temp.path());
        let payload = format!(r#"{{"{marker}":"safe"}}"#);
        let encoded = serde_json::to_string(&payload).unwrap();
        let events = format!(
            concat!(
                "{{\"sequence\":1,\"channel\":\"stdin\",\"payload\":\"{{\\\"request_id\\\":\\\"<CLAUDE_REQUEST_ID_1>\\\"}}\"}}\n",
                "{{\"sequence\":2,\"channel\":\"stdout\",\"payload\":{}}}\n",
                "{{\"sequence\":3,\"channel\":\"stderr\",\"payload\":\"safe diagnostic\"}}\n"
            ),
            encoded
        );
        overwrite(temp.path(), events_path, &events);
        let errors = validate_corpus(temp.path());
        assert!(
            matches!(
                errors.as_slice(),
                [CorpusError::UnresolvedPlaceholder { claim_id, location: "events" }]
                    if claim_id == "claude-model-reply"
            ),
            "marker {marker:?} returned {errors:#?}"
        );
    }

    write_valid_literal_corpus(temp.path());
    let index_path = temp.path().join("index.json");
    let mut index: Value = serde_json::from_slice(&std::fs::read(&index_path).unwrap()).unwrap();
    index["claims"][0]["<UNKNOWN-MARKER>"] = Value::String("ignored field".to_owned());
    overwrite(
        temp.path(),
        "index.json",
        &serde_json::to_string_pretty(&index).unwrap(),
    );
    assert!(matches!(
        validate_corpus(temp.path()).as_slice(),
        [CorpusError::UnresolvedPlaceholder { claim_id, location: "index" }]
            if claim_id == "claude-model-reply"
    ));

    write_valid_literal_corpus(temp.path());
    let manifest_path = "claude/2.1.227/model-discovery/manifest.json";
    let manifest = std::fs::read_to_string(temp.path().join(manifest_path))
        .unwrap()
        .replace("\"kind\": \"claude_request_id\"", "\"kind\": \"${SECRET}\"");
    overwrite(temp.path(), manifest_path, &manifest);
    assert!(matches!(
        validate_corpus(temp.path()).as_slice(),
        [CorpusError::UnresolvedPlaceholder { claim_id, location: "manifest" }]
            if claim_id == "claude-model-reply"
    ));

    write_valid_literal_corpus(temp.path());
    let events = std::fs::read_to_string(temp.path().join(events_path))
        .unwrap()
        .replace("safe diagnostic", "ordinary comparison 1 < 2 > 0");
    overwrite(temp.path(), events_path, &events);
    assert!(validate_corpus(temp.path()).is_empty());
}

/// Break caught: accepting declarations independently from uses lets a typo, duplicate, stale
/// definition, or kind mismatch make the sanitization accounting unauditable.
#[test]
fn corpus_cross_checks_typed_placeholder_definitions_against_every_use() {
    let temp = tempfile::tempdir().unwrap();
    let manifest_path = "claude/2.1.227/model-discovery/manifest.json";
    let definition = r#"    {"placeholder": "<CLAUDE_REQUEST_ID_1>", "kind": "claude_request_id"}"#;

    write_valid_literal_corpus(temp.path());
    let manifest = std::fs::read_to_string(temp.path().join(manifest_path))
        .unwrap()
        .replace(&format!(",\n{definition}"), "");
    overwrite(temp.path(), manifest_path, &manifest);
    assert!(matches!(
        validate_corpus(temp.path()).as_slice(),
        [CorpusError::MissingPlaceholderDefinition { placeholder, .. }]
            if placeholder == "<CLAUDE_REQUEST_ID_1>"
    ));

    write_valid_literal_corpus(temp.path());
    let manifest = std::fs::read_to_string(temp.path().join(manifest_path))
        .unwrap()
        .replace(
            definition,
            concat!(
                "    {\"placeholder\": \"<CLAUDE_REQUEST_ID_1>\", \"kind\": \"claude_request_id\"},\n",
                "    {\"placeholder\": \"<SESSION_ID_1>\", \"kind\": \"session_id\"}"
            ),
        );
    overwrite(temp.path(), manifest_path, &manifest);
    assert!(matches!(
        validate_corpus(temp.path()).as_slice(),
        [CorpusError::UnusedPlaceholderDefinition { placeholder, .. }]
            if placeholder == "<SESSION_ID_1>"
    ));

    write_valid_literal_corpus(temp.path());
    let manifest = std::fs::read_to_string(temp.path().join(manifest_path))
        .unwrap()
        .replace(definition, &format!("{definition},\n{definition}"));
    overwrite(temp.path(), manifest_path, &manifest);
    assert!(matches!(
        validate_corpus(temp.path()).as_slice(),
        [CorpusError::DuplicatePlaceholderDefinition { placeholder, .. }]
            if placeholder == "<CLAUDE_REQUEST_ID_1>"
    ));

    write_valid_literal_corpus(temp.path());
    let manifest = std::fs::read_to_string(temp.path().join(manifest_path))
        .unwrap()
        .replace(
            "\"kind\": \"claude_request_id\"",
            "\"kind\": \"session_id\"",
        );
    overwrite(temp.path(), manifest_path, &manifest);
    assert!(matches!(
        validate_corpus(temp.path()).as_slice(),
        [CorpusError::PlaceholderKindMismatch {
            placeholder,
            expected_kind,
            actual_kind,
            ..
        }] if placeholder == "<CLAUDE_REQUEST_ID_1>"
            && expected_kind == "claude_request_id"
            && actual_kind == "session_id"
    ));
}

/// Break caught: sorting frames or merely checking uniqueness accepts gaps and duplicates, so a
/// claim can name a sequence that never occupied that place in the recorded observer order.
#[test]
fn corpus_requires_contiguous_increasing_event_sequences() {
    let temp = tempfile::tempdir().unwrap();
    write_valid_literal_corpus(temp.path());
    overwrite(
        temp.path(),
        "claude/2.1.227/model-discovery/events.jsonl",
        concat!(
            "{\"sequence\":1,\"channel\":\"stdin\",\"payload\":\"{}\"}\n",
            "{\"sequence\":3,\"channel\":\"stdout\",\"payload\":\"{}\"}\n"
        ),
    );

    assert!(matches!(
        validate_corpus(temp.path()).as_slice(),
        [CorpusError::NonContiguousEventSequence {
            claim_id,
            expected: 2,
            actual: 3,
            ..
        }] if claim_id == "claude-model-reply"
    ));
}

/// Break caught: permissive channel decoding accepts invented streams and lets an index claim
/// stdout evidence that was actually sent on stdin.
#[test]
fn corpus_allows_only_stdio_channels_and_requires_an_exact_frame_channel() {
    let temp = tempfile::tempdir().unwrap();
    write_valid_literal_corpus(temp.path());
    let events_path = "claude/2.1.227/model-discovery/events.jsonl";
    overwrite(
        temp.path(),
        events_path,
        "{\"sequence\":1,\"channel\":\"network\",\"payload\":\"{}\"}\n",
    );
    assert!(matches!(
        validate_corpus(temp.path()).as_slice(),
        [CorpusError::InvalidEvent { claim_id, line: 1, .. }]
            if claim_id == "claude-model-reply"
    ));

    write_valid_literal_corpus(temp.path());
    let index = std::fs::read_to_string(temp.path().join("index.json"))
        .unwrap()
        .replacen(r#""channel": "stdout""#, r#""channel": "stdin""#, 1);
    overwrite(temp.path(), "index.json", &index);
    assert!(matches!(
        validate_corpus(temp.path()).as_slice(),
        [CorpusError::FrameChannelMismatch {
            claim_id,
            sequence: 2,
            expected: comet_harness::capture::Channel::Stdin,
            actual: comet_harness::capture::Channel::Stdout,
        }] if claim_id == "claude-model-reply"
    ));
}

/// Break caught: indexing claims by last-write-wins or joining unvalidated paths can silently
/// select another claim, leave the corpus root, or accept non-canonical aliases to one file.
#[test]
fn corpus_requires_unique_claim_ids_and_canonical_safe_relative_paths() {
    let temp = tempfile::tempdir().unwrap();
    write_valid_literal_corpus(temp.path());
    let index = std::fs::read_to_string(temp.path().join("index.json"))
        .unwrap()
        .replacen("\n  ]", ",\n    {\"id\":\"claude-model-reply\",\"consumer\":\"duplicate\",\"evidence\":[{\"manifest\":\"claude/2.1.227/model-discovery/manifest.json\",\"frames\":[{\"sequence\":2,\"channel\":\"stdout\"}]}],\"fact\":\"duplicate\"}\n  ]", 1);
    overwrite(temp.path(), "index.json", &index);
    assert!(matches!(
        validate_corpus(temp.path()).as_slice(),
        [CorpusError::DuplicateClaimId { claim_id }]
            if claim_id == "claude-model-reply"
    ));

    for unsafe_path in [
        "../manifest.json",
        "/absolute/manifest.json",
        "claude\\\\2.1.227\\\\manifest.json",
        "claude/./2.1.227/manifest.json",
        "C:/manifest.json",
        "claude/2.1.227/model-discovery/evidence.json",
        "claude./2.1.227/model-discovery/manifest.json",
        "claude /2.1.227/model-discovery/manifest.json",
        " /2.1.227/model-discovery/manifest.json",
        "AUX/2.1.227/model-discovery/manifest.json",
        "con.txt/2.1.227/model-discovery/manifest.json",
    ] {
        write_valid_literal_corpus(temp.path());
        let index = std::fs::read_to_string(temp.path().join("index.json"))
            .unwrap()
            .replacen(
                "claude/2.1.227/model-discovery/manifest.json",
                unsafe_path,
                1,
            );
        overwrite(temp.path(), "index.json", &index);
        let errors = validate_corpus(temp.path());
        assert!(
            matches!(
                errors.as_slice(),
                [CorpusError::UnsafeManifestPath { claim_id }]
                    if claim_id == "claude-model-reply"
            ),
            "{unsafe_path:?} returned {errors:#?}"
        );
    }
}

/// Break caught: flattening a comparative claim to one manifest cannot prove a fact that depends
/// on two captures, while selected_payload must not guess among several referenced frames.
#[test]
fn corpus_validates_every_evidence_entry_and_counts_all_selected_frames() {
    let temp = tempfile::tempdir().unwrap();
    write_valid_literal_corpus(temp.path());
    let index = std::fs::read_to_string(temp.path().join("index.json"))
        .unwrap()
        .replace(
            r#""evidence": [{
        "manifest": "claude/2.1.227/model-discovery/manifest.json",
        "frames": [{"sequence": 2, "channel": "stdout"}]
      }]"#,
            r#""evidence": [
        {
          "manifest": "claude/2.1.227/model-discovery/manifest.json",
          "frames": [{"sequence": 2, "channel": "stdout"}]
        },
        {
          "manifest": "claude/2.1.227/command-discovery/manifest.json",
          "frames": [{"sequence": 1, "channel": "stdin"}]
        }
      ]"#,
        );
    overwrite(temp.path(), "index.json", &index);

    let errors = validate_corpus(temp.path());
    assert!(
        matches!(
            errors.as_slice(),
            [CorpusError::MissingManifest { claim_id, manifest }]
                if claim_id == "claude-model-reply"
                    && manifest == "claude/2.1.227/command-discovery/manifest.json"
        ),
        "unexpected evidence errors: {errors:#?}"
    );
    assert!(matches!(
        selected_payload(temp.path(), "claude-model-reply"),
        Err(CorpusError::SelectedFrameCount { claim_id, count: 2 })
            if claim_id == "claude-model-reply"
    ));
}

/// Break caught: corpus comparison policy must reject an unsubstantiated claim without changing
/// the separate exact-one-frame contract that returns its literal provider payload.
#[test]
fn corpus_comparison_policy_does_not_change_single_payload_selection() {
    let temp = tempfile::tempdir().unwrap();
    write_valid_literal_corpus(temp.path());
    let index = std::fs::read_to_string(temp.path().join("index.json"))
        .unwrap()
        .replace(
            r#""consumer": "crates/harness/src/claude/discovery.rs:the_captured_reply_decodes_onto_curated_ids","#,
            concat!(
                r#""consumer": "crates/harness/src/claude/discovery.rs:the_captured_reply_decodes_onto_curated_ids","#,
                "\n      \"comparison\": true,"
            ),
        );
    overwrite(temp.path(), "index.json", &index);

    assert!(matches!(
        validate_corpus(temp.path()).as_slice(),
        [CorpusError::InsufficientComparisonEvidence {
            claim_id,
            total_frames: 1,
            distinct_observations: 1,
        }] if claim_id == "claude-model-reply"
    ));
    assert_eq!(
        selected_payload(temp.path(), "claude-model-reply").unwrap(),
        r#"{"type":"control_response","request_id":"<CLAUDE_REQUEST_ID_1>","models":[{"value":"sonnet"}]}"#
    );
}

/// Break caught: copying one selector twice must not turn one captured observation into a
/// comparison, even though both references independently resolve to a valid frame.
#[test]
fn corpus_comparisons_reject_duplicate_identical_frame_selectors() {
    let temp = tempfile::tempdir().unwrap();
    write_valid_literal_corpus(temp.path());
    let index = std::fs::read_to_string(temp.path().join("index.json"))
        .unwrap()
        .replace(
            r#""consumer": "crates/harness/src/claude/discovery.rs:the_captured_reply_decodes_onto_curated_ids","#,
            concat!(
                r#""consumer": "crates/harness/src/claude/discovery.rs:the_captured_reply_decodes_onto_curated_ids","#,
                "\n      \"comparison\": true,"
            ),
        )
        .replace(
            r#""frames": [{"sequence": 2, "channel": "stdout"}]"#,
            r#""frames": [
          {"sequence": 2, "channel": "stdout"},
          {"sequence": 2, "channel": "stdout"}
        ]"#,
        );
    overwrite(temp.path(), "index.json", &index);

    assert!(matches!(
        validate_corpus(temp.path()).as_slice(),
        [CorpusError::InsufficientComparisonEvidence {
            claim_id,
            total_frames: 2,
            distinct_observations: 1,
        }] if claim_id == "claude-model-reply"
    ));

    write_valid_literal_corpus(temp.path());
    let index = std::fs::read_to_string(temp.path().join("index.json"))
        .unwrap()
        .replace(
            r#""consumer": "crates/harness/src/claude/discovery.rs:the_captured_reply_decodes_onto_curated_ids","#,
            concat!(
                r#""consumer": "crates/harness/src/claude/discovery.rs:the_captured_reply_decodes_onto_curated_ids","#,
                "\n      \"comparison\": true,"
            ),
        )
        .replace(
            r#""frames": [{"sequence": 2, "channel": "stdout"}]"#,
            r#""frames": [
          {"sequence": 2, "channel": "stdout"},
          {"sequence": 3, "channel": "stdout"}
        ]"#,
        );
    overwrite(temp.path(), "index.json", &index);
    let events_path = "claude/2.1.227/model-discovery/events.jsonl";
    let events = std::fs::read_to_string(temp.path().join(events_path))
        .unwrap()
        .replace(
            r#"{"sequence":3,"channel":"stderr","payload":"safe diagnostic"}"#,
            r#"{"sequence":3,"channel":"stdout","payload":"safe diagnostic"}"#,
        );
    overwrite(temp.path(), events_path, &events);
    assert!(validate_corpus(temp.path()).is_empty());
}

/// Break caught: weak referential checks can accept a claim whose evidence file, exact frame, or
/// reciprocal manifest consumer entry is missing.
#[test]
fn corpus_requires_manifest_frame_and_reciprocal_consumer_references() {
    let temp = tempfile::tempdir().unwrap();
    write_valid_literal_corpus(temp.path());
    std::fs::remove_file(
        temp.path()
            .join("claude/2.1.227/model-discovery/manifest.json"),
    )
    .unwrap();
    assert!(matches!(
        validate_corpus(temp.path()).as_slice(),
        [CorpusError::MissingManifest { claim_id, .. }]
            if claim_id == "claude-model-reply"
    ));

    write_valid_literal_corpus(temp.path());
    let index = std::fs::read_to_string(temp.path().join("index.json"))
        .unwrap()
        .replacen(r#""sequence": 2"#, r#""sequence": 99"#, 1);
    overwrite(temp.path(), "index.json", &index);
    assert!(matches!(
        validate_corpus(temp.path()).as_slice(),
        [CorpusError::MissingFrame {
            claim_id,
            sequence: 99,
            ..
        }] if claim_id == "claude-model-reply"
    ));

    write_valid_literal_corpus(temp.path());
    let manifest_path = "claude/2.1.227/model-discovery/manifest.json";
    let manifest = std::fs::read_to_string(temp.path().join(manifest_path))
        .unwrap()
        .replace(
            "crates/harness/src/claude/discovery.rs:the_captured_reply_decodes_onto_curated_ids",
            "crates/harness/src/claude/discovery.rs:another_consumer",
        );
    overwrite(temp.path(), manifest_path, &manifest);
    assert!(matches!(
        validate_corpus(temp.path()).as_slice(),
        [CorpusError::MissingManifestConsumer {
            claim_id,
            consumer,
            ..
        }] if claim_id == "claude-model-reply"
            && consumer.ends_with("the_captured_reply_decodes_onto_curated_ids")
    ));

    write_valid_literal_corpus(temp.path());
    let manifest_path = "claude/2.1.227/model-discovery/manifest.json";
    let manifest = std::fs::read_to_string(temp.path().join(manifest_path))
        .unwrap()
        .replace(
            "  ]\n}",
            "    ,\"crates/harness/src/claude/discovery.rs:stale_consumer\"\n  ]\n}",
        );
    overwrite(temp.path(), manifest_path, &manifest);
    assert!(matches!(
        validate_corpus(temp.path()).as_slice(),
        [CorpusError::ExtraManifestConsumer { manifest, consumer }]
            if manifest.ends_with("model-discovery/manifest.json")
                && consumer.ends_with("stale_consumer")
    ));

    write_valid_literal_corpus(temp.path());
    let manifest = std::fs::read_to_string(temp.path().join(manifest_path))
        .unwrap()
        .replace(
            "    \"crates/harness/src/claude/discovery.rs:the_captured_reply_decodes_onto_curated_ids\"",
            concat!(
                "    \"crates/harness/src/claude/discovery.rs:the_captured_reply_decodes_onto_curated_ids\",\n",
                "    \"crates/harness/src/claude/discovery.rs:the_captured_reply_decodes_onto_curated_ids\""
            ),
        );
    overwrite(temp.path(), manifest_path, &manifest);
    assert!(matches!(
        validate_corpus(temp.path()).as_slice(),
        [CorpusError::DuplicateManifestConsumer { manifest, consumer }]
            if manifest.ends_with("model-discovery/manifest.json")
                && consumer.ends_with("the_captured_reply_decodes_onto_curated_ids")
    ));
}

/// Break caught: accepting ad-hoc redaction markers lets a reviewer mistake unresolved sensitive
/// material for one of the sanitizer's known, audited placeholder families.
#[test]
fn corpus_rejects_unknown_or_unresolved_placeholder_syntax() {
    let temp = tempfile::tempdir().unwrap();
    write_valid_literal_corpus(temp.path());
    let events_path = "claude/2.1.227/model-discovery/events.jsonl";
    let events = std::fs::read_to_string(temp.path().join(events_path))
        .unwrap()
        .replace("safe diagnostic", "<UNKNOWN_SECRET>");
    overwrite(temp.path(), events_path, &events);

    assert!(matches!(
        validate_corpus(temp.path()).as_slice(),
        [CorpusError::UnresolvedPlaceholder { claim_id, .. }]
            if claim_id == "claude-model-reply"
    ));

    write_valid_literal_corpus(temp.path());
    let index = std::fs::read_to_string(temp.path().join("index.json"))
        .unwrap()
        .replace(
            "The initialize response nests the provider model list twice.",
            "Unresolved evidence <PENDING_SECRET>",
        );
    overwrite(temp.path(), "index.json", &index);
    assert!(matches!(
        validate_corpus(temp.path()).as_slice(),
        [CorpusError::UnresolvedPlaceholder { claim_id, location: "index" }]
            if claim_id == "claude-model-reply"
    ));
}

/// Break caught: promoting token-free evidence without shrinking the pending inventory hides the
/// exact token-spending worklist that still needs explicit authorization.
#[test]
fn corpus_inventory_reports_the_exact_pending_manifest_set() {
    let corpus_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");
    let index: Value =
        serde_json::from_slice(&std::fs::read(corpus_root.join("index.json")).unwrap()).unwrap();
    let claim_ids: std::collections::BTreeSet<&str> = index["claims"]
        .as_array()
        .unwrap()
        .iter()
        .map(|claim| claim["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        claim_ids,
        std::collections::BTreeSet::from([
            "claude-approval-fixture-shape",
            "claude-approval-no-cwd",
            "claude-approval-tool-input-shapes",
            "claude-approval-wire-fields",
            "claude-approval-write-path-absolute",
            "claude-attachment-block-order",
            "claude-attachment-block-order-test",
            "claude-attachment-run-order",
            "claude-command-absent-aliases",
            "claude-command-discovery-behavior",
            "claude-command-empty-hint",
            "claude-command-nonbare-count",
            "claude-command-reply-decoder",
            "claude-model-bare-effects",
            "claude-model-close-exit",
            "claude-model-curated-id-decoder",
            "claude-model-cwd-invariance",
            "claude-model-default-alias",
            "claude-model-effort-levels",
            "claude-model-fixture-shape",
            "claude-model-initialize-request",
            "claude-model-integration-shape",
            "claude-model-no-modality",
            "claude-model-real-catalog-merge",
            "claude-model-reply-shape",
            "claude-routine-frame-fixture",
            "claude-routine-frame-ignore-list",
            "claude-routine-frame-integration",
            "codex-approval-policy-semantics",
            "codex-approval-request-shapes",
            "codex-command-approval-stability",
            "codex-command-launcher-fixture",
            "codex-file-change-approval-join",
            "codex-file-change-diff-shape",
            "codex-file-change-kind-object",
            "codex-file-change-kind-source",
            "codex-model-cwd-invariance",
            "codex-model-effort-objects",
            "codex-model-fixture-shape",
            "codex-model-input-modalities",
            "codex-model-integration-shape",
            "codex-model-logged-out-fallback",
            "codex-model-logged-out-integration",
            "codex-model-notification-order",
            "codex-model-one-page",
            "codex-model-page-decoder",
            "codex-model-reply-shape",
            "codex-model-request-shape",
            "codex-model-source-notification-order",
            "codex-model-text-only-integration",
            "codex-routine-notification-fixture",
            "codex-routine-notification-ignore-list",
            "codex-routine-notification-integration",
        ])
    );
    let comparison_claim_ids: std::collections::BTreeSet<&str> = index["claims"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|claim| claim["comparison"].as_bool() == Some(true))
        .map(|claim| claim["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        comparison_claim_ids,
        std::collections::BTreeSet::from([
            "claude-command-nonbare-count",
            "claude-model-bare-effects",
            "claude-model-cwd-invariance",
            "codex-approval-policy-semantics",
            "codex-command-approval-stability",
            "codex-model-cwd-invariance",
            "codex-model-logged-out-fallback",
        ])
    );

    let errors = validate_corpus(&corpus_root);
    assert_eq!(errors.len(), 17, "inventory errors: {errors:#?}");
    assert!(
        errors
            .iter()
            .all(|error| matches!(error, CorpusError::MissingManifest { .. })),
        "inventory produced non-missing-manifest errors: {errors:#?}"
    );
    let actual: std::collections::BTreeSet<(&str, &str)> = errors
        .iter()
        .filter_map(|error| match error {
            CorpusError::MissingManifest { claim_id, manifest } => {
                Some((claim_id.as_str(), manifest.as_str()))
            }
            _ => None,
        })
        .collect();
    let mut expected = std::collections::BTreeSet::new();
    let mut add = |manifest: &'static str, claims: &[&'static str]| {
        expected.extend(claims.iter().map(|claim| (*claim, manifest)));
    };
    add(
        "claude/pending-observed-version/approval/manifest.json",
        &[
            "claude-approval-fixture-shape",
            "claude-approval-no-cwd",
            "claude-approval-tool-input-shapes",
            "claude-approval-wire-fields",
            "claude-approval-write-path-absolute",
        ],
    );
    add(
        "codex/pending-observed-version/approval-on-request/manifest.json",
        &["codex-approval-policy-semantics"],
    );
    add(
        "codex/pending-observed-version/approval/manifest.json",
        &["codex-approval-policy-semantics"],
    );
    add(
        "codex/pending-observed-version/approval/manifest.json",
        &[
            "codex-approval-request-shapes",
            "codex-command-approval-stability",
            "codex-command-launcher-fixture",
            "codex-file-change-approval-join",
            "codex-file-change-diff-shape",
            "codex-file-change-kind-object",
            "codex-file-change-kind-source",
        ],
    );
    add(
        "codex/pending-observed-version/fresh-text/manifest.json",
        &[
            "codex-routine-notification-fixture",
            "codex-routine-notification-ignore-list",
            "codex-routine-notification-integration",
        ],
    );
    assert_eq!(actual, expected, "pending error identity/path set changed");
}

/// Break caught: a promoted token-free selector can exist in the index while its reviewed
/// manifest, frame, reciprocal consumer, or placeholder accounting is invalid.
#[test]
fn corpus_promoted_token_free_claims_are_valid() {
    let corpus_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");
    let index: Value =
        serde_json::from_slice(&std::fs::read(corpus_root.join("index.json")).unwrap()).unwrap();
    let promoted: std::collections::BTreeSet<&str> = index["claims"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|claim| {
            claim["evidence"]
                .as_array()
                .unwrap()
                .iter()
                .all(|evidence| {
                    !evidence["manifest"]
                        .as_str()
                        .unwrap()
                        .contains("pending-observed-version")
                })
        })
        .map(|claim| claim["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        promoted,
        std::collections::BTreeSet::from([
            "claude-attachment-block-order",
            "claude-attachment-block-order-test",
            "claude-attachment-run-order",
            "claude-command-absent-aliases",
            "claude-command-discovery-behavior",
            "claude-command-empty-hint",
            "claude-command-nonbare-count",
            "claude-command-reply-decoder",
            "claude-model-bare-effects",
            "claude-model-close-exit",
            "claude-model-curated-id-decoder",
            "claude-model-cwd-invariance",
            "claude-model-default-alias",
            "claude-model-effort-levels",
            "claude-model-fixture-shape",
            "claude-model-initialize-request",
            "claude-model-integration-shape",
            "claude-model-no-modality",
            "claude-model-real-catalog-merge",
            "claude-model-reply-shape",
            "claude-routine-frame-fixture",
            "claude-routine-frame-ignore-list",
            "claude-routine-frame-integration",
            "codex-model-cwd-invariance",
            "codex-model-effort-objects",
            "codex-model-fixture-shape",
            "codex-model-input-modalities",
            "codex-model-integration-shape",
            "codex-model-logged-out-fallback",
            "codex-model-logged-out-integration",
            "codex-model-notification-order",
            "codex-model-one-page",
            "codex-model-page-decoder",
            "codex-model-reply-shape",
            "codex-model-request-shape",
            "codex-model-source-notification-order",
            "codex-model-text-only-integration",
        ])
    );
    let errors = validate_corpus(&corpus_root);
    assert!(
        errors.iter().all(|error| matches!(
            error,
            CorpusError::MissingManifest { claim_id, manifest }
                if !promoted.contains(claim_id.as_str())
                    && manifest.contains("pending-observed-version")
        )),
        "a promoted token-free claim is invalid: {errors:#?}"
    );
}

/// Break caught: either ordering claim selects only the final reply, reverses the observed
/// neighbors, or points at a cwd variant instead of the reviewed base observation.
#[test]
fn corpus_inventory_pins_the_observed_codex_discovery_order() {
    let corpus_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");
    let index: Value =
        serde_json::from_slice(&std::fs::read(corpus_root.join("index.json")).unwrap()).unwrap();
    for claim_id in [
        "codex-model-notification-order",
        "codex-model-source-notification-order",
    ] {
        let claim = index["claims"]
            .as_array()
            .unwrap()
            .iter()
            .find(|claim| claim["id"] == claim_id)
            .unwrap();
        assert_eq!(
            claim["evidence"],
            serde_json::json!([{
                "manifest": "codex/0.147.0/model-discovery/manifest.json",
                "frames": [
                    {"sequence": 2, "channel": "stdout"},
                    {"sequence": 3, "channel": "stdout"},
                    {"sequence": 6, "channel": "stdout"}
                ]
            }]),
            "wrong observed selector for {claim_id}"
        );
        if claim_id == "codex-model-source-notification-order" {
            assert_eq!(
                claim["consumer"], "crates/harness/src/codex/discovery.rs:reply_to",
                "ordering rationale must name the actual reply matcher"
            );
        }
    }
}

/// Break caught: selector numbers alone do not prove that the promoted neighboring frames are
/// the initialize response, unsolicited remote-control notification, and model-list response.
#[test]
fn corpus_promoted_codex_ordering_frames_have_the_observed_payloads() {
    let corpus_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");
    let events: Vec<Value> =
        std::fs::read_to_string(corpus_root.join("codex/0.147.0/model-discovery/events.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();

    for claim_id in [
        "codex-model-notification-order",
        "codex-model-source-notification-order",
    ] {
        let payload = |sequence| {
            let event = events
                .iter()
                .find(|event| event["sequence"] == sequence)
                .unwrap_or_else(|| panic!("{claim_id}: missing promoted frame {sequence}"));
            assert_eq!(event["channel"], "stdout", "{claim_id}: frame {sequence}");
            serde_json::from_str::<Value>(event["payload"].as_str().unwrap()).unwrap()
        };
        let initialize = payload(2);
        let notification = payload(3);
        let model_list = payload(6);

        assert_eq!(initialize["id"], 1, "{claim_id}");
        assert!(initialize["result"].is_object(), "{claim_id}");
        assert_eq!(
            notification["method"], "remoteControl/status/changed",
            "{claim_id}"
        );
        assert_eq!(model_list["id"], 2, "{claim_id}");
        assert!(model_list["result"]["data"].is_array(), "{claim_id}");
    }
}

/// Break caught: skipping any semantic branch leaves captured identifiers, human-authored
/// content, or attachment bytes in reviewable evidence.
#[test]
fn sanitizer_replaces_semantic_values_with_typed_placeholders() {
    let temp = tempfile::tempdir().unwrap();
    let raw = write_raw_capture(
        temp.path(),
        "semantic-values",
        &[
            r#"{"type":"user","request_id":"claude-request-secret","session_id":"session-secret","message":{"role":"user","content":[{"type":"text","text":"my confidential prompt"},{"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBORw0KGgo-attachment-secret"}}]},"parent_tool_use_id":null}"#,
            r#"{"jsonrpc":"2.0","id":73,"method":"turn/start","params":{"threadId":"thread-secret","turnId":"turn-secret","input":[{"type":"text","text":"second user prompt"}]}}"#,
            r#"{"method":"item/agentMessage/delta","params":{"itemId":"message-item-secret","delta":"codex assistant prose"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"private assistant answer"},{"type":"tool_use","id":"tool-secret","name":"Bash","input":{"command":"pwd"}}]}}"#,
            r#"{"level":"debug","message":"safe diagnostic for session-secret"}"#,
        ],
    );

    let report = sanitize_dir(&raw, &staging_dir(temp.path(), "semantic-values")).unwrap();
    let payloads = sanitized_payloads(&report.events_bytes);

    assert_eq!(payloads[0]["type"], "user");
    assert_eq!(payloads[0]["request_id"], "<CLAUDE_REQUEST_ID_1>");
    assert_eq!(payloads[0]["session_id"], "<SESSION_ID_1>");
    assert_eq!(
        payloads[0]["message"]["content"][0]["text"],
        "<USER_TEXT_1>"
    );
    assert_eq!(
        payloads[0]["message"]["content"][1]["source"]["data"],
        "<ATTACHMENT_BYTES_1>"
    );
    assert!(payloads[0]["parent_tool_use_id"].is_null());

    assert_eq!(payloads[1]["jsonrpc"], "2.0");
    assert_eq!(payloads[1]["method"], "turn/start");
    assert_eq!(payloads[1]["id"], "<CODEX_RPC_ID_1>");
    assert_eq!(payloads[1]["params"]["threadId"], "<THREAD_ID_1>");
    assert_eq!(payloads[1]["params"]["turnId"], "<TURN_ID_1>");
    assert_eq!(payloads[1]["params"]["input"][0]["text"], "<USER_TEXT_2>");

    assert_eq!(payloads[2]["method"], "item/agentMessage/delta");
    assert_eq!(payloads[2]["params"]["itemId"], "<TOOL_USE_ID_1>");
    assert_eq!(payloads[2]["params"]["delta"], "<ASSISTANT_PROSE_1>");

    assert_eq!(payloads[3]["type"], "assistant");
    assert_eq!(
        payloads[3]["message"]["content"][0]["text"],
        "<ASSISTANT_PROSE_2>"
    );
    assert_eq!(
        payloads[3]["message"]["content"][1]["id"],
        "<TOOL_USE_ID_2>"
    );
    assert_eq!(payloads[3]["message"]["content"][1]["name"], "Bash");
    assert_eq!(payloads[4]["message"], "<PROVIDER_PROSE_1>");

    let all_output = String::from_utf8(
        [
            report.events_bytes.as_slice(),
            report.manifest_bytes.as_slice(),
        ]
        .concat(),
    )
    .unwrap();
    for leaked in [
        "claude-request-secret",
        "session-secret",
        "my confidential prompt",
        "iVBORw0KGgo-attachment-secret",
        "thread-secret",
        "turn-secret",
        "second user prompt",
        "message-item-secret",
        "codex assistant prose",
        "private assistant answer",
        "tool-secret",
    ] {
        assert!(
            !all_output.contains(leaked),
            "sanitized output leaked {leaked}"
        );
    }
}

/// Break caught: Claude discovery can embed locally authored command and agent descriptions,
/// including machine paths, even though those prose fields are irrelevant to corpus claims.
#[test]
fn sanitizer_replaces_discovery_descriptions_as_provider_prose() {
    let temp = tempfile::tempdir().unwrap();
    let raw = write_raw_capture(
        temp.path(),
        "discovery-provider-prose",
        &[
            r#"{"type":"control_response","response":{"subtype":"success","response":{"commands":[{"name":"safe-command","description":"private command prose at D:\\private\\command.md","argumentHint":"<private-path>"},{"name":"no-argument-command","description":"second private command prose","argumentHint":""}],"agents":[{"name":"safe-agent","description":"private agent prose at /private/agent.md"}],"models":[{"value":"safe-model","description":"private model prose"}]}}}"#,
            r#"{"level":"debug"}"#,
        ],
    );
    let report = sanitize_dir(&raw, &staging_dir(temp.path(), "discovery-provider-prose")).unwrap();
    let payloads = sanitized_payloads(&report.events_bytes);
    let reply = &payloads[0]["response"]["response"];

    assert_eq!(reply["agents"][0]["description"], "<PROVIDER_PROSE_1>");
    assert_eq!(reply["commands"][0]["argumentHint"], "<PROVIDER_PROSE_2>");
    assert_eq!(reply["commands"][0]["description"], "<PROVIDER_PROSE_3>");
    assert_eq!(reply["commands"][1]["argumentHint"], "");
    assert_eq!(reply["models"][0]["description"], "<PROVIDER_PROSE_5>");
    let manifest: Value = serde_json::from_slice(&report.manifest_bytes).unwrap();
    assert_eq!(manifest["redaction_counts"]["provider_prose"], 5);
}

/// Break caught: non-bare discovery publishes local hook output, identifiers, or paths even
/// though hook lifecycle frames are needed only as neighboring protocol evidence.
#[test]
fn sanitizer_replaces_hook_output_and_identifiers() {
    let temp = tempfile::tempdir().unwrap();
    let raw = write_raw_capture(
        temp.path(),
        "hook-output",
        &[
            r#"{"type":"system","subtype":"hook_response","hook_id":"hook-private","uuid":"uuid-private","pid":4812,"session_id":"session-private","output":"local hook output D:\\private\\hook.txt","stdout":"local hook output D:\\private\\hook.txt","stderr":""}"#,
            r#"{"level":"debug"}"#,
        ],
    );
    let report = sanitize_dir(&raw, &staging_dir(temp.path(), "hook-output")).unwrap();
    let payload = &sanitized_payloads(&report.events_bytes)[0];

    assert_eq!(payload["hook_id"], "<TOOL_USE_ID_1>");
    assert_eq!(payload["uuid"], "<MACHINE_ID_2>");
    assert_eq!(payload["pid"], "<MACHINE_ID_1>");
    assert_eq!(payload["session_id"], "<SESSION_ID_1>");
    assert_eq!(payload["output"], "<PROVIDER_PROSE_1>");
    assert_eq!(payload["stdout"], "<PROVIDER_PROSE_1>");
}

/// Break caught: free-form provider stderr can reveal ambient account/auth configuration even
/// when it contains no credential value, and has no literal corpus consumer.
#[test]
fn sanitizer_replaces_free_form_provider_stderr() {
    let temp = tempfile::tempdir().unwrap();
    let raw = write_raw_capture(
        temp.path(),
        "provider-stderr",
        &[r#"{"type":"control_response"}"#, "ambient account warning"],
    );
    let report = sanitize_dir(&raw, &staging_dir(temp.path(), "provider-stderr")).unwrap();
    let lines: Vec<Value> = std::str::from_utf8(&report.events_bytes)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines[1]["payload"], "<PROVIDER_PROSE_1>");
}

/// Break caught: Codex initialize/model-list metadata can publish the local server name and
/// provider-authored catalog/Nux prose, including paths emitted by a logged-out CLI.
#[test]
fn sanitizer_replaces_codex_server_name_and_catalog_prose() {
    let temp = tempfile::tempdir().unwrap();
    let raw = write_raw_capture(
        temp.path(),
        "codex-discovery-prose",
        &[
            r#"{"method":"account/login/completed","params":{"serverName":"private-host","installationId":"private-installation"}}"#,
            r#"{"jsonrpc":"2.0","id":2,"result":{"data":[{"id":"model","description":"private model prose","availabilityNux":{"message":"private Nux path D:\\private\\nux.md"}}]}}"#,
            r#"{"level":"debug"}"#,
        ],
    );
    let report = sanitize_dir(&raw, &staging_dir(temp.path(), "codex-discovery-prose")).unwrap();
    let payloads = sanitized_payloads(&report.events_bytes);

    assert_eq!(payloads[0]["params"]["installationId"], "<MACHINE_ID_1>");
    assert_eq!(payloads[0]["params"]["serverName"], "<MACHINE_ID_2>");
    assert_eq!(
        payloads[1]["result"]["data"][0]["description"],
        "<PROVIDER_PROSE_2>"
    );
    assert_eq!(
        payloads[1]["result"]["data"][0]["availabilityNux"]["message"],
        "<PROVIDER_PROSE_1>"
    );
}

/// Break caught: treating every bare `id` as structural lets actual Codex thread, turn, and item
/// objects retain provider-generated identifiers when those IDs are nested below named entities.
#[test]
fn sanitizer_redacts_nested_codex_thread_turn_and_every_item_id() {
    let temp = tempfile::tempdir().unwrap();
    let raw = write_raw_capture(
        temp.path(),
        "nested-codex-ids",
        &[
            r#"{"jsonrpc":"2.0","id":1,"result":{"thread":{"id":"thread-secret"}}}"#,
            r#"{"jsonrpc":"2.0","id":2,"result":{"turn":{"id":"turn-secret"}}}"#,
            r#"{"method":"item/started","params":{"item":{"id":"message-secret","type":"agentMessage"}}}"#,
            r#"{"method":"item/completed","params":{"item":{"id":"todo-secret","type":"todoList","items":[]}}}"#,
            r#"{"method":"item/started","params":{"item":{"id":"compaction-secret","type":"contextCompaction"}}}"#,
        ],
    );

    let report = sanitize_dir(&raw, &staging_dir(temp.path(), "nested-codex-ids")).unwrap();
    let payloads = sanitized_payloads(&report.events_bytes);
    assert_eq!(payloads[0]["result"]["thread"]["id"], "<THREAD_ID_1>");
    assert_eq!(payloads[1]["result"]["turn"]["id"], "<TURN_ID_1>");
    assert_eq!(payloads[2]["params"]["item"]["id"], "<TOOL_USE_ID_1>");
    assert_eq!(payloads[3]["params"]["item"]["id"], "<TOOL_USE_ID_2>");
    assert_eq!(payloads[4]["params"]["item"]["id"], "<TOOL_USE_ID_3>");
    let output = String::from_utf8(report.events_bytes).unwrap();
    for leaked in [
        "thread-secret",
        "turn-secret",
        "message-secret",
        "todo-secret",
        "compaction-secret",
    ] {
        assert!(!output.contains(leaked));
    }
}

/// Break caught: accepting non-JSON structured-channel frames makes user and assistant content
/// impossible to classify, so a raw line can bypass semantic redaction entirely.
#[test]
fn sanitizer_rejects_unparseable_stdout_before_writing_staging() {
    let temp = tempfile::tempdir().unwrap();
    let raw = write_raw_capture(
        temp.path(),
        "unparseable-stdout",
        &["unclassified human text", r#"{"level":"debug"}"#],
    );
    let output = staging_dir(temp.path(), "unparseable-stdout");

    let error = sanitize_dir(&raw, &output).unwrap_err();
    assert!(matches!(
        error,
        SanitizationError::UnparseableStructuredPayload { sequence: 1 }
    ));
    assert!(!output.exists());
}

/// Break caught: validating only event payloads lets a secret-like provider version or platform
/// metadata leak through the deterministic manifest.
#[test]
fn sanitizer_scans_every_manifest_string_before_writing_staging() {
    let temp = tempfile::tempdir().unwrap();
    let raw = write_raw_capture(temp.path(), "unsafe-metadata", &[r#"{"level":"debug"}"#]);
    let mut capture: Value =
        serde_json::from_slice(&std::fs::read(raw.join("capture.json")).unwrap()).unwrap();
    capture["cli_version"] = Value::String("sk-proj-version-secret".into());
    std::fs::write(
        raw.join("capture.json"),
        serde_json::to_vec_pretty(&capture).unwrap(),
    )
    .unwrap();
    let output = staging_dir(temp.path(), "unsafe-metadata");

    let error = sanitize_dir(&raw, &output).unwrap_err();
    assert!(matches!(error, SanitizationError::SecretLikeValue { .. }));
    assert!(!output.exists());
}

/// Break caught: a caller can otherwise direct reviewed artifacts outside the repository's
/// explicitly ignored staging tree.
#[test]
fn sanitizer_rejects_an_output_directory_outside_staging() {
    let temp = tempfile::tempdir().unwrap();
    let raw = write_raw_capture(temp.path(), "unsafe-output", &[r#"{"level":"debug"}"#]);
    let output = temp.path().join("reviewed-but-not-ignored");

    let error = sanitize_dir(&raw, &output).unwrap_err();
    assert!(matches!(error, SanitizationError::UnsafeOutputDirectory));
    assert!(!output.exists());
}

/// Break caught: a lexical `..` after the staging marker can escape the ignored tree while still
/// satisfying a naive component-pair check.
#[test]
fn sanitizer_rejects_parent_traversal_after_the_staging_directory() {
    let temp = tempfile::tempdir().unwrap();
    let raw = write_raw_capture(temp.path(), "traversal-output", &[r#"{"level":"debug"}"#]);
    let output = temp
        .path()
        .join(".comet-provider-captures")
        .join("staging")
        .join("..")
        .join("escaped");

    let error = sanitize_dir(&raw, &output).unwrap_err();
    assert!(matches!(error, SanitizationError::UnsafeOutputDirectory));
    assert!(
        !temp
            .path()
            .join(".comet-provider-captures/escaped")
            .exists()
    );
}

/// Break caught: allowing a known local root through unchanged, or failing to replace it when
/// embedded in prose, exposes machine-specific paths while making regeneration non-portable.
#[test]
fn sanitizer_replaces_allowlisted_paths_in_values_and_embedded_text() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    let cwd = repo.join("working-dir");
    std::fs::create_dir_all(&cwd).unwrap();
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap();
    let system_temp = std::env::temp_dir();
    let raw = write_raw_capture(
        temp.path(),
        "known-paths",
        &[&format!(
            r#"{{"type":"system","subtype":"init","cwd":{},"note":{},"repo_note":{},"home_note":{},"temp_note":{}}}"#,
            serde_json::to_string(&cwd).unwrap(),
            serde_json::to_string(&format!("opened {}\\src\\main.rs", cwd.display())).unwrap(),
            serde_json::to_string(&format!("repo {}\\Cargo.toml", repo.display())).unwrap(),
            serde_json::to_string(&format!("home {}\\profile", home.display())).unwrap(),
            serde_json::to_string(&format!("temp {}\\capture", system_temp.display())).unwrap()
        )],
    );
    let mut capture: Value =
        serde_json::from_slice(&std::fs::read(raw.join("capture.json")).unwrap()).unwrap();
    capture["command"]["cwd"] = Value::String(cwd.display().to_string());
    capture["redaction_roots"] = serde_json::json!({
        "cwd": cwd,
        "repo": repo,
        "home": home,
        "temp": system_temp
    });
    std::fs::write(
        raw.join("capture.json"),
        serde_json::to_vec_pretty(&capture).unwrap(),
    )
    .unwrap();

    let report = sanitize_dir(&raw, &staging_dir(temp.path(), "known-paths")).unwrap();
    let payloads = sanitized_payloads(&report.events_bytes);
    assert_eq!(payloads[0]["cwd"], "<CWD>");
    assert_eq!(payloads[0]["note"], "opened <CWD>\\src\\main.rs");
    assert_eq!(payloads[0]["repo_note"], "repo <REPO>\\Cargo.toml");
    assert_eq!(payloads[0]["home_note"], "home <HOME>\\profile");
    assert_eq!(payloads[0]["temp_note"], "temp <TEMP>\\capture");
    assert!(
        !String::from_utf8(report.manifest_bytes)
            .unwrap()
            .contains(&cwd.display().to_string())
    );
}

/// Break caught: Claude encodes the cwd into the basename beneath `memory_paths`, so replacing
/// only the literal HOME prefix leaves machine-specific workspace identity in reviewed evidence.
#[test]
fn sanitizer_replaces_every_nonempty_claude_memory_path_as_typed_local_metadata() {
    let temp = tempfile::tempdir().unwrap();
    let home = r"D:\capture-home";
    let cwd = r"C:\private workspace\project";
    let first_encoded_slug = "C--private-workspace-project";
    let second_encoded_slug = "Z--another-opaque-workspace";
    let payload = serde_json::json!({
        "type": "system",
        "subtype": "init",
        "memory_paths": {
            "auto": format!(r"{home}\.claude\projects\{first_encoded_slug}\memory\"),
            "nested": {
                "unknown_future_scope": format!(
                    r"{home}\.claude\projects\{second_encoded_slug}\memory\"
                )
            },
            "disabled": ""
        }
    })
    .to_string();
    let raw = write_raw_capture(temp.path(), "claude-memory-paths", &[&payload]);
    let capture_path = raw.join("capture.json");
    let mut capture: Value =
        serde_json::from_slice(&std::fs::read(&capture_path).unwrap()).unwrap();
    capture["redaction_roots"]["cwd"] = Value::String(cwd.into());
    capture["redaction_roots"]["home"] = Value::String(home.into());
    capture["command"]["cwd"] = Value::String(cwd.into());
    std::fs::write(&capture_path, serde_json::to_vec_pretty(&capture).unwrap()).unwrap();

    let report = sanitize_dir(&raw, &staging_dir(temp.path(), "claude-memory-paths")).unwrap();
    let payload = &sanitized_payloads(&report.events_bytes)[0];
    assert_eq!(
        payload["memory_paths"],
        serde_json::json!({
            "auto": "<CLAUDE_MEMORY_PATH_1>",
            "nested": {"unknown_future_scope": "<CLAUDE_MEMORY_PATH_2>"},
            "disabled": ""
        })
    );
    let manifest: Value = serde_json::from_slice(&report.manifest_bytes).unwrap();
    assert_eq!(manifest["redaction_counts"]["claude_memory_path"], 2);
    assert!(
        manifest["placeholders"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|definition| definition["kind"] == "claude_memory_path")
            .map(|definition| definition["placeholder"].as_str().unwrap())
            .eq(["<CLAUDE_MEMORY_PATH_1>", "<CLAUDE_MEMORY_PATH_2>"])
    );
    let sanitized = String::from_utf8(report.events_bytes).unwrap();
    for private_component in [home, cwd, first_encoded_slug, second_encoded_slug] {
        assert!(!sanitized.contains(private_component));
    }
}

/// Break caught: Claude assistant message IDs are provider-generated correlators, and leaving
/// them literal exposes run identity even when session, request, and tool IDs are typed correctly.
#[test]
fn sanitizer_types_claude_assistant_message_ids_by_shape_and_reuses_placeholders() {
    let temp = tempfile::tempdir().unwrap();
    let first_message_id = "opaque-provider-message-alpha";
    let second_message_id = "future-format-message-beta";
    let raw = write_raw_capture(
        temp.path(),
        "claude-message-ids",
        &[
            &serde_json::json!({
                "type": "stream_event",
                "request_id": "request-private",
                "session_id": "session-private",
                "event": {
                    "type": "message_start",
                    "message": {
                        "type": "message",
                        "role": "assistant",
                        "id": first_message_id,
                        "content": [{"type": "tool_use", "id": "tool-private"}]
                    }
                }
            })
            .to_string(),
            &serde_json::json!({
                "type": "assistant",
                "message": {"type": "message", "role": "assistant", "id": first_message_id}
            })
            .to_string(),
            &serde_json::json!({
                "type": "assistant",
                "message": {"type": "message", "role": "assistant", "id": second_message_id}
            })
            .to_string(),
            r#"{"type":"assistant","message":{"type":"message","role":"assistant","id":null}}"#,
            r#"{"type":"metadata","id":"safe-unrelated-literal"}"#,
        ],
    );

    let report = sanitize_dir(&raw, &staging_dir(temp.path(), "claude-message-ids")).unwrap();
    let payloads = sanitized_payloads(&report.events_bytes);
    assert_eq!(
        payloads[0]["event"]["message"]["id"],
        "<CLAUDE_MESSAGE_ID_1>"
    );
    assert_eq!(payloads[1]["message"]["id"], "<CLAUDE_MESSAGE_ID_1>");
    assert_eq!(payloads[2]["message"]["id"], "<CLAUDE_MESSAGE_ID_2>");
    assert_eq!(payloads[3]["message"]["id"], Value::Null);
    assert_eq!(payloads[4]["id"], "safe-unrelated-literal");
    assert_eq!(payloads[0]["request_id"], "<CLAUDE_REQUEST_ID_1>");
    assert_eq!(payloads[0]["session_id"], "<SESSION_ID_1>");
    assert_eq!(
        payloads[0]["event"]["message"]["content"][0]["id"],
        "<TOOL_USE_ID_1>"
    );

    let manifest: Value = serde_json::from_slice(&report.manifest_bytes).unwrap();
    assert_eq!(manifest["redaction_counts"]["claude_message_id"], 3);
    assert!(
        manifest["placeholders"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|definition| definition["kind"] == "claude_message_id")
            .map(|definition| definition["placeholder"].as_str().unwrap())
            .eq(["<CLAUDE_MESSAGE_ID_1>", "<CLAUDE_MESSAGE_ID_2>"])
    );

    let capture_path = raw.join("capture.json");
    let mut non_claude_capture: Value =
        serde_json::from_slice(&std::fs::read(&capture_path).unwrap()).unwrap();
    non_claude_capture["provider"] = Value::String("codex".into());
    std::fs::write(
        &capture_path,
        serde_json::to_vec_pretty(&non_claude_capture).unwrap(),
    )
    .unwrap();
    let non_claude = sanitize_dir(
        &raw,
        &staging_dir(temp.path(), "codex-message-shaped-object"),
    )
    .unwrap();
    assert_eq!(
        sanitized_payloads(&non_claude.events_bytes)[0]["event"]["message"]["id"],
        first_message_id,
        "a message-shaped object is not a Claude message ID outside Claude evidence"
    );
}

/// Break caught: the resume identifier is repeated in Claude's launch argv, outside event JSON,
/// and must reuse the exact typed session placeholder discovered from the captured wire frames.
#[test]
fn sanitizer_reuses_the_event_session_mapping_in_claude_resume_argv() {
    let temp = tempfile::tempdir().unwrap();
    let session_id = "opaque-session-to-resume";
    let raw = write_raw_capture(
        temp.path(),
        "claude-resume-argv",
        &[&serde_json::json!({
            "type": "result",
            "subtype": "success",
            "session_id": session_id
        })
        .to_string()],
    );
    let capture_path = raw.join("capture.json");
    let mut capture: Value =
        serde_json::from_slice(&std::fs::read(&capture_path).unwrap()).unwrap();
    capture["scenario"] = Value::String("resume".into());
    capture["command"]["args"] = serde_json::json!(["--print", format!("--resume={session_id}")]);
    std::fs::write(&capture_path, serde_json::to_vec_pretty(&capture).unwrap()).unwrap();

    let report = sanitize_dir(&raw, &staging_dir(temp.path(), "claude-resume-argv")).unwrap();
    let manifest: Value = serde_json::from_slice(&report.manifest_bytes).unwrap();
    assert_eq!(
        manifest["command"]["args"],
        serde_json::json!(["--print", "--resume=<SESSION_ID_1>"])
    );
    assert_eq!(manifest["redaction_counts"]["session_id"], 2);
    assert!(
        !String::from_utf8(report.manifest_bytes)
            .unwrap()
            .contains(session_id)
    );
}

#[test]
fn sanitizer_rejects_a_claude_resume_argv_identifier_absent_from_events() {
    let temp = tempfile::tempdir().unwrap();
    let raw = write_raw_capture(
        temp.path(),
        "claude-unmapped-resume-argv",
        &[r#"{"type":"result","subtype":"success","session_id":"captured-session"}"#],
    );
    let capture_path = raw.join("capture.json");
    let mut capture: Value =
        serde_json::from_slice(&std::fs::read(&capture_path).unwrap()).unwrap();
    capture["scenario"] = Value::String("resume".into());
    capture["command"]["args"] = serde_json::json!(["--resume=different-session"]);
    std::fs::write(&capture_path, serde_json::to_vec_pretty(&capture).unwrap()).unwrap();
    let output = staging_dir(temp.path(), "claude-unmapped-resume-argv");

    let error = sanitize_dir(&raw, &output).unwrap_err();
    assert!(matches!(
        error,
        SanitizationError::InvalidClaudeResumeCommand { .. }
    ));
    assert!(!error.to_string().contains("different-session"));
    assert!(!output.exists());
}

/// Break caught: recognizing only the happy-path prefix lets malformed, split, duplicate, absent,
/// or scenario-inappropriate resume arguments bypass the manifest's semantic-ID contract.
#[test]
fn sanitizer_enforces_the_exact_claude_resume_command_grammar() {
    let session_id = "captured-session";
    let cases = [
        ("missing", "resume", serde_json::json!(["--print"])),
        ("empty", "resume", serde_json::json!(["--resume="])),
        (
            "split",
            "resume",
            serde_json::json!(["--resume", session_id]),
        ),
        (
            "duplicate",
            "resume",
            serde_json::json!([
                format!("--resume={session_id}"),
                format!("--resume={session_id}")
            ]),
        ),
        (
            "malformed",
            "resume",
            serde_json::json!([format!("--resume-id={session_id}")]),
        ),
        (
            "mismatch",
            "resume",
            serde_json::json!(["--resume=another-session"]),
        ),
        (
            "unexpected-nonresume",
            "fresh-text",
            serde_json::json!([format!("--resume={session_id}")]),
        ),
    ];

    for (name, scenario, args) in cases {
        let temp = tempfile::tempdir().unwrap();
        let raw = write_raw_capture(
            temp.path(),
            name,
            &[&serde_json::json!({
                "type": "result",
                "subtype": "success",
                "session_id": session_id
            })
            .to_string()],
        );
        let capture_path = raw.join("capture.json");
        let mut capture: Value =
            serde_json::from_slice(&std::fs::read(&capture_path).unwrap()).unwrap();
        capture["scenario"] = Value::String(scenario.into());
        capture["command"]["args"] = args;
        std::fs::write(&capture_path, serde_json::to_vec_pretty(&capture).unwrap()).unwrap();
        let output = staging_dir(temp.path(), name);

        let error = sanitize_dir(&raw, &output).unwrap_err();
        assert!(
            matches!(error, SanitizationError::InvalidClaudeResumeCommand { .. }),
            "{name}: {error}"
        );
        assert!(!error.to_string().contains(session_id), "{name}");
        assert!(!error.to_string().contains("another-session"), "{name}");
        assert!(!output.exists(), "{name}");
    }
}

#[test]
fn sanitizer_requires_the_claude_resume_command_to_map_the_sole_session_semantic() {
    let temp = tempfile::tempdir().unwrap();
    let raw = write_raw_capture(
        temp.path(),
        "multiple-session-semantics",
        &[
            r#"{"type":"system","session_id":"first-session"}"#,
            r#"{"type":"result","subtype":"success","session_id":"second-session"}"#,
        ],
    );
    let capture_path = raw.join("capture.json");
    let mut capture: Value =
        serde_json::from_slice(&std::fs::read(&capture_path).unwrap()).unwrap();
    capture["scenario"] = Value::String("resume".into());
    capture["command"]["args"] = serde_json::json!(["--resume=first-session"]);
    std::fs::write(&capture_path, serde_json::to_vec_pretty(&capture).unwrap()).unwrap();
    let output = staging_dir(temp.path(), "multiple-session-semantics");

    let error = sanitize_dir(&raw, &output).unwrap_err();
    assert!(matches!(
        error,
        SanitizationError::InvalidClaudeResumeCommand { .. }
    ));
    assert!(!error.to_string().contains("first-session"));
    assert!(!error.to_string().contains("second-session"));
    assert!(!output.exists());
}

/// Break caught: thinking signatures are opaque provider correlators repeated across stream and
/// assistant frames; they need their own structural type instead of prefix or secret heuristics.
#[test]
fn sanitizer_types_nonempty_claude_thinking_signatures_by_shape() {
    let temp = tempfile::tempdir().unwrap();
    let first_signature = "opaque-signature-alpha";
    let second_signature = "future-signature-format-beta";
    let raw = write_raw_capture(
        temp.path(),
        "claude-thinking-signatures",
        &[
            &serde_json::json!({
                "type": "stream_event",
                "event": {
                    "type": "content_block_delta",
                    "delta": {"type": "signature_delta", "signature": first_signature}
                }
            })
            .to_string(),
            &serde_json::json!({
                "type": "assistant",
                "message": {
                    "type": "message",
                    "role": "assistant",
                    "content": [
                        {"type": "thinking", "thinking": "private", "signature": first_signature},
                        {"type": "thinking", "thinking": "private", "signature": second_signature},
                        {"type": "thinking", "thinking": "private", "signature": ""}
                    ]
                }
            })
            .to_string(),
        ],
    );

    let report = sanitize_dir(
        &raw,
        &staging_dir(temp.path(), "claude-thinking-signatures"),
    )
    .unwrap();
    let payloads = sanitized_payloads(&report.events_bytes);
    assert_eq!(
        payloads[0]["event"]["delta"]["signature"],
        "<CLAUDE_THINKING_SIGNATURE_1>"
    );
    assert_eq!(
        payloads[1]["message"]["content"][0]["signature"],
        "<CLAUDE_THINKING_SIGNATURE_1>"
    );
    assert_eq!(
        payloads[1]["message"]["content"][1]["signature"],
        "<CLAUDE_THINKING_SIGNATURE_2>"
    );
    assert_eq!(payloads[1]["message"]["content"][2]["signature"], "");
    let manifest: Value = serde_json::from_slice(&report.manifest_bytes).unwrap();
    assert_eq!(manifest["redaction_counts"]["claude_thinking_signature"], 3);

    let capture_path = raw.join("capture.json");
    let mut codex_capture: Value =
        serde_json::from_slice(&std::fs::read(&capture_path).unwrap()).unwrap();
    codex_capture["provider"] = Value::String("codex".into());
    std::fs::write(
        &capture_path,
        serde_json::to_vec_pretty(&codex_capture).unwrap(),
    )
    .unwrap();
    let codex = sanitize_dir(
        &raw,
        &staging_dir(temp.path(), "codex-thinking-shaped-values"),
    )
    .unwrap();
    assert_eq!(
        sanitized_payloads(&codex.events_bytes)[0]["event"]["delta"]["signature"],
        first_signature
    );
}

/// Break caught: a one-character assistant delta used to be replaced as a substring in every
/// later JSON string, corrupting stable skill and slash-command metadata that happened to contain
/// the same character. Semantic redaction is structural and provider diagnostics are whole fields.
#[test]
fn sanitizer_does_not_apply_assistant_semantics_to_unrelated_metadata() {
    let temp = tempfile::tempdir().unwrap();
    let raw = write_raw_capture(
        temp.path(),
        "claude-short-assistant-delta",
        &[
            &serde_json::json!({
                "type": "system",
                "subtype": "init",
                "skills": ["deep-research", "r"],
                "slash_commands": ["review", "r"]
            })
            .to_string(),
            &serde_json::json!({
                "type": "stream_event",
                "event": {
                    "type": "content_block_delta",
                    "delta": {"type": "text_delta", "text": "r"}
                }
            })
            .to_string(),
            &serde_json::json!({"level": "debug", "message": "r"}).to_string(),
            &serde_json::json!({
                "type": "stream_event",
                "event": {
                    "type": "content_block_delta",
                    "delta": {"type": "input_json_delta", "partial_json": "{\"skill\":\"r\"}"}
                }
            })
            .to_string(),
            &serde_json::json!({
                "type": "stream_event",
                "event": {
                    "type": "content_block_delta",
                    "delta": {"type": "input_json_delta", "partial_json": "{\"skill\":\"r\"}"}
                }
            })
            .to_string(),
        ],
    );

    let report = sanitize_dir(
        &raw,
        &staging_dir(temp.path(), "claude-short-assistant-delta"),
    )
    .unwrap();
    let payloads = sanitized_payloads(&report.events_bytes);
    assert_eq!(
        payloads[0],
        serde_json::json!({
            "type": "system",
            "subtype": "init",
            "skills": ["deep-research", "r"],
            "slash_commands": ["review", "r"]
        }),
        "stable init metadata must remain byte-for-byte semantically unchanged"
    );
    assert_eq!(payloads[1]["event"]["delta"]["text"], "<ASSISTANT_PROSE_1>");
    assert_eq!(payloads[2]["message"], "<PROVIDER_PROSE_1>");
    assert_eq!(
        payloads[3]["event"]["delta"]["partial_json"],
        "<ASSISTANT_PROSE_2>"
    );
    assert_eq!(
        payloads[4]["event"]["delta"]["partial_json"],
        "<ASSISTANT_PROSE_2>"
    );
    let manifest: Value = serde_json::from_slice(&report.manifest_bytes).unwrap();
    assert_eq!(manifest["redaction_counts"]["assistant_prose"], 3);
}

/// Break caught: isolated logged-out discovery cannot be sanitized because its explicitly
/// configured CODEX_HOME is not one of the raw evidence's captured redaction roots.
#[test]
fn sanitizer_replaces_the_captured_codex_home() {
    let temp = tempfile::tempdir().unwrap();
    let raw = write_raw_capture(temp.path(), "codex-home", &[r#"{"level":"debug"}"#]);
    let capture_path = raw.join("capture.json");
    let mut capture: Value =
        serde_json::from_slice(&std::fs::read(&capture_path).unwrap()).unwrap();
    capture["redaction_roots"]["codex_home"] = Value::String(r"D:\isolated-codex-home".into());
    capture["command"]["configured_env"]["CODEX_HOME"] =
        Value::String(r"D:\isolated-codex-home".into());
    std::fs::write(&capture_path, serde_json::to_vec_pretty(&capture).unwrap()).unwrap();

    let report = sanitize_dir(&raw, &staging_dir(temp.path(), "codex-home")).unwrap();
    let manifest: Value = serde_json::from_slice(&report.manifest_bytes).unwrap();
    assert_eq!(
        manifest["command"]["configured_env"]["CODEX_HOME"],
        "<CODEX_HOME>"
    );
    assert_eq!(manifest["redaction_counts"]["codex_home_path"], 1);
}

#[test]
fn sanitizer_replaces_the_captured_approval_target() {
    let temp = tempfile::tempdir().unwrap();
    let target = r"D:\bounded approval target";
    let payload = format!(
        r#"{{"method":"item/commandExecution/requestApproval","params":{{"command":{}}}}}"#,
        serde_json::to_string(&format!("write {target}\\approval-marker.txt")).unwrap()
    );
    let raw = write_raw_capture(temp.path(), "approval-target", &[&payload]);
    let capture_path = raw.join("capture.json");
    let mut capture: Value =
        serde_json::from_slice(&std::fs::read(&capture_path).unwrap()).unwrap();
    capture["redaction_roots"]["approval_target"] = Value::String(target.into());
    std::fs::write(&capture_path, serde_json::to_vec_pretty(&capture).unwrap()).unwrap();

    let report = sanitize_dir(&raw, &staging_dir(temp.path(), "approval-target")).unwrap();
    let payloads = sanitized_payloads(&report.events_bytes);
    assert_eq!(
        payloads[0]["params"]["command"],
        "write <APPROVAL_TARGET>\\approval-marker.txt"
    );
    let manifest: Value = serde_json::from_slice(&report.manifest_bytes).unwrap();
    assert_eq!(manifest["redaction_counts"]["approval_target_path"], 1);
    assert!(
        manifest["placeholders"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| {
                entry["placeholder"] == "<APPROVAL_TARGET>"
                    && entry["kind"] == "approval_target_path"
            })
    );
}

/// Break caught: global value replacement can turn a protocol discriminator into a placeholder
/// when its literal happens to equal a dynamic identifier.
#[test]
fn sanitizer_never_replaces_protocol_discriminators_on_value_collision() {
    let temp = tempfile::tempdir().unwrap();
    let raw = write_raw_capture(
        temp.path(),
        "discriminator-collision",
        &[r#"{"type":"user","session_id":"user","message":{"role":"user","content":"prompt"}}"#],
    );

    let report = sanitize_dir(&raw, &staging_dir(temp.path(), "discriminator-collision")).unwrap();
    let payload = &sanitized_payloads(&report.events_bytes)[0];
    assert_eq!(payload["type"], "user");
    assert_eq!(payload["message"]["role"], "user");
    assert_eq!(payload["session_id"], "<SESSION_ID_1>");
}

/// Break caught: weakening the post-redaction scan permits an absolute machine path not covered
/// by the explicit HOME/REPO/CWD/TEMP allowlist to enter staging.
#[test]
fn sanitizer_rejects_unknown_unix_drive_unc_and_verbatim_windows_paths() {
    let cases = [
        ("unix", r#"{"path":"/srv/private/secret.txt"}"#),
        ("drive", r#"{"path":"D:\\private\\secret.txt"}"#),
        ("unc", r#"{"path":"\\\\server\\share\\secret.txt"}"#),
        (
            "verbatim-windows",
            r#"{"path":"\\\\?\\D:\\private\\secret.txt"}"#,
        ),
    ];

    for (name, payload) in cases {
        let temp = tempfile::tempdir().unwrap();
        let raw = write_raw_capture(temp.path(), name, &[payload]);
        let output = staging_dir(temp.path(), name);
        let error = sanitize_dir(&raw, &output).unwrap_err();
        assert!(
            matches!(error, SanitizationError::UnrecognizedAbsolutePath { .. }),
            "{name} returned {error:?}"
        );
        assert!(!output.exists(), "{name} wrote rejected staging output");
    }
}

/// Break caught: substring replacement can treat an allowlisted root as a prefix of a different
/// absolute path, hide its drive/root marker, and let the unknown path escape rejection.
#[test]
fn sanitizer_does_not_allow_path_prefix_collisions() {
    let temp = tempfile::tempdir().unwrap();
    let cwd = PathBuf::from(r"D:\allowed\repo");
    let raw = write_raw_capture(
        temp.path(),
        "path-prefix-collision",
        &[&format!(
            r#"{{"path":{}}}"#,
            serde_json::to_string(r"D:\allowed\repo-other\secret.txt").unwrap()
        )],
    );
    let mut capture: Value =
        serde_json::from_slice(&std::fs::read(raw.join("capture.json")).unwrap()).unwrap();
    capture["command"]["cwd"] = Value::String(cwd.display().to_string());
    capture["redaction_roots"]["cwd"] = Value::String(cwd.display().to_string());
    std::fs::write(
        raw.join("capture.json"),
        serde_json::to_vec_pretty(&capture).unwrap(),
    )
    .unwrap();

    let error = sanitize_dir(&raw, &staging_dir(temp.path(), "path-prefix-collision")).unwrap_err();
    assert!(matches!(
        error,
        SanitizationError::UnrecognizedAbsolutePath { .. }
    ));
}

/// Break caught: textual prefix replacement can bless an allowed root followed by `..`, and a
/// detector that recognizes only backslash UNC paths misses the equivalent forward-slash form.
#[test]
fn sanitizer_rejects_allowlist_traversal_and_forward_slash_unc_paths() {
    for (name, path) in [
        (
            "allowlist-traversal",
            r"D:\allowed\repo\..\private\secret.txt",
        ),
        ("forward-unc", "//server/share/private/secret.txt"),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let raw = write_raw_capture(
            temp.path(),
            name,
            &[&format!(
                r#"{{"path":{}}}"#,
                serde_json::to_string(path).unwrap()
            )],
        );
        let mut capture: Value =
            serde_json::from_slice(&std::fs::read(raw.join("capture.json")).unwrap()).unwrap();
        capture["command"]["cwd"] = Value::String(r"D:\allowed\repo".into());
        capture["redaction_roots"]["cwd"] = Value::String(r"D:\allowed\repo".into());
        std::fs::write(
            raw.join("capture.json"),
            serde_json::to_vec_pretty(&capture).unwrap(),
        )
        .unwrap();

        let error = sanitize_dir(&raw, &staging_dir(temp.path(), name)).unwrap_err();
        assert!(
            matches!(error, SanitizationError::UnrecognizedAbsolutePath { .. }),
            "{name} returned {error:?}"
        );
    }
}

/// Break caught: treating credential-bearing field names or recognizable token/key material as
/// ordinary strings can publish a usable credential in a sanitized artifact.
#[test]
fn sanitizer_rejects_secret_fields_provider_tokens_and_private_keys() {
    let cases = [
        (
            "authorization",
            r#"{"authorization":"Bearer definitely-not-for-review"}"#,
        ),
        ("api-key", r#"{"apiKey":"value-without-a-token-prefix"}"#),
        (
            "anthropic-token",
            r#"{"message":"sk-ant-api03-secretvalue"}"#,
        ),
        ("openai-token", r#"{"message":"sk-proj-secretvalue"}"#),
        (
            "private-key",
            r#"{"message":"-----BEGIN OPENSSH PRIVATE KEY-----"}"#,
        ),
    ];

    for (name, payload) in cases {
        let temp = tempfile::tempdir().unwrap();
        let raw = write_raw_capture(temp.path(), name, &[payload]);
        let error = sanitize_dir(&raw, &staging_dir(temp.path(), name)).unwrap_err();
        assert!(
            matches!(
                error,
                SanitizationError::SecretLikeField { .. }
                    | SanitizationError::SecretLikeValue { .. }
            ),
            "{name} returned {error:?}"
        );
        assert!(!error.to_string().contains("secretvalue"));
        assert!(!error.to_string().contains("definitely-not-for-review"));
    }
}

/// Break caught: validating only JSON values lets sensitive object keys through, while building
/// an error location from an untrusted key repeats the secret in diagnostics.
#[test]
fn sanitizer_rejects_sensitive_object_keys_without_echoing_them() {
    for (name, raw_key) in [
        ("secret-key-name", "sk-proj-key-name-secret"),
        ("absolute-path-key", r"D:\private\key-name-secret"),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let payload = serde_json::json!({raw_key: "opaque"}).to_string();
        let raw = write_raw_capture(temp.path(), name, &[&payload]);
        let output = staging_dir(temp.path(), name);

        let error = sanitize_dir(&raw, &output).unwrap_err();
        let display = error.to_string();
        assert!(matches!(
            error,
            SanitizationError::SecretLikeValue { .. }
                | SanitizationError::UnrecognizedAbsolutePath { .. }
        ));
        assert!(!display.contains(raw_key));
        assert!(!display.contains("key-name-secret"));
        assert!(!output.exists());
    }
}

/// Break caught: sanitizing a clone of an allowlisted path key and discarding the clone permits
/// the original machine-specific key to serialize unchanged.
#[test]
fn sanitizer_rejects_allowlisted_path_keys_without_echoing_them() {
    let temp = tempfile::tempdir().unwrap();
    let raw_key = r"D:\allowed\repo\private-key-name";
    let payload = serde_json::json!({raw_key: "opaque"}).to_string();
    let raw = write_raw_capture(temp.path(), "allowlisted-path-key", &[&payload]);
    let mut capture: Value =
        serde_json::from_slice(&std::fs::read(raw.join("capture.json")).unwrap()).unwrap();
    capture["redaction_roots"]["cwd"] = Value::String(r"D:\allowed\repo".into());
    std::fs::write(
        raw.join("capture.json"),
        serde_json::to_vec_pretty(&capture).unwrap(),
    )
    .unwrap();
    let output = staging_dir(temp.path(), "allowlisted-path-key");

    let error = sanitize_dir(&raw, &output).unwrap_err();
    assert!(!error.to_string().contains(raw_key));
    assert!(!error.to_string().contains("private-key-name"));
    assert!(!output.exists());
}

/// Break caught: credential fields with opaque values bypass prefix scanning, while an overbroad
/// `token` name rule would incorrectly reject ordinary numeric usage counters.
#[test]
fn sanitizer_rejects_opaque_credential_fields_but_keeps_token_counters() {
    for field in [
        "token",
        "refreshToken",
        "sessionToken",
        "clientSecret",
        "privateKey",
        "authorization",
        "apiKey",
        "anthropicApiKey",
        "openai_api_key",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let payload = serde_json::json!({field: "opaque-value"}).to_string();
        let raw = write_raw_capture(temp.path(), field, &[&payload]);
        let error = sanitize_dir(&raw, &staging_dir(temp.path(), field)).unwrap_err();
        assert!(
            matches!(error, SanitizationError::SecretLikeField { .. }),
            "{field} returned {error:?}"
        );
    }

    let temp = tempfile::tempdir().unwrap();
    let raw = write_raw_capture(
        temp.path(),
        "token-counters",
        &[
            r#"{"usage":{"input_tokens":10,"outputTokens":20,"max_tokens":30,"totalTokenCount":60}}"#,
        ],
    );
    let report = sanitize_dir(&raw, &staging_dir(temp.path(), "token-counters")).unwrap();
    assert_eq!(
        sanitized_payloads(&report.events_bytes)[0]["usage"],
        serde_json::json!({
            "input_tokens": 10,
            "outputTokens": 20,
            "max_tokens": 30,
            "totalTokenCount": 60
        })
    );
}

/// Break caught: exact credential names miss provider-prefixed token families, and counter-name
/// exemptions accept opaque strings that are credentials disguised as usage metadata.
#[test]
fn sanitizer_classifies_credential_families_and_requires_numeric_counters() {
    for field in [
        "apiToken",
        "githubApiToken",
        "providerAccessToken",
        "oauthRefreshToken",
        "serviceClientSecret",
        "accountPrivateKey",
        "proxyAuthorization",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let payload = serde_json::json!({field: "opaque-value"}).to_string();
        let raw = write_raw_capture(temp.path(), field, &[&payload]);
        assert!(matches!(
            sanitize_dir(&raw, &staging_dir(temp.path(), field)),
            Err(SanitizationError::SecretLikeField { .. })
        ));
    }

    let temp = tempfile::tempdir().unwrap();
    let raw = write_raw_capture(
        temp.path(),
        "string-token-counter",
        &[r#"{"usage":{"input_tokens":"opaque-value"}}"#],
    );
    assert!(matches!(
        sanitize_dir(&raw, &staging_dir(temp.path(), "string-token-counter")),
        Err(SanitizationError::SecretLikeField { .. })
    ));

    let temp = tempfile::tempdir().unwrap();
    let raw = write_raw_capture(
        temp.path(),
        "unrelated-token-field",
        &[r#"{"tokenizerMode":"provider-default"}"#],
    );
    let report = sanitize_dir(&raw, &staging_dir(temp.path(), "unrelated-token-field")).unwrap();
    assert_eq!(
        sanitized_payloads(&report.events_bytes)[0]["tokenizerMode"],
        "provider-default"
    );
}

/// Break caught: scanning for credentials before semantic replacement rejects safe redaction of
/// a token pasted as user-authored content, while skipping user replacement publishes it.
#[test]
fn sanitizer_replaces_secret_looking_user_text_before_the_fail_closed_scan() {
    let temp = tempfile::tempdir().unwrap();
    let raw = write_raw_capture(
        temp.path(),
        "token-in-user-text",
        &[r#"{"type":"user","message":{"role":"user","content":"sk-proj-user-pasted-token"}}"#],
    );

    let report = sanitize_dir(&raw, &staging_dir(temp.path(), "token-in-user-text")).unwrap();
    assert_eq!(
        sanitized_payloads(&report.events_bytes)[0]["message"]["content"],
        "<USER_TEXT_1>"
    );
}

/// Break caught: placeholder allocation from hash iteration, wall-clock manifest fields, or
/// output-directory metadata makes identical raw evidence produce different review bytes.
#[test]
fn sanitizer_is_byte_deterministic_and_uses_encounter_order() {
    let temp = tempfile::tempdir().unwrap();
    let raw = write_raw_capture(
        temp.path(),
        "deterministic",
        &[
            r#"{"type":"user","message":{"role":"user","content":"first prompt"}}"#,
            r#"{"type":"user","message":{"role":"user","content":"second prompt"}}"#,
            r#"{"level":"debug","message":"safe diagnostic"}"#,
        ],
    );

    let first = sanitize_dir(&raw, &staging_dir(temp.path(), "first")).unwrap();
    let second = sanitize_dir(&raw, &staging_dir(temp.path(), "second")).unwrap();
    assert_eq!(first.events_bytes, second.events_bytes);
    assert_eq!(first.manifest_bytes, second.manifest_bytes);

    let payloads = sanitized_payloads(&first.events_bytes);
    assert_eq!(payloads[0]["message"]["content"], "<USER_TEXT_1>");
    assert_eq!(payloads[1]["message"]["content"], "<USER_TEXT_2>");
}

/// Break caught: deriving manifest metadata from the raw directory makes byte-identical captures
/// sanitize differently when copied or renamed for review.
#[test]
fn sanitizer_output_is_independent_of_raw_directory_name_and_location() {
    let first_root = tempfile::tempdir().unwrap();
    let second_root = tempfile::tempdir().unwrap();
    let events = [
        r#"{"type":"user","session_id":"same-session","message":{"role":"user","content":"same prompt"}}"#,
        r#"{"type":"assistant","message":{"content":[{"type":"text","text":"same answer"}]}}"#,
    ];
    let first_raw = write_raw_capture(first_root.path(), "first-raw-name", &events);
    let second_raw = write_raw_capture(second_root.path(), "second-raw-name", &events);
    let first_capture = std::fs::read(first_raw.join("capture.json")).unwrap();
    let second_capture = std::fs::read(second_raw.join("capture.json")).unwrap();
    assert_eq!(first_capture, second_capture);

    let first = sanitize_dir(&first_raw, &staging_dir(first_root.path(), "first-output")).unwrap();
    let second = sanitize_dir(
        &second_raw,
        &staging_dir(second_root.path(), "second-output"),
    )
    .unwrap();

    assert_eq!(first.events_bytes, second.events_bytes);
    assert_eq!(first.manifest_bytes, second.manifest_bytes);
    let manifest = String::from_utf8(first.manifest_bytes).unwrap();
    for ambient in ["first-raw-name", "second-raw-name"] {
        assert!(!manifest.contains(ambient));
    }
    assert!(!manifest.contains(&first_raw.display().to_string()));
    assert!(!manifest.contains(&second_raw.display().to_string()));
}

/// Break caught: mapping lock contention to a generic write error gives callers no bounded retry
/// signal, while deleting a pre-existing lock would silently steal another publisher's ownership.
#[test]
fn sanitizer_reports_busy_without_stealing_an_existing_publication_lock() {
    let temp = tempfile::tempdir().unwrap();
    let raw = write_raw_capture(temp.path(), "busy-capture", &[r#"{"level":"debug"}"#]);
    let output = staging_dir(temp.path(), "private-destination-name");
    let parent = output.parent().unwrap();
    std::fs::create_dir_all(parent).unwrap();
    let lock = parent.join(".private-destination-name.publish.lock");
    std::fs::write(&lock, b"owned elsewhere").unwrap();

    let error = sanitize_dir(&raw, &output).unwrap_err();
    assert!(matches!(error, SanitizationError::PublicationBusy { .. }));
    assert!(!error.to_string().contains("private-destination-name"));
    assert_eq!(std::fs::read(&lock).unwrap(), b"owned elsewhere");
    assert!(!output.exists());
}

/// Break caught: deriving repository/home/temp roots during sanitization makes identical raw bytes
/// produce different artifacts when the checkout or host environment changes after capture.
#[test]
fn sanitizer_uses_only_captured_redaction_roots_after_filesystem_changes() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("captured-repo");
    let cwd = repo.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let raw = write_raw_capture(
        temp.path(),
        "captured-roots",
        &[&format!(
            r#"{{"note":{}}}"#,
            serde_json::to_string(&format!("repo {}\\Cargo.toml", repo.display())).unwrap()
        )],
    );
    let mut capture: Value =
        serde_json::from_slice(&std::fs::read(raw.join("capture.json")).unwrap()).unwrap();
    capture["command"]["cwd"] = Value::String(cwd.display().to_string());
    capture["redaction_roots"] = serde_json::json!({
        "cwd": cwd,
        "repo": repo,
        "home": r"C:\captured-home",
        "temp": r"C:\captured-temp"
    });
    std::fs::write(
        raw.join("capture.json"),
        serde_json::to_vec_pretty(&capture).unwrap(),
    )
    .unwrap();

    let first = sanitize_dir(&raw, &staging_dir(temp.path(), "captured-roots-first")).unwrap();
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    let second = sanitize_dir(&raw, &staging_dir(temp.path(), "captured-roots-second")).unwrap();

    assert_eq!(first.events_bytes, second.events_bytes);
    assert_eq!(first.manifest_bytes, second.manifest_bytes);
    assert_eq!(
        sanitized_payloads(&first.events_bytes)[0]["note"],
        "repo <REPO>\\Cargo.toml"
    );
}

/// Break caught: manifest accounting can silently omit a redaction category or count definitions
/// rather than actual replacements, preventing a reviewer from auditing what changed.
#[test]
fn sanitizer_manifest_accounts_for_placeholder_definitions_and_counts() {
    let temp = tempfile::tempdir().unwrap();
    let raw = write_raw_capture(
        temp.path(),
        "accounting",
        &[
            r#"{"type":"user","session_id":"same-session","message":{"role":"user","content":"first prompt"}}"#,
            r#"{"type":"user","session_id":"same-session","message":{"role":"user","content":"second prompt"}}"#,
            r#"{"level":"debug","message":"safe diagnostic"}"#,
        ],
    );

    let report = sanitize_dir(&raw, &staging_dir(temp.path(), "accounting")).unwrap();
    let manifest: Value = serde_json::from_slice(&report.manifest_bytes).unwrap();
    assert_eq!(manifest["schema_version"], 1);
    assert_eq!(manifest["source"], "capture.json");
    assert_eq!(manifest["provider"], "claude");
    assert_eq!(manifest["captured_at_unix_ms"], 1786464000123i64);
    assert_eq!(manifest["scenario"], "model-discovery");
    assert_eq!(
        manifest["purpose"],
        "capture Claude's token-free model initialize reply"
    );
    assert_eq!(manifest["redaction_counts"]["session_id"], 2);
    assert_eq!(manifest["redaction_counts"]["user_text"], 2);
    assert_eq!(manifest["redaction_counts"]["provider_prose"], 1);
    assert_eq!(
        manifest["placeholders"],
        serde_json::json!([
            {"placeholder": "<SESSION_ID_1>", "kind": "session_id"},
            {"placeholder": "<USER_TEXT_1>", "kind": "user_text"},
            {"placeholder": "<USER_TEXT_2>", "kind": "user_text"},
            {"placeholder": "<PROVIDER_PROSE_1>", "kind": "provider_prose"}
        ])
    );
    assert_eq!(
        std::fs::read(report.events_path).unwrap(),
        report.events_bytes
    );
    assert_eq!(
        std::fs::read(report.manifest_path).unwrap(),
        report.manifest_bytes
    );
}
