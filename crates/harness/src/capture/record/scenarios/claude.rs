use std::path::Path;

use crate::capture::record::providers::claude::ClaudeProvider;
use crate::capture::record::scenarios::ScenarioInput;
use crate::capture::record::session::Session;
use crate::launch::LaunchDescriptor;

/// SPAWN for `model-discovery` and its `-neutral-cwd`/`-project-cwd`
/// aliases: the bare initialize launch.
pub(in crate::capture::record) fn model_discovery_launch(
    input: &ScenarioInput,
    executable: &Path,
) -> anyhow::Result<LaunchDescriptor> {
    let cwd = input.cwd.clone().unwrap_or_else(std::env::temp_dir);
    Ok(crate::claude::discovery::model_discovery_launch(
        executable, &cwd,
    ))
}

/// SPAWN for `command-discovery`: the non-bare initialize launch — the
/// entire reason this scenario needs its own launch fn rather than reusing
/// `model_discovery_launch`. Recording command discovery with `--bare`
/// would be silently wrong, which is exactly the drift a first draft of
/// this seam let happen when `launch` lived on the provider trait instead
/// of the scenario row.
pub(in crate::capture::record) fn command_discovery_launch(
    input: &ScenarioInput,
    executable: &Path,
) -> anyhow::Result<LaunchDescriptor> {
    let cwd = input.cwd.clone().unwrap_or_else(std::env::temp_dir);
    Ok(crate::claude::commands::command_discovery_launch(
        executable, &cwd,
    ))
}

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
