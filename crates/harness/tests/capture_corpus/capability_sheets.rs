//! The golden test for the capability sheets (Task 4, design §3.5).
//!
//! Every `<provider>/<version>` directory the committed archive holds must
//! render, byte-for-byte, into `docs/providers/<provider>-<version>.md`. The
//! sheet is generated from the archive bytes alone — [`observe_surface`] and
//! each scenario's `manifest.json` — never from the scenario table's
//! declarations, so this test regenerates evidence from the same corpus
//! `surface_map.rs` walks rather than trusting anything cached.
//!
//! `COMET_UPDATE_SHEETS=1` regenerates the committed files instead of
//! asserting against them — the same shape `observed_fields.rs` used for
//! `COMET_UPDATE_SURFACE` before this test replaced it (that file and
//! `tests/corpus/observed-fields.json` are deleted in the same change that
//! adds this one).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use comet_harness::capture::{Direction, SheetScenario, observe_surface, render_sheet};
use serde_json::Value;

fn corpus_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus")
}

/// `docs/providers/`, two levels above this crate — the sheets live outside
/// any test tree so they're what a reader finds deciding what to implement,
/// not something buried under `tests/`.
fn docs_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("docs")
        .join("providers")
}

fn sorted_dirs(parent: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(parent)
        .unwrap_or_else(|error| panic!("{} could not be read: {error}", parent.display()))
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    dirs
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Every `(provider, version)` pair `root` holds, discovered by walking
/// directories rather than a hand-maintained list — a newly promoted version
/// directory must make this test fail for want of a sheet, not silently go
/// unchecked because nobody added its name here.
fn corpus_versions(root: &Path) -> Vec<(String, String)> {
    let mut versions = Vec::new();
    for provider in sorted_dirs(root) {
        let provider_name = file_name(&provider);
        for version in sorted_dirs(&provider) {
            versions.push((provider_name.clone(), file_name(&version)));
        }
    }
    versions
}

/// Every promoted scenario under `root/provider/version`, as [`render_sheet`]
/// needs it: the manifest's `purpose` and its exact argv
/// (`command.program` followed by `command.args`), read straight from the
/// committed `manifest.json` — never the scenario table's declared strings,
/// so the sheet reports what a run actually recorded rather than what the
/// table merely claims.
fn scenarios_for(root: &Path, provider: &str, version: &str) -> Vec<SheetScenario> {
    let version_dir = root.join(provider).join(version);
    let mut scenarios = Vec::new();
    for entry in std::fs::read_dir(&version_dir)
        .unwrap_or_else(|error| panic!("{} could not be read: {error}", version_dir.display()))
        .filter_map(Result::ok)
    {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let manifest_path = path.join("manifest.json");
        if !manifest_path.is_file() {
            continue;
        }
        let name = file_name(&path);
        let manifest: Value = serde_json::from_str(
            &std::fs::read_to_string(&manifest_path).unwrap_or_else(|error| {
                panic!("{} could not be read: {error}", manifest_path.display())
            }),
        )
        .unwrap_or_else(|error| panic!("{} is not valid JSON: {error}", manifest_path.display()));

        let purpose = manifest["purpose"]
            .as_str()
            .unwrap_or_else(|| panic!("{} has no string \"purpose\"", manifest_path.display()))
            .to_owned();
        let program = manifest["command"]["program"]
            .as_str()
            .unwrap_or_else(|| panic!("{} has no command.program", manifest_path.display()))
            .to_owned();
        let args = manifest["command"]["args"]
            .as_array()
            .unwrap_or_else(|| panic!("{} has no command.args", manifest_path.display()))
            .iter()
            .map(|arg| {
                arg.as_str()
                    .unwrap_or_else(|| {
                        panic!(
                            "{} has a non-string command.args entry",
                            manifest_path.display()
                        )
                    })
                    .to_owned()
            });
        let mut argv = vec![program];
        argv.extend(args);

        scenarios.push(SheetScenario {
            name,
            purpose,
            argv,
        });
    }
    scenarios
}

/// Renders one version's sheet straight from `root`'s committed bytes: walks
/// the whole archive with [`observe_surface`] (one pass covers every
/// version), scopes the observations and vocabulary down to `(provider,
/// version)`, and hands the result to the pure renderer.
fn render_version(root: &Path, provider: &str, version: &str) -> String {
    let (observations, vocabulary) = observe_surface(root)
        .unwrap_or_else(|error| panic!("{} could not be walked: {error}", root.display()));

    let scoped_vocabulary: BTreeMap<(Direction, String), BTreeSet<String>> = vocabulary
        .into_iter()
        .filter(|((entry_provider, entry_version, _), _)| {
            entry_provider == provider && entry_version == version
        })
        .flat_map(|((_, _, direction), paths)| {
            paths
                .into_iter()
                .map(move |(path, values)| ((direction, path), values))
        })
        .collect();

    let scenarios = scenarios_for(root, provider, version);
    render_sheet(
        provider,
        version,
        &observations,
        &scoped_vocabulary,
        &scenarios,
    )
}

/// The line at which two documents first diverge, 1-indexed — enough for a
/// reader to act on without a diff tool, and it names which of the two ran
/// long if the content matches up to the shorter one's end.
fn first_difference(committed: &str, generated: &str) -> String {
    let committed_lines: Vec<&str> = committed.lines().collect();
    let generated_lines: Vec<&str> = generated.lines().collect();
    for (index, (committed_line, generated_line)) in committed_lines
        .iter()
        .zip(generated_lines.iter())
        .enumerate()
    {
        if committed_line != generated_line {
            return format!(
                "line {} differs:\n    committed:  {committed_line:?}\n    generated:  {generated_line:?}",
                index + 1
            );
        }
    }
    if committed_lines.len() != generated_lines.len() {
        return format!(
            "line count differs: committed has {}, generated has {}",
            committed_lines.len(),
            generated_lines.len()
        );
    }
    "content matches but bytes differ (e.g. trailing newline or line ending)".to_owned()
}

/// Compares every `(provider, version)` in `corpus_root` against the sheet
/// committed at `docs_root/<provider>-<version>.md`, returning one message
/// per mismatch or missing sheet. Shared by the golden test (real corpus,
/// real `docs/providers/`) and the missing-sheet regression test (a
/// synthetic corpus and an empty `docs_root`), so both exercise the same
/// comparison rather than two hand-written variants that could drift apart.
fn compare_all_versions(corpus_root: &Path, docs_root: &Path) -> Vec<String> {
    let mut failures = Vec::new();
    for (provider, version) in corpus_versions(corpus_root) {
        let generated = render_version(corpus_root, &provider, &version);
        let sheet_path = docs_root.join(format!("{provider}-{version}.md"));

        match std::fs::read_to_string(&sheet_path) {
            Ok(committed) if committed == generated => {}
            Ok(committed) => failures.push(format!(
                "{} does not match its generated sheet ({}); rerun with \
                 $env:COMET_UPDATE_SHEETS = \"1\" and review the diff before committing",
                sheet_path.display(),
                first_difference(&committed, &generated)
            )),
            Err(error) => failures.push(format!(
                "{} could not be read ({error}); a version directory with no committed sheet \
                 is the newly-promoted-capture case this test exists to catch — rerun with \
                 $env:COMET_UPDATE_SHEETS = \"1\" to generate it",
                sheet_path.display()
            )),
        }
    }
    failures
}

/// Break caught: every version's generated sheet must match its committed
/// document exactly, so a CLI update that adds, removes or reshapes a field
/// arrives as a failing test rather than as silence. `COMET_UPDATE_SHEETS=1`
/// regenerates the committed files instead of asserting.
#[test]
fn every_corpus_version_matches_its_committed_sheet() {
    let root = corpus_root();

    if std::env::var_os("COMET_UPDATE_SHEETS").is_some() {
        std::fs::create_dir_all(docs_root()).unwrap();
        for (provider, version) in corpus_versions(&root) {
            let generated = render_version(&root, &provider, &version);
            let sheet_path = docs_root().join(format!("{provider}-{version}.md"));
            std::fs::write(&sheet_path, generated).unwrap_or_else(|error| {
                panic!("{} could not be written: {error}", sheet_path.display())
            });
        }
        return;
    }

    let failures = compare_all_versions(&root, &docs_root());
    assert!(
        failures.is_empty(),
        "{} sheet(s) out of date:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

fn write_minimal_scenario(root: &Path, provider: &str, version: &str, scenario: &str) {
    let directory = root.join(provider).join(version).join(scenario);
    std::fs::create_dir_all(&directory).unwrap();

    let payload = serde_json::to_string(&serde_json::json!({"type": "system"})).unwrap();
    let line = serde_json::json!({"sequence": 1, "channel": "stdout", "payload": payload});
    std::fs::write(
        directory.join("events.jsonl"),
        format!("{}\n", serde_json::to_string(&line).unwrap()),
    )
    .unwrap();

    let manifest = serde_json::json!({
        "purpose": "smoke test the missing-sheet case",
        "command": {"program": "prog", "args": ["--flag"]},
    });
    std::fs::write(
        directory.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
}

/// Break caught: this is the newly-promoted-capture case the whole mechanism
/// exists for — a version directory with no committed sheet must fail the
/// suite, not render on the fly and pass silently.
#[test]
fn a_version_with_no_committed_sheet_fails() {
    let corpus = tempfile::tempdir().unwrap();
    write_minimal_scenario(corpus.path(), "claude", "9.9.9", "smoke");
    let empty_docs = tempfile::tempdir().unwrap();

    let failures = compare_all_versions(corpus.path(), empty_docs.path());

    assert_eq!(
        failures.len(),
        1,
        "exactly one version with no sheet must produce exactly one failure: {failures:?}"
    );
    assert!(
        failures[0].contains("claude-9.9.9.md"),
        "the failure must name the missing sheet file: {failures:?}"
    );
    assert!(
        failures[0].contains("COMET_UPDATE_SHEETS"),
        "the failure must tell the reader how to generate it: {failures:?}"
    );
}
