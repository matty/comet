use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{Value, json};

use super::filesystem::has_windows_reparse_point;
use super::types::{Channel, PartialRawCapture, Provider, RawCapture};

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
    #[error("incomplete capture evidence cannot be sanitized or promoted")]
    IncompleteCapture,
    #[error("staging output must be below .comet-provider-captures/staging")]
    UnsafeOutputDirectory,
    #[error("capture contains an unrecognized absolute path at {location}")]
    UnrecognizedAbsolutePath { location: String },
    #[error("capture contains a secret-like field at {location}")]
    SecretLikeField { location: String },
    #[error("capture contains a secret-like value at {location}")]
    SecretLikeValue { location: String },
    #[error("capture contains a sensitive object key at {location}")]
    SensitiveObjectKey { location: String },
    #[error("capture channel contains unparseable structured JSON at sequence {sequence}")]
    UnparseableStructuredPayload { sequence: u64 },
    #[error("Claude capture command has invalid resume arguments at {location}")]
    InvalidClaudeResumeCommand { location: String },
    #[error("sanitized capture could not be written")]
    WriteOutput {
        #[source]
        source: std::io::Error,
    },
    #[error("sanitized staging destination is busy; retry after the other publisher finishes")]
    PublicationBusy {
        #[source]
        source: std::io::Error,
    },
    #[error("sanitized staging destination already contains different evidence")]
    PublicationConflict {
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
    ProviderProse,
    MachineId,
    AttachmentBytes,
    ClaudeMemoryPath,
    ClaudeMessageId,
    ClaudeThinkingSignature,
    CodexMcpServerName,
    CodexThreadPath,
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
            Self::ProviderProse => "PROVIDER_PROSE",
            Self::MachineId => "MACHINE_ID",
            Self::AttachmentBytes => "ATTACHMENT_BYTES",
            Self::ClaudeMemoryPath => "CLAUDE_MEMORY_PATH",
            Self::ClaudeMessageId => "CLAUDE_MESSAGE_ID",
            Self::ClaudeThinkingSignature => "CLAUDE_THINKING_SIGNATURE",
            Self::CodexMcpServerName => "CODEX_MCP_SERVER_NAME",
            Self::CodexThreadPath => "CODEX_THREAD_PATH",
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
            Self::ProviderProse => "provider_prose",
            Self::MachineId => "machine_id",
            Self::AttachmentBytes => "attachment_bytes",
            Self::ClaudeMemoryPath => "claude_memory_path",
            Self::ClaudeMessageId => "claude_message_id",
            Self::ClaudeThinkingSignature => "claude_thinking_signature",
            Self::CodexMcpServerName => "codex_mcp_server_name",
            Self::CodexThreadPath => "codex_thread_path",
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
    claude_capture: bool,
    json_root: bool,
    speaker: Speaker,
    codex_turn_input: bool,
    codex_assistant_prose: bool,
    codex_assistant_prose_array: bool,
    discovery_metadata: bool,
    codex_catalog: bool,
    codex_root_notification: CodexNotification,
    codex_direct_params: CodexNotification,
    availability_nux: bool,
    claude_memory_paths: bool,
    claude_tool_use: bool,
    claude_tool_input: bool,
    entity: Entity,
}

#[derive(Clone, Copy, Default, Eq, PartialEq)]
enum CodexNotification {
    #[default]
    None,
    TokenUsage,
    McpStartupStatus,
}

#[derive(Clone, Copy, Default)]
enum Entity {
    #[default]
    None,
    Thread,
    Turn,
    Item,
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

