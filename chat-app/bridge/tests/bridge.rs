#![allow(clippy::unwrap_used, clippy::expect_used)] // Integration-test helpers assert on I/O failures.

mod support;

use futures_util::{poll, SinkExt, StreamExt};
use serde_json::{json, Value};
use std::{
    ffi::CString,
    fs,
    io::Write,
    net::SocketAddr,
    os::unix::{
        ffi::OsStrExt,
        fs::{symlink, PermissionsExt},
    },
    path::{Path, PathBuf},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpStream, UnixListener, UnixStream},
    process::Child,
};
use tokio_tungstenite::tungstenite::{
    protocol::{frame::coding::CloseCode, CloseFrame},
    Error, Message,
};

use support::{guard, Bridge, Rig, Tap, Ws, ORIGIN, OWNER};

const CONNECT: &str = r#"{ "type": "connect", "protocol_version":{"major":1,"minor":1} }"#;
const LIST: &str = " \t{\"id\": \"list\", \"params\":{},\"method\":\"channel.list\",\"protocol_version\":{\"minor\":1,\"major\":1},\"type\":\"request\"}  ";
const FORBIDDEN: &str = r#"{"type":"request","id":"denied","method":"daemon.info","params":{},"protocol_version":{"major":1,"minor":1}}"#;
const IDENTITY_DENIED: &str = r#"{"error":"identity"}"#;
const ORIGIN_DENIED: &str = r#"{"error":"origin"}"#;
const HEALTH_UP: &str = r#"{"bridge":"ok","daemon":"up","protocol":"1.1"}"#;
const HEALTH_DOWN: &str = r#"{"bridge":"ok","daemon":"down","protocol":"1.1"}"#;

struct Live {
    rig: Rig,
    daemon: Child,
    bridge: Bridge,
    tap: Tap,
    ws: Ws,
}

impl Live {
    async fn new() -> Self {
        Self::with_rig(Rig::new()).await
    }

    async fn with_rig(rig: Rig) -> Self {
        Self::start(rig, &[]).await
    }

    async fn start(rig: Rig, extra: &[&Path]) -> Self {
        let daemon = rig.daemon().await;
        let tap = Tap::start(&rig);
        let bridge = rig.bridge_with(&rig.path("tap.sock"), extra).await;
        let (ws, _) = bridge.ws(&[OWNER], Some(ORIGIN)).await.unwrap();
        Self {
            rig,
            daemon,
            bridge,
            tap,
            ws,
        }
    }

    async fn connect(&mut self) {
        send(&mut self.ws, CONNECT).await;
        self.assert_forwarded(CONNECT).await;
        let got = self.relayed().await;
        assert_eq!(got["type"], "connected");
        assert_eq!(got["protocol_version"], json!({"major":1,"minor":1}));
        for method in [
            "channel.list",
            "channel.history",
            "channel.post",
            "channel.seen",
            "decision.list",
            "decision.resolve",
            "decision.withdraw",
            "org.tree",
        ] {
            assert!(got["methods"].as_array().unwrap().contains(&json!(method)));
        }
    }

    async fn assert_forwarded(&mut self, frame: &str) {
        assert_eq!(
            self.tap.upstream.recv().await.unwrap(),
            format!("{frame}\n").as_bytes()
        );
    }

    async fn assert_relayed(&mut self, ws: &str) {
        let daemon = self.tap.downstream.recv().await.unwrap();
        assert_eq!(daemon.strip_suffix(b"\n").unwrap(), ws.as_bytes());
    }

    async fn relayed(&mut self) -> Value {
        let ws = text(&mut self.ws).await;
        self.assert_relayed(&ws).await;
        serde_json::from_str(&ws).unwrap()
    }

    async fn rpc(&mut self, frame: &str) -> Value {
        send(&mut self.ws, frame).await;
        let ws = text(&mut self.ws).await;
        let request: Value = serde_json::from_str(frame).unwrap();
        let response: Value = serde_json::from_str(&ws).unwrap();
        assert_eq!(response["type"], "response");
        assert_eq!(response["id"], request["id"]);
        // Look for a refusal before awaiting the tap, which never sees it.
        assert!(response.get("result").is_some(), "{frame} -> {ws}");
        self.assert_forwarded(frame).await;
        self.assert_relayed(&ws).await;
        response["result"].clone()
    }

    async fn finish(mut self) {
        drop(self.ws);
        self.tap.finish().await;
        self.bridge.stop().await;
        self.daemon.kill().await.unwrap();
    }
}

async fn send(ws: &mut Ws, frame: &str) {
    ws.send(Message::text(frame)).await.unwrap();
}

async fn text(ws: &mut Ws) -> String {
    match ws.next().await.unwrap().unwrap() {
        Message::Text(text) => text.to_string(),
        other => panic!("expected text, got {other:?}"),
    }
}

async fn refused(ws: &mut Ws, id: Option<&str>) {
    let value: Value = serde_json::from_str(&text(ws).await).unwrap();
    if let Some(id) = id {
        assert_eq!(value["type"], "response");
        assert_eq!(value["id"], id);
        assert_eq!(value["error"]["code"], "refused");
    } else {
        assert_eq!(value["type"], "error");
        assert_eq!(value["code"], "refused");
    }
}

fn assert_json_http((head, body): (String, String), status: u16, expected: &str) {
    assert!(
        head.starts_with(&format!("HTTP/1.1 {status} ")),
        "{head}\n{body}"
    );
    assert_eq!(body, expected);
    assert!(head.contains("content-type: application/json"), "{head}");
    assert!(head.contains("cache-control: no-store"), "{head}");
}

