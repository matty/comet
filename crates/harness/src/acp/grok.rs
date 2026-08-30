//! Grok Build, over ACP.
//!
//! The first harness on [`crate::acp`], and the first agent built ground-up on
//! the protocol rather than adapted onto it. Everything specific to Grok lives
//! here — the launch line, where the CLI installs, the curated catalog, and how
//! its models are discovered. The session loop itself is provider-neutral.
//!
//! **Discovery is a handshake plus one `session/new`, and no turn.** Both are
//! token-free, and the second is where the answer actually lives: Grok's
//! session config — which models it offers, and which effort is selected —
//! arrives at `_meta["x.ai/sessionConfig"]` on that reply and nowhere else.
//!
//! **The model paths are Grok's alone.** codex-acp puts models at
//! `models.availableModels`, claude-agent-acp answers `modes.availableModes`,
//! and the ACP org adapters use a top-level `configOptions` with the effort
//! ladder under `thought_level`. Grok matches none of them: its config is
//! vendor-namespaced and it spells the effort ladder `category: "mode"` — a
//! word another vendor's adapters use for a PERMISSION mode. Reading one shared
//! shape across the four would find nothing, or the wrong thing.
//!
//! **No effort setter was found among the methods tried, verified by
//! probing the real CLI directly (raw JSON-RPC over stdio, grok 1.0.5,
//! 2026-08-29) — not inferred.** `session/set_config_option`, the generic
//! ACP setter whose params shape (`configId` + `value`) matches Grok's flat
//! `category`-keyed option rows, answers `-32601 Method not found`: the
//! method is not registered at all. `session/set_mode` — the ACP spec's own
//! approval-style mode setter — DOES answer, but with `{}` for EVERY
//! `modeId` tried, including a deliberately invalid one
//! (`"not-a-real-mode-xyz"`); a setter that succeeds on garbage input is not
//! validating anything, so a success reply from it is not evidence it did
//! anything. Only `session/set_model` (the ACP spec's own dedicated,
//! unstable method) turned out to be real: it rejects an unknown id
//! (`-32602 Invalid params: unknown model id`) and accepts a real one
//! (`{"_meta": {"model": {"Ok": "<id>"}}}`). [`config_requests`] is built on
//! exactly that finding, not on the `SetSessionConfigOptionSelectRequest`
//! shape an earlier version of this function inferred from the ACP org's
//! reference SDK — see its own doc comment for the correction and the task
//! report for the full probe transcript.
//!
//! **This is absence of evidence for the two methods a generic ACP client
//! could plausibly reach for, not proof no mechanism exists anywhere.** A
//! vendor `_x.ai/*` setter is plausible — Grok already speaks the vendor
//! completion notification this module names `PROMPT_COMPLETE_METHOD` (see
//! `session.rs`) on this exact build — and so is passing the selection
//! inside `session/new`'s own `_meta`, neither of which this probe tried.
//! Sending nothing is still the right call either way: a guess at an
//! untried vendor method is no better evidenced than the two that turned
//! out not to work. `scripts/probe-acp-setters.py` (committed, not just
//! prose in a task report) is the script that produced the transcript
//! above; re-run it against a newer Grok build before assuming this finding
//! still holds.

use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde_json::{Value, json};

use comet_proto::{
    AgentCommand, AgentEvent, HarnessCapabilities, HarnessId, HarnessProbe, InstallMethod, Model,
    ModelCatalog, ReasoningLevel, RunRequest, RuntimeMode, SteeringMode,
};

use super::AgentDescription;
use super::session::{AcpSession, Discovered, SettleSignal, Timeouts};
use crate::discovery::{DiscoveredModel, Discovery, DiscoveryFailure};
use crate::launch::{LaunchDescriptor, StdioMode};
use crate::{Harness, HarnessError, RunControls};

/// Grok's ACP entry point.
///
/// **Every token was verified against grok 1.0.5 on 2026-08-28, because the
/// placement is not guessable.** `--no-auto-update` is a TOP-LEVEL flag and is
/// **hidden** — it appears in neither `grok --help` nor `grok agent --help`, and
/// the only way to tell it from a typo is that clap rejects unknown flags (a
/// `--no-such-flag` control errors with "unexpected argument"; this one exits
/// 0). `--no-leader` belongs to the `agent` SUBCOMMAND, not the top level, and
/// `stdio` is a sub-subcommand of `agent`. The plausible-looking
/// `grok agent stdio --no-leader` does not parse.
///
/// `--no-leader` is the load-bearing one: without it, `agent stdio` may attach
/// to a shared leader process over `~/.grok/leader.sock` instead of starting its
/// own agent, and Comet would be driving a session belonging to another client.
pub const GROK_ARGS: [&str; 4] = ["--no-auto-update", "agent", "--no-leader", "stdio"];

/// Locate the device's installed Grok CLI: `GROK_EXECUTABLE`, then our own
/// PATH, then the system's persisted PATH, then known install locations. Same
/// ladder and the same reasons as [`crate::codex::resolve_codex_executable`].
pub fn resolve_grok_executable() -> Option<PathBuf> {
    crate::resolve_cli(
        "GROK_EXECUTABLE",
        "grok",
        crate::all_known_dirs(grok_install_dirs()),
    )
}

/// Where a Grok CLI lands when PATH does not name it.
///
/// `~/.grok/bin` is the installer's own location and holds `grok` alongside an
/// identical `agent` binary — observed on this machine, and the reason the list
/// names the directory rather than either file.
fn grok_install_dirs() -> Vec<crate::KnownDir> {
    let mut dirs: Vec<crate::KnownDir> = Vec::new();
    if let Some(home) = crate::home_dir() {
        dirs.push((home.join(".grok").join("bin"), InstallMethod::Native));
        dirs.push((home.join(".local").join("bin"), InstallMethod::Native));
    }
    if cfg!(windows) {
        dirs.extend(
            crate::env_dir("LOCALAPPDATA")
                .map(|d| (d.join("Programs").join("grok"), InstallMethod::Native)),
        );
    } else {
        dirs.push((PathBuf::from("/opt/homebrew/bin"), InstallMethod::Homebrew));
        // Untagged for the same reason as the other providers' lists:
        // `/usr/local/bin` is Intel Homebrew, a manual copy, and several
        // installers' fallback all at once.
        dirs.push((PathBuf::from("/usr/local/bin"), InstallMethod::Unknown));
    }
    dirs
}