    let capture_path = raw_dir.join("capture.json");
    let partial_path = raw_dir.join("partial-capture.json");
    if partial_path.exists() {
        let bytes =
            std::fs::read(partial_path).map_err(|source| SanitizationError::ReadRaw { source })?;
        let _: PartialRawCapture = serde_json::from_slice(&bytes)
            .map_err(|source| SanitizationError::InvalidRaw { source })?;
        return Err(SanitizationError::IncompleteCapture);
    }
    let bytes =
        std::fs::read(capture_path).map_err(|source| SanitizationError::ReadRaw { source })?;
    let capture: RawCapture = serde_json::from_slice(&bytes)
        .map_err(|source| SanitizationError::InvalidRaw { source })?;
    let mut redactor = Redactor::new(&capture);
    let semantic_context = SemanticContext {
        claude_capture: capture.provider == Provider::Claude,
        json_root: true,
        ..SemanticContext::default()
    };

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
        match payload {
            Payload::Json(value) => {
                redactor.collect_semantics(value, semantic_context);
            }
            Payload::Text(text) => {
                redactor.register(RedactionKind::ProviderProse, &Value::String(text.clone()));
            }
        }
    }

    let mut command = serde_json::to_value(&capture.command)
        .map_err(|source| SanitizationError::EncodeOutput { source })?;
    if capture.provider == Provider::Claude {
        redactor.sanitize_claude_resume_argv(&mut command, &capture.scenario)?;
    }
    redactor.sanitize_nonsemantic_value(&mut command, "command")?;

    let mut events_bytes = Vec::new();
    for (event, payload) in capture.events.iter().zip(&mut payloads) {
        let payload = match payload {
            Payload::Json(value) => {
                redactor.sanitize_json(value, semantic_context, "event.payload")?;
                serde_json::to_string(value)
                    .map_err(|source| SanitizationError::EncodeOutput { source })?
            }
            Payload::Text(text) => {
                let mut value = Value::String(text.clone());
                redactor.replace_semantic(RedactionKind::ProviderProse, &mut value);
                *text = value
                    .as_str()
                    .expect("text replacement stays a string")
                    .to_owned();
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
    let mut scenario = capture.scenario.clone();
    redactor.sanitize_paths_and_validate(&mut scenario, "scenario")?;
    let mut purpose = capture.purpose.clone();
    redactor.sanitize_paths_and_validate(&mut purpose, "purpose")?;
    let mut platform = serde_json::to_value(&capture.platform)
        .map_err(|source| SanitizationError::EncodeOutput { source })?;
    redactor.sanitize_nonsemantic_value(&mut platform, "platform")?;
    let channels: Vec<Channel> = capture.events.iter().fold(Vec::new(), |mut seen, event| {
        if !seen.contains(&event.channel) {
            seen.push(event.channel);
        }
        seen
    });
    let manifest = json!({
        "schema_version": 1,
        "source": "capture.json",
        "provider": capture.provider,
        "cli_version": cli_version,
        "normalized_cli_version": normalized_cli_version,
        "captured_at_unix_ms": capture.captured_at_unix_ms,
        "scenario": scenario,
        "purpose": purpose,
        "platform": platform,
        "command": command,
        "channels": channels,
        "exit_code": capture.exit_code,
        "placeholders": redactor.placeholder_definitions(),
        "redaction_counts": redactor.counts,
    });
    let mut manifest_bytes = serde_json::to_vec_pretty(&manifest)
        .map_err(|source| SanitizationError::EncodeOutput { source })?;
    manifest_bytes.push(b'\n');

    let events_path = output_dir.join("events.jsonl");
    let manifest_path = output_dir.join("manifest.json");
    publish_staging_pair_with(output_dir, &events_bytes, &manifest_bytes, |_| Ok(())).map_err(
        |source| match source.kind() {
            std::io::ErrorKind::WouldBlock => SanitizationError::PublicationBusy { source },
            std::io::ErrorKind::AlreadyExists => SanitizationError::PublicationConflict { source },
            _ => SanitizationError::WriteOutput { source },
        },
    )?;

    Ok(SanitizationReport {
        events_path,
        manifest_path,
        events_bytes,
        manifest_bytes,
    })
}

fn publish_staging_pair_with<F>(
    output_dir: &Path,
    events: &[u8],
    manifest: &[u8],
    after_events: F,
) -> std::io::Result<()>
where
    F: FnOnce(&Path) -> std::io::Result<()>,
{
    publish_staging_pair_with_commit(output_dir, events, manifest, after_events, |from, to| {
        std::fs::rename(from, to)
    })
}

fn publish_staging_pair_with_commit<F, C>(
    output_dir: &Path,
    events: &[u8],
    manifest: &[u8],
    after_events: F,
    commit: C,
) -> std::io::Result<()>
where
    F: FnOnce(&Path) -> std::io::Result<()>,
    C: FnOnce(&Path, &Path) -> std::io::Result<()>,
{
    let parent = output_dir.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "staging destination has no parent",
        )
    })?;
    std::fs::create_dir_all(parent)?;
    let output_name = output_dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("capture");
    let publication_lock = PublicationLock::acquire(parent, output_name)?;
    let result = (|| {
        let temporary_prefix = format!(".{output_name}.sanitize-");
        let temporary = parent.join(format!("{temporary_prefix}{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&temporary)?;

        let prepared = (|| {
            write_synced(&temporary.join("events.jsonl"), events)?;
            after_events(&temporary)?;
            write_synced(&temporary.join("manifest.json"), manifest)?;
            Ok(())
        })();
        if let Err(error) = prepared {
            let _ = remove_verified_generated_dir(&temporary, parent, &temporary_prefix);
            return Err(error);
        }

        if output_dir.exists() {
            let identical = match destination_is_exact_pair(output_dir, events, manifest) {
                Ok(identical) => identical,
                Err(error) => {
                    let _ = remove_verified_generated_dir(&temporary, parent, &temporary_prefix);
                    return Err(error);
                }
            };
            remove_verified_generated_dir(&temporary, parent, &temporary_prefix)?;
            if identical {
                return Ok(());
            }
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "sanitized staging destination already contains different evidence",
            ));
        }

        if let Err(error) = commit(&temporary, output_dir) {
            let _ = remove_verified_generated_dir(&temporary, parent, &temporary_prefix);
            return Err(error);
        }
        Ok(())
    })();
    match publication_lock.release() {
        Ok(()) => result,
        Err(error) => Err(error),
    }
}

