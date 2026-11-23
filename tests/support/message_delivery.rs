//! Message delivery over the shared stand-in rig and the agent lifecycle rig:
//! a private daemon with a `[delivery]` config, stand-ins adopted as agents,
//! the real `atm msg`/`atm inbox` CLI, and committed events through `watch`.
//!
//! Delivery cases use a real private daemon. The scripted peer only checks
//! CLI request/response contracts. Waits use committed events (`Watch`),
//! stand-in events after a cursor taken before the action,
//! RPC answers, registered daemon-log writes, or private tmux hooks.
//! Scheduler-only presentation polls remain explicitly deferred.
//! "Nothing was typed" needs a fence showing the daemon decided: a committed
//! event, or the scheduler's return after a refused attempt. The lifecycle
//! rig checks the journal together with a styled screen comparison.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, Output, Stdio};
use std::time::Duration;

use atm_core::SessionStatus;
use atm_protocol::{ClientMessage, DaemonMessage, ResponsePayload};
use serde_json::{json, Value};

use crate::lifecycle::{self, checked, AgentRig};
use crate::stand_in::{self, StandInMode, StandInPane, StandInRig};

/// A hang guard for one awaited event, never a delay.
const GUARD: Duration = Duration::from_secs(60);
/// The fixture turn of `lifecycle::config`, in model time.
const TURN_MS: u64 = 500;

/// The confirmation window a test runs with. Only the stable-idle,
/// stable-empty and retry delays are ever shortened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confirmation {
    /// 20 s: a receipt arrives at any hook latency under load.
    Generous,
    /// The daemon's own default, for tests whose stand-in drops the hook: the
    /// receipt cannot come, so the window is not a race.
    Default,
}

/// The one line a recipient is given; the body never enters a terminal.
pub fn reference(id: &str) -> String {
    format!("[{id}] Read: atm inbox --id {id}")
}

/// A committed message event of this kind about this message.
pub fn about(kind: &'static str, id: &str) -> impl Fn(&Value) -> bool {
    let id = id.to_owned();
    move |event| event["kind"] == kind && event["body"]["message_id"] == id.as_str()
}

/// A notice the daemon wrote to the owner whose text starts like this.
pub fn notice(start: &str) -> impl Fn(&Value) -> bool {
    let start = start.to_owned();
    move |event| {
        event["kind"] == "msg.sent"
            && event["body"]["from"] == "system"
            && event["body"]["to"] == "owner"
            && event["body"]["body"]
                .as_str()
                .is_some_and(|body| body.starts_with(&start))
    }
}

/// One method call on a connection of its own, usable from another thread
/// while the test holds the rig. An error answer is `(code, message)`.
pub fn call_on(
    socket: &Path,
    method: &str,
    params: Value,
    from_pane: Option<String>,
) -> Result<Value, (String, String)> {
    let response = stand_in::protocol_request(
        socket,
        ClientMessage::request("message-rig".into(), method.into(), params, from_pane),
        // Above the router's highest ceiling (agent.clear, 135 s).
        Duration::from_secs(150),
    )
    .unwrap_or_else(|error| panic!("{method}: {error}"));
    match response {
        DaemonMessage::Response {
            payload: ResponsePayload::Result(value),
            ..
        } => Ok(value),
        DaemonMessage::Response {
            payload: ResponsePayload::Error(error),
            ..
        } => Err((error.code, error.message)),
        other => panic!("{method}: {other:?}"),
    }
}

/// One expected CLI request or a handshake that omits its capability.
#[allow(dead_code)] // Only the CLI contract test binary uses the scripted peer.
pub enum CliStep {
    Response {
        method: &'static str,
        params: Value,
        payload: Value,
    },
    TooOld,
}

/// Exercise the real CLI against literal replies on a private socket.
/// The caller supplies a sealed child with the fallback sentinel from task_cli.
#[allow(dead_code)] // Other test binaries use only the real private rig.
pub async fn run_cli_script(
    dir: &Path,
    command: tokio::process::Command,
    pane: Option<&str>,
    steps: Vec<CliStep>,
) -> Output {
    run_cli_script_with_methods(dir, command, pane, steps, None).await
}

