//! Task 4 of C0: the decision record behind the provider surface map.
//!
//! One entry per observed wire field, saying whether Comet reads it, has
//! decided not to, owes work on it, or has not looked. The file sits beside
//! `index.json` because it is the same kind of artifact - a hand-maintained
//! index over the corpus that a test validates - and a reviewer needs to read
//! the two together when evidence changes.
//!
//! `unknown` is a real state, not a missing one. It asserts nothing, which is
//! what lets the seed be generated rather than hand-written: a backlog of
//! honest "nobody looked" beats a backlog of rubber stamps.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use comet_harness::capture::Direction;
use serde::{Deserialize, Serialize};

pub(super) const SCHEMA_VERSION: u64 = 1;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum State {
    /// Comet reads it; `consumer` says where.
    Consumed,
    /// Nothing to build here; `reason` says why. Deliberately not debt: the
    /// debt index tracks *work* deferred, and a field null in every frame is
    /// not work owed.
    NotApplicable,
    /// Worth building, not yet. Costs a debt row and a `how` note.
    Deferred,
    /// Nobody has decided. The backlog.
    Unknown,
}

/// The record is written by the same derive that reads it, deliberately. A
/// hand-written writer beside a derived reader drops a field it was never
/// taught about, and the loss lands on the entries a person triaged by hand -
/// silently, because regeneration rewrites the whole file.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct Disposition {
    pub(super) provider: String,
    /// Sent and received are different surfaces, so the same key in each
    /// direction gets its own decision.
    pub(super) direction: Direction,
    pub(super) path: String,
    pub(super) state: State,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) consumer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) debt: Option<String>,
    /// What consuming this field would touch, and what it would cost. The
    /// difference between a catalogue and guidance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) how: Option<String>,
    /// Why a hand decision stands even though the decode sources now name this
    /// field. Records a coincidental name match (`command`, `id`, `message` are
    /// common words) so the disagreement is answered once instead of re-argued.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) derivation_note: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) domains: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct DispositionFile {
    pub(super) schema_version: u64,
    pub(super) entries: Vec<Disposition>,
}

pub(super) fn dispositions_path(crate_root: &Path) -> PathBuf {
    crate_root.join("tests/corpus/dispositions.json")
}

pub(super) fn load(path: &Path) -> Result<DispositionFile, String> {
    let bytes = std::fs::read(path)
        .map_err(|error| format!("{} could not be read: {error}", path.display()))?;
    let file: DispositionFile = serde_json::from_slice(&bytes)
        .map_err(|error| format!("{} is not a disposition file: {error}", path.display()))?;
    if file.schema_version != SCHEMA_VERSION {
        return Err(format!(
            "{} has schema_version {}, expected {SCHEMA_VERSION}",
            path.display(),
            file.schema_version
        ));
    }
    Ok(file)
}

/// Every `D<nn>` row id the debt index actually carries.
///
/// Read from the file rather than trusted from the entry, because a `deferred`
/// row pointing at a number nobody can follow is how a decision loses its
/// record - the failure `AGENTS.md` names for cited-but-unread documents.
pub(super) fn known_debt_rows(repo_root: &Path) -> Result<BTreeSet<String>, String> {
    let path = repo_root.join("docs/debt/README.md");
    let text = std::fs::read_to_string(&path)
        .map_err(|error| format!("{} could not be read: {error}", path.display()))?;
    let mut rows = BTreeSet::new();
    for line in text.lines() {
        let Some(rest) = line.strip_prefix('|') else {
            continue;
        };
        let Some(cell) = rest.split('|').next() else {
            continue;
        };
        // `| D4 |` and `| [D3](D03-slug.md) |` are both rows.
        let cell = cell.trim();
        let id = cell
            .strip_prefix('[')
            .and_then(|rest| rest.split(']').next())
            .unwrap_or(cell);
        if let Some(number) = id.strip_prefix('D')
            && !number.is_empty()
            && number.chars().all(|character| character.is_ascii_digit())
        {
            rows.insert(id.to_owned());
        }
    }
    if rows.is_empty() {
        return Err(format!("{} names no debt rows", path.display()));
    }
    Ok(rows)
}

