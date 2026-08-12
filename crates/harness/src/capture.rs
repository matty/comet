use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail};
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

const CLAUDE_APPROVAL_COMMAND: &str = "printf capture";
const CODEX_APPROVAL_COMMAND: &str = "echo capture";
const APPROVAL_MARKER_NAME: &str = "capture-marker.txt";
const APPROVAL_MARKER_CONTENT: &str = "capture\n";
const APPROVAL_MARKER_ADD_DIFF: &str = APPROVAL_MARKER_CONTENT;

pub fn claude_approval_prompt(cwd: &Path) -> String {
    let marker = cwd.join(APPROVAL_MARKER_NAME);
    format!(
        "Use Bash exactly once with input {{\"command\":{}}}. Wait for it to finish successfully. Then use Write exactly once with input {{\"file_path\":{},\"content\":{}}}.",
        serde_json::to_string(CLAUDE_APPROVAL_COMMAND).expect("static command serializes"),
        serde_json::to_string(&marker.display().to_string()).expect("path serializes"),
        serde_json::to_string(APPROVAL_MARKER_CONTENT).expect("static content serializes"),
    )
}

pub fn codex_approval_prompt(cwd: &Path) -> String {
    let marker = cwd.join(APPROVAL_MARKER_NAME);
    format!(
        "Run the exact command {} three separate times, then add exactly one file at {} containing exactly capture followed by one newline.",
        serde_json::to_string(CODEX_APPROVAL_COMMAND).expect("static command serializes"),
        serde_json::to_string(&marker.display().to_string()).expect("path serializes"),
    )
}

pub fn approval_marker_command(target: &Path) -> String {
    #[cfg(windows)]
    {
        let path = target
            .join("approval-marker.txt")
            .display()
            .to_string()
            .replace('\'', "''");
        format!(
            "powershell.exe -NoProfile -Command \"Set-Content -LiteralPath '{path}' -Value 'capture' -NoNewline\""
        )
    }
    #[cfg(not(windows))]
    {
        let path = target
            .join("approval-marker.txt")
            .display()
            .to_string()
            .replace('\'', "'\\''");
        format!("printf %s capture > '{path}'")
    }
}

pub fn approval_on_request_prompt(target: &Path) -> String {
    format!(
        "Run this exact command once and report success: {}",
        approval_marker_command(target)
    )
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

impl RedactionRoots {
    fn capture(
        command: &CommandSnapshot,
        approval_target: Option<&Path>,
        trusted_powershell: Option<&FileIdentity>,
    ) -> Self {
        let cwd = command.cwd.clone();
        let repo = cwd
            .as_deref()
            .map(Path::new)
            .and_then(repository_root)
            .map(|path| path.to_string_lossy().into_owned());
        Self {
            cwd,
            repo,
            home: crate::home_dir().map(|path| path.to_string_lossy().into_owned()),
            temp: Some(std::env::temp_dir().to_string_lossy().into_owned()),
            codex_home: command.configured_env.get("CODEX_HOME").cloned(),
            approval_target: approval_target.map(|path| path.to_string_lossy().into_owned()),
            trusted_powershell: trusted_powershell
                .map(|identity| identity.canonical.to_string_lossy().into_owned()),
        }
    }
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
enum PartialFailureClass {
    DriverError,
    Timeout,
    ProcessError,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum PartialOutcome {
    Incomplete,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PartialRawCapture {
    schema_version: u64,
    outcome: PartialOutcome,
    failure_class: PartialFailureClass,
    #[serde(flatten)]
    capture: RawCapture,
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

#[derive(Debug, thiserror::Error)]
pub enum CorpusError {
    #[error("provider corpus index could not be read ({kind:?})")]
    IndexRead { kind: std::io::ErrorKind },
    #[error("provider corpus index is not valid schema JSON at line {line}, column {column}")]
    InvalidIndex { line: usize, column: usize },
    #[error("provider corpus index schema version {version} is unsupported; expected version 1")]
    UnsupportedIndexSchemaVersion { version: u64 },
    #[error("provider corpus claim id {claim_id:?} is duplicated")]
    DuplicateClaimId { claim_id: String },
    #[error("provider corpus claim {claim_id:?} has a non-canonical or unsafe manifest path")]
    UnsafeManifestPath { claim_id: String },
    #[error("provider corpus claim {claim_id:?} is missing manifest {manifest}")]
    MissingManifest { claim_id: String, manifest: String },
    #[error(
        "provider corpus manifest {manifest} for claim {claim_id:?} is invalid at line {line}, column {column}"
    )]
    InvalidManifest {
        claim_id: String,
        manifest: String,
        line: usize,
        column: usize,
    },
    #[error(
        "provider corpus manifest {manifest} for claim {claim_id:?} uses unsupported schema version {version}; expected version 1"
    )]
    UnsupportedManifestSchemaVersion {
        claim_id: String,
        manifest: String,
        version: u64,
    },
    #[error(
        "provider corpus manifest {manifest} does not name consumer {consumer:?} for claim {claim_id:?}"
    )]
    MissingManifestConsumer {
        claim_id: String,
        manifest: String,
        consumer: String,
    },
    #[error("provider corpus manifest {manifest} names stale consumer {consumer:?}")]
    ExtraManifestConsumer { manifest: String, consumer: String },
    #[error("provider corpus manifest {manifest} repeats consumer {consumer:?}")]
    DuplicateManifestConsumer { manifest: String, consumer: String },
    #[error("provider corpus claim {claim_id:?} has no evidence entries")]
    MissingEvidence { claim_id: String },
    #[error(
        "provider corpus comparative claim {claim_id:?} selects {total_frames} frames but only {distinct_observations} distinct observations; at least two are required"
    )]
    InsufficientComparisonEvidence {
        claim_id: String,
        total_frames: usize,
        distinct_observations: usize,
    },
    #[error("provider corpus claim {claim_id:?} is missing events beside {manifest}")]
    MissingEvents { claim_id: String, manifest: String },
    #[error("provider corpus event line {line} for claim {claim_id:?} is invalid")]
    InvalidEvent { claim_id: String, line: usize },
    #[error(
        "provider corpus events for claim {claim_id:?} expected sequence {expected}, found {actual}"
    )]
    NonContiguousEventSequence {
        claim_id: String,
        manifest: String,
        expected: u64,
        actual: u64,
    },
    #[error("provider corpus claim {claim_id:?} references missing frame {sequence} in {manifest}")]
    MissingFrame {
        claim_id: String,
        manifest: String,
        sequence: u64,
    },
    #[error(
        "provider corpus claim {claim_id:?} frame {sequence} expects {expected:?}, found {actual:?}"
    )]
    FrameChannelMismatch {
        claim_id: String,
        sequence: u64,
        expected: Channel,
        actual: Channel,
    },
    #[error(
        "provider corpus claim {claim_id:?} contains unresolved placeholder syntax at {location}"
    )]
    UnresolvedPlaceholder {
        claim_id: String,
        location: &'static str,
    },
    #[error("provider corpus manifest {manifest} does not define used placeholder {placeholder}")]
    MissingPlaceholderDefinition {
        claim_id: String,
        manifest: String,
        placeholder: String,
    },
    #[error("provider corpus manifest {manifest} defines unused placeholder {placeholder}")]
    UnusedPlaceholderDefinition {
        claim_id: String,
        manifest: String,
        placeholder: String,
    },
    #[error("provider corpus manifest {manifest} defines placeholder {placeholder} more than once")]
    DuplicatePlaceholderDefinition {
        claim_id: String,
        manifest: String,
        placeholder: String,
    },
    #[error(
        "provider corpus manifest {manifest} defines {placeholder} as {actual_kind:?}, expected {expected_kind:?}"
    )]
    PlaceholderKindMismatch {
        claim_id: String,
        manifest: String,
        placeholder: String,
        expected_kind: String,
        actual_kind: String,
    },
    #[error("provider corpus claim {claim_id:?} was not found")]
    ClaimNotFound { claim_id: String },
    #[error("provider corpus claim {claim_id:?} selects {count} frames; exactly one is required")]
    SelectedFrameCount { claim_id: String, count: usize },
}

#[derive(Clone, Debug, Deserialize)]
struct CorpusIndex {
    schema_version: u64,
    claims: Vec<CorpusClaim>,
}

#[derive(Clone, Debug, Deserialize)]
struct CorpusClaim {
    id: String,
    consumer: String,
    #[serde(default)]
    comparison: bool,
    evidence: Vec<ClaimEvidence>,
    #[allow(dead_code)]
    fact: String,
}

