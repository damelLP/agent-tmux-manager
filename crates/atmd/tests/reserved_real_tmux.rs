//! Explicit private-server smoke test for the actual atmd actor/real tmux seam.
#![allow(clippy::unwrap_used)]
use atm_tmux::{PaneDirection, RealTmuxClient, TmuxClient};
use atmd::control::{reservation::*, service::spawn_control, SpawnContext};
use std::time::Duration;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

async fn rpc(
    socket: &std::path::Path,
    method: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let stream = tokio::net::UnixStream::connect(socket).await.unwrap();
    let mut stream = BufReader::new(stream);
    for message in [
        atm_protocol::ClientMessage::connect(Some("private-probe".into())),
        atm_protocol::ClientMessage::request("probe".into(), method.into(), params, None),
    ] {
        stream
            .get_mut()
            .write_all(format!("{}\n", serde_json::to_string(&message).unwrap()).as_bytes())
            .await
            .unwrap();
        let mut line = String::new();
        assert!(stream.read_line(&mut line).await.unwrap() > 0);
        let reply: serde_json::Value = serde_json::from_str(&line).unwrap();
        if reply["type"] == "response" {
            return reply;
        }
        assert_eq!(reply["type"], "connected");
    }
    panic!("missing RPC response")
}

fn reservation(kind: ReservationKind) -> ReservationRequest {
    ReservationRequest::new(
        "project/lead".parse().unwrap(),
        Some(PaneId::parse("%0").unwrap()),
        kind,
        WaitPolicy::Wait,
        Instant::now() + Duration::from_secs(60),
    )
}

