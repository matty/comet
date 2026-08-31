//! Codex harness: spawns the installed `codex` CLI as `codex app-server` and
//! speaks JSON-RPC 2.0 over stdio — the same interface the Codex IDE extension
//! uses (spec: docs/research/harness.md; behavior ported from comet's
//! `packages/harness/src/codex.ts`).
//!
//! - `initialize` handshake (clientInfo + `capabilities.experimentalApi`) then
//!   the `initialized` notification; unknown notification methods tolerated.
//! - `thread/start` (or `thread/resume` with a fresh-start fallback) →
//!   `SessionStarted`; `turn/start` carries the prompt, model, effort,
//!   `sandboxPolicy`, and approval policy.
//! - Notifications map to [`AgentEvent`]s: agentMessage/reasoning deltas (both
//!   `delta`/`textDelta` spellings), item lifecycles → typed ToolCall/ToolResult,
//!   `thread/tokenUsage/updated` → Usage, turn/completed|failed|aborted → Done.
//! - Approvals: the wire policy is derived from `runtime_mode`
//!   (`catalog::approval_policy`), and `item/commandExecution/requestApproval`,
//!   `item/fileChange/requestApproval` and `item/permissions/requestApproval`
//!   round-trip through [`RunControls::request_approval`]. A file-change
//!   request carries no path, so its detail is joined from the `item/started`
//!   that precedes it. Comet's engine owns "allow for this session"; the wire
//!   only ever hears `accept` or `decline`.
//! - Steering: `turn/steer { expectedTurnId }` into the live turn; a rejected
//!   steer (the turn-completed race) is queued and delivered as the next
//!   `turn/start` on the same thread. The session is persistent across turns
//!   while the steering mailbox lives.
//! - Interrupt: cancelling [`RunControls::interrupt`] sends `turn/interrupt`,
//!   escalating to SIGTERM → SIGKILL if the child is unresponsive; the stream
//!   always ends with `Done { status: Interrupted }`.

// `pub(crate)`, not private: `capture::record::scenarios::codex`'s approval reply builder calls
// `approval::decision_literal` directly, so the capture recorder sends the exact same "decision"
// string production's own `handle_server_request` does, instead of a second hand-copied literal
// that could drift. Narrowing this back to private breaks that build — see `decision_response`'s
// doc comment in `capture/record/scenarios/codex.rs`.
pub mod approval;
mod catalog;
pub mod discovery;
mod normalize;
mod update;

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::{Value, json};
use tokio::io::AsyncBufReadExt;
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

use comet_proto::{
    AgentEvent, ApprovalDecision, ApprovalRequest, DiagnosticSeverity, DoneStatus,
    HarnessCapabilities, HarnessId, HarnessProbe, InstallMethod, ModelCatalog, NoticeKind,
    NoticeSeverity, RunRequest, RuntimeMode, SteeringMode,
};

use crate::jsonrpc::{Incoming, RpcClient};
use crate::{Harness, HarnessError, RunControls, Signal, send_signal, shutdown_child};
use catalog::{
    REASONING_LEVELS, approval_policy, approvals_reviewer, sandbox_mode, sandbox_policy_value,
    static_models, to_effort,
};
use normalize::{
    Phase, RateLimitThresholds, delta_text, ignored_notification_reason, item_id, item_type,
    map_item, notice_for, plan_update_event, rate_limit_notice, turn_error_message, turn_id,
    usage_event,
};

/// Locate the device's installed Codex CLI: `CODEX_EXECUTABLE`, then our own
/// PATH, then the system's own PATH (a GUI/service launch's PATH misses what
/// the user's shell init shapes on unix, and goes stale against the persisted
/// environment on Windows — see [`crate::shell_env`]), then known install
/// locations as a last resort. Resolved per call — cheap after the snapshot is
/// cached.
pub fn resolve_codex_executable() -> Option<PathBuf> {
    crate::resolve_cli(
        "CODEX_EXECUTABLE",
        "codex",
        crate::all_known_dirs(codex_install_dirs()),
    )
}

/// Where a Codex CLI lands when PATH doesn't name it, each tagged with what
/// finding it there means (see [`crate::KnownDir`]).
fn codex_install_dirs() -> Vec<crate::KnownDir> {
    let mut dirs: Vec<crate::KnownDir> = Vec::new();
    if let Some(home) = crate::home_dir() {
        dirs.push((home.join(".local").join("bin"), InstallMethod::Native));
        dirs.push((home.join(".codex").join("bin"), InstallMethod::Native));
        dirs.push((home.join(".npm-global").join("bin"), InstallMethod::Npm));
    }
    if cfg!(windows) {
        // The official Windows installer is per-user under LOCALAPPDATA; the
        // rest are the package managers' shim dirs (all `.cmd`/`.exe`).
        dirs.extend(crate::env_dir("LOCALAPPDATA").map(|d| {
            (
                d.join("Programs").join("OpenAI").join("Codex").join("bin"),
                InstallMethod::Native,
            )
        }));
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
        // Untagged for the same reason as Claude's list: `/usr/local/bin` is
        // Intel Homebrew, a manual copy, and several installers' fallback all
        // at once.
        dirs.push((PathBuf::from("/usr/local/bin"), InstallMethod::Unknown));
    }
    dirs
}

/// True when `cwd` is a LINKED git worktree whose checked-out branch name
/// contains '/' — the exact shape that trips codex's sandbox worktree-mount
/// derivation (see the escalation in [`CodexHarness::run`]). Pure filesystem
/// reads: the worktree's `.git` pointer FILE names the admin dir, whose HEAD
/// symref carries the branch.
fn worktree_on_slashed_branch(cwd: &str) -> bool {
    if cwd.is_empty() {
        return false;
    }
    let dot_git = std::path::Path::new(cwd).join(".git");
    let is_pointer_file = std::fs::metadata(&dot_git).is_ok_and(|m| m.is_file());
    if !is_pointer_file {
        return false; // main checkout (`.git` dir) — codex handles it fine
    }
    let Ok(pointer) = std::fs::read_to_string(&dot_git) else {
        return false;
    };
    let Some(gitdir) = pointer.strip_prefix("gitdir:").map(str::trim) else {
        return false;
    };
    let Ok(head) = std::fs::read_to_string(std::path::Path::new(gitdir).join("HEAD")) else {
        return false;
    };
    head.trim()
        .strip_prefix("ref: refs/heads/")
        .is_some_and(|branch| branch.contains('/'))
}

/// A normalized request plus the user-visible consequence of normalizing it.
struct NormalizedRunRequest {
    request: RunRequest,
    sandbox_widened: bool,
}

/// Apply the request rewrites that must precede both launch and wire setup.
pub fn normalize_run_request(request: RunRequest) -> RunRequest {
    normalize_run_request_with_context(request).request
}

