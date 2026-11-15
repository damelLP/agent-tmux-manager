//! Private task CLI peers and committed-state fixtures. No harness is launched.
use serde_json::{json, Value};
use std::{
    path::Path,
    process::{Output, Stdio},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    process::Command,
};

pub fn command(dir: &Path, args: &[&str], pane: Option<&str>) -> Command {
    use std::os::unix::fs::PermissionsExt;
    for name in ["home", "bin", "state", "config", "tmux"] {
        std::fs::create_dir_all(dir.join(name)).unwrap();
    }
    for name in ["atmd", "tmux"] {
        let path = dir.join("bin").join(name);
        std::fs::write(
            &path,
            "#!/bin/sh\nprintf fallback >> \"$ATM_TASK_FALLBACK\"\nexit 99\n",
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
        .env("ATM_TMUX_SOCKET", "task-cli")
        .env("ATM_AGENT", "wrong/identity")
        .env("ATM_TASK_FALLBACK", dir.join("fallback"))
        .env("PATH", dir.join("bin"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(pane) = pane {
        command.env("TMUX_PANE", pane);
    }
    command
}

pub async fn exchange(
    args: &[&str],
    method: &str,
    params: Value,
    reply: Value,
    pane: Option<&str>,
    advertised: bool,
    broken_stdout: bool,
) -> Output {
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    let dir = tempfile::tempdir().unwrap();
    let listener = UnixListener::bind(dir.path().join("rpc.sock")).unwrap();
    let method = method.to_owned();
    let pane = pane.map(str::to_owned);
    let caller_pane = pane.clone();
    let seen = Arc::new(AtomicBool::new(false));
    let observed = seen.clone();
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
            observed.store(true, Ordering::SeqCst);
            break (read, write);
        };
        let methods = if advertised {
            json!([method])
        } else {
            json!([])
        };
        write.write_all(format!("{}\n", json!({"type":"connected","protocol_version":{"major":1,"minor":1},"client_id":"task-cli","methods":methods})).as_bytes()).await.unwrap();
        let mut line = String::new();
        if advertised {
            read.read_line(&mut line).await.unwrap();
            let mut expected = json!({"type":"request","protocol_version":{"major":1,"minor":1},"id":"r1","method":method,"params":params});
            if let Some(pane) = caller_pane {
                expected["from_pane"] = json!(pane);
            }
            assert_eq!(serde_json::from_str::<Value>(&line).unwrap(), expected);
            let mut response = json!({"type":"response","id":"r1"});
            response
                .as_object_mut()
                .unwrap()
                .extend(reply.as_object().unwrap().clone());
            write
                .write_all(format!("{response}\n").as_bytes())
                .await
                .unwrap();
            line.clear();
        }
        assert_eq!(read.read_line(&mut line).await.unwrap(), 0, "only one RPC");
        listener
    });
    let mut cmd = command(dir.path(), args, pane.as_deref());
    if broken_stdout {
        cmd.stdout(Stdio::from(
            std::fs::OpenOptions::new()
                .write(true)
                .open("/dev/full")
                .unwrap(),
        ));
    }
    let output = tokio::time::timeout(
        Duration::from_secs(60),
        cmd.spawn().unwrap().wait_with_output(),
    )
    .await
    .unwrap()
    .unwrap();
    if !seen.load(Ordering::SeqCst) {
        peer.abort();
        panic!(
            "CLI never connected: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let listener = tokio::time::timeout(Duration::from_secs(60), peer)
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(listener.into_std().unwrap().accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
        "no retry connection"
    );
    assert!(
        !dir.path().join("fallback").exists(),
        "no tmux/local daemon fallback"
    );
    output
}

pub fn contracts() -> Vec<Value> {
    let folder = Path::new(env!("CARGO_MANIFEST_DIR")).join("crates/atmd/tests/fixtures/tasks");
    let mut paths: Vec<_> = std::fs::read_dir(folder)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            let mut value: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            value["name"] = json!(path.file_stem().unwrap().to_str().unwrap());
            value
        })
        .collect()
}

pub async fn method_answer(fixture: &Value, cli_args: Option<&[&str]>) -> Value {
    use atm_org::{OrgCommand, TaskActor, TaskCommand, TaskCreated, TaskKind, TaskSubmission};
    use atmd::{
        org::{agents::AgentHandle, messages::MessageHandle, OrgHandle},
        registry::spawn_registry,
        server::{methods::MethodServices, DaemonServer},
    };
    use tokio_util::sync::CancellationToken;
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let org = OrgHandle::open(dir.path().join("events"), cancel.clone()).await;
    let now: chrono::DateTime<chrono::Utc> = "2026-01-15T12:00:00Z".parse().unwrap();
    org.execute(
        OrgCommand::RegisterProject {
            project_id: "project".parse().unwrap(),
            root: dir.path().to_str().unwrap().into(),
            move_root: false,
        },
        now,
    )
    .await
    .unwrap();
    for (name, pane) in [
        ("lead", "%1"),
        ("worker", "%2"),
        ("reviewer", "%3"),
        ("other", "%4"),
    ] {
        org.execute(
            OrgCommand::Adopt {
                agent_id: format!("project/{name}").parse().unwrap(),
                pane: pane.into(),
                session_id: None,
            },
            now,
        )
        .await
        .unwrap();
    }
    let created: TaskCreated = serde_json::from_value(json!({"title":"Task","kind":"work","creator":"owner","parent":null,"judges":null,"measure":{"kind":"mechanical","command":"check"},"budget":null,"files":[{"path":"briefs/task.md","kind":"brief","note":null}],"rests_on":null})).unwrap();
    org.execute(OrgCommand::Task(TaskCommand::Create(created.clone())), now)
        .await
        .unwrap();
    let actor = |who: &str| TaskActor {
        who: who.parse().unwrap(),
        lead: who == "project/lead",
    };
    let setup = fixture["setup"].as_str().unwrap_or("");
    if !setup.is_empty() {
        org.execute(
            OrgCommand::Task(TaskCommand::Assign {
                task_id: "t6".parse().unwrap(),
                agent_id: "project/worker".parse().unwrap(),
                by: actor("owner"),
            }),
            now,
        )
        .await
        .unwrap();
    }
    if setup == "submitted" || setup == "review" {
        org.execute(
            OrgCommand::Task(TaskCommand::Submit {
                task_id: "t6".parse().unwrap(),
                what: TaskSubmission::Result,
                files: vec![],
                by: actor("project/worker"),
            }),
            now,
        )
        .await
        .unwrap();
    }
    if setup == "review" {
        let mut review = created;
        review.title = "Review".into();
        review.kind = TaskKind::Review;
        review.judges = Some("t6".parse().unwrap());
        org.execute(OrgCommand::Task(TaskCommand::Create(review)), now)
            .await
            .unwrap();
    }
    let mut services = MethodServices::new(spawn_registry());
    services.org = org;
    services.agents = Some(AgentHandle::disabled("task CLI fixture"));
    services.messages = Some(MessageHandle::disabled("task CLI fixture"));
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
    let server_task = tokio::spawn(run);
    let answer = tokio::time::timeout(Duration::from_secs(60), async {
        if let Some(args) = cli_args {
            let output = command(dir.path(), args, fixture["request"]["from_pane"].as_str())
                .output()
                .await
                .unwrap();
            assert!(output.status.success(), "{args:?}: {output:?}");
            assert!(output.stderr.is_empty());
            return json!({"result":serde_json::from_slice::<Value>(&output.stdout).unwrap()});
        }
        let stream = UnixStream::connect(socket).await.unwrap();
        let (read, mut write) = stream.into_split();
        let mut read = BufReader::new(read);
        write
            .write_all(b"{\"type\":\"connect\",\"protocol_version\":{\"major\":1,\"minor\":1}}\n")
            .await
            .unwrap();
        let mut line = String::new();
        read.read_line(&mut line).await.unwrap();
        let hello: Value = serde_json::from_str(&line).unwrap();
        assert!(hello["methods"]
            .as_array()
            .unwrap()
            .contains(&fixture["request"]["method"]));
        write
            .write_all(format!("{}\n", fixture["request"]).as_bytes())
            .await
            .unwrap();
        line.clear();
        read.read_line(&mut line).await.unwrap();
        let mut answer: Value = serde_json::from_str(&line).unwrap();
        answer.as_object_mut().unwrap().remove("type");
        answer.as_object_mut().unwrap().remove("id");
        answer
    })
    .await
    .unwrap();
    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(60), server_task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(60), control_task)
        .await
        .unwrap()
        .unwrap();
    answer
}
