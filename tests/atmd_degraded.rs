//! atmd keeps serving when something it depends on misbehaves.
//!
//! Each test runs the real binary in its own temp dir: its own socket and
//! state dir, and tmux calls sent to a `-L` server under its own
//! `TMUX_TMPDIR`. `TMUX` is removed so nothing reaches the developer's
//! tmux server.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)] // test code may panic

use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// `atmd start` in the foreground, isolated under `dir`.
fn atmd(dir: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_atmd"));
    cmd.arg("start")
        .env("ATM_SOCKET", dir.join("atmd.sock"))
        .env("XDG_STATE_HOME", dir.join("state"))
        .env("TMUX_TMPDIR", dir)
        .env("ATM_TMUX_SOCKET", "wedged")
        .env_remove("TMUX")
        .env("RUST_LOG", "warn")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd
}

fn wait_until(mut ready: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready() {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    true
}

/// SIGTERM, then SIGKILL if the daemon doesn't exit. Returns whether
/// SIGTERM was enough.
fn stop(mut daemon: Child) -> bool {
    unsafe {
        libc::kill(daemon.id() as i32, libc::SIGTERM);
    }
    let stopped = wait_until(|| daemon.try_wait().ok().flatten().is_some());
    if !stopped {
        let _ = daemon.kill();
        let _ = daemon.wait();
    }
    stopped
}

/// An accept() that fails with EMFILE leaves the connection in the
/// backlog, so the next accept fails at once. The daemon must pause
/// rather than spin on it, flooding its log and burning a core.
#[test]
fn test_accept_errors_back_off() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("atmd.sock");
    let log = dir.path().join("atmd.log");

    let mut cmd = atmd(dir.path());
    // tracing writes to stdout
    cmd.stdout(std::fs::File::create(&log).expect("create log"));
    // SAFETY: setrlimit is async-signal-safe, as pre_exec requires.
    unsafe {
        cmd.pre_exec(|| {
            let limit = libc::rlimit {
                rlim_cur: 64,
                rlim_max: 64,
            };
            if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let daemon = cmd.spawn().expect("spawn atmd");
    assert!(wait_until(|| socket.exists()), "atmd never bound");

    // Silent clients hold their descriptors until the 10s handshake
    // deadline, so the daemon runs out and the rest wait in the backlog.
    let _clients: Vec<UnixStream> = (0..128)
        .map(|_| UnixStream::connect(&socket).expect("connect"))
        .collect();
    let failures = || {
        std::fs::read_to_string(&log)
            .unwrap_or_default()
            .matches("Failed to accept connection")
            .count()
    };
    let failing = wait_until(|| failures() > 0);
    let before = failures();
    std::thread::sleep(Duration::from_secs(1));
    let in_one_second = failures() - before;
    let stopped = stop(daemon);

    assert!(failing, "accept never failed; is the fd limit too high?");
    assert!(
        in_one_second < 50,
        "{in_one_second} failed accepts logged in one second"
    );
    assert!(stopped, "SIGTERM didn't stop atmd while accepts failed");
}
