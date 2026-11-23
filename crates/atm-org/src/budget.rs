//! Task budget policy: report limits without stopping or changing work.
//!
//! Usage is supplied by the caller from the single task cost ledger. Missing
//! metrics stay unknown; cost and turn limits cross strictly above the limit,
//! while deadlines are inclusive and stop at terminal tasks' `changed_at`.
//!
//! [`budget_note`] emits a canonical v1 message. Its full content is the durable
//! once-per-task identity: changing the text requires recognizing the old copy
//! too. No title, usage, limit or time is embedded, so these cannot change that
//! identity. The ordinary `msg.sent` record is the latch, not delivery or read.

use crate::{
    Address, MessageContent, MessageId, MessageKind, OrgState, Refusal, Task, TaskBudget, TaskCost,
    TaskId, TaskState,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetKind {
    Cost,
    Turns,
    Deadline,
}

/// Crossed limits, in cost/turn/deadline order, without changing the task.
#[must_use]
pub fn budget_crossings(task: &Task, usage: &TaskCost, now: DateTime<Utc>) -> Vec<BudgetKind> {
    let mut crossed = Vec::new();
    let Some(budget) = &task.budget else {
        return crossed;
    };
    if task.state == TaskState::Unknown {
        return crossed;
    }
    if budget
        .cost
        .zip(usage.cost)
        .is_some_and(|(limit, used)| used > limit)
    {
        crossed.push(BudgetKind::Cost);
    }
    if budget
        .turns
        .zip(usage.turns)
        .is_some_and(|(limit, used)| used > limit)
    {
        crossed.push(BudgetKind::Turns);
    }
    let until = if matches!(task.state, TaskState::Accepted | TaskState::Cancelled) {
        task.changed_at
    } else {
        now
    };
    if budget.deadline.is_some_and(|deadline| until >= deadline) {
        crossed.push(BudgetKind::Deadline);
    }
    crossed
}

/// Stable notification content; the caller decides whether a limit crossed.
#[must_use]
pub fn budget_note(task: &Task) -> Option<MessageContent> {
    let budget = task.budget.as_ref()?;
    if task.state == TaskState::Unknown
        || (budget.cost.is_none() && budget.turns.is_none() && budget.deadline.is_none())
    {
        return None;
    }
    Some(MessageContent {
        delivery_notice: None,
        from: Address::System,
        to: if task.creator == Address::System {
            Address::Owner
        } else {
            task.creator.clone()
        },
        kind: MessageKind::Note,
        body: format!(
            "Task {} crossed a budget limit.\nInspect limits and current usage: atm task get {}\nStage 1 only flags budgets; nothing was stopped.",
            task.id, task.id
        ),
        attachments: Vec::new(),
        task: Some(task.id.clone()),
        post: None,
    })
}

/// Current crossings and the durable notification, independent of delivery/read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BudgetStatus {
    pub crossed: Vec<BudgetKind>,
    pub notification: Option<MessageId>,
}

/// Notes to enqueue and the earliest remaining task deadline.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BudgetStep {
    pub notes: Vec<MessageContent>,
    pub wake_at: Option<DateTime<Utc>>,
}

fn notification(state: &OrgState, note: &MessageContent) -> Option<MessageId> {
    state
        .messages()
        .find(|message| message.content == *note)
        .map(|message| message.id)
}

/// Evaluate supplied ledger usage; no/empty budgets and unknown states are absent.
#[must_use]
pub fn budget_status(
    state: &OrgState,
    task: &Task,
    usage: &TaskCost,
    now: DateTime<Utc>,
) -> Option<BudgetStatus> {
    let note = budget_note(task)?;
    Some(BudgetStatus {
        crossed: budget_crossings(task, usage, now),
        notification: notification(state, &note),
    })
}

/// Evaluate one committed snapshot using its task-ledger read interface.
/// Returning None from `usage` means unavailable metrics, not zero usage.
#[must_use]
pub fn budget_step(
    state: &OrgState,
    now: DateTime<Utc>,
    usage: impl Fn(&TaskId) -> Option<TaskCost>,
) -> BudgetStep {
    let mut step = BudgetStep::default();
    for task in state.tasks() {
        let Some(note) = budget_note(task) else {
            continue;
        };
        if notification(state, &note).is_some() {
            continue;
        }
        if !budget_crossings(task, &usage(&task.id).unwrap_or_default(), now).is_empty() {
            step.notes.push(note);
        } else if !matches!(task.state, TaskState::Accepted | TaskState::Cancelled) {
            if let Some(deadline) = task.budget.as_ref().and_then(|budget| budget.deadline) {
                step.wake_at = Some(step.wake_at.map_or(deadline, |wake| wake.min(deadline)));
            }
        }
    }
    step
}

/// Reject invalid newly requested limits, without restricting replay of old facts.
pub fn validate_budget(budget: Option<&TaskBudget>) -> Result<(), Refusal> {
    if budget
        .and_then(|budget| budget.cost)
        .is_some_and(|cost| cost.as_microdollars() < 0)
    {
        return Err(Refusal::InvalidValue {
            field: "budget.cost",
            reason: "must not be negative".into(),
        });
    }
    Ok(())
}
