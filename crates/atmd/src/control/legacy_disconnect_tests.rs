//! Real socket disconnects must reach the existing first-effect cancellation fence.
#[path = "legacy_disconnect_smoke.rs"]
mod smoke;
use super::*;
use crate::server::{methods, ConnectionHandler};
use atm_protocol::{ClientMessage, DaemonMessage, ResponsePayload};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    sync::{RwLock, Semaphore},
};

/// The existing server removal log is emitted after the real handler returned.
/// These tests use a current-thread runtime, so its task cannot resume the
/// waiter until the removal's lock scope has also ended.
struct ServerEvent {
    message: &'static str,
    ready: Arc<Semaphore>,
}
impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for ServerEvent {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _context: tracing_subscriber::layer::Context<'_, S>,
    ) {
        struct Message<'a>(&'a ServerEvent);
        impl tracing::field::Visit for Message<'_> {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" && format!("{value:?}") == self.0.message {
                    self.0.ready.add_permits(1);
                }
            }
        }
        if event.metadata().target().starts_with("atmd::server") {
            event.record(&mut Message(self));
        }
    }
}

fn server_event(message: &'static str) -> (Arc<Semaphore>, tracing::subscriber::DefaultGuard) {
    use tracing_subscriber::prelude::*;
    let ready = Arc::new(Semaphore::new(0));
    let logs = tracing::subscriber::set_default(tracing_subscriber::registry().with(ServerEvent {
        message,
        ready: ready.clone(),
    }));
    (ready, logs)
}

struct ObservedIdentity {
    mock: MockTmuxClient,
    called: tokio::sync::mpsc::UnboundedSender<()>,
}
impl TmuxClient for ObservedIdentity {
    fn raw_command<'s, 'c, 'a, 'v, 'async_trait>(
        &'s self,
        command: &'c str,
        args: &'a [&'v str],
    ) -> Call<'async_trait, String>
    where
        's: 'async_trait,
        'c: 'async_trait,
        'a: 'async_trait,
        'v: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            let result = self.mock.raw_command(command, args).await;
            if command == "display-message" && args.contains(&"#{socket_path}") {
                let _ = self.called.send(());
            }
            result
        })
    }
    forward!(paste_text<'a, 'b>(pane: &'a str, text: &'b str) -> ());
    forward!(send_keys<'a, 'b>(pane: &'a str, keys: &'b str) -> ());
    forward!(split_window<'a, 'b, 'c, 'd>(target: &'a str, size: &'b str, direction: PaneDirection, command: Option<&'c str>, cwd: Option<&'d str>, detached: bool) -> String);
    forward!(get_pane_cwd<'a>(pane: &'a str) -> Option<String>);
    forward!(new_window<'a, 'b>(session: &'a str, command: Option<&'b str>) -> String);
    forward!(kill_pane<'a>(pane: &'a str) -> ());
    forward!(resize_pane<'a>(pane: &'a str, width: Option<u16>, height: Option<u16>) -> ());
    forward!(list_panes<>() -> Vec<PaneInfo>);
    forward!(display_popup<'a, 'b, 'c>(width: &'a str, height: &'b str, command: &'c str) -> ());
    forward!(select_pane<'a>(pane: &'a str) -> ());
    forward!(capture_pane<'a>(pane: &'a str) -> Vec<String>);
    forward!(new_session<'a>(name: &'a str) -> String);
}

struct Connection {
    peer: BufReader<UnixStream>,
    run: JoinHandle<Option<String>>,
    admitted: Arc<Semaphore>,
    returned: Arc<AtomicBool>,
    gate_entered: Arc<Semaphore>,
    gate: Arc<Semaphore>,
}

async fn write(peer: &mut BufReader<UnixStream>, message: ClientMessage) {
    let mut bytes = serde_json::to_vec(&message).unwrap();
    bytes.push(b'\n');
    peer.get_mut().write_all(&bytes).await.unwrap();
}
async fn read(peer: &mut BufReader<UnixStream>) -> DaemonMessage {
    let mut line = String::new();
    assert_ne!(peer.read_line(&mut line).await.unwrap(), 0);
    serde_json::from_str(&line).unwrap()
}

