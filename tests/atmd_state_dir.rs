//! Real binaries share HOME/XDG_STATE_HOME, but not PID/log/socket state.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use atm_protocol::{ClientMessage, DaemonMessage};
#[path = "support/stand_in.rs"]
mod stand_in;

const DEADLINE: Duration = Duration::from_secs(30);

struct Rig {
    xdg_state: PathBuf,
    root: tempfile::TempDir,
    reapers: Vec<Receiver<std::process::ExitStatus>>,
}

impl Rig {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        for sub in ["home", "xdg", "config", "tmux", "tmp"] {
            std::fs::create_dir(root.path().join(sub)).unwrap();
        }
        Self {
            xdg_state: root.path().join("xdg"),
            root,
            reapers: Vec::new(),
        }
    }

    fn socket(&self, side: &str) -> PathBuf {
        self.root.path().join(format!("{side}.sock"))
    }

    fn command(&self, exe: &str, side: &str, state: Option<&Path>) -> Command {
        let mut command = Command::new(exe);
        command
            .env_clear()
            .current_dir(self.root.path())
            .env("HOME", self.root.path().join("home"))
            .env("XDG_STATE_HOME", &self.xdg_state)
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("TMPDIR", self.root.path().join("tmp"))
            .env("TMUX_TMPDIR", self.root.path().join("tmux"))
            .env("ATM_TMUX_SOCKET", side)
            .env("ATM_SOCKET", self.socket(side))
            .env("PATH", "/usr/local/bin:/usr/bin:/bin");
        if let Some(state) = state {
            command.env("ATM_STATE_DIR", state);
        }
        command
    }

    fn atmd(&self, side: &str, state: Option<&Path>) -> Command {
        self.command(env!("CARGO_BIN_EXE_atmd"), side, state)
    }

    fn start(&mut self, side: &str, state: Option<&Path>) -> u32 {
        let (release, reap_after) = mpsc::channel();
        drop(release);
        self.start_reaping_after(side, state, reap_after)
    }

    fn start_reaping_after(
        &mut self,
        side: &str,
        state: Option<&Path>,
        reap_after: Receiver<()>,
    ) -> u32 {
        let log = std::fs::File::create(self.root.path().join(format!("{side}.stderr"))).unwrap();
        let mut child = self
            .atmd(side, state)
            .arg("start")
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        let pid = child.id();
        let (tx, rx) = mpsc::channel();
        // A held sender delays reaping; dropping it also releases on panic.
        std::thread::spawn(move || {
            let _ = reap_after.recv();
            tx.send(child.wait().unwrap()).ok()
        });
        self.reapers.push(rx);
        self.ready(side);
        pid
    }

    fn ready(&self, side: &str) {
        let deadline = Instant::now() + DEADLINE;
        loop {
            if matches!(
                stand_in::protocol_request(
                    &self.socket(side),
                    ClientMessage::list_sessions(),
                    DEADLINE,
                ),
                Ok(DaemonMessage::SessionList { .. })
            ) {
                return;
            }
            assert!(Instant::now() < deadline, "no round trip on side {side}");
            // Condition poll, not a settling delay: only failed round trips retry.
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn status(&self, side: &str, state: Option<&Path>, pid: Option<u32>) {
        let out = stand_in::bounded::run(self.atmd(side, state).arg("status"));
        assert_eq!(out.status.success(), pid.is_some(), "{out:?}");
        if let Some(pid) = pid {
            assert!(String::from_utf8_lossy(&out.stdout).contains(&format!("(PID {pid})")));
        }
    }

    fn owns(&self, pid: u32) -> bool {
        let expected = format!("HOME={}", self.root.path().join("home").display());
        std::fs::read(format!("/proc/{pid}/environ"))
            .unwrap_or_default()
            .split(|byte| *byte == 0)
            .any(|entry| entry == expected.as_bytes())
            && std::fs::read_link(format!("/proc/{pid}/exe"))
                .ok()
                .as_deref()
                == Some(Path::new(env!("CARGO_BIN_EXE_atmd")))
    }

    fn stop(&self, side: &str, state: Option<&Path>) {
        let legacy = self.xdg_state.join("atm");
        let pid = std::fs::read_to_string(
            self.root
                .path()
                .join(state.unwrap_or(&legacy))
                .join("atmd.pid"),
        )
        .unwrap()
        .parse::<u32>()
        .unwrap();
        assert!(self.owns(pid), "refusing an unowned PID");
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
        assert!(fd >= 0, "pidfd_open: {}", std::io::Error::last_os_error());
        // SAFETY: pidfd_open returned a valid, uniquely owned descriptor.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        assert!(self.owns(pid), "ownership changed before stop");
        let out = stand_in::bounded::run(self.atmd(side, state).arg("stop"));
        assert!(out.status.success(), "{out:?}");
        assert!(String::from_utf8_lossy(&out.stdout).contains(&format!("(PID {pid})")));
        // Exit, not reaping: a non-reaping parent may retain /proc/<pid>.
        let mut poll = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(unsafe { libc::poll(&mut poll, 1, 0) }, 1);
        assert_ne!(poll.revents & libc::POLLIN, 0, "daemon has not exited");
        assert!(!self.socket(side).exists());
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        // Also covers failed assertions and detached autostarts; never trust only
        // the PID-file path whose resolution is precisely what these tests break.
        let entries = std::fs::read_dir("/proc");
        if let Err(error) = &entries {
            eprintln!("cleanup could not read /proc: {error}");
        }
        for entry in entries.into_iter().flatten().flatten() {
            let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse().ok()) else {
                continue;
            };
            if !self.owns(pid) {
                continue;
            }
            // pidfds pin identity across exit/PID reuse. Recheck ownership after
            // opening, and signal through that fd rather than through a stale PID.
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
            if fd < 0 {
                continue;
            }
            if self.owns(pid) {
                unsafe {
                    libc::syscall(libc::SYS_pidfd_send_signal, fd, libc::SIGTERM, 0usize, 0);
                    let mut poll = libc::pollfd {
                        fd,
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    if libc::poll(&mut poll, 1, 30_000) != 1 || poll.revents & libc::POLLIN == 0 {
                        eprintln!("daemon {pid} did not exit on SIGTERM; sending SIGKILL");
                        libc::syscall(libc::SYS_pidfd_send_signal, fd, libc::SIGKILL, 0usize, 0);
                        if libc::poll(&mut poll, 1, 30_000) != 1 || poll.revents & libc::POLLIN == 0
                        {
                            eprintln!("daemon {pid} did not exit on SIGKILL");
                        }
                    }
                }
            }
            unsafe { libc::close(fd) };
        }
        for reaper in &self.reapers {
            if let Err(error) = reaper.recv_timeout(DEADLINE) {
                eprintln!("foreground daemon reaper failed: {error}");
            }
        }
    }
}

// A PID prefix must not let a status assertion pass for the wrong daemon.
#[test]
fn status_rejects_a_prefix_of_the_daemons_pid() {
    let mut rig = Rig::new();
    let state = Path::new("pid-state");
    let pid = rig.start("pid", Some(state));
    rig.status("pid", Some(state), Some(pid));
    let wrong_pid = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        rig.status("pid", Some(state), Some(pid / 10));
    }));
    rig.stop("pid", Some(state));
    assert!(wrong_pid.is_err(), "status accepted a PID prefix");
}

