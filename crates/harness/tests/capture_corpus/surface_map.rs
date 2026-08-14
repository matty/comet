//! The corpus walker behind the new-field gate.
//!
//! Every property here is one the gate depends on: a field must not lose its
//! direction, a data-keyed map must not become a row per key, the same key at
//! two paths must stay two fields, and a corpus the walker cannot read must be
//! an error rather than an empty answer.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use comet_harness::capture::{Direction, FieldObservation, observe_corpus};
use serde_json::{Value, json};

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

    let observations = observe_corpus(root.path()).unwrap();

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

    let observations = observe_corpus(root.path()).unwrap();
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

    let observations = observe_corpus(root.path()).unwrap();
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
    let corpus_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");
    let observations = observe_corpus(&corpus_root).unwrap();

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
        observe_corpus(root.path()).is_ok(),
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
        observe_corpus(root.path()).is_err(),
        "an unparseable stdout frame silently vanished from the inventory"
    );
}

/// Break caught: the walker silently reports an empty inventory when the
/// corpus root is wrong, which reads as "no new fields" - the most dangerous
/// possible false negative for a gate whose job is finding what arrived.
#[test]
fn an_empty_or_missing_corpus_root_is_an_error_not_an_empty_inventory() {
    let missing = tempfile::tempdir().unwrap().path().join("absent");
    assert!(observe_corpus(&missing).is_err());

    let empty = tempfile::tempdir().unwrap();
    assert!(observe_corpus(empty.path()).is_err());
}
