//! Router request and reply contracts inside the existing request envelope.
use atm_comms::channel_types::{
    ChannelRouteParams, ChannelRouteResult, RouterAttachParams, RouterAttachResult,
};
use serde_json::{json, Value};

// Break: accepting a hybrid, an absent answer, or caller-supplied metadata.
#[test]
fn router_wire_has_two_exclusive_closed_answer_forms() {
    for valid in [
        json!({"post":"m42","wake":[]}),
        json!({"post":"m42","wake":["owner"],"reason":"asked"}),
        json!({"post":"m42","decline":"model down"}),
    ] {
        let p: ChannelRouteParams = serde_json::from_value(valid.clone()).unwrap();
        assert_eq!(serde_json::to_value(p).unwrap(), valid);
    }
    for invalid in [
        json!({"post":"m42"}),
        json!({"post":"m42","wake":[],"decline":"model down"}),
        json!({"post":"m42","wake":[],"decline":null}),
        json!({"post":"m42","wake":null}),
        json!({"post":"m42","decline":null}),
        json!({"post":"m42","decline":"down","reason":"why"}),
        json!({"post":"m42","wake":[],"by":{"kind":"stopgap","cause":"timeout"}}),
        json!({"post":"m42","wake":[],"woke":[]}),
        json!(["m42", []]),
    ] {
        assert!(
            serde_json::from_value::<ChannelRouteParams>(invalid.clone()).is_err(),
            "{invalid}"
        );
    }
    assert!(serde_json::from_value::<RouterAttachParams>(
        json!({"name":"r","model":"local","scope":"owner"})
    )
    .is_err());
}

// Break: making pending catch-up cursorless or returning post IDs as wake IDs.
#[test]
fn router_wire_preserves_pending_deadline_cursor_and_wake_ids() {
    fn round_trip<T: serde::de::DeserializeOwned + serde::Serialize>(wire: Value) {
        let typed: T = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(serde_json::to_value(typed).unwrap(), wire);
    }
    round_trip::<RouterAttachParams>(json!({"name":"r","model":"local"}));
    round_trip::<RouterAttachResult>(json!({
        "seq": 44,
        "timeout_secs": 20,
        "pending":[{"post":"m42","channel":"demo","deadline":"2026-01-01T00:00:20Z"}]
    }));
    round_trip::<ChannelRouteResult>(json!({"seq":45,"woke":[{"to":"owner","message_id":"m45"}]}));
}

#[path = "../../../../tests/support/fake_router.rs"]
mod fake_router;

use crate::eventlog::{Envelope, EventLogError};
use crate::org::{
    actor,
    channel_post::spawn_wake_repair,
    channel_post_tests::{disk_org, restart, setup},
    channel_route::RouteHandle,
    messages::MessageClock,
    storage::{Page, Reader},
    watch::ReadAccess,
    OrgHandle,
};
use crate::server::{
    methods::{router_with_routes, MethodServices},
    ConnectionHandler, SubscribersMap,
};
use atm_org::{
    channel_policy::RouterSettings,
    channel_query::{pending_routes, post, wake_for},
    Address, ChannelCommand, ChannelId, DeliveryStatus, MessageId, MessageKind, OrgCommand,
    OrgEvent, OrgState, PostRequest, PostRouted, PostTarget, RouteBy,
};
use chrono::{DateTime, TimeDelta, Utc};
use fake_router::FakeRouter;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::DuplexStream,
    sync::{broadcast, mpsc, oneshot, watch, RwLock},
    task::JoinHandle,
    time::{advance, timeout, Instant},
};
use tokio_util::sync::CancellationToken;

fn utc() -> DateTime<Utc> {
    DateTime::from_timestamp(1_700_000_000, 0).unwrap()
}
fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}
fn enabled() -> RouterSettings {
    RouterSettings {
        enabled: true,
        ..RouterSettings::default()
    }
}
fn addr(value: &str) -> Address {
    serde_json::from_value(json!(value)).unwrap()
}
fn demo() -> ChannelId {
    ChannelId::Project("demo".parse().unwrap())
}
fn top() -> PostTarget {
    PostTarget::Channel(demo())
}
fn make_post(author: Address, target: PostTarget, body: &str) -> Box<PostRequest> {
    Box::new(PostRequest {
        target,
        author,
        body: body.into(),
        kind: MessageKind::Note,
        attachments: vec!["example.md".into()],
        task: None,
        tags: vec![],
        groups: Default::default(),
        decision: None,
        client_key: None,
        also_in_chat: false,
        no_route: false,
        internal: false,
    })
}
async fn write(org: &OrgHandle, at: DateTime<Utc>, req: Box<PostRequest>) -> MessageId {
    MessageId::new(
        org.execute(OrgCommand::Channel(ChannelCommand::Post(req)), at)
            .await
            .unwrap()[0]
            .seq,
    )
}
fn wake(post: MessageId, recipients: &[&str]) -> ChannelRouteParams {
    ChannelRouteParams::Wake {
        post,
        wake: recipients.iter().map(|to| addr(to)).collect(),
        reason: Some("scripted choice".into()),
    }
}
fn decline(post: MessageId) -> ChannelRouteParams {
    ChannelRouteParams::Decline {
        post,
        decline: "not journaled".into(),
    }
}

