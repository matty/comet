//! The provider-neutral capture recorder.
//!
//! `record()` is the entry point: it looks `(config.provider,
//! config.scenario_name)` up in [`scenarios::SCENARIOS`] and dispatches
//! straight to the [`Session`]/[`CaptureProvider`] machinery. There is no
//! second path any more — every scenario, discovery and run alike, is a
//! table row.

mod provider;
mod providers;
mod scenarios;
mod session;

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use comet_proto::RunRequest;
use provider::CaptureProvider;
use providers::claude::ClaudeProvider;
use providers::codex::CodexProvider;
pub use scenarios::{Requirements, SCENARIOS, ScenarioSpec, scenario};
use scenarios::{ScenarioBody, ScenarioInput, ScenarioLaunch};
use session::{FenceOutcome, Session};

use crate::capture::types::{CaptureConfig, PartialFailureClass, Provider, RawCapture};
use crate::launch::LaunchDescriptor;

/// Record one explicitly selected provider scenario into ignored raw
/// storage. `config.provider`/`config.scenario_name` must name a row in
/// [`SCENARIOS`] — the binary only ever reaches this after `scenario()`
/// resolved the same pair, so a caller that violates it is a programming
/// error, not a user-facing one.
pub async fn record(config: CaptureConfig) -> anyhow::Result<RawCapture> {
    let spec = scenario(config.provider, config.scenario_name).unwrap_or_else(|| {
        panic!(
            "{}/{} must be registered in SCENARIOS",
            config.provider, config.scenario_name
        )
    });
    let input = ScenarioInput {
        cwd: config.cwd.clone(),
        resume_id: config.resume_id.clone(),
        attachment: config.attachment.clone(),
        codex_home: config.codex_home.clone(),
        approval_target: config.approval_target.clone(),
    };
    match &spec.body {
        ScenarioBody::Claude(body) => record_claude(&config, spec, input, *body).await,
        ScenarioBody::Codex(body) => record_codex(&config, spec, input, *body).await,
    }
}

async fn record_claude(
    config: &CaptureConfig,
    spec: &ScenarioSpec,
    input: ScenarioInput,
    body: ScenarioBodyFn<ClaudeProvider>,
) -> anyhow::Result<RawCapture> {
    let executable = session::resolve_executable(
        Provider::Claude,
        config
            .executable
            .clone()
            .or_else(crate::claude::resolve_claude_executable),
    )?;
    let (launch, request) =
        derive_launch(&spec.launch, &input, &executable, crate::claude::run_launch)?;
    // Claude has no pre-spawn fence: nothing here validates an environment
    // before spawn the way `codex_fence` below does for Codex. Claude's
    // `approval` DOES grant a filesystem write — a Bash command or a Write
    // into cwd — so it is protected the same way Task 6 protects Codex's
    // grant-time rechecks: `record/scenarios/claude.rs`'s `approval` body
    // recomputes a marker-shape check (`claude_marker_grant`) immediately
    // before answering each request, and DECLINES — without aborting the
    // capture — anything that does not match. See that function's own doc
    // comment.
    record_generic(
        ClaudeProvider,
        config,
        launch,
        input,
        body,
        FenceOutcome::none(),
        request,
    )
    .await
}

async fn record_codex(
    config: &CaptureConfig,
    spec: &ScenarioSpec,
    input: ScenarioInput,
    body: ScenarioBodyFn<CodexProvider>,
) -> anyhow::Result<RawCapture> {
    let executable = session::resolve_executable(
        Provider::Codex,
        config
            .executable
            .clone()
            .or_else(crate::codex::resolve_codex_executable),
    )?;
    let (launch, request) =
        derive_launch(&spec.launch, &input, &executable, crate::codex::run_launch)?;
    let fence = codex_fence(spec, config, &launch)?;
    record_generic(
        CodexProvider::new(),
        config,
        launch,
        input,
        body,
        fence,
        request,
    )
    .await
}

