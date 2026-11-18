//! Chat's real CLI/controller boundary. All fixtures are synthetic; real daemon
//! checks below use only the private stand-in, never a model harness.
//! The shared integration rig and pty/procfs checks require Linux; portable
//! reducer/render coverage lives in atm-chat.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{process::Stdio, time::Duration};

async fn cli(args: &[&str]) -> std::process::Output {
    cli_with_state(args, None).await
}

async fn cli_with_state(args: &[&str], state: Option<&std::path::Path>) -> std::process::Output {
    let dir = tempfile::tempdir().unwrap();
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_atm"))
        .args(args)
        .env_clear()
        .env("HOME", dir.path())
        .env("PATH", dir.path())
        .env("ATM_SOCKET", dir.path().join("never-connect.sock"))
        .env("ATM_STATE_DIR", state.unwrap_or(dir.path()))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    // Keep ownership on timeout, so even failure reaps the child.
    let status = match tokio::time::timeout(Duration::from_secs(60), child.wait()).await {
        Ok(result) => result.unwrap(),
        Err(error) => {
            child.start_kill().unwrap();
            tokio::time::timeout(Duration::from_secs(60), child.wait())
                .await
                .unwrap()
                .unwrap();
            panic!("CLI hung: {error}");
        }
    };
    use tokio::io::AsyncReadExt;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    tokio::time::timeout(Duration::from_secs(60), async {
        child
            .stdout
            .take()
            .unwrap()
            .read_to_end(&mut stdout)
            .await
            .unwrap();
        child
            .stderr
            .take()
            .unwrap()
            .read_to_end(&mut stderr)
            .await
            .unwrap();
    })
    .await
    .expect("reaped CLI output hung");
    std::process::Output {
        status,
        stdout,
        stderr,
    }
}

// Break caught: chat is omitted from dispatch/help or --thread loses its typed argument.
#[tokio::test]
async fn chat_help_and_typed_thread_are_real_cli_commands() {
    for args in [
        vec!["chat", "--help"],
        vec!["chat", "--thread", "alpha/chief", "--help"],
    ] {
        let output = cli(&args).await;
        assert!(output.status.success(), "{output:?}");
        let help = String::from_utf8(output.stdout).unwrap();
        assert!(help.contains("--thread <THREAD>"), "{help}");
        assert!(help.contains("chat"), "{help}");
    }
}

// Break caught: chat guesses a project for a bare seat, or accepts malformed addresses.
#[tokio::test]
async fn chat_rejects_invalid_full_addresses_without_daemon_io() {
    for address in [
        "chief",
        "alpha/",
        "/chief",
        "alpha/chief/extra",
        "Alpha/chief",
    ] {
        let output = cli(&["chat", "--thread", address]).await;
        assert!(!output.status.success(), "{address}: {output:?}");
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(error.contains("invalid value"), "{address}: {error}");
        assert!(!error.contains("unrecognized subcommand"), "{error}");
    }
}

// Break caught: opening a FIFO in spawn_blocking keeps the runtime/process alive
// forever. Process exit is the synchronization; no duration/sleep assertion.
#[tokio::test]
async fn fifo_log_cannot_hang_chat_startup_or_runtime_shutdown() {
    let state = tempfile::tempdir().unwrap();
    let path = std::ffi::CString::new(state.path().join("tui.log").as_os_str().as_encoded_bytes())
        .unwrap();
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    let script = r#"
import fcntl, os, pty, select, signal, struct, subprocess, sys, termios
master, slave = pty.openpty()
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 32, 120, 0, 0))
before = termios.tcgetattr(slave)
def setup():
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)
child = None
reader = None
def hung(*_): raise TimeoutError('FIFO terminal event guard')
signal.signal(signal.SIGALRM, hung)
try:
    signal.alarm(90)
    child = subprocess.Popen([sys.argv[1], 'chat'], stdin=slave, stdout=slave,
        stderr=slave, preexec_fn=setup)
    output = bytearray()
    fd = os.pidfd_open(child.pid)
    try:
        while b'\x1b[?25l' not in output:
            ready = select.select([master, fd], [], [], 90)[0]
            assert ready, 'first fallback frame guard'
            if master in ready: output.extend(os.read(master, 65536))
            assert fd not in ready, ('chat exited instead of drawing without a log', output)
        # The opener stays blocked until the no-log frame really reached the tty.
        reader = os.open(os.path.join(os.environ['ATM_STATE_DIR'], 'tui.log'), os.O_RDONLY | os.O_NONBLOCK)
        os.write(master, b'q')
        while True:
            ready = select.select([master, fd], [], [], 60)[0]
            assert ready, 'chat exit guard'
            if master in ready: output.extend(os.read(master, 65536))
            if fd in ready: break
    finally:
        os.close(fd)
    assert child.wait(timeout=60) == 0, output
    assert termios.tcgetattr(slave) == before, 'raw terminal survived no-log chat'
    warning = b'chat measurements unavailable: log file could not be opened'
    assert output.count(warning) == 1, output
    assert b'chat log initialization timed out' not in output, output
    assert b'Warning: Failed' not in output, output
finally:
    signal.alarm(0)
    if child is not None and child.poll() is None:
        child.kill()
        child.wait(timeout=60)
    if reader is not None: os.close(reader)
    os.close(master)
    os.close(slave)
"#;
    run_pty_script(script, state.path()).await;
}

// Break caught: the worker and foreground both print about one failed log open.
#[tokio::test]
async fn unavailable_log_warns_once_before_terminal_setup() {
    let state = tempfile::tempdir().unwrap();
    std::fs::create_dir(state.path().join("tui.log")).unwrap();
    let output = cli_with_state(&["chat"], Some(state.path())).await;
    let error = String::from_utf8(output.stderr).unwrap();
    assert_eq!(
        error
            .matches("chat measurements unavailable: log file could not be opened")
            .count(),
        1,
        "{error}"
    );
    assert!(
        !error.contains("Warning: Failed"),
        "worker diagnostic escaped: {error}"
    );
}

// Break caught: setup enables raw mode, then fails entering alternate screen,
// and the empty RAII guard leaves the terminal raw on that partial-init path.
#[tokio::test]
async fn partial_terminal_initialization_restores_raw_mode() {
    let root = tempfile::tempdir().unwrap();
    let script = r#"
import fcntl, os, pty, signal, subprocess, sys, termios
master, slave = pty.openpty()
before = termios.tcgetattr(slave)
read_end, write_end = os.pipe()
os.close(read_end)
def setup():
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)
child = None
try:
    child = subprocess.Popen([sys.argv[1], 'chat', '--thread', 'alpha/chief'], stdin=slave, stdout=write_end,
        stderr=subprocess.PIPE, preexec_fn=setup)
    os.close(write_end)
    try:
        code = child.wait(timeout=60)
    except subprocess.TimeoutExpired:
        child.kill()
        child.wait(timeout=60)
        raise
    error = child.stderr.read().decode()
    assert code != 0 and 'Failed to initialize terminal' in error, (code, error)
    after = termios.tcgetattr(slave)
    assert after == before, ('raw terminal survived failed initialization', before, after)
    print('partial-init terminal restored')
finally:
    if child is not None and child.poll() is None:
        child.kill()
        child.wait(timeout=60)
    os.close(master)
    os.close(slave)
"#;
    run_pty_script(script, root.path()).await;
}

async fn run_pty_script(script: &str, root: &std::path::Path) {
    let harness = r#"
import os, pathlib, select, signal, socket, sys
# A listening private socket prevents DaemonClient's existing sibling-binary
# autostart path; this fixture tests terminal/logging, not daemon startup.
guard_listener = socket.socket(socket.AF_UNIX)
guard_listener.bind(os.environ['ATM_SOCKET'])
guard_listener.listen(32)
guard_owned = []
try:
    exec(sys.argv[2])
finally:
    guard_listener.close()
    guard_marker = ('ATM_STATE_DIR=' + os.environ['ATM_STATE_DIR']).encode()
    for guard_proc in pathlib.Path('/proc').iterdir():
        if not guard_proc.name.isdigit():
            continue
        try:
            guard_env = (guard_proc/'environ').read_bytes().split(b'\0')
            guard_exe = pathlib.Path(os.readlink(guard_proc/'exe')).name
            if guard_marker not in guard_env or guard_exe != 'atmd':
                continue
            guard_start = (guard_proc/'stat').read_text().rsplit(')',1)[1].split()[19]
            guard_fd = os.pidfd_open(int(guard_proc.name))
            try:
                assert guard_marker in (guard_proc/'environ').read_bytes().split(b'\0')
                assert guard_start == (guard_proc/'stat').read_text().rsplit(')',1)[1].split()[19]
                guard_owned.append(guard_proc.name)
                signal.pidfd_send_signal(guard_fd, signal.SIGTERM)
                assert select.select([guard_fd], [], [], 60)[0], 'owned daemon exit guard'
            finally:
                os.close(guard_fd)
        except (FileNotFoundError, PermissionError, ProcessLookupError):
            pass
    assert not guard_owned, ('terminal fixture unexpectedly autostarted daemons; stopped', guard_owned)
"#;
    let mut child = tokio::process::Command::new("python3")
        .args(["-c", harness, env!("CARGO_BIN_EXE_atm"), script])
        .env("HOME", root)
        .env("ATM_STATE_DIR", root)
        .env("ATM_SOCKET", root.join("never.sock"))
        .env("ATM_TMUX_SOCKET", "logger-fixture")
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .env("SHELL", "/bin/sh")
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(150), child.wait()).await;
    if result.is_err() {
        let _ = child.start_kill();
        let _ = tokio::time::timeout(Duration::from_secs(60), child.wait()).await;
    }
    assert!(result.unwrap().unwrap().success());
}

#[tokio::test]
async fn failed_log_writes_do_not_spill_into_the_terminal() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("tui.log"), b"").unwrap();
    let script = r#"
import fcntl, os, pty, resource, select, signal, struct, subprocess, sys, termios
def hung(*_): raise TimeoutError('terminal event guard')
signal.signal(signal.SIGALRM, hung)
master, slave = pty.openpty()
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 32, 120, 0, 0))
def setup():
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)
    signal.signal(signal.SIGXFSZ, signal.SIG_IGN)
    resource.setrlimit(resource.RLIMIT_FSIZE, (0, 0))
child = None
try:
    signal.alarm(60)
    child = subprocess.Popen([sys.argv[1], 'chat'], stdin=slave, stdout=slave, stderr=slave, preexec_fn=setup)
    output = bytearray()
    while b'\x1b[?25l' not in output:
        assert select.select([master], [], [], 60)[0]
        output.extend(os.read(master, 65536))
    os.write(master, b'q')
    fd = os.pidfd_open(child.pid)
    try:
        while True:
            ready = select.select([fd, master], [], [], 60)[0]
            assert ready
            if master in ready:
                output.extend(os.read(master, 65536))
            if fd in ready:
                break
    finally:
        os.close(fd)
    assert child.wait(timeout=60) == 0
    signal.alarm(0)
    if select.select([master], [], [], 0)[0]:
        output.extend(os.read(master, 65536))
    assert b'[tracing-subscriber]' not in output, output
finally:
    signal.alarm(0)
    if child is not None and child.poll() is None:
        child.kill()
        child.wait(timeout=60)
    os.close(master)
    os.close(slave)
"#;
    run_pty_script(script, root.path()).await;
}

#[path = "../src/bin/atm/chat.rs"]
mod chat;

