//! End-to-end harness exercising real binaries against a real tmux server.
//!
//! Five scenarios are covered:
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
//!
//! 4. **`atm spawn` with a `claude` fixture**: run the actual `atm spawn`
//!    subcommand against our isolated tmux server (via `ATM_TMUX_SOCKET`),
//!    using a Python shim injected by absolute path through
//!    `ATM_SPAWN_BIN`. (Earlier iterations tried prepending the
//!    shim dir to `PATH`, but interactive shell init re-derives `PATH`
//!    from scratch and dropped the prepended dir. Absolute-path
//!    injection sidesteps that entirely.) The shim connects to atmd
//!    over its socket, sends a `UserPromptSubmit` hook event carrying
//!    `$TMUX_PANE`, and sleeps to keep the pane alive (atmd's
//!    stale-session reaper runs every 2s). The test then polls
//!    `atm list --format json` until a session whose `tmux_pane` matches
//!    the pane atm spawn created shows up. This validates the entire
//!    spawn → tmux split → hook event → registry update loop.
//!
//! 5. **`atm workspace create` / `atm workspace attach`**: verifies both
//!    workspace flows install the resize and new-window hooks. The create
//!    scenario also opens a new tmux window and asserts the hook injects a
//!    sidebar there; the attach scenario starts from a multi-window session
//!    and asserts every window gets exactly one sidebar, including on a
//!    second idempotency run.
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

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use atm_protocol::{ClientMessage, DaemonMessage, ProtocolVersion};
use atm_tmux::{PaneDirection, RealTmuxClient, TmuxClient};
use serde_json::{json, Value};
use tempfile::TempDir;

const SOCKET_WAIT_TIMEOUT: Duration = Duration::from_secs(5);
const SOCKET_POLL_INTERVAL: Duration = Duration::from_millis(50);
const SESSION_APPEAR_TIMEOUT: Duration = Duration::from_secs(2);
const SESSION_POLL_INTERVAL: Duration = Duration::from_millis(50);
const DAEMON_SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// Returns `Some(())` if `tmux` is on PATH, `None` otherwise.
fn tmux_on_path() -> Option<()> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        if dir.join("tmux").is_file() {
            return Some(());
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
    let output = cmd
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn tmux for {label}: {e}"));
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
/// and `$TMUX_PANE`, then sleep. Any error is logged to stderr and the
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
import json, os, socket, sys, time, uuid

PROTOCOL_VERSION = {{"major": {major}, "minor": {minor}}}

socket_path = os.environ.get("ATM_SOCKET", "/tmp/atm.sock")
pane = os.environ.get("TMUX_PANE", "")
session_id = "fixture-" + uuid.uuid4().hex[:12]

try:
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.connect(socket_path)
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
    msg = recv()
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
            "prompt": "fixture probe",
        }},
    }})

    # Stay alive — atmd's stale-session reaper runs every 2s and would
    # remove this session if the registered PID exited.
    print("[claude-fixture] registered session " + session_id + " for pane " + pane, flush=True)
    time.sleep(60)
except Exception as e:
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

/// Owns a private tmux server scoped to this test (tmux `-L <socket>`).
///
/// We pass this label to `RealTmuxClient::with_socket(...)` so the project's
/// own tmux library code talks to *this* server, not the developer's default.
///
/// Env vars passed to `start_with_env(...)` propagate into the tmux server's
/// process — and from there into the shells running in panes the server
/// creates. Scenario 4 uses this channel to hand the `claude` shim the
/// `ATM_SOCKET` it needs to phone home to atmd. (The shim path itself is
/// injected via `ATM_SPAWN_BIN` at the `atm spawn` invocation, not
/// via tmux env, so PATH precedence isn't load-bearing here.)
struct PrivateTmux {
    socket_label: String,
}

