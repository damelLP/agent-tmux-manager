use super::*;
use crate::{
    org::OrgHandle,
    registry::{RegistryCommand, RegistryHandle},
    server::methods::{self, CallContext, CallerId, MethodServices},
};
use atm_core::{SessionId, SessionView};
use atm_tmux::{mock::MockCall, MockTmuxClient, TmuxClient};
use serde_json::{json, Value};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    sync::{broadcast, mpsc},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

struct Rig {
    control: super::super::service::ControlHandle,
    registry: RegistryHandle,
    reads: Arc<AtomicUsize>,
    cancel: CancellationToken,
    actor: JoinHandle<()>,
    rows: JoinHandle<()>,
}
impl Rig {
    fn new(client: Box<dyn TmuxClient>) -> Self {
        let cancel = CancellationToken::new();
        let (send, mut receive) = mpsc::channel(32);
        let registry = RegistryHandle::new(send, broadcast::channel(16).0);
        let reads = Arc::new(AtomicUsize::new(0));
        let count = reads.clone();
        let stop = cancel.clone();
        let rows = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _=stop.cancelled()=>break,
                    command=receive.recv()=>match command {
                        Some(RegistryCommand::GetAllSessions{respond_to})=>{
                            count.fetch_add(1,Ordering::SeqCst);
                            let _=respond_to.send(vec![row("a",Some("%0"))]);
                        },
                        None=>break,
                        _=>panic!("unexpected registry command"),
                    }
                }
            }
        });
        let (control, actor) = super::super::service::spawn_control(
            client,
            registry.clone(),
            OrgHandle::disabled("test"),
            cancel.clone(),
        );
        Self {
            control,
            registry,
            reads,
            cancel,
            actor,
            rows,
        }
    }
    fn router(&self) -> methods::MethodRouter {
        let mut services = MethodServices::new(self.registry.clone());
        services.control = Some(self.control.clone());
        methods::router(services).unwrap()
    }
    async fn close(&mut self) {
        self.cancel.cancel();
        tokio::time::timeout(Duration::from_secs(60), async {
            (&mut self.actor).await.unwrap();
            (&mut self.rows).await.unwrap();
        })
        .await
        .unwrap();
    }
}
impl Drop for Rig {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.actor.abort();
        self.rows.abort();
    }
}
fn caller(id: &str) -> CallContext {
    CallContext {
        client_id: CallerId(id.into()),
        from_pane: None,
    }
}

// Break caught: method absent, caller fencing, implicit role I/O, or accepting invalid params.
#[tokio::test]
async fn windows_router_advertises_and_validates_before_io_for_any_caller() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let mock = MockTmuxClient::new();
        let mut rig = Rig::new(Box::new(mock.clone()));
        let router = rig.router();
        assert!(router.names().iter().any(|name| name == "session.windows"));
        let info = router
            .call(caller("owner"), "daemon.info", json!({}))
            .await
            .unwrap();
        assert!(info["methods"]
            .as_array()
            .unwrap()
            .iter()
            .any(|name| name == "session.windows"));
        for bad in [
            json!({"caller_server":"/unverified/server"}),
            json!([]),
            json!(3),
            json!("x"),
        ] {
            assert_eq!(
                router
                    .call(caller("anonymous"), "session.windows", bad)
                    .await
                    .unwrap_err()
                    .0
                    .code,
                "invalid_params"
            );
        }
        assert!(mock.calls().is_empty());
        assert_eq!(rig.reads.load(Ordering::SeqCst), 0);
        let capture: Value = serde_json::from_str(CAPTURES[0]).unwrap();
        for id in ["owner", "alpha/worker", "anonymous"] {
            mock.set_next_raw_output(capture["snapshots"]["grouped_linked"].as_str().unwrap());
            let result = router
                .call(caller(id), "session.windows", Value::Null)
                .await
                .unwrap();
            assert_eq!(result["state"], "ok");
            assert_eq!(result["windows"][0]["panes"], json!(["%0"]));
        }
        assert_eq!(mock.call_count(), 3);
        assert_eq!(rig.reads.load(Ordering::SeqCst), 3);
        rig.close().await;
    })
    .await
    .unwrap();
}

