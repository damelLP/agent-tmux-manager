//! Lifecycle composition over U11b, not a second stand-in or daemon fixture.
//!
//! Ready adoption types the launcher into interactive bash: `rig.launch` uses
//! a noninteractive child/retained-shell shape and is only for refusal tests.
//! Each agent has its own detached window, sized before launch (200x50 by
//! default; styled Claude/Codex starts use 100x30).
//! An `agent.up` or `agent.respawn` is called from a placement window born at
//! twice the stand-in's height plus the border, so the daemon's vertical split
//! leaves the stand-in its size. Never split/resize a running stand-in or attach.
//! Take an event cursor before each action; wait for rendered/row events and
//! release manual-clock gates explicitly. Initial shell readiness and audit-log
//! delivery completion lack subscriptions and use the shared bounded condition
//! poll. Journal observations are assertions, not production readiness evidence
//! or a raw-key transcript.

use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use atm_core::{SessionStatus, SessionView};
use atm_protocol::{ClientMessage, DaemonMessage, ResponsePayload};
use serde_json::{json, Value};

use crate::stand_in::{self, StandInConfig, StandInMode, StandInPane, StandInRig};

pub fn checked(output: Output) -> String {
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout).unwrap().trim().into()
}

/// The configuration of a stand-in that `agent.up` starts. A start is Ready
/// only on an editor the adapter can prove empty, which for Claude and Codex
/// takes the styled captures of a fresh editor: the stand-in has those at
/// 100x30 only (its `rich-enabled` and `rich` variants). Pi needs no style.
pub fn started_config(mode: StandInMode) -> StandInConfig {
    let mut config = config(mode);
    match mode {
        StandInMode::Claude => config.screen_variant = "rich-enabled".into(),
        StandInMode::Codex => config.screen_variant = "rich".into(),
        StandInMode::Pi => return config,
    }
    config.columns = 100;
    config.rows = 30;
    config
}

pub fn config(mode: StandInMode) -> StandInConfig {
    let mut config = StandInConfig::new(mode);
    config.manual_clock = true;
    config.work_ms = 500;
    config.columns = 200;
    config.rows = 50;
    config
}

/// PID plus procfs start time: a reused PID cannot satisfy identity assertions.
pub fn process_identity(pid: u32) -> (u32, String) {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let fields: Vec<_> = stat
        .rsplit_once(')')
        .unwrap()
        .1
        .split_whitespace()
        .collect();
    assert_ne!(fields[0], "Z", "process is a zombie: {pid}");
    (pid, fields[19].to_owned())
}

fn quote(path: &Path) -> String {
    format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"))
}

/// One lifecycle request. The bound is above every lifecycle call's own (a
/// start has 60 s).
fn request(
    socket: &Path,
    method: &str,
    params: Value,
    from_pane: Option<String>,
) -> Result<Value, String> {
    let response = stand_in::protocol_request(
        socket,
        ClientMessage::request("lifecycle".into(), method.into(), params, from_pane),
        Duration::from_secs(90),
    )
    .unwrap();
    match response {
        DaemonMessage::Response { id, payload } => {
            assert_eq!(id, "lifecycle");
            match payload {
                ResponsePayload::Result(value) => Ok(value),
                ResponsePayload::Error(error) => Err(format!("{error:?}")),
            }
        }
        other => Err(format!("{method}: {other:?}")),
    }
}

pub struct AgentRig {
    pub rig: StandInRig,
    pub project: PathBuf,
}

impl AgentRig {
    pub fn start(mode: StandInMode) -> Self {
        Self::start_with_config(mode, "")
    }

    /// As [`Self::start`], with `config` as the daemon's `config.toml`.
    pub fn start_with_config(mode: StandInMode, config: &str) -> Self {
        let mut rig = StandInRig::start();
        let dir = rig.root().join("config/atm");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.toml"), config).unwrap();
        rig.set_daemon_env("ATM_SPAWN_BIN", rig.launcher(mode));
        rig.restart_daemon();
        let project = rig.root().join("project");
        std::fs::create_dir(&project).unwrap();
        let test = Self { rig, project };
        // agent.list waits for the initial reconciliation pass; socket existence
        // alone is not permission to begin a lifecycle mutation.
        assert_eq!(test.rpc("agent.list", json!({}), None), json!([]));
        test
    }

    pub fn rpc(&self, method: &str, params: Value, from_pane: Option<String>) -> Value {
        match self.call(method, params, from_pane) {
            Ok(value) => value,
            Err(error) => panic!("{method}: {error}\n{}", self.rig.diagnostics()),
        }
    }

