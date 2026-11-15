//! Tasks: who works on what, in which state, judged by what.
//!
//! A task is created by `task.created` and is from then on in one of ten
//! states. Its id is `t` plus the sequence number of that event, as message
//! ids are `m<seq>`; the free-text task reference of a message is not checked
//! against tasks. Everything follows the crate's decide → append → apply rule:
//! [`TaskCommand`] is decided against the table below, and the events it
//! returns are folded without checking the table again, so a log written under
//! a later policy still folds. Apply checks references only (the task, the
//! agents and the tasks an event names exist).
//!
//! # The transition table
//!
//! A cell names the state the task ends in. `same` is accepted and writes
//! nothing. `—` is refused. `accepted` and `cancelled` are final.
//!
//! | From ↓ · Move → | assign | working | waiting | blocked | submit | start review | accepted | changes requested | back to open | cancel |
//! |---|---|---|---|---|---|---|---|---|---|---|
//! | open | assigned | — | — | — | — | — | — | — | same | cancelled |
//! | assigned | assigned ¹ | working | waiting | blocked | submitted | — | — | — | open | cancelled |
//! | working | assigned ¹ | same | waiting | blocked | submitted | — | — | — | open | cancelled |
//! | waiting | assigned ¹ | working | waiting ² | blocked | submitted | — | — | — | open | cancelled |
//! | blocked | assigned ¹ | working | waiting | blocked ² | submitted | — | — | — | open | cancelled |
//! | submitted | — | working ³ | — | — | same ⁴ | in review | accepted or working ⁵ | changes requested | — | cancelled |
//! | in review | — | — | — | — | — | same ⁶ | accepted or working ⁵ | changes requested | — | cancelled |
//! | changes requested | assigned ¹ | working | waiting | blocked | submitted | — | — | same | open | cancelled |
//! | accepted | — | — | — | — | — | — | same | — | — | — |
//! | cancelled | — | — | — | — | — | — | — | — | — | same |
//!
//! 1. To another agent the task starts over at `assigned`; to the agent it
//!    already has is `same`.
//! 2. `waiting` and `blocked` need a reason. The same reason again is `same`.
//! 3. The agent takes its submission back.
//! 4. The same submission again is `same`; a different one is refused until a
//!    verdict or until the first is taken back.
//! 5. An accepted result ends the task. An accepted plan sends it back to
//!    `working`: the fold reads what the task last submitted. Known limit: a
//!    verdict lands on whatever is submitted when it is decided.
//! 6. The same review task again is `same`; a second one is refused.
//!
//! Attaching a file is allowed in every state and changes no state.
//!
//! # Who may ask
//!
//! The caller is a [`TaskActor`]: its address, and whether the daemon counts
//! it as the lead (this crate cannot know who the lead is). *Authority* over a
//! task is its creator, the lead and the owner.
//!
//! | Move | Who |
//! |---|---|
//! | create, attach | anyone |
//! | working, waiting, blocked, submit | only the task's agent |
//! | assign, start review, back to open, cancel | authority |
//! | the verdict | authority, but never the task's own agent |
//! | reminded, flagged, cost recorded | reports of the daemon; no caller |
//!
//! A caller without the right is refused even where nothing would change.
//!
//! # Reviews and measures
//!
//! A review is a task of kind `review` that names the task it judges when it
//! is created. `StartReview` names it and moves the judged task to
//! `in review`. A verdict may name a review task: it must judge this task, be
//! `submitted`, and not be handed in by this task's own agent. The verdict then
//! writes two events, its own and the review task's `accepted`; every event
//! changes exactly one task.
//!
//! Accepting a *result* depends on the task's measure. Mechanical: authority.
//! Reviewed: the verdict names a submitted review task, unless the owner
//! accepts. The owner's judgement: only the owner. Approving a plan and
//! asking for changes need authority only.
//!
//! # Reports
//!
//! `Reminded` and `Flagged` are the audit of the reminder on a silent turn
//! end: each at most once per state the task is in, a flag only after a
//! reminder, both only while the task's agent is expected to act. `RecordCost`
//! keeps the cost-of-change record of an accepted task. When to remind is the
//! caller's policy, not this module's.
//!
//! # Log format
//!
//! The ten `task.*` kinds are additive: an older daemon skips them, and no
//! other kind refers to a task. `tests/task_golden.rs` states every body.
//! Every task stays in memory, in creation order; none is ever removed.

