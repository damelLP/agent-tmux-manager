//! Private real-binary persistence/startup acceptance tests.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use atm_protocol::ClientMessage;
use atmd::eventlog::{read_from, Envelope, EventLog, Stream};
use chrono::Utc;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

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

    fn state(&self) -> PathBuf {
        self.dir.path().join("relative-state")
    }

    fn pid_file(&self) -> PathBuf {
        self.state().join("atmd.pid")
    }

    fn start(&mut self, daemonized: bool) {
        let mut command = Command::new(env!("CARGO_BIN_EXE_atmd"));
        command.arg("start");
        if daemonized {
            command.arg("-d");
        }
        let log_dir = self.dir.path().join("xdg/atm");
        std::fs::create_dir_all(&log_dir).unwrap();
        let changes = directory_changes(&log_dir);
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
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        if daemonized {
            let mut child = self.child.take().unwrap();
            assert!(blocking(move || child.wait().unwrap()).success());
            let log = log_dir.join("atm.log");
            blocking(move || loop {
                if std::fs::read_to_string(&log)
                    .unwrap_or_default()
                    .contains("Daemon server listening")
                {
                    break;
                }
                readable(&changes);
                let mut bytes = [0u8; 4096];
                assert!(
                    unsafe {
                        libc::read(changes.as_raw_fd(), bytes.as_mut_ptr().cast(), bytes.len())
                    } > 0
                );
            });
        } else {
            let output = self.child.as_mut().unwrap().stdout.take().unwrap();
            let (ready, receive) = std::sync::mpsc::channel();
            std::thread::Builder::new()
                .spawn(move || {
                    let mut ready = Some(ready);
                    for line in BufReader::new(output).lines() {
                        let Ok(line) = line else {
                            break;
                        };
                        if line.contains("Daemon server listening") {
                            if let Some(ready) = ready.take() {
                                let _ = ready.send(());
                            }
                        }
                    }
                })
                .unwrap();
            receive.recv_timeout(Duration::from_secs(60)).unwrap();
        }
    }

    fn info(&self) -> Value {
        let mut stream = UnixStream::connect(self.socket()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(60)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(60)))
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
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
            assert!(fd >= 0);
            let exited = unsafe { std::fs::File::from_raw_fd(fd as i32) };
            assert!(self.owns(pid), "daemon identity changed before signaling");
            assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGTERM) }, 0);
            readable(&exited); // kernel process-exit notification, including daemonized children
            assert!(!self.pid_file().exists());
        }
        if let Some(mut child) = self.child.take() {
            if child.try_wait().unwrap().is_none() && self.owns(child.id()) {
                // Covers a failed foreground startup before the PID file exists.
                unsafe {
                    libc::kill(child.id() as i32, libc::SIGTERM);
                }
            }
            blocking(move || child.wait().unwrap());
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Client {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
}

impl Client {
    fn connect(daemon: &Daemon) -> Self {
        let stream = UnixStream::connect(daemon.socket()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(60)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(60)))
            .unwrap();
        let reader = BufReader::new(stream.try_clone().unwrap());
        let mut client = Self { stream, reader };
        client.send(ClientMessage::connect(None));
        let connected = client.recv();
        assert_eq!(connected["type"], "connected");
        assert!(connected["methods"]
            .as_array()
            .unwrap()
            .contains(&json!("log.tail")));
        client
    }

    fn send(&mut self, message: ClientMessage) {
        writeln!(self.stream, "{}", serde_json::to_string(&message).unwrap()).unwrap();
    }

    fn recv(&mut self) -> Value {
        let mut line = String::new();
        assert_ne!(
            self.reader.read_line(&mut line).unwrap(),
            0,
            "unexpected daemon EOF"
        );
        serde_json::from_str(&line).unwrap()
    }

    fn tail(&mut self, after: u64) -> Vec<Envelope> {
        self.send(ClientMessage::request(
            "tail".into(),
            "log.tail".into(),
            json!({"after_seq":after,"limit":1000}),
            None,
        ));
        let response = self.recv();
        assert!(response.get("error").is_none(), "{response}");
        serde_json::from_value(response["result"].clone()).unwrap()
    }

    fn replay(&mut self, after: u64, expected: &[Envelope]) {
        self.send(ClientMessage::watch(vec![], after));
        for envelope in expected {
            let event = self.recv();
            assert_eq!(event["type"], "event");
            assert_eq!(event["seq"], envelope.seq);
            assert_eq!(event["kind"], envelope.kind);
            assert_eq!(event["body"], envelope.body);
            assert_eq!(event["ts"], json!(envelope.ts));
        }
        // Unwatch followed by pong is the boundary, not the earlier tail.
        // A coalesced summary may commit in between; accept only contiguous
        // new events, so duplicates or holes still fail before the sentinel.
        self.send(ClientMessage::unwatch());
        self.send(ClientMessage::ping(42));
        let mut cursor = expected.last().map_or(after, |record| record.seq);
        loop {
            let frame = self.recv();
            if frame == json!({"type":"pong","seq":42}) {
                break;
            }
            assert_eq!(frame["type"], "event");
            assert_eq!(frame["seq"], cursor + 1);
            cursor += 1;
        }
    }

    fn captured(&mut self, fixture: &str, finish: bool) -> String {
        let mut id = String::new();
        for line in fixture.lines() {
            let mut frame: Value = serde_json::from_str(line).unwrap();
            let data = &mut frame["data"];
            if !finish && data["event"] == "session_shutdown" {
                continue;
            }
            data["pid"] = json!(std::process::id()); // live PID owned by this test
            id = data["session_id"].as_str().unwrap().to_owned();
            self.send(serde_json::from_value(frame).unwrap());
        }
        id
    }

    fn assert_live_cost(&mut self, id: &str) {
        self.send(ClientMessage::list_sessions());
        let rows = self.recv();
        let row = rows["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == id)
            .unwrap();
        assert_eq!(row["cost_usd"], 0.01065);
        assert_eq!(row["first_prompt"], "Read note.txt and reply.");
        self.send(ClientMessage::ping(7));
        assert_eq!(self.recv(), json!({"type":"pong","seq":7}));
    }
}

const COST_CAPTURE: &str =
    include_str!("../crates/atm-pi-adapter/tests/fixtures/pi_session_cost_0.73.1.jsonl");
const CLEAR_CAPTURE: &str =
    include_str!("../crates/atm-pi-adapter/tests/fixtures/pi_new_session_0.73.1.jsonl");

fn wait_for_log_close(daemon: &Daemon) {
    // Drop of EventLog doesn't join its worker; wait for its actual lock release
    // before starting a daemon. This never opens/retries another log worker.
    let file = std::fs::File::open(daemon.state().join("log/events/.lock")).unwrap();
    blocking(move || {
        assert_eq!(unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) }, 0);
        drop(file);
    });
}