impl PrivateTmux {
    fn start_with_env<I, K, V>(env: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<std::ffi::OsStr>,
        V: AsRef<std::ffi::OsStr>,
    {
        let now_nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let socket_label = format!("atm-e2e-{}-{}", std::process::id(), now_nanos);
        Self::start_with_label_session_env(&socket_label, "probe", env)
    }

    fn start_with_label_session_env<I, K, V>(socket_label: &str, session_name: &str, env: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<std::ffi::OsStr>,
        V: AsRef<std::ffi::OsStr>,
    {
        let mut cmd = Command::new("tmux");
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.args(["-L", socket_label, "new-session", "-d", "-s", session_name]);
        run_tmux_or_panic(&mut cmd, "new-session");

        Self {
            socket_label: socket_label.to_string(),
        }
    }

    fn label(&self) -> &str {
        &self.socket_label
    }
}

impl Drop for PrivateTmux {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .args(["-L", &self.socket_label, "kill-server"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// Owns a child `atmd` process and its sandbox dirs. Drop sends SIGTERM and
/// waits briefly so PID files / sockets are cleaned up.
struct Daemon {
    child: Option<Child>,
    socket_path: PathBuf,
    state_dir: PathBuf,
    /// Path to the file capturing daemon stdout+stderr. Surfaced on
    /// failure so a "socket didn't appear" panic includes actual evidence.
    log_path: PathBuf,
    _state_dir_guard: TempDir,
    _socket_dir_guard: TempDir,
}

impl Daemon {
    async fn spawn(binary: &Path) -> Self {
        let socket_dir_guard = tempfile::tempdir().expect("create socket tempdir");
        let state_dir_guard = tempfile::tempdir().expect("create state tempdir");
        let socket_path = socket_dir_guard.path().join("atmd.sock");
        let state_dir = state_dir_guard.path().to_path_buf();
        let log_path = state_dir_guard.path().join("atmd.log");

        let log_file = std::fs::File::create(&log_path).expect("create atmd log file");
        let log_clone = log_file.try_clone().expect("clone atmd log file");

        let child = Command::new(binary)
            .arg("start")
            .env("ATM_SOCKET", &socket_path)
            .env("XDG_STATE_HOME", &state_dir)
            .env("RUST_LOG", "warn")
            .stdout(log_file)
            .stderr(log_clone)
            .spawn()
            .expect("spawn atmd child");

        let daemon = Self {
            child: Some(child),
            socket_path,
            state_dir,
            log_path,
            _state_dir_guard: state_dir_guard,
            _socket_dir_guard: socket_dir_guard,
        };

        daemon.wait_for_socket().await;
        daemon
    }

    async fn wait_for_socket(&self) {
        let start = Instant::now();
        while start.elapsed() < SOCKET_WAIT_TIMEOUT {
            if self.socket_path.exists() {
                return;
            }
            tokio::time::sleep(SOCKET_POLL_INTERVAL).await;
        }
        let log = std::fs::read_to_string(&self.log_path).unwrap_or_default();
        panic!(
            "atmd socket {} did not appear within {:?}\n--- atmd log ---\n{}",
            self.socket_path.display(),
            SOCKET_WAIT_TIMEOUT,
            if log.is_empty() {
                "(empty)".to_string()
            } else {
                log
            }
        );
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            unsafe {
                libc::kill(child.id() as i32, libc::SIGTERM);
            }

            let deadline = Instant::now() + DAEMON_SHUTDOWN_GRACE;
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => return,
                    Ok(None) if Instant::now() >= deadline => break,
                    Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                    Err(_) => break,
                }
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Synchronous protocol client. Blocking std `UnixStream` keeps the harness
/// readable as a linear "send / recv / assert" script without dragging in a
/// runtime for the protocol-level scenario.
struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Client {
    fn connect(path: &Path) -> Self {
        let stream = UnixStream::connect(path).expect("connect to atmd socket");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set read timeout");
        let writer = stream.try_clone().expect("clone unix stream");
        Self {
            reader: BufReader::new(stream),
            writer,
        }
    }

    fn send(&mut self, msg: ClientMessage) {
        let mut json = serde_json::to_vec(&msg).expect("serialize ClientMessage");
        json.push(b'\n');
        self.writer.write_all(&json).expect("write client message");
        self.writer.flush().expect("flush client message");
    }

    fn recv(&mut self) -> DaemonMessage {
        let mut line = String::new();
        let bytes = self.reader.read_line(&mut line).expect("read daemon line");
        assert!(bytes > 0, "daemon closed connection unexpectedly");
        serde_json::from_str(&line).expect("deserialize DaemonMessage")
    }
}

/// Shared fixture for all 5 scenarios — daemon, primary tmux server,
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
        // shim reads it from there to find the daemon socket.
        let daemon = Daemon::spawn(&atmd_path).await;

        // Stage the `claude` shim once; scenario 4 will inject it via
        // ATM_SPAWN_BIN. Earlier iterations tried prepending the shim
        // dir to PATH, but interactive shell init re-derived PATH from
        // scratch and dropped it. Absolute-path injection sidesteps
        // that entirely.
        let shim_dir_guard = tempfile::tempdir().expect("create shim tempdir");
        let shim_path = write_claude_shim(shim_dir_guard.path());

        let tmux_server =
            PrivateTmux::start_with_env([("ATM_SOCKET", daemon.socket_path.as_os_str())]);
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

    let deadline = Instant::now() + SESSION_APPEAR_TIMEOUT;
    let found = loop {
        client.send(ClientMessage::list_sessions());
        let sessions = match client.recv() {
            DaemonMessage::SessionList { sessions } => sessions,
            other => panic!("expected SessionList, got {other:?}"),
        };

        if sessions.iter().any(|s| s.id.as_str() == session_id) {
            assert!(
                sessions.len() > baseline_count,
                "session count should grow by at least one (baseline={baseline_count}, now={})",
                sessions.len()
            );
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        tokio::time::sleep(SESSION_POLL_INTERVAL).await;
    };
    assert!(
        found,
        "session {session_id} never appeared in registry within {SESSION_APPEAR_TIMEOUT:?}"
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
    let output = Command::new(&env.atm_path)
        .args(["list", "--format", "json"])
        .env("ATM_SOCKET", &env.daemon.socket_path)
        .env("XDG_STATE_HOME", &env.daemon.state_dir)
        .output()
        .expect("run atm list");

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

/// Scenario 3 — drive the isolated tmux server through `atm-tmux`'s own
/// library. Unlike atmd's discovery code, `RealTmuxClient` supports `-L`,
/// so this stays fully isolated from the developer's default tmux.
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
        .split_window(&probe_pane, "30%", PaneDirection::Below, None)
        .await
        .expect("split-window");
    assert!(
        new_pane.starts_with('%'),
        "split-window should return a tmux pane id like '%N', got {new_pane:?}"
    );

    env.tmux
        .send_keys(&new_pane, "echo atm-e2e-marker")
        .await
        .expect("send-keys");
    env.tmux
        .send_keys(&new_pane, "Enter")
        .await
        .expect("send-keys Enter");

    // Wait for the echo to land in the pane's scrollback. tmux send-keys
    // is asynchronous from the shell's perspective, so this is a true
    // poll, not a sleep-based race.
    let capture_deadline = Instant::now() + Duration::from_secs(3);
    let mut saw_marker = false;
    while Instant::now() < capture_deadline {
        let lines = env.tmux.capture_pane(&new_pane).await.unwrap_or_default();
        if lines.iter().any(|l| l.contains("atm-e2e-marker")) {
            saw_marker = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        saw_marker,
        "expected 'atm-e2e-marker' to appear in pane {new_pane}'s capture within 3s"
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

/// Scenario 4 — `atm spawn` end-to-end. Splits a pane in our isolated
/// server, the pane runs our Python shim, the shim phones home to atmd,
/// we verify the registered session has the expected `tmux_pane`.
async fn scenario_atm_spawn(env: &E2eEnv) {
    let panes_pre_spawn = env.tmux.list_panes().await.expect("list panes (pre-spawn)");
    let probe_pane_pre_spawn = panes_pre_spawn
        .iter()
        .find(|p| p.session_name == "probe")
        .expect("probe pane present")
        .pane_id
        .clone();

    let spawn_output = Command::new(&env.atm_path)
        .args([
            "spawn",
            "--target-pane",
            &probe_pane_pre_spawn,
            "--direction",
            "below",
            "--size",
            "20%",
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
        .output()
        .expect("run atm spawn");

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

    // Poll `atm list --format json` until our fixture-minted session
    // shows up. We require BOTH the `id` prefix (rules out user-process
    // discoveries from /proc) AND `tmux_pane == spawned_pane` (defense
    // in depth — atmd starts fresh per test, but a future test that
    // spawns multiple shims could otherwise pick the wrong one).
    //
    // Generous deadline because the shim has to: (a) wait for tmux's
    // send-keys to land, (b) start the Python interpreter, (c) connect
    // to atmd, (d) hand off the hook event, (e) atmd has to apply it.
    let spawn_deadline = Instant::now() + Duration::from_secs(10);
    let mut fixture_session: Option<Value> = None;
    while Instant::now() < spawn_deadline {
        let list_output = Command::new(&env.atm_path)
            .args(["list", "--format", "json"])
            .env("ATM_SOCKET", &env.daemon.socket_path)
            .env("XDG_STATE_HOME", &env.daemon.state_dir)
            .output()
            .expect("run atm list");

        if list_output.status.success() {
            if let Ok(parsed) = serde_json::from_slice::<Value>(&list_output.stdout) {
                if let Some(arr) = parsed.as_array() {
                    if let Some(s) = arr.iter().find(|s| {
                        let id_match = s
                            .get("id")
                            .and_then(Value::as_str)
                            .is_some_and(|id| id.starts_with("fixture-"));
                        let pane_match = s.get("tmux_pane").and_then(Value::as_str)
                            == Some(spawned_pane.as_str());
                        id_match && pane_match
                    }) {
                        fixture_session = Some(s.clone());
                        break;
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }

    let session = match fixture_session {
        Some(s) => s,
        None => {
            // Snapshot what's in the spawned pane so the failure is
            // diagnosable instead of mysterious.
            let pane_dump = env
                .tmux
                .capture_pane(&spawned_pane)
                .await
                .unwrap_or_else(|e| vec![format!("(capture-pane failed: {e})")]);
            panic!(
                "no session with id starting with 'fixture-' appeared within 10s.\n\
                 atm spawn stdout: {}\n\
                 atm spawn stderr: {}\n\
                 spawned pane ({spawned_pane}) contents:\n{}",
                String::from_utf8_lossy(&spawn_output.stdout),
                String::from_utf8_lossy(&spawn_output.stderr),
                pane_dump.join("\n")
            );
        }
    };

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
}

/// Scenario 5a — `atm workspace create` should install the same long-lived
/// hooks as attach. In particular, after-new-window must inject a sidebar into
/// windows opened after creation.
async fn scenario_workspace_create(env: &E2eEnv) {
    let create_session_name = format!("jghcrt{}", std::process::id());
    let create_socket_label = format!("atm-{create_session_name}");
    let create_data_dir = tempfile::tempdir().expect("create workspace data tempdir");

    let create_output = Command::new(&env.atm_path)
        .args([
            "workspace",
            "create",
            "--name",
            &create_session_name,
            "--isolate",
        ])
        .env("ATM_NO_ATTACH", "1")
        .env("XDG_DATA_HOME", create_data_dir.path())
        .env("XDG_STATE_HOME", &env.daemon.state_dir)
        .env("ATM_SOCKET", &env.daemon.socket_path)
        .output()
        .expect("run atm workspace create");
    assert!(
        create_output.status.success(),
        "atm workspace create failed: stdout={} stderr={}",
        String::from_utf8_lossy(&create_output.stdout),
        String::from_utf8_lossy(&create_output.stderr)
    );

    let create_tmux = PrivateTmux {
        socket_label: create_socket_label,
    };

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
    tmux_run_capture(
        create_tmux.label(),
        &["resize-pane", "-t", &sidebar_pane, "-x", &skewed_width],
    )
    .expect("skew sidebar width");
    let resize_script = create_data_dir
        .path()
        .join("atm")
        .join(format!("resize-sidebar-{create_session_name}.sh"));
    tmux_run_capture(
        create_tmux.label(),
        &["run-shell", &resize_script.display().to_string()],
    )
    .expect("run resize script");
    let resized_width = tmux_run_capture(
        create_tmux.label(),
        &[
            "display-message",
            "-p",
            "-t",
            &sidebar_pane,
            "#{pane_width}",
        ],
    )
    .expect("read sidebar width");
    assert_eq!(
        resized_width.trim(),
        expected_width.to_string(),
        "resize script should restore the sidebar width even after its pane title changed"
    );

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
    let deadline = Instant::now() + SESSION_APPEAR_TIMEOUT;
    loop {
        let width = tmux_run_capture(
            create_tmux.label(),
            &[
                "display-message",
                "-p",
                "-t",
                &sidebar_pane,
                "#{pane_width}",
            ],
        )
        .expect("read sidebar width after teammate spawn");
        if width.trim() == expected_width.to_string() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "after-resize-pane hook should restore sidebar width {expected_width} after Claude's rebalance; got {}",
            width.trim()
        );
        tokio::time::sleep(SESSION_POLL_INTERVAL).await;
    }

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
    let deadline = Instant::now() + SESSION_APPEAR_TIMEOUT;
    while read_sidebar("#{pane_width}") != expected_width.to_string() {
        assert!(
            Instant::now() < deadline,
            "after-resize-pane hook should restore a below-target sidebar to {expected_width}; got {}",
            read_sidebar("#{pane_width}")
        );
        tokio::time::sleep(SESSION_POLL_INTERVAL).await;
    }

    // A window too narrow for the target width: the hook's own resize-pane
    // re-fires it, so it must give up after its retry cap instead of looping.
    tmux_run_capture(
        create_tmux.label(),
        &["resize-window", "-t", &create_session_name, "-x", "15"],
    )
    .expect("shrink window below sidebar minimum");
    let deadline = Instant::now() + SESSION_APPEAR_TIMEOUT;
    while read_sidebar("#{@atm-sidebar-tries}")
        .parse::<u32>()
        .unwrap_or(0)
        < 5
    {
        assert!(
            Instant::now() < deadline,
            "hook should reach its retry cap in a too-small window; tries={}",
            read_sidebar("#{@atm-sidebar-tries}")
        );
        tokio::time::sleep(SESSION_POLL_INTERVAL).await;
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
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
    let deadline = Instant::now() + SESSION_APPEAR_TIMEOUT;
    while read_sidebar("#{pane_width}:#{@atm-sidebar-tries}") != format!("{expected_width}:") {
        assert!(
            Instant::now() < deadline,
            "sidebar should recover (width:tries) after the window grows; got {}",
            read_sidebar("#{pane_width}:#{@atm-sidebar-tries}")
        );
        tokio::time::sleep(SESSION_POLL_INTERVAL).await;
    }

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

    let mut cmd = Command::new("tmux");
    cmd.args([
        "-L",
        create_tmux.label(),
        "new-window",
        "-t",
        &create_session_name,
    ]);
    run_tmux_or_panic(&mut cmd, "new-window after workspace create");

    let deadline = Instant::now() + SESSION_APPEAR_TIMEOUT;
    loop {
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

        if windows_with_sidebar.len() == 2 && windows_with_sidebar.values().all(|count| *count == 1)
        {
            break;
        }

        assert!(
            Instant::now() < deadline,
            "new window should receive exactly one sidebar via after-new-window hook; panes: {panes:?}"
        );
        tokio::time::sleep(SESSION_POLL_INTERVAL).await;
    }
}

/// Scenario 5b — `atm workspace attach` against a separate isolated
/// server (attach's `--isolate` flag hardcodes the socket label as
/// `atm-<sessname>`, so it can't share the primary scenario server).
/// Asserts every window gets an `@atm-sidebar=1` pane, both hooks are
/// installed, and the second invocation is idempotent.
async fn scenario_workspace_attach(env: &E2eEnv) {
    let attach_session_name = format!("jghatt{}", std::process::id());
    let attach_socket_label = format!("atm-{attach_session_name}");
    // Hooks/scripts go to a sandboxed XDG_DATA_HOME so the test never
    // writes to `~/.local/share/atm/`.
    let attach_data_dir = tempfile::tempdir().expect("create attach data tempdir");

    let attach_tmux = PrivateTmux::start_with_label_session_env(
        &attach_socket_label,
        &attach_session_name,
        std::iter::empty::<(&str, &str)>(),
    );

    // Build a multi-window session — the interesting case for attach,
    // since per-window sidebar injection is the loop attach owns.
    let tmux_attach_client = RealTmuxClient::with_socket(attach_tmux.label().to_string());
    for label in ["new-window (2nd)", "new-window (3rd)"] {
        let mut cmd = Command::new("tmux");
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

    let attach_output = Command::new(&env.atm_path)
        .args(["workspace", "attach", &attach_session_name, "--isolate"])
        .env("ATM_NO_ATTACH", "1")
        .env("XDG_DATA_HOME", attach_data_dir.path())
        .env("XDG_STATE_HOME", &env.daemon.state_dir)
        .env("ATM_SOCKET", &env.daemon.socket_path)
        .output()
        .expect("run atm workspace attach");
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

    // Idempotency: running attach a second time must NOT add another
    // sidebar to any window. The production code keys off `@atm-sidebar`.
    let attach_output_2 = Command::new(&env.atm_path)
        .args(["workspace", "attach", &attach_session_name, "--isolate"])
        .env("ATM_NO_ATTACH", "1")
        .env("XDG_DATA_HOME", attach_data_dir.path())
        .env("XDG_STATE_HOME", &env.daemon.state_dir)
        .env("ATM_SOCKET", &env.daemon.socket_path)
        .output()
        .expect("run atm workspace attach (second time)");
    assert!(
        attach_output_2.status.success(),
        "second attach failed: stderr={}",
        String::from_utf8_lossy(&attach_output_2.stderr)
    );

    let final_panes = list_panes_with_sidebar_marker(attach_tmux.label())
        .expect("list panes after second attach");
    let final_sidebar_count = final_panes.iter().filter(|p| p.is_sidebar).count();
    assert_eq!(
        final_sidebar_count, 3,
        "second attach should be a no-op for already-injected windows, \
         but ended up with {final_sidebar_count} sidebars"
    );
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
    scenario_real_tmux_lib(&env).await;

    if python3_on_path().is_some() {
        scenario_atm_spawn(&env).await;
    } else {
        eprintln!("SKIP scenario 4: python3 not on PATH (scenario 5 still runs)");
    }

    scenario_workspace_create(&env).await;
    scenario_workspace_attach(&env).await;
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
    let output = Command::new("tmux")
        .args([
            "-L",
            socket_label,
            "list-panes",
            "-a",
            "-F",
            "#{window_id}|#{pane_id}|#{@atm-sidebar}",
        ])
        .output()
        .ok()?;
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

/// Tiny `tmux -L <label> ...` helper for scenario 5's hook-inspection
/// queries. Returns stdout on success.
fn tmux_run_capture(socket_label: &str, args: &[&str]) -> Option<String> {
    let mut cmd = Command::new("tmux");
    cmd.args(["-L", socket_label]);
    cmd.args(args);
    let output = cmd.output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).to_string())
}