#[derive(Clone, Debug, Deserialize)]
struct ClaimEvidence {
    manifest: String,
    frames: Vec<ClaimFrame>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
struct ClaimFrame {
    sequence: u64,
    channel: Channel,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct CorpusManifest {
    schema_version: u64,
    source: String,
    provider: Provider,
    cli_version: String,
    normalized_cli_version: String,
    captured_at_unix_ms: i64,
    scenario: String,
    purpose: String,
    platform: PlatformMetadata,
    command: CorpusCommand,
    channels: Vec<Channel>,
    exit_code: Option<i32>,
    placeholders: Vec<PlaceholderDefinition>,
    redaction_counts: BTreeMap<String, u64>,
    #[serde(default)]
    consumers: Vec<String>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct CorpusCommand {
    program: String,
    args: Vec<String>,
    cwd: Option<String>,
    configured_env: BTreeMap<String, String>,
    stdin: StdioMode,
    stdout: StdioMode,
    stderr: StdioMode,
    kill_on_drop: bool,
    #[serde(default)]
    creation_flags: Option<u32>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct PlaceholderDefinition {
    placeholder: String,
    kind: String,
}

/// Validate every claim in a checked-in provider corpus without stopping at the first absent
/// evidence file. Index-shape errors are global; artifact errors are returned once per claim.
pub fn validate_corpus(corpus_root: &Path) -> Vec<CorpusError> {
    let index = match read_index(corpus_root) {
        Ok(index) => index,
        Err(error) => return vec![error],
    };

    if let Some(duplicate) = duplicate_claim_id(&index.claims) {
        return vec![CorpusError::DuplicateClaimId {
            claim_id: duplicate,
        }];
    }

    let expected_consumers = expected_manifest_consumers(&index.claims);
    let mut errors = Vec::new();
    for claim in &index.claims {
        if let Err(error) = validate_comparison_frame_count(claim) {
            errors.push(error);
            continue;
        }
        if claim.evidence.is_empty() {
            errors.push(CorpusError::MissingEvidence {
                claim_id: claim.id.clone(),
            });
            continue;
        }
        let errors_before_claim = errors.len();
        for evidence in &claim.evidence {
            if let Err(error) = validate_evidence(
                corpus_root,
                claim,
                evidence,
                expected_consumers
                    .get(&evidence.manifest)
                    .expect("evidence contributes consumer expectations"),
            ) {
                errors.push(error);
            }
        }
        if errors.len() == errors_before_claim
            && let Err(error) = validate_distinct_comparison_observations(claim)
        {
            errors.push(error);
        }
    }
    errors
}

/// Return the provider's literal payload for a claim that references exactly one event frame.
/// The payload is never decoded into or reserialized through a Comet wire type.
/// Corpus-wide comparison policy does not change this exact-one-frame selection contract.
pub fn selected_payload(corpus_root: &Path, claim_id: &str) -> Result<String, CorpusError> {
    let index = read_index(corpus_root)?;
    if let Some(duplicate) = duplicate_claim_id(&index.claims) {
        return Err(CorpusError::DuplicateClaimId {
            claim_id: duplicate,
        });
    }
    let claim = index
        .claims
        .iter()
        .find(|claim| claim.id == claim_id)
        .ok_or_else(|| CorpusError::ClaimNotFound {
            claim_id: claim_id.to_owned(),
        })?;
    let frame_count = claim
        .evidence
        .iter()
        .map(|evidence| evidence.frames.len())
        .sum();
    if frame_count != 1 {
        return Err(CorpusError::SelectedFrameCount {
            claim_id: claim.id.clone(),
            count: frame_count,
        });
    }
    let expected_consumers = expected_manifest_consumers(&index.claims);
    let evidence = claim
        .evidence
        .iter()
        .find(|evidence| !evidence.frames.is_empty())
        .ok_or_else(|| CorpusError::MissingEvidence {
            claim_id: claim.id.clone(),
        })?;
    let frame = evidence.frames[0];
    let events = validate_evidence(
        corpus_root,
        claim,
        evidence,
        expected_consumers
            .get(&evidence.manifest)
            .expect("selected evidence contributes consumer expectations"),
    )?;
    events
        .into_iter()
        .find(|event| event.sequence == frame.sequence)
        .map(|event| event.payload)
        .ok_or_else(|| CorpusError::MissingFrame {
            claim_id: claim.id.clone(),
            manifest: evidence.manifest.clone(),
            sequence: frame.sequence,
        })
}

fn read_index(corpus_root: &Path) -> Result<CorpusIndex, CorpusError> {
    let bytes =
        std::fs::read(corpus_root.join("index.json")).map_err(|source| CorpusError::IndexRead {
            kind: source.kind(),
        })?;
    let value: Value =
        serde_json::from_slice(&bytes).map_err(|source| CorpusError::InvalidIndex {
            line: source.line(),
            column: source.column(),
        })?;
    let version = value
        .get("schema_version")
        .and_then(Value::as_u64)
        .ok_or(CorpusError::InvalidIndex { line: 0, column: 0 })?;
    if version != 1 {
        return Err(CorpusError::UnsupportedIndexSchemaVersion { version });
    }
    let index: CorpusIndex =
        serde_json::from_value(value.clone()).map_err(|source| CorpusError::InvalidIndex {
            line: source.line(),
            column: source.column(),
        })?;
    debug_assert_eq!(index.schema_version, 1);
    let raw_claims = value
        .get("claims")
        .and_then(Value::as_array)
        .ok_or(CorpusError::InvalidIndex { line: 0, column: 0 })?;
    for (claim, raw_claim) in index.claims.iter().zip(raw_claims) {
        let mut ignored_uses = BTreeMap::new();
        if inspect_placeholder_value(raw_claim, &mut ignored_uses).is_err() {
            return Err(CorpusError::UnresolvedPlaceholder {
                claim_id: claim.id.clone(),
                location: "index",
            });
        }
    }
    Ok(index)
}

fn duplicate_claim_id(claims: &[CorpusClaim]) -> Option<String> {
    let mut seen = std::collections::BTreeSet::new();
    claims
        .iter()
        .find(|claim| !seen.insert(claim.id.as_str()))
        .map(|claim| claim.id.clone())
}

fn comparison_evidence_counts(claim: &CorpusClaim) -> (usize, usize) {
    let mut total_frames = 0;
    let mut distinct_observations = std::collections::BTreeSet::new();
    for evidence in &claim.evidence {
        for frame in &evidence.frames {
            total_frames += 1;
            distinct_observations.insert((
                evidence.manifest.as_str(),
                frame.sequence,
                match frame.channel {
                    Channel::Stdin => 0,
                    Channel::Stdout => 1,
                    Channel::Stderr => 2,
                },
            ));
        }
    }
    (total_frames, distinct_observations.len())
}

fn validate_comparison_frame_count(claim: &CorpusClaim) -> Result<(), CorpusError> {
    let (total_frames, distinct_observations) = comparison_evidence_counts(claim);
    if claim.comparison && total_frames < 2 {
        return Err(CorpusError::InsufficientComparisonEvidence {
            claim_id: claim.id.clone(),
            total_frames,
            distinct_observations,
        });
    }
    Ok(())
}

fn validate_distinct_comparison_observations(claim: &CorpusClaim) -> Result<(), CorpusError> {
    let (total_frames, distinct_observations) = comparison_evidence_counts(claim);
    if claim.comparison && distinct_observations < 2 {
        return Err(CorpusError::InsufficientComparisonEvidence {
            claim_id: claim.id.clone(),
            total_frames,
            distinct_observations,
        });
    }
    Ok(())
}

fn expected_manifest_consumers(
    claims: &[CorpusClaim],
) -> BTreeMap<String, BTreeMap<String, String>> {
    let mut expected = BTreeMap::<String, BTreeMap<String, String>>::new();
    for claim in claims {
        for evidence in &claim.evidence {
            expected
                .entry(evidence.manifest.clone())
                .or_default()
                .entry(claim.consumer.clone())
                .or_insert_with(|| claim.id.clone());
        }
    }
    expected
}

fn validate_evidence(
    corpus_root: &Path,
    claim: &CorpusClaim,
    evidence: &ClaimEvidence,
    expected_consumers: &BTreeMap<String, String>,
) -> Result<Vec<CaptureEvent>, CorpusError> {
    if !is_canonical_relative_path(&evidence.manifest) {
        return Err(CorpusError::UnsafeManifestPath {
            claim_id: claim.id.clone(),
        });
    }
    let manifest_path = corpus_root.join(&evidence.manifest);
    let manifest_bytes = match std::fs::read(&manifest_path) {
        Ok(bytes) => bytes,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            return Err(CorpusError::MissingManifest {
                claim_id: claim.id.clone(),
                manifest: evidence.manifest.clone(),
            });
        }
        Err(source) => {
            return Err(CorpusError::InvalidManifest {
                claim_id: claim.id.clone(),
                manifest: evidence.manifest.clone(),
                line: 0,
                column: source.raw_os_error().unwrap_or_default() as usize,
            });
        }
    };
    if !existing_path_stays_below(corpus_root, &manifest_path) {
        return Err(CorpusError::UnsafeManifestPath {
            claim_id: claim.id.clone(),
        });
    }
    let manifest_value: Value =
        serde_json::from_slice(&manifest_bytes).map_err(|source| CorpusError::InvalidManifest {
            claim_id: claim.id.clone(),
            manifest: evidence.manifest.clone(),
            line: source.line(),
            column: source.column(),
        })?;
    let version = manifest_value
        .get("schema_version")
        .and_then(Value::as_u64)
        .ok_or_else(|| CorpusError::InvalidManifest {
            claim_id: claim.id.clone(),
            manifest: evidence.manifest.clone(),
            line: 0,
            column: 0,
        })?;
    if version != 1 {
        return Err(CorpusError::UnsupportedManifestSchemaVersion {
            claim_id: claim.id.clone(),
            manifest: evidence.manifest.clone(),
            version,
        });
    }
    let manifest: CorpusManifest =
        serde_json::from_value(manifest_value.clone()).map_err(|source| {
            CorpusError::InvalidManifest {
                claim_id: claim.id.clone(),
                manifest: evidence.manifest.clone(),
                line: source.line(),
                column: source.column(),
            }
        })?;
    let mut placeholder_uses = BTreeMap::new();
    let mut manifest_payload = manifest_value;
    let mut ignored_definition_uses = BTreeMap::new();
    if manifest_payload
        .get("placeholders")
        .is_some_and(|definitions| {
            inspect_placeholder_value(definitions, &mut ignored_definition_uses).is_err()
        })
    {
        return Err(CorpusError::UnresolvedPlaceholder {
            claim_id: claim.id.clone(),
            location: "manifest",
        });
    }
    if let Some(object) = manifest_payload.as_object_mut() {
        object.remove("placeholders");
    }
    if inspect_placeholder_value(&manifest_payload, &mut placeholder_uses).is_err() {
        return Err(CorpusError::UnresolvedPlaceholder {
            claim_id: claim.id.clone(),
            location: "manifest",
        });
    }
    let mut actual_consumers = BTreeMap::<&str, usize>::new();
    for consumer in &manifest.consumers {
        let count = actual_consumers.entry(consumer).or_default();
        *count += 1;
        if *count > 1 {
            return Err(CorpusError::DuplicateManifestConsumer {
                manifest: evidence.manifest.clone(),
                consumer: consumer.clone(),
            });
        }
    }
    for (consumer, claim_id) in expected_consumers {
        if !actual_consumers.contains_key(consumer.as_str()) {
            return Err(CorpusError::MissingManifestConsumer {
                claim_id: claim_id.clone(),
                manifest: evidence.manifest.clone(),
                consumer: consumer.clone(),
            });
        }
    }
    if let Some(extra) = actual_consumers
        .keys()
        .find(|consumer| !expected_consumers.contains_key(**consumer))
    {
        return Err(CorpusError::ExtraManifestConsumer {
            manifest: evidence.manifest.clone(),
            consumer: (*extra).to_owned(),
        });
    }

    let events_path = manifest_path.with_file_name("events.jsonl");
    let events_bytes = match std::fs::read(&events_path) {
        Ok(bytes) => bytes,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            return Err(CorpusError::MissingEvents {
                claim_id: claim.id.clone(),
                manifest: evidence.manifest.clone(),
            });
        }
        Err(_) => {
            return Err(CorpusError::InvalidEvent {
                claim_id: claim.id.clone(),
                line: 0,
            });
        }
    };
    if !existing_path_stays_below(corpus_root, &events_path) {
        return Err(CorpusError::UnsafeManifestPath {
            claim_id: claim.id.clone(),
        });
    }
    let mut events = Vec::new();
    for (offset, line) in events_bytes.split(|byte| *byte == b'\n').enumerate() {
        if line.is_empty() {
            continue;
        }
        let event_value: Value =
            serde_json::from_slice(line).map_err(|_| CorpusError::InvalidEvent {
                claim_id: claim.id.clone(),
                line: offset + 1,
            })?;
        if inspect_placeholder_value(&event_value, &mut placeholder_uses).is_err() {
            return Err(CorpusError::UnresolvedPlaceholder {
                claim_id: claim.id.clone(),
                location: "events",
            });
        }
        let event: CaptureEvent =
            serde_json::from_value(event_value).map_err(|_| CorpusError::InvalidEvent {
                claim_id: claim.id.clone(),
                line: offset + 1,
            })?;
        if let Ok(payload) = serde_json::from_str::<Value>(&event.payload)
            && inspect_placeholder_value(&payload, &mut placeholder_uses).is_err()
        {
            return Err(CorpusError::UnresolvedPlaceholder {
                claim_id: claim.id.clone(),
                location: "events",
            });
        }
        let expected = events.len() as u64 + 1;
        if event.sequence != expected {
            return Err(CorpusError::NonContiguousEventSequence {
                claim_id: claim.id.clone(),
                manifest: evidence.manifest.clone(),
                expected,
                actual: event.sequence,
            });
        }
        events.push(event);
    }

    validate_placeholder_definitions(claim, evidence, &manifest, &placeholder_uses)?;

    for frame in &evidence.frames {
        let Some(event) = events.iter().find(|event| event.sequence == frame.sequence) else {
            return Err(CorpusError::MissingFrame {
                claim_id: claim.id.clone(),
                manifest: evidence.manifest.clone(),
                sequence: frame.sequence,
            });
        };
        if event.channel != frame.channel {
            return Err(CorpusError::FrameChannelMismatch {
                claim_id: claim.id.clone(),
                sequence: frame.sequence,
                expected: frame.channel,
                actual: event.channel,
            });
        }
    }
    Ok(events)
}

fn is_canonical_relative_path(path: &str) -> bool {
    if path.is_empty()
        || path.contains('\\')
        || path.starts_with('/')
        || path.ends_with('/')
        || path.contains("//")
        || path.contains(':')
    {
        return false;
    }
    let path_value = Path::new(path);
    if path_value.file_name().and_then(|name| name.to_str()) != Some("manifest.json") {
        return false;
    }
    let normalized = path_value
        .components()
        .map(|component| match component {
            std::path::Component::Normal(value) => value.to_string_lossy().into_owned(),
            _ => String::new(),
        })
        .collect::<Vec<_>>();
    !normalized.iter().any(|component| {
        component.is_empty()
            || component.trim_end_matches(['.', ' ']) != component
            || is_windows_reserved_component(component)
    }) && normalized.join("/") == path
}

fn is_windows_reserved_component(component: &str) -> bool {
    let stem = component
        .split_once('.')
        .map_or(component, |(stem, _)| stem)
        .to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || stem
            .strip_prefix("COM")
            .or_else(|| stem.strip_prefix("LPT"))
            .is_some_and(|number| {
                matches!(number, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
            })
}

fn existing_path_stays_below(corpus_root: &Path, path: &Path) -> bool {
    let Ok(root) = std::fs::canonicalize(corpus_root) else {
        return false;
    };
    let Ok(path) = std::fs::canonicalize(path) else {
        return false;
    };
    path.starts_with(root)
}

#[derive(Clone, Copy)]
enum KnownPlaceholder {
    Static(&'static str),
    Typed(&'static str),
}

fn inspect_placeholder_value(
    value: &Value,
    placeholder_uses: &mut BTreeMap<String, String>,
) -> Result<(), ()> {
    match value {
        Value::Array(values) => {
            for value in values {
                inspect_placeholder_value(value, placeholder_uses)?;
            }
        }
        Value::Object(object) => {
            for (key, value) in object {
                inspect_placeholder_text(key, placeholder_uses)?;
                inspect_placeholder_value(value, placeholder_uses)?;
            }
        }
        Value::String(text) => inspect_placeholder_text(text, placeholder_uses)?,
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
    Ok(())
}

fn inspect_placeholder_text(
    text: &str,
    placeholder_uses: &mut BTreeMap<String, String>,
) -> Result<(), ()> {
    if text.contains("{{") || text.contains("${") || text.contains("[REDACTED]") {
        return Err(());
    }
    let mut remaining = text;
    while let Some(start) = remaining.find('<') {
        remaining = &remaining[start + 1..];
        let Some(end) = remaining.find('>') else {
            return if marker_like(remaining) {
                Err(())
            } else {
                Ok(())
            };
        };
        let candidate = &remaining[..end];
        if marker_like(candidate) {
            match known_placeholder(candidate) {
                Some(KnownPlaceholder::Static(kind) | KnownPlaceholder::Typed(kind)) => {
                    placeholder_uses.insert(format!("<{candidate}>"), kind.to_owned());
                }
                None => return Err(()),
            }
        }
        remaining = &remaining[end + 1..];
    }
    Ok(())
}

fn marker_like(candidate: &str) -> bool {
    !candidate.is_empty()
        && !candidate.chars().any(char::is_whitespace)
        && candidate
            .bytes()
            .any(|byte| byte.is_ascii_alphabetic() || matches!(byte, b'_' | b'-'))
}

fn known_placeholder(candidate: &str) -> Option<KnownPlaceholder> {
    let static_kind = match candidate {
        "CWD" => Some("cwd_path"),
        "REPO" => Some("repo_path"),
        "HOME" => Some("home_path"),
        "TEMP" => Some("temp_path"),
        "CODEX_HOME" => Some("codex_home_path"),
        "APPROVAL_TARGET" => Some("approval_target_path"),
        "TRUSTED_POWERSHELL" => Some("trusted_powershell_path"),
        _ => None,
    };
    if let Some(kind) = static_kind {
        return Some(KnownPlaceholder::Static(kind));
    }
    const INDEXED: &[(&str, &str)] = &[
        ("CLAUDE_REQUEST_ID", "claude_request_id"),
        ("CODEX_RPC_ID", "codex_rpc_id"),
        ("SESSION_ID", "session_id"),
        ("THREAD_ID", "thread_id"),
        ("TURN_ID", "turn_id"),
        ("TOOL_USE_ID", "tool_use_id"),
        ("USER_TEXT", "user_text"),
        ("ASSISTANT_PROSE", "assistant_prose"),
        ("PROVIDER_PROSE", "provider_prose"),
        ("MACHINE_ID", "machine_id"),
        ("ATTACHMENT_BYTES", "attachment_bytes"),
        ("CLAUDE_MEMORY_PATH", "claude_memory_path"),
        ("CLAUDE_MESSAGE_ID", "claude_message_id"),
        ("CLAUDE_THINKING_SIGNATURE", "claude_thinking_signature"),
        ("CODEX_MCP_SERVER_NAME", "codex_mcp_server_name"),
        ("CODEX_THREAD_PATH", "codex_thread_path"),
    ];
    INDEXED.iter().find_map(|(prefix, kind)| {
        candidate
            .strip_prefix(prefix)
            .and_then(|suffix| suffix.strip_prefix('_'))
            .filter(|number| {
                number
                    .as_bytes()
                    .first()
                    .is_some_and(|digit| matches!(digit, b'1'..=b'9'))
                    && number.bytes().all(|byte| byte.is_ascii_digit())
            })
            .map(|_| KnownPlaceholder::Typed(kind))
    })
}

fn validate_placeholder_definitions(
    claim: &CorpusClaim,
    evidence: &ClaimEvidence,
    manifest: &CorpusManifest,
    placeholder_uses: &BTreeMap<String, String>,
) -> Result<(), CorpusError> {
    let mut definitions = BTreeMap::<String, String>::new();
    for definition in &manifest.placeholders {
        let candidate = definition
            .placeholder
            .strip_prefix('<')
            .and_then(|value| value.strip_suffix('>'));
        let Some(known) = candidate.and_then(known_placeholder) else {
            return Err(CorpusError::UnresolvedPlaceholder {
                claim_id: claim.id.clone(),
                location: "manifest",
            });
        };
        let expected_kind = match known {
            KnownPlaceholder::Static(kind) | KnownPlaceholder::Typed(kind) => kind,
        };
        if definition.kind != expected_kind {
            return Err(CorpusError::PlaceholderKindMismatch {
                claim_id: claim.id.clone(),
                manifest: evidence.manifest.clone(),
                placeholder: definition.placeholder.clone(),
                expected_kind: expected_kind.to_owned(),
                actual_kind: definition.kind.clone(),
            });
        }
        if definitions
            .insert(definition.placeholder.clone(), definition.kind.clone())
            .is_some()
        {
            return Err(CorpusError::DuplicatePlaceholderDefinition {
                claim_id: claim.id.clone(),
                manifest: evidence.manifest.clone(),
                placeholder: definition.placeholder.clone(),
            });
        }
    }
    if let Some((placeholder, _)) = placeholder_uses
        .iter()
        .find(|(placeholder, _)| !definitions.contains_key(*placeholder))
    {
        return Err(CorpusError::MissingPlaceholderDefinition {
            claim_id: claim.id.clone(),
            manifest: evidence.manifest.clone(),
            placeholder: placeholder.clone(),
        });
    }
    if let Some((placeholder, _)) = definitions
        .iter()
        .find(|(placeholder, _)| !placeholder_uses.contains_key(*placeholder))
    {
        return Err(CorpusError::UnusedPlaceholderDefinition {
            claim_id: claim.id.clone(),
            manifest: evidence.manifest.clone(),
            placeholder: placeholder.clone(),
        });
    }
    Ok(())
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

#[cfg(windows)]
fn has_windows_reparse_point(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt as _;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn has_windows_reparse_point(_metadata: &std::fs::Metadata) -> bool {
    false
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
    if object.get("method").and_then(Value::as_str) == Some("turn/start") {
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
        "turnid" => return Some(RedactionKind::TurnId),
        "tooluseid" | "parenttooluseid" | "itemid" => {
            return Some(RedactionKind::ToolUseId);
        }
        "hookid" => return Some(RedactionKind::ToolUseId),
        "uuid" | "pid" | "servername" | "installationid" => {
            return Some(RedactionKind::MachineId);
        }
        "id" if object.contains_key("jsonrpc") => return Some(RedactionKind::CodexRpcId),
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

#[derive(Clone, Debug, Eq, PartialEq)]
struct DirectoryIdentity {
    canonical: PathBuf,
    #[cfg(windows)]
    volume_serial_number: u32,
    #[cfg(windows)]
    file_index: u64,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

fn directory_identity(path: &Path) -> anyhow::Result<DirectoryIdentity> {
    let link_metadata = std::fs::symlink_metadata(path).map_err(|_| {
        anyhow!("Codex on-request approval target must remain an accessible empty directory.")
    })?;
    if link_metadata.file_type().is_symlink() || has_windows_reparse_point(&link_metadata) {
        bail!("Codex on-request approval target must not be a symbolic link.");
    }
    let canonical = std::fs::canonicalize(path).map_err(|_| {
        anyhow!("Codex on-request approval target must remain an accessible empty directory.")
    })?;
    let metadata = std::fs::metadata(&canonical).map_err(|_| {
        anyhow!("Codex on-request approval target must remain an accessible empty directory.")
    })?;
    if !metadata.is_dir() {
        bail!("Codex on-request approval target must remain an empty directory.");
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use std::os::windows::io::{AsRawHandle, FromRawHandle};
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE,
            FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileInformationByHandle, OPEN_EXISTING,
        };
        let wide: Vec<u16> = canonical.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: `wide` is a live, NUL-terminated UTF-16 path for this call.
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            bail!("Codex on-request approval target identity could not be opened.");
        }
        // SAFETY: ownership of the newly opened handle transfers to `File`.
        let file = unsafe { std::fs::File::from_raw_handle(handle) };
        let mut info = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
        // SAFETY: the live file handle and writable output pointer are valid for this call.
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), info.as_mut_ptr()) } == 0 {
            bail!("Codex on-request approval target identity could not be read.");
        }
        // SAFETY: the successful call initialized the whole structure.
        let info = unsafe { info.assume_init() };
        Ok(DirectoryIdentity {
            canonical,
            volume_serial_number: info.dwVolumeSerialNumber,
            file_index: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        })
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(DirectoryIdentity {
            canonical,
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
}

fn require_empty_approval_target(
    target: &Path,
    expected_identity: Option<&DirectoryIdentity>,
) -> anyhow::Result<DirectoryIdentity> {
    let identity = directory_identity(target)?;
    if expected_identity.is_some_and(|expected| expected != &identity) {
        bail!("Codex on-request approval target changed identity before approval.");
    }
    let mut entries = std::fs::read_dir(target).map_err(|_| {
        anyhow!("Codex on-request approval target must remain an accessible empty directory.")
    })?;
    if entries.next().is_some() {
        bail!("Codex on-request approval target must remain empty before approval.");
    }
    Ok(identity)
}

fn validate_on_request_preflight(
    config: &CaptureConfig,
) -> anyhow::Result<Option<DirectoryIdentity>> {
    let CaptureOperation::Codex(CodexCaptureOperation::Run { request, script }) =
        &config.scenario.operation
    else {
        return Ok(None);
    };
    if !matches!(script, CodexRunScript::ApprovalOnRequest) {
        return Ok(None);
    }
    if request.runtime_mode != comet_proto::RuntimeMode::AutoAcceptEdits
        || request.sandbox != comet_proto::SandboxLevel::WorkspaceWrite
    {
        bail!("Codex on-request capture requires workspace-write/on-request runtime settings.");
    }
    let cwd = Path::new(&request.cwd);
    let cwd = std::fs::canonicalize(cwd).map_err(|_| {
        anyhow!("Codex on-request capture requires an accessible non-repository cwd.")
    })?;
    if repository_root(&cwd).is_some() {
        bail!("Codex on-request capture requires a non-repository, non-worktree cwd.");
    }
    let target = config
        .approval_target
        .as_deref()
        .ok_or_else(|| anyhow!("Codex on-request capture requires a validated approval target."))?;
    let identity = directory_identity(target)?;
    if identity.canonical.starts_with(&cwd) || cwd.starts_with(&identity.canonical) {
        bail!("Codex on-request approval target must remain isolated from the cwd.");
    }
    let temp = std::fs::canonicalize(std::env::temp_dir()).unwrap_or_else(|_| std::env::temp_dir());
    if identity.canonical.starts_with(temp) {
        bail!("Codex on-request approval target must remain outside the system temporary tree.");
    }
    if target.join(".git").is_file() {
        bail!("Codex on-request approval target must not be a linked worktree.");
    }
    require_empty_approval_target(target, Some(&identity))?;
    Ok(Some(identity))
}

fn validate_ordinary_approval_cwd(
    cwd: &Path,
    expected: Option<&DirectoryIdentity>,
    require_marker_absent: bool,
) -> anyhow::Result<DirectoryIdentity> {
    let identity = directory_identity(cwd)
        .map_err(|_| anyhow!("Codex approval capture cwd identity could not be validated."))?;
    if expected.is_some_and(|expected| expected != &identity) {
        bail!("Codex approval capture cwd changed identity during the scenario.");
    }
    let marker = cwd.join(APPROVAL_MARKER_NAME);
    if require_marker_absent && std::fs::symlink_metadata(&marker).is_ok() {
        bail!("Codex approval marker must be absent before file approval.");
    }
    Ok(identity)
}

fn validate_ordinary_approval_marker(cwd: &Path) -> anyhow::Result<()> {
    let marker = cwd.join(APPROVAL_MARKER_NAME);
    let metadata = std::fs::symlink_metadata(&marker)
        .map_err(|_| anyhow!("Codex approval marker was not created."))?;
    if !metadata.file_type().is_file()
        || metadata.file_type().is_symlink()
        || has_windows_reparse_point(&metadata)
    {
        bail!("Codex approval marker was not a regular non-reparse file.");
    }
    let content = std::fs::read_to_string(marker)
        .map_err(|_| anyhow!("Codex approval marker could not be read."))?;
    if content != APPROVAL_MARKER_CONTENT {
        bail!("Codex approval marker did not contain the exact bounded content.");
    }
    Ok(())
}

#[derive(Default)]
struct ClaudeApprovalState {
    request_ids: BTreeSet<String>,
    bash_tool_id: Option<String>,
    bash_succeeded: bool,
    write_tool_id: Option<String>,
    write_input: Option<Value>,
    write_approved: bool,
}

fn validate_claude_marker_input(input: &Value, cwd: &Path) -> anyhow::Result<()> {
    let expected_path = cwd.join(APPROVAL_MARKER_NAME);
    let expected = json!({
        "file_path": expected_path.display().to_string(),
        "content": APPROVAL_MARKER_CONTENT,
    });
    if input != &expected {
        bail!("Claude Write approval request did not match the exact bounded marker.");
    }
    let canonical_cwd = std::fs::canonicalize(cwd)
        .map_err(|_| anyhow!("Claude Write approval request cwd could not be validated."))?;
    let canonical_parent = expected_path
        .parent()
        .and_then(|parent| std::fs::canonicalize(parent).ok())
        .ok_or_else(|| {
            anyhow!("Claude Write approval request marker parent could not be validated.")
        })?;
    if canonical_parent != canonical_cwd {
        bail!("Claude Write approval request escaped the configured cwd.");
    }
    Ok(())
}

fn strict_claude_approval_block(
    message: &crate::claude::wire::MessageFrame,
    is_candidate: impl Fn(&Value) -> bool,
) -> anyhow::Result<Option<crate::claude::wire::ContentBlock>> {
    let Some(raw_blocks) = message.message.content.as_array() else {
        if is_candidate(&message.message.content) {
            bail!("Claude approval capture observed malformed approval message content.");
        }
        return Ok(None);
    };
    if !raw_blocks.iter().any(&is_candidate) {
        return Ok(None);
    }
    if raw_blocks.len() != 1 {
        bail!("Claude approval capture observed extra approval message content.");
    }
    serde_json::from_value(raw_blocks[0].clone())
        .map(Some)
        .map_err(|_| anyhow!("Claude approval capture observed a malformed approval block."))
}

fn observe_claude_approval_frame(
    frame: &crate::claude::wire::Frame,
    cwd: &Path,
    state: &mut ClaudeApprovalState,
) -> anyhow::Result<Option<(String, Value)>> {
    use crate::claude::wire::Frame;

    match frame {
        Frame::Assistant(message) => {
            let Some(block) = strict_claude_approval_block(message, |block| {
                matches!(block["name"].as_str(), Some("Bash" | "Write"))
                    || (block["type"] == "tool_use"
                        && (block["input"].get("command").is_some()
                            || block["input"].get("file_path").is_some()))
            })?
            else {
                return Ok(None);
            };
            if block.kind != "tool_use" || (block.name != "Bash" && block.name != "Write") {
                bail!("Claude approval capture observed a malformed bounded tool use.");
            }
            if message.parent_tool_use_id.is_some()
                || message.message.role != "assistant"
                || block.id.trim().is_empty()
            {
                bail!("Claude approval capture observed a malformed bounded tool use.");
            }
            if block.name == "Bash" {
                if block.input != json!({"command": CLAUDE_APPROVAL_COMMAND}) {
                    bail!("Claude approval capture observed an unexpected Bash command.");
                }
                match state.bash_tool_id.as_deref() {
                    Some(id) if id != block.id => {
                        bail!("Claude approval capture observed duplicate Bash tool uses.")
                    }
                    None => state.bash_tool_id = Some(block.id.clone()),
                    _ => {}
                }
            } else {
                if !state.bash_succeeded {
                    bail!("Claude approval capture observed Write before successful Bash.");
                }
                validate_claude_marker_input(&block.input, cwd)?;
                match state.write_tool_id.as_deref() {
                    Some(_) => {
                        bail!("Claude approval capture observed duplicate Write tool uses.")
                    }
                    None => {
                        state.write_tool_id = Some(block.id.clone());
                        state.write_input = Some(block.input.clone());
                    }
                }
            }
        }
        Frame::User(message) => {
            let Some(bash_id) = state.bash_tool_id.as_deref() else {
                return Ok(None);
            };
            let Some(block) = strict_claude_approval_block(message, |block| {
                block["tool_use_id"] == bash_id
                    || (block["type"] == "tool_result" && block["tool_use_id"] == bash_id)
            })?
            else {
                return Ok(None);
            };
            if message.parent_tool_use_id.is_some()
                || message.message.role != "user"
                || block.kind != "tool_result"
                || block.tool_use_id != bash_id
                || block.is_error != Some(false)
            {
                bail!("Claude approval capture did not observe a successful Bash result.");
            }
            state.bash_succeeded = true;
        }
        Frame::ControlRequest(control) => {
            if control.request_id.trim().is_empty() {
                bail!("Claude approval request had no nonempty request identifier.");
            }
            if !state.request_ids.insert(control.request_id.clone()) {
                bail!("Claude approval request repeated a request identifier.");
            }
            if control.request.subtype != "can_use_tool"
                || control.request.tool_name != "Write"
                || !state.bash_succeeded
                || state.write_approved
                || state.write_tool_id.as_deref() != Some(control.request.tool_use_id.as_str())
                || state.write_input.as_ref() != Some(&control.request.input)
            {
                bail!("Claude approval request used an unexpected tool or order.");
            }
            validate_claude_marker_input(&control.request.input, cwd)?;
            state.write_approved = true;
            return Ok(Some((
                control.request_id.clone(),
                control.request.input.clone(),
            )));
        }
        _ => {}
    }
    Ok(None)
}

#[derive(Default)]
struct CodexApprovalState {
    thread_id: Option<String>,
    turn_id: Option<String>,
    request_ids: BTreeSet<u64>,
    item_ids: BTreeSet<String>,
    command_items: Vec<Value>,
    command_completions: BTreeSet<String>,
    stage: u8,
    file_change_approvals: u8,
    file_items: BTreeMap<String, Value>,
    routine_items: BTreeMap<String, Value>,
}

fn observe_codex_approval_routine_item(
    value: &Value,
    method: &str,
    state: &mut CodexApprovalState,
) -> anyhow::Result<()> {
    require_exact_keys(
        value,
        &["method", "params", "emittedAtMs"],
        "routine item notification",
    )?;
    if value["emittedAtMs"].as_u64().is_none() {
        bail!("Codex routine item notification had no numeric emission time.");
    }
    validate_codex_approval_context(value, state)?;
    require_exact_keys(
        &value["params"],
        &[
            "item",
            "threadId",
            "turnId",
            if method == "item/started" {
                "startedAtMs"
            } else {
                "completedAtMs"
            },
        ],
        "routine item event",
    )?;
    let item = &value["params"]["item"];
    let kind = item["type"].as_str().unwrap_or_default();
    let keys: &[&str] = match kind {
        "userMessage" => &["type", "id", "clientId", "content"],
        "reasoning" => &["type", "id", "summary", "content"],
        "agentMessage" => &["type", "id", "text", "phase", "memoryCitation"],
        _ => bail!("Codex approval capture observed an unreviewed item type."),
    };
    require_exact_keys(item, keys, "routine item")?;
    let id = item["id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| anyhow!("Codex approval routine item had no nonempty identifier."))?;
    match kind {
        "userMessage" => {
            if item["clientId"].as_str().is_none() || !item["content"].is_array() {
                bail!("Codex approval user-message item had an unexpected value shape.");
            }
        }
        "reasoning" => {
            if !item["summary"].is_array() || !item["content"].is_array() {
                bail!("Codex approval reasoning item had an unexpected value shape.");
            }
        }
        "agentMessage" => {
            if item["text"].as_str().is_none()
                || item["phase"] != "commentary"
                || !item["memoryCitation"].is_null()
            {
                bail!("Codex approval agent-message item had an unexpected value shape.");
            }
        }
        _ => unreachable!("kind was matched above"),
    }
    if method == "item/started" {
        if (kind == "reasoning"
            && item["summary"]
                .as_array()
                .is_none_or(|summary| !summary.is_empty()))
            || (kind == "agentMessage" && item["text"] != "")
        {
            bail!("Codex approval routine item start was not in its reviewed initial state.");
        }
        if state
            .routine_items
            .insert(id.to_owned(), item.clone())
            .is_some()
        {
            bail!("Codex approval routine item repeated its start.");
        }
    } else {
        let Some(started) = state.routine_items.remove(id) else {
            bail!("Codex approval routine item completion did not join its start.");
        };
        let preserved = match kind {
            "userMessage" => started == *item,
            "reasoning" => {
                started["type"] == item["type"]
                    && started["id"] == item["id"]
                    && started["content"] == item["content"]
                    && item["summary"]
                        .as_array()
                        .is_some_and(|summary| !summary.is_empty())
            }
            "agentMessage" => {
                started["type"] == item["type"]
                    && started["id"] == item["id"]
                    && started["phase"] == item["phase"]
                    && started["memoryCitation"] == item["memoryCitation"]
                    && item["text"].as_str().is_some_and(|text| !text.is_empty())
            }
            _ => unreachable!("kind was matched above"),
        };
        if !preserved {
            bail!("Codex approval routine item completion changed a reviewed invariant.");
        }
    }
    Ok(())
}

fn validate_codex_approval_context(
    value: &Value,
    state: &CodexApprovalState,
) -> anyhow::Result<()> {
    let thread_id = value["params"]["threadId"]
        .as_str()
        .filter(|id| !id.is_empty());
    let turn_id = value["params"]["turnId"]
        .as_str()
        .filter(|id| !id.is_empty());
    if thread_id != state.thread_id.as_deref() || turn_id != state.turn_id.as_deref() {
        bail!("Codex approval event changed the active thread or turn identifier.");
    }
    Ok(())
}

fn validate_codex_approval_lifecycle(
    value: &Value,
    state: &CodexApprovalState,
) -> anyhow::Result<()> {
    require_exact_keys(
        value,
        &["method", "params", "emittedAtMs"],
        "turn lifecycle notification",
    )?;
    if value["emittedAtMs"].as_u64().is_none() {
        bail!("Codex turn lifecycle had no numeric emission time.");
    }
    require_exact_keys(&value["params"], &["threadId", "turn"], "turn lifecycle")?;
    let turn = &value["params"]["turn"];
    require_exact_keys(
        turn,
        &[
            "id",
            "items",
            "itemsView",
            "status",
            "error",
            "startedAt",
            "completedAt",
            "durationMs",
        ],
        "turn lifecycle value",
    )?;
    if value["params"]["threadId"].as_str() != state.thread_id.as_deref()
        || turn["id"].as_str() != state.turn_id.as_deref()
    {
        bail!("Codex approval lifecycle changed the active thread or turn identifier.");
    }
    if !turn["items"].is_array()
        || turn["itemsView"] != "summary"
        || turn["status"] != "completed"
        || !turn["error"].is_null()
        || turn["startedAt"].as_u64().is_none()
        || turn["completedAt"].as_u64().is_none()
        || turn["durationMs"].as_u64().is_none()
    {
        bail!("Codex approval terminal lifecycle did not match the reviewed completed shape.");
    }
    Ok(())
}

fn observe_codex_approval_turn_started(
    value: &Value,
    expected_thread_id: &str,
    state: &mut CodexApprovalState,
) -> anyhow::Result<String> {
    if state.thread_id.is_some() || state.turn_id.is_some() {
        bail!("Codex approval capture repeated turn/started.");
    }
    require_exact_keys(
        value,
        &["method", "params", "emittedAtMs"],
        "turn lifecycle notification",
    )?;
    if value["emittedAtMs"].as_u64().is_none() {
        bail!("Codex turn lifecycle had no numeric emission time.");
    }
    require_exact_keys(&value["params"], &["threadId", "turn"], "turn lifecycle")?;
    let turn = &value["params"]["turn"];
    require_exact_keys(
        turn,
        &[
            "id",
            "items",
            "itemsView",
            "status",
            "error",
            "startedAt",
            "completedAt",
            "durationMs",
        ],
        "turn lifecycle value",
    )?;
    let thread_id = value["params"]["threadId"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| anyhow!("Codex approval turn had no thread identifier."))?;
    let turn_id = turn["id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| anyhow!("Codex approval turn had no nonempty turn identifier."))?;
    if thread_id != expected_thread_id
        || turn["items"]
            .as_array()
            .is_none_or(|items| !items.is_empty())
        || turn["itemsView"] != "notLoaded"
        || turn["status"] != "inProgress"
        || !turn["error"].is_null()
        || turn["startedAt"].as_u64().is_none()
        || !turn["completedAt"].is_null()
        || !turn["durationMs"].is_null()
    {
        bail!("Codex approval turn/started did not match the reviewed initial shape.");
    }
    state.thread_id = Some(thread_id.to_owned());
    state.turn_id = Some(turn_id.to_owned());
    Ok(turn_id.to_owned())
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FileIdentity {
    canonical: PathBuf,
    #[cfg(windows)]
    volume_serial_number: u32,
    #[cfg(windows)]
    file_index: u64,
}

#[cfg(windows)]
fn file_identity(path: &Path) -> anyhow::Result<FileIdentity> {
    let link_metadata = std::fs::symlink_metadata(path)
        .map_err(|_| anyhow!("The trusted PowerShell executable could not be inspected."))?;
    if !link_metadata.file_type().is_file()
        || link_metadata.file_type().is_symlink()
        || has_windows_reparse_point(&link_metadata)
    {
        bail!("The trusted PowerShell executable must be a regular file.");
    }
    let canonical = std::fs::canonicalize(path)
        .map_err(|_| anyhow!("The trusted PowerShell executable could not be resolved."))?;
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use std::os::windows::io::{AsRawHandle, FromRawHandle};
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_SHARE_DELETE, FILE_SHARE_READ,
            FILE_SHARE_WRITE, GetFileInformationByHandle, OPEN_EXISTING,
        };
        let wide: Vec<u16> = canonical.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: `wide` is a live, NUL-terminated UTF-16 path for this call.
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            bail!("The trusted PowerShell executable identity could not be opened.");
        }
        // SAFETY: ownership of the newly opened handle transfers to `File`.
        let file = unsafe { std::fs::File::from_raw_handle(handle) };
        let mut info = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
        // SAFETY: the live file handle and writable output pointer are valid for this call.
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), info.as_mut_ptr()) } == 0 {
            bail!("The trusted PowerShell executable identity could not be read.");
        }
        // SAFETY: the successful call initialized the whole structure.
        let info = unsafe { info.assume_init() };
        Ok(FileIdentity {
            canonical,
            volume_serial_number: info.dwVolumeSerialNumber,
            file_index: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        })
    }
    #[cfg(not(windows))]
    {
        Ok(FileIdentity { canonical })
    }
}

#[cfg(windows)]
fn canonical_protected_roots<'a>(
    roots: impl IntoIterator<Item = Option<&'a Path>>,
) -> anyhow::Result<Vec<PathBuf>> {
    let mut canonical = Vec::new();
    for root in roots.into_iter().flatten() {
        let root = std::fs::canonicalize(root).map_err(|_| {
            anyhow!("A configured Windows system root could not be validated for capture.")
        })?;
        if !canonical.contains(&root) {
            canonical.push(root);
        }
    }
    if canonical.is_empty() {
        bail!("Windows system installation roots could not be found for approval capture.");
    }
    Ok(canonical)
}

#[cfg(windows)]
fn windows_protected_roots() -> anyhow::Result<Vec<PathBuf>> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{
        FOLDERID_ProgramFiles, FOLDERID_ProgramFilesX86, FOLDERID_Windows, SHGetKnownFolderPath,
    };
    if usize::BITS != 64 {
        bail!("Windows approval capture is only reviewed for 64-bit hosts.");
    }
    let mut known = Vec::new();
    for folder in [
        &FOLDERID_Windows,
        &FOLDERID_ProgramFiles,
        &FOLDERID_ProgramFilesX86,
    ] {
        let mut raw = std::ptr::null_mut();
        // SAFETY: the known-folder GUID is static and `raw` is a writable out pointer.
        let status = unsafe { SHGetKnownFolderPath(folder, 0, std::ptr::null_mut(), &mut raw) };
        if status >= 0 && !raw.is_null() {
            let mut len = 0;
            // SAFETY: successful API output is a NUL-terminated UTF-16 allocation.
            while unsafe { *raw.add(len) } != 0 {
                len += 1;
            }
            // SAFETY: the allocation contains `len` initialized code units.
            known.push(PathBuf::from(OsString::from_wide(unsafe {
                std::slice::from_raw_parts(raw, len)
            })));
        }
        // SAFETY: SHGetKnownFolderPath documents CoTaskMemFree ownership for every nonnull
        // output, including a buffer returned alongside failure.
        if !raw.is_null() {
            unsafe { CoTaskMemFree(raw.cast()) };
        }
    }
    if known.len() != 3 {
        bail!("Windows protected installation roots could not be resolved for approval capture.");
    }
    let roots = canonical_protected_roots([
        Some(known[0].as_path()),
        Some(known[1].as_path()),
        Some(known[2].as_path()),
    ])?;
    if roots.len() != 3 {
        bail!("Windows protected installation roots resolved inconsistently.");
    }
    Ok(roots)
}

#[cfg(windows)]
fn select_trusted_powershell(
    candidates: &[PathBuf],
    protected_roots: &[PathBuf],
    forbidden_roots: &[PathBuf],
) -> anyhow::Result<FileIdentity> {
    candidates
        .iter()
        .filter_map(|candidate| file_identity(candidate).ok())
        .find(|identity| {
            protected_roots
                .iter()
                .any(|root| identity.canonical.starts_with(root))
                && !forbidden_roots
                    .iter()
                    .any(|root| identity.canonical.starts_with(root))
        })
        .ok_or_else(|| {
            anyhow!(
                "Codex approval capture requires PowerShell from a protected Windows system root."
            )
        })
}

#[cfg(windows)]
fn resolve_trusted_powershell(cwd: &Path, raw_root: &Path) -> anyhow::Result<FileIdentity> {
    let mut paths = Vec::new();
    if let Some(path) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&path));
    }
    if let Some(path) = crate::shell_env::system_path() {
        paths.extend(std::env::split_paths(path));
    }
    let candidates: Vec<_> = paths
        .into_iter()
        .map(|dir| dir.join("pwsh.exe"))
        .filter(|path| path.is_file())
        .collect();
    let protected_roots = windows_protected_roots()?;
    let mut forbidden_roots = vec![
        std::fs::canonicalize(cwd)
            .map_err(|_| anyhow!("Codex approval capture cwd could not be validated."))?,
    ];
    forbidden_roots
        .push(std::fs::canonicalize(std::env::temp_dir()).unwrap_or_else(|_| std::env::temp_dir()));
    if let Some(home) = crate::home_dir().and_then(|path| std::fs::canonicalize(path).ok()) {
        forbidden_roots.push(home);
    }
    if let Ok(path) = std::path::absolute(raw_root) {
        forbidden_roots.push(std::fs::canonicalize(&path).unwrap_or(path));
    }
    // These roots establish the capture trust boundary. ACL ownership inference is deliberately
    // avoided; every observed launcher is still reopened and compared by file identity.
    select_trusted_powershell(&candidates, &protected_roots, &forbidden_roots)
}

#[cfg(not(windows))]
fn resolve_trusted_powershell(_cwd: &Path, _raw_root: &Path) -> anyhow::Result<FileIdentity> {
    bail!(
        "Codex approval capture has no observed safe Unix launcher contract. Review real evidence and update the design before retrying."
    )
}

#[derive(Default)]
struct CodexOnRequestState {
    request_ids: BTreeSet<u64>,
    failed_item_id: Option<String>,
    stage: u8,
}

fn codex_approval_ids(
    value: &Value,
    request_ids: &mut BTreeSet<u64>,
    item_ids: &mut BTreeSet<String>,
) -> anyhow::Result<(u64, String)> {
    let request_id = value["id"].as_u64().ok_or_else(|| {
        anyhow!("Codex approval request had no valid numeric request identifier.")
    })?;
    if !request_ids.insert(request_id) {
        bail!("Codex approval request repeated a request identifier.");
    }
    let item_id = value["params"]["itemId"]
        .as_str()
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| anyhow!("Codex approval request had no nonempty item identifier."))?
        .to_owned();
    if !item_ids.insert(item_id.clone()) {
        bail!("Codex approval request repeated an item identifier.");
    }
    Ok((request_id, item_id))
}

