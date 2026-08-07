//! The slow-request toast: what the user sees when a request is taking too
//! long, instead of a skeleton that might never resolve.
//!
//! Design prototype — see `.agents/rules/user-facing-errors.md`. The rule says
//! no waiting state may last forever. A hard timeout would satisfy that by
//! failing work that was going to succeed (a large `git diff`, a cold repo
//! scan), so this takes the other route: the wait is never cut short, but after
//! a few seconds it stops being silent and offers a way out.
//!
//! Shape decisions:
//!
//! - **Top-anchored and transient**, not an inline strip. The slot that is
//!   waiting may be scrolled out of view or behind a closed popover, so the
//!   notice cannot live inside it.
//! - **Cancel is text, not a button.** Cancelling is the secondary action —
//!   waiting is usually right — and a filled button next to a spinner reads as
//!   the thing you are supposed to press.
//! - **Cancel is optional.** Some waits have nothing to offer: a background
//!   revalidation already has its rows on screen, so stopping it changes
//!   nothing the user can see, and a control that does nothing visible is worse
//!   than no control. Those register with [`begin_uncancellable`] and get the
//!   sentence without the affordance.
//! - **It never becomes an error on its own.** If the reply lands, the toast
//!   leaves and the surface fills in. The user cancels, or the work finishes.

use gpui::{App, EntityId, IntoElement, SharedString, Styled, div, prelude::*, px};

use crate::errors::Loading;
use crate::frost::frosted;
use crate::motion;
use crate::theme::{Theme, hairline};

/// Corner radius of the toast card. Passed to [`frosted`] as well — the blur
/// has to be clipped to the same rounding or it squares off the corners.
const TOAST_RADIUS: f32 = 10.0;

/// Distance from the window's top edge: [`Theme::TITLEBAR_HEIGHT`] plus a gap.
///
/// Must clear the titlebar outright. At 8px the card landed *in* the tab strip,
/// overlapping the session tabs and sitting level with the window controls,
/// which read as broken chrome rather than as a notice. Below the strip it
/// floats over the content card, which is what it is talking about.
pub const TOAST_TOP_INSET: f32 = Theme::TITLEBAR_HEIGHT + 8.0;

/// The clickable "Cancel" text.
///
/// Returned already identified so the caller only attaches the listener:
/// `toast::cancel_link(&theme).on_click(cx.listener(…))`.
pub fn cancel_link(theme: &Theme) -> gpui::Stateful<gpui::Div> {
    div()
        .id("slow-request-cancel")
        .flex_none()
        .px(px(6.0))
        .py(px(2.0))
        .rounded(px(5.0))
        .text_size(px(12.0))
        .font_weight(gpui::FontWeight::MEDIUM)
        // Accent rather than danger: cancelling a slow read destroys nothing.
        .text_color(theme.accent)
        .cursor_pointer()
        .hover(|s| s.bg(crate::theme::ink(0.08)))
        .child("Cancel")
}

/// The toast card: spinner, message, and the caller's cancel affordance.
///
/// `message` is the waiting sentence ("Still loading the model list…") — the
/// same `Loading` vocabulary the failure copy uses, so a wait and a failure
/// name the thing identically.
/// `cancel` is `None` for a wait that cannot usefully be stopped; the card then
/// closes up to symmetric padding rather than leaving a gap where the link was.
/// `left_inset` is the sidebar's CURRENT width, so the card centres over the
/// content pane rather than the window.
///
/// Window-centring looked wrong for a reason worth recording: the sidebar
/// pushes the app's optical centre to the right, so a window-centred card lands
/// at an x that matches nothing — neither the perceived middle nor the content
/// card's middle. It read as dropped rather than placed. Taking the live tweened
/// width (not the settled target) means the toast rides the sidebar's 200ms
/// collapse instead of jumping when it finishes.
pub fn slow_request_toast(
    theme: &Theme,
    message: impl Into<SharedString>,
    cancel: Option<impl IntoElement>,
    left_inset: f32,
    view: EntityId,
    cx: &mut App,
) -> impl IntoElement {
    // 8px on the right only because the cancel link carries 6px of its own; with
    // no link the card would read as lopsided, so it closes up to match the left.
    let pad_right = if cancel.is_some() { 8.0 } else { 12.0 };
    let card = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(10.0))
        .pl(px(12.0))
        .pr(px(pad_right))
        .py(px(8.0))
        .rounded(px(TOAST_RADIUS))
        .border_1()
        .border_color(hairline(0.10))
        .bg(theme.surface_raised)
        .shadow(crate::theme::card_selected_shadows())
        // Only the CARD swallows clicks. The positioning wrapper spans the
        // window's full width, so occluding there would put a dead strip
        // across the top of the app.
        .occlude()
        .child(
            div()
                .flex_none()
                // The same working indicator the sessions sidebar uses for a
                // live run — a wait looks like a wait everywhere in the app.
                // 4px cells, not the 2.5px of a sidebar status dot: at that
                // size the indicator was legible only as a smudge, and the
                // motion — the part that says "still going" — was lost.
                .child(crate::loaders::mini_gradient_spinner(
                    "slow-request-toast-spinner",
                    4.0,
                    view,
                    cx,
                )),
        )
        .child(
            div()
                .min_w_0()
                .text_size(px(12.0))
                .text_color(theme.text)
                .child(message.into()),
        )
        .children(cancel);

    // The entrance rides the CARD, never this wrapper: every motion helper
    // sets `relative()` to apply its translate as a positional inset, which
    // would silently undo the wrapper's `absolute()` and drop the toast into
    // normal flow at the bottom of the render tree.
    //
    // Frosted so the whole card composites in one scene layer; the radius must
    // match the card's own rounding (see frost.rs).
    div()
        .absolute()
        .top(px(TOAST_TOP_INSET))
        .left(px(left_inset))
        .right_0()
        .flex()
        .flex_row()
        .justify_center()
        // `Frosted` is an Element, not `Styled`, so the animation cannot ride
        // it directly — it rides a plain div wrapping it.
        .child(motion::toast_in(
            "slow-request-toast",
            div().child(frosted(TOAST_RADIUS, 16.0, card)),
        ))
}

