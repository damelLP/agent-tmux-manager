//! End-to-end harness exercising real binaries against a real tmux server.
//!
//! Six shared-fixture scenarios and a daemon-free role test are covered:
//!
//! **Coordinator roles**: a separate private-server test checks set/clear,
//! caller-pane targeting and missing panes without starting/contacting atmd.
//! A relocated atm has a sibling-atmd trap (not just a PATH trap), and a
//! listener catches even readiness probes. Copied executables retry ETXTBSY.
//! The real-daemon scenario also checks JSON coordinator/null roles.
//!
//! 1. **Protocol-level e2e**: spawn `atmd` as a child, hand-roll a client over
//!    a Unix socket, register a session via `HookEvent`, observe via
//!    `ListSessions`. This is the wire-protocol regression net.
//!
//! 2. **Real `atm` client**: shell out to the actual `atm` binary
//!    (`atm list --format json`), pointing it at our test daemon via
//!    `ATM_SOCKET`. Asserts the test session is present in the parsed
//!    JSON output. This validates the full client → daemon → JSON path.
//!
//! 3. **Real tmux library against an isolated server**: drive
//!    `atm_tmux::RealTmuxClient::with_socket(label)` against the private
//!    tmux server we spun up. Splits a pane, sends keys, captures output.
//!    Pastes into panes running `cat` through a named buffer: a trailing
//!    `;` and a newline arrive intact, `-p` brackets the paste only when
//!    the program asked for it, and no buffer is left behind, even after a
//!    failed paste. An empty paste is a no-op, and a paste with no server
//!    reports tmux's error. Then the real `atm send` pastes to those panes
//!    and refuses copy mode, and multi-line text to `cat` from tmux 3.7
//!    (older tmux can't report bracketed paste).
//!
//! 4. **`atm spawn` with a `claude` fixture**: run the actual `atm spawn`
//!    subcommand against our isolated tmux server (via `ATM_TMUX_SOCKET`),
//!    using a Python shim injected by absolute path through
//!    `ATM_SPAWN_BIN`. (Earlier iterations tried prepending the
//!    shim dir to `PATH`, but interactive shell init re-derives `PATH`
//!    from scratch and dropped the prepended dir. Absolute-path
//!    injection sidesteps that entirely.) The shim connects to atmd
//!    over its socket, sends a `UserPromptSubmit` hook event carrying
//!    `$TMUX_PANE`, and stays alive until pane teardown. `atm spawn` must print the
//!    pane it created, and its `--prompt` must reach the shim as its one
//!    argument (the shim sends its argv as the prompt). A subscription established
//!    before spawn observes the committed session, then `atm list --format json`
//!    checks that its `tmux_pane` matches the pane atm spawn created. This validates the entire
//!    spawn → tmux split → hook event → registry update loop. A second
//!    spawn with no `-t` and `$TMUX_PANE` set to the probe pane must split
//!    that pane, where the old first-active-pane fallback would split
//!    another, and print the new pane.
//!
//! 5. **`atm workspace create` / `atm workspace attach`**: verifies both
//!    workspace flows install the resize and new-window hooks. The create
//!    scenario opens detached and selected windows and checks the hook puts a
//!    sidebar only in each new window, sizes it there and restores its original
//!    pane without switching the client's window; attach starts from a
//!    multi-window session and asserts every window gets exactly one sidebar, including on a
//!    second idempotency run. Both keep their scripts in a data dir whose
//!    path has a space, quotes, `$` and `#`, so every hook, prefix-R and
//!    attach's own resize run must quote it. A no-name attach that
//!    auto-picks a session named "my work" must fail before installing
//!    anything. Create runs the configured default harness in the agent
//!    pane (a stand-in `pi` named by an isolated config), and an unknown
//!    default fails before tmux runs, leaving nothing behind. An
//!    `ATM_SPAWN_*_ARGS` override ending in `;` still launches it. With no
//!    config, create types exactly `claude`, then `Enter` on its own; a
//!    logging `tmux` on atm's PATH swallows that `Enter`, so nothing runs.
//!
//! 6. **The TUI's capture preview**: the real `atm` runs in a pane and
//!    previews panes that each print their own marker. Its daemon is a
//!    stand-in, so a key, a session update, a full session list and a removal
//!    each fire on their own: a real atmd's events come with others, which
//!    could hide one that leaves the preview behind. A second TUI runs under
//!    `--tmux-session`, whose filter hides the selected session once its pane
//!    leaves that tmux session.
//!
//! The test skips cleanly (printed message, returns Ok) when `tmux` is
//! absent from PATH, when the binaries weren't built, or — for scenario
//! 4 only — when `python3` is unavailable.
//!
//! Per CLAUDE.md, tests are an explicit `unwrap()`/`expect()`-allowed zone.
//!
//! The harness is Unix-only (uses `std::os::unix` and SIGTERM via libc).
//! That's not a real loss of coverage — the production code is Unix-only
//! too (atmd uses libc::kill, the daemonize crate, /proc walking) — but
//! gating the file lets `cargo check`/`cargo test` pass cleanly on
//! Windows runners instead of spewing missing-symbol errors.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)] // test code may panic

use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use atm_core::{SessionId, SessionView};
use atm_protocol::{ClientMessage, DaemonMessage, ProtocolVersion};
use atm_tmux::{PaneDirection, RealTmuxClient, TmuxClient};
use serde_json::{json, Value};
use tempfile::TempDir;

#[path = "support/role.rs"]
mod role_support;

const HANG_GUARD: Duration = Duration::from_secs(60);
/// Wait on SIGCHLD, not elapsed time. Subscribe before checking the child so
/// an exit between try_wait and recv is retained. Every wake uses the same guard.
fn wait_child(child: &mut Child, deadline: Instant) -> io::Result<ExitStatus> {
    let result = std::thread::scope(|scope| {
        std::thread::Builder::new()
            .spawn_scoped(scope, || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?
                    .block_on(async {
                        let mut exited =
                            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::child())?;
                        loop {
                            if let Some(status) = child.try_wait()? {
                                return Ok(status);
                            }
                            if Instant::now() >= deadline || !matches!(
                                tokio::time::timeout_at(deadline.into(), exited.recv()).await,
                                Ok(Some(()))
                            ) {
                                return Err(io::Error::new(
                                    io::ErrorKind::TimedOut,
                                    "child exit guard expired or signal stream closed (killed and reaped)",
                                ));
                            }
                        }
                    })
            })?
            .join()
            .unwrap_or_else(|_| Err(io::Error::other("child waiter thread panicked")))
    });
    // Setup/signal errors must reap too, not only the normal timeout branch.
    if let Err(error) = &result {
        eprintln!("child {} waiter: {error}; killing and reaping", child.id());
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}

fn tmux_command() -> Command {
    let mut command = Command::new("tmux");
    // Without -u, tmux maps tabs to underscores for a non-UTF-8 client (#138).
    command.arg("-u").env_remove("TMUX").env_remove("TMUX_PANE");
    command
}

/// Files avoid a full stdout/stderr pipe preventing the exit we await.
fn command_output(command: &mut Command) -> Output {
    try_command_output(command).unwrap_or_else(|error| panic!("{command:?}: {error}"))
}

fn try_command_output(command: &mut Command) -> io::Result<Output> {
    let deadline = Instant::now() + HANG_GUARD;
    let mut stdout = tempfile::tempfile()?;
    let mut stderr = tempfile::tempfile()?;
    let mut child = command
        .stdout(stdout.try_clone()?)
        .stderr(stderr.try_clone()?)
        .spawn()?;
    let status = wait_child(&mut child, deadline);
    stdout.seek(SeekFrom::Start(0))?;
    stderr.seek(SeekFrom::Start(0))?;
    let mut out = Vec::new();
    let mut err = Vec::new();
    stdout.read_to_end(&mut out)?;
    stderr.read_to_end(&mut err)?;
    Ok(Output {
        status: status.map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "{error}\nstdout: {}\nstderr: {}",
                    String::from_utf8_lossy(&out),
                    String::from_utf8_lossy(&err)
                ),
            )
        })?,
        stdout: out,
        stderr: err,
    })
}

/// Kernel fd readiness is an event; poll(2) blocks, without a timer retry loop.
fn readable(fd: RawFd, deadline: Instant) -> io::Result<()> {
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "readiness guard expired"))?;
        let mut poll = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // fd is borrowed from an open listener/FIFO throughout this call.
        let result = unsafe {
            libc::poll(
                &mut poll,
                1,
                remaining.as_millis().max(1).try_into().unwrap_or(i32::MAX),
            )
        };
        if result > 0 {
            return Ok(());
        }
        if result == 0 {
            return Err(io::ErrorKind::TimedOut.into());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn connect_until(path: &Path, deadline: Instant) -> io::Result<UnixStream> {
    if Instant::now() >= deadline {
        return Err(io::ErrorKind::TimedOut.into());
    }
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .spawn_scoped(scope, || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?
                    .block_on(async {
                        let connected = tokio::time::timeout_at(
                            deadline.into(),
                            tokio::net::UnixStream::connect(path),
                        )
                        .await
                        .map_err(|error| io::Error::new(io::ErrorKind::TimedOut, error))??;
                        let stream = connected.into_std()?;
                        stream.set_nonblocking(false)?;
                        Ok(stream)
                    })
            })?
            .join()
            .unwrap_or_else(|_| Err(io::Error::other("socket connector thread panicked")))
    })
}

/// A trickle of partial frames must not renew the operation's hang guard.
fn read_frame(reader: &mut BufReader<UnixStream>, deadline: Instant) -> io::Result<String> {
    let mut line = Vec::new();
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "frame guard expired"))?;
        reader.get_ref().set_read_timeout(Some(remaining))?;
        let available = match reader.fill_buf() {
            Ok(available) => available,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "frame guard timed out",
                ));
            }
            Err(error) => return Err(error),
        };
        if available.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "peer closed before a frame",
            ));
        }
        let end = available.iter().position(|byte| *byte == b'\n');
        let count = end.map_or(available.len(), |pos| pos + 1);
        line.extend_from_slice(&available[..count]);
        reader.consume(count);
        if end.is_some() {
            return String::from_utf8(line)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e));
        }
    }
}

fn write_bytes(stream: &mut UnixStream, mut bytes: &[u8], deadline: Instant) -> io::Result<()> {
    while !bytes.is_empty() {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "write guard expired"))?;
        stream.set_write_timeout(Some(remaining))?;
        let written = match stream.write(bytes) {
            Ok(written) => written,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "write guard timed out",
                ));
            }
            Err(error) => return Err(error),
        };
        if written == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        bytes = &bytes[written..];
    }
    Ok(())
}

/// cat writes into a FIFO, so exact-byte assertions wait for fd readiness rather
/// than checking a regular file on a timer. The idle writer prevents premature
/// EOF before the pane opens its end; both descriptors close with this fixture.
struct ByteSink {
    reader: std::fs::File,
    _writer: std::fs::File,
    bytes: Vec<u8>,
}

impl ByteSink {
    fn new(path: &Path) -> Self {
        let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(
            unsafe { libc::mkfifo(name.as_ptr(), 0o600) },
            0,
            "create FIFO: {}",
            io::Error::last_os_error()
        );
        let reader = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
            .unwrap();
        let writer = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
            .unwrap();
        Self {
            reader,
            _writer: writer,
            bytes: Vec::new(),
        }
    }

    fn read_through(&mut self, want: &[u8]) -> Vec<u8> {
        let deadline = Instant::now() + HANG_GUARD;
        let mut buffer = [0; 4096];
        loop {
            assert!(
                Instant::now() < deadline,
                "pane byte guard timed out; expected: {}; actual: {}",
                want.escape_ascii(),
                self.bytes.escape_ascii()
            );
            match self.reader.read(&mut buffer) {
                Ok(0) => panic!("FIFO unexpectedly closed"),
                Ok(count) => self.bytes.extend_from_slice(&buffer[..count]),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if self.bytes.len() >= want.len() {
                        return self.bytes.clone();
                    }
                    readable(self.reader.as_raw_fd(), deadline).unwrap_or_else(|error| {
                        panic!(
                            "pane byte guard: {error}; expected: {}; actual: {}",
                            want.escape_ascii(),
                            self.bytes.escape_ascii()
                        );
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => panic!("read pane bytes: {error}"),
            }
        }
    }
}

/// tmux 3.6 and 3.8 retain a signal sent before its waiter (verified in private
/// servers). Each operation uses a distinct channel; repeated signals coalesce.
fn wait_for_signal(label: &str, channel: &str, deadline: Instant) -> io::Result<()> {
    let mut child = tmux_command()
        .args(["-L", label, "wait-for", channel])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let status = wait_child(&mut child, deadline)?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "tmux wait-for {channel}: {status}"
        )))
    }
}

#[test]
fn expired_connect_guard_never_reaches_the_listener() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("connect.sock");
    let listener = UnixListener::bind(&path).unwrap();
    listener.set_nonblocking(true).unwrap();
    assert_eq!(
        connect_until(&path, Instant::now()).unwrap_err().kind(),
        io::ErrorKind::TimedOut
    );
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    let _connected = connect_until(&path, Instant::now() + HANG_GUARD).unwrap();
    listener.accept().unwrap();
}

#[test]
fn shim_connection_failure_records_its_stage() {
    let Some(python) = python3_on_path() else {
        eprintln!("SKIP: python3 not on PATH");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let shim = write_claude_shim(dir.path());
    let output = command_output(
        Command::new(python)
            .arg(&shim)
            .env("ATM_SOCKET", dir.path().join("missing.sock"))
            .env("TMUX_PANE", "%fixture"),
    );
    assert!(!output.status.success());
    let stages = std::fs::read_to_string(shim.with_extension("stages")).unwrap();
    assert!(
        stages.contains("python started")
            && stages.contains("connecting")
            && stages.contains("error:"),
        "{stages}"
    );
    assert!(!stages.contains("socket connected"), "{stages}");
}

#[test]
fn cleanup_capture_spawn_failure_returns_none() {
    // A NUL makes spawn fail without changing the process-wide PATH or
    // contacting any server. Cleanup must remain safe while unwinding.
    assert!(tmux_run_capture("invalid\0label", &["display-message"]).is_none());
}

#[test]
fn protocol_write_preserves_all_bytes() {
    let (mut writer, reader) = UnixStream::pair().unwrap();
    write_bytes(&mut writer, b"first\nsecond\n", Instant::now() + HANG_GUARD).unwrap();
    let mut reader = BufReader::new(reader);
    assert_eq!(
        read_frame(&mut reader, Instant::now() + HANG_GUARD).unwrap(),
        "first\n"
    );
    assert_eq!(
        read_frame(&mut reader, Instant::now() + HANG_GUARD).unwrap(),
        "second\n"
    );
}

#[test]
fn protocol_frames_share_the_operation_deadline() {
    let (mut writer, reader) = UnixStream::pair().unwrap();
    writer.write_all(b"first\nsecond\n").unwrap();
    let mut reader = BufReader::new(reader);
    assert_eq!(
        read_frame(&mut reader, Instant::now() + HANG_GUARD).unwrap(),
        "first\n"
    );
    // Even an already-buffered next message cannot renew an expired operation.
    assert_eq!(
        read_frame(&mut reader, Instant::now()).unwrap_err().kind(),
        io::ErrorKind::TimedOut
    );
}

#[test]
fn fifo_reader_preserves_accumulated_and_extra_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bytes");
    let mut sink = ByteSink::new(&path);
    let mut writer = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    writer.write_all(b"a;\nb").unwrap();
    assert_eq!(sink.read_through(b"a;\nb"), b"a;\nb");
    writer.write_all(b"\nextra").unwrap();
    assert_eq!(
        sink.read_through(b"a;\nb\n"),
        b"a;\nb\nextra",
        "do not truncate unexpected bytes"
    );
}

#[test]
fn tmux_completion_signal_can_precede_its_waiter() {
    if tmux_on_path().is_none() {
        eprintln!("SKIP: tmux not on PATH");
        return;
    }
    let server = PrivateTmux::start_with_label_session_env(
        &PrivateTmux::fresh_label(),
        "signal",
        std::iter::empty::<(&str, &str)>(),
    );
    tmux_stdout(server.label(), &["wait-for", "-S", "early-completion"]);
    wait_for_signal(
        server.label(),
        "early-completion",
        Instant::now() + HANG_GUARD,
    )
    .unwrap();
}

#[test]
fn child_exit_is_observed_and_expired_guards_reap() {
    let mut done = Command::new("sh").args(["-c", "exit 7"]).spawn().unwrap();
    assert_eq!(
        wait_child(&mut done, Instant::now() + HANG_GUARD)
            .unwrap()
            .code(),
        Some(7)
    );
    let mut held = Command::new("sh")
        .args(["-c", "read answer"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    assert_eq!(
        wait_child(&mut held, Instant::now()).unwrap_err().kind(),
        io::ErrorKind::TimedOut
    );
    assert!(
        held.try_wait().unwrap().is_some(),
        "timed-out child was reaped"
    );
}

/// Returns `Some(path)` if `tmux` is on PATH, `None` otherwise.
fn tmux_on_path() -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        if dir.join("tmux").is_file() {
            return Some(dir.join("tmux"));
        }
    }
    None
}

fn atmd_binary() -> Option<PathBuf> {
    option_env!("CARGO_BIN_EXE_atmd").map(PathBuf::from)
}

fn atm_binary() -> Option<PathBuf> {
    option_env!("CARGO_BIN_EXE_atm").map(PathBuf::from)
}

/// Runs a prepared `tmux ...` `Command`, capturing both streams, and
/// panics with stderr included if it fails. Use whenever a tmux
/// invocation's failure is signal — `expect("...")` on its own only
/// catches "couldn't even spawn tmux" and silently swallows everything
/// tmux itself prints to stderr.
fn run_tmux_or_panic(cmd: &mut Command, label: &str) -> Vec<u8> {
    let output = command_output(cmd);
    if !output.status.success() {
        panic!(
            "tmux {label} failed (exit {:?})\nstderr: {}\nstdout: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim(),
            String::from_utf8_lossy(&output.stdout).trim(),
        );
    }
    output.stdout
}

/// Returns `Some(path)` if `python3` resolves on PATH, `None` otherwise.
fn python3_on_path() -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join("python3");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Writes a Python `claude` shim into `dir` and makes it executable.
///
/// The shim impersonates Claude Code's startup just enough to register a
/// session with atmd and stay alive: connect to `$ATM_SOCKET`, send
/// `Connect`, then send a `UserPromptSubmit` `HookEvent` carrying our PID
/// and `$TMUX_PANE`, then wait for pane teardown. Started with arguments, it sends them as
/// the prompt, a JSON array, which atm shows as the session's
/// `first_prompt`. Any error is logged to stderr and the
/// shim exits non-zero so tmux's pane keeps the error visible for
/// debugging.
///
/// The protocol version is templated in from `ProtocolVersion::CURRENT`
/// so a major/minor bump in atm-protocol won't silently break this
/// scenario with a confusing "version mismatch" reject.
fn write_claude_shim(dir: &Path) -> PathBuf {
    let path = dir.join("claude");
    let major = ProtocolVersion::CURRENT.major;
    let minor = ProtocolVersion::CURRENT.minor;
    let body = format!(
        r#"#!/usr/bin/env python3
import json, os, socket, sys, time, uuid, signal

PROTOCOL_VERSION = {{"major": {major}, "minor": {minor}}}

pane = os.environ.get("TMUX_PANE", "")
session_id = "fixture-" + uuid.uuid4().hex[:12]

def stage(label):
    with open(__file__ + ".stages", "a") as log:
        log.write(str(time.monotonic()) + " pid=" + str(os.getpid()) + " pane=" + pane + " " + label + "\n")

stage("python started")
try:
    # Missing fixture configuration must never fall back to a live daemon.
    socket_path = os.environ["ATM_SOCKET"]
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    stage("connecting")
    s.connect(socket_path)
    stage("socket connected")
    f = s.makefile("rwb", buffering=0)

    def send(obj):
        f.write((json.dumps(obj) + "\n").encode())
        f.flush()

    def recv():
        line = f.readline()
        if not line:
            raise RuntimeError("daemon closed connection")
        return json.loads(line.decode())

    send({{
        "protocol_version": PROTOCOL_VERSION,
        "type": "connect",
        "client_id": "claude-fixture",
    }})
    stage("Connect written; waiting for Connected")
    msg = recv()
    stage("handshake reply: " + msg.get("type", "missing type"))
    if msg.get("type") != "connected":
        raise RuntimeError("unexpected handshake response: " + json.dumps(msg))

    send({{
        "protocol_version": PROTOCOL_VERSION,
        "type": "hook_event",
        "data": {{
            "session_id": session_id,
            "hook_event_name": "UserPromptSubmit",
            "pid": os.getpid(),
            "tmux_pane": pane,
            # The arguments it was started with, so a test can check the
            # command `atm spawn --prompt` typed reached it as one word.
            "prompt": json.dumps(sys.argv[1:]) if len(sys.argv) > 1 else "fixture probe",
        }},
    }})

    stage("hook written: " + session_id)
    # The test's subscriber, not this write, acknowledges registry application.
    # Keep this PID alive until the pane is torn down, with no timed expiry.
    print("[claude-fixture] sent hook for " + session_id + " for pane " + pane, flush=True)
    while True:
        signal.pause()
except Exception as e:
    stage("error: " + repr(e))
    print("[claude-fixture] error: " + repr(e), file=sys.stderr, flush=True)
    sys.exit(1)
"#
    );
    std::fs::write(&path, body).expect("write claude shim");
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(&path).expect("stat shim").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).expect("chmod shim");
    path
}

fn next_private_sequence() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn private_session_name(prefix: &str) -> String {
    format!("{prefix}{}-{}", std::process::id(), next_private_sequence())
}

/// Owns a private tmux server scoped to this test (tmux `-L <socket>`).
///
/// We pass this label to `RealTmuxClient::with_socket(...)` so the project's
/// own tmux library code talks to *this* server, not the developer's default.
///
/// Env vars passed to `start_with_label_session_env(...)` propagate into the tmux server's
/// process — and from there into the shells running in panes the server
/// creates. Scenario 4 uses this channel to hand the `claude` shim the
/// `ATM_SOCKET` it needs to phone home to atmd. (The shim path itself is
/// injected via `ATM_SPAWN_BIN` at the `atm spawn` invocation, not
/// via tmux env, so PATH precedence isn't load-bearing here.)
struct PrivateTmux {
    socket_label: String,
}

impl PrivateTmux {
    /// A socket label unique to this test run.
    fn fresh_label() -> String {
        let now_nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        Self::fresh_label_at(now_nanos)
    }

    fn fresh_label_at(now_nanos: u128) -> String {
        format!(
            "atm-e2e-{}-{now_nanos}-{}",
            std::process::id(),
            next_private_sequence()
        )
    }

