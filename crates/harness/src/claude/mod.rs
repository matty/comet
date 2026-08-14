//! Claude Code harness: spawns the installed `claude` CLI and speaks its
//! stream-json protocol directly (spec: docs/research/harness.md; behavior
//! ported from comet's `packages/harness/src/claude.ts`).
//!
//! - stdout JSONL frames are normalized into [`AgentEvent`]s (init dedupe,
//!   subagent filtering, typed tool decoding, error-code mapping).
//! - The bidirectional control channel is served: `can_use_tool` requests
//!   round-trip through [`RunControls::request_approval`], except
//!   `AskUserQuestion` which round-trips through [`RunControls::request_input`]
//!   (InputRequested → answers → InputResolved) instead.
//! - Steering: queued [`SteerMessage`]s are written to stdin as user lines at
//!   any time; the CLI applies them at its own step boundary.
//! - Interrupt: cancelling [`RunControls::interrupt`] sends the protocol-level
//!   interrupt control request, then escalates to SIGTERM and SIGKILL.
//!
//! `claude/2.1.228/attachment` frame 1 pins attachment stdin ordering: captured
//! attachment turns put image content blocks before text.

mod approval;
mod catalog;
pub(crate) mod commands;
pub(crate) mod discovery;
mod normalize;
mod update;
pub(crate) mod wire;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;

use comet_proto::{
    AgentEvent, ApprovalDecision, ApprovalRequest, DiagnosticSeverity, DoneStatus,
    HarnessCapabilities, HarnessId, HarnessProbe, InstallMethod, ModelCatalog, ReasoningLevel,
    RunRequest, RuntimeMode, SteeringMode, UserInputAnswer, UserInputQuestion,
};

use crate::{Harness, HarnessError, RunControls, Signal, send_signal};
use catalog::{apply_ultrathink, static_models, to_effort};
use normalize::Normalizer;
use wire::{
    ControlRequestFrame, Frame, allow_response, cancelled_response, control_response_line,
    deny_response,
};

/// Locate the device's installed Claude Code CLI: `CLAUDE_CODE_EXECUTABLE`,
/// then our own PATH, then the system's own PATH (a GUI/service launch's PATH
/// misses what the user's shell init shapes on unix, and goes stale against
/// the persisted environment on Windows — see [`crate::shell_env`]), then
/// known install locations as a last resort. Resolved per call — cheap after
/// the snapshot is cached.
pub fn resolve_claude_executable() -> Option<PathBuf> {
    crate::resolve_cli(
        "CLAUDE_CODE_EXECUTABLE",
        "claude",
        crate::all_known_dirs(claude_install_dirs()),
    )
}

/// Where a Claude Code CLI lands when PATH doesn't name it, each tagged with
/// what finding it there means (see [`crate::KnownDir`]).
fn claude_install_dirs() -> Vec<crate::KnownDir> {
    let mut dirs: Vec<crate::KnownDir> = Vec::new();
    if let Some(home) = crate::home_dir() {
        dirs.push((home.join(".claude").join("local"), InstallMethod::Native));
        // The native installer's per-user dir on every platform, `claude.exe`
        // included.
        dirs.push((home.join(".local").join("bin"), InstallMethod::Native));
    }
    if cfg!(windows) {
        dirs.extend(crate::env_dir("LOCALAPPDATA").map(|d| {
            (
                d.join("Microsoft").join("WinGet").join("Links"),
                InstallMethod::Winget,
            )
        }));
        if let Some(home) = crate::home_dir() {
            dirs.push((home.join("scoop").join("shims"), InstallMethod::Scoop));
        }
    } else {
        dirs.push((PathBuf::from("/opt/homebrew/bin"), InstallMethod::Homebrew));
        // `/usr/local/bin` is deliberately NOT tagged Homebrew. It is Intel
        // Homebrew's prefix, a manual `cp`, and half a dozen installers'
        // fallback all at once, so naming one of them would be a guess
        // presented as a fact.
        dirs.push((PathBuf::from("/usr/local/bin"), InstallMethod::Unknown));
    }
    dirs
}

fn option_is_on(options: &serde_json::Map<String, Value>, key: &str) -> bool {
    match options.get(key) {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s == "on" || s == "true",
        _ => false,
    }
}

/// Describe the exact process launch used for a Claude run.
pub(crate) fn run_launch(exe: &Path, request: &RunRequest) -> crate::capture::LaunchDescriptor {
    let mut args: Vec<std::ffi::OsString> = [
        "--print",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        // Route permission prompts to the stdio control channel so
        // `can_use_tool` (and AskUserQuestion in particular) reaches us.
        "--permission-prompt-tool",
        "stdio",
    ]
    .into_iter()
    .map(Into::into)
    .collect();
    // The 1M context window is selected via a model-id suffix
    // (`sonnet[1m]`), exactly how the CLI itself does it; fast mode and
    // always-on thinking are settings overrides.
    if let Some(model) = &request.model {
        let one_m = request
            .model_options
            .get("contextWindow")
            .and_then(Value::as_str)
            == Some("1m");
        args.push("--model".into());
        args.push(if one_m {
            format!("{model}[1m]").into()
        } else {
            model.into()
        });
    }
    if let Some(effort) = to_effort(request.reasoning, request.model.as_deref()) {
        args.extend(["--effort".into(), effort.into()]);
    }
    // `default` is the CLI's unadvertised alias for the mode it now lists
    // as `manual`; both ask before each tool call. Keep `default` because it
    // is accepted by every CLI version comet resolves, and `manual` is not.
    let (permission_mode, skip_permissions) = match request.runtime_mode {
        RuntimeMode::ApprovalRequired => ("default", false),
        RuntimeMode::AutoAcceptEdits => ("acceptEdits", false),
        RuntimeMode::Auto => ("auto", false),
        RuntimeMode::FullAccess => ("bypassPermissions", true),
    };
    args.extend(["--permission-mode".into(), permission_mode.into()]);
    if skip_permissions {
        args.push("--dangerously-skip-permissions".into());
    }
    if let Some(resume) = &request.resume {
        args.push(format!("--resume={resume}").into());
    }
    let mut settings = serde_json::Map::new();
    if option_is_on(&request.model_options, "fastMode") {
        settings.insert("fastMode".into(), Value::Bool(true));
    }
    if option_is_on(&request.model_options, "thinking") {
        settings.insert("alwaysThinkingEnabled".into(), Value::Bool(true));
    }
    if request.reasoning == Some(ReasoningLevel::Ultracode) {
        settings.insert("ultracode".into(), Value::Bool(true));
    }
    if !settings.is_empty() {
        args.push("--settings".into());
        args.push(Value::Object(settings).to_string().into());
    }

    let mut configured_env = std::collections::BTreeMap::new();
    if let Some(path) = crate::child_path(exe) {
        configured_env.insert("PATH".into(), path);
    }
    crate::capture::LaunchDescriptor {
        program: exe.into(),
        args,
        cwd: (!request.cwd.is_empty()).then(|| request.cwd.clone().into()),
        configured_env,
        stdin: crate::capture::StdioMode::Piped,
        stdout: crate::capture::StdioMode::Piped,
        stderr: crate::capture::StdioMode::Piped,
        kill_on_drop: true,
        #[cfg(windows)]
        creation_flags: 0,
    }
}

