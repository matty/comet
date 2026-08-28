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
use providers::acp::AcpProvider;
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
        claude_config_dir: config.claude_config_dir.clone(),
        approval_target: config.approval_target.clone(),
    };
    match &spec.body {
        ScenarioBody::Claude(body) => record_claude(&config, spec, input, *body).await,
        ScenarioBody::Codex(body) => record_codex(&config, spec, input, *body).await,
        ScenarioBody::Acp(body) => record_acp(&config, spec, input, *body).await,
    }
}

/// Resolves the executable, derives the launch, builds the fence, and dispatches into
/// `record_generic`. The two providers differ only in the default-executable lookup and
/// `run_launch`, passed as plain `fn` values rather than a fifth `CaptureProvider` member —
/// that trait only grows a member once a third provider exists to design it against
/// (`provider.rs`'s own doc comment).
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

async fn record_acp(
    config: &CaptureConfig,
    spec: &ScenarioSpec,
    input: ScenarioInput,
    body: ScenarioBodyFn<AcpProvider>,
) -> anyhow::Result<RawCapture> {
    record_provider(
        AcpProvider::new(),
        config,
        spec,
        input,
        body,
        crate::capture::record::scenarios::acp::resolve_node_executable,
        crate::capture::record::scenarios::acp::run_launch,
    )
    .await
}

/// Builds a `RunRequest` once and returns it alongside the derived launch, so the caller can
/// reuse it for `Session::request` instead of calling a Run builder a second time to construct
/// the wire line — see `ScenarioLaunch`'s own doc comment (`scenarios.rs`) for the hazard this
/// closes. A discovery row has no `RunRequest`, so that half of the match returns `None`.
fn derive_launch(
    launch: &ScenarioLaunch,
    input: &ScenarioInput,
    executable: &Path,
    run_launch: fn(&Path, &RunRequest) -> LaunchDescriptor,
) -> anyhow::Result<(LaunchDescriptor, Option<RunRequest>)> {
    let (mut launch, request) = match launch {
        ScenarioLaunch::Discovery(build_launch) => (build_launch(input, executable)?, None),
        ScenarioLaunch::Run(build_request) => {
            let request = build_request(input)?;
            let launch = run_launch(executable, &request);
            (launch, Some(request))
        }
    };
    // D91, applied here rather than inside the production launch builders: Comet itself never
    // sets `CLAUDE_CONFIG_DIR`, so teaching `claude::run_launch` about it would put a
    // capture-only concern on the runtime path — the boundary AGENTS.md asks to keep readable.
    // Isolating through the environment also leaves argv byte-identical to production's, which
    // is what makes a capture evidence of what Comet spawns (and is why `--safe-mode`, which
    // changes argv, is the wrong tool; see the debt page).
    if let Some(config_dir) = &input.claude_config_dir {
        launch
            .configured_env
            .insert("CLAUDE_CONFIG_DIR".into(), config_dir.clone().into());
    }
    Ok((launch, request))
}

/// The pre-spawn fence for Codex's `approval` and `approval-on-request` rows (both point their
/// `fence` field here). D79: a row reaching this function is itself the declaration that it
/// wants a fence — `needs_approval_target` below only picks which of this function's two
/// fences applies, not whether one runs at all. Do not gate entry on `runtime_mode` instead:
/// a future `ApprovalRequired` row for an unrelated reason would silently inherit the
/// Windows-only trusted-PowerShell fence (see `resolve_trusted_powershell`'s doc comment).
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
        // `approval-on-request`: a non-repository cwd and an empty, identity-stable,
        // isolated approval target. RECHECKED right before spawn (`FenceOutcome::recheck`)
        // because directory creation and the `--version` probe between this check and
        // spawn leave a real race window — see `validate_on_request_preflight`'s doc.
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

