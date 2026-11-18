//! Driver proofs reuse the real VW1 client/private peer; clock-only proofs use channels.
use super::*;
use crate::tui_work::WorkController;
use atm_tui::{
    control::ControlNotice,
    work::{Generation, WorkEffect, WorkInput, WorkState},
};
use std::task::Poll;

async fn pending_once(future: impl Future) {
    tokio::pin!(future);
    std::future::poll_fn(|cx| {
        assert!(future.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
}
fn shown(state: &mut WorkState) -> Vec<WorkEffect> {
    let _ = state.reduce(
        WorkInput::Capabilities {
            methods: vec!["session.windows".into(), "org.tree".into()],
            watch: false,
        },
        Duration::ZERO,
    );
    state.reduce(WorkInput::Shown, Duration::ZERO)
}
fn generations() -> (Generation, Generation) {
    let effects = shown(&mut WorkState::default());
    let windows = effects
        .iter()
        .find_map(|effect| match effect {
            WorkEffect::RequestWindows(generation) => Some(*generation),
            _ => None,
        })
        .unwrap();
    let org = effects
        .iter()
        .find_map(|effect| match effect {
            WorkEffect::RequestOrg(generation) => Some(*generation),
            _ => None,
        })
        .unwrap();
    (windows, org)
}
fn windows() -> Value {
    json!({"generated_at":"2026-01-15T12:00:00Z","state":"ok","reason":null,
        "windows":[{"id":"@3","name":"support","links":[{"session":"alpha","index":0}],"panes":["%7"]}]})
}
async fn pair(peer: &mut Peer) -> Vec<(String, String)> {
    let mut reads = Vec::new();
    for _ in 0..2 {
        let MessageType::Request {
            id,
            method,
            params,
            from_pane,
        } = peer.recv().await
        else {
            panic!("expected Work read")
        };
        assert_eq!(params, json!({}));
        assert_eq!(from_pane.as_deref(), Some("%99"));
        reads.push((id, method));
    }
    let mut methods: Vec<_> = reads.iter().map(|(_, method)| method.as_str()).collect();
    methods.sort_unstable();
    assert_eq!(methods, ["org.tree", "session.windows"]);
    reads
}
async fn reply_pair(peer: &mut Peer, reads: Vec<(String, String)>) {
    for (id, method) in reads {
        let value = if method == "session.windows" {
            windows()
        } else {
            tree()
        };
        peer.send(DaemonMessage::response_ok(id, value)).await;
    }
}
fn driver(rig: &Rig) -> WorkController {
    WorkController::new(ControlHandle::new(rig.commands.clone()), Some("%99".into()))
}

// Break caught: merely constructing the driver starts traffic, or Shown uses a second connection/caller.
#[tokio::test]
async fn work_driver_is_idle_until_shown() {
    let (rig, mut peer) = Rig::start(&["session.windows", "org.tree"]).await;
    let _cancel = rig.cancel.clone().drop_guard();
    let mut owner = rig.controller();
    let mut driver = driver(&rig);
    let mut state = WorkState::default();
    assert!(driver
        .execute(state.reduce(WorkInput::Hidden, Duration::ZERO), &mut owner)
        .is_none());
    pending_once(driver.recv()).await;
    peer.fence(&rig.commands).await;
    let _ = driver.execute(shown(&mut state), &mut owner);
    pending_once(driver.recv()).await;
    let reads = pair(&mut peer).await;
    reply_pair(&mut peer, reads).await;
    for _ in 0..2 {
        let input = guarded(driver.recv()).await;
        assert!(matches!(
            input,
            WorkInput::WindowsReply { .. } | WorkInput::OrgReply { .. }
        ));
        let effects = state.reduce(input, driver.now());
        let _ = driver.execute(effects, &mut owner);
    }
    assert_eq!(state.windows().first().unwrap().name, "support");
    assert_eq!(state.org().unwrap().seq, Some(41));
    peer.fence(&rig.commands).await;
    assert!(peer.watches.is_empty());
    driver.shutdown();
    owner.shutdown();
    rig.stop().await;
}

// Break caught: a queued pre-disconnect join snapshot activates Work on the new connection.
#[tokio::test]
async fn work_join_disconnect_reconnect_with_work_visible() {
    use atm_tui::{
        app::{App, AppView},
        control::ControlNotice,
    };
    fn input(
        app: &mut App,
        driver: &mut WorkController,
        owner: &mut DecisionController,
        input: WorkInput,
    ) {
        let mut effects = app.reduce_work(input, Duration::ZERO);
        loop {
            if let Some(seq) = app.work.org().and_then(|org| org.seq) {
                owner.advance_work_cursor(seq);
            }
            match driver.execute(effects, owner) {
                Some(input) => effects = app.reduce_work(input, Duration::ZERO),
                None => break,
            }
        }
    }
    let mut methods = METHODS.to_vec();
    methods.push("session.windows");
    let (mut rig, mut peer) = Rig::start(&methods).await;
    let mut owner = rig.controller();
    let mut driver = driver(&rig);
    // One bound for the whole proof; cleanup below also runs after a guard/assertion failure.
    let result = {
        let proof = guarded(async {
            work_snapshot(&mut owner, &mut peer, 41).await;
            peer.fence(&rig.commands).await;
            let mut app = App::new();
            app.view = AppView::Sessions;
            loop {
                if let Event::Control(notice @ ControlNotice::Capabilities { .. }) =
                    guarded(rig._events.recv()).await.unwrap()
                {
                    owner.notice(&notice);
                    let ControlNotice::Capabilities { methods, .. } = notice else {
                        unreachable!()
                    };
                    input(
                        &mut app,
                        &mut driver,
                        &mut owner,
                        WorkInput::Capabilities {
                            methods,
                            watch: true,
                        },
                    );
                    break;
                }
            }
            let effects = app.toggle_work_view(Duration::ZERO);
            assert!(driver.execute(effects, &mut owner).is_none());
            pending_once(driver.recv()).await;
            for (id, method) in pair(&mut peer).await {
                let mut org = tree();
                org["seq"] = json!(1065);
                peer.send(DaemonMessage::response_ok(
                    id,
                    if method == "org.tree" { org } else { windows() },
                ))
                .await;
            }
            for _ in 0..2 {
                let reply = guarded(driver.recv()).await;
                input(&mut app, &mut driver, &mut owner, reply);
            }
            let held = work_join_org(&mut peer).await;
            let mut org = tree();
            org["seq"] = json!(1065);
            peer.send(DaemonMessage::response_ok(held, org)).await;
            peer.reply("decision.list", page(vec![], None)).await;
            let stale = guarded(owner.recv()).await.unwrap(); // queued, not yet applied
            assert_eq!(app.view, AppView::Work);
            let path = rig._dir.path().join("peer.sock");
            tokio::fs::remove_file(&path).await.unwrap();
            let listener = UnixListener::bind(&path).unwrap();
            drop(peer);
            loop {
                if let Event::DaemonDisconnected(_) = guarded(rig._events.recv()).await.unwrap() {
                    owner.disconnected();
                    input(&mut app, &mut driver, &mut owner, WorkInput::WatchFailed);
                    break;
                }
            }
            let (stream, _) = guarded(listener.accept()).await.unwrap();
            let (reader, writer) = stream.into_split();
            let mut peer = Peer {
                reader: BufReader::new(reader),
                writer,
                watches: vec![],
            };
            assert!(matches!(peer.recv().await, MessageType::Connect { .. }));
            peer.send(DaemonMessage::Connected {
                protocol_version: ProtocolVersion::new(1, 1),
                client_id: "work-reconnected".into(),
                methods: methods.iter().map(|m| (*m).into()).collect(),
            })
            .await;
            assert!(matches!(peer.recv().await, MessageType::Subscribe { .. }));
            peer.send(DaemonMessage::session_list(vec![])).await;
            loop {
                if let Event::Control(notice @ ControlNotice::Capabilities { .. }) =
                    guarded(rig._events.recv()).await.unwrap()
                {
                    owner.notice(&notice);
                    let ControlNotice::Capabilities { methods, .. } = notice else {
                        unreachable!()
                    };
                    input(
                        &mut app,
                        &mut driver,
                        &mut owner,
                        WorkInput::Capabilities {
                            methods,
                            watch: true,
                        },
                    );
                    break;
                }
            }
            owner.apply(stale);
            let mut seen = Vec::new();
            let mut frame = Vec::new();
            while seen.len() < 2 {
                tokio::select! {
                    count = peer.reader.read_until(b'\n', &mut frame) => {
                        assert_ne!(count.unwrap(), 0);
                        let message = serde_json::from_slice::<ClientMessage>(&frame).unwrap().message;
                        frame.clear(); // read_until retains partial data if another select arm wins.
                        match message {
                        MessageType::Request {id,method,..} => {
                            let value = match method.as_str() {
                                "org.tree" => {let mut org=tree();org["seq"]=json!(2090);org},
                                "session.windows" => windows(),
                                "decision.list" => page(vec![],None),
                                _ => panic!("unexpected reconnect method {method}"),
                            };
                            peer.send(DaemonMessage::response_ok(id,value)).await;
                        }
                        MessageType::Unwatch => {},
                        MessageType::Ping {seq} => peer.send(DaemonMessage::Pong {seq}).await,
                        MessageType::Watch {topics,cursor} => {
                            if topics.iter().any(|p|p.starts_with("task.")) {
                                assert_eq!(cursor,2090,"stale join resumed on reconnect");
                                for (seq,kind) in [(2091,"task.assigned"),(2092,"decision.requested")] {
                                    peer.send(DaemonMessage::Event {topic:"org".into(),seq,kind:kind.into(),
                                        ts:"2026-01-15T12:00:00Z".into(),body:json!({})}).await;
                                }
                            }
                            peer.watches.push((topics,cursor));
                        }
                        other => panic!("unexpected reconnect frame {other:?}"),
                        }
                    },
                    update = owner.recv() => owner.apply(update.unwrap()),
                    reply = driver.recv() => input(&mut app,&mut driver,&mut owner,reply),
                    event = rig._events.recv() => if let Event::Control(notice) = event.unwrap() {
                        owner.notice(&notice);
                        if let ControlNotice::Event(DaemonMessage::Event {seq,kind,..}) = notice {
                            seen.push(kind.clone());
                            input(&mut app,&mut driver,&mut owner,WorkInput::WatchEvent {seq,kind});
                        }
                    }
                }
            }
            assert_eq!(seen, ["task.assigned", "decision.requested"]);
            assert_eq!(app.view, AppView::Work);
            assert_eq!(app.work.org().unwrap().seq, Some(2090));
        });
        tokio::pin!(proof);
        std::future::poll_fn(|cx| {
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| proof.as_mut().poll(cx)))
            {
                Ok(Poll::Ready(())) => Poll::Ready(Ok(())),
                Ok(Poll::Pending) => Poll::Pending,
                Err(error) => Poll::Ready(Err(error)),
            }
        })
        .await
    };
    driver.shutdown();
    owner.shutdown();
    rig.stop().await;
    result.unwrap_or_else(|error| std::panic::resume_unwind(error));
}

// Break caught: wrong budget, timeout not returned, or canceling recv drops a stored request.
#[tokio::test(start_paused = true)]
async fn work_driver_budgets_and_terminal_replies() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let control = ControlHandle::new(tx);
    let mut owner = DecisionController::new(control.clone(), None);
    let mut driver = WorkController::new(control, None);
    let (windows, org) = generations();
    let _ = driver.execute(
        vec![
            WorkEffect::RequestWindows(windows),
            WorkEffect::RequestOrg(org),
        ],
        &mut owner,
    );
    pending_once(driver.recv()).await;
    let _first = rx.try_recv().unwrap();
    let _second = rx.try_recv().unwrap();
    assert!(rx.try_recv().is_err());
    tokio::time::advance(Duration::from_secs(5)).await;
    let WorkInput::WindowsReply { generation, result } = guarded(driver.recv()).await else {
        panic!("windows must end at 5 s")
    };
    assert_eq!(generation, windows);
    assert!(result.unwrap_err().0.contains("timed out"));
    tokio::time::advance(Duration::from_secs(7)).await;
    let WorkInput::OrgReply { generation, result } = guarded(driver.recv()).await else {
        panic!("org must end at 12 s")
    };
    assert_eq!(generation, org);
    assert!(result.unwrap_err().0.contains("timed out"));
    pending_once(driver.recv()).await;
    driver.shutdown();
}

