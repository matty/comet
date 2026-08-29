use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::bail;

use crate::capture::record::provider::CaptureProvider;
use crate::capture::record::providers::acp::{AcpProvider, rpc_request};
use crate::capture::record::scenarios::ScenarioInput;
use crate::capture::record::session::Session;
use crate::launch::{LaunchDescriptor, StdioMode};

/// The npm package whose `dist/index.js` each adapter row spawns. The corpus
/// version directory is named for the adapter, not for the CLI behind it, so
/// two rows recording the same ACP surface through different agents stay
/// separable.
pub(in crate::capture::record) const CODEX_ACP_PACKAGE: &str = "@agentclientprotocol/codex-acp";
pub(in crate::capture::record) const CLAUDE_ACP_PACKAGE: &str =
    "@agentclientprotocol/claude-agent-acp";

/// SPAWN for every ACP adapter row.
///
/// **Spawns `node <package>/dist/index.js`, never the npm shim.** On Windows the
/// installed entry point is a `.cmd`, and spawning a `.cmd` without a shell
/// fails `EINVAL` outright — measured 2026-08-28, Node 24.16.0. Going through a
/// shell to fix that would put a `cmd.exe` between Comet and the agent, which
/// changes signal delivery and argument quoting on the one platform no PR check
/// covers. Resolving to the JS entry sidesteps both: it is a plain executable
/// spawn on every OS, and the argv the corpus records is the argv that ran.
///
/// `COMET_ACP_ADAPTER_ROOT` overrides the search root, which is what a capture
/// on a machine with a non-standard npm prefix needs.
fn adapter_launch(
    package: &str,
    input: &ScenarioInput,
    node: &Path,
) -> anyhow::Result<LaunchDescriptor> {
    let node = node.to_path_buf();
    let entry = adapter_entry(package)?;
    let cwd = input.cwd.clone().unwrap_or_else(std::env::temp_dir);
    Ok(LaunchDescriptor {
        program: node,
        args: vec![entry.into_os_string()],
        cwd: Some(cwd),
        configured_env: BTreeMap::new(),
        stdin: StdioMode::Piped,
        stdout: StdioMode::Piped,
        stderr: StdioMode::Piped,
        kill_on_drop: true,
        #[cfg(windows)]
        creation_flags: 0,
    })
}

/// `<root>/<package>/dist/index.js`, where root is `COMET_ACP_ADAPTER_ROOT` or
/// the npm global `node_modules`.
fn adapter_entry(package: &str) -> anyhow::Result<PathBuf> {
    let root = match std::env::var_os("COMET_ACP_ADAPTER_ROOT") {
        Some(root) => PathBuf::from(root),
        None => npm_global_root()?,
    };
    let entry = root.join(package).join("dist").join("index.js");
    if !entry.is_file() {
        bail!(
            "ACP adapter {package} was not found at {}. Install it with `npm i -g {package}`, or point COMET_ACP_ADAPTER_ROOT at the directory holding it.",
            entry.display()
        );
    }
    Ok(entry)
}

/// Serializes every test in this crate that mutates the process-wide
/// `COMET_ACP_ADAPTER_ROOT` [`adapter_entry`] reads.
///
/// **Redundant under the documented gate, load-bearing under the other one
/// that still runs.** `cargo nextest run` (`.config/nextest.toml`) gives
/// each test its own process, so no two of these tests are ever live at
/// once and this lock never contends -- true today, and the reason the
/// three call sites below could each carry their own "single-threaded"
/// SAFETY comment in isolation. But `cargo nextest run -p comet-harness`'s
/// sibling, plain `cargo test -p comet-harness`, is still directly
/// runnable and puts every test in this crate's default unit-test binary
/// in ONE process, by default across multiple threads -- and under that,
/// two of these tests can genuinely interleave their `set_var`/`remove_var`
/// pairs and read back each other's override. Finding 5 of the ACP
/// whole-branch review (2026-08-29) named this gap on
/// `every_scenario_launch_matches_its_committed_corpus_manifest`
/// (`record.rs`) specifically; the other two call sites here share the
/// exact same env var and the exact same exposure, so they take the same
/// lock rather than leaving two of three sites fixed and one not.
///
/// **Not a soundness proof for the `unsafe` `set_var`/`remove_var` calls it
/// guards.** Their real requirement, per `std::env::set_var`'s own doc, is
/// exclusion of every concurrent access to the process environment — any
/// key, not just a concurrent writer of this one. This lock only excludes
/// the three call sites known to touch `COMET_ACP_ADAPTER_ROOT`; a strict
/// improvement over the single-test SAFETY reasoning that stood here
/// before, not a guarantee that no other test anywhere in this crate reads
/// or writes any env var while one of these three runs.
#[cfg(test)]
pub(crate) static ADAPTER_ROOT_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// npm's global `node_modules`. Read from the environment rather than by
/// shelling out to `npm root -g`: a capture must not depend on a second child
/// process whose own output would not be recorded.
fn npm_global_root() -> anyhow::Result<PathBuf> {
    if let Some(prefix) = std::env::var_os("npm_config_prefix") {
        let prefix = PathBuf::from(prefix);
        return Ok(if cfg!(windows) {
            prefix.join("node_modules")
        } else {
            prefix.join("lib").join("node_modules")
        });
    }
    if cfg!(windows)
        && let Some(appdata) = std::env::var_os("APPDATA")
    {
        return Ok(PathBuf::from(appdata).join("npm").join("node_modules"));
    }
    bail!(
        "npm's global node_modules could not be located. Set COMET_ACP_ADAPTER_ROOT to the directory holding the adapter package."
    )
}

