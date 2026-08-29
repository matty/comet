//! Every scalar in every committed `events.jsonl` is either at an
//! allowlisted path or is a placeholder.
//!
//! This replaces thirty-one example-based tests (`sanitizer_semantics.rs`,
//! deleted by this same change) that each asserted one particular shape gets
//! redacted -- "does a Codex reasoning summary get typed as prose," "does a
//! thinking signature get typed by shape." Under an allowlist those questions
//! stop existing: unlisted means replaced, no recognition involved. What
//! replaces them is a **total property** over the frame evidence, not a
//! sample of it -- it asserts nothing anywhere escaped, over every committed
//! `events.jsonl` byte, and it keeps holding for captures that do not exist
//! yet.
//!
//! **Scope: `events.jsonl` only, not the whole corpus directory.** The one
//! kind of committed file this walk still does not visit is each scenario's
//! `README.md` -- prose, not frame evidence, so it was never a candidate.
//! Until D75, a second carve-out sat beside it: `claude/2.1.229/subagent/
//! read-back-run-journal.jsonl` and `read-back-doc-snapshot.json` were a
//! *different* session's read-back evidence, hand-sanitized by an earlier
//! slice to its own documented standard rather than through `sanitize_dir`,
//! so this property skipped them too. D75 deleted both files rather than
//! bringing them under the allowlist, so that carve-out is gone -- the
//! property's remit is now the whole corpus's frame evidence, with nothing
//! exempted for being sanitized to a different standard. A green run here
//! certifies the frame evidence, not literally every byte under
//! `tests/corpus/`.
//!
//! **What a green run does *not* certify.** This property only ever inspects
//! the content of a scalar it is about to call an escape (an unlisted path,
//! or the `mcp__` exception below) -- a value that is genuinely kept because
//! its path is allowed is never inspected for what it contains. That is
//! `sanitize_dir`'s job (the fail-closed scans in
//! `crates/capture/src/sanitize.rs`, covered by
//! `sanitizer_safety.rs`), not this property's. A green result here means
//! "clean at every unlisted path," on the assumption the corpus came from
//! `sanitize_dir` and was never hand-edited afterward -- it does not mean
//! "the archive contains no credential or path," because an allowlisted
//! field is exactly the place this test does not look.
//!
//! Deliberately red at the commit that adds it. The committed corpus was
//! sanitized by the blocklist this stage replaces, and the blocklist let
//! through exactly the shapes an allowlist is built to catch by construction
//! (the recording user's OS build, installed plugins, subagent roster, MCP
//! connector identity). Task 5 re-sanitizes the archive against the new
//! allowlist and turns this green; until then, the failure below is the
//! proof the property does what it says.

use std::path::Path;

use comet_capture::{
    Provider, allows, allows_prefix, corpus_root, escape_path_segment, frames, is_map_path,
    is_named_map_child, is_placeholder_token, promoted_scenarios,
};
use serde_json::Value;

/// One committed scalar that is neither on its provider's allowlist nor
/// shaped like a placeholder -- named by where it lives, never by what it
/// says. Carries no value: echoing the value here would make this failure
/// message a second copy of exactly what the allowlist exists to withhold,
/// and this message lands in terminals and PR bodies.
///
/// `path` itself can still carry a map key verbatim (Claude's `.modelUsage`
/// is keyed by model id), the same caveat `NovelPath`'s own doc comment
/// records in `sanitize.rs` -- an object key is never a *value* this struct
/// withholds, and `validate_key` fail-closed-scans every key before it can
/// reach here.
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
    let corpus_root = corpus_root();
    let mut escapes = Vec::new();
    let mut scenario_count = 0u64;

    let scenarios = promoted_scenarios(&corpus_root)
        .unwrap_or_else(|error| panic!("{} could not be walked: {error}", corpus_root.display()));
    for scenario in scenarios {
        scenario_count += 1;
        let manifest_path = scenario.directory.join("manifest.json");
        let provider = manifest_provider(&manifest_path, &scenario.label);
        check_scenario(&scenario.directory, &scenario.label, provider, &mut escapes);
    }

    // Mirrors the same guard the shared corpus walk uses: a broken walk that
    // silently visits nothing must fail loudly, not read as "the property
    // holds" by finding zero frames to check.
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
         against `crates/capture/src/allowlist/claude.txt` or `codex.txt` \
         before deciding whether the path belongs there):",
        escapes.len()
    )];
    for escape in &escapes {
        report.push(format!("  {escape}"));
    }
    panic!("{}", report.join("\n"));
}