// Lead Low 2: hide/reopen/refresh cannot release a reducer flight without its tagged reply.
#[tokio::test]
async fn work_driver_hide_reopen_refresh_retains_single_flights() {
    let (rig, mut peer) = Rig::start(&["session.windows", "org.tree"]).await;
    let _cancel = rig.cancel.clone().drop_guard();
    let mut owner = rig.controller();
    let mut driver = driver(&rig);
    let mut state = WorkState::default();
    let _ = driver.execute(shown(&mut state), &mut owner);
    pending_once(driver.recv()).await;
    let held = pair(&mut peer).await;
    for input in [WorkInput::Hidden, WorkInput::Shown, WorkInput::Refresh] {
        let effects = state.reduce(input, driver.now());
        assert!(!effects.iter().any(|effect| matches!(
            effect,
            WorkEffect::RequestWindows(_) | WorkEffect::RequestOrg(_)
        )));
        let _ = driver.execute(effects, &mut owner);
    }
    pending_once(driver.recv()).await;
    peer.fence(&rig.commands).await;
    reply_pair(&mut peer, held).await;
    for _ in 0..2 {
        let input = guarded(driver.recv()).await;
        assert!(matches!(
            input,
            WorkInput::WindowsReply { .. } | WorkInput::OrgReply { .. }
        ));
        let effects = state.reduce(input, driver.now());
        let _ = driver.execute(effects, &mut owner);
    }
    // Each obsolete completion releases exactly its own one coalesced follow-up.
    pending_once(driver.recv()).await;
    let followups = pair(&mut peer).await;
    peer.fence(&rig.commands).await;
    assert!(state.windows().is_empty() && state.org().is_none());
    let _ = driver.execute(state.reduce(WorkInput::Hidden, driver.now()), &mut owner);
    reply_pair(&mut peer, followups).await;
    for _ in 0..2 {
        let input = guarded(driver.recv()).await;
        let effects = state.reduce(input, driver.now());
        assert!(effects.is_empty());
        let _ = driver.execute(effects, &mut owner);
    }
    pending_once(driver.recv()).await;
    peer.fence(&rig.commands).await;
    assert!(state.windows().is_empty() && state.org().is_none());
    driver.shutdown();
    owner.shutdown();
    rig.stop().await;
}

// Break caught: the second select poll restarts an RPC or forgets its terminal result.
#[tokio::test]
async fn work_driver_cancelled_recv_keeps_flights() {
    let (rig, mut peer) = Rig::start(&["session.windows", "org.tree"]).await;
    let _cancel = rig.cancel.clone().drop_guard();
    let mut owner = rig.controller();
    let mut driver = driver(&rig);
    let (windows, org) = generations();
    let _ = driver.execute(
        vec![
            WorkEffect::RequestWindows(windows),
            WorkEffect::RequestOrg(org),
        ],
        &mut owner,
    );
    for _ in 0..3 {
        pending_once(driver.recv()).await;
    }
    let reads = pair(&mut peer).await;
    peer.fence(&rig.commands).await;
    reply_pair(&mut peer, reads).await;
    let mut got = Vec::new();
    for _ in 0..2 {
        got.push(match guarded(driver.recv()).await {
            WorkInput::WindowsReply { generation, result } => {
                assert!(result.is_ok());
                generation
            }
            WorkInput::OrgReply { generation, result } => {
                assert!(result.is_ok());
                generation
            }
            other => panic!("expected terminal reply: {other:?}"),
        });
    }
    assert!(got.contains(&windows) && got.contains(&org));
    pending_once(driver.recv()).await;
    driver.shutdown();
    owner.shutdown();
    rig.stop().await;
}