// ---------------------------------------------------------------------------
// In-flight registry
// ---------------------------------------------------------------------------

/// Every request currently being waited on, app-wide.
///
/// A global rather than state on one view, because the waiting and the telling
/// happen in different entities: `Pickers` owns the request, `Shell` paints the
/// toast, and neither should have to know about the other to make a wait
/// visible.
#[derive(Default)]
pub struct SlowRequests {
    next_id: u64,
    entries: Vec<Entry>,
}

struct Entry {
    id: u64,
    what: Loading,
    started: std::time::Instant,
    /// Taken by [`cancel`]. Sending it resolves the waiter's select arm, which
    /// drops the RPC future — and `RpcClient`'s `PendingGuard` turns that drop
    /// into a `{id, cancel}` frame, so the engine stops working on it too.
    ///
    /// `None` from the start for a wait registered via [`begin_uncancellable`].
    /// It is never `None` for a cancellable entry that is still in the registry:
    /// [`cancel`] takes the sender and removes the entry in the same breath.
    cancel: Option<futures::channel::oneshot::Sender<()>>,
}

impl gpui::Global for SlowRequests {}

/// A wait worth telling the user about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlowRequest {
    pub id: u64,
    pub what: Loading,
    /// Whether to offer the Cancel affordance — see [`begin_uncancellable`].
    pub cancellable: bool,
}

/// Register a request as in flight. The receiver resolves if the user cancels.
pub fn begin(cx: &mut App, what: Loading) -> (u64, futures::channel::oneshot::Receiver<()>) {
    let (tx, rx) = futures::channel::oneshot::channel();
    let id = push(cx, what, Some(tx));
    (id, rx)
}

/// Register a wait that the user is told about but is not offered a way out of.
///
/// For a load that is **revalidating content already on screen**: the rows stay
/// painted whether it finishes or not, so Cancel would be a control with no
/// visible effect. Naming the wait is still worth doing — it explains why the
/// list has not changed yet.
///
/// Not an escape hatch from the no-unbounded-wait rule. Use it only where the
/// surface has something to show *now*; a skeleton must always be cancellable,
/// because there the wait is the whole of what the user can see.
pub fn begin_uncancellable(cx: &mut App, what: Loading) -> u64 {
    push(cx, what, None)
}

fn push(cx: &mut App, what: Loading, cancel: Option<futures::channel::oneshot::Sender<()>>) -> u64 {
    let registry = cx.default_global::<SlowRequests>();
    registry.next_id += 1;
    let id = registry.next_id;
    registry.entries.push(Entry {
        id,
        what,
        started: std::time::Instant::now(),
        cancel,
    });
    id
}

/// Deregister a finished request. Idempotent — a cancelled request is removed
/// when it is cancelled *and* again when its task unwinds.
pub fn end(cx: &mut App, id: u64) {
    cx.default_global::<SlowRequests>()
        .entries
        .retain(|e| e.id != id);
}

