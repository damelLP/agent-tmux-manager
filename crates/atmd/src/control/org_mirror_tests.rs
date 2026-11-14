use super::*;
use crate::control::reservation::*;
use atm_tmux::{mock::MockCall, MockTmuxClient};
use std::{future::Future, task::Poll, time::Duration};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn reservation_mirror_stall_parks_input_not_the_actor() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let dir = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        let org = OrgHandle::open(dir.path().into(), cancel.clone()).await;
        let (updates, projection) = watch::channel(Projection::Loading);
        let (release, released) = tokio::sync::oneshot::channel();
        let source = org.clone();
        let worker = tokio::spawn(async move {
            released.await.unwrap();
            let _ = refresh(source, &updates).await;
        });
        let mirror = Mirror { org, projection, worker, open: true };
        let mock = MockTmuxClient::new();
        let (control, actor) = super::super::spawn_control_with_audit(
            Box::new(mock.clone()), crate::registry::spawn_registry(), mirror, cancel.clone(), |_, _| async { Ok(Vec::new()) });
        let c = control.clone();
        let mut send = Box::pin(async move { c.send("%1".into(), "after mirror".into()).await });
        std::future::poll_fn(|cx| { assert!(send.as_mut().poll(cx).is_pending()); Poll::Ready(()) }).await;
        let send = tokio::spawn(send);
        control.prepare_spawn(Some("%999".into()), Some("/private".into()), None, None).await.unwrap();
        mock.set_next_raw_output("ATM-PANE-v1\t%2\tprivate\t0\t123\t80\t24\t1\t0\t0\t1\t0\t0\n");
        mock.set_next_raw_output("p/b\n");
        control.run_reserved(ReservationRequest::new("p/b".parse().unwrap(), Some(PaneId::parse("%2").unwrap()), ReservationKind::Delivery, WaitPolicy::Wait, tokio::time::Instant::now() + Duration::from_secs(60)), |r| async move {
            r.paste(&PaneId::parse("%2")?, r.agent().clone(), "independent".into()).await
        }, |_, _| async { Ok::<_, ReservationError>(()) }).await.unwrap();
        assert!(!send.is_finished());
        assert!(mock.calls().iter().any(|c| matches!(c, MockCall::PasteText { pane, text } if pane == "%2" && text == "independent")));
        mock.set_next_raw_output("0 1");
        release.send(()).unwrap();
        assert_eq!(send.await.unwrap().unwrap(), "%1");
        cancel.cancel(); actor.await.unwrap();
    }).await.unwrap();
}

#[tokio::test]
async fn reservation_last_caller_drop_still_drains_admitted_input_after_projection() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let dir = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        let org = OrgHandle::open(dir.path().into(), cancel.clone()).await;
        let (updates, projection) = watch::channel(Projection::Loading);
        let (release, released) = tokio::sync::oneshot::channel();
        let worker = tokio::spawn(async move {
            released.await.unwrap();
            updates.send_replace(Projection::Ready {
                through: 0,
                state: Arc::new(OrgState::default()),
            });
            std::future::pending::<()>().await;
        });
        let mirror = Mirror {
            org,
            projection,
            worker,
            open: true,
        };
        let mock = MockTmuxClient::new();
        let (control, actor) = super::super::spawn_control_with_audit(
            Box::new(mock.clone()),
            crate::registry::spawn_registry(),
            mirror,
            CancellationToken::new(),
            |_, _| async { Ok(Vec::new()) },
        );
        let mut input = Box::pin(control.type_spawn_command("%1".into(), "once".into()));
        std::future::poll_fn(|cx| {
            assert!(input.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        control
            .prepare_spawn(Some("%999".into()), Some("/private".into()), None, None)
            .await
            .unwrap();
        drop(input);
        drop(control);
        // Publication follows caller EOF; neither is permission to discard the
        // already-admitted action. No external I/O or elapsed-time assumption.
        let _ = release.send(());
        actor.await.unwrap();
        assert_eq!(
            mock.calls()
                .iter()
                .filter(|call| matches!(call, MockCall::SendKeys { .. }))
                .count(),
            2
        );
        cancel.cancel();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn reservation_mirror_never_authorizes_a_snapshot_with_an_old_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let org = OrgHandle::open(dir.path().into(), cancel.clone()).await;
    let (updates, projection) = watch::channel(Projection::Ready {
        through: 0,
        state: Arc::new(OrgState::default()),
    });
    let worker = tokio::spawn(std::future::pending());
    let mirror = Mirror {
        org: org.clone(),
        projection,
        worker,
        open: true,
    };
    assert!(matches!(mirror.view(), View::Ready(_)));
    org.execute(
        atm_org::OrgCommand::RegisterProject {
            project_id: "p".parse().unwrap(),
            root: "/private".into(),
            move_root: false,
        },
        chrono::Utc::now(),
    )
    .await
    .unwrap();
    assert!(matches!(mirror.view(), View::Loading));
    updates.send_replace(Projection::Ready {
        through: 1,
        state: Arc::new(org.snapshot().await.unwrap()),
    });
    assert!(matches!(mirror.view(), View::Ready(_)));
    updates.send_replace(Projection::Unavailable);
    assert!(matches!(mirror.view(), View::Unavailable));
    mirror.stop().await;
    cancel.cancel();
}
