//! Task 2: real actor, in-memory synchronization, no sleeps or timing assertions.
//! Each test catches loss of exclusion, lifetime, fencing or bounded admission.
use super::*;
use crate::control::service::spawn_control;
use atm_tmux::MockTmuxClient;
use std::{future::poll_fn, task::Poll, time::Duration};
use tokio::{sync::oneshot, task::JoinHandle};
use tokio_util::sync::CancellationToken;

fn request(agent: &str, pane: Option<&str>, kind: ReservationKind) -> ReservationRequest {
    ReservationRequest {
        agent: agent.parse().unwrap(),
        pane: pane.map(|p| PaneId::parse(p).unwrap()),
        kind,
        wait: WaitPolicy::Wait,
        deadline: Instant::now() + Duration::from_secs(60),
    }
}

fn actor() -> (
    ControlHandle,
    CancellationToken,
    JoinHandle<()>,
    MockTmuxClient,
) {
    let mock = MockTmuxClient::new();
    let cancel = CancellationToken::new();
    let (control, task) = spawn_control(
        Box::new(mock.clone()),
        crate::registry::spawn_registry(),
        crate::org::OrgHandle::disabled("ownership test"),
        cancel.clone(),
    );
    (control, cancel, task, mock)
}

async fn noop(_: Reservation, _: Completion) -> Result<(), ReservationError> {
    Ok(())
}

// Poll through the bounded channel enqueue before returning. A subsequent
// prepare reply is an actor acknowledgement, not a scheduler/yield assumption.
async fn queued<W, WF, C, CF>(
    control: &ControlHandle,
    request: ReservationRequest,
    work: W,
    cleanup: C,
) -> JoinHandle<Result<(), ReservationError>>
where
    W: FnOnce(Reservation) -> WF + Send + 'static,
    WF: Future<Output = Result<(), ReservationError>> + Send + 'static,
    C: FnOnce(Reservation, Completion) -> CF + Send + 'static,
    CF: Future<Output = Result<(), ReservationError>> + Send + 'static,
{
    let control = control.clone();
    let mut future = Box::pin(async move { control.run_reserved(request, work, cleanup).await });
    poll_fn(|cx| match future.as_mut().poll(cx) {
        Poll::Pending => Poll::Ready(()),
        Poll::Ready(result) => panic!("job must enqueue before completion: {result:?}"),
    })
    .await;
    tokio::spawn(future)
}

async fn barrier(control: &ControlHandle) {
    control
        .prepare_spawn(Some("%999".into()), Some("/private".into()), None, None)
        .await
        .unwrap();
}

async fn held(
    control: &ControlHandle,
    request: ReservationRequest,
) -> (
    JoinHandle<Result<(), ReservationError>>,
    Reservation,
    oneshot::Sender<()>,
) {
    let (entered, entry) = oneshot::channel();
    let (release, released) = oneshot::channel();
    let job = queued(
        control,
        request,
        move |reservation| async move {
            entered.send(reservation).unwrap();
            released.await.unwrap();
            Ok(())
        },
        noop,
    )
    .await;
    (job, entry.await.unwrap(), release)
}

async fn ready_org(cancel: CancellationToken) -> (tempfile::TempDir, crate::org::OrgHandle) {
    use atm_org::OrgCommand;
    let dir = tempfile::tempdir().unwrap();
    let org = crate::org::OrgHandle::open(dir.path().into(), cancel).await;
    let now = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
    org.execute(
        OrgCommand::RegisterProject {
            project_id: "p".parse().unwrap(),
            root: "/private".into(),
            move_root: false,
        },
        now,
    )
    .await
    .unwrap();
    org.execute(
        OrgCommand::Adopt {
            agent_id: "p/a".parse().unwrap(),
            pane: "%1".into(),
            session_id: Some("old".into()),
        },
        now,
    )
    .await
    .unwrap();
    (dir, org)
}

