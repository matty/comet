use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use comet_proto::RunRequest;

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
    // The capture recorder is the only caller; see `recording.rs`.
    #[allow(dead_code)]
    pub(crate) fn from_launch(launch: &LaunchDescriptor) -> Self {
        const CAPTURED_ENV: &[&str] = &["CODEX_HOME"];

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

#[derive(Clone, Debug)]
pub enum ClaudeCaptureOperation {
    ModelDiscovery,
    ModelDiscoveryAt {
        cwd: PathBuf,
    },
    CommandDiscovery {
        cwd: PathBuf,
    },
    Run {
        request: RunRequest,
        script: ClaudeRunScript,
    },
}

#[derive(Clone, Copy, Debug)]
pub enum ClaudeRunScript {
    FreshText,
    Approval,
    Resume,
    Attachment,
    /// Drives the task-list tools (`TaskCreate` / `TaskUpdate`) so a capture
    /// carries real checklist mutations.
    Checklist,
    /// The same list, continued by a SECOND process resuming the first. This
    /// is the only way to observe what a resumed run is told about a list it
    /// did not create — which is nothing, so its updates arrive for ids the
    /// process has never seen. Separate from [`ClaudeRunScript::Resume`]
    /// because that one's prompt creates no tasks, and separate from
    /// [`ClaudeRunScript::Checklist`] because it additionally requires
    /// `--resume-id`.
    ChecklistResume,
}

#[derive(Clone, Debug)]
pub enum CodexCaptureOperation {
    ModelDiscovery,
    ModelDiscoveryAt {
        cwd: PathBuf,
    },
    Run {
        request: RunRequest,
        script: CodexRunScript,
    },
}

#[derive(Clone, Copy, Debug)]
pub enum CodexRunScript {
    FreshText,
    Approval,
    ApprovalOnRequest,
    Resume,
    Steer,
    Interruption,
}

#[derive(Clone, Debug)]
pub enum CaptureOperation {
    Claude(ClaudeCaptureOperation),
    Codex(CodexCaptureOperation),
}

#[derive(Clone, Debug)]
pub struct CaptureScenario {
    pub name: &'static str,
    pub purpose: &'static str,
    pub operation: CaptureOperation,
}

#[derive(Clone, Debug)]
pub struct CaptureConfig {
    pub scenario: CaptureScenario,
    pub executable: Option<PathBuf>,
    pub codex_home: Option<PathBuf>,
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
