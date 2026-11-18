//! Synthetic task histories; project attribution is independent of assignment.
#![allow(clippy::unwrap_used, clippy::expect_used)] // test rig/helpers only
mod support;
#[path = "support/tasks.rs"]
mod tasks;
use atm_org::*;
use serde_json::{json, Value};
use tasks::*;

fn project_body(project: Value) -> TaskCreated {
    let mut body =
        serde_json::to_value(new_task(TaskKind::Work, TaskMeasure::Owner, LEAD)).unwrap();
    body["project_id"] = project;
    serde_json::from_value(body).unwrap()
}

fn world() -> Rig {
    let mut rig = Rig::new();
    for project in ["alpha", "beta", "gamma"] {
        rig.org(OrgCommand::RegisterProject {
            project_id: project.parse().unwrap(),
            root: format!("/{project}"),
            move_root: false,
        })
        .unwrap();
    }
    let mut role = support::role();
    role.name = "worker".into();
    rig.org(OrgCommand::Up {
        agent_id: agent("beta/worker"),
        role,
    })
    .unwrap();
    rig
}

// Break caught: dropping the new event field or deriving it from a later worker.
#[test]
fn recorded_project_survives_cross_project_assignment_reopen_and_cancel() {
    let mut rig = world();
    let task = rig.create(project_body(json!("alpha")));
    for command in [
        assign(&task, "beta/worker", by(LEAD)),
        submit(
            &task,
            TaskSubmission::Result,
            "result.md",
            by("beta/worker"),
        ),
        verdict(&task, TaskVerdict::ChangesRequested, None, by("owner")),
        set(&task, TaskState::Open, None, by(LEAD)),
        set(&task, TaskState::Cancelled, None, by(LEAD)),
    ] {
        rig.ok(command);
        assert_eq!(
            serde_json::to_value(rig.task(&task)).unwrap()["project_id"],
            "alpha"
        );
        assert_eq!(
            rig.task(&task).project(Some(&agent("beta/worker"))),
            Some(&"alpha".parse().unwrap())
        );
    }
    for cut in 0..=rig.log.len() {
        let mut state = OrgState::default();
        let mut stats = ReplayStats::default();
        for part in [&rig.log[..cut], &rig.log[cut..]] {
            for (seq, event) in part {
                stats
                    .apply_parts(&mut state, *seq, ts(*seq), event.kind(), &event.to_body())
                    .unwrap();
            }
        }
        assert_eq!(state, rig.state);
    }
}

// Break caught: accepting unknown references before fold, including on replay.
#[test]
fn unknown_or_malformed_projects_refuse_without_mutation() {
    let mut rig = world();
    let body = project_body(json!("missing"));
    let before = rig.state.clone();
    assert_eq!(
        rig.run(TaskCommand::Create(body.clone())),
        Err(Refusal::Invariant(ApplyError::UnknownProject(
            "missing".parse().unwrap()
        )))
    );
    assert!(rig
        .state
        .apply(999, ts(999), &OrgEvent::TaskCreated(body))
        .is_err());
    assert_eq!(rig.state, before);
    for project in [json!("../alpha"), json!(""), json!(3), json!({})] {
        let mut body =
            serde_json::to_value(new_task(TaskKind::Work, TaskMeasure::Owner, LEAD)).unwrap();
        body["project_id"] = project;
        assert!(OrgEvent::from_parts("task.created", &body).is_err());
        assert_eq!(rig.state, before);
    }
}

// Break caught: making the field required or serializing None into legacy bodies.
#[test]
fn old_bodies_remain_unrecorded_and_keep_their_serialization() {
    let mut rig = world();
    let body = new_task(TaskKind::Work, TaskMeasure::Owner, LEAD);
    let json = serde_json::to_value(&body).unwrap();
    assert!(json.get("project_id").is_none());
    let event = OrgEvent::from_parts("task.created", &json)
        .unwrap()
        .unwrap();
    assert_eq!(event.to_body(), json);
    let task = rig.create(body);
    assert!(serde_json::to_value(rig.task(&task))
        .unwrap()
        .get("project_id")
        .is_none());
    assert_eq!(rig.task(&task).project(None), Some(&"p".parse().unwrap()));
    assert_eq!(
        rig.task(&task).project(Some(&agent("beta/worker"))),
        Some(&"beta".parse().unwrap())
    );
    let owner = rig.create(new_task(TaskKind::Work, TaskMeasure::Owner, "owner"));
    assert_eq!(rig.task(&owner).project(None), None);
    assert_eq!(
        rig.task(&owner).project(Some(&agent("beta/worker"))),
        Some(&"beta".parse().unwrap())
    );
}

// Break caught: conflating project ownership with the existing parent/review links.
#[test]
fn parent_and_review_links_do_not_set_or_restrict_the_recorded_project() {
    let mut rig = world();
    let parent = rig.create(project_body(json!("alpha")));
    let mut child_body = project_body(json!("beta"));
    child_body.parent = Some(parent.clone());
    let child = rig.create(child_body);
    let mut review_body = project_body(json!("gamma"));
    review_body.kind = TaskKind::Review;
    review_body.judges = Some(child.clone());
    let review = rig.create(review_body);
    assert_eq!(rig.task(&child).parent, Some(parent));
    assert_eq!(rig.task(&review).judges, Some(child));
    assert_eq!(
        serde_json::to_value(rig.task(&review)).unwrap()["project_id"],
        "gamma"
    );
}

// Break caught: redo clears the recorded project along with the former assignee.
#[test]
fn decision_redo_preserves_recorded_project() {
    let mut w = support::decision_world::World::new();
    let d = w.request("alpha/lead", true);
    let mut body =
        serde_json::to_value(new_task(TaskKind::Work, TaskMeasure::Owner, "owner")).unwrap();
    body["project_id"] = json!("alpha");
    body["rests_on"] = json!(d);
    let task = tid(w.event(OrgEvent::TaskCreated(serde_json::from_value(body).unwrap())));
    w.assign(&task, "beta/lead").unwrap();
    w.away();
    w.take(d);
    w.answer(d, "large").unwrap();
    assert_eq!(w.state.task(&task).unwrap().state, TaskState::Open);
    assert_eq!(
        serde_json::to_value(w.state.task(&task).unwrap()).unwrap()["project_id"],
        "alpha"
    );
    assert_eq!(w.replay(), w.state);
}
