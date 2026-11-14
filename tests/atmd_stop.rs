//! Real daemon exit, not socket disappearance, PID-file cleanup, or reaping.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead, BufReader};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use atm_protocol::{ClientMessage, DaemonMessage};
#[path = "support/stand_in.rs"]
mod stand_in;

const GUARD: Duration = Duration::from_secs(30);

// Direct, unreaped children pin their PID until wait; cleanup always resumes a
// stopped child before terminating it, including assertion-failure paths.
struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(None)) {
            unsafe {
                libc::kill(self.0.id() as i32, libc::SIGCONT);
                libc::kill(self.0.id() as i32, libc::SIGKILL);
            }
            let _ = self.0.wait();
        }
    }
}

fn command(root: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_atmd"));
    cmd.env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("HOME", root)
        .env("SHELL", "/bin/sh")
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("ATM_STATE_DIR", root.join("state"))
        .env("ATM_SOCKET", root.join("a.sock"))
        .env("ATM_TMUX_SOCKET", "stop-test")
        .env("TMUX_TMPDIR", root.join("tmux"))
        .env("TMPDIR", root)
        .stdin(Stdio::null());
    cmd
}

fn exited(fd: &OwnedFd) -> bool {
    let mut poll = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    assert!(unsafe { libc::poll(&mut poll, 1, 0) } >= 0);
    poll.revents & libc::POLLIN != 0
}

fn held_daemon_stop(remove_files: bool) {
    let root = tempfile::tempdir().unwrap();
    let log = std::fs::File::create(root.path().join("daemon.log")).unwrap();
    let mut start = command(root.path());
    start
        .arg("start")
        .stdout(log.try_clone().unwrap())
        .stderr(log);
    // SAFETY: setsid/prctl do not allocate or lock after fork.
    // Isolate from tmux's resume signals, but kill even a stopped daemon if the
    // test thread dies before ChildGuard can run (e.g. an interrupted test run).
    unsafe {
        start.pre_exec(|| {
            if libc::setsid() < 0 || libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let mut daemon = ChildGuard(start.spawn().unwrap());
    let pid = daemon.0.id();
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
    assert!(fd >= 0);
    // SAFETY: syscall returned a new owned descriptor.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let deadline = std::time::Instant::now() + GUARD;
    loop {
        if matches!(
            stand_in::protocol_request(
                &root.path().join("a.sock"),
                ClientMessage::list_sessions(),
                GUARD,
            ),
            Ok(DaemonMessage::SessionList { .. })
        ) {
            break;
        }
        assert!(!exited(&fd), "daemon exited before readiness");
        assert!(std::time::Instant::now() < deadline, "daemon not ready");
        // Only failed protocol round trips retry: a condition poll, not settling.
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(unsafe { libc::getsid(pid as i32) }, pid as i32);
    assert_eq!(
        std::fs::read_link(format!("/proc/{pid}/exe")).unwrap(),
        Path::new(env!("CARGO_BIN_EXE_atmd"))
    );
    let expected = format!("ATM_SOCKET={}", root.path().join("a.sock").display());
    assert!(std::fs::read(format!("/proc/{pid}/environ"))
        .unwrap()
        .split(|byte| *byte == 0)
        .any(|entry| entry == expected.as_bytes()));
    assert_eq!(
        unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                fd.as_raw_fd(),
                libc::SIGSTOP,
                0usize,
                0,
            )
        },
        0
    );
    let mut status = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
    // Observe exit too, but leave all reaping to Child so its guard retains ownership.
    assert_eq!(
        unsafe {
            libc::waitid(
                libc::P_PID,
                pid,
                &mut status,
                libc::WSTOPPED | libc::WEXITED | libc::WNOWAIT,
            )
        },
        0
    );
    assert_eq!(status.si_code, libc::CLD_STOPPED);

    let mut stop = ChildGuard(
        command(root.path())
            .arg("stop")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let output = stop.0.stdout.take().unwrap();
    let (send, lines) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(output).lines() {
            if send.send(line.unwrap()).is_err() {
                break;
            }
        }
    });
    assert_eq!(
        lines.recv_timeout(GUARD).unwrap(),
        format!("Stopping daemon (PID {pid})...")
    );
    if remove_files {
        std::fs::remove_file(root.path().join("state/atmd.pid")).unwrap();
        std::fs::remove_file(root.path().join("a.sock")).unwrap();
    }
    // The progress event releases the daemon, never a sleep or wall-time assert.
    assert_eq!(
        lines
            .recv_timeout(GUARD)
            .expect("stop exited without progress"),
        "Daemon is still stopping; waiting for shutdown to finish..."
    );
    assert!(!exited(&fd), "stopped daemon somehow exited before resume");
    assert_eq!(
        unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                fd.as_raw_fd(),
                libc::SIGCONT,
                0usize,
                0,
            )
        },
        0
    );
    assert_eq!(lines.recv_timeout(GUARD).unwrap(), "Daemon stopped.");
    assert!(exited(&fd), "success preceded real process exit");
    assert!(stop.0.wait().unwrap().success());
    reader.join().unwrap();
    assert!(lines.try_recv().is_err(), "extra progress line");
    assert!(daemon.0.wait().unwrap().success());
    assert!(!root.path().join("a.sock").exists());
    assert!(!root.path().join("state/atmd.pid").exists());
}

#[test]
fn real_daemon_stop_waits_through_signal_delivery_stall() {
    held_daemon_stop(false);
}

#[test]
fn stop_does_not_confuse_removed_files_with_exit() {
    held_daemon_stop(true);
}
