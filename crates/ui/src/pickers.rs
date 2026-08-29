//! Composer pickers (feature-inventory §1.7): RepoPicker (recents + search +
//! in-app folder browser + clone/create), BranchPicker (search + isolated-
//! worktree toggle), HarnessModelPicker (harness rail + model list, harness
//! locked once the chat exists), TraitsPicker (reasoning ladder + advertised
//! model options; trigger shows the non-default summary "High · 1M · Fast").
//!
//! All selections accumulate into a [`DraftConfig`] the composer threads into
//! the Run command and the `Mutate createChat` call on first send.
//!
//! Pure logic (repo ordering, folder-browser navigation, traits summary) lives
//! in free functions with unit tests; RPC results land in [`Loadable`] slots
//! rendered as skeletons / inline errors with Retry.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, Focusable as _, KeyDownEvent, PathBuilder,
    SharedString, Subscription, Task, Window, canvas, div, point, prelude::*, px,
};

use comet_engine::registry::HarnessDescriptor;
use comet_proto::{
    CatalogSource, ChatConfig, FolderListing, HarnessId, Model, ModelCatalog, ReasoningLevel,
    RepoRef, RuntimeMode, ServerRef,
};
use comet_rpc::methods;

/// Display cap for the ref list (t3code shows pages of 100 with a status
/// footer; a flat cap + "Showing X of Y refs" reads the same without
/// pagination plumbing).
const MAX_REF_ROWS: usize = 300;

/// How often an open harness picker re-asks for the catalog while provider
/// probes are still landing. Local IPC returning three small objects, so the
/// cost is negligible next to keeping a stale row selectable.
/// Also used by the Agents settings pane, which has the same race for the same
/// reason — see `settings::accounts::AccountsPage::poll_harness_installs`.
pub(crate) const HARNESS_REVALIDATE_INTERVAL: Duration = Duration::from_millis(500);

/// Cap on those re-asks. `HARNESS_REVALIDATE_INTERVAL * ATTEMPTS` covers the
/// harness crate's own 10s `--version` timeout, so a probe that is merely slow
/// is still waited out, while one that never answers cannot poll forever.
pub(crate) const HARNESS_REVALIDATE_ATTEMPTS: usize = 20;

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::errors;
use crate::motion;
use crate::popover::{self, Loadable, MenuKey};
use crate::settings::composer::ComposerDefaults;
use crate::state::{AppState, ServerClient};
use crate::theme::Theme;
use crate::toast;

// ---------------------------------------------------------------------------
// Draft config (what the pickers accumulate)
// ---------------------------------------------------------------------------

/// Everything a new chat is configured with before the first send. The folder
/// and device come from the selected SPACE — the draft only carries the git
/// extras (ref + checkout kind) and the run config.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DraftConfig {
    pub harness: Option<HarnessId>,
    pub model: Option<String>,
    pub reasoning: Option<ReasoningLevel>,
    /// option id → choice id (only non-defaults are meaningful).
    pub model_options: serde_json::Map<String, serde_json::Value>,
    /// The new-chat canvas's pick. Nothing writes this yet — it only ever
    /// takes its `Default` — until the composer grows a picker for it.
    pub runtime_mode: RuntimeMode,
    /// The picked ref (base branch in NewWorktree mode; a worktree's branch
    /// when reusing one). `None` = the repo's current branch.
    pub branch: Option<String>,
    /// Where the new session runs (the t3code env-mode).
    pub checkout: CheckoutKind,
}

/// Where a new session runs (t3code's env-mode: `local | worktree`). "Current
/// worktree" is NOT a third mode — it's `Local` when the picked ref is already
/// materialized as a worktree (the session reuses that checkout's path).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CheckoutKind {
    /// The space's own folder — or the picked ref's existing worktree.
    #[default]
    Local,
    /// A fresh isolated worktree created off the picked base ref on send.
    NewWorktree,
}

/// The resolved on-send checkout action (composer consumes this — see
/// [`Pickers::checkout_plan`]).
#[derive(Debug, Clone, PartialEq)]
pub enum CheckoutPlan {
    /// Run in the space folder as-is. `branch` is the checkout's branch (the
    /// picked or current ref), carried onto `createChat` so the session names
    /// it from the first frame; `None` = refs never loaded.
    CurrentCheckout { branch: Option<String> },
    /// Reuse the picked ref's existing worktree (a cwd override; no git).
    ReuseWorktree { path: String, branch: String },
    /// `CreateWorktree` off `base` on send (comet mints a `comet/<name>`
    /// branch). `base: None` = refs never loaded — send falls back to the
    /// space folder rather than failing.
    NewWorktree { base: Option<String> },
}

/// The fully-resolved run configuration the composer sends: concrete harness,
/// model and reasoning (never a "default" passthrough once the catalog is
/// loaded), plus the explicit non-default option picks.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResolvedRunConfig {
    pub harness: Option<HarnessId>,
    pub model: Option<String>,
    pub reasoning: Option<ReasoningLevel>,
    pub model_options: serde_json::Map<String, serde_json::Value>,
    pub runtime_mode: RuntimeMode,
}

impl ResolvedRunConfig {
    /// The `ChatConfig` recorded on `Mutate createChat` (needs a known harness).
    pub fn chat_config(&self) -> Option<ChatConfig> {
        Some(ChatConfig {
            harness: self.harness?,
            model: self.model.clone(),
            reasoning: self.reasoning,
            model_options: self.model_options.clone(),
            sandbox: self.runtime_mode.sandbox(),
            runtime_mode: self.runtime_mode,
        })
    }
}

// ---------------------------------------------------------------------------
// Pure: default resolution (no "Default" placeholders — a concrete pick always)
// ---------------------------------------------------------------------------

/// The harness's default model: the first catalog row (both curated catalogs
/// lead with the flagship — comet's `pickDefaultModel` Opus preference maps to
/// the same row here).
pub fn default_model(models: &[Model]) -> Option<&Model> {
    models.first()
}

/// A model's default reasoning: X-High when the ladder offers it (comet
/// `DEFAULT_REASONING = "xhigh"`), else High, else the ladder's first entry.
/// `None` only for ladder-less models (e.g. Haiku's thinking toggle instead).
pub fn default_reasoning(ladder: &[ReasoningLevel]) -> Option<ReasoningLevel> {
    // The recommended default is High (user-corrected — not X-High globally);
    // fall to Medium then the ladder's first entry for shorter ladders.
    if ladder.contains(&ReasoningLevel::High) {
        return Some(ReasoningLevel::High);
    }
    if ladder.contains(&ReasoningLevel::Medium) {
        return Some(ReasoningLevel::Medium);
    }
    ladder.first().copied()
}

/// Clamp a picked/remembered level to what the model actually offers: keep it
/// when the ladder lists it, else fall to the model's default (never a stale
/// or foreign level — comet use-run-config.ts's derived-model discipline).
pub fn clamp_reasoning(
    level: Option<ReasoningLevel>,
    ladder: &[ReasoningLevel],
) -> Option<ReasoningLevel> {
    match level {
        Some(level) if ladder.contains(&level) => Some(level),
        _ => default_reasoning(ladder),
    }
}

/// Shown under the model list while discovery has failed. Amber
/// `warning_muted`, not red: a state to resolve, not an error the user
/// caused (0.2a's rail caption is the worked example). No Retry button of
/// its own — the pane's existing Retry row is the escape hatch.
fn built_in_caption(harness: HarnessId) -> &'static str {
    match harness {
        HarnessId::ClaudeCode => "Built-in list — couldn't reach Claude",
        HarnessId::Codex => "Built-in list — couldn't reach Codex",
        HarnessId::Cursor => "Built-in list — couldn't reach Cursor",
        HarnessId::Grok => "Built-in list — couldn't reach Grok",
        HarnessId::Hermes => "Built-in list — couldn't reach Hermes",
        HarnessId::Mock => "Built-in list",
    }
}

fn caption_for(source: CatalogSource, harness: HarnessId) -> Option<&'static str> {
    match source {
        CatalogSource::Live => None,
        CatalogSource::BuiltIn => Some(built_in_caption(harness)),
    }
}

/// Whether images may be attached for `model`.
///
/// Split out of the entity so the rule is testable at all: the UI crate has no
/// gpui test context, so anything reachable only through `App` is verified by
/// the rendered check and nothing else. The part worth pinning here is the
/// direction of the unknown case.
///
/// `None` means no model has resolved yet, which is true on every boot until
/// the catalog lands. It must ALLOW — a gate that closed while the catalog was
/// in flight would disable the paperclip at startup for everyone. Same
/// direction as `Model::accepts_images`' own default, and for the same reason
/// (`.agents/rules/optional-wire-fields.md`): absent is "not said", never "no".
fn images_allowed(model: Option<&Model>) -> bool {
    model.map(|model| model.accepts_images).unwrap_or(true)
}

/// The one place the `ListModels` reply is decoded. Extracted so a test can
/// bind THIS function rather than re-deriving the same `from_value` call:
/// a test that decodes the wire literal into `ModelCatalog` on its own stays
/// green while the call site drifts, which is exactly what happened when the
/// reply gained its `{models, source}` envelope.
fn decode_models_reply(value: serde_json::Value) -> Result<ModelCatalog, serde_json::Error> {
    serde_json::from_value(value)
}

/// The one place the `ListHarnesses` reply is decoded, for the same reason as
/// [`decode_models_reply`] — and now with a second consumer, the Agents
/// settings pane, which reads the install fields off the same descriptors.
///
/// Two call sites here plus one there is exactly the shape that lets a reply
/// change break one reader and not the others. `apps/ios` does not decode this
/// reply at all (it carries a hardcoded `HarnessCatalog.swift`), so nothing
/// outside this function is watching the wire.
pub(crate) fn decode_harnesses_reply(
    value: serde_json::Value,
) -> Result<Vec<HarnessDescriptor>, serde_json::Error> {
    serde_json::from_value(value)
}

/// Applies a picker's field change to a chat config that is about to be
/// persisted: preserve the row's own runtime mode across the change (the
/// caller's `config` may already carry the draft's default, not the row's),
/// then re-derive `sandbox` from whatever mode `change` leaves behind. Doing
/// the derivation last is what keeps the two fields from disagreeing — a
/// closure that sets `runtime_mode` must not leave `sandbox` pointing at the
/// mode that used to be there.
fn apply_owned_fields(
    config: &mut ChatConfig,
    existing: Option<&ChatConfig>,
    change: impl FnOnce(&mut ChatConfig),
) {
    if let Some(existing) = existing {
        config.runtime_mode = existing.runtime_mode;
    }
    change(config);
    config.sandbox = config.runtime_mode.sandbox();
}

// ---------------------------------------------------------------------------
// Pure: labels + traits summary
// ---------------------------------------------------------------------------

pub fn reasoning_label(level: ReasoningLevel) -> &'static str {
    match level {
        ReasoningLevel::Minimal => "Minimal",
        ReasoningLevel::Low => "Low",
        ReasoningLevel::Medium => "Medium",
        ReasoningLevel::High => "High",
        ReasoningLevel::XHigh => "X-High",
        ReasoningLevel::Max => "Max",
        ReasoningLevel::Ultra => "Ultra",
        ReasoningLevel::Ultracode => "Ultracode",
        ReasoningLevel::Ultrathink => "Ultrathink",
    }
}

/// Chip labels for the permission axis. User vocabulary, not the wire's: the
/// menu says what the agent will do, never which policy string the provider is
/// sent.
pub fn runtime_mode_label(mode: RuntimeMode) -> &'static str {
    match mode {
        RuntimeMode::ApprovalRequired => "Ask first",
        RuntimeMode::AutoAcceptEdits => "Auto-accept edits",
        RuntimeMode::Auto => "Auto",
        RuntimeMode::FullAccess => "Full access",
    }
}

/// The one line under the chip row, describing the mode that is active.
/// `FullAccess` is the only one that names a removal, because it is the only
/// mode with no sandbox left behind it.
pub fn runtime_mode_caption(mode: RuntimeMode) -> &'static str {
    match mode {
        RuntimeMode::ApprovalRequired => "Every file change and command waits for you.",
        RuntimeMode::AutoAcceptEdits => {
            "Edits inside the workspace go ahead; the sandbox is the boundary."
        }
        RuntimeMode::Auto => "Edits go ahead, and the agent reviews its own calls where it can.",
        RuntimeMode::FullAccess => {
            "No sandbox and no approvals — the agent can change anything on this machine."
        }
    }
}

/// The TraitsPicker trigger summary: a non-default runtime mode + non-default
/// reasoning + non-default model option choices, joined with " · " (comet:
/// "High · 1M · Fast"). `None` when everything is at its default.
///
/// The mode leads, because it is the only one of the three that changes what
/// the agent is allowed to do to the machine.
/// Diameter of the context gauge. A plain number: it must not vary with the
/// palette, or a theme toggle reflows the traits row.
const CONTEXT_GAUGE_SIZE: f32 = 12.0;

/// Dev/testing knob: `COMET_CONTEXT_DEMO=<percent>` supplies a context reading
/// so the gauge can be checked at any fill without a provider.
///
/// Filling a real window costs a long session, and the states worth reviewing
/// — nearly empty, nearly full — are the expensive ones to reach. This only
/// fills in where there is no real reading, so it can never mask live data.
fn demo_context() -> Option<comet_proto::ContextUsage> {
    const WINDOW: u64 = 200_000;
    let percent: u64 = std::env::var("COMET_CONTEXT_DEMO").ok()?.parse().ok()?;
    Some(comet_proto::ContextUsage {
        prompt_tokens: WINDOW * percent.min(100) / 100,
        context_window: WINDOW,
    })
}

/// Points on the wedge outline for `fraction` of a disc, filled clockwise from
/// twelve o'clock: centre, then the rim sampled to the sweep's end.
///
/// Split out from the painter so the geometry is testable — the painter itself
/// only turns these into a path. Sampled rather than arced because a 12px rim
/// is a handful of device pixels: line segments at this size are
/// indistinguishable from a curve, and they avoid depending on how the fork's
/// `arc_to` handles a sweep past a quadrant.
fn gauge_wedge_points(centre: (f32, f32), radius: f32, fraction: f32) -> Vec<(f32, f32)> {
    let fraction = fraction.clamp(0.0, 1.0);
    if fraction <= 0.0 {
        return Vec::new();
    }
    // One segment per ~6 degrees, so the rim never shows a flat edge.
    let steps = ((fraction * 60.0).ceil() as usize).max(2);
    let sweep = fraction * std::f32::consts::TAU;
    let mut points = Vec::with_capacity(steps + 2);
    points.push(centre);
    for step in 0..=steps {
        // Zero at twelve o'clock, increasing clockwise: screen y grows
        // downward, so the vertical term is negated rather than added.
        let angle = sweep * (step as f32 / steps as f32);
        points.push((
            centre.0 + radius * angle.sin(),
            centre.1 - radius * angle.cos(),
        ));
    }
    points
}

fn paint_gauge_wedge(
    bounds: gpui::Bounds<gpui::Pixels>,
    fraction: f32,
    fill: gpui::Hsla,
    window: &mut Window,
) {
    let radius = CONTEXT_GAUGE_SIZE / 2.0;
    let centre = (
        f32::from(bounds.origin.x) + radius,
        f32::from(bounds.origin.y) + radius,
    );
    let points = gauge_wedge_points(centre, radius, fraction);
    let Some((first, rest)) = points.split_first() else {
        return; // nothing drawn at zero: an empty track is the honest picture
    };
    let mut path = PathBuilder::fill();
    path.move_to(point(px(first.0), px(first.1)));
    for (x, y) in rest {
        path.line_to(point(px(*x), px(*y)));
    }
    path.close();
    if let Ok(path) = path.build() {
        window.paint_path(path, fill);
    }
}

pub fn traits_summary(
    model: Option<&Model>,
    reasoning: Option<ReasoningLevel>,
    selections: &serde_json::Map<String, serde_json::Value>,
    mode: RuntimeMode,
) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    // The default mode is what every chat has always run; naming it here would
    // make the ordinary case read as a setting the user went and changed.
    if mode != RuntimeMode::default() {
        parts.push(runtime_mode_label(mode).to_string());
    }
    if let Some(level) = reasoning {
        parts.push(reasoning_label(level).to_string());
    }
    if let Some(model) = model {
        for option in &model.options {
            let Some(choice_id) = selections.get(&option.id).and_then(|v| v.as_str()) else {
                continue;
            };
            if choice_id == option.default_choice {
                continue;
            }
            if let Some(choice) = option.choices.iter().find(|c| c.id == choice_id) {
                parts.push(choice.label.clone());
            }
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" · "))
    }
}

// ---------------------------------------------------------------------------
// Pure: folder-browser navigation (used by the shell's add-space flow)
// ---------------------------------------------------------------------------

/// Parent of an absolute path; `None` at the filesystem root.
pub fn parent_path(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return None; // was "/" (or empty)
    }
    match trimmed.rfind('/') {
        Some(0) => Some("/".to_string()),
        Some(at) => Some(trimmed[..at].to_string()),
        None => None,
    }
}

/// Join a listing path and an entry name.
pub fn child_path(base: &str, name: &str) -> String {
    if base.ends_with('/') {
        format!("{base}{name}")
    } else {
        format!("{base}/{name}")
    }
}

/// Breadcrumb segments for a path: `(label, full path)`, root first.
pub fn breadcrumbs(path: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = vec![("/".to_string(), "/".to_string())];
    let mut acc = String::new();
    for segment in path.split('/').filter(|s| !s.is_empty()) {
        acc.push('/');
        acc.push_str(segment);
        out.push((segment.to_string(), acc.clone()));
    }
    out
}

/// Directory rows of a listing (files never render in the browser).
pub fn browser_rows(listing: &FolderListing) -> Vec<&comet_proto::FolderEntry> {
    listing.entries.iter().filter(|e| e.is_dir).collect()
}

// ---------------------------------------------------------------------------
// Entity
// ---------------------------------------------------------------------------

/// Which picker popover is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerKind {
    Branch,
    /// The checkout-kind dropdown in the composer footer (Current
    /// checkout/worktree | New worktree).
    Checkout,
    HarnessModel,
    /// The reasoning ladder + advertised model options. Its own chip and its
    /// own menu (user request) — it used to be a section pinned inside the
    /// harness/model popover.
    Reasoning,
    /// The permission axis (`RuntimeMode`). Its own chip and menu, for the
    /// same reason.
    Permissions,
}

pub struct Pickers {
    state: Entity<AppState>,
    config: DraftConfig,
    /// Sticky last-used picks (comet `comet.composer.defaults:v1`): seeds the
    /// new-chat chips and is rewritten on every new-chat pick.
    defaults: ComposerDefaults,
    /// Where [`Self::defaults`] persists (`{data_dir}/composer-defaults.json`);
    /// `None` before bootstrap stamps the state (writes are skipped).
    data_dir: Option<PathBuf>,
    /// Selection the draft picks belong to — switching chats drops them so a
    /// pick made in one chat never leaks into another.
    draft_owner: Option<ServerRef>,
    /// Space the branch draft/cache belong to (see the state observer).
    space_owner: Option<ServerRef>,
    owner_generation: u64,
    open: Option<PickerKind>,
    harnesses: Loadable<Vec<HarnessDescriptor>>,
    /// In-flight guard for [`Self::revalidate_harnesses`], so repeated opens
    /// queue no more than one refetch.
    harness_revalidating: bool,
    revalidate_task: Option<Task<()>>,
    models: HashMap<HarnessId, Loadable<ModelCatalog>>,
    refs: Loadable<Vec<RepoRef>>,
    /// Space id the `refs` slot belongs to (invalidated on space change).
    refs_space: Option<ServerRef>,
    /// Highlighted row in the open list (keyboard nav).
    active: usize,
    /// Whether the arrow keys have moved [`Self::active`] since the popover
    /// opened. Until they have, the model list re-seats the highlight on the
    /// selected row every frame — the catalog usually lands after the menu is
    /// already open, and an index chosen before the rows existed would leave
    /// row 0 lit next to the checked one.
    nav_touched: bool,
    /// Models-list scroll — keyboard nav keeps the highlighted row in view
    /// (`scroll_to_item`; the add-space palette standard).
    model_scroll: gpui::ScrollHandle,
    /// Shared search / URL / name input, reused across popovers.
    search: Entity<ComposerInput>,
    focus: FocusHandle,
    /// Re-open suppression after outside-click dismissal (the dismiss and the
    /// trigger click would otherwise toggle twice).
    suppressed: Option<(PickerKind, Instant)>,
    /// `COMET_OPEN_PICKER` boot: keep claiming focus until it sticks, so
    /// keyboard nav drives the data-side-opened popover (headless rigs have
    /// no synthetic pointer, but synthetic keys do arrive).
    boot_focus_pending: bool,
    load_task: Option<Task<()>>,
    /// Own slot: the refs load runs concurrently with the eager
    /// harness/model loads — sharing `load_task` would abort one mid-flight.
    refs_task: Option<Task<()>>,
    /// In-flight mid-session `SwitchRef` (the ref being switched to).
    switching: Option<String>,
    switch_task: Option<Task<()>>,
    /// Last mid-session switch failure (shown in the ref popover).
    switch_error: Option<String>,
    /// Harnesses whose model load the user cancelled from the slow-request
    /// toast.
    ///
    /// A cancelled slot holds an `Error`, which `ensure_models` refuses to
    /// reload — that is deliberate for a real failure (render re-runs
    /// `ensure_*` every frame, so a self-reloading error state would spam the
    /// engine). But a cancel is not a failure: the user stopped *this*
    /// attempt, not the feature. So the cancel is remembered here and the slot
    /// is re-armed on the next DISCRETE demand — opening the picker, or picking
    /// the harness — never from render.
    models_cancelled: std::collections::HashSet<HarnessId>,
    /// The same, for the agent catalog — one slot, so one flag.
    ///
    /// The catalog is the only one of the four toasted loads that needs a
    /// marker at all. The branch list, the folder browser and the accounts page
    /// all reload unconditionally on their discrete triggers (a forced reopen, a
    /// navigation, Retry/Refresh), so a cancelled slot there re-arms by
    /// construction. `ensure_harnesses` cannot: render calls it every frame, so
    /// it has to refuse to reload an `Error`, and a cancel leaves exactly that.
    harnesses_cancelled: bool,
    mutate_task: Option<Task<()>>,
    _search_events: Subscription,
    _state_observe: Subscription,
}

