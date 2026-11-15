//! Queue bounds, deadlines and cross-path pacing for the new pane requests.
use super::*;

#[tokio::test(start_paused = true)]
async fn a_guarded_reserved_key_waits_for_a_legacy_capture() {
    let client = ScriptedTmuxClient::new();
    let mock = client.mock();
    let mut owned = inspection();
    owned.local_agent_tag = Some("p/a".into());
    client
        .script_capture(
            &PaneCapture::new("ready\n".into(), owned, CaptureFormat::Plain)
                .with_focus(Some(false)),
        )
        .unwrap();
    let (control, actor, cancel) = start(Box::new(client));
    control.capture("%1".into(), None, false).await.unwrap();
    mock.clear_calls();
    let c = control.clone();
    let (entered, entry) = oneshot::channel();
    let job = queued(async move {
        c.run_reserved(
            request(ReservationKind::Delivery),
            move |r| async move {
                entered.send(()).unwrap();
                r.keys_guarded(
                    &pane(),
                    agent(),
                    PaneKey::Enter,
                    CaptureFormat::Plain,
                    |_| true,
                )
                .await
            },
            noop,
        )
        .await
    })
    .await;
    entry.await.unwrap();
    let before = Instant::now();
    barrier(&control).await;
    assert_eq!(Instant::now(), before);
    assert_eq!(effects(&mock), vec![key("%9", "C-c")]);
    tokio::time::advance(Duration::from_millis(250)).await;
    job.await.unwrap().unwrap();
    assert_eq!(effects(&mock), vec![key("%9", "C-c"), key("%1", "Enter")]);
    cancel.cancel();
    actor.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn one_panes_capture_queue_is_bounded_and_shutdown_drains_live_readers() {
    let mock = MockTmuxClient::new();
    let (control, actor, cancel) = start(Box::new(mock.clone()));
    control.capture("%1".into(), None, false).await.unwrap();
    let mut readers = Vec::new();
    for _ in 0..32 {
        let c = control.clone();
        readers.push(queued(async move { c.capture("%1".into(), None, false).await }).await);
    }
    let before = Instant::now();
    let error = control.capture("%1".into(), None, false).await.unwrap_err();
    assert_eq!(
        error.to_string(),
        "capture pacing refused pane %1; nothing was sent"
    );
    control.capture("%2".into(), None, false).await.unwrap();
    assert_eq!(Instant::now(), before);
    cancel.cancel();
    actor.await.unwrap();
    for reader in readers {
        assert!(matches!(
            reader.await.unwrap(),
            Err(ServiceError::Unavailable)
        ));
    }
    assert_eq!(
        effects(&mock),
        vec![
            MockCall::CapturePane { pane: "%1".into() },
            MockCall::CapturePane { pane: "%2".into() }
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn expired_or_abandoned_paced_reads_never_capture_again() {
    for abandon in [true, false] {
        let mock = MockTmuxClient::new();
        let (control, actor, cancel) = start(Box::new(mock.clone()));
        control.capture("%1".into(), None, false).await.unwrap();
        let c = control.clone();
        let read = queued(async move { c.capture("%1".into(), None, false).await }).await;
        barrier(&control).await;
        if abandon {
            read.abort();
            let _ = read.await;
            tokio::time::advance(Duration::from_millis(250)).await;
        } else {
            tokio::time::advance(ACTION_TIMEOUT).await;
            assert!(matches!(read.await.unwrap(), Err(ServiceError::Timeout)));
        }
        barrier(&control).await;
        assert_eq!(
            effects(&mock)
                .iter()
                .filter(|c| matches!(c, MockCall::CapturePane { .. }))
                .count(),
            1
        );
        cancel.cancel();
        actor.await.unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn a_parked_reply_can_expire_without_keys() {
    let mock = MockTmuxClient::new();
    let (control, actor, cancel) = start(Box::new(mock.clone()));
    let (job, release) = hold(&control, ReservationKind::Delivery, |_| async { Ok(()) }).await;
    let c = control.clone();
    let reply = queued(async move { c.reply("%1".into(), None, false, false).await }).await;
    barrier(&control).await;
    tokio::time::advance(ACTION_TIMEOUT).await;
    assert!(matches!(reply.await.unwrap(), Err(ServiceError::Timeout)));
    barrier(&control).await;
    assert!(!effects(&mock)
        .iter()
        .any(|c| matches!(c, MockCall::SendKeys { pane, .. } if pane == "%1")));
    release.send(()).unwrap();
    job.await.unwrap().unwrap();
    cancel.cancel();
    actor.await.unwrap();
}

#[tokio::test]
async fn requests_expired_in_the_queue_are_not_actions_or_audits() {
    let (_dir, org, org_cancel) = logged().await;
    let mock = MockTmuxClient::new();
    let cancel = CancellationToken::new();
    let (control, actor) = spawn_control(
        Box::new(mock.clone()),
        crate::registry::spawn_registry(),
        org.clone(),
        cancel.clone(),
    );
    let (reply, answered) = oneshot::channel();
    control
        .sender
        .send(Pending {
            server: None,
            deadline: Instant::now(),
            caller: None,
            request: Request::Reply {
                target: "%1".into(),
                option: None,
                yes: true,
                no: false,
                reply,
            },
        })
        .await
        .unwrap();
    let (reply, captured) = oneshot::channel();
    control
        .sender
        .send(Pending {
            server: None,
            deadline: Instant::now(),
            caller: None,
            request: Request::Capture {
                target: "%1".into(),
                tail: None,
                prompt: false,
                reply,
            },
        })
        .await
        .unwrap();
    control.flush_audits().await;
    assert!(matches!(
        answered.await.unwrap(),
        Err(ServiceError::Timeout)
    ));
    assert!(matches!(
        captured.await.unwrap(),
        Err(ServiceError::Timeout)
    ));
    assert_eq!(mock.call_count(), 0);
    assert!(events(&org).await.is_empty());
    cancel.cancel();
    actor.await.unwrap();
    org_cancel.cancel();
}

#[tokio::test]
async fn another_server_is_refused_before_reply_or_read_becomes_an_action() {
    let (dir, org, org_cancel) = logged().await;
    let own = dir.path().join("own");
    let foreign = dir.path().join("foreign");
    std::fs::write(&own, "").unwrap();
    std::fs::write(&foreign, "").unwrap();
    let mock = MockTmuxClient::new();
    let cancel = CancellationToken::new();
    let (control, actor) = spawn_control(
        Box::new(mock.clone()),
        crate::registry::spawn_registry(),
        org.clone(),
        cancel.clone(),
    );
    let caller = control
        .clone()
        .for_server(Some(foreign.to_str().unwrap().into()));
    mock.set_next_raw_output(own.to_str().unwrap());
    assert!(matches!(
        caller.reply("%1".into(), None, true, false).await,
        Err(ServiceError::ServerRefused(_))
    ));
    mock.set_next_raw_output(own.to_str().unwrap());
    assert!(matches!(
        caller.capture("%1".into(), None, false).await,
        Err(ServiceError::ServerRefused(_))
    ));
    control.flush_audits().await;
    assert!(effects(&mock).is_empty());
    assert!(events(&org).await.is_empty());
    cancel.cancel();
    actor.await.unwrap();
    org_cancel.cancel();
}

#[tokio::test]
async fn a_clearing_refusal_is_audited_but_peek_is_not() {
    let (_dir, org, org_cancel) = logged().await;
    let mock = MockTmuxClient::new();
    let cancel = CancellationToken::new();
    let (control, actor) = spawn_control(
        Box::new(mock.clone()),
        crate::registry::spawn_registry(),
        org.clone(),
        cancel.clone(),
    );
    let (job, release) = hold(&control, ReservationKind::Clear, |_| async { Ok(()) }).await;
    assert_eq!(
        control
            .reply("%1".into(), None, false, true)
            .await
            .unwrap_err()
            .to_string(),
        CLEARING
    );
    control.capture("%1".into(), None, false).await.unwrap();
    control.flush_audits().await;
    let records = events(&org).await;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].0, "control.sent");
    assert_eq!(records[0].1["action"]["error"], CLEARING);
    assert_eq!(records[0].1["action"]["outcome"], "error");
    release.send(()).unwrap();
    job.await.unwrap().unwrap();
    cancel.cancel();
    actor.await.unwrap();
    org_cancel.cancel();
}
