//! Live model discovery: the provider-agnostic half.
//!
//! Two rules live here, and both were decided in
//! `specs/2026-08-11-phase-2-discovery.md` rather than in an adapter:
//!
//! 1. **Union, never replacement.** The provider decides what is NEW; it does
//!    not get to delete a curated model. A flaky or partial answer would
//!    otherwise silently remove a model the user is mid-session with.
//! 2. **Curated capability wins on a matched id.** Both providers under-report
//!    what a model can do — Claude's `supportedEffortLevels` tops out at `max`
//!    and never mentions `ultracode`/`ultrathink`/`ultra`, and neither provider
//!    reports Comet's option sets (context window, fast mode) at all.

use comet_proto::{Model, ReasoningLevel};

/// One model as a provider described it. Deliberately narrower than
/// [`Model`]: it holds only what a provider can actually tell us, so an
/// adapter cannot accidentally invent an option set.
#[derive(Debug, Clone, PartialEq)]
pub struct DiscoveredModel {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
    /// Efforts the provider reported. Only consulted for a model with no
    /// curated entry.
    pub reasoning_levels: Vec<ReasoningLevel>,
    /// `None` = the provider did not say. NOT "no images" — see
    /// `.agents/rules/optional-wire-fields.md`.
    pub accepts_images: Option<bool>,
}

/// A provider's discovery answer.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Discovery {
    pub models: Vec<DiscoveredModel>,
}

/// Why a discovery answer is missing. Both render the SAME caption, because
/// the user's action is identical either way — use the built-in list, maybe
/// fix the install or the login. The split exists for the other reader:
/// only `Unparseable` means a provider changed its protocol under us, which
/// is exactly what slice 0b.2's `Diagnostic` channel was built to surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoveryFailure {
    /// No answer at all: spawn failed, the handshake timed out, the child
    /// died. Ordinary and usually the user's environment.
    Unreachable,
    /// The provider answered and we could not read it. Not ordinary.
    Unparseable,
}

/// Reasoning levels Comet layers on top of a provider rather than reading
/// from one: `ultrathink` is a prompt prefix, `ultracode` an `xhigh` plus a
/// setting, `ultra` a Codex-only tier. A model nobody has curated must not be
/// offered any of them.
fn is_comet_special(level: ReasoningLevel) -> bool {
    matches!(
        level,
        ReasoningLevel::Ultracode | ReasoningLevel::Ultrathink | ReasoningLevel::Ultra
    )
}

