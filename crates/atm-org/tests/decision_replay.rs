//! Property tests for decisions: random command histories, decided then
//! applied one at a time with a clock that is an input. No waits, no I/O.
#![allow(clippy::unwrap_used, clippy::expect_used)] // test generators/helpers only
#[path = "support/decision.rs"]
mod decision_support;
use atm_org::*;
use chrono::{DateTime, Duration, Utc};
use decision_support::*;
use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed, TestRunner};
use serde_json::Value;
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
};

fn start() -> DateTime<Utc> {
    "2026-01-15T12:00:00Z".parse().unwrap()
}
fn who(index: usize) -> Address {
    match index % 4 {
        0 => Address::Owner,
        1 => Address::System,
        2 => Address::Agent(builder()),
        _ => "p/other".parse().unwrap(),
    }
}
/// Two listed options and one that never is.
fn option(index: usize) -> String {
    ["a", "b", "c"][index % 3].into()
}

#[derive(Debug, Clone)]
struct Action {
    pick: u64,
    target: usize,
    who: usize,
    option: usize,
    recommended: usize,
    may_be_undone: bool,
    changes_intent: bool,
    deadline: Option<i64>,
    advance: i64,
    as_owner: bool,
}
fn arb_action() -> impl Strategy<Value = Action> {
    (
        (0u64..5, 0usize..8, 0usize..4, 0usize..3, 0usize..3),
        (any::<bool>(), any::<bool>(), any::<bool>()),
        (prop::option::of(0i64..12), 0i64..4),
    )
        .prop_map(
            |(
                (pick, target, who, option, recommended),
                (may_be_undone, changes_intent, as_owner),
                (deadline, advance),
            )| Action {
                pick,
                target,
                who,
                option,
                recommended,
                may_be_undone,
                changes_intent,
                deadline,
                advance,
                as_owner,
            },
        )
}
impl Action {
    fn command(&self, state: &OrgState, now: DateTime<Utc>) -> DecisionCommand {
        let decisions: Vec<_> = state.decisions().collect();
        let decision_id = decisions
            .get(self.target % decisions.len().max(1))
            .map(|decision| decision.id)
            .unwrap_or(DecisionId::new(999_999));
        match self.pick {
            0 => {
                let mut request = request();
                request.requester = who(self.who);
                request.recommended =
                    [None, Some("a".into()), Some("b".into())][self.recommended].clone();
                request.may_be_undone = self.may_be_undone;
                request.changes_intent = self.changes_intent;
                // Zero seconds ahead is "not in the future": a refused request.
                request.deadline = self.deadline.map(|ahead| now + Duration::seconds(ahead));
                DecisionCommand::Request(request)
            }
            1 => DecisionCommand::TakeProvisionally(ProvisionalChoice {
                decision_id,
                option: option(self.option),
                by: Address::System,
            }),
            2 => DecisionCommand::Resolve(DecisionResolution {
                decision_id,
                option: option(self.option),
                by: if self.as_owner {
                    Address::Owner
                } else {
                    who(self.who)
                },
                note: self.changes_intent.then(|| "note".into()),
                standing_rule: self.may_be_undone,
            }),
            3 => DecisionCommand::Expire { decision_id },
            _ => DecisionCommand::Withdraw(DecisionWithdrawal {
                decision_id,
                by: who(self.who),
                reason: None,
            }),
        }
    }
}

type Record = (u64, DateTime<Utc>, String, Value);

struct History {
    state: OrgState,
    records: Vec<Record>,
    now: DateTime<Utc>,
}
impl History {
    /// Project `p` with the agents `p/builder` and `p/other`.
    fn seeded() -> Self {
        let mut history = Self {
            state: OrgState::default(),
            records: Vec::new(),
            now: start(),
        };
        history.apply(&OrgEvent::ProjectRegistered {
            project_id: "p".parse().unwrap(),
            root: "/project".into(),
        });
        for (name, pane) in [("p/builder", "%1"), ("p/other", "%2")] {
            history.apply(&OrgEvent::AgentAdopted {
                agent_id: name.parse().unwrap(),
                pane: pane.into(),
                session_id: None,
            });
        }
        history
    }
    fn apply(&mut self, event: &OrgEvent) {
        let seq = self.records.len() as u64 + 1;
        // Every event goes to the log as kind and body, and comes back equal.
        let (kind, body) = (event.kind(), event.to_body());
        assert!(body.is_object(), "{kind}");
        assert_eq!(
            OrgEvent::from_parts(kind, &body).unwrap().as_ref(),
            Some(event)
        );
        let result = self.state.apply(seq, self.now, event);
        assert!(
            result.is_ok(),
            "decide returned an inapplicable event: {event:?}: {result:?}"
        );
        self.records.push((seq, self.now, kind.into(), body));
    }
}

fn fold(records: &[Record], state: &mut OrgState, stats: &mut ReplayStats) {
    for (seq, ts, kind, body) in records {
        stats.apply_parts(state, *seq, *ts, kind, body).unwrap();
    }
}