impl Pickers {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| ComposerInput::new("Search…", cx));
        let search_events = cx.subscribe(&search, |this: &mut Self, _, event, cx| match event {
            ComposerInputEvent::Edited => {
                this.active = 0;
                cx.notify();
            }
            ComposerInputEvent::Submitted => this.on_search_submit(cx),
            // Pasted images/files don't apply to a search box.
            ComposerInputEvent::PastedImages(_)
            | ComposerInputEvent::PastedPaths(_)
            | ComposerInputEvent::CursorMoved
            | ComposerInputEvent::ViewportChanged
            | ComposerInputEvent::MentionNavigate(_)
            | ComposerInputEvent::MentionAccept
            | ComposerInputEvent::MentionDismiss => {}
        });
        // Chat selection / config changes must re-render the chips (child views
        // only re-render on their own notify). A selection change also drops
        // the draft picks — they belonged to the previous chat/new-chat canvas.
        let state_observe = cx.observe(&state, |this: &mut Self, state, cx| {
            let selected = state.read(cx).selected_chat.clone();
            if selected != this.draft_owner {
                this.draft_owner = selected;
                this.owner_generation = this.owner_generation.wrapping_add(1);
                this.load_task = None;
                this.switch_task = None;
                this.mutate_task = None;
                this.config.harness = None;
                this.config.model = None;
                this.config.reasoning = None;
                this.config.model_options.clear();
                this.switch_error = None;
            }
            // A space switch invalidates the branch draft + cache — the folder
            // (and possibly the device) changed under them.
            let space = state.read(cx).selected_space.clone();
            if space != this.space_owner {
                this.space_owner = space;
                this.owner_generation = this.owner_generation.wrapping_add(1);
                this.refs_task = None;
                this.switch_task = None;
                this.config.branch = None;
                this.config.checkout = CheckoutKind::default();
                this.refs = Loadable::Idle;
                this.refs_space = None;
                // Catalogs are per-DEVICE (fetched from the space's host):
                // a space switch may land on another device, so refetch.
                this.harnesses = Loadable::Idle;
                this.models.clear();
            }
            cx.notify();
        });
        // Dev/testing knob:
        // `COMET_OPEN_PICKER=model|reasoning|permissions|branch|checkout` boots
        // with that popover open — synthetic input can't reach the app on
        // headless compositors, so captures need a data-side path.
        let open = match std::env::var("COMET_OPEN_PICKER").ok().as_deref() {
            Some("model") => Some(PickerKind::HarnessModel),
            // `traits` kept as an alias: it named the merged menu before the
            // reasoning ladder moved to its own chip.
            Some("reasoning") | Some("traits") => Some(PickerKind::Reasoning),
            Some("permissions") => Some(PickerKind::Permissions),
            Some("branch") => Some(PickerKind::Branch),
            Some("checkout") => Some(PickerKind::Checkout),
            _ => None,
        };
        // Sticky last-used picks: loaded synchronously so the very first frame
        // shows the remembered harness/model/reasoning, never a placeholder.
        let data_dir = state.read(cx).data_dir.clone();
        let defaults = data_dir
            .as_deref()
            .map(ComposerDefaults::load)
            .unwrap_or_default();
        let draft_owner = state.read(cx).selected_chat.clone();
        let space_owner = state.read(cx).selected_space.clone();
        Self {
            state,
            space_owner,
            owner_generation: 0,
            config: DraftConfig::default(),
            defaults,
            data_dir,
            draft_owner,
            open,
            harnesses: Loadable::Idle,
            harness_revalidating: false,
            revalidate_task: None,
            models: HashMap::new(),
            refs: Loadable::Idle,
            refs_space: None,
            active: 0,
            nav_touched: false,
            model_scroll: gpui::ScrollHandle::new(),
            search,
            focus: cx.focus_handle(),
            suppressed: None,
            boot_focus_pending: open.is_some(),
            load_task: None,
            refs_task: None,
            switching: None,
            switch_task: None,
            switch_error: None,
            models_cancelled: std::collections::HashSet::new(),
            harnesses_cancelled: false,
            mutate_task: None,
            _search_events: search_events,
            _state_observe: state_observe,
        }
    }

    /// Persist the sticky defaults (best-effort; picks are rare and tiny).
    fn save_defaults(&self) {
        if let Some(dir) = self.data_dir.as_deref()
            && let Err(err) = self.defaults.save(dir)
        {
            tracing::warn!(error = %err, "composer-defaults save failed");
        }
    }

    pub fn draft(&self) -> &DraftConfig {
        &self.config
    }

    /// Harness is locked once the chat exists (feature-inventory §1.7).
    fn harness_locked(&self, cx: &App) -> bool {
        self.state.read(cx).selected_chat.is_some()
    }

    /// Whether a probe has come back saying this harness cannot run.
    ///
    /// False whenever the catalog has not loaded or the probe has not landed —
    /// `Unknown` is not evidence of a problem, and blocking on it would make a
    /// working provider unpickable for the window before its probe returns.
    fn harness_unavailable(&self, harness: HarnessId) -> bool {
        self.harnesses
            .ready()
            .is_some_and(|list| harness_is_unavailable(list, harness))
    }

    fn engine(&self, cx: &App) -> Option<ServerClient> {
        self.state.read(cx).selected_client()
    }

    /// Effective harness: picked, or the chat's config, or the first listed.
    ///
    /// Public because the composer's `/` menu asks the same question the chips
    /// do — the command list belongs to whichever agent this turn will run on,
    /// and a menu answering for a different one offers commands that do not
    /// exist there.
    pub fn effective_harness(&self, cx: &App) -> Option<HarnessId> {
        if let Some(harness) = self.config.harness {
            return Some(harness);
        }
        if let Some(config) = self
            .state
            .read(cx)
            .selected_chat_row()
            .and_then(|c| c.config.as_ref())
        {
            return Some(config.harness);
        }
        // New-chat canvas: the remembered last-used harness (sticky defaults),
        // when the loaded catalog still offers it.
        if let Some(harness) = self.defaults.harness {
            let offered = match self.harnesses.ready() {
                Some(list) => visible_harnesses(list).iter().any(|d| d.id == harness),
                None => true, // catalog not loaded yet — trust the memory
            };
            if offered {
                return Some(harness);
            }
        }
        // Fall back to the first VISIBLE harness: the registry lists the mock
        // harness first, and resolving chips against it would boot the
        // new-chat canvas onto "Mock" instead of Claude Code + its default
        // model (it stays available under `COMET_HARNESS=mock`).
        self.harnesses
            .ready()
            .and_then(|list| visible_harnesses(list).first().map(|d| d.id))
    }

    /// Effective model id: the draft pick, the selected chat's config, or (on
    /// the new-chat canvas) the remembered last-used model for the harness.
    fn effective_model_id<'a>(&'a self, cx: &'a App) -> Option<&'a str> {
        if let Some(id) = self.config.model.as_deref() {
            return Some(id);
        }
        if let Some(chat) = self.state.read(cx).selected_chat_row() {
            return chat.config.as_ref().and_then(|c| c.model.as_deref());
        }
        let harness = self.effective_harness(cx)?;
        self.defaults.model_for(harness).map(|m| m.id.as_str())
    }

    /// Effective reasoning — always concrete once the model is known: the
    /// draft pick / chat config / remembered default, clamped to the selected
    /// model's ladder, falling back to the model's default level.
    fn effective_reasoning(&self, cx: &App) -> Option<ReasoningLevel> {
        let explicit = self.config.reasoning.or_else(|| {
            match self.state.read(cx).selected_chat_row() {
                Some(chat) => chat.config.as_ref().and_then(|c| c.reasoning),
                // New chat: the remembered last-used level.
                None => self.defaults.reasoning,
            }
        });
        if self.selected_model(cx).is_none() {
            // Catalog not loaded yet: show the explicit value as-is (nothing
            // to clamp against); it resolves to a concrete level on load.
            return explicit;
        }
        clamp_reasoning(explicit, &self.trait_ladder(cx))
    }

    /// The selected model — concrete from the moment the list loads: the
    /// effective id when the list still offers it, else the harness default
    /// (first row). Never `None` with a non-empty catalog.
    /// Whether the model this turn will run on takes image input.
    ///
    /// **True whenever we do not know**, and the direction is the whole point:
    /// `accepts_images` defaults true on the wire because a provider that does
    /// not report modality (Claude reports none at all) must not read as
    /// "cannot take images". A model nobody has heard of, or a catalog that has
    /// not landed yet, therefore keeps the attachment affordance rather than
    /// silently disabling it (`.agents/rules/optional-wire-fields.md`).
    /// The selected model's display name, when the catalog has resolved one.
    /// Used by copy that has to name the model rather than say "this model".
    pub fn selected_model_label(&self, cx: &App) -> Option<SharedString> {
        self.selected_model(cx)
            .map(|model| SharedString::from(model.label.clone()))
    }

    pub fn effective_accepts_images(&self, cx: &App) -> bool {
        images_allowed(self.selected_model(cx))
    }

    fn selected_model<'a>(&'a self, cx: &'a App) -> Option<&'a Model> {
        let harness = self.effective_harness(cx)?;
        let models = &self.models.get(&harness)?.ready()?.models;
        match self.effective_model_id(cx) {
            Some(id) => models
                .iter()
                .find(|m| m.id == id)
                .or_else(|| default_model(models)),
            None => default_model(models),
        }
    }

    /// The explicit (non-default) option picks: the chat's persisted
    /// selections for existing chats, the draft's for the new-chat canvas.
    fn explicit_options(&self, cx: &App) -> serde_json::Map<String, serde_json::Value> {
        match self
            .state
            .read(cx)
            .selected_chat_row()
            .and_then(|c| c.config.as_ref())
        {
            Some(config) => config.model_options.clone(),
            None => self.config.model_options.clone(),
        }
    }

    /// The chat's persisted mode for an existing chat, the draft's for the
    /// new-chat canvas.
    /// The context gauge: a disc that fills clockwise as the chat's context
    /// window fills, with the percentage on hover.
    ///
    /// Absent until the chat has completed a model request, because that is
    /// when a window size first exists — and absent for good on a provider
    /// that never publishes one. An unknown window is not an empty one, so
    /// there is nothing honest to draw.
    ///
    /// A disc rather than a ring: at this diameter a ring's stroke is about
    /// 2px, which reads as a smudge rather than a shape. One neutral tone at
    /// every level, deliberately — amber in this app means a state you must
    /// resolve (an unavailable agent, a failed update, an expired approval),
    /// and a context window at 85% is a fact, not a problem to fix.
    fn context_gauge(&self, theme: &Theme, cx: &App) -> Option<AnyElement> {
        let state = self.state.read(cx);
        let usage = state
            .session_for(state.selected_chat_id()?)
            .and_then(|s| s.context)
            .or_else(demo_context)?;
        let fraction = usage.fraction();
        let track = crate::theme::ink(0.12);
        let fill = theme.text_muted;
        Some(
            div()
                .id("context-gauge")
                .flex_none()
                .size(px(CONTEXT_GAUGE_SIZE))
                .rounded_full()
                .bg(track)
                .child(
                    canvas(
                        |_, _, _| (),
                        move |bounds, _, window, _| {
                            paint_gauge_wedge(bounds, fraction, fill, window);
                        },
                    )
                    .absolute()
                    .size_full(),
                )
                // Reuses the rail's tooltip card rather than a second one:
                // the chrome is identical and `row` exists only to re-key the
                // fade, so a constant that no rail index can collide with is
                // all this needs.
                .tooltip(move |_, cx| {
                    cx.new(|_| HarnessRowTooltip {
                        message: format!("Context window: {}%", usage.percent()).into(),
                        row: usize::MAX,
                    })
                    .into()
                })
                .into_any_element(),
        )
    }

    fn effective_runtime_mode(&self, cx: &App) -> RuntimeMode {
        match self
            .state
            .read(cx)
            .selected_chat_row()
            .and_then(|c| c.config.as_ref())
        {
            Some(config) => config.runtime_mode,
            None => self.config.runtime_mode,
        }
    }

    /// The fully-resolved config the composer threads into the Run request and
    /// `Mutate createChat`: concrete model + reasoning whenever the catalog is
    /// loaded (no "engine picks a default" passthrough).
    pub fn resolved(&self, cx: &App) -> ResolvedRunConfig {
        ResolvedRunConfig {
            harness: self.effective_harness(cx),
            model: self
                .selected_model(cx)
                .map(|m| m.id.clone())
                // Catalog not loaded (offline): still send the id we know.
                .or_else(|| self.effective_model_id(cx).map(str::to_string)),
            reasoning: self.effective_reasoning(cx),
            model_options: self.explicit_options(cx),
            runtime_mode: self.effective_runtime_mode(cx),
        }
    }

    /// Whether any picker popover is open (shell-side: session-nav shortcuts
    /// go quiet underneath an open popover instead of yanking the session out
    /// from under it).
    pub fn is_open(&self) -> bool {
        self.open.is_some()
    }

    // ---- open/close ----

    fn close(&mut self, cx: &mut Context<Self>) {
        if let Some(kind) = self.open.take() {
            self.suppressed = Some((kind, Instant::now()));
        }
        cx.notify();
    }

    /// Capture knob (`COMET_OPEN_DIALOG=model`): open the combined
    /// harness/model menu programmatically.
    pub fn open_model_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.open != Some(PickerKind::HarnessModel) {
            self.toggle(PickerKind::HarnessModel, window, cx);
        }
    }

    /// Put cancelled model slots back to `Idle` and immediately re-request
    /// them with `force: true`, so a slot the user gave up waiting on cannot
    /// come back to a discovery failure the engine cached while nobody was
    /// watching.
    ///
    /// Called only from discrete user demand (opening the picker, picking a
    /// harness), never from render: render runs `ensure_models` every frame, so
    /// re-arming there would restart the request the moment it was cancelled
    /// and the toast would never go away.
    fn rearm_cancelled_models(&mut self, cx: &mut Context<Self>) {
        for harness in rearm_cancelled(&mut self.models, &mut self.models_cancelled) {
            self.ensure_models(harness, true, cx);
        }
    }

    /// The catalog's half of [`Self::rearm_cancelled_models`], with the same
    /// only-a-still-`Error` slot guard: the marker can outlive the state it
    /// described (the Retry row sets `Idle` without going through the cancel
    /// path), and re-arming a `Ready` catalog would blank loaded rows into a
    /// skeleton and fire a second identical request.
    fn rearm_cancelled_harnesses(&mut self) {
        rearm_cancelled_slot(&mut self.harnesses, &mut self.harnesses_cancelled);
    }

    fn toggle(&mut self, kind: PickerKind, window: &mut Window, cx: &mut Context<Self>) {
        if self.open == Some(kind) {
            self.open = None;
            cx.notify();
            return;
        }
        // A just-dismissed popover's trigger click must not instantly reopen.
        if let Some((suppressed, at)) = self.suppressed.take()
            && suppressed == kind
            && at.elapsed() < Duration::from_millis(400)
        {
            cx.notify();
            return;
        }
        self.open = Some(kind);
        self.search.update(cx, |input, cx| {
            input.set_placeholder("Search…", cx);
            input.set_text("", cx);
        });
        // The keyboard-nav highlight starts ON the selected row — row 0
        // otherwise reads as a second active row (user report). The model menu
        // needs the same thing, and the catalog may not have landed yet when
        // this runs, so `nav_touched` lets the render re-seat the highlight
        // once the rows exist (user report: picking any model left the top row
        // highlighted on the next open).
        self.nav_touched = false;
        self.active = match kind {
            PickerKind::Checkout => match self.config.checkout {
                CheckoutKind::Local => 0,
                CheckoutKind::NewWorktree => 1,
            },
            PickerKind::Branch => self.selected_ref_index(cx),
            PickerKind::HarnessModel => self.selected_model_index(cx),
            PickerKind::Permissions => self.runtime_mode_index(cx),
            _ => 0,
        };
        if kind == PickerKind::HarnessModel {
            self.model_scroll.set_offset(gpui::Point::default());
            // Opening the menu IS asking for the models again.
            self.rearm_cancelled_models(cx);
        }
        // Searchable pickers focus the filter input (it sits inside the frame,
        // so the frame's key handler still sees arrows/Enter); the rest focus
        // the frame itself for pure keyboard nav.
        match kind {
            PickerKind::Branch => {
                self.switch_error = None; // stale mid-session failures don't linger
                let handle = self.search.read(cx).focus_handle(cx);
                self.search.update(cx, |input, cx| {
                    input.set_placeholder("Search refs…", cx);
                });
                window.focus(&handle, cx);
            }
            _ => window.focus(&self.focus, cx),
        }
        match kind {
            // Force: the checkout state moves under us (a send mints a
            // worktree+branch, terminals switch refs) — every open
            // revalidates, keeping stale rows visible until fresh ones land.
            PickerKind::Branch | PickerKind::Checkout => self.ensure_refs(true, cx),
            PickerKind::HarnessModel | PickerKind::Reasoning | PickerKind::Permissions => {
                // Opening any of the three IS asking for the catalog again.
                // The reasoning ladder and the permission modes both read the
                // harness descriptor, so they re-arm it too — unlike the model
                // re-arm above, which only the model menu owns.
                self.rearm_cancelled_harnesses();
                self.ensure_harnesses(cx);
                // Availability is probed in the background at engine boot,
                // while the catalog is fetched eagerly on the first render —
                // so the cached list is almost always the pre-probe snapshot.
                // Revalidating on open is what actually gets probe results in
                // front of the user (same stale-while-revalidate shape as the
                // refs arm above).
                self.revalidate_harnesses(cx);
                if let Some(harness) = self.effective_harness(cx) {
                    self.ensure_models(harness, false, cx);
                }
            }
        }
        cx.notify();
    }

    // ---- loads ----

    fn ensure_harnesses(&mut self, cx: &mut Context<Self>) {
        // Only load from Idle: `render` re-runs this every frame, so an Error
        // that could re-trigger a load would flip back to Loading before the
        // retry row ever painted (and spam the engine). Retry resets to Idle.
        if !matches!(self.harnesses, Loadable::Idle) {
            return;
        }
        let Some(engine) = self.engine(cx) else {
            return;
        };
        let generation = self.owner_generation;
        // A load is starting, so the cancel marker is spent — whatever re-armed
        // the slot has now been honoured. Same second-half-of-the-guard
        // reasoning as `ensure_models`.
        self.harnesses_cancelled = false;
        self.harnesses = Loadable::Loading;
        // Always a skeleton by construction (this loads only from `Idle`), so
        // the wait is the whole of what the user can see and Cancel is offered.
        let (request_id, cancelled) = toast::begin(cx, errors::Loading::Agents);
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let call = std::pin::pin!(
                engine
                    .client()
                    .call(methods::LIST_HARNESSES, serde_json::Value::Null)
            );
            // Losing this race drops the RPC future, which is what makes cancel
            // real — see `ensure_models`.
            let outcome = futures::future::select(call, cancelled).await;
            this.update(cx, |pickers, cx| {
                toast::end(cx, request_id);
                if pickers.owner_generation != generation {
                    return;
                }
                let result = match outcome {
                    futures::future::Either::Left((result, _)) => result,
                    futures::future::Either::Right(_) => {
                        pickers.harnesses =
                            Loadable::Error(toast::cancelled_message(errors::Loading::Agents));
                        // Ask for it again and it tries again — see
                        // `rearm_cancelled_harnesses`.
                        pickers.harnesses_cancelled = true;
                        cx.notify();
                        return;
                    }
                };
                pickers.harnesses = match result {
                    Ok(value) => match decode_harnesses_reply(value) {
                        Ok(list) => Loadable::Ready(list),
                        Err(err) => {
                            Loadable::Error(errors::decode_failure(errors::Loading::Agents, &err))
                        }
                    },
                    Err(err) => {
                        Loadable::Error(errors::load_failure(errors::Loading::Agents, &err))
                    }
                };
                if let Some(harness) = pickers.effective_harness(cx) {
                    pickers.ensure_models(harness, false, cx);
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Whether the cached catalog has an availability answer for every entry.
    ///
    /// `Loading` counts as unsettled as much as a `Unknown` row does: a picker
    /// opened while the very first fetch is still in flight would otherwise see
    /// nothing to revalidate, and the all-`Unknown` result would land with
    /// nothing left to correct it.
    fn harness_catalog_settled(&self) -> bool {
        harness_catalog_settled(&self.harnesses)
    }

    /// Poll the catalog while the picker is open and probes are still landing.
    ///
    /// Availability lands on the engine after boot, but the UI caches the
    /// catalog from its FIRST RENDER and only re-arms on a space switch or the
    /// Retry row — so without this the picker shows the pre-probe snapshot for
    /// the whole session and never greys anything out.
    ///
    /// A single refetch on open is not enough. Two windows leave the catalog
    /// stuck: opening before the initial fetch returns (nothing to revalidate
    /// yet), and a refetch that itself completes before the probes do. Both end
    /// with an all-`Unknown` catalog and no pending work to correct it, so an
    /// unusable provider stays selectable until the picker is closed and
    /// reopened. Retrying until the answer settles closes both.
    ///
    /// Driven by the picker OPENING, never by render: `ensure_harnesses` runs
    /// every frame, and a revalidation there would spawn an RPC per frame.
    ///
    /// Bounded three ways — it stops once every entry has an answer (a probed
    /// *failure* is an answer), when the picker closes, and after
    /// [`HARNESS_REVALIDATE_ATTEMPTS`] tries regardless. Hitting the cap just
    /// leaves the harness selectable, which is the same conservative state an
    /// unprobed harness always has.
    fn revalidate_harnesses(&mut self, cx: &mut Context<Self>) {
        if self.harness_revalidating || self.harness_catalog_settled() {
            return;
        }
        let Some(engine) = self.engine(cx) else {
            return;
        };
        let generation = self.owner_generation;
        self.harness_revalidating = true;
        // Deliberately never flipped to `Loading`: the rail keeps painting the
        // rows it already has, so an open never flashes to a skeleton.
        self.revalidate_task = Some(cx.spawn(async move |this, cx| {
            for _ in 0..HARNESS_REVALIDATE_ATTEMPTS {
                // Delay first: on the open that races the initial fetch, this
                // lets that request land instead of duplicating it.
                cx.background_executor()
                    .timer(HARNESS_REVALIDATE_INTERVAL)
                    .await;
                // Registered WITHOUT a cancel: the rail is painting its rows
                // throughout (this poll never flips the slot to `Loading`), so
                // stopping it would change nothing on screen. The wait is still
                // worth naming — it is the answer to "why is this row still
                // greyed out / still selectable?".
                // `_alive` is the registration's liveness handle, not a cancel
                // channel — nothing sends on it. Holding it to the end of the
                // iteration is what lets a dropped task retire its own entry.
                let (request_id, _alive) =
                    cx.update(|cx| toast::begin_uncancellable(cx, errors::Loading::Agents));
                let result = engine
                    .client()
                    .call(methods::LIST_HARNESSES, serde_json::Value::Null)
                    .await;
                // Outside the entity update so a `Pickers` dropped mid-poll
                // cannot strand the entry and leave the toast up for good.
                cx.update(|cx| toast::end(cx, request_id));
                let stop = this
                    .update(cx, |pickers, cx| {
                        if pickers.owner_generation != generation {
                            return true;
                        }
                        // A failed poll is silent: the rows on screen are still
                        // the best answer we have, and replacing them with an
                        // error would throw away a working catalog over a
                        // transient blip. Keep polling — it may be transient.
                        if let Ok(value) = result
                            && let Ok(list) = decode_harnesses_reply(value)
                        {
                            pickers.harnesses = Loadable::Ready(list);
                            cx.notify();
                        }
                        pickers.harness_catalog_settled() || !pickers.harness_picker_open()
                    })
                    .unwrap_or(true);
                if stop {
                    break;
                }
            }
            this.update(cx, |pickers, _| pickers.harness_revalidating = false)
                .ok();
        }));
    }

    /// Whether a popover that renders the harness catalog is currently open.
    /// Both read `availability`, so both are worth polling for.
    fn harness_picker_open(&self) -> bool {
        matches!(
            self.open,
            Some(PickerKind::HarnessModel)
                | Some(PickerKind::Reasoning)
                | Some(PickerKind::Permissions)
        )
    }

    /// `force` reaches the engine as-is and must never be `true` from a call
    /// made in `render`: the engine clears the harness's `DiscoveryCache` on
    /// a forced call, and render re-runs its eager load every frame — a
    /// `force` there would re-spawn a discovery subprocess per frame. Only a
    /// discrete user demand (the Retry row, re-arming a cancelled slot) may
    /// force; opening the picker and the eager render kick both pass `false`.
    /// `force` is read only after the Absent-or-Idle guard below, so a forced
    /// call still no-ops on a Loading/Ready/Error slot; the caller must clear
    /// the slot first.
    fn ensure_models(&mut self, harness: HarnessId, force: bool, cx: &mut Context<Self>) {
        // Absent or Idle only — same render-loop hazard as `ensure_harnesses`;
        // the retry row clears the map to re-arm.
        if self
            .models
            .get(&harness)
            .is_some_and(|slot| !matches!(slot, Loadable::Idle))
        {
            return;
        }
        let Some(engine) = self.engine(cx) else {
            return;
        };
        let generation = self.owner_generation;
        // A load is starting, so any cancel marker for this harness is spent —
        // whatever re-armed the slot (the Retry row, a space switch clearing
        // the map, `rearm_cancelled_models` itself) has now been honoured.
        //
        // Clearing it HERE rather than only in `rearm_cancelled_models` is what
        // keeps the marker from outliving its slot: left set, the next picker
        // open would put a freshly `Ready` slot back to `Idle`, blanking loaded
        // models into a skeleton and firing a second identical request.
        self.models_cancelled.remove(&harness);
        self.models.insert(harness, Loadable::Loading);
        // Registered so a wait longer than `SLOW_AFTER` becomes visible, and
        // so the toast's Cancel has something to resolve. `end` runs on every
        // path out, including the cancelled one.
        let (request_id, cancelled) = toast::begin(cx, errors::Loading::Models);
        cx.spawn(async move |this, cx| {
            let params = serde_json::json!({ "harness": harness, "force": force });
            let call = std::pin::pin!(engine.client().call(methods::LIST_MODELS, params));
            // Losing this race DROPS the RPC future, which is what makes cancel
            // real rather than cosmetic: `PendingGuard` turns the drop into a
            // `{id, cancel}` frame, so the engine stops working on it too.
            let outcome = futures::future::select(call, cancelled).await;
            this.update(cx, |pickers, cx| {
                toast::end(cx, request_id);
                if pickers.owner_generation != generation {
                    return;
                }
                let result = match outcome {
                    futures::future::Either::Left((result, _)) => result,
                    futures::future::Either::Right(_) => {
                        pickers.models.insert(
                            harness,
                            Loadable::Error(toast::cancelled_message(errors::Loading::Models)),
                        );
                        // Ask for it again and it tries again — see
                        // `rearm_cancelled_models`.
                        pickers.models_cancelled.insert(harness);
                        cx.notify();
                        return;
                    }
                };
                let loaded = match result {
                    Ok(value) => match decode_models_reply(value) {
                        Ok(catalog) => Loadable::Ready(catalog),
                        Err(err) => {
                            Loadable::Error(errors::decode_failure(errors::Loading::Models, &err))
                        }
                    },
                    Err(err) => {
                        Loadable::Error(errors::load_failure(errors::Loading::Models, &err))
                    }
                };
                if let Loadable::Ready(catalog) = &loaded {
                    let fresh = pickers.defaults.remember_labels(
                        catalog
                            .models
                            .iter()
                            .map(|m| (m.id.as_str(), m.label.as_str())),
                    );
                    if fresh {
                        pickers.save_defaults();
                    }
                }
                pickers.models.insert(harness, loaded);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// ListRefs for the selected SPACE's folder — targeted at the space's
    /// device (relay-forwarded when remote), keyed/invalidated by space id.
    /// Rows carry checkout state (`current`, `worktreePath`) so the picker can
    /// tag refs and the checkout-kind selector can offer worktree reuse.
    fn ensure_refs(&mut self, force: bool, cx: &mut Context<Self>) {
        let Some(space_owner) = self.state.read(cx).selected_space.clone() else {
            return;
        };
        let Some(space) = self.state.read(cx).selected_space_row().cloned() else {
            return;
        };
        if !space.git_detected {
            return;
        }
        let fresh = self.refs_space.as_ref() == Some(&space_owner);
        if fresh && matches!(self.refs, Loadable::Loading) {
            return; // a load is already in flight
        }
        // Non-forced (the footer's eager kick, re-run every render) only loads
        // from Idle: an Error must WAIT for an explicit retry/reopen (force),
        // or re-render would flip Error back to Loading before the retry row
        // ever paints — an eternal skeleton plus an RPC storm (user report:
        // "the ref dropdown never loads anything").
        if !force && fresh && !matches!(self.refs, Loadable::Idle) {
            return;
        }
        let Some(engine) = self.engine(cx) else {
            return;
        };
        let generation = self.owner_generation;
        // Stale-while-revalidate: a forced refresh of an already-loaded space
        // keeps the current rows on screen while the reload runs — a send that
        // just minted a worktree (or a terminal-side branch) appears on the
        // popover's next open without the list ever flashing to a skeleton.
        let revalidating = force && fresh && matches!(self.refs, Loadable::Ready(_));
        if !revalidating {
            self.refs = Loadable::Loading;
        }
        self.refs_space = Some(space_owner);
        // Cancel is offered only when the popover has nothing else to show. A
        // stale-while-revalidate refresh keeps its rows painted either way, so
        // there Cancel would be a control with no visible effect — and its
        // handler would have to decide whether to throw away a good list, which
        // is a choice not worth putting in front of anyone.
        let (request_id, waiter) = if revalidating {
            toast::begin_uncancellable(cx, errors::Loading::Branches)
        } else {
            toast::begin(cx, errors::Loading::Branches)
        };
        self.refs_task = Some(cx.spawn(async move |this, cx| {
            let mut params = serde_json::Map::new();
            params.insert(
                "repoPath".into(),
                serde_json::Value::String(space.path.clone()),
            );
            let call = std::pin::pin!(
                engine
                    .client()
                    .call(methods::LIST_REFS, serde_json::Value::Object(params))
            );
            // `None` is the cancelled arm; losing the select race drops the RPC
            // future, which is what stops the engine too. The revalidating arm
            // still holds `waiter`, because it is also the liveness handle that
            // retires this entry if the task is superseded.
            let result = if revalidating {
                let _alive = waiter;
                Some(call.await)
            } else {
                match futures::future::select(call, waiter).await {
                    futures::future::Either::Left((result, _)) => Some(result),
                    futures::future::Either::Right(_) => None,
                }
            };
            this.update(cx, |pickers, cx| {
                toast::end(cx, request_id);
                if pickers.owner_generation != generation {
                    return;
                }
                let Some(result) = result else {
                    // No marker to set: the branch list re-arms structurally,
                    // because every popover open calls `ensure_refs(true, …)`
                    // and a forced load reloads from any state.
                    pickers.refs =
                        Loadable::Error(toast::cancelled_message(errors::Loading::Branches));
                    cx.notify();
                    return;
                };
                pickers.refs = match result {
                    Ok(value) => match serde_json::from_value::<Vec<RepoRef>>(value) {
                        Ok(refs) => Loadable::Ready(refs),
                        Err(err) => {
                            Loadable::Error(errors::decode_failure(errors::Loading::Branches, &err))
                        }
                    },
                    Err(err) => {
                        Loadable::Error(errors::load_failure(errors::Loading::Branches, &err))
                    }
                };
                // Rows landed under an open, un-searched popover: re-home the
                // nav highlight to the selected row.
                if pickers.open == Some(PickerKind::Branch)
                    && pickers.search.read(cx).text().is_empty()
                {
                    pickers.active = pickers.selected_ref_index(cx);
                }
                cx.notify();
            })
            .ok();
        }));
    }

    // ---- selections ----

    fn pick_ref(&mut self, row: RepoRef, cx: &mut Context<Self>) {
        // Existing session: the pick SWITCHES the session's checkout (the
        // t3code mid-session `switchRef`) instead of updating the draft.
        if self.state.read(cx).selected_chat_row().is_some() {
            self.switch_session_ref(row, cx);
            return;
        }
        if row.worktree_path.is_some() {
            // Reuse the ref's existing worktree ("Current worktree") — the
            // t3code `reuseExistingWorktree` path.
            self.config.branch = Some(row.name.clone());
            self.config.checkout = CheckoutKind::Local;
        } else if self.config.checkout == CheckoutKind::NewWorktree || row.current {
            // Base pick for a new worktree, or the already-current ref.
            self.config.branch = Some(row.name.clone());
        } else {
            // Local mode + a plain non-current ref: CHECK OUT the space
            // folder (full t3code `switchRef` — picking `main` means "put my
            // local checkout on main", it must never flip the mode).
            self.switch_draft_ref(row, cx);
            return;
        }
        self.open = None;
        cx.notify();
    }

    /// Draft-mode checkout switch: `git checkout` in the SPACE's folder
    /// (relay-forwarded for remote spaces). Success records the pick and
    /// refreshes tags; failure keeps the popover open with git's message.
    fn switch_draft_ref(&mut self, row: RepoRef, cx: &mut Context<Self>) {
        if self.switching.is_some() {
            return; // one switch at a time
        }
        let Some(space) = self.state.read(cx).selected_space_row().cloned() else {
            return;
        };
        let Some(engine) = self.engine(cx) else {
            return;
        };
        let generation = self.owner_generation;
        self.switch_error = None;
        self.switching = Some(row.name.clone());
        let ref_name = row.name.clone();
        self.switch_task = Some(cx.spawn(async move |this, cx| {
            let mut params = serde_json::Map::new();
            params.insert(
                "repoPath".into(),
                serde_json::Value::String(space.path.clone()),
            );
            params.insert(
                "refName".into(),
                serde_json::Value::String(ref_name.clone()),
            );
            let result = engine
                .client()
                .call(methods::SWITCH_REF, serde_json::Value::Object(params))
                .await;
            this.update(cx, |pickers, cx| {
                if pickers.owner_generation != generation {
                    return;
                }
                pickers.switching = None;
                match result {
                    Ok(_) => {
                        pickers.config.branch = Some(ref_name);
                        pickers.open = None;
                        pickers.ensure_refs(true, cx);
                    }
                    Err(err) => pickers.switch_error = Some(errors::switch_failure(&err)),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Mid-session ref switch, two shapes (both t3code):
    ///
    /// - The picked ref already lives in ANOTHER worktree → RETARGET the
    ///   session onto that worktree (`reuseExistingWorktree`): a `setChatCwd`
    ///   + `setChatBranch` mutate, no git. Resume is cwd-scoped, so the next
    ///   run there starts a fresh harness conversation — the transcript
    ///   itself carries on.
    /// - Otherwise → `git checkout` in the SESSION's own cwd (`SwitchRef`,
    ///   relay-forwarded to the host device). The host's HEAD watcher
    ///   reconciles `chat.branch` to every device. Errors (dirty tree, ref
    ///   held by the MAIN checkout) keep the popover open with git's message.
    fn switch_session_ref(&mut self, row: RepoRef, cx: &mut Context<Self>) {
        if self.switching.is_some() {
            return; // one switch at a time
        }
        let Some(chat) = self.state.read(cx).selected_chat_row().cloned() else {
            return;
        };
        let Some(cwd) = chat.cwd.clone() else {
            return;
        };
        let Some(engine) = self.engine(cx) else {
            return;
        };
        let generation = self.owner_generation;
        if row.worktree_path.as_deref() == Some(cwd.as_str()) {
            // Already this session's worktree — nothing to do.
            self.open = None;
            cx.notify();
            return;
        }
        self.switch_error = None;
        self.switching = Some(row.name.clone());
        let ref_name = row.name.clone();
        let retarget = row.worktree_path.clone();
        // Which of the two operations ran decides the copy on failure: the
        // retarget arm mutates the document and never checks anything out.
        let is_retarget = retarget.is_some();
        self.switch_task = Some(cx.spawn(async move |this, cx| {
            let result = match retarget {
                // Reuse the ref's existing worktree: move the session there.
                Some(path) => {
                    let cwd_mutate = serde_json::json!({
                        "op": "setChatCwd",
                        "chatId": chat.id,
                        "cwd": path,
                    });
                    let branch_mutate = serde_json::json!({
                        "op": "setChatBranch",
                        "chatId": chat.id,
                        "branch": ref_name,
                    });
                    match engine.client().call(methods::MUTATE, cwd_mutate).await {
                        Ok(_) => engine.client().call(methods::MUTATE, branch_mutate).await,
                        Err(err) => Err(err),
                    }
                }
                // Plain ref: checkout in place on the chat's HOST device.
                None => {
                    let mut params = serde_json::Map::new();
                    params.insert("repoPath".into(), serde_json::Value::String(cwd));
                    params.insert(
                        "refName".into(),
                        serde_json::Value::String(ref_name.clone()),
                    );
                    engine
                        .client()
                        .call(methods::SWITCH_REF, serde_json::Value::Object(params))
                        .await
                }
            };
            this.update(cx, |pickers, cx| {
                if pickers.owner_generation != generation {
                    return;
                }
                pickers.switching = None;
                match result {
                    Ok(_) => {
                        pickers.open = None;
                        // Checkout state changed — refresh tags/current.
                        pickers.ensure_refs(true, cx);
                    }
                    // Two different operations reach here. Only the plain-ref
                    // arm ran a git checkout; the retarget arm mutated the
                    // document, where "check for uncommitted changes" is advice
                    // about a working tree that was never touched.
                    Err(err) => {
                        pickers.switch_error = Some(if is_retarget {
                            errors::session_move_failure(&err)
                        } else {
                            errors::switch_failure(&err)
                        })
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn pick_checkout(&mut self, kind: CheckoutKind, cx: &mut Context<Self>) {
        if kind == CheckoutKind::Local
            && self.config.checkout == CheckoutKind::NewWorktree
            && self.selected_ref_worktree().is_none()
            && self.selected_ref().is_some_and(|r| !r.current)
        {
            // Back to "Current checkout" with a non-current plain ref picked:
            // drop the pick (we don't checkout the main folder) — the current
            // branch takes over.
            self.config.branch = None;
        }
        self.config.checkout = kind;
        self.open = None;
        cx.notify();
    }

    fn pick_harness(&mut self, harness: HarnessId, cx: &mut Context<Self>) {
        if self.harness_locked(cx) {
            return;
        }
        // Guarded here as well as by the row's disabled rendering: the click
        // handler is attached unconditionally, so this is what actually makes
        // an unusable provider unpickable.
        if self.harness_unavailable(harness) {
            return;
        }
        if self.config.harness != Some(harness) {
            // The remembered model for this harness takes over via the
            // defaults fallback; a foreign pick must not linger.
            self.config.model = None;
            self.config.reasoning = None;
            self.config.model_options.clear();
        }
        self.config.harness = Some(harness);
        self.defaults.harness = Some(harness);
        self.save_defaults();
        self.model_scroll.set_offset(gpui::Point::default());
        // A different list of rows: hand the highlight back to whichever row
        // the new harness resolves to, however far the arrows had walked.
        self.nav_touched = false;
        // Picking the harness IS asking for its models again, so a cancelled
        // slot must not stay cancelled here either.
        self.rearm_cancelled_models(cx);
        self.ensure_models(harness, false, cx);
        cx.notify();
    }

    fn pick_model(&mut self, model_id: String, cx: &mut Context<Self>) {
        self.open = None;
        if self.state.read(cx).selected_chat.is_some() {
            // Existing chat: persist to the chat row (Mutate setChatConfig) —
            // survives restarts and syncs; next runs in this chat use it.
            self.update_chat_config(cx, move |config| config.model = Some(model_id));
        } else {
            // New chat: draft pick + sticky last-used memory for this harness.
            self.config.model = Some(model_id.clone());
            if let Some(harness) = self.effective_harness(cx) {
                let label = self
                    .models
                    .get(&harness)
                    .and_then(|l| l.ready())
                    .and_then(|catalog| catalog.models.iter().find(|m| m.id == model_id))
                    .map(|m| m.label.clone())
                    .unwrap_or_else(|| model_id.clone());
                self.defaults.remember_model(harness, model_id, label);
                self.save_defaults();
            }
        }
        cx.notify();
    }

    fn pick_reasoning(&mut self, level: ReasoningLevel, cx: &mut Context<Self>) {
        // Always a concrete selection (no toggle-back-to-default).
        if self.state.read(cx).selected_chat.is_some() {
            self.update_chat_config(cx, move |config| config.reasoning = Some(level));
        } else {
            self.config.reasoning = Some(level);
            self.defaults.reasoning = Some(level);
            self.save_defaults();
        }
        cx.notify();
    }

    /// The permission axis. Unlike reasoning, a pick here is **not** written
    /// into [`ComposerDefaults`]: `full-access` chosen once for one chat must
    /// not become the setting every later chat silently starts in.
    fn pick_runtime_mode(&mut self, mode: RuntimeMode, cx: &mut Context<Self>) {
        // Closes on pick, like the other dropdowns. It stayed open while it
        // was a chip row pinned inside the model menu, where closing would
        // have dismissed the menu the user was still working in.
        self.open = None;
        if self.state.read(cx).selected_chat.is_some() {
            // `apply_owned_fields` re-derives `sandbox` after the change, so
            // the row's two permission fields cannot be written disagreeing.
            self.update_chat_config(cx, move |config| config.runtime_mode = mode);
        } else {
            self.config.runtime_mode = mode;
        }
        cx.notify();
    }

    fn pick_option(
        &mut self,
        option_id: String,
        choice_id: String,
        default: bool,
        cx: &mut Context<Self>,
    ) {
        if self.state.read(cx).selected_chat.is_some() {
            self.update_chat_config(cx, move |config| {
                if default {
                    config.model_options.remove(&option_id);
                } else {
                    config
                        .model_options
                        .insert(option_id, serde_json::Value::String(choice_id));
                }
            });
        } else if default {
            self.config.model_options.remove(&option_id);
        } else {
            self.config
                .model_options
                .insert(option_id, serde_json::Value::String(choice_id));
        }
        cx.notify();
    }

    /// Apply `change` to the selected chat's effective config and persist it:
    /// optimistic row stamp (chips update on click) + `Mutate setChatConfig`
    /// (LWW workspace write — restarts and other devices see it). The written
    /// row always carries the CONCRETE resolved model/reasoning, with the
    /// reasoning re-clamped to the (possibly just-changed) model's ladder.
    fn update_chat_config(&mut self, cx: &mut Context<Self>, change: impl FnOnce(&mut ChatConfig)) {
        let Some(chat_id) = self
            .state
            .read(cx)
            .selected_chat
            .clone()
            .map(|id| id.local_id)
        else {
            return;
        };
        let resolved = self.resolved(cx);
        let Some(mut config) = resolved.chat_config() else {
            return; // harness unknown (catalog + chat row both missing) — nothing safe to write
        };
        let existing = self
            .state
            .read(cx)
            .selected_chat_row()
            .and_then(|c| c.config.as_ref())
            .cloned();
        apply_owned_fields(&mut config, existing.as_ref(), change);
        // Reasoning must stay concrete for whatever model the row now names —
        // same ladder resolution as [`Self::trait_ladder`] (model levels, else
        // the harness's advertised ladder).
        if let Some(catalog) = self.models.get(&config.harness).and_then(|l| l.ready()) {
            let mut ladder = config
                .model
                .as_deref()
                .and_then(|id| catalog.models.iter().find(|m| m.id == id))
                .map(|m| m.reasoning_levels.clone())
                .unwrap_or_default();
            if ladder.is_empty()
                && let Some(descriptor) = self
                    .harnesses
                    .ready()
                    .and_then(|list| list.iter().find(|d| d.id == config.harness))
            {
                ladder = descriptor.capabilities.reasoning_levels.clone();
            }
            if !ladder.is_empty() {
                config.reasoning = clamp_reasoning(config.reasoning, &ladder);
            }
        }
        self.state.update(cx, |state, cx| {
            state.apply_chat_config(&chat_id, config.clone());
            cx.notify();
        });
        let Some(engine) = self.engine(cx) else {
            return;
        };
        self.mutate_task = Some(cx.spawn(async move |_, _| {
            let params = serde_json::json!({
                "op": "setChatConfig",
                "chatId": chat_id,
                "config": config,
            });
            if let Err(err) = engine.client().call(methods::MUTATE, params).await {
                tracing::warn!(error = %err, "setChatConfig mutate failed");
            }
        }));
    }

    // ---- keyboard ----

    /// The traits popover's reasoning ladder (model levels, falling back to
    /// the harness's advertised ladder) — shared by render and keyboard nav.
    fn trait_ladder(&self, cx: &App) -> Vec<ReasoningLevel> {
        let Some(model) = self.selected_model(cx) else {
            return Vec::new();
        };
        if !model.reasoning_levels.is_empty() {
            return model.reasoning_levels.clone();
        }
        self.effective_harness(cx)
            .and_then(|h| {
                self.harnesses
                    .ready()
                    .and_then(|list| list.iter().find(|d| d.id == h))
                    .map(|d| d.capabilities.reasoning_levels.clone())
            })
            .unwrap_or_default()
    }

    /// The modes the viewed harness declares, in the order it declares them
    /// (safest first). Empty means "the harness has not said" — never
    /// "supports nothing" — so the section is hidden rather than offering a
    /// pick the provider would silently ignore.
    fn runtime_mode_choices(&self, cx: &App) -> Vec<RuntimeMode> {
        self.effective_harness(cx)
            .and_then(|h| {
                self.harnesses
                    .ready()
                    .and_then(|list| list.iter().find(|d| d.id == h))
                    .map(|d| d.capabilities.runtime_modes.clone())
            })
            .unwrap_or_default()
    }

    /// Whether the chat's provider delivers a denial's note to the model.
    /// `false` while the descriptor list is still loading: under-promising for
    /// a frame is recoverable, promising delivery wrongly is not.
    pub fn carries_deny_note(&self, cx: &App) -> bool {
        self.effective_harness(cx)
            .and_then(|h| {
                self.harnesses
                    .ready()
                    .and_then(|list| list.iter().find(|d| d.id == h))
                    .map(|d| d.capabilities.carries_deny_note)
            })
            .unwrap_or(false)
    }

    /// The viewed harness's model list, when loaded (keyboard nav rows).
    fn model_rows_len(&self, cx: &App) -> usize {
        self.effective_harness(cx)
            .and_then(|h| self.models.get(&h))
            .and_then(|l| l.ready())
            .map(|catalog| catalog.models.len())
            .unwrap_or(0)
    }

    /// Row index of the resolved model pick, for the keyboard-nav highlight.
    /// Falls back to 0, which is also what the row render treats as selected
    /// when nothing is picked yet.
    fn selected_model_index(&self, cx: &App) -> usize {
        let selected = self.selected_model(cx).map(|m| m.id.clone());
        let ids: Vec<&str> = self
            .effective_harness(cx)
            .and_then(|h| self.models.get(&h))
            .and_then(|l| l.ready())
            .map(|catalog| catalog.models.iter().map(|m| m.id.as_str()).collect())
            .unwrap_or_default();
        model_highlight_index(&ids, selected.as_deref())
    }

    /// Row index of the active permission mode, for the keyboard highlight.
    fn runtime_mode_index(&self, cx: &App) -> usize {
        let current = self.effective_runtime_mode(cx);
        self.runtime_mode_choices(cx)
            .iter()
            .position(|mode| *mode == current)
            .unwrap_or(0)
    }

    /// Enter on the harness/model popover: pick the highlighted model.
    fn activate_model_row(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self
            .effective_harness(cx)
            .and_then(|h| self.models.get(&h))
            .and_then(|l| l.ready())
            .and_then(|catalog| catalog.models.get(self.active))
            .map(|m| m.id.clone())
        else {
            return;
        };
        self.pick_model(id, cx);
    }

    fn filtered_ref_rows(&self, cx: &App) -> Vec<RepoRef> {
        let Some(refs) = self.refs.ready() else {
            return Vec::new();
        };
        let names: Vec<String> = refs.iter().map(|r| r.name.clone()).collect();
        let query = self.search.read(cx).text().to_string();
        popover::filter_indices(&query, &names)
            .into_iter()
            .map(|ix| refs[ix].clone())
            .collect()
    }

    // ---- checkout resolution (the t3code env-mode semantics) ----

    /// Index of the highlighted-by-default row in the (filtered) ref list:
    /// the session's branch on an existing chat, the draft pick on a new one,
    /// else the current branch. Capped to the displayed window.
    fn selected_ref_index(&self, cx: &App) -> usize {
        let rows = self.filtered_ref_rows(cx);
        let selected = self
            .state
            .read(cx)
            .selected_chat_row()
            .and_then(|c| c.branch.clone())
            .or_else(|| self.config.branch.clone());
        let index = match selected {
            Some(name) => rows.iter().position(|r| r.name == name).unwrap_or(0),
            None => rows.iter().position(|r| r.current).unwrap_or(0),
        };
        index.min(MAX_REF_ROWS.saturating_sub(1))
    }

    /// The picked ref's row, else the repo's current branch's row.
    fn selected_ref(&self) -> Option<&RepoRef> {
        let refs = self.refs.ready()?;
        match self.config.branch.as_deref() {
            Some(name) => refs.iter().find(|r| r.name == name),
            None => refs.iter().find(|r| r.current),
        }
    }

    /// The picked (or current) ref's name.
    fn effective_ref_name(&self) -> Option<String> {
        self.config
            .branch
            .clone()
            .or_else(|| self.selected_ref().map(|r| r.name.clone()))
    }

    /// The existing worktree the picked ref is materialized in, if any.
    fn selected_ref_worktree(&self) -> Option<String> {
        self.selected_ref().and_then(|r| r.worktree_path.clone())
    }

    /// The resolved on-send checkout action for a new session.
    pub fn checkout_plan(&self) -> CheckoutPlan {
        match self.config.checkout {
            CheckoutKind::NewWorktree => CheckoutPlan::NewWorktree {
                base: self.effective_ref_name(),
            },
            CheckoutKind::Local => match self.selected_ref_worktree() {
                Some(path) => CheckoutPlan::ReuseWorktree {
                    path,
                    branch: self.effective_ref_name().unwrap_or_default(),
                },
                None => CheckoutPlan::CurrentCheckout {
                    branch: self.effective_ref_name(),
                },
            },
        }
    }

    /// Label of the checkout-kind trigger (t3code `resolveEnvModeLabel` /
    /// `resolveCurrentWorkspaceLabel`).
    fn checkout_label(&self) -> &'static str {
        match self.config.checkout {
            CheckoutKind::NewWorktree => "New worktree",
            CheckoutKind::Local => {
                if self.selected_ref_worktree().is_some() {
                    "Current worktree"
                } else {
                    "Current checkout"
                }
            }
        }
    }

    /// Label of the ref trigger: `From <ref>` only when a NEW worktree will be
    /// created off it (t3code `getBranchTriggerLabel`); the bare name otherwise.
    fn ref_label(&self) -> SharedString {
        match (self.config.checkout, self.effective_ref_name()) {
            (_, None) => SharedString::from("Select ref"),
            (CheckoutKind::NewWorktree, Some(name)) => SharedString::from(format!("From {name}")),
            (CheckoutKind::Local, Some(name)) => SharedString::from(name),
        }
    }

    fn on_search_submit(&mut self, cx: &mut Context<Self>) {
        if self.open == Some(PickerKind::Branch)
            && let Some(row) = self.filtered_ref_rows(cx).into_iter().nth(self.active)
        {
            self.pick_ref(row, cx);
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &Window, cx: &mut Context<Self>) {
        let key = popover::classify_key(
            event.keystroke.key.as_str(),
            event.keystroke.modifiers.platform,
            event.keystroke.modifiers.control,
        );
        let search_focused = self.search.read(cx).focus_handle(cx).is_focused(window);
        match key {
            MenuKey::Escape => {
                self.open = None;
                cx.notify();
            }
            MenuKey::Up | MenuKey::Down => {
                let delta = if key == MenuKey::Up { -1 } else { 1 };
                let count = match self.open {
                    Some(PickerKind::Branch) => self.filtered_ref_rows(cx).len().min(MAX_REF_ROWS),
                    Some(PickerKind::Checkout) => 2,
                    Some(PickerKind::HarnessModel) => self.model_rows_len(cx),
                    // Permissions is a row dropdown, so it walks. The
                    // reasoning ladder and model options are segmented chips,
                    // and stay mouse-only.
                    Some(PickerKind::Permissions) => self.runtime_mode_choices(cx).len(),
                    Some(PickerKind::Reasoning) => 0,
                    None => 0,
                };
                (self.active, self.nav_touched) =
                    menu_nav(self.active, self.nav_touched, count, delta);
                // Keep the highlighted MODEL row in view (the rows are the
                // scroll container's direct children, so indices map 1:1);
                // the traits chips below live in the pinned tray and never
                // need scrolling into view.
                if self.open == Some(PickerKind::HarnessModel)
                    && self.active < self.model_rows_len(cx)
                {
                    self.model_scroll.scroll_to_item(self.active);
                }
                cx.notify();
            }
            MenuKey::Enter if !search_focused => {
                if self.open == Some(PickerKind::HarnessModel) {
                    self.activate_model_row(cx);
                } else if self.open == Some(PickerKind::Permissions) {
                    if let Some(mode) = self.runtime_mode_choices(cx).get(self.active).copied() {
                        self.pick_runtime_mode(mode, cx);
                    }
                } else if self.open == Some(PickerKind::Checkout) {
                    let kind = if self.active == 0 {
                        CheckoutKind::Local
                    } else {
                        CheckoutKind::NewWorktree
                    };
                    self.pick_checkout(kind, cx);
                } else {
                    self.on_search_submit(cx);
                }
            }
            _ => {}
        }
    }

    // ---- render ----

    fn trigger_chip(
        &self,
        kind: PickerKind,
        label: SharedString,
        set: bool,
        chip_icon: Option<(&'static str, Option<gpui::Hsla>)>,
        suffix: Option<SharedString>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let id: &'static str = match kind {
            PickerKind::Branch => "picker-branch",
            PickerKind::Checkout => "picker-checkout",
            PickerKind::HarnessModel => "picker-model",
            PickerKind::Reasoning => "picker-reasoning",
            PickerKind::Permissions => "picker-permissions",
        };
        let open = self.open == Some(kind);
        // Ghost pill (comet composer/styles.tsx `pill`): `h-8 rounded-lg px-2.5
        // gap-1.5 text-[12px] font-medium text-muted-foreground`, icons size-4,
        // hover/open wash — no border, no caret; the actions row stays quiet.
        div()
            .id(id)
            .h(px(32.0))
            .max_w(px(208.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .px(px(10.0))
            .rounded(px(8.0))
            .text_size(px(12.0))
            .font_weight(gpui::FontWeight::MEDIUM)
            // comet composer/styles.tsx `pill`: `transition-colors` — the wash
            // and text brighten fade over 150ms.
            .text_color(motion::hover_blend(
                id,
                if set {
                    theme.text.opacity(0.9)
                } else {
                    theme.text_muted
                },
                theme.text,
            ))
            .bg(if open {
                theme.element_hover
            } else {
                motion::hover_blend(id, gpui::transparent_black(), theme.element_hover)
            })
            .on_hover(motion::hover_listener(id))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, window, cx| this.toggle(kind, window, cx)))
            .when_some(chip_icon, |el, (path, tint)| {
                el.child(
                    crate::icons::icon(path)
                        .size(px(16.0))
                        .text_color(tint.unwrap_or(theme.text_muted)),
                )
            })
            .child(div().min_w_0().truncate().child(label))
            // The effort half of the combined model+effort chip: muted, no
            // icon (user request) — one button, two tones.
            .when_some(suffix, |el, suffix| {
                el.child(
                    div()
                        .flex_none()
                        .text_color(theme.text_muted.opacity(0.7))
                        .child(suffix),
                )
            })
    }

    /// A selector chip whose catalog hasn't landed yet: the same 32px box and
    /// 8px radius as [`Self::trigger_chip`], filled with the pulsing wash
    /// instead of a label, so nothing moves when the label arrives. Pulse math
    /// and wash are [`popover::skeleton_rows`]'; `slot` staggers the three so
    /// the row reads as one wave rather than three blinking blocks.
    ///
    /// Still clickable, deliberately: the menu behind it renders its own
    /// loading state, and for the model menu that is also where the Retry row
    /// appears if the fetch fails.
    fn skeleton_trigger(
        &self,
        kind: PickerKind,
        id: &'static str,
        width: f32,
        slot: usize,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let view = cx.entity_id();
        let phase = motion::staggered_phase(
            motion::pulse_delta(&motion::COMET_PULSE, view, cx),
            slot,
            0.08,
        );
        div()
            .id(id)
            .flex_none()
            .w(px(width))
            .h(px(32.0))
            .rounded(px(8.0))
            .bg(crate::theme::ink(0.04))
            .opacity(0.35 + 0.4 * motion::pulse_wave(phase))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, window, cx| this.toggle(kind, window, cx)))
    }

    /// A footer-row trigger (t3code ghost `Button size="xs"`): leading icon,
    /// truncating label, trailing chevron — smaller and quieter than the
    /// in-pill chips.
    fn footer_chip(
        &self,
        kind: PickerKind,
        id: &'static str,
        icon_path: &'static str,
        label: SharedString,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let open = self.open == Some(kind);
        div()
            .id(id)
            .h(px(20.0))
            .max_w(px(280.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .px(px(8.0))
            .rounded(px(6.0))
            .text_size(px(12.0))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(motion::hover_blend(
                id,
                theme.text_muted.opacity(0.7),
                theme.text.opacity(0.8),
            ))
            .bg(if open {
                theme.element_hover
            } else {
                motion::hover_blend(id, gpui::transparent_black(), theme.element_hover)
            })
            .on_hover(motion::hover_listener(id))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, window, cx| this.toggle(kind, window, cx)))
            .child(
                crate::icons::icon(icon_path)
                    .size(px(12.0))
                    .text_color(theme.text_muted.opacity(0.7)),
            )
            .child(div().min_w_0().truncate().child(label))
            .child(
                crate::icons::icon(crate::icons::ALT_ARROW_DOWN)
                    .size(px(12.0))
                    .text_color(theme.text_muted.opacity(0.5)),
            )
    }

    /// A read-only footer label (locked sessions — t3code's
    /// `resolveLockedWorkspaceLabel` span).
    fn footer_label(icon_path: &'static str, label: SharedString, theme: &Theme) -> gpui::Div {
        div()
            .h(px(20.0))
            .max_w(px(280.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .px(px(8.0))
            .text_size(px(12.0))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(theme.text_muted.opacity(0.6))
            .child(
                crate::icons::icon(icon_path)
                    .size(px(12.0))
                    .text_color(theme.text_muted.opacity(0.6)),
            )
            .child(div().min_w_0().truncate().child(label))
    }

    /// The composer footer row (t3code BranchToolbar): checkout-kind on the
    /// left, the ref selector right-aligned. `None` for non-git spaces. On an
    /// existing session both sides are read-only labels ("Worktree" /
    /// "Local checkout" + the chat's branch).
    pub fn render_footer(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = Theme::of(cx).clone();
        // A selected chat whose workspace row hasn't synced yet (the moment
        // right after send mints it) still renders the DRAFT footer — the
        // values are identical, so the toolbar never blinks through a
        // half-empty locked state.
        let (space, session) = {
            let state = self.state.read(cx);
            let space = state.selected_space_row().cloned()?;
            let session = state
                .selected_chat
                .as_ref()
                .and_then(|_| state.selected_chat_row().cloned());
            (space, session)
        };
        if !space.git_detected {
            return None;
        }
        let new_chat = session.is_none();

        // Refs feed both modes (draft labels, mid-session switch list) —
        // eager + idempotent.
        self.ensure_refs(false, cx);

        // Symmetric: the container's 8px gap sits above the toolbar; bleeding
        // 8 of the container's 16px bottom padding (mb -8) leaves 8 below —
        // equal air on both sides of the row.
        let row = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap(px(8.0))
            .px(px(10.0))
            .mb(px(-8.0));

        // The ref side is LIVE in both modes: draft pick on a new chat,
        // checkout switch on an existing session (t3code keeps its branch
        // selector interactive mid-session too).
        let ref_label = match &session {
            Some(chat) => chat
                .branch
                .clone()
                .map(SharedString::from)
                .unwrap_or_else(|| SharedString::from("Select ref")),
            None => self.ref_label(),
        };
        let mut overlay: Option<(PickerKind, AnyElement)> = match self.open {
            Some(PickerKind::Branch) => {
                let content = self.render_branch_popover(cx);
                Some((PickerKind::Branch, self.popover_frame(320.0, content, cx)))
            }
            Some(PickerKind::Checkout) if new_chat => {
                let content = self.render_checkout_popover(cx);
                Some((PickerKind::Checkout, self.popover_frame(224.0, content, cx)))
            }
            _ => None,
        };
        let ref_chip = self.footer_chip(
            PickerKind::Branch,
            "picker-branch",
            crate::icons::GIT_BRANCH,
            ref_label,
            &theme,
            cx,
        );
        let ref_side =
            attach_overlay_end(ref_chip, &mut overlay, PickerKind::Branch, "branch-popover");

        if let Some(chat) = &session {
            // The checkout KIND is fixed at creation (harness resume is
            // cwd-scoped — the session never moves folders): label only.
            let is_worktree = chat.cwd.as_deref().is_some_and(|cwd| cwd != space.path);
            let (icon_path, label) = if is_worktree {
                (crate::icons::FOLDER_WITH_FILES, "Worktree")
            } else {
                (crate::icons::FOLDER, "Local checkout")
            };
            let left = Self::footer_label(icon_path, SharedString::from(label), &theme);
            return Some(row.child(left).child(ref_side).into_any_element());
        }

        let kind_icon = match (self.config.checkout, self.selected_ref_worktree().is_some()) {
            (CheckoutKind::Local, false) => crate::icons::FOLDER,
            _ => crate::icons::FOLDER_WITH_FILES,
        };
        let kind_chip = self.footer_chip(
            PickerKind::Checkout,
            "picker-checkout",
            kind_icon,
            SharedString::from(self.checkout_label()),
            &theme,
            cx,
        );
        Some(
            row.child(attach_overlay(
                kind_chip,
                &mut overlay,
                PickerKind::Checkout,
                "checkout-popover",
            ))
            .child(ref_side)
            .into_any_element(),
        )
    }

    fn popover_frame(&self, width: f32, content: AnyElement, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        popover::popover_card(&theme)
            .w(px(width))
            .max_h(px(POPOVER_MAX_H))
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.on_key_down(event, window, cx)
            }))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close(cx)))
            .flex()
            .flex_col()
            .child(content)
            .into_any_element()
    }

    /// [`Self::popover_frame`] without the p-1 inset — the harness/model
    /// picker's rail + list panes bleed to the card edge (comet
    /// harness-model-picker.tsx `className="w-80 p-0"`).
    fn popover_frame_flush(
        &self,
        width: f32,
        content: AnyElement,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::of(cx).clone();
        popover::popover_card_flush(&theme)
            .w(px(width))
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.on_key_down(event, window, cx)
            }))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close(cx)))
            .flex()
            .flex_col()
            .child(content)
            .into_any_element()
    }

    fn search_box(&self, theme: &Theme) -> AnyElement {
        popover::search_input_frame(theme, self.search.clone().into_any_element())
            .into_any_element()
    }

    fn retry_row(
        &self,
        id: &'static str,
        message: &str,
        kind: PickerKind,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        popover::error_row(theme, message)
            .child(
                div()
                    .id(id)
                    .px(px(Theme::SPACE_SM))
                    .py(px(3.0))
                    .rounded(px(Theme::CONTROL_RADIUS))
                    .border_1()
                    .border_color(theme.border)
                    .text_color(theme.text)
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.element_hover))
                    .on_click(cx.listener(move |this, _, _, cx| match kind {
                        PickerKind::Branch | PickerKind::Checkout => this.ensure_refs(true, cx),
                        PickerKind::HarnessModel
                        | PickerKind::Reasoning
                        | PickerKind::Permissions => {
                            // Read the harness BEFORE clearing the catalog.
                            // With no explicit pick and nothing remembered,
                            // `effective_harness` falls back to the first
                            // visible row of `self.harnesses` — so resetting
                            // that to `Idle` first makes it answer `None`, the
                            // forced refetch never runs, and the reload that
                            // follows asks with `force: false` and gets the
                            // boot-cached failure straight back. Retry would
                            // look like it worked and change nothing.
                            let harness = this.effective_harness(cx);
                            this.harnesses = Loadable::Idle;
                            this.models.clear();
                            this.ensure_harnesses(cx);
                            // `force: true` — this is the escape hatch from a
                            // discovery failure the engine cached for the
                            // whole boot (`DiscoveryCache`): without it, this
                            // row refetches the same cached failure forever.
                            if let Some(harness) = harness {
                                this.ensure_models(harness, true, cx);
                            }
                        }
                    }))
                    .child(SharedString::from("Retry")),
            )
            .into_any_element()
    }

    /// The ref picker (t3code BranchToolbarBranchSelector): search on top,
    /// rows with right-aligned muted `current`/`worktree` tags, and a
    /// "Showing X of Y refs" footer when the list is capped.
    fn render_branch_popover(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        if self.state.read(cx).selected_space_row().is_none() {
            return div()
                .p(px(Theme::SPACE_SM))
                .text_size(px(12.0))
                .text_color(theme.text_faint)
                .child(SharedString::from("No space selected"))
                .into_any_element();
        }
        let rows = self.filtered_ref_rows(cx);
        let total = rows.len();
        let shown = total.min(MAX_REF_ROWS);
        // Existing session: the highlighted row is the SESSION's branch and a
        // pick switches the checkout (see `pick_ref`); a new chat highlights
        // the draft pick.
        let session_branch = self
            .state
            .read(cx)
            .selected_chat_row()
            .and_then(|c| c.branch.clone());
        let switching = self.switching.clone();
        let body: AnyElement =
            match &self.refs {
                Loadable::Loading | Loadable::Idle => {
                    popover::skeleton_rows("branch-skeleton", &theme, 4, cx.entity_id(), cx)
                }
                Loadable::Error(message) => {
                    let message = message.clone();
                    self.retry_row("branch-retry", &message, PickerKind::Branch, &theme, cx)
                }
                Loadable::Ready(_) if rows.is_empty() => div()
                    .p(px(Theme::SPACE_SM))
                    .text_size(px(12.0))
                    .text_color(theme.text_faint)
                    .child(SharedString::from("No refs found."))
                    .into_any_element(),
                Loadable::Ready(_) => {
                    let active = self.active;
                    let selected = session_branch.or_else(|| self.config.branch.clone());
                    div()
                        .id("branch-list")
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .max_h(px(224.0))
                        .overflow_y_scroll()
                        .children(rows.into_iter().take(MAX_REF_ROWS).enumerate().map(
                            |(ix, row)| {
                                let label: SharedString = row.name.clone().into();
                                let is_selected = selected.as_deref() == Some(row.name.as_str());
                                // Right-aligned muted tag (t3code `text-[10px]
                                // text-muted-foreground/45`): current beats worktree.
                                let tag: Option<&'static str> = if row.current {
                                    Some("current")
                                } else if row.worktree_path.is_some() {
                                    Some("worktree")
                                } else {
                                    None
                                };
                                let is_switching = switching.as_deref() == Some(row.name.as_str());
                                popover::menu_row_nav(
                                    &theme,
                                    is_selected,
                                    ix == active,
                                    format!("branch-row-{ix}"),
                                )
                                .id(("branch-row", ix))
                                .when(switching.is_some(), |el| el.opacity(0.55))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.pick_ref(row.clone(), cx);
                                }))
                                .child(div().flex_1().min_w_0().truncate().child(label))
                                .when(is_switching, |el| {
                                    el.child(
                                        div()
                                            .flex_none()
                                            .text_size(px(10.0))
                                            .text_color(theme.text_muted.opacity(0.6))
                                            .child(SharedString::from("switching…")),
                                    )
                                })
                                .when_some(tag, |el, tag| {
                                    el.child(
                                        div()
                                            .flex_none()
                                            .text_size(px(10.0))
                                            .text_color(theme.text_muted.opacity(0.45))
                                            .child(SharedString::from(tag)),
                                    )
                                })
                                .when(is_selected, |el| el.child(popover::menu_check(&theme)))
                            },
                        ))
                        .into_any_element()
                }
            };
        let mut popover = div()
            .flex()
            .flex_col()
            .child(self.search_box(&theme))
            .child(body);
        // Mid-session switch failure (dirty tree, ref checked out elsewhere):
        // git's own message, under a hairline.
        if let Some(error) = &self.switch_error {
            popover = popover.child(
                popover::menu_section().child(
                    div()
                        .px(px(Theme::SPACE_SM))
                        .py(px(4.0))
                        .text_size(px(11.0))
                        .text_color(theme.danger.opacity(0.9))
                        .child(SharedString::from(error.clone())),
                ),
            );
        }
        if total > shown {
            popover = popover.child(
                popover::menu_section().child(
                    div()
                        .px(px(Theme::SPACE_SM))
                        .py(px(4.0))
                        .text_size(px(11.0))
                        .text_color(theme.text_faint)
                        .child(SharedString::from(format!(
                            "Showing {shown} of {total} refs"
                        ))),
                ),
            );
        }
        popover.into_any_element()
    }

    /// The checkout-kind dropdown (t3code BranchToolbarEnvModeSelector): two
    /// rows — "Current checkout"/"Current worktree" (local) and "New worktree".
    fn render_checkout_popover(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let has_worktree = self.selected_ref_worktree().is_some();
        let local_label: &'static str = if has_worktree {
            "Current worktree"
        } else {
            "Current checkout"
        };
        let local_icon = if has_worktree {
            crate::icons::FOLDER_WITH_FILES
        } else {
            crate::icons::FOLDER
        };
        let options: [(CheckoutKind, &'static str, &'static str); 2] = [
            (CheckoutKind::Local, local_label, local_icon),
            (
                CheckoutKind::NewWorktree,
                "New worktree",
                crate::icons::FOLDER_WITH_FILES,
            ),
        ];
        let active = self.active;
        let current = self.config.checkout;
        div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .children(
                options
                    .into_iter()
                    .enumerate()
                    .map(|(ix, (kind, label, icon_path))| {
                        let is_selected = current == kind;
                        popover::menu_row_nav(
                            &theme,
                            is_selected,
                            ix == active,
                            format!("checkout-row-{ix}"),
                        )
                        .id(("checkout-row", ix))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.pick_checkout(kind, cx);
                        }))
                        .child(
                            crate::icons::icon(icon_path)
                                .size(px(14.0))
                                .text_color(theme.text_muted),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .child(SharedString::from(label)),
                        )
                        .when(is_selected, |el| el.child(popover::menu_check(&theme)))
                    }),
            )
            .into_any_element()
    }

    /// The combined harness + model switcher (comet harness-model-picker.tsx):
    /// a vertical harness rail of square brand-icon tabs on the left, the
    /// viewed harness's models on the right. On an existing chat the other
    /// tabs stay visible but disabled — the lock reads as a rule.
    fn render_harness_model_popover(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let locked = self.harness_locked(cx);
        let effective = self.effective_harness(cx);
        let model_scroll = self.model_scroll.clone();

        let rail: AnyElement = match &self.harnesses {
            Loadable::Loading | Loadable::Idle => div()
                .p(px(4.0))
                .child(popover::skeleton_rows(
                    "harness-skeleton",
                    &theme,
                    3,
                    cx.entity_id(),
                    cx,
                ))
                .into_any_element(),
            Loadable::Error(message) => {
                let message = message.clone();
                self.retry_row(
                    "harness-retry",
                    &message,
                    PickerKind::HarnessModel,
                    &theme,
                    cx,
                )
            }
            Loadable::Ready(list) => {
                let mut descriptors: Vec<HarnessDescriptor> = visible_harnesses(list);
                // The committed harness always gets its rail tab, even when
                // it's the (normally hidden) mock harness of a dev session.
                if let Some(effective) = effective
                    && !descriptors.iter().any(|d| d.id == effective)
                    && let Some(descriptor) = list.iter().find(|d| d.id == effective)
                {
                    descriptors.insert(0, descriptor.clone());
                }
                // The agent the session is committed to, named so a locked-out
                // row can say *why* it is inert instead of just looking faded.
                let committed_name: Option<SharedString> = effective
                    .and_then(|id| descriptors.iter().find(|d| d.id == id))
                    .map(|d| SharedString::from(d.name.clone()));
                // Vertical agents rail (the palette's Devices-rail language):
                // brand icon + name per row, active carries the glass ring.
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .p(px(4.0))
                    .child(popover::menu_heading(&theme, "Agents"))
                    .children(descriptors.into_iter().enumerate().map(|(ix, descriptor)| {
                        let harness = descriptor.id;
                        let is_viewed = effective == Some(harness);
                        // An unavailable provider greys out whether or not it
                        // is the viewed one: a committed harness whose CLI has
                        // gone missing is exactly the case worth surfacing.
                        //
                        // Kept apart from the lock because they are different
                        // facts that used to paint identically: "not installed
                        // on this machine" is durable and worth acting on,
                        // while "the session is already using another agent" is
                        // transient and needs no action. One shared 0.35 dim
                        // made them indistinguishable, and only one of the two
                        // ever explained itself.
                        let summary: Option<SharedString> = descriptor
                            .availability
                            .unavailable_summary()
                            .map(|s| SharedString::from(s.to_owned()));
                        let hint: Option<SharedString> = descriptor
                            .availability
                            .unavailable_hint()
                            .map(|h| SharedString::from(h.to_owned()));
                        let state = RailRowState::of(locked, is_viewed, summary.is_some());
                        let locked_out = state == RailRowState::LockedOut;
                        let is_disabled = state.is_disabled();
                        let (icon_path, tint) = harness_brand_icon(harness);
                        let name: SharedString = descriptor.name.clone().into();
                        div()
                            .id(("harness-tab", ix))
                            // Tall enough for the caption when there is one.
                            // Both heights are constants: an unavailable row is
                            // taller because it says more, never because of
                            // which appearance is painting it.
                            .h(px(if summary.is_some() { 42.0 } else { 30.0 }))
                            .px(px(8.0))
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(8.0))
                            .rounded(px(8.0))
                            .text_size(px(12.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(if is_viewed {
                                theme.text
                            } else {
                                theme.text_muted
                            })
                            .when(is_viewed, |el| {
                                el.bg(crate::theme::card_selected_bg())
                                    .shadow(crate::theme::card_selected_shadows())
                            })
                            // Only the LOCK dims wholesale. An unavailable row
                            // has to stay readable: it is carrying a caption
                            // the user is meant to act on, and 0.35 opacity
                            // drove that text under the contrast floor the
                            // theme's paired text tokens exist to guarantee.
                            // It reads as inert through `text_faint` (the
                            // documented disabled token, AA at 4.5:1) instead.
                            .when(locked_out, |el| el.opacity(0.35))
                            .when(summary.is_some(), |el| el.text_color(theme.text_faint))
                            .when(!is_disabled, |el| el.cursor_pointer())
                            // Hover must not replace the viewed row's selected
                            // fill with the weaker wash — that dims the active
                            // row under the pointer (same rule as the sidebar
                            // rows in shell.rs).
                            .when(!is_disabled && !is_viewed, |el| {
                                el.hover(|s| s.bg(crate::theme::ink(0.06)))
                            })
                            // Hover carries the FIX only. The state itself is
                            // in the caption, so this no longer has to restate
                            // it — which is what made the old tooltip a
                            // five-line block that covered the Retry control in
                            // the models pane behind it.
                            //
                            // Suppressed on the VIEWED row, where the models
                            // pane is already showing this same sentence a few
                            // pixels to the right: a tooltip that duplicates
                            // visible text earns nothing and lands on top of
                            // the Retry control while doing it.
                            .when_some(hint.filter(|_| !is_viewed), |el, hint| {
                                el.tooltip(move |_, cx| {
                                    cx.new(|_| HarnessRowTooltip {
                                        message: hint.clone(),
                                        row: ix,
                                    })
                                    .into()
                                })
                            })
                            // A locked row explains itself too. Without this it
                            // was the one dimmed state on screen with no way to
                            // find out why.
                            .when_some(
                                committed_name.clone().filter(|_| locked_out),
                                |el, committed| {
                                    el.tooltip(move |_, cx| {
                                        cx.new(|_| HarnessRowTooltip {
                                            message: format!(
                                                "This session is using {committed}. Start a new \
                                                 session to use a different agent."
                                            )
                                            .into(),
                                            row: ix,
                                        })
                                        .into()
                                    })
                                },
                            )
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.pick_harness(harness, cx);
                            }))
                            .child(
                                crate::icons::icon(icon_path)
                                    .size(px(16.0))
                                    .flex_none()
                                    .text_color(tint.unwrap_or(if is_viewed {
                                        theme.text
                                    } else {
                                        theme.text_muted
                                    })),
                            )
                            .child(
                                // The state rides UNDER the name, visible with
                                // no hover at all. The rail is 148px wide and
                                // this label is two words, so it fits where the
                                // old full-sentence reason could not — that
                                // width is exactly why the reason was banished
                                // to a tooltip in the first place.
                                div()
                                    .min_w_0()
                                    .flex()
                                    .flex_col()
                                    .gap(px(1.0))
                                    .child(div().min_w_0().truncate().child(name))
                                    .when_some(summary, |el, summary| {
                                        el.child(
                                            div()
                                                .min_w_0()
                                                .truncate()
                                                .text_size(px(10.0))
                                                .font_weight(gpui::FontWeight::NORMAL)
                                                // Amber, not red: an agent that
                                                // isn't installed is a state to
                                                // resolve, not an error the
                                                // user just caused.
                                                .text_color(theme.warning_muted)
                                                .child(summary),
                                        )
                                    }),
                            )
                    }))
                    .into_any_element()
            }
        };

        let _ = locked; // the lock still dims foreign rail rows above

        // Re-seat the keyboard highlight on the selected row until the arrows
        // move it. `toggle` can only guess when the catalog is still loading —
        // and a harness switch replaces the whole list under an already-open
        // menu — so row 0 would otherwise stay lit beside the checked row.
        if !self.nav_touched {
            self.active = self.selected_model_index(cx);
        }

        // The rows are collected FLAT — they become the scroll container's
        // direct children so `scroll_to_item(active)` maps 1:1 (the palette's
        // keyboard-follow standard).
        let model_children: Vec<AnyElement> = match effective.map(|h| (h, self.models.get(&h))) {
            Some((_, Some(Loadable::Ready(catalog)))) => {
                // The check mirrors the chip: the resolved concrete pick (draft
                // / chat config / remembered, else the harness default row).
                let selected = self.selected_model(cx).map(|m| m.id.clone());
                let active = self.active;
                let models = catalog.models.clone();
                models
                    .into_iter()
                    .enumerate()
                    .map(|(ix, model)| {
                        let label: SharedString = model.label.clone().into();
                        let description: Option<SharedString> =
                            model.description.clone().map(Into::into);
                        let id = model.id.clone();
                        let is_selected = selected.as_deref() == Some(model.id.as_str())
                            || (selected.is_none() && ix == 0);
                        popover::menu_row_nav(
                            &theme,
                            is_selected,
                            ix == active,
                            format!("model-row-{ix}"),
                        )
                        .when(is_selected || ix == active, |el| {
                            el.shadow(crate::theme::card_selected_shadows())
                        })
                        .id(("model-row", ix))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.pick_model(id.clone(), cx);
                        }))
                        .child(
                            // Name + 11px muted description subline, per
                            // harness-model-picker.tsx (`min-w-0 flex-1` column).
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .child(div().w_full().truncate().child(label))
                                .when_some(description, |el, description| {
                                    el.child(
                                        div()
                                            .w_full()
                                            .truncate()
                                            .text_size(px(11.0))
                                            .text_color(theme.text_muted.opacity(0.7))
                                            .child(description),
                                    )
                                }),
                        )
                        .when(is_selected, |el| el.child(popover::menu_check(&theme)))
                        .into_any_element()
                    })
                    .collect()
            }
            Some((_, Some(Loadable::Error(message)))) => {
                let message = message.clone();
                vec![self.retry_row(
                    "model-retry",
                    &message,
                    PickerKind::HarnessModel,
                    &theme,
                    cx,
                )]
            }
            _ => vec![
                div()
                    .px(px(8.0))
                    .py(px(24.0))
                    .text_size(px(12.0))
                    .text_color(theme.text_muted.opacity(0.6))
                    .text_center()
                    .child(SharedString::from("Loading models…"))
                    .into_any_element(),
            ],
        };

        // Quiet caption for a `BuiltIn` catalog — see `caption_for`. `None`
        // while loading/erroring too: those states already say their own
        // thing and don't need a second, contradictory line under them.
        let built_in_caption = effective.and_then(|h| {
            self.models
                .get(&h)
                .and_then(|l| l.ready())
                .and_then(|catalog| caption_for(catalog.source, h))
        });

        // The palette architecture: agents rail LEFT, models pane beside it,
        // legend footer under everything. FIXED height so harness switches and
        // loading skeletons don't resize the card. The reasoning/options tray
        // that used to be pinned under the models pane now has its own chip
        // and menu (user request), so this menu answers one question only:
        // which agent, which model.
        div()
            .h(px(420.0))
            .flex()
            .flex_col()
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_row()
                    .items_stretch()
                    .child(
                        div()
                            .w(px(148.0))
                            .flex_none()
                            .border_r_1()
                            .border_color(crate::theme::hairline(0.06))
                            .child(rail),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(
                                // Pinned heading (the palette's crumbs slot).
                                div()
                                    .flex_none()
                                    .px(px(4.0))
                                    .pt(px(4.0))
                                    .child(popover::menu_heading(&theme, "Models")),
                            )
                            .child(
                                // Models scroll — gutters on the WRAPPER,
                                // outside the scroll viewport (in-content
                                // bottom padding is eaten by the extent), and
                                // rows as DIRECT children so keyboard
                                // `scroll_to_item` indices line up.
                                div().flex_1().min_h_0().pb(px(4.0)).child(
                                    div()
                                        .id("model-menu-scroll")
                                        .size_full()
                                        .flex()
                                        .flex_col()
                                        .gap(px(2.0))
                                        .px(px(4.0))
                                        .overflow_y_scroll()
                                        .track_scroll(&model_scroll)
                                        .children(model_children),
                                ),
                            )
                            .when_some(built_in_caption, |el, caption| {
                                el.child(
                                    // Quiet, not an error: amber `warning_muted`,
                                    // same 11px as the row description line, no
                                    // Retry of its own (the pane's existing Retry
                                    // row is the escape hatch — Task 6 wires it).
                                    div()
                                        .flex_none()
                                        .px(px(8.0))
                                        .pb(px(4.0))
                                        .text_size(px(11.0))
                                        .text_color(theme.warning_muted)
                                        .child(SharedString::from(caption)),
                                )
                            }),
                    ),
            )
            .child(
                // The palette's legend footer, on the recessed band.
                div()
                    .flex_none()
                    .bg(popover::band())
                    .border_t_1()
                    .border_color(crate::theme::hairline(0.06))
                    .px(px(12.0))
                    .py(px(8.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(12.0))
                    .child(popover::key_hint_pair(
                        &theme,
                        crate::icons::ARROW_UP,
                        crate::icons::ARROW_DOWN,
                        "Navigate",
                    ))
                    .child(popover::key_hint(&theme, crate::icons::RETURN, "Select")),
            )
            .into_any_element()
    }

    /// The reasoning menu's body: the reasoning ladder plus every advertised
    /// model option, as headed segmented-chip sections. Selecting keeps the
    /// menu open; the active chip carries the wash + ring. Mouse-only.
    ///
    /// Permissions are NOT here — they are their own chip and menu (see
    /// [`Self::render_permissions_section`]). They used to share this tray
    /// because both hung off the one merged model popover.
    fn render_reasoning_sections(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let Some(model) = self.selected_model(cx).cloned() else {
            return div()
                .flex()
                .flex_col()
                .gap(px(4.0))
                .child(popover::skeleton_rows(
                    "traits-skeleton",
                    &theme,
                    3,
                    cx.entity_id(),
                    cx,
                ))
                .into_any_element();
        };
        let levels = self.trait_ladder(cx);
        // Display the effective level (draft pick or the chat's config), so
        // the ladder check mirrors the chip summary.
        let current = self.effective_reasoning(cx);

        let ladder: AnyElement = if levels.is_empty() {
            gpui::Empty.into_any_element()
        } else {
            div()
                .flex()
                .flex_col()
                .child(popover::menu_heading(&theme, "Reasoning"))
                .child(
                    div()
                        .px(px(4.0))
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .gap(px(4.0))
                        .children(levels.into_iter().enumerate().map(|(ix, level)| {
                            let is_active = current == Some(level);
                            trait_chip(&theme, is_active)
                                .id(("reasoning-row", ix))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.pick_reasoning(level, cx);
                                }))
                                .child(SharedString::from(reasoning_label(level)))
                        })),
                )
                .into_any_element()
        };

        let selections = self.explicit_options(cx);
        let options =
            div()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .children(model.options.iter().enumerate().map(|(opt_ix, option)| {
                    let selected_choice = selections
                        .get(&option.id)
                        .and_then(|v| v.as_str())
                        .unwrap_or(&option.default_choice)
                        .to_string();
                    let option_id = option.id.clone();
                    let default_choice = option.default_choice.clone();
                    div()
                        .flex()
                        .flex_col()
                        .child(popover::menu_heading(&theme, &option.label))
                        .child(
                            div()
                                .px(px(4.0))
                                .flex()
                                .flex_row()
                                .flex_wrap()
                                .gap(px(4.0))
                                .children(option.choices.iter().enumerate().map(
                                    |(choice_ix, choice)| {
                                        let is_active = selected_choice == choice.id;
                                        let choice_id = choice.id.clone();
                                        let option_id = option_id.clone();
                                        let is_default = choice.id == default_choice;
                                        trait_chip(&theme, is_active)
                                            .id(("trait-choice", opt_ix * 32 + choice_ix))
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.pick_option(
                                                    option_id.clone(),
                                                    choice_id.clone(),
                                                    is_default,
                                                    cx,
                                                );
                                            }))
                                            .child(SharedString::from(choice.label.clone()))
                                    },
                                )),
                        )
                }));

        // The body scrolls. `popover_card` is `overflow_hidden`, so a ladder
        // plus enough option groups to outrun the card would put the lower
        // groups permanently out of reach rather than merely off-card — the
        // pinned inspector this menu replaced carried its own
        // `overflow_y_scroll` for exactly that, and splitting it out lost it.
        // The bound is this element's own, so it holds however the frame
        // resolves its flex.
        div()
            .id("reasoning-menu-scroll")
            .flex()
            .flex_col()
            .gap(px(4.0))
            .pb(px(4.0))
            .max_h(px(REASONING_MENU_MAX_H))
            .overflow_y_scroll()
            .child(ladder)
            .child(options)
            .into_any_element()
    }

    /// The permission axis, as a plain dropdown (user request — it was a row
    /// of segmented chips while it lived inside the model menu): one row per
    /// mode the provider declares, each carrying its own caption, with the
    /// check on the active one. Empty when the provider declares no modes (see
    /// [`Self::runtime_mode_choices`]), which is what hides the chip.
    ///
    /// Colour carries nothing here (user request): every caption is the same
    /// muted tone, `full-access` included — the words already say what it
    /// removes, and the amber it used to get read as a warning about a mode
    /// the user had deliberately chosen.
    fn render_permissions_section(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let modes = self.runtime_mode_choices(cx);
        if modes.is_empty() {
            return gpui::Empty.into_any_element();
        }
        let current = self.effective_runtime_mode(cx);
        // Re-seat the highlight until the arrows move it, exactly as the model
        // list does. `toggle` seeds it too, but `runtime_mode_choices` reads
        // the harness catalog: seeded while that slot is momentarily unready
        // the seed falls back to 0, and row 0 lights up beside the checked row
        // — observed live with `full-access` picked.
        if !self.nav_touched {
            self.active = self.runtime_mode_index(cx);
        }
        let active = self.active;
        div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .children(modes.into_iter().enumerate().map(|(ix, mode)| {
                let is_selected = current == mode;
                popover::menu_row_nav(
                    &theme,
                    is_selected,
                    ix == active,
                    format!("runtime-mode-row-{ix}"),
                )
                .id(("runtime-mode-row", ix))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.pick_runtime_mode(mode, cx);
                }))
                .child(
                    // Label + 11px caption subline, the model rows' column.
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .w_full()
                                .truncate()
                                .child(SharedString::from(runtime_mode_label(mode))),
                        )
                        .child(
                            // No `truncate()`: the caption is the whole point
                            // of the row, and the card is sized to hold the
                            // longest one. Anything longer wraps rather than
                            // losing its end (user request).
                            div()
                                .w_full()
                                .text_size(px(11.0))
                                .text_color(theme.text_muted.opacity(0.7))
                                .child(SharedString::from(runtime_mode_caption(mode))),
                        ),
                )
                .when(is_selected, |el| el.child(popover::menu_check(&theme)))
            }))
            .into_any_element()
    }
}

