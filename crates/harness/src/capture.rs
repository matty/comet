use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::mpsc;

use comet_proto::RunRequest;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum StdioMode {
    Inherit,
    Null,
    Piped,
}

impl StdioMode {
    fn materialize(self) -> Stdio {
        match self {
            Self::Inherit => Stdio::inherit(),
            Self::Null => Stdio::null(),
            Self::Piped => Stdio::piped(),
        }
    }
}

/// Every process-launch choice shared by production and capture.
#[derive(Clone, Debug)]
pub(crate) struct LaunchDescriptor {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: Option<PathBuf>,
    pub configured_env: BTreeMap<OsString, OsString>,
    pub stdin: StdioMode,
    pub stdout: StdioMode,
    pub stderr: StdioMode,
    pub kill_on_drop: bool,
    #[cfg(windows)]
    pub creation_flags: u32,
}

impl LaunchDescriptor {
    pub(crate) fn command(&self) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(&self.program);
        command
            .args(&self.args)
            .envs(&self.configured_env)
            .stdin(self.stdin.materialize())
            .stdout(self.stdout.materialize())
            .stderr(self.stderr.materialize())
            .kill_on_drop(self.kill_on_drop);
        if let Some(cwd) = &self.cwd {
            command.current_dir(cwd);
        }
        #[cfg(windows)]
        command.creation_flags(self.creation_flags);
        command
    }
}

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
    #[allow(dead_code)] // Task 2 capture drivers consume this API.
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
}

#[derive(Clone, Debug)]
pub enum CodexCaptureOperation {
    ModelDiscovery,
    Run {
        request: RunRequest,
        script: CodexRunScript,
    },
}

#[derive(Clone, Copy, Debug)]
pub enum CodexRunScript {
    FreshText,
    Approval,
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
    pub raw_root: PathBuf,
    pub timeout: Duration,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PlatformMetadata {
    pub os: String,
    pub arch: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RawCapture {
    pub directory: PathBuf,
    pub provider: Provider,
    pub cli_version: String,
    pub platform: PlatformMetadata,
    pub command: CommandSnapshot,
    pub events: Vec<CaptureEvent>,
    pub exit_code: Option<i32>,
}

const CLAUDE_INITIALIZE_LINE: &str = r#"{"type":"control_request","request_id":"comet-discovery-1","request":{"subtype":"initialize"}}"#;
const CODEX_INITIALIZED_LINE: &str = r#"{"jsonrpc":"2.0","method":"initialized"}"#;
const READER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);

/// Record one explicitly selected provider script into ignored raw storage.
pub async fn record(config: CaptureConfig) -> anyhow::Result<RawCapture> {
    RecordingSession::start(config).await?.finish().await
}

#[derive(Clone, Debug)]
pub struct SanitizationReport {
    pub events_path: PathBuf,
    pub manifest_path: PathBuf,
    pub events_bytes: Vec<u8>,
    pub manifest_bytes: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
pub enum SanitizationError {
    #[error("raw capture could not be read")]
    ReadRaw {
        #[source]
        source: std::io::Error,
    },
    #[error("raw capture is not valid capture JSON")]
    InvalidRaw {
        #[source]
        source: serde_json::Error,
    },
    #[error("staging output must be below .comet-provider-captures/staging")]
    UnsafeOutputDirectory,
    #[error("capture contains an unrecognized absolute path at {location}")]
    UnrecognizedAbsolutePath { location: String },
    #[error("capture contains a secret-like field at {location}")]
    SecretLikeField { location: String },
    #[error("capture contains a secret-like value at {location}")]
    SecretLikeValue { location: String },
    #[error("capture channel contains unparseable structured JSON at sequence {sequence}")]
    UnparseableStructuredPayload { sequence: u64 },
    #[error("sanitized capture could not be written")]
    WriteOutput {
        #[source]
        source: std::io::Error,
    },
    #[error("sanitized capture could not be encoded")]
    EncodeOutput {
        #[source]
        source: serde_json::Error,
    },
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum RedactionKind {
    ClaudeRequestId,
    CodexRpcId,
    SessionId,
    ThreadId,
    TurnId,
    ToolUseId,
    UserText,
    AssistantProse,
    AttachmentBytes,
}

impl RedactionKind {
    fn placeholder_name(self) -> &'static str {
        match self {
            Self::ClaudeRequestId => "CLAUDE_REQUEST_ID",
            Self::CodexRpcId => "CODEX_RPC_ID",
            Self::SessionId => "SESSION_ID",
            Self::ThreadId => "THREAD_ID",
            Self::TurnId => "TURN_ID",
            Self::ToolUseId => "TOOL_USE_ID",
            Self::UserText => "USER_TEXT",
            Self::AssistantProse => "ASSISTANT_PROSE",
            Self::AttachmentBytes => "ATTACHMENT_BYTES",
        }
    }

    fn manifest_name(self) -> &'static str {
        match self {
            Self::ClaudeRequestId => "claude_request_id",
            Self::CodexRpcId => "codex_rpc_id",
            Self::SessionId => "session_id",
            Self::ThreadId => "thread_id",
            Self::TurnId => "turn_id",
            Self::ToolUseId => "tool_use_id",
            Self::UserText => "user_text",
            Self::AssistantProse => "assistant_prose",
            Self::AttachmentBytes => "attachment_bytes",
        }
    }
}

#[derive(Clone, Debug)]
struct SemanticValue {
    original: Value,
    placeholder: String,
}

#[derive(Default)]
struct Redactor {
    semantics: BTreeMap<RedactionKind, Vec<SemanticValue>>,
    counts: BTreeMap<String, u64>,
    paths: Vec<PathRedaction>,
}

#[derive(Clone)]
struct PathRedaction {
    values: Vec<String>,
    placeholder: &'static str,
    kind: &'static str,
}

#[derive(Clone, Copy, Default)]
enum Speaker {
    #[default]
    Unknown,
    User,
    Assistant,
}

#[derive(Clone, Copy, Default)]
struct SemanticContext {
    speaker: Speaker,
    codex_turn_input: bool,
    codex_assistant_prose: bool,
}

/// Convert one raw capture into reviewable staging artifacts.
///
/// All transformation and validation happens in memory. A rejected capture never creates its
/// output directory, and errors identify only the value's structural location.
pub fn sanitize_dir(
    raw_dir: &Path,
    output_dir: &Path,
) -> Result<SanitizationReport, SanitizationError> {
    if !is_staging_output(output_dir) {
        return Err(SanitizationError::UnsafeOutputDirectory);
    }

    let bytes = std::fs::read(raw_dir.join("capture.json"))
        .map_err(|source| SanitizationError::ReadRaw { source })?;
    let capture: RawCapture = serde_json::from_slice(&bytes)
        .map_err(|source| SanitizationError::InvalidRaw { source })?;
    let mut redactor = Redactor::new(&capture);

    let mut payloads = Vec::with_capacity(capture.events.len());
    for event in &capture.events {
        match serde_json::from_str::<Value>(&event.payload) {
            Ok(payload) => payloads.push(Payload::Json(payload)),
            Err(_) if event.channel == Channel::Stderr => {
                payloads.push(Payload::Text(event.payload.clone()))
            }
            Err(_) => {
                return Err(SanitizationError::UnparseableStructuredPayload {
                    sequence: event.sequence,
                });
            }
        }
    }
    for payload in &payloads {
        if let Payload::Json(value) = payload {
            redactor.collect_semantics(value, SemanticContext::default());
        }
    }

    let mut command = serde_json::to_value(&capture.command)
        .map_err(|source| SanitizationError::EncodeOutput { source })?;
    redactor.sanitize_nonsemantic_value(&mut command, "command")?;

    let mut events_bytes = Vec::new();
    for (event, payload) in capture.events.iter().zip(&mut payloads) {
        let payload = match payload {
            Payload::Json(value) => {
                redactor.sanitize_json(value, SemanticContext::default(), "event.payload")?;
                serde_json::to_string(value)
                    .map_err(|source| SanitizationError::EncodeOutput { source })?
            }
            Payload::Text(text) => {
                redactor.sanitize_string(text, "event.payload")?;
                text.clone()
            }
        };
        let line = SanitizedEvent {
            sequence: event.sequence,
            channel: event.channel,
            payload,
        };
        serde_json::to_writer(&mut events_bytes, &line)
            .map_err(|source| SanitizationError::EncodeOutput { source })?;
        events_bytes.push(b'\n');
    }

    let mut cli_version = capture.cli_version.clone();
    redactor.sanitize_paths_and_validate(&mut cli_version, "cli_version")?;
    let mut normalized_cli_version = capture.cli_version.trim().to_owned();
    redactor.sanitize_paths_and_validate(&mut normalized_cli_version, "normalized_cli_version")?;
    let mut platform = serde_json::to_value(&capture.platform)
        .map_err(|source| SanitizationError::EncodeOutput { source })?;
    redactor.sanitize_nonsemantic_value(&mut platform, "platform")?;
    let channels: Vec<Channel> = capture.events.iter().fold(Vec::new(), |mut seen, event| {
        if !seen.contains(&event.channel) {
            seen.push(event.channel);
        }
        seen
    });
    let mut scenario = raw_dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("capture")
        .to_owned();
    redactor.sanitize_paths_and_validate(&mut scenario, "scenario")?;
    let manifest = json!({
        "schema_version": 1,
        "provider": capture.provider,
        "cli_version": cli_version,
        "normalized_cli_version": normalized_cli_version,
        "platform": platform,
        "scenario": scenario,
        "command": command,
        "channels": channels,
        "exit_code": capture.exit_code,
        "placeholders": redactor.placeholder_definitions(),
        "redaction_counts": redactor.counts,
    });
    let mut manifest_bytes = serde_json::to_vec_pretty(&manifest)
        .map_err(|source| SanitizationError::EncodeOutput { source })?;
    manifest_bytes.push(b'\n');

    std::fs::create_dir_all(output_dir)
        .map_err(|source| SanitizationError::WriteOutput { source })?;
    let events_path = output_dir.join("events.jsonl");
    let manifest_path = output_dir.join("manifest.json");
    std::fs::write(&events_path, &events_bytes)
        .map_err(|source| SanitizationError::WriteOutput { source })?;
    std::fs::write(&manifest_path, &manifest_bytes)
        .map_err(|source| SanitizationError::WriteOutput { source })?;

    Ok(SanitizationReport {
        events_path,
        manifest_path,
        events_bytes,
        manifest_bytes,
    })
}

#[derive(Serialize)]
struct SanitizedEvent {
    sequence: u64,
    channel: Channel,
    payload: String,
}

enum Payload {
    Json(Value),
    Text(String),
}

impl Redactor {
    fn new(capture: &RawCapture) -> Self {
        let mut redactor = Self::default();
        redactor.add_path(capture.command.cwd.as_deref(), "<CWD>", "cwd_path");
        if let Some(cwd) = capture.command.cwd.as_deref().map(Path::new)
            && let Some(repo) = repository_root(cwd)
        {
            redactor.add_path(repo.to_str(), "<REPO>", "repo_path");
        }
        redactor.add_path(
            crate::home_dir().as_deref().and_then(Path::to_str),
            "<HOME>",
            "home_path",
        );
        let temp_dir = std::env::temp_dir();
        redactor.add_path(temp_dir.to_str(), "<TEMP>", "temp_path");
        redactor
    }

