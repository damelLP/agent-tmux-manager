//! The real `atmd` runs the `[notify]` command from a temp config when a
//! replayed Claude session starts needing input.
//!
//! Sealed: `XDG_CONFIG_HOME`, `XDG_STATE_HOME`, `HOME`, `TMUX_TMPDIR` and
//! `ATM_SOCKET` all point into the test's temp dir, and `TMUX`,
//! `TMUX_PANE` and `ATM_TMUX_SOCKET` are removed, so the daemon reaches
//! neither the user's daemon nor their tmux server. Startup discovery
//! still lists the host's real agents, but none of them sends this daemon
//! a hook, so they never notify.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)] // test code may panic

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use atm_protocol::{ClientMessage, DaemonMessage};

/// The hook payloads of one Claude Code TUI session, as Claude wrote them
/// to the hook's stdin. Captured: Claude Code 2.1.310, 2026-10-24, the run
/// described under "Captured payloads" in atm-claude-adapter's crate docs.
/// It has two permission dialogs (each a `PermissionRequest` and then a
/// `Notification` of type `permission_prompt`) and one `idle_prompt`.
const CLAUDE_SESSION: &str =
    "crates/atm-claude-adapter/tests/fixtures/claude_tui_session_2.1.310.jsonl";

/// A real `atmd start` in a temp dir, killed on drop.
struct Daemon {
    child: Child,
    socket: PathBuf,
    log: PathBuf,
}

