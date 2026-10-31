//! The TUI's capture preview follows the selected session's tmux pane, on
//! each event that syncs it: a key, a session update and a full session list.
//!
//! The real `atm` runs in a pane of a private tmux server (`-S` in the test's
//! temp dir, cleared environment) and previews panes that each print their
//! own marker. The daemon is a stand-in on `ATM_SOCKET`, so each event is sent
//! on its own: a real atmd's events come with others, and any key press also
//! syncs, which would hide a missing sync. Nothing reaches a real atmd.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)] // test code may panic

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use atm_core::{SessionId, SessionView};
use atm_protocol::DaemonMessage;

const MARKERS: [&str; 3] = ["preview-marker-a", "preview-marker-b", "preview-marker-c"];

/// The capture task polls once a second, so this allows several polls.
const STEP_TIMEOUT: Duration = Duration::from_secs(10);

/// A private tmux server, killed on drop.
struct Tmux {
    socket: PathBuf,
}

impl Tmux {
    /// `tmux -S <socket>` with an environment holding only `PATH`, so the
    /// agent's `$TMUX` and `$TMUX_PANE` can't redirect a call to its server.
    fn command(&self) -> Command {
        let mut cmd = Command::new("tmux");
        cmd.env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .arg("-S")
            .arg(&self.socket);
        cmd
    }

    /// Runs `args` and returns stdout, panicking when tmux fails.
    fn run(&self, args: &[&str]) -> String {
        let out = self.command().args(args).output().expect("run tmux");
        assert!(
            out.status.success(),
            "tmux {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim_end().to_string()
    }
}

impl Drop for Tmux {
    fn drop(&mut self) {
        let _ = self
            .command()
            .arg("kill-server")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// A stand-in daemon on `socket`. It answers `connect` with Connected and
/// `subscribe` with `sessions`, then hands over the subscriber's stream so
/// the test sends each later event itself.
fn fake_daemon(socket: &Path, sessions: Vec<SessionView>) -> mpsc::Receiver<UnixStream> {
    let listener = UnixListener::bind(socket).expect("bind");
    let connected =
        serde_json::to_string(&DaemonMessage::connected("tui".to_string())).expect("serialize");
    let list = serde_json::to_string(&DaemonMessage::session_list(sessions)).expect("serialize");
    let (subscriber_tx, subscriber_rx) = mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let stream = stream.expect("accept");
            let mut reader = BufReader::new(&stream);
            let mut line = String::new();
            // `atm` first checks that something listens, then hangs up
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                continue;
            }
            (&stream)
                .write_all(format!("{connected}\n").as_bytes())
                .expect("reply Connected");
            // Read Subscribe
            line.clear();
            let _ = reader.read_line(&mut line);
            (&stream)
                .write_all(format!("{list}\n").as_bytes())
                .expect("reply SessionList");
            let _ = subscriber_tx.send(stream);
            return;
        }
    });
    subscriber_rx
}

fn session(id: &str, started_at: &str, pane: Option<&str>) -> SessionView {
    SessionView {
        id: SessionId::new(id),
        id_short: id.to_string(),
        tmux_pane: pane.map(str::to_string),
        started_at: started_at.to_string(),
        project_root: Some("/rig".to_string()),
        working_directory: Some("/rig".to_string()),
        ..SessionView::default()
    }
}

/// Sessions sort newest first, so `session-a`'s row comes before `session-b`'s.
fn session_a(pane: Option<&str>) -> SessionView {
    session("session-a", "2026-01-01T00:00:02Z", pane)
}

fn session_b(pane: Option<&str>) -> SessionView {
    session("session-b", "2026-01-01T00:00:01Z", pane)
}

/// The TUI's pane and the stream that feeds it daemon events.
struct Rig {
    tmux: Tmux,
    tui_pane: String,
    tui_pid: i32,
    daemon: UnixStream,
}