#[tokio::test]
async fn health_tracks_private_atmd_lifecycle() {
    guard(async {
        let rig = Rig::new();
        let bridge = rig.bridge(&rig.path("daemon.sock")).await;
        assert_json_http(
            bridge.http("GET", "/health", &[OWNER]).await,
            200,
            HEALTH_DOWN,
        );
        let mut daemon = rig.daemon().await;
        assert_json_http(
            bridge.http("GET", "/health", &[OWNER]).await,
            200,
            HEALTH_UP,
        );
        daemon.kill().await.unwrap();
        // Reaping the daemon closes the listener; do not unlink the socket.
        assert_json_http(
            bridge.http("GET", "/health", &[OWNER]).await,
            200,
            HEALTH_DOWN,
        );
        bridge.stop().await;
    })
    .await;
}

#[tokio::test]
async fn health_requires_login_before_dial() {
    guard(async {
        let rig = Rig::new();
        let socket = rig.path("probe.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let bridge = rig.bridge(&socket).await;
        for logins in [&[][..], &["other@example.invalid"][..], &[OWNER, OWNER][..]] {
            assert_json_http(
                bridge.http("GET", "/health", logins).await,
                403,
                IDENTITY_DENIED,
            );
        }
        let (head, _) = bridge.http("POST", "/health", &[OWNER]).await;
        assert!(head.starts_with("HTTP/1.1 405"), "{head}");
        assert!(poll!(Box::pin(listener.accept())).is_pending());

        assert_json_http(
            bridge.http("GET", "/health", &[OWNER]).await,
            200,
            HEALTH_UP,
        );
        let (mut probe, _) = listener.accept().await.unwrap();
        assert_eq!(probe.read_to_end(&mut Vec::new()).await.unwrap(), 0);
        assert!(
            poll!(Box::pin(listener.accept())).is_pending(),
            "extra health dial"
        );
        bridge.stop().await;
    })
    .await;
}

#[tokio::test]
async fn identity_required_for_get_and_upgrade() {
    guard(async {
        let rig = Rig::new();
        let listener = UnixListener::bind(rig.path("tap.sock")).unwrap();
        let bridge = rig.bridge(&rig.path("tap.sock")).await;
        for (logins, origin) in [
            (&[][..], Some(ORIGIN)),
            (&["other@example.invalid"][..], Some(ORIGIN)),
            (&[OWNER, OWNER][..], Some(ORIGIN)),
        ] {
            assert_json_http(bridge.http("GET", "/", logins).await, 403, IDENTITY_DENIED);
            denied_upgrade(&bridge, logins, origin).await;
        }
        denied_upgrade(&bridge, &[OWNER], None).await;
        denied_upgrade(&bridge, &[OWNER], Some("https://other.example.invalid")).await;
        assert_json_http(
            bridge.http("GET", "/not-a-route", &[]).await,
            403,
            IDENTITY_DENIED,
        );
        let (head, body) = bridge.http("HEAD", "/not-a-route", &[]).await;
        assert!(head.starts_with("HTTP/1.1 403"), "{head}");
        assert!(body.is_empty());
        assert!(
            poll!(Box::pin(listener.accept())).is_pending(),
            "a denied upgrade dialed upstream"
        );
        let (head, body) = bridge.http("GET", "/", &[OWNER]).await;
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert!(head.contains("cache-control: no-store"));
        assert!(body.contains("ATM Phone Chat"));
        let (ws, response) = bridge.ws(&[OWNER], Some(ORIGIN)).await.unwrap();
        assert_eq!(response.status().as_u16(), 101);
        let tokio_tungstenite::MaybeTlsStream::Plain(tcp) = ws.get_ref() else {
            panic!("expected loopback plain TCP");
        };
        let addr = tcp.peer_addr().unwrap();
        let (upstream, _) = listener.accept().await.unwrap();
        drop(ws);
        drop(upstream);

        // Browsers cannot read an Upgrade refusal. A no-Upgrade POST still
        // runs both guards before Axum rejects the method.
        assert_json_http(post_ws(addr, &[], &[]).await, 403, IDENTITY_DENIED);
        for origins in [
            &[][..],
            &["https://other.example.invalid"][..],
            &[ORIGIN, ORIGIN][..],
        ] {
            assert_json_http(post_ws(addr, &[OWNER], origins).await, 403, ORIGIN_DENIED);
        }
        let (head, body) = post_ws(addr, &[OWNER], &[ORIGIN]).await;
        assert!(head.starts_with("HTTP/1.1 405"), "{head}");
        assert!(body.is_empty());
        assert!(
            poll!(Box::pin(listener.accept())).is_pending(),
            "POST dialed upstream"
        );
        bridge.stop().await;
    })
    .await;
}

async fn post_ws(addr: SocketAddr, logins: &[&str], origins: &[&str]) -> (String, String) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let mut request =
        format!("POST /ws HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nContent-Length: 0\r\n");
    for login in logins {
        request.push_str(&format!("Tailscale-User-Login: {login}\r\n"));
    }
    for origin in origins {
        request.push_str(&format!("Origin: {origin}\r\n"));
    }
    stream
        .write_all((request + "\r\n").as_bytes())
        .await
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    let (head, body) = response.split_once("\r\n\r\n").unwrap();
    (head.to_owned(), body.to_owned())
}

async fn denied_upgrade(bridge: &Bridge, logins: &[&str], origin: Option<&str>) {
    let Err(Error::Http(response)) = bridge.ws(logins, origin).await else {
        panic!("expected HTTP refusal");
    };
    assert_eq!(response.status().as_u16(), 403);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "application/json"
    );
}

