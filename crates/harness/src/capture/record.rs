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

use crate::capture::types::{CaptureConfig, PartialFailureClass, RawCapture};
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

/// Resolve the executable, derive the launch, build the fence, and dispatch into
/// `record_generic` — the whole of what `record_claude`/`record_codex` used to duplicate before
/// `launch`, `request` and `fence` all moved onto the row (Tasks 1 and 4). What's left differing
/// between the two providers is exactly the resolver named in `AGENTS.md`'s "What the providers
/// send": the default-executable lookup and `run_launch`, both plain per-provider `fn` values —
/// passed in here rather than added to `CaptureProvider` as a fifth member. That trait's own doc
/// comment (`provider.rs`) is explicit that a fifth member is earned by a third provider having
/// a *recording* to design against, not added ahead of one; neither of these varies per scenario
/// the way `launch`/`fence` do (the reason those two live on the row and not the trait), so a
/// plain parameter is the version that doesn't anticipate anything.
async fn record_provider<P: CaptureProvider>(
    provider: P,
    config: &CaptureConfig,
    spec: &ScenarioSpec,
    input: ScenarioInput,
    body: ScenarioBodyFn<P>,
    default_executable: fn() -> Option<PathBuf>,
    run_launch: fn(&Path, &RunRequest) -> LaunchDescriptor,
) -> anyhow::Result<RawCapture> {
    let executable = session::resolve_executable(
        P::provider(),
        config.executable.clone().or_else(default_executable),
    )?;
    let (launch, request) = derive_launch(&spec.launch, &input, &executable, run_launch)?;
    let fence = (spec.fence)(spec, config, &launch)?;
    record_generic(provider, config, launch, input, body, fence, request).await
}

async fn record_claude(
    config: &CaptureConfig,
    spec: &ScenarioSpec,
    input: ScenarioInput,
    body: ScenarioBodyFn<ClaudeProvider>,
) -> anyhow::Result<RawCapture> {
    record_provider(
        ClaudeProvider,
        config,
        spec,
        input,
        body,
        crate::claude::resolve_claude_executable,
        crate::claude::run_launch,
    )
    .await
}

async fn record_codex(
    config: &CaptureConfig,
    spec: &ScenarioSpec,
    input: ScenarioInput,
    body: ScenarioBodyFn<CodexProvider>,
) -> anyhow::Result<RawCapture> {
    record_provider(
        CodexProvider::new(),
        config,
        spec,
        input,
        body,
        crate::codex::resolve_codex_executable,
        crate::codex::run_launch,
    )
    .await
}

/// The only call site for a row's `ScenarioLaunch::Run` builder IN PRODUCTION CODE — this test
/// module's `every_row_s_builder_and_fence_match_its_declared_wiring` also calls a row's builder
/// through `spec.launch`, deliberately, as half of checking the row is wired to the right one.
/// Builds the `RunRequest` once, derives the launch from it through the
/// provider's own `run_launch`, and returns the same `RunRequest` so the caller can hand it
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

/// Builds the pre-spawn fence for the two Codex scenarios that name it —
/// `record/scenarios.rs`'s `approval` and `approval-on-request` rows point
/// their own `fence` field at this function directly. D79: this used to be
/// reached unconditionally for every Codex row, picking this branch by
/// testing `spec.runtime_mode == Some(RuntimeMode::ApprovalRequired)` —
/// which meant a *future* Codex row that legitimately wanted
/// `ApprovalRequired` for an unrelated reason would have silently inherited
/// the Windows-only trusted-PowerShell fence below (see
/// `resolve_trusted_powershell`'s own doc comment for why that fails closed
/// elsewhere rather than spawning unprotected). Now that every row must name
/// its own fence — [`scenarios::no_fence`] is the default — reaching this
/// function at all is itself the declaration; the `needs_approval_target`
/// check below only chooses between this function's own two fences, not
/// whether a fence runs at all.
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

    // `approval`: a trusted, protected-root PowerShell (Windows only — fails
    // closed elsewhere, see `resolve_trusted_powershell`'s own doc comment)
    // and a cwd whose identity `record::scenarios::codex::approval` rechecks
    // at every grant, via `approval_cwd_identity` below.
    let cwd = launch_cwd()?;
    let trusted = crate::capture::safety::resolve_trusted_powershell(&cwd, &config.raw_root)?;
    let identity = crate::capture::safety::validate_ordinary_approval_cwd(&cwd, None, true)?;
    Ok(FenceOutcome {
        approval_target: None,
        approval_target_identity: None,
        approval_cwd_identity: Some(identity),
        trusted_powershell: Some(trusted),
        recheck: None,
    })
}

