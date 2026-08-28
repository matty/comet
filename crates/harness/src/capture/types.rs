use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::launch::{LaunchDescriptor, StdioMode};

/// The reproducible, reviewable portion of a provider launch command.
///
/// Only explicitly allowlisted entries are retained here; PATH and unrelated
/// environment values can contain local paths or credentials.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CommandSnapshot {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub configured_env: BTreeMap<String, String>,
    pub stdin: StdioMode,
    pub stdout: StdioMode,
    pub stderr: StdioMode,
    pub kill_on_drop: bool,
    #[cfg(windows)]
    pub creation_flags: u32,
}

impl CommandSnapshot {
    // The capture recorder is the only caller; see `record/session.rs`.
    pub(crate) fn from_launch(launch: &LaunchDescriptor) -> Self {
        // Both decide what the CLI answers with, so both belong in the reviewable record:
        // `CODEX_HOME` selects Codex's account and config, `CLAUDE_CONFIG_DIR` selects Claude's
        // (D91). A manifest missing the latter cannot distinguish an isolated capture from one
        // carrying whatever plugins and models the capturer's machine happened to have.
        const CAPTURED_ENV: &[&str] = &["CODEX_HOME", "CLAUDE_CONFIG_DIR"];

        let configured_env = launch
            .configured_env
            .iter()
            .filter_map(|(key, value)| {
                let key = key.to_string_lossy();
                if !CAPTURED_ENV.contains(&key.as_ref()) {
                    return None;
                }
                Some((key.into_owned(), value.to_string_lossy().into_owned()))
            })
            .collect();

        Self {
            program: launch.program.to_string_lossy().into_owned(),
            args: launch
                .args
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect(),
            cwd: launch
                .cwd
                .as_ref()
                .map(|cwd| cwd.to_string_lossy().into_owned()),
            configured_env,
            stdin: launch.stdin,
            stdout: launch.stdout,
            stderr: launch.stderr,
            kill_on_drop: launch.kill_on_drop,
            #[cfg(windows)]
            creation_flags: launch.creation_flags,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Claude,
    Codex,
    /// Every agent recorded through an Agent Client Protocol adapter. One
    /// variant, not one per adapter: the wire is the protocol, and which agent
    /// sits behind it is the scenario row's business.
    Acp,
}

/// The corpus TOP-LEVEL directory name a scenario's evidence is promoted
/// under — `crates/harness/tests/corpus/<this>/<version>/<scenario>/`.
///
/// `Provider::Claude`/`Provider::Codex` answer their own lowercase name, same
/// as before this function existed. `Provider::Acp` is the one case the enum's
/// own doc comment names as "the scenario row's business": Claude and Codex
/// each speak one wire to one CLI, so the enum variant IS the corpus
/// provider. ACP is one wire spoken by several unrelated agents with their
/// own, unrelated version numbers (grok 1.0.5, codex-acp 1.7.0,
/// claude-agent-acp 0.70.0) — collapsing them under a single `acp/<version>/`
/// directory would force one version number to stand for three CLIs, so each
/// gets its own top-level directory instead, matched off the scenario's own
/// name rather than the wire it happens to share.
///
/// Matched by substring rather than an exhaustive enum-of-agents: adding a
/// fourth ACP agent (Hermes, PR1) means adding its scenarios' names here, not
/// widening `Provider` and every `match` that already exhausts it.
pub fn corpus_provider_name(provider: Provider, scenario_name: &str) -> &'static str {
    match provider {
        Provider::Claude => "claude",
        Provider::Codex => "codex",
        Provider::Acp => {
            if scenario_name.contains("codex-acp") {
                "codex-acp"
            } else if scenario_name.contains("claude-acp") {
                "claude-agent-acp"
            } else {
                "grok"
            }
        }
    }
}

#[cfg(test)]
mod corpus_provider_name_tests {
    use super::*;

