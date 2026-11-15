//! The away table, the stacking cap and the away settings. Pure: every input
//! is an argument.
#![allow(clippy::unwrap_used, clippy::expect_used)] // test helpers only
use atm_org::*;

use AwayStep::{AgentDecides, TakeProvisionally, Wait};
use AwayWait::{Irreversible as CannotBeUndone, NoRecommendation, OwnerPresent};
use DecisionClass::{ChangesIntent, Detail, Irreversible};

/// Every cell of the away table, one row each: class, recommended, away.
#[test]
fn away_table_every_cell() {
    let table = [
        // The owner is present: nothing is taken for him.
        (Detail, true, false, Wait(OwnerPresent)),
        (Detail, false, false, Wait(OwnerPresent)),
        (ChangesIntent, true, false, Wait(OwnerPresent)),
        (ChangesIntent, false, false, Wait(OwnerPresent)),
        (Irreversible, true, false, Wait(OwnerPresent)),
        (Irreversible, false, false, Wait(OwnerPresent)),
        // Away, with a recommended option.
        (Detail, true, true, AgentDecides),
        (ChangesIntent, true, true, TakeProvisionally),
        (Irreversible, true, true, Wait(CannotBeUndone)),
        // Away, nothing recommended: nothing can be recorded.
        (Detail, false, true, Wait(NoRecommendation)),
        (ChangesIntent, false, true, Wait(NoRecommendation)),
        (Irreversible, false, true, Wait(CannotBeUndone)),
    ];
    for (class, recommended, away, expected) in table {
        assert_eq!(
            away_step(class, recommended, away),
            expected,
            "{class:?}, recommended {recommended}, away {away}"
        );
    }
}

const STATUSES: [DecisionStatus; 6] = [
    DecisionStatus::Pending,
    DecisionStatus::TakenProvisionally,
    DecisionStatus::Resolved,
    DecisionStatus::Overruled,
    DecisionStatus::Expired,
    DecisionStatus::Withdrawn,
];

#[test]
fn default_cap_holds_every_further_task_on_a_provisional_change_of_intent() {
    assert_eq!(DEFAULT_STACKING_CAP, 1);
    for started in [0, 1, 7] {
        assert_eq!(
            stacking_step(
                DecisionStatus::TakenProvisionally,
                ChangesIntent,
                started,
                DEFAULT_STACKING_CAP
            ),
            StackingStep::Hold,
            "{started} started"
        );
    }
}

#[test]
fn only_a_provisional_change_of_intent_holds() {
    for status in STATUSES {
        for class in [Detail, ChangesIntent, Irreversible] {
            let expected = if status == DecisionStatus::TakenProvisionally && class == ChangesIntent
            {
                StackingStep::Hold
            } else {
                StackingStep::Start
            };
            assert_eq!(
                stacking_step(status, class, 0, 1),
                expected,
                "{status:?} {class:?}"
            );
        }
    }
}

/// The task in progress is the first of the cap: N allows N - 1 further tasks.
#[test]
fn cap_counts_the_task_in_progress_as_the_first() {
    let step = |started, cap| {
        stacking_step(
            DecisionStatus::TakenProvisionally,
            ChangesIntent,
            started,
            cap,
        )
    };
    for (started, cap, expected) in [
        (0, 1, StackingStep::Hold),
        (0, 2, StackingStep::Start),
        (1, 2, StackingStep::Hold),
        (1, 3, StackingStep::Start),
        (2, 3, StackingStep::Hold),
        (5, 3, StackingStep::Hold),
        // A cap of zero cannot be set; it would hold like one.
        (0, 0, StackingStep::Hold),
        (0, u32::MAX, StackingStep::Start),
        (usize::MAX, u32::MAX, StackingStep::Hold),
    ] {
        assert_eq!(step(started, cap), expected, "{started} started, cap {cap}");
    }
}

#[test]
fn away_settings_default_to_one_and_read_the_cap() {
    assert_eq!(AwaySettings::default().stacking_cap, 1);
    for text in ["", "\n", "# only a comment\n"] {
        assert_eq!(
            parse_away_settings(text).unwrap(),
            AwaySettings::default(),
            "{text:?}"
        );
    }
    assert_eq!(
        parse_away_settings("stacking_cap = 1\n")
            .unwrap()
            .stacking_cap,
        1
    );
    assert_eq!(
        parse_away_settings("stacking_cap = 3")
            .unwrap()
            .stacking_cap,
        3
    );
    assert_eq!(
        parse_away_settings("stacking_cap = 4294967295")
            .unwrap()
            .stacking_cap,
        u32::MAX
    );
}

