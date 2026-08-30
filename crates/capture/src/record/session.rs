//! The provider-neutral recording session.
//!
//! Everything here is a MOVE from `capture/recording.rs`, not a rewrite: the
//! child-process lifetime, reaping, timeout classification and
//! partial-capture persistence are the product of several Windows-specific
//! bug fixes and must survive exactly. Nothing in this file names Claude or
//! Codex — that split lives in `provider.rs` and `providers/`.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail};
use comet_proto::RunRequest;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::mpsc;

use super::provider::CaptureProvider;
use crate::safety::{DirectoryIdentity, FileIdentity, repository_root};
use crate::types::{
    CaptureConfig, CaptureEvent, Channel, CommandSnapshot, PartialFailureClass, PartialOutcome,
    PartialRawCapture, PlatformMetadata, Provider, RawCapture, RedactionRoots,
};
use comet_harness::launch::LaunchDescriptor;

const READER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
/// The fixed safety bound on the final exit wait, nested inside whichever
/// comes first: this or the scenario's own configured `timeout`. See
/// `Session::wait_for_exit`'s doc comment for why both exist and why only
/// one of them is ever the reason a wait fails as `Timeout`.
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);

/// The capitalized name user-facing copy calls the provider by. Kept
/// separate from `CaptureProvider::NAME` (lowercase, for the raw directory
/// name and internal tracing) rather than adding a second trait constant —
/// this is presentation, not identity, and it is a free function of the
/// four-member seam's own `Provider` return value, not something a provider
/// needs to declare for itself.
fn provider_display_name(provider: Provider) -> &'static str {
    match provider {
        Provider::Claude => "Claude",
        Provider::Codex => "Codex",
        // The family, not an agent: an ACP row names its adapter in the
        // scenario, and the operator-facing "not found" message here is about
        // node, which every adapter row spawns.
        Provider::Acp => "ACP",
    }
}

/// What the pre-spawn fence validated and needs to survive into the raw
/// capture's redaction roots, plus (for the one scenario that needs it) a
/// closure that re-runs the identity check right before spawn.
pub(super) struct FenceOutcome {
    pub(super) approval_target: Option<PathBuf>,
    // Read by `record/scenarios/codex.rs`'s `approval_on_request`, as the
    // expected identity for its grant-time `require_empty_approval_target`
    // recheck. Claude has no pre-spawn fence to record an identity from in
    // the first place (`record.rs::record_claude`'s doc comment) — its
    // `approval` scenario body (`record/scenarios/claude.rs`) still rechecks
    // at grant time, via `claude_marker_grant`, but that check recomputes a
    // fresh marker-shape/cwd match directly against the live filesystem
    // rather than comparing against an identity captured earlier, so this
    // field stays Codex-only.
    pub(super) approval_target_identity: Option<DirectoryIdentity>,
    // Read by `record/scenarios/codex.rs`'s `approval`, as the expected
    // identity for its grant-time `validate_ordinary_approval_cwd` recheck.
    // Same Codex-only note as above.
    pub(super) approval_cwd_identity: Option<DirectoryIdentity>,
    pub(super) trusted_powershell: Option<FileIdentity>,
    /// Re-validates the identity captured above, right before spawn — the
    /// TOCTOU window between `record.rs` building this `FenceOutcome` (which
    /// can involve real filesystem I/O) and `Session::start` actually
    /// launching the child (after `create_dir_all`, `probe_version`'s own
    /// subprocess round trip, and everything else `start` does first). Only
    /// `record::record_codex`'s `approval-on-request` fence sets this today
    /// — see its own doc comment for why the ordinary `approval` fence does
    /// not need a second check here (its protection continues at grant
    /// time, inside the scenario body, via `approval_cwd_identity` above).
    pub(super) recheck: Option<Box<dyn FnOnce() -> anyhow::Result<()> + Send>>,
}

impl FenceOutcome {
    pub(super) fn none() -> Self {
        Self {
            approval_target: None,
            approval_target_identity: None,
            approval_cwd_identity: None,
            trusted_powershell: None,
            recheck: None,
        }
    }
}

