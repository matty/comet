//! Every ACP `sessionUpdate` kind the decode names is one the corpus has
//! actually seen, and every kind the corpus has seen is one somebody decided
//! about.
//!
//! **This is the shape `docs/debt/README.md`'s D69 calls the valuable one** —
//! the check that catches a decode nobody can reach, rather than one that
//! compares version numbers. It is built here first because ACP is the one
//! surface where the corpus already records the vocabulary: `.params.update.
//! sessionUpdate` is a declared `VOCABULARY_PATHS` entry, so the evidence is a
//! set of strings rather than a manual read of every frame. Claude's and
//! Codex's equivalents want the same treatment and do not have that data yet —
//! D90 is the hand-measured version of the same question for Codex methods.
//!
//! Both directions matter and they fail for different reasons:
//!
//! - a decoded kind with no evidence is **blind-coded** — written from a
//!   typing or a guess, shipping a path that may never have been constructed
//!   (`.agents/rules/optional-wire-fields.md`'s failure, one level up);
//! - an evidenced kind nothing decodes is **drift** — the agent says something
//!   this build ignores, which is a decision somebody should take rather than
//!   a silence.

use std::collections::BTreeSet;
use std::path::Path;

use comet_capture::{Direction, corpus_root, observe_surface};

/// The path the ACP frame kind lives at, as `surface.rs` spells it.
const SESSION_UPDATE_PATH: &str = ".params.update.sessionUpdate";

/// Evidenced ACP update kinds that nothing decodes, each a decision on record.
///
/// This list must shrink only by a decode landing, and grow only by a
/// deliberate ruling — a new kind arriving in a future capture fails the test
/// until someone writes down which it is. That is the whole mechanism: the
/// alternative is a capability appearing in the archive and nobody noticing.
const UNDECODED_BY_DECISION: &[(&str, &str)] = &[
    (
        "tool_call_delta_chunk",
        "Grok's streaming tool-argument delta. `normalize::tool_update` reads the \
         completed call instead, so a partial argument has nothing to update.",
    ),
    (
        "pending_interaction",
        "Grok's own permission announcement (`kind: \"permission\"`). The approval \
         bridge is built on ACP's `session/request_permission`, which no promoted \
         Grok capture contains — D128 is the open question of whether Grok ever \
         sends it.",
    ),
    (
        "interaction_resolved",
        "The other half of `pending_interaction`; same row, D128.",
    ),
    (
        "response_completed",
        "A per-response marker inside a turn. Turn end is read from the prompt \
         reply's `stopReason` and Grok's own completion notification, which are \
         authoritative; this one would double-count.",
    ),
    (
        "session_summary_generated",
        "The agent's own session summary. Comet titles chats itself (`titles.rs`), \
         and taking the agent's would silently override a user's rename.",
    ),
    (
        "turn_completed",
        "Carries the usage breakdown, read by `grok::usage` off the reply rather \
         than here — see `acp/session.rs`'s settle arm, which already harvests it.",
    ),
];

/// Decoded kinds no ACP capture has produced, each with the reason it is
/// carried anyway.
///
/// Empty today, and that is the state worth keeping: every arm
/// `normalize::session_update` names is one a real agent has been observed
/// sending. A new arm added from a typing rather than a capture belongs here
/// with that said out loud.
const DECODED_WITHOUT_EVIDENCE: &[(&str, &str)] = &[];

/// Every `sessionUpdate` kind named as a match arm in
/// `normalize::session_update`.
///
/// Reads the source text rather than calling the function, for the reason
/// `no_runtime_cloud.rs` and `debt_citations.rs` do: what is under test is
/// which arms EXIST, and no runtime call can enumerate that. Bounded to the
/// one match block so an unrelated string literal elsewhere in the file cannot
/// masquerade as a decoded kind.
fn decoded_kinds() -> BTreeSet<String> {
    let source = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../harness/src/acp/normalize.rs"),
    )
    .expect("the ACP normalizer's source");

    let start = source
        .find("pub(crate) fn session_update(")
        .expect("session_update is where the ACP frame kind is dispatched");
    let block = &source[start..];
    let match_start = block.find("match kind {").expect("the kind dispatch");
    let block = &block[match_start..];
    // The catch-all arm ends the named kinds; everything after it is the
    // unknown-kind diagnostic, not a decode.
    let end = block.find("other =>").unwrap_or(block.len());
    let block = &block[..end];

    let mut kinds = BTreeSet::new();
    for line in block.lines() {
        let line = line.trim();
        // A match arm, not a string inside an arm's body.
        if !line.starts_with('"') || !line.contains("=>") {
            continue;
        }
        let patterns = line.split("=>").next().unwrap_or_default();
        for piece in patterns.split('|') {
            let piece = piece.trim().trim_matches('"');
            if !piece.is_empty() {
                kinds.insert(piece.to_owned());
            }
        }
    }
    kinds
}

