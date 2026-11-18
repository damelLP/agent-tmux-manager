//! Task test rig: one project, five agents and a serialized decide/apply loop.
//! Synthetic: hand-written commands; no harness, clock, daemon or tmux.
#![allow(dead_code)] // each test file uses its own part of the rig
use super::support::{role, time};
use atm_org::*;
use chrono::{DateTime, Duration, Utc};

/// Creates the tasks, so it has authority over them as their creator.
pub const LEAD: &str = "p/lead";
/// The agent the tasks are assigned to.
pub const BUILD: &str = "p/build";
/// An agent with no part in the task.
pub const OTHER: &str = "p/other";
/// A strict manager of BUILD in the supplied synthetic admission facts.
pub const BOSS: &str = "p/boss";
/// The agent review tasks are assigned to.
pub const REVIEWER: &str = "p/reviewer";
pub const AGENTS: [&str; 5] = [LEAD, BUILD, OTHER, BOSS, REVIEWER];

pub fn agent(text: &str) -> AgentId {
    text.parse().unwrap()
}
/// A caller without an assignee-scoped manager grant.
pub fn by(text: &str) -> TaskActor {
    TaskActor {
        who: text.parse().unwrap(),
        manager_of: None,
    }
}
/// Synthetic: the daemon checked that this caller strictly manages BUILD.
pub fn as_lead(text: &str) -> TaskActor {
    TaskActor {
        manager_of: Some(agent(BUILD)),
        ..by(text)
    }
}
pub fn tid(seq: u64) -> TaskId {
    format!("t{seq}").parse().unwrap()
}
pub fn ts(seq: u64) -> DateTime<Utc> {
    time() + Duration::seconds(seq as i64)
}
pub fn file(path: &str, kind: TaskFileKind) -> TaskFile {
    TaskFile {
        path: path.into(),
        kind,
        note: None,
    }
}
pub fn mechanical() -> TaskMeasure {
    TaskMeasure::Mechanical {
        command: "cargo test --workspace".into(),
    }
}
pub fn reviewed() -> TaskMeasure {
    TaskMeasure::Reviewed {
        rubric: "docs/rubric.md".into(),
    }
}
pub fn new_task(kind: TaskKind, measure: TaskMeasure, creator: &str) -> TaskCreated {
    TaskCreated {
        project_id: None,
        title: "Do the work".into(),
        kind,
        creator: creator.parse().unwrap(),
        parent: None,
        judges: None,
        measure,
        budget: None,
        files: vec![file("brief.md", TaskFileKind::Brief)],
        rests_on: None,
    }
}
pub fn set(task: &TaskId, state: TaskState, reason: Option<&str>, who: TaskActor) -> TaskCommand {
    TaskCommand::SetState {
        task_id: task.clone(),
        state,
        reason: reason.map(Into::into),
        by: who,
    }
}
pub fn assign(task: &TaskId, to: &str, who: TaskActor) -> TaskCommand {
    TaskCommand::Assign {
        task_id: task.clone(),
        agent_id: agent(to),
        by: who,
    }
}
pub fn submit(task: &TaskId, what: TaskSubmission, path: &str, who: TaskActor) -> TaskCommand {
    let kind = match what {
        TaskSubmission::Plan => TaskFileKind::Plan,
        TaskSubmission::Result | TaskSubmission::Unknown => TaskFileKind::Report,
    };
    TaskCommand::Submit {
        task_id: task.clone(),
        what,
        files: vec![file(path, kind)],
        by: who,
    }
}
pub fn start_review(task: &TaskId, review: &TaskId, who: TaskActor) -> TaskCommand {
    TaskCommand::StartReview {
        task_id: task.clone(),
        review_task: review.clone(),
        by: who,
    }
}
pub fn verdict(
    task: &TaskId,
    verdict: TaskVerdict,
    review: Option<&TaskId>,
    who: TaskActor,
) -> TaskCommand {
    TaskCommand::Verdict {
        task_id: task.clone(),
        verdict,
        review_task: review.cloned(),
        note: None,
        by: who,
    }
}

pub struct Rig {
    pub state: OrgState,
    pub seq: u64,
    /// Every committed event with its sequence number, in order.
    pub log: Vec<(u64, OrgEvent)>,
}
impl Rig {
    /// Project `p` with the five agents up.
    pub fn new() -> Self {
        let mut rig = Self {
            state: OrgState::default(),
            seq: 0,
            log: vec![],
        };
        rig.org(OrgCommand::RegisterProject {
            project_id: "p".parse().unwrap(),
            root: "/p".into(),
            move_root: false,
        })
        .unwrap();
        for name in AGENTS {
            let agent_id = agent(name);
            let mut role = role();
            role.name.clone_from(&agent_id.name);
            rig.org(OrgCommand::Up { agent_id, role }).unwrap();
        }
        rig
    }
    /// Decide, check that deciding changed nothing, then apply every event
    /// before the next decision, as the daemon's one actor does.
    pub fn org(&mut self, command: OrgCommand) -> Result<Vec<OrgEvent>, Refusal> {
        let before = self.state.clone();
        let result = self.state.decide(&command, time());
        assert_eq!(self.state, before, "decide mutated state for {command:?}");
        let events = result?;
        for event in &events {
            self.seq += 1;
            let applied = self.state.apply(self.seq, ts(self.seq), event);
            assert!(
                applied.is_ok(),
                "decide returned an inapplicable event: {command:?}, {event:?}: {applied:?}"
            );
            self.log.push((self.seq, event.clone()));
        }
        Ok(events)
    }
    pub fn run(&mut self, command: TaskCommand) -> Result<Vec<OrgEvent>, Refusal> {
        self.org(OrgCommand::Task(command))
    }
    pub fn ok(&mut self, command: TaskCommand) -> Vec<OrgEvent> {
        let shown = format!("{command:?}");
        self.run(command)
            .unwrap_or_else(|refusal| panic!("{shown} refused: {refusal}"))
    }
    pub fn task(&self, id: &TaskId) -> &Task {
        self.state.task(id).unwrap()
    }
    /// Create a task and return its id, `t` plus the sequence of its event.
    pub fn create(&mut self, body: TaskCreated) -> TaskId {
        assert_eq!(self.ok(TaskCommand::Create(body)).len(), 1);
        tid(self.seq)
    }
    /// A work task created by the lead, measured by a command.
    pub fn work_task(&mut self) -> TaskId {
        self.create(new_task(TaskKind::Work, mechanical(), LEAD))
    }
    /// A review task of `judged`, created by the lead.
    pub fn review_task(&mut self, judged: &TaskId) -> TaskId {
        let mut body = new_task(TaskKind::Review, reviewed(), LEAD);
        body.judges = Some(judged.clone());
        self.create(body)
    }
    /// A review task of `judged` that its reviewer has handed in.
    pub fn submitted_review(&mut self, judged: &TaskId) -> TaskId {
        let review = self.review_task(judged);
        self.ok(assign(&review, REVIEWER, by(LEAD)));
        self.ok(submit(
            &review,
            TaskSubmission::Result,
            "review.md",
            by(REVIEWER),
        ));
        review
    }
}
