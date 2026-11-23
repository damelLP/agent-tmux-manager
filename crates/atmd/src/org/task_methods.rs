//! Task schemas, parameter bounds and method/error registration.

use super::{
    department_directory::{DepartmentDirectory, DepartmentFiles},
    tasks::{sequence, MutationResult, TaskError, TaskHandle},
    OrgHandle, OrgServiceError,
};
use crate::server::methods::{parse_params, MethodError, MethodRouter};
use atm_org::{
    department::{task_budget_default, BudgetDefaultSources},
    Address, AgentId, ApplyError, DecisionId, ProjectId, Refusal, TaskBudget, TaskCommand,
    TaskCreated, TaskFile, TaskFileKind, TaskId, TaskKind, TaskMeasure, TaskRefusal, TaskState,
    TaskSubmission, TaskVerdict,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
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
params!(CarryBackParams { pub task_id: TaskId, pub fix_task: TaskId, pub review_task: TaskId, pub note: String, });
params!(GetParams { pub task_id: TaskId, });
params!(ListParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
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
            | TaskRefusal::CannotCarryBack { .. }
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
fn known(field: &str, known: bool) -> Result<(), TaskError> {
    if !known {
        return Err(TaskError::InvalidParams(format!("{field} must be known")));
    }
    Ok(())
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
    known("file.kind", file.kind != TaskFileKind::Unknown)?;
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
    known("kind", p.kind != TaskKind::Unknown)?;
    size("title", &p.title, 1024)?;
    files(&p.files)?;
    for id in p.parent.iter().chain(&p.judges) {
        sequence(id)?;
    }
    match &p.measure {
        TaskMeasure::Mechanical { command } => size("measure command", command, 16384)?,
        TaskMeasure::Reviewed { rubric } => size("measure rubric", rubric, 4096)?,
        TaskMeasure::Owner => (),
        TaskMeasure::Unknown => {
            return Err(TaskError::InvalidParams(
                "measure.kind must be known".into(),
            ));
        }
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
    known("state", p.state != Some(TaskState::Unknown))?;
    known("kind", p.kind != Some(TaskKind::Unknown))?;
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

#[derive(Serialize)]
struct CreateResult {
    #[serde(flatten)]
    mutation: MutationResult,
    #[serde(skip_serializing_if = "Option::is_none")]
    default_budget_from: Option<BudgetDefaultSources>,
    #[serde(skip_serializing_if = "Option::is_none")]
    warning: Option<String>,
}

fn fill_default_budget(
    directory: Option<DepartmentDirectory>,
    project: Option<&ProjectId>,
    budget: &mut Option<TaskBudget>,
) -> (Option<BudgetDefaultSources>, Option<String>) {
    let (Some(directory), Some(project)) = (directory, project) else {
        return (None, None);
    };
    if !directory.problems.is_empty() {
        let warning =
            format!("department policy incomplete; no default task budget applied for {project}");
        tracing::warn!(%warning);
        return (None, Some(warning));
    }
    let sources = task_budget_default(&directory.departments, project).map(|(value, sources)| {
        *budget = Some(value);
        sources
    });
    (sources, None)
}

/// Always advertised; unavailable persistence cannot yield a successful mutation.
pub fn register_task_methods(router: &mut MethodRouter, org: OrgHandle) -> Result<(), MethodError> {
    register_task_methods_with_reporting(
        router,
        org,
        super::reporting_line::ReportingFiles::default(),
        DepartmentFiles::new(None),
    )
}

pub(crate) fn register_task_methods_with_reporting(
    router: &mut MethodRouter,
    org: OrgHandle,
    reporting: super::reporting_line::ReportingFiles,
    departments: DepartmentFiles,
) -> Result<(), MethodError> {
    let tasks = TaskHandle::new(org.clone()).with_reporting_files(reporting.clone());
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
    let creating = tasks.clone();
    let intent = super::intent::IntentFiles::default();
    router.register(
        "task.create",
        Duration::from_secs(35),
        move |caller, params| {
            let tasks = creating.clone();
            let org = org.clone();
            let intent = intent.clone();
            let reporting = reporting.clone();
            let departments = departments.clone();
            async move {
                let mut p: CreateParams = parse_params(params)?;
                create(&p)?;
                let admission = intent.check(
                    &org, &caller, p.project_id.as_ref(), p.measure.clone(), &reporting,
                );
                let ((creator, project_id), directory) = if p.budget.is_none() && departments.is_configured() {
                    // Read the whole directory alongside admission, not after a
                    // potentially long authority check. Do not wait for the scan
                    // when admission refuses or produces an unplaced task.
                    let scan = departments.read();
                    tokio::pin!(admission, scan);
                    tokio::select! {
                        biased;
                        admitted = &mut admission => {
                            let admitted = admitted?;
                            let directory = if admitted.1.is_some() { Some(scan.await) } else { None };
                            (admitted, directory)
                        }
                        directory = &mut scan => (admission.await?, Some(directory)),
                    }
                } else {
                    (admission.await?, None)
                };
                let (default_budget_from, warning) =
                    fill_default_budget(directory, project_id.as_ref(), &mut p.budget);
                let mutation = tasks
                    .mutate(caller, move |_| {
                        TaskCommand::Create(TaskCreated {
                            project_id,
                            title: p.title,
                            kind: p.kind,
                            creator,
                            parent: p.parent,
                            judges: p.judges,
                            measure: p.measure,
                            budget: p.budget,
                            files: p.files,
                            rests_on: p.rests_on,
                        })
                    })
                    .await?;
                encode(CreateResult { mutation, default_budget_from, warning })
            }
        },
    )?;
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
            known("what", p.what != TaskSubmission::Unknown)?;
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
            known("verdict", p.verdict != TaskVerdict::Unknown)?;
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
    mutation!(
        "task.carry_back",
        CarryBackParams,
        |p: &CarryBackParams| {
            for id in [&p.task_id, &p.fix_task, &p.review_task] {
                sequence(id)?;
            }
            if p.note.trim().is_empty() {
                return Err(TaskError::InvalidParams("note must not be blank".into()));
            }
            size("note", &p.note, 4096)
        },
        |p: CarryBackParams, by| TaskCommand::CarryBack {
            task_id: p.task_id,
            fix_task: p.fix_task,
            review_task: p.review_task,
            note: p.note,
            by,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::methods::{router, CallContext, CallerId, CallerPane, MethodServices};
    use crate::{eventlog::Envelope, org::Request, registry::RegistryHandle};
    use atm_org::{OrgCommand, OrgState};
    use serde_json::{json, Value};
    use std::{
        fs,
        future::{poll_fn, Future},
        path::PathBuf,
        pin::Pin,
        task::Poll,
    };
    use tokio::sync::{broadcast, mpsc, watch};
    use tokio_util::sync::CancellationToken;

    struct World {
        dir: tempfile::TempDir,
        org: OrgHandle,
        cancel: CancellationToken,
    }
    impl Drop for World {
        fn drop(&mut self) {
            self.cancel.cancel();
        }
    }
    impl World {
        async fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let cancel = CancellationToken::new();
            let org = OrgHandle::open(dir.path().join("state"), cancel.clone()).await;
            Self { dir, org, cancel }
        }

        async fn project(&self, name: &str) -> PathBuf {
            let root = self.dir.path().join(name);
            fs::create_dir_all(root.join(".atm/roles")).unwrap();
            fs::write(
                root.join(".atm/intent.md"),
                "## How we will know\n- owner\n",
            )
            .unwrap();
            self.org
                .execute(
                    OrgCommand::RegisterProject {
                        project_id: name.parse().unwrap(),
                        root: root.to_str().unwrap().into(),
                        move_root: false,
                    },
                    chrono::Utc::now(),
                )
                .await
                .unwrap();
            root
        }

        fn departments(&self) -> PathBuf {
            self.dir.path().join("departments")
        }

        fn department(&self, id: &str, project: &str, mode: &str, rule: &str) {
            let dir = self.departments();
            fs::create_dir_all(&dir).unwrap();
            fs::write(
                dir.join(format!("{id}.toml")),
                format!("title='{id}'\nprojects=['{project}']\n[hire]\nmode='{mode}'\n{rule}\n"),
            )
            .unwrap();
        }

        fn router(&self) -> MethodRouter {
            // No task method calls the registry; no fake responses are needed.
            let (commands, _receive) = mpsc::channel(1);
            let (events, _) = broadcast::channel(1);
            let mut services = MethodServices::new(RegistryHandle::new(commands, events));
            services.org = self.org.clone();
            services.departments_dir = Some(self.departments());
            router(services).unwrap()
        }
    }

    fn caller(pane: Option<&str>) -> CallContext {
        CallContext {
            client_id: CallerId("create-default-budget".into()),
            from_pane: pane.map(|pane| CallerPane(pane.into())),
        }
    }

    fn request() -> Value {
        json!({"title":"Work","measure":{"kind":"owner"}})
    }

    fn legacy_reply(id: &Value) -> Value {
        json!({"task_id":id,"changed":true,"notification":null})
    }

    fn only_create(
        log: &mut broadcast::Receiver<std::sync::Arc<Envelope>>,
        answer: &Value,
    ) -> Value {
        let event = log.try_recv().unwrap();
        assert_eq!(event.kind, "task.created");
        assert_eq!(answer["task_id"], format!("t{}", event.seq));
        assert!(matches!(
            log.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        event.body.clone()
    }

    async fn create_and_check(
        world: &World,
        router: &MethodRouter,
        pane: Option<&str>,
        params: Value,
        budget: Value,
    ) -> (Value, Value) {
        let mut log = world.org.committed.subscribe();
        let answer = router
            .call(caller(pane), "task.create", params)
            .await
            .unwrap();
        let event = only_create(&mut log, &answer);
        assert_eq!(event["budget"], budget, "persisted task.created: {event}");
        let received = router
            .call(
                caller(pane),
                "task.get",
                json!({"task_id":answer["task_id"]}),
            )
            .await
            .unwrap();
        assert_eq!(received["task"]["budget"], budget, "stored task");
        (answer, received)
    }

    // Break caught: using the caller's project, returning provenance without
    // persisting the effective budget, or blending a default into an explicit budget.
    #[tokio::test]
    async fn create_defaults_only_the_admitted_project() {
        let world = World::new().await;
        let alpha = world.project("alpha").await;
        let beta = world.project("beta").await;
        world.project("gamma").await;
        for (id, pane, role, manager) in [
            (
                "alpha/manager",
                "%1",
                alpha.join(".atm/roles/manager.md"),
                "owner",
            ),
            (
                "beta/worker",
                "%2",
                beta.join(".atm/roles/worker.md"),
                "alpha/manager",
            ),
        ] {
            fs::write(
                role,
                format!("+++\nharness='pi'\nmanager='{manager}'\n+++\n"),
            )
            .unwrap();
            world
                .org
                .execute(
                    OrgCommand::Adopt {
                        agent_id: id.parse().unwrap(),
                        pane: pane.into(),
                        session_id: None,
                    },
                    chrono::Utc::now(),
                )
                .await
                .unwrap();
        }
        world.department(
            "caller",
            "alpha",
            "enforce",
            "[spend]\ntask_budget={turns=1,cost={microdollars=0}}",
        );
        world.department(
            "tools",
            "beta",
            "report",
            "[spend]\ntask_budget={turns=8,cost={microdollars=500000}}",
        );
        world.department(
            "apps",
            "beta",
            "enforce",
            "[spend]\ntask_budget={turns=20,cost={microdollars=120000}}",
        );
        let router = world.router();
        let mut p = request();
        p["project_id"] = json!("beta");
        let budget = json!({"turns":8,"cost":{"microdollars":120000},"deadline":null});
        let (reply, got) =
            create_and_check(&world, &router, Some("%1"), p.clone(), budget.clone()).await;
        assert_eq!(
            reply,
            json!({"task_id":reply["task_id"],"changed":true,"notification":null,
            "default_budget_from":{"turns":"tools","cost":"apps"}})
        );
        assert_eq!(got["task"]["project_id"], "beta");
        assert_eq!(got["task"]["creator"], "alpha/manager");
        assert_eq!(
            got["budget_status"],
            json!({"crossed":[],"notification":null})
        );
        let id = reply["task_id"].clone();
        p["budget"] = Value::Null;
        let (reply, _) =
            create_and_check(&world, &router, Some("%1"), p.clone(), budget.clone()).await;
        assert_eq!(
            reply["default_budget_from"],
            json!({"turns":"tools","cost":"apps"})
        );
        for (explicit, stored) in [
            (
                json!({"turns":9}),
                json!({"turns":9,"cost":null,"deadline":null}),
            ),
            (json!({}), json!({"turns":null,"cost":null,"deadline":null})),
            (
                json!({"deadline":"2099-01-01T00:00:00Z"}),
                json!({"turns":null,"cost":null,"deadline":"2099-01-01T00:00:00Z"}),
            ),
        ] {
            p["budget"] = explicit;
            let (reply, _) = create_and_check(&world, &router, Some("%1"), p.clone(), stored).await;
            assert_eq!(reply, legacy_reply(&reply["task_id"]));
        }
        p["budget"] = json!({"cost":{"microdollars":-1}});
        let before = world.org.status().unwrap();
        let error = router
            .call(caller(Some("%1")), "task.create", p.clone())
            .await
            .unwrap_err()
            .0;
        assert!(
            error.message.contains("budget.cost") && error.message.contains("must not be negative"),
            "{error:?}"
        );
        assert_eq!(world.org.status().unwrap(), before);

        p["project_id"] = json!("gamma");
        p.as_object_mut().unwrap().remove("budget");
        let (reply, _) = create_and_check(&world, &router, None, p.clone(), Value::Null).await;
        assert_eq!(reply, legacy_reply(&reply["task_id"]));
        world.department("no-budget", "gamma", "report", "[spend]\nmodels=['*']");
        let (reply, _) = create_and_check(&world, &router, None, p, Value::Null).await;
        assert_eq!(reply, legacy_reply(&reply["task_id"]));
        fs::remove_dir_all(world.departments()).unwrap();
        let got = router
            .call(caller(None), "task.get", json!({"task_id":id}))
            .await
            .unwrap();
        assert_eq!(
            got["task"]["budget"], budget,
            "a default is a stored budget, not a live projection"
        );
        let mut p = request();
        p["project_id"] = json!("beta");
        let (reply, _) = create_and_check(&world, &router, None, p, Value::Null).await;
        assert_eq!(reply, legacy_reply(&reply["task_id"]));
    }

    fn router_on_lane(org: OrgHandle, departments: DepartmentFiles) -> MethodRouter {
        let mut router = MethodRouter::default();
        register_task_methods_with_reporting(&mut router, org, Default::default(), departments)
            .unwrap();
        router
    }

    fn assert_warning(answer: &Value) {
        assert_eq!(
            *answer,
            json!({"task_id":answer["task_id"],"changed":true,"notification":null,
            "warning":"department policy incomplete; no default task budget applied for alpha"})
        );
    }

    // Break caught: a failed scan suppresses the creation or treats the readable
    // subset as sufficient to compute a minimum.
    #[tokio::test]
    async fn create_without_default_on_incomplete_department_read() {
        let world = World::new().await;
        world.project("alpha").await;
        world.department("tools", "alpha", "report", "[spend]\ntask_budget={turns=8}");
        fs::write(world.departments().join("bad.toml"), "title=[\n").unwrap();
        let closed = DepartmentFiles::new(Some(world.departments()));
        closed.slot.close();
        for router in [
            world.router(),
            router_on_lane(world.org.clone(), closed.clone()),
        ] {
            let (reply, got) =
                create_and_check(&world, &router, None, request(), Value::Null).await;
            assert_warning(&reply);
            assert!(got.get("budget_status").is_none());
            let mut p = request();
            p["budget"] = json!({});
            let (reply, _) = create_and_check(
                &world,
                &router,
                None,
                p,
                json!({"turns":null,"cost":null,"deadline":null}),
            )
            .await;
            assert_eq!(reply, legacy_reply(&reply["task_id"]));
        }
        let disabled = DepartmentFiles::new(None);
        disabled.slot.close();
        let router = router_on_lane(world.org.clone(), disabled);
        let (reply, _) = create_and_check(&world, &router, None, request(), Value::Null).await;
        assert_eq!(reply, legacy_reply(&reply["task_id"]));
    }

    // Fence the first snapshot only; all mutation operations go to the real actor.
    fn gated_world(org: &OrgHandle) -> (OrgHandle, mpsc::Receiver<Request>) {
        let (requests, receive) = mpsc::channel(8);
        (
            OrgHandle {
                requests,
                ..org.clone()
            },
            receive,
        )
    }

    fn release_admission(inbox: &mut mpsc::Receiver<Request>, state: OrgState) {
        let Request::Snapshot(reply) = inbox.try_recv().unwrap() else {
            panic!("expected intent's policy snapshot");
        };
        reply.send(state).unwrap();
    }

    fn use_real_writer(mut inbox: mpsc::Receiver<Request>, org: &OrgHandle) {
        let requests = org.requests.clone();
        tokio::spawn(async move {
            while let Some(request) = inbox.recv().await {
                requests.send(request).await.unwrap();
            }
        });
    }

    async fn poll_once<F: Future>(mut call: Pin<&mut F>) -> Poll<F::Output> {
        poll_fn(|cx| Poll::Ready(call.as_mut().poll(cx))).await
    }

    // Break caught: the department read is first polled after intent (adding a
    // serial five-second budget), or a refused admission waits for the reader.
    #[tokio::test]
    async fn create_starts_department_read_while_intent_is_pending() {
        let world = World::new().await;
        let files = DepartmentFiles::new(Some(world.departments()));
        let held = files.slot.acquire().await.unwrap();
        let (org, mut inbox) = gated_world(&world.org);
        let router = router_on_lane(org, files.clone());
        let mut log = world.org.committed.subscribe();
        let mut p = request();
        p["project_id"] = json!("missing");
        let mut call = Box::pin(router.call(caller(None), "task.create", p));
        assert!(poll_once(call.as_mut()).await.is_pending());
        // The department future queued for the lane in the same poll as the
        // held intent snapshot. The semaphore reserves the slot for it on release.
        drop(held);
        assert!(
            matches!(
                files.slot.try_acquire(),
                Err(tokio::sync::TryAcquireError::NoPermits)
            ),
            "department reader did not enter while intent was pending"
        );
        release_admission(&mut inbox, OrgState::default());
        let Poll::Ready(Err(error)) = poll_once(call.as_mut()).await else {
            panic!("intent refusal must return without waiting for the scan");
        };
        assert_eq!(error.0.code, "not_found");
        assert!(
            files.slot.try_acquire().is_ok(),
            "refusal did not release the queued read"
        );
        assert!(matches!(
            log.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
    }

    // Break caught: a held department slot puts the mutation on the read's
    // deadline or returns a timeout instead of one unbudgeted committed task.
    #[tokio::test]
    async fn create_commits_once_after_department_slot_times_out() {
        let world = World::new().await;
        world.project("alpha").await;
        world.department("tools", "alpha", "report", "[spend]\ntask_budget={turns=8}");
        let admitted = world.org.snapshot().await.unwrap();
        let files = DepartmentFiles::new(Some(world.departments()));
        let _held = files.slot.acquire().await.unwrap();
        let (org, mut inbox) = gated_world(&world.org);
        let router = router_on_lane(org, files.clone());
        let mut log = world.org.committed.subscribe();
        let mut call = Box::pin(router.call(caller(None), "task.create", request()));
        // While the clock is paused, both operations wait on in-memory gates.
        // The snapshot reply is ready before advancing; intent file I/O is
        // polled only after resuming the real clock.
        tokio::time::pause();
        assert!(poll_once(call.as_mut()).await.is_pending());
        release_admission(&mut inbox, admitted);
        tokio::time::advance(Duration::from_secs(5)).await;
        tokio::time::resume();
        use_real_writer(inbox, &world.org);
        let answer = call.await.unwrap();
        assert_warning(&answer);
        assert_eq!(only_create(&mut log, &answer)["budget"], Value::Null);
        assert_eq!(world.org.snapshot().await.unwrap().tasks().count(), 1);
    }

    // Break caught: an explicit budget starts the optional scan, or an
    // unplaced admission keeps waiting on the department reader.
    #[tokio::test]
    async fn create_does_not_wait_for_departments_without_a_default_target() {
        let world = World::new().await;
        world.department("tools", "alpha", "report", "[spend]\ntask_budget={turns=8}");
        for (budget, stored) in [
            (None, Value::Null),
            (
                Some(json!({})),
                json!({"turns":null,"cost":null,"deadline":null}),
            ),
        ] {
            let files = DepartmentFiles::new(Some(world.departments()));
            let held = files.slot.acquire().await.unwrap();
            let (org, mut inbox) = gated_world(&world.org);
            let router = router_on_lane(org, files.clone());
            let mut log = world.org.committed.subscribe();
            let mut p = request();
            if let Some(value) = &budget {
                p["budget"] = value.clone();
            }
            let mut call = Box::pin(router.call(caller(None), "task.create", p));
            assert!(poll_once(call.as_mut()).await.is_pending());
            if budget.is_some() {
                drop(held);
                let held_again = files
                    .slot
                    .try_acquire()
                    .expect("explicit budget started a department read");
                release_admission(&mut inbox, OrgState::default());
                assert!(poll_once(call.as_mut()).await.is_pending());
                drop(held_again);
            } else {
                release_admission(&mut inbox, OrgState::default());
                assert!(poll_once(call.as_mut()).await.is_pending());
                drop(held);
                assert!(
                    files.slot.try_acquire().is_ok(),
                    "unplaced task still waiting for departments"
                );
            }
            use_real_writer(inbox, &world.org);
            let answer = call.await.unwrap();
            assert_eq!(answer, legacy_reply(&answer["task_id"]));
            let event = only_create(&mut log, &answer);
            assert!(event["project_id"].is_null());
            assert_eq!(event["budget"], stored);
        }
    }

    // Break caught: the router discards an admitted mutation result at 30s,
    // even though create's policy admission and mutation have separate budgets.
    #[tokio::test(start_paused = true)]
    async fn create_returns_admitted_mutation_past_the_old_router_deadline() {
        let (requests, mut receive) = mpsc::channel(8);
        let (_health, health) = watch::channel(Ok(0));
        let (committed, _) = broadcast::channel(8);
        let org = OrgHandle {
            requests,
            health,
            committed,
            reads: None,
        };
        let mut router = MethodRouter::default();
        register_task_methods(&mut router, org).unwrap();
        assert_eq!(
            router
                .names()
                .iter()
                .filter(|n| *n == "task.create")
                .count(),
            1
        );
        let call = tokio::spawn(async move {
            router
                .call(
                    CallContext {
                        client_id: CallerId("create-deadline".into()),
                        from_pane: None,
                    },
                    "task.create",
                    json!({"title":"Work","measure":{"kind":"owner"}}),
                )
                .await
        });
        let Request::Snapshot(reply) = receive.recv().await.unwrap() else {
            panic!("expected policy snapshot");
        };
        reply.send(OrgState::default()).unwrap();
        // The snapshot is ready before the scheduling delay. All I/O here is
        // in-memory; only the paused clock advances, never a real-time wait.
        tokio::time::advance(Duration::from_secs(31)).await;
        let Request::Snapshot(reply) = receive.recv().await.unwrap() else {
            panic!("expected mutation snapshot");
        };
        reply.send(OrgState::default()).unwrap();
        let Request::Execute {
            command,
            now,
            reply,
            ..
        } = receive.recv().await.unwrap()
        else {
            panic!("expected admitted create");
        };
        let OrgCommand::Task(TaskCommand::Create(created)) = *command else {
            panic!("expected task creation");
        };
        reply
            .send(Ok(crate::org::TaskCommit {
                before: None,
                events: vec![Envelope {
                    seq: 1,
                    ts: now,
                    kind: "task.created".into(),
                    body: serde_json::to_value(created).unwrap(),
                }],
            }))
            .unwrap();
        let result = call.await.unwrap().unwrap();
        assert_eq!(result["task_id"], "t1");
        assert_eq!(result["changed"], true);
    }
}
