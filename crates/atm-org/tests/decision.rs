//! Decisions: the codec, the fold and the golden log. Pure: time and sequence
//! numbers are arguments.
#![allow(clippy::unwrap_used, clippy::expect_used)] // test helpers only
#[path = "support/decision.rs"]
mod decision_support;
use atm_org::*;
use chrono::{DateTime, Utc};
use decision_support::*;
use serde_json::json;

fn at(second: u32) -> DateTime<Utc> {
    format!("2026-01-15T12:00:{second:02}Z").parse().unwrap()
}
fn d(seq: u64) -> DecisionId {
    DecisionId::new(seq)
}

/// The golden log folded line by line.
fn folded() -> OrgState {
    let mut state = OrgState::default();
    let mut stats = ReplayStats::default();
    let log = golden_log();
    for line in &log {
        stats
            .apply_parts(&mut state, line.seq, line.ts, &line.kind, &line.body)
            .unwrap();
    }
    assert_eq!(stats.applied, log.len() as u64);
    assert_eq!(stats.skipped_unknown, 0);
    state
}

#[test]
fn decision_ids_are_canonical_d_and_sequence() {
    for seq in [0, 41, u64::MAX] {
        let id = d(seq);
        assert_eq!(id.to_string(), format!("d{seq}"));
        assert_eq!(id.to_string().parse::<DecisionId>().unwrap(), id);
        assert_eq!(id.seq(), seq);
        assert_eq!(serde_json::to_value(id).unwrap(), json!(format!("d{seq}")));
    }
    for text in [
        "",
        "d",
        "d01",
        "d+1",
        "d-1",
        "d 1",
        "D1",
        "m1",
        "1",
        "d18446744073709551616",
    ] {
        let error = text.parse::<DecisionId>().unwrap_err();
        assert_eq!(error.kind, "decision id", "{text:?}");
        assert!(serde_json::from_value::<DecisionId>(json!(text)).is_err());
    }
}

#[test]
fn every_decision_kind_has_a_golden_body_that_round_trips() {
    let log = golden_log();
    for kind in DECISION_KINDS {
        assert!(
            log.iter().any(|line| line.kind == kind),
            "no row for {kind}"
        );
    }
    for line in log.iter().filter(|l| l.kind.starts_with("decision.")) {
        assert!(
            DECISION_KINDS.contains(&line.kind.as_str()),
            "{}",
            line.kind
        );
        let event = OrgEvent::from_parts(&line.kind, &line.body)
            .unwrap()
            .unwrap();
        assert_eq!(event.kind(), line.kind);
        assert!(event.to_body().is_object(), "{}", line.kind);
        let mut expected = line.body.clone();
        if line.seq == 14 {
            // A newer kind folds, but its spelling is lost in this reader.
            expected["kind"] = json!("unknown");
        }
        assert_eq!(event.to_body(), expected, "{} seq {}", line.kind, line.seq);
        assert_eq!(
            OrgEvent::from_parts(event.kind(), &event.to_body()).unwrap(),
            Some(event)
        );
    }
}

// Break caught: dropping caller audit data, changing old JSON, or comparing a
// pane instead of only the answer when matching resolution events and repeats.
#[test]
fn resolution_audit_golden_bodies_round_trip_without_changing_answers() {
    let mut previous = None;
    for line in include_str!("fixtures/decision_resolution_events.jsonl").lines() {
        let row: serde_json::Value = serde_json::from_str(line).unwrap();
        let kind = row["kind"].as_str().unwrap();
        let body = &row["body"];
        let event = OrgEvent::from_parts(kind, body).unwrap().unwrap();
        assert_eq!(
            serde_json::to_string(&event.to_body()).unwrap(),
            serde_json::to_string(body).unwrap(),
            "caller audit must round-trip without changing the old flat body"
        );
        if let Some(old) = previous.take() {
            assert_eq!(event, old, "caller pane is audit, not the answer");
        } else {
            previous = Some(event.clone());
        }
        let mut state = folded();
        state.apply(15, at(15), &event).unwrap();
        let resolution = state.decision(&d(3)).unwrap().resolution.clone().unwrap();
        assert_eq!(
            resolution,
            DecisionResolution {
                decision_id: d(3),
                option: "b".into(),
                by: Address::Owner,
                note: Some("audit check".into()),
                standing_rule: false,
            }
        );
        assert_eq!(
            state.decide(
                &OrgCommand::Decision(DecisionCommand::Resolve(resolution)),
                at(40)
            ),
            Ok(vec![]),
            "the command is still the answer alone, regardless of recorded caller"
        );
    }
}

