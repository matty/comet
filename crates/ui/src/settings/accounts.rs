//! Settings → Agents / accounts (feature-inventory §1.9): provider cards
//! (Claude Code, Codex) with account rows — email, plan badge, Active, usage
//! meters (indigo → amber ≥80% → red ≥95%, reset time), Switch / Forget — plus
//! the add-account dialogs (paste-code and browser-poll flows) and
//! account-shaped loading skeletons.
//!
//! The accounts RPC surface is being implemented engine-side in parallel —
//! every call here surfaces failures as inline UI states rather than assuming
//! the methods exist. One deliberate exception: the harness-diagnostics call
//! (see the "Not understood" block below) is supplementary to the accounts
//! result the pane already owns, so any failure — decode error, RPC error,
//! or an older engine replying `UnknownMethod` because it predates this
//! surface — hides the block and logs instead of erroring the pane.

use chrono::{DateTime, Utc};
use gpui::{
    AnyElement, Context, Entity, Hsla, SharedString, Subscription, Task, Window, div, prelude::*,
    px,
};
use std::time::Duration;

use comet_engine::registry::{HarnessDescriptor, HarnessDiagnostics};
use comet_proto::{
    AgentAccount, AgentAccountsSnapshot, AgentLoginMode, AgentLoginPoll, AgentLoginStart,
    AgentLoginStatus, HarnessAvailability, HarnessId,
};
use comet_rpc::methods;

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::errors;
use crate::popover::{self, Loadable};
use crate::state::AppState;
use crate::theme::Theme;

// ---------------------------------------------------------------------------
// Pure: usage meters + labels
// ---------------------------------------------------------------------------

pub const USAGE_WARN_FRACTION: f32 = 0.80;
pub const USAGE_CRITICAL_FRACTION: f32 = 0.95;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageLevel {
    /// < 80% — indigo.
    Normal,
    /// ≥ 80% — amber.
    Warn,
    /// ≥ 95% — red.
    Critical,
}

/// The two lines under a provider's name: what version answered and how it got
/// there, then which binary that was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallLine {
    /// `"2.1.228 · Native installer"`, or just the method when the CLI answered
    /// without a readable version.
    pub summary: String,
    /// The resolved executable, verbatim.
    pub path: String,
}

/// The install line for one provider, or `None` when there is nothing honest to
/// say.
///
/// Pure and split out of the entity for the reason `pickers::images_allowed`
/// is: this crate has no gpui test context, so anything reachable only through
/// `App` is verified by the rendered check and nothing else.
///
/// Three absent cases, all deliberate. No descriptor means the catalog has not
/// landed or the engine predates the fields; no `install` means the CLI never
/// resolved, which the card's own empty state already explains; and a resolved
/// CLI with no readable version still earns a line, because **the method and
/// path are the diagnostic half**. A broken install is exactly when knowing
/// which binary was asked matters most.
pub fn install_line(descriptor: Option<&HarnessDescriptor>) -> Option<InstallLine> {
    let descriptor = descriptor?;
    let install = descriptor.install.as_ref()?;
    let version = match &descriptor.availability {
        HarnessAvailability::Available { version } => version.as_deref(),
        // An unprobed or failed CLI has no version to quote. Saying nothing
        // beats quoting a stale one from a previous probe.
        HarnessAvailability::Unknown | HarnessAvailability::Unavailable { .. } => None,
    };
    let method = install.method.label();
    Some(InstallLine {
        summary: match version {
            Some(version) => format!("{version} \u{00b7} {method}"),
            None => method.to_string(),
        },
        path: install.path.clone(),
    })
}

/// Threshold classification of a usage fraction. Pure.
pub fn usage_level(fraction: f32) -> UsageLevel {
    if fraction >= USAGE_CRITICAL_FRACTION {
        UsageLevel::Critical
    } else if fraction >= USAGE_WARN_FRACTION {
        UsageLevel::Warn
    } else {
        UsageLevel::Normal
    }
}

pub fn usage_color(level: UsageLevel, theme: &Theme) -> Hsla {
    match level {
        UsageLevel::Normal => theme.accent,
        UsageLevel::Warn => theme.warning,
        UsageLevel::Critical => theme.danger,
    }
}

/// Why a `ListAgentAccounts` load is happening. Pure input to
/// [`force_usage_for`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadTrigger {
    /// Page construction — the visit's first list.
    Mount,
    /// "Click to retry" after a failed load — still the visit's first
    /// successful list.
    Retry,
    /// The explicit Refresh button.
    Refresh,
    /// After a completed add-account login flow.
    PostLogin,
    /// After Switch/Forget succeeds.
    PostAction,
}

/// Whether a load should ask the engine to probe usage (`forceUsage`). The
/// engine only hits the provider when forced; non-forced lists serve the 60s
/// usage cache or nothing (engine/src/agent_accounts.rs module docs — the
/// design expects the UI to force "on page mount/refresh"). The visit's first
/// list (mount, or retry after a failure) must force, or every first open
/// renders "Usage unavailable" until a manual Refresh — the old app fetched
/// usage on every list. Post-Switch/Forget lists ride the still-warm cache.
pub fn force_usage_for(trigger: LoadTrigger) -> bool {
    match trigger {
        LoadTrigger::Mount | LoadTrigger::Retry | LoadTrigger::Refresh | LoadTrigger::PostLogin => {
            true
        }
        LoadTrigger::PostAction => false,
    }
}

/// Compact absolute reset moment (comet settings.agents.tsx `formatReset`):
/// a local clock time ("3:45 PM") when it lands within ~22h, else a short
/// weekday ("Mon"); the caller prefixes "resets ". Pure given `now`.
pub fn format_reset(resets_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> Option<String> {
    use chrono::Local;
    let at = resets_at?;
    let local = at.with_timezone(&Local);
    Some(if at.signed_duration_since(now).num_hours() < 22 {
        format!("resets {}", local.format("%-I:%M %p"))
    } else {
        format!("resets {}", local.format("%a"))
    })
}

/// The provider cards, in display order: (harness, name, CLI command — named
/// in the empty-state copy, comet settings.agents.tsx `PROVIDERS`).
pub const PROVIDERS: [(HarnessId, &str, &str); 2] = [
    (HarnessId::ClaudeCode, "Claude Code", "claude"),
    (HarnessId::Codex, "Codex", "codex"),
];

/// Accounts of one provider, active first (stable otherwise). Pure.
pub fn provider_accounts(
    snapshot: &AgentAccountsSnapshot,
    harness: HarnessId,
) -> Vec<&AgentAccount> {
    let mut accounts: Vec<&AgentAccount> = snapshot
        .accounts
        .iter()
        .filter(|a| a.harness == harness)
        .collect();
    accounts.sort_by_key(|a| !a.active);
    accounts
}

/// "message" or "messages" for `n`. Shared by the rollup sentence and the
/// overflow line so the two cannot drift into disagreeing grammar — the
/// overflow line reaches exactly 1 on the first arrival of a 65th distinct
/// discriminator, so the singular is reachable, not theoretical.
fn message_noun(n: u64) -> &'static str {
    if n == 1 { "message" } else { "messages" }
}

/// The per-provider "Not understood" rollup: total count and the sentence.
/// `None` when the provider has nothing recorded — the block is HIDDEN at
/// zero, which the harness-side Ignored tier keeps as the normal state.
/// `name` is the Agent's display name (never a harness id).
pub fn diagnostics_rollup(
    list: &[HarnessDiagnostics],
    harness: HarnessId,
    name: &str,
) -> Option<(u64, String)> {
    let report = list.iter().find(|d| d.harness == harness)?;
    let total: u64 = report
        .entries
        .iter()
        .map(|e| e.count)
        .fold(report.overflow, u64::saturating_add);
    if total == 0 {
        return None;
    }
    let noun = message_noun(total);
    Some((
        total,
        format!("{name} sent {total} {noun} this session that Comet doesn't recognize."),
    ))
}

/// Compact relative age for the diagnostics rows. Pure given `now_ms`; a
/// clock that ran backwards degrades to "just now".
pub fn format_ago(last_seen_ms: i64, now_ms: i64) -> String {
    let secs = (now_ms - last_seen_ms).max(0) / 1000;
    if secs < 10 {
        "just now".into()
    } else if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else {
        format!("{}h ago", secs / 3600)
    }
}

