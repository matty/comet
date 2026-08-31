//! comet-harness — one interface over Claude Code / Codex (and a mock for tests).
//!
//! Integration decisions (docs/research/harness.md):
//! - Claude Code: spawn the installed `claude` CLI with
//!   `--input-format stream-json --output-format stream-json --verbose
//!    --include-partial-messages`, implement the control channel (can_use_tool →
//!   requestInput, interrupt, set_model), steer by writing user lines mid-run.
//! - Codex: spawn `codex app-server`, JSON-RPC 2.0 over stdio (thread/start, turn/start,
//!   turn/steer{expectedTurnId}, turn/interrupt, item/* + delta notifications).

use std::time::Duration;

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
    /// The CLI answered the handshake but refused to open a session because
    /// it is not set up yet — signed out (Grok) or no provider configured
    /// (Hermes). `NotInstalled`-adjacent, not `Protocol`: the agent IS
    /// installed and DID answer, so "Agent CLI not found" would be wrong, and
    /// `Protocol`'s raw JSON-RPC text is exactly what
    /// `.agents/rules/user-facing-errors.md` forbids reaching the user
    /// (`err.to_string()` is what `drive_run` shows verbatim in
    /// `crates/engine/src/sessions.rs`, the same way `NotInstalled`'s Display
    /// already does — see that variant's own comment).
    ///
    /// Two fields rather than one baked string, matching
    /// `HarnessAvailability::Unavailable` (that type's own doc comment is the
    /// worked example the error-copy rule points at). `Display` joins them
    /// back into one line today (`drive_run`'s sink only ever wanted a
    /// single string), but the split is forward-looking, not decorative: a
    /// surface with somewhere to put a label AND a longer sentence
    /// separately -- the harness rail row `HarnessAvailability::Unavailable`
    /// already renders that way -- can render `summary`/`hint` apart the
    /// moment `NeedsSetup` needs to reach one, without a field ever having
    /// to be parsed back out of a joined string first.
    ///
    /// Built by a vendor module's own `map_open_failure` (see
    /// `acp::session::OpenFailureMapper`) for the wire-level cases — this crate
    /// does not know what "not set up yet" looks like on any agent's protocol —
    /// and by [`spawn_failure`] for the two that need no wire at all: a binary
    /// this account cannot execute, and one that will not start. Both answer
    /// the same user-facing question as a signed-out agent does, which is the
    /// test `hermes::map_open_failure`'s doc comment sets for reusing this
    /// variant rather than adding another.
    #[error("{summary}. {hint}")]
    NeedsSetup { summary: String, hint: String },
    /// The agent opened a session and then refused a SETTING sent with it —
    /// today only `session/set_model`, from `acp::session::AcpSession::open`'s
    /// `config_requests` loop.
    ///
    /// **A separate variant from `NeedsSetup`, because the user's next action
    /// is different.** `NeedsSetup` means "go do one more thing in the agent's
    /// own CLI"; this means "pick something else in the picker". The two would
    /// otherwise share a variant and differ only in wording, which is the test
    /// `hermes::map_open_failure`'s doc comment applies when it declines to
    /// add one.
    ///
    /// Its reason for existing at all is D119: that loop returned the
    /// provider's own JSON-RPC message verbatim (`HarnessError::Protocol`,
    /// built by `RpcClient::request`), and `drive_run` renders it to the user
    /// close to as-is — Grok answers `session/set_model: Invalid params:
    /// unknown model id`. `.agents/rules/user-facing-errors.md` RULE 1 forbids
    /// exactly that. The raw text is warn-logged at the refusal instead.
    #[error("{summary}. {hint}")]
    SettingRefused { summary: String, hint: String },
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

pub mod acp;
pub mod claude;
pub mod codex;
pub mod discovery;
pub(crate) mod jsonrpc;
pub mod launch;
pub mod mock;
pub mod shell_env;

