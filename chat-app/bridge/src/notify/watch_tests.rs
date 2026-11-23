use super::watch::{follow, Fact, Notice};
use serde_json::{json, Value};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::mpsc,
    time::timeout,
};
use tokio_util::sync::CancellationToken;

fn fact(seq: u64, kind: &str, body: Value) -> Fact {
    serde_json::from_value(json!({"seq":seq,"kind":kind,"body":body})).unwrap()
}

#[test]
fn only_owner_dm_explicit_owner_mention_and_decision_request_are_eligible() {
    for (kind, body, yes) in [
        (
            "channel.posted",
            json!({"author":"p/lead","channel":"p/lead+owner","mentions":["owner"]}),
            true,
        ),
        (
            "channel.posted",
            json!({"author":"p/lead","channel":"p","mentions":["owner"]}),
            true,
        ),
        (
            "channel.posted",
            json!({"author":"p/lead","channel":"p","wake":["owner"],"body":"@owner"}),
            false,
        ),
        (
            "channel.posted",
            json!({"author":"owner","channel":"p/lead+owner","mentions":["owner"]}),
            false,
        ),
        (
            "channel.posted",
            json!({"author":"p/lead","channel":"p/lead+owner","decision":{"decision_id":"d40","step":"asked"}}),
            false,
        ),
        (
            "channel.post_edited",
            json!({"post":"m41","actor":"p/lead","body":"@owner"}),
            false,
        ),
        (
            "msg.sent",
            json!({"from":"p/lead","to":"owner","post":"m41"}),
            false,
        ),
        ("msg.sent", json!({"from":"p/lead","to":"owner"}), true),
        ("msg.sent", json!({"from":"p/lead","to":"p/other"}), false),
        (
            "msg.sent",
            json!({"from":"system","to":"owner","delivery_notice":{"reason":"focus"}}),
            false,
        ),
        ("decision.requested", json!({"requester":"p/lead"}), true),
        ("decision.requested", json!({"requester":"owner"}), false),
        (
            "decision.taken_provisionally",
            json!({"decision_id":"d40"}),
            false,
        ),
        ("decision.resolved", json!({"decision_id":"d40"}), false),
    ] {
        assert_eq!(
            Notice::select(&fact(42, kind, body.clone())).is_some(),
            yes,
            "{kind} {body}"
        );
    }
}

#[test]
fn only_the_matching_seen_or_terminal_fact_clears_a_notice() {
    let post = Notice::select(&fact(
        42,
        "channel.posted",
        json!({"author":"p/lead","channel":"p/lead+owner"}),
    ))
    .unwrap();
    for (channel, reader, through, clears) in [
        ("p/lead+owner", "owner", "m43", true),
        ("p/lead+owner", "owner", "m41", false),
        ("p/lead+owner", "p/lead", "m43", false),
        ("p/other+owner", "owner", "m43", false),
    ] {
        assert_eq!(
            post.cleared_by(&fact(
                44,
                "channel.seen",
                json!({"channel":channel,"reader":reader,"through":through})
            )),
            clears
        );
    }
    assert!(post.cleared_by(&fact(44, "channel.post_removed", json!({"post":"m42"}))));
    assert!(!post.cleared_by(&fact(44, "channel.post_removed", json!({"post":"m41"}))));
    let dm = Notice::select(&fact(42, "msg.sent", json!({"from":"p/lead","to":"owner"}))).unwrap();
    assert!(dm.cleared_by(&fact(44, "msg.read", json!({"message_ids":["m42"]}))));
    let decision = Notice::select(&fact(
        42,
        "decision.requested",
        json!({"requester":"p/lead"}),
    ))
    .unwrap();
    assert!(decision.cleared_by(&fact(44, "decision.expired", json!({"decision_id":"d42"}))));
    assert!(!decision.cleared_by(&fact(
        44,
        "decision.taken_provisionally",
        json!({"decision_id":"d42"})
    )));
}

pub(super) async fn next(io: &mut BufReader<UnixStream>) -> Value {
    let mut line = String::new();
    assert_ne!(io.read_line(&mut line).await.unwrap(), 0);
    serde_json::from_str(&line).unwrap()
}

pub(super) async fn write(io: &mut BufReader<UnixStream>, body: Value) {
    io.get_mut()
        .write_all(format!("{body}\n").as_bytes())
        .await
        .unwrap();
}