/// When specified, advertise the same capability list on every handshake.
#[allow(dead_code)]
pub async fn run_cli_script_with_methods(
    dir: &Path,
    mut command: tokio::process::Command,
    pane: Option<&str>,
    steps: Vec<CliStep>,
    advertised: Option<&[&str]>,
) -> Output {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;
    use tokio::sync::watch;

    let listener = UnixListener::bind(dir.join("rpc.sock")).unwrap();
    let (exit, mut exited) = watch::channel(false);
    let child = async {
        let output = command.output().await.unwrap();
        exit.send_replace(true);
        output
    };
    let peer = async {
        for step in steps {
            let (mut read, mut write) = loop {
                let (stream, _) = tokio::select! {
                    biased;
                    stream = listener.accept() => stream.unwrap(),
                    _ = exited.wait_for(|done| *done) => return false,
                };
                let (read, write) = stream.into_split();
                let mut read = BufReader::new(read);
                let mut line = String::new();
                // The client's readiness probe opens and closes without Connect.
                if read.read_line(&mut line).await.unwrap() == 0 {
                    continue;
                }
                assert_eq!(
                    serde_json::from_str::<Value>(&line).unwrap()["type"],
                    "connect"
                );
                break (read, write);
            };
            let methods = match (advertised, &step) {
                (Some(methods), _) => methods,
                (None, CliStep::Response { method, .. }) => std::slice::from_ref(method),
                (None, CliStep::TooOld) => &[],
            };
            let hello = json!({"type":"connected","protocol_version":{"major":1,"minor":1},"client_id":"cli-test","methods":methods});
            write
                .write_all(format!("{hello}\n").as_bytes())
                .await
                .unwrap();
            let mut line = String::new();
            if let CliStep::Response {
                method,
                params,
                mut payload,
            } = step
            {
                assert_ne!(
                    read.read_line(&mut line).await.unwrap(),
                    0,
                    "missing request for {method}"
                );
                let request: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(request["type"], "request");
                assert_eq!(request["method"], method);
                assert_eq!(request["params"], params);
                assert_eq!(request.get("from_pane").and_then(Value::as_str), pane);
                payload["type"] = json!("response");
                payload["id"] = request["id"].clone();
                write
                    .write_all(format!("{payload}\n").as_bytes())
                    .await
                    .unwrap();
                line.clear();
            }
            assert_eq!(
                read.read_line(&mut line).await.unwrap(),
                0,
                "unexpected request: {line}"
            );
        }
        tokio::select! {
            biased;
            extra = listener.accept() => panic!("unexpected connection after script: {extra:?}"),
            _ = exited.wait_for(|done| *done) => {}
        }
        true
    };
    let (output, consumed) = tokio::time::timeout(GUARD, async { tokio::join!(child, peer) })
        .await
        .expect("CLI and its scripted peer must both finish");
    assert!(
        consumed,
        "CLI exited before sending all expected requests: {output:?}"
    );
    assert!(
        !dir.join("fallback").exists(),
        "CLI tried to spawn atmd or tmux"
    );
    output
}

/// An adopted, Ready agent and the stand-in behind it.
pub struct Recipient {
    pub pane: StandInPane,
    pub id: String,
    /// The session the agent was adopted with; `None` for a lazy Codex.
    pub session: Option<String>,
}

pub struct MessageRig {
    pub test: AgentRig,
}

