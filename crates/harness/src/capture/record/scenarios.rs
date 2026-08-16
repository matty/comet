//! The single source of truth for a capture scenario: its name, its
//! purpose, which provider it belongs to, whether it starts a turn, what the
//! caller must have collected before it may run, and its driving body.
//!
//! Closes D60 (the scenario name living in three unsynchronized places: the
//! binary's help text, its `supported_pair()`, and its dispatch `match`) —
//! `comet-provider-capture.rs` generates its `--help` text and validates its
//! arguments off this table, and `record()` dispatches off it directly by
//! `(provider, name)`. There is no other place a scenario's name is spelled.

pub(super) mod claude;
pub(super) mod codex;

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use comet_proto::{RunRequest, RuntimeMode};

use super::providers::claude::ClaudeProvider;
use super::providers::codex::CodexProvider;
use super::session::Session;
use crate::capture::Provider;
use crate::launch::LaunchDescriptor;

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
    pub approval_target: Option<PathBuf>,
}

/// What the binary must have collected before this scenario may run — read
/// by `comet-provider-capture.rs`'s argument validation, which looks the row
/// up by `(provider, name)` and checks each flag against what was supplied
/// instead of hand-coding a per-scenario `if` chain.
///
/// `needs_cwd` is not "may I pass --cwd" (every scenario tolerates the
/// flag); it is "does this scenario's behavior vary by cwd at all". False
/// only for the cwd-independent discovery aliases (`model-discovery`,
/// `model-discovery-neutral-cwd`, and Codex's `model-discovery-logged-out`)
/// — those always run from a neutral temp directory regardless of what
/// `--cwd` names, which is the entire reason `model-discovery-neutral-cwd`
/// is a distinct scenario from `model-discovery-project-cwd`. True
/// everywhere else, including every run scenario: an omitted `--cwd` still
/// resolves to the caller's current directory for those, not a temp
/// directory, so `model-discovery-project-cwd`'s point (capturing discovery
/// from the selected project directory) survives being run with no explicit
/// `--cwd` at all.
#[derive(Clone, Copy, Debug)]
pub struct Requirements {
    pub spends_tokens: bool,
    pub needs_cwd: bool,
    pub needs_resume_id: bool,
    pub needs_attachment: bool,
    pub needs_approval_target: bool,
    pub needs_empty_codex_home: bool,
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
    pub(super) body: ScenarioBody,
}

/// What a row's `launch` needs to build. A discovery scenario resolves a
/// `LaunchDescriptor` directly — it never starts a turn, so there is no
/// `RunRequest` to speak of. A run scenario instead names a builder that
/// produces a `RunRequest`, and `record.rs` (`derive_launch`) is the ONLY
/// caller that ever invokes it: it builds the `RunRequest` once, derives the
/// launch from it via the provider's own `run_launch`, and hands that same
/// `RunRequest` to the scenario body through `Session::request`. Before this
/// enum existed, a row's `launch` field was always a bare
/// `fn(&ScenarioInput, &Path) -> anyhow::Result<LaunchDescriptor>` — every run
/// scenario's `*_launch` wrapper called its own `*_request` builder to
/// satisfy that shape, and the scenario body separately called the same
/// builder again to build its wire line. Two independent calls agreed only
/// because every builder happened to be a pure function of `input`; nothing
/// enforced it. Routing both the launch and the body through one call closes
/// that hole structurally instead of by convention.
#[derive(Clone, Copy)]
pub(super) enum ScenarioLaunch {
    Run(fn(&ScenarioInput) -> anyhow::Result<RunRequest>),
    Discovery(fn(&ScenarioInput, &Path) -> anyhow::Result<LaunchDescriptor>),
}

type BoxedFuture<'a> = Pin<Box<dyn Future<Output = anyhow::Result<()>> + 'a>>;

pub(super) enum ScenarioBody {
    Claude(for<'a> fn(&'a mut Session<ClaudeProvider>, &'a ScenarioInput) -> BoxedFuture<'a>),
    Codex(for<'a> fn(&'a mut Session<CodexProvider>, &'a ScenarioInput) -> BoxedFuture<'a>),
}

