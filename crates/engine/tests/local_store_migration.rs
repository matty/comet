use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use comet_doc::{SessionDoc, WorkspaceDoc};
use comet_engine::{LegacyProfile, WORKSPACE_DOC_ID, prepare_local_store};
use comet_proto::Space;
use comet_sync::DocsStore;
use loro::LoroDoc;
use loro::LoroMap;
use tempfile::TempDir;

struct LegacyFixture {
    dir: TempDir,
}

#[derive(Debug, PartialEq, Eq)]
struct SourceFileState {
    bytes: Vec<u8>,
    modified: SystemTime,
    readonly: bool,
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

    fn write_snapshot(&self, doc_id: &str, bytes: &[u8]) {
        DocsStore::open(self.legacy_store())
            .unwrap()
            .save_snapshot(doc_id, bytes)
            .unwrap();
    }

    fn write_journal(&self, name: &str, bytes: &[u8]) {
        let journals = self.legacy_store().join("journals");
        std::fs::create_dir_all(&journals).unwrap();
        std::fs::write(journals.join(name), bytes).unwrap();
    }

    fn write_session_with_malformed_row(&self, chat_id: &str, container: &str) {
        let doc = SessionDoc::init(chat_id).unwrap();
        doc.doc()
            .get_list(container)
            .push_container(LoroMap::new())
            .unwrap();
        doc.doc().commit();
        self.write_snapshot(chat_id, &doc.export_snapshot().unwrap());
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

    fn source_manifest(&self, org_id: &str, user_id: &str) -> BTreeMap<PathBuf, SourceFileState> {
        fn collect(root: &Path, path: &Path, out: &mut BTreeMap<PathBuf, SourceFileState>) {
            for entry in std::fs::read_dir(path).unwrap() {
                let entry = entry.unwrap();
                let kind = entry.file_type().unwrap();
                if kind.is_dir() {
                    collect(root, &entry.path(), out);
                } else if kind.is_file() {
                    let metadata = entry.metadata().unwrap();
                    out.insert(
                        entry.path().strip_prefix(root).unwrap().to_path_buf(),
                        SourceFileState {
                            bytes: std::fs::read(entry.path()).unwrap(),
                            modified: metadata.modified().unwrap(),
                            readonly: metadata.permissions().readonly(),
                        },
                    );
                }
            }
        }

        let root = self.profile(org_id, user_id);
        let mut out = BTreeMap::new();
        collect(&root, &root, &mut out);
        out
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

#[test]
fn migration_does_not_change_legacy_source_files_or_metadata() {
    let fixture = LegacyFixture::new();
    let database = fixture.legacy_store().join("docs.sqlite3");
    let connection = rusqlite::Connection::open(&database).unwrap();
    connection
        .pragma_update(None, "journal_mode", "DELETE")
        .unwrap();
    drop(connection);
    let before = fixture.source_manifest("org-a", "user-a");

    prepare_local_store(fixture.root(), None).unwrap();

    assert_eq!(fixture.source_manifest("org-a", "user-a"), before);
}

#[test]
fn corrupt_chat_snapshot_blocks_marker_and_session_removal() {
    let fixture = LegacyFixture::new();
    fixture.write_session("org-a", "user-a");
    fixture.write_snapshot("chat-corrupt", b"not a loro snapshot");

    let err = prepare_local_store(fixture.root(), None)
        .unwrap_err()
        .to_string();

    assert!(err.contains("chat-corrupt"));
    assert!(fixture.root().join("session.json").exists());
    assert!(
        !fixture
            .root()
            .join("local-store/migration-complete.json")
            .exists()
    );
}

#[test]
fn workspace_snapshot_under_chat_id_is_rejected_as_wrong_document_type() {
    let fixture = LegacyFixture::new();
    fixture.write_session("org-a", "user-a");
    let bytes = WorkspaceDoc::new().export_snapshot().unwrap();
    fixture.write_snapshot("chat-wrong-type", &bytes);

    let err = prepare_local_store(fixture.root(), None)
        .unwrap_err()
        .to_string();

    assert!(err.contains("chat-wrong-type"));
    assert!(err.contains("not a session document"));
    assert!(fixture.root().join("session.json").exists());
    assert!(
        !fixture
            .root()
            .join("local-store/migration-complete.json")
            .exists()
    );
}

#[test]
fn corrupt_journal_blocks_marker_and_session_removal() {
    let fixture = LegacyFixture::new();
    fixture.write_session("org-a", "user-a");
    fixture.write_journal("chat-corrupt.jsonl", b"not json\n");

    let err = prepare_local_store(fixture.root(), None)
        .unwrap_err()
        .to_string();

    assert!(err.contains("chat-corrupt.jsonl"));
    assert!(fixture.root().join("session.json").exists());
    assert!(
        !fixture
            .root()
            .join("local-store/migration-complete.json")
            .exists()
    );
}

#[test]
fn malformed_session_message_row_blocks_marker_and_session_removal() {
    let fixture = LegacyFixture::new();
    fixture.write_session("org-a", "user-a");
    fixture.write_session_with_malformed_row("chat-bad-message", "messages");

    let err = prepare_local_store(fixture.root(), None)
        .unwrap_err()
        .to_string();

    assert!(err.contains("invalid message row 0"));
    assert!(fixture.root().join("session.json").exists());
    assert!(
        !fixture
            .root()
            .join("local-store/migration-complete.json")
            .exists()
    );
}

#[test]
fn malformed_session_command_row_blocks_marker_and_session_removal() {
    let fixture = LegacyFixture::new();
    fixture.write_session("org-a", "user-a");
    fixture.write_session_with_malformed_row("chat-bad-command", "commands");

    let err = prepare_local_store(fixture.root(), None)
        .unwrap_err()
        .to_string();

    assert!(err.contains("invalid command row 0"));
    assert!(fixture.root().join("session.json").exists());
    assert!(
        !fixture
            .root()
            .join("local-store/migration-complete.json")
            .exists()
    );
}
