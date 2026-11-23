use super::{
    preview_for_push,
    watch::follow,
    watch_tests::{answer, connect, next, write},
    Notifications, SUBSCRIPTION,
};
use crate::{http, relay::IO};
use serde_json::{json, Value};
use std::{future, net::SocketAddr, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream, UnixListener},
    sync::{mpsc, oneshot},
    time::{timeout, Instant},
};
use tokio_util::sync::CancellationToken;

const CHANNEL: &str = "p/lead+owner";
const OLD: &str = "https://fcm.googleapis.com/wp/fixture-one";
const NEW: &str = "https://fcm.googleapis.com/wp/fixture-two";
const LATEST: &str = "/notify/latest";

async fn request(addr: SocketAddr, path: &str, endpoint: Option<&str>) -> (u16, Option<Value>) {
    let mut socket = TcpStream::connect(addr).await.unwrap();
    let (method, body) = endpoint.map_or(("GET", String::new()), |endpoint| {
        ("POST", json!({"on":true,"endpoint":endpoint}).to_string())
    });
    let message = format!(
        "{method} {path} HTTP/1.1\r\nHost: fixture.invalid\r\nTailscale-User-Login: owner\r\nOrigin: https://fixture.invalid\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",body.len()
    );
    socket.write_all(message.as_bytes()).await.unwrap();
    let mut out = String::new();
    socket.read_to_string(&mut out).await.unwrap();
    let (head, body) = out.split_once("\r\n\r\n").unwrap();
    let code = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    let value = (!body.is_empty()).then(|| serde_json::from_str(body).unwrap());
    (code, value)
}

async fn history(daemon: &UnixListener, id: u64, body: Option<&str>) {
    let mut io = connect(daemon).await;
    let req = next(&mut io).await;
    assert_eq!(req["method"], "channel.history");
    assert_eq!(
        req["params"],
        json!({"channel":CHANNEL,"limit":1,"before":format!("m{}",id+1)})
    );
    if let Some(body) = body {
        answer(
            &mut io,
            &req,
            json!({"channel":CHANNEL,"posts":[{"id":format!("m{id}"),"body":body}]}),
        )
        .await;
    } else {
        write(&mut io, json!({"type":"response","id":req["id"],"error":{"code":"unavailable","message":"private fixture detail"}})).await;
    }
}

fn posted(seq: u64) -> Value {
    json!({"type":"event","seq":seq,"kind":"channel.posted","body":{"channel":CHANNEL,"author":"p/lead","mentions":[],"wake":["owner"]}})
}

