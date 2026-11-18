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
    let panes: BTreeMap<_, _> = unique_seats(org)
        .values()
        .filter_map(|seat| Some((seat.pane.as_ref()?.0.as_str(), threads.get(&seat.agent_id)?)))
        .collect();
    let originals: std::collections::HashMap<_, _> = sessions.iter().map(|s| (&s.id, s)).collect();
    let mut grouped = BTreeMap::<SeatId, Vec<SessionView>>::new();
    let mut unmatched = Vec::new();
    for session in sessions {
        let mut current = Some(session);
        let mut visited = std::collections::HashSet::new();
        let mut lead = None;
        while let Some(row) = current.filter(|row| visited.insert(&row.id)) {
            if let Some(pane) = &row.tmux_pane {
                lead = panes.get(pane.as_str()).copied();
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