    fn start_with_label_session_env<I, K, V>(socket_label: &str, session_name: &str, env: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<std::ffi::OsStr>,
        V: AsRef<std::ffi::OsStr>,
    {
        let mut cmd = tmux_command();
        for (k, v) in env {
            cmd.env(k, v);
        }
        // `-f /dev/null`: the developer's ~/.tmux.conf (key bindings,
        // plugins, a sourced ATM bindings file) must not leak into tests.
        cmd.args([
            "-L",
            socket_label,
            "-f",
            "/dev/null",
            "new-session",
            "-d",
            "-s",
            session_name,
        ]);
        run_tmux_or_panic(&mut cmd, "new-session");

        Self {
            socket_label: socket_label.to_string(),
        }
    }

    fn label(&self) -> &str {
        &self.socket_label
    }
}

#[test]
fn private_tmux_labels_differ_with_a_frozen_clock() {
    assert_ne!(
        PrivateTmux::fresh_label_at(7),
        PrivateTmux::fresh_label_at(7)
    );
}

#[test]
fn private_tmux_labels_are_unique_concurrently() {
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
    let workers: Vec<_> = (0..8)
        .map(|_| {
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                (0..256)
                    .map(|_| PrivateTmux::fresh_label_at(7))
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    let labels: std::collections::BTreeSet<_> = workers
        .into_iter()
        .flat_map(|worker| worker.join().unwrap())
        .collect();
    assert_eq!(labels.len(), 8 * 256);
}

fn assert_distinct_private_probe_servers(first: String, second: String) {
    if tmux_on_path().is_none() {
        return;
    }
    let first = PrivateTmux::start_with_label_session_env(
        &first,
        "probe",
        std::iter::empty::<(&str, &str)>(),
    );
    tmux_stdout(
        first.label(),
        &["set-option", "-g", "@namespace-owner", "first"],
    );
    let second = PrivateTmux::start_with_label_session_env(
        &second,
        "probe",
        std::iter::empty::<(&str, &str)>(),
    );
    tmux_stdout(
        second.label(),
        &["set-option", "-g", "@namespace-owner", "second"],
    );
    assert_eq!(
        tmux_stdout(
            first.label(),
            &["show-options", "-g", "-v", "@namespace-owner"]
        ),
        "first"
    );
    assert_eq!(
        tmux_stdout(
            second.label(),
            &["show-options", "-g", "-v", "@namespace-owner"]
        ),
        "second"
    );
    for server in [&first, &second] {
        assert_eq!(
            tmux_stdout(server.label(), &["list-sessions", "-F", "#{session_name}"]),
            "probe"
        );
    }
}

#[test]
fn private_tmux_frozen_clock_preserves_existing_probe() {
    assert_distinct_private_probe_servers(
        PrivateTmux::fresh_label_at(7),
        PrivateTmux::fresh_label_at(7),
    );
}

#[test]
fn private_tmux_cli_names_differ() {
    assert_ne!(
        private_session_name("probe-"),
        private_session_name("probe-")
    );
}

#[test]
fn private_tmux_cli_names_preserve_existing_probe() {
    assert_distinct_private_probe_servers(
        format!("atm-{}", private_session_name("probe-")),
        format!("atm-{}", private_session_name("probe-")),
    );
}

impl Drop for PrivateTmux {
    fn drop(&mut self) {
        let Some(identity) = tmux_run_capture(
            &self.socket_label,
            &["display-message", "-p", "#{socket_path}\t#{pid}"],
        ) else {
            return;
        };
        let private_root = std::env::var_os("TMUX_TMPDIR").unwrap_or_else(|| "/tmp".into());
        let Some((socket, _pid)) =
            owned_tmux_identity(&identity, &self.socket_label, Path::new(&private_root))
        else {
            eprintln!("skipping tmux cleanup: unexpected identity {identity:?}");
            return;
        };
        if let Err(error) = try_command_output(tmux_command().args(["-S", socket, "kill-server"])) {
            eprintln!("private tmux cleanup: {error}");
        }
    }
}

fn owned_tmux_identity<'a>(
    identity: &'a str,
    label: &str,
    private_root: &Path,
) -> Option<(&'a str, u32)> {
    let (socket, pid) = identity.trim().split_once('\t')?;
    let pid = pid.parse::<u32>().ok().filter(|pid| *pid > 1)?;
    (Path::new(socket).starts_with(private_root)
        && Path::new(socket).file_name() == Some(label.as_ref()))
    .then_some((socket, pid))
}

// Omitting -u corrupts the identity delimiter in a non-UTF-8 client locale.
#[test]
fn private_tmux_identity_preserves_tabs_in_ascii_locale() {
    if tmux_on_path().is_none() {
        return;
    }
    let server = PrivateTmux::start_with_label_session_env(
        &PrivateTmux::fresh_label(),
        "ascii-identity",
        std::iter::empty::<(&str, &str)>(),
    );
    let output = command_output(
        tmux_command()
            .env("LC_ALL", "C")
            .env_remove("LC_CTYPE")
            .env_remove("LANG")
            .args([
                "-L",
                server.label(),
                "display-message",
                "-p",
                "#{socket_path}\t#{pid}",
            ]),
    );
    assert!(output.status.success(), "{output:?}");
    let identity = String::from_utf8(output.stdout).unwrap();
    let root = std::env::var_os("TMUX_TMPDIR").unwrap_or_else(|| "/tmp".into());
    assert!(
        owned_tmux_identity(&identity, server.label(), Path::new(&root)).is_some(),
        "ASCII locale must retain the exact tab-separated owned identity: {identity:?}"
    );
}

fn assert_private_tmux_guard_cleans_up(unwind: bool) {
    if tmux_on_path().is_none() {
        return;
    }
    let label = PrivateTmux::fresh_label();
    let result = std::panic::catch_unwind(|| {
        let _server = PrivateTmux::start_with_label_session_env(
            &label,
            "cleanup",
            std::iter::empty::<(&str, &str)>(),
        );
        if unwind {
            panic!("injected private tmux owner unwind");
        }
    });
    assert_eq!(result.is_err(), unwind);
    let remaining = command_output(tmux_command().args(["-L", &label, "list-sessions"]));
    assert!(
        !remaining.status.success(),
        "private guard left its server running: {remaining:?}"
    );
}

#[test]
fn private_tmux_guard_cleans_up_on_return() {
    assert_private_tmux_guard_cleans_up(false);
}

#[test]
fn private_tmux_guard_cleans_up_on_unwind() {
    assert_private_tmux_guard_cleans_up(true);
}

#[test]
fn private_tmux_identity_refuses_unowned_sockets() {
    let root = Path::new("/private-e2e");
    for identity in [
        "/other/owned\t42",
        "/private-e2e/foreign\t42",
        "/private-e2e/owned\t1",
        "/private-e2e/owned\tnot-a-pid",
        "/private-e2e/owned_42",
    ] {
        assert_eq!(owned_tmux_identity(identity, "owned", root), None);
    }
    assert_eq!(
        owned_tmux_identity("/private-e2e/owned\t42\n", "owned", root),
        Some(("/private-e2e/owned", 42))
    );
}

fn append_log_line(writer: &mut impl Write, line: &str) -> io::Result<()> {
    writer.write_all(format!("{line}\n").as_bytes())
}

// Fragmented formatting lets the independent stderr writer split a stdout line.
#[test]
fn daemon_log_keeps_complete_stdout_lines_while_stderr_appends() {
    #[derive(Default)]
    struct Interleaved {
        bytes: Vec<u8>,
        stderr_written: bool,
    }
    impl Write for Interleaved {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.bytes.extend_from_slice(bytes);
            if !self.stderr_written {
                self.bytes
                    .extend_from_slice(b"stderr before readiness 4095\n");
                self.stderr_written = true;
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut log = Interleaved::default();
    append_log_line(&mut log, "stdout before readiness 4095").unwrap();
    append_log_line(&mut log, "Daemon server listening").unwrap();
    assert_eq!(
        String::from_utf8(log.bytes).unwrap(),
        "stdout before readiness 4095\nstderr before readiness 4095\nDaemon server listening\n"
    );
}

fn write_daemon_fixture(dir: &Path, body: &str) -> PathBuf {
    let path = dir.join("atmd-fixture");
    let mut script = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o700)
        .open(&path)
        .unwrap();
    writeln!(script, "#!/bin/sh\n{body}").unwrap();
    path
}

#[tokio::test]
async fn daemon_readiness_returns_on_log_event_without_socket() {
    let dir = tempfile::tempdir().unwrap();
    let binary = write_daemon_fixture(
        dir.path(),
        r#"i=0
while [ "$i" -lt 4096 ]; do
    printf 'stdout before readiness %s\n' "$i"
    printf 'stderr before readiness %s\n' "$i" >&2
    i=$((i + 1))
done
printf 'Daemon server listening\n'
exec /bin/cat "$0.hold"
"#,
    );
    // A concurrent fork can retain this writer even after its parent closes it.
    let _writer = std::fs::OpenOptions::new()
        .write(true)
        .open(&binary)
        .unwrap();
    // Keep the child and its stdout open after the event, without any socket.
    let _hold = ByteSink::new(&binary.with_extension("hold"));
    let mut command = Command::new("/bin/sh");
    command.arg(&binary);
    let mut daemon = Daemon::spawn_command(command, "unused").await;
    assert!(!daemon.socket_path.exists());
    assert!(daemon.child.as_mut().unwrap().try_wait().unwrap().is_none());
    let log = std::fs::read_to_string(&daemon.log_path).unwrap();
    assert!(log.contains("stdout before readiness 4095\n"));
    assert!(log.contains("stderr before readiness 4095\n"));
    assert!(log.contains("Daemon server listening\n"));
}

#[tokio::test]
async fn daemon_readiness_rejects_eof_before_event() {
    let dir = tempfile::tempdir().unwrap();
    let binary = write_daemon_fixture(
        dir.path(),
        "printf 'stdout without listening\\n'\nprintf 'stderr without listening\\n' >&2\nexit 7",
    );
    let _writer = std::fs::OpenOptions::new()
        .write(true)
        .open(&binary)
        .unwrap();
    let mut command = Command::new("/bin/sh");
    command.arg(&binary);
    let failure = tokio::spawn(async move { Daemon::spawn_command(command, "unused").await })
        .await
        .err()
        .expect("EOF without the listening event must fail readiness")
        .into_panic();
    let message = failure
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| failure.downcast_ref::<&str>().copied())
        .expect("readiness failure has a diagnostic message");
    assert!(message.contains("daemon readiness:"), "{message}");
    assert!(message.contains("stdout without listening"), "{message}");
    assert!(message.contains("stderr without listening"), "{message}");
}

/// Owns a child `atmd` process and its sandbox dirs. Drop sends SIGTERM and
/// waits briefly so PID files / sockets are cleaned up.
struct Daemon {
    child: Option<Child>,
    socket_path: PathBuf,
    state_dir: PathBuf,
    /// Path to the file capturing daemon stdout+stderr. Surfaced on
    /// failure so a readiness panic includes actual evidence.
    log_path: PathBuf,
    _state_dir_guard: TempDir,
    _socket_dir_guard: TempDir,
}

impl Daemon {
    /// `tmux_label` is the private tmux server's `-L` label. Without it,
    /// atmd's discovery would list the panes of whatever server `$TMUX`
    /// names, which is the developer's own when tests run inside tmux.
    async fn spawn(binary: &Path, tmux_label: &str) -> Self {
        Self::spawn_command(Command::new(binary), tmux_label).await
    }

    async fn spawn_command(command: Command, tmux_label: &str) -> Self {
        // Startup waits on a blocking channel, not on the async runtime.
        let tmux_label = tmux_label.to_owned();
        tokio::task::spawn_blocking(move || Self::spawn_ready(command, &tmux_label))
            .await
            .unwrap()
    }

    /// The log event is emitted after bind/listen; no sleep or socket polling.
    fn spawn_ready(mut command: Command, tmux_label: &str) -> Self {
        let sockets = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let socket_path = sockets.path().join("atmd.sock");
        let state_dir = state.path().to_owned();
        let log_path = state_dir.join("atmd.log");
        let mut log = std::fs::OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(&log_path)
            .unwrap();
        let mut child = command
            .arg("start")
            .env("ATM_SOCKET", &socket_path)
            .env("ATM_TMUX_SOCKET", tmux_label)
            .env("ATM_STATE_DIR", state_dir.join("eventlog"))
            .env("XDG_STATE_HOME", &state_dir)
            // atmd reads `[notify]` from atm's config.toml: never the developer's.
            .env("XDG_CONFIG_HOME", state_dir.join("config"))
            .env("RUST_LOG", "warn,atmd::server=info")
            .stdout(Stdio::piped())
            .stderr(log.try_clone().unwrap())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let daemon = Self {
            child: Some(child),
            socket_path,
            state_dir,
            log_path,
            _state_dir_guard: state,
            _socket_dir_guard: sockets,
        };
        let (ready, receiver) = mpsc::channel();
        std::thread::Builder::new()
            .name("daemon-log".into())
            .spawn(move || {
                let mut ready = Some(ready);
                for line in BufReader::new(stdout).lines() {
                    let line = line.unwrap();
                    append_log_line(&mut log, &line).unwrap();
                    if line.contains("Daemon server listening") {
                        if let Some(ready) = ready.take() {
                            let _ = ready.send(());
                        }
                    }
                }
            })
            .unwrap();
        receiver.recv_timeout(HANG_GUARD).unwrap_or_else(|error| {
            panic!(
                "daemon readiness: {error}; {}",
                std::fs::read_to_string(&daemon.log_path).unwrap_or_default()
            )
        });
        daemon
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            if std::thread::panicking() {
                eprintln!(
                    "--- daemon log ---\n{}",
                    std::fs::read_to_string(&self.log_path).unwrap_or_default()
                );
                for name in ["status", "wchan", "schedstat"] {
                    eprintln!(
                        "--- daemon {name} ---\n{}",
                        std::fs::read_to_string(format!("/proc/{}/{name}", child.id()))
                            .unwrap_or_default()
                    );
                }
            }
            unsafe {
                libc::kill(child.id() as i32, libc::SIGTERM);
            }

            if let Err(error) = wait_child(&mut child, Instant::now() + HANG_GUARD) {
                eprintln!("daemon shutdown: {error}");
            }
        }
    }
}

/// Synchronous protocol client. Blocking std `UnixStream` keeps the harness
/// readable as a linear "send / recv / assert" script without dragging in a
/// runtime for the protocol-level scenario.
struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    request: String,
    sent_at: Instant,
    deadline: Instant,
}

impl Client {
    fn connect(path: &Path) -> Self {
        let stream =
            connect_until(path, Instant::now() + HANG_GUARD).expect("connect to atmd socket");
        let writer = stream.try_clone().expect("clone unix stream");
        Self {
            reader: BufReader::new(stream),
            writer,
            request: "initial connection".into(),
            sent_at: Instant::now(),
            deadline: Instant::now() + HANG_GUARD,
        }
    }

    fn send(&mut self, msg: ClientMessage) {
        self.request = format!("{msg:?}");
        self.sent_at = Instant::now();
        self.deadline = self.sent_at + HANG_GUARD;
        eprintln!("protocol send: {}", self.request);
        let mut json = serde_json::to_vec(&msg).expect("serialize ClientMessage");
        json.push(b'\n');
        write_bytes(&mut self.writer, &json, self.deadline).expect("write client message");
    }

    fn recv(&mut self) -> DaemonMessage {
        let line = read_frame(&mut self.reader, self.deadline).unwrap_or_else(|e| {
            panic!(
                "protocol read after {} ({:?} since send): {e}",
                self.request,
                self.sent_at.elapsed()
            )
        });
        serde_json::from_str(&line).expect("deserialize DaemonMessage")
    }
}

/// Shared fixture for all 6 scenarios — daemon, primary tmux server,
/// shim, binary paths. Scenarios borrow `&E2eEnv` immutably; everything
/// inside is RAII-cleaned when the fixture drops at end-of-test.
struct E2eEnv {
    atm_path: PathBuf,
    daemon: Daemon,
    tmux_server: PrivateTmux,
    tmux: RealTmuxClient,
    shim_path: PathBuf,
    _shim_dir_guard: TempDir,
}

impl E2eEnv {
    /// Returns `None` (after printing a SKIP line) when any precondition
    /// is missing. Lets the orchestrator early-return cleanly without
    /// failing the whole test on minimal CI images.
    async fn try_setup() -> Option<Self> {
        if tmux_on_path().is_none() {
            eprintln!("SKIP: tmux not on PATH");
            return None;
        }
        let atmd_path = match atmd_binary() {
            Some(p) => p,
            None => {
                eprintln!("SKIP: CARGO_BIN_EXE_atmd not set");
                return None;
            }
        };
        let atm_path = match atm_binary() {
            Some(p) => p,
            None => {
                eprintln!("SKIP: CARGO_BIN_EXE_atm not set");
                return None;
            }
        };

        // Daemon comes up first so the tmux server's env can carry
        // ATM_SOCKET straight into any panes it births. Scenario 4's
        // shim reads it from there to find the daemon socket. The label
        // is picked first so the daemon's discovery can target the
        // private server too.
        let tmux_label = PrivateTmux::fresh_label();
        let daemon = Daemon::spawn(&atmd_path, &tmux_label).await;

        // Stage the `claude` shim once; scenario 4 will inject it via
        // ATM_SPAWN_BIN. Earlier iterations tried prepending the shim
        // dir to PATH, but interactive shell init re-derived PATH from
        // scratch and dropped it. Absolute-path injection sidesteps
        // that entirely.
        let shim_dir_guard = tempfile::tempdir().expect("create shim tempdir");
        let shim_path = write_claude_shim(shim_dir_guard.path());

        let tmux_server = PrivateTmux::start_with_label_session_env(
            &tmux_label,
            "probe",
            [("ATM_SOCKET", daemon.socket_path.as_os_str())],
        );
        let tmux = RealTmuxClient::with_socket(tmux_server.label().to_string());

        Some(Self {
            atm_path,
            daemon,
            tmux_server,
            tmux,
            shim_path,
            _shim_dir_guard: shim_dir_guard,
        })
    }
}

// This unit owns its readiness event; inherited fixtures are deliberately untouched.
fn staged_spawn_daemon(binary: &Path, label: &str, cwd: &Path) -> Daemon {
    let sockets = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("atmd.sock");
    let log_path = state.path().join("spawn-daemon.log");
    let mut log = std::fs::OpenOptions::new()
        .create_new(true)
        .append(true)
        .open(&log_path)
        .unwrap();
    let mut child = Command::new(binary)
        .arg("start")
        .current_dir(cwd)
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .env("ATM_SOCKET", &socket_path)
        .env("ATM_TMUX_SOCKET", label)
        .env("ATM_STATE_DIR", state.path().join("events"))
        .env("XDG_STATE_HOME", state.path())
        .env("XDG_CONFIG_HOME", state.path().join("config"))
        .env("RUST_LOG", "warn,atmd::server=info")
        .stdout(Stdio::piped())
        .stderr(log.try_clone().unwrap())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (ready, receive) = mpsc::channel();
    std::thread::Builder::new()
        .name("spawn-proof-readiness".into())
        .spawn(move || {
            let mut ready = Some(ready);
            for line in BufReader::new(stdout).lines() {
                let line = line.unwrap();
                writeln!(log, "{line}").unwrap();
                if line.contains("Daemon server listening") {
                    if let Some(ready) = ready.take() {
                        let _ = ready.send(());
                    }
                }
            }
        })
        .unwrap();
    let daemon = Daemon {
        child: Some(child),
        socket_path,
        state_dir: state.path().to_owned(),
        log_path,
        _state_dir_guard: state,
        _socket_dir_guard: sockets,
    };
    receive.recv_timeout(HANG_GUARD).unwrap_or_else(|error| {
        panic!(
            "spawn daemon readiness: {error}: {}",
            std::fs::read_to_string(&daemon.log_path).unwrap_or_default()
        )
    });
    daemon
}

