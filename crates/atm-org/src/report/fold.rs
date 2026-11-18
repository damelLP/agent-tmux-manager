//! Supplemental facts only. No usage totals or private event text are retained.
use super::OwnerStepIns;
use crate::{
    Address, AgentId, DecisionId, OrgEvent, OrgState, TaskId, TaskState, TaskSubmission,
    TaskVerdict,
};
use atm_core::SessionId;
use chrono::{DateTime, Utc};
use std::collections::{BTreeMap, HashSet};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ReportLedger {
    pub(super) tasks: BTreeMap<TaskId, TaskFacts>,
    pub(super) decisions: BTreeMap<DecisionId, DecisionFacts>,
    closure: Option<ReviewClosure>,
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct TaskFacts {
    pub agent: Option<AgentId>,
    pub started: Option<DateTime<Utc>>,
    pub submissions: u64,
    pub reminders: u64,
    pub flags: u64,
    pub steps: OwnerStepIns,
    pub sessions: HashSet<SessionId>,
    last_context: Option<u8>,
    pub submit_context: Option<Option<u8>>,
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct DecisionFacts {
    pub task: Option<TaskId>,
    pub answers: u64,
    pub redone: BTreeMap<TaskId, u64>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
struct ReviewClosure {
    parent: TaskId,
    review: TaskId,
    by: Address,
    seq: u64,
    cost_allowed: bool,
}
#[derive(Debug)]
pub(crate) struct ReportDelta {
    action: Action,
    closure: Option<ReviewClosure>,
}
#[derive(Debug)]
enum Action {
    None,
    Assigned(TaskId, AgentId, DateTime<Utc>, bool),
    Submitted(TaskId),
    Count(TaskId, Count),
    Usage(TaskId, SessionId, Option<u8>),
    Requested(DecisionId, Option<TaskId>),
    Answered(DecisionId, Vec<TaskId>),
}
#[derive(Debug)]
enum Count {
    Reminder,
    Flag,
    Verdict,
    Redirect,
    Message,
}
fn unfinished(state: TaskState) -> bool {
    !matches!(state, TaskState::Accepted | TaskState::Cancelled)
}
fn only_task(state: &OrgState, address: &Address) -> Option<TaskId> {
    let Address::Agent(id) = address else {
        return None;
    };
    let mut tasks = state
        .tasks()
        .filter(|t| t.agent_id.as_ref() == Some(id) && unfinished(t.state));
    let first = tasks.next()?;
    tasks.next().is_none().then(|| first.id.clone())
}
impl OrgState {
    /// Capture the before image, but mutate nothing until the original fold succeeds.
    pub(crate) fn report_delta(
        &self,
        seq: u64,
        at: DateTime<Utc>,
        event: &OrgEvent,
    ) -> ReportDelta {
        let prior = self
            .reports
            .closure
            .as_ref()
            .filter(|v| v.seq.checked_add(1) == Some(seq));
        let closing = matches!((prior,event), (Some(p),OrgEvent::TaskVerdict(v)) if v.task_id == p.review && v.by == p.by && v.review_task.is_none() && v.verdict == TaskVerdict::Accepted);
        let closure = match event {
            OrgEvent::TaskVerdict(v) => v.review_task.as_ref().map(|id| ReviewClosure {
                parent: v.task_id.clone(),
                review: id.clone(),
                by: v.by.clone(),
                seq,
                cost_allowed: true,
            }),
            OrgEvent::TaskCostRecorded(v) => prior
                .filter(|p| p.cost_allowed && p.parent == v.task_id)
                .map(|p| ReviewClosure {
                    seq,
                    cost_allowed: false,
                    ..p.clone()
                }),
            _ => None,
        };
        let action = match event {
            OrgEvent::TaskAssigned(v) => {
                let first = self
                    .reports
                    .tasks
                    .get(&v.task_id)
                    .is_none_or(|f| f.started.is_none());
                let exempt = first
                    && self
                        .task(&v.task_id)
                        .is_some_and(|t| t.creator == Address::Owner);
                Action::Assigned(
                    v.task_id.clone(),
                    v.agent_id.clone(),
                    at,
                    v.by == Address::Owner && !exempt,
                )
            }
            OrgEvent::TaskSubmitted(v) if v.what == TaskSubmission::Result => {
                Action::Submitted(v.task_id.clone())
            }
            OrgEvent::TaskReminded(v) => Action::Count(v.task_id.clone(), Count::Reminder),
            OrgEvent::TaskFlagged(v) => Action::Count(v.task_id.clone(), Count::Flag),
            OrgEvent::TaskVerdict(v) if v.by == Address::Owner && !closing => {
                Action::Count(v.task_id.clone(), Count::Verdict)
            }
            OrgEvent::TaskStateChanged(v) if v.by == Address::Owner => {
                Action::Count(v.task_id.clone(), Count::Redirect)
            }
            OrgEvent::TaskReviewStarted(v) if v.by == Address::Owner => {
                Action::Count(v.task_id.clone(), Count::Redirect)
            }
            OrgEvent::TaskAttached(v) if v.by == Address::Owner => {
                Action::Count(v.task_id.clone(), Count::Redirect)
            }
            OrgEvent::MessageSent(v) if v.from == Address::Owner && v.task.is_none() => {
                only_task(self, &v.to).map_or(Action::None, |id| Action::Count(id, Count::Message))
            }
            OrgEvent::DecisionRequested(v) => {
                Action::Requested(DecisionId::new(seq), only_task(self, &v.requester))
            }
            OrgEvent::DecisionResolved(v) | OrgEvent::DecisionOverruled(v)
                if v.resolution.by == Address::Owner =>
            {
                Action::Answered(
                    v.resolution.decision_id,
                    if matches!(event, OrgEvent::DecisionOverruled(_)) {
                        v.redo_tasks.clone().unwrap_or_default()
                    } else {
                        Vec::new()
                    },
                )
            }
            OrgEvent::TaskUsageObserved(v)
                if v.turn
                    || v.cumulative_cost.is_some_and(|m| m.as_microdollars() >= 0)
                    || v.context_percent.is_some_and(|p| p <= 100) =>
            {
                v.task_id
                    .as_ref()
                    .filter(|id| {
                        self.task(id)
                            .is_some_and(|t| unfinished(t.state) && t.state != TaskState::Unknown)
                    })
                    .map_or(Action::None, |id| {
                        Action::Usage(
                            id.clone(),
                            v.session_id.clone(),
                            v.context_percent.filter(|p| *p <= 100),
                        )
                    })
            }
            _ => Action::None,
        };
        ReportDelta { action, closure }
    }
}
impl ReportLedger {
    pub(crate) fn apply(&mut self, delta: ReportDelta) {
        self.closure = delta.closure;
        match delta.action {
            Action::None => (),
            Action::Assigned(id, agent, at, redirect) => {
                let f = self.tasks.entry(id).or_default();
                f.started.get_or_insert(at);
                f.agent = Some(agent);
                if redirect {
                    f.steps.redirect = f.steps.redirect.saturating_add(1);
                    f.steps.total = f.steps.total.saturating_add(1);
                }
            }
            Action::Submitted(id) => {
                let f = self.tasks.entry(id).or_default();
                f.submissions = f.submissions.saturating_add(1);
                f.submit_context.get_or_insert(f.last_context);
            }
            Action::Count(id, kind) => {
                let f = self.tasks.entry(id).or_default();
                let value = match kind {
                    Count::Reminder => &mut f.reminders,
                    Count::Flag => &mut f.flags,
                    Count::Verdict => &mut f.steps.verdict,
                    Count::Redirect => &mut f.steps.redirect,
                    Count::Message => &mut f.steps.message,
                };
                *value = value.saturating_add(1);
                if !matches!(kind, Count::Reminder | Count::Flag) {
                    f.steps.total = f.steps.total.saturating_add(1);
                }
            }
            Action::Usage(id, session, context) => {
                let f = self.tasks.entry(id).or_default();
                f.sessions.insert(session);
                if let Some(p) = context {
                    f.last_context = Some(p);
                }
            }
            Action::Requested(id, task) => {
                self.decisions.insert(
                    id,
                    DecisionFacts {
                        task,
                        ..DecisionFacts::default()
                    },
                );
            }
            Action::Answered(id, redo) => {
                let f = self.decisions.entry(id).or_default();
                f.answers = f.answers.saturating_add(1);
                for id in redo {
                    let redone = f.redone.entry(id.clone()).or_default();
                    *redone = redone.saturating_add(1);
                    let steps = &mut self.tasks.entry(id).or_default().steps;
                    steps.redo = steps.redo.saturating_add(1);
                    steps.total = steps.total.saturating_add(1);
                }
            }
        }
    }
}
