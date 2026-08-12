use std::path::PathBuf;
use std::time::Duration;

use comet_harness::capture::{
    CaptureConfig, CaptureOperation, CaptureScenario, ClaudeCaptureOperation, ClaudeRunScript,
    CodexCaptureOperation, CodexRunScript, record,
};
use comet_proto::{ReasoningLevel, RunRequest, RuntimeMode};

const HELP: &str = r#"Record a raw Claude Code or Codex provider session.

Usage:
  comet-provider-capture <PROVIDER> <SCENARIO> [OPTIONS]

Providers:
  claude    Claude Code stream-json
  codex     Codex app-server JSON-RPC

Scenarios:
  claude: model-discovery, model-discovery-neutral-cwd, model-discovery-project-cwd,
          command-discovery, fresh-text, approval, resume, attachment
  codex:  model-discovery, model-discovery-neutral-cwd, model-discovery-project-cwd,
          model-discovery-logged-out, fresh-text, approval, resume, steer, interruption

Options:
  --executable <PATH>              Override the provider executable
  --codex-home <PATH>              Override CODEX_HOME for Codex discovery
  --raw-root <PATH>                Raw output root [default: .comet-provider-captures/raw]
  --timeout-seconds <SECONDS>      Hard timeout, from 1 through 300
  --acknowledge-token-spend        Required for scenarios that can call a model
  --resume-id <ID>                 Provider session/thread id required by resume
  --attachment <PATH>              Image path required by Claude attachment
  -h, --help                       Print help without looking up a provider
"#;

#[derive(Default)]
struct Args {
    provider: Option<String>,
    scenario: Option<String>,
    executable: Option<PathBuf>,
    codex_home: Option<PathBuf>,
    raw_root: Option<PathBuf>,
    timeout_seconds: Option<u64>,
    acknowledge_token_spend: bool,
    resume_id: Option<String>,
    attachment: Option<PathBuf>,
}

#[tokio::main]
async fn main() {
    let args = match parse_args() {
        Ok(Some(args)) => args,
        Ok(None) => return,
        Err(message) => exit_with(&message),
    };
    let config = match capture_config(args) {
        Ok(config) => config,
        Err(message) => exit_with(&message),
    };
    match record(config).await {
        Ok(capture) => println!(
            "Raw capture written to {} ({} events, exit {:?}).",
            capture.directory.display(),
            capture.events.len(),
            capture.exit_code
        ),
        Err(error) => exit_with(&format!("Capture failed. {error}")),
    }
}

fn parse_args() -> Result<Option<Args>, String> {
    let mut parsed = Args::default();
    let mut positional = Vec::new();
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "-h" | "--help" => {
                print!("{HELP}");
                return Ok(None);
            }
            "--acknowledge-token-spend" => parsed.acknowledge_token_spend = true,
            "--executable" => parsed.executable = Some(value(&mut arguments, &argument)?.into()),
            "--codex-home" => parsed.codex_home = Some(value(&mut arguments, &argument)?.into()),
            "--raw-root" => parsed.raw_root = Some(value(&mut arguments, &argument)?.into()),
            "--timeout-seconds" => {
                let raw = value(&mut arguments, &argument)?;
                parsed.timeout_seconds = Some(raw.parse().map_err(|_| {
                    "--timeout-seconds must be a whole number from 1 through 300.".to_owned()
                })?);
            }
            "--resume-id" => parsed.resume_id = Some(value(&mut arguments, &argument)?),
            "--attachment" => parsed.attachment = Some(value(&mut arguments, &argument)?.into()),
            option if option.starts_with('-') => {
                return Err(format!(
                    "Unknown option {option}. Run with --help to see supported options."
                ));
            }
            value => positional.push(value.to_owned()),
        }
    }
    if positional.len() != 2 {
        return Err(
            "Choose one provider and one scenario. Run with --help to see the valid pairs.".into(),
        );
    }
    parsed.provider = Some(positional.remove(0));
    parsed.scenario = Some(positional.remove(0));
    Ok(Some(parsed))
}

fn value(arguments: &mut impl Iterator<Item = String>, option: &str) -> Result<String, String> {
    arguments.next().ok_or_else(|| {
        format!("{option} needs a value. Run with --help to see an example invocation.")
    })
}

