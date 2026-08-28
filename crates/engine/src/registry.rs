//! HarnessRegistry — the engine's harness catalog: eager instances (mock) plus lazy
//! slots resolved on first use (claude-code spawns subprocess discovery; codex/cursor
//! later). Lazy slots carry a static descriptor so `ListHarnesses` never forces a spawn.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde::{Deserialize, Serialize};

use comet_harness::{Harness, HarnessError, mock::MockHarness};
use comet_proto::{
    AgentEvent, DiagnosticSeverity, DoneStatus, HarnessAvailability, HarnessCapabilities,
    HarnessId, HarnessInstall, HarnessProbe, HarnessUpdate, sanitize_discriminator,
};

/// What `ListHarnesses` reports per harness.
///
/// `capabilities` is flattened, so the wire shape is unchanged from when these
/// were three sibling fields — a remote client on an older build decodes this
/// descriptor byte-identically.
///
/// `availability` is a sibling rather than a capability, and is serde-defaulted
/// so an older engine that omits it reads as `Unknown` (selectable) instead of
/// failing the whole reply. See [`HarnessAvailability`] for why it is kept out
/// of [`HarnessCapabilities`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessDescriptor {
    pub id: HarnessId,
    pub name: String,
    #[serde(flatten)]
    pub capabilities: HarnessCapabilities,
    #[serde(default)]
    pub availability: HarnessAvailability,
    /// Which binary answered, and how it got there. A sibling of `availability`
    /// for the same reason it is: discovered at run time, published
    /// asynchronously, absent until the probe lands.
    ///
    /// `skip_serializing_if` keeps the descriptor byte-identical for a harness
    /// with no CLI (the mock), so nothing that snapshots this reply changes
    /// shape for the in-process case.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install: Option<HarnessInstall>,
    /// How current that binary is, to the extent its provider will say. A third
    /// sibling for the same reasons as `install`, and skipped the same way so a
    /// harness with no CLI — or a provider that publishes nothing — sends a
    /// descriptor byte-identical to the one it sent before this field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update: Option<HarnessUpdate>,
}

fn describe(harness: &dyn Harness) -> HarnessDescriptor {
    HarnessDescriptor {
        id: harness.id(),
        name: harness.display_name().to_string(),
        capabilities: harness.capabilities(),
        // Neither is the harness's to answer synchronously — both are overlaid
        // from the probe cache by `descriptors()`.
        availability: HarnessAvailability::Unknown,
        install: None,
        update: None,
    }
}

/// One aggregated diagnostic row: a discriminator this boot has seen, how
/// often, and when. `severity` is not enforced to be fixed per discriminator
/// — nothing stops a provider frame from being typed literally
/// `"unparseable"`, colliding with the parse-failure sentinel — so treat it
/// as: whichever severity arrived first for this discriminator is the one
/// retained; later arrivals with a different severity are silently ignored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessDiagnosticEntry {
    pub discriminator: String,
    pub severity: DiagnosticSeverity,
    pub count: u64,
    pub first_seen_ms: i64,
    pub last_seen_ms: i64,
}

/// What `ListHarnessDiagnostics` reports per harness. Per-boot, not
/// persisted: it describes the pairing of THIS Comet build with THIS CLI
/// version, and is worthless the moment either changes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessDiagnostics {
    pub harness: HarnessId,
    /// Distinct discriminators, most frequent first (≤ 64).
    pub entries: Vec<HarnessDiagnosticEntry>,
    /// Arrivals whose (new) discriminator no longer fit under the cap.
    #[serde(default)]
    pub overflow: u64,
}

/// Bound on distinct discriminators per harness — past it, new names pour
/// into the overflow bucket so a chatty future protocol cannot grow memory.
const MAX_DISTINCT_DIAGNOSTICS: usize = 64;

#[derive(Default)]
struct DiagnosticsBucket {
    entries: HashMap<String, HarnessDiagnosticEntry>,
    overflow: u64,
}

type Factory = Box<dyn Fn() -> Result<Arc<dyn Harness>, HarnessError> + Send + Sync>;

enum Slot {
    Ready(Arc<dyn Harness>),
    Lazy {
        /// Boxed to keep the two variants a similar size. `Ready` is a fat
        /// pointer; the descriptor grows every time a probed fact is added to
        /// it (`availability`, then `install`, now `update`), and without this
        /// every `Slot` in the map would be sized for the largest descriptor
        /// any future slice adds.
        descriptor: Box<HarnessDescriptor>,
        factory: Factory,
    },
}

