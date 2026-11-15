//! Budget policy against plain task facts; no harness, daemon, clock or I/O.
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/mod.rs"]
mod support;
#[path = "support/tasks.rs"]
mod tasks;

use atm_core::Money;
use atm_org::{
    budget::{budget_crossings, budget_note, budget_status, budget_step, BudgetKind},
    *,
};
use chrono::Duration;
use proptest::prelude::*;
use serde_json::json;
use tasks::*;

fn limits(cost: Option<i64>, turns: Option<u32>) -> TaskBudget {
    TaskBudget {
        cost: cost.map(Money::from_microdollars),
        turns,
        deadline: None,
    }
}

fn budget_task(budget: Option<TaskBudget>) -> Task {
    let mut rig = Rig::new();
    let mut body = new_task(TaskKind::Work, mechanical(), LEAD);
    body.budget = budget;
    let id = rig.create(body);
    rig.task(&id).clone()
}

fn usage(cost: Option<i64>, turns: Option<u32>) -> TaskCost {
    TaskCost {
        cost: cost.map(Money::from_microdollars),
        turns,
        ..TaskCost::default()
    }
}

// Break caught: create persists a newly requested negative spend limit.
#[test]
fn negative_create_cost_refused_but_old_log_replays() {
    let mut rig = Rig::new();
    let mut body = new_task(TaskKind::Work, mechanical(), LEAD);
    body.budget = Some(limits(Some(-1), None));
    let before = rig.state.clone();
    assert!(matches!(
        rig.state
            .decide(&OrgCommand::Task(TaskCommand::Create(body.clone())), ts(0)),
        Err(Refusal::InvalidValue {
            field: "budget.cost",
            ..
        })
    ));
    assert_eq!(rig.state, before);

    // An old committed fact must still fold, without today's create validation.
    let fact = OrgEvent::TaskCreated(body);
    let fact = OrgEvent::from_parts(fact.kind(), &fact.to_body())
        .unwrap()
        .unwrap();
    rig.state.apply(12, ts(12), &fact).unwrap();
    assert_eq!(
        rig.state.task(&tid(12)).unwrap().budget,
        Some(limits(Some(-1), None))
    );
    assert_eq!(
        budget_crossings(
            rig.state.task(&tid(12)).unwrap(),
            &usage(Some(0), None),
            ts(12)
        ),
        vec![BudgetKind::Cost]
    );
}

// Break caught: treating zero, no limits or an already-past deadline as invalid.
#[test]
fn empty_zero_positive_and_past_deadline_budgets_are_valid() {
    for budget in [
        None,
        Some(limits(None, None)),
        Some(limits(Some(0), Some(0))),
        Some(limits(Some(i64::MAX), Some(u32::MAX))),
        Some(TaskBudget {
            deadline: Some(ts(0)),
            ..limits(None, None)
        }),
    ] {
        let mut rig = Rig::new();
        let mut body = new_task(TaskKind::Work, mechanical(), LEAD);
        body.budget = budget.clone();
        let id = rig.create(body);
        assert_eq!(rig.task(&id).budget, budget);
    }
}

// Break caught: >= flags at equality, or float conversion loses a microdollar.
#[test]
fn counter_equality_does_not_cross() {
    let task = budget_task(Some(limits(Some(1_000_000), Some(10))));
    for (cost, turns, want) in [
        (999_999, 9, vec![]),
        (1_000_000, 10, vec![]),
        (1_000_001, 10, vec![BudgetKind::Cost]),
        (1_000_000, 11, vec![BudgetKind::Turns]),
        (1_000_001, 11, vec![BudgetKind::Cost, BudgetKind::Turns]),
    ] {
        assert_eq!(
            budget_crossings(&task, &usage(Some(cost), Some(turns)), ts(0)),
            want
        );
    }
    let large = budget_task(Some(limits(Some(9_007_199_254_740_992), None)));
    assert_eq!(
        budget_crossings(&large, &usage(Some(9_007_199_254_740_993), None), ts(0)),
        vec![BudgetKind::Cost]
    );
    let largest = budget_task(Some(limits(Some(i64::MAX), Some(u32::MAX))));
    assert!(budget_crossings(&largest, &usage(Some(i64::MAX), Some(u32::MAX)), ts(0)).is_empty());
}

