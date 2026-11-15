//! Synthetic: plain task commands and reported measurements, no harness or I/O.
//! Breaks caught: summing cumulative prices, arbitrary attribution, status-edge
//! turn counts, truncating each state visit, and accepting a plan as final work.
use atm_core::{LifecycleEvent, Money, SessionId};
use atm_org::*;
use chrono::{DateTime, Duration, Utc};
use serde_json::json;

struct Rig {
    state: OrgState,
    seq: u64,
    now: DateTime<Utc>,
}
impl Rig {
    fn new() -> Self {
        let mut r = Self {
            state: OrgState::default(),
            seq: 0,
            now: "2026-01-01T00:00:00Z".parse().unwrap(),
        };
        r.run(OrgCommand::RegisterProject {
            project_id: "p".parse().unwrap(),
            root: "/project".into(),
            move_root: false,
        });
        for (name, pane, session) in [("p/build", "%1", "s1"), ("p/review", "%2", "s2")] {
            r.run(OrgCommand::Adopt {
                agent_id: name.parse().unwrap(),
                pane: pane.into(),
                session_id: Some(session.into()),
            });
        }
        r
    }
    fn run(&mut self, command: OrgCommand) -> Vec<OrgEvent> {
        let before = self.state.clone();
        let events = self.state.decide(&command, self.now).unwrap();
        assert_eq!(self.state, before);
        for e in &events {
            self.seq += 1;
            self.state.apply(self.seq, self.now, e).unwrap();
        }
        events
    }
    fn task(&mut self) -> TaskId {
        let body = serde_json::from_value(json!({"title":"work", "kind":"work", "creator":"owner", "parent":null, "judges":null, "measure":{"kind":"mechanical","command":"check"}, "budget":null, "files":[], "rests_on":null})).unwrap();
        self.run(OrgCommand::Task(TaskCommand::Create(body)));
        format!("t{}", self.seq).parse().unwrap()
    }
    fn assign(&mut self, id: &TaskId, who: &str) {
        self.run(OrgCommand::Task(TaskCommand::Assign {
            task_id: id.clone(),
            agent_id: who.parse().unwrap(),
            by: actor("owner"),
        }));
    }
    fn state(&mut self, id: &TaskId, state: TaskState, reason: Option<&str>) {
        self.run(OrgCommand::Task(TaskCommand::SetState {
            task_id: id.clone(),
            state,
            reason: reason.map(Into::into),
            by: actor("p/build"),
        }));
    }
    fn submit(&mut self, id: &TaskId, what: TaskSubmission) {
        self.run(OrgCommand::Task(TaskCommand::Submit {
            task_id: id.clone(),
            what,
            files: vec![],
            by: actor("p/build"),
        }));
    }
    fn verdict(&mut self, id: &TaskId, verdict: TaskVerdict, review: Option<TaskId>) {
        self.run(OrgCommand::Task(TaskCommand::Verdict {
            task_id: id.clone(),
            verdict,
            review_task: review,
            note: None,
            by: actor("owner"),
        }));
    }
    fn sample(
        &mut self,
        session: &str,
        pane: &str,
        cost: Option<i64>,
        peak: Option<u8>,
        turn: bool,
    ) {
        self.run(OrgCommand::ObserveTaskUsage(TaskUsageObservation {
            session_id: session.into(),
            pane: Some(pane.into()),
            turn,
            cumulative_cost: cost.map(Money::from_microdollars),
            context_percent: peak,
        }));
    }
    fn cost(&self, id: &TaskId) -> TaskCost {
        self.state.task_cost(id, self.now).unwrap()
    }
}
fn actor(who: &str) -> TaskActor {
    TaskActor {
        who: who.parse().unwrap(),
        lead: false,
    }
}

#[test]
fn cumulative_samples_are_not_summed() {
    let mut r = Rig::new();
    let t = r.task();
    r.assign(&t, "p/build");
    for value in [100_000, 120_000, 120_000, 110_000, 125_000] {
        r.sample("s1", "%1", Some(value), None, false);
    }
    assert_eq!(r.cost(&t).cost.unwrap().as_microdollars(), 25_000);
}