// A failed reaper must not panic in Drop and abort cleanup during unwinding.
#[test]
fn rig_drop_cleans_up_even_with_a_disconnected_reaper() {
    let mut rig = Rig::new();
    rig.start("drop", Some(Path::new("drop-state")));
    let reaped = rig.reapers.pop().unwrap();
    let (tx, rx) = mpsc::channel();
    drop(tx);
    rig.reapers.push(rx);
    let dropped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(rig)));
    assert!(
        reaped.recv_timeout(DEADLINE).is_ok(),
        "daemon was not reaped"
    );
    assert!(dropped.is_ok(), "Drop panicked on a failed reaper");
}

// Counting /proc/<pid> existence as liveness makes stop time out on this
// already-exited child, independently of whether the host's PID 1 reaps.
#[test]
fn stop_succeeds_before_the_parent_reaps_the_daemon() {
    let mut rig = Rig::new();
    // Declared after the rig: on panic this drops before Rig::drop joins reapers.
    let (release, reap_after) = mpsc::channel();
    let state = Path::new("unreaped-state");
    let pid = rig.start_reaping_after("unreaped", Some(state), reap_after);
    rig.stop("unreaped", Some(state));
    assert!(Path::new(&format!("/proc/{pid}")).exists());
    drop(release);
}

