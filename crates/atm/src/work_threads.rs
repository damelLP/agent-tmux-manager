//! Reporting groups from existing org facts; not an authority or a new org entity.

use super::*;

// Reject both sides of an ambiguous identity/pane join, never pick a lucky first row.
pub(super) fn unique_seats(org: &WorkOrg) -> BTreeMap<&SeatId, &WorkSeat> {
    let seats: Vec<_> = org.projects.iter().flat_map(|p| &p.seats).collect();
    seats
        .iter()
        .filter(|seat| {
            seats
                .iter()
                .filter(|other| other.agent_id == seat.agent_id)
                .count()
                == 1
                && seat.pane.as_ref().is_none_or(|pane| {
                    seats
                        .iter()
                        .filter(|other| other.pane.as_ref() == Some(pane))
                        .count()
                        == 1
                })
        })
        .map(|seat| (&seat.agent_id, *seat))
        .collect()
}

pub(super) fn reporting_threads(org: &WorkOrg) -> BTreeMap<SeatId, SeatId> {
    let seats = unique_seats(org);
    let valid: BTreeSet<_> = seats
        .keys()
        .copied()
        .filter(|id| {
            let mut current = Some(*id);
            let mut visited = BTreeSet::new();
            while let Some(id) = current {
                let Some(seat) = seats.get(id) else {
                    return false;
                };
                if seat.line_error.is_some() || !visited.insert(id) {
                    return false;
                }
                current = seat
                    .manager
                    .as_ref()
                    .filter(|id| !matches!(id.0.as_str(), "owner" | "system"));
            }
            true
        })
        .collect();
    let leads: BTreeSet<_> = seats
        .values()
        .filter(|seat| valid.contains(&seat.agent_id))
        .filter_map(|seat| seat.manager.as_ref())
        .filter(|id| valid.contains(id))
        .collect();
    seats
        .values()
        .filter(|seat| valid.contains(&seat.agent_id))
        .filter_map(|seat| {
            let lead = if leads.contains(&seat.agent_id) {
                &seat.agent_id
            } else {
                seat.manager.as_ref().filter(|id| leads.contains(id))?
            };
            Some((seat.agent_id.clone(), lead.clone()))
        })
        .collect()
}

