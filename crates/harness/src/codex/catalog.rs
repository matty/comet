//! Model catalog + effort/sandbox mapping for Codex, ported from comet's
//! `packages/harness/src/codex.ts`.
//!
//! The TS harness discovers models live via the app server's `model/list`
//! (experimentalApi) and falls back to a curated snapshot; here the snapshot IS
//! the catalog, and `CodexHarness::models` is the single seam where a
//! short-lived `codex app-server` + `model/list` pagination can later be
//! spliced in (same call t3code's Codex provider makes).

use comet_proto::{
    Model, ModelOption, ModelOptionChoice, ReasoningLevel, RuntimeMode, SandboxLevel,
};

/// The unified reasoning ladder Codex accepts (`minimal` is offered but clamped
/// on the wire — see [`to_effort`]).
pub(crate) const REASONING_LEVELS: &[ReasoningLevel] = &[
    ReasoningLevel::Minimal,
    ReasoningLevel::Low,
    ReasoningLevel::Medium,
    ReasoningLevel::High,
    ReasoningLevel::XHigh,
    ReasoningLevel::Max,
    ReasoningLevel::Ultra,
];

/// Codex's API rejects `minimal` when default tools (web_search, image_gen)
/// are enabled, and doesn't know Claude's ultracode/ultrathink modes. It DOES
/// accept `max` and `ultra` natively (gpt-5.6+), so those pass straight
/// through — only the levels Codex can't take are clamped to the nearest
/// effort (port of codex.ts `toEffort`).
pub(crate) fn to_effort(reasoning: Option<ReasoningLevel>) -> Option<&'static str> {
    Some(match reasoning? {
        ReasoningLevel::Minimal | ReasoningLevel::Low => "low",
        ReasoningLevel::Medium => "medium",
        ReasoningLevel::High => "high",
        ReasoningLevel::XHigh | ReasoningLevel::Ultracode | ReasoningLevel::Ultrathink => "xhigh",
        ReasoningLevel::Max => "max",
        ReasoningLevel::Ultra => "ultra",
    })
}

/// `thread/start`'s `sandbox` param (kebab-case wire words).
pub(crate) fn sandbox_mode(sandbox: SandboxLevel) -> &'static str {
    match sandbox {
        SandboxLevel::ReadOnly => "read-only",
        SandboxLevel::WorkspaceWrite => "workspace-write",
        SandboxLevel::DangerFullAccess => "danger-full-access",
    }
}

/// `turn/start`'s `sandboxPolicy.type` (camelCase variant of the same policy).
pub(crate) fn sandbox_policy_type(sandbox: SandboxLevel) -> &'static str {
    match sandbox {
        SandboxLevel::ReadOnly => "readOnly",
        SandboxLevel::WorkspaceWrite => "workspaceWrite",
        SandboxLevel::DangerFullAccess => "dangerFullAccess",
    }
}

/// `turn/start`'s full `sandboxPolicy` object. Workspace-write keeps network
/// access: comet agents fetch deps and hit APIs unattended, and with the
/// approval policy pinned to "never" a network-less sandbox would fail those
/// commands with no escalation path.
pub(crate) fn sandbox_policy_value(sandbox: SandboxLevel) -> serde_json::Value {
    let mut policy = serde_json::Map::new();
    policy.insert("type".into(), sandbox_policy_type(sandbox).into());
    if matches!(sandbox, SandboxLevel::WorkspaceWrite) {
        policy.insert("networkAccess".into(), true.into());
    }
    serde_json::Value::Object(policy)
}

/// `thread/start`'s `approvalsReviewer`: who answers an approval when one is
/// raised. Only `Auto` delegates that to the provider; the others keep the
/// user in the role, which is also the app-server's default when the key is
/// absent.
///
/// Sending this is currently inert — the wire approval policy is pinned to
/// `"never"`, so no approval is ever raised for a reviewer to answer. It is
/// sent anyway so the thread carries the mode's full intent, and so the
/// change that unpins the policy changes one literal rather than three.
/// `thread/start`'s and `turn/start`'s `approvalPolicy`: when the server stops
/// to ask.
///
/// **`untrusted` and `on-request` are not two settings on one dial** — captured
/// 2026-08-10, and worth knowing before reading either name as a severity:
///
/// - `untrusted` asks **before every command**, with no `reason`. One turn asked
///   four times for two commands, because each retry is a fresh request.
/// - `on-request` asks **only after a sandboxed attempt has already failed**,
///   carrying a `reason` that says so. The user sees a failed command in the
///   transcript first, then a card offering to run it unsandboxed.
///
/// `never` removes the prompt, which is what `FullAccess` means on the wire.
pub(crate) fn approval_policy(mode: RuntimeMode) -> &'static str {
    match mode {
        RuntimeMode::ApprovalRequired => "untrusted",
        RuntimeMode::AutoAcceptEdits | RuntimeMode::Auto => "on-request",
        RuntimeMode::FullAccess => "never",
    }
}

pub(crate) fn approvals_reviewer(mode: RuntimeMode) -> &'static str {
    match mode {
        RuntimeMode::Auto => "auto_review",
        RuntimeMode::ApprovalRequired | RuntimeMode::AutoAcceptEdits | RuntimeMode::FullAccess => {
            "user"
        }
    }
}

const ULTRA_LADDER: &[ReasoningLevel] = &[
    ReasoningLevel::Low,
    ReasoningLevel::Medium,
    ReasoningLevel::High,
    ReasoningLevel::XHigh,
    ReasoningLevel::Max,
    ReasoningLevel::Ultra,
];