pub struct HarnessRegistry {
    slots: Mutex<HashMap<HarnessId, Slot>>,
    order: Mutex<Vec<HarnessId>>,
    /// Probe results, overlaid onto every descriptor. Absent = never probed,
    /// which reports as `Unknown` and leaves the harness selectable.
    ///
    /// One entry per harness holding both halves, never two maps: a descriptor
    /// showing an install from one probe beside a version from another would
    /// be a plausible-looking lie during the window between two writes.
    probes: Mutex<HashMap<HarnessId, HarnessProbe>>,
    /// Per-boot log of unrecognized provider frames, keyed by
    /// (harness, discriminator). Bounded; counts saturate.
    diagnostics: Mutex<HashMap<HarnessId, DiagnosticsBucket>>,
}

impl Default for HarnessRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl HarnessRegistry {
    pub fn new() -> Self {
        Self {
            slots: Mutex::new(HashMap::new()),
            order: Mutex::new(Vec::new()),
            probes: Mutex::new(HashMap::new()),
            diagnostics: Mutex::new(HashMap::new()),
        }
    }

    fn slots(&self) -> MutexGuard<'_, HashMap<HarnessId, Slot>> {
        self.slots.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn order(&self) -> MutexGuard<'_, Vec<HarnessId>> {
        self.order.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn register(&self, harness: Arc<dyn Harness>) {
        let id = harness.id();
        if self.slots().insert(id, Slot::Ready(harness)).is_none() {
            self.order().push(id);
        }
    }

    /// Register a slot resolved on first `resolve` (the factory result is cached).
    pub fn register_lazy(&self, descriptor: HarnessDescriptor, factory: Factory) {
        let id = descriptor.id;
        if self
            .slots()
            .insert(
                id,
                Slot::Lazy {
                    descriptor: Box::new(descriptor),
                    factory,
                },
            )
            .is_none()
        {
            self.order().push(id);
        }
    }

    pub fn resolve(&self, id: HarnessId) -> Result<Arc<dyn Harness>, HarnessError> {
        let mut slots = self.slots();
        match slots.get(&id) {
            Some(Slot::Ready(harness)) => Ok(harness.clone()),
            Some(Slot::Lazy { factory, .. }) => {
                let harness = factory()?;
                slots.insert(id, Slot::Ready(harness.clone()));
                Ok(harness)
            }
            None => Err(HarnessError::NotInstalled(format!("{id:?}"))),
        }
    }

    fn probes(&self) -> MutexGuard<'_, HashMap<HarnessId, HarnessProbe>> {
        self.probes.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Record a probe result. Idempotent; a later probe replaces an earlier one.
    pub fn set_probe(&self, id: HarnessId, probe: HarnessProbe) {
        self.probes().insert(id, probe);
    }

    fn diagnostics_lock(&self) -> MutexGuard<'_, HashMap<HarnessId, DiagnosticsBucket>> {
        self.diagnostics
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Count one unrecognized frame. Saturating; the 65th distinct
    /// discriminator (and later new ones) accumulate into the overflow
    /// bucket. Re-sanitizes defensively — the registry is the last owner
    /// before an RPC reply and a settings card.
    ///
    /// Deliberately not filtered by subagent origin: an unknown frame is
    /// unknown regardless of which thread produced it, and this count is a
    /// protocol-drift signal (does this build understand this CLI version),
    /// not a per-conversation statistic.
    pub fn record_diagnostic(
        &self,
        id: HarnessId,
        discriminator: &str,
        severity: DiagnosticSeverity,
    ) {
        let discriminator = sanitize_discriminator(discriminator);
        let now_ms = chrono::Utc::now().timestamp_millis();
        let mut all = self.diagnostics_lock();
        let bucket = all.entry(id).or_default();
        if let Some(entry) = bucket.entries.get_mut(&discriminator) {
            entry.count = entry.count.saturating_add(1);
            entry.last_seen_ms = now_ms;
        } else if bucket.entries.len() < MAX_DISTINCT_DIAGNOSTICS {
            bucket.entries.insert(
                discriminator.clone(),
                HarnessDiagnosticEntry {
                    discriminator,
                    severity,
                    count: 1,
                    first_seen_ms: now_ms,
                    last_seen_ms: now_ms,
                },
            );
        } else {
            bucket.overflow = bucket.overflow.saturating_add(1);
        }
    }