/// A segmented choice chip for the traits inspector (reasoning ladder /
/// model options): the key-cap voice — every chip carries a faint fill so it
/// reads as a pressable segment (bare text read as labels, not buttons);
/// the active chip adds the app-wide wash + glass ring.
/// The caller adds id/click/label.
fn trait_chip(theme: &Theme, active: bool) -> gpui::Div {
    div()
        .h(px(24.0))
        .px(px(10.0))
        .rounded(px(6.0))
        .flex()
        .flex_row()
        .items_center()
        .text_size(px(11.5))
        .cursor_pointer()
        .when(active, |el| {
            el.bg(crate::theme::card_selected_bg())
                .text_color(theme.text)
        })
        .when(!active, |el| {
            el.bg(crate::theme::ink(0.04))
                .text_color(theme.text_muted.opacity(0.7))
                .hover(|s| s.bg(theme.element_hover))
        })
        .when(active, |el| {
            el.shadow(crate::theme::card_selected_shadows())
        })
}

/// Brand mark + optional tint for a harness (the Claude mark keeps its brand
/// orange even on the monochrome surface; the mock harness scripts
/// Claude-flavoured runs, so it wears the Claude mark).
/// What the model chip says when it has no name to show.
///
/// Two different failures reach the same empty label, and they are not the same
/// sentence: no harness at all (discovery failed on a fresh install) versus a
/// known harness whose model catalog did not land. Both open the same popover,
/// which is where the reason and the Retry row live — the chip only has to stop
/// being blank, and stop guessing.
fn unresolved_chip_label(has_harness: bool) -> &'static str {
    if has_harness {
        "Model unavailable"
    } else {
        "Agent unavailable"
    }
}

