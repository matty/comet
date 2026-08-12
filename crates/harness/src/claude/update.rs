//! What Claude says about its own currency — which is less than Codex says,
//! and the copy has to respect the difference.
//!
//! Claude publishes **no latest version anywhere a caller can read**. It has no
//! `--json` doctor, its `doctor` output is human prose, and the changelog it
//! caches lags the installed binary (it read `2.1.227` against an installed
//! `2.1.228` during the capture, so using it would report updates backwards).
//! What it does keep is a record of what its own updater last *did*.
//!
//! So the strongest honest answer here is [`UpdateState::SelfUpdating`] — the
//! updater ran and worked. Never `Current`: nothing in this file says the
//! installed version is the newest one that exists.
//!
//! `claude update` is not an alternative. Like Codex's, it checks and installs
//! in one shot with no dry-run flag (capture
//! `2026-08-12-agent-update-check.md`).

use std::path::{Path, PathBuf};

use comet_proto::{HarnessUpdate, UpdateState};

/// Where this device's Claude state lives: `$CLAUDE_CONFIG_DIR`, else
/// `~/.claude`.
///
/// The override is honoured by the real CLI, not assumed: pointing it at an
/// empty directory on 2026-08-12 flipped `claude doctor` to "Last update
/// attempt: none recorded" and "Config install method: not set".
///
/// Mirrors [`crate::codex::discovery::codex_home`]. Change one, read the other.
pub(crate) fn claude_home() -> Option<PathBuf> {
    crate::env_dir("CLAUDE_CONFIG_DIR")
        .or_else(|| crate::home_dir().map(|home| home.join(".claude")))
}

/// `$CLAUDE_CONFIG_DIR/.last-update-result.json`, as observed on Claude Code
/// 2.1.228:
///
/// ```json
/// {"timestamp":"2026-08-11T19:59:18.645Z","path":"native","outcome":"success",
///  "status":"success","version_from":"2.1.227","version_to":"2.1.228","error_code":null}
/// ```
///
/// `outcome` and `status` were identical in every observed record. Only
/// `outcome` is read — two fields agreeing is not a reason to require both, and
/// demanding a field that turns out to be optional is how a decode starts
/// silently returning nothing.
#[derive(serde::Deserialize)]
struct LastUpdateResult {
    outcome: Option<String>,
    timestamp: Option<String>,
    // `version_to` is present in the record and deliberately not decoded — see
    // where `latest` is built below for why publishing it would be wrong.
}

/// What Claude's own updater last did.
///
/// `None` for every unreadable shape, and for a CLI that has never updated —
/// an absent record says nothing about whether updating works, so the card
/// renders one line less rather than reassuring anyone.
pub(crate) fn read_update(home: Option<&Path>) -> Option<HarnessUpdate> {
    let path = home?.join(".last-update-result.json");
    let raw = std::fs::read_to_string(&path)
        .inspect_err(|err| {
            // A fresh install has never updated. Ordinary, not actionable.
            tracing::debug!(path = %path.display(), %err, "no claude update record to read");
        })
        .ok()?;
    let record: LastUpdateResult = serde_json::from_str(&raw)
        .inspect_err(|err| {
            tracing::debug!(path = %path.display(), %err, "claude update record did not parse");
        })
        .ok()?;

    let outcome = record.outcome?;
    let state = match outcome.as_str() {
        "success" => UpdateState::SelfUpdating,
        // Anything that is not a success is a failure worth surfacing,
        // including an outcome spelling this build has never seen. The
        // conservative direction is inverted here compared with every other
        // decode in this slice: a silent updater failure is the one thing on
        // this card a user can act on, so an unrecognized outcome must not be
        // rounded down to "fine".
        other => {
            tracing::debug!(outcome = %other, "claude's last update attempt did not succeed");
            UpdateState::UpdateFailed
        }
    };

    Some(HarnessUpdate {
        state,
        // Deliberately not `version_to`. That is the version this CLI updated
        // *to*, which is where it already is — publishing it as `latest` would
        // read as a newer version being available.
        latest: None,
        checked_at: record.timestamp,
    })
}

/// [`read_update`], but only for a CLI that resolved to a path. Same reasoning
/// as Codex's — see [`crate::codex::update::read_resolved_update`].
pub(crate) fn read_resolved_update(
    install: Option<&comet_proto::HarnessInstall>,
    home: Option<&Path>,
) -> Option<HarnessUpdate> {
    install?;
    read_update(home)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home_with(contents: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join(".last-update-result.json"), contents)
            .expect("write record");
        dir
    }

    /// The record this machine actually had, byte for byte.
    #[test]
    fn a_successful_attempt_reads_as_self_updating() {
        let dir = home_with(
            r#"{"timestamp":"2026-08-11T19:59:18.645Z","path":"native","outcome":"success","status":"success","version_from":"2.1.227","version_to":"2.1.228","error_code":null}"#,
        );
        let update = read_update(Some(dir.path())).expect("a readable record");
        assert_eq!(update.state, UpdateState::SelfUpdating);
        assert_eq!(
            update.checked_at.as_deref(),
            Some("2026-08-11T19:59:18.645Z")
        );
    }

    /// `version_to` is where the CLI already is. Publishing it as `latest`
    /// would render as an update being available when the update has happened.
    #[test]
    fn the_version_it_updated_to_is_never_reported_as_a_latest() {
        let dir = home_with(r#"{"outcome":"success","version_to":"2.1.228"}"#);
        let update = read_update(Some(dir.path())).expect("a readable record");
        assert_eq!(
            update.latest, None,
            "the version already installed must not read as one to install"
        );
    }

    /// The state that earns this reader its place: the auto-updater has been
    /// failing and nothing else on the machine says so.
    #[test]
    fn a_failed_attempt_reads_as_failed() {
        let dir = home_with(
            r#"{"timestamp":"2026-08-11T19:59:18.645Z","outcome":"failure","error_code":"EACCES"}"#,
        );
        let update = read_update(Some(dir.path())).expect("a readable record");
        assert_eq!(update.state, UpdateState::UpdateFailed);
    }

    /// An outcome this build has never seen is treated as a failure, not as a
    /// success. Rounding an unknown outcome down to "fine" would hide exactly
    /// the state worth showing.
    #[test]
    fn an_unrecognized_outcome_is_not_mistaken_for_success() {
        let dir = home_with(r#"{"outcome":"partiallyRolledBack"}"#);
        let update = read_update(Some(dir.path())).expect("a readable record");
        assert_eq!(update.state, UpdateState::UpdateFailed);
    }

    #[test]
    fn a_record_without_an_outcome_reads_as_nothing() {
        let dir = home_with(r#"{"timestamp":"2026-08-11T19:59:18.645Z"}"#);
        assert!(read_update(Some(dir.path())).is_none());
    }

    /// The live state produced by pointing `CLAUDE_CONFIG_DIR` at an empty
    /// directory during the capture — the CLI reported "none recorded".
    #[test]
    fn a_missing_record_reads_as_nothing() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert!(read_update(Some(dir.path())).is_none());
    }

    #[test]
    fn malformed_json_reads_as_nothing_rather_than_panicking() {
        let dir = home_with("{ not json");
        assert!(read_update(Some(dir.path())).is_none());
    }

    #[test]
    fn no_home_reads_as_nothing() {
        assert!(read_update(None).is_none());
    }

    #[test]
    fn an_unresolved_cli_reads_nothing_even_with_a_record_present() {
        let dir = home_with(r#"{"outcome":"success"}"#);
        assert!(read_resolved_update(None, Some(dir.path())).is_none());
    }
}
