//! `channel.post` on the message service (unit U5): record order, the
//! send-time check, the result, repeats, refusals and the owner heal (N6).
//! A real organisation actor with in-memory appends; every wait is on a
//! committed or folded sequence, never on time.

use super::channel_post::{pass, repair, spawn_wake_repair, wake_rows};
use super::message_service_tests::{
    discovery_row, guarded, other, own_process, own_process_start, owner, pane, utc, worker, Rig,
};
use super::messages::MessageHandle;
use super::{OrgHandle, OrgServiceError};
use crate::eventlog::Envelope;
use crate::eventlog::EventLogError;
use crate::server::methods::{CallContext, CallerId, MethodError, MethodRouter};
use atm_comms::channel_types::{ChannelPostParams, ChannelPostResult};
use atm_org::{
    Address, ChannelCommand, ChannelId, DeliveryStatus, MessageId, MessageKind, OrgCommand,
    PostRequest, PostTarget,
};
use serde_json::{json, Value};
use std::{
    path::Path,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};
use tokio::sync::{broadcast, mpsc, watch};
use tokio_util::sync::CancellationToken;

/// The message clock, read from the rig's fixed time.
fn clock() -> super::messages::MessageClock {
    super::messages::MessageClock::new(tokio::time::Instant::now(), utc())
}

fn lead() -> atm_org::AgentId {
    "demo/lead".parse().unwrap()
}

/// The seeded rig plus `demo/lead`, so the project channel `demo` is listed.
async fn rig() -> Rig {
    let rig = Rig::seeded().await;
    rig.run(OrgCommand::Adopt {
        agent_id: lead(),
        pane: "%3".into(),
        session_id: Some("s3".into()),
    })
    .await;
    rig
}

async fn post(rig: &Rig, caller: CallContext, params: Value) -> Result<Value, MethodError> {
    let p: ChannelPostParams = serde_json::from_value(params).unwrap();
    guarded(rig.messages.post(p, caller))
        .await
        .map(|result: ChannelPostResult| serde_json::to_value(result).unwrap())
}

fn drain(records: &mut broadcast::Receiver<Arc<Envelope>>) -> Vec<Arc<Envelope>> {
    std::iter::from_fn(|| records.try_recv().ok()).collect()
}

fn kinds(records: &[Arc<Envelope>]) -> Vec<&str> {
    records.iter().map(|r| r.kind.as_str()).collect()
}

/// A post as the policy writes it, with no wake rows: what a request that
/// died after its first append leaves.
fn raw_post(body: &str, tags: &[&str], key: Option<&str>) -> OrgCommand {
    OrgCommand::Channel(ChannelCommand::Post(Box::new(PostRequest {
        target: PostTarget::Channel(ChannelId::Project("demo".parse().unwrap())),
        author: Address::Owner,
        kind: MessageKind::Note,
        body: body.into(),
        attachments: vec![],
        task: None,
        tags: tags.iter().map(|t| (*t).to_owned()).collect(),
        groups: Default::default(),
        decision: None,
        client_key: key.map(str::to_owned),
        also_in_chat: false,
        no_route: false,
        internal: false,
    })))
}

/// Relay every commit to the held fold until the call answers: each relay
/// lets the handler take its next step, which commits again or answers.
async fn relay_until<T>(rig: &mut Rig, mut call: tokio::task::JoinHandle<T>) -> T {
    let mut health = rig.org.health.clone();
    loop {
        rig.relay();
        tokio::select! {
            result = &mut call => break result.unwrap(),
            changed = health.changed() => changed.unwrap(),
        }
    }
}

/// Each row that delivers `post`: its recipient and id, in recipient order.
fn rows_for(state: &atm_org::OrgState, post: u64) -> Vec<(String, MessageId)> {
    let mut rows: Vec<_> = state
        .messages()
        .filter(|m| m.content.post == Some(MessageId::new(post)))
        .map(|m| (m.content.to.to_string(), m.id))
        .collect();
    rows.sort();
    rows
}

fn m(seq: u64) -> String {
    format!("m{seq}")
}

fn seq_of(result: &Value) -> u64 {
    result["post_id"].as_str().unwrap()[1..].parse().unwrap()
}

// Break: wakes before the post, a row that is not a copy, rows in address
// order instead of the post's wake order, or a result that names other rows.
#[tokio::test]
async fn post_writes_the_post_then_one_exact_wake_per_target_in_order() {
    let rig = rig().await;
    let mut committed = rig.org.committed.subscribe();
    let result = post(
        &rig,
        owner(),
        json!({"channel":"demo","body":"hold W2","kind":"brief","attachments":["w2.md"],
            "task":"t1","tags":["worker","other"]}),
    )
    .await
    .unwrap();
    let records = drain(&mut committed);
    let p = seq_of(&result);
    assert_eq!(kinds(&records), ["channel.posted", "msg.sent", "msg.sent"]);
    assert_eq!(records[0].seq, p);
    // `worker` was tagged first; address order would put `other` first.
    for (record, to) in records[1..].iter().zip(["demo/worker", "demo/other"]) {
        assert_eq!(
            record.body,
            json!({"from":"owner","to":to,"kind":"brief","body":"hold W2",
                "attachments":["w2.md"],"task":"t1","post":m(p)})
        );
    }
    assert_eq!(
        result,
        json!({"seq":p + 2,"post_id":m(p),"channel":"demo","thread":null,
            "mentions":[{"seat":"demo/worker"},{"seat":"demo/other"}],
            "woke":[{"to":"demo/worker","message_id":m(p + 1)},{"to":"demo/other","message_id":m(p + 2)}],
            "unavailable":[],"route":"none","unresolved":[]})
    );
    // Each wake is an ordinary queued message for its seat.
    let state = rig.org.snapshot().await.unwrap();
    for id in [p + 1, p + 2] {
        let row = state.message(&MessageId::new(id)).unwrap();
        assert_eq!(row.status, DeliveryStatus::Queued);
        assert_eq!(row.content.post, Some(MessageId::new(p)));
    }
}

// Break: sharing by creating a second post/wake, or rewriting visibility on Retry.
#[tokio::test]
async fn shared_reply_is_one_committed_post_and_wake() {
    let rig = rig().await;
    let root = post(
        &rig,
        owner(),
        json!({"channel":"demo","body":"root","tags":["worker"]}),
    )
    .await
    .unwrap();
    let mut committed = rig.org.committed.subscribe();
    let mut params = json!({"reply_to":root["post_id"], "body":"one reply",
        "tags":["worker"], "also_in_chat":true, "client_key":"share-1"});
    let reply = post(&rig, owner(), params.clone()).await.unwrap();
    assert_eq!(reply["thread"], root["post_id"]);
    let records = drain(&mut committed);
    assert_eq!(kinds(&records), ["channel.posted", "msg.sent"]);
    assert_eq!(records[0].body["also_in_chat"], true);
    assert_eq!(records[1].body["post"], reply["post_id"]);
    assert_eq!(reply["woke"].as_array().unwrap().len(), 1);
    params["also_in_chat"] = json!(false);
    assert_eq!(post(&rig, owner(), params).await.unwrap(), reply);
    assert!(drain(&mut committed).is_empty());
}

fn change_router(rig: &Rig) -> MethodRouter {
    let mut router = MethodRouter::default();
    super::channel_methods::register(&mut router, rig.org.clone(), utc).unwrap();
    super::channel_methods::register_post(&mut router, rig.messages.clone()).unwrap();
    router
}