fn normalize_run_request_with_context(mut request: RunRequest) -> NormalizedRunRequest {
    // Historical Codex ≤0.144.x compatibility policy (docs/debt/README.md D13): a
    // workspace-write sandbox could derive a malformed mount for a linked
    // worktree whose branch contains '/'. Escalate that exact shape instead
    // of shipping a session where commands cannot run. This is maintained as
    // compatibility behavior, not as a claim about the currently captured CLI.
    let sandbox_widened = request.sandbox == comet_proto::SandboxLevel::WorkspaceWrite
        && worktree_on_slashed_branch(&request.cwd);
    if sandbox_widened {
        tracing::warn!(
            cwd = %request.cwd,
            "codex sandbox escalated to danger-full-access: linked worktree on a \
             slash-named branch trips codex's worktree-mount derivation"
        );
        // `runtime_mode` is read further down — by `approvals_reviewer`
        // on `thread/start`, and by the `RuntimeMode::FullAccess` check
        // below — so this escalation matters to more than the sandbox.
        // It raises only `sandbox` and deliberately leaves `runtime_mode`
        // as the caller set it, so the reviewer keeps reflecting what the
        // caller asked for rather than what this CLI-bug workaround
        // forced. On a full-access request the pair stays coherent by
        // coincidence; on any other mode it does not — the request now
        // runs with a danger-full-access sandbox under a mode that did
        // not ask for one. Approval policy and reviewer remain derived
        // from `runtime_mode` at thread and turn start; only the sandbox
        // is widened by this compatibility workaround.
        request.sandbox = comet_proto::SandboxLevel::DangerFullAccess;
    }
    NormalizedRunRequest {
        request,
        sandbox_widened,
    }
}

/// Build the provider-owned parameters for starting a new Codex thread.
pub fn thread_start_params(request: &RunRequest) -> Value {
    // Approval policy is derived, not pinned. ApprovalRequired intentionally
    // maps to `untrusted`; AutoAcceptEdits and Auto map to `on-request` now
    // that provider approvals reach Comet's approval surface.
    let mut params = serde_json::Map::new();
    params.insert("cwd".into(), request.cwd.clone().into());
    params.insert(
        "approvalPolicy".into(),
        approval_policy(request.runtime_mode).into(),
    );
    params.insert("sandbox".into(), sandbox_mode(request.sandbox).into());
    params.insert(
        "approvalsReviewer".into(),
        approvals_reviewer(request.runtime_mode).into(),
    );
    if let Some(model) = &request.model {
        params.insert("model".into(), model.clone().into());
    }
    // Service tier rides thread-start and every turn (mirrors the Codex IDE
    // client). "default" means Standard — omit it entirely.
    if let Some(tier) = service_tier(request) {
        params.insert("serviceTier".into(), tier.into());
    }
    Value::Object(params)
}

/// Build the provider-owned parameters for resuming a Codex thread.
pub fn thread_resume_params(request: &RunRequest, thread_id: &str) -> Value {
    let Value::Object(mut params) = thread_start_params(request) else {
        unreachable!("thread parameters are always a JSON object")
    };
    params.insert("threadId".into(), thread_id.into());
    Value::Object(params)
}

/// Build the provider-owned parameters for starting a Codex turn.
pub fn turn_start_params(request: &RunRequest, thread_id: &str, text: &str) -> Value {
    let mut params = serde_json::Map::new();
    params.insert("threadId".into(), thread_id.into());
    params.insert("input".into(), json!([{ "type": "text", "text": text }]));
    params.insert(
        "approvalPolicy".into(),
        approval_policy(request.runtime_mode).into(),
    );
    params.insert(
        "sandboxPolicy".into(),
        sandbox_policy_value(request.sandbox),
    );
    // Reasoning summaries stream (`item/reasoning/summaryTextDelta`) only
    // when asked for — without this codex "thinks" in silence for minutes:
    // nothing renders and the UI's 45s staleness gate flips Working off.
    params.insert("summary".into(), "auto".into());
    if let Some(model) = &request.model {
        params.insert("model".into(), model.clone().into());
    }
    if let Some(effort) = to_effort(request.reasoning) {
        params.insert("effort".into(), effort.into());
    }
    if let Some(tier) = service_tier(request) {
        params.insert("serviceTier".into(), tier.into());
    }
    Value::Object(params)
}

pub fn turn_steer_params(thread_id: &str, turn_id: &str, text: &str) -> Value {
    json!({
        "threadId": thread_id,
        "expectedTurnId": turn_id,
        "input": [{"type": "text", "text": text}],
    })
}

pub fn turn_interrupt_params(thread_id: &str, turn_id: &str) -> Value {
    json!({"threadId": thread_id, "turnId": turn_id})
}

fn service_tier(request: &RunRequest) -> Option<&str> {
    request
        .model_options
        .get("serviceTier")
        .and_then(Value::as_str)
        .filter(|tier| *tier != "default")
}

/// Describe the exact process launch used for a Codex run.
pub fn run_launch(exe: &Path, request: &RunRequest) -> crate::launch::LaunchDescriptor {
    let mut configured_env = std::collections::BTreeMap::new();
    if let Some(path) = crate::child_path(exe) {
        configured_env.insert("PATH".into(), path);
    }
    crate::launch::LaunchDescriptor {
        program: crate::discovery::program_path(exe),
        args: vec!["app-server".into()],
        cwd: (!request.cwd.is_empty()).then(|| request.cwd.clone().into()),
        configured_env,
        stdin: crate::launch::StdioMode::Piped,
        stdout: crate::launch::StdioMode::Piped,
        stderr: crate::launch::StdioMode::Piped,
        kill_on_drop: true,
        #[cfg(windows)]
        creation_flags: 0,
    }
}

/// Build the exact process command used for a Codex run.
pub(crate) fn build_run_command(exe: &Path, request: &RunRequest) -> Command {
    run_launch(exe, request).command()
}

const STARTUP_TIMEOUT_MESSAGE: &str =
    "Codex didn't finish starting. Open Codex in a terminal to sign in, then try again.";
const STARTUP_FAILURE_MESSAGE: &str =
    "Codex couldn't start. Check that Codex is signed in, then try again.";

/// The Codex harness. Construct with [`CodexHarness::new`]; tests point it at a
/// fake app server with [`CodexHarness::with_executable`].
pub struct CodexHarness {
    executable: Option<PathBuf>,
    /// Grace between `turn/interrupt` and SIGTERM.
    interrupt_grace: Duration,
    /// Grace between SIGTERM and SIGKILL.
    kill_grace: Duration,
    /// Bound on initialize plus thread resume/start. It never covers a turn.
    startup_timeout: Duration,
    /// One `model/list` per boot. `models()` is also called by titling
    /// (`crates/engine/src/titles.rs:159`) on every title generation, so an
    /// uncached discovery would spawn an app-server on a path the user never
    /// sees.
    discovery_cache: crate::discovery::DiscoveryCache,
    /// The live reply's `isDefault` row, if the last successful discovery
    /// named one (D72, `docs/debt/README.md`). Written once, alongside
    /// `discovery_cache`'s own cached answer, by the same `discovery::discover`
    /// closure — see that function's doc comment for why this rides beside
    /// the cache rather than inside it. `None` means either "not asked yet"
    /// or "asked and no row claimed it"; both leave catalog order alone.
    live_default_model: Arc<std::sync::Mutex<Option<String>>>,
    /// Overrides `$CODEX_HOME`/`~/.codex` for the login check; tests set it.
    codex_home: Option<PathBuf>,
}