// Break caught: a synchronous Watch submit failure loops through immediate re-installation.
#[tokio::test(start_paused = true)]
async fn work_driver_local_watch_failure_is_delayed_and_shutdown_disarms_it() {
    let (tx, rx) = mpsc::unbounded_channel();
    drop(rx);
    let control = ControlHandle::new(tx);
    let mut owner = DecisionController::new(control.clone(), None);
    let mut driver = WorkController::new(control, None);
    let mut state = WorkState::default();
    let _ = state.reduce(
        WorkInput::Capabilities {
            methods: vec!["session.windows".into(), "org.tree".into()],
            watch: true,
        },
        Duration::ZERO,
    );
    let reads = state.reduce(WorkInput::Shown, Duration::ZERO);
    let org = reads
        .iter()
        .find_map(|e| match e {
            WorkEffect::RequestOrg(id) => Some(*id),
            _ => None,
        })
        .unwrap();
    let effects = state.reduce(
        WorkInput::OrgReply {
            generation: org,
            result: Ok(serde_json::from_value(tree()).unwrap()),
        },
        Duration::ZERO,
    );
    let failure = driver
        .execute(effects, &mut owner)
        .expect("closed Watch channel");
    let effects = state.reduce(failure, Duration::ZERO);
    assert!(
        driver.execute(effects, &mut owner).is_none(),
        "failure reduction must finish"
    );
    let before = state.reduce(WorkInput::Wake, Duration::from_secs(9));
    assert!(!before.iter().any(|e| matches!(e, WorkEffect::Watch { .. })));
    let retry = state.reduce(WorkInput::Wake, Duration::from_secs(10));
    assert!(!retry.iter().any(|e| matches!(e, WorkEffect::Watch { .. })));
    assert!(retry.iter().any(|e| matches!(e, WorkEffect::RequestOrg(_))));
    assert!(driver.execute(retry, &mut owner).is_none());
    let failure = guarded(driver.recv()).await;
    assert!(matches!(
        &failure,
        WorkInput::OrgReply { result: Err(_), .. }
    ));
    let effects = state.reduce(failure, Duration::from_secs(10));
    assert!(driver.execute(effects, &mut owner).is_none());
    assert!(!state
        .reduce(WorkInput::Wake, Duration::from_secs(19))
        .iter()
        .any(|e| matches!(e, WorkEffect::RequestOrg(_) | WorkEffect::Watch { .. })));
    assert!(state
        .reduce(WorkInput::Wake, Duration::from_secs(20))
        .iter()
        .any(|e| matches!(e, WorkEffect::RequestOrg(_))));
    driver.shutdown();
    tokio::time::advance(Duration::from_secs(100)).await;
    pending_once(driver.recv()).await;
    owner.shutdown();
}

// Break caught: an omitted WakeAt leaves a previous timer armed, or replacement adds another timer.
#[tokio::test(start_paused = true)]
async fn work_driver_replaces_and_disarms_wake() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let control = ControlHandle::new(tx);
    let mut owner = DecisionController::new(control.clone(), None);
    let mut driver = WorkController::new(control, None);
    let _ = driver.execute(vec![WorkEffect::WakeAt(Duration::from_secs(3))], &mut owner);
    let _ = driver.execute(vec![WorkEffect::WakeAt(Duration::from_secs(8))], &mut owner);
    tokio::time::advance(Duration::from_secs(3)).await;
    pending_once(driver.recv()).await;
    tokio::time::advance(Duration::from_secs(5)).await;
    assert!(matches!(guarded(driver.recv()).await, WorkInput::Wake));
    let _ = driver.execute(vec![WorkEffect::WakeAt(Duration::from_secs(9))], &mut owner);
    let _ = driver.execute(vec![], &mut owner);
    tokio::time::advance(Duration::from_secs(100)).await;
    pending_once(driver.recv()).await;
    driver.shutdown();
}

// Break caught: RPC/decode/local failures disappear instead of ending their generation.
#[tokio::test]
async fn work_driver_reports_rpc_decode_and_local_failures() {
    for reply in [None, Some(json!({"state":"ok","windows":"wrong type"}))] {
        let (rig, mut peer) = Rig::start(&["session.windows"]).await;
        let _cancel = rig.cancel.clone().drop_guard();
        let mut owner = rig.controller();
        let mut driver = driver(&rig);
        let (generation, _) = generations();
        let _ = driver.execute(vec![WorkEffect::RequestWindows(generation)], &mut owner);
        pending_once(driver.recv()).await;
        let (id, _) = peer.request("session.windows").await;
        peer.send(match reply {
            Some(value) => DaemonMessage::response_ok(id, value),
            None => DaemonMessage::response_err(id, "unavailable", "read unavailable"),
        })
        .await;
        let WorkInput::WindowsReply {
            generation: got,
            result,
        } = guarded(driver.recv()).await
        else {
            panic!("terminal failure")
        };
        assert_eq!(got, generation);
        assert!(result.is_err());
        driver.shutdown();
        owner.shutdown();
        rig.stop().await;
    }
    let (tx, rx) = mpsc::unbounded_channel();
    drop(rx);
    let control = ControlHandle::new(tx);
    let mut owner = DecisionController::new(control.clone(), None);
    let mut driver = WorkController::new(control, None);
    let (generation, _) = generations();
    let _ = driver.execute(vec![WorkEffect::RequestWindows(generation)], &mut owner);
    assert!(
        matches!(guarded(driver.recv()).await, WorkInput::WindowsReply {generation: got, result: Err(_)} if got == generation)
    );
    assert!(matches!(
        driver.execute(
            vec![WorkEffect::Watch {
                cursor: 41,
                topics: vec!["task.".into()]
            }],
            &mut owner
        ),
        Some(WorkInput::WatchFailed)
    ));
    driver.shutdown();
}

// Break caught: missing capabilities start unsupported polling, or unavailable overwrites a good cache.
#[tokio::test]
async fn work_driver_capabilities_and_failed_cache_fallback() {
    let (rig, mut peer) = Rig::start(&["session.windows"]).await;
    let _cancel = rig.cancel.clone().drop_guard();
    let mut owner = rig.controller();
    let mut driver = driver(&rig);
    let mut state = WorkState::default();
    let _ = driver.execute(
        state.reduce(
            WorkInput::Capabilities {
                methods: vec![],
                watch: false,
            },
            Duration::ZERO,
        ),
        &mut owner,
    );
    let _ = driver.execute(state.reduce(WorkInput::Shown, Duration::ZERO), &mut owner);
    assert!(state.needs_newer_daemon());
    pending_once(driver.recv()).await;
    peer.fence(&rig.commands).await;
    let _ = driver.execute(
        state.reduce(
            WorkInput::Capabilities {
                methods: vec!["session.windows".into()],
                watch: false,
            },
            Duration::ZERO,
        ),
        &mut owner,
    );
    pending_once(driver.recv()).await;
    peer.reply("session.windows", windows()).await;
    let effects = state.reduce(guarded(driver.recv()).await, Duration::ZERO);
    let _ = driver.execute(effects, &mut owner);
    assert!(state.no_org_data());
    let _ = driver.execute(
        state.reduce(WorkInput::Refresh, Duration::from_secs(1)),
        &mut owner,
    );
    pending_once(driver.recv()).await;
    peer.reply(
        "session.windows",
        json!({"state":"unavailable","reason":"tmux down","windows":[]}),
    )
    .await;
    let effects = state.reduce(guarded(driver.recv()).await, Duration::from_secs(1));
    let _ = driver.execute(effects, &mut owner);
    assert_eq!(state.windows().first().unwrap().name, "support");
    assert_eq!(state.stale_age(Duration::from_secs(10)), None);
    assert_eq!(
        state.stale_age(Duration::from_secs(11)),
        Some(Duration::from_secs(11))
    );
    driver.shutdown();
    owner.shutdown();
    rig.stop().await;
}

// Break caught: startup never opts into real capability notices, or Work needs a manual w to read.
#[tokio::test]
async fn startup_capabilities_follow_existing_decision_opt_in() {
    use atm_tui::{
        app::{App, AppView},
        control::ControlNotice,
    };
    for supported in [true, false] {
        let methods = if supported {
            vec!["session.windows", "org.tree"]
        } else {
            vec!["org.tree"]
        };
        let (mut rig, mut peer) = Rig::start(&methods).await;
        let _cancel = rig.cancel.clone().drop_guard();
        let mut owner = rig.controller();
        let mut driver = driver(&rig);
        owner.refresh();
        pending_once(owner.recv()).await;
        let _held = peer.request("org.tree").await;
        let notice = loop {
            if let Event::Control(notice @ ControlNotice::Capabilities { .. }) =
                guarded(rig._events.recv()).await.unwrap()
            {
                break notice;
            }
        };
        owner.notice(&notice);
        let ControlNotice::Capabilities {
            protocol_version,
            methods,
        } = notice
        else {
            unreachable!()
        };
        let mut app = App::new();
        let effects = app.reduce_work(
            WorkInput::Capabilities {
                methods,
                watch: protocol_version.minor >= 1,
            },
            driver.now(),
        );
        assert_eq!(
            app.view,
            if supported {
                AppView::Work
            } else {
                AppView::Sessions
            }
        );
        assert_eq!(
            effects
                .iter()
                .filter(|effect| matches!(
                    effect,
                    WorkEffect::RequestWindows(_) | WorkEffect::RequestOrg(_)
                ))
                .count(),
            if supported { 2 } else { 0 }
        );
        let _ = driver.execute(effects, &mut owner);
        pending_once(driver.recv()).await;
        if supported {
            let _reads = pair(&mut peer).await;
        } else {
            peer.fence(&rig.commands).await;
        }
        driver.shutdown();
        owner.shutdown();
        rig.stop().await;
    }
}

