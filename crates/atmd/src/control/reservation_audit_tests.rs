//! Kill completion/audit ownership through U7's real bounded audit queue.
use super::*;
use crate::control::reservation::{testing::ScriptedTmuxClient, *};
use atm_tmux::mock::MockCall;
use std::{future::Future, task::Poll};

fn request(agent: &str, pane: &str, kind: ReservationKind) -> ReservationRequest {
    ReservationRequest::new(
        agent.parse().unwrap(),
        Some(PaneId::parse(pane).unwrap()),
        kind,
        WaitPolicy::Wait,
        Instant::now() + Duration::from_secs(60),
    )
}
fn script(client: &ScriptedTmuxClient) {
    client
        .script_tag_check(&PaneInspection::new(
            atm_tmux::PaneInfo {
                pane_id: "%1".into(),
                session_name: "private".into(),
                window_index: 0,
                pane_pid: 1,
                width: 80,
                height: 24,
                is_active: true,
            },
            Some("p/a".into()),
            false,
            false,
            Some(true),
            0,
            0,
        ))
        .unwrap();
}
async fn cleanup(_: Reservation, _: Completion) -> Result<(), ReservationError> {
    Ok(())
}
fn now() -> chrono::DateTime<chrono::Utc> {
    "2001-02-03T04:05:06Z".parse().unwrap()
}

#[tokio::test]
async fn reserved_server_identity_mismatch_rejects_before_callbacks_or_effects() {
    let dir = tempfile::tempdir().unwrap();
    let own = dir.path().join("own");
    let other = dir.path().join("other");
    std::fs::write(&own, "").unwrap();
    std::fs::write(&other, "").unwrap();
    let mock = atm_tmux::MockTmuxClient::new();
    mock.set_next_raw_output(own.to_str().unwrap());
    let cancel = CancellationToken::new();
    let (control, actor) = spawn_control(
        Box::new(mock.clone()),
        crate::registry::spawn_registry(),
        OrgHandle::disabled("identity test"),
        cancel.clone(),
    );
    let control = control.for_server(Some(other.to_str().unwrap().into()));
    let result = control
        .run_reserved::<(), ReservationError, _, _, _, _>(
            request("p/a", "%1", ReservationKind::Down),
            |_| async { panic!("foreign server work admitted") },
            |_, _| async { panic!("foreign server cleanup admitted") },
        )
        .await;
    assert!(matches!(
        result,
        Err(ReservationError::InvalidRequest { .. })
    ));
    assert_eq!(mock.call_count(), 1);
    cancel.cancel();
    actor.await.unwrap();
}

#[tokio::test]
async fn reserved_kill_audits_once_before_return_with_supplied_timestamp() {
    let dir = tempfile::tempdir().unwrap();
    let org_cancel = CancellationToken::new();
    let org = OrgHandle::open(dir.path().into(), org_cancel.clone()).await;
    let client = ScriptedTmuxClient::new();
    script(&client);
    let cancel = CancellationToken::new();
    let (control, actor) = spawn_control(
        Box::new(client.clone()),
        crate::registry::spawn_registry(),
        org.clone(),
        cancel.clone(),
    );
    control
        .run_reserved(
            request("p/a", "%1", ReservationKind::Down),
            |r| async move {
                r.kill(
                    &PaneId::parse("%1")?,
                    r.agent().clone(),
                    serde_json::json!({"reason":"down"}),
                    now(),
                )
                .await
            },
            cleanup,
        )
        .await
        .unwrap();
    let events = org.tail(crate::org::TailParams::default()).await.unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, "control.killed");
    assert_eq!(events[0].body["pane"], "%1");
    assert_eq!(events[0].body["action"]["reason"], "down");
    assert_eq!(events[0].ts, now());
    assert_eq!(
        client
            .mock()
            .calls()
            .iter()
            .filter(|c| matches!(c, MockCall::KillPane { .. }))
            .count(),
        1
    );
    cancel.cancel();
    actor.await.unwrap();
    org_cancel.cancel();
}