use atm_chat::{ChatConfig, ChatEffect, ChatFailure, ChatInput, FrameReport, OpToken};
use atm_comms::message_types::{AckParams, SendParams};
use atm_protocol::{DaemonMessage, MessageType, ProtocolVersion, ResponsePayload};
use atm_tui::{
    client::{DaemonClient, DaemonConfig},
    control::ControlFailure,
    daemon::ControlHandle,
    input::{ClientCommand, Event},
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{backend::TestBackend, Terminal};
use serde_json::{json, Value};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{
        unix::{OwnedReadHalf, OwnedWriteHalf},
        UnixListener,
    },
    sync::mpsc,
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use tracing::instrument::WithSubscriber;

const METHODS: &[&str] = &[
    "org.tree",
    "msg.history",
    "msg.inbox",
    "msg.send",
    "msg.ack",
];
type DriverTask = JoinHandle<(anyhow::Result<()>, Terminal<TestBackend>)>;
type DriverFrames = (DriverTask, mpsc::UnboundedReceiver<FrameReport>);
const GUARD: Duration = Duration::from_secs(60);
async fn guarded<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(GUARD, future)
        .await
        .expect("chat test hang guard")
}
async fn matching_frame(
    frames: &mut mpsc::UnboundedReceiver<FrameReport>,
    predicate: impl Fn(&FrameReport) -> bool,
) -> FrameReport {
    guarded(async {
        loop {
            let frame = frames.recv().await.expect("chat frames closed");
            if predicate(&frame) {
                return frame;
            }
        }
    })
    .await
}

#[tokio::test(start_paused = true)]
async fn frame_predicate_has_one_guard_despite_periodic_draw_noise() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let noise = tokio::spawn(async move {
        let state = atm_chat::ChatState::new(atm_chat::ChatConfig::default());
        let mut terminal = Terminal::new(TestBackend::new(120, 32)).unwrap();
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tick.tick().await;
            let mut report = None;
            terminal
                .draw(|frame| report = Some(atm_chat::render(frame, &state)))
                .unwrap();
            if tx.send(report.unwrap()).is_err() {
                break;
            }
        }
    });
    let mut waiter =
        tokio::spawn(async move { matching_frame(&mut rx, |frame| frame.can_compose).await });
    let result = tokio::time::timeout(GUARD + Duration::from_secs(1), &mut waiter).await;
    noise.abort();
    let _ = noise.await;
    if result.is_err() {
        waiter.abort();
        let _ = waiter.await;
    }
    assert!(
        matches!(result,Ok(Err(error)) if error.is_panic()),
        "predicate must hit its own single guard, not wait forever on draw noise"
    );
}

// Scoped subscribers still share process-global callsite interests. Run each
// trace assertion alone, with a positive check that its exact child case ran.
#[must_use]
fn trace_case_in_child(case: &str) -> bool {
    use std::io::{Read, Seek};
    if std::env::var("ATM_CHAT_TRACE_CASE").as_deref() == Ok(case) {
        return false;
    }
    let mut output = tempfile::tempfile().unwrap();
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", case, "--nocapture"])
        .env("ATM_CHAT_TRACE_CASE", case)
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .stdin(Stdio::null())
        .stdout(output.try_clone().unwrap())
        .stderr(output.try_clone().unwrap())
        .spawn()
        .unwrap();
    let pid = child.id();
    let child = stand_in::process::OwnedChild::new(child);
    // The inherited idle-trace meter takes a real minute; this is only an exit
    // guard. Reuse the event-based wait and owner so failure/unwind reaps too.
    stand_in::process::wait_exit(pid, Duration::from_secs(180)).unwrap();
    let status = child.wait_with_output().unwrap().status;
    output.rewind().unwrap();
    let mut text = String::new();
    output.read_to_string(&mut text).unwrap();
    assert!(status.success(), "trace child {case}: {status}\n{text}");
    assert!(
        text.lines()
            .any(|line| line.starts_with("test result: ok. 1 passed; 0 failed;")),
        "expected exactly one child test for {case}: {text}"
    );
    true
}

fn run_id(pid: u32) -> chat::ChatRunId {
    chat::ChatRunId::new("2026-01-01T00:00:00Z".parse().unwrap(), pid)
}
fn tree(pane: Option<&str>) -> Value {
    json!({"seq":40,"generated_at":"2026-01-01T00:00:00Z","reporting":{"state":"ok","reason":null},
        "needs_owner":{"away_since":null,"owner_unread":0,"unnamed_attention":0,"items":[],"items_total":0},
        "projects":[{"id":"alpha","root":"/fixture/alpha","seats":[{"agent_id":"alpha/chief","state":"ready","activity":"idle","harness":null,"model":null,"context_percentage":null,"pane":pane,"manager":"owner","line_error":null,"tasks":[]}],"tasks_open":0,"tasks":[],"decisions":{"pending":0,"provisional":0}}],"unplaced_tasks":[]})
}
fn head() -> Value {
    json!({"seq":40,"snapshot_seq":40,"messages":[],"next_cursor":null})
}
fn inbox() -> Value {
    json!({"messages":[],"unread_count":0,"next_after":null})
}
fn send(token: u64) -> ChatEffect {
    ChatEffect::Send {
        token: OpToken(token),
        params: SendParams {
            to: "alpha/chief".parse().unwrap(),
            kind: atm_comms::MessageKind::Note,
            body: "literal λ\nsecond line".into(),
            attachments: vec![],
            task: None,
        },
    }
}
struct Peer {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}
impl Peer {
    async fn recv(&mut self) -> MessageType {
        self.recv_with_budget(GUARD).await
    }
    async fn recv_with_budget(&mut self, budget: Duration) -> MessageType {
        let mut line = String::new();
        assert_ne!(
            tokio::time::timeout(budget, self.reader.read_line(&mut line))
                .await
                .expect("peer read hang guard")
                .unwrap(),
            0,
            "peer closed"
        );
        serde_json::from_str::<atm_protocol::ClientMessage>(&line)
            .unwrap()
            .message
    }
    async fn send(&mut self, message: DaemonMessage) {
        guarded(
            self.writer
                .write_all(format!("{}\n", serde_json::to_string(&message).unwrap()).as_bytes()),
        )
        .await
        .unwrap();
    }
    async fn request(&mut self, want: &str, pane: Option<&str>) -> (String, Value) {
        let MessageType::Request {
            id,
            method,
            params,
            from_pane,
        } = self.recv().await
        else {
            panic!("expected {want}")
        };
        assert_eq!(method, want);
        assert_eq!(from_pane.as_deref(), pane);
        (id, params)
    }
    async fn ok(&mut self, id: String, value: Value) {
        self.send(DaemonMessage::response_ok(id, value)).await;
    }
}
struct Script {
    tmux: atm_tmux::MockTmuxClient,
    frame_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    _root: tempfile::TempDir,
    listener: UnixListener,
    commands: mpsc::UnboundedSender<ClientCommand>,
    events: Option<mpsc::UnboundedReceiver<Event>>,
    event_tx: mpsc::UnboundedSender<Event>,
    cancel: CancellationToken,
    client: JoinHandle<()>,
}
impl Script {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("chat.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (event_tx, events) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let client = DaemonClient::new(
            DaemonConfig {
                socket_path: socket,
                ..Default::default()
            },
            event_tx.clone(),
            command_rx,
            cancel.clone(),
        );
        let client = tokio::spawn(async move { client.run().await });
        Self {
            tmux: atm_tmux::MockTmuxClient::new(),
            frame_count: Default::default(),
            _root: root,
            listener,
            commands,
            events: Some(events),
            event_tx,
            cancel,
            client,
        }
    }
    fn control(&self) -> ControlHandle {
        ControlHandle::new(self.commands.clone())
    }
    async fn connect(&self) -> Peer {
        let (stream, _) = guarded(self.listener.accept()).await.unwrap();
        let (reader, writer) = stream.into_split();
        let mut peer = Peer {
            reader: BufReader::new(reader),
            writer,
        };
        assert!(matches!(peer.recv().await, MessageType::Connect { .. }));
        peer.send(DaemonMessage::Connected {
            protocol_version: ProtocolVersion::new(1, 1),
            client_id: "chat-test".into(),
            methods: METHODS.iter().map(|s| (*s).into()).collect(),
        })
        .await;
        assert!(matches!(peer.recv().await, MessageType::Subscribe { .. }));
        peer.send(DaemonMessage::session_list(vec![])).await;
        peer
    }
    async fn stop(&mut self) {
        self.cancel.cancel();
        guarded(&mut self.client).await.unwrap();
    }
    fn drive(&mut self, pane: Option<&str>) -> DriverFrames {
        let control = self.control();
        let events = self.events.take().unwrap();
        let cancel = self.cancel.clone();
        let config = ChatConfig {
            initial_thread: Some("alpha/chief".parse().unwrap()),
            caller_pane: pane.map(str::to_owned),
        };
        let (tx, rx) = mpsc::unbounded_channel();
        let frame_count = self.frame_count.clone();
        let tmux = self.tmux.clone();
        let driver = tokio::spawn(
            async move {
                let mut traced_frame = None;
                let mut terminal = Terminal::new(TestBackend::new(120, 32)).unwrap();
                let result = chat::drive(
                    &mut terminal,
                    config,
                    control,
                    events,
                    &tmux,
                    cancel,
                    run_id(7),
                    move |report| {
                        chat::trace_frame(&run_id(7), report, &mut traced_frame);
                        frame_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let _ = tx.send(report.clone());
                    },
                )
                .await;
                (result, terminal)
            }
            .with_current_subscriber(),
        );
        (driver, rx)
    }
}
impl Drop for Script {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.client.abort();
    }
}

// Break caught: caller attribution is stripped at the freshness check or actual mutation.
#[tokio::test]
async fn real_caller_pane_is_never_removed() {
    let mut rig = Script::new();
    let mut peer = rig.connect().await;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let worker = tokio::spawn(chat::execute_request(
        send(3),
        rig.control(),
        Some("%7".into()),
        tx,
        run_id(7),
    ));
    let (id, params) = peer.request("org.tree", Some("%7")).await;
    assert_eq!(params, json!({}));
    peer.ok(id, tree(None)).await;
    assert!(matches!(
        guarded(rx.recv()).await,
        Some(chat::Completion::Input(ChatInput::SendDispatched(OpToken(
            3
        ))))
    ));
    let (id, params) = peer.request("msg.send", Some("%7")).await;
    assert_eq!(params["to"], "alpha/chief");
    assert_eq!(params["body"], "literal λ\nsecond line");
    peer.ok(id, json!({"message_id":"m6"})).await;
    assert!(
        matches!(guarded(rx.recv()).await,Some(chat::Completion::Input(ChatInput::SendResult(OpToken(3),Ok(result)))) if result.message_id.to_string()=="m6")
    );
    guarded(worker).await.unwrap();
    rig.stop().await;
}

// Break caught: stale/failed tree evidence admits a mutation or reports an uncertain no-send.
#[tokio::test]
async fn precheck_answers_refused_without_send_or_ack_rpc() {
    for bound in [true, false] {
        for ack in [false, true] {
            let mut rig = Script::new();
            let mut peer = rig.connect().await;
            let (tx, mut rx) = mpsc::unbounded_channel();
            let effect = if ack {
                ChatEffect::Ack {
                    token: OpToken(3),
                    params: AckParams {
                        ids: vec!["m4".parse().unwrap()],
                    },
                }
            } else {
                send(3)
            };
            let worker = tokio::spawn(chat::execute_request(
                effect,
                rig.control(),
                Some("%7".into()),
                tx,
                run_id(7),
            ));
            let (id, _) = peer.request("org.tree", Some("%7")).await;
            if bound {
                peer.ok(id, tree(Some("%7"))).await
            } else {
                peer.send(DaemonMessage::response_err(
                    id,
                    "unavailable",
                    "tree unavailable",
                ))
                .await
            };
            let input = guarded(rx.recv()).await.unwrap();
            assert!(
                matches!(input,chat::Completion::Input(ChatInput::SendResult(OpToken(3),Err(ChatFailure::Refused(ref s)))|ChatInput::AckResult(OpToken(3),Err(ChatFailure::Refused(ref s)))) if s.starts_with("not sent:")),
                "{input:?}"
            );
            guarded(worker).await.unwrap();
            rig.commands.send(ClientCommand::Discover).unwrap();
            assert!(
                matches!(peer.recv().await, MessageType::Discover),
                "a mutation preceded the command fence"
            );
            rig.stop().await;
        }
    }
}