pub(crate) fn harness_brand_icon(harness: HarnessId) -> (&'static str, Option<gpui::Hsla>) {
    match harness {
        HarnessId::ClaudeCode | HarnessId::Mock => (
            crate::icons::CLAUDE_MARK,
            Some(crate::icons::claude_brand()),
        ),
        HarnessId::Codex => (crate::icons::OPENAI_MARK, None),
        HarnessId::Cursor => (crate::icons::CURSOR_MARK, None),
        // Untinted on purpose: the Grok logomark is monochrome, so `theme.text`
        // reproduces xAI's own black-on-light / white-on-dark pair. A brand
        // colour here would invent one they don't publish.
        HarnessId::Grok => (crate::icons::GROK_MARK, None),
        // Same reasoning as Grok immediately above: no embedded Hermes glyph
        // exists, and a neutral mark reads as "no brand mark yet" rather than
        // claiming a wrong one.
        HarnessId::Hermes => (crate::icons::WIDGET, None),
    }
}

/// Display-only toggle switch (comet branch-picker.tsx `Toggle`): an 18×32
/// pill whose knob slides right and track flips white when on. State is owned
/// by the parent row.
#[allow(dead_code)]
fn toggle_switch(theme: &Theme, on: bool) -> gpui::Div {
    div()
        .flex_none()
        .w(px(32.0))
        .h(px(18.0))
        .rounded_full()
        .bg(if on {
            theme.text
        } else {
            crate::theme::ink(0.15)
        })
        .relative()
        .child(
            div()
                .absolute()
                .top(px(2.0))
                .left(px(if on { 16.0 } else { 2.0 }))
                .size(px(14.0))
                .rounded_full()
                .bg(if on {
                    theme.on_solid
                } else {
                    crate::theme::ink(0.7)
                }),
        )
}

