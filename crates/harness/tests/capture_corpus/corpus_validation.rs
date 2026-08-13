use super::support::*;

use std::path::Path;

use comet_harness::capture::{CorpusError, sanitize_dir, selected_payload, validate_corpus};
use serde_json::Value;

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

/// Break caught: Git's Windows checkout turns trailing blank LF records into CRLF records, and a
/// validator that skips only zero-length slices tries to decode the remaining `\r` as JSON.
#[test]
fn corpus_accepts_crlf_blank_records_from_windows_checkouts() {
    let temp = tempfile::tempdir().unwrap();
    write_valid_literal_corpus(temp.path());
    let events_path = temp
        .path()
        .join("claude/2.1.227/model-discovery/events.jsonl");
    let events = std::fs::read_to_string(&events_path)
        .unwrap()
        .replace('\n', "\r\n");
    std::fs::write(&events_path, format!("{events}\r\n\r\n")).unwrap();

    let errors = validate_corpus(temp.path());

    assert!(errors.is_empty(), "CRLF corpus returned {errors:#?}");
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

/// Break caught: index drift can silently reintroduce an unsupported pending claim or delete a
/// retained claim whose literal evidence is still required.
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
            "codex-steer-reply-before-completion",
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
            "codex-model-cwd-invariance",
            "codex-model-logged-out-fallback",
        ])
    );

    let errors = validate_corpus(&corpus_root);
    assert!(errors.is_empty(), "inventory errors: {errors:#?}");
}

/// Break caught: a retained corpus claim can outlive the source rationale it supports, leaving a
/// maintainer unable to trace the claim from the consumer named by the index.
#[test]
fn every_retained_claim_is_named_by_its_consumer_source() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let repo_root = manifest_dir.parent().unwrap().parent().unwrap();
    let index: Value = serde_json::from_slice(
        &std::fs::read(manifest_dir.join("tests/corpus/index.json")).unwrap(),
    )
    .unwrap();
    for claim in index["claims"].as_array().unwrap() {
        let id = claim["id"].as_str().unwrap();
        let consumer = claim["consumer"].as_str().unwrap();
        let (relative_path, _) = consumer
            .split_once(':')
            .expect("consumer uses path:anchor syntax");
        let source = std::fs::read_to_string(repo_root.join(relative_path)).unwrap();
        assert!(
            source.contains(id),
            "consumer source {relative_path} does not name corpus claim {id}"
        );
    }
}
