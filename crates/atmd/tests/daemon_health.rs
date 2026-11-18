//! Real private Unix-socket health round trip, synchronized on server initialization.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use atm_protocol::{ClientMessage, DaemonMessage, ResponsePayload};
use atm_tmux::MockTmuxClient;
use atmd::{
    control::service::spawn_control,
    registry::spawn_registry,
    server::{methods::MethodServices, DaemonServer},
};
use serde_json::{json, Value};
use std::{
    future::{poll_fn, Future},
    task::Poll,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn daemon_health_socket_round_trip_is_dynamic_and_advertised() {
    tokio::time::timeout(Duration::from_secs(60), async {
        for configured in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let socket = dir.path().join("health.sock");
            let cancel = CancellationToken::new();
            let dependencies = CancellationToken::new();
            let registry = spawn_registry();
            let mut services = MethodServices::new(registry.clone());
            if configured {
                services.org = atmd::org::OrgHandle::open(dir.path().join("state"), dependencies.child_token()).await;
                services.org.status().unwrap();
            }
            let mock = MockTmuxClient::new();
            let (control, control_task) = spawn_control(Box::new(mock.clone()), registry, services.org.clone(), dependencies.child_token());
            services.control = Some(control);
            let server = DaemonServer::with_services(&socket, services, cancel.clone());
            let mut running = Box::pin(async move { server.run().await });
            // run binds synchronously before its first pending accept. Poll once,
            // then hand the same future to its task: no retry/sleep/readiness race.
            poll_fn(|cx| match running.as_mut().poll(cx) {
                Poll::Pending => Poll::Ready(()),
                Poll::Ready(result) => panic!("server stopped at initialization: {result:?}"),
            }).await;
            assert!(socket.exists());
            let server_task = tokio::spawn(running);
            let stream = UnixStream::connect(&socket).await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut lines = BufReader::new(read).lines();
            let frame = serde_json::to_string(&ClientMessage::connect(Some("health-socket-test".into()))).unwrap() + "\n";
            write.write_all(frame.as_bytes()).await.unwrap();
            let connected: DaemonMessage = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            let DaemonMessage::Connected { methods, .. } = connected else { panic!("not connected: {connected:?}") };
            assert!(methods.contains(&"daemon.health".to_owned()));
            for (id, method) in [("info", "daemon.info"), ("health", "daemon.health"), ("again", "daemon.health")] {
                let frame = serde_json::to_string(&ClientMessage::request(id.into(), method.into(), Value::Null, None)).unwrap() + "\n";
                write.write_all(frame.as_bytes()).await.unwrap();
                let reply: DaemonMessage = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
                let DaemonMessage::Response { id: reply_id, payload: ResponsePayload::Result(result) } = reply else { panic!("not a successful reply: {reply:?}") };
                assert_eq!(reply_id, id);
                if id == "info" {
                    assert!(result["methods"].as_array().unwrap().contains(&json!("daemon.health")));
                } else {
                    assert_eq!(result, json!({
                        "reservations": {"state":"ok", "reason":null, "live":0, "quarantined":0, "capacity":32},
                        "lifecycle": {"state":if configured { "ok" } else { "absent" }, "capacity":32}
                    }));
                }
            }
            drop(write);
            drop(lines);
            cancel.cancel();
            server_task.await.unwrap().unwrap();
            dependencies.cancel();
            control_task.await.unwrap();
            assert!(mock.calls().is_empty(), "health must not inspect/mutate panes");
        }
    }).await.expect("health socket test hung");
}
