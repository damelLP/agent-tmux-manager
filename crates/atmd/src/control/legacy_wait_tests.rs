use super::*;

#[tokio::test]
async fn reservation_wrong_server_is_not_a_control_action_or_audit() {
    let (dir, org, cancel) = healthy().await;
    let own = dir.path().join("own.sock");
    let other = dir.path().join("other.sock");
    std::fs::write(&own, "").unwrap();
    std::fs::write(&other, "").unwrap();
    let mock = MockTmuxClient::new();
    let (control, actor) = spawn_control(
        Box::new(mock.clone()),
        crate::registry::spawn_registry(),
        org.clone(),
        cancel.clone(),
    );
    mock.set_next_raw_output(own.to_str().unwrap());
    assert!(matches!(
        control
            .clone()
            .for_server(Some(other.to_str().unwrap().into()))
            .send("%1".into(), "foreign".into())
            .await,
        Err(ServiceError::ServerRefused(_))
    ));
    control.flush_audits().await;
    assert!(org
        .tail(crate::org::TailParams::default())
        .await
        .unwrap()
        .is_empty());
    assert_eq!(mock.call_count(), 1);
    cancel.cancel();
    actor.await.unwrap();
}

#[tokio::test]
async fn reservation_parked_send_reresolves_registry_instead_of_reusing_old_pane() {
    let (_dir, org, cancel) = healthy().await;
    let mock = MockTmuxClient::new();
    let registry = crate::registry::spawn_registry();
    registry
        .register(SessionDomain {
            id: SessionId::new("moving"),
            tmux_pane: Some("%1".into()),
            ..SessionDomain::default()
        })
        .await
        .unwrap();
    let (control, actor) = spawn_control(
        Box::new(mock.clone()),
        registry.clone(),
        org,
        cancel.clone(),
    );
    let (job, release) = hold(&control, ReservationKind::Delivery).await;
    let c = control.clone();
    let send = queued(async move { c.send("moving".into(), "fresh".into()).await }).await;
    barrier(&control).await;
    assert!(input_calls(&mock).is_empty());
    registry.remove(SessionId::new("moving")).await.unwrap();
    registry
        .register(SessionDomain {
            id: SessionId::new("moving"),
            tmux_pane: Some("%2".into()),
            ..SessionDomain::default()
        })
        .await
        .unwrap();
    mock.set_next_raw_output("0 1");
    release.send(()).unwrap();
    job.await.unwrap().unwrap();
    assert_eq!(send.await.unwrap().unwrap(), "%2");
    assert_eq!(
        input_calls(&mock),
        vec![
            MockCall::PasteText {
                pane: "%2".into(),
                text: "fresh".into()
            },
            MockCall::SendKeys {
                pane: "%2".into(),
                keys: "Enter".into()
            }
        ]
    );
    cancel.cancel();
    actor.await.unwrap();
}

#[tokio::test]
async fn reservation_parked_send_rechecks_new_clear_before_input() {
    let (_dir, org, cancel) = healthy().await;
    let mock = MockTmuxClient::new();
    let (control, actor) = spawn_control(
        Box::new(mock.clone()),
        crate::registry::spawn_registry(),
        org,
        cancel.clone(),
    );
    let (delivery, release) = hold(&control, ReservationKind::Delivery).await;
    let c = control.clone();
    let send = queued(async move { c.send("%1".into(), "unsafe".into()).await }).await;
    let (entered, entry) = oneshot::channel();
    let (editor, confirmed) = oneshot::channel();
    let c = control.clone();
    let clear = queued(async move {
        c.run_reserved(
            request(ReservationKind::Clear),
            move |_| async move {
                entered.send(()).unwrap();
                confirmed.await.unwrap();
                Ok::<_, ReservationError>(())
            },
            noop,
        )
        .await
    })
    .await;
    barrier(&control).await;
    assert!(input_calls(&mock).is_empty());
    release.send(()).unwrap();
    delivery.await.unwrap().unwrap();
    entry.await.unwrap();
    assert!(matches!(
        send.await.unwrap(),
        Err(ServiceError::Control(ControlError::AgentClearing { .. }))
    ));
    assert!(input_calls(&mock).is_empty());
    editor.send(()).unwrap();
    clear.await.unwrap().unwrap();
    cancel.cancel();
    actor.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn reservation_parked_legacy_queue_is_bounded_and_shutdown_never_inputs() {
    let mock = MockTmuxClient::new();
    let cancel = CancellationToken::new();
    let (control, actor) = spawn_control(
        Box::new(mock.clone()),
        crate::registry::spawn_registry(),
        OrgHandle::disabled("test"),
        cancel.clone(),
    );
    let (job, _release) = hold(&control, ReservationKind::Delivery).await;
    let mut queued_calls = Vec::new();
    for _ in 0..32 {
        let c = control.clone();
        queued_calls.push(queued(async move { c.send("%1".into(), "held".into()).await }).await);
        barrier(&control).await;
    }
    assert!(matches!(
        control.send("%1".into(), "overflow".into()).await,
        Err(ServiceError::Unavailable)
    ));
    // Existing work cancellation enters cleanup; no parked input can become an
    // implicit retry or mutate during shutdown, even with live caller handles.
    cancel.cancel();
    for call in queued_calls {
        assert!(matches!(
            call.await.unwrap(),
            Err(ServiceError::Unavailable)
        ));
    }
    assert!(job.await.unwrap().is_err());
    actor.await.unwrap();
    assert!(input_calls(&mock).is_empty());
}

#[tokio::test]
async fn reservation_dropped_raw_caller_does_not_cancel_admitted_input() {
    let (_dir, org, cancel) = healthy().await;
    let mock = MockTmuxClient::new();
    let (control, actor) = spawn_control(
        Box::new(mock.clone()),
        crate::registry::spawn_registry(),
        org,
        cancel.clone(),
    );
    let (job, release) = hold(&control, ReservationKind::Delivery).await;
    let c = control.clone();
    let send = queued(async move { c.send("%1".into(), "once".into()).await }).await;
    barrier(&control).await;
    send.abort();
    let _ = send.await;
    assert!(input_calls(&mock).is_empty());
    mock.set_next_raw_output("0 1");
    release.send(()).unwrap();
    job.await.unwrap().unwrap();
    control.flush_audits().await;
    assert_eq!(
        input_calls(&mock),
        vec![
            MockCall::PasteText {
                pane: "%1".into(),
                text: "once".into()
            },
            MockCall::SendKeys {
                pane: "%1".into(),
                keys: "Enter".into()
            }
        ]
    );
    cancel.cancel();
    actor.await.unwrap();
}
