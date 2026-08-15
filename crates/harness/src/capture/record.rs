//! The provider-neutral capture recorder.
//!
//! `record()` is the entry point: it dispatches a [`CaptureConfig`] onto
//! either the new [`Session`]/[`CaptureProvider`] machinery (currently: the
//! four discovery scenarios) or, for everything not yet ported, the
//! still-live `capture::recording` module. Both halves compile and both are
//! exercised — the standing hazard while this split exists is a scenario
//! left behind in *both* places, which is why each porting task deletes the
//! arm it moves out of `recording.rs`, not merely copies it.

mod provider;
mod providers;
mod scenarios;
mod session;

use std::future::Future;
use std::path::Path;
use std::pin::Pin;

use provider::CaptureProvider;
use providers::claude::ClaudeProvider;
use providers::codex::CodexProvider;
use scenarios::ScenarioInput;
use session::{FenceOutcome, Session};

use crate::capture::types::{
    CaptureConfig, CaptureOperation, ClaudeCaptureOperation, CodexCaptureOperation,
    PartialFailureClass, Provider, RawCapture,
};
use crate::launch::LaunchDescriptor;

/// Record one explicitly selected provider scenario into ignored raw
/// storage.
pub async fn record(config: CaptureConfig) -> anyhow::Result<RawCapture> {
    let (provider_str, name, input) = match config.scenario.operation.clone() {
        CaptureOperation::Claude(ClaudeCaptureOperation::ModelDiscovery) => {
            ("claude", "model-discovery", ScenarioInput::default())
        }
        CaptureOperation::Claude(ClaudeCaptureOperation::ModelDiscoveryAt { cwd }) => (
            "claude",
            "model-discovery-project-cwd",
            ScenarioInput {
                cwd: Some(cwd),
                ..ScenarioInput::default()
            },
        ),
        CaptureOperation::Claude(ClaudeCaptureOperation::CommandDiscovery { cwd }) => (
            "claude",
            "command-discovery",
            ScenarioInput {
                cwd: Some(cwd),
                ..ScenarioInput::default()
            },
        ),
        CaptureOperation::Codex(CodexCaptureOperation::ModelDiscovery) => (
            "codex",
            "model-discovery",
            ScenarioInput {
                codex_home: config.codex_home.clone(),
                ..ScenarioInput::default()
            },
        ),
        CaptureOperation::Codex(CodexCaptureOperation::ModelDiscoveryAt { cwd }) => (
            "codex",
            "model-discovery-project-cwd",
            ScenarioInput {
                cwd: Some(cwd),
                codex_home: config.codex_home.clone(),
                ..ScenarioInput::default()
            },
        ),
        // Run scenarios: not yet ported (Tasks 2-6). `recording::record`
        // still owns their spawn, drive and finish end to end.
        _ => return crate::capture::recording::record(config).await,
    };
    // The canonical name picked above must be a real row: this is the table
    // `record()` actually dispatches through, not a name that merely looks
    // right in a match arm — see the amendment on `CaptureProvider`'s doc
    // comment for why `launch` moved here.
    let spec = scenarios::scenario(provider_str, name)
        .unwrap_or_else(|| panic!("{provider_str}/{name} must be registered in SCENARIOS"));
    match &spec.body {
        scenarios::ScenarioBody::Claude(body) => {
            record_claude(&config, spec.launch, input, *body).await
        }
        scenarios::ScenarioBody::Codex(body) => {
            record_codex(&config, spec.launch, input, *body).await
        }
    }
}

type LaunchFn = fn(&ScenarioInput, &Path) -> anyhow::Result<LaunchDescriptor>;

async fn record_claude(
    config: &CaptureConfig,
    launch: LaunchFn,
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
    let launch = launch(&input, &executable)?;
    record_generic(ClaudeProvider, config, launch, input, body).await
}

async fn record_codex(
    config: &CaptureConfig,
    launch: LaunchFn,
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
    let launch = launch(&input, &executable)?;
    record_generic(CodexProvider::new(), config, launch, input, body).await
}

/// A scenario body: given a spawned, hand-shaken session, drive it and
/// report whether the scenario completed.
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
/// Deliberately does NOT call `P::handshake` — see the amendment on
/// `CaptureProvider`'s doc comment ("the scenario body calls the handshake;
/// the recorder does not"). Whether a scenario handshakes at all is a
/// scenario decision: every discovery body and every Codex run body opens
/// with `P::handshake(session, input).await?` itself; a Claude run body
/// calls nothing, because a real Claude run sends no handshake and this
/// function calling one unconditionally would put a line on the tape the
/// product never sends.
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
) -> anyhow::Result<RawCapture> {
    let mut session = Session::start(provider, config, launch, FenceOutcome::none()).await?;
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
    use std::path::PathBuf;
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
        let mut cfg = config(
            "codex-model-discovery",
            fixture_path("fake-codex"),
            CaptureOperation::Codex(CodexCaptureOperation::ModelDiscovery),
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
            "claude-timeout",
            fixture_path("fake-claude-discovery-stall"),
            CaptureOperation::Claude(ClaudeCaptureOperation::ModelDiscovery),
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
        let error = record(config(
            "claude-driver-error",
            fixture_path("fake-claude-discovery-stall"),
            CaptureOperation::Claude(ClaudeCaptureOperation::CommandDiscovery {
                cwd: cwd.path().into(),
            }),
            raw.path(),
        ))
        .await
        .unwrap_err();

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