// Break caught: an omitted metric is fabricated as a positive/zero crossing.
#[test]
fn unknown_usage_is_not_zero() {
    let task = budget_task(Some(limits(Some(0), Some(0))));
    for reported in [usage(None, None), usage(Some(0), Some(0))] {
        assert!(budget_crossings(&task, &reported, ts(0)).is_empty());
    }
    assert_eq!(
        budget_crossings(&task, &usage(Some(1), None), ts(0)),
        vec![BudgetKind::Cost]
    );
    assert_eq!(
        budget_crossings(&task, &usage(None, Some(1)), ts(0)),
        vec![BudgetKind::Turns]
    );
    assert_eq!(
        budget_crossings(&task, &usage(Some(1), Some(1)), ts(0)),
        vec![BudgetKind::Cost, BudgetKind::Turns]
    );
}

// Break caught: a strict deadline comparison misses the exact due instant.
#[test]
fn deadline_is_due_at_equality() {
    let task = budget_task(Some(TaskBudget {
        deadline: Some(ts(20)),
        ..limits(None, None)
    }));
    for (now, want) in [
        (ts(20) - Duration::nanoseconds(1), vec![]),
        (ts(20), vec![BudgetKind::Deadline]),
        (
            ts(20) + Duration::nanoseconds(1),
            vec![BudgetKind::Deadline],
        ),
    ] {
        assert_eq!(budget_crossings(&task, &TaskCost::default(), now), want);
    }
}

// Break caught: restarting after an early finish invents an overdue deadline,
// or terminal filtering loses a crossing committed just before the finish.
#[test]
fn terminal_deadline_stops_at_changed_at() {
    let mut task = budget_task(Some(TaskBudget {
        deadline: Some(ts(20)),
        ..limits(Some(1_000_000), Some(10))
    }));
    for state in [TaskState::Accepted, TaskState::Cancelled] {
        task.state = state;
        for (end, want) in [
            (19, vec![]),
            (20, vec![BudgetKind::Deadline]),
            (21, vec![BudgetKind::Deadline]),
        ] {
            task.changed_at = ts(end);
            assert_eq!(budget_crossings(&task, &TaskCost::default(), ts(100)), want);
        }
        task.changed_at = ts(19);
        assert_eq!(
            budget_crossings(&task, &usage(Some(1_000_001), Some(11)), ts(100)),
            vec![BudgetKind::Cost, BudgetKind::Turns]
        );
    }
}

// Break caught: excluding open/waiting/review tasks, or acting on unknown states.
#[test]
fn whole_lifetime_includes_every_known_state_but_not_unknown() {
    let mut task = budget_task(Some(TaskBudget {
        deadline: Some(ts(20)),
        ..limits(Some(0), Some(0))
    }));
    task.changed_at = ts(20);
    for state in [
        TaskState::Open,
        TaskState::Assigned,
        TaskState::Working,
        TaskState::Waiting,
        TaskState::Blocked,
        TaskState::Submitted,
        TaskState::InReview,
        TaskState::Accepted,
        TaskState::ChangesRequested,
        TaskState::Cancelled,
    ] {
        task.state = state;
        assert_eq!(
            budget_crossings(&task, &usage(Some(1), Some(1)), ts(20)),
            vec![BudgetKind::Cost, BudgetKind::Turns, BudgetKind::Deadline],
            "{state:?}"
        );
    }
    task.state = TaskState::Unknown;
    assert!(budget_crossings(&task, &usage(Some(1), Some(1)), ts(100)).is_empty());
    assert!(budget_note(&task).is_none());
}

