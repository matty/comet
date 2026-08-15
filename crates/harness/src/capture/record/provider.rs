use serde_json::Value;

use super::session::Session;
use crate::capture::Provider;

/// The provider seam, and the whole of it.
///
/// Claude Code and Codex differ on exactly four things (design
/// `2026-08-14-provider-capture-simplification-design.md` §3.1): how to
/// spawn them, how their stdout lines frame into a value, how to hand-shake
/// once connected, and which frame means a turn is over. Everything else —
/// sequencing, channel tagging, timeouts, the raw write, partial-capture
/// classification — is provider-neutral and lives once in [`Session`].
///
/// SPAWN is the fourth member and it is *not* here — see the amendment in
/// `plan-preamble.md`'s "seam, written out once": which launch a scenario
/// needs varies per scenario as well as per provider (Claude alone needs
/// three: bare model discovery, non-bare command discovery, a run), so it
/// lives on `ScenarioSpec::launch`, a per-row `fn` pointer into the same
/// per-provider production launch builders. Do not move it back here — a
/// first draft did, and it could not express `command-discovery` needing a
/// different launch than `model-discovery` without a bypass that silently
/// left the two indistinguishable at the table.
///
/// Nothing anticipatory: no approval abstraction (approvals are a scenario,
/// not a provider capability) and no capability enum. A fifth member is
/// added when a third provider — pi, or an ACP agent — has a *recording* to
/// design against, per this repository's capture-before-planning rule, not
/// before.
pub(super) trait CaptureProvider: Sized {
    /// `"claude"` | `"codex"` — used in the raw directory name and internal
    /// tracing. Not for user-facing copy: that needs "Claude"/"Codex", which
    /// is `session::provider_display_name`, kept off the trait since it's
    /// presentation, not identity.
    const NAME: &'static str;

    /// Which archive provider this records as.
    fn provider() -> Provider;

    /// FRAMING. Both providers are newline-delimited JSON, parsed
    /// identically — `serde_json::from_str(line).ok()`. Codex's JSON-RPC
    /// envelope (`id`/`method`/`result`) is read directly off the parsed
    /// value by callers (`wait_for`'s `pick` closures); nothing here unwraps
    /// it. Returns `None` for a line that is not a frame at all (progress
    /// noise, a blank line) — never an error, because a line the recorder
    /// cannot read is evidence, not a failure.
    fn frame(line: &str) -> Option<Value>;

    /// HANDSHAKE. Claude's `control_request`/`initialize`; Codex's
    /// `initialize` → await reply → `initialized`, in that order.
    ///
    /// `thread/start` is NOT part of the Codex handshake: no discovery
    /// scenario sends one. It belongs to the run scenario bodies that need a
    /// thread (Task 5) — a handshake that branches per scenario would not be
    /// a seam member.
    async fn handshake(
        session: &mut Session<Self>,
        input: &super::scenarios::ScenarioInput,
    ) -> anyhow::Result<()>;

    /// TURN-COMPLETE. Which frame means "stop recording".
    ///
    /// Unused by production code until the SCENARIOS table wires a run
    /// scenario's `body` in (Task 7); Tasks 2, 3 and 5 write the callers
    /// (`Session::wait_for_turn_end`) in the meantime, exercised only by each
    /// scenario file's own tests. Covered directly by each provider's own
    /// unit tests as well.
    #[allow(dead_code)]
    fn turn_complete(frame: &Value) -> bool;
}