struct Rig {
    org: OrgHandle,
    routes: RouteHandle,
    clock: MessageClock,
    origin: Instant,
    stored: Arc<Mutex<Vec<Envelope>>>,
    stop: CancellationToken,
}
impl Drop for Rig {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

const RECORD_CAPACITY: usize = 8;

impl Rig {
    async fn new(settings: RouterSettings, fail_first_route: bool) -> Self {
        Self::with_first_route_error(
            settings,
            fail_first_route.then_some(EventLogError::TimeoutBeforeHandover),
        )
        .await
    }

    async fn with_first_route_error(
        settings: RouterSettings,
        mut fail: Option<EventLogError>,
    ) -> Self {
        let origin = Instant::now();
        let clock = MessageClock::new(origin, utc());
        let (requests, input) = mpsc::channel(64);
        let (health, status) = watch::channel(Ok(0));
        let (committed, _) = broadcast::channel(RECORD_CAPACITY);
        let stop = CancellationToken::new();
        let stored = Arc::new(Mutex::new(Vec::new()));
        let store = stored.clone();
        tokio::spawn(actor::run(
            Default::default(),
            input,
            health,
            committed.clone(),
            stop.clone(),
            move |mut e: Envelope| {
                if e.kind == "channel.routed" {
                    if let Some(error) = fail.take() {
                        return std::future::ready(Err(error));
                    }
                }
                let result = {
                    let mut log = store.lock().unwrap();
                    e.seq = log.len() as u64 + 1;
                    let seq = e.seq;
                    log.push(e);
                    Ok(seq)
                };
                std::future::ready(result)
            },
        ));
        let store = stored.clone();
        let reader = Reader::in_memory_for_test(move |after, fence, topics| {
            Ok(Page {
                scanned: fence,
                records: store
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|e| {
                        e.seq > after
                            && e.seq <= fence
                            && crate::org::watch::matches(topics, &e.kind)
                    })
                    .cloned()
                    .collect(),
            })
        });
        let org = OrgHandle {
            requests,
            health: status,
            committed,
            reads: Some(ReadAccess { reader }),
        };
        for command in setup() {
            org.execute(command, utc()).await.unwrap();
        }
        let routes = RouteHandle::new(org.clone(), settings, clock.clone(), utc());
        Self {
            org,
            routes,
            clock,
            origin,
            stored,
            stop,
        }
    }
    async fn post(&self, author: &str, target: PostTarget, body: &str) -> MessageId {
        write(
            &self.org,
            self.clock.now().unwrap(),
            make_post(addr(author), target, body),
        )
        .await
    }
    async fn down(&self, agent: &str) {
        self.org
            .execute(
                OrgCommand::Down {
                    agent_id: agent.parse().unwrap(),
                    reason: "fixture".into(),
                },
                self.clock.now().unwrap(),
            )
            .await
            .unwrap();
    }
    async fn connect(&self) -> FakeRouter<DuplexStream> {
        connect(&self.org, self.routes.clone()).await
    }
    fn timer(&self) -> Timer {
        let routes = self.routes.clone();
        let stop = self.stop.child_token();
        let cancel = stop.clone();
        let (send, parked) = watch::channel(None);
        let run = tokio::spawn(async move {
            routes
                .run(stop, |seq, until| {
                    send.send_replace(Some((seq, until)));
                })
                .await;
        });
        Timer {
            parked,
            run,
            cancel,
        }
    }
    async fn state(&self) -> atm_org::OrgState {
        self.org.snapshot().await.unwrap()
    }
    fn by(&self, post: MessageId) -> Vec<Envelope> {
        let p = json!(post);
        self.stored
            .lock()
            .unwrap()
            .iter()
            .filter(|e| (e.seq == post.seq() && e.kind == "channel.posted") || e.body["post"] == p)
            .cloned()
            .collect()
    }
    fn assert_pending(&self, post: MessageId) {
        assert_eq!(self.by(post).len(), 1, "{post} was routed early");
    }
    fn check(&self, post: MessageId, by: Value, wake: &[&str], reason: Value) -> Vec<Envelope> {
        let events = self.by(post);
        let names: Vec<_> = events.iter().map(|e| e.kind.as_str()).collect();
        let mut expected = vec!["channel.posted", "channel.routed"];
        expected.extend(std::iter::repeat_n("msg.sent", wake.len()));
        assert_eq!(names, expected, "{post}: {events:?}");
        assert_eq!(
            events[1].body,
            json!({"post":post, "by":by, "wake":wake,"reason":reason})
        );
        for (row, to) in events.iter().skip(2).zip(wake) {
            assert_eq!(&row.body["to"], to);
            assert_eq!(row.body["from"], events[0].body["author"]);
            for field in ["kind", "body", "attachments"] {
                assert_eq!(row.body[field], events[0].body[field]);
            }
        }
        events
    }
}

