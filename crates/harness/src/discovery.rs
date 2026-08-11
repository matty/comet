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

use comet_proto::{Model, ModelCatalog, ReasoningLevel};

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

/// A discovery answer for the life of the engine boot.
///
/// Holds the FAILURE as well as the success on purpose: a provider that
/// cannot answer (not logged in, CLI missing, protocol changed) would
/// otherwise be re-spawned on every picker open, and every one of those
/// spawns costs a 10-second timeout. The escape hatch is [`DiscoveryCache::clear`], wired to
/// the picker's existing Retry row.
///
/// Callers AWAIT this rather than reading a snapshot. That is what keeps a
/// push channel out of the design: the picker's `Loadable` slot, slow-request
/// toast and Cancel already cover a slow await, whereas a
/// stale-list-then-swap would change the rows under the user's cursor.
type Cell = tokio::sync::OnceCell<Result<Discovery, DiscoveryFailure>>;

#[derive(Debug, Default)]
pub struct DiscoveryCache {
    /// The cell is behind an `Arc` swapped under a std `Mutex` so that
    /// `clear` can take `&self`: the `Harness` trait hands out `&self`
    /// everywhere, and a `&mut self` clear would be uncallable from
    /// `clear_discovery`.
    ///
    /// The lock is released before any await — `get` clones the `Arc` out
    /// and awaits on the clone. Holding a std `MutexGuard` across an await
    /// point is the deadlock this shape exists to avoid.
    cell: std::sync::Mutex<std::sync::Arc<Cell>>,
}