// Break caught: same/invalid numbered selections disarm the driver-owned timer while reads are held.
#[tokio::test(start_paused = true)]
async fn numbered_noops_keep_the_owned_work_timer_armed() {
    use atm_tui::{
        app::App,
        keybinding::{InputHandler, UiAction},
    };
    let (commands, mut requests) = mpsc::unbounded_channel();
    let control = ControlHandle::new(commands);
    let mut owner = DecisionController::new(control.clone(), None);
    let mut driver = WorkController::new(control, None);
    let mut app = App::new();
    let effects = app.reduce_work(
        WorkInput::Capabilities {
            methods: vec!["session.windows".into(), "org.tree".into()],
            watch: false,
        },
        driver.now(),
    );
    let _ = driver.execute(effects, &mut owner);
    pending_once(driver.recv()).await;
    let mut held = Vec::new();
    for _ in 0..2 {
        held.push(guarded(requests.recv()).await.unwrap());
    }
    for keys in ["1gt", "3gt"] {
        let mut handler = InputHandler::new();
        let mut action = None;
        for c in keys.chars() {
            action = handler.handle(key(KeyCode::Char(c)));
        }
        let Some(UiAction::GoToTab(number)) = action else {
            panic!("expected numbered tab")
        };
        if let Some(effects) = app.select_tab(number, driver.now()) {
            let _ = driver.execute(effects, &mut owner);
        }
    }
    tokio::time::advance(Duration::from_secs(3)).await;
    assert!(matches!(guarded(driver.recv()).await, WorkInput::Wake));
    driver.shutdown();
    owner.shutdown();
    drop(held);
}

// Break caught: gw changes tabs/selection/filter, opens another flight, or disarms the owned wake.
#[tokio::test(start_paused = true)]
async fn gw_keeps_tab_selection_filter_and_work_polling() {
    use atm_tui::{
        app::{App, AppView},
        keybinding::{InputHandler, UiAction},
        work::WorkGrouping,
    };
    let (commands, mut requests) = mpsc::unbounded_channel();
    let control = ControlHandle::new(commands);
    let mut owner = DecisionController::new(control.clone(), None);
    let mut driver = WorkController::new(control, None);
    let mut app = App::new();
    app.update_sessions(vec![SessionView {
        id: "selected".into(),
        id_short: "selected".into(),
        tmux_pane: Some("%7".into()),
        ..Default::default()
    }]);
    app.start_search();
    for c in "selected".chars() {
        app.push_search_char(c);
    }
    app.confirm_search();
    app.select_go_to(1);
    let effects = app.reduce_work(
        WorkInput::Capabilities {
            methods: vec!["session.windows".into(), "org.tree".into()],
            watch: false,
        },
        driver.now(),
    );
    let _ = driver.execute(effects, &mut owner);
    pending_once(driver.recv()).await;
    let held = [
        guarded(requests.recv()).await.unwrap(),
        guarded(requests.recv()).await.unwrap(),
    ];
    for grouping in [WorkGrouping::Windows, WorkGrouping::Threads] {
        let mut handler = InputHandler::new();
        assert!(handler.handle(key(KeyCode::Char('g'))).is_none());
        assert_eq!(
            handler.handle(key(KeyCode::Char('w'))),
            Some(UiAction::ToggleWorkGrouping)
        );
        let effects = app.toggle_work_grouping(driver.now()).unwrap();
        assert_eq!(effects, [WorkEffect::WakeAt(Duration::from_secs(3))]);
        let _ = driver.execute(effects, &mut owner);
        assert_eq!(app.work_grouping, grouping);
        assert_eq!(app.view, AppView::Work);
        assert_eq!(app.filter_query, "selected");
        assert_eq!(
            app.selected_session().unwrap().tmux_pane.as_deref(),
            Some("%7")
        );
        assert!(requests.try_recv().is_err());
    }
    tokio::time::advance(Duration::from_secs(3)).await;
    assert!(matches!(guarded(driver.recv()).await, WorkInput::Wake));
    let _ = app.toggle_work_view(driver.now());
    assert!(app.toggle_work_grouping(driver.now()).is_none());
    driver.shutdown();
    owner.shutdown();
    drop(held);
}

// Break caught: a dropped controller retains an RPC/timer and admits work after its owner exits.
struct StdoutRestore(i32);
impl Drop for StdoutRestore {
    fn drop(&mut self) {
        unsafe {
            libc::dup2(self.0, libc::STDOUT_FILENO);
            libc::close(self.0);
        }
    }
}

// Break caught: numbered tab actions are parsed but never dispatched, or change session identity.
#[tokio::test]
async fn numbered_gt_selects_exact_tab_without_canceling_wake() {
    if std::env::var_os("ATM_TAB_LOOP_CHILD").is_none() {
        let result = guarded(tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "tui_decisions_tests::tui_work_tests::numbered_gt_selects_exact_tab_without_canceling_wake", "--nocapture"])
            .env("ATM_TAB_LOOP_CHILD", "1").kill_on_drop(true).output()).await.unwrap();
        assert!(
            result.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        return;
    }
    use atm_tui::app::{App, AppView};
    use std::os::fd::AsRawFd;
    tokio::time::pause();
    let saved = unsafe { libc::dup(libc::STDOUT_FILENO) };
    assert!(saved >= 0);
    let restore = StdoutRestore(saved);
    let sink = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/null")
        .unwrap();
    assert_eq!(
        unsafe { libc::dup2(sink.as_raw_fd(), libc::STDOUT_FILENO) },
        libc::STDOUT_FILENO
    );
    for compact in [false, true] {
        for (keys, before, after) in [
            ("2gt", AppView::Work, AppView::Sessions),
            ("1gt", AppView::Sessions, AppView::Work),
            ("1gt", AppView::Work, AppView::Work),
            ("2gt", AppView::Sessions, AppView::Sessions),
            ("3gt", AppView::Work, AppView::Work),
            ("99999gt", AppView::Work, AppView::Work),
            ("gw", AppView::Work, AppView::Work),
            ("gwgw", AppView::Work, AppView::Work),
            ("gw", AppView::Sessions, AppView::Sessions),
        ] {
            let mut app = App::new();
            app.view = before;
            app.compact = compact;
            app.update_sessions(vec![SessionView {
                id: "tab-seat".into(),
                id_short: "tab-seat".into(),
                tmux_pane: Some("%7".into()),
                project_root: Some("/fixtures/tabs".into()),
                ..Default::default()
            }]);
            app.select_go_to(1);
            let mut terminal = Terminal::with_options(
                ratatui::backend::CrosstermBackend::new(std::io::stdout()),
                ratatui::TerminalOptions {
                    viewport: ratatui::Viewport::Fixed(ratatui::layout::Rect::new(0, 0, 100, 24)),
                },
            )
            .unwrap();
            let (commands, _requests) = mpsc::unbounded_channel();
            let control = ControlHandle::new(commands.clone());
            let (input, mut events) = mpsc::unbounded_channel();
            for c in keys.chars().chain(std::iter::once('q')) {
                input.send(Event::Key(key(KeyCode::Char(c)))).unwrap();
            }
            let (capture, _) = tokio::sync::watch::channel(None);
            let (beads, _) = tokio::sync::watch::channel(None);
            guarded(crate::tui::run_event_loop(
                &mut terminal,
                &mut app,
                &mut events,
                &commands,
                &control,
                &CancellationToken::new(),
                &capture,
                &beads,
            ))
            .await
            .unwrap();
            assert_eq!(app.view, after, "{keys}, compact={compact}");
            assert_eq!(
                app.work_grouping,
                if keys == "gw" && before == AppView::Work {
                    atm_tui::work::WorkGrouping::Windows
                } else {
                    atm_tui::work::WorkGrouping::Threads
                }
            );
            assert_eq!(app.selected_session().unwrap().id.as_str(), "tab-seat");
        }
    }
    drop(restore);
}

// Each cursor-hide write is one completed non-editing frame in ratatui 0.29.
// Calibrate against Terminal::get_frame().count(), never use screen bytes as synchronization.
fn replay_draws(file: &std::fs::File) -> usize {
    use std::os::unix::fs::FileExt;
    let mut bytes = vec![0; usize::try_from(file.metadata().unwrap().len()).unwrap()];
    file.read_exact_at(&mut bytes, 0).unwrap();
    bytes
        .windows(b"\x1b[?25l".len())
        .filter(|w| *w == b"\x1b[?25l")
        .count()
}

