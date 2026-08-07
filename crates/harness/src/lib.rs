//! comet-harness — one interface over Claude Code / Codex (and a mock for tests).
//!
//! Integration decisions (docs/research/harness.md):
//! - Claude Code: spawn the installed `claude` CLI with
//!   `--input-format stream-json --output-format stream-json --verbose
//!    --include-partial-messages`, implement the control channel (can_use_tool →
//!   requestInput, interrupt, set_model), steer by writing user lines mid-run.
//! - Codex: spawn `codex app-server`, JSON-RPC 2.0 over stdio (thread/start, turn/start,
//!   turn/steer{expectedTurnId}, turn/interrupt, item/* + delta notifications).

use async_trait::async_trait;
use futures::stream::BoxStream;
use tokio::sync::{mpsc, oneshot};
pub use tokio_util::sync::CancellationToken;

use comet_proto::{
    AgentEvent, HarnessAvailability, HarnessCapabilities, HarnessId, Model, RunRequest,
    UserInputAnswer, UserInputQuestion,
};

#[derive(Debug, thiserror::Error)]
pub enum HarnessError {
    // "Agent CLI", not "harness binary": this string reaches the models pane
    // verbatim, and "harness" is our internal word for something the UI calls
    // an Agent. Every constructor passes either a CLI name, a path, or a
    // sentence, so the prefix has to read as a lead-in to all three.
    #[error("Agent CLI not found: {0}")]
    NotInstalled(String),
    #[error("harness protocol error: {0}")]
    Protocol(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// A steer prompt pushed into a live run; delivered at the harness's steering boundary.
pub struct SteerMessage {
    pub prompt: String,
    pub message_id: Option<String>,
}

/// Host-side controls handed to a run: input-request bridge + steering mailbox.
pub struct RunControls {
    /// The run sends questions and awaits answers (blocks the agent, mirrors comet).
    pub request_input: Box<
        dyn Fn(Vec<UserInputQuestion>) -> oneshot::Receiver<Vec<UserInputAnswer>> + Send + Sync,
    >,
    /// Steer prompts consumed at step/turn boundaries.
    pub steering: mpsc::Receiver<SteerMessage>,
    /// Cancel to interrupt the live run: the harness sends its protocol-level
    /// interrupt, then escalates to SIGTERM/SIGKILL on the child after a grace
    /// period. The run's stream ends with `Done { status: Interrupted }`.
    pub interrupt: CancellationToken,
}

#[async_trait]
pub trait Harness: Send + Sync {
    fn id(&self) -> HarnessId;
    fn display_name(&self) -> &str;
    /// What this harness can honor. Each implementor delegates to its own
    /// associated `capabilities()` so the engine registry can name the same
    /// value without re-typing it — see [`comet_proto::HarnessCapabilities`].
    fn capabilities(&self) -> HarnessCapabilities;
    /// Whether this harness is usable on this device right now.
    ///
    /// Defaults to available: an in-process harness (the mock, and every test
    /// fixture) has no CLI to resolve, so there is nothing that could be
    /// missing. Only the harnesses that spawn a real binary override this.
    ///
    /// Called off the hot path — the engine probes in the background at boot
    /// and caches the result, because this spawns a subprocess.
    async fn availability(&self) -> HarnessAvailability {
        HarnessAvailability::Available { version: None }
    }
    async fn models(&self) -> Result<Vec<Model>, HarnessError>;
    /// Run one (persistent) session; the stream ends with `AgentEvent::Done`.
    async fn run(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError>;
}

pub mod claude;
pub mod codex;
pub mod mock;
pub mod shell_env;

/// The user's home directory. A Windows GUI or service launch routinely has no
/// `HOME` (Comet's own startup seeds it, but this crate must not depend on the
/// binary that links it), so `USERPROFILE` is the documented fallback.
pub(crate) fn home_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME")
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|s| !s.is_empty()))
        .map(std::path::PathBuf::from)
}

/// A directory from an environment variable, ignoring unset/empty values.
pub(crate) fn env_dir(var: &str) -> Option<std::path::PathBuf> {
    std::env::var_os(var)
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from)
}