fn status_name(status: DecisionStatus) -> &'static str {
    match status {
        DecisionStatus::Pending => "pending",
        DecisionStatus::TakenProvisionally => "taken provisionally",
        DecisionStatus::Resolved => "resolved",
        DecisionStatus::Overruled => "overruled",
        DecisionStatus::Expired => "expired",
        DecisionStatus::Withdrawn => "withdrawn",
    }
}
/// Exhaustive on purpose: a new refusal must be reached by the generator.
fn refusal_name(refusal: &Refusal) -> &'static str {
    match refusal {
        Refusal::Decision(DecisionRefusal::InvalidState { .. }) => "invalid state",
        Refusal::Decision(DecisionRefusal::MustWait { .. }) => "must wait",
        Refusal::Decision(DecisionRefusal::NotRecommended { .. }) => "not recommended",
        Refusal::Decision(DecisionRefusal::UnknownOption { .. }) => "unknown option",
        Refusal::Decision(DecisionRefusal::NotAllowed { .. }) => "not allowed",
        Refusal::Decision(DecisionRefusal::NoDeadline { .. }) => "no deadline",
        Refusal::Decision(DecisionRefusal::DeadlineNotReached { .. }) => "deadline not reached",
        Refusal::Invariant(_) => "reference",
        Refusal::InvalidValue { .. } => "invalid value",
        Refusal::ProjectRootConflict { .. }
        | Refusal::InvalidAgentState { .. }
        | Refusal::AgentAlreadyBound { .. }
        | Refusal::RoleNameMismatch { .. }
        | Refusal::InvalidDeliveryState { .. }
        | Refusal::WrongRecipient { .. }
        | Refusal::InvalidRecipient(_) => "not a decision refusal",
    }
}
const REFUSALS: [&str; 9] = [
    "invalid state",
    "must wait",
    "not recommended",
    "unknown option",
    "not allowed",
    "no deadline",
    "deadline not reached",
    "reference",
    "invalid value",
];
/// The arrows of the state table. Everything else must never be observed.
const ARROWS: [(&str, &str); 6] = [
    ("pending", "taken provisionally"),
    ("pending", "resolved"),
    ("pending", "expired"),
    ("pending", "withdrawn"),
    ("taken provisionally", "resolved"),
    ("taken provisionally", "overruled"),
];

/// What must hold for every decision in every state a decided history reaches.
fn check_decision(decision: &Decision) -> Result<(), TestCaseError> {
    let request = &decision.request;
    let provisional = decision.provisional.as_ref();
    let resolution = decision.resolution.as_ref();
    if let Some(taken) = provisional {
        prop_assert!(
            request.may_be_undone,
            "taken though it may not be undone: {decision:?}"
        );
        prop_assert_eq!(Some(&taken.option), request.recommended.as_ref());
        prop_assert!(decision.taken_at.is_some());
    }
    if let Some(answer) = resolution {
        prop_assert_eq!(&answer.by, &Address::Owner);
        prop_assert!(request.options.contains(&answer.option));
    }
    match decision.status {
        DecisionStatus::Pending => {
            prop_assert!(provisional.is_none() && resolution.is_none());
            prop_assert!(decision.withdrawal.is_none() && decision.closed_at.is_none());
        }
        DecisionStatus::TakenProvisionally => {
            prop_assert!(provisional.is_some() && resolution.is_none());
            prop_assert!(decision.closed_at.is_none());
        }
        DecisionStatus::Resolved => {
            let answer = resolution.expect("a resolved decision has its answer");
            if let Some(taken) = provisional {
                prop_assert_eq!(
                    &taken.option,
                    &answer.option,
                    "a changed answer is an overrule"
                );
            }
        }
        DecisionStatus::Overruled => {
            let answer = resolution.expect("an overruled decision has its answer");
            let taken = provisional.expect("only a provisional choice is overruled");
            prop_assert_ne!(&taken.option, &answer.option);
        }
        DecisionStatus::Expired => {
            let deadline = request.deadline.expect("only a deadline expires");
            prop_assert!(decision.closed_at.expect("closed") >= deadline);
            prop_assert!(provisional.is_none() && resolution.is_none());
        }
        DecisionStatus::Withdrawn => {
            let by = &decision.withdrawal.as_ref().expect("who withdrew").by;
            prop_assert!(by == &Address::Owner || by == &request.requester);
            prop_assert!(provisional.is_none() && resolution.is_none());
        }
    }
    prop_assert_eq!(
        decision.closed_at.is_some(),
        !matches!(
            decision.status,
            DecisionStatus::Pending | DecisionStatus::TakenProvisionally
        )
    );
    Ok(())
}

