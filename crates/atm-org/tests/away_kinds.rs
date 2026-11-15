//! Kinds that always wait while the owner is away, whatever class the
//! requester's two fields claim. Pure: decide and apply, no clock.
#![allow(clippy::unwrap_used, clippy::expect_used)] // test helpers only
use atm_org::*;
use chrono::{DateTime, Utc};

fn at(second: u32) -> DateTime<Utc> {
    format!("2026-01-15T12:00:{second:02}Z").parse().unwrap()
}

/// Project `p`, the agent `p/builder`, and the owner away.
fn away_state() -> OrgState {
    let mut state = OrgState::default();
    for (seq, event) in [
        OrgEvent::ProjectRegistered {
            project_id: "p".parse().unwrap(),
            root: "/p".into(),
        },
        OrgEvent::AgentAdopted {
            agent_id: "p/builder".parse().unwrap(),
            pane: "%1".into(),
            session_id: None,
        },
        OrgEvent::OwnerAway(OwnerAway {
            by: Address::Owner,
            stacking_caps: [].into(),
        }),
    ]
    .into_iter()
    .enumerate()
    {
        state.apply(seq as u64 + 1, at(0), &event).unwrap();
    }
    state
}

fn request(kind: DecisionKind, may_be_undone: bool, changes_intent: bool) -> DecisionRequest {
    DecisionRequest {
        requester: "p/builder".parse().unwrap(),
        kind,
        title: "May I".into(),
        context: String::new(),
        files: vec![],
        options: vec!["yes".into(), "no".into()],
        recommended: Some("yes".into()),
        may_be_undone,
        changes_intent,
        deadline: None,
    }
}

const WAITING_KINDS: [DecisionKind; 4] = [
    DecisionKind::Action,
    DecisionKind::Hire,
    DecisionKind::Budget,
    DecisionKind::Unknown,
];
const FREE_KINDS: [DecisionKind; 3] = [
    DecisionKind::Policy,
    DecisionKind::Plan,
    DecisionKind::Dialog,
];

/// Every kind against every class a requester can claim. The request is
/// folded as a fact, as a log holds it, so an unknown kind is covered too.
#[test]
fn action_hire_budget_and_unknown_kinds_wait_whatever_the_request_claims() {
    for kind in WAITING_KINDS.into_iter().chain(FREE_KINDS) {
        for (may_be_undone, changes_intent) in
            [(true, true), (true, false), (false, true), (false, false)]
        {
            let mut state = away_state();
            let id = DecisionId::new(10);
            let fact = OrgEvent::DecisionRequested(request(kind, may_be_undone, changes_intent));
            state.apply(10, at(1), &fact).unwrap();
            let decision = state.decision(&id).unwrap();
            let waits = WAITING_KINDS.contains(&kind) || !may_be_undone;
            let expected = match (waits, changes_intent) {
                (true, _) => AwayStep::Wait(AwayWait::Irreversible),
                (false, true) => AwayStep::TakeProvisionally,
                (false, false) => AwayStep::AgentDecides,
            };
            let case = format!("{kind:?}, undone {may_be_undone}, intent {changes_intent}");
            assert_eq!(decision.away_step(true), expected, "{case}");
            // Present, the kind changes nothing: everything waits for the owner.
            assert_eq!(
                decision.away_step(false),
                AwayStep::Wait(AwayWait::OwnerPresent),
                "{case}"
            );
            let events = state
                .decide(
                    &OrgCommand::Owner(OwnerCommand::ApplyAway { decision_id: id }),
                    at(2),
                )
                .unwrap();
            assert_eq!(events.len(), usize::from(!waits), "{case}: {events:?}");
        }
    }
}
