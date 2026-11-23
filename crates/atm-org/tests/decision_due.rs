//! The expiry queries the decision bridge's timer runs on (design 12, owner Q2).
//! Pure: the state and `now` are arguments.
#![allow(clippy::unwrap_used, clippy::expect_used)] // test helpers only
#[path = "support/decision.rs"]
mod decision_support;
use atm_org::*;
use chrono::{DateTime, Utc};
use decision_support::*;

fn at(second: u32) -> DateTime<Utc> {
    format!("2026-01-15T12:00:{second:02}Z").parse().unwrap()
}
fn d(seq: u64) -> DecisionId {
    DecisionId::new(seq)
}

struct World {
    state: OrgState,
    seq: u64,
}
impl World {
    fn new() -> Self {
        let mut world = Self {
            state: OrgState::default(),
            seq: 0,
        };
        world.apply(OrgEvent::ProjectRegistered {
            project_id: "p".parse().unwrap(),
            root: "/project".into(),
        });
        world.apply(OrgEvent::AgentAdopted {
            agent_id: builder(),
            pane: "%1".into(),
            session_id: None,
        });
        world
    }
    fn apply(&mut self, event: OrgEvent) -> u64 {
        self.seq += 1;
        self.state.apply(self.seq, at(0), &event).unwrap();
        self.seq
    }
    fn ask(&mut self, deadline: Option<u32>) -> DecisionId {
        let mut request = request();
        request.deadline = deadline.map(at);
        d(self.apply(OrgEvent::DecisionRequested(request)))
    }
}

/// d3 pending (30), d4 pending (20), d5 pending without a deadline,
/// d6 taken provisionally (10), d8 resolved (10), d10 withdrawn (10),
/// d12 already expired (10), d14 pending (40).
fn world() -> World {
    let mut w = World::new();
    assert_eq!(w.ask(Some(30)), d(3));
    w.ask(Some(20));
    w.ask(None);
    let taken = w.ask(Some(10));
    w.apply(OrgEvent::DecisionTakenProvisionally(ProvisionalChoice {
        relayed_by: None,
        decision_id: taken,
        option: "a".into(),
        by: Address::System,
    }));
    let resolved = w.ask(Some(10));
    w.apply(OrgEvent::DecisionResolved(
        DecisionResolution {
            decision_id: resolved,
            option: "a".into(),
            by: Address::Owner,
            note: None,
            standing_rule: false,
        }
        .into(),
    ));
    let withdrawn = w.ask(Some(10));
    w.apply(OrgEvent::DecisionWithdrawn(DecisionWithdrawal {
        decision_id: withdrawn,
        by: Address::Owner,
        reason: None,
    }));
    let expired = w.ask(Some(10));
    w.apply(OrgEvent::DecisionExpired {
        decision_id: expired,
    });
    assert_eq!(w.ask(Some(40)), d(14));
    w
}

// Break caught: a strict `<`, a status other than pending, a missing deadline
// treated as due, or ids out of order.
#[test]
fn expiring_lists_pending_decisions_at_or_past_their_deadline() {
    let w = world();
    assert_eq!(expiring(&w.state, at(19)), Vec::<DecisionId>::new());
    assert_eq!(expiring(&w.state, at(20)), [d(4)]);
    assert_eq!(expiring(&w.state, at(30)), [d(3), d(4)]);
    assert_eq!(expiring(&w.state, at(59)), [d(3), d(4), d(14)]);
}

// Break caught: the query and the policy disagree, so the bridge either sends
// an Expire the policy refuses or never sends one it would take.
#[test]
fn expiring_agrees_with_the_expire_policy_at_every_second() {
    let w = world();
    for second in 0..60 {
        let now = at(second);
        let due = expiring(&w.state, now);
        for decision in w.state.decisions() {
            let command = OrgCommand::Decision(DecisionCommand::Expire {
                decision_id: decision.id,
            });
            let takes = w
                .state
                .decide(&command, now)
                .is_ok_and(|events| !events.is_empty());
            assert_eq!(
                due.contains(&decision.id),
                takes,
                "{} at {second}",
                decision.id
            );
        }
    }
}

// Break caught: the earliest deadline of any decision (closed ones too), or the
// latest, arms the timer; or a deadline already passed re-arms it at once, so
// a decision whose expiry was refused would spin the bridge.
#[test]
fn next_deadline_is_the_earliest_pending_deadline_after_now() {
    let mut w = world();
    assert_eq!(next_deadline(&w.state, at(0)), Some(at(20)));
    assert_eq!(next_deadline(&w.state, at(20)), Some(at(30)));
    assert_eq!(next_deadline(&w.state, at(40)), None);
    w.apply(OrgEvent::DecisionExpired { decision_id: d(4) });
    assert_eq!(next_deadline(&w.state, at(0)), Some(at(30)));
    w.apply(OrgEvent::DecisionExpired { decision_id: d(3) });
    w.apply(OrgEvent::DecisionWithdrawn(DecisionWithdrawal {
        decision_id: d(14),
        by: Address::Owner,
        reason: None,
    }));
    assert_eq!(next_deadline(&w.state, at(0)), None);
    assert_eq!(next_deadline(&OrgState::default(), at(0)), None);
}
