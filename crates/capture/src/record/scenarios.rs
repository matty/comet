//! The single source of truth for a capture scenario: its name, its
//! purpose, which provider it belongs to, whether it starts a turn, what the
//! caller must have collected before it may run, and its driving body.
//!
//! Closes D60 (the scenario name living in three unsynchronized places: the
//! binary's help text, its `supported_pair()`, and its dispatch `match`) —
//! `comet-provider-capture.rs` generates its `--help` text and validates its
//! arguments off this table, and `record()` dispatches off it directly by
//! `(provider, name)`. There is no other place a scenario's name is spelled.

pub(super) mod acp;
pub(super) mod claude;
pub(super) mod codex;

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use comet_proto::{RunRequest, RuntimeMode};

use super::providers::acp::AcpProvider;
use super::providers::claude::ClaudeProvider;
use super::providers::codex::CodexProvider;
use super::session::{FenceOutcome, Session};
use crate::Provider;
use crate::types::CaptureConfig;
use comet_harness::launch::LaunchDescriptor;

/// The parameters a scenario body reads to vary its behavior without owning
/// its own copy of `CaptureConfig`. Deliberately minimal: a run scenario
/// builds its own `RunRequest` (decision "the scenario owns its prompt");
/// this only carries what a caller collects once, up front, and that more
/// than one scenario needs.
#[derive(Clone, Debug, Default)]
pub struct ScenarioInput {
    pub cwd: Option<PathBuf>,
    pub resume_id: Option<String>,
    pub attachment: Option<PathBuf>,
    pub codex_home: Option<PathBuf>,
    /// The configuration home every Claude launch is spawned against (D91). Set for a Claude
    /// row or `None`; the binary rejects it for a Codex row, so nothing downstream has to ask
    /// which provider it belongs to.
    pub claude_config_dir: Option<PathBuf>,
    pub approval_target: Option<PathBuf>,
}

/// What the binary must have collected before this scenario may run — read by
/// `comet-provider-capture.rs`'s argument validation, which looks the row up by
/// `(provider, name)` instead of hand-coding a per-scenario `if` chain.
///
/// `needs_cwd` means "does this scenario's behavior vary by cwd at all", not "may I pass --cwd"
/// (every scenario tolerates the flag). False only for the cwd-independent discovery scenarios
/// (`model-discovery` and Codex's `model-discovery-logged-out`), which always run from a neutral
/// temp directory regardless of `--cwd`. True everywhere else, including every run scenario.
///
/// `model-discovery` used to have cwd-varying siblings `-neutral-cwd`/`-project-cwd`; a real
/// capture proved `--bare` discovery is cwd-independent for both providers (`command-discovery`
/// is where cwd actually changes the reply — D32). The siblings are deleted; see
/// `docs/debt/closed.md` D80 for the evidence.
#[derive(Clone, Copy, Debug)]
pub struct Requirements {
    pub spends_tokens: bool,
    pub needs_cwd: bool,
    pub needs_resume_id: bool,
    pub needs_attachment: bool,
    pub needs_approval_target: bool,
    pub needs_empty_codex_home: bool,
    /// D91. True only where an empty configuration home is provably the CLI's own surface
    /// rather than a different observation. `model-discovery` qualifies because `--bare` never
    /// reads OAuth or the keychain (see `claude::discovery::DISCOVERY_ARGS`), so an empty home
    /// and the operator's authenticated one answer identically but for the operator's own
    /// models and commands — measured 2026-08-23 on 2.1.241.
    ///
    /// It is deliberately NOT set on `command-discovery` or the run rows, which do read
    /// credentials: an empty home logs them out, turning a capture of Claude's command surface
    /// into a capture of its logged-out one (42 commands and `account.tokenSource: "none"`,
    /// against 46 and a real account under a credentials-only home). Isolating those without
    /// logging them out needs a home seeded with `.credentials.json`, which
    /// `docs/testing/provider-captures.md` describes and this flag cannot validate as empty.
    pub needs_empty_claude_config: bool,
}

