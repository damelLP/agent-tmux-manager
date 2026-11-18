//! One annotated overrule invalidates recorded work atomically. Legacy answers
//! remain unmeasured and never acquire destructive effects on upgrade.
//!
//! Known limit: each `redo_notices` reconciliation rescans all redo notices and
//! stored messages for exact committed content. This is a growing history-scan
//! cost only; replay and notice deduplication remain correct.
use crate::*;
use chrono::{DateTime, Utc};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RedoProgress {
    pub required: Option<Vec<TaskId>>,
    pub completed: BTreeSet<TaskId>,
    pub cancelled: BTreeSet<TaskId>,
    pub caller_pane: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RedoRecord {
    pub seq: u64,
    progress: RedoProgress,
    assigned: BTreeSet<TaskId>,
    notices: Vec<MessageContent>,
}
pub(crate) type RedoHistory = BTreeMap<DecisionId, RedoRecord>;

pub fn overrule_redo(state: &OrgState, id: DecisionId) -> Result<Vec<TaskId>, Refusal> {
    let impact = decision_impact(state, id);
    if !impact.unknown.is_empty() {
        return Err(Refusal::InvalidValue {
            field: "decision",
            reason: format!(
                "a recorded task has an unknown state; overruling is refused: {}",
                impact
                    .unknown
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        });
    }
    Ok(impact.started)
}
impl OrgState {
    #[must_use]
    pub fn decision_redo(&self, id: DecisionId) -> Option<RedoProgress> {
        let mut progress = self.redo_history.get(&id)?.progress.clone();
        progress.cancelled = progress
            .required
            .iter()
            .flatten()
            .filter(|id| !progress.completed.contains(*id))
            .filter(|id| {
                self.task(id)
                    .is_some_and(|t| t.state == TaskState::Cancelled)
            })
            .cloned()
            .collect();
        Some(progress)
    }
    /// Exact committed msg.sent content is the completion marker, not delivery.
    #[must_use]
    pub fn redo_notices(&self) -> Vec<MessageContent> {
        self.redo_history
            .values()
            .flat_map(|r| &r.notices)
            .filter(|note| !self.messages().any(|m| m.content == **note))
            .cloned()
            .collect()
    }
    pub(crate) fn track_redo_transition(&mut self, event: &OrgEvent) {
        let (id, assigned, accepted) = match event {
            OrgEvent::TaskAssigned(fact) => (&fact.task_id, true, false),
            OrgEvent::TaskVerdict(fact) => (
                &fact.task_id,
                false,
                fact.verdict == TaskVerdict::Accepted
                    && self
                        .task(&fact.task_id)
                        .is_some_and(|t| t.submission == Some(TaskSubmission::Result)),
            ),
            _ => return,
        };
        for record in self.redo_history.values_mut().filter(|r| {
            r.progress
                .required
                .as_ref()
                .is_some_and(|ids| ids.contains(id))
        }) {
            if assigned {
                record.assigned.insert(id.clone());
            }
            if accepted && record.assigned.contains(id) {
                record.progress.completed.insert(id.clone());
            }
        }
    }
    /// All task references and duplicates were validated before any mutation.
    pub(crate) fn apply_overrule(
        &mut self,
        seq: u64,
        at: DateTime<Utc>,
        answer: &DecisionResolutionEvent,
    ) -> Result<(), ApplyError> {
        let resolution = &answer.resolution;
        let id = resolution.decision_id;
        let mut notices = Vec::new();
        for task_id in answer.redo_tasks.iter().flatten() {
            let task = self.require_task(task_id)?;
            let creator = if task.creator == Address::System {
                Address::Owner
            } else {
                task.creator.clone()
            };
            let mut recipients = vec![creator];
            if let Some(agent) = &task.agent_id {
                let to = Address::Agent(agent.clone());
                if !recipients.contains(&to) {
                    recipients.push(to);
                }
            }
            for to in recipients {
                notices.push(MessageContent { delivery_notice: None, from: Address::System, to, kind: MessageKind::Note, body: format!("Task {task_id} needs redo after decision {id} was overruled: {}\nIt was reopened and unassigned. Read the new answer before assigning or submitting it again. No files were reverted.", resolution.option), attachments: vec![], task: Some(task_id.clone()) });
            }
            // Reuse accounting's ordinary pre-transition hook; this is not a
            // second durable task event or a relaxation of task.set_state.
            self.track_cost_transition(
                &OrgEvent::TaskStateChanged(TaskStateChanged {
                    task_id: task_id.clone(),
                    state: TaskState::Open,
                    reason: None,
                    by: resolution.by.clone(),
                }),
                at,
            );
            self.task_mut(task_id)?
                .reopen_for_overrule(id, &resolution.option, at);
        }
        self.redo_history.insert(
            id,
            RedoRecord {
                seq,
                progress: RedoProgress {
                    required: answer.redo_tasks.clone(),
                    caller_pane: answer.caller_pane.clone(),
                    ..RedoProgress::default()
                },
                assigned: BTreeSet::new(),
                notices,
            },
        );
        Ok(())
    }
}
