//! Opt-in real daemon/CLI/tmux check; no harness or model is started.
use super::*;
use atm_tmux::{PaneDirection, RealTmuxClient};
use std::{
    os::unix::process::ExitStatusExt,
    path::{Path, PathBuf},
};

struct ObservedTmux {
    mock: RealTmuxClient,
    resolved: Arc<tokio::sync::Notify>,
}
impl TmuxClient for ObservedTmux {
    fn raw_command<'s, 'c, 'a, 'v, 'async_trait>(
        &'s self,
        command: &'c str,
        args: &'a [&'v str],
    ) -> Call<'async_trait, String>
    where
        's: 'async_trait,
        'c: 'async_trait,
        'a: 'async_trait,
        'v: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            let result = self.mock.raw_command(command, args).await;
            if args.contains(&"#{pane_id}") {
                self.resolved.notify_one();
            }
            result
        })
    }
    forward!(paste_text<'a, 'b>(pane: &'a str, text: &'b str) -> ());
    forward!(send_keys<'a, 'b>(pane: &'a str, keys: &'b str) -> ());
    forward!(split_window<'a, 'b, 'c, 'd>(target: &'a str, size: &'b str, direction: PaneDirection, command: Option<&'c str>, cwd: Option<&'d str>, detached: bool) -> String);
    forward!(get_pane_cwd<'a>(pane: &'a str) -> Option<String>);
    forward!(new_window<'a, 'b>(session: &'a str, command: Option<&'b str>) -> String);
    forward!(kill_pane<'a>(pane: &'a str) -> ());
    forward!(resize_pane<'a>(pane: &'a str, width: Option<u16>, height: Option<u16>) -> ());
    forward!(list_panes<>() -> Vec<PaneInfo>);
    forward!(display_popup<'a, 'b, 'c>(width: &'a str, height: &'b str, command: &'c str) -> ());
    forward!(select_pane<'a>(pane: &'a str) -> ());
    forward!(capture_pane<'a>(pane: &'a str) -> Vec<String>);
    forward!(new_session<'a>(name: &'a str) -> String);
}

fn socket_fds() -> Vec<PathBuf> {
    let mut sockets: Vec<_> = std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(|entry| std::fs::read_link(entry.ok()?.path()).ok())
        .filter(|link| link.to_string_lossy().starts_with("socket:"))
        .collect();
    sockets.sort();
    sockets
}

async fn private_identity(client: &RealTmuxClient, root: &Path) -> String {
    let identity = client
        .raw_command("display-message", &["-p", "#{socket_path}\t#{pid}"])
        .await
        .unwrap();
    let (socket, pid) = identity.trim().split_once('\t').unwrap();
    assert!(Path::new(socket).starts_with(root));
    assert!(pid.parse::<u32>().unwrap() > 0);
    identity
}

#[tokio::test]
#[ignore = "requires sealed Linux environment, private tmux and ATM_TEST_CLI"]
async fn real_cli_sigint_discards_reservation_parked_send() {
    assert!(std::env::var_os("TMUX").is_none());
    assert!(std::env::var_os("TMUX_PANE").is_none());
    let root = PathBuf::from(std::env::var("TMUX_TMPDIR").unwrap());
    let socket = PathBuf::from(std::env::var("ATM_SOCKET").unwrap());
    assert!(root.is_absolute());
    assert!(socket.starts_with(root.parent().unwrap()));
    assert_ne!(socket, Path::new("/tmp/atm.sock"));
    let label = std::env::var("ATM_TMUX_SOCKET").unwrap();
    assert!(label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    let cli = PathBuf::from(std::env::var("ATM_TEST_CLI").unwrap());
    assert!(cli.is_absolute());
    let client = RealTmuxClient::with_socket(label);
    eprintln!("{}", client.raw_command("-V", &[]).await.unwrap().trim());
    client
        .raw_command("new-session", &["-d", "-s", "fixture", "cat"])
        .await
        .unwrap();
    let identity = private_identity(&client, &root).await;
    let cancel = CancellationToken::new();
    let registry = crate::registry::spawn_registry();
    registry
        .register(SessionDomain {
            id: SessionId::new("fixture-session"),
            tmux_pane: Some("fixture:1.0".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    let resolved = Arc::new(tokio::sync::Notify::new());
    let (control, actor) = spawn_control(
        Box::new(ObservedTmux {
            mock: client.clone(),
            resolved: resolved.clone(),
        }),
        registry.clone(),
        OrgHandle::disabled("private fixture"),
        cancel.clone(),
    );
    let mut services = methods::MethodServices::new(registry);
    services.control = Some(control.clone());
    let server = crate::server::DaemonServer::with_services(&socket, services, cancel.clone());
    let mut server = Box::pin(async move { server.run().await });
    std::future::poll_fn(|cx| {
        assert!(server.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    let server = tokio::spawn(server);
    let work = client.clone();
    let check_root = root.clone();
    let check_identity = identity.clone();
    let probe = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(60), async move {
            let input = check_root.join("input.bin");
            let ready_file = check_root.join("pane-ready");
            let script = check_root.join("reader.py");
            std::fs::write(&script, format!(
                "import os, pathlib, tty\ntty.setraw(0)\npathlib.Path({:?}).touch()\nwith open({:?}, 'ab', buffering=0) as f:\n while True:\n  b=os.read(0,4096)\n  if not b: break\n  f.write(b)\n", ready_file, input
            )).unwrap();
            assert_eq!(work.new_window("fixture", Some(&format!("/usr/bin/python3 {}", script.display()))).await.unwrap(), "%1");
            while !ready_file.exists() { tokio::task::yield_now().await; }
            let (job, release) = hold(&control, ReservationKind::Delivery).await;
            let baseline = socket_fds();
            let mut child = tokio::process::Command::new(&cli)
                .args(["send", "fixture-session", "CANCELLED-U8C9"])
                .kill_on_drop(true).spawn().unwrap();
            resolved.notified().await; // real alias resolution inside send's actor dispatch
            barrier(&control).await; // dispatch finished parking behind the held reservation
            assert_ne!(socket_fds(), baseline);
            assert!(std::fs::read(&input).unwrap().is_empty());
            assert_eq!(private_identity(&work, &check_root).await, check_identity);
            let pid = child.id().unwrap();
            // Direct child owned by this fixture, not a discovered process.
            assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGINT) }, 0);
            assert_eq!(child.wait().await.unwrap().signal(), Some(libc::SIGINT));
            // The server closes both its accepted fd and readiness duplicate
            // after dropping the method wait. Wait for that resource event,
            // not a sleep, before making the pane available.
            while socket_fds() != baseline { tokio::task::yield_now().await; }
            release.send(()).unwrap();
            job.await.unwrap().unwrap();
            control.flush_audits().await;
            assert!(std::fs::read(&input).unwrap().is_empty());
            let status = tokio::process::Command::new(&cli)
                .args(["send", "%1", "POSITIVE-U8C9"])
                .kill_on_drop(true).status().await.unwrap();
            assert!(status.success());
            while std::fs::read(&input).unwrap() != b"POSITIVE-U8C9\r" {
                tokio::task::yield_now().await;
            }
            eprintln!("SIGINT parked CLI: no bytes or Enter; positive control: POSITIVE-U8C9\\r");
        }).await.unwrap();
    }).await;
    cancel.cancel();
    actor.await.unwrap();
    server.await.unwrap().unwrap();
    assert_eq!(private_identity(&client, &root).await, identity);
    client.raw_command("kill-server", &[]).await.unwrap();
    probe.unwrap();
}