impl DiscoveryCache {
    fn current(&self) -> std::sync::Arc<Cell> {
        self.cell
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Run `run` at most once per boot and hand back its answer. An `Err` is
    /// a cached failure, not "not yet tried" — and it keeps its kind, because
    /// only `Unparseable` earns a `Diagnostic`.
    ///
    /// Returns an owned answer rather than a borrow: the cell it came from
    /// can be swapped out by `clear` at any time, so a reference into it
    /// cannot be handed across that boundary.
    pub async fn get<F, Fut>(&self, run: F) -> Result<Discovery, DiscoveryFailure>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<Discovery, DiscoveryFailure>>,
    {
        let cell = self.current();
        cell.get_or_init(run).await.clone()
    }

    /// Re-arm the cell, so the next `get` runs its closure again. Wired to
    /// the picker's Retry row; it is the only escape from a cached failure
    /// inside one boot.
    pub fn clear(&self) {
        *self
            .cell
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = std::sync::Arc::new(Cell::new());
    }

    /// The cached failure, if this boot's discovery already ran and failed.
    ///
    /// A read-only peek: unlike [`get`], it never runs a discovery, so the
    /// engine can ask "did that call fail, and how" after `models()` returns
    /// without starting one on a cell that was never filled. `None` covers
    /// both "not tried" and "succeeded" — neither is drift.
    pub fn cached_failure(&self) -> Option<DiscoveryFailure> {
        self.current().get().and_then(|r| r.as_ref().err().copied())
    }

    /// The single place `CatalogSource` is decided, so no adapter can report
    /// a built-in list as live.
    pub fn catalog(
        &self,
        curated: Vec<Model>,
        discovery: Result<Discovery, DiscoveryFailure>,
    ) -> ModelCatalog {
        match discovery {
            Ok(discovery) => ModelCatalog::live(merge(curated, &discovery)),
            Err(_) => ModelCatalog::built_in(curated),
        }
    }
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

    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// The subprocess runs once per boot, not once per caller. `models()` is
    /// called from the picker's render path AND from titling
    /// (`crates/engine/src/titles.rs:159`), so a cache that missed would spawn a
    /// CLI on a path the user never sees.
    #[tokio::test]
    async fn discovery_runs_once_across_concurrent_callers() {
        let cache = DiscoveryCache::default();
        let runs = Arc::new(AtomicUsize::new(0));
        let run = || {
            let runs = runs.clone();
            async move {
                runs.fetch_add(1, Ordering::SeqCst);
                tokio::task::yield_now().await;
                Ok(Discovery {
                    models: vec![discovered("m-1", "One")],
                })
            }
        };
        let (a, b) = tokio::join!(cache.get(run), cache.get(run));
        assert!(a.is_ok() && b.is_ok());
        assert_eq!(runs.load(Ordering::SeqCst), 1, "one spawn, two callers");
    }

    /// A failure is cached too. Without this, a broken login spawns a doomed
    /// subprocess on every picker open for the rest of the session.
    #[tokio::test]
    async fn a_failure_is_cached_for_the_boot() {
        let cache = DiscoveryCache::default();
        let runs = Arc::new(AtomicUsize::new(0));
        let run = || {
            let runs = runs.clone();
            async move {
                runs.fetch_add(1, Ordering::SeqCst);
                Err(DiscoveryFailure::Unreachable)
            }
        };
        assert_eq!(cache.get(run).await, Err(DiscoveryFailure::Unreachable));
        assert_eq!(cache.get(run).await, Err(DiscoveryFailure::Unreachable));
        assert_eq!(runs.load(Ordering::SeqCst), 1, "the failure is remembered");
    }

    /// The kind survives the cache. Only `Unparseable` raises a `Diagnostic`,
    /// so a cache that flattened both kinds would silently stop reporting the
    /// one failure that means a provider changed its protocol.
    #[tokio::test]
    async fn the_failure_kind_survives_caching() {
        let cache = DiscoveryCache::default();
        let answer = cache
            .get(|| async { Err(DiscoveryFailure::Unparseable) })
            .await;
        assert_eq!(answer, Err(DiscoveryFailure::Unparseable));
        let again = cache
            .get(|| async { Err(DiscoveryFailure::Unreachable) })
            .await;
        assert_eq!(
            again,
            Err(DiscoveryFailure::Unparseable),
            "the cached kind wins; the closure must not run again"
        );
    }

    /// Retry is the only escape hatch from a cached failure, so clearing has to
    /// actually re-arm the cell.
    #[tokio::test]
    async fn clearing_re_arms_a_cached_failure() {
        let cache = DiscoveryCache::default();
        let runs = Arc::new(AtomicUsize::new(0));
        let run = || {
            let runs = runs.clone();
            async move {
                runs.fetch_add(1, Ordering::SeqCst);
                Err(DiscoveryFailure::Unreachable)
            }
        };
        assert!(cache.get(run).await.is_err());
        cache.clear();
        assert!(cache.get(run).await.is_err());
        assert_eq!(runs.load(Ordering::SeqCst), 2);
    }

    /// The peek must never itself trigger a discovery: an empty cell reads
    /// `None`, not a run of anything.
    #[test]
    fn cached_failure_is_none_before_any_discovery_runs() {
        let cache = DiscoveryCache::default();
        assert_eq!(cache.cached_failure(), None);
    }

    /// The kind reaches the peek unchanged, so the engine can tell
    /// `Unparseable` (drift) from `Unreachable` (ordinary) after the fact.
    #[tokio::test]
    async fn cached_failure_reports_the_cached_kind() {
        let cache = DiscoveryCache::default();
        cache
            .get(|| async { Err(DiscoveryFailure::Unparseable) })
            .await
            .ok();
        assert_eq!(cache.cached_failure(), Some(DiscoveryFailure::Unparseable));
    }

    /// A success is not a failure to report — `None` covers it too.
    #[tokio::test]
    async fn cached_failure_is_none_after_a_success() {
        let cache = DiscoveryCache::default();
        cache
            .get(|| async {
                Ok(Discovery {
                    models: vec![discovered("m-1", "One")],
                })
            })
            .await
            .ok();
        assert_eq!(cache.cached_failure(), None);
    }

    /// The caption's whole input. A failed discovery still answers with a
    /// working list — it is just the built-in one, and the picker says so.
    #[test]
    fn source_reports_built_in_when_discovery_failed() {
        let cache = DiscoveryCache::default();
        let answer = Discovery {
            models: vec![discovered("m-2", "Live")],
        };
        let live = cache.catalog(vec![curated("m-1", "Curated", &[])], Ok(answer));
        assert_eq!(live.source, comet_proto::CatalogSource::Live);
        assert_eq!(live.models.len(), 2);

        let failed = cache.catalog(
            vec![curated("m-1", "Curated", &[])],
            Err(DiscoveryFailure::Unreachable),
        );
        assert_eq!(failed.source, comet_proto::CatalogSource::BuiltIn);
        assert_eq!(failed.models.len(), 1, "the curated list still works");
    }
}
