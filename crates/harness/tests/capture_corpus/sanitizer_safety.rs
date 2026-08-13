use super::support::*;

use std::path::PathBuf;

use comet_harness::capture::{SanitizationError, sanitize_dir};
use serde_json::Value;

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

/// Break caught: Codex's documented token-usage notification wraps numeric counters in a
/// `tokenUsage` object, which an otherwise-correct credential family check mistakes for a token.
#[test]
fn sanitizer_accepts_only_the_codex_token_usage_notification_object() {
    let temp = tempfile::tempdir().unwrap();
    let payload = r#"{"method":"thread/tokenUsage/updated","params":{"threadId":"thread-secret","turnId":"turn-secret","tokenUsage":{"total":{"inputTokens":20,"outputTokens":3},"last":{"inputTokens":10,"cachedInputTokens":4,"outputTokens":3,"reasoningOutputTokens":0,"totalTokens":13},"modelContextWindow":200000}}}"#;
    let raw = write_raw_capture(temp.path(), "codex-token-usage", &[payload]);
    let path = raw.join("capture.json");
    let mut capture: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    capture["provider"] = Value::String("codex".into());
    std::fs::write(&path, serde_json::to_vec_pretty(&capture).unwrap()).unwrap();
    let report = sanitize_dir(&raw, &staging_dir(temp.path(), "codex-token-usage")).unwrap();
    let got = sanitized_payloads(&report.events_bytes);
    assert_eq!(got[0]["params"]["threadId"], "<THREAD_ID_1>");
    assert_eq!(got[0]["params"]["turnId"], "<TURN_ID_1>");
    assert_eq!(got[0]["params"]["tokenUsage"]["last"]["inputTokens"], 10);

    for (name, provider, method) in [
        (
            "claude-token-usage-object",
            "claude",
            "thread/tokenUsage/updated",
        ),
        ("codex-wrong-method-token-usage", "codex", "thread/other"),
    ] {
        let raw = write_raw_capture(temp.path(), name, &[payload]);
        let path = raw.join("capture.json");
        let mut capture: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        capture["provider"] = Value::String(provider.into());
        let mut value: Value = capture["events"][0]["payload"]
            .as_str()
            .and_then(|payload| serde_json::from_str(payload).ok())
            .unwrap();
        value["method"] = Value::String(method.into());
        capture["events"][0]["payload"] = Value::String(value.to_string());
        std::fs::write(&path, serde_json::to_vec_pretty(&capture).unwrap()).unwrap();
        assert!(matches!(
            sanitize_dir(&raw, &staging_dir(temp.path(), name)),
            Err(SanitizationError::SecretLikeField { .. })
        ));
    }

    for (name, payload) in [
        (
            "codex-root-token-usage",
            r#"{"method":"thread/tokenUsage/updated","tokenUsage":{"total":{"inputTokens":20}},"params":{"threadId":"thread-secret","turnId":"turn-secret"}}"#,
        ),
        (
            "codex-nested-token-usage",
            r#"{"method":"thread/tokenUsage/updated","params":{"threadId":"thread-secret","turnId":"turn-secret","wrapper":{"tokenUsage":{"total":{"inputTokens":20}}}}}"#,
        ),
        (
            "codex-scalar-token-usage",
            r#"{"method":"thread/tokenUsage/updated","params":{"threadId":"thread-secret","turnId":"turn-secret","tokenUsage":"opaque-secret"}}"#,
        ),
        (
            "codex-token-usage-nested-credential",
            r#"{"method":"thread/tokenUsage/updated","params":{"threadId":"thread-secret","turnId":"turn-secret","tokenUsage":{"total":{"inputTokens":20,"accessToken":"opaque-secret"}}}}"#,
        ),
        (
            "codex-token-usage-nonnumeric-counter",
            r#"{"method":"thread/tokenUsage/updated","params":{"threadId":"thread-secret","turnId":"turn-secret","tokenUsage":{"total":{"inputTokens":"opaque-secret"}}}}"#,
        ),
    ] {
        let raw = write_raw_capture(temp.path(), name, &[payload]);
        let path = raw.join("capture.json");
        let mut capture: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        capture["provider"] = Value::String("codex".into());
        std::fs::write(&path, serde_json::to_vec_pretty(&capture).unwrap()).unwrap();
        assert!(matches!(
            sanitize_dir(&raw, &staging_dir(temp.path(), name)),
            Err(SanitizationError::SecretLikeField { .. })
        ));
    }
}
