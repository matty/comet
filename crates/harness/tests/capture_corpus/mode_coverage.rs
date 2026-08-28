//! Coverage gate: every `RuntimeMode` variant must have at least one scenario per provider in
//! `SCENARIOS`. Mirrors `scenario_coverage.rs`'s shape (a required set, checked in both
//! directions) applied to *modes* rather than scenario names — but unlike that gate, there is no
//! evidence-vs-declaration split here: a scenario row IS the coverage this gate wants, so there is
//! no named-exemption list to keep honest, only the required set itself.
//!
//! Comet ships four `RuntimeMode` variants (`crates/proto/src/agent.rs`). Before the stage-6
//! `auto`/`full-access` scenario rows landed, the table exercised only two of them — `Auto` and
//! `FullAccess` had never been captured for either provider, and nothing noticed. This gate makes
//! that finding permanent.

use comet_harness::capture::{Provider, SCENARIOS};
use comet_proto::RuntimeMode;

/// Every `RuntimeMode` variant, named through an exhaustive match rather than a plain literal
/// list. Break caught: a fifth variant added to the enum with no arm added here fails this file
/// to **compile** — a non-exhaustive-match error naming the new variant — rather than silently
/// missing it in a runtime comparison, which is the failure mode a hand-maintained `&[...]` array
/// would have.
fn all_modes() -> Vec<RuntimeMode> {
    fn exhaustive(mode: RuntimeMode) -> RuntimeMode {
        match mode {
            RuntimeMode::ApprovalRequired
            | RuntimeMode::AutoAcceptEdits
            | RuntimeMode::Auto
            | RuntimeMode::FullAccess => mode,
        }
    }
    [
        RuntimeMode::ApprovalRequired,
        RuntimeMode::AutoAcceptEdits,
        RuntimeMode::Auto,
        RuntimeMode::FullAccess,
    ]
    .into_iter()
    .map(exhaustive)
    .collect()
}

fn provider_str(provider: Provider) -> &'static str {
    match provider {
        Provider::Claude => "claude",
        Provider::Codex => "codex",
        Provider::Acp => "acp",
    }
}

/// Break caught: a `RuntimeMode` Comet can put on the wire with no scenario recording it, for one
/// or both providers — `Auto` and `FullAccess` were both this, for both providers, before this
/// stage's four new rows landed.
#[test]
fn every_runtime_mode_has_a_scenario_per_provider() {
    let declared: Vec<(&str, RuntimeMode)> = SCENARIOS
        .iter()
        .filter_map(|spec| {
            spec.runtime_mode
                .map(|mode| (provider_str(spec.provider), mode))
        })
        .collect();

    let mut missing = Vec::new();
    for provider in [Provider::Claude, Provider::Codex] {
        for mode in all_modes() {
            let key = (provider_str(provider), mode);
            if !declared.contains(&key) {
                missing.push(format!("{}/{:?}", key.0, key.1));
            }
        }
    }

    assert!(
        missing.is_empty(),
        "{} (provider, mode) pair(s) have no scenario anywhere in SCENARIOS:\n{}",
        missing.len(),
        missing.join("\n")
    );
}