/// The pre-spawn fence for both providers' `full-access` row — Claude's `bypassPermissions` and
/// Codex's `danger-full-access` both remove the sandbox entirely, so there is no approval
/// channel to protect and nothing to recheck at grant time. The only guarantee left to give is
/// that the process doesn't start inside a repository this project's operator cares about
/// (reuses `safety::repository_root`). Shared by both providers since the blast radius is the
/// same either way. Deliberately allows the system temp tree, unlike `resolve_trusted_powershell`'s
/// forbidden roots — a disposable temp directory is the expected `--cwd` for this scenario; this
/// only guards against forgetting `--cwd` and landing, unsandboxed, in whatever repo was current.
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

/// The provider-neutral orchestration shared by every scenario: spawn, drive the scenario body,
/// finish, all under one shared deadline. A drive failure or timeout is classified explicitly
/// (`DriverError` vs `Timeout`) rather than reaching [`Session::finish`].
///
/// Deliberately does NOT call `P::handshake` unconditionally — whether a scenario handshakes is
/// a scenario-body decision, because a real Claude run sends no handshake and calling one here
/// would put a line on the tape the product never sends (see `CaptureProvider::handshake`'s doc).
///
/// `deadline` is computed once, right after spawn, and shared between the outer `timeout_at` and
/// [`Session::finish`]'s exit wait, so the exit wait doesn't get a separate, unrelated budget.
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
                ("CLAUDE_CONFIG_DIR".into(), "safe configured claude".into()),
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

        // D91: `CLAUDE_CONFIG_DIR` is captured for the same reason `CODEX_HOME` is — it decides
        // what the CLI answers with, so a manifest that omits it cannot tell an isolated capture
        // from one carrying the capturer's own plugins, skills and models.
        assert_eq!(
            snapshot.configured_env,
            [
                ("CODEX_HOME".into(), "safe configured home".into()),
                ("CLAUDE_CONFIG_DIR".into(), "safe configured claude".into()),
            ]
            .into()
        );
        assert_eq!(snapshot.stdin, StdioMode::Inherit);
        assert_eq!(snapshot.stdout, StdioMode::Null);
        assert_eq!(snapshot.stderr, StdioMode::Piped);
        assert!(!snapshot.kill_on_drop);
    }

    /// D91: an isolated config directory only isolates if it reaches the spawn. Every Claude
    /// row — discovery and run alike — must carry it, because the contamination is not confined
    /// to discovery: the `tools` array a run's `system`/`init` frame reports is the operator's
    /// MCP and plugin roster too.
    ///
    /// Codex rows are absent by construction rather than by assertion here: the binary rejects
    /// `--claude-config-dir` for them (`claude_config_dir_is_rejected_for_a_codex_scenario`), so
    /// `input.claude_config_dir` is only ever `Some` on a Claude row.
    ///
    /// Break caught, verified by falsification: threading the directory into the discovery
    /// launches only — every run row then fails here with "claude/fresh-text: CLAUDE_CONFIG_DIR
    /// missing from the launch environment". Restored after confirming.
    #[test]
    fn every_claude_launch_carries_the_configured_claude_config_dir() {
        let config_dir = std::env::temp_dir().join("comet-claude-config-pin");
        let exe = absolute_program("claude");

        for spec in SCENARIOS
            .iter()
            .filter(|spec| spec.provider == Provider::Claude)
        {
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
                approval_target: None,
                codex_home: None,
                claude_config_dir: Some(config_dir.clone()),
            };

            let (launch, _request) =
                derive_launch(&spec.launch, &input, &exe, crate::claude::run_launch)
                    .unwrap_or_else(|error| panic!("claude/{}: {error}", spec.name));

            assert_eq!(
                launch
                    .configured_env
                    .get(std::ffi::OsStr::new("CLAUDE_CONFIG_DIR")),
                Some(&std::ffi::OsString::from(&config_dir)),
                "claude/{}: CLAUDE_CONFIG_DIR missing from the launch environment",
                spec.name
            );
        }
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

    /// D91: the archive must be able to answer "was this capture isolated?" without knowing who
    /// ran it. That takes both halves — the manifest records the variable, and the redaction
    /// roots carry the directory so its path leaves as `<CLAUDE_CONFIG_DIR>` rather than as one
    /// machine's layout.
    ///
    /// Break caught, verified by falsification: recording the variable in `configured_env` but
    /// leaving it out of `capture_redaction_roots` — the manifest assertion passes and this one
    /// fails with the raw temp path, which is exactly the shape that would publish it.
    #[tokio::test]
    async fn recorder_records_an_isolated_claude_config_dir_and_redacts_its_path() {
        let raw = tempfile::tempdir().unwrap();
        let config_dir = tempfile::tempdir().unwrap();
        let mut cfg = config(
            "model-discovery",
            fixture_path("fake-claude"),
            "claude",
            raw.path(),
        );
        cfg.claude_config_dir = Some(config_dir.path().into());
        let capture = record(cfg).await.unwrap();

        assert_eq!(
            capture
                .command
                .configured_env
                .get("CLAUDE_CONFIG_DIR")
                .map(String::as_str),
            Some(config_dir.path().to_string_lossy().as_ref()),
            "the manifest must record which configuration home the CLI read"
        );
        assert_eq!(
            capture.redaction_roots.claude_config_dir.as_deref(),
            Some(config_dir.path().to_string_lossy().as_ref()),
            "without the redaction root the operator's path publishes verbatim"
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
    /// `PartialFailureClass::Timeout`, or the exit wait outliving the configured timeout. Does
    /// NOT catch a dropped `terminate_and_reap()` call — `Session`'s `Drop` impl kills and reaps
    /// the child regardless, so only `record/session.rs`'s
    /// `wait_error_retains_child_for_cleanup_and_quarantine` observes that. Drives a REAL timeout
    /// through `record()`, via a fixture that receives the request and never replies.
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

    /// Break caught: `record_generic` calling `P::handshake` unconditionally, putting an
    /// `initialize` line before a real Claude run's first line (which the product never sends —
    /// `crates/harness/src/claude/mod.rs`'s run driver). The scenario-level tests in
    /// `record/scenarios/claude.rs` construct a `Session` directly and never go through
    /// `record_generic`, so only driving the real entry point (`record()`) can catch this.
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

    /// The Codex counterpart: a real Codex run DOES handshake first, proven through `record()`
    /// itself that the `fresh-text` row's body calls `CodexProvider::handshake` before anything
    /// scenario-specific.
    ///
    /// Break caught, verified by falsification: removing the handshake call from
    /// `record/scenarios/codex.rs`'s `fresh_text` body fails loudly — `fake-codex` expects
    /// `initialize` first and errors "Codex stopped before the expected JSON-RPC reply."
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
    /// mutating the target's emptiness after building the fence must make the returned `recheck`
    /// fail, independent of whether `Session::start` remembers to call it (that wiring is
    /// `record/session.rs`'s own `start_runs_the_fence_recheck_after_directory_creation_and_before_spawn`).
    ///
    /// Break caught: `recheck` captures and replays the first check's `Ok` instead of re-running
    /// `validate_on_request_preflight` against live filesystem state.
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

    /// `None` is not a failure for the grant-time rechecks in `record/scenarios/codex.rs` —
    /// `validate_ordinary_approval_cwd`/`require_empty_approval_target` treat a missing identity
    /// as "nothing to compare against" and silently degrade to an emptiness/marker check (see
    /// `.agents/rules/optional-wire-fields.md`). Every existing scenario test hand-builds
    /// `FenceOutcome{ ..: Some(...) }` without calling `codex_fence`, so nothing else proves
    /// `codex_fence` itself populates the field the recheck depends on.
    ///
    /// Break caught: `codex_fence` returning `None` for `approval_target_identity` on
    /// `approval-on-request` — the pre-spawn fence still runs and succeeds, only the identity
    /// half of grant-time protection silently disappears.
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

    /// The hazard `ScenarioLaunch`/`derive_launch` close: a run scenario's launch and its wire
    /// line must come from exactly one call to the request builder, not two independent calls
    /// that happen to agree only because every real builder is a pure function of `input`.
    /// `counting_request` is deliberately impure (a call counter folded into `model` and
    /// `prompt`), so a second independent call is observable: the recorded argv (`--model
    /// call-N`) and the recorded wire line (`call-N`) must name the same call. Before
    /// `derive_launch` existed, this assertion failed with `--model call-0` vs `call-1`.
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

    /// Merges Task 2's per-row purity/wiring checks and D79's fence-wiring check into one loop
    /// over `SCENARIOS` against `EXPECTED_ROWS`, a `(provider, name) → (builder, fence)` table.
    /// Per row this checks: purity (`build_request` called twice must agree, `Run` rows only);
    /// run-builder wiring (the call also matches `EXPECTED_ROWS`' entry); fence wiring
    /// (`spec.fence` fingerprinted and compared against the table's `FenceKind`); Run/Discovery
    /// agreement (`EXPECTED_ROWS`' builder is `Some` iff the row is `Run`); and, after the loop,
    /// coverage (`EXPECTED_ROWS` lists every `SCENARIOS` row exactly once, both directions).
    ///
    /// `ScenarioInput` here is derived from `spec.requirements` rather than hardcoded per row, so
    /// a row whose `needs_resume_id`/`needs_attachment`/`needs_approval_target` flag disagrees
    /// with what its own builder demands fails here too.
    ///
    /// Fence fingerprint: comparing `spec.fence` by function-pointer identity
    /// (`std::ptr::fn_addr_eq`) does not work — distinct `fn` items are not guaranteed distinct
    /// addresses across codegen units. Instead this calls the fence with a `cwd: None` launch:
    /// `codex_fence`'s first statement in both branches fails with "requires a resolved working
    /// directory" before touching `spec.requirements`, `config`, or the filesystem; `no_fence`
    /// always returns `Ok`. That's deterministic and needs no real filesystem state, which is why
    /// it stays portable to Linux CI, where `resolve_trusted_powershell` fails closed regardless.
    ///
    /// The purity assertion only catches impurity that varies call-to-call in the same process (a
    /// counter, a fine clock) — NOT `normalize_run_request` reading `.git` state
    /// (`cheap_codex_request`'s own doc), which returns the same value on both calls whenever the
    /// filesystem is stable, i.e. always. That class of drift is instead caught by
    /// `every_scenario_launch_matches_its_committed_corpus_manifest`, the corpus pin.
    ///
    /// Does NOT independently prove `derive_launch`'s provider dispatch: `run_launch` is chosen
    /// here by `spec.provider`, but production dispatches by `spec.body`; that the two agree is
    /// pinned by `every_row_s_declared_provider_matches_its_body_variant` (`scenarios.rs`), not
    /// this test. And since `derive_launch`'s `Run` arm is exactly `run_launch(executable,
    /// &build_request(input)?)`, this assertion is a change-detector over that one function's
    /// body, not an independent oracle.
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
            (
                Provider::Acp,
                "session-discovery-codex-acp",
                None,
                FenceKind::None,
            ),
            (
                Provider::Acp,
                "session-discovery-claude-acp",
                None,
                FenceKind::None,
            ),
            (
                Provider::Acp,
                "session-discovery-grok",
                None,
                FenceKind::None,
            ),
            (
                Provider::Acp,
                "run-grok",
                Some(scenarios::acp::run_request),
                FenceKind::None,
            ),
            (
                Provider::Acp,
                "steer-grok",
                Some(scenarios::acp::steer_request),
                FenceKind::None,
            ),
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
                Provider::Acp => "acp",
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
                            // Every ACP row is discovery, so the run launch is
                            // never invoked; `every_acp_row_is_discovery` pins
                            // that rather than this arm hoping for it.
                            Provider::Acp => (
                                absolute_program("node"),
                                crate::capture::record::scenarios::acp::run_launch
                                    as fn(&Path, &RunRequest) -> LaunchDescriptor,
                            ),
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

    /// Independent oracle for the loop above (`every_row_s_builder_and_fence_match_its_declared_wiring`):
    /// that loop's checks are all derived from the same production code this test exists to
    /// check, but this one compares against the committed capture archive, frozen before this
    /// branch existed and protected by policy ("No byte under `crates/harness/tests/corpus/`
    /// changes").
    ///
    /// `claude/2.1.229/subagent` is a hand-sanitized capture with no matching `SCENARIOS` row and
    /// is never looked up (lookup is by row name, not directory listing).
    ///
    /// Comparison is STRUCTURAL, not byte-for-byte — the archive redacts `cwd`, `program`, and
    /// any resume/session id in argv (`docs/testing/provider-captures.md`):
    /// - `args`: exact equality after normalizing `--resume=<id>` to `--resume=<REDACTED>` on
    ///   both sides — every other token (flags, model id, effort, `--bare`) is unredacted and
    ///   compared literally.
    /// - `cwd`: presence only (the archive redacts the value to `<CWD>`).
    /// - `program`: final path component, `.exe` stripped (`program_stem`) — the archive redacts
    ///   everything before the binary name. A bare `is_empty()` check used to stand in here and
    ///   could never fail for any production change; this replaced it with a real assertion.
    /// - `configured_env`: key set only (a set value like `CODEX_HOME` redacts to `<CODEX_HOME>`).
    /// - `stdin`/`stdout`/`stderr`/`kill_on_drop`/`creation_flags` (Windows only): exact equality —
    ///   none carry machine- or session-specific data.
    #[test]
    fn every_scenario_launch_matches_its_committed_corpus_manifest() {
        // The two ACP adapter rows resolve their launch through
        // `adapter_entry`, which needs an npm global root that genuinely
        // holds the package -- real on this machine (the adapters are
        // installed here) and real nowhere in CI, which runs
        // `ubuntu-24.04` with neither adapter present
        // (`comet-provider-sanitize` CI job, PR #127, 2026-08-29:
        // "npm's global node_modules could not be located"). Promoting the
        // ACP captures gave this test its first ACP rows to check, and
        // turned a comparison that had always passed vacuously into one
        // that depends on the machine running it.
        //
        // Fixed the same way `codex_home` below keeps the Codex discovery
        // rows hermetic: supply a deterministic answer explicitly rather
        // than let production fall back to auto-discovering one from
        // whatever the real machine happens to have. `adapter_entry` reads
        // its root from `COMET_ACP_ADAPTER_ROOT`, not from `ScenarioInput`
        // the way `codex_home` does, so the equivalent here is a stub
        // directory this test creates itself and points that variable at,
        // unconditionally, on every machine -- not a variable the test
        // reads and branches on, which would just move the CI dependency
        // rather than remove it. The suffix-based argv comparison
        // (`normalize_argv` below) already treats everything before the
        // package name as environment-specific and never compares it, so
        // a synthetic root changes nothing about what this test actually
        // verifies: the real package name and `dist/index.js` layout still
        // have to match the committed manifest, on every machine, with no
        // skip anywhere.
        let adapter_root = tempfile::tempdir().expect("tempdir for stub ACP adapters");
        for package in [
            scenarios::acp::CODEX_ACP_PACKAGE,
            scenarios::acp::CLAUDE_ACP_PACKAGE,
        ] {
            let entry_dir = adapter_root.path().join(package).join("dist");
            std::fs::create_dir_all(&entry_dir)
                .unwrap_or_else(|error| panic!("stub adapter dir for {package}: {error}"));
            std::fs::write(
                entry_dir.join("index.js"),
                "// stub, for argv-shape comparison only\n",
            )
            .unwrap_or_else(|error| panic!("stub adapter entry for {package}: {error}"));
        }
        // SAFETY: single-threaded per nextest's one-process-per-test model
        // (`.config/nextest.toml`); no other test in this process reads or
        // writes this variable while this one runs.
        unsafe { std::env::set_var("COMET_ACP_ADAPTER_ROOT", adapter_root.path()) };

        // codex-acp and claude-agent-acp discovery are promoted in this same
        // change (evidence: `tests/corpus/{codex-acp,claude-agent-acp}/`).
        // Grok stays exempt: `comet-provider-sanitize` structurally REJECTS
        // every Grok capture today, `initialize` reply onward -- Grok's own
        // `_meta["x.ai/..."]` vendor-namespace keys contain a literal `.`,
        // which `validate_key`'s `AmbiguousObjectKey` check refuses
        // unconditionally, and that check's own doc comment says explicitly
        // this is "a design question about path encoding, not something to
        // escape past on the day it arrives." See D102.
        const EXEMPT_UNCAPTURED: &[(Provider, &str)] = &[
            (Provider::Acp, "session-discovery-grok"),
            (Provider::Acp, "run-grok"),
            (Provider::Acp, "steer-grok"),
        ];

        let root = crate::capture::corpus_root();
        let promoted = crate::capture::promoted_scenarios(&root)
            .unwrap_or_else(|error| panic!("{} could not be walked: {error}", root.display()));

        let mut failures = Vec::new();
        let mut unevidenced: Vec<String> = Vec::new();

        for spec in SCENARIOS {
            let provider_str = crate::capture::corpus_provider_name(spec.provider, spec.name);
            let label = format!("{provider_str}/{}", spec.name);

            // EVERY corpus directory for this scenario, across every version — not just the
            // first found. A `.find()` here would always bind to the oldest version
            // (`promoted_scenarios` sorts ascending) and silently miss a newer capture once a
            // scenario exists under two versions. The launch under test is version-independent,
            // so every version's manifest is a valid oracle and checking all is strictly stronger.
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

            let (exe, run_launch): (PathBuf, fn(&Path, &RunRequest) -> LaunchDescriptor) =
                match spec.provider {
                    Provider::Claude => (absolute_program("claude"), crate::claude::run_launch),
                    Provider::Codex => (absolute_program("codex"), crate::codex::run_launch),
                    Provider::Acp => (
                        absolute_program("node"),
                        crate::capture::record::scenarios::acp::run_launch
                            as fn(&Path, &RunRequest) -> LaunchDescriptor,
                    ),
                };

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

                // Derived per manifest, not once per row, because isolation is a capture-time
                // parameter like `cwd`: a row can hold both a pre-D91 ambient capture and a
                // later isolated one, and a single derived launch cannot match both key sets.
                // Reading `CLAUDE_CONFIG_DIR`'s presence from the manifest under test asks the
                // right question of each — "does production build the launch this capture
                // recorded" — and leaves "was it isolated at all" to the binary's own
                // requirement gate, which is where it can actually be enforced.
                //
                // `cwd` only needs to be present (comparison is presence-only, see this test's
                // own doc comment); the neutral-cwd discovery rows ignore it entirely in
                // production and always spawn from a temp directory regardless, so
                // `Some(temp_dir())` here is behaviourally identical to leaving it `None`.
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
                    // Every Codex discovery row's launch builder needs a codex_home or falls
                    // back to auto-discovering one from the real environment this test happens
                    // to run in — supplying one explicitly keeps the test hermetic regardless
                    // of what's installed on the machine running it.
                    codex_home: (spec.provider == Provider::Codex
                        && !spec.requirements.spends_tokens)
                        .then(|| std::env::temp_dir().join("comet-corpus-pin-codex-home")),
                    claude_config_dir: corpus_command
                        .configured_env
                        .contains_key("CLAUDE_CONFIG_DIR")
                        .then(|| std::env::temp_dir().join("comet-corpus-pin-claude-config")),
                };

                let (derived_launch, _request) =
                    derive_launch(&spec.launch, &input, &exe, run_launch)
                        .unwrap_or_else(|error| panic!("{label}: derive_launch failed: {error}"));
                let derived = CommandSnapshot::from_launch(&derived_launch);

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
            vec![
                "grok/run-grok".to_owned(),
                "grok/session-discovery-grok".to_owned(),
                "grok/steer-grok".to_owned(),
            ],
            "exactly the rows in EXEMPT_UNCAPTURED may land in unevidenced — a row losing \
             corpus evidence must update this assertion deliberately, not pass through silently. \
             Delete a row here once its capture is promoted."
        );

        assert!(
            failures.is_empty(),
            "{} row(s) disagree with their committed corpus manifest:\n\n{}",
            failures.len(),
            failures.join("\n\n")
        );

        // SAFETY: see the matching set_var above.
        unsafe { std::env::remove_var("COMET_ACP_ADAPTER_ROOT") };
    }

    /// The final path component of `program` with a trailing `.exe` stripped.
    ///
    /// Deliberately does NOT use `std::path::Path` — the corpus string always uses `\` (captured
    /// on Windows), and on a non-Windows host (this workspace's CI runs `ubuntu-24.04`) `Path`
    /// treats `\` as an ordinary character, not a separator, so `file_stem()` would return the
    /// whole redacted string unsplit. Splitting on both `/` and `\` by hand works on every host.
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
        // Any argv element containing `/` or `\`, for every row this
        // function compares (not only ACP's) -- collapses to its last THREE
        // path components, dropping everything before them. Written for ACP's
        // two adapter rows (codex-acp, claude-agent-acp), the first whose
        // argv element IS a path rather than a flag: `args[0]` is the
        // resolved absolute path to the adapter's own JS entry file, and the
        // corpus redacts its home-directory prefix to `<HOME>`. Comparing
        // that byte-for-byte against a freshly derived launch on THIS machine
        // can never agree -- same reason `program` is compared by stem alone
        // below, not full equality. Unlike `program`, one final component
        // (`index.js`) is identical for every npm package that follows this
        // convention and would compare two different adapters' entries as
        // equal, so this keeps three instead -- still insensitive to where
        // npm's global root sits on a given machine or OS, but distinguishing
        // the package itself.
        //
        // **The condition is "contains a separator", not "is this row's
        // adapter-entry argument"**, so it applies uniformly to any path-
        // shaped arg on any provider. No row today has one other than the two
        // above, but a future Codex or Claude flag carrying a real path (a
        // `--config-dir=/a/b/c/d`, say) would compare as `b/c/d`, silently
        // dropping everything before it rather than comparing the flag
        // verbatim the way every other argv token here still does.
        //
        // Split on both separators by hand, same reasoning as
        // `program_stem`'s own doc comment: the corpus string mixes `\`
        // (Windows path joins) and `/` (the npm scope separator inside
        // `@agentclientprotocol/codex-acp`) in the same string, and
        // `std::path::Path` treats `\` as an ordinary character on a
        // non-Windows host (this workspace's CI runs `ubuntu-24.04`).
        fn normalize_argv(args: &[String]) -> Vec<String> {
            fn path_suffix(raw: &str, components: usize) -> String {
                let mut parts: Vec<&str> = raw.rsplit(['/', '\\']).take(components).collect();
                parts.reverse();
                parts.join("/")
            }
            args.iter()
                .map(|arg| {
                    if arg.starts_with("--resume=") {
                        "--resume=<REDACTED>".to_owned()
                    } else if arg.contains('/') || arg.contains('\\') {
                        path_suffix(arg, 3)
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