#[tokio::test]
async fn edit_mutation_reads_through_queued_wake_and_remove_leaves_a_tombstone() {
    use super::messages::{AckParams, InboxParams};
    let rig = rig().await; // The fold is live, delivery is held off by the rig.
    let first = post(
        &rig,
        owner(),
        json!({"channel":"demo","body":"old @worker",
        "attachments":["brief.md"],"task":"t1"}),
    )
    .await
    .unwrap();
    let post_id = first["post_id"].as_str().unwrap();
    let wake = first["woke"][0]["message_id"].as_str().unwrap();
    let reader = pane("%1");
    let inbox = |id: &str| serde_json::from_value::<InboxParams>(json!({"id":id})).unwrap();
    let initial = rig.inbox(reader.clone(), inbox(wake)).await;
    assert_eq!(initial.unread_count, 1);
    assert_eq!(initial.messages[0].status, DeliveryStatus::Queued);
    assert_eq!(initial.messages[0].content.body, "old @worker");
    let router = change_router(&rig);
    let read_history = || router.call(owner(), "channel.history", json!({"channel":"demo"}));
    let list = || router.call(owner(), "channel.list", json!({}));
    let prior_channels = list().await.unwrap()["channels"].clone();
    let orig_row = &read_history().await.unwrap()["posts"][0];
    let mut records = rig.org.committed.subscribe();
    let seq = rig.org.status().unwrap();
    let edited = router
        .call(
            owner(),
            "channel.edit",
            json!({"post":post_id,"body":"fixed @other"}),
        )
        .await
        .unwrap();
    assert_eq!(edited, json!({"seq":seq+1,"post_id":post_id}));
    for id in [post_id, wake] {
        let page = rig.inbox(reader.clone(), inbox(id)).await;
        let entry = &page.messages[0];
        assert_eq!(entry.message_id.to_string(), wake);
        assert_eq!(entry.content.body, "fixed @other");
        assert_eq!(entry.content.attachments, ["brief.md"]);
        assert_eq!(entry.status, DeliveryStatus::Queued);
        assert_eq!(page.unread_count, 1);
    }
    let row = &read_history().await.unwrap()["posts"][0];
    assert_eq!(row["id"], post_id);
    assert_eq!(row["posted_at"], orig_row["posted_at"]);
    assert_eq!(row["woke"], orig_row["woke"]);
    assert_eq!(row["mentions"], json!([]));
    assert_eq!(row["body"], "fixed @other");
    assert_eq!(row["edited"], true);
    assert!(row.get("removed").is_none());
    assert_eq!(list().await.unwrap()["channels"], prior_channels);
    let facts = drain(&mut records);
    assert_eq!(kinds(&facts), ["channel.post_edited"]);
    assert_eq!(
        facts[0].body,
        json!({"post":post_id,"actor":"owner","body":"fixed @other"})
    );

    let ids = vec![wake.parse().unwrap()];
    assert_eq!(
        rig.messages
            .ack(AckParams { ids }, reader.clone())
            .await
            .unwrap()
            .count,
        1
    );
    // Ack of an open wake cancels the original delivery, then marks it read.
    let ack = drain(&mut records);
    assert_eq!(kinds(&ack), ["msg.delivery_failed", "msg.read"]);
    assert_eq!(ack[0].body["message_id"], wake);
    assert_eq!(ack[0].body["final"], true);
    router
        .call(
            owner(),
            "channel.edit",
            json!({"post":post_id,"body":"read earlier"}),
        )
        .await
        .unwrap();
    let previous = rig.inbox(reader.clone(), inbox(wake)).await;
    assert!(previous.messages[0].read_at.is_some());
    assert_eq!(previous.messages[0].content.body, "read earlier");
    router
        .call(owner(), "channel.remove", json!({"post":post_id}))
        .await
        .unwrap();
    let facts = drain(&mut records);
    assert_eq!(
        kinds(&facts),
        ["channel.post_edited", "channel.post_removed"]
    );
    assert_eq!(facts[1].body, json!({"post":post_id,"actor":"owner"}));
    let row = &read_history().await.unwrap()["posts"][0];
    assert_eq!(row["id"], post_id);
    assert_eq!(row["author"], "owner");
    assert_eq!(row["posted_at"], orig_row["posted_at"]);
    assert_eq!(row["body"], "removed");
    assert_eq!(row["removed"], true);
    assert_eq!(row["attachments"], json!([]));
    assert!(row["task"].is_null());
    let page = rig.inbox(reader, inbox(post_id)).await;
    assert_eq!(page.unread_count, 0);
    let entry = &page.messages[0];
    assert_eq!(entry.content.body, "removed");
    assert!(entry.content.attachments.is_empty());
    assert!(entry.content.task.is_none());
    assert_eq!(entry.read_at, previous.messages[0].read_at);
    assert_eq!(entry.status, previous.messages[0].status);
    assert_eq!(
        rig.org
            .snapshot()
            .await
            .unwrap()
            .message(&wake.parse().unwrap())
            .unwrap()
            .content
            .body,
        "old @worker"
    );
}

#[tokio::test(start_paused = true)]
async fn mutation_response_waits_for_the_message_fold() {
    let mut rig = Rig::relayed().await;
    let authored = match raw_post("original", &[], None) {
        OrgCommand::Channel(ChannelCommand::Post(mut p)) => {
            p.author = Address::Agent(lead());
            OrgCommand::Channel(ChannelCommand::Post(p))
        }
        unexpected => panic!("not a post: {unexpected:?}"),
    };
    for command in [
        OrgCommand::RegisterProject {
            project_id: "demo".parse().unwrap(),
            root: "/demo".into(),
            move_root: false,
        },
        OrgCommand::Adopt {
            agent_id: lead(),
            pane: "%3".into(),
            session_id: None,
        },
        authored,
    ] {
        guarded(rig.org.execute(command, utc())).await.unwrap();
    }
    // Hold the new pane binding out of the message projection as well. The
    // author has to be identified on the writer, not from that stale view.
    let id = rig.org.status().unwrap();
    let router = change_router(&rig);
    let (mut call, pending) = rig.write_awaiting_fold(id + 1, async move {
        router
            .call(
                pane("%3"),
                "channel.edit",
                json!({"post":m(id),"body":"edited"}),
            )
            .await
    });
    guarded(async {
        tokio::select! {
            result = &mut call => panic!("edit answered before its inbox projection: {result:?}"),
            entered = pending => entered.unwrap(),
        }
    })
    .await;
    assert_eq!(rig.org.status().unwrap(), id + 1);
    assert!(!call.is_finished());
    rig.relay();
    assert_eq!(
        guarded(call).await.unwrap().unwrap(),
        json!({"seq":id+1,"post_id":m(id)})
    );
}

