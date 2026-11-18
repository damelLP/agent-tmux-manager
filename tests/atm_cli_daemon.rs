//! List/status still fetch ListSessions from a fake daemon. Session control
//! commands exercise the real method router and an injected tmux client, and
//! autostart a private daemon with stand-in tmux. None falls back to an old
//! daemon's legacy exchange or the CLI's own tmux.
//!
//! Every CLI child has a cleared environment with private socket/state/tmux
//! paths. No test reaches a real tmux server or launches a model harness.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)] // test code may panic

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

use atm_core::{SessionId, SessionView};
#[path = "support/daemon.rs"]
mod daemon;
use daemon::fake_daemon;
#[path = "support/control.rs"]
mod control_support;
use control_support::role;

// Keep helper assertions at their implementation, but register once: other
// fixture consumers must not repeat these real-process and readiness checks.
#[test]
fn autostart_cleanup_does_not_panic_on_invalid_or_unowned_pid() {
    control_support::autostart_cleanup_does_not_panic_on_invalid_or_unowned_pid();
}

#[test]
fn listening_wait_holds_partial_line_until_pipe_release() {
    control_support::log_event_tests::listening_wait_holds_partial_line_until_pipe_release();
}

#[test]
fn listening_event_delivered_before_wait_is_retained() {
    control_support::log_event_tests::listening_event_delivered_before_wait_is_retained();
}

#[test]
fn daemon_exit_before_listening_returns_the_log() {
    control_support::log_event_tests::daemon_exit_before_listening_returns_the_log();
}

#[test]
fn synchronous_readiness_does_not_block_on_a_full_backlog() {
    control_support::socket_readiness_tests::synchronous_readiness_does_not_block_on_a_full_backlog(
    );
}

#[tokio::test]
async fn wait_for_socket_retries_bound_socket_until_listen() {
    control_support::socket_readiness_tests::wait_for_socket_retries_bound_socket_until_listen()
        .await;
}

#[tokio::test]
async fn wait_for_socket_observes_creation_after_watch() {
    control_support::socket_readiness_tests::wait_for_socket_observes_creation_after_watch().await;
}

#[tokio::test]
async fn wait_for_socket_connects_to_existing_listener() {
    control_support::socket_readiness_tests::wait_for_socket_connects_to_existing_listener().await;
}

#[test]
fn cli_exit_observes_pipe_held_and_already_exited_children() {
    use std::io::{BufRead, BufReader, Write};
    use std::os::fd::OwnedFd;
    use std::process::Stdio;
    use std::time::Duration;

    let (output, writer) = std::os::unix::net::UnixStream::pair().unwrap();
    output
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let mut child = Command::new("/bin/sh")
        .args(["-c", "printf 'ready\\n'; read -r finish; exit 7"])
        .stdin(Stdio::piped())
        .stdout(Stdio::from(OwnedFd::from(writer)))
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let pid = child.id();
    let child = role::process::OwnedChild::new(child);
    let mut ready = String::new();
    BufReader::new(output).read_line(&mut ready).unwrap();
    assert_eq!(ready, "ready\n");
    assert_eq!(
        role::process::wait_exit(pid, Duration::ZERO)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::TimedOut,
        "a ready child held on a pipe has not exited"
    );
    input.write_all(b"finish\n").unwrap();
    role::process::wait_exit(pid, Duration::from_secs(30)).unwrap();
    // The exit event is already present and the child is still unreaped.
    role::process::wait_exit(pid, Duration::ZERO).unwrap();
    assert_eq!(child.wait_with_output().unwrap().status.code(), Some(7));
}

#[test]
fn cli_exit_collects_full_file_backed_output() {
    let output = role::run(Command::new("/usr/bin/python3").args([
        "-c",
        "import sys; sys.stdout.write('x'*262144); sys.stderr.write('y'*262144)",
    ]));
    assert!(output.status.success());
    assert_eq!(output.stdout.len(), 262144);
    assert_eq!(output.stderr.len(), 262144);
    assert_eq!(output.stdout, vec![b'x'; 262144]);
    assert_eq!(output.stderr, vec![b'y'; 262144]);
}