async fn connect(org: &OrgHandle, routes: RouteHandle) -> FakeRouter<DuplexStream> {
    let (stream, peer) = tokio::io::duplex(8192);
    let (read, write) = tokio::io::split(stream);
    let registry = crate::registry::spawn_registry();
    let mut services = MethodServices::new(registry.clone());
    services.org = org.clone();
    let methods = Arc::new(router_with_routes(services, routes).unwrap());
    let subscribers: SubscribersMap<_> = Arc::new(RwLock::new(HashMap::new()));
    let handler = ConnectionHandler::new(
        read,
        write,
        registry,
        subscribers,
        1,
        CancellationToken::new(),
        methods,
    )
    .with_org(org.clone());
    tokio::spawn(handler.run());
    FakeRouter::connect(peer, "same-client-label").await
}

struct Timer {
    parked: watch::Receiver<Option<(u64, Option<Instant>)>>,
    run: JoinHandle<()>,
    cancel: CancellationToken,
}
impl Timer {
    async fn at(&mut self, since: u64, until: Option<Instant>) {
        timeout(
            secs(60),
            self.parked
                .wait_for(|s| s.is_some_and(|(seq, due)| seq >= since && due == until)),
        )
        .await
        .unwrap()
        .unwrap();
    }
    async fn stop(self) {
        self.cancel.cancel();
        self.run.await.unwrap();
    }
}
fn stopgap(cause: &str) -> Value {
    json!({"kind":"stopgap","cause":cause})
}

// Break: disabled routes wait for grace, bypass the stopgap policy, or write
// a row without first committing its decision (including owner wakes).
#[tokio::test(start_paused = true)]
async fn channel_route_disabled_runs_all_stopgap_rules_without_clock_advance() {
    let r = Rig::new(RouterSettings::default(), false).await;
    let mut timer = r.timer();
    assert_eq!(
        r.connect().await.attach().await.unwrap_err().code,
        "refused"
    );
    let mut check = async |author, target, to: &[&str]| {
        let p = r.post(author, target, "untagged").await;
        timer.at(p.seq(), None).await;
        for row in r.check(p, stopgap("disabled"), to, Value::Null) {
            assert_eq!(row.ts, utc());
        }
        p
    };
    let root = check("owner", top(), &["demo/lead"]).await;
    let worker = check("demo/worker", top(), &["demo/lead"]).await;
    let lead = check("demo/lead", top(), &["demo/other", "demo/worker"]).await;
    let owner_wake = check("demo/lead", PostTarget::Reply(root), &["owner"]).await;
    check("demo/other", PostTarget::Reply(lead), &["demo/lead"]).await;
    check("demo/lead", PostTarget::Reply(lead), &["demo/other"]).await; // latest earlier replier
    check("demo/worker", PostTarget::Reply(worker), &["demo/lead"]).await; // sole root wake
    let owner_id = wake_for(&r.state().await, owner_wake, &Address::Owner).unwrap();
    assert_eq!(
        r.state().await.message(&owner_id).unwrap().status,
        DeliveryStatus::Delivered
    );
    let dm = ChannelId::direct(Address::Owner, addr("demo/worker")).unwrap();
    r.post(
        "owner",
        PostTarget::Direct(addr("demo/worker")),
        "seed the DM",
    )
    .await;
    check("demo/worker", PostTarget::Channel(dm.clone()), &["owner"]).await;
    r.down("demo/worker").await;
    check("owner", PostTarget::Channel(dm), &["demo/worker"]).await;
    r.down("demo/lead").await;
    check("owner", top(), &[]).await;
    check("owner", PostTarget::Reply(root), &[]).await; // earlier replier and sole root target are down
    timer.stop().await;
}