/// Grok Build's ACP entry point, verified against grok 1.0.5 on 2026-08-28.
///
/// **Every token here was checked against the installed build, because the
/// placement is not guessable.** `--no-auto-update` is a TOP-LEVEL flag and is
/// hidden — it appears in neither `grok --help` nor `grok agent --help`, and the
/// only way to tell it from a typo is that clap rejects unknown flags (a
/// `--no-such-flag` control errors with "unexpected argument"; this one exits
/// 0). `--no-leader` is on the `agent` SUBCOMMAND, not the top level, and
/// `stdio` is a sub-subcommand of `agent`. Reordering any of the four breaks the
/// spawn.
///
/// `--no-leader` is the load-bearing one: without it, `agent stdio` may attach
/// to a shared leader process over `~/.grok/leader.sock` instead of starting its
/// own agent, and the capture would then record a session belonging to somebody
/// else's process.
pub(in crate::capture::record) const GROK_ARGS: [&str; 4] =
    ["--no-auto-update", "agent", "--no-leader", "stdio"];

/// SPAWN for the Grok row.
///
/// Unlike the adapter rows, `executable` here is a real agent CLI rather than
/// node — Grok is a native binary that speaks ACP itself. The ACP provider's
/// DEFAULT executable is still node (the adapters are the common case), so a
/// Grok capture must pass `--executable <grok>`; the guard below turns
/// forgetting that into a sentence naming the flag rather than a node process
/// that exits on an argument it cannot parse.
pub(in crate::capture::record) fn grok_launch(
    input: &ScenarioInput,
    executable: &Path,
) -> anyhow::Result<LaunchDescriptor> {
    let looks_like_grok = executable
        .file_stem()
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| stem.eq_ignore_ascii_case("grok"));
    if !looks_like_grok {
        bail!(
            "the grok row spawns the grok CLI, but --executable resolved to {}. Pass --executable <path to grok>; the ACP default resolves node, which is right for the adapter rows and wrong for this one.",
            executable.display()
        );
    }
    Ok(LaunchDescriptor {
        program: executable.to_path_buf(),
        args: GROK_ARGS.iter().map(Into::into).collect(),
        cwd: Some(input.cwd.clone().unwrap_or_else(std::env::temp_dir)),
        configured_env: BTreeMap::new(),
        stdin: StdioMode::Piped,
        stdout: StdioMode::Piped,
        stderr: StdioMode::Piped,
        kill_on_drop: true,
        #[cfg(windows)]
        creation_flags: 0,
    })
}

/// `executable` is **node**, not an agent CLI: an ACP adapter is a Node program
/// and the recorder spawns the interpreter directly. The capture binary resolves
/// it the same way it resolves any provider executable, so `--executable` points
/// at node for these rows.
pub(in crate::capture::record) fn codex_acp_launch(
    input: &ScenarioInput,
    executable: &Path,
) -> anyhow::Result<LaunchDescriptor> {
    adapter_launch(CODEX_ACP_PACKAGE, input, executable)
}

pub(in crate::capture::record) fn claude_acp_launch(
    input: &ScenarioInput,
    executable: &Path,
) -> anyhow::Result<LaunchDescriptor> {
    adapter_launch(CLAUDE_ACP_PACKAGE, input, executable)
}

