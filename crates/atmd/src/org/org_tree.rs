//! Caller-independent, allowlisted organisation projection from supplied facts.

use super::{
    agents::{eligible_live_row, list_entry},
    reporting_line::ReportingEdges,
    tasks::is_open,
};
use atm_core::{SessionStatus, SessionView};
use atm_org::{
    Address, AgentId, AgentStatus, DecisionStatus, DeliveryStatus, OrgState, ProjectId, Task,
    TaskId, TaskKind, TaskState,
};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::BTreeSet;

#[derive(Debug, Clone, Serialize)]
pub(super) struct OrgTree {
    seq: u64,
    generated_at: DateTime<Utc>,
    reporting: Reporting,
    needs_owner: NeedsOwner,
    projects: Vec<Project>,
    unplaced_tasks: Vec<OpenTask>,
}
#[derive(Debug, Clone, Serialize)]
struct Reporting {
    state: &'static str,
    reason: Option<String>,
}
#[derive(Debug, Clone, Serialize)]
struct NeedsOwner {
    away_since: Option<DateTime<Utc>>,
    owner_unread: usize,
    unnamed_attention: usize,
    items: Vec<NeedItem>,
    items_total: usize,
}
#[derive(Debug, Clone, Serialize)]
struct NeedItem {
    kind: &'static str,
    id: String,
    agent_id: Option<AgentId>,
    project: Option<ProjectId>,
    since: Option<DateTime<Utc>>,
}
#[derive(Debug, Clone, Serialize)]
struct Project {
    id: ProjectId,
    root: String,
    seats: Vec<Seat>,
    tasks_open: usize,
    tasks: Vec<OpenTask>,
    decisions: DecisionCounts,
}
#[derive(Debug, Clone, Serialize)]
struct Seat {
    agent_id: AgentId,
    state: AgentStatus,
    activity: Option<SessionStatus>,
    harness: Option<String>,
    model: Option<String>,
    context_percentage: Option<f64>,
    pane: Option<String>,
    manager: Option<Address>,
    line_error: Option<String>,
    tasks: Vec<TaskId>,
}
#[derive(Debug, Clone, Serialize)]
struct OpenTask {
    task_id: TaskId,
    title: String,
    kind: TaskKind,
    state: TaskState,
    agent_id: Option<AgentId>,
    creator: Address,
}
impl From<&Task> for OpenTask {
    fn from(task: &Task) -> Self {
        Self {
            task_id: task.id.clone(),
            title: task.title.clone(),
            kind: task.kind,
            state: task.state,
            agent_id: task.agent_id.clone(),
            creator: task.creator.clone(),
        }
    }
}
#[derive(Debug, Clone, Default, Serialize)]
struct DecisionCounts {
    pending: usize,
    provisional: usize,
}

fn task_project(task: &Task) -> Option<&ProjectId> {
    task.project(task.agent_id.as_ref())
}
fn need(
    kind: &'static str,
    id: String,
    agent: Option<&AgentId>,
    since: Option<DateTime<Utc>>,
) -> NeedItem {
    NeedItem {
        kind,
        id,
        agent_id: agent.cloned(),
        project: agent.map(|id| id.project.clone()),
        since,
    }
}
fn named(address: &Address) -> Option<&AgentId> {
    match address {
        Address::Agent(id) => Some(id),
        _ => None,
    }
}