    /// As [`Self::rpc`], with a refusal as a value: its code and message.
    pub fn call(
        &self,
        method: &str,
        params: Value,
        from_pane: Option<String>,
    ) -> Result<Value, String> {
        request(&self.rig.socket(), method, params, from_pane)
    }

    /// [`Self::call`] on its own thread, for a call the test must act during
    /// (a clear waits for the stand-in's clock, which the test moves).
    pub fn call_in_background(
        &self,
        method: &'static str,
        params: Value,
    ) -> std::thread::JoinHandle<Result<Value, String>> {
        let socket = self.rig.socket();
        std::thread::spawn(move || request(&socket, method, params, None))
    }

    pub fn server(&self) -> String {
        let env = self.rig.tmux_env();
        let path = env.split(',').next().unwrap();
        assert!(Path::new(path).starts_with(self.rig.root().join("tmux").canonicalize().unwrap()));
        path.to_owned()
    }

    pub fn adopt(&self, name: &str, pane: &StandInPane) -> Value {
        self.rpc(
            "agent.adopt",
            json!({
                "project_root": self.project,
                "name": name,
                "pane": pane.pane_id,
                "caller_server": self.server(),
            }),
            None,
        )
    }

    pub fn agent(&self, id: &str) -> Value {
        self.rpc("agent.list", json!({"project_root": self.project}), None)
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["agent_id"] == id)
            .unwrap()
            .clone()
    }

    pub fn tail(&self, mut after_seq: u64) -> Vec<Value> {
        let mut records = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            assert!(
                std::time::Instant::now() < deadline,
                "audit replay did not reach its end"
            );
            let page = self.rpc(
                "log.tail",
                json!({"after_seq": after_seq, "limit": 1000}),
                None,
            );
            let page = page.as_array().unwrap();
            let Some(last) = page.last() else {
                return records;
            };
            let next = last["seq"].as_u64().unwrap();
            assert!(next > after_seq);
            after_seq = next;
            records.extend(page.iter().cloned());
        }
    }

    pub fn cursor(&self) -> u64 {
        self.tail(0)
            .last()
            .map_or(0, |event| event["seq"].as_u64().unwrap())
    }

    pub fn shell_agent(&self, name: &str, config: &StandInConfig) -> StandInPane {
        assert!(name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
        let log = self.rig.root().join(format!("{name}.jsonl"));
        let path = log.with_extension("config.json");
        assert!(
            !path.exists(),
            "a launch name must be unique within the rig"
        );
        std::fs::write(
            &path,
            serde_json::to_vec(&json!({
                "LOG": log, "ID_PREFIX": name,
                "WORK_MS": config.work_ms, "CLEAR_GAP_MS": config.clear_gap_ms,
                "MANUAL_CLOCK": u8::from(config.manual_clock),
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
        // The window must be born at its size. The shell's prompt is printed
        // by the rig's PROMPT_COMMAND hook with PS1 empty; a resize that
        // arrives after it makes readline redraw the empty PS1 over that
        // prompt, and the pane then stays empty. So the size is the server's
        // default before the window exists, as in `StandInRig::launch`.
        checked(self.rig.tmux(&[
            "set-option",
            "-g",
            "default-size",
            &format!("{}x{}", config.columns.max(200), config.rows.max(50)),
        ]));
        // This window contains only its shell. Geometry never changes after
        // input starts the stand-in, even if another agent window is created.
        let pane_id = checked(self.rig.tmux(&[
            "new-window",
            "-d",
            "-P",
            "-F",
            "#{pane_id}",
            "-c",
            self.project.to_str().unwrap(),
            "/bin/bash --noprofile --norc",
        ]));
        let root =
            checked(
                self.rig
                    .tmux(&["display-message", "-p", "-t", &pane_id, "#{pane_pid}"]),
            );
        self.rig.wait("interactive shell prompt with no child", || {
            let children =
                std::fs::read_to_string(format!("/proc/{root}/task/{root}/children")).unwrap();
            let screen = checked(self.rig.tmux(&["capture-pane", "-p", "-t", &pane_id]));
            children.trim().is_empty() && screen.trim_end().ends_with("stand-in-shell$")
        });
        let text = format!(
            "{} --atm-config {}",
            quote(&self.rig.launcher(config.mode)),
            quote(&path)
        );
        checked(self.rig.tmux(&["send-keys", "-l", "-t", &pane_id, &text]));
        checked(self.rig.tmux(&["send-keys", "-t", &pane_id, "Enter"]));
        let pane = self.rig.observe_pane(pane_id, config.mode, log);
        // No StandInPane exists until identity arrives; zero replays only this
        // new process's events (PID + start time), not a prior pane occupant.
        self.rig
            .wait_event(&pane, 0, |event| event["kind"] == "rendered");
        pane
    }

    /// Write role `name` of the rig's project, for the rig's stand-in mode.
    /// `more` is appended to the role's front matter.
    pub fn role(&self, name: &str, mode: StandInMode, more: &str) {
        let roles = self.project.join(".atm/roles");
        std::fs::create_dir_all(&roles).unwrap();
        std::fs::write(
            roles.join(format!("{name}.md")),
            format!("+++\nharness='{}'\n{more}+++\n", mode.name()),
        )
        .unwrap();
    }

    /// A detached window that holds only an idle shell: the pane a start is
    /// called from. It is born twice as tall as `config`'s pane plus the
    /// border, so the start's split leaves the stand-in exactly `config`'s
    /// size, and nothing is ever typed into it.
    pub fn placement(&self, config: &StandInConfig) -> String {
        checked(self.rig.tmux(&[
            "set-option",
            "-g",
            "default-size",
            &format!("{}x{}", config.columns, config.rows * 2 + 1),
        ]));
        let pane_id = checked(self.rig.tmux(&[
            "new-window",
            "-d",
            "-P",
            "-F",
            "#{pane_id}",
            "-c",
            self.project.to_str().unwrap(),
            "/bin/bash --noprofile --norc",
        ]));
        self.rig.wait("the placement shell's prompt", || {
            checked(self.rig.tmux(&["capture-pane", "-p", "-t", &pane_id]))
                .trim_end()
                .ends_with("stand-in-shell$")
        });
        pane_id
    }

    /// What the next stand-in the daemon starts will read: panes inherit the
    /// private tmux server's environment, not the daemon's. Returns its journal.
    pub fn next_stand_in(&self, name: &str, config: &StandInConfig) -> PathBuf {
        let log = self.rig.root().join(format!("{name}.jsonl"));
        for (key, value) in [
            ("LOG", log.to_str().unwrap().to_owned()),
            ("ID_PREFIX", name.to_owned()),
            ("WORK_MS", config.work_ms.to_string()),
            ("CLEAR_GAP_MS", config.clear_gap_ms.to_string()),
            ("MANUAL_CLOCK", u8::from(config.manual_clock).to_string()),
            ("FOLDER_TRUST", u8::from(config.folder_trust).to_string()),
            ("UPDATE_PROMPT", u8::from(config.update_prompt).to_string()),
            (
                "WORKTREE_CHOICE",
                u8::from(config.worktree_choice).to_string(),
            ),
            ("HOOKS_REVIEW", u8::from(config.hooks_review).to_string()),
            (
                "PERMISSION_DIALOG",
                u8::from(config.permission_dialog).to_string(),
            ),
            ("SCREEN_VARIANT", config.screen_variant.clone()),
            ("FOLDER", config.folder.clone().unwrap_or_default()),
        ] {
            checked(self.rig.tmux(&[
                "set-environment",
                "-g",
                &format!("ATM_STANDIN_{key}"),
                &value,
            ]));
        }
        log
    }

    /// `agent.up` for role `name`, called from `from_pane`.
    pub fn up_from(&self, name: &str, from_pane: &str) -> Result<Value, String> {
        self.call(
            "agent.up",
            json!({
                "project_root": self.project,
                "name": name,
                "caller_server": self.server(),
            }),
            Some(from_pane.to_owned()),
        )
    }

    /// The agent tag on `pane_id`; empty when it has none.
    pub fn tag(&self, pane_id: &str) -> String {
        checked(self.rig.tmux(&[
            "show-options",
            "-p",
            "-v",
            "-q",
            "-t",
            pane_id,
            "@atm-agent",
        ]))
    }

    /// The stand-in's journal shows no prompt and no editor input.
    pub fn assert_no_prompt(&self, pane: &StandInPane) {
        self.rig.control(pane, json!({"command": "barrier"}));
        for event in self.rig.journal(pane) {
            assert!(
                !matches!(
                    event["kind"].as_str(),
                    Some("accepted" | "rejected" | "queued" | "selection" | "draft")
                ),
                "unexpected input effect: {event}"
            );
            assert!(
                !matches!(event["event"].as_str(), Some("UserPromptSubmit" | "input")),
                "unexpected prompt: {event}"
            );
            if event["kind"] == "rendered" {
                assert_eq!(event["draft"], "", "unexpected draft: {event}");
            }
        }
    }

    /// Prime an existing conversation without changing its session identity.
    /// The fresh Claude suggestion source at this geometry has no style proof
    /// that it is a placeholder; a completed turn has a genuinely empty editor.
    pub fn complete_fixture_turn(&self, pane: &StandInPane) -> SessionView {
        let original = self.rig.wait_for_row(pane, |r| {
            !r.id.is_pending() && r.status == SessionStatus::Idle
        });
        self.rig.wait_ready(pane, Some(original.id.as_str()));
        let after = self.rig.event_cursor(pane);
        self.rig.paste(pane, "existing fixture conversation");
        self.rig.wait_event(pane, after, |e| {
            e["kind"] == "rendered" && e["state"] == "working" && e["draft"] == ""
        });
        self.rig
            .wait_for_row(pane, |r| r.status == SessionStatus::Working);
        let now = self.rig.control(pane, json!({"command":"barrier"}))["now_ms"]
            .as_u64()
            .unwrap();
        let after = self.rig.event_cursor(pane);
        // This helper consumes config()'s 500ms fixture turn, not wall time.
        let reply = self
            .rig
            .control(pane, json!({"command":"advance_to", "now_ms":now + 500}));
        assert!(reply["error"].is_null(), "{reply}");
        self.rig.wait_event(pane, after, |e| {
            e["kind"] == "rendered" && e["state"] == "idle"
        });
        let row = self
            .rig
            .wait_for_row(pane, |r| r.status == SessionStatus::Idle);
        assert_eq!(row.id, original.id);
        self.rig.wait_ready(pane, Some(row.id.as_str()));
        row
    }

    /// Best-effort input-effect check, paired with a before/after styled screen
    /// comparison. The event-socket barrier does not order queued pty input,
    /// and the journal cannot detect keys the stand-in ignores.
    pub fn assert_no_input_since(&self, pane: &StandInPane, after: u64) {
        self.rig.control(pane, json!({"command": "barrier"}));
        for event in self
            .rig
            .journal(pane)
            .iter()
            .filter(|e| e["event_sequence"].as_u64().unwrap() > after)
        {
            assert!(
                !matches!(
                    event["kind"].as_str(),
                    Some("identity" | "accepted" | "rejected" | "queued" | "selection" | "draft")
                ),
                "unexpected input effect: {event}"
            );
            assert!(
                !matches!(event["event"].as_str(), Some("UserPromptSubmit" | "input")),
                "unexpected prompt: {event}"
            );
            if event["kind"] == "rendered" {
                assert_eq!(event["draft"], "", "unexpected draft: {event}");
            }
        }
    }
}

/// An agent started through `agent.up`, called from a placement window.
pub struct Started {
    /// The audit cursor taken before the call.
    pub cursor: u64,
    /// The caller's pane, and its screen before the call.
    pub placement: String,
    pub placement_screen: String,
    pub result: Value,
    pub id: String,
    pub pane: StandInPane,
}

impl Started {
    /// Write role `name`, start it from a new placement window, and observe
    /// the stand-in the daemon launched. `more` goes into the role.
    pub fn up_with(test: &AgentRig, name: &str, config: &StandInConfig, more: &str) -> Self {
        test.role(name, config.mode, more);
        let log = test.next_stand_in(name, config);
        let placement = test.placement(config);
        let placement_screen = checked(test.rig.tmux(&["capture-pane", "-p", "-t", &placement]));
        let cursor = test.cursor();
        let result = match test.up_from(name, &placement) {
            Ok(result) => result,
            Err(error) => panic!(
                "agent.up: {error}\n{}\n{}",
                test.rpc("agent.list", json!({}), None),
                test.rig.diagnostics()
            ),
        };
        let pane_id = result["pane"].as_str().unwrap().to_owned();
        let pane = test.rig.observe_pane(pane_id, config.mode, log);
        Self {
            cursor,
            placement,
            placement_screen,
            id: result["agent_id"].as_str().unwrap().to_owned(),
            result,
            pane,
        }
    }

    pub fn up(test: &AgentRig, name: &str, config: &StandInConfig) -> Self {
        Self::up_with(test, name, config, "")
    }
}
