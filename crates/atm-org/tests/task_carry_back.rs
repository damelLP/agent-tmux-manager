//! Synthetic policy and historical review evidence; no daemon or harness.
#![allow(clippy::unwrap_used, clippy::expect_used)] // test fixture helpers only
use atm_org::*;
use serde_json::json;

mod support;
#[path = "support/tasks.rs"]
mod tasks;
use support::decision_world::World;
use tasks::*;

struct Case {
    rig: Rig,
    original: TaskId,
    fix: TaskId,
    review: TaskId,
}

impl Case {
    fn new(measure: TaskMeasure, fix_verdict: TaskVerdict, fix_what: TaskSubmission) -> Self {
        let mut rig = Rig::new();
        let original = rig.create(new_task(TaskKind::Work, measure, LEAD));
        rig.ok(assign(&original, BUILD, by(LEAD)));
        rig.ok(submit(
            &original,
            TaskSubmission::Result,
            "work.md",
            by(BUILD),
        ));
        rig.ok(verdict(
            &original,
            TaskVerdict::ChangesRequested,
            None,
            by(LEAD),
        ));
        let fix = rig.create(new_task(TaskKind::Work, reviewed(), LEAD));
        rig.ok(assign(&fix, OTHER, by(LEAD)));
        let review = rig.submitted_review(&fix);
        rig.ok(submit(&fix, fix_what, "fix.md", by(OTHER)));
        rig.ok(verdict(&fix, fix_verdict, Some(&review), by(LEAD)));
        rig.org(OrgCommand::Down {
            agent_id: agent(BUILD),
            reason: "seat closed".into(),
        })
        .unwrap();
        Self {
            rig,
            original,
            fix,
            review,
        }
    }

    fn reviewed() -> Self {
        Self::new(reviewed(), TaskVerdict::Accepted, TaskSubmission::Result)
    }

    fn command(&self, by: TaskActor) -> TaskCommand {
        TaskCommand::CarryBack {
            task_id: self.original.clone(),
            fix_task: self.fix.clone(),
            review_task: self.review.clone(),
            note: "  Covers the missing case.  ".into(),
            by,
        }
    }

    fn refused(&mut self, command: TaskCommand, diagnostic: &str) {
        let (state, seq, len) = (self.rig.state.clone(), self.rig.seq, self.rig.log.len());
        let error = self.rig.run(command).unwrap_err().to_string();
        assert!(error.contains(diagnostic), "{error:?} lacks {diagnostic:?}");
        assert_eq!(self.rig.state, state);
        assert_eq!((self.rig.seq, self.rig.log.len()), (seq, len));
    }
}

// Break caught: accepting rewrites the sources, loses review evidence on restart, or changes the journal shape.
#[test]
fn carry_back_records_one_verdict_for_the_original() {
    let mut c = Case::reviewed();
    let mut replay = OrgState::default();
    let mut stats = ReplayStats::default();
    for (seq, e) in &c.rig.log {
        stats
            .apply_parts(&mut replay, *seq, ts(*seq), e.kind(), &e.to_body())
            .unwrap();
    }
    assert_eq!(replay, c.rig.state);
    c.rig.state = replay;
    c.refused(
        verdict(&c.original, TaskVerdict::Accepted, None, by("owner")),
        "while ChangesRequested",
    );
    let expected_note = format!(
        "Accepted via fix {} (review {}); Covers the missing case.",
        c.fix, c.review
    );
    let fix = c.rig.task(&c.fix).clone();
    let review = c.rig.task(&c.review).clone();
    let cmd = c.command(as_lead(BOSS));
    let [e] = c
        .rig
        .ok(cmd.clone())
        .try_into()
        .expect("one original verdict");
    let e: OrgEvent = e;
    assert_eq!(e.kind(), "task.verdict");
    assert_eq!(
        e.to_body(),
        json!({"task_id":c.original,"verdict":"accepted","review_task":null,"note":expected_note,"by":BOSS})
    );
    let original = c.rig.task(&c.original);
    assert_eq!(original.state, TaskState::Accepted);
    assert_eq!(original.agent_id, Some(agent(BUILD)));
    assert_eq!(original.reason.as_deref(), Some(expected_note.as_str()));
    assert_eq!(c.rig.task(&c.fix), &fix);
    assert_eq!(c.rig.task(&c.review), &review);
    c.refused(cmd, "while Accepted");
}

