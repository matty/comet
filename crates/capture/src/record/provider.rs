use serde_json::Value;

use super::session::Session;
use crate::Provider;

/// The provider seam, and the whole of it.
///
/// Claude Code and Codex differ on exactly four things (design
/// `2026-08-14-provider-capture-simplification-design.md` §3.1): how to spawn them, how their
/// stdout lines frame into a value, how to hand-shake once connected, and which frame means a
/// turn is over. Everything else — sequencing, channel tagging, timeouts, the raw write,
/// partial-capture classification — is provider-neutral and lives once in [`Session`].
///
/// SPAWN is not a fifth member: which launch a scenario needs varies per scenario as well as per
/// provider (Claude alone needs three), so it lives on `ScenarioSpec::launch` instead. Do not
/// move it back here — a first draft did, and it could not express `command-discovery` needing a
/// different launch than `model-discovery` without a bypass that silently left the two
/// indistinguishable at the table.
///
/// Nothing anticipatory: no approval abstraction, no capability enum. A member is added only
/// once a third provider has a *recording* to design against, not before.
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

    /// HANDSHAKE. Claude's `control_request`/`initialize`; Codex's `initialize` → await reply →
    /// `initialized`, in that order.
    ///
    /// `thread/start` is NOT part of the Codex handshake: no discovery scenario sends one. It
    /// belongs to run scenario bodies that need a thread — a handshake that branches per
    /// scenario would not be a seam member.
    ///
    /// **The scenario body calls this, not the recorder.** `record_generic` never calls a seam
    /// member unconditionally: every discovery body and every Codex run body opens with
    /// `Self::handshake(session, input).await?` itself; a Claude run body calls nothing, because
    /// Comet's own run path sends the user turn as its first stdin line, no `control_request`/
    /// `initialize` at all. The provider owns *how* to handshake; the scenario owns *whether*.
    async fn handshake(
        session: &mut Session<Self>,
        input: &super::scenarios::ScenarioInput,
    ) -> anyhow::Result<()>;

    /// TURN-COMPLETE. Which frame means "stop recording".
    fn turn_complete(frame: &Value) -> bool;
}
