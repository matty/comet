use std::path::{Path, PathBuf};
use std::time::Duration;

use comet_proto::{ReasoningLevel, RunRequest, RuntimeMode};
use serde_json::json;

use super::safety::repository_root;
use super::{CaptureConfig, Channel, RawCapture};

pub(super) fn contract_request() -> RunRequest {
    let mut request = RunRequest {
        prompt: "capture contract".into(),
        model: Some("claude-sonnet-5".into()),
        reasoning: Some(ReasoningLevel::XHigh),
        cwd: std::env::temp_dir()
            .join("comet capture cwd")
            .display()
            .to_string(),
        resume: Some("session-to-resume".into()),
        ..RunRequest::for_session(RuntimeMode::FullAccess)
    };
    request
        .model_options
        .insert("contextWindow".into(), json!("1m"));
    request.model_options.insert("fastMode".into(), json!(true));
    request.model_options.insert("thinking".into(), json!("on"));
    request
}

pub(super) fn absolute_program(name: &str) -> PathBuf {
    std::env::current_dir().unwrap().join(name)
}

pub(super) fn fixture_path(name: &str) -> PathBuf {
    let variable = format!("CARGO_BIN_EXE_{}", name.replace('-', "_"));
    if let Some(path) = std::env::var_os(&variable) {
        return path.into();
    }
    let suffix = std::env::consts::EXE_SUFFIX;
    std::env::current_exe()
        .expect("current test executable")
        .parent()
        .and_then(Path::parent)
        .expect("target debug directory")
        .join(format!("{name}{suffix}"))
}

pub(super) fn isolated_tempdir(prefix: &str) -> tempfile::TempDir {
    let current = std::env::current_dir().expect("current test directory");
    let parent = current
        .ancestors()
        .find(|path| repository_root(path).is_none())
        .expect("an ancestor outside the repository");
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(parent)
        .expect("isolated test directory")
}

pub(super) fn isolated_approval_target(prefix: &str) -> Option<tempfile::TempDir> {
    let current = std::fs::canonicalize(std::env::current_dir().ok()?).ok()?;
    let temp = std::fs::canonicalize(std::env::temp_dir()).unwrap_or_else(|_| std::env::temp_dir());
    let mut parents = current
        .ancestors()
        .filter(|path| repository_root(path).is_none())
        .map(Path::to_path_buf)
        .collect::<Vec<_>>();
    if let Some(home) = crate::home_dir() {
        parents.push(home);
    }

    for parent in parents {
        let Ok(parent) = std::fs::canonicalize(parent) else {
            continue;
        };
        if parent.starts_with(&temp) {
            continue;
        }
        let Ok(target) = tempfile::Builder::new().prefix(prefix).tempdir_in(parent) else {
            continue;
        };
        let Ok(canonical) = std::fs::canonicalize(target.path()) else {
            continue;
        };
        if canonical.starts_with(&temp)
            || canonical.starts_with(&current)
            || current.starts_with(&canonical)
            || repository_root(&canonical).is_some()
        {
            continue;
        }
        return Some(target);
    }

    eprintln!("skipping Codex on-request test: no isolated approval target is writable");
    None
}

/// A minimal [`CaptureConfig`] for tests that drive `Session::start` or a
/// scenario body directly. `name` need only match a real `SCENARIOS` row
/// name for the tests that call `record()` itself (which looks the row up
/// by name) — every other test call site uses it purely as a label for the
/// raw capture directory and `capture.scenario`.
pub(super) fn config(
    name: &'static str,
    executable: PathBuf,
    provider: &'static str,
    raw_root: &Path,
) -> CaptureConfig {
    CaptureConfig {
        provider,
        scenario_name: name,
        purpose: "local recorder test",
        executable: Some(executable),
        codex_home: None,
        claude_config_dir: None,
        cwd: None,
        resume_id: None,
        attachment: None,
        approval_target: None,
        raw_root: raw_root.into(),
        timeout: Duration::from_secs(5),
    }
}

pub(super) fn channel_payloads(capture: &RawCapture, channel: Channel) -> Vec<&str> {
    capture
        .events
        .iter()
        .filter(|event| event.channel == channel)
        .map(|event| event.payload.as_str())
        .collect()
}

pub(super) fn find_named_file(root: &Path, name: &str) -> bool {
    std::fs::read_dir(root).is_ok_and(|entries| {
        entries.filter_map(Result::ok).any(|entry| {
            entry.file_name() == name
                || (entry.path().is_dir() && find_named_file(&entry.path(), name))
        })
    })
}