fn validate_command_action(params: &Value, expected_command: &str) -> anyhow::Result<()> {
    let actions = params["commandActions"]
        .as_array()
        .ok_or_else(|| anyhow!("Codex command approval request had no bounded command action."))?;
    let expected = json!({"type": "unknown", "command": expected_command});
    if actions.len() != 1 || actions[0] != expected {
        bail!("Codex command approval request used an unexpected command action.");
    }
    Ok(())
}

fn validate_codex_raw_launcher(raw: &str, trusted: &FileIdentity) -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        let Some(rest) = raw.strip_prefix('"') else {
            bail!("Codex command execution used an unexpected raw launcher.");
        };
        let Some((path, suffix)) = rest.split_once('"') else {
            bail!("Codex command execution used an unexpected raw launcher.");
        };
        if suffix != " -Command 'echo capture'" {
            bail!("Codex command execution used an unexpected raw launcher.");
        }
        let observed = file_identity(Path::new(path))?;
        if &observed != trusted {
            bail!("Codex command execution used an untrusted launcher identity.");
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = (raw, trusted);
        bail!(
            "Codex approval capture has no observed safe Unix launcher contract. Review real evidence and update the design before retrying."
        );
    }
}

fn validate_marker_change(item: &Value, cwd: &Path) -> anyhow::Result<()> {
    require_exact_keys(
        item,
        &["type", "id", "changes", "status"],
        "file-change item",
    )?;
    if item["type"] != "fileChange" || item["status"] != "inProgress" {
        bail!("Codex file-change approval request did not join an active file change.");
    }
    let changes = item["changes"]
        .as_array()
        .ok_or_else(|| anyhow!("Codex file-change approval request had no bounded change."))?;
    if changes.len() != 1 {
        bail!("Codex file-change approval request had an unexpected number of changes.");
    }
    let change = &changes[0];
    require_exact_keys(change, &["path", "kind", "diff"], "file-change add")?;
    let expected_path = cwd.join(APPROVAL_MARKER_NAME);
    if change["path"].as_str() != Some(expected_path.to_string_lossy().as_ref())
        || change["kind"] != json!({"type": "add"})
        || change["diff"] != APPROVAL_MARKER_ADD_DIFF
    {
        bail!("Codex file-change approval request did not match the exact bounded marker add.");
    }
    let canonical_cwd = std::fs::canonicalize(cwd)
        .map_err(|_| anyhow!("Codex file-change approval request cwd could not be validated."))?;
    let canonical_parent = expected_path
        .parent()
        .and_then(|parent| std::fs::canonicalize(parent).ok())
        .ok_or_else(|| {
            anyhow!("Codex file-change approval request marker parent could not be validated.")
        })?;
    if canonical_parent != canonical_cwd {
        bail!("Codex file-change approval request escaped the configured cwd.");
    }
    Ok(())
}