    fn add_path(&mut self, value: Option<&str>, placeholder: &'static str, kind: &'static str) {
        let Some(value) = value.filter(|value| !value.is_empty()) else {
            return;
        };
        if self
            .paths
            .iter()
            .any(|redaction| redaction.values.iter().any(|known| known == value))
        {
            return;
        }
        let mut values = vec![value.to_owned()];
        let slash = value.replace('\\', "/");
        let backslash = value.replace('/', "\\");
        for variant in [slash, backslash] {
            if !values.contains(&variant) {
                values.push(variant);
            }
        }
        for variant in values.clone() {
            if is_drive_absolute(&variant) {
                values.push(format!(r"\\?\{variant}"));
                values.push(format!("//?/{variant}"));
            }
        }
        values.sort_by_key(|value| std::cmp::Reverse(value.len()));
        self.paths.push(PathRedaction {
            values,
            placeholder,
            kind,
        });
        self.paths
            .sort_by_key(|path| std::cmp::Reverse(path.values.first().map_or(0, String::len)));
    }

    fn collect_semantics(&mut self, value: &Value, context: SemanticContext) {
        match value {
            Value::Array(values) => {
                for value in values {
                    self.collect_semantics(value, context);
                }
            }
            Value::Object(object) => {
                let context = object_context(object, context);
                for (key, value) in object {
                    if let Some(kind) = semantic_kind(object, key, value, context) {
                        self.register(kind, value);
                    } else {
                        self.collect_semantics(value, context);
                    }
                }
            }
            _ => {}
        }
    }

    fn register(&mut self, kind: RedactionKind, original: &Value) {
        if !matches!(original, Value::String(_) | Value::Number(_)) {
            return;
        }
        let values = self.semantics.entry(kind).or_default();
        if values.iter().any(|value| value.original == *original) {
            return;
        }
        values.push(SemanticValue {
            original: original.clone(),
            placeholder: format!("<{}_{}>", kind.placeholder_name(), values.len() + 1),
        });
    }

    fn sanitize_json(
        &mut self,
        value: &mut Value,
        context: SemanticContext,
        location: &str,
    ) -> Result<(), SanitizationError> {
        match value {
            Value::Array(values) => {
                for (index, value) in values.iter_mut().enumerate() {
                    self.sanitize_json(value, context, &format!("{location}[{index}]"))?;
                }
            }
            Value::Object(object) => {
                let context = object_context(object, context);
                let keys: Vec<String> = object.keys().cloned().collect();
                for key in keys {
                    let child_location = format!("{location}.{key}");
                    if is_secret_field(&key) {
                        return Err(SanitizationError::SecretLikeField {
                            location: child_location,
                        });
                    }
                    let kind = semantic_kind(object, &key, &object[&key], context);
                    let child = object.get_mut(&key).expect("key came from object");
                    if let Some(kind) = kind {
                        self.replace_semantic(kind, child);
                    } else if is_protocol_discriminator(&key) {
                        if let Value::String(text) = child {
                            self.sanitize_paths_and_validate(text, &child_location)?;
                        }
                    } else {
                        self.sanitize_json(child, context, &child_location)?;
                    }
                }
            }
            Value::String(text) => self.sanitize_string(text, location)?,
            _ => {}
        }
        Ok(())
    }

    fn sanitize_nonsemantic_value(
        &mut self,
        value: &mut Value,
        location: &str,
    ) -> Result<(), SanitizationError> {
        match value {
            Value::Array(values) => {
                for (index, value) in values.iter_mut().enumerate() {
                    self.sanitize_nonsemantic_value(value, &format!("{location}[{index}]"))?;
                }
            }
            Value::Object(object) => {
                for (key, value) in object {
                    let child_location = format!("{location}.{key}");
                    if is_secret_field(key) {
                        return Err(SanitizationError::SecretLikeField {
                            location: child_location,
                        });
                    }
                    self.sanitize_nonsemantic_value(value, &child_location)?;
                }
            }
            Value::String(text) => self.sanitize_paths_and_validate(text, location)?,
            _ => {}
        }
        Ok(())
    }

    fn replace_semantic(&mut self, kind: RedactionKind, value: &mut Value) {
        let Some(replacement) = self
            .semantics
            .get(&kind)
            .and_then(|values| values.iter().find(|known| known.original == *value))
            .map(|known| known.placeholder.clone())
        else {
            return;
        };
        *value = Value::String(replacement);
        *self
            .counts
            .entry(kind.manifest_name().to_owned())
            .or_default() += 1;
    }

    fn sanitize_string(
        &mut self,
        text: &mut String,
        location: &str,
    ) -> Result<(), SanitizationError> {
        let replacements: Vec<(RedactionKind, String, String)> = self
            .semantics
            .iter()
            .flat_map(|(kind, values)| {
                values.iter().filter_map(move |value| {
                    value
                        .original
                        .as_str()
                        .map(|original| (*kind, original.to_owned(), value.placeholder.clone()))
                })
            })
            .filter(|(_, original, _)| !original.is_empty())
            .collect();
        for (kind, original, placeholder) in replacements {
            let occurrences = text.matches(&original).count() as u64;
            if occurrences != 0 {
                *text = text.replace(&original, &placeholder);
                *self
                    .counts
                    .entry(kind.manifest_name().to_owned())
                    .or_default() += occurrences;
            }
        }
        self.sanitize_paths_and_validate(text, location)
    }

    fn sanitize_paths_and_validate(
        &mut self,
        text: &mut String,
        location: &str,
    ) -> Result<(), SanitizationError> {
        for path in self.paths.clone() {
            let mut occurrences = 0;
            for value in path.values {
                let found = replace_path_occurrences(text, &value, path.placeholder);
                if found != 0 {
                    occurrences += found;
                }
            }
            if occurrences != 0 {
                *self.counts.entry(path.kind.to_owned()).or_default() += occurrences;
            }
        }
        if contains_absolute_path(text) {
            return Err(SanitizationError::UnrecognizedAbsolutePath {
                location: location.to_owned(),
            });
        }
        if contains_secret_value(text) {
            return Err(SanitizationError::SecretLikeValue {
                location: location.to_owned(),
            });
        }
        Ok(())
    }

