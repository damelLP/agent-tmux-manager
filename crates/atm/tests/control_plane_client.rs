//! Synthetic protocol peers on private sockets. Synchronisation is wire/event
//! ordering, never sleeps or elapsed-time assertions. Timeouts are hang guards.

#![allow(clippy::unwrap_used, clippy::expect_used)] // test helpers may panic

use std::{future::Future, task::Poll, time::Duration};

use atm_core::SessionId;
use atm_protocol::{ClientMessage, DaemonMessage, MessageType, ProtocolVersion, ResponsePayload};
use atm_tui::{
    client::{DaemonClient, DaemonConfig},
    control::{ControlFailure, ControlNotice},
    daemon::ControlHandle,
    input::{ClientCommand, Event},
};
use serde_json::json;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{unix::OwnedReadHalf, unix::OwnedWriteHalf, UnixListener},
    sync::mpsc,
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

const GUARD: Duration = Duration::from_secs(60);

async fn guarded<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(GUARD, future)
        .await
        .expect("hung test")
}

struct Peer {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

impl Peer {
    async fn recv(&mut self) -> MessageType {
        let mut line = String::new();
        assert_ne!(guarded(self.reader.read_line(&mut line)).await.unwrap(), 0);
        serde_json::from_str::<ClientMessage>(&line)
            .unwrap()
            .message
    }

    async fn send(&mut self, message: DaemonMessage) {
        let line = format!("{}\n", serde_json::to_string(&message).unwrap());
        guarded(self.writer.write_all(line.as_bytes()))
            .await
            .unwrap();
    }
}

struct Rig {
    _dir: tempfile::TempDir,
    listener: UnixListener,
    commands: mpsc::UnboundedSender<ClientCommand>,
    events: mpsc::UnboundedReceiver<Event>,
    cancel: CancellationToken,
    run: JoinHandle<()>,
}

impl Rig {
    fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("peer.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (event_tx, events) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let client = DaemonClient::new(
            DaemonConfig {
                socket_path: path,
                ..Default::default()
            },
            event_tx,
            command_rx,
            cancel.clone(),
        );
        let run = tokio::spawn(async move { client.run().await });
        Self {
            _dir: dir,
            listener,
            commands,
            events,
            cancel,
            run,
        }
    }

    fn handle(&self) -> ControlHandle {
        ControlHandle::new(self.commands.clone())
    }

    async fn connect(&mut self, minor: u16, methods: &[&str]) -> Peer {
        let (stream, _) = guarded(self.listener.accept()).await.unwrap();
        let (reader, writer) = stream.into_split();
        let mut peer = Peer {
            reader: BufReader::new(reader),
            writer,
        };
        assert!(matches!(peer.recv().await, MessageType::Connect { .. }));
        peer.send(DaemonMessage::Connected {
            protocol_version: ProtocolVersion::new(1, minor),
            client_id: "scripted".into(),
            methods: methods.iter().map(|s| (*s).into()).collect(),
        })
        .await;
        assert!(matches!(
            peer.recv().await,
            MessageType::Subscribe { session_id: None }
        ));
        peer.send(DaemonMessage::session_list(vec![])).await;
        assert!(
            matches!(guarded(self.events.recv()).await, Some(Event::SessionListReplace(v)) if v.is_empty())
        );
        peer
    }

    async fn watch(&mut self, peer: &mut Peer, cursor: u64) {
        self.handle().watch(vec!["task.".into()], cursor).unwrap();
        // If Watch is ignored, Discover appears first: no negative wait needed.
        self.commands.send(ClientCommand::Discover).unwrap();
        assert!(
            matches!(peer.recv().await, MessageType::Watch { topics, cursor: c } if c == cursor && topics == ["task."])
        );
        assert!(matches!(peer.recv().await, MessageType::Discover));
    }

    async fn notice(&mut self) -> ControlNotice {
        loop {
            match guarded(self.events.recv()).await.expect("events closed") {
                Event::Control(ControlNotice::Capabilities { .. }) => {}
                Event::Control(notice) => return notice,
                other => panic!("unexpected event: {other:?}"),
            }
        }
    }

    async fn disconnected(&mut self) {
        loop {
            match guarded(self.events.recv()).await.unwrap() {
                Event::Control(ControlNotice::Capabilities { .. }) => {}
                Event::DaemonDisconnected(None) => return,
                other => panic!("unexpected event: {other:?}"),
            }
        }
    }

    async fn stop(&mut self) {
        self.cancel.cancel();
        guarded(&mut self.run).await.unwrap();
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.run.abort();
    }
}

fn event(seq: u64) -> DaemonMessage {
    DaemonMessage::event(
        "task".into(),
        seq,
        "2026-01-01T00:00:00Z".into(),
        "task.future".into(),
        json!({"unknown":true}),
    )
}

fn request(
    handle: ControlHandle,
    method: &str,
) -> JoinHandle<Result<ResponsePayload, ControlFailure>> {
    let method = method.to_owned();
    tokio::spawn(async move {
        handle
            .request(
                method,
                json!({}),
                Some("%7".into()),
                Duration::from_secs(120),
            )
            .await
    })
}

#[tokio::test]
async fn requests_correlate_out_of_order_while_sessions_and_watch_continue() {
    let mut rig = Rig::start();
    let mut peer = rig.connect(1, &["task.list", "task.get"]).await;
    let mut first = request(rig.handle(), "task.list");
    let first_wire = tokio::select! {
        result = &mut first => panic!("request completed before reaching daemon: {result:?}"),
        message = peer.recv() => message,
    };
    let MessageType::Request {
        id: list_id,
        method,
        from_pane,
        params,
    } = first_wire
    else {
        panic!("request")
    };
    assert_eq!(method, "task.list");
    assert_eq!(from_pane.as_deref(), Some("%7"));
    assert_eq!(params, json!({}));
    let second = request(rig.handle(), "task.get");
    let MessageType::Request {
        id: get_id, method, ..
    } = peer.recv().await
    else {
        panic!("request")
    };
    assert_eq!(method, "task.get");
    assert_ne!(list_id, get_id);
    rig.watch(&mut peer, 40).await;
    peer.send(DaemonMessage::response_ok(
        list_id.replacen("tui-", "tui-0", 1),
        json!("different wire id"),
    ))
    .await;
    peer.send(DaemonMessage::response_ok("unrelated".into(), json!(9)))
        .await;
    peer.send(DaemonMessage::response_ok(get_id.clone(), json!(null)))
        .await;
    peer.send(event(42)).await;
    peer.send(DaemonMessage::session_removed(SessionId::new("s1")))
        .await;
    peer.send(DaemonMessage::response_err(list_id, "refused", "no"))
        .await;
    assert!(
        matches!(guarded(first).await.unwrap(), Ok(ResponsePayload::Error(e)) if e.code == "refused" && e.message == "no")
    );
    assert!(
        matches!(guarded(second).await.unwrap(), Ok(ResponsePayload::Result(v)) if v.is_null())
    );
    assert!(
        matches!(rig.notice().await, ControlNotice::Event(DaemonMessage::Event { seq: 42, body, .. }) if body == json!({"unknown":true}))
    );
    assert!(
        matches!(guarded(rig.events.recv()).await, Some(Event::SessionRemoved(id)) if id == "s1")
    );
    peer.send(DaemonMessage::response_ok(get_id, json!(99)))
        .await;
    peer.send(event(43)).await;
    assert!(matches!(
        rig.notice().await,
        ControlNotice::Event(DaemonMessage::Event { seq: 43, .. })
    ));
    rig.stop().await;
}