fn seed_pending_decision(rig: &Rig) {
    let journal = rig.path("state/log/events");
    let project = rig.path("project");
    fs::create_dir_all(&project).unwrap();
    fs::create_dir_all(&journal).unwrap();
    let mut out = fs::File::create(journal.join("00000001.jsonl")).unwrap();
    // Replay open decisions from a now-down seat without starting a pane or
    // harness. U8 backfills their question posts on startup.
    for (seq, kind, body) in [
        (
            1,
            "project.registered",
            json!({"project_id":"fixture","root":project}),
        ),
        (
            2,
            "agent.adopted",
            json!({"agent_id":"fixture/lead","pane":"%1","session_id":null}),
        ),
        (
            3,
            "decision.requested",
            json!({"requester":"fixture/lead","kind":"policy","title":"Review","context":"","files":[],"options":["go","hold"],"recommended":"go","may_be_undone":true,"changes_intent":false,"deadline":null}),
        ),
        (
            4,
            "decision.requested",
            json!({"requester":"fixture/lead","kind":"policy","title":"Another review","context":"","files":[],"options":["go","hold"],"recommended":"go","may_be_undone":true,"changes_intent":false,"deadline":null}),
        ),
        (
            5,
            "agent.down",
            json!({"agent_id":"fixture/lead","reason":"gone"}),
        ),
    ] {
        writeln!(
            out,
            "{}",
            json!({"seq":seq,"kind":kind,"body":body,"ts":"2026-01-01T00:00:00Z"})
        )
        .unwrap();
    }
}

fn rpc(method: &str, params: Value) -> String {
    json!({"type":"request","id":method,"method":method,"params":params,"protocol_version":{"major":1,"minor":1}}).to_string()
}