#[test]
fn malformed_decision_bodies_error_and_additive_fields_are_tolerated() {
    for kind in DECISION_KINDS {
        let error = OrgEvent::from_parts(kind, &json!({}))
            .unwrap_err()
            .to_string();
        assert!(error.contains(kind), "{error}");
        let event = golden_event(kind);
        let mut body = event.to_body();
        body["future"] = json!({"new":1});
        assert_eq!(OrgEvent::from_parts(kind, &body).unwrap(), Some(event));
    }
    for (kind, field, bad) in [
        ("decision.requested", "requester", json!("p/bad name")),
        ("decision.requested", "kind", json!(1)),
        ("decision.requested", "may_be_undone", json!("true")),
        ("decision.requested", "deadline", json!("tomorrow")),
        ("decision.requested", "options", json!("a")),
        ("decision.taken_provisionally", "decision_id", json!("m3")),
        ("decision.resolved", "standing_rule", json!(1)),
        ("decision.overruled", "by", json!("")),
        ("decision.expired", "decision_id", json!(3)),
        ("decision.withdrawn", "decision_id", json!("d03")),
    ] {
        let mut body = golden_event(kind).to_body();
        body[field] = bad;
        assert!(OrgEvent::from_parts(kind, &body).is_err(), "{kind} {field}");
    }
}

#[test]
fn unknown_decision_kinds_decode_to_a_fallback_without_the_original_word() {
    for kind in ["merge", "unknown"] {
        let mut body = request_body();
        body["kind"] = json!(kind);
        let event = OrgEvent::from_parts("decision.requested", &body)
            .unwrap()
            .unwrap();
        body["kind"] = json!("unknown");
        assert_eq!(event.to_body(), body);
        assert_eq!(
            OrgEvent::from_parts("decision.requested", &event.to_body()).unwrap(),
            Some(event)
        );
    }
}

#[test]
fn the_class_follows_from_the_two_facts() {
    let mut request = request();
    for (may_be_undone, changes_intent, class) in [
        (true, false, DecisionClass::Detail),
        (true, true, DecisionClass::ChangesIntent),
        (false, false, DecisionClass::Irreversible),
        (false, true, DecisionClass::Irreversible),
    ] {
        request.may_be_undone = may_be_undone;
        request.changes_intent = changes_intent;
        assert_eq!(request.class(), class);
    }
}

#[test]
fn golden_log_folds_to_the_expected_state() {
    let state = folded();
    let ids: Vec<_> = state.decisions().map(|decision| decision.id).collect();
    assert_eq!(ids, [3, 6, 9, 11, 13, 14].map(d));

    // Taken provisionally on `a`, then overruled by the owner with `b`.
    let overruled = state.decision(&d(3)).unwrap();
    assert_eq!(
        overruled,
        &Decision {
            id: d(3),
            request: DecisionRequest {
                requester: Address::Agent(builder()),
                kind: DecisionKind::Plan,
                title: "Approve the D2 plan".into(),
                context: "Two shapes for the list method; A is smaller.".into(),
                files: vec!["docs/d2-plan.md".into()],
                options: vec!["a".into(), "b".into()],
                recommended: Some("a".into()),
                may_be_undone: true,
                changes_intent: true,
                deadline: Some("2026-11-15T18:00:00Z".parse().unwrap()),
            },
            requested_at: at(3),
            status: DecisionStatus::Overruled,
            provisional: Some(ProvisionalChoice {
                decision_id: d(3),
                option: "a".into(),
                by: Address::System,
            }),
            taken_at: Some(at(4)),
            resolution: Some(DecisionResolution {
                decision_id: d(3),
                option: "b".into(),
                by: Address::Owner,
                note: Some("B keeps the wire smaller".into()),
                standing_rule: true,
            }),
            withdrawal: None,
            closed_at: Some(at(5)),
        }
    );
    assert_eq!(overruled.class(), DecisionClass::ChangesIntent);

    // A detail the agent decided, confirmed by the owner.
    let confirmed = state.decision(&d(6)).unwrap();
    assert_eq!(confirmed.class(), DecisionClass::Detail);
    assert_eq!(confirmed.status, DecisionStatus::Resolved);
    assert_eq!(confirmed.request.deadline, None);
    assert_eq!(
        confirmed.provisional,
        Some(ProvisionalChoice {
            decision_id: d(6),
            option: "list".into(),
            by: Address::Agent(builder()),
        })
    );
    assert_eq!(confirmed.taken_at, Some(at(7)));
    assert_eq!(
        confirmed.resolution,
        Some(DecisionResolution {
            decision_id: d(6),
            option: "list".into(),
            by: Address::Owner,
            note: None,
            standing_rule: false,
        })
    );
    assert_eq!(confirmed.closed_at, Some(at(8)));

    // Cannot be undone: it waited, and its deadline passed.
    let expired = state.decision(&d(9)).unwrap();
    assert_eq!(expired.class(), DecisionClass::Irreversible);
    assert_eq!(expired.request.requester, Address::System);
    assert_eq!(expired.status, DecisionStatus::Expired);
    assert_eq!((&expired.provisional, &expired.resolution), (&None, &None));
    assert_eq!(expired.closed_at, Some(at(10)));

    let withdrawn = state.decision(&d(11)).unwrap();
    assert_eq!(withdrawn.status, DecisionStatus::Withdrawn);
    assert_eq!(withdrawn.request.recommended, None);
    assert_eq!(
        withdrawn.withdrawal,
        Some(DecisionWithdrawal {
            decision_id: d(11),
            by: Address::Agent(builder()),
            reason: Some("answered by the lead".into()),
        })
    );
    assert_eq!(withdrawn.closed_at, Some(at(12)));

    let pending = state.decision(&d(13)).unwrap();
    assert_eq!(
        pending,
        &Decision {
            id: d(13),
            request: DecisionRequest {
                requester: Address::Agent(builder()),
                kind: DecisionKind::Hire,
                title: "Add a second reviewer".into(),
                context: String::new(),
                files: vec![],
                options: vec!["yes".into(), "no".into()],
                recommended: None,
                may_be_undone: false,
                changes_intent: true,
                deadline: None,
            },
            requested_at: at(13),
            status: DecisionStatus::Pending,
            provisional: None,
            taken_at: None,
            resolution: None,
            withdrawal: None,
            closed_at: None,
        }
    );
    let future = state.decision(&d(14)).unwrap();
    assert_eq!(
        future,
        &Decision {
            id: d(14),
            request: DecisionRequest {
                requester: Address::System,
                kind: DecisionKind::Unknown,
                title: "A decision from a newer build".into(),
                context: String::new(),
                files: vec![],
                options: vec!["yes".into(), "no".into()],
                recommended: None,
                may_be_undone: false,
                changes_intent: true,
                deadline: None,
            },
            requested_at: at(14),
            status: DecisionStatus::Pending,
            provisional: None,
            taken_at: None,
            resolution: None,
            withdrawal: None,
            closed_at: None,
        }
    );
    assert_eq!(state.decision(&d(4)), None);
}