/// Handshake, then open one session.
///
/// Token-free by construction: `initialize` and `session/new` set up the
/// conversation and never reach a model. That is what makes this a discovery
/// row rather than a run row, and it is why the capability surface — which
/// `sessionCapabilities` the agent has, whether `_meta.steering` is supported,
/// which `authMethods` it offers — can be recorded without spending anything.
pub(in crate::capture::record) async fn session_discovery(
    session: &mut Session<AcpProvider>,
    input: &ScenarioInput,
) -> anyhow::Result<()> {
    AcpProvider::handshake(session, input).await?;

    let cwd = input
        .cwd
        .clone()
        .unwrap_or_else(std::env::temp_dir)
        .to_string_lossy()
        .into_owned();
    let id = session.provider.next_id();
    session
        .send(&rpc_request(
            id,
            "session/new",
            crate::acp::new_session_params(&cwd),
        ))
        .await?;
    session
        .wait_for("JSON-RPC reply", |frame| {
            (frame["id"].as_u64() == Some(id)).then_some(())
        })
        .await
}

/// Resolve `node` from PATH. An ACP adapter is a Node program, so this is the
/// "default executable" an ACP row records against; `--executable` still wins.
///
/// Written here rather than pulled in as a dependency because it is the only
/// PATH walk in the crate — every other harness resolves a CLI through its own
/// provider-specific lookup.
pub(in crate::capture::record) fn resolve_node_executable() -> Option<PathBuf> {
    let name = if cfg!(windows) { "node.exe" } else { "node" };
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(name))
            .find(|candidate| candidate.is_file())
    })
}

/// SPAWN for the two Grok run rows (`run-grok`, `steer-grok`).
///
/// Delegates to production's own [`crate::acp::grok::run_launch`] — the same
/// launch `GrokHarness` uses for a real session — rather than building argv a
/// second time here. That builder's own doc comment records why: a recorder
/// with its own copy would be evidence of the recorder, not of Comet.
///
/// **codex-acp and claude-agent-acp never reach this function.** Both rows
/// stay `ScenarioLaunch::Discovery` (`codex_acp_launch`/`claude_acp_launch`
/// above) — `derive_launch` only calls a `Run` launch for
/// `ScenarioLaunch::Run` rows, and neither has a production `Harness` to
/// register one against: `comet_proto::agent::HarnessId` has no
/// `CodexAcp`/`ClaudeAgentAcp` variant, only `ClaudeCode`, `Codex`, `Cursor`,
/// `Grok`, `Hermes` and `Mock` — check that enum, not a design document, if
/// this ever needs re-verifying.
///
/// **This hard-delegates to Grok specifically, not to "whichever ACP agent
/// the row names."** Fine today because Grok is the only ACP agent with a
/// `Run` row registered in `SCENARIOS` — Hermes already has its own
/// production `HermesHarness` and its own `crate::acp::hermes::run_launch`
/// (landed in a parallel PR before this one), but no capture scenario row
/// yet. The day one is added, wiring it to THIS function unchanged would
/// silently derive Grok's argv for a Hermes recording. Nothing pins that
/// today the way `every_acp_row_is_discovery` used to pin "no run rows
/// exist at all": a Hermes row wired to this function fails loudly instead,
/// at `every_scenario_launch_matches_its_committed_corpus_manifest`'s argv
/// comparison (Hermes's launch and Grok's would disagree) — deliberately not
/// worth a real dispatch mechanism for a row that does not exist yet.
pub(in crate::capture::record) fn run_launch(
    executable: &Path,
    request: &comet_proto::RunRequest,
) -> LaunchDescriptor {
    crate::acp::grok::run_launch(executable, request)
}

/// The cheap text turn `run-grok` records: a fresh session, one prompt, one
/// reply. `model`/`reasoning` are left `None` — Grok's `run_launch` ignores
/// the request for argv (model/effort ride the session-config wire surface
/// instead, per that function's own doc comment), and `GrokHarness` offers
/// exactly one model, so there is nothing cheaper to select.
pub(in crate::capture::record) fn run_request(
    input: &ScenarioInput,
) -> anyhow::Result<comet_proto::RunRequest> {
    let cwd = input.cwd.clone().unwrap_or_else(std::env::temp_dir);
    Ok(comet_proto::RunRequest {
        prompt: "Reply with the single word capture.".into(),
        cwd: cwd.display().to_string(),
        ..comet_proto::RunRequest::for_session(comet_proto::RuntimeMode::FullAccess)
    })
}

