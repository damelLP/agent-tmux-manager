//! Run this file and its frozen synthetic fixture on the unmodified rollback reader, too.
use atm_org::{AgentStatus, DeliveryStatus, MessageKind, OrgState, ReplayStats, TaskState};
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
struct Row {
    seq: u64,
    ts: chrono::DateTime<chrono::Utc>,
    kind: String,
    body: Value,
}

// Break caught: older validation refuses the acceptance or replay silently skips a row.
#[test]
fn rollback_reader_applies_carry_back_without_new_event_types() {
    let mut state = OrgState::default();
    let mut stats = ReplayStats::default();
    let originals = "t7".parse().unwrap();
    let sources = ["t12".parse().unwrap(), "t15".parse().unwrap()];
    let mut before = None;
    for line in include_str!("fixtures/task_carry_back_rollback.jsonl").lines() {
        let row: Row = serde_json::from_str(line).unwrap();
        if row.seq == 20 {
            assert_eq!(
                state.task(&originals).unwrap().state,
                TaskState::ChangesRequested
            );
            assert_eq!(
                state.agent(&"p/worker".parse().unwrap()).unwrap().status,
                AgentStatus::Down
            );
            before = Some(sources.each_ref().map(|id| state.task(id).unwrap().clone()));
        }
        stats
            .apply_parts(&mut state, row.seq, row.ts, &row.kind, &row.body)
            .unwrap();
    }
    assert_eq!(stats.applied, 22);
    assert_eq!(stats.skipped_unknown, 0);
    let original = state.task(&originals).unwrap();
    assert_eq!(original.state, TaskState::Accepted);
    assert_eq!(original.agent_id, Some("p/worker".parse().unwrap()));
    assert_eq!(
        original.reason.as_deref(),
        Some("Accepted via fix t12 (review t15); Covers the missing case.")
    );
    assert_eq!(original.cost.as_ref().unwrap().changes_requested, Some(1));
    assert_eq!(
        sources.each_ref().map(|id| state.task(id).unwrap().clone()),
        before.unwrap()
    );
    let notes = state.inbox(&"p/creator".parse().unwrap());
    assert_eq!(notes.len(), 1);
    let note = &notes[0];
    assert_eq!(note.status, DeliveryStatus::Queued);
    assert_eq!(note.content.task, Some(originals));
    assert_eq!(note.content.kind, MessageKind::Note);
    assert_eq!(note.content.from, "p/lead".parse().unwrap());
    assert!(note
        .content
        .body
        .starts_with("Task t7 accepted via fix t12 (review t15).\n"));
}
