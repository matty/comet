//! Guard: every provenance citation under `comet-harness`'s `tests/fixtures/`
//! names a corpus path that exists.
//!
//! A fixture may cite the capture a hand-typed literal was shaped from. Six
//! such citations named `run2-claude-subagent.jsonl`, a raw capture that was
//! never committed to this repository — corrected in #80 to name the real,
//! checked-in corpus instead. This test is the mechanical version of that
//! correction: it catches a citation nobody can follow. It does **not**
//! verify a cited literal still *matches* the frame it names — only that the
//! path exists — so drift between a hand-typed literal and its cited frame
//! passes this test silently.
//!
//! Citations follow one convention, backtick-quoted and prefixed with
//! `tests/corpus/`, e.g. `` `tests/corpus/claude/2.1.229/subagent` `` — the
//! same relative path `comet_capture::corpus_root()` resolves from
//! this crate's manifest directory.
//!
//! The citing fixtures (`fake_claude.rs`, `fake_codex.rs`) live in a
//! different crate, `comet-harness`: they are scripted fake CLIs spawned
//! through `CARGO_BIN_EXE_*` by harness's own adapter integration suites, not
//! capture-only tooling, so they did not move here with `capture/`. This test
//! scans that sibling crate's `tests/fixtures/` for citations and checks each
//! one against **this** crate's own `tests/corpus/`, which is where the cited
//! frame now actually lives.

use std::fs;
use std::path::Path;

#[test]
fn every_fixture_provenance_citation_names_a_corpus_path_that_exists() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let fixtures_dir = manifest_dir
        .parent()
        .unwrap_or_else(|| panic!("{} has no parent directory", manifest_dir.display()))
        .join("harness")
        .join("tests")
        .join("fixtures");

    let mut checked = 0usize;
    let mut missing = Vec::new();

    for entry in fs::read_dir(&fixtures_dir)
        .unwrap_or_else(|error| panic!("read {}: {error}", fixtures_dir.display()))
    {
        let path = entry.expect("fixture dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let text = fs::read_to_string(&path).expect("read fixture file");
        for citation in citations(&text) {
            checked += 1;
            if !manifest_dir.join(&citation).is_dir() {
                missing.push(format!("{}: cites `{citation}`", path.display()));
            }
        }
    }

    assert!(
        checked > 0,
        "found no provenance citations to check — the convention may have drifted out from under this test's `tests/corpus/` prefix match"
    );
    assert!(
        missing.is_empty(),
        "provenance citation(s) name a corpus path nobody can open:\n{}",
        missing.join("\n")
    );
}

/// Every backtick-quoted `tests/corpus/...` span in `text`.
fn citations(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(rel) = text[from..].find("`tests/corpus/") {
        let start = from + rel + 1; // past the opening backtick
        let Some(rel_end) = text[start..].find('`') else {
            break;
        };
        out.push(text[start..start + rel_end].to_string());
        from = start + rel_end + 1;
    }
    out
}