// Ignoring ATM_STATE_DIR would put A's PID in the shared XDG location and
// prevent B starting (or make B's status/stop address A).
#[test]
fn separate_state_dirs_isolate_start_status_and_stop() {
    let mut rig = Rig::new();
    let a = Path::new("a-state");
    let b = Path::new("b-state");
    let a_pid = rig.start("a", Some(a));
    assert!(rig.root.path().join(a).join("atmd.pid").is_file());
    assert!(!rig.root.path().join("xdg/atm/atmd.pid").exists());
    rig.status("a", Some(a), Some(a_pid));
    rig.status("b", Some(b), None);
    let b_pid = rig.start("b", Some(b));
    assert_ne!(a_pid, b_pid);
    rig.status("b", Some(b), Some(b_pid));
    rig.stop("b", Some(b));
    rig.ready("a");
    rig.status("a", Some(a), Some(a_pid));
    rig.status("b", Some(b), None);
    rig.stop("a", Some(a));
}

// The client's legacy PID lookup incorrectly treats A as B's running daemon
// and skips the sibling-atmd autostart, even though B's socket has no listener.
#[test]
fn client_autostarts_its_daemon_despite_a_live_legacy_pid() {
    let mut rig = Rig::new();
    let a = Path::new("xdg/atm");
    let b = Path::new("b-state");
    let a_pid = rig.start("a", Some(a));
    assert_eq!(
        std::fs::read_to_string(rig.root.path().join(a).join("atmd.pid")).unwrap(),
        a_pid.to_string()
    );
    let out = stand_in::bounded::run(
        rig.command(env!("CARGO_BIN_EXE_atm"), "b", Some(b))
            .args(["list", "-f", "json"]),
    );
    assert!(out.status.success(), "autostart B: {out:?}");
    rig.ready("b");
    assert!(rig.root.path().join(b).join("atm.log").is_file());
    rig.stop("b", Some(b));
    rig.ready("a");
    rig.status("a", Some(a), Some(a_pid));
    rig.stop("a", Some(a));
}

// Reusing the eventlog resolver for PID/log state would redirect these files
// into relative-xdg/atm, disagreeing with the client's legacy dirs fallback.
#[test]
fn relative_xdg_without_override_keeps_legacy_pid_and_log_paths() {
    let mut rig = Rig::new();
    rig.xdg_state = "relative-xdg".into();
    let state = rig.root.path().join("home/.local/state/atm");
    let out = stand_in::bounded::run(rig.atmd("legacy", None).args(["start", "-d"]));
    assert!(out.status.success(), "{out:?}");
    rig.ready("legacy");
    let pid = std::fs::read_to_string(state.join("atmd.pid"))
        .expect("PID remains at the legacy path")
        .parse()
        .unwrap();
    assert!(state.join("atm.log").is_file());
    assert!(rig
        .root
        .path()
        .join("relative-xdg/atm/log/events/00000001.jsonl")
        .is_file());
    rig.status("legacy", None, Some(pid));
    rig.stop("legacy", Some(&state));
}

// Stop must find the legacy PID when ATM_STATE_DIR is absent, not just when
// the caller explicitly points an override at the legacy directory.
#[test]
fn stop_without_override_uses_the_private_legacy_state_dir() {
    let mut rig = Rig::new();
    let pid = rig.start("legacy-stop", None);
    let pid_file = rig.xdg_state.join("atm/atmd.pid");
    assert_eq!(std::fs::read_to_string(&pid_file).unwrap(), pid.to_string());
    rig.status("legacy-stop", None, Some(pid));
    rig.stop("legacy-stop", None);
    assert!(!pid_file.exists());
    rig.status("legacy-stop", None, None);
}

// Promotion compatibility: without an override (or with an empty override),
// the PID remains at exactly the established XDG path, not a new subdirectory.
#[test]
fn unset_and_empty_override_keep_the_legacy_pid_path() {
    for state in [None, Some(Path::new(""))] {
        let mut rig = Rig::new();
        let pid = rig.start("legacy", state);
        assert_eq!(
            std::fs::read_to_string(rig.root.path().join("xdg/atm/atmd.pid")).unwrap(),
            pid.to_string()
        );
        rig.status("legacy", state, Some(pid));
        rig.stop("legacy", Some(Path::new("xdg/atm")));
    }
}