#[test]
fn fold_records_facts_without_enforcing_the_table() {
    // Facts the policy would refuse still fold: an overrule of a decision that
    // was never taken, a provisional choice on one that must wait, an answer
    // after the end, and a second provisional choice replacing the first.
    let mut state = folded();
    let overrule = OrgEvent::DecisionOverruled(
        DecisionResolution {
            decision_id: d(13),
            option: "elsewhere".into(),
            by: Address::Agent(builder()),
            note: None,
            standing_rule: false,
        }
        .into(),
    );
    state.apply(20, at(20), &overrule).unwrap();
    let decision = state.decision(&d(13)).unwrap();
    assert_eq!(decision.status, DecisionStatus::Overruled);
    assert_eq!(decision.provisional, None);
    assert_eq!(decision.closed_at, Some(at(20)));

    for (seq, option) in [(21, "yes"), (22, "no")] {
        let taken = OrgEvent::DecisionTakenProvisionally(ProvisionalChoice {
            decision_id: d(9),
            option: option.into(),
            by: Address::System,
        });
        state.apply(seq, at(seq as u32), &taken).unwrap();
    }
    let decision = state.decision(&d(9)).unwrap();
    assert_eq!(decision.status, DecisionStatus::TakenProvisionally);
    assert_eq!(decision.provisional.as_ref().unwrap().option, "no");
    assert_eq!(decision.taken_at, Some(at(22)));
    assert_eq!(decision.closed_at, Some(at(10)), "the earlier fact stays");

    state
        .apply(23, at(23), &OrgEvent::DecisionExpired { decision_id: d(6) })
        .unwrap();
    let decision = state.decision(&d(6)).unwrap();
    assert_eq!(decision.status, DecisionStatus::Expired);
    assert_eq!(decision.closed_at, Some(at(23)));
    assert!(decision.resolution.is_some());

    // Reading old facts still accepts an owner requester, unlike new commands.
    let mut request = request();
    request.requester = Address::Owner;
    state
        .apply(24, at(24), &OrgEvent::DecisionRequested(request.clone()))
        .unwrap();
    assert_eq!(state.decision(&d(24)).unwrap().request, request);
    assert_eq!(
        state.decision(&d(24)).unwrap().status,
        DecisionStatus::Pending
    );
}

#[test]
fn unknown_references_and_duplicate_ids_leave_the_state_unchanged() {
    let mut state = folded();
    let before = state.clone();
    let stranger: Address = "p/stranger".parse().unwrap();
    let mut unknown_requester = request();
    unknown_requester.requester = stranger.clone();
    let resolution = |decision_id, by: &Address| DecisionResolution {
        decision_id,
        option: "a".into(),
        by: by.clone(),
        note: None,
        standing_rule: false,
    };
    let choice = |decision_id, by: &Address| ProvisionalChoice {
        decision_id,
        option: "a".into(),
        by: by.clone(),
    };
    let withdrawal = |decision_id, by: &Address| DecisionWithdrawal {
        decision_id,
        by: by.clone(),
        reason: None,
    };
    let owner = Address::Owner;
    let unknown_agent = ApplyError::UnknownAgent("p/stranger".parse().unwrap());
    for (event, error) in [
        (
            OrgEvent::DecisionRequested(unknown_requester),
            unknown_agent.clone(),
        ),
        (
            OrgEvent::DecisionTakenProvisionally(choice(d(4), &owner)),
            ApplyError::UnknownDecision(d(4)),
        ),
        (
            OrgEvent::DecisionTakenProvisionally(choice(d(13), &stranger)),
            unknown_agent.clone(),
        ),
        (
            OrgEvent::DecisionResolved(resolution(d(4), &owner).into()),
            ApplyError::UnknownDecision(d(4)),
        ),
        (
            OrgEvent::DecisionResolved(resolution(d(13), &stranger).into()),
            unknown_agent.clone(),
        ),
        (
            OrgEvent::DecisionOverruled(resolution(d(4), &owner).into()),
            ApplyError::UnknownDecision(d(4)),
        ),
        (
            OrgEvent::DecisionOverruled(resolution(d(13), &stranger).into()),
            unknown_agent.clone(),
        ),
        (
            OrgEvent::DecisionExpired { decision_id: d(4) },
            ApplyError::UnknownDecision(d(4)),
        ),
        (
            OrgEvent::DecisionWithdrawn(withdrawal(d(4), &owner)),
            ApplyError::UnknownDecision(d(4)),
        ),
        (
            OrgEvent::DecisionWithdrawn(withdrawal(d(13), &stranger)),
            unknown_agent,
        ),
    ] {
        assert_eq!(state.apply(30, at(30), &event), Err(error), "{event:?}");
        assert_eq!(state, before);
    }
    // The ID is the sequence number: a second request at a used one is refused.
    assert_eq!(
        state.apply(13, at(30), &OrgEvent::DecisionRequested(request())),
        Err(ApplyError::DuplicateDecision(d(13)))
    );
    assert_eq!(state, before);
    // Replay reports the same error and counts nothing.
    let mut stats = ReplayStats::default();
    assert!(stats
        .apply_parts(
            &mut state,
            13,
            at(30),
            "decision.requested",
            &request_body()
        )
        .is_err());
    assert_eq!(stats, ReplayStats::default());
    assert_eq!(state, before);
}

