//! The new-field gate.
//!
//! One committed snapshot of every field the promoted corpus shows, so a newly
//! promoted capture — or a new CLI version's added field — arrives as a test
//! failure rather than as something nobody noticed. That is the whole job.
//!
//! It deliberately records **no decision** about a field. An earlier version
//! carried a four-state disposition per field with a validator, a debt
//! cross-check and two generated reports; of 655 entries, 7 held a human
//! judgement and all 281 "consumed" markings came from matching the field's leaf
//! name against the decode sources, which counted `.message.diagnostics.
//! cache_miss_reason.type` as read because something, somewhere, names `type`.
//! What survived is the part that could fail for a real reason.
//!
//! **Its blind spot is absence.** It reports fields that are present; a
//! capability no capture ever exercised cannot appear here at all.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use comet_harness::capture::{observe_corpus, observed_field_lines};

fn corpus_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus")
}

fn snapshot_path() -> PathBuf {
    corpus_root().join("observed-fields.json")
}

/// Break caught: a field arrives that the snapshot does not name — a new CLI
/// version adding one, or a newly promoted capture — and it lands silently
/// instead of as a test failure.
#[test]
fn the_corpus_shows_exactly_the_fields_the_snapshot_names() {
    let observations = observe_corpus(&corpus_root()).unwrap();
    let observed = observed_field_lines(&observations);

    if std::env::var_os("COMET_UPDATE_SURFACE").is_some() {
        let mut bytes = serde_json::to_vec_pretty(&observed).unwrap();
        bytes.push(b'\n');
        std::fs::write(snapshot_path(), bytes).unwrap();
    }

    let bytes = std::fs::read(snapshot_path()).unwrap_or_else(|error| {
        panic!(
            "{} could not be read ({error}); rerun with COMET_UPDATE_SURFACE=1 to seed it",
            snapshot_path().display()
        )
    });
    let recorded: BTreeSet<String> = serde_json::from_slice(&bytes).unwrap();

    // Both directions are reported, and an arrival carries the frame it was
    // first seen in so triage starts there rather than at a grep.
    let first_seen: std::collections::BTreeMap<String, String> = observations
        .iter()
        .map(|observation| {
            let line = observed_field_lines(std::slice::from_ref(observation))
                .into_iter()
                .next()
                .expect("one observation yields one line");
            let where_ = format!(
                "{} #{}",
                observation.first_seen.scenario, observation.first_seen.sequence
            );
            (line, where_)
        })
        .collect();
    let arrived: Vec<String> = observed
        .difference(&recorded)
        .map(|line| format!("{line} (first seen {})", first_seen[line]))
        .collect();
    let gone: Vec<&String> = recorded.difference(&observed).collect();

    assert!(
        arrived.is_empty() && gone.is_empty(),
        "the corpus no longer matches its field snapshot. Read what changed, then rerun with \
         COMET_UPDATE_SURFACE=1.\n{} field(s) arrived:\n  {}\n{} field(s) went away:\n  {}",
        arrived.len(),
        arrived.join("\n  "),
        gone.len(),
        gone.iter()
            .map(|line| line.as_str())
            .collect::<Vec<_>>()
            .join("\n  ")
    );
}