#[tokio::test]
async fn mutation_refusals_are_typed_and_never_append() {
    let rig = rig().await;
    let first = post(
        &rig,
        pane("%1"),
        json!({"channel":"demo","body":"by worker"}),
    )
    .await
    .unwrap();
    let id = first["post_id"].clone();
    let old = rig
        .send(owner(), Address::Agent(worker()), "legacy inbox body")
        .await;
    let router = change_router(&rig);
    let seq = rig.org.status().unwrap();
    for (caller, method, params, code) in [
        (
            owner(),
            "channel.edit",
            json!({"post":id,"body":"by owner"}),
            "refused",
        ),
        (owner(), "channel.remove", json!({"post":id}), "refused"),
        (pane("%2"), "channel.remove", json!({"post":id}), "refused"),
        (
            pane("broken"),
            "channel.remove",
            json!({"post":id}),
            "invalid_params",
        ),
        (
            pane("%1"),
            "channel.edit",
            json!({"post":id,"body":" \n\t"}),
            "invalid_params",
        ),
        (
            pane("%1"),
            "channel.edit",
            json!({"post":id,"body":"é".repeat(8193)}),
            "invalid_params",
        ),
        (
            pane("%1"),
            "channel.remove",
            json!({"post":id,"actor":"demo/worker"}),
            "invalid_params",
        ),
        (owner(), "channel.remove", json!({"post":old}), "refused"),
        (
            owner(),
            "channel.remove",
            json!({"post":"m99999"}),
            "not_found",
        ),
    ] {
        assert_eq!(
            router
                .call(caller, method, params.clone())
                .await
                .unwrap_err()
                .0
                .code,
            code,
            "{method}: {params}"
        );
        assert_eq!(rig.org.status().unwrap(), seq);
    }
    let valid = "🫠".repeat(4096);
    let edited = router
        .call(pane("%1"), "channel.edit", json!({"post":id,"body":valid}))
        .await
        .unwrap();
    assert_eq!(edited["seq"], seq + 1);
    router
        .call(pane("%1"), "channel.remove", json!({"post":id}))
        .await
        .unwrap();
    assert_eq!(
        router
            .call(
                pane("%1"),
                "channel.edit",
                json!({"post":id,"body":"resurrect"})
            )
            .await
            .unwrap_err()
            .0
            .code,
        "refused"
    );
    assert_eq!(rig.org.status().unwrap(), seq + 2);
}

// Break: the full-content overlay (text/attachments/task or post link/context)
// is applied to a managed inbox or history row after it has been redacted.
#[tokio::test]
async fn linked_wake_mutation_does_not_enrich_managed_rows() {
    use crate::server::methods::{router, MethodServices};
    let rig = rig().await;
    let dir = tempfile::tempdir().unwrap();
    let roles = dir.path().join(".atm/roles");
    std::fs::create_dir_all(&roles).unwrap();
    std::fs::write(
        roles.join("worker.md"),
        "+++\nharness='pi'\nmanager='lead'\n+++\nFixture\n",
    )
    .unwrap();
    rig.run(OrgCommand::RegisterProject {
        project_id: "demo".parse().unwrap(),
        root: dir.path().to_str().unwrap().into(),
        move_root: true,
    })
    .await;
    let first = post(
        &rig,
        owner(),
        json!({"to":"demo/worker","body":"original detail",
        "attachments":["fixture.md"],"task":"t1"}),
    )
    .await
    .unwrap();
    let post = first["post_id"].as_str().unwrap();
    let wake = first["woke"][0]["message_id"].as_str().unwrap();
    let mut services = MethodServices::new(rig.rt.registry.clone());
    services.org = rig.org.clone();
    services.messages = Some(rig.messages.clone());
    let router = router(services).unwrap();
    let read_inbox =
        |caller| router.call(caller, "msg.inbox", json!({"for":"demo/worker","id":wake}));
    let read_history = |caller| router.call(caller, "msg.history", json!({"agent":"demo/worker"}));
    let manager = pane("%3");
    let worker = pane("%1");
    let metadata = json!({"from":"owner","to":"demo/worker","kind":"note",
        "body":"","attachments":[],"task":null});
    for (method, params, expected, attachments, task) in [
        (
            "channel.edit",
            json!({"post":post,"body":"edited private detail"}),
            "edited private detail",
            json!(["fixture.md"]),
            json!("t1"),
        ),
        (
            "channel.remove",
            json!({"post":post}),
            "removed",
            json!([]),
            Value::Null,
        ),
    ] {
        router.call(owner(), method, params).await.unwrap();
        let inbox = read_inbox(manager.clone()).await.unwrap();
        let history = read_history(manager.clone()).await.unwrap();
        assert_eq!(history["visibility"], "delivery");
        for page in [inbox, history] {
            let rows = page["messages"].as_array().unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0]["message_id"], wake);
            assert_eq!(rows[0]["content"], metadata);
            assert!(rows[0].get("post_context").is_none());
        }
        for page in [
            read_inbox(worker.clone()).await.unwrap(),
            read_history(worker.clone()).await.unwrap(),
        ] {
            let content = &page["messages"][0]["content"];
            assert_eq!(content["body"], expected);
            assert_eq!(content["post"], post);
            assert_eq!(content["attachments"], attachments);
            assert_eq!(content["task"], task);
        }
    }
    for (key, value) in [("text", "original detail"), ("task", "t1")] {
        let mut params = json!({"agent":"demo/worker"});
        params[key] = json!(value);
        assert_eq!(
            router
                .call(manager.clone(), "msg.history", params)
                .await
                .unwrap_err()
                .0
                .code,
            "refused"
        );
    }
}

#[tokio::test]
async fn reaction_counts_are_per_actor_and_changes_never_wake() {
    let rig = rig().await;
    let first = post(
        &rig,
        owner(),
        json!({"channel":"demo","body":"note","tags":["worker","other"]}),
    )
    .await
    .unwrap();
    let id = first["post_id"].as_str().unwrap();
    let wake = first["woke"][0]["message_id"].as_str().unwrap();
    let router = change_router(&rig);
    let history = |caller| router.call(caller, "channel.history", json!({"channel":"demo"}));
    let react = |caller, post: &str, emoji: &str| {
        router.call(caller, "channel.react", json!({"post":post,"emoji":emoji}))
    };
    let mut records = rig.org.committed.subscribe();
    let original_list = router
        .call(owner(), "channel.list", json!({}))
        .await
        .unwrap()["channels"]
        .clone();
    let seq = rig.org.status().unwrap();
    assert_eq!(
        react(pane("%1"), wake, "👍").await.unwrap(),
        json!({"seq":seq+1,"post_id":id})
    );
    assert_eq!(react(owner(), id, "👍").await.unwrap()["seq"], seq + 2);
    for emoji in ["❌", "👀", "✅"] {
        react(pane("%2"), id, emoji).await.unwrap();
    }
    assert_eq!(
        history(owner()).await.unwrap()["posts"][0]["reactions"],
        json!([
            {"emoji":"👍","count":2,"mine":true}, {"emoji":"✅","count":1,"mine":false},
            {"emoji":"👀","count":1,"mine":false}, {"emoji":"❌","count":1,"mine":false},
        ])
    );
    react(pane("%1"), id, "👍").await.unwrap();
    let worker_view = history(pane("%1")).await.unwrap();
    assert_eq!(
        worker_view["posts"][0]["reactions"][0],
        json!({"emoji":"👍","count":1,"mine":false})
    );
    react(owner(), id, "👍").await.unwrap();
    let other_view = history(pane("%2")).await.unwrap();
    assert_eq!(
        other_view["posts"][0]["reactions"],
        json!([
            {"emoji":"✅","count":1,"mine":true}, {"emoji":"👀","count":1,"mine":true},
            {"emoji":"❌","count":1,"mine":true},
        ])
    );
    let facts = drain(&mut records);
    assert_eq!(kinds(&facts), ["channel.reacted"; 7]);
    assert_eq!(
        facts[0].body,
        json!({"post":id,"actor":"demo/worker","emoji":"👍","active":true})
    );
    assert_eq!(
        facts[5].body,
        json!({"post":id,"actor":"demo/worker","emoji":"👍","active":false})
    );
    assert_eq!(
        router
            .call(owner(), "channel.list", json!({}))
            .await
            .unwrap()["channels"],
        original_list
    );
    let reader = pane("%1");
    let params = serde_json::from_value(json!({"id":wake})).unwrap();
    let inbox = rig.inbox(reader, params).await;
    assert_eq!(inbox.unread_count, 1);
    assert_eq!(inbox.messages[0].status, DeliveryStatus::Queued);
    router
        .call(owner(), "channel.remove", json!({"post":id}))
        .await
        .unwrap();
    let page = history(pane("%2")).await.unwrap();
    assert!(page["posts"][0].get("reactions").is_none());
    assert_eq!(
        react(owner(), id, "👍").await.unwrap_err().0.code,
        "refused"
    );
    assert_eq!(kinds(&drain(&mut records)), ["channel.post_removed"]);
}