/// Real shell Enter is acknowledged by tmux wait-for, not a redraw delay.
/// Real DaemonServer, OrgHandle and shared control actor; no model CLI runs.
#[tokio::test]
#[ignore = "requires explicit sealed environment and private tmux server"]
async fn reserved_real_rpc_refuses_clear_and_holds_delivery_until_ready() {
    use atm_org::OrgCommand;
    use std::{future::Future, task::Poll};
    use tokio::sync::oneshot;
    assert!(std::env::var_os("TMUX").is_none());
    assert!(std::env::var_os("TMUX_PANE").is_none());
    let root = std::env::var("TMUX_TMPDIR").unwrap();
    assert!(std::path::Path::new(&root).is_absolute());
    let socket = std::path::PathBuf::from(std::env::var("ATM_SOCKET").unwrap());
    assert!(socket.starts_with(std::path::Path::new(&root).parent().unwrap()));
    assert_ne!(socket, std::path::Path::new("/tmp/atm.sock"));
    let label = std::env::var("ATM_TMUX_SOCKET").unwrap();
    assert!(label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    let client = RealTmuxClient::with_socket(label.clone());
    client.raw_command("new-session", &["-d", "-s", "private-clear", "-x", "80", "-y", "24",
        &format!("sh -c 'read line; env -u TMUX -u TMUX_PANE tmux -L {label} wait-for -S clear-entered; exec cat'")]).await.unwrap();
    verify_private(&client, &root).await;
    let cancel = CancellationToken::new();
    let org_cancel = CancellationToken::new();
    let state = tempfile::tempdir().unwrap();
    let org = atmd::org::OrgHandle::open(state.path().into(), org_cancel.clone()).await;
    let registry = atmd::registry::spawn_registry();
    let (control, actor) = spawn_control(
        Box::new(client.clone()),
        registry.clone(),
        org.clone(),
        cancel.clone(),
    );
    let mut services = atmd::server::methods::MethodServices::new(registry);
    services.org = org.clone();
    services.control = Some(control.clone());
    let daemon = atmd::server::DaemonServer::with_services(&socket, services, cancel.clone());
    let mut server = Box::pin(async move { daemon.run().await });
    std::future::poll_fn(|cx| {
        assert!(server.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    let server = tokio::spawn(server);
    let work_client = client.clone();
    // Join catches assertion failures, so even a failed probe tears down only
    // its verified private server and daemon instead of abandoning live panes.
    let probe = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(90), async move {
            org.execute(
                OrgCommand::RegisterProject {
                    project_id: "project".parse().unwrap(),
                    root: "/private".into(),
                    move_root: false,
                },
                chrono::Utc::now(),
            )
            .await
            .unwrap();
            let o = org.clone();
            control
                .run_reserved(
                    reservation(ReservationKind::Adopt),
                    move |r| async move {
                        r.tag(&PaneId::parse("%0")?, ExpectedTag::Absent).await?;
                        o.execute(
                            OrgCommand::Adopt {
                                agent_id: r.agent().clone(),
                                pane: "%0".into(),
                                session_id: Some("old".into()),
                            },
                            chrono::Utc::now(),
                        )
                        .await
                        .unwrap();
                        Ok::<_, ReservationError>(())
                    },
                    |_, _| async { Ok(()) },
                )
                .await
                .unwrap();
            let (entered, entry) = oneshot::channel();
            let (editor, confirmed) = oneshot::channel();
            let c = control.clone();
            let o = org.clone();
            let clearing = tokio::spawn(async move {
                c.run_reserved(
                    reservation(ReservationKind::Clear),
                    move |r| async move {
                        let pane = PaneId::parse("%0")?;
                        o.execute(
                            OrgCommand::Clear {
                                agent_id: r.agent().clone(),
                            },
                            chrono::Utc::now(),
                        )
                        .await
                        .unwrap();
                        r.paste_guarded(
                            &pane,
                            r.agent().clone(),
                            "#clear-probe".into(),
                            CaptureFormat::Plain,
                            |_| true,
                        )
                        .await?;
                        r.keys_guarded(
                            &pane,
                            r.agent().clone(),
                            PaneKey::Enter,
                            CaptureFormat::Plain,
                            |_| true,
                        )
                        .await?;
                        work_client
                            .raw_command("wait-for", &["clear-entered"])
                            .await
                            .unwrap();
                        entered.send(r.clone()).unwrap();
                        confirmed.await.unwrap();
                        o.execute(
                            OrgCommand::EditorReady {
                                agent_id: r.agent().clone(),
                            },
                            chrono::Utc::now(),
                        )
                        .await
                        .unwrap();
                        Ok::<_, ReservationError>(())
                    },
                    |_, _| async { Ok(()) },
                )
                .await
            });
            let held = entry.await.unwrap(); // real shell consumed clear's Enter
            let (delivered, mut delivery_entered) = oneshot::channel();
            let c = control.clone();
            let mut delivery = Box::pin(async move {
                c.run_reserved(
                    reservation(ReservationKind::Delivery),
                    move |r| async move {
                        delivered.send(()).unwrap();
                        let pane = PaneId::parse("%0")?;
                        r.paste_guarded(
                            &pane,
                            r.agent().clone(),
                            "delivery".into(),
                            CaptureFormat::Plain,
                            |_| true,
                        )
                        .await?;
                        r.keys_guarded(
                            &pane,
                            r.agent().clone(),
                            PaneKey::Enter,
                            CaptureFormat::Plain,
                            |_| true,
                        )
                        .await
                    },
                    |_, _| async { Ok::<_, ReservationError>(()) },
                )
                .await
            });
            std::future::poll_fn(|cx| {
                assert!(delivery.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            let delivery = tokio::spawn(delivery);
            control
                .prepare_spawn(Some("%0".into()), Some("/private".into()), None, None)
                .await
                .unwrap();
            assert!(matches!(
                delivery_entered.try_recv(),
                Err(oneshot::error::TryRecvError::Empty)
            ));
            for (method, params) in [
                (
                    "session.send",
                    serde_json::json!({"target":"%00","text":"UNSAFE-RAW"}),
                ),
                (
                    "session.spawn_type",
                    serde_json::json!({"pane":"private-clear:0.0","command":"UNSAFE-TYPE"}),
                ),
            ] {
                let reply = rpc(&socket, method, params).await;
                assert_eq!(reply["error"]["code"], "refused");
                assert_eq!(
                    reply["error"]["message"],
                    "Refusing to send to %0: agent project/lead is clearing; try again in a moment"
                );
            }
            assert!(matches!(
                delivery_entered.try_recv(),
                Err(oneshot::error::TryRecvError::Empty)
            ));
            assert!(!held
                .capture(&PaneId::parse("%0").unwrap(), CaptureFormat::Plain)
                .await
                .unwrap()
                .text
                .contains("UNSAFE"));
            editor.send(()).unwrap();
            clearing.await.unwrap().unwrap();
            delivery_entered.await.unwrap();
            delivery.await.unwrap().unwrap();
            let reply = rpc(
                &socket,
                "session.send",
                serde_json::json!({"target":"%0","text":"after-ready"}),
            )
            .await;
            assert_eq!(reply["result"]["pane"], "%0");
            assert_eq!(
                org.snapshot()
                    .await
                    .unwrap()
                    .agent(&"project/lead".parse().unwrap())
                    .unwrap()
                    .status,
                atm_org::AgentStatus::Ready
            );
        })
        .await
        .unwrap();
    })
    .await;
    cancel.cancel();
    server.await.unwrap().unwrap();
    actor.await.unwrap();
    org_cancel.cancel();
    verify_private(&client, &root).await;
    client.raw_command("kill-server", &[]).await.unwrap();
    probe.unwrap();
}

#[tokio::test]
#[ignore = "requires explicit sealed environment and private tmux server"]
async fn reserved_real_input_refuses_mode_and_unbracketed_multiline() {
    assert!(std::env::var_os("TMUX").is_none());
    assert!(std::env::var_os("TMUX_PANE").is_none());
    let root = std::env::var("TMUX_TMPDIR").unwrap();
    assert!(std::path::Path::new(&root).is_absolute());
    assert_ne!(std::env::var("ATM_SOCKET").unwrap(), "/tmp/atm.sock");
    let client = RealTmuxClient::with_socket(std::env::var("ATM_TMUX_SOCKET").unwrap());
    client
        .raw_command("new-session", &["-d", "-s", "input-refusal", "cat"])
        .await
        .unwrap();
    verify_private(&client, &root).await;
    let cancel = CancellationToken::new();
    let (control, actor) = spawn_control(
        Box::new(client.clone()),
        atmd::registry::spawn_registry(),
        atmd::org::OrgHandle::disabled("input safety probe"),
        cancel.clone(),
    );
    control
        .run_reserved(
            reservation(ReservationKind::Adopt),
            |r| async move { r.tag(&PaneId::parse("%0")?, ExpectedTag::Absent).await },
            |_, _| async { Ok(()) },
        )
        .await
        .unwrap();
    let probe = client.clone();
    let result = control
        .run_reserved(
            reservation(ReservationKind::Delivery),
            move |r| async move {
                let pane = PaneId::parse("%0")?;
                // cat never enables bracketed paste. tmux <3.7 cannot report the flag;
                // its unknown/allow policy is covered separately, not silently changed.
                let fresh = r.inspect(&pane).await?.unwrap();
                if fresh.bracketed_paste == Some(false) {
                    for guarded in [false, true] {
                        let result = if guarded {
                            r.paste_guarded(
                                &pane,
                                r.agent().clone(),
                                "UNSAFE-FIRST\nSECOND".into(),
                                CaptureFormat::Plain,
                                |_| true,
                            )
                            .await
                        } else {
                            r.paste(&pane, r.agent().clone(), "UNSAFE-FIRST\nSECOND".into())
                                .await
                        };
                        assert!(matches!(
                            result,
                            Err(ReservationError::Control(
                                atmd::control::ControlError::SendRefused { .. }
                            ))
                        ));
                    }
                }
                probe
                    .raw_command("clock-mode", &["-t", "%0"])
                    .await
                    .unwrap();
                probe.raw_command("copy-mode", &["-t", "%0"]).await.unwrap();
                for operation in 0..4 {
                    let result = match operation {
                        0 => r.keys(&pane, r.agent().clone(), PaneKey::Enter).await,
                        1 => {
                            r.paste(&pane, r.agent().clone(), "UNSAFE-FIRST\nSECOND".into())
                                .await
                        }
                        2 => {
                            r.keys_guarded(
                                &pane,
                                r.agent().clone(),
                                PaneKey::Enter,
                                CaptureFormat::Ansi,
                                |_| true,
                            )
                            .await
                        }
                        _ => {
                            r.paste_guarded(
                                &pane,
                                r.agent().clone(),
                                "UNSAFE-FIRST\nSECOND".into(),
                                CaptureFormat::Ansi,
                                |_| true,
                            )
                            .await
                        }
                    };
                    assert!(matches!(
                        result,
                        Err(ReservationError::Control(
                            atmd::control::ControlError::SendRefused { .. }
                        ))
                    ));
                }
                assert!(!r
                    .capture(&pane, CaptureFormat::Plain)
                    .await?
                    .text
                    .contains("UNSAFE-FIRST"));
                Ok::<_, ReservationError>(())
            },
            |_, _| async { Ok(()) },
        )
        .await;
    cancel.cancel();
    actor.await.unwrap();
    verify_private(&client, &root).await;
    client.raw_command("kill-server", &[]).await.unwrap();
    result.unwrap();
}

async fn verify_private(client: &RealTmuxClient, root: &str) {
    let identity = client
        .raw_command("display-message", &["-p", "#{socket_path}\t#{pid}"])
        .await
        .unwrap();
    let (socket, pid) = identity.trim_end().split_once('\t').unwrap();
    assert!(std::path::Path::new(socket).starts_with(root));
    assert!(pid.parse::<u32>().unwrap() > 0);
}

#[tokio::test]
#[ignore = "requires explicit sealed environment and private tmux server"]
async fn reserved_real_tmux_effects_and_guard_rejection() {
    assert!(std::env::var_os("TMUX").is_none());
    assert!(std::env::var_os("TMUX_PANE").is_none());
    let root = std::env::var("TMUX_TMPDIR").unwrap();
    assert!(std::path::Path::new(&root).is_absolute());
    assert_ne!(std::env::var("ATM_SOCKET").unwrap(), "/tmp/atm.sock");
    let label = std::env::var("ATM_TMUX_SOCKET").unwrap();
    let client = RealTmuxClient::with_socket(label);
    client
        .raw_command(
            "new-session",
            &["-d", "-s", "private", "-x", "80", "-y", "24"],
        )
        .await
        .unwrap();
    verify_private(&client, &root).await;
    let cancel = CancellationToken::new();
    let org_cancel = CancellationToken::new();
    let state = tempfile::tempdir().unwrap();
    let org = atmd::org::OrgHandle::open(state.path().into(), org_cancel.clone()).await;
    let (control, actor) = spawn_control(
        Box::new(client.clone()),
        atmd::registry::spawn_registry(),
        org.clone(),
        cancel.clone(),
    );
    let verifier = client.clone();
    let private_root = root.clone();
    let result = control
        .run_reserved(
            ReservationRequest::new(
                "project/lead".parse().unwrap(),
                None,
                ReservationKind::Respawn,
                WaitPolicy::Wait,
                Instant::now() + Duration::from_secs(60),
            ),
            move |r| async move {
                let placement = PaneId::parse("%0")?;
                assert!(r.inspect(&placement).await?.is_some());
                let pane = r
                    .split(
                        SpawnContext::new("%0".into(), None, false),
                        PaneDirection::Below,
                        "50%".into(),
                    )
                    .await?;
                r.tag(&pane, ExpectedTag::Absent).await?;
                assert_eq!(
                    r.inspect(&pane).await?.unwrap().local_agent_tag.as_deref(),
                    Some("project/lead")
                );
                for format in [CaptureFormat::Plain, CaptureFormat::Ansi] {
                    let capture = r.capture(&pane, format).await?;
                    assert_eq!(
                        capture.text.split_terminator('\n').count(),
                        usize::from(capture.inspection.pane.height)
                    );
                }
                assert!(matches!(
                    r.keys_guarded(
                        &pane,
                        r.agent().clone(),
                        PaneKey::Enter,
                        CaptureFormat::Plain,
                        |_| false
                    )
                    .await,
                    Err(ReservationError::ScreenChanged { .. })
                ));
                r.paste_guarded(
                    &pane,
                    r.agent().clone(),
                    "# harmless reserved probe;".into(),
                    CaptureFormat::Plain,
                    |_| true,
                )
                .await?;
                r.keys_guarded(
                    &pane,
                    r.agent().clone(),
                    PaneKey::Enter,
                    CaptureFormat::Plain,
                    |_| true,
                )
                .await?;
                r.untag(&pane, r.agent().clone()).await?;
                assert_eq!(r.inspect(&pane).await?.unwrap().local_agent_tag, None);
                r.tag(&pane, ExpectedTag::Absent).await?;
                verify_private(&verifier, &private_root).await;
                r.kill(
                    &pane,
                    r.agent().clone(),
                    serde_json::json!({"probe":true}),
                    chrono::Utc::now(),
                )
                .await?;
                assert!(r.inspect(&pane).await?.is_none());
                Ok::<_, ReservationError>(())
            },
            |_, _| async { Ok(()) },
        )
        .await;
    cancel.cancel();
    actor.await.unwrap();
    verify_private(&client, &root).await;
    client.raw_command("kill-server", &[]).await.unwrap();
    result.unwrap();
    let records = org.tail(atmd::org::TailParams::default()).await.unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].kind, "control.killed");
    assert_eq!(records[0].body["action"]["probe"], true);
    org_cancel.cancel();
}