/// The file names a CLI can have on this platform, most specific first. Unix
/// has one; Windows decides executability by EXTENSION, and the npm-installed
/// CLIs ship there as `.cmd` shims only — never `<name>` or `<name>.exe`.
/// (`.ps1` is deliberately absent: CreateProcess cannot run it.)
pub(crate) fn executable_names(stem: &str) -> Vec<String> {
    if cfg!(windows) {
        vec![
            format!("{stem}.exe"),
            format!("{stem}.cmd"),
            format!("{stem}.bat"),
        ]
    } else {
        vec![stem.to_owned()]
    }
}

/// Resolve an installed CLI: `$override_var`, then our own PATH, then the
/// system's own PATH (the login-shell snapshot on unix, the persisted machine +
/// user environment on Windows — see [`shell_env`]), then `known_dirs` and the
/// Node version managers' bin dirs as a last resort. Each directory is probed
/// with every [`executable_names`] spelling.
pub(crate) fn resolve_cli(
    override_var: &str,
    stem: &str,
    known_dirs: Vec<std::path::PathBuf>,
) -> Option<std::path::PathBuf> {
    if let Some(p) = std::env::var_os(override_var).filter(|p| !p.is_empty()) {
        return Some(std::path::PathBuf::from(p));
    }
    let mut dirs: Vec<std::path::PathBuf> = Vec::new();
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path));
    }
    if let Some(system_path) = shell_env::system_path() {
        dirs.extend(std::env::split_paths(system_path));
    }
    dirs.extend(known_dirs);
    dirs.extend(node_version_manager_bins());
    let names = executable_names(stem);
    dirs.into_iter()
        .filter(|d| !d.as_os_str().is_empty())
        .flat_map(|dir| {
            names
                .iter()
                .map(|name| dir.join(name))
                .collect::<Vec<std::path::PathBuf>>()
        })
        // `is_file` rather than `exists`: a DIRECTORY named `codex` on PATH
        // would otherwise resolve and then fail at spawn.
        .find(|p| p.is_file())
}

/// Every place [`find_on_path`] and its callers look for a CLI. Diagnostic
/// detail: it names ten locations and differs per platform, which is useful in
/// a log and unreadable in a picker row.
///
/// This used to be concatenated into the user-facing message, ahead of the
/// override hint. That put the only actionable clause last, behind ~180
/// characters of inventory — so it was both the hardest part to reach and the
/// first part any truncation dropped.
pub(crate) fn searched_locations() -> &'static str {
    if cfg!(windows) {
        "PATH, the persisted machine/user PATH, %LOCALAPPDATA%\\Programs, \
         %APPDATA%\\npm, the WinGet/scoop shim dirs, and volta/fnm/pnpm/bun \
         install dirs"
    } else {
        "PATH, the login shell's PATH, ~/.local/bin, the CLI's own install dir, \
         /opt/homebrew/bin, /usr/local/bin, and fnm/nvm/volta/pnpm/bun install dirs"
    }
}

/// The user-facing halves of "this CLI could not be found": a row label and the
/// one sentence that says what to do. Logs the searched locations as a side
/// effect, which is the only place that inventory is now reachable.
pub(crate) fn not_installed(stem: &str, override_var: &str) -> (String, String) {
    tracing::debug!(
        cli = stem,
        searched = searched_locations(),
        "cli did not resolve"
    );
    (
        "Not installed".to_string(),
        format!("Install {stem}, or set {override_var} to its path."),
    )
}

/// The same failure as one line, for [`HarnessError::NotInstalled`] — an error
/// that surfaces without a row to label it and so has to name the CLI itself.
///
/// Says only the CLI name and the fix: the error's own Display already supplies
/// "not found", and stating it twice is how this read before
/// ("Agent CLI not found: codex isn't installed…").
pub(crate) fn not_installed_message(stem: &str, override_var: &str) -> String {
    let (_, hint) = not_installed(stem, override_var);
    format!("{stem}. {hint}")
}