/// A live capture owns its child until a terminal frame or hard timeout.
pub(super) struct Session<P: CaptureProvider> {
    pub(super) provider: P,
    pub(super) timeout: Duration,
    pub(super) directory: PathBuf,
    pub(super) cli_version: String,
    pub(super) captured_at_unix_ms: i64,
    pub(super) scenario: String,
    pub(super) purpose: String,
    pub(super) command: CommandSnapshot,
    pub(super) fence: FenceOutcome,
    /// The `RunRequest` `record.rs`'s `derive_launch` built THIS launch from
    /// — `None` for a discovery scenario, which has no `RunRequest` at all.
    /// A run scenario's body reads this instead of rebuilding its own
    /// request, so the launch and the wire line can never describe two
    /// different requests. See `ScenarioLaunch`'s own doc comment
    /// (`scenarios.rs`) for the hazard this closes.
    pub(super) request: Option<RunRequest>,
    pub(super) child: Option<Child>,
    pub(super) stdin: Option<ChildStdin>,
    pub(super) stdout_lines: mpsc::UnboundedReceiver<String>,
    pub(super) readers: Vec<tokio::task::JoinHandle<()>>,
    pub(super) events: Arc<Mutex<Vec<CaptureEvent>>>,
    #[cfg(test)]
    pub(super) reap_notice: Option<std::sync::mpsc::SyncSender<u32>>,
    #[cfg(test)]
    pub(super) wait_error_once: bool,
}

