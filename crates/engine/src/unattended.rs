//! The unattended policy: how long a wait no client can answer may last, and
//! what the transcript says when it ends.
//!
//! The rule is one sentence — **a wait on a human is bounded only while it is
//! unanswerable.** A connected supervisor means answerable (the sidebar already
//! floats it to the top of the Active list via `attention_rank`), so nothing
//! expires. No supervisor connected means nobody can answer, and that stretch
//! is capped.
//!
//! Wall clock, not `Instant`: a laptop asleep for six hours has genuinely been
//! unattended for six hours, and a monotonic clock may disagree.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};

/// 24 hours. Deliberately generous: a blocked provider is *idle* waiting on a
/// tool result, not spinning, so the cost of a long bound is a parked process
/// and its memory. Overridden by `COMET_UNATTENDED_TIMEOUT_SECS`.
pub const DEFAULT_UNATTENDED_TIMEOUT_SECS: u64 = 86_400;

/// The env var's name, named once so the warn log and any future reference
/// agree with each other.
const UNATTENDED_TIMEOUT_ENV_VAR: &str = "COMET_UNATTENDED_TIMEOUT_SECS";

/// `COMET_UNATTENDED_TIMEOUT_SECS`, resolved once so the headed and headless
/// entry points (`apps/comet/src/main.rs`, `crates/ui/src/state.rs`) cannot
/// silently drift on what counts as a valid override.
///
/// Thin wrapper over [`parse_unattended_timeout`] — the env read itself isn't
/// unit-testable (env vars are process-global), so the parsing logic lives in
/// a pure function this only calls.
pub fn unattended_timeout_from_env() -> Duration {
    parse_unattended_timeout(std::env::var(UNATTENDED_TIMEOUT_ENV_VAR).ok())
}

/// Parse the raw env value, falling back to the default and warning once for
/// anything that isn't a positive integer.
///
/// Zero is deliberately rejected rather than taken literally: an operator
/// writing `0` means "turn this off", and a literal zero-second bound would
/// instead expire every blocked run almost immediately — the opposite of
/// what they asked for. Treating it as invalid (and falling back to the
/// generous default) is the only reading that fails safe in both directions.
fn parse_unattended_timeout(raw: Option<String>) -> Duration {
    match raw.as_deref().map(|v| v.trim()) {
        None => {}
        Some(value) => match value.parse::<u64>() {
            Ok(secs) if secs > 0 => return Duration::from_secs(secs),
            _ => tracing::warn!(
                var = UNATTENDED_TIMEOUT_ENV_VAR,
                value,
                "ignoring invalid unattended timeout override; using the default"
            ),
        },
    }
    Duration::from_secs(DEFAULT_UNATTENDED_TIMEOUT_SECS)
}

/// Which kind of parked wait ended the turn. Only changes one word of the note.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitKind {
    Approval,
    Answer,
}

#[derive(Debug)]
struct State {
    attached: usize,
    unattended_since: Option<DateTime<Utc>>,
}

/// How many supervisors are attached to this engine, and since when nobody has
/// been.
///
/// **Per-engine, not per-chat, and that is the decision not the shortcut.** A
/// user running a swarm of chats has one open at a time; per-chat presence
/// would expire every chat they are not currently looking at. Connected means
/// available.
#[derive(Debug)]
pub struct Presence {
    state: Mutex<State>,
}

impl Presence {
    /// A fresh engine is unattended: a daemon nobody ever connects to must
    /// still expire its blocked runs, so the stretch starts at boot.
    pub fn new(now: DateTime<Utc>) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                attached: 0,
                unattended_since: Some(now),
            }),
        })
    }

    /// Register one attached supervisor. Detaches when the lease drops —
    /// including on a panicking connection task, which is why this is a guard
    /// and not a pair of calls.
    pub fn attach(self: &Arc<Self>) -> PresenceLease {
        self.attach_at();
        PresenceLease {
            presence: Arc::clone(self),
        }
    }

    /// The increment on its own. Split out because a `PresenceLease`'s `Drop`
    /// necessarily reads `Utc::now()`, so a test asserting the stamped instant
    /// cannot go through a lease.
    fn attach_at(&self) {
        let mut state = self.state.lock().expect("presence mutex poisoned");
        state.attached += 1;
        state.unattended_since = None;
    }

    pub fn attached_count(&self) -> usize {
        self.state.lock().expect("presence mutex poisoned").attached
    }

    /// `None` while anyone is attached — the wait is answerable and unbounded.
    pub fn unattended_since(&self) -> Option<DateTime<Utc>> {
        self.state
            .lock()
            .expect("presence mutex poisoned")
            .unattended_since
    }

    /// Detach at an explicit instant. `PresenceLease::drop` calls this with
    /// `Utc::now()`; tests call it directly for determinism.
    pub fn detach_at(&self, now: DateTime<Utc>) {
        let mut state = self.state.lock().expect("presence mutex poisoned");
        state.attached = state.attached.saturating_sub(1);
        if state.attached == 0 && state.unattended_since.is_none() {
            state.unattended_since = Some(now);
        }
    }
}