// Break caught: router/transport uncertainty is made into editable refusal.
#[test]
fn mutation_timeout_internal_unavailable_are_unknown() {
    for code in ["timeout", "internal", "unavailable", "future_error"] {
        let payload = ResponsePayload::Error(atm_protocol::ResponseError {
            code: code.into(),
            message: "failure\u{1b}[31m".into(),
        });
        assert!(
            matches!(
                chat::response_value(Ok(payload), true),
                Err(ChatFailure::OutcomeUnknown(_))
            ),
            "{code}"
        );
    }
    for code in ["invalid_params", "refused", "not_found"] {
        let payload = ResponsePayload::Error(atm_protocol::ResponseError {
            code: code.into(),
            message: "no admission".into(),
        });
        assert!(
            matches!(
                chat::response_value(Ok(payload), true),
                Err(ChatFailure::Refused(_))
            ),
            "{code}"
        );
    }
    for error in [
        atm_tui::control::ControlFailure::Disconnected,
        atm_tui::control::ControlFailure::Closed,
        atm_tui::control::ControlFailure::Timeout,
    ] {
        assert!(matches!(
            chat::response_value(Err(error), true),
            Err(ChatFailure::OutcomeUnknown(_))
        ));
    }
}

// ControlState rejects Unsupported before emitting any Send effect, so this
// traced pre-admission failure must not lock the draft as outcome-unknown.
#[test]
fn unsupported_mutation_is_a_traced_no_admission_refusal() {
    let error = atm_tui::control::ControlFailure::Unsupported("msg.send".into());
    assert!(
        matches!(chat::response_value(Err(error.clone()),true),Err(ChatFailure::Refused(message)) if message.starts_with("not sent:"))
    );
    assert!(matches!(
        chat::response_value(Err(error), false),
        Err(ChatFailure::TooOld(_))
    ));
}

// Break caught: bound startup starts Owner history/inbox or offers a seat-view fallback.
#[tokio::test]
async fn bound_start_refuses_before_owner_reads() {
    let mut rig = Script::new();
    let mut peer = rig.connect().await;
    let (driver, _frames) = rig.drive(Some("%7"));
    let (id, _) = peer.request("org.tree", Some("%7")).await;
    peer.ok(id, tree(Some("%7"))).await;
    let (result, _) = guarded(driver).await.unwrap();
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("chat runs from an unbound pane"));
    // Returning drive drops its event receiver; DaemonClient treats that as
    // shutdown. EOF after joining the client fences every possible Owner RPC.
    rig.stop().await;
    let mut line = String::new();
    while guarded(peer.reader.read_line(&mut line)).await.unwrap() != 0 {
        let message = serde_json::from_str::<atm_protocol::ClientMessage>(&line)
            .unwrap()
            .message;
        assert!(
            !matches!(message,MessageType::Request {ref method,..} if method.starts_with("msg.")),
            "Owner read escaped refusal: {message:?}"
        );
        line.clear();
    }
}

// Break caught: an outstanding read blocks keyboard shutdown or survives the driver.
#[tokio::test]
async fn shutdown_joins_stalled_readers() {
    let mut rig = Script::new();
    let mut peer = rig.connect().await;
    let (driver, _frames) = rig.drive(Some("%7"));
    peer.request("org.tree", Some("%7")).await;
    rig.event_tx
        .send(Event::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
        )))
        .unwrap();
    assert!(guarded(driver).await.unwrap().0.is_ok());
    rig.stop().await;
}

// Synthetic compatibility fixtures retain every original row, including broken
// framing, while adding the option column captured on tmux 3.8.
fn focus_rows(setting: &str, clients: &str) -> String {
    clients
        .split_inclusive('\n')
        .map(|line| {
            let (row, end) = line
                .strip_suffix('\n')
                .map_or((line, ""), |row| (row, "\n"));
            format!("{row}\t{setting}{end}")
        })
        .collect()
}

// Break caught: sticky focus, control-only clients, wrong pane or malformed evidence permits acknowledgment.
#[tokio::test]
async fn unfocused_locked_or_focus_off_cannot_ack() {
    let cases = [
        ("on", "focused,UTF-8\t%7\t0\n", true),
        ("on", "UTF-8\t%7\t0\n", false),
        ("on", "control-mode,focused\t%7\t0\n", false),
        ("off", "focused\t%7\t0\n", false),
        ("on", "focused\t%8\t0\n", false),
        ("unknown", "focused\t%7\t0\n", false),
        ("on", "notfocused\t%7\t0\n", false),
        ("on", "focused %7\n", false),
        ("on", "focused\t%7\textra\n", false),
        ("on", "focused\t%7\t0\nbad\n", false),
        ("on", "focused\t%7\t0", false),
        ("on", "focused,!invalid\t%7\t0\n", false),
        ("on", "focused,pause-after=\t%7\t0\n", false),
        ("on", "focused,control-mode=1\t%7\t0\n", false),
        ("on", "focused,pause-after=3\t%7\t0\n", true),
        ("on", "focused\t%7\t1\n", false), // actual copy-mode capture
        ("on", "focused\t%7\t2\n", false),
        ("on", "focused\t%7\t\n", false),
        ("on", "focused\t%7\n", false),
        ("on", "focused\t%7\t-1\n", false),
        ("on", "focused\t%7\t0\textra\n", false),
    ];
    for (option, clients, want) in cases {
        let tmux = atm_tmux::MockTmuxClient::new();
        let setting = match option {
            "on" => "1",
            "off" => "0",
            other => other,
        };
        tmux.set_next_raw_output(&focus_rows(setting, clients));
        let (visible, hint) = chat::visibility(&tmux, Some("%7")).await;
        assert_eq!(visible, want, "{option:?} {clients:?}");
        assert_eq!(hint.is_none(), want);
        if option == "on" {
            assert!(tmux.calls().iter().any(|call| matches!(call,atm_tmux::mock::MockCall::RawCommand {subcommand,args} if subcommand=="list-clients" && args==&["-F","#{client_flags}\t#{pane_id}\t#{pane_in_mode}\t#{focus-events}"])));
        }
    }
    let tmux = atm_tmux::MockTmuxClient::new();
    assert!(!chat::visibility(&tmux, None).await.0);
    assert!(tmux.calls().is_empty());
}

// Captured: tmux 3.6a and 3.8, 2026-11-18, private 90% display-popup
// over chat plus display-message -a. Popup/overlay fields are absent; only the
// byte counter changes. Do not invent a supported popup guard from empty fields.
#[tokio::test]
async fn popup_over_chat_retains_the_documented_visibility_limit() {
    let tmux = atm_tmux::MockTmuxClient::new();
    tmux.set_next_raw_output("attached,focused,UTF-8\t%0\t0\t1\n");
    assert_eq!(chat::visibility(&tmux, Some("%0")).await, (true, None));
}

// Break caught: a separate option read defeats one-command observations, or an
// expanded option field is ignored when deciding whether a drawn frame is seen.
#[tokio::test]
async fn visibility_reads_focus_setting_in_one_command() {
    let tmux = atm_tmux::MockTmuxClient::new();
    tmux.set_next_raw_output("attached,focused,UTF-8\t%7\t0\t1\n");
    assert_eq!(chat::visibility(&tmux, Some("%7")).await, (true, None));
    assert_eq!(tmux.calls().len(), 1);
}

// Break caught: an older tmux's empty option expansion silently disables Ack,
// or cached focus bypasses the option check on a later observation.
#[tokio::test]
async fn visibility_empty_focus_field_checks_the_option_each_time() {
    for option in ["on\n", "off\n", "unknown\n", ""] {
        let tmux = atm_tmux::MockTmuxClient::new();
        for _ in 0..2 {
            tmux.set_next_raw_output("attached,focused\t%7\t0\t\n");
            tmux.set_next_raw_output(option);
        }
        for _ in 0..2 {
            assert_eq!(
                chat::visibility(&tmux, Some("%7")).await.0,
                option == "on\n"
            );
        }
        assert_eq!(tmux.calls().len(), 4);
        assert!(
            matches!(&tmux.calls()[1],atm_tmux::mock::MockCall::RawCommand {subcommand,args} if subcommand=="show-options" && args==&["-s","-v","focus-events"])
        );
    }
}

// Break caught: off/unknown/mixed focus evidence is treated as an unsupported
// empty expansion and upgraded by a permissive fallback.
#[tokio::test]
async fn visibility_malformed_or_mixed_focus_fields_fail_closed() {
    for rows in [
        "focused\t%7\t0\t0\n",
        "focused\t%7\t0\ton\n",
        "focused\t%7\t0\t2\n",
        "focused\t%7\t0\t1\nattached\t%8\t0\t0\n",
        "focused\t%7\t0\t1\nattached\t%8\t0\t\n",
        "focused\t%7\t0\t\nattached\t%8\t0\t1\n",
    ] {
        let tmux = atm_tmux::MockTmuxClient::new();
        tmux.set_next_raw_output(rows);
        tmux.set_next_raw_output("on\n");
        assert!(!chat::visibility(&tmux, Some("%7")).await.0, "{rows:?}");
        assert_eq!(
            tmux.calls().len(),
            1,
            "malformed evidence must not fall back"
        );
    }
}

// Measure the real controller over 61 virtual seconds, not calls to visibility().
// A Pending-poll event fences each tick; q/driver completion fences every call.
#[tokio::test(start_paused = true)]
async fn visibility_idle_driver_minute() {
    let tmux = atm_tmux::MockTmuxClient::new();
    for _ in 0..256 {
        tmux.set_next_raw_output("attached,focused,UTF-8\t%7\t0\t1\n");
    }
    let probe = tmux.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_env_filter("atm::chat=info")
        .with_writer(std::io::sink)
        .finish();
    let (polled, mut processed) = mpsc::unbounded_channel();
    let (commands, _requests) = mpsc::unbounded_channel();
    let (input, events) = mpsc::unbounded_channel();
    let driver = tokio::spawn(async move {
        let mut terminal = Terminal::new(TestBackend::new(120, 32)).unwrap();
        let drive = chat::drive(
            &mut terminal,
            ChatConfig {
                initial_thread: None,
                caller_pane: Some("%7".into()),
            },
            ControlHandle::new(commands),
            events,
            &probe,
            CancellationToken::new(),
            run_id(30),
            |_| {},
        );
        tokio::pin!(drive);
        std::future::poll_fn(|context| {
            let result = std::future::Future::poll(drive.as_mut(), context);
            if result.is_pending() {
                let _ = polled.send(tokio::time::Instant::now());
            }
            result
        })
        .with_subscriber(subscriber)
        .await
    });
    guarded(processed.recv()).await.unwrap();
    tmux.clear_calls();
    let start = tokio::time::Instant::now();
    for _ in 0..61 {
        tokio::time::advance(Duration::from_secs(1)).await;
        let tick = tokio::time::Instant::now();
        guarded(async { while processed.recv().await.unwrap() < tick {} }).await;
    }
    input
        .send(Event::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
        )))
        .unwrap();
    guarded(driver).await.unwrap().unwrap();
    let elapsed = start.elapsed().as_secs_f64();
    let calls = tmux.calls();
    let options = calls.iter().filter(|call|matches!(call,atm_tmux::mock::MockCall::RawCommand {subcommand,..} if subcommand=="show-options")).count();
    let clients = calls.iter().filter(|call|matches!(call,atm_tmux::mock::MockCall::RawCommand {subcommand,..} if subcommand=="list-clients")).count();
    println!("VISIBILITY_METER virtual_seconds={elapsed:.0} commands={} commands_per_virtual_minute={:.3} options={options} clients={clients}",calls.len(),calls.len() as f64*60.0/elapsed);
    assert!(
        !calls.is_empty(),
        "the real controller must observe visibility"
    );
    assert_eq!(options, 0);
    assert_eq!(clients, calls.len());
    assert!(
        calls.len() <= 62,
        "unexpected extra observations: {}",
        calls.len()
    );
}

