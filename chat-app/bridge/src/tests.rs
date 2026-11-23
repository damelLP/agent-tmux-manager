use std::{fs, future::pending, io, os::unix::fs::symlink, sync::mpsc, time::Duration};

use futures_util::{poll, SinkExt, StreamExt};
use tempfile::tempdir;
use tokio::{
    io::{duplex, AsyncReadExt, AsyncWriteExt, BufReader, DuplexStream},
    net::UnixListener,
    time::{advance, timeout, Instant},
};
use tokio_tungstenite::{
    client_async,
    tungstenite::{client::IntoClientRequest, protocol::frame::coding::CloseCode, Message},
};
use tokio_util::sync::CancellationToken;

use crate::{assets, http, origin_port, policy, relay};

const CONNECT: &str = r#"{"type":"connect","protocol_version":{"major":1,"minor":1}}"#;

fn request(method: &str) -> String {
    format!(
        r#"{{"method":"{method}","type":"request","id":"r1","params":{{"extra":"left to atmd"}},"protocol_version":{{"major":1,"minor":1}}}}"#
    )
}

#[test]
fn admits_exact_channel_and_decision_requests_after_connect() {
    assert!(policy::classify(CONNECT, true).is_ok());
    assert!(policy::classify(CONNECT, false).is_err());
    for method in [
        "channel.list",
        "channel.history",
        "channel.post",
        "channel.seen",
        "decision.list",
        "decision.resolve",
        "decision.withdraw",
        "org.tree",
        "agent.list",
    ] {
        assert!(
            policy::classify(&request(method), false).is_ok(),
            "{method}"
        );
        assert!(
            policy::classify(&request(method), true).is_err(),
            "{method}"
        );
    }
    let ping = r#"{"type":"ping","seq":1,"protocol_version":{"major":1,"minor":1}}"#;
    assert!(policy::classify(ping, true).is_err());
    assert!(policy::classify(ping, false).is_ok());
    let null_pane = request("channel.post").replacen('{', r#"{"from_pane":null,"#, 1);
    assert!(policy::classify(&null_pane, false).is_ok());
}

#[test]
fn allows_only_exact_owner_edit_and_remove_requests() {
    for method in ["channel.edit", "channel.remove"] {
        let frame = request(method);
        assert!(policy::classify(&frame, false).is_ok(), "{method}");
        assert!(policy::classify(&frame, true).is_err(), "{method}");
        let attributed = frame.replacen('{', r#"{"from_pane":"%1","#, 1);
        assert!(policy::classify(&attributed, false).is_err(), "{method}");
    }
    for method in [
        "channel.edit.extra",
        "channel.post_removed",
        "channel.delete",
        "msg.edit",
    ] {
        assert!(
            policy::classify(&request(method), false).is_err(),
            "{method}"
        );
    }
}

#[test]
fn admits_react_without_widening_the_post_policy() {
    let frame = request("channel.react");
    assert!(policy::classify(&frame, false).is_ok());
    assert!(policy::classify(&frame, true).is_err());
    let attributed = frame.replacen('{', r#"{"from_pane":"%1","#, 1);
    assert!(policy::classify(&attributed, false).is_err());
    for method in ["channel.reacted", "channel.react.extra", "msg.react"] {
        assert!(
            policy::classify(&request(method), false).is_err(),
            "{method}"
        );
    }
}

#[test]
fn admits_only_the_canonical_watch_topics() {
    let watch = |routing: &str| {
        format!(
            r#"{{"type":"watch",{routing}"cursor":0,"protocol_version":{{"major":1,"minor":1}}}}"#
        )
    };
    let canonical = watch(r#""topics":["channel.","decision.","msg."],"#);
    assert!(policy::classify(&canonical, false).is_ok());
    assert!(policy::classify(&canonical, true).is_err());
    for routing in [
        "",
        r#""topics":[],"#,
        r#""topics":["msg."],"#,
        r#""topics":["channel.","decision."],"#,
        r#""topics":["channel.","decision.","msg.","session."],"#,
        r#""topics":["channel.","decision.","decision."],"#,
        r#""topics":["msg.","decision.","channel."],"#,
        r#""topics":["channel","decision.","msg."],"#,
        r#""topics":["channel.","decision.",""],"#,
        r#""topics":["channel.","decision.","msg."],"topics":[],"#,
    ] {
        assert!(
            policy::classify(&watch(routing), false).is_err(),
            "{routing}"
        );
    }
}

#[test]
fn refuses_legacy_browser_msg_requests() {
    for method in ["msg.history", "msg.inbox", "msg.send", "msg.ack"] {
        assert!(
            policy::classify(&request(method), false).is_err(),
            "{method}"
        );
    }
}

#[test]
fn refuses_bad_framing_scope_and_attribution() {
    for method in [
        "channel.list.extra",
        "channel.route",
        "router.attach",
        "decision.request",
        "decision.resolve.extra",
        "decision.withdraw.extra",
        "agent.list.extra",
        "agent.up",
        "agent.adopt",
        "agent.down",
        "agent.respawn",
        "agent.clear",
        "msg.retry",
        "msg.give_up",
        "daemon.info",
    ] {
        assert!(
            policy::classify(&request(method), false).is_err(),
            "{method}"
        );
    }
    for method in ["decision.withdraw", "agent.list"] {
        let attributed = request(method).replacen('{', r#"{"from_pane":"%1","#, 1);
        assert!(policy::classify(&attributed, false).is_err());
    }
    let request = request("channel.list");
    for frame in [
        format!("{request}\n{request}"),
        format!("{request}\n"),
        format!("{request}{request}"),
        request.replacen('{', r#"{"from_pane":"%1","#, 1),
        request.replacen('{', r#"{"type":"request","#, 1),
        request.replacen('{', r#"{"m\u0065thod":"channel.list","#, 1),
        request.replacen('{', r#"{"id":"r1","#, 1),
        request.replacen('{', r#"{"protocol_version":null,"#, 1),
        request.replacen('{', r#"{"from_pane":null,"from_pane":null,"#, 1),
        format!("[{request}]"),
        format!("{} ", " ".repeat(1_048_575)),
        "{invalid".to_owned(),
    ] {
        assert!(policy::classify(&frame, false).is_err());
    }
    let with_escape = request.replace("left to atmd", r"line one\nline two");
    assert!(policy::classify(&with_escape, false).is_ok());
}

#[tokio::test(start_paused = true)]
async fn partial_line_survives_other_leg_activity() {
    let (mut input, reader) = duplex(64);
    let mut reader = BufReader::new(reader);
    let mut buf = Vec::new();
    input.write_all(b" {\"b\":").await.unwrap();
    {
        let read = relay::read_line(&mut reader, &mut buf);
        tokio::pin!(read);
        assert!(poll!(&mut read).is_pending());
    }
    input.write_all(b" 2 }\r\n{}\npartial").await.unwrap();
    assert_eq!(
        relay::read_line(&mut reader, &mut buf).await.unwrap(),
        " {\"b\": 2 }\r"
    );
    assert_eq!(relay::read_line(&mut reader, &mut buf).await.unwrap(), "{}");
    drop(input);
    assert!(relay::read_line(&mut reader, &mut buf).await.is_err());
}

#[tokio::test]
async fn daemon_line_cap_includes_lf_and_rejects_invalid_utf8() {
    let mut buf = Vec::new();
    let mut line = vec![b' '; 1_048_575];
    line.push(b'\n');
    assert_eq!(
        relay::read_line(&mut line.as_slice(), &mut buf)
            .await
            .unwrap()
            .len(),
        1_048_575
    );
    line.insert(0, b' ');
    assert!(relay::read_line(&mut line.as_slice(), &mut buf)
        .await
        .is_err());
    buf.clear();
    assert!(relay::read_line(&mut b"\xff\n".as_slice(), &mut buf)
        .await
        .is_err());
}

#[tokio::test]
async fn only_bounded_regular_page_assets_are_served() {
    let dir = tempdir().unwrap();
    assert!(assets::load(None)
        .await
        .unwrap()
        .contains_key("/index.html"));
    assert!(assets::load(Some(dir.path().join("missing")))
        .await
        .is_err());
    let web = dir.path().join("web");
    fs::create_dir_all(web.join("assets")).unwrap();
    fs::write(web.join("index.html"), "TEST HTML").unwrap();
    fs::write(web.join("assets/main.js"), "TEST JS").unwrap();
    fs::write(web.join(".hidden"), "NOT PUBLIC").unwrap();
    fs::write(dir.path().join("outside"), "NOT PUBLIC").unwrap();
    symlink(dir.path().join("outside"), web.join("assets/link.js")).unwrap();
    symlink(dir.path().join("outside"), web.join("link.js")).unwrap();
    let assets = assets::load(Some(web.clone())).await.unwrap();
    assert_eq!(assets.get("/index.html").unwrap().1.as_ref(), b"TEST HTML");
    assert_eq!(
        assets.get("/assets/main.js").unwrap().0,
        "text/javascript; charset=utf-8"
    );
    assert_eq!(
        assets.get("/assets/main.js").unwrap().1.as_ref(),
        b"TEST JS"
    );
    for path in [
        "/link.js",
        "/assets/link.js",
        "/outside",
        "/../outside",
        "/.hidden",
        "/assets/",
    ] {
        assert!(!assets.contains_key(path), "{path}");
    }
    let oversized = fs::File::create(web.join("large.js")).unwrap();
    oversized.set_len(16 * 1024 * 1024).unwrap();
    assert!(crate::assets::load(Some(web)).await.is_err());
}

#[test]
fn origin_is_canonical_https_authority() {
    assert_eq!(origin_port("https://chat.example.invalid").unwrap(), 443);
    assert_eq!(
        origin_port("https://chat.example.invalid:8443").unwrap(),
        8443
    );
    for origin in [
        "http://chat.example.invalid",
        "https://chat.example.invalid/",
        "https://chat.example.invalid:443",
        "https://foo@chat.example.invalid",
        "https://chat.example.invalid?x",
        "https://CHAT.example.invalid",
        "https://chat.example.invalid:0",
    ] {
        assert!(origin_port(origin).is_err(), "{origin}");
    }
}

#[tokio::test(start_paused = true)]
async fn tcp_accept_io_bounds_stalled_reads_and_writes() {
    let (raw, _peer) = duplex(1);
    let mut io = http::timed_io(raw);
    let start = Instant::now();
    let mut byte = [0];
    {
        let read = io.read_exact(&mut byte);
        tokio::pin!(read);
        assert!(poll!(&mut read).is_pending());
        advance(Duration::from_secs(300)).await;
        let std::task::Poll::Ready(Err(error)) = poll!(&mut read) else {
            panic!("stalled read not timed out");
        };
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    }
    io.write_all(b"a").await.unwrap();
    let write = io.write_all(b"b");
    tokio::pin!(write);
    assert!(poll!(&mut write).is_pending());
    advance(Duration::from_secs(10)).await;
    let std::task::Poll::Ready(Err(error)) = poll!(&mut write) else {
        panic!("stalled write not timed out");
    };
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    assert_eq!(Instant::now() - start, Duration::from_secs(310));
}

#[tokio::test(start_paused = true)]
async fn read_deadline_wins_over_simultaneous_transport_timeout() {
    let (mut peer, raw) = duplex(1);
    let mut io = http::timed_io(raw);
    let mut byte = [0];
    peer.write_all(b"a").await.unwrap();
    assert!(matches!(
        relay::read_before_idle(Instant::now() + relay::IDLE, io.read_exact(&mut byte)).await,
        Ok(Ok(1))
    ));
    assert_eq!(byte, *b"a");

    let read = relay::read_before_idle(Instant::now() + relay::IDLE, io.read_exact(&mut byte));
    tokio::pin!(read);
    assert!(poll!(&mut read).is_pending());
    advance(relay::IDLE).await;
    let std::task::Poll::Ready(Err(end @ relay::End::Idle)) = poll!(&mut read) else {
        panic!("transport read won at the idle deadline");
    };
    let frame = end.close_frame().unwrap();
    assert_eq!(frame.code, 1001);
    assert_eq!(frame.reason.as_str(), "idle timeout");
}

#[tokio::test(start_paused = true)]
async fn client_connect_deadline_sends_idle_close() {
    // Run the actual HTTP upgrade and timed WebSocket over an in-memory wire.
    struct OneClient(Option<DuplexStream>);
    impl axum::serve::Listener for OneClient {
        type Io = std::pin::Pin<Box<tokio_io_timeout::TimeoutStream<DuplexStream>>>;
        type Addr = ();

        async fn accept(&mut self) -> (Self::Io, ()) {
            match self.0.take() {
                Some(io) => (http::timed_io(io), ()),
                None => pending().await,
            }
        }

        fn local_addr(&self) -> io::Result<()> {
            Ok(())
        }
    }

    timeout(Duration::from_secs(60), async {
        // Tokio inhibits auto-advance during blocking tasks. Hold the clock
        // through the real Unix dial/accept; recv_timeout is only a hang guard.
        let (release, hold) = mpsc::channel::<()>();
        let held = tokio::task::spawn_blocking(move || hold.recv_timeout(Duration::from_secs(60)));
        let dir = tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let daemon = UnixListener::bind(&socket).unwrap();
        let (client, server) = duplex(4096);
        let stop = CancellationToken::new();
        let shutdown = stop.clone().cancelled_owned();
        let login = "owner@example.invalid";
        let origin = "https://chat.example.invalid";
        let app = http::router(http::AppState {
            login: login.parse().unwrap(),
            origin: origin.parse().unwrap(),
            assets: Default::default(),
            socket,
            stop: stop.clone(),
        });
        let serving = tokio::spawn(async move {
            axum::serve(OneClient(Some(server)), app)
                .with_graceful_shutdown(shutdown)
                .await
        });
        let mut request = "ws://chat.example.invalid/ws"
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("tailscale-user-login", login.parse().unwrap());
        request
            .headers_mut()
            .insert("origin", origin.parse().unwrap());
        let (mut ws, _) = client_async(request, client).await.unwrap();
        let (_peer, _) = daemon.accept().await.unwrap();

        // A refusal fences relay startup without sending a Connect or
        // releasing the silent Unix peer. All remaining traffic is in memory.
        ws.send(Message::text(
            r#"{"type":"ping","seq":1,"protocol_version":{"major":1,"minor":1}}"#,
        ))
        .await
        .unwrap();
        let refused = ws.next().await.unwrap().unwrap();
        let refusal: serde_json::Value = serde_json::from_str(refused.to_text().unwrap()).unwrap();
        assert_eq!(refusal["code"], "refused");
        assert!(poll!(ws.next()).is_pending());
        advance(Duration::from_secs(10)).await;
        drop(release);
        assert_eq!(
            held.await.unwrap(),
            Err(mpsc::RecvTimeoutError::Disconnected)
        );

        let next = ws.next().await;
        let Some(Ok(Message::Close(Some(frame)))) = next else {
            panic!("expected an idle Close frame, got {next:?}");
        };
        assert_eq!(frame.code, CloseCode::Away);
        assert_eq!(frame.reason.as_str(), "idle timeout");
        drop(ws);
        stop.cancel();
        serving.await.unwrap().unwrap();
    })
    .await
    .expect("idle-close test hung");
}
