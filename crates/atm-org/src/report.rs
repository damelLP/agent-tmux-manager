//! Pure evaluation reports. Usage totals come only from OrgState::task_cost;
//! supplemental replay metadata never changes or reconstructs that ledger.
mod fold;
mod trend;
mod types;
use crate::{
    department::Department, Address, DecisionStatus, OrgState, ProjectId, Task, TaskCost, TaskState,
};
use atm_core::Money;
use chrono::{DateTime, Utc};
pub(crate) use fold::ReportLedger;
use std::collections::{BTreeMap, BTreeSet};
pub use types::*;

#[derive(Debug, Clone, thiserror::Error)]
pub enum ReportError {
    #[error("invalid report parameters: {0}")]
    InvalidParams(String),
    #[error("{0}")]
    NotFound(String),
    #[error("report projection failed: {0}")]
    Internal(String),
}
fn terminal(state: TaskState) -> bool {
    matches!(state, TaskState::Accepted | TaskState::Cancelled)
}
fn add<T: Copy>(sum: &mut Option<T>, value: Option<T>, plus: impl FnOnce(T, T) -> T) {
    if let Some(value) = value {
        *sum = Some(sum.map_or(value, |prior| plus(prior, value)));
    }
}
fn add_over(sum: &mut Option<OverBudget>, value: &Option<OverBudget>) {
    if let Some(value) = value {
        let sum = sum.get_or_insert_with(OverBudget::default);
        add(&mut sum.turns, value.turns, u64::saturating_add);
        add(&mut sum.cost, value.cost, |a, b| a + b);
        add(
            &mut sum.deadline_seconds,
            value.deadline_seconds,
            u64::saturating_add,
        );
    }
}
fn over_budget(task: &Task, usage: &TaskCost, now: DateTime<Utc>) -> Option<OverBudget> {
    let budget = task.budget.as_ref()?;
    if budget.turns.is_none() && budget.cost.is_none() && budget.deadline.is_none() {
        return None;
    }
    Some(OverBudget {
        turns: budget
            .turns
            .zip(usage.turns)
            .map(|(limit, used)| u64::from(used.saturating_sub(limit))),
        cost: budget.cost.zip(usage.cost).map(|(limit, used)| {
            Money::from_microdollars(
                used.as_microdollars()
                    .saturating_sub(limit.as_microdollars())
                    .max(0),
            )
        }),
        deadline_seconds: budget.deadline.map(|deadline| {
            u64::try_from(
                if terminal(task.state) {
                    task.changed_at
                } else {
                    now
                }
                .signed_duration_since(deadline)
                .num_seconds(),
            )
            .unwrap_or(0)
        }),
    })
}
fn task_report(
    state: &OrgState,
    task: &Task,
    now: DateTime<Utc>,
) -> Result<TaskReport, ReportError> {
    let empty = fold::TaskFacts::default();
    let facts = state.reports.tasks.get(&task.id).unwrap_or(&empty);
    let agent_id = task.agent_id.clone().or_else(|| facts.agent.clone());
    let project_id = task.project(agent_id.as_ref()).cloned();
    let usage = state
        .task_cost(&task.id, now)
        .ok_or_else(|| ReportError::Internal(format!("known task {} has no ledger", task.id)))?;
    let mut steps = facts.steps.clone();
    for (id, decision) in &state.reports.decisions {
        if decision.task.as_ref() == Some(&task.id) || task.rests_on.as_ref() == Some(id) {
            steps.answer = steps.answer.saturating_add(decision.answers);
            // One owner event can match both answer and redo; count it once
            // in total while preserving both kind-specific measurements.
            let overlap = decision.redone.get(&task.id).copied().unwrap_or(0);
            steps.total = steps
                .total
                .saturating_add(decision.answers.saturating_sub(overlap));
        }
    }
    let mut harnesses = BTreeSet::new();
    let mut models = BTreeSet::new();
    for summary in facts
        .sessions
        .iter()
        .filter_map(|s| state.session_summary(s))
    {
        harnesses.insert(summary.harness.short_tag().to_owned());
        if let Some(model) = &summary.model {
            models.insert(model.clone());
        }
    }
    let cost_source = CostSource {
        label: if usage.cost.is_some() {
            CostLabel::HarnessReported
        } else {
            CostLabel::Unknown
        },
        harnesses: harnesses.into_iter().collect(),
        models: models.into_iter().collect(),
    };
    let over_budget = over_budget(task, &usage, now);
    Ok(TaskReport {
        task_id: task.id.clone(),
        title: task.title.clone(),
        kind: task.kind,
        measure: task.measure.clone(),
        project_id,
        agent_id,
        state: task.state,
        outcome: match task.state {
            TaskState::Accepted => Outcome::Accepted,
            TaskState::Cancelled => Outcome::Cancelled,
            TaskState::Unknown => Outcome::Unknown,
            _ => Outcome::Open,
        },
        created_at: task.created_at,
        started_at: facts.started,
        ended_at: terminal(task.state).then_some(task.changed_at),
        usage,
        submissions: facts.submissions,
        reminders: facts.reminders,
        flags: facts.flags,
        redone: steps.redo,
        owner_step_ins: steps,
        context_percent_at_submit: facts.submit_context.flatten(),
        cost_source,
        over_budget,
    })
}
fn project<'a>(
    projects: &'a mut BTreeMap<Option<ProjectId>, ProjectReport>,
    id: &Option<ProjectId>,
    bucket: u16,
) -> &'a mut ProjectReport {
    projects.entry(id.clone()).or_insert_with(|| ProjectReport {
        project_id: id.clone(),
        tasks: OutcomeCounts::default(),
        owner_step_ins: OwnerStepIns::default(),
        tasks_with_step_ins: 0,
        cost: None,
        cost_unknown_tasks: 0,
        over_budget: None,
        decisions: DecisionCounts::default(),
        trend: trend::trend(std::iter::empty(), bucket),
    })
}

