//! Hermes Agent, over ACP.
//!
//! The second harness on [`crate::acp`], and the first evidence for or against
//! the shared layer's central claim -- "zero vendor paths in the shared
//! decode, all ten in `grok.rs`" -- which PR1's own capture (2026-08-28,
//! hermes-agent 0.15.2) had been asserted against a sample of one before this
//! file existed.
//!
//! **Hermes is the capability-degradation case.** Its `initialize` reply
//! carries no `_meta.steering` at all (a steer falls back to the turn
//! boundary, same as Grok) and no effort ladder anywhere in the handshake or
//! session config -- `capabilities()` declares an empty one deliberately,
//! because a populated ladder here would be a promise the run breaks.
//!
//! **`session/new` is NOT auth-free the way Grok's is.** Grok's handshake and
//! `session/new` both answer regardless of login state; Hermes' `session/new`
//! eagerly constructs the underlying LLM client and fails outright
//! (`-32603`, `"No LLM provider configured"`) on a machine with no provider
//! selected via `hermes model`/`hermes setup`. `AcpSession::open_for_discovery`
//! already tolerates a failed `session/new` (falls back to `initialize` alone),
//! which is exactly what makes an unconfigured Hermes degrade to the curated
//! model list rather than erroring the picker.
//!
//! **No live turn was captured.** This machine has no LLM provider configured
//! for Hermes, and `hermes model`/`hermes setup` are interactive-terminal-only
//! (refuse to run under a piped subprocess), so the picker's OAuth/API-key
//! flow could not be driven here. A second attempt with a fabricated
//! `OPENROUTER_API_KEY` got further -- past the "no provider" check and into
//! constructing an OpenAI-compatible client -- and crashed on an unrelated
//! Hermes-side bug (`module 'collections' has no attribute 'MutableSet'`, a
//! Python 3.10+ stdlib removal this hermes-agent 0.15.2 install has not
//! patched around). Neither is a Comet defect. Where this file relies on
//! Hermes' behaviour beyond the handshake (the model-list shape, the usage
//! reader), the doc comments say so and point at Hermes' own installed
//! source as the evidence in place of a capture -- see the PR1 task report.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde_json::{Value, json};

use comet_proto::{
    AgentCommand, AgentEvent, HarnessCapabilities, HarnessId, HarnessProbe, InstallMethod, Model,
    ModelCatalog, RunRequest, RuntimeMode, SteeringMode,
};

use super::AgentDescription;
use super::normalize;
use super::session::{AcpSession, Discovered, Timeouts};
use crate::discovery::{DiscoveredModel, Discovery, DiscoveryFailure};
use crate::launch::{LaunchDescriptor, StdioMode};
use crate::{Harness, HarnessError, RunControls};

/// Hermes' ACP entry point: `hermes acp`, and nothing else.
///
/// Verified against hermes-agent 0.15.2 on 2026-08-28: `--accept-hooks`,
/// `--setup` and `--setup-browser` all change what the process does, and
/// `--check` / `--version` exit without opening a session at all.
pub const HERMES_ARGS: [&str; 1] = ["acp"];

/// Locate the device's installed Hermes CLI: `HERMES_EXECUTABLE`, then our own
/// PATH, then the system's persisted PATH, then known install locations. Same
/// ladder and the same reasons as [`crate::codex::resolve_codex_executable`].
///
/// **On this machine `hermes` resolves to a Python-scripts shim**
/// (`...\Python314\Scripts\hermes.exe`), so PATH is what actually answers
/// here -- confirming PATH has to stay the first place looked, not an
/// optimization to drop in favor of the fixed install dirs below.
pub fn resolve_hermes_executable() -> Option<PathBuf> {
    crate::resolve_cli(
        "HERMES_EXECUTABLE",
        "hermes",
        crate::all_known_dirs(hermes_install_dirs()),
    )
}

/// Where a Hermes CLI lands when PATH does not name it: `~/.local/bin`, then
/// `~/.hermes/bin`, in that order -- for POSIX installs; a pip/pipx install on
/// PATH (as on this machine) never reaches this list at all.
fn hermes_install_dirs() -> Vec<crate::KnownDir> {
    let mut dirs: Vec<crate::KnownDir> = Vec::new();
    if let Some(home) = crate::home_dir() {
        dirs.push((home.join(".local").join("bin"), InstallMethod::Native));
        dirs.push((home.join(".hermes").join("bin"), InstallMethod::Native));
    }
    if !cfg!(windows) {
        dirs.push((PathBuf::from("/opt/homebrew/bin"), InstallMethod::Homebrew));
        // Untagged for the same reason as the other providers' lists:
        // `/usr/local/bin` is Intel Homebrew, a manual copy, and several
        // installers' fallback all at once.
        dirs.push((PathBuf::from("/usr/local/bin"), InstallMethod::Unknown));
    }
    dirs
}