    fn placeholder_definitions(&self) -> Vec<Value> {
        let mut definitions = Vec::new();
        for (kind, values) in &self.semantics {
            for value in values {
                definitions.push(json!({
                    "placeholder": value.placeholder,
                    "kind": kind.manifest_name(),
                }));
            }
        }
        for path in &self.paths {
            if self.counts.contains_key(path.kind) {
                definitions.push(json!({
                    "placeholder": path.placeholder,
                    "kind": path.kind,
                }));
            }
        }
        definitions
    }
}

fn object_context(
    object: &serde_json::Map<String, Value>,
    mut context: SemanticContext,
) -> SemanticContext {
    let marker = object
        .get("role")
        .or_else(|| object.get("type"))
        .and_then(Value::as_str);
    context.speaker = match marker {
        Some("user") => Speaker::User,
        Some("assistant") => Speaker::Assistant,
        _ => context.speaker,
    };
    if object.get("method").and_then(Value::as_str) == Some("turn/start") {
        context.codex_turn_input = true;
    }
    if object
        .get("method")
        .and_then(Value::as_str)
        .is_some_and(|method| {
            method.starts_with("item/agentMessage/")
                || method.starts_with("item/reasoning/")
                || method.starts_with("item/plan/")
        })
    {
        context.codex_assistant_prose = true;
    }
    context.speaker = match marker {
        Some("userMessage" | "user_message") => Speaker::User,
        Some("agentMessage" | "agent_message" | "reasoning") => Speaker::Assistant,
        _ => context.speaker,
    };
    context
}

fn semantic_kind(
    object: &serde_json::Map<String, Value>,
    key: &str,
    value: &Value,
    context: SemanticContext,
) -> Option<RedactionKind> {
    if !matches!(value, Value::String(_) | Value::Number(_)) {
        return None;
    }
    let normalized = normalize_field(key);
    match normalized.as_str() {
        "requestid" => return Some(RedactionKind::ClaudeRequestId),
        "sessionid" => return Some(RedactionKind::SessionId),
        "threadid" => return Some(RedactionKind::ThreadId),
        "turnid" => return Some(RedactionKind::TurnId),
        "tooluseid" | "parenttooluseid" | "itemid" => {
            return Some(RedactionKind::ToolUseId);
        }
        "id" if object.contains_key("jsonrpc") => return Some(RedactionKind::CodexRpcId),
        "id" if object.get("type").and_then(Value::as_str) == Some("tool_use") => {
            return Some(RedactionKind::ToolUseId);
        }
        "id" if object
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(is_tool_item_type) =>
        {
            return Some(RedactionKind::ToolUseId);
        }
        "data" if object.get("type").and_then(Value::as_str) == Some("base64") => {
            return Some(RedactionKind::AttachmentBytes);
        }
        "prompt" => return Some(RedactionKind::UserText),
        "content" | "text" if matches!(context.speaker, Speaker::User) => {
            return Some(RedactionKind::UserText);
        }
        "content" | "text" if matches!(context.speaker, Speaker::Assistant) => {
            return Some(RedactionKind::AssistantProse);
        }
        "text"
            if context.codex_turn_input
                && object.get("type").and_then(Value::as_str) == Some("text") =>
        {
            return Some(RedactionKind::UserText);
        }
        "text" if object.get("type").and_then(Value::as_str) == Some("text_delta") => {
            return Some(RedactionKind::AssistantProse);
        }
        "delta" | "textdelta" if context.codex_assistant_prose => {
            return Some(RedactionKind::AssistantProse);
        }
        "thinking" => return Some(RedactionKind::AssistantProse),
        "result" if object.get("type").and_then(Value::as_str) == Some("result") => {
            return Some(RedactionKind::AssistantProse);
        }
        _ => {}
    }
    None
}

fn is_tool_item_type(item_type: &str) -> bool {
    matches!(
        item_type,
        "commandExecution"
            | "command_execution"
            | "fileChange"
            | "file_change"
            | "mcpToolCall"
            | "mcp_tool_call"
            | "webSearch"
            | "web_search"
            | "dynamicToolCall"
            | "dynamic_tool_call"
    )
}

fn normalize_field(field: &str) -> String {
    field
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn is_protocol_discriminator(field: &str) -> bool {
    matches!(
        normalize_field(field).as_str(),
        "type"
            | "subtype"
            | "method"
            | "role"
            | "jsonrpc"
            | "name"
            | "status"
            | "kind"
            | "channel"
            | "level"
    )
}

fn is_secret_field(field: &str) -> bool {
    matches!(
        normalize_field(field).as_str(),
        "authorization"
            | "proxyauthorization"
            | "apikey"
            | "openaiapikey"
            | "anthropicapikey"
            | "accesstoken"
            | "authtoken"
            | "credential"
            | "credentials"
            | "password"
            | "secret"
    )
}

fn contains_secret_value(value: &str) -> bool {
    let uppercase = value.to_ascii_uppercase();
    if uppercase.contains("-----BEGIN ") && uppercase.contains(" PRIVATE KEY-----") {
        return true;
    }
    ["sk-ant-", "sk-proj-", "xoxb-", "ghp_", "github_pat_"]
        .iter()
        .any(|prefix| value.contains(prefix))
        || value
            .match_indices("sk-")
            .any(|(index, _)| index == 0 || !value.as_bytes()[index - 1].is_ascii_alphanumeric())
}

fn replace_path_occurrences(text: &mut String, path: &str, placeholder: &str) -> u64 {
    let mut count = 0;
    let mut cursor = 0;
    while let Some(relative) = text[cursor..].find(path) {
        let start = cursor + relative;
        let end = start + path.len();
        let ends_at_path_boundary = end == text.len()
            || text[end..]
                .chars()
                .next()
                .is_some_and(|character| matches!(character, '/' | '\\'));
        if ends_at_path_boundary {
            text.replace_range(start..end, placeholder);
            cursor = start + placeholder.len();
            count += 1;
        } else {
            cursor = end;
        }
    }
    count
}

fn contains_absolute_path(value: &str) -> bool {
    let bytes = value.as_bytes();
    for index in 0..bytes.len() {
        if index + 2 < bytes.len()
            && bytes[index].is_ascii_alphabetic()
            && bytes[index + 1] == b':'
            && matches!(bytes[index + 2], b'/' | b'\\')
            && (index == 0 || !bytes[index - 1].is_ascii_alphanumeric())
        {
            return true;
        }
        if index + 1 < bytes.len() && bytes[index] == b'\\' && bytes[index + 1] == b'\\' {
            return true;
        }
        if bytes[index] == b'/'
            && bytes.get(index + 1).is_some_and(|next| {
                !next.is_ascii_whitespace() && !matches!(*next, b'/' | b'>' | b'}' | b']')
            })
            && (index == 0
                || (!bytes[index - 1].is_ascii_alphanumeric()
                    && !matches!(bytes[index - 1], b':' | b'/' | b'>')))
        {
            return true;
        }
    }
    false
}

fn is_drive_absolute(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'/' | b'\\')
}

fn repository_root(start: &Path) -> Option<&Path> {
    start
        .ancestors()
        .find(|ancestor| ancestor.join(".git").exists())
}

fn is_staging_output(output: &Path) -> bool {
    if output
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return false;
    }
    let components: Vec<_> = output
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .collect();
    components.windows(2).any(|pair| {
        pair[0] == ".comet-provider-captures" && pair[1].eq_ignore_ascii_case("staging")
    })
}

/// A live capture owns its child until a terminal frame or hard timeout.
///
/// The type remains private to the module; tests reach it only to retain the
/// spawned pid while exercising the same `finish` path as [`record`].
struct RecordingSession {
    provider: Provider,
    operation: CaptureOperation,
    timeout: Duration,
    directory: PathBuf,
    cli_version: String,
    command: CommandSnapshot,
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    stdout_lines: mpsc::UnboundedReceiver<String>,
    readers: Vec<tokio::task::JoinHandle<()>>,
    events: Arc<Mutex<Vec<CaptureEvent>>>,
    #[cfg(test)]
    reap_notice: Option<std::sync::mpsc::SyncSender<u32>>,
}