fn destination_is_exact_pair(
    output_dir: &Path,
    expected_events: &[u8],
    expected_manifest: &[u8],
) -> std::io::Result<bool> {
    let directory_metadata = std::fs::symlink_metadata(output_dir)?;
    if !directory_metadata.file_type().is_dir()
        || directory_metadata.file_type().is_symlink()
        || has_windows_reparse_point(&directory_metadata)
    {
        return Ok(false);
    }

    let mut events_match = false;
    let mut manifest_match = false;
    let mut entry_count = 0;
    for entry in std::fs::read_dir(output_dir)? {
        let entry = entry?;
        entry_count += 1;
        let metadata = std::fs::symlink_metadata(entry.path())?;
        if !metadata.file_type().is_file()
            || metadata.file_type().is_symlink()
            || has_windows_reparse_point(&metadata)
        {
            return Ok(false);
        }
        if entry.file_name() == "events.jsonl" {
            events_match = std::fs::read(entry.path())? == expected_events;
        } else if entry.file_name() == "manifest.json" {
            manifest_match = std::fs::read(entry.path())? == expected_manifest;
        } else {
            return Ok(false);
        }
    }
    Ok(entry_count == 2 && events_match && manifest_match)
}

struct PublicationLock {
    _file: std::fs::File,
    path: PathBuf,
    parent: PathBuf,
    name: String,
    owner: String,
}

impl PublicationLock {
    fn acquire(parent: &Path, output_name: &str) -> std::io::Result<Self> {
        let parent = std::fs::canonicalize(parent)?;
        let name = format!(".{output_name}.publish.lock");
        let path = parent.join(&name);
        if path.parent() != Some(parent.as_path()) || path.file_name() != Some(name.as_ref()) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "refusing to create an unverified publication lock",
            ));
        }
        let mut file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "sanitized staging destination is busy",
                ));
            }
            Err(error) => return Err(error),
        };
        let owner = uuid::Uuid::new_v4().to_string();
        {
            use std::io::Write as _;
            file.write_all(owner.as_bytes())?;
            file.sync_all()?;
        }
        Ok(Self {
            _file: file,
            path,
            parent,
            name,
            owner,
        })
    }

    fn release(self) -> std::io::Result<()> {
        remove_verified_generated_file(&self.path, &self.parent, &self.name, self.owner.as_bytes())
    }
}

fn write_synced(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn remove_verified_generated_dir(
    generated: &Path,
    parent: &Path,
    expected_prefix: &str,
) -> std::io::Result<()> {
    let resolved_parent = std::fs::canonicalize(parent)?;
    let resolved_generated = std::fs::canonicalize(generated)?;
    let valid_name = resolved_generated
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with(expected_prefix));
    if resolved_generated.parent() != Some(resolved_parent.as_path()) || !valid_name {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "refusing to clean an unverified staging sibling",
        ));
    }
    std::fs::remove_dir_all(resolved_generated)
}

