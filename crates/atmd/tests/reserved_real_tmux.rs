//! Explicit private-server smoke test for the actual atmd actor/real tmux seam.
#![allow(clippy::unwrap_used)]
use atm_tmux::{PaneDirection, RealTmuxClient, TmuxClient};
use atmd::control::{reservation::*, service::spawn_control, SpawnContext};
use std::time::Duration;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

#[allow(dead_code, clippy::expect_used)]
#[path = "../../../tests/support/stand_in/process.rs"]
mod process;

fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// Observe bytes without attaching a client or changing focus/geometry. The
/// reader's same-server signal runs after tmux's PTY callback parsed the bytes.
async fn output_event(client: &RealTmuxClient, pane: &str, marker: &str, channel: &str) {
    let socket = client
        .raw_command("display-message", &["-p", "#{socket_path}"])
        .await
        .unwrap();
    let program = format!(
        "import os, subprocess\ndata=b''\nwhile {marker:?}.encode() not in data:\n chunk=os.read(0,65536)\n if not chunk: raise EOFError('pane output')\n data+=chunk\nsubprocess.run(['tmux','-S',{:?},'wait-for','-S',{channel:?}],check=True,timeout=30)\n", socket.trim()
    );
    client
        .raw_command(
            "pipe-pane",
            &["-O", "-t", pane, &format!("python3 -c {}", quote(&program))],
        )
        .await
        .unwrap();
}

async fn wait_event(client: &RealTmuxClient, channel: &str) {
    let socket = client
        .raw_command("display-message", &["-p", "#{socket_path}"])
        .await
        .unwrap();
    let channel = channel.to_owned();
    let output = tokio::task::spawn_blocking(move || {
        process::run(std::process::Command::new("tmux").args([
            "-S",
            socket.trim(),
            "wait-for",
            &channel,
        ]))
    })
    .await
    .unwrap();
    assert!(output.status.success(), "{output:?}");
}

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

