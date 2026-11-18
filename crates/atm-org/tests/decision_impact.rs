//! Synthetic histories for recorded-link exposure; no harness or filesystem.
use atm_org::*;
mod support;
use support::decision_world::{owner, World};

// Break caught: reopening/cancelling refunds a cap slot or resuming spends twice.
#[test]
fn cap_two_reopen_does_not_refund_an_id() {
    for cancelled in [false, true] {
        let mut w = World::new();
        w.away();
        let d = w.request("beta/lead", true);
        w.take(d);
        let a = w.create(Some(d), None, None);
        let b = w.create(Some(d), None, None);
        w.assign(&a, "gamma/chief").unwrap();
        w.set(
            &a,
            if cancelled {
                TaskState::Cancelled
            } else {
                TaskState::Open
            },
        );
        assert!(
            matches!(w.assign(&b, "alpha/lead"), Err(Refusal::Away(_))),
            "new ID must stay capped after cancellation/reopen"
        );
        if !cancelled {
            assert_eq!(w.assign(&a, "alpha/lead").unwrap().len(), 1);
        } else {
            assert!(w.assign(&a, "alpha/lead").is_err());
        }
        assert_eq!(w.replay(), w.state);
    }
}

// Break caught: only the direct decision is checked, hiding a stricter ancestor.
#[test]
fn inherited_task_checks_every_provisional_cap() {
    let mut w = World::new();
    w.away();
    let strict = w.request("alpha/lead", true);
    let permissive = w.request("gamma/chief", true);
    w.take(strict);
    w.take(permissive);
    let parent = w.create(Some(strict), None, None);
    let child = w.create(Some(permissive), Some(parent.clone()), None);
    let review = w.create(None, None, Some(parent));
    for task in [&child, &review] {
        assert!(
            matches!(w.assign(task, "beta/lead"), Err(Refusal::Away(AwayRefusal::Held { decision_id, cap: 1, .. })) if decision_id == strict)
        );
    }
}

// Break caught: selecting the assignee's project rather than the requester's cap.
#[test]
fn cap_uses_requester_project_across_three_projects() {
    let mut w = World::new();
    w.away();
    for (who, allowed, cap) in [
        ("alpha/lead", 0, 1),
        ("beta/lead", 1, 2),
        ("gamma/chief", 2, 3),
    ] {
        let d = w.request(who, true);
        w.take(d);
        for _ in 0..allowed {
            let t = w.create(Some(d), None, None);
            w.assign(&t, "alpha/lead").unwrap();
        }
        let t = w.create(Some(d), None, None);
        assert!(
            matches!(w.assign(&t, "beta/lead"), Err(Refusal::Away(AwayRefusal::Held { cap: actual, .. })) if actual == cap)
        );
    }
    assert_eq!(w.replay(), w.state);
}

// Break caught: direct-only traversal, duplicate paths, or lexical task ordering.
#[test]
fn impact_includes_descendants_and_judging_reviews_once() {
    let mut w = World::new();
    let d = w.request("alpha/lead", true);
    let root = w.create(Some(d), None, None);
    let sibling = w.create(None, None, None);
    let child = w.create(None, Some(root.clone()), None);
    let review = w.create(None, Some(child.clone()), Some(root.clone()));
    w.assign(&root, "alpha/lead").unwrap();
    w.assign(&review, "gamma/chief").unwrap();
    let impact = decision_impact(&w.state, d);
    assert_eq!(impact.started, vec![root.clone(), review]);
    assert_eq!(impact.not_started, vec![child]);
    assert!(!impact.contains(&sibling));
    assert!(impact.contains(&root));
    assert_eq!(impact.started_count(), 2);
    assert_eq!(w.replay(), w.state);
}

// Break caught: cancelled parents prune a live child, or current Open loses history.
#[test]
fn cancelled_parent_does_not_hide_live_child() {
    let mut w = World::new();
    let d = w.request("alpha/lead", true);
    let root = w.create(Some(d), None, None);
    let child = w.create(None, Some(root.clone()), None);
    w.assign(&root, "alpha/lead").unwrap();
    w.assign(&child, "beta/lead").unwrap();
    w.set(&root, TaskState::Cancelled);
    w.set(&child, TaskState::Open);
    assert!(w.state.task_was_assigned(&child));
    let impact = decision_impact(&w.state, d);
    assert_eq!(impact.started, vec![child]);
    assert_eq!(impact.cancelled, vec![root]);
    assert_eq!(w.replay(), w.state);
}

// Break caught: an unknown live state is assumed accepted, or unlinked work is guessed.
#[test]
fn open_reopened_accepted_and_unknown_are_distinguished() {
    let mut w = World::new();
    let d = w.request("alpha/lead", true);
    let unstarted = w.create(Some(d), None, None);
    let accepted = w.create(Some(d), None, None);
    let unknown = w.create(Some(d), None, None);
    let unlinked = w.create(None, None, None);
    for task in [&accepted, &unknown, &unlinked] {
        w.assign(task, "alpha/lead").unwrap();
    }
    for (task, state) in [
        (&accepted, TaskState::Accepted),
        (&unknown, TaskState::Unknown),
    ] {
        w.event(OrgEvent::TaskStateChanged(TaskStateChanged {
            task_id: task.clone(),
            state,
            reason: None,
            by: Address::Owner,
        }));
    }
    let impact = decision_impact(&w.state, d);
    assert_eq!(impact.started, vec![accepted, unknown.clone()]);
    assert_eq!(impact.not_started, vec![unstarted]);
    assert_eq!(impact.unknown, vec![unknown]);
    assert!(!impact.contains(&unlinked), "no original task is guessed");
}

// Break caught: the new overrule exception accidentally relaxes ordinary task policy.
#[test]
fn ordinary_accepted_task_cannot_be_reopened() {
    let mut w = World::new();
    let t = w.create(None, None, None);
    w.event(OrgEvent::TaskStateChanged(TaskStateChanged {
        task_id: t.clone(),
        state: TaskState::Accepted,
        reason: None,
        by: Address::Owner,
    }));
    assert!(w
        .step(OrgCommand::Task(TaskCommand::SetState {
            task_id: t,
            state: TaskState::Open,
            reason: None,
            by: owner()
        }))
        .is_err());
}
