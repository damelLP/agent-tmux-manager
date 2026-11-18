//! Recorded dependency graph shared by caps, overruling and return reports.
//! Links cross projects; task IDs are emitted in creation order, once each.
use crate::{DecisionId, OrgState, TaskId, TaskState};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DecisionImpact {
    pub started: Vec<TaskId>,
    pub not_started: Vec<TaskId>,
    pub cancelled: Vec<TaskId>,
    /// Annotation overlapping the live partitions, never assumed completed.
    pub unknown: Vec<TaskId>,
}
impl DecisionImpact {
    #[must_use]
    pub fn contains(&self, id: &TaskId) -> bool {
        self.started.contains(id) || self.not_started.contains(id) || self.cancelled.contains(id)
    }
    #[must_use]
    pub fn started_count(&self) -> usize {
        self.started.len()
    }
}

#[must_use]
pub fn decision_impact(state: &OrgState, id: DecisionId) -> DecisionImpact {
    let mut reached: BTreeSet<_> = state.resting_tasks(id).map(|t| t.id.clone()).collect();
    loop {
        let before = reached.len();
        for task in state.tasks() {
            if task
                .parent
                .iter()
                .chain(&task.judges)
                .any(|id| reached.contains(id))
            {
                reached.insert(task.id.clone());
            }
        }
        if reached.len() == before {
            break;
        }
    }
    let mut impact = DecisionImpact::default();
    for task in state.tasks().filter(|t| reached.contains(&t.id)) {
        if task.state == TaskState::Cancelled {
            impact.cancelled.push(task.id.clone());
        } else {
            if task.state == TaskState::Unknown {
                impact.unknown.push(task.id.clone());
            }
            if state.task_was_assigned(&task.id) {
                impact.started.push(task.id.clone());
            } else {
                impact.not_started.push(task.id.clone());
            }
        }
    }
    impact
}
