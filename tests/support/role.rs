//! Shared isolated CLI rig: sibling-atmd trap, connection trap and bounded exec.

use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

pub fn executable(path: &Path, body: &str) {
    std::fs::write(path, body).expect("write executable");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
}

pub fn rig() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).expect("mkdir bin");
    let exe = bin.join("atm");
    if std::fs::hard_link(env!("CARGO_BIN_EXE_atm"), &exe).is_err() {
        std::fs::copy(env!("CARGO_BIN_EXE_atm"), &exe).expect("copy atm");
    }
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
    // The real daemon can discover many sessions. Files let it finish a
    // large JSON list while we poll for exit, without filling a pipe.
    let mut stdout = tempfile::tempfile().expect("stdout file");
    let mut stderr = tempfile::tempfile().expect("stderr file");
    let mut tries = 0;
    let mut child = loop {
        match cmd
            .stdin(Stdio::null())
            .stdout(stdout.try_clone().expect("clone stdout"))
            .stderr(stderr.try_clone().expect("clone stderr"))
            .spawn()
        {
            // As in atm_cli_daemon.rs: a concurrent fork may inherit a
            // freshly copied executable's write fd. Retry only this error.
            Err(e) if e.kind() == io::ErrorKind::ExecutableFileBusy && tries < 50 => {
                tries += 1;
                std::thread::sleep(Duration::from_millis(20));
            }
            result => break result.expect("spawn command"),
        }
    };
    let deadline = Instant::now() + Duration::from_secs(8);
    while child.try_wait().expect("poll command").is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            child.wait().expect("reap timed out command");
            panic!("command timed out: {cmd:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let mut output = Output {
        status: child.wait().expect("command status"),
        stdout: Vec::new(),
        stderr: Vec::new(),
    };
    stdout.seek(SeekFrom::Start(0)).unwrap();
    stderr.seek(SeekFrom::Start(0)).unwrap();
    stdout.read_to_end(&mut output.stdout).unwrap();
    stderr.read_to_end(&mut output.stderr).unwrap();
    output
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