#[test]
fn cli_exit_owned_child_reaps_during_unwind() {
    role::process::tests::owned_child_drop_reaps_on_unwind();
}

mod shutdown {
    use atm_tmux::{mock::MockCall, MockTmuxClient, PaneDirection, TmuxClient, TmuxError};
    use atmd::{control::service::spawn_control, org::OrgHandle};
    use std::{sync::Arc, time::Duration};
    use tokio::sync::{Notify, Semaphore};
    use tokio_util::sync::CancellationToken;

    struct PausedPaste {
        mock: MockTmuxClient,
        pasted: Arc<Notify>,
        release: Arc<Semaphore>,
    }
    #[async_trait::async_trait]
    impl TmuxClient for PausedPaste {
        async fn raw_command(&self, cmd: &str, args: &[&str]) -> Result<String, TmuxError> {
            self.mock.raw_command(cmd, args).await
        }
        async fn split_window(
            &self,
            target: &str,
            size: &str,
            direction: PaneDirection,
            command: Option<&str>,
            cwd: Option<&str>,
            detached: bool,
        ) -> Result<String, TmuxError> {
            self.mock
                .split_window(target, size, direction, command, cwd, detached)
                .await
        }
        async fn get_pane_cwd(&self, pane: &str) -> Result<Option<String>, TmuxError> {
            self.mock.get_pane_cwd(pane).await
        }
        async fn new_window(
            &self,
            session: &str,
            command: Option<&str>,
        ) -> Result<String, TmuxError> {
            self.mock.new_window(session, command).await
        }
        async fn kill_pane(&self, pane: &str) -> Result<(), TmuxError> {
            self.mock.kill_pane(pane).await
        }
        async fn resize_pane(
            &self,
            pane: &str,
            width: Option<u16>,
            height: Option<u16>,
        ) -> Result<(), TmuxError> {
            self.mock.resize_pane(pane, width, height).await
        }
        async fn send_keys(&self, pane: &str, keys: &str) -> Result<(), TmuxError> {
            self.mock.send_keys(pane, keys).await
        }
        async fn paste_text(&self, pane: &str, text: &str) -> Result<(), TmuxError> {
            self.mock.paste_text(pane, text).await?;
            self.pasted.notify_one();
            let _permit = self.release.acquire().await.unwrap();
            Ok(())
        }
        async fn list_panes(&self) -> Result<Vec<atm_tmux::PaneInfo>, TmuxError> {
            self.mock.list_panes().await
        }
        async fn display_popup(
            &self,
            width: &str,
            height: &str,
            command: &str,
        ) -> Result<(), TmuxError> {
            self.mock.display_popup(width, height, command).await
        }
        async fn select_pane(&self, pane: &str) -> Result<(), TmuxError> {
            self.mock.select_pane(pane).await
        }
        async fn capture_pane(&self, pane: &str) -> Result<Vec<String>, TmuxError> {
            self.mock.capture_pane(pane).await
        }
        async fn new_session(&self, name: &str) -> Result<String, TmuxError> {
            self.mock.new_session(name).await
        }
    }

    #[tokio::test]
    async fn shutdown_finishes_paste_enter_and_drains_its_audit() {
        tokio::time::timeout(Duration::from_secs(30), async {
            let dir = tempfile::tempdir().unwrap();
            let org_cancel = CancellationToken::new();
            let org = OrgHandle::open(dir.path().into(), org_cancel.clone()).await;
            let mock = MockTmuxClient::new();
            let pasted = Arc::new(Notify::new());
            let release = Arc::new(Semaphore::new(0));
            let cancel = CancellationToken::new();
            let (control, task) = spawn_control(Box::new(PausedPaste { mock: mock.clone(), pasted: pasted.clone(), release: release.clone() }), atmd::registry::spawn_registry(), org.clone(), cancel.clone());
            let action = tokio::spawn(async move { control.send("%7".into(), "once".into()).await });
            pasted.notified().await;
            cancel.cancel();
            release.add_permits(1);
            assert_eq!(action.await.unwrap().unwrap(), "%7");
            task.await.unwrap();
            assert!(matches!(mock.calls().last(), Some(MockCall::SendKeys { keys, .. }) if keys == "Enter"));
            let events = org.tail(atmd::org::TailParams::default()).await.unwrap();
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].body["action"]["outcome"], "ok");
            org_cancel.cancel();
        }).await.unwrap();
    }
}