impl Daemon {
    fn start(dir: &Path) -> Self {
        let socket = dir.join("atm.sock");
        let log = dir.join("atmd.log");
        for sub in ["state", "home", "tmux"] {
            fs::create_dir_all(dir.join(sub)).unwrap();
        }
        let child = Command::new(env!("CARGO_BIN_EXE_atmd"))
            .arg("start")
            .env("ATM_SOCKET", &socket)
            .env("XDG_CONFIG_HOME", dir.join("config"))
            .env("XDG_STATE_HOME", dir.join("state"))
            .env("HOME", dir.join("home"))
            .env("TMUX_TMPDIR", dir.join("tmux"))
            .env("RUST_LOG", "atmd=debug")
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .env_remove("ATM_TMUX_SOCKET")
            .stdin(Stdio::null())
            .stdout(fs::File::create(&log).unwrap())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("start atmd");
        let daemon = Self { child, socket, log };
        let deadline = Instant::now() + Duration::from_secs(10);
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

/// One connection that sends each message and waits for it to apply.
struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    seq: u64,
}

impl Client {
    fn connect(socket: &Path) -> Self {
        let writer = UnixStream::connect(socket).unwrap();
        writer
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut client = Self {
            reader: BufReader::new(writer.try_clone().unwrap()),
            writer,
            seq: 0,
        };
        client.send(&ClientMessage::connect(Some("atmd-notify-test".into())));
        match client.recv() {
            DaemonMessage::Connected { .. } => client,
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

    /// atmd handles one connection's messages in order and awaits the
    /// registry for each, so the Pong means the hook has been applied.
    fn apply(&mut self, message: &ClientMessage) {
        self.seq += 1;
        self.send(message);
        self.send(&ClientMessage::ping(self.seq));
        match self.recv() {
            DaemonMessage::Pong { seq } if seq == self.seq => {}
            other => panic!("expected Pong {}, got {other:?}", self.seq),
        }
    }
}

/// The captured hook payloads, each with the `pid` and `tmux_pane` that
/// atm-hook adds: this test's own pid, so the row's process is alive.
fn captured_hooks() -> Vec<serde_json::Value> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(CLAUDE_SESSION);
    fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
        .lines()
        .map(|line| {
            let mut payload: serde_json::Value = serde_json::from_str(line).unwrap();
            payload["pid"] = std::process::id().into();
            payload["tmux_pane"] = "%42".into();
            payload
        })
        .collect()
}

/// The argument lists of every call so far, one file per call.
fn calls(dir: &Path) -> Vec<Vec<String>> {
    let mut files: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    files.sort();
    files
        .iter()
        .map(|file| {
            let bytes = fs::read(file).unwrap();
            let text = String::from_utf8(bytes).unwrap();
            let mut args: Vec<String> = text.split('\0').map(String::from).collect();
            assert_eq!(args.pop().as_deref(), Some(""), "args end with NUL");
            args
        })
        .collect()
}

/// Waits until `n` calls have arrived, then a little more, so a call
/// that should not happen has time to show up.
fn settle(dir: &Path, n: usize, daemon: &Daemon) -> Vec<Vec<String>> {
    let deadline = Instant::now() + Duration::from_secs(10);
    while calls(dir).len() < n {
        assert!(
            Instant::now() < deadline,
            "{} of {n} calls arrived\n{}",
            calls(dir).len(),
            daemon.log()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_millis(700));
    calls(dir)
}

#[test]
fn atmd_notifies_once_per_permission_dialog() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("calls");
    fs::create_dir_all(&out).unwrap();
    let recorder = dir.path().join("record");
    fs::write(
        &recorder,
        format!(
            "#!/bin/sh\nf=$(mktemp '{}/call.XXXXXX') && printf '%s\\0' \"$@\" > \"$f\"\n",
            out.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&recorder, fs::Permissions::from_mode(0o755)).unwrap();
    fs::create_dir_all(dir.path().join("config/atm")).unwrap();
    fs::write(
        dir.path().join("config/atm/config.toml"),
        format!(
            "[harness]\ndefault = 'claude'\n\n[notify]\ncommand = ['{}', '--']\n",
            recorder.display()
        ),
    )
    .unwrap();

    let daemon = Daemon::start(dir.path());
    let mut client = Client::connect(&daemon.socket);
    let hooks = captured_hooks();
    let kind = |i: usize| {
        let hook = &hooks[i];
        (
            hook["hook_event_name"].as_str().unwrap().to_string(),
            hook["notification_type"].as_str().map(String::from),
        )
    };
    let notification = |n: &str| ("Notification".to_string(), Some(n.to_string()));

    // The first dialog: PermissionRequest turns the row to needs input,
    // and its Notification keeps it there.
    let first_dialog_end = 4;
    assert_eq!(kind(3).0, "PermissionRequest");
    assert_eq!(kind(first_dialog_end), notification("permission_prompt"));
    for hook in &hooks[..=first_dialog_end] {
        client.apply(&ClientMessage::hook_event(hook.clone()));
    }
    let after_first = settle(&out, 1, &daemon);
    assert_eq!(after_first.len(), 1, "{after_first:?}");
    let [dashes, message] = after_first[0].as_slice() else {
        panic!("expected `--` and one message, got {:?}", after_first[0]);
    };
    assert_eq!(dashes, "--");
    assert!(
        message.ends_with(" %42: claude needs input (Bash: touch atm-probe.txt && echo atm-probe)"),
        "{message:?}"
    );

    // The rest of the session: one more dialog, a subagent run, then the
    // idle prompt, `/compact`, `/clear` and a resume. Only the dialog
    // notifies; `idle_prompt` maps to idle.
    let idle_at = (0..hooks.len())
        .find(|&i| kind(i) == notification("idle_prompt"))
        .unwrap();
    assert!(idle_at > first_dialog_end);
    for hook in &hooks[first_dialog_end + 1..] {
        client.apply(&ClientMessage::hook_event(hook.clone()));
    }
    let all = settle(&out, 2, &daemon);
    assert_eq!(all.len(), 2, "{all:?}");
}

/// The default config `atm setup` writes leaves notify off, and its
/// commented-out example is a working `[notify]` once uncommented.
#[test]
fn default_config_example_parses_once_uncommented() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    assert!(atm_tui::setup::ensure_default_atm_config(&path).unwrap());
    let text = fs::read_to_string(&path).unwrap();
    assert!(atmd::notify::parse_notify_command(&text).unwrap().is_none());

    let example = "# [notify]\n# command = ";
    assert_eq!(text.matches(example).count(), 1, "{text}");
    let uncommented = text
        .replace(example, "[notify]\ncommand = ")
        .replace("# delay_secs = ", "delay_secs = ");
    let config = atmd::notify::parse_notify_command(&uncommented)
        .unwrap()
        .expect("the example configures a command");
    assert_eq!(config.command, ["notify-send", "atm", "--"]);
    assert_eq!(config.delay_secs, 0, "the example keeps the default");
}