/// `COMET_HARNESS=mock` (the e2e/dev rig) opts the mock harness into the UI;
/// production launches never set it, so the mock never surfaces there.
fn mock_harness_enabled() -> bool {
    std::env::var("COMET_HARNESS")
        .ok()
        .as_deref()
        .map(str::trim)
        == Some("mock")
}

/// Production pickers AND chip resolution hide the mock harness — the
/// registry always lists it, but it must never surface in real UI (neither in
/// the picker rail nor as the eager default the chips resolve against).
/// `COMET_HARNESS=mock` shows it; otherwise it only remains when it's
/// literally all there is (a dev build with no real harness registered).
pub fn visible_harnesses(list: &[HarnessDescriptor]) -> Vec<HarnessDescriptor> {
    visible_harnesses_impl(list, mock_harness_enabled())
}

fn visible_harnesses_impl(list: &[HarnessDescriptor], allow_mock: bool) -> Vec<HarnessDescriptor> {
    if allow_mock {
        return list.to_vec();
    }
    let real: Vec<HarnessDescriptor> = list
        .iter()
        .filter(|d| d.id != HarnessId::Mock)
        .cloned()
        .collect();
    if real.is_empty() { list.to_vec() } else { real }
}

/// Whether a cached catalog still holds entries whose probe had not landed
/// when it was fetched.
///
/// This is the revalidate-on-open trigger. It has to go false once every entry
/// is probed, or the picker refetches on every single open forever.
///
/// Shared with the Agents settings pane, whose install line is absent for
/// exactly the entries this reports — `install` is populated by the same probe
/// that fills `availability`, so "unprobed" and "no path yet" are one state.
pub(crate) fn catalog_awaits_probes(list: &[HarnessDescriptor]) -> bool {
    list.iter().any(|d| d.availability.is_unprobed())
}