fn seed(daemon: &Daemon) -> Vec<Envelope> {
    let log = EventLog::open(&daemon.state(), Stream::Events).unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        for (kind, body) in [
            ("project.registered", json!({"project_id":"demo","root":"/fixture"})),
            ("agent.adopted", json!({"agent_id":"demo/lead","pane":"%0","session_id":"pre-clear"})),
            ("msg.sent", json!({"from":"owner","to":"demo/lead","kind":"brief","body":"fixture task","attachments":[],"task":null})),
            ("msg.sent", json!({"from":"demo/lead","to":"owner","kind":"report","body":"fixture report","attachments":[],"task":null})),
            ("msg.read", json!({"message_ids":["m4"]})),
            ("agent.clear_requested", json!({"agent_id":"demo/lead"})),
            ("agent.editor_ready", json!({"agent_id":"demo/lead"})),
        ] {
            log.append(Utc::now(), kind, body).await.unwrap();
        }
    });
    drop(log);
    wait_for_log_close(daemon);
    read_from(&daemon.state(), Stream::Events, 0)
        .map(Result::unwrap)
        .collect()
}

fn latest<'a>(history: &'a [Envelope], id: &str) -> &'a Value {
    &history
        .iter()
        .rev()
        .find(|event| event.kind == "session.summary" && event.body["session_id"] == id)
        .unwrap()
        .body
}