impl RecordingSession {
    async fn start(mut config: CaptureConfig) -> anyhow::Result<Self> {
        if let CaptureOperation::Codex(CodexCaptureOperation::Run { request, .. }) =
            &mut config.scenario.operation
        {
            *request = crate::codex::normalize_run_request(request.clone());
        }
        let provider = match &config.scenario.operation {
            CaptureOperation::Claude(_) => Provider::Claude,
            CaptureOperation::Codex(_) => Provider::Codex,
        };
        let executable = resolve_executable(provider, config.executable.as_ref())?;
        let launch = select_launch(&config, &executable)?;
        let command = CommandSnapshot::from_launch(&launch);
        let cli_version = probe_version(&executable).await;
        let directory = config.raw_root.join(format!(
            "{}-{}-{}",
            provider_name(provider),
            config.scenario.name,
            uuid::Uuid::new_v4()
        ));
        tokio::fs::create_dir_all(&directory).await.map_err(|err| {
            tracing::debug!(path = %directory.display(), %err, "capture raw directory creation failed");
            anyhow!(
                "Raw capture storage could not be created. Check --raw-root permissions and try again."
            )
        })?;

        let mut child = launch.command().spawn().map_err(|err| {
            tracing::debug!(provider = provider_name(provider), cli = %executable.display(), %err, "capture provider spawn failed");
            anyhow!(
                "The {} CLI could not be started. Check --executable and try again.",
                provider_name(provider)
            )
        })?;
        let stdin = child.stdin.take().ok_or_else(|| {
            anyhow!("The provider did not open its input channel. Update the CLI and try again.")
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            anyhow!("The provider did not open its output channel. Update the CLI and try again.")
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            anyhow!("The provider did not open its error channel. Update the CLI and try again.")
        })?;

        let events = Arc::new(Mutex::new(Vec::new()));
        let (stdout_tx, stdout_lines) = mpsc::unbounded_channel();
        let stdout_events = Arc::clone(&events);
        let stdout_reader = tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        push_event(&stdout_events, Channel::Stdout, line.clone());
                        if stdout_tx.send(line).is_err() {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(err) => {
                        tracing::debug!(%err, "capture stdout reader stopped");
                        break;
                    }
                }
            }
        });
        let stderr_events = Arc::clone(&events);
        let stderr_reader = tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => push_event(&stderr_events, Channel::Stderr, line),
                    Ok(None) => break,
                    Err(err) => {
                        tracing::debug!(%err, "capture stderr reader stopped");
                        break;
                    }
                }
            }
        });

        Ok(Self {
            provider,
            operation: config.scenario.operation,
            timeout: config.timeout,
            directory,
            cli_version,
            command,
            child: Some(child),
            stdin: Some(stdin),
            stdout_lines,
            readers: vec![stdout_reader, stderr_reader],
            events,
            #[cfg(test)]
            reap_notice: None,
        })
    }

    #[cfg(test)]
    fn child_id(&self) -> Option<u32> {
        self.child.as_ref().and_then(Child::id)
    }

    async fn finish(&mut self) -> anyhow::Result<RawCapture> {
        let operation = self.operation.clone();
        let outcome = tokio::time::timeout(self.timeout, async {
            self.drive(operation).await?;
            self.stdin.take();
            self.wait_for_exit().await
        })
        .await;
        let exit_code = match outcome {
            Ok(Ok(exit_code)) => exit_code,
            Ok(Err(err)) => {
                self.terminate_and_reap().await;
                return Err(err);
            }
            Err(_) => {
                self.terminate_and_reap().await;
                bail!(
                    "Capture timed out after {} seconds. The provider was stopped; retry with --timeout-seconds up to 300.",
                    self.timeout.as_secs_f64()
                );
            }
        };
        self.finish_readers().await;
        let capture = RawCapture {
            directory: self.directory.clone(),
            provider: self.provider,
            cli_version: self.cli_version.clone(),
            platform: PlatformMetadata {
                os: std::env::consts::OS.into(),
                arch: std::env::consts::ARCH.into(),
            },
            command: self.command.clone(),
            events: self.events.lock().expect("capture event lock").clone(),
            exit_code,
        };
        persist_raw_capture(&capture).await?;
        Ok(capture)
    }

    async fn drive(&mut self, operation: CaptureOperation) -> anyhow::Result<()> {
        match operation {
            CaptureOperation::Claude(ClaudeCaptureOperation::ModelDiscovery)
            | CaptureOperation::Claude(ClaudeCaptureOperation::CommandDiscovery { .. }) => {
                self.claude_initialize().await
            }
            CaptureOperation::Claude(ClaudeCaptureOperation::Run { request, script }) => {
                self.claude_run(request, script).await
            }
            CaptureOperation::Codex(CodexCaptureOperation::ModelDiscovery) => {
                self.codex_model_discovery().await
            }
            CaptureOperation::Codex(CodexCaptureOperation::Run { request, script }) => {
                self.codex_run(request, script).await
            }
        }
    }

    async fn claude_initialize(&mut self) -> anyhow::Result<()> {
        self.write_line(CLAUDE_INITIALIZE_LINE).await?;
        while let Some(line) = self.next_stdout().await? {
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if value["type"] == "control_response" {
                return Ok(());
            }
        }
        protocol_stopped("Claude", "initialize reply")
    }

    async fn claude_run(
        &mut self,
        request: RunRequest,
        script: ClaudeRunScript,
    ) -> anyhow::Result<()> {
        let line = claude_user_line(&request, script).await?;
        self.write_line(&line).await?;
        while let Some(line) = self.next_stdout().await? {
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if value["type"] == "control_request" && matches!(script, ClaudeRunScript::Approval) {
                let request_id = value["request_id"]
                    .as_str()
                    .or_else(|| value["response"]["request_id"].as_str())
                    .unwrap_or_default();
                let response = json!({
                    "type": "control_response",
                    "response": {
                        "subtype": "success",
                        "request_id": request_id,
                        "response": { "behavior": "allow" },
                    },
                });
                self.write_line(&response.to_string()).await?;
            }
            if value["type"] == "result" {
                return Ok(());
            }
        }
        protocol_stopped("Claude", "terminal result")
    }

    async fn codex_model_discovery(&mut self) -> anyhow::Result<()> {
        self.write_line(&codex_initialize_line()).await?;
        self.codex_reply(1).await?;
        self.write_line(CODEX_INITIALIZED_LINE).await?;

        let mut cursor: Option<String> = None;
        for page in 0..20_u64 {
            let id = page + 2;
            self.write_line(&codex_model_list_line(id, cursor.as_deref()))
                .await?;
            let reply = self.codex_reply(id).await?;
            cursor = reply["result"]["nextCursor"].as_str().map(str::to_owned);
            if cursor.is_none() {
                return Ok(());
            }
        }
        bail!("Codex returned too many model pages. Update the CLI or retry the capture later.")
    }

    async fn codex_run(
        &mut self,
        request: RunRequest,
        script: CodexRunScript,
    ) -> anyhow::Result<()> {
        let mut next_id = 1_u64;
        self.write_line(&codex_initialize_line()).await?;
        self.codex_reply(next_id).await?;
        next_id += 1;
        self.write_line(CODEX_INITIALIZED_LINE).await?;

        let (method, thread_params) = if matches!(script, CodexRunScript::Resume) {
            (
                "thread/resume",
                crate::codex::thread_resume_params(
                    &request,
                    request.resume.as_deref().unwrap_or_default(),
                ),
            )
        } else {
            ("thread/start", crate::codex::thread_start_params(&request))
        };
        self.write_line(&rpc_request(next_id, method, thread_params))
            .await?;
        let mut thread_reply = self.codex_reply(next_id).await?;
        next_id += 1;
        if thread_reply.get("error").is_some() && method == "thread/resume" {
            let params = crate::codex::thread_start_params(&request);
            self.write_line(&rpc_request(next_id, "thread/start", params))
                .await?;
            thread_reply = self.codex_reply(next_id).await?;
            next_id += 1;
        }
        let thread_id = thread_reply["result"]["thread"]["id"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        if thread_id.is_empty() {
            return protocol_stopped("Codex", "thread identifier");
        }
        self.write_line(&rpc_request(
            next_id,
            "turn/start",
            crate::codex::turn_start_params(&request, &thread_id, &request.prompt),
        ))
        .await?;
        next_id += 1;

        let mut active_turn = None;
        let mut scripted_action_sent = false;
        while let Some(line) = self.next_stdout().await? {
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let method = value["method"].as_str().unwrap_or_default();
            if method == "turn/started" {
                active_turn = value["params"]["turn"]["id"].as_str().map(str::to_owned);
            }
            if !scripted_action_sent {
                match script {
                    CodexRunScript::Steer if active_turn.is_some() => {
                        self.write_line(&rpc_request(
                            next_id,
                            "turn/steer",
                            json!({
                                "threadId": thread_id,
                                "expectedTurnId": active_turn,
                                "input": [{"type": "text", "text": "Capture steering message."}],
                            }),
                        ))
                        .await?;
                        next_id += 1;
                        scripted_action_sent = true;
                    }
                    CodexRunScript::Interruption if active_turn.is_some() => {
                        self.write_line(&rpc_request(
                            next_id,
                            "turn/interrupt",
                            json!({"threadId": thread_id, "turnId": active_turn}),
                        ))
                        .await?;
                        next_id += 1;
                        scripted_action_sent = true;
                    }
                    _ => {}
                }
            }
            if matches!(script, CodexRunScript::Approval)
                && value.get("id").is_some()
                && method.ends_with("/requestApproval")
            {
                self.write_line(
                    &json!({
                        "jsonrpc": "2.0",
                        "id": value["id"],
                        "result": {"decision": "accept"},
                    })
                    .to_string(),
                )
                .await?;
            }
            if matches!(method, "turn/completed" | "turn/failed" | "turn/aborted") {
                return Ok(());
            }
        }
        protocol_stopped("Codex", "terminal turn notification")
    }

    async fn codex_reply(&mut self, id: u64) -> anyhow::Result<Value> {
        while let Some(line) = self.next_stdout().await? {
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if value["id"].as_u64() == Some(id) {
                return Ok(value);
            }
        }
        protocol_stopped("Codex", "JSON-RPC reply")
    }

    async fn write_line(&mut self, line: &str) -> anyhow::Result<()> {
        let Some(stdin) = self.stdin.as_mut() else {
            return protocol_stopped(provider_name(self.provider), "stdin channel");
        };
        push_event(&self.events, Channel::Stdin, line.to_owned());
        stdin.write_all(line.as_bytes()).await.map_err(|err| {
            tracing::debug!(provider = provider_name(self.provider), %err, "capture stdin write failed");
            anyhow!(
                "The provider stopped accepting capture input. Retry with a current CLI version."
            )
        })?;
        stdin.write_all(b"\n").await.map_err(|err| {
            tracing::debug!(provider = provider_name(self.provider), %err, "capture stdin newline write failed");
            anyhow!(
                "The provider stopped accepting capture input. Retry with a current CLI version."
            )
        })?;
        stdin.flush().await.map_err(|err| {
            tracing::debug!(provider = provider_name(self.provider), %err, "capture stdin flush failed");
            anyhow!(
                "The provider stopped accepting capture input. Retry with a current CLI version."
            )
        })
    }

    async fn next_stdout(&mut self) -> anyhow::Result<Option<String>> {
        Ok(self.stdout_lines.recv().await)
    }

    async fn wait_for_exit(&mut self) -> anyhow::Result<Option<i32>> {
        let Some(child) = self.child.as_mut() else {
            return Ok(None);
        };
        match tokio::time::timeout(CLEANUP_TIMEOUT, child.wait()).await {
            Ok(Ok(status)) => {
                self.child.take();
                Ok(status.code())
            }
            Ok(Err(err)) => {
                tracing::debug!(provider = provider_name(self.provider), %err, "capture child wait failed");
                self.child.take();
                bail!(
                    "The provider ended but its exit status could not be read. Retry the capture."
                )
            }
            Err(_) => {
                self.terminate_and_reap().await;
                bail!(
                    "The provider did not exit after its final response. It was stopped; retry the capture."
                )
            }
        }
    }

    async fn terminate_and_reap(&mut self) {
        self.stdin.take();
        let Some(mut child) = self.child.take() else {
            return;
        };
        if let Err(err) = child.start_kill() {
            tracing::debug!(provider = provider_name(self.provider), %err, "capture child kill failed");
        }
        match tokio::time::timeout(CLEANUP_TIMEOUT, child.wait()).await {
            Ok(Ok(_)) => {}
            Ok(Err(err)) => {
                tracing::debug!(provider = provider_name(self.provider), %err, "capture child reap failed");
            }
            Err(_) => {
                tracing::warn!(
                    provider = provider_name(self.provider),
                    "capture child reap timed out"
                );
            }
        }
        self.finish_readers().await;
    }

    async fn finish_readers(&mut self) {
        for mut reader in self.readers.drain(..) {
            if tokio::time::timeout(READER_SHUTDOWN_TIMEOUT, &mut reader)
                .await
                .is_err()
            {
                reader.abort();
                let _ = reader.await;
            }
        }
    }
}