impl Drop for Rig {
    /// The TUI answers a closed daemon stream by starting a real atmd, which
    /// would outlive the test, so it has to die before `daemon` closes.
    fn drop(&mut self) {
        // SAFETY: `kill` takes plain integers and touches no memory.
        unsafe {
            libc::kill(self.tui_pid, libc::SIGKILL);
        }
        let proc_dir = format!("/proc/{}", self.tui_pid);
        let start = Instant::now();
        while Path::new(&proc_dir).exists() && start.elapsed() < STEP_TIMEOUT {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Rig {
    fn screen(&self) -> String {
        self.tmux
            .run(&["capture-pane", "-p", "-t", self.tui_pane.as_str()])
    }

    fn press(&self, key: &str) {
        self.tmux
            .run(&["send-keys", "-t", self.tui_pane.as_str(), key]);
    }

    fn send(&mut self, message: &DaemonMessage) {
        let line = serde_json::to_string(message).expect("serialize");
        self.daemon
            .write_all(format!("{line}\n").as_bytes())
            .expect("write to the TUI");
    }

    /// Waits until the screen satisfies `done`, panicking with the screen at
    /// the deadline.
    fn wait_until(&self, step: &str, done: impl Fn(&str) -> bool) {
        let start = Instant::now();
        loop {
            let screen = self.screen();
            if done(&screen) {
                return;
            }
            assert!(
                start.elapsed() < STEP_TIMEOUT,
                "{step}: timed out, screen:\n{screen}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Waits until the preview shows `marker` and no other pane's marker.
    fn wait_for_preview(&self, step: &str, marker: &str) {
        self.wait_until(step, |screen| {
            MARKERS.iter().all(|m| screen.contains(m) == (*m == marker))
        });
    }
}

#[test]
fn test_capture_preview_follows_the_selected_sessions_pane() {
    if Command::new("tmux")
        .arg("-V")
        .stdout(Stdio::null())
        .status()
        .is_err()
    {
        eprintln!("SKIP: tmux not on PATH");
        return;
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let tmux = Tmux {
        socket: dir.path().join("tmux.sock"),
    };

    // One pane per marker, in a session of their own.
    let mut panes = Vec::new();
    for (i, marker) in MARKERS.iter().enumerate() {
        let print_and_stay = format!("echo {marker}; exec sleep 600");
        let mut args = vec!["-f", "/dev/null"];
        args.extend(if i == 0 {
            ["new-session", "-d", "-s", "agents"]
        } else {
            ["new-window", "-d", "-t", "agents:"]
        });
        args.extend(["-P", "-F", "#{pane_id}", "sh", "-c", &print_and_stay]);
        panes.push(tmux.run(&args));
    }
    let [pane_a, pane_b, pane_c] = [&panes[0], &panes[1], &panes[2]].map(String::as_str);

    let socket = dir.path().join("atmd.sock");
    let subscriber = fake_daemon(
        &socket,
        vec![session_a(Some(pane_a)), session_b(Some(pane_b))],
    );

    // A window as big as the session asks for, so the preview has room.
    let tui_pane = tmux.run(&[
        "new-session",
        "-d",
        "-s",
        "tui",
        "-x",
        "120",
        "-y",
        "40",
        "-e",
        &format!("ATM_SOCKET={}", socket.display()),
        "-e",
        &format!("XDG_STATE_HOME={}", dir.path().join("state").display()),
        "-P",
        "-F",
        "#{pane_id}",
        "env",
        env!("CARGO_BIN_EXE_atm"),
    ]);
    let tui_pid = tmux
        .run(&["display-message", "-p", "-t", &tui_pane, "#{pane_pid}"])
        .parse()
        .expect("pane pid");
    let daemon = subscriber
        .recv_timeout(STEP_TIMEOUT)
        .expect("the TUI subscribes");
    let mut rig = Rig {
        tmux,
        tui_pane,
        tui_pid,
        daemon,
    };
    rig.wait_until("the list arrives", |screen| screen.contains("session-a"));

    // A key: selecting a row targets its pane, and moving on retargets.
    rig.press("j");
    rig.wait_for_preview("key to session-a", MARKERS[0]);
    rig.press("j");
    rig.wait_for_preview("key to session-b", MARKERS[1]);

    // An update that keeps the selected pane leaves the preview alone: it
    // must not blank until the next capture, which a poll would catch.
    let start = Instant::now();
    let mut sent = Duration::ZERO;
    while start.elapsed() < Duration::from_millis(1500) {
        if start.elapsed() >= sent {
            rig.send(&DaemonMessage::session_updated(session_b(Some(pane_b))));
            sent += Duration::from_millis(150);
        }
        let screen = rig.screen();
        assert!(
            screen.contains(MARKERS[1]),
            "same-pane update blanked the preview, screen:\n{screen}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    // A session update, with no key: the selected session moved pane.
    rig.send(&DaemonMessage::session_updated(session_b(Some(pane_c))));
    rig.wait_for_preview("update to pane c", MARKERS[2]);

    // A full list, with no key: the same again.
    rig.send(&DaemonMessage::session_list(vec![
        session_a(Some(pane_a)),
        session_b(Some(pane_b)),
    ]));
    rig.wait_for_preview("list back to pane b", MARKERS[1]);

    // No pane to capture, so nothing would ever replace the old output: only
    // clearing it empties the preview.
    rig.send(&DaemonMessage::session_updated(session_b(None)));
    rig.wait_until("update to no pane", |screen| {
        MARKERS.iter().all(|m| !screen.contains(m)) && !screen.contains(" Terminal ")
    });
}