#[test]
fn daemon_largest_spawn_stages_complete_with_generated_hooks_and_foreign_ids() {
    use std::os::unix::fs::PermissionsExt;
    if tmux_on_path().is_none() {
        eprintln!("SKIP: tmux not on PATH");
        return;
    }
    let Some(binary) = atmd_binary() else {
        eprintln!("SKIP: atmd not built");
        return;
    };
    let own = PrivateTmux::start_with_label_session_env(
        &PrivateTmux::fresh_label(),
        "spawn-own",
        std::iter::empty::<(&str, &str)>(),
    );
    let foreign = PrivateTmux::start_with_label_session_env(
        &PrivateTmux::fresh_label(),
        "spawn-foreign",
        std::iter::empty::<(&str, &str)>(),
    );
    let data = tempfile::tempdir().unwrap();
    let cwd = data.path().join("target's cwd");
    let caller_cwd = std::env::current_dir().unwrap();
    let daemon_cwd = data.path().join("daemon cwd");
    std::fs::create_dir(&cwd).unwrap();
    std::fs::create_dir(&daemon_cwd).unwrap();
    let socket = |server: &PrivateTmux| {
        let identity = tmux_stdout(
            server.label(),
            &["display-message", "-p", "#{socket_path}\t#{pid}"],
        );
        let (path, pid) = identity.trim().split_once('\t').unwrap();
        assert!(Path::new(path).canonicalize().unwrap().starts_with(
            Path::new(&std::env::var_os("TMUX_TMPDIR").unwrap_or_else(|| "/tmp".into()))
                .canonicalize()
                .unwrap()
        ));
        assert!(pid.parse::<u32>().unwrap() > 1);
        path.to_owned()
    };
    let own_socket = socket(&own);
    let foreign_socket = socket(&foreign);
    assert_ne!(own_socket, foreign_socket);
    let target = |server: &PrivateTmux| {
        tmux_stdout(
            server.label(),
            &["set-option", "-g", "default-shell", "/bin/sh"],
        );
        tmux_stdout(
            server.label(),
            &["resize-window", "-t", "%0", "-x", "320", "-y", "60"],
        );
        let command = format!(
            "tmux -L {} wait-for -S spawn-target-ready; exec /bin/sh",
            atmd::control::shell_quote(server.label())
        );
        let pane = tmux_stdout(
            server.label(),
            &[
                "split-window",
                "-d",
                "-h",
                "-l",
                "80%",
                "-t",
                "%0",
                "-c",
                cwd.to_str().unwrap(),
                "-P",
                "-F",
                "#{pane_id}",
                &command,
            ],
        )
        .trim()
        .to_owned();
        wait_for_signal(
            server.label(),
            "spawn-target-ready",
            Instant::now() + HANG_GUARD,
        )
        .unwrap();
        pane
    };
    let target = target(&own);
    assert_eq!(target, "%1");
    // Both servers retain the same numeric pane IDs, not just different labels.
    let foreign_target = tmux_stdout(
        foreign.label(),
        &[
            "split-window",
            "-d",
            "-h",
            "-t",
            "%0",
            "-P",
            "-F",
            "#{pane_id}",
            "/bin/sh",
        ],
    )
    .trim()
    .to_owned();
    assert_eq!(target, foreign_target);
    let stub = data.path().join("bin");
    std::fs::create_dir(&stub).unwrap();
    std::fs::write(stub.join("atm"), "#!/bin/sh\nexec /bin/cat\n").unwrap();
    std::fs::set_permissions(stub.join("atm"), std::fs::Permissions::from_mode(0o700)).unwrap();
    tmux_stdout(
        own.label(),
        &[
            "set-environment",
            "-g",
            "PATH",
            &format!("{}:/usr/bin:/bin", stub.display()),
        ],
    );
    tmux_stdout(
        own.label(),
        &["set-option", "-p", "-t", "%0", "@atm-sidebar", "1"],
    );
    let hooks = atmd::control::workspace::WorkspaceHooks {
        script_dir: data.path().join("hooks"),
        socket: Some(own.label().into()),
        tmux_prefix: format!("tmux -L {}", atmd::control::shell_quote(own.label())),
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let client = RealTmuxClient::with_socket(own.label());
    runtime.block_on(async {
        atmd::control::workspace::install_resize_hooks(&client, "spawn-own", &hooks)
            .await
            .unwrap();
        atmd::control::workspace::install_new_window_hook(&client, "spawn-own", &hooks)
            .await
            .unwrap();
    });
    for (hook, signal) in [
        ("after-new-window", "spawn-hook-new"),
        ("after-resize-window", "spawn-hook-window"),
        ("after-resize-pane", "spawn-hook-pane"),
    ] {
        tmux_stdout(
            own.label(),
            &[
                "set-hook",
                "-a",
                "-t",
                "=spawn-own:",
                hook,
                &format!("wait-for -S {signal}"),
            ],
        );
    }
    let window = tmux_stdout(
        own.label(),
        &[
            "new-window",
            "-d",
            "-t",
            "spawn-own:",
            "-P",
            "-F",
            "#{window_id}",
            "/bin/sh",
        ],
    )
    .trim()
    .to_owned();
    wait_for_signal(own.label(), "spawn-hook-new", Instant::now() + HANG_GUARD).unwrap();
    assert_eq!(
        tmux_stdout(
            own.label(),
            &["list-panes", "-t", &window, "-F", "#{@atm-sidebar}"]
        )
        .lines()
        .filter(|line| *line == "1")
        .count(),
        1
    );
    tmux_stdout(own.label(), &["kill-window", "-t", &window]);
    tmux_stdout(
        own.label(),
        &["resize-window", "-t", "%0", "-x", "300", "-y", "60"],
    );
    wait_for_signal(
        own.label(),
        "spawn-hook-window",
        Instant::now() + HANG_GUARD,
    )
    .unwrap();
    assert_eq!(
        tmux_stdout(
            own.label(),
            &["display-message", "-p", "-t", "%0", "#{pane_width}"]
        )
        .trim(),
        "40"
    );
    tmux_stdout(own.label(), &["resize-pane", "-t", "%0", "-x", "50"]);
    wait_for_signal(own.label(), "spawn-hook-pane", Instant::now() + HANG_GUARD).unwrap();
    assert_eq!(
        tmux_stdout(
            own.label(),
            &["display-message", "-p", "-t", "%0", "#{pane_width}"]
        )
        .trim(),
        "40"
    );
    let daemon = staged_spawn_daemon(&binary, own.label(), &daemon_cwd);
    // Readiness already established bind/listen: no connect-until polling needed.
    let stream = UnixStream::connect(&daemon.socket_path).unwrap();
    let writer = stream.try_clone().unwrap();
    let mut peer = Client {
        reader: BufReader::new(stream),
        writer,
        request: "spawn connect".into(),
        sent_at: Instant::now(),
        deadline: Instant::now() + HANG_GUARD,
    };
    peer.send(ClientMessage::connect(Some("staged-spawn-proof".into())));
    assert!(
        matches!(peer.recv(),DaemonMessage::Connected{methods,..} if ["session.spawn_prepare_largest","session.spawn","session.spawn_type"].iter().all(|name|methods.iter().any(|m|m==name)))
    );
    let call = |peer: &mut Client, method: &str, params: serde_json::Value| {
        peer.send(ClientMessage::request(
            method.into(),
            method.into(),
            params,
            Some("%0".into()),
        ));
        match peer.recv() {
            DaemonMessage::Response { payload, .. } => payload,
            other => panic!("spawn response {other:?}"),
        }
    };
    let panes = |server: &PrivateTmux| {
        tmux_stdout(server.label(), &["list-panes", "-a", "-F", "#{pane_id}"])
    };
    let own_before = panes(&own);
    let foreign_before = panes(&foreign);
    for (method, params) in [
        (
            "session.spawn_prepare_largest",
            json!({"caller_server":foreign_socket,"caller_pane":"%0"}),
        ),
        (
            "session.spawn",
            json!({"caller_server":foreign_socket,"target_pane":target,"caller_pane_missing":false,"direction":"below","size":"50%","cwd":null}),
        ),
        (
            "session.spawn_type",
            json!({"caller_server":foreign_socket,"pane":target,"command":"/bin/false"}),
        ),
    ] {
        assert!(
            matches!(call(&mut peer,method,params),atm_protocol::ResponsePayload::Error(error) if error.code=="refused")
        );
        assert_eq!(panes(&own), own_before);
        assert_eq!(panes(&foreign), foreign_before);
    }
    assert!(
        matches!(call(&mut peer,"session.spawn_prepare_largest",json!({"caller_server":own_socket,"caller_pane":"%0"})),atm_protocol::ResponsePayload::Result(value) if value==json!({"target_pane":target,"cwd":cwd}))
    );
    for (index, direction) in ["left", "right", "above", "below"].into_iter().enumerate() {
        let split = call(
            &mut peer,
            "session.spawn",
            json!({"caller_server":own_socket,"target_pane":target,"caller_pane_missing":false,"direction":direction,"size":"50%","cwd":caller_cwd}),
        );
        let pane = match split {
            atm_protocol::ResponsePayload::Result(value) => {
                value["pane"].as_str().unwrap().to_owned()
            }
            other => panic!("split {other:?}"),
        };
        assert_eq!(
            tmux_stdout(
                own.label(),
                &["display-message", "-p", "-t", &pane, "#{pane_start_path}"]
            )
            .trim(),
            caller_cwd.to_str().unwrap(),
            "split shell must start in the TUI caller's directory, before typed cd"
        );
        let output = data.path().join(format!("typed-{index}"));
        let signal = format!("spawn-typed-{index}");
        let command = format!(
            "cd {} && printf 'typed;literal\\n' > {}; tmux -L {} wait-for -S {}",
            atmd::control::shell_quote(cwd.to_str().unwrap()),
            atmd::control::shell_quote(output.to_str().unwrap()),
            atmd::control::shell_quote(own.label()),
            signal
        );
        assert!(
            matches!(call(&mut peer,"session.spawn_type",json!({"caller_server":own_socket,"pane":pane,"command":command})),atm_protocol::ResponsePayload::Result(value) if value.is_null())
        );
        wait_for_signal(own.label(), &signal, Instant::now() + HANG_GUARD).unwrap();
        assert_eq!(std::fs::read(output).unwrap(), b"typed;literal\n");
    }
    assert_eq!(panes(&foreign), foreign_before);
}

// Breaks if kill retargets, ignores caller_server, or removes a foreign pane
// with the same id. This uses the real daemon actor and real private tmux.
#[test]
fn daemon_kill_exact_target_and_foreign_server_refusal() {
    if tmux_on_path().is_none() {
        eprintln!("SKIP: tmux not on PATH");
        return;
    }
    let Some(binary) = atmd_binary() else {
        eprintln!("SKIP: atmd not built");
        return;
    };
    let own = PrivateTmux::start_with_label_session_env(
        &PrivateTmux::fresh_label(),
        "kill-own",
        std::iter::empty::<(&str, &str)>(),
    );
    let foreign = PrivateTmux::start_with_label_session_env(
        &PrivateTmux::fresh_label(),
        "kill-foreign",
        std::iter::empty::<(&str, &str)>(),
    );
    let identity = |server: &PrivateTmux| {
        let identity = tmux_stdout(
            server.label(),
            &["display-message", "-p", "#{socket_path}\t#{pid}"],
        );
        let (socket, pid) = identity.trim().split_once('\t').unwrap();
        let root = std::env::var_os("TMUX_TMPDIR").unwrap_or_else(|| "/tmp".into());
        assert!(Path::new(socket)
            .canonicalize()
            .unwrap()
            .starts_with(Path::new(&root).canonicalize().unwrap()));
        assert!(pid.parse::<u32>().unwrap() > 1);
        eprintln!("private tmux: {identity}");
        socket.to_owned()
    };
    let own_socket = identity(&own);
    let foreign_socket = identity(&foreign);
    assert_ne!(own_socket, foreign_socket);
    let split = |server: &PrivateTmux| {
        tmux_stdout(
            server.label(),
            &[
                "split-window",
                "-d",
                "-h",
                "-P",
                "-F",
                "#{pane_id}",
                "-t",
                "%0",
                "/bin/sh",
            ],
        )
        .trim()
        .to_owned()
    };
    let target = split(&own);
    assert_eq!(target, split(&foreign), "exercise colliding pane ids");
    let panes = |server: &PrivateTmux| {
        tmux_stdout(server.label(), &["list-panes", "-a", "-F", "#{pane_id}"])
            .lines()
            .map(str::to_owned)
            .collect::<std::collections::BTreeSet<_>>()
    };
    let before = std::collections::BTreeSet::from(["%0".to_owned(), target.clone()]);
    assert_eq!(panes(&own), before);
    assert_eq!(panes(&foreign), before);
    let daemon = Daemon::spawn_ready(Command::new(&binary), own.label());
    let mut client = Client::connect(&daemon.socket_path);
    client.send(ClientMessage::connect(Some("kill-e2e".into())));
    assert!(
        matches!(client.recv(), DaemonMessage::Connected { methods, .. } if methods.iter().any(|m| m == "session.kill"))
    );
    client.send(ClientMessage::request(
        "foreign".into(),
        "session.kill".into(),
        json!({"target": target, "caller_server": foreign_socket}),
        Some("%0".into()),
    ));
    assert!(
        matches!(client.recv(), DaemonMessage::Response { payload: atm_protocol::ResponsePayload::Error(error), .. }
        if error.code == "refused" && error.message.contains("atmd controls tmux server"))
    );
    assert_eq!(panes(&own), before);
    assert_eq!(panes(&foreign), before);
    client.send(ClientMessage::request(
        "own".into(),
        "session.kill".into(),
        json!({"target": target, "caller_server": own_socket}),
        Some("%0".into()),
    ));
    assert!(
        matches!(client.recv(), DaemonMessage::Response { payload: atm_protocol::ResponsePayload::Result(value), .. }
        if value == json!({"pane": target}))
    );
    assert_eq!(
        panes(&own),
        std::collections::BTreeSet::from(["%0".to_owned()])
    );
    assert_eq!(panes(&foreign), before);
}

// Breaks if daemon membership matches prefixes or conflates equal pane ids
// across servers. Real sockets/daemon/tmux; readiness is the existing log event.
#[test]
fn daemon_session_panes_exact_membership_and_foreign_server_refusal() {
    if tmux_on_path().is_none() {
        eprintln!("SKIP: tmux not on PATH");
        return;
    }
    let Some(binary) = atmd_binary() else {
        eprintln!("SKIP: atmd not built");
        return;
    };
    let own = PrivateTmux::start_with_label_session_env(
        &PrivateTmux::fresh_label(),
        "work",
        std::iter::empty::<(&str, &str)>(),
    );
    let foreign = PrivateTmux::start_with_label_session_env(
        &PrivateTmux::fresh_label(),
        "work",
        std::iter::empty::<(&str, &str)>(),
    );
    let own_socket = tmux_stdout(own.label(), &["display-message", "-p", "#{socket_path}"]);
    let foreign_socket = tmux_stdout(
        foreign.label(),
        &["display-message", "-p", "#{socket_path}"],
    );
    let root = std::env::var_os("TMUX_TMPDIR").unwrap_or_else(|| "/tmp".into());
    for socket in [&own_socket, &foreign_socket] {
        assert!(Path::new(socket)
            .canonicalize()
            .unwrap()
            .starts_with(Path::new(&root).canonicalize().unwrap()));
    }
    assert_ne!(own_socket, foreign_socket);
    assert_eq!(
        tmux_stdout(own.label(), &["list-panes", "-a", "-F", "#{pane_id}"]),
        "%0"
    );
    assert_eq!(
        tmux_stdout(foreign.label(), &["list-panes", "-a", "-F", "#{pane_id}"]),
        "%0"
    );
    let second = tmux_stdout(
        own.label(),
        &[
            "new-window",
            "-d",
            "-t",
            "work:",
            "-P",
            "-F",
            "#{pane_id}",
            "/bin/sh",
        ],
    );
    tmux_stdout(
        own.label(),
        &["new-session", "-d", "-s", "work-extra", "/bin/sh"],
    );
    let daemon = Daemon::spawn_ready(Command::new(&binary), own.label());
    let mut client = Client::connect(&daemon.socket_path);
    client.send(ClientMessage::connect(Some("panes-e2e".into())));
    assert!(
        matches!(client.recv(), DaemonMessage::Connected { methods, .. } if methods.iter().any(|m|m=="session.panes"))
    );
    client.send(ClientMessage::request(
        "foreign".into(),
        "session.panes".into(),
        json!({"session_name":"work","caller_server":foreign_socket}),
        Some("%0".into()),
    ));
    assert!(
        matches!(client.recv(), DaemonMessage::Response { payload:atm_protocol::ResponsePayload::Error(e), .. } if e.code=="refused")
    );
    for (name, expected) in [
        ("work", json!(["%0", second])),
        ("wor", json!([])),
        ("missing", json!([])),
    ] {
        client.send(ClientMessage::request(
            "own".into(),
            "session.panes".into(),
            json!({"session_name":name,"caller_server":own_socket}),
            Some("%0".into()),
        ));
        assert!(
            matches!(client.recv(), DaemonMessage::Response { payload:atm_protocol::ResponsePayload::Result(v), .. } if v==json!({"pane_ids":expected}))
        );
    }
    assert_eq!(
        tmux_stdout(foreign.label(), &["list-panes", "-a", "-F", "#{pane_id}"]),
        "%0"
    );
}

// Role metadata uses the same identity fence as pane control. No harness
// starts; command completion and the existing log-ready event synchronize it.
#[test]
fn daemon_roles_read_set_clear_and_foreign_equal_ids() {
    if tmux_on_path().is_none() {
        eprintln!("SKIP: tmux not on PATH");
        return;
    }
    let Some(binary) = atmd_binary() else {
        eprintln!("SKIP: atmd not built");
        return;
    };
    let own = PrivateTmux::start_with_label_session_env(
        &PrivateTmux::fresh_label(),
        "roles-own",
        std::iter::empty::<(&str, &str)>(),
    );
    let foreign = PrivateTmux::start_with_label_session_env(
        &PrivateTmux::fresh_label(),
        "roles-foreign",
        std::iter::empty::<(&str, &str)>(),
    );
    let identity = |server: &PrivateTmux| {
        let identity = tmux_stdout(
            server.label(),
            &["display-message", "-p", "#{socket_path}\t#{pid}"],
        );
        let (socket, pid) = identity.trim().split_once('\t').unwrap();
        let root = std::env::var_os("TMUX_TMPDIR").unwrap_or_else(|| "/tmp".into());
        assert!(Path::new(socket)
            .canonicalize()
            .unwrap()
            .starts_with(Path::new(&root).canonicalize().unwrap()));
        assert!(pid.parse::<u32>().unwrap() > 1);
        socket.to_owned()
    };
    let own_socket = identity(&own);
    let foreign_socket = identity(&foreign);
    assert_ne!(own_socket, foreign_socket);
    let panes = |server: &PrivateTmux| {
        tmux_stdout(
            server.label(),
            &["list-panes", "-a", "-F", "#{pane_id}\t#{@atm-role}"],
        )
    };
    assert_eq!(
        panes(&own),
        panes(&foreign),
        "exercise colliding server-local pane ids"
    );
    let target = panes(&own).split_whitespace().next().unwrap().to_owned();
    let untouched = panes(&foreign);
    // spawn_ready already retains logs and signals only after bind/listen.
    // Connect directly: no polling helper is needed after that event.
    let daemon = Daemon::spawn_ready(Command::new(&binary), own.label());
    let stream = UnixStream::connect(&daemon.socket_path).unwrap();
    let mut client = Client {
        writer: stream.try_clone().unwrap(),
        reader: BufReader::new(stream),
        request: "roles connect".into(),
        sent_at: Instant::now(),
        deadline: Instant::now() + HANG_GUARD,
    };
    client.send(ClientMessage::connect(Some("roles-e2e".into())));
    assert!(
        matches!(client.recv(),DaemonMessage::Connected{methods,..} if methods.iter().any(|m|m=="session.roles") && methods.iter().any(|m|m=="session.role_set"))
    );
    for role in [json!("coordinator"), Value::Null, Value::Null] {
        client.send(ClientMessage::request(
            "set".into(),
            "session.role_set".into(),
            json!({"caller_server":own_socket,"pane":target,"role":role}),
            Some(target.clone()),
        ));
        assert!(
            matches!(client.recv(),DaemonMessage::Response{payload:atm_protocol::ResponsePayload::Result(value),..} if value==json!({"pane":target,"role":role}))
        );
        client.send(ClientMessage::request(
            "roles".into(),
            "session.roles".into(),
            json!({"caller_server":own_socket}),
            None,
        ));
        let expected = if role.is_null() {
            json!({"coordinator_panes":[]})
        } else {
            json!({"coordinator_panes":[target]})
        };
        assert!(
            matches!(client.recv(),DaemonMessage::Response{payload:atm_protocol::ResponsePayload::Result(value),..} if value==expected)
        );
    }
    let own_before = panes(&own);
    for (method, params) in [
        ("session.roles", json!({"caller_server":foreign_socket})),
        (
            "session.role_set",
            json!({"caller_server":foreign_socket,"pane":target,"role":"coordinator"}),
        ),
        (
            "session.role_set",
            json!({"caller_server":foreign_socket,"pane":target,"role":null}),
        ),
    ] {
        client.send(ClientMessage::request(
            "foreign".into(),
            method.into(),
            params,
            None,
        ));
        assert!(
            matches!(client.recv(),DaemonMessage::Response{payload:atm_protocol::ResponsePayload::Error(error),..} if error.code=="refused")
        );
        assert_eq!(panes(&own), own_before);
        assert_eq!(panes(&foreign), untouched);
    }
}

/// Control must refuse a second real server even when both have pane %0.
async fn scenario_control_server_identity_and_start_directory(env: &E2eEnv) {
    let other = PrivateTmux::start_with_label_session_env(
        &PrivateTmux::fresh_label(),
        "other",
        [("ATM_SOCKET", env.daemon.socket_path.as_os_str())],
    );
    let before = env.tmux.list_panes().await.unwrap().len();
    for args in [
        vec!["send", "%0", "must not arrive"],
        vec!["spawn", "-t", "%0"],
    ] {
        let config_home = env.daemon.state_dir.join("config");
        let mut command = Command::new(&env.atm_path);
        command
            .args(args)
            .env("ATM_SOCKET", &env.daemon.socket_path)
            .env("XDG_STATE_HOME", &env.daemon.state_dir)
            .env("ATM_TMUX_SOCKET", other.label())
            .env("TMUX", "private-test")
            .env("ATM_SPAWN_BIN", "true")
            .env("XDG_CONFIG_HOME", &config_home)
            .env_remove("ATM_SPAWN_ARGS")
            .env_remove("ATM_SPAWN_CLAUDE_BIN")
            .env_remove("ATM_SPAWN_CLAUDE_ARGS");
        // Even if server refusal regresses, no ambient harness may launch.
        let command_env = |key: &str| {
            command
                .get_envs()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| value)
        };
        assert_eq!(
            command_env("XDG_CONFIG_HOME"),
            Some(Some(config_home.as_os_str()))
        );
        assert_eq!(
            command_env("ATM_SPAWN_BIN"),
            Some(Some(std::ffi::OsStr::new("true")))
        );
        for key in [
            "ATM_SPAWN_ARGS",
            "ATM_SPAWN_CLAUDE_BIN",
            "ATM_SPAWN_CLAUDE_ARGS",
        ] {
            assert_eq!(command_env(key), Some(None), "must remove {key}");
        }
        let output = command_output(&mut command);
        assert!(!output.status.success(), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("atmd controls tmux server"),
            "{output:?}"
        );
    }
    assert_eq!(env.tmux.list_panes().await.unwrap().len(), before);

    // Stop between split and type: the pane must start in the caller directory,
    // not a later typed `cd` or the daemon's working dir. On tmux 3.4/3.6a,
    // pane_start_path is stored before fork; pane_current_path can still report
    // the inherited cwd while the child does systemd setup (tmux/tmux#4719).
    let cwd = tempfile::tempdir().unwrap();
    let mut client = Client::connect(&env.daemon.socket_path);
    client.send(ClientMessage::connect(Some("cwd-test".into())));
    assert!(matches!(client.recv(), DaemonMessage::Connected { .. }));
    client.send(ClientMessage::request("cwd".into(), "session.spawn".into(), json!({
        "target_pane":"%0", "caller_pane_missing":false, "direction":"below", "size":"20%", "cwd":cwd.path()
    }), None));
    let pane = match client.recv() {
        DaemonMessage::Response {
            payload: atm_protocol::ResponsePayload::Result(result),
            ..
        } => result["pane"].as_str().unwrap().to_owned(),
        other => panic!("split response: {other:?}"),
    };
    let start_path = tmux_run_capture(
        env.tmux_server.label(),
        &["display-message", "-p", "-t", &pane, "#{pane_start_path}"],
    )
    .expect("query pane start path");
    assert_eq!(start_path.trim(), cwd.path().to_str().unwrap());
    env.tmux.kill_pane(&pane).await.unwrap();
}

/// Scenario 1 — protocol-level e2e via a hand-rolled client.
/// Returns the session id it registered, so scenario 2 can verify it
/// shows up in the real `atm list` output.
async fn scenario_protocol_e2e(env: &E2eEnv) -> String {
    let mut client = Client::connect(&env.daemon.socket_path);

    client.send(ClientMessage::connect(Some("e2e-harness".into())));
    match client.recv() {
        DaemonMessage::Connected {
            protocol_version,
            client_id,
            ..
        } => {
            assert_eq!(protocol_version, ProtocolVersion::CURRENT);
            assert_eq!(client_id, "e2e-harness");
        }
        other => panic!("expected Connected, got {other:?}"),
    }

    // Snapshot baseline so we tolerate sessions discovered from /proc.
    client.send(ClientMessage::list_sessions());
    let baseline_count = match client.recv() {
        DaemonMessage::SessionList { sessions } => sessions.len(),
        other => panic!("expected SessionList, got {other:?}"),
    };

    client.send(ClientMessage::discover());
    match client.recv() {
        DaemonMessage::DiscoveryComplete { .. } => {}
        other => panic!("expected DiscoveryComplete, got {other:?}"),
    }

    let session_id = format!(
        "e2e-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let hook_event = json!({
        "session_id": session_id,
        "hook_event_name": "UserPromptSubmit",
        "pid": std::process::id(),
        "prompt": "e2e harness probe",
    });
    client.send(ClientMessage::hook_event(hook_event));

    // The connection awaits hook application before handling the next request.
    // Its ListSessions reply is a barrier, not a race with another connection.
    client.send(ClientMessage::list_sessions());
    let sessions = match client.recv() {
        DaemonMessage::SessionList { sessions } => sessions,
        other => panic!("expected SessionList, got {other:?}"),
    };
    assert!(
        sessions.iter().any(|s| s.id.as_str() == session_id),
        "session {session_id} missing after the hook application barrier"
    );
    assert!(
        sessions.len() > baseline_count,
        "session count should grow by at least one (baseline={baseline_count}, now={})",
        sessions.len()
    );

    client.send(ClientMessage::disconnect());
    session_id
}