impl Drop for RecordingSession {
    fn drop(&mut self) {
        self.stdin.take();
        let Some(mut child) = self.child.take() else {
            return;
        };
        let _ = child.start_kill();
        #[cfg(test)]
        let pid = child.id();
        #[cfg(test)]
        let notice = self.reap_notice.take();
        let spawn = std::thread::Builder::new()
            .name("comet-capture-reaper".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build();
                let reaped = runtime.is_ok_and(|runtime| {
                    runtime.block_on(async {
                        matches!(
                            tokio::time::timeout(CLEANUP_TIMEOUT, child.wait()).await,
                            Ok(Ok(_))
                        )
                    })
                });
                #[cfg(test)]
                if reaped && let (Some(pid), Some(notice)) = (pid, notice) {
                    let _ = notice.send(pid);
                }
                #[cfg(not(test))]
                let _ = reaped;
            });
        if let Err(err) = spawn {
            tracing::warn!(%err, "capture drop reaper thread could not start");
        }
    }
}

fn select_launch(
    config: &CaptureConfig,
    executable: &std::path::Path,
) -> anyhow::Result<LaunchDescriptor> {
    match &config.scenario.operation {
        CaptureOperation::Claude(ClaudeCaptureOperation::ModelDiscovery) => Ok(
            crate::claude::discovery::model_discovery_launch(executable, &std::env::temp_dir()),
        ),
        CaptureOperation::Claude(ClaudeCaptureOperation::CommandDiscovery { cwd }) => Ok(
            crate::claude::commands::command_discovery_launch(executable, cwd),
        ),
        CaptureOperation::Claude(ClaudeCaptureOperation::Run { request, .. }) => {
            Ok(crate::claude::run_launch(executable, request))
        }
        CaptureOperation::Codex(CodexCaptureOperation::ModelDiscovery) => {
            let home = config
                .codex_home
                .clone()
                .or_else(crate::codex::discovery::codex_home)
                .ok_or_else(|| {
                    anyhow!("Codex home could not be found. Pass --codex-home and try again.")
                })?;
            let home = absolute_from_parent(home)?;
            Ok(crate::codex::discovery::discovery_launch(
                executable,
                &home,
                &std::env::temp_dir(),
            ))
        }
        CaptureOperation::Codex(CodexCaptureOperation::Run { request, .. }) => {
            Ok(crate::codex::run_launch(executable, request))
        }
    }
}

fn resolve_executable(provider: Provider, configured: Option<&PathBuf>) -> anyhow::Result<PathBuf> {
    configured
        .cloned()
        .or_else(|| match provider {
            Provider::Claude => crate::claude::resolve_claude_executable(),
            Provider::Codex => crate::codex::resolve_codex_executable(),
        })
        .ok_or_else(|| {
            anyhow!(
                "The {} CLI was not found. Install it or pass --executable with its path.",
                provider_name(provider)
            )
        })
}

fn absolute_from_parent(path: PathBuf) -> anyhow::Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path);
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(path))
        .map_err(|err| {
            tracing::debug!(%err, "capture could not resolve a relative Codex home");
            anyhow!("Codex home could not be resolved. Pass an absolute --codex-home path.")
        })
}

fn push_event(events: &Arc<Mutex<Vec<CaptureEvent>>>, channel: Channel, payload: String) {
    let mut events = events.lock().expect("capture event lock");
    // Sequence is the recorder's observer order. Concurrent stdout/stderr
    // reads cannot recover byte-level ordering inside the kernel's two pipes.
    let sequence = events.len() as u64 + 1;
    events.push(CaptureEvent {
        sequence,
        channel,
        payload,
    });
}

fn provider_name(provider: Provider) -> &'static str {
    match provider {
        Provider::Claude => "Claude",
        Provider::Codex => "Codex",
    }
}

fn protocol_stopped<T>(provider: &str, expected: &str) -> anyhow::Result<T> {
    tracing::debug!(
        provider,
        expected,
        "capture protocol ended before expected response"
    );
    bail!("{provider} stopped before the expected {expected}. Retry with a current CLI version.")
}