/// The last real segment of a field path: `.a.b[].c` is `c`.
pub(super) fn leaf_name(path: &str) -> &str {
    path.rsplit('.')
        .find(|segment| !segment.is_empty() && *segment != "{}" && !segment.starts_with("[]"))
        .map(|segment| segment.trim_end_matches("[]"))
        .unwrap_or(path)
}

/// Every rule the record has to keep, as messages naming the offending entry.
pub(super) fn validate(
    file: &DispositionFile,
    observed: &BTreeSet<(String, Direction, String)>,
    debt_rows: &BTreeSet<String>,
    consumed: &BTreeSet<String>,
) -> Vec<String> {
    let mut problems = Vec::new();
    let mut seen: BTreeMap<(&str, Direction, &str), usize> = BTreeMap::new();

    for entry in &file.entries {
        let key = (
            entry.provider.as_str(),
            entry.direction,
            entry.path.as_str(),
        );
        *seen.entry(key).or_default() += 1;
        let at = format!("{} {:?} {}", entry.provider, entry.direction, entry.path);

        match entry.state {
            State::Consumed => {
                if entry.consumer.as_deref().unwrap_or_default().is_empty() {
                    problems.push(format!("{at}: consumed needs a consumer naming where"));
                }
            }
            State::NotApplicable => {
                if entry.reason.as_deref().unwrap_or_default().is_empty() {
                    problems.push(format!("{at}: not-applicable needs a reason"));
                }
                if entry.debt.is_some() {
                    problems.push(format!(
                        "{at}: not-applicable carries a debt row; if work is owed it is deferred"
                    ));
                }
            }
            State::Deferred => {
                match entry.debt.as_deref() {
                    None | Some("") => {
                        problems.push(format!("{at}: deferred needs a debt row"));
                    }
                    Some(debt) if !debt_rows.contains(debt) => {
                        problems.push(format!(
                            "{at}: debt row {debt} is not in docs/debt/README.md"
                        ));
                    }
                    Some(_) => {}
                }
                if entry.how.as_deref().unwrap_or_default().is_empty() {
                    problems.push(format!(
                        "{at}: deferred needs a how note; without it the row only repeats that the field exists"
                    ));
                }
            }
            State::Unknown => {
                if entry.debt.is_some() || entry.how.is_some() {
                    problems.push(format!(
                        "{at}: unknown carries a decision; a field somebody reasoned about is not unknown"
                    ));
                }
            }
        }

        if !observed.contains(&(entry.provider.clone(), entry.direction, entry.path.clone())) {
            problems.push(format!(
                "{at}: no promoted evidence observes this field; the record is stale"
            ));
        }

        // A hand decision wins over the derivation, but it is not allowed to
        // ignore it. A field parked as deferred or not-applicable that the
        // decode sources have since started naming is either done, or a
        // coincidental name match somebody should say so about once.
        if matches!(entry.state, State::Deferred | State::NotApplicable)
            && consumed.contains(leaf_name(&entry.path))
            && entry.derivation_note.is_none()
        {
            problems.push(format!(
                "{at}: the decode sources now name `{}` - close it, or add a derivation_note \
                 saying why the match is coincidental",
                leaf_name(&entry.path)
            ));
        }
    }

    for ((provider, direction, path), count) in seen {
        if count > 1 {
            problems.push(format!(
                "{provider} {direction:?} {path}: {count} entries for one field"
            ));
        }
    }

    problems
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observed(pairs: &[(&str, &str)]) -> BTreeSet<(String, Direction, String)> {
        pairs
            .iter()
            .map(|(provider, path)| {
                (
                    (*provider).to_owned(),
                    Direction::FromProvider,
                    (*path).to_owned(),
                )
            })
            .collect()
    }

    fn file(entries: serde_json::Value) -> DispositionFile {
        serde_json::from_value(serde_json::json!({
            "schema_version": SCHEMA_VERSION,
            "entries": entries,
        }))
        .unwrap()
    }

    fn debt() -> BTreeSet<String> {
        BTreeSet::from(["D35".to_owned(), "D36".to_owned()])
    }

    /// The keys the first entry is written with, sorted, by the same call
    /// regeneration makes.
    ///
    /// Sorted because read-back order is not ours to predict: `serde_json`'s
    /// object is a `BTreeMap` under `cargo test -p comet-harness` and an
    /// insertion-ordered map under `cargo test --workspace`, where another
    /// member unifies the `preserve_order` feature in. The question here is
    /// which fields survive the write, not what order they read in.
    fn written_keys(file: &DispositionFile) -> Vec<String> {
        let written: serde_json::Value =
            serde_json::from_slice(&serde_json::to_vec_pretty(file).unwrap()).unwrap();
        let mut keys: Vec<String> = written["entries"][0]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        keys.sort();
        keys
    }

    /// Break caught: a deferred field points at a debt row nobody can follow,
    /// and the decision loses the record that was its whole point.
    #[test]
    fn deferred_needs_a_real_debt_row_and_a_how_note() {
        let missing_row = file(serde_json::json!([{
            "provider": "codex", "direction": "from-provider", "path": ".upgrade", "state": "deferred",
            "debt": "D999", "how": "read it onto Notice",
        }]));
        let problems = validate(
            &missing_row,
            &observed(&[("codex", ".upgrade")]),
            &debt(),
            &BTreeSet::new(),
        );
        assert!(
            problems
                .iter()
                .any(|problem| problem.contains("D999 is not in docs/debt")),
            "{problems:?}"
        );

        let no_how = file(serde_json::json!([{
            "provider": "codex", "direction": "from-provider", "path": ".upgrade", "state": "deferred", "debt": "D35",
        }]));
        let problems = validate(
            &no_how,
            &observed(&[("codex", ".upgrade")]),
            &debt(),
            &BTreeSet::new(),
        );
        assert!(
            problems.iter().any(|problem| problem.contains("how note")),
            "{problems:?}"
        );
    }

    /// Break caught: `not-applicable` becomes a second debt channel, and the
    /// index that has to stay scannable fills with fields nobody owes work on.
    #[test]
    fn not_applicable_needs_a_reason_and_must_not_carry_debt() {
        let no_reason = file(serde_json::json!([{
            "provider": "claude", "direction": "from-provider", "path": ".stop_sequence", "state": "not-applicable",
        }]));
        let problems = validate(
            &no_reason,
            &observed(&[("claude", ".stop_sequence")]),
            &debt(),
            &BTreeSet::new(),
        );
        assert!(
            problems
                .iter()
                .any(|problem| problem.contains("needs a reason")),
            "{problems:?}"
        );

        let with_debt = file(serde_json::json!([{
            "provider": "claude", "direction": "from-provider", "path": ".stop_sequence", "state": "not-applicable",
            "reason": "null in every captured frame", "debt": "D35",
        }]));
        let problems = validate(
            &with_debt,
            &observed(&[("claude", ".stop_sequence")]),
            &debt(),
            &BTreeSet::new(),
        );
        assert!(
            problems
                .iter()
                .any(|problem| problem.contains("if work is owed it is deferred")),
            "{problems:?}"
        );
    }

    /// Break caught: `unknown` is used as a parking space for decisions, so
    /// the backlog stops meaning "nobody looked" and the seed stops being
    /// honest.
    #[test]
    fn unknown_may_not_carry_a_decision() {
        let reasoned = file(serde_json::json!([{
            "provider": "claude", "direction": "from-provider", "path": ".ttft_ms", "state": "unknown",
            "how": "we thought about this a lot",
        }]));
        let problems = validate(
            &reasoned,
            &observed(&[("claude", ".ttft_ms")]),
            &debt(),
            &BTreeSet::new(),
        );
        assert!(
            problems
                .iter()
                .any(|problem| problem.contains("not unknown")),
            "{problems:?}"
        );
    }

    /// Break caught: a consumed claim with nowhere to check it, which is how a
    /// field gets marked read when only a name matched.
    #[test]
    fn consumed_needs_a_consumer() {
        let bare = file(serde_json::json!([{
            "provider": "claude", "direction": "from-provider", "path": ".session_id", "state": "consumed",
        }]));
        let problems = validate(
            &bare,
            &observed(&[("claude", ".session_id")]),
            &debt(),
            &BTreeSet::new(),
        );
        assert!(
            problems
                .iter()
                .any(|problem| problem.contains("naming where")),
            "{problems:?}"
        );
    }

    /// Break caught: the record keeps rows for evidence that no longer exists,
    /// so a scenario removal leaves decisions about fields nothing observes.
    #[test]
    fn an_entry_with_no_evidence_is_stale() {
        let stale = file(serde_json::json!([{
            "provider": "claude", "direction": "from-provider", "path": ".gone", "state": "unknown",
        }]));
        let problems = validate(
            &stale,
            &observed(&[("claude", ".present")]),
            &debt(),
            &BTreeSet::new(),
        );
        assert!(
            problems.iter().any(|problem| problem.contains("stale")),
            "{problems:?}"
        );
    }

    /// Break caught: two entries disagree about one field and whichever is read
    /// first wins silently.
    #[test]
    fn one_field_gets_one_entry() {
        let twice = file(serde_json::json!([
            {"provider": "claude", "direction": "from-provider", "path": ".x", "state": "unknown"},
            {"provider": "claude", "direction": "from-provider", "path": ".x", "state": "unknown"},
        ]));
        let problems = validate(
            &twice,
            &observed(&[("claude", ".x")]),
            &debt(),
            &BTreeSet::new(),
        );
        assert!(
            problems
                .iter()
                .any(|problem| problem.contains("2 entries for one field")),
            "{problems:?}"
        );
    }

    /// Break caught: regeneration drops a field somebody triaged by hand.
    /// `COMET_UPDATE_SURFACE=1` rewrites the whole record, so a writer that does
    /// not know about a field erases it from every entry at once - and the
    /// entries that lose the most are the ones a person wrote.
    #[test]
    fn a_written_entry_carries_every_field_it_was_given() {
        let full = file(serde_json::json!([{
            "provider": "codex", "direction": "from-provider", "path": ".upgrade",
            "state": "deferred", "consumer": "codex/normalize.rs:209", "reason": "not yet",
            "debt": "D35", "how": "read it onto AgentEvent::Notice",
            "derivation_note": "`upgrade` is a common word", "domains": ["notices"],
        }]));

        assert_eq!(
            written_keys(&full),
            [
                "consumer",
                "debt",
                "derivation_note",
                "direction",
                "domains",
                "how",
                "path",
                "provider",
                "reason",
                "state",
            ]
        );

        let reread: DispositionFile =
            serde_json::from_slice(&serde_json::to_vec_pretty(&full).unwrap()).unwrap();
        assert_eq!(reread.entries[0].debt.as_deref(), Some("D35"));
        assert_eq!(reread.entries[0].domains, ["notices"]);

        // The absent ones stay absent, so a seeded backlog entry reads as four
        // lines rather than ten mostly-null ones.
        let bare = file(serde_json::json!([{
            "provider": "claude", "direction": "from-provider", "path": ".ttft_ms", "state": "unknown",
        }]));
        assert_eq!(
            written_keys(&bare),
            ["direction", "path", "provider", "state"]
        );
    }

    /// Break caught: the debt reader stops finding rows - every deferred entry
    /// then fails, or worse, every one passes.
    #[test]
    fn the_real_debt_index_yields_its_rows() {
        let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        let rows = known_debt_rows(&repo_root).unwrap();
        assert!(rows.len() > 50, "{} rows", rows.len());
        for known in ["D35", "D36", "D63", "D71"] {
            assert!(rows.contains(known), "{known} missing from {}", rows.len());
        }
        assert!(known_debt_rows(Path::new("does-not-exist")).is_err());
    }
}