// ---------------------------------------------------------------------------
// Entity
// ---------------------------------------------------------------------------

enum LoginFlow {
    /// StartAgentLogin in flight.
    Starting { harness: HarnessId },
    /// Claude-style: open the URL, paste the code back.
    PasteCode {
        harness: HarnessId,
        start: AgentLoginStart,
        submitting: bool,
        error: Option<SharedString>,
    },
    /// Codex-style: open the URL, poll until the browser flow lands.
    Browser {
        harness: HarnessId,
        start: AgentLoginStart,
        message: Option<SharedString>,
        error: Option<SharedString>,
    },
}

impl LoginFlow {
    /// Dialog title (comet: "Add Claude account" / "Add Codex account").
    fn title(&self) -> &'static str {
        let harness = match self {
            LoginFlow::Starting { harness }
            | LoginFlow::PasteCode { harness, .. }
            | LoginFlow::Browser { harness, .. } => *harness,
        };
        match harness {
            HarnessId::Codex => "Add Codex account",
            _ => "Add Claude account",
        }
    }
}

pub struct AccountsPage {
    state: Entity<AppState>,
    /// Which device's logins are shown; `None` = this device (no passthrough).
    /// Retargeted by the page-header device switcher (comet parity: the
    /// account RPCs are server-qualified; CLI logins are per Comet instance).
    selected_device: Option<String>,
    device_menu_open: bool,
    /// Outside-click dismissal instant — suppresses the trigger click that
    /// follows the same mouse-down from instantly reopening the menu.
    device_menu_dismissed_at: Option<std::time::Instant>,
    snapshot: Loadable<AgentAccountsSnapshot>,
    /// Per-boot unrecognized-message counts, fetched alongside the accounts
    /// list. Empty = zero everywhere OR the fetch failed/was cancelled — the
    /// block is supplementary and hides either way (an older engine without
    /// the method simply lacks the feature).
    diagnostics: Vec<HarnessDiagnostics>,
    /// The agent catalog, fetched alongside the accounts list purely for the
    /// installed-version line under each provider name. Supplementary in the
    /// same way `diagnostics` is: empty means the fetch failed or the engine
    /// predates the fields, and the line is simply absent.
    harnesses: Vec<HarnessDescriptor>,
    /// Account id with an in-flight Switch/Forget.
    busy_account: Option<String>,
    login: Option<LoginFlow>,
    error: Option<SharedString>,
    code_input: Entity<ComposerInput>,
    load_task: Option<Task<()>>,
    harness_poll_task: Option<Task<()>>,
    action_task: Option<Task<()>>,
    poll_task: Option<Task<()>>,
    _observe: Subscription,
    _code_events: Subscription,
}

