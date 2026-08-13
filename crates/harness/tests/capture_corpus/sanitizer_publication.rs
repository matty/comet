use super::support::*;

use comet_harness::capture::{SanitizationError, sanitize_dir};
use serde_json::Value;

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