#[tokio::test]
async fn reaction_refuses_other_inbox_ids_nonmembers_and_bad_emoji() {
    let rig = rig().await;
    let first = post(
        &rig,
        owner(),
        json!({"to":"demo/worker","body":"private pair"}),
    )
    .await
    .unwrap();
    let id = first["post_id"].clone();
    let wake = first["woke"][0]["message_id"].clone();
    let legacy = rig
        .send(owner(), Address::Agent(worker()), "legacy body")
        .await;
    let router = change_router(&rig);
    let seq = rig.org.status().unwrap();
    for (caller, params, code) in [
        (pane("%2"), json!({"post":id,"emoji":"👍"}), "refused"),
        (owner(), json!({"post":wake,"emoji":"👍"}), "refused"),
        (pane("%1"), json!({"post":legacy,"emoji":"👍"}), "refused"),
        (
            pane("%1"),
            json!({"post":"m99999","emoji":"👍"}),
            "not_found",
        ),
        (
            pane("%1"),
            json!({"post":id,"emoji":"👍🏻"}),
            "invalid_params",
        ),
        (pane("%1"), json!({"post":id,"emoji":42}), "invalid_params"),
        (
            pane("%1"),
            json!({"post":id,"emoji":"✅","active":false}),
            "invalid_params",
        ),
    ] {
        assert_eq!(
            router
                .call(caller, "channel.react", params.clone())
                .await
                .unwrap_err()
                .0
                .code,
            code,
            "{params}"
        );
        assert_eq!(rig.org.status().unwrap(), seq);
    }
}

#[tokio::test]
async fn present_sharing_flag_rejects_both_top_level_targets_before_append() {
    let rig = rig().await;
    let seq = rig.org.status().unwrap();
    for target in ["channel", "to"] {
        for flag in [false, true] {
            let mut params = json!({"body":"not a reply", "also_in_chat":flag});
            params[target] = if target == "to" {
                json!("demo/worker")
            } else {
                json!("demo")
            };
            assert_eq!(
                post(&rig, owner(), params).await.unwrap_err().0.code,
                "invalid_params"
            );
            assert_eq!(rig.org.status().unwrap(), seq);
        }
    }
}

// Break: an untagged post woken by rule, or routed nowhere instead of pending.
#[tokio::test]
async fn untagged_post_is_pending_and_wakes_nobody() {
    let rig = rig().await;
    let mut committed = rig.org.committed.subscribe();
    let result = post(&rig, owner(), json!({"channel":"demo","body":"anyone?"}))
        .await
        .unwrap();
    assert_eq!(kinds(&drain(&mut committed)), ["channel.posted"]);
    assert_eq!(result["route"], "pending");
    assert_eq!(result["woke"], json!([]));
}

// Break: the check skipped for posts, run after the post is written, or run
// on seats the post does not wake by rule.
#[tokio::test]
async fn post_to_an_unconfirmed_session_is_refused_before_anything_is_written() {
    let rig = rig().await;
    let _cwd = discovery_row(&rig).await; // %1: demo/worker's pane
    let before = rig.org.status().unwrap();
    for params in [
        json!({"channel":"demo","body":"see @owner","tags":["worker"]}),
        json!({"channel":"demo","body":"see @worker"}),
        json!({"to":"demo/worker","body":"see"}),
    ] {
        let error = post(&rig, pane("%2"), params.clone()).await.unwrap_err();
        assert_eq!(error.0.code, "refused", "{params}");
        assert_eq!(
            error.0.message,
            "agent demo/worker has a discovered process but its recorded session is not confirmed; post not written; inspect atm list and atm agent list before retrying"
        );
        assert_eq!(rig.org.status().unwrap(), before, "{params}");
    }
    // A post that does not wake that seat is not held back by it.
    let result = post(
        &rig,
        pane("%2"),
        json!({"channel":"demo","body":"see @owner"}),
    )
    .await
    .unwrap();
    assert_eq!(result["woke"][0]["to"], "owner");
}

// s3-d1. Break: the post check ignores the recorded process of the seat's
// session: its discovery row is that process, so the post is written.
#[tokio::test]
async fn post_to_a_restarted_seat_whose_process_is_recorded_is_written() {
    let rig = rig().await;
    let _cwd = discovery_row(&rig).await; // %1: demo/worker's pane
    rig.run(own_process("s1", own_process_start())).await;
    let result = post(&rig, pane("%2"), json!({"to":"demo/worker","body":"see"}))
        .await
        .unwrap();
    assert_eq!(result["woke"][0]["to"], "demo/worker");
}

// Break: no row for the owner, or his row queued instead of delivered at once.
#[tokio::test]
async fn owner_tag_and_direct_post_to_owner_create_his_delivered_row() {
    let rig = rig().await;
    let tagged = post(
        &rig,
        pane("%1"),
        json!({"channel":"demo","body":"done @owner"}),
    )
    .await
    .unwrap();
    let direct = post(&rig, pane("%1"), json!({"to":"owner","body":"done"}))
        .await
        .unwrap();
    assert_eq!(direct["channel"], "demo/worker+owner");
    let state = rig.org.snapshot().await.unwrap();
    for result in [tagged, direct] {
        assert_eq!(result["woke"].as_array().unwrap().len(), 1, "{result}");
        assert_eq!(result["woke"][0]["to"], "owner");
        let id: MessageId =
            serde_json::from_value(result["woke"][0]["message_id"].clone()).unwrap();
        assert_eq!(
            state.message(&id).unwrap().status,
            DeliveryStatus::Delivered
        );
        assert_eq!(result["unavailable"], json!([]));
    }
}

// Break: a down or stuck seat's wake dropped, not reported, or reported with
// the wrong status, time or row; a seat that can be woken reported.
#[tokio::test]
async fn tag_of_a_down_or_stuck_seat_queues_and_is_listed_unavailable() {
    let rig = rig().await;
    // Later than the adoption, so `since` is the status change, not the record's birth.
    let changed = utc() + chrono::Duration::minutes(7);
    for command in [
        OrgCommand::Stuck {
            agent_id: worker(),
            reason: "fixture".into(),
        },
        OrgCommand::Down {
            agent_id: other(),
            reason: "fixture".into(),
        },
    ] {
        let seq = rig.org.execute(command, changed).await.unwrap()[0].seq;
        rig.folded(seq).await;
    }
    let result = post(
        &rig,
        owner(),
        json!({"channel":"demo","body":"status?","tags":["worker","other","lead"]}),
    )
    .await
    .unwrap();
    let p = seq_of(&result);
    let since = serde_json::to_value(changed).unwrap();
    assert_eq!(
        result["unavailable"],
        json!([{"to":"demo/worker","state":"stuck","since":since,"message_id":m(p + 1)},
               {"to":"demo/other","state":"down","since":since,"message_id":m(p + 2)}])
    );
    assert_eq!(result["woke"].as_array().unwrap().len(), 3);
    let state = rig.org.snapshot().await.unwrap();
    for id in [p + 1, p + 2, p + 3] {
        assert_eq!(
            state.message(&MessageId::new(id)).unwrap().status,
            DeliveryStatus::Queued
        );
    }
}