#[tokio::test]
async fn channel_and_decision_round_trip_is_byte_identical() {
    guard(async {
        let rig = Rig::new();
        seed_pending_decision(&rig);
        let mut live = Live::with_rig(rig).await;
        live.connect().await;
        let list = live.rpc(LIST).await;
        assert!(list["channels"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["id"] == "fixture"));
        let posted = live
            .rpc(&rpc(
                "channel.post",
                json!({"channel":"fixture","body":"sealed note","client_key":"w0-fixture"}),
            ))
            .await;
        let id = posted["post_id"].as_str().unwrap();
        assert_eq!(posted["channel"], "fixture");
        let history = live
            .rpc(&rpc("channel.history", json!({"channel":"fixture"})))
            .await;
        assert_eq!(history["posts"][0]["id"], id);
        assert_eq!(history["posts"][0]["body"], "sealed note");
        let seen = live
            .rpc(&rpc(
                "channel.seen",
                json!({"channel":"fixture","through":id}),
            ))
            .await;
        assert_eq!(seen["changed"], true);
        assert_eq!(seen["through"], id);
        let questions = live
            .rpc(&rpc("decision.list", json!({"statuses":["pending"]})))
            .await;
        let question = &questions["decisions"][0];
        assert_eq!(question["id"], "d3");
        assert_eq!(question["status"], "pending");
        assert_eq!(question["channel"], "fixture");
        let question_id = question["post_id"].as_str().unwrap();
        let linked_history = live
            .rpc(&rpc("channel.history", json!({"channel":"fixture"})))
            .await;
        let linked_post = linked_history["posts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|post| post["id"] == question_id)
            .expect("the decision link points to a real question post");
        assert_eq!(linked_post["channel"], "fixture");
        assert_eq!(linked_post["kind"], "question");
        assert_eq!(linked_post["body"], "Review");
        let resolved = live
            .rpc(&rpc(
                "decision.resolve",
                json!({"id":"d3","option":"go","note":"sealed answer"}),
            ))
            .await;
        assert_eq!(resolved["changed"], true);
        assert_eq!(resolved["decision"]["id"], "d3");
        assert_eq!(resolved["decision"]["status"], "resolved");
        assert_eq!(resolved["decision"]["resolution"]["note"], "sealed answer");
        let withdrawn = live
            .rpc(&rpc(
                "decision.withdraw",
                json!({"id":"d4","reason":"Fixture is stale"}),
            ))
            .await;
        assert_eq!(withdrawn["changed"], true);
        assert_eq!(withdrawn["decision"]["id"], "d4");
        assert_eq!(withdrawn["decision"]["status"], "withdrawn");
        assert_eq!(withdrawn["decision"]["withdrawal"]["by"], "owner");
        assert_eq!(
            withdrawn["decision"]["withdrawal"]["reason"],
            "Fixture is stale"
        );
        live.finish().await;
    })
    .await;
}

const LOGIN_HEADER: (&str, &str) = ("Tailscale-User-Login", OWNER);
const UPLOAD_HEADERS: [(&str, &str); 2] = [LOGIN_HEADER, ("Origin", ORIGIN)];
const PHOTO_BYTES: &[u8] = b"\x00\xff\r\nJpeg bytes\xfe";

fn file_query(path: &Path) -> String {
    format!("/file?p={}", path.to_str().unwrap().replace('/', "%2F"))
}

async fn upload_photo(bridge: &Bridge) -> Value {
    let (head, body) = bridge
        .file_http(
            "POST",
            "/upload?name=album%2F2+shots.JPG",
            &UPLOAD_HEADERS,
            PHOTO_BYTES,
            false,
        )
        .await;
    assert!(head.starts_with("HTTP/1.1 201 "), "{head}");
    assert!(head.contains("content-type: application/json"), "{head}");
    serde_json::from_slice(&body).unwrap()
}

#[tokio::test]
async fn photo_only_post_round_trips_bytes_and_path_through_real_atmd() {
    guard(async {
        let rig = Rig::new();
        let store = rig.path("store");
        seed_pending_decision(&rig);
        let mut live = Live::start(rig, &[Path::new("--files-dir"), &store]).await;
        let uploaded = upload_photo(&live.bridge).await;
        let path = Path::new(uploaded["path"].as_str().unwrap());
        assert!(path.is_absolute() && path.starts_with(&store));
        assert_eq!(path.file_name().unwrap(), "2_shots.JPG");
        assert_eq!(uploaded["size"], PHOTO_BYTES.len());
        assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
        assert_ne!(upload_photo(&live.bridge).await["path"], uploaded["path"]);
        live.connect().await;
        let posted = live.rpc(&rpc("channel.post", json!({"channel":"fixture","body":"","attachments":[path],"client_key":"photo-fixture"}))).await;
        let history = live.rpc(&rpc("channel.history", json!({"channel":"fixture"}))).await;
        let row = history["posts"].as_array().unwrap().iter().find(|row| row["id"] == posted["post_id"]).unwrap();
        assert_eq!(row["body"], "");
        assert_eq!(row["attachments"], json!([path]));
        let file = file_query(path);
        for method in ["GET", "HEAD"] {
            let (head, bytes) = live.bridge.file_http(method, &file, &[LOGIN_HEADER, ("Sec-Fetch-Site", "same-origin")], &[], false).await;
            assert!(head.starts_with("HTTP/1.1 200 "), "{head}");
            for expected in ["content-type: image/jpeg", "content-security-policy: sandbox; default-src 'none'", "x-content-type-options: nosniff", "cross-origin-resource-policy: same-origin", "cache-control: private, max-age=31536000, immutable"] {
                assert!(head.contains(expected), "{expected} missing in {head}");
            }
            assert!(head.contains(&format!("content-length: {}", PHOTO_BYTES.len())), "{head}");
            assert_eq!(bytes, if method == "GET" { PHOTO_BYTES } else { &[] });
        }
        live.finish().await;
    }).await;
}

#[tokio::test]
async fn downloaded_file_has_stored_name_and_head_size_without_body() {
    guard(async {
        let rig = Rig::new();
        let store = rig.path("store");
        let bridge = rig
            .bridge_with(
                &rig.path("unused.sock"),
                &[Path::new("--files-dir"), &store],
            )
            .await;
        let pdf = b"%PDF-1.4\n%%EOF\n";
        let (head, body) = bridge
            .file_http(
                "POST",
                "/upload?name=report+7.pdf",
                &UPLOAD_HEADERS,
                pdf,
                false,
            )
            .await;
        assert!(head.starts_with("HTTP/1.1 201 "), "{head}");
        let receipt: Value = serde_json::from_slice(&body).unwrap();
        let path = Path::new(receipt["path"].as_str().unwrap());
        assert_eq!(path.file_name().unwrap(), "report_7.pdf");
        for method in ["HEAD", "GET"] {
            let (head, bytes) = bridge
                .file_http(method, &file_query(path), &[LOGIN_HEADER], &[], false)
                .await;
            assert!(head.starts_with("HTTP/1.1 200 "), "{head}");
            assert!(
                head.contains("content-type: application/octet-stream"),
                "{head}"
            );
            assert!(
                head.contains("content-disposition: attachment; filename=\"report_7.pdf\""),
                "{head}"
            );
            assert!(
                head.contains(&format!("content-length: {}\r\n", pdf.len())),
                "{head}"
            );
            assert_eq!(bytes, if method == "GET" { &pdf[..] } else { &[] });
        }
        bridge.stop().await;
    })
    .await;
}

#[tokio::test]
async fn download_header_has_one_exact_sanitized_filename() {
    guard(async {
        let rig = Rig::new();
        let store = rig.path("store");
        let bridge = rig
            .bridge_with(
                &rig.path("unused.sock"),
                &[Path::new("--files-dir"), &store],
            )
            .await;
        let encoded = |value: &str| {
            value
                .bytes()
                .map(|byte| format!("%{byte:02X}"))
                .collect::<String>()
        };
        let name = format!("folder\\q\"\r\n;é{}.pdf", "x".repeat(283));
        assert_eq!(name.chars().count(), 300);
        let (head, body) = bridge
            .file_http(
                "POST",
                &format!("/upload?name={}", encoded(&name)),
                &UPLOAD_HEADERS,
                b"file",
                false,
            )
            .await;
        assert!(head.starts_with("HTTP/1.1 201 "), "{head}");
        let receipt: Value = serde_json::from_slice(&body).unwrap();
        let uploaded = receipt["path"].as_str().unwrap();
        let shortened = format!("q_____{}", "x".repeat(94));
        assert_eq!(Path::new(uploaded).file_name().unwrap(), shortened.as_str());

        // Also exercise the header boundary with a file not normalized by POST.
        let original = store.join("folder\\q\"\r\n;é.pdf");
        fs::write(&original, b"file").unwrap();
        for (path, safe) in [
            (uploaded, shortened.as_str()),
            (original.to_str().unwrap(), "q_____.pdf"),
        ] {
            for method in ["HEAD", "GET"] {
                let (head, body) = bridge
                    .file_http(
                        method,
                        &format!("/file?p={}", encoded(path)),
                        &[LOGIN_HEADER],
                        &[],
                        false,
                    )
                    .await;
                assert!(head.starts_with("HTTP/1.1 200 "), "{head}");
                let disposition: Vec<_> = head
                    .lines()
                    .filter_map(|line| {
                        let (key, value) = line.split_once(':')?;
                        key.eq_ignore_ascii_case("content-disposition")
                            .then(|| value.trim_start())
                    })
                    .collect();
                assert_eq!(disposition, [format!("attachment; filename=\"{safe}\"")]);
                assert_eq!(body, if method == "GET" { &b"file"[..] } else { &[] });
            }
        }
        bridge.stop().await;
    })
    .await;
}

fn assert_file_error((head, body): (String, Vec<u8>), method: &str, status: u16, code: &str) {
    assert!(head.starts_with(&format!("HTTP/1.1 {status} ")), "{head}");
    if method == "HEAD" {
        assert!(body.is_empty());
    } else {
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap(),
            json!({"error":code})
        );
    }
}

