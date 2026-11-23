//! Channel reads over a private seeded journal, with no agent or delivery runtime.
#![allow(clippy::unwrap_used)] // Fixtures and their helpers abort on bad test setup.

use atm_org::{
    channel_policy::GroupDirectory, Address, ChannelCommand, ChannelId, ChannelRefusal, MessageId,
    MessageKind, OrgCommand, OrgEvent, PostMutation, PostRequest, PostTarget, Refusal,
};
use atmd::{
    org::{OrgHandle, TailParams},
    registry::spawn_registry,
    server::{
        methods::{
            router, CallContext, CallerId, CallerPane, MethodError, MethodRouter, MethodServices,
        },
        DaemonServer,
    },
};
use chrono::{DateTime, Duration as Delta, Utc};
use serde_json::{json, Value};
use std::{
    future::{poll_fn, Future},
    io::Write,
    path::Path,
    task::Poll,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    time::timeout,
};
use tokio_util::sync::CancellationToken;

fn at(seq: u64) -> DateTime<Utc> {
    "2026-01-01T12:00:00Z".parse::<DateTime<Utc>>().unwrap()
        + Delta::seconds(i64::try_from(seq).unwrap())
}

fn fact(kind: &str, body: Value) -> OrgEvent {
    OrgEvent::from_parts(kind, &body).unwrap().unwrap()
}

fn project(project: &str) -> OrgEvent {
    fact(
        "project.registered",
        json!({"project_id":project,"root":format!("/sample/{project}")}),
    )
}

fn adopt(id: &str, pane: &str, session: Option<&str>) -> OrgEvent {
    fact(
        "agent.adopted",
        json!({"agent_id":id,"pane":pane,"session_id":session}),
    )
}

fn message(from: &str, to: &str, body: &str, kind: &str, post: Option<&str>) -> OrgEvent {
    fact(
        "msg.sent",
        json!({"from":from,"to":to,"body":body,"kind":kind,"attachments":[],"task":null,"post":post}),
    )
}

fn sample() -> Vec<OrgEvent> {
    vec![
        project("alpha"),
        adopt("alpha/lead", "%1", Some("s-past")),
        adopt("alpha/worker", "%2", Some("s-reader")),
        adopt("alpha/former", "%3", None),
        fact(
            "agent.down",
            json!({"agent_id":"alpha/former","reason":"gone"}),
        ),
        project("beta"),
        adopt("beta/lead", "%4", None),
        fact(
            "session.summary",
            json!({"session_id":"s-past","pane":"%1","agent_id":"alpha/lead","harness":"pi","started":"2026-01-01T11:59:00Z"}),
        ),
        message("alpha/worker", "owner", "old note", "note", None),
        message("system", "alpha/worker", "system note", "note", None),
        message(
            "alpha/worker",
            "beta/lead",
            "cross-project note",
            "note",
            None,
        ),
        fact(
            "decision.requested",
            json!({"requester":"alpha/lead","kind":"policy","title":"Review","context":"","files":[],"options":["go","hold"],"recommended":"go","may_be_undone":true,"changes_intent":false}),
        ),
        fact(
            "channel.posted",
            json!({"channel":"alpha","author":"alpha/lead","session_id":"s-past","kind":"question","body":"Review @owner","attachments":[],"mentions":["owner"],"wake":["owner","alpha/worker"],"route":false,"decision":{"decision_id":"d12","step":"asked"}}),
        ),
        message(
            "alpha/lead",
            "alpha/worker",
            "Review @owner",
            "question",
            Some("m13"),
        ),
        fact(
            "channel.posted",
            json!({"channel":"alpha","author":"owner","kind":"note","body":"Owner's note","attachments":[],"mentions":[],"wake":[],"route":false}),
        ),
        fact(
            "channel.posted",
            json!({"channel":"alpha","author":"alpha/worker","session_id":"s-reader","thread":"m13","kind":"note","body":"Reply","attachments":[],"mentions":[],"wake":[],"route":true}),
        ),
        fact(
            "channel.routed",
            json!({"post":"m16","wake":["owner"],"by":{"kind":"router","name":"fixture","model":null}}),
        ),
        message("alpha/worker", "owner", "Reply", "note", Some("m16")),
    ]
}

fn seed(dir: &Path, facts: &[OrgEvent]) {
    let path = dir.join("log/events");
    std::fs::create_dir_all(&path).unwrap();
    let mut out =
        std::io::BufWriter::new(std::fs::File::create(path.join("00000001.jsonl")).unwrap());
    for (i, fact) in facts.iter().enumerate() {
        let seq = u64::try_from(i + 1).unwrap();
        writeln!(
            out,
            "{}",
            json!({"seq":seq,"ts":at(seq),"kind":fact.kind(),"body":fact.to_body()})
        )
        .unwrap();
    }
    out.flush().unwrap();
}

struct Rig {
    org: OrgHandle,
    router: MethodRouter,
    cancel: CancellationToken,
    dir: tempfile::TempDir,
}