/// Availability for a CLI that never resolved far enough to be probed.
///
/// Kept out of the adapters so both name the same summaries: the picker groups
/// rows by that label, and two adapters inventing their own wording for the
/// same failure is exactly the drift this collapses.
pub(crate) fn unavailable_from_resolve(
    err: &HarnessError,
    stem: &str,
    override_var: &str,
) -> HarnessAvailability {
    match err {
        HarnessError::NotInstalled(_) => {
            let (summary, hint) = not_installed(stem, override_var);
            HarnessAvailability::Unavailable {
                summary,
                hint: Some(hint),
            }
        }
        // Anything else is a configured-but-broken install (a bad override
        // path, a permissions failure) — it has no install hint to offer, so
        // the error itself is the most useful sentence available.
        other => HarnessAvailability::Unavailable {
            summary: "Not working".to_string(),
            hint: Some(other.to_string()),
        },
    }
}

/// Bin directories where npm-installed CLIs land under Node version managers.
/// GUI launches never see these on PATH — the managers shape PATH in shell
/// init (fnm's per-shell multishells, nvm's shell function), which a
/// Dock/Finder-launched app never runs.
pub(crate) fn node_version_manager_bins() -> Vec<std::path::PathBuf> {
    use std::path::PathBuf;
    let home = home_dir();
    let mut dirs: Vec<PathBuf> = Vec::new();
    if cfg!(windows) {
        // Windows managers keep shims in fixed per-user dirs, and the version
        // dirs hold the shims directly (no `bin` subdir).
        dirs.extend(env_dir("APPDATA").map(|d| d.join("npm")));
        for root in env_dir("FNM_DIR")
            .into_iter()
            .chain(env_dir("APPDATA").map(|d| d.join("fnm")))
        {
            dirs.push(root.join("aliases").join("default"));
        }
        dirs.extend(env_dir("LOCALAPPDATA").map(|d| d.join("Volta").join("bin")));
        dirs.extend(env_dir("LOCALAPPDATA").map(|d| d.join("pnpm")));
        if let Some(home) = &home {
            dirs.push(home.join(".bun").join("bin"));
        }
        return dirs;
    }
    // fnm: `aliases/default` is a stable symlink to the active default
    // installation (the multishell PATH entries are ephemeral, per-shell).
    let mut fnm_roots: Vec<PathBuf> = std::env::var_os("FNM_DIR")
        .map(PathBuf::from)
        .into_iter()
        .collect();
    if let Some(home) = &home {
        fnm_roots.push(home.join(".local").join("share").join("fnm"));
        fnm_roots.push(home.join("Library").join("Application Support").join("fnm"));
        fnm_roots.push(home.join(".fnm"));
    }
    for root in fnm_roots {
        dirs.push(root.join("aliases").join("default").join("bin"));
    }
    if let Some(home) = &home {
        // volta / bun keep real shims in a fixed bin dir; pnpm has a global bin.
        dirs.push(home.join(".volta").join("bin"));
        dirs.push(home.join(".bun").join("bin"));
        dirs.push(home.join("Library").join("pnpm"));
        dirs.push(home.join(".local").join("share").join("pnpm"));
        // nvm: every installed version's bin, newest first.
        let nvm = home.join(".nvm").join("versions").join("node");
        if let Ok(entries) = std::fs::read_dir(&nvm) {
            let mut versions: Vec<PathBuf> =
                entries.flatten().map(|e| e.path().join("bin")).collect();
            versions.sort();
            versions.reverse();
            dirs.append(&mut versions);
        }
    }
    dirs
}

/// Compose the child's PATH: the resolved executable's directory first, then
/// our own PATH, then the login-shell PATH snapshot — deduped. npm-shim CLIs
/// are `#!/usr/bin/env node` scripts whose `node` lives beside them in the
/// version manager's bin dir, and the CLIs themselves shell out to tools
/// (git, rg, node) that a GUI/service launch's own PATH may lack.
pub(crate) fn compose_child_path(cmd: &mut tokio::process::Command, exe: &std::path::Path) {
    let mut paths: Vec<std::path::PathBuf> = Vec::new();
    if let Some(dir) = exe.parent().filter(|d| !d.as_os_str().is_empty()) {
        paths.push(dir.to_path_buf());
    }
    if let Some(path) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&path));
    }
    if let Some(system_path) = shell_env::system_path() {
        paths.extend(std::env::split_paths(system_path));
    }
    let mut seen = std::collections::HashSet::new();
    paths.retain(|p| !p.as_os_str().is_empty() && seen.insert(p.clone()));
    if let Ok(joined) = std::env::join_paths(paths) {
        cmd.env("PATH", joined);
    }
}