/// Held for one connection's lifetime.
#[derive(Debug)]
pub struct PresenceLease {
    presence: Arc<Presence>,
}

impl Drop for PresenceLease {
    fn drop(&mut self) {
        self.presence.detach_at(Utc::now());
    }
}

/// Is this parked wait past its deadline?
///
/// `deadline = max(parked_at, unattended_since) + bound`. **Both terms are
/// load-bearing.** A run that parks while the engine is already unattended must
/// get a full window, not the remainder of a stretch that began before it
/// existed — without the `max`, a run parking 23h into a disconnect dies in an
/// hour.
///
/// `unattended_since == None` means somebody is attached, so the wait is
/// answerable and never due.
pub fn due_for_expiry(
    parked_at: DateTime<Utc>,
    unattended_since: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    bound: Duration,
) -> bool {
    let Some(unattended_since) = unattended_since else {
        return false;
    };
    let started = parked_at.max(unattended_since);
    // An absurd override must not panic; treat unrepresentable as never due.
    let Ok(bound) = TimeDelta::from_std(bound) else {
        return false;
    };
    match started.checked_add_signed(bound) {
        Some(deadline) => now > deadline,
        None => false,
    }
}

/// How often the sweeper wakes. A quarter of the bound, clamped so the default
/// costs one wakeup a minute and a test bound of 100ms is still observable.
pub fn sweep_interval(bound: Duration) -> Duration {
    (bound / 4).clamp(Duration::from_millis(250), Duration::from_secs(60))
}

/// Largest whole unit that fits, so a 30-second override reads honestly in the
/// live check's screenshot.
pub fn humanize_bound(bound: Duration) -> String {
    let secs = bound.as_secs();
    let plural = |n: u64, unit: &str| {
        if n == 1 {
            format!("1 {unit}")
        } else {
            format!("{n} {unit}s")
        }
    };
    if secs >= 3_600 && secs.is_multiple_of(3_600) {
        plural(secs / 3_600, "hour")
    } else if secs >= 60 && secs.is_multiple_of(60) {
        plural(secs / 60, "minute")
    } else {
        plural(secs, "second")
    }
}