// Break caught: turning expected read failures into RPC errors or available empty snapshots.
#[tokio::test]
async fn windows_router_maps_unconfigured_failed_malformed_and_closed_reads_to_values() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let (send, receive) = mpsc::channel(1);
        let registry = RegistryHandle::new(send, broadcast::channel(1).0);
        drop(receive);
        let router = methods::router(MethodServices::new(registry)).unwrap();
        assert_eq!(
            router
                .call(caller("anonymous"), "session.windows", json!({}))
                .await
                .unwrap()["state"],
            "unavailable"
        );
        let mock = MockTmuxClient::new();
        let mut rig = Rig::new(Box::new(mock.clone()));
        let router = rig.router();
        for error in [
            atm_tmux::TmuxError::NotFound,
            atm_tmux::TmuxError::CommandFailed {
                command: "list-panes".into(),
                stderr: "private\x1bdetails".into(),
            },
            atm_tmux::TmuxError::Timeout {
                command: "list-panes".into(),
                timeout: Duration::from_secs(5),
            },
        ] {
            mock.set_next_error(error);
            let reply = router
                .call(caller("a"), "session.windows", json!({}))
                .await
                .unwrap();
            assert_eq!(reply["state"], "unavailable");
            assert_eq!(reply["windows"], json!([]));
            assert!(!reply.to_string().contains("private"));
        }
        mock.set_next_raw_output("malformed output");
        assert_eq!(
            router
                .call(caller("a"), "session.windows", json!({}))
                .await
                .unwrap()["state"],
            "unavailable"
        );
        rig.close().await;
        assert_eq!(
            serde_json::to_value(rig.control.windows().await).unwrap()["state"],
            "unavailable"
        );
    })
    .await
    .unwrap();
}

type TmuxCall<'a, T> = std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<T, atm_tmux::TmuxError>> + Send + 'a>,
>;
struct GatedTmux {
    inner: MockTmuxClient,
    entered: mpsc::UnboundedSender<()>,
    release: Arc<tokio::sync::Semaphore>,
}
macro_rules! forward {
    ($method:ident <$($lt:lifetime),*> ($($arg:ident : $ty:ty),*) -> $result:ty) => {
        fn $method<'s,$($lt,)*'async_trait>(&'s self,$($arg:$ty),*) -> TmuxCall<'async_trait,$result>
        where 's:'async_trait,$($lt:'async_trait,)*Self:'async_trait {
            Box::pin(self.inner.$method($($arg),*))
        }
    };
}
impl TmuxClient for GatedTmux {
    fn raw_command<'s, 'c, 'a, 'v, 'async_trait>(
        &'s self,
        subcommand: &'c str,
        args: &'a [&'v str],
    ) -> TmuxCall<'async_trait, String>
    where
        's: 'async_trait,
        'c: 'async_trait,
        'a: 'async_trait,
        'v: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            let _ = self.entered.send(());
            self.release.acquire().await.unwrap().forget();
            self.inner.raw_command(subcommand, args).await
        })
    }
    forward!(split_window<'a,'b,'c,'d>(target:&'a str,size:&'b str,direction:atm_tmux::PaneDirection,command:Option<&'c str>,cwd:Option<&'d str>,detached:bool)->String);
    forward!(get_pane_cwd<'a>(pane:&'a str)->Option<String>);
    forward!(new_window<'a,'b>(session:&'a str,command:Option<&'b str>)->String);
    forward!(kill_pane<'a>(pane:&'a str)->());
    forward!(resize_pane<'a>(pane:&'a str,width:Option<u16>,height:Option<u16>)->());
    forward!(send_keys<'a,'b>(pane:&'a str,keys:&'b str)->());
    forward!(paste_text<'a,'b>(pane:&'a str,text:&'b str)->());
    forward!(list_panes<>()->Vec<atm_tmux::PaneInfo>);
    forward!(display_popup<'a,'b,'c>(width:&'a str,height:&'b str,command:&'c str)->());
    forward!(select_pane<'a>(pane:&'a str)->());
    forward!(capture_pane<'a>(pane:&'a str)->Vec<String>);
    forward!(new_session<'a>(name:&'a str)->String);
}
fn gated() -> (
    GatedTmux,
    mpsc::UnboundedReceiver<()>,
    Arc<tokio::sync::Semaphore>,
) {
    let (entered, receive) = mpsc::unbounded_channel();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    (
        GatedTmux {
            inner: MockTmuxClient::new(),
            entered,
            release: release.clone(),
        },
        receive,
        release,
    )
}
// Break caught: different callers start separate reads, or leader cancellation frees a running slot.
#[tokio::test]
async fn windows_overlapping_cancelled_callers_share_one_retained_observation() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let (gate, mut entered, release) = gated();
        let mock = gate.inner.clone();
        let capture: Value = serde_json::from_str(CAPTURES[0]).unwrap();
        mock.set_next_raw_output(capture["snapshots"]["joined_pane"].as_str().unwrap());
        let mut rig = Rig::new(Box::new(gate));
        let router = Arc::new(rig.router());
        let first_router = router.clone();
        let first = tokio::spawn(async move {
            first_router
                .call(caller("owner"), "session.windows", json!({}))
                .await
        });
        entered.recv().await.unwrap();
        let mut second =
            Box::pin(router.call(caller("alpha/worker"), "session.windows", json!({})));
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(std::future::Future::poll(
                second.as_mut(),
                cx
            )))
            .await
            .is_pending()
        );
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        // A broken duplicate read can finish too, so it fails data/count assertions,
        // not merely the outer hang guard.
        release.add_permits(2);
        let result = second.await.unwrap();
        assert_eq!(result["windows"][0]["id"], "@0");
        assert_eq!(mock.call_count(), 1);
        assert_eq!(rig.reads.load(Ordering::SeqCst), 1);
        // Completion releases the slot: a later read sees fresh data, not an indefinitely cached reply.
        mock.set_next_raw_output("");
        release.add_permits(1);
        assert_eq!(
            router
                .call(caller("anonymous"), "session.windows", json!({}))
                .await
                .unwrap()["windows"],
            json!([])
        );
        assert_eq!(mock.call_count(), 2);
        rig.close().await;
    })
    .await
    .unwrap();
}
// Break caught: an outer router timeout escapes as an RPC error, or a stalled read survives its budget.
#[tokio::test(start_paused = true)]
async fn windows_stalled_read_budget_returns_unavailable_and_releases_flight() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let (gate, mut entered, release) = gated();
        let mut rig = Rig::new(Box::new(gate));
        let router = rig.router();
        let first = tokio::spawn(async move {
            router
                .call(caller("anonymous"), "session.windows", json!({}))
                .await
        });
        entered.recv().await.unwrap();
        tokio::time::advance(Duration::from_secs(5)).await;
        assert_eq!(first.await.unwrap().unwrap()["state"], "unavailable");
        release.add_permits(1);
        assert_eq!(
            serde_json::to_value(rig.control.windows().await).unwrap()["state"],
            "ok"
        );
        rig.close().await;
    })
    .await
    .unwrap();
}