fn blocking<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    let (send, receive) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .spawn(move || {
            let _ = send.send(work());
        })
        .unwrap();
    receive.recv_timeout(Duration::from_secs(60)).unwrap()
}

fn readable(file: &std::fs::File) {
    let mut fd = libc::pollfd {
        fd: file.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    assert_eq!(
        unsafe { libc::poll(&mut fd, 1, 60_000) },
        1,
        "kernel event did not arrive"
    );
    assert_ne!(fd.revents & libc::POLLIN, 0);
}

fn directory_changes(path: &std::path::Path) -> std::fs::File {
    let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK) };
    assert!(fd >= 0);
    let file = unsafe { std::fs::File::from_raw_fd(fd) };
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    assert!(
        unsafe {
            libc::inotify_add_watch(
                fd,
                path.as_ptr(),
                libc::IN_MODIFY | libc::IN_CREATE | libc::IN_MOVED_TO,
            )
        } >= 0
    );
    file
}

#[test]
fn real_daemon_restart_keeps_history_cost_prompt_and_org_state() {
    for finish in [false, true] {
        let mut daemon = Daemon::new();
        let seeded = seed(&daemon);
        daemon.start(false);
        let mut client = Client::connect(&daemon);
        assert_eq!(client.tail(0), seeded);
        client.replay(0, &seeded);
        let id = client.captured(COST_CAPTURE, finish);
        if !finish {
            client.assert_live_cost(&id);
        }
        let mut events = Client::connect(&daemon);
        events.send(ClientMessage::watch(vec!["session".into()], 0));
        loop {
            let event = events.recv();
            assert_eq!(event["type"], "event");
            let body = &event["body"];
            if body["session_id"] == id
                && body["first_prompt"] == "Read note.txt and reply."
                && (!finish || (body["cost"]["microdollars"] == 10650 && !body["ended"].is_null()))
            {
                break;
            }
        }
        drop(events);
        let saved = client.tail(0);
        let cursor = saved.last().unwrap().seq;
        client.replay(0, &saved);
        drop(client);
        daemon.stop();
        // On clean shutdown an ACTIVE session's sub-threshold cost is flushed
        // exactly; no fabricated session-end event accompanies daemon exit.
        let durable: Vec<_> = read_from(&daemon.state(), Stream::Events, 0)
            .map(Result::unwrap)
            .collect();
        assert!(durable.starts_with(&saved));
        let summary = latest(&durable, &id);
        assert_eq!(summary["cost"], json!({"microdollars":10650}));
        assert_eq!(summary["first_prompt"], "Read note.txt and reply.");
        assert_eq!(summary["agent_id"], "demo/lead");
        assert_eq!(!summary["ended"].is_null(), finish);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let before = runtime.block_on(async {
            let cancel = CancellationToken::new();
            let org = atmd::org::OrgHandle::open(daemon.state(), cancel.clone()).await;
            let state = org.snapshot().await.unwrap();
            cancel.cancel();
            state
        });
        wait_for_log_close(&daemon);
        assert_eq!(before.projects().count(), 1);
        assert_eq!(before.agents().count(), 1);
        let agent = before.agent(&"demo/lead".parse().unwrap()).unwrap();
        assert_eq!(json!(agent.status), "ready");
        assert!(agent.session_id.is_none());
        assert_eq!(
            agent
                .excluded_session_id
                .as_ref()
                .map(atm_core::SessionId::as_str),
            Some("pre-clear")
        );
        assert_eq!(
            before.inbox(&"demo/lead".parse().unwrap())[0].content.body,
            "fixture task"
        );
        assert!(before.inbox(&"owner".parse().unwrap())[0].read_at.is_some());
        daemon.start(false);
        let mut client = Client::connect(&daemon);
        assert_eq!(client.tail(0), durable);
        client.replay(0, &durable);
        let suffix: Vec<_> = durable
            .iter()
            .filter(|row| row.seq > cursor)
            .cloned()
            .collect();
        client.replay(cursor, &suffix);
        drop(client);
        daemon.stop();
        let after = runtime.block_on(async {
            let cancel = CancellationToken::new();
            let org = atmd::org::OrgHandle::open(daemon.state(), cancel.clone()).await;
            let state = org.snapshot().await.unwrap();
            cancel.cancel();
            state
        });
        wait_for_log_close(&daemon);
        assert_eq!(before, after);
    }
}