// Break caught: an accepted review row, plan, or owner override masquerades as a reviewed fix.
#[test]
fn carry_back_requires_the_review_used_to_accept_the_result() {
    let no_evidence = "a distinct same-project fix and its accepted result review are required";
    for (v, what) in [
        (TaskVerdict::Accepted, TaskSubmission::Plan),
        (TaskVerdict::ChangesRequested, TaskSubmission::Result),
    ] {
        let mut c = Case::new(reviewed(), v, what);
        assert_eq!(c.rig.task(&c.review).state, TaskState::Accepted);
        c.refused(c.command(as_lead(BOSS)), no_evidence);
        c.rig.ok(submit(
            &c.fix,
            TaskSubmission::Result,
            "new-fix.md",
            by(OTHER),
        ));
        c.rig
            .ok(verdict(&c.fix, TaskVerdict::Accepted, None, by("owner")));
        assert_eq!(c.rig.task(&c.fix).state, TaskState::Accepted);
        c.refused(c.command(as_lead(BOSS)), no_evidence);
    }
    let mut c = Case::reviewed();
    let substitute = c.rig.submitted_review(&c.fix);
    c.rig.ok(verdict(
        &substitute,
        TaskVerdict::Accepted,
        None,
        by("owner"),
    ));
    let mut cmd = c.command(as_lead(BOSS));
    if let TaskCommand::CarryBack { review_task, .. } = &mut cmd {
        *review_task = substitute;
    }
    c.refused(cmd, no_evidence);
    let mut cmd = c.command(as_lead(BOSS));
    if let TaskCommand::CarryBack { fix_task, .. } = &mut cmd {
        *fix_task = c.original.clone();
    }
    c.refused(cmd, no_evidence);
    let mut cmd = c.command(as_lead(BOSS));
    if let TaskCommand::CarryBack { review_task, .. } = &mut cmd {
        *review_task = tid(999);
    }
    c.refused(cmd, "unknown task t999");
}

fn carry_back_after_redo(redo_fix: bool) -> (World, OrgCommand) {
    let mut w = World::new();
    let decision = w.request("alpha/lead", false);
    let original = w.create(None, None, None);
    let mut fix_body = new_task(TaskKind::Work, reviewed(), "owner");
    // Keep the fix in the original's project even if redo changes the assignee.
    fix_body.project_id = Some("beta".parse().unwrap());
    fix_body.rests_on = redo_fix.then_some(decision);
    let fix = tid(w.run(OrgCommand::Task(TaskCommand::Create(fix_body))));
    let review = w.create((!redo_fix).then_some(decision), None, Some(fix.clone()));
    w.away();
    w.take(decision);
    for (task, assignee) in [
        (&original, "beta/lead"),
        (&fix, "beta/lead"),
        (&review, "gamma/chief"),
    ] {
        w.assign(task, assignee).unwrap();
        w.run(OrgCommand::Task(submit(
            task,
            TaskSubmission::Result,
            "report.md",
            by(assignee),
        )));
    }
    w.run(OrgCommand::Task(verdict(
        &original,
        TaskVerdict::ChangesRequested,
        None,
        by("owner"),
    )));
    w.step(OrgCommand::Task(verdict(
        &fix,
        TaskVerdict::Accepted,
        Some(&review),
        by("owner"),
    )))
    .unwrap();
    w.run(OrgCommand::Down {
        agent_id: agent("beta/lead"),
        reason: "seat closed".into(),
    });
    let command = OrgCommand::Task(TaskCommand::CarryBack {
        task_id: original,
        fix_task: fix.clone(),
        review_task: review.clone(),
        note: "Covers the missing case.".into(),
        by: by("owner"),
    });
    w.state
        .decide(&command, w.now)
        .expect("the first review acceptance completes the link");

    let accepted_fix = w.state.task(&fix).unwrap().clone();
    w.answer(decision, "large").unwrap();
    if redo_fix {
        assert_eq!(
            w.state.decision_redo(decision).unwrap().required,
            Some(vec![fix.clone(), review.clone()])
        );
        w.assign(&fix, "gamma/chief").unwrap();
        w.run(OrgCommand::Task(submit(
            &fix,
            TaskSubmission::Result,
            "new-fix.md",
            by("gamma/chief"),
        )));
        w.run(OrgCommand::Task(verdict(
            &fix,
            TaskVerdict::Accepted,
            None,
            by("owner"),
        )));
    } else {
        assert_eq!(
            w.state.decision_redo(decision).unwrap().required,
            Some(vec![review.clone()])
        );
        assert_eq!(w.state.task(&fix), Some(&accepted_fix));
    }
    assert_eq!(w.state.task(&review).unwrap().state, TaskState::Open);
    w.assign(&review, "alpha/lead").unwrap();
    w.run(OrgCommand::Task(submit(
        &review,
        TaskSubmission::Result,
        "new-review.md",
        by("alpha/lead"),
    )));
    w.run(OrgCommand::Task(verdict(
        &review,
        TaskVerdict::Accepted,
        None,
        by("owner"),
    )));
    assert_eq!(w.state.task(&review).unwrap().state, TaskState::Accepted);
    (w, command)
}

fn assert_missing_review_evidence(result: Result<Vec<OrgEvent>, Refusal>) {
    let refusal = result.expect_err("superseded review evidence justified a carry-back");
    assert!(
        matches!(
            refusal,
            Refusal::Task(TaskRefusal::CannotCarryBack {
                reason: "a distinct same-project fix and its accepted result review are required",
                ..
            })
        ),
        "unexpected refusal: {refusal}"
    );
}

