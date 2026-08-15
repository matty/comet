use std::path::PathBuf;
use std::time::Duration;

use comet_harness::capture::{CaptureConfig, Provider, SCENARIOS, record, scenario};

/// `"claude"` | `"codex"` — the string form every `--help` line, argument
/// pair and `CaptureConfig::provider` value uses.
fn provider_key(provider: Provider) -> &'static str {
    match provider {
        Provider::Claude => "claude",
        Provider::Codex => "codex",
    }
}

/// The column budget a wrapped scenario line stays under, prefix included —
/// matched to the hand-typed text this replaced, which broke around 90-95
/// characters.
const SCENARIO_LINE_WIDTH: usize = 92;

/// Generated from [`SCENARIOS`] rather than hand-typed, so a new or renamed
/// row cannot leave `--help` behind — closing D60 (the scenario name living
/// in the help text, `supported_pair()`, and the dispatch `match` as three
/// unsynchronized copies; there is now exactly one, this table). Wraps long
/// per-provider lists onto continuation lines indented to align under the
/// first name, the same shape the hand-typed text used.
fn scenario_help_lines() -> String {
    let mut lines = String::new();
    for provider in [Provider::Claude, Provider::Codex] {
        let names: Vec<&str> = SCENARIOS
            .iter()
            .filter(|spec| spec.provider == provider)
            .map(|spec| spec.name)
            .collect();
        let prefix = format!("  {:<7} ", format!("{}:", provider_key(provider)));
        let indent = " ".repeat(prefix.len());
        let mut current = prefix;
        let mut line_has_word = false;
        for (index, name) in names.iter().enumerate() {
            let word = if index + 1 == names.len() {
                (*name).to_owned()
            } else {
                format!("{name},")
            };
            if line_has_word && current.len() + word.len() > SCENARIO_LINE_WIDTH {
                lines.push_str(current.trim_end());
                lines.push('\n');
                current = indent.clone();
            }
            current.push_str(&word);
            current.push(' ');
            line_has_word = true;
        }
        lines.push_str(current.trim_end());
        lines.push('\n');
    }
    lines
}

fn help_text() -> String {
    format!(
        r#"Record a raw Claude Code or Codex provider session.

Usage:
  comet-provider-capture <PROVIDER> <SCENARIO> [OPTIONS]

Providers:
  claude    Claude Code stream-json
  codex     Codex app-server JSON-RPC

Scenarios:
{}
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
"#,
        scenario_help_lines()
    )
}

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
                print!("{}", help_text());
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

/// Look the row up, then check each `Requirements` flag against what was
/// supplied — the per-scenario `if` chain and the `ClaudeRunScript`/
/// `CodexRunScript` construction this replaced both collapse into this one
/// table-driven validation, and the scenario's own body (in
/// `comet-harness`) is what turns the result into wire traffic. This binary
/// never builds a prompt, a `RunRequest`, or a runtime mode — see
/// `record::scenarios::{claude,codex}` for those; decision "the scenario
/// owns its prompt".
fn capture_config(args: Args) -> Result<CaptureConfig, String> {
    capture_config_with_env(args, |name| std::env::var_os(name).is_some())
}

