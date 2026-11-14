//! Shared real-daemon/private-tmux fixture. See stand_in/README.md for its contract.
#![allow(dead_code)] // Consumers intentionally use different subsets of test support.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::time::{Duration, Instant};

use atm_core::{SessionId, SessionView};
use atm_protocol::{ClientMessage, DaemonMessage};
use serde_json::Value;

#[path = "role.rs"]
pub(crate) mod bounded;
#[path = "stand_in/events.rs"]
mod events;
#[path = "stand_in/process.rs"]
mod process;
#[path = "stand_in/rows.rs"]
mod rows;

const DEADLINE: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(20);
const SOURCE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/stand_in");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StandInMode {
    Claude,
    Codex,
    Pi,
}
impl StandInMode {
    pub fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Pi => "pi",
        }
    }
}

#[derive(Debug, Clone)]
pub struct StandInConfig {
    pub mode: StandInMode,
    pub work_ms: u64,
    pub clear_gap_ms: u64,
    pub manual_clock: bool,
    pub id_prefix: String,
    pub folder_trust: bool,
    pub update_prompt: bool,
    pub worktree_choice: bool,
    pub hooks_review: bool,
    pub permission_dialog: bool,
    pub screen_variant: String,
    pub folder: Option<String>,
    pub columns: u16,
    pub rows: u16,
}
impl StandInConfig {
    pub fn new(mode: StandInMode) -> Self {
        Self {
            mode,
            work_ms: 500,
            clear_gap_ms: 100,
            manual_clock: false,
            id_prefix: "stand-in".into(),
            folder_trust: false,
            update_prompt: false,
            worktree_choice: false,
            hooks_review: false,
            permission_dialog: false,
            screen_variant: "u5".into(),
            folder: None,
            columns: 120,
            rows: 40,
        }
    }
}

#[derive(Debug)]
pub struct StandInPane {
    pub pane_id: String,
    pub pid: u32,
    start_time: String,
    pub launcher: PathBuf,
    pub log: PathBuf,
    owner: PathBuf,
}

pub struct StandInRig {
    // Drop the listener before deleting its socket directory.
    events: events::Events,
    rows: Option<rows::Rows>,
    dir: tempfile::TempDir,
    label: String,
    tmux_identity: Option<(PathBuf, Option<u32>)>,
    daemon: Option<Child>,
    daemon_env: std::collections::BTreeMap<String, std::ffi::OsString>,
    serial: usize,
    path: std::ffi::OsString,
}

fn checked(output: Output) -> String {
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout)
        .expect("utf8 output")
        .trim()
        .into()
}

fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

pub fn require_tools(path: &std::ffi::OsStr) {
    use std::os::unix::fs::PermissionsExt;
    for tool in ["python3", "tmux", "jq", "socat", "timeout", "bash"] {
        assert!(
            std::env::split_paths(path)
                .filter(|dir| dir.is_absolute())
                .any(|dir| {
                    dir.join(tool)
                        .metadata()
                        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                }),
            "required stand-in prerequisite: {tool} (install it; this test never skips)"
        );
    }
}