impl Default for CodexHarness {
    fn default() -> Self {
        Self {
            executable: None,
            interrupt_grace: Duration::from_secs(2),
            kill_grace: Duration::from_secs(3),
            startup_timeout: Duration::from_secs(120),
            discovery_cache: crate::discovery::DiscoveryCache::default(),
            live_default_model: Arc::new(std::sync::Mutex::new(None)),
            codex_home: None,
        }
    }
}

impl CodexHarness {
    pub fn new() -> Self {
        Self::default()
    }

    /// The single declaration of what Codex can honor. `default_registry`
    /// calls this for the lazy slot's descriptor and the trait impl returns it
    /// once resolved, so the catalog cannot change on first use.
    ///
    /// Native `turn/steer` injects into the active turn; a steer that misses
    /// the turn falls back to a follow-up `turn/start` on the same thread.
    pub fn capabilities() -> HarnessCapabilities {
        HarnessCapabilities {
            supports_steering: true,
            steering_mode: SteeringMode::StepBoundary,
            reasoning_levels: REASONING_LEVELS.to_vec(),
            // All four, now that the wire policy is derived from the mode and an
            // approval it raises reaches the user. `ApprovalRequired` and `Auto`
            // were withheld while the policy was pinned at `"never"`, because
            // declaring a mode the adapter could not keep is a promise the run
            // breaks.
            //
            // Two of the four carry a caveat worth knowing before reading this
            // list as four guarantees. `AutoAcceptEdits`'s workspace-write
            // sandbox can be raised to danger-full-access by the linked-worktree
            // workaround below (`docs/debt/README.md` D13); the run emits a
            // transcript warning when that happens. And `Auto`
            // hands review to the provider via `approvalsReviewer:
            // "auto_review"` — no capture exercised that path, so what reaches
            // Comet in that mode follows the mapping table rather than an
            // observed run.
            runtime_modes: vec![
                RuntimeMode::ApprovalRequired,
                RuntimeMode::AutoAcceptEdits,
                RuntimeMode::Auto,
                RuntimeMode::FullAccess,
            ],
            // `FileChangeApprovalDecision` / `CommandExecutionApprovalDecision`
            // are bare literals — the wire carries `"decline"` and nothing
            // else, so a note has no field to travel in (`docs/debt/README.md` D24).
            carries_deny_note: false,
            // Both approval decision enums accept `"cancel"`, which denies
            // the action and interrupts the requesting turn.
            supports_approval_interrupt: true,
            // codex app-server names no chat on its wire; Comet titles it.
            self_titles: false,
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
        self
    }

    /// Use a fixed Codex home instead of `$CODEX_HOME`/`~/.codex`.
    ///
    /// Discovery reads `auth.json` there to decide whether asking the CLI is
    /// worth anything (see [`discovery::codex_home`]), so a test driving the
    /// fake app-server has to bring a home that looks logged in — otherwise it
    /// asserts against whatever the machine running it happens to have.
    pub fn with_codex_home(mut self, home: impl Into<PathBuf>) -> Self {
        self.codex_home = Some(home.into());
        self.discovery_cache = crate::discovery::DiscoveryCache::default();
        self
    }

    /// Where this device's Codex credentials live. Overridden by
    /// [`Self::with_codex_home`], else `$CODEX_HOME`, else `~/.codex`.
    fn codex_home(&self) -> Option<PathBuf> {
        self.codex_home.clone().or_else(discovery::codex_home)
    }

    /// Tune the interrupt→SIGTERM→SIGKILL escalation timing.
    pub fn with_graces(mut self, interrupt_grace: Duration, kill_grace: Duration) -> Self {
        self.interrupt_grace = interrupt_grace;
        self.kill_grace = kill_grace;
        self
    }

    /// Tune the initialize and thread setup bound. Running turns are unbounded.
    pub fn with_startup_timeout(mut self, timeout: Duration) -> Self {
        self.startup_timeout = timeout;
        self
    }

    fn resolve_executable(&self) -> Result<PathBuf, HarnessError> {
        if let Some(p) = &self.executable {
            return Ok(p.clone());
        }
        resolve_codex_executable().ok_or_else(|| {
            HarnessError::NotInstalled(crate::not_installed_message("codex", "CODEX_EXECUTABLE"))
        })
    }
}

#[async_trait]
impl Harness for CodexHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Codex
    }
    fn display_name(&self) -> &str {
        // "Codex" (not "Codex CLI") — comet composer/defaults.ts
        // HARNESS_LABEL; must also match the registry's lazy descriptor so
        // the catalog entry doesn't change after the first resolve.
        "Codex"
    }
    fn capabilities(&self) -> HarnessCapabilities {
        Self::capabilities()
    }

    async fn probe(&self) -> HarnessProbe {
        let (mut probe, installed) = crate::probe_installed_cli(
            self.resolve_executable(),
            "codex",
            "CODEX_EXECUTABLE",
            crate::all_known_dirs(codex_install_dirs()),
        )
        .await;
        // Read against the version this same pass just probed, so the verdict
        // can never describe a different binary than the one named beside it.
        // A blocking read is deliberate: it is ~100 bytes of local file, and
        // `probe` already runs in the engine's background boot task behind a
        // subprocess spawn that costs orders of magnitude more.
        probe.update = update::read_resolved_update(
            probe.install.as_ref(),
            self.codex_home().as_deref(),
            installed.as_deref(),
        );
        probe
    }

    /// The curated catalog (see [`catalog`]) unioned with whatever a
    /// short-lived `codex app-server` reported, then led by the live
    /// `isDefault` row when the reply named one (D72, `docs/debt/README.md`) —
    /// see `catalog::order_by_live_default`. An absent CLI still surfaces as
    /// [`HarnessError::NotInstalled`] rather than as a failed discovery: the
    /// user's action is different, and the picker's caption is not the place to
    /// say "no CLI".
    async fn models(&self) -> Result<ModelCatalog, HarnessError> {
        let exe = self.resolve_executable()?;
        let home = self.codex_home();
        let curated = static_models();
        let live_default = self.live_default_model.clone();
        let discovery = self
            .discovery_cache
            .get(move || discovery::discover(exe, home, live_default))
            .await;
        let default_id = self
            .live_default_model
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let catalog = self.discovery_cache.catalog(curated, discovery);
        Ok(catalog::order_by_live_default(
            catalog,
            default_id.as_deref(),
        ))
    }

    fn clear_discovery(&self) {
        self.discovery_cache.clear();
        // A retry may land on a different provider answer (or none); a stale
        // default from the attempt being discarded must not survive it.
        *self
            .live_default_model
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
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
        let normalized = normalize_run_request_with_context(request);
        let mut cmd = build_run_command(&exe, &normalized.request);
        let mut child = cmd
            .spawn()
            .map_err(|error| crate::spawn_failure(&exe, &error))?;
        // D46: as early as possible after spawn — see `ProcessTreeJob`'s own
        // doc for why "as early as possible" and not "atomically" is the best
        // this can do without `CREATE_SUSPENDED`. `Arc` because both the
        // interrupt-escalation task below and the final `shutdown_child` need
        // to reach it, and only one of them may own the eventual close.
        let tree = Arc::new(crate::ProcessTreeJob::attach(&child));

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| HarnessError::Protocol("codex child has no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| HarnessError::Protocol("codex child has no stdout".into()))?;
        let stderr_tail = crate::StderrTail::default();
        if let Some(stderr) = child.stderr.take() {
            let tail = stderr_tail.clone();
            tokio::spawn(async move {
                let mut lines = tokio::io::BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(target: "comet_harness::codex", "stderr: {line}");
                    tail.push(&line);
                }
            });
        }

        let (client, incoming) = RpcClient::new(stdin, stdout);
        let (event_tx, event_rx) = mpsc::channel::<Result<AgentEvent, HarnessError>>(256);
        tokio::spawn(run_session(Session {
            child,
            tree,
            client,
            incoming,
            event_tx,
            controls,
            request: normalized.request,
            sandbox_widened: normalized.sandbox_widened,
            interrupt_grace: self.interrupt_grace,
            kill_grace: self.kill_grace,
            startup_timeout: self.startup_timeout,
            stderr_tail,
        }));

        Ok(futures::stream::unfold(event_rx, |mut rx| async move {
            rx.recv().await.map(|ev| (ev, rx))
        })
        .boxed())
    }
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