/// How long a `--version` probe may run before the CLI is called unusable. A
/// hung probe must not keep a harness in `Unknown` forever, but the bound is
/// generous: the npm shims start a Node runtime, and a cold first run on
/// Windows also pays Defender's scan of the shim.
pub(crate) const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Ask an already-resolved CLI for its version.
///
/// Every failure mode collapses to `Unavailable` with prose naming the binary,
/// because the caller renders this string verbatim and "something went wrong"
/// is not actionable. Success with an unreadable version is still `Available`
/// — the CLI answered, which is the question being asked.
pub(crate) async fn probe_cli_version(exe: &std::path::Path) -> HarnessAvailability {
    let mut cmd = tokio::process::Command::new(exe);
    compose_child_path(&mut cmd, exe);
    cmd.arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // The timeout arm drops the future; without this the child outlives
        // the probe and we leak a process per boot.
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        // CREATE_NO_WINDOW: the `.cmd` shims are console apps, so without this
        // every boot flashes a console window for a probe the user never asked
        // to see. `tokio::process::Command` exposes this directly on Windows.
        cmd.creation_flags(0x0800_0000);
    }
    let name = exe.display();
    match tokio::time::timeout(PROBE_TIMEOUT, cmd.output()).await {
        Ok(Ok(output)) if output.status.success() => HarnessAvailability::Available {
            version: parse_cli_version(&String::from_utf8_lossy(&output.stdout)),
        },
        // A resolved-but-broken CLI keeps its own stderr: "not installed" would
        // be actively misleading, and the stderr line is usually the only thing
        // that explains a half-finished install. It goes in the hint, where the
        // full text is reachable, while the summary stays row-sized.
        Ok(Ok(output)) => {
            let status = describe_exit(Some(output.status));
            let stderr = String::from_utf8_lossy(&output.stderr);
            let detail = stderr.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
            tracing::debug!(cli = %name, %status, detail, "cli --version failed");
            HarnessAvailability::Unavailable {
                summary: "Not working".to_string(),
                hint: Some(if detail.is_empty() {
                    format!("`--version` failed ({status}).")
                } else {
                    format!("`--version` failed ({status}): {}", detail.trim())
                }),
            }
        }
        Ok(Err(err)) => {
            tracing::debug!(cli = %name, error = %err, "cli could not be started");
            HarnessAvailability::Unavailable {
                summary: "Not working".to_string(),
                hint: Some(format!("{name} could not be started: {err}")),
            }
        }
        Err(_) => HarnessAvailability::Unavailable {
            summary: "Not responding".to_string(),
            hint: Some(format!(
                "{name} did not answer `--version` within {}s.",
                PROBE_TIMEOUT.as_secs()
            )),
        },
    }
}

/// Pull a version out of `--version` output.
///
/// The CLIs disagree on shape — Claude prints `1.0.30 (Claude Code)`, Codex
/// prints `codex-cli 0.20.0` — so take the first dotted-numeric token rather
/// than a fixed position. An unrecognized line yields `None`, which still
/// reads as available; the version is a nicety, the answer is the signal.
pub(crate) fn parse_cli_version(output: &str) -> Option<String> {
    let line = output.lines().find(|l| !l.trim().is_empty())?.trim();
    line.split_whitespace()
        .map(|token| token.trim_matches(|c: char| !c.is_ascii_alphanumeric()))
        // A leading `v` is a conventional prefix, not part of the version.
        .map(|token| match token.strip_prefix(['v', 'V']) {
            Some(rest) if rest.starts_with(|c: char| c.is_ascii_digit()) => rest,
            _ => token,
        })
        .find(|token| token.contains('.') && token.starts_with(|c: char| c.is_ascii_digit()))
        .map(str::to_owned)
}

/// Rolling tail of a child's stderr, shared between the reader task and the
/// crash-message composer: an unexpected exit surfaces "<name> exited
/// unexpectedly (<status>): <last stderr lines>" instead of a bare shrug —
/// the proper background-crash message old comet showed (user requirement).
#[derive(Clone, Default)]
pub(crate) struct StderrTail(std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<String>>>);

impl StderrTail {
    const KEEP_LINES: usize = 6;
    const KEEP_BYTES: usize = 700;