// Break: unresolved inline tokens dropped, or a strict tag's text listed.
#[tokio::test]
async fn inline_unresolved_tokens_are_reported() {
    let rig = rig().await;
    let result = post(
        &rig,
        owner(),
        json!({"channel":"demo","body":"hi @nobody and @worker","tags":["other"]}),
    )
    .await
    .unwrap();
    assert_eq!(
        result["unresolved"],
        json!([{"text":"@nobody","reason":"unknown"}])
    );
    assert_eq!(result["woke"].as_array().unwrap().len(), 2);
}

// Break: a limit checked after the post is written, or not at all.
#[tokio::test]
async fn size_limits_and_one_target_are_checked_before_anything_is_written() {
    let rig = rig().await;
    let before = rig.org.status().unwrap();
    let many: Vec<String> = (0..33).map(|i| format!("f{i}.md")).collect();
    let tags: Vec<String> = (0..33).map(|i| format!("t{i}")).collect();
    for (params, message) in [
        (
            json!({"channel":"demo","body":"x".repeat(16 * 1024 + 1)}),
            "body exceeds 16384 UTF-8 bytes",
        ),
        (
            json!({"channel":"demo","body":""}),
            "empty body requires an attachment",
        ),
        (
            json!({"channel":"demo","body":"x","attachments":many}),
            "at most 32 attachments are allowed",
        ),
        (
            json!({"channel":"demo","body":"x","tags":tags}),
            "at most 32 tags are allowed",
        ),
        (
            json!({"channel":"demo","to":"owner","body":"x"}),
            "exactly one of channel, to and reply_to is required",
        ),
        (
            json!({"body":"x"}),
            "exactly one of channel, to and reply_to is required",
        ),
    ] {
        let error = post(&rig, owner(), params.clone()).await.unwrap_err();
        assert_eq!(error.0.code, "invalid_params", "{params}");
        assert!(error.0.message.contains(message), "{}", error.0.message);
    }
    assert_eq!(rig.org.status().unwrap(), before);
}

// Break: a repeated key writes a second post or new rows.
#[tokio::test]
async fn repeat_client_key_names_the_first_post_and_writes_nothing() {
    let rig = rig().await;
    let params = json!({"channel":"demo","body":"once","tags":["worker"],"client_key":"k1"});
    let first = post(&rig, owner(), params.clone()).await.unwrap();
    let mut committed = rig.org.committed.subscribe();
    let again = post(&rig, owner(), params).await.unwrap();
    assert!(drain(&mut committed).is_empty());
    assert_eq!(again["post_id"], first["post_id"]);
    assert_eq!(again["woke"], first["woke"]);
}

// Break: a repeat that names the post but leaves its missing rows missing.
#[tokio::test]
async fn repeat_completes_the_missing_wakes_of_the_first_post() {
    let rig = rig().await;
    // The post alone, as a request that died after its first append leaves it.
    let seq = rig
        .run(raw_post("half done", &["worker"], Some("k2")))
        .await;
    assert_eq!(wake_rows(&rig.org.snapshot().await.unwrap(), None).len(), 1);
    let mut committed = rig.org.committed.subscribe();
    let result = post(
        &rig,
        owner(),
        json!({"channel":"demo","body":"half done","tags":["worker"],"client_key":"k2"}),
    )
    .await
    .unwrap();
    assert_eq!(result["post_id"], m(seq));
    let records = drain(&mut committed);
    assert_eq!(kinds(&records), ["msg.sent"]);
    assert_eq!(
        result["woke"],
        json!([{"to":"demo/worker","message_id":m(records[0].seq)}])
    );
    assert!(wake_rows(&rig.org.snapshot().await.unwrap(), None).is_empty());
}

// Break: a policy refusal mapped to the wrong code, or written anyway.
#[tokio::test]
async fn post_refusals_map_to_codes() {
    let rig = rig().await;
    // A direct channel the owner is not a party to.
    post(
        &rig,
        pane("%1"),
        json!({"to":"demo/other","body":"between us"}),
    )
    .await
    .unwrap();
    let before = rig.org.status().unwrap();
    for (caller, params, code) in [
        (
            owner(),
            json!({"channel":"nope","body":"x"}),
            "unknown_channel",
        ),
        (
            owner(),
            json!({"channel":"demo/other+demo/worker","body":"x"}),
            "refused",
        ),
        (
            owner(),
            json!({"channel":"demo","body":"x","tags":["ghost"]}),
            "invalid_params",
        ),
        (
            owner(),
            json!({"channel":"demo","body":"x","tags":["a/b/c"]}),
            "invalid_params",
        ),
        (owner(), json!({"reply_to":"m999","body":"x"}), "not_found"),
        (owner(), json!({"to":"system","body":"x"}), "invalid_params"),
        (
            pane("%1"),
            json!({"to":"demo/worker","body":"x"}),
            "invalid_params",
        ),
        (owner(), json!({"to":"demo/ghost","body":"x"}), "not_found"),
        (
            owner(),
            json!({"channel":"demo","body":"x","client_key":"k".repeat(65)}),
            "invalid_params",
        ),
    ] {
        let error = post(&rig, caller, params.clone()).await.unwrap_err();
        assert_eq!(error.0.code, code, "{params}: {}", error.0.message);
    }
    assert_eq!(rig.org.status().unwrap(), before);
}

// Break: a request field that marks a post internal, or a valid request that
// claims success from a disabled service.
#[tokio::test]
async fn post_params_cannot_mark_a_post_internal() {
    let mut router = MethodRouter::default();
    super::channel_methods::register_post(&mut router, MessageHandle::disabled("test")).unwrap();
    let call = |params: Value| {
        router.call(
            CallContext {
                client_id: CallerId("post".into()),
                from_pane: None,
            },
            "channel.post",
            params,
        )
    };
    for field in ["internal", "decision", "no_route"] {
        let mut params = json!({"channel":"demo","body":"x"});
        params[field] = json!(true);
        let error = call(params).await.unwrap_err();
        assert_eq!(error.0.code, "invalid_params", "{field}");
    }
    let error = call(json!({"channel":"demo","body":"x"}))
        .await
        .unwrap_err();
    assert_eq!(error.0.code, "unavailable");
}

// The lead's note N6: the owner's marker passed the post before his wake row
// was written. Break: the row stays unread, or the heal reads other rows.
#[tokio::test]
async fn owner_row_behind_his_marker_is_read_with_it() {
    let mut rig = Rig::relayed().await;
    let mut last = 0;
    for command in [
        OrgCommand::RegisterProject {
            project_id: "demo".parse().unwrap(),
            root: "/demo".into(),
            move_root: false,
        },
        OrgCommand::Adopt {
            agent_id: lead(),
            pane: "%3".into(),
            session_id: Some("s3".into()),
        },
        OrgCommand::Adopt {
            agent_id: worker(),
            pane: "%1".into(),
            session_id: Some("s1".into()),
        },
    ] {
        last = rig.org.execute(command, utc()).await.unwrap()[0].seq;
    }
    rig.relay();
    rig.folded(last).await;
    let mut committed = rig.org.committed.subscribe();
    let p = last + 1;
    let (call, pending) = rig.write_awaiting_fold(p, {
        let messages = rig.messages.clone();
        async move {
            let params =
                serde_json::from_value(json!({"channel":"demo","body":"done @owner"})).unwrap();
            messages.post(params, pane("%1")).await
        }
    });
    // The post is committed, the service has not folded it, so the handler
    // has not written his row: the owner's app shows the post and marks it.
    guarded(pending).await.unwrap();
    rig.org
        .execute(
            OrgCommand::Channel(ChannelCommand::Seen {
                channel: ChannelId::Project("demo".parse().unwrap()),
                reader: Address::Owner,
                through: MessageId::new(p),
            }),
            utc(),
        )
        .await
        .unwrap();
    let result = relay_until(&mut rig, call).await.unwrap();
    let records = drain(&mut committed);
    assert_eq!(
        kinds(&records),
        ["channel.posted", "channel.seen", "msg.sent", "msg.read"]
    );
    let row = MessageId::new(records[2].seq);
    assert_eq!(records[3].body, json!({"message_ids":[row]}));
    assert_eq!(result.woke.len(), 1);
    let state = rig.org.snapshot().await.unwrap();
    assert!(state.message(&row).unwrap().read_at.is_some());
}

