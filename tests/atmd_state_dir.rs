//! Real binaries share HOME/XDG_STATE_HOME, but not PID/log/socket state.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

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
        // Reap concurrently: `atmd stop` waits for /proc/<pid> to disappear.
        std::thread::spawn(move || tx.send(child.wait().unwrap()).ok());
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
            assert!(String::from_utf8_lossy(&out.stdout).contains(&format!("PID {pid}")));
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

    fn stop(&self, side: &str, state: &Path) {
        let pid = std::fs::read_to_string(self.root.path().join(state).join("atmd.pid"))
            .unwrap()
            .parse::<u32>()
            .unwrap();
        assert!(self.owns(pid), "refusing an unowned PID");
        let out = stand_in::bounded::run(self.atmd(side, Some(state)).arg("stop"));
        assert!(out.status.success(), "{out:?}");
        assert!(String::from_utf8_lossy(&out.stdout).contains(&format!("PID {pid}")));
        assert!(!Path::new(&format!("/proc/{pid}")).exists());
        assert!(!self.socket(side).exists());
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        // Also covers failed assertions and detached autostarts; never trust only
        // the PID-file path whose resolution is precisely what these tests break.
        for entry in std::fs::read_dir("/proc").unwrap().flatten() {
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
                    assert_eq!(libc::poll(&mut poll, 1, 30_000), 1, "daemon did not exit");
                }
            }
            unsafe { libc::close(fd) };
        }
        for reaper in &self.reapers {
            reaper
                .recv_timeout(DEADLINE)
                .expect("foreground daemon exit");
        }
    }
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
    rig.stop("b", b);
    rig.ready("a");
    rig.status("a", Some(a), Some(a_pid));
    rig.status("b", Some(b), None);
    rig.stop("a", a);
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
    rig.stop("b", b);
    rig.ready("a");
    rig.status("a", Some(a), Some(a_pid));
    rig.stop("a", a);
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
    rig.stop("legacy", &state);
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
        rig.stop("legacy", Path::new("xdg/atm"));
    }
}
