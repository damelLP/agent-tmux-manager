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

#[path = "support/control.rs"]
mod control_support;

use control_support::process::OwnedChild;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

const STARTERS: usize = 5;

fn atmd(socket: &Path, state: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_atmd"));
    cmd.arg("start")
        .args(args)
        .env("ATM_SOCKET", socket)
        .env("XDG_STATE_HOME", state)
        .env("ATM_STATE_DIR", state.join("eventlog"))
        // atmd reads `[notify]` from atm's config.toml: never the developer's.
        .env("XDG_CONFIG_HOME", state.join("config"))
        .env("RUST_LOG", "warn")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd
}

/// Live atmd processes under this test's unique absolute state root.
fn daemons_for(root: &Path) -> Vec<u32> {
    assert!(
        root.is_absolute(),
        "cleanup requires a unique absolute root"
    );
    let exe = env!("CARGO_BIN_EXE_atmd").as_bytes();
    std::fs::read_dir("/proc")
        .expect("read /proc")
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| {
            let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
            let environ = std::fs::read(format!("/proc/{pid}/environ")).unwrap_or_default();
            cmdline.split(|b| *b == 0).next() == Some(exe)
                && environ.split(|b| *b == 0).any(|value| {
                    value.strip_prefix(b"ATM_STATE_DIR=").is_some_and(|state| {
                        Path::new(std::ffi::OsStr::from_bytes(state)).starts_with(root)
                    })
                })
        })
        .collect()
}

fn stop(pid: u32, root: &Path) {
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if raw < 0 {
        return;
    }
    let fd = unsafe { OwnedFd::from_raw_fd(raw as i32) };
    if !daemons_for(root).contains(&pid) {
        return;
    }
    unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            fd.as_raw_fd(),
            libc::SIGTERM,
            0usize,
            0,
        );
    }
    let mut event = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    if unsafe { libc::poll(&mut event, 1, 30_000) } != 1 && daemons_for(root).contains(&pid) {
        unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                fd.as_raw_fd(),
                libc::SIGKILL,
                0usize,
                0,
            );
        }
        assert_eq!(
            unsafe { libc::poll(&mut event, 1, 30_000) },
            1,
            "owned daemon exit"
        );
    }
    // Foreground reaping belongs to its retained OwnedChild. Reaping here
    // would invalidate that guard's unreaped-PID ownership guarantee.
}

struct DaemonCleanup(PathBuf);
impl Drop for DaemonCleanup {
    fn drop(&mut self) {
        for pid in daemons_for(&self.0) {
            stop(pid, &self.0);
        }
    }
}

fn owned(child: Child) -> (u32, OwnedChild) {
    (child.id(), OwnedChild::new(child))
}

fn start(command: &mut Command, log: &Path) -> (u32, OwnedChild) {
    let mut events = control_support::LogEvents::new(log);
    let output = std::fs::File::create(log).unwrap();
    let child = command
        .stdout(output.try_clone().unwrap())
        .stderr(output)
        .spawn()
        .unwrap();
    let (pid, child) = owned(child);
    events.listening(Some(pid));
    (pid, child)
}

/// A PID that can't be running: PIDs are always below pid_max.
fn dead_pid() -> String {
    std::fs::read_to_string("/proc/sys/kernel/pid_max")
        .expect("pid_max")
        .trim()
        .to_string()
}

fn wait_exit(pid: u32, child: OwnedChild) -> Option<ExitStatus> {
    control_support::process::wait_exit(pid, Duration::from_secs(30)).ok()?;
    Some(child.wait_with_output().expect("reap starter").status)
}

/// Concurrent `atmd start -d`s, as the TUI's auto-start runs them, leave
/// one daemon, and the PID file names it. The lock is taken before
/// daemonizing, so the losers' exit status reaches the caller instead of
/// being lost in a fork.
fn assert_concurrent_daemonized_starts_leave_one_daemon(stale_pid_file: bool) {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("atmd.sock");
    let state = dir.path().join("state");
    let pid_file = state.join("eventlog").join("atmd.pid");
    if stale_pid_file {
        std::fs::create_dir_all(state.join("eventlog")).expect("mkdir state");
        std::fs::write(&pid_file, dead_pid()).expect("write stale PID");
    }

    let _cleanup = DaemonCleanup(dir.path().to_owned());
    let mut events = control_support::LogEvents::new(&state.join("eventlog/atm.log"));
    let starters: Vec<_> = (0..STARTERS)
        .map(|_| owned(atmd(&socket, &state, &["-d"]).spawn().expect("spawn atmd")))
        .collect();
    let statuses: Vec<ExitStatus> = starters
        .into_iter()
        .map(|(pid, child)| wait_exit(pid, child).expect("wait for starter"))
        .collect();
    // Every competing starter has exited; the winner's listening event now
    // fences the one survivor, without a duplicate-appearance quiet interval.
    events.listening(None);
    assert!(control_support::socket_is_ready(&socket));
    let daemons = daemons_for(dir.path());
    let recorded = std::fs::read_to_string(&pid_file).unwrap_or_default();
    daemons.iter().for_each(|pid| stop(*pid, dir.path()));

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
    let pid_file = state.join("eventlog").join("atmd.pid");

    let _cleanup = DaemonCleanup(dir.path().to_owned());
    let (daemon_pid, daemon) = start(
        &mut atmd(&socket, &state, &[]),
        &dir.path().join("foreground.log"),
    );
    // The window a winner is in after locking and before writing its PID:
    // the file still names a dead process.
    std::fs::write(&pid_file, dead_pid()).expect("write stale PID");

    let (loser_pid, loser) = owned(atmd(&socket, &state, &[]).spawn().expect("spawn loser"));
    let status = wait_exit(loser_pid, loser);
    let left = std::fs::read_to_string(&pid_file).ok();
    stop(daemon_pid, dir.path());
    let _ = daemon.wait_with_output();

    assert_eq!(status.and_then(|s| s.code()), Some(1), "loser: {status:?}");
    assert_eq!(
        left,
        Some(dead_pid()),
        "the losing starter touched the PID file"
    );
}

