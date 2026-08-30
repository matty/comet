//! Every row in the debt record has as many cells as its table has columns.
//!
//! A five-cell row in a four-column table is not a rendering nit. GitHub drops
//! the extra cells, so a state appended as a *new* cell instead of replacing
//! the old one is invisible on the rendered page, and the superseded state is
//! what a reader sees. It has happened twice, both found on 2026-08-30:
//!
//! - **D89** sat in the Closed table showing `☐ open, low priority, unowned`
//!   while its `☑ fixed the same day` update — including the correction that
//!   the row's own "16/16 idle" figure was wrong — hid in a fifth cell.
//! - **D53** showed "the subagent surface itself is not built" for four days
//!   after slice 4.4 built it, long enough for a session reading the index to
//!   recommend re-planning a slice that had already shipped.
//!
//! Grep finds the hidden cell; the rendered page does not, and the rendered
//! page is what the index is read on.
//!
//! Lives beside `debt_citations.rs`, which walks the same record for the same
//! reason and whose module doc explains why these tests are here rather than
//! in `comet-engine` proper.

use std::path::PathBuf;

/// The cells of one markdown table row, honouring `\|` as an escaped pipe
/// rather than a cell boundary — several rows carry one inside prose.
///
/// A row is delimited by a leading and a trailing pipe, so the first and last
/// pieces are empty padding and not cells.
fn cells(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut escaped = false;
    for ch in line.chars() {
        if escaped {
            cur.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            '\\' => {
                escaped = true;
                cur.push(ch);
            }
            '|' => out.push(std::mem::take(&mut cur)),
            _ => cur.push(ch),
        }
    }
    out.push(cur);
    out
}

fn debt_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/debt")
}

#[test]
fn every_debt_table_row_has_its_table_s_column_count() {
    let mut failures = Vec::new();

    for name in ["README.md", "closed.md"] {
        let path = debt_dir().join(name);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("read {}: {err}", path.display()));

        // A table's column count is set by its first row and holds until a
        // non-row line ends it, so a second table in the same file is free to
        // have a different width.
        let mut columns: Option<usize> = None;
        for (index, line) in text.lines().enumerate() {
            if !line.starts_with('|') {
                columns = None;
                continue;
            }
            let count = cells(line).len().saturating_sub(2);
            match columns {
                None => columns = Some(count),
                Some(expected) if count != expected => failures.push(format!(
                    "docs/debt/{name}:{} has {count} cells, its table has {expected}: {}",
                    index + 1,
                    line.chars().take(90).collect::<String>()
                )),
                Some(_) => {}
            }
        }
    }

    assert!(
        failures.is_empty(),
        "debt table rows whose cell count does not match their table.\n\
         An extra cell is DROPPED when rendered, hiding whatever it holds — \
         replace the stale cell instead of appending beside it.\n{}",
        failures.join("\n")
    );
}