impl MessageRig {
    pub fn start(confirmation: Confirmation) -> Self {
        let mut rig = stand_in::StandInRig::start();
        let config = rig.root().join("config/atm");
        std::fs::create_dir_all(&config).unwrap();
        let mut settings = String::from(
            "[delivery]\n\
             stable_idle = { secs = 0, nanos = 200000000 }\n\
             stable_empty_input = { secs = 0, nanos = 200000000 }\n\
             retry_delays = [{ secs = 0, nanos = 200000000 }, { secs = 0, nanos = 400000000 }, \
             { secs = 0, nanos = 800000000 }]\n",
        );
        if confirmation == Confirmation::Generous {
            settings.push_str("confirmation_timeout = { secs = 20, nanos = 0 }\n");
        }
        std::fs::write(config.join("config.toml"), settings).unwrap();
        // The daemon reads its config once, at start.
        rig.restart_daemon();
        let project = rig.root().join("project");
        std::fs::create_dir(&project).unwrap();
        let rig = Self {
            test: AgentRig { rig, project },
        };
        // agent.list answers after the first reconciliation pass.
        assert_eq!(rig.test.rpc("agent.list", json!({}), None), json!([]));
        rig.assert_delivery_config_in_force();
        rig
    }

    /// Fails closed: the settings above count only if this daemon read them.
    /// A missing file would silently mean the defaults, an invalid one a
    /// disabled service.
    pub fn assert_delivery_config_in_force(&self) {
        let rig = &self.test.rig;
        let pids = rig.daemon_pids();
        assert_eq!(pids.len(), 1, "one private daemon: {pids:?}");
        let environ = std::fs::read(format!("/proc/{}/environ", pids[0])).unwrap();
        let config = rig.root().join("config");
        let wanted = format!("XDG_CONFIG_HOME={}", config.display());
        assert!(
            environ.split(|b| *b == 0).any(|e| e == wanted.as_bytes()),
            "the daemon's config home is not the rig's"
        );
        assert!(config.join("atm/config.toml").is_file());
        let log = self.daemon_log();
        assert!(!log.contains("Invalid delivery configuration"), "{log}");
        assert!(!log.contains("Message delivery is unavailable"), "{log}");
        self.call("msg.inbox", json!({}), None)
            .unwrap_or_else(|error| panic!("message service is not served: {error:?}\n{log}"));
    }

    pub fn daemon_log(&self) -> String {
        std::fs::read_to_string(self.test.rig.root().join("daemon.log")).unwrap_or_default()
    }

    /// One method call; an error answer is `(code, message)`.
    pub fn call(
        &self,
        method: &str,
        params: Value,
        from: Option<&StandInPane>,
    ) -> Result<Value, (String, String)> {
        call_on(
            &self.test.rig.socket(),
            method,
            params,
            from.map(|pane| pane.pane_id.clone()),
        )
    }