/// Build the exact process command used for a Claude run.
pub(crate) fn build_run_command(exe: &Path, request: &RunRequest) -> Command {
    run_launch(exe, request).command()
}

/// The Claude Code harness. Construct with [`ClaudeHarness::new`]; tests point
/// it at a fake CLI with [`ClaudeHarness::with_executable`].
pub struct ClaudeHarness {
    executable: Option<PathBuf>,
    /// Grace between the interrupt control request and SIGTERM.
    interrupt_grace: Duration,
    /// Grace between SIGTERM and SIGKILL.
    kill_grace: Duration,
    /// One handshake per boot. `models()` is also called by titling
    /// (`crates/engine/src/titles.rs:159`) on every title generation, so an
    /// uncached discovery would spawn a CLI on a path the user never sees —
    /// and each spawn runs the user's `SessionStart` hooks.
    discovery_cache: crate::discovery::DiscoveryCache,
    /// One command handshake per directory per boot. Separate from
    /// `discovery_cache` because commands are cwd-scoped and models are not,
    /// and because this one's spawn is the expensive non-bare handshake that
    /// runs the user's `SessionStart` hooks.
    command_cache: crate::discovery::CommandCache,
}

impl Default for ClaudeHarness {
    fn default() -> Self {
        Self {
            executable: None,
            interrupt_grace: Duration::from_secs(2),
            kill_grace: Duration::from_secs(3),
            discovery_cache: crate::discovery::DiscoveryCache::default(),
            command_cache: crate::discovery::CommandCache::default(),
        }
    }
}

impl ClaudeHarness {
    pub fn new() -> Self {
        Self::default()
    }

    /// The single declaration of what Claude Code can honor. `default_registry`
    /// calls this for the lazy slot's descriptor and the trait impl returns it
    /// once resolved, so the catalog cannot change on first use.
    ///
    /// Steering rides the persistent stdin stream, so an accepted steer lands
    /// at the next step boundary within the live turn.
    pub fn capabilities() -> HarnessCapabilities {
        HarnessCapabilities {
            supports_steering: true,
            steering_mode: SteeringMode::StepBoundary,
            reasoning_levels: vec![
                ReasoningLevel::Low,
                ReasoningLevel::Medium,
                ReasoningLevel::High,
                ReasoningLevel::XHigh,
                ReasoningLevel::Max,
            ],
            runtime_modes: vec![
                RuntimeMode::ApprovalRequired,
                RuntimeMode::AutoAcceptEdits,
                RuntimeMode::Auto,
                RuntimeMode::FullAccess,
            ],
            // `deny_response(message)` puts the user's note on the provider
            // wire, so the adapter can truthfully advertise this capability.
            carries_deny_note: true,
        }
    }

    /// Use a fixed CLI binary instead of PATH/known-location resolution.
    pub fn with_executable(mut self, path: impl Into<PathBuf>) -> Self {
        self.executable = Some(path.into());
        // A cached answer belongs to the CLI that gave it. Carried across a
        // change of executable it would be replayed for a binary that was
        // never asked, and the picker would show one CLI's models under
        // another's name.
        self.discovery_cache = crate::discovery::DiscoveryCache::default();
        self.command_cache = crate::discovery::CommandCache::default();
        self
    }

    /// Tune the interrupt→SIGTERM→SIGKILL escalation timing.
    pub fn with_graces(mut self, interrupt_grace: Duration, kill_grace: Duration) -> Self {
        self.interrupt_grace = interrupt_grace;
        self.kill_grace = kill_grace;
        self
    }

    fn resolve_executable(&self) -> Result<PathBuf, HarnessError> {
        if let Some(p) = &self.executable {
            return Ok(p.clone());
        }
        resolve_claude_executable().ok_or_else(|| {
            HarnessError::NotInstalled(crate::not_installed_message(
                "claude",
                "CLAUDE_CODE_EXECUTABLE",
            ))
        })
    }
}

#[async_trait]
impl Harness for ClaudeHarness {
    fn id(&self) -> HarnessId {
        HarnessId::ClaudeCode
    }
    fn display_name(&self) -> &str {
        "Claude Code"
    }
    fn capabilities(&self) -> HarnessCapabilities {
        Self::capabilities()
    }

    async fn probe(&self) -> HarnessProbe {
        let mut probe = crate::probe_installed_cli(
            self.resolve_executable(),
            "claude",
            "CLAUDE_CODE_EXECUTABLE",
            crate::all_known_dirs(claude_install_dirs()),
        )
        .await;
        // No installed version is passed, unlike Codex: there is nothing to
        // compare it against. Claude publishes no latest version, so this
        // reports what its updater last did and never a verdict on currency.
        probe.update =
            update::read_resolved_update(probe.install.as_ref(), update::claude_home().as_deref());
        probe
    }

