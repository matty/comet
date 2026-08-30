//! How Comet launches a provider CLI.
//!
//! These live here rather than in `capture` because production owns them:
//! `claude::run_launch`, `codex::run_launch`, and the discovery launches
//! all return or build a `LaunchDescriptor`, none of them under `cfg(test)`.
//! They sat in `capture/types.rs` until 2026-08-15, which is why a design doc
//! once recorded that nothing on the runtime path touched `capture`.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Stdio;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum StdioMode {
    Inherit,
    Null,
    Piped,
}

impl StdioMode {
    fn materialize(self) -> Stdio {
        match self {
            Self::Inherit => Stdio::inherit(),
            Self::Null => Stdio::null(),
            Self::Piped => Stdio::piped(),
        }
    }
}

/// Every process-launch choice shared by production and capture.
#[derive(Clone, Debug)]
pub struct LaunchDescriptor {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: Option<PathBuf>,
    pub configured_env: BTreeMap<OsString, OsString>,
    pub stdin: StdioMode,
    pub stdout: StdioMode,
    pub stderr: StdioMode,
    pub kill_on_drop: bool,
    #[cfg(windows)]
    pub creation_flags: u32,
}

impl LaunchDescriptor {
    pub fn command(&self) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(&self.program);
        command
            .args(&self.args)
            .envs(&self.configured_env)
            .stdin(self.stdin.materialize())
            .stdout(self.stdout.materialize())
            .stderr(self.stderr.materialize())
            .kill_on_drop(self.kill_on_drop);
        if let Some(cwd) = &self.cwd {
            command.current_dir(cwd);
        }
        #[cfg(windows)]
        command.creation_flags(self.creation_flags);
        own_process_group(&mut command);
        command
    }
}

/// D46: put `command`'s child in its own process group so a later escalation
/// signal can reach the whole tree it spawns (`send_signal` targets this
/// group, not just the one pid) via `killpg`. `0` means "use the child's own
/// pid as the group id" — set before the child's own code runs, so a
/// grandchild it forks before ever touching stdin already inherits this
/// group; nothing here changes what the child inherits otherwise; it only
/// changes which pids answer a signal aimed at it.
///
/// **Reused, not inlined at every spawn site (D133).** [`LaunchDescriptor::command`]
/// calls this for every adapter that builds a `Command` from a descriptor.
/// `acp::session::connect` calls it AGAIN, directly, on whatever `Command`
/// its caller handed in — the ACP session loop is the one spawn site every
/// ACP agent (and every ACP test) shares, so establishing the group THERE,
/// not only inside `LaunchDescriptor`, means it holds even for a caller that
/// never built one. That gap was not hypothetical: CI's Ubuntu run against
/// this PR's own `wedge-with-child` test caught it — `acp_turn.rs` spawns
/// `fake-acp` from a bare `tokio::process::Command`, so production's own
/// `grok`/`hermes` `run()` methods happening to route through
/// `LaunchDescriptor` was never a guarantee `connect()` itself enforced.
/// Idempotent: calling it twice on the same `Command` before `spawn` (as
/// every production ACP call site now does) is harmless — the second call
/// just sets the same group again.
#[cfg(unix)]
pub(crate) fn own_process_group(command: &mut tokio::process::Command) {
    command.process_group(0);
}

#[cfg(not(unix))]
pub(crate) fn own_process_group(_command: &mut tokio::process::Command) {}