async fn replay_draw_sample(recorded: &[DaemonMessage], burst: u64, ahead: u64) -> Vec<Value> {
    use atm_tui::{app::App, control::ControlNotice};
    use std::os::fd::AsRawFd;
    let seq = |frame: &DaemonMessage| match frame {
        DaemonMessage::Event { seq, .. } => *seq,
        _ => panic!("not a recorded event"),
    };
    let old = seq(recorded.first().unwrap()) - 1;
    let mut methods = METHODS.to_vec();
    methods.push("session.windows");
    let (mut rig, mut peer) = Rig::start(&methods).await;
    let capture = tempfile::tempfile().unwrap();
    let saved = unsafe { libc::dup(libc::STDOUT_FILENO) };
    assert!(saved >= 0);
    let restore = StdoutRestore(saved);
    assert_eq!(
        unsafe { libc::dup2(capture.as_raw_fd(), libc::STDOUT_FILENO) },
        libc::STDOUT_FILENO
    );
    let mut terminal = Terminal::with_options(
        ratatui::backend::CrosstermBackend::new(std::io::stdout()),
        ratatui::TerminalOptions {
            viewport: ratatui::Viewport::Fixed(ratatui::layout::Rect::new(0, 0, 100, 24)),
        },
    )
    .unwrap();
    let mut source = std::mem::replace(&mut rig._events, mpsc::unbounded_channel().1);
    let (input_tx, mut events) = mpsc::unbounded_channel();
    let control = ControlHandle::new(rig.commands.clone());
    let mut app = App::new();
    app.view = atm_tui::app::AppView::Sessions;
    let cancel = CancellationToken::new();
    let (capture_tx, _) = tokio::sync::watch::channel(None);
    let (beads_tx, _) = tokio::sync::watch::channel(None);
    let result = {
        let script = async {
            let mut frame = Vec::new();
            let mut initial = None;
            let mut initial_fence = false;
            let mut capabilities = false;
            let mut startup_watches = 0;
            let mut round = 0u64;
            let rounds = if ahead == 0 { 2 } else { 1 };
            let mut org_reads = 0;
            let mut start_draws = 0;
            let mut decision_floor = old;
            let mut cursor = old;
            let mut replayed = 0u64;
            let mut tail = 0u64;
            let mut awaiting_fence = false;
            let mut metrics = Vec::new();
            loop {
                tokio::select! {
                    count = peer.reader.read_until(b'\n',&mut frame) => {
                        assert_ne!(count.unwrap(),0);
                        let message=serde_json::from_slice::<ClientMessage>(&frame).unwrap().message;frame.clear();
                        match message {
                            MessageType::Request {id,method,..} => {
                                let value = match method.as_str() {
                                    "org.tree" if !initial_fence => {assert!(initial.replace(id).is_none());continue;}
                                    "org.tree" => {
                                        let mut org=tree();
                                        let fresh = if round==0 {old} else {
                                            org_reads+=1;
                                            old+burst*round+if org_reads>1 {ahead} else {0}
                                        };
                                        org["seq"]=json!(fresh);org
                                    }
                                    "decision.list" => page(vec![],None),
                                    "session.windows" => windows(),
                                    _ => panic!("unexpected replay method {method}"),
                                };
                                peer.send(DaemonMessage::response_ok(id,value)).await;
                            }
                            MessageType::Unwatch => {},
                            MessageType::Ping {seq} => peer.send(DaemonMessage::Pong {seq}).await,
                            MessageType::Watch {topics,cursor:installed} => {
                                if topics.iter().any(|p|p.starts_with("task.")) {
                                    cursor=installed;
                                    let fresh=old+burst*round;
                                    for event in recorded {
                                        let event_seq=seq(event);
                                        if event_seq>installed && event_seq<=fresh+1 {
                                            let mut event=event.clone();
                                            // Synthetic lifecycle tail; accounting backlog need not reach Work.
                                            if event_seq==fresh+1 {
                                                if let DaemonMessage::Event {kind,body,..}=&mut event {
                                                    *kind="task.assigned".into();
                                                    *body=json!({});
                                                }
                                            }
                                            peer.send(event).await;
                                        }
                                    }
                                    peer.send(DaemonMessage::discovery_complete(u32::try_from(round).unwrap(),u32::MAX)).await;
                                } else {
                                    decision_floor=installed;
                                    if round==0 {
                                        startup_watches+=1;
                                        if startup_watches<2 {continue;}
                                        round=1;
                                    } else {
                                        assert_eq!(round,1);round=2;
                                    }
                                    org_reads=0;replayed=0;tail=0;
                                    start_draws=replay_draws(&capture);
                                    input_tx.send(Event::Key(key(KeyCode::Char('w')))).unwrap();
                                }
                            }
                            MessageType::Discover if round==0 => {
                                initial_fence=true;
                                let id=initial.take().unwrap();
                                let mut org=tree();org["seq"]=json!(old);
                                peer.send(DaemonMessage::response_ok(id,org)).await;
                            }
                            MessageType::Discover => {
                                assert!(awaiting_fence);awaiting_fence=false;
                                let end_draws=replay_draws(&capture);
                                metrics.push(json!({"join":round,"burst":burst,"decision_ahead":ahead,
                                    "decision_floor_before":decision_floor,"work_floor":old+burst*round,
                                    "installed_cursor":cursor,"covered_replay":replayed,"needed_tail":tail,
                                    "frames_start":start_draws,"frames_end":end_draws,"actual_draws":end_draws-start_draws}));
                                if round==rounds {input_tx.send(Event::Key(key(KeyCode::Char('q')))).unwrap();break;}
                                input_tx.send(Event::Key(key(KeyCode::Char('w')))).unwrap();
                            }
                            other => panic!("unexpected replay frame {other:?}"),
                        }
                    }
                    event = source.recv() => {
                        let event=event.unwrap();
                        match &event {
                            Event::Control(ControlNotice::Capabilities {..}) if !capabilities => {
                                capabilities=true;
                                input_tx.send(event).unwrap();
                                // Force initial capability invalidation before replying to org.tree.
                                input_tx.send(Event::Key(key(KeyCode::Char('r')))).unwrap();
                                continue;
                            }
                            Event::Control(ControlNotice::Event(DaemonMessage::Event {seq,kind,..})) if kind.starts_with("task.") => {
                                if *seq<=old+burst*round {replayed+=1;} else {tail+=1;}
                            }
                            Event::DiscoveryComplete {discovered,failed} if *failed==u32::MAX => {
                                assert_eq!(u64::from(*discovered),round);
                                input_tx.send(event).unwrap();
                                input_tx.send(Event::Key(key(KeyCode::Char('r')))).unwrap();
                                awaiting_fence=true;
                                continue;
                            }
                            Event::DaemonDisconnected(reason) => panic!("replay disconnected: {reason:?}"),
                            _ => {}
                        }
                        input_tx.send(event).unwrap();
                    }
                }
            }
            metrics
        };
        let proof = async {
            let (result, metrics) = tokio::join!(
                crate::tui::run_event_loop(
                    &mut terminal,
                    &mut app,
                    &mut events,
                    &rig.commands,
                    &control,
                    &cancel,
                    &capture_tx,
                    &beads_tx
                ),
                script
            );
            result.unwrap();
            metrics
        };
        tokio::pin!(proof);
        std::future::poll_fn(|cx| {
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| proof.as_mut().poll(cx)))
            {
                Ok(std::task::Poll::Ready(metrics)) => std::task::Poll::Ready(Ok(metrics)),
                Ok(std::task::Poll::Pending) => std::task::Poll::Pending,
                Err(error) => std::task::Poll::Ready(Err(error)),
            }
        })
        .await
    };
    let frames = terminal.get_frame().count();
    let counted = replay_draws(&capture);
    drop(terminal);
    drop(restore);
    rig.stop().await;
    assert_eq!(
        counted, frames,
        "stdout draw count must match actual ratatui frame count"
    );
    result.unwrap_or_else(|error| std::panic::resume_unwind(error))
}

