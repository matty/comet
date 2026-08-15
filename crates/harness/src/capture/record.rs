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
use std::path::PathBuf;
use std::pin::Pin;

use provider::CaptureProvider;
use providers::claude::ClaudeProvider;
use providers::codex::CodexProvider;
pub use scenarios::{Requirements, SCENARIOS, ScenarioSpec, scenario};
use scenarios::{ScenarioBody, ScenarioInput};
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
    let launch = (spec.launch)(&input, &executable)?;
    // Claude has no pre-spawn fence today: `approval` (its only scenario
    // with an approval surface) protects no filesystem identity the way a
    // Codex approval target or cwd does — see `record/scenarios/claude.rs`'s
    // `approval` doc comment.
    record_generic(
        ClaudeProvider,
        config,
        launch,
        input,
        body,
        FenceOutcome::none(),
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
    let launch = (spec.launch)(&input, &executable)?;
    let fence = codex_fence(spec, config, &launch)?;
    record_generic(CodexProvider::new(), config, launch, input, body, fence).await
}

/// Builds the pre-spawn fence for the two Codex scenarios that need one.
/// Every other scenario (every Claude row, and every Codex row but
/// `approval`/`approval-on-request`) gets [`FenceOutcome::none`].
///
/// This is the pre-spawn fence decision #6 in the stage plan requires stay —
/// `crate::capture::approval`'s checks ran inside `recording.rs`'s
/// `RecordingSession::start` before this task deleted that file; this is
/// their new home, run before `Session::start` is even called.
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
        // `crate::capture::approval::validate_on_request_preflight`'s
        // doc comment and `docs/testing/provider-captures.md` describe.
        let cwd = launch_cwd()?;
        let target = config.approval_target.clone();
        let identity =
            crate::capture::approval::validate_on_request_preflight(&cwd, target.as_deref())?;
        let recheck_cwd = cwd.clone();
        let recheck_target = target.clone();
        let recheck_identity = identity.clone();
        return Ok(FenceOutcome {
            approval_target: target,
            approval_target_identity: identity,
            approval_cwd_identity: None,
            trusted_powershell: None,
            recheck: Some(Box::new(move || {
                let spawn_identity = crate::capture::approval::validate_on_request_preflight(
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
        let trusted = crate::capture::approval::resolve_trusted_powershell(&cwd, &config.raw_root)?;
        let identity = crate::capture::approval::validate_ordinary_approval_cwd(&cwd, None, true)?;
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
) -> anyhow::Result<RawCapture> {
    let mut session = Session::start(provider, config, launch, fence).await?;
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

    /// Break caught: the hard-timeout branch returns before killing and reaping the child, or the
    /// exit wait's own budget silently outlives the configured timeout. Drives a REAL timeout
    /// through `record()` — not a hand-copied reproduction of its own timeout-handling code —
    /// using a fixture that receives the discovery initialize request and genuinely never replies.
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
        let launch = (spec.launch)(&input, &executable).unwrap();

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
        let launch = (spec.launch)(&input, &executable).unwrap();

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
        let launch = (spec.launch)(&input, &executable).unwrap();

        let fence = codex_fence(spec, &cfg, &launch).unwrap();

        assert!(
            fence.approval_cwd_identity.is_some(),
            "approval's fence must record an expected cwd identity for the grant-time recheck \
             to compare against"
        );
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
