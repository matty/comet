//! The corpus walker behind the new-field gate.
//!
//! Every property here is one the gate depends on: a field must not lose its
//! direction, a data-keyed map must not become a row per key, the same key at
//! two paths must stay two fields, and a corpus the walker cannot read must be
//! an error rather than an empty answer.
//!
//! The D77 tests below are a different layer of the same mechanism: not the
//! corpus walker itself, but the review-time heuristic (`surface::suspected_map`,
//! wired into `sanitize_dir`) that warns about an undeclared map *before* a
//! corpus exists for the walker above to check at all.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use comet_capture::{
    Direction, FieldObservation, SanitizationError, corpus_root, observe_surface, sanitize_dir,
};
use serde_json::{Value, json};

use super::support::*;

/// Writes one scenario directory: the frames, each payload a JSON string
/// exactly as the corpus stores it.
fn write_scenario(
    root: &Path,
    provider: &str,
    version: &str,
    scenario: &str,
    frames: &[(u64, &str, Value)],
) -> PathBuf {
    let directory = root.join(provider).join(version).join(scenario);
    std::fs::create_dir_all(&directory).unwrap();
    let mut events = String::new();
    for (sequence, channel, payload) in frames {
        let line = json!({
            "sequence": sequence,
            "channel": channel,
            "payload": serde_json::to_string(payload).unwrap(),
        });
        events.push_str(&serde_json::to_string(&line).unwrap());
        events.push('\n');
    }
    std::fs::write(directory.join("events.jsonl"), events).unwrap();
    directory
}

fn find<'a>(observations: &'a [FieldObservation], path: &str) -> &'a FieldObservation {
    observations
        .iter()
        .find(|observation| observation.path == path)
        .unwrap_or_else(|| {
            let known: Vec<&str> = observations
                .iter()
                .map(|observation| observation.path.as_str())
                .collect();
            panic!("no observation at {path}; inventory holds {known:?}")
        })
}

fn paths(observations: &[FieldObservation]) -> BTreeSet<&str> {
    observations
        .iter()
        .map(|observation| observation.path.as_str())
        .collect()
}

/// Break caught: the inventory loses the direction a field travelled, so input
/// surface (how Comet drives the client) is indistinguishable from reply
/// surface (what the client reports), and the gate answers half the question.
#[test]
fn inventory_records_direction_and_the_first_frame() {
    let root = tempfile::tempdir().unwrap();
    write_scenario(
        root.path(),
        "claude",
        "2.1.228",
        "fresh-text",
        &[
            (1, "stdin", json!({"type": "user", "prompt_len": 12})),
            (
                2,
                "stdout",
                json!({"type": "assistant", "stop_reason": "end_turn"}),
            ),
        ],
    );
    write_scenario(
        root.path(),
        "claude",
        "2.1.229",
        "checklist",
        &[(
            7,
            "stdout",
            json!({"type": "assistant", "stop_reason": "tool_use"}),
        )],
    );

    let observations = observe_surface(root.path()).unwrap().0;

    let prompt_len = find(&observations, ".prompt_len");
    assert_eq!(prompt_len.direction, Direction::ToProvider);
    assert_eq!(prompt_len.first_seen.scenario, "claude/2.1.228/fresh-text");
    assert_eq!(prompt_len.first_seen.sequence, 1);

    let stop_reason = find(&observations, ".stop_reason");
    assert_eq!(stop_reason.direction, Direction::FromProvider);
    assert_eq!(
        stop_reason.first_seen.sequence, 2,
        "the first frame, not the last"
    );
}

