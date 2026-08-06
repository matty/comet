//! Sidebar search: one field over both spaces and sessions.
//!
//! Pure matching lives here as a free function with unit tests (the `rail.rs`
//! convention); rendering is an `impl Shell` extension below it.
//!
//! Search deliberately ignores [`SidebarScope`] — it always reads the whole
//! projected set. A scoped list is a convenience, never a wall.

use comet_proto::{Chat, Space};

/// Matching spaces and chats, by id, in the input's order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct SearchResults {
    pub spaces: Vec<String>,
    pub chats: Vec<String>,
}

impl SearchResults {
    pub fn is_empty(&self) -> bool {
        self.spaces.is_empty() && self.chats.is_empty()
    }
}

/// `None` = not searching (blank query). `Some(empty)` = searching, nothing
/// matched — a different state, and a different thing to draw.
///
/// Matches, case-insensitively, on: the space's display name, its path, the
/// session title, the branch, and the owning device's name. The path is
/// included because `display_name()` falls back to the folder basename, so a
/// space named "api" at `~/work/acme/api` must still be findable by "acme".
pub(super) fn filter(
    query: &str,
    spaces: &[Space],
    chats: &[Chat],
    device_name: &dyn Fn(&str) -> Option<String>,
) -> Option<SearchResults> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return None;
    }
    let hit = |hay: &str| hay.to_lowercase().contains(&needle);

    let matching_spaces: Vec<String> = spaces
        .iter()
        .filter(|s| hit(s.display_name()) || hit(&s.path))
        .map(|s| s.id.clone())
        .collect();

    let matching_chats: Vec<String> = chats
        .iter()
        .filter(|c| !c.archived)
        .filter(|c| {
            let title_hit = c.title.as_deref().is_some_and(hit);
            let branch_hit = c.branch.as_deref().is_some_and(hit);
            let device_hit = device_name(&c.device_id).as_deref().is_some_and(hit);
            let space_hit = c.space_id.as_deref().is_some_and(|id| {
                spaces
                    .iter()
                    .find(|s| s.id == id)
                    .is_some_and(|s| hit(s.display_name()) || hit(&s.path))
            });
            title_hit || branch_hit || device_hit || space_hit
        })
        .map(|c| c.id.clone())
        .collect();

    Some(SearchResults {
        spaces: matching_spaces,
        chats: matching_chats,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn spaces() -> Vec<Space> {
        vec![
            Space {
                id: "comet-id".into(),
                device_id: "d2".into(),
                path: "/home/m/comet".into(),
                name: None,
                git_detected: true,
                git_checked_at: None,
                checkout_id: None,
                created_at: Utc::now(),
            },
            Space {
                id: "fade-lab-id".into(),
                device_id: "d1".into(),
                path: "/home/m/work/acme/fade-lab".into(),
                name: None,
                git_detected: true,
                git_checked_at: None,
                checkout_id: None,
                created_at: Utc::now(),
            },
        ]
    }

    fn chats() -> Vec<Chat> {
        vec![
            Chat {
                id: "c-fade".into(),
                device_id: "d2".into(),
                title: Some("Fade exploration".into()),
                archived: false,
                cwd: Some("/home/m/work/acme/fade-lab".into()),
                branch: Some("main".into()),
                checkout_id: None,
                config: None,
                last_message_preview: None,
                last_message_at: Some(Utc::now()),
                created_at: Utc::now(),
                harness_session_id: None,
                harness_session_cwd: None,
                space_id: Some("fade-lab-id".into()),
                last_seen_at: None,
            },
            Chat {
                id: "c-tabs".into(),
                device_id: "d1".into(),
                title: Some("Chat about the sidebar".into()),
                archived: false,
                cwd: Some("/home/m/comet".into()),
                branch: Some("tab-drag-followup".into()),
                checkout_id: None,
                config: None,
                last_message_preview: None,
                last_message_at: Some(Utc::now()),
                created_at: Utc::now(),
                harness_session_id: None,
                harness_session_cwd: None,
                space_id: Some("comet-id".into()),
                last_seen_at: None,
            },
        ]
    }

    fn archived_chat_titled(title: &str) -> Chat {
        Chat {
            id: "c-archived".into(),
            device_id: "d3".into(),
            title: Some(title.into()),
            archived: true,
            cwd: None,
            branch: None,
            checkout_id: None,
            config: None,
            last_message_preview: None,
            last_message_at: None,
            created_at: Utc::now(),
            harness_session_id: None,
            harness_session_cwd: None,
            space_id: None,
            last_seen_at: None,
        }
    }

    fn devices(id: &str) -> Option<String> {
        (id == "d1").then(|| "mac-studio".to_string())
    }

    #[test]
    fn empty_query_is_not_a_search() {
        assert!(filter("", &spaces(), &chats(), &devices).is_none());
        assert!(filter("   ", &spaces(), &chats(), &devices).is_none());
    }

    #[test]
    fn matches_space_name_and_session_title() {
        let r = filter("fade", &spaces(), &chats(), &devices).unwrap();
        assert_eq!(r.spaces, ["fade-lab-id"]);
        assert!(r.chats.contains(&"c-fade".to_string()));
    }

    #[test]
    fn matches_the_space_path_not_just_its_display_name() {
        // "fade-lab" lives at /home/m/work/acme/fade-lab.
        let r = filter("acme", &spaces(), &chats(), &devices).unwrap();
        assert_eq!(
            r.spaces,
            ["fade-lab-id"],
            "display_name falls back to the basename, so the path has to match too"
        );
    }

    #[test]
    fn matches_branch_and_device() {
        let r = filter("tab-drag", &spaces(), &chats(), &devices).unwrap();
        assert_eq!(r.chats, ["c-tabs"]);
        let r = filter("mac-studio", &spaces(), &chats(), &devices).unwrap();
        assert!(!r.chats.is_empty());
    }

    #[test]
    fn matching_is_case_insensitive() {
        let lower = filter("fade", &spaces(), &chats(), &devices).unwrap();
        let upper = filter("FaDe", &spaces(), &chats(), &devices).unwrap();
        assert_eq!(lower.chats, upper.chats);
    }

    #[test]
    fn archived_chats_are_never_returned() {
        let mut all = chats();
        all.push(archived_chat_titled("fade something"));
        let r = filter("fade", &spaces(), &all, &devices).unwrap();
        assert!(!r.chats.iter().any(|id| id == "c-archived"));
    }

    #[test]
    fn no_matches_is_a_search_with_empty_groups() {
        let r = filter("wingleeio", &spaces(), &chats(), &devices).unwrap();
        assert!(r.spaces.is_empty() && r.chats.is_empty());
        assert!(r.is_empty());
    }
}
