//! Every debt citation in the Rust sources resolves.
//!
//! Two failures this catches, and the repository has hit both:
//!
//! - **A stale path.** Nine files cited `DEBT.md` long after the record moved
//!   to `docs/debt/README.md` (D125), so a reader chasing one found nothing.
//! - **A dangling number.** The 2026-08-29 debt batch hit the inverse three
//!   times: a row deleted while code still cited it by number. `README.md`'s
//!   own rule is that a number is never reused, so a citation that resolves
//!   nowhere is always a mistake rather than a renumbering.
//!
//! Lives beside `no_runtime_cloud.rs` because that test already establishes
//! the pattern — walk the workspace from `CARGO_MANIFEST_DIR/../..` and assert
//! on source text — and neither belongs to `comet-engine` in particular.

use std::path::{Path, PathBuf};

/// Every `.rs` file under `roots`, recursively.
fn rust_sources(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack: Vec<PathBuf> = roots.to_vec();
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                // `target/` holds generated sources that cite nothing of ours,
                // and walking it is slow enough to notice.
                if path.file_name().is_some_and(|name| name == "target") {
                    continue;
                }
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                files.push(path);
            }
        }
    }
    files
}

/// Every `D<nn>` token in `text`, as written — bounded on both sides so a
/// hex literal or an identifier ending in a digit cannot masquerade as one.
fn debt_numbers(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut found = Vec::new();
    for (index, _) in text.match_indices('D') {
        let preceded_by_word = index
            .checked_sub(1)
            .is_some_and(|before| bytes[before].is_ascii_alphanumeric() || bytes[before] == b'_');
        if preceded_by_word {
            continue;
        }
        let digits: String = text[index + 1..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        if digits.is_empty() || digits.len() > 3 {
            continue;
        }
        let after = index + 1 + digits.len();
        let followed_by_word = bytes
            .get(after)
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_');
        if followed_by_word {
            continue;
        }
        found.push(format!("D{digits}"));
    }
    found
}

/// Whether the debt record names `number` anywhere — an open row in
/// `README.md`, or a closed one there or in `closed.md`.
fn record_names(record: &str, number: &str) -> bool {
    debt_numbers(record).iter().any(|found| found == number)
}

#[test]
fn every_debt_citation_in_the_sources_resolves() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let debt = workspace.join("docs/debt");
    let readme = std::fs::read_to_string(debt.join("README.md")).expect("docs/debt/README.md");
    let closed = std::fs::read_to_string(debt.join("closed.md")).expect("docs/debt/closed.md");

    let mut failures = Vec::new();

    for file in rust_sources(&[workspace.join("crates"), workspace.join("apps")]) {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        let shown = file.strip_prefix(&workspace).unwrap_or(&file).display();

        // A cited page has to exist at the path it is cited by. `D125` is what
        // happens when it does not: the number still resolved, the path did
        // not, and nothing failed.
        // Split so this scanner does not match its own needle — the same
        // trick, for the same reason, as `no_runtime_cloud.rs`'s `concat!`ed
        // forbidden literals.
        for (index, _) in text.match_indices(concat!("docs", "/debt/")) {
            let rest = &text[index..];
            let end = rest
                .find(".md")
                .map(|at| at + ".md".len())
                .unwrap_or(rest.len());
            let cited = &rest[..end];
            if !cited.ends_with(".md") {
                continue;
            }
            if !workspace.join(cited).exists() {
                failures.push(format!("{shown}: cites {cited}, which does not exist"));
            }
        }

        for number in debt_numbers(&text) {
            if !record_names(&readme, &number) && !record_names(&closed, &number) {
                failures.push(format!(
                    "{shown}: cites {number}, which is in neither docs/debt/README.md nor \
                     docs/debt/closed.md"
                ));
            }
        }
    }

    failures.sort();
    failures.dedup();
    assert!(
        failures.is_empty(),
        "{} debt citation(s) resolve to nothing. A number is never reused, so this is a \
         deleted row or a typo, never a renumbering — repoint the comment, or restore the \
         row it names:\n\n{}",
        failures.len(),
        failures.join("\n")
    );
}