#[tokio::test]
async fn committed_event_offers_once_seen_clears_and_late_gone_cannot_remove_a_new_slot() {
    timeout(Duration::from_secs(60), async {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("org.sock");
        let daemon = UnixListener::bind(&socket).unwrap();
        let stop = CancellationToken::new();
        let n = Notifications::load(root.path().join("notify"), Vec::new(), socket.clone()).await.unwrap();
        let (facts, mut applied) = mpsc::unbounded_channel();
        let (send, mut sent) = mpsc::unbounded_channel();
        let worker = tokio::spawn({
            let n = n.clone();
            let stop = stop.clone();
            async move {
                let (n, facts, send) = (&n, &facts, &send);
                tokio::join!(
                    follow(&socket, &stop, |fact| async move {
                        let seq = fact.seq;
                        n.observe(fact).await;
                        facts.send(seq).unwrap();
                    }),
                    n.deliver(&stop, |ep| async move {
                        let (reply, code) = oneshot::channel();
                        send.send((ep.as_str().to_string(), reply)).unwrap();
                        Ok(code.await?)
                    }),
                );
            }
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = http::router_with_features(http::AppState {
            login: "owner".parse().unwrap(),
            origin: "https://fixture.invalid".parse().unwrap(),
            assets: Default::default(),
            socket: root.path().join("org.sock"),
            stop: stop.clone(),
        }, None, Some(n.clone()));
        let http = tokio::spawn(async move {axum::serve(listener, app).await.unwrap()});
        let mut stream = connect(&daemon).await;
        let req = next(&mut stream).await;
        assert_eq!(req["method"], "channel.list");
        answer(&mut stream, &req, json!({"seq":10,"channels":[]})).await;
        let watch = next(&mut stream).await;
        assert_eq!(watch["type"], "watch");
        assert_eq!(watch["cursor"], 10);
        assert_eq!(request(addr, SUBSCRIPTION, Some(OLD)).await.0, 204);
        write(&mut stream, posted(11)).await;
        assert_eq!(applied.recv().await, Some(11));
        // The first probe errors; the retry opens the gate. Read again through HTTP.
        history(&daemon,11,None).await;
        history(&daemon,11,Some("original")).await;
        let (endpoint, late) = sent.recv().await.unwrap();
        assert_eq!(endpoint, OLD);
        let (reply, ()) = tokio::join!(request(addr,LATEST,None), history(&daemon,11,Some("visible preview")));
        assert_eq!(reply,(200,Some(json!({"body":"visible preview","target":{"channel":CHANNEL}}))));

        write(&mut stream,posted(11)).await;
        write(&mut stream,json!({"type":"event","seq":12,"kind":"msg.sent","body":{"post":"m11","to":"owner","from":"p/lead"}})).await;
        assert_eq!(applied.recv().await,Some(12),"replayed sequence must not be passed to observe");
        assert!(n.slot.lock().await.waiting.is_none(),"linked wake must not queue another push");
        assert!(sent.try_recv().is_err()); // First transport call is still in flight.

        write(&mut stream,json!({"type":"event","seq":13,"kind":"channel.seen","body":{"channel":CHANNEL,"reader":"owner","through":"m11"}})).await;
        assert_eq!(applied.recv().await,Some(13));
        tokio::select! {
            reply = request(addr,LATEST,None) => assert_eq!(reply,(204,None)),
            _ = daemon.accept() => panic!("already seen preview was still offered"),
        }
        assert_eq!(request(addr,SUBSCRIPTION,Some(NEW)).await.0,204);
        late.send(410).unwrap();
        let ttl = Duration::from_secs(600);
        let first_expiry = Instant::now() + ttl;
        write(&mut stream,posted(14)).await;
        assert_eq!(applied.recv().await,Some(14));
        // Two errors are inconclusive, not closed. Send the empty ping anyway.
        history(&daemon,14,None).await;
        history(&daemon,14,None).await;
        let (endpoint, done) = sent.recv().await.unwrap();
        assert_eq!(endpoint,NEW,"stale 410 must not clear the replacement");
        let expires = n.slot.lock().await.offered.as_ref().unwrap().1;
        assert!((first_expiry..=Instant::now()+ttl).contains(&expires), "preview lifetime must match provider TTL");
        let (reply, ()) = tokio::join!(request(addr,LATEST,None), history(&daemon,14,Some("recovered preview")));
        assert_eq!(reply,(200,Some(json!({"body":"recovered preview","target":{"channel":CHANNEL}}))));
        done.send(201).unwrap();
        stop.cancel();
        worker.await.unwrap();
        http.abort();
        let _ = http.await;
    }).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn timeouts_get_one_attempt_more_and_a_definitively_closed_row_gets_none() {
    let stop = CancellationToken::new();
    let start = Instant::now();
    let mut attempts = Vec::new();
    assert_eq!(
        preview_for_push(&stop, || {
            attempts.push(Instant::now() - start);
            future::pending()
        })
        .await,
        Some(true)
    );
    assert_eq!(attempts, [Duration::ZERO, IO]);
    assert_eq!(Instant::now() - start, IO + IO);
    let mut closed = 0;
    assert_eq!(
        preview_for_push(&stop, || {
            closed += 1;
            future::ready(Ok(None))
        })
        .await,
        Some(false)
    );
    assert_eq!(closed, 1);
}