#[tokio::test]
async fn file_routes_require_opt_in_identity_origin_and_site() {
    guard(async {
        let rig = Rig::new();
        let store = rig.path("store");
        let socket = rig.path("unused.sock");
        let disabled = rig.bridge(&socket).await;
        for (method, url, status) in [
            ("POST", "/upload?name=a.jpg", 405),
            ("GET", "/file?p=unused", 404),
            ("HEAD", "/file?p=unused", 404),
        ] {
            let (head, _) = disabled
                .file_http(method, url, &UPLOAD_HEADERS, &[], false)
                .await;
            assert!(head.starts_with(&format!("HTTP/1.1 {status} ")), "{head}");
        }
        disabled.stop().await;
        let bridge = rig
            .bridge_with(&socket, &[Path::new("--files-dir"), &store])
            .await;
        let real_file = store.join("fixture.jpg");
        fs::write(&real_file, PHOTO_BYTES).unwrap();
        let file = file_query(&real_file);
        for (method, url) in [
            ("POST", "/upload?name=a.jpg"),
            ("GET", &file),
            ("HEAD", &file),
        ] {
            for mut headers in [
                vec![],
                vec![("Tailscale-User-Login", "other@example.invalid")],
                vec![LOGIN_HEADER, LOGIN_HEADER],
            ] {
                headers.push(("Origin", ORIGIN));
                assert_file_error(
                    bridge.file_http(method, url, &headers, &[], false).await,
                    method,
                    403,
                    "identity",
                );
            }
        }
        for mut headers in [
            vec![],
            vec![("Origin", "https://other.example.invalid")],
            vec![("Origin", ORIGIN), ("Origin", ORIGIN)],
        ] {
            headers.push(LOGIN_HEADER);
            assert_file_error(
                bridge
                    .file_http("POST", "/upload?name=a.jpg", &headers, &[], false)
                    .await,
                "POST",
                403,
                "origin",
            );
        }
        for method in ["GET", "HEAD"] {
            for site in ["cross-site", "same-site"] {
                assert_file_error(
                    bridge
                        .file_http(
                            method,
                            &file,
                            &[LOGIN_HEADER, ("Sec-Fetch-Site", site)],
                            &[],
                            false,
                        )
                        .await,
                    method,
                    403,
                    "site",
                );
            }
            let (head, _) = bridge
                .file_http(method, &file, &[LOGIN_HEADER], &[], false)
                .await;
            assert!(head.starts_with("HTTP/1.1 200 "), "{head}");
        }
        assert_file_error(
            bridge
                .file_http("POST", "/upload", &UPLOAD_HEADERS, &[], false)
                .await,
            "POST",
            400,
            "name",
        );
        // Only the test fixture was written; the guard path did not start work.
        assert_eq!(fs::read_dir(&store).unwrap().count(), 1);
        bridge.stop().await;
    })
    .await;
}

#[tokio::test]
async fn file_route_only_reads_bounded_regular_files_inside_private_store() {
    guard(async {
        let rig = Rig::new();
        let socket = rig.path("unused.sock");
        let store = rig.path("store");
        let bridge = rig
            .bridge_with(&socket, &[Path::new("--files-dir"), &store])
            .await;
        // Every target is a synthetic fixture in this test's own temp directory.
        let outside = rig.path("outside.jpg");
        fs::write(&outside, PHOTO_BYTES).unwrap();
        symlink(&outside, store.join("link.jpg")).unwrap();
        fs::write(store.join(".hidden.jpg"), PHOTO_BYTES).unwrap();
        fs::create_dir(store.join("dir.jpg")).unwrap();
        let fifo = store.join("fifo.jpg");
        let fifo_name = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: the CString pointer stays valid and NUL-terminated for the call.
        let created = unsafe { libc::mkfifo(fifo_name.as_ptr(), 0o600) };
        assert_eq!(created, 0, "{}", std::io::Error::last_os_error());
        fs::File::create(store.join("large.jpg"))
            .unwrap()
            .set_len(32 * 1024 * 1024 + 1)
            .unwrap();
        for path in [
            Path::new("relative.jpg").to_owned(),
            outside,
            store.join("../outside.jpg"),
            store.join("link.jpg"),
            store.join(".hidden.jpg"),
            store.join("dir.jpg"),
            fifo,
            store.join("large.jpg"),
        ] {
            let url = file_query(&path);
            assert_file_error(
                bridge
                    .file_http("GET", &url, &[LOGIN_HEADER], &[], false)
                    .await,
                "GET",
                403,
                "path",
            );
        }
        let missing = file_query(&store.join("missing.jpg"));
        assert_file_error(
            bridge
                .file_http("GET", &missing, &[LOGIN_HEADER], &[], false)
                .await,
            "GET",
            404,
            "gone",
        );
        assert_file_error(
            bridge
                .file_http("GET", "/file", &[LOGIN_HEADER], &[], false)
                .await,
            "GET",
            403,
            "path",
        );
        let body = b"This is a fixture, not a picture.";
        let (head, bytes) = bridge
            .file_http(
                "POST",
                "/upload?name=sample.html",
                &UPLOAD_HEADERS,
                body,
                false,
            )
            .await;
        assert!(head.starts_with("HTTP/1.1 201 "), "{head}");
        let uploaded: Value = serde_json::from_slice(&bytes).unwrap();
        let url = file_query(Path::new(uploaded["path"].as_str().unwrap()));
        let (head, bytes) = bridge
            .file_http("GET", &url, &[LOGIN_HEADER], &[], false)
            .await;
        assert!(head.starts_with("HTTP/1.1 200 "), "{head}");
        assert!(
            head.contains("content-type: application/octet-stream"),
            "{head}"
        );
        assert!(head.contains("content-disposition: attachment"), "{head}");
        assert_eq!(bytes, body);
        bridge.stop().await;
    })
    .await;
}

