use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;
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

use comet_proto::{ReasoningLevel, RunRequest, RuntimeMode, SandboxLevel};

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
pub struct RawCapture {
    pub directory: PathBuf,
    pub provider: Provider,
    pub cli_version: String,
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
}

impl RecordingSession {
    async fn start(config: CaptureConfig) -> anyhow::Result<Self> {
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

        let (method, thread_params) = codex_thread_request(&request, script);
        self.write_line(&rpc_request(next_id, method, thread_params))
            .await?;
        let mut thread_reply = self.codex_reply(next_id).await?;
        next_id += 1;
        if thread_reply.get("error").is_some() && method == "thread/resume" {
            let (_, params) = codex_thread_request(&request, CodexRunScript::FreshText);
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
            codex_turn_params(&request, &thread_id),
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
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _ = tokio::time::timeout(CLEANUP_TIMEOUT, child.wait()).await;
            });
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

fn codex_thread_request(request: &RunRequest, script: CodexRunScript) -> (&'static str, Value) {
    let mut params = serde_json::Map::new();
    params.insert("cwd".into(), request.cwd.clone().into());
    params.insert(
        "approvalPolicy".into(),
        codex_approval_policy(request.runtime_mode).into(),
    );
    params.insert("sandbox".into(), codex_sandbox_mode(request.sandbox).into());
    params.insert(
        "approvalsReviewer".into(),
        if request.runtime_mode == RuntimeMode::Auto {
            "auto_review"
        } else {
            "user"
        }
        .into(),
    );
    if let Some(model) = &request.model {
        params.insert("model".into(), model.clone().into());
    }
    if let Some(tier) = request
        .model_options
        .get("serviceTier")
        .and_then(Value::as_str)
        .filter(|tier| *tier != "default")
    {
        params.insert("serviceTier".into(), tier.into());
    }
    if matches!(script, CodexRunScript::Resume) {
        params.insert(
            "threadId".into(),
            request.resume.clone().unwrap_or_default().into(),
        );
        ("thread/resume", Value::Object(params))
    } else {
        ("thread/start", Value::Object(params))
    }
}

fn codex_turn_params(request: &RunRequest, thread_id: &str) -> Value {
    let mut params = serde_json::Map::new();
    params.insert("threadId".into(), thread_id.into());
    params.insert(
        "input".into(),
        json!([{"type": "text", "text": request.prompt}]),
    );
    params.insert(
        "approvalPolicy".into(),
        codex_approval_policy(request.runtime_mode).into(),
    );
    let mut sandbox = serde_json::Map::new();
    sandbox.insert("type".into(), codex_sandbox_policy(request.sandbox).into());
    if request.sandbox == SandboxLevel::WorkspaceWrite {
        sandbox.insert("networkAccess".into(), true.into());
    }
    params.insert("sandboxPolicy".into(), Value::Object(sandbox));
    params.insert("summary".into(), "auto".into());
    if let Some(model) = &request.model {
        params.insert("model".into(), model.clone().into());
    }
    if let Some(effort) = codex_effort(request.reasoning) {
        params.insert("effort".into(), effort.into());
    }
    if let Some(tier) = request
        .model_options
        .get("serviceTier")
        .and_then(Value::as_str)
        .filter(|tier| *tier != "default")
    {
        params.insert("serviceTier".into(), tier.into());
    }
    Value::Object(params)
}

fn codex_effort(level: Option<ReasoningLevel>) -> Option<&'static str> {
    Some(match level? {
        ReasoningLevel::Minimal | ReasoningLevel::Low => "low",
        ReasoningLevel::Medium => "medium",
        ReasoningLevel::High => "high",
        ReasoningLevel::XHigh | ReasoningLevel::Ultracode | ReasoningLevel::Ultrathink => "xhigh",
        ReasoningLevel::Max => "max",
        ReasoningLevel::Ultra => "ultra",
    })
}

fn codex_approval_policy(mode: RuntimeMode) -> &'static str {
    match mode {
        RuntimeMode::ApprovalRequired => "untrusted",
        RuntimeMode::AutoAcceptEdits | RuntimeMode::Auto => "on-request",
        RuntimeMode::FullAccess => "never",
    }
}

fn codex_sandbox_mode(level: SandboxLevel) -> &'static str {
    match level {
        SandboxLevel::ReadOnly => "read-only",
        SandboxLevel::WorkspaceWrite => "workspace-write",
        SandboxLevel::DangerFullAccess => "danger-full-access",
    }
}

fn codex_sandbox_policy(level: SandboxLevel) -> &'static str {
    match level {
        SandboxLevel::ReadOnly => "readOnly",
        SandboxLevel::WorkspaceWrite => "workspaceWrite",
        SandboxLevel::DangerFullAccess => "dangerFullAccess",
    }
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