struct Session {
    child: Child,
    /// D46: the whole-provider-tree half of shutdown, alongside `send_signal`
    /// — see `crate::ProcessTreeJob`.
    tree: Arc<crate::ProcessTreeJob>,
    client: RpcClient,
    incoming: mpsc::Receiver<Incoming>,
    event_tx: mpsc::Sender<Result<AgentEvent, HarnessError>>,
    controls: RunControls,
    request: RunRequest,
    /// True only for the linked-worktree compatibility escalation. The event
    /// follows SessionStarted so the transcript already has its run boundary.
    sandbox_widened: bool,
    interrupt_grace: Duration,
    kill_grace: Duration,
    startup_timeout: Duration,
    /// Rolling stderr tail for the crash message on an unexpected exit.
    stderr_tail: crate::StderrTail,
}

/// Turn-routing state (port of codex.ts's activeTurnId/completedTurnIds): the
/// `turn/start` response and the turn lifecycle notifications are separate
/// app-server messages that may arrive in either order — never revive a turn
/// that `turn/completed` already declared finished.
#[derive(Default)]
struct TurnRouter {
    active: Option<String>,
    completed: VecDeque<String>,
}

impl TurnRouter {
    fn is_completed(&self, id: &str) -> bool {
        self.completed.iter().any(|c| c == id)
    }

    fn note_started(&mut self, id: String) {
        if id.is_empty() || self.is_completed(&id) {
            return;
        }
        // A replacement `turn/started` is authoritative evidence that a stale
        // active turn is over, even if its completion notification was lost.
        if let Some(prev) = self.active.take()
            && prev != id
        {
            self.remember_completed(prev);
        }
        self.active = Some(id);
    }

    fn note_completed(&mut self, id: &str) {
        if id.is_empty() {
            return;
        }
        self.remember_completed(id.to_owned());
        if self.active.as_deref() == Some(id) {
            self.active = None;
        }
    }

    /// Adopt a turn id from a `turn/start` RESPONSE (the notification is
    /// allowed to beat it).
    fn adopt_started(&mut self, id: String) {
        self.active = (!id.is_empty() && !self.is_completed(&id)).then_some(id);
    }

    fn remember_completed(&mut self, id: String) {
        self.completed.push_back(id);
        // Bounded so a months-long persistent session can't grow it forever.
        while self.completed.len() > 32 {
            self.completed.pop_front();
        }
    }
}

fn new_message_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Rotate the assistant message id; returns (previous, next).
fn rotate(id: &mut String) -> (String, String) {
    let prev = std::mem::replace(id, new_message_id());
    (prev, id.clone())
}

async fn send(tx: &mpsc::Sender<Result<AgentEvent, HarnessError>>, ev: AgentEvent) -> bool {
    tx.send(Ok(ev)).await.is_ok()
}

/// `turn/start` and return the new turn id from the response.
async fn start_turn(client: &RpcClient, params: Value) -> Result<String, HarnessError> {
    let started = client.request("turn/start", params).await?;
    Ok(started["turn"]["id"].as_str().unwrap_or("").to_owned())
}

