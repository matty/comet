use std::path::Path;

use serde_json::Value;

use super::session::Session;
use crate::capture::Provider;
use crate::launch::LaunchDescriptor;

/// The provider seam, and the whole of it.
///
/// Claude Code and Codex differ on exactly four things (design
/// `2026-08-14-provider-capture-simplification-design.md` §3.1): how to
/// spawn them, how their stdout lines frame into a value, how to hand-shake
/// once connected, and which frame means a turn is over. Everything else —
/// sequencing, channel tagging, timeouts, the raw write, partial-capture
/// classification — is provider-neutral and lives once in [`Session`].
///
/// Nothing anticipatory: no approval abstraction (approvals are a scenario,
/// not a provider capability) and no capability enum. A fifth member is
/// added when a third provider — pi, or an ACP agent — has a *recording* to
/// design against, per this repository's capture-before-planning rule, not
/// before.
pub(super) trait CaptureProvider: Sized {
    /// `"claude"` | `"codex"` — used in errors and in the raw directory name.
    const NAME: &'static str;

    /// Which archive provider this records as.
    fn provider() -> Provider;

    /// SPAWN. Built from production launch builders only.
    fn launch(
        input: &super::scenarios::ScenarioInput,
        executable: &Path,
    ) -> anyhow::Result<LaunchDescriptor>;

    /// FRAMING. Both providers are newline-delimited JSON; Codex additionally
    /// unwraps JSON-RPC. Returns `None` for a line that is not a frame at all
    /// (progress noise, a blank line) — never an error, because a line the
    /// recorder cannot read is evidence, not a failure.
    fn frame(line: &str) -> Option<Value>;

    /// HANDSHAKE. Claude's `control_request`/`initialize`; Codex's
    /// `initialized` + `thread/start`.
    async fn handshake(
        session: &mut Session<Self>,
        input: &super::scenarios::ScenarioInput,
    ) -> anyhow::Result<()>;

    /// TURN-COMPLETE. Which frame means "stop recording".
    ///
    /// Unused by production code until a run scenario needs
    /// [`Session::wait_for_turn_end`] (Tasks 2 and 5); covered directly by
    /// each provider's own unit tests in the meantime.
    #[allow(dead_code)]
    fn turn_complete(frame: &Value) -> bool;
}
