//! New CLI paths on a real private daemon/tmux stand-in, and a real CLI
//! disconnect against a deliberately stalled pane read. No model harness runs.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use stand_in::control_support;
#[allow(dead_code)]
#[path = "support/message_delivery.rs"]
mod delivery_support;
#[allow(dead_code)]
#[path = "support/agent_lifecycle.rs"]
mod lifecycle;
#[path = "support/stand_in.rs"]
mod stand_in;
// Reuse the component tests' controlled external-I/O boundary, not a second rig.
#[allow(dead_code)]
#[path = "../crates/atmd/src/control/dialog_request_test_client.rs"]
mod paused;

use serde_json::{json, Value};
use stand_in::{StandInConfig, StandInMode, StandInRig};
use std::{path::PathBuf, sync::Arc, time::Duration};

fn checked(output: std::process::Output) -> Vec<u8> {
    assert!(output.status.success(), "{output:?}");
    output.stdout
}
fn tail(rig: &StandInRig) -> Vec<Value> {
    let response = stand_in::protocol_request(
        &rig.socket(),
        atm_protocol::ClientMessage::request(
            "audit".into(),
            "log.tail".into(),
            json!({"limit":1000}),
            None,
        ),
        Duration::from_secs(30),
    )
    .unwrap();
    serde_json::to_value(response).unwrap()["result"]
        .as_array()
        .unwrap()
        .clone()
}

#[test]
fn reply_keys_and_peek_bytes_go_through_a_real_daemon_and_tmux() {
    let mut rig = StandInRig::start();
    let mut watch = delivery_support::Watch::on(&rig, 0);
    for (flags, line, option) in [
        (vec![], "Sent Enter", Some(1)),
        (vec!["-y"], "Sent Enter", Some(1)),
        (vec!["-n"], "Sent Escape", None),
        (vec!["-o", "2"], "Selected option 2", Some(2)),
    ] {
        let mut config = StandInConfig::new(StandInMode::Claude);
        config.manual_clock = true;
        let pane = rig.launch(&config);
        let before = rig.event_cursor(&pane);
        rig.paste(&pane, "/atm/dialog");
        rig.wait_event(&pane, before, |e| {
            e["kind"] == "rendered" && e["state"] == "dialog" && e["draft"] == ""
        });
        // The legacy capture is the independent external reference, not the
        // daemon's observer representation (which would retain -N padding).
        // RealTmuxClient has always removed terminal-padding blank rows after
        // capture. Preserve every other byte, including interior blank rows.
        let expected = format!("{}\n", rig.capture_plain(&pane).trim_end_matches('\n'));
        assert_eq!(
            String::from_utf8(checked(rig.atm(&["peek", &pane.pane_id]))).unwrap(),
            expected
        );
        assert_eq!(checked(rig.atm(&["peek", &pane.pane_id, "-n", "0"])), b"");
        let prompt = String::from_utf8(checked(rig.atm(&[
            "peek",
            &pane.pane_id,
            "--prompt",
            "-n",
            "0",
        ])))
        .unwrap();
        for visible in [
            "Do you want to proceed?",
            "1. Yes",
            "2. No",
            "Esc to cancel",
        ] {
            assert!(prompt.contains(visible), "{prompt}");
        }
        let before = rig.event_cursor(&pane);
        let mut args = vec!["reply", &pane.pane_id];
        args.extend(flags);
        let expected = if option == Some(2) {
            format!("{line} on {}\n", pane.pane_id)
        } else {
            format!("{line} to {}\n", pane.pane_id)
        };
        assert_eq!(checked(rig.atm(&args)), expected.as_bytes());
        if let Some(option) = option {
            rig.wait_event(&pane, before, |e| {
                e["kind"] == "selection" && e["option"] == option
            });
        }
        rig.wait_event(&pane, before, |e| {
            e["kind"] == "rendered" && e["state"] != "dialog"
        });
    }
    for _ in 0..4 {
        watch.wait("a reply audit", |e| {
            e["body"]["action"]["method"] == "session.reply"
        });
    }
    let records = tail(&rig);
    assert_eq!(
        records
            .iter()
            .filter(|e| e["body"]["action"]["method"] == "session.reply")
            .count(),
        4
    );
    let controls: Vec<_> = records
        .iter()
        .filter(|e| {
            e["kind"]
                .as_str()
                .is_some_and(|k| k.starts_with("control."))
        })
        .collect();
    assert_eq!(
        controls.len(),
        4,
        "frequent read-only peek must not add events"
    );
    for event in controls {
        assert_eq!(event["kind"], "control.sent");
        assert_eq!(event["body"]["action"]["method"], "session.reply");
        assert_eq!(event["body"]["action"]["outcome"], "ok");
        assert!(event["body"].get("lines").is_none());
    }
    rig.close();
}