// Break caught: Work join adds old covered task frames, or max(cursor) drops the needed tail.
#[tokio::test]
async fn work_join_recorded_replay_and_actual_draws() {
    if std::env::var_os("ATM_WORK_REPLAY_CHILD").is_none() {
        use std::os::unix::fs::FileExt;
        let output = tempfile::tempfile().unwrap();
        let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tui_decisions_tests::tui_work_tests::work_join_recorded_replay_and_actual_draws",
                "--nocapture",
            ])
            .env("ATM_WORK_REPLAY_CHILD", "1")
            .stdout(output.try_clone().unwrap())
            .stderr(output.try_clone().unwrap())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let status = match tokio::time::timeout(Duration::from_secs(600), child.wait()).await {
            Ok(status) => status.unwrap(),
            Err(error) => {
                guarded(child.kill()).await.unwrap();
                guarded(child.wait()).await.unwrap();
                panic!("replay child hang guard: {error}");
            }
        };
        let mut bytes = vec![0; usize::try_from(output.metadata().unwrap().len()).unwrap()];
        output.read_exact_at(&mut bytes, 0).unwrap();
        assert!(status.success(), "{}", String::from_utf8_lossy(&bytes));
        return;
    }
    let burst = std::env::var("ATM_WORK_REPLAY_BURST")
        .ok()
        .map(|n| n.parse::<u64>().unwrap())
        .unwrap_or(1024);
    assert!((1..=25_000).contains(&burst));
    let path = std::env::var_os("ATM_WORK_REPLAY_LOG").map(std::path::PathBuf::from);
    let recorded: Vec<DaemonMessage> = if let Some(path) = path.as_ref().filter(|p| p.exists()) {
        tokio::fs::read_to_string(path)
            .await
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    } else {
        let frames =
            real_daemon::recorded_work_usage_log(usize::try_from(burst * 2 + 1).unwrap()).await;
        if let Some(path) = &path {
            let mut text = String::new();
            for frame in &frames {
                text.push_str(&serde_json::to_string(frame).unwrap());
                text.push('\n');
            }
            tokio::fs::write(path, text).await.unwrap();
        }
        frames
    };
    assert_eq!(recorded.len(), usize::try_from(burst * 2 + 1).unwrap());
    let mut metrics = Vec::new();
    for ahead in [0, 5] {
        metrics.extend(
            tokio::time::timeout(
                Duration::from_secs(300),
                replay_draw_sample(&recorded, burst, ahead),
            )
            .await
            .unwrap(),
        );
    }
    for row in &metrics {
        eprintln!("WORK_REPLAY_MEASURE {row}");
    }
    if let Some(path) = std::env::var_os("ATM_WORK_REPLAY_METRICS") {
        tokio::fs::write(path, serde_json::to_vec_pretty(&metrics).unwrap())
            .await
            .unwrap();
    }
    for row in &metrics {
        assert_eq!(
            row["needed_tail"], 1,
            "needed post-snapshot event was lost: {row}"
        );
        assert_eq!(
            row["covered_replay"], 0,
            "old task replay on Work join: {row}"
        );
        assert!(row["actual_draws"].as_u64().unwrap() >= row["needed_tail"].as_u64().unwrap());
    }
}

// Break caught: w/r/modal/session events block on Work reads, or quit/EOF/render error retains them.
#[tokio::test]
async fn work_event_loop_keeps_input_live_and_cancels_reads() {
    if std::env::var_os("ATM_WORK_LOOP_CHILD").is_none() {
        let result = guarded(tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "tui_decisions_tests::tui_work_tests::work_event_loop_keeps_input_live_and_cancels_reads", "--nocapture"])
            .env("ATM_WORK_LOOP_CHILD", "1").kill_on_drop(true).output()).await.unwrap();
        assert!(
            result.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        return;
    }
    use std::os::fd::AsRawFd;
    // No socket/process I/O under this paused clock: only channels and synchronous stdio.
    tokio::time::pause();
    for exit in ["quit", "closed", "render"] {
        let saved = unsafe { libc::dup(libc::STDOUT_FILENO) };
        assert!(saved >= 0);
        let restore = StdoutRestore(saved);
        let full = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/full")
            .unwrap();
        let mut terminal = Terminal::with_options(
            ratatui::backend::CrosstermBackend::new(std::io::stdout()),
            ratatui::TerminalOptions {
                viewport: ratatui::Viewport::Fixed(ratatui::layout::Rect::new(0, 0, 100, 24)),
            },
        )
        .unwrap();
        let (commands, mut requests) = mpsc::unbounded_channel();
        let control = ControlHandle::new(commands.clone());
        let (input, mut events) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let mut app = atm_tui::app::App::new();
        app.view = atm_tui::app::AppView::Sessions;
        app.update_sessions(vec![SessionView {
            id: "main".into(),
            id_short: "main".into(),
            tmux_pane: Some("%7".into()),
            project_root: Some("/fixtures/project".into()),
            ..Default::default()
        }]);
        app.select_go_to(1);
        let (capture, _) = tokio::sync::watch::channel(None);
        let (beads, _) = tokio::sync::watch::channel(None);
        let input_key = |c| Event::Key(key(KeyCode::Char(c)));
        input.send(input_key('w')).unwrap();
        let script = async {
            let mut queued = Vec::new();
            for _ in 0..3 {
                queued.push(guarded(requests.recv()).await.unwrap());
            }
            let mut methods: Vec<_> = queued
                .iter()
                .map(|command| {
                    let text = format!("{command:?}");
                    text.split_once("method: \"")
                        .unwrap()
                        .1
                        .split_once('"')
                        .unwrap()
                        .0
                        .to_owned()
                })
                .collect();
            methods.sort();
            assert_eq!(methods, ["org.tree", "org.tree", "session.windows"]);
            // Tab/group keys are text/ignored inside search/help/decision modals.
            for c in "?gtgT1gtwgw?/gtgT1gtwgw".chars() {
                input.send(input_key(c)).unwrap();
            }
            input.send(Event::Key(key(KeyCode::Esc))).unwrap();
            for c in "agtgT1gtwgw".chars() {
                input.send(input_key(c)).unwrap();
            }
            input.send(Event::Key(key(KeyCode::Esc))).unwrap();
            input.send(input_key('r')).unwrap();
            assert!(matches!(
                guarded(requests.recv()).await.unwrap(),
                ClientCommand::Discover
            ));
            input
                .send(Event::SessionUpdate(vec![SessionView {
                    id: "main".into(),
                    id_short: "main".into(),
                    tmux_pane: Some("%8".into()),
                    project_root: Some("/fixtures/project".into()),
                    ..Default::default()
                }]))
                .unwrap();
            for c in ['w', 'w', 'r'] {
                input.send(input_key(c)).unwrap();
            }
            assert!(matches!(
                guarded(requests.recv()).await.unwrap(),
                ClientCommand::Discover
            ));
            match exit {
                "quit" => input.send(input_key('q')).unwrap(),
                "closed" => drop(input),
                "render" => {
                    assert_eq!(
                        unsafe { libc::dup2(full.as_raw_fd(), libc::STDOUT_FILENO) },
                        libc::STDOUT_FILENO
                    );
                    input.send(Event::Resize(100, 24)).unwrap();
                }
                _ => panic!("unknown exit proof"),
            }
            queued
        };
        let (result, queued) = guarded(async {
            tokio::join!(
                crate::tui::run_event_loop(
                    &mut terminal,
                    &mut app,
                    &mut events,
                    &commands,
                    &control,
                    &cancel,
                    &capture,
                    &beads
                ),
                script
            )
        })
        .await;
        drop(restore);
        assert_eq!(result.is_err(), exit == "render");
        assert_eq!(app.view, atm_tui::app::AppView::Work);
        assert_eq!(app.work_grouping, atm_tui::work::WorkGrouping::Threads);
        assert_eq!(app.selected_session().unwrap().id.as_str(), "main");
        assert_eq!(
            app.selected_session().unwrap().tmux_pane.as_deref(),
            Some("%8")
        );
        let stopped = tokio::time::Instant::now();
        drop(control);
        drop(commands);
        while let Some(command) = guarded(requests.recv()).await {
            assert_eq!(
                format!("{command:?}"),
                "Control(ControlCommand { input: Unwatch, reply: None })"
            );
        }
        assert_eq!(
            tokio::time::Instant::now(),
            stopped,
            "owned reads must not live until their deadlines"
        );
        drop(queued);
    }
}