/// The per-run event loop: one task multiplexing app-server messages, the
/// steering mailbox, the interrupt token, and consumer liveness.
async fn run_session(session: Session) {
    let Session {
        mut child,
        tree,
        client,
        mut incoming,
        event_tx,
        controls,
        request,
        sandbox_widened,
        interrupt_grace,
        kill_grace,
        startup_timeout,
        stderr_tail,
    } = session;
    let RunControls {
        // Unclaimed on the Codex side: the synthesized yes/no question that used
        // to stand in for an approval is gone, and this adapter answers
        // `item/tool/requestUserInput` and `mcpServer/elicitation/request` with
        // -32601. Whichever slice claims one of those claims this field.
        request_input: _request_input,
        request_approval,
        mut steering,
        interrupt,
    } = controls;
    let request_approval = Arc::new(request_approval);

    // ---- handshake + thread (interruptible and bounded) -------------------
    let setup = async {
        client
            .request(
                "initialize",
                json!({
                    "clientInfo": {
                        "name": "comet-native",
                        "title": "Comet",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                    "capabilities": { "experimentalApi": true },
                }),
            )
            .await?;
        client.notify("initialized", None);

        let thread = if let Some(resume) = &request.resume {
            match client
                .request("thread/resume", thread_resume_params(&request, resume))
                .await
            {
                Ok(thread) => thread,
                // A missing/foreign rollout falls back to a fresh thread.
                Err(e) => {
                    tracing::debug!(
                        target: "comet_harness::codex",
                        "thread/resume failed (starting fresh): {e}"
                    );
                    client
                        .request("thread/start", thread_start_params(&request))
                        .await?
                }
            }
        } else {
            client
                .request("thread/start", thread_start_params(&request))
                .await?
        };
        let thread_id = thread["thread"]["id"].as_str().unwrap_or("").to_owned();
        Ok::<String, HarnessError>(thread_id)
    };
    let thread_id = tokio::select! {
        result = tokio::time::timeout(startup_timeout, setup) => match result {
            Ok(Ok(thread_id)) => thread_id,
            Ok(Err(error)) => {
                let status = child.try_wait().ok().flatten();
                if status.is_some() {
                    // Let the stderr reader drain the pipe after process exit.
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                tracing::warn!(
                    target: "comet_harness::codex",
                    %error,
                    ?status,
                    stderr = ?stderr_tail.snapshot(),
                    "codex startup failed"
                );
                let _ = event_tx
                    .send(Ok(AgentEvent::Done {
                        status: DoneStatus::Errored,
                        result: None,
                        error: Some(STARTUP_FAILURE_MESSAGE.into()),
                        session_id: None,
                    }))
                    .await;
                shutdown_child(&mut child, kill_grace).await;
                tree.terminate();
                return;
            }
            Err(_) => {
                tracing::warn!(
                    target: "comet_harness::codex",
                    timeout_secs = startup_timeout.as_secs_f64(),
                    status = ?child.try_wait().ok().flatten(),
                    stderr = ?stderr_tail.snapshot(),
                    "codex startup timed out"
                );
                let _ = event_tx
                    .send(Ok(AgentEvent::Done {
                        status: DoneStatus::Errored,
                        result: None,
                        error: Some(STARTUP_TIMEOUT_MESSAGE.into()),
                        session_id: None,
                    }))
                    .await;
                shutdown_child(&mut child, kill_grace).await;
                tree.terminate();
                return;
            }
        },
        _ = interrupt.cancelled() => {
            let _ = event_tx
                .send(Ok(AgentEvent::Done {
                    status: DoneStatus::Interrupted,
                    result: None,
                    error: None,
                    session_id: None,
                }))
                .await;
            shutdown_child(&mut child, kill_grace).await;
            tree.terminate();
            return;
        }
    };

    let mut assistant_message_id = new_message_id();
    if !send(
        &event_tx,
        AgentEvent::SessionStarted {
            harness: HarnessId::Codex,
            model: request.model.clone().unwrap_or_default(),
            tools: Vec::new(),
            cwd: request.cwd.clone(),
            session_id: thread_id.clone(),
            assistant_message_id: assistant_message_id.clone(),
            runtime_mode: request.runtime_mode,
        },
    )
    .await
    {
        shutdown_child(&mut child, kill_grace).await;
        tree.terminate();
        return;
    }

    if sandbox_widened
        && !send(
            &event_tx,
            AgentEvent::Notice {
                kind: NoticeKind::Info,
                severity: NoticeSeverity::Warning,
                summary: "Sandbox access widened".into(),
                detail: Some(
                    "This run can write anywhere on this machine, outside the workspace. Use a branch name without a slash to keep workspace-only write access.".into(),
                ),
                key: Some("codex-sandbox-escalated".into()),
            },
        )
        .await
    {
        shutdown_child(&mut child, kill_grace).await;
        return;
    }

    let mut router = TurnRouter::default();
    match start_turn(
        &client,
        turn_start_params(&request, &thread_id, &request.prompt),
    )
    .await
    {
        Ok(id) => router.adopt_started(id),
        Err(e) => {
            let _ = event_tx
                .send(Ok(AgentEvent::Done {
                    status: DoneStatus::Errored,
                    result: None,
                    error: Some(e.to_string()),
                    session_id: Some(thread_id.clone()),
                }))
                .await;
            shutdown_child(&mut child, kill_grace).await;
            tree.terminate();
            return;
        }
    }

    // ---- main loop --------------------------------------------------------
    // Deltas seen per agent-message item, so a model that never streams
    // (item/completed only) still emits its text exactly once.
    let mut streamed_text: HashSet<String> = HashSet::new();
    // Threshold latch for account/rateLimits/updated (per run, not per turn:
    // a session that crossed 80% stays crossed).
    let mut rate_thresholds = RateLimitThresholds::default();
    // Token usage is held until the turn ends, emitted just before Done.
    let mut pending_usage: Option<AgentEvent> = None;
    // `item/fileChange/requestApproval` carries no path and no diff — only an
    // `itemId` in the generated schema. The detail is
    // on the `item/started` that precedes it, so it is held here until the
    // request that needs it arrives, and dropped when the item completes.
    let mut file_changes: HashMap<String, Value> = HashMap::new();
    // Steers whose `turn/steer` lost the turn-completed race; delivered as the
    // next `turn/start` when the expected turn's end notification arrives.
    let mut queued_steers: VecDeque<String> = VecDeque::new();
    let mut steering_open = true;
    let mut interrupted = false;
    let mut interrupt_sent = false;
    // A Done has been emitted for the turn currently/last in flight.
    let mut done_current = false;
    let mut done_after_interrupt = false;
    let mut escalation: Option<tokio::task::JoinHandle<()>> = None;

    'main: loop {
        tokio::select! {
            inc = incoming.recv() => match inc {
                Some(Incoming::Notification { method, params }) => match method.as_str() {
                    "turn/started" => router.note_started(turn_id(&params)),

                    "item/agentMessage/delta" => {
                        // Unowned provider content must not attach to a future transcript entry.
                        if router.active.is_none() && queued_steers.is_empty() {
                            continue;
                        }
                        streamed_text.insert(item_id(&params));
                        if let Some(text) = delta_text(&params)
                            && !send(&event_tx, AgentEvent::TextDelta { text }).await
                        {
                            break 'main;
                        }
                    }

                    "item/reasoning/textDelta" | "item/reasoning/summaryTextDelta" => {
                        if router.active.is_none() && queued_steers.is_empty() {
                            continue;
                        }
                        if let Some(text) = delta_text(&params)
                            && !send(&event_tx, AgentEvent::ReasoningDelta { text }).await
                        {
                            break 'main;
                        }
                    }

                    // The whole plan, every time — Codex sends a complete
                    // snapshot per change rather than a delta, which is why
                    // this is a replacement and needs no accumulator.
                    "turn/plan/updated" => {
                        if router.active.is_none() && queued_steers.is_empty() {
                            continue;
                        }
                        if let Some(event) = plan_update_event(&params)
                            && !send(&event_tx, event).await
                        {
                            break 'main;
                        }
                    }

                    "item/started" | "item/completed" => {
                        if router.active.is_none() && queued_steers.is_empty() {
                            continue;
                        }
                        let phase = if method == "item/started" {
                            Phase::Started
                        } else {
                            Phase::Completed
                        };
                        let item = params.get("item").cloned().unwrap_or(Value::Null);
                        if matches!(item_type(&item), "agentMessage" | "agent_message") {
                            if phase == Phase::Completed {
                                // Fallback for non-streamed messages only.
                                let id = item.get("id").and_then(Value::as_str).unwrap_or("");
                                let text = item.get("text").and_then(Value::as_str).unwrap_or("");
                                if !streamed_text.contains(id)
                                    && !text.is_empty()
                                    && !send(&event_tx, AgentEvent::TextDelta { text: text.into() }).await
                                {
                                    break 'main;
                                }
                                // Deltas are token chunks, not steering
                                // boundaries: the completed item is the
                                // provider-authoritative end of the text part.
                                let (prev, _next) = rotate(&mut assistant_message_id);
                                if !send(
                                    &event_tx,
                                    AgentEvent::AssistantMessageCompleted {
                                        assistant_message_id: prev,
                                    },
                                )
                                .await
                                {
                                    break 'main;
                                }
                            }
                        } else {
                            track_file_change(&mut file_changes, phase, &item);
                            for ev in map_item(phase, &item) {
                                if !send(&event_tx, ev).await {
                                    break 'main;
                                }
                            }
                        }
                    }

                    "thread/tokenUsage/updated" => {
                        if router.active.is_none() && queued_steers.is_empty() {
                            continue;
                        }
                        if let Some(usage) = usage_event(&params) {
                            pending_usage = Some(usage);
                        }
                    }

                    "turn/completed" => {
                        let id = turn_id(&params);
                        if router.is_completed(&id) {
                            continue;
                        }
                        router.note_completed(&id);
                        // Item ids never span turns; without this the set grew
                        // one entry per message for a persistent session's life.
                        streamed_text.clear();
                        file_changes.clear();
                        if let Some(usage) = pending_usage.take()
                            && !send(&event_tx, usage).await
                        {
                            break 'main;
                        }
                        let error = turn_error_message(&params);
                        // `cancel` is an approval RESPONSE, not Comet's
                        // `turn/interrupt` request, so it never trips the
                        // local interrupt token. D44 records that Codex can
                        // report an interrupted turn as a completed
                        // notification whose own status is `interrupted`, so
                        // the provider's terminal state is authoritative no
                        // matter which interrupt route produced it. Ignoring
                        // that state turns "Deny & stop" into a clean
                        // completion.
                        let provider_interrupted = params
                            .get("turn")
                            .and_then(|turn| turn.get("status"))
                            .and_then(Value::as_str)
                            == Some("interrupted");
                        let status = if interrupted || provider_interrupted {
                            DoneStatus::Interrupted
                        } else if error.is_some() {
                            DoneStatus::Errored
                        } else {
                            DoneStatus::Completed
                        };
                        done_current = true;
                        if !send(
                            &event_tx,
                            AgentEvent::Done {
                                status,
                                result: None,
                                error,
                                session_id: Some(thread_id.clone()),
                            },
                        )
                        .await
                        {
                            break 'main;
                        }
                        if interrupted {
                            done_after_interrupt = true;
                            break 'main;
                        }
                        // Persistent session: a steer that lost the race with
                        // this turn's end becomes the next turn now; otherwise
                        // stay alive for the mailbox — the caller owns teardown.
                        if let Some(text) = queued_steers.pop_front() {
                            if !steer_as_new_turn(
                                &client,
                                turn_start_params(&request, &thread_id, &text),
                                &mut router,
                                &event_tx,
                                &mut assistant_message_id,
                                &mut done_current,
                            )
                            .await
                            {
                                break 'main;
                            }
                        } else if !steering_open {
                            break 'main;
                        }
                    }

                    "turn/failed" => {
                        router.note_completed(&turn_id(&params));
                        if let Some(usage) = pending_usage.take()
                            && !send(&event_tx, usage).await
                        {
                            break 'main;
                        }
                        done_current = true;
                        if interrupted {
                            done_after_interrupt = true;
                        }
                        let _ = send(
                            &event_tx,
                            AgentEvent::Done {
                                status: if interrupted {
                                    DoneStatus::Interrupted
                                } else {
                                    DoneStatus::Errored
                                },
                                result: None,
                                error: Some(
                                    turn_error_message(&params)
                                        .unwrap_or_else(|| "Codex turn failed".into()),
                                ),
                                session_id: Some(thread_id.clone()),
                            },
                        )
                        .await;
                        break 'main;
                    }

                    "turn/aborted" => {
                        router.note_completed(&turn_id(&params));
                        done_current = true;
                        if interrupted {
                            done_after_interrupt = true;
                        }
                        let _ = send(
                            &event_tx,
                            AgentEvent::Done {
                                status: DoneStatus::Interrupted,
                                result: None,
                                error: None,
                                session_id: Some(thread_id.clone()),
                            },
                        )
                        .await;
                        break 'main;
                    }

                    "error" => {
                        let message = params
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("Codex error")
                            .to_owned();
                        if !send(&event_tx, AgentEvent::Error { message }).await {
                            break 'main;
                        }
                    }

                    "mcpServer/startupStatus/updated"
                    | "mcpServer/oauthLogin/completed"
                    | "thread/environment/disconnected" => {
                        if let Some(ev) = notice_for(&method, &params)
                            && !send(&event_tx, ev).await
                        {
                            break 'main;
                        }
                    }

                    "account/rateLimits/updated" => {
                        if let Some(ev) = rate_limit_notice(&params, &mut rate_thresholds)
                            && !send(&event_tx, ev).await
                        {
                            break 'main;
                        }
                    }

                    // Recognized, deliberately dropped — the Ignored tier
                    // (thread/status, settings echo, output deltas, …).
                    m if ignored_notification_reason(m).is_some() => {}

                    // Sink 2: on neither list — still dropped, now counted.
                    _ => {
                        tracing::warn!(
                            target: "comet_harness::codex",
                            %method,
                            params = %params,
                            "unrecognized notification (recorded as a diagnostic)"
                        );
                        let ev = crate::diagnostic(&method, DiagnosticSeverity::Unknown);
                        if !send(&event_tx, ev).await {
                            break 'main;
                        }
                    }
                },

                Some(Incoming::Request { id, method, params }) => {
                    // The join happens here, while the map is in scope; the
                    // mapping itself stays a pure function of what it is handed.
                    let changes = params
                        .get("itemId")
                        .and_then(Value::as_str)
                        .and_then(|item_id| file_changes.get(item_id))
                        .cloned();
                    if let Some(ev) = handle_server_request(
                        &client,
                        id,
                        &method,
                        &params,
                        changes.as_ref(),
                        &request_approval,
                    ) && !send(&event_tx, ev).await
                    {
                        break 'main;
                    }
                }

                Some(Incoming::Malformed(kind)) => {
                    // Sink 5 (Codex side): the reader already warn-logged the
                    // raw line; only the KIND travels (D9) — a bare sentinel
                    // could not tell log noise on stdout from a protocol that
                    // moved.
                    let ev = crate::diagnostic(kind.discriminator(), DiagnosticSeverity::Malformed);
                    if !send(&event_tx, ev).await {
                        break 'main;
                    }
                }

                // stdout EOF or reader gone: the app server exited.
                Some(Incoming::Eof) | None => break 'main,
            },

            steer = steering.recv(), if steering_open && !interrupted => match steer {
                Some(msg) => {
                    let text = msg.prompt;
                    if let Some(expected) = router.active.clone() {
                        let steer_params = turn_steer_params(&thread_id, &expected, &text);
                        match client.request("turn/steer", steer_params).await {
                            Ok(_) => {
                                let (prev, next) = rotate(&mut assistant_message_id);
                                if !send(
                                    &event_tx,
                                    AgentEvent::Steered {
                                        assistant_message_id: Some(prev),
                                        next_assistant_message_id: Some(next),
                                    },
                                )
                                .await
                                {
                                    break 'main;
                                }
                            }
                            // A failed `turn/steer` does NOT mean the text is
                            // bad: most commonly the active turn finished
                            // between the UI send and this request. Queue it
                            // for redelivery as the next `turn/start` when the
                            // expected turn's end arrives (also the safe
                            // fallback for older Codex without steering).
                            Err(e) => {
                                tracing::debug!(
                                    target: "comet_harness::codex",
                                    "turn/steer rejected (queued as next turn): {e}"
                                );
                                if router.active.as_deref() == Some(expected.as_str())
                                    && !router.is_completed(&expected)
                                {
                                    queued_steers.push_back(text);
                                } else if !steer_as_new_turn(
                                    &client,
                                    turn_start_params(&request, &thread_id, &text),
                                    &mut router,
                                    &event_tx,
                                    &mut assistant_message_id,
                                    &mut done_current,
                                )
                                .await
                                {
                                    break 'main;
                                }
                            }
                        }
                    } else if !steer_as_new_turn(
                        &client,
                        turn_start_params(&request, &thread_id, &text),
                        &mut router,
                        &event_tx,
                        &mut assistant_message_id,
                        &mut done_current,
                    )
                    .await
                    {
                        break 'main;
                    }
                }
                None => {
                    // Mailbox closed (the caller's graceful idle-reap): finish
                    // once nothing is in flight — mirrors codex.ts's steer loop
                    // `finish()` on a null take.
                    steering_open = false;
                    if router.active.is_none() && queued_steers.is_empty() {
                        break 'main;
                    }
                }
            },

            _ = interrupt.cancelled(), if !interrupt_sent => {
                interrupt_sent = true;
                interrupted = true;
                if let Some(turn) = router.active.clone() {
                    let client = client.clone();
                    let thread = thread_id.clone();
                    tokio::spawn(async move {
                        if let Err(e) = client
                            .request("turn/interrupt", turn_interrupt_params(&thread, &turn))
                            .await
                        {
                            tracing::debug!(
                                target: "comet_harness::codex",
                                "turn/interrupt failed (escalation will reap): {e}"
                            );
                        }
                    });
                    // Escalate if the app server doesn't wind down (turn/aborted)
                    // within the grace periods: SIGTERM, then SIGKILL — plus
                    // the whole-tree kill (D46), which unix already gets
                    // through `send_signal`'s group-kill but Windows only
                    // gets through `tree`.
                    if let Some(pid) = child.id() {
                        let tree = tree.clone();
                        escalation = Some(tokio::spawn(async move {
                            tokio::time::sleep(interrupt_grace).await;
                            send_signal(pid, Signal::Term);
                            tokio::time::sleep(kill_grace).await;
                            tree.terminate();
                            send_signal(pid, Signal::Kill);
                        }));
                    }
                } else {
                    // Idle between turns: nothing to interrupt — the terminal
                    // bookkeeping below still guarantees Done { Interrupted }.
                    break 'main;
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
                    session_id: Some(thread_id.clone()),
                }))
                .await;
        } else if !interrupted && !done_current {
            // A child KILLED mid-turn (OS memory pressure, `killall codex`)
            // must not read as a silent success — codex.ts's signal-death
            // handling, reduced to the turn-in-flight case.
            let status = child.try_wait().ok().flatten();
            let _ = event_tx
                .send(Ok(AgentEvent::Done {
                    status: DoneStatus::Errored,
                    result: None,
                    error: Some(crate::crash_message(
                        "codex app-server",
                        status,
                        &stderr_tail,
                    )),
                    session_id: Some(thread_id.clone()),
                }))
                .await;
        }
    }

    shutdown_child(&mut child, kill_grace).await;
    tree.terminate();
    if let Some(handle) = escalation {
        handle.abort();
    }
}

