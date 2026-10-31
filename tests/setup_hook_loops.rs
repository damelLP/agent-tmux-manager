//! What `atm setup` and `atm uninstall` print and write for each hook event,
//! for Claude Code and Codex: the whole stdout, the error text and the
//! file's bytes. The event lists are written out, so adding an event to an
//! adapter means adding it here too.
//!
//! Every run gets its own HOME and XDG dirs in a temp dir, so nothing
//! reaches the real harness configs, a daemon or tmux.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)] // test code may panic

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{json, Map, Value};

/// The facts that differ between the two harnesses' hook loops.
struct Harness {
    dir: &'static str,
    file: &'static str,
    script: &'static str,
    other_script: &'static str,
    events: &'static [&'static str],
    not_object: &'static str,
    /// Setup's "Detected coding agents" lines with only this harness present.
    detected: &'static str,
    title: &'static str,
    /// What setup prints after the event lines when it changed the file.
    written: &'static str,
    /// What setup prints after the event lines when nothing changed.
    unchanged: &'static str,
}

const CLAUDE: Harness = Harness {
    dir: ".claude",
    file: ".claude/settings.json",
    script: "atm-hook",
    other_script: "atm-codex-hook",
    events: &[
        "PreToolUse",
        "PostToolUse",
        "PostToolUseFailure",
        "UserPromptSubmit",
        "Stop",
        "StopFailure",
        "SubagentStart",
        "SubagentStop",
        "SessionStart",
        "SessionEnd",
        "PreCompact",
        "Setup",
        "Notification",
        "PermissionRequest",
        "TeammateIdle",
        "TaskCreated",
        "TaskCompleted",
    ],
    not_object: "hooks is not an object",
    detected: "  ✓ Claude Code  (~/.claude/)\n  \
               ✗ pi           (~/.pi/agent/ not present)\n  \
               ✗ Codex CLI    (~/.codex/ not present)\n",
    title: "Claude Code",
    written: "    statusLine - added\n  Claude Code configuration written.\n",
    unchanged: "    statusLine - already configured\n  Claude Code already configured.\n",
};

const CODEX: Harness = Harness {
    dir: ".codex",
    file: ".codex/hooks.json",
    script: "atm-codex-hook",
    other_script: "atm-hook",
    events: &[
        "PreToolUse",
        "PostToolUse",
        "PermissionRequest",
        "UserPromptSubmit",
        "Stop",
        "SubagentStart",
        "SubagentStop",
        "SessionStart",
        "SessionEnd",
        "PreCompact",
        "PostCompact",
    ],
    not_object: "hooks is not an object in codex hooks.json",
    detected: "  ✗ Claude Code  (~/.claude/ not present)\n  \
               ✗ pi           (~/.pi/agent/ not present)\n  \
               ✓ Codex CLI    (~/.codex/)\n",
    title: "Codex CLI",
    written: "  Codex configuration written.\n\n  \
              IMPORTANT: Codex requires one-time trust approval for hooks\n  \
              it did not create. Launch codex and run: /hooks\n  \
              then approve the atm-codex-hook entries when prompted.\n",
    unchanged: "  Codex already configured.\n",
};

impl Harness {
    /// A temp home holding only this harness's directory, so setup and
    /// uninstall detect only this harness.
    fn home(&self) -> tempfile::TempDir {
        let home = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(home.path().join(self.dir)).unwrap();
        home
    }

    fn path(&self, home: &Path) -> PathBuf {
        home.join(self.file)
    }

    fn script_path(&self, home: &Path) -> String {
        script_path(home, self.script)
    }

    /// The entry setup writes for `event`.
    fn entry(&self, home: &Path, event: &str) -> Value {
        let command = self.script_path(home);
        if self.script == CODEX.script {
            return json!({"hooks": [{"type": "command", "command": command, "timeout": 5}]});
        }
        let hooks = json!([{"type": "command", "command": command}]);
        match event {
            "PreToolUse" | "PostToolUse" | "PostToolUseFailure" | "PermissionRequest" => {
                json!({"matcher": "*", "hooks": hooks})
            }
            _ => json!({"hooks": hooks}),
        }
    }

    /// The whole file setup writes into an empty home.
    fn first_install(&self, home: &Path) -> Value {
        let hooks: Map<String, Value> = self
            .events
            .iter()
            .map(|event| ((*event).to_string(), json!([self.entry(home, event)])))
            .collect();
        let mut settings = json!({"hooks": hooks});
        if self.script == CLAUDE.script {
            settings["statusLine"] = json!({"type": "command", "command": self.script_path(home)});
        }
        settings
    }