/// Identify the accepted RPC and its monitor duplicate by peer credentials,
/// not a process-wide fd diff that sibling integration tests could contaminate.
fn peer_fds(pid: u32) -> Vec<(PathBuf, PathBuf)> {
    std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            let link = std::fs::read_link(&path).ok()?;
            if !link.to_string_lossy().starts_with("socket:") {
                return None;
            }
            let fd: i32 = path.file_name()?.to_str()?.parse().ok()?;
            let mut peer: libc::ucred = unsafe { std::mem::zeroed() };
            let mut size = std::mem::size_of_val(&peer) as libc::socklen_t;
            let rc = unsafe {
                libc::getsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_PEERCRED,
                    std::ptr::from_mut(&mut peer).cast(),
                    &mut size,
                )
            };
            (rc == 0 && peer.pid == pid as i32).then_some((path, link))
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sigint_of_the_real_reply_cli_discards_pre_key_input() {
    use atm_tmux::{mock::MockCall, MockTmuxClient};
    use atmd::{
        control::service::spawn_control,
        org::OrgHandle,
        server::{methods::MethodServices, DaemonServer},
    };
    use tokio_util::sync::CancellationToken;
    let dir = tempfile::tempdir().unwrap();
    let socket = dir
        .path()
        .join(format!("tmux-{}/default", unsafe { libc::getuid() }));
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    std::fs::write(&socket, "").unwrap();
    std::fs::create_dir(dir.path().join("path")).unwrap();
    let mock = MockTmuxClient::new();
    mock.set_next_raw_output(socket.to_str().unwrap());
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let cancel = CancellationToken::new();
    let org_cancel = CancellationToken::new();
    let org = OrgHandle::open(dir.path().join("org"), org_cancel.clone()).await;
    let registry = atmd::registry::spawn_registry();
    let (control, actor) = spawn_control(
        Box::new(paused::Paused {
            inner: Box::new(mock.clone()),
            at: paused::At::Capture,
            entered: entered.clone(),
            release: release.clone(),
        }),
        registry.clone(),
        org.clone(),
        cancel.clone(),
    );
    let mut services = MethodServices::new(registry);
    services.control = Some(control.clone());
    services.org = org.clone();
    services.state_dir = dir.path().into();
    let server =
        DaemonServer::with_services(dir.path().join("atmd.sock"), services, cancel.clone());
    let server = control_support::start_server(server, &dir.path().join("atmd.sock")).await;
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_atm"));
    command
        .args(["reply", "%1", "-o", "2"])
        .env_clear()
        .env("PATH", dir.path().join("path"))
        .env("HOME", dir.path())
        .env("ATM_SOCKET", dir.path().join("atmd.sock"))
        .env("ATM_STATE_DIR", dir.path().join("state"))
        .env("TMUX_TMPDIR", dir.path())
        .env("XDG_STATE_HOME", dir.path().join("xdg"))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    // Background runners can inherit SIGINT ignored (observed in /proc).
    // Model an interactive CLI: reset only this owned child's disposition,
    // using async-signal-safe calls between fork and exec.
    unsafe {
        command.pre_exec(|| {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = libc::SIG_DFL;
            libc::sigemptyset(&mut action.sa_mask);
            if libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut()) == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }
    let mut child = command.spawn().unwrap();
    let pid = child.id().unwrap();
    tokio::time::timeout(Duration::from_secs(20), entered.notified())
        .await
        .expect("CLI never reached its private capture barrier");
    let fds = peer_fds(pid);
    assert!(!fds.is_empty(), "accepted CLI RPC must be identifiable");
    assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGINT) }, 0);
    assert!(!tokio::time::timeout(Duration::from_secs(20), child.wait())
        .await
        .unwrap()
        .unwrap()
        .success());
    // No delay assumed to mean disconnected: observe closure of the actual
    // accepted socket and its duplicate, while its actor dependency stays held.
    tokio::time::timeout(Duration::from_secs(20), async {
        while fds
            .iter()
            .any(|(path, link)| std::fs::read_link(path).ok().as_ref() == Some(link))
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("full-close did not drop the waiting reply RPC");
    release.add_permits(1);
    control.interrupt("%9".into()).await.unwrap(); // actor-progress barrier on another mock pane
    assert!(!mock
        .calls()
        .iter()
        .any(|c| matches!(c, MockCall::SendKeys { pane, .. } if pane == "%1")));
    cancel.cancel();
    server.await.unwrap().unwrap();
    actor.await.unwrap();
    assert!(org
        .tail(atmd::org::TailParams::default())
        .await
        .unwrap()
        .iter()
        .all(|e| e.body["action"]["method"] != "session.reply"));
    org_cancel.cancel();
}
