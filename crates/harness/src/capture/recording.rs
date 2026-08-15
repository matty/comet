use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::mpsc;

use comet_proto::RunRequest;

#[cfg(test)]
use super::SanitizationError;
use super::approval::{
    FileIdentity, repository_root, resolve_trusted_powershell, validate_on_request_preflight,
    validate_ordinary_approval_cwd,
};
use super::types::{
    CaptureConfig, CaptureEvent, CaptureOperation, Channel, ClaudeCaptureOperation,
    CodexCaptureOperation, CodexRunScript, CommandSnapshot, PartialFailureClass, PartialOutcome,
    PartialRawCapture, PlatformMetadata, Provider, RawCapture, RedactionRoots,
};
use crate::launch::LaunchDescriptor;

fn capture_redaction_roots(
    command: &CommandSnapshot,
    approval_target: Option<&Path>,
    trusted_powershell: Option<&FileIdentity>,
) -> RedactionRoots {
    let cwd = command.cwd.clone();
    let repo = cwd
        .as_deref()
        .map(Path::new)
        .and_then(repository_root)
        .map(|path| path.to_string_lossy().into_owned());
    RedactionRoots {
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

const READER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);

/// Record one explicitly selected provider script into ignored raw storage.
pub async fn record(config: CaptureConfig) -> anyhow::Result<RawCapture> {
    RecordingSession::start(config).await?.finish().await
}

#[cfg(test)]
pub(super) async fn start_for_preflight_test(config: CaptureConfig) -> anyhow::Result<()> {
    RecordingSession::start(config).await.map(|_| ())
}

/// A live capture owns its child until a terminal frame or hard timeout.
///
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
    trusted_powershell: Option<FileIdentity>,
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    // Never read: its sole reader, `next_stdout`, was deleted along with
    // `codex_run`'s driving body (every `CodexRunScript` now bails before
    // reading a line). Still populated so the reader task that feeds it has
    // somewhere to send, matching `claude_run`'s equivalent dead weight above
    // — both disappear together when Task 7 deletes this file.
    #[allow(dead_code)]
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
        // Both pre-spawn checks below still run and can still bail — nothing
        // in this task touches the fence (decision #6 in the stage plan:
        // "every one of those runs before spawn"). Only the *identity* they
        // resolved stopped being stored: `codex_run`'s mid-loop rechecks
        // against it are gone along with the rest of that function (this
        // task's `record/scenarios/codex.rs::approval` does not repeat them
        // — see that function's own doc comment), so nothing downstream of
        // `start` ever reads it again.
        let trusted_powershell = match &config.scenario.operation {
            CaptureOperation::Codex(CodexCaptureOperation::Run {
                request,
                script: CodexRunScript::Approval,
            }) => {
                let cwd = Path::new(&request.cwd);
                let trusted = resolve_trusted_powershell(cwd, &config.raw_root)?;
                validate_ordinary_approval_cwd(cwd, None, true)?;
                Some(trusted)
            }
            _ => None,
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
            redaction_roots: capture_redaction_roots(
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
            CaptureOperation::Claude(ClaudeCaptureOperation::Run { request, .. }) => {
                self.claude_run(request).await
            }
            CaptureOperation::Codex(CodexCaptureOperation::Run { request, script }) => {
                self.codex_run(request, script).await
            }
            // Discovery is recorded through `capture::record` (the
            // `record/` module) before a `RecordingSession` is ever built;
            // `capture::record::record` routes every discovery operation
            // there and only falls back to `recording::record` (this type)
            // for a `Run` operation. See that module's dispatch.
            CaptureOperation::Claude(
                ClaudeCaptureOperation::ModelDiscovery
                | ClaudeCaptureOperation::ModelDiscoveryAt { .. }
                | ClaudeCaptureOperation::CommandDiscovery { .. },
            )
            | CaptureOperation::Codex(
                CodexCaptureOperation::ModelDiscovery
                | CodexCaptureOperation::ModelDiscoveryAt { .. },
            ) => unreachable!(
                "discovery operations are routed to capture::record before reaching RecordingSession::drive"
            ),
        }
    }

    /// Every Claude run script — `fresh-text`/`attachment`/`resume`/`checklist`/
    /// `checklist-resume`/`approval`, i.e. every `ClaudeRunScript` variant — is now ported to
    /// `record/scenarios/claude.rs`. Nothing reaches this function through a real capture any
    /// more: `capture::record()` (the new `record/` module) still falls back to
    /// `recording::record` for every `Run` operation until the SCENARIOS table is wired in (Task
    /// 7), and `comet-provider-capture.rs` still constructs and dispatches all six scripts, so
    /// the fallback route stays live traffic, not dead code — an accidental invocation here would
    /// otherwise silently drive a duplicate, unreviewed implementation of already-ported behavior
    /// against a real (token-spending) CLI. Bailing immediately, before a single line reaches the
    /// child's stdin, costs nothing: the process was already spawned by
    /// `RecordingSession::start`, but `finish()`'s `DriverError` path (`terminate_and_reap`) kills
    /// it having sent no prompt, so no tokens are spent. This entire function is dead weight that
    /// Task 7 deletes along with the rest of `recording.rs`.
    async fn claude_run(&mut self, _request: RunRequest) -> anyhow::Result<()> {
        bail!(
            "Claude run scenarios are ported to record/scenarios/claude.rs and are not driven \
             from here any more; the SCENARIOS table wires them into capture::record in Task 7."
        );
    }

    /// Every Codex run script — `fresh-text`/`resume`/`steer`/`interruption`
    /// (ported in the task before this one) and now `approval`/
    /// `approval-on-request` too, i.e. every `CodexRunScript` variant — is
    /// ported to `record/scenarios/codex.rs`. Nothing reaches this function
    /// through a real capture any more, for the same reason `claude_run`'s
    /// doc comment gives: `capture::record()` still falls back to
    /// `recording::record` for every `Run` operation until the SCENARIOS
    /// table is wired in (Task 7), and `comet-provider-capture.rs` still
    /// constructs and dispatches all six scripts, so the fallback route stays
    /// live traffic, not dead code — an accidental invocation here would
    /// otherwise silently drive a duplicate, unreviewed implementation of
    /// already-ported behavior against a real (token-spending) CLI. Bailing
    /// immediately, before a single line reaches the child's stdin, costs
    /// nothing: the process was already spawned by `RecordingSession::start`,
    /// but `finish()`'s `DriverError` path (`terminate_and_reap`) kills it
    /// having sent no prompt, so no tokens are spent.
    ///
    /// This entire function, like `claude_run` above, is dead weight that
    /// Task 7 deletes along with the rest of `recording.rs`. `script` is kept
    /// (not `_script`) purely so the message below still names which script
    /// was rejected.
    async fn codex_run(
        &mut self,
        _request: RunRequest,
        script: CodexRunScript,
    ) -> anyhow::Result<()> {
        bail!(
            "Codex {script:?} is ported to record/scenarios/codex.rs and is not driven from here \
             any more; the SCENARIOS table wires it into capture::record in Task 7."
        );
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
        CaptureOperation::Claude(ClaudeCaptureOperation::Run { request, .. }) => {
            Ok(crate::claude::run_launch(executable, request))
        }
        CaptureOperation::Codex(CodexCaptureOperation::Run { request, .. }) => {
            Ok(crate::codex::run_launch(executable, request))
        }
        // Discovery never reaches here — see the matching guard in `drive`.
        // Dead at the earliest point rather than late: a discovery config
        // that somehow arrived would otherwise resolve the executable,
        // build a launch, and spawn a real provider process before this
        // function even got a chance to say no.
        CaptureOperation::Claude(
            ClaudeCaptureOperation::ModelDiscovery
            | ClaudeCaptureOperation::ModelDiscoveryAt { .. }
            | ClaudeCaptureOperation::CommandDiscovery { .. },
        )
        | CaptureOperation::Codex(
            CodexCaptureOperation::ModelDiscovery | CodexCaptureOperation::ModelDiscoveryAt { .. },
        ) => unreachable!(
            "discovery operations are routed to capture::record before reaching select_launch"
        ),
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

#[cfg(test)]
mod tests {
    use comet_proto::{RunRequest, RuntimeMode};
    use serde_json::Value;

    use super::RecordingSession;
    use crate::capture::test_support::{config, fixture_path};
    use crate::capture::{
        CaptureOperation, ClaudeCaptureOperation, ClaudeRunScript, CodexCaptureOperation,
        CodexRunScript, sanitize_dir,
    };

    const APPROVAL_MARKER_NAME: &str = "capture-marker.txt";

    /// Break caught: a driving failure is discarded after a provider spawn, leaving no
    /// quarantine trail even though the child was safely stopped before ever being written to.
    ///
    /// Retargeted by the task that made `claude_run` an unconditional fail-fast bail (every
    /// `ClaudeRunScript` is ported to `record/scenarios/claude.rs`, so nothing here drives a real
    /// turn any more — see `claude_run`'s own doc comment). The original trigger — a fixture
    /// stopping before a terminal `result` frame, after partial transcript content — is no longer
    /// reachable at all: `claude_run` now bails before writing a single line, so there is no
    /// transcript left to preserve. What this test proves instead, and what still matters, is
    /// that the fail-fast bail is STILL correctly classified as a driver failure and STILL
    /// quarantines rather than silently discarding: the spawned child is killed, no `capture.json`
    /// is published, and a `partial-capture.json` is written (with zero events, since nothing was
    /// ever sent or received) that the sanitizer still refuses to treat as complete.
    #[tokio::test]
    async fn recorder_quarantines_evidence_after_claude_run_fail_fast() {
        let raw = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let request = RunRequest {
            prompt: "capture".into(),
            cwd: cwd.path().display().to_string(),
            ..RunRequest::for_session(RuntimeMode::ApprovalRequired)
        };
        let mut session = RecordingSession::start(config(
            "claude-fail-fast",
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
            "Claude run scenarios are ported to record/scenarios/claude.rs and are not driven \
             from here any more; the SCENARIOS table wires them into capture::record in Task 7."
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
        assert!(
            events.is_empty(),
            "the fail-fast bail must write nothing to the child's stdin before erroring: {events:?}"
        );

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

    /// Break caught: `codex_run`'s fail-fast for an already-ported script (`FreshText`/`Resume`/
    /// `Steer`/`Interruption`) regresses to driving a real turn again through this stripped-down
    /// body — the live-traffic hazard `codex_run`'s own doc comment describes: `capture::record()`
    /// still falls back to this exact path for every Codex `Run` operation until the SCENARIOS
    /// table is wired in (Task 7), and `comet-provider-capture.rs` still constructs and dispatches
    /// all six `CodexRunScript` values, so a regression here would silently mis-drive a real,
    /// token-spending capture (see `codex_run`'s doc comment for the specific mislabeling each
    /// script would produce). Covers `Resume` — the sharpest case, since its old branch chose
    /// `thread/resume` vs `thread/start` on `script`, the exact selection now bailed out before it
    /// can run.
    #[tokio::test]
    async fn recorder_bails_before_a_ported_codex_script_reaches_the_old_driver() {
        let raw = tempfile::tempdir().unwrap();
        let request = RunRequest {
            prompt: "capture".into(),
            cwd: std::env::temp_dir().display().to_string(),
            ..RunRequest::for_session(RuntimeMode::AutoAcceptEdits)
        };
        let mut session = RecordingSession::start(config(
            "codex-fail-fast",
            fixture_path("fake-codex"),
            CaptureOperation::Codex(CodexCaptureOperation::Run {
                request,
                script: CodexRunScript::Resume,
            }),
            raw.path(),
        ))
        .await
        .unwrap();
        let pid = session.child_id().expect("spawned child id");

        let error = session.finish().await.unwrap_err();

        assert_eq!(
            error.to_string(),
            "Codex Resume is ported to record/scenarios/codex.rs and is not driven from here \
             any more; the SCENARIOS table wires it into capture::record in Task 7."
        );
        assert!(!process_is_live(pid), "provider child {pid} remains live");
    }

    /// Break caught: a directory containing a successful-looking `capture.json` can bypass the
    /// quarantine marker and publish incomplete evidence.
    #[tokio::test]
    async fn sanitizer_rejects_partial_capture_even_beside_complete_shaped_raw() {
        let raw = tempfile::tempdir().unwrap();
        // Discovery now routes through `capture::record` (the new `record/`
        // module), not this module's own `record` — see `drive`'s comment.
        let mut capture = crate::capture::record(config(
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
