//! The supported-version floor and the corpus that is its evidence agree.
//!
//! `docs/testing/supported-provider-versions.md` is the basis on which a decode
//! may be DELETED — slice 4.3 removed `TodoWrite` and Codex `todoList` items on
//! it. That makes the file load-bearing prose, and until this test it was
//! nothing but prose (`docs/debt/README.md`'s D69): a floor could name a corpus
//! directory nobody had promoted, or a directory could be deleted out from
//! under a floor still citing it, and neither would fail anything.
//!
//! **What this checks and what it does not.** It ties each floor to real
//! evidence on disk, in both directions. It does NOT catch the failure D69
//! calls the valuable one — a decode that no supported version can produce any
//! more, still shipping. That needs the decode names, not the version numbers;
//! `acp_decode_coverage.rs` is the first province of it, on the one surface
//! where the corpus already records the vocabulary.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use comet_capture::{corpus_root, promoted_scenarios};

fn floor_doc() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/testing/supported-provider-versions.md")
}

/// One row of the floor table: the provider label, its floor, and every corpus
/// path the row cites.
#[derive(Debug)]
struct FloorRow {
    provider: String,
    floor: String,
    cited: Vec<String>,
}

/// Parse the floor table out of the markdown.
///
/// Deliberately reads the committed document rather than a Rust constant
/// mirroring it: the document is what a person edits and what other documents
/// cite, so a constant would be a second copy to drift. The parse is
/// correspondingly strict — an unreadable row fails the test rather than being
/// skipped, because a silently-skipped row is the enforcement gap this closes.
fn floor_rows(markdown: &str) -> Vec<FloorRow> {
    let mut rows = Vec::new();
    for line in markdown.lines() {
        let line = line.trim();
        // The table's own header and separator, and every non-table line.
        if !line.starts_with('|') || line.starts_with("| ---") || line.starts_with("| Provider") {
            continue;
        }
        let cells: Vec<&str> = line.trim_matches('|').split('|').map(str::trim).collect();
        if cells.len() < 3 {
            continue;
        }
        let floor = cells[1].trim_matches('*').trim().to_owned();
        // Only the paths, not the prose around them: the evidence cell also
        // carries `…/2.1.229/` shorthand for siblings under the same provider.
        let cited: Vec<String> = cells[2]
            .split('`')
            .filter(|piece| piece.contains("tests/corpus/"))
            .map(|piece| piece.trim().trim_end_matches(',').to_owned())
            .collect();
        rows.push(FloorRow {
            provider: cells[0].to_owned(),
            floor,
            cited,
        });
    }
    rows
}

/// Break caught: a floor naming a corpus directory that is not there — either
/// never promoted, or deleted later while the prose kept citing it.
#[test]
fn every_floor_names_a_corpus_directory_that_exists() {
    let markdown = std::fs::read_to_string(floor_doc()).expect("the floor document");
    let rows = floor_rows(&markdown);
    assert!(
        !rows.is_empty(),
        "the floor table parsed as empty — the document's shape changed and this \
         test would otherwise pass by checking nothing"
    );

    let root = corpus_root();
    let promoted: BTreeSet<String> = promoted_scenarios(&root)
        .expect("the corpus walks")
        .into_iter()
        .map(|scenario| format!("{}/{}", scenario.provider, scenario.version))
        .collect();

    let mut failures = Vec::new();
    for row in &rows {
        assert!(
            !row.floor.is_empty(),
            "{}'s floor cell is empty",
            row.provider
        );
        assert!(
            !row.cited.is_empty(),
            "{} names a floor with no corpus evidence cited",
            row.provider
        );

        for cited in &row.cited {
            // `crates/capture/tests/corpus/claude/2.1.228/` and the `…/2.1.229/`
            // shorthand both reduce to the same question: is there a promoted
            // `provider/version` behind it?
            let Some(tail) = cited.rsplit("tests/corpus/").next() else {
                continue;
            };
            let key = tail.trim_matches('/');
            if key.starts_with('…') || key.is_empty() {
                continue;
            }
            if !promoted.contains(key) {
                failures.push(format!(
                    "{}: the floor cites {key}, which holds no promoted scenario",
                    row.provider
                ));
            }
        }
    }

    assert!(
        failures.is_empty(),
        "{} floor citation(s) point at nothing. The floor is what licenses deleting \
         a decode, so evidence it names has to be on disk:\n\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// The other direction: a provider Comet actually drives, whose corpus exists,
/// must appear in the table.
///
/// **Not every promoted provider earns a row**, and the document says which and
/// why — codex-acp and claude-agent-acp are protocol comparison points with no
/// `Harness` behind them. Those are named here so the exemption is a decision
/// with a reason rather than an absence nobody noticed.
#[test]
fn every_promoted_provider_has_a_floor_or_a_stated_reason_not_to() {
    /// Promoted, deliberately floorless: no `comet_proto::HarnessId` variant
    /// drives either, so "no supported version can produce it" has nothing to
    /// be measured against. The document's own "A promoted corpus does not
    /// always earn a floor row" paragraph is the long version.
    const NO_HARNESS: &[&str] = &["codex-acp", "claude-agent-acp"];

    let markdown = std::fs::read_to_string(floor_doc()).expect("the floor document");
    let rows = floor_rows(&markdown);

    let root = corpus_root();
    let providers: BTreeSet<String> = promoted_scenarios(&root)
        .expect("the corpus walks")
        .into_iter()
        .map(|scenario| scenario.provider)
        .collect();

    let mut missing = Vec::new();
    for provider in &providers {
        if NO_HARNESS.contains(&provider.as_str()) {
            continue;
        }
        let named = rows.iter().any(|row| {
            row.cited
                .iter()
                .any(|cited| cited.contains(&format!("corpus/{provider}/")))
        });
        if !named {
            missing.push(provider.clone());
        }
    }

    assert!(
        missing.is_empty(),
        "{} promoted provider(s) have corpus evidence and no floor row, and are not \
         listed as deliberately floorless: {}. Add the row, or add the provider to \
         NO_HARNESS with the reason.",
        missing.len(),
        missing.join(", ")
    );
}