/// Scenario 2 — drive the real `atm` binary against the test daemon and
/// confirm the JSON output contains the session scenario 1 registered.
async fn scenario_atm_list_client(env: &E2eEnv, expected_session_id: &str) {
    // `atm list --format json` opens its own connection to the daemon. We
    // point it at our test socket via ATM_SOCKET and hand it the same
    // XDG_STATE_HOME so its `ensure_daemon_running` check (which reads the
    // PID file) sees the daemon we already started — preventing it from
    // spawning a second one.
    let output = command_output(
        Command::new(&env.atm_path)
            .args(["list", "--format", "json"])
            .env("ATM_SOCKET", &env.daemon.socket_path)
            .env("XDG_STATE_HOME", &env.daemon.state_dir)
            .env("ATM_TMUX_SOCKET", env.tmux_server.label()),
    );

    assert!(
        output.status.success(),
        "atm list exited with {:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = std::str::from_utf8(&output.stdout).expect("utf-8 stdout");
    let parsed: Value = serde_json::from_str(stdout).expect("atm list emits valid JSON");
    let arr = parsed.as_array().expect("atm list emits a JSON array");
    let our_session = arr
        .iter()
        .find(|s| s.get("id").and_then(Value::as_str) == Some(expected_session_id));
    assert!(
        our_session.is_some(),
        "real `atm list` JSON output did not contain our session {expected_session_id}; got: {stdout}"
    );
}

/// Real daemon + real CLI, with synthetic hook sessions on private panes.
async fn scenario_role_json(env: &E2eEnv, session_id: &str) {
    let panes = env.tmux.list_panes().await.unwrap();
    let pane = &panes
        .iter()
        .find(|p| p.session_name == "probe")
        .unwrap()
        .pane_id;
    let other = env
        .tmux
        .split_window(pane, "30%", PaneDirection::Below, None, None, false)
        .await
        .unwrap();
    let other_pid: u32 = env
        .tmux
        .raw_command("display-message", &["-p", "-t", &other, "#{pane_pid}"])
        .await
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let other_id = format!("other-{session_id}");
    let mut client = Client::connect(&env.daemon.socket_path);
    client.send(ClientMessage::connect(Some("role-test".into())));
    assert!(matches!(client.recv(), DaemonMessage::Connected { .. }));
    for (id, pane, pid) in [
        (session_id, pane.as_str(), std::process::id()),
        (other_id.as_str(), other.as_str(), other_pid),
    ] {
        client.send(ClientMessage::hook_event(json!({
            "session_id": id, "hook_event_name": "UserPromptSubmit",
            "pid": pid, "tmux_pane": pane, "prompt": "role fixture",
        })));
    }
    // Flush the two events through the actor before the CLI requests its list.
    client.send(ClientMessage::list_sessions());
    assert!(matches!(client.recv(), DaemonMessage::SessionList { .. }));
    client.send(ClientMessage::disconnect());
    let run_atm = |args: &[&str]| {
        command_output(
            Command::new(&env.atm_path)
                .args(args)
                .env("ATM_SOCKET", &env.daemon.socket_path)
                .env("XDG_STATE_HOME", &env.daemon.state_dir)
                .env("ATM_TMUX_SOCKET", env.tmux_server.label())
                .env_remove("TMUX")
                .env_remove("TMUX_PANE"),
        )
    };
    for (arg, expected) in [
        ("coordinator", json!("coordinator")),
        ("--clear", Value::Null),
    ] {
        let tag = run_atm(&["role", arg, "-t", session_id]);
        assert!(tag.status.success(), "{tag:?}");
        assert!(tag.stdout.is_empty());
        let list = run_atm(&["list", "-f", "json"]);
        assert!(list.status.success(), "{list:?}");
        let rows: Vec<Value> = serde_json::from_slice(&list.stdout).unwrap();
        let lead = rows.iter().find(|row| row["id"] == session_id).unwrap();
        assert_eq!(lead["tmux_pane"], pane.as_str());
        assert_eq!(lead.get("role"), Some(&expected));
        let other_row = rows.iter().find(|row| row["id"] == other_id).unwrap();
        assert_eq!(other_row.get("role"), Some(&Value::Null));
        assert!(rows.iter().all(|row| row.get("role").is_some()));
    }
    env.tmux.kill_pane(&other).await.unwrap();
}

/// Scenario 3 — drive the isolated tmux server through `atm-tmux`'s own
/// library. `RealTmuxClient` supports `-L`, so this stays fully isolated
/// from the developer's default tmux.
async fn scenario_real_tmux_lib(env: &E2eEnv) {
    let panes_before = env.tmux.list_panes().await.expect("list panes (initial)");
    assert!(
        !panes_before.is_empty(),
        "private tmux server should have at least the probe session's pane"
    );
    let probe_pane = panes_before
        .iter()
        .find(|p| p.session_name == "probe")
        .expect("probe session present")
        .pane_id
        .clone();

    let new_pane = env
        .tmux
        .split_window(&probe_pane, "30%", PaneDirection::Below, None, None, false)
        .await
        .expect("split-window");
    assert!(
        new_pane.starts_with('%'),
        "split-window should return a tmux pane id like '%N', got {new_pane:?}"
    );

    let deadline = Instant::now() + HANG_GUARD;
    let command = format!(
        "echo atm-e2e-marker; {} -L {} wait-for -S library-echo",
        shell_word(&tmux_on_path().unwrap().display().to_string()),
        shell_word(env.tmux_server.label())
    );
    env.tmux
        .send_keys(&new_pane, &command)
        .await
        .expect("send-keys");
    env.tmux
        .send_keys(&new_pane, "Enter")
        .await
        .expect("send-keys Enter");

    // Merely capturing the typed command could pass before the shell ran it.
    // Its completion signal proves execution before retaining the capture check.
    wait_for_signal(env.tmux_server.label(), "library-echo", deadline).expect("echo completed");
    let lines = env.tmux.capture_pane(&new_pane).await.unwrap_or_default();
    assert!(
        lines.iter().any(|line| line.contains("atm-e2e-marker")),
        "expected 'atm-e2e-marker' in pane {new_pane}'s capture after execution"
    );

    let panes_after = env
        .tmux
        .list_panes()
        .await
        .expect("list panes (after split)");
    assert_eq!(
        panes_after.len(),
        panes_before.len() + 1,
        "split-window should add exactly one pane to the private server"
    );
}

// Break caught: copying through argv rejects valid long message bodies, or an
// empty copy changes the existing buffer (tmux 3.8 leaves it intact). No daemon.
#[tokio::test(flavor = "current_thread")]
async fn copy_buffer_preserves_long_and_empty_bodies() {
    if tmux_on_path().is_none() {
        eprintln!("SKIP: tmux not on PATH");
        return;
    }
    let server = PrivateTmux::start_with_label_session_env(
        &PrivateTmux::fresh_label(),
        "copy",
        std::iter::empty::<(&str, &str)>(),
    );
    let tmux = RealTmuxClient::with_socket(server.label());
    let boundary = "x".repeat(16_384);
    let long = format!("- literal λ\n{}", "x".repeat(65_536));
    for (body, want) in [
        ("previous buffer", "previous buffer"),
        ("", "previous buffer"),
        (boundary.as_str(), boundary.as_str()),
        (long.as_str(), long.as_str()),
        ("", long.as_str()),
    ] {
        tmux.load_buffer(body).await.expect("copy complete body");
        assert_eq!(tmux.raw_command("show-buffer", &[]).await.unwrap(), want);
    }
    let missing = RealTmuxClient::with_socket(format!("{}-missing", server.label()));
    for body in [String::new(), "x".repeat(1 << 20)] {
        let error = missing
            .load_buffer(&body)
            .await
            .expect_err("missing server");
        assert!(
            matches!(error, atm_tmux::TmuxError::CommandFailed {ref command,..} if command=="load-buffer"),
            "{error:?}"
        );
    }
}

/// Scenario 3b: `RealTmuxClient::paste_text`, then the real `atm send`,
/// against panes running `cat > file`, so each file holds exactly the
/// bytes its program read.
async fn scenario_paste_text_and_atm_send(env: &E2eEnv) {
    let label = &env.tmux_server.socket_label;
    let out = TempDir::new().expect("paste output dir");
    let plain_file = out.path().join("plain");
    let bracketed_file = out.path().join("bracketed");
    let probe_pane = env
        .tmux
        .list_panes()
        .await
        .expect("list panes")
        .into_iter()
        .find(|p| p.session_name == "probe")
        .expect("probe session present")
        .pane_id;
    let atm_buffers = || {
        tmux_run_capture(label, &["list-buffers", "-F", "#{buffer_name}"])
            .expect("list-buffers")
            .lines()
            .filter(|name| name.starts_with("atm-send-"))
            .map(str::to_string)
            .collect::<Vec<_>>()
    };
    let mut plain_sink = ByteSink::new(&plain_file);
    let mut bracketed_sink = ByteSink::new(&bracketed_file);
    let mut read_when = |path: &Path, want: &[u8]| {
        if path == plain_file {
            plain_sink.read_through(want)
        } else {
            assert_eq!(path, bracketed_file);
            bracketed_sink.read_through(want)
        }
    };

    // `cat` never asks for bracketed paste. Each pane prints READY once its
    // escape sequences have gone out; tmux reads them in order, so READY on
    // screen means tmux has seen the bracketed-paste request.
    let plain = env
        .tmux
        .split_window(
            &probe_pane,
            "20%",
            PaneDirection::Below,
            Some(&format!(
                "printf READY; exec cat > '{}'",
                plain_file.display()
            )),
            None,
            false,
        )
        .await
        .expect("split plain cat pane");
    let bracketed = env
        .tmux
        .split_window(
            &probe_pane,
            "20%",
            PaneDirection::Below,
            Some(&format!(
                "printf '\\033[?2004hREADY'; exec cat > '{}'",
                bracketed_file.display()
            )),
            None,
            false,
        )
        .await
        .expect("split bracketed cat pane");
    for pane in [&plain, &bracketed] {
        let deadline = Instant::now() + HANG_GUARD;
        // No semantic terminal-parser acknowledgment exists. A child signal
        // would not prove tmux consumed the preceding bracketed-paste escape.
        while !env
            .tmux
            .capture_pane(pane)
            .await
            .unwrap_or_default()
            .iter()
            .any(|line| line.contains("READY"))
        {
            assert!(Instant::now() < deadline, "pane {pane} never printed READY");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    // The library: empty text is a no-op, a trailing `;` survives
    // (send-keys drops it), a newline stays inside the paste, and `Enter`
    // goes on its own call.
    env.tmux
        .paste_text(&plain, "")
        .await
        .expect("empty paste is a no-op");
    env.tmux
        .paste_text(&plain, "a;\nb")
        .await
        .expect("paste into plain pane");
    env.tmux
        .send_keys(&plain, "Enter")
        .await
        .expect("Enter in plain pane");
    env.tmux
        .paste_text(&bracketed, "x;")
        .await
        .expect("paste into bracketed pane");
    env.tmux
        .send_keys(&bracketed, "Enter")
        .await
        .expect("Enter in bracketed pane");
    // paste-buffer turns LF into CR, and the tty turns CR back into LF.
    let mut plain_want = b"a;\nb\n".to_vec();
    assert_eq!(
        read_when(&plain_file, &plain_want),
        plain_want,
        "plain pane read"
    );
    let mut bracketed_want = b"\x1b[200~x;\x1b[201~\n".to_vec();
    assert_eq!(
        read_when(&bracketed_file, &bracketed_want),
        bracketed_want,
        "bracketed pane read"
    );
    assert!(
        atm_buffers().is_empty(),
        "paste must delete its buffer: {:?}",
        atm_buffers()
    );

    // A failed paste must not leave the text behind in a buffer.
    assert!(env.tmux.paste_text("%999999", "secret").await.is_err());
    assert!(
        atm_buffers().is_empty(),
        "failed paste left a buffer: {:?}",
        atm_buffers()
    );

    // With no server, tmux exits before reading its input. The error must
    // be tmux's own, not the broken pipe from the 1 MiB write.
    let no_server = RealTmuxClient::with_socket(format!("{label}-none"));
    let error = no_server
        .paste_text("%0", &"a".repeat(1 << 20))
        .await
        .expect_err("no server");
    assert!(
        matches!(&error, atm_tmux::TmuxError::CommandFailed { command, .. } if command == "load-buffer"),
        "{error:?}"
    );

    // The real `atm send`, on whatever tmux this machine has.
    let atm_send = |pane: &str, text: &str| {
        command_output(
            Command::new(&env.atm_path)
                .args(["send", pane, text])
                // No tmux on CLI PATH: only the running daemon can control panes.
                .env("PATH", env._shim_dir_guard.path())
                .env("ATM_SOCKET", &env.daemon.socket_path)
                .env("XDG_STATE_HOME", &env.daemon.state_dir)
                .env("ATM_TMUX_SOCKET", label),
        )
    };
    let sent = atm_send(&plain, "/clear;");
    assert!(sent.status.success(), "{sent:?}");
    assert_eq!(
        String::from_utf8_lossy(&sent.stdout).trim(),
        format!("Sent to {plain} by tmux paste")
    );
    plain_want.extend_from_slice(b"/clear;\n");
    assert_eq!(
        read_when(&plain_file, &plain_want),
        plain_want,
        "atm send /clear;"
    );

    let sent = atm_send(&bracketed, "m1\nm2");
    assert!(sent.status.success(), "{sent:?}");
    bracketed_want.extend_from_slice(b"\x1b[200~m1\nm2\x1b[201~\n");
    assert_eq!(
        read_when(&bracketed_file, &bracketed_want),
        bracketed_want,
        "atm send of two lines to a bracketed pane"
    );

    // `bracket_paste_flag` arrived in tmux 3.7. On older tmux it reads as
    // unknown, so atm sends multi-line text to `cat` too.
    let sent = atm_send(&plain, "p1\np2");
    if tmux_version_at_least(3, 7) {
        assert!(!sent.status.success(), "{sent:?}");
        assert!(
            String::from_utf8_lossy(&sent.stderr).contains("bracketed paste"),
            "{sent:?}"
        );
    } else {
        assert!(sent.status.success(), "{sent:?}");
        plain_want.extend_from_slice(b"p1\np2\n");
    }

    tmux_run_capture(label, &["copy-mode", "-t", &plain]).expect("enter copy mode");
    let sent = atm_send(&plain, "hi");
    assert!(!sent.status.success(), "{sent:?}");
    assert!(
        String::from_utf8_lossy(&sent.stderr).contains("copy mode"),
        "{sent:?}"
    );
    tmux_run_capture(label, &["send-keys", "-t", &plain, "-X", "cancel"]).expect("leave copy mode");
    assert_eq!(
        read_when(&plain_file, &plain_want),
        plain_want,
        "refused sends must not reach the pane"
    );

    for pane in [&plain, &bracketed] {
        env.tmux.kill_pane(pane).await.expect("kill cat pane");
    }
}

/// The `--prompt` scenario 4 spawns the shim with: shell quotes and
/// specials that must reach it as one argument.
const SPAWN_PROMPT: &str = r#"/atm-implement --profile p it's "$HOME" & `id`; done;"#;

/// Scenario 4 — `atm spawn` end-to-end. Splits a pane in our isolated
/// server, the pane runs our Python shim with `--prompt`, the shim phones
/// home to atmd, we verify the registered session has the expected
/// `tmux_pane` and the prompt as the shim's one argument.
async fn scenario_atm_spawn(env: &E2eEnv) {
    let panes_pre_spawn = env.tmux.list_panes().await.expect("list panes (pre-spawn)");
    let probe_pane_pre_spawn = panes_pre_spawn
        .iter()
        .find(|p| p.session_name == "probe")
        .expect("probe pane present")
        .pane_id
        .clone();

    // Subscribe before any spawn side effect. The initial snapshot acknowledges
    // installation of the subscription; its guard also bounds later events.
    let mut subscriber = Client::connect(&env.daemon.socket_path);
    subscriber.send(ClientMessage::connect(Some("spawn-observer".into())));
    assert!(matches!(subscriber.recv(), DaemonMessage::Connected { .. }));
    subscriber.send(ClientMessage::subscribe(None));
    let initial = match subscriber.recv() {
        DaemonMessage::SessionList { sessions } => sessions,
        other => panic!("subscription snapshot: {other:?}"),
    };

    let spawn_output = command_output(
        Command::new(&env.atm_path)
            .env("PATH", env._shim_dir_guard.path())
            .args([
                "spawn",
                "--target-pane",
                &probe_pane_pre_spawn,
                "--direction",
                "below",
                "--size",
                "20%",
                "--prompt",
                SPAWN_PROMPT,
            ])
            // is_in_tmux() just checks for the env var; any value passes.
            .env("TMUX", "atm-e2e-fixture-no-real-tmux-here")
            // Redirect every tmux invocation atm spawn makes onto our server.
            .env("ATM_TMUX_SOCKET", env.tmux_server.label())
            // Override the hardcoded "claude" command with our shim, by
            // absolute path so shell PATH precedence is irrelevant.
            .env("ATM_SPAWN_BIN", &env.shim_path)
            // The shim talks back to *our* daemon over this socket.
            .env("ATM_SOCKET", &env.daemon.socket_path)
            // ensure_daemon_running reads PID file under XDG_STATE_HOME.
            .env("XDG_STATE_HOME", &env.daemon.state_dir)
            // The developer's own config (a default harness) and spawn
            // overrides (ARGS before the prompt) would change the shim's argv.
            .env("XDG_CONFIG_HOME", env.daemon.state_dir.join("config"))
            .env_remove("ATM_SPAWN_ARGS")
            .env_remove("ATM_SPAWN_CLAUDE_BIN")
            .env_remove("ATM_SPAWN_CLAUDE_ARGS"),
    );

    assert!(
        spawn_output.status.success(),
        "atm spawn exited with {:?}\nstdout: {}\nstderr: {}",
        spawn_output.status.code(),
        String::from_utf8_lossy(&spawn_output.stdout),
        String::from_utf8_lossy(&spawn_output.stderr)
    );

    // Identify the pane atm spawn just created on our server.
    let panes_post_spawn = env
        .tmux
        .list_panes()
        .await
        .expect("list panes (post-spawn)");
    let new_panes: Vec<_> = panes_post_spawn
        .iter()
        .filter(|p| !panes_pre_spawn.iter().any(|q| q.pane_id == p.pane_id))
        .collect();
    assert_eq!(
        new_panes.len(),
        1,
        "atm spawn should create exactly one new pane; got {} ({:?})",
        new_panes.len(),
        new_panes.iter().map(|p| &p.pane_id).collect::<Vec<_>>()
    );
    let spawned_pane = new_panes[0].pane_id.clone();
    // A caller learns the new pane from atm spawn's stdout (#121).
    assert_eq!(
        String::from_utf8_lossy(&spawn_output.stdout),
        format!("{spawned_pane}\n"),
        "atm spawn should print the pane it created"
    );

    let matches = |session: &SessionView| {
        session.id.as_str().starts_with("fixture-")
            && session.tmux_pane.as_deref() == Some(spawned_pane.as_str())
    };
    let observed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if let Some(session) = initial.into_iter().find(&matches) {
            return session;
        }
        loop {
            match subscriber.recv() {
                DaemonMessage::SessionUpdated { session } if matches(&session) => return *session,
                DaemonMessage::SessionList { sessions } => {
                    if let Some(session) = sessions.into_iter().find(&matches) {
                        return session;
                    }
                }
                DaemonMessage::SessionUpdated { .. } | DaemonMessage::SessionRemoved { .. } => {}
                other => panic!("spawn subscription: {other:?}"),
            }
        }
    }));
    let observed = match observed {
        Ok(session) => session,
        Err(error) => {
            let stages =
                std::fs::read_to_string(env.shim_path.with_extension("stages")).unwrap_or_default();
            let pane_dump = env
                .tmux
                .capture_pane(&spawned_pane)
                .await
                .unwrap_or_else(|e| vec![format!("capture failed: {e}")]);
            eprintln!("spawn registry acknowledgment failed; shim stages:\n{stages}\nstdout: {}\nstderr: {}\npane {spawned_pane}:\n{}",
                String::from_utf8_lossy(&spawn_output.stdout), String::from_utf8_lossy(&spawn_output.stderr), pane_dump.join("\n"));
            std::panic::resume_unwind(error);
        }
    };
    // Retain real CLI/JSON coverage, after the committed registry event instead
    // of repeatedly forking a list command to observe eventual completion.
    let list_output = command_output(
        Command::new(&env.atm_path)
            .args(["list", "--format", "json"])
            .env("ATM_SOCKET", &env.daemon.socket_path)
            .env("XDG_STATE_HOME", &env.daemon.state_dir)
            .env("ATM_TMUX_SOCKET", env.tmux_server.label()),
    );
    assert!(list_output.status.success(), "{list_output:?}");
    let rows: Vec<Value> = serde_json::from_slice(&list_output.stdout).expect("list JSON");
    let session = rows
        .iter()
        .find(|row| row.get("id").and_then(Value::as_str) == Some(observed.id.as_str()))
        .expect("committed fixture session is present in real CLI list");

    // The shim records `$TMUX_PANE`, which inside the spawned pane
    // equals the id atm spawn assigned it. This is what closes the
    // loop: spawn created the pane, the pane ran the shim, the shim
    // told atmd, atmd remembered the pane id we know — full e2e for
    // the spawn flow.
    let registered_pane = session
        .get("tmux_pane")
        .and_then(Value::as_str)
        .expect("fixture session should carry tmux_pane");
    assert_eq!(
        registered_pane, spawned_pane,
        "fixture session's tmux_pane should match the pane atm spawn created"
    );
    // The shim got the prompt as its one argument, through the pane's
    // shell, quotes and all.
    let first_prompt = session
        .get("first_prompt")
        .and_then(Value::as_str)
        .expect("fixture session should carry first_prompt");
    // "fixture probe" here means the shim got no arguments at all.
    let argv: Value = serde_json::from_str(first_prompt)
        .unwrap_or_else(|e| panic!("the shim's argv as JSON ({e}): {first_prompt:?}"));
    assert_eq!(argv, serde_json::json!([SPAWN_PROMPT]));

    scenario_atm_spawn_defaults_to_tmux_pane(env, &probe_pane_pre_spawn).await;
}

/// Scenario 4b — `atm spawn` with no `-t` splits `$TMUX_PANE`, the pane it
/// runs in (gap 1 of #130), and prints the new pane. Another pane of the
/// probe window is made active first, so the old fallback (the first
/// active pane in `list-panes -a`) would split that one: the split must
/// take its rows from the probe pane.
async fn scenario_atm_spawn_defaults_to_tmux_pane(env: &E2eEnv, probe_pane: &str) {
    // The first spawn's pane is active but only a row or two tall, too
    // small to split. Make the roomiest other pane active instead, so that
    // a wrong target fails the height check below, not tmux's split.
    let panes = env.tmux.list_panes().await.expect("list panes (4b decoy)");
    let decoy = panes
        .iter()
        .filter(|p| p.session_name == "probe" && p.pane_id != probe_pane)
        .max_by_key(|p| p.height)
        .expect("another pane in the probe window")
        .pane_id
        .clone();
    env.tmux
        .select_pane(&decoy)
        .await
        .expect("select the decoy pane");

    let before = env.tmux.list_panes().await.expect("list panes (pre-4b)");
    let probe_before = before
        .iter()
        .find(|p| p.pane_id == probe_pane)
        .expect("probe pane still present")
        .clone();
    let fallback = before
        .iter()
        .find(|p| p.is_active)
        .expect("an active pane")
        .clone();
    assert_ne!(
        fallback.pane_id, probe_pane,
        "precondition: the old fallback must pick another pane, or 4b proves nothing"
    );

    let config_home = env.daemon.state_dir.join("config");
    let mut spawn = Command::new(&env.atm_path);
    spawn
        .args(["spawn", "--direction", "below", "--size", "1"])
        .env("PATH", env._shim_dir_guard.path())
        .env("TMUX", "atm-e2e-fixture-no-real-tmux-here")
        .env("TMUX_PANE", probe_pane)
        .env("ATM_TMUX_SOCKET", env.tmux_server.label())
        // A command that ends at once: the split is what 4b checks.
        .env("ATM_SPAWN_BIN", "true")
        .env("ATM_SOCKET", &env.daemon.socket_path)
        .env("XDG_STATE_HOME", &env.daemon.state_dir)
        .env("XDG_CONFIG_HOME", &config_home)
        .env_remove("ATM_SPAWN_ARGS")
        .env_remove("ATM_SPAWN_CLAUDE_BIN")
        .env_remove("ATM_SPAWN_CLAUDE_ARGS");
    // Check the launch boundary before executing: missing removals would let
    // inherited per-harness overrides supersede the harmless generic binary.
    let command_env = |key: &str| {
        spawn
            .get_envs()
            .find(|(name, _)| *name == key)
            .map(|(_, value)| value)
    };
    assert_eq!(
        command_env("XDG_CONFIG_HOME"),
        Some(Some(config_home.as_os_str()))
    );
    assert_eq!(
        command_env("ATM_SPAWN_BIN"),
        Some(Some(std::ffi::OsStr::new("true")))
    );
    for key in [
        "ATM_SPAWN_ARGS",
        "ATM_SPAWN_CLAUDE_BIN",
        "ATM_SPAWN_CLAUDE_ARGS",
    ] {
        assert_eq!(command_env(key), Some(None), "must remove {key}");
    }
    let out = command_output(&mut spawn);
    assert!(
        out.status.success(),
        "atm spawn (4b) exited with {:?}\nstdout: {}\nstderr: {}\npanes before: {before:?}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let after = env.tmux.list_panes().await.expect("list panes (post-4b)");
    let new_panes: Vec<_> = after
        .iter()
        .filter(|p| !before.iter().any(|q| q.pane_id == p.pane_id))
        .collect();
    assert_eq!(
        new_panes.len(),
        1,
        "4b should create one pane: {new_panes:?}"
    );
    let new_pane = new_panes[0];
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!("{}\n", new_pane.pane_id),
        "atm spawn should print the pane it created"
    );
    assert_eq!(
        (new_pane.session_name.as_str(), new_pane.window_index),
        (
            probe_before.session_name.as_str(),
            probe_before.window_index
        ),
        "the new pane should sit beside the probe pane"
    );
    let height = |pane: &str| {
        after
            .iter()
            .find(|p| p.pane_id == pane)
            .map(|p| p.height)
            .expect("pane still present")
    };
    assert!(
        height(probe_pane) < probe_before.height,
        "the split should take its rows from $TMUX_PANE's pane ({probe_pane}): {before:?} -> {after:?}"
    );
    assert_eq!(
        height(&fallback.pane_id),
        fallback.height,
        "the old fallback's pane ({}) should be untouched",
        fallback.pane_id
    );

    env.tmux
        .kill_pane(&new_pane.pane_id)
        .await
        .expect("kill 4b pane");
}

fn workspace_pane_identity(label: &str, window: &str) -> String {
    tmux_stdout(
        label,
        &[
            "list-panes",
            "-t",
            window,
            "-F",
            "#{pane_id}|#{pane_pid}|#{@atm-sidebar}",
        ],
    )
}