    /// The per-boot report for `ListHarnessDiagnostics`: only harnesses that
    /// recorded something, rows most frequent first (name as tiebreak, so the
    /// order is deterministic).
    pub fn diagnostics(&self) -> Vec<HarnessDiagnostics> {
        let all = self.diagnostics_lock();
        let mut out: Vec<HarnessDiagnostics> = all
            .iter()
            .map(|(id, bucket)| {
                let mut entries: Vec<HarnessDiagnosticEntry> =
                    bucket.entries.values().cloned().collect();
                entries.sort_by(|a, b| {
                    b.count
                        .cmp(&a.count)
                        .then_with(|| a.discriminator.cmp(&b.discriminator))
                });
                HarnessDiagnostics {
                    harness: *id,
                    entries,
                    overflow: bucket.overflow,
                }
            })
            .collect();
        out.sort_by_key(|d| format!("{:?}", d.harness));
        out
    }

    /// Catalog for `ListHarnesses` — never forces a lazy resolve.
    pub fn descriptors(&self) -> Vec<HarnessDescriptor> {
        let slots = self.slots();
        let probes = self.probes();
        self.order()
            .iter()
            .filter_map(|id| {
                let mut descriptor = match slots.get(id) {
                    Some(Slot::Ready(harness)) => describe(harness.as_ref()),
                    Some(Slot::Lazy { descriptor, .. }) => (**descriptor).clone(),
                    None => return None,
                };
                // Overlaid here rather than stored on the slot, so the probe
                // never has to reach into a descriptor a lazy slot owns. All
                // three come from the same cached probe, so the path can never
                // be shown beside a version it was not read with, nor an
                // update verdict beside a different binary's version.
                if let Some(probed) = probes.get(id) {
                    descriptor.availability = probed.availability.clone();
                    descriptor.install = probed.install.clone();
                    descriptor.update = probed.update.clone();
                }
                Some(descriptor)
            })
            .collect()
    }

    /// Probe every registered harness in the background, one task each.
    ///
    /// Deliberately fire-and-forget: `ListHarnesses` is request/response with
    /// no push channel, so results are simply read by whichever `descriptors()`
    /// call comes after they land. Until then a harness reports `Unknown` and
    /// stays selectable — the picker opens long after boot, so in practice the
    /// probe has finished, and when it has not the user loses nothing.
    ///
    /// A no-op outside a tokio runtime, which is what test callers of
    /// [`default_registry`] get; nothing depends on the probe having run.
    pub fn spawn_probes(self: &Arc<Self>) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let ids: Vec<HarnessId> = self.order().clone();
        for id in ids {
            let registry = self.clone();
            handle.spawn(async move {
                // Resolving a lazy slot only constructs the harness — CLI
                // discovery still happens inside `availability()`. A slot whose
                // factory fails is itself the reason it is unusable.
                let probe = match registry.resolve(id) {
                    Ok(harness) => harness.probe().await,
                    Err(err) => HarnessProbe::unresolved(HarnessAvailability::unavailable(
                        "Not working",
                        Some(err.to_string()),
                    )),
                };
                if let Some(summary) = probe.availability.unavailable_summary() {
                    tracing::info!(
                        ?id,
                        summary,
                        hint = probe.availability.unavailable_hint().unwrap_or("-"),
                        // A resolved-but-broken CLI logs WHICH binary failed.
                        // Without it the log says a provider is broken without
                        // saying which of two installs was asked.
                        path = probe
                            .install
                            .as_ref()
                            .map(|i| i.path.as_str())
                            .unwrap_or("-"),
                        "harness unavailable"
                    );
                }
                registry.set_probe(id, probe);
            });
        }
    }
}