use crate::command::nonempty;
use crate::{Address, AgentId, ApplyError, IdError, OrgEvent, OrgState, Refusal, TaskId};
use atm_core::Money;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    Work,
    Review,
    Plan,
    Errand,
    DriftCheck,
}

/// The ten states. Reasons and what was submitted are fields of [`Task`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Open,
    Assigned,
    Working,
    Waiting,
    Blocked,
    Submitted,
    InReview,
    Accepted,
    ChangesRequested,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskFileKind {
    Brief,
    Report,
    Evidence,
    Plan,
    Contract,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskFile {
    pub path: String,
    pub kind: TaskFileKind,
    pub note: Option<String>,
}

/// What a task's result is accepted against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TaskMeasure {
    /// A command that passes or fails.
    Mechanical { command: String },
    /// A fresh agent judging against this rubric file.
    Reviewed { rubric: String },
    /// The owner's judgement.
    Owner,
}

/// Fields only: nothing in this crate reads a budget.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskBudget {
    #[serde(default, with = "crate::event::optional_money")]
    pub cost: Option<Money>,
    pub turns: Option<u32>,
    pub deadline: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskSubmission {
    Plan,
    Result,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskVerdict {
    Accepted,
    ChangesRequested,
}

/// The cost-of-change record of an accepted task, as its caller measured it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskCost {
    pub turns: Option<u32>,
    pub review_rounds: Option<u32>,
    pub changes_requested: Option<u32>,
    #[serde(default, with = "crate::event::optional_money")]
    pub cost: Option<Money>,
    pub peak_context_percent: Option<u8>,
    pub seconds_in_state: BTreeMap<TaskState, u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskCreated {
    pub title: String,
    pub kind: TaskKind,
    pub creator: Address,
    pub parent: Option<TaskId>,
    /// The task a review judges; no other kind has one.
    pub judges: Option<TaskId>,
    pub measure: TaskMeasure,
    pub budget: Option<TaskBudget>,
    pub files: Vec<TaskFile>,
    /// The sequence number of the provisional decision this task is built on.
    pub rests_on: Option<u64>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskAssigned {
    pub task_id: TaskId,
    pub agent_id: AgentId,
    pub by: Address,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskStateChanged {
    pub task_id: TaskId,
    pub state: TaskState,
    pub reason: Option<String>,
    pub by: Address,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskSubmitted {
    pub task_id: TaskId,
    pub what: TaskSubmission,
    pub files: Vec<TaskFile>,
    pub by: Address,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskReviewStarted {
    pub task_id: TaskId,
    pub review_task: TaskId,
    pub by: Address,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskVerdictGiven {
    pub task_id: TaskId,
    pub verdict: TaskVerdict,
    pub review_task: Option<TaskId>,
    pub note: Option<String>,
    pub by: Address,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskAttached {
    pub task_id: TaskId,
    pub file: TaskFile,
    pub by: Address,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskReminded {
    pub task_id: TaskId,
    pub agent_id: AgentId,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskFlagged {
    pub task_id: TaskId,
    pub to: Address,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskCostRecorded {
    pub task_id: TaskId,
    pub record: TaskCost,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub id: TaskId,
    pub title: String,
    pub kind: TaskKind,
    pub creator: Address,
    /// The agent the task is assigned to (the design's "owner agent").
    pub agent_id: Option<AgentId>,
    pub parent: Option<TaskId>,
    pub judges: Option<TaskId>,
    pub state: TaskState,
    /// The text given with the latest change of state: why it waits or is
    /// blocked, or the note of a verdict.
    pub reason: Option<String>,
    /// What was handed in, while `submitted` or `in review`.
    pub submission: Option<TaskSubmission>,
    /// The review task that is running, while `in review`.
    pub review_task: Option<TaskId>,
    pub files: Vec<TaskFile>,
    pub measure: TaskMeasure,
    pub budget: Option<TaskBudget>,
    pub rests_on: Option<u64>,
    /// Set by the reports; cleared by the next change of state.
    pub reminded_at: Option<DateTime<Utc>>,
    pub flagged_at: Option<DateTime<Utc>>,
    pub cost: Option<TaskCost>,
    pub created_at: DateTime<Utc>,
    /// When the task last changed state.
    pub changed_at: DateTime<Utc>,
}

/// Who asks. The daemon takes `who` from the caller's pane and says whether
/// it counts that caller as the lead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskActor {
    pub who: Address,
    pub lead: bool,
}

/// Requests and the daemon's reports about tasks. The module docs hold the
/// policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskCommand {
    /// The body's `creator` is the caller.
    Create(TaskCreated),
    Assign {
        task_id: TaskId,
        agent_id: AgentId,
        by: TaskActor,
    },
    SetState {
        task_id: TaskId,
        state: TaskState,
        reason: Option<String>,
        by: TaskActor,
    },
    Submit {
        task_id: TaskId,
        what: TaskSubmission,
        files: Vec<TaskFile>,
        by: TaskActor,
    },
    StartReview {
        task_id: TaskId,
        review_task: TaskId,
        by: TaskActor,
    },
    Verdict {
        task_id: TaskId,
        verdict: TaskVerdict,
        review_task: Option<TaskId>,
        note: Option<String>,
        by: TaskActor,
    },
    Attach {
        task_id: TaskId,
        file: TaskFile,
        by: TaskActor,
    },
    Reminded {
        task_id: TaskId,
        agent_id: AgentId,
    },
    Flagged {
        task_id: TaskId,
        to: Address,
    },
    RecordCost {
        task_id: TaskId,
        record: TaskCost,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TaskRefusal {
    #[error("cannot {operation} task {task_id} while {state:?}")]
    InvalidState {
        task_id: TaskId,
        state: TaskState,
        operation: &'static str,
    },
    #[error("{who} may not {operation} task {task_id}")]
    NotAllowed {
        task_id: TaskId,
        who: Address,
        operation: &'static str,
    },
    #[error("a task cannot be set to {state:?} directly")]
    NotSettable { state: TaskState },
    #[error("task {review_task} is not a review of task {task_id}")]
    NotReviewOf {
        task_id: TaskId,
        review_task: TaskId,
    },
    #[error("review task {review_task} is {state:?}, not submitted")]
    ReviewNotSubmitted {
        review_task: TaskId,
        state: TaskState,
    },
    #[error("review task {review_task} was handed in by the agent of task {task_id} itself")]
    OwnReview {
        task_id: TaskId,
        review_task: TaskId,
    },
    #[error("accepting the result of task {task_id} needs {needs}")]
    Measure {
        task_id: TaskId,
        needs: &'static str,
    },
    #[error("only a review names a task it judges, and a review must: this task is {kind:?}")]
    Judges { kind: TaskKind },
    #[error("task {task_id} has no reminder to follow up")]
    NotReminded { task_id: TaskId },
}

impl TaskCommand {
    /// The fact this command asks for, from the command alone. Decide checks
    /// its references before any policy.
    pub(crate) fn event(&self) -> OrgEvent {
        match self.clone() {
            Self::Create(body) => OrgEvent::TaskCreated(body),
            Self::Assign {
                task_id,
                agent_id,
                by,
            } => OrgEvent::TaskAssigned(TaskAssigned {
                task_id,
                agent_id,
                by: by.who,
            }),
            Self::SetState {
                task_id,
                state,
                reason,
                by,
            } => OrgEvent::TaskStateChanged(TaskStateChanged {
                task_id,
                state,
                reason,
                by: by.who,
            }),
            Self::Submit {
                task_id,
                what,
                files,
                by,
            } => OrgEvent::TaskSubmitted(TaskSubmitted {
                task_id,
                what,
                files,
                by: by.who,
            }),
            Self::StartReview {
                task_id,
                review_task,
                by,
            } => OrgEvent::TaskReviewStarted(TaskReviewStarted {
                task_id,
                review_task,
                by: by.who,
            }),
            Self::Verdict {
                task_id,
                verdict,
                review_task,
                note,
                by,
            } => OrgEvent::TaskVerdict(TaskVerdictGiven {
                task_id,
                verdict,
                review_task,
                note,
                by: by.who,
            }),
            Self::Attach { task_id, file, by } => OrgEvent::TaskAttached(TaskAttached {
                task_id,
                file,
                by: by.who,
            }),
            Self::Reminded { task_id, agent_id } => {
                OrgEvent::TaskReminded(TaskReminded { task_id, agent_id })
            }
            Self::Flagged { task_id, to } => OrgEvent::TaskFlagged(TaskFlagged { task_id, to }),
            Self::RecordCost { task_id, record } => {
                OrgEvent::TaskCostRecorded(TaskCostRecorded { task_id, record })
            }
        }
    }
}

/// Only the canonical `t<seq>` names a task; any other text finds nothing.
fn task_seq(id: &TaskId) -> Option<u64> {
    let text = id.to_string();
    let seq = text.strip_prefix('t')?.parse::<u64>().ok()?;
    (format!("t{seq}") == text).then_some(seq)
}
fn task_id(seq: u64) -> Result<TaskId, IdError> {
    format!("t{seq}").parse()
}

impl Task {
    /// Every change of state drops what belonged to the state it leaves.
    fn enter(&mut self, state: TaskState, reason: Option<String>, ts: DateTime<Utc>) {
        self.state = state;
        self.reason = reason;
        self.changed_at = ts;
        self.submission = None;
        self.review_task = None;
        self.reminded_at = None;
        self.flagged_at = None;
    }
    fn attach(&mut self, file: &TaskFile) {
        if !self.files.contains(file) {
            self.files.push(file.clone());
        }
    }
    /// Its agent is expected to act on it.
    fn held(&self) -> bool {
        matches!(
            self.state,
            TaskState::Assigned
                | TaskState::Working
                | TaskState::Waiting
                | TaskState::Blocked
                | TaskState::ChangesRequested
        )
    }
    fn refuse(&self, operation: &'static str) -> TaskRefusal {
        TaskRefusal::InvalidState {
            task_id: self.id.clone(),
            state: self.state,
            operation,
        }
    }
    fn not_allowed(&self, who: &Address, operation: &'static str) -> TaskRefusal {
        TaskRefusal::NotAllowed {
            task_id: self.id.clone(),
            who: who.clone(),
            operation,
        }
    }
    fn is_agent(&self, who: &Address) -> bool {
        matches!((&self.agent_id, who), (Some(agent), Address::Agent(caller)) if agent == caller)
    }
    /// The creator, the lead and the owner.
    fn authority(&self, by: &TaskActor, operation: &'static str) -> Result<(), TaskRefusal> {
        if by.lead || by.who == Address::Owner || by.who == self.creator {
            Ok(())
        } else {
            Err(self.not_allowed(&by.who, operation))
        }
    }
    /// Only the task's agent. A task nobody holds is refused for its state.
    fn own_agent(&self, by: &TaskActor, operation: &'static str) -> Result<(), TaskRefusal> {
        if self.agent_id.is_none() {
            Err(self.refuse(operation))
        } else if self.is_agent(&by.who) {
            Ok(())
        } else {
            Err(self.not_allowed(&by.who, operation))
        }
    }
}

impl OrgState {
    #[must_use]
    pub fn task(&self, id: &TaskId) -> Option<&Task> {
        self.tasks.get(&task_seq(id)?)
    }
    /// In the order the tasks were created.
    pub fn tasks(&self) -> impl Iterator<Item = &Task> {
        self.tasks.values()
    }
    pub(crate) fn require_task(&self, id: &TaskId) -> Result<&Task, ApplyError> {
        self.task(id)
            .ok_or_else(|| ApplyError::UnknownTask(id.clone()))
    }
    pub(crate) fn task_mut(&mut self, id: &TaskId) -> Result<&mut Task, ApplyError> {
        task_seq(id)
            .and_then(|seq| self.tasks.get_mut(&seq))
            .ok_or_else(|| ApplyError::UnknownTask(id.clone()))
    }
    /// `review_task`, which must be a review task that judges `task`.
    fn review_of(&self, task: &Task, review_task: &TaskId) -> Result<&Task, Refusal> {
        let review = self.require_task(review_task)?;
        if review.kind != TaskKind::Review || review.judges.as_ref() != Some(&task.id) {
            return Err(TaskRefusal::NotReviewOf {
                task_id: task.id.clone(),
                review_task: review_task.clone(),
            }
            .into());
        }
        Ok(review)
    }

    /// The task policy of the module docs. `event` is `command.event()`, whose
    /// references decide has already checked.
    pub(crate) fn decide_task(
        &self,
        command: &TaskCommand,
        event: OrgEvent,
    ) -> Result<Vec<OrgEvent>, Refusal> {
        use TaskState as S;
        let changed = match command {
            TaskCommand::Create(body) => {
                nonempty("title", &body.title)?;
                files_named(&body.files)?;
                match &body.measure {
                    TaskMeasure::Mechanical { command } => nonempty("measure command", command)?,
                    TaskMeasure::Reviewed { rubric } => nonempty("measure rubric", rubric)?,
                    TaskMeasure::Owner => {}
                }
                if (body.kind == TaskKind::Review) != body.judges.is_some() {
                    return Err(TaskRefusal::Judges { kind: body.kind }.into());
                }
                true
            }
            TaskCommand::Assign {
                task_id,
                agent_id,
                by,
            } => {
                let task = self.require_task(task_id)?;
                task.authority(by, "assign")?;
                if !(task.state == S::Open || task.held()) {
                    return Err(task.refuse("assign").into());
                }
                task.agent_id.as_ref() != Some(agent_id)
            }
            TaskCommand::SetState {
                task_id,
                state,
                reason,
                by,
            } => {
                let task = self.require_task(task_id)?;
                let from = match state {
                    S::Working | S::Waiting | S::Blocked => {
                        task.own_agent(by, "work on")?;
                        task.held() || (*state == S::Working && task.state == S::Submitted)
                    }
                    S::Open => {
                        task.authority(by, "reopen")?;
                        task.state == S::Open || task.held()
                    }
                    S::Cancelled => {
                        task.authority(by, "cancel")?;
                        task.state != S::Accepted
                    }
                    S::Assigned
                    | S::Submitted
                    | S::InReview
                    | S::Accepted
                    | S::ChangesRequested => {
                        return Err(TaskRefusal::NotSettable { state: *state }.into())
                    }
                };
                match reason {
                    Some(reason) => nonempty("reason", reason)?,
                    None if matches!(state, S::Waiting | S::Blocked) => nonempty("reason", "")?,
                    None => {}
                }
                if !from {
                    return Err(task.refuse("change the state of").into());
                }
                // Asking for the state it is in writes nothing. Only waiting
                // and blocked record a new reason.
                task.state != *state
                    || (matches!(state, S::Waiting | S::Blocked) && &task.reason != reason)
            }
            TaskCommand::Submit {
                task_id,
                what,
                files,
                by,
            } => {
                let task = self.require_task(task_id)?;
                task.own_agent(by, "submit")?;
                files_named(files)?;
                let again = task.state == S::Submitted
                    && task.submission == Some(*what)
                    && files.iter().all(|file| task.files.contains(file));
                if !(task.held() || again) {
                    return Err(task.refuse("submit").into());
                }
                !again
            }
            TaskCommand::StartReview {
                task_id,
                review_task,
                by,
            } => {
                let task = self.require_task(task_id)?;
                task.authority(by, "start a review of")?;
                if task.state == S::InReview && task.review_task.as_ref() == Some(review_task) {
                    return Ok(vec![]);
                }
                if task.state != S::Submitted {
                    return Err(task.refuse("start a review of").into());
                }
                let review = self.review_of(task, review_task)?;
                if matches!(review.state, S::Accepted | S::Cancelled) {
                    return Err(review.refuse("start a review with").into());
                }
                true
            }
            TaskCommand::Verdict {
                task_id,
                verdict,
                review_task,
                note,
                by,
            } => {
                let task = self.require_task(task_id)?;
                task.authority(by, "give the verdict on")?;
                if task.is_agent(&by.who) {
                    return Err(task
                        .not_allowed(&by.who, "give the verdict on its own")
                        .into());
                }
                if let Some(note) = note {
                    nonempty("note", note)?;
                }
                match (task.state, verdict) {
                    (S::Accepted, TaskVerdict::Accepted)
                    | (S::ChangesRequested, TaskVerdict::ChangesRequested) => return Ok(vec![]),
                    (S::Submitted | S::InReview, _) => {}
                    _ => return Err(task.refuse("give the verdict on").into()),
                }
                let review = match review_task {
                    Some(id) => {
                        let review = self.review_of(task, id)?;
                        if review.state != S::Submitted {
                            return Err(TaskRefusal::ReviewNotSubmitted {
                                review_task: id.clone(),
                                state: review.state,
                            }
                            .into());
                        }
                        // Nobody judges its own work, through a review either.
                        if review.agent_id.is_some() && review.agent_id == task.agent_id {
                            return Err(TaskRefusal::OwnReview {
                                task_id: task_id.clone(),
                                review_task: id.clone(),
                            }
                            .into());
                        }
                        Some(review)
                    }
                    None => None,
                };
                // The measure limits who accepts a result. A plan's approval
                // is not acceptance of the work.
                let owner = by.who == Address::Owner;
                if *verdict == TaskVerdict::Accepted
                    && task.submission != Some(TaskSubmission::Plan)
                {
                    let needs = match task.measure {
                        TaskMeasure::Owner if !owner => Some("the owner's verdict"),
                        TaskMeasure::Reviewed { .. } if !owner && review.is_none() => {
                            Some("a submitted review task, or the owner's verdict")
                        }
                        _ => None,
                    };
                    if let Some(needs) = needs {
                        return Err(TaskRefusal::Measure {
                            task_id: task_id.clone(),
                            needs,
                        }
                        .into());
                    }
                }
                let mut events = vec![event];
                // The review is done: its verdict was used.
                events.extend(review.map(|review| {
                    OrgEvent::TaskVerdict(TaskVerdictGiven {
                        task_id: review.id.clone(),
                        verdict: TaskVerdict::Accepted,
                        review_task: None,
                        note: None,
                        by: by.who.clone(),
                    })
                }));
                return Ok(events);
            }
            TaskCommand::Attach { task_id, file, .. } => {
                nonempty("file path", &file.path)?;
                !self.require_task(task_id)?.files.contains(file)
            }
            TaskCommand::Reminded { task_id, agent_id } => {
                let task = self.require_task(task_id)?;
                if !task.held() {
                    return Err(task.refuse("remind the agent of").into());
                }
                let reminded = Address::Agent(agent_id.clone());
                if !task.is_agent(&reminded) {
                    return Err(task.not_allowed(&reminded, "be reminded of").into());
                }
                task.reminded_at.is_none()
            }
            TaskCommand::Flagged { task_id, to } => {
                let task = self.require_task(task_id)?;
                if !task.held() {
                    return Err(task.refuse("flag").into());
                }
                if to != &task.creator {
                    return Err(task.not_allowed(to, "be told about").into());
                }
                if task.reminded_at.is_none() {
                    return Err(TaskRefusal::NotReminded {
                        task_id: task_id.clone(),
                    }
                    .into());
                }
                task.flagged_at.is_none()
            }
            TaskCommand::RecordCost { task_id, record } => {
                let task = self.require_task(task_id)?;
                if task.state != S::Accepted {
                    return Err(task.refuse("record the cost of").into());
                }
                task.cost.as_ref() != Some(record)
            }
        };
        Ok(if changed { vec![event] } else { vec![] })
    }
}

fn files_named(files: &[TaskFile]) -> Result<(), Refusal> {
    files
        .iter()
        .try_for_each(|file| nonempty("file path", &file.path))
}

// What apply does with each fact. `check` runs before any change and checks
// references only. `fold` changes one task and enforces no policy.

impl TaskCreated {
    pub(crate) fn check(&self, state: &OrgState) -> Result<(), ApplyError> {
        state.validate_address(&self.creator)?;
        for id in self.parent.iter().chain(&self.judges) {
            state.require_task(id)?;
        }
        Ok(())
    }
    /// The new task takes its id from the event's sequence number.
    pub(crate) fn fold(
        &self,
        tasks: &mut BTreeMap<u64, Task>,
        seq: u64,
        ts: DateTime<Utc>,
    ) -> Result<(), ApplyError> {
        let id = task_id(seq)?;
        if tasks.contains_key(&seq) {
            return Err(ApplyError::DuplicateTask(id));
        }
        tasks.insert(
            seq,
            Task {
                id,
                title: self.title.clone(),
                kind: self.kind,
                creator: self.creator.clone(),
                agent_id: None,
                parent: self.parent.clone(),
                judges: self.judges.clone(),
                state: TaskState::Open,
                reason: None,
                submission: None,
                review_task: None,
                files: self.files.clone(),
                measure: self.measure.clone(),
                budget: self.budget.clone(),
                rests_on: self.rests_on,
                reminded_at: None,
                flagged_at: None,
                cost: None,
                created_at: ts,
                changed_at: ts,
            },
        );
        Ok(())
    }
}
impl TaskAssigned {
    pub(crate) fn check(&self, state: &OrgState) -> Result<(), ApplyError> {
        state.require_task(&self.task_id)?;
        state.require_agent(&self.agent_id)?;
        state.validate_address(&self.by)
    }
    pub(crate) fn fold(&self, task: &mut Task, ts: DateTime<Utc>) {
        task.enter(TaskState::Assigned, None, ts);
        task.agent_id = Some(self.agent_id.clone());
    }
}
impl TaskStateChanged {
    pub(crate) fn check(&self, state: &OrgState) -> Result<(), ApplyError> {
        state.require_task(&self.task_id)?;
        state.validate_address(&self.by)
    }
    pub(crate) fn fold(&self, task: &mut Task, ts: DateTime<Utc>) {
        task.enter(self.state, self.reason.clone(), ts);
        if self.state == TaskState::Open {
            task.agent_id = None;
        }
    }
}
impl TaskSubmitted {
    pub(crate) fn check(&self, state: &OrgState) -> Result<(), ApplyError> {
        state.require_task(&self.task_id)?;
        state.validate_address(&self.by)
    }
    pub(crate) fn fold(&self, task: &mut Task, ts: DateTime<Utc>) {
        task.enter(TaskState::Submitted, None, ts);
        task.submission = Some(self.what);
        self.files.iter().for_each(|file| task.attach(file));
    }
}
impl TaskReviewStarted {
    pub(crate) fn check(&self, state: &OrgState) -> Result<(), ApplyError> {
        state.require_task(&self.task_id)?;
        state.require_task(&self.review_task)?;
        state.validate_address(&self.by)
    }
    pub(crate) fn fold(&self, task: &mut Task, ts: DateTime<Utc>) {
        let submission = task.submission;
        task.enter(TaskState::InReview, None, ts);
        task.submission = submission;
        task.review_task = Some(self.review_task.clone());
    }
}
impl TaskVerdictGiven {
    pub(crate) fn check(&self, state: &OrgState) -> Result<(), ApplyError> {
        state.require_task(&self.task_id)?;
        if let Some(review_task) = &self.review_task {
            state.require_task(review_task)?;
        }
        state.validate_address(&self.by)
    }
    pub(crate) fn fold(&self, task: &mut Task, ts: DateTime<Utc>) {
        let state = match (self.verdict, task.submission) {
            // The plan stop is over: build it.
            (TaskVerdict::Accepted, Some(TaskSubmission::Plan)) => TaskState::Working,
            (TaskVerdict::Accepted, _) => TaskState::Accepted,
            (TaskVerdict::ChangesRequested, _) => TaskState::ChangesRequested,
        };
        task.enter(state, self.note.clone(), ts);
    }
}
impl TaskAttached {
    pub(crate) fn check(&self, state: &OrgState) -> Result<(), ApplyError> {
        state.require_task(&self.task_id)?;
        state.validate_address(&self.by)
    }
    pub(crate) fn fold(&self, task: &mut Task) {
        task.attach(&self.file);
    }
}
impl TaskReminded {
    pub(crate) fn check(&self, state: &OrgState) -> Result<(), ApplyError> {
        state.require_task(&self.task_id)?;
        state.require_agent(&self.agent_id).map(|_| ())
    }
    pub(crate) fn fold(&self, task: &mut Task, ts: DateTime<Utc>) {
        task.reminded_at = Some(ts);
    }
}
impl TaskFlagged {
    pub(crate) fn check(&self, state: &OrgState) -> Result<(), ApplyError> {
        state.require_task(&self.task_id)?;
        state.validate_address(&self.to)
    }
    pub(crate) fn fold(&self, task: &mut Task, ts: DateTime<Utc>) {
        task.flagged_at = Some(ts);
    }
}
impl TaskCostRecorded {
    pub(crate) fn check(&self, state: &OrgState) -> Result<(), ApplyError> {
        state.require_task(&self.task_id).map(|_| ())
    }
    pub(crate) fn fold(&self, task: &mut Task) {
        task.cost = Some(self.record.clone());
    }
}