/// Whether the catalog slot holds an availability answer for every entry.
///
/// Only a `Ready` list with no unprobed row settles. `Idle` and `Loading` are
/// unsettled by construction — a picker opened while the first fetch is still
/// in flight has nothing to revalidate yet, and if that counted as settled the
/// all-`Unknown` result would land with nothing left to correct it.
fn harness_catalog_settled(slot: &Loadable<Vec<HarnessDescriptor>>) -> bool {
    slot.ready()
        .is_some_and(|list| !catalog_awaits_probes(list))
}

/// Put cancelled slots back to `Idle` so the next `ensure_*` reloads them.
/// Returns the harnesses actually re-armed, so a caller that needs to kick
/// off their reload itself (`rearm_cancelled_models`) knows which ones.
///
/// Re-arms **only a slot still holding an `Error`**. A marker can outlive the
/// state it described — a Retry, or a space switch clearing the map, reloads
/// the slot without going through the cancel path — and re-arming
/// unconditionally would then overwrite a `Ready` slot with `Idle`, blanking
/// loaded rows into a skeleton and firing a second identical request. The
/// marker is also cleared when a load starts (see `ensure_models`); this is the
/// second half of the same guard, for the window where no load has begun yet.
fn rearm_cancelled<T>(
    slots: &mut HashMap<HarnessId, Loadable<T>>,
    cancelled: &mut std::collections::HashSet<HarnessId>,
) -> Vec<HarnessId> {
    let mut rearmed = Vec::new();
    for harness in cancelled.drain() {
        if matches!(slots.get(&harness), Some(Loadable::Error(_))) {
            slots.insert(harness, Loadable::Idle);
            rearmed.push(harness);
        }
    }
    rearmed
}

/// [`rearm_cancelled`] for a surface with one slot rather than a map.
///
/// Same guard, same reason: the marker is spent whether or not it applied, and
/// only a slot still holding an `Error` is put back.
fn rearm_cancelled_slot<T>(slot: &mut Loadable<T>, cancelled: &mut bool) {
    if std::mem::take(cancelled) && matches!(slot, Loadable::Error(_)) {
        *slot = Loadable::Idle;
    }
}

/// Why a rail row cannot be picked, if it cannot.
///
/// The two inert states are genuinely different facts and must not paint the
/// same: [`Unavailable`] is durable, machine-wide, and worth acting on, while
/// [`LockedOut`] is a property of *this* session and needs no action at all.
///
/// [`Unavailable`]: RailRowState::Unavailable
/// [`LockedOut`]: RailRowState::LockedOut
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RailRowState {
    Pickable,
    /// The session is committed to a different agent.
    LockedOut,
    /// The CLI is missing or broken on this machine.
    Unavailable,
}

impl RailRowState {
    /// `unavailable` outranks the lock: a missing CLI stays true after this
    /// session ends, so it is the more useful of the two to report — and it is
    /// the one the *viewed* row can be in, which the lock never is.
    fn of(locked: bool, is_viewed: bool, unavailable: bool) -> Self {
        if unavailable {
            Self::Unavailable
        } else if locked && !is_viewed {
            Self::LockedOut
        } else {
            Self::Pickable
        }
    }

    fn is_disabled(self) -> bool {
        !matches!(self, Self::Pickable)
    }
}

/// Whether a probed harness came back unusable.
///
/// A harness the catalog does not list, and one whose probe has not landed,
/// both answer `false`. `Unknown` means *not probed yet*, never *broken*, so
/// treating it as unavailable would grey out every provider for the window
/// between the picker opening and the probes returning.
fn harness_is_unavailable(list: &[HarnessDescriptor], harness: HarnessId) -> bool {
    list.iter()
        .find(|d| d.id == harness)
        .is_some_and(|d| d.availability.is_unavailable())
}

/// What to do about a rail row that cannot be picked — the install hint for an
/// unavailable agent, or which agent the session is locked to.
///
/// Carries the ACTION, never the state: the row itself now shows the state in
/// a caption. The predecessor to this struct restated the whole failure here,
/// which produced a five-line block sitting on top of the models pane and the
/// Retry control the user needed to reach.
struct HarnessRowTooltip {
    message: SharedString,
    /// Distinct per rail row, so moving between two disabled harnesses re-runs
    /// the fade instead of reusing the previous row's animation.
    row: usize,
}

impl gpui::Render for HarnessRowTooltip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx);
        motion::fade_quick(
            ("harness-row-tooltip", self.row),
            div()
                // Wraps rather than truncates — one sentence, but a path in an
                // override hint can still be long.
                .max_w(px(260.0))
                .px(px(8.0))
                .py(px(6.0))
                .rounded(px(5.0))
                .border_1()
                .border_color(theme.border_strong)
                .bg(theme.surface_raised)
                .text_size(px(11.0))
                .text_color(theme.text_muted)
                .child(self.message.clone()),
        )
    }
}

/// Attach the (single) open popover overlay to its trigger chip.
fn attach_overlay(
    chip: gpui::Stateful<gpui::Div>,
    overlay: &mut Option<(PickerKind, AnyElement)>,
    kind: PickerKind,
    id: &'static str,
) -> gpui::Stateful<gpui::Div> {
    if overlay.as_ref().is_some_and(|(k, _)| *k == kind)
        && let Some((_, element)) = overlay.take()
    {
        return chip.child(popover::anchored_menu_above(id, element));
    }
    chip
}

/// [`attach_overlay`] with the menu RIGHT-ALIGNED to the trigger (t3code
/// `align="end"` — right-edge triggers like the ref picker open leftward).
fn attach_overlay_end(
    chip: gpui::Stateful<gpui::Div>,
    overlay: &mut Option<(PickerKind, AnyElement)>,
    kind: PickerKind,
    id: &'static str,
) -> gpui::Stateful<gpui::Div> {
    if overlay.as_ref().is_some_and(|(k, _)| *k == kind)
        && let Some((_, element)) = overlay.take()
    {
        return chip
            .relative()
            .child(popover::anchored_menu_above_end(id, element));
    }
    chip
}

impl Render for Pickers {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        // A COMET_OPEN_PICKER popover never went through `toggle`, so claim
        // its keyboard focus here (re-claim until it sticks — the shell's
        // first-paint fallback focuses the composer after our first render).
        if self.boot_focus_pending {
            match self.open {
                Some(PickerKind::Branch) => {
                    self.search.update(cx, |input, cx| {
                        input.set_placeholder("Search refs…", cx);
                    });
                    let handle = self.search.read(cx).focus_handle(cx);
                    if handle.is_focused(window) {
                        self.boot_focus_pending = false;
                    } else {
                        window.focus(&handle, cx);
                    }
                }
                Some(_) => {
                    if self.focus.is_focused(window) {
                        self.boot_focus_pending = false;
                    } else {
                        window.focus(&self.focus, cx);
                    }
                }
                None => self.boot_focus_pending = false,
            }
        }

