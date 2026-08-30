//! Every Codex JSON-RPC method Comet's production code names is one the corpus
//! has seen, or one somebody wrote down a reason for.
//!
//! **This automates D90**, which measured the same thing by hand on 2026-08-16
//! and has been a static list ever since. A hand measurement answers the
//! question once; this answers it on every run, which is what turns "22 blind
//! decodes" from a fact about that afternoon into a bound that cannot quietly
//! grow.
//!
//! **The two lists below are a risk split, not one undifferentiated set** —
//! D90's own row asks for exactly that, because the entries do not carry equal
//! risk:
//!
//! - a method in `normalize::IGNORED_NOTIFICATIONS` that no capture shows costs
//!   a **dead row**. Comet recognizes it and drops it; if the name is wrong or
//!   the notification no longer exists, nothing breaks and nothing is reached.
//! - a **claimed** method no capture shows is the real failure: a decode path
//!   that ships never having been constructed, which is
//!   `.agents/rules/optional-wire-fields.md`'s problem one level up. Seventeen
//!   of the twenty-five are dead rows; eight are claimed.
//!
//! **This measurement disagrees with D90's, in both directions, and the
//! disagreement is the point of automating it.** D90 counted `model/rerouted`
//! and `item/contextCompaction`, which production names nowhere — both appear
//! only inside `#[cfg(test)]` blocks, so no shipped decode mentions them. It
//! missed `item/permissions/requestApproval`, `item/reasoning/textDelta`,
//! `item/untyped`, `turn/aborted`, `turn/failed` and `turn/plan/updated`, all
//! of which production genuinely dispatches on.

use std::collections::BTreeSet;
use std::path::Path;

use comet_capture::{corpus_root, observe_surface};

/// Entries of `normalize::IGNORED_NOTIFICATIONS` that no promoted capture
/// shows. Recognized-and-dropped, so the cost of each is a dead row rather
/// than an unreachable decode.
///
/// The reasons here are deliberately short: the ignore table carries its own
/// per-entry reason for *dropping* the notification, and repeating it would be
/// a second copy to drift. What this list records is why the name is carried
/// with no evidence behind it.
const IGNORED_WITHOUT_EVIDENCE: &[(&str, &str)] = &[
    (
        "account/updated",
        "Named by the generated schema, never observed firing; dropping it costs a dead ignore row, not a decode.",
    ),
    (
        "command/exec/outputDelta",
        "The pre-`item/` spelling of an output stream Comet does not render; kept for an older codex-cli, never captured.",
    ),
    (
        "hook/completed",
        "Hook lifecycle, schema-named. `normalize.rs`'s own comment says neither hook method has ever been observed on any capture.",
    ),
    (
        "hook/started",
        "Hook lifecycle, schema-named; same never-observed note as its sibling in the ignore table.",
    ),
    (
        "item/autoApprovalReview/completed",
        "Produced only under `approvalsReviewer: auto_review`, which only `RuntimeMode::Auto` sets. D90 records that an `auto` scenario was expected to force it and did not.",
    ),
    (
        "item/autoApprovalReview/started",
        "The other half of the auto-review pair, and unforced for the same reason.",
    ),
    (
        "item/commandExecution/outputDelta",
        "High-volume exec output Comet does not render. Marked observed in the ignore table's own prose (2026-08-08) but absent from every promoted capture — a discrepancy worth knowing about.",
    ),
    (
        "item/fileChange/outputDelta",
        "Schema-deprecated, per the ignore table's own grouping; carried for an older peer that might still send it.",
    ),
    (
        "item/fileChange/patchUpdated",
        "Incremental patch stream Comet does not render; no promoted scenario edits a file often enough to produce one.",
    ),
    (
        "item/mcpToolCall/progress",
        "MCP tool progress. No promoted Codex scenario calls an MCP tool at all, which is the same gap AGENTS.md names for tool-name vocabulary.",
    ),
    (
        "item/plan/delta",
        "Deliberately declined by slice 4.3, which owns the plan surface; the ignore table records it has never been observed.",
    ),
    (
        "process/exited",
        "Process bookkeeping, schema-named, never observed.",
    ),
    (
        "skills/changed",
        "Owned by slice 2.4 (the `/` menu), which is the row that would consume it; nothing has captured one.",
    ),
    (
        "process/outputDelta",
        "Process output stream Comet does not render, never observed.",
    ),
    (
        "thread/compacted",
        "Schema-deprecated in favour of the context-compaction item; carried for an older peer.",
    ),
    (
        "thread/environment/connected",
        "Baseline environment state. Its `disconnected` sibling IS claimed, which is why only this one lands here.",
    ),
    (
        "thread/name/updated",
        "Redundant with Comet's own titling; never observed.",
    ),
];