fn remove_verified_generated_file(
    generated: &Path,
    parent: &Path,
    expected_name: &str,
    expected_contents: &[u8],
) -> std::io::Result<()> {
    let resolved_parent = std::fs::canonicalize(parent)?;
    let resolved_generated = std::fs::canonicalize(generated)?;
    let metadata = std::fs::symlink_metadata(generated)?;
    let valid_name = resolved_generated.file_name() == Some(expected_name.as_ref());
    if resolved_generated.parent() != Some(resolved_parent.as_path())
        || !valid_name
        || !metadata.file_type().is_file()
        || metadata.file_type().is_symlink()
        || has_windows_reparse_point(&metadata)
        || std::fs::read(generated)? != expected_contents
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "refusing to clean an unverified publication lock",
        ));
    }
    std::fs::remove_file(resolved_generated)
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
        redactor.add_path(capture.redaction_roots.cwd.as_deref(), "<CWD>", "cwd_path");
        redactor.add_path(
            capture.redaction_roots.repo.as_deref(),
            "<REPO>",
            "repo_path",
        );
        redactor.add_path(
            capture.redaction_roots.home.as_deref(),
            "<HOME>",
            "home_path",
        );
        redactor.add_path(
            capture.redaction_roots.temp.as_deref(),
            "<TEMP>",
            "temp_path",
        );
        redactor.add_path(
            capture.redaction_roots.codex_home.as_deref(),
            "<CODEX_HOME>",
            "codex_home_path",
        );
        redactor.add_path(
            capture.redaction_roots.approval_target.as_deref(),
            "<APPROVAL_TARGET>",
            "approval_target_path",
        );
        redactor.add_path(
            capture.redaction_roots.trusted_powershell.as_deref(),
            "<TRUSTED_POWERSHELL>",
            "trusted_powershell_path",
        );
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
        if context.codex_assistant_prose_array && value.is_string() {
            self.register(RedactionKind::AssistantProse, value);
            return;
        }
        match value {
            Value::Array(values) => {
                for value in values {
                    self.collect_semantics(value, descendant_context(context));
                }
            }
            Value::Object(object) => {
                let context = object_context(object, context);
                for (key, value) in object {
                    if let Some(kind) = semantic_kind(object, key, value, context) {
                        self.register(kind, value);
                    } else {
                        self.collect_semantics(value, child_context(context, key));
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
        if context.codex_assistant_prose_array && value.is_string() {
            self.replace_semantic(RedactionKind::AssistantProse, value);
        }
        match value {
            Value::Array(values) => {
                for (index, value) in values.iter_mut().enumerate() {
                    self.sanitize_json(
                        value,
                        descendant_context(context),
                        &format!("{location}[{index}]"),
                    )?;
                }
            }
            Value::Object(object) => {
                let context = object_context(object, context);
                let keys: Vec<String> = object.keys().cloned().collect();
                for (index, key) in keys.into_iter().enumerate() {
                    let child_location = format!("{location}.object[{index}]");
                    self.validate_key(&key, &child_location)?;
                    if is_secret_field(&key, &object[&key])
                        && !is_codex_token_usage_object(&key, &object[&key], context)
                    {
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
                        self.sanitize_json(child, child_context(context, &key), &child_location)?;
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
                for (index, (key, value)) in object.iter_mut().enumerate() {
                    let child_location = format!("{location}.object[{index}]");
                    self.validate_key(key, &child_location)?;
                    if is_secret_field(key, value) {
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

    fn validate_key(&mut self, key: &str, location: &str) -> Result<(), SanitizationError> {
        let mut sanitized = key.to_owned();
        self.sanitize_paths_and_validate(&mut sanitized, location)?;
        if sanitized != key {
            return Err(SanitizationError::SensitiveObjectKey {
                location: location.to_owned(),
            });
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

    fn sanitize_claude_resume_argv(
        &mut self,
        command: &mut Value,
        scenario: &str,
    ) -> Result<(), SanitizationError> {
        let Some(args) = command.get_mut("args").and_then(Value::as_array_mut) else {
            return Err(SanitizationError::InvalidClaudeResumeCommand {
                location: "command.args".to_owned(),
            });
        };
        let resume_like: Vec<usize> = args
            .iter()
            .enumerate()
            .filter_map(|(index, arg)| {
                arg.as_str()
                    .is_some_and(|arg| arg.starts_with("--resume"))
                    .then_some(index)
            })
            .collect();
        if scenario != "resume" {
            if resume_like.is_empty() {
                return Ok(());
            }
            return Err(SanitizationError::InvalidClaudeResumeCommand {
                location: "command.args".to_owned(),
            });
        }

        let Some(&index) = resume_like
            .as_slice()
            .first()
            .filter(|_| resume_like.len() == 1)
        else {
            return Err(SanitizationError::InvalidClaudeResumeCommand {
                location: "command.args".to_owned(),
            });
        };
        let Some(raw_session_id) = args[index]
            .as_str()
            .and_then(|arg| arg.strip_prefix("--resume="))
            .filter(|session_id| !session_id.is_empty())
        else {
            return Err(SanitizationError::InvalidClaudeResumeCommand {
                location: "command.args".to_owned(),
            });
        };
        let Some([session]) = self
            .semantics
            .get(&RedactionKind::SessionId)
            .map(Vec::as_slice)
        else {
            return Err(SanitizationError::InvalidClaudeResumeCommand {
                location: "command.args".to_owned(),
            });
        };
        let Some(original) = session.original.as_str().filter(|value| !value.is_empty()) else {
            return Err(SanitizationError::InvalidClaudeResumeCommand {
                location: "command.args".to_owned(),
            });
        };
        if raw_session_id != original {
            return Err(SanitizationError::InvalidClaudeResumeCommand {
                location: "command.args".to_owned(),
            });
        }
        args[index] = Value::String(format!("--resume={}", session.placeholder));
        *self
            .counts
            .entry(RedactionKind::SessionId.manifest_name().to_owned())
            .or_default() += 1;
        Ok(())
    }

    fn sanitize_string(
        &mut self,
        text: &mut String,
        location: &str,
    ) -> Result<(), SanitizationError> {
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
                if path_occurrence_escapes_root(text, &value) {
                    return Err(SanitizationError::UnrecognizedAbsolutePath {
                        location: location.to_owned(),
                    });
                }
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
    if object
        .get("method")
        .and_then(Value::as_str)
        .is_some_and(|method| matches!(method, "turn/start" | "turn/steer"))
    {
        context.codex_turn_input = true;
    }
    if context.json_root && !context.claude_capture {
        context.codex_root_notification = match object.get("method").and_then(Value::as_str) {
            Some("thread/tokenUsage/updated") => CodexNotification::TokenUsage,
            Some("mcpServer/startupStatus/updated") => CodexNotification::McpStartupStatus,
            _ => CodexNotification::None,
        };
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
    if context.claude_capture && object.get("type").and_then(Value::as_str) == Some("tool_use") {
        context.claude_tool_use = true;
    }
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
    if context.claude_memory_paths && value.as_str().is_some_and(|value| !value.is_empty()) {
        return Some(RedactionKind::ClaudeMemoryPath);
    }
    let normalized = normalize_field(key);
    if context.codex_direct_params == CodexNotification::McpStartupStatus
        && normalized == "name"
        && value.as_str().is_some_and(|value| !value.is_empty())
    {
        return Some(RedactionKind::CodexMcpServerName);
    }
    if !context.claude_capture
        && matches!(context.entity, Entity::Thread)
        && normalized == "path"
        && value.as_str().is_some_and(|value| !value.is_empty())
    {
        return Some(RedactionKind::CodexThreadPath);
    }
    match normalized.as_str() {
        "requestid" => return Some(RedactionKind::ClaudeRequestId),
        "sessionid" => return Some(RedactionKind::SessionId),
        "threadid" => return Some(RedactionKind::ThreadId),
        "turnid" | "expectedturnid" => return Some(RedactionKind::TurnId),
        "tooluseid" | "parenttooluseid" | "itemid" => {
            return Some(RedactionKind::ToolUseId);
        }
        "hookid" => return Some(RedactionKind::ToolUseId),
        "uuid" | "pid" | "servername" | "installationid" => {
            return Some(RedactionKind::MachineId);
        }
        "id" if object.contains_key("jsonrpc") => return Some(RedactionKind::CodexRpcId),
        "id" if !context.claude_capture
            && context.json_root
            && !object.contains_key("method")
            && (object.contains_key("result") || object.contains_key("error")) =>
        {
            return Some(RedactionKind::CodexRpcId);
        }
        "id" if matches!(context.entity, Entity::Thread) => {
            return Some(RedactionKind::ThreadId);
        }
        "id" if matches!(context.entity, Entity::Turn) => return Some(RedactionKind::TurnId),
        "id" if matches!(context.entity, Entity::Item) => {
            return Some(RedactionKind::ToolUseId);
        }
        "id" if context.claude_capture
            && object.get("type").and_then(Value::as_str) == Some("message")
            && object.get("role").and_then(Value::as_str) == Some("assistant") =>
        {
            return Some(RedactionKind::ClaudeMessageId);
        }
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
        "signature"
            if context.claude_capture
                && value.as_str().is_some_and(|value| !value.is_empty())
                && object
                    .get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|kind| matches!(kind, "thinking" | "signature_delta")) =>
        {
            return Some(RedactionKind::ClaudeThinkingSignature);
        }
        "prompt" => return Some(RedactionKind::UserText),
        "content" | "text" if matches!(context.speaker, Speaker::User) => {
            return Some(RedactionKind::UserText);
        }
        "content" | "text"
            if matches!(context.speaker, Speaker::Assistant) && !context.claude_tool_input =>
        {
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
        "partialjson"
            if context.claude_capture
                && object.get("type").and_then(Value::as_str) == Some("input_json_delta")
                && value.as_str().is_some_and(|value| !value.is_empty()) =>
        {
            return Some(RedactionKind::AssistantProse);
        }
        "delta" | "textdelta" if context.codex_assistant_prose => {
            return Some(RedactionKind::AssistantProse);
        }
        "thinking" => return Some(RedactionKind::AssistantProse),
        "result" if object.get("type").and_then(Value::as_str) == Some("result") => {
            return Some(RedactionKind::AssistantProse);
        }
        "description" if context.discovery_metadata => {
            return Some(RedactionKind::ProviderProse);
        }
        "argumenthint"
            if context.discovery_metadata
                && value.as_str().is_some_and(|value| !value.is_empty()) =>
        {
            return Some(RedactionKind::ProviderProse);
        }
        "description" if context.codex_catalog => {
            return Some(RedactionKind::ProviderProse);
        }
        "message" if context.availability_nux => {
            return Some(RedactionKind::ProviderProse);
        }
        "message"
            if object.contains_key("level")
                && value.as_str().is_some_and(|value| !value.is_empty()) =>
        {
            return Some(RedactionKind::ProviderProse);
        }
        "output" | "stdout" | "stderr"
            if object.get("subtype").and_then(Value::as_str) == Some("hook_response") =>
        {
            return Some(RedactionKind::ProviderProse);
        }
        _ => {}
    }
    None
}

fn child_context(mut context: SemanticContext, key: &str) -> SemanticContext {
    let normalized = normalize_field(key);
    context.codex_direct_params = if context.json_root && normalized == "params" {
        context.codex_root_notification
    } else {
        CodexNotification::None
    };
    context.codex_root_notification = CodexNotification::None;
    context.json_root = false;
    context.entity = match normalized.as_str() {
        "thread" => Entity::Thread,
        "turn" => Entity::Turn,
        "item" => Entity::Item,
        "turns" if !context.claude_capture && matches!(context.entity, Entity::Thread) => {
            Entity::Turn
        }
        "items" if !context.claude_capture && matches!(context.entity, Entity::Turn) => {
            Entity::Item
        }
        _ => Entity::None,
    };
    if matches!(normalized.as_str(), "commands" | "agents" | "models") {
        context.discovery_metadata = true;
    }
    if normalized == "data" {
        context.codex_catalog = true;
    }
    if normalized == "availabilitynux" {
        context.availability_nux = true;
    }
    if normalized == "memorypaths" {
        context.claude_memory_paths = true;
    }
    if normalized == "input" && context.claude_tool_use {
        context.claude_tool_input = true;
    }
    if !context.claude_capture
        && normalized == "summary"
        && matches!(context.speaker, Speaker::Assistant)
    {
        context.codex_assistant_prose_array = true;
    }
    context
}

fn descendant_context(mut context: SemanticContext) -> SemanticContext {
    context.json_root = false;
    context.codex_root_notification = CodexNotification::None;
    context.codex_direct_params = CodexNotification::None;
    context
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

fn is_secret_field(field: &str, value: &Value) -> bool {
    let normalized = normalize_field(field);
    if is_token_counter_field(&normalized) {
        return !value.is_number();
    }
    normalized == "token"
        || [
            "apitoken",
            "accesstoken",
            "authtoken",
            "bearertoken",
            "idtoken",
            "oauthtoken",
            "personalaccesstoken",
            "refreshtoken",
            "sessiontoken",
            "apikey",
            "clientsecret",
            "privatekey",
            "secretkey",
            "signingkey",
            "credential",
            "credentials",
            "password",
            "authorization",
        ]
        .iter()
        .any(|family| normalized.ends_with(family))
        || normalized == "secret"
}

fn is_codex_token_usage_object(field: &str, value: &Value, context: SemanticContext) -> bool {
    context.codex_direct_params == CodexNotification::TokenUsage
        && normalize_field(field) == "tokenusage"
        && value.is_object()
}

fn is_token_counter_field(field: &str) -> bool {
    matches!(
        field,
        "inputtoken"
            | "inputtokens"
            | "outputtoken"
            | "outputtokens"
            | "cachedinputtokens"
            | "cachecreationinputtokens"
            | "cachereadinputtokens"
            | "reasoningoutputtokens"
            | "maxtoken"
            | "maxtokens"
            | "totaltoken"
            | "totaltokens"
            | "tokencount"
            | "totaltokencount"
            | "tokenusage"
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
                .is_some_and(|character| matches!(character, '/' | '\\' | '"' | '\''));
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

fn path_occurrence_escapes_root(text: &str, root: &str) -> bool {
    let mut cursor = 0;
    while let Some(relative) = text[cursor..].find(root) {
        let end = cursor + relative + root.len();
        let tail = &text[end..];
        if tail.starts_with(['/', '\\']) {
            let path_tail = tail
                .split(|character: char| {
                    character.is_whitespace() || matches!(character, '"' | '\'' | '<' | '>' | '|')
                })
                .next()
                .unwrap_or(tail);
            let mut depth = 0usize;
            for component in path_tail.split(['/', '\\']).filter(|part| !part.is_empty()) {
                match component {
                    "." => {}
                    ".." if depth == 0 => return true,
                    ".." => depth -= 1,
                    _ => depth += 1,
                }
            }
        }
        cursor = end;
    }
    false
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
        if index + 1 < bytes.len()
            && bytes[index] == b'/'
            && bytes[index + 1] == b'/'
            && (index == 0 || !matches!(bytes[index - 1], b':' | b'/'))
        {
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use super::{PublicationLock, publish_staging_pair_with, publish_staging_pair_with_commit};

    /// Break caught: writing events directly into the destination before manifest creation can
    /// leave a mixed or half-written pair and destroy a previously reviewable staging artifact.
    #[test]
    fn staging_pair_publish_preserves_existing_destination_on_second_write_failure() {
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join(".comet-provider-captures/staging");
        let destination = parent.join("scenario");
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::write(destination.join("events.jsonl"), b"old events").unwrap();
        std::fs::write(destination.join("manifest.json"), b"old manifest").unwrap();

        let error = publish_staging_pair_with(&destination, b"new events", b"new manifest", |_| {
            Err(std::io::Error::other("injected second-write failure"))
        })
        .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::Other);
        assert_eq!(
            std::fs::read(destination.join("events.jsonl")).unwrap(),
            b"old events"
        );
        assert_eq!(
            std::fs::read(destination.join("manifest.json")).unwrap(),
            b"old manifest"
        );
        let siblings: Vec<_> = std::fs::read_dir(parent)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(siblings, [std::ffi::OsString::from("scenario")]);
    }

    /// Break caught: backup-and-replace publication mutates reviewed evidence when rerun bytes
    /// differ, and a failed rollback can then lose the original pair.
    #[test]
    fn staging_pair_publish_rejects_a_different_existing_destination() {
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join(".comet-provider-captures/staging");
        let destination = parent.join("scenario");
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::write(destination.join("events.jsonl"), b"reviewed events").unwrap();
        std::fs::write(destination.join("manifest.json"), b"reviewed manifest").unwrap();

        let error = publish_staging_pair_with(
            &destination,
            b"different events",
            b"different manifest",
            |_| Ok(()),
        )
        .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(
            std::fs::read(destination.join("events.jsonl")).unwrap(),
            b"reviewed events"
        );
        assert_eq!(
            std::fs::read(destination.join("manifest.json")).unwrap(),
            b"reviewed manifest"
        );
        let siblings: Vec<_> = std::fs::read_dir(parent)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(siblings, [std::ffi::OsString::from("scenario")]);
    }

    /// Break caught: an identical rerun still swaps directories, violating immutable destination
    /// semantics and exposing a needless Windows rename failure path.
    #[test]
    fn staging_pair_publish_identical_existing_destination_is_a_noop() {
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join(".comet-provider-captures/staging");
        let destination = parent.join("scenario");
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::write(destination.join("events.jsonl"), b"same events").unwrap();
        std::fs::write(destination.join("manifest.json"), b"same manifest").unwrap();

        publish_staging_pair_with_commit(
            &destination,
            b"same events",
            b"same manifest",
            |_| Ok(()),
            |_, _| panic!("identical evidence must not invoke the commit rename"),
        )
        .unwrap();

        assert_eq!(
            std::fs::read(destination.join("events.jsonl")).unwrap(),
            b"same events"
        );
        assert_eq!(
            std::fs::read(destination.join("manifest.json")).unwrap(),
            b"same manifest"
        );
        let siblings: Vec<_> = std::fs::read_dir(parent)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(siblings, [std::ffi::OsString::from("scenario")]);
    }

    /// Break caught: comparing only the expected file bytes accepts a destination containing
    /// unreviewed extra entries as though it were the exact immutable evidence pair.
    #[test]
    fn staging_pair_publish_rejects_identical_pair_with_extra_entry() {
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join(".comet-provider-captures/staging");
        let destination = parent.join("scenario");
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::write(destination.join("events.jsonl"), b"same events").unwrap();
        std::fs::write(destination.join("manifest.json"), b"same manifest").unwrap();
        std::fs::write(destination.join("unreviewed.txt"), b"must remain untouched").unwrap();

        let error =
            publish_staging_pair_with(&destination, b"same events", b"same manifest", |_| Ok(()))
                .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(
            std::fs::read(destination.join("events.jsonl")).unwrap(),
            b"same events"
        );
        assert_eq!(
            std::fs::read(destination.join("manifest.json")).unwrap(),
            b"same manifest"
        );
        assert_eq!(
            std::fs::read(destination.join("unreviewed.txt")).unwrap(),
            b"must remain untouched"
        );
        let siblings: Vec<_> = std::fs::read_dir(parent)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(siblings, [std::ffi::OsString::from("scenario")]);
    }

    /// Break caught: a failed final rename can leave a generated sibling or a partially visible
    /// destination, especially because directory replacement differs across platforms.
    #[test]
    fn staging_pair_publish_cleans_up_after_final_rename_failure() {
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join(".comet-provider-captures/staging");
        let destination = parent.join("scenario");

        let error = publish_staging_pair_with_commit(
            &destination,
            b"complete events",
            b"complete manifest",
            |_| Ok(()),
            |_, _| Err(std::io::Error::other("injected final rename failure")),
        )
        .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::Other);
        assert!(!destination.exists());
        assert_eq!(std::fs::read_dir(parent).unwrap().count(), 0);
    }

    /// Break caught: checking destination existence without serializing publishers lets a second
    /// sanitizer enter preparation and publish while the first is paused before commit.
    #[test]
    fn staging_pair_publish_serializes_concurrent_publishers() {
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join(".comet-provider-captures/staging");
        let destination = parent.join("scenario");
        let (first_entered_tx, first_entered_rx) = mpsc::channel();
        let (release_first_tx, release_first_rx) = mpsc::channel();
        let first_destination = destination.clone();
        let first = thread::spawn(move || {
            publish_staging_pair_with(
                &first_destination,
                b"concurrent events",
                b"concurrent manifest",
                |_| {
                    first_entered_tx.send(()).unwrap();
                    release_first_rx.recv().unwrap();
                    Ok(())
                },
            )
        });
        first_entered_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap();

        let second_hook_ran = Arc::new(AtomicBool::new(false));
        let second_hook_flag = Arc::clone(&second_hook_ran);
        let second_destination = destination.clone();
        let (second_done_tx, second_done_rx) = mpsc::channel();
        let second = thread::spawn(move || {
            let result = publish_staging_pair_with(
                &second_destination,
                b"concurrent events",
                b"concurrent manifest",
                |_| {
                    second_hook_flag.store(true, Ordering::SeqCst);
                    Ok(())
                },
            );
            second_done_tx.send(result).unwrap();
        });
        let second_result = second_done_rx.recv_timeout(Duration::from_secs(2));
        release_first_tx.send(()).unwrap();
        let first_result = first.join().unwrap();
        second.join().unwrap();

        let second_error = second_result
            .expect("a contending publisher must fail without waiting")
            .unwrap_err();
        assert!(matches!(
            second_error.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::AlreadyExists
        ));
        assert!(!second_error.to_string().contains("scenario"));
        assert!(!second_hook_ran.load(Ordering::SeqCst));
        first_result.unwrap();
        assert_eq!(
            std::fs::read(destination.join("events.jsonl")).unwrap(),
            b"concurrent events"
        );
        assert_eq!(
            std::fs::read(destination.join("manifest.json")).unwrap(),
            b"concurrent manifest"
        );
        let siblings: Vec<_> = std::fs::read_dir(parent)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(siblings, [std::ffi::OsString::from("scenario")]);
    }

    /// Break caught: cleanup keyed only by lock pathname can delete a replacement lock that the
    /// publisher did not acquire, stealing ownership from a non-cooperating actor.
    #[test]
    fn publication_lock_cleanup_refuses_changed_ownership() {
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join(".comet-provider-captures/staging");
        std::fs::create_dir_all(&parent).unwrap();
        let lock = PublicationLock::acquire(&parent, "scenario").unwrap();
        let path = parent.join(".scenario.publish.lock");
        std::fs::write(&path, b"replacement owner").unwrap();

        let error = lock.release().unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(std::fs::read(path).unwrap(), b"replacement owner");
    }
}