/// No I/O or caller identity: all facts (including the cursor and clock) are inputs.
#[must_use]
pub(super) fn project(
    state: &OrgState,
    seq: u64,
    rows: &[SessionView],
    edges: &ReportingEdges,
    now: DateTime<Utc>,
) -> OrgTree {
    let mut items = Vec::new();
    let open: Vec<_> = state.tasks().filter(|task| is_open(task)).collect();
    for decision in state.decisions() {
        let (kind, since) = match decision.status {
            DecisionStatus::Pending => ("decision_pending", Some(decision.requested_at)),
            DecisionStatus::TakenProvisionally => ("decision_provisional", decision.taken_at),
            _ => continue,
        };
        items.push(need(
            kind,
            decision.id.to_string(),
            named(&decision.request.requester),
            since,
        ));
    }
    for message in state
        .messages()
        .filter(|m| m.status == DeliveryStatus::Failed)
    {
        items.push(need(
            "delivery_failed",
            message.id.to_string(),
            named(&message.content.to),
            message.failed_at,
        ));
    }
    let projects = state
        .projects()
        .map(|p| {
            let seats = state
                .agents()
                .filter(|a| a.id.project == p.id)
                .map(|agent| {
                    let entry = list_entry(state, agent, rows);
                    let live = eligible_live_row(state, agent, rows);
                    if agent.status == AgentStatus::Stuck {
                        items.push(need(
                            "seat_stuck",
                            agent.id.to_string(),
                            Some(&agent.id),
                            Some(agent.updated_at),
                        ));
                    }
                    if let Some(row) =
                        live.filter(|row| row.status == SessionStatus::AttentionNeeded)
                    {
                        // This is the observation time, not a proven attention onset.
                        let since = DateTime::parse_from_rfc3339(&row.last_activity)
                            .ok()
                            .map(|time| time.with_timezone(&Utc));
                        items.push(need(
                            "seat_attention",
                            agent.id.to_string(),
                            Some(&agent.id),
                            since,
                        ));
                    }
                    let (manager, line_error) = match edges
                        .edges
                        .get(&agent.id)
                        .filter(|_| edges.reason.is_none())
                    {
                        Some(Ok(manager)) => (Some(manager.clone()), None),
                        Some(Err(error)) => {
                            items.push(need(
                                "line_broken",
                                agent.id.to_string(),
                                Some(&agent.id),
                                None,
                            ));
                            (None, Some(error.clone()))
                        }
                        None => (None, None),
                    };
                    Seat {
                        agent_id: entry.agent_id,
                        state: entry.state,
                        activity: entry.activity,
                        harness: entry.harness,
                        model: entry.model,
                        context_percentage: live.map(|r| r.context_percentage),
                        pane: entry.pane,
                        manager,
                        line_error,
                        tasks: open
                            .iter()
                            .filter(|task| task.agent_id.as_ref() == Some(&agent.id))
                            .map(|task| task.id.clone())
                            .collect(),
                    }
                })
                .collect();
            let tasks: Vec<_> = open
                .iter()
                .copied()
                .filter(|task| task_project(task) == Some(&p.id))
                .collect();
            let mut decisions = DecisionCounts::default();
            for decision in state
                .decisions()
                .filter(|d| named(&d.request.requester).is_some_and(|id| id.project == p.id))
            {
                match decision.status {
                    DecisionStatus::Pending => decisions.pending += 1,
                    DecisionStatus::TakenProvisionally => decisions.provisional += 1,
                    _ => {}
                }
            }
            Project {
                id: p.id.clone(),
                root: p.root.clone(),
                seats,
                tasks_open: tasks.len(),
                tasks: tasks.into_iter().take(50).map(OpenTask::from).collect(),
                decisions,
            }
        })
        .collect();
    // Each source contributes once per (kind,id); notices are not sources.
    items.sort_by(|a, b| {
        a.since
            .is_none()
            .cmp(&b.since.is_none())
            .then(a.since.cmp(&b.since))
            .then(a.id.cmp(&b.id))
            .then(a.kind.cmp(b.kind))
    });
    let items_total = items.len();
    items.truncate(100);
    let panes: BTreeSet<_> = state
        .agents()
        .filter_map(|agent| agent.pane.as_deref())
        .collect();
    OrgTree {
        seq,
        generated_at: now,
        reporting: Reporting {
            state: if edges.reason.is_some() {
                "unavailable"
            } else {
                "ok"
            },
            reason: edges.reason.clone(),
        },
        needs_owner: NeedsOwner {
            away_since: state.owner().away_since,
            owner_unread: state
                .messages()
                .filter(|m| m.content.to == Address::Owner && m.read_at.is_none())
                .count(),
            unnamed_attention: rows
                .iter()
                .filter(|row| {
                    row.parent_session_id.is_none()
                        && row.status == SessionStatus::AttentionNeeded
                        && row
                            .tmux_pane
                            .as_deref()
                            .is_none_or(|pane| !panes.contains(pane))
                })
                .count(),
            items,
            items_total,
        },
        projects,
        unplaced_tasks: open
            .into_iter()
            .filter(|task| task_project(task).is_none())
            .map(OpenTask::from)
            .collect(),
    }
}

#[cfg(test)]
#[path = "org_tree_tests.rs"]
mod tests;