// Break caught: unchecked defaults hide missing fences or discard decision/binding facts.
#[test]
fn checked_tree_and_history_decode_preserve_scopes() {
    let mut value = tree(Some("%7"));
    value["needs_owner"]["items"] = json!([{"kind":"decision_pending","id":"d1","agent_id":null,"project":null,"since":null},{"kind":"decision_provisional","id":"d2","agent_id":"alpha/chief","project":"alpha","since":null}]);
    value["needs_owner"]["items_total"] = json!(8);
    let facts = chat::decode_tree(value, Some("%7"), &[]).unwrap();
    assert!(facts.caller_bound);
    assert_eq!(facts.waiting_decisions, 2);
    assert!(facts.decisions_truncated);
    assert_eq!(facts.seats[0].agent.to_string(), "alpha/chief");
    let page = chat::decode_page(head()).unwrap();
    assert_eq!((page.head, page.fence), (40, 40));
    for invalid in [
        json!({}),
        json!({"seq":40,"snapshot_seq":"40","messages":[],"next_cursor":null}),
        json!({"seq":40,"snapshot_seq":40,"messages":[],"next_cursor":3}),
    ] {
        assert!(chat::decode_page(invalid).is_err());
    }
    assert!(chat::decode_tree(json!({"seq":40}), Some("%7"), &[]).is_err());
}

// Break caught: appending runs joins reused token 3 to the wrong committed count/id.
#[test]
fn trace_tokens_are_scoped_to_run_id() {
    if trace_case_in_child("trace_tokens_are_scoped_to_run_id") {
        return;
    }
    use std::{
        io::Write,
        sync::{Arc, Mutex},
    };
    #[derive(Clone)]
    struct Capture(Arc<Mutex<Vec<u8>>>);
    impl Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let capture = Capture(Arc::new(Mutex::new(vec![])));
    let writer = capture.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        chat::trace_effect(&run_id(1), &send(3)); // remains unknown, no committed event
        let mut second = send(3);
        if let ChatEffect::Send { params, .. } = &mut second {
            params.body = "one two three four five six seven".into()
        }
        chat::trace_effect(&run_id(2), &second);
        chat::trace_effect(
            &run_id(2),
            &ChatEffect::RecordSend {
                token: OpToken(3),
                message_id: "m9".parse().unwrap(),
                word_count: 7,
            },
        );
    });
    let log = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    let commits: Vec<_> = log
        .lines()
        .filter(|line| line.contains("chat.send_committed"))
        .collect();
    assert_eq!(commits.len(), 1, "{log}");
    assert!(
        commits[0].contains("run_id=2026-01-01T00:00:00+00:00-2"),
        "{log}"
    );
    assert!(
        commits[0].contains("word_count=7")
            && commits[0].contains("token=3")
            && commits[0].contains("m9"),
        "{log}"
    );
    assert!(log.lines().all(|line| line.contains("run_id=")), "{log}");
    assert!(
        !log.contains("literal λ") && !log.contains("one two three"),
        "body leaked: {log}"
    );
}

async fn ready_chat(
    rig: &mut Script,
) -> (
    Peer,
    JoinHandle<(anyhow::Result<()>, Terminal<TestBackend>)>,
    mpsc::UnboundedReceiver<FrameReport>,
) {
    ready_chat_with_tree(rig, tree(None)).await
}

async fn ready_chat_with_tree(
    rig: &mut Script,
    facts: Value,
) -> (Peer, DriverTask, mpsc::UnboundedReceiver<FrameReport>) {
    let mut peer = rig.connect().await;
    let (driver, mut frames) = rig.drive(Some("%7"));
    let (id, _) = peer.request("org.tree", Some("%7")).await;
    peer.ok(id, facts).await;
    let mut watched = false;
    let mut history = false;
    let mut summary = false;
    while !(watched && history && summary) {
        match peer.recv().await {
            MessageType::Watch { topics, cursor } => {
                assert_eq!(cursor, 40);
                assert_eq!(topics, ["msg.", "agent.", "project.", "decision."]);
                assert!(!watched);
                watched = true;
            }
            MessageType::Request {
                id,
                method,
                params,
                from_pane,
            } => {
                assert_eq!(from_pane.as_deref(), Some("%7"));
                match method.as_str() {
                    "msg.history" => {
                        assert!(!history);
                        history = true;
                        assert_eq!(params["limit"], 100);
                        assert_eq!(params["agent"], "alpha/chief");
                        assert_eq!(params["correspondent"], "owner");
                        peer.ok(id, head()).await;
                    }
                    "msg.inbox" => {
                        assert!(!summary);
                        summary = true;
                        assert_eq!(params["for"], "owner");
                        assert_eq!(params["after"], Value::Null);
                        assert_eq!(params["limit"], 100);
                        assert_eq!(params["unread_only"], true);
                        peer.ok(id, inbox()).await;
                    }
                    _ => panic!("unexpected startup method {method}"),
                }
            }
            other => panic!("unexpected startup envelope {other:?}"),
        }
    }
    matching_frame(&mut frames, |frame| frame.can_compose).await;
    (peer, driver, frames)
}
async fn stop_chat(
    rig: &mut Script,
    driver: JoinHandle<(anyhow::Result<()>, Terminal<TestBackend>)>,
) {
    rig.event_tx
        .send(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)))
        .unwrap();
    rig.event_tx
        .send(Event::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
        )))
        .unwrap();
    assert!(guarded(driver).await.unwrap().0.is_ok());
    rig.stop().await;
}

// Break caught: the controller puts selected body text back into raw argv or
// accidentally pastes it instead of using the bounded stdin buffer operation.
#[tokio::test]
async fn copy_selected_body_uses_the_stdin_buffer_operation() {
    let mut rig = Script::new();
    let (mut peer, driver, mut frames) = ready_chat(&mut rig).await;
    let body = "- literal λ\nsecond line;";
    rig.event_tx
        .send(Event::Control(atm_tui::control::ControlNotice::Event(
            DaemonMessage::event(
                "org".into(),
                41,
                "2026-01-01T00:00:00Z".into(),
                "msg.sent".into(),
                json!({}),
            ),
        )))
        .unwrap();
    for _ in 0..2 {
        let MessageType::Request { id, method, .. } = peer.recv().await else {
            panic!("message refresh")
        };
        let value = match method.as_str() {
            "msg.history" => {
                json!({"seq":41,"snapshot_seq":41,"messages":[incoming(body)],"next_cursor":null})
            }
            "msg.inbox" => inbox(),
            other => panic!("unexpected copy setup {other}"),
        };
        peer.ok(id, value).await;
    }
    let frame = matching_frame(&mut frames, |frame| {
        frame.visible_ids.contains(&"m5".parse().unwrap())
    })
    .await;
    rig.event_tx
        .send(Event::Key(KeyEvent::new(
            KeyCode::Char('y'),
            KeyModifiers::NONE,
        )))
        .unwrap();
    matching_frame(&mut frames, |next| next.generation > frame.generation).await;
    stop_chat(&mut rig, driver).await;
    let copies: Vec<_> = rig.tmux.calls().into_iter().filter(|call| !matches!(call,atm_tmux::mock::MockCall::RawCommand {subcommand,..} if subcommand=="list-clients")).collect();
    assert_eq!(
        copies,
        vec![atm_tmux::mock::MockCall::LoadBuffer { text: body.into() }]
    );
}

// Composition check for the internal watch marker: an actual no-id catch-up
// error stays a compatible daemon, retries one Watch, and refreshes to writable.
#[tokio::test]
async fn runtime_watch_failure_rewatches_and_recovers_without_becoming_too_old() {
    let mut rig = Script::new();
    let (mut peer, driver, mut frames) = ready_chat(&mut rig).await;
    peer.send(DaemonMessage::error_with_code(
        "synthetic catch-up failure",
        "unavailable",
    ))
    .await;
    matching_frame(&mut frames, |frame| !frame.can_compose).await;
    let mut unwatch = 0;
    let mut pings = 0;
    let mut watched = false;
    let mut seen = std::collections::BTreeSet::new();
    // ControlState stops the failed stream, then replaces it using an explicit
    // Unwatch/Ping/Pong barrier. Refresh RPCs can race that barrier, not its Watch.
    while !(watched && seen.len() == 3) {
        match peer.recv().await {
            MessageType::Unwatch => {
                unwatch += 1;
                assert!(unwatch <= 2);
            }
            MessageType::Ping { seq } => {
                pings += 1;
                assert_eq!(pings, 1);
                peer.send(DaemonMessage::pong(seq)).await;
            }
            MessageType::Watch { topics, cursor } => {
                assert!(!watched);
                assert_eq!(topics, ["msg.", "agent.", "project.", "decision."]);
                assert_eq!(cursor, 40);
                watched = true;
            }
            MessageType::Request {
                id,
                method,
                from_pane,
                ..
            } => {
                assert_eq!(from_pane.as_deref(), Some("%7"));
                let value = match method.as_str() {
                    "org.tree" => tree(None),
                    "msg.history" => head(),
                    "msg.inbox" => inbox(),
                    other => panic!("refresh {other}"),
                };
                assert!(seen.insert(method));
                peer.ok(id, value).await;
            }
            other => panic!("watch retry barrier: {other:?}"),
        }
    }
    assert_eq!((unwatch, pings), (2, 1));
    matching_frame(&mut frames, |frame| frame.can_compose).await;
    rig.commands.send(ClientCommand::Discover).unwrap();
    assert!(
        matches!(peer.recv().await, MessageType::Discover),
        "duplicate Watch or refresh preceded the command fence"
    );
    rig.event_tx
        .send(Event::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
        )))
        .unwrap();
    let (result, terminal) = guarded(driver).await.unwrap();
    rig.stop().await;
    result.unwrap();
    let screen: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(!screen.contains("daemon too old"), "{screen}");
}

// Shared only by this test: expose an external terminal-size change without a
// keyboard Resize event, and count actual backend draw calls (including empty diffs).
#[derive(Clone)]
struct ResizableBackend(
    std::sync::Arc<std::sync::Mutex<TestBackend>>,
    std::sync::Arc<std::sync::atomic::AtomicUsize>,
);
impl ratatui::backend::Backend for ResizableBackend {
    fn draw<'a, I>(&mut self, content: I) -> std::io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a ratatui::buffer::Cell)>,
    {
        self.1.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        ratatui::backend::Backend::draw(&mut *self.0.lock().unwrap(), content)
    }
    fn hide_cursor(&mut self) -> std::io::Result<()> {
        ratatui::backend::Backend::hide_cursor(&mut *self.0.lock().unwrap())
    }
    fn show_cursor(&mut self) -> std::io::Result<()> {
        ratatui::backend::Backend::show_cursor(&mut *self.0.lock().unwrap())
    }
    fn get_cursor_position(&mut self) -> std::io::Result<ratatui::layout::Position> {
        ratatui::backend::Backend::get_cursor_position(&mut *self.0.lock().unwrap())
    }
    fn set_cursor_position<P: Into<ratatui::layout::Position>>(
        &mut self,
        position: P,
    ) -> std::io::Result<()> {
        ratatui::backend::Backend::set_cursor_position(&mut *self.0.lock().unwrap(), position)
    }
    fn clear(&mut self) -> std::io::Result<()> {
        ratatui::backend::Backend::clear(&mut *self.0.lock().unwrap())
    }
    fn size(&self) -> std::io::Result<ratatui::layout::Size> {
        ratatui::backend::Backend::size(&*self.0.lock().unwrap())
    }
    fn window_size(&mut self) -> std::io::Result<ratatui::backend::WindowSize> {
        ratatui::backend::Backend::window_size(&mut *self.0.lock().unwrap())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        ratatui::backend::Backend::flush(&mut *self.0.lock().unwrap())
    }
}