/// The pre-spawn fence for both providers' `full-access` row.
///
/// `FullAccess` disables the sandbox entirely — Claude's `bypassPermissions`
/// and Codex's `danger-full-access` both mean nothing stands between the
/// model and the filesystem — so unlike `codex_fence`'s two rows there is no
/// approval channel to protect at grant time: nothing will ever ask, so
/// there is nothing to recheck. The one guarantee a pre-spawn check can
/// still give is that the process does not start inside a repository this
/// project's own operator might care about. Reuses
/// `safety::repository_root`, the same primitive `validate_on_request_preflight`
/// already applies to Codex's on-request approval cwd, rather than inventing
/// a second "is this cwd safe" rule for one more row.
///
/// Applies to Claude's `full-access` row too, not only Codex's — the blast
/// radius is the same on either provider, and the check itself is
/// provider-neutral, so the row is wired to this function directly rather
/// than a Codex-only one.
///
/// Deliberately does **not** forbid the system temp tree the way
/// `resolve_trusted_powershell`'s forbidden roots do: a disposable directory
/// is exactly what the safety note in the stage plan asks an operator to
/// point this scenario at, and a temp directory is a reasonable choice for
/// that. The one thing this fence protects against is the operator forgetting
/// `--cwd` and the process landing, unsandboxed, in whatever repository they
/// happened to be sitting in.
fn full_access_fence(
    _spec: &ScenarioSpec,
    _config: &CaptureConfig,
    launch: &LaunchDescriptor,
) -> anyhow::Result<FenceOutcome> {
    let cwd = launch.cwd.clone().ok_or_else(|| {
        anyhow::anyhow!("Full-access capture requires a resolved working directory.")
    })?;
    let canonical = std::fs::canonicalize(&cwd)
        .map_err(|_| anyhow::anyhow!("Full-access capture cwd could not be validated."))?;
    if crate::capture::safety::repository_root(&canonical).is_some() {
        anyhow::bail!(
            "Full-access capture requires a non-repository, non-worktree cwd — pick a disposable \
             directory outside anything you care about."
        );
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
    use crate::capture::types::{Channel, CommandSnapshot, Provider};
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
            "model-discovery",
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
        assert_eq!(persisted["scenario"], "model-discovery");
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

    /// `full_access_fence`'s whole reason to exist: reject a cwd inside a git repository or
    /// linked worktree, cross-platform (unlike `codex_fence`, which is Windows-only). Both
    /// providers' `full-access` row points at this same function.
    #[test]
    fn full_access_fence_rejects_a_repository_cwd() {
        let raw = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        std::fs::create_dir(cwd.path().join(".git")).unwrap();
        let cfg = config(
            "full-access",
            fixture_path("fake-codex"),
            "codex",
            raw.path(),
        );
        let spec = scenario("codex", "full-access").unwrap();
        let launch = LaunchDescriptor {
            program: fixture_path("fake-codex"),
            args: Vec::new(),
            cwd: Some(cwd.path().into()),
            configured_env: Default::default(),
            stdin: StdioMode::Piped,
            stdout: StdioMode::Piped,
            stderr: StdioMode::Piped,
            kill_on_drop: true,
            #[cfg(windows)]
            creation_flags: 0,
        };

        let error = match super::full_access_fence(spec, &cfg, &launch) {
            Ok(_) => panic!("a repository cwd must be rejected"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("non-repository"), "{error}");
    }

    /// The other half: an ordinary, non-repository directory passes.
    #[test]
    fn full_access_fence_accepts_a_plain_directory() {
        let raw = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let cfg = config(
            "full-access",
            fixture_path("fake-codex"),
            "codex",
            raw.path(),
        );
        let spec = scenario("codex", "full-access").unwrap();
        let launch = LaunchDescriptor {
            program: fixture_path("fake-codex"),
            args: Vec::new(),
            cwd: Some(cwd.path().into()),
            configured_env: Default::default(),
            stdin: StdioMode::Piped,
            stdout: StdioMode::Piped,
            stderr: StdioMode::Piped,
            kill_on_drop: true,
            #[cfg(windows)]
            creation_flags: 0,
        };

        assert!(super::full_access_fence(spec, &cfg, &launch).is_ok());
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
            fence: scenarios::no_fence,
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

    /// Task 2's twelve-row purity/wiring loop and D79's twenty-row fence loop, merged into one
    /// loop over every row in `SCENARIOS` against one `(provider, name) → (builder, fence)`
    /// table. A whole-branch review flagged the two as one 20-row roster enumerated across two
    /// separate tables with two separate coverage guards; this is that merge — same coverage, one
    /// table and one guard instead of two.
    ///
    /// What the merged table replaces:
    /// - the twelve per-row `*_row_is_wired_to_*_request` tests (six in
    ///   `record/scenarios/claude.rs`: `fresh_text_row_is_wired_to_fresh_text_request`,
    ///   `resume_row_is_wired_to_resume_request`, `attachment_row_is_wired_to_attachment_request`,
    ///   `checklist_row_is_wired_to_checklist_request`,
    ///   `checklist_resume_row_is_wired_to_checklist_resume_request`,
    ///   `approval_row_is_wired_to_approval_request`; six in `record/scenarios/codex.rs`:
    ///   `fresh_text_row_is_wired_to_fresh_text_request`, `resume_row_is_wired_to_resume_request`,
    ///   `steer_row_is_wired_to_steer_request`, `interruption_row_is_wired_to_interruption_request`,
    ///   `approval_row_is_wired_to_approval_request`,
    ///   `approval_on_request_row_is_wired_to_approval_on_request_request`), each of which
    ///   compared `build_request(&input)` via `spec.launch` against the same-named builder called
    ///   directly;
    /// - D79's own recorded residual (`docs/debt/closed.md`): Task 4 moved fence selection onto
    ///   each row (`scenarios::no_fence` for eighteen rows, `codex_fence` for the two Codex
    ///   approval rows), but nothing checked a row's `fence` field against what it SHOULD be.
    ///   D79's entry names the fix by its future name directly: "a future `EXPECTED_FENCES`-style
    ///   table, mirroring the existing `EXPECTED_RUN_BUILDERS` one, would close it the same way" —
    ///   this loop is that table, merged with the run-builder one rather than kept beside it.
    ///
    /// What this loop checks, per row:
    /// - **purity** (`Run` rows only): `build_request` is called twice (`first`/`second` below)
    ///   and must agree.
    /// - **run-builder wiring** (`Run` rows only): `first` is also compared against
    ///   `EXPECTED_ROWS`' entry for this row — the builder that row is supposed to name, looked up
    ///   by `(provider, name)` rather than through `spec.launch` a second time. A row repointed at
    ///   another row's builder disagrees with the table and fails, naming the row.
    /// - **fence wiring** (every row): `spec.fence` is fingerprinted (see below) and compared
    ///   against `EXPECTED_ROWS`' `FenceKind` entry for this row.
    /// - **Run/Discovery agreement** (every row): `EXPECTED_ROWS`' builder field must be `Some`
    ///   exactly when the row is `ScenarioLaunch::Run` and `None` exactly when it is
    ///   `ScenarioLaunch::Discovery` — a row flipped from one to the other without updating the
    ///   table fails this directly, naming the row, instead of silently dropping out of a loop
    ///   built only for one kind.
    /// - **coverage** (both directions, after the loop): `EXPECTED_ROWS` must list every row in
    ///   `SCENARIOS` exactly once. A row with no table entry panics inside the loop, naming the
    ///   row; a stale table entry with no matching row fails the `covered == expected` set
    ///   comparison.
    ///
    /// It also keeps a property the twelve never had: `ScenarioInput` is derived from
    /// `spec.requirements` here rather than hardcoded per row, so a row whose
    /// `needs_resume_id`/`needs_attachment`/`needs_approval_target` flag disagrees with what its
    /// own builder demands now fails here too (e.g. clearing `needs_resume_id` on a resume row
    /// makes its builder return a "needs a --resume-id" error and the loop panics).
    ///
    /// The fence fingerprint: comparing `spec.fence` by function-pointer identity
    /// (`std::ptr::fn_addr_eq`) was considered and rejected — two distinct `fn` items are not
    /// guaranteed to have distinct addresses across codegen units, so that comparison can pass
    /// while comparing nothing. Instead this fingerprints a row's fence by an OBSERVABLE property
    /// the two real fences differ on unconditionally: `codex_fence`'s very first statement in BOTH
    /// of its branches (above, this file) is `let cwd = launch_cwd()?;`, which fails with
    /// "requires a resolved working directory" whenever `launch.cwd` is `None` — before it reads
    /// `spec.requirements`, `config`, or the filesystem at all. `no_fence` ignores every argument
    /// and always returns `Ok`. Calling a row's fence with a `cwd: None` launch therefore
    /// distinguishes the two kinds deterministically, without any of the real filesystem state (a
    /// trusted PowerShell, an approval target, a cwd identity) `codex_fence`'s ordinary path needs
    /// — which is what keeps this portable to the Linux CI this workspace runs on, where
    /// `resolve_trusted_powershell` fails closed regardless of input (see its own doc comment).
    ///
    /// Break caught, on any of these hazards:
    /// - **non-purity**: `build_request(&input)` called twice returns two different `RunRequest`s.
    ///   This only catches impurity that varies between two adjacent calls in the same process —
    ///   a counter, a fine clock. The impurity this codebase actually has —
    ///   `normalize_run_request` reading `.git` state, named on `cheap_codex_request`'s own doc —
    ///   returns the same value on both calls whenever the filesystem is stable during the test,
    ///   which is always, so this assertion cannot catch it. What makes that impurity harmless is
    ///   the single-call architecture itself (see this file's own
    ///   `scenario_launch_and_body_must_share_one_request_builder_call`), not this loop; the
    ///   corpus pin (`every_scenario_launch_matches_its_committed_corpus_manifest`) is what would
    ///   catch machine-stable argv drift. This assertion stays cheap and still catches the
    ///   call-to-call non-determinism class, so it is kept.
    /// - **run-builder mis-wiring**: `spec.launch` names a different row's builder — `first`
    ///   disagrees with `EXPECTED_ROWS`' entry for this row's `(provider, name)`.
    /// - **fence mis-wiring**: pointing a non-approval Codex row (e.g. `steer`) at `codex_fence` —
    ///   the exact mis-wiring D79's residual named as uncaught — makes that row's fingerprint
    ///   `CodexApproval` while `EXPECTED_ROWS` still says `None`, and this test fails naming the
    ///   row.
    /// - **a row silently changing kind**: a `Run` row flipped to `Discovery` (or the reverse)
    ///   without updating the table — caught directly by the Run/Discovery agreement check.
    /// - **a row silently leaving the loop entirely**: renamed without a matching table entry, or
    ///   a stale table entry with no matching row — caught by the `covered == expected` count
    ///   check.
    ///
    /// One assertion this test does NOT independently prove: `derive_launch` — the actual, only
    /// production call site — is checked against `run_launch(exe, &first)`, where `run_launch` is
    /// chosen here by `spec.provider` (not by `spec.body`, which is what production actually
    /// dispatches on). That the two choices agree is pinned by a different test,
    /// `every_row_s_declared_provider_matches_its_body_variant` (`scenarios.rs`), not this
    /// one. And because `derive_launch`'s `Run` arm is exactly `run_launch(executable,
    /// &build_request(input)?)`, this assertion is a change-detector over that one function's
    /// three-line body rather than an independent oracle — it still catches an edit that drops
    /// the executable/request pairing or otherwise changes what that arm returns, just not a
    /// provider mis-dispatch (that hazard belongs to the test named above).
    #[test]
    fn every_row_s_builder_and_fence_match_its_declared_wiring() {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        enum FenceKind {
            None,
            CodexApproval,
            FullAccess,
        }

        // The `(provider, name) → (builder, fence)` table the twelve-plus-twenty rows across the
        // old two tables implicitly encoded. Kept exhaustive by the `covered == expected` check
        // below: a row missing here — or a stale entry with no matching row — fails that
        // assertion instead of the gap going unnoticed.
        type RunBuilder = fn(&ScenarioInput) -> anyhow::Result<RunRequest>;
        const EXPECTED_ROWS: &[(Provider, &str, Option<RunBuilder>, FenceKind)] = &[
            (Provider::Claude, "model-discovery", None, FenceKind::None),
            (Provider::Claude, "command-discovery", None, FenceKind::None),
            (
                Provider::Claude,
                "fresh-text",
                Some(scenarios::claude::fresh_text_request),
                FenceKind::None,
            ),
            (
                Provider::Claude,
                "approval",
                Some(scenarios::claude::approval_request),
                FenceKind::None,
            ),
            (
                Provider::Claude,
                "resume",
                Some(scenarios::claude::resume_request),
                FenceKind::None,
            ),
            (
                Provider::Claude,
                "attachment",
                Some(scenarios::claude::attachment_request),
                FenceKind::None,
            ),
            (
                Provider::Claude,
                "checklist",
                Some(scenarios::claude::checklist_request),
                FenceKind::None,
            ),
            (
                Provider::Claude,
                "checklist-resume",
                Some(scenarios::claude::checklist_resume_request),
                FenceKind::None,
            ),
            (
                Provider::Claude,
                "auto",
                Some(scenarios::claude::auto_request),
                FenceKind::None,
            ),
            (
                Provider::Claude,
                "full-access",
                Some(scenarios::claude::full_access_request),
                FenceKind::FullAccess,
            ),
            (Provider::Codex, "model-discovery", None, FenceKind::None),
            (
                Provider::Codex,
                "model-discovery-logged-out",
                None,
                FenceKind::None,
            ),
            (
                Provider::Codex,
                "fresh-text",
                Some(scenarios::codex::fresh_text_request),
                FenceKind::None,
            ),
            (
                Provider::Codex,
                "approval",
                Some(scenarios::codex::approval_request),
                FenceKind::CodexApproval,
            ),
            (
                Provider::Codex,
                "approval-on-request",
                Some(scenarios::codex::approval_on_request_request),
                FenceKind::CodexApproval,
            ),
            (
                Provider::Codex,
                "resume",
                Some(scenarios::codex::resume_request),
                FenceKind::None,
            ),
            (
                Provider::Codex,
                "steer",
                Some(scenarios::codex::steer_request),
                FenceKind::None,
            ),
            (
                Provider::Codex,
                "interruption",
                Some(scenarios::codex::interruption_request),
                FenceKind::None,
            ),
            (
                Provider::Codex,
                "auto",
                Some(scenarios::codex::auto_request),
                FenceKind::None,
            ),
            (
                Provider::Codex,
                "full-access",
                Some(scenarios::codex::full_access_request),
                FenceKind::FullAccess,
            ),
        ];

        let raw = tempfile::tempdir().unwrap();
        let mut covered: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

        for spec in SCENARIOS {
            covered.insert(format!("{:?}/{}", spec.provider, spec.name));
            let (_, _, expected_builder, expected_fence) = EXPECTED_ROWS
                .iter()
                .find(|(provider, name, _, _)| *provider == spec.provider && *name == spec.name)
                .unwrap_or_else(|| {
                    panic!(
                        "{:?}/{}: no entry in EXPECTED_ROWS — add one so this row's wiring is \
                         checked",
                        spec.provider, spec.name
                    )
                });

            // Fence wiring — every row, discovery and run alike.
            let provider_str = match spec.provider {
                Provider::Claude => "claude",
                Provider::Codex => "codex",
            };
            let cfg = config(
                spec.name,
                PathBuf::from("provider"),
                provider_str,
                raw.path(),
            );
            let launch = LaunchDescriptor {
                program: Path::new("provider").into(),
                args: Vec::new(),
                cwd: None,
                configured_env: Default::default(),
                stdin: StdioMode::Piped,
                stdout: StdioMode::Piped,
                stderr: StdioMode::Piped,
                kill_on_drop: true,
                #[cfg(windows)]
                creation_flags: 0,
            };
            let actual_fence = match (spec.fence)(spec, &cfg, &launch) {
                Ok(_) => FenceKind::None,
                Err(error) => {
                    let message = error.to_string();
                    if message.starts_with("Codex approval capture requires") {
                        FenceKind::CodexApproval
                    } else if message.starts_with("Full-access capture requires") {
                        FenceKind::FullAccess
                    } else {
                        panic!(
                            "{:?}/{}: fence errored on a cwd-less launch with an unrecognized \
                             message — got {error}",
                            spec.provider, spec.name
                        );
                    }
                }
            };
            assert_eq!(
                actual_fence, *expected_fence,
                "{:?}/{}: fence kind mismatch — the row is wired to a different fence than \
                 EXPECTED_ROWS says it should be",
                spec.provider, spec.name
            );

            // Run-builder wiring and Run/Discovery agreement.
            match (spec.launch, expected_builder) {
                (ScenarioLaunch::Discovery(_), None) => continue,
                (ScenarioLaunch::Discovery(_), Some(_)) => panic!(
                    "{:?}/{}: EXPECTED_ROWS names a builder for this row but it is a Discovery \
                     row — the row was flipped from Run without updating the table",
                    spec.provider, spec.name
                ),
                (ScenarioLaunch::Run(_), None) => panic!(
                    "{:?}/{}: this is a Run row but EXPECTED_ROWS names no builder for it — the \
                     row was flipped from Discovery without updating the table",
                    spec.provider, spec.name
                ),
                (ScenarioLaunch::Run(build_request), Some(expected_builder)) => {
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
                        "{:?}/{}: calling the row's request builder twice with the same input \
                         produced two different RunRequests — the builder is not pure",
                        spec.provider, spec.name
                    );

                    let expected_request = expected_builder(&input).unwrap_or_else(|err| {
                        panic!(
                            "{:?}/{}: EXPECTED_ROWS' builder failed: {err}",
                            spec.provider, spec.name
                        )
                    });
                    assert_eq!(
                        first, expected_request,
                        "{:?}/{}: spec.launch's builder does not match the builder \
                         EXPECTED_ROWS says this row should name — the row is wired to the \
                         wrong builder",
                        spec.provider, spec.name
                    );

                    let (exe, run_launch): (PathBuf, fn(&Path, &RunRequest) -> LaunchDescriptor) =
                        match spec.provider {
                            Provider::Claude => {
                                (absolute_program("claude"), crate::claude::run_launch)
                            }
                            Provider::Codex => {
                                (absolute_program("codex"), crate::codex::run_launch)
                            }
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
                        "{:?}/{}: the launch record.rs's own derive_launch produced does not \
                         match run_launch(exe, &first) — the row's launch and its request have \
                         drifted apart",
                        spec.provider,
                        spec.name
                    );
                }
            }
        }

        let expected: std::collections::BTreeSet<String> = EXPECTED_ROWS
            .iter()
            .map(|(provider, name, _, _)| format!("{provider:?}/{name}"))
            .collect();
        assert_eq!(
            covered, expected,
            "every row in SCENARIOS must have exactly one entry in EXPECTED_ROWS, and vice versa \
             — a row missing here would otherwise leave the loop in silence"
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

    /// Task 3 (`2026-08-16-scenario-request-builders.md`): the purity/wiring checks in the loop
    /// above (`every_row_s_builder_and_fence_match_its_declared_wiring`) prove a row's builder is
    /// pure and that `derive_launch` faithfully turns its `RunRequest` into a launch — but every
    /// oracle in that test is itself derived from the SAME source code this
    /// task exists to check. This test compares against something none of that code can
    /// influence: the committed capture archive, frozen before this branch existed (`git diff
    /// 7d4e903..HEAD -- crates/harness/tests/corpus/` is empty) and explicitly protected by the
    /// plan's own constraints ("No byte under `crates/harness/tests/corpus/` changes").
    ///
    /// Every declared row has corpus evidence as of the stage-6 promotion — `EXEMPT_UNCAPTURED`
    /// below is empty, matching `capture_corpus/scenario_coverage.rs`'s own now-empty list.
    /// `claude/2.1.229/subagent` is a hand-sanitized exploratory capture with no matching
    /// `SCENARIOS` row (see its own `README.md`) and is never looked up, since lookup is driven
    /// by row name, not by directory listing.
    ///
    /// Comparison is STRUCTURAL, not byte-for-byte — the archive redacts `cwd`, `program`, and
    /// any resume/session id embedded in argv (`docs/testing/provider-captures.md`):
    /// - `args`: compared for exact equality after normalizing a `--resume=<id>` token on BOTH
    ///   sides to `--resume=<REDACTED>` — the only value-bearing argv token any scenario here
    ///   produces. Every other token — every flag, and every other value (model id, effort,
    ///   permission-mode, `--bare`) — is real, unredacted, and compared literally; the archive
    ///   does not redact those.
    /// - `cwd`: presence only (`Some` vs `Some`, `None` vs `None`) — the archive redacts the
    ///   value itself to `<CWD>`.
    /// - `program`: compared by final path component with any `.exe` suffix stripped (via
    ///   `program_stem`, below) — the archive redacts everything BEFORE the binary name
    ///   (`<HOME>\...\claude.exe`), but keeps the name itself, so `claude` vs `codex` is a real
    ///   assertion, not a no-op. A bare `is_empty()` check on both sides used to stand in here
    ///   and could never fail for any production change — the derived side is always
    ///   `current_dir().join("claude"|"codex")`, never empty.
    /// - `configured_env`: key set only — a set value (`CODEX_HOME`) redacts to `<CODEX_HOME>`.
    /// - `stdin`/`stdout`/`stderr`/`kill_on_drop`/`creation_flags` (Windows only): exact
    ///   equality — none of these carry machine- or session-specific data, and `creation_flags`
    ///   is a compile-time constant per launch function (`0x0800_0000` for every discovery
    ///   launch, `0` for every run launch), not something spawn-time state could vary.
    #[test]
    fn every_scenario_launch_matches_its_committed_corpus_manifest() {
        const EXEMPT_UNCAPTURED: &[(Provider, &str)] = &[];

        let root = crate::capture::corpus_root();
        let promoted = crate::capture::promoted_scenarios(&root)
            .unwrap_or_else(|error| panic!("{} could not be walked: {error}", root.display()));

        let mut failures = Vec::new();
        let mut unevidenced: Vec<String> = Vec::new();

        for spec in SCENARIOS {
            let provider_str = match spec.provider {
                Provider::Claude => "claude",
                Provider::Codex => "codex",
            };
            let label = format!("{provider_str}/{}", spec.name);

            // EVERY corpus directory this scenario has, across every version — not just the
            // first `.find()` turns up. Versions sort ascending (`promoted_scenarios`'s own doc
            // comment), so a `.find()` here always binds to the OLDEST version's manifest,
            // silently ignoring any newer one — harmless today (no scenario exists under two
            // versions yet) but not once a live re-capture promotes a second version of an
            // existing scenario: a freshly captured `claude/2.1.233/fresh-text` would sit right
            // beside `2.1.228/fresh-text` unchecked, and this test would keep passing against
            // the superseded evidence. The launch under test is version-independent — built from
            // the same production code regardless of which CLI version produced the corpus
            // evidence — so every version's manifest is a valid oracle, and checking all of them
            // is strictly stronger than checking one.
            let scenario_dirs: Vec<&crate::capture::PromotedScenario> = promoted
                .iter()
                .filter(|scenario| {
                    scenario.provider == provider_str
                        && scenario
                            .directory
                            .file_name()
                            .and_then(|name| name.to_str())
                            == Some(spec.name)
                })
                .collect();

            if scenario_dirs.is_empty() {
                if EXEMPT_UNCAPTURED.contains(&(spec.provider, spec.name)) {
                    unevidenced.push(label);
                } else {
                    failures.push(format!(
                        "{label}: no corpus evidence anywhere under {}, and no exemption in \
                         EXEMPT_UNCAPTURED",
                        root.display()
                    ));
                }
                continue;
            }

            // `cwd` only needs to be present (comparison is presence-only, see this test's own
            // doc comment); the two neutral-cwd discovery rows ignore it entirely in production
            // and always spawn from a temp directory regardless, so `Some(temp_dir())` here is
            // behaviourally identical to leaving it `None` for them.
            let input = ScenarioInput {
                cwd: Some(std::env::temp_dir()),
                resume_id: spec
                    .requirements
                    .needs_resume_id
                    .then(|| "corpus-pin-resume-id".to_owned()),
                attachment: spec
                    .requirements
                    .needs_attachment
                    .then(|| PathBuf::from("tiny.png")),
                approval_target: spec
                    .requirements
                    .needs_approval_target
                    .then(|| PathBuf::from("target-dir")),
                // Every Codex discovery row's launch builder needs a codex_home or falls back to
                // auto-discovering one from the real environment this test happens to run in —
                // supplying one explicitly keeps the test hermetic regardless of what's installed
                // on the machine running it.
                codex_home: (spec.provider == Provider::Codex && !spec.requirements.spends_tokens)
                    .then(|| std::env::temp_dir().join("comet-corpus-pin-codex-home")),
            };

            let (exe, run_launch): (PathBuf, fn(&Path, &RunRequest) -> LaunchDescriptor) =
                match spec.provider {
                    Provider::Claude => (absolute_program("claude"), crate::claude::run_launch),
                    Provider::Codex => (absolute_program("codex"), crate::codex::run_launch),
                };

            let (derived_launch, _request) = derive_launch(&spec.launch, &input, &exe, run_launch)
                .unwrap_or_else(|error| panic!("{label}: derive_launch failed: {error}"));
            let derived = CommandSnapshot::from_launch(&derived_launch);

            for scenario_dir in &scenario_dirs {
                let manifest_path = scenario_dir.directory.join("manifest.json");
                let manifest: serde_json::Value =
                    serde_json::from_str(&std::fs::read_to_string(&manifest_path).unwrap_or_else(
                        |error| panic!("{} could not be read: {error}", manifest_path.display()),
                    ))
                    .unwrap_or_else(|error| {
                        panic!("{} is not valid JSON: {error}", manifest_path.display())
                    });
                let corpus_command: CommandSnapshot =
                    serde_json::from_value(manifest["command"].clone()).unwrap_or_else(|error| {
                        panic!(
                            "{} has no valid command object: {error}",
                            manifest_path.display()
                        )
                    });

                // Names which version disagreed, not just the row — the whole point of checking
                // every version instead of the first is a message that says which one broke.
                let versioned_label = format!("{label} ({})", scenario_dir.version);
                failures.extend(compare_launch_against_corpus_manifest(
                    &versioned_label,
                    &derived,
                    &corpus_command,
                ));
            }
        }

        let mut unevidenced_sorted = unevidenced.clone();
        unevidenced_sorted.sort();
        assert_eq!(
            unevidenced_sorted,
            Vec::<String>::new(),
            "EXEMPT_UNCAPTURED is empty, so nothing should land in unevidenced — a row losing \
             corpus evidence must update this assertion deliberately, not pass through silently"
        );

        assert!(
            failures.is_empty(),
            "{} row(s) disagree with their committed corpus manifest:\n\n{}",
            failures.len(),
            failures.join("\n\n")
        );
    }

    /// The final path component of a `program`, with a trailing `.exe` stripped — `"claude"` from
    /// both `C:\dev\comet\claude` (the derived side, built by `absolute_program`, no extension)
    /// and `<HOME>\.local\bin\claude.exe` (the corpus side, redacted down to a directory prefix
    /// but keeping the binary name — see this file's `docs/testing/provider-captures.md`).
    ///
    /// Deliberately does NOT use `std::path::Path` — the corpus string was captured on Windows
    /// and always uses `\` regardless of which OS this test runs on, and `Path` on a non-Windows
    /// host (this workspace's CI runs `ubuntu-24.04`) treats `\` as an ordinary character, not a
    /// separator, so `Path::new(corpus).file_stem()` would return the whole redacted string
    /// unsplit. Splitting on both `/` and `\` by hand keeps this correct on every host.
    fn program_stem(raw: &str) -> String {
        let name = raw.rsplit(['/', '\\']).next().unwrap_or(raw);
        match name.rsplit_once('.') {
            Some((stem, ext)) if ext.eq_ignore_ascii_case("exe") => stem.to_owned(),
            _ => name.to_owned(),
        }
    }

    /// The archive-vs-derived comparator `every_scenario_launch_matches_its_committed_corpus_manifest`
    /// uses for one row, returning one message per mismatched field rather than stopping at the
    /// first — see that test's own doc comment for which fields are compared exactly and which
    /// only for presence/shape.
    fn compare_launch_against_corpus_manifest(
        label: &str,
        derived: &CommandSnapshot,
        corpus: &CommandSnapshot,
    ) -> Vec<String> {
        fn normalize_argv(args: &[String]) -> Vec<String> {
            args.iter()
                .map(|arg| {
                    if arg.starts_with("--resume=") {
                        "--resume=<REDACTED>".to_owned()
                    } else {
                        arg.clone()
                    }
                })
                .collect()
        }

        let mut failures = Vec::new();

        let derived_args = normalize_argv(&derived.args);
        let corpus_args = normalize_argv(&corpus.args);
        if derived_args != corpus_args {
            failures.push(format!(
                "{label}: args differ (normalized)\n    derived: {derived_args:?}\n    corpus:  {corpus_args:?}"
            ));
        }

        if derived.cwd.is_some() != corpus.cwd.is_some() {
            failures.push(format!(
                "{label}: cwd presence differs — derived {:?}, corpus {:?}",
                derived.cwd, corpus.cwd
            ));
        }
        let derived_stem = program_stem(&derived.program);
        let corpus_stem = program_stem(&corpus.program);
        if derived_stem.is_empty() || corpus_stem.is_empty() || derived_stem != corpus_stem {
            failures.push(format!(
                "{label}: program stem differs — derived {:?} (stem {derived_stem:?}), corpus \
                 {:?} (stem {corpus_stem:?})",
                derived.program, corpus.program
            ));
        }

        let derived_keys: std::collections::BTreeSet<&String> =
            derived.configured_env.keys().collect();
        let corpus_keys: std::collections::BTreeSet<&String> =
            corpus.configured_env.keys().collect();
        if derived_keys != corpus_keys {
            failures.push(format!(
                "{label}: configured_env key set differs — derived {derived_keys:?}, corpus {corpus_keys:?}"
            ));
        }

        if derived.stdin != corpus.stdin {
            failures.push(format!(
                "{label}: stdin differs — derived {:?}, corpus {:?}",
                derived.stdin, corpus.stdin
            ));
        }
        if derived.stdout != corpus.stdout {
            failures.push(format!(
                "{label}: stdout differs — derived {:?}, corpus {:?}",
                derived.stdout, corpus.stdout
            ));
        }
        if derived.stderr != corpus.stderr {
            failures.push(format!(
                "{label}: stderr differs — derived {:?}, corpus {:?}",
                derived.stderr, corpus.stderr
            ));
        }
        if derived.kill_on_drop != corpus.kill_on_drop {
            failures.push(format!(
                "{label}: kill_on_drop differs — derived {}, corpus {}",
                derived.kill_on_drop, corpus.kill_on_drop
            ));
        }
        #[cfg(windows)]
        if derived.creation_flags != corpus.creation_flags {
            failures.push(format!(
                "{label}: creation_flags differ — derived {:#x}, corpus {:#x}",
                derived.creation_flags, corpus.creation_flags
            ));
        }

        failures
    }
}
