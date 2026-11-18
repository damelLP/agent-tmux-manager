//! Driver proofs reuse the real VW1 client/private peer; clock-only proofs use channels.
use super::*;
use crate::tui_work::WorkController;
use atm_tui::work::{Generation, WorkEffect, WorkInput, WorkState};
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
            // w is text/ignored inside search/help/decision modals, not another view toggle.
            for c in ['?', 'w', '?', '/', 'w'] {
                input.send(input_key(c)).unwrap();
            }
            input.send(Event::Key(key(KeyCode::Esc))).unwrap();
            input.send(input_key('a')).unwrap();
            input.send(input_key('w')).unwrap();
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