    /// Setup's stdout up to and including the install of the hook script.
    fn setup_stdout_start(&self, home: &Path) -> String {
        format!(
            "Setting up ATM...\n\nDetected coding agents:\n{}\nConfiguring {}...\n  \
             Installing hook script to {}... done\n",
            self.detected,
            self.title,
            self.script_path(home)
        )
    }

    /// Setup's whole stdout: `event_lines`, then `tail`, around the fixed
    /// lines. `config` is the outcome printed for atm's config.toml.
    fn setup_stdout(&self, home: &Path, event_lines: &str, tail: &str, config: &str) -> String {
        let config_dir = home.join(".config/atm");
        format!(
            "{}{event_lines}{tail}\nInstalled tmux bindings: {bindings}\n\
             Add to your .tmux.conf: source-file {bindings}\n\n\
             Writing atm config to {config_path}... {config}\n\nNext step:\n  Run: atm\n",
            self.setup_stdout_start(home),
            bindings = config_dir.join("tmux-bindings.conf").display(),
            config_path = config_dir.join("config.toml").display(),
        )
    }

    /// Uninstall's whole stdout, with `hook_lines` as the output of this
    /// harness's hook loop, in a home where this is the only harness.
    /// `status_line` is the line about ATM's statusLine, printed for any home.
    /// `script_outcome` is printed for this harness's hook script.
    fn uninstall_stdout(
        &self,
        home: &Path,
        hook_lines: &str,
        status_line: &str,
        script_outcome: &str,
    ) -> String {
        let claude = script_path(home, CLAUDE.script);
        let codex = script_path(home, CODEX.script);
        let (claude_lines, claude_script, codex_section, codex_script) =
            if self.script == CLAUDE.script {
                (hook_lines, script_outcome, String::new(), "not found")
            } else {
                (
                    "  No hooks found\n",
                    "not found",
                    format!("\nRemoving Codex hooks...\n{hook_lines}"),
                    script_outcome,
                )
            };
        format!(
            "Uninstalling ATM...\n\nRemoving Claude Code hooks...\n{claude_lines}\n{status_line}\n\n\
             Removing hook script {claude}... {claude_script}\n{codex_section}\n\
             Removing codex hook script {codex}... {codex_script}\n\n\
             ATM uninstalled successfully!\n"
        )
    }
}

/// Runs `atm <cmd>` with HOME as the isolated home directory.
fn run_atm(home: &Path, cmd: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_atm"))
        .arg(cmd)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env_remove("TMUX")
        .output()
        .expect("run atm")
}

/// Runs `atm <cmd>`, asserting success and an empty stderr, and returns
/// its stdout.
fn run_atm_ok(home: &Path, cmd: &str) -> String {
    let output = run_atm(home, cmd);
    assert!(output.status.success(), "{cmd}: {output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stderr), "", "{cmd}");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn script_path(home: &Path, script: &str) -> String {
    home.join(".local/bin").join(script).display().to_string()
}

/// The file's content as pretty JSON, which is what setup and uninstall
/// write when they write at all.
fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap()
}

/// `value` as compact JSON with a trailing newline. Setup and uninstall
/// write neither, so a file holding these bytes shows it was never rewritten.
fn unwritable_by_atm(value: &Value) -> String {
    format!("{value}\n")
}

fn event_lines(events: &[&str], outcome: &str) -> String {
    events
        .iter()
        .map(|event| format!("    {event} - {outcome}\n"))
        .collect()
}

fn user_hook() -> Value {
    json!({"type": "command", "command": "/usr/bin/my-own-hook"})
}

fn assert_first_install(harness: &Harness) {
    let home = harness.home();
    let home = home.path();

    let stdout = run_atm_ok(home, "setup");

    assert_eq!(
        stdout,
        harness.setup_stdout(
            home,
            &event_lines(harness.events, "added"),
            harness.written,
            "created"
        )
    );
    assert_eq!(
        fs::read_to_string(harness.path(home)).unwrap(),
        pretty(&harness.first_install(home))
    );
}

#[test]
fn claude_first_install_adds_every_event() {
    assert_first_install(&CLAUDE);
}

#[test]
fn codex_first_install_adds_every_event() {
    assert_first_install(&CODEX);
}

/// A second setup reports every event as configured and doesn't write. The
/// file is reformatted between the runs, so a rewrite would show.
fn assert_second_install(harness: &Harness) {
    let home = harness.home();
    let home = home.path();
    run_atm_ok(home, "setup");
    let bytes = unwritable_by_atm(&harness.first_install(home));
    fs::write(harness.path(home), &bytes).unwrap();

    let stdout = run_atm_ok(home, "setup");

    assert_eq!(
        stdout,
        harness.setup_stdout(
            home,
            &event_lines(harness.events, "already configured"),
            harness.unchanged,
            "already present (preserved)"
        )
    );
    assert_eq!(fs::read_to_string(harness.path(home)).unwrap(), bytes);
}

