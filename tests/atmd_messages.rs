//! The real `atmd` hands its `[delivery]` config to the message service: a
//! valid one serves the message methods, an invalid one leaves them
//! unavailable with the cause.
//!
//! Sealed like `atmd_notify`: `XDG_CONFIG_HOME`, `XDG_STATE_HOME`,
//! `ATM_STATE_DIR`, `HOME`, `TMUX_TMPDIR` and `ATM_SOCKET` all point into the
//! test's temp dir, and `TMUX`, `TMUX_PANE` and `ATM_TMUX_SOCKET` are removed,
//! so the daemon reaches neither the user's daemon nor their tmux server.
//! Only owner messages are sent: nothing here is addressed to a pane.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)] // test code may panic

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use atm_protocol::{ClientMessage, DaemonMessage, ResponsePayload};
use serde_json::{json, Value};

/// A real `atmd start` in a temp dir with this `config.toml`, killed on drop.
struct Daemon {
    child: Child,
    socket: PathBuf,
    log: PathBuf,
}

impl Daemon {
    fn start(dir: &Path, config: &str) -> Self {
        let socket = dir.join("atm.sock");
        let log = dir.join("atmd.log");
        for sub in ["state", "home", "tmux", "config/atm"] {
            fs::create_dir_all(dir.join(sub)).unwrap();
        }
        fs::write(dir.join("config/atm/config.toml"), config).unwrap();
        let output = fs::File::create(&log).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_atmd"))
            .arg("start")
            .env("ATM_SOCKET", &socket)
            .env("XDG_CONFIG_HOME", dir.join("config"))
            .env("XDG_STATE_HOME", dir.join("state"))
            .env("ATM_STATE_DIR", dir.join("eventlog"))
            .env("HOME", dir.join("home"))
            .env("TMUX_TMPDIR", dir.join("tmux"))
            .env("NO_COLOR", "1")
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .env_remove("ATM_TMUX_SOCKET")
            .stdin(Stdio::null())
            .stdout(output.try_clone().unwrap())
            .stderr(output)
            .spawn()
            .expect("start atmd");
        let daemon = Self { child, socket, log };
        // A bounded poll on a condition (the daemon listens), as the other
        // real-daemon tests do; nothing after it waits on time.
        let deadline = Instant::now() + Duration::from_secs(30);
        while UnixStream::connect(&daemon.socket).is_err() {
            assert!(
                Instant::now() < deadline,
                "atmd never listened\n{}",
                daemon.log()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        daemon
    }

    fn log(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// One connection that calls methods and returns each one's own response.
struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    calls: u64,
}

impl Client {
    fn connect(socket: &Path) -> (Self, Vec<String>) {
        let writer = UnixStream::connect(socket).unwrap();
        // A hang guard only: every read below is answered by the daemon.
        writer
            .set_read_timeout(Some(Duration::from_secs(60)))
            .unwrap();
        let mut client = Self {
            reader: BufReader::new(writer.try_clone().unwrap()),
            writer,
            calls: 0,
        };
        client.send(&ClientMessage::connect(Some("atmd-messages-test".into())));
        match client.recv() {
            DaemonMessage::Connected { methods, .. } => (client, methods),
            other => panic!("expected Connected, got {other:?}"),
        }
    }

    fn send(&mut self, message: &ClientMessage) {
        let line = serde_json::to_string(message).unwrap();
        self.writer
            .write_all(format!("{line}\n").as_bytes())
            .unwrap();
    }

    fn recv(&mut self) -> DaemonMessage {
        let mut line = String::new();
        self.reader.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("{e}: {line:?}"))
    }

    /// The method's result, or its error as `(code, message)`.
    fn call(&mut self, method: &str, params: Value) -> Result<Value, (String, String)> {
        self.calls += 1;
        let id = format!("call-{}", self.calls);
        self.send(&ClientMessage::request(
            id.clone(),
            method.into(),
            params,
            None,
        ));
        loop {
            match self.recv() {
                DaemonMessage::Response {
                    id: answered,
                    payload,
                } if answered == id => {
                    return match payload {
                        ResponsePayload::Result(result) => Ok(result),
                        ResponsePayload::Error(error) => Err((error.code, error.message)),
                    };
                }
                // Session updates of the host's own agents may arrive between.
                _ => continue,
            }
        }
    }
}

const MESSAGE_METHODS: [&str; 5] = [
    "msg.ack",
    "msg.give_up",
    "msg.inbox",
    "msg.retry",
    "msg.send",
];

#[test]
fn valid_delivery_config_serves_the_message_methods() {
    let dir = tempfile::tempdir().unwrap();
    let daemon = Daemon::start(
        dir.path(),
        "[delivery]\nstable_idle = { secs = 1, nanos = 0 }\n",
    );
    let (mut client, methods) = Client::connect(&daemon.socket);
    for method in MESSAGE_METHODS {
        assert!(methods.iter().any(|name| name == method), "{methods:?}");
    }
    let sent = client
        .call(
            "msg.send",
            json!({"to": "owner", "body": "first line\nsecond line"}),
        )
        .unwrap_or_else(|error| panic!("{error:?}\n{}", daemon.log()));
    let id = sent["message_id"].clone();
    let inbox = client.call("msg.inbox", json!({})).unwrap();
    assert_eq!(inbox["unread_count"], 1);
    let entry = &inbox["messages"][0];
    assert_eq!(entry["message_id"], id);
    assert_eq!(entry["content"]["body"], "first line\nsecond line");
    assert_eq!(entry["status"], "delivered");
    // Acknowledged once, and only once.
    for count in [1, 0] {
        let acked = client.call("msg.ack", json!({"ids": [id]})).unwrap();
        assert_eq!(acked["count"], count);
    }
    assert_eq!(
        client.call("msg.inbox", json!({})).unwrap()["unread_count"],
        0
    );
    // The owner's decisions reach the service too: it knows no such message.
    let (code, message) = client
        .call("msg.give_up", json!({"id": "m999"}))
        .unwrap_err();
    assert_eq!(
        (code.as_str(), message.as_str()),
        ("not_found", "message m999 was not found")
    );
    assert!(
        !daemon.log().contains("Invalid delivery configuration"),
        "{}",
        daemon.log()
    );
}

#[test]
fn invalid_delivery_config_warns_and_leaves_the_message_methods_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    // An unknown key: the settings are never replaced by defaults.
    let daemon = Daemon::start(dir.path(), "[delivery]\nunknown = 1\n");
    let cause = "invalid config: malformed TOML or invalid [delivery] settings";
    let log = daemon.log();
    assert!(
        log.contains("Invalid delivery configuration; message delivery is unavailable"),
        "{log}"
    );
    assert!(log.contains(cause), "{log}");
    // The methods stay advertised, and each valid call says why it cannot run.
    let (mut client, methods) = Client::connect(&daemon.socket);
    for method in MESSAGE_METHODS {
        assert!(methods.iter().any(|name| name == method), "{methods:?}");
    }
    let unavailable = format!("message service unavailable: {cause}");
    for (method, params) in [
        ("msg.send", json!({"to": "owner", "body": "hello"})),
        ("msg.inbox", json!({})),
        ("msg.ack", json!({"ids": ["m1"]})),
        ("msg.retry", json!({"id": "m1"})),
        ("msg.give_up", json!({"id": "m1"})),
    ] {
        let (code, message) = client.call(method, params).unwrap_err();
        assert_eq!(
            (code.as_str(), message.as_str()),
            ("unavailable", unavailable.as_str()),
            "{method}"
        );
    }
    // The rest of the daemon is unaffected.
    assert!(client.call("daemon.info", json!({})).is_ok());
}