/// Handshake, open a session, send one `session/prompt`, wait for its reply.
///
/// Reads the request `record.rs`'s `derive_launch` already built for the
/// launch (`Session::request`) rather than rebuilding it — same
/// one-call-per-recording contract `codex::fresh_text_request`'s own doc
/// comment names, so the recorded argv and the recorded wire line can never
/// describe two different requests.
pub(in crate::capture::record) async fn run(
    session: &mut Session<AcpProvider>,
    input: &ScenarioInput,
) -> anyhow::Result<()> {
    AcpProvider::handshake(session, input).await?;
    let request = session
        .request
        .clone()
        .expect("run-grok is a Run scenario and always carries a request");

    let cwd = input
        .cwd
        .clone()
        .unwrap_or_else(std::env::temp_dir)
        .to_string_lossy()
        .into_owned();
    let new_id = session.provider.next_id();
    session
        .send(&rpc_request(
            new_id,
            "session/new",
            crate::acp::new_session_params(&cwd),
        ))
        .await?;
    let session_id = session
        .wait_for("JSON-RPC reply", |frame| {
            (frame["id"].as_u64() == Some(new_id))
                .then(|| frame["result"]["sessionId"].as_str().map(str::to_owned))
                .flatten()
        })
        .await?;

    let prompt_id = session.provider.next_id();
    session
        .send(&rpc_request(
            prompt_id,
            "session/prompt",
            crate::acp::prompt_params(&session_id, &request.prompt, &[]),
        ))
        .await?;
    session.wait_for_turn_end().await
}

/// The turn `steer-grok` opens before its queued follow-up: a different,
/// deliberately open-ended prompt from [`run_request`]'s (which asks for a
/// single word and leaves nothing to steer). This is the request the FIRST
/// `session/prompt` carries; [`STEER_MESSAGE`] below is the second, sent only
/// after this one's reply lands — see [`steer`]'s own doc comment for why
/// that is two sequential prompts rather than an in-turn steer.
pub(in crate::capture::record) fn steer_request(
    input: &ScenarioInput,
) -> anyhow::Result<comet_proto::RunRequest> {
    let cwd = input.cwd.clone().unwrap_or_else(std::env::temp_dir);
    Ok(comet_proto::RunRequest {
        prompt: "Begin a short response, then accept the follow-up instruction.".into(),
        cwd: cwd.display().to_string(),
        ..comet_proto::RunRequest::for_session(comet_proto::RuntimeMode::FullAccess)
    })
}

/// The exact text `steer-grok` sends as its queued follow-up prompt. Named,
/// like `codex::STEER_MESSAGE`, so the driving code and its own test cannot
/// drift into two separate literals.
const STEER_MESSAGE: &str = "Capture steering message.";