fn capture_config_with_env(
    args: Args,
    env_present: impl Fn(&str) -> bool,
) -> Result<CaptureConfig, String> {
    let provider = args.provider.as_deref().unwrap_or_default();
    let scenario_name = args.scenario.as_deref().unwrap_or_default();
    let Some(spec) = scenario(provider, scenario_name) else {
        return Err(match provider {
            "claude" | "codex" => format!(
                "Scenario {scenario_name:?} is not supported for {provider}. Run with --help to see the valid pairs."
            ),
            _ => "Provider must be claude or codex. Run with --help to see the valid pairs.".into(),
        });
    };
    let requirements = spec.requirements;

    if args.approval_target.is_some() && !requirements.needs_approval_target {
        return Err("--approval-target is only valid for codex approval-on-request.".into());
    }
    if requirements.spends_tokens && !args.acknowledge_token_spend {
        return Err(
            "This scenario can spend provider tokens. Re-run with --acknowledge-token-spend after checking the selected provider and scenario."
                .into(),
        );
    }
    let timeout_seconds = args
        .timeout_seconds
        .unwrap_or(if requirements.spends_tokens { 120 } else { 30 });
    if !(1..=300).contains(&timeout_seconds) {
        return Err("--timeout-seconds must be from 1 through 300.".into());
    }

    // Every scenario tolerates `--cwd`; only scenarios whose behavior varies
    // by cwd resolve and validate it (see `Requirements::needs_cwd`'s own
    // doc comment). An omitted `--cwd` still resolves to the caller's
    // current directory here, exactly as it always has — the scenario body
    // only falls back to a neutral temp directory when this binary hands it
    // `None`, which happens only for the cwd-independent discovery aliases.
    let cwd = if requirements.needs_cwd {
        Some(resolve_existing_directory(args.cwd.as_deref(), "--cwd")?)
    } else {
        None
    };

    let approval_target = if requirements.needs_approval_target {
        let cwd = cwd
            .as_deref()
            .expect("needs_approval_target implies needs_cwd");
        Some(validate_approval_target(
            args.approval_target.as_deref().ok_or_else(|| {
                "The approval-on-request scenario needs --approval-target with an empty external directory."
                    .to_owned()
            })?,
            cwd,
        )?)
    } else {
        None
    };

    if requirements.needs_empty_codex_home {
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

    let resume_id = if requirements.needs_resume_id {
        Some(args.resume_id.clone().ok_or_else(|| {
            format!(
                "The {} scenario needs --resume-id with a provider session/thread id.",
                spec.name
            )
        })?)
    } else {
        None
    };

    let attachment = if requirements.needs_attachment {
        Some(args.attachment.clone().ok_or_else(|| {
            "The attachment scenario needs --attachment with an image path.".to_owned()
        })?)
    } else {
        None
    };

    Ok(CaptureConfig {
        provider: provider_key(spec.provider),
        scenario_name: spec.name,
        purpose: spec.purpose,
        executable: args.executable,
        codex_home: args.codex_home,
        cwd,
        resume_id,
        attachment,
        approval_target,
        raw_root: args
            .raw_root
            .unwrap_or_else(|| PathBuf::from(".comet-provider-captures").join("raw")),
        timeout: Duration::from_secs(timeout_seconds),
    })
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
    use comet_proto::RuntimeMode;

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
        for (provider, scenario_name) in [
            ("claude", "model-discovery-neutral-cwd"),
            ("claude", "model-discovery-project-cwd"),
            ("claude", "command-discovery"),
            ("codex", "model-discovery-neutral-cwd"),
            ("codex", "model-discovery-project-cwd"),
            ("codex", "model-discovery-logged-out"),
        ] {
            assert!(
                scenario(provider, scenario_name).is_some(),
                "missing token-free capture pair {provider}/{scenario_name}"
            );
        }
    }

    /// Break caught: a discovery alias is classified as a turn merely because its name does not
    /// end in `discovery`, forcing the token-spend acknowledgment for a token-free handshake.
    #[test]
    fn configures_every_token_free_discovery_without_spend_acknowledgment() {
        let empty_home = tempfile::tempdir().unwrap();
        for (provider, scenario_name, codex_home) in [
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
            let config = capture_config(token_free_args(provider, scenario_name, codex_home))
                .unwrap_or_else(|error| panic!("{provider}/{scenario_name}: {error}"));
            assert_eq!(config.scenario_name, scenario_name);
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
        let cwd = config
            .cwd
            .expect("model-discovery-project-cwd must resolve a cwd");
        assert!(cwd.is_absolute());
        assert!(cwd.is_dir());
        #[cfg(windows)]
        assert!(!cwd.as_os_str().to_string_lossy().starts_with(r"\\?\"));
    }

    /// Break caught: a run scenario's runtime mode drifts from what the table declares — most
    /// importantly Codex `approval` staying `ApprovalRequired` (load-bearing: under
    /// `AutoAcceptEdits`, Codex never asks and the capture records nothing) while
    /// `approval-on-request` stays `AutoAcceptEdits` (Codex's on-request approval path is entered
    /// under auto-accept, not approval-required).
    #[test]
    fn scenario_names_own_their_runtime_modes() {
        for (provider, name, expected) in [
            ("claude", "fresh-text", RuntimeMode::AutoAcceptEdits),
            ("claude", "approval", RuntimeMode::ApprovalRequired),
            ("claude", "resume", RuntimeMode::AutoAcceptEdits),
            ("claude", "attachment", RuntimeMode::AutoAcceptEdits),
            ("claude", "checklist", RuntimeMode::AutoAcceptEdits),
            ("claude", "checklist-resume", RuntimeMode::AutoAcceptEdits),
            ("codex", "fresh-text", RuntimeMode::AutoAcceptEdits),
            ("codex", "approval", RuntimeMode::ApprovalRequired),
            ("codex", "approval-on-request", RuntimeMode::AutoAcceptEdits),
            ("codex", "resume", RuntimeMode::AutoAcceptEdits),
            ("codex", "steer", RuntimeMode::AutoAcceptEdits),
            ("codex", "interruption", RuntimeMode::AutoAcceptEdits),
        ] {
            let spec =
                scenario(provider, name).unwrap_or_else(|| panic!("missing {provider}/{name}"));
            assert_eq!(spec.runtime_mode, Some(expected), "{provider}/{name}");
        }
    }

    #[test]
    fn on_request_requires_a_bounded_external_empty_target() {
        assert!(scenario("claude", "approval-on-request").is_none());
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

    /// Reconstructs the comma-separated scenario list `scenario_help_lines` prints for one
    /// provider, joining its wrapped continuation lines back into one logical list — `--help`
    /// wraps a long provider's names across several physical lines (see `SCENARIO_LINE_WIDTH`),
    /// so a test that only inspected the first matching line would silently stop checking after
    /// the wrap point.
    fn advertised_scenario_names(help: &str, prefix: &str) -> Vec<String> {
        let lines: Vec<&str> = help.lines().collect();
        let start = lines
            .iter()
            .position(|line| line.trim_start().starts_with(prefix))
            .unwrap_or_else(|| panic!("--help has no scenario line starting with {prefix:?}"));
        let mut joined = lines[start]
            .trim_start()
            .strip_prefix(prefix)
            .unwrap()
            .to_owned();
        for line in &lines[start + 1..] {
            let trimmed = line.trim_start();
            if trimmed.is_empty() || trimmed.starts_with("claude:") || trimmed.starts_with("codex:")
            {
                break;
            }
            joined.push(' ');
            joined.push_str(trimmed);
        }
        joined
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .collect()
    }

    /// Closes D60: the `--help` scenario list is generated from `SCENARIOS`, not hand-typed. This
    /// test parses the printed `--help` text and compares it against `SCENARIOS` computed
    /// directly — it therefore checks `scenario_help_lines` against the table, not the table
    /// against itself; a `SCENARIOS` row that is simply wrong (missing, misnamed, wrong provider
    /// field) is `every_scenario_name_the_binary_advertises_is_in_the_table`'s and
    /// `every_row_s_declared_provider_matches_its_body_variant`'s job (`record/scenarios.rs`), not
    /// this one's. Also resolves every advertised `(provider, name)` pair through `scenario()`.
    ///
    /// Break caught, verified by falsification: `scenario_help_lines` silently drops a row (e.g.
    /// `names.pop()` before joining) — `--help` and SCENARIOS diverge for claude: left has 9
    /// names, right (the real table) has 10, and `checklist-resume` is the one missing. Restored
    /// after confirming.
    #[test]
    fn every_help_text_scenario_is_a_table_row_and_dispatches() {
        let help = help_text();
        for provider in [Provider::Claude, Provider::Codex] {
            let key = provider_key(provider);
            let prefix = format!("{key}:");
            let advertised = advertised_scenario_names(&help, &prefix);
            let table: Vec<&str> = SCENARIOS
                .iter()
                .filter(|spec| spec.provider == provider)
                .map(|spec| spec.name)
                .collect();
            assert_eq!(advertised, table, "--help and SCENARIOS diverge for {key}");
            assert!(!table.is_empty(), "{key} advertises no scenarios");
            for name in table {
                let spec = scenario(key, name)
                    .unwrap_or_else(|| panic!("{key}/{name} is advertised but does not resolve"));
                assert_eq!(
                    spec.provider, provider,
                    "{key}/{name} resolved under the wrong provider"
                );
            }
        }
    }

    /// Every one of the 20 `(provider, name)` pairs in `SCENARIOS` must build a valid
    /// `CaptureConfig` through this binary's own argument validation, given whatever its
    /// `Requirements` demand — restores what Task 2 broke: the binary routed every ported
    /// scenario into a dead end, and this is the CLI-level proof that every name is reachable
    /// again, not just the ones a hand test happens to cover.
    ///
    /// Break caught, verified by falsification: a `Requirements` gate stops being read (e.g.
    /// `needs_resume_id` hardcoded to `true`) — every row this test builds with only what its own
    /// `Requirements` says it needs fails immediately: "claude/model-discovery: The
    /// model-discovery scenario needs --resume-id with a provider session/thread id." Restored
    /// after confirming.
    #[test]
    fn every_registered_scenario_builds_a_valid_capture_config() {
        let cwd = tempfile::tempdir().unwrap();
        // `validate_approval_target` rejects anything under the system temp tree, so a plain
        // `tempfile::tempdir()` will not do here; the checkout root is outside it.
        let target = tempfile::Builder::new()
            .prefix("comet-cli-approval-target-")
            .tempdir_in(std::env::current_dir().unwrap())
            .unwrap();
        let empty_codex_home = tempfile::tempdir().unwrap();
        let attachment = tempfile::tempdir().unwrap().path().join("image.png");
        for spec in SCENARIOS {
            let provider = provider_key(spec.provider);
            let mut args = token_free_args(provider, spec.name, None);
            args.cwd = Some(cwd.path().into());
            args.acknowledge_token_spend = spec.requirements.spends_tokens;
            if spec.requirements.needs_resume_id {
                args.resume_id = Some("resume-id".into());
            }
            if spec.requirements.needs_attachment {
                args.attachment = Some(attachment.clone());
            }
            if spec.requirements.needs_approval_target {
                args.approval_target = Some(target.path().into());
            }
            if spec.requirements.needs_empty_codex_home {
                args.codex_home = Some(empty_codex_home.path().into());
            }
            let config = capture_config(args)
                .unwrap_or_else(|error| panic!("{provider}/{}: {error}", spec.name));
            assert_eq!(config.provider, provider);
            assert_eq!(config.scenario_name, spec.name);
        }
    }
}