/// The ONLY call site for a row's `ScenarioLaunch::Run` builder. Builds the
/// `RunRequest` once, derives the launch from it through the provider's own
/// `run_launch`, and returns the same `RunRequest` so the caller can hand it
/// to `Session::request` — the recorder never calls a run builder a second
/// time to build a scenario's wire line. A discovery row has no `RunRequest`
/// at all, so its half of the match returns `None`. See `ScenarioLaunch`'s
/// own doc comment (`scenarios.rs`) for the hazard this closes.
fn derive_launch(
    launch: &ScenarioLaunch,
    input: &ScenarioInput,
    executable: &Path,
    run_launch: fn(&Path, &RunRequest) -> LaunchDescriptor,
) -> anyhow::Result<(LaunchDescriptor, Option<RunRequest>)> {
    match launch {
        ScenarioLaunch::Discovery(build_launch) => Ok((build_launch(input, executable)?, None)),
        ScenarioLaunch::Run(build_request) => {
            let request = build_request(input)?;
            let launch = run_launch(executable, &request);
            Ok((launch, Some(request)))
        }
    }
}

/// Builds the pre-spawn fence for the two Codex scenarios that need one.
/// Every other scenario (every Claude row, and every Codex row but
/// `approval`/`approval-on-request`) gets [`FenceOutcome::none`].
///
/// This is the pre-spawn fence decision #6 in the stage plan requires stay —
/// `crate::capture::safety`'s checks ran inside `recording.rs`'s
/// `RecordingSession::start` before the neutral-recorder stage's Task 7
/// deleted that file; this is their new home, run before `Session::start`
/// is even called.
fn codex_fence(
    spec: &ScenarioSpec,
    config: &CaptureConfig,
    launch: &LaunchDescriptor,
) -> anyhow::Result<FenceOutcome> {
    let launch_cwd = || -> anyhow::Result<PathBuf> {
        launch.cwd.clone().ok_or_else(|| {
            anyhow::anyhow!("Codex approval capture requires a resolved working directory.")
        })
    };

    if spec.requirements.needs_approval_target {
        // `approval-on-request`: a non-repository cwd and an empty,
        // identity-stable, isolated approval target. Checked once here
        // (matching `record_codex`'s own call), and RECHECKED right before
        // spawn (`FenceOutcome::recheck`, run inside `Session::start`) — the
        // window between this call (which can involve real filesystem I/O)
        // and the eventual spawn (after directory creation and a
        // `--version` probe) is exactly the race
        // `crate::capture::safety::validate_on_request_preflight`'s
        // doc comment and `docs/testing/provider-captures.md` describe.
        let cwd = launch_cwd()?;
        let target = config.approval_target.clone();
        let identity =
            crate::capture::safety::validate_on_request_preflight(&cwd, target.as_deref())?;
        let recheck_cwd = cwd.clone();
        let recheck_target = target.clone();
        let recheck_identity = identity.clone();
        return Ok(FenceOutcome {
            approval_target: target,
            approval_target_identity: identity,
            approval_cwd_identity: None,
            trusted_powershell: None,
            recheck: Some(Box::new(move || {
                let spawn_identity = crate::capture::safety::validate_on_request_preflight(
                    &recheck_cwd,
                    recheck_target.as_deref(),
                )?;
                if spawn_identity != recheck_identity {
                    anyhow::bail!(
                        "Codex on-request approval target changed identity before provider spawn."
                    );
                }
                Ok(())
            })),
        });
    }

    if spec.runtime_mode == Some(comet_proto::RuntimeMode::ApprovalRequired) {
        // `approval`: a trusted, protected-root PowerShell (Windows only —
        // fails closed elsewhere, see `resolve_trusted_powershell`'s own
        // doc comment) and a cwd whose identity `record::scenarios::codex::approval`
        // rechecks at every grant, via `approval_cwd_identity` below.
        let cwd = launch_cwd()?;
        let trusted = crate::capture::safety::resolve_trusted_powershell(&cwd, &config.raw_root)?;
        let identity = crate::capture::safety::validate_ordinary_approval_cwd(&cwd, None, true)?;
        return Ok(FenceOutcome {
            approval_target: None,
            approval_target_identity: None,
            approval_cwd_identity: Some(identity),
            trusted_powershell: Some(trusted),
            recheck: None,
        });
    }

    Ok(FenceOutcome::none())
}