    /// The curated catalog (see [`catalog`]) unioned with whatever the live
    /// handshake reported. An absent CLI still surfaces as
    /// [`HarnessError::NotInstalled`] rather than as a failed discovery: the
    /// user's action is different, and the picker's caption is not the place to
    /// say "no CLI".
    async fn models(&self) -> Result<ModelCatalog, HarnessError> {
        let exe = self.resolve_executable()?;
        let curated = static_models();
        let curated_ids: Vec<String> = curated.iter().map(|m| m.id.clone()).collect();
        let discovery = self
            .discovery_cache
            .get(move || discovery::discover(exe, curated_ids))
            .await;
        Ok(self.discovery_cache.catalog(curated, discovery))
    }

    async fn commands(&self, cwd: &str) -> Result<Vec<comet_proto::AgentCommand>, HarnessError> {
        let exe = self.resolve_executable()?;
        let dir = PathBuf::from(cwd);
        // A failure must NOT degrade to an empty list. An empty list is a real
        // answer — a directory whose CLI offers no commands — and rendering
        // "couldn't reach Claude" as "no commands" is the same class of
        // confident-wrong-answer the `--bare` bug and the logged-out Codex
        // model list both were. The caller gets an error and says so.
        self.command_cache
            .get(cwd, move || commands::discover_commands(exe, dir))
            .await
            .map_err(|failure| {
                HarnessError::Protocol(format!("claude command discovery failed: {failure:?}"))
            })
    }

    fn clear_discovery(&self) {
        self.discovery_cache.clear();
        self.command_cache.clear();
    }

    fn take_unreported_discovery_failure(&self) -> Option<crate::discovery::DiscoveryFailure> {
        self.discovery_cache.take_unreported_failure()
    }

    async fn run(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let exe = self.resolve_executable()?;
        let mut cmd = build_run_command(&exe, &request);
        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                HarnessError::NotInstalled(exe.display().to_string())
            } else {
                HarnessError::Io(e)
            }
        })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| HarnessError::Protocol("claude child has no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| HarnessError::Protocol("claude child has no stdout".into()))?;
        let stderr_tail = crate::StderrTail::default();
        if let Some(stderr) = child.stderr.take() {
            let tail = stderr_tail.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(target: "comet_harness::claude", "stderr: {line}");
                    tail.push(&line);
                }
            });
        }

        let (stdin_tx, stdin_rx) = mpsc::unbounded_channel::<StdinMsg>();
        tokio::spawn(stdin_writer(stdin, stdin_rx));

        // The initial prompt as the first stdin user line (streaming-input
        // mode). Ultrathink rides every user message — steers included.
        // Staged image attachments are inlined as base64 image content blocks
        // ahead of the text (verified against the real CLI); their path refs
        // also ride the prompt text, so a skipped/unreadable file degrades to
        // the old-app behavior (the agent opens the path with its Read tool).
        let images = load_image_blocks(&request.attachments).await;
        let first = wire::user_message_line_with_images(
            &apply_ultrathink(request.reasoning, &request.prompt),
            &images,
        );
        let _ = stdin_tx.send(StdinMsg::Line(first));

        let (event_tx, event_rx) = mpsc::channel::<Result<AgentEvent, HarnessError>>(256);
        tokio::spawn(run_session(Session {
            child,
            stdout_lines: BufReader::new(stdout).lines(),
            stdin_tx,
            event_tx,
            controls,
            reasoning: request.reasoning,
            runtime_mode: request.runtime_mode,
            interrupt_grace: self.interrupt_grace,
            kill_grace: self.kill_grace,
            stderr_tail,
        }));

        Ok(futures::stream::unfold(event_rx, |mut rx| async move {
            rx.recv().await.map(|ev| (ev, rx))
        })
        .boxed())
    }
}

enum StdinMsg {
    Line(String),
    /// Close stdin (end of steering input): the CLI finishes the current turn
    /// and exits, which ends the run stream at stdout EOF.
    Close,
}

/// Anthropic's API caps inline images at 5MB of raw bytes; larger files stay
/// path refs only.
const MAX_INLINE_IMAGE_BYTES: u64 = 5 * 1024 * 1024;

/// Media type for an inline image block — extension first, magic bytes as the
/// fallback (pasted screenshots may carry odd names). Only the API-supported
/// inline types map; anything else (svg/bmp/tiff/…) returns `None`.
fn image_media_type(path: &std::path::Path, bytes: &[u8]) -> Option<&'static str> {
    let by_ext = match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => Some("image/png"),
        Some("jpg" | "jpeg") => Some("image/jpeg"),
        Some("gif") => Some("image/gif"),
        Some("webp") => Some("image/webp"),
        _ => None,
    };
    by_ext.or(match bytes {
        [0x89, b'P', b'N', b'G', ..] => Some("image/png"),
        [0xFF, 0xD8, 0xFF, ..] => Some("image/jpeg"),
        [b'G', b'I', b'F', b'8', ..] => Some("image/gif"),
        [
            b'R',
            b'I',
            b'F',
            b'F',
            _,
            _,
            _,
            _,
            b'W',
            b'E',
            b'B',
            b'P',
            ..,
        ] => Some("image/webp"),
        _ => None,
    })
}

