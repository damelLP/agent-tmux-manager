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
