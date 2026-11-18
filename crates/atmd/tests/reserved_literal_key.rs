//! Literal input uses the real reservation actor; only tmux is faked.
#![allow(clippy::unwrap_used)]
use atm_tmux::{mock::MockCall, MockTmuxClient, PaneInfo, TmuxClient, TmuxError};
use atmd::control::{
    reservation::{testing::ScriptedTmuxClient, *},
    service::{spawn_control, ControlHandle},
    ControlError,
};
use std::time::Duration;
use tokio::{sync::oneshot, task::JoinHandle, time::Instant};
use tokio_util::sync::CancellationToken;

fn pane() -> PaneId {
    PaneId::parse("%0").unwrap()
}
fn frame(format: CaptureFormat) -> PaneCapture {
    PaneCapture::new(
        "fixture ready\n\n".into(),
        PaneInspection::new(
            PaneInfo {
                pane_id: "%0".into(),
                session_name: "private".into(),
                window_index: 0,
                pane_pid: 123,
                width: 40,
                height: 2,
                is_active: true,
            },
            Some("project/lead".into()),
            false,
            false,
            Some(true),
            0,
            0,
        )
        .with_foreground_process(None, Some("fixture".into())),
        format,
    )
    .with_focus(Some(false))
}
fn approved(fresh: &PaneCapture) -> bool {
    fresh.focused == Some(false)
        && fresh.inspection.pane_root_pid() == Some(123)
        && fresh.inspection.foreground_command.as_deref() == Some("fixture")
        && fresh.text == "fixture ready\n\n"
}
fn request(kind: ReservationKind) -> ReservationRequest {
    ReservationRequest::new(
        "project/lead".parse().unwrap(),
        Some(pane()),
        kind,
        WaitPolicy::Wait,
        Instant::now() + Duration::from_secs(60),
    )
}
async fn noop(_: Reservation, _: Completion) -> Result<(), ReservationError> {
    Ok(())
}
fn actor(client: impl TmuxClient + 'static) -> (ControlHandle, CancellationToken, JoinHandle<()>) {
    let cancel = CancellationToken::new();
    let (control, task) = spawn_control(
        Box::new(client),
        atmd::registry::spawn_registry(),
        atmd::org::OrgHandle::disabled("literal test"),
        cancel.clone(),
    );
    (control, cancel, task)
}
// Include raw send-keys: the older pane-effect helper intentionally omits it.
fn effects(mock: &MockTmuxClient) -> Vec<MockCall> {
    mock.calls().into_iter().filter(|call| !matches!(call,
        MockCall::RawCommand { subcommand, .. }
            if matches!(subcommand.as_str(), "list-panes" | "display-message" | "show-options")
    )).collect()
}
fn literal(value: &str) -> MockCall {
    MockCall::RawCommand {
        subcommand: "send-keys".into(),
        args: ["-l", "-t", "%0", "--", value].map(str::to_owned).into(),
    }
}
async fn send(r: &Reservation) -> Result<(), ReservationError> {
    r.literal_key_guarded(
        &pane(),
        r.agent().clone(),
        't',
        CaptureFormat::Plain,
        approved,
    )
    .await
}

// Catches paste/named-key routing, missing -l/--, extra Enter and dropped guards.
#[tokio::test(start_paused = true)]
async fn literal_key_guarded_sends_one_literal_without_enter() {
    let client = ScriptedTmuxClient::new();
    let (control, cancel, task) = actor(client.clone());
    for kind in [
        ReservationKind::Up,
        ReservationKind::Respawn,
        ReservationKind::Clear,
        ReservationKind::Delivery,
    ] {
        let scripts = client.clone();
        control
            .run_reserved(
                request(kind),
                move |r| async move {
                    for format in [CaptureFormat::Plain, CaptureFormat::Ansi] {
                        for bracketed in [Some(true), Some(false), None] {
                            let mut fresh = frame(format);
                            fresh.inspection.bracketed_paste = bracketed;
                            scripts.script_capture(&fresh).unwrap();
                            scripts.mock().clear_calls();
                            let (tx, rx) = oneshot::channel();
                            r.literal_key_guarded(
                                &pane(),
                                r.agent().clone(),
                                't',
                                format,
                                move |capture| {
                                    tx.send(capture.clone()).unwrap();
                                    approved(capture)
                                },
                            )
                            .await?;
                            assert_eq!(rx.await.unwrap(), fresh);
                            assert_eq!(effects(&scripts.mock()), [literal("t")]);
                            assert_eq!(scripts.pending_reads(&pane()).unwrap(), 0);
                        }
                    }
                    Ok(())
                },
                noop,
            )
            .await
            .unwrap();
    }
    cancel.cancel();
    task.await.unwrap();
}