/// Load `RunRequest::attachments` into inline image blocks, best-effort: an
/// unreadable, oversized, or unsupported file is skipped — its path ref still
/// rides the prompt text — never fatal to the run.
pub(crate) async fn load_image_blocks(paths: &[String]) -> Vec<wire::ImageBlock> {
    use base64::Engine as _;
    let mut blocks = Vec::new();
    for path in paths {
        let bytes = match tokio::fs::read(path).await {
            Ok(bytes) => bytes,
            Err(err) => {
                tracing::warn!(target: "comet_harness::claude", %path, error = %err, "attachment unreadable; path ref only");
                continue;
            }
        };
        if bytes.len() as u64 > MAX_INLINE_IMAGE_BYTES {
            tracing::debug!(target: "comet_harness::claude", %path, "attachment over inline cap; path ref only");
            continue;
        }
        let Some(media_type) = image_media_type(std::path::Path::new(path), &bytes) else {
            tracing::debug!(target: "comet_harness::claude", %path, "attachment not an inline-supported image; path ref only");
            continue;
        };
        blocks.push(wire::ImageBlock {
            media_type: media_type.to_string(),
            data: base64::engine::general_purpose::STANDARD.encode(&bytes),
        });
    }
    blocks
}

/// Owns the child's stdin; a write failure (EPIPE after the child died) is
/// tolerated and logged, matching the TS harness's swallowed-EPIPE behavior.
async fn stdin_writer(mut stdin: ChildStdin, mut rx: mpsc::UnboundedReceiver<StdinMsg>) {
    while let Some(msg) = rx.recv().await {
        match msg {
            StdinMsg::Line(line) => {
                let write = async {
                    stdin.write_all(line.as_bytes()).await?;
                    stdin.write_all(b"\n").await?;
                    stdin.flush().await
                };
                if let Err(e) = write.await {
                    tracing::debug!(target: "comet_harness::claude", "stdin write failed (tolerated): {e}");
                    return;
                }
            }
            StdinMsg::Close => {
                let _ = stdin.shutdown().await;
                return;
            }
        }
    }
}

struct Session {
    child: Child,
    stdout_lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    stdin_tx: mpsc::UnboundedSender<StdinMsg>,
    event_tx: mpsc::Sender<Result<AgentEvent, HarnessError>>,
    controls: RunControls,
    reasoning: Option<ReasoningLevel>,
    /// Carried only so `SessionStarted` can record what the run was launched
    /// under; the CLI arguments are built from the request before this point.
    runtime_mode: RuntimeMode,
    interrupt_grace: Duration,
    kill_grace: Duration,
    /// Rolling stderr tail for the crash message on an unexpected exit.
    stderr_tail: crate::StderrTail,
}

/// The per-run event loop: one task multiplexing stdout frames, the steering
/// mailbox, the interrupt token, and consumer liveness.
async fn run_session(session: Session) {
    let Session {
        mut child,
        mut stdout_lines,
        stdin_tx,
        event_tx,
        controls,
        reasoning,
        runtime_mode,
        interrupt_grace,
        kill_grace,
        stderr_tail,
    } = session;
    let RunControls {
        request_input,
        request_approval,
        mut steering,
        interrupt,
    } = controls;
    let request_input = Arc::new(request_input);
    let request_approval = Arc::new(request_approval);

    let mut norm = Normalizer::new(runtime_mode);
    let mut steering_open = true;
    let mut interrupted = false;
    let mut interrupt_sent = false;
    let mut any_done = false;
    let mut done_after_interrupt = false;
    let mut escalation: Option<tokio::task::JoinHandle<()>> = None;

    'main: loop {
        tokio::select! {
            line = stdout_lines.next_line() => match line {
                Ok(Some(line)) => {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    let frame = match wire::parse_frame(line) {
                        Ok(frame) => frame,
                        Err(e) => {
                            // Sink 5 — the only producer of `Malformed`. The
                            // raw line stays HERE, in tracing; the event
                            // carries only the fixed sentinel.
                            tracing::warn!(
                                target: "comet_harness::claude",
                                line,
                                "unparseable frame (recorded as a diagnostic): {e}"
                            );
                            let ev = crate::diagnostic(
                                crate::UNPARSEABLE,
                                DiagnosticSeverity::Malformed,
                            );
                            if event_tx.send(Ok(ev)).await.is_err() {
                                break 'main;
                            }
                            continue;
                        }
                    };
                    if let Frame::ControlRequest(req) = frame {
                        if let Some(ev) = handle_control_request(
                            req,
                            &request_input,
                            &request_approval,
                            &stdin_tx,
                        ) && event_tx.send(Ok(ev)).await.is_err()
                        {
                            break 'main;
                        }
                        continue;
                    }
                    for ev in norm.normalize(frame, interrupted) {
                        let is_done = matches!(ev, AgentEvent::Done { .. });
                        if event_tx.send(Ok(ev)).await.is_err() {
                            break 'main; // consumer gone — reap below
                        }
                        if is_done {
                            any_done = true;
                            if interrupted {
                                done_after_interrupt = true;
                                break 'main;
                            }
                        }
                    }
                }
                Ok(None) => break 'main, // stdout EOF: the CLI exited
                Err(e) => {
                    let _ = event_tx.send(Err(HarnessError::Io(e))).await;
                    break 'main;
                }
            },

            steer = steering.recv(), if steering_open && !interrupted => match steer {
                Some(msg) => {
                    let line = wire::user_message_line(&apply_ultrathink(reasoning, &msg.prompt));
                    let _ = stdin_tx.send(StdinMsg::Line(line));
                    // The CLI consumes the queued line at its own step
                    // boundary; rotate the assistant message id so post-steer
                    // output folds into a fresh message.
                    let (prev, next) = norm.rotate_for_steer();
                    let ev = AgentEvent::Steered {
                        assistant_message_id: Some(prev),
                        next_assistant_message_id: Some(next),
                    };
                    if event_tx.send(Ok(ev)).await.is_err() {
                        break 'main;
                    }
                }
                None => {
                    // Mailbox closed: end the input so the run can finish
                    // after the current turn (mirrors claude.ts steeredInput).
                    steering_open = false;
                    let _ = stdin_tx.send(StdinMsg::Close);
                }
            },

            _ = interrupt.cancelled(), if !interrupt_sent => {
                interrupt_sent = true;
                interrupted = true;
                let _ = stdin_tx.send(StdinMsg::Line(wire::interrupt_request_line("int_1")));
                // Escalate if the CLI doesn't wind down within the grace
                // periods: SIGTERM (kills bash trees, runs SessionEnd hooks),
                // then SIGKILL. Aborted once the child is reaped.
                if let Some(pid) = child.id() {
                    escalation = Some(tokio::spawn(async move {
                        tokio::time::sleep(interrupt_grace).await;
                        send_signal(pid, Signal::Term);
                        tokio::time::sleep(kill_grace).await;
                        send_signal(pid, Signal::Kill);
                    }));
                }
            },

            _ = event_tx.closed() => break 'main,
        }
    }

    // Terminal bookkeeping: never end the stream without a Done unless the
    // consumer already hung up.
    if !event_tx.is_closed() {
        if interrupted && !done_after_interrupt {
            let _ = event_tx
                .send(Ok(AgentEvent::Done {
                    status: DoneStatus::Interrupted,
                    result: None,
                    error: None,
                    session_id: norm.session_id.clone(),
                }))
                .await;
        } else if !interrupted && !any_done {
            let status = child.try_wait().ok().flatten();
            let _ = event_tx
                .send(Ok(AgentEvent::Done {
                    status: DoneStatus::Errored,
                    result: None,
                    error: Some(crate::crash_message("claude", status, &stderr_tail)),
                    session_id: norm.session_id.clone(),
                }))
                .await;
        }
    }

    shutdown_child(&mut child, kill_grace).await;
    if let Some(handle) = escalation {
        handle.abort();
    }
}