impl AccountsPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&state, |_, _, cx| cx.notify());
        let code_input = cx.new(|cx| ComposerInput::new("Paste the authorization code", cx));
        let code_events = cx.subscribe(&code_input, |this: &mut Self, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.submit_code(cx);
            }
        });
        let mut page = Self {
            state,
            selected_device: None,
            device_menu_open: false,
            device_menu_dismissed_at: None,
            snapshot: Loadable::Idle,
            diagnostics: Vec::new(),
            harnesses: Vec::new(),
            busy_account: None,
            login: None,
            error: None,
            code_input,
            load_task: None,
            harness_poll_task: None,
            action_task: None,
            poll_task: None,
            _observe: observe,
            _code_events: code_events,
        };
        // Force the usage probe on the visit's first list — a plain list
        // returns no usage windows on a cold engine cache, which rendered
        // every account as "Usage unavailable" until a manual Refresh. The
        // Loading skeleton (meter ghosts) covers the probe latency, so
        // "Usage unavailable" is reserved for a probe that genuinely failed.
        page.load(force_usage_for(LoadTrigger::Mount), cx);
        page
    }

    /// Retarget the page at another device's logins: every accounts RPC is
    /// server-qualified, so the whole page — list, usage probes, switch,
    /// forget, login flows — follows the passthrough.
    fn set_selected_device(&mut self, target: Option<String>, cx: &mut Context<Self>) {
        self.device_menu_open = false;
        if self.selected_device == target {
            cx.notify();
            return;
        }
        self.selected_device = target;
        // A different device = a different accounts world: drop in-flight
        // login/action state and reload with a forced usage probe (the new
        // device's cache is cold).
        self.login = None;
        self.busy_account = None;
        self.error = None;
        self.load(force_usage_for(LoadTrigger::Mount), cx);
    }

    fn params(&self, value: serde_json::Value) -> serde_json::Value {
        value
    }

    /// The page-header device switcher (comet device-switcher.tsx): a quiet
    /// trigger — platform glyph · name · presence dot · sort glyph — opening a
    /// dropdown of every registered device. Selecting one retargets the page.
    fn render_device_switcher(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        use crate::icons::{self, icon};
        let (mut devices, local_id) = {
            let s = self.state.read(cx);
            (s.devices.clone(), s.local_device_id.clone())
        };
        // Stable row order (registration time, then id) — comet's switcher
        // sorts the same way so rows never reshuffle on heartbeats.
        devices.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        let effective = self.selected_device.clone().or_else(|| local_id.clone());
        let selected = devices
            .iter()
            .find(|d| Some(d.id.as_str()) == effective.as_deref())
            .cloned();
        let platform_glyph = |platform: &str| match platform {
            "macos" | "darwin" => icons::LAPTOP,
            "ios" | "android" => icons::SMARTPHONE,
            _ => icons::MONITOR,
        };
        let trigger_glyph = platform_glyph(
            selected
                .as_ref()
                .map(|d| d.platform.as_str())
                .unwrap_or("macos"),
        );
        let trigger_label: SharedString = selected
            .as_ref()
            .map(|d| d.name.clone().into())
            .unwrap_or_else(|| SharedString::from("This device"));
        let emerald = theme.success;
        let open = self.device_menu_open;

        let mut trigger =
            div()
                .id("accounts-device-switcher")
                .flex_none()
                .h(px(28.0))
                .px(px(8.0))
                .rounded(px(6.0))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                .cursor_pointer()
                .bg(if open {
                    crate::theme::ink(0.06)
                } else {
                    gpui::transparent_black()
                })
                .when(!open, |el| el.hover(|s| s.bg(crate::theme::ink(0.04))))
                .on_click(cx.listener(|this, _, _, cx| {
                    let just_dismissed = this
                        .device_menu_dismissed_at
                        .is_some_and(|at| at.elapsed() < Duration::from_millis(400));
                    this.device_menu_open = !this.device_menu_open && !just_dismissed;
                    this.device_menu_dismissed_at = None;
                    cx.notify();
                }))
                .child(
                    icon(trigger_glyph)
                        .size(px(16.0))
                        .flex_none()
                        .text_color(theme.text_muted),
                )
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(px(12.5))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text)
                        .child(trigger_label),
                )
                .child(div().size(px(6.0)).rounded_full().flex_none().bg(
                    if effective == local_id {
                        emerald
                    } else {
                        crate::theme::ink(0.2)
                    },
                ))
                .child(
                    icon(icons::SORT_VERTICAL)
                        .size(px(14.0))
                        .flex_none()
                        .text_color(theme.text_muted.opacity(if open { 0.9 } else { 0.4 })),
                );

        if open {
            let menu = popover::popover_card(theme)
                .w(px(220.0))
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.device_menu_open = false;
                    this.device_menu_dismissed_at = Some(std::time::Instant::now());
                    cx.notify();
                }))
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(popover::menu_heading(theme, "Devices"))
                .children(devices.into_iter().enumerate().map(|(ix, d)| {
                    let is_active = Some(d.id.as_str()) == effective.as_deref();
                    let is_local = local_id.as_deref() == Some(d.id.as_str());
                    let glyph = platform_glyph(&d.platform);
                    let name: SharedString = d.name.clone().into();
                    let pick_local = is_local;
                    let pick_id = d.id.clone();
                    popover::menu_row(theme, is_active, format!("accounts-device-row-{ix}"))
                        .id(("accounts-device-row", ix))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            // Local device = no passthrough (calls stay direct).
                            let target = (!pick_local).then(|| pick_id.clone());
                            this.set_selected_device(target, cx);
                        }))
                        .child(
                            icon(glyph)
                                .size(px(16.0))
                                .flex_none()
                                .text_color(theme.text_muted),
                        )
                        .child(div().flex_1().min_w_0().truncate().child(name))
                        .when(is_local, |el| {
                            el.child(
                                div()
                                    .flex_none()
                                    .text_size(px(10.5))
                                    .text_color(theme.text_muted.opacity(0.35))
                                    .child(SharedString::from("You")),
                            )
                        })
                        .when(is_active, |el| el.child(popover::menu_check(theme)))
                        .child(
                            div()
                                .size(px(6.0))
                                .rounded_full()
                                .flex_none()
                                .bg(if is_local {
                                    emerald
                                } else {
                                    crate::theme::ink(0.2)
                                }),
                        )
                }))
                .into_any_element();
            trigger = trigger.child(popover::anchored_menu("accounts-device-menu", menu));
        }
        trigger.into_any_element()
    }

    fn load(&mut self, force_usage: bool, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).selected_client() else {
            self.snapshot =
                Loadable::Error("Couldn't load your accounts — Comet isn't connected.".into());
            return;
        };
        self.snapshot = Loadable::Loading;
        let params = self.params(serde_json::json!({ "forceUsage": force_usage }));
        // Every entry point here blanks the page to a skeleton first, so the
        // wait is the whole of what the user can see and Cancel is offered. No
        // re-arm marker: this reloads from any state, and Retry, Refresh, a
        // device switch and every post-action refresh all call it.
        let (request_id, cancelled) = crate::toast::begin(cx, errors::Loading::Accounts);
        self.load_task = Some(cx.spawn(async move |this, cx| {
            // Both RPCs ride the one registered request: the toast covers the
            // whole wait, and Cancel drops both futures together. `cancelled`
            // is also the registration's liveness handle, so it stays inside
            // this task — dropping the task (a newer load replaces it) drops
            // the receiver, and the registry forgets the entry rather than
            // leaving a toast for work that is no longer running.
            //
            // The diagnostics call is NOT device-qualified: its counts
            // describe the engine answering it, this boot.
            let client = engine.client();
            let accounts_call = client.call(methods::LIST_AGENT_ACCOUNTS, params);
            let diagnostics_call =
                client.call(methods::LIST_HARNESS_DIAGNOSTICS, serde_json::Value::Null);
            // Nor is the harness catalog device-qualified: it describes the CLIs
            // installed on the engine answering, which is the same engine the
            // diagnostics counts belong to.
            let harnesses_call = client.call(methods::LIST_HARNESSES, serde_json::Value::Null);
            let both = std::pin::pin!(futures::future::join3(
                accounts_call,
                diagnostics_call,
                harnesses_call
            ));
            let outcome = futures::future::select(both, cancelled).await;
            this.update(cx, |page, cx| {
                crate::toast::end(cx, request_id);
                let (result, diagnostics_result, harnesses_result) = match outcome {
                    futures::future::Either::Left((results, _)) => results,
                    futures::future::Either::Right(_) => {
                        page.snapshot = Loadable::Error(crate::toast::cancelled_message(
                            errors::Loading::Accounts,
                        ));
                        page.diagnostics = Vec::new();
                        page.harnesses = Vec::new();
                        cx.notify();
                        return;
                    }
                };
                page.snapshot = match result {
                    Ok(value) => match serde_json::from_value::<AgentAccountsSnapshot>(value) {
                        Ok(snapshot) => Loadable::Ready(snapshot),
                        Err(err) => {
                            Loadable::Error(errors::decode_failure(errors::Loading::Accounts, &err))
                        }
                    },
                    Err(err) => {
                        Loadable::Error(errors::load_failure(errors::Loading::Accounts, &err))
                    }
                };
                // Supplementary: a failure (an older engine's UnknownMethod
                // included) hides the block rather than erroring a pane the
                // accounts result already owns. Detail stays in tracing.
                page.diagnostics = match diagnostics_result {
                    Ok(value) => serde_json::from_value::<Vec<HarnessDiagnostics>>(value)
                        .unwrap_or_else(|err| {
                            tracing::warn!(
                                error = %err,
                                "harness diagnostics decode failed (block hidden)"
                            );
                            Vec::new()
                        }),
                    Err(err) => {
                        tracing::warn!(
                            error = %err,
                            "harness diagnostics load failed (block hidden)"
                        );
                        Vec::new()
                    }
                };
                // Same supplementary treatment: the version line is a nicety
                // beside the accounts this pane exists for, so a failure here
                // hides one line rather than erroring the pane.
                page.harnesses = match harnesses_result {
                    Ok(value) => {
                        crate::pickers::decode_harnesses_reply(value).unwrap_or_else(|err| {
                            tracing::warn!(
                                error = %err,
                                "harness catalog decode failed (version line hidden)"
                            );
                            Vec::new()
                        })
                    }
                    Err(err) => {
                        tracing::warn!(
                            error = %err,
                            "harness catalog load failed (version line hidden)"
                        );
                        Vec::new()
                    }
                };
                page.poll_harness_installs(cx);
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Keep asking for the catalog while provider probes are still landing.
    ///
    /// The pane fetches `ListHarnesses` exactly once, at mount, but the engine
    /// probes providers in the background *after* boot — so on a cold start the
    /// install line was simply absent for the rest of the session, and the only
    /// way to reveal it was a Refresh nobody has a reason to press. **Found by
    /// the rendered check, not by a test**: with both CLIs already warm the race
    /// is almost always won, and the run that lost it was the one where a
    /// deliberately-broken override answered instantly while the real CLI was
    /// still starting a Node runtime.
    ///
    /// `catalog_awaits_probes` is the same predicate the picker revalidates on,
    /// and it is the right one here rather than a coincidence: `install` is
    /// filled by the same probe that fills `availability`, so "unprobed" and "no
    /// path yet" are one state.
    ///
    /// Bounded exactly as `pickers::revalidate_harnesses` is — every entry
    /// answered (a probed *failure* is an answer), or
    /// [`HARNESS_REVALIDATE_ATTEMPTS`] tries. Hitting the cap degrades to no
    /// line, which is the same thing an older engine shows, never a spinner
    /// that waits forever.
    ///
    /// [`HARNESS_REVALIDATE_ATTEMPTS`]: crate::pickers::HARNESS_REVALIDATE_ATTEMPTS
    fn poll_harness_installs(&mut self, cx: &mut Context<Self>) {
        if !crate::pickers::catalog_awaits_probes(&self.harnesses) {
            return;
        }
        let Some(engine) = self.state.read(cx).selected_client() else {
            return;
        };
        self.harness_poll_task = Some(cx.spawn(async move |this, cx| {
            for _ in 0..crate::pickers::HARNESS_REVALIDATE_ATTEMPTS {
                cx.background_executor()
                    .timer(crate::pickers::HARNESS_REVALIDATE_INTERVAL)
                    .await;
                let result = engine
                    .client()
                    .call(methods::LIST_HARNESSES, serde_json::Value::Null)
                    .await;
                let stop = this
                    .update(cx, |page, cx| {
                        // No toast and no `Loading` flip: the pane is fully
                        // rendered throughout, and this only ever ADDS a line.
                        // A failed poll keeps what is on screen and tries again.
                        if let Ok(value) = result
                            && let Ok(list) = crate::pickers::decode_harnesses_reply(value)
                        {
                            page.harnesses = list;
                            cx.notify();
                        }
                        !crate::pickers::catalog_awaits_probes(&page.harnesses)
                    })
                    .unwrap_or(true);
                if stop {
                    break;
                }
            }
        }));
    }

    /// Switch / Forget an account.
    fn account_action(
        &mut self,
        method: &'static str,
        account: &AgentAccount,
        cx: &mut Context<Self>,
    ) {
        let Some(engine) = self.state.read(cx).selected_client() else {
            return;
        };
        self.busy_account = Some(account.id.clone());
        self.error = None;
        // Tolerant param shape: both `id` and `accountId` plus the harness.
        let params = self.params(serde_json::json!({
            "id": account.id,
            "accountId": account.id,
            "harness": account.harness,
        }));
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = engine.client().call(method, params).await;
            this.update(cx, |page, cx| {
                page.busy_account = None;
                match result {
                    Ok(_) => page.load(force_usage_for(LoadTrigger::PostAction), cx),
                    Err(err) => page.error = Some(format!("{err}").into()),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    // ---- add-account flows ----

    fn start_login(&mut self, harness: HarnessId, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).selected_client() else {
            return;
        };
        self.login = Some(LoginFlow::Starting { harness });
        self.error = None;
        let params = self.params(serde_json::json!({ "harness": harness }));
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::START_AGENT_LOGIN, params)
                .await;
            this.update(cx, |page, cx| {
                match result.and_then(|value| {
                    serde_json::from_value::<AgentLoginStart>(value)
                        .map_err(|e| comet_rpc::RpcError::Failed(e.to_string()))
                }) {
                    Ok(start) => {
                        cx.open_url(&start.url);
                        match start.mode {
                            AgentLoginMode::PasteCode => {
                                page.code_input
                                    .update(cx, |input, cx| input.set_text("", cx));
                                page.login = Some(LoginFlow::PasteCode {
                                    harness,
                                    start,
                                    submitting: false,
                                    error: None,
                                });
                            }
                            AgentLoginMode::Browser => {
                                page.login = Some(LoginFlow::Browser {
                                    harness,
                                    start,
                                    message: None,
                                    error: None,
                                });
                                page.spawn_poll(cx);
                            }
                        }
                    }
                    Err(err) => {
                        page.login = None;
                        page.error = Some(format!("Login failed to start: {err}").into());
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn submit_code(&mut self, cx: &mut Context<Self>) {
        let Some(LoginFlow::PasteCode {
            start, submitting, ..
        }) = &mut self.login
        else {
            return;
        };
        if *submitting {
            return;
        }
        let code = self.code_input.read(cx).text().trim().to_string();
        if code.is_empty() {
            return;
        }
        let login_id = start.login_id.clone();
        *submitting = true;
        let Some(engine) = self.state.read(cx).selected_client() else {
            return;
        };
        let params = self.params(serde_json::json!({ "loginId": login_id, "code": code }));
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::COMPLETE_AGENT_LOGIN, params)
                .await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(_) => {
                        page.login = None;
                        page.load(force_usage_for(LoadTrigger::PostLogin), cx);
                    }
                    Err(err) => {
                        if let Some(LoginFlow::PasteCode {
                            submitting, error, ..
                        }) = &mut page.login
                        {
                            *submitting = false;
                            *error = Some(format!("{err}").into());
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// The browser-wait poll loop: PollAgentLogin every 1.5s until Done/Error.
    fn spawn_poll(&mut self, cx: &mut Context<Self>) {
        let Some(LoginFlow::Browser { start, .. }) = &self.login else {
            return;
        };
        let login_id = start.login_id.clone();
        let Some(engine) = self.state.read(cx).selected_client() else {
            return;
        };
        let params = self.params(serde_json::json!({ "loginId": login_id }));
        self.poll_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(1500))
                    .await;
                let result = engine
                    .client()
                    .call(methods::POLL_AGENT_LOGIN, params.clone())
                    .await;
                let outcome = this.update(cx, |page, cx| {
                    let Some(LoginFlow::Browser { message, error, .. }) = &mut page.login else {
                        return true; // dialog dismissed — stop polling
                    };
                    match result.as_ref().ok().and_then(|value| {
                        serde_json::from_value::<AgentLoginPoll>(value.clone()).ok()
                    }) {
                        Some(poll) => match poll.status {
                            AgentLoginStatus::Done => {
                                page.login = None;
                                page.load(force_usage_for(LoadTrigger::PostLogin), cx);
                                cx.notify();
                                true
                            }
                            AgentLoginStatus::Error => {
                                *error = Some(
                                    poll.message
                                        .unwrap_or_else(|| "Login failed".to_string())
                                        .into(),
                                );
                                cx.notify();
                                true
                            }
                            AgentLoginStatus::Pending => {
                                if let Some(text) = poll.message {
                                    *message = Some(text.into());
                                }
                                cx.notify();
                                false
                            }
                        },
                        None => {
                            let text = match &result {
                                Err(err) => format!("Poll failed: {err}"),
                                Ok(_) => "Poll failed: malformed reply".to_string(),
                            };
                            *error = Some(text.into());
                            cx.notify();
                            true
                        }
                    }
                });
                match outcome {
                    Ok(true) | Err(_) => break,
                    Ok(false) => {}
                }
            }
        }));
    }

    fn cancel_login(&mut self, cx: &mut Context<Self>) {
        let login_id = match &self.login {
            Some(LoginFlow::PasteCode { start, .. }) | Some(LoginFlow::Browser { start, .. }) => {
                Some(start.login_id.clone())
            }
            _ => None,
        };
        self.login = None;
        self.poll_task = None;
        if let (Some(login_id), Some(engine)) = (login_id, self.state.read(cx).selected_client()) {
            let params = self.params(serde_json::json!({ "loginId": login_id }));
            self.action_task = Some(cx.spawn(async move |_, _| {
                if let Err(err) = engine
                    .client()
                    .call(methods::CANCEL_AGENT_LOGIN, params)
                    .await
                {
                    tracing::debug!(error = %err, "CancelAgentLogin failed (best-effort)");
                }
            }));
        }
        cx.notify();
    }

    // ---- render pieces ----

    /// One usage window (comet settings.agents.tsx `UsageMeter`): label ·
    /// 5px rounded-full bar (indigo → amber ≥80% → red ≥95%) · "NN% used" ·
    /// quiet reset time.
    fn render_usage_meter(
        &self,
        window: &comet_proto::AgentUsageWindow,
        theme: &Theme,
        now: DateTime<Utc>,
    ) -> AnyElement {
        let fraction = window.used_fraction.clamp(0.0, 1.0);
        let level = usage_level(fraction);
        let fill = usage_color(level, theme).opacity(match level {
            UsageLevel::Normal => 0.8,
            _ => 0.85,
        });
        let reset = format_reset(window.resets_at, now);
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .text_size(px(11.5))
            .text_color(theme.text_muted.opacity(0.7))
            .child(
                div()
                    .w(px(48.0))
                    .flex_none()
                    .truncate()
                    .child(SharedString::from(window.label.clone())),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(56.0))
                    .max_w(px(230.0))
                    .h(px(5.0))
                    .rounded_full()
                    .overflow_hidden()
                    .bg(crate::theme::ink(0.07))
                    .when(fraction > 0.0, |el| {
                        el.child(
                            div()
                                .h_full()
                                // A 1.5% floor keeps tiny non-zero usage
                                // visible (comet `max(used, 1.5)%`).
                                .w(gpui::relative(fraction.max(0.015)))
                                .rounded_full()
                                .bg(fill),
                        )
                    }),
            )
            .child(
                div()
                    .w(px(64.0))
                    .flex_none()
                    .text_right()
                    .child(SharedString::from(format!(
                        "{}% used",
                        (fraction * 100.0).round() as u32
                    ))),
            )
            .when_some(reset, |el, reset| {
                el.child(
                    div()
                        .flex_none()
                        .truncate()
                        .text_color(theme.text_muted.opacity(0.45))
                        .child(SharedString::from(reset)),
                )
            })
            .into_any_element()
    }

    /// The "Not understood" block under a provider card. Hidden entirely at
    /// zero — with the harness-side Ignored tier in place, zero is the normal
    /// state, so a non-zero count means the provider shipped something new.
    /// Muted neutrals, not amber: this is information about protocol drift,
    /// not a state the user can resolve. Layout constants are plain numbers
    /// and do not vary by appearance (gpui-ui rules); discriminators are wire
    /// identifiers already sanitized at the harness boundary.
    fn render_diagnostics_block(
        &self,
        harness: HarnessId,
        name: &str,
        theme: &Theme,
        now_ms: i64,
    ) -> Option<AnyElement> {
        let (_total, sentence) = diagnostics_rollup(&self.diagnostics, harness, name)?;
        let report = self.diagnostics.iter().find(|d| d.harness == harness)?;
        let mut block = div()
            .mt(px(8.0))
            .rounded(px(12.0))
            .border_1()
            .border_color(theme.border)
            .bg(theme.surface)
            // 20px, not 16 — this block stacks directly beneath the account
            // card, whose rows inset by 20 (`widgets::section_card`). At 16 the
            // heading and every row visibly hang left of the avatar above them.
            // Verified by rendering the two cards adjacent.
            .px(px(20.0))
            .py(px(12.0))
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(
                div()
                    .text_size(px(13.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text)
                    .child(SharedString::from("Not understood")),
            )
            .child(
                div()
                    .text_size(px(12.5))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(sentence)),
            );
        for entry in &report.entries {
            block = block.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(12.0))
                    .text_size(px(12.0))
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .truncate()
                            .text_color(theme.text)
                            .child(SharedString::from(entry.discriminator.clone())),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_color(theme.text_muted)
                            .child(SharedString::from(format!("×{}", entry.count))),
                    )
                    .child(div().flex_none().text_color(theme.text_muted).child(
                        SharedString::from(format!(
                            "last {}",
                            format_ago(entry.last_seen_ms, now_ms)
                        )),
                    )),
            );
        }
        if report.overflow > 0 {
            block = block.child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(format!(
                        "…and {} more {}",
                        report.overflow,
                        message_noun(report.overflow)
                    ))),
            );
        }
        Some(block.into_any_element())
    }

    /// One account row (comet settings.agents.tsx `AccountRow`): initial
    /// avatar, email + usage meters left; badges over the Switch/Forget
    /// actions right-anchored.
    fn render_account_row(
        &self,
        account: &AgentAccount,
        ix: usize,
        first: bool,
        theme: &Theme,
        now: DateTime<Utc>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use crate::settings::widgets;
        let is_busy = self.busy_account.as_deref() == Some(account.id.as_str());
        let email: SharedString = account
            .email
            .clone()
            .or_else(|| account.display_name.clone())
            .unwrap_or_else(|| "Unknown account".into())
            .into();
        let initial: SharedString = email
            .chars()
            .next()
            .map(|c| c.to_uppercase().to_string())
            .unwrap_or_else(|| "?".into())
            .into();
        let switch_account = account.clone();
        let forget_account = account.clone();

        let badges = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .when(account.active, |el| {
                el.child(widgets::badge_active(theme, "Active"))
            })
            .when_some(account.plan_label.clone(), |el, plan| {
                el.child(widgets::badge(theme, plan))
            });

        // Actions only on INACTIVE accounts (comet `{!account.active && …}`):
        // an icon-only Forget (trash, hover → foreground) then Switch, which
        // reads "Switching…" while the activate round-trips.
        let actions: Option<gpui::Div> = (!account.active).then(|| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(4.0))
                .child(
                    div()
                        .id(("account-forget", ix))
                        .rounded(px(6.0))
                        .px(px(6.0))
                        .py(px(4.0))
                        .text_color(theme.text_muted)
                        .cursor_pointer()
                        .when(is_busy, |el| el.opacity(0.5))
                        .hover(|s| s.bg(crate::theme::ink(0.06)).text_color(theme.text))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.account_action(methods::FORGET_AGENT_ACCOUNT, &forget_account, cx);
                        }))
                        .child(
                            crate::icons::icon(crate::icons::TRASH_BIN_MINIMALISTIC)
                                .size(px(14.0))
                                .text_color(theme.text_muted),
                        ),
                )
                .when(account.switchable, |el| {
                    el.child(
                        crate::popover::btn_primary(
                            theme,
                            if is_busy { "Switching…" } else { "Switch" },
                        )
                        .id(("account-switch", ix))
                        .px(px(8.0))
                        .py(px(4.0))
                        .rounded(px(6.0))
                        .text_size(px(11.5))
                        .when(is_busy, |el| el.opacity(0.5))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.account_action(
                                methods::ACTIVATE_AGENT_ACCOUNT,
                                &switch_account,
                                cx,
                            );
                        })),
                    )
                })
        });

        div()
            .px(px(20.0))
            .py(px(14.0))
            .when(!first, |el| el.border_t_1().border_color(theme.border))
            .flex()
            .flex_row()
            .items_stretch()
            .gap(px(12.0))
            .child(
                // Initial avatar: size-8 rounded-full border bg-white/[0.03].
                div()
                    .flex_none()
                    .self_center()
                    .size(px(32.0))
                    .rounded_full()
                    .border_1()
                    .border_color(theme.border)
                    .bg(crate::theme::ink(0.03))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(12.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text_muted)
                    .child(initial),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(widgets::row_title(theme, email))
                    .map(|el| {
                        // Meters XOR the quiet fallback line — never both
                        // (comet: `usage ? meters : "Usage unavailable"…`).
                        if account.usage_windows.is_empty() {
                            el.child(
                                div()
                                    .mt(px(6.0))
                                    .truncate()
                                    .text_size(px(11.5))
                                    .text_color(theme.text_muted.opacity(0.6))
                                    .child(SharedString::from(if account.switchable {
                                        "Usage unavailable"
                                    } else {
                                        "Credentials unavailable"
                                    })),
                            )
                        } else {
                            el.child(
                                div().mt(px(6.0)).flex().flex_col().gap(px(4.0)).children(
                                    account
                                        .usage_windows
                                        .iter()
                                        .map(|w| self.render_usage_meter(w, theme, now)),
                                ),
                            )
                        }
                    }),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .flex_col()
                    .items_end()
                    .justify_between()
                    .gap(px(8.0))
                    .child(badges)
                    .children(actions),
            )
            .into_any_element()
    }

    fn render_login_dialog(
        &mut self,
        viewport: gpui::Size<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let theme = Theme::of(cx).clone();
        let red_text = theme.danger_muted.opacity(0.9); // red-300
        let login = self.login.as_ref()?;
        let title = login.title();
        let url_link =
            |id: &'static str, label: &'static str, url: &str, cx: &mut Context<Self>| {
                let open_url = url.to_string();
                // "Reopen the …" text link (comet: `text-[12px]
                // text-muted-foreground/60 hover:underline`).
                div()
                    .id(id)
                    .mt(px(6.0))
                    .text_size(px(12.0))
                    .text_color(theme.text_muted.opacity(0.6))
                    .truncate()
                    .cursor_pointer()
                    .hover(|s| s.text_color(theme.text))
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.open_url(&open_url);
                    }))
                    .child(SharedString::from(label))
            };
        let body: AnyElement = match login {
            LoginFlow::Starting { .. } => div()
                .mt(px(8.0))
                .child(popover::skeleton_rows(
                    "login-starting",
                    &theme,
                    2,
                    cx.entity_id(),
                    cx,
                ))
                .into_any_element(),
            LoginFlow::PasteCode {
                start,
                submitting,
                error,
                ..
            } => {
                let submitting = *submitting;
                div()
                    .flex()
                    .flex_col()
                    .child(div().mt(px(8.0)).child(popover::dialog_body(
                        &theme,
                        "A browser window opened. Sign in to the account you want to add, \
                         approve access, then paste the code Anthropic shows you below. Your \
                         current login is untouched until you switch.",
                    )))
                    .child(url_link(
                        "login-open-url",
                        "Reopen the authorization page",
                        &start.url,
                        cx,
                    ))
                    .child(
                        div().mt(px(12.0)).child(
                            popover::dialog_field(self.code_input.clone().into_any_element())
                                .font_family(theme.font_mono.clone())
                                .text_size(px(13.0)),
                        ),
                    )
                    .when_some(error.clone(), |el, message| {
                        el.child(
                            div()
                                .mt(px(8.0))
                                .text_size(px(12.0))
                                .text_color(red_text)
                                .child(message),
                        )
                    })
                    .child(
                        div()
                            .mt(px(16.0))
                            .flex()
                            .flex_row()
                            .justify_end()
                            .gap(px(8.0))
                            .child(
                                popover::btn_ghost(&theme, "Cancel", "login-cancel")
                                    .id("login-cancel")
                                    .on_click(cx.listener(|this, _, _, cx| this.cancel_login(cx))),
                            )
                            .child(
                                popover::btn_primary(
                                    &theme,
                                    if submitting {
                                        "Verifying…"
                                    } else {
                                        "Add account"
                                    },
                                )
                                .id("login-submit-code")
                                .when(submitting, |el| el.opacity(0.5))
                                .on_click(cx.listener(|this, _, _, cx| this.submit_code(cx))),
                            ),
                    )
                    .into_any_element()
            }
            LoginFlow::Browser {
                start,
                message,
                error,
                ..
            } => {
                let has_error = error.is_some();
                div()
                    .flex()
                    .flex_col()
                    .child(div().mt(px(8.0)).child(popover::dialog_body(
                        &theme,
                        "Finish signing in to OpenAI in your browser. The new login is \
                         captured in an isolated profile — your current session is untouched \
                         until you switch.",
                    )))
                    .child(url_link(
                        "login-open-url-browser",
                        "Reopen the sign-in page",
                        &start.url,
                        cx,
                    ))
                    .when(!has_error, |el| {
                        el.child(
                            div()
                                .mt(px(16.0))
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(8.0))
                                .child(crate::loaders::gradient_spinner(
                                    "login-poll",
                                    &theme,
                                    3.0,
                                    cx.entity_id(),
                                    cx,
                                ))
                                .child(
                                    div()
                                        .text_size(px(12.5))
                                        .text_color(theme.text_muted.opacity(0.7))
                                        .child(message.clone().unwrap_or_else(|| {
                                            SharedString::from("Waiting for the browser…")
                                        })),
                                ),
                        )
                    })
                    .when_some(error.clone(), |el, message| {
                        el.child(
                            div()
                                .mt(px(12.0))
                                .text_size(px(12.0))
                                .text_color(red_text)
                                .child(message),
                        )
                    })
                    .child(
                        div().mt(px(16.0)).flex().flex_row().justify_end().child(
                            popover::btn_ghost(
                                &theme,
                                if has_error { "Close" } else { "Cancel" },
                                "login-cancel",
                            )
                            .id("login-cancel")
                            .on_click(cx.listener(|this, _, _, cx| this.cancel_login(cx))),
                        ),
                    )
                    .into_any_element()
            }
        };
        let card = popover::dialog_card(&theme)
            .child(popover::dialog_title(&theme, title))
            .child(body)
            .into_any_element();
        Some(popover::modal("add-account-dialog", viewport, card))
    }

    /// A ghost account row (comet settings.agents.tsx `SkeletonRow`): avatar,
    /// email line, two usage-meter ghosts, a badge — same geometry as the real
    /// row so loaded data lands without a layout jump. `dim` fades row two.
    fn render_skeleton_row(
        &self,
        _id: (&'static str, usize),
        dim: bool,
        first: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use crate::motion;
        let delta = motion::pulse_delta(&motion::COMET_PULSE, cx.entity_id(), cx);
        let ghost = |w: gpui::Length, h: f32, round_full: bool| {
            div()
                .w(w)
                .h(px(h))
                .flex_none()
                .map(|el| {
                    if round_full {
                        el.rounded_full()
                    } else {
                        el.rounded(px(4.0))
                    }
                })
                .bg(crate::theme::ink(0.05))
        };
        let meters = div()
            .mt(px(8.0))
            .flex()
            .flex_col()
            .gap(px(7.0))
            .children((0..2).map(|_| {
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .child(ghost(px(48.0).into(), 9.0, false))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(56.0))
                            .max_w(px(230.0))
                            .h(px(5.0))
                            .rounded_full()
                            .bg(crate::theme::ink(0.04)),
                    )
                    .child(ghost(px(64.0).into(), 9.0, false))
            }));
        let inner = div()
            .flex()
            .flex_row()
            .items_stretch()
            .gap(px(12.0))
            .child(
                div()
                    .flex_none()
                    .self_center()
                    .size(px(32.0))
                    .rounded_full()
                    .bg(crate::theme::ink(0.05)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(ghost(px(176.0).into(), 13.0, false).max_w(gpui::relative(0.6)))
                    .child(meters),
            )
            .child(div().flex_none().flex().flex_col().items_end().child(ghost(
                px(64.0).into(),
                21.0,
                true,
            )));
        div()
            .px(px(20.0))
            .py(px(14.0))
            .when(!first, |el| el.border_t_1().border_color(theme.border))
            .when(dim, |el| el.opacity(0.6))
            .child(inner.opacity(0.55 + 0.35 * motion::pulse_wave(delta)))
            .into_any_element()
    }
}

