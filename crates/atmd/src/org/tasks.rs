//! Task RPC facade over the sole organisation writer.
//!
//! Mutation and notification enqueue are two durable operations, not a transaction.
//! A finite worker retains responsibility when its RPC waiter disappears. A daemon
//! death between the writes can leave a task without its automatic message; inspect
//! the task and inbox before repairing it. Replay never re-emits old notifications.
//! Delivery itself belongs exclusively to the existing confirmed-message runtime.

use super::{
    message_inbox::caller_address,
    message_methods::validate_send,
    messages::{MessageError, SendParams},
    OrgHandle, OrgServiceError,
};
use crate::server::methods::{CallContext, MethodError};
use atm_org::{
    Address, AgentId, MessageContent, MessageId, MessageKind, OrgCommand, OrgEvent, OrgState, Task,
    TaskActor, TaskCommand, TaskFileKind, TaskId, TaskKind, TaskState, TaskSubmission,
};
use atm_protocol::ResponseError;
use serde::{Deserialize, Serialize};
use std::{future::Future, sync::Arc, time::Duration};
use tokio::sync::Semaphore;

#[cfg(test)]
#[path = "task_worker_tests.rs"]
mod tests;

pub use super::task_methods::{
    register_task_methods, AssignParams, AttachParams, CreateParams, GetParams, ListParams,
    ReviewParams, SetStateParams, SubmitParams, VerdictParams,
};

pub(super) const PAGE_BYTES: usize = 512 * 1024;
const MUTATION_BUDGET: Duration = Duration::from_secs(25);
const READ_BUDGET: Duration = Duration::from_secs(8);

#[derive(Debug, thiserror::Error)]
pub enum TaskError {
    #[error("invalid task parameters: {0}")]
    InvalidParams(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Refused(String),
    #[error("task service unavailable: {0}")]
    Unavailable(String),
    #[error("task request timed out; accepted work may complete; inspect tasks and inbox before retrying")]
    Timeout,
    #[error("task operation failed: {0}")]
    Internal(String),
    #[error(transparent)]
    Organisation(#[from] OrgServiceError),
    #[error(transparent)]
    Message(#[from] MessageError),
    #[error("{source}; inspect tasks {task_id} and {review_task}: the verdict may have committed only a prefix")]
    Verdict {
        task_id: TaskId,
        review_task: TaskId,
        source: Box<OrgServiceError>,
    },
}

/// A shared list/entry-point subset, not another field-for-field Task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSummary {
    pub task_id: TaskId,
    pub title: String,
    pub kind: TaskKind,
    pub state: TaskState,
    pub agent_id: Option<AgentId>,
    pub reason: Option<String>,
}

#[must_use]
pub fn summarize(task: &Task) -> TaskSummary {
    TaskSummary {
        task_id: task.id.clone(),
        title: task.title.clone(),
        kind: task.kind,
        state: task.state,
        agent_id: task.agent_id.clone(),
        reason: task.reason.clone(),
    }
}

#[must_use]
pub fn is_open(task: &Task) -> bool {
    !matches!(task.state, TaskState::Accepted | TaskState::Cancelled)
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Notification {
    Queued { message_id: MessageId },
    Unconfirmed { error: ResponseError },
}
#[derive(Debug, Serialize, Deserialize)]
pub struct MutationResult {
    pub task_id: TaskId,
    pub changed: bool,
    pub notification: Option<Notification>,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct ListResult {
    pub tasks: Vec<TaskSummary>,
    pub next_after: Option<TaskId>,
}
#[derive(Debug, Serialize)]
pub struct GetResult {
    pub task: Task,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_status: Option<atm_org::budget::BudgetStatus>,
}

/// Canonical task sequence; legacy free-text message references are not task IDs.
pub fn sequence(id: &TaskId) -> Result<u64, TaskError> {
    let text = id.to_string();
    text.strip_prefix('t')
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|seq| format!("t{seq}") == text)
        .ok_or_else(|| {
            TaskError::InvalidParams(format!(
                "{text:?}: expected t followed by canonical u64 decimal"
            ))
        })
}

fn task<'a>(state: &'a OrgState, id: &TaskId) -> Result<&'a Task, TaskError> {
    state
        .task(id)
        .ok_or_else(|| TaskError::NotFound(format!("unknown task {id}")))
}

fn unique_paths<'a>(paths: impl Iterator<Item = &'a str>) -> Vec<String> {
    paths.fold(Vec::new(), |mut out, path| {
        if !out.iter().any(|p| p == path) {
            out.push(path.to_owned());
        }
        out
    })
}