// Break caught: a generation-only gate misses physical size changes. The final
// Resize is an independent positive FIFO fence, so the mutant fails a draw count
// immediately rather than waiting for a missing frame. Entirely in memory.
#[tokio::test(start_paused = true)]
async fn terminal_size_alone_requires_a_draw_before_the_resize_input() {
    let inner = std::sync::Arc::new(std::sync::Mutex::new(TestBackend::new(120, 32)));
    let draws = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let backend = ResizableBackend(inner.clone(), draws.clone());
    let (commands, _requests) = mpsc::unbounded_channel();
    let control = ControlHandle::new(commands);
    let (input, events) = mpsc::unbounded_channel();
    let (tx, mut frames) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        let mut terminal = Terminal::new(backend).unwrap();
        chat::drive(
            &mut terminal,
            ChatConfig {
                initial_thread: Some("alpha/chief".parse().unwrap()),
                caller_pane: None,
            },
            control,
            events,
            &atm_tmux::MockTmuxClient::new(),
            CancellationToken::new(),
            run_id(8),
            move |frame| {
                let _ = tx.send(frame.clone());
            },
        )
        .await
    });
    let first = guarded(frames.recv()).await.unwrap();
    let ready = matching_frame(&mut frames, |frame| frame.generation > first.generation).await;
    let before = draws.load(std::sync::atomic::Ordering::Relaxed);
    inner.lock().unwrap().resize(90, 20);
    input
        .send(Event::Key(KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::NONE,
        )))
        .unwrap();
    input.send(Event::Resize(90, 20)).unwrap();
    matching_frame(&mut frames, |frame| frame.generation > ready.generation).await;
    let count = draws.load(std::sync::atomic::Ordering::Relaxed) - before;
    input
        .send(Event::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
        )))
        .unwrap();
    guarded(task).await.unwrap().unwrap();
    assert_eq!(
        count, 2,
        "physical size change and then accepted Resize each require a draw"
    );
}

#[derive(Clone)]
struct MeterWriter {
    bytes: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    presented: mpsc::UnboundedSender<()>,
}
impl std::io::Write for MeterWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.bytes.lock().unwrap().extend_from_slice(bytes);
        if String::from_utf8_lossy(bytes).contains("chat.presented ") {
            let _ = self.presented.send(());
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// Break caught: an unchanged idle session or periodic focus result redraws and
// appends logs. The minute is measured by 61 positive one-second interval events,
// NEVER a sleep/absence timeout; the final resize is a processed FIFO frame fence.
#[tokio::test]
async fn idle_driver_minute_has_no_redundant_draws_or_trace_records() {
    if tokio::task::spawn_blocking(|| {
        trace_case_in_child("idle_driver_minute_has_no_redundant_draws_or_trace_records")
    })
    .await
    .unwrap()
    {
        return;
    }
    let (presented, mut observed) = mpsc::unbounded_channel();
    let bytes = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let writer = MeterWriter {
        bytes: bytes.clone(),
        presented,
    };
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_env_filter("atm::chat=info")
        .with_writer(move || writer.clone())
        .finish();
    async {
        let mut rig=Script::new();
        let (mut peer,driver,mut frames)=ready_chat_with_tree(&mut rig,tree(Some("%8"))).await;
        guarded(observed.recv()).await.unwrap();
        let stopped=CancellationToken::new();let stop=stopped.clone();
        let responder=tokio::spawn(async move {
            loop {
                let input=tokio::select! {_=stop.cancelled()=>break,input=peer.recv_with_budget(Duration::from_secs(180))=>input};
                match input {
                    MessageType::Ping {seq}=>peer.send(DaemonMessage::pong(seq)).await,
                    MessageType::Request {id,method,..}=> {
                        let value=match method.as_str() {
                            "msg.history"=> {let mut row=incoming("idle meter canonical read");row["read_at"]=json!("2026-01-01T00:00:01Z");json!({"seq":40,"snapshot_seq":40,"messages":[row],"next_cursor":null})},
                            "msg.inbox"=>inbox(),
                            other=>panic!("unexpected idle method {other}"),
                        };
                        peer.ok(id,value).await;
                    },
                    MessageType::Unwatch=>{},
                    other=>panic!("unexpected idle envelope {other:?}"),
                }
            }
        });
        let mut root=atm_core::SessionDomain::new(atm_comms::SessionId::new("meter-idle"),atm_core::AgentType::GeneralPurpose,atm_core::Model::Unknown);
        root.tmux_pane=Some("%8".into());root.status=atm_core::SessionStatus::Idle;
        let view=atm_core::SessionView::from_domain_at(&root,"2026-01-01T00:00:00Z".parse().unwrap());
        rig.event_tx.send(Event::SessionUpdate(vec![view.clone()])).unwrap();
        for code in [KeyCode::Char('h'),KeyCode::Char('G'),KeyCode::Enter] {rig.event_tx.send(Event::Key(KeyEvent::new(code,KeyModifiers::NONE))).unwrap();}
        let warm=matching_frame(&mut frames,|frame|frame.thread==atm_chat::ThreadKey::All && frame.read_ids.contains(&"m5".parse().unwrap())).await;
        let start_lines=String::from_utf8(bytes.lock().unwrap().clone()).unwrap().lines().count();
        let start_draws=rig.frame_count.load(std::sync::atomic::Ordering::Relaxed);
        let start=std::time::Instant::now();
        let mut pulse=tokio::time::interval(Duration::from_secs(1));
        pulse.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        for _ in 0..61 {pulse.tick().await;rig.event_tx.send(Event::SessionUpdate(vec![view.clone()])).unwrap();}
        rig.event_tx.send(Event::Resize(119,32)).unwrap();
        matching_frame(&mut frames,|frame|frame.generation>warm.generation && frame.read_ids.contains(&"m5".parse().unwrap())).await;
        let elapsed=start.elapsed().as_secs_f64();
        let log=String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        let lines=log.lines().count()-start_lines;
        let draws=rig.frame_count.load(std::sync::atomic::Ordering::Relaxed)-start_draws;
        println!("IDLE_METER seconds={elapsed:.3} lines={lines} lines_per_minute={:.3} draws={draws} idle_seats=1 session_updates=61",lines as f64*60.0/elapsed);
        stopped.cancel();guarded(responder).await.unwrap();
        stop_chat(&mut rig,driver).await;
        assert_eq!(lines,0,"steady idle traces must be quiet: {log}");
        assert_eq!(draws,1,"only the changed-size final barrier may draw");
    }.with_subscriber(subscriber).await;
}

// Break caught: capabilities/watch loss leaves an eligible composer after the processed frame.
#[tokio::test]
async fn capability_loss_is_read_only() {
    let mut rig = Script::new();
    let (_peer, driver, mut frames) = ready_chat(&mut rig).await;
    let before = frames.try_recv().ok().map(|f| f.generation).unwrap_or(0);
    rig.event_tx
        .send(Event::Control(
            atm_tui::control::ControlNotice::Capabilities {
                protocol_version: ProtocolVersion::new(1, 0),
                methods: vec![],
            },
        ))
        .unwrap();
    matching_frame(&mut frames, |frame| {
        frame.generation > before && !frame.can_compose
    })
    .await;
    stop_chat(&mut rig, driver).await;
}

// Break caught: a message burst reloads tree, installs another Watch, or fails to coalesce reads.
#[tokio::test]
async fn topic_bursts_do_not_reload_tree_for_messages() {
    let mut rig = Script::new();
    let (mut peer, driver, mut frames) = ready_chat(&mut rig).await;
    for seq in 41..105 {
        rig.event_tx
            .send(Event::Control(atm_tui::control::ControlNotice::Event(
                DaemonMessage::event(
                    "org".into(),
                    seq,
                    "2026-01-01T00:00:00Z".into(),
                    "msg.sent".into(),
                    json!({}),
                ),
            )))
            .unwrap();
    }
    // This FIFO capability change is a processed-frame barrier after the burst.
    rig.event_tx
        .send(Event::Control(
            atm_tui::control::ControlNotice::Capabilities {
                protocol_version: ProtocolVersion::new(1, 0),
                methods: vec![],
            },
        ))
        .unwrap();
    matching_frame(&mut frames, |frame| !frame.can_compose).await;
    let mut counts = std::collections::BTreeMap::new();
    for _ in 0..2 {
        let MessageType::Request {
            id, method, params, ..
        } = peer.recv().await
        else {
            panic!("one Watch only")
        };
        assert!(
            matches!(method.as_str(), "msg.history" | "msg.inbox"),
            "{method}"
        );
        if method == "msg.inbox" {
            assert_eq!(params["after"], Value::Null);
        }
        *counts.entry(method.clone()).or_insert(0) += 1;
        peer.ok(
            id,
            if method == "msg.history" {
                head()
            } else {
                inbox()
            },
        )
        .await;
    }
    stop_chat(&mut rig, driver).await;
    let mut line = String::new();
    loop {
        match guarded(peer.reader.read_line(&mut line)).await {
            Ok(0) => break,
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => break,
            Err(error) => panic!("shutdown wire: {error}"),
        }
        match serde_json::from_str::<atm_protocol::ClientMessage>(&line)
            .unwrap()
            .message
        {
            MessageType::Request { method, .. } => {
                assert!(
                    matches!(method.as_str(), "msg.history" | "msg.inbox"),
                    "{method}"
                );
                *counts.entry(method).or_insert(0) += 1;
            }
            MessageType::Watch { .. } => panic!("duplicate Watch"),
            _ => {}
        }
        line.clear();
    }
    assert!((1..=2).contains(counts.get("msg.history").unwrap()));
    assert!((1..=2).contains(counts.get("msg.inbox").unwrap()));
}

// Break caught: a lost mutation result is retried on transport reconnect.
#[tokio::test]
async fn unknown_send_is_not_replayed() {
    let mut rig = Script::new();
    let mut peer = rig.connect().await;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let worker = tokio::spawn(chat::execute_request(
        send(3),
        rig.control(),
        Some("%7".into()),
        tx,
        run_id(7),
    ));
    let (id, _) = peer.request("org.tree", Some("%7")).await;
    peer.ok(id, tree(None)).await;
    assert!(matches!(
        guarded(rx.recv()).await,
        Some(chat::Completion::Input(ChatInput::SendDispatched(OpToken(
            3
        ))))
    ));
    let (_id, params) = peer.request("msg.send", Some("%7")).await;
    assert_eq!(params["body"], "literal λ\nsecond line"); // accept the write, lose only its response
    drop(peer);
    assert!(matches!(
        guarded(rx.recv()).await,
        Some(chat::Completion::Input(ChatInput::SendResult(
            OpToken(3),
            Err(ChatFailure::OutcomeUnknown(_))
        )))
    ));
    guarded(worker).await.unwrap();
    let mut peer = rig.connect().await;
    rig.commands.send(ClientCommand::Discover).unwrap();
    assert!(matches!(peer.recv().await, MessageType::Discover));
    rig.stop().await;
}

// Break caught: out-of-order operation completions get relabeled and revive an obsolete search.
#[tokio::test]
async fn old_request_completion_is_ignored() {
    use atm_chat::{ChatState, ChatTime};
    let now = ChatTime {
        utc: "2026-01-01T00:00:00Z".parse().unwrap(),
        elapsed: Duration::ZERO,
    };
    let mut state = ChatState::new(ChatConfig {
        initial_thread: Some("alpha/chief".parse().unwrap()),
        caller_pane: Some("%7".into()),
    });
    let _ = state.reduce(
        ChatInput::Connection {
            online: true,
            methods: METHODS.iter().map(|m| (*m).into()).collect(),
            watch_available: true,
        },
        now,
    );
    let ChatEffect::LoadThreads { token } = state.reduce(ChatInput::Start, now).remove(0) else {
        panic!("tree")
    };
    let effects = state.reduce(
        ChatInput::ThreadsLoaded(token, chat::decode_tree(tree(None), Some("%7"), &[])),
        now,
    );
    let old = effects
        .into_iter()
        .find(|e| matches!(e, ChatEffect::LoadPage { .. }))
        .unwrap();
    for code in [KeyCode::Char('h'), KeyCode::Char('G')] {
        let _ = state.reduce(ChatInput::Key(KeyEvent::new(code, KeyModifiers::NONE)), now);
    }
    let new = state
        .reduce(
            ChatInput::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            now,
        )
        .into_iter()
        .find(|e| matches!(e, ChatEffect::LoadPage { .. }))
        .unwrap();
    let mut rig = Script::new();
    let mut peer = rig.connect().await;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let old_token = match &old {
        ChatEffect::LoadPage { token, .. } => *token,
        _ => panic!("old page"),
    };
    let new_token = match &new {
        ChatEffect::LoadPage { token, .. } => *token,
        _ => panic!("new page"),
    };
    let first = tokio::spawn(chat::execute_request(
        old,
        rig.control(),
        Some("%7".into()),
        tx.clone(),
        run_id(7),
    ));
    let second = tokio::spawn(chat::execute_request(
        new,
        rig.control(),
        Some("%7".into()),
        tx,
        run_id(7),
    ));
    let mut replies = vec![];
    for _ in 0..2 {
        let (id, params) = peer.request("msg.history", Some("%7")).await;
        replies.push((id, !params["agent"].is_null()));
    }
    replies.sort_by_key(|(_, old)| *old);
    for (id, _) in replies {
        peer.ok(id, head()).await;
    }
    let chat::Completion::Input(new) = guarded(rx.recv()).await.unwrap() else {
        panic!("new")
    };
    assert!(matches!(&new,ChatInput::PageLoaded(token,_) if *token==new_token));
    let _ = state.reduce(new, now);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    let mut before = None;
    terminal
        .draw(|f| before = Some(atm_chat::render(f, &state)))
        .unwrap();
    let chat::Completion::Input(old) = guarded(rx.recv()).await.unwrap() else {
        panic!("old")
    };
    assert!(matches!(&old,ChatInput::PageLoaded(token,_) if *token==old_token));
    assert!(state.reduce(old, now).is_empty());
    let mut after = None;
    terminal
        .draw(|f| after = Some(atm_chat::render(f, &state)))
        .unwrap();
    assert_eq!(after, before);
    guarded(first).await.unwrap();
    guarded(second).await.unwrap();
    rig.stop().await;
}

fn incoming(body: &str) -> Value {
    json!({"message_id":"m5","content":{"from":"alpha/chief","to":"owner","kind":"answer","body":body,"attachments":[],"task":null},
        "sent_at":"2026-01-01T00:00:00Z","status":"delivered","read_at":null,
        "delivery":{"phase":"delivered","reason":null,"attempt":0,"pane":null,"session_id":null,"confirmation":null,"retry_at":null,"blocked_by":null}})
}
fn state_with_incoming(body: &str) -> atm_chat::ChatState {
    state_with_entry(incoming(body))
}
fn state_with_entry(entry: Value) -> atm_chat::ChatState {
    use atm_chat::{ChatState, ChatTime};
    let now = ChatTime {
        utc: "2026-01-01T00:00:00Z".parse().unwrap(),
        elapsed: Duration::ZERO,
    };
    let mut state = ChatState::new(ChatConfig {
        initial_thread: Some("alpha/chief".parse().unwrap()),
        caller_pane: Some("%7".into()),
    });
    let _ = state.reduce(
        ChatInput::Connection {
            online: true,
            methods: METHODS.iter().map(|s| (*s).into()).collect(),
            watch_available: true,
        },
        now,
    );
    let ChatEffect::LoadThreads { token } = state.reduce(ChatInput::Start, now).remove(0) else {
        panic!("tree")
    };
    let effects = state.reduce(
        ChatInput::ThreadsLoaded(token, chat::decode_tree(tree(None), Some("%7"), &[])),
        now,
    );
    let token = effects
        .iter()
        .find_map(|e| {
            if let ChatEffect::LoadPage { token, .. } = e {
                Some(*token)
            } else {
                None
            }
        })
        .unwrap();
    let page = chat::decode_page(
        json!({"seq":40,"snapshot_seq":40,"messages":[entry],"next_cursor":null}),
    )
    .unwrap();
    let _ = state.reduce(ChatInput::PageLoaded(token, Ok(page)), now);
    state
}

// Break caught: the real draw/focus/reducer transition fails to emit a single Ack,
// or marks a canonical row read merely because the RPC acknowledged it.
#[tokio::test]
async fn presented_to_ack_then_canonical_refresh() {
    use atm_chat::ChatTime;
    let mut state = state_with_incoming("a complete visible reply");
    let now = ChatTime {
        utc: "2026-01-01T00:00:00Z".parse().unwrap(),
        elapsed: Duration::ZERO,
    };
    let mut terminal = Terminal::new(TestBackend::new(120, 32)).unwrap();
    let mut report = None;
    terminal
        .draw(|f| report = Some(atm_chat::render(f, &state)))
        .unwrap();
    let report = report.unwrap();
    assert_eq!(report.visible_ids, vec!["m5".parse().unwrap()]);
    assert!(report.read_ids.is_empty());
    for clients in [
        "attached\t%7\n",
        "attached,control-mode,focused\t%7\t0\n",
        "attached,focused\t%8\t0\n",
        "attached,focused\t%7\t1\n",
    ] {
        let tmux = atm_tmux::MockTmuxClient::new();
        tmux.set_next_raw_output(&focus_rows("1", clients));
        let (visible, hint) = chat::visibility(&tmux, Some("%7")).await;
        let mut displayed = None;
        terminal
            .draw(|f| displayed = Some(atm_chat::render(f, &state)))
            .unwrap();
        let displayed = displayed.unwrap();
        let effects = state.reduce(
            ChatInput::Presented {
                generation: displayed.generation,
                visible_ids: displayed.visible_ids,
                visible,
                hint,
            },
            now,
        );
        assert!(!effects.iter().any(|e| matches!(e, ChatEffect::Ack { .. })));
    }
    let mut report = None;
    terminal
        .draw(|f| report = Some(atm_chat::render(f, &state)))
        .unwrap();
    let report = report.unwrap();
    let tmux = atm_tmux::MockTmuxClient::new();
    tmux.set_next_raw_output("attached,focused\t%7\t0\t1\n");
    let (visible, hint) = chat::visibility(&tmux, Some("%7")).await;
    let effects = state.reduce(
        ChatInput::Presented {
            generation: report.generation,
            visible_ids: report.visible_ids,
            visible,
            hint,
        },
        now,
    );
    let ack = effects
        .into_iter()
        .find(|e| matches!(e, ChatEffect::Ack { .. }))
        .expect("presented must Ack immediately");
    let token = match &ack {
        ChatEffect::Ack { token, params } => {
            assert_eq!(params.ids, vec!["m5".parse().unwrap()]);
            *token
        }
        _ => panic!("ack"),
    };
    let mut rig = Script::new();
    let mut peer = rig.connect().await;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let worker = tokio::spawn(chat::execute_request(
        ack,
        rig.control(),
        Some("%7".into()),
        tx,
        run_id(7),
    ));
    let (id, _) = peer.request("org.tree", Some("%7")).await;
    peer.ok(id, tree(None)).await;
    let (id, params) = peer.request("msg.ack", Some("%7")).await;
    assert_eq!(params, json!({"ids":["m5"]}));
    peer.ok(id, json!({"count":1})).await;
    let chat::Completion::Input(result) = guarded(rx.recv()).await.unwrap() else {
        panic!("ack result")
    };
    assert!(matches!(&result,ChatInput::AckResult(t,Ok(value)) if *t==token && value.count==1));
    let effects = state.reduce(result, now);
    assert!(effects
        .iter()
        .any(|e| matches!(e, ChatEffect::LoadPage { .. })));
    let mut report = None;
    terminal
        .draw(|f| report = Some(atm_chat::render(f, &state)))
        .unwrap();
    assert!(report.unwrap().read_ids.is_empty());
    guarded(worker).await.unwrap();
    rig.stop().await;
}
// Synthetic queued Owner row exercises eligibility even for legacy/malformed data.
// Every negative has an actual processed draw before the immediate no-Ack assertion.
#[test]
fn clipped_hidden_and_undelivered_frames_do_not_ack() {
    let mut queued = incoming("an undelivered reply");
    queued["status"] = json!("queued");
    queued["delivery"]["phase"] = json!("queued");
    for (name, row, visible, last_line) in [
        ("16 KiB clipped", incoming(&"x".repeat(16384)), true, false),
        ("hidden", incoming("a hidden complete reply"), false, true),
        ("undelivered", queued, true, true),
    ] {
        let mut state = state_with_entry(row);
        let now = atm_chat::ChatTime {
            utc: "2026-01-01T00:00:00Z".parse().unwrap(),
            elapsed: Duration::ZERO,
        };
        let _ = state.reduce(
            ChatInput::Resize {
                width: 80,
                height: 12,
            },
            now,
        );
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        let mut report = None;
        terminal
            .draw(|frame| report = Some(atm_chat::render(frame, &state)))
            .unwrap();
        let report = report.unwrap();
        assert_eq!(
            report.visible_ids.contains(&"m5".parse().unwrap()),
            last_line,
            "{name}"
        );
        let effects = state.reduce(
            ChatInput::Presented {
                generation: report.generation,
                visible_ids: report.visible_ids,
                visible,
                hint: None,
            },
            now,
        );
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, ChatEffect::Ack { .. })),
            "{name}"
        );
    }
}