/// Methods Comet **claims** — dispatches on with a real decode — that no
/// promoted capture shows. Each is a path that ships never having been
/// constructed, which is the failure this lint exists to bound.
///
/// A new entry here is a decision to ship a decode written from a schema or a
/// typing rather than from evidence. That is sometimes right — a turn that
/// fails is not a thing a capture can be asked for politely — but it should be
/// deliberate, which is what writing the reason down forces.
const CLAIMED_WITHOUT_EVIDENCE: &[(&str, &str)] = &[
    (
        "item/permissions/requestApproval",
        "Codex's third approval method, answered `Unknown` and written blind — D22 is the open row, and it says outright that no capture run has produced one.",
    ),
    (
        "item/reasoning/textDelta",
        "Shares an arm with `item/reasoning/summaryTextDelta`, which IS evidenced. The unsummarized spelling has never arrived; the arm costs nothing extra and would otherwise be a silent drop.",
    ),
    (
        "item/untyped",
        "Not a wire method at all — the discriminator `normalize::map_item` mints for an item whose `type` it does not recognize. It cannot appear in a capture by construction, and is here so the scan's own shape is on the record rather than mistaken for a gap.",
    ),
    (
        "mcpServer/oauthLogin/completed",
        "MCP OAuth completion, decoded into a notice. No promoted scenario configures an MCP server that needs a login.",
    ),
    (
        "thread/environment/disconnected",
        "Decoded into a notice; the environment never disconnected during any capture, which is what a healthy run looks like.",
    ),
    (
        "turn/aborted",
        "The interrupt path's terminal frame. Every promoted turn scenario runs to completion — a capture that aborts mid-turn is its own scenario nobody has written.",
    ),
    (
        "turn/failed",
        "The error path's terminal frame, carrying the message a user sees. No promoted scenario provokes a failing turn.",
    ),
    (
        "turn/plan/updated",
        "Claimed by slice 4.3 and evidenced only as a HAND-COPIED fixture in `normalize.rs`'s `plan_tests`, taken from a 2026-08-13 capture nobody promoted. The decode is not blind, but the corpus cannot show that — promoting a Codex plan scenario is what would close this one.",
    ),
];

/// Corpus methods production names nowhere, each a decision on record.
const EVIDENCED_WITHOUT_A_NAME: &[(&str, &str)] = &[(
    "thread/goal/cleared",
    "The thread-goal family is named in `IGNORED_NOTIFICATIONS`' doc comment as \
     deliberately NOT ignored, so it falls to the Unknown catch-all and raises a \
     diagnostic — the honest signal for a notification no surface wants.",
)];

/// Every JSON-RPC method name Comet's production Codex code mentions.
///
/// **Production only**: each file is cut at its first `#[cfg(test)]`, because a
/// method named solely by a test is not a decode that ships. That single rule
/// is what removes `model/rerouted` and `item/contextCompaction` from D90's
/// hand-measured list — both are test-only mentions.
///
/// **Slash-shaped names only.** `initialize` and `initialized` are real methods
/// with no slash, and a scan loose enough to catch them would catch every
/// lowercase string literal in the crate. The limit is stated rather than
/// hidden: this lint covers the `family/event` namespace, which is every
/// notification and every method Codex answers a turn with.
fn named_methods() -> BTreeSet<String> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../harness/src/codex");
    let mut names = BTreeSet::new();
    let entries = std::fs::read_dir(&dir).expect("the Codex adapter's source directory");
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "rs") {
            continue;
        }
        let source = std::fs::read_to_string(&path).expect("a Codex source file");
        let production = match source.find("#[cfg(test)]") {
            Some(cut) => &source[..cut],
            None => &source[..],
        };
        names.extend(method_literals(production));
    }
    names
}

/// Every `"family/event"`-shaped string literal in `source`.
fn method_literals(source: &str) -> Vec<String> {
    let mut found = Vec::new();
    for (index, _) in source.match_indices('"') {
        let rest = &source[index + 1..];
        let Some(end) = rest.find('"') else { continue };
        let literal = &rest[..end];
        if is_method_shaped(literal) {
            found.push(literal.to_owned());
        }
    }
    found
}

/// `family/event` or `family/thing/event`: lowercase-initial segments, at least
/// one slash, letters only.
fn is_method_shaped(literal: &str) -> bool {
    let mut segments = literal.split('/');
    let mut count = 0;
    for segment in &mut segments {
        count += 1;
        let mut chars = segment.chars();
        let Some(first) = chars.next() else {
            return false;
        };
        if !first.is_ascii_lowercase() || !chars.all(|c| c.is_ascii_alphabetic()) {
            return false;
        }
    }
    count >= 2
}

/// Every method any promoted Codex capture shows, either direction.
fn evidenced_methods() -> BTreeSet<String> {
    let (_, vocabulary) = observe_surface(&corpus_root()).expect("the corpus walks");
    vocabulary
        .iter()
        .filter(|((provider, _, _), _)| provider == "codex")
        .filter_map(|(_, paths)| paths.get(".method"))
        .flatten()
        .filter(|method| is_method_shaped(method))
        .cloned()
        .collect()
}

