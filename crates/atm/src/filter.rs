//! Free-text session filter for the `/` search prompt.
//!
//! Pure logic: no I/O, no TUI types. A session matches when the query is a
//! case-insensitive substring of one of its display fields, so typing a
//! harness tag (`codex`) or a status label (`needs input`) filters by vendor
//! or status without dedicated keys.

use atm_core::{SessionId, SessionView};
use std::collections::{HashMap, HashSet};

/// Result of applying a query to a set of sessions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FilterSelection {
    /// Sessions that match the query.
    pub matched: HashSet<SessionId>,
    /// Matched sessions plus their ancestors, kept so a matching child
    /// stays nested under its lead.
    pub visible: HashSet<SessionId>,
}

/// Returns true if `query` is a case-insensitive substring of any of the
/// session's searchable fields. An empty query matches everything.
#[must_use]
pub fn matches(session: &SessionView, query: &str) -> bool {
    let query = query.to_lowercase();
    [
        Some(session.harness.as_str()),
        Some(session.status_label.as_str()),
        Some(session.model.as_str()),
        Some(session.agent_type.as_str()),
        Some(session.id_short.as_str()),
        session.project_root.as_deref(),
        session.worktree_branch.as_deref(),
        // `working_directory` is a shortened display suffix; the full
        // worktree path covers the part it cuts off.
        session.worktree_path.as_deref(),
        session.working_directory.as_deref(),
    ]
    .into_iter()
    .flatten()
    .any(|field| field.to_lowercase().contains(&query))
}

/// Selects the sessions matching `query` and the ancestors needed to show
/// them in place. Children of a matching session are not pulled in unless
/// they match too.
#[must_use]
pub fn select(sessions: &[&SessionView], query: &str) -> FilterSelection {
    let present: HashSet<&SessionId> = sessions.iter().map(|s| &s.id).collect();
    let parent_of: HashMap<&SessionId, &SessionId> = sessions
        .iter()
        .filter_map(|s| s.parent_session_id.as_ref().map(|p| (&s.id, p)))
        .filter(|(_, p)| present.contains(p))
        .collect();

    let mut selection = FilterSelection::default();
    for session in sessions.iter().filter(|s| matches(s, query)) {
        selection.matched.insert(session.id.clone());
        // Walk up the parent chain. `insert` returning false means the
        // ancestor was already added (or a cycle), so stop there.
        let mut current = Some(&session.id);
        while let Some(id) = current {
            if !selection.visible.insert(id.clone()) {
                break;
            }
            current = parent_of.get(id).copied();
        }
    }
    selection
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str, parent: Option<&str>) -> SessionView {
        SessionView {
            id: SessionId::new(id),
            id_short: id.to_string(),
            harness: "claude".to_string(),
            status_label: "idle".to_string(),
            parent_session_id: parent.map(SessionId::new),
            ..Default::default()
        }
    }

    fn ids(set: &HashSet<SessionId>) -> Vec<&str> {
        let mut v: Vec<&str> = set.iter().map(|id| id.as_str()).collect();
        v.sort_unstable();
        v
    }

    #[test]
    fn matches_is_case_insensitive_across_fields() {
        let mut s = session("abc", None);
        s.harness = "codex".to_string();
        s.status_label = "needs input".to_string();
        s.worktree_branch = Some("feat/Login".to_string());
        assert!(matches(&s, "CODEX"));
        assert!(matches(&s, "needs in"));
        assert!(matches(&s, "login"));
        assert!(!matches(&s, "pi-agent"));
    }

    #[test]
    fn matches_full_worktree_path_beyond_truncated_cwd() {
        // SessionView shortens working_directory to its last 27 bytes.
        let mut s = session("a", None);
        s.working_directory = Some("...rees/feature-with-long-name".to_string());
        s.worktree_path =
            Some("/home/user/client-acme/.worktrees/feature-with-long-name".to_string());
        assert!(matches(&s, "client-acme"));
    }

    #[test]
    fn empty_query_matches_everything() {
        assert!(matches(&session("a", None), ""));
    }

    #[test]
    fn missing_optional_fields_do_not_match() {
        let s = session("a", None);
        assert!(!matches(&s, "/home"));
    }

    #[test]
    fn matching_child_keeps_its_ancestors_visible() {
        let lead = session("lead", None);
        let mate = session("mate", Some("lead"));
        let mut sub = session("sub", Some("mate"));
        sub.harness = "codex".to_string();
        let sel = select(&[&lead, &mate, &sub], "codex");
        assert_eq!(ids(&sel.matched), vec!["sub"]);
        assert_eq!(ids(&sel.visible), vec!["lead", "mate", "sub"]);
    }

    #[test]
    fn matching_lead_does_not_pull_in_children() {
        let mut lead = session("lead", None);
        lead.harness = "codex".to_string();
        let child = session("child", Some("lead"));
        let sel = select(&[&lead, &child], "codex");
        assert_eq!(ids(&sel.visible), vec!["lead"]);
    }

    #[test]
    fn missing_parent_is_not_added() {
        let orphan = session("orphan", Some("gone"));
        let sel = select(&[&orphan], "orphan");
        assert_eq!(ids(&sel.visible), vec!["orphan"]);
    }

    #[test]
    fn parent_cycle_terminates() {
        let a = session("first", Some("second"));
        let b = session("second", Some("first"));
        let sel = select(&[&a, &b], "first");
        assert_eq!(ids(&sel.matched), vec!["first"]);
        assert_eq!(ids(&sel.visible), vec!["first", "second"]);
    }
}