#[test]
fn real_daemon_resume_defaults_do_not_erase_first_prompt_start_or_cost() {
    let mut daemon = Daemon::new();
    daemon.start(false);
    let mut client = Client::connect(&daemon);
    let id = client.captured(COST_CAPTURE, false);
    client.assert_live_cost(&id);
    drop(client);
    daemon.stop();
    daemon.start(false);
    let mut client = Client::connect(&daemon);
    let before = client.tail(0);
    let summary = latest(&before, &id).clone();
    // Captured startup/input with a different resumed prompt; no new usage yet.
    // The registry's initial zero is not a correction to the recovered cost.
    for line in COST_CAPTURE.lines().take(2) {
        let mut frame: Value = serde_json::from_str(line).unwrap();
        frame["data"]["pid"] = json!(std::process::id());
        if frame["data"]["event"] == "input" {
            frame["data"]["payload"]["text"] = json!("Later resumed prompt.");
        }
        client.send(serde_json::from_value(frame).unwrap());
    }
    client.send(ClientMessage::list_sessions());
    let live = client.recv();
    let row = live["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == id)
        .unwrap();
    assert_eq!(row["first_prompt"], "Later resumed prompt.");
    assert_eq!(row["cost_usd"], 0.0);
    drop(client);
    daemon.stop(); // flush the resumed row, not merely a ping barrier
    daemon.start(false);
    let mut client = Client::connect(&daemon);
    let after = client.tail(0);
    assert_eq!(latest(&after, &id), &summary);
    assert_eq!(summary["cost"]["microdollars"], 10650);
    assert_eq!(summary["first_prompt"], "Read note.txt and reply.");
    drop(client);
    daemon.stop();
}

#[test]
fn real_daemon_clear_and_end_summaries_survive_restart() {
    let mut daemon = Daemon::new();
    daemon.start(false);
    let mut client = Client::connect(&daemon);
    client.captured(CLEAR_CAPTURE, true);
    let ids: std::collections::BTreeSet<_> = CLEAR_CAPTURE
        .lines()
        .map(|line| {
            serde_json::from_str::<Value>(line).unwrap()["data"]["session_id"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(ids.len(), 2);
    let mut events = Client::connect(&daemon);
    events.send(ClientMessage::watch(vec!["session".into()], 0));
    let mut ended = std::collections::BTreeSet::new();
    while ended != ids {
        let event = events.recv();
        assert_eq!(event["type"], "event");
        if !event["body"]["ended"].is_null() {
            let id = event["body"]["session_id"].as_str().unwrap().to_owned();
            assert!(ids.contains(&id));
            ended.insert(id);
        }
    }
    drop(events);
    let saved = client.tail(0);
    client.replay(0, &saved);
    drop(client);
    daemon.stop();
    daemon.start(false);
    let mut client = Client::connect(&daemon);
    assert_eq!(client.tail(0), saved);
    client.replay(0, &saved);
    for id in ids {
        assert!(!latest(&saved, &id)["ended"].is_null());
    }
    drop(client);
    daemon.stop();
}

#[test]
fn real_daemon_bad_newer_or_locked_events_keep_session_tracking_usable() {
    for fault in ["corrupt-old", "invalid-body", "newer-format", "locked"] {
        let mut daemon = Daemon::new();
        let events = daemon.state().join("log/events");
        std::fs::create_dir_all(&events).unwrap();
        let lock = if fault == "locked" {
            Some(EventLog::open(&daemon.state(), Stream::Events).unwrap())
        } else {
            None
        };
        let expected = match fault {
            "corrupt-old" | "invalid-body" => {
                let old = if fault == "corrupt-old" {
                    "not json\n".to_owned()
                } else {
                    format!(
                        "{}\n",
                        json!({"seq":1,"ts":"2023-11-14T22:13:20Z","kind":"agent.down","body":{"agent_id":"absent/a","reason":"gone"}})
                    )
                };
                std::fs::write(events.join("00000001.jsonl"), old).unwrap();
                std::fs::write(
                    events.join("00000002.jsonl"),
                    format!(
                        "{}\n",
                        json!({"seq":2,"ts":"2023-11-14T22:13:20Z","kind":"future.kind","body":{}})
                    ),
                )
                .unwrap();
                if fault == "corrupt-old" {
                    "line 1"
                } else {
                    "events replay at sequence 1"
                }
            }
            "newer-format" => {
                std::fs::write(daemon.state().join("log/format"), "2\n").unwrap();
                "unsupported log format"
            }
            "locked" => "locked",
            _ => unreachable!(),
        };
        daemon.start(false);
        let mut client = Client::connect(&daemon);
        client.send(ClientMessage::request(
            "tail".into(),
            "log.tail".into(),
            json!({}),
            None,
        ));
        let error = client.recv();
        assert_eq!(error["error"]["code"], "unavailable", "{fault}: {error}");
        assert!(
            error["error"]["message"]
                .as_str()
                .unwrap()
                .contains(expected),
            "{fault}: {error}"
        );
        client.send(ClientMessage::watch(vec![], 0));
        let error = client.recv();
        assert_eq!(error["type"], "error");
        assert_eq!(error["code"], "unavailable");
        assert!(error.get("id").is_none());
        let id = client.captured(COST_CAPTURE, false);
        client.assert_live_cost(&id);
        drop(client);
        daemon.stop();
        drop(lock);
    }
}

#[test]
fn real_daemon_failed_raw_open_does_not_disable_summaries_or_sessions() {
    let mut daemon = Daemon::new();
    std::fs::create_dir_all(daemon.state().join("log")).unwrap();
    std::fs::write(daemon.state().join("log/raw"), "not a directory").unwrap();
    daemon.start(false);
    let mut client = Client::connect(&daemon);
    let id = client.captured(COST_CAPTURE, false);
    client.assert_live_cost(&id);
    drop(client);
    daemon.stop();
    daemon.start(false);
    let mut client = Client::connect(&daemon);
    let saved = client.tail(0);
    assert_eq!(latest(&saved, &id)["cost"]["microdollars"], 10650);
    assert_eq!(
        latest(&saved, &id)["first_prompt"],
        "Read note.txt and reply."
    );
    assert_eq!(
        std::fs::read_to_string(daemon.state().join("log/raw")).unwrap(),
        "not a directory"
    );
    drop(client);
    daemon.stop();
}

#[test]
fn info_resolves_relative_state_dir_before_foreground_or_daemonized_start() {
    for daemonized in [false, true] {
        let mut daemon = Daemon::new();
        daemon.start(daemonized);
        let info = daemon.info();
        let expected = daemon.dir.path().join("relative-state");
        assert_eq!(info["result"]["state_dir"], json!(expected));
        assert!(expected.join("log/events/00000001.jsonl").is_file());
        assert!(expected.join("log/raw/00000001.jsonl").is_file());
        // Relative overrides keep PID/log paths beside the event log even
        // after daemonize changes cwd to /; legacy XDG state stays untouched.
        assert!(daemon.pid_file().exists());
        assert!(!daemon.dir.path().join("xdg/atm").exists());
        if daemonized {
            assert!(expected.join("atm.log").is_file());
        }
        daemon.stop();
    }
}
