//! Synthetic neutral usage through the real actor and task.get method router.
//! No raw harness schema, second counter, tmux or model is involved.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use atm_core::{Money, SessionId};
use atm_org::{Address, OrgCommand, TaskActor, TaskCommand, TaskCreated, TaskUsageObservation};
use atmd::{
    org::{tasks::register_task_methods, OrgHandle},
    server::methods::{CallContext, CallerId, MethodRouter},
};
use chrono::{DateTime, Utc};
use serde_json::json;
use tokio_util::sync::CancellationToken;

// Break caught: budget_status sees absent/final Task.cost instead of T4a's
// live task_cost, or the standard committed message is not reflected in get.
#[tokio::test]
async fn task_get_budget_status_reads_the_running_task_ledger() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let org = OrgHandle::open(dir.path().to_owned(), cancel.clone()).await;
    let now: DateTime<Utc> = "2026-11-15T12:00:00Z".parse().unwrap();
    for command in [
        OrgCommand::RegisterProject {
            project_id: "p".parse().unwrap(),
            root: "/p".into(),
            move_root: false,
        },
        OrgCommand::Adopt {
            agent_id: "p/build".parse().unwrap(),
            pane: "%1".into(),
            session_id: Some(SessionId::new("s1")),
        },
    ] {
        org.execute(command, now).await.unwrap();
    }
    let created: TaskCreated = serde_json::from_value(json!({
        "title":"Live budget", "kind":"work", "creator":"owner", "measure":{"kind":"owner"},
        "budget":{"cost":{"microdollars":0}, "turns":0}, "files":[]
    }))
    .unwrap();
    let made = org
        .execute(OrgCommand::Task(TaskCommand::Create(created)), now)
        .await
        .unwrap();
    let task_id: atm_org::TaskId = format!("t{}", made.first().unwrap().seq).parse().unwrap();
    org.execute(
        OrgCommand::Task(TaskCommand::Assign {
            task_id: task_id.clone(),
            agent_id: "p/build".parse().unwrap(),
            by: TaskActor {
                who: Address::Owner,
                lead: false,
            },
        }),
        now,
    )
    .await
    .unwrap();
    org.execute(
        OrgCommand::ObserveTaskUsage(TaskUsageObservation {
            session_id: SessionId::new("s1"),
            pane: Some("%1".into()),
            turn: true,
            cumulative_cost: Some(Money::zero()),
            context_percent: None,
        }),
        now,
    )
    .await
    .unwrap();

    let mut router = MethodRouter::default();
    register_task_methods(&mut router, org.clone()).unwrap();
    let caller = || CallContext {
        client_id: CallerId("budget-test".into()),
        from_pane: None,
    };
    let first = router
        .call(caller(), "task.get", json!({"task_id":task_id}))
        .await
        .unwrap();
    assert_eq!(first["budget_status"]["crossed"], json!(["turns"]));
    assert_eq!(
        first["budget_status"]["notification"],
        serde_json::Value::Null
    );
    assert_eq!(first["task"]["cost"], serde_json::Value::Null);

    org.execute(
        OrgCommand::ObserveTaskUsage(TaskUsageObservation {
            session_id: SessionId::new("s1"),
            pane: Some("%1".into()),
            turn: false,
            cumulative_cost: Some(Money::from_microdollars(1)),
            context_percent: None,
        }),
        now,
    )
    .await
    .unwrap();
    let state = org.snapshot().await.unwrap();
    let note = atm_org::budget::budget_note(state.task(&task_id).unwrap()).unwrap();
    let sent = org
        .execute(OrgCommand::SendMessage(note), now)
        .await
        .unwrap();
    let second = router
        .call(caller(), "task.get", json!({"task_id":task_id}))
        .await
        .unwrap();
    assert_eq!(second["budget_status"]["crossed"], json!(["cost", "turns"]));
    assert_eq!(
        second["budget_status"]["notification"],
        format!("m{}", sent.first().unwrap().seq)
    );
    assert_eq!(second["task"]["state"], "assigned");
    cancel.cancel();
}