/// Break caught (§2.1, the defect this stage exists to fix): the inventory
/// keyed on `(provider, direction, path)` alone merges every version of a
/// provider into one set. A field only one version emits is easy to get
/// right by accident (the paths never collide), so the real probe is a field
/// **both** versions emit at the same path: without `version` in the key,
/// `or_insert_with` only records the first scenario that reaches it, so the
/// second version's observation is silently dropped and a per-version filter
/// would wrongly report the field absent from whichever version lost the
/// race.
#[test]
fn inventory_keeps_two_versions_of_one_provider_apart() {
    let root = tempfile::tempdir().unwrap();
    write_scenario(
        root.path(),
        "claude",
        "1.0.0",
        "alpha",
        &[(1, "stdout", json!({"onlyOld": 1, "shared": "old-value"}))],
    );
    write_scenario(
        root.path(),
        "claude",
        "2.0.0",
        "alpha",
        &[(1, "stdout", json!({"onlyNew": 1, "shared": "new-value"}))],
    );

    let observations = observe_surface(root.path()).unwrap().0;

    let old_field = find(&observations, ".onlyOld");
    assert_eq!(old_field.version, "1.0.0");
    let new_field = find(&observations, ".onlyNew");
    assert_eq!(new_field.version, "2.0.0");

    assert!(
        !observations
            .iter()
            .any(|observation| observation.path == ".onlyOld" && observation.version == "2.0.0"),
        "a field only 1.0.0 emitted must not appear under 2.0.0"
    );
    assert!(
        !observations
            .iter()
            .any(|observation| observation.path == ".onlyNew" && observation.version == "1.0.0"),
        "a field only 2.0.0 emitted must not appear under 1.0.0"
    );

    // The real probe: a field both versions emit at the same path must yield
    // two observations, not one first-scenario-wins entry.
    let shared: Vec<&FieldObservation> = observations
        .iter()
        .filter(|observation| observation.path == ".shared")
        .collect();
    let shared_versions: BTreeSet<&str> = shared
        .iter()
        .map(|observation| observation.version.as_str())
        .collect();
    assert_eq!(
        shared_versions,
        BTreeSet::from(["1.0.0", "2.0.0"]),
        "a field shared by both versions must be observed under each, not merged into one \
         first-scenario-wins entry: {shared:?}"
    );
}

/// Break caught: a map keyed on data reports each key as a field, so the
/// snapshot grows a row per model id and the real field
/// (`.modelUsage.{}.costUSD`) is never named. Observed for real:
/// `claude-haiku-4-5-20251001` under `.modelUsage`.
#[test]
fn inventory_folds_a_declared_map_path_into_one_entry() {
    let root = tempfile::tempdir().unwrap();
    write_scenario(
        root.path(),
        "claude",
        "2.1.229",
        "checklist",
        &[(
            1,
            "stdout",
            json!({
                "modelUsage": {
                    "claude-haiku-4-5-20251001": {"costUSD": 0.01},
                    "claude-sonnet-5": {"costUSD": 0.02},
                }
            }),
        )],
    );

    let observations = observe_surface(root.path()).unwrap().0;
    let seen = paths(&observations);

    assert!(
        !seen.contains(".modelUsage.claude-haiku-4-5-20251001"),
        "a model id became a field name: {seen:?}"
    );
    assert!(seen.contains(".modelUsage"));
    assert!(
        seen.contains(".modelUsage.{}.costUSD"),
        "the map's value fields must still be named: {seen:?}"
    );
}

