//! comet — headed by default; `comet headless` runs the engine alone. Direct
//! remote administration is local-IPC-first and falls back to locked offline
//! configuration only when no engine owns the data directory.

mod daemon;
mod remote_cli;
mod update_cli;

use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "comet", about = "Multi-device controller for coding agents")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the engine without a UI (VPS / remote device mode).
    Headless,
    /// Show local engine, IPC, LAN listener, clients, and direct remotes.
    Status,
    /// Configure direct Comet-to-Comet LAN connections.
    Remote {
        #[command(subcommand)]
        command: remote_cli::RemoteCommand,
    },
    /// Manage `comet headless` as a background service (launchd / systemd --user).
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
    /// Check for a newer release and apply it (download → verify → swap →
    /// service restart). `--check` only reports (exits 1 when one is available).
    Update {
        #[arg(long)]
        check: bool,
    },
}

#[derive(Subcommand)]
enum DaemonCommand {
    /// Install, enable, and start the service (captures COMET_* env).
    Install,
    /// Stop and remove the service.
    Uninstall,
    /// Start the installed service.
    Start,
    /// Stop the service.
    Stop,
    /// Restart the service.
    Restart,
    /// Show the service manager's view of the daemon.
    Status,
}

/// mimalloc: system malloc (macOS libmalloc especially) never returns the
/// streaming churn's high-water pages, so transient allocation became
/// permanent RSS (docs/memory-plan.md §1).
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn home_to_initialize(
    existing_home: Option<OsString>,
    os_home: impl FnOnce() -> Option<PathBuf>,
) -> anyhow::Result<Option<PathBuf>> {
    if existing_home.as_ref().is_some_and(|home| !home.is_empty()) {
        return Ok(None);
    }

    os_home()
        .filter(|home| !home.as_os_str().is_empty())
        .map(Some)
        .context("could not determine the current user's home directory")
}

fn initialize_home() -> anyhow::Result<()> {
    let existing_home = std::env::var_os("HOME");
    if existing_home.as_ref().is_some_and(|home| !home.is_empty()) {
        return Ok(());
    }

    if existing_home.is_some() {
        // SAFETY: this runs as the first operation in main, before Comet starts
        // any threads. Removing an empty value lets Unix consult its user DB.
        unsafe { std::env::remove_var("HOME") };
    }

    if let Some(home) = home_to_initialize(existing_home, std::env::home_dir)? {
        // SAFETY: this runs as the first operation in main, before Comet starts
        // any threads or child processes.
        unsafe { std::env::set_var("HOME", home) };
    }
    Ok(())
}

fn main() -> anyhow::Result<()> {
    initialize_home()?;
    let cli = Cli::parse();
    // Everything logs to stdout: long-running modes at info, one-shot CLI
    // commands at warn (RUST_LOG overrides either).
    // loro's internal block-encode diagnostics log at info and flood
    // journald on every snapshot export — enough to fill a disk on a
    // long-running headless host. Quiet them by default (RUST_LOG still
    // overrides the whole filter).
    let default_filter = match &cli.command {
        None | Some(Command::Headless) => "info,loro_internal=warn,loro=warn",
        Some(_) => "warn",
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| default_filter.into()),
        )
        .init();

    match cli.command {
        Some(Command::Headless) => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(async {
                let engine = comet_engine::Engine::new(engine_config_from_env());
                engine.run().await
            })
        }
        Some(Command::Status) => {
            let runtime = tokio::runtime::Runtime::new()?;
            let config = engine_config_from_env();
            runtime.block_on(remote_cli::status(&config.data_dir, config.ipc_port))
        }
        Some(Command::Remote { command }) => {
            let runtime = tokio::runtime::Runtime::new()?;
            let config = engine_config_from_env();
            runtime.block_on(remote_cli::run(command, &config.data_dir, config.ipc_port))
        }
        Some(Command::Update { check }) => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(update_cli::update(
                &comet_update::releases_url_from_env(),
                check,
            ))
        }
        Some(Command::Daemon { command }) => match command {
            DaemonCommand::Install => daemon::install(&engine_config_from_env().data_dir),
            DaemonCommand::Uninstall => daemon::uninstall(),
            DaemonCommand::Start => daemon::start(),
            DaemonCommand::Stop => daemon::stop(),
            DaemonCommand::Restart => daemon::restart(),
            DaemonCommand::Status => daemon::status(),
        },
        None => {
            // Headed: the UI probes COMET_IPC_PORT and connects to a running
            // daemon, or embeds the engine in-process (ARCHITECTURE §1).
            comet_ui::run_app(comet_ui::UiConfig {
                data_dir: std::env::var_os("COMET_DATA_DIR")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(dirs_data_dir),
                ipc_port: std::env::var("COMET_IPC_PORT")
                    .ok()
                    .and_then(|p| p.parse().ok())
                    .unwrap_or(27654),
                releases_url: comet_update::releases_url_from_env(),
                default_harness: comet_ui::HarnessId::ClaudeCode,
            });
            Ok(())
        }
    }
}