/// A separate hook array entry fences the production hook, including no-ops.
/// Each creation owns a distinct latched signal; no pane-count polling.
fn workspace_chat_test_window(label: &str, session: &str, name: &str, detached: bool) -> String {
    let signal = format!("chat-window-{}", next_private_sequence());
    tmux_stdout(
        label,
        &[
            "set-hook",
            "-t",
            &format!("={session}:"),
            "after-new-window[1]",
            &format!("wait-for -S {signal}"),
        ],
    );
    let mut args = vec!["new-window", "-S", "-P", "-F", "#{window_id}"];
    if detached {
        args.push("-d");
    }
    let target = format!("={session}:");
    args.extend([
        "-t",
        &target,
        "-n",
        name,
        if name == "chat" {
            "atm chat"
        } else {
            "/bin/sh"
        },
    ]);
    // tmux takes PATH from this creating client, not just the server's env.
    let binary = atm_binary().unwrap();
    let path = format!(
        "{}:{}",
        binary.parent().unwrap().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let window = String::from_utf8(run_tmux_or_panic(
        tmux_command()
            .env("PATH", path)
            .args(["-L", label])
            .args(args),
        "chat test new-window",
    ))
    .unwrap()
    .trim()
    .to_owned();
    wait_for_signal(label, &signal, Instant::now() + HANG_GUARD)
        .expect("new-window hook completed");
    window
}

// Break caught: the after-new-window script treats chat like an agent window,
// checks the current window instead of its target, or overmatches chat's name.
#[tokio::test(flavor = "current_thread")]
async fn workspace_chat_window_has_no_sidebar() {
    let Some(env) = E2eEnv::try_setup().await else {
        return;
    };
    let session = private_session_name("chat-create");
    let data = awkward_data_dir();
    let config = tempfile::tempdir().unwrap();
    let standin = tempfile::tempdir().unwrap();
    let log = standin.path().join("pi.log");
    let pi = write_pi_standin(standin.path(), &log);
    write_pi_default_config(config.path(), &pi);
    let path = format!(
        "{}:{}",
        env.atm_path.parent().unwrap().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let server = PrivateTmux {
        socket_label: format!("atm-{session}"),
    };
    let output = command_output(
        workspace_create_command(&env, &session, config.path(), data.path()).env("PATH", &path),
    );
    assert!(output.status.success(), "create: {output:?}");
    let agent = top_non_sidebar_pane(server.label(), &session);
    read_pi_record(server.label(), &agent, &log, Instant::now() + HANG_GUARD);
    let work = tmux_stdout(
        server.label(),
        &["display-message", "-p", "-t", &agent, "#{window_id}"],
    )
    .trim()
    .to_owned();
    let work_before = workspace_pane_identity(server.label(), &work);
    let chat = workspace_chat_test_window(server.label(), &session, "chat", false);
    let chat_before = workspace_pane_identity(server.label(), &chat);
    assert_eq!(chat_before.lines().count(), 1, "chat panes: {chat_before}");
    assert!(
        chat_before.trim().ends_with('|'),
        "chat has a sidebar: {chat_before}"
    );
    assert_eq!(workspace_pane_identity(server.label(), &work), work_before);

    tmux_stdout(server.label(), &["select-window", "-t", &work]);
    tmux_stdout(
        server.label(),
        &["new-window", "-S", "-n", "chat", "atm chat"],
    );
    assert_eq!(workspace_pane_identity(server.label(), &chat), chat_before);
    // Retain the original chat process while creating a second, detached chat.
    tmux_stdout(
        server.label(),
        &["rename-window", "-t", &chat, "retained-chat"],
    );
    tmux_stdout(server.label(), &["select-window", "-t", &work]);
    let _client = ControlClient::attach(server.label(), &session);
    let detached = workspace_chat_test_window(server.label(), &session, "chat", true);
    let panes = workspace_pane_identity(server.label(), &detached);
    assert_eq!(panes.lines().count(), 1, "detached chat panes: {panes}");
    assert!(
        panes.trim().ends_with('|'),
        "detached chat has a sidebar: {panes}"
    );
    assert_eq!(workspace_pane_identity(server.label(), &work), work_before);
    assert_eq!(workspace_pane_identity(server.label(), &chat), chat_before);
    assert_eq!(
        tmux_stdout(
            server.label(),
            &[
                "display-message",
                "-p",
                "-t",
                &format!("={session}:"),
                "#{window_id}"
            ]
        )
        .trim(),
        work,
        "detached creation must leave the attached client's work window current"
    );
    for name in ["Chat", "chat-notes", "work $(false) \"quoted\" spaces"] {
        let window = workspace_chat_test_window(server.label(), &session, name, true);
        let panes = workspace_pane_identity(server.label(), &window);
        assert_eq!(panes.lines().count(), 2, "ordinary {name:?}: {panes}");
        assert_eq!(
            panes.lines().filter(|line| line.ends_with("|1")).count(),
            1,
            "ordinary {name:?}: {panes}"
        );
    }
}

// Break caught: reattach injects into a clean reserved chat, fails to replace
// stale scripts, or destroys an already-contaminated chat's retained panes.
#[tokio::test(flavor = "current_thread")]
async fn workspace_attach_preserves_chat_and_refreshes_hooks() {
    let Some(env) = E2eEnv::try_setup().await else {
        return;
    };
    let session = private_session_name("chat-attach");
    let data = awkward_data_dir();
    let server = PrivateTmux::start_with_label_session_env(
        &PrivateTmux::fresh_label(),
        &session,
        [("ATM_SOCKET", env.daemon.socket_path.as_os_str())],
    );
    let path = format!(
        "{}:{}",
        env.atm_path.parent().unwrap().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    tmux_stdout(server.label(), &["set-environment", "-g", "PATH", &path]);
    tmux_stdout(
        server.label(),
        &["rename-window", "-t", &format!("={session}:"), "work"],
    );
    let work = tmux_stdout(server.label(), &["display-message", "-p", "#{window_id}"])
        .trim()
        .to_owned();
    // No production hook is installed yet: existing chat is a clean one-pane window.
    let chat = workspace_chat_test_window(server.label(), &session, "chat", true);
    let chat_before = workspace_pane_identity(server.label(), &chat);
    assert_eq!(chat_before.lines().count(), 1);
    let ordinary = ["Chat", "chat-notes", "work $(false) \"quoted\" spaces"]
        .map(|name| workspace_chat_test_window(server.label(), &session, name, true));
    let attach = || {
        let output = command_output(
            Command::new(&env.atm_path)
                .args(["workspace", "attach", &session])
                .env_remove("TMUX")
                .env_remove("TMUX_PANE")
                .env("ATM_TMUX_SOCKET", server.label())
                .env("ATM_SOCKET", &env.daemon.socket_path)
                .env("ATM_NO_ATTACH", "1")
                .env("XDG_DATA_HOME", data.path())
                .env("XDG_STATE_HOME", &env.daemon.state_dir)
                .env("PATH", &path),
        );
        assert!(output.status.success(), "attach: {output:?}");
    };
    attach();
    assert_eq!(
        workspace_pane_identity(server.label(), &chat),
        chat_before,
        "attach changed chat panes"
    );
    for window in std::iter::once(&work).chain(&ordinary) {
        let panes = workspace_pane_identity(server.label(), window);
        assert_eq!(panes.lines().count(), 2, "ordinary window: {panes}");
        assert_eq!(
            panes.lines().filter(|line| line.ends_with("|1")).count(),
            1,
            "ordinary window: {panes}"
        );
    }
    let all_panes = || {
        tmux_stdout(
            server.label(),
            &[
                "list-panes",
                "-s",
                "-t",
                &format!("={session}:"),
                "-F",
                "#{window_id}|#{pane_id}|#{pane_pid}|#{@atm-sidebar}",
            ],
        )
    };
    let before = all_panes();
    let script = single_workspace_script(data.path(), &format!("inject-sidebar-{session}.sh"));
    std::fs::write(&script, "#!/bin/sh\nexit 37\n").unwrap();
    attach();
    assert_eq!(
        all_panes(),
        before,
        "repeated attach changed pane identities"
    );
    assert_ne!(
        std::fs::read_to_string(&script).unwrap(),
        "#!/bin/sh\nexit 37\n"
    );

    let sidebar = tmux_stdout(
        server.label(),
        &[
            "split-window",
            "-d",
            "-h",
            "-t",
            &chat,
            "-P",
            "-F",
            "#{pane_id}",
            "/bin/sh",
        ],
    )
    .trim()
    .to_owned();
    tmux_stdout(
        server.label(),
        &["set-option", "-p", "-t", &sidebar, "@atm-sidebar", "1"],
    );
    let contaminated = workspace_pane_identity(server.label(), &chat);
    attach();
    assert_eq!(
        workspace_pane_identity(server.label(), &chat),
        contaminated,
        "reattach must not remove an old sidebar or restart chat"
    );
    tmux_stdout(
        server.label(),
        &["rename-window", "-t", &chat, "retained-chat"],
    );
    let fresh = workspace_chat_test_window(server.label(), &session, "chat", true);
    let panes = workspace_pane_identity(server.label(), &fresh);
    assert_eq!(panes.lines().count(), 1, "refreshed hook chat: {panes}");
    assert!(
        panes.trim().ends_with('|'),
        "refreshed hook added a sidebar: {panes}"
    );
}

/// Scenario 5a — `atm workspace create` should install the same long-lived
/// hooks as attach. In particular, after-new-window must inject a sidebar into
/// windows opened after creation. An isolated config names the pi stand-in,
/// whose launch in the agent pane is checked before the hooks.
async fn scenario_workspace_create(env: &E2eEnv) {
    let create_session_name = private_session_name("jghcrt");
    let create_socket_label = format!("atm-{create_session_name}");
    let create_data_dir = awkward_data_dir();
    let config_home = tempfile::tempdir().expect("create workspace config tempdir");
    let standin_dir = tempfile::tempdir().expect("create workspace stand-in tempdir");
    let log = standin_dir.path().join("pi.log");
    let pi = write_pi_standin(standin_dir.path(), &log);
    write_pi_default_config(config_home.path(), &pi);
    // Own cleanup before create can fail, and before its directories disappear.
    let create_tmux = PrivateTmux {
        socket_label: create_socket_label,
    };

    let deadline = Instant::now() + HANG_GUARD;
    let create_output = command_output(&mut workspace_create_command(
        env,
        &create_session_name,
        config_home.path(),
        create_data_dir.path(),
    ));
    assert!(
        create_output.status.success(),
        "atm workspace create failed: stdout={} stderr={}",
        String::from_utf8_lossy(&create_output.stdout),
        String::from_utf8_lossy(&create_output.stderr)
    );

    let agent_pane = top_non_sidebar_pane(create_tmux.label(), &create_session_name);
    let recorded = read_pi_record(create_tmux.label(), &agent_pane, &log, deadline);
    assert_eq!(
        recorded,
        format!("{agent_pane}\n\n"),
        "the stand-in must run in the agent pane with no arguments"
    );

    let installed_hooks = tmux_run_capture(
        create_tmux.label(),
        &["show-hooks", "-t", &create_session_name],
    )
    .expect("show-hooks after create");
    assert!(
        installed_hooks.contains("after-resize-window"),
        "after-resize-window hook should be installed after create; got: {installed_hooks}"
    );
    assert!(
        installed_hooks.contains("after-resize-pane"),
        "after-resize-pane hook should be installed after create; got: {installed_hooks}"
    );
    assert!(
        installed_hooks.contains("after-new-window"),
        "after-new-window hook should be installed after create; got: {installed_hooks}"
    );

    let initial_panes = list_panes_with_sidebar_marker(create_tmux.label())
        .expect("list panes with sidebar marker after create");
    let initial_sidebar_count = initial_panes.iter().filter(|p| p.is_sidebar).count();
    assert_eq!(
        initial_sidebar_count, 1,
        "created workspace should start with exactly one sidebar; panes: {initial_panes:?}"
    );

    // Regression: the resize script (prefix-R / after-resize-window) must
    // find the sidebar after the shell overwrites its pane title (e.g.
    // oh-my-zsh auto-title renames it to the running `atm --compact ...`).
    let sidebar_pane = initial_panes
        .iter()
        .find(|p| p.is_sidebar)
        .map(|p| p.pane_id.clone())
        .expect("sidebar pane present");
    tmux_run_capture(
        create_tmux.label(),
        &["select-pane", "-t", &sidebar_pane, "-T", "atm --compact"],
    )
    .expect("overwrite sidebar pane title");
    let window_width: u32 = tmux_run_capture(
        create_tmux.label(),
        &[
            "display-message",
            "-p",
            "-t",
            &sidebar_pane,
            "#{window_width}",
        ],
    )
    .expect("read window width")
    .trim()
    .parse()
    .expect("window width is a number");
    let expected_width = (window_width * 16 / 100).clamp(20, 40);
    let skewed_width = (expected_width + 5).to_string();
    // Park the after-resize-pane hook (it skips a sidebar at its 5-try cap)
    // so only prefix-R can restore the width.
    tmux_run_capture(
        create_tmux.label(),
        &[
            "set-option",
            "-p",
            "-t",
            &sidebar_pane,
            "@atm-sidebar-tries",
            "5",
        ],
    )
    .expect("park the after-resize-pane hook");
    tmux_run_capture(
        create_tmux.label(),
        &["resize-pane", "-t", &sidebar_pane, "-x", &skewed_width],
    )
    .expect("skew sidebar width");
    let sidebar_width = || {
        tmux_run_capture(
            create_tmux.label(),
            &[
                "display-message",
                "-p",
                "-t",
                &sidebar_pane,
                "#{pane_width}",
            ],
        )
        .expect("read sidebar width")
    };
    press_prefix_key(create_tmux.label(), &create_session_name, "R", || {
        sidebar_width().trim() == expected_width.to_string()
    })
    .await;
    let resized_width = sidebar_width();
    assert_eq!(
        resized_width.trim(),
        expected_width.to_string(),
        "prefix-R should restore the sidebar width even after its pane title changed"
    );
    tmux_run_capture(
        create_tmux.label(),
        &[
            "set-option",
            "-pu",
            "-t",
            &sidebar_pane,
            "@atm-sidebar-tries",
        ],
    )
    .expect("unpark the after-resize-pane hook");

    // Regression: Claude Code agent teams (tmux mode, verified against
    // 2.1.288 `createTeammatePaneWithLeader`/`rebalancePanesWithLeader`)
    // treat the first listed pane (our sidebar) as the leader: they split a
    // later pane, apply `main-vertical`, then resize the first pane to 30%.
    // The after-resize-pane hook must put the sidebar back.
    let window_target = format!("{create_session_name}:0");
    for _teammate in 0..2 {
        let window_panes = tmux_run_capture(
            create_tmux.label(),
            &["list-panes", "-t", &window_target, "-F", "#{pane_id}"],
        )
        .expect("list window panes");
        let others: Vec<&str> = window_panes.lines().skip(1).collect();
        let split_target = others[(others.len() - 1) / 2];
        let split_dir = if others.len() % 2 == 1 { "-v" } else { "-h" };
        tmux_run_capture(
            create_tmux.label(),
            &["split-window", "-d", "-t", split_target, split_dir],
        )
        .expect("split teammate pane");
        tmux_run_capture(
            create_tmux.label(),
            &["select-layout", "-t", &window_target, "main-vertical"],
        )
        .expect("apply main-vertical");
        let first_pane = tmux_run_capture(
            create_tmux.label(),
            &["list-panes", "-t", &window_target, "-F", "#{pane_id}"],
        )
        .expect("list window panes")
        .lines()
        .next()
        .expect("window has a pane")
        .to_string();
        tmux_run_capture(
            create_tmux.label(),
            &["resize-pane", "-t", &first_pane, "-x", "30%"],
        )
        .expect("resize first pane to 30%");
    }
    // Non-background run-shell hooks finish before the triggering command
    // returns, including nested resize hooks (verified on tmux 3.6 and 3.8).
    assert_eq!(
        sidebar_width().trim(),
        expected_width.to_string(),
        "after-resize-pane hook should restore sidebar width after Claude's rebalance"
    );

    // Below-target widths are restored too when attainable: in windows under
    // ~67 cols Claude's 30% is narrower than the 20-col minimum target.
    let read_sidebar = |format: &str| {
        tmux_run_capture(
            create_tmux.label(),
            &["display-message", "-p", "-t", &sidebar_pane, format],
        )
        .expect("read sidebar format")
        .trim()
        .to_string()
    };
    let narrow_width = (expected_width - 5).to_string();
    tmux_run_capture(
        create_tmux.label(),
        &["resize-pane", "-t", &sidebar_pane, "-x", &narrow_width],
    )
    .expect("narrow sidebar below target");
    assert_eq!(
        read_sidebar("#{pane_width}"),
        expected_width.to_string(),
        "after-resize-pane hook should restore a below-target sidebar"
    );

    // A window too narrow for the target width: the hook's own resize-pane
    // re-fires it, so it must give up after its retry cap instead of looping.
    tmux_run_capture(
        create_tmux.label(),
        &["resize-window", "-t", &create_session_name, "-x", "15"],
    )
    .expect("shrink window below sidebar minimum");
    assert_eq!(
        read_sidebar("#{@atm-sidebar-tries}"),
        "5",
        "hook should reach its retry cap in a too-small window"
    );
    // The recursive hook chain has exited. Deliberately re-enter at the cap:
    // completion, rather than a quiet second, proves another trigger cannot
    // keep incrementing the counter. A cap-of-six disposable mutant fails this.
    tmux_run_capture(
        create_tmux.label(),
        &[
            "resize-pane",
            "-t",
            &sidebar_pane,
            "-x",
            &expected_width.to_string(),
        ],
    )
    .expect("re-enter the resize hook at its cap");
    assert_eq!(
        read_sidebar("#{@atm-sidebar-tries}"),
        "5",
        "hook should stop retrying at the cap, not keep re-firing itself"
    );
    let full_width = window_width.to_string();
    tmux_run_capture(
        create_tmux.label(),
        &[
            "resize-window",
            "-t",
            &create_session_name,
            "-x",
            &full_width,
        ],
    )
    .expect("restore window width");
    assert_eq!(
        read_sidebar("#{pane_width}:#{@atm-sidebar-tries}"),
        format!("{expected_width}:"),
        "sidebar should recover (width:tries) after the window grows"
    );

    // Regression: prefix-a must focus the sidebar even after a layout
    // change moves it away from pane index 0.
    let other_pane = initial_panes
        .iter()
        .find(|p| !p.is_sidebar)
        .map(|p| p.pane_id.clone())
        .expect("non-sidebar pane present");
    tmux_run_capture(
        create_tmux.label(),
        &["swap-pane", "-s", &sidebar_pane, "-t", &other_pane],
    )
    .expect("swap sidebar out of index 0");
    tmux_run_capture(create_tmux.label(), &["select-pane", "-t", &other_pane])
        .expect("focus non-sidebar pane");
    // tmux 3.7 `list-keys` ignores a key filter, so find the binding line
    // and run its command via source-file (as a keypress would).
    let prefix_keys =
        tmux_run_capture(create_tmux.label(), &["list-keys", "-T", "prefix"]).expect("list keys");
    let prefix_a_cmd = prefix_keys
        .lines()
        .find_map(|line| {
            let mut parts = line.split_whitespace();
            (parts.next() == Some("bind-key")
                && parts.next() == Some("-T")
                && parts.next() == Some("prefix")
                && parts.next() == Some("a"))
            .then(|| parts.collect::<Vec<_>>().join(" "))
        })
        .expect("prefix-a binding installed");
    let prefix_a_file = create_data_dir.path().join("prefix-a.conf");
    std::fs::write(&prefix_a_file, &prefix_a_cmd).expect("write prefix-a command");
    tmux_run_capture(
        create_tmux.label(),
        &["source-file", &prefix_a_file.display().to_string()],
    )
    .expect("run prefix-a command");
    let active_pane = tmux_run_capture(
        create_tmux.label(),
        &[
            "display-message",
            "-p",
            "-t",
            &create_session_name,
            "#{pane_id}",
        ],
    )
    .expect("read active pane");
    assert_eq!(
        active_pane.trim(),
        sidebar_pane,
        "prefix-a ({prefix_a_cmd}) should focus the sidebar after it left index 0"
    );

    // A detached window must not inject another sidebar beside the client's
    // current pane. Different widths also catch an untargeted width query.
    let current_window = read_sidebar("#{window_id}");
    tmux_stdout(
        create_tmux.label(),
        &[
            "set-option",
            "-t",
            &create_session_name,
            "default-size",
            "200x40",
        ],
    );
    tmux_stdout(
        create_tmux.label(),
        &["resize-window", "-t", &current_window, "-x", "80"],
    );
    assert_eq!(read_sidebar("#{window_width}"), "80");
    let current_panes = tmux_stdout(
        create_tmux.label(),
        &[
            "list-panes",
            "-t",
            &current_window,
            "-F",
            "#{pane_id}:#{@atm-sidebar}",
        ],
    );
    let mut client = ControlClient::attach(create_tmux.label(), &create_session_name);
    let created = tmux_stdout(
        create_tmux.label(),
        &[
            "new-window",
            "-d",
            "-t",
            &create_session_name,
            "-P",
            "-F",
            "#{window_id}|#{pane_id}",
        ],
    );
    let (new_window, original_pane) = created
        .trim()
        .split_once('|')
        .expect("new window and pane IDs");
    assert_ne!(new_window, current_window);
    // Non-background run-shell completion is the barrier, as above; no
    // readiness polling can hide a sidebar injected into the wrong window.
    assert_eq!(
        tmux_stdout(
            create_tmux.label(),
            &[
                "list-panes",
                "-t",
                &current_window,
                "-F",
                "#{pane_id}:#{@atm-sidebar}"
            ],
        ),
        current_panes,
        "detached new-window must leave the client's current window untouched"
    );
    let new_panes =
        list_panes_with_sidebar_marker(create_tmux.label()).expect("list detached panes");
    let new_sidebars: Vec<_> = new_panes
        .iter()
        .filter(|pane| pane.window_id == new_window && pane.is_sidebar)
        .collect();
    assert_eq!(
        new_sidebars.len(),
        1,
        "one sidebar in the detached window: {new_panes:?}"
    );
    assert_eq!(
        tmux_stdout(
            create_tmux.label(),
            &[
                "display-message",
                "-p",
                "-t",
                &new_sidebars[0].pane_id,
                "#{window_width}:#{pane_width}"
            ],
        )
        .trim(),
        "200:32",
        "sidebar width must be calculated from the new window, not the current 80-column window"
    );
    assert_eq!(
        tmux_stdout(
            create_tmux.label(),
            &["display-message", "-p", "-t", new_window, "#{pane_id}"],
        )
        .trim(),
        original_pane,
        "restore the detached window's original pane, not a pane from the current window"
    );
    client
        .child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"display-message -p 'client-window=#{window_id}'\n")
        .unwrap();
    assert_eq!(
        client
            .until(|line| line.starts_with("client-window="))
            .expect("client window reply")
            .trim(),
        format!("client-window={current_window}"),
        "creating a detached window must not switch the attached client's window"
    );
    drop(client);

    let mut cmd = tmux_command();
    cmd.args([
        "-L",
        create_tmux.label(),
        "new-window",
        "-t",
        &create_session_name,
    ]);
    run_tmux_or_panic(&mut cmd, "new-window after workspace create");

    let panes = list_panes_with_sidebar_marker(create_tmux.label())
        .expect("list panes after create new-window");
    let mut windows_with_sidebar: std::collections::HashMap<String, u32> =
        std::collections::HashMap::new();
    for entry in &panes {
        if entry.is_sidebar {
            *windows_with_sidebar
                .entry(entry.window_id.clone())
                .or_insert(0) += 1;
        }
    }
    assert!(
        windows_with_sidebar.len() == 3 && windows_with_sidebar.values().all(|count| *count == 1),
        "new window should receive exactly one sidebar via after-new-window hook; panes: {panes:?}"
    );

    // An older hook line or a hand-run script may omit the window argument.
    // Keep a client attached so an empty target would resolve to its window.
    let _client = ControlClient::attach(create_tmux.label(), &create_session_name);
    let script = single_workspace_script(
        create_data_dir.path(),
        &format!("inject-sidebar-{create_session_name}.sh"),
    );
    let pane_set = || {
        tmux_stdout(
            create_tmux.label(),
            &[
                "list-panes",
                "-a",
                "-F",
                "#{window_id}:#{pane_id}:#{@atm-sidebar}:#{pane_width}:#{pane_active}",
            ],
        )
    };
    let mut changes = Vec::new();
    for args in [&[][..], &[""][..]] {
        let before = pane_set();
        // Process completion is the barrier for all script tmux commands.
        let output = command_output(Command::new(&script).args(args));
        assert!(
            output.status.success() && output.stdout.is_empty() && output.stderr.is_empty(),
            "a missing window must exit successfully and silently: {args:?}: {output:?}"
        );
        let after = pane_set();
        if after != before {
            changes.push(format!("{args:?}: {before:?} -> {after:?}"));
        }
    }
    assert!(
        changes.is_empty(),
        "sidebar script without a window must leave every window unchanged: {changes:?}"
    );
}

/// Scenario 5b — `atm workspace attach` against a separate isolated
/// server (attach's `--isolate` flag hardcodes the socket label as
/// `atm-<sessname>`, so it can't share the primary scenario server).
/// Asserts every window gets an `@atm-sidebar=1` pane, both hooks are
/// installed, and the second invocation is idempotent and runs the resize
/// script once.
async fn scenario_workspace_attach(env: &E2eEnv) {
    let attach_session_name = private_session_name("jghatt");
    let attach_socket_label = format!("atm-{attach_session_name}");
    // Hooks/scripts go to a sandboxed XDG_DATA_HOME so the test never
    // writes to `~/.local/share/atm/`.
    let attach_data_dir = awkward_data_dir();

    let attach_tmux = PrivateTmux::start_with_label_session_env(
        &attach_socket_label,
        &attach_session_name,
        std::iter::empty::<(&str, &str)>(),
    );

    // Build a multi-window session — the interesting case for attach,
    // since per-window sidebar injection is the loop attach owns.
    let tmux_attach_client = RealTmuxClient::with_socket(attach_tmux.label().to_string());
    for label in ["new-window (2nd)", "new-window (3rd)"] {
        let mut cmd = tmux_command();
        cmd.args([
            "-L",
            attach_tmux.label(),
            "new-window",
            "-t",
            &attach_session_name,
        ]);
        run_tmux_or_panic(&mut cmd, label);
    }

    let pre_attach_panes = tmux_attach_client
        .list_panes()
        .await
        .expect("list panes pre-attach");
    let pre_attach_window_count = pre_attach_panes
        .iter()
        .map(|p| p.window_index)
        .collect::<std::collections::HashSet<_>>()
        .len();
    assert_eq!(
        pre_attach_window_count, 3,
        "expected 3 windows in the simulated workspace, got {pre_attach_window_count}"
    );

    // Attach must span the window, not only the first pane: cover a single
    // pane, a top/bottom split and a 2x2 grid at a known geometry.
    for (index, pane) in pre_attach_panes.iter().enumerate() {
        tmux_stdout(
            attach_tmux.label(),
            &[
                "resize-window",
                "-t",
                &pane.pane_id,
                "-x",
                "180",
                "-y",
                "85",
            ],
        );
        if index == 0 {
            continue;
        }
        let below = tmux_stdout(
            attach_tmux.label(),
            &[
                "split-window",
                "-vd",
                "-t",
                &pane.pane_id,
                "-P",
                "-F",
                "#{pane_id}",
            ],
        );
        if index == 2 {
            for target in [&pane.pane_id, below.trim()] {
                tmux_stdout(attach_tmux.label(), &["split-window", "-hd", "-t", target]);
            }
        }
    }
    let content_panes = tmux_attach_client
        .list_panes()
        .await
        .expect("split content panes");
    assert_eq!(content_panes.len(), 7);

    let attach_output = command_output(
        Command::new(&env.atm_path)
            .args(["workspace", "attach", &attach_session_name, "--isolate"])
            .env("ATM_NO_ATTACH", "1")
            .env("XDG_DATA_HOME", attach_data_dir.path())
            .env("XDG_STATE_HOME", &env.daemon.state_dir)
            .env("ATM_SOCKET", &env.daemon.socket_path),
    );
    assert!(
        attach_output.status.success(),
        "atm workspace attach failed: stdout={} stderr={}",
        String::from_utf8_lossy(&attach_output.stdout),
        String::from_utf8_lossy(&attach_output.stderr)
    );

    // Each window should now contain a pane tagged `@atm-sidebar=1`.
    let post_attach_panes = list_panes_with_sidebar_marker(attach_tmux.label())
        .expect("list panes with sidebar marker post-attach");

    let mut windows_with_sidebar: std::collections::HashMap<String, u32> =
        std::collections::HashMap::new();
    for entry in &post_attach_panes {
        if entry.is_sidebar {
            *windows_with_sidebar
                .entry(entry.window_id.clone())
                .or_insert(0) += 1;
        }
    }

    assert_eq!(
        windows_with_sidebar.len(),
        3,
        "every one of the 3 windows should have an @atm-sidebar pane; got {} \
         (full pane list: {:?})",
        windows_with_sidebar.len(),
        post_attach_panes
    );
    for (window, count) in &windows_with_sidebar {
        assert_eq!(
            *count, 1,
            "window {window} should have exactly one sidebar pane, got {count}"
        );
    }

    for sidebar in post_attach_panes.iter().filter(|pane| pane.is_sidebar) {
        assert_eq!(
            tmux_stdout(
                attach_tmux.label(),
                &[
                    "display-message",
                    "-p",
                    "-t",
                    &sidebar.pane_id,
                    "#{pane_left}:#{pane_top}:#{pane_width}:#{pane_height}:#{window_height}"
                ],
            )
            .trim(),
            "0:0:28:85:85",
            "sidebar must cover the full height of an already split window"
        );
    }
    for pane in &content_panes {
        assert!(post_attach_panes
            .iter()
            .any(|after| after.pane_id == pane.pane_id));
        let left: u32 = tmux_stdout(
            attach_tmux.label(),
            &["display-message", "-p", "-t", &pane.pane_id, "#{pane_left}"],
        )
        .trim()
        .parse()
        .expect("content pane left");
        assert!(
            left >= 29,
            "content pane must not extend underneath the sidebar"
        );
    }

    // Verify both hooks attach is responsible for are installed on the
    // session (the divergence with create lives here: create installs
    // only `after-resize-window`).
    let installed_hooks = tmux_run_capture(
        attach_tmux.label(),
        &["show-hooks", "-t", &attach_session_name],
    )
    .expect("show-hooks");
    assert!(
        installed_hooks.contains("after-resize-window"),
        "after-resize-window hook should be installed; got: {installed_hooks}"
    );
    assert!(
        installed_hooks.contains("after-new-window"),
        "after-new-window hook (only attach installs this; see divergence \
         note in cmd_workspace) should be present; got: {installed_hooks}"
    );

    // Every non-current window must be corrected on reattach, not the
    // attached client's unrelated session. Park hooks at their retry cap so
    // command completion, not a delayed hook, proves the one-off correction.
    let other_session = format!("attach-other-{}", std::process::id());
    let other_pane = tmux_stdout(
        attach_tmux.label(),
        &[
            "new-session",
            "-d",
            "-s",
            &other_session,
            "-P",
            "-F",
            "#{pane_id}",
        ],
    )
    .trim()
    .to_string();
    let _client = ControlClient::attach(attach_tmux.label(), &other_session);
    let other_geometry = || {
        tmux_stdout(
            attach_tmux.label(),
            &[
                "display-message",
                "-p",
                "-t",
                &other_pane,
                "#{pane_id}:#{pane_left}:#{pane_top}:#{pane_width}:#{pane_height}",
            ],
        )
    };
    let other_before = other_geometry();
    let sidebars: Vec<_> = post_attach_panes
        .iter()
        .filter(|pane| pane.is_sidebar)
        .collect();
    for sidebar in &sidebars {
        tmux_stdout(
            attach_tmux.label(),
            &[
                "set-option",
                "-p",
                "-t",
                &sidebar.pane_id,
                "@atm-sidebar-tries",
                "5",
            ],
        );
        tmux_stdout(
            attach_tmux.label(),
            &["resize-pane", "-t", &sidebar.pane_id, "-x", "38"],
        );
        assert_eq!(
            tmux_stdout(
                attach_tmux.label(),
                &[
                    "display-message",
                    "-p",
                    "-t",
                    &sidebar.pane_id,
                    "#{pane_width}"
                ]
            )
            .trim(),
            "38"
        );
    }

    // Idempotency: running attach a second time must NOT add another
    // sidebar to any window. The production code keys off `@atm-sidebar`.
    let attach_output_2 = command_output(
        Command::new(&env.atm_path)
            .args(["workspace", "attach", &attach_session_name, "--isolate"])
            .env("TMUX_PANE", &other_pane)
            .env("ATM_NO_ATTACH", "1")
            .env("XDG_DATA_HOME", attach_data_dir.path())
            .env("XDG_STATE_HOME", &env.daemon.state_dir)
            .env("ATM_SOCKET", &env.daemon.socket_path),
    );
    assert!(
        attach_output_2.status.success(),
        "second attach failed: stderr={}",
        String::from_utf8_lossy(&attach_output_2.stderr)
    );
    for sidebar in &sidebars {
        assert_eq!(
            tmux_stdout(attach_tmux.label(), &["display-message", "-p", "-t", &sidebar.pane_id, "#{pane_width}"]).trim(),
            "28",
            "reattach must resize every explicitly attached window, not the client's current window"
        );
    }
    assert_eq!(
        other_geometry(),
        other_before,
        "reattach must leave the other session untouched"
    );

    let final_panes = list_panes_with_sidebar_marker(attach_tmux.label())
        .expect("list panes after second attach");
    assert_eq!(
        final_panes
            .iter()
            .filter(|pane| pane.pane_id != other_pane)
            .map(|pane| &pane.pane_id)
            .collect::<std::collections::HashSet<_>>(),
        post_attach_panes
            .iter()
            .map(|pane| &pane.pane_id)
            .collect::<std::collections::HashSet<_>>(),
        "reattach must preserve every content and sidebar pane"
    );
    let final_sidebar_count = final_panes.iter().filter(|p| p.is_sidebar).count();
    assert_eq!(
        final_sidebar_count, 3,
        "second attach should be a no-op for already-injected windows, \
         but ended up with {final_sidebar_count} sidebars"
    );
}

/// Resize hooks and prefix-R must use their window, even when a different
/// workspace is current and the server environment leaks its TMUX_PANE.
async fn scenario_workspace_resize_targets_window(env: &E2eEnv) {
    let label = PrivateTmux::fresh_label();
    let data = awkward_data_dir();
    let tmux = PrivateTmux::start_with_label_session_env(
        &label,
        "resize-alpha",
        [("ATM_SOCKET", env.daemon.socket_path.as_os_str())],
    );
    let beta_shell = tmux_stdout(
        &label,
        &[
            "new-session",
            "-d",
            "-s",
            "resize-beta",
            "-P",
            "-F",
            "#{pane_id}",
        ],
    )
    .trim()
    .to_string();
    for session in ["resize-alpha", "resize-beta"] {
        tmux_stdout(
            &label,
            &[
                "resize-window",
                "-t",
                &format!("={session}:"),
                "-x",
                "180",
                "-y",
                "85",
            ],
        );
        let output = command_output(
            Command::new(&env.atm_path)
                .args(["workspace", "attach", session])
                .env("ATM_TMUX_SOCKET", &label)
                .env("ATM_NO_ATTACH", "1")
                .env("ATM_SOCKET", &env.daemon.socket_path)
                .env("XDG_DATA_HOME", data.path())
                .env("XDG_STATE_HOME", &env.daemon.state_dir),
        );
        assert!(output.status.success(), "attach {session}: {output:?}");
    }
    let sidebar = |session: &str| {
        tmux_stdout(
            &label,
            &[
                "list-panes",
                "-t",
                &format!("={session}:"),
                "-F",
                "#{@atm-sidebar} #{pane_id}",
            ],
        )
        .lines()
        .find_map(|line| line.strip_prefix("1 ").map(str::to_string))
        .expect("workspace sidebar")
    };
    let alpha = sidebar("resize-alpha");
    let beta = sidebar("resize-beta");
    tmux_stdout(&label, &["select-pane", "-t", &beta_shell]);
    let mut client = ControlClient::attach(&label, "resize-beta");
    tmux_stdout(&label, &["set-environment", "-g", "TMUX_PANE", &beta_shell]);
    let beta_geometry =
        || {
            tmux_stdout(&label, &["list-panes", "-t", "=resize-beta:", "-F",
        "#{pane_id}:#{pane_left}:#{pane_top}:#{pane_width}:#{pane_height}:#{pane_active}"])
        };
    let before = beta_geometry();
    let width = || {
        tmux_stdout(
            &label,
            &["display-message", "-p", "-t", &alpha, "#{pane_width}"],
        )
    };
    tmux_stdout(&label, &["resize-window", "-t", &alpha, "-x", "240"]);
    assert_eq!(
        width().trim(),
        "38",
        "resize-window must correct the triggering window"
    );
    assert_eq!(
        beta_geometry(),
        before,
        "resize-window must not change the other workspace"
    );
    tmux_stdout(&label, &["resize-pane", "-t", &alpha, "-x", "50"]);
    assert_eq!(
        width().trim(),
        "38",
        "resize-pane must correct the triggering window"
    );
    assert_eq!(
        beta_geometry(),
        before,
        "resize-pane must not change the other workspace"
    );

    // Prefix-R runs outside hook mode and repairs a sidebar even at its cap.
    tmux_stdout(
        &label,
        &["set-option", "-p", "-t", &alpha, "@atm-sidebar-tries", "5"],
    );
    tmux_stdout(&label, &["resize-pane", "-t", &alpha, "-x", "50"]);
    assert_eq!(
        width().trim(),
        "50",
        "hook must stay parked at its retry cap"
    );
    let keys = tmux_stdout(&label, &["list-keys", "-T", "prefix"]);
    let command = keys
        .lines()
        .find_map(|line| {
            let mut words = line.split_whitespace();
            (words.next() == Some("bind-key")
                && words.next() == Some("-T")
                && words.next() == Some("prefix")
                && words.next() == Some("R"))
            .then(|| words.collect::<Vec<_>>().join(" "))
        })
        .expect("prefix-R command");
    client.child.stdin.as_mut().expect("control stdin").write_all(
        format!("switch-client -t '=resize-alpha:'\n{command}\ndisplay-message -p resize-key-done\n").as_bytes()
    ).expect("run prefix-R in its client's queue");
    client
        .until(|line| line.trim() == "resize-key-done")
        .expect("prefix-R completion");
    assert_eq!(
        width().trim(),
        "38",
        "prefix-R must resize its client's window"
    );
    assert_eq!(
        beta_geometry(),
        before,
        "prefix-R must not follow leaked TMUX_PANE"
    );

    // Empty -t falls back to current on both supported tmux versions. Leave
    // that other pane deliberately skewed so removing the guard changes it.
    let pane_set = || {
        tmux_stdout(&label, &["list-panes", "-a", "-F",
        "#{pane_id}:#{pane_left}:#{pane_top}:#{pane_width}:#{pane_height}:#{@atm-sidebar-tries}"])
    };
    let script = single_workspace_script(data.path(), "resize-sidebar-resize-alpha.sh");
    let mut changed = Vec::new();
    for args in [&[][..], &[""][..]] {
        tmux_stdout(
            &label,
            &["set-option", "-p", "-t", &beta, "@atm-sidebar-tries", "5"],
        );
        tmux_stdout(&label, &["resize-pane", "-t", &beta, "-x", "38"]);
        let before = pane_set();
        let output = command_output(
            Command::new(&script)
                .args(args)
                .env("TMUX_PANE", &beta_shell),
        );
        assert!(
            output.status.success() && output.stdout.is_empty() && output.stderr.is_empty(),
            "{args:?}: {output:?}"
        );
        if pane_set() != before {
            changed.push(format!("{args:?}"));
        }
    }
    assert!(
        changed.is_empty(),
        "resize without a window changed panes: {changed:?}"
    );
    drop(client);
    drop(tmux);
}

/// Scenario 5c — `atm workspace attach` with no name auto-picks the most
/// recently attached session. A picked name with a space must be rejected
/// before anything is installed, as create and a named attach already do:
/// the run-shell hooks and sidebar keys embed it unquoted. Renaming it to a
/// supported name (what the error suggests) then attaches.
async fn scenario_workspace_attach_autopick_rejects_unsupported_name(env: &E2eEnv) {
    let label = PrivateTmux::fresh_label();
    let data_dir = tempfile::tempdir().expect("create autopick data tempdir");
    let tmux = PrivateTmux::start_with_label_session_env(
        &label,
        "my work",
        [("ATM_SOCKET", env.daemon.socket_path.as_os_str())],
    );

    // A detached session has an empty #{session_last_attached}, which the
    // auto-pick skips. A control-mode client attaches without a tty and
    // exits when stdin closes, leaving "my work" the most recently attached.
    // The deadline turns a client that ignores EOF into a failure, not a hang.
    let mut ctl = tmux_command()
        .args([
            "-L",
            tmux.label(),
            "-C",
            "attach-session",
            "-t",
            "=my work:",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn control-mode attach");
    wait_child(&mut ctl, Instant::now() + HANG_GUARD)
        .expect("control-mode attach to 'my work' must exit on stdin EOF");
    let ctl = ctl.wait_with_output().expect("collect control-mode attach");
    assert!(
        ctl.status.success(),
        "control-mode attach to 'my work' failed: {}",
        String::from_utf8_lossy(&ctl.stderr)
    );

    let run_autopick_attach = || {
        command_output(
            Command::new(&env.atm_path)
                .args(["workspace", "attach"])
                // With no name and no --isolate, attach uses the default server:
                // a bogus $TMUX keeps a missed ATM_TMUX_SOCKET off the developer's.
                .env("TMUX", "atm-e2e-fixture-no-real-tmux-here")
                .env("ATM_TMUX_SOCKET", tmux.label())
                .env("ATM_NO_ATTACH", "1")
                .env("XDG_DATA_HOME", data_dir.path())
                .env("XDG_STATE_HOME", &env.daemon.state_dir)
                .env("ATM_SOCKET", &env.daemon.socket_path),
        )
    };

    let rejected = run_autopick_attach();
    let stderr = String::from_utf8_lossy(&rejected.stderr);
    assert!(
        !rejected.status.success(),
        "attach should reject the auto-picked 'my work'; stdout={} stderr={stderr}",
        String::from_utf8_lossy(&rejected.stdout),
    );
    assert!(
        stderr.contains("'my work' contains unsupported characters"),
        "error should name the picked session; stderr={stderr}"
    );
    assert!(
        stderr.contains("tmux rename-session"),
        "error should say how to fix the name; stderr={stderr}"
    );

    let panes =
        list_panes_with_sidebar_marker(tmux.label()).expect("list panes after rejected attach");
    assert!(
        panes.iter().all(|p| !p.is_sidebar),
        "a rejected attach must not inject a sidebar; panes: {panes:?}"
    );
    let hooks = tmux_run_capture(tmux.label(), &["show-hooks", "-t", "=my work:"])
        .expect("show-hooks after rejected attach");
    assert!(
        hooks.trim().is_empty(),
        "a rejected attach must not install any session hook; got: {hooks}"
    );
    let keys =
        tmux_run_capture(tmux.label(), &["list-keys"]).expect("list-keys after rejected attach");
    assert!(
        !keys.contains("resize-sidebar") && !keys.contains("@atm-sidebar"),
        "a rejected attach must not bind prefix-R or prefix-a; got: {keys}"
    );
    assert!(
        !data_dir.path().join("atm").exists(),
        "a rejected attach must not write hook scripts"
    );

    // Control: the same session under a supported name attaches.
    tmux_run_capture(
        tmux.label(),
        &["rename-session", "-t", "=my work:", "mywork"],
    )
    .expect("rename 'my work' to 'mywork'");
    let attached = run_autopick_attach();
    assert!(
        attached.status.success(),
        "auto-picked 'mywork' should attach; stdout={} stderr={}",
        String::from_utf8_lossy(&attached.stdout),
        String::from_utf8_lossy(&attached.stderr)
    );
    let panes =
        list_panes_with_sidebar_marker(tmux.label()).expect("list panes after control attach");
    assert_eq!(
        panes.iter().filter(|p| p.is_sidebar).count(),
        1,
        "control attach should inject one sidebar; panes: {panes:?}"
    );
    let hooks = tmux_run_capture(tmux.label(), &["show-hooks", "-t", "=mywork:"])
        .expect("show-hooks after control attach");
    for hook in [
        "after-resize-window",
        "after-resize-pane",
        "after-new-window",
    ] {
        assert!(
            hooks.contains(hook),
            "control attach should install {hook}; got: {hooks}"
        );
    }
    let keys =
        tmux_run_capture(tmux.label(), &["list-keys"]).expect("list-keys after control attach");
    assert!(
        keys.contains("resize-sidebar-mywork.sh") && keys.contains("@atm-sidebar"),
        "control attach should bind prefix-R and prefix-a; got: {keys}"
    );
}

/// Scenario 5d — `atm workspace create` launches the configured default
/// harness in the agent pane, not always `claude`, with no prompt or model.
async fn scenario_workspace_create_launches_configured_harness(env: &E2eEnv) {
    let args = create_workspace_with_pi_default(env, "jghpi", None).await;
    assert_eq!(args, "", "pi should start with no arguments");
}

/// Scenario 5f — an `ATM_SPAWN_*_ARGS` override ending in `;` still launches
/// the harness. tmux reads an argument ending in `;` as a command separator,
/// so sending the command and `Enter` in one `send-keys` fails with
/// `unknown command: Enter` (tmux 3.8) after the session already exists.
async fn scenario_workspace_create_sends_args_ending_in_semicolon(env: &E2eEnv) {
    let args = create_workspace_with_pi_default(env, "jghsemi", Some("x pi;")).await;
    assert_eq!(args, "x pi", "pi should get the override's arguments");
}

/// Runs `atm workspace create --isolate` with an isolated config whose
/// default is `pi`, asserts the stand-in `pi` ran in the agent pane, and
/// returns the arguments it got. The stand-in goes in by absolute path
/// through `[harness.pi] binary`, for scenario 4's reason: shell init can
/// drop a directory prepended to PATH.
async fn create_workspace_with_pi_default(
    env: &E2eEnv,
    prefix: &str,
    spawn_pi_args: Option<&str>,
) -> String {
    let session_name = private_session_name(prefix);
    let data_dir = tempfile::tempdir().expect("create pi data tempdir");
    let config_home = tempfile::tempdir().expect("create pi config tempdir");
    let standin_dir = tempfile::tempdir().expect("create pi stand-in tempdir");
    // Kills the workspace's server however the scenario ends, before the
    // dirs above go.
    let tmux = PrivateTmux {
        socket_label: format!("atm-{session_name}"),
    };
    let log = standin_dir.path().join("pi.log");
    let pi = write_pi_standin(standin_dir.path(), &log);
    write_pi_default_config(config_home.path(), &pi);

    let mut create =
        workspace_create_command(env, &session_name, config_home.path(), data_dir.path());
    if let Some(args) = spawn_pi_args {
        create.env("ATM_SPAWN_PI_ARGS", args);
    }
    let deadline = Instant::now() + HANG_GUARD;
    let output = command_output(&mut create);
    assert!(
        output.status.success(),
        "atm workspace create with a pi default failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let agent_pane = top_non_sidebar_pane(tmux.label(), &session_name);
    let recorded = read_pi_record(tmux.label(), &agent_pane, &log, deadline);
    let mut lines = recorded.lines();
    assert_eq!(
        lines.next(),
        Some(agent_pane.as_str()),
        "the configured pi should run in the agent pane"
    );
    lines.next().unwrap_or_default().to_string()
}

/// Waits for the stand-in's atomic record, diagnosing a missing signal with
/// its stage log and the agent pane's actual contents.
fn read_pi_record(socket_label: &str, agent_pane: &str, log: &Path, deadline: Instant) -> String {
    // Latched even if the stand-in completes before this client starts.
    if let Err(error) = wait_for_signal(socket_label, "pi-recorded", deadline) {
        let screen = tmux_run_capture(socket_label, &["capture-pane", "-p", "-t", agent_pane])
            .unwrap_or_default();
        let stages = std::fs::read_to_string(log.with_extension("stages")).unwrap_or_default();
        panic!(
            "pi record completion: {error}; stages:\n{stages}\nagent pane {agent_pane}:\n{screen}"
        );
    }
    std::fs::read_to_string(log).expect("pi signalled after atomically writing its record")
}

/// `atm workspace create --name <session> --isolate`, without attaching,
/// reading its config from `config_home` and writing scripts to `data_dir`.
/// The runner's tmux variables must not reach the workspace's server or its
/// hook jobs, and its `ATM_SPAWN_*` would change the agent command.
fn workspace_create_command(
    env: &E2eEnv,
    session: &str,
    config_home: &Path,
    data_dir: &Path,
) -> Command {
    let mut create = Command::new(&env.atm_path);
    create
        .args(["workspace", "create", "--name", session, "--isolate"])
        .env("ATM_NO_ATTACH", "1")
        .env("XDG_CONFIG_HOME", config_home)
        .env("XDG_DATA_HOME", data_dir)
        .env("XDG_STATE_HOME", &env.daemon.state_dir)
        .env("ATM_SOCKET", &env.daemon.socket_path);
    for key in [
        "TMUX",
        "TMUX_PANE",
        "ATM_TMUX_SOCKET",
        "ATM_SPAWN_BIN",
        "ATM_SPAWN_ARGS",
        "ATM_SPAWN_CLAUDE_BIN",
        "ATM_SPAWN_CLAUDE_ARGS",
        "ATM_SPAWN_PI_BIN",
        "ATM_SPAWN_PI_ARGS",
    ] {
        create.env_remove(key);
    }
    create
}

/// The agent pane, found from the layout rather than focus: the topmost pane
/// of `session` that isn't the sidebar. The sidebar is full height on the
/// left and the shell sits below the agent.
fn top_non_sidebar_pane(socket_label: &str, session: &str) -> String {
    let panes = tmux_run_capture(
        socket_label,
        &[
            "list-panes",
            "-s",
            "-t",
            &format!("={session}:"),
            "-F",
            "#{pane_top} #{pane_id} #{@atm-sidebar}",
        ],
    )
    .expect("list the workspace panes");
    let mut candidates: Vec<(u32, &str)> = panes
        .lines()
        .filter_map(|line| {
            let mut fields = line.split(' ');
            let top = fields.next()?.parse().ok()?;
            let id = fields.next()?;
            (fields.next() != Some("1")).then_some((top, id))
        })
        .collect();
    candidates.sort_unstable();
    match candidates.as_slice() {
        [(top, id), rest @ ..] if rest.iter().all(|(other, _)| other > top) => id.to_string(),
        _ => panic!("expected one topmost non-sidebar pane; panes:\n{panes}"),
    }
}

/// Scenario 5e — an unknown configured harness fails `atm workspace create`
/// before it runs tmux, so no server, session, hook or script is left behind.
async fn scenario_workspace_create_rejects_unknown_harness(env: &E2eEnv) {
    let session_name = private_session_name("jghbad");
    let data_dir = tempfile::tempdir().expect("create unknown-harness data tempdir");
    let config_home = tempfile::tempdir().expect("create unknown-harness config tempdir");
    // Kills the server if create wrongly made one.
    let tmux = PrivateTmux {
        socket_label: format!("atm-{session_name}"),
    };
    write_atm_config(config_home.path(), "[harness]\ndefault = 'nope'\n");

    let output = command_output(&mut workspace_create_command(
        env,
        &session_name,
        config_home.path(),
        data_dir.path(),
    ));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "an unknown default harness should fail workspace create; stderr={stderr}"
    );
    assert!(
        stderr.contains("unknown harness 'nope'"),
        "the error should name the unknown harness; stderr={stderr}"
    );
    // list-sessions starts no server, so failing means none exists: no
    // session and no hooks. Earlier scenarios prove tmux itself runs.
    let sessions = tmux_run_capture(tmux.label(), &["list-sessions"]);
    assert!(
        sessions.is_none(),
        "no tmux server should exist on {}; sessions: {sessions:?}",
        tmux.label()
    );
    let scripts = data_dir.path().join("atm");
    assert!(
        !scripts.exists(),
        "no workspace script should be written; found {:?}",
        std::fs::read_dir(&scripts).map(|d| d.flatten().map(|e| e.file_name()).collect::<Vec<_>>())
    );
}

/// Selects the absolute pi stand-in as the default in the isolated config.
fn write_pi_default_config(config_home: &Path, pi: &Path) {
    write_atm_config(
        config_home,
        &format!(
            "[harness]\ndefault = 'pi'\n\n[harness.pi]\nbinary = '{}'\n",
            pi.display()
        ),
    );
}

/// Writes `atm/config.toml` under an isolated `XDG_CONFIG_HOME`.
fn write_atm_config(config_home: &Path, toml: &str) {
    let dir = config_home.join("atm");
    std::fs::create_dir_all(&dir).expect("create atm config dir");
    std::fs::write(dir.join("config.toml"), toml).expect("write atm config");
}

/// Records argv atomically, then signals completion on its own tmux server.
/// The stage log distinguishes never-executed, write/rename and signal failures.
/// Stdin holds the pane alive until explicit teardown, not a timed sleep.
fn write_pi_standin(dir: &Path, log: &Path) -> PathBuf {
    let path = dir.join("pi");
    let stages = log.with_extension("stages").display().to_string();
    let real_tmux = tmux_on_path()
        .expect("tmux available")
        .display()
        .to_string();
    let log = log.display().to_string();
    // Both paths go inside single quotes: in the script and in TOML.
    assert!(
        !log.contains('\''),
        "stand-in paths must not contain a single quote: {log}"
    );
    let body = format!(
        "#!/bin/sh\nset -e\nprintf 'entered pid=%s pane=%s\\n' \"$$\" \"$TMUX_PANE\" > '{stages}'\nprintf '%s\\n%s\\n' \"$TMUX_PANE\" \"$*\" > '{log}.tmp'\nprintf 'record written\\n' >> '{stages}'\nmv '{log}.tmp' '{log}'\nprintf 'record renamed\\n' >> '{stages}'\n'{real_tmux}' -S \"${{TMUX%%,*}}\" wait-for -S pi-recorded\nprintf 'completion signalled\\n' >> '{stages}'\nexec cat >/dev/null\n"
    );
    std::fs::write(&path, body).expect("write pi stand-in");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("chmod pi stand-in");
    path
}

/// Scenario 5g — with no config, create types exactly `claude` into the agent
/// pane, then `Enter` in its own `send-keys`: the same keys it typed before it
/// read the config. A logging `tmux` first on atm's PATH records every argv
/// and forwards it to the real tmux, except that lone `Enter`, so the pane
/// never runs a harness.
async fn scenario_workspace_create_sends_default_keys(env: &E2eEnv) {
    let session_name = private_session_name("jghkeys");
    let data_dir = tempfile::tempdir().expect("create keys data tempdir");
    let config_home = tempfile::tempdir().expect("create keys config tempdir");
    let wrapper_dir = tempfile::tempdir().expect("create tmux wrapper tempdir");
    // Kills the workspace's server however the scenario ends, before the
    // dirs above go.
    let tmux = PrivateTmux {
        socket_label: format!("atm-{session_name}"),
    };
    let log = wrapper_dir.path().join("tmux-argv.log");
    write_tmux_logging_wrapper(wrapper_dir.path(), &log);
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    let path = std::env::join_paths(
        std::iter::once(wrapper_dir.path().to_path_buf()).chain(std::env::split_paths(&path_var)),
    )
    .expect("join PATH");

    // config_home stays empty: no config.toml, so the built-in default applies.
    let output = command_output(
        workspace_create_command(env, &session_name, config_home.path(), data_dir.path())
            .env("PATH", &path),
    );
    assert!(
        output.status.success(),
        "atm workspace create with no config failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let agent_pane = top_non_sidebar_pane(tmux.label(), &session_name);
    let logged = std::fs::read_to_string(&log).expect("read the tmux argv log");
    let target = format!("[send-keys] [-t] [{agent_pane}] ");
    let agent_keys: Vec<&str> = logged.lines().filter(|l| l.contains(&target)).collect();
    // `-u`: tmux prints UTF-8 whatever the caller's locale (#138).
    let call = format!("[-u] [-L] [{}] {target}", tmux.label());
    assert_eq!(
        agent_keys,
        [
            format!("{call}[claude]"),
            format!("{call}[Enter] swallowed")
        ],
        "create should send `claude`, then `Enter` alone, to agent pane {agent_pane}; \
         tmux argv log:\n{logged}"
    );
}

/// Writes a `tmux` into `dir` that appends its argv to `log`, one `[arg]` per
/// argument, then execs the real tmux. A lone `send-keys -t <pane> Enter`
/// is logged with ` swallowed` and not forwarded, whatever global flags
/// come before it (`-u -L <label>`), so a change to those flags can't let
/// the Enter reach the pane and start a harness.
fn write_tmux_logging_wrapper(dir: &Path, log: &Path) {
    let real = tmux_on_path().expect("tmux on PATH (checked at setup)");
    let (real, log) = (real.display().to_string(), log.display().to_string());
    // Both paths go inside single quotes in the script.
    assert!(
        !real.contains('\'') && !log.contains('\''),
        "wrapper paths must not contain a single quote: {real} {log}"
    );
    let body = format!(
        "#!/bin/sh\nline=\na=\nb=\nc=\nd=\n\
         for arg in \"$@\"; do line=\"$line [$arg]\"; a=$b; b=$c; c=$d; d=$arg; done\n\
         if [ \"$a\" = send-keys ] && [ \"$b\" = -t ] && [ \"$d\" = Enter ]; then\n\
         \x20 printf '%s swallowed\\n' \"${{line# }}\" >> '{log}'\n\x20 exit 0\nfi\n\
         printf '%s\\n' \"${{line# }}\" >> '{log}'\nexec '{real}' \"$@\"\n"
    );
    let path = dir.join("tmux");
    std::fs::write(&path, body).expect("write tmux wrapper");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("chmod tmux wrapper");
}

const PREVIEW_MARKERS: [&str; 3] = ["preview-marker-a", "preview-marker-b", "preview-marker-c"];

/// Only a hang guard; success is observed in the rendered screen.
const PREVIEW_STEP_TIMEOUT: Duration = HANG_GUARD;

/// Runs `tmux -L <label> <args>` and returns stdout.
fn tmux_stdout(socket_label: &str, args: &[&str]) -> String {
    let stdout = run_tmux_or_panic(
        tmux_command().args(["-L", socket_label]).args(args),
        args.first().copied().unwrap_or("tmux"),
    );
    String::from_utf8_lossy(&stdout).trim_end().to_string()
}

/// A stand-in daemon on the existing session connection. The scenario still
/// supplies session events; capture and membership execute on the daemon side.
struct PreviewDaemonPeer {
    stream: UnixStream,
    writes: std::sync::Arc<std::sync::Mutex<()>>,
    panes_requests: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

fn fake_tui_daemon(
    socket: &Path,
    sessions: Vec<SessionView>,
    capture_tmux: PathBuf,
    label: String,
    captures: std::sync::Arc<std::sync::Mutex<Vec<Value>>>,
) -> mpsc::Receiver<PreviewDaemonPeer> {
    let listener = UnixListener::bind(socket).expect("bind");
    let connected = serde_json::to_string(&DaemonMessage::Connected {
        protocol_version: ProtocolVersion::new(1, 1),
        client_id: "tui".into(),
        methods: vec!["session.capture".into(), "session.panes".into()],
    })
    .expect("serialize");
    let list = serde_json::to_string(&DaemonMessage::session_list(sessions)).expect("serialize");
    let (subscriber_tx, subscriber_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let deadline = Instant::now() + HANG_GUARD;
        loop {
            readable(listener.as_raw_fd(), deadline).expect("TUI connection before guard");
            let (mut stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            // `atm` first checks that something listens, then hangs up.
            match read_frame(&mut reader, deadline) {
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => continue,
                result => {
                    result.expect("TUI Connect");
                }
            }
            write_bytes(&mut stream, format!("{connected}\n").as_bytes(), deadline)
                .expect("reply Connected");
            read_frame(&mut reader, deadline).expect("TUI Subscribe");
            write_bytes(&mut stream, format!("{list}\n").as_bytes(), deadline)
                .expect("reply SessionList");
            let writes = std::sync::Arc::new(std::sync::Mutex::new(()));
            let panes_requests = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let _ = subscriber_tx.send(PreviewDaemonPeer {
                stream: stream.try_clone().unwrap(),
                writes: writes.clone(),
                panes_requests: panes_requests.clone(),
            });
            loop {
                let deadline = Instant::now() + HANG_GUARD;
                let Ok(frame) = read_frame(&mut reader, deadline) else {
                    return;
                };
                let message: ClientMessage = serde_json::from_str(&frame).unwrap();
                if let atm_protocol::MessageType::Request {
                    id,
                    method,
                    params,
                    from_pane,
                } = message.message
                {
                    if method == "session.panes" {
                        assert_eq!(method, "session.panes");
                        let session_name = params["session_name"].as_str().unwrap();
                        let rows = tmux_stdout(
                            &label,
                            &["list-panes", "-a", "-F", "#{pane_id}\t#{session_name}"],
                        );
                        let mut ids: Vec<_> = rows
                            .lines()
                            .filter_map(|row| row.split_once('\t'))
                            .filter(|(_, name)| *name == session_name)
                            .map(|(id, _)| id.to_owned())
                            .collect();
                        ids.sort();
                        ids.dedup();
                        let response = DaemonMessage::response_ok(id, json!({"pane_ids":ids}));
                        let _write = writes.lock().unwrap();
                        write_bytes(
                            &mut stream,
                            format!("{}\n", serde_json::to_string(&response).unwrap()).as_bytes(),
                            deadline,
                        )
                        .unwrap();
                        panes_requests.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    } else {
                        assert_eq!(method, "session.capture");
                        assert_eq!(params["tail"], Value::Null);
                        assert_eq!(params["prompt"], false);
                        assert!(from_pane.is_some());
                        let target = params["target"].as_str().unwrap();
                        captures.lock().unwrap().push(params.clone());
                        let output = command_output(
                            Command::new(&capture_tmux)
                                .arg("-u")
                                .env_remove("TMUX")
                                .env_remove("TMUX_PANE")
                                .args(["-L", &label, "capture-pane", "-t", target, "-p"]),
                        );
                        let reply = if output.status.success() {
                            let text = String::from_utf8(output.stdout).unwrap();
                            let mut lines: Vec<_> = text.lines().map(str::to_owned).collect();
                            while lines.last().is_some_and(|line| line.trim().is_empty()) {
                                lines.pop();
                            }
                            DaemonMessage::response_ok(id, json!({"lines": lines}))
                        } else {
                            DaemonMessage::response_err(
                                id,
                                "internal",
                                &String::from_utf8_lossy(&output.stderr),
                            )
                        };
                        let line = format!("{}\n", serde_json::to_string(&reply).unwrap());
                        let _write = writes.lock().unwrap();
                        if write_bytes(&mut stream, line.as_bytes(), Instant::now() + HANG_GUARD)
                            .is_err()
                        {
                            return;
                        }
                    }
                }
            }
        }
    });
    subscriber_rx
}

fn preview_session(id: &str, started_at: &str, pane: Option<&str>) -> SessionView {
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
fn preview_session_a(pane: Option<&str>) -> SessionView {
    preview_session("session-a", "2026-01-01T00:00:02Z", pane)
}

fn preview_session_b(pane: Option<&str>) -> SessionView {
    preview_session("session-b", "2026-01-01T00:00:01Z", pane)
}

fn preview_session_c(pane: Option<&str>) -> SessionView {
    preview_session("session-c", "2026-01-01T00:00:00Z", pane)
}

/// The real TUI in a pane, and the stream that feeds it daemon events, once
/// it has subscribed.
struct PreviewRig {
    socket_label: String,
    tui_pane: String,
    daemon: Option<PreviewDaemonPeer>,
    capture_hold: PathBuf,
    captures: std::sync::Arc<std::sync::Mutex<Vec<Value>>>,
}

/// The stand-in daemon uses this wrapper for captures; the TUI uses it only
/// for unmigrated effects. The test's screen reads still reach real tmux.
/// A lock stalls the daemon's capture response, never the render loop.
fn preview_tmux_wrapper(dir: &Path, label: &str) -> PathBuf {
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let real = shell_word(&tmux_on_path().unwrap().display().to_string());
    let hold = shell_word(&dir.join("hold-capture").display().to_string());
    let trace = shell_word(&dir.join("tmux-calls").display().to_string());
    let label = shell_word(label);
    role_support::executable(
        &bin.join("tmux"),
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {trace}\ncapture=0\nfor arg do [ \"$arg\" = capture-pane ] && capture=1; done\n\
         if [ \"$capture\" = 1 ] && [ -e {hold} ]; then\n\
         {real} -L {label} wait-for -S preview-capture-blocked\n\
         {real} -L {label} wait-for -L preview-capture-lock\n\
         {real} -L {label} wait-for -U preview-capture-lock\nfi\nexec {real} \"$@\"\n"
        ),
    );
    bin
}

fn shell_word(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

fn quiet_tmux(label: &str, args: &[&str]) {
    match tmux_command()
        .args(["-L", label])
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            let _ = wait_child(&mut child, Instant::now() + HANG_GUARD);
        }
        Err(error) => eprintln!("cleanup tmux -L {label} {args:?}: {error}"),
    }
}

struct CaptureHold {
    label: String,
    flag: PathBuf,
}

impl Drop for CaptureHold {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.flag);
        quiet_tmux(&self.label, &["wait-for", "-U", "preview-capture-lock"]);
    }
}

impl Drop for PreviewRig {
    /// The TUI answers a closed daemon stream or a missing socket by starting
    /// a real atmd, which would outlive the test, so it has to die before
    /// `daemon` closes and the socket's temp dir goes. Its death closes the far
    /// end, so EOF here means it's gone, zombie or not. A TUI that never
    /// subscribed gets only the pane's hangup.
    fn drop(&mut self) {
        let tmux = |args: &[&str]| quiet_tmux(&self.socket_label, args);
        tmux(&["kill-pane", "-t", self.tui_pane.as_str()]);
        if let Some(daemon) = &mut self.daemon {
            let deadline = Instant::now() + HANG_GUARD;
            let mut buf = [0u8; 1024];
            loop {
                let Some(remaining) = deadline
                    .checked_duration_since(Instant::now())
                    .filter(|left| !left.is_zero())
                else {
                    eprintln!("the TUI outlived its pane's hang guard");
                    break;
                };
                let _ = daemon.stream.set_read_timeout(Some(remaining));
                match daemon.stream.read(&mut buf) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => {
                        eprintln!("the TUI outlived its pane: {e}");
                        break;
                    }
                }
            }
        }
        tmux(&["kill-session", "-t", "=preview-agents:"]);
        tmux(&["kill-session", "-t", "=preview-hold:"]);
    }
}

impl PreviewRig {
    fn hold_captures(&self) -> CaptureHold {
        let calls = self.capture_hold.with_file_name("tmux-calls");
        assert!(
            calls.exists(),
            "TUI did not use its private tmux wrapper; environment: {}",
            tmux_stdout(
                &self.socket_label,
                &["show-environment", "-t", "preview-tui", "PATH"]
            )
        );
        let deadline = Instant::now() + HANG_GUARD;
        tmux_stdout(
            &self.socket_label,
            &["wait-for", "-L", "preview-capture-lock"],
        );
        let hold = CaptureHold {
            label: self.socket_label.clone(),
            flag: self.capture_hold.clone(),
        };
        std::fs::write(&hold.flag, "hold").unwrap();
        wait_for_signal(&self.socket_label, "preview-capture-blocked", deadline).unwrap_or_else(
            |error| {
                panic!(
                    "the TUI's next capture entered the test barrier: {error}; calls: {}",
                    std::fs::read_to_string(calls).unwrap_or_default()
                )
            },
        );
        hold
    }

    fn screen(&self) -> String {
        tmux_stdout(
            &self.socket_label,
            &["capture-pane", "-p", "-t", self.tui_pane.as_str()],
        )
    }

    fn press(&self, key: &str) {
        tmux_stdout(
            &self.socket_label,
            &["send-keys", "-t", self.tui_pane.as_str(), key],
        );
    }

    fn send(&mut self, message: &DaemonMessage) {
        let line = serde_json::to_string(message).expect("serialize");
        let daemon = self.daemon.as_mut().expect("the TUI subscribed");
        let _write = daemon.writes.lock().unwrap();
        write_bytes(
            &mut daemon.stream,
            format!("{line}\n").as_bytes(),
            Instant::now() + HANG_GUARD,
        )
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
                start.elapsed() < PREVIEW_STEP_TIMEOUT,
                "{step}: timed out, screen:\n{screen}"
            );
            // The TUI has no semantic render acknowledgment; a daemon write
            // or tmux output event cannot prove this screen predicate is true.
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Waits until the preview shows `marker` and no other pane's marker.
    fn wait_for_preview(&self, step: &str, marker: &str) {
        self.wait_until(step, |screen| {
            PREVIEW_MARKERS
                .iter()
                .all(|m| screen.contains(m) == (*m == marker))
        });
    }

    /// Waits until the preview is gone, with no pane's marker left on screen.
    fn wait_for_no_preview(&self, step: &str) {
        self.wait_until(step, |screen| {
            PREVIEW_MARKERS.iter().all(|m| !screen.contains(m)) && !screen.contains(" Terminal ")
        });
    }
}

/// One pane per marker, in a session of their own, in `PREVIEW_MARKERS` order.
fn start_preview_panes(label: &str) -> Vec<String> {
    let mut panes = Vec::new();
    for (i, marker) in PREVIEW_MARKERS.iter().enumerate() {
        let print_and_stay = format!("echo {marker}; exec cat >/dev/null");
        let mut args: Vec<&str> = if i == 0 {
            vec!["new-session", "-d", "-s", "preview-agents"]
        } else {
            vec!["new-window", "-d", "-t", "=preview-agents:"]
        };
        args.extend(["-P", "-F", "#{pane_id}", "sh", "-c", &print_and_stay]);
        panes.push(tmux_stdout(label, &args));
    }
    panes
}

/// Starts the real TUI with `atm_args` against a stand-in daemon that lists
/// `sessions`, and waits until the list is on screen. `dir` holds the socket
/// and the TUI's state, so it has to outlive the returned rig.
fn start_preview_tui(
    env: &E2eEnv,
    dir: &Path,
    sessions: Vec<SessionView>,
    atm_args: &[&str],
) -> PreviewRig {
    let label = env.tmux_server.label();
    let socket = dir.join("atmd.sock");
    let wrapper = preview_tmux_wrapper(dir, label);
    let captures = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let subscriber = fake_tui_daemon(
        &socket,
        sessions,
        wrapper.join("tmux"),
        label.into(),
        captures.clone(),
    );
    let path_env = format!(
        "PATH={}:{}",
        wrapper.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    // A window as big as the session asks for, so the preview has room.
    let socket_env = format!("ATM_SOCKET={}", socket.display());
    let tmux_env = format!("ATM_TMUX_SOCKET={label}");
    let state_env = format!("XDG_STATE_HOME={}", dir.join("state").display());
    let atm_path = env.atm_path.display().to_string();
    let mut args = vec![
        "new-session",
        "-d",
        "-s",
        "preview-tui",
        "-x",
        "120",
        "-y",
        "40",
        "-e",
        &socket_env,
        "-e",
        &tmux_env,
        "-e",
        &state_env,
        "-P",
        "-F",
        "#{pane_id}",
        "env",
        // tmux overwrites PATH for a new pane with its client's PATH, even
        // with new-session -e PATH=... (verified 3.8). Set it at exec instead.
        &path_env,
        &atm_path,
    ];
    args.extend_from_slice(atm_args);
    let tui_pane = tmux_stdout(label, &args);
    let mut rig = PreviewRig {
        socket_label: label.to_string(),
        tui_pane,
        daemon: None,
        capture_hold: dir.join("hold-capture"),
        captures,
    };
    let daemon = subscriber
        .recv_timeout(PREVIEW_STEP_TIMEOUT)
        .unwrap_or_else(|e| panic!("the TUI didn't subscribe: {e}, screen:\n{}", rig.screen()));
    rig.daemon = Some(daemon);
    rig.wait_until("the list arrives", |screen| screen.contains("session-a"));
    rig
}

/// Scenario 6 — the capture preview follows the selected session's pane.
async fn scenario_tui_capture_preview(env: &E2eEnv) {
    let label = env.tmux_server.label();
    let panes = start_preview_panes(label);
    let [pane_a, pane_b, pane_c] = [&panes[0], &panes[1], &panes[2]].map(String::as_str);

    let dir = tempfile::tempdir().expect("tempdir");
    let mut rig = start_preview_tui(
        env,
        dir.path(),
        vec![
            preview_session_a(Some(pane_a)),
            preview_session_b(Some(pane_b)),
        ],
        &[],
    );

    // A key: selecting a row targets its pane, and moving on retargets.
    rig.press("j");
    rig.wait_for_preview("key to session-a", PREVIEW_MARKERS[0]);
    assert!(
        rig.captures
            .lock()
            .unwrap()
            .iter()
            .any(|params| params["target"] == pane_a),
        "preview must come from session.capture, not a client-side tmux read"
    );
    rig.press("j");
    rig.wait_for_preview("key to session-b", PREVIEW_MARKERS[1]);

    // Hold the replacement capture, then acknowledge processing of the update
    // through a changed detail value. A clear-on-update bug cannot be hidden by
    // a fast subsequent capture, nor by sampling the screen too late.
    let hold = rig.hold_captures();
    let mut updated = preview_session_b(Some(pane_b));
    updated.working_directory = Some("same-pane-ack".into());
    rig.send(&DaemonMessage::session_updated(updated));
    rig.wait_until("same-pane update applied", |screen| {
        if !screen.contains("same-pane-ack") {
            return false;
        }
        assert!(
            screen.contains(PREVIEW_MARKERS[1]),
            "same-pane update blanked the preview, screen:\n{screen}"
        );
        true
    });
    drop(hold);

    // A session update, with no key: the selected session moved pane.
    rig.send(&DaemonMessage::session_updated(preview_session_b(Some(
        pane_c,
    ))));
    rig.wait_for_preview("update to pane c", PREVIEW_MARKERS[2]);

    // A full list, with no key: the same again.
    rig.send(&DaemonMessage::session_list(vec![
        preview_session_a(Some(pane_a)),
        preview_session_b(Some(pane_b)),
    ]));
    rig.wait_for_preview("list back to pane b", PREVIEW_MARKERS[1]);

    // No pane to capture, so nothing would ever replace the old output: only
    // clearing it empties the preview.
    rig.send(&DaemonMessage::session_updated(preview_session_b(None)));
    rig.wait_until("update to no pane", |screen| {
        PREVIEW_MARKERS.iter().all(|m| !screen.contains(m)) && !screen.contains(" Terminal ")
    });

    // Removals, with no key. The cursor holds its row index, so `session-b`,
    // the second agent, is selected again after each full list.
    let all_three = || {
        DaemonMessage::session_list(vec![
            preview_session_a(Some(pane_a)),
            preview_session_b(Some(pane_b)),
            preview_session_c(Some(pane_c)),
        ])
    };
    rig.send(&all_three());
    rig.wait_for_preview("list of three", PREVIEW_MARKERS[1]);

    // The selected session goes, and the next one takes its row.
    rig.send(&DaemonMessage::session_removed(SessionId::new("session-b")));
    rig.wait_for_preview("remove selected session-b", PREVIEW_MARKERS[2]);

    // atmd's id upgrade: a removal, then an update for the same pane. The
    // preview may blank between them, but it has to end up back on that pane.
    rig.send(&all_three());
    rig.wait_for_preview("list of three again", PREVIEW_MARKERS[1]);
    rig.send(&DaemonMessage::session_removed(SessionId::new("session-b")));
    rig.send(&DaemonMessage::session_updated(preview_session(
        "session-b-real",
        "2026-01-01T00:00:01Z",
        Some(pane_b),
    )));
    rig.wait_until("id upgrade arrives", |screen| {
        screen.contains("ID: session-b-real")
    });
    rig.wait_for_preview("id upgrade of session-b", PREVIEW_MARKERS[1]);

    // A session above the selection goes. The cursor keeps its row index, so
    // it lands on another session. This records today's positional cursor, not
    // a goal: a cursor that followed its session would stay on `session-b`.
    rig.send(&all_three());
    rig.send(&DaemonMessage::session_removed(SessionId::new("session-a")));
    rig.wait_for_preview("remove session-a above the cursor", PREVIEW_MARKERS[2]);

    // The selected session was the last row, so the cursor clamps up.
    rig.send(&DaemonMessage::session_removed(SessionId::new("session-c")));
    rig.wait_for_preview("remove last-row session-c", PREVIEW_MARKERS[1]);

    // Nothing left to select. The panel draws no output then, stale or not, so
    // this covers only the cleared layout.
    rig.send(&DaemonMessage::session_removed(SessionId::new("session-b")));
    rig.wait_for_no_preview("remove the only session");

    // A sync with nothing selected still drops the old output. With its pane
    // gone, no capture would replace it if it showed again. The emptied tree
    // left the cursor on the header, hence the key.
    tmux_stdout(label, &["kill-pane", "-t", pane_b]);
    rig.send(&DaemonMessage::session_updated(preview_session_b(Some(
        pane_b,
    ))));
    rig.wait_until("session-b comes back", |screen| {
        screen.contains("session-b")
    });
    rig.press("j");
    rig.wait_until("the removed session's output stays gone", |screen| {
        screen.contains("ID: session-b") && PREVIEW_MARKERS.iter().all(|m| !screen.contains(m))
    });
}

/// Scenario 6, under `--tmux-session`: the preview follows the selection when
/// the filter hides the selected session. The TUI's own filter task sends
/// `FilterUpdate`, so a pane has to really leave the tmux session.
async fn scenario_tui_capture_preview_filter(env: &E2eEnv) {
    let label = env.tmux_server.label();
    let panes = start_preview_panes(label);
    let [pane_a, pane_b, pane_c] = [&panes[0], &panes[1], &panes[2]].map(String::as_str);
    tmux_stdout(label, &["new-session", "-d", "-s", "preview-hold", "cat"]);
    let hide = |pane: &str| {
        tmux_stdout(
            label,
            &["break-pane", "-d", "-s", pane, "-t", "=preview-hold:"],
        );
    };

    let dir = tempfile::tempdir().expect("tempdir");
    let rig = start_preview_tui(
        env,
        dir.path(),
        vec![
            preview_session_a(Some(pane_a)),
            preview_session_b(Some(pane_b)),
            preview_session_c(Some(pane_c)),
        ],
        &["--tmux-session", "preview-agents"],
    );
    rig.press("j");
    rig.wait_for_preview("filter: key to session-a", PREVIEW_MARKERS[0]);
    assert!(rig
        .captures
        .lock()
        .unwrap()
        .iter()
        .any(|params| params["target"] == pane_a));
    rig.press("j");
    rig.wait_for_preview("filter: key to session-b", PREVIEW_MARKERS[1]);

    // The selected session is hidden, and the next one takes its row.
    hide(pane_b);
    rig.wait_for_preview("filter hides selected session-b", PREVIEW_MARKERS[2]);

    // Every session is hidden, so there is nothing to select. The panel draws
    // no output then, so this covers only the cleared layout. Unlike a removal,
    // it can't check the output was dropped: a hidden session only comes back
    // with its pane alive, so a capture would replace the output anyway.
    hide(pane_a);
    hide(pane_c);
    rig.wait_for_no_preview("filter hides every session");
    assert!(
        rig.daemon
            .as_ref()
            .unwrap()
            .panes_requests
            .load(std::sync::atomic::Ordering::SeqCst)
            > 0,
        "filter bypassed daemon membership"
    );
}

/// Independent of E2eEnv, which would start a real daemon before the test.
#[test]
fn atm_role_pane_targets_need_no_daemon() {
    let Some(tmux_path) = tmux_on_path() else {
        eprintln!("SKIP: tmux not on PATH");
        return;
    };
    let label = PrivateTmux::fresh_label();
    let server = PrivateTmux::start_with_label_session_env(
        &label,
        "role",
        std::iter::empty::<(&str, &str)>(),
    );
    let tmux = |args: &[&str]| {
        role_support::run(
            Command::new(&tmux_path)
                .arg("-u")
                .args(["-L", server.label()])
                .args(args)
                .env_remove("TMUX")
                .env_remove("TMUX_PANE"),
        )
    };
    let panes = tmux(&["list-panes", "-F", "#{pane_id}"]);
    assert!(panes.status.success(), "{panes:?}");
    let pane = std::str::from_utf8(&panes.stdout).unwrap().trim();
    let split = tmux(&["split-window", "-d", "-P", "-F", "#{pane_id}", "-t", pane]);
    assert!(split.status.success(), "{split:?}");
    let other = std::str::from_utf8(&split.stdout).unwrap().trim();

    for listening in [false, true] {
        let dir = role_support::rig();
        std::os::unix::fs::symlink(&tmux_path, dir.path().join("bin/tmux")).unwrap();
        let listener = listening.then(|| role_support::connection_trap(dir.path()));
        let role = |args: &[&str], caller: Option<&str>| {
            let mut cmd = role_support::command(dir.path(), args);
            cmd.env("ATM_TMUX_SOCKET", server.label())
                .env_remove("TMUX_TMPDIR");
            if let Some(tmp) = std::env::var_os("TMUX_TMPDIR") {
                cmd.env("TMUX_TMPDIR", tmp);
            }
            if let Some(caller) = caller {
                cmd.env("TMUX_PANE", caller);
            }
            let out = role_support::run(&mut cmd);
            role_support::assert_no_daemon(dir.path(), listener.as_ref());
            out
        };
        for (args, caller, tagged) in [
            (vec!["role", "coordinator", "-t", pane], Some(other), true),
            (vec!["role", "--clear", "-t", pane], None, false),
            (vec!["role", "--clear", "-t", pane], None, false),
            (vec!["role", "coordinator"], Some(pane), true),
            (vec!["role", "--clear"], Some(pane), false),
        ] {
            let out = role(&args, caller);
            assert!(out.status.success(), "{args:?}: {out:?}");
            assert!(out.stdout.is_empty(), "{out:?}");
            let option = tmux(&["show-options", "-p", "-v", "-t", pane, "@atm-role"]);
            if tagged {
                assert!(option.status.success(), "{option:?}");
                assert_eq!(option.stdout, b"coordinator\n");
            } else {
                assert!(
                    !option.status.success(),
                    "pane option was not removed: {option:?}"
                );
            }
            assert!(
                !tmux(&["show-options", "-p", "-v", "-t", other, "@atm-role"])
                    .status
                    .success(),
                "explicit target tagged the caller's other pane"
            );
        }
        let out = role(&["role", "coordinator", "-t", "%999999"], None);
        assert_eq!(out.status.code(), Some(1), "{out:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("%999999"),
            "{out:?}"
        );
        assert!(out.stdout.is_empty());
        for caller in [None, Some("")] {
            let out = role(&["role", "coordinator"], caller);
            assert_eq!(out.status.code(), Some(1), "{out:?}");
            assert!(
                String::from_utf8_lossy(&out.stderr).contains("pass -t"),
                "{out:?}"
            );
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn atm_atmd_tmux_end_to_end() {
    let Some(env) = E2eEnv::try_setup().await else {
        return;
    };

    // Scenarios run sequentially against the shared fixture. Each
    // scenario lives in its own async fn so a panic surfaces with that
    // function's local frame instead of getting buried in a 500-LOC
    // monolithic test body.
    let session_id = scenario_protocol_e2e(&env).await;
    scenario_atm_list_client(&env, &session_id).await;
    scenario_control_server_identity_and_start_directory(&env).await;
    scenario_role_json(&env, &session_id).await;
    scenario_real_tmux_lib(&env).await;
    scenario_paste_text_and_atm_send(&env).await;

    if python3_on_path().is_some() {
        scenario_atm_spawn(&env).await;
    } else {
        eprintln!("SKIP scenario 4: python3 not on PATH (scenario 5 still runs)");
    }

    scenario_workspace_create(&env).await;
    scenario_workspace_attach(&env).await;
    scenario_workspace_attach_autopick_rejects_unsupported_name(&env).await;
    scenario_workspace_create_launches_configured_harness(&env).await;
    scenario_workspace_create_rejects_unknown_harness(&env).await;
    scenario_workspace_create_sends_args_ending_in_semicolon(&env).await;
    scenario_workspace_create_sends_default_keys(&env).await;
    scenario_tui_capture_preview(&env).await;
    scenario_tui_capture_preview_filter(&env).await;
}

#[tokio::test(flavor = "current_thread")]
async fn workspace_resize_uses_triggering_window() {
    let Some(env) = E2eEnv::try_setup().await else {
        return;
    };
    scenario_workspace_resize_targets_window(&env).await;
}

#[tokio::test(flavor = "current_thread")]
async fn workspace_scripts_do_not_cross_tmux_sockets() {
    let Some(env) = E2eEnv::try_setup().await else {
        return;
    };
    for reverse in [false, true] {
        let data = awkward_data_dir();
        let session = format!("{}-{}", private_session_name("shared-"), u8::from(reverse));
        let labels = [PrivateTmux::fresh_label(), format!("atm-{session}")];
        let servers: Vec<_> = labels
            .iter()
            .map(|label| {
                PrivateTmux::start_with_label_session_env(
                    label,
                    &session,
                    [("ATM_SOCKET", env.daemon.socket_path.as_os_str())],
                )
            })
            .collect();
        let install = |index: usize| {
            let mut command = Command::new(&env.atm_path);
            command.args(["workspace", "attach", &session]);
            if index == 1 {
                command.arg("--isolate");
            }
            let output = command_output(
                command
                    .env("ATM_TMUX_SOCKET", &labels[index])
                    .env("ATM_NO_ATTACH", "1")
                    .env("ATM_SOCKET", &env.daemon.socket_path)
                    .env("XDG_DATA_HOME", data.path())
                    .env("XDG_STATE_HOME", &env.daemon.state_dir),
            );
            assert!(output.status.success(), "attach socket {index}: {output:?}");
        };
        let state = |index: usize| {
            tmux_stdout(&labels[index], &["list-panes", "-a", "-F",
            "#{window_id}:#{pane_id}:#{@atm-sidebar}:#{pane_left}:#{pane_top}:#{pane_width}:#{pane_height}:#{pane_active}"])
        };
        let new_window = |index: usize| {
            tmux_stdout(
                &labels[index],
                &[
                    "new-window",
                    "-d",
                    "-t",
                    &format!("={session}:"),
                    "-P",
                    "-F",
                    "#{window_id}",
                ],
            )
            .trim()
            .to_string()
        };
        let sidebar_count = |index: usize, window: &str| {
            tmux_stdout(
                &labels[index],
                &["list-panes", "-t", window, "-F", "#{@atm-sidebar}"],
            )
            .lines()
            .filter(|line| *line == "1")
            .count()
        };
        for label in &labels {
            tmux_stdout(
                label,
                &[
                    "resize-window",
                    "-t",
                    &format!("={session}:"),
                    "-x",
                    "180",
                    "-y",
                    "85",
                ],
            );
        }
        let [first, second] = if reverse { [1, 0] } else { [0, 1] };
        install(first);
        let filenames = [
            format!("inject-sidebar-{session}.sh"),
            format!("resize-sidebar-{session}.sh"),
        ];
        let original_scripts: Vec<_> = filenames
            .iter()
            .map(|name| {
                let paths = workspace_script_paths(data.path(), name);
                assert_eq!(paths.len(), 1);
                let path = paths.into_iter().next().expect("installed script");
                let content = std::fs::read_to_string(&path).expect("script contents");
                (path, content)
            })
            .collect();
        install(second);

        // Both servers deliberately have the same numeric window IDs. A
        // window argument alone cannot save a script overwritten for another socket.
        let second_window = new_window(second);
        let second_before = state(second);
        let first_window = new_window(first);
        assert_eq!(
            first_window, second_window,
            "matching server-local window IDs"
        );
        assert_eq!(
            sidebar_count(first, &first_window),
            1,
            "new sidebar must stay on its own socket"
        );
        assert_eq!(
            state(second),
            second_before,
            "new-window must not add a sidebar to the other socket"
        );
        let first_next = new_window(first);
        let first_before = state(first);
        let second_next = new_window(second);
        assert_eq!(first_next, second_next);
        assert_eq!(sidebar_count(second, &second_next), 1);
        assert_eq!(
            state(first),
            first_before,
            "reverse new-window must not touch the first socket"
        );

        for (path, content) in &original_scripts {
            assert_eq!(
                std::fs::read_to_string(path).expect("original retained script"),
                *content,
                "second install must not overwrite the first script"
            );
        }
        for filename in &filenames {
            assert_eq!(
                workspace_script_paths(data.path(), filename).len(),
                2,
                "same-name scripts need separate socket namespaces"
            );
        }
        let injection_paths = workspace_script_paths(data.path(), &filenames[0]);
        let first_path = &original_scripts[0].0;
        let second_path = injection_paths
            .iter()
            .find(|path| *path != first_path)
            .expect("second socket's script");
        for (index, path) in [(first, first_path), (second, second_path)] {
            let namespace = path
                .parent()
                .expect("script namespace")
                .file_name()
                .expect("namespace name")
                .to_str()
                .expect("namespace UTF-8");
            assert!(tmux_stdout(
                &labels[index],
                &["show-hooks", "-t", &format!("={session}:")]
            )
            .contains(&format!("/{namespace}/")));
        }
        for (index, label) in labels.iter().enumerate() {
            let sidebar = tmux_stdout(
                label,
                &[
                    "list-panes",
                    "-t",
                    &format!("={session}:@0"),
                    "-F",
                    "#{@atm-sidebar} #{pane_id}",
                ],
            )
            .lines()
            .find_map(|line| line.strip_prefix("1 ").map(str::to_string))
            .expect("original sidebar");
            let other_before = state(1 - index);
            tmux_stdout(label, &["resize-window", "-t", &sidebar, "-x", "240"]);
            assert_eq!(
                tmux_stdout(
                    label,
                    &["display-message", "-p", "-t", &sidebar, "#{pane_width}"]
                )
                .trim(),
                "38",
                "resize hook must use its own socket's script"
            );
            assert_eq!(
                state(1 - index),
                other_before,
                "resize must not touch the other socket"
            );
            tmux_stdout(
                label,
                &[
                    "set-option",
                    "-p",
                    "-t",
                    &sidebar,
                    "@atm-sidebar-tries",
                    "5",
                ],
            );
            tmux_stdout(label, &["resize-pane", "-t", &sidebar, "-x", "50"]);
            install(index);
            assert_eq!(
                tmux_stdout(
                    label,
                    &["display-message", "-p", "-t", &sidebar, "#{pane_width}"]
                )
                .trim(),
                "38",
                "reattach must find its own resize script"
            );
            assert_eq!(
                state(1 - index),
                other_before,
                "reattach must not touch the other socket"
            );
        }
        for filename in &filenames {
            assert_eq!(
                workspace_script_paths(data.path(), filename).len(),
                2,
                "reattach must reuse stable script paths"
            );
        }
        drop(servers);
    }
}

#[derive(Debug)]
struct PaneEntry {
    window_id: String,
    pane_id: String,
    is_sidebar: bool,
}

/// Lists every pane on the given tmux server with its window id and
/// `@atm-sidebar` option value. Avoids `RealTmuxClient::list_panes`
/// because we need the option, not just the standard fields.
fn list_panes_with_sidebar_marker(socket_label: &str) -> Option<Vec<PaneEntry>> {
    let output = command_output(tmux_command().args([
        "-L",
        socket_label,
        "list-panes",
        "-a",
        "-F",
        "#{window_id}|#{pane_id}|#{@atm-sidebar}",
    ]));
    if !output.status.success() {
        return None;
    }
    let mut entries = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let parts: Vec<&str> = line.split('|').collect();
        if let (Some(window_id), Some(pane_id)) = (parts.first(), parts.get(1)) {
            let is_sidebar = parts.get(2).is_some_and(|s| *s == "1");
            entries.push(PaneEntry {
                window_id: window_id.to_string(),
                pane_id: pane_id.to_string(),
                is_sidebar,
            });
        }
    }
    Some(entries)
}

/// An `XDG_DATA_HOME` for the workspace scripts whose path each layer the
/// hooks pass it through would break unquoted: tmux's command parser, then
/// run-shell's format expansion (`#S`), then `sh -c`. macOS's data dir
/// (`~/Library/Application Support`) has a space.
fn awkward_data_dir() -> TempDir {
    tempfile::Builder::new()
        .prefix("atm data 'q\" $HOME #S ")
        .tempdir()
        .expect("create workspace data tempdir")
}

fn single_workspace_script(data: &Path, filename: &str) -> PathBuf {
    let mut paths = workspace_script_paths(data, filename);
    assert_eq!(paths.len(), 1, "one installed {filename}");
    paths.pop().expect("installed workspace script")
}

/// Find generated artifacts without duplicating the production path encoder.
/// Accept the old flat layout too, so the routing regression can run RED.
fn workspace_script_paths(data: &Path, filename: &str) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(data.join("atm")).expect("workspace script base") {
        let entry = entry.expect("script entry");
        let path = if entry.file_type().expect("script entry type").is_dir() {
            entry.path().join(filename)
        } else {
            entry.path()
        };
        if path.file_name() == Some(std::ffi::OsStr::new(filename)) && path.is_file() {
            paths.push(path);
        }
    }
    paths.sort();
    paths
}

struct ControlClient {
    child: Child,
    reader: BufReader<UnixStream>,
    deadline: Instant,
}

impl ControlClient {
    fn attach(socket_label: &str, session: &str) -> Self {
        let deadline = Instant::now() + HANG_GUARD;
        let (reader, writer) = UnixStream::pair().expect("control output socketpair");
        let child = tmux_command()
            .args([
                "-L",
                socket_label,
                "-C",
                "attach-session",
                "-t",
                &format!("={session}:"),
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::from(OwnedFd::from(writer)))
            .stderr(Stdio::null())
            .spawn()
            .expect("attach a control client");
        let mut client = Self {
            child,
            reader: BufReader::new(reader),
            deadline,
        };
        client
            .until(|line| line.starts_with("%session-changed "))
            .expect("control client attached before guard");
        client
    }

    fn until(&mut self, done: impl Fn(&str) -> bool) -> io::Result<String> {
        loop {
            let line = read_frame(&mut self.reader, self.deadline)?;
            assert!(
                !line.starts_with("%error ") && !line.starts_with("%exit"),
                "{line}"
            );
            if done(&line) {
                return Ok(line);
            }
        }
    }
}

impl Drop for ControlClient {
    fn drop(&mut self) {
        self.child.stdin.take();
        if std::thread::panicking() {
            let _ = self.child.kill();
        }
        let _ = wait_child(&mut self.child, Instant::now() + HANG_GUARD);
    }
}

/// Presses prefix-`key` as if typed, through a control-mode client attached
/// to `session`, and keeps the client until `done` or a deadline: the
/// binding runs in that client's queue. Running the binding's `list-keys`
/// line instead would re-parse tmux's printed form, and tmux 3.4 prints a
/// `$` in it as `\\$`.
async fn press_prefix_key(socket_label: &str, session: &str, key: &str, done: impl Fn() -> bool) {
    let mut client = ControlClient::attach(socket_label, session);
    // Query through the attached client, not list-clients polling from outside.
    client
        .child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"list-clients -F \"#{client_control_mode} #{client_name}\"\n")
        .unwrap();
    let name = client
        .until(|line| line.starts_with("1 "))
        .expect("control client name before guard");
    let name = name.trim().strip_prefix("1 ").unwrap();
    client
        .until(|line| line.starts_with("%end "))
        .expect("control client list completed before guard");
    // The server may have loaded the developer's ~/.tmux.conf.
    let prefix = tmux_run_capture(socket_label, &["show-options", "-gv", "prefix"])
        .expect("read the prefix key");
    tmux_run_capture(
        socket_label,
        &["send-keys", "-K", "-c", name, prefix.trim(), key],
    )
    .expect("press the prefix key");
    // Layout events are retained in the socket even if the key completed
    // before we started reading. Recheck the semantic condition on each event.
    while !done() {
        client
            .until(|line| line.starts_with("%layout-change "))
            .unwrap_or_else(|error| {
                panic!(
                    "prefix-{key} layout guard: {error}; actual pane widths: {:?}",
                    tmux_run_capture(
                        socket_label,
                        &[
                            "list-panes",
                            "-t",
                            &format!("={session}:"),
                            "-F",
                            "#{pane_id}=#{pane_width}"
                        ]
                    )
                );
            });
    }
}

/// Whether the `tmux` on PATH is at least `major.minor`. A version tmux
/// prints without numbers (a `master` build) counts as new.
fn tmux_version_at_least(major: u32, minor: u32) -> bool {
    let label = PrivateTmux::fresh_label();
    let output = command_output(tmux_command().args(["-L", &label, "-V"]));
    let version = String::from_utf8_lossy(&output.stdout);
    let numbers: Vec<u32> = version
        .split_whitespace()
        .last()
        .unwrap_or_default()
        .trim_start_matches("next-")
        .split('.')
        .map(|part| {
            part.chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
        })
        .map_while(|digits| digits.parse().ok())
        .collect();
    match numbers.as_slice() {
        [found_major, found_minor, ..] => (*found_major, *found_minor) >= (major, minor),
        _ => true,
    }
}

/// Tiny `tmux -L <label> ...` helper for scenario 5's hook-inspection
/// queries. Returns stdout on success.
fn tmux_run_capture(socket_label: &str, args: &[&str]) -> Option<String> {
    let mut cmd = tmux_command();
    cmd.args(["-L", socket_label]);
    cmd.args(args);
    let output = try_command_output(&mut cmd)
        .map_err(|error| {
            eprintln!("{cmd:?}: {error}");
        })
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).to_string())
}