async fn connection(
    control: ControlHandle,
    registry: crate::registry::RegistryHandle,
) -> Connection {
    let mut services = methods::MethodServices::new(registry.clone());
    services.control = Some(control);
    let real = Arc::new(methods::router(services).unwrap());
    let admitted = Arc::new(Semaphore::new(0));
    let signal = admitted.clone();
    let returned = Arc::new(AtomicBool::new(false));
    let finished = returned.clone();
    let mut router = methods::MethodRouter::default();
    router
        .register(
            "session.send",
            Duration::from_secs(60),
            move |caller, params| {
                let real = real.clone();
                let signal = signal.clone();
                let finished = finished.clone();
                async move {
                    let mut call = Box::pin(real.call(caller, "session.send", params));
                    // Poll the real handler through enqueue, then expose an admission
                    // barrier. No timing assumptions about spawned request tasks.
                    std::future::poll_fn(|cx| match call.as_mut().poll(cx) {
                        Poll::Pending => Poll::Ready(()),
                        Poll::Ready(_) => panic!("send should be held by the fixture"),
                    })
                    .await;
                    signal.add_permits(1);
                    let result = call.await;
                    finished.store(true, Ordering::SeqCst);
                    result
                }
            },
        )
        .unwrap();
    let gate_entered = Arc::new(Semaphore::new(0));
    let gate = Arc::new(Semaphore::new(0));
    let (entered, held) = (gate_entered.clone(), gate.clone());
    router
        .register("gate", Duration::from_secs(60), move |_, _| {
            let (entered, held) = (entered.clone(), held.clone());
            async move {
                entered.add_permits(1);
                held.acquire().await.unwrap().forget();
                Ok(serde_json::Value::Null)
            }
        })
        .unwrap();
    let (ours, peer) = UnixStream::pair().unwrap();
    let (reader, writer) = ours.into_split();
    let handler = ConnectionHandler::new(
        reader,
        writer,
        registry,
        Arc::new(RwLock::new(HashMap::new())),
        1,
        CancellationToken::new(),
        Arc::new(router),
    )
    .with_disconnect_monitor()
    .unwrap();
    let run = tokio::spawn(handler.run());
    let mut peer = BufReader::new(peer);
    write(&mut peer, ClientMessage::connect(None)).await;
    assert!(matches!(
        read(&mut peer).await,
        DaemonMessage::Connected { .. }
    ));
    Connection {
        peer,
        run,
        admitted,
        returned,
        gate_entered,
        gate,
    }
}

async fn send(connection: &mut Connection) {
    write(
        &mut connection.peer,
        ClientMessage::request(
            "send".into(),
            "session.send".into(),
            serde_json::json!({"target":"%1", "text":"once"}),
            None,
        ),
    )
    .await;
    connection.admitted.acquire().await.unwrap().forget();
}

