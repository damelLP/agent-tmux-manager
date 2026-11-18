//! Shared isolated CLI rig: sibling-atmd trap, connection trap and bounded exec.

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::{Command, Output};

#[allow(dead_code)]
#[path = "stand_in/process.rs"]
pub mod process;

pub fn executable(path: &Path, body: &str) {
    std::fs::write(path, body).expect("write executable");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
}

/// Keep native-executable fixtures on the artifact's filesystem: linking opens
/// no writer that a concurrent libtest fork could inherit (ETXTBSY).
pub fn artifact_dir() -> tempfile::TempDir {
    tempfile::tempdir_in(Path::new(env!("CARGO_BIN_EXE_atm")).parent().unwrap())
        .expect("fixture beside built atm")
}

pub fn rig() -> tempfile::TempDir {
    let dir = artifact_dir();
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).expect("mkdir bin");
    let exe = bin.join("atm");
    std::fs::hard_link(env!("CARGO_BIN_EXE_atm"), &exe).expect("link atm");
    executable(
        &bin.join("atmd"),
        "#!/bin/sh\necho attempted > \"$HOME/daemon-started\"\nexit 1\n",
    );
    dir
}

pub fn command(dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(dir.join("bin/atm"));
    cmd.args(args)
        .env_clear()
        .env("PATH", dir.join("bin"))
        .env("HOME", dir)
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("XDG_STATE_HOME", dir.join("state"))
        .env("ATM_SOCKET", dir.join("atmd.sock"))
        .env("TMUX_TMPDIR", dir);
    cmd
}

pub fn run(cmd: &mut Command) -> Output {
    process::run(cmd)
}

pub fn connection_trap(dir: &Path) -> UnixListener {
    let listener = UnixListener::bind(dir.join("atmd.sock")).expect("bind trap");
    listener.set_nonblocking(true).expect("nonblocking");
    listener
}

pub fn assert_no_daemon(dir: &Path, listener: Option<&UnixListener>) {
    assert!(!dir.join("daemon-started").exists(), "atmd was started");
    assert!(!dir.join("state/atm/atmd.pid").exists(), "daemon pid file");
    if let Some(listener) = listener {
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock,
            "atm contacted the daemon socket"
        );
    } else {
        assert!(!dir.join("atmd.sock").exists(), "daemon socket appeared");
    }
}
