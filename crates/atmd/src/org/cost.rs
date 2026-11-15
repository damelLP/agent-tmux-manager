//! Task accounting uses the organisation actor, never a subscriber/second writer.
//! Ingestion is bounded and nonwaiting, so a stuck log cannot stall hook tracking.
//! Accepted commands retain responsibility after the one-way connection ends.
//! Overflow/unavailability is logged by the input boundary: those observations
//! cannot be recovered from the registry's latest row. No automatic retries.
use super::{OrgHandle, OrgServiceError, Request};
use atm_org::{
    OrgCommand, OrgEvent, OrgState, TaskCommand, TaskCostRecorded, TaskId, TaskState,
    TaskUsageObservation,
};
use chrono::{DateTime, Utc};
use tokio::sync::oneshot;

impl OrgHandle {
    pub(crate) fn observe_task_usage(
        &self,
        input: TaskUsageObservation,
        now: DateTime<Utc>,
    ) -> Result<(), OrgServiceError> {
        let (reply, _) = oneshot::channel();
        self.submit(Request::Execute {
            command: Box::new(OrgCommand::ObserveTaskUsage(input)),
            now,
            caller_pane: None,
            reply,
        })
    }
}

fn record(state: &OrgState, id: &TaskId) -> Option<TaskCostRecorded> {
    let task = state.task(id)?;
    if task.state != TaskState::Accepted || task.cost.is_some() {
        return None;
    }
    let body = TaskCostRecorded {
        task_id: id.clone(),
        record: state.task_cost(id, task.changed_at)?,
    };
    // Future/Unknown historical metadata must not make startup write a forbidden
    // record or disable unrelated organisation operations. Reuse current policy.
    state
        .decide(&command(body.clone()), task.changed_at)
        .ok()?
        .first()?;
    Some(body)
}
fn command(body: TaskCostRecorded) -> OrgCommand {
    OrgCommand::Task(TaskCommand::RecordCost {
        task_id: body.task_id,
        record: body.record,
    })
}

pub(super) fn after_event(state: &OrgState, event: &OrgEvent) -> Option<OrgEvent> {
    if let OrgEvent::TaskVerdict(verdict) = event {
        record(state, &verdict.task_id).map(OrgEvent::TaskCostRecorded)
    } else {
        None
    }
}

pub(super) fn missing(state: &OrgState) -> Vec<(OrgCommand, DateTime<Utc>)> {
    state
        .tasks()
        .filter_map(|task| record(state, &task.id).map(|body| (command(body), task.changed_at)))
        .collect()
}