#[test]
fn claude_second_install_changes_nothing() {
    assert_second_install(&CLAUDE);
}

#[test]
fn codex_second_install_changes_nothing() {
    assert_second_install(&CODEX);
}

/// Setup over a file with some of the entries already there: it appends
/// ATM's entry after a user's, leaves an entry already running ATM's script
/// (from anywhere) alone, and touches nothing outside the event list.
fn assert_install_adds_only_what_is_missing(harness: &Harness) {
    let home = harness.home();
    let home = home.path();
    let user_entry = json!({"matcher": "Bash", "hooks": [user_hook()]});
    let moved_entry = json!({"hooks": [{"type": "command", "command": format!("/elsewhere/{}", harness.script)}]});
    let seed = json!({
        "userKey": "kept",
        "hooks": {
            "PreToolUse": [user_entry],
            "Stop": [moved_entry],
            "CustomEvent": [user_entry],
        },
    });
    fs::write(harness.path(home), pretty(&seed)).unwrap();

    let stdout = run_atm_ok(home, "setup");

    let mut expected = harness.first_install(home);
    expected["userKey"] = json!("kept");
    expected["hooks"]["PreToolUse"] = json!([user_entry, harness.entry(home, "PreToolUse")]);
    expected["hooks"]["Stop"] = json!([moved_entry]);
    expected["hooks"]["CustomEvent"] = json!([user_entry]);
    let lines: String = harness
        .events
        .iter()
        .map(|event| {
            let outcome = if *event == "Stop" {
                "already configured"
            } else {
                "added"
            };
            format!("    {event} - {outcome}\n")
        })
        .collect();
    assert_eq!(
        stdout,
        harness.setup_stdout(home, &lines, harness.written, "created")
    );
    assert_eq!(
        fs::read_to_string(harness.path(home)).unwrap(),
        pretty(&expected)
    );
}

#[test]
fn claude_install_adds_only_what_is_missing() {
    assert_install_adds_only_what_is_missing(&CLAUDE);
}

#[test]
fn codex_install_adds_only_what_is_missing() {
    assert_install_adds_only_what_is_missing(&CODEX);
}

/// Setup fails with exit 1 and exactly `error`, printing exactly `stdout`
/// and leaving the file byte-identical.
fn assert_setup_fails(harness: &Harness, home: &Path, seed: &str, stdout: &str, error: &str) {
    fs::write(harness.path(home), seed).unwrap();

    let output = run_atm(home, "setup");

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), stdout);
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        format!("Error: {error}\n")
    );
    assert_eq!(fs::read_to_string(harness.path(home)).unwrap(), seed);
}

fn assert_hooks_not_an_object_fails(harness: &Harness) {
    let home = harness.home();
    let home = home.path();
    assert_setup_fails(
        harness,
        home,
        "{\"hooks\": []}\n",
        &harness.setup_stdout_start(home),
        harness.not_object,
    );
}

#[test]
fn claude_setup_fails_when_hooks_is_not_an_object() {
    assert_hooks_not_an_object_fails(&CLAUDE);
}

#[test]
fn codex_setup_fails_when_hooks_is_not_an_object() {
    assert_hooks_not_an_object_fails(&CODEX);
}

/// A non-array event stops the loop there: the events before it were
/// printed as added, but nothing reaches the file.
fn assert_event_not_an_array_fails(harness: &Harness) {
    let home = harness.home();
    let home = home.path();
    let before_stop = harness
        .events
        .iter()
        .position(|event| *event == "Stop")
        .unwrap();
    let stdout = format!(
        "{}{}",
        harness.setup_stdout_start(home),
        event_lines(&harness.events[..before_stop], "added")
    );
    assert_setup_fails(
        harness,
        home,
        "{\"hooks\": {\"Stop\": {}}}\n",
        &stdout,
        "hook type is not an array",
    );
}

#[test]
fn claude_setup_fails_when_an_event_is_not_an_array() {
    assert_event_not_an_array_fails(&CLAUDE);
}

#[test]
fn codex_setup_fails_when_an_event_is_not_an_array() {
    assert_event_not_an_array_fails(&CODEX);
}