/// Reap the child: graceful SIGTERM first, SIGKILL after `kill_grace`.
/// (`kill_on_drop` remains the last-resort backstop.)
async fn shutdown_child(child: &mut Child, kill_grace: Duration) {
    if matches!(child.try_wait(), Ok(Some(_))) {
        return;
    }
    if let Some(pid) = child.id() {
        send_signal(pid, Signal::Term);
        if tokio::time::timeout(kill_grace, child.wait()).await.is_ok() {
            return;
        }
    }
    let _ = child.start_kill();
    let _ = child.wait().await;
}

type RequestInputFn = Box<
    dyn Fn(Vec<UserInputQuestion>) -> tokio::sync::oneshot::Receiver<Vec<UserInputAnswer>>
        + Send
        + Sync,
>;

type RequestApprovalFn =
    Box<dyn Fn(ApprovalRequest) -> tokio::sync::oneshot::Receiver<ApprovalDecision> + Send + Sync>;

/// Serve one `can_use_tool` control request. `AskUserQuestion` is intercepted
/// — surface the questions through the engine's input bridge (which owns the
/// `InputRequested`/`InputResolved` lifecycle), wait for the user's answers
/// (in a subtask so the frame loop keeps flowing), and hand them back keyed
/// by question text, as the tool expects. Every other tool round-trips
/// through [`RunControls::request_approval`].
fn handle_control_request(
    req: ControlRequestFrame,
    request_input: &Arc<RequestInputFn>,
    request_approval: &Arc<RequestApprovalFn>,
    stdin_tx: &mpsc::UnboundedSender<StdinMsg>,
) -> Option<AgentEvent> {
    if req.request.subtype != "can_use_tool" {
        // Sink 3: an unclaimed inbound control request. Still counted as a
        // diagnostic — a subtype Comet does not model is still something the
        // user should be able to see was ignored — but no longer left
        // hanging: sdk.d.ts requires a host to reply `{behavior:"cancelled"}`
        // to a `request_user_dialog` kind it does not recognize, and skipping
        // that reply leaves the CLI waiting on an answer that never comes.
        // Comet applies the same reply to every unclaimed subtype (~52
        // others), which the SDK does not specify. The reply shape is
        // derived from the SDK typings rather than a captured live frame.
        //
        // If one of the non-dialog subtypes ever does fire, the safer generic
        // reply is a `control_response` with `subtype: "error"` (an
        // `error` string in place of `response`): "this host does not serve
        // that request" is true of every unclaimed subtype, whereas
        // `{behavior:"cancelled"}` claims a permission answer the caller may
        // not have asked for. Exposure is near zero today — Comet does no
        // initialize handshake and declares no SDK hooks or SDK MCP servers,
        // so the reply that IS specified for the one reachable kind stays
        // until reproducible protocol evidence says otherwise.
        tracing::warn!(
            target: "comet_harness::claude",
            request = %serde_json::json!({
                "request_id": req.request_id,
                "subtype": req.request.subtype,
                "tool_name": req.request.tool_name,
                "input": req.request.input,
            }),
            "unclaimed control_request (recorded as a diagnostic)"
        );
        let event = crate::diagnostic(
            &format!("control_request/{}", req.request.subtype),
            DiagnosticSeverity::Unknown,
        );
        let _ = stdin_tx.send(StdinMsg::Line(control_response_line(
            &req.request_id,
            cancelled_response(),
        )));
        return Some(event);
    }
    if req.request.tool_name == "AskUserQuestion" {
        let request_input = Arc::clone(request_input);
        let stdin_tx = stdin_tx.clone();
        tokio::spawn(async move {
            let request_id = req.request_id;
            let input = req.request.input;
            let questions = parse_questions(&input);
            // The engine's input bridge is the SOLE emitter of
            // `InputRequested`/`InputResolved`: it mints the request id, parks
            // the resolver for `respond_input`, and surfaces both events.
            // Emitting our own copy here (keyed by Claude's control-request
            // id) folded a SECOND input part into the doc whose id no
            // resolver knew — the QuestionPanel answered that unanswerable
            // twin and the run never resumed.
            //
            // A dropped sender (caller went away) degrades to empty answers
            // so the agent is unblocked rather than wedged.
            let answers = (request_input)(questions.clone()).await.unwrap_or_default();
            let updated = updated_input_with_answers(&input, &questions, &answers);
            let line = control_response_line(&request_id, allow_response(updated));
            let _ = stdin_tx.send(StdinMsg::Line(line));
        });
        return None;
    }
    // Every other tool is the user's call. The reply is written from a
    // subtask so the frame loop keeps flowing while the user thinks — a
    // blocked read here would stall the transcript the user is reading to
    // decide.
    // The adapter runs on the machine holding the file, so whether a Write
    // creates or overwrites is a question with an answer here — one `stat` per
    // approval, on a request that is about to wait for a human anyway.
    let approval = approval::approval_request(&req.request, |p| std::path::Path::new(p).exists());
    let request_approval = Arc::clone(request_approval);
    let stdin_tx = stdin_tx.clone();
    tokio::spawn(async move {
        let request_id = req.request_id;
        let decision = (request_approval)(approval).await;
        let response = match decision {
            Ok(ApprovalDecision::Allow) | Ok(ApprovalDecision::AllowForSession) => {
                allow_response(req.request.input)
            }
            Ok(ApprovalDecision::Deny { message }) => deny_response(message),
            // Expired, or a dropped resolver: the user never answered and
            // never will. Not approved.
            Ok(ApprovalDecision::Expired) | Err(_) => {
                deny_response(crate::approval_unanswered_message())
            }
        };
        let _ = stdin_tx.send(StdinMsg::Line(control_response_line(&request_id, response)));
    });
    None
}

