//! D70: the corpus has a stated coverage rule, and the "latest" half of it is
//! checked against disk.
//!
//! `docs/testing/supported-provider-versions.md`'s "Latest promoted" column is meant to name the
//! newest version each provider's corpus holds evidence for, kept current in the same PR that
//! promotes a new capture. Before this test, nothing checked that — `claude/2.1.251/` was
//! promoted in [PR #190](https://github.com/matty/comet/pull/190) and nothing in the repository
//! said so for four more commits, because nothing compared the doc to the corpus. This is the
//! sibling of `version_floor.rs`'s two checks, for the column that names, not the column that
//! licenses deletion.
//!
//! **What this does not check.** Retirement — whether a version other than the floor or the
//! latest may be deleted — turns on whether any decode's only evidence is unique to that one
//! version, which is a per-version question the existing decode-coverage lints
//! (`claude_tool_coverage.rs`, `codex_method_coverage.rs`, `acp_decode_coverage.rs`) do not
//! answer; they show whether a decode has any evidence in the corpus, not which version supplied
//! it. `docs/testing/provider-captures.md`'s "Corpus coverage and retirement" section says this
//! plainly and leaves that judgment to whoever writes the retirement PR.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use comet_capture::{corpus_root, promoted_scenarios};

fn floor_doc() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/testing/supported-provider-versions.md")
}

/// One row of the floor table, the two columns this test cares about: the corpus-evidence cell
/// (to recover the on-disk provider folder name, the same way `version_floor.rs` does) and the
/// documented "Latest promoted" cell.
struct CoverageRow {
    provider: String,
    corpus_folder: String,
    documented_latest: String,
}

/// Parse the floor table's rows, deriving the on-disk provider folder (`claude`, `codex`, `grok`,
/// …) from the first `tests/corpus/<folder>/...` path the evidence cell cites, and reading the
/// fourth cell as the documented latest version.
///
/// Reads the committed document rather than a Rust constant for the same reason
/// `version_floor.rs` does: the document is what a person edits, so a constant would be a second
/// copy to drift silently.
fn coverage_rows(markdown: &str) -> Vec<CoverageRow> {
    let mut rows = Vec::new();
    for line in markdown.lines() {
        let line = line.trim();
        if !line.starts_with('|') || line.starts_with("| ---") || line.starts_with("| Provider") {
            continue;
        }
        let cells: Vec<&str> = line.trim_matches('|').split('|').map(str::trim).collect();
        if cells.len() < 4 {
            // A row from some other table in this file, or the table's shape changed under this
            // parse without the parse changing with it — either way, not a coverage row.
            continue;
        }
        let provider = cells[0].to_owned();
        let documented_latest = cells[3].trim_matches('*').trim().to_owned();

        // The evidence cell carries one or more backticked `crates/capture/tests/corpus/<folder>/
        // <version>/` paths (plus `…/<version>/` shorthand for siblings); the folder segment right
        // after `tests/corpus/` is the on-disk provider name `promoted_scenarios` returns.
        let corpus_folder = cells[2]
            .split('`')
            .filter(|piece| piece.contains("tests/corpus/"))
            .find_map(|piece| {
                let tail = piece.rsplit("tests/corpus/").next()?;
                tail.trim_matches('/').split('/').next().map(str::to_owned)
            });

        let Some(corpus_folder) = corpus_folder else {
            // A row naming no corpus folder at all (none of today's rows) has nothing this test
            // can check disk against; skip rather than guess.
            continue;
        };

        rows.push(CoverageRow {
            provider,
            corpus_folder,
            documented_latest,
        });
    }
    rows
}

/// `"2.1.251"` -> `[2, 1, 251]`, so version ordering compares numerically instead of
/// lexicographically (`"2.1.9" < "2.1.10"` would fail as strings). Every version in the corpus
/// today is a dotted run of integers; a component that fails to parse sorts as `0`, which only
/// ever matters for a malformed string this test would rather still compare than panic on.
fn version_key(version: &str) -> Vec<u64> {
    version
        .split('.')
        .map(|part| part.parse().unwrap_or(0))
        .collect()
}

/// Break caught: a version promoted to the corpus with no update to the "Latest promoted"
/// column — the exact gap `claude/2.1.251/` sat in for four commits before this test existed.
#[test]
fn latest_promoted_column_matches_the_newest_version_on_disk() {
    let markdown = std::fs::read_to_string(floor_doc()).expect("the floor document");
    let rows = coverage_rows(&markdown);
    assert!(
        !rows.is_empty(),
        "the floor table parsed as empty — the document's shape changed and this test would \
         otherwise pass by checking nothing"
    );

    let root = corpus_root();
    let scenarios = promoted_scenarios(&root).expect("the corpus walks");

    let mut failures = Vec::new();
    for row in &rows {
        assert!(
            !row.documented_latest.is_empty(),
            "{}'s Latest promoted cell is empty",
            row.provider
        );

        let versions: BTreeSet<String> = scenarios
            .iter()
            .filter(|scenario| scenario.provider == row.corpus_folder)
            .map(|scenario| scenario.version.clone())
            .collect();
        let Some(actual_latest) = versions.iter().max_by_key(|version| version_key(version)) else {
            failures.push(format!(
                "{}: no promoted scenario exists under corpus/{}/ at all, so there is nothing to \
                 compare its documented latest ({}) against",
                row.provider, row.corpus_folder, row.documented_latest
            ));
            continue;
        };

        if actual_latest != &row.documented_latest {
            failures.push(format!(
                "{}: the table names {} as latest, but corpus/{}/ holds {} as its newest \
                 promoted version — update the Latest promoted column in the same change that \
                 promoted it",
                row.provider, row.documented_latest, row.corpus_folder, actual_latest
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "{} coverage mismatch(es). The Latest promoted column is meant to name the newest \
         version the corpus holds evidence for, updated in the same PR that promotes it:\n\n{}",
        failures.len(),
        failures.join("\n")
    );
}