/// A disk-backed writer whose `fail_at`-th append fails as an I/O error does:
/// nothing written, and the writer stops until a restart.
pub(super) async fn disk_org(dir: &Path, fail_at: usize) -> (OrgHandle, CancellationToken) {
    let ready = super::storage::open(dir.into()).await.unwrap();
    let (requests, receive) = mpsc::channel(64);
    let (health, status) = watch::channel(Ok(ready.seq));
    let (committed, _) = broadcast::channel(1024);
    let cancel = CancellationToken::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let log = ready.log.clone();
    tokio::spawn(super::actor::run(
        ready.state,
        receive,
        health,
        committed.clone(),
        cancel.clone(),
        move |envelope: Envelope| {
            let call = calls.fetch_add(1, Ordering::SeqCst) + 1;
            let log = log.clone();
            async move {
                if call == fail_at {
                    Err(EventLogError::Io {
                        path: "events".into(),
                        source: std::io::Error::other("injected failure"),
                    })
                } else {
                    log.append(envelope.ts, &envelope.kind, envelope.body).await
                }
            }
        },
    ));
    let org = OrgHandle {
        requests,
        health: status,
        committed,
        reads: None,
    };
    (org, cancel)
}

/// A daemon start on a copy of what reached the disk.
pub(super) async fn restart(from: &Path) -> (tempfile::TempDir, OrgHandle, CancellationToken) {
    let dir = tempfile::tempdir().unwrap();
    let events = dir.path().join("log/events");
    std::fs::create_dir_all(&events).unwrap();
    std::fs::copy(
        from.join("log/events/00000001.jsonl"),
        events.join("00000001.jsonl"),
    )
    .unwrap();
    let cancel = CancellationToken::new();
    let org = OrgHandle::open(dir.path().into(), cancel.clone()).await;
    (dir, org, cancel)
}

pub(super) fn setup() -> [OrgCommand; 4] {
    [
        OrgCommand::RegisterProject {
            project_id: "demo".parse().unwrap(),
            root: "/demo".into(),
            move_root: false,
        },
        OrgCommand::Adopt {
            agent_id: lead(),
            pane: "%3".into(),
            session_id: Some("s3".into()),
        },
        OrgCommand::Adopt {
            agent_id: worker(),
            pane: "%1".into(),
            session_id: Some("s1".into()),
        },
        OrgCommand::Adopt {
            agent_id: other(),
            pane: "%2".into(),
            session_id: Some("s2".into()),
        },
    ]
}

/// The post commits, then the writer fails after `before` of its two rows.
async fn crash_after_post(before: usize) {
    let dir = tempfile::tempdir().unwrap();
    let (org, cancel) = disk_org(dir.path(), setup().len() + 1 + before + 1).await;
    for command in setup() {
        org.execute(command, utc()).await.unwrap();
    }
    let p = org
        .execute(raw_post("hold W2", &["worker", "other"], None), utc())
        .await
        .unwrap()[0]
        .seq;
    let rows = wake_rows(&org.snapshot().await.unwrap(), Some(MessageId::new(p)));
    assert_eq!(rows.len(), 2);
    for row in &rows[..before] {
        org.execute(OrgCommand::SendMessage(row.clone()), utc())
            .await
            .unwrap();
    }
    let crash = org
        .execute(OrgCommand::SendMessage(rows[before].clone()), utc())
        .await
        .unwrap_err();
    assert!(
        matches!(crash, OrgServiceError::Unavailable(_)),
        "{crash:?}"
    );
    cancel.cancel();

    let (first_dir, first, stop) = restart(dir.path()).await;
    let state = first.snapshot().await.unwrap();
    assert_eq!(
        rows_for(&state, p).len(),
        before,
        "the disk holds the prefix"
    );
    assert_eq!(repair(&first, utc(), true).await.unwrap(), 2 - before);
    let state = first.snapshot().await.unwrap();
    let pairs: Vec<String> = rows_for(&state, p).into_iter().map(|(to, _)| to).collect();
    assert_eq!(pairs, ["demo/other", "demo/worker"]);
    assert!(wake_rows(&state, None).is_empty());
    stop.cancel();

    let (_dir, second, stop) = restart(first_dir.path()).await;
    assert_eq!(
        repair(&second, utc(), true).await.unwrap(),
        0,
        "a second restart writes nothing"
    );
    assert_eq!(rows_for(&second.snapshot().await.unwrap(), p).len(), 2);
    stop.cancel();
}

// Break: no repair after startup, or a repair that writes a pair twice.
#[tokio::test]
async fn failed_append_after_the_post_leaves_every_wake_to_the_restart() {
    crash_after_post(0).await;
}

// Break: a repair that rewrites the row that did reach the disk.
#[tokio::test]
async fn failed_append_after_one_wake_restart_writes_only_the_rest() {
    crash_after_post(1).await;
}

// Break: a pass that writes again what an earlier pass wrote.
#[tokio::test]
async fn repair_pass_is_idempotent() {
    let rig = rig().await;
    let p = rig.run(raw_post("twice", &["worker", "other"], None)).await;
    assert_eq!(repair(&rig.org, utc(), false).await.unwrap(), 2);
    assert_eq!(repair(&rig.org, utc(), false).await.unwrap(), 0);
    assert_eq!(rows_for(&rig.org.snapshot().await.unwrap(), p).len(), 2);
}