// Break caught: absent limits create automatic budget notes.
#[test]
fn no_limits_means_no_crossing_or_note() {
    for budget in [None, Some(limits(None, None))] {
        let task = budget_task(budget);
        assert!(
            budget_crossings(&task, &usage(Some(i64::MAX), Some(u32::MAX)), ts(100)).is_empty()
        );
        assert!(budget_note(&task).is_none());
    }
}

// Break caught: changing titles/state/limits changes the durable note identity,
// or several limits produce dimension-specific identities and extra alerts.
#[test]
fn all_limits_one_canonical_note() {
    let mut task = budget_task(Some(TaskBudget {
        deadline: Some(ts(20)),
        ..limits(Some(0), Some(0))
    }));
    let expected = json!({
        "from": "system", "to": "p/lead", "kind": "note",
        "body": "Task t7 crossed a budget limit.\nInspect limits and current usage: atm task get t7\nStage 1 only flags budgets; nothing was stopped.",
        "task": "t7", "attachments": []
    });
    assert_eq!(
        serde_json::to_value(budget_note(&task).unwrap()).unwrap(),
        expected
    );
    task.title = "A renamed task".into();
    task.state = TaskState::Working;
    task.changed_at = ts(100);
    task.budget = Some(limits(None, Some(2)));
    assert_eq!(
        serde_json::to_value(budget_note(&task).unwrap()).unwrap(),
        expected
    );
}

// Break caught: system creators become invalid recipients, or self-creators
// are silently rerouted to a manager rather than the actual task creator.
#[test]
fn notes_address_the_creator_with_system_falling_back_to_owner() {
    for (creator, want) in [
        ("owner", "owner"),
        ("p/lead", "p/lead"),
        ("system", "owner"),
    ] {
        let mut task = budget_task(Some(limits(None, Some(1))));
        task.creator = creator.parse().unwrap();
        task.agent_id = Some(agent(LEAD));
        let note = budget_note(&task).unwrap();
        assert_eq!(note.to, want.parse().unwrap());
        assert_eq!(note.from, Address::System);
        assert_eq!(note.kind, MessageKind::Note);
        assert_eq!(note.task, Some(tid(7)));
        assert!(note.attachments.is_empty());
    }
}

// Break caught: new validation changes the old budget JSON/replay codec.
#[test]
fn budget_body_roundtrips_unchanged() {
    let encoded = json!({
        "cost": {"microdollars": 1_000_001}, "turns": 10,
        "deadline": "2026-01-15T12:00:00Z"
    });
    let decoded: TaskBudget = serde_json::from_value(encoded.clone()).unwrap();
    assert_eq!(decoded.cost, Some(Money::from_microdollars(1_000_001)));
    assert_eq!(serde_json::to_value(decoded).unwrap(), encoded);
    let partial: TaskBudget =
        serde_json::from_value(json!({"turns": 10, "future": "skip"})).unwrap();
    assert_eq!(partial, limits(None, Some(10)));
}

// Break caught: no timer for the earliest future deadline, or rearming after
// a cost crossing already queued the task's single canonical notification.
#[test]
fn budget_step_selects_earliest_unnotified_deadline_and_supplied_usage() {
    let mut rig = Rig::new();
    let mut body = new_task(TaskKind::Work, mechanical(), LEAD);
    body.budget = Some(TaskBudget {
        deadline: Some(ts(30)),
        ..limits(Some(0), None)
    });
    let first = rig.create(body.clone());
    body.budget.as_mut().unwrap().deadline = Some(ts(20));
    let second = rig.create(body);
    let step = budget_step(&rig.state, ts(0), |_| None);
    assert!(step.notes.is_empty());
    assert_eq!(step.wake_at, Some(ts(20)));
    let due = budget_step(&rig.state, ts(20), |_| None);
    assert_eq!(due.notes.len(), 1);
    assert_eq!(due.notes.first().unwrap().task, Some(second));
    assert_eq!(due.wake_at, Some(ts(30)));
    let charged = budget_step(&rig.state, ts(0), |id| {
        (id == &first).then(|| usage(Some(1), None))
    });
    assert_eq!(charged.notes.len(), 1);
    assert_eq!(charged.notes.first().unwrap().task, Some(first));
    assert_eq!(charged.wake_at, Some(ts(20)));
}

