//! Tasks 5 and 6 of C0: the join, the rendered report, and the gate.
//!
//! The report is the artifact someone reads when deciding what to build on a
//! provider. It is generated and committed, so it diffs in review; the gate
//! below fails when the committed copy has drifted, and when a field arrives
//! that no disposition covers.
//!
//! Regenerate both the report and the disposition seed with:
//!
//! ```powershell
//! $env:COMET_UPDATE_SURFACE = "1"; cargo test -p comet-harness --test capture_corpus surface_report
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use comet_harness::capture::{Direction, FieldObservation, observe_corpus};

use super::consumed::{consumed_fields, decode_sources};
use super::dispositions::{
    Disposition, DispositionFile, SCHEMA_VERSION, State, dispositions_path, known_debt_rows,
    leaf_name, load, validate,
};

/// Consumers written by the seed rather than by a person. Named so the report
/// can say so: a name match proves something mentions the field, never that a
/// value reaches a user.
const DERIVED_CONSUMER: &str = "derived: named in the decode sources";

fn crate_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn repo_root() -> PathBuf {
    crate_root().join("../..").canonicalize().unwrap()
}

fn corpus_root() -> PathBuf {
    crate_root().join("tests/corpus")
}

fn report_path(repo_root: &Path, provider: &str) -> PathBuf {
    repo_root
        .join("docs/testing/provider-surface")
        .join(format!("{provider}.md"))
}

fn updating() -> bool {
    std::env::var_os("COMET_UPDATE_SURFACE").is_some()
}

/// Adopting fields the corpus has never shown before, which is deliberately a
/// second key: see the arrivals check in the regeneration test.
fn adopting() -> bool {
    std::env::var_os("COMET_ADOPT_FIELDS").is_some()
}

fn observed_keys(observations: &[FieldObservation]) -> BTreeSet<(String, Direction, String)> {
    observations
        .iter()
        .map(|observation| {
            (
                observation.provider.clone(),
                observation.direction,
                observation.path.clone(),
            )
        })
        .collect()
}

/// The seed: every observed field, `consumed` where the decode sources name it
/// and `unknown` otherwise.
///
/// Generated rather than hand-written on purpose. `unknown` asserts nothing, so
/// seeding it is not a rubber stamp; hand-writing 350 entries would be.
fn seed(observations: &[FieldObservation], consumed: &BTreeSet<String>) -> DispositionFile {
    let mut entries: Vec<Disposition> = observations
        .iter()
        .map(|observation| {
            let named = consumed.contains(leaf_name(&observation.path));
            Disposition {
                provider: observation.provider.clone(),
                direction: observation.direction,
                path: observation.path.clone(),
                state: if named {
                    State::Consumed
                } else {
                    State::Unknown
                },
                consumer: named.then(|| DERIVED_CONSUMER.to_owned()),
                reason: None,
                debt: None,
                how: None,
                derivation_note: None,
                domains: Vec::new(),
            }
        })
        .collect();
    entries.sort_by(|left, right| {
        (&left.provider, left.direction, &left.path).cmp(&(
            &right.provider,
            right.direction,
            &right.path,
        ))
    });
    DispositionFile {
        schema_version: SCHEMA_VERSION,
        entries,
    }
}

/// Keeps a hand-made decision and re-seeds only what is new, so regenerating
/// never silently discards triage.
fn merge(existing: &DispositionFile, fresh: DispositionFile) -> DispositionFile {
    let kept: BTreeMap<(String, Direction, String), &Disposition> = existing
        .entries
        .iter()
        .filter(|entry| !matches!(entry.state, State::Unknown))
        .map(|entry| {
            (
                (entry.provider.clone(), entry.direction, entry.path.clone()),
                entry,
            )
        })
        .collect();
    DispositionFile {
        schema_version: SCHEMA_VERSION,
        entries: fresh
            .entries
            .into_iter()
            .map(|entry| {
                kept.get(&(entry.provider.clone(), entry.direction, entry.path.clone()))
                    .map_or(entry, |held| (*held).clone())
            })
            .collect(),
    }
}

fn values_cell(observation: &FieldObservation) -> String {
    let mut parts = Vec::new();
    if !observation.values.redaction_kinds.is_empty() {
        let kinds: Vec<&str> = observation
            .values
            .redaction_kinds
            .iter()
            .map(String::as_str)
            .collect();
        parts.push(format!("redacted: {}", kinds.join(", ")));
    }
    if !observation.values.literals.is_empty() {
        let literals: Vec<String> = observation
            .values
            .literals
            .iter()
            .map(|literal| format!("`{literal}`"))
            .collect();
        // Escaped: a bare pipe would split the markdown column it sits in.
        parts.push(literals.join(" \\| "));
    }
    if observation.values.truncated {
        parts.push("(more)".to_owned());
    }
    if observation.values.withheld {
        parts.push("_withheld_".to_owned());
    }
    if parts.is_empty() {
        "-".to_owned()
    } else {
        parts.join("; ")
    }
}

fn types_cell(observation: &FieldObservation) -> String {
    let mut types: Vec<String> = observation
        .json_types
        .iter()
        .map(|json_type| format!("{json_type:?}").to_lowercase())
        .collect();
    types.sort();
    types.join("/")
}