fn request_body() -> serde_json::Value {
    OrgEvent::DecisionRequested(request()).to_body()
}

// ---- Policy: the state table, cell by cell -------------------------------

fn other() -> Address {
    "p/other".parse().unwrap()
}
fn requester() -> Address {
    Address::Agent(builder())
}
/// The golden request's deadline, moved next to the rig's clock.
fn deadline() -> DateTime<Utc> {
    at(30)
}

/// Decide, check that deciding changed nothing, then apply what was decided.
struct Rig {
    state: OrgState,
    seq: u64,
}
impl Rig {
    /// Project `p` with the agents `p/builder` and `p/other`.
    fn new() -> Self {
        let mut rig = Self {
            state: OrgState::default(),
            seq: 0,
        };
        rig.apply(OrgEvent::ProjectRegistered {
            project_id: "p".parse().unwrap(),
            root: "/project".into(),
        });
        for (name, pane) in [("p/builder", "%1"), ("p/other", "%2")] {
            rig.apply(OrgEvent::AgentAdopted {
                agent_id: name.parse().unwrap(),
                pane: pane.into(),
                session_id: None,
            });
        }
        rig
    }
    fn apply(&mut self, event: OrgEvent) {
        self.seq += 1;
        self.state
            .apply(self.seq, at(self.seq as u32), &event)
            .unwrap();
    }
    fn run(
        &mut self,
        command: DecisionCommand,
        now: DateTime<Utc>,
    ) -> Result<Vec<OrgEvent>, Refusal> {
        let before = self.state.clone();
        let result = self.state.decide(&OrgCommand::Decision(command), now);
        assert_eq!(self.state, before, "decide changed the state");
        let events = result?;
        for event in &events {
            self.apply(event.clone());
        }
        Ok(events)
    }
    /// One emitted event, as (kind, the decision's status afterwards).
    fn emits(
        &mut self,
        id: DecisionId,
        command: DecisionCommand,
        now: DateTime<Utc>,
    ) -> (&'static str, DecisionStatus) {
        let events = self.run(command, now).unwrap();
        assert_eq!(events.len(), 1, "{events:?}");
        (events[0].kind(), self.status(id))
    }
    fn status(&self, id: DecisionId) -> DecisionStatus {
        self.state.decision(&id).unwrap().status
    }
    fn request(&mut self, request: DecisionRequest) -> DecisionId {
        let events = self.run(DecisionCommand::Request(request), at(0)).unwrap();
        assert_eq!(events.len(), 1);
        d(self.seq)
    }
    /// A decision on the golden request (changes the intent, recommends `a`,
    /// options `a` and `b`), walked to `status` by legal commands only.
    fn with(status: DecisionStatus) -> (Self, DecisionId) {
        let mut rig = Self::new();
        let mut request = request();
        request.deadline = Some(deadline());
        let id = rig.request(request);
        let steps = match status {
            DecisionStatus::Pending => vec![],
            DecisionStatus::TakenProvisionally => vec![take(id, "a")],
            DecisionStatus::Resolved => vec![resolve(id, "a")],
            DecisionStatus::Overruled => vec![take(id, "a"), resolve(id, "b")],
            DecisionStatus::Expired => vec![DecisionCommand::Expire { decision_id: id }],
            DecisionStatus::Withdrawn => vec![withdraw(id, requester())],
        };
        for step in steps {
            assert_eq!(rig.run(step, deadline()).unwrap().len(), 1);
        }
        assert_eq!(rig.status(id), status);
        (rig, id)
    }
}