// Break caught: a replacement review result inherits evidence earned by the old result.
#[test]
fn carry_back_refuses_redone_review_after_live_fold() {
    let (w, command) = carry_back_after_redo(false);
    assert_missing_review_evidence(w.state.decide(&command, w.now));
}

#[test]
fn carry_back_refuses_redone_review_after_replay() {
    let (w, command) = carry_back_after_redo(false);
    let replay = w.replay();
    assert_eq!(replay, w.state);
    assert_missing_review_evidence(replay.decide(&command, w.now));
}

// Control: after redoing the fix, a standalone verdict must not recover its old link.
#[test]
fn carry_back_refuses_fix_reaccepted_without_review_after_redo() {
    let (w, command) = carry_back_after_redo(true);
    assert_missing_review_evidence(w.state.decide(&command, w.now));
    assert_missing_review_evidence(w.replay().decide(&command, w.now));
}

// Break caught: a second committed acceptance is mistaken for the first.
#[test]
fn carry_back_refuses_a_replayed_replacement_review_verdict() {
    let c = Case::reviewed();
    let mut replay = OrgState::default();
    let mut stats = ReplayStats::default();
    for (seq, event) in &c.rig.log {
        stats
            .apply_parts(&mut replay, *seq, ts(*seq), event.kind(), &event.to_body())
            .unwrap();
    }
    // Replay accepts a later verdict row without re-admitting it as a command.
    let seq = c.rig.seq + 1;
    stats
        .apply_parts(
            &mut replay,
            seq,
            ts(seq),
            "task.verdict",
            &json!({
                "task_id": c.review, "verdict": "accepted", "review_task": null,
                "note": "Replacement verdict", "by": "owner"
            }),
        )
        .unwrap();
    assert_eq!(replay.task(&c.review).unwrap().state, TaskState::Accepted);
    assert_missing_review_evidence(
        replay.decide(&OrgCommand::Task(c.command(as_lead(BOSS))), ts(seq)),
    );
}

// Break caught: a creator, stale grant or revived seat can use the exceptional transition.
#[test]
fn carry_back_checks_the_current_down_assignee_and_strict_manager() {
    let mut c = Case::reviewed();
    for actor in [by(LEAD), by(OTHER), as_lead(BUILD), by("system")] {
        c.refused(c.command(actor), "may not carry back");
    }
    let cmd = c.command(as_lead(BOSS));
    c.rig
        .org(OrgCommand::Adopt {
            agent_id: agent(BUILD),
            pane: "%10".into(),
            session_id: None,
        })
        .unwrap();
    c.refused(cmd.clone(), "it has no down assignee");
    c.rig.ok(assign(&c.original, OTHER, by(LEAD)));
    c.rig.ok(submit(
        &c.original,
        TaskSubmission::Result,
        "new.md",
        by(OTHER),
    ));
    c.rig.ok(verdict(
        &c.original,
        TaskVerdict::ChangesRequested,
        None,
        by(LEAD),
    ));
    c.rig
        .org(OrgCommand::Down {
            agent_id: agent(OTHER),
            reason: "seat closed".into(),
        })
        .unwrap();
    c.refused(cmd, "may not carry back");
    c.rig
        .ok(set(&c.original, TaskState::Cancelled, None, by(LEAD)));
    c.refused(c.command(by("owner")), "while Cancelled");
}

// Break caught: managerial carry-back downgrades an owner-judgement measure.
#[test]
fn carry_back_preserves_required_owner_judgement() {
    let mut c = Case::new(
        TaskMeasure::Owner,
        TaskVerdict::Accepted,
        TaskSubmission::Result,
    );
    c.refused(c.command(as_lead(BOSS)), "needs the owner's verdict");
    let cmd = c.command(by("owner"));
    c.rig.ok(cmd);
    assert_eq!(c.rig.task(&c.original).state, TaskState::Accepted);
}

// Break caught: same assignee project or creator substitutes for the recorded project.
#[test]
fn carry_back_uses_the_tasks_effective_project() {
    let mut c = Case::reviewed();
    let project_id: ProjectId = "q".parse().unwrap();
    c.rig
        .org(OrgCommand::RegisterProject {
            project_id: project_id.clone(),
            root: "/q".into(),
            move_root: false,
        })
        .unwrap();
    let mut body = new_task(TaskKind::Work, reviewed(), LEAD);
    body.project_id = Some(project_id);
    c.original = c.rig.create(body);
    c.rig.ok(assign(&c.original, BUILD, by(LEAD)));
    c.rig.ok(submit(
        &c.original,
        TaskSubmission::Result,
        "new.md",
        by(BUILD),
    ));
    c.rig.ok(verdict(
        &c.original,
        TaskVerdict::ChangesRequested,
        None,
        by(LEAD),
    ));
    c.refused(
        c.command(as_lead(BOSS)),
        "a distinct same-project fix and its accepted result review are required",
    );
}
