//! The single source of truth for a capture scenario: its name, its
//! purpose, which provider it belongs to, whether it starts a turn, what the
//! caller must have collected before it may run, and its driving body.
//!
//! Closes D60 (the scenario name living in three unsynchronized places: the
//! binary's help text, its `supported_pair()`, and its dispatch `match`) —
//! all three are read off this table starting in the task that rewires the
//! binary. This task populates only the discovery rows; the run-scenario
//! tasks add the rest.

pub(super) mod claude;
pub(super) mod codex;

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use comet_proto::RuntimeMode;

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
    // Read by the scenarios that need them (`resume`/`attachment` in Tasks 2,
    // 3, 5; `approval_target` in Tasks 4, 6) — but only the SCENARIOS table
    // wiring in Task 7 makes those scenarios reachable from production code,
    // so the read stays invisible to dead-code analysis until then.
    #[allow(dead_code)]
    pub resume_id: Option<String>,
    #[allow(dead_code)]
    pub attachment: Option<PathBuf>,
    pub codex_home: Option<PathBuf>,
    #[allow(dead_code)]
    pub approval_target: Option<PathBuf>,
}

/// What the binary must have collected before this scenario may run.
///
/// Populated for every row now, per decision "one scenario table" — but read
/// starting only in the task that rewires `comet-provider-capture.rs` to
/// validate against it (closing D60), so every field but `spends_tokens`
/// (asserted directly by this file's own test) is unread until then.
#[allow(dead_code)]
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
    #[allow(dead_code)] // See the container-level note above.
    const fn discovery() -> Self {
        Self {
            spends_tokens: false,
            needs_cwd: false,
            needs_resume_id: false,
            needs_attachment: false,
            needs_approval_target: false,
            needs_empty_codex_home: false,
        }
    }
}

pub struct ScenarioSpec {
    pub name: &'static str,
    #[allow(dead_code)] // Read starting when the binary reports it in --help (Task 7).
    pub purpose: &'static str,
    pub provider: Provider,
    /// `None` for discovery scenarios, which start no turn.
    #[allow(dead_code)] // Read starting when the binary validates against it (Task 7).
    pub runtime_mode: Option<RuntimeMode>,
    #[allow(dead_code)] // Read starting when the binary validates against it (Task 7).
    pub requirements: Requirements,
    /// SPAWN, per row — not a provider trait member. See the amendment on
    /// `CaptureProvider`'s doc comment for why: which launch a scenario
    /// needs varies per scenario as well as per provider.
    pub(super) launch: fn(&ScenarioInput, &Path) -> anyhow::Result<LaunchDescriptor>,
    pub(super) body: ScenarioBody,
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
        launch: claude::model_discovery_launch,
        body: ScenarioBody::Claude(|s, i| Box::pin(claude::model_discovery(s, i))),
    },
    ScenarioSpec {
        name: "model-discovery-neutral-cwd",
        purpose: "capture Claude model discovery from a neutral working directory",
        provider: Provider::Claude,
        runtime_mode: None,
        requirements: Requirements::discovery(),
        launch: claude::model_discovery_launch,
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
        launch: claude::model_discovery_launch,
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
        launch: claude::command_discovery_launch,
        body: ScenarioBody::Claude(|s, i| Box::pin(claude::command_discovery(s, i))),
    },
    ScenarioSpec {
        name: "model-discovery",
        purpose: "capture Codex initialize and paged model/list replies",
        provider: Provider::Codex,
        runtime_mode: None,
        requirements: Requirements::discovery(),
        launch: codex::model_discovery_launch,
        body: ScenarioBody::Codex(|s, i| Box::pin(codex::model_discovery(s, i))),
    },
    ScenarioSpec {
        name: "model-discovery-neutral-cwd",
        purpose: "capture Codex model discovery from a neutral working directory",
        provider: Provider::Codex,
        runtime_mode: None,
        requirements: Requirements::discovery(),
        launch: codex::model_discovery_launch,
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
        launch: codex::model_discovery_launch,
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
        launch: codex::model_discovery_launch,
        body: ScenarioBody::Codex(|s, i| Box::pin(codex::model_discovery(s, i))),
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

    /// Break caught: a discovery scenario's name drifts from what the
    /// binary's `--help` and `supported_pair()` still advertise, or a
    /// duplicate row shadows another provider's scenario of the same name.
    #[test]
    fn every_discovery_name_the_binary_advertises_is_in_the_table() {
        for (provider, name) in [
            ("claude", "model-discovery"),
            ("claude", "model-discovery-neutral-cwd"),
            ("claude", "model-discovery-project-cwd"),
            ("claude", "command-discovery"),
            ("codex", "model-discovery"),
            ("codex", "model-discovery-neutral-cwd"),
            ("codex", "model-discovery-project-cwd"),
            ("codex", "model-discovery-logged-out"),
        ] {
            assert!(
                scenario(provider, name).is_some(),
                "missing table row for {provider}/{name}"
            );
        }
        assert!(scenario("claude", "no-such-scenario").is_none());
        assert!(scenario("codex", "model-discovery-project-cwd").is_some());
    }

    #[test]
    fn discovery_rows_start_no_turn_and_spend_no_tokens() {
        for spec in SCENARIOS {
            assert_eq!(spec.runtime_mode, None, "{} sets a runtime mode", spec.name);
            assert!(
                !spec.requirements.spends_tokens,
                "{} is marked as spending tokens",
                spec.name
            );
        }
    }
}