impl Render for AccountsPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        use crate::settings::widgets;
        let theme = Theme::of(cx).clone();
        let now = Utc::now();
        let dialog = self.render_login_dialog(window.viewport_size(), cx);
        let refreshing = matches!(self.snapshot, Loadable::Loading);
        let account_count = self
            .snapshot
            .ready()
            .map(|s| s.accounts.len())
            .filter(|&n| n > 0);

        let provider_icon = |harness: HarnessId| match harness {
            HarnessId::Codex => (crate::icons::OPENAI_MARK, None),
            HarnessId::Cursor => (crate::icons::CURSOR_MARK, None),
            _ => (
                crate::icons::CLAUDE_MARK,
                Some(crate::icons::claude_brand()),
            ),
        };
        // Brand mark inside a 24px centered box (comet: `grid size-6
        // place-items-center [&_svg]:size-4`).
        let provider_mark = |harness: HarnessId, theme: &Theme| {
            let (mark, tint) = provider_icon(harness);
            div()
                .flex_none()
                .size(px(24.0))
                .flex()
                .items_center()
                .justify_center()
                .child(
                    crate::icons::icon(mark)
                        .size(px(16.0))
                        .text_color(tint.unwrap_or(theme.text_muted)),
                )
        };

        // One section per provider (comet settings.agents.tsx `ProviderSection`):
        // brand header + Add account, then the account rows card.
        let sections: Vec<AnyElement> = match &self.snapshot {
            Loadable::Idle | Loadable::Loading => PROVIDERS
                .into_iter()
                .map(|(harness, name, _cli)| {
                    let skeleton_id = match harness {
                        HarnessId::Codex => "accounts-skeleton-codex",
                        _ => "accounts-skeleton-claude",
                    };
                    div()
                        .mt(px(24.0))
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(8.0))
                                .child(provider_mark(harness, &theme))
                                .child(
                                    div()
                                        .text_size(px(14.0))
                                        .font_weight(gpui::FontWeight::MEDIUM)
                                        .text_color(theme.text)
                                        .child(SharedString::from(name)),
                                ),
                        )
                        .child(
                            // Ghost rows shaped like real ones (row two dimmed)
                            // so the card keeps its size while data develops.
                            widgets::section_card(&theme)
                                .mt(px(8.0))
                                .child(self.render_skeleton_row(
                                    (skeleton_id, 0),
                                    false,
                                    true,
                                    &theme,
                                    cx,
                                ))
                                .child(self.render_skeleton_row(
                                    (skeleton_id, 1),
                                    true,
                                    false,
                                    &theme,
                                    cx,
                                )),
                        )
                        .into_any_element()
                })
                .collect(),
            Loadable::Error(message) => {
                let message = message.clone();
                vec![
                    widgets::error_strip_with_hint(&theme, message, "Click to retry")
                        .id("accounts-load-error")
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| {
                            // Retry IS the visit's first successful list — force usage.
                            this.load(force_usage_for(LoadTrigger::Retry), cx)
                        }))
                        .into_any_element(),
                ]
            }
            Loadable::Ready(snapshot) => {
                let snapshot = snapshot.clone();
                PROVIDERS
                    .into_iter()
                    .map(|(harness, name, cli)| {
                        let accounts = provider_accounts(&snapshot, harness);
                        // EVERY warning renders its own strip (comet maps them).
                        let warnings: Vec<String> = snapshot
                            .warnings
                            .iter()
                            .filter(|w| w.harness == harness)
                            .map(|w| w.message.clone())
                            .collect();
                        let rows: Vec<AnyElement> = accounts
                            .iter()
                            .enumerate()
                            .map(|(ix, account)| {
                                self.render_account_row(account, ix, ix == 0, &theme, now, cx)
                            })
                            .collect();
                        let add_id: SharedString = format!("add-account-{name}").into();
                        let install_for = self.harnesses.iter().find(|d| d.id == harness);
                        let card = widgets::section_card(&theme).mt(px(8.0));
                        let card = if rows.is_empty() {
                            card.child(
                                div()
                                    .px(px(20.0))
                                    .py(px(32.0))
                                    .text_center()
                                    .text_size(px(14.0))
                                    .text_color(theme.text_muted.opacity(0.6))
                                    .child(SharedString::from(format!(
                                        "No {name} login detected on this device — sign in \
                                         with \u{201C}{cli}\u{201D} or add an account."
                                    ))),
                            )
                        } else {
                            card.children(rows)
                        };
                        div()
                            .mt(px(24.0))
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap(px(8.0))
                                    .child(provider_mark(harness, &theme))
                                    .child(
                                        // `min_w_0` + `flex_1` on the name column
                                        // and `truncate` on the path: a resolved
                                        // Windows path is long enough to push the
                                        // Add-account button off the row
                                        // otherwise, and this header is a fixed
                                        // two-line shape whatever the path is.
                                        div()
                                            .flex()
                                            .flex_col()
                                            .flex_1()
                                            .min_w_0()
                                            .child(
                                                div()
                                                    .text_size(px(14.0))
                                                    .font_weight(gpui::FontWeight::MEDIUM)
                                                    .text_color(theme.text)
                                                    .child(SharedString::from(name)),
                                            )
                                            .children(install_line(install_for).map(|line| {
                                                div()
                                                    .flex()
                                                    .flex_row()
                                                    .items_baseline()
                                                    .gap(px(6.0))
                                                    .mt(px(2.0))
                                                    .text_size(px(11.5))
                                                    .child(
                                                        div()
                                                            .flex_none()
                                                            .text_color(theme.text_muted)
                                                            .child(SharedString::from(
                                                                line.summary,
                                                            )),
                                                    )
                                                    .child(
                                                        div()
                                                            .min_w_0()
                                                            .truncate()
                                                            .text_color(
                                                                theme.text_muted.opacity(0.6),
                                                            )
                                                            .child(SharedString::from(line.path)),
                                                    )
                                            })),
                                    )
                                    .child(
                                        widgets::ghost_action(&theme)
                                            .id(add_id)
                                            .hover(|s| widgets::ghost_hover(&theme, s))
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.start_login(harness, cx);
                                            }))
                                            .child(
                                                crate::icons::icon(crate::icons::ADD_CIRCLE)
                                                    .size(px(16.0))
                                                    .text_color(theme.text_muted),
                                            )
                                            .child(SharedString::from("Add account")),
                                    ),
                            )
                            .children(
                                warnings
                                    .into_iter()
                                    .map(|warning| widgets::warning_strip(&theme, warning)),
                            )
                            .child(card)
                            .children(self.render_diagnostics_block(
                                harness,
                                name,
                                &theme,
                                now.timestamp_millis(),
                            ))
                            .into_any_element()
                    })
                    .collect()
            }
        };

        div()
            .id("accounts-page")
            .size_full()
            .overflow_y_scroll()
            .child(
                widgets::page_column()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(10.0))
                            .child(widgets::page_header(&theme, "Accounts", account_count))
                            .child(div().flex_1())
                            .child(
                                // `text-[12.5px]` + leading 16px Refresh icon,
                                // dimmed while a refresh is in flight (comet
                                // `disabled:opacity-50`).
                                widgets::ghost_action(&theme)
                                    .id("accounts-refresh")
                                    .flex_none()
                                    .text_size(px(12.5))
                                    .hover(|s| widgets::ghost_hover(&theme, s))
                                    .when(refreshing, |el| el.opacity(0.5))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.load(force_usage_for(LoadTrigger::Refresh), cx)
                                    }))
                                    .child(
                                        crate::icons::icon(crate::icons::REFRESH)
                                            .size(px(16.0))
                                            .text_color(theme.text_muted),
                                    )
                                    .child(SharedString::from("Refresh")),
                            )
                            .child(self.render_device_switcher(&theme, cx)),
                    )
                    .child(widgets::page_subtitle(
                        &theme,
                        "The Claude Code and Codex logins on this device. Comet detects the \
                         live session, keeps each account backed up, and can swap between \
                         them.",
                    ))
                    .when_some(self.error.clone(), |el, message| {
                        el.child(
                            widgets::error_strip(&theme, message)
                                .id("accounts-action-error")
                                .cursor_pointer()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.error = None;
                                    cx.notify();
                                })),
                        )
                    })
                    .children(sections)
                    // Footer note (comet: `mt-6 text-[12px] leading-relaxed
                    // text-muted-foreground/60`).
                    .child(
                        div()
                            .mt(px(24.0))
                            .text_size(px(12.0))
                            .line_height(px(19.0))
                            .text_color(theme.text_muted.opacity(0.6))
                            .child(SharedString::from(
                                "Switching rewrites the CLI\u{2019}s stored login, so new \
                                 agent sessions use the selected account immediately. On \
                                 macOS, an already-running Claude Code can hold the previous \
                                 login for up to ~30 seconds (Keychain cache).",
                            )),
                    ),
            )
            .when_some(dialog, |el, dialog| el.child(dialog))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;
    use comet_proto::{HarnessInstall, InstallMethod};

    fn descriptor(
        availability: HarnessAvailability,
        install: Option<HarnessInstall>,
    ) -> HarnessDescriptor {
        HarnessDescriptor {
            id: HarnessId::ClaudeCode,
            name: "Claude Code".into(),
            capabilities: comet_proto::HarnessCapabilities::default(),
            availability,
            install,
        }
    }

    /// The ordinary case, built from the capture's real values
    /// (`captures/2026-08-11-agent-version-install-method.md`).
    #[test]
    fn a_working_install_reads_version_then_method() {
        let d = descriptor(
            HarnessAvailability::Available {
                version: Some("2.1.228".into()),
            },
            Some(HarnessInstall {
                path: r"C:\Users\coding\.local\bin\claude.exe".into(),
                method: InstallMethod::Native,
            }),
        );
        let line = install_line(Some(&d)).expect("a probed install has a line");
        assert_eq!(line.summary, "2.1.228 \u{00b7} Native installer");
        assert_eq!(line.path, r"C:\Users\coding\.local\bin\claude.exe");
    }

    /// The case the whole sibling design exists for: the CLI resolved and then
    /// failed, so there is no version — but the line still names the binary,
    /// which is the only way to tell which of two installs was asked.
    #[test]
    fn a_broken_install_still_names_its_binary() {
        let d = descriptor(
            HarnessAvailability::unavailable("Not working", Some("`--version` failed.".into())),
            Some(HarnessInstall {
                path: r"C:\Users\coding\AppData\Roaming\npm\codex.cmd".into(),
                method: InstallMethod::Npm,
            }),
        );
        let line = install_line(Some(&d)).expect("a broken install still has a line");
        assert_eq!(line.summary, "npm (global)");
        assert_eq!(line.path, r"C:\Users\coding\AppData\Roaming\npm\codex.cmd");
    }

    /// A CLI that answered without a parseable version is still available. The
    /// method carries the line on its own rather than the row vanishing.
    #[test]
    fn an_unreadable_version_leaves_the_method_alone_on_the_line() {
        let d = descriptor(
            HarnessAvailability::Available { version: None },
            Some(HarnessInstall {
                path: "/opt/homebrew/bin/codex".into(),
                method: InstallMethod::Homebrew,
            }),
        );
        let line = install_line(Some(&d)).unwrap();
        assert_eq!(line.summary, "Homebrew");
    }

    /// Nothing to say, said as nothing. A CLI that never resolved has no path,
    /// and an engine that predates the field sends no descriptor half at all —
    /// neither may render an empty or invented line.
    #[test]
    fn nothing_known_renders_no_line() {
        assert_eq!(install_line(None), None);
        let unresolved = descriptor(
            HarnessAvailability::unavailable("Not installed", None),
            None,
        );
        assert_eq!(install_line(Some(&unresolved)), None);
        let unprobed = descriptor(HarnessAvailability::Unknown, None);
        assert_eq!(install_line(Some(&unprobed)), None);
    }

    #[test]
    fn first_load_of_a_visit_forces_the_usage_probe() {
        // The engine only probes usage when forced (M5c); without forcing on
        // mount, the first Accounts open always rendered "Usage unavailable".
        assert!(force_usage_for(LoadTrigger::Mount));
        // A retry after a failed load is still the visit's first successful
        // list — same requirement.
        assert!(force_usage_for(LoadTrigger::Retry));
        // Explicit refresh and a just-completed login always re-probe.
        assert!(force_usage_for(LoadTrigger::Refresh));
        assert!(force_usage_for(LoadTrigger::PostLogin));
        // Switch/Forget re-lists ride the still-warm 60s cache.
        assert!(!force_usage_for(LoadTrigger::PostAction));
    }

    #[test]
    fn usage_thresholds_match_comet() {
        assert_eq!(usage_level(0.0), UsageLevel::Normal);
        assert_eq!(usage_level(0.79), UsageLevel::Normal);
        assert_eq!(usage_level(0.80), UsageLevel::Warn);
        assert_eq!(usage_level(0.94), UsageLevel::Warn);
        assert_eq!(usage_level(0.95), UsageLevel::Critical);
        assert_eq!(usage_level(1.0), UsageLevel::Critical);
    }

    #[test]
    fn usage_colors_map_to_theme_accents() {
        let theme = Theme::dark();
        assert_eq!(usage_color(UsageLevel::Normal, &theme), theme.accent);
        assert_eq!(usage_color(UsageLevel::Warn, &theme), theme.warning);
        assert_eq!(usage_color(UsageLevel::Critical, &theme), theme.danger);
    }

    #[test]
    fn reset_formatting_is_absolute() {
        use chrono::Local;
        let now = Utc::now();
        assert_eq!(format_reset(None, now), None);
        // Within ~22h: a local clock time ("resets 3:45 PM").
        let soon = now + TimeDelta::minutes(125);
        assert_eq!(
            format_reset(Some(soon), now),
            Some(format!(
                "resets {}",
                soon.with_timezone(&Local).format("%-I:%M %p")
            ))
        );
        // Beyond: a short weekday ("resets Mon").
        let later = now + TimeDelta::days(3);
        assert_eq!(
            format_reset(Some(later), now),
            Some(format!(
                "resets {}",
                later.with_timezone(&Local).format("%a")
            ))
        );
    }

    #[test]
    fn provider_grouping_puts_active_first() {
        let account = |id: &str, harness: HarnessId, active: bool| AgentAccount {
            id: id.into(),
            harness,
            email: None,
            plan_label: None,
            active,
            usage_windows: vec![],
            display_name: None,
            organization: None,
            auth_kind: None,
            switchable: true,
            saved_at: None,
        };
        let snapshot = AgentAccountsSnapshot {
            accounts: vec![
                account("c1", HarnessId::ClaudeCode, false),
                account("x1", HarnessId::Codex, false),
                account("c2", HarnessId::ClaudeCode, true),
            ],
            warnings: vec![],
        };
        let claude = provider_accounts(&snapshot, HarnessId::ClaudeCode);
        let ids: Vec<&str> = claude.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, ["c2", "c1"], "active account leads");
        assert_eq!(provider_accounts(&snapshot, HarnessId::Codex).len(), 1);
        assert!(provider_accounts(&snapshot, HarnessId::Cursor).is_empty());
    }

    fn diag_report(
        harness: HarnessId,
        counts: &[(&str, u64)],
        overflow: u64,
    ) -> comet_engine::registry::HarnessDiagnostics {
        comet_engine::registry::HarnessDiagnostics {
            harness,
            entries: counts
                .iter()
                .map(|(d, c)| comet_engine::registry::HarnessDiagnosticEntry {
                    discriminator: (*d).into(),
                    severity: comet_proto::DiagnosticSeverity::Unknown,
                    count: *c,
                    first_seen_ms: 0,
                    last_seen_ms: 0,
                })
                .collect(),
            overflow,
        }
    }

    /// "Hidden when zero" is the honest normal state — the Ignored tier is
    /// what makes it true. The rollup is the single gate for the block.
    #[test]
    fn diagnostics_rollup_hides_at_zero_and_sums_overflow() {
        // Absent provider → hidden.
        assert_eq!(diagnostics_rollup(&[], HarnessId::Codex, "Codex"), None);
        // Present but empty → hidden.
        assert_eq!(
            diagnostics_rollup(
                &[diag_report(HarnessId::Codex, &[], 0)],
                HarnessId::Codex,
                "Codex"
            ),
            None
        );
        // Counts + overflow sum; copy pluralizes and names the Agent — never
        // the harness id.
        let list = vec![diag_report(
            HarnessId::Codex,
            &[
                ("thread/checkpoint/created", 4),
                ("item/webSearch/started", 2),
            ],
            3,
        )];
        assert_eq!(
            diagnostics_rollup(&list, HarnessId::Codex, "Codex"),
            Some((
                9,
                "Codex sent 9 messages this session that Comet doesn't recognize.".to_string()
            ))
        );
        // The OTHER provider's card stays hidden.
        assert_eq!(
            diagnostics_rollup(&list, HarnessId::ClaudeCode, "Claude Code"),
            None
        );
        // Singular.
        let one = vec![diag_report(HarnessId::ClaudeCode, &[("unparseable", 1)], 0)];
        assert_eq!(
            diagnostics_rollup(&one, HarnessId::ClaudeCode, "Claude Code"),
            Some((
                1,
                "Claude Code sent 1 message this session that Comet doesn't recognize.".to_string()
            ))
        );
    }

    /// Overflow with no entries at all: the discriminator cap was hit, so the
    /// block shows the sentence over zero rows plus the overflow line. The
    /// most unusual render the block can produce, and the only shape where
    /// the count comes entirely from the overflow bucket.
    #[test]
    fn diagnostics_rollup_counts_overflow_with_no_entries() {
        let only_overflow = vec![diag_report(HarnessId::Codex, &[], 5)];
        assert_eq!(
            diagnostics_rollup(&only_overflow, HarnessId::Codex, "Codex"),
            Some((
                5,
                "Codex sent 5 messages this session that Comet doesn't recognize.".to_string()
            ))
        );
        // Exactly one overflowed message — the 65th distinct discriminator's
        // first arrival. Both the sentence and the overflow line must say
        // "message", which is why the choice is one shared function.
        let single = vec![diag_report(HarnessId::ClaudeCode, &[], 1)];
        assert_eq!(
            diagnostics_rollup(&single, HarnessId::ClaudeCode, "Claude Code"),
            Some((
                1,
                "Claude Code sent 1 message this session that Comet doesn't recognize.".to_string()
            ))
        );
        assert_eq!(message_noun(0), "messages");
        assert_eq!(message_noun(1), "message");
        assert_eq!(message_noun(2), "messages");
    }

    #[test]
    fn format_ago_buckets() {
        assert_eq!(format_ago(1_000, 5_000), "just now");
        assert_eq!(format_ago(0, 45_000), "45s ago");
        assert_eq!(format_ago(0, 120_000), "2m ago");
        assert_eq!(format_ago(0, 7_200_000), "2h ago");
        // A clock that ran backwards degrades to "just now", never negative.
        assert_eq!(format_ago(10_000, 5_000), "just now");
    }

    /// Each bucket edge from both sides — an off-by-one in `format_ago` can
    /// only live here, and nowhere the coarse cases above would catch it.
    #[test]
    fn format_ago_bucket_edges() {
        // "just now" holds up to but not including 10s.
        assert_eq!(format_ago(0, 9_999), "just now");
        assert_eq!(format_ago(0, 10_000), "10s ago");
        // Seconds hold up to but not including a minute.
        assert_eq!(format_ago(0, 59_000), "59s ago");
        assert_eq!(format_ago(0, 60_000), "1m ago");
        // Minutes hold up to but not including an hour.
        assert_eq!(format_ago(0, 3_599_000), "59m ago");
        assert_eq!(format_ago(0, 3_600_000), "1h ago");
        // There is deliberately no day bucket: these counts are per-boot, so
        // hours stay readable and an ageing session reads "31h ago" rather
        // than needing a calendar. Pinned so it stays a decision.
        assert_eq!(format_ago(0, 111_600_000), "31h ago");
    }
}