/// Sum existing project rows in each department independently of task paging.
#[must_use]
pub fn department_costs(
    projects: &[ProjectReport],
    departments: &[Department],
) -> Vec<DepartmentCostReport> {
    if departments.is_empty() {
        return Vec::new();
    }
    let by_project: BTreeMap<_, _> = projects
        .iter()
        .filter_map(|project| project.project_id.as_ref().map(|id| (id, project)))
        .collect();
    departments
        .iter()
        .map(|department| {
            let mut row = DepartmentCostReport {
                id: department.id.clone(),
                title: department.title.clone(),
                cost: None,
                cost_unknown_tasks: 0,
            };
            for project in department
                .projects
                .iter()
                .filter_map(|id| by_project.get(id))
            {
                add(&mut row.cost, project.cost, |a, b| a + b);
                row.cost_unknown_tasks = row
                    .cost_unknown_tasks
                    .saturating_add(project.cost_unknown_tasks);
            }
            row
        })
        .collect()
}

/// Build a read-only whole-scope projection, then page its tasks. now is supplied
/// by the boundary; all metrics remain whole-task totals, not window-clipped.
pub fn report(
    state: &OrgState,
    params: &ReportParams,
    visible: &ReportVisibility,
    now: DateTime<Utc>,
) -> Result<Report, ReportError> {
    if !(1..=1000).contains(&params.bucket) || !(1..=1000).contains(&params.limit) {
        return Err(ReportError::InvalidParams(
            "bucket and limit must be between 1 and 1000".into(),
        ));
    }
    let until = params.until.unwrap_or(now);
    if params.since.is_some_and(|since| since > until) {
        return Err(ReportError::InvalidParams(
            "since must not be after until".into(),
        ));
    }
    if let Some(id) = &params.project_id {
        if state.project(id).is_none() {
            return Err(ReportError::NotFound(format!("unknown project {id}")));
        }
    }
    let after = params
        .after
        .as_ref()
        .map(|id| {
            state
                .tasks
                .iter()
                .find(|(_, t)| &t.id == id)
                .map(|(seq, _)| *seq)
                .ok_or_else(|| ReportError::NotFound(format!("unknown task cursor {id}")))
        })
        .transpose()?;
    let mut rows = Vec::new();
    for (seq, task) in &state.tasks {
        let facts = state.reports.tasks.get(&task.id);
        let allowed = visible.address(&task.creator)
            || task
                .agent_id
                .as_ref()
                .or_else(|| facts.and_then(|f| f.agent.as_ref()))
                .is_some_and(|id| visible.contains(id));
        if !allowed
            || task.created_at >= until
            || (terminal(task.state) && params.since.is_some_and(|since| task.changed_at < since))
        {
            continue;
        }
        let row = task_report(state, task, now)?;
        if params
            .project_id
            .as_ref()
            .is_none_or(|id| row.project_id.as_ref() == Some(id))
        {
            rows.push((*seq, row));
        }
    }
    let mut projects = BTreeMap::new();
    for (_, row) in &rows {
        let p = project(&mut projects, &row.project_id, params.bucket);
        let count = match row.outcome {
            Outcome::Accepted => &mut p.tasks.accepted,
            Outcome::Cancelled => &mut p.tasks.cancelled,
            Outcome::Open => &mut p.tasks.open,
            Outcome::Unknown => &mut p.tasks.unknown,
        };
        *count = count.saturating_add(1);
        p.owner_step_ins.add(&row.owner_step_ins);
        p.tasks_with_step_ins = p
            .tasks_with_step_ins
            .saturating_add(u64::from(row.owner_step_ins.total > 0));
        if let Some(cost) = row.usage.cost {
            add(&mut p.cost, Some(cost), |a, b| a + b);
        } else {
            p.cost_unknown_tasks = p.cost_unknown_tasks.saturating_add(1);
        }
        add_over(&mut p.over_budget, &row.over_budget);
    }
    for decision in state.decisions().filter(|d| {
        visible.address(&d.request.requester)
            && d.requested_at < until
            && params.since.is_none_or(|since| d.requested_at >= since)
    }) {
        let id = match &decision.request.requester {
            Address::Agent(a) => Some(a.project.clone()),
            _ => None,
        };
        if params
            .project_id
            .as_ref()
            .is_some_and(|wanted| id.as_ref() != Some(wanted))
        {
            continue;
        }
        let d = &mut project(&mut projects, &id, params.bucket).decisions;
        if let Some(choice) = &decision.provisional {
            if choice.relayed_by.is_some() {
                d.relayed = d.relayed.saturating_add(1);
            } else if choice.by == Address::System {
                d.taken_for_owner = d.taken_for_owner.saturating_add(1);
            } else if choice.by == decision.request.requester {
                d.agent_decided = d.agent_decided.saturating_add(1);
            }
        }
        if decision.status == DecisionStatus::Overruled {
            d.overruled = d.overruled.saturating_add(1);
        }
        if let Some(facts) = state.reports.decisions.get(&decision.id) {
            d.tasks_redone = facts
                .redone
                .values()
                .fold(d.tasks_redone, |sum, n| sum.saturating_add(*n));
        }
    }
    for p in projects.values_mut() {
        p.trend = trend::trend(
            rows.iter()
                .map(|(_, t)| t)
                .filter(|t| t.project_id == p.project_id),
            params.bucket,
        );
    }
    let mut wanted = rows
        .into_iter()
        .filter(|(seq, _)| after.is_none_or(|after| *seq > after))
        .map(|(_, t)| t);
    let tasks: Vec<_> = wanted.by_ref().take(usize::from(params.limit)).collect();
    let next_after = wanted
        .next()
        .and_then(|_| tasks.last().map(|t| t.task_id.clone()));
    let (reader, sees) = match visible {
        ReportVisibility::All => (Address::Owner, ReportSees::All),
        ReportVisibility::Reports { reader, .. } => {
            (Address::Agent(reader.clone()), ReportSees::Reports)
        }
    };
    Ok(Report {
        now,
        scope: ReportScope {
            project_id: params.project_id.clone(),
            since: params.since,
            until,
            reader,
            sees,
        },
        tasks,
        projects: projects.into_values().collect(),
        next_after,
    })
}