fn autostart_control(args: &[&str], expected: &[u8]) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("path");
    std::fs::create_dir(&path).unwrap();
    let tmux = path.join("tmux");
    std::fs::write(
        &tmux,
        r#"#!/bin/sh
[ "$1" = -u ] && shift
if [ "$1" = -L ]; then shift; shift; fi
case $1 in
list-panes) printf '%%7\tprobe\t0\t1\t80\t24\t1\n' ;;
display-message)
case "$*" in *'#{socket_path}'*) echo "$CONTROL_TEST_TMUX_SOCKET" ;; *) echo '0 1' ;; esac ;;
split-window) echo %42 ;;
esac
"#,
    )
    .unwrap();
    std::fs::set_permissions(&tmux, std::fs::Permissions::from_mode(0o755)).unwrap();
    let guard = control_support::Autostarted(dir.path().to_owned());
    assert!(!dir.path().join("atmd.sock").exists());
    let mut command = Command::new(env!("CARGO_BIN_EXE_atm"));
    control_support::isolated(&mut command, dir.path())
        .env("PATH", &path)
        .env("TMUX", "private-stand-in")
        .env("TMUX_PANE", "%7")
        .env("ATM_SPAWN_BIN", "true")
        .args(args);
    let out = command.output().unwrap();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(out.stdout, expected);
    assert!(dir.path().join("atmd.sock").exists());
    control_support::assert_owned(guard.pid(), dir.path());
    drop(guard);
    assert!(!dir.path().join("atmd.sock").exists());
    // Exercise production server -> real actor -> real org wiring, not an
    // injected control handle. Shutdown is the audit-drain barrier.
    let events = atmd::eventlog::read_from(
        &dir.path().join("events"),
        atmd::eventlog::Stream::Events,
        0,
    )
    .collect::<Result<Vec<_>, _>>()
    .unwrap();
    let kinds: Vec<_> = events
        .iter()
        .filter(|e| e.kind.starts_with("control."))
        .map(|e| e.kind.as_str())
        .collect();
    match args.first() {
        Some(&"send") | Some(&"interrupt") | Some(&"reply") => assert_eq!(kinds, ["control.sent"]),
        Some(&"peek") => assert!(kinds.is_empty(), "read-only captures are not audited"),
        Some(&"kill") => assert_eq!(kinds, ["control.killed"]),
        _ => assert_eq!(kinds, ["control.spawned", "control.sent"]),
    }
}

#[test]
fn send_autostarts_a_stopped_private_daemon() {
    autostart_control(&["send", "%7", "hello"], b"Sent to %7 by tmux paste\n");
}

#[test]
fn spawn_autostarts_a_stopped_private_daemon() {
    autostart_control(&["spawn", "-w", "/work"], b"%42\n");
}

#[test]
fn kill_autostarts_a_stopped_private_daemon() {
    autostart_control(&["kill", "%7"], b"Killed %7\n");
}

#[test]
fn interrupt_autostarts_a_stopped_private_daemon() {
    autostart_control(&["interrupt", "%7"], b"Interrupted %7\n");
}

#[test]
fn reply_autostarts_a_stopped_private_daemon() {
    autostart_control(&["reply", "%7", "-y"], b"Sent Enter to %7\n");
}