#[tokio::test]
async fn watch_replays_then_follows_and_resumes_last_delivered_cursor() {
    let mut rig = Rig::start();
    let mut peer = rig.connect(1, &[]).await;
    rig.watch(&mut peer, 40).await;
    for seq in [42, 42, 41, 47] {
        peer.send(event(seq)).await;
    }
    for expected in [42, 47] {
        assert!(
            matches!(rig.notice().await, ControlNotice::Event(DaemonMessage::Event { seq, .. }) if seq == expected)
        );
    }
    drop(peer);
    rig.disconnected().await;
    let mut peer = rig.connect(1, &[]).await;
    assert!(matches!(
        peer.recv().await,
        MessageType::Watch { cursor: 47, .. }
    ));
    peer.send(event(47)).await;
    peer.send(event(50)).await;
    assert!(matches!(
        rig.notice().await,
        ControlNotice::Event(DaemonMessage::Event { seq: 50, .. })
    ));
    rig.stop().await;
}

#[tokio::test]
async fn rewatch_uses_unwatch_ping_barrier_before_accepting_reset_replay() {
    let mut rig = Rig::start();
    let mut peer = rig.connect(1, &[]).await;
    rig.watch(&mut peer, 80).await;
    rig.handle().watch(vec!["task.".into()], 0).unwrap();
    rig.commands.send(ClientCommand::Discover).unwrap();
    assert!(matches!(peer.recv().await, MessageType::Unwatch));
    let MessageType::Ping { seq } = peer.recv().await else {
        panic!("barrier ping")
    };
    assert!(matches!(peer.recv().await, MessageType::Discover));
    peer.send(event(90)).await;
    peer.send(DaemonMessage::pong(seq)).await;
    assert!(matches!(
        peer.recv().await,
        MessageType::Watch { cursor: 0, .. }
    ));
    peer.send(event(1)).await;
    assert!(matches!(
        rig.notice().await,
        ControlNotice::Event(DaemonMessage::Event { seq: 1, .. })
    ));
    rig.stop().await;
}

#[tokio::test]
async fn older_daemon_receives_only_legacy_frames_and_sessions_still_work() {
    let mut rig = Rig::start();
    let mut peer = rig.connect(0, &[]).await;
    rig.handle().watch(vec![], 0).unwrap();
    let result = guarded(request(rig.handle(), "task.list")).await.unwrap();
    assert!(matches!(result, Err(ControlFailure::Unsupported(_))));
    assert!(matches!(
        rig.notice().await,
        ControlNotice::WatchFailed(ControlFailure::Unsupported(_))
    ));
    rig.commands.send(ClientCommand::Discover).unwrap();
    assert!(matches!(peer.recv().await, MessageType::Discover));
    peer.send(DaemonMessage::session_removed(SessionId::new("old")))
        .await;
    assert!(
        matches!(guarded(rig.events.recv()).await, Some(Event::SessionRemoved(id)) if id == "old")
    );
    rig.stop().await;
}

#[tokio::test]
async fn watch_refusal_and_missing_method_do_not_disconnect_sessions() {
    let mut rig = Rig::start();
    let mut peer = rig.connect(1, &[]).await;
    let result = guarded(request(rig.handle(), "missing")).await.unwrap();
    assert!(matches!(result, Err(ControlFailure::Unsupported(_))));
    rig.watch(&mut peer, 0).await;
    peer.send(DaemonMessage::error_with_code(
        "reader unavailable",
        "unavailable",
    ))
    .await;
    assert!(matches!(peer.recv().await, MessageType::Unwatch));
    assert!(
        matches!(rig.notice().await, ControlNotice::WatchFailed(ControlFailure::Watch(e)) if e.code == "unavailable")
    );
    peer.send(DaemonMessage::session_removed(SessionId::new(
        "still-connected",
    )))
    .await;
    assert!(
        matches!(guarded(rig.events.recv()).await, Some(Event::SessionRemoved(id)) if id == "still-connected")
    );
    rig.stop().await;
}

#[tokio::test]
async fn disconnected_requests_are_not_resent_but_watch_is() {
    let mut rig = Rig::start();
    let mut peer = rig.connect(1, &["task.list"]).await;
    rig.watch(&mut peer, 5).await;
    let pending = request(rig.handle(), "task.list");
    assert!(matches!(peer.recv().await, MessageType::Request { .. }));
    drop(peer);
    assert!(matches!(
        guarded(pending).await.unwrap(),
        Err(ControlFailure::Disconnected)
    ));
    rig.disconnected().await;
    let mut peer = rig.connect(1, &["task.list"]).await;
    assert!(matches!(
        peer.recv().await,
        MessageType::Watch { cursor: 5, .. }
    ));
    rig.commands.send(ClientCommand::Discover).unwrap();
    assert!(matches!(peer.recv().await, MessageType::Discover));
    rig.stop().await;
}

