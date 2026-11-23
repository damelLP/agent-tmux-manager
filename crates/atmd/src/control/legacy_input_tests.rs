//! Legacy input must share the reservation fence, including aliases and restart.
#[path = "legacy_deadline_tests.rs"]
mod deadline_tests;
#[path = "legacy_wait_tests.rs"]
mod wait_tests;
use super::*;
use crate::control::{legacy_survey_test::mock_shell, reservation::*};
use atm_core::{SessionDomain, SessionId, SessionStatus};
use atm_tmux::{mock::MockCall, MockTmuxClient};
use std::{future::Future, task::Poll};

fn request(kind: ReservationKind) -> ReservationRequest {
    ReservationRequest::new(
        "p/a".parse().unwrap(),
        Some(PaneId::parse("%1").unwrap()),
        kind,
        WaitPolicy::Wait,
        Instant::now() + Duration::from_secs(60),
    )
}
async fn noop(_: Reservation, _: Completion) -> Result<(), ReservationError> {
    Ok(())
}
async fn barrier(control: &ControlHandle) {
    control
        .prepare_spawn(Some("%999".into()), Some("/private".into()), None, None)
        .await
        .unwrap();
}
async fn queued<T: Send + 'static>(
    future: impl Future<Output = T> + Send + 'static,
) -> JoinHandle<T> {
    let mut future = Box::pin(future);
    std::future::poll_fn(|cx| match future.as_mut().poll(cx) {
        Poll::Pending => Poll::Ready(()),
        Poll::Ready(_) => panic!("expected queued request"),
    })
    .await;
    tokio::spawn(future)
}
async fn hold(
    control: &ControlHandle,
    kind: ReservationKind,
) -> (
    JoinHandle<Result<(), ReservationError>>,
    oneshot::Sender<()>,
) {
    let c = control.clone();
    let (entered, entry) = oneshot::channel();
    let (release, released) = oneshot::channel();
    let job = queued(async move {
        c.run_reserved(
            request(kind),
            move |_| async move {
                entered.send(()).unwrap();
                released.await.unwrap();
                Ok(())
            },
            noop,
        )
        .await
    })
    .await;
    entry.await.unwrap();
    (job, release)
}
async fn healthy() -> (tempfile::TempDir, OrgHandle, CancellationToken) {
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let org = OrgHandle::open(dir.path().into(), cancel.clone()).await;
    (dir, org, cancel)
}
async fn ready(org: &OrgHandle) {
    org.execute(
        OrgCommand::RegisterProject {
            project_id: "p".parse().unwrap(),
            root: "/private".into(),
            move_root: false,
        },
        chrono::Utc::now(),
    )
    .await
    .unwrap();
    org.execute(
        OrgCommand::Adopt {
            agent_id: "p/a".parse().unwrap(),
            pane: "%1".into(),
            session_id: Some("old-session".into()),
        },
        chrono::Utc::now(),
    )
    .await
    .unwrap();
}
async fn clear(org: &OrgHandle) {
    org.execute(
        OrgCommand::Clear {
            agent_id: "p/a".parse().unwrap(),
        },
        chrono::Utc::now(),
    )
    .await
    .unwrap();
}
fn input_calls(mock: &MockTmuxClient) -> Vec<MockCall> {
    mock.calls()
        .into_iter()
        .filter(|c| matches!(c, MockCall::PasteText { .. } | MockCall::SendKeys { .. }))
        .collect()
}
fn untagged(mock: &MockTmuxClient, pane: &str) {
    mock.set_next_raw_output(&format!(
        "ATM-PANE-v1\t{pane}\tprivate\t0\t123\t80\t24\t1\t0\t0\t1\t0\t0\n"
    ));
    mock.set_next_raw_output("");
}

