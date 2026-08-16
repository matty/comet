use comet_proto::{
    TOOL_DIFF_PATH_MAX_BYTES, TOOL_DIFF_PAYLOAD_MAX_BYTES, TOOL_DIFF_SOURCE_MAX_BYTES, ToolDiff,
};
use rusqlite::{OptionalExtension, params};

use super::{DocsStore, StoreError, now_ms};

const SIDECAR_LIMITS: SidecarLimits = SidecarLimits {
    path_bytes: TOOL_DIFF_PATH_MAX_BYTES,
    source_bytes: TOOL_DIFF_SOURCE_MAX_BYTES,
    payload_bytes: TOOL_DIFF_PAYLOAD_MAX_BYTES,
    total_bytes: 512 * 1024 * 1024,
    records: 4_096,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PutToolDiffOutcome {
    Stored { diff_ref: String, byte_len: u64 },
    Rejected(ToolDiffLimit),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolDiffLimit {
    Path,
    OldSource,
    NewSource,
    Payload,
}

#[derive(Clone, Copy)]
struct SidecarLimits {
    path_bytes: usize,
    source_bytes: usize,
    payload_bytes: usize,
    total_bytes: u64,
    records: u64,
}

impl DocsStore {
    pub fn put_tool_diff(
        &self,
        chat_id: &str,
        part_id: &str,
        diff: &ToolDiff,
    ) -> Result<PutToolDiffOutcome, StoreError> {
        self.put_tool_diff_at(chat_id, part_id, diff, now_ms(), SIDECAR_LIMITS)
    }

    pub fn read_tool_diff(
        &self,
        chat_id: &str,
        part_id: &str,
        requested_ref: &str,
    ) -> Result<Option<ToolDiff>, StoreError> {
        self.read_tool_diff_at(chat_id, part_id, requested_ref, now_ms())
    }

    pub fn delete_tool_diffs(&self, chat_id: &str) -> Result<(), StoreError> {
        self.conn().execute(
            "DELETE FROM tool_diff_sidecars WHERE chat_id = ?1",
            params![chat_id],
        )?;
        Ok(())
    }

    fn put_tool_diff_at(
        &self,
        chat_id: &str,
        part_id: &str,
        diff: &ToolDiff,
        now_ms: i64,
        limits: SidecarLimits,
    ) -> Result<PutToolDiffOutcome, StoreError> {
        if diff.path.len() > limits.path_bytes {
            return Ok(PutToolDiffOutcome::Rejected(ToolDiffLimit::Path));
        }
        if diff
            .old_text
            .as_ref()
            .is_some_and(|old_text| old_text.len() > limits.source_bytes)
        {
            return Ok(PutToolDiffOutcome::Rejected(ToolDiffLimit::OldSource));
        }
        if diff.new_text.len() > limits.source_bytes {
            return Ok(PutToolDiffOutcome::Rejected(ToolDiffLimit::NewSource));
        }

        let bytes = diff
            .canonical_bytes()
            .expect("ToolDiff serialization cannot fail");
        if bytes.len() > limits.payload_bytes {
            return Ok(PutToolDiffOutcome::Rejected(ToolDiffLimit::Payload));
        }
        let diff_ref = diff.diff_ref().expect("ToolDiff serialization cannot fail");
        let byte_len = u64::try_from(bytes.len()).expect("usize always fits in u64");

        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO tool_diff_sidecars \
                (chat_id, part_id, diff_ref, bytes, byte_len, created_at, accessed_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6) \
             ON CONFLICT(chat_id, part_id) DO UPDATE SET \
                diff_ref = excluded.diff_ref, \
                bytes = excluded.bytes, \
                byte_len = excluded.byte_len, \
                accessed_at = excluded.accessed_at",
            params![chat_id, part_id, diff_ref, bytes, byte_len, now_ms],
        )?;

        let byte_quota = i64::try_from(limits.total_bytes).unwrap_or(i64::MAX);
        let record_quota = i64::try_from(limits.records).unwrap_or(i64::MAX);
        loop {
            let (total_bytes, records): (i64, i64) = tx.query_row(
                "SELECT COALESCE(SUM(byte_len), 0), COUNT(*) FROM tool_diff_sidecars",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            if total_bytes <= byte_quota && records <= record_quota {
                break;
            }
            tx.execute(
                "DELETE FROM tool_diff_sidecars WHERE (chat_id, part_id) = (\
                    SELECT chat_id, part_id FROM tool_diff_sidecars \
                    ORDER BY accessed_at, created_at, chat_id, part_id LIMIT 1\
                 )",
                [],
            )?;
        }
        let current_survives: bool = tx.query_row(
            "SELECT EXISTS( \
                SELECT 1 FROM tool_diff_sidecars \
                WHERE chat_id = ?1 AND part_id = ?2 AND diff_ref = ?3\
             )",
            params![chat_id, part_id, diff_ref],
            |row| row.get(0),
        )?;
        if !current_survives {
            return Err(StoreError::ToolDiffQuota);
        }
        tx.commit()?;

        Ok(PutToolDiffOutcome::Stored { diff_ref, byte_len })
    }

    fn read_tool_diff_at(
        &self,
        chat_id: &str,
        part_id: &str,
        requested_ref: &str,
        now_ms: i64,
    ) -> Result<Option<ToolDiff>, StoreError> {
        let conn = self.conn();
        let row: Option<(String, Vec<u8>, i64)> = conn
            .query_row(
                "SELECT diff_ref, bytes, byte_len FROM tool_diff_sidecars \
                 WHERE chat_id = ?1 AND part_id = ?2",
                params![chat_id, part_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((stored_ref, bytes, byte_len)) = row else {
            return Ok(None);
        };
        if stored_ref != requested_ref {
            return Ok(None);
        }
        let stored_len = usize::try_from(byte_len)
            .map_err(|_| StoreError::CorruptToolDiff("stored byte length is negative"))?;
        if stored_len != bytes.len() {
            return Err(StoreError::CorruptToolDiff(
                "stored byte length does not match bytes",
            ));
        }

        let diff: ToolDiff = serde_json::from_slice(&bytes)
            .map_err(|_| StoreError::CorruptToolDiff("stored bytes are not a tool diff"))?;
        let canonical_bytes = diff
            .canonical_bytes()
            .expect("ToolDiff serialization cannot fail");
        if canonical_bytes != bytes {
            return Err(StoreError::CorruptToolDiff(
                "stored bytes are not canonical",
            ));
        }
        let canonical_ref = diff.diff_ref().expect("ToolDiff serialization cannot fail");
        if canonical_ref != stored_ref {
            return Err(StoreError::CorruptToolDiff(
                "stored reference does not match bytes",
            ));
        }

        conn.execute(
            "UPDATE tool_diff_sidecars SET accessed_at = ?1 WHERE chat_id = ?2 AND part_id = ?3",
            params![now_ms, chat_id, part_id],
        )?;
        Ok(Some(diff))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DocsStore, PutToolDiffOutcome, StoreError, ToolDiffLimit};
    use comet_proto::ToolDiff;
    use rusqlite::OptionalExtension;

    const TEST_LIMITS: SidecarLimits = SidecarLimits {
        path_bytes: 8,
        source_bytes: 16,
        payload_bytes: 64,
        total_bytes: 1_024,
        records: 8,
    };

    fn diff(path: &str, old_text: Option<&str>, new_text: &str) -> ToolDiff {
        ToolDiff {
            path: path.into(),
            old_text: old_text.map(str::to_owned),
            new_text: new_text.into(),
        }
    }

    fn stored_ref(outcome: PutToolDiffOutcome) -> String {
        match outcome {
            PutToolDiffOutcome::Stored { diff_ref, .. } => diff_ref,
            PutToolDiffOutcome::Rejected(limit) => panic!("unexpected rejection: {limit:?}"),
        }
    }

    fn put_at(
        store: &DocsStore,
        chat_id: &str,
        part_id: &str,
        value: &ToolDiff,
        now_ms: i64,
        limits: SidecarLimits,
    ) -> String {
        stored_ref(
            store
                .put_tool_diff_at(chat_id, part_id, value, now_ms, limits)
                .unwrap(),
        )
    }

    fn canonical_len(value: &ToolDiff) -> u64 {
        value.canonical_bytes().unwrap().len() as u64
    }

    fn sidecar_row(
        store: &DocsStore,
        chat_id: &str,
        part_id: &str,
    ) -> Option<(String, Vec<u8>, i64, i64, i64)> {
        store
            .conn()
            .query_row(
                "SELECT diff_ref, bytes, byte_len, created_at, accessed_at \
                 FROM tool_diff_sidecars WHERE chat_id = ?1 AND part_id = ?2",
                rusqlite::params![chat_id, part_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()
            .unwrap()
    }

    fn sidecar_count(store: &DocsStore) -> i64 {
        store
            .conn()
            .query_row("SELECT COUNT(*) FROM tool_diff_sidecars", [], |row| {
                row.get(0)
            })
            .unwrap()
    }

    fn sidecar_total_bytes(store: &DocsStore) -> i64 {
        store
            .conn()
            .query_row(
                "SELECT COALESCE(SUM(byte_len), 0) FROM tool_diff_sidecars",
                [],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn assert_quota_failure(result: Result<PutToolDiffOutcome, StoreError>) {
        let error = result.expect_err("a pruned current sidecar must roll back");
        assert_eq!(
            error.to_string(),
            "tool diff sidecar cannot retain the current diff within quota"
        );
    }

    #[test]
    fn tool_diff_migration_roundtrip_and_restart() {
        let dir = tempfile::tempdir().unwrap();
        let original = diff("src/a", Some("old"), "new");
        let replacement = diff("src/a", Some("before"), "after");

        let replacement_ref = {
            let store = DocsStore::open(dir.path()).unwrap();
            stored_ref(
                store
                    .put_tool_diff_at("chat-1", "part-1", &original, 10, TEST_LIMITS)
                    .unwrap(),
            );
            let replacement_ref = stored_ref(
                store
                    .put_tool_diff_at("chat-1", "part-1", &replacement, 20, TEST_LIMITS)
                    .unwrap(),
            );
            let (created_at, accessed_at): (i64, i64) = store
                .conn()
                .query_row(
                    "SELECT created_at, accessed_at FROM tool_diff_sidecars \
                     WHERE chat_id = ?1 AND part_id = ?2",
                    ["chat-1", "part-1"],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert_eq!((created_at, accessed_at), (10, 20));
            replacement_ref
        };

        let store = DocsStore::open(dir.path()).unwrap();
        assert_eq!(
            store
                .read_tool_diff_at("chat-1", "part-1", &replacement_ref, 30)
                .unwrap(),
            Some(replacement)
        );
    }

    #[test]
    fn tool_diff_rejects_each_size_limit() {
        let dir = tempfile::tempdir().unwrap();
        let store = DocsStore::open(dir.path()).unwrap();
        let oversized_path = diff(
            &"p".repeat(TEST_LIMITS.path_bytes + 1),
            Some(&"o".repeat(TEST_LIMITS.source_bytes + 1)),
            &"n".repeat(TEST_LIMITS.source_bytes + 1),
        );
        let oversized_old = diff(
            "src/a",
            Some(&"o".repeat(TEST_LIMITS.source_bytes + 1)),
            &"n".repeat(TEST_LIMITS.source_bytes + 1),
        );
        let oversized_new = diff("src/a", None, &"\"".repeat(TEST_LIMITS.source_bytes + 1));
        let oversized_payload = diff(
            "src/a",
            Some(&"\"".repeat(TEST_LIMITS.source_bytes)),
            &"\"".repeat(TEST_LIMITS.source_bytes),
        );

        assert!(matches!(
            store
                .put_tool_diff_at("chat-1", "path", &oversized_path, 10, TEST_LIMITS)
                .unwrap(),
            PutToolDiffOutcome::Rejected(ToolDiffLimit::Path)
        ));
        assert!(matches!(
            store
                .put_tool_diff_at("chat-1", "old", &oversized_old, 10, TEST_LIMITS)
                .unwrap(),
            PutToolDiffOutcome::Rejected(ToolDiffLimit::OldSource)
        ));
        assert!(matches!(
            store
                .put_tool_diff_at("chat-1", "new", &oversized_new, 10, TEST_LIMITS)
                .unwrap(),
            PutToolDiffOutcome::Rejected(ToolDiffLimit::NewSource)
        ));
        assert!(matches!(
            store
                .put_tool_diff_at("chat-1", "payload", &oversized_payload, 10, TEST_LIMITS)
                .unwrap(),
            PutToolDiffOutcome::Rejected(ToolDiffLimit::Payload)
        ));
        let production_oversized_source = "n".repeat(TOOL_DIFF_SOURCE_MAX_BYTES + 1);
        assert!(matches!(
            store
                .put_tool_diff(
                    "chat-1",
                    "production-new",
                    &diff("src/a", None, &production_oversized_source),
                )
                .unwrap(),
            PutToolDiffOutcome::Rejected(ToolDiffLimit::NewSource)
        ));
    }

    #[test]
    fn tool_diff_size_limits_use_utf8_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let store = DocsStore::open(dir.path()).unwrap();
        let limits = SidecarLimits {
            path_bytes: 4,
            source_bytes: 4,
            payload_bytes: 256,
            ..TEST_LIMITS
        };
        let path_at_limit = diff(&"é".repeat(2), None, "x");
        let path_over_limit = diff(&"é".repeat(2), None, "x");
        let old_at_limit = diff("src", Some(&"é".repeat(2)), "x");
        let old_over_limit = diff("src", Some(&"é".repeat(3)), "x");
        let new_at_limit = diff("src", None, &"é".repeat(2));
        let new_over_limit = diff("src", None, &"é".repeat(3));

        assert!(matches!(
            store
                .put_tool_diff_at("chat", "path-at", &path_at_limit, 10, limits)
                .unwrap(),
            PutToolDiffOutcome::Stored { .. }
        ));
        assert!(matches!(
            store
                .put_tool_diff_at(
                    "chat",
                    "path-over",
                    &ToolDiff {
                        path: format!("{}x", path_over_limit.path),
                        ..path_over_limit
                    },
                    10,
                    limits,
                )
                .unwrap(),
            PutToolDiffOutcome::Rejected(ToolDiffLimit::Path)
        ));
        assert!(matches!(
            store
                .put_tool_diff_at("chat", "old-at", &old_at_limit, 10, limits)
                .unwrap(),
            PutToolDiffOutcome::Stored { .. }
        ));
        assert!(matches!(
            store
                .put_tool_diff_at("chat", "old-over", &old_over_limit, 10, limits)
                .unwrap(),
            PutToolDiffOutcome::Rejected(ToolDiffLimit::OldSource)
        ));
        assert!(matches!(
            store
                .put_tool_diff_at("chat", "new-at", &new_at_limit, 10, limits)
                .unwrap(),
            PutToolDiffOutcome::Stored { .. }
        ));
        assert!(matches!(
            store
                .put_tool_diff_at("chat", "new-over", &new_over_limit, 10, limits)
                .unwrap(),
            PutToolDiffOutcome::Rejected(ToolDiffLimit::NewSource)
        ));
    }

    #[test]
    fn tool_diff_read_rejects_a_stale_reference() {
        let dir = tempfile::tempdir().unwrap();
        let store = DocsStore::open(dir.path()).unwrap();
        let value = diff("src/a", None, "new");
        stored_ref(
            store
                .put_tool_diff_at("chat-1", "part-1", &value, 10, TEST_LIMITS)
                .unwrap(),
        );

        assert_eq!(
            store
                .read_tool_diff_at("chat-1", "part-1", "v1:stale", 20)
                .unwrap(),
            None
        );
        let accessed_at: i64 = store
            .conn()
            .query_row(
                "SELECT accessed_at FROM tool_diff_sidecars WHERE chat_id = ?1 AND part_id = ?2",
                ["chat-1", "part-1"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(accessed_at, 10);
    }

    #[test]
    fn tool_diff_read_detects_corrupt_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let store = DocsStore::open(dir.path()).unwrap();
        let original = diff("src/a", None, "old value");
        let original_bytes = original.canonical_bytes().unwrap();
        let original_ref = put_at(&store, "chat-1", "part-1", &original, 10, TEST_LIMITS);
        let other = diff("src/a", None, "new value");
        let other_bytes = other.canonical_bytes().unwrap();
        let noncanonical_bytes = br#"{"newText":"old value","path":"src/a"}"#.to_vec();
        let cases = [
            (
                "negative length",
                original_bytes.clone(),
                -1,
                original_ref.clone(),
                "stored byte length is negative",
            ),
            (
                "length mismatch",
                original_bytes.clone(),
                original_bytes.len() as i64 + 1,
                original_ref.clone(),
                "stored byte length does not match bytes",
            ),
            (
                "malformed json",
                b"not-json".to_vec(),
                8,
                original_ref.clone(),
                "stored bytes are not a tool diff",
            ),
            (
                "noncanonical json",
                noncanonical_bytes.clone(),
                noncanonical_bytes.len() as i64,
                original_ref.clone(),
                "stored bytes are not canonical",
            ),
            (
                "reference mismatch",
                other_bytes.clone(),
                other_bytes.len() as i64,
                original_ref.clone(),
                "stored reference does not match bytes",
            ),
        ];

        for (name, bytes, byte_len, diff_ref, reason) in cases {
            store
                .conn()
                .execute(
                    "UPDATE tool_diff_sidecars \
                     SET diff_ref = ?1, bytes = ?2, byte_len = ?3, accessed_at = 10 \
                     WHERE chat_id = ?4 AND part_id = ?5",
                    rusqlite::params![diff_ref, bytes, byte_len, "chat-1", "part-1"],
                )
                .unwrap();

            let error = store
                .read_tool_diff_at("chat-1", "part-1", &original_ref, 20)
                .expect_err(name);
            match error {
                StoreError::CorruptToolDiff(actual) => assert_eq!(actual, reason, "{name}"),
                other => panic!("{name} returned unexpected error: {other}"),
            }
            assert_eq!(
                sidecar_row(&store, "chat-1", "part-1").unwrap().4,
                10,
                "{name} must not refresh LRU age"
            );
        }
    }

    #[test]
    fn tool_diff_read_refreshes_lru_age() {
        let dir = tempfile::tempdir().unwrap();
        let store = DocsStore::open(dir.path()).unwrap();
        let value = diff("src/a", None, "new");
        let reference = stored_ref(
            store
                .put_tool_diff_at("chat-1", "part-1", &value, 10, TEST_LIMITS)
                .unwrap(),
        );

        assert_eq!(
            store
                .read_tool_diff_at("chat-1", "part-1", &reference, 30)
                .unwrap(),
            Some(value)
        );
        let accessed_at: i64 = store
            .conn()
            .query_row(
                "SELECT accessed_at FROM tool_diff_sidecars WHERE chat_id = ?1 AND part_id = ?2",
                ["chat-1", "part-1"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(accessed_at, 30);
    }

    #[test]
    fn tool_diff_prunes_byte_quota() {
        let dir = tempfile::tempdir().unwrap();
        let store = DocsStore::open(dir.path()).unwrap();
        let value = diff("src/a", None, "abc");
        let byte_quota = value.canonical_bytes().unwrap().len() as u64 * 2;
        let limits = SidecarLimits {
            total_bytes: byte_quota,
            ..TEST_LIMITS
        };
        let first_ref = stored_ref(
            store
                .put_tool_diff_at("chat-1", "part-1", &value, 10, limits)
                .unwrap(),
        );
        let second_ref = stored_ref(
            store
                .put_tool_diff_at("chat-1", "part-2", &value, 20, limits)
                .unwrap(),
        );
        let third_ref = stored_ref(
            store
                .put_tool_diff_at("chat-1", "part-3", &value, 30, limits)
                .unwrap(),
        );

        assert_eq!(
            store
                .read_tool_diff_at("chat-1", "part-1", &first_ref, 30)
                .unwrap(),
            None
        );
        assert_eq!(
            store
                .read_tool_diff_at("chat-1", "part-2", &second_ref, 30)
                .unwrap(),
            Some(value.clone())
        );
        assert_eq!(
            store
                .read_tool_diff_at("chat-1", "part-3", &third_ref, 30)
                .unwrap(),
            Some(value)
        );
    }

    #[test]
    fn tool_diff_prunes_record_quota() {
        let dir = tempfile::tempdir().unwrap();
        let store = DocsStore::open(dir.path()).unwrap();
        let value = diff("src/a", None, "abc");
        let limits = SidecarLimits {
            total_bytes: 1_024,
            records: 2,
            ..TEST_LIMITS
        };
        let first_ref = stored_ref(
            store
                .put_tool_diff_at("chat-1", "part-1", &value, 10, limits)
                .unwrap(),
        );
        let second_ref = stored_ref(
            store
                .put_tool_diff_at("chat-1", "part-2", &value, 20, limits)
                .unwrap(),
        );
        assert_eq!(sidecar_count(&store), 2);
        let third_ref = stored_ref(
            store
                .put_tool_diff_at("chat-1", "part-3", &value, 30, limits)
                .unwrap(),
        );

        assert_eq!(
            store
                .read_tool_diff_at("chat-1", "part-1", &first_ref, 30)
                .unwrap(),
            None
        );
        assert_eq!(
            store
                .read_tool_diff_at("chat-1", "part-2", &second_ref, 30)
                .unwrap(),
            Some(value.clone())
        );
        assert_eq!(
            store
                .read_tool_diff_at("chat-1", "part-3", &third_ref, 30)
                .unwrap(),
            Some(value)
        );
    }

    #[test]
    fn delete_tool_diffs_is_scoped_to_one_chat() {
        let dir = tempfile::tempdir().unwrap();
        let store = DocsStore::open(dir.path()).unwrap();
        let value = diff("src/a", None, "new");
        let first_chat_ref = stored_ref(
            store
                .put_tool_diff_at("chat-1", "part-1", &value, 10, TEST_LIMITS)
                .unwrap(),
        );
        let second_part_ref = stored_ref(
            store
                .put_tool_diff_at("chat-1", "part-2", &value, 20, TEST_LIMITS)
                .unwrap(),
        );
        let second_chat_ref = stored_ref(
            store
                .put_tool_diff_at("chat-2", "part-1", &value, 30, TEST_LIMITS)
                .unwrap(),
        );
        assert_eq!(sidecar_count(&store), 3);
        assert!(sidecar_row(&store, "chat-1", "part-1").is_some());
        assert!(sidecar_row(&store, "chat-1", "part-2").is_some());
        assert!(sidecar_row(&store, "chat-2", "part-1").is_some());

        store.delete_tool_diffs("chat-1").unwrap();

        assert_eq!(
            store
                .read_tool_diff_at("chat-1", "part-1", &first_chat_ref, 30)
                .unwrap(),
            None
        );
        assert_eq!(
            store
                .read_tool_diff_at("chat-1", "part-2", &second_part_ref, 30)
                .unwrap(),
            None
        );
        assert_eq!(
            store
                .read_tool_diff_at("chat-2", "part-1", &second_chat_ref, 30)
                .unwrap(),
            Some(value)
        );
    }

    #[test]
    fn tool_diff_quota_failure_rolls_back_unretainable_new_rows() {
        let dir = tempfile::tempdir().unwrap();
        let store = DocsStore::open(dir.path()).unwrap();
        let value = diff("src/a", None, "new");
        let cases = [
            (
                "zero records",
                SidecarLimits {
                    records: 0,
                    ..TEST_LIMITS
                },
            ),
            (
                "sub-row byte quota",
                SidecarLimits {
                    total_bytes: canonical_len(&value) - 1,
                    records: 1,
                    ..TEST_LIMITS
                },
            ),
        ];

        for (name, limits) in cases {
            assert_quota_failure(store.put_tool_diff_at("chat-1", name, &value, 10, limits));
            assert!(
                sidecar_row(&store, "chat-1", name).is_none(),
                "{name} must roll back its write"
            );
        }
    }

    #[test]
    fn tool_diff_clock_regression_cannot_publish_a_pruned_row() {
        let dir = tempfile::tempdir().unwrap();
        let store = DocsStore::open(dir.path()).unwrap();
        let limits = SidecarLimits {
            records: 1,
            ..TEST_LIMITS
        };
        let existing = diff("old", None, "old");
        let clock_regressed = diff("new", None, "new");
        let existing_ref = put_at(&store, "chat", "existing", &existing, 20, limits);

        assert_quota_failure(store.put_tool_diff_at(
            "chat",
            "regressed",
            &clock_regressed,
            10,
            limits,
        ));
        assert_eq!(
            sidecar_row(&store, "chat", "existing").unwrap().0,
            existing_ref
        );
        assert!(sidecar_row(&store, "chat", "regressed").is_none());
    }

    #[test]
    fn tool_diff_growing_replacement_rolls_back_when_it_loses_an_equal_time_tie() {
        let dir = tempfile::tempdir().unwrap();
        let store = DocsStore::open(dir.path()).unwrap();
        let original = diff("src/a", None, "a");
        let peer = diff("src/b", None, "a");
        let growing = diff("src/a", None, "this replacement grows");
        let limits = SidecarLimits {
            path_bytes: 64,
            source_bytes: 64,
            payload_bytes: 256,
            total_bytes: canonical_len(&original) + canonical_len(&peer),
            records: 2,
        };
        let original_ref = put_at(&store, "chat-z", "part", &original, 0, limits);
        assert_eq!(
            store
                .read_tool_diff_at("chat-z", "part", &original_ref, 10)
                .unwrap(),
            Some(original.clone())
        );
        put_at(&store, "chat-a", "part", &peer, 10, limits);

        assert_quota_failure(store.put_tool_diff_at("chat-z", "part", &growing, 10, limits));
        let original_row = sidecar_row(&store, "chat-z", "part").unwrap();
        assert_eq!(original_row.0, original_ref);
        assert_eq!(original_row.1, original.canonical_bytes().unwrap());
        assert_eq!((original_row.3, original_row.4), (0, 10));
        assert!(sidecar_row(&store, "chat-a", "part").is_some());
    }

    #[test]
    fn tool_diff_prunes_global_byte_quota_across_chats() {
        let dir = tempfile::tempdir().unwrap();
        let store = DocsStore::open(dir.path()).unwrap();
        let value = diff("src/a", None, "abc");
        let limits = SidecarLimits {
            total_bytes: canonical_len(&value) * 2,
            ..TEST_LIMITS
        };
        put_at(&store, "chat-a", "part-1", &value, 10, limits);
        put_at(&store, "chat-b", "part-1", &value, 20, limits);
        put_at(&store, "chat-a", "part-2", &value, 30, limits);

        assert_eq!(sidecar_count(&store), 2);
        assert!(sidecar_row(&store, "chat-a", "part-1").is_none());
        assert!(sidecar_row(&store, "chat-b", "part-1").is_some());
        assert!(sidecar_row(&store, "chat-a", "part-2").is_some());
    }

    #[test]
    fn tool_diff_prunes_global_record_quota_across_chats() {
        let dir = tempfile::tempdir().unwrap();
        let store = DocsStore::open(dir.path()).unwrap();
        let value = diff("src/a", None, "abc");
        let limits = SidecarLimits {
            records: 2,
            ..TEST_LIMITS
        };
        put_at(&store, "chat-a", "part-1", &value, 10, limits);
        put_at(&store, "chat-b", "part-1", &value, 20, limits);
        put_at(&store, "chat-a", "part-2", &value, 30, limits);

        assert_eq!(sidecar_count(&store), 2);
        assert!(sidecar_row(&store, "chat-a", "part-1").is_none());
        assert!(sidecar_row(&store, "chat-b", "part-1").is_some());
        assert!(sidecar_row(&store, "chat-a", "part-2").is_some());
    }

    #[test]
    fn tool_diff_pruning_breaks_equal_access_ties_by_created_at() {
        let dir = tempfile::tempdir().unwrap();
        let store = DocsStore::open(dir.path()).unwrap();
        let value = diff("src/a", None, "abc");
        let limits = SidecarLimits {
            records: 1,
            ..TEST_LIMITS
        };
        let old_ref = put_at(&store, "chat", "part-z", &value, 10, limits);
        store
            .read_tool_diff_at("chat", "part-z", &old_ref, 30)
            .unwrap();
        put_at(&store, "chat", "part-a", &value, 30, limits);

        assert!(sidecar_row(&store, "chat", "part-z").is_none());
        assert!(sidecar_row(&store, "chat", "part-a").is_some());
    }

    #[test]
    fn tool_diff_pruning_breaks_equal_access_ties_by_chat_id() {
        let dir = tempfile::tempdir().unwrap();
        let store = DocsStore::open(dir.path()).unwrap();
        let value = diff("src/a", None, "abc");
        let limits = SidecarLimits {
            records: 1,
            ..TEST_LIMITS
        };
        put_at(&store, "chat-a", "part-z", &value, 10, limits);
        put_at(&store, "chat-z", "part-a", &value, 10, limits);

        assert!(sidecar_row(&store, "chat-a", "part-z").is_none());
        assert!(sidecar_row(&store, "chat-z", "part-a").is_some());
    }

    #[test]
    fn tool_diff_pruning_breaks_equal_access_ties_by_part_id() {
        let dir = tempfile::tempdir().unwrap();
        let store = DocsStore::open(dir.path()).unwrap();
        let value = diff("src/a", None, "abc");
        let limits = SidecarLimits {
            records: 1,
            ..TEST_LIMITS
        };
        put_at(&store, "chat", "part-a", &value, 10, limits);
        put_at(&store, "chat", "part-z", &value, 10, limits);

        assert!(sidecar_row(&store, "chat", "part-a").is_none());
        assert!(sidecar_row(&store, "chat", "part-z").is_some());
    }

    #[test]
    fn tool_diff_replacements_update_quota_accounting_and_preserve_created_at() {
        let dir = tempfile::tempdir().unwrap();
        let store = DocsStore::open(dir.path()).unwrap();
        let original = diff("src/a", None, "a");
        let peer = diff("src/b", None, "peer");
        let growing = diff("src/a", None, "this replacement grows");
        let shrinking = diff("src/a", None, "x");
        let limits = SidecarLimits {
            path_bytes: 64,
            source_bytes: 64,
            payload_bytes: 256,
            ..TEST_LIMITS
        };
        let original_ref = put_at(&store, "chat", "part-a", &original, 10, limits);
        put_at(&store, "chat", "part-b", &peer, 20, limits);

        put_at(&store, "chat", "part-a", &growing, 30, limits);
        let grown_row = sidecar_row(&store, "chat", "part-a").unwrap();
        assert_eq!(grown_row.3, 10);
        assert_eq!(grown_row.4, 30);
        assert_ne!(grown_row.0, original_ref);
        assert_eq!(
            sidecar_total_bytes(&store),
            (canonical_len(&growing) + canonical_len(&peer)) as i64
        );

        put_at(&store, "chat", "part-a", &shrinking, 40, limits);
        let shrunk_row = sidecar_row(&store, "chat", "part-a").unwrap();
        assert_eq!(shrunk_row.3, 10);
        assert_eq!(shrunk_row.4, 40);
        assert_eq!(
            sidecar_total_bytes(&store),
            (canonical_len(&shrinking) + canonical_len(&peer)) as i64
        );
    }

    #[test]
    fn tool_diff_successful_read_changes_the_later_eviction_victim() {
        let dir = tempfile::tempdir().unwrap();
        let store = DocsStore::open(dir.path()).unwrap();
        let value = diff("src/a", None, "abc");
        let limits = SidecarLimits {
            records: 2,
            ..TEST_LIMITS
        };
        let first_ref = put_at(&store, "chat", "part-1", &value, 10, limits);
        put_at(&store, "chat", "part-2", &value, 20, limits);
        assert_eq!(
            store
                .read_tool_diff_at("chat", "part-1", &first_ref, 30)
                .unwrap(),
            Some(value.clone())
        );
        put_at(&store, "chat", "part-3", &value, 40, limits);

        assert!(sidecar_row(&store, "chat", "part-1").is_some());
        assert!(sidecar_row(&store, "chat", "part-2").is_none());
        assert!(sidecar_row(&store, "chat", "part-3").is_some());
    }
}