/// The env-resolved engine configuration shared by the headed, headless, and
/// local-administration entry points.
fn engine_config_from_env() -> comet_engine::EngineConfig {
    comet_engine::EngineConfig {
        data_dir: std::env::var_os("COMET_DATA_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(dirs_data_dir),
        ipc_port: std::env::var("COMET_IPC_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(27654),
        default_harness: harness_from_env(),
        releases_url: comet_update::releases_url_from_env(),
    }
}

/// `COMET_HARNESS` (kebab-case id) picks the default harness for chats without a
/// config row — `mock` powers the e2e smoke; default `claude-code`.
fn harness_from_env() -> comet_engine::HarnessId {
    match std::env::var("COMET_HARNESS").as_deref().map(str::trim) {
        Ok("mock") => comet_engine::HarnessId::Mock,
        Ok("codex") => comet_engine::HarnessId::Codex,
        Ok("cursor") => comet_engine::HarnessId::Cursor,
        _ => comet_engine::HarnessId::ClaudeCode,
    }
}

fn dirs_data_dir() -> std::path::PathBuf {
    let home = std::env::var_os("HOME").expect("HOME not set");
    std::path::PathBuf::from(home).join(".comet-native")
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn configured_home_does_not_require_initialization() {
        let result = home_to_initialize(Some("configured-home".into()), || {
            panic!("OS lookup must not run when HOME is configured")
        })
        .unwrap();

        assert_eq!(result, None);
    }

    #[test]
    fn empty_home_uses_os_home() {
        let result =
            home_to_initialize(Some(OsString::new()), || Some(PathBuf::from("os-home"))).unwrap();

        assert_eq!(result, Some(PathBuf::from("os-home")));
    }

    #[test]
    fn missing_home_uses_os_home() {
        let result = home_to_initialize(None, || Some(PathBuf::from("os-home"))).unwrap();

        assert_eq!(result, Some(PathBuf::from("os-home")));
    }

    #[test]
    fn unresolved_home_returns_contextual_error() {
        let error = home_to_initialize(None, || None).unwrap_err();

        assert_eq!(
            error.to_string(),
            "could not determine the current user's home directory"
        );
    }

    #[test]
    fn login_and_logout_are_not_commands() {
        assert!(Cli::try_parse_from(["comet", "login"]).is_err());
        assert!(Cli::try_parse_from(["comet", "logout"]).is_err());
    }

    #[test]
    fn rejects_removed_tui_subcommand() {
        assert!(Cli::try_parse_from(["comet", "tui"]).is_err());
    }

    #[test]
    fn parses_manual_remote_endpoint() {
        let cli = Cli::try_parse_from(["comet", "remote", "add", "buildbox.local:27655"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Remote {
                command: remote_cli::RemoteCommand::Add { .. }
            })
        ));
    }

    #[test]
    fn migrate_is_not_a_command() {
        assert!(Cli::try_parse_from(["comet", "migrate", "--from", "org-a/user-a"]).is_err());
    }

    #[test]
    fn parses_server_side_pairing_session() {
        let cli = Cli::try_parse_from(["comet", "remote", "pair"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Remote {
                command: remote_cli::RemoteCommand::Pair
            })
        ));
    }
}