    pub(crate) fn push(&self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        let mut tail = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        tail.push_back(line.chars().take(Self::KEEP_BYTES).collect());
        while tail.len() > Self::KEEP_LINES {
            tail.pop_front();
        }
    }

    /// The captured tail as one display string, `None` when nothing arrived.
    pub(crate) fn snapshot(&self) -> Option<String> {
        let tail = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if tail.is_empty() {
            return None;
        }
        let mut joined = tail.iter().cloned().collect::<Vec<_>>().join("\n");
        joined.truncate(Self::KEEP_BYTES * 2);
        Some(joined)
    }
}

/// "exit code 137" / "signal 9 (killed)" / "unknown" — the status half of a
/// crash message, from a `try_wait` result after the stream ended.
pub(crate) fn describe_exit(status: Option<std::process::ExitStatus>) -> String {
    let Some(status) = status else {
        return "still running".into();
    };
    if let Some(code) = status.code() {
        return format!("exit code {code}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return format!("killed by signal {signal}");
        }
    }
    "unknown exit".into()
}

/// The full crash message: status plus the stderr tail when there is one.
pub(crate) fn crash_message(
    name: &str,
    status: Option<std::process::ExitStatus>,
    stderr: &StderrTail,
) -> String {
    let status = describe_exit(status);
    match stderr.snapshot() {
        Some(tail) => format!("{name} exited unexpectedly ({status}): {tail}"),
        None => format!("{name} exited unexpectedly ({status})"),
    }
}

/// How hard to hit a child during interrupt escalation.
#[derive(Clone, Copy)]
pub(crate) enum Signal {
    /// Wind down: on unix SIGTERM, which also tears down bash trees and lets
    /// the CLI run its SessionEnd hooks. Windows has no equivalent for a
    /// piped, console-less child, so escalation there is kill-only.
    Term,
    /// Last resort: SIGKILL / `TerminateProcess`.
    Kill,
}

/// Signal a child by pid. Safe against pid reuse only while the caller still
/// holds the unreaped `Child` — every call site does.
#[cfg(unix)]
pub(crate) fn send_signal(pid: u32, signal: Signal) {
    let sig = match signal {
        Signal::Term => libc::SIGTERM,
        Signal::Kill => libc::SIGKILL,
    };
    // SAFETY: plain kill(2) on a pid we spawned and have not yet reaped.
    unsafe {
        libc::kill(pid as libc::pid_t, sig);
    }
}

/// Windows has no graceful signal to send a piped child, so `Term` is a no-op
/// and `Kill` is `TerminateProcess`. Without this the escalation task was
/// inert off unix: an unresponsive CLI was never reaped, and the run loop —
/// which only ends on stdout EOF — hung until the child chose to exit.
///
/// Only the process itself dies, not the tree it spawned; that is the same
/// caveat `start_kill`/`kill_on_drop` already carry.
#[cfg(windows)]
pub(crate) fn send_signal(pid: u32, signal: Signal) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};

    if matches!(signal, Signal::Term) {
        return;
    }
    // SAFETY: the caller still owns the unreaped child, so Windows cannot have
    // recycled the pid; a failed open yields a null handle we simply skip.
    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if !handle.is_null() {
            TerminateProcess(handle, 1);
            CloseHandle(handle);
        }
    }
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn send_signal(_pid: u32, _signal: Signal) {
    // `start_kill`/`kill_on_drop` are the only lever on other platforms.
}

pub use claude::{ClaudeHarness, resolve_claude_executable};
pub use codex::{CodexHarness, resolve_codex_executable};

#[cfg(test)]
mod probe_tests {
    use super::*;

    /// The two CLIs put the version in different positions — Claude leads with
    /// it, Codex leads with a name containing no dot — so the parse is
    /// positional-agnostic. Both strings are real `--version` output, captured
    /// from the installed CLIs rather than invented.
    #[test]
    fn version_is_read_from_either_cli_shape() {
        assert_eq!(
            parse_cli_version("2.1.224 (Claude Code)\n").as_deref(),
            Some("2.1.224")
        );
        assert_eq!(
            parse_cli_version("codex-cli 0.146.0\n").as_deref(),
            Some("0.146.0")
        );
        // Leading blank lines and a `v` prefix both survive.
        assert_eq!(parse_cli_version("\n\nv2.1.4\n").as_deref(), Some("2.1.4"));
    }