#[test]
fn away_settings_refuse_a_cap_below_one_unknown_keys_and_wrong_types() {
    assert!(matches!(
        parse_away_settings("stacking_cap = 0"),
        Err(AwaySettingsError::CapBelowOne)
    ));
    assert_eq!(
        parse_away_settings("stacking_cap = 0")
            .unwrap_err()
            .to_string(),
        "stacking_cap must be at least 1"
    );
    for text in [
        "stacking_cap = -1",
        "stacking_cap = 4294967296",
        "stacking_cap = \"1\"",
        "stacking_cap = 1.0",
        "stacking_cap = true",
        "stacking_cpa = 2",
        "stacking_cap = 2\nfuture = 1",
        "[stacking_cap]\n",
        "not toml",
    ] {
        assert!(
            matches!(parse_away_settings(text), Err(AwaySettingsError::Toml(_))),
            "{text:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// The switch, the away table applied to decisions, and the hold, through
// decide and apply. Time is the sequence number: nothing reads a clock.

mod world {
    use atm_org::*;
    use chrono::{DateTime, Duration, Utc};
    use serde_json::Value;

    pub fn start() -> DateTime<Utc> {
        "2026-01-15T12:00:00Z".parse().unwrap()
    }
    pub fn agent(text: &str) -> Address {
        Address::Agent(text.parse().unwrap())
    }
    pub fn owner() -> TaskActor {
        TaskActor {
            who: Address::Owner,
            lead: false,
        }
    }

    /// Projects `p` and `q` with the agents `p/lead`, `p/builder` and `q/lead`.
    pub struct World {
        pub state: OrgState,
        pub records: Vec<(u64, DateTime<Utc>, String, Value)>,
    }
    impl World {
        pub fn new() -> Self {
            let mut world = Self {
                state: OrgState::default(),
                records: Vec::new(),
            };
            for project in ["p", "q"] {
                world.must(OrgCommand::RegisterProject {
                    project_id: project.parse().unwrap(),
                    root: format!("/{project}"),
                    move_root: false,
                });
            }
            for (pane, name) in ["p/lead", "p/builder", "q/lead"].into_iter().enumerate() {
                world.must(OrgCommand::Adopt {
                    agent_id: name.parse().unwrap(),
                    pane: format!("%{pane}"),
                    session_id: None,
                });
            }
            world
        }
        pub fn now(&self) -> DateTime<Utc> {
            start() + Duration::seconds(self.records.len() as i64)
        }
        /// Decide, check that deciding changed nothing, then apply each event.
        pub fn step(&mut self, command: OrgCommand) -> Result<Vec<OrgEvent>, Refusal> {
            let before = self.state.clone();
            let result = self.state.decide(&command, self.now());
            assert_eq!(self.state, before, "decide changed the state: {command:?}");
            let events = result?;
            for event in &events {
                let seq = self.records.len() as u64 + 1;
                let ts = self.now();
                self.state.apply(seq, ts, event).unwrap();
                self.records
                    .push((seq, ts, event.kind().into(), event.to_body()));
            }
            Ok(events)
        }
        /// A command that must write exactly one event; its sequence number.
        pub fn must(&mut self, command: OrgCommand) -> u64 {
            let events = self.step(command.clone()).unwrap();
            assert_eq!(events.len(), 1, "{command:?}: {events:?}");
            self.records.len() as u64
        }
        pub fn owner(&mut self, command: OwnerCommand) -> Result<Vec<OrgEvent>, Refusal> {
            self.step(OrgCommand::Owner(command))
        }
        pub fn away(&mut self, caps: &[(&str, u32)]) {
            self.must(OrgCommand::Owner(OwnerCommand::Away {
                by: Address::Owner,
                stacking_caps: caps
                    .iter()
                    .map(|(project, cap)| (project.parse().unwrap(), *cap))
                    .collect(),
            }));
        }
        pub fn back(&mut self) {
            self.must(OrgCommand::Owner(OwnerCommand::Back { by: Address::Owner }));
        }
        /// A request with the options `a` and `b`.
        pub fn request(
            &mut self,
            requester: &str,
            may_be_undone: bool,
            changes_intent: bool,
            recommended: Option<&str>,
        ) -> DecisionId {
            DecisionId::new(self.must(OrgCommand::Decision(DecisionCommand::Request(
                DecisionRequest {
                    requester: agent(requester),
                    kind: DecisionKind::Policy,
                    title: "Which shape".into(),
                    context: String::new(),
                    files: vec![],
                    options: vec!["a".into(), "b".into()],
                    recommended: recommended.map(Into::into),
                    may_be_undone,
                    changes_intent,
                    deadline: None,
                },
            ))))
        }
        pub fn apply_away(&mut self, decision_id: DecisionId) -> Result<Vec<OrgEvent>, Refusal> {
            self.owner(OwnerCommand::ApplyAway { decision_id })
        }
        pub fn resolve(&mut self, decision_id: DecisionId, option: &str) {
            self.must(OrgCommand::Decision(DecisionCommand::Resolve(
                DecisionResolution {
                    decision_id,
                    option: option.into(),
                    by: Address::Owner,
                    note: None,
                    standing_rule: false,
                },
            )));
        }
        pub fn status(&self, id: DecisionId) -> DecisionStatus {
            self.state.decision(&id).unwrap().status
        }
        /// An open task, created by the owner, that rests on `rests_on`.
        pub fn task(&mut self, rests_on: Option<DecisionId>) -> TaskId {
            let seq = self.must(OrgCommand::Task(TaskCommand::Create(TaskCreated {
                title: "Build on it".into(),
                kind: TaskKind::Work,
                creator: Address::Owner,
                parent: None,
                judges: None,
                measure: TaskMeasure::Owner,
                budget: None,
                files: vec![],
                rests_on,
            })));
            format!("t{seq}").parse().unwrap()
        }
        pub fn assign(&mut self, task_id: &TaskId, to: &str) -> Result<Vec<OrgEvent>, Refusal> {
            self.step(OrgCommand::Task(TaskCommand::Assign {
                task_id: task_id.clone(),
                agent_id: to.parse().unwrap(),
                by: owner(),
            }))
        }
        pub fn set_state(&mut self, task_id: &TaskId, state: TaskState, by: TaskActor) {
            self.must(OrgCommand::Task(TaskCommand::SetState {
                task_id: task_id.clone(),
                state,
                reason: None,
                by,
            }));
        }
        /// A decision that changes the intent, taken provisionally on `a`.
        pub fn provisional(&mut self, requester: &str) -> DecisionId {
            let id = self.request(requester, true, true, Some("a"));
            assert_eq!(self.apply_away(id).unwrap().len(), 1);
            assert_eq!(self.status(id), DecisionStatus::TakenProvisionally);
            id
        }
        pub fn replayed(&self) -> OrgState {
            let mut state = OrgState::default();
            let mut stats = ReplayStats::default();
            for (seq, ts, kind, body) in &self.records {
                stats
                    .apply_parts(&mut state, *seq, *ts, kind, body)
                    .unwrap();
            }
            assert_eq!(stats.skipped_unknown, 0);
            state
        }
    }
}
use world::{agent, World};

fn held(result: Result<Vec<OrgEvent>, Refusal>) -> (TaskId, DecisionId, u32) {
    match result {
        Err(Refusal::Away(AwayRefusal::Held {
            task_id,
            decision_id,
            cap,
        })) => (task_id, decision_id, cap),
        other => panic!("not held: {other:?}"),
    }
}

#[test]
fn only_the_owner_sets_away_and_back() {
    let mut world = World::new();
    for who in [agent("p/lead"), Address::System] {
        // Refused also where nothing would change (back while present).
        for command in [
            OwnerCommand::Away {
                by: who.clone(),
                stacking_caps: [].into(),
            },
            OwnerCommand::Back { by: who.clone() },
        ] {
            let refusal = world.owner(command).unwrap_err();
            assert_eq!(
                refusal,
                Refusal::Away(AwayRefusal::NotOwner { who: who.clone() })
            );
            assert_eq!(refusal.to_string(), "only the owner may set away or back");
        }
    }
    assert_eq!(world.state.owner(), &OwnerState::default());
    world.away(&[]);
    for who in [agent("p/lead"), Address::System] {
        assert!(world.owner(OwnerCommand::Back { by: who }).is_err());
    }
    assert!(world.state.owner().away_since.is_some());
    // An agent the state does not know is a reference error, as everywhere.
    assert!(matches!(
        world.owner(OwnerCommand::Back {
            by: agent("p/nobody")
        }),
        Err(Refusal::Invariant(ApplyError::UnknownAgent(_)))
    ));
}

#[test]
fn away_and_back_write_once_and_fold_the_time() {
    let mut world = World::new();
    assert_eq!(world.state.owner().away_since, None);
    assert_eq!(
        world.owner(OwnerCommand::Back { by: Address::Owner }),
        Ok(vec![])
    );
    let at = world.now();
    let away = OwnerCommand::Away {
        by: Address::Owner,
        stacking_caps: [("p".parse().unwrap(), 2)].into(),
    };
    assert_eq!(
        world.owner(away.clone()),
        Ok(vec![OrgEvent::OwnerAway(OwnerAway {
            by: Address::Owner,
            stacking_caps: [("p".parse().unwrap(), 2)].into(),
        })])
    );
    assert_eq!(world.state.owner().away_since, Some(at));
    // Again, also with other caps: nothing is written and nothing changes.
    assert_eq!(world.owner(away), Ok(vec![]));
    assert_eq!(
        world.owner(OwnerCommand::Away {
            by: Address::Owner,
            stacking_caps: [("p".parse().unwrap(), 5)].into(),
        }),
        Ok(vec![])
    );
    assert_eq!(world.state.owner().away_since, Some(at));
    assert_eq!(
        world
            .state
            .owner()
            .stacking_cap(Some(&"p".parse().unwrap())),
        2
    );
    assert_eq!(
        world.owner(OwnerCommand::Back { by: Address::Owner }),
        Ok(vec![OrgEvent::OwnerBack(OwnerBack { by: Address::Owner })])
    );
    assert_eq!(world.state.owner().away_since, None);
    assert_eq!(
        world.owner(OwnerCommand::Back { by: Address::Owner }),
        Ok(vec![])
    );
    // A second absence starts at its own time.
    let later = world.now();
    assert_ne!(later, at);
    world.away(&[]);
    assert_eq!(world.state.owner().away_since, Some(later));
    assert_eq!(world.replayed(), world.state);
}

#[test]
fn caps_are_at_least_one_default_to_one_and_follow_the_latest_away() {
    let mut world = World::new();
    let (p, q): (ProjectId, ProjectId) = ("p".parse().unwrap(), "q".parse().unwrap());
    let cap =
        |world: &World, project: Option<&ProjectId>| world.state.owner().stacking_cap(project);
    assert_eq!(cap(&world, Some(&p)), 1);
    let zero = OwnerCommand::Away {
        by: Address::Owner,
        stacking_caps: [(p.clone(), 3), (q.clone(), 0)].into(),
    };
    assert!(matches!(
        world.owner(zero.clone()),
        Err(Refusal::InvalidValue {
            field: "stacking_cap",
            ..
        })
    ));
    assert_eq!(world.state.owner().away_since, None);
    world.away(&[("p", 3)]);
    // A bad cap is refused while away too, not passed over as a repeat.
    assert!(world.owner(zero).is_err());
    assert_eq!(cap(&world, Some(&p)), 3);
    assert_eq!(cap(&world, Some(&q)), 1);
    assert_eq!(cap(&world, None), 1);
    // Back keeps the caps: what is still provisional stays capped.
    world.back();
    assert_eq!(cap(&world, Some(&p)), 3);
    // The next away replaces them all.
    world.away(&[("q", 2)]);
    assert_eq!(cap(&world, Some(&p)), 1);
    assert_eq!(cap(&world, Some(&q)), 2);
    assert_eq!(world.replayed(), world.state);
}

#[test]
fn apply_away_takes_a_change_of_intent_by_system_and_a_detail_by_its_requester() {
    let mut world = World::new();
    world.away(&[]);
    let change = world.request("p/builder", true, true, Some("b"));
    let detail = world.request("q/lead", true, false, Some("a"));
    let at = world.now();
    assert_eq!(
        world.apply_away(change),
        Ok(vec![OrgEvent::DecisionTakenProvisionally(
            ProvisionalChoice {
                decision_id: change,
                option: "b".into(),
                by: Address::System,
            }
        )])
    );
    assert_eq!(
        world.apply_away(detail),
        Ok(vec![OrgEvent::DecisionTakenProvisionally(
            ProvisionalChoice {
                decision_id: detail,
                option: "a".into(),
                by: agent("q/lead"),
            }
        )])
    );
    for id in [change, detail] {
        assert_eq!(world.status(id), DecisionStatus::TakenProvisionally);
        // Again: it is no longer pending, so nothing is written.
        assert_eq!(world.apply_away(id), Ok(vec![]));
    }
    assert_eq!(world.state.decision(&change).unwrap().taken_at, Some(at));
    assert_eq!(world.replayed(), world.state);
}

#[test]
fn apply_away_writes_nothing_while_present_for_class_three_or_without_a_recommendation() {
    let mut world = World::new();
    let rows = [
        (true, true, Some("a")),
        (true, false, Some("a")),
        (false, true, Some("a")),
        (false, false, Some("a")),
        (true, true, None),
        (true, false, None),
        (false, false, None),
    ];
    // Present: nothing is taken, whatever the class.
    let present: Vec<_> = rows
        .iter()
        .map(|(undone, intent, recommended)| {
            world.request("p/builder", *undone, *intent, *recommended)
        })
        .collect();
    for id in &present {
        assert_eq!(world.apply_away(*id), Ok(vec![]), "{id}");
        assert_eq!(world.status(*id), DecisionStatus::Pending);
    }
    // Away: only the two rows that can be undone and name a recommendation.
    world.away(&[]);
    for (id, (undone, _, recommended)) in present.iter().zip(rows) {
        let taken = world.apply_away(*id).unwrap().len();
        assert_eq!(taken == 1, undone && recommended.is_some(), "{id}");
    }
    // A decision that is no longer pending is left alone.
    let resolved = world.request("p/builder", true, true, Some("a"));
    world.resolve(resolved, "b");
    let withdrawn = world.request("p/builder", true, true, Some("a"));
    world.must(OrgCommand::Decision(DecisionCommand::Withdraw(
        DecisionWithdrawal {
            decision_id: withdrawn,
            by: Address::Owner,
            reason: None,
        },
    )));
    for id in [resolved, withdrawn] {
        let before = world.status(id);
        assert_eq!(world.apply_away(id), Ok(vec![]));
        assert_eq!(world.status(id), before);
    }
    assert!(matches!(
        world.apply_away(DecisionId::new(999)),
        Err(Refusal::Invariant(ApplyError::UnknownDecision(_)))
    ));
}

/// The owner comes back between the request and the daemon's report: the
/// report is decided against the flag as it is, and takes nothing.
#[test]
fn nothing_is_taken_after_the_owner_is_back() {
    let mut world = World::new();
    world.away(&[]);
    let id = world.request("p/builder", true, true, Some("a"));
    world.back();
    assert_eq!(world.apply_away(id), Ok(vec![]));
    assert_eq!(world.status(id), DecisionStatus::Pending);
    // And a request from before he left is taken once he is away.
    world.away(&[]);
    assert_eq!(world.apply_away(id).unwrap().len(), 1);
}

#[test]
fn default_cap_holds_a_task_that_rests_on_a_provisional_decision_until_it_is_answered() {
    for answer in ["a", "b"] {
        let mut world = World::new();
        world.away(&[]);
        let decision = world.provisional("p/builder");
        let task = world.task(Some(decision));
        let refusal = world.assign(&task, "p/lead").unwrap_err();
        assert_eq!(
            refusal.to_string(),
            format!("task {task} rests on decision {decision}, which was taken provisionally for the owner: it waits until he has seen {decision} (stacking cap 1)")
        );
        assert_eq!(
            held(world.assign(&task, "p/lead")),
            (task.clone(), decision, 1)
        );
        assert_eq!(world.state.task(&task).unwrap().state, TaskState::Open);
        // Being back is not enough: he has not answered it.
        world.back();
        held(world.assign(&task, "p/lead"));
        // Confirmed (`a`) or overruled (`b`): he has seen it.
        world.resolve(decision, answer);
        assert_eq!(world.assign(&task, "p/lead").unwrap().len(), 1, "{answer}");
        assert_eq!(world.replayed(), world.state);
    }
}

#[test]
fn nothing_else_is_held() {
    let mut world = World::new();
    let pending = world.request("p/builder", true, true, Some("a"));
    world.away(&[]);
    let detail = world.request("p/builder", true, false, Some("a"));
    assert_eq!(world.apply_away(detail).unwrap().len(), 1);
    let waits = world.request("p/builder", false, true, Some("a"));
    assert_eq!(world.apply_away(waits), Ok(vec![]));
    for (rests_on, why) in [
        (None, "rests on nothing"),
        (Some(pending), "a pending decision"),
        (Some(detail), "a detail the agent decided"),
        (Some(waits), "a decision that waits"),
        (Some(DecisionId::new(999)), "a decision nobody knows"),
    ] {
        let task = world.task(rests_on);
        assert_eq!(world.assign(&task, "p/lead").unwrap().len(), 1, "{why}");
    }
    // A task that already started when its decision is taken runs on, and may
    // be handed to another agent: only a start is held.
    let running = world.task(Some(pending));
    world.assign(&running, "p/lead").unwrap();
    assert_eq!(world.apply_away(pending).unwrap().len(), 1);
    assert_eq!(world.assign(&running, "p/builder").unwrap().len(), 1);
    assert_eq!(world.assign(&running, "p/builder"), Ok(vec![]));
    // Authority is checked before the cap: a stranger learns nothing of it.
    let task = world.task(Some(pending));
    assert!(matches!(
        world.step(OrgCommand::Task(TaskCommand::Assign {
            task_id: task,
            agent_id: "p/lead".parse().unwrap(),
            by: TaskActor {
                who: agent("q/lead"),
                lead: false,
            },
        })),
        Err(Refusal::Task(TaskRefusal::NotAllowed { .. }))
    ));
}

#[test]
fn a_cap_of_two_lets_one_further_task_start() {
    let mut world = World::new();
    world.away(&[("p", 2)]);
    let decision = world.provisional("p/builder");
    let [first, second, third] = [(); 3].map(|()| world.task(Some(decision)));
    // Open tasks do not count: three are written down, none has started.
    assert_eq!(world.assign(&first, "p/lead").unwrap().len(), 1);
    assert_eq!(held(world.assign(&second, "p/lead")).2, 2);
    held(world.assign(&third, "p/builder"));
    // The started one still counts while it works and once it is submitted.
    let lead = TaskActor {
        who: agent("p/lead"),
        lead: false,
    };
    world.set_state(&first, TaskState::Working, lead.clone());
    held(world.assign(&second, "p/lead"));
    world.must(OrgCommand::Task(TaskCommand::Submit {
        task_id: first.clone(),
        what: TaskSubmission::Result,
        files: vec![],
        by: lead,
    }));
    held(world.assign(&second, "p/lead"));
    // Cancelled, it frees its place; the next start takes it.
    world.set_state(&first, TaskState::Cancelled, world::owner());
    assert_eq!(world.assign(&second, "p/lead").unwrap().len(), 1);
    held(world.assign(&third, "p/builder"));
    // Reopened, the second is a start again and the third may take the place.
    world.set_state(&second, TaskState::Open, world::owner());
    assert_eq!(world.assign(&third, "p/builder").unwrap().len(), 1);
    held(world.assign(&second, "p/lead"));
    assert_eq!(
        world
            .state
            .resting_tasks(decision)
            .map(|task| task.id.clone())
            .collect::<Vec<_>>(),
        vec![first, second, third]
    );
}

#[test]
fn the_cap_is_the_one_of_the_requesters_project() {
    let mut world = World::new();
    world.away(&[("p", 2)]);
    let ours = world.provisional("p/builder");
    let theirs = world.provisional("q/lead");
    let (ours, theirs) = (world.task(Some(ours)), world.task(Some(theirs)));
    assert_eq!(world.assign(&ours, "p/lead").unwrap().len(), 1);
    assert_eq!(held(world.assign(&theirs, "p/lead")).2, 1);
}

#[test]
fn owner_event_bodies_are_golden_and_tolerate_additions() {
    use serde_json::json;
    let away = OrgEvent::OwnerAway(OwnerAway {
        by: Address::Owner,
        stacking_caps: [("atm".parse().unwrap(), 1), ("other".parse().unwrap(), 3)].into(),
    });
    let back = OrgEvent::OwnerBack(OwnerBack { by: Address::Owner });
    for (event, kind, body) in [
        (
            away,
            "owner.away",
            json!({"by":"owner","stacking_caps":{"atm":1,"other":3}}),
        ),
        (back, "owner.back", json!({"by":"owner"})),
    ] {
        assert_eq!(event.kind(), kind);
        assert_eq!(event.to_body(), body);
        assert_eq!(
            OrgEvent::from_parts(kind, &body).unwrap().as_ref(),
            Some(&event)
        );
        let mut later = body.clone();
        later["future"] = json!({"added":true});
        assert_eq!(OrgEvent::from_parts(kind, &later).unwrap(), Some(event));
    }
    for (kind, body) in [
        ("owner.away", json!({"by":"owner"})),
        ("owner.away", json!({"stacking_caps":{}})),
        (
            "owner.away",
            json!({"by":"owner","stacking_caps":{"atm":-1}}),
        ),
        (
            "owner.away",
            json!({"by":"owner","stacking_caps":{"atm":"1"}}),
        ),
        (
            "owner.away",
            json!({"by":"owner","stacking_caps":{"Not A Slug":1}}),
        ),
        (
            "owner.away",
            json!({"by":"owner","stacking_caps":[["atm",1]]}),
        ),
        ("owner.away", json!({"by":"p/bad name","stacking_caps":{}})),
        ("owner.back", json!({})),
        ("owner.back", json!({"by":7})),
    ] {
        assert!(OrgEvent::from_parts(kind, &body).is_err(), "{kind} {body}");
    }
}

/// Apply folds facts without today's policy: a log that names a project nobody
/// registered, repeats an away, or was written by someone else still folds.
#[test]
fn owner_events_fold_without_policy_and_a_log_without_them_is_present() {
    use serde_json::json;
    let mut state = OrgState::default();
    let mut stats = ReplayStats::default();
    let at = |second: i64| world::start() + chrono::Duration::seconds(second);
    stats
        .apply_parts(
            &mut state,
            1,
            at(1),
            "project.registered",
            &json!({"project_id":"p","root":"/p"}),
        )
        .unwrap();
    assert_eq!(state.owner(), &OwnerState::default());
    for (seq, kind, body) in [
        (
            2,
            "owner.away",
            json!({"by":"owner","stacking_caps":{"gone":4}}),
        ),
        (
            3,
            "owner.away",
            json!({"by":"system","stacking_caps":{"p":2}}),
        ),
    ] {
        stats
            .apply_parts(&mut state, seq, at(seq as i64), kind, &body)
            .unwrap();
    }
    assert_eq!(state.owner().away_since, Some(at(3)));
    assert_eq!(
        state.owner().stacking_caps,
        [("p".parse().unwrap(), 2)].into()
    );
    stats
        .apply_parts(&mut state, 4, at(4), "owner.back", &json!({"by":"owner"}))
        .unwrap();
    assert_eq!(state.owner().away_since, None);
    assert_eq!(stats.applied, 4);
    // Only a reference is checked: an agent the log never made is an error.
    assert!(stats
        .apply_parts(&mut state, 5, at(5), "owner.back", &json!({"by":"p/ghost"}))
        .is_err());
}

mod property {
    use super::world::World;
    use atm_org::*;
    use proptest::prelude::*;
    use proptest::test_runner::{Config, RngSeed, TestRunner};
    use std::{cell::RefCell, collections::BTreeMap};

    #[derive(Debug, Clone)]
    struct Action {
        pick: usize,
        target: usize,
        who: usize,
        may_be_undone: bool,
        changes_intent: bool,
        recommended: usize,
        cap: u32,
    }
    fn arb_action() -> impl Strategy<Value = Action> {
        (
            0usize..6,
            0usize..8,
            0usize..4,
            any::<bool>(),
            any::<bool>(),
            0usize..3,
            0u32..4,
        )
            .prop_map(
                |(pick, target, who, may_be_undone, changes_intent, recommended, cap)| Action {
                    pick,
                    target,
                    who,
                    may_be_undone,
                    changes_intent,
                    recommended,
                    cap,
                },
            )
    }
    fn who(index: usize) -> Address {
        match index {
            0 | 1 => Address::Owner,
            2 => Address::System,
            _ => super::agent("p/lead"),
        }
    }

    /// Random histories of the switch, requests, the daemon's report and the
    /// owner's answers. After every step: a take happens only while away, only
    /// on a pending decision that can be undone, on its recommended option and
    /// by the right taker; and the log replays to the same state.
    #[test]
    fn decided_histories_keep_the_away_table_and_replay_to_the_same_state() {
        let seen = RefCell::new(BTreeMap::<&'static str, usize>::new());
        let mark = |name| *seen.borrow_mut().entry(name).or_default() += 1;
        let mut runner = TestRunner::new(Config {
            cases: 256,
            rng_seed: RngSeed::Fixed(0xa1),
            failure_persistence: None, // the fixed seed reproduces the run
            ..Config::default()
        });
        runner
            .run(&prop::collection::vec(arb_action(), 0..80), |actions| {
                let mut world = World::new();
                for action in actions {
                    let decisions: Vec<_> = world.state.decisions().map(|d| d.id).collect();
                    let target = decisions
                        .get(action.target % decisions.len().max(1))
                        .copied()
                        .unwrap_or(DecisionId::new(999_999));
                    let before = world.state.clone();
                    let away = before.owner().away_since.is_some();
                    match action.pick {
                        0 => {
                            let recommended = [None, Some("a"), Some("b")][action.recommended];
                            world.request(
                                "p/builder",
                                action.may_be_undone,
                                action.changes_intent,
                                recommended,
                            );
                        }
                        1 => {
                            let result = world.owner(OwnerCommand::Away {
                                by: who(action.who),
                                stacking_caps: [("p".parse().unwrap(), action.cap)].into(),
                            });
                            let allowed = action.who < 2 && action.cap >= 1;
                            prop_assert_eq!(result.is_ok(), allowed);
                            prop_assert_eq!(
                                result.is_ok_and(|events| !events.is_empty()),
                                allowed && !away
                            );
                        }
                        2 => {
                            let result = world.owner(OwnerCommand::Back {
                                by: who(action.who),
                            });
                            prop_assert_eq!(result.is_ok(), action.who < 2);
                            prop_assert_eq!(
                                result.is_ok_and(|events| !events.is_empty()),
                                action.who < 2 && away
                            );
                        }
                        3 | 4 => {
                            let Some(old) = before.decision(&target) else {
                                prop_assert!(world.apply_away(target).is_err());
                                continue;
                            };
                            let events = world.apply_away(target).unwrap();
                            let expected = if old.status != DecisionStatus::Pending {
                                mark("not pending");
                                None
                            } else {
                                match old.away_step(away) {
                                    AwayStep::Wait(reason) => {
                                        mark(match reason {
                                            AwayWait::OwnerPresent => "present",
                                            AwayWait::Irreversible => "class three",
                                            AwayWait::NoRecommendation => "no recommendation",
                                        });
                                        None
                                    }
                                    AwayStep::TakeProvisionally => {
                                        mark("taken");
                                        Some(Address::System)
                                    }
                                    AwayStep::AgentDecides => {
                                        mark("agent decides");
                                        Some(old.request.requester.clone())
                                    }
                                }
                            };
                            match (events.as_slice(), expected) {
                                ([], None) => {}
                                ([OrgEvent::DecisionTakenProvisionally(choice)], Some(by)) => {
                                    prop_assert!(away);
                                    prop_assert!(old.request.may_be_undone);
                                    prop_assert_eq!(
                                        Some(&choice.option),
                                        old.request.recommended.as_ref()
                                    );
                                    prop_assert_eq!(&choice.by, &by);
                                    prop_assert_eq!(
                                        by == Address::System,
                                        old.request.changes_intent
                                    );
                                }
                                other => prop_assert!(false, "{:?}", other),
                            }
                        }
                        _ => {
                            let option = ["a", "b"][action.recommended % 2];
                            let _ = world.step(OrgCommand::Decision(DecisionCommand::Resolve(
                                DecisionResolution {
                                    decision_id: target,
                                    option: option.into(),
                                    by: Address::Owner,
                                    note: None,
                                    standing_rule: false,
                                },
                            )));
                        }
                    }
                    // Whatever happened, nothing that cannot be undone is provisional.
                    for decision in world.state.decisions() {
                        prop_assert!(
                            decision.status != DecisionStatus::TakenProvisionally
                                || decision.request.may_be_undone
                        );
                    }
                }
                prop_assert_eq!(world.replayed(), world.state);
                Ok(())
            })
            .unwrap();
        for name in [
            "not pending",
            "present",
            "class three",
            "no recommendation",
            "taken",
            "agent decides",
        ] {
            assert!(seen.borrow().get(name).is_some_and(|n| *n > 0), "{name}");
        }
    }
}
