//! Decision builders and the golden log. The log file is the one place that
//! holds the six `decision.*` bodies: the codec, fold and replay tests read it.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)] // test builders only
use atm_org::*;
use chrono::{DateTime, Utc};
use serde_json::Value;

pub const DECISION_KINDS: [&str; 6] = [
    "decision.requested",
    "decision.taken_provisionally",
    "decision.resolved",
    "decision.overruled",
    "decision.expired",
    "decision.withdrawn",
];

#[derive(Debug, Clone)]
pub struct Line {
    pub seq: u64,
    pub ts: DateTime<Utc>,
    pub kind: String,
    pub body: Value,
}

/// Synthetic: hand-written lines in the daemon's `{"seq","ts","kind","body"}`
/// log shape, one or more per decision kind. No harness payload is involved.
pub fn golden_log() -> Vec<Line> {
    include_str!("../fixtures/decision_log.jsonl")
        .lines()
        .map(|line| {
            let value: Value = serde_json::from_str(line).unwrap();
            Line {
                seq: value["seq"].as_u64().unwrap(),
                ts: value["ts"].as_str().unwrap().parse().unwrap(),
                kind: value["kind"].as_str().unwrap().into(),
                body: value["body"].clone(),
            }
        })
        .collect()
}

/// The first golden body of a kind, decoded.
pub fn golden_event(kind: &str) -> OrgEvent {
    let line = golden_log().into_iter().find(|l| l.kind == kind).unwrap();
    OrgEvent::from_parts(kind, &line.body).unwrap().unwrap()
}

pub fn builder() -> AgentId {
    "p/builder".parse().unwrap()
}

/// The golden request: changes the intent, can be undone, recommends `a`.
pub fn request() -> DecisionRequest {
    match golden_event("decision.requested") {
        OrgEvent::DecisionRequested(request) => request,
        other => panic!("not a request: {other:?}"),
    }
}

/// One event per decision kind, for the crate-wide generators (slots 0..6).
pub fn decision_event_case(slot: usize, text: String, n: u64, flag: bool) -> OrgEvent {
    let decision_id = DecisionId::new(n);
    let by = if flag {
        Address::Owner
    } else {
        Address::Agent(builder())
    };
    let resolution = DecisionResolution {
        decision_id,
        option: text.clone(),
        by: by.clone(),
        note: flag.then(|| text.clone()),
        standing_rule: flag,
    };
    match slot {
        0 => {
            let mut request = request();
            request.title.clone_from(&text);
            request.context.clone_from(&text);
            request.files = vec![text.clone()];
            request.options = vec![text.clone(), format!("{n}")];
            request.recommended = flag.then_some(text);
            request.may_be_undone = n.is_multiple_of(2);
            request.changes_intent = flag;
            if !flag {
                request.deadline = None;
            }
            request.kind = match n % 6 {
                0 => DecisionKind::Action,
                1 => DecisionKind::Hire,
                2 => DecisionKind::Budget,
                3 => DecisionKind::Policy,
                4 => DecisionKind::Plan,
                _ => DecisionKind::Dialog,
            };
            OrgEvent::DecisionRequested(request)
        }
        1 => OrgEvent::DecisionTakenProvisionally(ProvisionalChoice {
            decision_id,
            option: text,
            by,
        }),
        2 => OrgEvent::DecisionResolved(resolution),
        3 => OrgEvent::DecisionOverruled(resolution),
        4 => OrgEvent::DecisionExpired { decision_id },
        _ => OrgEvent::DecisionWithdrawn(DecisionWithdrawal {
            decision_id,
            by,
            reason: flag.then_some(text),
        }),
    }
}
