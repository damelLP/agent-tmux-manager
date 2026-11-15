//! Synthetic golden history: exact integer cost and deterministic supplied time.
//! Breaks caught: dropping observations on restart, recomputing accepted time at
//! restart, loss of exact money, and prefix-dependent accumulation.
use atm_core::Money;
use atm_org::*;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Deserialize)]
struct Row {
    seq: u64,
    ts: DateTime<Utc>,
    kind: String,
    body: Value,
}
fn rows() -> Vec<Row> {
    include_str!("fixtures/task_cost_log.jsonl")
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect()
}
fn fold(state: &mut OrgState, rows: &[Row]) {
    let mut stats = ReplayStats::default();
    for row in rows {
        stats
            .apply_parts(state, row.seq, row.ts, &row.kind, &row.body)
            .unwrap();
    }
}
#[test]
fn observation_kind_is_recognized_and_money_roundtrips_exactly() {
    let body = json!({"session_id":"s","task_id":null,"turn":true,"cumulative_cost":{"microdollars":i64::MAX},"context_percent":70});
    let event = OrgEvent::from_parts("task.usage_observed", &body)
        .unwrap()
        .expect("new observation kind must be recognized");
    assert_eq!(event.to_body(), body);
    assert!(OrgEvent::from_parts("task.usage_observed", &json!({})).is_err());
    assert_eq!(
        OrgEvent::from_parts("future.unknown", &json!([1])).unwrap(),
        None
    );
}
#[test]
fn replay_and_every_prefix_restore_running_and_frozen_cost() {
    let rows = rows();
    let mut all = OrgState::default();
    fold(&mut all, &rows);
    let id: TaskId = "t3".parse().unwrap();
    let now = "2027-01-01T00:00:00Z".parse().unwrap();
    let cost = all.task_cost(&id, now).unwrap();
    assert_eq!(cost.cost, Some(Money::from_microdollars(25_000)));
    assert_eq!(cost.turns, Some(1));
    assert_eq!(cost.peak_context_percent, Some(70));
    assert_eq!(cost.seconds_in_state.values().sum::<u64>(), 5);
    for split in 0..=rows.len() {
        let mut restored = OrgState::default();
        fold(&mut restored, &rows[..split]);
        fold(&mut restored, &rows[split..]);
        assert_eq!(restored, all);
    }
    let mut live = OrgState::default();
    fold(&mut live, &rows[..6]);
    assert_eq!(
        live.task_cost(&id, rows[5].ts).unwrap().cost,
        Some(Money::from_microdollars(25_000))
    );
    assert_eq!(
        live.task_cost(&id, rows[5].ts)
            .unwrap()
            .seconds_in_state
            .get(&TaskState::Assigned),
        Some(&2)
    );
}
