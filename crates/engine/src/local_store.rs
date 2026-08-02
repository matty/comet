use std::path::{Path, PathBuf};

use comet_doc::WorkspaceDoc;
use comet_sync::DocsStore;
use loro::LoroDoc;
use serde::{Deserialize, Serialize};

use crate::{EngineError, RunJournal, WORKSPACE_DOC_ID};

const LOCAL_STORE_DIR: &str = "local-store";
const STAGING_DIR: &str = "local-store.staging";
const MARKER_FILE: &str = "migration-complete.json";
const RECOVERY_COMMAND: &str = "comet migrate --from <org>/<user>";

#[derive(Debug, Clone)]
pub struct LocalStore {
    pub root: PathBuf,
    pub migrated_from: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyProfile {
    pub org_id: String,
    pub user_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MigrationMarker {
    migrated_from: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredSessionSelector {
    user: StoredUserSelector,
    #[serde(default)]
    org_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StoredUserSelector {
    id: String,
}

pub fn prepare_local_store(
    data_dir: &Path,
    selected: Option<&LegacyProfile>,
) -> Result<LocalStore, EngineError> {
    std::fs::create_dir_all(data_dir)?;
    let local_root = data_dir.join(LOCAL_STORE_DIR);
    let marker_path = local_root.join(MARKER_FILE);
    if marker_path.exists() {
        let marker: MigrationMarker = serde_json::from_slice(&std::fs::read(&marker_path)?)
            .map_err(|err| EngineError::Other(format!("invalid local-store marker: {err}")))?;
        verify_store(&local_root)?;
        return Ok(LocalStore {
            root: local_root,
            migrated_from: marker.migrated_from,
        });
    }
    if local_root.exists() {
        return Err(EngineError::Other(format!(
            "local store exists without a completed migration marker: {}",
            local_root.display()
        )));
    }

    let migrated_from = select_profile(data_dir, selected)?;
    let device_id = read_device_id(data_dir)?;
    let staging_root = data_dir.join(STAGING_DIR);
    if staging_root.exists() {
        std::fs::remove_dir_all(&staging_root)?;
    }

    let migration_result = stage_store(
        &staging_root,
        migrated_from.as_deref(),
        device_id.as_deref(),
    );
    if let Err(err) = migration_result {
        let _ = std::fs::remove_dir_all(&staging_root);
        return Err(err);
    }

    std::fs::rename(&staging_root, &local_root)?;
    let marker = MigrationMarker {
        migrated_from: migrated_from.clone(),
    };
    write_marker_atomically(&marker_path, &marker)?;
    let session_path = data_dir.join("session.json");
    match std::fs::remove_file(session_path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err.into()),
    }

    Ok(LocalStore {
        root: local_root,
        migrated_from,
    })
}

fn stage_store(
    staging_root: &Path,
    migrated_from: Option<&Path>,
    device_id: Option<&str>,
) -> Result<(), EngineError> {
    let destination = DocsStore::open(staging_root)?;
    if let Some(source_root) = migrated_from {
        let source = DocsStore::open(source_root)?;
        source.copy_snapshots_to(&destination)?;
        if let Some(bytes) = destination.load_snapshot(WORKSPACE_DOC_ID)? {
            let device_id = device_id.ok_or_else(|| {
                EngineError::Other("device-id is required to filter the legacy workspace".into())
            })?;
            let doc = LoroDoc::new();
            doc.import(&bytes)
                .map_err(|err| EngineError::Other(format!("workspace import: {err}")))?;
            let owned = WorkspaceDoc::from_doc(doc).owned_by(device_id)?;
            destination.save_snapshot(WORKSPACE_DOC_ID, &owned.export_snapshot()?)?;
        }
        copy_tree_if_present(
            &source_root.join("journals"),
            &staging_root.join("journals"),
        )?;
    }
    drop(destination);
    RunJournal::open(staging_root.join("journals"))?;
    verify_store(staging_root)
}

fn verify_store(root: &Path) -> Result<(), EngineError> {
    let store = DocsStore::open(root)?;
    for doc_id in store.snapshot_ids()? {
        let bytes = store.load_snapshot(&doc_id)?.ok_or_else(|| {
            EngineError::Other(format!(
                "snapshot disappeared during verification: {doc_id}"
            ))
        })?;
        if doc_id == WORKSPACE_DOC_ID {
            let doc = LoroDoc::new();
            doc.import(&bytes)
                .map_err(|err| EngineError::Other(format!("workspace verification: {err}")))?;
            WorkspaceDoc::from_doc(doc).read_all()?;
        }
    }
    RunJournal::open(root.join("journals"))?;
    Ok(())
}

fn select_profile(
    data_dir: &Path,
    selected: Option<&LegacyProfile>,
) -> Result<Option<PathBuf>, EngineError> {
    let profiles = legacy_profiles(data_dir)?;
    if let Some(profile) = selected {
        if !safe_segment(&profile.org_id) || !safe_segment(&profile.user_id) {
            return Err(EngineError::Other(format!(
                "legacy profile must be <org>/<user>; run {RECOVERY_COMMAND}"
            )));
        }
        let path = data_dir
            .join("orgs")
            .join(&profile.org_id)
            .join(&profile.user_id);
        if profiles.contains(&path) {
            return Ok(Some(path));
        }
        return Err(EngineError::Other(format!(
            "legacy profile does not exist; run {RECOVERY_COMMAND}"
        )));
    }

    if let Some(session_profile) = session_profile(data_dir)? {
        let path = data_dir
            .join("orgs")
            .join(session_profile.org_id)
            .join(session_profile.user_id);
        if profiles.contains(&path) {
            return Ok(Some(path));
        }
        return Err(EngineError::Other(format!(
            "session legacy profile does not exist; run {RECOVERY_COMMAND}"
        )));
    }

    match profiles.as_slice() {
        [] => Ok(None),
        [only] => Ok(Some(only.clone())),
        _ => Err(EngineError::Other(format!(
            "multiple legacy profiles found; run {RECOVERY_COMMAND}"
        ))),
    }
}

fn legacy_profiles(data_dir: &Path) -> Result<Vec<PathBuf>, EngineError> {
    let orgs_root = data_dir.join("orgs");
    if !orgs_root.exists() {
        return Ok(Vec::new());
    }
    let mut profiles = Vec::new();
    for org in std::fs::read_dir(orgs_root)? {
        let org = org?;
        if !org.file_type()?.is_dir() {
            continue;
        }
        for user in std::fs::read_dir(org.path())? {
            let user = user?;
            if user.file_type()?.is_dir() {
                profiles.push(user.path());
            }
        }
    }
    profiles.sort();
    Ok(profiles)
}

fn session_profile(data_dir: &Path) -> Result<Option<LegacyProfile>, EngineError> {
    let bytes = match std::fs::read(data_dir.join("session.json")) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let session: StoredSessionSelector = serde_json::from_slice(&bytes)
        .map_err(|err| EngineError::Other(format!("invalid legacy session.json: {err}")))?;
    Ok(session.org_id.map(|org_id| LegacyProfile {
        org_id,
        user_id: session.user.id,
    }))
}

fn read_device_id(data_dir: &Path) -> Result<Option<String>, EngineError> {
    match std::fs::read_to_string(data_dir.join("device-id")) {
        Ok(value) if !value.trim().is_empty() => Ok(Some(value.trim().to_string())),
        Ok(_) => Err(EngineError::Other("device-id is empty".into())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

fn copy_tree_if_present(source: &Path, destination: &Path) -> Result<(), EngineError> {
    if !source.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(destination)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let destination_path = destination.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_tree_if_present(&entry.path(), &destination_path)?;
        } else if kind.is_file() {
            std::fs::copy(entry.path(), destination_path)?;
        } else {
            return Err(EngineError::Other(format!(
                "unsupported journal entry: {}",
                entry.path().display()
            )));
        }
    }
    Ok(())
}

fn write_marker_atomically(path: &Path, marker: &MigrationMarker) -> Result<(), EngineError> {
    let temporary = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(marker)
        .map_err(|err| EngineError::Other(format!("serialize migration marker: {err}")))?;
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)?;
    use std::io::Write;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(temporary, path)?;
    Ok(())
}

fn safe_segment(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}