#[tokio::test(start_paused = true)]
async fn channel_route_explicit_recipients_and_no_route_never_enter_the_queue() {
    let r = Rig::new(RouterSettings::default(), false).await;
    let mut timer = r.timer();
    let cases = [
        make_post(
            Address::Owner,
            PostTarget::Direct(addr("demo/worker")),
            "direct",
        ),
        Box::new(PostRequest {
            tags: vec!["worker".into()],
            ..*make_post(Address::Owner, top(), "tagged")
        }),
        make_post(Address::System, top(), "internal notice"),
        Box::new(PostRequest {
            no_route: true,
            ..*make_post(Address::Owner, top(), "no route")
        }),
    ];
    let mut ids = Vec::new();
    for request in cases {
        ids.push(write(&r.org, utc(), request).await);
    }
    let p = r.post("owner", top(), "fence").await;
    timer.at(p.seq(), None).await;
    for (id, expected) in ids.into_iter().zip([
        json!(["demo/worker"]),
        json!(["demo/worker"]),
        json!([]),
        json!([]),
    ]) {
        let e = r.by(id);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].body["route"], false);
        assert_eq!(e[0].body["wake"], expected);
    }
    timer.stop().await;
}

#[tokio::test(start_paused = true)]
async fn channel_route_fake_router_replays_and_answers_once() {
    let r = Rig::new(enabled(), false).await;
    let old = write(
        &r.org,
        utc() - TimeDelta::seconds(40),
        make_post(Address::Owner, top(), "old"),
    )
    .await;
    let mut timer = r.timer();
    timer.at(old.seq(), Some(r.origin + secs(20))).await;
    advance(secs(2)).await;
    let mut fake = r.connect().await;
    let lease = fake.attach().await.unwrap();
    assert_eq!(lease.seq, old.seq());
    assert_eq!(lease.pending.len(), 1);
    assert_eq!(lease.pending[0].post, old);
    assert_eq!(lease.pending[0].channel, demo());
    assert_eq!(lease.pending[0].deadline, utc() + TimeDelta::seconds(22));
    let live = r.post("owner", top(), "route this").await;
    fake.watch_after(lease.seq).await;
    assert_eq!(fake.next_pending(|body| body == "route this").await, live);
    let (release, wait) = oneshot::channel();
    release.send(()).unwrap();
    let a = fake
        .answer_when_released(wake(live, &["demo/worker", "demo/other"]), wait)
        .await
        .unwrap();
    let events = r.check(
        live,
        json!({"kind":"router","name":"fake","model":"script"}),
        &["demo/worker", "demo/other"],
        json!("scripted choice"),
    );
    assert_eq!(
        a.woke
            .iter()
            .map(|w| w.message_id.seq())
            .collect::<Vec<_>>(),
        vec![events[2].seq, events[3].seq]
    );
    let again = fake
        .call(
            "channel.route",
            json!({"post":live,"wake":["demo/other","demo/worker"],"reason":"worded differently"}),
            None,
        )
        .await
        .unwrap();
    assert_eq!(again, serde_json::to_value(a).unwrap());
    assert_eq!(
        fake.answer(wake(live, &["demo/lead"]))
            .await
            .unwrap_err()
            .code,
        "conflict"
    );
    assert!(fake.answer(wake(old, &[])).await.unwrap().woke.is_empty());
    assert_eq!(
        fake.answer(wake(old, &["demo/lead"]))
            .await
            .unwrap_err()
            .code,
        "conflict"
    );
    assert_eq!(r.by(old).len(), 2);
    timer.stop().await;
}

#[tokio::test(start_paused = true)]
async fn channel_route_withheld_router_times_out_and_late_answer_conflicts() {
    let r = Rig::new(enabled(), false).await;
    let mut timer = r.timer();
    let mut fake = r.connect().await;
    let seq = fake.attach().await.unwrap().seq;
    fake.watch_after(seq).await;
    let p = r.post("owner", top(), "withheld").await;
    let (seen, held) = oneshot::channel();
    let (release, gate) = oneshot::channel();
    let answer = tokio::spawn(async move {
        let p = fake.next_pending(|b| b == "withheld").await;
        seen.send(p).unwrap();
        fake.answer_when_released(wake(p, &["demo/worker"]), gate)
            .await
    });
    assert_eq!(held.await.unwrap(), p);
    timer.at(p.seq(), Some(r.origin + secs(20))).await;
    advance(secs(19)).await;
    r.assert_pending(p);
    advance(secs(1)).await;
    timer.at(p.seq() + 2, None).await;
    let events = r.check(p, stopgap("timeout"), &["demo/lead"], Value::Null);
    assert_eq!(events[1].ts, utc() + TimeDelta::seconds(20));
    let before = r.org.status().unwrap();
    release.send(()).unwrap();
    assert_eq!(answer.await.unwrap().unwrap_err().code, "conflict");
    assert_eq!(r.org.status().unwrap(), before);
    timer.stop().await;
}

