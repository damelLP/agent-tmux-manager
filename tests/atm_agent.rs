//! Named-agent CLI contract against a private scripted RPC peer, not a harness.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::{json, Value};
use std::{path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixListener,
    process::Command,
};

async fn exchange(
    args: &[&str],
    method: &str,
    params: Value,
    response: Value,
) -> std::process::Output {
    scripted(
        args,
        method,
        |_| params,
        Some(response),
        Some(json!([method])),
        false,
    )
    .await
}

// The tmux sentinel detects local fallback. A live listener handles startup
// probes; EOF and post-exit accept checks detect extra requests/connections.
// A PATH atmd sentinel would not catch the preferred sibling executable.
fn command(args: &[&str], dir: &Path, socket: &Path) -> Command {
    use std::os::unix::fs::PermissionsExt;
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let path = bin.join("tmux");
    std::fs::write(
        &path,
        "#!/bin/sh\nprintf 'fallback\\n' >> \"$ATM_AGENT_FALLBACK_MARKER\"\nexit 99\n",
    )
    .unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut paths = vec![bin];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let mut command = Command::new(env!("CARGO_BIN_EXE_atm"));
    command
        .args(args)
        .current_dir(dir)
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .env_remove("RUST_BACKTRACE")
        .env_remove("RUST_LIB_BACKTRACE")
        .env("ATM_SOCKET", socket)
        .env("ATM_TMUX_SOCKET", "agent-cli-test")
        .env("TMUX_TMPDIR", dir.join("tmux"))
        .env("XDG_STATE_HOME", dir.join("xdg"))
        .env("ATM_STATE_DIR", dir.join("state"))
        .env("ATM_AGENT_FALLBACK_MARKER", dir.join("fallback"))
        .env("PATH", std::env::join_paths(paths).unwrap())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

async fn scripted(
    args: &[&str],
    method: &str,
    params: impl FnOnce(&Path) -> Value,
    response: Option<Value>,
    methods: Option<Value>,
    broken_stdout: bool,
) -> std::process::Output {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("rpc.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let mut params = params(dir.path());
    if matches!(method, "agent.up" | "agent.adopt" | "agent.respawn") && response.is_some() {
        params["caller_server"] = json!(dir
            .path()
            .join(format!("tmux/tmux-{}/agent-cli-test", unsafe {
                libc::getuid()
            })));
    }
    let mut command = command(args, dir.path(), &socket);
    if broken_stdout {
        // Shutdown is shared by even a briefly inherited reader in a concurrently
        // spawning test child; merely dropping a pipe reader is not enough.
        let (reader, writer) = std::os::unix::net::UnixStream::pair().unwrap();
        reader.shutdown(std::net::Shutdown::Read).unwrap();
        command.stdout(Stdio::from(std::os::fd::OwnedFd::from(writer)));
    }
    let child = command.spawn().unwrap();
    let peer = async {
        // ensure_daemon_running may open and close a liveness-probe connection.
        let (mut read, mut write) = loop {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, write) = stream.into_split();
            let mut read = BufReader::new(read);
            let mut hello = String::new();
            if read.read_line(&mut hello).await.unwrap() == 0 {
                continue;
            }
            assert_eq!(
                serde_json::from_str::<Value>(&hello).unwrap()["type"],
                "connect"
            );
            break (read, write);
        };
        let mut connected =
            json!({"type":"connected","protocol_version":{"major":1,"minor":1},"client_id":"cli"});
        if let Some(methods) = methods {
            connected["methods"] = methods;
        }
        write
            .write_all(format!("{connected}\n").as_bytes())
            .await
            .unwrap();
        let mut line = String::new();
        if let Some(response) = response {
            read.read_line(&mut line).await.unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(
                request,
                json!({"protocol_version":{"major":1,"minor":1},"type":"request","id":"r1","method":method,"params":params})
            );
            write
                .write_all(format!("{response}\n").as_bytes())
                .await
                .unwrap();
            line.clear();
        }
        assert_eq!(
            read.read_line(&mut line).await.unwrap(),
            0,
            "no extra or unsupported RPC"
        );
    };
    let output = tokio::time::timeout(Duration::from_secs(30), async {
        let ((), output) = tokio::join!(peer, child.wait_with_output());
        output.unwrap()
    })
    .await
    .unwrap();
    std::future::poll_fn(|cx| {
        assert!(
            listener.poll_accept(cx).is_pending(),
            "unexpected retry connection"
        );
        std::task::Poll::Ready(())
    })
    .await;
    assert!(
        !dir.path().join("fallback").exists(),
        "local fallback invoked"
    );
    output
}

#[tokio::test]
async fn agent_cli_roots_default_to_cwd_only_for_up_and_adopt() {
    for action in ["up", "adopt", "list"] {
        for root in [None, Some("relative/project"), Some("/absolute/project")] {
            let mut args = vec!["agent", action];
            if action != "list" {
                args.push("lead");
            }
            if action == "adopt" {
                args.push("%7");
            }
            if let Some(root) = root {
                args.extend(["--project-root", root]);
            }
            let method = format!("agent.{action}");
            let result = match action {
                "up" => json!({"agent_id":"project/lead","pane":"%7","state":"ready"}),
                "adopt" => json!({"agent_id":"project/lead","state":"starting"}),
                _ => json!([]),
            };
            let output = scripted(
                &args,
                &method,
                |cwd| {
                    assert!(cwd.is_absolute());
                    let root = match root {
                        Some("/absolute/project") => json!("/absolute/project"),
                        Some(relative) => json!(cwd.join(relative)),
                        None if action == "list" => Value::Null,
                        None => json!(cwd),
                    };
                    let mut expected = json!({"project_root":root});
                    if action != "list" {
                        expected["name"] = json!("lead");
                    }
                    if action == "adopt" {
                        expected["pane"] = json!("%7");
                    }
                    expected
                },
                Some(json!({"type":"response","id":"r1","result":result})),
                Some(json!([method])),
                false,
            )
            .await;
            assert_eq!(
                output.status.code(),
                Some(0),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(output.stderr.is_empty());
        }
    }
}

#[tokio::test]
async fn agent_cli_timeout_boundaries_full_ids_and_down_default_are_transmitted() {
    let id = "01234567890123456789012345678901/01234567890123456789012345678901";
    let session = "full-session-id-never-abbreviated-0123456789";
    for (args, method, params, result, text) in [
        (
            vec!["agent", "clear", id, "--timeout-ms", "5000"],
            "agent.clear",
            json!({"agent":id,"timeout_ms":5000}),
            json!({"agent_id":id,"session_id":session}),
            format!("{id}\t{session}\n"),
        ),
        (
            vec!["agent", "clear", id, "--timeout-ms", "120000"],
            "agent.clear",
            json!({"agent":id,"timeout_ms":120000}),
            json!({"agent_id":id,"session_id":null}),
            format!("{id}\t-\n"),
        ),
        (
            vec!["agent", "respawn", id],
            "agent.respawn",
            json!({"agent":id}),
            json!({"agent_id":id,"pane":"%123456"}),
            format!("{id}\t%123456\n"),
        ),
        (
            vec!["agent", "down", id],
            "agent.down",
            json!({"agent":id,"kill":false}),
            json!({"agent_id":id}),
            format!("{id}\n"),
        ),
    ] {
        let output = exchange(
            &args,
            method,
            params,
            json!({"type":"response","id":"r1","result":result}),
        )
        .await;
        assert_eq!(output.status.code(), Some(0));
        assert!(output.stderr.is_empty());
        assert_eq!(String::from_utf8(output.stdout).unwrap(), text);
    }
}

#[tokio::test]
async fn agent_cli_parse_failures_exit_two_without_connecting() {
    let mut cases = Vec::new();
    for timeout in [
        "0",
        "1",
        "4999",
        "120001",
        "-1",
        "1.5",
        "no",
        "18446744073709551616",
    ] {
        cases.push(vec![
            "agent",
            "clear",
            "project/lead",
            "--timeout-ms",
            timeout,
        ]);
    }
    for action in ["clear", "respawn", "down"] {
        for id in [
            "",
            "Lead",
            "léad",
            " lead",
            "owner",
            "system",
            "012345678901234567890123456789012",
            "/lead",
            "project/",
            "project/owner",
            "project/system",
            "project/a/b",
        ] {
            cases.push(vec!["agent", action, id]);
        }
    }
    for action in ["up", "adopt"] {
        for name in [
            "",
            "../lead",
            "a/b",
            "Lead",
            "léad",
            "-lead",
            " lead",
            "owner",
            "system",
            "012345678901234567890123456789012",
        ] {
            let mut args = vec!["agent", action, name];
            if action == "adopt" {
                args.push("%7");
            }
            cases.push(args);
        }
    }
    for pane in [
        "%00",
        "%07",
        "7",
        "%",
        "%+7",
        "%-7",
        "%٧",
        " %7",
        "%7\n",
        "%7; kill-pane",
        "session:0.1",
    ] {
        cases.push(vec!["agent", "adopt", "lead", pane]);
    }
    for args in cases {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("rpc.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let output = tokio::time::timeout(
            Duration::from_secs(30),
            command(&args, dir.path(), &socket).output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.is_empty());
        std::future::poll_fn(|cx| {
            assert!(
                listener.poll_accept(cx).is_pending(),
                "parse failure connected: {args:?}"
            );
            std::task::Poll::Ready(())
        })
        .await;
        assert!(!dir.path().join("fallback").exists());
    }
}

#[tokio::test]
async fn agent_cli_too_old_never_sends_an_operation_or_falls_back() {
    for (args, method) in [
        (vec!["agent", "up", "lead"], "agent.up"),
        (vec!["agent", "adopt", "lead", "%7"], "agent.adopt"),
        (vec!["agent", "clear", "project/lead"], "agent.clear"),
        (vec!["agent", "respawn", "project/lead"], "agent.respawn"),
        (vec!["agent", "down", "project/lead"], "agent.down"),
        (vec!["agent", "list"], "agent.list"),
    ] {
        for methods in [None, Some(json!([])), Some(json!(["session.send"]))] {
            let output = scripted(&args, method, |_| Value::Null, None, methods, false).await;
            assert_eq!(output.status.code(), Some(1));
            assert!(output.stdout.is_empty());
            assert_eq!(
                String::from_utf8(output.stderr).unwrap(),
                "Error: the daemon is too old; restart atmd\n"
            );
        }
    }
}

#[tokio::test]
async fn agent_cli_broken_stdout_is_an_io_failure_after_one_rpc() {
    for json in [false, true] {
        let mut args = vec!["agent", "clear", "project/lead"];
        if json {
            args.extend(["--format", "json"]);
        }
        let output = scripted(&args, "agent.clear", |_| json!({"agent":"project/lead","timeout_ms":30000}),
            Some(json!({"type":"response","id":"r1","result":{"agent_id":"project/lead","session_id":null}})),
            Some(json!(["agent.clear"])), true).await;
        assert_eq!(output.status.code(), Some(1), "json={json}: {output:?}");
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("Broken pipe"));
    }
}

#[tokio::test]
async fn agent_cli_rejects_malformed_results_instead_of_printing_success() {
    for (args, method, params) in [
        (
            vec!["agent", "up", "lead", "--project-root", "/project"],
            "agent.up",
            json!({"name":"lead","project_root":"/project"}),
        ),
        (
            vec!["agent", "adopt", "lead", "%7", "--project-root", "/project"],
            "agent.adopt",
            json!({"name":"lead","pane":"%7","project_root":"/project"}),
        ),
        (
            vec!["agent", "clear", "project/lead"],
            "agent.clear",
            json!({"agent":"project/lead","timeout_ms":30000}),
        ),
        (
            vec!["agent", "respawn", "project/lead"],
            "agent.respawn",
            json!({"agent":"project/lead"}),
        ),
        (
            vec!["agent", "down", "project/lead"],
            "agent.down",
            json!({"agent":"project/lead","kill":false}),
        ),
        (
            vec!["agent", "list"],
            "agent.list",
            json!({"project_root":null}),
        ),
    ] {
        let output = exchange(
            &args,
            method,
            params,
            json!({"type":"response","id":"r1","result":{"agent_id":"not-a-full-id"}}),
        )
        .await;
        assert_eq!(output.status.code(), Some(1), "{method}");
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.is_empty());
    }
}

