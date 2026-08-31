use super::support::*;

use comet_capture::{SanitizationError, sanitize_dir};
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

    // `.message.content` (a bare string, not the `.message.content[].type` array-element form)
    // is not on the allowlist, so it numbers into the generic bucket -- the placeholder names
    // changed, not the property this test proves, which is the two-run byte comparison above.
    let payloads = sanitized_payloads(&first.events_bytes);
    assert_eq!(payloads[0]["message"]["content"], "<V1>");
    assert_eq!(payloads[1]["message"]["content"], "<V2>");
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
///
/// Under the allowlist an *unlisted* field like the old `"note"` no longer partially substitutes
/// a captured root into its value -- it's replaced whole, root and all, same as anything else
/// not on the list. Partial substitution only still happens for a value at an *allowlisted*
/// path, so this carries the captured repo root inside `.claude_code_version` (a plain string on
/// `claude.txt`) instead: root substitution still runs before the fail-closed scan even on a
/// kept path (`Redactor::sanitize_scalar` calls `sanitize_paths_and_validate`, not a bare
/// `contains_absolute_path` check), which is what makes the root's replacement observable here.
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
            r#"{{"claude_code_version":{}}}"#,
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
        sanitized_payloads(&first.events_bytes)[0]["claude_code_version"],
        "repo <REPO>\\Cargo.toml"
    );
}

/// Break caught: `cwd`/`repo`/`home`/`temp` are proven above, but
/// `Redactor::new` (`sanitize.rs:538-552`) wires three more roots the same
/// way -- `codex_home`, `approval_target`, and `trusted_powershell`, each
/// derived by `recording.rs` for a Codex or approval capture -- and nothing
/// exercised them: each of the three could be deleted from `Redactor::new`
/// without failing anything before this test.
#[test]
fn sanitizer_substitutes_the_codex_and_approval_specific_redaction_roots() {
    let cases = [
        (
            "codex-home-root",
            "codex_home",
            r"C:\captured-codex-home",
            "<CODEX_HOME>",
        ),
        (
            "approval-target-root",
            "approval_target",
            r"C:\captured-approval-target",
            "<APPROVAL_TARGET>",
        ),
        (
            "trusted-powershell-root",
            "trusted_powershell",
            r"C:\captured-trusted.ps1",
            "<TRUSTED_POWERSHELL>",
        ),
    ];

    for (name, root_key, root_value, placeholder) in cases {
        let temp = tempfile::tempdir().unwrap();
        let raw = write_raw_capture(
            temp.path(),
            name,
            &[&format!(
                r#"{{"claude_code_version":{}}}"#,
                serde_json::to_string(&format!("root {root_value}\\extra")).unwrap()
            )],
        );
        let capture_path = raw.join("capture.json");
        let mut capture: Value =
            serde_json::from_slice(&std::fs::read(&capture_path).unwrap()).unwrap();
        capture["redaction_roots"][root_key] = Value::String(root_value.into());
        std::fs::write(&capture_path, serde_json::to_vec_pretty(&capture).unwrap()).unwrap();

        let report = sanitize_dir(&raw, &staging_dir(temp.path(), name)).unwrap();
        assert_eq!(
            sanitized_payloads(&report.events_bytes)[0]["claude_code_version"],
            format!("root {placeholder}\\extra"),
            "{name}"
        );
    }
}

