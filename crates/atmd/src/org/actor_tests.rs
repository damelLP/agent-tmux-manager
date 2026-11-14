use super::*;
use crate::org::OrgHandle;
use atm_org::{AgentStatus, OrgEvent};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio::sync::oneshot;

fn now() -> DateTime<Utc> {
    DateTime::from_timestamp(1_700_000_000, 0).unwrap()
}

fn project() -> OrgCommand {
    OrgCommand::RegisterProject {
        project_id: "demo".parse().unwrap(),
        root: "/demo".into(),
        move_root: false,
    }
}

fn spawn<F, Fut>(append: F) -> (OrgHandle, CancellationToken, tokio::task::JoinHandle<()>)
where
    F: FnMut(Envelope) -> Fut + Send + 'static,
    Fut: Future<Output = Result<u64, EventLogError>> + Send + 'static,
{
    let (requests, receive) = mpsc::channel(64);
    let (health, status) = watch::channel(Ok(0));
    let cancel = CancellationToken::new();
    let task = tokio::spawn(run(
        OrgState::default(),
        receive,
        health,
        cancel.clone(),
        append,
    ));
    (
        OrgHandle {
            requests,
            health: status,
        },
        cancel,
        task,
    )
}

// Deliberately control only the disk boundary; decisions/apply/queue are real.
#[tokio::test]
async fn cancelled_reply_does_not_cancel_command_or_publish_early() {
    let (entered, mut entry) = mpsc::channel(1);
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let gate = release.clone();
    let (org, cancel, task) = spawn(move |_| {
        let entered = entered.clone();
        let gate = gate.clone();
        async move {
            entered.send(()).await.unwrap();
            let _permit = gate.acquire().await.unwrap();
            Ok(1)
        }
    });
    let caller = org.clone();
    let caller = tokio::spawn(async move { caller.execute(project(), now()).await });
    entry.recv().await.unwrap();
    assert_eq!(org.status().unwrap(), 0, "pending sync is not committed");
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    release.add_permits(1);
    assert_eq!(org.snapshot().await.unwrap().projects().count(), 1);
    assert_eq!(org.status().unwrap(), 1);
    cancel.cancel();
    task.await.unwrap();
}

#[tokio::test]
async fn write_errors_stop_decisions_and_state_reads_until_restart() {
    for error in [
        EventLogError::Io {
            path: "events".into(),
            source: std::io::Error::other("disk failed"),
        },
        EventLogError::WriteOutcomeUnknown {
            path: "events".into(),
            source: std::io::Error::other("sync failed"),
        },
        EventLogError::TimeoutAfterHandover,
        EventLogError::Closed,
        EventLogError::SequenceExhausted,
    ] {
        let mut error = Some(error);
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let (org, _, task) = spawn(move |_| {
            count.fetch_add(1, Ordering::SeqCst);
            std::future::ready(Err(error.take().expect("no retries")))
        });
        assert!(matches!(
            org.execute(project(), now()).await,
            Err(OrgServiceError::Unavailable(_))
        ));
        assert!(matches!(
            org.execute(project(), now()).await,
            Err(OrgServiceError::Unavailable(_))
        ));
        assert!(org.snapshot().await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        task.await.unwrap();
    }
}

#[tokio::test]
async fn before_handover_timeout_keeps_state_known_and_never_auto_retries() {
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let (org, cancel, task) = spawn(move |_| {
        let n = count.fetch_add(1, Ordering::SeqCst);
        std::future::ready(if n == 0 {
            Err(EventLogError::TimeoutBeforeHandover)
        } else {
            Ok(1)
        })
    });
    assert!(matches!(
        org.execute(project(), now()).await,
        Err(OrgServiceError::Timeout)
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(org.status().unwrap(), 0);
    assert_eq!(org.snapshot().await.unwrap().projects().count(), 0);
    org.execute(project(), now()).await.unwrap();
    assert_eq!(org.snapshot().await.unwrap().projects().count(), 1);
    cancel.cancel();
    task.await.unwrap();
}

#[tokio::test]
async fn failed_second_event_preserves_committed_prefix() {
    let mut state = OrgState::default();
    let id = "demo/lead".parse().unwrap();
    state
        .apply(
            1,
            now(),
            &OrgEvent::ProjectRegistered {
                project_id: "demo".parse().unwrap(),
                root: "/demo".into(),
            },
        )
        .unwrap();
    state
        .apply(
            2,
            now(),
            &OrgEvent::AgentAdopted {
                agent_id: id,
                pane: "%1".into(),
                session_id: None,
            },
        )
        .unwrap();
    let command = OrgCommand::Respawn {
        agent_id: "demo/lead".parse().unwrap(),
        role: atm_org::parse_role("lead", "+++\nharness='pi'\n+++\nInstructions").unwrap(),
        reason: "restart".into(),
    };
    let (health, status) = watch::channel(Ok(2));
    let mut kinds = Vec::new();
    let result = execute(
        &mut state,
        &command,
        now(),
        &health,
        &mut |envelope: Envelope| {
            kinds.push(envelope.kind);
            std::future::ready(if kinds.len() == 1 {
                Ok(3)
            } else {
                Err(EventLogError::TimeoutAfterHandover)
            })
        },
    )
    .await;
    assert!(result.is_err());
    assert_eq!(kinds, ["agent.down", "agent.up_requested"]);
    assert_eq!(
        state.agent(&"demo/lead".parse().unwrap()).unwrap().status,
        AgentStatus::Down
    );
    assert!(status.borrow().is_err());
}

#[tokio::test(start_paused = true)]
async fn caller_timeout_does_not_cancel_a_later_commit() {
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let gate = release.clone();
    let (org, cancel, task) = spawn(move |_| {
        let gate = gate.clone();
        async move {
            let _permit = gate.acquire().await.unwrap();
            Ok(1)
        }
    });
    assert!(matches!(
        org.execute(project(), now()).await,
        Err(OrgServiceError::Timeout)
    ));
    assert_eq!(org.status().unwrap(), 0);
    release.add_permits(1);
    assert_eq!(org.snapshot().await.unwrap().projects().count(), 1);
    cancel.cancel();
    task.await.unwrap();
}

#[tokio::test]
async fn queue_overload_is_an_explicit_refusal_not_unbounded_work() {
    let (requests, _receive) = mpsc::channel(1);
    let (_health, status) = watch::channel(Ok(0));
    let org = OrgHandle {
        requests,
        health: status,
    };
    let (reply, _) = oneshot::channel();
    org.submit(Request::Snapshot(reply)).unwrap();
    let (reply, _) = oneshot::channel();
    let error = org.submit(Request::Snapshot(reply)).unwrap_err();
    assert!(error.to_string().contains("queue"));
    assert_eq!(org.status().unwrap(), 0);
}
