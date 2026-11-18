//! Opt-in real daemon/CLI/tmux check; no harness or model is started.
use super::*;
use atm_tmux::{PaneDirection, RealTmuxClient};
use std::{
    os::unix::process::ExitStatusExt,
    path::{Path, PathBuf},
};

#[allow(dead_code)]
#[path = "../../../../tests/support/stand_in/process.rs"]
mod process;

struct CliChild {
    child: process::OwnedChild,
    pid: u32,
}
impl CliChild {
    fn start(cli: &Path, args: &[&str]) -> Self {
        use std::os::unix::process::CommandExt;
        let mut command = std::process::Command::new(cli);
        command.args(args).stdin(std::process::Stdio::null());
        // A background runner may inherit SIGINT ignored. Restore the owned
        // CLI's interactive disposition without changing the test process.
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
        let child = command.spawn().unwrap();
        let pid = child.id();
        Self {
            child: process::OwnedChild::new(child),
            pid,
        }
    }
    async fn wait(self) -> std::process::ExitStatus {
        let pid = self.pid;
        tokio::task::spawn_blocking(move || process::wait_exit(pid, Duration::from_secs(60)))
            .await
            .unwrap()
            .unwrap();
        self.child.wait_with_output().unwrap().status
    }
}

struct ObservedTmux {
    mock: RealTmuxClient,
    resolved: tokio::sync::mpsc::UnboundedSender<Vec<String>>,
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
                let _ = self
                    .resolved
                    .send(args.iter().map(|arg| (*arg).to_owned()).collect());
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
    for (id, pane) in [
        ("survivor-session", "fixture:1"),
        ("fixture-session", "fixture:1.0"),
    ] {
        registry
            .register(SessionDomain {
                id: SessionId::new(id),
                tmux_pane: Some(pane.into()),
                ..Default::default()
            })
            .await
            .unwrap();
    }
    let (resolved_tx, mut resolved) = tokio::sync::mpsc::unbounded_channel();
    let (control, actor) = spawn_control(
        Box::new(ObservedTmux {
            mock: client.clone(),
            resolved: resolved_tx,
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
    // This test's current-thread runtime keeps the log event and handler
    // return in the same poll; the waiter resumes only after its FDs drop.
    let (disconnected, _logs) = server_event("Client disconnected");
    let work = client.clone();
    let check_root = root.clone();
    let check_identity = identity.clone();
    let probe = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(60), async move {
            let fixture = tempfile::tempdir_in(&check_root).unwrap();
            let input = fixture.path().join("input.bin");
            let bytes = fixture.path().join("bytes.sock");
            let listener = tokio::net::UnixListener::bind(&bytes).unwrap();
            let script = fixture.path().join("reader.py");
            std::fs::write(&script, format!(
                "import os, socket, tty\ns=socket.socket(socket.AF_UNIX)\ns.connect({:?})\ntty.setraw(0)\nwith open({:?}, 'wb', buffering=0) as f:\n s.sendall(b'R')\n while True:\n  b=os.read(0,4096)\n  if not b: break\n  f.write(b)\n  s.sendall(b)\n", bytes, input
            )).unwrap();
            assert_eq!(work.new_window("fixture", Some(&format!("/usr/bin/python3 {}", script.display()))).await.unwrap(), "%1");
            use tokio::io::AsyncReadExt;
            let (mut receiver, _) = listener.accept().await.unwrap();
            assert_eq!(receiver.read_u8().await.unwrap(), b'R');
            let (job, release) = hold(&control, ReservationKind::Delivery).await;
            let survivor = CliChild::start(&cli, &["send", "survivor-session", "SURVIVOR-U8C10"]);
            while !resolved.recv().await.unwrap().iter().any(|arg| arg == "fixture:1") {}
            barrier(&control).await; // survivor parked first: its deadline is earlier
            let baseline = socket_fds();
            let child = CliChild::start(&cli, &["send", "fixture-session", "CANCELLED-U8C9"]);
            // Parked sends re-resolve aliases. Require the interrupted sender's
            // distinct alias, not a leftover notification from the survivor.
            while !resolved.recv().await.unwrap().iter().any(|arg| arg == "fixture:1.0") {}
            barrier(&control).await;
            let interrupted_fds: Vec<_> = socket_fds().into_iter()
                .filter(|fd| !baseline.contains(fd)).collect();
            assert!(!interrupted_fds.is_empty());
            assert!(std::fs::read(&input).unwrap().is_empty());
            assert_eq!(private_identity(&work, &check_root).await, check_identity);
            let pid = child.pid;
            // Direct child owned by this fixture, not a discovered process.
            assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGINT) }, 0);
            assert_eq!(child.wait().await.signal(), Some(libc::SIGINT));
            // Watch only this connection: without the monitor, the earlier
            // survivor's descriptors also disappear before this send times out.
            disconnected.acquire().await.unwrap().forget();
            let remaining = socket_fds();
            assert!(!interrupted_fds.iter().any(|fd| remaining.contains(fd)));
            release.send(()).unwrap();
            job.await.unwrap().unwrap();
            let status = survivor.wait().await;
            assert!(status.success(), "earlier parked survivor must succeed, not time out: {status}");
            control.flush_audits().await;
            let status = CliChild::start(&cli, &["send", "%1", "POSITIVE-U8C9"]).wait().await;
            assert!(status.success());
            // This final positive-control input fences all preceding actor input
            // in the pane's byte stream; require no interrupted-send bytes.
            let expected = b"SURVIVOR-U8C10\rPOSITIVE-U8C9\r";
            let mut delivered = vec![0; expected.len()];
            receiver.read_exact(&mut delivered).await.unwrap();
            assert_eq!(delivered, expected);
            assert_eq!(std::fs::read(&input).unwrap(), expected);
            eprintln!("SIGINT parked CLI: survivor + Enter and positive control only; interrupted send absent");
        }).await.unwrap();
    }).await;
    cancel.cancel();
    actor.await.unwrap();
    server.await.unwrap().unwrap();
    assert_eq!(private_identity(&client, &root).await, identity);
    client.raw_command("kill-server", &[]).await.unwrap();
    probe.unwrap();
}