// Break: no pass at startup, no pass after a new post, or a task that does
// not stop with its token. A real log: the task watches committed records.
#[tokio::test]
async fn repair_task_runs_after_startup_and_after_each_channel_event() {
    let dir = tempfile::tempdir().unwrap();
    let stop = CancellationToken::new();
    let org = OrgHandle::open(dir.path().into(), stop.clone()).await;
    for command in setup() {
        org.execute(command, utc()).await.unwrap();
    }
    let first = org
        .execute(raw_post("one", &["worker"], None), utc())
        .await
        .unwrap()[0]
        .seq;
    let cancel = CancellationToken::new();
    let (mut status, task) = spawn_wake_repair(org.clone(), clock(), cancel.clone());
    let through = |seq: u64| move |s: &Result<u64, String>| matches!(s, Ok(c) if *c >= seq);
    guarded(status.wait_for(through(first))).await.unwrap();
    assert_eq!(rows_for(&org.snapshot().await.unwrap(), first).len(), 1);

    let second = org
        .execute(raw_post("two", &["worker", "other"], None), utc())
        .await
        .unwrap()[0]
        .seq;
    guarded(status.wait_for(through(second))).await.unwrap();
    let state = org.snapshot().await.unwrap();
    assert_eq!(rows_for(&state, second).len(), 2);
    assert!(wake_rows(&state, None).is_empty());

    // A marker alone runs no pass. Each pass reports the record that started
    // it, and awaits the writer before the next one, so this single-threaded
    // test sees every pass's report: the marker's sequence never appears.
    let marker = org
        .execute(
            OrgCommand::Channel(ChannelCommand::Seen {
                channel: ChannelId::Project("demo".parse().unwrap()),
                reader: Address::Owner,
                through: MessageId::new(second),
            }),
            utc(),
        )
        .await
        .unwrap()[0]
        .seq;
    let third = org
        .execute(raw_post("three", &["worker"], None), utc())
        .await
        .unwrap()[0]
        .seq;
    let mut reported = Vec::new();
    guarded(status.wait_for(|s| {
        if let Ok(c) = s {
            reported.push(*c);
        }
        matches!(s, Ok(c) if *c >= third)
    }))
    .await
    .unwrap();
    assert!(
        !reported.contains(&marker),
        "{reported:?} has the marker {marker}"
    );
    assert_eq!(reported.last(), Some(&third));

    cancel.cancel();
    guarded(task).await.unwrap();
    assert!(status.borrow().is_err(), "a stopped task says so");
    stop.cancel();
}

// Break: the inline path writes a second row for a pair the repair already
// wrote, or its result names rows it did not find.
#[tokio::test]
async fn repair_before_inline_writes_one_row_per_pair() {
    let mut rig = Rig::relayed().await;
    let mut last = 0;
    for command in setup() {
        last = rig.org.execute(command, utc()).await.unwrap()[0].seq;
    }
    rig.relay();
    rig.folded(last).await;
    let p = last + 1;
    let (call, pending) = rig.write_awaiting_fold(p, {
        let messages = rig.messages.clone();
        async move {
            let params = serde_json::from_value(
                json!({"channel":"demo","body":"race","tags":["worker","other"]}),
            )
            .unwrap();
            messages.post(params, owner()).await
        }
    });
    // The post is committed and its handler waits for the fold: the repair
    // (which the same post triggers) writes both rows first.
    guarded(pending).await.unwrap();
    assert_eq!(repair(&rig.org, utc(), false).await.unwrap(), 2);
    let result = relay_until(&mut rig, call).await.unwrap();
    let rows = rows_for(&rig.org.snapshot().await.unwrap(), p);
    assert_eq!(rows.len(), 2, "one row per pair");
    let mut named: Vec<(String, MessageId)> = result
        .woke
        .iter()
        .map(|w| (w.to.to_string(), w.message_id))
        .collect();
    named.sort();
    assert_eq!(named, rows);
}

// N6 on the repair path: a restart (or a routing answer) writes the owner's
// row after his marker passed the post. Break: the repair leaves it unread.
#[tokio::test]
async fn repair_reads_an_owner_row_behind_his_marker() {
    let rig = rig().await;
    let OrgCommand::Channel(ChannelCommand::Post(mut request)) =
        raw_post("for you @owner", &[], None)
    else {
        unreachable!("raw_post builds a post");
    };
    request.author = Address::Agent(worker());
    let p = rig
        .run(OrgCommand::Channel(ChannelCommand::Post(request)))
        .await;
    rig.run(OrgCommand::Channel(ChannelCommand::Seen {
        channel: ChannelId::Project("demo".parse().unwrap()),
        reader: Address::Owner,
        through: MessageId::new(p),
    }))
    .await;
    let mut committed = rig.org.committed.subscribe();
    assert_eq!(repair(&rig.org, utc(), false).await.unwrap(), 2);
    let records = drain(&mut committed);
    assert_eq!(kinds(&records), ["msg.sent", "msg.read"]);
    let row = MessageId::new(records[0].seq);
    assert_eq!(records[1].body, json!({"message_ids":[row]}));
    assert!(rig
        .org
        .snapshot()
        .await
        .unwrap()
        .message(&row)
        .unwrap()
        .read_at
        .is_some());
}

// The common production race (review S2): the repair, triggered by the post's
// own event, writes a row between two inline rows, so the inline SendMessage
// for that pair decides to nothing. Break: an empty commit treated as an
// error, or a result that misses the repair's row.
#[tokio::test]
async fn inline_row_the_repair_already_wrote_is_not_an_error() {
    let mut rig = Rig::relayed().await;
    let mut last = 0;
    for command in setup() {
        last = rig.org.execute(command, utc()).await.unwrap()[0].seq;
    }
    rig.relay();
    rig.folded(last).await;
    let p = last + 1;
    let (call, pending) = rig.write_awaiting_fold(p, {
        let messages = rig.messages.clone();
        async move {
            let params = serde_json::from_value(
                json!({"channel":"demo","body":"race","tags":["worker","other"]}),
            )
            .unwrap();
            messages.post(params, owner()).await
        }
    });
    // The post is committed, its fold held: let only the post through.
    guarded(pending).await.unwrap();
    rig.relay();
    // The handler writes worker's row, then waits for that row's fold.
    let mut health = rig.org.health.clone();
    guarded(health.wait_for(|h| h.as_ref().is_ok_and(|seq| *seq > p)))
        .await
        .unwrap();
    // The repair now finds only other's row missing, and writes it.
    assert_eq!(repair(&rig.org, utc(), false).await.unwrap(), 1);
    let result = relay_until(&mut rig, call).await.unwrap();
    let rows = rows_for(&rig.org.snapshot().await.unwrap(), p);
    assert_eq!(rows.len(), 2, "one row per pair: {rows:?}");
    let mut named: Vec<(String, MessageId)> = result
        .woke
        .iter()
        .map(|w| (w.to.to_string(), w.message_id))
        .collect();
    named.sort();
    assert_eq!(named, rows);
    assert_eq!(rows[1], ("demo/worker".to_owned(), MessageId::new(p + 1)));
    assert_eq!(rows[0], ("demo/other".to_owned(), MessageId::new(p + 2)));
}

/// A writer the test scripts: snapshots of a real fold, writes decided and
/// applied on it, and an injected answer for the n-th write (0-based).
fn scripted_writer(
    mut state: atm_org::OrgState,
    mut seq: u64,
    script: impl Fn(&OrgCommand, usize) -> Option<OrgServiceError> + Send + 'static,
) -> (OrgHandle, watch::Sender<super::Health>, Arc<AtomicUsize>) {
    let (requests, mut receive) = mpsc::channel::<super::Request>(64);
    let (health, status) = watch::channel(Ok(seq));
    let (committed, _) = broadcast::channel(1024);
    let writes = Arc::new(AtomicUsize::new(0));
    let count = writes.clone();
    tokio::spawn(async move {
        while let Some(request) = receive.recv().await {
            match request {
                super::Request::Snapshot(reply) => {
                    let _ = reply.send(state.clone());
                }
                super::Request::Execute {
                    command,
                    now,
                    reply,
                    ..
                } => {
                    let n = count.fetch_add(1, Ordering::SeqCst);
                    let answer = match script(&command, n) {
                        Some(error) => Err(error),
                        None => state
                            .decide(&command, now)
                            .map_err(Into::into)
                            .map(|events| {
                                let mut committed = Vec::new();
                                for event in events {
                                    seq += 1;
                                    state.apply(seq, now, &event).unwrap();
                                    committed.push(Envelope {
                                        seq,
                                        ts: now,
                                        kind: event.kind().into(),
                                        body: event.to_body(),
                                    });
                                }
                                super::TaskCommit {
                                    events: committed,
                                    before: None,
                                }
                            }),
                    };
                    let _ = reply.send(answer);
                }
                _ => unreachable!("repair takes snapshots and writes only"),
            }
        }
    });
    let org = OrgHandle {
        requests,
        health: status,
        committed,
        reads: None,
    };
    (org, health, writes)
}