/// Pure composition from an already committed task event and supplied state.
/// A later restart-recoverable bridge can reuse this without changing delivery.
pub fn notification(
    event: &OrgEvent,
    state: &OrgState,
) -> Result<Option<MessageContent>, TaskError> {
    let (id, from, to, kind, body, attachments) = match event {
        OrgEvent::TaskAssigned(assigned) => {
            let t = task(state, &assigned.task_id)?;
            let files = unique_paths(
                t.files
                    .iter()
                    .filter(|f| f.kind == TaskFileKind::Brief)
                    .map(|f| f.path.as_str()),
            );
            let instruction = if files.is_empty() {
                "No brief file is attached; inspect the task before starting."
            } else {
                "Read the attached brief files before starting."
            };
            (
                t.id.clone(),
                assigned.by.clone(),
                Address::Agent(assigned.agent_id.clone()),
                MessageKind::Brief,
                format!(
                    "Task {} assigned: {}\nInspect the task: atm task get {}\n{instruction}",
                    t.id, t.title, t.id
                ),
                files,
            )
        }
        OrgEvent::TaskSubmitted(submitted) => {
            let t = task(state, &submitted.task_id)?;
            let files = unique_paths(submitted.files.iter().map(|f| f.path.as_str()));
            let what = match submitted.what {
                TaskSubmission::Plan => "plan",
                TaskSubmission::Result => "result",
                TaskSubmission::Unknown => "unknown",
            };
            let instruction = if files.is_empty() {
                "No submission file is attached."
            } else {
                "Read the attached submission files before giving a verdict."
            };
            let mut body = format!("Task {} submitted ({what}): {}\nSubmitted by {}; inspect the task: atm task get {}\n{instruction}", t.id, t.title, submitted.by, t.id);
            let to = if t.creator == Address::System {
                body.push_str("\nCreator is system; report routed to owner.");
                Address::Owner
            } else {
                t.creator.clone()
            };
            (
                t.id.clone(),
                submitted.by.clone(),
                to,
                MessageKind::Report,
                body,
                files,
            )
        }
        _ => return Ok(None),
    };
    Ok(Some(MessageContent {
        from,
        to,
        kind,
        body,
        attachments,
        task: Some(id),
    }))
}

async fn enqueue(org: OrgHandle, content: MessageContent) -> Result<MessageId, TaskError> {
    validate_send(&SendParams {
        to: content.to.clone(),
        kind: content.kind,
        body: content.body.clone(),
        attachments: content.attachments.clone(),
        task: content.task.clone(),
    })?;
    let events = org
        .execute(OrgCommand::SendMessage(content), chrono::Utc::now())
        .await?;
    events
        .first()
        .map(|e| MessageId::new(e.seq))
        .ok_or_else(|| TaskError::Internal("message enqueue committed no event".into()))
}

fn target(command: &TaskCommand) -> Option<TaskId> {
    match command {
        TaskCommand::Create(_) => None,
        TaskCommand::Assign { task_id, .. }
        | TaskCommand::SetState { task_id, .. }
        | TaskCommand::Submit { task_id, .. }
        | TaskCommand::StartReview { task_id, .. }
        | TaskCommand::Verdict { task_id, .. }
        | TaskCommand::Attach { task_id, .. }
        | TaskCommand::Reminded { task_id, .. }
        | TaskCommand::Flagged { task_id, .. }
        | TaskCommand::RecordCost { task_id, .. } => Some(task_id.clone()),
    }
}