/// Break caught: the manifest carrying more than bare provenance again (`placeholders` and
/// `redaction_counts` were dropped at the stage-6 promotion), or placeholder identity failing to
/// collide equal values into one token and silently breaking a cross-frame join.
///
/// `session_id` repeats the identical value `"same-session"` across both events, so both
/// occurrences must collide into the same `<SESSION_1>` placeholder.
#[test]
fn sanitizer_manifest_is_bare_provenance_and_placeholders_still_collide_equal_values() {
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
    assert_eq!(manifest["provider"], "claude");
    assert_eq!(manifest["captured_at_unix_ms"], 1786464000123i64);
    assert_eq!(manifest["scenario"], "model-discovery");
    assert_eq!(
        manifest["purpose"],
        "capture Claude's token-free model initialize reply"
    );
    assert!(
        manifest.get("placeholders").is_none(),
        "the manifest is provenance only -- placeholders must not reappear: {manifest}"
    );
    assert!(
        manifest.get("redaction_counts").is_none(),
        "the manifest is provenance only -- redaction_counts must not reappear: {manifest}"
    );

    let payloads = sanitized_payloads(&report.events_bytes);
    assert_eq!(payloads[0]["session_id"], "<SESSION_1>");
    assert_eq!(
        payloads[1]["session_id"], "<SESSION_1>",
        "the identical session_id in both events must collide into one placeholder"
    );
    assert_ne!(
        payloads[0]["message"]["content"], payloads[1]["message"]["content"],
        "the two distinct prompt texts must not collide with each other"
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

/// Break caught: a capture whose own program lives outside every declared
/// redaction root cannot be sanitized at all, and the message names a JSON
/// position rather than the reason.
///
/// Real failure, not hypothetical: the ACP adapter rows spawn `node`, and a
/// system-wide install resolves to `C:\Program Files\nodejs\node.exe`, which
/// is under none of `RedactionRoots`' categories. `sanitize_paths_and_validate`
/// hard-fails on a leftover absolute path rather than publishing it, so
/// `comet-provider-sanitize` rejected the capture with
/// `capture contains an unrecognized absolute path at command.object[5]`.
/// The 2026-08-28 promotion worked around it by re-recording with
/// `--executable` pointed at a different interpreter that happened to live
/// under `<HOME>`; an operator with only a standard install (nvm, Program
/// Files, `/usr/local/bin`, most package managers) has nothing to reach for.
///
/// The program's own directory is a root like any other now, added only when
/// nothing already covers it -- so a program under `<HOME>` keeps spelling
/// itself `<HOME>\...`, exactly as every promoted capture does today.
#[test]
fn a_program_outside_every_declared_root_still_sanitizes() {
    let temp = tempfile::tempdir().unwrap();
    let raw = write_raw_capture(
        temp.path(),
        "system-interpreter",
        &[r#"{"type":"control_response","response":{"subtype":"success"}}"#],
    );
    let capture_path = raw.join("capture.json");
    let mut capture: Value =
        serde_json::from_slice(&std::fs::read(&capture_path).unwrap()).unwrap();
    capture["command"]["program"] = Value::String(r"C:\Program Files\nodejs\node.exe".into());
    std::fs::write(&capture_path, serde_json::to_vec_pretty(&capture).unwrap()).unwrap();

    let report = sanitize_dir(&raw, &staging_dir(temp.path(), "system-interpreter"))
        .expect("a program outside every root must not reject the capture");
    let manifest: Value = serde_json::from_slice(&report.manifest_bytes).unwrap();
    let command = manifest["command"]["program"]
        .as_str()
        .expect("the manifest records the program it launched");
    assert!(
        command.starts_with("<PROGRAM_DIR>"),
        "the program's directory must redact to its own root, got {command}"
    );
    assert!(
        command.ends_with("node.exe"),
        "the program's own name still has to survive, or the manifest stops naming what ran: \
         {command}"
    );
}

/// The other half: a program that already lives under a declared root keeps
/// that root's spelling. Every promoted capture reads `<HOME>\...`, and a new
/// root that outranked `<HOME>` because it is a longer string would silently
/// rewrite all of them on the next re-sanitize.
#[test]
fn a_program_under_an_existing_root_keeps_that_roots_spelling() {
    let temp = tempfile::tempdir().unwrap();
    let raw = write_raw_capture(
        temp.path(),
        "home-interpreter",
        &[r#"{"type":"control_response","response":{"subtype":"success"}}"#],
    );
    let capture_path = raw.join("capture.json");
    let mut capture: Value =
        serde_json::from_slice(&std::fs::read(&capture_path).unwrap()).unwrap();
    capture["redaction_roots"]["home"] = Value::String(r"C:\Users\somebody".into());
    capture["command"]["program"] = Value::String(r"C:\Users\somebody\.grok\bin\grok.exe".into());
    std::fs::write(&capture_path, serde_json::to_vec_pretty(&capture).unwrap()).unwrap();

    let report = sanitize_dir(&raw, &staging_dir(temp.path(), "home-interpreter")).unwrap();
    let manifest: Value = serde_json::from_slice(&report.manifest_bytes).unwrap();
    assert_eq!(
        manifest["command"]["program"],
        Value::String(r"<HOME>\.grok\bin\grok.exe".into()),
        "a program under an existing root must keep that root, not gain a new one"
    );
}

/// D91's residual: the manifest must say plainly whether a Claude capture ran against an
/// isolated `CLAUDE_CONFIG_DIR` or the capturer's ambient one, rather than leaving a reader to
/// infer it from whether `command.configured_env` happens to carry that key.
///
/// `write_raw_capture`'s fixture is Claude with an empty `configured_env` -- ambient by
/// construction, since nothing recorded `--claude-config-dir` for it.
#[test]
fn sanitizer_manifest_records_ambient_claude_config_isolation() {
    let temp = tempfile::tempdir().unwrap();
    let raw = write_raw_capture(
        temp.path(),
        "ambient-claude-config",
        &[r#"{"type":"control_response","response":{"subtype":"success"}}"#],
    );

    let report = sanitize_dir(&raw, &staging_dir(temp.path(), "ambient-claude-config")).unwrap();
    let manifest: Value = serde_json::from_slice(&report.manifest_bytes).unwrap();

    assert_eq!(manifest["claude_config_isolation"], "ambient");
}

/// The other half: a Claude capture whose launch carried `CLAUDE_CONFIG_DIR` must be marked
/// `"isolated"`, distinguishably from the ambient case above.
#[test]
fn sanitizer_manifest_records_isolated_claude_config_isolation() {
    let temp = tempfile::tempdir().unwrap();
    let raw = write_raw_capture(
        temp.path(),
        "isolated-claude-config",
        &[r#"{"type":"control_response","response":{"subtype":"success"}}"#],
    );
    let capture_path = raw.join("capture.json");
    let mut capture: Value =
        serde_json::from_slice(&std::fs::read(&capture_path).unwrap()).unwrap();
    // Mirrors production (`record/session.rs`'s `capture_redaction_roots`, which derives
    // `redaction_roots.claude_config_dir` from this same `configured_env` entry): the fail-closed
    // absolute-path scan only recognizes a value against a declared redaction root, so the two
    // must agree here exactly as they always do coming out of a real capture.
    capture["command"]["configured_env"]["CLAUDE_CONFIG_DIR"] =
        Value::String(r"C:\captured-claude-config".into());
    capture["redaction_roots"]["claude_config_dir"] =
        Value::String(r"C:\captured-claude-config".into());
    std::fs::write(&capture_path, serde_json::to_vec_pretty(&capture).unwrap()).unwrap();

    let report = sanitize_dir(&raw, &staging_dir(temp.path(), "isolated-claude-config")).unwrap();
    let manifest: Value = serde_json::from_slice(&report.manifest_bytes).unwrap();

    assert_eq!(manifest["claude_config_isolation"], "isolated");
}

/// The field is Claude-only: the concept has no meaning for a Codex or ACP capture, and a
/// spurious `"ambient"` there would read as a claim about a scenario that was never in question.
#[test]
fn sanitizer_manifest_omits_claude_config_isolation_for_a_non_claude_capture() {
    let temp = tempfile::tempdir().unwrap();
    let raw = write_raw_capture(
        temp.path(),
        "codex-capture",
        &[r#"{"type":"control_response","response":{"subtype":"success"}}"#],
    );
    let capture_path = raw.join("capture.json");
    let mut capture: Value =
        serde_json::from_slice(&std::fs::read(&capture_path).unwrap()).unwrap();
    capture["provider"] = Value::String("codex".into());
    capture["command"]["configured_env"]["CODEX_HOME"] =
        Value::String(r"C:\captured-codex-home".into());
    capture["redaction_roots"]["codex_home"] = Value::String(r"C:\captured-codex-home".into());
    std::fs::write(&capture_path, serde_json::to_vec_pretty(&capture).unwrap()).unwrap();

    let report = sanitize_dir(&raw, &staging_dir(temp.path(), "codex-capture")).unwrap();
    let manifest: Value = serde_json::from_slice(&report.manifest_bytes).unwrap();

    assert!(
        manifest.get("claude_config_isolation").is_none(),
        "a non-Claude manifest must not carry a Claude-only isolation claim: {manifest}"
    );
}