fn capture_config(args: Args) -> Result<CaptureConfig, String> {
    let provider = args.provider.as_deref().unwrap_or_default();
    let scenario = args.scenario.as_deref().unwrap_or_default();
    if !supported_pair(provider, scenario) {
        return Err(match provider {
            "claude" | "codex" => format!(
                "Scenario {scenario:?} is not supported for {provider}. Run with --help to see the valid pairs."
            ),
            _ => "Provider must be claude or codex. Run with --help to see the valid pairs.".into(),
        });
    }
    let discovery = is_discovery_pair(provider, scenario);
    if !discovery && !args.acknowledge_token_spend {
        return Err(
            "This scenario can spend provider tokens. Re-run with --acknowledge-token-spend after checking the selected provider and scenario."
                .into(),
        );
    }
    let timeout_seconds = args
        .timeout_seconds
        .unwrap_or(if discovery { 30 } else { 120 });
    if !(1..=300).contains(&timeout_seconds) {
        return Err("--timeout-seconds must be from 1 through 300.".into());
    }
    let cwd = std::env::current_dir().map_err(|_| {
        "The current directory could not be read. Start the command from an accessible directory."
            .to_owned()
    })?;
    if scenario == "model-discovery-logged-out" {
        let Some(home) = args.codex_home.as_deref() else {
            return Err(
                "Logged-out discovery needs an explicit empty --codex-home directory.".into(),
            );
        };
        let mut entries = std::fs::read_dir(home).map_err(|_| {
            "Logged-out discovery needs an existing explicit empty --codex-home directory."
                .to_owned()
        })?;
        if entries.next().is_some() {
            return Err(
                "Logged-out discovery needs an explicit empty --codex-home directory.".into(),
            );
        }
    }

    let scenario = match (provider, scenario) {
        ("claude", "model-discovery") => CaptureScenario {
            name: "model-discovery",
            purpose: "capture Claude's token-free model initialize reply",
            operation: CaptureOperation::Claude(ClaudeCaptureOperation::ModelDiscovery),
        },
        ("claude", "model-discovery-neutral-cwd") => CaptureScenario {
            name: "model-discovery-neutral-cwd",
            purpose: "capture Claude model discovery from a neutral working directory",
            operation: CaptureOperation::Claude(ClaudeCaptureOperation::ModelDiscovery),
        },
        ("claude", "model-discovery-project-cwd") => CaptureScenario {
            name: "model-discovery-project-cwd",
            purpose: "capture Claude model discovery from the selected project directory",
            operation: CaptureOperation::Claude(ClaudeCaptureOperation::ModelDiscoveryAt { cwd }),
        },
        ("claude", "command-discovery") => CaptureScenario {
            name: "command-discovery",
            purpose: "capture Claude's cwd-scoped command initialize reply",
            operation: CaptureOperation::Claude(ClaudeCaptureOperation::CommandDiscovery { cwd }),
        },
        ("claude", name @ ("fresh-text" | "approval" | "resume" | "attachment")) => {
            let (prompt, script) = match name {
                "fresh-text" => (
                    "Reply with the single word capture.",
                    ClaudeRunScript::FreshText,
                ),
                "approval" => (
                    "Use the shell to print the single word capture.",
                    ClaudeRunScript::Approval,
                ),
                "resume" => (
                    "Reply with the single word resumed.",
                    ClaudeRunScript::Resume,
                ),
                "attachment" => (
                    "Describe the attached image in one short sentence.",
                    ClaudeRunScript::Attachment,
                ),
                _ => unreachable!(),
            };
            let mut request = cheap_claude_request(prompt, cwd);
            if matches!(script, ClaudeRunScript::Resume) {
                request.resume = Some(args.resume_id.clone().ok_or_else(|| {
                    "The resume scenario needs --resume-id with a Claude session id.".to_owned()
                })?);
            }
            if matches!(script, ClaudeRunScript::Attachment) {
                request.attachments.push(
                    args.attachment
                        .clone()
                        .ok_or_else(|| {
                            "The attachment scenario needs --attachment with an image path."
                                .to_owned()
                        })?
                        .display()
                        .to_string(),
                );
            }
            CaptureScenario {
                name: canonical_scenario_name(name),
                purpose: "capture one bounded Claude run script",
                operation: CaptureOperation::Claude(ClaudeCaptureOperation::Run {
                    request,
                    script,
                }),
            }
        }
        ("codex", "model-discovery") => CaptureScenario {
            name: "model-discovery",
            purpose: "capture Codex initialize and paged model/list replies",
            operation: CaptureOperation::Codex(CodexCaptureOperation::ModelDiscovery),
        },
        ("codex", "model-discovery-neutral-cwd") => CaptureScenario {
            name: "model-discovery-neutral-cwd",
            purpose: "capture Codex model discovery from a neutral working directory",
            operation: CaptureOperation::Codex(CodexCaptureOperation::ModelDiscovery),
        },
        ("codex", "model-discovery-project-cwd") => CaptureScenario {
            name: "model-discovery-project-cwd",
            purpose: "capture Codex model discovery from the selected project directory",
            operation: CaptureOperation::Codex(CodexCaptureOperation::ModelDiscoveryAt { cwd }),
        },
        ("codex", "model-discovery-logged-out") => CaptureScenario {
            name: "model-discovery-logged-out",
            purpose: "capture Codex model discovery with an isolated empty Codex home",
            operation: CaptureOperation::Codex(CodexCaptureOperation::ModelDiscovery),
        },
        ("codex", name @ ("fresh-text" | "approval" | "resume" | "steer" | "interruption")) => {
            let (prompt, script) = match name {
                "fresh-text" => (
                    "Reply with the single word capture.",
                    CodexRunScript::FreshText,
                ),
                "approval" => (
                    "Use the shell to print the single word capture.",
                    CodexRunScript::Approval,
                ),
                "resume" => (
                    "Reply with the single word resumed.",
                    CodexRunScript::Resume,
                ),
                "steer" => (
                    "Begin a short response, then accept the follow-up instruction.",
                    CodexRunScript::Steer,
                ),
                "interruption" => (
                    "Count upward slowly and keep working until interrupted.",
                    CodexRunScript::Interruption,
                ),
                _ => unreachable!(),
            };
            let mut request = cheap_codex_request(prompt, cwd);
            if matches!(script, CodexRunScript::Resume) {
                request.resume = Some(args.resume_id.clone().ok_or_else(|| {
                    "The resume scenario needs --resume-id with a Codex thread id.".to_owned()
                })?);
            }
            CaptureScenario {
                name: canonical_scenario_name(name),
                purpose: "capture one bounded Codex run script",
                operation: CaptureOperation::Codex(CodexCaptureOperation::Run { request, script }),
            }
        }
        _ => unreachable!("provider/scenario pair was validated"),
    };

    Ok(CaptureConfig {
        scenario,
        executable: args.executable,
        codex_home: args.codex_home,
        raw_root: args
            .raw_root
            .unwrap_or_else(|| PathBuf::from(".comet-provider-captures").join("raw")),
        timeout: Duration::from_secs(timeout_seconds),
    })
}

