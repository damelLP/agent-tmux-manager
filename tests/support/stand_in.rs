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
    pub id_prefix: String,
}
impl StandInConfig {
    pub fn new(mode: StandInMode) -> Self {
        Self {
            mode,
            work_ms: 500,
            clear_gap_ms: 100,
            id_prefix: "stand-in".into(),
        }
    }
}

#[derive(Debug)]
pub struct StandInPane {
    pub pane_id: String,
    pub pid: u32,
    pub launcher: PathBuf,
    pub log: PathBuf,
    owner: PathBuf,
}

pub struct StandInRig {
    dir: tempfile::TempDir,
    label: String,
    tmux_identity: Option<(PathBuf, u32)>,
    daemon: Option<Child>,
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
            dir,
            label: "stand-in".into(),
            tmux_identity: None,
            daemon: None,
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
            checked(bounded::run(rig.command(Path::new(tool)).arg(flag)));
        }
        for mode in [StandInMode::Claude, StandInMode::Codex, StandInMode::Pi] {
            // A script path ending in the vendor name is visible to discovery;
            // the Python process itself sets comm before starting the forwarder.
            std::os::unix::fs::symlink(Path::new(SOURCE).join("harness.py"), rig.launcher(mode))
                .unwrap();
        }
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
        rig.tmux_identity = Some((socket.into(), pid.parse().unwrap()));
        assert!(Path::new(socket).starts_with(rig.dir.path().join("tmux")));
        checked(rig.tmux(&["set-option", "-g", "default-shell", "/bin/bash"]));
        checked(rig.tmux(&[
            "set-option",
            "-g",
            "default-command",
            "/bin/bash --noprofile --norc",
        ]));
        let log = std::fs::File::create(rig.dir.path().join("daemon.log")).unwrap();
        rig.daemon = Some(
            rig.command(Path::new(env!("CARGO_BIN_EXE_atmd")))
                .arg("start")
                .stdout(log.try_clone().unwrap())
                .stderr(log)
                .spawn()
                .expect("start private atmd"),
        );
        rig.wait("daemon socket", || {
            rig.socket()
                .metadata()
                .is_ok_and(|m| m.file_type().is_socket())
        });
        rig
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
        format!("{},{pid},0", socket.display())
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
            .env("ATM_DEBUG", "0")
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .current_dir(self.root());
        cmd
    }
    pub fn tmux(&self, args: &[&str]) -> Output {
        bounded::run(
            self.command(Path::new("tmux"))
                .args(["-L", &self.label])
                .args(args),
        )
    }
    pub fn atm(&self, args: &[&str]) -> Output {
        bounded::run(
            self.command(Path::new(env!("CARGO_BIN_EXE_atm")))
                .args(args),
        )
    }
    pub fn run(&self, command: &mut Command) -> Output {
        bounded::run(command)
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
        let command = format!(
            "export ATM_STANDIN_WORK_MS={} ATM_STANDIN_CLEAR_GAP_MS={} ATM_STANDIN_ID_PREFIX={} ATM_STANDIN_LOG={}; {}; exec /bin/bash --noprofile --norc",
            config.work_ms, config.clear_gap_ms, quote(&prefix), quote(log.to_str().unwrap()),
            quote(self.launcher(config.mode).to_str().unwrap()));
        let pane_id = checked(self.tmux(&[
            "new-window",
            "-d",
            "-P",
            "-F",
            "#{pane_id}",
            &format!("/bin/bash --noprofile --norc -c {}", quote(&command)),
        ]));
        let pane = self.observe_pane(pane_id, config.mode, log);
        self.wait_ready(&pane, None);
        pane
    }

    /// Observe a stand-in started by atm spawn; unlike launch, it may already be working.
    pub fn observe_pane(&self, pane_id: String, mode: StandInMode, log: PathBuf) -> StandInPane {
        assert_eq!(
            log.parent(),
            Some(self.root()),
            "journal must be in this rig"
        );
        let mut pane = StandInPane {
            pane_id,
            pid: 0,
            launcher: self.launcher(mode),
            log,
            owner: self.root().into(),
        };
        self.wait("stand-in identity", || {
            let identity = self.journal(&pane).into_iter().rev().find_map(|e| {
                if e["kind"] != "identity" || e["pane"] != pane.pane_id || e["mode"] != mode.name()
                {
                    return None;
                }
                let pid = u32::try_from(e["pid"].as_u64()?).ok()?;
                let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
                let start_time = stat.rsplit_once(')')?.1.split_whitespace().nth(19)?;
                (Some(start_time) == e["start_time"].as_str()).then_some(pid)
            });
            if let Some(pid) = identity {
                pane.pid = pid;
            }
            identity.is_some()
        });
        pane
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
        checked(self.tmux(&["capture-pane", "-p", "-t", &pane.pane_id]))
    }
    pub fn wait_ready(&self, pane: &StandInPane, session_id: Option<&str>) {
        self.wait("stand-in ready", || {
            self.journal(pane)
                .iter()
                .rev()
                .find(|e| e["kind"] == "render")
                .is_some_and(|e| {
                    e["state"] == "idle" && session_id.is_none_or(|id| e["session_id"] == id)
                })
                && self.capture(pane).contains(": idle")
                && checked(self.tmux(&[
                    "display-message",
                    "-p",
                    "-t",
                    &pane.pane_id,
                    "#{bracket_paste_flag}",
                ])) == "1"
        });
    }
    pub fn wait_for_shell(&self, pane: &StandInPane) {
        self.wait("stand-in exited to shell", || {
            !Path::new(&format!("/proc/{}", pane.pid)).exists()
                && checked(self.tmux(&[
                    "display-message",
                    "-p",
                    "-t",
                    &pane.pane_id,
                    "#{pane_current_command}",
                ])) == "bash"
        });
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
        let mut result = None;
        self.wait("session row", || {
            result = self.pane_rows(pane).into_iter().find(|r| predicate(r));
            result.is_some()
        });
        result.unwrap()
    }
    pub fn wait_for_absent(&self, id: &SessionId) {
        self.wait("session absent", || {
            !self.rows().iter().any(|r| &r.id == id)
        });
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
    fn diagnostics(&self) -> String {
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
        if let Some(child) = &mut self.daemon {
            if child.try_wait().map_err(|e| e.to_string())?.is_none() {
                // Child is unreaped: its PID cannot have been recycled.
                let env = std::fs::read(format!("/proc/{}/environ", child.id()))
                    .map_err(|e| e.to_string())?;
                let expected = format!("ATM_SOCKET={}", self.dir.path().join("atm.sock").display());
                if !env.split(|b| *b == 0).any(|s| s == expected.as_bytes()) {
                    return Err("refusing to signal daemon with foreign environment".into());
                }
                unsafe {
                    libc::kill(child.id() as i32, libc::SIGTERM);
                }
                let deadline = Instant::now() + Duration::from_secs(2);
                while child.try_wait().map_err(|e| e.to_string())?.is_none() {
                    if Instant::now() >= deadline {
                        child.kill().map_err(|e| e.to_string())?;
                        break;
                    }
                    std::thread::sleep(POLL);
                }
                child.wait().map_err(|e| e.to_string())?;
            }
            self.daemon = None;
        }
        if let Some((socket, pid)) = &self.tmux_identity {
            let mut cmd = self.command(Path::new("tmux"));
            cmd.args([
                "-S",
                socket.to_str().ok_or("socket utf8")?,
                "display-message",
                "-p",
                "#{socket_path} #{pid}",
            ]);
            let output = bounded::run(&mut cmd);
            let expected = format!("{} {pid}", socket.display());
            if !output.status.success()
                || String::from_utf8_lossy(&output.stdout).trim() != expected
                || !socket.starts_with(self.root().join("tmux"))
            {
                return Err("refusing to kill tmux: server identity mismatch".into());
            }
            let result = bounded::run(self.command(Path::new("tmux")).args([
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
fn protocol_request(
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

#[test]
fn startup_diagnostics_include_private_pane_errors() {
    let mut rig = StandInRig::start();
    let pane = checked(rig.tmux(&[
        "new-window",
        "-d",
        "-P",
        "-F",
        "#{pane_id}",
        "printf 'stand-in-startup-error\\n'; exec /bin/bash --noprofile --norc",
    ]));
    rig.wait("error marker", || {
        checked(rig.tmux(&["capture-pane", "-p", "-t", &pane])).contains("stand-in-startup-error")
    });
    assert!(rig.diagnostics().contains("stand-in-startup-error"));
    rig.close();
}

#[test]
fn protocol_request_is_bounded_when_listener_never_answers() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("silent.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let before = Instant::now();
    let error = protocol_request(
        &socket,
        ClientMessage::list_sessions(),
        Duration::from_millis(30),
    )
    .expect_err("a silent peer must time out");
    assert!(error.to_string().contains("deadline"), "{error}");
    assert!(before.elapsed() < Duration::from_secs(2));
}

use std::os::unix::fs::FileTypeExt;