/// The same total property, in key position: every object key committed at a
/// **map path** is either a prefix of an allowlisted path, a placeholder, or
/// a declared `named_children` entry (D123) — a key whose survival is a
/// reviewed decision about the *field name*, unrelated to whether its value
/// is allowlisted.
///
/// Split from the scalar property rather than folded into it because the two
/// answer different questions. A key that is a *field name* is published on
/// purpose — each version's capability sheet under `docs/providers/` is a
/// snapshot of exactly those names, and this walk must not be read as
/// licensing their redaction. A key that is
/// *data* is an identifier the archive has no more business publishing than a
/// value, and until the key rule landed nothing checked it: `collect_scalars`
/// visits `String`/`Number` leaves, so an MCP server name in key position was
/// invisible to every test in this file.
#[test]
fn every_committed_map_key_is_allowlisted_or_a_placeholder() {
    let corpus_root = corpus_root();
    let mut escapes = Vec::new();
    let mut scenario_count = 0u64;

    let scenarios = promoted_scenarios(&corpus_root)
        .unwrap_or_else(|error| panic!("{} could not be walked: {error}", corpus_root.display()));
    for scenario in scenarios {
        scenario_count += 1;
        let provider =
            manifest_provider(&scenario.directory.join("manifest.json"), &scenario.label);
        let events = frames(&scenario.directory)
            .unwrap_or_else(|error| panic!("{}: events.jsonl unreadable: {error}", scenario.label));
        for event in events {
            let sequence = event["sequence"].as_u64().unwrap_or_default();
            let Some(payload) = event["payload"].as_str() else {
                continue;
            };
            let Ok(payload) = serde_json::from_str::<Value>(payload) else {
                continue;
            };
            let mut keys = Vec::new();
            collect_map_keys(&payload, "", &mut keys);
            for (parent_path, key) in keys {
                let formed = format!("{parent_path}.{key}");
                // A named child (D123) is the third way a committed map key
                // can be legitimate: its spelling survives on purpose, as a
                // reviewed field name, not because its value happened to be
                // allowlisted or its identity happened to be placeholdered.
                if allows_prefix(provider, &formed)
                    || is_placeholder_token(&key)
                    || is_named_map_child(&parent_path, &key)
                {
                    continue;
                }
                escapes.push(Escape {
                    scenario: scenario.label.clone(),
                    sequence,
                    // The map position, never the key: this message
                    // lands in terminals and PR bodies, and naming the
                    // key would republish the identifier the failure
                    // is complaining about.
                    path: format!("{parent_path}.{{}}"),
                });
            }
        }
    }

    assert!(
        scenario_count > 0,
        "found no events.jsonl under {} -- corpus walk is broken, not just empty",
        corpus_root.display()
    );
    assert!(
        escapes.is_empty(),
        "{} map key(s) are committed verbatim at a path the allowlist does not license \
         (keys withheld deliberately -- open the named frame locally):\n  {}",
        escapes.len(),
        escapes
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n  ")
    );
}

/// Every `(parent_path, key)` pair sitting at a declared map path. Mirrors
/// `sanitize_value_tree`'s own array/object path arithmetic, the same way
/// `collect_scalars` does.
fn collect_map_keys(value: &Value, path: &str, out: &mut Vec<(String, String)>) {
    match value {
        Value::Array(items) => {
            let child_path = format!("{path}[]");
            for item in items {
                collect_map_keys(item, &child_path, out);
            }
        }
        Value::Object(object) => {
            let is_map = is_map_path(path);
            for (key, child) in object {
                if is_map {
                    out.push((path.to_owned(), key.clone()));
                }
                collect_map_keys(child, &format!("{path}.{}", escape_path_segment(key)), out);
            }
        }
        _ => {}
    }
}

/// Every `<..._N>`/`<...>` bracket-shaped token committed anywhere in a
/// manifest is actually shaped like a placeholder this sanitizer can produce
/// (`is_placeholder_token`): a numbered generic/named token, or one of the
/// seven literal path roots.
///
/// Used to check reciprocity against a `placeholders` array each manifest
/// declared; that array was dropped at the stage-6 promotion (nobody read
/// the per-capture accounting), so this checks shape directly instead. No
/// declared list left to fall out of sync with the text.
#[test]
fn every_bracket_shaped_manifest_token_is_placeholder_shaped() {
    let corpus_root = corpus_root();
    let mut unshaped = Vec::new();
    let mut manifest_count = 0u64;

    let scenarios = promoted_scenarios(&corpus_root)
        .unwrap_or_else(|error| panic!("{} could not be walked: {error}", corpus_root.display()));
    for scenario in scenarios {
        let manifest_path = scenario.directory.join("manifest.json");
        if !manifest_path.is_file() {
            continue;
        }
        manifest_count += 1;
        let text = std::fs::read_to_string(&manifest_path).unwrap_or_else(|error| {
            panic!("{}: manifest.json unreadable: {error}", scenario.label)
        });
        let manifest: Value = serde_json::from_str(&text).unwrap_or_else(|error| {
            panic!(
                "{}: manifest.json is not valid JSON: {error}",
                scenario.label
            )
        });

        let mut tokens = Vec::new();
        collect_bracket_tokens(&manifest, &mut tokens);
        for token in tokens {
            if is_placeholder_token(&token) {
                continue;
            }
            unshaped.push(format!("{}: {token}", scenario.label));
        }
    }

    assert!(
        manifest_count > 0,
        "found no manifest.json under {} -- corpus walk is broken, not just empty",
        corpus_root.display()
    );
    assert!(
        unshaped.is_empty(),
        "bracket-shaped token(s) in a committed manifest are not shaped like any \
         placeholder this sanitizer produces:\n{}",
        unshaped.join("\n")
    );
}

