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

use crate::{Address, MessageContent, MessageKind, Refusal, Task, TaskBudget, TaskCost, TaskState};
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
    })
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
