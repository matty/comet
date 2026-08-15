//! Fail-closed guarantees the allowlist does not itself provide.
//!
//! `allowlist_property.rs`'s total property (every committed scalar is
//! either allowlisted or a placeholder) subsumed most of what used to live
//! here: an absolute path or a secret-looking string on a path nobody
//! allowlisted is now redacted unconditionally, by construction, so a test
//! that fed one through a single unlisted field and asserted rejection was
//! testing blocklist-era machinery that no longer runs. What is left is
//! everything the allowlist genuinely does not reach: staging/output-path
//! safety (independent of any capture content), and the two checks that run
//! *regardless* of whether a path is on the list -- a credential-shaped
//! field name, and a credential-shaped value riding a path the list would
//! otherwise keep verbatim.

use super::support::*;

use comet_harness::capture::{Provider, SanitizationError, allows, sanitize_dir};
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

/// Break caught: validating only JSON values lets sensitive object keys through, while building
/// an error location from an untrusted key repeats the secret in diagnostics.
///
/// Not subsumed by the allowlist: `validate_key` runs on every object key
/// during the walk, independent of whether the key's own *value* ends up on
/// an allowed path or not -- a key can be rejected even where the value
/// beside it would have passed.
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
///
/// Same mechanism as above, at the case where the key itself collides with a
/// known local root: a key can never be silently rewritten (only kept or
/// rejected), so a key that only *partially* matches a redaction root must
/// reject rather than publish the fragment left over from the record.
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

/// An allowlisted path is a decision about the *field*, not a licence for
/// whatever a provider happens to put in it: `sanitize_scalar` runs every
/// kept string through the same fail-closed secret scan a redacted value
/// never needs, specifically because being on the list means the value
/// survives verbatim and therefore must not be allowed to smuggle a
/// credential through. Mirrors `sanitize.rs`'s own inline unit test
/// `a_credential_in_an_allowlisted_value_still_rejects`, through the full
/// `sanitize_dir` file pipeline this file's other tests exercise.
#[test]
fn sanitizer_rejects_a_credential_riding_an_allowlisted_path() {
    for (name, provider, allowed_path, payload) in [
        (
            "claude-type",
            Provider::Claude,
            ".type",
            r#"{"type":"sk-ant-api03-should-not-survive-allowlisting"}"#,
        ),
        (
            "codex-method",
            Provider::Codex,
            ".method",
            r#"{"method":"sk-proj-should-not-survive-allowlisting"}"#,
        ),
    ] {
        assert!(
            allows(provider, allowed_path),
            "{name}: {allowed_path} must actually be on the allowlist for this test to mean anything"
        );

        let temp = tempfile::tempdir().unwrap();
        let raw = write_raw_capture(temp.path(), name, &[payload]);
        let path = raw.join("capture.json");
        let mut capture: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        capture["provider"] = Value::String(match provider {
            Provider::Claude => "claude".into(),
            Provider::Codex => "codex".into(),
        });
        std::fs::write(&path, serde_json::to_vec_pretty(&capture).unwrap()).unwrap();

        let error = sanitize_dir(&raw, &staging_dir(temp.path(), name)).unwrap_err();
        assert!(
            matches!(error, SanitizationError::SecretLikeValue { .. }),
            "{name} returned {error:?}"
        );
        assert!(!error.to_string().contains("should-not-survive"), "{name}");
    }
}

/// Field-name credential detection (`is_secret_field`) runs before the path
/// allowlist is even consulted -- a credential-shaped field name is rejected
/// wherever it appears, allowlisted path or not, so this is not a check the
/// allowlist subsumes. The numeric-counter carve-out is the one deliberate
/// exception in that same check (a `token`-suffixed field holding a *number*
/// is a usage count, not a credential); it is pinned here too since nothing
/// else in this file exercises it after this trim.
#[test]
fn sanitizer_rejects_secret_field_names_regardless_of_path_and_still_permits_numeric_counters() {
    for field in [
        "authorization",
        "apiKey",
        "refreshToken",
        "proxyAuthorization",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let payload =
            serde_json::json!({field: "opaque-value-not-a-recognizable-prefix"}).to_string();
        let raw = write_raw_capture(temp.path(), field, &[&payload]);
        let error = sanitize_dir(&raw, &staging_dir(temp.path(), field)).unwrap_err();
        assert!(
            matches!(error, SanitizationError::SecretLikeField { .. }),
            "{field} returned {error:?}"
        );
        assert!(!error.to_string().contains("opaque-value"), "{field}");
    }

    let temp = tempfile::tempdir().unwrap();
    let raw = write_raw_capture(
        temp.path(),
        "token-counters",
        &[r#"{"usage":{"input_tokens":10,"outputTokens":20,"totalTokenCount":60}}"#],
    );
    // A numeric counter under a token-shaped name is not a credential -- the
    // capture must sanitize cleanly rather than reject (what its redacted
    // value becomes is the allowlist's concern, covered by
    // `allowlist_property.rs`, not this file's).
    sanitize_dir(&raw, &staging_dir(temp.path(), "token-counters")).unwrap();

    let temp = tempfile::tempdir().unwrap();
    let raw = write_raw_capture(
        temp.path(),
        "string-token-counter",
        &[r#"{"usage":{"input_tokens":"opaque-value"}}"#],
    );
    // The same field name holding a *string* is not a counter -- the
    // exception must not cover an opaque string riding a token-shaped name.
    assert!(matches!(
        sanitize_dir(&raw, &staging_dir(temp.path(), "string-token-counter")),
        Err(SanitizationError::SecretLikeField { .. })
    ));
}