fn serve_directory(rig: &Rig, name: &str) -> PathBuf {
    let root = rig.path(name);
    fs::create_dir_all(&root).unwrap();
    root.canonicalize().unwrap()
}

#[tokio::test]
async fn serve_roots_get_head_stay_under_the_selected_root() {
    guard(async {
        let rig = Rig::new();
        let docs = serve_directory(&rig, "demo/docs/plans");
        let other = serve_directory(&rig, "second/docs/plans");
        let store = rig.path("store");
        let outside = rig.path("demo/docs/outside.md");
        fs::write(&outside, b"Outside fixture").unwrap();
        let second_report = other.join("note.md");
        fs::write(&second_report, b"Second fixture").unwrap();
        let hidden = docs.join(".private/note.md");
        fs::create_dir_all(hidden.parent().unwrap()).unwrap();
        fs::write(&hidden, b"Hidden fixture").unwrap();
        symlink(&outside, docs.join("outside-link.md")).unwrap();
        symlink(&second_report, docs.join("other-root-link.md")).unwrap();
        symlink(&hidden, docs.join("hidden-link.md")).unwrap();
        fs::create_dir(docs.join("directory.md")).unwrap();
        let fifo = docs.join("pipe.md");
        let name = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: the CString pointer stays valid and NUL-terminated for the call.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let large = docs.join("large.log");
        fs::File::create(&large)
            .unwrap()
            .set_len(32 * 1024 * 1024 + 1)
            .unwrap();
        let bridge = rig
            .bridge_with(
                &rig.path("unused.sock"),
                &[
                    Path::new("--files-dir"),
                    &store,
                    Path::new("--serve-root"),
                    &docs,
                    Path::new("--serve-root"),
                    &other,
                ],
            )
            .await;
        for path in [
            outside,
            docs.join("../outside.md"),
            hidden,
            docs.join("outside-link.md"),
            docs.join("other-root-link.md"),
            docs.join("hidden-link.md"),
            docs.join("directory.md"),
            fifo,
            large,
        ] {
            for method in ["GET", "HEAD"] {
                assert_file_error(
                    bridge
                        .file_http(method, &file_query(&path), &[LOGIN_HEADER], &[], false)
                        .await,
                    method,
                    403,
                    "path",
                );
            }
        }
        for method in ["GET", "HEAD"] {
            assert_file_error(
                bridge
                    .file_http(
                        method,
                        &file_query(&docs.join("gone.md")),
                        &[LOGIN_HEADER],
                        &[],
                        false,
                    )
                    .await,
                method,
                404,
                "gone",
            );
            let url = file_query(&second_report);
            for headers in [
                &[][..],
                &[("Tailscale-User-Login", "other@example.invalid")],
                &[LOGIN_HEADER, LOGIN_HEADER],
            ] {
                assert_file_error(
                    bridge.file_http(method, &url, headers, &[], false).await,
                    method,
                    403,
                    "identity",
                );
            }
            for site in ["cross-site", "same-site"] {
                assert_file_error(
                    bridge
                        .file_http(
                            method,
                            &url,
                            &[LOGIN_HEADER, ("Sec-Fetch-Site", site)],
                            &[],
                            false,
                        )
                        .await,
                    method,
                    403,
                    "site",
                );
            }
            let (head, body) = bridge
                .file_http(method, &url, &[LOGIN_HEADER], &[], false)
                .await;
            assert!(head.starts_with("HTTP/1.1 200 "), "{head}");
            assert_eq!(
                body,
                if method == "GET" {
                    &b"Second fixture"[..]
                } else {
                    &[]
                }
            );
        }
        bridge.stop().await;
    })
    .await;
}

#[tokio::test]
async fn serve_roots_type_the_resolved_file_and_do_not_cache_it() {
    guard(async {
        let rig = Rig::new();
        let docs = serve_directory(&rig, "docs/plans");
        let store = rig.path("store");
        let bridge = rig
            .bridge_with(
                &rig.path("unused.sock"),
                &[
                    Path::new("--files-dir"),
                    &store,
                    Path::new("--serve-root"),
                    &docs,
                ],
            )
            .await;
        let bytes = b"# Report\n<b>literal text</b>\n";
        let report = docs.join("note.MD");
        let alias = docs.join("alias.png");
        let page = docs.join("page+.html");
        let uploaded = store.join("uploaded.md");
        for path in [&report, &page, &uploaded] {
            fs::write(path, bytes).unwrap();
        }
        symlink(&report, &alias).unwrap();
        for (path, mime, cache, disposition) in [
            (&alias, "text/plain; charset=utf-8", "no-store", None),
            (
                &page,
                "application/octet-stream",
                "no-store",
                Some("page_.html"),
            ),
            (
                &uploaded,
                "application/octet-stream",
                "private, max-age=31536000, immutable",
                Some("uploaded.md"),
            ),
        ] {
            let url = file_query(path).replace('+', "%2B");
            for method in ["GET", "HEAD"] {
                let (head, body) = bridge
                    .file_http(method, &url, &[LOGIN_HEADER], &[], false)
                    .await;
                assert!(head.starts_with("HTTP/1.1 200 "), "{head}");
                for line in [
                    "x-content-type-options: nosniff".to_owned(),
                    "content-security-policy: sandbox; default-src 'none'".to_owned(),
                    "cross-origin-resource-policy: same-origin".to_owned(),
                    format!("content-type: {mime}"),
                    format!("cache-control: {cache}"),
                    format!("content-length: {}", bytes.len()),
                ] {
                    assert!(
                        head.lines().any(|found| found == line),
                        "expected {line} in {head}"
                    );
                }
                let filename = disposition.map(|name| format!("attachment; filename=\"{name}\""));
                assert_eq!(
                    head.lines()
                        .find_map(|line| line.strip_prefix("content-disposition: ")),
                    filename.as_deref()
                );
                assert_eq!(body, if method == "GET" { &bytes[..] } else { &[] });
            }
        }
        assert_file_error(
            bridge
                .file_http(
                    "GET",
                    &format!("{}&thumb=1", file_query(&alias)),
                    &[LOGIN_HEADER],
                    &[],
                    false,
                )
                .await,
            "GET",
            404,
            "gone",
        );
        bridge.stop().await;
    })
    .await;
}