#[test]
fn peek_autostarts_a_stopped_private_daemon() {
    autostart_control(&["peek", "%7"], b"");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reply_and_peek_use_the_daemon_and_preserve_exact_output_and_refusals() {
    use atm_core::SessionDomain;
    use atm_tmux::{mock::MockCall, MockTmuxClient};
    use atmd::{
        control::service::spawn_control,
        server::{methods::MethodServices, DaemonServer},
    };
    use tokio_util::sync::CancellationToken;
    let dir = tempfile::tempdir().unwrap();
    let registry = atmd::registry::spawn_registry();
    for (id, pane) in [
        ("abc-session-1", Some("%5")),
        ("dup-1111", Some("%6")),
        ("dup-2222", Some("%7")),
        ("bare-session", None),
    ] {
        registry
            .register(SessionDomain {
                id: SessionId::new(id),
                tmux_pane: pane.map(str::to_owned),
                ..Default::default()
            })
            .await
            .unwrap();
    }
    let mock = MockTmuxClient::new();
    mock.set_pane_content(
        "%5",
        [
            "earlier  ",
            "Choose café?",
            "❯ 1. Sí",
            "  2. No",
            "Enter to select",
            "",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect(),
    );
    let cancel = CancellationToken::new();
    let org_cancel = CancellationToken::new();
    let org = atmd::org::OrgHandle::open(dir.path().join("org"), org_cancel.clone()).await;
    let (control, actor) = spawn_control(
        Box::new(mock.clone()),
        registry.clone(),
        org.clone(),
        cancel.clone(),
    );
    let mut services = MethodServices::new(registry);
    services.org = org.clone();
    services.control = Some(control.clone());
    let server =
        DaemonServer::with_services(dir.path().join("atmd.sock"), services, cancel.clone());
    let server = control_support::start_server(server, &dir.path().join("atmd.sock")).await;
    let socket = dir
        .path()
        .join(format!("tmux-{}/default", unsafe { libc::getuid() }));
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    std::fs::write(&socket, "").unwrap();
    let exe = Path::new(env!("CARGO_BIN_EXE_atm"));
    for (args, expected, keys, captures) in [
        (
            vec!["reply", "abc"],
            "Sent Enter to %5\n",
            vec!["Enter"],
            false,
        ),
        (
            vec!["reply", "abc", "-y"],
            "Sent Enter to %5\n",
            vec!["Enter"],
            false,
        ),
        (
            vec!["reply", "abc", "-n"],
            "Sent Escape to %5\n",
            vec!["Escape"],
            false,
        ),
        (
            vec!["reply", "abc", "-o", "3"],
            "Selected option 3 on %5\n",
            vec!["Down", "Down", "Enter"],
            true,
        ),
        (
            vec!["reply", "abc", "-o", "0"],
            "Selected option 0 on %5\n",
            vec!["Up", "Enter"],
            true,
        ),
        (
            vec!["peek", "abc"],
            "earlier  \nChoose café?\n❯ 1. Sí\n  2. No\nEnter to select\n\n",
            vec![],
            true,
        ),
        (vec!["peek", "abc", "-n", "0"], "", vec![], true),
        (
            vec!["peek", "abc", "-n", "2"],
            "Enter to select\n\n",
            vec![],
            true,
        ),
        (
            vec!["peek", "abc", "--prompt", "-n", "0"],
            "Choose café?\n❯ 1. Sí\n  2. No\nEnter to select\n",
            vec![],
            true,
        ),
    ] {
        mock.clear_calls();
        mock.set_next_raw_output(socket.to_str().unwrap());
        let out = atm(exe, dir.path(), &args);
        assert!(out.status.success(), "{args:?}: {out:?}");
        assert_eq!(out.stdout, expected.as_bytes());
        let mut expected_calls = Vec::new();
        if captures {
            expected_calls.push(MockCall::CapturePane { pane: "%5".into() });
        }
        expected_calls.extend(keys.into_iter().map(|keys| MockCall::SendKeys {
            pane: "%5".into(),
            keys: keys.into(),
        }));
        assert_eq!(
            mock.calls()
                .into_iter()
                .filter(|call| !matches!(call, MockCall::RawCommand { .. }))
                .collect::<Vec<_>>(),
            expected_calls,
            "prefix must reach the daemon's tmux as %5"
        );
    }
    for command in ["reply", "peek"] {
        for (target, expected) in [
            ("nope", "Error: No session matching 'nope'"),
            ("bare", "Error: Session bare-ses has no tmux pane"),
        ] {
            mock.clear_calls();
            mock.set_next_raw_output(socket.to_str().unwrap());
            let args = [command, target];
            assert_fails_with(&atm(exe, dir.path(), &args), &args, expected);
            assert!(matches!(
                mock.calls().as_slice(),
                [MockCall::RawCommand { .. }]
            ));
        }
        mock.clear_calls();
        mock.set_next_raw_output(socket.to_str().unwrap());
        let out = atm(exe, dir.path(), &[command, "dup"]);
        assert_eq!(out.status.code(), Some(1));
        assert!([
            "Error: Ambiguous target 'dup' matches 2 sessions: dup-1111, dup-2222\n",
            "Error: Ambiguous target 'dup' matches 2 sessions: dup-2222, dup-1111\n"
        ]
        .contains(&String::from_utf8(out.stderr).unwrap().as_str()));
        assert!(matches!(
            mock.calls().as_slice(),
            [MockCall::RawCommand { .. }]
        ));
    }
    cancel.cancel();
    server.await.unwrap().unwrap();
    actor.await.unwrap();
    let records = org.tail(atmd::org::TailParams::default()).await.unwrap();
    assert_eq!(
        records.len(),
        5,
        "only five replies, never a capture, are audited"
    );
    assert!(records
        .iter()
        .all(|e| e.kind == "control.sent" && e.body["action"]["method"] == "session.reply"));
    org_cancel.cancel();
}

/// The CLI has no tmux executable: the daemon resolves the target in its
/// registry and its control actor makes the one tmux call.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill_and_interrupt_use_daemon_registry_and_preserve_refusals() {
    use atm_core::SessionDomain;
    use atm_tmux::{mock::MockCall, MockTmuxClient};
    use atmd::{
        control::service::spawn_control,
        server::{methods::MethodServices, DaemonServer},
    };
    use tokio_util::sync::CancellationToken;
    let dir = tempfile::tempdir().unwrap();
    let registry = atmd::registry::spawn_registry();
    registry
        .register(SessionDomain {
            id: SessionId::new("abc-session-1"),
            tmux_pane: Some("%5".into()),
            ..SessionDomain::default()
        })
        .await
        .unwrap();
    let mock = MockTmuxClient::new();
    let cancel = CancellationToken::new();
    let (control, actor) = spawn_control(
        Box::new(mock.clone()),
        registry.clone(),
        atmd::org::OrgHandle::disabled("test disabled"),
        cancel.clone(),
    );
    let mut services = MethodServices::new(registry.clone());
    services.control = Some(control);
    let server =
        DaemonServer::with_services(dir.path().join("atmd.sock"), services, cancel.clone());
    let server = control_support::start_server(server, &dir.path().join("atmd.sock")).await;
    let exe = Path::new(env!("CARGO_BIN_EXE_atm"));
    let tmux_socket = dir
        .path()
        .join(format!("tmux-{}/default", unsafe { libc::getuid() }));
    std::fs::create_dir_all(tmux_socket.parent().unwrap()).unwrap();
    std::fs::write(&tmux_socket, "").unwrap();
    for (command, done) in [("interrupt", "Interrupted %5\n"), ("kill", "Killed %5\n")] {
        // A known prefix reaches tmux with the session's pane, once.
        mock.clear_calls();
        mock.set_next_raw_output(tmux_socket.to_str().unwrap());
        let out = atm(exe, dir.path(), &[command, "abc"]);
        assert!(out.status.success(), "atm {command}: {out:?}");
        assert_eq!(String::from_utf8_lossy(&out.stdout), done);
        let calls = mock.calls();
        let effect = match calls.as_slice() {
            [MockCall::RawCommand { .. }, MockCall::KillPane { pane }] => {
                command == "kill" && pane == "%5"
            }
            [MockCall::RawCommand { .. }, MockCall::SendKeys { pane, keys }] => {
                command == "interrupt" && pane == "%5" && keys == "C-c"
            }
            _ => false,
        };
        assert!(effect, "atm {command}: {calls:?}");
        // An unknown one fails before any pane effect, with the same text.
        mock.clear_calls();
        mock.set_next_raw_output(tmux_socket.to_str().unwrap());
        let args = [command, "nope"];
        assert_fails_with(
            &atm(exe, dir.path(), &args),
            &args,
            "Error: No session matching 'nope'",
        );
        assert!(matches!(
            mock.calls().as_slice(),
            [MockCall::RawCommand { .. }]
        ));
    }
    cancel.cancel();
    server.await.unwrap().unwrap();
    actor.await.unwrap();
}

/// The CLI has no tmux executable; registry resolution and safety checks run
/// in the real server through its injected control actor.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_uses_daemon_registry_and_preserves_refusals() {
    use atm_core::{SessionDomain, SessionStatus};
    use atm_tmux::{mock::MockCall, MockTmuxClient};
    use atmd::{
        control::service::spawn_control,
        server::{methods::MethodServices, DaemonServer},
    };
    use tokio_util::sync::CancellationToken;
    let dir = tempfile::tempdir().unwrap();
    let registry = atmd::registry::spawn_registry();
    registry
        .register(SessionDomain {
            id: SessionId::new("abc-session-1"),
            tmux_pane: Some("%5".into()),
            ..SessionDomain::default()
        })
        .await
        .unwrap();
    let mock = MockTmuxClient::new();
    let cancel = CancellationToken::new();
    let (control, actor) = spawn_control(
        Box::new(mock.clone()),
        registry.clone(),
        atmd::org::OrgHandle::disabled("test disabled"),
        cancel.clone(),
    );
    let mut services = MethodServices::new(registry.clone());
    services.control = Some(control);
    let server =
        DaemonServer::with_services(dir.path().join("atmd.sock"), services, cancel.clone());
    let server = control_support::start_server(server, &dir.path().join("atmd.sock")).await;
    let exe = Path::new(env!("CARGO_BIN_EXE_atm"));
    let tmux_socket = dir
        .path()
        .join(format!("tmux-{}/default", unsafe { libc::getuid() }));
    std::fs::create_dir_all(tmux_socket.parent().unwrap()).unwrap();
    std::fs::write(&tmux_socket, "").unwrap();
    mock.set_next_raw_output(tmux_socket.to_str().unwrap());
    mock.set_next_raw_output("ATM-PANE-v1\t%5\tprivate\t0\t123\t80\t24\t1\t0\t0\t1\t0\t0\n");
    mock.set_next_raw_output(""); // disabled org, positively untagged pane
    mock.set_next_raw_output("0 1");
    let out = atm(exe, dir.path(), &["send", "abc", "hello;"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(out.stdout, b"Sent to %5 by tmux paste\n");
    assert!(
        matches!(mock.calls().as_slice(), [MockCall::RawCommand { .. }, MockCall::RawCommand { .. }, MockCall::RawCommand { .. }, MockCall::RawCommand { .. }, MockCall::PasteText { pane, text }, MockCall::SendKeys { keys, .. }] if pane == "%5" && text == "hello;" && keys == "Enter")
    );
    mock.clear_calls();
    mock.set_next_raw_output(tmux_socket.to_str().unwrap());
    let args = ["send", "nope", "hello"];
    assert_fails_with(
        &atm(exe, dir.path(), &args),
        &args,
        "Error: No session matching 'nope'",
    );
    assert!(matches!(
        mock.calls().as_slice(),
        [MockCall::RawCommand { .. }]
    ));
    for (state, text, reason) in [
        (
            "1 0",
            "hi",
            "the pane is in copy mode (or another mode), so Enter would go to tmux",
        ),
        (
            "0 0",
            "a\nb",
            "the program didn't ask for bracketed paste, so each line would submit alone",
        ),
    ] {
        mock.set_next_raw_output(tmux_socket.to_str().unwrap());
        mock.set_next_raw_output("ATM-PANE-v1\t%5\tprivate\t0\t123\t80\t24\t1\t0\t0\t1\t0\t0\n");
        mock.set_next_raw_output(""); // ownership read precedes the original safety check
        mock.set_next_raw_output(state);
        let args = ["send", "%5", text];
        assert_fails_with(
            &atm(exe, dir.path(), &args),
            &args,
            &format!("Error: Refusing to send to %5: {reason}"),
        );
    }
    registry
        .register(SessionDomain {
            id: SessionId::new("dialog"),
            tmux_pane: Some("%5".into()),
            status: SessionStatus::AttentionNeeded,
            ..SessionDomain::default()
        })
        .await
        .unwrap();
    mock.set_next_raw_output(tmux_socket.to_str().unwrap());
    mock.set_next_raw_output("ATM-PANE-v1\t%5\tprivate\t0\t123\t80\t24\t1\t0\t0\t1\t0\t0\n");
    mock.set_next_raw_output("");
    mock.set_next_raw_output("0 1");
    let args = ["send", "%5", "hi"];
    assert_fails_with(&atm(exe, dir.path(), &args), &args, "Error: Refusing to send to %5: the session is waiting on a dialog that Enter would answer; use `atm reply`, or, if the dialog is gone, type in the pane");
    assert!(mock
        .calls()
        .iter()
        .all(|c| matches!(c, MockCall::RawCommand { .. })));
    cancel.cancel();
    server.await.unwrap().unwrap();
    actor.await.unwrap();
}

/// Pane commands that are daemon requests. They read no session list.
fn daemon_commands(target: &str) -> Vec<Vec<&str>> {
    vec![
        vec!["kill", target],
        vec!["interrupt", target],
        vec!["reply", target],
        vec!["peek", target],
    ]
}

/// Every command that still reads the legacy session list.
fn all_commands() -> Vec<Vec<&'static str>> {
    vec![vec!["list", "--format", "json"], vec!["status"]]
}

/// Runs `exe` with a cleared environment rooted in `dir`. `PATH` is
/// `dir/path`, which is empty unless a test puts a stand-in there.
fn atm(exe: &Path, dir: &Path, args: &[&str]) -> Output {
    let path = dir.join("path");
    std::fs::create_dir_all(&path).expect("mkdir PATH dir");
    role::run(
        Command::new(exe)
            .args(args)
            .env_clear()
            .env("PATH", &path)
            .env("HOME", dir)
            .env("ATM_SOCKET", dir.join("atmd.sock"))
            .env("XDG_STATE_HOME", dir.join("state"))
            .env("ATM_STATE_DIR", dir.join("eventlog"))
            .env("TMUX_TMPDIR", dir),
    )
}

fn assert_fails_with(out: &Output, args: &[&str], expected: &str) {
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(1),
        "atm {args:?}: stdout {:?}, stderr {stderr:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(stderr.trim_end(), expected, "atm {args:?}");
}

/// No daemon listens and no `atmd` can be found, so every command fails
/// at the auto-start, before it asks for the session list.
#[test]
fn test_commands_fail_when_the_daemon_cannot_start() {
    let dir = role::artifact_dir();
    // The auto-start looks for `atmd` beside `atm` first. A fixture beside
    // the artifact guarantees a hard link without an inherited copy writer.
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).expect("mkdir bin");
    let exe = bin.join("atm");
    std::fs::hard_link(env!("CARGO_BIN_EXE_atm"), &exe).expect("link atm");

    for args in all_commands()
        .into_iter()
        .chain([vec!["send", "%0", "hello"]])
        .chain(daemon_commands("%0"))
    {
        let out = atm(&exe, dir.path(), &args);
        // `ensure_daemon_running`'s own message already starts with
        // "Failed to start daemon", and the CLI adds the same words.
        assert_fails_with(
            &out,
            &args,
            "Error: Failed to start daemon: Failed to start daemon: \
             No such file or directory (os error 2)",
        );
    }
}