#[tokio::test]
async fn legacy_send_checks_the_fresh_survey_frame_before_paste_or_enter() {
    use crate::control::legacy_survey_test::{fixture, mock_viewport};
    let (_dir, org, cancel) = healthy().await;
    let mock = MockTmuxClient::new();
    let registry = crate::registry::spawn_registry();
    registry
        .register(SessionDomain {
            id: SessionId::new("recorded-session"),
            harness: atm_core::Harness::ClaudeCode,
            tmux_pane: Some("%2".into()),
            status: SessionStatus::Idle,
            ..SessionDomain::default()
        })
        .await
        .unwrap();
    let (control, actor) = spawn_control(Box::new(mock.clone()), registry, org, cancel.clone());
    let survey = fixture();
    let digit_draft = survey.replacen("❯\u{a0} ", "❯\u{a0}0", 1);
    assert_ne!(digit_draft, survey);
    // An unrecognized notice between the survey and the editor border is a known fail-open.
    let unknown_notice = survey.replace("✔ Update installed", "Unrecognized notice");
    let no_completion = survey.replace("Crunched for 27s · done 12:15", "No completion recorded");
    let busy = survey.replace(
        "(shift+tab to cycle)",
        "(shift+tab to cycle) · esc to interrupt",
    );
    for (pane, frame, text, flag, refused) in [
        ("%1", &survey, "0", Some(true), true),
        ("%2", &survey, "1", Some(true), true),
        ("%1", &survey, "１", Some(true), true),
        ("%1", &survey, "&", Some(true), true),
        ("%1", &survey, "4", Some(true), false),
        ("%1", &survey, "1 test", Some(true), false),
        ("%1", &survey, "1 test", Some(false), true),
        ("%1", &survey, "1 test", None, true),
        ("%1", &survey, "1 ", Some(true), true),
        ("%1", &survey, "0\n", Some(true), true),
        ("%1", &survey, "é\u{2003}", Some(true), true),
        ("%1", &survey, "1 ", Some(false), true),
        ("%1", &survey, "1 ", None, true),
        ("%1", &digit_draft, "", Some(true), true),
        ("%1", &no_completion, "1", Some(true), true),
        ("%1", &busy, "1", Some(true), true),
        ("%1", &unknown_notice, "1", Some(true), false),
    ] {
        mock.clear_calls();
        mock.set_next_raw_output("0 1\t0\t0\n");
        mock_viewport(&mock, pane, frame, flag);
        let result = control.send(pane.into(), text.into()).await;
        if refused {
            let Err(error) = result else {
                panic!("expected refusal for {text:?} with bracketed={flag:?}");
            };
            let error = crate::server::methods::control_error(error).0;
            assert_eq!(error.code, "refused");
            assert!(
                error.message.contains("feedback survey"),
                "{}",
                error.message
            );
            assert!(input_calls(&mock).is_empty());
        } else {
            assert_eq!(result.unwrap(), pane);
            assert_eq!(
                input_calls(&mock),
                [
                    MockCall::PasteText {
                        pane: pane.into(),
                        text: text.into()
                    },
                    MockCall::SendKeys {
                        pane: pane.into(),
                        keys: "Enter".into()
                    },
                ]
            );
        }
    }
    // A malformed new observation is a read error, not a survey or a grant.
    mock.clear_calls();
    mock.set_next_raw_output("0 1\t0\t0\n");
    mock.set_next_raw_output("malformed frame\n");
    let err = control.send("%1".into(), "probe".into()).await.unwrap_err();
    assert!(err.to_string().contains("invalid observation"), "{err}");
    assert!(input_calls(&mock).is_empty());
    cancel.cancel();
    actor.await.unwrap();
}