/// Every supplied session appears once; paneless org seats create no session rows.
#[must_use]
pub(crate) fn build_work_threads(
    sessions: &[SessionView],
    windows: &[WorkWindow],
    org: &WorkOrg,
) -> Vec<TreeNode> {
    let threads = reporting_threads(org);
    // A bound but unresolved pane is a boundary, not an unbound teammate.
    let mut panes: BTreeMap<_, _> = org
        .projects
        .iter()
        .flat_map(|project| &project.seats)
        .filter_map(|seat| Some((seat.pane.as_ref()?.0.as_str(), None)))
        .collect();
    panes.extend(
        unique_seats(org)
            .values()
            .filter_map(|seat| Some((seat.pane.as_ref()?.0.as_str(), threads.get(&seat.agent_id)))),
    );
    let originals: std::collections::HashMap<_, _> = sessions.iter().map(|s| (&s.id, s)).collect();
    let mut grouped = BTreeMap::<SeatId, Vec<SessionView>>::new();
    let mut unmatched = Vec::new();
    for session in sessions {
        let mut current = Some(session);
        let mut visited = std::collections::HashSet::new();
        let mut lead = None;
        while let Some(row) = current.filter(|row| visited.insert(&row.id)) {
            if let Some(indexed) = row.tmux_pane.as_deref().and_then(|pane| panes.get(pane)) {
                lead = *indexed;
                break;
            }
            current = row
                .parent_session_id
                .as_ref()
                .and_then(|id| originals.get(id).copied());
        }
        match lead {
            Some(lead) => grouped
                .entry(lead.clone())
                .or_default()
                .push(session.clone()),
            None => unmatched.push(session.clone()),
        }
    }
    grouped
        .into_iter()
        .map(|(lead, sessions)| {
            let mut children = agents(build_work_tree(&sessions, windows));
            children.sort_by(newest_agent_first);
            TreeNode::Team {
                name: lead.0,
                children,
            }
        })
        .chain(build_work_tree(&unmatched, windows))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use atm_core::{all_node_ids, flatten_tree, TreeRowKind};

    fn session(id: &str, pane: Option<&str>, parent: Option<&str>) -> SessionView {
        SessionView {
            id: id.into(),
            tmux_pane: pane.map(Into::into),
            parent_session_id: parent.map(Into::into),
            ..Default::default()
        }
    }

    fn seat(id: &str, pane: Option<&str>, manager: &str) -> WorkSeat {
        WorkSeat {
            agent_id: SeatId(id.into()),
            pane: pane.map(|p| PaneId(p.into())),
            manager: Some(SeatId(manager.into())),
            line_error: None,
            tasks: vec![],
        }
    }

    fn org(extra: Vec<WorkSeat>) -> WorkOrg {
        let mut seats = vec![
            seat("repo/lead", Some("%1"), "owner"),
            seat("repo/member", Some("%2"), "repo/lead"),
        ];
        seats.extend(extra);
        WorkOrg {
            seq: None,
            projects: vec![WorkProject {
                seats,
                tasks: vec![],
            }],
            needs_owner: WorkNeedsOwner { items: vec![] },
        }
    }

    fn window(id: &str, panes: &[&str]) -> WorkWindow {
        WorkWindow {
            id: WindowId(id.into()),
            name: id.into(),
            links: vec![],
            panes: panes.iter().map(|p| PaneId((*p).into())).collect(),
        }
    }

    fn group(tree: &[TreeNode], id: TreeNodeId) -> Vec<(SessionView, u8)> {
        let node = std::slice::from_ref(tree.iter().find(|n| n.node_id() == id).unwrap());
        flatten_tree(node, &all_node_ids(node))
            .into_iter()
            .filter_map(|row| match row.kind {
                TreeRowKind::Agent { session } => Some((session, row.depth)),
                _ => None,
            })
            .collect()
    }

    fn ids(rows: &[(SessionView, u8)]) -> Vec<&str> {
        let mut ids: Vec<_> = rows.iter().map(|(s, _)| s.id.as_str()).collect();
        ids.sort_unstable();
        ids
    }

    // Break caught: a non-seat pane stops the parent walk and splits a teammate out of its Team.
    #[test]
    fn work_threads_inherit_lead_through_unbound_panes() {
        let sessions = [
            session("leader", Some("%1"), None),
            session("member", Some("%2"), None),
            session("mate", Some("%3"), Some("leader")),
            session("leaf", None, Some("mate")),
            session("other", Some("%4"), None),
        ];
        for same_window in [true, false] {
            let windows = if same_window {
                [window("@1", &["%1", "%2", "%3"]), window("@2", &["%4"])]
            } else {
                [window("@1", &["%1", "%2"]), window("@2", &["%3", "%4"])]
            };
            let tree = build_work_threads(&sessions, &windows, &org(vec![]));
            let team = group(&tree, TreeNodeId::Team("repo/lead".into()));
            assert_eq!(ids(&team), ["leader", "leaf", "mate", "member"]);
            assert_eq!(
                ids(&group(&tree, TreeNodeId::Window("@2".into()))),
                ["other"]
            );
            assert_eq!(
                tree.iter().map(TreeNode::agent_count).sum::<usize>(),
                sessions.len()
            );

            let physical = build_work_tree(&sessions, &windows);
            let physical_id = TreeNodeId::Window(if same_window { "@1" } else { "@2" }.into());
            for rows in [team, group(&physical, physical_id)] {
                let (mate, depth) = rows.iter().find(|(s, _)| s.id.as_str() == "mate").unwrap();
                assert_eq!(mate.tmux_pane.as_deref(), Some("%3"));
                assert_eq!(mate.parent_session_id.as_ref().unwrap().as_str(), "leader");
                assert_eq!(*depth, if same_window { 2 } else { 1 });
            }
        }
    }

    // Break caught: an ambiguous seat borrows a parent lead, or an unbound cycle drops rows.
    #[test]
    fn work_threads_keep_unresolved_seats_and_parent_cycles_in_fallback() {
        let mut broken = seat("repo/broken", Some("%3"), "repo/lead");
        broken.line_error = Some("broken".into());
        let duplicate = vec![
            seat("repo/dup-a", Some("%3"), "repo/lead"),
            seat("repo/dup-b", Some("%3"), "repo/lead"),
        ];
        let other_team = vec![
            seat("other/lead", Some("%3"), "owner"),
            seat("other/worker", None, "other/lead"),
        ];
        let sessions = [
            session("leader", Some("%1"), None),
            session("member", Some("%2"), None),
            session("mate", Some("%3"), Some("leader")),
            session("cycle-a", Some("%4"), Some("cycle-b")),
            session("cycle-b", Some("%5"), Some("cycle-a")),
            session("orphan", Some("%6"), Some("missing")),
        ];
        let windows = [
            window("@1", &["%1", "%2"]),
            window("@2", &["%3", "%4", "%5", "%6"]),
        ];
        for (extra, matched) in [
            (vec![broken], false),
            (duplicate, false),
            (other_team, true),
        ] {
            let tree = build_work_threads(&sessions, &windows, &org(extra));
            assert_eq!(
                ids(&group(&tree, TreeNodeId::Team("repo/lead".into()))),
                ["leader", "member"]
            );
            let fallback = group(&tree, TreeNodeId::Window("@2".into()));
            if matched {
                assert_eq!(
                    ids(&group(&tree, TreeNodeId::Team("other/lead".into()))),
                    ["mate"]
                );
                assert_eq!(ids(&fallback), ["cycle-a", "cycle-b", "orphan"]);
            } else {
                assert_eq!(ids(&fallback), ["cycle-a", "cycle-b", "mate", "orphan"]);
            }
            assert_eq!(
                tree.iter().map(TreeNode::agent_count).sum::<usize>(),
                sessions.len()
            );
        }
    }
}