const MAX_LADDER: &[ReasoningLevel] = &[
    ReasoningLevel::Low,
    ReasoningLevel::Medium,
    ReasoningLevel::High,
    ReasoningLevel::XHigh,
    ReasoningLevel::Max,
];

const XHIGH_LADDER: &[ReasoningLevel] = &[
    ReasoningLevel::Low,
    ReasoningLevel::Medium,
    ReasoningLevel::High,
    ReasoningLevel::XHigh,
];

/// The service-tier select the app server reports per model (`serviceTiers` /
/// `additionalSpeedTiers` in `model/list`); "default" means Standard and is
/// omitted from the wire params entirely.
fn service_tier() -> ModelOption {
    ModelOption {
        id: "serviceTier".into(),
        label: "Service Tier".into(),
        choices: vec![
            ModelOptionChoice {
                id: "default".into(),
                label: "Standard".into(),
            },
            ModelOptionChoice {
                id: "fast".into(),
                label: "Fast".into(),
            },
        ],
        default_choice: "default".into(),
    }
}

fn model(id: &str, label: &str, description: &str, ladder: &[ReasoningLevel]) -> Model {
    Model {
        id: id.into(),
        label: label.into(),
        description: (!description.is_empty()).then(|| description.into()),
        reasoning_levels: ladder.to_vec(),
        options: vec![service_tier()],
    }
}

/// The curated catalog: a snapshot of codex-cli 0.144's `model/list`, newest
/// family first — efforts as the server reports them (gpt-5.6 goes up to
/// `ultra`). Mirrors codex.ts's `CODEX_MODELS` fallback.
pub(crate) fn static_models() -> Vec<Model> {
    vec![
        model(
            "gpt-5.6-sol",
            "GPT-5.6-Sol",
            "Frontier reasoning flagship",
            ULTRA_LADDER,
        ),
        model(
            "gpt-5.6-terra",
            "GPT-5.6-Terra",
            "Deep multi-step agentic work",
            ULTRA_LADDER,
        ),
        model(
            "gpt-5.6-luna",
            "GPT-5.6-Luna",
            "Fast frontier model",
            MAX_LADDER,
        ),
        model(
            "gpt-5.5",
            "GPT-5.5",
            "Previous generation flagship",
            XHIGH_LADDER,
        ),
        model(
            "gpt-5.4",
            "GPT-5.4",
            "Reliable general coding",
            XHIGH_LADDER,
        ),
        model(
            "gpt-5.4-mini",
            "GPT-5.4-Mini",
            "Small, fast and capable",
            XHIGH_LADDER,
        ),
        model(
            "gpt-5.3-codex-spark",
            "GPT-5.3-Codex-Spark",
            "Ultra-fast lightweight coding",
            XHIGH_LADDER,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effort_clamps_like_codex_ts() {
        assert_eq!(to_effort(None), None);
        assert_eq!(to_effort(Some(ReasoningLevel::Minimal)), Some("low"));
        assert_eq!(to_effort(Some(ReasoningLevel::Ultracode)), Some("xhigh"));
        assert_eq!(to_effort(Some(ReasoningLevel::Ultrathink)), Some("xhigh"));
        assert_eq!(to_effort(Some(ReasoningLevel::Max)), Some("max"));
        assert_eq!(to_effort(Some(ReasoningLevel::Ultra)), Some("ultra"));
    }

    #[test]
    fn sandbox_maps_both_spellings() {
        assert_eq!(sandbox_mode(SandboxLevel::ReadOnly), "read-only");
        assert_eq!(sandbox_policy_type(SandboxLevel::ReadOnly), "readOnly");
        assert_eq!(
            sandbox_policy_type(SandboxLevel::WorkspaceWrite),
            "workspaceWrite"
        );
        assert_eq!(
            sandbox_mode(SandboxLevel::DangerFullAccess),
            "danger-full-access"
        );
    }

    #[test]
    fn catalog_is_newest_first_with_service_tiers() {
        let models = static_models();
        assert_eq!(models.len(), 7);
        assert_eq!(models[0].id, "gpt-5.6-sol");
        assert!(models[0].reasoning_levels.contains(&ReasoningLevel::Ultra));
        assert!(!models[3].reasoning_levels.contains(&ReasoningLevel::Max));
        for m in &models {
            let tier = m.options.iter().find(|o| o.id == "serviceTier");
            assert!(tier.is_some(), "{} missing serviceTier", m.id);
        }
    }

    /// Every mode maps to a reviewer the app-server's schema accepts. `Auto` is
    /// the only mode that hands review to the provider; the rest keep the human
    /// in the role, which is also the server's own default.
    #[test]
    fn every_runtime_mode_maps_to_a_schema_reviewer() {
        for (mode, want) in [
            (RuntimeMode::ApprovalRequired, "user"),
            (RuntimeMode::AutoAcceptEdits, "user"),
            (RuntimeMode::Auto, "auto_review"),
            (RuntimeMode::FullAccess, "user"),
        ] {
            assert_eq!(approvals_reviewer(mode), want, "{mode:?}");
        }
    }

    /// Every mode maps to a policy literal `AskForApproval` accepts. The pair
    /// that matters is the middle two: they share `on-request`, which asks only
    /// after a sandboxed attempt fails, while `ApprovalRequired` takes
    /// `untrusted`, which asks before every command.
    #[test]
    fn every_runtime_mode_maps_to_a_schema_approval_policy() {
        for (mode, want) in [
            (RuntimeMode::ApprovalRequired, "untrusted"),
            (RuntimeMode::AutoAcceptEdits, "on-request"),
            (RuntimeMode::Auto, "on-request"),
            (RuntimeMode::FullAccess, "never"),
        ] {
            assert_eq!(approval_policy(mode), want, "{mode:?}");
        }
    }
}