    /// U8: a decision's question thread as the owner reads it through
    /// `channel.history`, one `(author, step, woke)` per post, oldest first.
    #[allow(dead_code)] // Only the decision test binaries read threads.
    pub fn decision_thread(&self, id: &str) -> Vec<(String, String, Vec<String>)> {
        let row = self.call("decision.get", json!({"id":id}), None).unwrap();
        let page = self
            .call(
                "channel.history",
                json!({"channel":row["decision"]["channel"],"thread":row["decision"]["post_id"]}),
                None,
            )
            .unwrap();
        let text = |value: &Value| value.as_str().unwrap().to_owned();
        page["posts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|post| {
                (
                    text(&post["author"]),
                    text(&post["decision"]["step"]),
                    post["woke"].as_array().unwrap().iter().map(text).collect(),
                )
            })
            .collect()
    }

    /// The real CLI, as the owner or from an agent's pane.
    pub fn atm(&self, caller: Option<&StandInPane>, args: &[&str]) -> Output {
        let rig = &self.test.rig;
        let mut command = rig.command(Path::new(env!("CARGO_BIN_EXE_atm")));
        command.args(args);
        if let Some(pane) = caller {
            command
                .env("TMUX", rig.tmux_env())
                .env("TMUX_PANE", &pane.pane_id);
        }
        rig.run(&mut command)
    }

    /// The CLI's `--format json` answer of a command that must succeed.
    pub fn atm_json(&self, caller: Option<&StandInPane>, args: &[&str]) -> Value {
        let output = self.atm(caller, args);
        assert!(
            output.status.success(),
            "atm {args:?}: {output:?}\n{}",
            self.daemon_log()
        );
        serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|error| panic!("atm {args:?}: {error}: {output:?}"))
    }

    /// `atm msg send`; the new message's id.
    pub fn send(&self, caller: Option<&StandInPane>, to: &str, body: &str) -> String {
        let sent = self.atm_json(
            caller,
            &[
                "msg", "send", "--to", to, "--body", body, "--format", "json",
            ],
        );
        sent["message_id"].as_str().expect("message id").to_owned()
    }

    /// `atm inbox --id`: the one entry, as the caller may see it.
    pub fn inbox_entry(&self, caller: Option<&StandInPane>, of: Option<&str>, id: &str) -> Value {
        let mut args = vec!["inbox", "--id", id, "--format", "json"];
        if let Some(of) = of {
            args.extend(["--for", of]);
        }
        let inbox = self.atm_json(caller, &args);
        let entries = inbox["messages"].as_array().expect("inbox entries");
        assert_eq!(entries.len(), 1, "{inbox}");
        assert_eq!(entries[0]["message_id"], id);
        entries[0].clone()
    }

    /// A watch of every committed event after the log's current end. Open it
    /// before the action whose events are awaited.
    pub fn watch(&self) -> Watch {
        Watch::open(self, self.test.cursor())
    }

    /// A stand-in in an interactive shell with a finished turn behind it,
    /// adopted as `project/<name>` and Ready.
    pub fn ready_agent(&self, name: &str, mode: StandInMode) -> Recipient {
        let pane = self.test.shell_agent(name, &lifecycle::config(mode));
        let session = match mode {
            // Codex announces a session with its first prompt only.
            StandInMode::Codex => self.first_codex_turn(&pane),
            _ => self.test.complete_fixture_turn(&pane).id.to_string(),
        };
        let adopted = self.test.adopt(name, &pane);
        let id = adopted["agent_id"].as_str().expect("agent id").to_owned();
        let agent = self.test.agent(&id);
        assert_eq!(
            adopted["state"],
            "ready",
            "{mode:?} was not adopted Ready: {agent}\n{}",
            self.test.rig.capture(&pane)
        );
        assert_eq!(agent["session_id"], session.as_str(), "{agent}");
        Recipient {
            pane,
            id,
            session: Some(session),
        }
    }

    /// The first prompt of a Codex stand-in, to its end: the session id.
    fn first_codex_turn(&self, pane: &StandInPane) -> String {
        let rig = &self.test.rig;
        rig.wait_ready(pane, None);
        let after = rig.event_cursor(pane);
        rig.paste(pane, "existing fixture conversation");
        rig.wait_event(pane, after, |e| {
            e["kind"] == "rendered" && e["state"] == "working" && e["draft"] == ""
        });
        let row = rig.wait_for_row(pane, |r| {
            !r.id.is_pending() && r.status == SessionStatus::Working
        });
        self.finish_turn(pane);
        let idle = rig.wait_for_row(pane, |r| r.status == SessionStatus::Idle);
        assert_eq!(idle.id, row.id);
        rig.wait_ready(pane, Some(row.id.as_str()));
        row.id.to_string()
    }

    /// Start a turn the stand-in's manual clock holds: the recipient is busy
    /// until `finish_turn`. Test input, never the daemon's.
    pub fn hold_turn(&self, pane: &StandInPane) {
        let rig = &self.test.rig;
        let after = rig.event_cursor(pane);
        rig.paste(pane, "held fixture turn");
        rig.wait_event(pane, after, |e| {
            e["kind"] == "rendered" && e["state"] == "working" && e["draft"] == ""
        });
        rig.wait_for_row(pane, |r| r.status == SessionStatus::Working);
    }

    /// Move the stand-in's manual clock on by `ms` and wait for a frame in
    /// `state` on the terminal. Model time, never wall time.
    pub fn advance(&self, pane: &StandInPane, ms: u64, state: &str) {
        let rig = &self.test.rig;
        let now = rig.control(pane, json!({"command": "barrier"}))["now_ms"]
            .as_u64()
            .expect("model time");
        let after = rig.event_cursor(pane);
        let reply = rig.control(pane, json!({"command": "advance_to", "now_ms": now + ms}));
        assert!(reply["error"].is_null(), "{reply}");
        rig.wait_event(pane, after, |e| {
            e["kind"] == "rendered" && e["state"] == state
        });
    }

    /// End the stand-in's held turn.
    pub fn finish_turn(&self, pane: &StandInPane) {
        self.advance(pane, TURN_MS, "idle");
    }

    /// Type one of the stand-in's own `/atm/...` controls (test input, never
    /// the daemon's) and wait until the stand-in has taken it.
    pub fn stand_in_control(&self, pane: &StandInPane, line: &str, detail: &str) {
        let rig = &self.test.rig;
        let after = rig.event_cursor(pane);
        rig.paste(pane, line);
        rig.wait_event(pane, after, |e| {
            e["kind"] == "rendered" && e["detail"] == detail && e["draft"] == ""
        });
    }

    /// Every prompt the stand-in accepted after the cursor, in order.
    pub fn accepted_since(&self, pane: &StandInPane, after: u64) -> Vec<String> {
        self.test
            .rig
            .journal(pane)
            .iter()
            .filter(|e| e["event_sequence"].as_u64().unwrap() > after && e["kind"] == "accepted")
            .map(|e| e["text"].as_str().expect("accepted text").to_owned())
            .collect()
    }

    /// Nothing was typed into the pane since the cursor, and its screen is
    /// the one captured then. Call it only after a fence showing the daemon's
    /// decision or the failed scheduling round's return: the journal's silence
    /// alone says nothing about a daemon that has not yet decided.
    pub fn assert_untouched(&self, pane: &StandInPane, after: u64, screen: &str) {
        self.test.assert_no_input_since(pane, after);
        assert_eq!(
            self.test.rig.capture_styled(pane),
            screen,
            "the pane's screen changed"
        );
    }
}