impl Rig {
    async fn new(facts: &[OrgEvent]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path(), facts);
        let cancel = CancellationToken::new();
        let org = OrgHandle::open(dir.path().into(), cancel.clone()).await;
        assert_eq!(org.status().unwrap(), facts.len() as u64);
        let router = router(services(&org, dir.path())).unwrap();
        Self {
            org,
            router,
            cancel,
            dir,
        }
    }

    async fn call(
        &self,
        pane: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<Value, MethodError> {
        self.router
            .call(
                CallContext {
                    client_id: CallerId("channel-read".into()),
                    from_pane: pane.map(|p| CallerPane(p.into())),
                },
                method,
                params,
            )
            .await
    }

    async fn list(&self, pane: Option<&str>, all: bool) -> Value {
        let params = if all { json!({"all":true}) } else { json!({}) };
        self.call(pane, "channel.list", params).await.unwrap()
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

fn services(org: &OrgHandle, dir: &Path) -> MethodServices {
    let mut svc = MethodServices::new(spawn_registry());
    svc.org = org.clone();
    svc.state_dir = dir.to_owned();
    svc
}

struct Wire {
    io: BufReader<UnixStream>,
    stop: CancellationToken,
    task: tokio::task::JoinHandle<Result<(), atmd::server::ServerError>>,
    control_task: tokio::task::JoinHandle<()>,
}

const CAGE: Duration = Duration::from_secs(30);

impl Wire {
    async fn start(dir: &Path, mut svc: MethodServices) -> Self {
        let path = dir.join("wire.sock");
        let stop = CancellationToken::new();
        // The default server starts real control services. This rig only needs IPC.
        let (control, control_task) = atmd::control::service::spawn_control(
            Box::new(atm_tmux::MockTmuxClient::new()),
            svc.registry.clone(),
            svc.org.clone(),
            stop.clone(),
        );
        svc.control = Some(control);
        svc.agents = Some(atmd::org::agents::AgentHandle::disabled("channel fixture"));
        svc.decisions = Some(atmd::org::decisions::DecisionHandle::disabled(
            "channel fixture",
        ));
        let server = DaemonServer::with_services(&path, svc, stop.clone());
        let mut run = Box::pin(async move { server.run().await });
        // As in conformance: bind precedes the first pending accept.
        poll_fn(|cx| {
            assert!(run.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        let task = tokio::spawn(run);
        let io = BufReader::new(
            timeout(CAGE, UnixStream::connect(path))
                .await
                .unwrap()
                .unwrap(),
        );
        Self {
            io,
            stop,
            task,
            control_task,
        }
    }

    async fn exchange(&mut self, request: Value) -> Value {
        timeout(CAGE, async {
            self.io
                .get_mut()
                .write_all(format!("{request}\n").as_bytes())
                .await
                .unwrap();
            let mut response = String::new();
            assert_ne!(self.io.read_line(&mut response).await.unwrap(), 0);
            serde_json::from_str(&response).unwrap()
        })
        .await
        .unwrap()
    }

    async fn connect(&mut self) -> Value {
        self.exchange(json!({"protocol_version":{"major":1,"minor":1},"type":"connect","client_id":"channel-wire"})).await
    }

    async fn close(self) {
        self.stop.cancel();
        timeout(CAGE, self.task).await.unwrap().unwrap().unwrap();
        timeout(CAGE, self.control_task).await.unwrap().unwrap();
    }
}

fn channels(value: &Value) -> &[Value] {
    value["channels"].as_array().unwrap()
}

fn channel<'a>(value: &'a Value, id: &str) -> &'a Value {
    channels(value).iter().find(|row| row["id"] == id).unwrap()
}

// Break: default all, wrong attribution, or flags copied from owner scope.
#[tokio::test]
async fn seat_list_defaults_to_membership_and_all_keeps_read_only_rows() {
    let rig = Rig::new(&sample()).await;
    let own = rig.list(Some("%2"), false).await;
    let ids: Vec<_> = channels(&own)
        .iter()
        .map(|row| row["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        [
            "alpha",
            "alpha/worker+beta/lead",
            "alpha/worker+owner",
            "alpha/worker+system"
        ]
    );
    let all = rig.list(Some("%2"), true).await;
    let beta = channel(&all, "beta");
    assert_eq!(beta["member"], false);
    assert_eq!(beta["writable"], false);
    assert_eq!(beta["last_post"], Value::Null);
    assert_eq!(beta["activity_seq"], 0);
    assert_eq!(all["seq"], 18);
    assert_eq!(rig.org.status().unwrap(), 18);
}

// Break: owner filtered by membership, system DMs exposed by default, or seq guessed from rows.
#[tokio::test]
async fn owner_list_includes_nonmember_dms_and_hides_seat_system_dms_by_default() {
    let rig = Rig::new(&sample()).await;
    let own = rig.list(None, false).await;
    let direct = channel(&own, "alpha/worker+beta/lead");
    assert_eq!(direct["member"], false);
    assert_eq!(direct["writable"], false);
    assert!(!channels(&own)
        .iter()
        .any(|row| row["id"] == "alpha/worker+system"));
    assert_eq!(
        channel(&rig.list(None, true).await, "alpha/worker+system")["section"],
        "system"
    );
    assert_eq!(
        channel(&own, "alpha"),
        &json!({
            "id":"alpha","kind":"project","project":"alpha","section":"channel",
            "members":[{"address":"owner","state":null},{"address":"alpha/lead","state":"ready"},{"address":"alpha/worker","state":"ready"}],
            "former":["alpha/former"],"lead":"alpha/lead","member":true,"writable":true,
            "last_post":"m16","last_activity":"2026-01-01T12:00:16Z","activity_seq":17,"unread":1,"seen_through":null
        })
    );
    assert_eq!(own["seq"], 18);
    assert_eq!(rig.org.status().unwrap(), 18);
}

// Break: special sections depend on the caller or label every project lead DM as a lead row.
#[tokio::test]
async fn sections_come_from_the_dm_pair_not_the_caller() {
    let mut facts = sample();
    facts.extend([
        adopt("alpha/chief", "%5", None),
        message("owner", "alpha/chief", "hello", "note", None),
        message("alpha/lead", "owner", "hello", "note", None),
    ]);
    let rig = Rig::new(&facts).await;
    let all = rig.list(Some("%1"), true).await;
    for (id, section) in [
        ("alpha/chief+owner", "chief"),
        ("alpha/lead+owner", "lead"),
        ("alpha/worker+system", "system"),
        ("alpha/worker+beta/lead", "direct"),
        ("alpha", "channel"),
    ] {
        assert_eq!(channel(&all, id)["section"], section, "{id}");
    }
}

/// A seat started from a role, shaped like a live `agent.up_requested`.
fn up(id: &str, skills: &[&str]) -> OrgEvent {
    let name = id.split_once('/').unwrap().1;
    fact(
        "agent.up_requested",
        json!({"agent_id":id,"role":{"name":name,"harness":"pi","model":null,"cwd":".","manager":null,
            "profile":null,"skills":skills,"spawn_args":[],"command_prefix":[],"pane":null,"clear":null,
            "trust_dirs":[],"instructions":""}}),
    )
}

fn lead_seats() -> Vec<OrgEvent> {
    let mut facts = sample();
    facts.extend([
        up("alpha/planner", &["atm-lead"]),
        up("alpha/builder", &["atm-member"]),
        up("alpha/chief", &["atm-lead"]),
        up("beta/chief", &["atm-lead"]),
        message("alpha/builder", "owner", "hello", "note", None),
        message("owner", "alpha/chief", "hello", "note", None),
        message("alpha/planner", "alpha/worker", "hello", "note", None),
    ]);
    facts
}

fn owner_channel_post(channel: &str, body: &str) -> OrgCommand {
    OrgCommand::Channel(ChannelCommand::Post(Box::new(PostRequest {
        target: PostTarget::Channel(ChannelId::from(channel.to_owned())),
        author: Address::Owner,
        kind: MessageKind::Note,
        body: body.into(),
        attachments: vec![],
        task: None,
        tags: vec![],
        groups: GroupDirectory::new(),
        decision: None,
        client_key: None,
        also_in_chat: false,
        no_route: false,
        internal: false,
    })))
}

// Break: a lead is found by name only, a role-less `lead` or a chief with the lead skill is
// misfiled, a builder or an agent-to-agent DM is promoted, or empty rows reach seats' lists.
#[tokio::test]
async fn every_lead_seat_is_a_lead_row_for_the_owner_even_before_a_dm() {
    let rig = Rig::new(&lead_seats()).await;
    let own = rig.list(None, false).await;
    for (id, section) in [
        ("alpha/planner+owner", "lead"),
        ("alpha/lead+owner", "lead"),
        ("beta/lead+owner", "lead"),
        ("alpha/chief+owner", "chief"),
        ("alpha/builder+owner", "direct"),
        ("alpha/worker+owner", "direct"),
        ("alpha/planner+alpha/worker", "direct"),
        ("alpha/worker+beta/lead", "direct"),
    ] {
        assert_eq!(channel(&own, id)["section"], section, "{id}");
    }
    assert_eq!(
        channel(&own, "alpha/planner+owner"),
        &json!({
            "id":"alpha/planner+owner","kind":"direct","project":null,"section":"lead",
            "members":[{"address":"alpha/planner","state":"starting"},{"address":"owner","state":null}],
            "former":[],"lead":null,"member":true,"writable":true,
            "last_post":null,"last_activity":null,"activity_seq":0,"unread":0,"seen_through":null
        })
    );
    let ids: Vec<&str> = channels(&own)
        .iter()
        .map(|row| row["id"].as_str().unwrap())
        .collect();
    // Only owner DMs with lead seats are added: not a chief's, a non-lead seat's, or a pair
    // of seats.
    for absent in [
        "beta/chief+owner",
        "alpha/former+owner",
        "alpha/worker+alpha/lead",
    ] {
        assert!(!ids.contains(&absent), "{absent}");
    }
    let directs: Vec<&str> = ids.iter().copied().filter(|id| id.contains('+')).collect();
    let mut sorted = directs.clone();
    sorted.sort_unstable();
    assert_eq!(directs, sorted, "direct rows stay in id order");
    let all = rig.list(None, true).await;
    for id in ["alpha/planner+owner", "alpha/lead+owner", "beta/lead+owner"] {
        assert_eq!(channel(&all, id), channel(&own, id), "{id}");
    }

    // Other readers get no empty DM row, and the sections they share agree.
    for pane in ["%1", "%2", "%4"] {
        for all in [false, true] {
            let seat = rig.list(Some(pane), all).await;
            for row in channels(&seat) {
                let id = row["id"].as_str().unwrap();
                if row["kind"] == "direct" {
                    assert_ne!(row["last_post"], Value::Null, "{pane} {all} {id}");
                }
                if let Some(mine) = channels(&own).iter().find(|r| r["id"] == id) {
                    assert_eq!(row["section"], mine["section"], "{pane} {all} {id}");
                }
            }
        }
    }
}

// Break: the empty row is a dead end (history errors, the owner's first post is refused or
// lands in a second row), or seen works on a channel id other than the listed one.
#[tokio::test]
async fn an_empty_lead_dm_reads_posts_and_marks_seen_as_one_channel() {
    let rig = Rig::new(&lead_seats()).await;
    let empty = history(&rig, json!({"channel":"alpha/planner+owner"})).await;
    assert_eq!(empty["posts"], json!([]));
    assert_eq!(empty["more"], false);
    let error = rig
        .call(
            None,
            "channel.seen",
            json!({"channel":"alpha/planner+owner","through":"m16"}),
        )
        .await
        .unwrap_err();
    assert_eq!(error.0.code, "not_in_channel");

    let base = rig.org.status().unwrap();
    let posted = rig
        .org
        .execute(
            owner_channel_post("alpha/planner+owner", "first"),
            at(base + 1),
        )
        .await
        .unwrap();
    assert_eq!(posted[0].kind, "channel.posted");
    let first = format!("m{}", posted[0].seq);

    let own = rig.list(None, false).await;
    let rows: Vec<&Value> = channels(&own)
        .iter()
        .filter(|row| row["id"] == "alpha/planner+owner")
        .collect();
    assert_eq!(rows.len(), 1, "one row after the first post");
    assert_eq!(rows[0]["section"], "lead");
    assert_eq!(rows[0]["last_post"], first.as_str());
    let page = history(&rig, json!({"channel":"alpha/planner+owner"})).await;
    let ids: Vec<&Value> = page["posts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| &p["id"])
        .collect();
    assert_eq!(ids, [&json!(first)]);

    let reply = seen(&rig, "alpha/planner+owner", &first).await;
    assert_eq!(reply["changed"], true);
    assert_eq!(reply["through"], first.as_str());
    let row = channel(&rig.list(None, false).await, "alpha/planner+owner").clone();
    assert_eq!(row["seen_through"], first.as_str());
    assert_eq!(row["unread"], 0);
}

// Break: an over-budget list is silently cut or written as an oversized success frame.
#[tokio::test]
async fn oversize_channel_list_is_refused_not_truncated() {
    let p = "project-with-a-long-id-for-rows";
    let mut facts = vec![project(p), adopt(&format!("{p}/lead"), "%1", None)];
    for n in 0..7000 {
        let name = format!("seat-{n:026}");
        facts.push(fact("agent.up_requested", json!({
            "agent_id":format!("{p}/{name}"),"role":{"name":name,"harness":"pi","instructions":""}
        })));
    }
    let rig = Rig::new(&facts).await;
    let mut wire = Wire::start(rig.dir.path(), services(&rig.org, rig.dir.path())).await;
    assert_eq!(wire.connect().await["type"], "connected");
    let actual = wire.exchange(json!({"protocol_version":{"major":1,"minor":1},"type":"request","id":"large-list","method":"channel.list","params":{}})).await;
    assert_eq!(
        actual,
        json!({"type":"response","id":"large-list","error":{"code":"refused","message":"channel.list result exceeds the 512 KiB reply limit"}})
    );
    assert_eq!(
        wire.exchange(json!({"protocol_version":{"major":1,"minor":1},"type":"ping","seq":19}))
            .await,
        json!({"type":"pong","seq":19})
    );
    wire.close().await;
    assert_eq!(rig.org.status().unwrap(), 7002);
}

fn ids(page: &Value) -> Vec<&str> {
    page["posts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|post| post["id"].as_str().unwrap())
        .collect()
}

async fn history(rig: &Rig, params: Value) -> Value {
    rig.call(None, "channel.history", params).await.unwrap()
}

// Break: a bound seat is denied another pair's channel, or history invents a legacy session/author.
#[tokio::test]
async fn legacy_direct_history_is_readable_by_a_nonmember() {
    let rig = Rig::new(&sample()).await;
    let page = rig
        .call(
            Some("%1"),
            "channel.history",
            json!({"channel":"alpha/worker+beta/lead"}),
        )
        .await
        .unwrap();
    assert_eq!(
        page,
        json!({"seq":18,"channel":"alpha/worker+beta/lead","posts":[{
        "id":"m11","channel":"alpha/worker+beta/lead","thread":null,"author":"alpha/worker",
        "session_id":null,"session_started":null,"kind":"note","body":"cross-project note",
        "attachments":[],"task":null,"mentions":[],"posted_at":"2026-01-01T12:00:11Z",
        "replies":0,"last_reply_at":null,"routing":"none","woke":["beta/lead"],"legacy":true,"decision":null
    }],"more":false})
    );
    assert_eq!(rig.org.status().unwrap(), 18);
}

// Break: a wake or reply uses its own ID as root; bounds are inclusive or ordered the wrong way.
#[tokio::test]
async fn history_threads_accept_post_reply_and_wake_ids_and_page_both_ways() {
    let rig = Rig::new(&sample()).await;
    for thread in ["m13", "m14", "m16", "m18"] {
        let page = history(&rig, json!({"channel":"alpha","thread":thread})).await;
        assert_eq!(ids(&page), ["m13", "m16"], "thread={thread}");
        assert_eq!(page["more"], false);
    }
    for (bounds, want, more) in [
        (json!({"limit":1}), vec!["m15"], true),
        (json!({"before":"m15","limit":1}), vec!["m13"], false),
        (json!({"thread":"m13","after":"m13"}), vec!["m16"], false),
        (json!({"before":"m15","after":"m15"}), vec![], false),
        (
            json!({"thread":"m13","before":"m13","after":"m16"}),
            vec![],
            false,
        ),
        (json!({"before":"m999"}), vec!["m15", "m13"], false),
        (json!({"after":"m14"}), vec!["m15"], false),
    ] {
        let mut params = bounds;
        params["channel"] = json!("alpha");
        let page = history(&rig, params).await;
        assert_eq!(ids(&page), want);
        assert_eq!(page["more"], more);
    }
}

// Break: leaking a shared row to legacy clients, losing its real thread/time,
// or paginating too early and hiding the shared row below an ordinary reply.
#[tokio::test]
async fn expanded_main_history_is_explicit_and_keeps_the_canonical_reply() {
    let mut facts = sample();
    facts.push(fact("channel.posted", json!({
        "channel":"alpha","author":"owner","kind":"note","body":"shared",
        "attachments":[],"thread":"m13","mentions":[],"wake":[],"route":false,"also_in_chat":true
    })));
    facts.push(owner_post("ordinary later reply", &[], Some("m13")));
    let rig = Rig::new(&facts).await;
    let plain = history(&rig, json!({"channel":"alpha"})).await;
    assert_eq!(ids(&plain), ["m15", "m13"]);
    let expanded = history(
        &rig,
        json!({"channel":"alpha","include_shared_replies":true,"limit":1}),
    )
    .await;
    assert_eq!(ids(&expanded), ["m19"]);
    assert_eq!(expanded["more"], true);
    let mut canonical = expanded["posts"][0].clone();
    assert_eq!(canonical["thread"], "m13");
    assert_eq!(canonical["posted_at"], "2026-01-01T12:00:19Z");
    assert_eq!(
        canonical.as_object_mut().unwrap().remove("also_in_chat"),
        Some(json!(true))
    );
    let thread = history(&rig, json!({"channel":"alpha","thread":"m19"})).await;
    assert_eq!(ids(&thread), ["m13", "m16", "m19", "m20"]);
    assert_eq!(thread["posts"][2], canonical);
    assert_eq!(thread["posts"][0]["replies"], 3);
    let error = rig
        .call(
            None,
            "channel.history",
            json!({
                "channel":"alpha","thread":"m13","include_shared_replies":true
            }),
        )
        .await
        .unwrap_err();
    assert_eq!(error.0.code, "invalid_params");
    assert_eq!(rig.org.status().unwrap(), 20);
}

// Break: silently hiding bad threads, or confusing an unknown post with one in another channel.
#[tokio::test]
async fn unknown_threads_and_cross_channel_roots_have_distinct_codes() {
    let rig = Rig::new(&sample()).await;
    for (channel, thread, code, message) in [
        ("alpha", "m1000", "not_found", "unknown post m1000"),
        (
            "beta",
            "m14",
            "not_in_channel",
            "thread m13 is not in channel beta",
        ),
    ] {
        let error = rig
            .call(
                None,
                "channel.history",
                json!({"channel":channel,"thread":thread}),
            )
            .await
            .unwrap_err();
        assert_eq!(error.0.code, code);
        assert_eq!(error.0.message, message);
    }
    assert_eq!(rig.org.status().unwrap(), 18);
}

// Break: untyped or lossy schemas, permissive coercion, or treating a malformed pane as owner.
#[tokio::test]
async fn channel_read_params_are_strict() {
    let rig = Rig::new(&sample()).await;
    for (method, params) in [
        ("channel.list", json!({"all":null})),
        ("channel.list", json!({"all":"true"})),
        ("channel.list", json!([])),
        ("channel.history", json!({})),
        ("channel.history", json!({"channel":"alpha","limit":null})),
        ("channel.history", json!({"channel":"alpha","thread":13})),
        (
            "channel.history",
            json!({"channel":"alpha","before":"m013"}),
        ),
        ("channel.seen", json!({"channel":"alpha"})),
        ("channel.seen", json!({"channel":"alpha","through":1})),
    ] {
        let error = rig.call(None, method, params.clone()).await.unwrap_err();
        assert_eq!(error.0.code, "invalid_params", "{method} {params}");
    }
    let error = rig
        .call(
            Some("not-a-pane"),
            "channel.history",
            json!({"channel":"alpha"}),
        )
        .await
        .unwrap_err();
    assert_eq!(error.0.code, "invalid_params");
    assert_eq!(rig.org.status().unwrap(), 18);
}

fn owner_post(body: &str, attachments: &[String], thread: Option<&str>) -> OrgEvent {
    fact(
        "channel.posted",
        json!({"channel":"alpha","author":"owner","kind":"note","body":body,
        "attachments":attachments,"thread":thread,"mentions":[],"wake":[],"route":false}),
    )
}

// Break: ignoring the caller's limit, wrong default, or an off-by-one more flag.
#[tokio::test]
async fn history_uses_default_50_and_max_200() {
    let mut facts = vec![project("alpha")];
    facts.extend((0..201).map(|_| owner_post("post", &[], None)));
    let rig = Rig::new(&facts).await;
    let default = history(&rig, json!({"channel":"alpha"})).await;
    assert_eq!(ids(&default).len(), 50);
    assert_eq!(default["more"], true);
    let max = history(&rig, json!({"channel":"alpha","limit":200})).await;
    let ids_at_max = ids(&max);
    assert_eq!(ids_at_max.len(), 200);
    assert_eq!(ids_at_max.first(), Some(&"m202"));
    assert_eq!(ids_at_max.last(), Some(&"m3"));
    assert_eq!(max["more"], true);
    let last = history(&rig, json!({"channel":"alpha","limit":200,"before":"m3"})).await;
    assert_eq!(ids(&last), ["m2"]);
    assert_eq!(last["more"], false);
}

// Break: projecting the current binding, wrong reply metadata, or serving a stale decision join.
#[tokio::test]
async fn native_history_joins_historical_session_and_decision() {
    let mut facts = sample();
    facts.push(fact(
        "agent.session_bound",
        json!({"agent_id":"alpha/lead","session_id":"s-new"}),
    ));
    let rig = Rig::new(&facts).await;
    let params = json!({"channel":"alpha","thread":"m13"});
    let page = history(&rig, params.clone()).await;
    assert_eq!(page["seq"], 19);
    assert_eq!(
        page["posts"][0],
        json!({
            "id":"m13","channel":"alpha","thread":null,"author":"alpha/lead",
            "session_id":"s-past","session_started":"2026-01-01T11:59:00Z","kind":"question","body":"Review @owner",
            "attachments":[],"task":null,"mentions":["owner"],"posted_at":"2026-01-01T12:00:13Z",
            "replies":1,"last_reply_at":"2026-01-01T12:00:16Z","routing":"none","woke":["owner","alpha/worker"],"legacy":false,
            "decision":{"decision_id":"d12","step":"asked","status":"pending","options":["go","hold"],"recommended":"go","deadline":null,"class":"detail","answer":null}
        })
    );
    let reply = &page["posts"][1];
    assert_eq!(reply["thread"], "m13");
    assert_eq!(reply["session_started"], Value::Null);
    assert_eq!(reply["routing"], "router");
    assert_eq!(reply["woke"], json!(["owner"]));
    drop(rig);

    for (event, body, status, answer) in [
        (
            "decision.resolved",
            json!({"decision_id":"d12","option":"go","by":"owner","note":"Go ahead","standing_rule":false}),
            "resolved",
            json!({"option":"go","by":"owner","note":"Go ahead","relayed_by":null}),
        ),
        (
            "decision.taken_provisionally",
            json!({"decision_id":"d12","option":"go","by":"owner","relayed_by":"alpha/worker"}),
            "taken_provisionally",
            json!({"option":"go","by":"owner","note":null,"relayed_by":"alpha/worker"}),
        ),
    ] {
        let mut updated = facts.clone();
        updated.push(fact(event, body));
        let rig = Rig::new(&updated).await;
        let next = history(&rig, params.clone()).await;
        let mut want = page["posts"][0].clone();
        want["decision"]["status"] = json!(status);
        want["decision"]["answer"] = answer;
        assert_eq!(next["seq"], 20);
        assert_eq!(next["posts"][0], want);
    }
}

// Break: leaking a later journal kind into the fixed v1 routing vocabulary or dropping an entire post.
#[tokio::test]
async fn history_projects_later_forms_without_breaking_the_v1_page() {
    let mut facts = vec![project("alpha")];
    for body in ["pending", "stopgap", "later"] {
        facts.push(fact(
            "channel.posted",
            json!({"channel":"alpha","author":"owner","kind":"note","body":body,
            "attachments":[],"mentions":["owner",{"later":"shape"}],"wake":[],"route":true}),
        ));
    }
    facts.extend([
        fact(
            "channel.routed",
            json!({"post":"m3","wake":[],"by":{"kind":"stopgap","cause":"disabled"}}),
        ),
        fact(
            "channel.routed",
            json!({"post":"m4","wake":[],"by":{"kind":"later"}}),
        ),
    ]);
    let rig = Rig::new(&facts).await;
    let page = history(&rig, json!({"channel":"alpha"})).await;
    assert_eq!(ids(&page), ["m4", "m3", "m2"]);
    for (row, (body, routing)) in page["posts"].as_array().unwrap().iter().zip([
        ("later", "none"),
        ("stopgap", "stopgap"),
        ("pending", "pending"),
    ]) {
        assert_eq!(row["body"], body);
        assert_eq!(row["mentions"], json!(["owner"]));
        assert_eq!(row["routing"], routing);
    }
}

// Break: sizing raw rather than encoded text, clipping a row, or losing it on the next page.
#[tokio::test]
async fn history_byte_cap_pages_escaped_rows_without_loss() {
    let body = "\u{1}".repeat(16 * 1024);
    let files = vec!["\u{2}".repeat(4096); 4];
    for (thread, page_ids, next_ids, next_params) in [
        (
            None,
            ["m5", "m4"],
            ["m3", "m2"],
            json!({"channel":"alpha","before":"m4"}),
        ),
        (
            Some("m2"),
            ["m2", "m3"],
            ["m4", "m5"],
            json!({"channel":"alpha","thread":"m2","after":"m3"}),
        ),
    ] {
        let mut facts = vec![project("alpha"), owner_post(&body, &files, None)];
        facts.extend((0..3).map(|_| owner_post(&body, &files, thread)));
        let rig = Rig::new(&facts).await;
        let first = history(&rig, json!({"channel":"alpha","thread":thread})).await;
        let second = history(&rig, next_params).await;
        assert_eq!(ids(&first), page_ids);
        assert_eq!(ids(&second), next_ids);
        assert_eq!(first["more"], true);
        assert_eq!(second["more"], false);
        for page in [first, second] {
            assert!(serde_json::to_vec(&page).unwrap().len() <= 512 * 1024);
            for post in page["posts"].as_array().unwrap() {
                assert_eq!(post["body"], body);
                assert_eq!(post["attachments"], json!(&files));
            }
        }
    }
    let files = vec!["\u{2}".repeat(4096); 32];
    let rig = Rig::new(&[project("alpha"), owner_post(&body, &files, None)]).await;
    let error = rig
        .call(None, "channel.history", json!({"channel":"alpha"}))
        .await
        .unwrap_err();
    assert_eq!(error.0.code, "refused");
    assert_eq!(
        error.0.message,
        "first post exceeds the 512 KiB history result limit"
    );
}

async fn mutate_as_owner(rig: &Rig, post: &str, change: PostMutation) {
    let seq = rig.org.status().unwrap();
    let events = rig
        .org
        .execute(
            OrgCommand::Channel(ChannelCommand::Mutate {
                post: post.parse().unwrap(),
                actor: Address::Owner,
                change,
            }),
            at(seq + 1),
        )
        .await
        .unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].seq, seq + 1);
}

// Break: sizing before the edit overlay, clipping an edited row, or advancing
// beyond the first row that didn't fit. One longer body moves the page boundary.
#[tokio::test]
async fn history_byte_cap_accounts_for_longer_edited_body() {
    let body = "\u{1}".repeat(16 * 1024);
    let mut facts = vec![project("alpha"), owner_post("short", &[], None)];
    facts.extend((0..5).map(|_| owner_post(&body, &[], None)));
    let rig = Rig::new(&facts).await;
    let params = json!({"channel":"alpha"});
    let before = history(&rig, params.clone()).await;
    assert_eq!(ids(&before), ["m7", "m6", "m5", "m4", "m3", "m2"]);
    assert_eq!(before["more"], false);
    mutate_as_owner(&rig, "m2", PostMutation::Edit(body.clone())).await;
    let first = history(&rig, params).await;
    assert_eq!(ids(&first), ["m7", "m6", "m5", "m4", "m3"]);
    assert_eq!(first["more"], true);
    let next = history(&rig, json!({"channel":"alpha","before":"m3"})).await;
    assert_eq!(ids(&next), ["m2"]);
    assert_eq!(next["more"], false);
    assert_eq!(next["posts"][0]["body"], body);
    assert_eq!(next["posts"][0]["edited"], true);
    for page in [first, next] {
        assert!(serde_json::to_vec(&page).unwrap().len() <= 512 * 1024);
    }
}

// Break: the author check alone admits a decision-linked post or the System
// author. Give each command the actual author to isolate the additional guard.
#[tokio::test]
async fn decision_and_system_posts_refuse_even_the_actual_author() {
    let mut facts = sample();
    facts.push(fact(
        "channel.posted",
        json!({"channel":"alpha","author":"system","kind":"note",
        "body":"system notice","attachments":[],"mentions":[],"wake":[],"route":false}),
    ));
    let rig = Rig::new(&facts).await;
    let state = rig.org.snapshot().await.unwrap();
    let seq = rig.org.status().unwrap();
    for (post, author) in [("m13", "alpha/lead"), ("m19", "system")] {
        for change in [
            PostMutation::Edit("replacement".into()),
            PostMutation::Remove,
        ] {
            let post = post.parse().unwrap();
            let command = OrgCommand::Channel(ChannelCommand::Mutate {
                post,
                actor: author.parse().unwrap(),
                change,
            });
            assert!(matches!(state.decide(&command, at(seq+1)),
                Err(Refusal::Channel(ChannelRefusal::PostMutation { post: denied, .. })) if denied == post));
        }
    }
}

async fn seen(rig: &Rig, channel: &str, through: &str) -> Value {
    rig.call(
        None,
        "channel.seen",
        json!({"channel":channel,"through":through}),
    )
    .await
    .unwrap()
}

// Break: advertised only when org is configured, protocol bump, or disabled mutation returns success.
#[tokio::test]
async fn advertises_channel_reads_at_protocol_1_1() {
    let dir = tempfile::tempdir().unwrap();
    let mut wire = Wire::start(
        dir.path(),
        services(&OrgHandle::disabled("channel fixture"), dir.path()),
    )
    .await;
    let connected = wire.connect().await;
    assert_eq!(connected["type"], "connected");
    assert_eq!(connected["protocol_version"], json!({"major":1,"minor":1}));
    let info = wire.exchange(json!({"protocol_version":{"major":1,"minor":1},"type":"request","id":"info","method":"daemon.info","params":{}})).await;
    assert_eq!(info["result"]["protocol"], "1.1");
    for (method, params) in [
        ("channel.list", json!({})),
        ("channel.history", json!({"channel":"alpha"})),
        ("channel.seen", json!({"channel":"alpha","through":"m1"})),
    ] {
        assert!(
            connected["methods"]
                .as_array()
                .unwrap()
                .contains(&json!(method)),
            "{method}"
        );
        assert!(
            info["result"]["methods"]
                .as_array()
                .unwrap()
                .contains(&json!(method)),
            "{method}"
        );
        let reply = wire.exchange(json!({"protocol_version":{"major":1,"minor":1},"type":"request","id":method,"method":method,"params":params})).await;
        assert_eq!(reply["error"]["code"], "unavailable", "{method}");
        assert!(reply.get("result").is_none());
        assert!(reply["error"].get("seq").is_none());
    }
    wire.close().await;
}

// Break: marks by row ID in place of post ID, doesn't persist the read, or returns the pre-commit cursor.
#[tokio::test]
async fn owner_seen_acks_by_post_in_one_command_and_returns_commit_seq() {
    let rig = Rig::new(&sample()).await;
    assert_eq!(channel(&rig.list(None, false).await, "alpha")["unread"], 1);
    let reply = seen(&rig, "alpha", "m16").await;
    assert_eq!(reply, json!({"seq":20,"changed":true,"through":"m16"}));
    let tail = rig
        .org
        .tail(TailParams {
            after_seq: 18,
            ..TailParams::default()
        })
        .await
        .unwrap();
    assert_eq!(
        tail.iter()
            .map(|event| (event.seq, event.kind.as_str()))
            .collect::<Vec<_>>(),
        [(19, "channel.seen"), (20, "msg.read")]
    );
    assert_eq!(
        tail[0].body,
        json!({"channel":"alpha","through":"m16","reader":"owner","session_id":null})
    );
    assert_eq!(tail[1].body, json!({"message_ids":["m18"]}));
    let state = rig.org.snapshot().await.unwrap();
    assert!(state
        .message(&MessageId::new(18))
        .unwrap()
        .read_at
        .is_some());
    assert!(state.message(&MessageId::new(9)).unwrap().read_at.is_none());
    let list = rig.list(None, false).await;
    assert_eq!(list["seq"], 20);
    assert_eq!(channel(&list, "alpha")["unread"], 0);
    assert_eq!(channel(&list, "alpha/worker+owner")["unread"], 1);
}

// Break: echoing an older cursor, appending on a no-op, or skipping U3 on a repeated view.
#[tokio::test]
async fn same_and_older_seen_return_the_effective_marker() {
    let rig = Rig::new(&sample()).await;
    seen(&rig, "alpha", "m16").await;
    for through in ["m16", "m13"] {
        assert_eq!(
            seen(&rig, "alpha", through).await,
            json!({"seq":20,"changed":false,"through":"m16"})
        );
    }
    assert_eq!(rig.org.status().unwrap(), 20);
    let OrgEvent::MessageSent(content) = message(
        "alpha/lead",
        "owner",
        "Review @owner",
        "question",
        Some("m13"),
    ) else {
        panic!("expected a message")
    };
    let late = rig
        .org
        .execute(OrgCommand::SendMessage(content), at(21))
        .await
        .unwrap();
    assert_eq!(late[0].seq, 21);
    let before = rig.org.snapshot().await.unwrap();
    assert!(before
        .message(&MessageId::new(21))
        .unwrap()
        .read_at
        .is_none());
    assert_eq!(
        seen(&rig, "alpha", "m16").await,
        json!({"seq":22,"changed":false,"through":"m16"})
    );
    let tail = rig
        .org
        .tail(TailParams {
            after_seq: 21,
            ..TailParams::default()
        })
        .await
        .unwrap();
    assert_eq!(tail.len(), 1);
    assert_eq!(tail[0].kind, "msg.read");
    assert_eq!(tail[0].body, json!({"message_ids":["m21"]}));
    assert!(rig
        .org
        .snapshot()
        .await
        .unwrap()
        .message(&MessageId::new(21))
        .unwrap()
        .read_at
        .is_some());
}

// Break: edit/remove moves the seen prefix or stops counting later posts as unread.
#[tokio::test]
async fn seen_prefix_survives_edit_then_remove_of_its_anchor() {
    let rig = Rig::new(&sample()).await;
    assert_eq!(seen(&rig, "alpha", "m15").await["through"], "m15");
    let before = rig.list(None, false).await;
    let row = channel(&before, "alpha");
    assert_eq!(row["seen_through"], "m15");
    assert_eq!(row["unread"], 1);
    for (change, body, flag) in [
        (
            PostMutation::Edit("replacement".into()),
            "replacement",
            "edited",
        ),
        (PostMutation::Remove, "removed", "removed"),
    ] {
        mutate_as_owner(&rig, "m15", change).await;
        assert_eq!(channel(&rig.list(None, false).await, "alpha"), row);
        let page = history(&rig, json!({"channel":"alpha","before":"m16","limit":1})).await;
        assert_eq!(ids(&page), ["m15"]);
        assert_eq!(page["posts"][0]["body"], body);
        assert_eq!(page["posts"][0][flag], true);
    }
    assert_eq!(seen(&rig, "alpha", "m15").await["changed"], false);
}

// Break: seat writes a marker, wake IDs are accepted, a foreign post writes, or malformed panes get owner scope.
#[tokio::test]
async fn seat_seen_and_invalid_through_write_nothing() {
    let rig = Rig::new(&sample()).await;
    for (pane, id, code) in [
        (Some("%2"), "m16", "refused"),
        (Some("not-a-pane"), "m16", "invalid_params"),
        (None, "m18", "not_in_channel"),
        (None, "m11", "not_in_channel"),
    ] {
        let error = rig
            .call(
                pane,
                "channel.seen",
                json!({"channel":"alpha","through":id}),
            )
            .await
            .unwrap_err();
        assert_eq!(error.0.code, code);
        if code == "refused" {
            assert_eq!(error.0.message, "only the owner may mark a channel seen");
        }
    }
    assert_eq!(rig.org.status().unwrap(), 18);
    assert_eq!(
        channel(&rig.list(None, false).await, "alpha")["seen_through"],
        Value::Null
    );
    assert!(rig
        .org
        .snapshot()
        .await
        .unwrap()
        .message(&MessageId::new(18))
        .unwrap()
        .read_at
        .is_none());
}

// Break: still counting inbox rows after a marker, counting the owner's own post, or ignoring the new marker.
#[tokio::test]
async fn marker_changes_unread_basis() {
    let rig = Rig::new(&sample()).await;
    rig.org
        .execute(
            OrgCommand::ReadMessages {
                who: Address::Owner,
                message_ids: vec![MessageId::new(18)],
            },
            at(19),
        )
        .await
        .unwrap();
    assert_eq!(channel(&rig.list(None, false).await, "alpha")["unread"], 0);
    assert_eq!(
        seen(&rig, "alpha", "m13").await,
        json!({"seq":20,"changed":true,"through":"m13"})
    );
    let list = rig.list(None, false).await;
    assert_eq!(channel(&list, "alpha")["unread"], 1);
    assert_eq!(channel(&list, "alpha")["seen_through"], "m13");
}

// Break: reading back through the owner default list, which intentionally omits this channel.
#[tokio::test]
async fn owner_can_repeat_seen_in_a_hidden_system_dm() {
    let rig = Rig::new(&sample()).await;
    let id = "alpha/worker+system";
    assert_eq!(
        seen(&rig, id, "m10").await,
        json!({"seq":19,"changed":true,"through":"m10"})
    );
    assert_eq!(
        seen(&rig, id, "m10").await,
        json!({"seq":19,"changed":false,"through":"m10"})
    );
    assert!(!channels(&rig.list(None, false).await)
        .iter()
        .any(|row| row["id"] == id));
    assert_eq!(
        channel(&rig.list(None, true).await, id)["seen_through"],
        "m10"
    );
}

// Break: joining context in the shared row projector changes msg.history too.
#[tokio::test]
async fn message_history_keeps_its_old_projection_on_a_wake() {
    let rig = Rig::new(&sample()).await;
    let history = rig.call(None, "msg.history", json!({})).await.unwrap();
    let row = history["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["message_id"] == "m14")
        .unwrap();
    assert_eq!(row["content"]["post"], "m13");
    assert!(row.get("post_context").is_none());
    assert_eq!(rig.org.status().unwrap(), 18);
}