// Break caught: an observation expired in the actor queue still touches tmux/registry,
// or loss of the registry is misreported as a successful empty observation.
#[tokio::test]
async fn windows_expired_and_closed_registry_reads_are_unavailable() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let (send, mut receive) = mpsc::channel(1);
        let registry = RegistryHandle::new(send, broadcast::channel(1).0);
        let mock = MockTmuxClient::new();
        let result = read(&mock, &registry, tokio::time::Instant::now()).await;
        assert_eq!(
            serde_json::to_value(result).unwrap()["state"],
            "unavailable"
        );
        assert!(matches!(
            receive.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        assert!(mock.calls().is_empty());
        drop(receive);
        let result = read(
            &mock,
            &registry,
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await;
        let value = serde_json::to_value(result).unwrap();
        assert_eq!(value["state"], "unavailable");
        assert_eq!(value["reason"], "session registry unavailable");
    })
    .await
    .unwrap();
}
// Break caught: keeping a sender in the shared slot strands followers after actor loss.
#[tokio::test]
async fn windows_actor_loss_wakes_all_followers_without_detached_readers() {
    tokio::time::timeout(Duration::from_secs(60),async {
        let (gate,mut entered,_release)=gated(); let mut rig=Rig::new(Box::new(gate));
        let control=rig.control.clone();
        let mut read=Box::pin(control.windows());
        tokio::select! { result=&mut read=>panic!("read returned early: {result:?}"),_=entered.recv()=>{} }
        let mut follower=Box::pin(control.windows());
        assert!(std::future::poll_fn(|cx|std::task::Poll::Ready(std::future::Future::poll(follower.as_mut(),cx))).await.is_pending());
        rig.actor.abort(); assert!((&mut rig.actor).await.unwrap_err().is_cancelled());
        assert_eq!(serde_json::to_value(read.await).unwrap()["state"],"unavailable");
        assert_eq!(serde_json::to_value(follower.await).unwrap()["state"],"unavailable");
        rig.cancel.cancel(); (&mut rig.rows).await.unwrap();
    }).await.unwrap();
}

// Captured: tmux 3.8 and 3.6a (package 3.6a-2ubuntu0.2), 2026-11-17,
// private servers, list-panes -a -F; metadata and full raw command output in each fixture.
const CAPTURES: [&str; 2] = [
    include_str!("../../../atm-tmux/tests/fixtures/tmux_3.8_pane_windows.json"),
    include_str!("../../../atm-tmux/tests/fixtures/tmux_3.6a_pane_windows.json"),
];
fn row(id: &str, pane: Option<&str>) -> SessionView {
    SessionView {
        id: SessionId::new(id),
        tmux_pane: pane.map(Into::into),
        ..Default::default()
    }
}
fn now() -> chrono::DateTime<chrono::Utc> {
    "2026-01-01T00:00:00Z".parse().unwrap()
}

