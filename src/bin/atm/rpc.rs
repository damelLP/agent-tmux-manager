//! One-shot capability-checked control-plane client.
//!
//! The caller supplies a total budget, including daemon startup. Requests are
//! never retried: a timeout does not prove that a mutation had no effect.

use std::path::Path;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use atm_protocol::{ClientMessage, DaemonMessage, ProtocolVersion, ResponseError, ResponsePayload};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{unix::OwnedReadHalf, unix::OwnedWriteHalf, UnixStream};

const MAX_LINE_SIZE: usize = 1_048_576;
// A timed-out startup may still be blocked on a PID file. Retain its permit
// in that thread so subsequent calls cannot accumulate more blocked threads.
static STARTUP_SLOT: LazyLock<Arc<tokio::sync::Semaphore>> =
    LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(1)));

#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    #[error("the daemon is too old; restart atmd")]
    TooOld,
    #[error("{message}", message = .0.message)]
    Daemon(ResponseError),
    #[error("daemon request timed out")]
    Timeout,
    #[error("Failed to start daemon: {0}")]
    Startup(String),
    #[error("daemon connection failed: {0}")]
    Connection(String),
}

/// Connect, check capabilities, and issue exactly one request. Send uses one
/// call; spawn uses separate prepare/split/type calls so it can print between
/// split and type. Other session commands retain their legacy exchange.
pub async fn call(method: &str, params: Value, budget: Duration) -> Result<Value, RpcError> {
    let socket = atm_tui::client::resolve_socket_path();
    let from_pane = super::spawn::non_empty_env("TMUX_PANE");
    call_with_startup(
        Arc::clone(&STARTUP_SLOT),
        &socket,
        method,
        params,
        from_pane,
        budget,
        atm_tui::daemon::ensure_daemon_running,
    )
    .await
}

async fn call_with_startup<F>(
    startup_slot: Arc<tokio::sync::Semaphore>,
    socket: &Path,
    method: &str,
    params: Value,
    from_pane: Option<String>,
    budget: Duration,
    startup: F,
) -> Result<Value, RpcError>
where
    F: FnOnce() -> Result<(), String> + Send + 'static,
{
    tokio::time::timeout(budget, async {
        // Startup reads a PID file: even a FIFO must not keep the async runtime
        // alive after our deadline. One detached call, never a retry loop.
        let permit = startup_slot
            .acquire_owned()
            .await
            .map_err(connection_error)?;
        let (send, receive) = tokio::sync::oneshot::channel();
        std::thread::Builder::new()
            .name("atm-rpc-start".into())
            .spawn(move || {
                let _permit = permit;
                let _ = send.send(startup());
            })
            .map_err(connection_error)?;
        receive
            .await
            .map_err(connection_error)?
            .map_err(RpcError::Startup)?;
        exchange(socket, method, params, from_pane).await
    })
    .await
    .map_err(|_| RpcError::Timeout)?
}

fn connection_error(error: impl std::fmt::Display) -> RpcError {
    RpcError::Connection(error.to_string())
}

async fn send(writer: &mut OwnedWriteHalf, message: &ClientMessage) -> Result<(), RpcError> {
    let line = serde_json::to_string(message).map_err(connection_error)?;
    writer
        .write_all(format!("{line}\n").as_bytes())
        .await
        .map_err(connection_error)
}

async fn read_line(reader: &mut BufReader<OwnedReadHalf>) -> Result<Vec<u8>, RpcError> {
    let mut line = Vec::new();
    let n = (&mut *reader)
        .take(MAX_LINE_SIZE as u64 + 1)
        .read_until(b'\n', &mut line)
        .await
        .map_err(connection_error)?;
    if n == 0 {
        return Err(RpcError::Connection("daemon closed the connection".into()));
    }
    if line.len() > MAX_LINE_SIZE {
        return Err(RpcError::Connection(
            "daemon line is too large (max: 1 MiB)".into(),
        ));
    }
    Ok(line)
}