#[test]
fn ambiguous_and_unbound_samples_are_unattributed_but_advance_baselines() {
    let mut r = Rig::new();
    r.sample("s1", "%1", Some(100), None, true);
    let a = r.task();
    r.assign(&a, "p/build");
    r.sample("s1", "%1", Some(150), Some(40), true);
    let b = r.task();
    r.assign(&b, "p/build");
    r.sample("s1", "%1", Some(200), Some(90), true);
    r.assign(&b, "p/review");
    r.sample("s1", "%1", Some(250), Some(10), true);
    r.sample("s1", "%2", Some(300), Some(100), true); // stale/reused pane
    r.sample("s2", "%2", Some(900), Some(70), true); // separate session baseline
    r.sample("s2", "%2", Some(910), None, false);
    assert_eq!(r.cost(&a).cost.unwrap().as_microdollars(), 100);
    assert_eq!(r.cost(&a).turns, Some(2));
    assert_eq!(r.cost(&a).peak_context_percent, Some(40));
    assert_eq!(r.cost(&b).cost.unwrap().as_microdollars(), 10);
    assert_eq!(r.cost(&b).peak_context_percent, Some(70));
    r.run(OrgCommand::Clear {
        agent_id: "p/build".parse().unwrap(),
    });
    r.sample("s1", "%1", Some(400), Some(100), true);
    r.sample("pending-9", "%1", Some(0), Some(100), true);
    assert_eq!(r.cost(&a).turns, Some(2));
    assert_eq!(r.cost(&a).peak_context_percent, Some(40));
}

#[test]
fn only_prompts_count_and_missing_metrics_remain_unknown() {
    let mut r = Rig::new();
    let t = r.task();
    r.assign(&t, "p/build");
    let events = [
        LifecycleEvent::PromptSubmit {
            prompt: Some("same".into()),
        },
        LifecycleEvent::WorkingStart,
        LifecycleEvent::WorkingEnd,
        LifecycleEvent::Idle,
        LifecycleEvent::SessionStart { source: None },
    ];
    for event in events {
        if let Some(input) =
            TaskUsageObservation::from_lifecycle("s1".into(), Some("%1".into()), &event)
        {
            r.run(OrgCommand::ObserveTaskUsage(input));
        }
    }
    assert_eq!(r.cost(&t).turns, Some(1));
    assert_eq!(r.cost(&t).cost, None);
    assert_eq!(r.cost(&t).peak_context_percent, None);
    r.sample("s1", "%1", Some(0), Some(0), true);
    r.sample("s1", "%1", Some(0), Some(70), false);
    r.sample("s1", "%1", Some(0), Some(10), false);
    assert_eq!(r.cost(&t).turns, Some(2));
    assert_eq!(r.cost(&t).cost, Some(Money::zero()));
    assert_eq!(r.cost(&t).peak_context_percent, Some(70));
}

#[test]
fn normalization_requires_reported_current_context_and_valid_money() {
    for (tokens, window, want) in [
        (Some(1), Some(3), Some(34)),
        (Some(0), Some(100), Some(0)),
        (Some(999), Some(1), Some(100)),
        (Some(1), None, None),
        (None, Some(100), None),
        (Some(1), Some(0), None),
    ] {
        let event = LifecycleEvent::ContextUpdate {
            tokens: Some(1_000_000),
            current_tokens: tokens,
            context_window_size: window,
            cost_usd: Some(0.0),
        };
        let got = TaskUsageObservation::from_lifecycle(SessionId::new("s"), None, &event).unwrap();
        assert_eq!(got.context_percent, want);
        assert_eq!(got.cumulative_cost, Some(Money::zero()));
    }
    for price in [f64::NAN, f64::INFINITY, -0.1] {
        let event = LifecycleEvent::ContextUpdate {
            tokens: None,
            current_tokens: None,
            context_window_size: None,
            cost_usd: Some(price),
        };
        assert!(TaskUsageObservation::from_lifecycle("s".into(), None, &event).is_none());
    }
}