/// Break caught (D123): a declared map path used to fold *every* child to
/// `.{}`, so a field production genuinely decodes (`normalize::typed_call`
/// reads `rawInput["pattern"]` and `rawInput["path"]` on a `search`-kind ACP
/// frame) was invisible to the golden test — a future agent dropping
/// `pattern` would break the decode with the sheet still green. `MapPath`'s
/// `named_children` is the fix: a listed child stays visible as its own
/// field, while every *unlisted* sibling under the same map (`glob`, a
/// filesystem path here, or `target_file` for a different tool) still folds,
/// which is the property that keeps this from becoming a leaf-name heuristic.
#[test]
fn a_named_map_child_reaches_the_sheet_while_an_unnamed_sibling_still_folds() {
    let root = tempfile::tempdir().unwrap();
    write_scenario(
        root.path(),
        "claude-agent-acp",
        "0.70.0",
        "steer-grok",
        &[(
            1,
            "stdout",
            json!({
                "params": {
                    "update": {
                        "rawInput": {
                            "variant": "Grep",
                            "pattern": "needle",
                            "path": "C:\\Users\\coding\\AppData\\Local\\Temp\\cwd",
                            // An unnamed sibling with a nested object, the
                            // same shape `.modelUsage.{}.costUSD` proves
                            // folding with above: if the fold still applies,
                            // this shows up as `.{}.mimeType`, never as
                            // `.attachment.mimeType`.
                            "attachment": {"mimeType": "text/plain"},
                        }
                    }
                }
            }),
        )],
    );

    let observations = observe_surface(root.path()).unwrap().0;
    let seen = paths(&observations);

    assert!(
        seen.contains(".params.update.rawInput.pattern"),
        "a named child must be recorded under its own field name: {seen:?}"
    );
    assert!(
        seen.contains(".params.update.rawInput.path"),
        "a named child must be recorded under its own field name: {seen:?}"
    );
    assert!(
        !seen.contains(".params.update.rawInput.variant"),
        "an unnamed scalar sibling must not publish as a field: {seen:?}"
    );
    assert!(
        !seen.contains(".params.update.rawInput.attachment"),
        "an unnamed sibling's own key must not publish as a field: {seen:?}"
    );
    assert!(
        seen.contains(".params.update.rawInput.{}.mimeType"),
        "an unnamed sibling must still fold to `{{}}`, not disappear or leak its key: {seen:?}"
    );
}

/// Break caught (D77, second live instance, 2026-08-29): ACP's
/// `session/request_permission` carries the same tool-argument map at
/// `.params.toolCall.rawInput` that `.params.update.rawInput` already
/// declares — `acp::approval::command` (`crates/harness/src/acp/approval.rs`)
/// reads `command` and `cwd` off it — but nothing had declared this second
/// path, so a tool's own argument keys would have published as field names
/// the day an approval scenario is promoted. Mirrors
/// `a_named_map_child_reaches_the_sheet_while_an_unnamed_sibling_still_folds`
/// above, at the sibling path.
#[test]
fn a_named_map_child_reaches_the_sheet_for_tool_call_raw_input_too() {
    let root = tempfile::tempdir().unwrap();
    write_scenario(
        root.path(),
        "claude-agent-acp",
        "0.70.0",
        "approve-command",
        &[(
            1,
            "stdout",
            json!({
                "params": {
                    "toolCall": {
                        "kind": "execute",
                        "rawInput": {
                            "command": "rm -rf /tmp/x",
                            "cwd": "C:\\Users\\coding\\AppData\\Local\\Temp\\cwd",
                            // An unnamed sibling this build does not decode —
                            // must still fold, the same probe
                            // `attachment`/`mimeType` runs above for
                            // `.params.update.rawInput`.
                            "timeout": {"seconds": 30},
                        }
                    }
                }
            }),
        )],
    );

    let observations = observe_surface(root.path()).unwrap().0;
    let seen = paths(&observations);

    assert!(
        seen.contains(".params.toolCall.rawInput.command"),
        "a named child must be recorded under its own field name: {seen:?}"
    );
    assert!(
        seen.contains(".params.toolCall.rawInput.cwd"),
        "a named child must be recorded under its own field name: {seen:?}"
    );
    assert!(
        !seen.contains(".params.toolCall.rawInput.timeout"),
        "an unnamed sibling's own key must not publish as a field: {seen:?}"
    );
    assert!(
        seen.contains(".params.toolCall.rawInput.{}.seconds"),
        "an unnamed sibling must still fold to `{{}}`, not disappear or leak its key: {seen:?}"
    );
}

/// Break caught: the same key at two different places folds into one row, so
/// `content` in a tool input and `content` in an assistant message become
/// indistinguishable.
#[test]
fn inventory_separates_the_same_key_seen_at_two_paths() {
    let root = tempfile::tempdir().unwrap();
    write_scenario(
        root.path(),
        "claude",
        "2.1.229",
        "checklist",
        &[(
            1,
            "stdout",
            json!({
                "message": {"content": "outer"},
                "input": {"content": "inner"},
            }),
        )],
    );

    let observations = observe_surface(root.path()).unwrap().0;
    let seen = paths(&observations);

    assert!(seen.contains(".message.content"), "{seen:?}");
    assert!(seen.contains(".input.content"), "{seen:?}");
}