    /// No version is not a failure — the CLI answered, which is the question.
    #[test]
    fn unreadable_version_is_none_rather_than_an_error() {
        assert_eq!(parse_cli_version(""), None);
        assert_eq!(parse_cli_version("   \n  \n"), None);
        assert_eq!(parse_cli_version("no version here\n"), None);
        // A bare integer is not a version; requiring the dot avoids reporting
        // "2" from prose like "codex 2 beta".
        assert_eq!(parse_cli_version("codex 2 beta\n"), None);
    }

    /// Windows resolves these CLIs to `.cmd` shims when they come from npm —
    /// [`executable_names`] exists precisely because that is the only spelling
    /// present. `CreateProcess` handling of batch files is a real trip hazard
    /// on this repo's primary dev platform, so pin that a shim is probeable
    /// rather than assuming it behaves like an `.exe`.
    #[cfg(windows)]
    #[tokio::test]
    async fn a_windows_cmd_shim_is_probeable() {
        let dir = tempfile::tempdir().unwrap();
        let shim = dir.path().join("fake-cli.cmd");
        std::fs::write(&shim, "@echo off\r\necho fake-cli 4.5.6\r\n").unwrap();

        let availability = probe_cli_version(&shim).await;
        assert_eq!(
            availability,
            HarnessAvailability::Available {
                version: Some("4.5.6".into())
            },
            "a .cmd shim must probe like any other executable"
        );
    }

    /// A CLI that resolves but fails carries its own stderr into the hint —
    /// "Not installed" would be actively misleading for a broken install.
    ///
    /// The hint deliberately does NOT repeat the binary path: this string is
    /// rendered against a row that already names the agent, and re-stating a
    /// full Windows path there is what made the old single-string reason too
    /// long to read.
    #[cfg(windows)]
    #[tokio::test]
    async fn a_failing_cli_reports_its_own_error() {
        let dir = tempfile::tempdir().unwrap();
        let shim = dir.path().join("broken-cli.cmd");
        std::fs::write(&shim, "@echo off\r\necho boom 1>&2\r\nexit /b 3\r\n").unwrap();

        let availability = probe_cli_version(&shim).await;
        assert_eq!(
            availability.unavailable_summary(),
            Some("Not working"),
            "a broken install must not claim to be missing"
        );
        let hint = availability
            .unavailable_hint()
            .expect("a non-zero exit has something to say");
        assert!(hint.contains("exit code 3"), "must carry status: {hint}");
        assert!(hint.contains("boom"), "must carry stderr: {hint}");
    }

    /// A binary that does not exist names itself in the hint — this is the
    /// bad-override case, where *which* path failed is the whole diagnosis.
    #[tokio::test]
    async fn a_missing_binary_probes_as_unavailable() {
        let missing = std::path::Path::new("comet-definitely-not-a-real-cli");
        let availability = probe_cli_version(missing).await;
        assert_eq!(availability.unavailable_summary(), Some("Not working"));
        let hint = availability
            .unavailable_hint()
            .expect("a missing binary is unavailable");
        assert!(
            hint.contains("comet-definitely-not-a-real-cli"),
            "hint must name the binary it could not start: {hint}"
        );
    }

    /// The summary is a ROW LABEL: the rail is 148px, so anything long enough
    /// to truncate there defeats the caption and sends the user back to hover.
    /// Every summary this crate can produce is checked, not just a sample.
    #[test]
    fn every_summary_is_short_enough_to_render_in_the_rail() {
        let (not_installed_summary, hint) = not_installed("codex", "CODEX_EXECUTABLE");
        for summary in [
            not_installed_summary.as_str(),
            "Not working",
            "Not responding",
        ] {
            assert!(
                summary.len() <= 16,
                "summary must fit the rail caption: {summary:?}"
            );
            assert!(
                !summary.contains("searched"),
                "the searched-location inventory belongs in the log: {summary:?}"
            );
        }
        // The hint is the actionable half, and the override variable is the
        // action — burying it behind an inventory is the bug being fixed.
        assert!(
            hint.contains("CODEX_EXECUTABLE"),
            "hint must name the override: {hint}"
        );
        assert!(
            !hint.contains("searched"),
            "hint must not carry the inventory: {hint}"
        );
    }
}
