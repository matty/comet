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
    AgentCommand, AgentEvent, ApprovalDecision, ApprovalRequest, HarnessAvailability,
    HarnessCapabilities, HarnessId, HarnessInstall, HarnessProbe, InstallMethod, ModelCatalog,
    RunRequest, UserInputAnswer, UserInputQuestion,
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
    /// The run asks permission and awaits a decision (blocks the agent). The
    /// host mints the request id and owns the lifecycle: an adapter that
    /// emitted its own request event would put a card in the doc under an id
    /// no resolver knows, and answering it would never unblock the run.
    pub request_approval:
        Box<dyn Fn(ApprovalRequest) -> oneshot::Receiver<ApprovalDecision> + Send + Sync>,
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
    /// Whether this harness is usable on this device right now, and which
    /// binary answered.
    ///
    /// One call for both, on purpose: the resolve that finds the path is the
    /// same resolve the version probe runs against, so returning them together
    /// is what makes the path shown provably the path probed. Two methods would
    /// resolve twice and could disagree between the calls.
    ///
    /// Defaults to available with no install: an in-process harness (the mock,
    /// and every test fixture) has no CLI to resolve, so there is nothing that
    /// could be missing and no path to name. Only the harnesses that spawn a
    /// real binary override this.
    ///
    /// Called off the hot path — the engine probes in the background at boot
    /// and caches the result, because this spawns a subprocess.
    async fn probe(&self) -> HarnessProbe {
        HarnessProbe::unresolved(HarnessAvailability::Available { version: None })
    }
    /// The model list, plus where it came from.
    ///
    /// Called from the picker's render path AND from titling
    /// (`comet_engine::titles`), so an implementation that spawns a
    /// subprocess must cache it — see [`discovery::DiscoveryCache`].
    async fn models(&self) -> Result<ModelCatalog, HarnessError>;
    /// The slash commands this harness offers **in `cwd`**, for the composer's
    /// `/` menu.
    ///
    /// Takes a directory because the answer depends on one: a provider
    /// discovers user and project skills from the working directory, so the
    /// same CLI answers differently per chat. An implementation that spawns
    /// must cache per directory — see [`discovery::CommandCache`].
    ///
    /// Defaults to an empty list, which is the honest answer for a harness with
    /// no command surface, and the answer Codex gives deliberately: its
    /// app-server does not parse slash commands at all, and its skills are
    /// invoked through a structured turn-input item instead (debt row D39).
    async fn commands(&self, _cwd: &str) -> Result<Vec<AgentCommand>, HarnessError> {
        Ok(Vec::new())
    }
    /// Drop any cached discovery answer so the next `models()` re-runs it.
    ///
    /// Defaulted to a no-op: an in-process harness has nothing to discover,
    /// and an adapter that has not grown discovery yet has nothing to clear.
    fn clear_discovery(&self) {}
    /// The kind of this attempt's discovery failure, if it failed and nobody
    /// has reported it yet. Answers at most once per attempt.
    ///
    /// Taking rather than peeking is what keeps one unreadable answer from
    /// reading as many: the cached failure survives the whole boot, so every
    /// later `models()` would otherwise re-report it.
    ///
    /// Defaulted to `None`: a harness with no discovery has no failure to
    /// report. Called by the engine AFTER `models()` returns, because that is
    /// what populates the cell.
    fn take_unreported_discovery_failure(&self) -> Option<discovery::DiscoveryFailure> {
        None
    }
    /// Run one (persistent) session; the stream ends with `AgentEvent::Done`.
    async fn run(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError>;
}

pub mod claude;
pub mod codex;
pub mod discovery;
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

/// A directory Comet knows how to look in, tagged with what finding a CLI
/// there *means*.
///
/// One tagged list serves both jobs on purpose. The place we search and the
/// label we report are the same entry, so they cannot drift apart — add a
/// lookup location without saying what it implies and it will not compile.
pub(crate) type KnownDir = (std::path::PathBuf, InstallMethod);

