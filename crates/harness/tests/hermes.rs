//! `HermesHarness` identity and capability contract.
//!
//! **No fixture-backed end-to-end test here, unlike `tests/grok.rs`.** The
//! `fake-acp` fixture (`crates/harness/tests/fixtures/fake_acp.rs`) sends no
//! `usage` block on `session/prompt` at all today — Grok's own usage decode is
//! likewise only ever pinned at the unit level, never exercised through a live
//! fixture turn — and that fixture belongs to a different branch in this
//! plan (PR5). What is here mirrors the identity/capability half of
//! `tests/grok.rs`'s own `the_harness_identifies_itself_consistently`; the
//! literal-wire-pinned decode tests (launch line, usage split, model shape)
//! live as unit tests inside `crates/harness/src/acp/hermes.rs` instead, the
//! same place Grok's own equivalents live — see that file's test module for
//! why `crates/harness/tests/hermes.rs` cannot reach `grok::usage` or
//! `normalize::usage` at all (`pub(crate)`, and `normalize` is a
//! `pub(crate) mod`).

use comet_harness::Harness;
use comet_harness::acp::hermes::HermesHarness;
use comet_proto::{HarnessId, SteeringMode};

/// The registry's lazy descriptor names `HermesHarness::capabilities()`, so the
/// catalog entry shown before first use must equal what the trait reports after
/// the slot resolves. A drift here shows as a picker row that changes when the
/// user clicks it.
#[test]
fn identity_and_capabilities_do_not_drift() {
    let harness = HermesHarness::new();
    assert_eq!(harness.id(), HarnessId::Hermes);
    assert_eq!(harness.display_name(), "Hermes");
    assert_eq!(harness.capabilities(), HermesHarness::capabilities());
}

/// Break caught: declaring an effort ladder Hermes does not offer. An empty
/// ladder is the honest answer and the traits picker is built to render it;
/// a populated one puts choices on screen that the run silently discards.
#[test]
fn the_effort_ladder_is_empty_because_hermes_has_none() {
    assert!(
        HermesHarness::capabilities().reasoning_levels.is_empty(),
        "Hermes advertises no effort config; a ladder here is a promise the run breaks"
    );
}

/// Break caught: reading an absent `_meta.steering` as StepBoundary. Hermes
/// sends no steering extension, so a steer must be delivered as the next prompt
/// on the same session. Declaring StepBoundary loses the steer silently.
#[test]
fn steering_falls_back_to_the_turn_boundary() {
    let caps = HermesHarness::capabilities();
    assert!(caps.supports_steering);
    assert_eq!(caps.steering_mode, SteeringMode::TurnBoundary);
}
