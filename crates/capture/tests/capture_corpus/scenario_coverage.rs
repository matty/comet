//! Coverage gate: every scenario declared in `SCENARIOS` must have at least
//! one promoted corpus directory behind it, somewhere under its provider (any
//! version — a scenario captured on one version counts as covered).
//!
//! Nothing previously checked this. A row can sit in the table, render in
//! `--help`, and never once have been captured, and nothing would notice.
//! Codex's `approval`, `approval-on-request` and `interruption` rows are
//! exactly that: declared across several stages with zero corpus evidence
//! behind any of them.

use std::collections::BTreeSet;

use comet_capture::{SCENARIOS, corpus_provider_name, corpus_root, promoted_scenarios};

/// Declared scenarios with no corpus evidence at all, named explicitly rather
/// than silently skipped: `(provider, scenario name)`.
///
/// This list must shrink only by a real capture landing, never grow silently
/// to paper over a new gap. `scenario_coverage_gate` below checks both
/// directions: it fails when an *unexempted* row has no evidence, and it
/// fails when an *exempted* row has gained evidence and this entry was left
/// stale — that second direction is what stops the exemption list itself from
/// becoming the next place a coverage gap hides unnoticed.
///
/// **One row, and it is a scheduling state rather than a permanent one.**
/// `claude/edit` is declared so the capture can be taken; taking it spends
/// tokens on a live Claude turn, which `docs/testing/provider-captures.md`
/// makes a separate authorization from landing the code. It is the capture
/// that closes `claude_tool_coverage.rs`'s `Edit` entry — and with it the
/// typings-only reasoning in D17 and D18, which is why the row is worth
/// carrying rather than deferring until someone has a spare turn.
///
/// Everything else has promoted evidence: the Claude/Codex rows landed in the
/// stage-6 re-capture, Grok's three in `grok/1.0.5/`. A new row belongs here
/// only with the reason it cannot be captured written beside it.
const EXEMPT_UNCAPTURED: &[(&str, &str)] = &[("claude", "edit")];

/// Break caught: a scenario declared in `SCENARIOS` (and rendered in
/// `--help`) with no promoted corpus directory behind it anywhere, for any
/// version, under its provider — unless it is named in `EXEMPT_UNCAPTURED`.
/// Also catches the exemption list itself going stale once a listed scenario
/// is finally captured.
#[test]
fn every_declared_scenario_has_corpus_evidence_or_a_named_exemption() {
    let root = corpus_root();
    let promoted = promoted_scenarios(&root)
        .unwrap_or_else(|error| panic!("{} could not be walked: {error}", root.display()));

    let covered: BTreeSet<(String, String)> = promoted
        .iter()
        .map(|scenario| {
            let name = scenario
                .directory
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_owned();
            (scenario.provider.clone(), name)
        })
        .collect();

    let exempt: BTreeSet<(&str, &str)> = EXEMPT_UNCAPTURED.iter().copied().collect();

    let mut missing = Vec::new();
    let mut stale_exemptions = Vec::new();

    for spec in SCENARIOS {
        let provider = corpus_provider_name(spec.provider, spec.name);
        let key = (provider.to_owned(), spec.name.to_owned());
        let is_covered = covered.contains(&key);
        let is_exempt = exempt.contains(&(provider, spec.name));

        if !is_covered && !is_exempt {
            missing.push(format!("{provider}/{}", spec.name));
        }
        if is_covered && is_exempt {
            stale_exemptions.push(format!("{provider}/{}", spec.name));
        }
    }

    assert!(
        missing.is_empty(),
        "{} declared scenario(s) have no corpus evidence and no exemption in \
         EXEMPT_UNCAPTURED:\n{}",
        missing.len(),
        missing.join("\n")
    );
    assert!(
        stale_exemptions.is_empty(),
        "{} scenario(s) listed in EXEMPT_UNCAPTURED now have corpus evidence; remove their \
         entry so the exemption doesn't hide a future regression:\n{}",
        stale_exemptions.len(),
        stale_exemptions.join("\n")
    );
}
