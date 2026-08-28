//! `GrokHarness` end to end against the `fake-acp` fixture.
//!
//! The unit tests in `acp/grok.rs` pin the decode against literal wire; these
//! prove the harness actually reaches it — spawn, handshake, `session/new`,
//! catalog — which is the path the model picker runs on.

use std::path::PathBuf;
use std::time::Duration;

use comet_harness::Harness;
use comet_harness::acp::grok::GrokHarness;
use comet_harness::acp::session::Timeouts;
use comet_proto::{CatalogSource, HarnessId, ReasoningLevel};

/// Short enough that a hung probe fails the suite instead of stalling it.
const TEST_TIMEOUTS: Timeouts = Timeouts {
    handshake: Duration::from_secs(10),
    cancel_grace: Duration::from_millis(750),
    kill_grace: Duration::from_millis(250),
};

/// A harness pointed at the fixture, with the vendor config surface on.
fn against_fixture() -> GrokHarness {
    // SAFETY: the fixture reads this at ITS startup, in a child process. Set
    // here because `with_executable` carries no env, and the value is the same
    // for every test in this binary.
    unsafe { std::env::set_var("FAKE_ACP_SESSION_CONFIG", "grok") };
    GrokHarness::new()
        .with_executable(env!("CARGO_BIN_EXE_fake-acp"))
        .with_timeouts(TEST_TIMEOUTS)
}

/// A path that resolves (so the override is honored) but cannot be spawned.
fn missing_binary() -> PathBuf {
    std::env::temp_dir().join("comet-grok-does-not-exist")
}

#[tokio::test]
async fn the_picker_gets_a_live_catalog_from_the_agent() {
    let catalog = against_fixture().models().await.expect("models resolve");

    assert_eq!(
        catalog.source,
        CatalogSource::Live,
        "the agent answered, so this is not the built-in list"
    );

    // The catalog is the curated list UNIONED with what was discovered, so the
    // curated `grok-4.6` leads and the fixture's models follow.
    let ids: Vec<&str> = catalog.models.iter().map(|m| m.id.as_str()).collect();
    assert!(
        ids.contains(&"fake-model") && ids.contains(&"fake-mini"),
        "{ids:?}"
    );

    // **Two discovered models, not four.** The fixture's `availableModels`
    // carries four rows — model x effort — while its config surface names two.
    // Reading the deprecated surface as the list is the failure this asserts
    // against, and it is invisible on the real Grok, which has only one model.
    for combinatorial in ["fake-model-low", "fake-mini-low"] {
        assert!(
            !ids.contains(&combinatorial),
            "a model x effort row reached the picker as a model: {ids:?}"
        );
    }

    let discovered = catalog
        .models
        .iter()
        .find(|m| m.id == "fake-model")
        .expect("the discovered model is in the catalog");
    assert_eq!(discovered.label, "Fake Model");
    assert_eq!(
        discovered.description.as_deref(),
        Some("the fixture's model"),
        "the description is joined on from availableModels, which the config rows lack"
    );
    assert_eq!(
        discovered.reasoning_levels,
        vec![ReasoningLevel::High, ReasoningLevel::Low],
        "the ladder comes off `category: \"mode\"`, in the order the agent listed it"
    );
}

/// **A discovery failure must not degrade to an empty list.** The picker would
/// then say "this agent has no models" when the truth is "we could not reach
/// Grok" — the confident-wrong-answer shape this repository has hit twice.
#[tokio::test]
async fn an_unreachable_agent_falls_back_to_a_non_empty_built_in_list() {
    let harness = GrokHarness::new()
        .with_executable(missing_binary())
        .with_timeouts(TEST_TIMEOUTS);

    let catalog = harness
        .models()
        .await
        .expect("an unreachable agent still answers with a catalog");

    assert_eq!(catalog.source, CatalogSource::BuiltIn);
    assert!(
        !catalog.models.is_empty(),
        "the built-in list is what makes the failure survivable"
    );
    assert!(catalog.models.iter().any(|m| m.id == "grok-4.6"));
}

/// The failure is reported once and only once — the cached attempt survives the
/// whole boot, so a second `models()` must not re-raise it as fresh drift.
#[tokio::test]
async fn a_discovery_failure_is_reported_at_most_once() {
    let harness = GrokHarness::new()
        .with_executable(missing_binary())
        .with_timeouts(TEST_TIMEOUTS);

    harness.models().await.expect("first call answers");
    assert!(
        harness.take_unreported_discovery_failure().is_some(),
        "the first failure is reported"
    );
    assert!(
        harness.take_unreported_discovery_failure().is_none(),
        "the same failure must not be reported twice"
    );

    // Retry re-arms it: the picker's Retry row is the only escape from a
    // cached failure inside one boot.
    harness.clear_discovery();
    harness.models().await.expect("second call answers");
    assert!(
        harness.take_unreported_discovery_failure().is_some(),
        "a failure that recurs after a retry is reported again"
    );
}

/// **The user never sees a raw technical error.** An unusable CLI has to become
/// a short summary plus an actionable hint, per
/// `.agents/rules/user-facing-errors.md` — not an `io::Error` on screen.
#[tokio::test]
async fn an_unusable_cli_probes_into_something_actionable() {
    let harness = GrokHarness::new()
        .with_executable(missing_binary())
        .with_timeouts(TEST_TIMEOUTS);

    let probe = harness.probe().await;
    let summary = probe
        .availability
        .unavailable_summary()
        .expect("an unusable CLI is unavailable, with a summary");

    assert!(!summary.is_empty());
    assert!(
        summary.len() <= 60,
        "the summary rides a narrow rail; got {} chars: {summary}",
        summary.len()
    );
    for leak in ["os error", "No such file", "Error {", "io:"] {
        assert!(
            !summary.contains(leak),
            "raw technical detail reached the summary: {summary}"
        );
    }

    let hint = probe
        .availability
        .unavailable_hint()
        .expect("an unusable CLI has something to suggest");
    assert!(
        hint.contains("comet-grok-does-not-exist"),
        "the hint must name WHICH binary failed — with an override set, that is          the whole diagnosis: {hint}"
    );

    // **The hint is NOT leak-checked, and that is a recorded gap, not an
    // oversight.** `probe_cli_version` interpolates the raw `io::Error` and the
    // CLI's own stderr into it (`lib.rs`, the "could not be started" and
    // "`--version` failed" arms), so this hint really does carry "(os error 2)".
    // That contradicts `.agents/rules/user-facing-errors.md` rule 1, and
    // `probe_tests::a_failing_cli_reports_its_own_error` asserts the behavior
    // deliberately. It is shared by claude and codex alike and predates Grok, so
    // it is not this harness's to change — see the debt row.
}

#[tokio::test]
async fn the_harness_identifies_itself_consistently() {
    let harness = GrokHarness::new();
    assert_eq!(harness.id(), HarnessId::Grok);
    assert_eq!(harness.display_name(), "Grok");
    // The registry's lazy descriptor names this same associated function, so a
    // drift between the two is unrepresentable rather than merely tested for.
    assert_eq!(harness.capabilities(), GrokHarness::capabilities());
}