// Break caught: a child/stale duplicate or parsed/reconstructed ID supplies the seat's binding.
#[test]
fn session_cards_keep_only_the_unique_canonical_root_binding() {
    use atm_core::{AgentType, Model, SessionDomain, SessionStatus, SessionView};
    let mut root = SessionDomain::new(
        atm_comms::SessionId::new("opaque root:λ"),
        AgentType::GeneralPurpose,
        Model::Unknown,
    );
    root.tmux_pane = Some("%8".into());
    root.status = SessionStatus::Working;
    let mut child = root.clone();
    child.id = atm_comms::SessionId::new("child");
    child.parent_session_id = Some(root.id.clone());
    let time = "2026-01-01T00:00:00Z".parse().unwrap();
    let rows = vec![
        SessionView::from_domain_at(&child, time),
        SessionView::from_domain_at(&root, time),
    ];
    let facts = chat::decode_tree(tree(Some("%8")), Some("%7"), &rows).unwrap();
    assert_eq!(
        facts.seats[0].binding.session_id.as_deref(),
        Some("opaque root:λ")
    );
    assert!(facts.seats[0].working);
    let rows = vec![rows[0].clone()];
    let facts = chat::decode_tree(tree(Some("%8")), Some("%7"), &rows).unwrap();
    assert_eq!(facts.seats[0].binding.session_id, None);
    assert!(!facts.seats[0].working);
}

#[path = "support/agent_lifecycle.rs"]
#[allow(dead_code)] // shared rig exposes helpers for other integration suites
mod lifecycle;
#[path = "support/message_delivery.rs"]
#[allow(dead_code)] // reuse the unchanged rig, not a reduced copy
mod messages;
fn actual_report(state: &atm_chat::ChatState, width: u16, height: u16) -> FrameReport {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    let mut report = None;
    terminal
        .draw(|frame| report = Some(atm_chat::render(frame, state)))
        .unwrap();
    report.unwrap()
}

