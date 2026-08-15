use crate::capture::record::providers::claude::ClaudeProvider;
use crate::capture::record::scenarios::ScenarioInput;
use crate::capture::record::session::Session;

/// Model discovery's entire drive is the handshake: `record()` already ran
/// it before calling this body, so there is nothing left to do here. Kept as
/// its own function (rather than folded away) because it is a named row in
/// [`super::SCENARIOS`], not because its body is nontrivial.
pub(in crate::capture::record) async fn model_discovery(
    _session: &mut Session<ClaudeProvider>,
    _input: &ScenarioInput,
) -> anyhow::Result<()> {
    Ok(())
}

/// Command discovery's driving is identical to model discovery's — the
/// difference between the two scenarios is entirely in which launch builder
/// `record()` selects (`command_discovery_launch` vs `model_discovery_launch`
/// picks `--bare` or not), never in what happens after spawn.
pub(in crate::capture::record) async fn command_discovery(
    _session: &mut Session<ClaudeProvider>,
    _input: &ScenarioInput,
) -> anyhow::Result<()> {
    Ok(())
}