fn render(
    provider: &str,
    observations: &[FieldObservation],
    dispositions: &DispositionFile,
) -> String {
    let by_field: BTreeMap<(&str, Direction, &str), &Disposition> = dispositions
        .entries
        .iter()
        .map(|entry| {
            (
                (
                    entry.provider.as_str(),
                    entry.direction,
                    entry.path.as_str(),
                ),
                entry,
            )
        })
        .collect();

    let mut report = String::new();
    report.push_str(&format!("# {provider}: the provider surface\n\n"));
    report.push_str(
        "**Generated. Do not edit.** Rebuilt from the promoted corpus, the decode sources, and\n\
         `crates/harness/tests/corpus/dispositions.json` by\n\
         `cargo test -p comet-harness --test capture_corpus surface_report`, which fails when this\n\
         file is stale. Decisions belong in `dispositions.json`, not here.\n\n\
         Every field the promoted evidence observes, with what Comet does about it. Values are\n\
         printed only where the value's own grammar makes that safe; prose is withheld and a\n\
         redacted value reports its kind. A `consumed` row marked *derived* is a name match in the\n\
         decode sources, which proves something mentions the field, never that a value reaches a\n\
         user.\n\n",
    );

    for direction in [Direction::FromProvider, Direction::ToProvider] {
        let heading = match direction {
            Direction::FromProvider => "What the client reports (stdout, stderr)",
            Direction::ToProvider => "How Comet drives the client (stdin)",
        };
        let mut in_direction: Vec<&FieldObservation> = observations
            .iter()
            .filter(|observation| {
                observation.provider == provider && observation.direction == direction
            })
            .collect();
        if in_direction.is_empty() {
            continue;
        }
        in_direction.sort_by(|left, right| {
            right
                .count
                .cmp(&left.count)
                .then(left.path.cmp(&right.path))
        });
        report.push_str(&format!("## {heading}\n\n"));

        for state in [
            State::Unknown,
            State::Deferred,
            State::Consumed,
            State::NotApplicable,
        ] {
            let rows: Vec<&&FieldObservation> = in_direction
                .iter()
                .filter(|observation| {
                    by_field
                        .get(&(provider, direction, observation.path.as_str()))
                        .map(|entry| entry.state)
                        == Some(state)
                })
                .collect();
            if rows.is_empty() {
                continue;
            }
            let (title, blurb) = match state {
                State::Unknown => ("Unknown", "Nobody has decided. This is the backlog."),
                State::Deferred => ("Deferred", "Worth building; each names its debt row."),
                State::Consumed => ("Consumed", "Something in Comet names this field."),
                State::NotApplicable => ("Not applicable", "Nothing to build here."),
            };
            report.push_str(&format!(
                "### {title} - {} fields\n\n{blurb}\n\n",
                rows.len()
            ));
            report.push_str("| field | type | n | versions | first seen | values | note |\n");
            report.push_str("| --- | --- | --- | --- | --- | --- | --- |\n");
            for observation in rows {
                let entry = by_field.get(&(provider, direction, observation.path.as_str()));
                let note = entry
                    .and_then(|entry| match entry.state {
                        State::Deferred => entry.how.as_deref().map(|how| {
                            format!("**{}** - {how}", entry.debt.as_deref().unwrap_or("?"))
                        }),
                        State::Consumed => entry.consumer.clone().map(|consumer| {
                            if consumer == DERIVED_CONSUMER {
                                "_derived_".to_owned()
                            } else {
                                consumer
                            }
                        }),
                        State::NotApplicable => entry.reason.clone(),
                        State::Unknown => None,
                    })
                    .unwrap_or_else(|| "-".to_owned());
                report.push_str(&format!(
                    "| `{}` | {} | {} | {} | {} #{} | {} | {} |\n",
                    observation.path,
                    types_cell(observation),
                    observation.count,
                    observation
                        .versions
                        .iter()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", "),
                    observation.first_seen.scenario,
                    observation.first_seen.sequence,
                    values_cell(observation),
                    note,
                ));
            }
            report.push('\n');
        }
    }
    report
}

fn providers(observations: &[FieldObservation]) -> BTreeSet<String> {
    observations
        .iter()
        .map(|observation| observation.provider.clone())
        .collect()
}