impl Requirements {
    pub(super) const fn discovery() -> Self {
        Self {
            spends_tokens: false,
            needs_cwd: false,
            needs_resume_id: false,
            needs_attachment: false,
            needs_approval_target: false,
            needs_empty_codex_home: false,
            needs_empty_claude_config: false,
        }
    }

    /// Every non-discovery scenario: spends tokens, and — unlike the
    /// cwd-independent discovery aliases — does read `--cwd`.
    pub(super) const fn run() -> Self {
        Self {
            spends_tokens: true,
            needs_cwd: true,
            needs_resume_id: false,
            needs_attachment: false,
            needs_approval_target: false,
            needs_empty_codex_home: false,
            needs_empty_claude_config: false,
        }
    }
}

pub struct ScenarioSpec {
    pub name: &'static str,
    pub purpose: &'static str,
    pub provider: Provider,
    /// `None` for discovery scenarios, which start no turn.
    pub runtime_mode: Option<RuntimeMode>,
    pub requirements: Requirements,
    /// SPAWN, per row — not a provider trait member. See the amendment on
    /// `CaptureProvider`'s doc comment for why: which launch a scenario
    /// needs varies per scenario as well as per provider.
    pub(super) launch: ScenarioLaunch,
    /// The pre-spawn fence, per row — closes D79. Selection used to be derived from
    /// `spec.runtime_mode == Some(RuntimeMode::ApprovalRequired)`, so a future Codex row wanting
    /// `ApprovalRequired` for an unrelated reason would have silently inherited the Windows-only
    /// trusted-PowerShell fence. Every row now names its own fence: [`no_fence`] by default, the
    /// two Codex approval rows point at `super::codex_fence`, both providers' `full-access`
    /// row points at `super::full_access_fence`, and so does Claude's `edit` — it changes a
    /// file with no approval channel in front of it, so the repository check is the guarantee
    /// left to give.
    pub(super) fence:
        fn(&ScenarioSpec, &CaptureConfig, &LaunchDescriptor) -> anyhow::Result<FenceOutcome>,
    pub(super) body: ScenarioBody,
}

/// The pre-spawn fence default: every Claude row (Claude has no pre-spawn
/// fence at all — its `approval` body's grant-time `claude_marker_grant`
/// recheck is the analogous protection, run against live filesystem state at
/// every grant rather than once before spawn; see that function's own doc
/// comment in `record/scenarios/claude.rs`) and every Codex row except
/// `approval`/`approval-on-request`.
pub(super) fn no_fence(
    _spec: &ScenarioSpec,
    _config: &CaptureConfig,
    _launch: &LaunchDescriptor,
) -> anyhow::Result<FenceOutcome> {
    Ok(FenceOutcome::none())
}

/// What a row's `launch` needs to build. A discovery scenario resolves a `LaunchDescriptor`
/// directly (no turn, no `RunRequest`). A run scenario instead names a builder that produces a
/// `RunRequest`, and `record.rs`'s `derive_launch` is the ONLY caller that ever invokes it: it
/// builds the `RunRequest` once, derives the launch from it, and hands that same value to the
/// scenario body through `Session::request`.
///
/// Before this enum existed, a row's `launch` was a bare `fn(&ScenarioInput, &Path) ->
/// anyhow::Result<LaunchDescriptor>`, and the scenario body separately called the same
/// `*_request` builder again for its wire line — the two independent calls agreed only because
/// every builder happened to be pure, which nothing enforced. Routing both through one call
/// closes that hole structurally instead of by convention.
#[derive(Clone, Copy)]
pub(super) enum ScenarioLaunch {
    Run(fn(&ScenarioInput) -> anyhow::Result<RunRequest>),
    Discovery(fn(&ScenarioInput, &Path) -> anyhow::Result<LaunchDescriptor>),
}

type BoxedFuture<'a> = Pin<Box<dyn Future<Output = anyhow::Result<()>> + 'a>>;

pub(super) enum ScenarioBody {
    Claude(for<'a> fn(&'a mut Session<ClaudeProvider>, &'a ScenarioInput) -> BoxedFuture<'a>),
    Codex(for<'a> fn(&'a mut Session<CodexProvider>, &'a ScenarioInput) -> BoxedFuture<'a>),
    Acp(for<'a> fn(&'a mut Session<AcpProvider>, &'a ScenarioInput) -> BoxedFuture<'a>),
}

