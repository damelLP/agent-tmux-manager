//! Only one atmd may own a socket.
//!
//! Several `atm` processes can each spawn `atmd start -d` at once (for
//! example, workspace create starting one sidebar per window), and an atmd
//! with another state dir can share `ATM_SOCKET` (a devcontainer with the
//! socket dir bind-mounted). The PID-file check catches neither, and every
//! daemon that got past it unlinked and rebound the socket, orphaning the
//! earlier ones. The losers must exit 1 without touching the socket or the
//! PID file.
//!
//! Daemonized processes are found through `/proc`, so this is Linux-only.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)] // test code may panic

use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

const STARTERS: usize = 5;

fn atmd(socket: &Path, state: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_atmd"));
    cmd.arg("start")
        .args(args)
        .env("ATM_SOCKET", socket)
        .env("XDG_STATE_HOME", state)
        .env("RUST_LOG", "warn")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd
}

/// Live atmd processes started with this `ATM_SOCKET`.
fn daemons_for(socket: &Path) -> Vec<u32> {
    let exe = env!("CARGO_BIN_EXE_atmd").as_bytes();
    let env_var = format!("ATM_SOCKET={}", socket.display()).into_bytes();
    std::fs::read_dir("/proc")
        .expect("read /proc")
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| {
            let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
            let environ = std::fs::read(format!("/proc/{pid}/environ")).unwrap_or_default();
            cmdline.starts_with(exe) && environ.split(|b| *b == 0).any(|v| v == env_var)
        })
        .collect()
}

fn stop(pid: u32) {
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }
}

fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Concurrent `atmd start -d`s, as the TUI's auto-start runs them, leave
/// one daemon. The lock is taken before daemonizing, so the losers'
/// exit status reaches the caller instead of being lost in a fork.
#[test]
fn test_concurrent_daemonized_starts_leave_one_daemon() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("atmd.sock");
    let state = dir.path().join("state");
    let pid_file = state.join("atm").join("atmd.pid");

    let starters: Vec<Child> = (0..STARTERS)
        .map(|_| atmd(&socket, &state, &["-d"]).spawn().expect("spawn atmd"))
        .collect();
    let statuses: Vec<ExitStatus> = starters
        .into_iter()
        .map(|mut c| c.wait().expect("wait for starter"))
        .collect();
    wait_for("the PID file", || pid_file.exists());
    // Give a duplicate time to show up as a second survivor.
    std::thread::sleep(Duration::from_secs(1));
    let daemons = daemons_for(&socket);
    let recorded = std::fs::read_to_string(&pid_file).unwrap_or_default();
    daemons.iter().for_each(|pid| stop(*pid));

    assert_eq!(daemons.len(), 1, "daemons left running: {daemons:?}");
    assert_eq!(recorded.trim(), daemons[0].to_string());
    let failed = statuses.iter().filter(|s| s.code() == Some(1)).count();
    assert_eq!(failed, STARTERS - 1, "starter statuses: {statuses:?}");
}

/// A second atmd with its own state dir but the same socket must not
/// steal the socket from the first.
#[test]
fn test_start_with_other_state_dir_keeps_the_running_daemons_socket() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("atmd.sock");

    let mut first = atmd(&socket, &dir.path().join("state1"), &[])
        .spawn()
        .expect("spawn first atmd");
    wait_for("the socket", || socket.exists());
    let inode = std::fs::metadata(&socket).expect("socket metadata").ino();

    let mut second = atmd(&socket, &dir.path().join("state2"), &[])
        .spawn()
        .expect("spawn second atmd");
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        match second.try_wait().expect("try_wait") {
            Some(status) => break Some(status),
            None if Instant::now() >= deadline => break None,
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    };
    let still_bound = std::fs::metadata(&socket).map(|m| m.ino()).ok();
    for child in [&mut first, &mut second] {
        stop(child.id());
        let _ = child.wait();
    }

    assert_eq!(
        status.and_then(|s| s.code()),
        Some(1),
        "second atmd: {status:?}"
    );
    assert_eq!(
        still_bound,
        Some(inode),
        "the second atmd rebound the socket"
    );
}