#[tokio::test]
async fn serve_roots_bound_only_the_agent_image_preview() {
    guard(async {
        let rig = Rig::new();
        let docs = serve_directory(&rig, "docs/plans");
        let store = rig.path("store");
        let bridge = rig
            .bridge_with(
                &rig.path("unused.sock"),
                &[
                    Path::new("--files-dir"),
                    &store,
                    Path::new("--serve-root"),
                    &docs,
                ],
            )
            .await;
        const MIB: usize = 1024 * 1024;
        let small = docs.join("small.png");
        let large = docs.join("large.png");
        let uploaded = store.join("uploaded.png");
        for (path, length) in [(&small, MIB), (&large, MIB + 1), (&uploaded, MIB + 1)] {
            fs::File::create(path)
                .unwrap()
                .set_len(length as u64)
                .unwrap();
        }
        for method in ["GET", "HEAD"] {
            let variant = |path: &Path| format!("{}&thumb=1", file_query(path));
            assert_file_error(
                bridge
                    .file_http(method, &variant(&large), &[LOGIN_HEADER], &[], false)
                    .await,
                method,
                404,
                "gone",
            );
            for (url, length) in [
                (variant(&small), MIB),
                (file_query(&large), MIB + 1),
                (variant(&uploaded), MIB + 1),
            ] {
                let (head, body) = bridge
                    .file_http(method, &url, &[LOGIN_HEADER], &[], false)
                    .await;
                assert!(head.starts_with("HTTP/1.1 200 "), "{head}");
                assert!(
                    head.lines().any(|line| line == "content-type: image/png"),
                    "{head}"
                );
                assert!(
                    head.lines()
                        .any(|line| line == format!("content-length: {length}")),
                    "{head}"
                );
                assert_eq!(body.len(), if method == "GET" { length } else { 0 });
            }
        }
        bridge.stop().await;
    })
    .await;
}

#[tokio::test]
async fn chunked_upload_enforces_exact_cap_and_cleans_failed_file() {
    guard(async {
        let rig = Rig::new();
        let store = rig.path("store");
        let bridge = rig
            .bridge_with(
                &rig.path("unused.sock"),
                &[Path::new("--files-dir"), &store],
            )
            .await;
        let max = 32 * 1024 * 1024;
        let bytes = vec![b'f'; max + 1];
        assert_file_error(
            bridge
                .file_http(
                    "POST",
                    "/upload?name=limit.jpg",
                    &UPLOAD_HEADERS,
                    &bytes,
                    true,
                )
                .await,
            "POST",
            413,
            "size",
        );
        assert_eq!(fs::read_dir(&store).unwrap().count(), 0);
        let (head, body) = bridge
            .file_http(
                "POST",
                "/upload?name=limit.jpg",
                &UPLOAD_HEADERS,
                &bytes[..max],
                true,
            )
            .await;
        assert!(head.starts_with("HTTP/1.1 201 "), "{head}");
        assert_eq!(serde_json::from_slice::<Value>(&body).unwrap()["size"], max);
        bridge.stop().await;
    })
    .await;
}

#[tokio::test]
async fn store_at_quota_refuses_without_evicting_existing_uploads() {
    guard(async {
        let rig = Rig::new();
        let store = rig.path("store");
        // Sparse synthetic prior uploads: metadata exercises restart accounting
        // without writing 2 GiB or adding a production test flag.
        for n in 0..64 {
            let dir = store.join(format!("upload-{n}"));
            fs::create_dir_all(&dir).unwrap();
            fs::File::create(dir.join("retained.jpg"))
                .unwrap()
                .set_len(32 * 1024 * 1024)
                .unwrap();
        }
        let bridge = rig
            .bridge_with(
                &rig.path("unused.sock"),
                &[Path::new("--files-dir"), &store],
            )
            .await;
        assert_file_error(
            bridge
                .file_http(
                    "POST",
                    "/upload?name=next.jpg",
                    &UPLOAD_HEADERS,
                    PHOTO_BYTES,
                    false,
                )
                .await,
            "POST",
            507,
            "full",
        );
        assert_eq!(fs::read_dir(&store).unwrap().count(), 64);
        let (head, body) = bridge
            .file_http(
                "HEAD",
                &file_query(&store.join("upload-0/retained.jpg")),
                &[LOGIN_HEADER],
                &[],
                false,
            )
            .await;
        assert!(head.starts_with("HTTP/1.1 200 "), "{head}");
        assert!(head.contains("content-length: 33554432"), "{head}");
        assert!(body.is_empty());
        bridge.stop().await;
    })
    .await;
}