// Catches controls/Unicode accepted, semicolon parsed as a tmux separator, or strings sent.
#[tokio::test(start_paused = true)]
async fn literal_key_guarded_validates_one_printable_ascii_character() {
    let client = ScriptedTmuxClient::new();
    let (control, cancel, task) = actor(client.clone());
    let scripts = client.clone();
    control
        .run_reserved(
            request(ReservationKind::Up),
            move |r| async move {
                for key in ['\0', '\t', '\n', '\r', '\x1b', '\x7f', 'é', '\u{85}'] {
                    scripts.mock().clear_calls();
                    let result = r
                        .literal_key_guarded(
                            &pane(),
                            r.agent().clone(),
                            key,
                            CaptureFormat::Plain,
                            |_| panic!("invalid key reached predicate"),
                        )
                        .await;
                    assert!(
                        matches!(result, Err(ReservationError::InvalidRequest { .. })),
                        "{result:?}"
                    );
                    assert!(scripts.mock().calls().is_empty());
                }
                for (key, encoded) in [
                    ('t', "t"),
                    (' ', " "),
                    ('-', "-"),
                    (';', "\\;"),
                    ('\\', "\\"),
                ] {
                    scripts.mock().clear_calls();
                    scripts
                        .script_capture(&frame(CaptureFormat::Plain))
                        .unwrap();
                    r.literal_key_guarded(
                        &pane(),
                        r.agent().clone(),
                        key,
                        CaptureFormat::Plain,
                        approved,
                    )
                    .await?;
                    assert_eq!(effects(&scripts.mock()), [literal(encoded)]);
                }
                Ok(())
            },
            noop,
        )
        .await
        .unwrap();
    cancel.cancel();
    task.await.unwrap();
}

// Each infrastructure rejection must happen before the predicate, not just before send.
#[tokio::test(start_paused = true)]
async fn literal_key_guarded_refuses_unsafe_pane_before_predicate() {
    let client = ScriptedTmuxClient::new();
    let (control, cancel, task) = actor(client.clone());
    let scripts = client.clone();
    control.run_reserved(request(ReservationKind::Up), move |r| async move {
        for (state, reason) in [(0, "dead"), (1, "mode"), (2, "synchronize-panes"), (3, "input off")] {
            let mut fresh = frame(CaptureFormat::Plain);
            fresh.inspection.dead = state == 0;
            fresh.inspection.in_mode = state == 1;
            fresh.inspection.synchronized = state == 2;
            fresh.inspection.input_off = state == 3;
            scripts.script_capture(&fresh).unwrap();
            scripts.mock().clear_calls();
            let result = r.literal_key_guarded(&pane(), r.agent().clone(), 't', CaptureFormat::Plain,
                |_| panic!("unsafe pane reached predicate")).await;
            assert!(matches!(result, Err(ReservationError::PaneInputRefused { reason: found, .. }) if found.contains(reason)), "{result:?}");
            assert!(effects(&scripts.mock()).is_empty());
            scripts.script_capture(&frame(CaptureFormat::Plain)).unwrap();
            send(&r).await?;
            assert_eq!(effects(&scripts.mock()), [literal("t")]);
        }
        Ok(())
    }, noop).await.unwrap();
    cancel.cancel();
    task.await.unwrap();
}