impl<P: CaptureProvider> Session<P> {
    /// Spawn, attach both readers, and record the command snapshot.
    pub(super) async fn start(
        provider: P,
        config: &CaptureConfig,
        launch: LaunchDescriptor,
        mut fence: FenceOutcome,
        request: Option<RunRequest>,
    ) -> anyhow::Result<Self> {
        let command = CommandSnapshot::from_launch(&launch);
        // `cli_version` is the LAUNCHED PROGRAM's own `--version`, not the
        // agent's -- the same thing for Claude and Codex, where `program`
        // IS the agent's own binary, but a different program entirely for
        // ACP's two adapter rows (codex-acp, claude-agent-acp), whose
        // `program` is `node`. Both promoted ACP-adapter manifests read
        // `cli_version: "v22.23.2"` for exactly that reason -- Node's own
        // version, not the adapter package's. The corpus directory those
        // captures promote under (and the capability sheet's own title) is
        // named from a different field entirely, `agentInfo.version` off
        // the `initialize` reply, read once at promotion time rather than
        // stored here. See D102's "unrelated third sanitizer gap" section
        // for the fuller record.
        let cli_version = probe_version(&launch.program).await;
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
            P::NAME,
            config.scenario_name,
            uuid::Uuid::new_v4()
        ));
        tokio::fs::create_dir_all(&directory).await.map_err(|err| {
            tracing::debug!(path = %directory.display(), %err, "capture raw directory creation failed");
            anyhow!(
                "Raw capture storage could not be created. Check --raw-root permissions and try again."
            )
        })?;

        // The TOCTOU recheck, immediately before spawn — see `FenceOutcome::recheck`'s own doc
        // comment for why this window exists and why only `approval-on-request` populates it.
        if let Some(recheck) = fence.recheck.take() {
            recheck()?;
        }

        let mut child = launch.command().spawn().map_err(|err| {
            tracing::debug!(provider = P::NAME, cli = %launch.program.display(), %err, "capture provider spawn failed");
            anyhow!(
                "The {} CLI could not be started. Check --executable and try again.",
                provider_display_name(P::provider())
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
            timeout: config.timeout,
            directory,
            cli_version,
            captured_at_unix_ms,
            scenario: config.scenario_name.into(),
            purpose: config.purpose.into(),
            command,
            fence,
            request,
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
    pub(super) fn child_id(&self) -> Option<u32> {
        self.child.as_ref().and_then(Child::id)
    }

    /// Write one line to stdin, recording it as a stdin event first.
    pub(super) async fn send(&mut self, line: &str) -> anyhow::Result<()> {
        let Some(stdin) = self.stdin.as_mut() else {
            return protocol_stopped::<()>(provider_display_name(P::provider()), "stdin channel")
                .map(|_| ());
        };
        push_event(&self.events, Channel::Stdin, line.to_owned());
        stdin.write_all(line.as_bytes()).await.map_err(|err| {
            tracing::debug!(provider = P::NAME, %err, "capture stdin write failed");
            anyhow!(
                "The provider stopped accepting capture input. Retry with a current CLI version."
            )
        })?;
        stdin.write_all(b"\n").await.map_err(|err| {
            tracing::debug!(provider = P::NAME, %err, "capture stdin newline write failed");
            anyhow!(
                "The provider stopped accepting capture input. Retry with a current CLI version."
            )
        })?;
        stdin.flush().await.map_err(|err| {
            tracing::debug!(provider = P::NAME, %err, "capture stdin flush failed");
            anyhow!(
                "The provider stopped accepting capture input. Retry with a current CLI version."
            )
        })
    }

    /// Next parsed frame, or `None` when the provider's stdout ends.
    /// Non-frame lines are recorded (by the reader task, unconditionally)
    /// and skipped here, never returned.
    pub(super) async fn next_frame(&mut self) -> anyhow::Result<Option<serde_json::Value>> {
        while let Some(line) = self.stdout_lines.recv().await {
            if let Some(frame) = P::frame(&line) {
                return Ok(Some(frame));
            }
        }
        Ok(None)
    }

    /// Pump frames until `pick` yields. Errors only when stdout ends first —
    /// "the provider stopped talking", which is a fact about the process, not
    /// a judgment about a frame's shape.
    pub(super) async fn wait_for<T>(
        &mut self,
        expected: &'static str,
        mut pick: impl FnMut(&serde_json::Value) -> Option<T>,
    ) -> anyhow::Result<T> {
        while let Some(frame) = self.next_frame().await? {
            if let Some(value) = pick(&frame) {
                return Ok(value);
            }
        }
        protocol_stopped(provider_display_name(P::provider()), expected)
    }

    /// Pump frames until `P::turn_complete`.
    pub(super) async fn wait_for_turn_end(&mut self) -> anyhow::Result<()> {
        while let Some(frame) = self.next_frame().await? {
            if P::turn_complete(&frame) {
                return Ok(());
            }
        }
        protocol_stopped(provider_display_name(P::provider()), "turn completion")
    }

    /// Wait for the child to exit, bounded by whichever comes first:
    /// `deadline` (the scenario's overall configured timeout — the same
    /// deadline `record_generic` used for handshake and the scenario body,
    /// passed through here so the exit wait shares its clock instead of
    /// getting a fresh, unrelated budget) or the fixed [`CLEANUP_TIMEOUT`]
    /// safety bound. Which one actually fires is exactly what decides
    /// [`Session::finish`]'s classification: the deadline firing is
    /// [`PartialFailureClass::Timeout`] (the user's configured budget ran
    /// out); [`CLEANUP_TIMEOUT`] firing while budget remained, or a genuine
    /// I/O error reading the exit status, is
    /// [`PartialFailureClass::ProcessError`] (the process itself is the
    /// problem, not the clock).
    async fn wait_for_exit(
        &mut self,
        deadline: tokio::time::Instant,
    ) -> Result<Option<i32>, ExitWait> {
        #[cfg(test)]
        if std::mem::take(&mut self.wait_error_once) {
            return Err(ExitWait::failed(
                "The provider ended but its exit status could not be read. Retry the capture.",
            ));
        }
        let Some(child) = self.child.as_mut() else {
            return Ok(None);
        };
        let cleanup_deadline = tokio::time::Instant::now() + CLEANUP_TIMEOUT;
        let bound = deadline.min(cleanup_deadline);
        let bound_is_the_configured_deadline = bound == deadline;
        match tokio::time::timeout_at(bound, child.wait()).await {
            Ok(Ok(status)) => {
                self.child.take();
                Ok(status.code())
            }
            Ok(Err(err)) => {
                tracing::debug!(provider = P::NAME, %err, "capture child wait failed");
                Err(ExitWait::failed(
                    "The provider ended but its exit status could not be read. Retry the capture.",
                ))
            }
            Err(_) if bound_is_the_configured_deadline => Err(ExitWait::TimedOut),
            Err(_) => Err(ExitWait::failed(
                "The provider did not exit after its final response. It was stopped; retry the capture.",
            )),
        }
    }

    pub(super) async fn terminate_and_reap(&mut self) {
        self.stdin.take();
        if let Some(mut child) = self.child.take() {
            if let Err(err) = child.start_kill() {
                tracing::debug!(provider = P::NAME, %err, "capture child kill failed");
            }
            match tokio::time::timeout(CLEANUP_TIMEOUT, child.wait()).await {
                Ok(Ok(_)) => {}
                Ok(Err(err)) => {
                    tracing::debug!(provider = P::NAME, %err, "capture child reap failed");
                }
                Err(_) => {
                    tracing::warn!(provider = P::NAME, "capture child reap timed out");
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

    pub(super) fn raw_capture(&self, exit_code: Option<i32>) -> RawCapture {
        RawCapture {
            directory: self.directory.clone(),
            provider: P::provider(),
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
                self.fence.approval_target.as_deref(),
                self.fence.trusted_powershell.as_ref(),
            ),
            command: self.command.clone(),
            events: self.events.lock().expect("capture event lock").clone(),
            exit_code,
        }
    }

    pub(super) async fn persist_partial_after_failure(&self, failure_class: PartialFailureClass) {
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

    /// Close stdin, wait for exit under the hard timeout, drain readers,
    /// persist. On failure or timeout: terminate, reap, persist the partial.
    ///
    /// Called only after driving (handshake + scenario body) has already
    /// completed without error — a driving failure is classified by the
    /// caller (`record_generic`'s `Ok(Err(err))` branch: `DriverError`),
    /// which still owns `&mut self` at that point and never reaches `finish`.
    /// `deadline` is the SAME configured-timeout deadline that bounded
    /// driving: `recording.rs`'s original `finish` wrapped drive *and* the
    /// exit wait in one `timeout(self.timeout, …)`, and a budget that
    /// stopped covering the exit wait once this became two functions was
    /// exactly the regression a review caught — the exit wait would run for
    /// up to a fresh, unrelated `CLEANUP_TIMEOUT` (5s) past the caller's own
    /// configured budget, and misreport `ProcessError` where the true cause
    /// was the timeout.
    pub(super) async fn finish(
        mut self,
        deadline: tokio::time::Instant,
    ) -> anyhow::Result<RawCapture> {
        self.stdin.take();
        match self.wait_for_exit(deadline).await {
            Ok(exit_code) => {
                self.finish_readers().await;
                let capture = self.raw_capture(exit_code);
                persist_raw_capture(&capture).await?;
                persist_session_id(&capture).await;
                Ok(capture)
            }
            Err(ExitWait::TimedOut) => {
                self.terminate_and_reap().await;
                self.persist_partial_after_failure(PartialFailureClass::Timeout)
                    .await;
                bail!(
                    "Capture timed out after {} seconds. The provider was stopped; retry with --timeout-seconds up to 300.",
                    self.timeout.as_secs_f64()
                )
            }
            Err(ExitWait::Failed(err)) => {
                self.terminate_and_reap().await;
                self.persist_partial_after_failure(PartialFailureClass::ProcessError)
                    .await;
                Err(err)
            }
        }
    }
}

/// The two ways [`Session::wait_for_exit`] can end without a clean exit
/// code, kept distinct because [`Session::finish`] classifies them
/// differently.
enum ExitWait {
    /// The shared configured-timeout deadline fired.
    TimedOut,
    /// Something about the process itself — an I/O error reading its exit
    /// status, or it simply not exiting within [`CLEANUP_TIMEOUT`] of the
    /// deadline still having budget left.
    Failed(anyhow::Error),
}

impl ExitWait {
    fn failed(message: &str) -> Self {
        Self::Failed(anyhow!("{message}"))
    }
}

impl<P: CaptureProvider> Drop for Session<P> {
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
        home: comet_harness::home_dir().map(|path| path.to_string_lossy().into_owned()),
        temp: Some(std::env::temp_dir().to_string_lossy().into_owned()),
        codex_home: command.configured_env.get("CODEX_HOME").cloned(),
        claude_config_dir: command.configured_env.get("CLAUDE_CONFIG_DIR").cloned(),
        approval_target: approval_target.map(|path| path.to_string_lossy().into_owned()),
        trusted_powershell: trusted_powershell
            .map(|identity| identity.canonical.to_string_lossy().into_owned()),
    }
}

pub(super) fn push_event(
    events: &Arc<Mutex<Vec<CaptureEvent>>>,
    channel: Channel,
    payload: String,
) {
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

pub(super) fn protocol_stopped<T>(provider: &str, expected: &str) -> anyhow::Result<T> {
    tracing::debug!(
        provider,
        expected,
        "capture protocol ended before expected response"
    );
    bail!("{provider} stopped before the expected {expected}. Retry with a current CLI version.")
}

pub(super) async fn probe_version(executable: &Path) -> String {
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

pub(super) fn resolve_executable(
    provider: Provider,
    resolved: Option<PathBuf>,
) -> anyhow::Result<PathBuf> {
    resolved.ok_or_else(|| {
        anyhow!(
            "The {} CLI was not found. Install it or pass --executable with its path.",
            provider_display_name(provider)
        )
    })
}

pub(super) fn absolute_from_parent(path: PathBuf) -> anyhow::Result<PathBuf> {
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

/// The file name a recorded session id is written to, beside `capture.json`.
pub const SESSION_ID_FILE: &str = "session-id.txt";

/// Write the provider session this capture opened, so a later `--resume-id` can
/// name the capture instead of a copied string (D62).
///
/// **Best-effort on purpose.** A discovery scenario opens no session and a
/// provider may spell it somewhere this does not look; neither is a reason to
/// fail a capture that already succeeded. What the file buys is that the
/// pairing between a resume and the session it resumes stops being a hand
/// copy — nothing downstream requires it to exist.
async fn persist_session_id(capture: &RawCapture) {
    let Some(id) = recorded_session_id(&capture.events) else {
        return;
    };
    let path = capture.directory.join(SESSION_ID_FILE);
    if let Err(err) = tokio::fs::write(&path, &id).await {
        tracing::debug!(path = %path.display(), %err, "session id could not be written");
    }
}

/// The first session identifier any recorded frame carries.
///
/// **A union of the providers' spellings rather than a trait member.** Claude
/// says `session_id`, Codex and ACP say `sessionId`, Codex's thread frames say
/// `threadId`. Reading all three here keeps the knowledge in one short function
/// instead of widening `CaptureProvider`'s seam, which `AGENTS.md` is explicit
/// about after spawn had to be moved back off it.
fn recorded_session_id(events: &[crate::types::CaptureEvent]) -> Option<String> {
    fn find(value: &serde_json::Value) -> Option<String> {
        match value {
            serde_json::Value::Object(map) => {
                for key in ["session_id", "sessionId", "threadId"] {
                    if let Some(serde_json::Value::String(id)) = map.get(key)
                        && !id.is_empty()
                    {
                        return Some(id.clone());
                    }
                }
                map.values().find_map(find)
            }
            serde_json::Value::Array(items) => items.iter().find_map(find),
            _ => None,
        }
    }

    events
        .iter()
        .filter_map(|event| serde_json::from_str::<serde_json::Value>(&event.payload).ok())
        .find_map(|value| find(&value))
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

pub(super) fn persist_immutable_bytes(directory: &Path, bytes: &[u8]) -> std::io::Result<()> {
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

    /// Break caught (D62): the resume id had to be regexed out of the previous
    /// run's `capture.json` by eye, and nothing checked it came from a
    /// compatible scenario. Writing it beside the capture is what makes the
    /// pairing mechanical.
    #[test]
    fn a_recorded_session_id_is_found_whatever_the_provider_calls_it() {
        for (payload, expected) in [
            (r#"{"type":"system","session_id":"claude-1"}"#, "claude-1"),
            (r#"{"params":{"sessionId":"acp-1"}}"#, "acp-1"),
            (r#"{"params":{"thread":{"threadId":"codex-1"}}}"#, "codex-1"),
        ] {
            assert_eq!(
                recorded_session_id(&events_of(&[payload])).as_deref(),
                Some(expected)
            );
        }
    }

    /// A discovery capture opens no session, and that is not a failure — the
    /// file is simply absent and `--resume-id` still takes a bare id.
    #[test]
    fn a_capture_with_no_session_records_none() {
        let events = events_of(&[r#"{"type":"system","subtype":"init"}"#]);
        assert_eq!(recorded_session_id(&events), None);
    }

    /// An empty id is not an id. A provider that sent the key with `""` would
    /// otherwise produce a file that fails the resume later, further from the
    /// cause.
    #[test]
    fn an_empty_session_id_is_not_recorded() {
        let events = events_of(&[r#"{"session_id":"","sessionId":"real-1"}"#]);
        assert_eq!(recorded_session_id(&events).as_deref(), Some("real-1"));
    }

    fn events_of(payloads: &[&str]) -> Vec<crate::types::CaptureEvent> {
        payloads
            .iter()
            .enumerate()
            .map(|(i, payload)| crate::types::CaptureEvent {
                sequence: i as u64 + 1,
                channel: crate::types::Channel::Stdout,
                payload: (*payload).to_string(),
            })
            .collect()
    }

    use std::sync::atomic::{AtomicBool, Ordering};

    use comet_proto::{RunRequest, RuntimeMode};
    use serde_json::Value;

    use super::*;
    use crate::record::provider::CaptureProvider;
    use crate::record::providers::claude::ClaudeProvider;
    use crate::record::scenarios::ScenarioInput;
    use crate::test_support::{config, find_named_file, fixture_path};

    /// The new behavior the neutral-recorder stage adds: `next_frame` records a non-JSON
    /// stdout line (on the stdout channel) and keeps pumping instead of
    /// erroring, then still returns the real frame that follows it.
    #[tokio::test]
    async fn session_records_non_frame_lines_and_keeps_pumping() {
        let raw = tempfile::tempdir().unwrap();
        let executable = fixture_path("fake-claude");
        let request = RunRequest {
            prompt: "scenario:capture-non-frame-tolerance".into(),
            ..RunRequest::for_session(RuntimeMode::AutoAcceptEdits)
        };
        let launch = comet_harness::claude::run_launch(&executable, &request);
        let cfg = config(
            "claude-non-frame-tolerance",
            executable,
            "claude",
            raw.path(),
        );
        let mut session = Session::start(ClaudeProvider, &cfg, launch, FenceOutcome::none(), None)
            .await
            .unwrap();
        let line = comet_harness::claude::wire::user_message_line_with_images(&request.prompt, &[]);
        session.send(&line).await.unwrap();

        let frame = session.next_frame().await.unwrap().expect("a real frame");
        assert_eq!(frame["type"], "result");

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let capture = session.finish(deadline).await.unwrap();
        assert!(
            capture.events.iter().any(|event| {
                event.channel == Channel::Stdout && event.payload == "not json, a progress line"
            }),
            "the non-frame line must still be recorded on stdout"
        );
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
            push_event(
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
        let mut session = Session {
            provider: ClaudeProvider,
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
                stdin: comet_harness::launch::StdioMode::Piped,
                stdout: comet_harness::launch::StdioMode::Piped,
                stderr: comet_harness::launch::StdioMode::Piped,
                kill_on_drop: true,
                #[cfg(windows)]
                creation_flags: 0,
            },
            fence: FenceOutcome::none(),
            request: None,
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

    /// Break caught: `finish`'s exit wait uses a budget disconnected from the shared `deadline` it
    /// was passed, silently giving the process up to `CLEANUP_TIMEOUT` (5s) past the caller's own
    /// configured timeout and misclassifying the eventual result as `ProcessError` instead of
    /// `Timeout` — the exact regression a review caught when driving and the exit wait split into
    /// two functions. The child here never exits; the deadline is far shorter than
    /// `CLEANUP_TIMEOUT`, so only honoring it (not just the fixed 5s bound) makes this fail fast.
    #[tokio::test]
    async fn finish_shares_the_configured_deadline_with_the_exit_wait() {
        let raw = tempfile::tempdir().unwrap();
        let executable = fixture_path("fake-claude-discovery-stall");
        let input = ScenarioInput::default();
        let launch =
            crate::record::scenarios::claude::model_discovery_launch(&input, &executable).unwrap();
        let cfg = config("claude-finish-deadline", executable, "claude", raw.path());
        let session = Session::start(ClaudeProvider, &cfg, launch, FenceOutcome::none(), None)
            .await
            .unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(100);

        let error = session.finish(deadline).await.unwrap_err();

        assert!(error.to_string().contains("timed out"), "{error}");
    }

    /// Break caught: a child-wait I/O error discards the only child handle before the outer
    /// failure cleanup can attempt kill/reap and finalize the partial transcript.
    #[tokio::test]
    async fn wait_error_retains_child_for_cleanup_and_quarantine() {
        let raw = tempfile::tempdir().unwrap();
        let executable = fixture_path("fake-claude");
        let input = ScenarioInput::default();
        let launch =
            crate::record::scenarios::claude::model_discovery_launch(&input, &executable).unwrap();
        let cfg = config("claude-wait-error", executable, "claude", raw.path());
        let mut session = Session::start(ClaudeProvider, &cfg, launch, FenceOutcome::none(), None)
            .await
            .unwrap();
        let pid = session.child_id().expect("spawned child id");
        let directory = session.directory.clone();
        ClaudeProvider::handshake(&mut session, &input)
            .await
            .unwrap();
        session.wait_error_once = true;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let error = session.finish(deadline).await.unwrap_err();

        assert!(error.to_string().contains("exit status could not be read"));
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
        let executable = fixture_path("fake-claude");
        let request = RunRequest {
            prompt: "scenario:interrupt".into(),
            ..RunRequest::for_session(RuntimeMode::ApprovalRequired)
        };
        let launch = comet_harness::claude::run_launch(&executable, &request);
        let cfg = config("claude-drop", executable, "claude", raw.path());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut session = runtime
            .block_on(Session::start(
                ClaudeProvider,
                &cfg,
                launch,
                FenceOutcome::none(),
                None,
            ))
            .unwrap();
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

    /// Break caught: setup/probe/spawn errors manufacture an incomplete provider transcript even
    /// though no provider process and therefore no observed protocol frame existed.
    #[tokio::test]
    async fn recorder_failure_before_spawn_creates_no_partial_capture() {
        let raw = tempfile::tempdir().unwrap();
        let missing = raw.path().join("missing-provider-executable");
        let input = ScenarioInput::default();
        let launch =
            crate::record::scenarios::claude::model_discovery_launch(&input, &missing).unwrap();
        let cfg = config("claude-pre-spawn-failure", missing, "claude", raw.path());
        let result = Session::start(ClaudeProvider, &cfg, launch, FenceOutcome::none(), None).await;
        let error = result
            .err()
            .expect("missing executable must fail before spawn");
        assert!(error.to_string().contains("could not be started"));
        assert!(!find_named_file(raw.path(), "partial-capture.json"));
    }

    /// Break caught: failure to quarantine evidence replaces the safe protocol error with a raw
    /// storage error that may disclose a local path or provider value.
    ///
    /// The driving failure here is real, not hand-typed: the stall fixture's
    /// non-bare (command-discovery) path exits without ever answering the
    /// initialize request, so `handshake` genuinely returns the "stopped
    /// before the expected reply" error `record_generic`'s `Ok(Err(err))`
    /// branch would forward — this test proves persistence failing to
    /// quarantine (because a partial file already exists) does not replace
    /// *that* error with a raw storage error, not a synthetic stand-in.
    #[tokio::test]
    async fn partial_persistence_failure_preserves_the_original_safe_error() {
        let raw = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let executable = fixture_path("fake-claude-discovery-stall");
        let input = ScenarioInput {
            cwd: Some(cwd.path().into()),
            ..ScenarioInput::default()
        };
        let launch =
            crate::record::scenarios::claude::command_discovery_launch(&input, &executable)
                .unwrap();
        let cfg = config(
            "claude-partial-write-failure",
            executable,
            "claude",
            raw.path(),
        );
        let mut session = Session::start(ClaudeProvider, &cfg, launch, FenceOutcome::none(), None)
            .await
            .unwrap();
        let directory = session.directory.clone();
        std::fs::write(
            directory.join("partial-capture.json"),
            b"existing quarantine",
        )
        .unwrap();

        let error = ClaudeProvider::handshake(&mut session, &input)
            .await
            .unwrap_err();
        session.terminate_and_reap().await;
        session
            .persist_partial_after_failure(PartialFailureClass::DriverError)
            .await;
        let error = error.to_string();

        assert!(error.contains("stopped before the expected"), "{error}");
        assert!(!error.contains(&directory.display().to_string()));
        assert_eq!(
            std::fs::read(directory.join("partial-capture.json")).unwrap(),
            b"existing quarantine"
        );
    }

    /// Break caught: `start` stops calling `fence.recheck` at all (e.g. the call the
    /// neutral-recorder stage added right after `create_dir_all` is deleted, or never wired in
    /// the first place). Every other
    /// test in this file uses `FenceOutcome::none()`, whose `recheck` is always `None`, so none of
    /// them can tell whether `start` actually invokes a `Some(recheck)` it was handed — this
    /// drives that invocation directly and observably, independent of `record::codex_fence`'s own
    /// construction of the closure (covered separately in `record.rs`'s tests).
    #[tokio::test]
    async fn start_runs_the_fence_recheck_after_directory_creation_and_before_spawn() {
        let raw = tempfile::tempdir().unwrap();
        let executable = fixture_path("fake-claude");
        let input = ScenarioInput::default();
        let launch =
            crate::record::scenarios::claude::model_discovery_launch(&input, &executable).unwrap();
        let cfg = config(
            "claude-fence-recheck-runs",
            executable,
            "claude",
            raw.path(),
        );
        let ran = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&ran);
        let fence = FenceOutcome {
            recheck: Some(Box::new(move || {
                flag.store(true, Ordering::SeqCst);
                Ok(())
            })),
            ..FenceOutcome::none()
        };

        let session = Session::start(ClaudeProvider, &cfg, launch, fence, None)
            .await
            .unwrap();

        assert!(
            ran.load(Ordering::SeqCst),
            "Session::start must run a Some(recheck) it was handed"
        );
        drop(session);
    }

    /// Break caught: `start` runs `fence.recheck` but ignores its `Err`, spawning the provider
    /// anyway instead of aborting before it — the TOCTOU protection `FenceOutcome::recheck` exists
    /// for would then be advisory only.
    #[tokio::test]
    async fn start_aborts_before_spawn_when_the_fence_recheck_fails() {
        let raw = tempfile::tempdir().unwrap();
        let executable = fixture_path("fake-claude");
        let input = ScenarioInput::default();
        let launch =
            crate::record::scenarios::claude::model_discovery_launch(&input, &executable).unwrap();
        let cfg = config(
            "claude-fence-recheck-aborts",
            executable,
            "claude",
            raw.path(),
        );
        let fence = FenceOutcome {
            recheck: Some(Box::new(|| {
                anyhow::bail!("synthetic recheck failure — the target changed underneath us")
            })),
            ..FenceOutcome::none()
        };

        let error = match Session::start(ClaudeProvider, &cfg, launch, fence, None).await {
            Ok(_) => panic!("a failing recheck must abort before spawn"),
            Err(error) => error,
        };

        assert!(
            error.to_string().contains("synthetic recheck failure"),
            "{error}"
        );
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