/// The organisation log's segment limit (`eventlog`'s 16 MiB).
const SEGMENT_BYTES: u64 = 16 * 1024 * 1024;

/// A running daemon whose organisation log refuses its next append, until
/// this is dropped. Nothing in the daemon is changed: the log's one segment
/// is filled to its limit with real owner messages, so the next record needs
/// a new segment file. The directory is then moved aside and replaced with a
/// regular file: opening a child fails with ENOTDIR, even for root. The open
/// segment stays intact in the moved directory.
///
/// The first refused append disables the daemon's writer until a restart, so
/// nothing here probes the fault with a write of its own.
pub struct RefusedAppends {
    directory: PathBuf,
    parked: PathBuf,
    size: u64,
}

impl RefusedAppends {
    /// The daemon's own words for a refused append.
    pub const LOGGED: &'static str = "Organisation writer disabled until restart";

    fn segment(directory: &Path) -> (PathBuf, u64) {
        let mut files: Vec<_> = std::fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|e| e == "jsonl"))
            .collect();
        assert_eq!(files.len(), 1, "the log rotated: {files:?}");
        let file = files.remove(0);
        let size = std::fs::metadata(&file).unwrap().len();
        (file, size)
    }

    pub fn start(rig: &MessageRig) -> Self {
        let directory = rig.test.rig.root().join("state/log/events");
        let owner = |body: usize, attachments: usize| {
            let params = json!({
                "to": "owner",
                "body": "x".repeat(body),
                "attachments": vec!["a".repeat(4000); attachments],
            });
            let before = Self::segment(&directory).1;
            rig.call("msg.send", params, None)
                .unwrap_or_else(|error| panic!("filling the log: {error:?}"));
            Self::segment(&directory).1 - before
        };
        // What one message's record costs beyond its body.
        let overhead = owner(100, 0) - 100;
        let left = |directory: &Path| SEGMENT_BYTES - Self::segment(directory).1;
        while left(&directory) > 400_000 {
            owner(16_000, 32);
        }
        while left(&directory) > 16_000 {
            owner(8_000, 0);
        }
        // The last record leaves less room than any event needs.
        let room = left(&directory);
        owner(usize::try_from(room - overhead - 40).unwrap(), 0);
        let room = left(&directory);
        assert!(room < 80, "{room} bytes are left in the segment");
        let size = Self::segment(&directory).1;
        let parked = directory.with_file_name("events-parked");
        std::fs::rename(&directory, &parked).unwrap();
        std::fs::write(&directory, b"rotation blocked\n").unwrap();
        Self {
            directory,
            parked,
            size,
        }
    }

    /// Nothing was written since the fault began, and no attempt is on record.
    pub fn assert_nothing_appended(&self) {
        assert!(self.directory.is_file(), "the rotation blocker was removed");
        let (file, size) = Self::segment(&self.parked);
        assert_eq!(size, self.size, "a refused append was written");
        let log = std::fs::read_to_string(file).unwrap();
        assert!(
            !log.contains("\"msg.delivery_attempted\""),
            "an attempt is in the log"
        );
    }
}

