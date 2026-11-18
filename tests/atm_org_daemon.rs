//! Real foreground atmd + built atm, sealed HOME/socket/state and inert tmux.
//! Readiness is the daemon's listening event; no sleeps or polling deadlines.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]
use atm_org::OrgEvent;
use serde_json::{json, Value};
use std::{os::unix::fs::PermissionsExt, path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
    sync::oneshot,
};

fn isolated(command: &mut Command, root: &Path) {
    command
        .env_clear()
        .env("HOME", root.join("home"))
        .env("SHELL", "/bin/sh")
        .env("PATH", root.join("bin"))
        .env("ATM_SOCKET", root.join("s/atm.sock"))
        .env("ATM_STATE_DIR", root.join("state"))
        .env("XDG_STATE_HOME", root.join("xdg"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("TMPDIR", root.join("tmp"))
        .env("TMUX_TMPDIR", root.join("tmux"))
        .env("ATM_TMUX_SOCKET", "org-smoke")
        .env("ATM_SPAWN_START_LINE", "off")
        .kill_on_drop(true);
}
fn seed(root: &Path) -> u64 {
    let mut records = Vec::new();
    let mut emit = |event: OrgEvent| {
        records.push(json!({"seq":records.len()+1,"ts":"2026-01-15T12:00:00Z","kind":event.kind(),"body":event.to_body()}).to_string());
    };
    for id in ["alpha", "beta", "gamma"] {
        let path = root.join("projects").join(id);
        std::fs::create_dir_all(path.join(".atm/roles")).unwrap();
        emit(OrgEvent::ProjectRegistered {
            project_id: id.parse().unwrap(),
            root: path.to_str().unwrap().into(),
        });
    }
    assert!(!root.join("projects/gamma/.git").exists());
    for (id, manager) in [
        ("alpha/lead", "gamma/chief"),
        ("alpha/worker", "lead"),
        ("beta/broken", "owner"),
        ("gamma/chief", "owner"),
    ] {
        let (project, name) = id.split_once('/').unwrap();
        let role =
            atm_org::parse_role(name, "+++\nharness='pi'\n+++\nSECRET saved role\n").unwrap();
        emit(OrgEvent::AgentUpRequested {
            agent_id: id.parse().unwrap(),
            role,
        });
        emit(OrgEvent::AgentStuck {
            agent_id: id.parse().unwrap(),
            reason: "SECRET startup reason".into(),
        });
        let text = if project == "beta" {
            "+++\nharness=17\n+++\nSECRET invalid role\n".into()
        } else {
            format!("+++\nharness='pi'\nmanager='{manager}'\n+++\nSECRET current role\n")
        };
        std::fs::write(
            root.join("projects")
                .join(project)
                .join(format!(".atm/roles/{name}.md")),
            text,
        )
        .unwrap();
    }
    emit(OrgEvent::TaskCreated(serde_json::from_value(json!({"title":"Triage queue","kind":"work","creator":"owner","parent":null,"judges":null,"measure":{"kind":"mechanical","command":"SECRET task command"},"budget":null,"files":[],"rests_on":null})).unwrap()));
    emit(OrgEvent::MessageSent(serde_json::from_value(json!({"from":"system","to":"owner","kind":"note","body":"SECRET owner body","attachments":["SECRET attachment"],"task":null})).unwrap()));
    let path = root.join("state/log/events");
    std::fs::create_dir_all(&path).unwrap();
    std::fs::write(root.join("state/log/format"), "1\n").unwrap();
    std::fs::write(
        path.join("00000001.jsonl"),
        format!("{}\n", records.join("\n")),
    )
    .unwrap();
    records.len() as u64
}
async fn cli(root: &Path, args: &[&str]) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_atm"));
    isolated(&mut command, root);
    command.args(args);
    tokio::time::timeout(Duration::from_secs(60), command.output())
        .await
        .unwrap()
        .unwrap()
}

// Break caught: real daemon registration/startup differs from the injected
// router, CLI loses raw seq/data, or text joins locally/omits broken/down work.
#[tokio::test]
async fn org_real_daemon_and_cli_json_text_smoke() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for sub in ["home", "bin", "state", "s", "xdg", "config", "tmp", "tmux"] {
        std::fs::create_dir_all(root.join(sub)).unwrap();
    }
    let tmux = root.join("bin/tmux");
    std::fs::write(
        &tmux,
        "#!/bin/sh\n# No model harness or tmux server is reachable from this fixture.\nexit 0\n",
    )
    .unwrap();
    std::fs::set_permissions(tmux, std::fs::Permissions::from_mode(0o700)).unwrap();
    let seed_seq = seed(root);
    let mut command = Command::new(env!("CARGO_BIN_EXE_atmd"));
    isolated(&mut command, root);
    command
        .arg("start")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut daemon = command.spawn().unwrap();
    let pid = daemon.id().unwrap();
    let stdout = daemon.stdout.take().unwrap();
    let stderr = daemon.stderr.take().unwrap();
    let (ready, readiness) = oneshot::channel();
    let log = tokio::spawn(async move {
        let mut ready = Some(ready);
        let mut lines = BufReader::new(stdout).lines();
        let mut log = String::new();
        while let Some(line) = lines.next_line().await.unwrap() {
            if line.contains("Daemon server listening") {
                if let Some(ready) = ready.take() {
                    let _ = ready.send(());
                }
            }
            log.push_str(&line);
            log.push('\n');
        }
        log
    });
    let errors = tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut stderr = stderr;
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).await.unwrap();
        bytes
    });
    tokio::time::timeout(Duration::from_secs(60), readiness)
        .await
        .unwrap()
        .unwrap();
    let output = cli(root, &["org", "--format", "json"]).await;
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    let tree: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(tree["seq"].as_u64().unwrap() >= seed_seq);
    assert_eq!(tree["reporting"]["state"], "ok");
    assert_eq!(tree["projects"].as_array().unwrap().len(), 3);
    assert_eq!(tree["projects"][0]["seats"][0]["manager"], "gamma/chief");
    assert_eq!(tree["projects"][0]["seats"][1]["manager"], "alpha/lead");
    assert_eq!(
        tree["projects"][1]["seats"][0]["line_error"],
        "reporting line at beta/broken: invalid role configuration"
    );
    assert_eq!(tree["unplaced_tasks"][0]["title"], "Triage queue");
    assert!(!tree.to_string().contains("SECRET"));
    let output = cli(root, &["org"]).await;
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.starts_with("Needs you:"));
    assert!(text.contains(" -> gamma/chief"));
    assert!(text.contains("line_error=reporting line at beta/broken: invalid role configuration"));
    assert!(text.contains("Unplaced tasks: 1"));
    assert!(!text.contains("SECRET"));
    // Verify ownership before asking the private binary to signal its PID file.
    let environ = std::fs::read(format!("/proc/{pid}/environ")).unwrap();
    assert!(environ
        .split(|b| *b == 0)
        .any(|v| v == format!("ATM_SOCKET={}", root.join("s/atm.sock").display()).as_bytes()));
    assert_eq!(
        std::fs::read_link(format!("/proc/{pid}/exe")).unwrap(),
        std::fs::canonicalize(env!("CARGO_BIN_EXE_atmd")).unwrap()
    );
    let mut stop = Command::new(env!("CARGO_BIN_EXE_atmd"));
    isolated(&mut stop, root);
    stop.arg("stop");
    let stopped = tokio::time::timeout(Duration::from_secs(90), stop.output())
        .await
        .unwrap()
        .unwrap();
    assert!(stopped.status.success(), "{stopped:?}");
    assert!(tokio::time::timeout(Duration::from_secs(90), daemon.wait())
        .await
        .unwrap()
        .unwrap()
        .success());
    let log = log.await.unwrap();
    let errors = errors.await.unwrap();
    assert!(errors.is_empty(), "{}", String::from_utf8_lossy(&errors));
    assert!(log.contains("Daemon server listening"));
    assert!(!root.join("s/atm.sock").exists());
    println!("private real-atmd/built-atm smoke: 3 projects, cross-project manager, broken line, private sentinels, JSON seq and text; listening-event readiness and confirmed process exit");
}