/// **Grok has no in-turn steering extension** (`session.rs`'s module doc:
/// "grok sends no `_meta.steering` at all"), so `GrokHarness` delivers a
/// queued steer as the next `session/prompt` on the same session once the
/// first one's reply lands — there is no `turn/steer` method to send here the
/// way Codex's capture does. This records exactly that: two sequential
/// `session/prompt` calls on one session, each awaited to completion before
/// the next is sent, matching `session.rs`'s own "between turns" delivery.
pub(in crate::capture::record) async fn steer(
    session: &mut Session<AcpProvider>,
    input: &ScenarioInput,
) -> anyhow::Result<()> {
    AcpProvider::handshake(session, input).await?;
    let request = session
        .request
        .clone()
        .expect("steer-grok is a Run scenario and always carries a request");

    let cwd = input
        .cwd
        .clone()
        .unwrap_or_else(std::env::temp_dir)
        .to_string_lossy()
        .into_owned();
    let new_id = session.provider.next_id();
    session
        .send(&rpc_request(
            new_id,
            "session/new",
            crate::acp::new_session_params(&cwd),
        ))
        .await?;
    let session_id = session
        .wait_for("JSON-RPC reply", |frame| {
            (frame["id"].as_u64() == Some(new_id))
                .then(|| frame["result"]["sessionId"].as_str().map(str::to_owned))
                .flatten()
        })
        .await?;

    let first_id = session.provider.next_id();
    session
        .send(&rpc_request(
            first_id,
            "session/prompt",
            crate::acp::prompt_params(&session_id, &request.prompt, &[]),
        ))
        .await?;
    session.wait_for_turn_end().await?;

    let steer_id = session.provider.next_id();
    session
        .send(&rpc_request(
            steer_id,
            "session/prompt",
            crate::acp::prompt_params(&session_id, STEER_MESSAGE, &[]),
        ))
        .await?;
    session.wait_for_turn_end().await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Break caught: resolving the adapter to npm's `.cmd` shim. That spawns
    /// `EINVAL` on Windows, and the failure surfaces as an unexplained dead
    /// child rather than as anything naming the shim.
    #[test]
    fn adapter_entry_points_at_the_js_file_not_a_shim() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pkg = dir.path().join(CODEX_ACP_PACKAGE).join("dist");
        std::fs::create_dir_all(&pkg).expect("create package dir");
        std::fs::write(pkg.join("index.js"), "// entry").expect("write entry");

        // Excludes every other test in this crate that touches the same env
        // var (see `ADAPTER_ROOT_ENV_LOCK`'s own doc) -- required under plain
        // `cargo test`, where they share this process; a no-op under the
        // documented `cargo nextest run` gate.
        let _guard = ADAPTER_ROOT_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // SAFETY: not a full soundness proof -- see `ADAPTER_ROOT_ENV_LOCK`'s
        // own doc on the gap between "excludes this key's other writers"
        // and `set_var`'s actual requirement. `_guard` above excludes the
        // other two known call sites; the var is read back immediately
        // below.
        unsafe { std::env::set_var("COMET_ACP_ADAPTER_ROOT", dir.path()) };
        let entry = adapter_entry(CODEX_ACP_PACKAGE).expect("entry resolves");
        unsafe { std::env::remove_var("COMET_ACP_ADAPTER_ROOT") };

        assert_eq!(entry.extension().and_then(|e| e.to_str()), Some("js"));
        assert!(entry.ends_with("dist/index.js") || entry.ends_with("dist\\index.js"));
    }

    /// Break caught: reordering Grok's four launch tokens. `--no-auto-update`
    /// is top-level, `--no-leader` belongs to `agent`, and `stdio` is under
    /// `agent` — a plausible-looking `grok agent stdio --no-leader` does not
    /// parse. Pinned as an exact sequence because that is the property, and
    /// because the flags are hidden from `--help` so nothing else records them.
    #[test]
    fn the_grok_launch_line_keeps_its_verified_token_order() {
        assert_eq!(
            GROK_ARGS,
            ["--no-auto-update", "agent", "--no-leader", "stdio"]
        );

        let dir = tempfile::tempdir().expect("tempdir");
        let exe = dir
            .path()
            .join(if cfg!(windows) { "grok.exe" } else { "grok" });
        let input = ScenarioInput {
            cwd: Some(dir.path().to_path_buf()),
            ..ScenarioInput::default()
        };
        let launch = grok_launch(&input, &exe).expect("grok launch builds");
        assert_eq!(launch.program, exe);
        assert_eq!(
            launch.args,
            GROK_ARGS
                .iter()
                .map(std::ffi::OsString::from)
                .collect::<Vec<_>>()
        );
    }

    /// Break caught: the Grok row silently spawning node, which is the ACP
    /// provider's default executable and correct for every OTHER row. Node
    /// would exit on `--no-auto-update` and the capture would fail with
    /// nothing naming the real mistake.
    #[test]
    fn the_grok_row_refuses_a_non_grok_executable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let node = dir
            .path()
            .join(if cfg!(windows) { "node.exe" } else { "node" });
        let err = grok_launch(&ScenarioInput::default(), &node).expect_err("must refuse node");
        let text = err.to_string();
        assert!(text.contains("--executable"), "no flag named: {text}");
        assert!(text.contains("node"), "no path named: {text}");
    }

    /// A missing adapter must name the install command. The alternative — a
    /// bare "not found" — sends the reader hunting for a path they have no
    /// reason to know.
    #[test]
    fn a_missing_adapter_names_how_to_install_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        // See `ADAPTER_ROOT_ENV_LOCK`'s doc: excludes every other test in
        // this crate that touches the same env var.
        let _guard = ADAPTER_ROOT_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        unsafe { std::env::set_var("COMET_ACP_ADAPTER_ROOT", dir.path()) };
        let err = adapter_entry(CODEX_ACP_PACKAGE).expect_err("must fail");
        unsafe { std::env::remove_var("COMET_ACP_ADAPTER_ROOT") };

        let text = err.to_string();
        assert!(text.contains("npm i -g"), "install hint missing: {text}");
        assert!(text.contains(CODEX_ACP_PACKAGE), "package missing: {text}");
    }
}