/// Every string leaf in `value` that looks like a placeholder token: starts
/// with `<`, ends with `>`, and holds no whitespace (a model-authored prose
/// placeholder never has this shape, so this cannot mistake ordinary text
/// for one).
fn collect_bracket_tokens(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Array(items) => {
            for item in items {
                collect_bracket_tokens(item, out);
            }
        }
        Value::Object(object) => {
            for child in object.values() {
                collect_bracket_tokens(child, out);
            }
        }
        Value::String(text)
            if text.starts_with('<')
                && text.ends_with('>')
                && !text.contains(char::is_whitespace) =>
        {
            out.push(text.clone());
        }
        _ => {}
    }
}

/// One scenario's worth of frames, checked against its own provider.
/// Failures are pushed onto `escapes` rather than panicking here, so one bad
/// scenario does not hide every other.
fn check_scenario(
    scenario_dir: &Path,
    scenario: &str,
    provider: Provider,
    escapes: &mut Vec<Escape>,
) {
    let events = frames(scenario_dir)
        .unwrap_or_else(|error| panic!("{scenario}: events.jsonl unreadable: {error}"));
    for event in events {
        let sequence = event["sequence"]
            .as_u64()
            .unwrap_or_else(|| panic!("{scenario}: an event line has no sequence: {event}"));
        let Some(payload) = event["payload"].as_str() else {
            panic!("{scenario} frame {sequence}: event has no payload string");
        };

        match serde_json::from_str::<Value>(payload) {
            Ok(value) => {
                let mut scalars = Vec::new();
                collect_scalars(&value, "", &mut scalars);
                for (path, scalar) in scalars {
                    if is_escape(provider, &path, &scalar) {
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
                if !is_placeholder_token(payload) {
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
/// `crates/capture/src/sanitize.rs`: an array element's path grows
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
                let child_path = format!("{path}.{}", escape_path_segment(key));
                collect_scalars(child, &child_path, out);
            }
        }
        Value::String(_) | Value::Number(_) => out.push((path.to_owned(), value.clone())),
        _ => {}
    }
}

/// Whether the scalar at `path` counts as an escape: present verbatim, not a
/// placeholder, at a spot the allowlist does not license.
///
/// Mirrors `Redactor::sanitize_scalar`'s own condition in `sanitize.rs`
/// exactly -- `if path_allowed && !is_mcp_tool_identity(value)` -- not just
/// `allows(provider, path)` alone. Five of Claude's allowed paths hold a
/// tool name (`.event.content_block.name`, `.last_tool_name`,
/// `.message.content[].content[].tool_name`, `.message.content[].name`,
/// `.request.tool_name` -- `.tool_use_result.matches[]` left this family at
/// the stage-6 promotion, closing D73), and an MCP invocation puts
/// `mcp__<server>__<tool>` there, embedding server identity the allowlist
/// otherwise excludes. See `is_mcp_tool_identity`'s doc comment in
/// `sanitize.rs` for the reviewed reasoning.
fn is_escape(provider: Provider, path: &str, scalar: &Value) -> bool {
    let kept_verbatim = allows(provider, path) && !is_mcp_tool_identity(scalar);
    if kept_verbatim {
        return false;
    }
    !matches!(scalar, Value::String(text) if is_placeholder_token(text))
}

/// Local mirror of the private `is_mcp_tool_identity` in `sanitize.rs` --
/// same name, same one-line rule (a string starting with the MCP tool-name
/// prefix), kept in lockstep by the `self_check` test below rather than by
/// visibility this test binary cannot reach across the crate boundary.
fn is_mcp_tool_identity(value: &Value) -> bool {
    value.as_str().is_some_and(|text| text.starts_with("mcp__"))
}

/// The provider a scenario's own manifest declares.
///
/// Used to also read `manifest["placeholders"]` alongside it; that array was
/// dropped at the stage-6 promotion, and placeholder recognition moved to
/// shape (`is_placeholder_token`), so `provider` is all that is left to read.
fn manifest_provider(manifest_path: &Path, scenario: &str) -> Provider {
    let text = std::fs::read_to_string(manifest_path)
        .unwrap_or_else(|error| panic!("{scenario}: manifest.json unreadable: {error}"));
    let manifest: Value = serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("{scenario}: manifest.json is not valid JSON: {error}"));
    match manifest["provider"].as_str() {
        Some("claude") => Provider::Claude,
        Some("codex") => Provider::Codex,
        Some("acp") => Provider::Acp,
        other => panic!("{scenario}: manifest has an unknown provider: {other:?}"),
    }
}

#[cfg(test)]
mod self_check {
    //! Self-review evidence, not corpus evidence: proves `collect_scalars`'s
    //! path strings agree with what the real `sanitize_dir` actually decides,
    //! rather than resting on "the code above looks like the code in
    //! `sanitize.rs`." Runs a synthetic capture through the production
    //! sanitizer and cross-checks every scalar's path against its real
    //! allow/placeholder outcome.

    use comet_capture::{Provider, allows, sanitize_dir};
    use serde_json::Value;

    use super::super::support::{staging_dir, write_raw_capture};
    use super::{collect_scalars, is_escape};

    #[test]
    fn collect_scalars_paths_agree_with_the_real_sanitizer() {
        let temp = tempfile::tempdir().unwrap();
        let raw = write_raw_capture(
            temp.path(),
            "path-equivalence",
            &[
                r#"{"type":"user","mystery":"unlisted-value","nested":{"list":[{"unknown":"a"},{"unknown":"b"}]},"mcp_servers":[{"status":"connected"}],"request":{"tool_name":"mcp__linear__create_issue"},"x.ai/vendor":{"leaf":"vendor-value"}}"#,
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

        // `.request.tool_name` is ALSO on claude.txt (the tool-name-at-
        // invocation family), but the value it held was `mcp__...` -- the
        // real sanitizer's `is_mcp_tool_identity` exception still redacted
        // it despite the allowed path, and `is_escape` agrees this is not an
        // escape (it is a placeholder, not the raw MCP identity).
        assert!(allows(Provider::Claude, ".request.tool_name"));
        let redacted_tool_name = by_path[".request.tool_name"].as_str().unwrap();
        assert_ne!(redacted_tool_name, "mcp__linear__create_issue");
        assert!(
            !is_escape(
                Provider::Claude,
                ".request.tool_name",
                by_path[".request.tool_name"],
            ),
            "the real sanitizer's redacted mcp__ placeholder must not read as an escape"
        );

        // A key carrying a path delimiter is escaped where it joins, exactly
        // as `sanitize_value_tree` and `Visit::walk` escape it -- so the
        // mirror spells a vendor-namespaced key the same way an allowlist
        // line has to. Unpinned until Grok's promotion, when three licensed
        // `x\.ai/sessionConfig` lines read as 30 escapes over the whole
        // corpus because this walker built `.x.ai/vendor` instead.
        assert!(
            by_path.contains_key(r".x\.ai/vendor.leaf"),
            "a dotted key must be escaped where it joins the path, got: {:?}",
            by_path.keys().collect::<Vec<_>>()
        );
        assert!(!allows(Provider::Claude, r".x\.ai/vendor.leaf"));
        assert!(
            by_path[r".x\.ai/vendor.leaf"]
                .as_str()
                .unwrap()
                .starts_with('<'),
            "the value under a dotted key is default-deny like any other"
        );

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

    /// The critical case: `is_escape` must not clear a value just because its
    /// path is on the allowlist. Constructs the raw (unsanitized) shape
    /// directly, rather than going through `sanitize_dir` -- the real
    /// sanitizer would never *leave* an `mcp__` value unredacted, so proving
    /// the property catches it requires a value that skipped sanitizing (a
    /// hand-repaired or regression-produced archive entry), which is exactly
    /// the scenario this test stands in for.
    #[test]
    fn is_escape_flags_an_mcp_tool_name_riding_an_allowlisted_path() {
        let path = ".request.tool_name";
        assert!(
            allows(Provider::Claude, path),
            "test premise: {path} must actually be on claude.txt"
        );

        let raw_mcp_identity = Value::String("mcp__linear__create_issue".to_owned());
        assert!(
            is_escape(Provider::Claude, path, &raw_mcp_identity),
            "a raw mcp__ value at an allowlisted path must still count as an escape"
        );

        // Same path, a built-in tool name -- the exception is scoped to the
        // `mcp__` prefix, not the whole path, so this must NOT be an escape.
        let builtin = Value::String("Bash".to_owned());
        assert!(!is_escape(Provider::Claude, path, &builtin));

        // The same mcp__ identity IS clean once it has actually become a
        // placeholder -- shape-recognized directly, with no declared list to
        // consult.
        let placeholder_value = Value::String("<V1>".to_owned());
        assert!(!is_escape(Provider::Claude, path, &placeholder_value));
    }
}