/// Uninstall removes ATM's commands from the listed events and nothing
/// else, drops the arrays that leaves empty, and prints one line per event
/// however many commands left it.
fn assert_uninstall_removes_only_atms_commands(harness: &Harness) {
    let home = harness.home();
    let home = home.path();
    run_atm_ok(home, "setup");
    let atm = json!({"type": "command", "command": harness.script_path(home)});
    let atm_entry = json!({"hooks": [atm]});
    let user_entry = json!({"hooks": [user_hook()]});
    let seed = json!({
        "userKey": "kept",
        "hooks": {
            // ATM's command beside a user's, and an entry of the user's.
            "PreToolUse": [{"matcher": "Bash", "hooks": [atm, user_hook()]}, user_entry],
            // Two of ATM's entries: one line, and nothing left.
            "PostToolUse": [atm_entry, atm_entry],
            // Only the user's.
            "SessionStart": [user_entry],
            // Already empty: dropped too, once the file is written.
            "PreCompact": [],
            // Not an array, or not an event ATM installs: left alone.
            "SessionEnd": {"userShape": true},
            "CustomEvent": [atm_entry],
            "Stop": [atm_entry],
        },
    });
    fs::write(harness.path(home), pretty(&seed)).unwrap();

    let stdout = run_atm_ok(home, "uninstall");

    let removed = "  PreToolUse - removed\n  PostToolUse - removed\n  Stop - removed\n";
    assert_eq!(
        stdout,
        harness.uninstall_stdout(home, removed, "statusLine - not configured by atm", "done")
    );
    let expected = json!({
        "userKey": "kept",
        "hooks": {
            "PreToolUse": [{"matcher": "Bash", "hooks": [user_hook()]}, user_entry],
            "SessionStart": [user_entry],
            "SessionEnd": {"userShape": true},
            "CustomEvent": [atm_entry],
        },
    });
    assert_eq!(
        fs::read_to_string(harness.path(home)).unwrap(),
        pretty(&expected)
    );
}

#[test]
fn claude_uninstall_removes_only_atms_commands() {
    assert_uninstall_removes_only_atms_commands(&CLAUDE);
}

#[test]
fn codex_uninstall_removes_only_atms_commands() {
    assert_uninstall_removes_only_atms_commands(&CODEX);
}

/// With no ATM command in the listed events, uninstall says so and leaves
/// the file byte-identical, whatever else the file holds.
fn assert_uninstall_finds_nothing(harness: &Harness) {
    let user_entry = json!({"hooks": [user_hook()]});
    let lookalike = format!("/x/{}-wrapper.sh", harness.script);
    let lookalike_entry = json!({"hooks": [{"type": "command", "command": lookalike}]});
    let other_harness_command = format!("/x/{}", harness.other_script);
    let other_harness_entry =
        json!({"hooks": [{"type": "command", "command": other_harness_command}]});
    let seeds = [
        json!({"userKey": "kept"}),
        json!({"hooks": []}),
        json!({"hooks": {}}),
        json!({"hooks": {"PreToolUse": {"userShape": true}}}),
        json!({"hooks": {"PreToolUse": []}}),
        json!({"hooks": {"PreToolUse": [user_entry]}}),
        json!({"hooks": {"PreToolUse": [lookalike_entry]}}),
        json!({"hooks": {"PreToolUse": [other_harness_entry]}}),
    ];
    for seed in seeds {
        let home = harness.home();
        let home = home.path();
        let bytes = unwritable_by_atm(&seed);
        fs::write(harness.path(home), &bytes).unwrap();

        let stdout = run_atm_ok(home, "uninstall");

        assert_eq!(
            stdout,
            harness.uninstall_stdout(
                home,
                "  No hooks found\n",
                "statusLine - not configured by atm",
                "not found"
            ),
            "{seed}"
        );
        assert_eq!(
            fs::read_to_string(harness.path(home)).unwrap(),
            bytes,
            "{seed}"
        );
    }
}

#[test]
fn claude_uninstall_with_nothing_to_remove_changes_nothing() {
    assert_uninstall_finds_nothing(&CLAUDE);
}

#[test]
fn codex_uninstall_with_nothing_to_remove_changes_nothing() {
    assert_uninstall_finds_nothing(&CODEX);
}

/// The hook loop edits the settings in memory even when it removes no
/// command. The statusLine step's write then persists that edit, so an
/// already-empty array goes when only the statusLine changes.
#[test]
fn claude_status_line_write_persists_the_empty_arrays_the_hook_loop_dropped() {
    let home = CLAUDE.home();
    let home = home.path();
    let seed = json!({
        "hooks": {"PreToolUse": []},
        "statusLine": {"type": "command", "command": CLAUDE.script_path(home)},
    });
    fs::write(CLAUDE.path(home), unwritable_by_atm(&seed)).unwrap();

    let stdout = run_atm_ok(home, "uninstall");

    assert_eq!(
        stdout,
        CLAUDE.uninstall_stdout(
            home,
            "  No hooks found\n",
            "statusLine configuration removed",
            "not found"
        )
    );
    assert_eq!(
        fs::read_to_string(CLAUDE.path(home)).unwrap(),
        pretty(&json!({"hooks": {}}))
    );
}