#[tokio::test]
async fn agent_cli_mutations_use_exact_schemas_and_print_optional_session() {
    for (args, method, params, result, text) in [
        (
            vec!["agent", "up", "lead", "--project-root", "/project"],
            "agent.up",
            json!({"project_root":"/project","name":"lead"}),
            json!({"agent_id":"project/lead","pane":"%7","state":"ready"}),
            "project/lead\t%7\tready\n",
        ),
        (
            vec!["agent", "adopt", "lead", "%7", "--project-root", "/project"],
            "agent.adopt",
            json!({"project_root":"/project","name":"lead","pane":"%7"}),
            json!({"agent_id":"project/lead","state":"starting"}),
            "project/lead\tstarting\n",
        ),
        (
            vec!["agent", "clear", "project/lead"],
            "agent.clear",
            json!({"agent":"project/lead","timeout_ms":30000}),
            json!({"agent_id":"project/lead","session_id":null}),
            "project/lead\t-\n",
        ),
        (
            vec!["agent", "respawn", "project/lead"],
            "agent.respawn",
            json!({"agent":"project/lead"}),
            json!({"agent_id":"project/lead","pane":"%9"}),
            "project/lead\t%9\n",
        ),
        (
            vec!["agent", "down", "project/lead", "--kill"],
            "agent.down",
            json!({"agent":"project/lead","kill":true}),
            json!({"agent_id":"project/lead"}),
            "project/lead\n",
        ),
        (
            vec!["agent", "list"],
            "agent.list",
            json!({"project_root":null}),
            json!([]),
            "AGENT\tSTATE\tACTIVITY\tPANE\tSESSION\tHARNESS\tMODEL\tCOST\tREASON\n",
        ),
    ] {
        for json_output in [false, true] {
            let mut args = args.clone();
            if json_output {
                args.extend(["--format", "json"]);
            }
            let mut wire_result = result.clone();
            if wire_result.is_object() {
                wire_result["future_field"] = json!(true);
            }
            let output = exchange(
                &args,
                method,
                params.clone(),
                json!({"type":"response","id":"r1","result":wire_result}),
            )
            .await;
            assert_eq!(
                output.status.code(),
                Some(0),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(output.stderr.is_empty());
            if json_output {
                assert_eq!(
                    serde_json::from_slice::<Value>(&output.stdout).unwrap(),
                    result,
                    "{method}"
                );
                assert_eq!(output.stdout.last(), Some(&b'\n'));
                assert_eq!(output.stdout.iter().filter(|&&b| b == b'\n').count(), 1);
            } else {
                assert_eq!(String::from_utf8(output.stdout).unwrap(), text, "{method}");
            }
        }
    }
}

#[tokio::test]
async fn agent_cli_daemon_errors_are_not_success_or_a_local_fallback() {
    for (code, message) in [
        (
            "unavailable",
            "agent operation 'agent.clear' is not implemented",
        ),
        (
            "refused",
            "Refusing input to %7: screen changed; nothing was sent",
        ),
        ("not_found", "agent project/lead was not found"),
        ("conflict", "agent project/lead is busy"),
        ("invalid_params", "invalid agent parameters"),
        (
            "timeout",
            "admitted effects may have completed; inspect agent list",
        ),
        ("internal", "storage failed\nCaused by: disk full"),
    ] {
        let output = exchange(
            &["agent", "clear", "project/lead"],
            "agent.clear",
            json!({"agent":"project/lead","timeout_ms":30000}),
            json!({"type":"response","id":"r1","error":{"code":code,"message":message}}),
        )
        .await;
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        assert_eq!(
            String::from_utf8(output.stderr).unwrap(),
            format!("Error: {message}\n")
        );
    }
}

#[tokio::test]
async fn agent_cli_table_keeps_daemon_order_and_escapes_reason() {
    assert_list_output(false).await;
}

#[tokio::test]
async fn agent_cli_json_keeps_daemon_order_and_nulls() {
    assert_list_output(true).await;
}

async fn assert_list_output(json_output: bool) {
    // Deliberately out of id order: sorting belongs to A1, never the CLI.
    let entries = json!([
        {"agent_id":"project/worker","state":"down","activity":null,"pane":null,"session_id":null,"harness":null,"model":null,"cost_usd":null,"reason":null},
        {"agent_id":"project/lead","state":"stuck","activity":"working","pane":"%7","session_id":"full-session-id-0123456789","harness":"pi","model":"full-model-name","cost_usd":0.000123,"reason":"line\nnext\tcell\rreturn\\slash"}
    ]);
    {
        let args = if json_output {
            vec!["agent", "list", "--format", "json"]
        } else {
            vec!["agent", "list"]
        };
        let output = exchange(
            &args,
            "agent.list",
            json!({"project_root":null}),
            json!({"type":"response","id":"r1","result":entries.clone()}),
        )
        .await;
        assert!(output.status.success());
        if json_output {
            assert_eq!(
                serde_json::from_slice::<Value>(&output.stdout).unwrap(),
                entries
            );
        } else {
            assert_eq!(String::from_utf8(output.stdout).unwrap(), "AGENT\tSTATE\tACTIVITY\tPANE\tSESSION\tHARNESS\tMODEL\tCOST\tREASON\nproject/worker\tdown\t-\t-\t-\t-\t-\t-\t-\nproject/lead\tstuck\tworking\t%7\tfull-session-id-0123456789\tpi\tfull-model-name\t0.000123\tline\\nnext\\tcell\\rreturn\\\\slash\n");
        }
    }
}