/// Merge a discovery answer over the curated catalog. Curated order first,
/// then live-only models in the order the provider listed them.
pub fn merge(curated: Vec<Model>, discovery: &Discovery) -> Vec<Model> {
    let mut merged: Vec<Model> = curated
        .into_iter()
        .map(|mut model| {
            if let Some(live) = discovery.models.iter().find(|d| d.id == model.id)
                && let Some(accepts) = live.accepts_images
            {
                model.accepts_images = accepts;
            }
            model
        })
        .collect();

    for live in &discovery.models {
        if merged.iter().any(|m| m.id == live.id) {
            continue;
        }
        merged.push(Model {
            id: live.id.clone(),
            label: live.label.clone(),
            description: live.description.clone(),
            reasoning_levels: live
                .reasoning_levels
                .iter()
                .copied()
                .filter(|l| !is_comet_special(*l))
                .collect(),
            options: Vec::new(),
            accepts_images: live.accepts_images.unwrap_or(true),
        });
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use comet_proto::{Model, ModelOption, ModelOptionChoice, ReasoningLevel};

    fn curated(id: &str, label: &str, levels: &[ReasoningLevel]) -> Model {
        Model {
            id: id.into(),
            label: label.into(),
            description: Some("curated".into()),
            reasoning_levels: levels.to_vec(),
            options: vec![ModelOption {
                id: "contextWindow".into(),
                label: "Context Window".into(),
                choices: vec![ModelOptionChoice {
                    id: "200k".into(),
                    label: "200K".into(),
                }],
                default_choice: "200k".into(),
            }],
            accepts_images: true,
        }
    }

    fn discovered(id: &str, label: &str) -> DiscoveredModel {
        DiscoveredModel {
            id: id.into(),
            label: label.into(),
            description: Some("live".into()),
            reasoning_levels: vec![ReasoningLevel::Low, ReasoningLevel::High],
            accepts_images: None,
        }
    }

    /// The provider knows what exists; the catalog knows what it can do. A
    /// matched id keeps every curated capability, because both providers
    /// under-report ladders and neither reports Comet's option sets at all.
    #[test]
    fn matched_id_keeps_curated_capability() {
        let merged = merge(
            vec![curated(
                "m-1",
                "Curated Label",
                &[ReasoningLevel::Ultrathink, ReasoningLevel::Max],
            )],
            &Discovery {
                models: vec![discovered("m-1", "Live Label")],
            },
        );
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].label, "Curated Label");
        assert_eq!(merged[0].description.as_deref(), Some("curated"));
        assert_eq!(
            merged[0].reasoning_levels,
            vec![ReasoningLevel::Ultrathink, ReasoningLevel::Max]
        );
        assert_eq!(merged[0].options.len(), 1, "curated options survive");
    }

    /// The point of the phase: a model shipped after this build appears
    /// without anyone editing Rust.
    #[test]
    fn live_only_model_appears_with_its_reported_ladder() {
        let merged = merge(
            vec![],
            &Discovery {
                models: vec![discovered("m-new", "Brand New")],
            },
        );
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].id, "m-new");
        assert_eq!(merged[0].label, "Brand New");
        assert_eq!(
            merged[0].reasoning_levels,
            vec![ReasoningLevel::Low, ReasoningLevel::High]
        );
        assert!(merged[0].options.is_empty(), "no options may be invented");
    }

    /// Comet's specials are prompt conventions and CLI-version-gated flags,
    /// not something to hand a model nobody has curated.
    #[test]
    fn live_only_model_never_acquires_curated_specials() {
        let mut live = discovered("m-new", "Brand New");
        live.reasoning_levels = vec![ReasoningLevel::Ultracode, ReasoningLevel::Ultrathink];
        let merged = merge(vec![], &Discovery { models: vec![live] });
        assert!(
            merged[0].reasoning_levels.is_empty(),
            "specials must be filtered out, got {:?}",
            merged[0].reasoning_levels
        );
    }

    /// Union, not replacement. A partial or degraded discovery answer must
    /// not delete a model the user is working with; a provider that has
    /// genuinely retired one fails loudly at run time instead, which is
    /// attributable in a way a vanished row is not.
    #[test]
    fn curated_model_absent_from_the_live_list_is_kept() {
        let merged = merge(
            vec![curated("m-1", "Curated", &[ReasoningLevel::Max])],
            &Discovery {
                models: vec![discovered("m-2", "Other")],
            },
        );
        let ids: Vec<&str> = merged.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["m-1", "m-2"], "curated first, then live-only");
    }

    /// Absent modality is not a value. Only an explicit `false` closes the
    /// gate; `None` leaves the curated flag alone.
    #[test]
    fn absent_modality_leaves_the_curated_flag_alone() {
        let merged = merge(
            vec![curated("m-1", "Curated", &[])],
            &Discovery {
                models: vec![discovered("m-1", "Live")],
            },
        );
        assert!(merged[0].accepts_images, "None must not close the gate");
    }

    #[test]
    fn explicit_modality_overrides_the_curated_flag() {
        let mut live = discovered("m-1", "Live");
        live.accepts_images = Some(false);
        let merged = merge(
            vec![curated("m-1", "Curated", &[])],
            &Discovery { models: vec![live] },
        );
        assert!(!merged[0].accepts_images);
    }
}