/// Describe the exact process launch used for a Grok run.
///
/// **Production's builder, and the one the capture recorder's Grok rows spawn**
/// — the same seam `claude::run_launch` and `codex::run_launch` sit on. A
/// recorder with its own copy would make the corpus evidence of the recorder.
///
/// The request contributes nothing to argv today: model and effort are selected
/// over the wire per session (`_x.ai/models/update` and the session's own
/// `models` block), not as spawn flags. It is taken anyway so the signature
/// matches the launch seam `capture::record::derive_launch` calls, and so a
/// future flag has an obvious home.
pub fn run_launch(exe: &Path, _request: &RunRequest) -> LaunchDescriptor {
    LaunchDescriptor {
        program: crate::discovery::program_path(exe),
        args: GROK_ARGS.iter().map(Into::into).collect(),
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

/// The effort ladder Grok reports.
///
/// Observed in the `initialize` reply's `reasoningEfforts` on grok-4.6:
/// `xhigh`, `high` (default), `medium`, `low`. Ordered low to high here, which
/// is how the traits picker renders a ladder.
///
/// **`ultra`, `ultracode` and `ultrathink` are absent deliberately.** The first
/// is not in Grok's vocabulary, and the other two are Comet-layered rather than
/// provider-reported (`discovery::is_comet_special`).
const REASONING_LEVELS: [ReasoningLevel; 4] = [
    ReasoningLevel::Low,
    ReasoningLevel::Medium,
    ReasoningLevel::High,
    ReasoningLevel::XHigh,
];

/// The curated list, used when discovery cannot run or cannot be read.
///
/// **Its job is to never be empty.** `DiscoveryCache::catalog` falls back to
/// this on any failure, and a picker showing "this agent has no models" when the
/// truth is "we could not reach Grok" is the confident-wrong-answer shape this
/// repository has hit twice.
///
/// One entry, because one is what `grok models` lists: grok-4.6 is the only
/// model the account offers. A live discovery unions over this, so an account
/// with more gets them.
fn static_models() -> Vec<Model> {
    vec![Model {
        id: "grok-4.6".into(),
        label: "Grok 4.6".into(),
        description: Some("SpaceXAI's latest frontier model".into()),
        deprecation: None,
        reasoning_levels: REASONING_LEVELS.to_vec(),
        options: Vec::new(),
        // Grok's `promptCapabilities.image` reads false on this build, and
        // unlike an absent field that is the provider saying so.
        accepts_images: false,
    }]
}

/// Grok's session config: a FLAT list keyed by `category`, under a vendor
/// `_meta` key rather than ACP's own `configOptions`.
///
/// Observed on grok 1.0.5 (`session/new`, captured 2026-08-28): five entries,
/// one `category: "model"` and four `category: "mode"`.
fn config_options(session: &Value) -> &[Value] {
    session["_meta"]["x.ai/sessionConfig"]["options"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
}

/// Rows of one `category` from the session config.
fn options_in<'a>(session: &'a Value, category: &str) -> Vec<&'a Value> {
    config_options(session)
        .iter()
        .filter(|o| o["category"].as_str() == Some(category))
        .collect()
}

/// **Grok spells its effort ladder `category: "mode"`.**
///
/// Not `thought_level`, which is what the ACP org adapters use and what a port
/// of upstream's mapping would look for. That mapping additionally reads a
/// `mode` category as a PERMISSION mode and forces it to the no-prompts choice
/// — applied to Grok that would drive reasoning effort as if it were a
/// permission setting. Read the categories the capture shows, not the ones
/// another vendor's adapters send.
///
/// Wire order is preserved: the agent lists strongest first and the picker
/// renders the ladder in the order it is given.
fn ladder_from_config(session: &Value) -> Vec<ReasoningLevel> {
    options_in(session, "mode")
        .iter()
        .filter_map(|o| effort_from_id(o["id"].as_str()?))
        .collect()
}

/// The model list, config surface first.
///
/// **`availableModels` is the deprecated surface and must not be read as a
/// model list when a config surface exists.** Agents that have both enumerate
/// one `availableModels` entry per model × effort there, so a five-model agent
/// with a four-rung ladder would arrive as twenty picker rows. Grok 1.0.5 has
/// one model, so nothing multiplies today — which is exactly why this would
/// have shipped unnoticed.
///
/// The two surfaces carry different halves, so both are read: `category:
/// "model"` rows are authoritative for WHICH models exist, and `availableModels`
/// is joined onto them by id for the richer per-model detail (description, the
/// per-model effort list) that the config rows do not carry.
///
/// Tolerant throughout: a row without an id is skipped rather than failing the
/// list, and an agent with neither surface yields an empty [`Discovery`] rather
/// than an error. "Listed nothing" and "could not be reached" are different
/// answers, and only the second is a [`DiscoveryFailure`].
fn models_from_discovery(discovered: &Discovered) -> Discovery {
    let session = &discovered.session;

    // Every `availableModels` entry we can find, keyed by id, as enrichment
    // ONLY. `session/new` carries the fresher copy; `initialize` is the
    // fallback for an agent that answers one and not the other.
    let detail: Vec<&Value> = [
        session["models"]["availableModels"].as_array(),
        discovered.initialized["_meta"]["modelState"]["availableModels"].as_array(),
    ]
    .into_iter()
    .flatten()
    .flatten()
    .collect();
    let detail_for = |id: &str| {
        detail
            .iter()
            .copied()
            .find(|m| m["modelId"].as_str() == Some(id))
    };

    let shared_ladder = ladder_from_config(session);
    // The handshake's own answer, agent-wide — there is no per-model modality
    // surface for Grok (checked above), but there IS now a readable one for
    // the agent as a whole: `agentCapabilities.promptCapabilities.image`. Read
    // once outside the closure below and applied identically to every model,
    // rather than left `None` the way an unreadable per-model field would be —
    // "the agent did not say" and "we never looked" are different, and Grok's
    // real 2026-08-28 reply DOES say (`false`).
    let image_support =
        AgentDescription::from_initialize(&discovered.initialized).image_attachments;
    let build = |id: String, label: Option<&str>| {
        let extra = detail_for(&id);
        let per_model = extra.map(efforts_of).unwrap_or_default();
        DiscoveredModel {
            label: label
                .or_else(|| extra.and_then(|m| m["name"].as_str()))
                .unwrap_or(&id)
                .to_owned(),
            description: extra.and_then(|m| m["description"].as_str().map(str::to_owned)),
            deprecation: None,
            // The per-model list wins where it exists: a ladder declared on the
            // model is more specific than the session-wide one. Falling back to
            // the session ladder keeps an agent that only declares it once.
            reasoning_levels: if per_model.is_empty() {
                shared_ladder.clone()
            } else {
                per_model
            },
            accepts_images: image_support,
            id,
        }
    };

    let configured = options_in(session, "model");
    if !configured.is_empty() {
        return Discovery {
            models: configured
                .iter()
                .filter_map(|o| {
                    let id = o["id"].as_str()?.to_owned();
                    let label = o["label"].as_str();
                    Some(build(id, label))
                })
                .collect(),
        };
    }

    // No config surface: the deprecated block is all there is. An agent this
    // old cannot be enumerating model × effort on it, because that convention
    // arrived with the config surface it lacks.
    Discovery {
        models: detail
            .iter()
            .filter_map(|m| {
                let id = m["modelId"].as_str()?.to_owned();
                Some(build(id, None))
            })
            .collect(),
    }
}

/// Grok's own token reading, from the `session/prompt` response's `_meta` —
/// vendor-namespaced, unlike Hermes' reading of the ACP spec's own top-level
/// `usage` block (`normalize::usage`). Moved out of `normalize.rs` in PR1: it
/// only ever looked spec-general because Grok was the only agent exercising
/// it, and Hermes' shape genuinely disagreeing is what proved the split real.
///
/// **`inputTokens` is the prompt size and it is cache-INCLUSIVE**, which is
/// exactly what [`comet_proto::AgentEvent::Usage::prompt_tokens`] is defined
/// to mean — read that field's own doc comment before changing this, because
/// the two existing providers disagree on this axis and the disagreement is
/// invisible. Measured on grok 1.0.5: `inputTokens: 14500` with
/// `cachedReadTokens: 10624`, so the cached read is part of the prompt rather
/// than beside it.
///
/// **`totalTokens` is deliberately NOT used.** It is input + output and it
/// accumulates, so drawing it against the window would show a session filling
/// up from its own replies — the mistake `AgentEvent::Usage`'s doc comment
/// records Codex's `total` making, at 41% of the window after three trivial
/// turns.
///
/// `None` when the agent reported no numbers at all: an empty meter that says
/// "not measured" is honest, where zeros would read as a measurement of zero.
pub(crate) fn usage(result: &Value, context_window: Option<u64>) -> Option<AgentEvent> {
    let meta = &result["_meta"];
    let prompt_tokens = meta["inputTokens"].as_u64();
    let output_tokens = meta["outputTokens"].as_u64();
    if prompt_tokens.is_none() && output_tokens.is_none() {
        return None;
    }
    Some(AgentEvent::Usage {
        prompt_tokens: prompt_tokens.unwrap_or(0),
        output_tokens: output_tokens.unwrap_or(0),
        // `None` is "the agent did not say", never "no limit".
        context_window,
    })
}

/// Grok's vendor extension: the AUTHORITATIVE turn-end signal. **Measured
/// live 2026-08-28, raw wire timestamps: this notification at 3618ms,
/// the matching `session/prompt` RPC response at 3621ms — 3ms apart,
/// consistently, this notification first.** That raw capture lives outside
/// this tree (the ACP hardening task's own planning record, not restated
/// verbatim anywhere else in `crates/`); this doc comment is the primary
/// citation inside the tree for the figure, not a pointer to one. No other
/// recorded speaker sends it — Hermes advertises
/// nothing under `_x.ai/*` — so recognizing it by method name alone, rather
/// than gating on a per-agent flag, leaves the arm silently dead for anyone
/// who doesn't; the RPC response stays the fallback for exactly that case.
///
/// A repo-wide guard (`crates/engine/tests/no_runtime_cloud.rs`) forbids a
/// slash, the word "session", and another slash appearing contiguously
/// anywhere under `crates/`, as a check against reintroducing
/// hosted-authority remnants. This vendor method name is not one, and the
/// guard exempts it by name — spelled plainly here so that
/// `grep -r "_x.ai/session/prompt_complete" crates/` finds the constant this
/// doc calls the method's primary citation.
///
/// **D122: lives here, not in `session.rs`.** The shared turn loop only
/// knows it as [`super::session::SettleSignal::method`] — an opaque string
/// it compares an incoming notification's own method against — which is what
/// lets that file stay free of this (or any other vendor's) wire vocabulary.
const PROMPT_COMPLETE_METHOD: &str = "_x.ai/session/prompt_complete";

/// How long the shared turn loop's notification-settle arm waits for the
/// already-in-flight `session/prompt` reply once [`PROMPT_COMPLETE_METHOD`]
/// has already ended the turn — long enough to catch the ~3ms gap that
/// constant's own doc measured live, short enough that an agent which never
/// answers the RPC at all (`complete-notification-only` in `fake-acp`, and
/// the shape upstream's original hang report was about) still settles
/// promptly rather than reintroducing that hang.
///
/// **~80x the measured ~3ms gap, not a round number picked for its own
/// sake.** A single measurement on one machine on one day is thin evidence
/// for a bound that fails SILENTLY when it is too tight — this build has no
/// drift sheet, no supported-version floor and no runnable live suite for
/// Grok (`docs/debt/README.md`'s D102), so a miss here would not be caught
/// by anything else in the tree. The margin also has to absorb process
/// scheduling and named-pipe latency on whatever machine runs this, not just
/// scheduler jitter on the one that measured 3ms — this repository's own
/// guidance is that a wall-clock figure from a GPU-less VM is an upper
/// bound, not a measurement. Still 120x below `Timeouts::prompt_stall`'s own
/// 30s default, so this can never be mistaken for — or mask — a wedged
/// agent: the only user-visible cost of a genuinely non-answering agent is
/// one extra 250ms per turn, not a hang.
const POST_NOTIFICATION_REPLY_BOUND: Duration = Duration::from_millis(250);

/// Grok's own [`SettleSignal`] (D122) — injected into
/// [`super::session::run`] the same way [`usage`] already is, so the method
/// name above, both promptId paths, and the reply bound stay in this vendor
/// file rather than in the shared turn loop. `session.rs` never sees this
/// value's shape: it is handed the whole struct through
/// `super::session::run`'s own `Option<SettleSignal>` parameter and reads it
/// generically.
///
/// Both promptId readers pull from a DIFFERENT shape: [`Self::method`]'s own
/// notification carries it at a top-level `promptId`, while a settled
/// `session/prompt` RPC reply carries the same value nested at
/// `_meta.promptId` (verified live, both on grok 1.0.5's 2026-08-28
/// capture) — one struct, two distinct paths, because the wire genuinely
/// disagrees about where this field lives depending on which message
/// carries it.
pub(crate) const SETTLE_SIGNAL: SettleSignal = SettleSignal {
    method: PROMPT_COMPLETE_METHOD,
    notification_prompt_id: |params| params["promptId"].as_str(),
    reply_prompt_id: |result| result["_meta"]["promptId"].as_str(),
    reply_bound: POST_NOTIFICATION_REPLY_BOUND,
};

/// One effort id from Grok's vocabulary. An unrecognized rung is dropped rather
/// than guessed at: offering one the picker cannot send is worse than a shorter
/// ladder.
fn effort_from_id(id: &str) -> Option<ReasoningLevel> {
    match id {
        "low" => Some(ReasoningLevel::Low),
        "medium" => Some(ReasoningLevel::Medium),
        "high" => Some(ReasoningLevel::High),
        "xhigh" => Some(ReasoningLevel::XHigh),
        _ => None,
    }
}

/// What to send to apply the caller's model choice to a freshly opened
/// session, as a single `session/set_model` request.
///
/// **Verified live against grok 1.0.5 on 2026-08-29 — and it corrects an
/// earlier, wrong design.** A first version of this function sent
/// `session/set_config_option` with `configId` set to the row's own
/// `category` (`"model"` or `"mode"`), inferred from the ACP org's reference
/// SDK schema (`SetSessionConfigOptionSelectRequest`) because Grok's session
/// config is shaped like exactly that: a flat `category`-keyed list of rows,
/// each carrying `id` and `selected`. **A raw JSON-RPC probe against the real
/// CLI proved that inference wrong**: `session/set_config_option` answers
/// `-32601 Method not found` — the method is not registered at all, not
/// merely rejecting the params. `session/set_model` (the ACP spec's own
/// dedicated, unstable method — the same one Hermes' installed source
/// implements) DOES work: it validates its `modelId` (an unknown id answers
/// `-32602 Invalid params: unknown model id`, a real model id answers
/// `{"_meta": {"model": {"Ok": "<id>"}}}`) and is what actually changes the
/// session's model.
///
/// **No effort setter was found among the methods tried — see this
/// module's own doc comment above ("No effort setter was found among the
/// methods tried") for the fuller account, what was tried, and what was
/// NOT.** `request.reasoning` is therefore never read here, the same as
/// Hermes' `config_requests`, though for a different underlying reason:
/// Hermes never had an effort ladder to begin with, while Grok's ladder is
/// real (`REASONING_LEVELS`, shown in the picker) but has no ACP setter this
/// build's probe could find.
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

/// Recognizes Grok's own signed-out shape on `session/new` and turns it into
/// a clean instruction, in place of the raw JSON-RPC text
/// [`HarnessError::Protocol`] would otherwise carry to the user
/// (`.agents/rules/user-facing-errors.md`).
///
/// **Verified live against grok 1.0.5 on 2026-08-29**, run with `GROK_HOME`
/// pointed at a scratch directory with no `auth.json` (the isolation D102
/// established): `initialize` answers normally — `authMethods:
/// [{"id": "grok.com", ...}]`, `_meta.defaultAuthMethodId: null` — and
/// `session/new` fails outright with
/// `{"code": -32000, "message": "Authentication required",
/// "data": "no auth method id provided"}`. `message` alone (`"Authentication
/// required"`) is what the check below matches on — Grok's own `data` is
/// redundant for detection here (unlike Hermes', see `hermes::map_open_failure`),
/// but is still available on [`crate::jsonrpc::RpcFailure::data`] if a future
/// build of Grok ever needs it.
///
/// **The match is on a substring of `message`, not the whole shape (`code`
/// included) — deliberately broad, not narrowed further.** Any Grok
/// failure whose `message` contains this exact phrase maps to "run `grok
/// login`", including a hypothetical mid-life token expiry that reused the
/// same wording rather than only the fresh-install "never signed in" case
/// this was captured against. That is still the right advice for either
/// cause (re-running `grok login` fixes both), so the breadth was left
/// alone rather than narrowed to the one `code`/`message` pairing this
/// capture happens to show — recorded here so a future reader does not
/// mistake it for an oversight.
///
/// **Step 5's decision: Comet never calls ACP's own `authenticate`
/// method.** Grok's `initialize` reply advertises exactly one entry —
/// `{"id": "grok.com", "name": "Grok", "description": "Sign in with
/// Grok"}` — with no `_meta` marking it headless or key-based; the only
/// sign-in flow evidenced here is the OAuth one `grok login` already drives
/// interactively. Launching that from a background discovery probe would be
/// a surprise the user never asked for, and the engine has never owned an
/// agent's own credentials (`AGENTS.md`'s local/LAN authority model). Send
/// the user to the CLI's own command instead — `grok login` is the exact
/// subcommand (`grok login --help`: "Sign in to Grok"), read from `grok
/// --help`'s own command list, not guessed.
pub(crate) fn map_open_failure(failure: &crate::jsonrpc::RpcFailure) -> Option<HarnessError> {
    if failure.message.contains("Authentication required") {
        Some(HarnessError::NeedsSetup {
            summary: "Sign-in required".into(),
            hint: "Run `grok login` to sign in to Grok, then try again.".into(),
        })
    } else {
        None
    }
}

/// A model's `_meta.reasoningEfforts[].id`, keeping only levels Comet knows.
///
/// An unrecognized effort is dropped rather than guessed at: offering a rung
/// the picker cannot send is worse than a shorter ladder.
fn efforts_of(model: &Value) -> Vec<ReasoningLevel> {
    model["_meta"]["reasoningEfforts"]
        .as_array()
        .map(|efforts| {
            efforts
                .iter()
                .filter_map(|effort| effort_from_id(effort["id"].as_str()?))
                .collect()
        })
        .unwrap_or_default()
}

/// Grok's model list off one handshake — [`super::discover_models`] with
/// Grok's launch and Grok's own model mapping. The probe itself is protocol,
/// not vendor, so it lives in `acp/mod.rs`.
async fn discover(exe: PathBuf, timeouts: Timeouts) -> Result<Discovery, DiscoveryFailure> {
    super::discover_models("grok", run_launch, exe, timeouts, models_from_discovery).await
}

/// **Grok's slash commands are free and pushed, so this costs one handshake.**
///
/// The agent sends its whole list unsolicited before `session/new` even
/// replies, so a discovery probe collects it without a prompt and without
/// tokens. Cwd-scoped like every other provider's, because a project's own
/// commands are discovered from the directory.
async fn discover_commands(
    exe: PathBuf,
    cwd: String,
    timeouts: Timeouts,
) -> Result<Vec<AgentCommand>, DiscoveryFailure> {
    super::discover_commands("grok", run_launch, exe, cwd, timeouts).await
}

/// The Grok harness. Construct with [`GrokHarness::new`]; tests point it at the
/// `fake-acp` fixture with [`GrokHarness::with_executable`].
///
/// The derived `Default` is the real one: `Timeouts` carries its own non-zero
/// defaults, so deriving here picks them up rather than zeroing them.
#[derive(Default)]
pub struct GrokHarness {
    executable: Option<PathBuf>,
    timeouts: Timeouts,
    /// One handshake per boot. `models()` is on the picker's render path AND is
    /// called by titling, so an uncached discovery would spawn an agent on a
    /// path the user never sees.
    discovery_cache: crate::discovery::DiscoveryCache,
    /// One handshake per DIRECTORY per boot. Separate from the model cache
    /// because commands are cwd-scoped and models are not — the same reason
    /// `CommandCache` exists at all (debt row D32).
    command_cache: crate::discovery::CommandCache,
}

impl GrokHarness {
    pub fn new() -> Self {
        Self::default()
    }

    /// The single declaration of what Grok can honor, named by both the
    /// registry's lazy descriptor and the trait impl so the two cannot drift.
    ///
    /// **Steering is at the turn boundary, and that is Grok's own limit, not a
    /// gap here.** Grok advertises no steering extension at all — its
    /// `initialize` carries no `_meta.steering` — so a steer is delivered as
    /// the next prompt on the same session. Declaring `StepBoundary` would be
    /// a promise the run breaks.
    pub fn capabilities() -> HarnessCapabilities {
        HarnessCapabilities {
            supports_steering: true,
            steering_mode: SteeringMode::TurnBoundary,
            reasoning_levels: REASONING_LEVELS.to_vec(),
            // **One mode, and this is now a measured fact, not an unrouted
            // gap.** `session/request_permission` IS routed to the user as of
            // PR6 (`acp::session::handle_permission_request`) — but Grok
            // 1.0.5, probed live 2026-08-29 under this exact launch
            // (`GROK_ARGS`, `initialize_params()`) with a prompt that writes a
            // file outside the working directory, never put that method on
            // the wire at all. Its own internal permission gate fired (a
            // vendor `_x.ai/session_notification` with `sessionUpdate:
            // "pending_interaction"`, `kind: "permission"`) and resolved
            // itself (`"interaction_resolved"`) without ever escalating to
            // the client, and the write completed. `grok agent --help`
            // confirms there is no flag to force the OTHER direction either:
            // `--always-approve` exists (redundant with what was already
            // observed) but nothing asks for a posture that blocks on the
            // user. Declaring `ApprovalRequired` here would be a promise a
            // "please ask about everything" selection cannot keep — the user
            // would pick it and nothing would ever be asked. Full trace in
            // the PR6 task report.
            //
            // **Not exhaustively probed.** Only a file-write trigger was
            // tested before this machine's free-tier Grok quota was
            // exhausted (`subscription:free-usage-exhausted`, same probe
            // run); a shell-command trigger was not tried. `FullAccess` is
            // the honest set for what was actually observed, and stays that
            // way until a command-approval scenario is captured too.
            runtime_modes: vec![RuntimeMode::FullAccess],
            // No captured `session/request_permission` from Grok exists to
            // check an options shape against (see above) — nothing to attach
            // a note to either way, since the mode that would carry one is
            // not declared.
            carries_deny_note: false,
            // Grok pushes `session_info_update` with its own title during the
            // turn — see `AgentEvent::SessionTitled`'s doc for the captured
            // wire evidence. The engine skips its upfront titling call on
            // this.
            self_titles: true,
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
        resolve_grok_executable().ok_or_else(|| {
            HarnessError::NotInstalled(crate::not_installed_message("grok", "GROK_EXECUTABLE"))
        })
    }
}

#[async_trait]
impl Harness for GrokHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Grok
    }

    fn display_name(&self) -> &str {
        // Matches the registry's lazy descriptor, so the catalog entry does not
        // change after the first resolve.
        "Grok"
    }

    fn capabilities(&self) -> HarnessCapabilities {
        Self::capabilities()
    }

    async fn probe(&self) -> HarnessProbe {
        crate::probe_installed_cli(
            self.resolve_executable(),
            "grok",
            "GROK_EXECUTABLE",
            crate::all_known_dirs(grok_install_dirs()),
        )
        .await
    }

    /// The curated catalog unioned with whatever the handshake reported.
    ///
    /// An absent CLI surfaces as [`HarnessError::NotInstalled`] rather than as
    /// a failed discovery: the user's action is different, and the picker's
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
    /// Overridden rather than left at the default empty list: Grok really does
    /// have 45 commands, and the default would present a working surface as
    /// absent. An unreachable agent answers an empty list rather than an error
    /// — the menu says so on screen, where the user who typed `/` is looking,
    /// and a command list that cannot be read is not the protocol-drift signal
    /// the diagnostics channel exists for.
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
        // The Retry row clears both: a user who hits it after fixing an install
        // means "ask again", not "ask again about models only".
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
            HarnessId::Grok,
            request,
            controls,
            usage,
            Some(SETTLE_SIGNAL),
        ))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// Break caught: reordering Grok's launch tokens. `--no-auto-update` is
    /// top-level, `--no-leader` belongs to `agent`, and `stdio` is under
    /// `agent`; any other arrangement fails to parse, and none of the three
    /// flags is documented in `--help` output where a reader could check.
    #[test]
    fn the_launch_line_keeps_its_verified_token_order() {
        assert_eq!(
            GROK_ARGS,
            ["--no-auto-update", "agent", "--no-leader", "stdio"]
        );
        let launch = run_launch(
            Path::new("/usr/local/bin/grok"),
            &RunRequest::for_session(RuntimeMode::default()),
        );
        assert_eq!(
            launch.args,
            GROK_ARGS
                .iter()
                .map(std::ffi::OsString::from)
                .collect::<Vec<_>>()
        );
    }

    /// The two replies grok 1.0.5 really sent on 2026-08-28, trimmed to the
    /// keys this decode reads. One helper rather than a literal per test, so
    /// every test that says "the captured wire" means the same bytes.
    fn captured() -> Discovered {
        Discovered {
            initialized: json!({
                "protocolVersion": 1,
                // `agentCapabilities.promptCapabilities.image: false`, exactly
                // as grok 1.0.5 answered on 2026-08-28 — the PR7 addition this
                // fixture omitted at PR1 time, when nothing yet read it.
                "agentCapabilities": {
                    "loadSession": true,
                    "promptCapabilities": {"image": false, "audio": false, "embeddedContext": true},
                },
                "_meta": {"agentVersion": "1.0.5", "modelState": {
                    "currentModelId": "grok-4.6",
                    "availableModels": [{"modelId": "grok-4.6", "name": "Grok 4.6"}],
                }},
            }),
            session: json!({
                "sessionId": "01a047b8-53a0-7342-b63b-3241fa0b25c2",
                "_meta": {"x.ai/sessionConfig": {"options": [
                    {"category": "model", "id": "grok-4.6", "label": "Grok 4.6", "selected": true},
                    {"category": "mode", "id": "xhigh", "label": "Extra High Effort", "selected": false},
                    {"category": "mode", "id": "high", "label": "High Effort", "selected": true},
                    {"category": "mode", "id": "medium", "label": "Medium Effort", "selected": false},
                    {"category": "mode", "id": "low", "label": "Low Effort", "selected": false},
                ]}},
                "models": {"currentModelId": "grok-4.6", "availableModels": [{
                    "modelId": "grok-4.6",
                    "name": "Grok 4.6",
                    "description": "SpaceXAI's latest frontier model",
                    "_meta": {"totalContextTokens": 500000, "reasoningEfforts": [
                        {"id": "xhigh"}, {"id": "high"}, {"id": "medium"}, {"id": "low"},
                    ]},
                }]},
            }),
            ..Default::default()
        }
    }

    /// The real replies, captured from grok 1.0.5 on 2026-08-28. Pinned against
    /// the literal wire rather than a Rust round-trip: a reshaped reply must
    /// fail here, which a type-mediated test would not catch.
    #[test]
    fn the_captured_replies_yield_the_model_list() {
        let discovered = captured();
        let discovery = models_from_discovery(&discovered);

        assert_eq!(discovery.models.len(), 1);
        let model = &discovery.models[0];
        assert_eq!(model.id, "grok-4.6");
        assert_eq!(model.label, "Grok 4.6");
        assert_eq!(
            model.description.as_deref(),
            Some("SpaceXAI's latest frontier model"),
            "the description comes off availableModels, which the config rows do not carry"
        );
        assert_eq!(
            model.reasoning_levels,
            vec![
                ReasoningLevel::XHigh,
                ReasoningLevel::High,
                ReasoningLevel::Medium,
                ReasoningLevel::Low,
            ],
            "efforts keep the order the agent listed them in"
        );
        assert_eq!(
            model.accepts_images,
            Some(false),
            "no per-model modality surface exists, but the handshake's own \
             agentCapabilities.promptCapabilities.image DOES say (false, on the \
             real 2026-08-28 reply) -- that agent-wide answer is what every \
             discovered model now carries, PR7 onward"
        );
    }

    /// **The model × effort trap.** `availableModels` is the deprecated surface,
    /// and agents that have both enumerate one entry there per model × effort.
    /// Reading it as the model list turns a 2-model, 3-rung agent into six
    /// picker rows.
    ///
    /// Grok 1.0.5 has one model, so nothing multiplies on the real wire — which
    /// is exactly why this would have shipped unnoticed. The fixture is
    /// therefore built by hand rather than captured, and deliberately so.
    #[test]
    fn the_config_surface_wins_over_the_combinatorial_one() {
        let discovered = Discovered {
            initialized: json!({}),
            session: json!({
                "_meta": {"x.ai/sessionConfig": {"options": [
                    {"category": "model", "id": "grok-4.6", "label": "Grok 4.6"},
                    {"category": "model", "id": "grok-mini", "label": "Grok Mini"},
                    {"category": "mode", "id": "high", "label": "High Effort"},
                    {"category": "mode", "id": "low", "label": "Low Effort"},
                ]}},
                // Six rows for two models: the shape that must NOT be read.
                "models": {"availableModels": [
                    {"modelId": "grok-4.6", "name": "Grok 4.6 (high)"},
                    {"modelId": "grok-4.6-medium", "name": "Grok 4.6 (medium)"},
                    {"modelId": "grok-4.6-low", "name": "Grok 4.6 (low)"},
                    {"modelId": "grok-mini", "name": "Grok Mini (high)"},
                    {"modelId": "grok-mini-medium", "name": "Grok Mini (medium)"},
                    {"modelId": "grok-mini-low", "name": "Grok Mini (low)"},
                ]},
            }),
            ..Default::default()
        };

        let discovery = models_from_discovery(&discovered);
        let ids: Vec<&str> = discovery.models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["grok-4.6", "grok-mini"],
            "the config surface names the models; availableModels only enriches them"
        );
    }

    /// **Grok's effort ladder is `category: "mode"`.** Break caught: reading it
    /// as `thought_level` (what the ACP org adapters send, and what a port of
    /// upstream's mapping looks for), which finds no ladder here — or worse,
    /// treating `mode` as a permission mode and driving reasoning effort as if
    /// it were a permission setting.
    #[test]
    fn the_effort_ladder_is_read_from_the_mode_category() {
        let session = json!({"_meta": {"x.ai/sessionConfig": {"options": [
            {"category": "mode", "id": "xhigh"},
            {"category": "mode", "id": "high"},
            {"category": "mode", "id": "medium"},
            {"category": "mode", "id": "low"},
        ]}}});
        assert_eq!(
            ladder_from_config(&session),
            vec![
                ReasoningLevel::XHigh,
                ReasoningLevel::High,
                ReasoningLevel::Medium,
                ReasoningLevel::Low,
            ]
        );

        // The name another vendor uses. Present here, it must contribute
        // nothing — Grok does not send it, and a decode that read both would
        // be guessing at which one this agent means.
        let other_vendor = json!({"_meta": {"x.ai/sessionConfig": {"options": [
            {"category": "thought_level", "id": "high"},
        ]}}});
        assert!(
            ladder_from_config(&other_vendor).is_empty(),
            "thought_level is not Grok's spelling and must not be read as one"
        );
    }

    /// A session ladder covers models that declare none of their own, and a
    /// per-model ladder wins where it exists — the more specific answer.
    #[test]
    fn a_per_model_ladder_beats_the_session_wide_one() {
        let discovered = Discovered {
            initialized: json!({}),
            session: json!({
                "_meta": {"x.ai/sessionConfig": {"options": [
                    {"category": "model", "id": "shared"},
                    {"category": "model", "id": "specific"},
                    {"category": "mode", "id": "high"},
                ]}},
                "models": {"availableModels": [
                    {"modelId": "specific", "_meta": {"reasoningEfforts": [{"id": "low"}]}},
                ]},
            }),
            ..Default::default()
        };

        let models = models_from_discovery(&discovered).models;
        let by_id = |id: &str| {
            models
                .iter()
                .find(|m| m.id == id)
                .unwrap_or_else(|| panic!("{id} missing"))
                .clone()
        };
        assert_eq!(
            by_id("shared").reasoning_levels,
            vec![ReasoningLevel::High],
            "a model declaring no ladder of its own inherits the session's"
        );
        assert_eq!(
            by_id("specific").reasoning_levels,
            vec![ReasoningLevel::Low],
            "a model's own ladder is the more specific answer and wins"
        );
    }

    /// **The other speakers' model paths are not Grok's.** Break caught:
    /// reading codex-acp's `models.availableModels` on `session/new`, or
    /// claude-agent-acp's `modes.availableModes` — the first would pick up an
    /// unrelated list, the second finds nothing at all.
    #[test]
    fn another_speakers_shape_is_not_mistaken_for_groks() {
        let claude_acp_shape = Discovered {
            initialized: json!({}),
            session: json!({"modes": {"availableModes": [{"id": "not-grok"}]}}),
            ..Default::default()
        };
        assert!(
            models_from_discovery(&claude_acp_shape).models.is_empty(),
            "claude-agent-acp's path must not be mistaken for Grok's"
        );

        // The org adapters' top-level `configOptions`, which Grok does not
        // send: its config is vendor-namespaced under `_meta`.
        let org_adapter_shape = Discovered {
            initialized: json!({}),
            session: json!({"configOptions": [
                {"category": "model", "options": [{"value": "not-grok"}]},
            ]}),
            ..Default::default()
        };
        assert!(
            models_from_discovery(&org_adapter_shape).models.is_empty(),
            "a top-level configOptions is not where Grok puts its config"
        );
    }

    /// An unreadable or absent pair of surfaces is an empty list, never an
    /// error: "listed nothing" and "could not be reached" are different
    /// answers, and only the second earns a `DiscoveryFailure`.
    #[test]
    fn absent_surfaces_are_empty_rather_than_a_failure() {
        for session in [
            json!({}),
            json!({"_meta": {}}),
            json!({"_meta": {"x.ai/sessionConfig": {}}}),
            json!({"_meta": {"x.ai/sessionConfig": {"options": []}}}),
            // Present but not an array.
            json!({"_meta": {"x.ai/sessionConfig": {"options": "grok-4.6"}}}),
            json!({"models": {"availableModels": "grok-4.6"}}),
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

    /// **A failed `session/new` still discovers.** The handshake answered, so
    /// the agent is reachable; only the richer surface is missing, and
    /// `initialize`'s own block carries the fallback. Degrading this to a
    /// discovery failure would show the built-in caption for an agent that
    /// answered.
    #[test]
    fn initialize_alone_still_yields_models() {
        let discovered = Discovered {
            initialized: json!({"_meta": {"modelState": {"availableModels": [
                {"modelId": "grok-4.6", "name": "Grok 4.6"},
            ]}}}),
            // What `open_for_discovery` substitutes when session/new did not
            // answer.
            session: Value::Null,
            ..Default::default()
        };
        let models = models_from_discovery(&discovered).models;
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "grok-4.6");
    }

    /// A row with no id is skipped rather than poisoning the list, and an
    /// effort this build does not know is dropped rather than guessed at.
    #[test]
    fn unreadable_entries_are_skipped_not_guessed() {
        let discovered = Discovered {
            initialized: json!({}),
            session: json!({
                "_meta": {"x.ai/sessionConfig": {"options": [
                    {"category": "model", "label": "No Id At All"},
                    {"category": "model", "id": "grok-9"},
                    {"category": "mode", "id": "low"},
                    {"category": "mode", "id": "an_effort_from_2027"},
                ]}},
            }),
            ..Default::default()
        };

        let models = models_from_discovery(&discovered).models;
        assert_eq!(models.len(), 1, "the id-less row is skipped");
        assert_eq!(models[0].id, "grok-9");
        assert_eq!(
            models[0].label, "grok-9",
            "a row with no label falls back to its id rather than to an empty string"
        );
        assert_eq!(
            models[0].reasoning_levels,
            vec![ReasoningLevel::Low],
            "the unknown rung is dropped, not guessed at"
        );
    }

    /// **The curated list must never be empty**, because it is what the picker
    /// shows when discovery fails. An empty one turns "couldn't reach Grok"
    /// into "this agent has no models".
    #[test]
    fn the_curated_list_is_never_empty() {
        let curated = static_models();
        assert!(!curated.is_empty());
        assert!(curated.iter().all(|m| !m.id.is_empty()));

        // And the fallback path itself produces it, rather than an empty list.
        let cache = crate::discovery::DiscoveryCache::default();
        let catalog = cache.catalog(static_models(), Err(DiscoveryFailure::Unreachable));
        assert_eq!(catalog.source, comet_proto::CatalogSource::BuiltIn);
        assert!(
            !catalog.models.is_empty(),
            "a discovery failure must not degrade to an empty list"
        );
    }

    /// The real `session/prompt` response `_meta`, captured from grok 1.0.5 on
    /// 2026-08-28. **`inputTokens` is cache-inclusive**: 14500 with 10624 of it
    /// a cached read, so the cache is part of the prompt rather than beside it.
    #[test]
    fn the_captured_response_meta_reads_as_prompt_and_output() {
        let result = json!({
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

        match usage(&result, Some(500_000)).expect("the agent reported numbers") {
            AgentEvent::Usage {
                prompt_tokens,
                output_tokens,
                context_window,
            } => {
                assert_eq!(prompt_tokens, 14500, "the prompt, cache included");
                assert_eq!(output_tokens, 171);
                assert_eq!(context_window, Some(500_000));
                // **Not 14671.** `totalTokens` accumulates input + output, so
                // metering it against the window shows a session filling up
                // from its own replies.
                assert_ne!(prompt_tokens, 14671);
            }
            other => panic!("{other:?}"),
        }
    }

    /// **No numbers means no reading, not a reading of zero.** An empty meter
    /// that says "not measured" is honest; zeros claim a measurement.
    #[test]
    fn an_absent_usage_block_yields_no_event() {
        for result in [
            json!({"stopReason": "end_turn"}),
            json!({"stopReason": "end_turn", "_meta": {}}),
            json!({"_meta": {"totalTokens": 999}}),
            // The spec-general path, not Grok's `_meta` reading.
            json!({"usage": {"inputTokens": 100, "outputTokens": 1}}),
            json!({}),
        ] {
            assert!(
                usage(&result, Some(500_000)).is_none(),
                "must not report: {result}"
            );
        }
    }

    /// A partial report is still a report — one half missing does not discard
    /// the other.
    #[test]
    fn a_half_reported_usage_still_counts() {
        let only_input = json!({"_meta": {"inputTokens": 100}});
        match usage(&only_input, None).expect("input alone is a reading") {
            AgentEvent::Usage {
                prompt_tokens,
                output_tokens,
                context_window,
            } => {
                assert_eq!(prompt_tokens, 100);
                assert_eq!(output_tokens, 0);
                assert_eq!(context_window, None, "absent window is unknown, not zero");
            }
            other => panic!("{other:?}"),
        }
    }

    /// Grok declares only `FullAccess` while approvals are unrouted. Break
    /// caught: advertising a mode that promises to ask the user, when
    /// `session/request_permission` is answered `-32601`.
    #[test]
    fn only_the_mode_the_harness_can_actually_keep_is_declared() {
        let capabilities = GrokHarness::capabilities();
        assert_eq!(capabilities.runtime_modes, vec![RuntimeMode::FullAccess]);
        assert_eq!(
            capabilities.steering_mode,
            SteeringMode::TurnBoundary,
            "Grok advertises no steering extension, so the boundary is the honest answer"
        );
    }

    /// Break caught: failing to recognize Grok's real signed-out shape at
    /// all, which would leave `open()` returning the raw
    /// `HarnessError::Protocol` fallback instead of a clean instruction.
    ///
    /// The input is [`crate::jsonrpc::RpcFailure`] built from Grok's real
    /// signed-out `session/new` reply (captured live 2026-08-29, see
    /// `map_open_failure`'s own doc comment): `message` is exactly what
    /// `jsonrpc::parse_error` reads off the wire's `.message`, `data` off
    /// its `.data`.
    #[test]
    fn a_signed_out_grok_is_recognized_and_asks_the_user_to_sign_in() {
        let failure = crate::jsonrpc::RpcFailure {
            message: "Authentication required".into(),
            data: Some("no auth method id provided".into()),
        };
        let mapped =
            map_open_failure(&failure).expect("Grok's signed-out shape must be recognized");
        let HarnessError::NeedsSetup { summary, hint } = mapped else {
            panic!("expected NeedsSetup, got {mapped:?}");
        };
        assert_eq!(summary, "Sign-in required");
        assert!(
            hint.contains("grok"),
            "the hint must name the command to run: {hint}"
        );
    }

    /// Break caught: the mapper's copy leaking wire text — echoing part of
    /// `message`/`data` into `summary`/`hint` instead of returning fixed,
    /// Comet-authored strings. Unlike the test above (whose input happens to
    /// be clean already, so it could not catch this), this feeds a
    /// `message`/`data` that DO carry a protocol code and the word
    /// "jsonrpc", and asserts neither survives into the mapped output —
    /// exactly the property `.agents/rules/user-facing-errors.md` requires
    /// and the brief's own test asks for.
    #[test]
    fn the_mapped_copy_never_carries_the_raw_wire_text_even_when_the_wire_text_does() {
        let failure = crate::jsonrpc::RpcFailure {
            message: "Authentication required (jsonrpc error -32000)".into(),
            data: Some("no auth method id provided, jsonrpc code -32000".into()),
        };
        let mapped = map_open_failure(&failure).expect("still recognized by the substring match");
        let HarnessError::NeedsSetup { summary, hint } = mapped else {
            panic!("expected NeedsSetup, got {mapped:?}");
        };
        for text in [&summary, &hint] {
            assert!(
                !text.contains("-32000"),
                "no protocol codes on screen: {text}"
            );
            assert!(!text.to_lowercase().contains("jsonrpc"), "{text}");
        }
    }

    /// A `session/new` failure that is NOT the signed-out shape must pass
    /// through unchanged — the mapper's whole point is to recognize one
    /// specific wire shape, not to relabel every failure as "sign in".
    #[test]
    fn an_unrecognized_open_failure_is_left_alone() {
        let failure = crate::jsonrpc::RpcFailure {
            message: "some other failure".into(),
            data: None,
        };
        assert!(
            map_open_failure(&failure).is_none(),
            "an unrecognized failure must not be reclassified"
        );
    }

    /// D122: pins the exact shape handed to the shared turn loop, against the
    /// literal wire paths grok 1.0.5 answers with — both promptId reads are on
    /// the CAPTURED shapes (a notification's own top-level `promptId`, a
    /// settled reply's nested `_meta.promptId`), not on a round-trip through a
    /// Rust type, per this repo's own rule for pinning what a reply's decode
    /// depends on.
    #[test]
    fn the_settle_signal_carries_grok_s_exact_method_paths_and_bound() {
        assert_eq!(SETTLE_SIGNAL.method, "_x.ai/session/prompt_complete");
        assert_eq!(SETTLE_SIGNAL.reply_bound, Duration::from_millis(250));

        let notification = json!({"sessionId": "s-1", "promptId": "p-1", "stopReason": "end_turn"});
        assert_eq!(
            (SETTLE_SIGNAL.notification_prompt_id)(&notification),
            Some("p-1")
        );
        assert_eq!(
            (SETTLE_SIGNAL.notification_prompt_id)(&json!({"stopReason": "end_turn"})),
            None,
            "an absent promptId on the notification must read as unknown, not a match failure"
        );

        let reply = json!({"stopReason": "end_turn", "_meta": {"promptId": "p-1"}});
        assert_eq!((SETTLE_SIGNAL.reply_prompt_id)(&reply), Some("p-1"));
        assert_eq!(
            (SETTLE_SIGNAL.reply_prompt_id)(&json!({"stopReason": "end_turn"})),
            None,
            "a reply with no _meta.promptId must read as unknown"
        );
        assert_eq!(
            (SETTLE_SIGNAL.reply_prompt_id)(&notification),
            None,
            "the reply reader must not accidentally also match the notification's \
             top-level promptId -- the two shapes are genuinely different"
        );
    }
}
