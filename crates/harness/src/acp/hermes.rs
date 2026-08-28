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

use comet_proto::{
    AgentCommand, AgentEvent, HarnessCapabilities, HarnessId, HarnessProbe, InstallMethod, Model,
    ModelCatalog, RunRequest, RuntimeMode, SteeringMode,
};

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
fn static_models() -> Vec<Model> {
    vec![Model {
        id: "gpt-5.4-mini".into(),
        label: "GPT-5.4 Mini".into(),
        description: Some("OpenAI's compact model, routed through Hermes".into()),
        reasoning_levels: Vec::new(),
        options: Vec::new(),
        accepts_images: true,
    }]
}

/// The model list, read off `session/new`'s ACP-spec-shape `models` block.
///
/// **No vendor config surface, unlike Grok's `_meta["x.ai/sessionConfig"]`.**
/// Hermes' installed `acp_adapter.server._build_model_state` (source, not a
/// capture -- see this module's header) populates only the spec's own
/// (unstable) `models.availableModels[].modelId/.name` and
/// `models.currentModelId`, the same top-level path Grok itself falls back to
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
                    // Per-model modality is not in this surface; `None` is
                    // "Hermes did not say", which leaves the curated entry's
                    // answer standing.
                    accepts_images: None,
                    id,
                })
            })
            .collect(),
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
    /// **No steering extension, and no effort ladder -- both confirmed
    /// absent from the real `initialize` reply** (hermes-agent 0.15.2,
    /// captured 2026-08-28): no `_meta.steering` anywhere in the result, and
    /// no reasoning/effort vocabulary in the handshake or the session config.
    /// A steer is therefore delivered as the next prompt on the same session
    /// -- slower than an in-turn steer and correct -- and a populated ladder
    /// here would be a promise the run breaks.
    pub fn capabilities() -> HarnessCapabilities {
        HarnessCapabilities {
            supports_steering: true,
            steering_mode: SteeringMode::TurnBoundary,
            reasoning_levels: Vec::new(),
            // **One mode, deliberately.** Same reasoning as Grok: approvals
            // are unrouted until PR6, and every mode that promises to ask is
            // a promise the run cannot keep.
            runtime_modes: vec![RuntimeMode::FullAccess],
            // Nothing to attach a note to while approvals are unrouted.
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
        let session = AcpSession::open(command, &request.cwd, self.timeouts).await?;
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
    /// captured with an OpenRouter provider selected (a fake key, so the
    /// handshake succeeded and the later LLM-client construction did not):
    /// `agentCapabilities.promptCapabilities.image: true`, and — like the
    /// unconfigured run — no `_meta.steering` anywhere in the result.
    ///
    /// This module's `models_from_discovery` reads `session/new`, not
    /// `initialize`; this test exists so the capability facts this file's
    /// doc comments cite have one literal pinning them, matching the style
    /// `grok.rs` and `mod.rs` already use for their own captured replies.
    #[test]
    fn the_captured_initialize_reply_has_no_steering_and_no_ladder() {
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
        assert!(
            initialized["_meta"]["steering"].is_null(),
            "the real reply carries no _meta.steering at all"
        );
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

    /// Hermes declares only `FullAccess` while approvals are unrouted, and no
    /// effort ladder at all -- both are honest answers, not gaps.
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
}