#[tokio::test(start_paused = true)]
async fn channel_route_at_deadline_conflicts_even_before_the_timer_runs() {
    let r = Rig::new(enabled(), false).await;
    let mut c = r.connect().await;
    c.attach().await.unwrap();
    let p = r.post("owner", top(), "deadline tie").await;
    advance(secs(20)).await; // worker not spawned yet
    for answer in [wake(p, &[]), decline(p)] {
        assert_eq!(c.answer(answer).await.unwrap_err().code, "conflict");
        assert_eq!(r.org.status().unwrap(), p.seq());
    }
    let mut timer = r.timer();
    timer.at(p.seq() + 2, None).await;
    r.check(p, stopgap("timeout"), &["demo/lead"], Value::Null);
    timer.stop().await;
}

#[tokio::test(start_paused = true)]
async fn channel_route_scope_is_the_socket_not_the_client_label() {
    let r = Rig::new(enabled(), false).await;
    let mut a = r.connect().await;
    let mut b = r.connect().await;
    let params = json!({"name":"fake","model":"script"});
    let p = r.post("owner", top(), "scope").await;
    assert_eq!(
        a.call("router.attach", params.clone(), Some("%1"))
            .await
            .unwrap_err()
            .code,
        "refused"
    );
    a.call("router.attach", params, Some("%999")).await.unwrap(); // valid unbound pane is owner scope
    assert_eq!(a.attach().await.unwrap_err().code, "refused");
    assert_eq!(b.attach().await.unwrap_err().code, "refused");
    assert_eq!(b.answer(wake(p, &[])).await.unwrap_err().code, "refused");
    for (pane, code) in [("%1", "refused"), ("invalid", "invalid_params")] {
        assert_eq!(
            a.call("channel.route", json!({"post":p,"wake":[]}), Some(pane))
                .await
                .unwrap_err()
                .code,
            code
        );
    }
    assert_eq!(r.org.status().unwrap(), p.seq());
    a.disconnect().await;
    assert_eq!(b.attach().await.unwrap().pending[0].post, p);
}

#[tokio::test(start_paused = true)]
async fn channel_route_detach_uses_close_time_and_reattach_gets_a_fresh_deadline() {
    for reattach in [false, true] {
        let r = Rig::new(enabled(), false).await;
        let mut a = r.connect().await;
        a.attach().await.unwrap();
        let p = r.post("owner", top(), "pending").await;
        advance(secs(5)).await; // T = 5; old deadline = 20
        a.disconnect().await;
        advance(secs(4)).await; // worker first observes the close late
        let mut timer = r.timer();
        timer.at(p.seq(), Some(r.origin + secs(25))).await;
        advance(secs(15)).await;
        r.assert_pending(p);
        if reattach {
            let mut b = r.connect().await;
            let a = b.attach().await.unwrap();
            assert_eq!(a.pending[0].post, p);
            assert_eq!(a.pending[0].deadline, utc() + TimeDelta::seconds(44));
            timer.at(p.seq(), Some(r.origin + secs(44))).await;
            advance(secs(1)).await;
            r.assert_pending(p);
            advance(secs(19)).await;
            timer.at(p.seq() + 2, None).await;
            r.check(p, stopgap("timeout"), &["demo/lead"], Value::Null);
        } else {
            advance(secs(1)).await;
            timer.at(p.seq() + 2, None).await;
            r.check(p, stopgap("no_router"), &["demo/lead"], Value::Null);
        }
        timer.stop().await;
    }
}

#[tokio::test(start_paused = true)]
async fn channel_route_startup_grace_rechecks_old_pending_posts() {
    let r = Rig::new(enabled(), false).await;
    let p = write(
        &r.org,
        utc() - TimeDelta::seconds(50),
        make_post(Address::Owner, top(), "replay"),
    )
    .await;
    let mut timer = r.timer();
    timer.at(p.seq(), Some(r.origin + secs(20))).await;
    advance(secs(19)).await;
    r.assert_pending(p);
    advance(secs(1)).await;
    timer.at(p.seq() + 2, None).await;
    r.check(p, stopgap("no_router"), &["demo/lead"], Value::Null);
    let later = r.post("owner", top(), "after grace").await;
    timer.at(later.seq() + 2, None).await;
    r.check(later, stopgap("no_router"), &["demo/lead"], Value::Null);
    assert_eq!(Instant::now(), r.origin + secs(20));
    timer.stop().await;
}

