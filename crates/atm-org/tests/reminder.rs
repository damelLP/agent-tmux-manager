//! The reminder policy, one test per rule of `atm_org::reminder`'s tables,
//! with the edges on both sides of every limit. Time is always an argument.
//! Synthetic: hand-written commands through the task rig; no harness, clock,
//! daemon or tmux.
#![allow(clippy::unwrap_used, clippy::expect_used)] // test helpers may panic
mod support;
#[path = "support/tasks.rs"]
mod tasks;
use atm_org::reminder::{reminder_step, ReminderSettings, ReminderStep};
use atm_org::*;
use chrono::{DateTime, TimeDelta, Utc};
use proptest::prelude::*;
use std::time::Duration;
use tasks::*;
use TaskState as S;

const DELAY: i64 = 60;

fn settings() -> ReminderSettings {
    ReminderSettings {
        delay: Duration::from_secs(DELAY.unsigned_abs()),
    }
}

/// `secs` on the tests' own clock, which starts after every event of the rig.
fn at(secs: i64) -> DateTime<Utc> {
    support::time() + TimeDelta::seconds(10_000 + secs)
}

/// What the policy says for the builder, whose last turn ended at `ended_at`.
fn step(rig: &Rig, ended_at: DateTime<Utc>, now: DateTime<Utc>) -> ReminderStep {
    reminder_step(&rig.state, &agent(BUILD), ended_at, &settings(), now)
}

/// The policy's answer once the wait after `ended_at` is over.
fn due(rig: &Rig, ended_at: DateTime<Utc>) -> ReminderStep {
    step(rig, ended_at, ended_at + TimeDelta::seconds(DELAY))
}

fn act(remind: &[&TaskId], flag: &[&TaskId]) -> ReminderStep {
    ReminderStep::Act {
        remind: remind.iter().map(|id| (*id).clone()).collect(),
        flag: flag.iter().map(|id| (*id).clone()).collect(),
    }
}

/// Commit one task command at `now`, as the daemon's one actor does. Returns
/// how many events it wrote.
fn commit(rig: &mut Rig, command: TaskCommand, now: DateTime<Utc>) -> Result<usize, Refusal> {
    let events = rig.state.decide(&OrgCommand::Task(command), now)?;
    for event in &events {
        rig.seq += 1;
        rig.state.apply(rig.seq, now, event).unwrap();
    }
    Ok(events.len())
}

fn reminded(task: &TaskId) -> TaskCommand {
    TaskCommand::Reminded {
        task_id: task.clone(),
        agent_id: agent(BUILD),
    }
}

fn flagged(task: &TaskId) -> TaskCommand {
    TaskCommand::Flagged {
        task_id: task.clone(),
        to: LEAD.parse().unwrap(),
    }
}

/// A work task of the lead's, assigned to the builder.
fn assigned(rig: &mut Rig) -> TaskId {
    let task = rig.work_task();
    rig.ok(assign(&task, BUILD, by(LEAD)));
    task
}

/// A message to `to`, queued.
fn mail(rig: &mut Rig, to: &str) -> MessageId {
    rig.org(OrgCommand::SendMessage(support::content(Address::Agent(
        agent(to),
    ))))
    .unwrap();
    MessageId::new(rig.seq)
}

fn attempt(rig: &mut Rig, message_id: MessageId) {
    rig.org(OrgCommand::DeliveryAttempted {
        message_id,
        attempt: 1,
        pane: "%1".into(),
    })
    .unwrap();
}

#[test]
fn the_default_delay_is_sixty_seconds() {
    assert_eq!(
        ReminderSettings::default().delay,
        Duration::from_secs(60),
        "choice 7 of the stage 1 plan"
    );
}

#[test]
fn an_agent_without_a_task_is_left_alone() {
    let mut rig = Rig::new();
    // Tasks exist, but none is the builder's.
    rig.work_task();
    let other = rig.work_task();
    rig.ok(assign(&other, OTHER, by(LEAD)));
    assert_eq!(due(&rig, at(0)), ReminderStep::Nothing);
    assert_eq!(
        reminder_step(
            &rig.state,
            &"p/nobody".parse().unwrap(),
            at(0),
            &settings(),
            at(DELAY)
        ),
        ReminderStep::Nothing
    );
}

