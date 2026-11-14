//! Named-agent CLI contract against a private scripted RPC peer, not a harness.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::{json, Value};
use std::time::Duration;
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
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("rpc.sock");
    let listener = UnixListener::bind(&socket).unwrap();
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
        write.write_all(format!("{}\n", json!({"type":"connected","protocol_version":{"major":1,"minor":1},"client_id":"cli","methods":[method]})).as_bytes()).await.unwrap();
        let mut line = String::new();
        read.read_line(&mut line).await.unwrap();
        let request: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(request["method"], method);
        assert_eq!(request["params"], params);
        assert!(request.get("from_pane").is_none());
        write
            .write_all(format!("{response}\n").as_bytes())
            .await
            .unwrap();
        line.clear();
        assert_eq!(read.read_line(&mut line).await.unwrap(), 0, "one RPC only");
    };
    let mut command = Command::new(env!("CARGO_BIN_EXE_atm"));
    command
        .args(args)
        .current_dir(dir.path())
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .env("ATM_SOCKET", &socket)
        .env("XDG_STATE_HOME", dir.path().join("xdg"))
        .env("ATM_STATE_DIR", dir.path().join("state"))
        .kill_on_drop(true);
    tokio::time::timeout(Duration::from_secs(30), async {
        let ((), output) = tokio::join!(peer, command.output());
        output.unwrap()
    })
    .await
    .unwrap()
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
        let output = exchange(
            &args,
            method,
            params,
            json!({"type":"response","id":"r1","result":result}),
        )
        .await;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(String::from_utf8(output.stdout).unwrap(), text, "{method}");
    }
}

#[tokio::test]
async fn agent_cli_unavailable_is_not_success_or_a_local_fallback() {
    let output = exchange(&["agent","clear","project/lead"], "agent.clear", json!({"agent":"project/lead","timeout_ms":30000}),
        json!({"type":"response","id":"r1","error":{"code":"unavailable","message":"agent operation 'agent.clear' is not implemented"}})).await;
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "Error: agent operation 'agent.clear' is not implemented\n"
    );
}

#[tokio::test]
async fn agent_cli_json_keeps_null_and_table_escapes_reason() {
    let entry = json!({"agent_id":"project/lead","state":"stuck","activity":null,"pane":"%7","session_id":null,"harness":"pi","model":null,"cost_usd":0.000123,"reason":"line\nnext\tcell"});
    for json_output in [false, true] {
        let args = if json_output {
            vec!["agent", "list", "--format", "json"]
        } else {
            vec!["agent", "list"]
        };
        let output = exchange(
            &args,
            "agent.list",
            json!({"project_root":null}),
            json!({"type":"response","id":"r1","result":[entry.clone()]}),
        )
        .await;
        assert!(output.status.success());
        if json_output {
            assert_eq!(
                serde_json::from_slice::<Value>(&output.stdout).unwrap(),
                json!([entry])
            );
        } else {
            assert_eq!(String::from_utf8(output.stdout).unwrap(), "AGENT\tSTATE\tACTIVITY\tPANE\tSESSION\tHARNESS\tMODEL\tCOST\tREASON\nproject/lead\tstuck\t-\t%7\t-\tpi\t-\t0.000123\tline\\nnext\\tcell\n");
        }
    }
}