fn supported_pair(provider: &str, scenario: &str) -> bool {
    matches!(
        (provider, scenario),
        (
            "claude",
            "model-discovery"
                | "model-discovery-neutral-cwd"
                | "model-discovery-project-cwd"
                | "command-discovery"
                | "fresh-text"
                | "approval"
                | "resume"
                | "attachment"
        ) | (
            "codex",
            "model-discovery"
                | "model-discovery-neutral-cwd"
                | "model-discovery-project-cwd"
                | "model-discovery-logged-out"
                | "fresh-text"
                | "approval"
                | "resume"
                | "steer"
                | "interruption"
        )
    )
}

fn is_discovery_pair(provider: &str, scenario: &str) -> bool {
    matches!(
        (provider, scenario),
        (
            "claude",
            "model-discovery"
                | "model-discovery-neutral-cwd"
                | "model-discovery-project-cwd"
                | "command-discovery"
        ) | (
            "codex",
            "model-discovery"
                | "model-discovery-neutral-cwd"
                | "model-discovery-project-cwd"
                | "model-discovery-logged-out"
        )
    )
}

fn canonical_scenario_name(name: &str) -> &'static str {
    match name {
        "fresh-text" => "fresh-text",
        "approval" => "approval",
        "resume" => "resume",
        "attachment" => "attachment",
        "steer" => "steer",
        "interruption" => "interruption",
        _ => unreachable!("only matched scenario names reach this helper"),
    }
}