/// Resolve an installed CLI: `$override_var`, then our own PATH, then the
/// system's own PATH (the login-shell snapshot on unix, the persisted machine +
/// user environment on Windows — see [`shell_env`]), then `known_dirs` and the
/// Node version managers' bin dirs as a last resort. Each directory is probed
/// with every [`executable_names`] spelling.
///
/// Pass the list from [`all_known_dirs`], so the directories searched and the
/// directories classified are the same values.
///
/// The tags are discarded here: search order is PATH-first, and a
/// normally-installed CLI is therefore almost always found through PATH rather
/// than through the entry that describes it. Which is exactly why
/// classification is a separate pass over the *resolved* path instead of a
/// by-product of the search.
pub(crate) fn resolve_cli(
    override_var: &str,
    stem: &str,
    known_dirs: Vec<KnownDir>,
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
    dirs.extend(known_dirs.into_iter().map(|(dir, _)| dir));
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

/// A provider's own install locations plus the Node version managers' bin
/// dirs — the complete set of places Comet recognizes, in search order.
///
/// Built once per resolve and handed to both [`resolve_cli`] and
/// [`classify_install`], so "where we looked" and "what that location means"
/// are literally the same list.
pub(crate) fn all_known_dirs(install_dirs: Vec<KnownDir>) -> Vec<KnownDir> {
    let mut dirs = install_dirs;
    dirs.extend(node_version_manager_bins());
    dirs
}

/// What finding the CLI at `exe` says about how it was installed.
///
/// Derived, never asked: the classification is a lookup of the resolved
/// binary's parent directory in the same tagged catalogue `resolve_cli`
/// searches. Nothing is spawned and the binary itself is never opened — a
/// native `claude.exe` is ~300MB.
///
/// `override_is_set` is the caller's answer to "was `$override_var`
/// non-empty", and it is checked first because it mirrors `resolve_cli`'s own
/// control flow: that function returns the override path before it looks
/// anywhere else, so when the variable is set, the resolved binary *is* the
/// override, whatever directory it happens to sit in. Reporting the override
/// rather than the directory's usual meaning is the deliberate choice — it is
/// the fact that explains why this binary and not another one, and it is the
/// one case where "how was this installed" is genuinely unanswerable.
///
/// An unrecognized directory yields [`InstallMethod::Unknown`], which is a real
/// answer rather than a failure: a CLI on PATH in a bespoke location works
/// fine.
pub(crate) fn classify_install(
    exe: &std::path::Path,
    override_is_set: bool,
    known_dirs: &[KnownDir],
) -> InstallMethod {
    if override_is_set {
        return InstallMethod::Override;
    }
    let Some(parent) = exe.parent() else {
        return InstallMethod::Unknown;
    };
    let parent = dir_key(parent);
    known_dirs
        .iter()
        .find(|(dir, _)| dir_key(dir) == parent)
        .map(|(_, method)| *method)
        .unwrap_or(InstallMethod::Unknown)
}

/// The shared body of both real adapters' [`Harness::probe`]: resolve, record
/// where it landed, then ask it for a version.
///
/// The install is recorded *before* the version probe and survives the probe
/// failing. That ordering is the point of the whole shape — a CLI that resolved
/// and then crashed on `--version` is exactly when naming the binary is worth
/// most, and building the install only on success would drop it there.
pub(crate) async fn probe_installed_cli(
    resolved: Result<std::path::PathBuf, HarnessError>,
    stem: &str,
    override_var: &str,
    known_dirs: Vec<KnownDir>,
) -> HarnessProbe {
    let exe = match resolved {
        Ok(exe) => exe,
        Err(err) => {
            return HarnessProbe::unresolved(unavailable_from_resolve(&err, stem, override_var));
        }
    };
    let override_is_set = std::env::var_os(override_var).is_some_and(|v| !v.is_empty());
    let install = HarnessInstall {
        path: exe.display().to_string(),
        method: classify_install(&exe, override_is_set, &known_dirs),
    };
    HarnessProbe {
        availability: probe_cli_version(&exe).await,
        install: Some(install),
        // Filled by the per-provider update readers; a provider that publishes
        // no state leaves this `None` and simply renders one line less.
        update: None,
    }
}

/// Normalize a directory for comparison: drop any trailing separator, and on
/// Windows fold case and separator direction too, because the string that
/// reaches us from PATH need not match the one we composed from `%APPDATA%`
/// in either respect.
///
/// Deliberately textual rather than `canonicalize`: canonicalizing would touch
/// the filesystem for every candidate directory on a settings render, and on
/// Windows it rewrites to the `\\?\` verbatim form that `program_path` exists
/// to avoid.
fn dir_key(dir: &std::path::Path) -> String {
    let raw = dir.to_string_lossy();
    let trimmed = raw.trim_end_matches(['/', '\\']);
    if cfg!(windows) {
        trimmed.to_lowercase().replace('/', "\\")
    } else {
        trimmed.to_string()
    }
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

/// What the model is told when an approval ended without an answer. Comet copy:
/// it reaches the transcript through the model's own reply.
pub(crate) fn approval_unanswered_message() -> String {
    "The user did not answer this request, so it was not approved.".to_string()
}

/// Byte budget for a notice `summary` built from provider prose.
pub(crate) const NOTICE_SUMMARY_MAX: usize = 160;
/// Byte budget for a notice `detail` built from provider prose.
pub(crate) const NOTICE_DETAIL_MAX: usize = 480;

/// Cap unbounded provider prose at the harness boundary: truncate to
/// `max_bytes` on a char boundary and append an ellipsis. Irreversible for
/// anyone reading the doc later, which is the point — the doc is a
/// user-facing transcript, not a log. Call sites debug-log the full text
/// BEFORE capping.
pub(crate) fn cap_prose(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// The five places a provider frame becomes an `AgentEvent::Diagnostic`
/// instead of vanishing silently. Numbered here so the "Sink N" comments
/// scattered at each site resolve against something real — that numbering
/// has no meaning outside this repository (the planning document it was
/// drafted against is not shipped), so this doc comment is its only
/// in-repo definition:
///
/// 1. Claude, two call sites feeding one arm: an unclaimed top-level frame
///    `type`, or an unclaimed `system/<subtype>` — both classified by
///    `claude::wire::classify_unclaimed` into `Frame::Unknown`, emitted as
///    one diagnostic in `claude::normalize`.
/// 2. Codex: the notification catch-all — a JSON-RPC method on neither the
///    claimed nor the `codex::normalize::IGNORED_NOTIFICATIONS` list.
/// 3. Claude: an unclaimed inbound `control_request` subtype, handled (by
///    not answering it) in `claude::mod::handle_control_request`.
/// 4. Codex: an unclaimed item `type` inside an otherwise-claimed
///    notification, in `codex::normalize::map_item`.
/// 5. Parse failures on both sides — a stdout line that never decoded at
///    all. Always [`comet_proto::DiagnosticSeverity::Malformed`], always
///    this fixed sentinel; the raw line stays in `tracing` and never
///    travels with the event.
pub(crate) const UNPARSEABLE: &str = "unparseable";

/// Build the diagnostic event for a dropped frame. The caller has already
/// warn-logged the full frame at the drop site — this carries only the
/// sanitized name and Comet copy, never provider text (redaction is
/// structural: the payload is absent, not truncated).
pub(crate) fn diagnostic(
    discriminator: &str,
    severity: comet_proto::DiagnosticSeverity,
) -> AgentEvent {
    let summary = match severity {
        comet_proto::DiagnosticSeverity::Unknown => {
            "The agent sent a message Comet doesn't recognize."
        }
        comet_proto::DiagnosticSeverity::Malformed => {
            "The agent sent a message Comet couldn't read."
        }
    }
    .to_string();
    AgentEvent::Diagnostic {
        discriminator: comet_proto::sanitize_discriminator(discriminator),
        severity,
        code: None,
        summary,
    }
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
            HarnessAvailability::unavailable(summary, Some(hint))
        }
        // Anything else is a configured-but-broken install (a bad override
        // path, a permissions failure) — it has no install hint to offer, so
        // the error itself is the most useful sentence available.
        other => HarnessAvailability::unavailable("Not working", Some(other.to_string())),
    }
}

/// Bin directories where npm-installed CLIs land under Node version managers.
/// GUI launches never see these on PATH — the managers shape PATH in shell
/// init (fnm's per-shell multishells, nvm's shell function), which a
/// Dock/Finder-launched app never runs.
pub(crate) fn node_version_manager_bins() -> Vec<KnownDir> {
    use std::path::PathBuf;
    let home = home_dir();
    let mut dirs: Vec<KnownDir> = Vec::new();
    if cfg!(windows) {
        // Windows managers keep shims in fixed per-user dirs, and the version
        // dirs hold the shims directly (no `bin` subdir).
        dirs.extend(env_dir("APPDATA").map(|d| (d.join("npm"), InstallMethod::Npm)));
        for root in env_dir("FNM_DIR")
            .into_iter()
            .chain(env_dir("APPDATA").map(|d| d.join("fnm")))
        {
            dirs.push((root.join("aliases").join("default"), InstallMethod::Fnm));
        }
        dirs.extend(
            env_dir("LOCALAPPDATA").map(|d| (d.join("Volta").join("bin"), InstallMethod::Volta)),
        );
        dirs.extend(env_dir("LOCALAPPDATA").map(|d| (d.join("pnpm"), InstallMethod::Pnpm)));
        if let Some(home) = &home {
            dirs.push((home.join(".bun").join("bin"), InstallMethod::Bun));
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
        dirs.push((
            root.join("aliases").join("default").join("bin"),
            InstallMethod::Fnm,
        ));
    }
    if let Some(home) = &home {
        // volta / bun keep real shims in a fixed bin dir; pnpm has a global bin.
        dirs.push((home.join(".volta").join("bin"), InstallMethod::Volta));
        dirs.push((home.join(".bun").join("bin"), InstallMethod::Bun));
        dirs.push((home.join("Library").join("pnpm"), InstallMethod::Pnpm));
        dirs.push((
            home.join(".local").join("share").join("pnpm"),
            InstallMethod::Pnpm,
        ));
        // nvm: every installed version's bin, newest first.
        let nvm = home.join(".nvm").join("versions").join("node");
        if let Ok(entries) = std::fs::read_dir(&nvm) {
            let mut versions: Vec<KnownDir> = entries
                .flatten()
                .map(|e| (e.path().join("bin"), InstallMethod::Nvm))
                .collect();
            // By path only — every entry carries the same method, and sorting
            // the pair would demand an `Ord` on `InstallMethod` that means
            // nothing.
            versions.sort_by(|(a, _), (b, _)| a.cmp(b));
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
            HarnessAvailability::unavailable(
                "Not working",
                Some(if detail.is_empty() {
                    format!("`--version` failed ({status}).")
                } else {
                    format!("`--version` failed ({status}): {}", detail.trim())
                }),
            )
        }
        Ok(Err(err)) => {
            tracing::debug!(cli = %name, error = %err, "cli could not be started");
            HarnessAvailability::unavailable(
                "Not working",
                Some(format!("{name} could not be started: {err}")),
            )
        }
        Err(_) => HarnessAvailability::unavailable(
            "Not responding",
            Some(format!(
                "{name} did not answer `--version` within {}s.",
                PROBE_TIMEOUT.as_secs()
            )),
        ),
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
mod install_classification_tests {
    use super::*;
    use std::path::{Path, PathBuf};

    /// The Windows half of the capture
    /// (`captures/2026-08-11-agent-version-install-method.md`), as the tagged
    /// list the real `all_known_dirs` would produce on that machine. Hard-coded
    /// rather than built from the environment so the test asserts the same
    /// thing on every machine and on CI's Linux runner.
    fn windows_dirs() -> Vec<KnownDir> {
        vec![
            (
                PathBuf::from(r"C:\Users\coding\.local\bin"),
                InstallMethod::Native,
            ),
            (
                PathBuf::from(r"C:\Users\coding\AppData\Local\Programs\OpenAI\Codex\bin"),
                InstallMethod::Native,
            ),
            (
                PathBuf::from(r"C:\Users\coding\AppData\Roaming\npm"),
                InstallMethod::Npm,
            ),
            (
                PathBuf::from(r"C:\Users\coding\AppData\Local\Microsoft\WinGet\Links"),
                InstallMethod::Winget,
            ),
        ]
    }

    /// Fabricated, because this machine is Windows and cannot exercise them.
    /// Written against the two `*_install_dirs` functions, not observed.
    fn unix_dirs() -> Vec<KnownDir> {
        vec![
            (PathBuf::from("/Users/a/.local/bin"), InstallMethod::Native),
            (PathBuf::from("/opt/homebrew/bin"), InstallMethod::Homebrew),
            (PathBuf::from("/usr/local/bin"), InstallMethod::Unknown),
            (PathBuf::from("/Users/a/.volta/bin"), InstallMethod::Volta),
            (
                PathBuf::from("/Users/a/.nvm/versions/node/v22.3.0/bin"),
                InstallMethod::Nvm,
            ),
        ]
    }

    /// The three binaries the capture actually found on this machine, each
    /// classified from its real path. The npm row is the one that matters: it
    /// is a *different install of the same CLI*, a full minor behind the native
    /// one, and the only thing that distinguishes them on screen is this label
    /// plus the path beside it.
    ///
    /// Windows-only, and not for the reason the other `cfg(windows)` tests
    /// here are. A backslash path is only a *path* on Windows: on unix
    /// `Path::new(r"C:\a\b\claude.exe").parent()` is `""`, because nothing in
    /// the string is a separator — so off Windows this would assert `Unknown`
    /// for a reason that has nothing to do with the classifier. CI runs on
    /// Ubuntu and caught exactly that. The unix half of the same lookup is
    /// covered by `the_unix_locations_carry_their_documented_meanings`, whose
    /// forward-slash paths parse on both platforms.
    #[test]
    #[cfg(windows)]
    fn the_captured_windows_installs_classify_as_captured() {
        let dirs = windows_dirs();
        assert_eq!(
            classify_install(
                Path::new(r"C:\Users\coding\.local\bin\claude.exe"),
                false,
                &dirs
            ),
            InstallMethod::Native
        );
        assert_eq!(
            classify_install(
                Path::new(r"C:\Users\coding\AppData\Local\Programs\OpenAI\Codex\bin\codex.exe"),
                false,
                &dirs
            ),
            InstallMethod::Native
        );
        assert_eq!(
            classify_install(
                Path::new(r"C:\Users\coding\AppData\Roaming\npm\codex.cmd"),
                false,
                &dirs
            ),
            InstallMethod::Npm
        );
    }

    /// An override is reported as an override even when it points *into* a
    /// directory we would otherwise recognize. The variable is why this binary
    /// and not another one, which is the fact worth showing; and `resolve_cli`
    /// returns the override before consulting any directory, so any other
    /// answer would describe a lookup that never happened.
    #[test]
    fn an_override_outranks_the_directory_it_points_into() {
        assert_eq!(
            classify_install(
                Path::new(r"C:\Users\coding\.local\bin\claude.exe"),
                true,
                &windows_dirs()
            ),
            InstallMethod::Override
        );
    }

    /// Windows PATH entries arrive in whatever case and separator direction the
    /// user's environment wrote them, while our own list is composed from
    /// `%APPDATA%`. Comparing them raw would classify the *same* directory two
    /// different ways depending on which list found it.
    #[test]
    #[cfg(windows)]
    fn windows_matching_ignores_case_separator_and_trailing_slash() {
        let dirs = windows_dirs();
        for spelling in [
            r"c:\users\coding\appdata\roaming\npm\codex.cmd",
            r"C:/Users/coding/AppData/Roaming/npm/codex.cmd",
            r"C:\USERS\CODING\APPDATA\ROAMING\NPM\codex.cmd",
        ] {
            assert_eq!(
                classify_install(Path::new(spelling), false, &dirs),
                InstallMethod::Npm,
                "{spelling} did not match the npm dir"
            );
        }
        // A trailing separator on the catalogue side must not break the match
        // either — `%APPDATA%` itself can arrive with one.
        let trailing = vec![(
            PathBuf::from(r"C:\Users\coding\AppData\Roaming\npm\"),
            InstallMethod::Npm,
        )];
        assert_eq!(
            classify_install(
                Path::new(r"C:\Users\coding\AppData\Roaming\npm\codex.cmd"),
                false,
                &trailing
            ),
            InstallMethod::Npm,
            "a trailing separator on the catalogue side broke the match"
        );
    }

    /// Unix paths are case-SENSITIVE, so two directories differing only in case
    /// are two directories. Folding them the way Windows does would be a wrong
    /// answer, not a lenient one.
    #[test]
    #[cfg(not(windows))]
    fn unix_matching_is_case_sensitive() {
        let dirs = unix_dirs();
        assert_eq!(
            classify_install(Path::new("/opt/homebrew/bin/claude"), false, &dirs),
            InstallMethod::Homebrew
        );
        assert_eq!(
            classify_install(Path::new("/opt/Homebrew/bin/claude"), false, &dirs),
            InstallMethod::Unknown,
            "a differently-cased unix path is a different directory"
        );
    }

    /// The platform branches this machine cannot run, asserted against the
    /// lists they are written from. Fabricated paths — see `unix_dirs`.
    #[test]
    fn the_unix_locations_carry_their_documented_meanings() {
        let dirs = unix_dirs();
        for (path, expected) in [
            ("/Users/a/.local/bin/claude", InstallMethod::Native),
            ("/opt/homebrew/bin/codex", InstallMethod::Homebrew),
            ("/Users/a/.volta/bin/codex", InstallMethod::Volta),
            (
                "/Users/a/.nvm/versions/node/v22.3.0/bin/codex",
                InstallMethod::Nvm,
            ),
        ] {
            assert_eq!(
                classify_install(Path::new(path), false, &dirs),
                expected,
                "{path}"
            );
        }
    }

    /// `/usr/local/bin` is Intel Homebrew, a manual copy, and several
    /// installers' fallback at once. It is listed as a place to LOOK while
    /// staying `Unknown` as an answer, and the two are not the same statement —
    /// tagging it `Homebrew` would be a guess rendered as a fact.
    #[test]
    fn usr_local_bin_is_searched_without_being_attributed() {
        let dirs = unix_dirs();
        assert!(
            dirs.iter().any(|(d, _)| d == Path::new("/usr/local/bin")),
            "it must still be a searched location"
        );
        assert_eq!(
            classify_install(Path::new("/usr/local/bin/codex"), false, &dirs),
            InstallMethod::Unknown
        );
    }

    /// A CLI on PATH somewhere bespoke is a working install, not a broken one.
    /// The label says we do not recognize the location, and nothing else about
    /// the row changes.
    #[test]
    fn an_unrecognized_directory_is_unknown_not_a_failure() {
        // Forward slashes, which parse as a path on both platforms. A
        // backslash literal would answer `Unknown` off Windows by failing to
        // split into components at all — the right answer for the wrong
        // reason, which is no test.
        assert_eq!(
            classify_install(Path::new("/opt/tools/bin/claude"), false, &unix_dirs()),
            InstallMethod::Unknown
        );
    }

    /// A bare file name has no parent directory to look up. Reachable through
    /// the override, which is not required to be absolute.
    #[test]
    fn a_path_with_no_parent_directory_is_unknown() {
        assert_eq!(
            classify_install(Path::new("claude"), false, &windows_dirs()),
            InstallMethod::Unknown
        );
    }

    /// The catalogue handed to `classify_install` must be the same one
    /// `resolve_cli` searched, or a binary found through a version manager
    /// classifies as `Unknown`. `all_known_dirs` is what guarantees it, so
    /// assert it actually appends them.
    #[test]
    fn all_known_dirs_includes_the_version_managers() {
        // Every path this returns is derived from the environment, so one with
        // none of those variables set legitimately yields nothing and the
        // length assertion below would blame the append. Fail on the
        // precondition instead, naming the actual cause.
        assert!(
            !node_version_manager_bins().is_empty(),
            "no version-manager dir is derivable here \
             (HOME/USERPROFILE, FNM_DIR, APPDATA and LOCALAPPDATA all unset)"
        );
        let install_only = vec![(PathBuf::from("/tmp/example"), InstallMethod::Native)];
        let combined = all_known_dirs(install_only.clone());
        assert!(
            combined.len() > install_only.len(),
            "the version-manager dirs were not appended"
        );
        assert_eq!(
            combined[0], install_only[0],
            "the provider's own locations must stay first, matching search order"
        );
    }
}

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

    /// The claim the whole sibling design rests on: a CLI that resolved and
    /// then failed `--version` still reports WHICH binary failed.
    ///
    /// Hanging the path off `HarnessAvailability::Available` would lose it in
    /// exactly this case, which is the case where it is worth most — the hint
    /// deliberately does not name the binary (see the test below for why), so
    /// without this the user is told a provider is broken with no way to learn
    /// which of several installs was asked.
    #[cfg(windows)]
    #[tokio::test]
    async fn a_broken_cli_still_names_the_binary_that_failed() {
        let dir = tempfile::tempdir().unwrap();
        let shim = dir.path().join("broken-cli.cmd");
        std::fs::write(&shim, "@echo off\r\necho boom 1>&2\r\nexit /b 3\r\n").unwrap();

        let probe = probe_installed_cli(
            Ok(shim.clone()),
            "broken-cli",
            "NO_SUCH_OVERRIDE_VAR",
            vec![(dir.path().to_path_buf(), InstallMethod::Npm)],
        )
        .await;

        assert!(
            probe.availability.is_unavailable(),
            "the probe must still report the failure"
        );
        let install = probe
            .install
            .expect("a resolved-but-broken CLI must still report its path");
        assert_eq!(install.path, shim.display().to_string());
        // And it is classified from where it sits, not from whether it worked.
        assert_eq!(install.method, InstallMethod::Npm);
    }

    /// The other half: a CLI that never resolved has no path to report, and
    /// must not invent one.
    #[tokio::test]
    async fn an_unresolved_cli_reports_no_install_at_all() {
        let probe = probe_installed_cli(
            Err(HarnessError::NotInstalled("nope".into())),
            "ghost",
            "GHOST_EXECUTABLE",
            Vec::new(),
        )
        .await;
        assert_eq!(
            probe.availability.unavailable_summary(),
            Some("Not installed")
        );
        assert_eq!(probe.install, None);
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

    #[test]
    fn cap_prose_truncates_on_char_boundaries_with_ellipsis() {
        // Under the cap: unchanged, no ellipsis.
        assert_eq!(cap_prose("short", 160), "short");
        // Over the cap: truncated at a char boundary + ellipsis.
        let long = "a".repeat(200);
        let capped = cap_prose(&long, 160);
        assert_eq!(capped, format!("{}…", "a".repeat(160)));
        // A multibyte char straddling the cap is not split ("é" is 2 bytes:
        // 161 bytes of "é" content has no boundary at 160, so it backs off).
        let multi = "é".repeat(81); // 162 bytes
        let capped = cap_prose(&multi, 161);
        assert!(capped.ends_with('…'));
        assert!(capped.chars().all(|c| c == 'é' || c == '…'));
    }

    /// `Malformed` is the parse-failure severity (sink 5, the only producer
    /// anywhere in the slice): the fixed `UNPARSEABLE` sentinel travels as the
    /// discriminator, never the offending line, and the summary is Comet's
    /// own copy — distinct from the `Unknown` copy used by every other sink.
    #[test]
    fn malformed_diagnostic_carries_the_fixed_sentinel_and_its_own_copy() {
        let ev = diagnostic(UNPARSEABLE, comet_proto::DiagnosticSeverity::Malformed);
        assert_eq!(
            ev,
            AgentEvent::Diagnostic {
                discriminator: UNPARSEABLE.into(),
                severity: comet_proto::DiagnosticSeverity::Malformed,
                code: None,
                summary: "The agent sent a message Comet couldn't read.".into(),
            }
        );
    }
}