/// Describe the exact process launch used for a Hermes run.
///
/// Production's builder, and the one the capture recorder's Hermes rows would
/// spawn -- the same seam `grok::run_launch` sits on. The request contributes
/// nothing to argv today: model selection is not wired to the ACP wire for
/// any agent yet (`crates/harness/src/acp/session.rs` never sends a model
/// choice), so this takes the request only to match the launch seam
/// `capture::record::derive_launch` calls.
pub(crate) fn run_launch(exe: &Path, _request: &RunRequest) -> LaunchDescriptor {
    LaunchDescriptor {
        program: crate::discovery::program_path(exe),
        args: HERMES_ARGS.iter().map(Into::into).collect(),
        cwd: None,
        configured_env: std::collections::BTreeMap::new(),
        stdin: StdioMode::Piped,
        stdout: StdioMode::Piped,
        stderr: StdioMode::Piped,
        kill_on_drop: true,
        #[cfg(windows)]
        creation_flags: 0,
    }
}

/// The curated list, used when discovery cannot run or cannot be read.
///
/// **Its job is to never be empty.** `DiscoveryCache::catalog` falls back to
/// this on any failure -- and on an unconfigured Hermes, that is EVERY
/// failure, because `session/new` cannot succeed without a provider selected
/// (see this module's header). A picker showing "this agent has no models"
/// when the truth is "Hermes has not been set up yet" is the confident-wrong-
/// answer shape this repository has hit twice.
///
/// One entry: `gpt-5.4-mini`, the cheapest model in Hermes' own OpenAI/Codex
/// catalog (`hermes_cli/codex_models.py`, read from the installed package —
/// not a capture, since no session ever reached a model choice on this
/// machine). `accepts_images: true` comes from the real captured `initialize`
/// reply: `agentCapabilities.promptCapabilities.image` read `true` on
/// hermes-agent 0.15.2, 2026-08-28.
///
/// **The id is `openai:gpt-5.4-mini`, in `provider:model` form, not the bare
/// model name.** Hermes encodes every wire `modelId` this way
/// (`_encode_model_choice`, `acp_adapter/server.py:568-576`, consumed by
/// `_build_model_state` at `:595` and `:612`) — `models_from_discovery`'s own
/// fixtures below assume it. `discovery::merge` dedupes by exact id equality
/// (`discovery.rs:102,112`), so a curated entry in the bare-name space would
/// never match a live row for the same model and would duplicate it in the
/// picker instead of enriching it — see
/// `the_curated_id_matches_the_encoded_space_discovery_uses` below.
fn static_models() -> Vec<Model> {
    vec![Model {
        id: "openai:gpt-5.4-mini".into(),
        label: "GPT-5.4 Mini".into(),
        description: Some("OpenAI's compact model, routed through Hermes".into()),
        reasoning_levels: Vec::new(),
        options: Vec::new(),
        accepts_images: true,
    }]
}

