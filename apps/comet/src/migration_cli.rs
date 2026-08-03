use clap::Args;
use std::path::Path;

use anyhow::{Context, bail};
use comet_engine::{InstanceLock, LegacyProfile, prepare_local_store};

#[derive(Debug, Args)]
pub struct MigrateArgs {
    /// Exact legacy profile under orgs/, formatted as ORG/USER.
    #[arg(long, value_name = "ORG/USER")]
    pub from: String,
}

pub fn run(data_dir: &Path, from: &str) -> anyhow::Result<()> {
    std::fs::create_dir_all(data_dir)?;
    let profile = parse_legacy_profile(from)?;
    validate_legacy_directory(data_dir, &profile)?;
    let _lock = InstanceLock::acquire(data_dir).map_err(|error| {
        anyhow::anyhow!("cannot migrate while the Comet engine is running: {error}")
    })?;
    let store = prepare_local_store(data_dir, Some(&profile))?;
    println!("Migrated {from} into {}.", store.root.display());
    Ok(())
}

fn parse_legacy_profile(value: &str) -> anyhow::Result<LegacyProfile> {
    let (org_id, user_id) = value
        .split_once('/')
        .ok_or_else(|| anyhow::anyhow!("--from must be formatted as ORG/USER"))?;
    if user_id.contains('/') || !safe_segment(org_id) || !safe_segment(user_id) {
        bail!("--from must contain exactly two safe path segments: ORG/USER");
    }
    Ok(LegacyProfile {
        org_id: org_id.into(),
        user_id: user_id.into(),
    })
}

fn safe_segment(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn validate_legacy_directory(data_dir: &Path, profile: &LegacyProfile) -> anyhow::Result<()> {
    let orgs = data_dir.join("orgs");
    let selected = orgs.join(&profile.org_id).join(&profile.user_id);
    if !selected.is_dir() {
        bail!("legacy profile does not exist: {}", selected.display());
    }
    if std::fs::symlink_metadata(&selected)?
        .file_type()
        .is_symlink()
    {
        bail!("legacy profile must not be a symbolic link");
    }
    let orgs = orgs
        .canonicalize()
        .context("resolving the legacy orgs directory")?;
    let selected = selected
        .canonicalize()
        .context("resolving the selected legacy profile")?;
    if !selected.starts_with(&orgs) {
        bail!("legacy profile resolves outside the data directory");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_selector_accepts_two_safe_segments_only() {
        let selected = parse_legacy_profile("org-a/user_a").unwrap();
        assert_eq!(selected.org_id, "org-a");
        assert_eq!(selected.user_id, "user_a");
        for unsafe_value in ["org", "org/user/extra", "../user", "org/..", "org/C:\\tmp"] {
            assert!(
                parse_legacy_profile(unsafe_value).is_err(),
                "accepted {unsafe_value}"
            );
        }
    }

    #[test]
    fn migration_requires_the_exact_profile_directory() {
        let dir = tempfile::tempdir().unwrap();
        let profile = comet_engine::LegacyProfile {
            org_id: "org-a".into(),
            user_id: "user-a".into(),
        };
        assert!(validate_legacy_directory(dir.path(), &profile).is_err());
        std::fs::create_dir_all(dir.path().join("orgs/org-a/user-a")).unwrap();
        assert!(validate_legacy_directory(dir.path(), &profile).is_ok());
    }

    #[test]
    fn migration_refuses_an_active_engine_before_writing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("orgs/org-a/user-a")).unwrap();
        let _lock = comet_engine::InstanceLock::acquire(dir.path()).unwrap();
        let err = run(dir.path(), "org-a/user-a").unwrap_err().to_string();
        assert!(err.contains("engine is running"), "{err}");
        assert!(!dir.path().join("local-store").exists());
    }
}