// Break caught: generation-only changes spam frame logs; canonical read/visible
// changes must still be logged, with the original run/generation/read fields.
#[test]
fn frame_trace_only_records_read_or_visible_id_set_changes() {
    if trace_case_in_child("frame_trace_only_records_read_or_visible_id_set_changes") {
        return;
    }
    let file = tempfile::NamedTempFile::new().unwrap();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(std::sync::Mutex::new(file.reopen().unwrap()))
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        let mut previous = None;
        let mut state = state_with_incoming("private trace body");
        let first = actual_report(&state, 120, 32);
        for _ in 0..61 {
            chat::trace_frame(&run_id(1), &first, &mut previous);
        }
        let _ = state.reduce(
            ChatInput::Resize {
                width: 119,
                height: 32,
            },
            atm_chat::ChatTime {
                utc: "2026-01-01T00:00:00Z".parse().unwrap(),
                elapsed: Duration::ZERO,
            },
        );
        let resized = actual_report(&state, 120, 32);
        assert_ne!(first.generation, resized.generation);
        chat::trace_frame(&run_id(1), &resized, &mut previous);
        let mut read = incoming("private trace body");
        read["read_at"] = json!("2026-01-01T00:00:01Z");
        let read = state_with_entry(read);
        chat::trace_frame(&run_id(1), &actual_report(&read, 120, 32), &mut previous);
        chat::trace_frame(&run_id(1), &actual_report(&read, 1, 1), &mut previous);
    });
    let log = std::fs::read_to_string(file.path()).unwrap();
    assert_eq!(
        log.lines()
            .filter(|line| line.contains("chat.frame "))
            .count(),
        3,
        "{log}"
    );
    assert!(
        log.lines().all(|line| line.contains("run_id=")
            && line.contains("generation=")
            && line.contains("read_ids=")),
        "{log}"
    );
    assert!(!log.contains("private trace body"), "{log}");
}

// Break caught: periodic focus checks repeat the same presentation; focus loss
// and regain with actual message IDs must still produce their changed sets.
#[test]
fn presented_trace_only_records_effectively_visible_id_set_changes() {
    if trace_case_in_child("presented_trace_only_records_effectively_visible_id_set_changes") {
        return;
    }
    let file = tempfile::NamedTempFile::new().unwrap();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(std::sync::Mutex::new(file.reopen().unwrap()))
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        let mut previous = None;
        let state = state_with_incoming("private presented body");
        let report = actual_report(&state, 120, 32);
        for _ in 0..61 {
            chat::trace_presented(&run_id(1), &report, true, &mut previous);
        }
        for _ in 0..61 {
            chat::trace_presented(&run_id(1), &report, false, &mut previous);
        }
        chat::trace_presented(&run_id(1), &report, true, &mut previous);
    });
    let log = std::fs::read_to_string(file.path()).unwrap();
    assert_eq!(
        log.lines()
            .filter(|line| line.contains("chat.presented "))
            .count(),
        3,
        "{log}"
    );
    assert!(
        log.lines().all(|line| line.contains("run_id=")
            && line.contains("message_ids=")
            && line.contains("visible=")),
        "{log}"
    );
    assert!(!log.contains("private presented body"), "{log}");
}

// Break caught: canonical no-op session updates append another idle-seat line.
#[test]
fn idle_seat_trace_only_records_binding_or_phase_changes() {
    if trace_case_in_child("idle_seat_trace_only_records_binding_or_phase_changes") {
        return;
    }
    let file = tempfile::NamedTempFile::new().unwrap();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(std::sync::Mutex::new(file.reopen().unwrap()))
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        let mut card = chat::decode_tree(tree(Some("%8")), Some("%7"), &[])
            .unwrap()
            .seats
            .remove(0);
        card.binding.session_id = Some("opaque-idle".into());
        card.activity_label = atm_core::SessionStatus::Idle.label().into();
        let mut previous = chat::SeatObservations::new();
        for _ in 0..61 {
            chat::trace_seats(&run_id(1), &[card.clone()], &mut previous);
        }
        card.working = true;
        chat::trace_seats(&run_id(1), &[card], &mut previous);
    });
    let log = std::fs::read_to_string(file.path()).unwrap();
    assert_eq!(
        log.lines()
            .filter(|line| line.contains("phase=\"idle\""))
            .count(),
        1,
        "{log}"
    );
    assert_eq!(
        log.lines()
            .filter(|line| line.contains("phase=\"working\""))
            .count(),
        1,
        "{log}"
    );
    assert!(log.lines().all(|line| line.contains("run_id=")), "{log}");
}

// Review regression: blocked/attention is not a completed Working->Idle turn.
#[test]
fn trace_attention_is_not_an_idle_turn() {
    if trace_case_in_child("trace_attention_is_not_an_idle_turn") {
        return;
    }
    let file = tempfile::NamedTempFile::new().unwrap();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(std::sync::Mutex::new(file.reopen().unwrap()))
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        let mut root = atm_core::SessionDomain::new(
            atm_comms::SessionId::new("opaque"),
            atm_core::AgentType::GeneralPurpose,
            atm_core::Model::Unknown,
        );
        root.status = atm_core::SessionStatus::AttentionNeeded;
        root.tmux_pane = Some("%8".into());
        let time = "2026-01-01T00:00:00Z".parse().unwrap();
        let view = atm_core::SessionView::from_domain_at(&root, time);
        let facts = chat::decode_tree(tree(Some("%8")), Some("%7"), &[view]).unwrap();
        chat::trace_seats(&run_id(1), &facts.seats, &mut chat::SeatObservations::new());
    });
    let log = std::fs::read_to_string(file.path()).unwrap();
    assert!(log.contains("phase=\"attention\""), "{log}");
    assert!(!log.contains("phase=\"idle\""), "{log}");
}

// Break caught: runtime watch loss erases method capabilities and masquerades
// as an unsupported envelope, instead of entering A's retry path.
#[test]
fn watch_failure_preserves_the_supported_envelope_marker_and_rpc_methods() {
    let mut methods = vec![];
    let input = chat::connection_notice(
        &run_id(1),
        atm_tui::control::ControlNotice::Capabilities {
            protocol_version: ProtocolVersion::new(1, 1),
            methods: METHODS.iter().map(|s| (*s).into()).collect(),
        },
        &mut methods,
    )
    .unwrap();
    assert!(matches!(
        input,
        ChatInput::Connection {
            online: true,
            watch_available: true,
            ..
        }
    ));
    assert!(methods.iter().any(|method| method == "watch"));
    let input = chat::connection_notice(
        &run_id(1),
        atm_tui::control::ControlNotice::WatchFailed(ControlFailure::Timeout),
        &mut methods,
    )
    .unwrap();
    let ChatInput::Connection {
        online: true,
        methods,
        watch_available: false,
    } = input
    else {
        panic!("watch failure input")
    };
    assert!(methods.iter().any(|method| method == "watch"));
    assert!(METHODS
        .iter()
        .all(|method| methods.iter().any(|m| m == method)));
    let mut old = vec![];
    chat::connection_notice(
        &run_id(1),
        atm_tui::control::ControlNotice::Capabilities {
            protocol_version: ProtocolVersion::new(1, 0),
            methods: vec!["watch".into()],
        },
        &mut old,
    )
    .unwrap();
    assert!(
        !old.iter().any(|method| method == "watch"),
        "wire method strings cannot confer envelope support"
    );
}

// Break caught: watch failures lack an actual reason or leak free-form daemon
// messages (which can contain bodies/paths) into the run-scoped trace.
#[test]
fn watch_failure_trace_records_the_real_error_class_without_free_form_content() {
    if trace_case_in_child(
        "watch_failure_trace_records_the_real_error_class_without_free_form_content",
    ) {
        return;
    }
    let file = tempfile::NamedTempFile::new().unwrap();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(std::sync::Mutex::new(file.reopen().unwrap()))
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        let mut methods = vec!["watch".into()];
        chat::connection_notice(
            &run_id(3),
            atm_tui::control::ControlNotice::WatchFailed(ControlFailure::Watch(
                atm_protocol::ResponseError {
                    code: "unavailable".into(),
                    message: "private body at /fixture/private/path".into(),
                },
            )),
            &mut methods,
        )
        .unwrap();
    });
    let log = std::fs::read_to_string(file.path()).unwrap();
    assert!(
        log.contains("reason=\"unavailable\"")
            && log.contains("live updates stopped")
            && log.contains("retrying"),
        "{log}"
    );
    assert!(
        log.lines()
            .all(|line| line.contains("run_id=2026-01-01T00:00:00+00:00-3")),
        "{log}"
    );
    assert!(
        !log.contains("private body") && !log.contains("/fixture/"),
        "{log}"
    );
}

// Review regression: observation gaps must be scoped to the running chat, not
// joined across stale session state or another appended process run.
#[test]
fn trace_connection_gaps_keep_run_scope() {
    if trace_case_in_child("trace_connection_gaps_keep_run_scope") {
        return;
    }
    let file = tempfile::NamedTempFile::new().unwrap();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(std::sync::Mutex::new(file.reopen().unwrap()))
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        chat::trace_connection(&run_id(2), false, false)
    });
    let log = std::fs::read_to_string(file.path()).unwrap();
    assert!(
        log.contains("chat.connection")
            && log.contains("run_id=2026-01-01T00:00:00+00:00-2")
            && log.contains("online=false")
            && log.contains("watch_available=false"),
        "{log}"
    );
}

#[test]
fn turn_trace_binding_keys_distinguish_rebinds_without_logging_paths() {
    if trace_case_in_child("turn_trace_binding_keys_distinguish_rebinds_without_logging_paths") {
        return;
    }
    let file = tempfile::NamedTempFile::new().unwrap();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(std::sync::Mutex::new(file.reopen().unwrap()))
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        let mut card = chat::decode_tree(tree(Some("%8")), Some("%7"), &[])
            .unwrap()
            .seats
            .remove(0);
        let mut previous = chat::SeatObservations::new();
        card.binding.session_id = Some("/fixture/opaque-session".into());
        card.working = true;
        chat::trace_seats(&run_id(1), &[card.clone()], &mut previous);
        card.working = false;
        card.activity_label = atm_core::SessionStatus::Idle.label().into();
        chat::trace_seats(&run_id(1), &[card.clone()], &mut previous);
        card.binding.pane = Some("%9".into());
        chat::trace_seats(&run_id(1), &[card.clone()], &mut previous);
        card.binding.pane = Some("%8".into());
        card.binding.session_id = Some("/fixture/new-session".into());
        chat::trace_seats(&run_id(1), &[card], &mut previous);
    });
    let log = std::fs::read_to_string(file.path()).unwrap();
    let keys: Vec<_> = log
        .lines()
        .map(|line| {
            line.split_whitespace()
                .find(|field| field.starts_with("binding_key="))
        })
        .collect();
    assert_eq!(keys.len(), 4);
    assert!(keys.iter().all(Option::is_some), "{log}");
    assert_eq!(keys[0], keys[1]);
    assert_ne!(keys[1], keys[2]);
    assert_ne!(keys[1], keys[3]);
    assert!(!log.contains("/fixture/"), "{log}");
}

#[path = "support/stand_in.rs"]
mod stand_in;