/// A scenario body: given a freshly spawned session, drive it (handshaking
/// first if this scenario needs one — see `record_generic`'s doc comment)
/// and report whether the scenario completed.
type ScenarioBodyFn<P> = for<'a> fn(
    &'a mut Session<P>,
    &'a ScenarioInput,
) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + 'a>>;

/// The provider-neutral orchestration shared by every scenario: spawn,
/// drive the scenario body, finish — all under one shared deadline. A
/// failure or timeout during drive never reaches [`Session::finish`]; this
/// function still owns `&mut session` at that point and classifies
/// explicitly (`DriverError` for a driving failure, `Timeout` for the
/// deadline firing) — the `drive_completed` distinction `recording.rs` made
/// with a boolean, carried here by which branch of this match runs.
///
/// Deliberately does NOT call `P::handshake` — see `CaptureProvider::handshake`'s
/// own doc comment ("the scenario body calls this, not the recorder").
/// Whether a scenario handshakes at all is a scenario decision: every
/// discovery body and every Codex run body opens with
/// `P::handshake(session, input).await?` itself; a Claude run body calls
/// nothing, because a real Claude run sends no handshake and this function
/// calling one unconditionally would put a line on the tape the product
/// never sends.
///
/// `deadline` is computed once, right after spawn, and passed into both the
/// outer `timeout_at` wrapping the body *and* into [`Session::finish`], so
/// the exit wait shares the same clock as driving instead of getting a
/// fresh, unrelated budget — `recording.rs`'s original `finish` wrapped
/// drive *and* the exit wait in one `timeout(self.timeout, …)`, and
/// splitting that into two functions must not silently narrow what the
/// configured timeout covers.
async fn record_generic<P: CaptureProvider>(
    provider: P,
    config: &CaptureConfig,
    launch: LaunchDescriptor,
    input: ScenarioInput,
    body: ScenarioBodyFn<P>,
    fence: FenceOutcome,
    request: Option<RunRequest>,
) -> anyhow::Result<RawCapture> {
    let mut session = Session::start(provider, config, launch, fence, request).await?;
    let deadline = tokio::time::Instant::now() + session.timeout;
    let outcome = tokio::time::timeout_at(deadline, body(&mut session, &input)).await;
    match outcome {
        Ok(Ok(())) => session.finish(deadline).await,
        Ok(Err(err)) => {
            session.terminate_and_reap().await;
            session
                .persist_partial_after_failure(PartialFailureClass::DriverError)
                .await;
            Err(err)
        }
        Err(_) => {
            session.terminate_and_reap().await;
            session
                .persist_partial_after_failure(PartialFailureClass::Timeout)
                .await;
            anyhow::bail!(
                "Capture timed out after {} seconds. The provider was stopped; retry with --timeout-seconds up to 300.",
                session.timeout.as_secs_f64()
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use serde_json::json;

    use super::*;
    use crate::capture::test_support::{absolute_program, channel_payloads, config, fixture_path};
    use crate::capture::types::{Channel, CommandSnapshot};
    use crate::launch::StdioMode;

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

    /// Break caught: selecting command discovery's non-bare launch for model discovery,
    /// dropping a configured pipe, or allocating sequence numbers outside observer order.
    #[tokio::test]
    async fn recorder_claude_model_discovery_keeps_all_channels_and_monotonic_sequence() {
        let raw = tempfile::tempdir().unwrap();
        let capture = record(config(
            "model-discovery",
            fixture_path("fake-claude"),
            "claude",
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
        let persisted: RawCapture =
            serde_json::from_slice(&std::fs::read(capture.directory.join("capture.json")).unwrap())
                .unwrap();
        assert_eq!(persisted.events, capture.events);
    }

    /// Break caught: command discovery accidentally inherits model discovery's `--bare`.
    #[tokio::test]
    async fn recorder_claude_command_discovery_uses_non_bare_initialize() {
        let raw = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let mut cfg = config(
            "command-discovery",
            fixture_path("fake-claude"),
            "claude",
            raw.path(),
        );
        cfg.cwd = Some(cwd.path().into());
        let capture = record(cfg).await.unwrap();

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
            "model-discovery-neutral-cwd",
            fixture_path("fake-claude"),
            "claude",
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
        assert_eq!(persisted["scenario"], "model-discovery-neutral-cwd");
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
        let mut cfg = config(
            "model-discovery",
            fixture_path("fake-codex"),
            "codex",
            raw.path(),
        );
        cfg.codex_home = Some(home.path().into());
        let capture = record(cfg).await.unwrap();

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

    /// Break caught: the hard-timeout branch failing to persist a partial capture under
    /// `PartialFailureClass::Timeout`, or the exit wait's own budget silently outliving the
    /// configured timeout. Does NOT catch a dropped `terminate_and_reap()` call on this branch —
    /// `Session`'s own `Drop` impl (`record/session.rs`) kills and reaps the child in the
    /// background regardless of whether that call ran, and nothing here observes the live process
    /// the way `record/session.rs`'s `wait_error_retains_child_for_cleanup_and_quarantine` does.
    /// Drives a REAL timeout through `record()` — not a hand-copied reproduction of its own
    /// timeout-handling code — using a fixture that receives the discovery initialize request and
    /// genuinely never replies.
    #[tokio::test]
    async fn recorder_timeout_kills_and_reaps_the_child() {
        let raw = tempfile::tempdir().unwrap();
        let mut cfg = config(
            "model-discovery",
            fixture_path("fake-claude-discovery-stall"),
            "claude",
            raw.path(),
        );
        cfg.timeout = Duration::from_millis(100);

        let error = record(cfg).await.unwrap_err();

        assert!(error.to_string().contains("timed out"), "{error}");
        let directory = only_raw_subdirectory(raw.path());
        let partial: serde_json::Value = serde_json::from_slice(
            &std::fs::read(directory.join("partial-capture.json"))
                .expect("timeout partial evidence"),
        )
        .unwrap();
        assert_eq!(partial["failure_class"], "timeout");
    }

    /// Break caught: `record`'s `Ok(Err(err))` branch (a driving failure with the child still
    /// alive) replaces the driving error with something else, or classifies the partial capture
    /// as anything but `DriverError`. With the timeout test above, this closes out coverage of all
    /// three `PartialFailureClass` variants through production code: `ProcessError` is
    /// `Session`-level and covered by `wait_error_retains_child_for_cleanup_and_quarantine`.
    #[tokio::test]
    async fn record_reports_the_driving_error_and_classifies_it_as_driver_error() {
        let raw = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let mut cfg = config(
            "command-discovery",
            fixture_path("fake-claude-discovery-stall"),
            "claude",
            raw.path(),
        );
        cfg.cwd = Some(cwd.path().into());
        let error = record(cfg).await.unwrap_err();

        assert!(
            error.to_string().contains("stopped before the expected"),
            "{error}"
        );
        let directory = only_raw_subdirectory(raw.path());
        let partial: serde_json::Value = serde_json::from_slice(
            &std::fs::read(directory.join("partial-capture.json"))
                .expect("driver-error partial evidence"),
        )
        .unwrap();
        assert_eq!(partial["failure_class"], "driver_error");
    }

    /// Break caught: `record_generic` calls `P::handshake` unconditionally —
    /// putting a `control_request`/`initialize` line on the tape before a
    /// real Claude run's first line, which the product itself never sends
    /// (`crates/harness/src/claude/mod.rs`'s run driver). The scenario-level
    /// tests in `record/scenarios/claude.rs` construct a `Session` directly
    /// and call the scenario body themselves, so none of them can catch a
    /// regression in `record_generic` — only driving through the real public
    /// entry point, with a scenario the SCENARIOS table now actually wires
    /// up, can.
    #[tokio::test]
    async fn record_claude_run_sends_no_handshake_before_the_user_turn() {
        let raw = tempfile::tempdir().unwrap();
        let capture = record(config(
            "fresh-text",
            fixture_path("fake-claude"),
            "claude",
            raw.path(),
        ))
        .await
        .unwrap();

        let stdin = channel_payloads(&capture, Channel::Stdin);
        assert_eq!(
            stdin.len(),
            1,
            "a Claude run sends exactly one initial line: {stdin:?}"
        );
        assert!(
            !stdin[0].contains("control_request") && !stdin[0].contains("initialize"),
            "no control_request/initialize line may precede the user turn: {stdin:?}"
        );
        let first: serde_json::Value = serde_json::from_str(stdin[0]).unwrap();
        assert_eq!(
            first["type"], "user",
            "the first (and only) line must be the user turn: {stdin:?}"
        );
        assert_eq!(capture.exit_code, Some(0));
    }

    /// The Codex counterpart: a real Codex run DOES handshake first — driven
    /// the same way, through `record()` itself, to prove the `fresh-text`
    /// row's `body` genuinely calls `CodexProvider::handshake` before
    /// anything scenario-specific.
    ///
    /// Break caught, verified by falsification: removing the `CodexProvider::handshake(...)`
    /// call from `record/scenarios/codex.rs`'s `fresh_text` body fails loudly — `fake-codex`
    /// expects `initialize` first and the whole capture errors: "Codex stopped before the
    /// expected JSON-RPC reply." Restored after confirming.
    #[tokio::test]
    async fn record_codex_run_sends_the_initialize_handshake_first() {
        let raw = tempfile::tempdir().unwrap();
        let capture = record(config(
            "fresh-text",
            fixture_path("fake-codex"),
            "codex",
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
            methods[..2],
            ["initialize".to_owned(), "initialized".to_owned()],
            "a Codex run must handshake before anything else: {methods:?}"
        );
        assert_eq!(capture.exit_code, Some(0));
    }

    /// The recheck closure `codex_fence` builds for `approval-on-request`, proven directly:
    /// building the fence once, then mutating the approval target's emptiness before calling the
    /// returned `recheck`, must make `recheck` itself fail — independent of whether
    /// `Session::start` remembers to call it at all (that wiring is
    /// `record/session.rs`'s own `start_runs_the_fence_recheck_after_directory_creation_and_before_spawn`/
    /// `start_aborts_before_spawn_when_the_fence_recheck_fails`).
    ///
    /// Break caught: `codex_fence`'s `recheck` closure stops re-running
    /// `validate_on_request_preflight` against live filesystem state — e.g. it captures and
    /// replays the first check's `Ok` result instead of calling the function again.
    #[tokio::test]
    async fn codex_fence_recheck_catches_a_target_that_stopped_being_empty() {
        let raw = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        // Outside the system temp tree, unlike a plain `tempfile::tempdir()` — the fence itself
        // rejects an in-temp target, and this test needs the fence to accept it going in so the
        // *recheck*, not the initial check, is what's being proven.
        let target = tempfile::Builder::new()
            .prefix("comet-fence-recheck-target-")
            .tempdir_in(std::env::current_dir().unwrap())
            .unwrap();
        let mut cfg = config(
            "approval-on-request",
            fixture_path("fake-codex"),
            "codex",
            raw.path(),
        );
        cfg.cwd = Some(cwd.path().into());
        cfg.approval_target = Some(target.path().into());
        let spec = scenario("codex", "approval-on-request").unwrap();
        let input = ScenarioInput {
            cwd: cfg.cwd.clone(),
            approval_target: cfg.approval_target.clone(),
            ..ScenarioInput::default()
        };
        let executable = fixture_path("fake-codex");
        let (launch, _request) =
            derive_launch(&spec.launch, &input, &executable, crate::codex::run_launch).unwrap();

        let fence = codex_fence(spec, &cfg, &launch).unwrap();
        let recheck = fence
            .recheck
            .expect("approval-on-request must build a recheck closure");

        // The race the recheck exists to close: the target stops being empty after the fence was
        // built but before spawn.
        std::fs::write(target.path().join("appeared-after-fence.txt"), "hostile").unwrap();

        let error = recheck().unwrap_err();
        assert!(error.to_string().contains("empty"), "{error}");
    }

    /// The grant-time rechecks in `record/scenarios/codex.rs`'s `approval`/`approval_on_request`
    /// (`answer_every_approval`'s `recheck` closures) read `session.fence.approval_cwd_identity`/
    /// `approval_target_identity` as `Option<&DirectoryIdentity>`, and `None` is not a failure
    /// there — `validate_ordinary_approval_cwd`/`require_empty_approval_target` both treat `None`
    /// as "no expected identity to compare against" and silently degrade to an emptiness/marker
    /// check with no identity comparison at all (see `.agents/rules/optional-wire-fields.md`).
    /// Every existing test for those scenarios hand-builds `FenceOutcome{ ..: Some(...) }` and
    /// never calls `codex_fence` at all, so nothing before this test proved `codex_fence` itself
    /// populates the field the grant-time recheck depends on.
    ///
    /// Break caught: `codex_fence` starts returning `None` for `approval_target_identity` on the
    /// `approval-on-request` row (e.g. the `identity` binding stops being threaded into the
    /// `FenceOutcome` literal) — the pre-spawn fence would still run and still succeed, `--help`
    /// and dispatch would look untouched, and only the identity half of the grant-time protection
    /// would be silently gone.
    #[tokio::test]
    async fn codex_fence_populates_the_approval_target_identity_for_on_request() {
        let raw = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let target = tempfile::Builder::new()
            .prefix("comet-fence-identity-target-")
            .tempdir_in(std::env::current_dir().unwrap())
            .unwrap();
        let mut cfg = config(
            "approval-on-request",
            fixture_path("fake-codex"),
            "codex",
            raw.path(),
        );
        cfg.cwd = Some(cwd.path().into());
        cfg.approval_target = Some(target.path().into());
        let spec = scenario("codex", "approval-on-request").unwrap();
        let input = ScenarioInput {
            cwd: cfg.cwd.clone(),
            approval_target: cfg.approval_target.clone(),
            ..ScenarioInput::default()
        };
        let executable = fixture_path("fake-codex");
        let (launch, _request) =
            derive_launch(&spec.launch, &input, &executable, crate::codex::run_launch).unwrap();

        let fence = codex_fence(spec, &cfg, &launch).unwrap();

        assert!(
            fence.approval_target_identity.is_some(),
            "approval-on-request's fence must record an expected target identity for the \
             grant-time recheck to compare against"
        );
    }

    /// Same concern as `codex_fence_populates_the_approval_target_identity_for_on_request`, for
    /// `approval`'s `approval_cwd_identity`. Windows-only, matching every other test in this crate
    /// that goes through `resolve_trusted_powershell` — it fails closed on every other platform
    /// (see that function's own doc comment), so this would never reach the assertion elsewhere.
    #[cfg(windows)]
    #[tokio::test]
    async fn codex_fence_populates_the_approval_cwd_identity_for_approval() {
        let raw = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let mut cfg = config("approval", fixture_path("fake-codex"), "codex", raw.path());
        cfg.cwd = Some(cwd.path().into());
        let spec = scenario("codex", "approval").unwrap();
        let input = ScenarioInput {
            cwd: cfg.cwd.clone(),
            ..ScenarioInput::default()
        };
        let executable = fixture_path("fake-codex");
        let (launch, _request) =
            derive_launch(&spec.launch, &input, &executable, crate::codex::run_launch).unwrap();

        let fence = codex_fence(spec, &cfg, &launch).unwrap();

        assert!(
            fence.approval_cwd_identity.is_some(),
            "approval's fence must record an expected cwd identity for the grant-time recheck \
             to compare against"
        );
    }

    /// The hazard `ScenarioLaunch`/`derive_launch` exist to close: before this
    /// task, a run scenario's launch and its wire line both came from calling
    /// the same `*_request` builder, independently, from two different
    /// places — the `*_launch` wrapper (to build the argv
    /// `record_claude`/`record_codex` spawns) and the scenario body (to build
    /// the wire line the body sends). Nothing enforced the two calls returned
    /// the same value; only every real builder happening to be a pure
    /// function of `input` did.
    ///
    /// `counting_request` is deliberately NOT pure (a call counter folded
    /// into both `model` and `prompt`), so it turns that assumption into an
    /// observable fact: `record_claude` reaches it through exactly one path
    /// now — `derive_launch`, called once, whose `RunRequest` is used for
    /// BOTH the launch and (via `Session::request`) `hazard_body`'s wire
    /// line — so the recorded argv (`--model call-N`) and the recorded wire
    /// line (`call-N`) must name the same call. Before this task, with a
    /// `*_launch` wrapper calling the builder once and the body calling it a
    /// second, independent time, this same assertion failed (`--model
    /// call-0` vs `call-1`) — see the task report for the quoted failure.
    #[tokio::test]
    async fn scenario_launch_and_body_must_share_one_request_builder_call() {
        use std::sync::atomic::{AtomicU64, Ordering};

        static CALLS: AtomicU64 = AtomicU64::new(0);
        fn counting_request(input: &ScenarioInput) -> anyhow::Result<comet_proto::RunRequest> {
            let call = CALLS.fetch_add(1, Ordering::SeqCst);
            let cwd = input.cwd.clone().unwrap_or_else(std::env::temp_dir);
            Ok(comet_proto::RunRequest {
                prompt: format!("scenario:capture-fresh call-{call}"),
                model: Some(format!("call-{call}")),
                cwd: cwd.display().to_string(),
                ..comet_proto::RunRequest::for_session(comet_proto::RuntimeMode::AutoAcceptEdits)
            })
        }
        fn hazard_body<'a>(
            session: &'a mut Session<ClaudeProvider>,
            _input: &'a ScenarioInput,
        ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + 'a>> {
            Box::pin(async move {
                let request = session
                    .request
                    .clone()
                    .expect("a Run scenario always carries a request");
                let line = crate::claude::wire::user_message_line_with_images(&request.prompt, &[]);
                session.send(&line).await?;
                session.wait_for_turn_end().await
            })
        }

        CALLS.store(0, Ordering::SeqCst);
        let raw = tempfile::tempdir().unwrap();
        let executable = fixture_path("fake-claude");
        let spec = ScenarioSpec {
            name: "test-only-request-builder-hazard",
            purpose: "test-only: prove the recorder cannot call a Run builder twice",
            provider: Provider::Claude,
            runtime_mode: Some(comet_proto::RuntimeMode::AutoAcceptEdits),
            requirements: Requirements::run(),
            launch: ScenarioLaunch::Run(counting_request),
            body: ScenarioBody::Claude(hazard_body),
        };
        let cfg = config(
            "test-only-request-builder-hazard",
            executable,
            "claude",
            raw.path(),
        );
        let input = ScenarioInput::default();

        let capture = record_claude(&cfg, &spec, input, hazard_body)
            .await
            .unwrap();

        assert_eq!(
            CALLS.load(Ordering::SeqCst),
            1,
            "the recorder must call a Run scenario's request builder exactly once per recording"
        );
        let model_index = capture
            .command
            .args
            .iter()
            .position(|arg| arg == "--model")
            .and_then(|position| capture.command.args.get(position + 1))
            .and_then(|value| value.strip_prefix("call-"))
            .expect("--model call-N recorded in argv");
        let stdin = channel_payloads(&capture, Channel::Stdin);
        let wire: serde_json::Value = serde_json::from_str(stdin[0]).unwrap();
        let wire_prompt = wire["message"]["content"].as_str().unwrap();
        let wire_index = wire_prompt
            .strip_prefix("scenario:capture-fresh call-")
            .expect("wire line carries the counted prompt");

        assert_eq!(
            model_index, wire_index,
            "the recorded argv (--model call-{model_index}) and the recorded wire line \
             (call-{wire_index}) must describe the same request, not two independent builder \
             calls: {stdin:?}"
        );
    }

    /// Task 2: the twelve per-row `*_row_is_wired_to_*_request` tests (six in
    /// `record/scenarios/claude.rs`, six in `record/scenarios/codex.rs`) folded into one loop over
    /// every `Run` row in `SCENARIOS`, replacing:
    ///
    /// - claude.rs: `fresh_text_row_is_wired_to_fresh_text_request`,
    ///   `resume_row_is_wired_to_resume_request`, `attachment_row_is_wired_to_attachment_request`,
    ///   `checklist_row_is_wired_to_checklist_request`,
    ///   `checklist_resume_row_is_wired_to_checklist_resume_request`,
    ///   `approval_row_is_wired_to_approval_request`
    /// - codex.rs: `fresh_text_row_is_wired_to_fresh_text_request`,
    ///   `resume_row_is_wired_to_resume_request`, `steer_row_is_wired_to_steer_request`,
    ///   `interruption_row_is_wired_to_interruption_request`,
    ///   `approval_row_is_wired_to_approval_request`,
    ///   `approval_on_request_row_is_wired_to_approval_on_request_request`
    ///
    /// Those twelve were tautologies before Task 1 (each compared `build_request(&input)` against
    /// itself) but Task 1 gave every one of them a second, independent call through the row's own
    /// `spec.launch` — turning them into the only per-row purity check in the suite: two calls to
    /// the SAME builder, through TWO different paths, must agree. This test keeps exactly that
    /// property but drops the per-row duplication, and — unlike the twelve hand-written copies —
    /// covers any `Run` row added later automatically.
    ///
    /// Break caught, on either hazard:
    /// - **non-purity**: `build_request(&input)` called twice returns two different `RunRequest`s.
    ///   This is the hazard the whole plan exists to close — see this file's own
    ///   `scenario_launch_and_body_must_share_one_request_builder_call` for the recorder-level half
    ///   (a non-pure builder called once still can't disagree with itself); this is the row-level
    ///   half, catching a non-pure builder BEFORE it ever reaches the recorder.
    /// - **derivation drift**: `derive_launch` — the actual, only call site `record_claude`/
    ///   `record_codex` use — stops producing the same `LaunchDescriptor` `run_launch(exe, &first)`
    ///   would, e.g. a future edit routes a row through the wrong provider's `run_launch` or drops
    ///   the executable/request pairing.
    #[test]
    fn every_run_rows_request_builder_is_pure_and_derives_its_own_launch() {
        for spec in SCENARIOS {
            let ScenarioLaunch::Run(build_request) = spec.launch else {
                continue;
            };
            let input = ScenarioInput {
                resume_id: spec
                    .requirements
                    .needs_resume_id
                    .then(|| "purity-loop-resume-id".to_owned()),
                attachment: spec
                    .requirements
                    .needs_attachment
                    .then(|| PathBuf::from("tiny.png")),
                approval_target: spec
                    .requirements
                    .needs_approval_target
                    .then(|| PathBuf::from("target-dir")),
                ..ScenarioInput::default()
            };

            let first = build_request(&input).unwrap_or_else(|err| {
                panic!(
                    "{:?}/{}: first request-builder call failed: {err}",
                    spec.provider, spec.name
                )
            });
            let second = build_request(&input).unwrap_or_else(|err| {
                panic!(
                    "{:?}/{}: second request-builder call failed: {err}",
                    spec.provider, spec.name
                )
            });
            assert_eq!(
                first, second,
                "{:?}/{}: calling the row's request builder twice with the same input produced \
                 two different RunRequests — the builder is not pure",
                spec.provider, spec.name
            );

            let (exe, run_launch): (PathBuf, fn(&Path, &RunRequest) -> LaunchDescriptor) =
                match spec.provider {
                    Provider::Claude => (absolute_program("claude"), crate::claude::run_launch),
                    Provider::Codex => (absolute_program("codex"), crate::codex::run_launch),
                };
            let (derived, _request) = derive_launch(&spec.launch, &input, &exe, run_launch)
                .unwrap_or_else(|err| {
                    panic!(
                        "{:?}/{}: derive_launch failed: {err}",
                        spec.provider, spec.name
                    )
                });
            let expected = run_launch(&exe, &first);
            assert_eq!(
                CommandSnapshot::from_launch(&derived),
                CommandSnapshot::from_launch(&expected),
                "{:?}/{}: the launch record.rs's own derive_launch produced does not match \
                 run_launch(exe, &first) — the row's launch and its request have drifted apart",
                spec.provider,
                spec.name
            );
        }
    }

    fn only_raw_subdirectory(raw_root: &Path) -> PathBuf {
        let mut entries: Vec<_> = std::fs::read_dir(raw_root)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect();
        assert_eq!(
            entries.len(),
            1,
            "expected exactly one raw capture directory"
        );
        entries.remove(0)
    }
}