/// The production registry: MockHarness (hidden from production pickers) plus a lazy
/// `claude-code` slot resolved through `comet_harness` on first use (subprocess
/// discovery only happens when a run/model call actually needs it).
pub fn default_registry() -> HarnessRegistry {
    // Warm the login-shell PATH snapshot in the background so the first
    // claude/codex resolve doesn't pay the shell-startup latency inline.
    comet_harness::shell_env::prewarm();
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(MockHarness::with_script(vec![
            AgentEvent::TextDelta {
                text: "## Streaming pipeline\n\nEvery turn flows through the same path:\n\n".into(),
            },
            AgentEvent::TextDelta {
                text: "1. **Doc command** — the composer queues a durable `run` entry\n2. **Host executor** — the chat's host device marks it processed, then dispatches\n3. **Fold** — events fold into parts and diff into the Loro doc every 120ms\n\n".into(),
            },
            AgentEvent::ToolCall {
                id: "mock-tool-1".into(),
                call: comet_proto::ToolCall::Exec {
                    command: "cargo test --workspace".into(),
                },
            },
            AgentEvent::ToolResult {
                id: "mock-tool-1".into(),
                is_error: false,
                diff: None,
                diff_ref: None,
                diff_stats: None,
            },
            AgentEvent::ToolCall {
                id: "mock-tool-2".into(),
                call: comet_proto::ToolCall::Exec {
                    command: "git log -5 --oneline --decorate && git merge-base HEAD origin/main"
                        .into(),
                },
            },
            AgentEvent::ToolResult {
                id: "mock-tool-2".into(),
                is_error: false,
                diff: None,
                diff_ref: None,
                diff_stats: None,
            },
            AgentEvent::Notice {
                kind: comet_proto::NoticeKind::Compaction,
                severity: comet_proto::NoticeSeverity::Info,
                summary: "Context compacted automatically".into(),
                detail: Some("41,000 tokens → 9,500".into()),
                key: Some("compaction".into()),
            },
            AgentEvent::TextDelta {
                text: "The `SegmentWriter` appends into `LoroText` so the oplog stays RLE-merged:\n\n```rust\nfolded = fold_event_into_parts(&folded, &event);\nwriter.sync(&folded)?; // 120ms coalesced commits\n```\n\nSynced to every device through the session room. *Mock harness reporting in.*".into(),
            },
            AgentEvent::Done {
                status: DoneStatus::Completed,
                result: None,
                error: None,
                session_id: None,
            },
        ])));
    // The lazy descriptors name the harness's own `capabilities()`, so the
    // catalog entry `ListHarnesses` reports before first use is the same value
    // `describe()` produces after the slot resolves. CLI discovery still only
    // happens when a run/model call actually resolves the slot.
    registry.register_lazy(
        HarnessDescriptor {
            id: HarnessId::ClaudeCode,
            name: "Claude Code".into(),
            capabilities: comet_harness::ClaudeHarness::capabilities(),
            availability: HarnessAvailability::Unknown,
            install: None,
            update: None,
        },
        Box::new(|| Ok(Arc::new(comet_harness::ClaudeHarness::new()) as Arc<dyn Harness>)),
    );
    registry.register_lazy(
        HarnessDescriptor {
            id: HarnessId::Codex,
            // "Codex" (not "Codex CLI") — comet composer/defaults.ts
            // HARNESS_LABEL; must match CodexHarness::display_name().
            name: "Codex".into(),
            capabilities: comet_harness::CodexHarness::capabilities(),
            availability: HarnessAvailability::Unknown,
            install: None,
            update: None,
        },
        Box::new(|| Ok(Arc::new(comet_harness::CodexHarness::new()) as Arc<dyn Harness>)),
    );
    registry.register_lazy(
        HarnessDescriptor {
            id: HarnessId::Grok,
            // Must match GrokHarness::display_name().
            name: "Grok".into(),
            capabilities: comet_harness::acp::grok::GrokHarness::capabilities(),
            availability: HarnessAvailability::Unknown,
            install: None,
            update: None,
        },
        Box::new(|| Ok(Arc::new(comet_harness::acp::grok::GrokHarness::new()) as Arc<dyn Harness>)),
    );
    registry
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lazy_slot_lists_without_resolving() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let registry = HarnessRegistry::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        registry.register_lazy(
            HarnessDescriptor {
                id: HarnessId::Mock,
                name: "Lazy Mock".into(),
                capabilities: HarnessCapabilities::default(),
                availability: HarnessAvailability::Unknown,
                install: None,
                update: None,
            },
            Box::new(move || {
                counted.fetch_add(1, Ordering::SeqCst);
                Err(HarnessError::NotInstalled("nope".into()))
            }),
        );
        let listed = registry.descriptors();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "Lazy Mock");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "listing must not force a resolve"
        );
        assert!(registry.resolve(HarnessId::Mock).is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// The registered slots and their ORDER. Order is asserted because it is
    /// what the picker renders, and because a registration appended in the
    /// wrong place is otherwise invisible.
    #[test]
    fn default_registry_lists_every_slot_in_picker_order() {
        let registry = default_registry();
        let ids: Vec<HarnessId> = registry.descriptors().iter().map(|d| d.id).collect();
        assert_eq!(
            ids,
            vec![
                HarnessId::Mock,
                HarnessId::ClaudeCode,
                HarnessId::Codex,
                HarnessId::Grok,
            ]
        );
        assert!(registry.resolve(HarnessId::Mock).is_ok());
        assert!(registry.resolve(HarnessId::ClaudeCode).is_ok());
        // Resolving answers the right harness (construction is cheap; CLI
        // discovery is deferred to models()/run()).
        for id in [HarnessId::Codex, HarnessId::Grok] {
            assert_eq!(registry.resolve(id).unwrap().id(), id);
        }
    }

    /// A lazy descriptor must be indistinguishable from `describe()` after the
    /// first resolve — otherwise the catalog entry silently changes the moment
    /// the harness is used (name/ladder flip in the picker rail). Both lazy
    /// slots are covered; `Mock` is registered eagerly, so it is always
    /// `describe()`-derived and has no second declaration to drift from.
    ///
    /// This previously covered Codex alone, and carried a comment claiming the
    /// claude-code descriptor was drifted; extending the test to that slot
    /// shows the claim was stale, so it is gone rather than restated. Both
    /// descriptors now name the harness's own `capabilities()`, making drift
    /// unrepresentable rather than merely tested for — these assertions stand
    /// as a guard against someone re-inlining a literal.
    #[test]
    fn lazy_descriptors_match_resolved_harnesses() {
        for id in [HarnessId::ClaudeCode, HarnessId::Codex, HarnessId::Grok] {
            let registry = default_registry();
            let find = |registry: &HarnessRegistry| {
                registry
                    .descriptors()
                    .into_iter()
                    .find(|d| d.id == id)
                    .unwrap_or_else(|| panic!("{id:?} missing from the catalog"))
            };
            let before = find(&registry);
            registry
                .resolve(id)
                .unwrap_or_else(|e| panic!("{id:?} failed to resolve: {e}"));
            let after = find(&registry);
            assert_eq!(before.name, after.name, "{id:?} name drifted on resolve");
            assert_eq!(
                before.capabilities, after.capabilities,
                "{id:?} capabilities drifted on resolve"
            );
        }
    }

    /// A harness nobody has probed reports `Unknown`, not `Unavailable`. The
    /// picker keeps `Unknown` selectable, so getting this backwards would
    /// disable every provider for the whole window between boot and the first
    /// probe landing.
    #[test]
    fn unprobed_harnesses_report_unknown() {
        let registry = default_registry();
        for descriptor in registry.descriptors() {
            assert_eq!(
                descriptor.availability,
                HarnessAvailability::Unknown,
                "{:?} should be unprobed",
                descriptor.id
            );
            assert!(!descriptor.availability.is_unavailable());
        }
    }

    /// A probe result reaches the catalog, and reaches it for the right
    /// harness only.
    #[test]
    fn a_probe_result_is_overlaid_onto_its_descriptor() {
        let registry = default_registry();
        registry.set_probe(
            HarnessId::Codex,
            HarnessProbe::unresolved(HarnessAvailability::unavailable(
                "Not installed",
                Some("Install codex, or set CODEX_EXECUTABLE to its path.".into()),
            )),
        );
        registry.set_probe(
            HarnessId::ClaudeCode,
            HarnessProbe {
                availability: HarnessAvailability::Available {
                    version: Some("1.0.30".into()),
                },
                install: Some(HarnessInstall {
                    path: "/Users/a/.local/bin/claude".into(),
                    method: comet_proto::InstallMethod::Native,
                }),
                update: Some(HarnessUpdate {
                    state: comet_proto::UpdateState::SelfUpdating,
                    latest: None,
                    checked_at: Some("2026-08-11T19:59:18.645Z".into()),
                }),
            },
        );

        let by_id = |id: HarnessId| {
            registry
                .descriptors()
                .into_iter()
                .find(|d| d.id == id)
                .expect("harness in catalog")
                .availability
        };
        assert_eq!(
            by_id(HarnessId::Codex).unavailable_summary(),
            Some("Not installed")
        );
        assert_eq!(
            by_id(HarnessId::Codex).unavailable_hint(),
            Some("Install codex, or set CODEX_EXECUTABLE to its path.")
        );
        assert_eq!(
            by_id(HarnessId::ClaudeCode),
            HarnessAvailability::Available {
                version: Some("1.0.30".into())
            }
        );
        // Untouched slots stay unprobed rather than inheriting a neighbour's.
        assert_eq!(by_id(HarnessId::Mock), HarnessAvailability::Unknown);

        // The install rides the same overlay. Both halves come from one cached
        // probe, so a harness cannot end up showing one and not the other.
        let install = |id: HarnessId| {
            registry
                .descriptors()
                .into_iter()
                .find(|d| d.id == id)
                .expect("harness in catalog")
                .install
        };
        let claude = install(HarnessId::ClaudeCode).expect("the probed install must be published");
        assert_eq!(claude.path, "/Users/a/.local/bin/claude");
        assert_eq!(claude.method, comet_proto::InstallMethod::Native);
        // Codex never resolved, so it has a reason but no path — and must not
        // borrow the neighbour's.
        assert_eq!(install(HarnessId::Codex), None);
        assert_eq!(install(HarnessId::Mock), None);

        // And so does the update state, from the same cached probe — a verdict
        // published beside another binary's version would be a plausible lie.
        let update = |id: HarnessId| {
            registry
                .descriptors()
                .into_iter()
                .find(|d| d.id == id)
                .expect("harness in catalog")
                .update
        };
        let claude = update(HarnessId::ClaudeCode).expect("the probed update must be published");
        assert_eq!(claude.state, comet_proto::UpdateState::SelfUpdating);
        assert_eq!(
            claude.checked_at.as_deref(),
            Some("2026-08-11T19:59:18.645Z")
        );
        assert_eq!(update(HarnessId::Codex), None);
        assert_eq!(update(HarnessId::Mock), None);
    }

    /// The overlay must survive a lazy slot resolving, which swaps the stored
    /// descriptor for a `describe()`-derived one. `describe()` cannot know the
    /// probe result, so a naive implementation loses it on first use.
    #[test]
    fn availability_survives_a_lazy_resolve() {
        let registry = default_registry();
        registry.set_probe(
            HarnessId::Codex,
            HarnessProbe {
                availability: HarnessAvailability::Available {
                    version: Some("0.20.0".into()),
                },
                install: Some(HarnessInstall {
                    path: "/opt/homebrew/bin/codex".into(),
                    method: comet_proto::InstallMethod::Homebrew,
                }),
                update: Some(HarnessUpdate {
                    state: comet_proto::UpdateState::Current,
                    latest: Some("0.20.0".into()),
                    checked_at: None,
                }),
            },
        );
        registry.resolve(HarnessId::Codex).unwrap();
        let after = registry
            .descriptors()
            .into_iter()
            .find(|d| d.id == HarnessId::Codex)
            .unwrap();
        assert_eq!(
            after.availability,
            HarnessAvailability::Available {
                version: Some("0.20.0".into())
            },
            "resolving the slot dropped the probe result"
        );
        assert_eq!(
            after.install.map(|i| i.path),
            Some("/opt/homebrew/bin/codex".to_string()),
            "resolving the slot dropped the install"
        );
        assert_eq!(
            after.update.map(|u| u.state),
            Some(comet_proto::UpdateState::Current),
            "resolving the slot dropped the update state"
        );
    }

    /// Probing must not be a precondition for anything: `default_registry()` is
    /// constructed off-runtime in tests and by sync callers.
    #[test]
    fn spawning_probes_off_runtime_is_a_no_op() {
        let registry = Arc::new(default_registry());
        registry.spawn_probes();
        assert!(
            registry
                .descriptors()
                .iter()
                .all(|d| d.availability == HarnessAvailability::Unknown)
        );
    }

    /// The mock harness has no CLI, so the trait default applies and it probes
    /// as available — a fixture harness must never render disabled.
    #[tokio::test]
    async fn an_in_process_harness_probes_as_available() {
        let registry = Arc::new(default_registry());
        let mock = registry.resolve(HarnessId::Mock).unwrap();
        let probe = mock.probe().await;
        assert_eq!(
            probe.availability,
            HarnessAvailability::Available { version: None }
        );
        // No CLI means no path to name. `None` here is the honest answer, and
        // the card must not invent one.
        assert_eq!(probe.install, None);
    }

    /// `capabilities` is `#[serde(flatten)]`, so the descriptor keeps the wire
    /// shape it had when these were three sibling fields. A remote client on a
    /// build that predates `HarnessCapabilities` must still decode it.
    #[test]
    fn descriptor_wire_shape_is_flat() {
        let descriptor = HarnessDescriptor {
            id: HarnessId::Codex,
            name: "Codex".into(),
            capabilities: comet_harness::CodexHarness::capabilities(),
            availability: HarnessAvailability::Unknown,
            install: None,
            update: None,
        };
        let json = serde_json::to_value(&descriptor).unwrap();
        let object = json
            .as_object()
            .expect("descriptor serializes to an object");
        assert!(
            object.get("capabilities").is_none(),
            "capabilities must flatten, not nest: {json}"
        );
        for key in [
            "id",
            "name",
            "supportsSteering",
            "steeringMode",
            "reasoningLevels",
        ] {
            assert!(object.contains_key(key), "missing `{key}` in {json}");
        }
        let round: HarnessDescriptor = serde_json::from_value(json).unwrap();
        assert_eq!(round.capabilities, descriptor.capabilities);
    }

    /// The literal JSON `ListHarnesses` sends, asserted key by key rather than
    /// round-tripped through the Rust type.
    ///
    /// AGENTS.md's rule about reply consumers exists because a round-trip test
    /// stayed green through the `ListModels` reshape that broke the picker at
    /// run time. The same trap applies here: `apps/ios` reads harnesses from a
    /// hardcoded Swift catalog rather than this reply, and `e2e.rs` indexes it
    /// untyped, so neither would fail on a wrong key name.
    #[test]
    fn the_wire_shape_of_a_probed_install() {
        let descriptor = HarnessDescriptor {
            id: HarnessId::ClaudeCode,
            name: "Claude Code".into(),
            capabilities: comet_harness::ClaudeHarness::capabilities(),
            availability: HarnessAvailability::Available {
                version: Some("2.1.228".into()),
            },
            install: Some(HarnessInstall {
                path: r"C:\Users\a\.local\bin\claude.exe".into(),
                method: comet_proto::InstallMethod::Native,
            }),
            update: Some(HarnessUpdate {
                state: comet_proto::UpdateState::Available,
                latest: Some("0.148.0".into()),
                checked_at: Some("2026-08-12T00:48:08.145707800Z".into()),
            }),
        };
        let json = serde_json::to_value(&descriptor).unwrap();
        assert_eq!(json["install"]["path"], r"C:\Users\a\.local\bin\claude.exe");
        assert_eq!(json["install"]["method"], "native");
        assert_eq!(json["availability"]["version"], "2.1.228");
        assert_eq!(json["update"]["state"], "available");
        assert_eq!(json["update"]["latest"], "0.148.0");
        assert_eq!(
            json["update"]["checkedAt"],
            "2026-08-12T00:48:08.145707800Z"
        );

        // A harness with no CLI omits the keys entirely rather than sending
        // `null`, so its payload is byte-identical to what it was before these
        // fields existed. Anything already snapshotting the mock's descriptor
        // keeps working.
        let cliless = HarnessDescriptor {
            install: None,
            update: None,
            ..descriptor
        };
        let json = serde_json::to_value(&cliless).unwrap();
        assert!(
            json.as_object().unwrap().get("install").is_none(),
            "an absent install must not serialize as null: {json}"
        );
        assert!(
            json.as_object().unwrap().get("update").is_none(),
            "an absent update must not serialize as null: {json}"
        );
    }

    #[test]
    fn diagnostics_aggregate_by_discriminator_per_harness() {
        use comet_proto::DiagnosticSeverity;
        let registry = HarnessRegistry::new();
        // Spec verification 1: a thousand arrivals of one discriminator are
        // one row with count 1000 — aggregation is what makes a
        // high-frequency unknown frame harmless.
        for _ in 0..1000 {
            registry.record_diagnostic(
                HarnessId::ClaudeCode,
                "system/somethingNew",
                DiagnosticSeverity::Unknown,
            );
        }
        registry.record_diagnostic(
            HarnessId::Codex,
            "thread/checkpoint/created",
            DiagnosticSeverity::Unknown,
        );
        let report = registry.diagnostics();
        let claude = report
            .iter()
            .find(|d| d.harness == HarnessId::ClaudeCode)
            .expect("claude bucket");
        assert_eq!(claude.entries.len(), 1);
        assert_eq!(claude.entries[0].discriminator, "system/somethingNew");
        assert_eq!(claude.entries[0].count, 1000);
        assert_eq!(claude.overflow, 0);
        assert!(claude.entries[0].last_seen_ms >= claude.entries[0].first_seen_ms);
        // The other harness's counts stay separate — the key is
        // (HarnessId, discriminator).
        let codex = report
            .iter()
            .find(|d| d.harness == HarnessId::Codex)
            .expect("codex bucket");
        assert_eq!(codex.entries.len(), 1);
        assert_eq!(codex.entries[0].count, 1);
    }

    /// Spec verification 4: the 65th DISTINCT discriminator lands in the
    /// overflow bucket; an existing one still counts normally after the cap.
    #[test]
    fn the_sixty_fifth_discriminator_lands_in_the_overflow_bucket() {
        use comet_proto::DiagnosticSeverity;
        let registry = HarnessRegistry::new();
        for i in 0..64 {
            registry.record_diagnostic(
                HarnessId::Codex,
                &format!("method/{i}"),
                DiagnosticSeverity::Unknown,
            );
        }
        registry.record_diagnostic(HarnessId::Codex, "method/64", DiagnosticSeverity::Unknown);
        registry.record_diagnostic(HarnessId::Codex, "method/65", DiagnosticSeverity::Unknown);
        registry.record_diagnostic(HarnessId::Codex, "method/0", DiagnosticSeverity::Unknown);
        let report = registry.diagnostics();
        let codex = report
            .iter()
            .find(|d| d.harness == HarnessId::Codex)
            .expect("codex bucket");
        assert_eq!(codex.entries.len(), 64);
        assert_eq!(codex.overflow, 2);
        let m0 = codex
            .entries
            .iter()
            .find(|e| e.discriminator == "method/0")
            .expect("existing row");
        assert_eq!(m0.count, 2);
        // Rows come back most frequent first, so the card's top line is the
        // loudest discriminator.
        assert_eq!(codex.entries[0].discriminator, "method/0");
    }

    /// Defense in depth (spec verification 2): the harness sanitizes at the
    /// drop site, but the registry is the last owner before an RPC reply and
    /// a settings card. A Windows path is rejected because the backslash is
    /// outside `sanitize_discriminator`'s alphabet — that is the only sense
    /// in which this is "path" protection. See the POSIX case below for what
    /// actually happens to a path made only of allowed characters.
    #[test]
    fn the_registry_re_sanitizes_discriminators() {
        use comet_proto::DiagnosticSeverity;
        let registry = HarnessRegistry::new();
        registry.record_diagnostic(
            HarnessId::ClaudeCode,
            r"C:\dev\secrets.txt",
            DiagnosticSeverity::Unknown,
        );
        let report = registry.diagnostics();
        let claude = report
            .iter()
            .find(|d| d.harness == HarnessId::ClaudeCode)
            .expect("claude bucket");
        assert_eq!(claude.entries[0].discriminator, "malformed");
    }

    /// Documented, intended behaviour — not an oversight: a POSIX-style path
    /// is built entirely from `sanitize_discriminator`'s allowed alphabet
    /// (`[A-Za-z0-9._/-]`), so it survives re-sanitization unchanged and
    /// would reach an RPC reply and a settings card verbatim. Every current
    /// caller only ever feeds this type names and JSON-RPC methods, so no
    /// live sink passes untrusted free text through; a future caller that
    /// does must sanitize for its own concerns first, per the doc on
    /// `sanitize_discriminator`.
    #[test]
    fn the_registry_passes_a_posix_style_path_through_unchanged() {
        use comet_proto::DiagnosticSeverity;
        let registry = HarnessRegistry::new();
        registry.record_diagnostic(
            HarnessId::ClaudeCode,
            "/home/matty/.ssh/id_rsa",
            DiagnosticSeverity::Unknown,
        );
        let report = registry.diagnostics();
        let claude = report
            .iter()
            .find(|d| d.harness == HarnessId::ClaudeCode)
            .expect("claude bucket");
        assert_eq!(claude.entries[0].discriminator, "/home/matty/.ssh/id_rsa");
    }
}