/// Deliver a steer as a fresh `turn/start` on the same thread (the fallback
/// leg of the steer race, and the between-turns delivery path). Returns false
/// when the loop should end (turn/start failed or the consumer hung up).
async fn steer_as_new_turn(
    client: &RpcClient,
    params: Value,
    router: &mut TurnRouter,
    event_tx: &mpsc::Sender<Result<AgentEvent, HarnessError>>,
    assistant_message_id: &mut String,
    done_current: &mut bool,
) -> bool {
    match start_turn(client, params).await {
        Ok(id) => {
            router.adopt_started(id);
            *done_current = false;
            let (prev, next) = rotate(assistant_message_id);
            send(
                event_tx,
                AgentEvent::Steered {
                    assistant_message_id: Some(prev),
                    next_assistant_message_id: Some(next),
                },
            )
            .await
        }
        Err(e) => {
            let _ = send(
                event_tx,
                AgentEvent::Error {
                    message: format!("Steering failed: {e}"),
                },
            )
            .await;
            false
        }
    }
}

// ---------------------------------------------------------------------------
// Approvals (approval-as-input parity with comet's UX)
// ---------------------------------------------------------------------------

type RequestApprovalFn =
    Box<dyn Fn(ApprovalRequest) -> tokio::sync::oneshot::Receiver<ApprovalDecision> + Send + Sync>;