/// The request that has been waiting longest, if it has been waiting long
/// enough to be worth mentioning.
///
/// Oldest rather than newest: if two are slow, the one the user has been
/// staring at is the older one. Only ever one toast — a stack of them would be
/// a worse version of the skeletons it replaces.
pub fn slow(cx: &App) -> Option<SlowRequest> {
    cx.try_global::<SlowRequests>()?
        .slow_at(std::time::Instant::now())
}

impl SlowRequests {
    /// [`slow`] against an explicit clock, so the selection rule is testable
    /// without sleeping through [`SLOW_AFTER`].
    fn slow_at(&self, now: std::time::Instant) -> Option<SlowRequest> {
        self.entries
            .iter()
            .filter(|e| now.duration_since(e.started) >= SLOW_AFTER)
            .min_by_key(|e| e.started)
            .map(|e| SlowRequest {
                id: e.id,
                what: e.what,
                cancellable: e.cancel.is_some(),
            })
    }
}

/// Whether anything is in flight at all — the cue for `Shell` to keep frames
/// coming so a request can be *noticed* crossing [`SLOW_AFTER`]. Nothing fires
/// an event at that moment; it is a clock, so someone has to look.
pub fn any_in_flight(cx: &App) -> bool {
    cx.try_global::<SlowRequests>()
        .is_some_and(|r| !r.entries.is_empty())
}

/// Cancel a request: resolve its waiter, then forget it.
///
/// A no-op for a wait registered via [`begin_uncancellable`]. Forgetting one
/// without stopping it would take the toast down while the work carried on —
/// the silent wait this whole module exists to prevent, just re-entered through
/// the control that was supposed to be the way out.
pub fn cancel(cx: &mut App, id: u64) {
    let registry = cx.default_global::<SlowRequests>();
    let Some(entry) = registry.entries.iter_mut().find(|e| e.id == id) else {
        return;
    };
    let Some(tx) = entry.cancel.take() else {
        return;
    };
    let _ = tx.send(());
    registry.entries.retain(|e| e.id != id);
}

/// What the user sees in the slot they were waiting on after cancelling.
///
/// Deliberately identical in shape to a failure — an inline strip with Retry —
/// so cancelling lands somewhere familiar and always has a way back. A slot
/// left empty would be the unbounded wait again, just with extra steps.
pub fn cancelled_message(what: Loading) -> String {
    format!("Stopped loading {}.", what.noun())
}

/// The toast's sentence. No trailing ellipsis: the spinner beside it already
/// says "ongoing", and the two together read as fussy.
pub fn waiting_message(what: Loading) -> String {
    format!("Still loading {}", what.noun())
}

/// How long a request may run before the toast appears.
///
/// Long enough that ordinary work never triggers it — a local `ListHarnesses`
/// answers in single-digit milliseconds, and a cold repo scan in well under
/// this — and short enough that a user who is already wondering gets an answer
/// rather than a decision to make about whether to wait.
pub const SLOW_AFTER: std::time::Duration = std::time::Duration::from_secs(4);

#[cfg(test)]
mod tests {
    use super::*;

    /// The threshold has to sit above real work and below a user's patience.
    /// Pinned so a later "let's make it snappier" edit has to argue with both
    /// bounds rather than just lowering a number.
    /// Build a registry with entries at given ages, ids 1..n. Every entry is
    /// cancellable; [`uncancellable_registry`] covers the other kind.
    ///
    /// Takes `now` rather than reading the clock itself: capturing a second
    /// instant in here put every entry a few microseconds "younger" than the
    /// test's own `now`, which is invisible for the coarse cases and flips the
    /// exact-threshold one.
    fn registry(now: std::time::Instant, ages: &[std::time::Duration]) -> SlowRequests {
        SlowRequests {
            next_id: ages.len() as u64,
            entries: ages
                .iter()
                .enumerate()
                .map(|(i, age)| Entry {
                    id: i as u64 + 1,
                    what: Loading::Models,
                    started: now - *age,
                    cancel: Some(futures::channel::oneshot::channel().0),
                })
                .collect(),
        }
    }

    /// The same, for waits registered via [`begin_uncancellable`].
    fn uncancellable_registry(
        now: std::time::Instant,
        ages: &[std::time::Duration],
    ) -> SlowRequests {
        let mut registry = registry(now, ages);
        for entry in &mut registry.entries {
            entry.cancel = None;
        }
        registry
    }

    /// A fast request must never raise the toast. This is the guard on the
    /// whole feature being noise: almost every request finishes in
    /// milliseconds, and a toast on those would be worse than the skeleton.
    #[test]
    fn a_request_under_the_threshold_is_not_slow() {
        let now = std::time::Instant::now();
        let quick = registry(now, &[std::time::Duration::from_millis(80)]);
        assert_eq!(quick.slow_at(now), None);
        // Exactly at the threshold counts — the boundary belongs to "slow", so
        // there is no window where a request is over time and still silent.
        let boundary = registry(now, &[SLOW_AFTER]);
        assert_eq!(boundary.slow_at(now).map(|s| s.id), Some(1));
    }

