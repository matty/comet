//! Every scalar in every committed frame is either at an allowlisted path or
//! is a placeholder.
//!
//! This replaces thirty-one example-based tests (`sanitizer_semantics.rs`,
//! deleted by this same change) that each asserted one particular shape gets
//! redacted -- "does a Codex reasoning summary get typed as prose," "does a
//! thinking signature get typed by shape." Under an allowlist those questions
//! stop existing: unlisted means replaced, no recognition involved. What
//! replaces them is a **total property** over the whole archive, not a
//! sample of it -- it asserts nothing anywhere escaped, over every committed
//! byte, and it keeps holding for captures that do not exist yet.
//!
//! Deliberately red at the commit that adds it. The committed corpus was
//! sanitized by the blocklist this stage replaces, and the blocklist let
//! through exactly the shapes an allowlist is built to catch by construction
//! (the recording user's OS build, installed plugins, subagent roster, MCP
//! connector identity). Task 5 re-sanitizes the archive against the new
//! allowlist and turns this green; until then, the failure below is the
//! proof the property does what it says.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use comet_harness::capture::{Provider, allows};
use serde_json::Value;

/// One committed scalar that is neither on its provider's allowlist nor
/// shaped like a placeholder -- named by where it lives, never by what it
/// says. Carries no value: echoing the value here would make this failure
/// message a second copy of exactly what the allowlist exists to withhold,
/// and this message lands in terminals and PR bodies.
struct Escape {
    scenario: String,
    sequence: u64,
    path: String,
}

impl std::fmt::Display for Escape {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} frame {}: {}",
            self.scenario, self.sequence, self.path
        )
    }
}

#[test]
fn every_committed_value_is_allowlisted_or_a_placeholder() {
    let corpus_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");
    let mut escapes = Vec::new();
    let mut scenario_count = 0u64;

    for provider_dir in subdirectories(&corpus_root) {
        for version_dir in subdirectories(&provider_dir) {
            for scenario_dir in subdirectories(&version_dir) {
                let events_path = scenario_dir.join("events.jsonl");
                if !events_path.is_file() {
                    continue;
                }
                scenario_count += 1;
                let scenario = scenario_dir
                    .strip_prefix(&corpus_root)
                    .unwrap_or(scenario_dir.as_path())
                    .to_string_lossy()
                    .replace('\\', "/");
                let manifest_path = scenario_dir.join("manifest.json");
                let (provider, placeholders) =
                    manifest_provider_and_placeholders(&manifest_path, &scenario);
                check_scenario(
                    &events_path,
                    &scenario,
                    provider,
                    &placeholders,
                    &mut escapes,
                );
            }
        }
    }

    // Mirrors the same guard `allowlist.rs`'s own corpus walk uses: a broken
    // walk that silently visits nothing must fail loudly, not read as "the
    // property holds" by finding zero frames to check.
    assert!(
        scenario_count > 0,
        "found no events.jsonl under {} -- corpus walk is broken, not just empty",
        corpus_root.display()
    );

    if escapes.is_empty() {
        return;
    }
    let mut report = vec![format!(
        "{} scalar(s) are present verbatim in committed evidence, neither on the \
         allowlist for their provider nor replaced by a placeholder (values withheld \
         deliberately -- open the named frame locally to inspect one, and weigh it \
         against `crates/harness/src/capture/allowlist/claude.txt` or `codex.txt` \
         before deciding whether the path belongs there):",
        escapes.len()
    )];
    for escape in &escapes {
        report.push(format!("  {escape}"));
    }
    panic!("{}", report.join("\n"));
}

