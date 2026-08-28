use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::bail;
use serde_json::json;

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
            json!({"cwd": cwd, "mcpServers": []}),
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

/// ACP has no run rows yet, so this is never reached: every registered ACP
/// scenario is `ScenarioLaunch::Discovery`, and `derive_launch` only calls a
/// run launch for `ScenarioLaunch::Run`.
///
/// The invariant is pinned by `every_acp_row_is_discovery` below rather than
/// left to hope — an added run row fails that test instead of panicking during
/// somebody's capture.
pub(in crate::capture::record) fn run_launch_unreachable(
    _executable: &Path,
    _request: &comet_proto::RunRequest,
) -> LaunchDescriptor {
    unreachable!(
        "ACP registers only discovery rows today; a run row must supply its own launch builder"
    )
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

        // SAFETY: single-threaded test; the var is read back immediately below.
        unsafe { std::env::set_var("COMET_ACP_ADAPTER_ROOT", dir.path()) };
        let entry = adapter_entry(CODEX_ACP_PACKAGE).expect("entry resolves");
        unsafe { std::env::remove_var("COMET_ACP_ADAPTER_ROOT") };

        assert_eq!(entry.extension().and_then(|e| e.to_str()), Some("js"));
        assert!(entry.ends_with("dist/index.js") || entry.ends_with("dist\\index.js"));
    }

    /// A missing adapter must name the install command. The alternative — a
    /// bare "not found" — sends the reader hunting for a path they have no
    /// reason to know.
    /// Guards `run_launch_unreachable`. If an ACP run row is ever registered,
    /// this fails here rather than panicking mid-capture on someone's machine.
    #[test]
    fn every_acp_row_is_discovery() {
        use crate::capture::Provider;
        use crate::capture::record::scenarios::{SCENARIOS, ScenarioLaunch};
        for spec in SCENARIOS.iter().filter(|s| s.provider == Provider::Acp) {
            assert!(
                matches!(spec.launch, ScenarioLaunch::Discovery(_)),
                "{} is an ACP run row; give it a real run launch and drop run_launch_unreachable",
                spec.name
            );
        }
    }

    #[test]
    fn a_missing_adapter_names_how_to_install_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        unsafe { std::env::set_var("COMET_ACP_ADAPTER_ROOT", dir.path()) };
        let err = adapter_entry(CODEX_ACP_PACKAGE).expect_err("must fail");
        unsafe { std::env::remove_var("COMET_ACP_ADAPTER_ROOT") };

        let text = err.to_string();
        assert!(text.contains("npm i -g"), "install hint missing: {text}");
        assert!(text.contains(CODEX_ACP_PACKAGE), "package missing: {text}");
    }
}