        // Eager-load the harness catalog + effective harness's models so the
        // chip reads "Fable 5" (a concrete pick) before any popover opens.
        self.ensure_harnesses(cx);
        if let Some(harness) = self.effective_harness(cx) {
            // `render` runs every frame — `force: true` here would re-spawn a
            // discovery subprocess per frame. See `ensure_models`.
            self.ensure_models(harness, false, cx);
        }
        // A popover opened data-side (COMET_OPEN_PICKER) never went through
        // `toggle`, so kick its loads here (all ensure_* are idempotent).
        if matches!(
            self.open,
            Some(PickerKind::Branch) | Some(PickerKind::Checkout)
        ) && matches!(self.refs, Loadable::Idle)
        {
            self.ensure_refs(false, cx);
        }
        // Chip shows the model's display name alone (comet `modelText`); the
        // harness reads from the brand mark beside it. Never "Default model":
        // before the catalog lands the remembered label (or the configured id)
        // names the pick; the loaded list then resolves it to a concrete row.
        let model_label: SharedString = {
            let loaded = self.selected_model(cx).map(|m| m.label.clone());
            let label = loaded.or_else(|| {
                let remembered = self
                    .effective_harness(cx)
                    .and_then(|h| self.defaults.model_for(h));
                match self.effective_model_id(cx) {
                    Some(id) => Some(
                        remembered
                            .filter(|m| m.id == id)
                            .map(|m| m.label.clone())
                            .or_else(|| self.defaults.label_for(id).map(str::to_string))
                            .unwrap_or_else(|| id.to_string()),
                    ),
                    None => remembered.map(|m| m.label.clone()),
                }
            });
            label.map(SharedString::from).unwrap_or_default()
        };
        // Nothing to name. The skeleton branch below cannot cover this: it
        // treats `Error` as settled (deliberately — a skeleton that never
        // resolves is the forbidden forever-wait), so an errored catalog draws
        // the REAL chip. On a fresh install with nothing remembered the label
        // bottomed out empty and the mark fell back to Claude — a brand nobody
        // chose, beside no words at all.
        let harness = self.effective_harness(cx);
        let model_label = if model_label.is_empty() {
            SharedString::from(unresolved_chip_label(harness.is_some()))
        } else {
            model_label
        };
        let harness_icon: (&'static str, Option<gpui::Hsla>) = harness
            .map(harness_brand_icon)
            // No harness means no brand to draw. The triangle says the chip is
            // a problem to open, not a pick to change; the popover behind it
            // carries the reason and the Retry row.
            .unwrap_or((crate::icons::DANGER_TRIANGLE, None));
        // The reasoning chip is hidden when the model offers neither a ladder
        // nor any options — an empty menu is worse than no affordance.
        let ladder = self.trait_ladder(cx);
        let has_options = self
            .selected_model(cx)
            .is_some_and(|m| !m.options.is_empty());
        let reasoning_level = self.effective_reasoning(cx);
        let reasoning_chip_label: Option<SharedString> =
            (!ladder.is_empty() || has_options).then(|| {
                match reasoning_level {
                    Some(level) => SharedString::from(reasoning_label(level)),
                    // A model with options but no ladder still needs a way in.
                    None => SharedString::from("Options"),
                }
            });
        // All three selectors read the same two catalogs, so they resolve
        // together — see the skeleton branch below.
        let effective = self.effective_harness(cx);
        let catalog_pending = chips_pending(
            &self.harnesses,
            effective.is_some(),
            effective.as_ref().and_then(|h| self.models.get(h)),
        );
        // Same rule for permissions: hidden when the provider declares no
        // modes (`runtime_mode_choices`), which is what the old pinned section
        // did too.
        let runtime_mode = self.effective_runtime_mode(cx);
        let permissions_label: Option<SharedString> = (!self.runtime_mode_choices(cx).is_empty())
            .then(|| SharedString::from(runtime_mode_label(runtime_mode)));

        // Render the open popover's body first (mutable borrow), then the
        // chips. Branch/Checkout render in the composer FOOTER row (see
        // `render_footer`), not here.
        let mut overlay: Option<(PickerKind, AnyElement)> = match self.open {
            Some(PickerKind::Branch) | Some(PickerKind::Checkout) | None => None,
            Some(PickerKind::HarnessModel) => {
                let content = self.render_harness_model_popover(cx);
                Some((
                    PickerKind::HarnessModel,
                    self.popover_frame_flush(460.0, content, cx),
                ))
            }
            Some(PickerKind::Reasoning) => {
                let content = self.render_reasoning_sections(cx);
                Some((
                    PickerKind::Reasoning,
                    self.popover_frame(260.0, content, cx),
                ))
            }
            Some(PickerKind::Permissions) => {
                let content = self.render_permissions_section(cx);
                Some((
                    PickerKind::Permissions,
                    // Wide enough for the longest caption on one line
                    // ("No sandbox and no approvals — …"), the model menu's
                    // width. A narrower card cut the captions mid-sentence.
                    self.popover_frame(460.0, content, cx),
                ))
            }
        };

        // Left cluster (user request): model, then reasoning, then
        // permissions — each its own chip and its own menu, separated by a
        // hairline rule. The branch chip lives in the composer FOOTER row.
        //
        // While the catalog is still on its way all three render as
        // skeletons, and all three stay clickable — the menus behind them show
        // their own loading state, and the model menu is where the Retry row
        // lives if the fetch fails.
        if catalog_pending {
            let mut left = div()
                .flex()
                .flex_row()
                .items_center()
                .min_w_0()
                .gap(px(Theme::SPACE_SM));
            for (slot, (kind, id, width)) in [
                (PickerKind::HarnessModel, "picker-model", SKELETON_MODEL_W),
                (
                    PickerKind::Reasoning,
                    "picker-reasoning",
                    SKELETON_REASONING_W,
                ),
                (
                    PickerKind::Permissions,
                    "picker-permissions",
                    SKELETON_PERMISSIONS_W,
                ),
            ]
            .into_iter()
            .enumerate()
            {
                if slot > 0 {
                    left = left.child(chip_divider());
                }
                let chip = self.skeleton_trigger(kind, id, width, slot, cx);
                left = left.child(attach_overlay(chip, &mut overlay, kind, "picker-popover"));
            }
            return div()
                .w_full()
                .min_w_0()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap(px(Theme::SPACE_SM))
                .child(left)
                // The gauge rides the session, not the catalog, so it keeps
                // rendering while the chips are still resolving.
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .flex_none()
                        .gap(px(Theme::SPACE_SM))
                        .children(self.context_gauge(&theme, cx)),
                );
        }
        let model_chip = self.trigger_chip(
            PickerKind::HarnessModel,
            model_label,
            true,
            Some(harness_icon),
            None,
            &theme,
            cx,
        );
        let mut left = div()
            .flex()
            .flex_row()
            .items_center()
            .min_w_0()
            .gap(px(Theme::SPACE_SM))
            .child(attach_overlay(
                model_chip,
                &mut overlay,
                PickerKind::HarnessModel,
                "model-popover",
            ));
        if let Some(label) = reasoning_chip_label {
            let chip = self.trigger_chip(
                PickerKind::Reasoning,
                label,
                reasoning_level.is_some(),
                None,
                None,
                &theme,
                cx,
            );
            left = left.child(chip_divider()).child(attach_overlay(
                chip,
                &mut overlay,
                PickerKind::Reasoning,
                "reasoning-popover",
            ));
        }
        if let Some(label) = permissions_label {
            let chip =
                self.trigger_chip(PickerKind::Permissions, label, true, None, None, &theme, cx);
            left = left.child(chip_divider()).child(attach_overlay(
                chip,
                &mut overlay,
                PickerKind::Permissions,
                "permissions-popover",
            ));
        }
        // Right cluster: the context gauge. The composer appends attach + send
        // after this element (comet composer-actions.tsx arrangement).
        let right = div()
            .flex()
            .flex_row()
            .items_center()
            .flex_none()
            .gap(px(Theme::SPACE_SM))
            .children(self.context_gauge(&theme, cx));
        div()
            .w_full()
            .min_w_0()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap(px(Theme::SPACE_SM))
            .child(left)
            .child(right)
    }
}

/// The `|` between two selector chips: a hairline rule, not a glyph, so it
/// stays a separator at any type scale. A plain number for the height — it
/// must not vary with the palette (`.agents/rules/gpui-ui.md`).
fn chip_divider() -> gpui::Div {
    div()
        .flex_none()
        .w(px(1.0))
        .h(px(16.0))
        .bg(crate::theme::hairline(0.12))
}

/// Whether a catalog slot is still on its way to an answer. **`Error` is
/// settled**, deliberately: an errored catalog hands the real chips back (the
/// remembered label, and the popover's own Retry row), because a skeleton that
/// outlives its request is a wait with no end — the thing
/// `.agents/rules/user-facing-errors.md` forbids.
fn slot_pending<T>(slot: Option<&Loadable<T>>) -> bool {
    matches!(slot, None | Some(Loadable::Idle) | Some(Loadable::Loading))
}

/// Whether all three selectors should still render as skeletons.
///
/// The model slot is only asked when there is a harness to load models *for*.
/// A missing slot means two opposite things — "not requested yet" when a
/// harness is known, and "nothing to request" when none is — which is why the
/// harness is passed separately rather than read back out of the slot.
///
/// The case that bites: `effective_harness` bottoms out in the harness
/// catalog, so an **errored** catalog leaves no effective harness and hence no
/// model slot. Counting that absence as pending strands the chips as skeletons
/// with no request left in flight to end them, and the model menu's Retry row
/// sits unreachable behind them — the unbounded wait
/// `.agents/rules/user-facing-errors.md` forbids, arrived at by composition
/// even though [`slot_pending`] settles `Error` correctly on its own.
fn chips_pending<H, M>(
    harnesses: &Loadable<H>,
    has_harness: bool,
    model_slot: Option<&Loadable<M>>,
) -> bool {
    slot_pending(Some(harnesses)) || (has_harness && slot_pending(model_slot))
}

/// Row the model menu's keyboard highlight should sit on: the selected model,
/// or row 0 when nothing is picked yet (which is also the row the list draws
/// as checked in that case). An id the catalog no longer carries — a stale
/// remembered pick, or a list swapped by a harness switch — also lands on 0
/// rather than leaving the highlight where it was.
fn model_highlight_index(ids: &[&str], selected: Option<&str>) -> usize {
    let Some(selected) = selected else {
        return 0;
    };
    ids.iter().position(|id| *id == selected).unwrap_or(0)
}

/// One arrow press on an open menu: the row to highlight, and whether the
/// press counts as navigation.
///
/// A press against a menu with no rows navigated nothing, so it must not claim
/// it did. The chips stay clickable while the catalog is still on its way, and
/// the render-time re-seat that lands the highlight on the checked row is
/// gated on this flag — claim navigation here and the rows that arrive a
/// moment later keep the highlight on row 0, so Enter picks the first mode or
/// the first model instead of the one drawn as checked.
fn menu_nav(active: usize, nav_touched: bool, count: usize, delta: isize) -> (usize, bool) {
    match popover::menu_step(Some(active), count, delta) {
        Some(next) => (next, true),
        None => (active, nav_touched),
    }
}

/// Tallest a picker card grows before its body has to scroll — comet caps its
/// at min(640px, 75vh).
const POPOVER_MAX_H: f32 = 640.0;

/// The reasoning menu's scrolling body: the card's cap less its `p-1` inset
/// top and bottom (`popover::popover_card`). Derived rather than a second
/// literal, so raising the cap raises the body with it.
const REASONING_MENU_MAX_H: f32 = POPOVER_MAX_H - 2.0 * 4.0;

/// Skeleton widths, one per selector: roughly what each label occupies once
/// it lands ("Fable 5", "High", "Auto-accept edits"), so the row doesn't jump
/// when it does. Plain numbers — they must not vary with the palette
/// (`.agents/rules/gpui-ui.md`).
const SKELETON_MODEL_W: f32 = 104.0;
const SKELETON_REASONING_W: f32 = 56.0;
const SKELETON_PERMISSIONS_W: f32 = 128.0;

#[cfg(test)]
mod tests {
    use super::*;
    use comet_proto::{FolderEntry, Model, ModelOption, ModelOptionChoice, SandboxLevel};

    /// Every harness must name an icon the asset source can actually serve —
    /// `harness_brand_icon` hands back a `&'static str` path, so a typo or a
    /// mark that was never registered fails at paint time as a silently blank
    /// glyph rather than at compile time.
    ///
    /// Grok is called out because it spent a slice on the neutral `WIDGET`
    /// placeholder: the check is that it now names a mark of its own, not just
    /// that the path resolves.
    #[test]
    fn every_harness_names_a_loadable_mark_and_grok_has_its_own() {
        use crate::icons::Assets;
        use gpui::AssetSource as _;

        for harness in [
            HarnessId::ClaudeCode,
            HarnessId::Codex,
            HarnessId::Cursor,
            HarnessId::Grok,
            HarnessId::Hermes,
            HarnessId::Mock,
        ] {
            let (path, _) = harness_brand_icon(harness);
            assert!(
                Assets.load(path).unwrap().is_some(),
                "{harness:?} names an unserved icon: {path}"
            );
        }

        let (grok, tint) = harness_brand_icon(HarnessId::Grok);
        assert_eq!(grok, crate::icons::GROK_MARK);
        assert_ne!(
            grok,
            crate::icons::WIDGET,
            "Grok wears its own mark now, not the neutral placeholder"
        );
        // Untinted, so `theme.text` supplies xAI's black-on-light /
        // white-on-dark pair. A tint here would invent a brand colour.
        assert!(tint.is_none(), "the Grok mark must take the text colour");
    }

    /// Both failures reach the same empty label and they are not the same
    /// sentence — a missing agent is not a missing model list.
    #[test]
    fn the_unresolved_chip_names_which_half_is_missing() {
        assert_eq!(unresolved_chip_label(false), "Agent unavailable");
        assert_eq!(unresolved_chip_label(true), "Model unavailable");
    }

    /// Pinned by source scan because this crate has no entity harness to render
    /// the chip in. The state is a fresh install whose harness discovery
    /// errored: `chips_pending` treats `Error` as settled, so the real chip
    /// draws with nothing resolved behind it, and it used to draw a Claude mark
    /// with no words beside it.
    #[test]
    fn the_unresolved_chip_neither_guesses_a_brand_nor_goes_blank() {
        let source = include_str!("pickers.rs");
        let production = source
            .split_once("#[cfg(test)]")
            .expect("pickers test-module boundary")
            .0;
        let block = production
            .split_once("let harness = self.effective_harness(cx);")
            .expect("the chip's fallback block")
            .1
            .split_once("// The reasoning chip is hidden")
            .expect("the end of the chip's fallback block")
            .0;

        assert!(
            block.contains("unresolved_chip_label(harness.is_some())"),
            "an empty label must be replaced, not rendered: {block}"
        );
        assert!(
            block.contains("crate::icons::DANGER_TRIANGLE"),
            "the no-harness fallback must draw the problem glyph: {block}"
        );
        assert!(
            !block.contains("CLAUDE_MARK"),
            "the fallback must not name a brand nobody picked: {block}"
        );
    }

    /// The wedge starts at twelve o'clock and sweeps clockwise. Screen y grows
    /// downward, so getting the sign wrong fills anticlockwise or starts at
    /// three o'clock — both of which still look like a plausible gauge in a
    /// screenshot, which is why this is pinned rather than eyeballed.
    #[test]
    fn gauge_fills_clockwise_from_twelve() {
        let quarter = gauge_wedge_points((10.0, 10.0), 5.0, 0.25);
        let (first, last) = (quarter[0], *quarter.last().unwrap());
        assert_eq!(first, (10.0, 10.0), "the wedge starts at the centre");
        // First rim point is due north; the sweep ends due east.
        assert!((quarter[1].0 - 10.0).abs() < 0.01 && (quarter[1].1 - 5.0).abs() < 0.01);
        assert!((last.0 - 15.0).abs() < 0.01 && (last.1 - 10.0).abs() < 0.01);

        // Half a turn ends due south, not back at the top.
        let half = *gauge_wedge_points((10.0, 10.0), 5.0, 0.5).last().unwrap();
        assert!((half.0 - 10.0).abs() < 0.01 && (half.1 - 15.0).abs() < 0.01);
    }

    /// Nothing to paint at zero, and a reading past the window paints a full
    /// disc rather than wrapping into a thin sliver that reads as nearly empty.
    #[test]
    fn gauge_paints_nothing_at_zero_and_a_full_disc_when_over() {
        assert!(gauge_wedge_points((10.0, 10.0), 5.0, 0.0).is_empty());

        let over = gauge_wedge_points((10.0, 10.0), 5.0, 3.0);
        let full = gauge_wedge_points((10.0, 10.0), 5.0, 1.0);
        assert_eq!(over.len(), full.len());
        let last = *over.last().unwrap();
        assert!(
            (last.0 - 10.0).abs() < 0.01 && (last.1 - 5.0).abs() < 0.01,
            "a full sweep closes back at twelve o'clock"
        );
    }

    #[test]
    fn model_highlight_follows_the_selected_row() {
        let ids = ["fable-5", "sonnet-5", "haiku-4-5"];
        // The reported bug: picking any row left the highlight on row 0.
        assert_eq!(model_highlight_index(&ids, Some("sonnet-5")), 1);
        assert_eq!(model_highlight_index(&ids, Some("haiku-4-5")), 2);
        // Nothing picked yet — row 0 is also the row drawn as checked.
        assert_eq!(model_highlight_index(&ids, None), 0);
        // An id this catalog doesn't carry (stale remembered pick, or a list
        // swapped under an open menu) must not leave the highlight adrift.
        assert_eq!(model_highlight_index(&ids, Some("gone")), 0);
        assert_eq!(model_highlight_index(&[], Some("fable-5")), 0);
    }

    /// The chips stay clickable while the catalog loads, so an arrow can land
    /// on a menu that has no rows yet. Recording that as navigation suppressed
    /// the re-seat for the rows that arrived a moment later: the highlight sat
    /// on row 0 beside a checked row further down, and Enter picked row 0 —
    /// `approval-required` in the permissions menu, the first model in the
    /// model menu.
    #[test]
    fn an_arrow_on_an_empty_menu_is_not_navigation() {
        // Rows loaded: the press walks, and it counts.
        assert_eq!(menu_nav(0, false, 4, 1), (1, true));
        assert_eq!(menu_nav(0, false, 4, -1), (3, true));

        // Catalog still pending: nothing walked, nothing claimed.
        assert_eq!(menu_nav(0, false, 0, 1), (0, false));
        assert_eq!(menu_nav(0, false, 0, -1), (0, false));

        // Real navigation already recorded stays recorded — only reopening the
        // menu or switching harness hands the highlight back.
        assert_eq!(menu_nav(2, true, 0, 1), (2, true));
    }

    #[test]
    fn a_slot_is_pending_until_it_answers_but_an_error_is_an_answer() {
        // Nothing requested yet, and a request in flight: skeletons.
        assert!(slot_pending::<u8>(None));
        assert!(slot_pending(Some(&Loadable::<u8>::Idle)));
        assert!(slot_pending(Some(&Loadable::<u8>::Loading)));
        // Ready is obviously settled…
        assert!(!slot_pending(Some(&Loadable::Ready(7u8))));
        // …and so is Error. A skeleton that outlives its request is a wait
        // with no end; the chips come back so the menu's Retry is reachable.
        assert!(!slot_pending(Some(&Loadable::<u8>::Error("boom".into()))));
    }

    /// `slot_pending` settling `Error` is not enough on its own: the chips ask
    /// it twice and OR the answers, and the second slot is reached *through*
    /// the harness catalog. An errored catalog therefore leaves no effective
    /// harness and no model slot, and a missing slot reads as pending — so the
    /// skeletons outlived a request that had already failed, with the menu's
    /// Retry row unreachable behind them.
    #[test]
    fn an_errored_harness_catalog_brings_the_chips_back_not_endless_skeletons() {
        let errored = Loadable::<u8>::Error("no harnesses".into());
        // No harness to load models for, so the absent model slot is the end
        // of the road rather than a wait.
        assert!(
            !chips_pending(&errored, false, None::<&Loadable<u8>>),
            "an errored catalog is settled — the chips come back so Retry is reachable"
        );
        // Same shape when the catalog is Ready but offers nothing visible.
        assert!(!chips_pending(
            &Loadable::Ready(0u8),
            false,
            None::<&Loadable<u8>>
        ));

        // Still skeletons while the harness catalog is genuinely in flight…
        assert!(chips_pending(
            &Loadable::<u8>::Loading,
            false,
            None::<&Loadable<u8>>
        ));
        // …and when a harness IS known but its models were never requested,
        // which is the absence that really does mean "on its way".
        assert!(chips_pending(
            &Loadable::Ready(0u8),
            true,
            None::<&Loadable<u8>>
        ));
        assert!(chips_pending(
            &Loadable::Ready(0u8),
            true,
            Some(&Loadable::<u8>::Loading)
        ));

        // Both settled: real chips.
        assert!(!chips_pending(
            &Loadable::Ready(0u8),
            true,
            Some(&Loadable::Ready(1u8))
        ));
        // A model catalog that errored is settled too, same rule.
        assert!(!chips_pending(
            &Loadable::Ready(0u8),
            true,
            Some(&Loadable::<u8>::Error("boom".into()))
        ));
    }

    #[test]
    fn traits_summary_formats_non_defaults() {
        let model = Model {
            id: "opus".into(),
            label: "Opus".into(),
            description: None,
            reasoning_levels: vec![ReasoningLevel::Medium, ReasoningLevel::High],
            options: vec![
                ModelOption {
                    id: "context".into(),
                    label: "Context window".into(),
                    choices: vec![
                        ModelOptionChoice {
                            id: "standard".into(),
                            label: "Standard".into(),
                        },
                        ModelOptionChoice {
                            id: "1m".into(),
                            label: "1M".into(),
                        },
                    ],
                    default_choice: "standard".into(),
                },
                ModelOption {
                    id: "speed".into(),
                    label: "Speed".into(),
                    choices: vec![
                        ModelOptionChoice {
                            id: "normal".into(),
                            label: "Normal".into(),
                        },
                        ModelOptionChoice {
                            id: "fast".into(),
                            label: "Fast".into(),
                        },
                    ],
                    default_choice: "normal".into(),
                },
            ],
            accepts_images: true,
        };
        let mut selections = serde_json::Map::new();
        selections.insert("context".into(), serde_json::Value::String("1m".into()));
        selections.insert("speed".into(), serde_json::Value::String("fast".into()));
        let default_mode = RuntimeMode::default();
        assert_eq!(
            traits_summary(
                Some(&model),
                Some(ReasoningLevel::High),
                &selections,
                default_mode
            ),
            Some("High · 1M · Fast".to_string())
        );
        // All defaults → no summary.
        assert_eq!(
            traits_summary(Some(&model), None, &serde_json::Map::new(), default_mode),
            None
        );
        // Default-choice selections don't count as non-default.
        let mut defaults = serde_json::Map::new();
        defaults.insert("speed".into(), serde_json::Value::String("normal".into()));
        assert_eq!(
            traits_summary(Some(&model), None, &defaults, default_mode),
            None
        );
        // Reasoning shows without a model too.
        assert_eq!(
            traits_summary(
                None,
                Some(ReasoningLevel::Ultrathink),
                &serde_json::Map::new(),
                default_mode
            ),
            Some("Ultrathink".to_string())
        );
        // A non-default mode leads the summary, and the default one is silent:
        // the chip is for what the user changed.
        assert_eq!(
            traits_summary(
                Some(&model),
                Some(ReasoningLevel::High),
                &serde_json::Map::new(),
                RuntimeMode::FullAccess
            ),
            Some("Full access · High".to_string())
        );
        assert_eq!(
            traits_summary(
                None,
                None,
                &serde_json::Map::new(),
                RuntimeMode::ApprovalRequired
            ),
            Some("Ask first".to_string())
        );
    }

