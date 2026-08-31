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

use comet_proto::{AgentCommand, Model, ModelCatalog, ModelDeprecation, ReasoningLevel};

/// One model as a provider described it. Deliberately narrower than
/// [`Model`]: it holds only what a provider can actually tell us, so an
/// adapter cannot accidentally invent an option set.
#[derive(Debug, Clone, PartialEq)]
pub struct DiscoveredModel {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
    /// Retirement advice reported by the provider. `None` means the provider
    /// said nothing, not that the model is known to be permanent.
    pub deprecation: Option<ModelDeprecation>,
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
/// from one: `ultrathink` is a prompt prefix and `ultracode` an `xhigh` plus a
/// setting. A model nobody has curated must not be offered either.
///
/// **`ultra` is deliberately not on this list**, though it was until slice 2.3.
/// Codex reports it in `supportedReasoningEfforts` on gpt-5.6+ (capture
/// `2026-08-11-codex-model-list.md`) and `codex/catalog.rs`'s `to_effort`
/// already sends it on the wire, so it is provider-reported, not
/// Comet-layered. Filtering it stripped the top effort off exactly the models
/// this phase exists to surface: the ones no one has curated yet. Claude is
/// unaffected — its ladder tops out at `max`.
fn is_comet_special(level: ReasoningLevel) -> bool {
    matches!(
        level,
        ReasoningLevel::Ultracode | ReasoningLevel::Ultrathink
    )
}

/// The path to hand `Command::new` when the child's working directory is about
/// to be changed.
///
/// `current_dir` changes what a **relative** program path resolves against, and
/// std documents that as platform-specific and unstable — on Unix the child
/// chdirs before exec, so `./bin/claude` would be looked for under the temp
/// directory and the spawn would fail into a silent built-in-list fallback.
/// Both `CLAUDE_CODE_EXECUTABLE` and `CODEX_EXECUTABLE` are taken verbatim
/// (`lib.rs:161-163`), so a relative override is a real user configuration, not
/// a hypothetical.
///
/// Two cases are deliberately left alone. An absolute path is returned
/// unchanged rather than canonicalized, because on Windows canonicalization
/// rewrites it to a `\\?\` verbatim path, which would then land in the child's
/// PATH via `compose_child_path`. A bare command name (`codex`, no separator)
/// stays bare, because that is a PATH lookup — absolutizing it against the
/// parent's cwd would prefer a stray `./codex` over the installed CLI.
pub(crate) fn program_path(exe: &std::path::Path) -> std::path::PathBuf {
    if exe.is_absolute() || exe.components().count() < 2 {
        return exe.to_path_buf();
    }
    // A path that cannot be resolved is left as it came: failing the spawn on
    // the CLI's own terms beats substituting a path nobody configured.
    std::fs::canonicalize(exe).unwrap_or_else(|_| exe.to_path_buf())
}

/// Merge a discovery answer over the curated catalog. Curated order first,
/// then live-only models in the order the provider listed them.
pub fn merge(curated: Vec<Model>, discovery: &Discovery) -> Vec<Model> {
    let mut merged: Vec<Model> = curated
        .into_iter()
        .map(|mut model| {
            if let Some(live) = discovery.models.iter().find(|d| d.id == model.id) {
                if let Some(accepts) = live.accepts_images {
                    model.accepts_images = accepts;
                }
                model.deprecation = live.deprecation.clone();
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
            deprecation: live.deprecation.clone(),
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

/// One attempt at discovery, plus whether its failure has been reported.
///
/// The two travel together so a failure is reported exactly once per attempt.
/// Kept apart, the count on the diagnostics card would climb every time
/// anything re-read the cached failure — one unreadable answer rendering as
/// dozens of protocol failures with a refreshed timestamp.
#[derive(Debug, Default)]
struct Attempt {
    cell: std::sync::Arc<Cell>,
    failure_reported: bool,
}

#[derive(Debug, Default)]
pub struct DiscoveryCache {
    /// The attempt is behind a std `Mutex` and its cell behind an `Arc` so
    /// that `clear` can take `&self`: the `Harness` trait hands out `&self`
    /// everywhere, and a `&mut self` clear would be uncallable from
    /// `clear_discovery`.
    ///
    /// The lock is released before any await — `get` clones the `Arc` out
    /// and awaits on the clone. Holding a std `MutexGuard` across an await
    /// point is the deadlock this shape exists to avoid.
    attempt: std::sync::Mutex<Attempt>,
}

impl DiscoveryCache {
    fn lock(&self) -> std::sync::MutexGuard<'_, Attempt> {
        self.attempt
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn current(&self) -> std::sync::Arc<Cell> {
        self.lock().cell.clone()
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

    /// Re-arm the cache, so the next `get` runs its closure again. Wired to
    /// the picker's Retry row; it is the only escape from a cached failure
    /// inside one boot. The new attempt starts unreported, so a failure that
    /// recurs is reported again.
    pub fn clear(&self) {
        *self.lock() = Attempt::default();
    }

    /// The cached failure, if this attempt failed and nobody has reported it
    /// yet. Reports it at most once, then answers `None` for that attempt.
    ///
    /// Never runs a discovery, unlike [`get`], so the engine can ask "did that
    /// fail, and how" after `models()` returns without starting one on a cell
    /// nothing filled. `None` covers "not tried", "succeeded", and "already
    /// reported" — none of the three is a fresh drift signal.
    ///
    /// The check and the mark happen under one lock, so two concurrent callers
    /// cannot both report the same failure. One residual race is accepted and
    /// not worth more machinery: a forced retry that lands between another
    /// caller's `get` and its report swaps in a fresh attempt, and the older
    /// caller then sees nothing to report. That only loses the signal when the
    /// retry it raced *succeeded*, which means the drift is already over.
    pub fn take_unreported_failure(&self) -> Option<DiscoveryFailure> {
        let mut attempt = self.lock();
        if attempt.failure_reported {
            return None;
        }
        let failure = attempt.cell.get().and_then(|r| r.as_ref().err().copied())?;
        attempt.failure_reported = true;
        Some(failure)
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

/// One directory's command list for the life of the engine boot.
///
/// The same shape as [`DiscoveryCache`] — one attempt, failures cached, `&self`
/// throughout — with one difference that is the whole reason it is a separate
/// type: **commands are cwd-scoped and models are not.** A single cell would
/// serve one directory's project skills to every other directory, which is
/// exactly the wrong answer rather than a missing one (debt row D32).
///
/// There is no `take_unreported_failure` twin here. A command list that cannot
/// be read raises no `Diagnostic`: the menu says so on screen, where the user
/// who typed `/` is already looking, and a provider that answers models but not
/// commands is not the protocol-drift signal 0b.2 built that channel for.
/// One directory's attempt. Named for the same reason [`Cell`] is: the nested
/// generic is unreadable inline, and clippy calls it out.
type CommandCell = tokio::sync::OnceCell<Result<Vec<AgentCommand>, DiscoveryFailure>>;

#[derive(Debug, Default)]
pub struct CommandCache {
    /// Same `Arc`-under-a-std-`Mutex` shape as `DiscoveryCache`, and for the
    /// same reason: the cell is cloned out before any await, because holding a
    /// std guard across an await point deadlocks.
    cells: std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<CommandCell>>>,
}

impl CommandCache {
    /// Run `run` at most once per directory per boot.
    pub async fn get<F, Fut>(
        &self,
        cwd: &str,
        run: F,
    ) -> Result<Vec<AgentCommand>, DiscoveryFailure>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<Vec<AgentCommand>, DiscoveryFailure>>,
    {
        let cell = {
            let mut cells = self
                .cells
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            cells.entry(cwd.to_owned()).or_default().clone()
        };
        cell.get_or_init(run).await.clone()
    }

    /// Drop every directory's answer, so the next `get` runs again. Wired to
    /// the same Retry the model catalog uses.
    pub fn clear(&self) {
        self.cells
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
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
            deprecation: None,
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
            deprecation: None,
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

    /// Live retirement metadata is advisory state, not a capability. A
    /// matched curated row must receive it or the common union path drops the
    /// warning for exactly the models Comet already knows about.
    #[test]
    fn matched_id_receives_live_deprecation_guidance() {
        let mut live = discovered("m-1", "Live Label");
        live.deprecation = Some(ModelDeprecation {
            replacement: Some("m-2".into()),
            migration_markdown: Some("Move soon".into()),
        });
        let merged = merge(
            vec![curated("m-1", "Curated", &[ReasoningLevel::High])],
            &Discovery { models: vec![live] },
        );
        assert_eq!(
            merged[0]
                .deprecation
                .as_ref()
                .and_then(|d| d.replacement.as_deref()),
            Some("m-2")
        );
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

    /// The read must never itself trigger a discovery: an empty cell reads
    /// `None`, not a run of anything.
    #[test]
    fn no_failure_to_report_before_any_discovery_runs() {
        let cache = DiscoveryCache::default();
        assert_eq!(cache.take_unreported_failure(), None);
    }

    /// The kind survives, so the engine can tell `Unparseable` (drift) from
    /// `Unreachable` (ordinary) after the fact.
    #[tokio::test]
    async fn the_failure_kind_reaches_the_reader() {
        let cache = DiscoveryCache::default();
        cache
            .get(|| async { Err(DiscoveryFailure::Unparseable) })
            .await
            .ok();
        assert_eq!(
            cache.take_unreported_failure(),
            Some(DiscoveryFailure::Unparseable)
        );
    }

    /// A success is not a failure to report — `None` covers it too.
    #[tokio::test]
    async fn no_failure_to_report_after_a_success() {
        let cache = DiscoveryCache::default();
        cache
            .get(|| async {
                Ok(Discovery {
                    models: vec![discovered("m-1", "One")],
                })
            })
            .await
            .ok();
        assert_eq!(cache.take_unreported_failure(), None);
    }

    /// One unreadable answer is ONE signal. The failure stays cached for the
    /// whole boot, so a reader that answered every time would turn a single
    /// event into a climbing count with a refreshed timestamp — a provider
    /// that failed once reading as one that keeps failing.
    #[tokio::test]
    async fn a_failure_is_reported_once_per_attempt() {
        let cache = DiscoveryCache::default();
        cache
            .get(|| async { Err(DiscoveryFailure::Unparseable) })
            .await
            .ok();
        assert_eq!(
            cache.take_unreported_failure(),
            Some(DiscoveryFailure::Unparseable)
        );
        assert_eq!(
            cache.take_unreported_failure(),
            None,
            "the same failure must not report twice"
        );
        assert_eq!(cache.take_unreported_failure(), None);
    }

    /// A retry is a new attempt, so a failure that recurs is news again.
    /// Without this, Retry would silence the drift signal permanently.
    #[tokio::test]
    async fn a_failure_that_recurs_after_a_retry_reports_again() {
        let cache = DiscoveryCache::default();
        cache
            .get(|| async { Err(DiscoveryFailure::Unparseable) })
            .await
            .ok();
        assert!(cache.take_unreported_failure().is_some());

        cache.clear();
        cache
            .get(|| async { Err(DiscoveryFailure::Unparseable) })
            .await
            .ok();
        assert_eq!(
            cache.take_unreported_failure(),
            Some(DiscoveryFailure::Unparseable),
            "a fresh attempt starts unreported"
        );
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

    /// A relative program path with a directory in it is the case std warns
    /// about: with `current_dir` set to the temp directory, a Unix child
    /// chdirs before exec and the CLI is looked for in the wrong place. The
    /// spawn failure would degrade silently to the built-in list.
    #[test]
    fn a_relative_program_path_is_absolutized() {
        // Two components and it exists relative to the crate root, which is
        // where cargo runs the test from.
        let rel = std::path::Path::new("src/claude");
        let out = program_path(rel);
        assert!(
            out.is_absolute(),
            "a relative path with a directory must be resolved against the parent's cwd, got {out:?}"
        );
    }

    /// Break caught (D34): `program_path` existed and the RUN launches did not
    /// call it. Discovery was fixed in 2.2 and the run path kept
    /// `program: exe.into()`, so a relative `CLAUDE_CODE_EXECUTABLE` or
    /// `CODEX_EXECUTABLE` — taken verbatim by `resolve_cli`, so a real user
    /// configuration — resolved against the session cwd instead of Comet's.
    ///
    /// Asserts every launch builder that sets a `cwd`, not just the two that
    /// were broken: the ACP pair already routed through `program_path`, and a
    /// future provider that forgets belongs in this failure too.
    #[test]
    fn every_run_launch_resolves_a_relative_program_before_setting_a_cwd() {
        // Two components, and it exists relative to the crate root — where
        // cargo runs this from — so `canonicalize` has something to resolve.
        let rel = std::path::Path::new("src/claude");
        let request = comet_proto::RunRequest {
            cwd: std::env::temp_dir().display().to_string(),
            ..comet_proto::RunRequest::for_session(comet_proto::RuntimeMode::default())
        };

        for (provider, launch) in [
            ("claude", crate::claude::run_launch(rel, &request)),
            ("codex", crate::codex::run_launch(rel, &request)),
            ("grok", crate::acp::grok::run_launch(rel, &request)),
        ] {
            assert!(
                launch.program.is_absolute(),
                concat!(
                    "{}'s run launch must resolve a relative program against the PARENT's ",
                    "cwd, or setting `cwd` moves what it means: {:?}",
                ),
                provider,
                launch.program
            );
        }
    }

    /// The other half, so the fix above cannot become "absolutize everything":
    /// a bare command name is a PATH lookup and every run launch must leave it
    /// alone.
    #[test]
    fn every_run_launch_leaves_a_bare_command_name_alone() {
        let bare = std::path::Path::new("claude");
        let request = comet_proto::RunRequest {
            cwd: std::env::temp_dir().display().to_string(),
            ..comet_proto::RunRequest::for_session(comet_proto::RuntimeMode::default())
        };

        for (provider, launch) in [
            ("claude", crate::claude::run_launch(bare, &request)),
            ("codex", crate::codex::run_launch(bare, &request)),
            ("grok", crate::acp::grok::run_launch(bare, &request)),
        ] {
            assert_eq!(
                launch.program,
                bare.to_path_buf(),
                "{provider} must leave a PATH lookup as a PATH lookup"
            );
        }
    }

    /// A bare name is a PATH lookup, and must stay one — absolutizing it
    /// against the parent's cwd would prefer a stray `./codex` over the
    /// installed CLI.
    #[test]
    fn a_bare_command_name_stays_a_path_lookup() {
        let bare = std::path::Path::new("codex");
        assert_eq!(program_path(bare), bare.to_path_buf());
    }

    /// An absolute path is returned byte-identical, not canonicalized: on
    /// Windows canonicalization yields a `\?\` verbatim path, which
    /// `compose_child_path` would then put in the child's PATH.
    #[test]
    fn an_absolute_path_is_left_exactly_as_it_came() {
        let abs = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        assert_eq!(program_path(&abs), abs);
    }
    /// `ultra` is provider-reported on Codex, not layered on by Comet, so a
    /// model nobody has curated must keep it. Slice 2.3's capture is the
    /// evidence; before that it was filtered with the two real specials.
    #[test]
    fn a_live_only_model_keeps_a_provider_reported_ultra() {
        let answer = Discovery {
            models: vec![DiscoveredModel {
                id: "gpt-5.7-nova".into(),
                label: "Nova".into(),
                description: None,
                deprecation: None,
                reasoning_levels: vec![
                    ReasoningLevel::High,
                    ReasoningLevel::Ultra,
                    ReasoningLevel::Ultracode,
                    ReasoningLevel::Ultrathink,
                ],
                accepts_images: Some(true),
            }],
        };
        let merged = merge(Vec::new(), &answer);
        assert_eq!(
            merged[0].reasoning_levels,
            vec![ReasoningLevel::High, ReasoningLevel::Ultra],
            "ultracode and ultrathink are Comet's own; ultra is the provider's"
        );
    }

    fn command(name: &str) -> AgentCommand {
        AgentCommand {
            name: name.into(),
            description: None,
            argument_hint: None,
            aliases: Vec::new(),
        }
    }

    /// The reason this cache is not `DiscoveryCache`. Two directories are two
    /// answers: serving one project's skills to another directory is a wrong
    /// answer that looks like a right one, which is the failure mode this
    /// phase has now paid for three times.
    #[tokio::test]
    async fn two_directories_get_their_own_answers() {
        let cache = CommandCache::default();
        let a = cache
            .get("/a", || async { Ok(vec![command("from-a")]) })
            .await
            .unwrap();
        let b = cache
            .get("/b", || async { Ok(vec![command("from-b")]) })
            .await
            .unwrap();
        assert_eq!(a[0].name, "from-a");
        assert_eq!(b[0].name, "from-b");
    }

    /// One spawn per directory per boot, however many callers ask. The spawn
    /// is a non-bare CLI that runs the user's `SessionStart` hooks, so a cache
    /// miss is not merely slow — it re-runs their hooks.
    #[tokio::test]
    async fn one_directory_spawns_once_across_concurrent_callers() {
        let cache = CommandCache::default();
        let runs = Arc::new(AtomicUsize::new(0));
        let run = || {
            let runs = runs.clone();
            async move {
                runs.fetch_add(1, Ordering::SeqCst);
                tokio::task::yield_now().await;
                Ok(vec![command("one")])
            }
        };
        let (a, b) = tokio::join!(cache.get("/same", run), cache.get("/same", run));
        assert!(a.is_ok() && b.is_ok());
        assert_eq!(runs.load(Ordering::SeqCst), 1, "one spawn, two callers");
    }

    /// A failure is cached like a success, per directory. Without it, every
    /// `/` keystroke in a directory whose CLI cannot answer spawns another
    /// doomed subprocess and waits out another timeout.
    #[tokio::test]
    async fn a_failure_is_cached_per_directory() {
        let cache = CommandCache::default();
        let runs = Arc::new(AtomicUsize::new(0));
        let run = || {
            let runs = runs.clone();
            async move {
                runs.fetch_add(1, Ordering::SeqCst);
                Err(DiscoveryFailure::Unreachable)
            }
        };
        assert!(cache.get("/x", run).await.is_err());
        assert!(cache.get("/x", run).await.is_err());
        assert_eq!(runs.load(Ordering::SeqCst), 1, "the failure is remembered");
        assert!(cache.get("/y", run).await.is_err());
        assert_eq!(runs.load(Ordering::SeqCst), 2, "a different cwd still runs");
    }

    /// Retry is the only escape from a cached failure inside one boot.
    #[tokio::test]
    async fn clearing_re_arms_every_directory() {
        let cache = CommandCache::default();
        let runs = Arc::new(AtomicUsize::new(0));
        let run = || {
            let runs = runs.clone();
            async move {
                runs.fetch_add(1, Ordering::SeqCst);
                Err(DiscoveryFailure::Unreachable)
            }
        };
        assert!(cache.get("/x", run).await.is_err());
        cache.clear();
        assert!(cache.get("/x", run).await.is_err());
        assert_eq!(runs.load(Ordering::SeqCst), 2);
    }
}