impl Drop for RefusedAppends {
    fn drop(&mut self) {
        // Put the intact log back before the rig removes its directory.
        let _ = std::fs::remove_file(&self.directory);
        let _ = std::fs::rename(&self.parked, &self.directory);
    }
}

/// Committed events through the daemon's real `watch`, on a connection of
/// its own. Every frame read is kept in `seen` for assertions on order.
pub struct Watch {
    reader: BufReader<UnixStream>,
    log: PathBuf,
    next: usize,
    cursor: u64,
    pub seen: Vec<Value>,
}

impl Watch {
    /// A watch of every committed event after `cursor`.
    pub fn open(rig: &MessageRig, cursor: u64) -> Self {
        Self::on(&rig.test.rig, cursor)
    }

    /// Register before the action; replay from the supplied committed cursor
    /// also covers an action that completes before the next receive.
    pub fn on(rig: &StandInRig, cursor: u64) -> Self {
        Self::at(&rig.socket(), rig.root().join("daemon.log"), cursor)
    }

    fn at(socket: &Path, log: PathBuf, cursor: u64) -> Self {
        let mut writer = UnixStream::connect(socket).unwrap();
        writer.set_read_timeout(Some(GUARD)).unwrap();
        writer.set_write_timeout(Some(GUARD)).unwrap();
        let mut watch = Self {
            reader: BufReader::new(writer.try_clone().unwrap()),
            log,
            next: 0,
            cursor,
            seen: Vec::new(),
        };
        for message in [
            ClientMessage::connect(Some("message-rig-watch".into())),
            ClientMessage::watch(vec![], cursor),
        ] {
            let line = serde_json::to_string(&message).unwrap();
            writer.write_all(format!("{line}\n").as_bytes()).unwrap();
        }
        let connected = watch.frame("the watch handshake");
        assert_eq!(connected["type"], "connected", "{connected}");
        watch
    }

    fn frame(&mut self, what: &str) -> Value {
        let mut line = String::new();
        match self.reader.read_line(&mut line) {
            Ok(read) if read > 0 => {
                serde_json::from_str(&line).unwrap_or_else(|error| panic!("{error}: {line:?}"))
            }
            other => panic!(
                "no committed event for: {what} ({other:?})\nseen: {:#?}\n{}",
                self.seen
                    .iter()
                    .map(|e| format!("{} {} {}", e["seq"], e["kind"], e["body"]))
                    .collect::<Vec<_>>(),
                std::fs::read_to_string(&self.log).unwrap_or_default()
            ),
        }
    }

    fn read(&mut self, what: &str) {
        let frame = self.frame(what);
        if frame["type"] == "event" {
            let seq = frame["seq"].as_u64().expect("committed event sequence");
            if seq > self.cursor {
                self.cursor = seq;
                self.seen.push(frame);
            }
        }
    }

    /// The next committed event, after the last one returned, that matches.
    pub fn wait(&mut self, what: &str, predicate: impl Fn(&Value) -> bool) -> Value {
        loop {
            while let Some(event) = self.seen.get(self.next) {
                self.next += 1;
                if predicate(event) {
                    return event.clone();
                }
            }
            self.read(what);
        }
    }