#[test]
fn the_wait_ends_at_the_delay_and_not_a_nanosecond_before() {
    let mut rig = Rig::new();
    let task = assigned(&mut rig);
    let limit = at(DELAY);
    assert_eq!(step(&rig, at(0), at(0)), ReminderStep::WaitUntil(limit));
    assert_eq!(
        step(&rig, at(0), limit - TimeDelta::nanoseconds(1)),
        ReminderStep::WaitUntil(limit)
    );
    assert_eq!(step(&rig, at(0), limit), act(&[&task], &[]));
    assert_eq!(step(&rig, at(0), at(10 * DELAY)), act(&[&task], &[]));
    // A clock that stepped back waits for the same limit.
    assert_eq!(step(&rig, at(0), at(-5)), ReminderStep::WaitUntil(limit));
}

/// Every state, with the builder as the task's agent wherever the state has
/// one: only the three that say "the agent is acting" are silent.
#[test]
fn only_a_task_its_agent_is_acting_on_is_due() {
    type Reach = fn(&mut Rig, &TaskId);
    let submit_result: Reach = |rig, task| {
        rig.ok(submit(task, TaskSubmission::Result, "r.md", by(BUILD)));
    };
    let states: [(S, bool, Reach); 10] = [
        (S::Open, false, |rig, task| {
            rig.ok(set(task, S::Open, None, by(LEAD)));
        }),
        (S::Assigned, true, |_, _| {}),
        (S::Working, true, |rig, task| {
            rig.ok(set(task, S::Working, None, by(BUILD)));
        }),
        (S::Waiting, false, |rig, task| {
            rig.ok(set(task, S::Waiting, Some("an answer"), by(BUILD)));
        }),
        (S::Blocked, false, |rig, task| {
            rig.ok(set(task, S::Blocked, Some("no access"), by(BUILD)));
        }),
        (S::Submitted, false, submit_result),
        (S::InReview, false, |rig, task| {
            rig.ok(submit(task, TaskSubmission::Result, "r.md", by(BUILD)));
            let review = rig.review_task(task);
            rig.ok(assign(&review, REVIEWER, by(LEAD)));
            rig.ok(start_review(task, &review, by(LEAD)));
        }),
        (S::ChangesRequested, true, |rig, task| {
            rig.ok(submit(task, TaskSubmission::Result, "r.md", by(BUILD)));
            rig.ok(verdict(task, TaskVerdict::ChangesRequested, None, by(LEAD)));
        }),
        (S::Accepted, false, |rig, task| {
            rig.ok(submit(task, TaskSubmission::Result, "r.md", by(BUILD)));
            rig.ok(verdict(task, TaskVerdict::Accepted, None, by(LEAD)));
        }),
        (S::Cancelled, false, |rig, task| {
            rig.ok(set(task, S::Cancelled, None, by(LEAD)));
        }),
    ];
    for (state, silent, reach) in states {
        let mut rig = Rig::new();
        let task = assigned(&mut rig);
        reach(&mut rig, &task);
        assert_eq!(rig.task(&task).state, state);
        let expected = if silent {
            act(&[&task], &[])
        } else {
            ReminderStep::Nothing
        };
        assert_eq!(due(&rig, at(0)), expected, "{state:?}");
    }
}

#[test]
fn a_task_whose_state_began_after_the_turn_end_has_had_no_turn() {
    let mut rig = Rig::new();
    let task = assigned(&mut rig);
    let began = rig.task(&task).changed_at;
    assert_eq!(
        due(&rig, began - TimeDelta::nanoseconds(1)),
        ReminderStep::Nothing
    );
    assert_eq!(due(&rig, began), act(&[&task], &[]));
}

#[test]
fn a_reminded_task_is_flagged_by_a_later_silent_turn_end_only() {
    let mut rig = Rig::new();
    let task = assigned(&mut rig);
    // The reminder is recorded when the wait after the first turn end is over.
    commit(&mut rig, reminded(&task), at(DELAY)).unwrap();
    // The turn end that led to the reminder, and one at the same instant.
    assert_eq!(due(&rig, at(0)), ReminderStep::Nothing);
    assert_eq!(due(&rig, at(DELAY)), ReminderStep::Nothing);
    // The next turn ends silent: the same wait, then the flag.
    let next = at(DELAY) + TimeDelta::nanoseconds(1);
    assert_eq!(
        step(&rig, next, next),
        ReminderStep::WaitUntil(next + TimeDelta::seconds(DELAY))
    );
    assert_eq!(due(&rig, next), act(&[], &[&task]));
}