/// `atmd status` and `atmd stop` don't take the lock, so they must not
/// clear a stale PID file either. One that read the stale PID just before
/// a starter wrote its own would delete the starter's fresh PID file.
#[test]
fn test_status_and_stop_leave_a_stale_pid_file_alone() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("atmd.sock");
    let state = dir.path().join("state");
    let pid_file = state.join("eventlog").join("atmd.pid");
    std::fs::create_dir_all(state.join("eventlog")).expect("mkdir state");
    std::fs::write(&pid_file, dead_pid()).expect("write stale PID");

    for command in ["status", "stop"] {
        let output = Command::new(env!("CARGO_BIN_EXE_atmd"))
            .arg(command)
            .env("ATM_SOCKET", &socket)
            .env("XDG_STATE_HOME", &state)
            .env("ATM_STATE_DIR", state.join("eventlog"))
            .output()
            .expect("run atmd");

        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("not running"), "atmd {command}: {stdout}");
        assert_eq!(
            std::fs::read_to_string(&pid_file).ok(),
            Some(dead_pid()),
            "atmd {command} touched the PID file"
        );
    }
}

/// A second atmd with its own state dir but the same socket must not
/// steal the socket from the first.
#[test]
fn test_start_with_other_state_dir_keeps_the_running_daemons_socket() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("atmd.sock");

    let _cleanup = DaemonCleanup(dir.path().to_owned());
    let (first_pid, first) = start(
        &mut atmd(&socket, &dir.path().join("state1"), &[]),
        &dir.path().join("foreground.log"),
    );
    let inode = std::fs::metadata(&socket).expect("socket metadata").ino();

    let (second_pid, second) = owned(
        atmd(&socket, &dir.path().join("state2"), &[])
            .spawn()
            .expect("spawn second atmd"),
    );
    let status = wait_exit(second_pid, second);
    let still_bound = std::fs::metadata(&socket).map(|m| m.ino()).ok();
    stop(first_pid, dir.path());
    let _ = first.wait_with_output();

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

    let _cleanup = DaemonCleanup(dir.path().to_owned());
    let mut events = control_support::LogEvents::new(&dir.path().join("state/eventlog/atm.log"));
    let (starter_pid, starter) = owned(
        atmd(relative, &dir.path().join("state"), &["-d"])
            .current_dir(dir.path())
            .spawn()
            .expect("run atmd"),
    );
    let status = wait_exit(starter_pid, starter).expect("daemonized starter exit");
    events.listening(None);
    let bound = socket.exists();
    let daemons = daemons_for(dir.path());
    daemons.iter().for_each(|pid| stop(*pid, dir.path()));

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
    let _cleanup = DaemonCleanup(dir.path().to_owned());
    let (daemon_pid, daemon) = start(
        &mut atmd(&socket, &dir.path().join("state1"), &[]),
        &dir.path().join("foreground.log"),
    );
    assert!(control_support::socket_is_ready(&socket));

    let started = Instant::now();
    let list = Command::new(env!("CARGO_BIN_EXE_atm"))
        .args(["list", "--format", "json"])
        .env("ATM_SOCKET", &socket)
        .env("XDG_STATE_HOME", dir.path().join("state2"))
        .env("ATM_STATE_DIR", dir.path().join("state2"))
        .env("ATM_TMUX_SOCKET", "atm-single-instance")
        .env("TMUX_TMPDIR", dir.path())
        .output()
        .expect("run atm list");
    let elapsed = started.elapsed();
    let daemons = daemons_for(dir.path());
    daemons.iter().for_each(|pid| stop(*pid, dir.path()));
    let _ = daemon.wait_with_output();

    assert!(
        list.status.success(),
        "atm list failed: {}",
        String::from_utf8_lossy(&list.stderr)
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "atm list took {elapsed:?}"
    );
    assert_eq!(daemons, vec![daemon_pid], "a second daemon survived");
}