    /// Read up to the log's end as it is now, so that `seen` holds every
    /// event committed before this call.
    pub fn catch_up(&mut self, rig: &MessageRig) {
        self.catch_up_to(rig.test.cursor());
    }

    pub fn catch_up_to(&mut self, end: u64) {
        while self.cursor < end {
            self.read("the log's end");
        }
    }

    /// Every event seen so far that matches, in commit order.
    pub fn all(&self, predicate: impl Fn(&Value) -> bool) -> Vec<Value> {
        self.seen.iter().filter(|e| predicate(e)).cloned().collect()
    }
}

/// A terminal attached to the rig's private tmux server, to give a pane the
/// owner's focus. It exists only on that server, addressed by its socket;
/// the client and the program that holds its pty end in `Drop`.
pub struct FocusClient {
    child: Option<stand_in::process::OwnedChild>,
    pid: u32,
    stdin: Option<ChildStdin>,
}

/// Holds a pty with one attached tmux client. A line "in" or "out" on stdin
/// writes the terminal's focus report to it; end of input detaches and exits.
const FOCUS_PROGRAM: &str = r#"
import fcntl, os, select, signal, struct, sys, termios
argv = sys.argv[1:]
master, slave = os.openpty()
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 50, 200, 0, 0))
pid = os.fork()
if pid == 0:
    os.setsid()
    fcntl.ioctl(slave, termios.TIOCSCTTY, 0)
    for fd in (0, 1, 2):
        os.dup2(slave, fd)
    os.close(master)
    os.close(slave)
    os.execvp(argv[0], argv)
os.close(slave)
while True:
    ready, _, _ = select.select([0, master], [], [])
    if master in ready:
        try:
            if not os.read(master, 65536):
                break
        except OSError:
            break
    if 0 in ready:
        line = os.read(0, 64)
        if not line:
            break
        os.write(master, b"\x1b[I" if line.strip() == b"in" else b"\x1b[O")
signal.signal(signal.SIGALRM, lambda *_: os.kill(pid, signal.SIGKILL))
signal.alarm(10)
try:
    os.kill(pid, signal.SIGHUP)
except ProcessLookupError:
    pass
os.close(master)
os.waitpid(pid, 0)
"#;