async fn loop_handshake(listener: &UnixListener, sessions: Vec<SessionView>) -> Peer {
    let (stream, _) = guarded(listener.accept()).await.unwrap();
    let (reader, writer) = stream.into_split();
    let mut peer = Peer {
        reader: BufReader::new(reader),
        writer,
        watches: vec![],
    };
    assert!(matches!(peer.recv().await, MessageType::Connect { .. }));
    peer.send(DaemonMessage::Connected {
        protocol_version: ProtocolVersion::new(1, 1),
        client_id: "work-loop".into(),
        methods: vec![
            "org.tree".into(),
            "decision.list".into(),
            "session.windows".into(),
        ],
    })
    .await;
    assert!(matches!(peer.recv().await, MessageType::Subscribe { .. }));
    peer.send(DaemonMessage::session_list(sessions)).await;
    peer
}
async fn loop_watch(peer: &mut Peer, seq: u64, work: bool) {
    loop {
        match peer.raw().await {
            MessageType::Request { id, method, .. } => {
                let value = match method.as_str() {
                    "org.tree" => {
                        let mut value = tree();
                        value["seq"] = json!(seq);
                        // Synthetic unnamed session: keep window recovery visible at narrow list widths.
                        value["projects"][0]["seats"][0]["pane"] = Value::Null;
                        value
                    }
                    "decision.list" => page(vec![], None),
                    "session.windows" => {
                        let mut value = windows();
                        if seq == 100 {
                            value["windows"][0]["name"] = json!("recovered");
                        }
                        value
                    }
                    _ => panic!("unexpected loop request {method}"),
                };
                peer.send(DaemonMessage::response_ok(id, value)).await;
            }
            MessageType::Watch { topics, cursor }
                if cursor == seq
                    && topics.iter().any(|t| t == "task.created") == work
                    && topics.iter().any(|t| t == "project.") =>
            {
                assert!(topics.iter().any(|t| t == "decision."));
                assert!(topics.iter().any(|t| t == "project."));
                assert!(!topics.iter().any(|t| t == "task."));
                if work {
                    assert!(topics.iter().any(|t| t == "msg.delivery_failed"));
                }
                return;
            }
            MessageType::Ping { seq } => peer.send(DaemonMessage::Pong { seq }).await,
            MessageType::Watch { .. } | MessageType::Unwatch | MessageType::Discover => {}
            other => panic!("unexpected loop frame {other:?}"),
        }
    }
}

