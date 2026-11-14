//! Real actor with per-pane tmux scripts; every assertion follows a reply/channel.
#![allow(clippy::unwrap_used)]
use atm_tmux::{mock::MockCall, PaneDirection, PaneInfo};
use atmd::control::{reservation::*, service::spawn_control, SpawnContext};
use std::time::Duration;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

fn pane() -> PaneId {
    PaneId::parse("%0").unwrap()
}
fn inspection(tag: Option<&str>) -> PaneInspection {
    PaneInspection::new(
        PaneInfo {
            pane_id: "%0".into(),
            session_name: "private".into(),
            window_index: 0,
            pane_pid: 2353666,
            width: 40,
            height: 8,
            is_active: true,
        },
        tag.map(str::to_owned),
        false,
        false,
        Some(true),
        5,
        1,
    )
}
fn frame(tag: Option<&str>, format: CaptureFormat) -> PaneCapture {
    PaneCapture::new(
        "probe     \nready     \n\n\n\n\n\n\n".into(),
        inspection(tag),
        format,
    )
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
fn effects(mock: &atm_tmux::MockTmuxClient) -> Vec<MockCall> {
    mock.calls().into_iter().filter(|call| !matches!(call, MockCall::RawCommand { subcommand, .. } if subcommand != "set-option")).collect()
}

#[tokio::test(start_paused = true)]
async fn reserved_input_refuses_unsafe_state_before_any_guard_or_effect() {
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    let client = testing::ScriptedTmuxClient::new();
    let cancel = CancellationToken::new();
    let (control, actor) = spawn_control(
        Box::new(client.clone()),
        atmd::registry::spawn_registry(),
        atmd::org::OrgHandle::disabled("safety test"),
        cancel.clone(),
    );
    let scripts = client.clone();
    control
        .run_reserved(
            request(ReservationKind::Delivery),
            move |r| async move {
                for dead in [false, true] {
                    for operation in 0..4 {
                        let mut fresh = frame(Some("project/lead"), CaptureFormat::Plain);
                        fresh.inspection.dead = dead;
                        fresh.inspection.in_mode = !dead;
                        if operation >= 2 {
                            scripts.script_capture(&fresh).unwrap();
                        } else {
                            scripts.script_tag_check(&fresh.inspection).unwrap();
                        }
                        let called = Arc::new(AtomicBool::new(false));
                        let mark = called.clone();
                        let guard = move |_: &PaneCapture| {
                            mark.store(true, Ordering::SeqCst);
                            true
                        };
                        let result = match operation {
                            0 => r.keys(&pane(), r.agent().clone(), PaneKey::Enter).await,
                            1 => r.paste(&pane(), r.agent().clone(), "a\nb".into()).await,
                            2 => {
                                r.keys_guarded(
                                    &pane(),
                                    r.agent().clone(),
                                    PaneKey::Enter,
                                    CaptureFormat::Plain,
                                    guard,
                                )
                                .await
                            }
                            _ => {
                                r.paste_guarded(
                                    &pane(),
                                    r.agent().clone(),
                                    "a\nb".into(),
                                    CaptureFormat::Plain,
                                    guard,
                                )
                                .await
                            }
                        };
                        assert!(
                            matches!(result, Err(ReservationError::PaneInputRefused { .. })),
                            "{result:?}"
                        );
                        assert!(!called.load(Ordering::SeqCst));
                        assert!(effects(&scripts.mock()).is_empty());
                    }
                }
                Ok(())
            },
            noop,
        )
        .await
        .unwrap();
    cancel.cancel();
    actor.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn reserved_paste_preserves_unknown_flag_policy_but_refuses_control_bytes() {
    let client = testing::ScriptedTmuxClient::new();
    let cancel = CancellationToken::new();
    let (control, actor) = spawn_control(
        Box::new(client.clone()),
        atmd::registry::spawn_registry(),
        atmd::org::OrgHandle::disabled("paste safety test"),
        cancel.clone(),
    );
    let scripts = client.clone();
    control
        .run_reserved(
            request(ReservationKind::Delivery),
            move |r| async move {
                for guarded in [false, true] {
                    for bracketed in [Some(false), Some(true), None] {
                        for text in [
                            "a",
                            "a\nb",
                            "a\rb",
                            "a\tb",
                            "a\x1b[201~b",
                            "a\x03b",
                            "a\0b",
                            "a\x7fb",
                        ] {
                            let mut fresh = frame(Some("project/lead"), CaptureFormat::Plain);
                            fresh.inspection.bracketed_paste = bracketed;
                            if guarded {
                                scripts.script_capture(&fresh).unwrap();
                            } else {
                                scripts.script_tag_check(&fresh.inspection).unwrap();
                            }
                            let before = effects(&scripts.mock()).len();
                            let result = if guarded {
                                r.paste_guarded(
                                    &pane(),
                                    r.agent().clone(),
                                    text.into(),
                                    CaptureFormat::Plain,
                                    |_| true,
                                )
                                .await
                            } else {
                                r.paste(&pane(), r.agent().clone(), text.into()).await
                            };
                            let refuse = (bracketed == Some(false) && text.contains(['\n', '\r']))
                                || text.contains(['\x1b', '\x03', '\0', '\x7f']);
                            assert_eq!(result.is_err(), refuse, "{guarded} {bracketed:?} {text:?}");
                            if text.contains(['\x1b', '\x03', '\0', '\x7f']) {
                                assert!(
                                    matches!(result, Err(ReservationError::UnsafePaste { .. })),
                                    "{result:?}"
                                );
                            } else if refuse {
                                assert!(
                                    matches!(
                                        result,
                                        Err(ReservationError::PaneInputRefused { .. })
                                    ),
                                    "{result:?}"
                                );
                            }
                            assert_eq!(
                                effects(&scripts.mock()).len(),
                                before + usize::from(!refuse)
                            );
                        }
                    }
                }
                Ok(())
            },
            noop,
        )
        .await
        .unwrap();
    cancel.cancel();
    actor.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn reserved_guard_uses_fresh_capture_and_refuses_changed_screen() {
    let client = testing::ScriptedTmuxClient::new();
    let cancel = CancellationToken::new();
    let (control, actor) = spawn_control(
        Box::new(client.clone()),
        atmd::registry::spawn_registry(),
        atmd::org::OrgHandle::disabled("pane effect test"),
        cancel.clone(),
    );
    let scripts = client.clone();
    control
        .run_reserved(
            request(ReservationKind::Delivery),
            move |r| async move {
                scripts
                    .script_capture(&frame(Some("project/lead"), CaptureFormat::Ansi))
                    .unwrap();
                let (seen, received) = tokio::sync::oneshot::channel();
                assert!(matches!(
                    r.keys_guarded(
                        &pane(),
                        r.agent().clone(),
                        PaneKey::Enter,
                        CaptureFormat::Ansi,
                        move |fresh| {
                            let _ = seen.send(fresh.text.clone());
                            false
                        }
                    )
                    .await,
                    Err(ReservationError::ScreenChanged { .. })
                ));
                assert_eq!(
                    received.await.unwrap(),
                    "probe     \nready     \n\n\n\n\n\n\n"
                );
                assert!(effects(&scripts.mock()).is_empty());
                scripts
                    .script_capture(&frame(Some("project/lead"), CaptureFormat::Plain))
                    .unwrap();
                r.paste_guarded(
                    &pane(),
                    r.agent().clone(),
                    "Enter;\n'\"".into(),
                    CaptureFormat::Plain,
                    |fresh| fresh.inspection.cursor_column == 5,
                )
                .await?;
                scripts
                    .script_capture(&frame(Some("project/lead"), CaptureFormat::Plain))
                    .unwrap();
                r.keys_guarded(
                    &pane(),
                    r.agent().clone(),
                    PaneKey::Enter,
                    CaptureFormat::Plain,
                    |_| true,
                )
                .await?;
                assert_eq!(scripts.pending_reads(&pane()).unwrap(), 0);
                Ok(())
            },
            noop,
        )
        .await
        .unwrap();
    assert_eq!(
        effects(&client.mock()),
        vec![
            MockCall::PasteText {
                pane: "%0".into(),
                text: "Enter;\n'\"".into()
            },
            MockCall::SendKeys {
                pane: "%0".into(),
                keys: "Enter".into()
            }
        ]
    );
    cancel.cancel();
    actor.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn reserved_every_effect_rechecks_local_tag_and_expected_agent() {
    let client = testing::ScriptedTmuxClient::new();
    let cancel = CancellationToken::new();
    let (control, actor) = spawn_control(
        Box::new(client.clone()),
        atmd::registry::spawn_registry(),
        atmd::org::OrgHandle::disabled("pane effect test"),
        cancel.clone(),
    );
    let scripts = client.clone();
    control.run_reserved(request(ReservationKind::Respawn), move |r| async move {
        scripts.script_inspect(&pane(), Some(&inspection(Some("project/lead")))).unwrap();
        assert_eq!(r.inspect(&pane()).await?.unwrap().local_agent_tag.as_deref(), Some("project/lead"));
        for actual in [None, Some(""), Some("foreign/agent"), Some("malformed\ntag")] {
            for operation in 0..5 {
                scripts.script_tag_check(&inspection(actual)).unwrap();
                let result = match operation {
                    0 => r.tag(&pane(), ExpectedTag::Agent(r.agent().clone())).await,
                    1 => r.untag(&pane(), r.agent().clone()).await,
                    2 => r.paste(&pane(), r.agent().clone(), "never".into()).await,
                    3 => r.keys(&pane(), r.agent().clone(), PaneKey::Enter).await,
                    _ => r.kill(&pane(), r.agent().clone(), serde_json::json!({}), chrono::Utc::now()).await,
                };
                assert!(matches!(result, Err(ReservationError::TagMismatch { actual: found, .. }) if found.as_deref() == actual));
            }
        }
        for operation in 0..7 {
            let other = "foreign/agent".parse().unwrap();
            let result = match operation {
                0 => r.tag(&pane(), ExpectedTag::Agent(other)).await,
                1 => r.untag(&pane(), other).await,
                2 => r.paste(&pane(), other, "never".into()).await,
                3 => r.keys(&pane(), other, PaneKey::Enter).await,
                4 => r.kill(&pane(), other, serde_json::json!({}), chrono::Utc::now()).await,
                5 => r.keys_guarded(&pane(), other, PaneKey::Enter, CaptureFormat::Plain, |_| panic!("foreign guard ran")).await,
                _ => r.paste_guarded(&pane(), other, "never".into(), CaptureFormat::Plain, |_| panic!("foreign guard ran")).await,
            };
            assert!(matches!(result, Err(ReservationError::InvalidRequest { .. })));
        }
        Ok(())
    }, noop).await.unwrap();
    assert!(effects(&client.mock()).is_empty());
    assert_eq!(client.pending_reads(&pane()).unwrap(), 0);
    cancel.cancel();
    actor.await.unwrap();
}

#[tokio::test]
async fn reserved_adopt_absent_does_not_match_empty_and_inspect_can_read_placement() {
    let client = testing::ScriptedTmuxClient::new();
    let cancel = CancellationToken::new();
    let (control, actor) = spawn_control(
        Box::new(client.clone()),
        atmd::registry::spawn_registry(),
        atmd::org::OrgHandle::disabled("pane effect test"),
        cancel.clone(),
    );
    let scripts = client.clone();
    control.run_reserved(request(ReservationKind::Adopt), move |r| async move {
        scripts.script_tag_check(&inspection(Some(""))).unwrap();
        assert!(matches!(r.tag(&pane(), ExpectedTag::Absent).await, Err(ReservationError::TagMismatch { actual: Some(tag), .. }) if tag.is_empty()));
        scripts.script_tag_check(&inspection(None)).unwrap();
        r.tag(&pane(), ExpectedTag::Absent).await?;
        scripts.script_tag_check(&inspection(Some("project/lead"))).unwrap();
        r.tag(&pane(), ExpectedTag::Agent(r.agent().clone())).await?;
        let other = PaneId::parse("%9")?;
        let mut placement = inspection(Some("project/other")); placement.pane.pane_id = other.to_string();
        scripts.script_inspect(&other, Some(&placement)).unwrap();
        assert_eq!(r.inspect(&other).await?, Some(placement));
        assert!(matches!(r.capture(&other, CaptureFormat::Plain).await, Err(ReservationError::StaleReservation)));
        assert!(matches!(r.keys(&pane(), r.agent().clone(), PaneKey::Enter).await, Err(ReservationError::InvalidRequest { .. })));
        Ok(())
    }, noop).await.unwrap();
    assert_eq!(effects(&client.mock()).len(), 2);
    cancel.cancel();
    actor.await.unwrap();
}

#[tokio::test]
async fn reserved_missing_is_positive_and_transport_errors_are_not_absence() {
    let client = testing::ScriptedTmuxClient::new();
    let cancel = CancellationToken::new();
    let (control, actor) = spawn_control(
        Box::new(client.clone()),
        atmd::registry::spawn_registry(),
        atmd::org::OrgHandle::disabled("pane effect test"),
        cancel.clone(),
    );
    let scripts = client.clone();
    control
        .run_reserved(
            request(ReservationKind::Observe),
            move |r| async move {
                scripts.script_inspect(&pane(), None).unwrap();
                assert!(r.inspect(&pane()).await?.is_none());
                scripts.script_inspect(&pane(), None).unwrap();
                assert!(matches!(
                    r.capture(&pane(), CaptureFormat::Plain).await,
                    Err(ReservationError::PaneGone { .. })
                ));
                let error = r.inspect(&pane()).await.unwrap_err();
                assert!(matches!(error, ReservationError::Control(_)), "{error:?}");
                let message = error.to_string();
                assert!(
                    message.contains("%0") && message.contains("read pane metadata"),
                    "{error}"
                );
                Ok(())
            },
            noop,
        )
        .await
        .unwrap();
    assert!(effects(&client.mock()).is_empty());
    cancel.cancel();
    actor.await.unwrap();
}

#[tokio::test]
async fn reserved_allowlisted_keys_paste_untag_and_kill_have_exact_effects() {
    let client = testing::ScriptedTmuxClient::new();
    let cancel = CancellationToken::new();
    let (control, actor) = spawn_control(
        Box::new(client.clone()),
        atmd::registry::spawn_registry(),
        atmd::org::OrgHandle::disabled("pane effect test"),
        cancel.clone(),
    );
    let scripts = client.clone();
    control
        .run_reserved(
            request(ReservationKind::Respawn),
            move |r| async move {
                for key in [PaneKey::Enter, PaneKey::Escape, PaneKey::Up, PaneKey::Down] {
                    scripts
                        .script_tag_check(&inspection(Some("project/lead")))
                        .unwrap();
                    r.keys(&pane(), r.agent().clone(), key).await?;
                }
                scripts
                    .script_tag_check(&inspection(Some("project/lead")))
                    .unwrap();
                r.paste(&pane(), r.agent().clone(), "Enter;\n'\"".into())
                    .await?;
                scripts
                    .script_tag_check(&inspection(Some("project/lead")))
                    .unwrap();
                r.untag(&pane(), r.agent().clone()).await?;
                scripts
                    .script_tag_check(&inspection(Some("project/lead")))
                    .unwrap();
                r.kill(
                    &pane(),
                    r.agent().clone(),
                    serde_json::json!({}),
                    chrono::Utc::now(),
                )
                .await?;
                Ok(())
            },
            noop,
        )
        .await
        .unwrap();
    assert_eq!(
        effects(&client.mock()),
        vec![
            MockCall::SendKeys {
                pane: "%0".into(),
                keys: "Enter".into()
            },
            MockCall::SendKeys {
                pane: "%0".into(),
                keys: "Escape".into()
            },
            MockCall::SendKeys {
                pane: "%0".into(),
                keys: "Up".into()
            },
            MockCall::SendKeys {
                pane: "%0".into(),
                keys: "Down".into()
            },
            MockCall::PasteText {
                pane: "%0".into(),
                text: "Enter;\n'\"".into()
            },
            MockCall::RawCommand {
                subcommand: "set-option".into(),
                args: vec!["-p", "-u", "-t", "%0", "@atm-agent"]
                    .into_iter()
                    .map(str::to_owned)
                    .collect()
            },
            MockCall::KillPane { pane: "%0".into() },
        ]
    );
    cancel.cancel();
    actor.await.unwrap();
}

#[tokio::test]
async fn reserved_split_claims_new_pane_and_fences_old_capability() {
    let client = testing::ScriptedTmuxClient::new();
    client.mock().set_next_pane_id("%1");
    let cancel = CancellationToken::new();
    let (control, actor) = spawn_control(
        Box::new(client.clone()),
        atmd::registry::spawn_registry(),
        atmd::org::OrgHandle::disabled("pane effect test"),
        cancel.clone(),
    );
    let scripts = client.clone();
    let stale = control
        .run_reserved(
            request(ReservationKind::Respawn),
            move |r| async move {
                let new = r
                    .split(
                        SpawnContext::new("%0".into(), None, false),
                        PaneDirection::Below,
                        "50%".into(),
                    )
                    .await?;
                assert_eq!(new.as_str(), "%1");
                let mut new_inspection = inspection(None);
                new_inspection.pane.pane_id = new.to_string();
                scripts.script_tag_check(&new_inspection).unwrap();
                r.tag(&new, ExpectedTag::Absent).await?;
                scripts
                    .script_tag_check(&inspection(Some("project/lead")))
                    .unwrap();
                r.keys(&pane(), r.agent().clone(), PaneKey::Escape).await?;
                assert!(matches!(
                    r.split(
                        SpawnContext::new("%0".into(), None, false),
                        PaneDirection::Below,
                        "50%".into()
                    )
                    .await,
                    Err(ReservationError::InvalidRequest { .. })
                ));
                Ok(r)
            },
            noop,
        )
        .await
        .unwrap();
    let before = client.mock().call_count();
    assert!(matches!(
        stale
            .paste(
                &PaneId::parse("%1").unwrap(),
                stale.agent().clone(),
                "late".into()
            )
            .await,
        Err(ReservationError::StaleReservation)
    ));
    assert_eq!(before, client.mock().call_count());
    cancel.cancel();
    actor.await.unwrap();
}