/// Break caught: a corpus map path nobody declared turns its data keys into
/// field names, and the snapshot grows a row per model id.
///
/// This runs against the real promoted corpus rather than a fixture, because
/// `MAP_PATHS` in `src/capture/surface.rs` is a declared list and the only
/// thing that can prove it is still complete is the evidence itself. When a
/// provider adds a data-keyed object, this is what says so.
#[test]
fn the_promoted_corpus_yields_both_directions_and_no_data_shaped_field_names() {
    let observations = observe_surface(&corpus_root()).unwrap().0;

    assert!(observations.len() > 100, "{} observed", observations.len());
    for direction in [Direction::ToProvider, Direction::FromProvider] {
        assert!(
            observations
                .iter()
                .any(|observation| observation.direction == direction),
            "the corpus records no {direction:?} surface"
        );
    }

    let data_shaped: Vec<&str> = observations
        .iter()
        .filter(|observation| {
            observation
                .path
                .split('.')
                .any(|segment| segment.starts_with("claude-") || segment.starts_with("gpt-"))
        })
        .map(|observation| observation.path.as_str())
        .collect();
    assert!(
        data_shaped.is_empty(),
        "undeclared map path: these are values, not fields: {data_shaped:?}"
    );
}

/// Break caught (PR #58 review): a structured frame that will not parse is
/// skipped, so the snapshot looks complete while quietly missing that frame's
/// fields. Only stderr may carry plain text.
#[test]
fn an_unparseable_structured_frame_is_an_error_and_stderr_text_is_not() {
    let root = tempfile::tempdir().unwrap();
    let directory = write_scenario(
        root.path(),
        "claude",
        "2.1.229",
        "checklist",
        &[(1, "stdout", json!({"kept": "yes"}))],
    );
    // stderr is allowed to be prose; the walk continues past it.
    let mut events = std::fs::read_to_string(directory.join("events.jsonl")).unwrap();
    events.push_str(
        &serde_json::to_string(&json!({
            "sequence": 2, "channel": "stderr", "payload": "a plain diagnostic line",
        }))
        .unwrap(),
    );
    events.push('\n');
    std::fs::write(directory.join("events.jsonl"), &events).unwrap();
    assert!(
        observe_surface(root.path()).is_ok(),
        "stderr prose must not stop the walk"
    );

    // stdout carrying the same prose must not.
    events.push_str(
        &serde_json::to_string(&json!({
            "sequence": 3, "channel": "stdout", "payload": "not json at all",
        }))
        .unwrap(),
    );
    events.push('\n');
    std::fs::write(directory.join("events.jsonl"), events).unwrap();
    assert!(
        observe_surface(root.path()).is_err(),
        "an unparseable stdout frame silently vanished from the inventory"
    );
}

/// Break caught: the walker silently reports an empty inventory when the
/// corpus root is wrong, which reads as "no new fields" - the most dangerous
/// possible false negative for a gate whose job is finding what arrived.
#[test]
fn an_empty_or_missing_corpus_root_is_an_error_not_an_empty_inventory() {
    let missing = tempfile::tempdir().unwrap().path().join("absent");
    assert!(observe_surface(&missing).is_err());

    let empty = tempfile::tempdir().unwrap();
    assert!(observe_surface(empty.path()).is_err());
}

