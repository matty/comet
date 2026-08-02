use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use comet_doc::WorkspaceDoc;
use comet_engine::{LegacyProfile, WORKSPACE_DOC_ID, prepare_local_store};
use comet_proto::Space;
use comet_sync::DocsStore;
use loro::LoroDoc;
use tempfile::TempDir;

struct LegacyFixture {
    dir: TempDir,
}

impl LegacyFixture {
    fn new() -> Self {
        Self::single_profile("org-a", "user-a")
    }

    fn single_profile(org_id: &str, user_id: &str) -> Self {
        Self::with_profiles(&[(org_id, user_id)])
    }

    fn with_profiles(profiles: &[(&str, &str)]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("device-id"), "device-local\n").unwrap();
        for (org_id, user_id) in profiles {
            DocsStore::open(dir.path().join("orgs").join(org_id).join(user_id)).unwrap();
        }
        Self { dir }
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn profile(&self, org_id: &str, user_id: &str) -> PathBuf {
        self.root().join("orgs").join(org_id).join(user_id)
    }

    fn legacy_store(&self) -> PathBuf {
        self.profile("org-a", "user-a")
    }

    fn write_session(&self, org_id: &str, user_id: &str) {
        let session = serde_json::json!({
            "refreshToken": "refresh-token",
            "user": {
                "id": user_id,
                "email": format!("{user_id}@example.com")
            },
            "orgId": org_id
        });
        std::fs::write(
            self.root().join("session.json"),
            serde_json::to_vec(&session).unwrap(),
        )
        .unwrap();
    }

    fn write_workspace(&self, spaces: &[Space]) {
        let workspace = WorkspaceDoc::new();
        for space in spaces {
            workspace.upsert_space(space).unwrap();
        }
        let bytes = workspace.export_snapshot().unwrap();
        DocsStore::open(self.legacy_store())
            .unwrap()
            .save_snapshot(WORKSPACE_DOC_ID, &bytes)
            .unwrap();
    }

    fn read_migrated_workspace(
        &self,
        local: &comet_engine::LocalStore,
    ) -> comet_doc::WorkspaceState {
        let bytes = DocsStore::open(&local.root)
            .unwrap()
            .load_snapshot(WORKSPACE_DOC_ID)
            .unwrap()
            .expect("migrated workspace snapshot");
        let doc = LoroDoc::new();
        doc.import(&bytes).unwrap();
        WorkspaceDoc::from_doc(doc).read_all().unwrap()
    }
}

fn space(id: &str, device_id: &str) -> Space {
    Space {
        id: id.into(),
        device_id: device_id.into(),
        path: format!("/tmp/{id}"),
        name: None,
        git_detected: false,
        git_checked_at: None,
        checkout_id: None,
        created_at: DateTime::<Utc>::UNIX_EPOCH,
    }
}

#[test]
fn session_selects_legacy_profile_and_filters_other_devices() {
    let fixture = LegacyFixture::new();
    fixture.write_session("org-a", "user-a");
    fixture.write_workspace(&[
        space("space-local", "device-local"),
        space("space-foreign", "device-foreign"),
    ]);

    let local = prepare_local_store(fixture.root(), None).unwrap();
    let state = fixture.read_migrated_workspace(&local);
    assert_eq!(
        state
            .spaces
            .iter()
            .map(|s| s.id.as_str())
            .collect::<Vec<_>>(),
        ["space-local"]
    );
    assert!(fixture.legacy_store().exists());
    assert!(!fixture.root().join("session.json").exists());
}

#[test]
fn ambiguous_profiles_fail_without_creating_marker() {
    let fixture = LegacyFixture::with_profiles(&[("org-a", "user-a"), ("org-b", "user-b")]);
    let err = prepare_local_store(fixture.root(), None)
        .unwrap_err()
        .to_string();
    assert!(err.contains("comet migrate --from <org>/<user>"));
    assert!(
        !fixture
            .root()
            .join("local-store/migration-complete.json")
            .exists()
    );
}

#[test]
fn explicit_profile_resolves_ambiguity_without_merging() {
    let fixture = LegacyFixture::with_profiles(&[("org-a", "user-a"), ("org-b", "user-b")]);
    let selected = LegacyProfile {
        org_id: "org-b".into(),
        user_id: "user-b".into(),
    };
    let local = prepare_local_store(fixture.root(), Some(&selected)).unwrap();
    assert_eq!(
        local.migrated_from.as_deref(),
        Some(fixture.profile("org-b", "user-b").as_path())
    );
    assert!(fixture.profile("org-a", "user-a").exists());
}

#[test]
fn completed_migration_is_idempotent() {
    let fixture = LegacyFixture::single_profile("org-a", "user-a");
    let first = prepare_local_store(fixture.root(), None).unwrap();
    let second = prepare_local_store(fixture.root(), None).unwrap();
    assert_eq!(first.root, second.root);
}

#[test]
fn workspace_migration_without_device_id_does_not_publish_foreign_rows() {
    let fixture = LegacyFixture::new();
    fixture.write_workspace(&[space("space-foreign", "device-foreign")]);
    std::fs::remove_file(fixture.root().join("device-id")).unwrap();

    let err = prepare_local_store(fixture.root(), None)
        .unwrap_err()
        .to_string();

    assert!(err.contains("device-id"));
    assert!(fixture.legacy_store().exists());
    assert!(
        !fixture
            .root()
            .join("local-store/migration-complete.json")
            .exists()
    );
}