/// Forward the real protocol unchanged. Hold the first acknowledged msg.send
/// success until the test has observed its committed event, then drop only the
/// connection. All later connections pass through, exercising ControlState resume.
async fn loss_proxy(
    listener: UnixListener,
    socket: std::path::PathBuf,
    cancel: CancellationToken,
    held: std::sync::mpsc::Sender<String>,
    release: tokio::sync::oneshot::Receiver<()>,
    mut drops: mpsc::UnboundedReceiver<()>,
) {
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    };
    let lose = Arc::new(AtomicBool::new(true));
    let mut release = Some(release);
    loop {
        let accepted = tokio::select! {_=cancel.cancelled()=>return,result=guarded(listener.accept())=>result.unwrap()};
        let server = guarded(tokio::net::UnixStream::connect(&socket))
            .await
            .unwrap();
        let (client_read, mut client_write) = accepted.0.into_split();
        let (server_read, mut server_write) = server.into_split();
        let send_id = Arc::new(Mutex::new(None::<String>));
        let request_id = send_id.clone();
        let requests = async {
            let mut reader = BufReader::new(client_read);
            let mut line = String::new();
            loop {
                line.clear();
                match guarded(reader.read_line(&mut line)).await {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
                if let MessageType::Request { id, method, .. } =
                    serde_json::from_str::<atm_protocol::ClientMessage>(&line)
                        .unwrap()
                        .message
                {
                    if method == "msg.send" {
                        *request_id.lock().unwrap() = Some(id);
                    }
                }
                if guarded(server_write.write_all(line.as_bytes()))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        };
        let responses = async {
            let mut reader = BufReader::new(server_read);
            let mut line = String::new();
            loop {
                line.clear();
                match guarded(reader.read_line(&mut line)).await {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
                if let DaemonMessage::Response {
                    id,
                    payload: ResponsePayload::Result(value),
                } = serde_json::from_str::<DaemonMessage>(&line).unwrap()
                {
                    if lose.load(Ordering::SeqCst) && send_id.lock().unwrap().as_ref() == Some(&id)
                    {
                        lose.store(false, Ordering::SeqCst);
                        held.send(value["message_id"].as_str().unwrap().to_owned())
                            .unwrap();
                        if let Some(release) = release.take() {
                            let _ = guarded(release).await;
                        }
                        return;
                    }
                }
                if guarded(client_write.write_all(line.as_bytes()))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        };
        tokio::select! {_=cancel.cancelled()=>return,_=requests=>{},_=responses=>{},_=drops.recv()=>{}}
    }
}

struct DrivenChat {
    runtime: tokio::runtime::Runtime,
    task: Option<JoinHandle<Result<ratatui::buffer::Buffer, String>>>,
    cancel: CancellationToken,
    input: mpsc::UnboundedSender<Event>,
    frames: mpsc::UnboundedReceiver<FrameReport>,
    proxy: Option<JoinHandle<()>>,
    held: Option<std::sync::mpsc::Receiver<String>>,
    release: Option<tokio::sync::oneshot::Sender<()>>,
    drop_connection: Option<mpsc::UnboundedSender<()>>,
    _proxy_dir: Option<tempfile::TempDir>,
}
impl DrivenChat {
    fn start(socket: std::path::PathBuf, pane: String, target: String) -> Self {
        Self::start_inner(socket, pane, target, false)
    }
    fn start_inner(
        socket: std::path::PathBuf,
        pane: String,
        target: String,
        lose_response: bool,
    ) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let cancel = CancellationToken::new();
        let (input, events) = mpsc::unbounded_channel();
        let (frames_tx, frames) = mpsc::unbounded_channel();
        let event_tx = input.clone();
        let token = cancel.clone();
        let mut proxy = None;
        let mut held = None;
        let mut release = None;
        let mut proxy_dir = None;
        let mut drop_connection = None;
        let socket = if lose_response {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("proxy.sock");
            let listener = {
                let _entered = runtime.enter();
                UnixListener::bind(&path).unwrap()
            };
            let (held_tx, held_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = tokio::sync::oneshot::channel();
            let (drop_tx, drop_rx) = mpsc::unbounded_channel();
            drop_connection = Some(drop_tx);
            proxy = Some(runtime.spawn(loss_proxy(
                listener,
                socket,
                cancel.clone(),
                held_tx,
                release_rx,
                drop_rx,
            )));
            held = Some(held_rx);
            release = Some(release_tx);
            proxy_dir = Some(root);
            path
        } else {
            socket
        };
        let task = runtime.spawn(async move {
            let (commands, command_rx) = mpsc::unbounded_channel();
            let control = ControlHandle::new(commands);
            let client = DaemonClient::new(
                DaemonConfig {
                    socket_path: socket,
                    ..Default::default()
                },
                event_tx,
                command_rx,
                token.clone(),
            );
            let client = tokio::spawn(async move { client.run().await });
            let tmux = atm_tmux::MockTmuxClient::new();
            // Controlled focus sink: finite test input, not a live tmux/client claim.
            for _ in 0..10_000 {
                tmux.set_next_raw_output(&format!("attached,focused,UTF-8\t{pane}\t0\t1\n"));
            }
            let config = ChatConfig {
                initial_thread: Some(target.parse().unwrap()),
                caller_pane: Some(pane),
            };
            let mut terminal = Terminal::new(TestBackend::new(120, 32)).unwrap();
            let result = chat::drive(
                &mut terminal,
                config,
                control,
                events,
                &tmux,
                token.clone(),
                run_id(41),
                move |frame| {
                    let _ = frames_tx.send(frame.clone());
                },
            )
            .await;
            token.cancel();
            guarded(client).await.map_err(|error| error.to_string())?;
            result.map_err(|error| error.to_string())?;
            Ok(terminal.backend().buffer().clone())
        });
        Self {
            runtime,
            task: Some(task),
            cancel,
            input,
            frames,
            proxy,
            held,
            release,
            drop_connection,
            _proxy_dir: proxy_dir,
        }
    }
    fn frame(&mut self, predicate: impl Fn(&FrameReport) -> bool) -> FrameReport {
        self.runtime
            .block_on(matching_frame(&mut self.frames, predicate))
    }
    fn key(&self, code: KeyCode) {
        self.input
            .send(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)))
            .unwrap();
    }
    fn finish(&mut self) -> ratatui::buffer::Buffer {
        self.key(KeyCode::Esc);
        self.key(KeyCode::Char('q'));
        let task = self.task.take().unwrap();
        let buffer = self.runtime.block_on(guarded(task)).unwrap().unwrap();
        if let Some(proxy) = self.proxy.take() {
            self.runtime.block_on(guarded(proxy)).unwrap();
        }
        buffer
    }
}
impl Drop for DrivenChat {
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Some(mut task) = self.task.take() {
            self.runtime.block_on(async {
                if tokio::time::timeout(GUARD, &mut task).await.is_err() {
                    task.abort();
                    let _ = tokio::time::timeout(GUARD, task).await;
                }
            });
        }
        if let Some(proxy) = self.proxy.take() {
            let _ = self.runtime.block_on(guarded(proxy));
        }
        // Runtime drop aborts remaining futures and joins its owned worker
        // threads even when the test unwinds; this helper creates no std thread.
    }
}
fn read_about(id: &str) -> impl Fn(&Value) -> bool {
    let id = id.to_owned();
    move |event| {
        event["kind"] == "msg.read"
            && event["body"]["message_ids"]
                .as_array()
                .is_some_and(|ids| ids.iter().any(|value| value == id.as_str()))
    }
}
fn buffer_text(buffer: &ratatui::buffer::Buffer) -> String {
    buffer
        .content
        .chunks(usize::from(buffer.area.width))
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

// Real private daemon, canonical reference delivery, bound CLI reply, actual
// controller/draw/focus/Ack and committed Watch. No model executes submitted text.
#[test]
fn owner_talk_reference_reply_seen_once() {
    let mut rig = messages::MessageRig::start(messages::Confirmation::Generous);
    let chief = rig.ready_agent("chief", stand_in::StandInMode::Pi);
    let pane = lifecycle::checked(rig.test.rig.tmux(&[
        "new-window",
        "-d",
        "-n",
        "chat",
        "-P",
        "-F",
        "#{pane_id}",
        "/bin/sh",
    ]));
    let mut watch = rig.watch();
    let mut chat = DrivenChat::start_inner(rig.test.rig.socket(), pane, chief.id.clone(), true);
    chat.frame(|frame| frame.can_compose);
    let cursor = rig.test.rig.event_cursor(&chief.pane);
    let body = "Please read the durable fixture note.";
    chat.key(KeyCode::Char('i'));
    for c in body.chars() {
        chat.key(KeyCode::Char(c));
    }
    chat.key(KeyCode::Enter);
    let sent = watch.wait("Owner chat send", |e| {
        e["kind"] == "msg.sent"
            && e["body"]["from"] == "owner"
            && e["body"]["to"] == chief.id
            && e["body"]["body"] == body
    });
    eprintln!("real msg.sent envelope: {sent}");
    let id = atm_comms::MessageId::new(sent["seq"].as_u64().unwrap()).to_string();
    assert_eq!(
        chat.held
            .as_ref()
            .unwrap()
            .recv_timeout(GUARD)
            .expect("committed response barrier"),
        id
    );
    let locked = chat.frame(|frame| !frame.can_compose);
    chat.release.take().unwrap().send(()).unwrap(); // lose a real committed response, not an invented timeout
    chat.frame(|frame| frame.generation > locked.generation && frame.can_compose);
    rig.test.rig.wait_event(&chief.pane, cursor, |event| {
        event["kind"] == "accepted" && event["text"] == messages::reference(&id)
    });
    watch.wait("reference delivered", messages::about("msg.delivered", &id));
    assert_eq!(
        rig.accepted_since(&chief.pane, cursor),
        [messages::reference(&id)]
    );
    let fetched = rig.inbox_entry(Some(&chief.pane), None, &id);
    assert_eq!(fetched["content"]["body"], body);
    assert!(rig
        .atm(Some(&chief.pane), &["msg", "ack", &id])
        .status
        .success());
    let reply_body = "A durable reply from the chief.";
    let reply = rig.atm_json(
        Some(&chief.pane),
        &[
            "msg", "send", "--to", "owner", "--kind", "answer", "--body", reply_body, "--format",
            "json",
        ],
    );
    let reply = reply["message_id"].as_str().unwrap().to_owned();
    let reply_id = reply.parse().unwrap();
    chat.frame(|frame| frame.visible_ids.contains(&reply_id));
    watch.wait("reply read committed", read_about(&reply));
    let read_frame = chat.frame(|frame| frame.read_ids.contains(&reply_id));
    // Distinct generations fence the actual resize and then a second real
    // reconnect AFTER reading; cached pre-action frames cannot satisfy them.
    chat.input.send(Event::Resize(119, 32)).unwrap();
    let redraw = chat.frame(|frame| {
        frame.generation > read_frame.generation && frame.read_ids.contains(&reply_id)
    });
    chat.drop_connection.as_ref().unwrap().send(()).unwrap();
    let offline = chat.frame(|frame| frame.generation > redraw.generation && !frame.can_compose);
    chat.frame(|frame| {
        frame.generation > offline.generation
            && frame.can_compose
            && frame.read_ids.contains(&reply_id)
    });
    watch.catch_up(&rig);
    assert_eq!(watch.all(read_about(&reply)).len(), 1);
    let history = rig
        .call(
            "msg.history",
            json!({"agent":chief.id,"correspondent":"owner","limit":100}),
            None,
        )
        .unwrap();
    let rows = history["messages"].as_array().unwrap();
    assert_eq!(
        rows.iter().filter(|row| row["message_id"] == reply).count(),
        1
    );
    assert_eq!(
        rows.iter()
            .filter(|row| row["content"]["from"] == "owner" && row["content"]["body"] == body)
            .count(),
        1
    );
    let screen = buffer_text(&chat.finish());
    assert!(
        screen.contains(reply_body) && screen.contains("seen (read)"),
        "{screen}"
    );
    assert_eq!(screen.matches(reply_body).count(), 1, "{screen}");
    drop(chat);
    rig.finish_turn(&chief.pane);
    rig.test.rig.close();
}

// The same runtime/client/controller ownership must also close on deliberate unwind.
#[test]
fn chat_runtime_is_reaped_on_deliberate_unwind() {
    let mut rig = messages::MessageRig::start(messages::Confirmation::Generous);
    let chief = rig.ready_agent("chief", stand_in::StandInMode::Pi);
    let pane = lifecycle::checked(rig.test.rig.tmux(&[
        "new-window",
        "-d",
        "-n",
        "chat",
        "-P",
        "-F",
        "#{pane_id}",
        "/bin/sh",
    ]));
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut chat = DrivenChat::start(rig.test.rig.socket(), pane, chief.id.clone());
        chat.frame(|frame| frame.can_compose);
        panic!("deliberate chat owner unwind");
    }));
    assert!(outcome.is_err());
    rig.test.rig.close();
}