/// Break caught: the value vocabulary is what turns "a `.type` field exists"
/// into "this harness emits `system`, `assistant`, `result`" (design §3.5).
/// Three frames whose `.type` differs must all show up as three distinct
/// values, and a field that varies just as much but is not on
/// `VOCABULARY_PATHS` must not contribute anything — collecting every path
/// would make the vocabulary indistinguishable from the field inventory
/// `observe_surface`'s inventory half already provides.
#[test]
fn vocabulary_collects_declared_paths_only() {
    let root = tempfile::tempdir().unwrap();
    write_scenario(
        root.path(),
        "claude",
        "2.1.229",
        "checklist",
        &[
            (
                1,
                "stdout",
                json!({"type": "system", "undeclared": "alpha"}),
            ),
            (
                2,
                "stdout",
                json!({"type": "assistant", "undeclared": "beta"}),
            ),
            (
                3,
                "stdout",
                json!({"type": "result", "undeclared": "gamma"}),
            ),
        ],
    );

    let vocabulary = observe_surface(root.path()).unwrap().1;
    let claude = vocabulary
        .get(&(
            "claude".to_string(),
            "2.1.229".to_string(),
            Direction::FromProvider,
        ))
        .unwrap_or_else(|| {
            panic!("no vocabulary for claude/2.1.229/from-provider: {vocabulary:?}")
        });

    assert_eq!(
        claude.get(".type").cloned().unwrap_or_default(),
        BTreeSet::from([
            "system".to_string(),
            "assistant".to_string(),
            "result".to_string(),
        ]),
        "all three .type values must be collected: {claude:?}"
    );
    assert!(
        !claude.contains_key(".undeclared"),
        "an undeclared path must not appear in the vocabulary: {claude:?}"
    );
}

/// Break caught: an object or array sitting at a declared path is a shape
/// change worth noticing on its own, not a value to stringify into the
/// vocabulary — stringifying it would either panic reaching for a string or
/// silently print `{...}`/`[...]` as if that were one more discriminator
/// value.
#[test]
fn vocabulary_ignores_a_non_scalar_at_a_declared_path() {
    let root = tempfile::tempdir().unwrap();
    // A sibling scalar at another declared path (`.subtype`) in the same
    // frame is the point: a fixture with only the non-scalar would pass this
    // test even if collection were disabled outright, because an entirely
    // empty vocabulary also satisfies "no `.type` value" (review finding,
    // 2026-08-16 — the original fixture could not distinguish the fixed
    // implementation from a broken one and was never actually falsified).
    write_scenario(
        root.path(),
        "claude",
        "2.1.229",
        "checklist",
        &[(
            1,
            "stdout",
            json!({"type": {"nested": "shape-change"}, "subtype": "init"}),
        )],
    );

    let vocabulary = observe_surface(root.path()).unwrap().1;
    let claude = vocabulary
        .get(&(
            "claude".to_string(),
            "2.1.229".to_string(),
            Direction::FromProvider,
        ))
        .unwrap_or_else(|| {
            panic!("no vocabulary for claude/2.1.229/from-provider: {vocabulary:?}")
        });

    assert_eq!(
        claude.get(".subtype").cloned().unwrap_or_default(),
        BTreeSet::from(["init".to_string()]),
        "a sibling scalar at a declared path must still be collected: {claude:?}"
    );
    assert!(
        !claude.contains_key(".type") || claude[".type"].is_empty(),
        "a non-scalar at a declared path must not become a vocabulary value: {claude:?}"
    );
}

