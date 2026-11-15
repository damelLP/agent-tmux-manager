//! Observed task usage, not billing attribution. One parent prompt is one turn;
//! cumulative prices are differenced against a per-session high-water mark.
//! The first price is a baseline. Unattributed observations still advance it.
//! Time/reviews/rework replay from existing task events; the additive
//! `task.usage_observed` retains otherwise ephemeral usage, without prompt text.
//! Old readers skip it and still understand the unchanged accepted TaskCost.

use crate::{
    AgentStatus, ApplyError, OrgEvent, OrgState, Refusal, Task, TaskCost, TaskId, TaskState,
    TaskVerdict,
};
use atm_core::{LifecycleEvent, Money, SessionId};
use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

/// Daemon input, after proving the original parent/session/pane at the boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskUsageObservation {
    pub session_id: SessionId,
    pub pane: Option<String>,
    pub turn: bool,
    pub cumulative_cost: Option<Money>,
    pub context_percent: Option<u8>,
}
impl TaskUsageObservation {
    /// Only explicit current context and denominator count; no model defaults,
    /// cumulative-token fallback, or inferred cost. Invalid metrics are absent.
    #[must_use]
    pub fn from_lifecycle(
        session_id: SessionId,
        pane: Option<String>,
        event: &LifecycleEvent,
    ) -> Option<Self> {
        let (turn, cumulative_cost, context_percent) = match event {
            LifecycleEvent::PromptSubmit { .. } => (true, None, None),
            LifecycleEvent::ContextUpdate {
                current_tokens,
                context_window_size,
                cost_usd,
                ..
            } => {
                let cost = cost_usd
                    .filter(|v| v.is_finite() && *v >= 0.0)
                    .map(Money::from_usd);
                let percent = current_tokens
                    .zip(context_window_size.filter(|size| *size > 0))
                    .map(|(tokens, window)| {
                        ((tokens as f64 / f64::from(window) * 100.0)
                            .ceil()
                            .min(100.0)) as u8
                    });
                (false, cost, percent)
            }
            _ => return None,
        };
        (turn || cumulative_cost.is_some() || context_percent.is_some()).then_some(Self {
            session_id,
            pane,
            turn,
            cumulative_cost,
            context_percent,
        })
    }
    pub(crate) fn event(&self, task_id: Option<TaskId>) -> OrgEvent {
        OrgEvent::TaskUsageObserved(TaskUsageObserved {
            session_id: self.session_id.clone(),
            task_id,
            turn: self.turn,
            cumulative_cost: self.cumulative_cost,
            context_percent: self.context_percent,
        })
    }
}