#[test]
fn decided_histories_keep_the_table_and_replay_to_the_same_state() {
    // The runner takes a `Fn`, so the coverage counters sit in cells.
    let arrows: RefCell<BTreeMap<(&str, &str), usize>> = RefCell::default();
    let refusals: RefCell<BTreeMap<&str, usize>> = RefCell::default();
    let repeats = Cell::new(0usize);
    let mut runner = TestRunner::new(Config {
        cases: 256,
        rng_seed: RngSeed::Fixed(0xd1),
        failure_persistence: None, // the fixed seed reproduces the whole run
        ..Config::default()
    });
    let inputs = (prop::collection::vec(arb_action(), 0..120), any::<usize>());
    runner
        .run(&inputs, |(actions, split)| {
            let mut history = History::seeded();
            for action in actions {
                history.now += Duration::seconds(action.advance);
                let command = action.command(&history.state, history.now);
                let before = history.state.clone();
                let result = history
                    .state
                    .decide(&OrgCommand::Decision(command.clone()), history.now);
                prop_assert_eq!(&history.state, &before, "decide changed the state");
                // Deciding is a function of state, command and time.
                prop_assert_eq!(
                    &before.decide(&OrgCommand::Decision(command.clone()), history.now),
                    &result
                );
                match result {
                    Err(refusal) => {
                        let name = refusal_name(&refusal);
                        prop_assert_ne!(name, "not a decision refusal");
                        *refusals.borrow_mut().entry(name).or_default() += 1;
                    }
                    Ok(events) => {
                        prop_assert!(events.len() <= 1, "{:?}", events);
                        repeats.set(repeats.get() + usize::from(events.is_empty()));
                        for event in &events {
                            history.apply(event);
                        }
                    }
                }
                for decision in history.state.decisions() {
                    check_decision(decision)?;
                    match before.decision(&decision.id) {
                        // A new decision is pending, with its sequence as its ID.
                        None => {
                            prop_assert_eq!(decision.status, DecisionStatus::Pending);
                            prop_assert_eq!(decision.id.seq(), history.records.len() as u64);
                        }
                        Some(old) if old == decision => {}
                        Some(old) => {
                            let arrow = (status_name(old.status), status_name(decision.status));
                            prop_assert!(ARROWS.contains(&arrow), "{:?}: {:?}", arrow, command);
                            prop_assert_eq!(&old.request, &decision.request);
                            *arrows.borrow_mut().entry(arrow).or_default() += 1;
                        }
                    }
                }
                prop_assert!(before
                    .decisions()
                    .all(|old| history.state.decision(&old.id).is_some()));
            }

            // Folding the log gives the live state, twice, and from any split.
            let mut whole = OrgState::default();
            let mut stats = ReplayStats::default();
            fold(&history.records, &mut whole, &mut stats);
            prop_assert_eq!(&whole, &history.state);
            prop_assert_eq!(stats.applied, history.records.len() as u64);
            let mut repeated = OrgState::default();
            let mut repeated_stats = ReplayStats::default();
            fold(&history.records, &mut repeated, &mut repeated_stats);
            prop_assert_eq!(&whole, &repeated);
            prop_assert_eq!(&stats, &repeated_stats);
            for cut in [
                0,
                history.records.len(),
                split % (history.records.len() + 1),
            ] {
                let mut partial = OrgState::default();
                let mut partial_stats = ReplayStats::default();
                fold(&history.records[..cut], &mut partial, &mut partial_stats);
                fold(&history.records[cut..], &mut partial, &mut partial_stats);
                prop_assert_eq!(&whole, &partial);
                prop_assert_eq!(&stats, &partial_stats);
            }
            Ok(())
        })
        .unwrap();

    // The generator must have reached every arrow, every refusal and repeats,
    // or the properties above were checked on too little.
    let (arrows, refusals, repeats) = (arrows.into_inner(), refusals.into_inner(), repeats.get());
    println!("decision arrows (256 cases): {arrows:?}");
    println!("decision refusals: {refusals:?}; accepted repeats: {repeats}");
    for arrow in ARROWS {
        assert!(arrows.get(&arrow).is_some_and(|n| *n > 0), "{arrow:?}");
    }
    for refusal in REFUSALS {
        assert!(refusals.get(refusal).is_some_and(|n| *n > 0), "{refusal}");
    }
    assert!(repeats > 0);
}

proptest! {
    #![proptest_config(Config { failure_persistence: None, ..Config::default() })]

    #[test]
    fn decision_ids_round_trip_and_reject_other_text(seq in any::<u64>(), text in any::<String>()) {
        let id = DecisionId::new(seq);
        prop_assert_eq!(id.to_string().parse::<DecisionId>(), Ok(id));
        if let Ok(parsed) = text.parse::<DecisionId>() {
            prop_assert_eq!(parsed.to_string(), text);
        }
    }

    #[test]
    fn arbitrary_decision_bodies_never_panic(kind in 0usize..6, text in any::<String>(), flag in any::<bool>()) {
        let kind = DECISION_KINDS[kind];
        let mut body = golden_event(kind).to_body();
        for field in ["decision_id", "requester", "by", "option", "deadline", "kind"] {
            if flag && body.get(field).is_some() {
                body[field] = Value::String(text.clone());
            }
        }
        let _ = OrgEvent::from_parts(kind, &body);
        let _ = OrgEvent::from_parts(kind, &Value::String(text));
    }
}
