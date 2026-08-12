//! What Codex says about its own currency, read from the file its updater
//! already maintains.
//!
//! **Never by spawning `codex update`.** That subcommand has no dry-run flag —
//! it checks and installs in one shot — so asking it the question is
//! indistinguishable from telling it to act (capture
//! `2026-08-12-agent-update-check.md`).
//!
//! `codex doctor --json` *does* answer this authoritatively, under
//! `checks["updates.status"]`, and is the documented fallback if this file ever
//! moves. It is not used here because it is a full health sweep: it opens a
//! WebSocket to the provider, resolves DNS and runs HTTP reachability probes,
//! which is far too much to do to every provider at every boot.

use std::cmp::Ordering;
use std::path::Path;

use comet_proto::{HarnessUpdate, UpdateState};

/// `$CODEX_HOME/version.json`, as observed on codex-cli 0.147.0:
///
/// ```json
/// {"latest_version":"0.147.0","last_checked_at":"2026-08-12T00:48:08.145707800Z","dismissed_version":null}
/// ```
///
/// `dismissed_version` is deliberately unread. It is Codex's own "stop telling
/// me about this one" flag for its own banner, and a settings card that states
/// the fact is not the surface it was dismissing. Anything that later *notifies*
/// about an update must honour it.
#[derive(serde::Deserialize)]
struct VersionCache {
    latest_version: Option<String>,
    last_checked_at: Option<String>,
}

/// [`read_update`], but only for a CLI that actually resolved to a path.
///
/// An uninstalled Codex can still leave `version.json` behind, and offering an
/// update for a binary that is not there is worse than saying nothing. A CLI
/// that resolved and then failed `--version` keeps its line: a broken install
/// beside a known-latest version is a diagnosis rather than noise.
///
/// A function rather than an `if` at the call site because the negative arm is
/// otherwise only reachable on a machine with no Codex installed, which is not
/// a condition a test can create without mutating the environment out from
/// under every other test in the process.
pub(crate) fn read_resolved_update(
    install: Option<&comet_proto::HarnessInstall>,
    home: Option<&Path>,
    installed: Option<&str>,
) -> Option<HarnessUpdate> {
    install?;
    read_update(home, installed)
}