/// Attribution is frozen by decide, never recomputed against later bindings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskUsageObserved {
    pub session_id: SessionId,
    pub task_id: Option<TaskId>,
    pub turn: bool,
    #[serde(default, with = "crate::event::optional_money")]
    pub cumulative_cost: Option<Money>,
    pub context_percent: Option<u8>,
}
impl TaskUsageObserved {
    pub(crate) fn check(&self, state: &OrgState) -> Result<(), ApplyError> {
        crate::state::valid_external_id("session id", self.session_id.as_str())?;
        if let Some(id) = &self.task_id {
            state.require_task(id)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CostLedger {
    sessions: HashMap<SessionId, Money>,
    tasks: BTreeMap<TaskId, Accumulator>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
struct Accumulator {
    record: TaskCost,
    durations: BTreeMap<TaskState, TimeDelta>,
}
impl Default for Accumulator {
    fn default() -> Self {
        Self {
            record: TaskCost {
                turns: Some(0),
                review_rounds: Some(0),
                changes_requested: Some(0),
                ..TaskCost::default()
            },
            durations: BTreeMap::new(),
        }
    }
}
fn terminal(task: &Task) -> bool {
    matches!(task.state, TaskState::Accepted | TaskState::Cancelled)
}
fn elapsed(from: DateTime<Utc>, to: DateTime<Utc>) -> TimeDelta {
    to.signed_duration_since(from).max(TimeDelta::zero())
}
fn add_time(durations: &mut BTreeMap<TaskState, TimeDelta>, state: TaskState, delta: TimeDelta) {
    let value = durations.entry(state).or_default();
    *value = value.checked_add(&delta).unwrap_or(TimeDelta::MAX);
}
fn increment(value: &mut Option<u32>) {
    *value = Some(value.unwrap_or(0).saturating_add(1));
}

impl OrgState {
    /// One definition of running totals for task.get, acceptance and budgets.
    /// Counts only observed prompts/spend; missing reports cannot be recovered.
    /// State wall time includes daemon downtime. Reads never change the ledger.
    #[must_use]
    pub fn task_cost(&self, task_id: &TaskId, now: DateTime<Utc>) -> Option<TaskCost> {
        let task = self.task(task_id)?;
        if task.state == TaskState::Accepted {
            if let Some(record) = &task.cost {
                return Some(record.clone());
            }
        }
        let mut accumulated = self.costs.tasks.get(task_id).cloned().unwrap_or_default();
        if !terminal(task) {
            add_time(
                &mut accumulated.durations,
                task.state,
                elapsed(task.changed_at, now),
            );
        }
        accumulated.record.seconds_in_state = accumulated
            .durations
            .into_iter()
            .map(|(state, duration)| (state, u64::try_from(duration.num_seconds()).unwrap_or(0)))
            .collect();
        Some(accumulated.record)
    }

    pub(crate) fn decide_usage(
        &self,
        input: &TaskUsageObservation,
    ) -> Result<Vec<OrgEvent>, Refusal> {
        if input
            .cumulative_cost
            .is_some_and(|v| v.as_microdollars() < 0)
            || input.context_percent.is_some_and(|v| v > 100)
        {
            return Err(Refusal::InvalidValue {
                field: "task usage",
                reason: "negative cost or context outside 0..100".into(),
            });
        }
        if input.session_id.is_pending()
            || (!input.turn && input.cumulative_cost.is_none() && input.context_percent.is_none())
        {
            return Ok(vec![]);
        }
        let agent = input
            .pane
            .as_deref()
            .and_then(|pane| self.agent_for_pane(pane))
            .filter(|a| {
                !matches!(a.status, AgentStatus::Clearing | AgentStatus::Down)
                    && a.session_id.as_ref() == Some(&input.session_id)
                    && a.excluded_session_id.as_ref() != Some(&input.session_id)
            });
        let mut candidates = self.tasks().filter(|task| {
            agent.is_some_and(|a| task.agent_id.as_ref() == Some(&a.id))
                && !terminal(task)
                && task.state != TaskState::Unknown
        });
        let first = candidates.next();
        let target = if candidates.next().is_none() {
            first.map(|task| task.id.clone())
        } else {
            None
        };
        Ok(vec![input.event(target)])
    }

    /// Before a task fold resets changed_at/submission/review_task. References
    /// have been validated; this hook cannot fail or change task policy.
    pub(crate) fn track_cost_transition(&mut self, event: &OrgEvent, now: DateTime<Utc>) {
        let id = match event {
            OrgEvent::TaskAssigned(v) => &v.task_id,
            OrgEvent::TaskStateChanged(v) => &v.task_id,
            OrgEvent::TaskSubmitted(v) => &v.task_id,
            OrgEvent::TaskReviewStarted(v) => &v.task_id,
            OrgEvent::TaskVerdict(v) => &v.task_id,
            _ => return,
        };
        let Some(task) = self.task(id) else {
            return;
        };
        if terminal(task) {
            return;
        }
        let state = task.state;
        let delta = elapsed(task.changed_at, now);
        let review = match event {
            OrgEvent::TaskReviewStarted(_) => true,
            OrgEvent::TaskVerdict(v) => {
                v.review_task.is_some() && v.review_task != task.review_task
            }
            _ => false,
        };
        let rework =
            matches!(event, OrgEvent::TaskVerdict(v) if v.verdict == TaskVerdict::ChangesRequested);
        let entry = self.costs.tasks.entry(id.clone()).or_default();
        add_time(&mut entry.durations, state, delta);
        if review {
            increment(&mut entry.record.review_rounds);
        }
        if rework {
            increment(&mut entry.record.changes_requested);
        }
    }

    pub(crate) fn apply_usage(&mut self, sample: &TaskUsageObserved) {
        let delta = sample
            .cumulative_cost
            .filter(|v| v.as_microdollars() >= 0)
            .map(|current| {
                let prior = self
                    .costs
                    .sessions
                    .entry(sample.session_id.clone())
                    .or_insert(current);
                let increase = current
                    .as_microdollars()
                    .saturating_sub(prior.as_microdollars())
                    .max(0);
                *prior = (*prior).max(current);
                Money::from_microdollars(increase)
            });
        let Some(id) = sample.task_id.as_ref().filter(|id| {
            self.task(id)
                .is_some_and(|t| !terminal(t) && t.state != TaskState::Unknown)
        }) else {
            return;
        };
        let entry = self.costs.tasks.entry(id.clone()).or_default();
        if sample.turn {
            increment(&mut entry.record.turns);
        }
        if let Some(delta) = delta {
            entry.record.cost = Some(entry.record.cost.unwrap_or(Money::zero()) + delta);
        }
        if let Some(share) = sample.context_percent.filter(|v| *v <= 100) {
            entry.record.peak_context_percent =
                Some(entry.record.peak_context_percent.unwrap_or(0).max(share));
        }
    }
}
