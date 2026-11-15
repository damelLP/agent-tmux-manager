//! Worker ownership and partial-write contracts; gates are events, not delays.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use crate::server::methods::{CallerId, CallerPane};
use atm_org::{TaskCreated, TaskMeasure};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

fn caller() -> CallContext {
    CallContext {
        client_id: CallerId("worker-test".into()),
        from_pane: Some(CallerPane("%1".into())),
    }
}
fn created(title: String) -> TaskCreated {
    TaskCreated {
        title,
        kind: TaskKind::Work,
        creator: Address::Owner,
        parent: None,
        judges: None,
        measure: TaskMeasure::Owner,
        budget: None,
        files: vec![],
        rests_on: None,
    }
}
async fn org() -> (tempfile::TempDir, OrgHandle, CancellationToken) {
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let org = OrgHandle::open(dir.path().into(), cancel.clone()).await;
    for command in [
        OrgCommand::RegisterProject {
            project_id: "p".parse().unwrap(),
            root: "/p".into(),
            move_root: false,
        },
        OrgCommand::Adopt {
            agent_id: "p/worker".parse().unwrap(),
            pane: "%1".into(),
            session_id: None,
        },
        OrgCommand::Task(TaskCommand::Create(created("Work".into()))),
    ] {
        org.execute(command, chrono::Utc::now()).await.unwrap();
    }
    (dir, org, cancel)
}
fn assign(_: TaskActor) -> TaskCommand {
    TaskCommand::Assign {
        task_id: "t3".parse().unwrap(),
        agent_id: "p/worker".parse().unwrap(),
        by: TaskActor {
            who: Address::Owner,
            lead: false,
        },
    }
}

// Break caught: waiter cancellation drops a post-commit enqueue or releases its permit early.
#[tokio::test]
async fn cancelled_waiter_leaves_worker_to_finish_enqueue_exactly_once() {
    retained_worker(false).await;
}
// Break caught: the service timeout aborts admitted work or permits replacement workers.
#[tokio::test]
async fn timed_out_waiter_leaves_worker_to_finish_enqueue_exactly_once() {
    retained_worker(true).await;
}
async fn retained_worker(timeout: bool) {
    let (_dir, org, cancel) = org().await;
    let tasks = TaskHandle::new(org.clone());
    let workers = tasks.workers.clone();
    let (entered, entry) = oneshot::channel();
    let (release, gate) = oneshot::channel();
    let (done, finished) = oneshot::channel();
    let sender = org.clone();
    let call = tokio::spawn(async move {
        tasks
            .run(execute_mutation(
                tasks.org.clone(),
                caller(),
                assign,
                move |content| async move {
                    entered.send(()).unwrap();
                    gate.await.unwrap();
                    let result = enqueue(sender, content).await;
                    done.send(()).unwrap();
                    result
                },
            ))
            .await
    });
    entry.await.unwrap();
    if timeout {
        // All real log I/O has completed; only an in-memory gate is in flight.
        tokio::time::pause();
        assert!(matches!(call.await.unwrap(), Err(TaskError::Timeout)));
        tokio::time::resume();
    } else {
        call.abort();
        assert!(call.await.unwrap_err().is_cancelled());
    }
    assert!(workers.clone().try_acquire_many_owned(64).is_err());
    release.send(()).unwrap();
    finished.await.unwrap();
    drop(workers.acquire_many_owned(64).await.unwrap());
    let state = org.snapshot().await.unwrap();
    assert_eq!(
        state.task(&"t3".parse().unwrap()).unwrap().state,
        TaskState::Assigned
    );
    let messages: Vec<_> = state.messages().collect();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].content.task.as_ref().unwrap().to_string(), "t3");
    cancel.cancel();
}

// Break caught: admission parks more callers, or cancellation creates more than 64 workers.
#[tokio::test]
async fn full_pool_refuses_the_next_mutation_without_waiting_for_a_slot() {
    let tasks = TaskHandle::new(OrgHandle::disabled("not used by admission"));
    let release = CancellationToken::new();
    let (entered, mut entry) = mpsc::unbounded_channel();
    let mut calls = Vec::new();
    for _ in 0..64 {
        let tasks = tasks.clone();
        let gate = release.clone();
        let entered = entered.clone();
        calls.push(tokio::spawn(async move {
            tasks
                .run(async move {
                    entered.send(()).unwrap();
                    gate.cancelled().await;
                    Ok(())
                })
                .await
        }));
    }
    for _ in 0..64 {
        entry.recv().await.unwrap();
    }
    assert!(matches!(
        tasks.run(async { Ok(()) }).await,
        Err(TaskError::Unavailable(_))
    ));
    release.cancel();
    for call in calls {
        call.await.unwrap().unwrap();
    }
    tasks.run(async { Ok(()) }).await.unwrap();
}