#[test]
fn a_flagged_task_is_left_alone() {
    let mut rig = Rig::new();
    let task = assigned(&mut rig);
    commit(&mut rig, reminded(&task), at(60)).unwrap();
    commit(&mut rig, flagged(&task), at(200)).unwrap();
    assert_eq!(due(&rig, at(300)), ReminderStep::Nothing);
    assert_eq!(due(&rig, at(3000)), ReminderStep::Nothing);
}

#[test]
fn a_change_of_state_starts_over() {
    let mut rig = Rig::new();
    let task = assigned(&mut rig);
    commit(&mut rig, reminded(&task), at(60)).unwrap();
    // The reminder turn: the agent takes the task up and stops again.
    let moved = commit(&mut rig, set(&task, S::Working, None, by(BUILD)), at(70)).unwrap();
    assert_eq!(moved, 1);
    assert_eq!(due(&rig, at(80)), act(&[&task], &[]));
}

#[test]
fn asking_for_the_same_state_again_keeps_the_latch() {
    let mut rig = Rig::new();
    let task = assigned(&mut rig);
    commit(&mut rig, set(&task, S::Working, None, by(BUILD)), at(0)).unwrap();
    commit(&mut rig, reminded(&task), at(70)).unwrap();
    let again = commit(&mut rig, set(&task, S::Working, None, by(BUILD)), at(80)).unwrap();
    assert_eq!(again, 0);
    assert_eq!(due(&rig, at(90)), act(&[], &[&task]));
}

#[test]
fn nothing_is_decided_while_mail_to_the_agent_is_on_its_way() {
    let mut rig = Rig::new();
    let task = assigned(&mut rig);
    let now = at(DELAY + 7);
    let look_again = ReminderStep::WaitUntil(now + TimeDelta::seconds(DELAY));

    let message = mail(&mut rig, BUILD);
    assert_eq!(step(&rig, at(0), now), look_again, "queued");
    // Before the delay the turn end's own limit comes first.
    assert_eq!(step(&rig, at(0), at(1)), ReminderStep::WaitUntil(at(DELAY)));

    attempt(&mut rig, message);
    assert_eq!(step(&rig, at(0), now), look_again, "delivering");

    rig.org(OrgCommand::DeliveryFailed {
        message_id: message,
        reason: "no receipt".into(),
        final_failure: false,
    })
    .unwrap();
    assert_eq!(step(&rig, at(0), now), look_again, "queued for a retry");

    rig.org(OrgCommand::Delivered {
        message_id: message,
        session_id: "s1".into(),
        confirmation: "prompt token".into(),
    })
    .unwrap();
    assert_eq!(step(&rig, at(0), now), act(&[&task], &[]), "delivered");
}

#[test]
fn mail_that_failed_for_good_or_goes_elsewhere_holds_nothing() {
    let mut rig = Rig::new();
    let task = assigned(&mut rig);
    let failed = mail(&mut rig, BUILD);
    rig.org(OrgCommand::DeliveryFailed {
        message_id: failed,
        reason: "given up".into(),
        final_failure: true,
    })
    .unwrap();
    mail(&mut rig, OTHER);
    // The builder's own report to the lead is not mail to the builder.
    mail(&mut rig, LEAD);
    assert_eq!(due(&rig, at(0)), act(&[&task], &[]));
}

#[test]
fn mail_holds_a_flag_too_and_no_task_due_comes_first() {
    let mut rig = Rig::new();
    let task = assigned(&mut rig);
    commit(&mut rig, reminded(&task), at(60)).unwrap();
    mail(&mut rig, BUILD);
    let now = at(200);
    assert_eq!(
        step(&rig, at(100), now),
        ReminderStep::WaitUntil(now + TimeDelta::seconds(DELAY))
    );
    // With nothing due the entry is dropped, mail or not.
    commit(&mut rig, flagged(&task), at(210)).unwrap();
    assert_eq!(step(&rig, at(220), at(400)), ReminderStep::Nothing);
}