async fn execute_mutation<F, E, EF>(
    org: OrgHandle,
    caller: CallContext,
    make: F,
    send: E,
) -> Result<MutationResult, TaskError>
where
    F: FnOnce(TaskActor) -> TaskCommand,
    E: FnOnce(MessageContent) -> EF,
    EF: Future<Output = Result<MessageId, TaskError>>,
{
    let state = org.snapshot().await?;
    let who = caller_address(&state, &caller)?;
    let lead = matches!(&who, Address::Agent(id) if id.name == "lead");
    let command = make(TaskActor { who, lead });
    let id = target(&command);
    let review = match &command {
        TaskCommand::Verdict { review_task, .. } => review_task.clone(),
        _ => None,
    };
    let events = org
        .execute(OrgCommand::Task(command), chrono::Utc::now())
        .await
        .map_err(|source| match (&id, review, &source) {
            (
                Some(task_id),
                Some(review_task),
                OrgServiceError::Timeout | OrgServiceError::Unavailable(_),
            ) => TaskError::Verdict {
                task_id: task_id.clone(),
                review_task,
                source: Box::new(source),
            },
            _ => TaskError::Organisation(source),
        })?;
    let task_id = match id {
        Some(id) => id,
        None => events
            .first()
            .ok_or_else(|| TaskError::Internal("task create committed no event".into()))
            .and_then(|e| {
                format!("t{}", e.seq)
                    .parse()
                    .map_err(|e: atm_org::IdError| TaskError::Internal(e.to_string()))
            })?,
    };
    let mut result = MutationResult {
        task_id,
        changed: !events.is_empty(),
        notification: None,
    };
    if let Some(envelope) = events
        .first()
        .filter(|e| matches!(e.kind.as_str(), "task.assigned" | "task.submitted"))
    {
        let sent = async {
            let event = OrgEvent::from_parts(&envelope.kind, &envelope.body)
                .map_err(|e| TaskError::Internal(e.to_string()))?
                .ok_or_else(|| {
                    TaskError::Internal("committed task event was not recognized".into())
                })?;
            let state = org.snapshot().await?;
            let content = notification(&event, &state)?
                .ok_or_else(|| TaskError::Internal("task notification was not composed".into()))?;
            send(content).await
        }
        .await;
        result.notification = Some(match sent {
            Ok(message_id) => Notification::Queued { message_id },
            Err(error) => {
                let mut error = MethodError::from(error).0;
                error.message = format!("task {} committed; automatic {} enqueue was not confirmed: {}; inspect the task and inbox before resending", result.task_id, if envelope.kind == "task.assigned" { "brief" } else { "report" }, error.message);
                Notification::Unconfirmed { error }
            }
        });
    }
    Ok(result)
}

/// Clones share one bounded worker pool; no task projection or second writer.
#[derive(Debug, Clone)]
pub struct TaskHandle {
    org: OrgHandle,
    workers: Arc<Semaphore>,
}
impl TaskHandle {
    #[must_use]
    pub fn new(org: OrgHandle) -> Self {
        Self {
            org,
            workers: Arc::new(Semaphore::new(64)),
        }
    }

