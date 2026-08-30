//! Guards the boundary D63 draws between an exploratory capture and promoted corpus evidence.
//!
//! `docs/testing/provider-captures.md`'s "Exploratory captures" section describes the procedure;
//! this file is what stops the procedure drifting. Three invariants, each pinned by its own
//! test: the destination exists and is not the corpus itself; nothing in the promoted corpus
//! carries the exploratory marker (an entry mistakenly copied wholesale would be caught by
//! name); and any exploratory entry that keeps evidence alongside its write-up carries the
//! marker in the same directory as that evidence, not merely somewhere in the tree.
//!
//! The marker-scan logic is exercised against synthetic temp directories, never against this
//! crate's real `tests/corpus/` — deliberately, since this row's task is barred from writing
//! anything under `tests/corpus/`, even a file created and removed within one test run.

use std::path::{Path, PathBuf};

use comet_capture::{EXPLORATORY_MARKER_FILENAME, corpus_root, exploratory_root};

#[test]
fn exploratory_root_exists_and_is_not_nested_with_the_corpus() {
    let exploratory = exploratory_root();
    let corpus = corpus_root();
    assert!(
        exploratory.is_dir(),
        "{} must exist as the exploratory destination D63 names",
        exploratory.display()
    );
    assert_ne!(
        exploratory, corpus,
        "the exploratory destination must be a different directory than the corpus"
    );
    assert!(
        !exploratory.starts_with(&corpus),
        "exploratory must not live inside the corpus ({} is under {})",
        exploratory.display(),
        corpus.display()
    );
    assert!(
        !corpus.starts_with(&exploratory),
        "the corpus must not live inside exploratory ({} is under {})",
        corpus.display(),
        exploratory.display()
    );
}

/// The real corpus never carries the exploratory marker anywhere in it. Proves the negative over
/// the real tree; [`marker_scan_tests`] below proves the scan itself would catch one if it were
/// there, using a synthetic tree instead of touching `tests/corpus/`.
#[test]
fn no_promoted_corpus_directory_carries_the_exploratory_marker() {
    let found = find_marker_files(&corpus_root());
    assert!(
        found.is_empty(),
        "promoted corpus director{} carr{} the exploratory marker, so {} would no longer be \
         unmistakable for evidence (D63):\n{}",
        if found.len() == 1 { "y" } else { "ies" },
        if found.len() == 1 { "ies" } else { "y" },
        EXPLORATORY_MARKER_FILENAME,
        found
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// Every exploratory entry that keeps `events.jsonl` beside its write-up must carry the marker
/// in that same directory — the guard the README's step 3 documents. Currently vacuous (no
/// entry has been added yet), which is correct: the point is that the check is live for the
/// first one that is.
#[test]
fn every_exploratory_entry_with_events_carries_its_own_marker() {
    let mut missing = Vec::new();
    find_unmarked_evidence_dirs(&exploratory_root(), &mut missing);
    assert!(
        missing.is_empty(),
        "exploratory director{} under {} hold events.jsonl with no {} beside it:\n{}",
        if missing.len() == 1 { "y" } else { "ies" },
        exploratory_root().display(),
        EXPLORATORY_MARKER_FILENAME,
        missing
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// Every file named [`EXPLORATORY_MARKER_FILENAME`] anywhere under `root`, found by a plain
/// recursive descent. An unreadable subtree is skipped rather than propagated as an error —
/// unlike `corpus::promoted_scenarios`'s walk, a missing or locked directory here just means
/// nothing to report from it, not a corpus read that silently dropped evidence.
fn find_marker_files(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    walk(root, &mut |path| {
        if path.file_name().and_then(|name| name.to_str()) == Some(EXPLORATORY_MARKER_FILENAME) {
            found.push(path.to_path_buf());
        }
    });
    found
}

/// Every directory under `root` that holds `events.jsonl` but not
/// [`EXPLORATORY_MARKER_FILENAME`] beside it, appended to `missing`.
fn find_unmarked_evidence_dirs(root: &Path, missing: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if path.join("events.jsonl").is_file() && !path.join(EXPLORATORY_MARKER_FILENAME).is_file()
        {
            missing.push(path.clone());
        }
        find_unmarked_evidence_dirs(&path, missing);
    }
}

/// Recursive descent from `root`, calling `visit` with every file path found. Directories that
/// cannot be read are silently skipped — see [`find_marker_files`]'s own doc comment for why
/// that is the right contract for this scan specifically.
fn walk(root: &Path, visit: &mut impl FnMut(&Path)) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, visit);
        } else {
            visit(&path);
        }
    }
}

#[cfg(test)]
mod marker_scan_tests {
    //! Falsifies the two scans above against a synthetic tree, never the real corpus.

    use super::*;

    #[test]
    fn finds_a_marker_planted_in_a_synthetic_tree() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("claude/1.0.0/some-scenario")).unwrap();
        std::fs::write(
            dir.path()
                .join("claude/1.0.0/some-scenario")
                .join(EXPLORATORY_MARKER_FILENAME),
            "exploratory, not evidence",
        )
        .unwrap();
        let found = find_marker_files(dir.path());
        assert_eq!(found.len(), 1);
        assert!(found[0].ends_with(EXPLORATORY_MARKER_FILENAME));
    }

    #[test]
    fn reports_nothing_over_a_tree_with_no_marker() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("claude/1.0.0/some-scenario")).unwrap();
        std::fs::write(
            dir.path()
                .join("claude/1.0.0/some-scenario")
                .join("events.jsonl"),
            "{}",
        )
        .unwrap();
        assert!(find_marker_files(dir.path()).is_empty());
    }

    #[test]
    fn flags_evidence_with_no_marker_beside_it() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("unmarked-finding")).unwrap();
        std::fs::write(
            dir.path().join("unmarked-finding").join("events.jsonl"),
            "{}",
        )
        .unwrap();
        let mut missing = Vec::new();
        find_unmarked_evidence_dirs(dir.path(), &mut missing);
        assert_eq!(missing.len(), 1);
        assert!(missing[0].ends_with("unmarked-finding"));
    }

    #[test]
    fn does_not_flag_evidence_with_its_marker_beside_it() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("marked-finding")).unwrap();
        std::fs::write(dir.path().join("marked-finding").join("events.jsonl"), "{}").unwrap();
        std::fs::write(
            dir.path()
                .join("marked-finding")
                .join(EXPLORATORY_MARKER_FILENAME),
            "exploratory, not evidence",
        )
        .unwrap();
        let mut missing = Vec::new();
        find_unmarked_evidence_dirs(dir.path(), &mut missing);
        assert!(missing.is_empty());
    }
}
