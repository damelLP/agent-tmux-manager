//! Private real-binary persistence/startup acceptance tests.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use atm_protocol::ClientMessage;
use serde_json::{json, Value};

struct Daemon {
    dir: tempfile::TempDir,
    child: Option<Child>,
}

impl Daemon {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
            child: None,
        }
    }

    fn socket(&self) -> PathBuf {
        self.dir.path().join("atm.sock")
    }

    fn pid_file(&self) -> PathBuf {
        self.dir.path().join("xdg/atm/atmd.pid")
    }

    fn start(&mut self, daemonized: bool) {
        let mut command = Command::new(env!("CARGO_BIN_EXE_atmd"));
        command.arg("start");
        if daemonized {
            command.arg("-d");
        }
        self.child = Some(
            command
                .current_dir(self.dir.path())
                .env_remove("TMUX")
                .env_remove("TMUX_PANE")
                .env("ATM_SOCKET", self.socket())
                .env("ATM_STATE_DIR", "relative-state")
                .env("ATM_TMUX_SOCKET", "eventlog-test")
                .env("TMUX_TMPDIR", self.dir.path().join("tmux"))
                .env("XDG_STATE_HOME", self.dir.path().join("xdg"))
                .env("XDG_CONFIG_HOME", self.dir.path().join("config"))
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        wait_until(|| self.socket().exists());
        if daemonized {
            let child = self.child.as_mut().unwrap();
            wait_until(|| child.try_wait().unwrap().is_some());
            assert!(child.wait().unwrap().success());
        }
    }

    fn info(&self) -> Value {
        let mut stream = UnixStream::connect(self.socket()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        writeln!(
            stream,
            "{}",
            serde_json::to_string(&ClientMessage::connect(None)).unwrap()
        )
        .unwrap();
        reader.read_line(&mut line).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&line).unwrap()["type"],
            "connected"
        );
        writeln!(
            stream,
            "{}",
            serde_json::to_string(&ClientMessage::request(
                "info".into(),
                "daemon.info".into(),
                json!({}),
                None
            ))
            .unwrap()
        )
        .unwrap();
        line.clear();
        reader.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }

    fn owns(&self, pid: u32) -> bool {
        let expected = format!("ATM_SOCKET={}", self.socket().display());
        let environment = std::fs::read(format!("/proc/{pid}/environ")).unwrap_or_default();
        environment
            .split(|b| *b == 0)
            .any(|entry| entry == expected.as_bytes())
            && std::fs::read_link(format!("/proc/{pid}/exe"))
                .ok()
                .as_deref()
                == Some(std::path::Path::new(env!("CARGO_BIN_EXE_atmd")))
    }

    fn stop(&mut self) {
        if let Some(pid) = std::fs::read_to_string(self.pid_file())
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
        {
            assert!(
                self.owns(pid),
                "refusing to signal a daemon outside this rig"
            );
            // Rechecked immediately before signaling; no shared PID/socket state.
            unsafe {
                libc::kill(pid as i32, libc::SIGTERM);
            }
            wait_until(|| !self.pid_file().exists());
        }
        if let Some(mut child) = self.child.take() {
            if child.try_wait().unwrap().is_none() && self.owns(child.id()) {
                // Covers a failed foreground startup before the PID file exists.
                unsafe {
                    libc::kill(child.id() as i32, libc::SIGTERM);
                }
            }
            wait_until(|| child.try_wait().unwrap().is_some());
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.stop();
    }
}

fn wait_until(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready() {
        assert!(Instant::now() < deadline, "private daemon timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn info_resolves_relative_state_dir_before_foreground_or_daemonized_start() {
    for daemonized in [false, true] {
        let mut daemon = Daemon::new();
        daemon.start(daemonized);
        let info = daemon.info();
        let expected = daemon.dir.path().join("relative-state");
        assert_eq!(info["result"]["state_dir"], json!(expected));
        // Event-log state must not move the established PID/log locations.
        assert!(daemon.pid_file().exists());
        if daemonized {
            assert!(daemon.dir.path().join("xdg/atm/atm.log").exists());
        }
        daemon.stop();
    }
}
