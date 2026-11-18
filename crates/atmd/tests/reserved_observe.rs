//! Read-only observation and capture pacing through the real control actor.
#![allow(clippy::unwrap_used)]
use atm_tmux::{mock::MockCall, PaneInfo};
use atmd::control::{reservation::*, service::spawn_control};
use std::{future::Future, task::Poll, time::Duration};
use tokio::{sync::oneshot, time::Instant};
use tokio_util::sync::CancellationToken;

fn pane(id: &str) -> PaneId {
    PaneId::parse(id).unwrap()
}
fn frame(id: &str, focused: Option<bool>) -> PaneCapture {
    PaneCapture::new(
        "\x1b[31mready  \x1b[0m\n\n".into(),
        PaneInspection::new(
            PaneInfo {
                pane_id: id.into(),
                session_name: "private".into(),
                window_index: 0,
                pane_pid: 123,
                width: 80,
                height: 2,
                is_active: true,
            },
            Some("p/a".into()),
            false,
            false,
            Some(true),
            7,
            0,
        )
        .with_foreground_process(None, Some("neutral-program".into())),
        CaptureFormat::Ansi,
    )
    .with_focus(focused)
}
fn request() -> ReservationRequest {
    ReservationRequest::new(
        "p/a".parse().unwrap(),
        Some(pane("%1")),
        ReservationKind::Delivery,
        WaitPolicy::Wait,
        Instant::now() + Duration::from_secs(60),
    )
}
async fn cleanup(_: Reservation, _: Completion) -> Result<(), ReservationError> {
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn capture_uses_one_raw_command_for_a_complete_observation() {
    let client = testing::ScriptedTmuxClient::new();
    let cancel = CancellationToken::new();
    let (control, actor) = spawn_control(
        Box::new(client.clone()),
        atmd::registry::spawn_registry(),
        atmd::org::OrgHandle::disabled("compound capture"),
        cancel.clone(),
    );
    let expected = frame("%1", Some(false));
    client.script_observe(&expected).unwrap();
    let actual = control
        .observe(&pane("%1"), CaptureFormat::Ansi)
        .await
        .unwrap();
    assert_eq!(actual, expected);
    assert_eq!(
        client.mock().call_count(),
        1,
        "one tmux process per successful look"
    );
    assert_eq!(client.pending_reads(&pane("%1")).unwrap(), 0);
    cancel.cancel();
    actor.await.unwrap();
}

// Review focus 5: old observations never authorize fresh guarded input.
#[tokio::test(start_paused = true)]
async fn compound_observation_never_replaces_guarded_input() {
    let client = testing::ScriptedTmuxClient::new();
    let cancel = CancellationToken::new();
    let (control, actor) = spawn_control(
        Box::new(client.clone()),
        atmd::registry::spawn_registry(),
        atmd::org::OrgHandle::disabled("compound fresh guard"),
        cancel.clone(),
    );
    client.script_observe(&frame("%1", Some(false))).unwrap();
    control
        .observe(&pane("%1"), CaptureFormat::Ansi)
        .await
        .unwrap();
    let mut changed = frame("%1", Some(false));
    changed.inspection.local_agent_tag = Some("beta/seat0".into());
    client.script_capture(&changed).unwrap();
    let error = control
        .run_reserved(
            request(),
            |r| async move {
                r.keys_guarded(
                    &pane("%1"),
                    r.agent().clone(),
                    PaneKey::Enter,
                    CaptureFormat::Ansi,
                    |_| true,
                )
                .await
            },
            cleanup,
        )
        .await
        .unwrap_err();
    assert!(matches!(error, ReservationError::TagMismatch { .. }));
    assert_eq!(
        client.mock().call_count(),
        2,
        "both looks must be fresh single-process captures"
    );
    assert!(client
        .mock()
        .calls()
        .iter()
        .all(|call| matches!(call, MockCall::RawCommand { .. })));
    cancel.cancel();
    actor.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn observe_is_read_only_and_does_not_acquire_or_wait_for_a_reservation() {
    let client = testing::ScriptedTmuxClient::new();
    let cancel = CancellationToken::new();
    let (control, actor) = spawn_control(
        Box::new(client.clone()),
        atmd::registry::spawn_registry(),
        atmd::org::OrgHandle::disabled("observe"),
        cancel.clone(),
    );
    let (entered, entered_rx) = oneshot::channel();
    let (release, released) = oneshot::channel();
    let held = control.clone();
    let job = tokio::spawn(async move {
        held.run_reserved(
            request(),
            move |_| async move {
                entered.send(()).unwrap();
                released.await.unwrap();
                Ok(())
            },
            cleanup,
        )
        .await
    });
    entered_rx.await.unwrap();
    for focused in [Some(true), Some(false), None] {
        let expected = frame("%1", focused);
        client.script_observe(&expected).unwrap();
        let observed = control
            .observe(&pane("%1"), CaptureFormat::Ansi)
            .await
            .unwrap();
        assert_eq!(observed, expected);
        assert_eq!(observed.inspection.pane_root_pid(), Some(123));
        assert_eq!(observed.inspection.foreground_pid, None);
        assert_eq!(
            observed.inspection.foreground_command.as_deref(),
            Some("neutral-program")
        );
    }
    assert!(client
        .mock()
        .calls()
        .iter()
        .all(|call| matches!(call, MockCall::RawCommand { .. })));
    release.send(()).unwrap();
    job.await.unwrap().unwrap();
    cancel.cancel();
    actor.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn observe_pacing_defers_reads_not_other_panes_and_never_reuses_a_frame() {
    let client = testing::ScriptedTmuxClient::new();
    let cancel = CancellationToken::new();
    let (control, actor) = spawn_control(
        Box::new(client.clone()),
        atmd::registry::spawn_registry(),
        atmd::org::OrgHandle::disabled("pacing"),
        cancel.clone(),
    );
    client.script_observe(&frame("%1", Some(false))).unwrap();
    control
        .observe(&pane("%1"), CaptureFormat::Ansi)
        .await
        .unwrap();
    let start = Instant::now();
    client.script_observe(&frame("%1", Some(true))).unwrap();
    let target = pane("%1");
    let mut next = Box::pin(control.observe(&target, CaptureFormat::Ansi));
    std::future::poll_fn(|cx| {
        assert!(next.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    client.script_observe(&frame("%2", Some(false))).unwrap();
    control
        .observe(&pane("%2"), CaptureFormat::Ansi)
        .await
        .unwrap();
    assert_eq!(Instant::now(), start);
    assert_eq!(client.pending_reads(&target).unwrap(), 3);
    tokio::time::advance(Duration::from_millis(249)).await;
    let _ = control.prepare_spawn(None, None, None, None).await; // FIFO actor barrier
    assert_eq!(client.pending_reads(&target).unwrap(), 3);
    tokio::time::advance(Duration::from_millis(1)).await;
    assert_eq!(next.await.unwrap().focused, Some(true));
    cancel.cancel();
    actor.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn guarded_input_recaptures_focus_after_observe_and_pacing() {
    let client = testing::ScriptedTmuxClient::new();
    let cancel = CancellationToken::new();
    let (control, actor) = spawn_control(
        Box::new(client.clone()),
        atmd::registry::spawn_registry(),
        atmd::org::OrgHandle::disabled("fresh focus"),
        cancel.clone(),
    );
    client.script_observe(&frame("%1", Some(false))).unwrap();
    assert_eq!(
        control
            .observe(&pane("%1"), CaptureFormat::Ansi)
            .await
            .unwrap()
            .focused,
        Some(false)
    );
    for focused in [Some(true), None, Some(false)] {
        client.script_capture(&frame("%1", focused)).unwrap();
        let result = control
            .run_reserved(
                request(),
                |r| async move {
                    r.keys_guarded(
                        &pane("%1"),
                        r.agent().clone(),
                        PaneKey::Enter,
                        CaptureFormat::Ansi,
                        |fresh| fresh.focused == Some(false),
                    )
                    .await
                },
                cleanup,
            )
            .await;
        assert_eq!(result.is_ok(), focused == Some(false));
    }
    assert_eq!(
        client
            .mock()
            .calls()
            .iter()
            .filter(|call| matches!(call, MockCall::SendKeys { .. }))
            .count(),
        1
    );
    cancel.cancel();
    actor.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn both_guard_predicates_receive_fresh_neutral_process_facts() {
    let client = testing::ScriptedTmuxClient::new();
    let cancel = CancellationToken::new();
    let (control, actor) = spawn_control(
        Box::new(client.clone()),
        atmd::registry::spawn_registry(),
        atmd::org::OrgHandle::disabled("process guards"),
        cancel.clone(),
    );
    for paste in [false, true] {
        let mut expected = frame("%1", Some(false));
        // Synthetic: exercise the optional PID seam; real tmux 3.6/3.8 cannot
        // supply it. A command name must never be promoted to process identity.
        expected.inspection = expected
            .inspection
            .with_foreground_process(Some(321), Some("neutral-child".into()));
        client.script_capture(&expected).unwrap();
        let (seen, received) = oneshot::channel();
        let result = control
            .run_reserved(
                request(),
                move |r| async move {
                    let check = move |fresh: &PaneCapture| {
                        let _ = seen.send(fresh.clone());
                        false
                    };
                    if paste {
                        r.paste_guarded(
                            &pane("%1"),
                            r.agent().clone(),
                            "text".into(),
                            CaptureFormat::Ansi,
                            check,
                        )
                        .await
                    } else {
                        r.keys_guarded(
                            &pane("%1"),
                            r.agent().clone(),
                            PaneKey::Enter,
                            CaptureFormat::Ansi,
                            check,
                        )
                        .await
                    }
                },
                cleanup,
            )
            .await;
        assert!(matches!(
            result,
            Err(ReservationError::ScreenChanged { .. })
        ));
        assert_eq!(received.await.unwrap(), expected);
    }
    assert!(client
        .mock()
        .calls()
        .iter()
        .all(|call| matches!(call, MockCall::RawCommand { .. })));
    cancel.cancel();
    actor.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn observe_queue_is_bounded_and_shutdown_refuses_parked_reads() {
    let client = testing::ScriptedTmuxClient::new();
    let cancel = CancellationToken::new();
    let (control, actor) = spawn_control(
        Box::new(client.clone()),
        atmd::registry::spawn_registry(),
        atmd::org::OrgHandle::disabled("bounded observe"),
        cancel.clone(),
    );
    let target = pane("%1");
    client.script_observe(&frame("%1", Some(false))).unwrap();
    control.observe(&target, CaptureFormat::Ansi).await.unwrap();
    let calls = client.mock().call_count();
    let mut queued = Vec::new();
    for _ in 0..32 {
        let mut read = Box::pin(control.observe(&target, CaptureFormat::Ansi));
        std::future::poll_fn(|cx| {
            assert!(read.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        let _ = control.prepare_spawn(None, None, None, None).await; // enqueue acknowledgement
        queued.push(read);
    }
    let overflow = control
        .observe(&target, CaptureFormat::Ansi)
        .await
        .unwrap_err();
    assert!(overflow.to_string().contains("nothing was sent"));
    assert!(matches!(overflow, ReservationError::PacingRefused { pane } if pane == target));
    // Each barrier may read placement, but none may consume another pane frame.
    assert_eq!(client.mock().calls().iter().skip(calls).filter(|call| matches!(call, MockCall::RawCommand { subcommand, args } if subcommand == "list-panes" && args.iter().any(|arg| arg == "-f"))).count(), 0);
    cancel.cancel();
    for read in queued {
        assert!(matches!(read.await, Err(ReservationError::Unavailable)));
    }
    actor.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn observer_burst_cannot_refuse_another_panes_guarded_paste() {
    let client = testing::ScriptedTmuxClient::new();
    let cancel = CancellationToken::new();
    let (control, actor) = spawn_control(
        Box::new(client.clone()),
        atmd::registry::spawn_registry(),
        atmd::org::OrgHandle::disabled("pacing isolation"),
        cancel.clone(),
    );
    let busy = pane("%9");
    client.script_observe(&frame("%9", Some(false))).unwrap();
    control.observe(&busy, CaptureFormat::Ansi).await.unwrap();
    let mut queued = Vec::new();
    for _ in 0..32 {
        let mut read = Box::pin(control.observe(&busy, CaptureFormat::Ansi));
        std::future::poll_fn(|cx| {
            assert!(read.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        let _ = control.prepare_spawn(None, None, None, None).await;
        queued.push(read);
    }
    for _ in 0..2 {
        client.script_capture(&frame("%1", Some(false))).unwrap();
    }
    let result = control
        .run_reserved(
            request(),
            |r| async move {
                r.capture(&pane("%1"), CaptureFormat::Ansi).await?;
                r.paste_guarded(
                    &pane("%1"),
                    r.agent().clone(),
                    "hello".into(),
                    CaptureFormat::Ansi,
                    |_| true,
                )
                .await
            },
            cleanup,
        )
        .await;
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(
        client
            .mock()
            .calls()
            .iter()
            .filter(|call| matches!(call, MockCall::PasteText { .. }))
            .count(),
        1
    );
    cancel.cancel();
    for read in queued {
        let _ = read.await;
    }
    actor.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn shutdown_still_allows_paced_cleanup_captures() {
    let client = testing::ScriptedTmuxClient::new();
    let cancel = CancellationToken::new();
    for _ in 0..2 {
        client.script_capture(&frame("%1", Some(false))).unwrap();
    }
    let (control, actor) = spawn_control(
        Box::new(client.clone()),
        atmd::registry::spawn_registry(),
        atmd::org::OrgHandle::disabled("cleanup pacing"),
        cancel.clone(),
    );
    let (entered, entering) = oneshot::channel();
    let (cleaned, cleaning) = oneshot::channel();
    let job = tokio::spawn(async move {
        control
            .run_reserved(
                request(),
                move |_| async move {
                    entered.send(()).unwrap();
                    std::future::pending::<Result<(), ReservationError>>().await
                },
                move |r, _| async move {
                    r.capture(&pane("%1"), CaptureFormat::Ansi).await?;
                    let result = r.capture(&pane("%1"), CaptureFormat::Ansi).await;
                    cleaned.send(result.is_ok()).unwrap();
                    result.map(|_| ())
                },
            )
            .await
    });
    entering.await.unwrap();
    cancel.cancel();
    assert!(cleaning.await.unwrap());
    assert!(matches!(
        job.await.unwrap(),
        Err(ReservationError::Unavailable)
    ));
    actor.await.unwrap();
    assert_eq!(client.pending_reads(&pane("%1")).unwrap(), 0);
}

#[tokio::test(start_paused = true)]
async fn observe_missing_and_unscripted_are_errors_not_synthetic_unfocused_screens() {
    let client = testing::ScriptedTmuxClient::new();
    let cancel = CancellationToken::new();
    let (control, actor) = spawn_control(
        Box::new(client.clone()),
        atmd::registry::spawn_registry(),
        atmd::org::OrgHandle::disabled("missing observe"),
        cancel.clone(),
    );
    client.script_inspect(&pane("%1"), None).unwrap();
    assert!(matches!(
        control.observe(&pane("%1"), CaptureFormat::Ansi).await,
        Err(ReservationError::PaneGone { .. })
    ));
    assert!(matches!(
        control.observe(&pane("%1"), CaptureFormat::Ansi).await,
        Err(ReservationError::Control(_))
    ));
    cancel.cancel();
    actor.await.unwrap();
}