fn require_exact_keys(value: &Value, expected: &[&str], label: &str) -> anyhow::Result<()> {
    let Some(object) = value.as_object() else {
        bail!("Codex {label} was not an object.");
    };
    let actual: BTreeSet<_> = object.keys().map(String::as_str).collect();
    let expected: BTreeSet<_> = expected.iter().copied().collect();
    if actual != expected {
        bail!("Codex {label} had unexpected or missing fields.");
    }
    Ok(())
}

fn validate_codex_approval_command_event(
    value: &Value,
    method: &str,
    cwd: &Path,
    trusted_powershell: &FileIdentity,
    state: &mut CodexApprovalState,
) -> anyhow::Result<()> {
    require_exact_keys(
        value,
        &["method", "params", "emittedAtMs"],
        "command notification",
    )?;
    if value["emittedAtMs"].as_u64().is_none() {
        bail!("Codex command notification had no numeric emission time.");
    }
    validate_codex_approval_context(value, state)?;
    require_exact_keys(
        &value["params"],
        &[
            "item",
            "threadId",
            "turnId",
            if method == "item/started" {
                "startedAtMs"
            } else {
                "completedAtMs"
            },
        ],
        "command event",
    )?;
    let item = &value["params"]["item"];
    require_exact_keys(
        item,
        &[
            "type",
            "id",
            "pluginId",
            "scriptPath",
            "command",
            "cwd",
            "processId",
            "source",
            "status",
            "commandActions",
            "aggregatedOutput",
            "exitCode",
            "durationMs",
        ],
        "command item",
    )?;
    let id = item["id"]
        .as_str()
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| anyhow!("Codex command execution had no nonempty item identifier."))?;
    if item["type"] != "commandExecution"
        || item["cwd"].as_str() != Some(cwd.to_string_lossy().as_ref())
    {
        bail!("Codex command execution did not match the bounded cwd and item type.");
    }
    validate_command_action(item, CODEX_APPROVAL_COMMAND)?;
    let raw = item["command"]
        .as_str()
        .ok_or_else(|| anyhow!("Codex command execution had no raw launcher."))?;
    validate_codex_raw_launcher(raw, trusted_powershell)?;

    let start_at = match state.stage {
        0..=2 => Some(state.stage as usize),
        5 => Some(3),
        7 => Some(4),
        _ => None,
    };
    if method == "item/started" {
        let Some(index) = start_at else {
            bail!("Codex command executions did not match the reviewed ordering.");
        };
        if item["status"] != "inProgress"
            || item.get("exitCode").is_some_and(|code| !code.is_null())
            || state
                .command_items
                .iter()
                .any(|existing| existing["id"].as_str() == Some(id))
        {
            bail!("Codex command execution start was duplicated or malformed.");
        }
        if state.command_items.len() != index {
            bail!("Codex command executions did not match the reviewed ordering.");
        }
        state.command_items.push(item.clone());
        state.stage += 1;
        return Ok(());
    }

    let expected_index = match state.stage {
        3 => 2,
        4 => 0,
        6 => 3,
        8 => 4,
        _ => bail!("Codex command completions did not match the reviewed ordering."),
    };
    let started = state.command_items.get(expected_index).ok_or_else(|| {
        anyhow!("Codex command completion did not join a preceding bounded start.")
    })?;
    if started["id"].as_str() != Some(id)
        || item["status"] != "failed"
        || item["exitCode"].as_i64() != Some(-1)
        || started["command"] != item["command"]
        || started["cwd"] != item["cwd"]
        || started["commandActions"] != item["commandActions"]
        || !state.command_completions.insert(id.to_owned())
    {
        bail!("Codex command completion did not match its exact bounded start.");
    }
    state.stage += 1;
    Ok(())
}

fn observe_codex_approval_file_item(
    value: &Value,
    cwd: &Path,
    state: &mut CodexApprovalState,
) -> anyhow::Result<()> {
    require_exact_keys(
        value,
        &["method", "params", "emittedAtMs"],
        "file-change notification",
    )?;
    if value["emittedAtMs"].as_u64().is_none() {
        bail!("Codex file-change notification had no numeric emission time.");
    }
    validate_codex_approval_context(value, state)?;
    require_exact_keys(
        &value["params"],
        &["item", "threadId", "turnId", "startedAtMs"],
        "file-change start",
    )?;
    if state.stage != 9 || !state.file_items.is_empty() {
        bail!("Codex file change did not match the reviewed ordering.");
    }
    let item = &value["params"]["item"];
    validate_marker_change(item, cwd)?;
    let item_id = item["id"]
        .as_str()
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| anyhow!("Codex file-change item preceding approval had no identifier."))?
        .to_owned();
    state.file_items.insert(item_id, item.clone());
    state.stage = 10;
    Ok(())
}

fn validate_codex_approval_request(
    value: &Value,
    method: &str,
    cwd: &Path,
    state: &mut CodexApprovalState,
) -> anyhow::Result<()> {
    require_exact_keys(
        value,
        &["id", "method", "params"],
        "file-change approval request",
    )?;
    validate_codex_approval_context(value, state)?;
    require_exact_keys(
        &value["params"],
        &[
            "threadId",
            "turnId",
            "itemId",
            "startedAtMs",
            "reason",
            "grantRoot",
        ],
        "file-change approval request",
    )?;
    if method != "item/fileChange/requestApproval"
        || state.stage != 10
        || state.file_change_approvals != 0
    {
        bail!("Codex approval request had an unexpected method, order, or count.");
    }
    let (request_id, item_id) =
        codex_approval_ids(value, &mut state.request_ids, &mut state.item_ids)?;
    if request_id != 0 {
        bail!("Codex file-change approval request did not use the reviewed numeric identifier.");
    }
    let item = state.file_items.get(&item_id).ok_or_else(|| {
        anyhow!("Codex file-change approval request did not join a preceding item/started.")
    })?;
    validate_marker_change(item, cwd)?;
    state.file_change_approvals = 1;
    state.stage = 11;
    Ok(())
}

fn validate_on_request_item(
    value: &Value,
    expected_command: &str,
    state: &mut CodexOnRequestState,
) -> anyhow::Result<()> {
    let item = &value["params"]["item"];
    if item["type"] != "commandExecution" || item["command"] != expected_command {
        bail!(
            "Codex on-request approval request command item did not match the exact bounded command."
        );
    }
    let item_id = item["id"]
        .as_str()
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| anyhow!("Codex on-request command item had no nonempty identifier."))?;
    let exit_code = item["exitCode"].as_i64();
    match state.stage {
        0 if item["status"] == "failed" && exit_code.is_some_and(|code| code != 0) => {
            state.failed_item_id = Some(item_id.to_owned());
            state.stage = 1;
        }
        2 if item["status"] == "completed"
            && exit_code == Some(0)
            && state.failed_item_id.as_deref() == Some(item_id) =>
        {
            state.stage = 3;
        }
        _ => bail!("Codex on-request command events arrived out of order or changed identity."),
    }
    Ok(())
}

fn validate_codex_on_request_approval(
    value: &Value,
    method: &str,
    expected_command: &str,
    state: &mut CodexOnRequestState,
) -> anyhow::Result<()> {
    if method != "item/commandExecution/requestApproval" {
        bail!("Codex on-request approval request had an unexpected method.");
    }
    let request_id = value["id"]
        .as_u64()
        .ok_or_else(|| anyhow!("Codex on-request approval request had no valid identifier."))?;
    if !state.request_ids.insert(request_id) {
        bail!("Codex on-request approval request repeated its request identifier.");
    }
    if state.stage != 1 {
        bail!("Codex on-request approval request arrived before the sandbox failure.");
    }
    if value["params"]["itemId"].as_str() != state.failed_item_id.as_deref()
        || value["params"]["command"] != expected_command
    {
        bail!("Codex on-request approval request changed the failed command or item.");
    }
    validate_command_action(&value["params"], expected_command)?;
    let reason = value["params"]["reason"]
        .as_str()
        .or_else(|| value["params"]["failureReason"].as_str())
        .unwrap_or_default();
    if reason.trim().is_empty() {
        bail!("Codex on-request approval request had no sandbox-failure reason.");
    }
    state.stage = 2;
    Ok(())
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
    captured_at_unix_ms: i64,
    scenario: String,
    purpose: String,
    command: CommandSnapshot,
    approval_target: Option<PathBuf>,
    approval_target_identity: Option<DirectoryIdentity>,
    approval_cwd_identity: Option<DirectoryIdentity>,
    trusted_powershell: Option<FileIdentity>,
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    stdout_lines: mpsc::UnboundedReceiver<String>,
    readers: Vec<tokio::task::JoinHandle<()>>,
    events: Arc<Mutex<Vec<CaptureEvent>>>,
    #[cfg(test)]
    reap_notice: Option<std::sync::mpsc::SyncSender<u32>>,
    #[cfg(test)]
    wait_error_once: bool,
}

