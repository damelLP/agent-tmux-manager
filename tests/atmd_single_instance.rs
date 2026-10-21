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

fn wait_until(mut ready: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready() {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    true
}

fn wait_for(what: &str, ready: impl FnMut() -> bool) {
    assert!(wait_until(ready), "timed out waiting for {what}");
}

/// A PID that can't be running: PIDs are always below pid_max.
fn dead_pid() -> String {
    std::fs::read_to_string("/proc/sys/kernel/pid_max")
        .expect("pid_max")
        .trim()
        .to_string()
}

fn wait_exit(child: &mut Child) -> Option<ExitStatus> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match child.try_wait().expect("try_wait") {
            Some(status) => return Some(status),
            None if Instant::now() >= deadline => return None,
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

/// Concurrent `atmd start -d`s, as the TUI's auto-start runs them, leave
/// one daemon, and the PID file names it. The lock is taken before
/// daemonizing, so the losers' exit status reaches the caller instead of
/// being lost in a fork.
fn assert_concurrent_daemonized_starts_leave_one_daemon(stale_pid_file: bool) {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("atmd.sock");
    let state = dir.path().join("state");
    let pid_file = state.join("atm").join("atmd.pid");
    if stale_pid_file {
        std::fs::create_dir_all(state.join("atm")).expect("mkdir state");
        std::fs::write(&pid_file, dead_pid()).expect("write stale PID");
    }

    let starters: Vec<Child> = (0..STARTERS)
        .map(|_| atmd(&socket, &state, &["-d"]).spawn().expect("spawn atmd"))
        .collect();
    let statuses: Vec<ExitStatus> = starters
        .into_iter()
        .map(|mut c| c.wait().expect("wait for starter"))
        .collect();
    // The winner writes its PID before it binds.
    wait_for("the socket", || socket.exists());
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

#[test]
fn test_concurrent_daemonized_starts_leave_one_daemon() {
    assert_concurrent_daemonized_starts_leave_one_daemon(false);
}

/// After a crash the PID file names a dead process, and every starter's
/// stale-PID check wants to remove it.
#[test]
fn test_concurrent_starts_after_a_crash_keep_the_winners_pid_file() {
    assert_concurrent_daemonized_starts_leave_one_daemon(true);
}

/// Only the lock holder may clear a stale PID file. A losing starter that
/// cleared it could delete a winner's fresh PID file and hide the live
/// daemon from `atmd stop`/`status` and from the client's auto-start check.
#[test]
fn test_losing_start_leaves_the_pid_file_alone() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("atmd.sock");
    let state = dir.path().join("state");
    let pid_file = state.join("atm").join("atmd.pid");

    let mut daemon = atmd(&socket, &state, &[]).spawn().expect("spawn atmd");
    wait_for("the socket", || socket.exists());
    // The window a winner is in after locking and before writing its PID:
    // the file still names a dead process.
    std::fs::write(&pid_file, dead_pid()).expect("write stale PID");

    let mut loser = atmd(&socket, &state, &[]).spawn().expect("spawn loser");
    let status = wait_exit(&mut loser);
    let left = std::fs::read_to_string(&pid_file).ok();
    for child in [&mut daemon, &mut loser] {
        stop(child.id());
        let _ = child.wait();
    }

    assert_eq!(status.and_then(|s| s.code()), Some(1), "loser: {status:?}");
    assert_eq!(
        left,
        Some(dead_pid()),
        "the losing starter touched the PID file"
    );
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
    let status = wait_exit(&mut second);
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

/// `daemonize()` changes to `/`. A relative `ATM_SOCKET` must still lock
/// and bind one path, resolved against the directory `atmd` started in.
#[test]
fn test_daemonized_start_with_a_relative_socket_locks_and_binds_one_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    let relative = Path::new("run/atmd.sock");
    let socket = dir.path().join(relative);

    let status = atmd(relative, &dir.path().join("state"), &["-d"])
        .current_dir(dir.path())
        .status()
        .expect("run atmd");
    let bound = wait_until(|| socket.exists());
    let daemons = daemons_for(relative);
    daemons.iter().for_each(|pid| stop(*pid));

    assert!(status.success(), "atmd start -d: {status}");
    assert!(bound, "no socket at {}", socket.display());
    assert!(dir.path().join("run/atmd.sock.lock").exists());
    assert_eq!(daemons.len(), 1, "daemons: {daemons:?}");
}

/// A client whose state dir has no PID file must still see a daemon that
/// already owns the shared socket (the devcontainer case), instead of
/// spawning a duplicate that loses the lock and then giving up.
#[test]
fn test_client_with_other_state_dir_uses_the_running_daemon() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("atmd.sock");
    let mut daemon = atmd(&socket, &dir.path().join("state1"), &[])
        .spawn()
        .expect("spawn atmd");
    wait_for("the socket", || socket.exists());

    let started = Instant::now();
    let list = Command::new(env!("CARGO_BIN_EXE_atm"))
        .args(["list", "--format", "json"])
        .env("ATM_SOCKET", &socket)
        .env("XDG_STATE_HOME", dir.path().join("state2"))
        .output()
        .expect("run atm list");
    let elapsed = started.elapsed();
    let daemons = daemons_for(&socket);
    daemons.iter().for_each(|pid| stop(*pid));
    let _ = daemon.wait();

    assert!(
        list.status.success(),
        "atm list failed: {}",
        String::from_utf8_lossy(&list.stderr)
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "atm list took {elapsed:?}"
    );
    assert_eq!(daemons, vec![daemon.id()], "a second daemon survived");
}