/// A unix socket address holds at most 108 bytes (`sun_path`). A short
/// relative `ATM_SOCKET` in a deep working directory binds as given, so a
/// foreground start must not expand it to an absolute path first.
#[test]
fn test_foreground_start_binds_a_short_relative_socket_in_a_deep_dir() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut deep = dir.path().to_path_buf();
    while deep.join("a.sock").as_os_str().len() <= 108 {
        deep.push("deep-working-directory");
    }
    std::fs::create_dir_all(&deep).expect("mkdir deep");
    let socket = deep.join("a.sock");

    let _cleanup = DaemonCleanup(dir.path().to_owned());
    let (daemon_pid, daemon) = start(
        atmd(Path::new("a.sock"), &dir.path().join("state"), &[]).current_dir(&deep),
        &dir.path().join("foreground.log"),
    );
    let bound = socket.exists();
    // One inspection after listening, not a wait used to establish readiness.
    let exited = match control_support::process::wait_exit(daemon_pid, Duration::ZERO) {
        Ok(()) => Some(()),
        Err(error) if error.kind() == std::io::ErrorKind::TimedOut => None,
        Err(error) => panic!("inspect foreground exit: {error}"),
    };
    stop(daemon_pid, dir.path());
    let _ = daemon.wait_with_output();

    assert!(exited.is_none(), "atmd exited: {exited:?}");
    assert!(bound, "no socket at {}", socket.display());
}

// A pinned fallback keeps the survivor probe owned even when its assertion fails.
struct PinnedChild {
    pid: u32,
    child: Option<OwnedChild>,
    pin: OwnedFd,
}

impl PinnedChild {
    fn new((pid, child): (u32, OwnedChild)) -> Self {
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        assert!(raw >= 0);
        Self {
            pid,
            child: Some(child),
            pin: unsafe { OwnedFd::from_raw_fd(raw as i32) },
        }
    }
}

impl Drop for PinnedChild {
    fn drop(&mut self) {
        unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.pin.as_raw_fd(),
                libc::SIGKILL,
                0usize,
                0,
            );
            let mut event = libc::pollfd {
                fd: self.pin.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            libc::poll(&mut event, 1, 30_000);
        }
        if let Some(child) = self.child.take() {
            let _ = child.wait_with_output();
        }
    }
}

#[test]
fn test_cleanup_does_not_stop_a_different_relative_socket_owner() {
    let own = tempfile::tempdir().unwrap();
    let other_root = tempfile::tempdir().unwrap();
    let relative = Path::new("same-relative.sock");
    let own_child = PinnedChild::new(start(
        atmd(relative, &own.path().join("state"), &[]).current_dir(own.path()),
        &own.path().join("daemon.log"),
    ));
    let other = PinnedChild::new(start(
        atmd(relative, &other_root.path().join("state"), &[]).current_dir(other_root.path()),
        &other_root.path().join("daemon.log"),
    ));
    assert_eq!(daemons_for(own.path()), vec![own_child.pid]);
    assert_eq!(daemons_for(other_root.path()), vec![other.pid]);
    {
        let _cleanup = DaemonCleanup(own.path().to_owned());
    }
    let mut own_exit = libc::pollfd {
        fd: own_child.pin.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    assert_eq!(
        unsafe { libc::poll(&mut own_exit, 1, 0) },
        1,
        "owned daemon survived cleanup"
    );
    let mut event = libc::pollfd {
        fd: other.pin.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let exited = unsafe { libc::poll(&mut event, 1, 0) };
    assert_eq!(
        exited, 0,
        "cleanup killed a separate root's daemon sharing only the relative socket spelling"
    );
}

#[test]
fn test_cleanup_reaps_a_foreground_child_that_exited_before_unwind() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("exited.sock");
    let (pid, child) = start(
        &mut atmd(&socket, &dir.path().join("state"), &[]),
        &dir.path().join("daemon.log"),
    );
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    assert!(raw >= 0);
    let pin = unsafe { OwnedFd::from_raw_fd(raw as i32) };
    assert_eq!(
        unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                pin.as_raw_fd(),
                libc::SIGTERM,
                0usize,
                0,
            )
        },
        0
    );
    control_support::process::wait_exit(pid, Duration::from_secs(30)).unwrap();
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap();
    eprintln!(
        "exited child cmdline length={}, matcher={:?}",
        cmdline.len(),
        daemons_for(dir.path())
    );
    assert!(cmdline.is_empty(), "child must have exited before unwind");
    let result = std::panic::catch_unwind(|| {
        let _cleanup = DaemonCleanup(dir.path().to_owned());
        let _child = child;
        panic!("injected assertion failure after an owned child exit");
    });
    assert!(result.is_err());
    let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
    let observed = unsafe {
        libc::waitid(
            libc::P_PIDFD,
            pin.as_raw_fd() as u32,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    let error = std::io::Error::last_os_error().raw_os_error();
    let was_reaped = observed == -1 && error == Some(libc::ECHILD);
    // Reap the deliberately exposed zombie before reporting a RED failure.
    if !was_reaped {
        unsafe {
            libc::waitid(
                libc::P_PIDFD,
                pin.as_raw_fd() as u32,
                &mut info,
                libc::WEXITED | libc::WNOHANG,
            );
        }
    }
    assert!(
        was_reaped,
        "cleanup left its already-exited foreground child unreaped: waitid={observed} errno={error:?}"
    );
}
