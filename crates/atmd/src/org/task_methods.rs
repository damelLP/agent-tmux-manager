//! Task schemas, parameter bounds and method/error registration.

use super::{
    tasks::{sequence, TaskError, TaskHandle},
    OrgHandle, OrgServiceError,
};
use crate::server::methods::{parse_params, MethodError, MethodRouter};
use atm_org::{
    Address, AgentId, ApplyError, DecisionId, Refusal, TaskActor, TaskBudget, TaskCommand,
    TaskCreated, TaskFile, TaskId, TaskKind, TaskMeasure, TaskRefusal, TaskState, TaskSubmission,
    TaskVerdict,
};
use atm_protocol::error_code;
use serde::{Deserialize, Serialize};
use std::time::Duration;

macro_rules! params {
    ($name:ident { $($fields:tt)* }) => {
        #[derive(Debug, Clone, Serialize, Deserialize)]
        #[serde(deny_unknown_fields)]
        pub struct $name { $($fields)* }
    };
}
fn work() -> TaskKind {
    TaskKind::Work
}
fn result() -> TaskSubmission {
    TaskSubmission::Result
}
fn limit() -> u16 {
    100
}
params!(CreateParams {
    pub title: String,
    #[serde(default = "work")] pub kind: TaskKind,
    pub parent: Option<TaskId>,
    pub judges: Option<TaskId>,
    pub measure: TaskMeasure,
    pub budget: Option<TaskBudget>,
    #[serde(default)] pub files: Vec<TaskFile>,
    pub rests_on: Option<DecisionId>,
});
params!(AssignParams { pub task_id: TaskId, pub agent_id: AgentId, });
params!(SetStateParams { pub task_id: TaskId, pub state: TaskState, pub reason: Option<String>, });
params!(SubmitParams { pub task_id: TaskId, #[serde(default = "result")] pub what: TaskSubmission, #[serde(default)] pub files: Vec<TaskFile>, });
params!(AttachParams { pub task_id: TaskId, pub file: TaskFile, });
params!(ReviewParams { pub task_id: TaskId, pub review_task: TaskId, });
params!(VerdictParams { pub task_id: TaskId, pub verdict: TaskVerdict, pub review_task: Option<TaskId>, pub note: Option<String>, });
params!(GetParams { pub task_id: TaskId, });
params!(ListParams {
    pub agent_id: Option<AgentId>, pub creator: Option<Address>, pub state: Option<TaskState>,
    pub kind: Option<TaskKind>, pub parent: Option<TaskId>, pub after: Option<TaskId>,
    #[serde(default)] pub open_only: bool,
    #[serde(default = "limit")] pub limit: u16,
});

impl From<TaskError> for MethodError {
    fn from(error: TaskError) -> Self {
        let message = error.to_string();
        let code = match error {
            TaskError::InvalidParams(_) => error_code::INVALID_PARAMS.into(),
            TaskError::NotFound(_) => error_code::NOT_FOUND.into(),
            TaskError::Refused(_) => error_code::REFUSED.into(),
            TaskError::Unavailable(_) => error_code::UNAVAILABLE.into(),
            TaskError::Timeout => error_code::TIMEOUT.into(),
            TaskError::Internal(_) => error_code::INTERNAL.into(),
            TaskError::Message(error) => MethodError::from(error).0.code,
            TaskError::Organisation(error) => org_error(error).0.code,
            TaskError::Verdict { source, .. } => org_error(*source).0.code,
        };
        MethodError::new(&code, message)
    }
}
fn org_error(error: OrgServiceError) -> MethodError {
    let message = error.to_string();
    let code = match error {
        OrgServiceError::Refused(Refusal::Task(error)) => match error {
            TaskRefusal::InvalidState { .. }
            | TaskRefusal::ReviewNotSubmitted { .. }
            | TaskRefusal::NotReminded { .. } => error_code::CONFLICT,
            TaskRefusal::NotSettable { .. } | TaskRefusal::Judges { .. } => {
                error_code::INVALID_PARAMS
            }
            TaskRefusal::NotAllowed { .. }
            | TaskRefusal::NotReviewOf { .. }
            | TaskRefusal::OwnReview { .. }
            | TaskRefusal::Measure { .. } => error_code::REFUSED,
        },
        OrgServiceError::Refused(Refusal::Invariant(
            ApplyError::UnknownTask(_) | ApplyError::UnknownDecision(_),
        )) => error_code::NOT_FOUND,
        OrgServiceError::Refused(Refusal::Invariant(
            ApplyError::DuplicateTask(_) | ApplyError::DuplicateDecision(_),
        )) => error_code::CONFLICT,
        other => return super::agents::AgentError::Organisation(other).into(),
    };
    MethodError::new(code, message)
}
fn size(field: &str, text: &str, max: usize) -> Result<(), TaskError> {
    if text.len() > max {
        return Err(TaskError::InvalidParams(format!(
            "{field} exceeds {max} UTF-8 bytes"
        )));
    }
    Ok(())
}
fn optional_size(field: &str, text: Option<&str>, max: usize) -> Result<(), TaskError> {
    if let Some(text) = text {
        size(field, text, max)?;
    }
    Ok(())
}
fn file(file: &TaskFile) -> Result<(), TaskError> {
    size("file path", &file.path, 4096)?;
    optional_size("file note", file.note.as_deref(), 4096)
}
fn files(files: &[TaskFile]) -> Result<(), TaskError> {
    if files.len() > 32 {
        return Err(TaskError::InvalidParams(
            "at most 32 task files are allowed".into(),
        ));
    }
    files.iter().try_for_each(file)
}
fn create(p: &CreateParams) -> Result<(), TaskError> {
    size("title", &p.title, 1024)?;
    files(&p.files)?;
    for id in p.parent.iter().chain(&p.judges) {
        sequence(id)?;
    }
    match &p.measure {
        TaskMeasure::Mechanical { command } => size("measure command", command, 16384)?,
        TaskMeasure::Reviewed { rubric } => size("measure rubric", rubric, 4096)?,
        TaskMeasure::Owner => (),
    }
    Ok(())
}
fn state(p: &SetStateParams) -> Result<(), TaskError> {
    sequence(&p.task_id)?;
    if !matches!(
        p.state,
        TaskState::Open
            | TaskState::Working
            | TaskState::Waiting
            | TaskState::Blocked
            | TaskState::Cancelled
    ) {
        return Err(TaskError::InvalidParams("state must be open, working, waiting, blocked or cancelled; use the corresponding task method for other transitions".into()));
    }
    optional_size("reason", p.reason.as_deref(), 16384)
}
fn listing(p: &ListParams) -> Result<(), TaskError> {
    if !(1..=1000).contains(&p.limit) {
        return Err(TaskError::InvalidParams(
            "limit must be between 1 and 1000".into(),
        ));
    }
    for id in p.parent.iter().chain(&p.after) {
        sequence(id)?;
    }
    Ok(())
}
fn encode(value: impl Serialize) -> Result<serde_json::Value, MethodError> {
    serde_json::to_value(value).map_err(|e| MethodError::new(error_code::INTERNAL, e.to_string()))
}

/// Always advertised; unavailable persistence cannot yield a successful mutation.
pub fn register_task_methods(router: &mut MethodRouter, org: OrgHandle) -> Result<(), MethodError> {
    let tasks = TaskHandle::new(org);
    macro_rules! mutation {
        ($name:literal, $ty:ty, $validate:expr, $make:expr) => {{
            let tasks = tasks.clone();
            router.register($name, Duration::from_secs(30), move |caller, params| {
                let tasks = tasks.clone();
                async move {
                    let p: $ty = parse_params(params)?;
                    ($validate)(&p)?;
                    encode(tasks.mutate(caller, move |by| ($make)(p, by)).await?)
                }
            })?;
        }};
    }
    mutation!(
        "task.create",
        CreateParams,
        create,
        |p: CreateParams, by: TaskActor| TaskCommand::Create(TaskCreated {
            title: p.title,
            kind: p.kind,
            creator: by.who,
            parent: p.parent,
            judges: p.judges,
            measure: p.measure,
            budget: p.budget,
            files: p.files,
            rests_on: p.rests_on
        })
    );
    mutation!(
        "task.assign",
        AssignParams,
        |p: &AssignParams| sequence(&p.task_id).map(|_| ()),
        |p: AssignParams, by| TaskCommand::Assign {
            task_id: p.task_id,
            agent_id: p.agent_id,
            by
        }
    );
    mutation!(
        "task.set_state",
        SetStateParams,
        state,
        |p: SetStateParams, by| TaskCommand::SetState {
            task_id: p.task_id,
            state: p.state,
            reason: p.reason,
            by
        }
    );
    mutation!(
        "task.submit",
        SubmitParams,
        |p: &SubmitParams| {
            sequence(&p.task_id)?;
            files(&p.files)
        },
        |p: SubmitParams, by| TaskCommand::Submit {
            task_id: p.task_id,
            what: p.what,
            files: p.files,
            by
        }
    );
    mutation!(
        "task.attach",
        AttachParams,
        |p: &AttachParams| {
            sequence(&p.task_id)?;
            file(&p.file)
        },
        |p: AttachParams, by| TaskCommand::Attach {
            task_id: p.task_id,
            file: p.file,
            by
        }
    );
    mutation!(
        "task.review",
        ReviewParams,
        |p: &ReviewParams| {
            sequence(&p.task_id)?;
            sequence(&p.review_task).map(|_| ())
        },
        |p: ReviewParams, by| TaskCommand::StartReview {
            task_id: p.task_id,
            review_task: p.review_task,
            by
        }
    );
    mutation!(
        "task.verdict",
        VerdictParams,
        |p: &VerdictParams| {
            sequence(&p.task_id)?;
            if let Some(id) = &p.review_task {
                sequence(id)?;
            }
            optional_size("note", p.note.as_deref(), 16384)
        },
        |p: VerdictParams, by| TaskCommand::Verdict {
            task_id: p.task_id,
            verdict: p.verdict,
            review_task: p.review_task,
            note: p.note,
            by
        }
    );
    let list = tasks.clone();
    router.register(
        "task.list",
        Duration::from_secs(10),
        move |caller, params| {
            let tasks = list.clone();
            async move {
                let p: ListParams = parse_params(params)?;
                listing(&p)?;
                encode(tasks.list(caller, p).await?)
            }
        },
    )?;
    router.register(
        "task.get",
        Duration::from_secs(10),
        move |caller, params| {
            let tasks = tasks.clone();
            async move {
                let p: GetParams = parse_params(params)?;
                sequence(&p.task_id)?;
                encode(tasks.get(caller, p).await?)
            }
        },
    )?;
    Ok(())
}