    /// With several slow requests the OLDEST wins: that is the one the user has
    /// been staring at. Only ever one toast.
    #[test]
    fn the_longest_wait_is_the_one_reported() {
        let now = std::time::Instant::now();
        let mixed = registry(
            now,
            &[
                std::time::Duration::from_millis(10), // fast, ignored
                std::time::Duration::from_secs(30),   // oldest slow
                std::time::Duration::from_secs(5),    // slow but younger
            ],
        );
        assert_eq!(mixed.slow_at(now).map(|s| s.id), Some(2));
    }

    /// A wait that revalidates content already on screen is reported without a
    /// Cancel: stopping it would change nothing the user can see. The *sentence*
    /// is identical either way — the flexibility is in the affordance, not in
    /// how the wait is described.
    #[test]
    fn an_uncancellable_wait_is_still_reported() {
        let now = std::time::Instant::now();
        let quiet = uncancellable_registry(now, &[std::time::Duration::from_secs(9)]);
        let reported = quiet.slow_at(now).expect("a 9s wait is worth naming");
        assert_eq!(reported.what, Loading::Models);
        assert!(!reported.cancellable);
        // And an ordinary registration still offers the way out.
        let ordinary = registry(now, &[std::time::Duration::from_secs(9)]);
        assert!(ordinary.slow_at(now).expect("slow").cancellable);
    }

    /// Oldest-wins does not care which kind it picks: an uncancellable wait that
    /// started first must not be skipped in favour of a cancellable younger one,
    /// or the toast would misreport which wait it is talking about.
    #[test]
    fn cancellability_does_not_change_which_wait_is_reported() {
        let now = std::time::Instant::now();
        let mut mixed = uncancellable_registry(
            now,
            &[
                std::time::Duration::from_secs(30), // oldest, uncancellable
                std::time::Duration::from_secs(5),
            ],
        );
        mixed.entries[1].cancel = Some(futures::channel::oneshot::channel().0);
        let reported = mixed.slow_at(now).expect("slow");
        assert_eq!(reported.id, 1);
        assert!(!reported.cancellable);
    }

    /// An empty registry is silent, and a registry of only fast work is too.
    #[test]
    fn nothing_in_flight_shows_nothing() {
        let now = std::time::Instant::now();
        assert_eq!(registry(now, &[]).slow_at(now), None);
        assert_eq!(
            registry(now, &[std::time::Duration::from_millis(1); 5]).slow_at(now),
            None
        );
    }

    /// Cancelling must not be permanent. The slot holds an `Error` so the user
    /// sees what happened, and `Pickers::rearm_cancelled_models` puts it back
    /// to `Idle` on the next discrete demand — otherwise `ensure_models`'
    /// refuse-to-reload-an-Error rule would make one cancel disable the model
    /// list for the rest of the session.
    ///
    /// The re-arm deliberately does NOT happen from render: `ensure_models`
    /// runs every frame, so re-arming there would restart the request the
    /// instant it was cancelled and the toast could never be dismissed.
    #[test]
    fn a_cancel_is_remembered_as_re_armable_not_as_a_dead_slot() {
        // The contract lives in Pickers; this pins the reasoning next to the
        // cancel machinery so a later edit to either has to meet both halves:
        // a cancelled slot must be (a) visibly explained and (b) reloadable.
        let explained = cancelled_message(Loading::Models);
        assert!(
            explained.starts_with("Stopped loading"),
            "a cancel must not read as a failure: {explained}"
        );
        assert!(
            !explained.contains("Couldn't"),
            "cancel copy must not borrow failure wording: {explained}"
        );
    }

    /// The cancelled slot must land on a message, never an empty slot — an
    /// empty slot is the unbounded wait again with extra steps.
    #[test]
    fn cancelling_names_what_it_stopped() {
        let message = cancelled_message(Loading::Models);
        assert!(message.contains("the model list"), "{message}");
        assert!(!message.is_empty());
        // And the waiting sentence names the same thing the same way.
        assert!(waiting_message(Loading::Models).contains("the model list"));
    }

    #[test]
    fn the_slow_threshold_is_above_ordinary_work() {
        assert!(
            SLOW_AFTER >= std::time::Duration::from_secs(3),
            "below ~3s this fires on cold-cache work that was about to succeed"
        );
        assert!(
            SLOW_AFTER <= std::time::Duration::from_secs(8),
            "past ~8s the user has already decided the app is broken"
        );
    }
}