#[tokio::test]
async fn refused_frames_do_not_reach_the_daemon() {
    guard(async {
        let mut live = Live::new().await;
        live.connect().await;
        send(&mut live.ws, FORBIDDEN).await;
        refused(&mut live.ws, Some("denied")).await;
        send(
            &mut live.ws,
            &rpc("decision.withdraw.extra", json!({"id":"d3"})),
        )
        .await;
        refused(&mut live.ws, Some("decision.withdraw.extra")).await;
        live.ws
            .send(Message::binary(LIST.as_bytes()))
            .await
            .unwrap();
        refused(&mut live.ws, None).await;
        // `rpc` checks that the next upstream line is this allowed request.
        assert!(live.rpc(LIST).await["channels"].is_array());
        live.finish().await;
    })
    .await;
}

#[tokio::test]
async fn request_before_connect_is_refused_and_never_forwarded() {
    guard(async {
        let mut live = Live::new().await;
        send(&mut live.ws, LIST).await;
        refused(&mut live.ws, Some("list")).await;
        // The tap must see Connect as the FIRST upstream line. Fencing with
        // that positive write avoids any timing-based assertion of absence.
        live.connect().await;
        assert!(live.rpc(LIST).await["channels"].is_array());
        live.finish().await;
    })
    .await;
}

#[tokio::test]
async fn watch_relays_committed_chat_event() {
    guard(async {
        let mut live = Live::new().await;
        live.connect().await;
        let watch = r#"{"type":"watch","topics":["channel.","decision.","msg."],"cursor":0,"protocol_version":{"major":1,"minor":1}}"#;
        let ping = r#"{"type":"ping","seq":99,"protocol_version":{"major":1,"minor":1}}"#;
        send(&mut live.ws, watch).await;
        send(&mut live.ws, ping).await;
        let pong = text(&mut live.ws).await;
        assert_eq!(serde_json::from_str::<Value>(&pong).unwrap(), json!({"type":"pong","seq":99}));
        live.assert_forwarded(watch).await;
        live.assert_forwarded(ping).await;
        live.assert_relayed(&pong).await;
        let mut independent = BufReader::new(UnixStream::connect(live.rig.path("daemon.sock")).await.unwrap());
        assert_eq!(direct(&mut independent, CONNECT).await["type"], "connected");
        let commit = direct(&mut independent, r#"{"type":"request","id":"sent","method":"msg.send","params":{"to":"owner","body":"sealed chat note"},"protocol_version":{"major":1,"minor":1}}"#).await;
        assert_eq!(commit["id"], "sent");
        let id = commit["result"]["message_id"].as_str().unwrap();
        let got = live.relayed().await;
        assert_eq!(got["type"], "event");
        assert_eq!(got["kind"], "msg.sent");
        assert_eq!(format!("m{}", got["seq"].as_u64().unwrap()), id);
        assert_eq!(got["body"]["body"], "sealed chat note");
        drop(independent);
        live.finish().await;
    }).await;
}

async fn direct(stream: &mut BufReader<UnixStream>, request: &str) -> Value {
    stream
        .write_all(format!("{request}\n").as_bytes())
        .await
        .unwrap();
    let mut line = String::new();
    stream.read_line(&mut line).await.unwrap();
    serde_json::from_str(&line).unwrap()
}

#[tokio::test]
async fn failed_dial_closes_with_daemon_unavailable() {
    guard(async {
        let rig = Rig::new();
        let bridge = rig.bridge(&rig.path("missing.sock")).await;
        let (mut ws, _) = bridge.ws(&[OWNER], Some(ORIGIN)).await.unwrap();
        assert_backend_close(&mut ws, "daemon unavailable").await;
        drop(ws);
        bridge.stop().await;
    })
    .await;
}

#[tokio::test]
async fn lost_backend_closes_with_daemon_disconnected() {
    guard(async {
        let mut live = Live::new().await;
        live.connect().await;
        live.daemon.kill().await.unwrap();
        assert_backend_close(&mut live.ws, "daemon disconnected").await;
        drop(live.ws);
        live.tap.finish().await;
        live.bridge.stop().await;
    })
    .await;
}

#[tokio::test]
async fn invalid_unix_record_closes_with_daemon_disconnected() {
    guard(async {
        let rig = Rig::new();
        let socket = rig.path("backend.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let bridge = rig.bridge(&socket).await;
        let (mut ws, _) = bridge.ws(&[OWNER], Some(ORIGIN)).await.unwrap();
        let (mut upstream, _) = listener.accept().await.unwrap();
        upstream.write_all(b"\xff\n").await.unwrap();
        assert_backend_close(&mut ws, "daemon disconnected").await;
        assert_eq!(upstream.read_to_end(&mut Vec::new()).await.unwrap(), 0);
        drop(ws);
        bridge.stop().await;
    })
    .await;
}

async fn assert_backend_close(ws: &mut Ws, reason: &str) {
    match ws.next().await.unwrap().unwrap() {
        Message::Close(Some(frame)) => {
            assert_eq!(frame.code, CloseCode::Error);
            assert_eq!(frame.reason.as_str(), reason);
        }
        other => panic!("expected clean 1011 close, got {other:?}"),
    }
}

#[tokio::test]
async fn client_drop_closes_unix_connection() {
    guard(async {
        let mut live = Live::new().await;
        live.connect().await;
        drop(live.ws);
        assert!(
            live.tap.upstream.recv().await.is_none(),
            "Unix peer did not see EOF"
        );
        live.tap.finish().await;
        live.bridge.stop().await;
        live.daemon.kill().await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn client_close_acknowledges_and_releases_upstream() {
    guard(async {
        let mut live = Live::new().await;
        live.connect().await;
        let frame = CloseFrame {
            code: CloseCode::Normal,
            reason: "leaving".into(),
        };
        live.ws
            .send(Message::Close(Some(frame.clone())))
            .await
            .unwrap();
        assert_eq!(
            live.ws.next().await.unwrap().unwrap(),
            Message::Close(Some(frame))
        );
        assert!(live.tap.upstream.recv().await.is_none());
        live.finish().await;
    })
    .await;
}