#[tokio::test]
async fn reservation_raw_send_and_spawn_refused_during_clear_via_router() {
    let (_dir, org, cancel) = healthy().await;
    let mock = MockTmuxClient::new();
    let registry = crate::registry::spawn_registry();
    let (control, actor) = spawn_control(
        Box::new(mock.clone()),
        registry.clone(),
        org.clone(),
        cancel.clone(),
    );
    mock.set_next_raw_output("ATM-PANE-v1\t%1\tprivate\t0\t123\t80\t24\t1\t0\t0\t1\t0\t0\n");
    mock.set_next_raw_output("p/a\n");
    let (entered, entry) = oneshot::channel();
    let (release, released) = oneshot::channel();
    let c = control.clone();
    let job = queued(async move {
        c.run_reserved(
            request(ReservationKind::Clear),
            move |r| async move {
                r.keys(&PaneId::parse("%1")?, r.agent().clone(), PaneKey::Enter)
                    .await?;
                entered.send(()).unwrap();
                released.await.unwrap();
                Ok::<_, ReservationError>(())
            },
            noop,
        )
        .await
    })
    .await;
    entry.await.unwrap(); // Enter acknowledged; editor confirmation held
    assert_eq!(input_calls(&mock).len(), 1);
    mock.clear_calls();
    let (delivered, mut delivery_entered) = oneshot::channel();
    let c = control.clone();
    let delivery = queued(async move {
        c.run_reserved(
            request(ReservationKind::Delivery),
            move |_| async move {
                delivered.send(()).unwrap();
                Ok::<_, ReservationError>(())
            },
            noop,
        )
        .await
    })
    .await;
    barrier(&control).await;
    assert!(matches!(
        delivery_entered.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    let mut services = crate::server::methods::MethodServices::new(registry);
    services.org = org;
    services.control = Some(control.clone());
    let router = crate::server::methods::router(services).unwrap();
    for (method, params) in [
        (
            "session.send",
            serde_json::json!({"target":"%1","text":"unsafe"}),
        ),
        (
            "session.spawn_type",
            serde_json::json!({"pane":"%1","command":"unsafe"}),
        ),
    ] {
        let error = router
            .call(
                crate::server::methods::CallContext {
                    client_id: crate::server::methods::CallerId("test".into()),
                    from_pane: None,
                },
                method,
                params,
            )
            .await
            .unwrap_err();
        assert_eq!(error.0.code, "refused");
        assert_eq!(
            error.0.message,
            "Refusing to send to %1: agent p/a is clearing; try again in a moment"
        );
    }
    assert!(input_calls(&mock).is_empty());
    assert!(matches!(
        delivery_entered.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    release.send(()).unwrap();
    job.await.unwrap().unwrap();
    delivery_entered.await.unwrap();
    delivery.await.unwrap().unwrap();
    cancel.cancel();
    actor.await.unwrap();
}

#[tokio::test]
async fn reservation_legacy_aliases_cannot_bypass_clear() {
    let (_dir, org, cancel) = healthy().await;
    let mock = MockTmuxClient::new();
    let (control, actor) = spawn_control(
        Box::new(mock.clone()),
        crate::registry::spawn_registry(),
        org,
        cancel.clone(),
    );
    let (job, release) = hold(&control, ReservationKind::Clear).await;
    for target in ["%01", "private:0.1"] {
        mock.set_next_raw_output("%1\n");
        assert!(
            matches!(control.type_spawn_command(target.into(), "unsafe".into()).await,
            Err(ServiceError::Control(ControlError::AgentClearing { pane, .. })) if pane == "%1")
        );
    }
    mock.set_next_raw_output("%1\n");
    assert!(matches!(
        control.send("%01".into(), "unsafe".into()).await,
        Err(ServiceError::Control(ControlError::AgentClearing { .. }))
    ));
    mock.set_next_raw_output("\n");
    assert!(control
        .type_spawn_command("%1junk".into(), "unsafe".into())
        .await
        .is_err());
    assert!(input_calls(&mock).is_empty());
    release.send(()).unwrap();
    job.await.unwrap().unwrap();
    cancel.cancel();
    actor.await.unwrap();
}

#[tokio::test]
async fn reservation_other_raw_sends_and_spawn_wait_without_blocking_other_panes() {
    tokio::time::timeout(Duration::from_secs(60), async {
        for kind in [
            ReservationKind::Adopt,
            ReservationKind::Up,
            ReservationKind::Respawn,
            ReservationKind::Down,
            ReservationKind::Delivery,
            ReservationKind::Observe,
        ] {
            let (_dir, org, cancel) = healthy().await;
            let mock = MockTmuxClient::new();
            let (control, actor) = spawn_control(
                Box::new(mock.clone()),
                crate::registry::spawn_registry(),
                org,
                cancel.clone(),
            );
            let (job, release) = hold(&control, kind).await;
            let c = control.clone();
            let send = queued(async move { c.send("%1".into(), "after".into()).await }).await;
            let c = control.clone();
            let spawn =
                queued(async move { c.type_spawn_command("%1".into(), "launch".into()).await })
                    .await;
            barrier(&control).await; // actor processed both enqueues; not scheduler timing
            assert!(input_calls(&mock).is_empty(), "{kind:?}");
            control
                .type_spawn_command("%2".into(), "other".into())
                .await
                .unwrap();
            assert!(input_calls(&mock)
                .iter()
                .all(|c| matches!(c, MockCall::SendKeys { pane, .. } if pane == "%2")));
            mock.clear_calls();
            mock.set_next_raw_output("0 1");
            mock_shell(&mock, "%1");
            release.send(()).unwrap();
            job.await.unwrap().unwrap();
            assert_eq!(send.await.unwrap().unwrap(), "%1");
            spawn.await.unwrap().unwrap();
            assert_eq!(
                input_calls(&mock),
                vec![
                    MockCall::PasteText {
                        pane: "%1".into(),
                        text: "after".into()
                    },
                    MockCall::SendKeys {
                        pane: "%1".into(),
                        keys: "Enter".into()
                    },
                    MockCall::SendKeys {
                        pane: "%1".into(),
                        keys: "launch".into()
                    },
                    MockCall::SendKeys {
                        pane: "%1".into(),
                        keys: "Enter".into()
                    },
                ]
            );
            cancel.cancel();
            actor.await.unwrap();
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn reservation_up_created_pane_parks_send_and_spawn_once_in_order() {
    let (_dir, org, cancel) = healthy().await;
    let mock = MockTmuxClient::new();
    let (control, actor) = spawn_control_with_audit(
        Box::new(mock.clone()),
        crate::registry::spawn_registry(),
        org_mirror::Mirror::start(org),
        cancel.clone(),
        |_, _| async { Ok(Vec::new()) },
    );
    // Wait for the real mirror before testing ownership, so Loading cannot hide
    // a missing created-pane fence.
    control
        .type_spawn_command("%2".into(), "warmup".into())
        .await
        .unwrap();
    control.flush_audits().await;
    mock.clear_calls();
    mock.set_next_pane_id("%9");
    let (split, created) = oneshot::channel();
    let (release, released) = oneshot::channel();
    let c = control.clone();
    let up = queued(async move {
        c.run_reserved(
            ReservationRequest::new(
                "p/a".parse().unwrap(),
                None,
                ReservationKind::Up,
                WaitPolicy::Wait,
                Instant::now() + Duration::from_secs(60),
            ),
            move |r| async move {
                let pane = r
                    .split(
                        SpawnContext::new("%0".into(), None, false),
                        PaneDirection::Below,
                        "50%".into(),
                    )
                    .await?;
                split.send(pane).unwrap();
                released.await.unwrap();
                Ok::<_, ReservationError>(())
            },
            noop,
        )
        .await
    })
    .await;
    assert_eq!(created.await.unwrap().as_str(), "%9");
    let c = control.clone();
    let send = queued(async move { c.send("%9".into(), "raw".into()).await }).await;
    let c = control.clone();
    let spawn =
        queued(async move { c.type_spawn_command("%9".into(), "launch".into()).await }).await;
    barrier(&control).await;
    assert!(input_calls(&mock).is_empty());
    mock.set_next_raw_output("0 1");
    mock_shell(&mock, "%9");
    release.send(()).unwrap();
    up.await.unwrap().unwrap();
    assert_eq!(send.await.unwrap().unwrap(), "%9");
    spawn.await.unwrap().unwrap();
    assert_eq!(
        input_calls(&mock),
        vec![
            MockCall::PasteText {
                pane: "%9".into(),
                text: "raw".into()
            },
            MockCall::SendKeys {
                pane: "%9".into(),
                keys: "Enter".into()
            },
            MockCall::SendKeys {
                pane: "%9".into(),
                keys: "launch".into()
            },
            MockCall::SendKeys {
                pane: "%9".into(),
                keys: "Enter".into()
            },
        ]
    );
    cancel.cancel();
    actor.await.unwrap();
}

#[tokio::test]
async fn reservation_durable_clearing_and_excluded_rows_survive_actor_restart() {
    let (_dir, org, cancel) = healthy().await;
    ready(&org).await;
    clear(&org).await;
    let registry = crate::registry::spawn_registry();
    registry
        .register(SessionDomain {
            id: SessionId::new("old-session"),
            tmux_pane: Some("%1".into()),
            status: SessionStatus::AttentionNeeded,
            ..SessionDomain::default()
        })
        .await
        .unwrap();
    let mock = MockTmuxClient::new();
    let (control, actor) = spawn_control(
        Box::new(mock.clone()),
        registry.clone(),
        org.clone(),
        cancel.clone(),
    );
    for target in ["%1", "old-session"] {
        assert!(matches!(
            control.send(target.into(), "unsafe".into()).await,
            Err(ServiceError::Control(ControlError::AgentClearing { .. }))
        ));
    }
    assert!(matches!(
        control
            .type_spawn_command("%1".into(), "unsafe".into())
            .await,
        Err(ServiceError::Control(ControlError::AgentClearing { .. }))
    ));
    assert!(input_calls(&mock).is_empty());
    let o = org.clone();
    control
        .run_reserved(
            request(ReservationKind::Observe),
            move |_| async move {
                o.execute(
                    OrgCommand::EditorReady {
                        agent_id: "p/a".parse().unwrap(),
                    },
                    chrono::Utc::now(),
                )
                .await
                .unwrap();
                Ok::<_, ReservationError>(())
            },
            noop,
        )
        .await
        .unwrap();
    assert!(matches!(
        control.send("old-session".into(), "obsolete".into()).await,
        Err(ServiceError::Control(ControlError::NoSession { .. }))
    ));
    mock.set_next_raw_output("0 1");
    mock_shell(&mock, "%1");
    assert_eq!(
        control.send("%1".into(), "current".into()).await.unwrap(),
        "%1"
    );
    registry
        .register(SessionDomain {
            id: SessionId::new("new-session"),
            tmux_pane: Some("%1".into()),
            status: SessionStatus::AttentionNeeded,
            ..SessionDomain::default()
        })
        .await
        .unwrap();
    mock.set_next_raw_output("0 1");
    assert!(matches!(
        control.send("%1".into(), "dialog".into()).await,
        Err(ServiceError::Control(ControlError::SendRefused { .. }))
    ));
    registry
        .register(SessionDomain {
            id: SessionId::new("elsewhere"),
            tmux_pane: Some("%2".into()),
            status: SessionStatus::AttentionNeeded,
            ..SessionDomain::default()
        })
        .await
        .unwrap();
    mock.set_next_raw_output("0 1");
    assert!(matches!(
        control.send("%2".into(), "dialog".into()).await,
        Err(ServiceError::Control(ControlError::SendRefused { .. }))
    ));
    cancel.cancel();
    actor.await.unwrap();
}

#[tokio::test]
async fn reservation_disabled_org_unknown_or_tagged_fails_closed_but_untagged_sends() {
    let mock = MockTmuxClient::new();
    let cancel = CancellationToken::new();
    let (control, actor) = spawn_control(
        Box::new(mock.clone()),
        crate::registry::spawn_registry(),
        OrgHandle::disabled("test"),
        cancel.clone(),
    );
    // No metadata is positive unknown ownership, never permission to input.
    for spawn in [false, true] {
        let error = if spawn {
            control
                .type_spawn_command("%1".into(), "unsafe".into())
                .await
                .map(|()| "%1".into())
        } else {
            control.send("%1".into(), "unsafe".into()).await
        };
        assert!(matches!(
            error,
            Err(ServiceError::Control(
                ControlError::OrganisationUnavailable { .. }
            ))
        ));
    }
    for tag in ["p/a\n", "\n", "malformed\n"] {
        mock.set_next_raw_output("ATM-PANE-v1\t%1\tprivate\t0\t123\t80\t24\t1\t0\t0\t1\t0\t0\n");
        mock.set_next_raw_output(tag);
        assert!(matches!(
            control.send("%1".into(), "unsafe".into()).await,
            Err(ServiceError::Control(
                ControlError::OrganisationUnavailable { .. }
            ))
        ));
    }
    assert!(input_calls(&mock).is_empty());
    untagged(&mock, "%1");
    mock.set_next_raw_output("0 1");
    mock_shell(&mock, "%1");
    assert_eq!(
        control.send("%1".into(), "safe".into()).await.unwrap(),
        "%1"
    );
    untagged(&mock, "%2");
    control
        .type_spawn_command("%2".into(), "launch".into())
        .await
        .unwrap();
    assert_eq!(input_calls(&mock).len(), 4);
    cancel.cancel();
    actor.await.unwrap();
}

#[tokio::test]
async fn reservation_quarantine_refuses_both_legacy_input_paths() {
    let (_dir, org, cancel) = healthy().await;
    let mock = MockTmuxClient::new();
    let (control, actor) = spawn_control(
        Box::new(mock.clone()),
        crate::registry::spawn_registry(),
        org,
        cancel.clone(),
    );
    let result = control
        .run_reserved(
            request(ReservationKind::Down),
            |_| async { Ok(()) },
            |_, _| async { Err::<(), _>(ReservationError::Unavailable) },
        )
        .await;
    assert!(result.is_err());
    assert!(matches!(
        control.send("%1".into(), "unsafe".into()).await,
        Err(ServiceError::Control(ControlError::PaneQuarantined { .. }))
    ));
    assert!(matches!(
        control
            .type_spawn_command("%1".into(), "unsafe".into())
            .await,
        Err(ServiceError::Control(ControlError::PaneQuarantined { .. }))
    ));
    assert!(input_calls(&mock).is_empty());
    cancel.cancel();
    actor.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn reservation_parked_legacy_input_expires_without_late_effects() {
    let mock = MockTmuxClient::new();
    let cancel = CancellationToken::new();
    let (control, actor) = spawn_control(
        Box::new(mock.clone()),
        crate::registry::spawn_registry(),
        OrgHandle::disabled("test"),
        cancel.clone(),
    );
    let (job, release) = hold(&control, ReservationKind::Delivery).await;
    let c = control.clone();
    let send = queued(async move { c.send("%1".into(), "late".into()).await }).await;
    barrier(&control).await;
    tokio::time::advance(Duration::from_secs(31)).await;
    assert!(matches!(send.await.unwrap(), Err(ServiceError::Timeout)));
    release.send(()).unwrap();
    job.await.unwrap().unwrap();
    barrier(&control).await;
    assert!(input_calls(&mock).is_empty());
    cancel.cancel();
    actor.await.unwrap();
}