#[tokio::test]
async fn unwatch_clears_reconnect_desire_and_buffered_events() {
    let mut rig = Rig::start();
    let mut peer = rig.connect(1, &[]).await;
    rig.watch(&mut peer, 5).await;
    rig.handle().unwatch().unwrap();
    rig.commands.send(ClientCommand::Discover).unwrap();
    assert!(matches!(peer.recv().await, MessageType::Unwatch));
    assert!(matches!(peer.recv().await, MessageType::Discover));
    peer.send(event(6)).await;
    peer.send(DaemonMessage::session_removed(SessionId::new("sentinel")))
        .await;
    loop {
        match guarded(rig.events.recv()).await.unwrap() {
            Event::Control(ControlNotice::Capabilities { .. }) => {}
            Event::SessionRemoved(id) if id == "sentinel" => break,
            other => panic!("unexpected event after Unwatch: {other:?}"),
        }
    }
    drop(peer);
    rig.disconnected().await;
    let mut peer = rig.connect(1, &[]).await;
    rig.commands.send(ClientCommand::Discover).unwrap();
    assert!(matches!(peer.recv().await, MessageType::Discover));
    rig.stop().await;
}

#[tokio::test]
async fn cancellation_completes_an_admitted_request_without_retry() {
    let mut rig = Rig::start();
    let mut peer = rig.connect(1, &["task.list"]).await;
    let pending = request(rig.handle(), "task.list");
    assert!(matches!(peer.recv().await, MessageType::Request { .. }));
    rig.stop().await;
    assert!(matches!(
        guarded(pending).await.unwrap(),
        Err(ControlFailure::Disconnected)
    ));
}

#[tokio::test]
async fn closing_events_ends_the_client_instead_of_redialling() {
    let mut rig = Rig::start();
    let _peer = rig.connect(1, &[]).await;
    let (_, replacement) = mpsc::unbounded_channel();
    drop(std::mem::replace(&mut rig.events, replacement));
    guarded(async {
        tokio::select! {
            result = &mut rig.run => result.unwrap(),
            _ = rig.listener.accept() => panic!("closed event channel triggered reconnect"),
        }
    })
    .await;
}

#[tokio::test]
async fn closing_commands_ends_the_client_instead_of_redialling() {
    let mut rig = Rig::start();
    let _peer = rig.connect(1, &[]).await;
    let (replacement, _) = mpsc::unbounded_channel();
    drop(std::mem::replace(&mut rig.commands, replacement));
    guarded(async {
        tokio::select! {
            result = &mut rig.run => result.unwrap(),
            _ = rig.listener.accept() => panic!("closed command channel triggered reconnect"),
        }
    })
    .await;
}

#[tokio::test]
async fn closing_events_during_reconnect_stops_before_redial() {
    let mut rig = Rig::start();
    let peer = rig.connect(1, &[]).await;
    drop(peer);
    rig.disconnected().await;
    let (_, replacement) = mpsc::unbounded_channel();
    drop(std::mem::replace(&mut rig.events, replacement));
    guarded(async {
        tokio::select! {
            result = &mut rig.run => result.unwrap(),
            _ = rig.listener.accept() => panic!("event sink closed during backoff but client redialled"),
        }
    })
    .await;
}

#[tokio::test]
async fn closing_commands_during_reconnect_stops_before_redial() {
    let mut rig = Rig::start();
    let peer = rig.connect(1, &[]).await;
    drop(peer);
    rig.disconnected().await;
    let (replacement, _) = mpsc::unbounded_channel();
    drop(std::mem::replace(&mut rig.commands, replacement));
    guarded(async {
        tokio::select! {
            result = &mut rig.run => result.unwrap(),
            _ = rig.listener.accept() => panic!("command channel closed during backoff but client redialled"),
        }
    })
    .await;
}

#[tokio::test]
async fn offline_unwatch_prevents_resuming_old_watch() {
    let mut rig = Rig::start();
    let mut peer = rig.connect(1, &[]).await;
    rig.watch(&mut peer, 5).await;
    drop(peer);
    rig.disconnected().await;
    rig.handle().unwatch().unwrap();
    rig.commands.send(ClientCommand::Discover).unwrap();
    let mut peer = rig.connect(1, &[]).await;
    loop {
        match peer.recv().await {
            MessageType::Discover => break,
            MessageType::Unwatch => {}
            message => panic!("offline Unwatch resurrected old subscription: {message:?}"),
        }
    }
    rig.stop().await;
}