async fn probe_version(executable: &std::path::Path) -> String {
    let mut command = tokio::process::Command::new(executable);
    command
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let Ok(mut child) = command.spawn() else {
        return "unknown".into();
    };
    let stdout = child.stdout.take();
    let status = tokio::time::timeout(CLEANUP_TIMEOUT, child.wait()).await;
    if status.is_err() {
        let _ = child.start_kill();
        let _ = tokio::time::timeout(CLEANUP_TIMEOUT, child.wait()).await;
        return "unknown".into();
    }
    let Some(stdout) = stdout else {
        return "unknown".into();
    };
    let mut lines = BufReader::new(stdout).lines();
    match tokio::time::timeout(Duration::from_secs(1), lines.next_line()).await {
        Ok(Ok(Some(line))) if !line.trim().is_empty() => line.trim().to_owned(),
        _ => "unknown".into(),
    }
}

async fn persist_raw_capture(capture: &RawCapture) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec_pretty(capture).map_err(|err| {
        tracing::debug!(%err, "raw capture serialization failed");
        anyhow!(
            "Raw evidence could not be prepared. Retry the capture with the current app version."
        )
    })?;
    let path = capture.directory.join("capture.json");
    tokio::fs::write(&path, bytes).await.map_err(|err| {
        tracing::debug!(path = %path.display(), %err, "raw capture write failed");
        anyhow!(
            "Capture finished but raw evidence could not be written. Check --raw-root permissions and retry."
        )
    })
}

async fn claude_user_line(request: &RunRequest, script: ClaudeRunScript) -> anyhow::Result<String> {
    if !matches!(script, ClaudeRunScript::Attachment) || request.attachments.is_empty() {
        return Ok(json!({
            "type": "user",
            "message": {"role": "user", "content": request.prompt},
            "parent_tool_use_id": Value::Null,
        })
        .to_string());
    }
    let mut blocks = Vec::new();
    for path in &request.attachments {
        let bytes = tokio::fs::read(path).await.map_err(|err| {
            tracing::debug!(path, %err, "capture attachment read failed");
            anyhow!(
                "An attachment could not be read. Check the attachment path and retry the capture."
            )
        })?;
        let media_type = image_media_type(std::path::Path::new(path), &bytes).ok_or_else(|| {
            anyhow!("An attachment format is not supported. Use PNG, JPEG, GIF, or WebP and retry.")
        })?;
        blocks.push(json!({
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": media_type,
                "data": base64::engine::general_purpose::STANDARD.encode(bytes),
            },
        }));
    }
    blocks.push(json!({"type": "text", "text": request.prompt}));
    Ok(json!({
        "type": "user",
        "message": {"role": "user", "content": blocks},
        "parent_tool_use_id": Value::Null,
    })
    .to_string())
}

fn image_media_type(path: &std::path::Path, bytes: &[u8]) -> Option<&'static str> {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => Some("image/png"),
        Some("jpg" | "jpeg") => Some("image/jpeg"),
        Some("gif") => Some("image/gif"),
        Some("webp") => Some("image/webp"),
        _ if bytes.starts_with(b"\x89PNG\r\n\x1a\n") => Some("image/png"),
        _ if bytes.starts_with(&[0xff, 0xd8, 0xff]) => Some("image/jpeg"),
        _ if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") => Some("image/gif"),
        _ if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") => Some("image/webp"),
        _ => None,
    }
}

fn codex_initialize_line() -> String {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "clientInfo": {
                "name": "comet-native",
                "title": "Comet",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "capabilities": {"experimentalApi": true},
        },
    })
    .to_string()
}

fn codex_model_list_line(id: u64, cursor: Option<&str>) -> String {
    let mut params = serde_json::Map::new();
    if let Some(cursor) = cursor {
        params.insert("cursor".into(), cursor.into());
    }
    rpc_request(id, "model/list", Value::Object(params))
}