// Break caught: loop disconnect/capability dispatch loses Work interest, its old read slots, or selection.
#[tokio::test]
async fn work_event_loop_disconnect_reconnect_keeps_work_visible() {
    use tokio::io::AsyncReadExt;
    if std::env::var_os("ATM_WORK_RECONNECT_CHILD").is_none() {
        use std::process::Stdio;
        let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "tui_decisions_tests::tui_work_tests::work_event_loop_disconnect_reconnect_keeps_work_visible", "--nocapture"])
            .env("ATM_WORK_RECONNECT_CHILD", "1").stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true).spawn().unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let out = tokio::spawn(async move {
            let mut bytes = vec![];
            guarded(stdout.read_to_end(&mut bytes)).await.unwrap();
            bytes
        });
        let err = tokio::spawn(async move {
            let mut bytes = vec![];
            guarded(stderr.read_to_end(&mut bytes)).await.unwrap();
            bytes
        });
        let status = tokio::time::timeout(Duration::from_secs(60), child.wait()).await;
        if status.is_err() {
            let _ = guarded(child.kill()).await;
            let _ = guarded(child.wait()).await;
        }
        let output = guarded(out).await.unwrap();
        let errors = guarded(err).await.unwrap();
        assert!(
            status.unwrap().unwrap().success(),
            "{}\n{}",
            String::from_utf8_lossy(&output),
            String::from_utf8_lossy(&errors)
        );
        return;
    }
    use std::os::fd::AsRawFd;
    let mut terminal = Terminal::with_options(
        ratatui::backend::CrosstermBackend::new(std::io::stdout()),
        ratatui::TerminalOptions {
            viewport: ratatui::Viewport::Fixed(ratatui::layout::Rect::new(0, 0, 100, 24)),
        },
    )
    .unwrap();
    // A socketpair observes actual rendering: quit cannot race the windows reply.
    // This child runs exactly one test, so stdout redirection is process-local.
    let saved = unsafe { libc::dup(libc::STDOUT_FILENO) };
    assert!(saved >= 0);
    let restore = StdoutRestore(saved);
    let (reader, writer) = std::os::unix::net::UnixStream::pair().unwrap();
    reader.set_nonblocking(true).unwrap();
    let mut screen = tokio::net::UnixStream::from_std(reader).unwrap();
    assert_eq!(
        unsafe { libc::dup2(writer.as_raw_fd(), libc::STDOUT_FILENO) },
        libc::STDOUT_FILENO
    );
    drop(writer);
    let (rendered, render_ready) = tokio::sync::oneshot::channel();
    let capture = tokio::spawn(async move {
        let mut rendered = Some(rendered);
        let mut tail = Vec::new();
        let mut chunk = [0; 4096];
        loop {
            let n = guarded(screen.read(&mut chunk)).await.unwrap();
            if n == 0 {
                break;
            }
            if rendered.is_some() {
                tail.extend_from_slice(&chunk[..n]);
                if tail.windows(b"recovered".len()).any(|s| s == b"recovered") {
                    if let Some(tx) = rendered.take() {
                        let _ = tx.send(());
                    }
                } else if tail.len() > 8192 {
                    tail.drain(..4096);
                }
            }
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("loop.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let (commands, command_rx) = mpsc::unbounded_channel();
    let control = ControlHandle::new(commands.clone());
    let (client_events, mut client_rx) = mpsc::unbounded_channel();
    let (input, mut events) = mpsc::unbounded_channel();
    let cancel = CancellationToken::new();
    let _cancel_on_drop = cancel.clone().drop_guard();
    let client = DaemonClient::new(
        DaemonConfig {
            socket_path: path,
            ..Default::default()
        },
        client_events,
        command_rx,
        cancel.clone(),
    );
    let client_run = tokio::spawn(async move { client.run().await });
    let negative = std::env::var_os("ATM_WORK_DROP_RECONNECT_CAPS").is_some();
    let bridge_cancel = cancel.clone();
    let forward = input.clone();
    let bridge = tokio::spawn(async move {
        let mut capabilities = 0;
        loop {
            tokio::select! {
                () = bridge_cancel.cancelled() => break,
                event = client_rx.recv() => match event {
                    Some(event) => {
                        if matches!(&event, Event::Control(ControlNotice::Capabilities { .. })) {
                            capabilities += 1;
                            if negative && capabilities > 1 { continue; }
                        }
                        if forward.send(event).is_err() { break; }
                    }
                    None => break,
                }
            }
        }
    });
    let session = SessionView {
        id: "selected".into(),
        id_short: "selected".into(),
        tmux_pane: Some("%7".into()),
        project_root: Some("/fixtures/project".into()),
        ..Default::default()
    };
    let mut app = atm_tui::app::App::new();
    app.view = atm_tui::app::AppView::Sessions;
    app.update_sessions(vec![session.clone()]);
    app.select_go_to(1);
    let (capture_target, _) = tokio::sync::watch::channel(None);
    let (beads, _) = tokio::sync::watch::channel(None);
    let script = tokio::spawn(async move {
        let mut peer = loop_handshake(&listener, vec![session.clone()]).await;
        loop_watch(&mut peer, 41, false).await;
        input.send(Event::Key(key(KeyCode::Char('w')))).unwrap();
        loop_watch(&mut peer, 41, true).await;
        input.send(Event::Key(key(KeyCode::Char('r')))).unwrap();
        let mut held = Vec::new();
        while held.len() < 2 {
            match peer.recv().await {
                MessageType::Request { method, .. } => held.push(method),
                MessageType::Discover => {}
                other => panic!("expected held reads: {other:?}"),
            }
        }
        held.sort();
        assert_eq!(held, ["org.tree", "session.windows"]);
        drop(peer);
        let mut peer = loop_handshake(&listener, vec![session]).await;
        if !negative {
            loop_watch(&mut peer, 100, true).await;
            guarded(render_ready).await.unwrap();
        }
        input.send(Event::Key(key(KeyCode::Char('q')))).unwrap();
    });
    let script_cancel = cancel.clone();
    let (result, script_result) = guarded(async {
        tokio::join!(
            crate::tui::run_event_loop(
                &mut terminal,
                &mut app,
                &mut events,
                &commands,
                &control,
                &cancel,
                &capture_target,
                &beads
            ),
            async {
                let result = script.await;
                if result.is_err() {
                    script_cancel.cancel();
                }
                result
            }
        )
    })
    .await;
    cancel.cancel();
    guarded(client_run).await.unwrap();
    guarded(bridge).await.unwrap();
    drop(restore);
    guarded(capture).await.unwrap();
    result.unwrap();
    script_result.unwrap();
    assert_eq!(app.view, atm_tui::app::AppView::Work);
    assert_eq!(app.selected_session().unwrap().id.as_str(), "selected");
    assert_eq!(app.work.org().unwrap().seq, Some(100));
    assert_eq!(app.work.windows().first().unwrap().name, "recovered");
    assert!(!app.work.windows_unavailable() && !app.work.org_unavailable());
}

// Break caught: a slow reconnect queues a cached Work Watch before any fresh org reply.
// Ported from the independent review's held-handshake wire probe.
#[tokio::test]
async fn work_slow_reconnect_never_sends_cached_cursor() {
    use atm_tui::control::ControlNotice;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("slow.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let (commands, command_rx) = mpsc::unbounded_channel();
    let (event_tx, mut events) = mpsc::unbounded_channel();
    let cancel = CancellationToken::new();
    let _cancel_on_drop = cancel.clone().drop_guard();
    let client = DaemonClient::new(
        DaemonConfig {
            socket_path: path,
            ..Default::default()
        },
        event_tx,
        command_rx,
        cancel.clone(),
    );
    let client_run = tokio::spawn(async move { client.run().await });
    let control = ControlHandle::new(commands.clone());
    let mut owner = DecisionController::new(control.clone(), None);
    let mut driver = WorkController::new(control, None);
    let result = {
        let proof = guarded(async {
            let mut state = WorkState::default();
            let _ = state.reduce(
                WorkInput::Capabilities {
                    methods: vec!["session.windows".into(), "org.tree".into()],
                    watch: true,
                },
                Duration::ZERO,
            );
            let shown = state.reduce(WorkInput::Shown, Duration::ZERO);
            let org_id = shown
                .iter()
                .find_map(|e| match e {
                    WorkEffect::RequestOrg(id) => Some(*id),
                    _ => None,
                })
                .unwrap();
            let windows_id = shown
                .iter()
                .find_map(|e| match e {
                    WorkEffect::RequestWindows(id) => Some(*id),
                    _ => None,
                })
                .unwrap();
            let _ = state.reduce(
                WorkInput::WindowsReply {
                    generation: windows_id,
                    result: Ok(serde_json::from_value(windows()).unwrap()),
                },
                Duration::ZERO,
            );
            let initial = state.reduce(
                WorkInput::OrgReply {
                    generation: org_id,
                    result: Ok(serde_json::from_value(tree()).unwrap()),
                },
                Duration::ZERO,
            );
            let mut peer = loop_handshake(&listener, vec![]).await;
            assert!(driver.execute(initial, &mut owner).is_none());
            peer.watch_at(41).await;
            peer.fence(&commands).await;
            drop(peer);
            loop {
                if matches!(
                    guarded(events.recv()).await.unwrap(),
                    Event::DaemonDisconnected(_)
                ) {
                    break;
                }
            }
            owner.disconnected();
            let failed = state.reduce(WorkInput::WatchFailed, Duration::from_secs(1));
            assert!(driver.execute(failed, &mut owner).is_none());
            // Hold Connected: Work time can pass without the new socket's capabilities.
            let (stream, _) = guarded(listener.accept()).await.unwrap();
            let (reader, writer) = stream.into_split();
            let mut peer = Peer {
                reader: BufReader::new(reader),
                writer,
                watches: vec![],
            };
            assert!(matches!(peer.recv().await, MessageType::Connect { .. }));
            let due = state.reduce(WorkInput::Wake, Duration::from_secs(11));
            eprintln!("slow reconnect t=11 effects: {due:?}");
            assert!(driver.execute(due, &mut owner).is_none());
            pending_once(driver.recv()).await; // Admit retry reads while the handshake is still held.
            commands.send(ClientCommand::Discover).unwrap();
            peer.send(DaemonMessage::Connected {
                protocol_version: ProtocolVersion::new(1, 1),
                client_id: "slow-reconnect".into(),
                methods: vec![
                    "session.windows".into(),
                    "org.tree".into(),
                    "decision.list".into(),
                ],
            })
            .await;
            assert!(matches!(peer.recv().await, MessageType::Subscribe { .. }));
            peer.send(DaemonMessage::session_list(vec![])).await;
            // The loop notifies the shared Watch owner before reducing Work's replies.
            loop {
                if let Event::Control(notice @ ControlNotice::Capabilities { .. }) =
                    guarded(events.recv()).await.unwrap()
                {
                    owner.notice(&notice);
                    break;
                }
            }
            let mut fresh_org = None;
            let mut reads = Vec::new();
            loop {
                match peer.recv().await {
                    MessageType::Request { id, method, .. } => {
                        if method == "org.tree" {
                            fresh_org = Some(id);
                        } else {
                            peer.send(DaemonMessage::response_ok(id, windows())).await;
                        }
                        reads.push(method);
                    }
                    MessageType::Discover => break,
                    other => panic!("expected retry reads and command fence: {other:?}"),
                }
            }
            let observed = peer.watches.clone();
            eprintln!("new socket Watches before fresh org reply: {observed:?}");
            let mut fresh_watch = false;
            if observed.is_empty() {
                if let Some(id) = fresh_org {
                    let mut value = tree();
                    value["seq"] = json!(90);
                    peer.send(DaemonMessage::response_ok(id, value)).await;
                    for _ in 0..2 {
                        let input = guarded(driver.recv()).await;
                        let effects = state.reduce(input, Duration::from_secs(12));
                        assert!(driver.execute(effects, &mut owner).is_none());
                    }
                    // Service however many owner projections the join requires, not a fixed read count.
                    let mut projection_applied = false;
                    let mut frame = Vec::new();
                    while !fresh_watch || !projection_applied {
                        tokio::select! {
                            update = owner.recv() => {
                                owner.apply(update.unwrap());
                                projection_applied = true;
                            }
                            count = peer.reader.read_until(b'\n', &mut frame) => {
                                assert_ne!(count.unwrap(), 0);
                                let message = serde_json::from_slice::<ClientMessage>(&frame).unwrap().message;
                                frame.clear(); // Retain partial frames when the owner update wins select.
                                match message {
                                    MessageType::Request { id, method, .. } => {
                                        let value = match method.as_str() {
                                            "org.tree" => { let mut value = tree(); value["seq"] = json!(90); value }
                                            "decision.list" => page(vec![], None),
                                            _ => panic!("unexpected owner projection method {method}"),
                                        };
                                        peer.send(DaemonMessage::response_ok(id, value)).await;
                                    }
                                    MessageType::Watch { topics, cursor } => {
                                        fresh_watch |= cursor == 90;
                                        peer.watches.push((topics, cursor));
                                    }
                                    MessageType::Unwatch => {}
                                    MessageType::Ping { seq } => peer.send(DaemonMessage::Pong { seq }).await,
                                    other => panic!("expected fresh Watch or owner projection: {other:?}"),
                                }
                            }
                        }
                    }
                    eprintln!("new socket Watch after fresh org reply: {:?}", peer.watches);
                }
            }
            drop(peer);
            assert!(
                observed.is_empty(),
                "cached pre-reconnect Work cursor escaped: {observed:?}"
            );
            reads.sort();
            assert_eq!(reads, ["org.tree", "session.windows"]);
            assert!(
                fresh_watch,
                "only the fresh org reply may install the retry Watch"
            );
        });
        tokio::pin!(proof);
        std::future::poll_fn(|cx| {
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| proof.as_mut().poll(cx)))
            {
                Ok(Poll::Ready(())) => Poll::Ready(Ok(())),
                Ok(Poll::Pending) => Poll::Pending,
                Err(error) => Poll::Ready(Err(error)),
            }
        })
        .await
    };
    driver.shutdown();
    owner.shutdown();
    cancel.cancel();
    guarded(client_run).await.unwrap();
    result.unwrap_or_else(|error| std::panic::resume_unwind(error));
}

#[tokio::test]
async fn work_driver_shutdown_and_drop_cancel_owned_reads() {
    for explicit in [false, true] {
        let (rig, mut peer) = Rig::start(&["session.windows", "org.tree"]).await;
        let _cancel = rig.cancel.clone().drop_guard();
        let mut owner = rig.controller();
        let mut driver = driver(&rig);
        let (windows, org) = generations();
        let _ = driver.execute(
            vec![
                WorkEffect::RequestWindows(windows),
                WorkEffect::RequestOrg(org),
                WorkEffect::WakeAt(Duration::from_secs(3)),
            ],
            &mut owner,
        );
        pending_once(driver.recv()).await;
        let reads = pair(&mut peer).await;
        if explicit {
            driver.shutdown();
            pending_once(driver.recv()).await;
        }
        drop(driver);
        reply_pair(&mut peer, reads).await;
        peer.fence(&rig.commands).await;
        owner.shutdown();
        rig.stop().await;
    }
}