impl FocusClient {
    /// Attach one unfocused client that shows the pane's window. The window
    /// keeps its size whatever the client's: a stand-in draws at its launch
    /// size only.
    pub fn attach(rig: &MessageRig, pane: &StandInPane) -> Self {
        let tmux = &rig.test.rig;
        let screen = tmux.capture_styled(pane);
        let size = |rig: &MessageRig| {
            checked(rig.test.rig.tmux(&[
                "display-message",
                "-p",
                "-t",
                &pane.pane_id,
                "#{pane_width}x#{pane_height}",
            ]))
        };
        let before = size(rig);
        // tmux tracks a client's focus only with this option on, as on the
        // owner's server.
        checked(tmux.tmux(&["set-option", "-g", "focus-events", "on"]));
        checked(tmux.tmux(&[
            "set-option",
            "-w",
            "-t",
            &pane.pane_id,
            "window-size",
            "manual",
        ]));
        checked(tmux.tmux(&["select-window", "-t", &pane.pane_id]));
        let socket = rig.test.server();
        // Install before attachment: wait-for retains an already-delivered hook.
        checked(tmux.tmux(&[
            "set-hook",
            "-g",
            "client-attached",
            "wait-for -S fixture-attached",
        ]));
        // `command` clears the environment: no TMUX or TMUX_PANE reaches it.
        let mut child = tmux
            .command(Path::new("python3"))
            .args(["-c", FOCUS_PROGRAM, "tmux", "-S", &socket])
            .args(["attach-session", "-t", "fixture"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start the focus client");
        let pid = child.id();
        let stdin = child.stdin.take();
        let client = Self {
            child: Some(stand_in::process::OwnedChild::new(child)),
            pid,
            stdin,
        };
        checked(tmux.tmux(&["wait-for", "fixture-attached"]));
        assert!(client.flags(rig).is_some());
        assert_eq!(size(rig), before, "attaching resized the stand-in's pane");
        assert_eq!(tmux.capture_styled(pane), screen);
        assert_eq!(
            client.flags(rig).map(|(_, on)| on),
            Some(pane.pane_id.clone())
        );
        client
    }

    /// The one client's flags and the pane it shows.
    fn flags(&self, rig: &MessageRig) -> Option<(Vec<String>, String)> {
        let rows =
            checked(
                rig.test
                    .rig
                    .tmux(&["list-clients", "-F", "#{client_flags}\t#{pane_id}"]),
            );
        let mut rows = rows.lines();
        let (flags, pane) = rows.next()?.split_once('\t')?;
        assert_eq!(rows.next(), None, "more than one client");
        Some((flags.split(',').map(str::to_owned).collect(), pane.into()))
    }

    /// Report focus in or out, and wait until tmux says so for the client.
    pub fn focus(&mut self, rig: &MessageRig, focused: bool) {
        let hook = if focused {
            "client-focus-in"
        } else {
            "client-focus-out"
        };
        checked(
            rig.test
                .rig
                .tmux(&["set-hook", "-g", hook, "wait-for -S fixture-focus"]),
        );
        let line: &[u8] = if focused { b"in\n" } else { b"out\n" };
        let stdin = self.stdin.as_mut().expect("focus client input");
        stdin.write_all(line).unwrap();
        stdin.flush().unwrap();
        checked(rig.test.rig.tmux(&["wait-for", "fixture-focus"]));
        assert!(self
            .flags(rig)
            .is_some_and(|(flags, _)| flags.iter().any(|flag| flag == "focused") == focused));
    }
}

impl Drop for FocusClient {
    fn drop(&mut self) {
        // End of input: the program hangs up its client and exits.
        drop(self.stdin.take());
        let _ = stand_in::process::wait_exit(self.pid, Duration::from_secs(30));
        // OwnedChild explicitly kills and reaps, even if the graceful guard failed.
        drop(self.child.take());
    }
}

#[cfg(test)]
pub mod watch_tests {
    use super::*;

    pub fn replay_live_overlap_keeps_each_sequence_once() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("watch.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let peer = std::thread::spawn(move || {
            let (mut writer, _) = listener.accept().unwrap();
            writer.set_read_timeout(Some(GUARD)).unwrap();
            writer.set_write_timeout(Some(GUARD)).unwrap();
            let mut reader = BufReader::new(writer.try_clone().unwrap());
            for kind in ["connect", "watch"] {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let frame: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(frame["type"], kind);
                if kind == "watch" {
                    assert_eq!(frame["cursor"], 5);
                }
            }
            writeln!(writer, "{}", json!({"type":"connected"})).unwrap();
            for (seq, body) in [
                (5, "covered"),
                (6, "replay-a"),
                (7, "replay-b"),
                (7, "overlap"),
                (8, "live"),
            ] {
                writeln!(
                    writer,
                    "{}",
                    json!({"type":"event", "seq":seq, "kind":"msg.sent", "body":{"body":body}})
                )
                .unwrap();
            }
        });
        let mut watch = Watch::at(&socket, dir.path().join("daemon.log"), 5);
        // All frames, including the live overlap, arrive before the waiter.
        peer.join().unwrap();
        watch.catch_up_to(5); // No event needed at an already-covered cursor.
        for (seq, body) in [(6, "replay-a"), (7, "replay-b"), (8, "live")] {
            let event = watch.wait("ordered replay and live events", |_| true);
            assert_eq!(event["seq"], seq);
            assert_eq!(event["body"]["body"], body);
        }
        watch.catch_up_to(8);
        assert_eq!(
            watch
                .seen
                .iter()
                .map(|e| e["seq"].as_u64().unwrap())
                .collect::<Vec<_>>(),
            [6, 7, 8]
        );
    }
}