#[test]
fn several_tasks_are_judged_alone_and_answered_together() {
    let mut rig = Rig::new();
    let stale = assigned(&mut rig);
    let fresh = assigned(&mut rig);
    let waiting = assigned(&mut rig);
    let done = assigned(&mut rig);
    let theirs = rig.work_task();
    rig.ok(assign(&theirs, OTHER, by(LEAD)));
    rig.ok(set(&waiting, S::Waiting, Some("a verdict"), by(BUILD)));
    commit(&mut rig, reminded(&stale), at(60)).unwrap();
    commit(&mut rig, reminded(&done), at(60)).unwrap();
    commit(&mut rig, flagged(&done), at(200)).unwrap();
    let late = assigned(&mut rig);
    commit(&mut rig, set(&late, S::Working, None, by(BUILD)), at(500)).unwrap();
    // In the order the tasks were created.
    assert_eq!(due(&rig, at(300)), act(&[&fresh], &[&stale]));
    assert_eq!(due(&rig, at(600)), act(&[&fresh, &late], &[&stale]));
}

#[test]
fn a_limit_outside_the_calendar_is_never_due() {
    let mut rig = Rig::new();
    assigned(&mut rig);
    let never = ReminderSettings {
        delay: Duration::MAX,
    };
    let builder = agent(BUILD);
    assert_eq!(
        reminder_step(&rig.state, &builder, at(0), &never, at(1_000_000)),
        ReminderStep::Nothing
    );
    // Mail in flight at the last representable instant: no time to look again.
    mail(&mut rig, BUILD);
    assert_eq!(
        reminder_step(
            &rig.state,
            &builder,
            at(0),
            &settings(),
            DateTime::<Utc>::MAX_UTC
        ),
        ReminderStep::Nothing
    );
}

/// One thing that can happen between two looks at the policy.
#[derive(Debug, Clone)]
enum Op {
    /// A turn ends now; the policy is asked `wait` seconds later and what it
    /// proposes is committed.
    TurnEnd {
        wait: i64,
    },
    Working,
    Waiting(u8),
    Blocked(u8),
    Submit,
    ChangesRequested,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => (0..3 * DELAY).prop_map(|wait| Op::TurnEnd { wait }),
        2 => Just(Op::Working),
        1 => (0..2u8).prop_map(Op::Waiting),
        1 => (0..2u8).prop_map(Op::Blocked),
        1 => Just(Op::Submit),
        1 => Just(Op::ChangesRequested),
    ]
}

proptest! {
    /// Whatever the agent and the lead do between turn ends, the task rules
    /// accept and record everything the policy proposes: it never proposes a
    /// second reminder or flag for the same stay in a state, never a flag
    /// without a reminder, and nothing for a task its agent is not acting on.
    #[test]
    fn the_task_rules_record_everything_the_policy_proposes(ops in prop::collection::vec(op(), 1..40)) {
        let mut rig = Rig::new();
        let task = assigned(&mut rig);
        let mut clock = 0;
        for op in ops {
            clock += 1;
            let now = at(clock);
            let moved = match op {
                Op::TurnEnd { wait } => {
                    clock += wait;
                    let proposed = step(&rig, now, at(clock));
                    let acting = matches!(
                        rig.task(&task).state,
                        S::Assigned | S::Working | S::ChangesRequested
                    );
                    if let ReminderStep::Act { remind, flag } = proposed {
                        prop_assert!(acting && wait >= DELAY);
                        prop_assert_eq!(remind.len() + flag.len(), 1);
                        for id in &remind {
                            prop_assert_eq!(commit(&mut rig, reminded(id), at(clock)), Ok(1));
                        }
                        for id in &flag {
                            prop_assert_eq!(commit(&mut rig, flagged(id), at(clock)), Ok(1));
                        }
                    } else {
                        // A silent turn end never goes unanswered: a task
                        // not yet reminded is, once the wait is over.
                        let fresh = rig.task(&task).reminded_at.is_none();
                        prop_assert!(!(acting && fresh && wait >= DELAY), "{proposed:?}");
                        if !acting {
                            prop_assert_eq!(proposed, ReminderStep::Nothing);
                        }
                    }
                    continue;
                }
                Op::Working => set(&task, S::Working, None, by(BUILD)),
                Op::Waiting(why) => set(&task, S::Waiting, Some(&format!("w{why}")), by(BUILD)),
                Op::Blocked(why) => set(&task, S::Blocked, Some(&format!("b{why}")), by(BUILD)),
                Op::Submit => submit(&task, TaskSubmission::Result, "r.md", by(BUILD)),
                Op::ChangesRequested => verdict(&task, TaskVerdict::ChangesRequested, None, by(LEAD)),
            };
            // A move the table refuses changes nothing.
            let _ = commit(&mut rig, moved, now);
        }
    }
}
