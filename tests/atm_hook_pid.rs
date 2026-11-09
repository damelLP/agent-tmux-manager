//! Exercise the complete atm-hook with real /proc ancestry, without a harness,
//! daemon, or tmux. Only the socket transport is replaced by a recorder.
//!
//! Every fixture has a real process named `claude` above fifteen shell parents.
//! That padding keeps both this sentinel and any real Claude running cargo out
//! of the hook's bounded walk, including on the old outermost-match code.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::process::Command;

use serde_json::{json, Value};

const HOOK: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/crates/atm/scripts/atm-hook");

struct Capture {
    pids: HashMap<String, u32>,
    messages: Vec<Value>,
}

/// `tree` is ordered outermost to innermost; each pair is (comm, label).
fn run_hook(tree: &[(&str, &str)], input: &Value) -> Capture {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    fs::create_dir(&bin).unwrap();
    // Unlike `exec -a`, the executable's basename sets Linux comm. The live
    // processes record comm below, so a platform change fails explicitly.
    for name in ["claude", "sh"] {
        symlink("/bin/bash", bin.join(name)).unwrap();
    }
    let node = dir.path().join("node.sh");
    fs::write(
        &node,
        r#"set -eu
label=$1
shift
read -r comm < "/proc/$$/comm"
printf '%s %s %s %s\n' "$label" "$$" "$PPID" "$comm" >> "$ATM_TEST_PIDS"
if [ "$#" -gt 0 ]; then
    program=$1
    shift
    "$ATM_TEST_BIN/$program" "$ATM_TEST_NODE" "$@"
else
    /bin/bash "$ATM_TEST_HOOK" < "$ATM_TEST_INPUT"
fi
# Keep this shell as the child's direct parent, rather than exec'ing it.
rc=$?
exit "$rc"
"#,
    )
    .unwrap();
    let input_path = dir.path().join("input.json");
    fs::write(&input_path, input.to_string()).unwrap();
    let pid_path = dir.path().join("pids");
    let messages_path = dir.path().join("messages.jsonl");
    let socket_path = dir.path().join("atm.sock");
    let _socket = UnixListener::bind(&socket_path).unwrap();
    let socat = bin.join("socat");
    fs::write(&socat, "#!/bin/sh\ncat > \"$ATM_TEST_MESSAGES\"\n").unwrap();
    fs::set_permissions(&socat, fs::Permissions::from_mode(0o755)).unwrap();

    let mut nodes = vec![("claude".to_owned(), "outside-fixture".to_owned())];
    nodes.extend((0..15).map(|i| ("sh".to_owned(), format!("guard-{i}"))));
    nodes.extend(
        tree.iter()
            .map(|(comm, label)| (comm.to_string(), label.to_string())),
    );
    // timeout owns the process group, so a hung hook also kills its shell
    // ancestors/children. On success each shell waits for and reaps its child.
    let mut command = Command::new("timeout");
    command
        .args(["--kill-after=1s", "10s"])
        .arg(bin.join("claude"))
        .arg(&node)
        .arg("outside-fixture");
    for (comm, label) in nodes.iter().skip(1) {
        command.arg(comm).arg(label);
    }
    let output = command
        .env_clear()
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("ATM_SOCKET", socket_path)
        .env("ATM_TEST_BIN", &bin)
        .env("ATM_TEST_NODE", &node)
        .env("ATM_TEST_HOOK", HOOK)
        .env("ATM_TEST_INPUT", input_path)
        .env("ATM_TEST_PIDS", &pid_path)
        .env("ATM_TEST_MESSAGES", &messages_path)
        .output()
        .expect("run real process tree with timeout");
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");

    let records = fs::read_to_string(pid_path).unwrap();
    assert_eq!(records.lines().count(), nodes.len(), "{records}");
    let mut pids = HashMap::new();
    let mut previous_pid = None;
    for (line, (comm, label)) in records.lines().zip(&nodes) {
        let fields: Vec<_> = line.split_whitespace().collect();
        assert_eq!(fields.len(), 4, "{line}");
        assert_eq!(fields[0], label);
        assert_eq!(
            fields[3], comm,
            "the fixture must set comm, not just argv[0]"
        );
        let pid: u32 = fields[1].parse().unwrap();
        if let Some(parent) = previous_pid {
            assert_eq!(fields[2].parse::<u32>().unwrap(), parent, "{records}");
        }
        previous_pid = Some(pid);
        pids.insert(label.clone(), pid);
    }
    let messages = match fs::read_to_string(messages_path) {
        Ok(text) => text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => panic!("read captured messages: {error}"),
    };
    Capture { pids, messages }
}