pub const SCENARIOS: &[ScenarioSpec] = &[
    ScenarioSpec {
        name: "model-discovery",
        purpose: "capture Claude's token-free model initialize reply",
        provider: Provider::Claude,
        runtime_mode: None,
        requirements: Requirements::discovery(),
        launch: ScenarioLaunch::Discovery(claude::model_discovery_launch),
        body: ScenarioBody::Claude(|s, i| Box::pin(claude::model_discovery(s, i))),
    },
    ScenarioSpec {
        name: "model-discovery-neutral-cwd",
        purpose: "capture Claude model discovery from a neutral working directory",
        provider: Provider::Claude,
        runtime_mode: None,
        requirements: Requirements::discovery(),
        launch: ScenarioLaunch::Discovery(claude::model_discovery_launch),
        body: ScenarioBody::Claude(|s, i| Box::pin(claude::model_discovery(s, i))),
    },
    ScenarioSpec {
        name: "model-discovery-project-cwd",
        purpose: "capture Claude model discovery from the selected project directory",
        provider: Provider::Claude,
        runtime_mode: None,
        requirements: Requirements {
            needs_cwd: true,
            ..Requirements::discovery()
        },
        launch: ScenarioLaunch::Discovery(claude::model_discovery_launch),
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
        body: ScenarioBody::Claude(|s, i| Box::pin(claude::command_discovery(s, i))),
    },
    ScenarioSpec {
        name: "model-discovery",
        purpose: "capture Codex initialize and paged model/list replies",
        provider: Provider::Codex,
        runtime_mode: None,
        requirements: Requirements::discovery(),
        launch: ScenarioLaunch::Discovery(codex::model_discovery_launch),
        body: ScenarioBody::Codex(|s, i| Box::pin(codex::model_discovery(s, i))),
    },
    ScenarioSpec {
        name: "model-discovery-neutral-cwd",
        purpose: "capture Codex model discovery from a neutral working directory",
        provider: Provider::Codex,
        runtime_mode: None,
        requirements: Requirements::discovery(),
        launch: ScenarioLaunch::Discovery(codex::model_discovery_launch),
        body: ScenarioBody::Codex(|s, i| Box::pin(codex::model_discovery(s, i))),
    },
    ScenarioSpec {
        name: "model-discovery-project-cwd",
        purpose: "capture Codex model discovery from the selected project directory",
        provider: Provider::Codex,
        runtime_mode: None,
        requirements: Requirements {
            needs_cwd: true,
            ..Requirements::discovery()
        },
        launch: ScenarioLaunch::Discovery(codex::model_discovery_launch),
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
        body: ScenarioBody::Codex(|s, i| Box::pin(codex::model_discovery(s, i))),
    },
    ScenarioSpec {
        name: "fresh-text",
        purpose: "capture a plain Claude text turn",
        provider: Provider::Claude,
        runtime_mode: Some(RuntimeMode::AutoAcceptEdits),
        requirements: Requirements::run(),
        launch: ScenarioLaunch::Run(claude::fresh_text_request),
        body: ScenarioBody::Claude(|s, i| Box::pin(claude::fresh_text(s, i))),
    },
    ScenarioSpec {
        name: "approval",
        purpose: "capture a Claude run that answers Bash and Write approval requests",
        provider: Provider::Claude,
        runtime_mode: Some(RuntimeMode::ApprovalRequired),
        requirements: Requirements::run(),
        launch: ScenarioLaunch::Run(claude::approval_request),
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
        body: ScenarioBody::Claude(|s, i| Box::pin(claude::attachment(s, i))),
    },
    ScenarioSpec {
        name: "checklist",
        purpose: "capture a Claude run driving TaskCreate/TaskUpdate",
        provider: Provider::Claude,
        runtime_mode: Some(RuntimeMode::AutoAcceptEdits),
        requirements: Requirements::run(),
        launch: ScenarioLaunch::Run(claude::checklist_request),
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
        body: ScenarioBody::Claude(|s, i| Box::pin(claude::checklist_resume(s, i))),
    },
    ScenarioSpec {
        name: "fresh-text",
        purpose: "capture a plain Codex text turn",
        provider: Provider::Codex,
        runtime_mode: Some(RuntimeMode::AutoAcceptEdits),
        requirements: Requirements::run(),
        launch: ScenarioLaunch::Run(codex::fresh_text_request),
        body: ScenarioBody::Codex(|s, i| Box::pin(codex::fresh_text(s, i))),
    },
    ScenarioSpec {
        name: "approval",
        purpose: "capture a Codex run that answers file-change approval requests",
        provider: Provider::Codex,
        runtime_mode: Some(RuntimeMode::ApprovalRequired),
        requirements: Requirements::run(),
        launch: ScenarioLaunch::Run(codex::approval_request),
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
        body: ScenarioBody::Codex(|s, i| Box::pin(codex::resume(s, i))),
    },
    ScenarioSpec {
        name: "steer",
        purpose: "capture a Codex run receiving a mid-turn steering message",
        provider: Provider::Codex,
        runtime_mode: Some(RuntimeMode::AutoAcceptEdits),
        requirements: Requirements::run(),
        launch: ScenarioLaunch::Run(codex::steer_request),
        body: ScenarioBody::Codex(|s, i| Box::pin(codex::steer(s, i))),
    },
    ScenarioSpec {
        name: "interruption",
        purpose: "capture a Codex run interrupted mid-turn",
        provider: Provider::Codex,
        runtime_mode: Some(RuntimeMode::AutoAcceptEdits),
        requirements: Requirements::run(),
        launch: ScenarioLaunch::Run(codex::interruption_request),
        body: ScenarioBody::Codex(|s, i| Box::pin(codex::interruption(s, i))),
    },
];