/// Parse Claude's `AskUserQuestion` tool input into [`UserInputQuestion`]s
/// (tolerant of `header`/`title`, `question`/`prompt`, string or object
/// options — option descriptions are dropped, the wire type carries labels).
fn parse_questions(input: &Value) -> Vec<UserInputQuestion> {
    let raw = input.get("questions").and_then(Value::as_array);
    raw.map(|a| a.as_slice())
        .unwrap_or_default()
        .iter()
        .map(|q| {
            let field =
                |keys: [&str; 2]| keys.iter().find_map(|k| q.get(*k).and_then(Value::as_str));
            UserInputQuestion {
                id: uuid::Uuid::new_v4().to_string(),
                header: field(["header", "title"]).unwrap_or("Question").into(),
                question: field(["question", "prompt"]).unwrap_or("").into(),
                multi_select: ["multiSelect", "multi_select"]
                    .iter()
                    .find_map(|k| q.get(*k).and_then(Value::as_bool))
                    .unwrap_or(false),
                options: q
                    .get("options")
                    .and_then(Value::as_array)
                    .map(|a| a.as_slice())
                    .unwrap_or_default()
                    .iter()
                    .map(|op| match op {
                        Value::String(s) => s.clone(),
                        other => other
                            .get("label")
                            .or_else(|| other.get("value"))
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .into(),
                    })
                    .collect(),
            }
        })
        .collect()
}

/// Merge the user's answers back into the tool input, keyed by question text
/// (single-select ⇒ a string, multi-select ⇒ an array), as the tool expects.
fn updated_input_with_answers(
    input: &Value,
    questions: &[UserInputQuestion],
    answers: &[UserInputAnswer],
) -> Value {
    let mut updated = match input {
        Value::Object(map) => map.clone(),
        _ => serde_json::Map::new(),
    };
    let mut by_question = serde_json::Map::new();
    for q in questions {
        let labels: Vec<String> = answers
            .iter()
            .find(|a| a.question_id == q.id)
            .map(|a| a.labels.clone())
            .unwrap_or_default();
        let value = if q.multi_select {
            Value::Array(labels.into_iter().map(Value::String).collect())
        } else {
            Value::String(labels.into_iter().next().unwrap_or_default())
        };
        by_question.insert(q.question.clone(), value);
    }
    updated.insert("answers".into(), Value::Object(by_question));
    Value::Object(updated)
}

#[cfg(test)]
mod install_dir_tests {
    use super::*;

    /// The real catalogue's tags, for the same reason as Codex's: the lookup
    /// is tested elsewhere, but the label a user reads comes from these tags.
    ///
    /// `~/.local/bin` is where the capture found the live `claude.exe`
    /// (`captures/2026-08-11-agent-version-install-method.md`), so this is the
    /// entry that decides what this machine's card says.
    #[test]
    fn the_native_installer_dirs_are_tagged_native() {
        let Some(home) = crate::home_dir() else {
            return;
        };
        let dirs = claude_install_dirs();
        for expected_dir in [
            home.join(".local").join("bin"),
            home.join(".claude").join("local"),
        ] {
            let tag = dirs
                .iter()
                .find(|(d, _)| *d == expected_dir)
                .map(|(_, m)| *m)
                .unwrap_or_else(|| panic!("{} must be in the catalogue", expected_dir.display()));
            assert_eq!(tag, InstallMethod::Native, "{}", expected_dir.display());
        }
    }

    /// `/usr/local/bin` stays a searched location with no attribution. The two
    /// halves are a single deliberate statement, and testing only one of them
    /// would let the other flip silently.
    #[test]
    #[cfg(not(windows))]
    fn usr_local_bin_is_listed_but_unattributed() {
        let dirs = claude_install_dirs();
        let tag = dirs
            .iter()
            .find(|(d, _)| *d == std::path::Path::new("/usr/local/bin"))
            .map(|(_, m)| *m)
            .expect("it must still be searched");
        assert_eq!(tag, InstallMethod::Unknown);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_questions_tolerantly() {
        let input = json!({
            "questions": [
                {
                    "header": "Choice",
                    "question": "Pick one",
                    "options": ["A", {"label": "B", "description": "second"}],
                    "multiSelect": false
                },
                { "title": "Alt", "prompt": "Pick many", "multi_select": true }
            ]
        });
        let qs = parse_questions(&input);
        assert_eq!(qs.len(), 2);
        assert_eq!(qs[0].header, "Choice");
        assert_eq!(qs[0].options, vec!["A".to_string(), "B".to_string()]);
        assert!(!qs[0].multi_select);
        assert_eq!(qs[1].header, "Alt");
        assert_eq!(qs[1].question, "Pick many");
        assert!(qs[1].multi_select);
    }

    #[test]
    fn answers_key_by_question_text() {
        let input =
            json!({"questions": [{"header": "H", "question": "Pick one", "options": ["A", "B"]}]});
        let qs = parse_questions(&input);
        let answers = vec![UserInputAnswer {
            question_id: qs[0].id.clone(),
            labels: vec!["B".into()],
        }];
        let updated = updated_input_with_answers(&input, &qs, &answers);
        assert_eq!(updated["answers"]["Pick one"], json!("B"));
        // Original input is preserved alongside the answers.
        assert!(updated["questions"].is_array());
    }
}

#[cfg(test)]
mod control_request_tests {
    use super::*;
    use wire::ControlRequestBody;