#[tokio::test]
async fn reserved_kill_dropped_future_retains_audit_and_ownership_without_stalling_other_panes() {
    tokio::time::timeout(Duration::from_secs(30),async {
        let client=ScriptedTmuxClient::new(); script(&client);
        let gate=Arc::new(tokio::sync::Semaphore::new(0));
        let (entered,mut entering)=mpsc::unbounded_channel();
        let cancel=CancellationToken::new();
        let held=gate.clone();
        let (control,actor)=spawn_control_with_audit(Box::new(client.clone()),crate::registry::spawn_registry(),org_mirror::Mirror::start(OrgHandle::disabled("audit test")),cancel.clone(),move |command,time| {
            let gate=held.clone(); let entered=entered.clone();
            async move { entered.send((command,time)).unwrap(); let _permit=gate.acquire().await.unwrap(); Ok(Vec::new()) }
        });
        let (drop_kill,dropped)=oneshot::channel();
        let (work_done,done)=oneshot::channel();
        let (cleaned,mut cleanup_seen)=oneshot::channel();
        let job_control=control.clone();
        let job=tokio::spawn(async move {
            job_control.run_reserved(request("p/a","%1",ReservationKind::Down),move |r| async move {
                let pane=PaneId::parse("%1")?;
                tokio::select! {
                    biased;
                    _ = dropped => {},
                    result = r.kill(&pane,r.agent().clone(),serde_json::json!({}),now()) => { result?; panic!("kill returned before audit acknowledgement"); }
                }
                work_done.send(r).unwrap(); Ok::<_, ReservationError>(())
            },move |_,_| async move { cleaned.send(()).unwrap(); Ok(()) }).await
        });
        let (command,time)=entering.recv().await.unwrap(); assert!(matches!(command,OrgCommand::ControlKilled(_))); assert_eq!(time,now());
        drop_kill.send(()).unwrap(); let stale = done.await.unwrap();
        // Work has dropped the kill future. Cleanup may run but release must wait.
        (&mut cleanup_seen).await.unwrap();
        assert!(matches!(stale.kill(&PaneId::parse("%1").unwrap(), stale.agent().clone(), serde_json::json!({}), now()).await, Err(ReservationError::StaleReservation)));
        let mut rival=Box::pin(control.run_reserved(request("p/a","%1",ReservationKind::Observe),|_| async { Ok::<_,ReservationError>(()) },cleanup));
        std::future::poll_fn(|cx| { assert!(rival.as_mut().poll(cx).is_pending()); Poll::Ready(()) }).await;
        // An independent reservation proves the tmux actor isn't waiting for persistence.
        control.run_reserved(request("p/b","%2",ReservationKind::Observe),|r| async move { assert!(matches!(r.inspect(&PaneId::parse("%2")?).await,Err(ReservationError::Control(_)))); Ok(()) },cleanup).await.unwrap();
        assert!(!job.is_finished());
        assert!(std::future::poll_fn(|cx| Poll::Ready(rival.as_mut().poll(cx).is_pending())).await);
        gate.add_permits(1); job.await.unwrap().unwrap(); rival.await.unwrap();
        assert_eq!(client.mock().calls().iter().filter(|c|matches!(c,MockCall::KillPane{..})).count(),1);
        cancel.cancel(); actor.await.unwrap();
    }).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn reserved_stalled_kill_audit_quarantines_after_cleanup_budget() {
    let client = ScriptedTmuxClient::new();
    script(&client);
    let (entered, mut entering) = mpsc::unbounded_channel();
    let cancel = CancellationToken::new();
    let (control, actor) = spawn_control_with_audit(
        Box::new(client),
        crate::registry::spawn_registry(),
        org_mirror::Mirror::start(OrgHandle::disabled("audit test")),
        cancel.clone(),
        move |_, _| {
            let entered = entered.clone();
            async move {
                entered.send(()).unwrap();
                std::future::pending().await
            }
        },
    );
    let (drop_kill, dropped) = oneshot::channel();
    let job_control = control.clone();
    let job = tokio::spawn(async move {
        job_control.run_reserved(request("p/a","%1",ReservationKind::Down),move |r| async move {
            let pane=PaneId::parse("%1")?;
            tokio::select! { biased; _=dropped=>Ok(()), result=r.kill(&pane,r.agent().clone(),serde_json::json!({}),now())=>result }
        },cleanup).await
    });
    tokio::time::timeout(Duration::from_secs(30), entering.recv())
        .await
        .unwrap()
        .unwrap();
    drop_kill.send(()).unwrap();
    // Paused in-memory channels auto-advance to the bounded drain deadline.
    assert!(matches!(job.await.unwrap(), Err(ReservationError::Timeout)));
    assert!(matches!(
        control
            .run_reserved(
                request("p/a", "%1", ReservationKind::Down),
                |_| async { Ok::<_, ReservationError>(()) },
                cleanup
            )
            .await,
        Err(ReservationError::Quarantined { .. })
    ));
    cancel.cancel();
    actor.await.unwrap();
}

#[tokio::test]
async fn reserved_failed_audit_does_not_fail_or_repeat_kill_and_failed_effect_is_not_audited() {
    let client = ScriptedTmuxClient::new();
    script(&client);
    let (submitted, mut submissions) = mpsc::unbounded_channel();
    let cancel = CancellationToken::new();
    let (control, actor) = spawn_control_with_audit(
        Box::new(client.clone()),
        crate::registry::spawn_registry(),
        org_mirror::Mirror::start(OrgHandle::disabled("audit test")),
        cancel.clone(),
        move |command, time| {
            let submitted = submitted.clone();
            async move {
                submitted.send((command, time)).unwrap();
                Err(crate::org::OrgServiceError::Unavailable(
                    "injected failure".into(),
                ))
            }
        },
    );
    control
        .run_reserved(
            request("p/a", "%1", ReservationKind::Down),
            |r| async move {
                r.kill(
                    &PaneId::parse("%1")?,
                    r.agent().clone(),
                    serde_json::json!({}),
                    now(),
                )
                .await
            },
            cleanup,
        )
        .await
        .unwrap();
    assert!(matches!(
        submissions.try_recv().unwrap().0,
        OrgCommand::ControlKilled(_)
    ));
    client
        .script_inspect(&PaneId::parse("%1").unwrap(), None)
        .unwrap();
    assert!(matches!(
        control
            .run_reserved(
                request("p/a", "%1", ReservationKind::Down),
                |r| async move {
                    r.kill(
                        &PaneId::parse("%1")?,
                        r.agent().clone(),
                        serde_json::json!({}),
                        now(),
                    )
                    .await
                },
                cleanup
            )
            .await,
        Err(ReservationError::PaneGone { .. })
    ));
    control.flush_audits().await;
    assert!(submissions.try_recv().is_err());
    assert_eq!(
        client
            .mock()
            .calls()
            .iter()
            .filter(|c| matches!(c, MockCall::KillPane { .. }))
            .count(),
        1
    );
    cancel.cancel();
    actor.await.unwrap();
}