/// The fold after these commands: demo with lead, worker and other, a post by
/// the worker tagging the owner and `other`, and the owner's marker through it.
fn owner_post_behind_marker() -> (atm_org::OrgState, u64) {
    let mut state = atm_org::OrgState::default();
    let mut seq = 0;
    let OrgCommand::Channel(ChannelCommand::Post(mut request)) =
        raw_post("for you @owner", &["other"], None)
    else {
        unreachable!("raw_post builds a post");
    };
    request.author = Address::Agent(worker());
    let post = OrgCommand::Channel(ChannelCommand::Post(request));
    for command in setup().into_iter().chain([post]) {
        for event in state.decide(&command, utc()).unwrap() {
            seq += 1;
            state.apply(seq, utc(), &event).unwrap();
        }
    }
    let seen = OrgCommand::Channel(ChannelCommand::Seen {
        channel: ChannelId::Project("demo".parse().unwrap()),
        reader: Address::Owner,
        through: MessageId::new(seq),
    });
    for event in state.decide(&seen, utc()).unwrap() {
        seq += 1;
        state.apply(seq, utc(), &event).unwrap();
    }
    (state, seq)
}

// Review S1. A full request queue answers `Unavailable` while the writer is
// healthy; a timeout leaves it healthy too. Break: the pass (and the task that
// runs it) ends for good on either, or keeps retrying once the writer is gone.
// The paused clock runs the 1 s backoff at once: the test syncs on the pass.
#[tokio::test(start_paused = true)]
async fn repair_retries_while_the_writer_is_healthy_and_stops_when_it_is_gone() {
    for passing in [
        OrgServiceError::Unavailable("organisation queue: no available capacity".into()),
        OrgServiceError::Timeout,
    ] {
        let (state, seq) = owner_post_behind_marker();
        let failure = std::sync::Mutex::new(Some(passing));
        let (org, _health, writes) = scripted_writer(state, seq, move |_, n| {
            (n == 0).then(|| failure.lock().unwrap().take()).flatten()
        });
        guarded(pass(&org, utc(), false)).await.unwrap();
        // The failed first write, then two rows and the owner heal.
        assert_eq!(writes.load(Ordering::SeqCst), 4);
        assert!(wake_rows(&org.snapshot().await.unwrap(), None).is_empty());
    }

    let (state, seq) = owner_post_behind_marker();
    let (org, health, writes) = scripted_writer(state, seq, |_, _| {
        Some(OrgServiceError::Unavailable(
            "organisation actor stopped".into(),
        ))
    });
    let _ = health.send_replace(Err("organisation actor stopped".into()));
    let error = guarded(pass(&org, utc(), false)).await.unwrap_err();
    assert!(
        matches!(error, OrgServiceError::Unavailable(_)),
        "{error:?}"
    );
    assert_eq!(
        writes.load(Ordering::SeqCst),
        0,
        "a stopped writer takes no request"
    );
}

// Review S1. Break: a refused heal (or row) ends the pass instead of being
// logged and skipped like the rest.
#[tokio::test]
async fn repair_logs_a_refused_heal_or_row_and_finishes_its_pass() {
    let (state, seq) = owner_post_behind_marker();
    let (org, _health, writes) = scripted_writer(state, seq, |command, _| {
        matches!(command, OrgCommand::Channel(ChannelCommand::Seen { .. })).then(|| {
            OrgServiceError::Refused(atm_org::Refusal::InvalidValue {
                field: "through",
                reason: "injected".into(),
            })
        })
    });
    assert_eq!(
        repair(&org, utc(), false).await.unwrap(),
        2,
        "both rows, no heal"
    );
    assert_eq!(writes.load(Ordering::SeqCst), 3);

    let (state, seq) = owner_post_behind_marker();
    let (org, _health, _) = scripted_writer(state, seq, |command, _| {
        matches!(command, OrgCommand::SendMessage(row) if row.to == Address::Owner).then(|| {
            OrgServiceError::Refused(atm_org::Refusal::InvalidValue {
                field: "post",
                reason: "injected".into(),
            })
        })
    });
    assert_eq!(
        repair(&org, utc(), false).await.unwrap(),
        1,
        "other's row only"
    );
    let state = org.snapshot().await.unwrap();
    assert_eq!(
        wake_rows(&state, None).len(),
        1,
        "the owner's row waits for the next pass"
    );
}

// Review S3. The row was written, then the writer died (or the request was
// dropped) before its heal: no row is missing, so only a heal of every unread
// owner wake finds it. Break: the startup pass heals only rows it wrote.
#[tokio::test]
async fn startup_pass_heals_an_owner_row_left_unread_behind_his_marker() {
    let dir = tempfile::tempdir().unwrap();
    let stop = CancellationToken::new();
    let org = OrgHandle::open(dir.path().into(), stop.clone()).await;
    let (state, _) = owner_post_behind_marker();
    // The same facts on the real log: setup, the post, his marker through it.
    for command in setup() {
        org.execute(command, utc()).await.unwrap();
    }
    let OrgCommand::Channel(ChannelCommand::Post(mut request)) =
        raw_post("for you @owner", &["other"], None)
    else {
        unreachable!("raw_post builds a post");
    };
    request.author = Address::Agent(worker());
    let p = org
        .execute(OrgCommand::Channel(ChannelCommand::Post(request)), utc())
        .await
        .unwrap()[0]
        .seq;
    org.execute(
        OrgCommand::Channel(ChannelCommand::Seen {
            channel: ChannelId::Project("demo".parse().unwrap()),
            reader: Address::Owner,
            through: MessageId::new(p),
        }),
        utc(),
    )
    .await
    .unwrap();
    // His row and other's, with no heal: the request died after its rows.
    for row in wake_rows(&org.snapshot().await.unwrap(), Some(MessageId::new(p))) {
        org.execute(OrgCommand::SendMessage(row), utc())
            .await
            .unwrap();
    }
    assert_eq!(
        wake_rows(&state, None).len(),
        2,
        "the fixture's post wakes two"
    );
    assert_eq!(
        owner_row(&org, p).await,
        Some(None),
        "unread behind his marker"
    );
    // An ordinary pass finds nothing missing and heals nothing.
    assert_eq!(repair(&org, utc(), false).await.unwrap(), 0);
    assert_eq!(owner_row(&org, p).await, Some(None));

    let cancel = CancellationToken::new();
    let (mut status, task) = spawn_wake_repair(org.clone(), clock(), cancel.clone());
    let cursor = org.status().unwrap();
    guarded(status.wait_for(|s| matches!(s, Ok(c) if *c >= cursor)))
        .await
        .unwrap();
    assert!(
        owner_row(&org, p).await.unwrap().is_some(),
        "the startup pass read his row"
    );
    cancel.cancel();
    guarded(task).await.unwrap();
    stop.cancel();
}

/// When the owner read his wake of `post`: `None` if he has no row.
async fn owner_row(org: &OrgHandle, post: u64) -> Option<Option<chrono::DateTime<chrono::Utc>>> {
    org.snapshot()
        .await
        .unwrap()
        .messages()
        .find(|m| m.content.post == Some(MessageId::new(post)) && m.content.to == Address::Owner)
        .map(|m| m.read_at)
}