#[tokio::test(start_paused = true)]
async fn channel_route_decline_and_repeated_decline_do_not_rerun_policy() {
    let r = Rig::new(enabled(), false).await;
    let mut c = r.connect().await;
    c.attach().await.unwrap();
    let p = r.post("owner", top(), "decline").await;
    let first = c.answer(decline(p)).await.unwrap();
    let rows = r.check(p, stopgap("declined"), &["demo/lead"], Value::Null);
    assert_eq!(first.woke[0].message_id.seq(), rows[2].seq);
    r.down("demo/lead").await;
    let seq = r.org.status().unwrap();
    let repeat = c.answer(decline(p)).await.unwrap();
    assert_eq!(repeat.seq, seq);
    assert_eq!(repeat.woke[0].message_id, first.woke[0].message_id);
    assert_eq!(
        c.answer(wake(p, &["demo/lead"])).await.unwrap_err().code,
        "conflict"
    );
    assert_eq!(r.org.status().unwrap(), seq);
}

#[tokio::test(start_paused = true)]
async fn channel_route_invalid_answer_leaves_the_deadline_armed() {
    let r = Rig::new(enabled(), false).await;
    r.down("demo/other").await;
    let mut timer = r.timer();
    let mut c = r.connect().await;
    c.attach().await.unwrap();
    let addressed = r
        .post("owner", PostTarget::Direct(addr("demo/worker")), "tagged")
        .await;
    let p = r.post("owner", top(), "valid eventually").await;
    let denied = r.post("owner", top(), "times out").await;
    let before = r.org.status().unwrap();
    for params in [
        json!({"post":p,"wake":["demo/other"]}),
        json!({"post":p,"wake":["outside/member"]}),
        json!({"post":p,"wake":["owner"]}),
        json!({"post":p,"wake":["system"]}),
        json!({"post":"m99999","wake":[]}),
        json!({"post":addressed,"wake":[]}),
        json!({"post":p,"wake":[],"reason":"é".repeat(129)}),
        json!({"post":p,"wake":[],"reason":"two\nlines"}),
    ] {
        assert_eq!(
            c.call("channel.route", params.clone(), None)
                .await
                .unwrap_err()
                .code,
            "invalid_params",
            "{params}"
        );
        assert_eq!(r.org.status().unwrap(), before);
    }
    c.answer(wake(p, &["demo/worker"])).await.unwrap();
    // A wake row is not a timer invalidation. Fence on the routing
    // decision, which the snapshot may see before the inline row.
    timer.at(denied.seq() + 1, Some(r.origin + secs(20))).await;
    advance(secs(20)).await;
    timer.at(denied.seq() + 4, None).await;
    r.check(denied, stopgap("timeout"), &["demo/lead"], Value::Null);
    timer.stop().await;
}

#[tokio::test(start_paused = true)]
async fn channel_route_refused_stopgap_commits_empty_once() {
    let r = Rig::new(RouterSettings::default(), false).await;
    let p = r.post("owner", top(), "stale snapshot").await;
    let stale = r.state().await;
    assert_eq!(
        atm_org::channel_policy::stopgap_wake(&stale, p),
        [addr("demo/lead")]
    );
    r.down("demo/lead").await;
    assert!(r
        .routes
        .commit_stopgap(&stale, p, utc(), "disabled")
        .await
        .unwrap());
    assert!(pending_routes(&r.state().await).next().is_none());
    let mut timer = r.timer();
    timer.at(r.org.status().unwrap(), None).await;
    r.check(p, stopgap("disabled"), &[], Value::Null);
    timer.stop().await;
}