/// The model list, read off `session/new`'s ACP-spec-shape `models` block.
///
/// **No vendor `_meta` config surface, unlike Grok's
/// `_meta["x.ai/sessionConfig"]` — and no `configOptions` either.** But
/// `session/new` is not bare: Hermes' installed `acp_adapter.server` DOES
/// return a `modes` block (`new_session` returns `modes=self._session_modes
/// (state)`, `server.py:1083`), and that method's own docstring
/// (`:530-536`) says plainly what it is for: "Hermes maps edit approval
/// policy onto modes instead of advertising config options." `modes` is
/// Hermes' edit-approval-policy selector, not a model or effort surface — see
/// `HermesHarness::capabilities()` for what that mapping means for approvals
/// — so it has nothing this function needs to read.
///
/// The one surface this function DOES read is the spec's own (unstable)
/// `models.availableModels[].modelId/.name` and `models.currentModelId`
/// (`acp_adapter.server._build_model_state`, source, not a capture -- see
/// this module's header), the same top-level path Grok itself falls back to
/// when its own vendor surface is absent
/// (`grok::models_from_discovery`'s "no config surface" branch). Reusing that
/// reading here is not a guess at Hermes' shape; it is the one path Hermes'
/// own source is confirmed to write.
///
/// Tolerant throughout, matching Grok's discovery: an entry without an id is
/// skipped, and an agent with no `models` block at all yields an empty
/// [`Discovery`] rather than an error -- "listed nothing" and "could not be
/// reached" stay different answers.
fn models_from_discovery(discovered: &Discovered) -> Discovery {
    let entries = discovered.session["models"]["availableModels"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    // The handshake's own answer, agent-wide -- there is no per-model
    // modality surface here either, but the agent as a whole DOES say:
    // Hermes' real 2026-08-28 reply carries
    // `agentCapabilities.promptCapabilities.image: true`. Read once and
    // applied identically to every discovered row, the same propagation
    // Grok's own `models_from_discovery` does.
    let image_support =
        AgentDescription::from_initialize(&discovered.initialized).image_attachments;
    Discovery {
        models: entries
            .iter()
            .filter_map(|m| {
                let id = m["modelId"].as_str()?.to_owned();
                let label = m["name"].as_str().unwrap_or(&id).to_owned();
                Some(DiscoveredModel {
                    label,
                    description: m["description"].as_str().map(str::to_owned),
                    // Hermes offers no effort ladder anywhere in its ACP
                    // surface -- see `HermesHarness::capabilities()`.
                    reasoning_levels: Vec::new(),
                    accepts_images: image_support,
                    id,
                })
            })
            .collect(),
    }
}

/// What to send to apply the caller's model choice to a freshly opened
/// session, as a single `session/set_model` request. **Hermes' own installed
/// source implements the ACP spec's dedicated setter**
/// (`acp_adapter/server.py:1882`, `set_session_model`, under its own comment
/// "Model switching (ACP protocol method)") -- a source read, not a capture:
/// no live turn ever reached a model choice on this machine (this module's
/// header).
///
/// **The same method Grok's own `config_requests` sends, it turns out** — a
/// live probe against the real grok 1.0.5 CLI found `session/set_model`
/// working there too, and found no working `session/set_config_option` at
/// all (`grok::config_requests`'s own doc comment carries the probe
/// evidence and the design correction that produced it). The two functions
/// are not merged into one shared implementation even so: today's agreement
/// is a fact about these two CLIs, not a protocol guarantee, and each
/// function's own evidence is worth keeping next to the code it justifies.
///
/// **No effort is ever sent, unconditionally.** `request.reasoning` is never
/// read here at all -- Hermes advertises no effort ladder anywhere in its ACP
/// surface (`HermesHarness::capabilities()`), and sending one anyway would be
/// an error or silently ignored, both worse than not sending it.
pub(crate) fn config_requests(
    request: &RunRequest,
    session_id: &str,
) -> Vec<(&'static str, Value)> {
    match &request.model {
        Some(model) => vec![(
            "session/set_model",
            json!({"sessionId": session_id, "modelId": model}),
        )],
        None => Vec::new(),
    }
}

/// Recognizes Hermes' own "not configured yet" shape on `session/new` and
/// turns it into a clean instruction, in place of the raw JSON-RPC text
/// [`HarnessError::Protocol`] would otherwise carry to the user
/// (`.agents/rules/user-facing-errors.md`).
///
/// **Verified live against hermes-agent 0.15.2 on 2026-08-29**, run with no
/// provider configured (this module's own header has the fuller account):
/// `initialize` answers normally — `authMethods: [{"id": "hermes-setup",
/// "name": "Configure Hermes provider", "type": "terminal", "args":
/// ["--setup"], "description": "Open Hermes' interactive model/provider
/// setup in a terminal…"}]` — and `session/new` fails with `{"code":
/// -32603, "message": "Internal error", "data": {"details": "No LLM
/// provider configured. Run \`hermes model\` to select a provider, or run
/// \`hermes setup\` for first-time configuration."}}`. **The actionable text
/// lives in `data.details`, not `message`** — `message` alone is the
/// useless generic "Internal error" — which is exactly the case
/// `jsonrpc.rs`'s error decode was widened for (see its own comment): after
/// that fold, this arrives as `HarnessError::Protocol("session/new:
/// Internal error: No LLM provider configured. Run \`hermes model\`…")`,
/// which is what the `msg.contains(...)` check below matches on.
///
/// **Same category as Grok's signed-out case, deliberately generalized as
/// one variant rather than two.** The two ARE different underlying states —
/// Grok has never been signed in at all; a from-scratch Hermes install has
/// no LLM provider selected, which is closer to first-run setup than to
/// "signed out" — but both answer the same user-facing question ("this
/// agent cannot run yet; go do one more thing in its own CLI"), so
/// [`HarnessError::NeedsSetup`] carries either without inventing a second
/// variant that would only ever differ in wording.
///
/// **Step 5's decision, same as Grok's: Comet never calls ACP's own
/// `authenticate` method.** Hermes' own advertised method is `type:
/// "terminal"` with `args: ["--setup"]` — not `_meta: {headless: true}`, and
/// not key-based — so it needs an interactive terminal the same way `grok
/// login`'s OAuth flow does. Launching it from a background discovery probe
/// would be the same surprise either way; the hint sends the user to run
/// `hermes model`/`hermes setup` themselves instead, the two commands
/// Hermes' own error text names.
pub(crate) fn map_open_failure(error: &HarnessError) -> Option<HarnessError> {
    match error {
        HarnessError::Protocol(msg) if msg.contains("No LLM provider configured") => {
            Some(HarnessError::NeedsSetup {
                summary: "Setup required".into(),
                hint: "Run `hermes model` to select a provider, or `hermes setup` for \
                       first-time configuration, then try again."
                    .into(),
            })
        }
        _ => None,
    }
}

/// One short-lived ACP session, just to read the handshake and `session/new`.
///
/// Token-free on Grok; on an unconfigured Hermes `session/new` fails outright
/// (see this module's header), and `open_for_discovery` already turns that
/// into `Discovered { session: Null, .. }` rather than an error -- so this
/// still resolves, just with nothing beyond `initialize` to read.
async fn probe_session(
    exe: &Path,
    cwd: &str,
    timeouts: Timeouts,
) -> Result<Discovered, DiscoveryFailure> {
    let request = RunRequest::for_session(RuntimeMode::default());
    let command = run_launch(exe, &request).command();
    AcpSession::open_for_discovery(command, cwd, timeouts)
        .await
        .map_err(|error| {
            tracing::debug!(target: "comet_harness::acp", "hermes discovery failed: {error}");
            // Every failure here is `Unreachable`, not `Unparseable`: the
            // handshake either answered or it did not, and reading the reply
            // cannot fail -- an unrecognized shape yields an empty list, which
            // is a real answer. `Unparseable` is reserved for a provider that
            // answered something we could not decode.
            DiscoveryFailure::Unreachable
        })
}

async fn discover(exe: PathBuf, timeouts: Timeouts) -> Result<Discovery, DiscoveryFailure> {
    let cwd = std::env::temp_dir().to_string_lossy().into_owned();
    match probe_session(&exe, &cwd, timeouts).await {
        Ok(discovered) => Ok(models_from_discovery(&discovered)),
        Err(failure) => Err(failure),
    }
}

/// **Hermes pushes `available_commands_update` on session creation**, per its
/// own source (`acp_adapter.server.new_session` schedules
/// `_send_available_commands_update` immediately). On an unconfigured
/// install `session/new` never gets that far (see this module's header), so
/// this answers empty until Hermes has a provider -- the honest "could not be
/// reached" answer, not an error.
async fn discover_commands(
    exe: PathBuf,
    cwd: String,
    timeouts: Timeouts,
) -> Result<Vec<AgentCommand>, DiscoveryFailure> {
    let discovered = probe_session(&exe, &cwd, timeouts).await?;
    Ok(normalize::commands(&discovered.commands))
}

/// The Hermes harness. Construct with [`HermesHarness::new`]; tests point it
/// at the `fake-acp` fixture with [`HermesHarness::with_executable`].
///
/// The derived `Default` is the real one: `Timeouts` carries its own non-zero
/// defaults, so deriving here picks them up rather than zeroing them.
#[derive(Default)]
pub struct HermesHarness {
    executable: Option<PathBuf>,
    timeouts: Timeouts,
    /// One handshake per boot. `models()` is on the picker's render path AND
    /// is called by titling, so an uncached discovery would spawn an agent on
    /// a path the user never sees.
    discovery_cache: crate::discovery::DiscoveryCache,
    /// One handshake per DIRECTORY per boot. Separate from the model cache
    /// because commands are cwd-scoped and models are not (debt row D32).
    command_cache: crate::discovery::CommandCache,
}

impl HermesHarness {
    pub fn new() -> Self {
        Self::default()
    }

    /// The single declaration of what Hermes can honor, named by both the
    /// registry's lazy descriptor and the trait impl so the two cannot drift.
    ///
    /// **No steering extension**, confirmed absent from the real `initialize`
    /// reply (hermes-agent 0.15.2, captured 2026-08-28): no `_meta.steering`
    /// anywhere in the result. Pinned against production's own decode of that
    /// literal in `the_captured_initialize_reply_has_no_steering` below, via
    /// `AgentDescription::from_initialize`. A steer is therefore delivered as
    /// the next prompt on the same session -- slower than an in-turn steer
    /// and correct.
    ///
    /// **No effort ladder**, on two different kinds of evidence. The
    /// handshake carries none: the captured `initialize` reply has no
    /// reasoning/effort vocabulary anywhere. The session config carries none
    /// either, but that half is a SOURCE read, not a capture -- no
    /// `session/new` reply was ever obtained (this module's header) -- and
    /// rests on `acp.schema.SessionModelState` (the type
    /// `_build_model_state` returns) having no effort-level field at all. A
    /// populated ladder here would be a promise the run breaks.
    pub fn capabilities() -> HarnessCapabilities {
        HarnessCapabilities {
            supports_steering: true,
            steering_mode: SteeringMode::TurnBoundary,
            reasoning_levels: Vec::new(),
            // **One mode, deliberately — a PR6 review correction, not the
            // original call.** A first draft of this branch widened to
            // `[ApprovalRequired, FullAccess]` on source evidence (below,
            // still true) that Hermes' own code asks unconditionally about
            // dangerous commands and by default about edits. Review caught
            // the flaw: `crates/harness/src/acp/session.rs` reads
            // `RunRequest.runtime_mode` NOWHERE except as the
            // `SessionStarted` event field, and `.sandbox()` nowhere at all —
            // no launch flag (contrast `claude::run_launch`'s
            // `--permission-mode`, `codex/mod.rs`'s approval policy), no
            // `session/new` param, no `session/set_mode` call. So
            // `ApprovalRequired` and `FullAccess` are BYTE-IDENTICAL on the
            // wire for Hermes: whichever the user picks, Hermes runs its own
            // fixed default regardless, which makes at least one of the two
            // declarations false by construction, not merely optimistic.
            //
            // Concretely, neither promise holds. `ApprovalRequired` means
            // "every tool call is asked about first" under a `ReadOnly`
            // sandbox — the picker's own copy reads "Every file change and
            // command waits for you". Hermes asks only about commands its own
            // classifier flags dangerous, plus edits; everything else it
            // considers ordinary runs unasked, and the ACP path applies no
            // sandbox at all. `FullAccess` means "no sandbox and no
            // approvals" — also false, because the same dangerous-command and
            // edit asks fire regardless of what Comet declares. A user
            // picking either gets neither of the two things it promises.
            //
            // What was established and is worth keeping (source-read, not a
            // capture — no live Hermes session has ever been obtained on this
            // machine, this module's header): `acp_adapter/permissions.py`'s
            // `make_approval_callback` is wired into `terminal_tool`'s
            // approval callback unconditionally for every ACP turn
            // (`server.py`'s `_run_agent`), with no policy check anywhere in
            // the call path — a dangerous command always asks. A file edit
            // asks under the session's `mode`, which defaults to `"default"`
            // -> edit-approval policy `"ask"` (`_MODE_TO_EDIT_APPROVAL_POLICY`,
            // `_session_modes` falling back to it whenever unset); two OTHER
            // modes exist on the wire (`accept_edits` -> `workspace_session`,
            // `dont_ask` -> `session`) that this crate has no way to select
            // yet (`session/set_mode` is the session-open path PR7 owns).
            // This is real evidence about Hermes' posture, not proof that
            // Comet can honor a mode built on it — recorded as debt row D104
            // (`docs/debt/README.md`) rather than acted on here, so the next
            // person with a working Hermes install widens on confirmation
            // instead of re-deriving this source read.
            //
            // `AutoAcceptEdits`/`Auto` stay off regardless of the above, and
            // permanently rather than "until PR7 wires `session/set_mode`":
            // Hermes' command-approval path is unconditional in its own code
            // with no session mode able to skip it, so `AutoAcceptEdits`'s
            // contract ("nothing able to block on a question") can never be
            // true for Hermes.
            runtime_modes: vec![RuntimeMode::FullAccess],
            // Neither `PermissionOption` nor the `outcome` ACP defines
            // carries a note field, and Hermes' own option builders
            // (`_build_permission_options`, `_build_permission_tool_call`)
            // send nothing a message could ride in either — checked against
            // source per Step 5's instruction, not assumed (D24).
            carries_deny_note: false,
        }
    }

    /// Use a fixed CLI binary instead of PATH/known-location resolution.
    pub fn with_executable(mut self, path: impl Into<PathBuf>) -> Self {
        self.executable = Some(path.into());
        // A cached answer belongs to the CLI that gave it; replayed for a
        // binary that was never asked, it would show one agent's models under
        // another's name.
        self.discovery_cache = crate::discovery::DiscoveryCache::default();
        self
    }

    /// Shrink the handshake and reap bounds. Tests use it; running turns are
    /// unbounded either way.
    pub fn with_timeouts(mut self, timeouts: Timeouts) -> Self {
        self.timeouts = timeouts;
        self
    }

    fn resolve_executable(&self) -> Result<PathBuf, HarnessError> {
        if let Some(p) = &self.executable {
            return Ok(p.clone());
        }
        resolve_hermes_executable().ok_or_else(|| {
            HarnessError::NotInstalled(crate::not_installed_message("hermes", "HERMES_EXECUTABLE"))
        })
    }
}

#[async_trait]
impl Harness for HermesHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Hermes
    }

    fn display_name(&self) -> &str {
        // Matches the registry's lazy descriptor, so the catalog entry does
        // not change after the first resolve.
        "Hermes"
    }

    fn capabilities(&self) -> HarnessCapabilities {
        Self::capabilities()
    }

    async fn probe(&self) -> HarnessProbe {
        crate::probe_installed_cli(
            self.resolve_executable(),
            "hermes",
            "HERMES_EXECUTABLE",
            crate::all_known_dirs(hermes_install_dirs()),
        )
        .await
    }

    /// The curated catalog unioned with whatever the handshake reported.
    ///
    /// An absent CLI surfaces as [`HarnessError::NotInstalled`] rather than a
    /// failed discovery: the user's action is different, and the picker's
    /// built-in caption is not the place to say "no CLI".
    async fn models(&self) -> Result<ModelCatalog, HarnessError> {
        let exe = self.resolve_executable()?;
        let timeouts = self.timeouts;
        let curated = static_models();
        let discovery = self
            .discovery_cache
            .get(move || discover(exe, timeouts))
            .await;
        Ok(self.discovery_cache.catalog(curated, discovery))
    }

    /// The `/` menu for `cwd`.
    ///
    /// Overridden rather than left at the default empty list: Hermes' source
    /// confirms it really does push a command list on session creation (see
    /// this module's header), so the default would present a working surface
    /// as absent once the CLI is configured. An unreachable agent answers an
    /// empty list rather than an error.
    async fn commands(&self, cwd: &str) -> Result<Vec<AgentCommand>, HarnessError> {
        let exe = self.resolve_executable()?;
        let timeouts = self.timeouts;
        let owned_cwd = cwd.to_owned();
        Ok(self
            .command_cache
            .get(cwd, move || discover_commands(exe, owned_cwd, timeouts))
            .await
            .unwrap_or_default())
    }

    fn clear_discovery(&self) {
        self.discovery_cache.clear();
        // The Retry row clears both: a user who hits it after fixing an
        // install means "ask again", not "ask again about models only".
        self.command_cache.clear();
    }

    fn take_unreported_discovery_failure(&self) -> Option<DiscoveryFailure> {
        self.discovery_cache.take_unreported_failure()
    }

    async fn run(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let exe = self.resolve_executable()?;
        let command = run_launch(&exe, &request).command();
        let session = AcpSession::open(
            command,
            &request.cwd,
            self.timeouts,
            &request,
            config_requests,
            map_open_failure,
        )
        .await?;
        Ok(super::session::run(
            session,
            HarnessId::Hermes,
            request,
            controls,
            // Hermes' own numbers live at the ACP spec's own top-level
            // `usage` block, which is exactly what `normalize::usage` reads
            // -- see its doc comment for why that is genuinely spec-general
            // rather than a second vendor path.
            normalize::usage,
        ))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::acp::AgentDescription;

    /// Break caught: reordering or adding to Hermes' launch tokens.
    #[test]
    fn the_launch_line_is_exactly_hermes_acp() {
        assert_eq!(HERMES_ARGS, ["acp"]);
        let launch = run_launch(
            Path::new("/usr/local/bin/hermes"),
            &RunRequest::for_session(RuntimeMode::default()),
        );
        assert_eq!(
            launch.args,
            HERMES_ARGS
                .iter()
                .map(std::ffi::OsString::from)
                .collect::<Vec<_>>()
        );
    }

    /// The real `initialize` reply hermes-agent 0.15.2 sent on 2026-08-28,
    /// from the UNCONFIGURED run (`raw-session.jsonl`) -- no provider
    /// selected, `authMethods` a single `hermes-setup` entry pointing at
    /// `--setup`. (The OpenRouter-provider run, `raw-session-2.jsonl`, is a
    /// different literal with two `authMethods` entries; this one is not
    /// it.) `agentCapabilities.promptCapabilities.image: true`, and no
    /// `_meta.steering` anywhere in the result.
    ///
    /// **Routed through production's own decode, not just indexed as raw
    /// JSON.** `AgentDescription::from_initialize` (`acp/mod.rs`, private to
    /// `acp` and therefore reachable from this child module) is the actual
    /// function that turns this literal into `HermesHarness::capabilities()`
    /// 's `SteeringMode::TurnBoundary` -- so this is a test that can go red
    /// if that decode ever changes, not a description of the literal it
    /// wrote three lines above itself.
    #[test]
    fn the_captured_initialize_reply_has_no_steering() {
        let initialized = json!({
            "agentCapabilities": {
                "loadSession": true,
                "promptCapabilities": {"image": true},
                "sessionCapabilities": {"fork": {}, "list": {}, "resume": {}},
            },
            "agentInfo": {"name": "hermes-agent", "version": "0.15.2"},
            "authMethods": [{
                "args": ["--setup"],
                "description": "Open Hermes' interactive model/provider setup \
                                 in a terminal. Use this when Hermes has not \
                                 been configured on this machine yet.",
                "id": "hermes-setup",
                "name": "Configure Hermes provider",
                "type": "terminal",
            }],
            "protocolVersion": 1,
        });

        assert_eq!(
            initialized["agentCapabilities"]["promptCapabilities"]["image"],
            true
        );

        let agent = AgentDescription::from_initialize(&initialized);
        assert_eq!(
            agent.steering, None,
            "the real reply carries no _meta.steering at all"
        );
        assert!(
            !agent.supports_steering(),
            "absent steering must decode as unknown, not as enabled"
        );
    }

    /// The handshake half of the "no effort ladder" claim: no reasoning/
    /// effort vocabulary anywhere in the real `initialize` reply. The OTHER
    /// half -- that `session/new`'s own model surface carries no effort
    /// field either -- is a source read of `acp.schema.SessionModelState`,
    /// not a capture (see `HermesHarness::capabilities()`'s doc comment), and
    /// is not what this test checks.
    #[test]
    fn the_captured_initialize_reply_has_no_effort_vocabulary() {
        let initialized = json!({
            "agentCapabilities": {
                "loadSession": true,
                "promptCapabilities": {"image": true},
                "sessionCapabilities": {"fork": {}, "list": {}, "resume": {}},
            },
            "agentInfo": {"name": "hermes-agent", "version": "0.15.2"},
            "protocolVersion": 1,
        });
        assert!(
            initialized.get("reasoningEfforts").is_none(),
            "no effort vocabulary anywhere in the handshake"
        );
    }

    /// **The model × spec-path trap, the other way from Grok's.** Grok's own
    /// `session.models.availableModels` is its DEPRECATED surface; here it is
    /// the only one Hermes ever populates (see this module's header), so
    /// reading it must not be skipped the way Grok skips it when a richer
    /// surface is present.
    #[test]
    fn the_spec_shaped_models_block_is_read() {
        let discovered = Discovered {
            initialized: json!({}),
            session: json!({
                "models": {
                    "currentModelId": "openai:gpt-5.4-mini",
                    "availableModels": [
                        {"modelId": "openai:gpt-5.4-mini", "name": "gpt-5.4-mini",
                         "description": "Provider: OpenAI • current"},
                        {"modelId": "openai:gpt-5.5", "name": "gpt-5.5"},
                    ],
                },
            }),
            ..Default::default()
        };

        let models = models_from_discovery(&discovered).models;
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "openai:gpt-5.4-mini");
        assert_eq!(models[0].label, "gpt-5.4-mini");
        assert_eq!(
            models[0].description.as_deref(),
            Some("Provider: OpenAI • current")
        );
        assert!(
            models[0].reasoning_levels.is_empty(),
            "Hermes offers no per-model ladder either"
        );
    }

    /// **The handshake's own answer, propagated.** There is no per-model
    /// modality surface here either, but the agent as a whole DOES say --
    /// hermes-agent 0.15.2's real captured `initialize` reply carries
    /// `agentCapabilities.promptCapabilities.image: true` -- and every
    /// discovered model now carries that same agent-wide answer, PR7 onward.
    #[test]
    fn discovered_models_carry_the_handshakes_image_capability() {
        let discovered = Discovered {
            initialized: json!({
                "agentCapabilities": {"promptCapabilities": {"image": true}},
            }),
            session: json!({"models": {"availableModels": [
                {"modelId": "openai:gpt-5.4-mini", "name": "gpt-5.4-mini"},
            ]}}),
            ..Default::default()
        };
        let models = models_from_discovery(&discovered).models;
        assert_eq!(
            models[0].accepts_images,
            Some(true),
            "the handshake said true, so the discovered row must carry that, not None"
        );
    }

    /// An unreadable or absent `models` block is an empty list, never an
    /// error: "listed nothing" and "could not be reached" are different
    /// answers, and this is what an unconfigured Hermes always yields (see
    /// this module's header on `session/new`'s auth requirement).
    #[test]
    fn absent_or_unreadable_models_are_empty_rather_than_a_failure() {
        for session in [
            json!({}),
            json!({"models": {}}),
            json!({"models": {"availableModels": []}}),
            json!({"models": {"availableModels": "not-an-array"}}),
            // `session/new` failing outright, as it does when no provider is
            // configured: `open_for_discovery` substitutes `Null`.
            Value::Null,
        ] {
            let discovered = Discovered {
                initialized: json!({}),
                session: session.clone(),
                ..Default::default()
            };
            assert!(
                models_from_discovery(&discovered).models.is_empty(),
                "must be empty: {session}"
            );
        }
    }

    /// A row with no id is skipped rather than poisoning the list.
    #[test]
    fn a_row_without_an_id_is_skipped() {
        let discovered = Discovered {
            initialized: json!({}),
            session: json!({"models": {"availableModels": [
                {"name": "No Id At All"},
                {"modelId": "openai:gpt-5.4-mini"},
            ]}}),
            ..Default::default()
        };
        let models = models_from_discovery(&discovered).models;
        assert_eq!(models.len(), 1, "the id-less row is skipped");
        assert_eq!(models[0].id, "openai:gpt-5.4-mini");
        assert_eq!(
            models[0].label, "openai:gpt-5.4-mini",
            "a row with no name falls back to its id rather than an empty string"
        );
    }

    /// **The curated list must never be empty**, because it is what the
    /// picker shows when discovery fails -- which is EVERY discovery on an
    /// unconfigured Hermes install (see this module's header).
    #[test]
    fn the_curated_list_is_never_empty() {
        let curated = static_models();
        assert!(!curated.is_empty());
        assert!(curated.iter().all(|m| !m.id.is_empty()));

        let cache = crate::discovery::DiscoveryCache::default();
        let catalog = cache.catalog(static_models(), Err(DiscoveryFailure::Unreachable));
        assert_eq!(catalog.source, comet_proto::CatalogSource::BuiltIn);
        assert!(
            !catalog.models.is_empty(),
            "a discovery failure must not degrade to an empty list"
        );
    }

    /// **The curated id must live in the same space discovery uses, or the
    /// picker shows the same model twice.** Hermes encodes every wire
    /// `modelId` as `provider:model` (`_encode_model_choice`,
    /// `acp_adapter/server.py:568-576`, consumed by `_build_model_state` at
    /// `:595` and `:612`) -- `openai:gpt-5.4-mini`, never the bare
    /// `gpt-5.4-mini`. `discovery::merge` dedupes by exact id equality
    /// (`discovery.rs:102,112`): a curated row and a live row with DIFFERENT
    /// ids never match, so `merge` treats them as two different models and
    /// appends the live one alongside the curated one instead of folding it
    /// in. Break caught: reverting `static_models()`'s id to the bare form
    /// makes `merge` return TWO rows here -- the curated `gpt-5.4-mini` and
    /// the discovered `openai:gpt-5.4-mini` -- because they no longer share
    /// an id for `merge` to fold on. Asserting `merged.len() == 1` is what
    /// catches that; counting only the rows that already carry the encoded
    /// id would not -- the stray bare-id row would simply go uncounted, and
    /// the assertion would pass with the bug present.
    #[test]
    fn the_curated_id_matches_the_encoded_space_discovery_uses() {
        let discovered = Discovered {
            initialized: json!({}),
            session: json!({"models": {"availableModels": [
                {"modelId": "openai:gpt-5.4-mini", "name": "GPT-5.4 Mini (live)"},
            ]}}),
            ..Default::default()
        };
        let discovery = models_from_discovery(&discovered);
        let merged = crate::discovery::merge(static_models(), &discovery);

        assert_eq!(
            merged.len(),
            1,
            "the curated and discovered rows must merge into ONE row, not two: {merged:#?}"
        );
        assert_eq!(
            merged[0].id, "openai:gpt-5.4-mini",
            "the surviving row must carry the encoded id, not the bare one"
        );
    }

    /// Both agents' own usage reader, side by side. **Placed here rather than
    /// in `crates/harness/tests/hermes.rs`, where the task brief first put
    /// it**: `grok::usage` and `normalize::usage` are `pub(crate)` inside a
    /// `pub(crate) mod normalize`, invisible to an external integration-test
    /// crate. Every other literal-wire-pinned decode test in this codebase
    /// already lives beside its function for exactly that reason — see
    /// `tests/grok.rs`'s own doc comment ("the unit tests in `acp/grok.rs`
    /// pin the decode against literal wire; these prove the harness actually
    /// reaches it"). Widening `normalize` to `pub` just to move this one test
    /// would be a bigger, unrelated API change for no behavioural gain.
    ///
    /// Grok's half is the real captured `session/prompt` reply (grok 1.0.5,
    /// 2026-08-28, pinned identically in `grok.rs`'s own tests). Hermes' half
    /// is hand-built from its installed `acp`/`acp_adapter` source, not a
    /// capture — see this module's header for why no live Hermes turn could
    /// be run here. `prompt_tokens` is `inputTokens`, never `totalTokens`,
    /// for both: Grok's `inputTokens` is cache-INCLUSIVE (14500 with 10624
    /// cached), which is what `AgentEvent::Usage` means by it; `totalTokens`
    /// accumulates input + output, so metering it shows a session filling
    /// from its own replies.
    #[test]
    fn each_agent_usage_shape_maps_to_the_same_event() {
        let grok_result = json!({
            "stopReason": "end_turn",
            "_meta": {
                "totalTokens": 14671,
                "modelId": "grok-4.6",
                "inputTokens": 14500,
                "outputTokens": 171,
                "cachedReadTokens": 10624,
                "reasoningTokens": 169,
                "usage": {"numTurns": 1, "modelCalls": 1},
            },
        });
        // Hand-built to match Hermes' installed `acp` schema (top-level
        // `usage.{inputTokens,outputTokens,totalTokens,...}`), using the same
        // numbers as Grok's real reply so the two cases are directly
        // comparable. Not a wire capture.
        let hermes_result = json!({
            "stopReason": "end_turn",
            "usage": {
                "inputTokens": 14500,
                "outputTokens": 171,
                "totalTokens": 14671,
                "cachedReadTokens": 10624,
                "thoughtTokens": 169,
            },
        });

        let grok_usage = crate::acp::grok::usage(&grok_result, Some(500_000))
            .expect("grok's vendor _meta reading");
        let hermes_usage = normalize::usage(&hermes_result, Some(500_000))
            .expect("hermes' spec-shaped top-level reading");

        assert_eq!(
            grok_usage, hermes_usage,
            "equal numbers from each agent's own reader land on the same AgentEvent"
        );
        match grok_usage {
            AgentEvent::Usage {
                prompt_tokens,
                output_tokens,
                context_window,
            } => {
                assert_eq!(prompt_tokens, 14500, "the prompt, cache included");
                assert_eq!(output_tokens, 171);
                assert_eq!(context_window, Some(500_000));
                assert_ne!(prompt_tokens, 14671, "not totalTokens for either agent");
            }
            other => panic!("{other:?}"),
        }
    }

    /// Hermes declares only `FullAccess`, and no effort ladder at all -- both
    /// honest answers, not gaps. `ApprovalRequired` would be dishonest in the
    /// specific way D104 records: nothing in the ACP path reads
    /// `runtime_mode` at all, so it would be byte-identical to `FullAccess`
    /// on the wire while promising a `ReadOnly` sandbox and "everything
    /// asked" that Hermes' own unconditional command-approval/edit-default
    /// behaviour does not match either declaration honestly.
    #[test]
    fn capabilities_declare_no_ladder_and_one_runtime_mode() {
        let capabilities = HermesHarness::capabilities();
        assert_eq!(capabilities.runtime_modes, vec![RuntimeMode::FullAccess]);
        assert!(capabilities.reasoning_levels.is_empty());
        assert_eq!(
            capabilities.steering_mode,
            SteeringMode::TurnBoundary,
            "Hermes advertises no steering extension, so the boundary is the honest answer"
        );
    }

    /// Break caught: surfacing a raw protocol error to a user whose Hermes
    /// install has no provider configured. `.agents/rules/user-facing-errors.md`:
    /// the user never sees `err.to_string()`, and every failure splits into
    /// a short summary and an actionable hint with the diagnostic detail
    /// left in `tracing`.
    ///
    /// The input is the literal `HarnessError::Protocol` text production
    /// builds from Hermes' real "no provider configured" `session/new`
    /// reply (captured live 2026-08-29, see `map_open_failure`'s own doc
    /// comment) after `jsonrpc.rs` folds the error's `data.details` onto its
    /// `message`.
    #[test]
    fn an_unconfigured_hermes_asks_the_user_to_run_setup() {
        let raw = HarnessError::Protocol(
            "session/new: Internal error: No LLM provider configured. Run `hermes model` to \
             select a provider, or run `hermes setup` for first-time configuration."
                .into(),
        );
        let mapped = map_open_failure(&raw).expect("Hermes' unconfigured shape must be recognized");
        let HarnessError::NeedsSetup { summary, hint } = mapped else {
            panic!("expected NeedsSetup, got {mapped:?}");
        };
        assert!(
            !summary.contains("-326"),
            "no protocol codes on screen: {summary}"
        );
        assert!(!summary.to_lowercase().contains("jsonrpc"), "{summary}");
        assert!(
            hint.contains("hermes"),
            "the hint must name the command to run: {hint}"
        );
    }

    /// Same guard as Grok's mapper: a `session/new` failure that is not the
    /// unconfigured shape must pass through unchanged.
    #[test]
    fn an_unrecognized_open_failure_is_left_alone() {
        let raw = HarnessError::Protocol("session/new: some other failure".into());
        assert!(
            map_open_failure(&raw).is_none(),
            "an unrecognized failure must not be reclassified"
        );
    }
}
