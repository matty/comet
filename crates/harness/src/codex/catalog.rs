//! Model catalog + effort/sandbox mapping for Codex, ported from comet's
//! `packages/harness/src/codex.ts`.
//!
//! The TS harness discovers models live via the app server's `model/list`
//! (experimentalApi) and falls back to a curated snapshot; here the snapshot IS
//! the catalog, and `CodexHarness::models` is the single seam where a
//! short-lived `codex app-server` + `model/list` pagination can later be
//! spliced in (same call t3code's Codex provider makes).

use comet_proto::{
    Model, ModelCatalog, ModelOption, ModelOptionChoice, ReasoningLevel, RuntimeMode, SandboxLevel,
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

/// `thread/start`'s and `turn/start`'s `approvalPolicy`: when the server stops
/// to ask.
///
/// **`untrusted` and `on-request` are not two settings on one dial.** In the
/// current protocol they control different approval phases:
///
/// - `untrusted` can run bounded commands under the read-only sandbox and ask
///   the user when the turn reaches a file change.
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

/// `thread/start`'s `approvalsReviewer`: who answers an approval when one is
/// raised. Only `Auto` delegates that to the provider; the others keep the user
/// in the role, which is also the app-server's default when the key is absent.
///
/// This used to carry a note calling itself inert, because the wire approval
/// policy was pinned at `"never"` and no approval could be raised for a
/// reviewer to answer. **Slice 1.7 unpinned it** — see `approval_policy` above,
/// which now derives all four modes — so the field has been live since then.
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
        deprecation: None,
        reasoning_levels: ladder.to_vec(),
        options: vec![service_tier()],
        accepts_images: true,
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

/// Lead the merged catalog with the model the live `model/list` reply itself
/// called default, rather than leaving `pickers::default_model` (which just
/// returns `models.first()`) reading catalog order — coincidentally correct
/// only because the curated list happens to lead with the same flagship the
/// server does today (D72, `docs/debt/README.md`).
///
/// `default_id: None` — no discovery ran, or none of its rows claimed
/// `isDefault` — is a no-op: catalog order is exactly what today ships, and an
/// absent claim must not be read as "the first row is the pick"
/// (`.agents/rules/optional-wire-fields.md`). An id that names no model in
/// `catalog` (hidden, or dropped some other way) is also a no-op rather than
/// a panic — the picker still gets a complete, merely unreordered, list.
pub(crate) fn order_by_live_default(
    mut catalog: ModelCatalog,
    default_id: Option<&str>,
) -> ModelCatalog {
    let Some(default_id) = default_id else {
        return catalog;
    };
    if let Some(pos) = catalog.models.iter().position(|m| m.id == default_id)
        && pos != 0
    {
        let model = catalog.models.remove(pos);
        catalog.models.insert(0, model);
    }
    catalog
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

    /// The behaviour D72 exists to fix: a live default that is not the
    /// curated flagship must lead the merged catalog, so
    /// `pickers::default_model`'s `models.first()` picks it up.
    #[test]
    fn a_live_default_that_is_not_first_is_promoted() {
        let catalog = ModelCatalog::live(static_models());
        assert_eq!(
            catalog.models[0].id, "gpt-5.6-sol",
            "curated order, unreordered"
        );
        let reordered = order_by_live_default(catalog, Some("gpt-5.5"));
        assert_eq!(reordered.models[0].id, "gpt-5.5");
        let ids: std::collections::HashSet<&str> =
            reordered.models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids.len(), 7, "reordering must not drop or duplicate a row");
    }

    /// No row claimed the default: today's whole behaviour, catalog order,
    /// must be exactly preserved. Reading the absence as "the first row is
    /// the pick" is the trap `.agents/rules/optional-wire-fields.md` names.
    #[test]
    fn no_live_default_leaves_catalog_order_untouched() {
        let catalog = ModelCatalog::live(static_models());
        let ids_before: Vec<String> = catalog.models.iter().map(|m| m.id.clone()).collect();
        let untouched = order_by_live_default(catalog, None);
        let ids_after: Vec<String> = untouched.models.iter().map(|m| m.id.clone()).collect();
        assert_eq!(ids_before, ids_after);
    }

    /// An id that names no row in the merged catalog (e.g. the default was a
    /// hidden model, dropped before merging) must not panic or drop rows —
    /// it is a no-op, not an error.
    #[test]
    fn an_unmatched_default_id_is_a_no_op() {
        let catalog = ModelCatalog::live(static_models());
        let ids_before: Vec<String> = catalog.models.iter().map(|m| m.id.clone()).collect();
        let untouched = order_by_live_default(catalog, Some("no-such-model"));
        let ids_after: Vec<String> = untouched.models.iter().map(|m| m.id.clone()).collect();
        assert_eq!(ids_before, ids_after);
    }

    /// A live default that already leads the catalog (today's coincidence) is
    /// also a no-op, not a needless remove-and-reinsert.
    #[test]
    fn a_live_default_already_first_is_a_no_op() {
        let catalog = ModelCatalog::live(static_models());
        let reordered = order_by_live_default(catalog, Some("gpt-5.6-sol"));
        assert_eq!(reordered.models[0].id, "gpt-5.6-sol");
        assert_eq!(reordered.models.len(), 7);
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
    /// that matters is the middle two: they share `on-request`, which can ask
    /// after a sandboxed attempt fails, while `ApprovalRequired` uses
    /// `untrusted` with the user reviewer and a read-only sandbox.
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