/// One scenario's worth of frames, checked against its own provider and its
/// own manifest's placeholder vocabulary. Failures are pushed onto `escapes`
/// rather than panicking here, so one bad scenario does not hide every other.
fn check_scenario(
    events_path: &Path,
    scenario: &str,
    provider: Provider,
    placeholders: &BTreeSet<String>,
    escapes: &mut Vec<Escape>,
) {
    let text = std::fs::read_to_string(events_path)
        .unwrap_or_else(|error| panic!("{scenario}: events.jsonl unreadable: {error}"));
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let event: Value = serde_json::from_str(line)
            .unwrap_or_else(|error| panic!("{scenario}: invalid event line: {error}"));
        let sequence = event["sequence"]
            .as_u64()
            .unwrap_or_else(|| panic!("{scenario}: an event line has no sequence: {line}"));
        let Some(payload) = event["payload"].as_str() else {
            panic!("{scenario} frame {sequence}: event has no payload string");
        };

        match serde_json::from_str::<Value>(payload) {
            Ok(value) => {
                let mut scalars = Vec::new();
                collect_scalars(&value, "", &mut scalars);
                for (path, scalar) in scalars {
                    if allows(provider, &path) {
                        continue;
                    }
                    let is_placeholder =
                        matches!(&scalar, Value::String(text) if placeholders.contains(text));
                    if !is_placeholder {
                        escapes.push(Escape {
                            scenario: scenario.to_owned(),
                            sequence,
                            path,
                        });
                    }
                }
            }
            // Only stderr may carry non-JSON prose (`sanitize_dir` rejects an
            // unparseable stdout/stdin frame outright, so a promoted capture
            // never has one). Prose has no dotted path of its own -- the
            // whole payload is always fully replaced, never kept -- so the
            // one thing to check is that the whole string is itself a known
            // placeholder.
            Err(_) => {
                if !placeholders.contains(payload) {
                    escapes.push(Escape {
                        scenario: scenario.to_owned(),
                        sequence,
                        path: "(unparsed stderr text, not a placeholder)".to_owned(),
                    });
                }
            }
        }
    }
}

/// Every dotted scalar path in `value`, computed exactly the way
/// `Redactor::sanitize_value_tree` computes it in
/// `crates/harness/src/capture/sanitize.rs`: an array element's path grows
/// `[]`, never an index (so every element of an array shares one allowlist
/// decision, matching that function's own comment), an object child's path
/// grows `.key`, and the walk starts at `path = ""`, the same root
/// `sanitize_dir` passes for `event.payload`. Only `String`/`Number` leaves
/// are scalars; `Bool`/`Null` carry no free-form data and are never redacted
/// either, so they are correctly excluded here too.
///
/// Equivalence with the real function is not just asserted by this comment --
/// see `collect_scalars_paths_agree_with_the_real_sanitizer` below, which
/// runs a value through the actual `sanitize_dir` and cross-checks this
/// function's paths against its real allow/placeholder decisions.
fn collect_scalars(value: &Value, path: &str, out: &mut Vec<(String, Value)>) {
    match value {
        Value::Array(items) => {
            let child_path = format!("{path}[]");
            for item in items {
                collect_scalars(item, &child_path, out);
            }
        }
        Value::Object(object) => {
            for (key, child) in object {
                let child_path = format!("{path}.{key}");
                collect_scalars(child, &child_path, out);
            }
        }
        Value::String(_) | Value::Number(_) => out.push((path.to_owned(), value.clone())),
        _ => {}
    }
}

/// The provider and the full set of placeholder strings a scenario's own
/// manifest recorded as actually used (`manifest["placeholders"][].placeholder`).
///
/// Reading this per scenario, rather than hardcoding the current placeholder
/// vocabulary, is deliberate: the committed corpus was sanitized by the
/// blocklist this stage replaces, so its manifests carry the *old* typed
/// names (`<SESSION_ID_1>`, `<CLAUDE_THINKING_SIGNATURE_1>`, ...), not the
/// new six-kind vocabulary `sanitize.rs` writes today. Reading each
/// manifest's own list is what lets this property hold across a
/// sanitizer-vocabulary change without editing the test -- exactly what Task
/// 5 needs it to do once it regenerates the corpus.
fn manifest_provider_and_placeholders(
    manifest_path: &Path,
    scenario: &str,
) -> (Provider, BTreeSet<String>) {
    let text = std::fs::read_to_string(manifest_path)
        .unwrap_or_else(|error| panic!("{scenario}: manifest.json unreadable: {error}"));
    let manifest: Value = serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("{scenario}: manifest.json is not valid JSON: {error}"));
    let provider = match manifest["provider"].as_str() {
        Some("claude") => Provider::Claude,
        Some("codex") => Provider::Codex,
        other => panic!("{scenario}: manifest has an unknown provider: {other:?}"),
    };
    let placeholders = manifest["placeholders"]
        .as_array()
        .unwrap_or_else(|| panic!("{scenario}: manifest has no placeholders array"))
        .iter()
        .filter_map(|entry| entry["placeholder"].as_str())
        .map(str::to_owned)
        .collect();
    (provider, placeholders)
}

