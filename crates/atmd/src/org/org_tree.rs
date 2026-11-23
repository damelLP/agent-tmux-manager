//! Caller-independent, allowlisted organisation projection from supplied facts.

use super::{
    agents::{eligible_live_row, list_entry},
    department_directory::DepartmentDirectory,
    reporting_line::ReportingEdges,
    tasks::is_open,
};
use atm_core::{SessionStatus, SessionView};
use atm_org::{
    department::{self, DepartmentId, HireMode, RosterEntry},
    Address, AgentId, AgentStatus, DecisionStatus, DeliveryStatus, OrgState, ProjectId, Task,
    TaskId, TaskKind, TaskState,
};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::{
    cell::OnceCell,
    collections::{BTreeMap, BTreeSet},
    num::NonZeroU64,
};

#[derive(Debug, Clone, Serialize)]
pub(super) struct OrgTree {
    seq: u64,
    generated_at: DateTime<Utc>,
    reporting: Reporting,
    needs_owner: NeedsOwner,
    projects: Vec<Project>,
    unplaced_tasks: Vec<OpenTask>,
    #[serde(flatten)]
    department_section: Option<DepartmentSection>,
}
#[derive(Debug, Clone, Serialize)]
struct DepartmentSection {
    departments: Vec<DepartmentView>,
    department_problems: Vec<String>,
}
#[derive(Debug, Clone, Serialize)]
struct DepartmentView {
    id: DepartmentId,
    title: String,
    goal: Option<String>,
    head: Option<AgentId>,
    projects: Vec<ProjectId>,
    roster: Vec<RosterUsage>,
    hire_mode: HireMode,
    problems: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    would_wait: Option<Vec<WouldWait>>,
}
#[derive(Debug, Clone, Serialize)]
struct WouldWait {
    seat: AgentId,
    reason: String,
}
#[derive(Debug, Clone, Serialize)]
struct RosterUsage {
    seats: String,
    max: Option<NonZeroU64>,
    live: usize,
}

fn department_section(
    state: &OrgState,
    directory: &DepartmentDirectory,
    current_models: &BTreeMap<AgentId, String>,
) -> Option<DepartmentSection> {
    if directory.departments.is_empty() && directory.problems.is_empty() {
        return None;
    }
    let registered = state.projects().map(|project| project.id.clone()).collect();
    let mut problems: BTreeMap<_, Vec<_>> = BTreeMap::new();
    for problem in department::validate(&directory.departments, &registered) {
        let id = problem.department_id();
        problems
            .entry(id.clone())
            .or_default()
            .push(format!("{id}.toml:{problem}"));
    }
    let live = OnceCell::new();
    let count = |entry: &RosterEntry| live.get_or_init(|| department::live_counts(state))(entry);
    let departments = directory
        .departments
        .iter()
        .map(|dept| DepartmentView {
            id: dept.id.clone(),
            title: dept.title.clone(),
            goal: dept.goal.clone(),
            head: dept.head.clone(),
            projects: dept.projects.clone(),
            roster: dept
                .roster
                .iter()
                .map(|entry| RosterUsage {
                    seats: entry.seats.to_string(),
                    max: entry.max,
                    live: count(entry),
                })
                .collect(),
            hire_mode: dept.hire_mode,
            problems: problems.remove(&dept.id).unwrap_or_default(),
            would_wait: (dept.hire_mode == HireMode::Report).then(|| {
                state
                    .agents()
                    .filter(|agent| {
                        agent.status != AgentStatus::Down
                            && dept.projects.contains(&agent.id.project)
                    })
                    .filter_map(|agent| {
                        let model = current_models
                            .get(&agent.id)
                            .map(String::as_str)
                            .or_else(|| agent.role.as_ref().map(department::role_model));
                        let reason =
                            department::start_refusal(dept, &agent.id, true, model, &count)?;
                        Some(WouldWait {
                            seat: agent.id.clone(),
                            reason: reason.to_string(),
                        })
                    })
                    .collect()
            }),
        })
        .collect();
    Some(DepartmentSection {
        departments,
        department_problems: directory.problems.clone(),
    })
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
    directory: &DepartmentDirectory,
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
        department_section: department_section(state, directory, &edges.current_models),
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