// Break caught: delivery/read/failure or state changes rearm budget alerts, or
// an ordinary note suppresses the task's required budget notification.
#[test]
fn note_survives_state_and_delivery_changes_and_replay() {
    for outcome in ["queued", "delivered", "read", "failed"] {
        let mut rig = Rig::new();
        let mut body = new_task(TaskKind::Work, mechanical(), LEAD);
        body.budget = Some(TaskBudget {
            deadline: Some(ts(20)),
            ..limits(None, None)
        });
        let id = rig.create(body);
        let mut ordinary = budget_note(rig.task(&id)).unwrap();
        ordinary.body = "An unrelated note".into();
        rig.org(OrgCommand::SendMessage(ordinary)).unwrap();
        let due = budget_step(&rig.state, ts(20), |_| None);
        assert_eq!(due.notes.len(), 1, "ordinary note is not the latch");
        rig.org(OrgCommand::SendMessage(due.notes.first().unwrap().clone()))
            .unwrap();
        let message_id = MessageId::new(rig.seq);
        if matches!(outcome, "delivered" | "read") {
            rig.org(OrgCommand::Delivered {
                message_id,
                session_id: atm_core::SessionId::new("s1"),
                confirmation: "observed".into(),
            })
            .unwrap();
        }
        if outcome == "read" {
            rig.org(OrgCommand::ReadMessages {
                who: LEAD.parse().unwrap(),
                message_ids: vec![message_id],
            })
            .unwrap();
        }
        if outcome == "failed" {
            rig.org(OrgCommand::DeliveryFailed {
                message_id,
                reason: "gave up".into(),
                final_failure: true,
            })
            .unwrap();
        }
        rig.ok(assign(&id, BUILD, by(LEAD)));
        let mut replayed = OrgState::default();
        for (seq, event) in &rig.log {
            replayed.apply(*seq, ts(*seq), event).unwrap();
        }
        let status = budget_status(
            &replayed,
            replayed.task(&id).unwrap(),
            &TaskCost::default(),
            ts(100),
        )
        .unwrap();
        assert_eq!(status.crossed, vec![BudgetKind::Deadline]);
        assert_eq!(status.notification, Some(message_id));
        let next = budget_step(&replayed, ts(100), |_| None);
        assert!(next.notes.is_empty(), "{outcome} must retain the latch");
        assert_eq!(next.wake_at, None);
    }
}

proptest! {
    #![proptest_config(ProptestConfig { failure_persistence: None, ..ProptestConfig::default() })]
    // Break caught: increasing usage/time makes a previous crossing disappear.
    #[test]
    fn increasing_usage_and_time_preserves_crossings(
        limit in 0_i64..i64::MAX,
        before in 0_i64..i64::MAX,
        extra in 0_i64..i64::MAX,
        turns in any::<u32>(),
        turn_limit in any::<u32>(),
    ) {
        let task = budget_task(Some(TaskBudget {
            deadline: Some(ts(20)),
            ..limits(Some(limit), Some(turn_limit))
        }));
        let earlier = budget_crossings(&task, &usage(Some(before), Some(turns)), ts(19));
        let later = budget_crossings(&task, &usage(Some(before.saturating_add(extra)), Some(turns.saturating_add(1))), ts(20));
        for kind in earlier {
            prop_assert!(later.contains(&kind));
        }
        prop_assert!(later.contains(&BudgetKind::Deadline));
    }
}
