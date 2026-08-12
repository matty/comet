use std::path::{Path, PathBuf};

use comet_harness::capture::{SanitizationError, sanitize_dir};
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
    assert_eq!(payloads[4]["message"], "safe diagnostic for <SESSION_ID_1>");

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
    assert_eq!(manifest["provider"], "claude");
    assert_eq!(manifest["scenario"], "accounting");
    assert_eq!(manifest["redaction_counts"]["session_id"], 2);
    assert_eq!(manifest["redaction_counts"]["user_text"], 2);
    assert_eq!(
        manifest["placeholders"],
        serde_json::json!([
            {"placeholder": "<SESSION_ID_1>", "kind": "session_id"},
            {"placeholder": "<USER_TEXT_1>", "kind": "user_text"},
            {"placeholder": "<USER_TEXT_2>", "kind": "user_text"}
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