/// Regenerates the seed and the reports when `COMET_UPDATE_SURFACE` is set, and
/// otherwise proves both are current.
///
/// Break caught: the committed report drifts from the evidence, and a document
/// people use to decide what to build quietly describes a corpus that no longer
/// exists.
#[test]
fn the_disposition_record_and_the_reports_are_current() {
    let observations = observe_corpus(&corpus_root()).unwrap();
    let consumed = consumed_fields(&decode_sources(&crate_root()).unwrap()).unwrap();
    let path = dispositions_path(&crate_root());

    if updating() {
        let fresh = seed(&observations, &consumed);
        let merged = match load(&path) {
            Ok(existing) => {
                // Adopting new surface is a separate decision from refreshing
                // the report. Without this the fix for "a field has no
                // disposition" is the command that silences it, and a CLI
                // version's forty new fields join a backlog of hundreds with
                // nobody having read what arrived.
                let known: BTreeSet<(String, Direction, String)> = existing
                    .entries
                    .iter()
                    .map(|entry| (entry.provider.clone(), entry.direction, entry.path.clone()))
                    .collect();
                let arrivals: Vec<String> = fresh
                    .entries
                    .iter()
                    .filter(|entry| {
                        !known.contains(&(
                            entry.provider.clone(),
                            entry.direction,
                            entry.path.clone(),
                        ))
                    })
                    .map(|entry| format!("{} {:?} {}", entry.provider, entry.direction, entry.path))
                    .collect();
                assert!(
                    arrivals.is_empty() || adopting(),
                    "{} field(s) are new to the corpus. Read them, then re-run with \
                     COMET_ADOPT_FIELDS=1 to record them as unknown:\n  {}",
                    arrivals.len(),
                    arrivals.join("\n  ")
                );
                merge(&existing, fresh)
            }
            Err(_) => fresh,
        };
        let mut bytes = serde_json::to_vec_pretty(&merged).unwrap();
        bytes.push(b'\n');
        replace(&path, &bytes);
    }

    let dispositions = load(&path)
        .unwrap_or_else(|error| panic!("{error}\nrun with COMET_UPDATE_SURFACE=1 to seed it"));

    for provider in providers(&observations) {
        let rendered = render(&provider, &observations, &dispositions);
        let destination = report_path(&repo_root(), &provider);
        if updating() {
            std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
            replace(&destination, rendered.as_bytes());
            continue;
        }
        let committed = std::fs::read_to_string(&destination)
            .unwrap_or_else(|_| panic!("{} is missing", destination.display()))
            .replace("\r\n", "\n");
        assert_eq!(
            committed,
            rendered,
            "{} is stale; rerun with COMET_UPDATE_SURFACE=1",
            destination.display()
        );
    }
}

/// Puts the new contents in place in one step.
///
/// The regeneration command's own filter (`--test capture_corpus surface_report`)
/// matches four tests, three of which read `dispositions.json` while this one
/// rewrites it. `fs::write` truncates before it fills, so a reader could catch
/// the file empty and fail as though the record were corrupt.
fn replace(path: &Path, bytes: &[u8]) {
    let temporary = path.with_extension("regenerating");
    std::fs::write(&temporary, bytes).unwrap();
    std::fs::rename(&temporary, path).unwrap();
}

/// The gate.
///
/// Break caught: a field arrives that no disposition covers - a new CLI version
/// adding one, or a newly promoted capture - and it lands silently instead of
/// as a test failure. This is the drift detector the whole slice exists for.
///
/// It does **not** require the backlog to be empty. Unknown entries pass;
/// gating on zero would stall the slice or fill the record with rubber stamps.
#[test]
fn every_observed_field_carries_a_disposition() {
    let observations = observe_corpus(&corpus_root()).unwrap();
    let dispositions = load(&dispositions_path(&crate_root())).unwrap();
    let covered: BTreeSet<(String, Direction, String)> = dispositions
        .entries
        .iter()
        .map(|entry| (entry.provider.clone(), entry.direction, entry.path.clone()))
        .collect();

    let uncovered: Vec<String> = observations
        .iter()
        .filter(|observation| {
            !covered.contains(&(
                observation.provider.clone(),
                observation.direction,
                observation.path.clone(),
            ))
        })
        .map(|observation| {
            format!(
                "{} {:?} {} (first seen {} #{})",
                observation.provider,
                observation.direction,
                observation.path,
                observation.first_seen.scenario,
                observation.first_seen.sequence,
            )
        })
        .collect();

    assert!(
        uncovered.is_empty(),
        "{} observed field(s) have no disposition; decide each in dispositions.json:\n  {}",
        uncovered.len(),
        uncovered.join("\n  ")
    );
}

/// Break caught: the record's own rules stop being enforced, and a deferred row
/// can point nowhere while a not-applicable row quietly becomes debt.
#[test]
fn the_disposition_record_keeps_its_own_rules() {
    let observations = observe_corpus(&corpus_root()).unwrap();
    let dispositions = load(&dispositions_path(&crate_root())).unwrap();
    let problems = validate(
        &dispositions,
        &observed_keys(&observations),
        &known_debt_rows(&repo_root()).unwrap(),
        &consumed_fields(&decode_sources(&crate_root()).unwrap()).unwrap(),
    );
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// Break caught: the report prints a value the walker withheld. The sentinel is
/// the one check that does not trust any classifier, including this slice's.
#[test]
fn no_report_prints_a_withheld_value() {
    let observations = observe_corpus(&corpus_root()).unwrap();
    let dispositions = load(&dispositions_path(&crate_root())).unwrap();
    for provider in providers(&observations) {
        let rendered = render(&provider, &observations, &dispositions);
        for observation in observations
            .iter()
            .filter(|observation| observation.provider == provider && observation.values.withheld)
        {
            assert!(
                !rendered.contains("Alpha step"),
                "withheld prose reached the {provider} report via {}",
                observation.path
            );
        }
    }
}