/// Real receiver Enter is acknowledged by tmux wait-for, not a redraw delay.
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
    let label = format!("{}-rpc", std::env::var("ATM_TMUX_SOCKET").unwrap());
    assert!(label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    let client = RealTmuxClient::with_socket(label.clone());
    // Like the literal-key fixture below, run a program as the pane root:
    // a Ready/bound agent backed by an idle shell is correctly reconciled Down.
    let receiver = r#"import os, subprocess, sys
tmux = ['tmux', '-S', os.environ['TMUX'].rsplit(',', 2)[0]]
def signal(channel):
    subprocess.run(tmux + ['wait-for', '-S', channel], check=True, timeout=30)
signal('fixture-ready')
if not sys.stdin.readline():
    raise EOFError('clear Enter')
signal('clear-entered')
while data := os.read(0, 4096):
    os.write(1, data)
"#;
    client
        .raw_command(
            "new-session",
            &[
                "-d",
                "-s",
                "private-clear",
                "-x",
                "80",
                "-y",
                "24",
                "python3",
                "-c",
                receiver,
            ],
        )
        .await
        .unwrap();
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
            // The event comes from Python after exec, never the launching shell.
            wait_event(&work_client, "fixture-ready").await;
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
            let _ = entry.await.unwrap(); // real receiver consumed clear's Enter
            let c = control.clone();
            let o = org.clone();
            let mut delivery = Box::pin(async move {
                c.run_reserved(
                    reservation(ReservationKind::Delivery),
                    move |r| async move {
                        assert_eq!(
                            o.snapshot().await.unwrap().agent(r.agent()).unwrap().status,
                            atm_org::AgentStatus::Ready,
                            "delivery must enter only after durable EditorReady"
                        );
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
            editor.send(()).unwrap();
            clearing.await.unwrap().unwrap();
            delivery.await.unwrap().unwrap();
            let reply = rpc(
                &socket,
                "session.send",
                serde_json::json!({"target":"%0","text":"after-ready"}),
            )
            .await;
            assert_eq!(reply["result"]["pane"], "%0");
            // The pty echoes input asynchronously. Observe until the later
            // sentinel is visible before asserting absence of earlier input.
            // Observe's actor pacing bounds the polls; the outer timeout is
            // solely a hang guard, not a delay used as evidence.
            loop {
                let frame = control
                    .observe(&PaneId::parse("%0").unwrap(), CaptureFormat::Plain)
                    .await
                    .unwrap();
                if frame
                    .text
                    .lines()
                    .any(|line| line.trim_end() == "after-ready")
                {
                    assert!(!frame.text.contains("UNSAFE"));
                    break;
                }
            }
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
    let client = RealTmuxClient::with_socket(format!(
        "{}-input",
        std::env::var("ATM_TMUX_SOCKET").unwrap()
    ));
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
    let legacy_client = client.clone();
    let legacy_control = control.clone();
    let legacy = tokio::spawn(async move {
        let sibling = legacy_client
            .raw_command(
                "split-window",
                &["-d", "-P", "-F", "#{pane_id}", "-t", "%0", "cat"],
            )
            .await
            .unwrap();
        for case in 0..3 {
            if case == 0 {
                legacy_client
                    .raw_command(
                        "set-window-option",
                        &["-t", "%0", "synchronize-panes", "on"],
                    )
                    .await
                    .unwrap();
            } else if case == 1 {
                legacy_client
                    .raw_command("select-pane", &["-t", "%0", "-d"])
                    .await
                    .unwrap();
            }
            let text = if case == 2 {
                "UNSAFE-LEGACY\u{1b}[201~"
            } else {
                "UNSAFE-LEGACY"
            };
            let result = legacy_control.send("%0".into(), text.into()).await;
            assert!(
                matches!(
                    result,
                    Err(atmd::control::service::ServiceError::Control(
                        atmd::control::ControlError::SendRefused { .. }
                    ))
                ),
                "{result:?}"
            );
            if case == 0 {
                legacy_client
                    .raw_command(
                        "set-window-option",
                        &["-t", "%0", "synchronize-panes", "off"],
                    )
                    .await
                    .unwrap();
            } else if case == 1 {
                legacy_client
                    .raw_command("select-pane", &["-t", "%0", "-e"])
                    .await
                    .unwrap();
            }
        }
        for target in ["%0", sibling.trim()] {
            assert!(!legacy_client
                .raw_command("capture-pane", &["-p", "-t", target])
                .await
                .unwrap()
                .contains("UNSAFE-LEGACY"));
        }
    })
    .await;
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
                            Err(ReservationError::PaneInputRefused { .. })
                        ));
                    }
                }
                let sibling = probe
                    .raw_command(
                        "split-window",
                        &["-d", "-P", "-F", "#{pane_id}", "-t", "%0", "cat"],
                    )
                    .await
                    .unwrap();
                for synchronized in [true, false] {
                    if synchronized {
                        probe
                            .raw_command(
                                "set-window-option",
                                &["-t", "%0", "synchronize-panes", "on"],
                            )
                            .await
                            .unwrap();
                    } else {
                        probe
                            .raw_command("select-pane", &["-t", "%0", "-d"])
                            .await
                            .unwrap();
                    }
                    for paste in [true, false] {
                        let result = if paste {
                            r.paste_guarded(
                                &pane,
                                r.agent().clone(),
                                "UNSAFE-STATE".into(),
                                CaptureFormat::Plain,
                                |_| true,
                            )
                            .await
                        } else {
                            r.keys_guarded(
                                &pane,
                                r.agent().clone(),
                                PaneKey::Enter,
                                CaptureFormat::Plain,
                                |_| true,
                            )
                            .await
                        };
                        assert!(
                            matches!(result, Err(ReservationError::PaneInputRefused { .. })),
                            "{result:?}"
                        );
                    }
                    if synchronized {
                        probe
                            .raw_command(
                                "set-window-option",
                                &["-t", "%0", "synchronize-panes", "off"],
                            )
                            .await
                            .unwrap();
                    } else {
                        probe
                            .raw_command("select-pane", &["-t", "%0", "-e"])
                            .await
                            .unwrap();
                    }
                }
                for target in ["%0", sibling.trim()] {
                    let text = probe
                        .raw_command("capture-pane", &["-p", "-t", target])
                        .await
                        .unwrap();
                    assert!(!text.contains("UNSAFE-STATE"));
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
                        Err(ReservationError::PaneInputRefused { .. })
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
    let names = tokio::spawn(check_control_byte_process_names(
        client.clone(),
        control.clone(),
    ))
    .await;
    cancel.cancel();
    actor.await.unwrap();
    verify_private(&client, &root).await;
    client.raw_command("kill-server", &[]).await.unwrap();
    result.unwrap();
    names.unwrap();
    legacy.unwrap();
}

async fn check_control_byte_process_names(
    client: RealTmuxClient,
    control: atmd::control::service::ControlHandle,
) {
    use tokio::io::AsyncReadExt;
    for name in ["A\nB", "tab\there", "esc\u{1b}[31mred"] {
        let fixture = tempfile::tempdir_in(std::env::var("TMUX_TMPDIR").unwrap()).unwrap();
        let source = fixture.path().join("exec.py");
        let socket = fixture.path().join("exec.sock");
        std::fs::write(&source, "import socket, sys\ns=socket.socket(socket.AF_UNIX)\ns.connect(sys.argv[1])\ns.sendall(b'R')\ns.recv(1)\n").unwrap();
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        // Python acknowledges from inside the exec'd process; argv0 retains
        // the same literal control bytes as the former sleeping fixture.
        let command = format!(
            "exec /bin/bash -c {}",
            quote(&format!(
                "exec -a {} /usr/bin/python3 {} {}",
                quote(name),
                quote(source.to_str().unwrap()),
                quote(socket.to_str().unwrap())
            ))
        );
        let created = client
            .raw_command("new-window", &["-d", "-P", "-F", "#{pane_id}", &command])
            .await
            .unwrap();
        let pane = PaneId::parse(created.trim()).unwrap();
        let pid = client
            .raw_command(
                "display-message",
                &["-p", "-t", pane.as_str(), "#{pane_pid}"],
            )
            .await
            .unwrap();
        // Keep the socket until inspection/kill: EOF would end the fixture.
        let (_ready, args) = tokio::time::timeout(Duration::from_secs(30), async {
            let (mut ready, _) = listener.accept().await.unwrap();
            assert_eq!(ready.read_u8().await.unwrap(), b'R');
            let args = tokio::fs::read(format!("/proc/{}/cmdline", pid.trim()))
                .await
                .unwrap();
            (ready, args)
        })
        .await
        .unwrap_or_else(|_| {
            panic!("after-exec fixture readiness/identity exceeded the 30 s hang guard")
        });
        assert_eq!(args.split(|byte| *byte == 0).next(), Some(name.as_bytes()));
        client
            .raw_command(
                "set-option",
                &["-p", "-t", pane.as_str(), "@atm-agent", "project/lead"],
            )
            .await
            .unwrap();
        let mut request = reservation(ReservationKind::Down);
        request.pane = Some(pane.clone());
        control
            .run_reserved(
                request,
                move |r| async move {
                    assert_eq!(r.inspect(&pane).await?.unwrap().foreground_command, None);
                    assert_eq!(
                        r.capture(&pane, CaptureFormat::Plain)
                            .await?
                            .inspection
                            .foreground_command,
                        None
                    );
                    r.kill(
                        &pane,
                        r.agent().clone(),
                        serde_json::json!({"test":"control-byte-name"}),
                        chrono::Utc::now(),
                    )
                    .await?;
                    assert!(r.inspect(&pane).await?.is_none());
                    Ok::<_, ReservationError>(())
                },
                |_, _| async { Ok(()) },
            )
            .await
            .unwrap();
    }
}

/// Captured: tmux 3.8, 2026-11-14, sealed raw-byte fixture (no harness CLI).
/// The socket acknowledges receiver readiness and every byte; no redraw sleeps.
#[tokio::test]
#[ignore = "requires explicit sealed environment, private tmux and python3"]
async fn reserved_real_literal_key_delivers_exactly_one_byte() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    assert!(std::env::var_os("TMUX").is_none());
    assert!(std::env::var_os("TMUX_PANE").is_none());
    let root = std::env::var("TMUX_TMPDIR").unwrap();
    assert!(std::path::Path::new(&root).is_absolute());
    let daemon_socket = std::path::PathBuf::from(std::env::var("ATM_SOCKET").unwrap());
    assert!(daemon_socket.starts_with(std::path::Path::new(&root).parent().unwrap()));
    assert_ne!(daemon_socket, std::path::Path::new("/tmp/atm.sock"));
    let label = format!("{}-literal", std::env::var("ATM_TMUX_SOCKET").unwrap());
    assert!(label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    let fixture = tempfile::tempdir_in(&root).unwrap();
    let source = fixture.path().join("receiver.py");
    let socket = fixture.path().join("bytes.sock");
    tokio::fs::write(
        &source,
        r#"import os, select, socket, sys, tty
s = socket.socket(socket.AF_UNIX)
s.connect(sys.argv[1])
tty.setraw(0)
s.sendall(b'T')
assert s.recv(1) == b'G'
os.write(1, b'literal fixture ready\r\n')
s.sendall(b'R')
while True:
    readable, _, _ = select.select([s, 0], [], [])
    if 0 in readable:
        s.sendall(os.read(0, 4096))
    if s in readable:
        s.recv(1)
        while select.select([0], [], [], 0)[0]:
            s.sendall(os.read(0, 4096))
        break
s.shutdown(socket.SHUT_WR)
# Keep the pane/server alive until the test's verified cleanup.
s.recv(1)
"#,
    )
    .await
    .unwrap();
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let client = RealTmuxClient::with_socket(label);
    client
        .raw_command(
            "new-session",
            &[
                "-d",
                "-s",
                "literal",
                "-x",
                "80",
                "-y",
                "24",
                "python3",
                source.to_str().unwrap(),
                socket.to_str().unwrap(),
            ],
        )
        .await
        .unwrap();
    verify_private(&client, &root).await;
    let cancel = CancellationToken::new();
    let registry = atmd::registry::spawn_registry();
    let (control, actor) = spawn_control(
        Box::new(client.clone()),
        registry.clone(),
        atmd::org::OrgHandle::disabled("literal smoke"),
        cancel.clone(),
    );
    // Exercise the real control actor and RealTmuxClient on private tmux;
    // calls use ControlHandle directly, so nothing travels over the daemon socket.
    let mut services = atmd::server::methods::MethodServices::new(registry);
    services.control = Some(control.clone());
    let daemon =
        atmd::server::DaemonServer::with_services(&daemon_socket, services, cancel.clone());
    let server = tokio::spawn(async move { daemon.run().await });
    let output_client = client.clone();
    let probe = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(90), async move {
            let (mut receiver, _) = listener.accept().await.unwrap();
            assert_eq!(receiver.read_u8().await.unwrap(), b'T');
            let held = control
                .observe(&PaneId::parse("%0").unwrap(), CaptureFormat::Plain)
                .await
                .unwrap();
            assert!(
                !held.text.contains("literal fixture ready"),
                "PTY output escaped its fixture gate"
            );
            output_event(
                &output_client,
                "%0",
                "literal fixture ready",
                "literal-parsed",
            )
            .await;
            receiver.write_all(b"G").await.unwrap();
            assert_eq!(receiver.read_u8().await.unwrap(), b'R');
            wait_event(&output_client, "literal-parsed").await;
            control
                .run_reserved(
                    reservation(ReservationKind::Adopt),
                    |r| async move { r.tag(&PaneId::parse("%0")?, ExpectedTag::Absent).await },
                    |_, _| async { Ok(()) },
                )
                .await
                .unwrap();
            control
                .run_reserved(
                    reservation(ReservationKind::Up),
                    move |r| async move {
                        let pane = PaneId::parse("%0")?;
                        let baseline = r.capture(&pane, CaptureFormat::Plain).await?;
                        assert!(baseline.text.contains("literal fixture ready"));
                        assert_eq!(baseline.focused, Some(false));
                        let pid = baseline.inspection.pane_root_pid();
                        let command = baseline.inspection.foreground_command.clone();
                        assert!(pid.is_some());
                        assert!(command
                            .as_deref()
                            .is_some_and(|name| name.starts_with("python")));
                        let refused = r
                            .literal_key_guarded(
                                &pane,
                                r.agent().clone(),
                                't',
                                CaptureFormat::Plain,
                                |_| false,
                            )
                            .await;
                        assert!(matches!(
                            refused,
                            Err(ReservationError::ScreenChanged { .. })
                        ));
                        // Include t first for D5, then every allowed ASCII character to catch
                        // tmux option/separator interpretation (especially '-' and ';').
                        for byte in std::iter::once(b't').chain(b' '..=b'~') {
                            let command = command.clone();
                            r.literal_key_guarded(
                                &pane,
                                r.agent().clone(),
                                char::from(byte),
                                CaptureFormat::Plain,
                                move |fresh| {
                                    fresh.focused == Some(false)
                                        && fresh.inspection.pane_root_pid() == pid
                                        && fresh.inspection.foreground_command == command
                                        && fresh.text.contains("literal fixture ready")
                                },
                            )
                            .await?;
                            assert_eq!(receiver.read_u8().await.unwrap(), byte);
                        }
                        receiver.write_all(b"S").await.unwrap();
                        let mut extra = Vec::new();
                        receiver.read_to_end(&mut extra).await.unwrap();
                        assert!(extra.is_empty(), "unexpected extra input: {extra:?}");
                        // Hold the socket until after verification so the fixture does not exit.
                        Ok::<_, ReservationError>(receiver)
                    },
                    |_, _| async { Ok(()) },
                )
                .await
                .unwrap()
        })
        .await
        .unwrap()
    })
    .await;
    // Join catches test panics/timeouts; always stop only this verified server.
    cancel.cancel();
    actor.await.unwrap();
    server.await.unwrap().unwrap();
    verify_private(&client, &root).await;
    client.raw_command("kill-server", &[]).await.unwrap();
    drop(probe.unwrap());
}