    async fn run<T: Send + 'static>(
        &self,
        work: impl Future<Output = Result<T, TaskError>> + Send + 'static,
    ) -> Result<T, TaskError> {
        let permit = self
            .workers
            .clone()
            .try_acquire_owned()
            .map_err(|_| TaskError::Unavailable("mutation workers are busy".into()))?;
        // The worker, not its RPC waiter, retains both the permit and the finite
        // mutation/enqueue chain. Dropping a JoinHandle does not abort that work.
        let worker = tokio::spawn(async move {
            let _permit = permit;
            work.await
        });
        tokio::time::timeout(MUTATION_BUDGET, worker)
            .await
            .map_err(|_| TaskError::Timeout)?
            .map_err(|e| TaskError::Internal(format!("mutation worker: {e}")))?
    }

    pub(super) async fn mutate(
        &self,
        caller: CallContext,
        make: impl FnOnce(TaskActor) -> TaskCommand + Send + 'static,
    ) -> Result<MutationResult, TaskError> {
        let org = self.org.clone();
        let sender = org.clone();
        self.run(execute_mutation(org, caller, make, move |content| {
            enqueue(sender, content)
        }))
        .await
    }

    pub(super) async fn list(
        &self,
        caller: CallContext,
        params: ListParams,
    ) -> Result<ListResult, TaskError> {
        tokio::time::timeout(READ_BUDGET, async {
            let state = self.org.snapshot().await?;
            caller_address(&state, &caller)?;
            list(&state, &params)
        })
        .await
        .map_err(|_| TaskError::Timeout)?
    }
    pub(super) async fn get(
        &self,
        caller: CallContext,
        params: GetParams,
    ) -> Result<GetResult, TaskError> {
        tokio::time::timeout(READ_BUDGET, async {
            let state = self.org.snapshot().await?;
            caller_address(&state, &caller)?;
            let task = task(&state, &params.task_id)?;
            let now = chrono::Utc::now();
            let usage = state.task_cost(&task.id, now).unwrap_or_default();
            let result = GetResult {
                task: task.clone(),
                budget_status: atm_org::budget::budget_status(&state, task, &usage, now),
            };
            bounded(&result)?;
            Ok(result)
        })
        .await
        .map_err(|_| TaskError::Timeout)?
    }
}

fn encoded_size(value: &impl Serialize) -> Result<usize, TaskError> {
    serde_json::to_vec(value)
        .map(|v| v.len())
        .map_err(|e| TaskError::Internal(e.to_string()))
}
fn bounded(value: &impl Serialize) -> Result<(), TaskError> {
    if encoded_size(value)? > PAGE_BYTES {
        return Err(TaskError::Refused(format!(
            "task result exceeds {PAGE_BYTES} bytes"
        )));
    }
    Ok(())
}

fn list(state: &OrgState, p: &ListParams) -> Result<ListResult, TaskError> {
    for id in p.agent_id.iter().chain(p.creator.iter().filter_map(|a| {
        if let Address::Agent(id) = a {
            Some(id)
        } else {
            None
        }
    })) {
        if state.agent(id).is_none() {
            return Err(TaskError::NotFound(format!("unknown agent {id}")));
        }
    }
    if let Some(parent) = &p.parent {
        task(state, parent)?;
    }
    let after = p.after.as_ref().map(sequence).transpose()?;
    let mut wanted = state
        .tasks()
        .filter(|t| {
            after.is_none_or(|after| sequence(&t.id).is_ok_and(|seq| seq > after))
                && p.agent_id
                    .as_ref()
                    .is_none_or(|id| t.agent_id.as_ref() == Some(id))
                && p.creator.as_ref().is_none_or(|who| &t.creator == who)
                && p.state.is_none_or(|s| t.state == s)
                && p.kind.is_none_or(|k| t.kind == k)
                && p.parent
                    .as_ref()
                    .is_none_or(|id| t.parent.as_ref() == Some(id))
                && (!p.open_only || is_open(t))
        })
        .peekable();
    let mut result = ListResult {
        tasks: Vec::new(),
        next_after: None,
    };
    while let Some(t) = wanted.next() {
        result.tasks.push(summarize(t));
        result.next_after = wanted.peek().map(|_| t.id.clone());
        if encoded_size(&result)? > PAGE_BYTES {
            result.tasks.pop();
            result.next_after = Some(
                result
                    .tasks
                    .last()
                    .ok_or_else(|| {
                        TaskError::Refused(format!(
                            "task {} exceeds {PAGE_BYTES}-byte list page",
                            t.id
                        ))
                    })?
                    .task_id
                    .clone(),
            );
            break;
        }
        if result.tasks.len() >= usize::from(p.limit) {
            break;
        }
    }
    Ok(result)
}
