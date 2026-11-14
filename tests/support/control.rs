//! Private real daemon for control CLI tests. Never inherits live sockets or tmux.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub fn isolated<'a>(command: &'a mut Command, dir: &Path) -> &'a mut Command {
    command
        .env_clear()
        .env("HOME", dir)
        .env("ATM_SOCKET", dir.join("atmd.sock"))
        .env("ATM_STATE_DIR", dir.join("events"))
        .env("XDG_STATE_HOME", dir.join("state"))
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("TMPDIR", dir)
        .env("TMUX_TMPDIR", dir)
        .env("ATM_TMUX_SOCKET", "control-test")
}

pub struct Daemon {
    child: Child,
    dir: PathBuf,
}

impl Daemon {
    pub fn start(dir: &Path, path: &Path) -> Self {
        let log = std::fs::File::create(dir.join("daemon.log")).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_atmd"));
        isolated(&mut command, dir)
            .env("PATH", path)
            .args(["start"])
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log);
        let child = command.spawn().unwrap();
        let mut daemon = Self {
            child,
            dir: dir.to_owned(),
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while !dir.join("atmd.sock").exists() {
            assert!(
                daemon.child.try_wait().unwrap().is_none(),
                "daemon exited: {}",
                std::fs::read_to_string(dir.join("daemon.log")).unwrap()
            );
            assert!(Instant::now() < deadline, "daemon startup timed out");
            std::thread::sleep(Duration::from_millis(10));
        }
        daemon
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if self.child.try_wait().unwrap().is_some() {
            return;
        }
        assert_owned(self.child.id(), &self.dir);
        unsafe {
            libc::kill(self.child.id() as i32, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                assert_owned(self.child.id(), &self.dir);
                self.child.kill().unwrap();
                self.child.wait().unwrap();
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

pub fn assert_owned(pid: u32, dir: &Path) {
    let exe = std::fs::read_link(format!("/proc/{pid}/exe")).unwrap();
    assert_eq!(
        exe,
        std::fs::canonicalize(env!("CARGO_BIN_EXE_atmd")).unwrap()
    );
    let env = std::fs::read(format!("/proc/{pid}/environ")).unwrap();
    let socket = format!("ATM_SOCKET={}", dir.join("atmd.sock").display());
    assert!(env.split(|b| *b == 0).any(|v| v == socket.as_bytes()));
}