/// Fresh single-process frames and actual input, synchronized by tmux events.
#[tokio::test]
#[ignore = "requires explicit sealed environment and private tmux server"]
async fn reserved_real_compound_capture_and_guarded_input() {
    assert!(std::env::var_os("TMUX").is_none());
    assert!(std::env::var_os("TMUX_PANE").is_none());
    let root = std::env::var("TMUX_TMPDIR").unwrap();
    let label = format!("{}-compound", std::env::var("ATM_TMUX_SOCKET").unwrap());
    assert!(label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    let client = RealTmuxClient::with_socket(label.clone());
    let command = format!(
        "printf '\\033[31mprobe  \\033[39m   \\nready     \\n'; \
         env -u TMUX -u TMUX_PANE tmux -L {label} wait-for -S screen-ready; \
         read -r line; printf 'ACK:%s\\n' \"$line\"; \
         env -u TMUX -u TMUX_PANE tmux -L {label} wait-for -S input-seen; exec /bin/cat"
    );
    client
        .raw_command(
            "new-session",
            &["-d", "-s", "alpha", "-x", "80", "-y", "8", &command],
        )
        .await
        .unwrap();
    verify_private(&client, &root).await;
    let identity = client
        .raw_command("display-message", &["-p", "#{socket_path}\t#{pid}"])
        .await
        .unwrap();
    let cancel = CancellationToken::new();
    let (control, actor) = spawn_control(
        Box::new(client.clone()),
        atmd::registry::spawn_registry(),
        atmd::org::OrgHandle::disabled("compound real probe"),
        cancel.clone(),
    );
    let c = client.clone();
    let result = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(90), async move {
            c.raw_command("wait-for", &["screen-ready"]).await.unwrap();
            c.raw_command("set-option", &["-g", "@atm-agent", "beta/inherited"])
                .await
                .unwrap();
            let pane = PaneId::parse("%0").unwrap();
            let inherited = control.observe(&pane, CaptureFormat::Plain).await.unwrap();
            assert_eq!(inherited.inspection.local_agent_tag, None);
            assert_eq!(inherited.focused, Some(false));
            c.raw_command(
                "set-option",
                &["-p", "-t", "%0", "@atm-agent", "alpha/seat0"],
            )
            .await
            .unwrap();
            c.raw_command("new-session", &["-d", "-s", "beta", "-t", "alpha"])
                .await
                .unwrap();
            for format in [CaptureFormat::Plain, CaptureFormat::Ansi] {
                let capture = control.observe(&pane, format).await.unwrap();
                assert_eq!(
                    capture.inspection.local_agent_tag.as_deref(),
                    Some("alpha/seat0")
                );
                assert_eq!(capture.focused, Some(false));
                assert!(capture.text.contains("ready"));
                assert_eq!(
                    capture.text.split_terminator('\n').count(),
                    usize::from(capture.inspection.pane.height)
                );
                if format == CaptureFormat::Ansi {
                    assert!(capture.text.contains("\x1b[31mprobe"));
                }
            }
            control
                .run_reserved(
                    ReservationRequest::new(
                        "alpha/seat0".parse().unwrap(),
                        Some(pane.clone()),
                        ReservationKind::Delivery,
                        WaitPolicy::Wait,
                        Instant::now() + Duration::from_secs(60),
                    ),
                    move |r| async move {
                        r.paste_guarded(
                            &pane,
                            r.agent().clone(),
                            "probe-accepted".into(),
                            CaptureFormat::Plain,
                            |fresh| fresh.focused == Some(false) && fresh.text.contains("ready"),
                        )
                        .await?;
                        r.keys_guarded(
                            &pane,
                            r.agent().clone(),
                            PaneKey::Enter,
                            CaptureFormat::Plain,
                            |fresh| {
                                fresh.focused == Some(false)
                                    && fresh.text.contains("probe-accepted")
                            },
                        )
                        .await
                    },
                    |_, _| async { Ok(()) },
                )
                .await
                .unwrap();
            c.raw_command("wait-for", &["input-seen"]).await.unwrap();
            let after = control
                .observe(&PaneId::parse("%0").unwrap(), CaptureFormat::Plain)
                .await
                .unwrap();
            assert!(after.text.contains("ACK:probe-accepted"));
            assert!(matches!(
                control
                    .observe(&PaneId::parse("%987654").unwrap(), CaptureFormat::Plain)
                    .await,
                Err(ReservationError::PaneGone { .. })
            ));
        })
        .await
        .unwrap();
    })
    .await;
    cancel.cancel();
    actor.await.unwrap();
    verify_private(&client, &root).await;
    assert_eq!(
        client
            .raw_command("display-message", &["-p", "#{socket_path}\t#{pid}"])
            .await
            .unwrap(),
        identity
    );
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
    let label = format!("{}-effects", std::env::var("ATM_TMUX_SOCKET").unwrap());
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
    client
        .raw_command("new-session", &["-d", "-s", "linked", "-t", "private"])
        .await
        .unwrap();
    let socket = client
        .raw_command("display-message", &["-p", "#{socket_path}"])
        .await
        .unwrap();
    let (notifications, output) = std::os::unix::net::UnixStream::pair().unwrap();
    notifications.set_nonblocking(true).unwrap();
    let mut notifications =
        tokio::io::BufReader::new(tokio::net::UnixStream::from_std(notifications).unwrap());
    let mut attached = std::process::Command::new("tmux")
        .args([
            "-S",
            socket.trim(),
            "-C",
            "attach-session",
            "-f",
            "no-output",
            "-t",
            "private",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::from(std::os::fd::OwnedFd::from(
            output,
        )))
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let input = attached.stdin.take();
    let attached_pid = attached.id();
    let attached = process::OwnedChild::new(attached);
    let ready = tokio::time::timeout(Duration::from_secs(30), async {
        use tokio::io::AsyncBufReadExt;
        loop {
            let mut line = String::new();
            if notifications.read_line(&mut line).await? == 0 {
                return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof));
            }
            if line.starts_with("%session-changed ") {
                return Ok(());
            }
        }
    })
    .await;
    if !matches!(ready, Ok(Ok(()))) {
        cancel.cancel();
        let stopped = tokio::time::timeout(Duration::from_secs(30), actor).await;
        org_cancel.cancel();
        verify_private(&client, &root).await;
        client.raw_command("kill-server", &[]).await.unwrap();
        drop(input);
        drop(attached); // kill + reap the owned client before reporting failure
        panic!("attachment progress failed: {ready:?}; control actor stop: {stopped:?}");
    }
    assert!(!client
        .raw_command("list-clients", &["-F", "#{client_flags}"])
        .await
        .unwrap()
        .is_empty());
    let baseline = control
        .observe(&PaneId::parse("%0").unwrap(), CaptureFormat::Plain)
        .await
        .unwrap();
    assert!(baseline.inspection.pane.is_active);
    assert_eq!(baseline.focused, Some(true));
    let observer = control.clone();
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
                        SpawnContext::new("%0".into(), None, false).with_detached(true),
                        PaneDirection::Below,
                        "50%".into(),
                    )
                    .await?;
                let after = observer.observe(&placement, CaptureFormat::Plain).await?;
                assert!(
                    after.inspection.pane.is_active,
                    "detached split moved keyboard selection"
                );
                assert_eq!(after.focused, baseline.focused);
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
                    assert_eq!(capture.focused, Some(false));
                    assert!(capture.inspection.pane_root_pid().is_some());
                    assert_eq!(capture.inspection.foreground_pid, None);
                    assert!(capture.inspection.foreground_command.is_some());
                    let observed = observer.observe(&pane, format).await?;
                    assert_eq!(observed.focused, Some(false));
                    assert_eq!(
                        observed.inspection.pane_root_pid(),
                        capture.inspection.pane_root_pid()
                    );
                    assert_eq!(
                        observed.inspection.local_agent_tag.as_deref(),
                        Some("project/lead")
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
    drop(input);
    tokio::task::spawn_blocking(move || process::wait_exit(attached_pid, Duration::from_secs(30)))
        .await
        .unwrap()
        .unwrap();
    attached.wait_with_output().unwrap();
    result.unwrap();
    let records = org.tail(atmd::org::TailParams::default()).await.unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].kind, "control.killed");
    assert_eq!(records[0].body["action"]["probe"], true);
    org_cancel.cancel();
}
