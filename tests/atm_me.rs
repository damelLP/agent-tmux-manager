//! Entry command contract: private scripted RPC and a real daemon server, no harness.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::{json, Value};
use std::{
    path::Path,
    process::{Output, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixListener,
    process::Command,
};

#[path = "support/me_tasks.rs"]
mod task_context;

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/atm_me/context.json")).unwrap()
}

fn command(dir: &Path, args: &[&str]) -> Command {
    use std::os::unix::fs::PermissionsExt;
    for path in ["home", "tmux", "state", "bin", "config"] {
        std::fs::create_dir_all(dir.join(path)).unwrap();
    }
    for binary in ["tmux", "atmd"] {
        let path = dir.join("bin").join(binary);
        std::fs::write(
            &path,
            "#!/bin/sh\nprintf 'fallback\\n' >> \"$ATM_ME_FALLBACK\"\nexit 99\n",
        )
        .unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let mut command = Command::new(env!("CARGO_BIN_EXE_atm"));
    command
        .env_clear()
        .args(args)
        .current_dir(dir)
        .env("HOME", dir.join("home"))
        .env("SHELL", "/bin/sh")
        .env("ATM_SOCKET", dir.join("rpc.sock"))
        .env("ATM_STATE_DIR", dir.join("state"))
        .env("XDG_STATE_HOME", dir.join("state"))
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("TMPDIR", dir)
        .env("TMUX_TMPDIR", dir.join("tmux"))
        .env("ATM_TMUX_SOCKET", "me-cli-test")
        .env("TMUX_PANE", "%7")
        .env("ATM_AGENT", "wrong/identity")
        .env("ATM_ME_FALLBACK", dir.join("fallback"))
        .env("PATH", dir.join("bin"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

async fn exchange(args: &[&str], payload: Value, advertised: bool, broken_stdout: bool) -> Output {
    let dir = tempfile::tempdir().unwrap();
    let listener = UnixListener::bind(dir.path().join("rpc.sock")).unwrap();
    let handshake = Arc::new(AtomicBool::new(false));
    let seen = handshake.clone();
    let peer = tokio::spawn(async move {
        let (mut read, mut write) = loop {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, write) = stream.into_split();
            let mut read = BufReader::new(read);
            let mut line = String::new();
            if read.read_line(&mut line).await.unwrap() == 0 {
                continue;
            }
            assert_eq!(
                serde_json::from_str::<Value>(&line).unwrap()["type"],
                "connect"
            );
            seen.store(true, Ordering::SeqCst);
            break (read, write);
        };
        let methods = if advertised {
            json!(["agent.context"])
        } else {
            json!([])
        };
        write.write_all(format!("{}\n", json!({"type":"connected","protocol_version":{"major":1,"minor":1},"client_id":"me-test","methods":methods})).as_bytes()).await.unwrap();
        let mut line = String::new();
        if advertised {
            read.read_line(&mut line).await.unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(&line).unwrap(),
                json!({"protocol_version":{"major":1,"minor":1},"type":"request","id":"r1","method":"agent.context","params":{},"from_pane":"%7"})
            );
            let mut response = json!({"type":"response","id":"r1"});
            response
                .as_object_mut()
                .unwrap()
                .extend(payload.as_object().unwrap().clone());
            write
                .write_all(format!("{response}\n").as_bytes())
                .await
                .unwrap();
            line.clear();
        }
        assert_eq!(
            read.read_line(&mut line).await.unwrap(),
            0,
            "no extra/unsupported RPC"
        );
        std::future::poll_fn(|cx| {
            assert!(
                listener.poll_accept(cx).is_pending(),
                "unexpected retry connection"
            );
            std::task::Poll::Ready(())
        })
        .await;
    });
    let mut command = command(dir.path(), args);
    if broken_stdout {
        let (reader, writer) = std::os::unix::net::UnixStream::pair().unwrap();
        reader.shutdown(std::net::Shutdown::Read).unwrap();
        command.stdout(Stdio::from(std::os::fd::OwnedFd::from(writer)));
    }
    // Command::output replaces configured stdout with a pipe; spawn preserves
    // the intentionally closed stdout used to exercise real write failures.
    let child = command.spawn().unwrap();
    let output = tokio::time::timeout(Duration::from_secs(60), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    if !handshake.load(Ordering::SeqCst) {
        peer.abort();
        panic!(
            "CLI never connected: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    tokio::time::timeout(Duration::from_secs(60), peer)
        .await
        .unwrap()
        .unwrap();
    assert!(
        !dir.path().join("fallback").exists(),
        "no tmux/local fallback"
    );
    output
}

// Break caught: missing entry command, non-actionable output or dropped JSON fields.
#[tokio::test]
async fn me_renders_actionable_text_and_exact_json() {
    for args in [vec!["me"], vec!["me", "--format", "text"]] {
        let output = exchange(&args, json!({"result":fixture()}), true, false).await;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stderr.is_empty());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            include_str!("fixtures/atm_me/context.txt")
        );
    }
    let mut value = fixture();
    value["future_field"] = json!({"keep":"é\u{009b}"});
    let output = exchange(
        &["me", "--format", "json"],
        json!({"result":value}),
        true,
        false,
    )
    .await;
    assert!(output.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        value
    );
    assert!(!String::from_utf8(output.stdout)
        .unwrap()
        .contains('\u{009b}'));
}

// Break caught: a missing role hides inbox/memory/actions or is rendered as invalid.
#[tokio::test]
async fn me_missing_role_is_plain_but_the_rest_is_shown() {
    let mut value = fixture();
    value["instructions"] = Value::Null;
    value["manager"] = json!("owner");
    value["report_command"] =
        json!("atm msg send --to owner --kind report --body 'DONE: <result>'");
    let output = exchange(&["me"], json!({"result":value}), true, false).await;
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains(
        "Instructions: none (no role file at /project/.atm/roles/worker.md); reports go to owner\n"
    ));
    for line in [
        "Agent: project/worker; manager: owner",
        "Memory: /project/.atm/memory/worker.md",
        "Open tasks: none",
        "Unread m7 brief from=owner",
        "Report: atm msg send --to owner --kind report",
    ] {
        assert!(text.contains(line), "{line}: {text}");
    }
}

// Break caught: ignoring refusals/capabilities, rendering malformed data, swallowing write errors.
#[tokio::test]
async fn me_surfaces_refusal_old_daemon_invalid_result_and_broken_stdout() {
    for message in [
        "atm me requires a named agent bound to the caller's pane",
        "invalid role configuration",
        "invalid role manager",
    ] {
        let output = exchange(
            &["me"],
            json!({"error":{"code":"refused","message":message}}),
            true,
            false,
        )
        .await;
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains(message));
    }
    let output = exchange(&["me"], json!({}), false, false).await;
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("daemon is too old"));
    let output = exchange(
        &["me"],
        json!({"result":{"agent_id":"project/worker"}}),
        true,
        false,
    )
    .await;
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    for format in ["text", "json"] {
        let output = exchange(
            &["me", "--format", format],
            json!({"result":fixture()}),
            true,
            true,
        )
        .await;
        assert_eq!(
            output.status.code(),
            Some(1),
            "{format}: stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

// Break caught: positional/identity override arguments accepted, or entry command not discoverable.
#[tokio::test]
async fn me_accepts_no_operands_and_text_or_json_only() {
    let dir = tempfile::tempdir().unwrap();
    for args in [
        vec!["me", "worker"],
        vec!["me", "--for", "project/worker"],
        vec!["me", "--format", "xml"],
    ] {
        let output = command(dir.path(), &args).output().await.unwrap();
        assert_eq!(output.status.code(), Some(2));
    }
    let help = command(dir.path(), &["me", "--help"])
        .output()
        .await
        .unwrap();
    assert!(help.status.success());
    let text = String::from_utf8(help.stdout).unwrap();
    assert!(text.contains("--format"));
    assert!(text.contains("json"));
    assert!(!dir.path().join("fallback").exists());
}

// Break caught: client/server schema drift, missing-role context hiding pending
// work, or the actual daemon-generated report command having incorrect shell argv.
#[tokio::test]
async fn me_real_daemon_context_and_report_command() {
    use atm_org::{Address, MessageContent, MessageKind, OrgCommand};
    use atmd::{
        org::{agents::AgentHandle, messages::MessageHandle, OrgHandle},
        registry::spawn_registry,
        server::{methods::MethodServices, DaemonServer},
    };
    use tokio_util::sync::CancellationToken;
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let org = OrgHandle::open(dir.path().join("events"), cancel.clone()).await;
    for command in [
        OrgCommand::RegisterProject {
            project_id: "project".parse().unwrap(),
            root: dir.path().to_str().unwrap().into(),
            move_root: false,
        },
        OrgCommand::Adopt {
            agent_id: "project/worker".parse().unwrap(),
            pane: "%7".into(),
            session_id: None,
        },
        OrgCommand::SendMessage(MessageContent {
            from: Address::Owner,
            to: "project/worker".parse().unwrap(),
            kind: MessageKind::Brief,
            body: "Pending work".into(),
            attachments: vec![],
            task: None,
        }),
    ] {
        org.execute(command, chrono::Utc::now()).await.unwrap();
    }
    let mut services = MethodServices::new(spawn_registry());
    services.org = org;
    services.agents = Some(AgentHandle::disabled("context fixture"));
    services.messages = Some(MessageHandle::disabled("context fixture"));
    let (control, control_task) = atmd::control::service::spawn_control(
        Box::new(atm_tmux::MockTmuxClient::new()),
        services.registry.clone(),
        services.org.clone(),
        cancel.clone(),
    );
    services.control = Some(control);
    let socket = dir.path().join("rpc.sock");
    let server = DaemonServer::with_services(&socket, services, cancel.clone());
    let mut run = Box::pin(async move { server.run().await });
    std::future::poll_fn(|cx| {
        use std::{future::Future, task::Poll};
        assert!(run.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    let task = tokio::spawn(run);
    let check = async {
        let output = command(dir.path(), &["me"]).output().await.unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(text.contains("Instructions: none (no role file at "));
        assert!(text.contains("; reports go to owner\n"));
        assert!(
            text.contains("Unread m3 brief from=owner: Pending work; read: atm inbox --id m3\n")
        );
        let output = command(dir.path(), &["me", "--format", "json"])
            .output()
            .await
            .unwrap();
        assert!(output.status.success());
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["instructions"], Value::Null);
        assert_eq!(value["open_tasks"], json!([]));
        let shell = format!(
            "atm() {{ printf '%s\\0' \"$@\"; }}; {}",
            value["report_command"].as_str().unwrap()
        );
        let output = Command::new("/bin/sh")
            .env_clear()
            .current_dir(dir.path())
            .arg("-c")
            .arg(shell)
            .output()
            .await
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            output.stdout,
            b"msg\0send\0--to\0owner\0--kind\0report\0--body\0DONE: <result>\0"
        );
        assert!(!dir.path().join("fallback").exists());
    };
    tokio::time::timeout(Duration::from_secs(60), check)
        .await
        .unwrap();
    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(60), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(60), control_task)
        .await
        .unwrap()
        .unwrap();
}
