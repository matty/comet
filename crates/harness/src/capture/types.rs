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