    fn frame(subtype: &str, tool_name: &str) -> ControlRequestFrame {
        ControlRequestFrame {
            request_id: "cr-1".into(),
            request: ControlRequestBody {
                subtype: subtype.into(),
                tool_name: tool_name.into(),
                input: serde_json::json!({"x": 1}),
                ..Default::default()
            },
        }
    }

    fn bridge() -> (
        Arc<RequestInputFn>,
        mpsc::UnboundedSender<StdinMsg>,
        mpsc::UnboundedReceiver<StdinMsg>,
    ) {
        let request_input: Arc<RequestInputFn> = Arc::new(Box::new(|_| {
            let (_tx, rx) = tokio::sync::oneshot::channel();
            rx
        }));
        let (stdin_tx, stdin_rx) = mpsc::unbounded_channel();
        (request_input, stdin_tx, stdin_rx)
    }

    /// A decision source the test controls, shaped like the engine's bridge.
    fn approver(answer: Option<ApprovalDecision>) -> Arc<RequestApprovalFn> {
        Arc::new(Box::new(move |_req: ApprovalRequest| {
            let (tx, rx) = tokio::sync::oneshot::channel();
            if let Some(answer) = answer.clone() {
                let _ = tx.send(answer);
            }
            // `None` drops the sender: the receiver resolves to Err, which must
            // read as NOT approved.
            rx
        }))
    }

    async fn recv_line(rx: &mut mpsc::UnboundedReceiver<StdinMsg>) -> String {
        match tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("a control response must be sent")
        {
            Some(StdinMsg::Line(line)) => line,
            _ => panic!("expected a control-response line"),
        }
    }

    /// Sink 3, now answered. Still counted — a subtype Comet does not understand
    /// is still a thing the user should be able to see was ignored.
    #[tokio::test]
    async fn an_unclaimed_control_request_is_cancelled_and_still_counted() {
        let (request_input, stdin_tx, mut stdin_rx) = bridge();
        let ev = handle_control_request(
            frame("request_user_dialog", ""),
            &request_input,
            &approver(Some(ApprovalDecision::Allow)),
            &stdin_tx,
        );
        assert_eq!(
            ev,
            Some(crate::diagnostic(
                "control_request/request_user_dialog",
                DiagnosticSeverity::Unknown,
            ))
        );
        let sent: serde_json::Value =
            serde_json::from_str(&recv_line(&mut stdin_rx).await).unwrap();
        assert_eq!(sent["response"]["response"]["behavior"], "cancelled");
        assert_eq!(sent["response"]["request_id"], "cr-1"); // what `frame()` mints
    }