/// Remember what a `fileChange` item is changing, so the approval request that
/// follows it — which carries only an `itemId` — can be rendered.
///
/// **Bounded.** A turn that changes thousands of files must not grow this
/// without limit (`docs/debt/README.md` D10 is the standing version of that mistake). At the
/// cap the entry is simply not recorded, so its approval degrades to
/// `Unknown` ("Change a file") rather than to a wrong path — vague, and on the
/// safe side of the permission boundary, since `Unknown` is not allowlistable.
fn track_file_change(map: &mut HashMap<String, Value>, phase: Phase, item: &Value) {
    if !matches!(item_type(item), "fileChange" | "file_change") {
        return;
    }
    let Some(id) = item.get("id").and_then(Value::as_str) else {
        return;
    };
    match phase {
        Phase::Started => {
            if map.len() >= MAX_TRACKED_FILE_CHANGES && !map.contains_key(id) {
                tracing::debug!(
                    target: "comet_harness::codex",
                    item_id = id,
                    "file-change detail not tracked (cap reached); its approval will read as Unknown"
                );
                return;
            }
            // Reduced here, never stored raw: what is held until the approval
            // arrives must not scale with the size of the change. See
            // `approval::summarize_changes`.
            if let Some(changes) = item.get("changes") {
                map.insert(id.to_owned(), approval::summarize_changes(changes));
            }
        }
        // The approval arrives between started and completed, so the detail is
        // no longer needed once the item is done.
        Phase::Completed => {
            map.remove(id);
        }
    }
}

/// Bound for [`track_file_change`]. Comfortably above any turn a human watches
/// approve one file at a time, and small enough that a runaway turn cannot use
/// it as an allocator.
const MAX_TRACKED_FILE_CHANGES: usize = 256;