/// Break caught (2026-08-16 correction): `.method` is Codex's frame-kind
/// discriminator — Codex is JSON-RPC and carries no root `.type`, so without
/// `.method` on `VOCABULARY_PATHS` a Codex sheet would report an empty
/// vocabulary for a provider that actually emits two dozen distinct methods.
/// `stdin` methods (what Comet can drive Codex with) and non-`stdin` methods
/// (what Codex emits) must both be collected, and land under their own
/// direction rather than one merged set.
#[test]
fn vocabulary_collects_method_for_codex() {
    let root = tempfile::tempdir().unwrap();
    write_scenario(
        root.path(),
        "codex",
        "0.147.0",
        "fresh-text",
        &[
            (1, "stdin", json!({"method": "initialize"})),
            (2, "stdin", json!({"method": "turn/start"})),
            (3, "stdout", json!({"method": "turn/started"})),
            (4, "stdout", json!({"method": "turn/completed"})),
        ],
    );

    let vocabulary = observe_surface(root.path()).unwrap().1;
    let to_provider = vocabulary
        .get(&(
            "codex".to_string(),
            "0.147.0".to_string(),
            Direction::ToProvider,
        ))
        .and_then(|paths| paths.get(".method"))
        .cloned()
        .unwrap_or_else(|| panic!("no .method vocabulary for codex to-provider: {vocabulary:?}"));
    let from_provider = vocabulary
        .get(&(
            "codex".to_string(),
            "0.147.0".to_string(),
            Direction::FromProvider,
        ))
        .and_then(|paths| paths.get(".method"))
        .cloned()
        .unwrap_or_else(|| panic!("no .method vocabulary for codex from-provider: {vocabulary:?}"));

    assert_eq!(
        to_provider,
        BTreeSet::from(["initialize".to_string(), "turn/start".to_string()]),
        "stdin methods must be collected under to-provider: {to_provider:?}"
    );
    assert_eq!(
        from_provider,
        BTreeSet::from(["turn/started".to_string(), "turn/completed".to_string()]),
        "non-stdin methods must be collected under from-provider: {from_provider:?}"
    );
}

/// Break caught (2026-08-16 correction): a declared path seen on both
/// directions must yield two separate vocabularies, not one merged set — for
/// Codex `.method`, `turn/start` (sent) and `turn/started` (received) are
/// genuinely different capabilities, and folding directions together would
/// make a driveable method indistinguishable from an emitted one.
#[test]
fn vocabulary_keeps_directions_of_the_same_path_apart() {
    let root = tempfile::tempdir().unwrap();
    write_scenario(
        root.path(),
        "claude",
        "2.1.229",
        "checklist",
        &[
            (1, "stdin", json!({"type": "user"})),
            (2, "stdout", json!({"type": "assistant"})),
        ],
    );

    let vocabulary = observe_surface(root.path()).unwrap().1;
    let to_provider = vocabulary
        .get(&(
            "claude".to_string(),
            "2.1.229".to_string(),
            Direction::ToProvider,
        ))
        .and_then(|paths| paths.get(".type"))
        .cloned()
        .unwrap_or_default();
    let from_provider = vocabulary
        .get(&(
            "claude".to_string(),
            "2.1.229".to_string(),
            Direction::FromProvider,
        ))
        .and_then(|paths| paths.get(".type"))
        .cloned()
        .unwrap_or_default();

    assert_eq!(
        to_provider,
        BTreeSet::from(["user".to_string()]),
        "the to-provider .type value must not include the from-provider one: {to_provider:?}"
    );
    assert_eq!(
        from_provider,
        BTreeSet::from(["assistant".to_string()]),
        "the from-provider .type value must not include the to-provider one: {from_provider:?}"
    );
}

/// Break caught (review finding, 2026-08-16): `.request.subtype` and
/// `.response.subtype` sit one level below where `.subtype` looks, inside
/// Claude's control-protocol envelope, so a bare `.subtype` match never sees
/// `can_use_tool` — the discriminator Comet's entire approval surface hangs
/// on. Reads the real committed corpus rather than a fixture, because the
/// point is that this evidence already existed and the declared set simply
/// hadn't caught up to it.
///
/// Both directions are asserted because the control protocol is
/// bidirectional in a way most of the corpus isn't: Claude Code opens a
/// `can_use_tool` request (received, hence `FromProvider`) and Comet opens
/// an `initialize` request (sent, hence `ToProvider`) — the same
/// `.request.subtype` path, genuinely different vocabularies per direction,
/// exactly the case direction-keying exists for.
#[test]
fn the_promoted_corpus_yields_the_control_protocol_subtypes() {
    let vocabulary = observe_surface(&corpus_root()).unwrap().1;

    let values_at = |direction: Direction, path: &str| -> BTreeSet<String> {
        vocabulary
            .get(&("claude".to_string(), "2.1.228".to_string(), direction))
            .and_then(|paths| paths.get(path))
            .cloned()
            .unwrap_or_default()
    };

    assert_eq!(
        values_at(Direction::ToProvider, ".request.subtype"),
        BTreeSet::from(["initialize".to_string()]),
        "Comet's own control_request kind"
    );
    assert_eq!(
        values_at(Direction::FromProvider, ".request.subtype"),
        BTreeSet::from(["can_use_tool".to_string()]),
        "Claude Code's control_request kind — the approval-surface discriminator"
    );
    assert_eq!(
        values_at(Direction::ToProvider, ".response.subtype"),
        BTreeSet::from(["success".to_string()]),
        "Comet's reply to Claude Code's can_use_tool request"
    );
    assert_eq!(
        values_at(Direction::FromProvider, ".response.subtype"),
        BTreeSet::from(["success".to_string()]),
        "Claude Code's reply to Comet's initialize request"
    );
}