/// Every `sessionUpdate` value any promoted ACP capture has shown.
fn evidenced_kinds() -> BTreeSet<String> {
    let (_, vocabulary) = observe_surface(&corpus_root()).expect("the corpus walks");
    vocabulary
        .iter()
        .filter(|((_, _, direction), _)| *direction == Direction::FromProvider)
        .filter_map(|(_, paths)| paths.get(SESSION_UPDATE_PATH))
        .flatten()
        .cloned()
        .collect()
}

#[test]
fn every_decoded_acp_update_kind_has_corpus_evidence() {
    let decoded = decoded_kinds();
    assert!(
        decoded.len() > 3,
        "the source scan found only {decoded:?} — the dispatch's shape changed and \
         this test would otherwise pass by checking almost nothing"
    );

    let evidenced = evidenced_kinds();
    assert!(
        !evidenced.is_empty(),
        "no ACP capture shows a sessionUpdate value; the vocabulary path or the \
         corpus moved"
    );

    let excused: BTreeSet<&str> = DECODED_WITHOUT_EVIDENCE
        .iter()
        .map(|(kind, _)| *kind)
        .collect();

    let blind: Vec<&String> = decoded
        .iter()
        .filter(|kind| !evidenced.contains(*kind) && !excused.contains(kind.as_str()))
        .collect();
    assert!(
        blind.is_empty(),
        "{} decoded ACP update kind(s) appear in no promoted capture: {blind:?}. \
         Either capture one, or add it to DECODED_WITHOUT_EVIDENCE with the reason \
         it is carried blind.",
        blind.len()
    );

    let stale: Vec<&str> = excused
        .iter()
        .copied()
        .filter(|kind| evidenced.contains(*kind))
        .collect();
    assert!(
        stale.is_empty(),
        "{} kind(s) in DECODED_WITHOUT_EVIDENCE now have evidence: {stale:?}. Delete \
         the entry — an exemption nobody prunes is where the next gap hides.",
        stale.len()
    );
}

#[test]
fn every_evidenced_acp_update_kind_is_decoded_or_declined() {
    let decoded = decoded_kinds();
    let evidenced = evidenced_kinds();
    let declined: BTreeSet<&str> = UNDECODED_BY_DECISION
        .iter()
        .map(|(kind, _)| *kind)
        .collect();

    for (kind, reason) in UNDECODED_BY_DECISION {
        assert!(
            reason.len() > 40,
            "{kind}'s entry has to say WHY, not just that it was seen: {reason:?}"
        );
    }

    let unhandled: Vec<&String> = evidenced
        .iter()
        .filter(|kind| !decoded.contains(*kind) && !declined.contains(kind.as_str()))
        .collect();
    assert!(
        unhandled.is_empty(),
        "{} ACP update kind(s) appear in the corpus with no decode and no decision: \
         {unhandled:?}. Decode it, or add it to UNDECODED_BY_DECISION with the \
         reason — an agent capability nobody ruled on is exactly what this \
         corpus exists to surface.",
        unhandled.len()
    );

    let vanished: Vec<&str> = declined
        .iter()
        .copied()
        .filter(|kind| !evidenced.contains(*kind))
        .collect();
    assert!(
        vanished.is_empty(),
        "{} kind(s) in UNDECODED_BY_DECISION no longer appear in any capture: \
         {vanished:?}. Either the agent stopped sending it — worth knowing — or the \
         list is stale; both need a person, which is why this fails rather than \
         passing quietly.",
        vanished.len()
    );
}