/// A daemon that hangs up before SessionList fails both legacy list readers.
#[test]
fn test_commands_fail_when_the_daemon_closes_before_the_session_list() {
    let dir = tempfile::tempdir().expect("tempdir");
    fake_daemon(dir.path(), None);

    for args in all_commands() {
        let out = atm(Path::new(env!("CARGO_BIN_EXE_atm")), dir.path(), &args);
        assert_fails_with(
            &out,
            &args,
            "Error: Daemon closed the connection before sending the session list",
        );
    }
}

/// List/status still print the daemon's legacy session list. Pane prefix and
/// refusal assertions now live in the real-router tests above.
#[test]
fn test_commands_read_the_daemons_session_list() {
    let dir = tempfile::tempdir().expect("tempdir");
    fake_daemon(
        dir.path(),
        Some(vec![SessionView {
            id: SessionId::new("abc-session-1"),
            id_short: "abc-sess".to_string(),
            tmux_pane: Some("%5".to_string()),
            ..SessionView::default()
        }]),
    );
    let exe = Path::new(env!("CARGO_BIN_EXE_atm"));
    let list = atm(exe, dir.path(), &["list", "--format", "json"]);
    assert!(list.status.success(), "atm list: {list:?}");
    let listed: serde_json::Value = serde_json::from_slice(&list.stdout).expect("list JSON");
    let ids: Vec<&str> = listed
        .as_array()
        .expect("JSON array")
        .iter()
        .filter_map(|s| s["id"].as_str())
        .collect();
    assert_eq!(ids, ["abc-session-1"]);

    let status = atm(exe, dir.path(), &["status"]);
    assert!(status.status.success(), "atm status: {status:?}");
    assert_eq!(
        String::from_utf8_lossy(&status.stdout),
        "1\u{2191} $0.00 ? unread ? decisions\n"
    );
    assert!(String::from_utf8_lossy(&status.stderr)
        .contains("unread count unavailable: the daemon is too old; restart atmd"));
    assert!(String::from_utf8_lossy(&status.stderr)
        .contains("decision count unavailable: the daemon is too old; restart atmd"));
}