/// What the user reads on reconnecting to an expired turn.
///
/// Two clauses by design: what happened, then what to do. The second is true
/// because the next dispatch resumes the same provider session, so sending
/// again continues the conversation rather than restarting it.
pub fn unattended_note(bound: Duration, waited_on: WaitKind) -> String {
    let needed = match waited_on {
        WaitKind::Approval => "your approval",
        WaitKind::Answer => "your answer",
    };
    format!(
        "Stopped after {} — this turn needed {needed} and nothing was connected to ask. \
         Send again to continue; the session still has its context.",
        humanize_bound(bound)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + secs, 0).unwrap()
    }

    /// A fresh engine has nobody attached, so the stretch starts at boot: a
    /// daemon that never sees a client must still expire its blocked runs.
    #[test]
    fn a_new_presence_is_unattended_from_boot() {
        let presence = Presence::new(t(0));
        assert_eq!(presence.attached_count(), 0);
        assert_eq!(presence.unattended_since(), Some(t(0)));
    }

    #[test]
    fn one_client_leaving_is_still_attended() {
        let presence = Presence::new(t(0));
        let first = presence.attach();
        let _second = presence.attach();
        assert_eq!(presence.unattended_since(), None);

        drop(first);
        assert_eq!(presence.attached_count(), 1);
        assert_eq!(
            presence.unattended_since(),
            None,
            "one client left is still attended"
        );
    }

    /// Uses the raw `attach_at`/`detach_at` pair rather than a lease, so the
    /// stamped instant is deterministic. A lease's `Drop` reads `Utc::now()`,
    /// which cannot be asserted against a fixture time.
    #[test]
    fn the_last_detach_stamps_the_stretch() {
        let presence = Presence::new(t(0));
        presence.attach_at();
        assert_eq!(presence.unattended_since(), None);

        presence.detach_at(t(60));
        assert_eq!(presence.attached_count(), 0);
        assert_eq!(presence.unattended_since(), Some(t(60)));
    }

    /// The guard half: `Drop` must detach, including when a connection task
    /// panics. This is why presence is a lease and not a call pair.
    #[test]
    fn dropping_a_lease_detaches() {
        let presence = Presence::new(t(0));
        {
            let _lease = presence.attach();
            assert_eq!(presence.attached_count(), 1);
        }
        assert_eq!(presence.attached_count(), 0);
        assert!(presence.unattended_since().is_some());
    }

    #[test]
    fn re_attaching_clears_a_stamped_stretch() {
        let presence = Presence::new(t(0));
        let lease = presence.attach();
        assert_eq!(presence.unattended_since(), None);
        drop(lease);
        assert!(presence.unattended_since().is_some());
        let _again = presence.attach();
        assert_eq!(presence.unattended_since(), None);
    }

    /// The rule: bounded only while unanswerable.
    #[test]
    fn attended_never_expires_however_long_it_waits() {
        let bound = Duration::from_secs(60);
        assert!(!due_for_expiry(t(0), None, t(86_400), bound));
    }

    #[test]
    fn unattended_expires_once_the_bound_passes() {
        let bound = Duration::from_secs(60);
        assert!(!due_for_expiry(t(0), Some(t(0)), t(59), bound));
        assert!(due_for_expiry(t(0), Some(t(0)), t(61), bound));
    }

    /// The `max`: a run that parks 23h into a disconnect gets a FULL window,
    /// not the hour left over from a stretch that began before it existed.
    #[test]
    fn parking_while_already_unattended_gets_a_full_window() {
        let bound = Duration::from_secs(3_600);
        // Disconnected at t=0, parked at t=82_800 (23h later).
        assert!(!due_for_expiry(t(82_800), Some(t(0)), t(83_000), bound));
        assert!(due_for_expiry(t(82_800), Some(t(0)), t(86_500), bound));
    }

    #[test]
    fn sweep_interval_is_cheap_at_the_default_and_quick_in_tests() {
        assert_eq!(
            sweep_interval(Duration::from_secs(DEFAULT_UNATTENDED_TIMEOUT_SECS)),
            Duration::from_secs(60)
        );
        assert_eq!(
            sweep_interval(Duration::from_millis(100)),
            Duration::from_millis(250)
        );
    }

    /// An operator who lowers the bound must not get a lying transcript.
    #[test]
    fn the_note_states_the_configured_bound_not_a_hard_coded_day() {
        let note = unattended_note(Duration::from_secs(86_400), WaitKind::Approval);
        assert!(note.contains("24 hours"), "{note}");
        assert!(note.contains("approval"), "{note}");
        assert!(note.contains("Send again"), "{note}");

        let short = unattended_note(Duration::from_secs(30), WaitKind::Answer);
        assert!(short.contains("30 seconds"), "{short}");
        assert!(short.contains("answer"), "{short}");
        assert!(
            !short.contains("24 hours"),
            "the copy must render the real bound: {short}"
        );
    }

    #[test]
    fn humanize_picks_the_largest_whole_unit_that_fits() {
        assert_eq!(humanize_bound(Duration::from_secs(86_400)), "24 hours");
        assert_eq!(humanize_bound(Duration::from_secs(3_600)), "1 hour");
        assert_eq!(humanize_bound(Duration::from_secs(1_800)), "30 minutes");
        assert_eq!(humanize_bound(Duration::from_secs(30)), "30 seconds");
        assert_eq!(humanize_bound(Duration::from_secs(5_400)), "90 minutes");
    }

    /// An absurd override must not panic. `TimeDelta::from_std` rejects it and
    /// the wait is treated as never due, which fails in the safe direction.
    #[test]
    fn an_absurd_bound_is_never_due_instead_of_panicking() {
        assert!(!due_for_expiry(
            t(0),
            Some(t(0)),
            t(1),
            Duration::from_secs(u64::MAX)
        ));
    }

    #[test]
    fn unset_falls_back_to_the_default_silently() {
        assert_eq!(
            parse_unattended_timeout(None),
            Duration::from_secs(DEFAULT_UNATTENDED_TIMEOUT_SECS)
        );
    }

    #[test]
    fn a_good_value_overrides_the_default() {
        assert_eq!(
            parse_unattended_timeout(Some("30".into())),
            Duration::from_secs(30)
        );
    }

    #[test]
    fn a_malformed_value_falls_back_to_the_default() {
        assert_eq!(
            parse_unattended_timeout(Some("not-a-number".into())),
            Duration::from_secs(DEFAULT_UNATTENDED_TIMEOUT_SECS)
        );
    }

    #[test]
    fn an_empty_value_falls_back_to_the_default() {
        assert_eq!(
            parse_unattended_timeout(Some(String::new())),
            Duration::from_secs(DEFAULT_UNATTENDED_TIMEOUT_SECS)
        );
    }

    /// Zero is rejected rather than taken literally: an operator writing `0`
    /// means "turn this off", and a literal zero-second bound would instead
    /// expire every blocked run almost immediately.
    #[test]
    fn zero_is_treated_as_invalid_not_as_an_instant_bound() {
        assert_eq!(
            parse_unattended_timeout(Some("0".into())),
            Duration::from_secs(DEFAULT_UNATTENDED_TIMEOUT_SECS)
        );
    }
}
