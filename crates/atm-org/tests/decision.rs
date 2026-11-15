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
        assert_eq!(event.to_body(), line.body, "{} seq {}", line.kind, line.seq);
        assert_eq!(
            OrgEvent::from_parts(event.kind(), &event.to_body()).unwrap(),
            Some(event)
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
        ("decision.requested", "kind", json!("other")),
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
    assert_eq!(ids, [3, 6, 9, 11, 13].map(d));

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
    assert_eq!(state.decision(&d(4)), None);
}

#[test]
fn fold_records_facts_without_enforcing_the_table() {
    // Facts the policy would refuse still fold: an overrule of a decision that
    // was never taken, a provisional choice on one that must wait, an answer
    // after the end, and a second provisional choice replacing the first.
    let mut state = folded();
    let overrule = OrgEvent::DecisionOverruled(DecisionResolution {
        decision_id: d(13),
        option: "elsewhere".into(),
        by: Address::Agent(builder()),
        note: None,
        standing_rule: false,
    });
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
            OrgEvent::DecisionResolved(resolution(d(4), &owner)),
            ApplyError::UnknownDecision(d(4)),
        ),
        (
            OrgEvent::DecisionResolved(resolution(d(13), &stranger)),
            unknown_agent.clone(),
        ),
        (
            OrgEvent::DecisionOverruled(resolution(d(4), &owner)),
            ApplyError::UnknownDecision(d(4)),
        ),
        (
            OrgEvent::DecisionOverruled(resolution(d(13), &stranger)),
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
