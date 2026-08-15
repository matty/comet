use std::path::{Path, PathBuf};
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
          command-discovery, fresh-text, approval, resume, attachment, checklist,
          checklist-resume
  codex:  model-discovery, model-discovery-neutral-cwd, model-discovery-project-cwd,
          model-discovery-logged-out, fresh-text, approval, approval-on-request, resume, steer,
          interruption

Options:
  --executable <PATH>              Override the provider executable
  --codex-home <PATH>              Override CODEX_HOME for Codex discovery
  --cwd <DIR>                      Existing working directory for the scenario
  --approval-target <DIR>          Empty external target for Codex on-request approval
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
    cwd: Option<PathBuf>,
    approval_target: Option<PathBuf>,
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
            "--cwd" => parsed.cwd = Some(value(&mut arguments, &argument)?.into()),
            "--approval-target" => {
                parsed.approval_target = Some(value(&mut arguments, &argument)?.into())
            }
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
    capture_config_with_env(args, |name| std::env::var_os(name).is_some())
}

fn capture_config_with_env(
    args: Args,
    env_present: impl Fn(&str) -> bool,
) -> Result<CaptureConfig, String> {
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
    if scenario != "approval-on-request" && args.approval_target.is_some() {
        return Err("--approval-target is only valid for codex approval-on-request.".into());
    }
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
    let cwd = resolve_existing_directory(args.cwd.as_deref(), "--cwd")?;
    let approval_target = if scenario == "approval-on-request" {
        Some(validate_approval_target(
            args.approval_target.as_deref().ok_or_else(|| {
                "The approval-on-request scenario needs --approval-target with an empty external directory."
                    .to_owned()
            })?,
            &cwd,
        )?)
    } else {
        None
    };
    if scenario == "model-discovery-logged-out" {
        if ["OPENAI_API_KEY", "CODEX_ACCESS_TOKEN"]
            .into_iter()
            .any(env_present)
        {
            return Err(
                "Logged-out discovery cannot run with ambient Codex authentication. Start it from an environment without Codex auth variables."
                    .into(),
            );
        }
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
        (
            "claude",
            name @ ("fresh-text" | "approval" | "resume" | "attachment" | "checklist"
            | "checklist-resume"),
        ) => {
            let (prompt, script) = match name {
                "fresh-text" => (
                    "Reply with the single word capture.".to_owned(),
                    ClaudeRunScript::FreshText,
                ),
                "approval" => (claude_approval_prompt(&cwd), ClaudeRunScript::Approval),
                "resume" => (
                    "Reply with the single word resumed.".to_owned(),
                    ClaudeRunScript::Resume,
                ),
                "attachment" => (
                    "Describe the attached image in one short sentence.".to_owned(),
                    ClaudeRunScript::Attachment,
                ),
                "checklist" => (claude_checklist_prompt(), ClaudeRunScript::Checklist),
                "checklist-resume" => (
                    claude_checklist_resume_prompt(),
                    ClaudeRunScript::ChecklistResume,
                ),
                _ => unreachable!(),
            };
            let mode = claude_runtime_mode(script);
            let mut request = cheap_claude_request(&prompt, cwd, mode);
            if matches!(
                script,
                ClaudeRunScript::Resume | ClaudeRunScript::ChecklistResume
            ) {
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
        (
            "codex",
            name @ ("fresh-text"
            | "approval"
            | "approval-on-request"
            | "resume"
            | "steer"
            | "interruption"),
        ) => {
            let (prompt, script) = match name {
                "fresh-text" => (
                    "Reply with the single word capture.".to_owned(),
                    CodexRunScript::FreshText,
                ),
                "approval" => (
                    comet_harness::capture::codex_approval_prompt(&cwd),
                    CodexRunScript::Approval,
                ),
                "approval-on-request" => (
                    comet_harness::capture::approval_on_request_prompt(
                        approval_target.as_deref().expect("validated target"),
                    ),
                    CodexRunScript::ApprovalOnRequest,
                ),
                "resume" => (
                    "Reply with the single word resumed.".to_owned(),
                    CodexRunScript::Resume,
                ),
                "steer" => (
                    "Begin a short response, then accept the follow-up instruction.".to_owned(),
                    CodexRunScript::Steer,
                ),
                "interruption" => (
                    "Count upward slowly and keep working until interrupted.".to_owned(),
                    CodexRunScript::Interruption,
                ),
                _ => unreachable!(),
            };
            let mode = codex_runtime_mode(script);
            let mut request = cheap_codex_request(&prompt, cwd, mode);
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
        approval_target,
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
                | "checklist"
                | "checklist-resume"
        ) | (
            "codex",
            "model-discovery"
                | "model-discovery-neutral-cwd"
                | "model-discovery-project-cwd"
                | "model-discovery-logged-out"
                | "fresh-text"
                | "approval"
                | "approval-on-request"
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
        "approval-on-request" => "approval-on-request",
        "resume" => "resume",
        "attachment" => "attachment",
        "checklist" => "checklist",
        "checklist-resume" => "checklist-resume",
        "steer" => "steer",
        "interruption" => "interruption",
        _ => unreachable!("only matched scenario names reach this helper"),
    }
}

/// Owed to Task 7: a literal duplicate of
/// `capture::record::scenarios::claude::claude_checklist_prompt`, which is
/// private to that module (decision "the scenario owns its prompt"). This
/// binary still builds its own `CaptureConfig` by hand and cannot yet reach
/// the scenario table (`SCENARIOS` has no `checklist` row until Task 7 wires
/// this binary to look scenarios up by name instead of constructing them
/// here), so until that rewiring lands, keeping this binary compiling means
/// keeping a second copy of the prompt text rather than exposing the private
/// helper across a boundary that is about to disappear.
fn claude_checklist_prompt() -> String {
    concat!(
        r#"Use ToolSearch exactly once with input {"query":"select:TaskCreate,TaskUpdate","max_results":5}. "#,
        r#"Then use TaskCreate exactly twice, first with input {"subject":"Alpha step","description":"The first step"} "#,
        r#"and then with input {"subject":"Beta step","description":"The second step"}. "#,
        r#"Then use TaskUpdate exactly once with input {"taskId":"1","status":"in_progress","activeForm":"Working the first step"}. "#,
        r#"Then use TaskUpdate exactly once with input {"taskId":"1","status":"completed"}. "#,
        r#"Do nothing else, and reply with the single word capture."#,
    )
    .to_owned()
}

/// See `claude_checklist_prompt`'s doc comment — same duplication, owed to
/// the same task.
fn claude_checklist_resume_prompt() -> String {
    concat!(
        r#"Use ToolSearch exactly once with input {"query":"select:TaskUpdate","max_results":5}. "#,
        r#"Then use TaskUpdate exactly once with input {"taskId":"2","status":"in_progress","activeForm":"Working the second step"}. "#,
        r#"Then use TaskUpdate exactly once with input {"taskId":"2","status":"completed"}. "#,
        r#"Do not create any task. Do nothing else, and reply with the single word resumed."#,
    )
    .to_owned()
}

/// Owed to Task 7 — see `claude_checklist_prompt`'s doc comment for why this
/// binary keeps its own copy rather than reaching across a boundary about to
/// disappear. A literal duplicate of
/// `capture::record::scenarios::claude::claude_approval_prompt`, moved there
/// (and out of the now-deleted `capture::approval::claude`) by the task that
/// ported the `approval` scenario.
fn claude_approval_prompt(cwd: &Path) -> String {
    let marker = cwd.join("capture-marker.txt");
    format!(
        "Use Bash exactly once with input {{\"command\":{}}}. Wait for it to finish successfully. Then use Write exactly once with input {{\"file_path\":{},\"content\":{}}}.",
        serde_json::to_string("printf capture").expect("static command serializes"),
        serde_json::to_string(&marker.display().to_string()).expect("path serializes"),
        serde_json::to_string("capture\n").expect("static content serializes"),
    )
}

fn cheap_claude_request(prompt: &str, cwd: PathBuf, mode: RuntimeMode) -> RunRequest {
    RunRequest {
        prompt: prompt.into(),
        model: Some("claude-haiku-4-5-20251001".into()),
        reasoning: Some(ReasoningLevel::Low),
        cwd: cwd.display().to_string(),
        ..RunRequest::for_session(mode)
    }
}

fn claude_runtime_mode(script: ClaudeRunScript) -> RuntimeMode {
    if matches!(script, ClaudeRunScript::Approval) {
        RuntimeMode::ApprovalRequired
    } else {
        RuntimeMode::AutoAcceptEdits
    }
}

fn codex_runtime_mode(script: CodexRunScript) -> RuntimeMode {
    if matches!(script, CodexRunScript::Approval) {
        RuntimeMode::ApprovalRequired
    } else {
        RuntimeMode::AutoAcceptEdits
    }
}

fn cheap_codex_request(prompt: &str, cwd: PathBuf, mode: RuntimeMode) -> RunRequest {
    RunRequest {
        prompt: prompt.into(),
        model: Some("gpt-5.6-luna".into()),
        reasoning: Some(ReasoningLevel::Low),
        cwd: cwd.display().to_string(),
        ..RunRequest::for_session(mode)
    }
}

fn resolve_existing_directory(
    path: Option<&std::path::Path>,
    option: &str,
) -> Result<PathBuf, String> {
    let path = match path {
        Some(path) => path.to_owned(),
        None => std::env::current_dir().map_err(|_| {
            "The current directory could not be read. Start the command from an accessible directory."
                .to_owned()
        })?,
    };
    let absolute = std::path::absolute(&path)
        .map_err(|_| format!("{option} must name an existing directory."))?;
    if !absolute.is_dir() {
        return Err(format!("{option} must name an existing directory."));
    }
    Ok(absolute)
}

fn validate_approval_target(
    path: &std::path::Path,
    cwd: &std::path::Path,
) -> Result<PathBuf, String> {
    let target = resolve_existing_directory(Some(path), "--approval-target")?;
    let canonical_target = target
        .canonicalize()
        .map_err(|_| "--approval-target must be an accessible empty directory.".to_owned())?;
    let canonical_cwd = cwd
        .canonicalize()
        .map_err(|_| "--cwd must name an existing directory.".to_owned())?;
    if canonical_target.starts_with(&canonical_cwd) || canonical_cwd.starts_with(&canonical_target)
    {
        return Err("--approval-target must be outside --cwd.".into());
    }
    let temp = std::env::temp_dir()
        .canonicalize()
        .unwrap_or_else(|_| std::env::temp_dir());
    if canonical_target.starts_with(&temp) {
        return Err(
            "--approval-target must be outside the system temporary directory tree.".into(),
        );
    }
    if target.join(".git").is_file() {
        return Err("--approval-target must not be a linked worktree.".into());
    }
    let mut entries = std::fs::read_dir(&target)
        .map_err(|_| "--approval-target must be an accessible empty directory.".to_owned())?;
    if entries.next().is_some() {
        return Err("--approval-target must be empty.".into());
    }
    Ok(target)
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

    /// Break caught: an isolated empty CODEX_HOME is still authenticated when Codex inherits a
    /// recognized ambient token, while removing env from the launch would break descriptor parity.
    #[test]
    fn logged_out_discovery_rejects_ambient_codex_auth_only_for_that_scenario() {
        let empty_home = tempfile::tempdir().unwrap();
        for variable in ["OPENAI_API_KEY", "CODEX_ACCESS_TOKEN"] {
            let logged_out = capture_config_with_env(
                token_free_args(
                    "codex",
                    "model-discovery-logged-out",
                    Some(empty_home.path().into()),
                ),
                |name| name == variable,
            );
            assert!(
                logged_out
                    .unwrap_err()
                    .contains("ambient Codex authentication")
            );

            let ordinary = capture_config_with_env(
                token_free_args("codex", "model-discovery", Some(empty_home.path().into())),
                |name| name == variable,
            );
            assert!(ordinary.is_ok(), "ordinary discovery rejected {variable}");
        }
    }

    #[test]
    fn cwd_is_resolved_to_an_existing_absolute_directory() {
        let mut args = token_free_args("claude", "model-discovery-project-cwd", None);
        args.cwd = Some(PathBuf::from("."));
        let config = capture_config(args).unwrap();
        let CaptureOperation::Claude(ClaudeCaptureOperation::ModelDiscoveryAt { cwd }) =
            config.scenario.operation
        else {
            panic!("wrong operation");
        };
        assert!(cwd.is_absolute());
        assert!(cwd.is_dir());
        #[cfg(windows)]
        assert!(!cwd.as_os_str().to_string_lossy().starts_with(r"\\?\"));
    }

    #[test]
    fn scenario_names_own_their_runtime_modes() {
        assert_eq!(
            codex_runtime_mode(CodexRunScript::ApprovalOnRequest),
            RuntimeMode::AutoAcceptEdits
        );
        for (provider, scenario, expected) in [
            ("claude", "fresh-text", RuntimeMode::AutoAcceptEdits),
            ("claude", "approval", RuntimeMode::ApprovalRequired),
            ("claude", "resume", RuntimeMode::AutoAcceptEdits),
            ("claude", "attachment", RuntimeMode::AutoAcceptEdits),
            ("codex", "fresh-text", RuntimeMode::AutoAcceptEdits),
            ("codex", "approval", RuntimeMode::ApprovalRequired),
            ("codex", "resume", RuntimeMode::AutoAcceptEdits),
            ("codex", "steer", RuntimeMode::AutoAcceptEdits),
            ("codex", "interruption", RuntimeMode::AutoAcceptEdits),
        ] {
            let mut args = token_free_args(provider, scenario, None);
            args.acknowledge_token_spend = true;
            args.resume_id = Some("resume-id".into());
            args.attachment = Some(PathBuf::from("image.png"));
            let config = capture_config(args).unwrap();
            let mode = match config.scenario.operation {
                CaptureOperation::Claude(ClaudeCaptureOperation::Run { request, .. })
                | CaptureOperation::Codex(CodexCaptureOperation::Run { request, .. }) => {
                    request.runtime_mode
                }
                _ => panic!("{provider}/{scenario} did not configure a run"),
            };
            assert_eq!(mode, expected, "{provider}/{scenario}");
        }
    }

    #[test]
    fn on_request_requires_a_bounded_external_empty_target() {
        assert!(!supported_pair("claude", "approval-on-request"));
        let mut unused = token_free_args("codex", "model-discovery", None);
        unused.approval_target = Some(PathBuf::from("."));
        assert!(capture_config(unused).unwrap_err().contains("only valid"));
        let mut args = token_free_args("codex", "approval-on-request", None);
        args.acknowledge_token_spend = true;
        assert!(
            capture_config(args)
                .unwrap_err()
                .contains("--approval-target")
        );

        let cwd = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir_in(cwd.path()).unwrap();
        let mut args = token_free_args("codex", "approval-on-request", None);
        args.acknowledge_token_spend = true;
        args.cwd = Some(cwd.path().into());
        args.approval_target = Some(target.path().into());
        assert!(capture_config(args).unwrap_err().contains("outside --cwd"));
    }

    #[test]
    fn on_request_command_quotes_a_target_with_spaces_and_quotes() {
        let target = PathBuf::from(if cfg!(windows) {
            r"C:\capture targets\O'Brien"
        } else {
            "/capture targets/O'Brien"
        });
        let prompt = comet_harness::capture::approval_on_request_prompt(&target);
        assert!(prompt.contains("approval-marker.txt"));
        if cfg!(windows) {
            assert!(
                prompt
                    .contains("-LiteralPath 'C:\\capture targets\\O''Brien\\approval-marker.txt'")
            );
            assert!(!prompt.contains("cmd.exe /C"));
        } else {
            assert!(prompt.contains("'/capture targets/O'\\''Brien/approval-marker.txt'"));
        }
    }
}