pub const SCENARIOS: &[ScenarioSpec] = &[
    ScenarioSpec {
        name: "session-discovery-codex-acp",
        purpose: "capture the ACP initialize + session/new surface through codex-acp",
        provider: Provider::Acp,
        runtime_mode: None,
        // `session/new` carries a cwd, so these rows genuinely read one --
        // unlike the neutral CLI discovery aliases, which do not.
        requirements: Requirements {
            needs_cwd: true,
            ..Requirements::discovery()
        },
        launch: ScenarioLaunch::Discovery(acp::codex_acp_launch),
        fence: no_fence,
        body: ScenarioBody::Acp(|s, i| Box::pin(acp::session_discovery(s, i))),
    },
    ScenarioSpec {
        name: "session-discovery-claude-acp",
        purpose: "the same ACP surface through claude-agent-acp, for the two-speaker diff",
        provider: Provider::Acp,
        runtime_mode: None,
        // `session/new` carries a cwd, so these rows genuinely read one --
        // unlike the neutral CLI discovery aliases, which do not.
        requirements: Requirements {
            needs_cwd: true,
            ..Requirements::discovery()
        },
        launch: ScenarioLaunch::Discovery(acp::claude_acp_launch),
        fence: no_fence,
        body: ScenarioBody::Acp(|s, i| Box::pin(acp::session_discovery(s, i))),
    },
    ScenarioSpec {
        name: "session-discovery-grok",
        purpose: "the same ACP surface from Grok Build, the first ground-up ACP agent",
        provider: Provider::Acp,
        runtime_mode: None,
        // `session/new` carries a cwd, so these rows genuinely read one --
        // unlike the neutral CLI discovery aliases, which do not.
        requirements: Requirements {
            needs_cwd: true,
            ..Requirements::discovery()
        },
        launch: ScenarioLaunch::Discovery(acp::grok_launch),
        fence: no_fence,
        body: ScenarioBody::Acp(|s, i| Box::pin(acp::session_discovery(s, i))),
    },
    ScenarioSpec {
        name: "run-grok",
        purpose: "capture a plain Grok text turn, including its session/update command push",
        provider: Provider::Acp,
        runtime_mode: Some(RuntimeMode::FullAccess),
        requirements: Requirements::run(),
        launch: ScenarioLaunch::Run(acp::run_request),
        fence: no_fence,
        body: ScenarioBody::Acp(|s, i| Box::pin(acp::run(s, i))),
    },
    ScenarioSpec {
        name: "steer-grok",
        purpose: "capture a Grok run receiving a queued steer, delivered as the next \
                   session/prompt on the same session (Grok advertises no in-turn \
                   steering extension)",
        provider: Provider::Acp,
        runtime_mode: Some(RuntimeMode::FullAccess),
        requirements: Requirements::run(),
        launch: ScenarioLaunch::Run(acp::steer_request),
        fence: no_fence,
        body: ScenarioBody::Acp(|s, i| Box::pin(acp::steer(s, i))),
    },
    ScenarioSpec {
        name: "model-discovery",
        purpose: "capture Claude's token-free model initialize reply",
        provider: Provider::Claude,
        runtime_mode: None,
        requirements: Requirements {
            needs_empty_claude_config: true,
            ..Requirements::discovery()
        },
        launch: ScenarioLaunch::Discovery(claude::model_discovery_launch),
        fence: no_fence,
        body: ScenarioBody::Claude(|s, i| Box::pin(claude::model_discovery(s, i))),
    },
    ScenarioSpec {
        name: "command-discovery",
        purpose: "capture Claude's cwd-scoped command initialize reply",
        provider: Provider::Claude,
        runtime_mode: None,
        requirements: Requirements {
            needs_cwd: true,
            ..Requirements::discovery()
        },
        // Visibly different from the `model-discovery` rows above: this is
        // the whole point of `launch` living on the row, not the provider.
        launch: ScenarioLaunch::Discovery(claude::command_discovery_launch),
        fence: no_fence,
        body: ScenarioBody::Claude(|s, i| Box::pin(claude::command_discovery(s, i))),
    },
    ScenarioSpec {
        name: "model-discovery",
        purpose: "capture Codex initialize and paged model/list replies",
        provider: Provider::Codex,
        runtime_mode: None,
        requirements: Requirements::discovery(),
        launch: ScenarioLaunch::Discovery(codex::model_discovery_launch),
        fence: no_fence,
        body: ScenarioBody::Codex(|s, i| Box::pin(codex::model_discovery(s, i))),
    },
    ScenarioSpec {
        name: "model-discovery-logged-out",
        purpose: "capture Codex model discovery with an isolated empty Codex home",
        provider: Provider::Codex,
        runtime_mode: None,
        requirements: Requirements {
            needs_empty_codex_home: true,
            ..Requirements::discovery()
        },
        launch: ScenarioLaunch::Discovery(codex::model_discovery_launch),
        fence: no_fence,
        body: ScenarioBody::Codex(|s, i| Box::pin(codex::model_discovery(s, i))),
    },
    ScenarioSpec {
        name: "fresh-text",
        purpose: "capture a plain Claude text turn",
        provider: Provider::Claude,
        runtime_mode: Some(RuntimeMode::AutoAcceptEdits),
        requirements: Requirements::run(),
        launch: ScenarioLaunch::Run(claude::fresh_text_request),
        fence: no_fence,
        body: ScenarioBody::Claude(|s, i| Box::pin(claude::fresh_text(s, i))),
    },
    ScenarioSpec {
        name: "edit",
        purpose: "capture a Claude run that edits an existing file with Edit",
        provider: Provider::Claude,
        runtime_mode: Some(RuntimeMode::AutoAcceptEdits),
        requirements: Requirements::run(),
        launch: ScenarioLaunch::Run(claude::edit_request),
        // The one Claude row that is not `no_fence`. It seeds a file in the cwd
        // and asks the model to change it under `AutoAcceptEdits`, so there is
        // no approval channel to protect and no grant-time recheck to lean on
        // (`approval`'s protection). What is left worth guaranteeing is that
        // the run does not start inside a repository somebody cares about,
        // which is exactly what this fence already checks.
        fence: super::full_access_fence,
        body: ScenarioBody::Claude(|s, i| Box::pin(claude::edit(s, i))),
    },
    ScenarioSpec {
        name: "edit-create",
        purpose: "capture a Claude Edit call with an empty old_string against a path that has \
                   never existed (D132)",
        provider: Provider::Claude,
        runtime_mode: Some(RuntimeMode::AutoAcceptEdits),
        requirements: Requirements::run(),
        launch: ScenarioLaunch::Run(claude::edit_create_request),
        // Same reasoning as `edit` above: AutoAcceptEdits under this fence, not an approval
        // round trip, is the whole guarantee available to a run with no seeded file to protect.
        fence: super::full_access_fence,
        body: ScenarioBody::Claude(|s, i| Box::pin(claude::edit_create(s, i))),
    },
    ScenarioSpec {
        name: "edit-noop",
        purpose: "capture a Claude Edit call with old_string absent and new_string empty, \
                   on a file that exists and has been read (D17's degenerate case)",
        provider: Provider::Claude,
        runtime_mode: Some(RuntimeMode::AutoAcceptEdits),
        requirements: Requirements::run(),
        launch: ScenarioLaunch::Run(claude::edit_noop_request),
        fence: super::full_access_fence,
        body: ScenarioBody::Claude(|s, i| Box::pin(claude::edit_noop(s, i))),
    },
    ScenarioSpec {
        name: "write-overwrite",
        purpose: "capture a Claude Write call that overwrites an existing file's content (D18)",
        provider: Provider::Claude,
        runtime_mode: Some(RuntimeMode::AutoAcceptEdits),
        requirements: Requirements::run(),
        launch: ScenarioLaunch::Run(claude::write_overwrite_request),
        fence: super::full_access_fence,
        body: ScenarioBody::Claude(|s, i| Box::pin(claude::write_overwrite(s, i))),
    },
    ScenarioSpec {
        name: "approval",
        purpose: "capture a Claude run that answers Bash and Write approval requests",
        provider: Provider::Claude,
        runtime_mode: Some(RuntimeMode::ApprovalRequired),
        requirements: Requirements::run(),
        launch: ScenarioLaunch::Run(claude::approval_request),
        fence: no_fence,
        body: ScenarioBody::Claude(|s, i| Box::pin(claude::approval(s, i))),
    },
    ScenarioSpec {
        name: "resume",
        purpose: "capture a Claude run resuming an existing session",
        provider: Provider::Claude,
        runtime_mode: Some(RuntimeMode::AutoAcceptEdits),
        requirements: Requirements {
            needs_resume_id: true,
            ..Requirements::run()
        },
        launch: ScenarioLaunch::Run(claude::resume_request),
        fence: no_fence,
        body: ScenarioBody::Claude(|s, i| Box::pin(claude::resume(s, i))),
    },
    ScenarioSpec {
        name: "attachment",
        purpose: "capture a Claude run with an inlined image attachment",
        provider: Provider::Claude,
        runtime_mode: Some(RuntimeMode::AutoAcceptEdits),
        requirements: Requirements {
            needs_attachment: true,
            ..Requirements::run()
        },
        launch: ScenarioLaunch::Run(claude::attachment_request),
        fence: no_fence,
        body: ScenarioBody::Claude(|s, i| Box::pin(claude::attachment(s, i))),
    },
    ScenarioSpec {
        name: "checklist",
        purpose: "capture a Claude run driving TaskCreate/TaskUpdate",
        provider: Provider::Claude,
        runtime_mode: Some(RuntimeMode::AutoAcceptEdits),
        requirements: Requirements::run(),
        launch: ScenarioLaunch::Run(claude::checklist_request),
        fence: no_fence,
        body: ScenarioBody::Claude(|s, i| Box::pin(claude::checklist(s, i))),
    },
    ScenarioSpec {
        name: "checklist-resume",
        purpose: "capture a Claude run resuming and continuing a checklist from a second process",
        provider: Provider::Claude,
        runtime_mode: Some(RuntimeMode::AutoAcceptEdits),
        requirements: Requirements {
            needs_resume_id: true,
            ..Requirements::run()
        },
        launch: ScenarioLaunch::Run(claude::checklist_resume_request),
        fence: no_fence,
        body: ScenarioBody::Claude(|s, i| Box::pin(claude::checklist_resume(s, i))),
    },
    ScenarioSpec {
        name: "auto",
        purpose: "capture what Claude's auto permission mode puts on the wire",
        provider: Provider::Claude,
        runtime_mode: Some(RuntimeMode::Auto),
        requirements: Requirements::run(),
        launch: ScenarioLaunch::Run(claude::auto_request),
        fence: no_fence,
        body: ScenarioBody::Claude(|s, i| Box::pin(claude::auto(s, i))),
    },
    ScenarioSpec {
        name: "full-access",
        purpose: "capture what Claude's bypassPermissions mode puts on the wire",
        provider: Provider::Claude,
        runtime_mode: Some(RuntimeMode::FullAccess),
        requirements: Requirements::run(),
        launch: ScenarioLaunch::Run(claude::full_access_request),
        fence: super::full_access_fence,
        body: ScenarioBody::Claude(|s, i| Box::pin(claude::full_access(s, i))),
    },
    ScenarioSpec {
        name: "fresh-text",
        purpose: "capture a plain Codex text turn",
        provider: Provider::Codex,
        runtime_mode: Some(RuntimeMode::AutoAcceptEdits),
        requirements: Requirements::run(),
        launch: ScenarioLaunch::Run(codex::fresh_text_request),
        fence: no_fence,
        body: ScenarioBody::Codex(|s, i| Box::pin(codex::fresh_text(s, i))),
    },
    ScenarioSpec {
        name: "approval",
        purpose: "capture a Codex run that answers file-change approval requests",
        provider: Provider::Codex,
        runtime_mode: Some(RuntimeMode::ApprovalRequired),
        requirements: Requirements::run(),
        launch: ScenarioLaunch::Run(codex::approval_request),
        // Points at `codex_fence` explicitly — D79. This row's `runtime_mode` happens to be
        // `ApprovalRequired`, but that is no longer why it gets the trusted-PowerShell fence:
        // the fence is this row's own declared choice, not something re-derived from the mode.
        fence: super::codex_fence,
        body: ScenarioBody::Codex(|s, i| Box::pin(codex::approval(s, i))),
    },
    ScenarioSpec {
        name: "approval-on-request",
        purpose: "capture a Codex run that answers command-execution approval requests \
                   against an external target",
        provider: Provider::Codex,
        // AutoAcceptEdits, not ApprovalRequired: `approval-on-request` records Codex's
        // "on-request" approval path, which the production runtime enters under auto-accept —
        // see `record::scenarios::codex::approval_on_request_request`.
        runtime_mode: Some(RuntimeMode::AutoAcceptEdits),
        requirements: Requirements {
            needs_approval_target: true,
            ..Requirements::run()
        },
        launch: ScenarioLaunch::Run(codex::approval_on_request_request),
        // Points at `codex_fence` explicitly — D79. `codex_fence` distinguishes this row from
        // `approval` by `requirements.needs_approval_target`, which this row already sets below;
        // it no longer reads `runtime_mode` at all.
        fence: super::codex_fence,
        body: ScenarioBody::Codex(|s, i| Box::pin(codex::approval_on_request(s, i))),
    },
    ScenarioSpec {
        name: "resume",
        purpose: "capture a Codex run resuming an existing thread",
        provider: Provider::Codex,
        runtime_mode: Some(RuntimeMode::AutoAcceptEdits),
        requirements: Requirements {
            needs_resume_id: true,
            ..Requirements::run()
        },
        launch: ScenarioLaunch::Run(codex::resume_request),
        fence: no_fence,
        body: ScenarioBody::Codex(|s, i| Box::pin(codex::resume(s, i))),
    },
    ScenarioSpec {
        name: "steer",
        purpose: "capture a Codex run receiving a mid-turn steering message",
        provider: Provider::Codex,
        runtime_mode: Some(RuntimeMode::AutoAcceptEdits),
        requirements: Requirements::run(),
        launch: ScenarioLaunch::Run(codex::steer_request),
        fence: no_fence,
        body: ScenarioBody::Codex(|s, i| Box::pin(codex::steer(s, i))),
    },
    ScenarioSpec {
        name: "interruption",
        purpose: "capture a Codex run interrupted mid-turn",
        provider: Provider::Codex,
        runtime_mode: Some(RuntimeMode::AutoAcceptEdits),
        requirements: Requirements::run(),
        launch: ScenarioLaunch::Run(codex::interruption_request),
        fence: no_fence,
        body: ScenarioBody::Codex(|s, i| Box::pin(codex::interruption(s, i))),
    },
    ScenarioSpec {
        name: "auto",
        purpose: "capture what Codex's auto_review reviewer puts on the wire",
        provider: Provider::Codex,
        runtime_mode: Some(RuntimeMode::Auto),
        requirements: Requirements::run(),
        launch: ScenarioLaunch::Run(codex::auto_request),
        fence: no_fence,
        body: ScenarioBody::Codex(|s, i| Box::pin(codex::auto(s, i))),
    },
    ScenarioSpec {
        name: "full-access",
        purpose: "capture what Codex's danger-full-access sandbox puts on the wire",
        provider: Provider::Codex,
        runtime_mode: Some(RuntimeMode::FullAccess),
        requirements: Requirements::run(),
        launch: ScenarioLaunch::Run(codex::full_access_request),
        fence: super::full_access_fence,
        body: ScenarioBody::Codex(|s, i| Box::pin(codex::full_access(s, i))),
    },
];