async fn parked(half_close: bool, then_disconnect: bool) {
    let mock = MockTmuxClient::new();
    let registry = crate::registry::spawn_registry();
    let cancel = CancellationToken::new();
    let (control, actor) = spawn_control(
        Box::new(mock.clone()),
        registry.clone(),
        OrgHandle::disabled("disconnect test"),
        cancel.clone(),
    );
    let (job, release) = hold(&control, ReservationKind::Delivery).await;
    let mut connection = connection(control.clone(), registry).await;
    send(&mut connection).await;
    barrier(&control).await;
    assert!(input_calls(&mock).is_empty());
    if half_close {
        for id in ["gate-a", "gate-b"] {
            write(
                &mut connection.peer,
                ClientMessage::request(id.into(), "gate".into(), serde_json::Value::Null, None),
            )
            .await;
            connection.gate_entered.acquire().await.unwrap().forget();
        }
        connection.peer.get_mut().shutdown().await.unwrap();
        // The first reply requires a driver turn after shutdown. The second
        // requires a later handler poll, whose biased select polls the monitor
        // first. Keep the reservation held until both replies have arrived, so
        // mistaking READ_CLOSED for full closure cannot race behind the send.
        for expected in ["gate-a", "gate-b"] {
            connection.gate.add_permits(1);
            let reply = read(&mut connection.peer).await;
            assert!(
                matches!(&reply, DaemonMessage::Response { id, payload: ResponsePayload::Result(value) }
                    if id == expected && value.is_null()),
                "half-close lost the parked send before {expected}: {reply:?}"
            );
        }
        assert!(input_calls(&mock).is_empty());
    }
    if !half_close || then_disconnect {
        drop(connection.peer);
        // Must finish while the reservation is still held: the server has
        // dropped the method future, not waited for the control deadline.
        connection.run.await.unwrap();
        assert!(
            !connection.returned.load(Ordering::SeqCst),
            "method returned instead of being dropped"
        );
        untagged(&mock, "%1");
        mock.set_next_raw_output("0 1");
        release.send(()).unwrap();
        job.await.unwrap().unwrap();
        control.flush_audits().await;
        assert!(input_calls(&mock).is_empty(), "abandoned send typed input");
    } else {
        untagged(&mock, "%1");
        mock.set_next_raw_output("0 1");
        mock_shell(&mock, "%1");
        release.send(()).unwrap();
        job.await.unwrap().unwrap();
        let reply = read(&mut connection.peer).await;
        assert!(
            matches!(&reply, DaemonMessage::Response { id, payload: ResponsePayload::Result(value) }
                if id == "send" && *value == serde_json::json!({"pane":"%1"})),
            "half-closed caller lost its parked send: {reply:?}"
        );
        connection.run.await.unwrap();
        assert_eq!(
            input_calls(&mock),
            vec![
                MockCall::PasteText {
                    pane: "%1".into(),
                    text: "once".into()
                },
                MockCall::SendKeys {
                    pane: "%1".into(),
                    keys: "Enter".into()
                },
            ]
        );
    }
    cancel.cancel();
    actor.await.unwrap();
}

#[tokio::test]
async fn full_disconnect_discards_parked_send() {
    tokio::time::timeout(Duration::from_secs(60), parked(false, false))
        .await
        .unwrap();
}

#[tokio::test]
async fn half_close_keeps_parked_send_and_delivers_reply() {
    tokio::time::timeout(Duration::from_secs(60), parked(true, false))
        .await
        .unwrap();
}

#[tokio::test]
async fn half_close_then_full_disconnect_discards_parked_send() {
    tokio::time::timeout(Duration::from_secs(60), parked(true, true))
        .await
        .unwrap();
}