impl StandInRig {
    pub fn start() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("u11-")
            .tempdir()
            .expect("private run dir");
        let root = dir.path();
        for sub in ["tmux", "state", "config", "tmp", "home", "bin"] {
            std::fs::create_dir(root.join(sub)).unwrap();
        }
        // Exclude installed atm/atmd even when a caller forgot the outer seal.
        let path = std::ffi::OsString::from("/usr/local/bin:/usr/bin:/bin");
        require_tools(&path);
        let mut rig = Self {
            events: events::Events::bind(root.join("events.sock")),
            rows: None,
            dir,
            label: "stand-in".into(),
            tmux_identity: None,
            daemon: None,
            daemon_env: Default::default(),
            serial: 0,
            path,
        };
        for (tool, flag) in [
            ("python3", "--version"),
            ("tmux", "-V"),
            ("jq", "--version"),
            ("socat", "-V"),
            ("timeout", "--version"),
            ("/bin/bash", "--version"),
        ] {
            checked(process::run(rig.command(Path::new(tool)).arg(flag)));
        }
        for mode in [StandInMode::Claude, StandInMode::Codex, StandInMode::Pi] {
            // A script path ending in the vendor name is visible to discovery;
            // the Python process itself sets comm before starting the forwarder.
            std::os::unix::fs::symlink(Path::new(SOURCE).join("harness.py"), rig.launcher(mode))
                .unwrap();
        }
        // Record the canonical private socket before new-session can create a server.
        // This also owns cleanup if the subsequent identity query fails.
        let tmux_dir = rig.root().join("tmux").canonicalize().unwrap();
        let socket = tmux_dir
            .join(format!("tmux-{}", unsafe { libc::getuid() }))
            .join(&rig.label);
        rig.tmux_identity = Some((socket, None));
        checked(rig.tmux(&[
            "-f",
            "/dev/null",
            "new-session",
            "-d",
            "-s",
            "fixture",
            "/bin/bash --noprofile --norc",
        ]));
        // Record the server before any subsequent fallible setup operation.
        let identity = checked(rig.tmux(&["display-message", "-p", "#{socket_path} #{pid}"]));
        let (socket, pid) = identity.rsplit_once(' ').expect("tmux identity");
        assert_eq!(Path::new(socket), rig.tmux_identity.as_ref().unwrap().0);
        rig.tmux_identity = Some((socket.into(), Some(pid.parse().unwrap())));
        checked(rig.tmux(&["set-option", "-g", "default-shell", "/bin/bash"]));
        checked(rig.tmux(&["set-option", "-g", "status", "off"]));
        checked(rig.tmux(&["set-option", "-g", "default-size", "120x40"]));
        checked(rig.tmux(&[
            "set-option",
            "-g",
            "default-command",
            "/bin/bash --noprofile --norc",
        ]));
        rig.start_daemon();
        rig
    }

    /// Extra environment survives restarts; sealing variables cannot be overridden.
    pub fn set_daemon_env(&mut self, key: &str, value: impl AsRef<std::ffi::OsStr>) {
        assert!(
            !matches!(key, "TMUX" | "TMUX_PANE"),
            "cannot override rig seal"
        );
        assert!(
            !self
                .command(Path::new("true"))
                .get_envs()
                .any(|(k, _)| k == key),
            "cannot override rig seal"
        );
        self.daemon_env.insert(key.into(), value.as_ref().into());
    }

    pub fn start_daemon(&mut self) {
        assert!(
            self.daemon_pids().is_empty(),
            "private daemon already running"
        );
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.root().join("daemon.log"))
            .unwrap();
        self.daemon = Some(
            self.command(Path::new(env!("CARGO_BIN_EXE_atmd")))
                .envs(&self.daemon_env)
                .arg("start")
                .stdout(log.try_clone().unwrap())
                .stderr(log)
                .spawn()
                .expect("start private atmd"),
        );
        self.wait("daemon request round trip", || {
            matches!(
                protocol_request(&self.socket(), ClientMessage::list_sessions(), DEADLINE),
                Ok(DaemonMessage::SessionList { .. })
            )
        });
        self.rows = Some(rows::Rows::connect(&self.socket()));
    }

    pub fn stop_daemon(&mut self) {
        self.stop_daemons().expect("stop private daemons");
    }

    pub fn restart_daemon(&mut self) {
        self.stop_daemon();
        self.start_daemon();
    }

    pub fn daemon_pids(&self) -> Vec<u32> {
        std::fs::read_dir("/proc")
            .unwrap()
            .flatten()
            .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
            .filter(|pid| self.daemon_identity(*pid).is_some())
            .collect()
    }

    fn daemon_identity(&self, pid: u32) -> Option<String> {
        let proc = PathBuf::from(format!("/proc/{pid}"));
        if std::fs::read_to_string(proc.join("comm")).ok()?.trim() != "atmd" {
            return None;
        }
        let env = std::fs::read(proc.join("environ")).ok()?;
        let owned = env
            .split(|b| *b == 0)
            .filter_map(|entry| {
                let (_, value) = std::str::from_utf8(entry).ok()?.split_once('=')?;
                Some(Path::new(value).starts_with(self.root()))
            })
            .any(|owned| owned);
        if !owned {
            return None;
        }
        let stat = std::fs::read_to_string(proc.join("stat")).ok()?;
        Some(stat.rsplit_once(')')?.1.split_whitespace().nth(19)?.into())
    }

    fn stop_daemons(&mut self) -> Result<(), String> {
        self.rows = None;
        for pid in self.daemon_pids() {
            let identity = self
                .daemon_identity(pid)
                .ok_or("daemon identity vanished")?;
            if self.daemon_identity(pid).as_ref() != Some(&identity) {
                return Err("daemon identity changed".into());
            }
            unsafe {
                libc::kill(pid as i32, libc::SIGTERM);
            }
            let deadline = Instant::now() + Duration::from_secs(3);
            while self.daemon_identity(pid).as_ref() == Some(&identity) {
                if Instant::now() >= deadline {
                    // Rechecked environment + procfs start time immediately before signalling.
                    unsafe {
                        libc::kill(pid as i32, libc::SIGKILL);
                    }
                    break;
                }
                std::thread::sleep(POLL);
            }
        }
        if let Some(mut child) = self.daemon.take() {
            child.wait().map_err(|e| e.to_string())?;
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while !self.daemon_pids().is_empty() {
            if Instant::now() >= deadline {
                return Err("private daemon failed to exit".into());
            }
            std::thread::sleep(POLL);
        }
        Ok(())
    }

    pub fn root(&self) -> &Path {
        self.dir.path()
    }
    pub fn socket(&self) -> PathBuf {
        self.root().join("atm.sock")
    }
    pub fn launcher(&self, mode: StandInMode) -> PathBuf {
        self.root().join("bin").join(mode.name())
    }
    pub fn tmux_env(&self) -> String {
        let (socket, pid) = self.tmux_identity.as_ref().expect("private server");
        format!("{},{},0", socket.display(), pid.expect("server pid"))
    }

    pub fn command(&self, program: &Path) -> Command {
        let mut cmd = Command::new(program);
        cmd.env_clear()
            .env("PATH", &self.path)
            .env("HOME", self.root().join("home"))
            .env("LANG", "C.UTF-8")
            .env("TERM", "xterm-256color")
            .env("ATM_SOCKET", self.socket())
            .env("ATM_TMUX_SOCKET", &self.label)
            .env("TMUX_TMPDIR", self.root().join("tmux"))
            .env("ATM_STATE_DIR", self.root().join("state"))
            .env("XDG_STATE_HOME", self.root().join("state"))
            .env("XDG_CONFIG_HOME", self.root().join("config"))
            .env("XDG_CACHE_HOME", self.root().join("tmp"))
            .env("TMPDIR", self.root().join("tmp"))
            .env("ATM_STANDIN_ROOT", self.root())
            .env("ATM_STANDIN_EVENTS", self.events.path())
            .env("ATM_DEBUG", "0")
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .current_dir(self.root());
        cmd
    }
    pub fn tmux(&self, args: &[&str]) -> Output {
        process::run(
            self.command(Path::new("tmux"))
                .args(["-L", &self.label])
                .args(args),
        )
    }
    pub fn atm(&self, args: &[&str]) -> Output {
        process::run(
            self.command(Path::new(env!("CARGO_BIN_EXE_atm")))
                .args(args),
        )
    }
    pub fn run(&self, command: &mut Command) -> Output {
        process::run(command)
    }

    pub fn launch(&mut self, config: &StandInConfig) -> StandInPane {
        self.serial += 1;
        let log = self.root().join(format!("pane-{}.jsonl", self.serial));
        let prefix = format!(
            "{}-{}-{}",
            config.id_prefix,
            self.root().file_name().unwrap().to_string_lossy(),
            self.serial
        );
        let config_path = log.with_extension("config.json");
        std::fs::write(
            &config_path,
            serde_json::to_vec(&serde_json::json!({
                "WORK_MS": config.work_ms, "CLEAR_GAP_MS": config.clear_gap_ms,
                "MANUAL_CLOCK": u8::from(config.manual_clock),
                "ID_PREFIX": prefix, "LOG": log,
                "FOLDER_TRUST": u8::from(config.folder_trust),
                "UPDATE_PROMPT": u8::from(config.update_prompt),
                "WORKTREE_CHOICE": u8::from(config.worktree_choice),
                "HOOKS_REVIEW": u8::from(config.hooks_review),
                "PERMISSION_DIALOG": u8::from(config.permission_dialog),
                "SCREEN_VARIANT": config.screen_variant,
                "FOLDER": config.folder.as_deref().unwrap_or(""),
            }))
            .unwrap(),
        )
        .unwrap();
        let command = format!(
            "{} --atm-config {}; exec /bin/bash --noprofile --norc",
            quote(self.launcher(config.mode).to_str().unwrap()),
            quote(config_path.to_str().unwrap())
        );
        checked(self.tmux(&[
            "set-option",
            "-g",
            "default-size",
            &format!("{}x{}", config.columns, config.rows),
        ]));
        let pane_id = checked(self.tmux(&[
            "new-window",
            "-d",
            "-P",
            "-F",
            "#{pane_id}",
            &format!("/bin/bash --noprofile --norc -c {}", quote(&command)),
        ]));
        let pane = self.observe_pane(pane_id, config.mode, log);
        self.wait_event(&pane, 0, |e| e["kind"] == "rendered");
        pane
    }

    /// Observe a stand-in started by atm spawn; unlike launch, it may already be working.
    pub fn observe_pane(&self, pane_id: String, mode: StandInMode, log: PathBuf) -> StandInPane {
        assert_eq!(
            log.parent(),
            Some(self.root()),
            "journal must be in this rig"
        );
        let identity = self.events.wait(|e| {
            if e["kind"] != "identity" || e["pane"] != pane_id || e["mode"] != mode.name() {
                return false;
            }
            let Some(pid) = e["pid"].as_u64().and_then(|p| u32::try_from(p).ok()) else {
                return false;
            };
            let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
                return false;
            };
            stat.rsplit_once(')')
                .and_then(|(_, tail)| tail.split_whitespace().nth(19))
                == e["start_time"].as_str()
        });
        StandInPane {
            pane_id,
            pid: u32::try_from(identity["pid"].as_u64().unwrap()).unwrap(),
            start_time: identity["start_time"].as_str().unwrap().into(),
            launcher: self.launcher(mode),
            log,
            owner: self.root().into(),
        }
    }

    fn owns(&self, pane: &StandInPane) {
        assert_eq!(pane.owner, self.root(), "foreign pane handle");
    }
    pub fn journal(&self, pane: &StandInPane) -> Vec<Value> {
        self.owns(pane);
        std::fs::read_to_string(&pane.log)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }
    pub fn capture(&self, pane: &StandInPane) -> String {
        self.owns(pane);
        checked(self.tmux(&["capture-pane", "-p", "-J", "-t", &pane.pane_id]))
    }
    pub fn capture_plain(&self, pane: &StandInPane) -> String {
        self.owns(pane);
        let output = self.tmux(&["capture-pane", "-p", "-t", &pane.pane_id]);
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).expect("utf8 pane")
    }
    pub fn capture_styled(&self, pane: &StandInPane) -> String {
        self.owns(pane);
        let output = self.tmux(&["capture-pane", "-e", "-N", "-p", "-t", &pane.pane_id]);
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).expect("utf8 styled pane")
    }
    pub fn event_cursor(&self, pane: &StandInPane) -> u64 {
        self.owns(pane);
        self.events.cursor(pane.pid, &pane.start_time)
    }
    pub fn wait_event(
        &self,
        pane: &StandInPane,
        after: u64,
        predicate: impl Fn(&Value) -> bool,
    ) -> Value {
        self.owns(pane);
        self.events.wait(|e| {
            events::matches_process(e, pane.pid, &pane.start_time)
                && e["event_sequence"].as_u64().is_some_and(|seq| seq > after)
                && predicate(e)
        })
    }
    pub fn control(&self, pane: &StandInPane, command: Value) -> Value {
        self.owns(pane);
        self.events.control(pane.pid, &pane.start_time, command)
    }
    pub fn wait_ready(&self, pane: &StandInPane, session_id: Option<&str>) {
        // Barrier orders this check after effects already processed by the runtime.
        // For a particular key/action use wait_event with a pre-action cursor.
        let barrier = self.control(pane, serde_json::json!({"command": "barrier"}));
        let sequence = barrier["render_sequence"].as_u64().unwrap();
        self.wait_event(pane, 0, |e| {
            e["kind"] == "ready"
                && e["sequence"].as_u64().is_some_and(|s| s >= sequence)
                && session_id.is_none_or(|id| e["session_id"] == id)
        });
    }
    pub fn wait_for_shell(&self, pane: &StandInPane) {
        self.owns(pane);
        process::wait_exit(pane.pid, Duration::from_secs(30)).expect("stand-in process exit");
        assert_eq!(
            checked(self.tmux(&[
                "display-message",
                "-p",
                "-t",
                &pane.pane_id,
                "#{pane_current_command}",
            ])),
            "bash",
            "retained shell after stand-in exit"
        );
    }
    pub fn key(&self, pane: &StandInPane, key: &str) {
        self.owns(pane);
        checked(self.tmux(&["send-keys", "-t", &pane.pane_id, key]));
    }
    pub fn paste(&self, pane: &StandInPane, text: &str) {
        self.owns(pane);
        let file = self.root().join("paste.txt");
        std::fs::write(&file, text).unwrap();
        checked(self.tmux(&["load-buffer", "-b", "stand-in", file.to_str().unwrap()]));
        checked(self.tmux(&[
            "paste-buffer",
            "-p",
            "-d",
            "-b",
            "stand-in",
            "-t",
            &pane.pane_id,
        ]));
        self.key(pane, "Enter");
    }

    fn request(&self, request: ClientMessage) -> DaemonMessage {
        protocol_request(&self.socket(), request, DEADLINE).expect("private daemon request")
    }
    pub fn rows(&self) -> Vec<SessionView> {
        match self.request(ClientMessage::list_sessions()) {
            DaemonMessage::SessionList { sessions } => sessions,
            other => panic!("unexpected {other:?}"),
        }
    }
    pub fn discover(&self) {
        assert!(matches!(
            self.request(ClientMessage::discover()),
            DaemonMessage::DiscoveryComplete { .. }
        ));
    }
    pub fn pane_rows(&self, pane: &StandInPane) -> Vec<SessionView> {
        self.owns(pane);
        self.rows()
            .into_iter()
            .filter(|r| r.tmux_pane.as_deref() == Some(&pane.pane_id))
            .collect()
    }
    pub fn wait_for_row(
        &self,
        pane: &StandInPane,
        predicate: impl Fn(&SessionView) -> bool,
    ) -> SessionView {
        self.owns(pane);
        self.rows
            .as_ref()
            .expect("daemon subscription")
            .wait(|rows| {
                rows.iter()
                    .find(|r| r.tmux_pane.as_deref() == Some(&pane.pane_id) && predicate(r))
                    .cloned()
            })
    }
    pub fn wait_for_absent(&self, id: &SessionId) {
        self.rows
            .as_ref()
            .expect("daemon subscription")
            .wait(|rows| (!rows.iter().any(|r| &r.id == id)).then_some(()));
    }
    pub fn wait(&self, what: &str, mut predicate: impl FnMut() -> bool) {
        let deadline = Instant::now() + DEADLINE;
        while !predicate() {
            assert!(
                Instant::now() < deadline,
                "timed out: {what}\n{}",
                self.diagnostics()
            );
            std::thread::sleep(POLL);
        }
    }
    pub fn diagnostics(&self) -> String {
        let mut out = String::new();
        for entry in std::fs::read_dir(self.root()).unwrap().flatten() {
            if entry
                .path()
                .extension()
                .is_some_and(|x| x == "log" || x == "jsonl")
            {
                out.push_str(&format!(
                    "\n{}:\n{}",
                    entry.path().display(),
                    std::fs::read_to_string(entry.path()).unwrap_or_default()
                ));
            }
        }
        if out.contains("Handshake failed") {
            out.push_str("\nPossible lost hook: daemon logged Handshake failed; journal event entries are send attempts, not acknowledgments.\n");
        }
        if self.tmux_identity.is_some() {
            let panes = self.tmux(&["list-panes", "-a", "-F", "#{pane_id}"]);
            if panes.status.success() {
                for pane in String::from_utf8_lossy(&panes.stdout).lines() {
                    let capture = self.tmux(&["capture-pane", "-p", "-t", pane]);
                    out.push_str(&format!(
                        "\npane {pane}:\n{}",
                        String::from_utf8_lossy(&capture.stdout)
                    ));
                }
            }
        }
        out
    }

    pub fn close(&mut self) {
        self.cleanup().expect("private rig cleanup");
    }
    fn cleanup(&mut self) -> Result<(), String> {
        self.stop_daemons()?;
        if let Some((socket, pid)) = &self.tmux_identity {
            let mut cmd = self.command(Path::new("tmux"));
            cmd.args([
                "-S",
                socket.to_str().ok_or("socket utf8")?,
                "display-message",
                "-p",
                "#{socket_path} #{pid}",
            ]);
            let output = process::run(&mut cmd);
            if !socket.exists() && pid.is_none() {
                self.tmux_identity = None;
                return Ok(());
            }
            let identity = String::from_utf8_lossy(&output.stdout);
            let actual = identity.trim().rsplit_once(' ');
            if !output.status.success()
                || !actual.is_some_and(|(path, actual_pid)| {
                    Path::new(path) == socket
                        && actual_pid.parse::<u32>().is_ok_and(|actual_pid| {
                            pid.is_none_or(|expected| actual_pid == expected)
                        })
                })
                || !socket.starts_with(
                    self.root()
                        .join("tmux")
                        .canonicalize()
                        .map_err(|e| e.to_string())?,
                )
            {
                return Err("refusing to kill tmux: server identity mismatch".into());
            }
            let result = process::run(self.command(Path::new("tmux")).args([
                "-S",
                socket.to_str().ok_or("socket utf8")?,
                "kill-server",
            ]));
            if !result.status.success() {
                return Err(format!("kill private server: {result:?}"));
            }
            self.tmux_identity = None;
        }
        Ok(())
    }
}
impl Drop for StandInRig {
    fn drop(&mut self) {
        // Diagnostics must not double-panic during assertion unwinding.
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.cleanup()))
            .map_or(true, |result| result.is_err())
        {
            eprintln!(
                "private fixture cleanup failed; inspect {}",
                self.root().display()
            );
        }
    }
}

// A synchronous test API with one deadline covering connect, handshake and reply.
pub fn protocol_request(
    path: &Path,
    request: ClientMessage,
    timeout: Duration,
) -> anyhow::Result<DaemonMessage> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        tokio::time::timeout(timeout, async {
            let stream = tokio::net::UnixStream::connect(path).await?;
            let (read, mut write) = stream.into_split();
            let mut reader = BufReader::new(read);
            let mut line = String::new();
            for (index, message) in [
                ClientMessage::connect(Some("stand-in-test".into())),
                request,
            ]
            .into_iter()
            .enumerate()
            {
                write
                    .write_all(format!("{}\n", serde_json::to_string(&message)?).as_bytes())
                    .await?;
                line.clear();
                reader.read_line(&mut line).await?;
                if index == 0
                    && !matches!(
                        serde_json::from_str::<DaemonMessage>(&line)?,
                        DaemonMessage::Connected { .. }
                    )
                {
                    anyhow::bail!("private daemon handshake refused");
                }
            }
            Ok(serde_json::from_str(&line)?)
        })
        .await?
    })
}
