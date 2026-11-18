//! Exercise the held sender and real dispatcher, never an independent health lane.
use super::*;
use serde_json::json;

fn service(sender: mpsc::Sender<LifecycleJob>) -> AgentHandle {
    let mut handle = AgentHandle::disabled("health test, no runtime needed");
    Arc::get_mut(&mut handle.inner).unwrap().lifecycle = Some(sender);
    handle
}

fn state(handle: &AgentHandle) -> serde_json::Value {
    serde_json::to_value(handle.lifecycle_health()).unwrap()
}

#[test]
fn lifecycle_health_absent_without_organisation() {
    assert_eq!(
        state(&AgentHandle::disabled("no organisation")),
        json!({"state":"absent", "capacity":32})
    );
}

#[tokio::test]
async fn lifecycle_health_panicking_job_closes_the_held_sender() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let (sender, receive) = mpsc::channel(32);
        let handle = service(sender.clone());
        let dispatcher = tokio::spawn(dispatch_lifecycle(receive, CancellationToken::new()));
        let (admitted, admission) = oneshot::channel();
        assert!(sender
            .try_send(LifecycleJob {
                agent: "p/panic".parse().unwrap(),
                deadline: Instant::now() + Duration::from_secs(60),
                admitted,
                run: Box::pin(async { panic!("test-only lifecycle fault") }),
            })
            .is_ok());
        admission.await.unwrap().unwrap();
        dispatcher.await.unwrap();
        assert_eq!(state(&handle), json!({"state":"stopped", "capacity":32}));
        assert_eq!(state(&handle), json!({"state":"stopped", "capacity":32}));
    })
    .await
    .expect("dispatcher did not finish after panic");
}

#[tokio::test]
async fn lifecycle_health_full_queue_recovers_after_dispatch_without_closing() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let (sender, receive) = mpsc::channel(32);
        let handle = service(sender.clone());
        let cancel = CancellationToken::new();
        let dispatcher_cancel = cancel.clone();
        let (start, started) = oneshot::channel();
        let dispatcher = tokio::spawn(async move {
            started.await.unwrap();
            dispatch_lifecycle(receive, dispatcher_cancel).await;
        });
        assert_eq!(state(&handle), json!({"state":"ok", "capacity":32}));
        let mut jobs = Vec::new();
        // Hold dispatch itself so all 32 queued jobs are parked, not a race
        // against the receiver draining them. Full describes sender capacity,
        // not active jobs: after admission all 32 may still run with state ok.
        for n in 0..32 {
            let (admitted, admission) = oneshot::channel();
            let (release, released) = oneshot::channel();
            let (reply, replied) = oneshot::channel();
            assert!(sender
                .try_send(LifecycleJob {
                    agent: format!("p/a{n}").parse().unwrap(),
                    deadline: Instant::now() + Duration::from_secs(60),
                    admitted,
                    run: Box::pin(async move {
                        released.await.unwrap();
                        Box::new(move || {
                            reply.send(()).unwrap();
                        }) as Reply
                    }),
                })
                .is_ok());
            jobs.push((admission, release, replied));
        }
        assert_eq!(state(&handle), json!({"state":"full", "capacity":32}));
        start.send(()).unwrap();
        let mut finishes = Vec::new();
        for (admission, release, replied) in jobs {
            admission.await.unwrap().unwrap();
            finishes.push((release, replied));
        }
        assert_eq!(state(&handle), json!({"state":"ok", "capacity":32}));
        for (release, replied) in finishes {
            release.send(()).unwrap();
            replied.await.unwrap();
        }
        assert_eq!(state(&handle), json!({"state":"ok", "capacity":32}));
        cancel.cancel();
        dispatcher.await.unwrap();
        assert_eq!(state(&handle), json!({"state":"stopped", "capacity":32}));
    })
    .await
    .expect("lifecycle health test hung");
}

#[tokio::test]
async fn lifecycle_health_closed_takes_precedence_over_full() {
    let (sender, mut receive) = mpsc::channel(32);
    let handle = service(sender.clone());
    for n in 0..32 {
        assert!(sender
            .try_send(LifecycleJob {
                agent: format!("p/a{n}").parse().unwrap(),
                deadline: Instant::now() + Duration::from_secs(60),
                admitted: oneshot::channel().0,
                run: Box::pin(async { Box::new(|| {}) as Reply }),
            })
            .is_ok());
    }
    receive.close();
    assert_eq!(state(&handle), json!({"state":"stopped", "capacity":32}));
}