/// D77's residual: an object at an *undeclared* path (an MCP server name, an
/// account, a machine) must block publication before it can publish its keys
/// verbatim.
///
/// Break caught: an undeclared object keyed by account ids, with no field
/// name in sight, must reject staging without ever printing an actual key.
#[test]
fn sanitize_dir_rejects_an_undeclared_object_keyed_by_account_ids_without_printing_keys() {
    let temp = tempfile::tempdir().unwrap();
    let raw = write_raw_capture(
        temp.path(),
        "undeclared-account-map",
        &[
            r#"{"type":"system","subtype":"init","accounts":{"acct-7f2c9a1d":{"plan":"pro"},"acct-04b6e2f9":{"plan":"team"}}}"#,
            "stderr sentinel",
        ],
    );
    let output = staging_dir(temp.path(), "undeclared-account-map");

    let error = sanitize_dir(&raw, &output).expect_err("D77 must block staging");
    assert!(matches!(
        error,
        SanitizationError::SuspectedUndeclaredMap {
            ref path,
            key_count: 2,
            non_identifier_count: 2,
        } if path == ".{}"
    ));
    let rendered = error.to_string();
    assert!(rendered.contains(".{}") && rendered.contains("2 of 2"));
    assert!(!rendered.contains("accounts"));
    assert!(!rendered.contains("acct-7f2c9a1d"));
    assert!(!rendered.contains("acct-04b6e2f9"));
    assert!(
        !output.exists(),
        "a rejected capture must create no staging artifact"
    );
}

/// An undeclared parent can be mixed, so its one data-shaped child may not
/// trigger D77 until a nested object does. That ancestor key is still data and
/// must not become part of the nested diagnostic path.
#[test]
fn sanitize_dir_rejects_a_nested_map_without_revealing_a_mixed_parent_data_key() {
    let temp = tempfile::tempdir().unwrap();
    let raw = write_raw_capture(
        temp.path(),
        "nested-undeclared-account-map",
        &[
            r#"{"type":"system","subtype":"init","accounts":{"primary":{},"acct-private-42":{"model-7f2c":{},"model-04b6":{}}}}"#,
            "stderr sentinel",
        ],
    );
    let output = staging_dir(temp.path(), "nested-undeclared-account-map");

    let error = sanitize_dir(&raw, &output).expect_err("D77 must block staging");
    assert!(matches!(
        error,
        SanitizationError::SuspectedUndeclaredMap {
            ref path,
            key_count: 2,
            non_identifier_count: 2,
        } if path == ".{}.{}"
    ));
    let rendered = error.to_string();
    assert!(rendered.contains(".{}.{}") && rendered.contains("2 of 2"));
    assert!(!rendered.contains("accounts"));
    assert!(!rendered.contains("acct-private-42"));
    assert!(
        !output.exists(),
        "a rejected capture must create no staging artifact"
    );
}

