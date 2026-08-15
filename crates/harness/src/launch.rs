//! How Comet launches a provider CLI.
//!
//! These live here rather than in `capture` because production owns them:
//! `claude::run_launch`, `codex::run_launch`, and the three discovery launches
//! all return or build a [`LaunchDescriptor`], none of them under `cfg(test)`.
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
pub(crate) struct LaunchDescriptor {
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
    pub(crate) fn command(&self) -> tokio::process::Command {
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
        command
    }
}