#[test]
fn every_named_codex_method_has_evidence_or_a_recorded_reason() {
    let named = named_methods();
    assert!(
        named.len() > 30,
        "the source scan found only {} method(s) — the adapter's shape changed and \
         this lint would otherwise pass by checking almost nothing",
        named.len()
    );
    let evidenced = evidenced_methods();
    assert!(
        !evidenced.is_empty(),
        "no Codex capture shows a method; the vocabulary path or the corpus moved"
    );

    let declared: BTreeSet<&str> = IGNORED_WITHOUT_EVIDENCE
        .iter()
        .chain(CLAIMED_WITHOUT_EVIDENCE)
        .map(|(method, _)| *method)
        .collect();

    let undeclared: Vec<&String> = named
        .iter()
        .filter(|method| !evidenced.contains(*method) && !declared.contains(method.as_str()))
        .collect();
    assert!(
        undeclared.is_empty(),
        "{} Codex method(s) are named in production with no capture behind them and \
         no reason recorded: {undeclared:?}. Capture one, or add it to \
         CLAIMED_WITHOUT_EVIDENCE (a decode nobody can reach) or \
         IGNORED_WITHOUT_EVIDENCE (a dead ignore row) with the reason.",
        undeclared.len()
    );
}

#[test]
fn a_declared_codex_method_that_gains_evidence_is_pruned() {
    let evidenced = evidenced_methods();
    let named = named_methods();

    let stale: Vec<&str> = IGNORED_WITHOUT_EVIDENCE
        .iter()
        .chain(CLAIMED_WITHOUT_EVIDENCE)
        .map(|(method, _)| *method)
        .filter(|method| evidenced.contains(*method))
        .collect();
    assert!(
        stale.is_empty(),
        "{} declared blind method(s) now appear in a capture: {stale:?}. Delete the \
         entry — the count is the point of this lint, and an exemption nobody prunes \
         inflates it forever.",
        stale.len()
    );

    let gone: Vec<&str> = IGNORED_WITHOUT_EVIDENCE
        .iter()
        .chain(CLAIMED_WITHOUT_EVIDENCE)
        .map(|(method, _)| *method)
        .filter(|method| !named.contains(*method))
        .collect();
    assert!(
        gone.is_empty(),
        "{} declared method(s) are no longer named in production: {gone:?}. The decode \
         was deleted and its entry outlived it.",
        gone.len()
    );

    for (method, reason) in IGNORED_WITHOUT_EVIDENCE
        .iter()
        .chain(CLAIMED_WITHOUT_EVIDENCE)
    {
        assert!(
            reason.len() > 40,
            "{method}'s entry has to say WHY it is carried, not just that it is: {reason:?}"
        );
    }
}

/// The reverse direction: the corpus showing a method production names nowhere.
///
/// Not automatically a bug — falling through to the Unknown catch-all raises a
/// diagnostic, which is the designed answer for a notification no surface
/// wants. It is a decision, and this test is what makes it one.
#[test]
fn every_evidenced_codex_method_is_named_or_declined() {
    let named = named_methods();
    let evidenced = evidenced_methods();
    let declined: BTreeSet<&str> = EVIDENCED_WITHOUT_A_NAME
        .iter()
        .map(|(method, _)| *method)
        .collect();

    let unhandled: Vec<&String> = evidenced
        .iter()
        .filter(|method| !named.contains(*method) && !declined.contains(method.as_str()))
        .collect();
    assert!(
        unhandled.is_empty(),
        "{} captured Codex method(s) are named nowhere in production and have no \
         recorded decision: {unhandled:?}. Claim it, ignore it, or record why the \
         Unknown diagnostic is the right answer.",
        unhandled.len()
    );

    let stale: Vec<&str> = declined
        .iter()
        .copied()
        .filter(|method| named.contains(*method))
        .collect();
    assert!(
        stale.is_empty(),
        "{} declined method(s) are now named in production: {stale:?}. Delete the entry.",
        stale.len()
    );
}

/// The bound itself, stated as a number so it cannot drift upward unnoticed.
///
/// **The split matters more than the total.** Seventeen dead ignore rows cost
/// nothing to carry; eight claimed decodes are eight paths that ship never
/// having been constructed. A change that turns a dead row into a claimed
/// decode keeps the total and fails here, which is the whole reason this is two
/// numbers rather than one.
#[test]
fn the_blind_codex_decode_count_is_pinned() {
    assert_eq!(
        IGNORED_WITHOUT_EVIDENCE.len(),
        17,
        "the dead-ignore-row count changed; update the number deliberately"
    );
    assert_eq!(
        CLAIMED_WITHOUT_EVIDENCE.len(),
        8,
        "the count of decodes nobody can reach changed — that is the number worth \
         watching, so it moves only on purpose"
    );
}