/// The enclosing call's single deadline bounds every socket operation, including
/// a stream of unrelated or malformed messages that never yields our response.
async fn exchange(
    socket: &Path,
    method: &str,
    params: Value,
    from_pane: Option<String>,
) -> Result<Value, RpcError> {
    let stream = UnixStream::connect(socket)
        .await
        .map_err(connection_error)?;
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::new(read);
    send(&mut write, &ClientMessage::connect(Some("atm-rpc".into()))).await?;
    loop {
        let line = read_line(&mut read).await?;
        match serde_json::from_slice::<DaemonMessage>(&line) {
            Ok(DaemonMessage::Connected {
                protocol_version,
                methods,
                ..
            }) => {
                if !protocol_version.is_compatible_with(&ProtocolVersion::CURRENT) {
                    return Err(RpcError::Connection(format!(
                        "incompatible protocol {protocol_version}"
                    )));
                }
                if !methods.iter().any(|name| name == method) {
                    return Err(RpcError::TooOld);
                }
                break;
            }
            Ok(DaemonMessage::Rejected { reason, .. }) => return Err(RpcError::Connection(reason)),
            _ => {}
        }
    }
    // One request per connection, so a fixed local correlation id is sufficient.
    send(
        &mut write,
        &ClientMessage::request("r1".into(), method.into(), params, from_pane),
    )
    .await?;
    loop {
        let line = read_line(&mut read).await?;
        if let Ok(DaemonMessage::Response { id, payload }) = serde_json::from_slice(&line) {
            if id == "r1" {
                return match payload {
                    ResponsePayload::Result(value) => Ok(value),
                    ResponsePayload::Error(error) => Err(RpcError::Daemon(error)),
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::net::UnixListener;

    fn assert_private_paths(run: &Path, socket: &Path, xdg: Option<&Path>) {
        let run = run.canonicalize().expect("run directory must exist");
        assert!(
            run.parent().is_some(),
            "filesystem root is not a private run directory"
        );
        let socket_dir = socket
            .parent()
            .unwrap()
            .canonicalize()
            .expect("socket directory must exist");
        let xdg = xdg
            .expect("private XDG_STATE_HOME required")
            .canonicalize()
            .expect("XDG directory must exist");
        assert!(
            socket_dir.starts_with(&run) && socket_dir != run,
            "ATM_SOCKET must be inside the run directory"
        );
        assert!(
            xdg.parent() == Some(run.as_path()),
            "XDG_STATE_HOME must be a private child of the run directory"
        );
    }

    fn startup_slot() -> std::sync::Arc<tokio::sync::Semaphore> {
        std::sync::Arc::new(tokio::sync::Semaphore::new(1))
    }

    #[tokio::test]
    async fn injected_slot_wait_is_bounded_without_running_startup() {
        let slot = startup_slot();
        let _held = slot.acquire().await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let result = call_with_startup(
            slot.clone(),
            &dir.path().join("unused.sock"),
            "daemon.info",
            json!({}),
            None,
            Duration::from_millis(30),
            || panic!("startup ran without its slot"),
        )
        .await;
        assert!(matches!(result, Err(RpcError::Timeout)));
    }

    #[test]
    fn autostart_guard_rejects_missing_or_escaped_xdg_and_socket() {
        let run = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(run.path().join("s")).unwrap();
        std::fs::create_dir(run.path().join("xdg")).unwrap();
        let socket = run.path().join("s/atm.sock");
        let xdg = run.path().join("xdg");
        assert_private_paths(run.path(), &socket, Some(&xdg));
        assert!(std::panic::catch_unwind(|| assert_private_paths(
            Path::new("/"),
            &socket,
            Some(&xdg)
        ))
        .is_err());
        for bad in [None, Some(outside.path())] {
            assert!(
                std::panic::catch_unwind(|| assert_private_paths(run.path(), &socket, bad))
                    .is_err()
            );
        }
        assert!(std::panic::catch_unwind(|| assert_private_paths(
            run.path(),
            &outside.path().join("atm.sock"),
            Some(&xdg)
        ))
        .is_err());
        std::os::unix::fs::symlink(outside.path(), run.path().join("escape")).unwrap();
        assert!(std::panic::catch_unwind(|| assert_private_paths(
            run.path(),
            &socket,
            Some(&run.path().join("escape"))
        ))
        .is_err());
    }

    fn connected(methods: Option<Value>) -> Value {
        let mut value =
            json!({"type":"connected","protocol_version":{"major":1,"minor":1},"client_id":"cli"});
        if let Some(methods) = methods {
            value["methods"] = methods;
        }
        value
    }

    async fn scripted_peer(
        hello: Value,
        replies: Vec<String>,
        expect_request: bool,
        from_pane: Option<String>,
    ) -> Result<Value, RpcError> {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("rpc.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let expected_pane = from_pane.clone();
        let peer = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut read = BufReader::new(read);
            let mut line = String::new();
            read.read_line(&mut line).await.unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(&line).unwrap()["type"],
                "connect"
            );
            write
                .write_all(format!("{hello}\n").as_bytes())
                .await
                .unwrap();
            line.clear();
            let n = read.read_line(&mut line).await.unwrap();
            if expect_request {
                assert_ne!(n, 0);
                let request: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(request["id"], "r1");
                assert_eq!(request["method"], "daemon.info");
                assert_eq!(request["params"], json!({}));
                assert_eq!(
                    request.get("from_pane").cloned(),
                    expected_pane.map(Value::String)
                );
                for reply in replies {
                    write
                        .write_all(format!("{reply}\n").as_bytes())
                        .await
                        .unwrap();
                }
            } else {
                assert_eq!(n, 0, "request sent before checking capabilities: {line}");
            }
        });
        let result = call_with_startup(
            startup_slot(),
            &socket,
            "daemon.info",
            json!({}),
            from_pane,
            Duration::from_secs(5),
            || Ok(()),
        )
        .await;
        tokio::time::timeout(Duration::from_secs(5), peer)
            .await
            .unwrap()
            .unwrap();
        result
    }

    #[tokio::test]
    async fn old_or_missing_method_is_rejected_before_sending_request() {
        for methods in [None, Some(json!([])), Some(json!(["other"]))] {
            let error = scripted_peer(connected(methods), vec![], false, None)
                .await
                .unwrap_err();
            assert!(matches!(error, RpcError::TooOld));
            assert_eq!(error.to_string(), "the daemon is too old; restart atmd");
        }
    }

    #[tokio::test]
    async fn matching_result_skips_other_lines_and_preserves_caller_pane() {
        for pane in [None, Some("%42".into())] {
            let result = scripted_peer(connected(Some(json!(["daemon.info"]))), vec![
                "not json".into(),
                json!({"type":"future_message"}).to_string(),
                json!({"type":"pong","seq":3}).to_string(),
                json!({"type":"event","topic":"agent","seq":1,"ts":"now","kind":"agent.up","body":{}}).to_string(),
                json!({"type":"response","id":"wrong","result":false}).to_string(),
                json!({"type":"response","id":"r1","result":null}).to_string(),
            ], true, pane).await.unwrap();
            assert_eq!(result, Value::Null);
        }
    }

    #[tokio::test]
    async fn remote_errors_keep_open_code_and_message() {
        let error = scripted_peer(connected(Some(json!(["daemon.info"]))), vec![
            json!({"type":"response","id":"r1","error":{"code":"future_code","message":"detail"}}).to_string()
        ], true, None).await.unwrap_err();
        assert!(
            matches!(error, RpcError::Daemon(error) if error.code == "future_code" && error.message == "detail")
        );
    }

    #[tokio::test]
    async fn rejection_incompatible_version_eof_and_oversize_are_connection_errors() {
        let mut incompatible = connected(Some(json!(["daemon.info"])));
        incompatible["protocol_version"]["major"] = json!(2);
        for hello in [
            incompatible,
            json!({"type":"rejected","protocol_version":{"major":2,"minor":0},"reason":"upgrade"}),
        ] {
            assert!(matches!(
                scripted_peer(hello, vec![], false, None).await,
                Err(RpcError::Connection(_))
            ));
        }
        assert!(matches!(
            scripted_peer(connected(Some(json!(["daemon.info"]))), vec![], true, None).await,
            Err(RpcError::Connection(_))
        ));
        // No newline before the cap: the client must not wait for the rest.
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("big.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let peer = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = stream.write_all(&vec![b'x'; 1_048_577]).await;
        });
        assert!(
            matches!(call_with_startup(startup_slot(), &socket, "daemon.info", json!({}), None, Duration::from_secs(5), || Ok(())).await, Err(RpcError::Connection(message)) if message.contains("large"))
        );
        peer.await.unwrap();
    }

    #[tokio::test]
    async fn total_deadline_covers_stalled_handshake_response_and_noise() {
        for phase in ["handshake", "response", "noise"] {
            let dir = tempfile::tempdir().unwrap();
            let socket = dir.path().join("slow.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
            let peer = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let (read, mut write) = stream.into_split();
                let mut read = BufReader::new(read);
                let mut line = String::new();
                read.read_line(&mut line).await.unwrap();
                if phase != "handshake" {
                    write
                        .write_all(
                            format!("{}\n", connected(Some(json!(["daemon.info"])))).as_bytes(),
                        )
                        .await
                        .unwrap();
                    line.clear();
                    read.read_line(&mut line).await.unwrap();
                }
                reached_tx.send(()).unwrap();
                loop {
                    if phase == "noise"
                        && write
                            .write_all(b"{\"type\":\"pong\",\"seq\":1}\n")
                            .await
                            .is_err()
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
            });
            let result = call_with_startup(
                startup_slot(),
                &socket,
                "daemon.info",
                json!({}),
                None,
                Duration::from_millis(100),
                || Ok(()),
            )
            .await;
            peer.abort();
            let _ = peer.await;
            reached_rx
                .await
                .expect("timeout must reach the phase being tested");
            assert!(
                matches!(result, Err(RpcError::Timeout)),
                "{phase}: {result:?}"
            );
        }
    }

    #[tokio::test]
    async fn startup_runs_once_before_connect_and_failure_is_typed() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("startup.sock");
        let (tx, rx) = tokio::sync::oneshot::channel();
        let bind_path = socket.clone();
        let result = call_with_startup(
            startup_slot(),
            &socket,
            "daemon.info",
            json!({}),
            None,
            Duration::from_secs(5),
            move || {
                let listener = std::os::unix::net::UnixListener::bind(bind_path).unwrap();
                tx.send(listener).unwrap();
                Err("startup refused".into())
            },
        )
        .await;
        assert!(rx.await.is_ok());
        assert!(
            matches!(result, Err(RpcError::Startup(message)) if message.contains("startup refused"))
        );
    }

    #[tokio::test]
    async fn stalled_startup_does_not_hold_the_runtime() {
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let slot = startup_slot();
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("absent.sock");
        let first = call_with_startup(
            slot.clone(),
            &socket,
            "daemon.info",
            json!({}),
            None,
            Duration::from_secs(5),
            move || {
                started_tx.send(()).unwrap();
                let _ = release_rx.recv();
                Ok(())
            },
        );
        tokio::pin!(first);
        tokio::select! {
            started = started_rx => started.unwrap(),
            result = &mut first => panic!("startup did not reach its blocking phase: {result:?}"),
        }
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(5)).await;
        assert!(matches!(first.await, Err(RpcError::Timeout)));
        // A timeout must not allow another caller to pile up a detached thread
        // on the same stuck PID-file read.
        let called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let second_called = called.clone();
        let second = call_with_startup(
            slot,
            &socket,
            "daemon.info",
            json!({}),
            None,
            Duration::from_millis(30),
            move || {
                second_called.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            },
        )
        .await;
        release_tx.send(()).unwrap();
        assert!(matches!(second, Err(RpcError::Timeout)));
        assert!(!called.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn actual_daemon_returns_typed_invalid_params() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("real.sock");
        let cancel = tokio_util::sync::CancellationToken::new();
        let server = atmd::server::DaemonServer::new(
            &socket,
            atmd::registry::spawn_registry(),
            cancel.clone(),
        );
        let run = tokio::spawn(async move { server.run().await });
        tokio::time::timeout(Duration::from_secs(5), async {
            while !socket.exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let result = call_with_startup(
            startup_slot(),
            &socket,
            "daemon.info",
            json!({"bad":1}),
            None,
            Duration::from_secs(5),
            || Ok(()),
        )
        .await;
        cancel.cancel();
        run.await.unwrap().unwrap();
        assert!(matches!(result, Err(RpcError::Daemon(error)) if error.code == "invalid_params"));
    }
}
