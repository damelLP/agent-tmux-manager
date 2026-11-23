//! Replies share input admission; legacy-format captures share the existing
//! scheduler. All races are held on channels, not on a delay.
use super::pane_request_tests::*;
use super::*;
use crate::control::legacy_survey_test::mock_shell;
use crate::control::{
    reservation::{testing::ScriptedTmuxClient, *},
    ReplyOutcome,
};
use atm_core::{SessionDomain, SessionId};
use atm_tmux::{mock::MockCall, MockTmuxClient, PaneInfo, TmuxError};

#[path = "dialog_request_test_client.rs"]
mod paused;
use paused::{At, Paused};

#[path = "dialog_admission_tests.rs"]
mod admission;

const CLEARING: &str = "Refusing to reply to %1: agent p/a is clearing; try again in a moment";
fn inspection() -> PaneInspection {
    PaneInspection::new(
        PaneInfo {
            pane_id: "%1".into(),
            session_name: "test".into(),
            window_index: 0,
            pane_pid: 1,
            width: 80,
            height: 1,
            is_active: false,
        },
        None,
        false,
        false,
        Some(true),
        0,
        0,
    )
}
fn script_admission(client: &ScriptedTmuxClient, count: usize) {
    for _ in 0..count {
        client.script_inspect(&pane(), Some(&inspection())).unwrap();
    }
}
fn key(pane: &str, keys: &str) -> MockCall {
    MockCall::SendKeys {
        pane: pane.into(),
        keys: keys.into(),
    }
}
async fn barrier(control: &ControlHandle) {
    control.interrupt("%9".into()).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn legacy_reply_veto_uses_the_fresh_viewport_before_the_first_key() {
    let frame = crate::control::legacy_survey_test::fixture();
    // Synthetic: a staged full-width digit; no key was sent to make it.
    let draft = frame.replacen("❯\u{a0} ", "❯\u{a0}１", 1);
    assert_ne!(draft, frame);
    let client = ScriptedTmuxClient::new();
    let mock = client.mock();
    let (control, actor, cancel) = start(Box::new(client.clone()));
    for (view, option, yes, no, refused) in [
        (draft.as_str(), None, false, false, true),
        (&draft, None, true, false, true),
        (&draft, Some(1), false, false, true),
        (&frame, Some(2), false, false, true),
        (&draft, None, false, true, false),
        (&frame, None, true, false, false),
        (&frame, Some(1), false, false, false),
        ("prompt\n", Some(2), false, false, false),
    ] {
        mock.clear_calls();
        script_admission(&client, if option.is_some() { 2 } else { 1 });
        if !no {
            script_viewport(&client, "%1", view);
        }
        let result = control.reply("%1".into(), option, yes, no).await;
        let keys: Vec<_> = effects(&mock)
            .into_iter()
            .filter(|call| matches!(call, MockCall::SendKeys { .. }))
            .collect();
        if refused {
            let err = crate::server::methods::control_error(result.unwrap_err()).0;
            assert_eq!(err.code, "refused");
            assert!(err.message.contains("feedback survey"));
            assert!(keys.is_empty(), "{keys:?}");
        } else {
            result.unwrap();
            let expected = match (no, option) {
                (true, _) => vec![key("%1", "Escape")],
                (_, Some(2)) => vec![key("%1", "Down"), key("%1", "Enter")],
                _ => vec![key("%1", "Enter")],
            };
            assert_eq!(keys, expected);
        }
        assert_eq!(client.pending_reads(&pane()).unwrap(), 0);
    }
    cancel.cancel();
    actor.await.unwrap();
}

fn script_viewport(client: &ScriptedTmuxClient, pane: &str, view: &str) {
    let mut info = inspection();
    info.pane.pane_id = pane.into();
    info.pane.width = 217;
    info.pane.height = view.lines().count().try_into().unwrap();
    client
        .script_capture(
            &PaneCapture::new(view.into(), info, CaptureFormat::Ansi).with_focus(Some(false)),
        )
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn held_reply_yields_to_reads_and_other_panes() {
    let client = ScriptedTmuxClient::new();
    let mock = client.mock();
    script_admission(&client, 2);
    script_viewport(&client, "%1", "held screen\n");
    mock.set_pane_content("%1", vec!["held screen".into()]);
    let (control, actor, cancel) = start(Box::new(client));
    let (job, release) = hold(&control, ReservationKind::Delivery, |_| async { Ok(()) }).await;
    let c = control.clone();
    let reply = queued(async move { c.reply("%1".into(), Some(1), false, false).await }).await;
    let before = Instant::now();
    assert_eq!(
        control.capture("%1".into(), None, false).await.unwrap(),
        vec!["held screen"]
    );
    barrier(&control).await;
    assert!(!reply.is_finished());
    assert_eq!(Instant::now(), before);
    assert_eq!(
        effects(&mock),
        vec![
            MockCall::CapturePane { pane: "%1".into() },
            key("%9", "C-c")
        ]
    );
    release.send(()).unwrap();
    job.await.unwrap().unwrap();
    assert_eq!(
        reply.await.unwrap().unwrap(),
        ReplyOutcome::Selected {
            pane: "%1".into(),
            option: 1
        }
    );
    assert_eq!(effects(&mock).last(), Some(&key("%1", "Enter")));
    cancel.cancel();
    actor.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn clearing_and_quarantine_refuse_input_but_never_reading() {
    let mock = MockTmuxClient::new();
    mock.set_pane_content("%1", vec!["still readable".into()]);
    let (control, actor, cancel) = start(Box::new(mock.clone()));
    let (job, release) = hold(&control, ReservationKind::Clear, |_| async { Ok(()) }).await;
    assert_eq!(
        control
            .reply("%1".into(), None, true, false)
            .await
            .unwrap_err()
            .to_string(),
        CLEARING
    );
    mock.set_next_raw_output("%1\n");
    assert_eq!(
        control
            .reply("%01".into(), None, false, true)
            .await
            .unwrap_err()
            .to_string(),
        CLEARING
    );
    assert_eq!(
        control.capture("%1".into(), None, false).await.unwrap(),
        vec!["still readable"]
    );
    release.send(()).unwrap();
    job.await.unwrap().unwrap();
    let error: Result<(), ReservationError> = control
        .run_reserved(
            request(ReservationKind::Delivery),
            |_| async { Ok(()) },
            |_, _| async { Err(ReservationError::Unavailable) },
        )
        .await;
    assert!(error.is_err());
    assert_eq!(
        control
            .quarantines(Instant::now() + Duration::from_secs(60))
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(control
        .reply("%1".into(), None, false, false)
        .await
        .unwrap_err()
        .to_string()
        .contains("is quarantined; restart atmd and reconcile"));
    assert_eq!(
        control.capture("%1".into(), None, false).await.unwrap(),
        vec!["still readable"]
    );
    assert!(!effects(&mock)
        .iter()
        .any(|c| matches!(c, MockCall::SendKeys { .. })));
    cancel.cancel();
    actor.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn captures_and_numbered_replies_share_observation_pacing_and_take_fresh_text() {
    let client = ScriptedTmuxClient::new();
    let mock = client.mock();
    let frame = PaneCapture::new("old\n".into(), inspection(), CaptureFormat::Plain)
        .with_focus(Some(false));
    client.script_observe(&frame).unwrap();
    script_admission(&client, 2);
    script_viewport(&client, "%1", "fresh café\n");
    let (control, actor, cancel) = start(Box::new(client));
    control
        .observe(&pane(), CaptureFormat::Plain)
        .await
        .unwrap();
    let before = Instant::now();
    let c = control.clone();
    let capture = queued(async move { c.capture("%1".into(), None, false).await }).await;
    barrier(&control).await;
    assert!(!capture.is_finished());
    assert_eq!(Instant::now(), before);
    assert!(!effects(&mock)
        .iter()
        .any(|c| matches!(c, MockCall::CapturePane { .. })));
    mock.set_pane_content("%1", vec!["fresh café  ".into(), "".into()]);
    tokio::time::advance(Duration::from_millis(249)).await;
    barrier(&control).await;
    assert!(!capture.is_finished());
    tokio::time::advance(Duration::from_millis(1)).await;
    assert_eq!(capture.await.unwrap().unwrap(), vec!["fresh café  ", ""]);
    let c = control.clone();
    let reply = queued(async move { c.reply("%1".into(), Some(2), false, false).await }).await;
    barrier(&control).await;
    assert!(!reply.is_finished());
    assert_eq!(
        effects(&mock)
            .iter()
            .filter(|c| matches!(c, MockCall::CapturePane { .. }))
            .count(),
        1
    );
    tokio::time::advance(Duration::from_millis(250)).await;
    assert_eq!(
        reply.await.unwrap().unwrap(),
        ReplyOutcome::Selected {
            pane: "%1".into(),
            option: 2
        }
    );
    let actions = effects(&mock);
    assert_eq!(
        actions
            .iter()
            .filter(|c| matches!(c, MockCall::CapturePane { .. }))
            .count(),
        2
    );
    assert!(actions.ends_with(&[key("%1", "Down"), key("%1", "Enter")]));
    cancel.cancel();
    actor.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_paced_reply_rechecks_a_new_clear_before_capturing_or_typing() {
    let client = ScriptedTmuxClient::new();
    let mock = client.mock();
    script_admission(&client, 1);
    let (control, actor, cancel) = start(Box::new(client));
    control.capture("%1".into(), None, false).await.unwrap();
    mock.clear_calls();
    let c = control.clone();
    let reply = queued(async move { c.reply("%1".into(), Some(2), false, false).await }).await;
    barrier(&control).await;
    let (job, release) = hold(&control, ReservationKind::Clear, |_| async { Ok(()) }).await;
    tokio::time::advance(Duration::from_millis(250)).await;
    assert_eq!(reply.await.unwrap().unwrap_err().to_string(), CLEARING);
    assert_eq!(effects(&mock), vec![key("%9", "C-c")]);
    release.send(()).unwrap();
    job.await.unwrap().unwrap();
    cancel.cancel();
    actor.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_paced_reply_that_becomes_held_parks_until_delivery_releases() {
    let client = ScriptedTmuxClient::new();
    let mock = client.mock();
    // Initial admission, release from the hold, then the fresh pacing grant.
    script_admission(&client, 3);
    script_viewport(&client, "%1", "dialog\n");
    let (control, actor, cancel) = start(Box::new(client));
    control.capture("%1".into(), None, false).await.unwrap();
    mock.clear_calls();
    let c = control.clone();
    let reply = queued(async move { c.reply("%1".into(), Some(2), false, false).await }).await;
    barrier(&control).await;
    let (job, release) = hold(&control, ReservationKind::Delivery, |_| async { Ok(()) }).await;
    tokio::time::advance(Duration::from_millis(250)).await;
    barrier(&control).await;
    assert!(
        !reply.is_finished(),
        "a pacing grant must park behind the new hold"
    );
    assert_eq!(effects(&mock), vec![key("%9", "C-c"), key("%9", "C-c")]);
    release.send(()).unwrap();
    job.await.unwrap().unwrap();
    assert_eq!(
        reply.await.unwrap().unwrap(),
        ReplyOutcome::Selected {
            pane: "%1".into(),
            option: 2
        }
    );
    assert!(effects(&mock).ends_with(&[
        MockCall::CapturePane { pane: "%1".into() },
        key("%1", "Down"),
        key("%1", "Enter")
    ]));
    cancel.cancel();
    actor.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn paced_capture_reresolves_the_original_target_and_paces_its_new_pane() {
    let mock = MockTmuxClient::new();
    mock.set_pane_content("%1", vec!["obsolete".into()]);
    mock.set_pane_content("%2", vec!["new".into()]);
    let registry = crate::registry::spawn_registry();
    registry
        .register(SessionDomain {
            id: SessionId::new("moving"),
            tmux_pane: Some("%1".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    let cancel = CancellationToken::new();
    let (control, actor) = spawn_control(
        Box::new(mock.clone()),
        registry.clone(),
        OrgHandle::disabled("test"),
        cancel.clone(),
    );
    control.capture("%1".into(), None, false).await.unwrap();
    tokio::time::advance(Duration::from_millis(125)).await;
    control.capture("%2".into(), None, false).await.unwrap();
    let c = control.clone();
    let capture = queued(async move { c.capture("moving".into(), None, false).await }).await;
    barrier(&control).await;
    registry.remove(SessionId::new("moving")).await.unwrap();
    registry
        .register(SessionDomain {
            id: SessionId::new("moving"),
            tmux_pane: Some("%2".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    let before = Instant::now();
    tokio::time::advance(Duration::from_millis(125)).await;
    barrier(&control).await;
    assert_eq!(
        effects(&mock)
            .iter()
            .filter(|call| matches!(call,
                MockCall::CapturePane { pane } if pane == "%2"
            ))
            .count(),
        1,
        "the new pane must not be captured inside its occupied slot"
    );
    assert!(
        !capture.is_finished(),
        "the new pane's slot is still occupied"
    );
    tokio::time::advance(Duration::from_millis(124)).await;
    barrier(&control).await;
    assert!(!capture.is_finished());
    tokio::time::advance(Duration::from_millis(1)).await;
    assert_eq!(capture.await.unwrap().unwrap(), vec!["new"]);
    assert_eq!(
        Instant::now() - before,
        Duration::from_millis(250),
        "a fresh-pane grant must not get stuck on an unnotified ready list"
    );
    assert_eq!(
        effects(&mock)
            .iter()
            .filter_map(|c| if let MockCall::CapturePane { pane } = c {
                Some(pane.as_str())
            } else {
                None
            })
            .collect::<Vec<_>>(),
        vec!["%1", "%2", "%2"]
    );
    cancel.cancel();
    actor.await.unwrap();
}

#[tokio::test]
async fn caller_gone_during_server_or_capture_reads_types_and_audits_nothing() {
    for at in [At::Server, At::Capture] {
        let (dir, org, org_cancel) = logged().await;
        let server = dir.path().join("server");
        std::fs::write(&server, "").unwrap();
        let mock = MockTmuxClient::new();
        mock.set_next_raw_output(server.to_str().unwrap());
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let cancel = CancellationToken::new();
        let (control, actor) = spawn_control(
            Box::new(Paused {
                inner: Box::new(mock.clone()),
                at,
                entered: entered.clone(),
                release: release.clone(),
            }),
            crate::registry::spawn_registry(),
            org.clone(),
            cancel.clone(),
        );
        let c = if matches!(at, At::Server) {
            control
                .clone()
                .for_server(Some(server.to_str().unwrap().into()))
        } else {
            control.clone()
        };
        let reply = tokio::spawn(async move { c.reply("%1".into(), Some(2), false, false).await });
        entered.notified().await;
        reply.abort();
        let _ = reply.await;
        release.add_permits(1);
        control.flush_audits().await;
        assert!(!effects(&mock)
            .iter()
            .any(|c| matches!(c, MockCall::SendKeys { .. })));
        assert!(events(&org).await.is_empty());
        cancel.cancel();
        actor.await.unwrap();
        org_cancel.cancel();
    }
}

#[tokio::test(start_paused = true)]
async fn a_reply_abandoned_or_expired_during_the_survey_read_sends_no_keys() {
    for abandoned in [false, true] {
        let client = ScriptedTmuxClient::new();
        let mock = client.mock();
        script_admission(&client, 1);
        script_viewport(&client, "%1", "no survey\n");
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let (control, actor, cancel) = start(Box::new(Paused {
            inner: Box::new(client),
            at: At::Survey,
            entered: entered.clone(),
            release: release.clone(),
        }));
        let c = control.clone();
        let reply = tokio::spawn(async move { c.reply("%1".into(), None, true, false).await });
        tokio::time::timeout(Duration::from_secs(60), entered.notified())
            .await
            .expect("never entered the veto read");
        if abandoned {
            reply.abort();
            assert!(reply.await.unwrap_err().is_cancelled());
        } else {
            tokio::time::advance(ACTION_TIMEOUT).await;
            assert!(matches!(reply.await.unwrap(), Err(ServiceError::Timeout)));
        }
        release.add_permits(1);
        control.flush_audits().await;
        assert!(effects(&mock).is_empty());
        cancel.cancel();
        actor.await.unwrap();
    }
}

#[tokio::test]
async fn caller_gone_after_first_navigation_still_finishes_and_audits() {
    let (_dir, org, org_cancel) = logged().await;
    let mock = MockTmuxClient::new();
    mock_shell(&mock, "%1");
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let cancel = CancellationToken::new();
    let (control, actor) = spawn_control(
        Box::new(Paused {
            inner: Box::new(mock.clone()),
            at: At::Navigation,
            entered: entered.clone(),
            release: release.clone(),
        }),
        crate::registry::spawn_registry(),
        org.clone(),
        cancel.clone(),
    );
    let c = control.clone();
    let reply = tokio::spawn(async move { c.reply("%1".into(), Some(2), false, false).await });
    entered.notified().await;
    reply.abort();
    let _ = reply.await;
    release.add_permits(1);
    control.flush_audits().await;
    assert_eq!(
        effects(&mock),
        vec![
            MockCall::CapturePane { pane: "%1".into() },
            key("%1", "Down"),
            key("%1", "Enter")
        ]
    );
    let records = events(&org).await;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].1["action"]["method"], "session.reply");
    assert_eq!(records[0].1["action"]["outcome"], "ok");
    cancel.cancel();
    actor.await.unwrap();
    org_cancel.cancel();
}

#[tokio::test(start_paused = true)]
async fn a_huge_option_has_a_total_deadline_and_never_enters_after_it() {
    let client = ScriptedTmuxClient::new();
    let mock = client.mock();
    script_admission(&client, 2);
    script_viewport(&client, "%1", "dialog\n");
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let (control, actor, cancel) = start(Box::new(Paused {
        inner: Box::new(client),
        at: At::Navigation,
        entered: entered.clone(),
        release,
    }));
    let c = control.clone();
    let reply =
        tokio::spawn(async move { c.reply("%1".into(), Some(usize::MAX), false, false).await });
    entered.notified().await;
    tokio::time::advance(ACTION_TIMEOUT).await;
    assert!(matches!(reply.await.unwrap(), Err(ServiceError::Timeout)));
    barrier(&control).await;
    assert_eq!(
        effects(&mock),
        vec![
            MockCall::CapturePane { pane: "%1".into() },
            key("%1", "Down"),
            key("%9", "C-c")
        ]
    );
    cancel.cancel();
    actor.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn ready_navigation_keys_recheck_the_deadline_without_yielding() {
    let client = ScriptedTmuxClient::new();
    let mock = client.mock();
    script_admission(&client, 2);
    script_viewport(&client, "%1", "dialog\n");
    let (control, actor, cancel) = start(Box::new(Paused {
        inner: Box::new(client),
        at: At::AdvanceClock {
            deadline: Instant::now() + ACTION_TIMEOUT,
        },
        entered: Arc::new(tokio::sync::Notify::new()),
        release: Arc::new(tokio::sync::Semaphore::new(0)),
    }));
    assert!(matches!(
        control
            .reply("%1".into(), Some(usize::MAX), false, false)
            .await,
        Err(ServiceError::Timeout)
    ));
    assert_eq!(
        effects(&mock),
        vec![
            MockCall::CapturePane { pane: "%1".into() },
            key("%1", "Down"),
            key("%1", "Down"),
            key("%1", "Down"),
        ],
        "no navigation or Enter may start at the expired deadline"
    );
    cancel.cancel();
    actor.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_ready_legacy_capture_is_not_overtaken_by_another_panes_stalled_frame() {
    let client = ScriptedTmuxClient::new();
    let mock = client.mock();
    let mut other = inspection();
    other.pane.pane_id = "%2".into();
    let other = PaneCapture::new("B\n".into(), other, CaptureFormat::Plain).with_focus(Some(false));
    client.script_observe(&other).unwrap();
    client.script_observe(&other).unwrap();
    client
        .script_observe(
            &PaneCapture::new("A\n".into(), inspection(), CaptureFormat::Plain)
                .with_focus(Some(false)),
        )
        .unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Semaphore::new(1));
    let (control, actor, cancel) = start(Box::new(Paused {
        inner: Box::new(client),
        at: At::OtherFrame,
        entered: entered.clone(),
        release: release.clone(),
    }));
    control.capture("%1".into(), None, false).await.unwrap();
    let other_pane = PaneId::parse("%2").unwrap();
    control
        .observe(&other_pane, CaptureFormat::Plain)
        .await
        .unwrap();
    entered.notified().await;
    mock.clear_calls();
    let c = control.clone();
    let capture = queued(async move { c.capture("%1".into(), None, false).await }).await;
    let c = control.clone();
    let other_read =
        queued(async move { c.observe(&other_pane, CaptureFormat::Plain).await }).await;
    let c = control.clone();
    let next_read = queued(async move { c.observe(&pane(), CaptureFormat::Plain).await }).await;
    barrier(&control).await;
    tokio::time::advance(Duration::from_millis(250)).await;
    entered.notified().await;
    assert_eq!(
        effects(&mock)
            .iter()
            .filter(|c| matches!(c, MockCall::CapturePane { pane } if pane == "%1"))
            .count(),
        1,
        "a later frame must not overtake an already-ready legacy capture"
    );
    release.add_permits(1);
    capture.await.unwrap().unwrap();
    other_read.await.unwrap().unwrap();
    next_read.await.unwrap().unwrap();
    cancel.cancel();
    actor.await.unwrap();
}

#[tokio::test]
async fn failures_preserve_context_and_read_errors_are_not_audited() {
    let (_dir, org, org_cancel) = logged().await;
    let mock = MockTmuxClient::new();
    let cancel = CancellationToken::new();
    let (control, actor) = spawn_control(
        Box::new(Paused {
            inner: Box::new(mock.clone()),
            at: At::FailEnter,
            entered: Arc::new(tokio::sync::Notify::new()),
            release: Arc::new(tokio::sync::Semaphore::new(0)),
        }),
        crate::registry::spawn_registry(),
        org.clone(),
        cancel.clone(),
    );
    mock.set_next_error(TmuxError::NotFound);
    assert_eq!(
        control
            .reply("%1".into(), Some(2), false, false)
            .await
            .unwrap_err()
            .to_string(),
        "Failed to capture pane"
    );
    mock_shell(&mock, "%1");
    assert_eq!(
        control
            .reply("%1".into(), None, true, false)
            .await
            .unwrap_err()
            .to_string(),
        "Failed to send Enter"
    );
    mock.set_next_error(TmuxError::NotFound);
    assert_eq!(
        control
            .capture("%1".into(), None, false)
            .await
            .unwrap_err()
            .to_string(),
        "Failed to capture pane %1"
    );
    control.flush_audits().await;
    let records = events(&org).await;
    assert_eq!(records.len(), 2);
    for (_, body) in records {
        assert_eq!(body["action"]["method"], "session.reply");
        assert_eq!(body["action"]["outcome"], "error");
    }
    cancel.cancel();
    actor.await.unwrap();
    org_cancel.cancel();
}