// Break caught: enqueue failure hides the committed task or claims no effect/delivery.
#[tokio::test]
async fn enqueue_failure_is_a_partial_result_not_a_failed_transition() {
    let (_dir, org, cancel) = org().await;
    let result = execute_mutation(org.clone(), caller(), assign, |_| async {
        Err(TaskError::Organisation(OrgServiceError::Unavailable(
            "unknown append outcome".into(),
        )))
    })
    .await
    .unwrap();
    let result = serde_json::to_value(result).unwrap();
    assert_eq!(result["task_id"], "t3");
    assert_eq!(result["changed"], true);
    assert_eq!(result["notification"]["status"], "unconfirmed");
    assert_eq!(result["notification"]["error"]["code"], "unavailable");
    assert!(result["notification"]["error"]["message"]
        .as_str()
        .unwrap()
        .contains("task t3 committed"));
    let state = org.snapshot().await.unwrap();
    assert_eq!(
        state.task(&"t3".parse().unwrap()).unwrap().state,
        TaskState::Assigned
    );
    assert_eq!(state.messages().count(), 0);
    cancel.cancel();
}

// Break caught: restart synthesizes messages for a task-only durable prefix, contrary to the approved limit.
#[tokio::test]
async fn task_only_durable_prefix_replays_without_fabricating_a_message() {
    let (dir, org, cancel) = org().await;
    let (entered, entry) = oneshot::channel();
    let task = tokio::spawn(execute_mutation(
        org.clone(),
        caller(),
        assign,
        |_| async move {
            entered.send(()).unwrap();
            std::future::pending::<Result<MessageId, TaskError>>().await
        },
    ));
    entry.await.unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    // Copy the real committed log while no append is in flight, giving the
    // next lifetime its own stream lock, as message persistence tests do.
    let next = tempfile::tempdir().unwrap();
    copy(dir.path(), next.path());
    cancel.cancel();
    let stop = CancellationToken::new();
    let reopened = OrgHandle::open(next.path().into(), stop.clone()).await;
    let state = reopened.snapshot().await.unwrap();
    assert_eq!(
        state.task(&"t3".parse().unwrap()).unwrap().state,
        TaskState::Assigned
    );
    assert_eq!(state.messages().count(), 0);
    stop.cancel();
}
fn copy(from: &std::path::Path, to: &std::path::Path) {
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            std::fs::create_dir_all(&target).unwrap();
            copy(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

// Break caught: list pagination uses lexicographic IDs, silently truncates, or emits empty continuations.
#[test]
fn list_byte_and_count_pages_preserve_numeric_order_and_full_entries() {
    let mut state = OrgState::default();
    let now = chrono::Utc::now();
    for seq in 1..=600 {
        state
            .apply(seq, now, &OrgEvent::TaskCreated(created("é".repeat(512))))
            .unwrap();
    }
    let p: ListParams = serde_json::from_value(serde_json::json!({"limit":1000})).unwrap();
    let page = list(&state, &p).unwrap();
    assert!(page.tasks.len() > 400 && page.tasks.len() < 600);
    assert!(encoded_size(&page).unwrap() <= PAGE_BYTES);
    assert_eq!(page.tasks.get(8).unwrap().task_id.to_string(), "t9");
    assert_eq!(page.tasks.get(9).unwrap().task_id.to_string(), "t10");
    let after = page.next_after.clone().unwrap();
    let p: ListParams =
        serde_json::from_value(serde_json::json!({"after":after,"limit":1000})).unwrap();
    let rest = list(&state, &p).unwrap();
    assert_eq!(page.tasks.len() + rest.tasks.len(), 600);
    assert!(rest.next_after.is_none());
    let p: ListParams =
        serde_json::from_value(serde_json::json!({"after":"t8","limit":2})).unwrap();
    let page = list(&state, &p).unwrap();
    assert_eq!(
        page.tasks
            .iter()
            .map(|t| t.task_id.to_string())
            .collect::<Vec<_>>(),
        ["t9", "t10"]
    );
    assert_eq!(page.next_after.unwrap().to_string(), "t10");
    let mut huge = OrgState::default();
    huge.apply(
        1,
        now,
        &OrgEvent::TaskCreated(created("x".repeat(PAGE_BYTES))),
    )
    .unwrap();
    assert!(matches!(list(&huge,&p),Ok(ListResult { tasks, .. }) if tasks.is_empty()));
    let p: ListParams = serde_json::from_value(serde_json::json!({})).unwrap();
    assert!(matches!(list(&huge, &p), Err(TaskError::Refused(_))));
}