// A prior safe observation must never substitute for the guard's post-pacing capture.
#[tokio::test(start_paused = true)]
async fn literal_key_guarded_rechecks_consumer_guard() {
    let client = ScriptedTmuxClient::new();
    let (control, cancel, task) = actor(client.clone());
    let scripts = client.clone();
    control
        .run_reserved(
            request(ReservationKind::Up),
            move |r| async move {
                for state in 0..6 {
                    scripts
                        .script_capture(&frame(CaptureFormat::Plain))
                        .unwrap();
                    assert!(approved(&r.capture(&pane(), CaptureFormat::Plain).await?));
                    let mut fresh = frame(CaptureFormat::Plain);
                    match state {
                        0 => fresh.inspection.foreground_command = Some("shell".into()),
                        1 => fresh.inspection.foreground_command = None,
                        2 => fresh.focused = Some(true),
                        3 => fresh.focused = None,
                        4 => fresh.text = "other dialog\n\n".into(),
                        _ => fresh.inspection.pane.pane_pid = 456,
                    }
                    scripts.script_capture(&fresh).unwrap();
                    scripts.mock().clear_calls();
                    let result = send(&r).await;
                    assert!(
                        matches!(result, Err(ReservationError::ScreenChanged { .. })),
                        "{result:?}"
                    );
                    assert!(effects(&scripts.mock()).is_empty());
                    scripts
                        .script_capture(&frame(CaptureFormat::Plain))
                        .unwrap();
                    send(&r).await?;
                    assert_eq!(effects(&scripts.mock()), [literal("t")]);
                }
                for panics in [false, true] {
                    scripts
                        .script_capture(&frame(CaptureFormat::Plain))
                        .unwrap();
                    scripts.mock().clear_calls();
                    let result = r
                        .literal_key_guarded(
                            &pane(),
                            r.agent().clone(),
                            't',
                            CaptureFormat::Plain,
                            move |_| {
                                assert!(!panics, "intentional guard panic");
                                false
                            },
                        )
                        .await;
                    assert!(matches!(
                        result,
                        Err(ReservationError::ScreenChanged { .. })
                    ));
                    assert!(effects(&scripts.mock()).is_empty());
                }
                Ok(())
            },
            noop,
        )
        .await
        .unwrap();
    cancel.cancel();
    task.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn literal_key_guarded_checks_tag_and_reservation() {
    let client = ScriptedTmuxClient::new();
    let (control, cancel, task) = actor(client.clone());
    let scripts = client.clone();
    let old = control.run_reserved(request(ReservationKind::Up), move |r| async move {
        for tag in [None, Some(""), Some("foreign/agent")] {
            let mut fresh = frame(CaptureFormat::Plain);
            fresh.inspection.local_agent_tag = tag.map(str::to_owned);
            scripts.script_capture(&fresh).unwrap();
            let result = r.literal_key_guarded(&pane(), r.agent().clone(), 't', CaptureFormat::Plain,
                |_| panic!("wrong tag reached predicate")).await;
            assert!(matches!(result, Err(ReservationError::TagMismatch { actual, .. }) if actual.as_deref() == tag));
        }
        assert!(effects(&scripts.mock()).is_empty());
        scripts.mock().clear_calls();
        let result = r.literal_key_guarded(&pane(), "foreign/agent".parse().unwrap(), 't', CaptureFormat::Plain,
            |_| panic!("wrong agent reached predicate")).await;
        assert!(matches!(result, Err(ReservationError::InvalidRequest { .. })));
        let result = r.literal_key_guarded(&PaneId::parse("%9")?, r.agent().clone(), 't', CaptureFormat::Plain,
            |_| panic!("unowned pane reached predicate")).await;
        assert!(matches!(result, Err(ReservationError::StaleReservation)));
        assert!(scripts.mock().calls().is_empty());
        scripts.script_capture(&frame(CaptureFormat::Plain)).unwrap();
        send(&r).await?;
        assert_eq!(effects(&scripts.mock()), [literal("t")]);
        Ok(r)
    }, noop).await.unwrap();
    client.mock().clear_calls();
    assert!(matches!(
        send(&old).await,
        Err(ReservationError::StaleReservation)
    ));
    for kind in [
        ReservationKind::Observe,
        ReservationKind::Adopt,
        ReservationKind::Down,
    ] {
        control
            .run_reserved(
                request(kind),
                |r| async move {
                    let result = r
                        .literal_key_guarded(
                            &pane(),
                            r.agent().clone(),
                            't',
                            CaptureFormat::Plain,
                            |_| panic!("wrong kind reached predicate"),
                        )
                        .await;
                    assert!(matches!(
                        result,
                        Err(ReservationError::InvalidRequest { .. })
                    ));
                    Ok(())
                },
                noop,
            )
            .await
            .unwrap();
    }
    assert!(client.mock().calls().is_empty());
    cancel.cancel();
    task.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn literal_key_guarded_expires_during_pacing_without_late_input() {
    let client = ScriptedTmuxClient::new();
    let (control, cancel, task) = actor(client.clone());
    let scripts = client.clone();
    let mut req = request(ReservationKind::Up);
    req.deadline = Instant::now() + Duration::from_millis(100);
    let result = control
        .run_reserved(
            req,
            move |r| async move {
                scripts
                    .script_capture(&frame(CaptureFormat::Plain))
                    .unwrap();
                r.capture(&pane(), CaptureFormat::Plain).await?;
                // Capture pacing is 250 ms. The queued literal cannot outlive this reservation.
                r.literal_key_guarded(
                    &pane(),
                    r.agent().clone(),
                    't',
                    CaptureFormat::Plain,
                    |_| panic!("expired operation reached predicate"),
                )
                .await
            },
            noop,
        )
        .await;
    assert!(
        matches!(result, Err(ReservationError::Timeout)),
        "{result:?}"
    );
    assert!(effects(&client.mock()).is_empty());
    // Drain the paced queue before shutdown using a fresh operation's reply.
    // The expired operation must neither consume this capture nor send late input.
    tokio::time::advance(Duration::from_secs(1)).await;
    client.script_capture(&frame(CaptureFormat::Plain)).unwrap();
    control
        .run_reserved(
            request(ReservationKind::Up),
            |r| async move { send(&r).await },
            noop,
        )
        .await
        .unwrap();
    assert_eq!(effects(&client.mock()), [literal("t")]);
    cancel.cancel();
    task.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn literal_key_guarded_missing_or_bad_capture_never_sends() {
    for metadata in ["", "malformed\n"] {
        let mock = MockTmuxClient::new();
        if metadata.is_empty() {
            mock.set_next_error(TmuxError::CommandFailed {
                command: "list-panes".into(),
                stderr: "can't find pane: %0".into(),
            });
            // Failed compound stdout is discarded; empty metadata diagnoses absence.
            mock.set_next_raw_output("discarded failed stdout");
        }
        mock.set_next_raw_output(metadata);
        let (control, cancel, task) = actor(mock.clone());
        let result = control
            .run_reserved(
                request(ReservationKind::Up),
                |r| async move {
                    r.literal_key_guarded(
                        &pane(),
                        r.agent().clone(),
                        't',
                        CaptureFormat::Plain,
                        |_| panic!("bad capture reached predicate"),
                    )
                    .await
                },
                noop,
            )
            .await;
        if metadata.is_empty() {
            assert!(matches!(result, Err(ReservationError::PaneGone { .. })));
        } else {
            assert!(
                matches!(result, Err(ReservationError::InvalidObservation { .. })),
                "{result:?}"
            );
        }
        assert!(effects(&mock).is_empty());
        cancel.cancel();
        task.await.unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn literal_key_guarded_tmux_failure_does_not_retry() {
    for error in [
        TmuxError::ParseError("literal transport failure".into()),
        TmuxError::Timeout {
            command: "send-keys".into(),
            timeout: Duration::from_secs(30),
        },
    ] {
        let expected = error.to_string();
        let client = ScriptedTmuxClient::new();
        client.script_capture(&frame(CaptureFormat::Plain)).unwrap();
        let (control, cancel, task) = actor(client.clone());
        let fault = client.mock();
        let result = control
            .run_reserved(
                request(ReservationKind::Up),
                move |r| async move {
                    r.literal_key_guarded(
                        &pane(),
                        r.agent().clone(),
                        't',
                        CaptureFormat::Plain,
                        move |fresh| {
                            // Test-only injection at the effect boundary after all reads.
                            fault.set_next_error(error);
                            approved(fresh)
                        },
                    )
                    .await
                },
                noop,
            )
            .await;
        assert!(
            matches!(result, Err(ReservationError::Control(ControlError::Tmux {
            ref source, ..
        })) if source.to_string() == expected),
            "{result:?}"
        );
        cancel.cancel();
        task.await.unwrap();
        assert_eq!(effects(&client.mock()), [literal("t")]);
    }
}

#[tokio::test]
async fn literal_script_fake_still_rejects_noncontract_raw_commands() {
    let client = ScriptedTmuxClient::new();
    for args in [
        vec!["-t", "%0", "Enter"],
        vec!["-l", "-t", "%00", "--", "t"],
        vec!["-l", "-t", "%0", "--", "two"],
        vec!["-l", "-t", "%0", "--", ";"],
        vec!["-l", "-t", "%0", "--", "\n"],
    ] {
        assert!(client.raw_command("send-keys", &args).await.is_err());
    }
    assert!(client.raw_command("run-shell", &["never"]).await.is_err());
}