#[test]
fn state_visits_keep_fractional_time_and_reads_do_not_mutate() {
    let mut r = Rig::new();
    let t = r.task();
    r.now += Duration::seconds(2);
    r.assign(&t, "p/build");
    r.now += Duration::seconds(3);
    r.state(&t, TaskState::Working, None);
    r.now += Duration::milliseconds(600);
    r.state(&t, TaskState::Waiting, Some("decision"));
    r.now += Duration::seconds(2);
    r.state(&t, TaskState::Waiting, Some("another decision"));
    r.now += Duration::seconds(1);
    r.state(&t, TaskState::Working, None);
    r.now += Duration::milliseconds(600);
    r.submit(&t, TaskSubmission::Plan);
    r.now += Duration::seconds(4);
    r.verdict(&t, TaskVerdict::Accepted, None);
    assert_eq!(r.state.task(&t).unwrap().state, TaskState::Working);
    assert!(r.state.task(&t).unwrap().cost.is_none());
    let before = r.state.clone();
    let record = r.cost(&t);
    assert_eq!(r.state, before);
    assert_eq!(record.seconds_in_state.get(&TaskState::Open), Some(&2));
    assert_eq!(record.seconds_in_state.get(&TaskState::Assigned), Some(&3));
    assert_eq!(record.seconds_in_state.get(&TaskState::Working), Some(&1));
    assert_eq!(record.seconds_in_state.get(&TaskState::Waiting), Some(&3));
    assert_eq!(record.seconds_in_state.get(&TaskState::Submitted), Some(&4));
    r.now -= Duration::seconds(5);
    assert_eq!(
        r.cost(&t).seconds_in_state.get(&TaskState::Working),
        Some(&1)
    );
}

#[test]
fn accepted_and_cancelled_time_and_usage_freeze() {
    for cancelled in [false, true] {
        let mut r = Rig::new();
        let t = r.task();
        r.assign(&t, "p/build");
        r.sample("s1", "%1", Some(0), Some(40), true);
        r.now += Duration::seconds(5);
        if cancelled {
            r.run(OrgCommand::Task(TaskCommand::SetState {
                task_id: t.clone(),
                state: TaskState::Cancelled,
                reason: None,
                by: actor("owner"),
            }));
        } else {
            r.submit(&t, TaskSubmission::Result);
            r.verdict(&t, TaskVerdict::Accepted, None);
        }
        let frozen = r.cost(&t);
        r.now += Duration::days(1);
        r.sample("s1", "%1", Some(1000), Some(100), true);
        assert_eq!(r.cost(&t), frozen);
        assert_eq!(frozen.seconds_in_state.get(&TaskState::Assigned), Some(&5));
    }
}

#[test]
fn reviews_count_started_and_implicit_rounds_once() {
    let mut r = Rig::new();
    let t = r.task();
    r.assign(&t, "p/build");
    r.submit(&t, TaskSubmission::Result);
    // Historical facts let this test exercise bookkeeping without a second policy fixture.
    for (start, verdict) in [
        (true, TaskVerdict::ChangesRequested),
        (false, TaskVerdict::Accepted),
    ] {
        let mut body: TaskCreated = serde_json::from_value(json!({"title":"review","kind":"review","creator":"owner","judges":t,"measure":{"kind":"owner"},"files":[]})).unwrap();
        body.parent = None;
        r.run(OrgCommand::Task(TaskCommand::Create(body)));
        let review: TaskId = format!("t{}", r.seq).parse().unwrap();
        r.assign(&review, "p/review");
        if start {
            r.run(OrgCommand::Task(TaskCommand::StartReview {
                task_id: t.clone(),
                review_task: review.clone(),
                by: actor("owner"),
            }));
        }
        r.run(OrgCommand::Task(TaskCommand::Submit {
            task_id: review.clone(),
            what: TaskSubmission::Result,
            files: vec![],
            by: actor("p/review"),
        }));
        r.verdict(&t, verdict, Some(review));
        if verdict == TaskVerdict::ChangesRequested {
            r.state(&t, TaskState::Working, None);
            r.submit(&t, TaskSubmission::Result);
        }
    }
    assert_eq!(r.cost(&t).review_rounds, Some(2));
    assert_eq!(r.cost(&t).changes_requested, Some(1));
    r.verdict(&t, TaskVerdict::Accepted, None);
    assert_eq!(r.cost(&t).review_rounds, Some(2));
}