/// Look up a scenario by the exact provider and name strings the binary's
/// `--help` and argument parsing use (`"claude"` | `"codex"`).
pub fn scenario(provider: &str, name: &str) -> Option<&'static ScenarioSpec> {
    SCENARIOS.iter().find(|spec| {
        let spec_provider = match spec.provider {
            Provider::Claude => "claude",
            Provider::Codex => "codex",
        };
        spec_provider == provider && spec.name == name
    })
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
            ("claude", "model-discovery"),
            ("claude", "model-discovery-neutral-cwd"),
            ("claude", "model-discovery-project-cwd"),
            ("claude", "command-discovery"),
            ("claude", "fresh-text"),
            ("claude", "approval"),
            ("claude", "resume"),
            ("claude", "attachment"),
            ("claude", "checklist"),
            ("claude", "checklist-resume"),
            ("codex", "model-discovery"),
            ("codex", "model-discovery-neutral-cwd"),
            ("codex", "model-discovery-project-cwd"),
            ("codex", "model-discovery-logged-out"),
            ("codex", "fresh-text"),
            ("codex", "approval"),
            ("codex", "approval-on-request"),
            ("codex", "resume"),
            ("codex", "steer"),
            ("codex", "interruption"),
        ] {
            assert!(
                scenario(provider, name).is_some(),
                "missing table row for {provider}/{name}"
            );
        }
        assert!(scenario("claude", "no-such-scenario").is_none());
        assert!(scenario("codex", "model-discovery-project-cwd").is_some());
        assert_eq!(
            SCENARIOS.len(),
            20,
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
    /// alias, letting a caller's `--cwd` silently start influencing
    /// `model-discovery`/`model-discovery-neutral-cwd`/
    /// `model-discovery-logged-out` and erasing their distinction from
    /// `model-discovery-project-cwd`.
    #[test]
    fn only_the_neutral_discovery_aliases_ignore_cwd() {
        const NEUTRAL: &[&str] = &[
            "model-discovery",
            "model-discovery-neutral-cwd",
            "model-discovery-logged-out",
        ];
        for spec in SCENARIOS {
            let neutral = NEUTRAL.contains(&spec.name);
            assert_eq!(
                spec.requirements.needs_cwd, !neutral,
                "{:?}/{} has an unexpected needs_cwd",
                spec.provider, spec.name
            );
        }
    }

    /// `ScenarioBody` is `pub(super)`, invisible outside `capture::record`,
    /// so this is the one place that can check it: `record()` dispatches on
    /// `spec.body`'s own variant (`ScenarioBody::Claude` → `record_claude`,
    /// `ScenarioBody::Codex` → `record_codex`), never on the row's declared
    /// `provider` field. A row whose `provider` disagrees with its `body`
    /// variant would silently run under the wrong provider's launch and
    /// session machinery — this is the falsification target the task brief
    /// names directly ("add a row whose body is the wrong provider
    /// variant").
    ///
    /// Break caught: swap one row's `body` for the other provider's variant
    /// while leaving `provider` alone.
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