/// A daemon that does not list the method is too old. All pane commands say
/// so and stop: they never fall back to a tmux call of their own, which
/// would be unordered and unlogged.
#[test]
fn kill_and_interrupt_refuse_an_old_daemon_without_touching_tmux() {
    let dir = tempfile::tempdir().expect("tempdir");
    fake_daemon(
        dir.path(),
        Some(vec![SessionView {
            id: SessionId::new("abc-session-1"),
            id_short: "abc-sess".to_string(),
            tmux_pane: Some("%5".to_string()),
            ..SessionView::default()
        }]),
    );
    let exe = Path::new(env!("CARGO_BIN_EXE_atm"));
    let tmux_log = dir.path().join("tmux.log");
    let tmux = dir.path().join("path").join("tmux");
    std::fs::create_dir_all(dir.path().join("path")).expect("mkdir PATH dir");
    std::fs::write(
        &tmux,
        format!("#!/bin/sh\necho \"$*\" >> '{}'\n", tmux_log.display()),
    )
    .expect("write stand-in tmux");
    std::fs::set_permissions(&tmux, std::fs::Permissions::from_mode(0o755)).expect("chmod tmux");

    for args in daemon_commands("abc")
        .into_iter()
        .chain(daemon_commands("%5"))
    {
        let out = atm(exe, dir.path(), &args);
        assert_fails_with(&out, &args, "Error: the daemon is too old; restart atmd");
    }
    assert!(!tmux_log.exists(), "an old daemon's kill reached tmux");
}