pub(super) async fn connect(server: &UnixListener) -> BufReader<UnixStream> {
    let mut io = BufReader::new(server.accept().await.unwrap().0);
    let hello = next(&mut io).await;
    assert_eq!(hello["type"], "connect");
    assert_eq!(hello["protocol_version"], json!({"major":1,"minor":1}));
    write(&mut io, json!({"type":"connected","protocol_version":{"major":1,"minor":1},"methods":["channel.list","channel.history","decision.get","msg.inbox"]})).await;
    io
}

pub(super) async fn answer(io: &mut BufReader<UnixStream>, req: &Value, result: Value) {
    assert_eq!(req["type"], "request");
    assert!(req.get("from_pane").is_none());
    write(
        io,
        json!({"type":"response","id":req["id"],"result":result}),
    )
    .await;
}

#[tokio::test]
async fn independent_watch_arms_at_snapshot_and_replay_keeps_each_sequence_once() {
    timeout(std::time::Duration::from_secs(60), async {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("org.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let stop = CancellationToken::new();
        let end = stop.clone();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let worker = tokio::spawn(async move {
            follow(&socket, &end, move |f| {
                tx.send(f.seq).unwrap();
                std::future::ready(())
            })
            .await;
        });
        let mut first = connect(&listener).await;
        let list = next(&mut first).await;
        assert_eq!(list["method"], "channel.list");
        answer(&mut first, &list, json!({"seq":10,"channels":[]})).await;
        let request = next(&mut first).await;
        assert_eq!(request["type"], "watch");
        assert_eq!(request["cursor"], 10);
        assert!(request["topics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|topic| topic
                .as_str()
                .is_some_and(|t| "channel.posted".starts_with(t))));
        write(
            &mut first,
            json!({"type":"event","kind":"channel.posted","seq":11,"body":{}}),
        )
        .await;
        assert_eq!(rx.recv().await, Some(11));
        drop(first);
        let mut after = connect(&listener).await;
        let request = next(&mut after).await;
        assert_eq!(request["type"], "watch");
        assert_eq!(request["cursor"], 11);
        for seq in [11, 13] {
            write(
                &mut after,
                json!({"type":"event","kind":"channel.posted","seq":seq,"body":{}}),
            )
            .await;
        }
        assert_eq!(rx.recv().await, Some(13));
        stop.cancel();
        worker.await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn preview_checks_exact_current_row_and_does_not_reveal_removed_or_closed_content() {
    timeout(std::time::Duration::from_secs(60), async {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("org.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        for (notice, method, params, reply, want) in [
            (fact(42,"channel.posted",json!({"author":"p/lead","channel":"p","mentions":["owner"],"thread":"m21"})), "channel.history", json!({"channel":"p","thread":"m21","limit":1,"after":"m41"}), json!({"channel":"p","posts":[{"id":"m21","body":"root"},{"id":"m42","body":"edited current text","thread":"m21","attachments":["private"]}]}), Some(json!({"body":"edited current text","target":{"channel":"p","thread":"m21"}}))),
            (fact(42,"channel.posted",json!({"author":"p/lead","channel":"p/lead+owner"})), "channel.history", json!({"channel":"p/lead+owner","limit":1,"before":"m43"}), json!({"channel":"p/lead+owner","posts":[{"id":"m42","body":"old text","removed":true}]}), None),
            (fact(42,"msg.sent",json!({"from":"p/lead","to":"owner"})), "msg.inbox",json!({"for":"owner","id":"m42","limit":1,"unread_only":true}),json!({"messages":[{"message_id":"m42","read_at":null,"content":{"body":"legacy DM"}}]}),Some(json!({"body":"legacy DM","target":{"channel":"p/lead+owner"}}))),
            (fact(42,"decision.requested",json!({"requester":"p/lead"})), "decision.get", json!({"id":"d42"}),json!({"decision":{"id":"d42","status":"resolved","request":{"title":"closed question"}}}),None),
            (fact(42,"decision.requested",json!({"requester":"p/lead"})), "decision.get", json!({"id":"d42"}),json!({"decision":{"id":"d42","status":"taken_provisionally","request":{"title":"still needs you"}}}),Some(json!({"body":"still needs you","target":{"decision":"d42"}}))),
        ] {
            let n = Notice::select(&notice).unwrap();
            let talk = async {
                let mut io = connect(&listener).await;
                let req = next(&mut io).await;
                assert_eq!(req["method"], method);
                assert_eq!(req["params"], params);
                answer(&mut io, &req, reply).await;
            };
            let (got, ()) = tokio::join!(n.preview(&socket), talk);
            assert_eq!(got.unwrap().map(|preview| serde_json::to_value(preview).unwrap()), want);
        }
    }).await.unwrap();
}