#[tokio::test]
async fn reservation_clear_blocks_delivery_and_other_pane_completes() {
    use atm_org::{AgentStatus, OrgCommand};
    let (control, cancel, task, mock) = actor();
    let (_dir, org) = ready_org(cancel.clone()).await;
    let work_org = org.clone();
    let (clearing, cleared) = oneshot::channel();
    let (editor, confirmed) = oneshot::channel();
    let clear = queued(
        &control,
        request("p/a", Some("%1"), ReservationKind::Clear),
        move |r| async move {
            let now = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
            work_org
                .execute(
                    OrgCommand::Clear {
                        agent_id: r.agent().clone(),
                    },
                    now,
                )
                .await
                .unwrap();
            clearing.send(()).unwrap();
            confirmed.await.unwrap();
            work_org
                .execute(
                    OrgCommand::EditorReady {
                        agent_id: r.agent().clone(),
                    },
                    now,
                )
                .await
                .unwrap();
            Ok(())
        },
        noop,
    )
    .await;
    cleared.await.unwrap();
    let (entered, mut entry) = oneshot::channel();
    let delivery = queued(
        &control,
        request("p/a", Some("%1"), ReservationKind::Delivery),
        move |r| async move {
            let state = org.snapshot().await.unwrap();
            let agent = state.agent(r.agent()).unwrap();
            assert_eq!(agent.status, AgentStatus::Ready);
            assert_eq!(
                agent.excluded_session_id.as_ref().map(|id| id.as_str()),
                Some("old")
            );
            entered.send(()).unwrap();
            Ok(())
        },
        noop,
    )
    .await;
    barrier(&control).await;
    assert!(matches!(
        entry.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    // A's stalled readiness does not occupy the actor or B's supervisor.
    for n in 0..3 {
        mock.set_next_raw_output("ATM-PANE-v1\t%2\tprivate\t0\t123\t80\t24\t1\t0\t0\t1\t0\t0\n");
        mock.set_next_raw_output("p/b\n");
        if n == 1 {
            mock.set_next_raw_output("");
        } // set-option reply
    }
    control
        .run_reserved(
            request("p/b", Some("%2"), ReservationKind::Respawn),
            |r| async move {
                let pane = PaneId::parse("%2")?;
                assert!(r.inspect(&pane).await?.is_some());
                r.tag(&pane, ExpectedTag::Agent(r.agent().clone())).await?;
                r.paste(&pane, r.agent().clone(), "probe".into()).await?;
                Ok::<_, ReservationError>(())
            },
            noop,
        )
        .await
        .unwrap();
    assert!(matches!(
        entry.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    assert!(!mock
        .calls()
        .iter()
        .any(|c| matches!(c, atm_tmux::mock::MockCall::PasteText { pane, .. } if pane == "%1")));
    editor.send(()).unwrap();
    clear.await.unwrap().unwrap();
    entry.await.unwrap();
    delivery.await.unwrap().unwrap();
    cancel.cancel();
    task.await.unwrap();
}

#[tokio::test]
async fn reservation_split_claim_holds_delivery_until_recorded_and_released() {
    let client = testing::ScriptedTmuxClient::new();
    client.mock().set_next_pane_id("%99");
    let cancel = CancellationToken::new();
    let (control, task) = spawn_control(
        Box::new(client.clone()),
        crate::registry::spawn_registry(),
        crate::org::OrgHandle::disabled("ownership test"),
        cancel.clone(),
    );
    let (created, creation) = oneshot::channel();
    let (recorded, record) = oneshot::channel();
    let (finished, finish) = oneshot::channel();
    let scripts = client.clone();
    let up = queued(
        &control,
        request("p/a", None, ReservationKind::Up),
        move |r| async move {
            let pane = r
                .split(
                    SpawnContext::new("%0".into(), None, false),
                    PaneDirection::Below,
                    "50%".into(),
                )
                .await?;
            let mut info = PaneInspection::new(
                atm_tmux::PaneInfo {
                    pane_id: "%99".into(),
                    session_name: "private".into(),
                    window_index: 0,
                    pane_pid: 1,
                    width: 80,
                    height: 24,
                    is_active: true,
                },
                None,
                false,
                false,
                Some(true),
                0,
                0,
            );
            scripts.script_tag_check(&info).unwrap();
            r.tag(&pane, ExpectedTag::Absent).await?;
            created.send(()).unwrap();
            record.await.unwrap(); // consumer's durable PaneCreated acknowledgement
            info.local_agent_tag = Some("p/a".into());
            scripts.script_tag_check(&info).unwrap();
            r.paste(&pane, r.agent().clone(), "launch".into()).await?;
            scripts.script_tag_check(&info).unwrap();
            r.keys(&pane, r.agent().clone(), PaneKey::Enter).await?;
            finish.await.unwrap();
            Ok(())
        },
        noop,
    )
    .await;
    creation.await.unwrap();
    let (entered, mut entry) = oneshot::channel();
    let delivery = queued(
        &control,
        request("p/b", Some("%99"), ReservationKind::Delivery),
        move |_| async move {
            entered.send(()).unwrap();
            Ok(())
        },
        noop,
    )
    .await;
    barrier(&control).await;
    assert!(matches!(
        entry.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    assert!(!client
        .mock()
        .calls()
        .iter()
        .any(|c| matches!(c, atm_tmux::mock::MockCall::PasteText { .. })));
    recorded.send(()).unwrap();
    finished.send(()).unwrap();
    up.await.unwrap().unwrap();
    delivery.await.unwrap().unwrap();
    entry.await.unwrap();
    let input: Vec<_> = client
        .mock()
        .calls()
        .into_iter()
        .filter(|c| {
            matches!(
                c,
                atm_tmux::mock::MockCall::PasteText { .. }
                    | atm_tmux::mock::MockCall::SendKeys { .. }
            )
        })
        .collect();
    assert_eq!(
        input,
        vec![
            atm_tmux::mock::MockCall::PasteText {
                pane: "%99".into(),
                text: "launch".into()
            },
            atm_tmux::mock::MockCall::SendKeys {
                pane: "%99".into(),
                keys: "Enter".into()
            }
        ]
    );
    cancel.cancel();
    task.await.unwrap();
}

#[tokio::test]
async fn reservation_caller_drop_keeps_owner_through_cleanup() {
    use atm_org::{AgentStatus, OrgCommand};
    let (control, cancel, task, _) = actor();
    let (_dir, org) = ready_org(cancel.clone()).await;
    let work_org = org.clone();
    let cleanup_org = org.clone();
    let (entered, entry) = oneshot::channel();
    let (work_done, work) = oneshot::channel();
    let (cleaning, cleanup_entry) = oneshot::channel();
    let (cleaned, cleanup_done) = oneshot::channel();
    let caller = queued(
        &control,
        request("p/a", Some("%1"), ReservationKind::Clear),
        move |r| async move {
            work_org
                .execute(
                    OrgCommand::Clear {
                        agent_id: r.agent().clone(),
                    },
                    DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
                )
                .await
                .unwrap();
            entered.send(()).unwrap();
            work.await.unwrap();
            Ok(())
        },
        move |r, completion| async move {
            assert_eq!(completion, Completion::Succeeded);
            cleaning.send(()).unwrap();
            cleanup_done.await.unwrap();
            cleanup_org
                .execute(
                    OrgCommand::EditorReady {
                        agent_id: r.agent().clone(),
                    },
                    DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
                )
                .await
                .unwrap();
            Ok(())
        },
    )
    .await;
    entry.await.unwrap();
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    let (delivered, mut delivery_entry) = oneshot::channel();
    let delivery = queued(
        &control,
        request("p/a", Some("%1"), ReservationKind::Delivery),
        move |r| async move {
            assert_eq!(
                org.snapshot()
                    .await
                    .unwrap()
                    .agent(r.agent())
                    .unwrap()
                    .status,
                AgentStatus::Ready
            );
            delivered.send(()).unwrap();
            Ok(())
        },
        noop,
    )
    .await;
    barrier(&control).await;
    assert!(matches!(
        delivery_entry.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    work_done.send(()).unwrap();
    cleanup_entry.await.unwrap();
    barrier(&control).await;
    assert!(matches!(
        delivery_entry.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    cleaned.send(()).unwrap();
    delivery_entry.await.unwrap();
    delivery.await.unwrap().unwrap();
    cancel.cancel();
    task.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn reservation_caller_drop_before_grant_does_not_discard_admitted_job() {
    let (control, cancel, task, _) = actor();
    let (owner, _, release) =
        held(&control, request("p/a", Some("%1"), ReservationKind::Clear)).await;
    let (entered, entry) = oneshot::channel();
    let (cleaned, clean) = oneshot::channel();
    let caller = queued(
        &control,
        request("p/a", Some("%1"), ReservationKind::Delivery),
        move |_| async move {
            entered.send(()).unwrap();
            Ok(())
        },
        move |_, _| async move {
            cleaned.send(()).unwrap();
            Ok(())
        },
    )
    .await;
    barrier(&control).await;
    caller.abort();
    caller.await.unwrap_err();
    release.send(()).unwrap();
    owner.await.unwrap().unwrap();
    entry.await.unwrap();
    clean.await.unwrap();
    cancel.cancel();
    task.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn reservation_stale_epochs_rejected_after_release_and_during_cleanup() {
    let (control, cancel, task, mock) = actor();
    let (entered, entry) = oneshot::channel();
    let (finished, finish) = oneshot::channel();
    let (cleaning, clean) = oneshot::channel();
    let (released, release) = oneshot::channel();
    let job = queued(
        &control,
        request("p/a", Some("%1"), ReservationKind::Clear),
        move |r| async move {
            entered.send(r).unwrap();
            finish.await.unwrap();
            Ok(())
        },
        move |r, _| async move {
            cleaning.send(r).unwrap();
            release.await.unwrap();
            Ok(())
        },
    )
    .await;
    let stale = entry.await.unwrap();
    finished.send(()).unwrap();
    let cleanup = clean.await.unwrap();
    let pane = PaneId::parse("%1").unwrap();
    assert!(matches!(
        stale
            .paste(&pane, stale.agent().clone(), "late".into())
            .await,
        Err(ReservationError::StaleReservation)
    ));
    assert!(cleanup.inspect(&pane).await.unwrap().is_none());
    released.send(()).unwrap();
    job.await.unwrap().unwrap();
    let (new_job, _, release) = held(
        &control,
        request("p/a", Some("%1"), ReservationKind::Observe),
    )
    .await;
    for old in [stale, cleanup] {
        assert!(matches!(
            old.kill(
                &pane,
                old.agent().clone(),
                serde_json::Value::Null,
                Utc::now()
            )
            .await,
            Err(ReservationError::StaleReservation)
        ));
    }
    assert_eq!(
        mock.call_count(),
        1,
        "only the valid cleanup inspection reads tmux"
    );
    release.send(()).unwrap();
    new_job.await.unwrap().unwrap();
    cancel.cancel();
    task.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn reservation_deadline_stops_work_before_cleanup_and_keeps_ownership() {
    let (control, cancel, task, _) = actor();
    let (entered, entry) = oneshot::channel();
    let (dropped, drop_seen) = oneshot::channel();
    let (cleaning, clean) = oneshot::channel();
    let (finish, finished) = oneshot::channel();
    struct OnDrop(Option<oneshot::Sender<()>>);
    impl Drop for OnDrop {
        fn drop(&mut self) {
            if let Some(tx) = self.0.take() {
                let _ = tx.send(());
            }
        }
    }
    let mut req = request("p/a", Some("%1"), ReservationKind::Clear);
    req.deadline = Instant::now() + Duration::from_secs(5);
    let job = queued(
        &control,
        req,
        move |r| async move {
            let _drop = OnDrop(Some(dropped));
            entered.send(r).unwrap();
            std::future::pending::<Result<(), ReservationError>>().await
        },
        move |r, completion| async move {
            assert_eq!(completion, Completion::TimedOut);
            cleaning.send(r).unwrap();
            finished.await.unwrap();
            Ok(())
        },
    )
    .await;
    let stale = entry.await.unwrap();
    tokio::time::advance(Duration::from_secs(5)).await;
    let _cleanup = clean.await.unwrap();
    drop_seen.await.unwrap();
    assert!(matches!(
        stale.inspect(&PaneId::parse("%1").unwrap()).await,
        Err(ReservationError::StaleReservation)
    ));
    let mut refused = request("p/b", Some("%1"), ReservationKind::Delivery);
    refused.wait = WaitPolicy::Refuse;
    assert!(matches!(
        control
            .run_reserved(refused, |_| async { Ok::<_, ReservationError>(()) }, noop)
            .await,
        Err(ReservationError::Busy { .. })
    ));
    finish.send(()).unwrap();
    assert!(matches!(job.await.unwrap(), Err(ReservationError::Timeout)));
    control
        .run_reserved(
            request("p/a", Some("%1"), ReservationKind::Observe),
            |_| async { Ok::<_, ReservationError>(()) },
            noop,
        )
        .await
        .unwrap();
    cancel.cancel();
    task.await.unwrap();
}

#[tokio::test]
async fn quarantines_are_read_without_reserving_and_while_work_is_stopped() {
    let (control, cancel, task, _) = actor();
    let deadline = || Instant::now() + Duration::from_secs(30);
    let ok = |_| async { Ok::<_, ReservationError>(()) };
    assert_eq!(control.quarantines(deadline()).await.unwrap(), Vec::new());
    // One operation whose cleanup fails, one whose cleanup succeeds.
    let failed = control
        .run_reserved(
            request("p/a", Some("%1"), ReservationKind::Down),
            ok,
            |_, _| async {
                Err(ReservationError::InvalidRequest {
                    reason: "cleanup failed".into(),
                })
            },
        )
        .await;
    assert!(matches!(
        failed,
        Err(ReservationError::InvalidRequest { .. })
    ));
    control
        .run_reserved(
            request("p/b", Some("%2"), ReservationKind::Observe),
            ok,
            noop,
        )
        .await
        .unwrap();
    let expected = vec![Quarantine {
        agent: "p/a".parse().unwrap(),
        pane: Some(PaneId::parse("%1").unwrap()),
        kind: ReservationKind::Down,
    }];
    assert_eq!(control.quarantines(deadline()).await.unwrap(), expected);
    // The read reserved nothing: the other agent is admitted again, and the
    // quarantined one is refused as before.
    control
        .run_reserved(
            request("p/b", Some("%2"), ReservationKind::Observe),
            ok,
            noop,
        )
        .await
        .unwrap();
    assert!(matches!(
        control
            .run_reserved(
                request("p/a", Some("%1"), ReservationKind::Observe),
                ok,
                noop
            )
            .await,
        Err(ReservationError::Quarantined { .. })
    ));
    // Daemon shutdown refuses new reservations; the read is still answered.
    control.stop_reserved_work();
    assert!(matches!(
        control
            .run_reserved(
                request("p/b", Some("%2"), ReservationKind::Observe),
                ok,
                noop
            )
            .await,
        Err(ReservationError::Unavailable)
    ));
    assert_eq!(control.quarantines(deadline()).await.unwrap(), expected);
    cancel.cancel();
    task.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn reservation_cleanup_failure_and_timeout_quarantine_waiters() {
    for timeout in [false, true] {
        let (control, cancel, task, _) = actor();
        let (entered, entry) = oneshot::channel();
        let (finish, finished) = oneshot::channel();
        let (cleaning, clean) = oneshot::channel();
        let job = queued(
            &control,
            request("p/a", Some("%1"), ReservationKind::Clear),
            move |_| async move {
                entered.send(()).unwrap();
                finished.await.unwrap();
                Ok(())
            },
            move |_, _| async move {
                cleaning.send(()).unwrap();
                if timeout {
                    std::future::pending::<()>().await;
                }
                Err(ReservationError::InvalidRequest {
                    reason: "cleanup failed".into(),
                })
            },
        )
        .await;
        entry.await.unwrap();
        let delivery = queued(
            &control,
            request("p/b", Some("%1"), ReservationKind::Delivery),
            |_| async { panic!("quarantined waiter must not run") },
            noop,
        )
        .await;
        barrier(&control).await;
        finish.send(()).unwrap();
        clean.await.unwrap();
        if timeout {
            tokio::time::advance(Duration::from_secs(10)).await;
        }
        let error = job.await.unwrap().unwrap_err();
        assert!(if timeout {
            matches!(error, ReservationError::Timeout)
        } else {
            matches!(error, ReservationError::InvalidRequest { .. })
        });
        assert!(matches!(
            delivery.await.unwrap(),
            Err(ReservationError::Quarantined { .. })
        ));
        for (agent, pane) in [("p/a", Some("%2")), ("p/c", Some("%1"))] {
            assert!(matches!(
                control
                    .run_reserved(
                        request(agent, pane, ReservationKind::Observe),
                        |_| async { Ok::<_, ReservationError>(()) },
                        noop
                    )
                    .await,
                Err(ReservationError::Quarantined { .. })
            ));
        }
        // Unrelated panes remain usable despite a failed cleanup.
        control
            .run_reserved(
                request("p/d", Some("%3"), ReservationKind::Observe),
                |_| async { Ok::<_, ReservationError>(()) },
                noop,
            )
            .await
            .unwrap();
        cancel.cancel();
        task.await.unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn reservation_shutdown_stops_admission_and_joins_cleanup() {
    let (control, cancel, task, _) = actor();
    let (entered, entry) = oneshot::channel();
    let (cleaning, clean) = oneshot::channel();
    let (finish, finished) = oneshot::channel();
    let job = queued(
        &control,
        request("p/a", Some("%1"), ReservationKind::Clear),
        move |_| async move {
            entered.send(()).unwrap();
            std::future::pending::<Result<(), ReservationError>>().await
        },
        move |r, completion| async move {
            assert_eq!(completion, Completion::WorkerStopped);
            cleaning.send(r).unwrap();
            finished.await.unwrap();
            Ok(())
        },
    )
    .await;
    entry.await.unwrap();
    let waiter = queued(
        &control,
        request("p/a", Some("%1"), ReservationKind::Delivery),
        |_| async { panic!("shutdown waiter ran") },
        noop,
    )
    .await;
    barrier(&control).await;
    cancel.cancel();
    let cleanup = clean.await.unwrap();
    assert!(!task.is_finished());
    assert!(cleanup
        .inspect(&PaneId::parse("%1").unwrap())
        .await
        .unwrap()
        .is_none());
    assert!(matches!(
        waiter.await.unwrap(),
        Err(ReservationError::Unavailable)
    ));
    assert!(matches!(
        control
            .run_reserved(
                request("p/b", Some("%2"), ReservationKind::Observe),
                |_| async { Ok::<_, ReservationError>(()) },
                noop
            )
            .await,
        Err(ReservationError::Unavailable)
    ));
    finish.send(()).unwrap();
    assert!(matches!(
        job.await.unwrap(),
        Err(ReservationError::Unavailable)
    ));
    task.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn reservation_admission_bounded_and_expired_waiter_never_runs() {
    let (control, cancel, task, _) = actor();
    let (owner, _, release) =
        held(&control, request("p/a", Some("%1"), ReservationKind::Clear)).await;
    let mut others = Vec::new();
    for n in 2..32 {
        others.push(
            held(
                &control,
                request(
                    &format!("p/a{n}"),
                    Some(&format!("%{n}")),
                    ReservationKind::Observe,
                ),
            )
            .await,
        );
    }
    let mut waiters = Vec::new();
    for _ in 0..1 {
        let mut req = request("p/a", Some("%1"), ReservationKind::Delivery);
        req.deadline = Instant::now() + Duration::from_secs(5);
        waiters.push(
            queued(
                &control,
                req,
                |_| async { panic!("expired waiter ran") },
                noop,
            )
            .await,
        );
        barrier(&control).await;
    }
    assert!(matches!(
        control
            .run_reserved(
                request("p/b", Some("%2"), ReservationKind::Observe),
                |_| async { Ok::<_, ReservationError>(()) },
                noop
            )
            .await,
        Err(ReservationError::Unavailable)
    ));
    tokio::time::advance(Duration::from_secs(5)).await;
    for waiter in waiters {
        assert!(matches!(
            waiter.await.unwrap(),
            Err(ReservationError::Timeout)
        ));
    }
    release.send(()).unwrap();
    owner.await.unwrap().unwrap();
    for (job, _, release) in others {
        release.send(()).unwrap();
        job.await.unwrap().unwrap();
    }
    control
        .run_reserved(
            request("p/b", Some("%2"), ReservationKind::Observe),
            |_| async { Ok::<_, ReservationError>(()) },
            noop,
        )
        .await
        .unwrap();
    cancel.cancel();
    task.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn reservation_agent_without_pane_is_exclusive_and_invalid_budgets_do_not_run() {
    let (control, cancel, task, _) = actor();
    let (owner, _, release) = held(&control, request("p/a", None, ReservationKind::Up)).await;
    let mut busy = request("p/a", Some("%3"), ReservationKind::Respawn);
    busy.wait = WaitPolicy::Refuse;
    assert!(matches!(
        control
            .run_reserved(busy, |_| async { Ok::<_, ReservationError>(()) }, noop)
            .await,
        Err(ReservationError::Busy { .. })
    ));
    for (kind, pane, seconds) in [
        (ReservationKind::Up, None, 121),
        (ReservationKind::Up, None, 0),
        (ReservationKind::Delivery, None, 60),
    ] {
        let mut req = request("p/b", pane, kind);
        req.deadline = Instant::now() + Duration::from_secs(seconds);
        let error = control
            .run_reserved::<(), ReservationError, _, _, _, _>(
                req,
                |_| async { panic!("invalid request ran") },
                noop,
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            ReservationError::InvalidRequest { .. } | ReservationError::Timeout
        ));
    }
    release.send(()).unwrap();
    owner.await.unwrap().unwrap();
    cancel.cancel();
    task.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn reservation_router_timeout_only_drops_reply_not_ownership() {
    use crate::server::methods::{CallContext, CallerId, MethodError, MethodRouter};
    use std::sync::{Arc, Mutex};
    let (control, cancel, task, _) = actor();
    let (entered, entry) = oneshot::channel();
    let (finish, finished) = oneshot::channel();
    let signals = Arc::new(Mutex::new(Some((entered, finished))));
    let mut router = MethodRouter::default();
    let handler_control = control.clone();
    router
        .register("test.reserved", Duration::from_secs(5), move |_, _| {
            let control = handler_control.clone();
            let (entered, finished) = signals.lock().unwrap().take().unwrap();
            async move {
                control
                    .run_reserved(
                        request("p/a", Some("%1"), ReservationKind::Clear),
                        move |_| async move {
                            entered.send(()).unwrap();
                            finished.await.unwrap();
                            Ok::<_, ReservationError>(())
                        },
                        noop,
                    )
                    .await
                    .map_err(|e| MethodError::new("unavailable", e.to_string()))?;
                Ok(serde_json::Value::Null)
            }
        })
        .unwrap();
    let rpc = tokio::spawn(async move {
        router
            .call(
                CallContext {
                    client_id: CallerId("test".into()),
                    from_pane: None,
                },
                "test.reserved",
                serde_json::Value::Null,
            )
            .await
    });
    entry.await.unwrap();
    tokio::time::advance(Duration::from_secs(5)).await;
    assert_eq!(rpc.await.unwrap().unwrap_err().0.code, "timeout");
    let mut refused = request("p/b", Some("%1"), ReservationKind::Delivery);
    refused.wait = WaitPolicy::Refuse;
    assert!(matches!(
        control
            .run_reserved(refused, |_| async { Ok::<_, ReservationError>(()) }, noop)
            .await,
        Err(ReservationError::Busy { .. })
    ));
    finish.send(()).unwrap();
    control
        .run_reserved(
            request("p/a", Some("%1"), ReservationKind::Delivery),
            |_| async { Ok::<_, ReservationError>(()) },
            noop,
        )
        .await
        .unwrap();
    cancel.cancel();
    task.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn reservation_shutdown_cleanup_timeout_joins_and_stale_actor_token_cannot_act() {
    let (control, cancel, task, _) = actor();
    let (entered, entry) = oneshot::channel();
    let (cleaning, clean) = oneshot::channel();
    let job = queued(
        &control,
        request("p/a", None, ReservationKind::Up),
        move |r| async move {
            entered.send(r).unwrap();
            std::future::pending::<Result<(), ReservationError>>().await
        },
        move |_, _| async move {
            cleaning.send(()).unwrap();
            std::future::pending::<Result<(), ReservationError>>().await
        },
    )
    .await;
    let stale = entry.await.unwrap();
    cancel.cancel();
    clean.await.unwrap();
    tokio::time::advance(Duration::from_secs(10)).await;
    assert!(matches!(
        job.await.unwrap(),
        Err(ReservationError::Unavailable)
    ));
    task.await.unwrap();
    let (other, cancel, task, mock) = actor();
    let (owner, _, release) = held(&other, request("p/a", None, ReservationKind::Up)).await;
    assert!(matches!(
        stale
            .split(
                SpawnContext::new("%0".into(), None, false),
                PaneDirection::Below,
                "50%".into()
            )
            .await,
        Err(ReservationError::Unavailable)
    ));
    assert_eq!(mock.call_count(), 0);
    release.send(()).unwrap();
    owner.await.unwrap().unwrap();
    cancel.cancel();
    task.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn reservation_work_failure_preserved_and_panics_get_cleanup() {
    for panic_work in [false, true] {
        let (control, cancel, task, _) = actor();
        let (cleaned, clean) = oneshot::channel();
        let result = control
            .run_reserved(
                request("p/a", Some("%1"), ReservationKind::Observe),
                move |_| async move {
                    if panic_work {
                        panic!("test worker stopped");
                    }
                    Err::<(), _>(ReservationError::InvalidRequest {
                        reason: "work failed".into(),
                    })
                },
                move |_, completion| async move {
                    cleaned.send(completion).unwrap();
                    Err(ReservationError::Unavailable)
                },
            )
            .await;
        if panic_work {
            assert!(matches!(result, Err(ReservationError::Unavailable)));
            assert_eq!(clean.await.unwrap(), Completion::WorkerStopped);
        } else {
            assert!(
                matches!(result, Err(ReservationError::InvalidRequest { reason }) if reason == "work failed")
            );
            assert_eq!(clean.await.unwrap(), Completion::Failed);
        }
        cancel.cancel();
        task.await.unwrap();
    }
}

// The daemon's stop: work ends at once, cleanup runs, and the actor stays
// for the cleanups, legacy input and audits of the stages that stop later.
#[tokio::test(start_paused = true)]
async fn stopping_reserved_work_aborts_work_and_runs_cleanup_as_worker_stopped() {
    let (control, cancel, task, _) = actor();
    let (entered, entry) = oneshot::channel();
    let (cleaning, clean) = oneshot::channel();
    let (finish, finished) = oneshot::channel();
    let job = queued(
        &control,
        request("p/a", Some("%1"), ReservationKind::Clear),
        move |_| async move {
            entered.send(()).unwrap();
            std::future::pending::<Result<(), ReservationError>>().await
        },
        move |r, completion| async move {
            assert_eq!(completion, Completion::WorkerStopped);
            cleaning.send(r).unwrap();
            finished.await.unwrap();
            Ok(())
        },
    )
    .await;
    entry.await.unwrap();
    let waiter = queued(
        &control,
        request("p/a", Some("%1"), ReservationKind::Delivery),
        |_| async { panic!("a waiting reservation ran after the stop") },
        noop,
    )
    .await;
    barrier(&control).await;
    control.stop_reserved_work();
    // The cleanup's own fenced pane operation is served.
    let cleanup = clean.await.unwrap();
    assert!(cleanup
        .inspect(&PaneId::parse("%1").unwrap())
        .await
        .unwrap()
        .is_none());
    assert!(matches!(
        waiter.await.unwrap(),
        Err(ReservationError::Unavailable)
    ));
    assert!(matches!(
        control
            .run_reserved(
                request("p/b", Some("%2"), ReservationKind::Observe),
                |_| async { Ok::<_, ReservationError>(()) },
                noop
            )
            .await,
        Err(ReservationError::Unavailable)
    ));
    assert!(matches!(
        control
            .observe(&PaneId::parse("%1").unwrap(), CaptureFormat::Plain)
            .await,
        Err(ReservationError::Unavailable)
    ));
    finish.send(()).unwrap();
    assert!(matches!(
        job.await.unwrap(),
        Err(ReservationError::Unavailable)
    ));
    // The service itself was not stopped: legacy requests are still answered.
    barrier(&control).await;
    assert!(!task.is_finished());
    assert!(!cancel.is_cancelled());
    cancel.cancel();
    task.await.unwrap();
}