fn cheap_claude_request(prompt: &str, cwd: PathBuf) -> RunRequest {
    RunRequest {
        prompt: prompt.into(),
        model: Some("claude-haiku-4-5-20251001".into()),
        reasoning: Some(ReasoningLevel::Low),
        cwd: cwd.display().to_string(),
        ..RunRequest::for_session(RuntimeMode::ApprovalRequired)
    }
}

fn cheap_codex_request(prompt: &str, cwd: PathBuf) -> RunRequest {
    RunRequest {
        prompt: prompt.into(),
        model: Some("gpt-5.6-luna".into()),
        reasoning: Some(ReasoningLevel::Low),
        cwd: cwd.display().to_string(),
        ..RunRequest::for_session(RuntimeMode::ApprovalRequired)
    }
}

fn exit_with(message: &str) -> ! {
    eprintln!("{message}");
    std::process::exit(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token_free_args(provider: &str, scenario: &str, codex_home: Option<PathBuf>) -> Args {
        Args {
            provider: Some(provider.into()),
            scenario: Some(scenario.into()),
            codex_home,
            timeout_seconds: Some(5),
            ..Args::default()
        }
    }

    /// Break caught: the CLI cannot reproduce the cwd and logged-out comparisons named by the
    /// corpus even though each observation is a token-free production discovery handshake.
    #[test]
    fn supports_every_token_free_discovery_observation() {
        for (provider, scenario) in [
            ("claude", "model-discovery-neutral-cwd"),
            ("claude", "model-discovery-project-cwd"),
            ("claude", "command-discovery"),
            ("codex", "model-discovery-neutral-cwd"),
            ("codex", "model-discovery-project-cwd"),
            ("codex", "model-discovery-logged-out"),
        ] {
            assert!(
                supported_pair(provider, scenario),
                "missing token-free capture pair {provider}/{scenario}"
            );
        }
    }

    /// Break caught: a discovery alias is classified as a turn merely because its name does not
    /// end in `discovery`, forcing the token-spend acknowledgment for a token-free handshake.
    #[test]
    fn configures_every_token_free_discovery_without_spend_acknowledgment() {
        let empty_home = tempfile::tempdir().unwrap();
        for (provider, scenario, codex_home) in [
            ("claude", "model-discovery-neutral-cwd", None),
            ("claude", "model-discovery-project-cwd", None),
            ("claude", "command-discovery", None),
            (
                "codex",
                "model-discovery-neutral-cwd",
                Some(empty_home.path().into()),
            ),
            (
                "codex",
                "model-discovery-project-cwd",
                Some(empty_home.path().into()),
            ),
            (
                "codex",
                "model-discovery-logged-out",
                Some(empty_home.path().into()),
            ),
        ] {
            let config = capture_config(token_free_args(provider, scenario, codex_home))
                .unwrap_or_else(|error| panic!("{provider}/{scenario}: {error}"));
            assert_eq!(config.scenario.name, scenario);
        }
    }

    /// Break caught: logged-out evidence accidentally uses the real Codex home or a directory
    /// with account/config state, making the observation neither isolated nor reproducible.
    #[test]
    fn logged_out_discovery_requires_an_explicit_empty_codex_home() {
        let missing = capture_config(token_free_args("codex", "model-discovery-logged-out", None));
        assert!(missing.unwrap_err().contains("explicit empty --codex-home"));

        let nonempty = tempfile::tempdir().unwrap();
        std::fs::write(nonempty.path().join("config.toml"), "model = 'example'").unwrap();
        let populated = capture_config(token_free_args(
            "codex",
            "model-discovery-logged-out",
            Some(nonempty.path().into()),
        ));
        assert!(
            populated
                .unwrap_err()
                .contains("explicit empty --codex-home")
        );
    }
}