#[tokio::test]
async fn offline_replacement_preserves_queued_request_and_discovery_order() {
    let mut rig = Rig::start();
    let mut peer = rig.connect(1, &["task.list"]).await;
    rig.watch(&mut peer, 5).await;
    drop(peer);
    rig.disconnected().await;
    let handle = rig.handle();
    let mut pending = Box::pin(handle.request(
        "task.list".into(),
        json!({"queued": true}),
        None,
        Duration::from_secs(120),
    ));
    // One poll submits the request before the later commands, without a sleep
    // or relying on when a spawned request task happens to run.
    std::future::poll_fn(|cx| {
        assert!(pending.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    rig.commands.send(ClientCommand::Discover).unwrap();
    handle.watch(vec!["future.".into()], 0).unwrap();
    rig.commands.send(ClientCommand::Discover).unwrap();
    let mut peer = rig.connect(1, &["task.list"]).await;
    let MessageType::Request { id, params, .. } = peer.recv().await else {
        panic!("queued request was overtaken")
    };
    assert_eq!(params, json!({"queued": true}));
    assert!(matches!(peer.recv().await, MessageType::Discover));
    assert!(
        matches!(peer.recv().await, MessageType::Watch { topics, cursor: 0 } if topics == ["future."])
    );
    assert!(matches!(peer.recv().await, MessageType::Discover));
    peer.send(DaemonMessage::response_ok(id, json!(null))).await;
    assert!(
        matches!(guarded(pending).await, Ok(ResponsePayload::Result(value)) if value.is_null())
    );
    rig.stop().await;
}

fn role_request(
    handle: ControlHandle,
    method: &'static str,
) -> JoinHandle<Result<ResponsePayload, ControlFailure>> {
    tokio::spawn(async move {
        let params = if method == "session.roles" {
            json!({"caller_server":"/private/caller-server"})
        } else {
            json!({"caller_server":"/private/caller-server","pane":"%42","role":null})
        };
        handle
            .request(
                method.into(),
                params,
                Some("%7".into()),
                Duration::from_secs(120),
            )
            .await
    })
}

// Breaks if either role name/context is lost, a stalled request blocks session
// frames, or a daemon refusal is treated as a reason to retry.
#[tokio::test]
async fn session_roles_preserve_context_and_session_flow() {
    for method in ["session.roles", "session.role_set"] {
        let mut rig = Rig::start();
        let mut peer = rig.connect(1, &[method]).await;
        for refused in [false, true] {
            let pending = role_request(rig.handle(), method);
            let MessageType::Request {
                id,
                method: actual,
                params,
                from_pane,
            } = peer.recv().await
            else {
                panic!("role request")
            };
            assert_eq!(actual, method);
            let expected = if method == "session.roles" {
                json!({"caller_server":"/private/caller-server"})
            } else {
                json!({"caller_server":"/private/caller-server","pane":"%42","role":null})
            };
            assert_eq!(params, expected);
            assert_eq!(from_pane.as_deref(), Some("%7"));
            peer.send(DaemonMessage::session_removed(SessionId::new(
                "while-role-stalls",
            )))
            .await;
            loop {
                match guarded(rig.events.recv()).await.unwrap() {
                    Event::Control(ControlNotice::Capabilities { .. }) => {}
                    Event::SessionRemoved(id) if id == "while-role-stalls" => break,
                    other => panic!("role blocked sessions: {other:?}"),
                }
            }
            if refused {
                peer.send(DaemonMessage::response_err(id, "refused", "foreign server"))
                    .await;
                assert!(
                    matches!(guarded(pending).await.unwrap(), Ok(ResponsePayload::Error(e)) if e.code == "refused")
                );
            } else {
                let value = if method == "session.roles" {
                    json!({"coordinator_panes":["%42"]})
                } else {
                    json!({"pane":"%42","role":null})
                };
                peer.send(DaemonMessage::response_ok(id, value.clone()))
                    .await;
                assert!(
                    matches!(guarded(pending).await.unwrap(), Ok(ResponsePayload::Result(v)) if v == value)
                );
            }
            rig.commands.send(ClientCommand::Discover).unwrap();
            assert!(matches!(peer.recv().await, MessageType::Discover));
        }
        rig.stop().await;
    }
}

// Exact advertised capabilities are independent: reads do not authorize writes.
#[tokio::test]
async fn session_roles_capability_refusal_preserves_discovery() {
    for method in ["session.roles", "session.role_set"] {
        let other = if method == "session.roles" {
            "session.role_set"
        } else {
            "session.roles"
        };
        for (minor, methods) in [(0, vec![method]), (1, vec![]), (1, vec![other])] {
            let mut rig = Rig::start();
            let mut peer = rig.connect(minor, &methods).await;
            assert!(
                matches!(guarded(role_request(rig.handle(), method)).await.unwrap(), Err(ControlFailure::Unsupported(name)) if name == method)
            );
            rig.commands.send(ClientCommand::Discover).unwrap();
            assert!(matches!(peer.recv().await, MessageType::Discover));
            rig.stop().await;
        }
    }
}

// A sent request is retired on disconnect; a canceled offline call must never
// become a role read or mutation after reconnect (Discover is the sentinel).
#[tokio::test]
async fn session_roles_disconnect_and_abandoned_queue_are_not_replayed() {
    for method in ["session.roles", "session.role_set"] {
        let mut rig = Rig::start();
        let mut peer = rig.connect(1, &[method]).await;
        let pending = role_request(rig.handle(), method);
        let MessageType::Request { id, .. } = peer.recv().await else {
            panic!("role request")
        };
        drop(peer);
        assert!(matches!(
            guarded(pending).await.unwrap(),
            Err(ControlFailure::Disconnected)
        ));
        rig.disconnected().await;
        let handle = rig.handle();
        let mut queued =
            Box::pin(handle.request(method.into(), json!({}), None, Duration::from_secs(120)));
        std::future::poll_fn(|cx| {
            assert!(queued.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        drop(queued);
        let mut peer = rig.connect(1, &[method]).await;
        rig.commands.send(ClientCommand::Discover).unwrap();
        assert!(matches!(peer.recv().await, MessageType::Discover));
        peer.send(DaemonMessage::response_ok(id, json!(null))).await;
        rig.commands.send(ClientCommand::Discover).unwrap();
        assert!(matches!(peer.recv().await, MessageType::Discover));
        rig.stop().await;
    }
}

fn kill_request(handle: ControlHandle) -> JoinHandle<Result<ResponsePayload, ControlFailure>> {
    tokio::spawn(async move {
        handle
            .request(
                "session.kill".into(),
                json!({"target": "%42", "caller_server": "/private/caller-server"}),
                Some("%7".into()),
                Duration::from_secs(120),
            )
            .await
    })
}

// Breaks if the session transport loses the kill target/context, blocks session
// frames behind its reply, or interprets a refusal as a reason to retry.
#[tokio::test]
async fn session_kill_preserves_target_context_and_session_flow() {
    let mut rig = Rig::start();
    let mut peer = rig.connect(1, &["session.kill"]).await;
    for refused in [false, true] {
        let pending = kill_request(rig.handle());
        let MessageType::Request {
            id,
            method,
            params,
            from_pane,
        } = peer.recv().await
        else {
            panic!("kill request")
        };
        assert_eq!(method, "session.kill");
        assert_eq!(
            params,
            json!({"target": "%42", "caller_server": "/private/caller-server"})
        );
        assert_eq!(from_pane.as_deref(), Some("%7"));
        peer.send(DaemonMessage::session_removed(SessionId::new(
            "while-kill-stalls",
        )))
        .await;
        loop {
            match guarded(rig.events.recv()).await.unwrap() {
                Event::Control(ControlNotice::Capabilities { .. }) => {}
                Event::SessionRemoved(id) if id == "while-kill-stalls" => break,
                other => panic!("session flow blocked: {other:?}"),
            }
        }
        if refused {
            peer.send(DaemonMessage::response_err(
                id,
                "unavailable",
                "wrong server",
            ))
            .await;
            assert!(
                matches!(guarded(pending).await.unwrap(), Ok(ResponsePayload::Error(e))
                if e.code == "unavailable" && e.message == "wrong server")
            );
        } else {
            peer.send(DaemonMessage::response_ok(id, json!({"pane": "%42"})))
                .await;
            assert!(
                matches!(guarded(pending).await.unwrap(), Ok(ResponsePayload::Result(v))
                if v == json!({"pane": "%42"}))
            );
        }
        rig.commands.send(ClientCommand::Discover).unwrap();
        assert!(matches!(peer.recv().await, MessageType::Discover));
    }
    rig.stop().await;
}

// Even a 1.0 peer advertising the name must not receive a control request.
#[tokio::test]
async fn session_kill_capability_refusal_keeps_sessions_and_discovery() {
    for (minor, methods) in [
        (0, vec!["session.kill"]),
        (1, vec![]),
        (1, vec!["session.capture"]),
    ] {
        let mut rig = Rig::start();
        let mut peer = rig.connect(minor, &methods).await;
        assert!(matches!(guarded(kill_request(rig.handle())).await.unwrap(),
            Err(ControlFailure::Unsupported(method)) if method == "session.kill"));
        rig.commands.send(ClientCommand::Discover).unwrap();
        assert!(matches!(peer.recv().await, MessageType::Discover));
        peer.send(DaemonMessage::session_removed(SessionId::new(
            "still-connected",
        )))
        .await;
        loop {
            match guarded(rig.events.recv()).await.unwrap() {
                Event::Control(ControlNotice::Capabilities { .. }) => {}
                Event::SessionRemoved(id) if id == "still-connected" => break,
                other => panic!("capability failure broke sessions: {other:?}"),
            }
        }
        rig.stop().await;
    }
}

#[tokio::test]
async fn session_kill_disconnect_is_not_replayed() {
    let mut rig = Rig::start();
    let mut peer = rig.connect(1, &["session.kill"]).await;
    let pending = kill_request(rig.handle());
    let MessageType::Request { id, .. } = peer.recv().await else {
        panic!("kill request")
    };
    drop(peer);
    assert!(matches!(
        guarded(pending).await.unwrap(),
        Err(ControlFailure::Disconnected)
    ));
    rig.disconnected().await;
    let mut peer = rig.connect(1, &["session.kill"]).await;
    rig.commands.send(ClientCommand::Discover).unwrap();
    assert!(matches!(peer.recv().await, MessageType::Discover));
    // An old response cannot become a request or interfere with Sessions.
    peer.send(DaemonMessage::response_ok(id, json!({"pane": "%42"})))
        .await;
    peer.send(DaemonMessage::session_removed(SessionId::new(
        "after-reconnect",
    )))
    .await;
    loop {
        match guarded(rig.events.recv()).await.unwrap() {
            Event::Control(ControlNotice::Capabilities { .. }) => {}
            Event::SessionRemoved(id) if id == "after-reconnect" => break,
            other => panic!("old kill leaked into new connection: {other:?}"),
        }
    }
    rig.stop().await;
}

#[tokio::test]
async fn session_kill_cancelled_queued_call_is_not_replayed() {
    let mut rig = Rig::start();
    let handle = rig.handle();
    let mut pending = Box::pin(handle.request(
        "session.kill".into(),
        json!({"target": "%42", "caller_server": "/private/caller-server"}),
        Some("%7".into()),
        Duration::from_secs(120),
    ));
    std::future::poll_fn(|cx| {
        assert!(pending.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    drop(pending);
    rig.commands.send(ClientCommand::Discover).unwrap();
    let mut peer = rig.connect(1, &["session.kill"]).await;
    assert!(matches!(peer.recv().await, MessageType::Discover));
    rig.stop().await;
}

const SPAWN_METHODS: [&str; 3] = [
    "session.spawn_prepare_largest",
    "session.spawn",
    "session.spawn_type",
];

fn staged_request(
    handle: ControlHandle,
    method: &'static str,
    params: serde_json::Value,
) -> JoinHandle<Result<ResponsePayload, ControlFailure>> {
    tokio::spawn(async move {
        handle
            .request(
                method.into(),
                params,
                Some("%7".into()),
                Duration::from_secs(40),
            )
            .await
    })
}

#[tokio::test]
async fn staged_spawn_methods_preserve_context_and_session_traffic() {
    let mut rig = Rig::start();
    let mut peer = rig.connect(1, &SPAWN_METHODS).await;
    for (method, params, result) in [
        (
            SPAWN_METHODS[0],
            json!({"caller_server":"/private/caller-server","caller_pane":"%7"}),
            json!({"target_pane":"%42","cwd":"/client"}),
        ),
        (
            SPAWN_METHODS[1],
            json!({"caller_server":"/private/caller-server","target_pane":"%42","caller_pane_missing":false,"direction":"left","size":"50%","cwd":"/caller process directory"}),
            json!({"pane":"%99"}),
        ),
        (
            SPAWN_METHODS[2],
            json!({"caller_server":"/private/caller-server","pane":"%99","command":"cd '/client' && inert-agent;"}),
            serde_json::Value::Null,
        ),
    ] {
        let pending = staged_request(rig.handle(), method, params.clone());
        let MessageType::Request {
            id,
            method: got,
            params: actual,
            from_pane,
        } = peer.recv().await
        else {
            panic!("spawn request")
        };
        assert_eq!(got, method);
        assert_eq!(actual, params);
        assert_eq!(from_pane.as_deref(), Some("%7"));
        peer.send(DaemonMessage::session_removed(SessionId::new(
            "spawn-in-flight",
        )))
        .await;
        loop {
            match guarded(rig.events.recv()).await.unwrap() {
                Event::Control(_) => {}
                Event::SessionRemoved(id) if id == "spawn-in-flight" => break,
                other => panic!("spawn blocked session flow {other:?}"),
            }
        }
        peer.send(DaemonMessage::response_ok(id, result.clone()))
            .await;
        assert!(
            matches!(guarded(pending).await.unwrap(),Ok(ResponsePayload::Result(value)) if value==result)
        );
        rig.commands.send(ClientCommand::Discover).unwrap();
        assert!(matches!(peer.recv().await, MessageType::Discover));
    }
    rig.stop().await;
}

#[tokio::test]
async fn staged_spawn_capability_and_disconnect_never_replay_any_stage() {
    for method in SPAWN_METHODS {
        for (minor, methods) in [
            (0, SPAWN_METHODS.to_vec()),
            (1, vec![]),
            (
                1,
                SPAWN_METHODS
                    .into_iter()
                    .filter(|name| *name != method)
                    .collect(),
            ),
        ] {
            let mut rig = Rig::start();
            let mut peer = rig.connect(minor, &methods).await;
            assert!(
                matches!(guarded(staged_request(rig.handle(),method,json!({}))).await.unwrap(),Err(ControlFailure::Unsupported(name)) if name==method)
            );
            rig.commands.send(ClientCommand::Discover).unwrap();
            assert!(matches!(peer.recv().await, MessageType::Discover));
            rig.stop().await;
        }
        let mut rig = Rig::start();
        let handle = rig.handle();
        let mut queued = Box::pin(handle.request(
            method.into(),
            json!({}),
            Some("%7".into()),
            Duration::from_secs(40),
        ));
        std::future::poll_fn(|cx| {
            assert!(queued.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        drop(queued);
        rig.commands.send(ClientCommand::Discover).unwrap();
        let mut peer = rig.connect(1, &SPAWN_METHODS).await;
        assert!(matches!(peer.recv().await, MessageType::Discover));
        let pending = staged_request(rig.handle(), method, json!({}));
        assert!(matches!(peer.recv().await,MessageType::Request{method:got,..} if got==method));
        drop(peer);
        assert!(matches!(
            guarded(pending).await.unwrap(),
            Err(ControlFailure::Disconnected)
        ));
        rig.disconnected().await;
        let mut peer = rig.connect(1, &SPAWN_METHODS).await;
        rig.commands.send(ClientCommand::Discover).unwrap();
        assert!(matches!(peer.recv().await, MessageType::Discover));
        rig.stop().await;
    }
}

fn panes_request(handle: ControlHandle) -> JoinHandle<Result<ResponsePayload, ControlFailure>> {
    tokio::spawn(async move {
        handle
            .request(
                "session.panes".into(),
                json!({"session_name":"work","caller_server":"/private/caller-server"}),
                Some("%7".into()),
                Duration::from_secs(40),
            )
            .await
    })
}

// Breaks if membership drops its literal session/context, blocks subscription
// traffic, or retries a refused observation on the session connection.
#[tokio::test]
async fn session_panes_preserves_context_and_session_flow() {
    let mut rig = Rig::start();
    let mut peer = rig.connect(1, &["session.panes"]).await;
    for result in [
        Some(json!({"pane_ids":["%0","%2"]})),
        Some(json!({"pane_ids":[]})),
        None,
    ] {
        let pending = panes_request(rig.handle());
        let MessageType::Request {
            id,
            method,
            params,
            from_pane,
        } = peer.recv().await
        else {
            panic!("membership request")
        };
        assert_eq!(method, "session.panes");
        assert_eq!(
            params,
            json!({"session_name":"work","caller_server":"/private/caller-server"})
        );
        assert_eq!(from_pane.as_deref(), Some("%7"));
        peer.send(DaemonMessage::session_removed(SessionId::new(
            "membership-in-flight",
        )))
        .await;
        loop {
            match guarded(rig.events.recv()).await.unwrap() {
                Event::Control(ControlNotice::Capabilities { .. }) => {}
                Event::SessionRemoved(id) if id == "membership-in-flight" => break,
                other => panic!("membership blocked sessions: {other:?}"),
            }
        }
        if let Some(value) = result {
            peer.send(DaemonMessage::response_ok(id, value.clone()))
                .await;
            assert!(
                matches!(guarded(pending).await.unwrap(), Ok(ResponsePayload::Result(v)) if v == value)
            );
        } else {
            peer.send(DaemonMessage::response_err(id, "refused", "foreign server"))
                .await;
            assert!(
                matches!(guarded(pending).await.unwrap(), Ok(ResponsePayload::Error(e)) if e.code == "refused")
            );
        }
        rig.commands.send(ClientCommand::Discover).unwrap();
        assert!(matches!(peer.recv().await, MessageType::Discover));
    }
    rig.stop().await;
}

#[tokio::test]
async fn session_panes_capability_refusal_preserves_discovery() {
    for (minor, methods) in [
        (0, vec!["session.panes"]),
        (1, vec![]),
        (1, vec!["session.capture"]),
    ] {
        let mut rig = Rig::start();
        let mut peer = rig.connect(minor, &methods).await;
        assert!(
            matches!(guarded(panes_request(rig.handle())).await.unwrap(), Err(ControlFailure::Unsupported(method)) if method == "session.panes")
        );
        rig.commands.send(ClientCommand::Discover).unwrap();
        assert!(matches!(peer.recv().await, MessageType::Discover));
        rig.stop().await;
    }
}

#[tokio::test]
async fn session_panes_disconnect_and_abandoned_queue_are_not_replayed() {
    let mut rig = Rig::start();
    let handle = rig.handle();
    let mut queued = Box::pin(handle.request(
        "session.panes".into(),
        json!({"session_name":"work","caller_server":"/private/caller-server"}),
        None,
        Duration::from_secs(40),
    ));
    std::future::poll_fn(|cx| {
        assert!(queued.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    drop(queued);
    rig.commands.send(ClientCommand::Discover).unwrap();
    let mut peer = rig.connect(1, &["session.panes"]).await;
    assert!(matches!(peer.recv().await, MessageType::Discover));
    let pending = panes_request(rig.handle());
    assert!(matches!(peer.recv().await, MessageType::Request { .. }));
    drop(peer);
    assert!(matches!(
        guarded(pending).await.unwrap(),
        Err(ControlFailure::Disconnected)
    ));
    rig.disconnected().await;
    let mut peer = rig.connect(1, &["session.panes"]).await;
    rig.commands.send(ClientCommand::Discover).unwrap();
    assert!(matches!(peer.recv().await, MessageType::Discover));
    rig.stop().await;
}

fn interrupt_request(handle: ControlHandle) -> JoinHandle<Result<ResponsePayload, ControlFailure>> {
    tokio::spawn(async move {
        handle
            .request(
                "session.interrupt".into(),
                json!({"target": "%42", "caller_server": "/private/caller-server"}),
                Some("%7".into()),
                Duration::from_secs(120),
            )
            .await
    })
}

// Breaks if the interrupt family loses attribution/target, blocks session flow,
// or retries after a clearing/quarantine refusal.
#[tokio::test]
async fn session_interrupt_preserves_target_context_and_session_flow() {
    let mut rig = Rig::start();
    let mut peer = rig.connect(1, &["session.interrupt"]).await;
    for refusal in [
        None,
        Some(("refused", "agent is clearing")),
        Some(("unavailable", "agent is quarantined")),
    ] {
        let pending = interrupt_request(rig.handle());
        let MessageType::Request {
            id,
            method,
            params,
            from_pane,
        } = peer.recv().await
        else {
            panic!("interrupt request")
        };
        assert_eq!(method, "session.interrupt");
        assert_eq!(
            params,
            json!({"target": "%42", "caller_server": "/private/caller-server"})
        );
        assert_eq!(from_pane.as_deref(), Some("%7"));
        peer.send(DaemonMessage::session_removed(SessionId::new(
            "while-interrupt-stalls",
        )))
        .await;
        loop {
            match guarded(rig.events.recv()).await.unwrap() {
                Event::Control(ControlNotice::Capabilities { .. }) => {}
                Event::SessionRemoved(id) if id == "while-interrupt-stalls" => break,
                other => panic!("interrupt blocked sessions: {other:?}"),
            }
        }
        if let Some((code, message)) = refusal {
            peer.send(DaemonMessage::response_err(id, code, message))
                .await;
            assert!(
                matches!(guarded(pending).await.unwrap(), Ok(ResponsePayload::Error(e))
                if e.code == code && e.message == message)
            );
        } else {
            peer.send(DaemonMessage::response_ok(id, json!({"pane": "%42"})))
                .await;
            assert!(
                matches!(guarded(pending).await.unwrap(), Ok(ResponsePayload::Result(v))
                if v == json!({"pane": "%42"}))
            );
        }
        rig.commands.send(ClientCommand::Discover).unwrap();
        assert!(matches!(peer.recv().await, MessageType::Discover));
    }
    rig.stop().await;
}

#[tokio::test]
async fn session_interrupt_capability_refusal_keeps_sessions_and_discovery() {
    for (minor, methods) in [
        (0, vec!["session.interrupt"]),
        (1, vec![]),
        (1, vec!["session.kill"]),
    ] {
        let mut rig = Rig::start();
        let mut peer = rig.connect(minor, &methods).await;
        assert!(
            matches!(guarded(interrupt_request(rig.handle())).await.unwrap(),
            Err(ControlFailure::Unsupported(method)) if method == "session.interrupt")
        );
        rig.commands.send(ClientCommand::Discover).unwrap();
        assert!(matches!(peer.recv().await, MessageType::Discover));
        peer.send(DaemonMessage::session_removed(SessionId::new(
            "still-connected",
        )))
        .await;
        loop {
            match guarded(rig.events.recv()).await.unwrap() {
                Event::Control(ControlNotice::Capabilities { .. }) => {}
                Event::SessionRemoved(id) if id == "still-connected" => break,
                other => panic!("capability refusal broke sessions: {other:?}"),
            }
        }
        rig.stop().await;
    }
}

#[tokio::test]
async fn session_interrupt_disconnect_is_not_replayed() {
    let mut rig = Rig::start();
    let mut peer = rig.connect(1, &["session.interrupt"]).await;
    let pending = interrupt_request(rig.handle());
    assert!(matches!(peer.recv().await, MessageType::Request { .. }));
    drop(peer);
    assert!(matches!(
        guarded(pending).await.unwrap(),
        Err(ControlFailure::Disconnected)
    ));
    rig.disconnected().await;
    let mut peer = rig.connect(1, &["session.interrupt"]).await;
    rig.commands.send(ClientCommand::Discover).unwrap();
    assert!(matches!(peer.recv().await, MessageType::Discover));
    rig.stop().await;
}

#[tokio::test]
async fn session_interrupt_cancelled_queued_call_is_not_replayed() {
    let mut rig = Rig::start();
    let handle = rig.handle();
    let mut pending = Box::pin(handle.request(
        "session.interrupt".into(),
        json!({"target": "%42", "caller_server": "/private/caller-server"}),
        Some("%7".into()),
        Duration::from_secs(120),
    ));
    std::future::poll_fn(|cx| {
        assert!(pending.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    drop(pending);
    rig.commands.send(ClientCommand::Discover).unwrap();
    let mut peer = rig.connect(1, &["session.interrupt"]).await;
    assert!(matches!(peer.recv().await, MessageType::Discover));
    rig.stop().await;
}

// An admitted-but-abandoned interrupt must not be replayed or have its late
// reply mistaken for the next explicit keypress's response.
#[tokio::test]
async fn session_interrupt_late_reply_cannot_complete_a_fresh_request() {
    let mut rig = Rig::start();
    let mut peer = rig.connect(1, &["session.interrupt"]).await;
    let first = interrupt_request(rig.handle());
    let MessageType::Request { id: old_id, .. } = peer.recv().await else {
        panic!("first request")
    };
    first.abort();
    assert!(guarded(first).await.unwrap_err().is_cancelled());
    let mut next = interrupt_request(rig.handle());
    let MessageType::Request { id, .. } = peer.recv().await else {
        panic!("fresh request")
    };
    assert_ne!(id, old_id);
    peer.send(DaemonMessage::response_ok(old_id, json!({"pane": "%42"})))
        .await;
    peer.send(DaemonMessage::session_removed(SessionId::new(
        "late-reply-barrier",
    )))
    .await;
    loop {
        match guarded(rig.events.recv()).await.unwrap() {
            Event::Control(ControlNotice::Capabilities { .. }) => {}
            Event::SessionRemoved(id) if id == "late-reply-barrier" => break,
            other => panic!("late reply broke sessions: {other:?}"),
        }
    }
    std::future::poll_fn(|cx| {
        assert!(
            std::pin::Pin::new(&mut next).poll(cx).is_pending(),
            "late reply completed fresh interrupt"
        );
        Poll::Ready(())
    })
    .await;
    peer.send(DaemonMessage::response_err(id, "refused", "fresh refusal"))
        .await;
    assert!(
        matches!(guarded(next).await.unwrap(), Ok(ResponsePayload::Error(e)) if e.message == "fresh refusal")
    );
    rig.commands.send(ClientCommand::Discover).unwrap();
    assert!(matches!(peer.recv().await, MessageType::Discover));
    rig.stop().await;
}

fn capture_request(handle: ControlHandle) -> JoinHandle<Result<ResponsePayload, ControlFailure>> {
    tokio::spawn(async move {
        handle.request(
            "session.capture".into(),
            json!({"target": "%42", "tail": null, "prompt": false, "caller_server": "/private/caller-server"}),
            Some("%7".into()), Duration::from_secs(40),
        ).await
    })
}

// A capture must use the selected pane, not the caller/session ID, and must
// remain independent of the Sessions and Discover traffic on this connection.
#[tokio::test]
async fn session_capture_preserves_target_context_and_session_flow() {
    let mut rig = Rig::start();
    let mut peer = rig.connect(1, &["session.capture"]).await;
    let pending = capture_request(rig.handle());
    let MessageType::Request {
        id,
        method,
        params,
        from_pane,
    } = peer.recv().await
    else {
        panic!("capture request")
    };
    assert_eq!(method, "session.capture");
    assert_eq!(
        params,
        json!({"target": "%42", "tail": null, "prompt": false, "caller_server": "/private/caller-server"})
    );
    assert_eq!(from_pane.as_deref(), Some("%7"));
    peer.send(DaemonMessage::session_removed(SessionId::new(
        "capture-stalled",
    )))
    .await;
    loop {
        match guarded(rig.events.recv()).await.unwrap() {
            Event::Control(ControlNotice::Capabilities { .. }) => {}
            Event::SessionRemoved(id) if id == "capture-stalled" => break,
            other => panic!("capture blocked sessions: {other:?}"),
        }
    }
    rig.commands.send(ClientCommand::Discover).unwrap();
    assert!(matches!(peer.recv().await, MessageType::Discover));
    peer.send(DaemonMessage::response_ok(
        id,
        json!({"lines": ["first", "", "café λ"]}),
    ))
    .await;
    assert!(
        matches!(guarded(pending).await.unwrap(), Ok(ResponsePayload::Result(v)) if v == json!({"lines": ["first", "", "café λ"]}))
    );
    rig.stop().await;
}

#[tokio::test]
async fn session_capture_capability_refusal_emits_zero_frames() {
    for (minor, methods) in [
        (0, vec!["session.capture"]),
        (1, vec![]),
        (1, vec!["session.interrupt"]),
    ] {
        let mut rig = Rig::start();
        let mut peer = rig.connect(minor, &methods).await;
        assert!(
            matches!(guarded(capture_request(rig.handle())).await.unwrap(), Err(ControlFailure::Unsupported(method)) if method == "session.capture")
        );
        rig.commands.send(ClientCommand::Discover).unwrap();
        assert!(matches!(peer.recv().await, MessageType::Discover));
        rig.stop().await;
    }
}

#[tokio::test]
async fn session_capture_disconnect_and_cancel_are_not_replayed() {
    let mut rig = Rig::start();
    let mut peer = rig.connect(1, &["session.capture"]).await;
    let pending = capture_request(rig.handle());
    assert!(matches!(peer.recv().await, MessageType::Request { .. }));
    drop(peer);
    assert!(matches!(
        guarded(pending).await.unwrap(),
        Err(ControlFailure::Disconnected)
    ));
    rig.disconnected().await;
    let handle = rig.handle();
    let mut abandoned = Box::pin(handle.request(
        "session.capture".into(),
        json!({"target": "%old"}),
        None,
        Duration::from_secs(40),
    ));
    std::future::poll_fn(|cx| {
        assert!(abandoned.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    drop(abandoned);
    rig.commands.send(ClientCommand::Discover).unwrap();
    let mut peer = rig.connect(1, &["session.capture"]).await;
    assert!(matches!(peer.recv().await, MessageType::Discover));
    rig.stop().await;
}

#[tokio::test]
async fn session_capture_late_reply_cannot_complete_a_new_selection() {
    let mut rig = Rig::start();
    let mut peer = rig.connect(1, &["session.capture"]).await;
    let first = capture_request(rig.handle());
    let MessageType::Request { id: old, .. } = peer.recv().await else {
        panic!("old capture")
    };
    first.abort();
    assert!(guarded(first).await.unwrap_err().is_cancelled());
    let mut next = capture_request(rig.handle());
    let MessageType::Request { id, .. } = peer.recv().await else {
        panic!("new capture")
    };
    assert_ne!(old, id);
    peer.send(DaemonMessage::response_ok(
        old,
        json!({"lines": ["old selection"]}),
    ))
    .await;
    peer.send(DaemonMessage::session_removed(SessionId::new(
        "late-capture-barrier",
    )))
    .await;
    loop {
        match guarded(rig.events.recv()).await.unwrap() {
            Event::Control(ControlNotice::Capabilities { .. }) => {}
            Event::SessionRemoved(id) if id == "late-capture-barrier" => break,
            other => panic!("late reply broke sessions: {other:?}"),
        }
    }
    std::future::poll_fn(|cx| {
        assert!(std::pin::Pin::new(&mut next).poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    peer.send(DaemonMessage::response_ok(
        id,
        json!({"lines": ["new selection"]}),
    ))
    .await;
    assert!(
        matches!(guarded(next).await.unwrap(), Ok(ResponsePayload::Result(v)) if v == json!({"lines": ["new selection"]}))
    );
    rig.stop().await;
}

#[tokio::test]
async fn closing_channels_during_handshake_stops_before_subscribe() {
    for close_commands in [false, true] {
        let mut rig = Rig::start();
        let (stream, _) = guarded(rig.listener.accept()).await.unwrap();
        let (reader, writer) = stream.into_split();
        let mut peer = Peer {
            reader: BufReader::new(reader),
            writer,
        };
        assert!(matches!(peer.recv().await, MessageType::Connect { .. }));
        if close_commands {
            let (replacement, _) = mpsc::unbounded_channel();
            drop(std::mem::replace(&mut rig.commands, replacement));
        } else {
            let (_, replacement) = mpsc::unbounded_channel();
            drop(std::mem::replace(&mut rig.events, replacement));
        }
        let reply = format!(
            "{}\n",
            serde_json::to_string(&DaemonMessage::Connected {
                protocol_version: ProtocolVersion::CURRENT,
                client_id: "closing".into(),
                methods: vec![],
            })
            .unwrap()
        );
        // Closing the client may race this write; either outcome is normal.
        let _ = guarded(peer.writer.write_all(reply.as_bytes())).await;
        let mut line = String::new();
        guarded(async {
            tokio::select! {
                result = &mut rig.run => result.unwrap(),
                read = peer.reader.read_line(&mut line) => {
                    match read {
                        Ok(0) => {},
                        Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {},
                        other => panic!("closed channel still sent {line}: {other:?}"),
                    }
                    assert!(line.is_empty(), "closed channel sent a partial frame: {line}");
                    guarded(&mut rig.run).await.unwrap();
                }
            }
        })
        .await;
    }
}