// Break caught: lost links, duplicated panes, unrelated tmux panes, or nondeterministic grouping.
#[tokio::test]
async fn windows_projection_joins_real_grouped_linked_moved_and_reparented_captures() {
    for fixture in CAPTURES {
        let capture: Value = serde_json::from_str(fixture).unwrap();
        let mock = MockTmuxClient::new();
        for (name, wanted) in [
            (
                "grouped_linked",
                json!([
                    {"id":"@0","name":"support","links":[{"session":"alpha","index":0},{"session":"beta","index":0},{"session":"gamma","index":4}],"panes":["%0"]}
                ]),
            ),
            (
                "moved_link",
                json!([
                    {"id":"@0","name":"support","links":[{"session":"alpha","index":0},{"session":"beta","index":0},{"session":"gamma","index":7}],"panes":["%0"]}
                ]),
            ),
            (
                "joined_pane",
                json!([
                    {"id":"@0","name":"support","links":[{"session":"alpha","index":0},{"session":"beta","index":0},{"session":"gamma","index":7}],"panes":["%0","%3"]}
                ]),
            ),
            (
                "broken_pane",
                json!([
                    {"id":"@0","name":"support","links":[{"session":"alpha","index":0},{"session":"beta","index":0},{"session":"gamma","index":7}],"panes":["%0"]},
                    {"id":"@4","name":"sh","links":[{"session":"gamma","index":1}],"panes":["%3"]}
                ]),
            ),
        ] {
            mock.set_next_raw_output(capture["snapshots"][name].as_str().unwrap());
            let panes = mock.list_pane_windows().await.unwrap();
            let result = serde_json::to_value(project(
                &[
                    row("a", Some("%0")),
                    row("shared", Some("%0")),
                    row("b", Some("%3")),
                    row("missing", Some("%404")),
                    row("paneless", None),
                ],
                &panes,
                now(),
            ))
            .unwrap();
            assert_eq!(
                result,
                json!({"generated_at":"2026-01-01T00:00:00Z","state":"ok","reason":null,"windows":wanted}),
                "{name}"
            );
        }
        assert_eq!(mock.call_count(), 4);
        assert!(mock.calls().iter().all(
            |call| matches!(call, MockCall::RawCommand{subcommand,..} if subcommand=="list-panes")
        ));
    }
}

// Synthetic: only window display names, not exact session link identities, have a 200-char limit.
#[test]
fn windows_projection_preserves_long_link_identity() {
    let session = "s".repeat(240);
    let panes =
        atm_tmux::windows::parse_pane_windows(&format!("%0\t@0\t{session}\t0\tx\n")).unwrap();
    assert_eq!(
        serde_json::to_value(project(&[row("a", Some("%0"))], &panes, now())).unwrap()["windows"]
            [0]["links"][0]["session"],
        session
    );
}

// Break caught: byte truncation, terminal controls, rejecting empty names, or dropping valid Unicode.
#[tokio::test]
async fn windows_projection_sanitizes_names_and_observes_available_empty() {
    for fixture in CAPTURES {
        let capture: Value = serde_json::from_str(fixture).unwrap();
        let mock = MockTmuxClient::new();
        for (name, wanted) in [
            ("unicode_name", "支援 café 🚀".to_string()),
            ("long_name", "界".repeat(200)),
        ] {
            mock.set_next_raw_output(capture["snapshots"][name].as_str().unwrap());
            let panes = mock.list_pane_windows().await.unwrap();
            let result =
                serde_json::to_value(project(&[row("a", Some("%0"))], &panes, now())).unwrap();
            assert_eq!(result["windows"][0]["name"], wanted);
            assert_eq!(
                serde_json::to_value(project(&[], &panes, now())).unwrap()["windows"],
                json!([])
            );
        }
        // Every retained real capture parses, including escaped 3.6a names and empty 3.8 auto names.
        for output in capture["snapshots"].as_object().unwrap().values() {
            mock.set_next_raw_output(output.as_str().unwrap());
            assert!(mock.list_pane_windows().await.is_ok());
        }
    }
    // Synthetic: tmux 3.8 refuses these names; defensive handling must still be control-free.
    let panes = atm_tmux::windows::parse_pane_windows("%0\t@0\ta\t0\t  x\x1b\t界\x07\n").unwrap();
    assert_eq!(
        serde_json::to_value(project(&[row("a", Some("%0"))], &panes, now())).unwrap()["windows"]
            [0]["name"],
        "  x界"
    );
    assert_eq!(
        serde_json::to_value(unavailable(now(), "bad\x1breason")).unwrap(),
        json!({"generated_at":"2026-01-01T00:00:00Z","state":"unavailable","reason":"badreason","windows":[]})
    );
}