/// What Codex's updater cache says, given the version the CLI actually
/// reported.
///
/// `None` for every unreadable shape — missing file (a CLI that has never
/// checked), unreadable, malformed JSON, or no `latest_version` key. The card
/// renders one line less; it never renders a guess.
pub(crate) fn read_update(home: Option<&Path>, installed: Option<&str>) -> Option<HarnessUpdate> {
    let path = home?.join("version.json");
    let raw = std::fs::read_to_string(&path)
        .inspect_err(|err| {
            // Absent is the ordinary case on a fresh install, so this is debug
            // rather than warn: it is not a fault, and it is not actionable.
            tracing::debug!(path = %path.display(), %err, "no codex version cache to read");
        })
        .ok()?;
    let cache: VersionCache = serde_json::from_str(&raw)
        .inspect_err(|err| {
            tracing::debug!(path = %path.display(), %err, "codex version cache did not parse");
        })
        .ok()?;
    let latest = cache.latest_version?;

    // No readable installed version means no comparison to make. The latest is
    // still carried, because it is a true fact about the provider, but the
    // state stays Unknown so nothing downstream can imply a verdict.
    let state = match installed.and_then(|installed| crate::compare_versions(&latest, installed)) {
        Some(Ordering::Greater) => UpdateState::Available,
        // Equal, or the cache trailing a CLI that has just updated itself.
        // Both are "not behind" — a newer binary than the cache knows about is
        // never a downgrade to offer.
        Some(Ordering::Less | Ordering::Equal) => UpdateState::Current,
        None => UpdateState::Unknown,
    };

    Some(HarnessUpdate {
        state,
        latest: Some(latest),
        checked_at: cache.last_checked_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home_with(contents: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("version.json"), contents).expect("write cache");
        dir
    }

    #[test]
    fn a_newer_cached_version_is_an_available_update() {
        let dir = home_with(
            r#"{"latest_version":"0.148.0","last_checked_at":"2026-08-12T00:48:08Z","dismissed_version":null}"#,
        );
        let update = read_update(Some(dir.path()), Some("0.147.0")).expect("a readable cache");
        assert_eq!(update.state, UpdateState::Available);
        assert_eq!(update.latest.as_deref(), Some("0.148.0"));
        assert_eq!(update.checked_at.as_deref(), Some("2026-08-12T00:48:08Z"));
    }

    /// The state this machine was actually in when the slice was captured.
    #[test]
    fn a_matching_cached_version_is_current() {
        let dir = home_with(r#"{"latest_version":"0.147.0","last_checked_at":null}"#);
        let update = read_update(Some(dir.path()), Some("0.147.0")).expect("a readable cache");
        assert_eq!(update.state, UpdateState::Current);
        assert_eq!(update.checked_at, None);
    }

    /// A CLI that just updated itself leaves the cache a version behind. That
    /// is "not behind", never an offer to install something older.
    #[test]
    fn a_cache_trailing_the_binary_is_current_not_a_downgrade() {
        let dir = home_with(r#"{"latest_version":"0.146.0","last_checked_at":null}"#);
        let update = read_update(Some(dir.path()), Some("0.147.0")).expect("a readable cache");
        assert_eq!(update.state, UpdateState::Current);
    }

    /// The comparison that a string sort gets backwards. `0.9.0` predates
    /// `0.147.0` by about forty releases.
    #[test]
    fn a_double_digit_minor_beats_a_single_digit_one() {
        let dir = home_with(r#"{"latest_version":"0.147.0"}"#);
        let update = read_update(Some(dir.path()), Some("0.9.0")).expect("a readable cache");
        assert_eq!(
            update.state,
            UpdateState::Available,
            "0.147.0 must read as newer than 0.9.0"
        );
    }

    #[test]
    fn malformed_json_reads_as_nothing_rather_than_panicking() {
        let dir = home_with("{not json at all");
        assert!(read_update(Some(dir.path()), Some("0.147.0")).is_none());
    }

    #[test]
    fn a_missing_cache_reads_as_nothing() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert!(read_update(Some(dir.path()), Some("0.147.0")).is_none());
    }

    #[test]
    fn a_cache_without_a_latest_version_reads_as_nothing() {
        let dir = home_with(r#"{"last_checked_at":"2026-08-12T00:48:08Z"}"#);
        assert!(read_update(Some(dir.path()), Some("0.147.0")).is_none());
    }

    /// An unreadable installed version cannot produce a verdict. The latest is
    /// still true and still carried; the state must not imply a comparison
    /// nobody made.
    #[test]
    fn an_unknown_installed_version_carries_the_latest_without_a_verdict() {
        let dir = home_with(r#"{"latest_version":"0.148.0"}"#);
        let update = read_update(Some(dir.path()), None).expect("a readable cache");
        assert_eq!(update.state, UpdateState::Unknown);
        assert_eq!(update.latest.as_deref(), Some("0.148.0"));
    }

    #[test]
    fn no_home_reads_as_nothing() {
        assert!(read_update(None, Some("0.147.0")).is_none());
    }

    fn an_install() -> comet_proto::HarnessInstall {
        comet_proto::HarnessInstall {
            path: "C:\\Users\\a\\AppData\\Local\\Programs\\OpenAI\\Codex\\bin\\codex.exe".into(),
            method: comet_proto::InstallMethod::Native,
        }
    }

    /// The guard that matters: Codex is gone, its updater cache is not. Nothing
    /// should offer to update a binary that is not installed.
    #[test]
    fn an_unresolved_cli_reads_nothing_even_with_a_cache_present() {
        let dir = home_with(r#"{"latest_version":"0.148.0"}"#);
        assert!(read_resolved_update(None, Some(dir.path()), Some("0.147.0")).is_none());
    }

    /// The other arm: resolved but broken keeps its line, because a named
    /// binary beside a known-latest is exactly when the card earns its place.
    #[test]
    fn a_resolved_cli_still_reads_its_cache() {
        let dir = home_with(r#"{"latest_version":"0.148.0"}"#);
        let update = read_resolved_update(Some(&an_install()), Some(dir.path()), Some("0.147.0"))
            .expect("a resolved CLI reads its cache");
        assert_eq!(update.state, UpdateState::Available);
    }
}