/// An identifier-shaped key under an undeclared object is not evidence that
/// it is a field, so the nested diagnostic must fold it as structural data.
#[test]
fn sanitize_dir_rejects_a_nested_map_without_revealing_an_identifier_shaped_account_key() {
    let temp = tempfile::tempdir().unwrap();
    let raw = write_raw_capture(
        temp.path(),
        "nested-identifier-account-map",
        &[
            r#"{"type":"system","subtype":"init","accounts":{"alice":{"model-a":{},"model-b":{}}}}"#,
            "stderr sentinel",
        ],
    );
    let output = staging_dir(temp.path(), "nested-identifier-account-map");

    let error = sanitize_dir(&raw, &output).expect_err("D77 must block staging");
    assert!(matches!(
        error,
        SanitizationError::SuspectedUndeclaredMap {
            ref path,
            key_count: 2,
            non_identifier_count: 2,
        } if path == ".{}.{}"
    ));
    let rendered = error.to_string();
    assert!(rendered.contains(".{}.{}") && rendered.contains("2 of 2"));
    assert!(!rendered.contains("accounts"));
    assert!(!rendered.contains("alice"));
    assert!(
        !output.exists(),
        "a rejected capture must create no staging artifact"
    );
}

/// Declared map keys are data even when their spelling looks like an ordinary
/// identifier, so a nested suspected map must fold an unnamed parent key.
#[test]
fn sanitize_dir_rejects_a_nested_map_without_revealing_a_declared_map_key() {
    let temp = tempfile::tempdir().unwrap();
    let raw = write_raw_capture(
        temp.path(),
        "nested-declared-model-map",
        &[
            r#"{"type":"system","subtype":"init","modelUsage":{"claude":{"model-7f2c":{},"model-04b6":{}}}}"#,
            "stderr sentinel",
        ],
    );
    let output = staging_dir(temp.path(), "nested-declared-model-map");

    let error = sanitize_dir(&raw, &output).expect_err("D77 must block staging");
    assert!(matches!(
        error,
        SanitizationError::SuspectedUndeclaredMap {
            ref path,
            key_count: 2,
            non_identifier_count: 2,
        } if path == ".modelUsage.{}"
    ));
    let rendered = error.to_string();
    assert!(rendered.contains(".modelUsage.{}") && rendered.contains("2 of 2"));
    assert!(!rendered.contains("claude"));
    assert!(
        !output.exists(),
        "a rejected capture must create no staging artifact"
    );
}

/// The false-negative-avoidance half: an object whose keys are ordinary
/// field names must still stage successfully.
#[test]
fn sanitize_dir_accepts_an_object_with_ordinary_field_names() {
    let temp = tempfile::tempdir().unwrap();
    let raw = write_raw_capture(
        temp.path(),
        "ordinary-fields",
        &[
            r#"{"type":"assistant","message":{"role":"assistant","stopReason":"end_turn"}}"#,
            "stderr sentinel",
        ],
    );
    let output = staging_dir(temp.path(), "ordinary-fields");

    sanitize_dir(&raw, &output).expect("an ordinary capture sanitizes cleanly");
    assert!(
        output.exists(),
        "ordinary field objects must remain promotable"
    );
}

/// The false-positive guard, exercised end to end rather than as a pure-function
/// unit test: a single vendor-namespaced key sitting beside four ordinary
/// siblings -- `_meta`'s own real shape, already reviewed field-by-field on
/// `allowlist/acp.txt` -- must still stage successfully. A heuristic that
/// rejects here would block every ACP sanitize run against an already-settled
/// decision, which is the "fires on everything" failure this row guards.
#[test]
fn sanitize_dir_accepts_one_vendor_key_among_ordinary_siblings() {
    let temp = tempfile::tempdir().unwrap();
    let raw = write_raw_capture(
        temp.path(),
        "meta-vendor-namespace",
        &[
            r#"{"result":{"_meta":{"x.ai/sessionConfig":{},"claudeCode":{},"steering":{},"goal":{},"jetbrains":{}}}}"#,
            "stderr sentinel",
        ],
    );
    let output = staging_dir(temp.path(), "meta-vendor-namespace");

    sanitize_dir(&raw, &output).expect("an ordinary capture sanitizes cleanly");
    assert!(
        output.exists(),
        "one vendor key among ordinary siblings must remain promotable"
    );
}