#[tokio::test]
async fn channel_route_restart_repairs_only_missing_wakes_and_heals_owner() {
    for owner_committed in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        // Setup (4), P, owner marker, R, then rows; fail first or second row.
        let (org, stop) = disk_org(dir.path(), if owner_committed { 9 } else { 8 }).await;
        for command in setup() {
            org.execute(command, utc()).await.unwrap();
        }
        let p = write(
            &org,
            utc(),
            make_post(addr("demo/worker"), top(), "crash seam"),
        )
        .await;
        org.execute(
            OrgCommand::Channel(ChannelCommand::Seen {
                channel: demo(),
                reader: Address::Owner,
                through: p,
            }),
            utc(),
        )
        .await
        .unwrap();
        let clock = MessageClock::new(Instant::now(), utc());
        let routes = RouteHandle::new(org.clone(), enabled(), clock.clone(), utc());
        let mut c = connect(&org, routes).await;
        c.attach().await.unwrap();
        assert_eq!(
            c.answer(wake(p, &["owner", "demo/other"]))
                .await
                .unwrap_err()
                .code,
            "unavailable"
        );
        c.disconnect().await;
        stop.cancel();
        let (copy, reopened, stop) = restart(dir.path()).await;
        let (state, cursor) = reopened.snapshot_at().await.unwrap();
        assert!(post(&state, p).unwrap().routed.is_some());
        assert_eq!(
            wake_for(&state, p, &Address::Owner).is_some(),
            owner_committed
        );
        assert!(wake_for(&state, p, &addr("demo/other")).is_none());
        let (mut status, task) = spawn_wake_repair(reopened.clone(), clock, stop.clone());
        timeout(
            secs(60),
            status.wait_for(|s| matches!(s, Ok(seq) if *seq >= cursor)),
        )
        .await
        .unwrap()
        .unwrap();
        let state = reopened.snapshot().await.unwrap();
        let owner = wake_for(&state, p, &Address::Owner).unwrap();
        assert!(state.message(&owner).unwrap().read_at.is_some());
        assert!(wake_for(&state, p, &addr("demo/other")).is_some());
        assert_eq!(
            state
                .messages()
                .filter(|m| m.content.post == Some(p))
                .count(),
            2
        );
        assert_eq!(
            reopened.status().unwrap(),
            cursor + if owner_committed { 2 } else { 3 }
        );
        assert!(pending_routes(&state).next().is_none());
        let repaired = reopened.status().unwrap();
        stop.cancel();
        task.await.unwrap();
        let (_second, org, cancel) = restart(copy.path()).await;
        assert_eq!(
            crate::org::channel_post::repair(&org, utc(), true)
                .await
                .unwrap(),
            0
        );
        assert_eq!(org.status().unwrap(), repaired);
        cancel.cancel();
    }
}

// Break: sticking with the old deadline or taking the last post's deadline.
#[tokio::test(start_paused = true)]
async fn channel_route_timer_rearms_for_the_earliest_deadline() {
    let r = Rig::new(enabled(), false).await;
    let future = write(
        &r.org,
        utc() + TimeDelta::seconds(20),
        make_post(Address::Owner, top(), "future"),
    )
    .await;
    let mut c = r.connect().await;
    c.attach().await.unwrap();
    let mut timer = r.timer();
    timer.at(future.seq(), Some(r.origin + secs(40))).await;
    let early = r.post("owner", top(), "earlier deadline").await;
    timer.at(early.seq(), Some(r.origin + secs(20))).await;
    let newest = write(
        &r.org,
        utc() + TimeDelta::seconds(30),
        make_post(Address::Owner, top(), "even later deadline"),
    )
    .await;
    timer.at(newest.seq(), Some(r.origin + secs(20))).await;
    advance(secs(19)).await;
    r.assert_pending(early);
    advance(secs(1)).await;
    timer.at(newest.seq() + 2, Some(r.origin + secs(40))).await;
    r.check(early, stopgap("timeout"), &["demo/lead"], Value::Null);
    r.assert_pending(future);
    r.assert_pending(newest);
    timer.stop().await;
}

// Break: throwing away Lagged and only scanning the still-buffered records.
#[tokio::test(start_paused = true)]
async fn channel_route_rescans_after_lag_with_no_deadline() {
    let r = Rig::new(RouterSettings::default(), false).await;
    let cancel = r.stop.child_token();
    let (parked, mut parks) = mpsc::unbounded_channel();
    // Poll the actual runner to its first park, but don't poll it again until
    // every pending post's notification has fallen out of the broadcast.
    let mut run = std::pin::pin!(r.routes.run(cancel.clone(), |seq, until| {
        parked.send((seq, until)).unwrap();
    }));
    let first = timeout(secs(60), async {
        tokio::select! {
            () = &mut run => panic!("routing worker stopped before parking"),
            state = parks.recv() => state.unwrap(),
        }
    })
    .await
    .unwrap();
    assert_eq!(first, (r.org.status().unwrap(), None));
    let mut witness = r.org.committed.subscribe();
    let mut posts = Vec::new();
    for _ in 0..3 {
        posts.push(r.post("owner", top(), "lost notification").await);
    }
    for _ in 0..RECORD_CAPACITY {
        r.post(
            "owner",
            PostTarget::Direct(addr("demo/worker")),
            "already addressed",
        )
        .await;
    }
    assert!(matches!(witness.try_recv(), Err(broadcast::error::TryRecvError::Lagged(n)) if n == 3));
    let final_seq = r.org.status().unwrap() + 2 * posts.len() as u64;
    let after = timeout(secs(60), async {
        tokio::select! {
            () = &mut run => panic!("routing worker stopped before recovering the burst"),
            state = parks.recv() => state.unwrap(),
        }
    })
    .await
    .unwrap();
    assert_eq!(after, (final_seq, None));
    assert_eq!(
        Instant::now(),
        r.origin,
        "lag recovery must not need a timer"
    );
    for p in posts {
        r.check(p, stopgap("disabled"), &["demo/lead"], Value::Null);
    }
    cancel.cancel();
    run.await;
}