/// The user's home directory. A Windows GUI or service launch routinely has no
/// `HOME` (Comet's own startup seeds it, but this crate must not depend on the
/// binary that links it), so `USERPROFILE` is the documented fallback.
pub fn home_dir() -> Option<std::path::PathBuf> {
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
pub type KnownDir = (std::path::PathBuf, InstallMethod);

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
pub fn resolve_cli(
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
pub fn all_known_dirs(install_dirs: Vec<KnownDir>) -> Vec<KnownDir> {
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
) -> (HarnessProbe, Option<String>) {
    let exe = match resolved {
        Ok(exe) => exe,
        Err(err) => {
            return (
                HarnessProbe::unresolved(unavailable_from_resolve(&err, stem, override_var)),
                None,
            );
        }
    };
    let override_is_set = std::env::var_os(override_var).is_some_and(|v| !v.is_empty());
    let install = HarnessInstall {
        path: exe.display().to_string(),
        method: classify_install(&exe, override_is_set, &known_dirs),
    };
    let availability = probe_cli_version(&exe).await;
    let version = match &availability {
        HarnessAvailability::Available { version } => version.clone(),
        _ => None,
    };
    (
        HarnessProbe {
            availability: enforce_version_floor(stem, availability),
            install: Some(install),
            // Filled by the per-provider update readers; a provider that publishes
            // no state leaves this `None` and simply renders one line less.
            update: None,
        },
        version,
    )
}

struct VersionFloor {
    minimum: &'static str,
    update_hint: &'static str,
}

fn version_floor(stem: &str) -> Option<VersionFloor> {
    match stem {
        "claude" => Some(VersionFloor {
            minimum: "2.1.228",
            update_hint: "Run `claude update` to install version 2.1.228 or newer.",
        }),
        "codex" => Some(VersionFloor {
            minimum: "0.147.0",
            update_hint: "Run `codex update` to install version 0.147.0 or newer.",
        }),
        "grok" => Some(VersionFloor {
            minimum: "1.0.5",
            update_hint: "Install Grok 1.0.5 or newer, or set GROK_EXECUTABLE to its path.",
        }),
        // Hermes has no promoted capture, and D110 deliberately leaves it
        // floorless rather than pretending a version is supported.
        _ => None,
    }
}

fn enforce_version_floor(stem: &str, availability: HarnessAvailability) -> HarnessAvailability {
    let HarnessAvailability::Available {
        version: Some(installed),
    } = &availability
    else {
        return availability;
    };
    let Some(floor) = version_floor(stem) else {
        return availability;
    };
    if compare_versions(installed, floor.minimum) == Some(std::cmp::Ordering::Less) {
        HarnessAvailability::unavailable("Update required", Some(floor.update_hint.to_string()))
    } else {
        availability
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

/// Why a resolved CLI would not spawn, as something the user can act on.
///
/// **D130.** Both run paths used to map `spawn()`'s error with two arms:
/// `NotFound` to [`HarnessError::NotInstalled`], and everything else to
/// [`HarnessError::Io`] — whose `Display` is `io: {0}`, so a `PermissionDenied`
/// reached the user through `drive_run`'s sink as `io: Access is denied. (os
/// error 5)`. That is rule 1 of `.agents/rules/user-facing-errors.md`, at the
/// third site after D119 and D100.
///
/// Reads `io::ErrorKind`, never the error's `Display`, exactly as
/// `probe_cli_version`'s `start_failure_hint` does for the same kinds — the two
/// answer the same question about the same binary, one before a run and one
/// during it.
///
/// **Reuses `NeedsSetup` rather than adding a third copy-carrying variant.**
/// `hermes::map_open_failure`'s doc comment sets the test: a new variant earns
/// its place only when the user's next action differs, not when the wording
/// does. A CLI that cannot be executed and a CLI that is signed out both answer
/// "this agent cannot run yet; go do one more thing to it", which is what
/// `NeedsSetup` says.
pub(crate) fn spawn_failure(exe: &std::path::Path, error: &std::io::Error) -> HarnessError {
    let name = exe.display();
    match error.kind() {
        // Unchanged, and deliberately not routed through the summary/hint
        // pair: `NotInstalled`'s own Display already reads as product copy in
        // the models pane, and its message is what the rail renders.
        std::io::ErrorKind::NotFound => HarnessError::NotInstalled(name.to_string()),
        std::io::ErrorKind::PermissionDenied => HarnessError::NeedsSetup {
            summary: "Not runnable".into(),
            hint: format!(
                "This account cannot run {name}. Check the file's permissions, or point Comet                  at a different copy."
            ),
        },
        _ => HarnessError::NeedsSetup {
            summary: "Won't start".into(),
            hint: format!(
                "{name} would not start. Reinstall the CLI, or point Comet at a working copy."
            ),
        },
    }
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
/// Byte budget for catalog migration markdown supplied by a provider. It is
/// the same class of bounded advisory prose as a notice detail, but remains
/// catalog metadata rather than becoming a transcript notice.
pub(crate) const MODEL_MIGRATION_MARKDOWN_MAX: usize = NOTICE_DETAIL_MAX;
/// Byte budget for a subagent's `prompt`, capped where it enters the doc
/// (`AgentEvent::SubagentStarted`) — never at its full, unbounded length.
/// The prompt is the same class of data `doc::parts::sanitize_tool_call`
/// already strips from `WriteFile`/`Mcp`: the transcript replays over the
/// LAN, so the full text never reaches the doc or the journal — the journal
/// records normalized `AgentEvent`s, and the cap runs before the event is
/// built, so it receives the same capped string. What keeps the full text
/// available locally is [`cap_prose`]'s own contract: the call site
/// debug-logs it before capping (`tracing::debug!`), so it survives in
/// `tracing`, not in any doc or journal. Same budget as
/// [`NOTICE_DETAIL_MAX`] — no reason for a subagent's instructions to get a
/// materially different allowance than any other provider prose.
pub(crate) const SUBAGENT_PROMPT_MAX: usize = NOTICE_DETAIL_MAX;
/// Byte budget for a subagent's `description` (D56), capped at the same
/// boundary and for the same class of reason as `SUBAGENT_PROMPT_MAX`: the
/// Task tool's own contract with the model is "a short (3-5 word)
/// description of the task", but nothing on the wire enforces that, so a
/// model that ignores it must not carry an unbounded label all the way into
/// the persisted doc. Unlike `prompt` this is not a privacy cap — a task
/// label is not the sensitive half of a subagent call — it exists only to
/// bound a field the wire never bounds. `description` is a label, the same
/// class of short text as a notice `summary`, not a body like `prompt` or
/// `NOTICE_DETAIL_MAX`, so it shares `NOTICE_SUMMARY_MAX`'s budget rather
/// than the longer one.
pub(crate) const SUBAGENT_DESCRIPTION_MAX: usize = NOTICE_SUMMARY_MAX;

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

/// The seven places a provider frame becomes an `AgentEvent::Diagnostic`
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
/// 5. Parse failures across all three providers — a stdout line that never
///    decoded at all. Always [`comet_proto::DiagnosticSeverity::Malformed`],
///    always this fixed sentinel; the raw line stays in `tracing` and never
///    travels with the event. Claude and Codex each have their own read-loop
///    call site; ACP's is `acp::session::handle_incoming`'s
///    `Incoming::Malformed` arm.
/// 6. ACP: an unclaimed `sessionUpdate` kind inside an otherwise-well-formed
///    `session/update` — `acp::normalize::session_update`'s Tier 3 arm,
///    rate-limited by kind-name through `session_update_once`. **Does NOT
///    call this function.** The summary has to embed the sanitized kind
///    name — task-mandated, since `AgentEvent::Diagnostic`'s payload never
///    travels and the kind name is the only thing that makes the report
///    actionable — which this function's fixed copy cannot express. The raw
///    frame is still warn-logged at the drop site (`session_update_once`
///    itself), matching every other sink's contract.
///
///    **This site emits a second, differently-shaped diagnostic, and it is
///    still one sink rather than an eighth.** Once `session_update_once`'s
///    rate limiter is full (`MAX_TRACKED_UPDATE_KINDS`), the first kind past
///    the cap emits `sessionUpdate/reporting-capped` — once per session —
///    saying that unknown-kind reporting has stopped. It reports on the
///    LIMITER, not on a frame, which is why it names no kind and embeds no
///    provider text at all; the kind that tripped it is agent-chosen and
///    would point a reader at nothing. See that const's own doc comment for
///    why saying so once beats capping silently.
/// 7. ACP: a `session/update` whose `update` object carries no `sessionUpdate`
///    key at all — same function, the `let Some(kind) = … else` arm. Calls
///    this function normally.
pub(crate) const UNPARSEABLE: &str = "unparseable";

/// Build the diagnostic event for a dropped frame. The caller has already
/// warn-logged the full frame at the drop site — this carries only the
/// sanitized name and Comet copy, never provider text (redaction is
/// structural: the payload is absent, not truncated).
/// How many times one discriminator's full payload may be logged before the
/// budget stops carrying it.
const FULL_FIDELITY_LOGS: u64 = 5;

/// Distinct discriminators the budget tracks. Matches the diagnostic registry's
/// own cap deliberately: the two answer the same question about the same
/// stream, and a budget that tracked more keys than the registry would be
/// bounding the cheap half.
const MAX_TRACKED_LOG_KEYS: usize = 64;

/// What the budget says about logging one occurrence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LogBudget {
    /// Log the frame or line in full — one of the first [`FULL_FIDELITY_LOGS`].
    Full,
    /// Log the count and the discriminator, never the payload. The nth
    /// occurrence, where n is past the budget.
    CountOnly(u64),
}

/// Per-discriminator log budget for the diagnostic drop sites (D10).
///
/// **The registry bounds memory and nothing bounded the producers.** A future
/// CLI renaming a high-volume method — `item/commandExecution/outputDelta` is
/// the worked example — moves it from Ignored to Unknown, and every output
/// chunk becomes a warn-level line CARRYING RAW COMMAND STDOUT, indefinitely.
/// The registry row saturating at one entry does nothing to slow that: the
/// count stays correct while the log does not stay bounded.
///
/// So the first few occurrences of each discriminator log in full — which is
/// what makes a new frame diagnosable — and every one after that logs the count
/// alone. The payload is the half that is both unbounded and sensitive.
///
/// **Process-global on purpose.** Log volume is a property of the process, not
/// of a session, and the alternative is threading a budget through
/// `parse_frame` and three reader loops to bound something none of them own. It
/// is only reached on the unknown/unparseable path, which is rare except in
/// exactly the scenario this exists for.
pub(crate) fn log_budget(discriminator: &str) -> LogBudget {
    use std::sync::{LazyLock, Mutex};
    static SEEN: LazyLock<Mutex<std::collections::HashMap<String, u64>>> =
        LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

    let mut seen = SEEN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    log_budget_in(&mut seen, discriminator)
}

/// The rule itself, over a caller's own table.
///
/// Split out so the tests own their state: the wrapper above is process-global,
/// and two tests sharing it made one of them depend on whether the other had
/// run — the exact in-process coupling `ADAPTER_ROOT_ENV_LOCK` exists to avoid
/// elsewhere. Nothing in production calls this directly.
fn log_budget_in(
    seen: &mut std::collections::HashMap<String, u64>,
    discriminator: &str,
) -> LogBudget {
    // Past the key cap an unseen discriminator is counted as one past the
    // budget rather than inserted: bounding the table is the point, and
    // `CountOnly` still logs it — the discriminator itself is short and fixed,
    // and losing the payload is what the cap is for.
    if !seen.contains_key(discriminator) && seen.len() >= MAX_TRACKED_LOG_KEYS {
        return LogBudget::CountOnly(FULL_FIDELITY_LOGS + 1);
    }
    let count = seen.entry(discriminator.to_owned()).or_insert(0);
    *count = count.saturating_add(1);
    if *count <= FULL_FIDELITY_LOGS {
        LogBudget::Full
    } else {
        LogBudget::CountOnly(*count)
    }
}

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
pub(crate) fn child_path(exe: &std::path::Path) -> Option<std::ffi::OsString> {
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
    std::env::join_paths(paths).ok()
}

pub(crate) fn compose_child_path(cmd: &mut tokio::process::Command, exe: &std::path::Path) {
    if let Some(path) = child_path(exe) {
        cmd.env("PATH", path);
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
            let detail = readable_detail(detail);
            HarnessAvailability::unavailable(
                "Not working",
                Some(if detail.is_empty() {
                    format!("`--version` failed ({status}).")
                } else {
                    format!("`--version` failed ({status}): {detail}")
                }),
            )
        }
        // **The `io::Error`'s own Display stops here** (D100). It reads "The
        // system cannot find the file specified. (os error 2)" — an OS error
        // code on screen, which `.agents/rules/user-facing-errors.md` rule 1
        // names outright. What the user can act on is the KIND, so that is what
        // survives; the full error stays in `tracing`, one line up.
        Ok(Err(err)) => {
            tracing::debug!(cli = %name, error = %err, kind = ?err.kind(), "cli could not be started");
            HarnessAvailability::unavailable("Not working", Some(start_failure_hint(&name, &err)))
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

/// One line of a CLI's stderr, made safe to render.
///
/// **Kept rather than dropped, by ruling (D100).** Rule 1 forbids "CLI stderr
/// dumped verbatim", and this is not a dump: one line, trimmed, control
/// characters removed, capped. For a CLI that runs and exits non-zero, that
/// line usually IS the diagnosis — a half-finished npm install says so in it —
/// and a generic sentence would lose the only actionable thing on offer.
/// `probe_tests::a_failing_cli_reports_its_own_error` asserts the passthrough
/// on purpose; this narrows what passes through, it does not end it.
///
/// The cap is a rail measurement, not a guess: the hint renders as hover text
/// beside a 148px row, and a stderr line long enough to wrap several times
/// pushes the actionable half out of the first glance — the same failure the
/// summary/hint split exists to fix.
fn readable_detail(line: &str) -> String {
    const MAX: usize = 160;
    let cleaned: String = line
        .trim()
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let cleaned = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    match cleaned.char_indices().nth(MAX) {
        None => cleaned,
        Some((cut, _)) => format!("{}…", cleaned[..cut].trim_end()),
    }
}

/// Why a resolved CLI would not start, in the user's words rather than the
/// operating system's.
///
/// Reads `io::ErrorKind` instead of the error's Display, so no OS error code
/// reaches a surface (D100). The path is still named: with an override set,
/// WHICH path failed is the whole diagnosis, which is what
/// `a_missing_binary_probes_as_unavailable` pins.
fn start_failure_hint(name: &std::path::Display<'_>, err: &std::io::Error) -> String {
    match err.kind() {
        std::io::ErrorKind::NotFound => {
            format!(
                "There is no file at {name}. Check the path, or clear the override to let Comet find the CLI itself."
            )
        }
        std::io::ErrorKind::PermissionDenied => {
            format!(
                "{name} is not runnable by this account. Check the file's permissions, or point Comet at a different copy."
            )
        }
        _ => format!(
            "{name} could not be started. Reinstall the CLI, or point Comet at a working copy."
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

/// Drain discovery stderr without retaining provider output. `tokio::io::copy`
/// uses a fixed-size buffer, so a noisy provider cannot grow memory or block on
/// a full pipe while production waits for its protocol reply.
pub(crate) fn drain_discovery_stderr(
    mut stderr: tokio::process::ChildStderr,
    provider: &'static str,
) {
    tokio::spawn(async move {
        let mut sink = tokio::io::sink();
        if let Err(err) = tokio::io::copy(&mut stderr, &mut sink).await {
            tracing::debug!(%provider, %err, "discovery stderr drain failed");
        }
    });
}

/// Compare two dotted-numeric versions, or decline to.
///
/// **String comparison is the bug this exists to prevent**: lexically
/// `"0.9.0" > "0.147.0"`, which would report a year-old CLI as newer than the
/// one that supersedes it. Components are compared numerically, and a missing
/// component counts as zero so `1.2` and `1.2.0` are equal.
///
/// `None` means "cannot say", and **every component must parse strictly** for
/// an answer to come back. A nightly tag, a git describe string, a pre-release
/// suffix like `0.148.0-rc1`, or a component too large for `u64` all decline.
/// The caller renders nothing rather than guessing, because both guesses are
/// wrong in a way the user would act on: "up to date" hides a real update, and
/// "update available" sends them to reinstall what they already have.
///
/// An earlier version truncated each component at its first non-digit, so
/// `0.148.0-rc1` compared *equal* to `0.148.0`. That was described as the
/// conservative direction and it was not: it is the "up to date" failure above,
/// telling someone on a release candidate that the stable release superseding
/// it does not exist. Truncation lost the one fact that mattered — that this
/// string is not a plain version and cannot be ranked against one.
pub(crate) fn compare_versions(a: &str, b: &str) -> Option<std::cmp::Ordering> {
    // Strict per component: `parse` rejects a pre-release suffix, an empty
    // component, and an overflowing one alike, and all three mean the same
    // thing here — this is not a dotted-numeric version.
    fn components(v: &str) -> Option<Vec<u64>> {
        v.split('.').map(|part| part.parse::<u64>().ok()).collect()
    }
    let (left, right) = (components(a)?, components(b)?);
    let width = left.len().max(right.len());
    for i in 0..width {
        let l = left.get(i).copied().unwrap_or(0);
        let r = right.get(i).copied().unwrap_or(0);
        if l != r {
            return Some(l.cmp(&r));
        }
    }
    Some(std::cmp::Ordering::Equal)
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

/// Signal a child by pid — or rather, by the process GROUP `launch::LaunchDescriptor::command`
/// places it in (D46). A negative pid is `kill(2)`'s "whole group" form, and
/// the group id equals the child's own pid because that command sets
/// `process_group(0)`. A real provider CLI owns longer-lived children of its
/// own — shells, command-safety helpers, MCP servers — that inherit this same
/// group unless they call `setsid`/`setpgid` themselves; those are reached
/// too, which is the point. Safe against pid reuse only while the caller
/// still holds the unreaped `Child` — every call site does — and reaches only
/// this group, never a process that left it or one that coincidentally
/// shares the numeric id on a different group.
#[cfg(unix)]
pub(crate) fn send_signal(pid: u32, signal: Signal) {
    let sig = match signal {
        Signal::Term => libc::SIGTERM,
        Signal::Kill => libc::SIGKILL,
    };
    // SAFETY: plain kill(2) on a process group we created for a pid we
    // spawned and have not yet reaped.
    unsafe {
        libc::kill(-(pid as libc::pid_t), sig);
    }
}

/// Windows has no graceful signal to send a piped child, so `Term` is a no-op
/// and `Kill` is `TerminateProcess`. Without this the escalation task was
/// inert off unix: an unresponsive CLI was never reaped, and the run loop —
/// which only ends on stdout EOF — hung until the child chose to exit.
///
/// Only the process itself dies here, not the tree it spawned — same caveat
/// `start_kill`/`kill_on_drop` carry. Reaching the tree on Windows is
/// `ProcessTreeJob`'s job (D46): unlike unix there is no "signal this whole
/// group" primitive, so that half of the fix is a Job Object established at
/// spawn instead of anything `send_signal` can do with just a pid.
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

/// D46's Windows half of "reach the whole provider-owned tree, not just the
/// pid we spawned".
///
/// Unix gets there through `send_signal` alone, because a POSIX process
/// group is nothing more than a number every descendant already carries
/// unless it opts out (`setsid`/`setpgid`) — `killpg` needs no handle kept
/// alive between spawn and shutdown. Windows has no such number: the only
/// way to reach a tree is a kernel object created at spawn and held onto
/// until shutdown, which is what this type is for.
///
/// **Deliberately scoped, not swept.** This kills only processes assigned to
/// the one job created for this one child — never a system-wide scan by
/// name, parent pid, or cwd, which is exactly the "reaches a process the
/// provider did not create" hazard `docs/debt/D46-provider-process-tree-cleanup.md`
/// warns about. A process the child launches with `CREATE_BREAKAWAY_FROM_JOB`
/// (when the job's own policy allows it, which this one does not request) is
/// not reached — that process asked to leave, the same way a unix child
/// calling `setsid` again leaves the group `send_signal` targets.
///
/// **Best-effort by construction.** Job creation, `SetInformationJobObject`,
/// or `AssignProcessToJobObject` can each fail (a denied nesting policy, a
/// handle-table limit, …), and none of that may fail the run — a failed
/// attach just falls back to the pre-existing direct-pid-only behavior via
/// `send_signal`. There is also an inherent race: a grandchild the child
/// forks before `attach` completes (called immediately after `spawn`, but not
/// atomically with it — Windows offers no `CREATE_SUSPENDED` plumbing through
/// `tokio::process`) is never assigned to the job. Accepting that race is the
/// documented tradeoff of this pattern without `CREATE_SUSPENDED`, not an
/// oversight; closing it fully would mean spawning suspended and resuming by
/// hand, which is a materially bigger and riskier change than this row asks
/// for.
#[cfg(windows)]
pub(crate) struct ProcessTreeJob(Option<windows_sys::Win32::Foundation::HANDLE>);

#[cfg(windows)]
impl ProcessTreeJob {
    /// Create a job with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` and assign
    /// `child` to it. Call this as soon as possible after `spawn` — see the
    /// race note on the type itself.
    pub(crate) fn attach(child: &tokio::process::Child) -> Self {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        };

        let Some(process_handle) = child.raw_handle() else {
            return Self(None);
        };
        // SAFETY: `CreateJobObjectW(null, null)` creates an unnamed job with
        // no security attributes, a documented plain use of the API; the
        // handle is closed on every path that does not store it in `Self`.
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return Self(None);
            }
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let configured = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                std::ptr::addr_of!(info).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if configured == 0 || AssignProcessToJobObject(job, process_handle) == 0 {
                CloseHandle(job);
                return Self(None);
            }
            Self(Some(job))
        }
    }

    /// Kill everything still assigned to the job — the whole tree, unlike
    /// `send_signal`'s direct-pid `TerminateProcess`. A no-op when `attach`
    /// could not set one up.
    pub(crate) fn terminate(&self) {
        let Some(job) = self.0 else { return };
        use windows_sys::Win32::System::JobObjects::TerminateJobObject;
        // SAFETY: `job` is a handle this instance created and still owns.
        unsafe {
            TerminateJobObject(job, 1);
        }
    }
}

#[cfg(windows)]
impl Drop for ProcessTreeJob {
    fn drop(&mut self) {
        if let Some(job) = self.0 {
            use windows_sys::Win32::Foundation::CloseHandle;
            // SAFETY: closes exactly the handle `attach` created for this
            // instance, once. `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` means this
            // also reaps anything still in the job on an ordinary drop — the
            // same backstop role `kill_on_drop` plays for the direct child.
            unsafe {
                CloseHandle(job);
            }
        }
    }
}

// SAFETY: the wrapped `HANDLE` is a kernel job-object handle, not a pointer
// into process-local memory — Windows documents cross-thread use of a handle
// as safe as long as calls are not literally concurrent on it, and every use
// here (`terminate`, the final `CloseHandle`) is a single atomic API call, so
// there is no shared mutable state to race.
#[cfg(windows)]
unsafe impl Send for ProcessTreeJob {}
#[cfg(windows)]
unsafe impl Sync for ProcessTreeJob {}

/// Unix already reaches the whole tree through `send_signal`'s group-targeted
/// `kill`, so there is nothing for this type to hold there — it exists only
/// so call sites stay platform-generic instead of growing their own
/// `#[cfg(windows)]`.
#[cfg(not(windows))]
pub(crate) struct ProcessTreeJob;

#[cfg(not(windows))]
impl ProcessTreeJob {
    pub(crate) fn attach(_child: &tokio::process::Child) -> Self {
        Self
    }

    pub(crate) fn terminate(&self) {}
}

/// Reap the child: graceful SIGTERM first, SIGKILL after `kill_grace`.
/// (`kill_on_drop` remains the last-resort backstop.)
///
/// Lives here rather than in one adapter because every adapter that spawns a
/// child ends the same way, and a second copy would be a second place for the
/// escalation order to drift. `send_signal` above is the direct-pid half of
/// D46's fix, and on unix it is the whole fix: the group-targeted `kill`
/// already reaches every descendant. **This function stays two-pid-only on
/// purpose — no `ProcessTreeJob` parameter** — because it is also the shared
/// call `acp::session` (a third adapter this row's fix does not reach yet;
/// see the row's own wording) already makes; adding a required tree argument
/// here would force that caller to attach one too, outside this change's
/// scope. Claude and Codex instead call `ProcessTreeJob::terminate` directly,
/// right alongside their own calls to this function — see either adapter's
/// `run_session`.
pub(crate) async fn shutdown_child(child: &mut tokio::process::Child, kill_grace: Duration) {
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

    /// Break caught (D10): the registry bounds memory, and nothing bounded the
    /// producers. A CLI renaming a high-volume method moves it from Ignored to
    /// Unknown, and every chunk warn-logs its full payload — raw command
    /// stdout — indefinitely. The registry row saturating at one entry does
    /// not slow that down at all.
    ///
    /// Asserts the shape rather than the numbers: the first few carry the
    /// payload, everything after carries a count, and the count keeps rising
    /// so an operator can still see the volume.
    #[test]
    fn a_repeated_discriminator_stops_carrying_its_payload() {
        let mut seen = std::collections::HashMap::new();
        let key = "test/one-loud-frame";
        for expected in 1..=FULL_FIDELITY_LOGS {
            assert_eq!(
                log_budget_in(&mut seen, key),
                LogBudget::Full,
                "occurrence {expected} is still diagnosable in full"
            );
        }
        assert_eq!(
            log_budget_in(&mut seen, key),
            LogBudget::CountOnly(FULL_FIDELITY_LOGS + 1)
        );
        assert_eq!(
            log_budget_in(&mut seen, key),
            LogBudget::CountOnly(FULL_FIDELITY_LOGS + 2)
        );
    }

    /// The key table is bounded too, and a discriminator past the cap is still
    /// LOGGED — just never with its payload. Losing the payload is what the cap
    /// is for; losing the fact would hide the drift the diagnostic exists to
    /// raise.
    #[test]
    fn a_discriminator_past_the_key_cap_still_logs_without_its_payload() {
        let mut seen = std::collections::HashMap::new();
        for i in 0..MAX_TRACKED_LOG_KEYS {
            let _ = log_budget_in(&mut seen, &format!("test/fill-{i}"));
        }
        assert_eq!(
            log_budget_in(&mut seen, "test/one-too-many"),
            LogBudget::CountOnly(FULL_FIDELITY_LOGS + 1),
            "past the cap nothing new is tracked, and nothing new is silent"
        );
    }

    /// Break caught (D130): every spawn failure but `NotFound` became
    /// `HarnessError::Io`, whose `Display` is `io: {0}`, and `drive_run`'s sink
    /// renders a `HarnessError` close to as-is — so `io: Access is denied. (os
    /// error 5)` reached the user. Rule 1 of
    /// `.agents/rules/user-facing-errors.md`, at the third site after D119 and
    /// D100.
    ///
    /// Asserts the absence AND the replacement, per the pattern the probe's own
    /// test set: dropping the raw text is no good if what replaced it says
    /// nothing to do.
    #[test]
    fn a_spawn_failure_never_reaches_the_user_as_an_os_error() {
        let exe = std::path::Path::new("/opt/agents/claude");
        for (kind, expected_summary) in [
            (std::io::ErrorKind::PermissionDenied, "Not runnable"),
            (std::io::ErrorKind::InvalidData, "Won't start"),
            (std::io::ErrorKind::WouldBlock, "Won't start"),
        ] {
            let error = std::io::Error::new(kind, "Access is denied. (os error 5)");
            match spawn_failure(exe, &error) {
                HarnessError::NeedsSetup { summary, hint } => {
                    assert_eq!(summary, expected_summary);
                    assert!(
                        hint.contains("claude"),
                        "which binary failed is the diagnosis: {hint}"
                    );
                    for leak in ["os error", "Access is denied", "io:"] {
                        assert!(
                            !format!("{summary}. {hint}").contains(leak),
                            "{kind:?} leaked raw OS detail: {hint}"
                        );
                    }
                }
                other => panic!("{kind:?} must become actionable copy, got {other:?}"),
            }
        }
    }

    /// The one arm that must NOT change: a missing binary keeps
    /// `NotInstalled`, whose Display already reads as product copy in the
    /// models pane ("Agent CLI not found: …") and which the harness rail
    /// renders directly.
    #[test]
    fn a_missing_binary_still_reports_as_not_installed() {
        let exe = std::path::Path::new("/opt/agents/claude");
        let error = std::io::Error::new(std::io::ErrorKind::NotFound, "no such file");
        match spawn_failure(exe, &error) {
            HarnessError::NotInstalled(named) => assert!(named.contains("claude"), "{named}"),
            other => panic!("a missing binary must stay NotInstalled, got {other:?}"),
        }
    }

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

    /// The whole reason this is not `a < b` on strings. Both of these are real
    /// codex-cli versions and the lexical order is the wrong way round.
    #[test]
    fn versions_compare_numerically_not_lexically() {
        use std::cmp::Ordering;
        assert_eq!(
            compare_versions("0.147.0", "0.9.0"),
            Some(Ordering::Greater),
            "0.147.0 supersedes 0.9.0; a string sort says the opposite"
        );
        assert_eq!(
            compare_versions("2.1.228", "2.1.228"),
            Some(Ordering::Equal)
        );
        assert_eq!(compare_versions("2.1.227", "2.1.228"), Some(Ordering::Less));
        // Differing component counts: the missing one is zero, not "smaller".
        assert_eq!(compare_versions("1.2", "1.2.0"), Some(Ordering::Equal));
        assert_eq!(compare_versions("1.2.1", "1.2"), Some(Ordering::Greater));
    }

    /// A version that is not plainly dotted-numeric is not orderable, and
    /// guessing either way misleads: "up to date" hides a real update, "update
    /// available" sends the user to reinstall what they have.
    #[test]
    fn an_unorderable_version_declines_to_compare() {
        assert_eq!(compare_versions("nightly", "0.147.0"), None);
        assert_eq!(compare_versions("0.147.0", "main-4a2077e"), None);
    }

    /// A pre-release must not compare EQUAL to its release. It used to, by
    /// truncating at the first non-digit, which told a user on `0.148.0-rc1`
    /// that the stable `0.148.0` superseding it did not exist. Declining is the
    /// honest answer: this string cannot be ranked against a plain version.
    #[test]
    fn a_pre_release_declines_rather_than_reading_as_its_release() {
        assert_eq!(compare_versions("0.148.0", "0.148.0-rc1"), None);
        assert_eq!(compare_versions("0.148.0-rc1", "0.148.0"), None);
    }

    /// A component too large for `u64` must decline, not silently become zero.
    /// Read as zero it inverts the comparison and suppresses a real update.
    #[test]
    fn an_overflowing_component_declines_rather_than_reading_as_zero() {
        assert_eq!(compare_versions("18446744073709551616.0", "0.147.0"), None);
        assert_eq!(compare_versions("0.147.0", "18446744073709551616.0"), None);
        // The largest value that does fit still compares.
        assert_eq!(
            compare_versions("18446744073709551615.0", "0.147.0"),
            Some(std::cmp::Ordering::Greater)
        );
    }

    /// An empty component is not a zero. `1..2` is malformed, not `1.0.2`.
    #[test]
    fn an_empty_component_declines() {
        assert_eq!(compare_versions("1..2", "1.0.2"), None);
        assert_eq!(compare_versions("1.2.", "1.2.0"), None);
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

    #[cfg(windows)]
    #[tokio::test]
    async fn a_codex_version_below_its_supported_floor_requires_an_update() {
        let dir = tempfile::tempdir().unwrap();
        let shim = dir.path().join("old-codex.cmd");
        std::fs::write(&shim, "@echo off\r\necho codex-cli 0.146.0\r\n").unwrap();

        let probe = probe_installed_cli(Ok(shim), "codex", "NO_SUCH_OVERRIDE_VAR", Vec::new())
            .await
            .0;

        assert_eq!(
            probe.availability.unavailable_summary(),
            Some("Update required")
        );
        assert_eq!(
            probe.availability.unavailable_hint(),
            Some("Run `codex update` to install version 0.147.0 or newer.")
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn an_outdated_codex_probe_keeps_its_install_and_update_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let shim = dir.path().join("old-codex.cmd");
        std::fs::write(&shim, "@echo off\r\necho codex-cli 0.146.0\r\n").unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join("version.json"),
            r#"{"latest_version":"0.148.0"}"#,
        )
        .unwrap();

        let harness = CodexHarness::new()
            .with_executable(&shim)
            .with_codex_home(home.path());
        let probe = harness.probe().await;

        assert_eq!(
            probe.availability.unavailable_summary(),
            Some("Update required")
        );
        assert_eq!(
            probe.install.expect("the resolved binary stays named").path,
            shim.display().to_string()
        );
        let update = probe.update.expect("the updater cache stays visible");
        assert_eq!(update.latest.as_deref(), Some("0.148.0"));
        assert_eq!(
            update.state,
            comet_proto::UpdateState::Available,
            "the update cache must compare against the probed outdated version"
        );
    }

    #[test]
    fn an_unorderable_or_missing_version_stays_available() {
        for version in [None, Some("0.147.0-rc1"), Some("nightly"), Some("1..2")] {
            let availability = enforce_version_floor(
                "codex",
                HarnessAvailability::Available {
                    version: version.map(str::to_owned),
                },
            );
            assert_eq!(
                availability,
                HarnessAvailability::Available {
                    version: version.map(str::to_owned),
                },
                "{version:?} cannot honestly be ranked against the Codex floor"
            );
        }
    }

    #[test]
    fn an_equal_or_newer_version_stays_available() {
        for version in ["0.147.0", "0.147.1", "0.148.0"] {
            assert_eq!(
                enforce_version_floor(
                    "codex",
                    HarnessAvailability::Available {
                        version: Some(version.into()),
                    },
                ),
                HarnessAvailability::Available {
                    version: Some(version.into()),
                },
                "{version} meets the Codex floor"
            );
        }
    }

    #[test]
    fn each_documented_provider_uses_its_own_floor_and_hermes_remains_floorless() {
        for (stem, installed, minimum, hint) in [
            (
                "claude",
                "2.1.227",
                "2.1.228",
                "Run `claude update` to install version 2.1.228 or newer.",
            ),
            (
                "codex",
                "0.146.0",
                "0.147.0",
                "Run `codex update` to install version 0.147.0 or newer.",
            ),
            (
                "grok",
                "1.0.4",
                "1.0.5",
                "Install Grok 1.0.5 or newer, or set GROK_EXECUTABLE to its path.",
            ),
        ] {
            let availability = enforce_version_floor(
                stem,
                HarnessAvailability::Available {
                    version: Some(installed.into()),
                },
            );
            assert_eq!(availability.unavailable_summary(), Some("Update required"));
            assert_eq!(availability.unavailable_hint(), Some(hint));
            assert!(
                hint.contains(minimum),
                "{stem}'s hint names its minimum version"
            );
        }

        assert_eq!(
            enforce_version_floor(
                "hermes",
                HarnessAvailability::Available {
                    version: Some("0.0.1".into()),
                },
            ),
            HarnessAvailability::Available {
                version: Some("0.0.1".into()),
            },
            "Hermes stays floorless until D110 has corpus evidence"
        );
    }

    #[test]
    fn probe_floors_match_the_supported_provider_versions_document() {
        const DOCUMENT: &str = include_str!("../../../docs/testing/supported-provider-versions.md");

        for (stem, provider, minimum) in [
            ("claude", "Claude Code", "2.1.228"),
            ("codex", "codex-cli", "0.147.0"),
            ("grok", "Grok", "1.0.5"),
        ] {
            let floor = version_floor(stem).expect("documented provider has a probe floor");
            assert_eq!(floor.minimum, minimum);
            assert!(
                DOCUMENT.contains(&format!("| {provider} | **{minimum}**")),
                "the production floor for {provider} must stay tied to the documented floor"
            );
        }
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
        .await
        .0;

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
        .await
        .0;
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

    /// Break caught (D100): the "could not be started" arm interpolated the
    /// `io::Error`'s Display, so the hint read `... could not be started: The
    /// system cannot find the file specified. (os error 2)` — an OS error code
    /// on a user-facing surface, which `.agents/rules/user-facing-errors.md`
    /// rule 1 names outright. It reached the models pane for every harness.
    ///
    /// Asserts the absence AND the replacement: dropping the raw text would be
    /// no good if what replaced it said nothing to do.
    #[tokio::test]
    async fn a_binary_that_cannot_start_names_no_os_error() {
        let missing = std::path::Path::new("comet-definitely-not-a-real-cli");
        let availability = probe_cli_version(missing).await;
        let hint = availability
            .unavailable_hint()
            .expect("a missing binary is unavailable");

        for leak in ["os error", "(os ", "Os {", "kind:"] {
            assert!(
                !hint.contains(leak),
                "raw OS detail reached the hint: {hint}"
            );
        }
        assert!(
            hint.contains("comet-definitely-not-a-real-cli"),
            "which path failed is the diagnosis with an override set: {hint}"
        );
        assert!(
            hint.contains("Check the path") || hint.contains("Reinstall"),
            "the hint has to name an action: {hint}"
        );
    }

    /// The stderr passthrough is KEPT by ruling (D100) and narrowed: one line,
    /// no control characters, capped. A CLI that runs and exits non-zero says
    /// the useful thing in that line, and a generic sentence would lose it.
    #[test]
    fn a_stderr_line_reaches_the_hint_readable_rather_than_raw() {
        assert_eq!(readable_detail("  boom  "), "boom");
        assert_eq!(
            readable_detail(
                "cannot find module
	at require (node:internal)"
            ),
            "cannot find module at require (node:internal)",
            "control characters must not reach a rendered string"
        );

        let long = readable_detail(&"x".repeat(400));
        assert!(
            long.chars().count() <= 161,
            "a stderr line long enough to wrap several times pushes the actionable              half out of the first glance; got {} chars",
            long.chars().count()
        );
        assert!(long.ends_with('…'), "a truncated line must say so: {long}");

        assert_eq!(readable_detail("   "), "", "an empty line stays empty");
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