#[tokio::test]
async fn daemon_disconnect_preserves_earlier_parked_survivor() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let mock = MockTmuxClient::new();
        let (called, mut identity_calls) = tokio::sync::mpsc::unbounded_channel();
        let (removed, _logs) = server_event("Removed disconnected subscriber");
        let registry = crate::registry::spawn_registry();
        let cancel = CancellationToken::new();
        let (control, actor) = spawn_control(
            Box::new(ObservedIdentity { mock: mock.clone(), called }), registry.clone(),
            OrgHandle::disabled("disconnect test"), cancel.clone(),
        );
        let mut services = methods::MethodServices::new(registry);
        services.control = Some(control.clone());
        let daemon = Arc::new(crate::server::DaemonServer::with_services(
            &socket, services, cancel.clone(),
        ));
        let serving = daemon.clone();
        let mut server = Box::pin(async move { serving.run().await });
        std::future::poll_fn(|cx| {
            assert!(server.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        }).await;
        let server = tokio::spawn(server);
        let (job, release) = hold(&control, ReservationKind::Delivery).await;
        let mut survivor = BufReader::new(UnixStream::connect(&socket).await.unwrap());
        let mut abandoned = BufReader::new(UnixStream::connect(&socket).await.unwrap());
        for peer in [&mut survivor, &mut abandoned] {
            write(peer, ClientMessage::connect(None)).await;
            assert!(matches!(read(peer).await, DaemonMessage::Connected { .. }));
        }
        write(&mut abandoned, ClientMessage::subscribe(None)).await;
        assert!(matches!(read(&mut abandoned).await, DaemonMessage::SessionList { .. }));
        assert_eq!(daemon.subscriber_count().await, 1);
        for (index, (peer, text)) in [(&mut survivor, "survivor"), (&mut abandoned, "abandoned")].into_iter().enumerate() {
            // Identity verification runs once per send inside actor dispatch,
            // unlike readiness notifications or repeatedly resolved aliases.
            // The fake reports the same existing private path as the caller.
            mock.set_next_raw_output(socket.to_str().unwrap());
            write(peer, ClientMessage::request(
                text.into(), "session.send".into(),
                serde_json::json!({"target":"%1", "text":text, "caller_server":socket}), None,
            )).await;
            identity_calls.recv().await.unwrap();
            assert_eq!(mock.calls().iter().filter(|call| matches!(call,
                MockCall::RawCommand { subcommand, args }
                    if subcommand == "display-message" && args.iter().any(|arg| arg == "#{socket_path}")
            )).count(), index + 1);
            barrier(&control).await; // each send is parked before the next starts
        }
        assert!(input_calls(&mock).is_empty());
        drop(abandoned);
        // DaemonServer removes this subscriber only after its handler finishes.
        // If the monitor was not attached, that requires the abandoned send's
        // deadline, by which time the earlier survivor has also expired.
        removed.acquire().await.unwrap().forget();
        assert_eq!(daemon.subscriber_count().await, 0);
        untagged(&mock, "%1");
        mock.set_next_raw_output("0 1");
        mock_shell(&mock, "%1");
        release.send(()).unwrap();
        job.await.unwrap().unwrap();
        let reply = read(&mut survivor).await;
        assert!(
            matches!(&reply, DaemonMessage::Response { id, payload: ResponsePayload::Result(value) }
                if id == "survivor" && *value == serde_json::json!({"pane":"%1"})),
            "earlier parked survivor must succeed, not time out: {reply:?}"
        );
        control.flush_audits().await;
        assert_eq!(input_calls(&mock), vec![
            MockCall::PasteText { pane: "%1".into(), text: "survivor".into() },
            MockCall::SendKeys { pane: "%1".into(), keys: "Enter".into() },
        ]);
        drop(survivor);
        cancel.cancel();
        actor.await.unwrap();
        server.await.unwrap().unwrap();
    }).await.unwrap();
}

#[tokio::test]
async fn full_disconnect_after_paste_still_finishes_enter() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let mock = MockTmuxClient::new();
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(Semaphore::new(0));
        let registry = crate::registry::spawn_registry();
        let cancel = CancellationToken::new();
        let (control, actor) = spawn_control(
            Box::new(DelayedClient {
                mock: mock.clone(),
                at: DelayAt::Paste,
                pause: Some((entered.clone(), release.clone())),
            }),
            registry.clone(),
            OrgHandle::disabled("disconnect test"),
            cancel.clone(),
        );
        untagged(&mock, "%1");
        mock.set_next_raw_output("0 1");
        mock_shell(&mock, "%1");
        let mut connection = connection(control.clone(), registry).await;
        send(&mut connection).await;
        entered.notified().await;
        drop(connection.peer);
        connection.run.await.unwrap();
        assert!(
            !connection.returned.load(Ordering::SeqCst),
            "method returned instead of being dropped"
        );
        release.add_permits(1);
        control.flush_audits().await;
        assert_eq!(
            input_calls(&mock),
            vec![
                MockCall::PasteText {
                    pane: "%1".into(),
                    text: "once".into()
                },
                MockCall::SendKeys {
                    pane: "%1".into(),
                    keys: "Enter".into()
                },
            ]
        );
        cancel.cancel();
        actor.await.unwrap();
    })
    .await
    .unwrap();
}