/// Look up a scenario by the exact provider and name strings the binary's
/// `--help` and argument parsing use (`"claude"` | `"codex"` | `"acp"`).
pub fn scenario(provider: &str, name: &str) -> Option<&'static ScenarioSpec> {
    SCENARIOS
        .iter()
        .find(|spec| spec.provider.wire_name() == provider && spec.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Break caught: a scenario's name drifts from what the binary's
    /// `--help` and argument validation still advertise, or a duplicate row
    /// shadows another provider's scenario of the same name.
    #[test]
    fn every_scenario_name_the_binary_advertises_is_in_the_table() {
        for (provider, name) in [
            ("acp", "session-discovery-codex-acp"),
            ("acp", "session-discovery-claude-acp"),
            ("acp", "session-discovery-grok"),
            ("claude", "model-discovery"),
            ("claude", "command-discovery"),
            ("claude", "fresh-text"),
            ("claude", "approval"),
            ("claude", "resume"),
            ("claude", "attachment"),
            ("claude", "checklist"),
            ("claude", "checklist-resume"),
            ("claude", "auto"),
            ("claude", "full-access"),
            ("codex", "model-discovery"),
            ("codex", "model-discovery-logged-out"),
            ("codex", "fresh-text"),
            ("codex", "approval"),
            ("codex", "approval-on-request"),
            ("codex", "resume"),
            ("codex", "steer"),
            ("codex", "interruption"),
            ("codex", "auto"),
            ("codex", "full-access"),
            ("claude", "edit"),
            ("claude", "edit-create"),
            ("claude", "edit-noop"),
            ("claude", "write-overwrite"),
        ] {
            assert!(
                scenario(provider, name).is_some(),
                "missing table row for {provider}/{name}"
            );
        }
        assert!(scenario("claude", "no-such-scenario").is_none());
        assert!(scenario("codex", "model-discovery-logged-out").is_some());
        assert_eq!(
            SCENARIOS.len(),
            29,
            "an added or removed row must update this count too"
        );
    }

    #[test]
    fn discovery_rows_start_no_turn_and_spend_no_tokens() {
        for spec in SCENARIOS
            .iter()
            .filter(|spec| !spec.requirements.spends_tokens)
        {
            assert_eq!(spec.runtime_mode, None, "{} sets a runtime mode", spec.name);
        }
    }

    /// Break caught, verified by falsification: a run row is miscategorized as free (missing
    /// from the token-spend acknowledgment gate) or never starts a turn at all — setting
    /// `steer`'s `runtime_mode` to `None` while leaving `Requirements::run()`'s
    /// `spends_tokens: true` in place fails with "steer spends tokens but sets no runtime mode".
    /// Restored after confirming.
    #[test]
    fn run_rows_spend_tokens_and_start_a_turn() {
        for spec in SCENARIOS
            .iter()
            .filter(|spec| spec.requirements.spends_tokens)
        {
            assert!(
                spec.runtime_mode.is_some(),
                "{} spends tokens but sets no runtime mode",
                spec.name
            );
        }
    }

    /// Break caught: `needs_cwd` regresses for a cwd-independent discovery
    /// scenario, letting a caller's `--cwd` silently start influencing
    /// `model-discovery`/`model-discovery-logged-out`.
    #[test]
    fn only_the_neutral_discovery_aliases_ignore_cwd() {
        const NEUTRAL: &[&str] = &["model-discovery", "model-discovery-logged-out"];
        for spec in SCENARIOS {
            let neutral = NEUTRAL.contains(&spec.name);
            assert_eq!(
                spec.requirements.needs_cwd, !neutral,
                "{:?}/{} has an unexpected needs_cwd",
                spec.provider, spec.name
            );
        }
    }

    /// `ScenarioBody` is `pub(super)`, so this is the one place that can check it: `record()`
    /// dispatches on `spec.body`'s own variant, never on the row's declared `provider` field. A
    /// row whose `provider` disagrees with its `body` variant would silently run under the wrong
    /// provider's launch and session machinery.
    ///
    /// Break caught: swap one row's `body` for the other provider's variant while leaving
    /// `provider` alone.
    #[test]
    fn every_row_s_declared_provider_matches_its_body_variant() {
        for spec in SCENARIOS {
            let body_is_claude = matches!(spec.body, ScenarioBody::Claude(_));
            let declared_claude = spec.provider == Provider::Claude;
            assert_eq!(
                body_is_claude,
                declared_claude,
                "{:?}/{} declares provider {:?} but its body is {}",
                spec.provider,
                spec.name,
                spec.provider,
                if body_is_claude { "Claude" } else { "Codex" }
            );
        }
    }
}