#[tokio::test(start_paused = true)]
async fn channel_route_timer_recovers_after_healthy_writer_timeout() {
    let r = Rig::new(RouterSettings::default(), true).await;
    let p = r.post("owner", top(), "first append times out").await;
    let mut timer = r.timer();
    timer.at(p.seq(), Some(r.origin + secs(1))).await;
    r.assert_pending(p);
    advance(secs(1)).await;
    timer.at(p.seq() + 2, None).await;
    r.check(p, stopgap("disabled"), &["demo/lead"], Value::Null);
    timer.stop().await;
}

// Break: treating an I/O failure like a retryable timeout.
#[tokio::test(start_paused = true)]
async fn channel_route_stops_on_unhealthy_writer_leaving_the_post_pending() {
    let r = Rig::with_first_route_error(
        RouterSettings::default(),
        Some(EventLogError::Io {
            path: "events".into(),
            source: std::io::Error::other("first route append failed"),
        }),
    )
    .await;
    let p = r.post("owner", top(), "survives writer failure").await;
    let timer = r.timer();
    timeout(secs(60), timer.run)
        .await
        .expect("the worker must stop on its own")
        .unwrap();
    assert!(timer.parked.borrow().is_none(), "dead writer was retried");
    assert!(r.org.status().is_err());
    assert_eq!(Instant::now(), r.origin);

    // The failed writer doesn't serve snapshots. Replay what it retained.
    let mut replayed = OrgState::default();
    for record in r.stored.lock().unwrap().iter() {
        let event = OrgEvent::from_parts(&record.kind, &record.body)
            .unwrap()
            .unwrap();
        replayed.apply(record.seq, record.ts, &event).unwrap();
    }
    assert_eq!(pending_routes(&replayed).collect::<Vec<_>>(), [(p, utc())]);
}

// Break: an invalid identity acquires the lease or leaks into the journal.
#[tokio::test(start_paused = true)]
async fn channel_route_attach_bounds_each_identity_field_before_taking_the_lease() {
    let r = Rig::new(enabled(), false).await;
    let mut bad = r.connect().await;
    for field in ["name", "model"] {
        for value in [
            String::new(),
            "r".repeat(129),
            "é".repeat(65),
            "two\nlines".into(),
            "two\rlines".into(),
            "two\u{2028}lines".into(),
        ] {
            let mut params = json!({"name":"router","model":"local"});
            params[field] = json!(value);
            let error = bad.call("router.attach", params, None).await.unwrap_err();
            assert_eq!(
                error.code, "invalid_params",
                "field {field}, value {value:?}"
            );
        }
    }
    let mut good = r.connect().await;
    let exact = "é".repeat(64);
    good.call("router.attach", json!({"name":exact,"model":exact}), None)
        .await
        .unwrap();
    let p = r
        .post("owner", top(), "route with the boundary metadata")
        .await;
    good.answer(wake(p, &[])).await.unwrap();
    r.check(
        p,
        json!({"kind":"router","name":exact,"model":exact}),
        &[],
        json!("scripted choice"),
    );
}

#[tokio::test(start_paused = true)]
async fn channel_route_oversized_attach_does_not_take_the_slot() {
    let r = Rig::new(enabled(), false).await;
    let mut posts = Vec::new();
    for _ in 0..8_200 {
        posts.push(r.post("owner", top(), "backlog").await);
    }
    let mut c = r.connect().await;
    let error = c.attach().await.unwrap_err();
    assert_eq!(error.code, "refused");
    assert!(error.message.contains("512 KiB"));
    assert_eq!(
        c.answer(wake(posts[0], &[])).await.unwrap_err().code,
        "refused"
    );
    for p in posts {
        r.org
            .execute(
                OrgCommand::Channel(ChannelCommand::Route(PostRouted {
                    post: p,
                    wake: vec![],
                    by: RouteBy::Stopgap {
                        cause: "no_router".into(),
                    },
                    reason: None,
                })),
                utc(),
            )
            .await
            .unwrap();
    }
    let mut other = r.connect().await;
    assert!(other.attach().await.unwrap().pending.is_empty());
}