    /// Break caught: a scenario row named for one agent silently promoting
    /// under another agent's corpus directory, or under the bare "acp" name
    /// the enum's own doc comment says must not stand in for any of them.
    #[test]
    fn each_acp_scenario_maps_to_its_own_agent_directory() {
        assert_eq!(
            corpus_provider_name(Provider::Acp, "session-discovery-grok"),
            "grok"
        );
        assert_eq!(
            corpus_provider_name(Provider::Acp, "session-discovery-codex-acp"),
            "codex-acp"
        );
        assert_eq!(
            corpus_provider_name(Provider::Acp, "session-discovery-claude-acp"),
            "claude-agent-acp"
        );
        assert_eq!(corpus_provider_name(Provider::Acp, "run-grok"), "grok");
        assert_eq!(corpus_provider_name(Provider::Acp, "steer-grok"), "grok");
    }

    #[test]
    fn claude_and_codex_ignore_the_scenario_name() {
        assert_eq!(corpus_provider_name(Provider::Claude, "anything"), "claude");
        assert_eq!(corpus_provider_name(Provider::Codex, "anything"), "codex");
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    Stdin,
    Stdout,
    Stderr,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CaptureEvent {
    pub sequence: u64,
    pub channel: Channel,
    pub payload: String,
}

/// What a capture run needs, collected once by the caller (the binary's
/// argument parsing, or a test) and handed to [`record::record`] by name —
/// the scenario itself, looked up in `record::scenarios::SCENARIOS`, is what
/// decides which of these fields it actually reads.
///
/// Replaces the pre-Task-7 `CaptureScenario`/`CaptureOperation` pair: those
/// encoded "which scenario" as a Rust enum SHAPE the binary had to construct
/// by hand (one arm per scenario, `ClaudeRunScript`/`CodexRunScript` inside),
/// so the scenario's name, its `--help` text and its dispatch arm could drift
/// from each other — closing D60. Now the scenario's name IS the dispatch
/// key, and everything else here is raw, ungrouped input a scenario body
/// reads through `ScenarioInput` if it needs it.
#[derive(Clone, Debug)]
pub struct CaptureConfig {
    /// `"claude"` | `"codex"` — matches [`Provider`]'s wire name and
    /// `record::scenarios::scenario`'s own parameter.
    pub provider: &'static str,
    /// Must name a row in `record::scenarios::SCENARIOS` for the chosen
    /// `provider`; `record()` panics on a caller that violates this,
    /// matching the old code's `unreachable!("provider/scenario pair was
    /// validated")` — the binary validates this by construction (it only
    /// reaches `record()` after `scenario(provider, name)` resolved).
    pub scenario_name: &'static str,
    pub purpose: &'static str,
    pub executable: Option<PathBuf>,
    pub codex_home: Option<PathBuf>,
    /// `CLAUDE_CONFIG_DIR` for the Claude launch (D91). Claude Code reads its configuration
    /// home regardless of `--cwd`, so without this every capture carries the operator's skills,
    /// plugins, MCP servers, hooks and locally configured models.
    pub claude_config_dir: Option<PathBuf>,
    pub cwd: Option<PathBuf>,
    pub resume_id: Option<String>,
    pub attachment: Option<PathBuf>,
    pub approval_target: Option<PathBuf>,
    pub raw_root: PathBuf,
    pub timeout: Duration,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PlatformMetadata {
    pub os: String,
    pub arch: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RedactionRoots {
    pub cwd: Option<String>,
    pub repo: Option<String>,
    pub home: Option<String>,
    pub temp: Option<String>,
    #[serde(default)]
    pub codex_home: Option<String>,
    #[serde(default)]
    pub claude_config_dir: Option<String>,
    #[serde(default)]
    pub approval_target: Option<String>,
    #[serde(default)]
    pub trusted_powershell: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RawCapture {
    pub directory: PathBuf,
    pub provider: Provider,
    pub cli_version: String,
    pub captured_at_unix_ms: i64,
    pub scenario: String,
    pub purpose: String,
    pub platform: PlatformMetadata,
    pub redaction_roots: RedactionRoots,
    pub command: CommandSnapshot,
    pub events: Vec<CaptureEvent>,
    pub exit_code: Option<i32>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum PartialFailureClass {
    DriverError,
    Timeout,
    ProcessError,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum PartialOutcome {
    Incomplete,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct PartialRawCapture {
    pub(super) schema_version: u64,
    pub(super) outcome: PartialOutcome,
    pub(super) failure_class: PartialFailureClass,
    #[serde(flatten)]
    pub(super) capture: RawCapture,
}