/// Serve one server→client request. Approval requests round-trip through
/// [`RunControls::request_approval`] (in a subtask so the message loop keeps
/// flowing while the user thinks — a blocked read here would stall the very
/// transcript they are reading to decide). Anything else is rejected as
/// unsupported so the server never wedges awaiting a reply.
///
/// `changes` is the `changes` array recorded for this request's `itemId`, if
/// any; see [`track_file_change`].
fn handle_server_request(
    client: &RpcClient,
    id: Value,
    method: &str,
    params: &Value,
    changes: Option<&Value>,
    request_approval: &Arc<RequestApprovalFn>,
) -> Option<AgentEvent> {
    let is_approval = matches!(
        method,
        approval::COMMAND_APPROVAL
            | approval::FILE_CHANGE_APPROVAL
            | approval::PERMISSIONS_APPROVAL
    );
    if !is_approval {
        // Answer FIRST — the server must never wedge awaiting a reply — then
        // count. The -32601 reply is the same one this adapter has always
        // sent for an unsupported method; counting rides the return path,
        // nothing more.
        client.respond_error(&id, -32601, &format!("unsupported method: {method}"));
        tracing::warn!(
            target: "comet_harness::codex",
            %method,
            params = %params,
            "unrecognized server request (recorded as a diagnostic)"
        );
        return Some(crate::diagnostic(method, DiagnosticSeverity::Unknown));
    }
    let request = approval::approval_request(method, params, changes);
    let client = client.clone();
    let request_approval = Arc::clone(request_approval);
    tokio::spawn(async move {
        // A dropped resolver (the run went away) means the user never answered
        // and never will. Decline — never silently accept, and never simply
        // stay quiet, which would leave the turn blocked on this call forever.
        let decision = (request_approval)(request)
            .await
            .unwrap_or(ApprovalDecision::Expired);
        client.respond(&id, approval::decision_response(&decision));
    });
    None
}

// ---------------------------------------------------------------------------
// Child lifecycle
// ---------------------------------------------------------------------------

#[cfg(test)]
mod install_dir_tests {
    use super::*;

    /// The tags on the real catalogue, not a fabricated one. `classify_install`
    /// is only ever as right as the list it is handed, so the lookup being
    /// tested elsewhere proves nothing about the label a user actually sees.
    ///
    /// Windows-only because the entries asserted are the Windows branch, and
    /// they are built from `%LOCALAPPDATA%` at call time.
    #[test]
    #[cfg(windows)]
    fn the_windows_catalogue_tags_the_official_installer_as_native() {
        let dirs = codex_install_dirs();
        let localappdata = crate::env_dir("LOCALAPPDATA").expect("set on every Windows machine");
        let official = localappdata
            .join("Programs")
            .join("OpenAI")
            .join("Codex")
            .join("bin");
        let tag = dirs
            .iter()
            .find(|(d, _)| *d == official)
            .map(|(_, m)| *m)
            .expect("the official installer dir must be in the catalogue");
        assert_eq!(tag, InstallMethod::Native);
    }

    /// `~/.npm-global/bin` is an npm prefix, not a native install — it sits in
    /// the same list as the native dirs and is the easiest one to mislabel.
    #[test]
    fn the_npm_prefix_is_tagged_npm_not_native() {
        let Some(home) = crate::home_dir() else {
            return;
        };
        let dirs = codex_install_dirs();
        let npm_global = home.join(".npm-global").join("bin");
        let tag = dirs
            .iter()
            .find(|(d, _)| *d == npm_global)
            .map(|(_, m)| *m)
            .expect("the npm prefix must be in the catalogue");
        assert_eq!(tag, InstallMethod::Npm);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn slashed_branch_worktrees_are_detected_for_sandbox_escalation() {
        let tmp = tempfile::tempdir().unwrap();
        let make = |name: &str, branch: &str| {
            let wt = tmp.path().join(name);
            let admin = tmp.path().join(format!("{name}-admin"));
            std::fs::create_dir_all(&wt).unwrap();
            std::fs::create_dir_all(&admin).unwrap();
            std::fs::write(wt.join(".git"), format!("gitdir: {}\n", admin.display())).unwrap();
            std::fs::write(admin.join("HEAD"), format!("ref: refs/heads/{branch}\n")).unwrap();
            wt.display().to_string()
        };
        assert!(worktree_on_slashed_branch(&make(
            "slashed",
            "wing/prd-5645"
        )));
        assert!(!worktree_on_slashed_branch(&make("plain", "brave-ember")));
        // A main checkout (`.git` DIRECTORY) never escalates.
        let main = tmp.path().join("main");
        std::fs::create_dir_all(main.join(".git")).unwrap();
        assert!(!worktree_on_slashed_branch(&main.display().to_string()));
        // Detached HEAD (raw sha) never escalates.
        let detached = make("detached", "x");
        std::fs::write(
            tmp.path().join("detached-admin").join("HEAD"),
            "0ba950848abc\n",
        )
        .unwrap();
        assert!(!worktree_on_slashed_branch(&detached));
        assert!(!worktree_on_slashed_branch(""));
        assert!(!worktree_on_slashed_branch("/nonexistent/path"));
    }

    /// The `changes` a file-change approval needs are held from `item/started`
    /// and released at `item/completed` — the request arrives between the two.
    #[test]
    fn a_file_changes_detail_is_held_only_while_the_item_is_open() {
        let mut map = HashMap::new();
        let item = json!({"type": "fileChange", "id": "f1",
                          "changes": [{"path": "/a.rs", "kind": {"type": "add"}}]});
        track_file_change(&mut map, Phase::Started, &item);
        assert!(map.contains_key("f1"));
        track_file_change(&mut map, Phase::Completed, &item);
        assert!(map.is_empty(), "detail outlived the item it describes");

        // Other item types never enter the map.
        track_file_change(
            &mut map,
            Phase::Started,
            &json!({"type": "commandExecution", "id": "c1", "command": "ls"}),
        );
        assert!(map.is_empty());
    }

    #[test]
    fn tracked_file_changes_are_bounded() {
        // An unbounded map here is `docs/debt/README.md` D10's mistake with a new name. At
        // the cap a new item is not recorded, so its approval degrades to
        // Unknown — vague, and un-allowlistable, which is the safe direction.
        let mut map = HashMap::new();
        for i in 0..MAX_TRACKED_FILE_CHANGES + 10 {
            let item = json!({"type": "fileChange", "id": format!("f{i}"),
                              "changes": [{"path": "/a.rs", "kind": {"type": "add"}}]});
            track_file_change(&mut map, Phase::Started, &item);
        }
        assert_eq!(map.len(), MAX_TRACKED_FILE_CHANGES);
    }

    #[test]
    fn turn_router_never_revives_completed_turns() {
        let mut r = TurnRouter::default();
        r.note_completed("t-1");
        // The turn/start response arriving after turn/completed must not
        // resurrect the turn.
        r.adopt_started("t-1".into());
        assert_eq!(r.active, None);
        // Nor may a late turn/started notification.
        r.note_started("t-1".into());
        assert_eq!(r.active, None);

        r.note_started("t-2".into());
        assert_eq!(r.active.as_deref(), Some("t-2"));
        // A replacement started turn retires the stale one.
        r.note_started("t-3".into());
        assert_eq!(r.active.as_deref(), Some("t-3"));
        assert!(r.is_completed("t-2"));
    }

    /// The decision literals have no message field, so a note typed into the
    /// composer cannot reach the model (`docs/debt/README.md` D24). This declaration is
    /// what stops the UI promising delivery.
    #[test]
    fn codex_cannot_carry_a_deny_note() {
        assert!(!CodexHarness::capabilities().carries_deny_note);
    }

    #[test]
    fn codex_declares_native_approval_interrupt_support() {
        assert!(CodexHarness::capabilities().supports_approval_interrupt);
    }
}