fn take(decision_id: DecisionId, option: &str) -> DecisionCommand {
    DecisionCommand::TakeProvisionally(ProvisionalChoice {
        decision_id,
        option: option.into(),
        by: Address::System,
    })
}
fn answer(decision_id: DecisionId, option: &str) -> DecisionResolution {
    DecisionResolution {
        decision_id,
        option: option.into(),
        by: Address::Owner,
        note: Some("because".into()),
        standing_rule: false,
    }
}
fn resolve(decision_id: DecisionId, option: &str) -> DecisionCommand {
    DecisionCommand::Resolve(answer(decision_id, option))
}
fn withdraw(decision_id: DecisionId, by: Address) -> DecisionCommand {
    DecisionCommand::Withdraw(DecisionWithdrawal {
        decision_id,
        by,
        reason: Some("no longer needed".into()),
    })
}

const STATES: [DecisionStatus; 6] = [
    DecisionStatus::Pending,
    DecisionStatus::TakenProvisionally,
    DecisionStatus::Resolved,
    DecisionStatus::Overruled,
    DecisionStatus::Expired,
    DecisionStatus::Withdrawn,
];

#[derive(Debug, Clone, Copy, PartialEq)]
enum Cell {
    /// One event of this kind, leaving the decision in this state.
    Emits(&'static str, DecisionStatus),
    /// Accepted, no event: a repeat of what already happened.
    Nothing,
    /// Refused because of the state the decision is in.
    Refused,
}

/// The plan's table: columns are take provisionally (on the recommended
/// option), the owner's answer (the recorded one, else the recommended
/// option), expire (deadline reached) and withdraw (by the requester).
fn expected(state: DecisionStatus) -> [Cell; 4] {
    use Cell::{Emits, Nothing, Refused};
    use DecisionStatus::*;
    match state {
        Pending => [
            Emits("decision.taken_provisionally", TakenProvisionally),
            Emits("decision.resolved", Resolved),
            Emits("decision.expired", Expired),
            Emits("decision.withdrawn", Withdrawn),
        ],
        TakenProvisionally => [
            Nothing,
            Emits("decision.resolved", Resolved),
            Refused,
            Refused,
        ],
        Resolved | Overruled => [Refused, Nothing, Refused, Refused],
        Expired => [Refused, Refused, Nothing, Refused],
        Withdrawn => [Refused, Refused, Refused, Nothing],
    }
}

#[test]
fn every_cell_of_the_state_table() {
    for state in STATES {
        for (column, cell) in expected(state).into_iter().enumerate() {
            let (mut rig, id) = Rig::with(state);
            let recorded = rig.state.decision(&id).unwrap().resolution.clone();
            let (command, operation) = match column {
                0 => (take(id, "a"), "take provisionally"),
                1 => (
                    DecisionCommand::Resolve(recorded.unwrap_or_else(|| answer(id, "a"))),
                    "resolve",
                ),
                2 => (DecisionCommand::Expire { decision_id: id }, "expire"),
                _ => (withdraw(id, requester()), "withdraw"),
            };
            let before = rig.state.clone();
            let result = rig.run(command, deadline());
            let label = format!("{state:?}, {operation}");
            match cell {
                Cell::Emits(kind, after) => {
                    let events = result.expect(&label);
                    assert_eq!(events.len(), 1, "{label}");
                    assert_eq!(events[0].kind(), kind, "{label}");
                    assert_eq!(rig.status(id), after, "{label}");
                }
                Cell::Nothing => {
                    assert_eq!(result, Ok(vec![]), "{label}");
                    assert_eq!(rig.state, before, "{label}");
                }
                Cell::Refused => {
                    assert_eq!(
                        result,
                        Err(Refusal::Decision(DecisionRefusal::InvalidState {
                            decision_id: id,
                            state,
                            operation,
                        })),
                        "{label}"
                    );
                    assert_eq!(rig.state, before, "{label}");
                }
            }
        }
    }
}

#[test]
fn emitted_events_carry_the_command_and_apply_stamps_the_time() {
    let (mut rig, id) = Rig::with(DecisionStatus::Pending);
    let events = rig.run(take(id, "a"), at(40)).unwrap();
    let choice = ProvisionalChoice {
        decision_id: id,
        option: "a".into(),
        by: Address::System,
    };
    assert_eq!(
        events,
        [OrgEvent::DecisionTakenProvisionally(choice.clone())]
    );
    let taken_at = at(rig.seq as u32);
    let events = rig.run(resolve(id, "a"), at(41)).unwrap();
    assert_eq!(events, [OrgEvent::DecisionResolved(answer(id, "a").into())]);
    let decision = rig.state.decision(&id).unwrap();
    assert_eq!(decision.provisional, Some(choice));
    assert_eq!(decision.taken_at, Some(taken_at));
    assert_eq!(decision.resolution, Some(answer(id, "a")));
    assert_eq!(decision.closed_at, Some(at(rig.seq as u32)));

    let (mut rig, id) = Rig::with(DecisionStatus::Pending);
    let events = rig.run(withdraw(id, Address::Owner), at(40)).unwrap();
    let withdrawal = DecisionWithdrawal {
        decision_id: id,
        by: Address::Owner,
        reason: Some("no longer needed".into()),
    };
    assert_eq!(events, [OrgEvent::DecisionWithdrawn(withdrawal.clone())]);
    assert_eq!(
        rig.state.decision(&id).unwrap().withdrawal,
        Some(withdrawal)
    );
}

#[test]
fn a_repeated_take_by_another_party_keeps_the_first_choice_and_time() {
    let (mut rig, id) = Rig::with(DecisionStatus::TakenProvisionally);
    let before = rig.state.clone();
    assert_eq!(
        rig.run(
            DecisionCommand::TakeProvisionally(ProvisionalChoice {
                decision_id: id,
                option: "a".into(),
                by: other(),
            }),
            at(40),
        ),
        Ok(vec![])
    );
    let decision = rig.state.decision(&id).unwrap();
    assert_eq!(decision.provisional.as_ref().unwrap().by, Address::System);
    assert_eq!(decision.taken_at, Some(at(5)));
    assert_eq!(rig.state, before);
}

#[test]
fn the_owner_can_repeat_the_requesters_withdrawal() {
    let (mut rig, id) = Rig::with(DecisionStatus::Withdrawn);
    let before = rig.state.clone();
    assert_eq!(rig.run(withdraw(id, Address::Owner), at(40)), Ok(vec![]));
    assert_eq!(rig.state, before);
}

#[test]
fn an_unlisted_answer_to_a_final_decision_is_an_invalid_state() {
    for state in [DecisionStatus::Resolved, DecisionStatus::Expired] {
        let (mut rig, id) = Rig::with(state);
        let before = rig.state.clone();
        assert_eq!(
            rig.run(resolve(id, "unlisted"), at(40)),
            Err(Refusal::Decision(DecisionRefusal::InvalidState {
                decision_id: id,
                state,
                operation: "resolve",
            }))
        );
        assert_eq!(rig.state, before);
    }
}

#[test]
fn an_irreversible_decision_waits_even_on_a_nonrecommended_option() {
    for changes_intent in [false, true] {
        let mut rig = Rig::new();
        let mut request = request();
        request.may_be_undone = false;
        request.changes_intent = changes_intent;
        let id = rig.request(request);
        let before = rig.state.clone();
        assert_eq!(
            rig.run(take(id, "b"), at(40)),
            Err(Refusal::Decision(DecisionRefusal::MustWait {
                decision_id: id,
            }))
        );
        assert_eq!(rig.state, before);
    }
}

#[test]
fn another_option_on_a_provisional_decision_is_an_overrule() {
    let (mut rig, id) = Rig::with(DecisionStatus::TakenProvisionally);
    let events = rig.run(resolve(id, "b"), at(40)).unwrap();
    assert_eq!(
        events,
        [OrgEvent::DecisionOverruled(answer(id, "b").into())]
    );
    let decision = rig.state.decision(&id).unwrap();
    assert_eq!(decision.status, DecisionStatus::Overruled);
    assert_eq!(decision.provisional.as_ref().unwrap().option, "a");
    assert_eq!(decision.resolution, Some(answer(id, "b")));
    // The same option confirms: resolved, never overruled.
    let (mut rig, id) = Rig::with(DecisionStatus::TakenProvisionally);
    assert_eq!(
        rig.emits(id, resolve(id, "a"), at(40)),
        ("decision.resolved", DecisionStatus::Resolved)
    );
    // From pending there is nothing to overrule, whatever the option.
    let (mut rig, id) = Rig::with(DecisionStatus::Pending);
    assert_eq!(
        rig.emits(id, resolve(id, "b"), at(40)),
        ("decision.resolved", DecisionStatus::Resolved)
    );
}

#[test]
fn a_final_answer_is_only_repeated_never_changed() {
    for state in [DecisionStatus::Resolved, DecisionStatus::Overruled] {
        let recorded = |rig: &Rig, id| rig.state.decision(&id).unwrap().resolution.clone().unwrap();
        let (rig, id) = Rig::with(state);
        let same = recorded(&rig, id);
        let mut other_option = same.clone();
        other_option.option = if same.option == "a" { "b" } else { "a" }.into();
        let mut other_note = same.clone();
        other_note.note = None;
        let mut now_a_rule = same.clone();
        now_a_rule.standing_rule = true;
        for changed in [other_option, other_note, now_a_rule] {
            let (mut rig, id) = Rig::with(state);
            assert_eq!(
                rig.run(DecisionCommand::Resolve(changed), at(40)),
                Err(Refusal::Decision(DecisionRefusal::InvalidState {
                    decision_id: id,
                    state,
                    operation: "resolve",
                }))
            );
        }
    }
}

#[test]
fn only_the_first_two_classes_are_taken_and_only_on_the_recommendation() {
    for (may_be_undone, changes_intent, taken) in [
        (true, false, true),
        (true, true, true),
        (false, false, false),
        (false, true, false),
    ] {
        let mut rig = Rig::new();
        let mut request = request();
        request.may_be_undone = may_be_undone;
        request.changes_intent = changes_intent;
        let id = rig.request(request);
        let result = rig.run(take(id, "a"), at(40));
        if taken {
            assert_eq!(result.unwrap().len(), 1);
            assert_eq!(rig.status(id), DecisionStatus::TakenProvisionally);
        } else {
            assert_eq!(
                result,
                Err(Refusal::Decision(DecisionRefusal::MustWait {
                    decision_id: id
                }))
            );
            assert_eq!(rig.status(id), DecisionStatus::Pending);
            // It waits for the owner, who may still answer it.
            assert_eq!(
                rig.emits(id, resolve(id, "a"), at(41)),
                ("decision.resolved", DecisionStatus::Resolved)
            );
        }
    }

    let not_recommended = |id, option: &str| {
        Err(Refusal::Decision(DecisionRefusal::NotRecommended {
            decision_id: id,
            option: option.into(),
        }))
    };
    // Another listed option, an unlisted one, and no recommendation at all.
    let (mut rig, id) = Rig::with(DecisionStatus::Pending);
    assert_eq!(rig.run(take(id, "b"), at(40)), not_recommended(id, "b"));
    assert_eq!(rig.run(take(id, "c"), at(40)), not_recommended(id, "c"));
    let mut request = request();
    request.recommended = None;
    let id = rig.request(request);
    assert_eq!(rig.run(take(id, "a"), at(40)), not_recommended(id, "a"));
    assert_eq!(rig.status(id), DecisionStatus::Pending);

    // A repeat is accepted only for the option already taken.
    let (mut rig, id) = Rig::with(DecisionStatus::TakenProvisionally);
    assert_eq!(
        rig.run(take(id, "b"), at(40)),
        Err(Refusal::Decision(DecisionRefusal::InvalidState {
            decision_id: id,
            state: DecisionStatus::TakenProvisionally,
            operation: "take provisionally",
        }))
    );
}

#[test]
fn only_the_owner_decides_in_every_state() {
    for state in STATES {
        for who in [requester(), other(), Address::System] {
            let (mut rig, id) = Rig::with(state);
            let mut resolution = answer(id, "a");
            resolution.by = who.clone();
            assert_eq!(
                rig.run(DecisionCommand::Resolve(resolution), at(40)),
                Err(Refusal::Decision(DecisionRefusal::NotAllowed {
                    decision_id: id,
                    who,
                    operation: "resolve",
                })),
                "{state:?}"
            );
            assert_eq!(rig.status(id), state);
        }
    }
}

#[test]
fn the_answer_must_be_a_listed_option() {
    for state in [DecisionStatus::Pending, DecisionStatus::TakenProvisionally] {
        let (mut rig, id) = Rig::with(state);
        for option in ["c", "", "A"] {
            assert_eq!(
                rig.run(resolve(id, option), at(40)),
                Err(Refusal::Decision(DecisionRefusal::UnknownOption {
                    decision_id: id,
                    option: option.into(),
                }))
            );
        }
        assert_eq!(rig.status(id), state);
    }
}

#[test]
fn only_the_requester_and_the_owner_withdraw() {
    for state in STATES {
        for who in [other(), Address::System] {
            let (mut rig, id) = Rig::with(state);
            assert_eq!(
                rig.run(withdraw(id, who.clone()), at(40)),
                Err(Refusal::Decision(DecisionRefusal::NotAllowed {
                    decision_id: id,
                    who,
                    operation: "withdraw",
                })),
                "{state:?}"
            );
            assert_eq!(rig.status(id), state);
        }
    }
    for who in [requester(), Address::Owner] {
        let (mut rig, id) = Rig::with(DecisionStatus::Pending);
        assert_eq!(
            rig.emits(id, withdraw(id, who), at(40)),
            ("decision.withdrawn", DecisionStatus::Withdrawn)
        );
    }
    // The daemon withdraws what it asked itself.
    let mut rig = Rig::new();
    let mut request = request();
    request.requester = Address::System;
    let id = rig.request(request);
    assert_eq!(
        rig.emits(id, withdraw(id, Address::System), at(40)),
        ("decision.withdrawn", DecisionStatus::Withdrawn)
    );
}

#[test]
fn a_decision_expires_at_its_deadline_and_not_before() {
    let expire = |id| DecisionCommand::Expire { decision_id: id };
    let (mut rig, id) = Rig::with(DecisionStatus::Pending);
    for early in [at(0), at(29)] {
        assert_eq!(
            rig.run(expire(id), early),
            Err(Refusal::Decision(DecisionRefusal::DeadlineNotReached {
                decision_id: id,
                deadline: deadline(),
            }))
        );
    }
    assert_eq!(rig.status(id), DecisionStatus::Pending);
    assert_eq!(
        rig.emits(id, expire(id), deadline()),
        ("decision.expired", DecisionStatus::Expired)
    );
    let (mut rig, id) = Rig::with(DecisionStatus::Pending);
    assert_eq!(
        rig.emits(id, expire(id), at(31)),
        ("decision.expired", DecisionStatus::Expired)
    );

    // Without a deadline nothing expires, however late it is.
    let mut rig = Rig::new();
    let mut request = request();
    request.deadline = None;
    let id = rig.request(request);
    assert_eq!(
        rig.run(expire(id), at(59)),
        Err(Refusal::Decision(DecisionRefusal::NoDeadline {
            decision_id: id
        }))
    );

    // Until the daemon records the expiry, the owner's late answer still counts.
    let (mut rig, id) = Rig::with(DecisionStatus::Pending);
    assert_eq!(
        rig.emits(id, resolve(id, "b"), at(45)),
        ("decision.resolved", DecisionStatus::Resolved)
    );
}

#[test]
fn a_request_is_checked_before_it_is_logged() {
    let invalid = |field: &'static str, reason: &str| {
        Err(Refusal::InvalidValue {
            field,
            reason: reason.into(),
        })
    };
    type Change = fn(&mut DecisionRequest);
    let cases: [(Change, &'static str, &str); 11] = [
        (
            |r| r.requester = Address::Owner,
            "requester",
            "must be an agent or system",
        ),
        (|r| r.kind = DecisionKind::Unknown, "kind", "must be known"),
        (|r| r.title.clear(), "title", "must not be empty"),
        (|r| r.options.clear(), "options", "need at least two"),
        (|r| r.options.truncate(1), "options", "need at least two"),
        (
            |r| r.options.push(String::new()),
            "options",
            "must not be empty",
        ),
        (
            |r| r.options.push("a".into()),
            "options",
            "must be distinct",
        ),
        (
            |r| r.recommended = Some("c".into()),
            "recommended",
            "must be one of the options",
        ),
        (
            |r| r.recommended = Some(String::new()),
            "recommended",
            "must be one of the options",
        ),
        (
            |r| r.deadline = Some(at(10)),
            "deadline",
            "must be in the future",
        ),
        (
            |r| r.deadline = Some(at(9)),
            "deadline",
            "must be in the future",
        ),
    ];
    for (change, field, reason) in cases {
        let mut rig = Rig::new();
        let before = rig.state.clone();
        let mut request = request();
        change(&mut request);
        assert_eq!(
            rig.run(DecisionCommand::Request(request), at(10)),
            invalid(field, reason),
            "{field}: {reason}"
        );
        assert_eq!(rig.state, before);
    }

    // References are checked first, as for every command.
    let mut rig = Rig::new();
    let mut unknown = request();
    unknown.requester = "p/stranger".parse().unwrap();
    unknown.title.clear();
    assert_eq!(
        rig.run(DecisionCommand::Request(unknown), at(10)),
        Err(Refusal::Invariant(ApplyError::UnknownAgent(
            "p/stranger".parse().unwrap()
        )))
    );

    // Accepted: a known agent or system requester, no recommendation, no deadline,
    // a deadline one second ahead, empty context and files, many options.
    let changes: [Change; 6] = [
        |r| r.requester = Address::System,
        |r| r.recommended = None,
        |r| r.deadline = None,
        |r| r.deadline = Some(at(11)),
        |r| {
            r.context.clear();
            r.files.clear();
        },
        |r| r.options.extend(["c".to_owned(), "d".to_owned()]),
    ];
    let mut rig = Rig::new();
    for change in changes {
        let mut request = request();
        change(&mut request);
        let events = rig
            .run(DecisionCommand::Request(request.clone()), at(10))
            .unwrap();
        assert_eq!(events, [OrgEvent::DecisionRequested(request.clone())]);
        let decision = rig.state.decision(&d(rig.seq)).unwrap();
        assert_eq!(decision.request, request);
        assert_eq!(decision.status, DecisionStatus::Pending);
        assert_eq!(decision.requested_at, at(rig.seq as u32));
    }
    assert_eq!(rig.state.decisions().count(), 6);
}

#[test]
fn commands_on_an_unknown_decision_are_refused() {
    let (mut rig, _) = Rig::with(DecisionStatus::Pending);
    let missing = d(999);
    for command in [
        take(missing, "a"),
        resolve(missing, "a"),
        DecisionCommand::Expire {
            decision_id: missing,
        },
        withdraw(missing, Address::Owner),
    ] {
        assert_eq!(
            rig.run(command, at(40)),
            Err(Refusal::Invariant(ApplyError::UnknownDecision(missing)))
        );
    }
    // An unknown `by` is a reference error too, before any policy.
    let (mut rig, id) = Rig::with(DecisionStatus::Pending);
    let stranger: Address = "p/stranger".parse().unwrap();
    assert_eq!(
        rig.run(withdraw(id, stranger), at(40)),
        Err(Refusal::Invariant(ApplyError::UnknownAgent(
            "p/stranger".parse().unwrap()
        )))
    );
}

#[test]
fn refusals_read_as_sentences() {
    let (mut rig, id) = Rig::with(DecisionStatus::Expired);
    let text = |result: Result<Vec<OrgEvent>, Refusal>| result.unwrap_err().to_string();
    assert_eq!(
        text(rig.run(take(id, "a"), at(40))),
        format!("cannot take provisionally decision {id} while Expired")
    );
    let mut resolution = answer(id, "a");
    resolution.by = other();
    assert_eq!(
        text(rig.run(DecisionCommand::Resolve(resolution), at(40))),
        format!("p/other may not resolve decision {id}")
    );
    let (mut rig, id) = Rig::with(DecisionStatus::Pending);
    assert_eq!(
        text(rig.run(resolve(id, "c"), at(40))),
        format!("\"c\" is not an option of decision {id}")
    );
    assert_eq!(
        text(rig.run(take(id, "b"), at(40))),
        format!(
            "decision {id} can only be taken provisionally on its recommended option, not \"b\""
        )
    );
    assert_eq!(
        text(rig.run(DecisionCommand::Expire { decision_id: id }, at(0))),
        format!("decision {id} is open until 2026-01-15 12:00:30 UTC")
    );
}