fn rpc_request(id: u64, method: &str, params: Value) -> String {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string()
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use comet_proto::{ReasoningLevel, RunRequest, RuntimeMode, SandboxLevel};
    use serde_json::json;

    use super::{
        CaptureConfig, CaptureOperation, CaptureScenario, Channel, ClaudeCaptureOperation,
        ClaudeRunScript, CodexCaptureOperation, CodexRunScript, CommandSnapshot, LaunchDescriptor,
        Provider, RecordingSession, StdioMode, record,
    };

    fn contract_request() -> RunRequest {
        let mut request = RunRequest {
            prompt: "capture contract".into(),
            model: Some("claude-sonnet-5".into()),
            reasoning: Some(ReasoningLevel::XHigh),
            cwd: std::env::temp_dir()
                .join("comet capture cwd")
                .display()
                .to_string(),
            resume: Some("session-to-resume".into()),
            ..RunRequest::for_session(RuntimeMode::FullAccess)
        };
        request
            .model_options
            .insert("contextWindow".into(), json!("1m"));
        request.model_options.insert("fastMode".into(), json!(true));
        request.model_options.insert("thinking".into(), json!("on"));
        request
    }

    fn absolute_program(name: &str) -> PathBuf {
        std::env::current_dir().unwrap().join(name)
    }

    #[test]
    fn claude_capture_uses_the_run_command_builder() {
        let request = contract_request();
        let exe = absolute_program("claude");
        let launch = crate::claude::run_launch(&exe, &request);
        let snapshot = CommandSnapshot::from_launch(&launch);

        assert_eq!(snapshot.program, exe.display().to_string());
        assert_eq!(
            &snapshot.args[..18],
            [
                "--print",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--verbose",
                "--include-partial-messages",
                "--permission-prompt-tool",
                "stdio",
                "--model",
                "claude-sonnet-5[1m]",
                "--effort",
                "xhigh",
                "--permission-mode",
                "bypassPermissions",
                "--dangerously-skip-permissions",
                "--resume=session-to-resume",
                "--settings",
            ]
        );
        assert_eq!(snapshot.args.len(), 19);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&snapshot.args[18]).unwrap(),
            json!({"alwaysThinkingEnabled": true, "fastMode": true})
        );
        assert_eq!(snapshot.cwd.as_deref(), Some(request.cwd.as_str()));
        assert!(snapshot.configured_env.is_empty(), "PATH is never captured");
        assert_eq!(snapshot.stdin, StdioMode::Piped);
        assert_eq!(snapshot.stdout, StdioMode::Piped);
        assert_eq!(snapshot.stderr, StdioMode::Piped);
        assert!(snapshot.kill_on_drop);
        #[cfg(windows)]
        assert_eq!(snapshot.creation_flags, 0);
    }

    #[test]
    fn codex_capture_uses_the_run_command_builder() {
        let request = contract_request();
        let exe = absolute_program("codex");
        let launch = crate::codex::run_launch(&exe, &request);
        let snapshot = CommandSnapshot::from_launch(&launch);

        assert_eq!(snapshot.program, exe.display().to_string());
        assert_eq!(snapshot.args, ["app-server"]);
        assert_eq!(snapshot.cwd.as_deref(), Some(request.cwd.as_str()));
        assert!(snapshot.configured_env.is_empty(), "PATH is never captured");
        assert_eq!(snapshot.stdin, StdioMode::Piped);
        assert_eq!(snapshot.stdout, StdioMode::Piped);
        assert_eq!(snapshot.stderr, StdioMode::Piped);
        assert!(snapshot.kill_on_drop);
        #[cfg(windows)]
        assert_eq!(snapshot.creation_flags, 0);
    }

    #[test]
    fn claude_model_discovery_capture_uses_the_initialize_builder() {
        let exe = absolute_program("claude");
        let cwd = std::env::temp_dir();
        let launch = crate::claude::discovery::model_discovery_launch(&exe, &cwd);
        let snapshot = CommandSnapshot::from_launch(&launch);

        assert_eq!(snapshot.program, exe.display().to_string());
        assert_eq!(
            snapshot.args,
            [
                "--print",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--verbose",
                "--bare",
            ]
        );
        assert_eq!(
            snapshot.cwd.as_deref(),
            Some(cwd.to_string_lossy().as_ref())
        );
        assert!(snapshot.configured_env.is_empty(), "PATH is never captured");
        assert_eq!(snapshot.stdin, StdioMode::Piped);
        assert_eq!(snapshot.stdout, StdioMode::Piped);
        assert_eq!(snapshot.stderr, StdioMode::Piped);
        assert!(snapshot.kill_on_drop);
        #[cfg(windows)]
        assert_eq!(snapshot.creation_flags, 0x0800_0000);
    }

    #[test]
    fn claude_command_discovery_capture_uses_the_initialize_builder() {
        let exe = absolute_program("claude");
        let cwd = std::env::temp_dir().join("comet command discovery");
        let launch = crate::claude::commands::command_discovery_launch(&exe, &cwd);
        let snapshot = CommandSnapshot::from_launch(&launch);

        assert_eq!(snapshot.program, exe.display().to_string());
        assert_eq!(
            snapshot.args,
            [
                "--print",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--verbose",
            ]
        );
        assert_eq!(
            snapshot.cwd.as_deref(),
            Some(cwd.to_string_lossy().as_ref())
        );
        assert!(
            !snapshot.args.iter().any(|arg| arg == "--bare"),
            "command discovery must not use --bare: {:?}",
            snapshot.args
        );
        assert_eq!(snapshot.stdin, StdioMode::Piped);
        assert_eq!(snapshot.stdout, StdioMode::Piped);
        assert_eq!(snapshot.stderr, StdioMode::Piped);
        assert!(snapshot.kill_on_drop);
        #[cfg(windows)]
        assert_eq!(snapshot.creation_flags, 0x0800_0000);
    }

    #[test]
    fn codex_model_discovery_capture_uses_the_discovery_builder() {
        let exe = absolute_program("codex");
        let home = absolute_program("codex-home");
        let cwd = std::env::temp_dir();
        let launch = crate::codex::discovery::discovery_launch(&exe, &home, &cwd);
        let snapshot = CommandSnapshot::from_launch(&launch);

        assert_eq!(snapshot.program, exe.display().to_string());
        assert_eq!(snapshot.args, ["app-server"]);
        assert_eq!(
            snapshot.cwd.as_deref(),
            Some(cwd.to_string_lossy().as_ref())
        );
        assert_eq!(
            snapshot
                .configured_env
                .get("CODEX_HOME")
                .map(String::as_str),
            Some(home.to_string_lossy().as_ref())
        );
        assert_eq!(snapshot.configured_env.len(), 1, "PATH is never captured");
        assert_eq!(snapshot.stdin, StdioMode::Piped);
        assert_eq!(snapshot.stdout, StdioMode::Piped);
        assert_eq!(snapshot.stderr, StdioMode::Piped);
        assert!(snapshot.kill_on_drop);
        #[cfg(windows)]
        assert_eq!(snapshot.creation_flags, 0x0800_0000);
    }

    #[test]
    fn command_snapshot_never_records_path_or_unallowlisted_environment() {
        let launch = LaunchDescriptor {
            program: Path::new("provider").into(),
            args: Vec::new(),
            cwd: None,
            configured_env: [
                ("PATH".into(), "secret ambient path".into()),
                ("UNRELATED_SECRET".into(), "must not be captured".into()),
                ("CODEX_HOME".into(), "safe configured home".into()),
            ]
            .into(),
            stdin: StdioMode::Inherit,
            stdout: StdioMode::Null,
            stderr: StdioMode::Piped,
            kill_on_drop: false,
            #[cfg(windows)]
            creation_flags: 0,
        };

        let snapshot = CommandSnapshot::from_launch(&launch);

        assert_eq!(
            snapshot.configured_env,
            [("CODEX_HOME".into(), "safe configured home".into())].into()
        );
        assert_eq!(snapshot.stdin, StdioMode::Inherit);
        assert_eq!(snapshot.stdout, StdioMode::Null);
        assert_eq!(snapshot.stderr, StdioMode::Piped);
        assert!(!snapshot.kill_on_drop);
    }

    fn fixture_path(name: &str) -> PathBuf {
        let variable = format!("CARGO_BIN_EXE_{}", name.replace('-', "_"));
        if let Some(path) = std::env::var_os(&variable) {
            return path.into();
        }
        let suffix = std::env::consts::EXE_SUFFIX;
        std::env::current_exe()
            .expect("current test executable")
            .parent()
            .and_then(Path::parent)
            .expect("target debug directory")
            .join(format!("{name}{suffix}"))
    }

    fn config(
        name: &'static str,
        executable: PathBuf,
        operation: CaptureOperation,
        raw_root: &Path,
    ) -> CaptureConfig {
        CaptureConfig {
            scenario: CaptureScenario {
                name,
                purpose: "local recorder test",
                operation,
            },
            executable: Some(executable),
            codex_home: None,
            raw_root: raw_root.into(),
            timeout: Duration::from_secs(5),
        }
    }

    fn channel_payloads(capture: &super::RawCapture, channel: Channel) -> Vec<&str> {
        capture
            .events
            .iter()
            .filter(|event| event.channel == channel)
            .map(|event| event.payload.as_str())
            .collect()
    }

    /// Break caught: selecting command discovery's non-bare launch for model discovery,
    /// dropping a configured pipe, or allocating sequence numbers outside observer order.
    #[tokio::test]
    async fn recorder_claude_model_discovery_keeps_all_channels_and_monotonic_sequence() {
        let raw = tempfile::tempdir().unwrap();
        let capture = record(config(
            "claude-model-discovery",
            fixture_path("fake-claude"),
            CaptureOperation::Claude(ClaudeCaptureOperation::ModelDiscovery),
            raw.path(),
        ))
        .await
        .unwrap();

        assert_eq!(capture.provider, Provider::Claude);
        assert!(capture.command.args.iter().any(|arg| arg == "--bare"));
        assert!(
            capture
                .events
                .windows(2)
                .all(|pair| pair[0].sequence < pair[1].sequence)
        );
        for channel in [Channel::Stdin, Channel::Stdout, Channel::Stderr] {
            assert!(
                capture.events.iter().any(|event| event.channel == channel),
                "missing configured {channel:?} channel"
            );
        }
        assert_eq!(
            channel_payloads(&capture, Channel::Stdin),
            [
                r#"{"type":"control_request","request_id":"comet-discovery-1","request":{"subtype":"initialize"}}"#
            ]
        );
        assert_eq!(capture.exit_code, Some(0));
        assert!(capture.directory.starts_with(raw.path()));
        let persisted: super::RawCapture =
            serde_json::from_slice(&std::fs::read(capture.directory.join("capture.json")).unwrap())
                .unwrap();
        assert_eq!(persisted.events, capture.events);
    }

    /// Break caught: command discovery accidentally inherits model discovery's `--bare`.
    #[tokio::test]
    async fn recorder_claude_command_discovery_uses_non_bare_initialize() {
        let raw = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let capture = record(config(
            "claude-command-discovery",
            fixture_path("fake-claude"),
            CaptureOperation::Claude(ClaudeCaptureOperation::CommandDiscovery {
                cwd: cwd.path().into(),
            }),
            raw.path(),
        ))
        .await
        .unwrap();

        assert!(!capture.command.args.iter().any(|arg| arg == "--bare"));
        assert_eq!(
            capture.command.cwd.as_deref(),
            Some(cwd.path().to_string_lossy().as_ref())
        );
        assert_eq!(capture.exit_code, Some(0));
    }

    /// Break caught: raw evidence cannot identify the OS/architecture that produced its
    /// provider frames, or persists prose instead of independently queryable fields.
    #[tokio::test]
    async fn recorder_persists_structured_host_platform() {
        let raw = tempfile::tempdir().unwrap();
        let capture = record(config(
            "claude-platform",
            fixture_path("fake-claude"),
            CaptureOperation::Claude(ClaudeCaptureOperation::ModelDiscovery),
            raw.path(),
        ))
        .await
        .unwrap();

        assert_eq!(capture.platform.os, std::env::consts::OS);
        assert_eq!(capture.platform.arch, std::env::consts::ARCH);
        let persisted: serde_json::Value =
            serde_json::from_slice(&std::fs::read(capture.directory.join("capture.json")).unwrap())
                .unwrap();
        assert_eq!(persisted["platform"]["os"], std::env::consts::OS);
        assert_eq!(persisted["platform"]["arch"], std::env::consts::ARCH);
    }

    /// Break caught: stopping after the first Codex page, failing to serialize an opaque cursor,
    /// or omitting either half of the initialize handshake from the raw stdin record.
    #[tokio::test]
    async fn recorder_codex_model_discovery_records_initialize_and_every_page() {
        let raw = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let mut config = config(
            "codex-model-discovery",
            fixture_path("fake-codex"),
            CaptureOperation::Codex(CodexCaptureOperation::ModelDiscovery),
            raw.path(),
        );
        config.codex_home = Some(home.path().into());
        let capture = record(config).await.unwrap();

        let stdin = channel_payloads(&capture, Channel::Stdin);
        assert_eq!(stdin.len(), 5, "initialize, initialized, and three pages");
        let lines: Vec<serde_json::Value> = stdin
            .iter()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines[0]["method"], "initialize");
        assert_eq!(lines[1], json!({"jsonrpc": "2.0", "method": "initialized"}));
        assert_eq!(lines[2]["method"], "model/list");
        assert!(lines[2]["params"].get("cursor").is_none());
        assert_eq!(lines[3]["params"]["cursor"], "2\"\\ opaque");
        assert_eq!(lines[4]["params"]["cursor"], "4\"\\ opaque");
        assert_eq!(capture.exit_code, Some(0));
    }

    /// Break caught: a Claude run driver invents its own initial wire line instead of recording
    /// the exact provider-specific user message it writes through the production run launch.
    #[tokio::test]
    async fn recorder_claude_run_records_the_exact_initial_write() {
        let raw = tempfile::tempdir().unwrap();
        let request = RunRequest {
            prompt: "scenario:happy".into(),
            ..RunRequest::for_session(RuntimeMode::ApprovalRequired)
        };
        let capture = record(config(
            "claude-fresh-text",
            fixture_path("fake-claude"),
            CaptureOperation::Claude(ClaudeCaptureOperation::Run {
                request,
                script: ClaudeRunScript::FreshText,
            }),
            raw.path(),
        ))
        .await
        .unwrap();

        assert_eq!(
            channel_payloads(&capture, Channel::Stdin),
            [
                r#"{"message":{"content":"scenario:happy","role":"user"},"parent_tool_use_id":null,"type":"user"}"#
            ]
        );
        assert_eq!(capture.exit_code, Some(0));
    }

    /// Break caught: the Codex run driver skips a handshake stage, loses the concrete run script,
    /// or waits forever after the provider's terminal turn notification.
    #[tokio::test]
    async fn recorder_codex_run_records_the_explicit_script() {
        let raw = tempfile::tempdir().unwrap();
        let request = RunRequest {
            prompt: "scenario:fail".into(),
            model: Some("gpt-5.6-luna".into()),
            cwd: std::env::temp_dir().display().to_string(),
            sandbox: SandboxLevel::WorkspaceWrite,
            ..RunRequest::for_session(RuntimeMode::ApprovalRequired)
        };
        let capture = record(config(
            "codex-fresh-text",
            fixture_path("fake-codex"),
            CaptureOperation::Codex(CodexCaptureOperation::Run {
                request,
                script: CodexRunScript::FreshText,
            }),
            raw.path(),
        ))
        .await
        .unwrap();

        let stdin = channel_payloads(&capture, Channel::Stdin);
        let methods: Vec<_> = stdin
            .iter()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter_map(|line| line["method"].as_str().map(str::to_owned))
            .collect();
        assert_eq!(
            methods,
            ["initialize", "initialized", "thread/start", "turn/start"]
        );
        assert_eq!(capture.exit_code, Some(0));
    }

    /// Break caught: capture skips the production request normalization that works around
    /// Codex's malformed workspace-write mount for linked slash-branch worktrees.
    #[tokio::test]
    async fn recorder_codex_run_preserves_production_linked_worktree_parameters() {
        let raw = tempfile::tempdir().unwrap();
        let worktree = tempfile::tempdir().unwrap();
        let admin = tempfile::tempdir().unwrap();
        std::fs::write(
            worktree.path().join(".git"),
            format!("gitdir: {}", admin.path().display()),
        )
        .unwrap();
        std::fs::write(
            admin.path().join("HEAD"),
            "ref: refs/heads/feature/capture\n",
        )
        .unwrap();
        let mut request = RunRequest {
            prompt: "scenario:fail".into(),
            model: Some("gpt-5.6-luna".into()),
            reasoning: Some(ReasoningLevel::Low),
            cwd: worktree.path().display().to_string(),
            sandbox: SandboxLevel::WorkspaceWrite,
            ..RunRequest::for_session(RuntimeMode::ApprovalRequired)
        };
        request
            .model_options
            .insert("serviceTier".into(), json!("fast"));
        let provider_request = crate::codex::normalize_run_request(request.clone());

        let capture = record(config(
            "codex-linked-worktree",
            fixture_path("fake-codex"),
            CaptureOperation::Codex(CodexCaptureOperation::Run {
                request,
                script: CodexRunScript::FreshText,
            }),
            raw.path(),
        ))
        .await
        .unwrap();

        let stdin: Vec<serde_json::Value> = channel_payloads(&capture, Channel::Stdin)
            .into_iter()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let thread = stdin
            .iter()
            .find(|line| line["method"] == "thread/start")
            .unwrap();
        let expected_thread = json!({
            "cwd": worktree.path().display().to_string(),
            "approvalPolicy": "untrusted",
            "sandbox": "danger-full-access",
            "approvalsReviewer": "user",
            "model": "gpt-5.6-luna",
            "serviceTier": "fast",
        });
        assert_eq!(thread["params"], expected_thread);
        assert_eq!(
            crate::codex::thread_start_params(&provider_request),
            expected_thread
        );
        assert_eq!(
            crate::codex::thread_resume_params(&provider_request, "resume-thread"),
            json!({
                "cwd": worktree.path().display().to_string(),
                "approvalPolicy": "untrusted",
                "sandbox": "danger-full-access",
                "approvalsReviewer": "user",
                "model": "gpt-5.6-luna",
                "serviceTier": "fast",
                "threadId": "resume-thread",
            })
        );
        let turn = stdin
            .iter()
            .find(|line| line["method"] == "turn/start")
            .unwrap();
        let expected_turn = json!({
            "threadId": "th-1",
            "input": [{"type": "text", "text": "scenario:fail"}],
            "approvalPolicy": "untrusted",
            "sandboxPolicy": {"type": "dangerFullAccess"},
            "summary": "auto",
            "model": "gpt-5.6-luna",
            "effort": "low",
            "serviceTier": "fast",
        });
        assert_eq!(turn["params"], expected_turn);
        assert_eq!(
            crate::codex::turn_start_params(&provider_request, "th-1", "scenario:fail"),
            expected_turn
        );
    }

    /// Break caught: the hard-timeout branch returns before killing and reaping the child.
    #[tokio::test]
    async fn recorder_timeout_kills_and_reaps_the_child() {
        let raw = tempfile::tempdir().unwrap();
        let request = RunRequest {
            prompt: "scenario:interrupt".into(),
            ..RunRequest::for_session(RuntimeMode::ApprovalRequired)
        };
        let mut config = config(
            "claude-timeout",
            fixture_path("fake-claude"),
            CaptureOperation::Claude(ClaudeCaptureOperation::Run {
                request,
                script: ClaudeRunScript::FreshText,
            }),
            raw.path(),
        );
        config.timeout = Duration::from_millis(100);

        let mut session = RecordingSession::start(config).await.unwrap();
        let pid = session.child_id().expect("spawned child id");
        let error = session.finish().await.unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert!(!process_is_live(pid), "provider child {pid} remains live");
    }

    /// Break caught: drop delegates `wait()` to the originating Tokio runtime, whose shutdown
    /// cancels the task before the killed child is reaped.
    #[test]
    fn recorder_drop_reaper_survives_originating_runtime_shutdown() {
        let raw = tempfile::tempdir().unwrap();
        let request = RunRequest {
            prompt: "scenario:interrupt".into(),
            ..RunRequest::for_session(RuntimeMode::ApprovalRequired)
        };
        let config = config(
            "claude-drop",
            fixture_path("fake-claude"),
            CaptureOperation::Claude(ClaudeCaptureOperation::Run {
                request,
                script: ClaudeRunScript::FreshText,
            }),
            raw.path(),
        );
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut session = runtime.block_on(RecordingSession::start(config)).unwrap();
        let pid = session.child_id().expect("spawned child id");
        let (reaped_tx, reaped_rx) = std::sync::mpsc::sync_channel(1);
        session.reap_notice = Some(reaped_tx);

        runtime.block_on(async move { drop(session) });
        drop(runtime);

        assert_eq!(
            reaped_rx.recv_timeout(Duration::from_secs(2)),
            Ok(pid),
            "drop reaper did not finish after its originating runtime shut down"
        );
        assert!(!process_is_live(pid), "provider child {pid} remains live");
    }

    #[cfg(unix)]
    fn process_is_live(pid: u32) -> bool {
        // SAFETY: signal 0 does not modify the target; it only probes whether pid exists.
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }

    #[cfg(windows)]
    fn process_is_live(pid: u32) -> bool {
        use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };

        // SAFETY: the handle is checked for null, used only for a status query, then closed.
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                return false;
            }
            let mut status = 0;
            let queried = GetExitCodeProcess(handle, &mut status) != 0;
            CloseHandle(handle);
            queried && status == STILL_ACTIVE as u32
        }
    }
}