    #[tokio::test]
    async fn an_unclaimed_subtype_never_reaches_the_approval_bridge() {
        // It is not a permission question; raising a card for it would ask the
        // user about something Comet cannot describe.
        let (request_input, stdin_tx, _rx) = bridge();
        let asked = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = asked.clone();
        let approver: Arc<RequestApprovalFn> = Arc::new(Box::new(move |_| {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
            let (_tx, rx) = tokio::sync::oneshot::channel();
            rx
        }));
        handle_control_request(
            frame("request_user_dialog", ""),
            &request_input,
            &approver,
            &stdin_tx,
        );
        // `#[tokio::test]` is a current-thread runtime: it never polls a
        // `tokio::spawn`ed task while this body runs synchronously. If the
        // guard above were deleted, the approval route would spawn a task
        // that calls the approver — but without a yield here, that task
        // would never be polled and `asked` would read false regardless of
        // whether the bridge was reached. Yielding once gives any spawned
        // task its first poll, which is after the approver closure runs.
        tokio::task::yield_now().await;
        assert!(!asked.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn an_allowed_tool_is_answered_allow_with_its_input_intact() {
        let (request_input, stdin_tx, mut stdin_rx) = bridge();
        let ev = handle_control_request(
            frame("can_use_tool", "Bash"),
            &request_input,
            &approver(Some(ApprovalDecision::Allow)),
            &stdin_tx,
        );
        assert!(ev.is_none(), "an approval is not a diagnostic");
        let line = recv_line(&mut stdin_rx).await;
        let sent: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(sent["response"]["response"]["behavior"], "allow");
        assert_eq!(
            sent["response"]["response"]["updatedInput"],
            serde_json::json!({"x": 1}),
            "the frame's input must ride back intact, unmodified by the approval round trip"
        );
        assert!(
            sent["response"]["response"]
                .get("updatedPermissions")
                .is_none(),
            "session grants are engine-owned and must not become provider permission updates"
        );
    }

    #[tokio::test]
    async fn a_denied_tool_is_answered_deny_with_the_users_note() {
        let (request_input, stdin_tx, mut stdin_rx) = bridge();
        handle_control_request(
            frame("can_use_tool", "Bash"),
            &request_input,
            &approver(Some(ApprovalDecision::Deny {
                message: "not that path".into(),
            })),
            &stdin_tx,
        );
        let sent: serde_json::Value =
            serde_json::from_str(&recv_line(&mut stdin_rx).await).unwrap();
        assert_eq!(sent["response"]["response"]["behavior"], "deny");
        assert_eq!(sent["response"]["response"]["message"], "not that path");
    }

    #[tokio::test]
    async fn allow_for_session_is_answered_allow_and_sends_no_permission_update() {
        // The engine remembers the session grant (Task 1). The CLI is told plain
        // "allow" without delegating persistence to provider configuration.
        let (request_input, stdin_tx, mut stdin_rx) = bridge();
        handle_control_request(
            frame("can_use_tool", "Write"),
            &request_input,
            &approver(Some(ApprovalDecision::AllowForSession)),
            &stdin_tx,
        );
        let sent: serde_json::Value =
            serde_json::from_str(&recv_line(&mut stdin_rx).await).unwrap();
        assert_eq!(sent["response"]["response"]["behavior"], "allow");
        assert!(
            sent["response"]["response"]
                .get("updatedPermissions")
                .is_none()
        );
    }

    #[tokio::test]
    async fn an_unanswerable_approval_denies_rather_than_allowing() {
        // The decision source went away (run torn down, client gone). Never
        // default to allow: that is how a permission defect ships looking correct.
        let (request_input, stdin_tx, mut stdin_rx) = bridge();
        handle_control_request(
            frame("can_use_tool", "Bash"),
            &request_input,
            &approver(None),
            &stdin_tx,
        );
        let sent: serde_json::Value =
            serde_json::from_str(&recv_line(&mut stdin_rx).await).unwrap();
        assert_eq!(sent["response"]["response"]["behavior"], "deny");
    }

    #[tokio::test]
    async fn expired_denies_too() {
        let (request_input, stdin_tx, mut stdin_rx) = bridge();
        handle_control_request(
            frame("can_use_tool", "Bash"),
            &request_input,
            &approver(Some(ApprovalDecision::Expired)),
            &stdin_tx,
        );
        let sent: serde_json::Value =
            serde_json::from_str(&recv_line(&mut stdin_rx).await).unwrap();
        assert_eq!(sent["response"]["response"]["behavior"], "deny");
    }

    #[tokio::test]
    async fn ask_user_question_still_goes_to_the_input_bridge_not_the_approval_bridge() {
        // Regression guard: AskUserQuestion is a different contract and must not
        // start raising approval cards.
        let (request_input, stdin_tx, _rx) = bridge();
        let ev = handle_control_request(
            frame("can_use_tool", "AskUserQuestion"),
            &request_input,
            &approver(Some(ApprovalDecision::Deny {
                message: "x".into(),
            })),
            &stdin_tx,
        );
        assert!(ev.is_none());
        // The input bridge answered; nothing was denied.
    }
}

#[cfg(test)]
mod command_tests {
    use super::*;
    use comet_proto::{RuntimeMode, SandboxLevel};

    /// The argument list `build_command` would spawn, for a run in `mode`.
    fn args_for(mode: RuntimeMode) -> Vec<String> {
        let request = RunRequest {
            prompt: "hi".into(),
            ..RunRequest::for_session(mode)
        };
        args_of(&request)
    }

    fn args_of(request: &RunRequest) -> Vec<String> {
        build_run_command(Path::new("claude"), request)
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    /// The value following `flag`, or `None` if the flag is absent.
    fn value_of<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
        args.iter()
            .position(|a| a == flag)
            .and_then(|at| args.get(at + 1))
            .map(String::as_str)
    }

    #[test]
    fn permission_mode_follows_the_runtime_mode() {
        for (mode, expected) in [
            (RuntimeMode::ApprovalRequired, "default"),
            (RuntimeMode::AutoAcceptEdits, "acceptEdits"),
            (RuntimeMode::Auto, "auto"),
            (RuntimeMode::FullAccess, "bypassPermissions"),
        ] {
            let args = args_for(mode);
            assert_eq!(
                value_of(&args, "--permission-mode"),
                Some(expected),
                "{mode:?} produced {args:?}"
            );
        }
    }

    /// The bypass flag is the one argument that must not spread: it is what
    /// removes every guardrail, and only the mode that names that is allowed
    /// to carry it.
    #[test]
    fn only_full_access_skips_permissions() {
        for mode in [
            RuntimeMode::ApprovalRequired,
            RuntimeMode::AutoAcceptEdits,
            RuntimeMode::Auto,
        ] {
            let args = args_for(mode);
            assert!(
                !args.iter().any(|a| a == "--dangerously-skip-permissions"),
                "{mode:?} must not skip permissions: {args:?}"
            );
        }
        assert!(
            args_for(RuntimeMode::FullAccess)
                .iter()
                .any(|a| a == "--dangerously-skip-permissions")
        );
    }

    /// Chat titling pairs a never-ask mode with a read-only sandbox. Claude
    /// reads the mode and not the sandbox, so that request must still produce
    /// the bypass pair — a run that stopped to ask would hang, since titling
    /// has no surface on which an answer could be given.
    #[test]
    fn a_read_only_sandbox_does_not_change_the_permission_mode() {
        let request = RunRequest {
            prompt: "name this chat".into(),
            sandbox: SandboxLevel::ReadOnly,
            ..RunRequest::for_session(RuntimeMode::FullAccess)
        };
        let args = args_of(&request);
        assert_eq!(
            value_of(&args, "--permission-mode"),
            Some("bypassPermissions")
        );
        assert!(args.iter().any(|a| a == "--dangerously-skip-permissions"));
    }

    /// Every mode the adapter maps is a mode it declares. A mode declared but
    /// unmapped would be offered to a user and then run as something else.
    #[test]
    fn every_declared_mode_is_a_mode_the_command_maps() {
        let declared = ClaudeHarness::capabilities().runtime_modes;
        assert_eq!(
            declared,
            vec![
                RuntimeMode::ApprovalRequired,
                RuntimeMode::AutoAcceptEdits,
                RuntimeMode::Auto,
                RuntimeMode::FullAccess,
            ]
        );
        for mode in declared {
            assert!(
                value_of(&args_for(mode), "--permission-mode").is_some(),
                "{mode:?} is declared but produces no permission mode"
            );
        }
    }

    /// The deny arm puts the note on the wire, so the composer may promise it.
    #[test]
    fn claude_carries_a_deny_note() {
        assert!(ClaudeHarness::capabilities().carries_deny_note);
    }
}