fn assert_pid(tree: &[(&str, &str)], expected: Option<&str>) {
    // Both users of find_claude_pid must carry the same selected PID.
    for (message_type, input) in [
        (
            "hook_event",
            json!({"session_id": "pid-test", "hook_event_name": "SessionStart"}),
        ),
        (
            "status_update",
            json!({"session_id": "pid-test", "model": {"id": "test-model"}}),
        ),
    ] {
        let capture = run_hook(tree, &input);
        assert!(capture.pids.contains_key("outside-fixture"));
        let Some(label) = expected else {
            assert!(
                capture.messages.is_empty(),
                "unexpected PID match: {:?}",
                capture.messages
            );
            continue;
        };
        assert_eq!(capture.messages.len(), 2, "{:?}", capture.messages);
        assert_eq!(capture.messages[0]["type"], "connect");
        assert_eq!(capture.messages[1]["type"], message_type);
        let mut expected_data = input;
        expected_data["pid"] = json!(capture.pids[label]);
        assert_eq!(
            capture.messages[1]["data"], expected_data,
            "tree: {tree:?}; pids: {:?}",
            capture.pids
        );
    }
}

#[test]
fn direct_claude_parent() {
    assert_pid(&[("claude", "session")], Some("session"));
}

#[test]
fn skips_shells_before_claude() {
    assert_pid(
        &[("claude", "session"), ("sh", "hook-shell")],
        Some("session"),
    );
}

#[test]
fn adjacent_wrapper_and_claude_report_wrapper() {
    assert_pid(
        &[
            ("claude", "wrapper"),
            ("claude", "session"),
            ("sh", "hook-shell"),
        ],
        Some("wrapper"),
    );
}

#[test]
fn longer_adjacent_claude_run_reports_its_parent() {
    assert_pid(
        &[
            ("claude", "wrapper"),
            ("claude", "middle"),
            ("claude", "session"),
            ("sh", "hook-shell"),
        ],
        Some("wrapper"),
    );
}

#[test]
fn nested_claude_stops_at_shell_boundary() {
    assert_pid(
        &[
            ("claude", "outer"),
            ("sh", "tool-shell"),
            ("claude", "inner"),
            ("sh", "hook-shell"),
        ],
        Some("inner"),
    );
}

#[test]
fn nested_wrappers_report_inner_wrapper() {
    assert_pid(
        &[
            ("claude", "outer-wrapper"),
            ("claude", "outer"),
            ("sh", "tool-shell"),
            ("claude", "inner-wrapper"),
            ("claude", "inner"),
            ("sh", "hook-shell"),
        ],
        Some("inner-wrapper"),
    );
}

#[test]
fn claude_above_fixture_is_outside_walk_even_when_cargo_has_claude_ancestors() {
    // With no test nodes, the fifteen guard shells occupy every visited
    // ancestor. The real sentinel process has comm=claude at depth 15;
    // cargo's own ancestors are still farther away. This must emit nothing.
    assert_pid(&[], None);
}

#[test]
fn claude_at_fifteenth_ancestor_is_found() {
    let mut tree = vec![("claude", "session")];
    let labels: Vec<_> = (0..14).map(|i| format!("shell-{i}")).collect();
    tree.extend(labels.iter().map(|label| ("sh", label.as_str())));
    assert_pid(&tree, Some("session"));
}

#[test]
fn claude_at_sixteenth_ancestor_is_out_of_range() {
    let mut tree = vec![("claude", "session")];
    let labels: Vec<_> = (0..15).map(|i| format!("shell-{i}")).collect();
    tree.extend(labels.iter().map(|label| ("sh", label.as_str())));
    assert_pid(&tree, None);
}