fn subdirectories(parent: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(parent)
        .unwrap_or_else(|error| panic!("{}: {error}", parent.display()))
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect()
}

#[cfg(test)]
mod self_check {
    //! Self-review evidence, not corpus evidence: proves `collect_scalars`'s
    //! path strings agree with what the real `sanitize_dir` actually decides,
    //! rather than resting on "the code above looks like the code in
    //! `sanitize.rs`." Runs a synthetic capture through the production
    //! sanitizer and cross-checks every scalar's path against its real
    //! allow/placeholder outcome.

    use comet_harness::capture::{Provider, allows, sanitize_dir};
    use serde_json::Value;

    use super::super::support::{staging_dir, write_raw_capture};
    use super::collect_scalars;

    #[test]
    fn collect_scalars_paths_agree_with_the_real_sanitizer() {
        let temp = tempfile::tempdir().unwrap();
        let raw = write_raw_capture(
            temp.path(),
            "path-equivalence",
            &[
                r#"{"type":"user","mystery":"unlisted-value","nested":{"list":[{"unknown":"a"},{"unknown":"b"}]},"mcp_servers":[{"status":"connected"}]}"#,
            ],
        );
        let report = sanitize_dir(&raw, &staging_dir(temp.path(), "path-equivalence")).unwrap();
        let line = std::str::from_utf8(&report.events_bytes)
            .unwrap()
            .lines()
            .next()
            .unwrap();
        let event: Value = serde_json::from_str(line).unwrap();
        let payload: Value = serde_json::from_str(event["payload"].as_str().unwrap()).unwrap();

        let mut scalars = Vec::new();
        collect_scalars(&payload, "", &mut scalars);
        let by_path: std::collections::BTreeMap<&str, &Value> = scalars
            .iter()
            .map(|(path, value)| (path.as_str(), value))
            .collect();

        // `.type` is on claude.txt -- the real sanitizer kept it verbatim,
        // and `collect_scalars` names it at exactly the path `allows` agrees
        // with.
        assert!(allows(Provider::Claude, ".type"));
        assert_eq!(by_path[".type"].as_str(), Some("user"));

        // `.mystery` is on no list -- the real sanitizer replaced it, and
        // `collect_scalars` names it at exactly the path `allows` rejects.
        assert!(!allows(Provider::Claude, ".mystery"));
        assert!(by_path[".mystery"].as_str().unwrap().starts_with('<'));

        // An array of objects collapses to one shared path with `[]`, not
        // one path per index -- both elements' `unknown` leaves land on
        // `.nested.list[].unknown`, matching `sanitize_value_tree`'s own
        // array handling, and the real sanitizer redacted both (same path,
        // not on the list).
        assert!(!allows(Provider::Claude, ".nested.list[].unknown"));
        let unknown_occurrences: Vec<&Value> = scalars
            .iter()
            .filter(|(path, _)| path == ".nested.list[].unknown")
            .map(|(_, value)| value)
            .collect();
        assert_eq!(unknown_occurrences.len(), 2);
        for value in unknown_occurrences {
            assert!(value.as_str().unwrap().starts_with('<'));
        }

        // `.mcp_servers[].status` IS on claude.txt -- the real sanitizer kept
        // it verbatim through the array, at the same collapsed path.
        assert!(allows(Provider::Claude, ".mcp_servers[].status"));
        assert_eq!(by_path[".mcp_servers[].status"].as_str(), Some("connected"));

        // Sanity: every path `collect_scalars` produced agrees in aggregate
        // with `allows` on verbatim-vs-placeholder for every scalar the real
        // sanitizer touched, not just the four spot-checked above.
        for (path, value) in &scalars {
            let kept_verbatim = match value {
                Value::String(text) => !text.starts_with('<') || !text.ends_with('>'),
                _ => true,
            };
            if kept_verbatim {
                assert!(
                    allows(Provider::Claude, path),
                    "{path} was kept verbatim ({value:?}) but is not on the allowlist"
                );
            }
        }
    }
}