impl RecordingSession {
    async fn start(mut config: CaptureConfig) -> anyhow::Result<Self> {
        let approval_target_identity = validate_on_request_preflight(&config)?;
        if let CaptureOperation::Codex(CodexCaptureOperation::Run { request, script }) =
            &mut config.scenario.operation
        {
            let approval_on_request = matches!(*script, CodexRunScript::ApprovalOnRequest);
            *request = crate::codex::normalize_run_request(request.clone());
            if approval_on_request
                && (request.runtime_mode != comet_proto::RuntimeMode::AutoAcceptEdits
                    || request.sandbox != comet_proto::SandboxLevel::WorkspaceWrite)
            {
                bail!(
                    "Codex on-request capture must remain workspace-write/on-request after production normalization."
                );
            }
        }
        let provider = match &config.scenario.operation {
            CaptureOperation::Claude(_) => Provider::Claude,
            CaptureOperation::Codex(_) => Provider::Codex,
        };
        let (trusted_powershell, approval_cwd_identity) = match &config.scenario.operation {
            CaptureOperation::Codex(CodexCaptureOperation::Run {
                request,
                script: CodexRunScript::Approval,
            }) => {
                let cwd = Path::new(&request.cwd);
                (
                    Some(resolve_trusted_powershell(cwd, &config.raw_root)?),
                    Some(validate_ordinary_approval_cwd(cwd, None, true)?),
                )
            }
            _ => (None, None),
        };
        let executable = resolve_executable(provider, config.executable.as_ref())?;
        let launch = select_launch(&config, &executable)?;
        let command = CommandSnapshot::from_launch(&launch);
        let cli_version = probe_version(&executable).await;
        let captured_at_unix_ms = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|_| {
                    anyhow!("The system clock is before the Unix epoch. Correct it and retry.")
                })?
                .as_millis(),
        )
        .map_err(|_| anyhow!("The system clock is outside the supported capture range."))?;
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

        let spawn_identity = validate_on_request_preflight(&config)?;
        if spawn_identity != approval_target_identity {
            bail!("Codex on-request approval target changed identity before provider spawn.");
        }

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
            captured_at_unix_ms,
            scenario: config.scenario.name.into(),
            purpose: config.scenario.purpose.into(),
            command,
            approval_target: config.approval_target,
            approval_target_identity,
            approval_cwd_identity,
            trusted_powershell,
            child: Some(child),
            stdin: Some(stdin),
            stdout_lines,
            readers: vec![stdout_reader, stderr_reader],
            events,
            #[cfg(test)]
            reap_notice: None,
            #[cfg(test)]
            wait_error_once: false,
        })
    }

    #[cfg(test)]
    fn child_id(&self) -> Option<u32> {
        self.child.as_ref().and_then(Child::id)
    }

    async fn finish(&mut self) -> anyhow::Result<RawCapture> {
        let operation = self.operation.clone();
        let mut drive_completed = false;
        let outcome = tokio::time::timeout(self.timeout, async {
            self.drive(operation).await?;
            drive_completed = true;
            self.stdin.take();
            self.wait_for_exit().await
        })
        .await;
        let exit_code = match outcome {
            Ok(Ok(exit_code)) => exit_code,
            Ok(Err(err)) => {
                self.terminate_and_reap().await;
                let failure_class = if drive_completed {
                    PartialFailureClass::ProcessError
                } else {
                    PartialFailureClass::DriverError
                };
                self.persist_partial_after_failure(failure_class).await;
                return Err(err);
            }
            Err(_) => {
                self.terminate_and_reap().await;
                self.persist_partial_after_failure(PartialFailureClass::Timeout)
                    .await;
                bail!(
                    "Capture timed out after {} seconds. The provider was stopped; retry with --timeout-seconds up to 300.",
                    self.timeout.as_secs_f64()
                );
            }
        };
        self.finish_readers().await;
        let capture = self.raw_capture(exit_code);
        persist_raw_capture(&capture).await?;
        Ok(capture)
    }

    fn raw_capture(&self, exit_code: Option<i32>) -> RawCapture {
        RawCapture {
            directory: self.directory.clone(),
            provider: self.provider,
            cli_version: self.cli_version.clone(),
            captured_at_unix_ms: self.captured_at_unix_ms,
            scenario: self.scenario.clone(),
            purpose: self.purpose.clone(),
            platform: PlatformMetadata {
                os: std::env::consts::OS.into(),
                arch: std::env::consts::ARCH.into(),
            },
            redaction_roots: RedactionRoots::capture(
                &self.command,
                self.approval_target.as_deref(),
                self.trusted_powershell.as_ref(),
            ),
            command: self.command.clone(),
            events: self.events.lock().expect("capture event lock").clone(),
            exit_code,
        }
    }

    async fn persist_partial_after_failure(&self, failure_class: PartialFailureClass) {
        let partial = PartialRawCapture {
            schema_version: 1,
            outcome: PartialOutcome::Incomplete,
            failure_class,
            capture: self.raw_capture(None),
        };
        if let Err(err) = persist_partial_raw_capture(&partial).await {
            tracing::debug!(%err, "partial raw capture persistence failed");
        }
    }

    async fn drive(&mut self, operation: CaptureOperation) -> anyhow::Result<()> {
        match operation {
            CaptureOperation::Claude(ClaudeCaptureOperation::ModelDiscovery)
            | CaptureOperation::Claude(ClaudeCaptureOperation::ModelDiscoveryAt { .. })
            | CaptureOperation::Claude(ClaudeCaptureOperation::CommandDiscovery { .. }) => {
                self.claude_initialize().await
            }
            CaptureOperation::Claude(ClaudeCaptureOperation::Run { request, script }) => {
                self.claude_run(request, script).await
            }
            CaptureOperation::Codex(CodexCaptureOperation::ModelDiscovery)
            | CaptureOperation::Codex(CodexCaptureOperation::ModelDiscoveryAt { .. }) => {
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
        let mut approval = ClaudeApprovalState::default();
        while let Some(line) = self.next_stdout().await? {
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if matches!(script, ClaudeRunScript::Approval) {
                let frame = crate::claude::wire::parse_frame(&line)
                    .map_err(|_| anyhow!("Claude approval capture received malformed JSON."))?;
                if value["type"] == "control_response"
                    && approval.bash_tool_id.is_some()
                    && !approval.bash_succeeded
                {
                    bail!("Claude approval capture observed an approval response for Bash.");
                }
                if let Some((request_id, original_input)) =
                    observe_claude_approval_frame(&frame, Path::new(&request.cwd), &mut approval)?
                {
                    let response = json!({
                        "type": "control_response",
                        "response": {
                            "subtype": "success",
                            "request_id": request_id,
                            "response": {
                                "behavior": "allow",
                                "updatedInput": original_input,
                            },
                        },
                    });
                    self.write_line(&response.to_string()).await?;
                }
            }
            if value["type"] == "result" {
                if value["subtype"] != "success" {
                    bail!("Claude ended the capture without a successful terminal result.");
                }
                if matches!(script, ClaudeRunScript::Approval)
                    && (!approval.bash_succeeded || !approval.write_approved)
                {
                    bail!(
                        "Claude approval capture did not observe the exact successful Bash and bounded Write approval."
                    );
                }
                if matches!(script, ClaudeRunScript::Resume)
                    && value["session_id"].as_str() != request.resume.as_deref()
                {
                    bail!("Claude resume capture returned a different session identifier.");
                }
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
        let thread_reply = self.codex_reply(next_id).await?;
        next_id += 1;
        if thread_reply.get("error").is_some() && method == "thread/resume" {
            bail!(
                "Codex rejected the requested thread resume; no fresh-thread fallback was recorded."
            );
        }
        let thread_id = thread_reply["result"]["thread"]["id"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        if thread_id.is_empty() {
            return protocol_stopped("Codex", "thread identifier");
        }
        if matches!(script, CodexRunScript::Resume)
            && request.resume.as_deref() != Some(thread_id.as_str())
        {
            bail!("Codex resume capture returned a different thread identifier.");
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
        let mut scripted_reply_ok = false;
        let mut scripted_request_id = None;
        let mut approval = CodexApprovalState::default();
        let mut on_request = CodexOnRequestState::default();
        let expected_on_request_command = if matches!(script, CodexRunScript::ApprovalOnRequest) {
            Some(approval_marker_command(
                self.approval_target.as_deref().ok_or_else(|| {
                    anyhow!("Codex on-request capture has no validated approval target.")
                })?,
            ))
        } else {
            None
        };
        while let Some(line) = self.next_stdout().await? {
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let method = value["method"].as_str().unwrap_or_default();
            if method == "turn/started" {
                if matches!(script, CodexRunScript::Approval) {
                    active_turn = Some(observe_codex_approval_turn_started(
                        &value,
                        &thread_id,
                        &mut approval,
                    )?);
                } else {
                    active_turn = value["params"]["turn"]["id"].as_str().map(str::to_owned);
                }
            }
            if matches!(script, CodexRunScript::Approval)
                && matches!(method, "item/started" | "item/completed")
            {
                match value["params"]["item"]["type"].as_str() {
                    Some("commandExecution") => validate_codex_approval_command_event(
                        &value,
                        method,
                        Path::new(&request.cwd),
                        self.trusted_powershell.as_ref().ok_or_else(|| {
                            anyhow!("Codex approval capture has no trusted PowerShell identity.")
                        })?,
                        &mut approval,
                    )?,
                    Some("fileChange") if method == "item/started" => {
                        observe_codex_approval_file_item(
                            &value,
                            Path::new(&request.cwd),
                            &mut approval,
                        )?
                    }
                    Some("fileChange") => {
                        bail!("Codex approval capture observed an extra file-change action.")
                    }
                    Some("userMessage" | "reasoning" | "agentMessage") => {
                        observe_codex_approval_routine_item(&value, method, &mut approval)?
                    }
                    _ => bail!("Codex approval capture observed an unreviewed item type."),
                }
            }
            if !scripted_action_sent {
                match script {
                    CodexRunScript::Steer if active_turn.is_some() => {
                        scripted_request_id = Some(next_id);
                        self.write_line(&rpc_request(
                            next_id,
                            "turn/steer",
                            crate::codex::turn_steer_params(
                                &thread_id,
                                active_turn.as_deref().unwrap_or_default(),
                                "Capture steering message.",
                            ),
                        ))
                        .await?;
                        next_id += 1;
                        scripted_action_sent = true;
                    }
                    CodexRunScript::Interruption if active_turn.is_some() => {
                        scripted_request_id = Some(next_id);
                        self.write_line(&rpc_request(
                            next_id,
                            "turn/interrupt",
                            crate::codex::turn_interrupt_params(
                                &thread_id,
                                active_turn.as_deref().unwrap_or_default(),
                            ),
                        ))
                        .await?;
                        next_id += 1;
                        scripted_action_sent = true;
                    }
                    _ => {}
                }
            }
            if value["id"].as_u64() == scripted_request_id {
                if value.get("error").is_some() {
                    bail!("Codex rejected the scripted steer or interruption request.");
                }
                scripted_reply_ok = true;
            }
            if matches!(
                script,
                CodexRunScript::Approval | CodexRunScript::ApprovalOnRequest
            ) && method.ends_with("/requestApproval")
            {
                if matches!(script, CodexRunScript::Approval) {
                    validate_codex_approval_request(
                        &value,
                        method,
                        Path::new(&request.cwd),
                        &mut approval,
                    )?;
                    validate_ordinary_approval_cwd(
                        Path::new(&request.cwd),
                        self.approval_cwd_identity.as_ref(),
                        true,
                    )?;
                } else {
                    validate_codex_on_request_approval(
                        &value,
                        method,
                        expected_on_request_command
                            .as_deref()
                            .expect("on-request command configured"),
                        &mut on_request,
                    )?;
                    let target = self.approval_target.as_deref().ok_or_else(|| {
                        anyhow!("Codex on-request capture has no validated approval target.")
                    })?;
                    require_empty_approval_target(target, self.approval_target_identity.as_ref())?;
                }
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
            if matches!(script, CodexRunScript::ApprovalOnRequest)
                && method == "item/completed"
                && value["params"]["item"]["type"] == "commandExecution"
            {
                validate_on_request_item(
                    &value,
                    expected_on_request_command
                        .as_deref()
                        .expect("on-request command configured"),
                    &mut on_request,
                )?;
            }
            if matches!(method, "turn/completed" | "turn/failed" | "turn/aborted") {
                if matches!(script, CodexRunScript::Approval) {
                    validate_codex_approval_lifecycle(&value, &approval)?;
                }
                match script {
                    CodexRunScript::Approval => {
                        validate_ordinary_approval_cwd(
                            Path::new(&request.cwd),
                            self.approval_cwd_identity.as_ref(),
                            false,
                        )?;
                        if method != "turn/completed"
                            || approval.stage != 11
                            || approval.command_items.len() != 5
                            || approval.command_completions.len() != 4
                            || approval.file_change_approvals != 1
                            || !approval.routine_items.is_empty()
                        {
                            bail!(
                                "Codex approval capture did not complete after the exact bounded command phase, file-change approval, and marker write."
                            );
                        }
                        validate_ordinary_approval_marker(Path::new(&request.cwd))?;
                    }
                    CodexRunScript::ApprovalOnRequest => {
                        if method != "turn/completed" || on_request.stage != 3 {
                            bail!(
                                "Codex on-request approval did not complete the required failure, approval, retry sequence."
                            );
                        }
                        let target = self.approval_target.as_deref().ok_or_else(|| {
                            anyhow!("Codex on-request capture has no validated approval target.")
                        })?;
                        let marker = tokio::fs::read_to_string(target.join("approval-marker.txt"))
                            .await
                            .map_err(|_| {
                                anyhow!(
                                    "Codex on-request capture did not create its bounded marker."
                                )
                            })?;
                        if marker != "capture" {
                            bail!(
                                "Codex on-request capture marker did not contain the expected value."
                            );
                        }
                    }
                    CodexRunScript::Steer => {
                        if method != "turn/completed"
                            || !scripted_action_sent
                            || !scripted_reply_ok
                            || value["params"]["turn"]["id"].as_str() != active_turn.as_deref()
                        {
                            bail!(
                                "Codex steer capture did not receive a successful steer reply before completion."
                            );
                        }
                    }
                    CodexRunScript::Interruption => {
                        if method != "turn/aborted"
                            || !scripted_action_sent
                            || !scripted_reply_ok
                            || value["params"]["turn"]["id"].as_str() != active_turn.as_deref()
                        {
                            bail!(
                                "Codex interruption capture did not receive a successful interrupt reply and aborted terminal event."
                            );
                        }
                    }
                    _ if method != "turn/completed" => {
                        bail!("Codex capture ended without a successful terminal turn.");
                    }
                    _ => {}
                }
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
        #[cfg(test)]
        if std::mem::take(&mut self.wait_error_once) {
            bail!("The provider ended but its exit status could not be read. Retry the capture.");
        }
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
        if let Some(mut child) = self.child.take() {
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
        CaptureOperation::Claude(ClaudeCaptureOperation::ModelDiscoveryAt { cwd }) => Ok(
            crate::claude::discovery::model_discovery_launch(executable, cwd),
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
        CaptureOperation::Codex(CodexCaptureOperation::ModelDiscoveryAt { cwd }) => {
            let home = config
                .codex_home
                .clone()
                .or_else(crate::codex::discovery::codex_home)
                .ok_or_else(|| {
                    anyhow!("Codex home could not be found. Pass --codex-home and try again.")
                })?;
            let home = absolute_from_parent(home)?;
            Ok(crate::codex::discovery::discovery_launch(
                executable, &home, cwd,
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

async fn persist_partial_raw_capture(capture: &PartialRawCapture) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec_pretty(capture)
        .map_err(|_| anyhow!("partial raw evidence could not be prepared"))?;
    let directory = capture.capture.directory.clone();
    tokio::task::spawn_blocking(move || persist_immutable_bytes(&directory, &bytes))
        .await
        .map_err(|_| anyhow!("partial raw evidence writer stopped"))??;
    Ok(())
}

fn persist_immutable_bytes(directory: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    let temporary = directory.join(".partial-capture.json.tmp");
    let destination = directory.join("partial-capture.json");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    let result = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::hard_link(&temporary, &destination)?;
        Ok(())
    })();
    let _ = std::fs::remove_file(&temporary);
    result
}

async fn claude_user_line(request: &RunRequest, script: ClaudeRunScript) -> anyhow::Result<String> {
    let images = crate::claude::load_image_blocks(&request.attachments).await;
    if matches!(script, ClaudeRunScript::Attachment) && images.is_empty() {
        bail!(
            "The selected attachment could not be inlined. Use a supported image under 5 MiB and retry."
        );
    }
    Ok(crate::claude::wire::user_message_line_with_images(
        &request.prompt,
        &images,
    ))
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
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    use comet_proto::{ReasoningLevel, RunRequest, RuntimeMode, SandboxLevel};
    use serde_json::{Value, json};

    use super::{
        APPROVAL_MARKER_NAME, CaptureConfig, CaptureOperation, CaptureScenario, Channel,
        ClaudeCaptureOperation, ClaudeRunScript, CodexCaptureOperation, CodexRunScript,
        CommandSnapshot, LaunchDescriptor, Provider, PublicationLock, RecordingSession, StdioMode,
        persist_immutable_bytes, publish_staging_pair_with, publish_staging_pair_with_commit,
        record, sanitize_dir,
    };

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

    fn isolated_tempdir(prefix: &str) -> tempfile::TempDir {
        let current = std::env::current_dir().expect("current test directory");
        let parent = current
            .ancestors()
            .find(|path| super::repository_root(path).is_none())
            .expect("an ancestor outside the repository");
        tempfile::Builder::new()
            .prefix(prefix)
            .tempdir_in(parent)
            .expect("isolated test directory")
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
            approval_target: None,
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

    async fn failed_session_stdin(config: CaptureConfig) -> (String, Vec<String>) {
        let mut session = RecordingSession::start(config).await.unwrap();
        let result = session.finish().await;
        let stdin = session
            .events
            .lock()
            .expect("capture event lock")
            .iter()
            .filter(|event| event.channel == Channel::Stdin)
            .map(|event| event.payload.clone())
            .collect();
        (
            result
                .expect_err("unsafe scenario must fail before approval")
                .to_string(),
            stdin,
        )
    }

    fn contains_response_id(lines: &[String], id: u64) -> bool {
        lines.iter().any(|line| {
            serde_json::from_str::<serde_json::Value>(line)
                .ok()
                .is_some_and(|value| value["id"].as_u64() == Some(id))
        })
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
        assert_eq!(capture.redaction_roots.cwd, capture.command.cwd);
        assert_eq!(
            capture.redaction_roots.home,
            crate::home_dir().map(|path| path.to_string_lossy().into_owned())
        );
        assert_eq!(
            capture.redaction_roots.temp,
            Some(std::env::temp_dir().to_string_lossy().into_owned())
        );
        let persisted: serde_json::Value =
            serde_json::from_slice(&std::fs::read(capture.directory.join("capture.json")).unwrap())
                .unwrap();
        assert_eq!(persisted["platform"]["os"], std::env::consts::OS);
        assert_eq!(persisted["platform"]["arch"], std::env::consts::ARCH);
        assert_eq!(persisted["scenario"], "claude-platform");
        assert_eq!(persisted["purpose"], "local recorder test");
        assert!(persisted["captured_at_unix_ms"].as_i64().is_some());
        assert_eq!(
            persisted["redaction_roots"]["cwd"],
            json!(capture.command.cwd)
        );
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

        let writes = channel_payloads(&capture, Channel::Stdin);
        assert_eq!(writes.len(), 1);
        assert_eq!(
            serde_json::from_str::<Value>(writes[0]).unwrap(),
            json!({
                "type": "user",
                "message": {"role": "user", "content": "scenario:happy"},
                "parent_tool_use_id": null,
            })
        );
        assert_eq!(capture.exit_code, Some(0));
    }

    #[tokio::test]
    async fn capture_attachment_line_uses_the_production_image_helpers() {
        let temp = tempfile::tempdir().unwrap();
        let image = temp.path().join("tiny.png");
        std::fs::write(&image, b"\x89PNG\r\n\x1a\n").unwrap();
        let request = RunRequest {
            prompt: "describe".into(),
            attachments: vec![image.display().to_string()],
            ..RunRequest::for_session(RuntimeMode::AutoAcceptEdits)
        };
        let production_images = crate::claude::load_image_blocks(&request.attachments).await;
        assert_eq!(
            super::claude_user_line(&request, ClaudeRunScript::Attachment)
                .await
                .unwrap(),
            crate::claude::wire::user_message_line_with_images(&request.prompt, &production_images)
        );
    }

    #[tokio::test]
    async fn claude_attachment_capture_requires_inline_image_before_text() {
        let raw = tempfile::tempdir().unwrap();
        let files = tempfile::tempdir().unwrap();
        let image = files.path().join("tiny.png");
        std::fs::write(&image, b"\x89PNG\r\n\x1a\n").unwrap();
        let request = RunRequest {
            prompt: "scenario:attachment".into(),
            attachments: vec![image.display().to_string()],
            ..RunRequest::for_session(RuntimeMode::AutoAcceptEdits)
        };
        let capture = record(config(
            "claude-attachment",
            fixture_path("fake-claude"),
            CaptureOperation::Claude(ClaudeCaptureOperation::Run {
                request,
                script: ClaudeRunScript::Attachment,
            }),
            raw.path(),
        ))
        .await
        .unwrap();
        let first: serde_json::Value =
            serde_json::from_str(channel_payloads(&capture, Channel::Stdin)[0]).unwrap();
        assert_eq!(first["message"]["content"][0]["type"], "image");
        assert_eq!(first["message"]["content"][1]["type"], "text");
    }

    #[test]
    fn capture_steer_and_interrupt_params_match_production_helpers() {
        assert_eq!(
            crate::codex::turn_steer_params("thread", "turn", "Capture steering message."),
            json!({"threadId":"thread","expectedTurnId":"turn","input":[{"type":"text","text":"Capture steering message."}]})
        );
        assert_eq!(
            crate::codex::turn_interrupt_params("thread", "turn"),
            json!({"threadId":"thread","turnId":"turn"})
        );
    }

    /// Break caught: the Codex run driver skips a handshake stage, loses the concrete run script,
    /// or waits forever after the provider's terminal turn notification.
    #[tokio::test]
    async fn recorder_codex_run_records_the_explicit_script() {
        let raw = tempfile::tempdir().unwrap();
        let request = RunRequest {
            prompt: "scenario:capture-fresh".into(),
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

    #[tokio::test]
    async fn claude_approval_requires_observed_bash_then_one_write_approval() {
        let raw = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let request = RunRequest {
            prompt: "scenario:capture-approval".into(),
            cwd: cwd.path().display().to_string(),
            ..RunRequest::for_session(RuntimeMode::ApprovalRequired)
        };
        let capture = record(config(
            "claude-approval",
            fixture_path("fake-claude"),
            CaptureOperation::Claude(ClaudeCaptureOperation::Run {
                request,
                script: ClaudeRunScript::Approval,
            }),
            raw.path(),
        ))
        .await
        .unwrap();
        let replies: Vec<serde_json::Value> = channel_payloads(&capture, Channel::Stdin)
            .into_iter()
            .skip(1)
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(replies.len(), 1);
        assert!(replies.iter().all(|reply| {
            reply["response"]["response"]["behavior"] == "allow"
                && reply["response"]["response"]["updatedInput"].is_object()
        }));
    }

    #[tokio::test]
    async fn claude_approval_rejects_destructive_requests_before_replying() {
        for prompt in [
            "scenario:capture-approval-destructive-command",
            "scenario:capture-approval-destructive-write",
        ] {
            let raw = tempfile::tempdir().unwrap();
            let cwd = tempfile::tempdir().unwrap();
            let request = RunRequest {
                prompt: prompt.into(),
                cwd: cwd.path().display().to_string(),
                ..RunRequest::for_session(RuntimeMode::ApprovalRequired)
            };
            let (error, stdin) = failed_session_stdin(config(
                "claude-approval-adversarial",
                fixture_path("fake-claude"),
                CaptureOperation::Claude(ClaudeCaptureOperation::Run {
                    request,
                    script: ClaudeRunScript::Approval,
                }),
                raw.path(),
            ))
            .await;
            assert_eq!(
                stdin.len(),
                1,
                "an unsafe request received an allow response: {stdin:?}"
            );
            assert!(error.contains("approval request"), "{error}");
        }
    }

    #[tokio::test]
    async fn claude_approval_rejects_deviations_from_the_observed_safe_contract() {
        for (scenario, expected_replies) in [
            ("missing-bash", 0),
            ("write-before-bash", 0),
            ("failed-bash", 0),
            ("wrong-bash", 0),
            ("duplicate-bash", 0),
            ("bash-control-response", 0),
            ("bash-malformed-extra", 0),
            ("bash-leading-text", 0),
            ("bash-trailing-text", 0),
            ("write-malformed-extra", 0),
            ("write-leading-text", 0),
            ("write-trailing-text", 0),
            ("user-malformed-extra", 0),
            ("user-leading-text", 0),
            ("user-trailing-text", 0),
            ("malformed-candidate", 0),
            ("missing-write", 0),
            ("duplicate-write", 1),
            ("missing-request-id", 0),
            ("duplicate-request-id", 1),
            ("extra-tool", 0),
        ] {
            let raw = tempfile::tempdir().unwrap();
            let cwd = tempfile::tempdir().unwrap();
            let request = RunRequest {
                prompt: format!("scenario:capture-approval-{scenario}"),
                cwd: cwd.path().display().to_string(),
                ..RunRequest::for_session(RuntimeMode::ApprovalRequired)
            };
            let (error, stdin) = failed_session_stdin(config(
                "claude-approval-deviation",
                fixture_path("fake-claude"),
                CaptureOperation::Claude(ClaudeCaptureOperation::Run {
                    request,
                    script: ClaudeRunScript::Approval,
                }),
                raw.path(),
            ))
            .await;
            let expected_error = match scenario {
                "bash-malformed-extra"
                | "bash-leading-text"
                | "bash-trailing-text"
                | "write-malformed-extra"
                | "write-leading-text"
                | "write-trailing-text"
                | "user-malformed-extra"
                | "user-leading-text"
                | "user-trailing-text" => "extra approval message content",
                "malformed-candidate" => "malformed approval block",
                _ => "Claude approval",
            };
            assert!(error.contains(expected_error), "{scenario}: {error}");
            assert_eq!(
                stdin.len().saturating_sub(1),
                expected_replies,
                "{scenario} received an unsafe response: {stdin:?}"
            );
        }
    }

    #[tokio::test]
    async fn claude_approval_tolerates_a_repeated_snapshot_of_the_same_bash_tool() {
        let raw = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let request = RunRequest {
            prompt: "scenario:capture-approval-bash-snapshot-duplicate".into(),
            cwd: cwd.path().display().to_string(),
            ..RunRequest::for_session(RuntimeMode::ApprovalRequired)
        };
        let capture = record(config(
            "claude-approval-snapshot",
            fixture_path("fake-claude"),
            CaptureOperation::Claude(ClaudeCaptureOperation::Run {
                request,
                script: ClaudeRunScript::Approval,
            }),
            raw.path(),
        ))
        .await
        .unwrap();
        assert_eq!(
            channel_payloads(&capture, Channel::Stdin).len(),
            2,
            "only the Write request receives a reply"
        );
    }

    /// Break caught: a fail-closed approval deviation is discarded after a paid provider run,
    /// leaving no reviewable transcript even though the child was safely stopped before reply.
    #[tokio::test]
    async fn recorder_quarantines_partial_approval_evidence_after_cleanup() {
        let raw = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let request = RunRequest {
            prompt: "scenario:capture-approval-unexpected-second".into(),
            cwd: cwd.path().display().to_string(),
            ..RunRequest::for_session(RuntimeMode::ApprovalRequired)
        };
        let mut session = RecordingSession::start(config(
            "claude-approval-partial",
            fixture_path("fake-claude"),
            CaptureOperation::Claude(ClaudeCaptureOperation::Run {
                request,
                script: ClaudeRunScript::Approval,
            }),
            raw.path(),
        ))
        .await
        .unwrap();
        let pid = session.child_id().expect("spawned child id");
        let directory = session.directory.clone();

        let error = session.finish().await.unwrap_err();

        assert_eq!(
            error.to_string(),
            "Claude approval request used an unexpected tool or order."
        );
        assert!(!process_is_live(pid), "provider child {pid} remains live");
        assert!(!cwd.path().join(APPROVAL_MARKER_NAME).exists());
        assert!(!directory.join("capture.json").exists());
        let partial_path = directory.join("partial-capture.json");
        let partial: Value =
            serde_json::from_slice(&std::fs::read(&partial_path).expect("partial raw evidence"))
                .unwrap();
        assert_eq!(partial["schema_version"], 1);
        assert_eq!(partial["outcome"], "incomplete");
        assert_eq!(partial["failure_class"], "driver_error");
        let events = partial["events"].as_array().unwrap();
        assert!(events.iter().any(|event| {
            event["channel"] == "stdout"
                && event["payload"].as_str().is_some_and(|payload| {
                    payload.contains("bad-read") && payload.contains("capture-marker.txt")
                })
        }));
        assert!(events.iter().any(|event| {
            event["channel"] == "stdin"
                && event["payload"]
                    .as_str()
                    .and_then(|payload| serde_json::from_str::<Value>(payload).ok())
                    .is_some_and(|payload| {
                        payload["response"]["request_id"] == "good-write"
                            && payload["response"]["response"]["behavior"] == "allow"
                    })
        }));
        assert!(!events.iter().any(|event| {
            event["channel"] == "stdin"
                && event["payload"]
                    .as_str()
                    .and_then(|payload| serde_json::from_str::<Value>(payload).ok())
                    .is_some_and(|payload| payload["response"]["request_id"] == "bad-read")
        }));

        let staging = raw
            .path()
            .join(".comet-provider-captures/staging/incomplete");
        let sanitize_error = sanitize_dir(&directory, &staging).unwrap_err();
        assert!(
            matches!(&sanitize_error, super::SanitizationError::IncompleteCapture),
            "unexpected sanitizer error: {sanitize_error}"
        );
        assert!(!staging.exists());
    }

    /// Break caught: a directory containing a successful-looking `capture.json` can bypass the
    /// quarantine marker and publish incomplete evidence.
    #[tokio::test]
    async fn sanitizer_rejects_partial_capture_even_beside_complete_shaped_raw() {
        let raw = tempfile::tempdir().unwrap();
        let mut capture = record(config(
            "claude-model-discovery",
            fixture_path("fake-claude"),
            CaptureOperation::Claude(ClaudeCaptureOperation::ModelDiscovery),
            raw.path(),
        ))
        .await
        .unwrap();
        capture.command.program = "fake-claude".into();
        std::fs::write(
            capture.directory.join("capture.json"),
            serde_json::to_vec_pretty(&capture).unwrap(),
        )
        .unwrap();
        let partial = super::PartialRawCapture {
            schema_version: 1,
            outcome: super::PartialOutcome::Incomplete,
            failure_class: super::PartialFailureClass::DriverError,
            capture: capture.clone(),
        };
        std::fs::write(
            capture.directory.join("partial-capture.json"),
            serde_json::to_vec_pretty(&partial).unwrap(),
        )
        .unwrap();
        let staging = raw.path().join(".comet-provider-captures/staging/mixed");

        let error = sanitize_dir(&capture.directory, &staging).unwrap_err();

        assert!(
            matches!(&error, super::SanitizationError::IncompleteCapture),
            "partial evidence bypassed explicit rejection: {error:?}"
        );
        assert!(!staging.exists());
    }

    /// Break caught: retrying persistence can overwrite the first failure transcript or expose a
    /// half-written JSON document under its final name.
    #[test]
    fn partial_capture_publication_is_atomic_and_immutable() {
        let directory = tempfile::Builder::new()
            .prefix("comet partial evidence ' ")
            .tempdir()
            .unwrap();
        persist_immutable_bytes(directory.path(), br#"{"first":true}"#).unwrap();
        let destination = directory.path().join("partial-capture.json");
        assert_eq!(std::fs::read(&destination).unwrap(), br#"{"first":true}"#);
        assert!(!directory.path().join(".partial-capture.json.tmp").exists());

        let error = persist_immutable_bytes(directory.path(), br#"{"second":true}"#).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(destination).unwrap(), br#"{"first":true}"#);
        assert!(!directory.path().join(".partial-capture.json.tmp").exists());
    }

    /// Break caught: setup/probe/spawn errors manufacture an incomplete provider transcript even
    /// though no provider process and therefore no observed protocol frame existed.
    #[tokio::test]
    async fn recorder_failure_before_spawn_creates_no_partial_capture() {
        let raw = tempfile::tempdir().unwrap();
        let missing = raw.path().join("missing-provider-executable");
        let result = record(config(
            "claude-pre-spawn-failure",
            missing,
            CaptureOperation::Claude(ClaudeCaptureOperation::ModelDiscovery),
            raw.path(),
        ))
        .await;
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("could not be started")
        );
        assert!(!find_named_file(raw.path(), "partial-capture.json"));
    }

    /// Break caught: failure to quarantine evidence replaces the safe protocol error with a raw
    /// storage error that may disclose a local path or provider value.
    #[tokio::test]
    async fn partial_persistence_failure_preserves_the_original_safe_error() {
        let raw = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let request = RunRequest {
            prompt: "scenario:capture-approval-unexpected-second".into(),
            cwd: cwd.path().display().to_string(),
            ..RunRequest::for_session(RuntimeMode::ApprovalRequired)
        };
        let mut session = RecordingSession::start(config(
            "claude-partial-write-failure",
            fixture_path("fake-claude"),
            CaptureOperation::Claude(ClaudeCaptureOperation::Run {
                request,
                script: ClaudeRunScript::Approval,
            }),
            raw.path(),
        ))
        .await
        .unwrap();
        let partial_path = session.directory.join("partial-capture.json");
        std::fs::write(&partial_path, b"existing quarantine").unwrap();

        let error = session.finish().await.unwrap_err().to_string();

        assert_eq!(
            error,
            "Claude approval request used an unexpected tool or order."
        );
        assert!(!error.contains(&session.directory.display().to_string()));
        assert_eq!(std::fs::read(partial_path).unwrap(), b"existing quarantine");
    }

    fn find_named_file(root: &Path, name: &str) -> bool {
        std::fs::read_dir(root).is_ok_and(|entries| {
            entries.filter_map(Result::ok).any(|entry| {
                entry.file_name() == name
                    || (entry.path().is_dir() && find_named_file(&entry.path(), name))
            })
        })
    }

    #[tokio::test]
    async fn strict_resume_cannot_be_relabelled_success() {
        let raw = tempfile::tempdir().unwrap();
        let claude_request = RunRequest {
            prompt: "scenario:happy".into(),
            resume: Some("different-session".into()),
            ..RunRequest::for_session(RuntimeMode::AutoAcceptEdits)
        };
        let error = record(config(
            "claude-resume",
            fixture_path("fake-claude"),
            CaptureOperation::Claude(ClaudeCaptureOperation::Run {
                request: claude_request,
                script: ClaudeRunScript::Resume,
            }),
            raw.path(),
        ))
        .await
        .unwrap_err();
        assert!(error.to_string().contains("different session identifier"));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn zero_approval_cannot_be_relabelled_success() {
        let raw = tempfile::tempdir().unwrap();
        let codex_request = RunRequest {
            prompt: "scenario:capture-fresh".into(),
            cwd: std::env::temp_dir().display().to_string(),
            ..RunRequest::for_session(RuntimeMode::ApprovalRequired)
        };
        let error = record(config(
            "codex-approval",
            fixture_path("fake-codex"),
            CaptureOperation::Codex(CodexCaptureOperation::Run {
                request: codex_request,
                script: CodexRunScript::Approval,
            }),
            raw.path(),
        ))
        .await
        .unwrap_err();
        assert!(error.to_string().contains("turn lifecycle"), "{error}");
    }

    #[tokio::test]
    async fn codex_resume_never_falls_back_to_a_fresh_thread() {
        let raw = tempfile::tempdir().unwrap();
        let request = RunRequest {
            prompt: "scenario:resumed".into(),
            resume: Some("resume-fail".into()),
            cwd: std::env::temp_dir().display().to_string(),
            ..RunRequest::for_session(RuntimeMode::AutoAcceptEdits)
        };
        let error = record(config(
            "codex-resume",
            fixture_path("fake-codex"),
            CaptureOperation::Codex(CodexCaptureOperation::Run {
                request,
                script: CodexRunScript::Resume,
            }),
            raw.path(),
        ))
        .await
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("rejected the requested thread resume")
        );
    }

    #[tokio::test]
    async fn codex_approval_requires_the_observed_command_phase_and_file_change() {
        let raw = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let request = RunRequest {
            prompt: "scenario:capture-approval".into(),
            cwd: cwd.path().display().to_string(),
            ..RunRequest::for_session(RuntimeMode::ApprovalRequired)
        };
        let result = record(config(
            "codex-approval",
            fixture_path("fake-codex"),
            CaptureOperation::Codex(CodexCaptureOperation::Run {
                request,
                script: CodexRunScript::Approval,
            }),
            raw.path(),
        ))
        .await;
        #[cfg(not(windows))]
        {
            let error = result.expect_err("Unix launcher evidence is not approved yet");
            assert!(
                error.to_string().contains("observed safe Unix launcher"),
                "{error}"
            );
            return;
        }
        #[cfg(windows)]
        {
            let capture = result.unwrap();
            let values: Vec<serde_json::Value> = channel_payloads(&capture, Channel::Stdout)
                .into_iter()
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect();
            assert_eq!(
                values
                    .iter()
                    .filter(|value| value["method"] == "item/started"
                        && value["params"]["item"]["type"] == "commandExecution")
                    .count(),
                5
            );
            assert_eq!(
                values
                    .iter()
                    .filter(|value| value["method"] == "item/completed"
                        && value["params"]["item"]["type"] == "commandExecution")
                    .count(),
                4
            );
            assert!(
                values.iter().any(|value| value["id"] == 0
                    && value["method"] == "item/fileChange/requestApproval")
            );
            assert!(
                channel_payloads(&capture, Channel::Stdin)
                    .iter()
                    .any(|line| {
                        serde_json::from_str::<Value>(line).is_ok_and(|value| {
                            value["id"] == 0 && value["result"]["decision"] == "accept"
                        })
                    })
            );
            assert_eq!(
                std::fs::read_to_string(cwd.path().join(APPROVAL_MARKER_NAME)).unwrap(),
                super::APPROVAL_MARKER_CONTENT
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn codex_approval_launcher_requires_exact_trusted_file_identity_and_grammar() {
        let root = tempfile::tempdir().unwrap();
        let program_files = root.path().join("Program Files");
        let system_root = root.path().join("Windows");
        let trusted_dir = program_files.join("WindowsApps/PowerShell 7.5.2");
        let hostile_dir = root.path().join("capture cwd");
        let prefix_collision = root.path().join("Program Files hostile");
        std::fs::create_dir_all(&trusted_dir).unwrap();
        std::fs::create_dir_all(&hostile_dir).unwrap();
        std::fs::create_dir_all(&system_root).unwrap();
        std::fs::create_dir_all(&prefix_collision).unwrap();
        let trusted = trusted_dir.join("pwsh.exe");
        let hostile = hostile_dir.join("pwsh.exe");
        let collision = prefix_collision.join("pwsh.exe");
        std::fs::write(&trusted, b"trusted").unwrap();
        std::fs::write(&hostile, b"hostile").unwrap();
        std::fs::write(&collision, b"collision").unwrap();
        let system_pwsh = system_root.join("System32/pwsh.exe");
        std::fs::create_dir_all(system_pwsh.parent().unwrap()).unwrap();
        std::fs::write(&system_pwsh, b"system").unwrap();
        let roots = super::canonical_protected_roots([
            Some(program_files.as_path()),
            None,
            Some(system_root.as_path()),
        ])
        .unwrap();
        let identity = super::select_trusted_powershell(
            &[hostile.clone(), collision.clone(), trusted.clone()],
            &roots,
            &[hostile_dir.clone(), root.path().join("raw")],
        )
        .unwrap();
        assert_eq!(identity, super::file_identity(&trusted).unwrap());
        assert!(
            super::select_trusted_powershell(
                &[hostile.clone(), collision],
                &roots,
                &[hostile_dir.clone(), root.path().join("raw")],
            )
            .is_err()
        );
        assert_eq!(
            super::select_trusted_powershell(
                std::slice::from_ref(&system_pwsh),
                &roots,
                std::slice::from_ref(&hostile_dir),
            )
            .unwrap(),
            super::file_identity(&system_pwsh).unwrap()
        );
        let valid = format!(r#""{}" -Command 'echo capture'"#, trusted.display());
        super::validate_codex_raw_launcher(&valid, &identity).unwrap();

        for launcher in [
            format!(r#""{}" -Command 'echo capture'"#, hostile.display()),
            format!(
                r#""{}" -NoProfile -Command 'echo capture'"#,
                trusted.display()
            ),
            format!(
                r#""{}" -Command 'echo capture && whoami'"#,
                trusted.display()
            ),
            format!(
                r#""{}" -Command 'echo capture > marker'"#,
                trusted.display()
            ),
            format!(
                r#""{}" -Command 'echo capture' trailing"#,
                trusted.display()
            ),
            format!(r#"{} -Command 'echo capture'"#, trusted.display()),
        ] {
            assert!(
                super::validate_codex_raw_launcher(&launcher, &identity).is_err(),
                "accepted untrusted launcher: {launcher:?}"
            );
        }

        std::fs::remove_file(&trusted).unwrap();
        std::fs::write(&trusted, b"replacement").unwrap();
        assert!(super::validate_codex_raw_launcher(&valid, &identity).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn codex_approval_trusted_roots_must_exist_and_canonicalize() {
        let root = tempfile::tempdir().unwrap();
        assert!(super::canonical_protected_roots([None, None, None]).is_err());
        assert!(
            super::canonical_protected_roots([
                Some(root.path().join("missing").as_path()),
                None,
                None,
            ])
            .is_err()
        );
        let roots =
            super::canonical_protected_roots([Some(root.path()), Some(root.path()), None]).unwrap();
        assert_eq!(
            roots.len(),
            1,
            "canonical root aliases must be deduplicated"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn codex_approval_launcher_remains_fail_closed_without_unix_evidence() {
        let error =
            super::resolve_trusted_powershell(Path::new("/"), Path::new("/tmp/raw")).unwrap_err();
        assert!(error.to_string().contains("safe Unix launcher"), "{error}");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn codex_approval_rejects_missing_or_invalid_rpc_ids_before_accepting() {
        for prompt in [
            "scenario:capture-approval-missing-id",
            "scenario:capture-approval-invalid-id",
        ] {
            let raw = tempfile::tempdir().unwrap();
            let cwd = tempfile::tempdir().unwrap();
            let request = RunRequest {
                prompt: prompt.into(),
                cwd: cwd.path().display().to_string(),
                ..RunRequest::for_session(RuntimeMode::ApprovalRequired)
            };
            let (error, stdin) = failed_session_stdin(config(
                "codex-approval-invalid-id",
                fixture_path("fake-codex"),
                CaptureOperation::Codex(CodexCaptureOperation::Run {
                    request,
                    script: CodexRunScript::Approval,
                }),
                raw.path(),
            ))
            .await;
            assert!(
                stdin.iter().all(|line| !line.contains("\"decision\"")),
                "invalid approval ID received accept: {stdin:?}"
            );
            assert!(error.contains("approval request"), "{error}");
        }
    }

    /// Break caught: JSON-RPC numeric zero is a valid request identifier and production echoes
    /// the exact Value; capture-only validation must not relabel it as missing.
    #[test]
    fn codex_approval_ids_accept_zero_and_reject_non_u64_json_values() {
        let mut request_ids = BTreeSet::new();
        let mut item_ids = BTreeSet::new();
        assert_eq!(
            super::codex_approval_ids(
                &json!({"id": 0, "params": {"itemId": "file-zero"}}),
                &mut request_ids,
                &mut item_ids,
            )
            .unwrap(),
            (0, "file-zero".to_owned())
        );
        assert!(
            super::codex_approval_ids(
                &json!({"id": 0, "params": {"itemId": "file-repeat"}}),
                &mut request_ids,
                &mut item_ids,
            )
            .unwrap_err()
            .to_string()
            .contains("repeated a request identifier")
        );

        for invalid in [
            Value::Null,
            json!("0"),
            json!(-1),
            json!(0.5),
            json!(true),
            json!({}),
            json!([]),
        ] {
            let mut request_ids = BTreeSet::new();
            let mut item_ids = BTreeSet::new();
            let request = json!({"id": invalid, "params": {"itemId": "file"}});
            assert!(
                super::codex_approval_ids(&request, &mut request_ids, &mut item_ids).is_err(),
                "accepted invalid JSON-RPC id: {invalid:?}"
            );
        }
        let mut request_ids = BTreeSet::new();
        let mut item_ids = BTreeSet::new();
        assert!(
            super::codex_approval_ids(
                &json!({"params": {"itemId": "file"}}),
                &mut request_ids,
                &mut item_ids,
            )
            .is_err()
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn codex_approval_rejects_destructive_requests_before_accepting() {
        for (prompt, rejected_id) in [
            ("scenario:capture-approval-destructive-command", 451),
            ("scenario:capture-approval-destructive-file", 464),
        ] {
            let raw = tempfile::tempdir().unwrap();
            let cwd = tempfile::tempdir().unwrap();
            let request = RunRequest {
                prompt: prompt.into(),
                cwd: cwd.path().display().to_string(),
                ..RunRequest::for_session(RuntimeMode::ApprovalRequired)
            };
            let (error, stdin) = failed_session_stdin(config(
                "codex-approval-adversarial",
                fixture_path("fake-codex"),
                CaptureOperation::Codex(CodexCaptureOperation::Run {
                    request,
                    script: CodexRunScript::Approval,
                }),
                raw.path(),
            ))
            .await;
            assert!(
                !contains_response_id(&stdin, rejected_id),
                "unsafe approval {rejected_id} received accept: {stdin:?}"
            );
            assert!(error.contains("approval request"), "{error}");
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn codex_approval_rejects_relevant_activity_after_file_accept() {
        let raw = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let request = RunRequest {
            prompt: "scenario:capture-approval-later-command".into(),
            cwd: cwd.path().display().to_string(),
            ..RunRequest::for_session(RuntimeMode::ApprovalRequired)
        };
        let (error, stdin) = failed_session_stdin(config(
            "codex-approval-later-command",
            fixture_path("fake-codex"),
            CaptureOperation::Codex(CodexCaptureOperation::Run {
                request,
                script: CodexRunScript::Approval,
            }),
            raw.path(),
        ))
        .await;
        assert!(
            contains_response_id(&stdin, 0),
            "bounded file was not accepted"
        );
        assert!(
            error.contains("reviewed ordering"),
            "late command was not contract-named: {error}"
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn codex_approval_rejects_observed_order_deviations_before_reply() {
        for mode in [
            "missing-thread",
            "second-turn-start",
            "wrong-thread",
            "missing-turn",
            "wrong-turn",
            "hidden-action",
            "file-extra-key",
            "file-missing-key",
            "file-missing-thread",
            "file-wrong-thread",
            "file-missing-turn",
            "file-wrong-turn",
            "request-extra-key",
            "request-missing-key",
            "request-missing-thread",
            "request-wrong-thread",
            "request-missing-turn",
            "request-wrong-turn",
            "routine-mismatch",
            "routine-type-change",
            "routine-id-change",
            "routine-completion-without-start",
            "marker-preapproval",
            "cwd-replaced",
            "wrong-order",
            "duplicate-start",
            "wrong-completion-id",
            "wrong-status",
            "wrong-exit",
            "wrong-fields",
            "command-approval",
            "missing-completion",
            "extra-file",
        ] {
            let raw = tempfile::tempdir().unwrap();
            let cwd_root = tempfile::tempdir().unwrap();
            let cwd = cwd_root.path().join("cwd");
            std::fs::create_dir(&cwd).unwrap();
            let request = RunRequest {
                prompt: format!("scenario:capture-approval-deviation:{mode}"),
                cwd: cwd.display().to_string(),
                ..RunRequest::for_session(RuntimeMode::ApprovalRequired)
            };
            let (error, stdin) = failed_session_stdin(config(
                "codex-approval-deviation",
                fixture_path("fake-codex"),
                CaptureOperation::Codex(CodexCaptureOperation::Run {
                    request,
                    script: CodexRunScript::Approval,
                }),
                raw.path(),
            ))
            .await;
            assert!(
                stdin.iter().all(|line| !line.contains("\"decision\"")),
                "{mode} received an approval reply: {stdin:?}"
            );
            assert!(
                error.contains("Codex") || error.contains("codex"),
                "{mode} lacked a safe contract error: {error}"
            );
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn codex_approval_rejects_post_accept_deviations() {
        for mode in [
            "extra-approval",
            "hidden-after",
            "marker-missing",
            "marker-wrong",
            "marker-link",
            "terminal-failed",
            "terminal-mismatch",
            "terminal-missing-thread",
            "terminal-wrong-thread",
            "terminal-missing-turn",
        ] {
            let raw = tempfile::tempdir().unwrap();
            let cwd = tempfile::tempdir().unwrap();
            let request = RunRequest {
                prompt: format!("scenario:capture-approval-deviation:{mode}"),
                cwd: cwd.path().display().to_string(),
                ..RunRequest::for_session(RuntimeMode::ApprovalRequired)
            };
            let (error, stdin) = failed_session_stdin(config(
                "codex-approval-post-accept-deviation",
                fixture_path("fake-codex"),
                CaptureOperation::Codex(CodexCaptureOperation::Run {
                    request,
                    script: CodexRunScript::Approval,
                }),
                raw.path(),
            ))
            .await;
            assert!(
                contains_response_id(&stdin, 0),
                "{mode} missed bounded accept"
            );
            assert!(
                stdin.iter().all(|line| {
                    serde_json::from_str::<Value>(line).map_or(true, |value| {
                        value["id"] != 1 || value["result"]["decision"] != "accept"
                    })
                }),
                "{mode} accepted an extra request"
            );
            assert!(error.contains("Codex"), "{mode} lacked safe error: {error}");
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn codex_approval_rejects_a_post_accept_marker_link() {
        let raw = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let request = RunRequest {
            prompt: "scenario:capture-approval-deviation:marker-link".into(),
            cwd: cwd.path().display().to_string(),
            ..RunRequest::for_session(RuntimeMode::ApprovalRequired)
        };
        let (error, stdin) = failed_session_stdin(config(
            "codex-approval-marker-link",
            fixture_path("fake-codex"),
            CaptureOperation::Codex(CodexCaptureOperation::Run {
                request,
                script: CodexRunScript::Approval,
            }),
            raw.path(),
        ))
        .await;
        assert!(contains_response_id(&stdin, 0));
        assert!(error.contains("regular non-reparse file"), "{error}");
    }

    #[tokio::test]
    async fn codex_on_request_requires_ordered_failure_approval_retry_and_marker() {
        let raw = tempfile::tempdir().unwrap();
        let cwd = isolated_tempdir("comet-onrequest-cwd-");
        let target = isolated_tempdir("comet-onrequest-target-");
        let request = RunRequest {
            prompt: format!("scenario:capture-onrequest:{}", target.path().display()),
            cwd: cwd.path().display().to_string(),
            ..RunRequest::for_session(RuntimeMode::AutoAcceptEdits)
        };
        let mut config = config(
            "codex-approval-on-request",
            fixture_path("fake-codex"),
            CaptureOperation::Codex(CodexCaptureOperation::Run {
                request,
                script: CodexRunScript::ApprovalOnRequest,
            }),
            raw.path(),
        );
        config.approval_target = Some(target.path().into());
        let capture = record(config).await.unwrap();
        assert_eq!(
            capture.redaction_roots.approval_target.as_deref(),
            Some(target.path().to_string_lossy().as_ref())
        );
    }

    #[tokio::test]
    async fn codex_on_request_echoes_zero_and_rejects_its_duplicate() {
        let raw = tempfile::tempdir().unwrap();
        let cwd = isolated_tempdir("comet-onrequest-zero-cwd-");
        let target = isolated_tempdir("comet-onrequest-zero-target-");
        let request = RunRequest {
            prompt: format!(
                "scenario:capture-onrequest-zero:{}",
                target.path().display()
            ),
            cwd: cwd.path().display().to_string(),
            ..RunRequest::for_session(RuntimeMode::AutoAcceptEdits)
        };
        let mut capture_config = config(
            "codex-approval-on-request-zero",
            fixture_path("fake-codex"),
            CaptureOperation::Codex(CodexCaptureOperation::Run {
                request,
                script: CodexRunScript::ApprovalOnRequest,
            }),
            raw.path(),
        );
        capture_config.approval_target = Some(target.path().into());
        let (error, stdin) = failed_session_stdin(capture_config).await;
        assert!(
            contains_response_id(&stdin, 0),
            "numeric zero was not echoed: {error}; {stdin:?}"
        );
        assert!(error.contains("repeated its request identifier"), "{error}");
        assert_eq!(
            stdin
                .iter()
                .filter(|line| serde_json::from_str::<Value>(line)
                    .is_ok_and(|value| value["id"] == 0 && value["result"]["decision"] == "accept"))
                .count(),
            1
        );
    }

    #[test]
    fn codex_on_request_ids_reject_every_non_u64_json_shape() {
        for invalid in [
            Value::Null,
            json!("0"),
            json!(-1),
            json!(0.5),
            json!(true),
            json!({}),
            json!([]),
        ] {
            let mut state = super::CodexOnRequestState {
                failed_item_id: Some("item".into()),
                stage: 1,
                ..Default::default()
            };
            let request = json!({"id":invalid,"method":"item/commandExecution/requestApproval","params":{"itemId":"item","command":"safe","commandActions":[{"type":"unknown","command":"safe"}],"reason":"sandbox denied"}});
            assert!(
                super::validate_codex_on_request_approval(
                    &request,
                    "item/commandExecution/requestApproval",
                    "safe",
                    &mut state
                )
                .is_err()
            );
        }
    }

    #[tokio::test]
    async fn codex_on_request_invalid_id_shapes_never_receive_a_reply() {
        for mode in [
            "missing", "null", "string", "negative", "fraction", "bool", "object", "array",
        ] {
            let raw = tempfile::tempdir().unwrap();
            let cwd = isolated_tempdir("comet-onrequest-invalid-cwd-");
            let target = isolated_tempdir("comet-onrequest-invalid-target-");
            let request = RunRequest {
                prompt: format!(
                    "scenario:capture-onrequest-invalid-{mode}:{}",
                    target.path().display()
                ),
                cwd: cwd.path().display().to_string(),
                ..RunRequest::for_session(RuntimeMode::AutoAcceptEdits)
            };
            let mut capture_config = config(
                "codex-approval-on-request-invalid-id",
                fixture_path("fake-codex"),
                CaptureOperation::Codex(CodexCaptureOperation::Run {
                    request,
                    script: CodexRunScript::ApprovalOnRequest,
                }),
                raw.path(),
            );
            capture_config.approval_target = Some(target.path().into());
            let (error, stdin) = failed_session_stdin(capture_config).await;
            assert!(
                stdin.iter().all(|line| !line.contains("\"decision\"")),
                "{mode} received an approval response: {stdin:?}"
            );
            assert!(error.contains("valid identifier"), "{mode}: {error}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn codex_approval_cwd_identity_and_marker_are_rechecked() {
        let parent = tempfile::tempdir().unwrap();
        let cwd = parent.path().join("cwd");
        std::fs::create_dir(&cwd).unwrap();
        let identity = super::validate_ordinary_approval_cwd(&cwd, None, true).unwrap();
        std::fs::write(cwd.join(APPROVAL_MARKER_NAME), "capture\n").unwrap();
        assert!(super::validate_ordinary_approval_cwd(&cwd, Some(&identity), true).is_err());
        std::fs::remove_file(cwd.join(APPROVAL_MARKER_NAME)).unwrap();
        std::fs::remove_dir(&cwd).unwrap();
        std::fs::create_dir(&cwd).unwrap();
        assert!(super::validate_ordinary_approval_cwd(&cwd, Some(&identity), true).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn codex_approval_cwd_rejects_directory_reparse_points() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        let link = root.path().join("cwd-link");
        std::fs::create_dir(&target).unwrap();
        let output = std::process::Command::new("cmd.exe")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(&link)
            .arg(&target)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "failed to construct a test-owned directory junction"
        );
        assert!(super::validate_ordinary_approval_cwd(&link, None, true).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn codex_approval_windows_api_roots_are_available_and_canonical() {
        assert_eq!(usize::BITS, 64, "32-bit Windows must fail closed");
        let roots = super::windows_protected_roots().unwrap();
        assert!(!roots.is_empty());
        assert_eq!(roots.iter().collect::<BTreeSet<_>>().len(), roots.len());
        assert!(roots.iter().all(|root| root.is_absolute() && root.is_dir()));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn codex_approval_precreated_marker_fails_before_spawn_or_reply() {
        let raw = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        std::fs::write(cwd.path().join(APPROVAL_MARKER_NAME), "capture\n").unwrap();
        let request = RunRequest {
            prompt: "scenario:capture-approval".into(),
            cwd: cwd.path().display().to_string(),
            ..RunRequest::for_session(RuntimeMode::ApprovalRequired)
        };
        let error = record(config(
            "codex-approval-precreated-marker",
            fixture_path("fake-codex"),
            CaptureOperation::Codex(CodexCaptureOperation::Run {
                request,
                script: CodexRunScript::Approval,
            }),
            raw.path(),
        ))
        .await
        .unwrap_err();
        assert!(
            error.to_string().contains("marker must be absent"),
            "{error}"
        );
        assert!(raw.path().read_dir().unwrap().next().is_none());
    }

    #[tokio::test]
    async fn codex_on_request_rejects_approval_before_sandbox_failure() {
        let raw = tempfile::tempdir().unwrap();
        let cwd = isolated_tempdir("comet-onrequest-cwd-");
        let target = isolated_tempdir("comet-onrequest-target-");
        let request = RunRequest {
            prompt: "scenario:capture-onrequest-out-of-order".into(),
            cwd: cwd.path().display().to_string(),
            ..RunRequest::for_session(RuntimeMode::AutoAcceptEdits)
        };
        let mut config = config(
            "codex-approval-on-request",
            fixture_path("fake-codex"),
            CaptureOperation::Codex(CodexCaptureOperation::Run {
                request,
                script: CodexRunScript::ApprovalOnRequest,
            }),
            raw.path(),
        );
        config.approval_target = Some(target.path().into());
        let error = record(config).await.unwrap_err();
        assert!(error.to_string().contains("before the sandbox failure"));
    }

    #[tokio::test]
    async fn codex_on_request_rejects_a_destructive_command_before_accepting() {
        let raw = tempfile::tempdir().unwrap();
        let cwd = isolated_tempdir("comet-onrequest-cwd-");
        let target = isolated_tempdir("comet-onrequest-target-");
        let request = RunRequest {
            prompt: "scenario:capture-onrequest-destructive".into(),
            cwd: cwd.path().display().to_string(),
            ..RunRequest::for_session(RuntimeMode::AutoAcceptEdits)
        };
        let mut capture_config = config(
            "codex-approval-on-request-adversarial",
            fixture_path("fake-codex"),
            CaptureOperation::Codex(CodexCaptureOperation::Run {
                request,
                script: CodexRunScript::ApprovalOnRequest,
            }),
            raw.path(),
        );
        capture_config.approval_target = Some(target.path().into());
        let (error, stdin) = failed_session_stdin(capture_config).await;
        assert!(
            !contains_response_id(&stdin, 471),
            "unsafe on-request command received accept: {stdin:?}"
        );
        assert!(error.contains("approval request"), "{error}");
    }

    #[tokio::test]
    async fn codex_on_request_preflight_rejects_repository_and_linked_worktree_cwds() {
        for linked in [false, true] {
            let raw = tempfile::tempdir().unwrap();
            let cwd = tempfile::tempdir().unwrap();
            if linked {
                std::fs::write(cwd.path().join(".git"), "gitdir: unused").unwrap();
            } else {
                std::fs::create_dir(cwd.path().join(".git")).unwrap();
            }
            let target = isolated_tempdir("comet-onrequest-target-");
            let request = RunRequest {
                prompt: "scenario:capture-onrequest-destructive".into(),
                cwd: cwd.path().display().to_string(),
                ..RunRequest::for_session(RuntimeMode::AutoAcceptEdits)
            };
            let mut capture_config = config(
                "codex-approval-on-request-repository",
                fixture_path("fake-codex"),
                CaptureOperation::Codex(CodexCaptureOperation::Run {
                    request,
                    script: CodexRunScript::ApprovalOnRequest,
                }),
                raw.path(),
            );
            capture_config.approval_target = Some(target.path().into());
            let error = match RecordingSession::start(capture_config).await {
                Ok(_) => panic!("repository cwd must fail before spawn"),
                Err(error) => error,
            };
            assert!(error.to_string().contains("non-repository"), "{error}");
        }
    }

    #[tokio::test]
    async fn codex_on_request_preflight_rechecks_target_emptiness_before_spawn() {
        let raw = tempfile::tempdir().unwrap();
        let cwd = isolated_tempdir("comet-onrequest-cwd-");
        let target = isolated_tempdir("comet-onrequest-target-");
        std::fs::write(target.path().join("appeared-after-config.txt"), "hostile").unwrap();
        let request = RunRequest {
            prompt: "scenario:capture-onrequest-destructive".into(),
            cwd: cwd.path().display().to_string(),
            ..RunRequest::for_session(RuntimeMode::AutoAcceptEdits)
        };
        let mut capture_config = config(
            "codex-approval-on-request-raced-before-spawn",
            fixture_path("fake-codex"),
            CaptureOperation::Codex(CodexCaptureOperation::Run {
                request,
                script: CodexRunScript::ApprovalOnRequest,
            }),
            raw.path(),
        );
        capture_config.approval_target = Some(target.path().into());
        let error = match RecordingSession::start(capture_config).await {
            Ok(_) => panic!("nonempty target must fail before spawn"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("empty"), "{error}");
    }

    #[tokio::test]
    async fn codex_on_request_rechecks_target_immediately_before_accepting() {
        for race in ["target", "marker", "identity"] {
            let raw = tempfile::tempdir().unwrap();
            let cwd = isolated_tempdir("comet-onrequest-cwd-");
            let target = isolated_tempdir("comet-onrequest-target-");
            let prompt = format!(
                "scenario:capture-onrequest-{race}-race:{}",
                target.path().display()
            );
            let request = RunRequest {
                prompt,
                cwd: cwd.path().display().to_string(),
                ..RunRequest::for_session(RuntimeMode::AutoAcceptEdits)
            };
            let mut capture_config = config(
                "codex-approval-on-request-target-race",
                fixture_path("fake-codex"),
                CaptureOperation::Codex(CodexCaptureOperation::Run {
                    request,
                    script: CodexRunScript::ApprovalOnRequest,
                }),
                raw.path(),
            );
            capture_config.approval_target = Some(target.path().into());
            let (error, stdin) = failed_session_stdin(capture_config).await;
            if race == "identity" {
                std::fs::remove_dir(format!("{}.original", target.path().display())).unwrap();
            }
            assert!(
                !contains_response_id(&stdin, 481),
                "raced target received accept: {stdin:?}"
            );
            assert!(error.contains("approval target"), "{error}");
        }
    }

    #[tokio::test]
    async fn codex_steer_and_interrupt_require_successful_protocol_neighborhoods() {
        for (name, prompt, script) in [
            ("codex-steer", "scenario:steer", CodexRunScript::Steer),
            (
                "codex-interruption",
                "scenario:interrupt",
                CodexRunScript::Interruption,
            ),
        ] {
            let raw = tempfile::tempdir().unwrap();
            let request = RunRequest {
                prompt: prompt.into(),
                cwd: std::env::temp_dir().display().to_string(),
                ..RunRequest::for_session(RuntimeMode::AutoAcceptEdits)
            };
            record(config(
                name,
                fixture_path("fake-codex"),
                CaptureOperation::Codex(CodexCaptureOperation::Run { request, script }),
                raw.path(),
            ))
            .await
            .unwrap();
        }
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
            prompt: "scenario:capture-fresh".into(),
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
            "input": [{"type": "text", "text": "scenario:capture-fresh"}],
            "approvalPolicy": "untrusted",
            "sandboxPolicy": {"type": "dangerFullAccess"},
            "summary": "auto",
            "model": "gpt-5.6-luna",
            "effort": "low",
            "serviceTier": "fast",
        });
        assert_eq!(turn["params"], expected_turn);
        assert_eq!(
            crate::codex::turn_start_params(&provider_request, "th-1", "scenario:capture-fresh"),
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

    /// Break caught: an error path with no retained child returns before pending pipe readers are
    /// drained, so the partial snapshot races and can omit the provider's final observed frame.
    #[tokio::test]
    async fn cleanup_without_a_child_drains_readers_before_partial_snapshot() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let reader_events = Arc::clone(&events);
        let reader_started = Arc::new(AtomicBool::new(false));
        let task_started = Arc::clone(&reader_started);
        let reader = tokio::spawn(async move {
            task_started.store(true, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(50)).await;
            super::push_event(
                &reader_events,
                Channel::Stdout,
                "late observed frame".into(),
            );
        });
        while !reader_started.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
        let (_stdout_tx, stdout_lines) = tokio::sync::mpsc::unbounded_channel();
        let raw = tempfile::tempdir().unwrap();
        let mut session = RecordingSession {
            provider: Provider::Claude,
            operation: CaptureOperation::Claude(ClaudeCaptureOperation::ModelDiscovery),
            timeout: Duration::from_secs(1),
            directory: raw.path().into(),
            cli_version: "fixture".into(),
            captured_at_unix_ms: 1,
            scenario: "pending-reader".into(),
            purpose: "prove cleanup ordering".into(),
            command: CommandSnapshot {
                program: "fake-claude".into(),
                args: Vec::new(),
                cwd: None,
                configured_env: Default::default(),
                stdin: StdioMode::Piped,
                stdout: StdioMode::Piped,
                stderr: StdioMode::Piped,
                kill_on_drop: true,
                #[cfg(windows)]
                creation_flags: 0,
            },
            approval_target: None,
            approval_target_identity: None,
            approval_cwd_identity: None,
            trusted_powershell: None,
            child: None,
            stdin: None,
            stdout_lines,
            readers: vec![reader],
            events,
            reap_notice: None,
            wait_error_once: false,
        };

        session.terminate_and_reap().await;
        let capture = session.raw_capture(None);

        assert!(session.readers.is_empty(), "pending reader was not joined");
        assert_eq!(capture.events.len(), 1);
        assert_eq!(capture.events[0].payload, "late observed frame");
    }

    /// Break caught: a child-wait I/O error discards the only child handle before the outer
    /// failure cleanup can attempt kill/reap and finalize the partial transcript.
    #[tokio::test]
    async fn wait_error_retains_child_for_cleanup_and_quarantine() {
        let raw = tempfile::tempdir().unwrap();
        let mut session = RecordingSession::start(config(
            "claude-wait-error",
            fixture_path("fake-claude"),
            CaptureOperation::Claude(ClaudeCaptureOperation::ModelDiscovery),
            raw.path(),
        ))
        .await
        .unwrap();
        let pid = session.child_id().expect("spawned child id");
        let directory = session.directory.clone();
        session.wait_error_once = true;

        let error = session.finish().await.unwrap_err();

        assert!(error.to_string().contains("exit status could not be read"));
        assert!(session.child.is_none(), "cleanup retained the child handle");
        assert!(!process_is_live(pid), "provider child {pid} remains live");
        let partial: Value = serde_json::from_slice(
            &std::fs::read(directory.join("partial-capture.json"))
                .expect("wait-error partial evidence"),
        )
        .unwrap();
        assert_eq!(partial["failure_class"], "process_error");
        assert!(
            partial["events"]
                .as_array()
                .is_some_and(|events| { events.iter().any(|event| event["channel"] == "stdout") })
        );
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
