//! Fail-closed guarantees the allowlist does not itself provide.
//!
//! `allowlist_property.rs`'s total property (every committed scalar is
//! either allowlisted or a placeholder) subsumed part of what used to live
//! here: a test that fed an absolute path or a secret-looking string through
//! a *single unlisted field* and asserted rejection was testing blocklist-era
//! machinery that no longer runs -- an unlisted path is now redacted
//! unconditionally, by construction, regardless of content.
//!
//! What is left, and what this file now covers, is everything the allowlist
//! genuinely does not reach:
//!
//! - Staging/output-path safety, independent of any capture content.
//! - Two checks that run *regardless* of whether a path is on the list: a
//!   credential-shaped field name, and a sensitive object key.
//! - The fail-closed scans (`sanitize_paths_and_validate` and everything it
//!   calls -- `contains_secret_value`, `contains_absolute_path`,
//!   `path_occurrence_escapes_root`, `replace_path_occurrences`'s boundary
//!   check) that still run on every value the allowlist keeps verbatim: an
//!   allowlisted path is a decision about the *field*, not a licence for
//!   whatever a provider puts in it, so these need coverage that lands on an
//!   allowed path, not an unlisted one -- a value on an unlisted path never
//!   reaches these scans at all anymore.
//! - The same scans, run unconditionally on the six manifest metadata fields
//!   (`command`, `cli_version`, `normalized_cli_version`, `scenario`,
//!   `purpose`, `platform`) that never pass through the path allowlist in the
//!   first place.

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
        (
            "claude-private-key",
            Provider::Claude,
            ".type",
            r#"{"type":"-----BEGIN OPENSSH PRIVATE KEY-----"}"#,
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

/// Break caught: validating only event payloads lets a secret-like provider version, scenario,
/// purpose, platform, or command value leak through the deterministic manifest.
///
/// Not subsumed by the allowlist, and not subsumed by
/// `sanitizer_rejects_a_credential_riding_an_allowlisted_path` above: these
/// six manifest fields (`command`'s argv and program path, `cli_version`,
/// `normalized_cli_version`, `scenario`, `purpose`, `platform`) never pass
/// through the path allowlist at all -- `sanitize_dir` scans each of them
/// directly, unconditionally, and `allowlist_property.rs` only ever reads a
/// manifest's `provider` and `placeholders` fields, never these. The manifest
/// is committed evidence in a public repo, so each of these six needs its own
/// proof the fail-closed scan still runs on it.
type ManifestMutator = fn(&mut Value);

#[test]
fn sanitizer_scans_every_manifest_string_before_writing_staging() {
    let secret = "sk-proj-manifest-metadata-secret";
    let mutations: [(&str, ManifestMutator); 6] = [
        ("cli_version", |capture| {
            capture["cli_version"] = Value::String("sk-proj-manifest-metadata-secret".into());
        }),
        ("scenario", |capture| {
            capture["scenario"] = Value::String("sk-proj-manifest-metadata-secret".into());
        }),
        ("purpose", |capture| {
            capture["purpose"] = Value::String("sk-proj-manifest-metadata-secret".into());
        }),
        ("platform", |capture| {
            capture["platform"]["os"] = Value::String("sk-proj-manifest-metadata-secret".into());
        }),
        ("command_program", |capture| {
            capture["command"]["program"] =
                Value::String("sk-proj-manifest-metadata-secret".into());
        }),
        ("command_args", |capture| {
            capture["command"]["args"] = serde_json::json!(["sk-proj-manifest-metadata-secret"]);
        }),
    ];

    for (name, mutate) in mutations {
        let temp = tempfile::tempdir().unwrap();
        let raw = write_raw_capture(temp.path(), name, &[r#"{"level":"debug"}"#]);
        let path = raw.join("capture.json");
        let mut capture: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        mutate(&mut capture);
        std::fs::write(&path, serde_json::to_vec_pretty(&capture).unwrap()).unwrap();
        let output = staging_dir(temp.path(), name);

        let error = sanitize_dir(&raw, &output).unwrap_err();
        assert!(
            matches!(error, SanitizationError::SecretLikeValue { .. }),
            "{name} returned {error:?}"
        );
        assert!(!error.to_string().contains(secret), "{name}");
        assert!(!output.exists(), "{name}");
    }
}

/// Break caught: substring replacement can treat an allowlisted root as a prefix of a different
/// absolute path, hide its drive/root marker, and let the unknown path escape rejection.
///
/// Runs on `.type`, an allowlisted field, not an unlisted one -- the earlier
/// version of this test used an unlisted `.path` field, which the allowlist
/// itself now redacts wholesale regardless of content, so it no longer
/// exercises `replace_path_occurrences`'s boundary check at all (the code
/// this test exists to pin never runs on a value that gets replaced
/// unconditionally). Landing the same payload on an allowed field is what
/// makes this test still mean something.
#[test]
fn sanitizer_does_not_allow_path_prefix_collisions_on_an_allowlisted_path() {
    assert!(allows(Provider::Claude, ".type"));

    let temp = tempfile::tempdir().unwrap();
    let cwd = std::path::PathBuf::from(r"D:\allowed\repo");
    let raw = write_raw_capture(
        temp.path(),
        "path-prefix-collision",
        &[&format!(
            r#"{{"type":{}}}"#,
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

/// `sanitize_claude_resume_argv` (`crates/harness/src/capture/sanitize.rs:822`)
/// runs only for Claude captures, and only rewrites `--resume=<id>` when the
/// id matches the sole `SESSION`-named placeholder events sanitizing already
/// discovered. Break caught: without this, the resume identifier repeated in
/// the launch argv (outside event JSON, so the ordinary path/value scan never
/// sees it) would ride into the manifest verbatim instead of reusing the
/// exact typed placeholder assigned to the same id inside the events.
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
        serde_json::json!(["--print", "--resume=<SESSION_1>"])
    );
    assert_eq!(manifest["redaction_counts"]["session"], 2);
    assert!(
        !String::from_utf8(report.manifest_bytes)
            .unwrap()
            .contains(session_id)
    );
}

/// Break caught: recognizing only the happy-path prefix lets malformed, split, duplicate, absent,
/// or scenario-inappropriate resume arguments bypass the manifest's semantic-ID contract --
/// `sanitize_claude_resume_argv` has seven separate fail-closed branches, and this pins six of
/// them (a `resume`-scenario command whose argv doesn't hold exactly one valid `--resume=<id>`
/// matching the sole `SESSION` semantic, or a non-`resume` scenario carrying `--resume` at all).
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

/// **Reverses an earlier ruling, deliberately.** This test previously asserted
/// that a capture carrying two distinct session ids must *reject*, on the
/// stated grounds that the sanitizer would otherwise "guess which one the
/// resume argument refers to". There is no guess to make: the argv names one
/// id literally, and `sanitize_claude_resume_argv` now looks that value up
/// among the placeholders the events were already assigned.
///
/// The old rule rejected the exact shape the `resume` scenario exists to
/// record — a resumed run whose frames still carry the ancestor session
/// alongside the current one — with an error naming invalid resume arguments,
/// which points a reader at the argv rather than at the second id. It survived
/// only because Claude reuses one session id across a resume today, so the
/// committed corpus never produced a second. A capture that spawns a subagent
/// would.
///
/// What the reversal must not cost is the safety property underneath, and does
/// not: the raw id still never survives, and an argv naming an id the events
/// never carried still rejects (below).
#[test]
fn a_resume_argv_maps_its_own_id_when_the_capture_holds_two_sessions() {
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

    let report = sanitize_dir(&raw, &output).expect("two session ids must not reject the capture");

    let manifest: Value = serde_json::from_slice(&report.manifest_bytes).unwrap();
    let argv = manifest["command"]["args"][0].as_str().unwrap();
    assert!(
        !argv.contains("first-session") && !argv.contains("second-session"),
        "no raw session id may survive in argv: {argv}"
    );
    // The argv must carry the placeholder its *own* id was given, which is the
    // one the first frame's `session_id` was redacted to -- not the second's.
    let events = std::str::from_utf8(&report.events_bytes).unwrap();
    let first_frame: Value = serde_json::from_str(events.lines().next().unwrap()).unwrap();
    let first_payload: Value =
        serde_json::from_str(first_frame["payload"].as_str().unwrap()).unwrap();
    let first_placeholder = first_payload["session_id"].as_str().unwrap();
    assert_eq!(argv, format!("--resume={first_placeholder}"));
}

/// The safety half of the reversal above: an argv naming an id no frame ever
/// carried means the command is not what the capture claims, and that still
/// stops promotion rather than being sanitized away.
#[test]
fn sanitizer_rejects_a_resume_argv_naming_a_session_id_the_events_never_carried() {
    let temp = tempfile::tempdir().unwrap();
    let raw = write_raw_capture(
        temp.path(),
        "unseen-session-semantics",
        &[r#"{"type":"system","session_id":"observed-session"}"#],
    );
    let capture_path = raw.join("capture.json");
    let mut capture: Value =
        serde_json::from_slice(&std::fs::read(&capture_path).unwrap()).unwrap();
    capture["scenario"] = Value::String("resume".into());
    capture["command"]["args"] = serde_json::json!(["--resume=never-observed-session"]);
    std::fs::write(&capture_path, serde_json::to_vec_pretty(&capture).unwrap()).unwrap();
    let output = staging_dir(temp.path(), "unseen-session-semantics");

    let error = sanitize_dir(&raw, &output).unwrap_err();
    assert!(matches!(
        error,
        SanitizationError::InvalidClaudeResumeCommand { .. }
    ));
    assert!(!error.to_string().contains("never-observed-session"));
    assert!(!error.to_string().contains("observed-session"));
    assert!(!output.exists());
}

/// Break caught: textual prefix replacement can bless an allowed root followed by `..`, and a
/// detector that recognizes only backslash UNC paths misses the equivalent forward-slash form.
///
/// Same adaptation as the prefix-collision test above: lands on `.type`
/// (allowlisted) rather than the unlisted `.path` the earlier version used,
/// so `path_occurrence_escapes_root`'s traversal check actually runs.
#[test]
fn sanitizer_rejects_allowlist_traversal_on_an_allowlisted_path() {
    assert!(allows(Provider::Claude, ".type"));

    let temp = tempfile::tempdir().unwrap();
    let raw = write_raw_capture(
        temp.path(),
        "allowlist-traversal",
        &[&format!(
            r#"{{"type":{}}}"#,
            serde_json::to_string(r"D:\allowed\repo\..\private\secret.txt").unwrap()
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

    let error = sanitize_dir(&raw, &staging_dir(temp.path(), "allowlist-traversal")).unwrap_err();
    assert!(matches!(
        error,
        SanitizationError::UnrecognizedAbsolutePath { .. }
    ));
}

/// Break caught: weakening the post-redaction scan permits an absolute machine path not covered
/// by the explicit HOME/REPO/CWD/TEMP allowlist to enter staging, in any of the forms
/// `contains_absolute_path` recognizes beyond a plain drive letter -- unix, backslash UNC,
/// forward-slash UNC, and the `\\?\` verbatim-Windows prefix.
///
/// Runs on `.type` (allowlisted), for the same reason the two tests above do:
/// an unlisted field is now wholesale-redacted regardless of content, so it
/// no longer exercises this scan.
#[test]
fn sanitizer_rejects_unrecognized_absolute_path_forms_on_an_allowlisted_path() {
    assert!(allows(Provider::Claude, ".type"));

    let cases = [
        ("unix", r#"{"type":"/srv/private/secret.txt"}"#),
        ("unc", r#"{"type":"\\\\server\\share\\secret.txt"}"#),
        (
            "forward-unc",
            r#"{"type":"//server/share/private/secret.txt"}"#,
        ),
        (
            "verbatim-windows",
            r#"{"type":"\\\\?\\D:\\private\\secret.txt"}"#,
        ),
    ];

    for (name, payload) in cases {
        let temp = tempfile::tempdir().unwrap();
        let raw = write_raw_capture(temp.path(), name, &[payload]);
        let error = sanitize_dir(&raw, &staging_dir(temp.path(), name)).unwrap_err();
        assert!(
            matches!(error, SanitizationError::UnrecognizedAbsolutePath { .. }),
            "{name} returned {error:?}"
        );
    }
}

/// A genuinely non-JSON stderr line is the one payload shape the ordinary
/// path/value walk never reaches: `sanitize_value_tree` only runs on parsed
/// JSON, so unparseable stderr text takes the separate
/// `placeholder_for_prose` branch (`sanitize.rs:172-199`, `721-723`)
/// instead. That branch has to redact deliberately, not by falling through
/// the same machinery -- nothing else in this file writes a last event that
/// actually fails `serde_json::from_str`, so nothing previously proved raw
/// provider stderr text (which can carry a stack trace, an environment
/// dump, or a path) doesn't survive into the committed archive verbatim.
#[test]
fn sanitizer_redacts_genuinely_non_json_stderr_text() {
    let temp = tempfile::tempdir().unwrap();
    let secret_looking_text = "panic at worker.rs:42: D:\\Users\\someone\\.ssh\\id_rsa not found";
    let raw = write_raw_capture(
        temp.path(),
        "non-json-stderr",
        &[r#"{"level":"debug"}"#, secret_looking_text],
    );

    let report = sanitize_dir(&raw, &staging_dir(temp.path(), "non-json-stderr")).unwrap();
    let events = std::str::from_utf8(&report.events_bytes).unwrap();
    let last_line: Value = serde_json::from_str(events.lines().next_back().unwrap()).unwrap();

    let payload = last_line["payload"].as_str().unwrap();
    assert_ne!(payload, secret_looking_text);
    assert!(
        payload.starts_with("<PROSE_") && payload.ends_with('>'),
        "non-JSON stderr must collapse to a PROSE placeholder, got {payload:?}"
    );
    assert!(!events.contains("id_rsa"));
    assert!(!events.contains("someone"));
}