    /// Every mode a provider can declare needs both strings — a chip with no
    /// label is unpickable, and a mode with no caption ships the one surface
    /// that explains what it does with a blank line.
    #[test]
    fn every_mode_has_a_label_and_a_caption() {
        for mode in [
            RuntimeMode::ApprovalRequired,
            RuntimeMode::AutoAcceptEdits,
            RuntimeMode::Auto,
            RuntimeMode::FullAccess,
        ] {
            assert!(!runtime_mode_label(mode).is_empty());
            assert!(!runtime_mode_caption(mode).is_empty());
        }
    }

    #[test]
    fn folder_paths_and_breadcrumbs() {
        assert_eq!(parent_path("/home/w/dev"), Some("/home/w".to_string()));
        assert_eq!(parent_path("/home"), Some("/".to_string()));
        assert_eq!(parent_path("/home/"), Some("/".to_string()));
        assert_eq!(parent_path("/"), None);
        assert_eq!(parent_path(""), None);
        assert_eq!(child_path("/home", "w"), "/home/w");
        assert_eq!(child_path("/", "home"), "/home");
        let crumbs = breadcrumbs("/home/w/dev");
        let labels: Vec<&str> = crumbs.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(labels, ["/", "home", "w", "dev"]);
        assert_eq!(crumbs[2].1, "/home/w");
        assert_eq!(breadcrumbs("/").len(), 1);
    }

    #[test]
    fn browser_navigation_reducer() {
        let listing = FolderListing {
            path: "/home/w".into(),
            entries: vec![
                FolderEntry {
                    name: "notes.txt".into(),
                    is_dir: false,
                    is_repo: false,
                },
                FolderEntry {
                    name: "dev".into(),
                    is_dir: true,
                    is_repo: false,
                },
                FolderEntry {
                    name: "comet".into(),
                    is_dir: true,
                    is_repo: true,
                },
            ],
            truncated: false,
        };
        // Files never show as rows.
        assert_eq!(browser_rows(&listing).len(), 2);
        assert_eq!(browser_rows(&listing)[1].name, "comet");
    }

    #[test]
    fn resolved_chat_config_requires_harness() {
        let mut resolved = ResolvedRunConfig::default();
        assert!(resolved.chat_config().is_none());
        resolved.harness = Some(HarnessId::ClaudeCode);
        resolved.model = Some("opus".into());
        resolved.reasoning = Some(ReasoningLevel::High);
        let config = resolved.chat_config().expect("harness set");
        assert_eq!(config.harness, HarnessId::ClaudeCode);
        assert_eq!(config.model.as_deref(), Some("opus"));
        assert_eq!(config.sandbox, SandboxLevel::WorkspaceWrite);
    }

    #[test]
    fn chat_config_carries_the_resolved_runtime_mode() {
        let resolved = ResolvedRunConfig {
            harness: Some(HarnessId::ClaudeCode),
            model: Some("claude-fable-5".into()),
            reasoning: None,
            model_options: serde_json::Map::new(),
            runtime_mode: RuntimeMode::ApprovalRequired,
        };
        let config = resolved.chat_config().expect("harness is set");
        assert_eq!(config.runtime_mode, RuntimeMode::ApprovalRequired);
        // The row's sandbox follows the mode, so a later reader of the row
        // cannot see the two disagree.
        assert_eq!(config.sandbox, SandboxLevel::ReadOnly);
    }

    #[test]
    fn apply_owned_fields_preserves_row_mode_and_rederives_sandbox() {
        // The row already has a non-default mode persisted.
        let existing = ChatConfig {
            harness: HarnessId::ClaudeCode,
            model: Some("opus".into()),
            reasoning: Some(ReasoningLevel::High),
            model_options: serde_json::Map::new(),
            sandbox: SandboxLevel::ReadOnly,
            runtime_mode: RuntimeMode::ApprovalRequired,
        };
        // `resolved.chat_config()` built this from the draft's default mode
        // (e.g. the row's config was read before the change landed), so it
        // disagrees with the row before the picker's own logic runs.
        let mut config = ChatConfig {
            runtime_mode: RuntimeMode::AutoAcceptEdits,
            sandbox: RuntimeMode::AutoAcceptEdits.sandbox(),
            ..existing.clone()
        };

        // The picker only changes the model — it never touches the mode.
        apply_owned_fields(&mut config, Some(&existing), |c| {
            c.model = Some("sonnet".into());
        });

        assert_eq!(config.model.as_deref(), Some("sonnet"));
        assert_eq!(
            config.runtime_mode,
            RuntimeMode::ApprovalRequired,
            "the row's mode must survive an unrelated field change"
        );
        assert_eq!(
            config.sandbox,
            config.runtime_mode.sandbox(),
            "sandbox must never disagree with the mode it was derived from"
        );
    }

    /// The picker's own change: the mode moves and the sandbox follows it in
    /// the same write. These are the two fields 1.1's one real bug came from,
    /// and a row that stores a permissive sandbox beside a restrictive mode
    /// would be read by whichever consumer looked at the wrong one.
    #[test]
    fn changing_the_mode_re_derives_the_sandbox() {
        let existing = ChatConfig {
            harness: HarnessId::ClaudeCode,
            model: Some("opus".into()),
            reasoning: None,
            model_options: serde_json::Map::new(),
            sandbox: SandboxLevel::WorkspaceWrite,
            runtime_mode: RuntimeMode::AutoAcceptEdits,
        };
        let mut config = existing.clone();
        apply_owned_fields(&mut config, Some(&existing), |c| {
            c.runtime_mode = RuntimeMode::ApprovalRequired;
        });
        assert_eq!(config.runtime_mode, RuntimeMode::ApprovalRequired);
        assert_eq!(config.sandbox, SandboxLevel::ReadOnly);

        // And in the loosening direction, where getting it wrong would leave a
        // read-only sandbox on a mode the user picked to remove it.
        let mut config = existing.clone();
        apply_owned_fields(&mut config, Some(&existing), |c| {
            c.runtime_mode = RuntimeMode::FullAccess;
        });
        assert_eq!(config.sandbox, RuntimeMode::FullAccess.sandbox());
    }

    #[test]
    fn resolved_run_config_defaults_the_runtime_mode() {
        assert_eq!(
            ResolvedRunConfig::default().runtime_mode,
            RuntimeMode::AutoAcceptEdits
        );
    }

    /// The modality gate's rule. Phase 2's whole point is that a live catalog
    /// can now say `false` here — `gpt-5.3-codex-spark` reports
    /// `inputModalities: ["text"]` against a curated `accepts_images: true`
    /// (2.3's capture), and that row is the first one this gate ever closes on.
    #[test]
    fn images_are_allowed_unless_a_model_says_otherwise() {
        let mut model = Model {
            id: "spark".into(),
            label: "Spark".into(),
            description: None,
            reasoning_levels: vec![],
            options: vec![],
            accepts_images: true,
        };
        assert!(images_allowed(Some(&model)));
        model.accepts_images = false;
        assert!(!images_allowed(Some(&model)), "an explicit false closes it");
        assert!(
            images_allowed(None),
            "no model resolved yet must not disable the paperclip at boot"
        );
    }

    #[test]
    fn default_model_is_first_catalog_row() {
        let models = vec![
            Model {
                id: "flagship".into(),
                label: "Flagship".into(),
                description: None,
                reasoning_levels: vec![],
                options: vec![],
                accepts_images: true,
            },
            Model {
                id: "fast".into(),
                label: "Fast".into(),
                description: None,
                reasoning_levels: vec![],
                options: vec![],
                accepts_images: true,
            },
        ];
        assert_eq!(default_model(&models).map(|m| &*m.id), Some("flagship"));
        assert!(default_model(&[]).is_none());
    }

    #[test]
    fn default_reasoning_prefers_high_then_medium() {
        use ReasoningLevel::*;
        // Recommended default is High (user-corrected), even on full ladders.
        assert_eq!(
            default_reasoning(&[Low, Medium, High, XHigh, Max, Ultracode, Ultrathink]),
            Some(High)
        );
        assert_eq!(default_reasoning(&[Low, Medium, High, Max]), Some(High));
        // No High: Medium.
        assert_eq!(default_reasoning(&[Minimal, Low, Medium]), Some(Medium));
        // Neither offered: first entry.
        assert_eq!(default_reasoning(&[Minimal, Low]), Some(Minimal));
        // Ladder-less model (Haiku): no reasoning at all.
        assert_eq!(default_reasoning(&[]), None);
    }

    #[test]
    fn clamp_reasoning_keeps_offered_levels_and_heals_foreign_ones() {
        use ReasoningLevel::*;
        let ladder = [Low, Medium, High, Max];
        // A pick the ladder offers survives.
        assert_eq!(clamp_reasoning(Some(Max), &ladder), Some(Max));
        // A remembered level the new model doesn't offer heals to its default.
        assert_eq!(clamp_reasoning(Some(XHigh), &ladder), Some(High));
        // No pick at all resolves to the concrete default too.
        assert_eq!(clamp_reasoning(None, &ladder), Some(High));
        assert_eq!(clamp_reasoning(Some(High), &[]), None);
    }

    /// The catalog the UI caches on its first render is taken before the
    /// engine's background probes land, so the picker must ask again — but
    /// only until every entry has an answer, otherwise every open refetches
    /// for the life of the session.
    #[test]
    fn a_catalog_awaits_probes_only_while_an_entry_is_unknown() {
        use comet_proto::HarnessAvailability;
        let with = |id: HarnessId, availability: HarnessAvailability| HarnessDescriptor {
            id,
            name: "n".into(),
            capabilities: comet_proto::HarnessCapabilities::default(),
            availability,
            install: None,
            update: None,
        };

        // The boot snapshot: nothing probed yet.
        let fresh_boot = vec![
            with(HarnessId::ClaudeCode, HarnessAvailability::Unknown),
            with(HarnessId::Codex, HarnessAvailability::Unknown),
        ];
        assert!(catalog_awaits_probes(&fresh_boot));

        // Partially probed still warrants another ask.
        let partial = vec![
            with(
                HarnessId::ClaudeCode,
                HarnessAvailability::Available { version: None },
            ),
            with(HarnessId::Codex, HarnessAvailability::Unknown),
        ];
        assert!(catalog_awaits_probes(&partial));

        // Fully probed — including a failure, which IS an answer — settles.
        let settled = vec![
            with(
                HarnessId::ClaudeCode,
                HarnessAvailability::Available {
                    version: Some("2.1.224".into()),
                },
            ),
            with(
                HarnessId::Codex,
                HarnessAvailability::unavailable(
                    "Not installed",
                    Some("Install codex, or set CODEX_EXECUTABLE to its path.".into()),
                ),
            ),
        ];
        assert!(
            !catalog_awaits_probes(&settled),
            "a settled catalog must stop triggering refetches"
        );
        assert!(!catalog_awaits_probes(&[]));
    }

    /// The poll must keep running until an answer exists, and must treat an
    /// in-flight first fetch as unsettled. Both windows Greptile flagged end
    /// with an all-`Unknown` catalog and nothing pending to correct it, so a
    /// slot state wrongly counted as settled leaves a broken provider
    /// selectable until the picker is closed and reopened.
    #[test]
    fn only_a_fully_probed_ready_catalog_counts_as_settled() {
        use comet_proto::HarnessAvailability;
        let with = |availability: HarnessAvailability| HarnessDescriptor {
            id: HarnessId::Codex,
            name: "n".into(),
            capabilities: comet_proto::HarnessCapabilities::default(),
            availability,
            install: None,
            update: None,
        };

        // Nothing fetched yet, and a fetch in flight: both unsettled, so the
        // poll arms rather than concluding there is nothing to wait for.
        assert!(!harness_catalog_settled(&Loadable::Idle));
        assert!(!harness_catalog_settled(&Loadable::Loading));
        // An error slot has no rows to correct; the Retry row owns that path.
        assert!(!harness_catalog_settled(&Loadable::Error("boom".into())));

        assert!(!harness_catalog_settled(&Loadable::Ready(vec![with(
            HarnessAvailability::Unknown
        )])));
        assert!(harness_catalog_settled(&Loadable::Ready(vec![
            with(HarnessAvailability::Available { version: None }),
            with(HarnessAvailability::unavailable("Not installed", None)),
        ])));
        // An empty catalog is vacuously settled — there is nothing to probe.
        assert!(harness_catalog_settled(&Loadable::Ready(vec![])));
    }

    /// Only a landed `Unavailable` blocks a pick. This predicate gates both the
    /// greyed-out rendering and `pick_harness`, so a false positive silently
    /// makes a working provider unselectable.
    #[test]
    fn only_a_probed_failure_marks_a_harness_unavailable() {
        use comet_proto::HarnessAvailability;
        let with = |id: HarnessId, availability: HarnessAvailability| HarnessDescriptor {
            id,
            name: "n".into(),
            capabilities: comet_proto::HarnessCapabilities::default(),
            availability,
            install: None,
            update: None,
        };
        let list = vec![
            with(HarnessId::ClaudeCode, HarnessAvailability::Unknown),
            with(
                HarnessId::Codex,
                HarnessAvailability::unavailable(
                    "Not installed",
                    Some("Install codex, or set CODEX_EXECUTABLE to its path.".into()),
                ),
            ),
            with(
                HarnessId::Mock,
                HarnessAvailability::Available {
                    version: Some("1.0.0".into()),
                },
            ),
        ];

        assert!(harness_is_unavailable(&list, HarnessId::Codex));
        // Unprobed stays pickable — the whole point of `Unknown`.
        assert!(!harness_is_unavailable(&list, HarnessId::ClaudeCode));
        assert!(!harness_is_unavailable(&list, HarnessId::Mock));
        // A harness absent from the catalog is not "unavailable" either; it is
        // simply not offered, and the rail never renders a row for it.
        assert!(!harness_is_unavailable(&list, HarnessId::Cursor));
        // An empty catalog must not block everything.
        assert!(!harness_is_unavailable(&[], HarnessId::Codex));
    }

    /// A cancel marker must never outlive the state it described. If it does,
    /// the next picker open replaces a freshly loaded slot with `Idle` — the
    /// rows vanish into a skeleton and a second identical request fires.
    #[test]
    fn re_arming_never_discards_a_slot_that_already_reloaded() {
        let mut slots: HashMap<HarnessId, Loadable<Vec<u8>>> = HashMap::new();
        let mut cancelled = std::collections::HashSet::new();

        // The slot the user actually cancelled: re-arm it.
        slots.insert(HarnessId::Codex, Loadable::Error("Stopped loading".into()));
        // Same harness family, but this one was retried and succeeded before
        // the picker reopened — its marker is stale.
        slots.insert(HarnessId::ClaudeCode, Loadable::Ready(vec![1, 2, 3]));
        // And this one is mid-flight from a retry.
        slots.insert(HarnessId::Mock, Loadable::Loading);
        cancelled.extend([HarnessId::Codex, HarnessId::ClaudeCode, HarnessId::Mock]);

        rearm_cancelled(&mut slots, &mut cancelled);

        assert_eq!(slots.get(&HarnessId::Codex), Some(&Loadable::Idle));
        assert_eq!(
            slots.get(&HarnessId::ClaudeCode),
            Some(&Loadable::Ready(vec![1, 2, 3])),
            "a reloaded slot must survive a stale marker"
        );
        assert_eq!(
            slots.get(&HarnessId::Mock),
            Some(&Loadable::Loading),
            "an in-flight reload must not be restarted"
        );
        assert!(cancelled.is_empty(), "markers are spent either way");
    }

    /// The agent catalog is the one toasted load that needs a marker at all —
    /// render calls `ensure_harnesses` every frame, so it refuses to reload an
    /// `Error` and a cancel would otherwise disable the picker for the session.
    /// Its single-slot re-arm carries the same guard as the map version.
    #[test]
    fn a_cancelled_catalog_re_arms_without_discarding_a_reload() {
        // The cancel the marker describes: put it back to `Idle`.
        let mut slot: Loadable<Vec<u8>> = Loadable::Error("Stopped loading".into());
        let mut cancelled = true;
        rearm_cancelled_slot(&mut slot, &mut cancelled);
        assert_eq!(slot, Loadable::Idle);
        assert!(!cancelled, "the marker is spent");

        // A stale marker — the Retry row reloaded the catalog without going
        // through the cancel path — must not blank the rows it already has.
        let mut reloaded = Loadable::Ready(vec![1, 2, 3]);
        let mut stale = true;
        rearm_cancelled_slot(&mut reloaded, &mut stale);
        assert_eq!(reloaded, Loadable::Ready(vec![1, 2, 3]));
        assert!(!stale);

        // And an ordinary failure re-arms only on Retry, never on a reopen:
        // without a marker the `Error` has to stand.
        let mut failed: Loadable<Vec<u8>> = Loadable::Error("Couldn't load".into());
        let mut none = false;
        rearm_cancelled_slot(&mut failed, &mut none);
        assert!(matches!(failed, Loadable::Error(_)));
    }

    /// The two inert states must stay tellable apart. They previously shared a
    /// single `is_disabled` flag and painted identically, so a missing CLI and
    /// a session already committed to another agent looked the same and only
    /// one of them ever said why.
    #[test]
    fn a_locked_row_and_an_unavailable_row_are_different_states() {
        // Locked out: another agent owns the session. Transient, no action.
        assert_eq!(
            RailRowState::of(true, false, false),
            RailRowState::LockedOut
        );
        // The viewed row is never locked out — the lock is what makes it the
        // viewed one.
        assert_eq!(RailRowState::of(true, true, false), RailRowState::Pickable);
        // Unavailable outranks the lock, and unlike the lock it applies to the
        // viewed row: a committed agent whose CLI vanished is exactly the case
        // worth surfacing.
        assert_eq!(
            RailRowState::of(true, true, true),
            RailRowState::Unavailable
        );
        assert_eq!(
            RailRowState::of(true, false, true),
            RailRowState::Unavailable
        );
        assert_eq!(
            RailRowState::of(false, false, true),
            RailRowState::Unavailable
        );
        // Nothing wrong, nothing committed.
        assert_eq!(
            RailRowState::of(false, false, false),
            RailRowState::Pickable
        );
        // Both inert states block the pick, which is what `pick_harness` and
        // the hover wash key off.
        assert!(RailRowState::LockedOut.is_disabled());
        assert!(RailRowState::Unavailable.is_disabled());
        assert!(!RailRowState::Pickable.is_disabled());
    }

    #[test]
    fn mock_harness_hidden_unless_alone() {
        // Visibility keys off the id alone, so the capability block is inert here.
        let descriptor = |id: HarnessId, name: &str| HarnessDescriptor {
            id,
            name: name.into(),
            capabilities: comet_proto::HarnessCapabilities::default(),
            availability: comet_proto::HarnessAvailability::Unknown,
            install: None,
            update: None,
        };
        let mixed = vec![
            descriptor(HarnessId::Mock, "Mock"),
            descriptor(HarnessId::ClaudeCode, "Claude Code"),
        ];
        // Env-independent core: mock hidden in production…
        let visible = visible_harnesses_impl(&mixed, false);
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].id, HarnessId::ClaudeCode);
        let only_mock = vec![descriptor(HarnessId::Mock, "Mock")];
        assert_eq!(visible_harnesses_impl(&only_mock, false).len(), 1);
        // …and opted back in by COMET_HARNESS=mock (the e2e rig).
        assert_eq!(visible_harnesses_impl(&mixed, true).len(), 2);
        assert_eq!(visible_harnesses_impl(&mixed, true)[0].id, HarnessId::Mock);
    }

    /// The caption exists so a user looking at a stale list can tell. It is
    /// copy, not a raw error: no provider name from an error string, no
    /// `err.to_string()`, nothing the user cannot act on.
    #[test]
    fn built_in_caption_names_the_agent_and_nothing_technical() {
        let caption = built_in_caption(HarnessId::ClaudeCode);
        assert!(caption.contains("Built-in list"), "got {caption}");
        assert!(!caption.contains("Error"), "no error vocabulary: {caption}");
        assert!(
            !caption.contains("harness"),
            "internal word on screen: {caption}"
        );
    }

    /// A live answer says nothing at all — the caption is for the degraded case.
    #[test]
    fn a_live_catalog_shows_no_caption() {
        assert!(caption_for(CatalogSource::Live, HarnessId::ClaudeCode).is_none());
        assert!(caption_for(CatalogSource::BuiltIn, HarnessId::ClaudeCode).is_some());
    }

    /// The decode is pinned against the reply's REAL JSON, not against a
    /// `ModelCatalog` this test built and re-serialized. Task 4 changed the
    /// wire shape and every one of `comet-ui`'s 501 tests stayed green while
    /// the picker was left decoding the old one — a round-trip through the
    /// Rust type would have stayed green too. Only the literal the engine
    /// actually sends catches that.
    #[test]
    fn the_picker_decodes_the_reply_the_engine_actually_sends() {
        let reply = serde_json::json!({
            "models": [{
                "id": "claude-sonnet-5",
                "label": "Sonnet 5",
                "reasoningLevels": [],
                "options": []
            }],
            "source": "builtIn"
        });
        let catalog = decode_models_reply(reply).expect("decode");
        assert_eq!(catalog.source, CatalogSource::BuiltIn);
        assert_eq!(catalog.models.len(), 1);
        assert_eq!(catalog.models[0].id, "claude-sonnet-5");
        assert!(
            catalog.models[0].accepts_images,
            "absent acceptsImages means images work"
        );
    }
}
