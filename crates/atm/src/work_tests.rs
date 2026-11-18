use super::*;
use atm_core::{all_node_ids, flatten_tree, SessionId, SessionView, TreeNode, TreeRowKind};
use serde_json::json;
use std::time::Duration;

fn at(seconds: u64) -> Duration {
    Duration::from_secs(seconds)
}
fn capabilities(state: &mut WorkState, seconds: u64, methods: &[&str]) -> Vec<WorkEffect> {
    state.reduce(
        WorkInput::Capabilities {
            methods: methods.iter().map(|m| (*m).into()).collect(),
            watch: true,
        },
        at(seconds),
    )
}
fn requested(effects: &[WorkEffect], windows: bool) -> Generation {
    effects
        .iter()
        .find_map(|effect| match effect {
            WorkEffect::RequestWindows(id) if windows => Some(*id),
            WorkEffect::RequestOrg(id) if !windows => Some(*id),
            _ => None,
        })
        .expect("request")
}
fn window_reply(id: Generation, state: &str) -> WorkInput {
    WorkInput::WindowsReply {
        generation: id,
        result: Ok(WorkWindows {
            state: state.into(),
            windows: windows(),
        }),
    }
}
fn org_reply(id: Generation, seq: Option<u64>) -> WorkInput {
    let mut value = org();
    value.seq = seq;
    WorkInput::OrgReply {
        generation: id,
        result: Ok(value),
    }
}
fn loaded() -> WorkState {
    let mut state = WorkState::default();
    capabilities(&mut state, 0, &["session.windows", "org.tree"]);
    let shown = state.reduce(WorkInput::Shown, at(0));
    let _ = state.reduce(window_reply(requested(&shown, true), "ok"), at(0));
    let _ = state.reduce(org_reply(requested(&shown, false), Some(42)), at(0));
    state
}
fn request_count(effects: &[WorkEffect]) -> usize {
    effects
        .iter()
        .filter(|e| matches!(e, WorkEffect::RequestWindows(_) | WorkEffect::RequestOrg(_)))
        .count()
}
// Break caught: hidden traffic, parallel reads, missing pane refreshes, or wrong 3 s cadence.
#[test]
fn work_reduce_visible_polling_and_inflight_triggers_coalesce() {
    let mut state = WorkState::default();
    assert!(capabilities(&mut state, 0, &["session.windows", "org.tree"]).is_empty());
    for input in [WorkInput::Wake, WorkInput::Refresh, WorkInput::PanesChanged] {
        assert!(state.reduce(input, at(0)).is_empty());
    }
    let shown = state.reduce(WorkInput::Shown, at(0));
    let w = requested(&shown, true);
    let o = requested(&shown, false);
    assert_eq!(
        shown,
        vec![
            WorkEffect::RequestWindows(w),
            WorkEffect::RequestOrg(o),
            WorkEffect::WakeAt(at(3))
        ]
    );
    assert_eq!(
        state.reduce(WorkInput::Shown, at(1)),
        vec![WorkEffect::WakeAt(at(3))]
    );
    for _ in 0..5 {
        assert_eq!(
            request_count(&state.reduce(WorkInput::PanesChanged, at(1))),
            0
        );
    }
    let completed = state.reduce(window_reply(w, "ok"), at(2));
    let second = requested(&completed, true);
    assert_ne!(w, second);
    assert_eq!(request_count(&completed), 1);
    assert_eq!(
        request_count(&state.reduce(window_reply(second, "ok"), at(2))),
        0
    );
    let org_effects = state.reduce(org_reply(o, Some(42)), at(2));
    assert!(org_effects.contains(&WorkEffect::Watch {
        cursor: 42,
        topics: vec![
            "task.created".into(),
            "task.assigned".into(),
            "task.state_changed".into(),
            "task.submitted".into(),
            "task.review_started".into(),
            "task.verdict".into(),
            "decision.".into(),
            "agent.".into(),
            "msg.delivery_failed".into()
        ]
    }));
    assert_eq!(request_count(&state.reduce(WorkInput::Wake, at(4))), 0);
    let poll = state.reduce(WorkInput::Wake, at(5));
    assert_eq!(request_count(&poll), 1);
    assert!(poll
        .iter()
        .any(|e| matches!(e, WorkEffect::RequestWindows(_))));
    let hidden = state.reduce(WorkInput::Hidden, at(5));
    assert_eq!(hidden, vec![WorkEffect::Unwatch]);
    for input in [
        WorkInput::Wake,
        WorkInput::Refresh,
        WorkInput::PanesChanged,
        WorkInput::WatchEvent {
            seq: 99,
            kind: "task.updated".into(),
        },
    ] {
        assert!(state.reduce(input, at(1000)).is_empty());
    }
}
// Break caught: unthrottled event storms, re-reading covered events, or losing a dirty follow-up.
#[test]
fn work_reduce_org_events_throttle_to_five_seconds_and_fallback_at_sixty() {
    let mut state = loaded();
    assert_eq!(
        request_count(&state.reduce(
            WorkInput::WatchEvent {
                seq: 42,
                kind: "task.created".into()
            },
            at(1)
        )),
        0
    );
    assert_eq!(
        request_count(&state.reduce(
            WorkInput::WatchEvent {
                seq: 50,
                kind: "unrelated".into()
            },
            at(1)
        )),
        0
    );
    for (seq, kind) in [
        (43, "task.created"),
        (44, "decision.changed"),
        (45, "agent.changed"),
        (46, "msg.delivery_failed"),
    ] {
        assert_eq!(
            request_count(&state.reduce(
                WorkInput::WatchEvent {
                    seq,
                    kind: kind.into()
                },
                at(2)
            )),
            0
        );
    }
    let due = state.reduce(WorkInput::Wake, at(5));
    let org_id = requested(&due, false);
    assert_eq!(
        due.iter()
            .filter(|e| matches!(e, WorkEffect::RequestOrg(_)))
            .count(),
        1
    );
    for seq in 47..60 {
        let _ = state.reduce(
            WorkInput::WatchEvent {
                seq,
                kind: "task.assigned".into(),
            },
            at(6),
        );
    }
    let reply = state.reduce(org_reply(org_id, Some(46)), at(7));
    assert!(!reply.iter().any(|e| matches!(e, WorkEffect::RequestOrg(_))));
    let next = state.reduce(WorkInput::Wake, at(10));
    let org_id = requested(&next, false);
    let _ = state.reduce(org_reply(org_id, Some(100)), at(10));
    assert!(!state
        .reduce(WorkInput::Wake, at(69))
        .iter()
        .any(|e| matches!(e, WorkEffect::RequestOrg(_))));
    assert!(state
        .reduce(WorkInput::Wake, at(70))
        .iter()
        .any(|e| matches!(e, WorkEffect::RequestOrg(_))));
}
// Break caught: a late hidden/old-generation reply mutates data or starts extra requests after reopen.
#[test]
fn work_reduce_hide_reopen_and_reconnect_reject_obsolete_replies() {
    let mut state = WorkState::default();
    capabilities(&mut state, 0, &["session.windows", "org.tree"]);
    let first = state.reduce(WorkInput::Shown, at(0));
    let old = requested(&first, true);
    let old_org = requested(&first, false);
    let _ = state.reduce(WorkInput::Hidden, at(1));
    assert_eq!(
        request_count(&state.reduce(WorkInput::Shown, at(2))),
        0,
        "old flights still retained"
    );
    let next = state.reduce(window_reply(old, "ok"), at(2));
    let current = requested(&next, true);
    assert!(state.windows().is_empty());
    let next_org = state.reduce(org_reply(old_org, Some(42)), at(2));
    let current_org = requested(&next_org, false);
    assert!(state.org().is_none());
    assert!(!next_org
        .iter()
        .any(|e| matches!(e, WorkEffect::Watch { .. })));
    let _ = state.reduce(window_reply(old, "ok"), at(3));
    assert!(state.windows().is_empty());
    let _ = state.reduce(window_reply(current, "ok"), at(3));
    assert_eq!(state.windows().len(), 2);
    capabilities(&mut state, 4, &["session.windows", "org.tree"]);
    let retry = state.reduce(org_reply(current_org, Some(42)), at(4));
    assert!(retry.iter().any(|e| matches!(e, WorkEffect::RequestOrg(_))));
    assert!(state.org().is_none());
}
// Break caught: unavailable/failed reads overwrite good data or reset freshness timestamps.
#[test]
fn work_reduce_failures_keep_last_good_values_and_exact_stale_boundaries() {
    let mut state = loaded();
    assert_eq!(state.stale_age(at(10)), None);
    assert_eq!(state.stale_age(at(11)), Some(at(11)));
    let refresh = state.reduce(WorkInput::Refresh, at(11));
    let w = requested(&refresh, true);
    let o = requested(&refresh, false);
    let _ = state.reduce(window_reply(w, "unavailable"), at(12));
    let _ = state.reduce(
        WorkInput::OrgReply {
            generation: o,
            result: Err(WorkFailure("timeout".into())),
        },
        at(12),
    );
    assert_eq!(state.windows().len(), 2);
    assert_eq!(state.org().unwrap().seq, Some(42));
    assert_eq!(state.stale_age(at(12)), Some(at(12)));
    let refresh = state.reduce(WorkInput::Refresh, at(180));
    let w = requested(&refresh, true);
    let o = requested(&refresh, false);
    let _ = state.reduce(window_reply(w, "ok"), at(180));
    let _ = state.reduce(
        WorkInput::OrgReply {
            generation: o,
            result: Err(WorkFailure("offline".into())),
        },
        at(180),
    );
    assert_eq!(state.stale_age(at(180)), None);
    assert_eq!(state.stale_age(at(181)), Some(at(181)));
    // A fresh successful org read clears the org staleness without a wall-clock read.
    let refresh = state.reduce(WorkInput::Refresh, at(181));
    let o = requested(&refresh, false);
    let _ = state.reduce(org_reply(o, Some(100)), at(181));
    assert_eq!(state.stale_age(at(181)), None);
}
// Break caught: unsupported methods still poll, old seq is invented, or watch failure spins retries.
#[test]
fn work_reduce_capabilities_missing_cursor_and_watch_failure_degrade_without_sessions_changes() {
    let mut older = WorkState::default();
    capabilities(&mut older, 0, &["org.tree"]);
    assert!(older.reduce(WorkInput::Shown, at(0)).is_empty());
    assert!(older.needs_newer_daemon());
    let mut no_org = WorkState::default();
    capabilities(&mut no_org, 0, &["session.windows"]);
    let show = no_org.reduce(WorkInput::Shown, at(0));
    assert!(no_org.no_org_data());
    assert_eq!(request_count(&show), 1);
    assert!(!show.iter().any(|e| matches!(e, WorkEffect::RequestOrg(_))));
    let mut state = WorkState::default();
    capabilities(&mut state, 0, &["session.windows", "org.tree"]);
    let shown = state.reduce(WorkInput::Shown, at(0));
    let _ = state.reduce(window_reply(requested(&shown, true), "ok"), at(0));
    assert!(!state
        .reduce(org_reply(requested(&shown, false), None), at(0))
        .iter()
        .any(|e| matches!(e, WorkEffect::Watch { .. })));
    let tick = state.reduce(WorkInput::Wake, at(60));
    let org_id = requested(&tick, false);
    assert!(state
        .reduce(org_reply(org_id, Some(80)), at(60))
        .iter()
        .any(|e| matches!(e, WorkEffect::Watch { cursor: 80, .. })));
    assert!(state
        .reduce(WorkInput::WatchFailed, at(61))
        .contains(&WorkEffect::Unwatch));
    let retry = state.reduce(WorkInput::Wake, at(71));
    assert!(!has_watch(&retry, 80));
    assert!(has_watch(
        &state.reduce(org_reply(requested(&retry, false), Some(90)), at(71)),
        90
    ));
    let tick = state.reduce(WorkInput::Wake, at(131));
    let org_id = requested(&tick, false);
    assert!(!state
        .reduce(org_reply(org_id, Some(100)), at(131))
        .iter()
        .any(|e| matches!(e, WorkEffect::Watch { .. })));
    capabilities(&mut state, 132, &["session.windows"]);
    assert!(state.org().is_none());
    assert!(state.no_org_data());
    // Before the first capability notice, Shown bootstraps the opt-in control transport.
    let mut bootstrap = WorkState::default();
    assert_eq!(request_count(&bootstrap.reduce(WorkInput::Shown, at(0))), 2);
}

fn session(id: &str, pane: Option<&str>, parent: Option<&str>, time: &str) -> SessionView {
    SessionView {
        id: SessionId::new(id),
        tmux_pane: pane.map(Into::into),
        parent_session_id: parent.map(SessionId::new),
        started_at: time.into(),
        ..Default::default()
    }
}
fn windows() -> Vec<WorkWindow> {
    serde_json::from_value(json!([
        {"id":"@2","name":"other","links":[{"session":"beta","index":2}],"panes":["%2"]},
        {"id":"@0","name":"support","links":[{"session":"zeta","index":7},{"session":"alpha","index":0}],"panes":["%0","%1"]}
    ])).unwrap()
}
fn placement(tree: &[TreeNode]) -> Vec<(String, Vec<(String, u8)>)> {
    tree.iter()
        .map(|node| {
            let TreeNode::Window { id, .. } = node else {
                panic!("not a window")
            };
            (
                id.clone(),
                flatten_tree(
                    std::slice::from_ref(node),
                    &all_node_ids(std::slice::from_ref(node)),
                )
                .into_iter()
                .filter_map(|row| match row.kind {
                    TreeRowKind::Agent { session } => Some((session.id.to_string(), row.depth)),
                    _ => None,
                })
                .collect(),
            )
        })
        .collect()
}
// Break caught: a cross-window child jumps into the wrong group, links duplicate rows,
// paneless descendants vanish, or the existing agent ordering/SessionView data is changed.
#[test]
fn work_tree_places_each_agent_once_where_its_pane_lives() {
    let sessions = vec![
        session("parent", Some("%0"), None, "1"),
        session("same", Some("%1"), Some("parent"), "2"),
        session("other", Some("%2"), Some("parent"), "3"),
        session("nested", None, Some("other"), "4"),
        session("unknown", Some("%404"), Some("parent"), "5"),
        session("orphan", None, Some("missing"), "6"),
        session("new", Some("%0"), None, "7"),
        session("paneless", None, Some("parent"), "8"),
    ];
    let tree = build_work_tree(&sessions, &windows());
    assert_eq!(
        placement(&tree),
        vec![
            (
                "@0".into(),
                vec![
                    ("new".into(), 1),
                    ("parent".into(), 1),
                    ("same".into(), 2),
                    ("paneless".into(), 2)
                ]
            ),
            ("@2".into(), vec![("other".into(), 1), ("nested".into(), 2)]),
            (
                "__no_window__".into(),
                vec![("orphan".into(), 1), ("unknown".into(), 1)]
            ),
        ]
    );
    assert!(matches!(&tree[0],TreeNode::Window{label,..} if label=="0 support"));
    let rows = flatten_tree(&tree, &all_node_ids(&tree));
    let other = rows
        .iter()
        .find_map(|row| match &row.kind {
            TreeRowKind::Agent { session } if session.id.as_str() == "other" => Some(session),
            _ => None,
        })
        .unwrap();
    assert_eq!(other.parent_session_id, Some(SessionId::new("parent")));
    assert_eq!(other.tmux_pane.as_deref(), Some("%2"));
    assert_eq!(
        rows.iter()
            .filter(|row| matches!(row.kind, TreeRowKind::Agent { .. }))
            .count(),
        sessions.len()
    );
    assert!(build_work_tree(&[], &windows()).is_empty());
}
// Break caught: project/worktree grouping leaks into Work, or invalid parent cycles disappear/recurse.
#[test]
fn work_tree_flattens_git_groups_and_retains_cyclic_and_unloaded_rows() {
    let mut a = session("a", Some("%0"), Some("b"), "1");
    a.project_root = Some("/alpha".into());
    let mut b = session("b", Some("%0"), Some("a"), "2");
    b.project_root = Some("/beta".into());
    let c = session("c", None, Some("b"), "3");
    let d = session("d", Some("%0"), None, "4");
    let all = vec![a, b, c, d];
    for panes in [windows(), vec![]] {
        let tree = build_work_tree(&all, &panes);
        let rows = flatten_tree(&tree, &all_node_ids(&tree));
        let mut ids = rows
            .iter()
            .filter_map(|row| match &row.kind {
                TreeRowKind::Agent { session } => Some(session.id.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        ids.sort();
        assert_eq!(ids, ["a", "b", "c", "d"]);
        for original in &all {
            let projected = rows
                .iter()
                .find_map(|row| match &row.kind {
                    TreeRowKind::Agent { session } if session.id == original.id => Some(session),
                    _ => None,
                })
                .unwrap();
            assert_eq!(projected.parent_session_id, original.parent_session_id);
        }
        assert!(
            matches!(&tree[0],TreeNode::Window{label,..} if if panes.is_empty(){label=="(no window)"} else {label=="0 support"})
        );
    }
}
fn org() -> WorkOrg {
    serde_json::from_value(json!({
        "seq":42,"unused_additive":true,
        "projects":[{"id":"alpha","seats":[
            {"agent_id":"alpha/watcher","pane":"%0","tasks":["t1","t2","t99"],"future":true},
            {"agent_id":"alpha/worker","pane":"%2","tasks":["t3"]},
            {"agent_id":"alpha/paneless","pane":null,"tasks":["t4"]}
        ],"tasks":[{"task_id":"t1","title":"First task"},{"task_id":"t3","title":"Other work"}]}],
        "needs_owner":{"items":[
            {"kind":"decision_pending","id":"d1","agent_id":"alpha/watcher"},
            {"kind":"decision_provisional","id":"d2","agent_id":"alpha/watcher"},
            {"kind":"seat_stuck","id":"alpha/watcher","agent_id":"alpha/watcher"},
            {"kind":"seat_attention","id":"alpha/watcher","agent_id":"alpha/watcher"},
            {"kind":"delivery_failed","id":"m1","agent_id":"alpha/worker"},
            {"kind":"line_broken","id":"alpha/worker","agent_id":"alpha/worker"},
            {"kind":"decision_pending","id":"d3","agent_id":null},
            {"kind":"future_kind","id":"x","agent_id":"alpha/worker"}
        ]}
    }))
    .unwrap()
}
// Break caught: decision ownership joined by project not seat; title caps undercount tasks;
// multiple linked session entries count a window's seat twice; unrelated items invent marks.
#[test]
fn work_marks_join_seats_and_tasks_by_identity_and_aggregate_each_pane_once() {
    let marks = work_marks(&org(), &windows());
    let watch = marks.by_pane.get(&PaneId("%0".into())).unwrap();
    assert_eq!(watch.name, "watcher");
    assert_eq!(watch.agent_id, SeatId("alpha/watcher".into()));
    assert_eq!(watch.tasks_open, 3);
    assert_eq!(watch.decisions_waiting, 2);
    assert!(watch.attention);
    let task = watch.first_task.as_ref().unwrap();
    assert_eq!(task.id, TaskId("t1".into()));
    assert_eq!(task.title.as_deref(), Some("First task"));
    assert_eq!(
        marks.by_window[&WindowId("@0".into())],
        WindowMarks {
            tasks_open: 3,
            decisions_waiting: 2,
            attention: true
        }
    );
    assert_eq!(
        marks.by_window[&WindowId("@2".into())],
        WindowMarks {
            tasks_open: 1,
            decisions_waiting: 0,
            attention: true
        }
    );
    assert!(!marks.by_pane.contains_key(&PaneId("%404".into())));
    let mut data = org();
    data.projects[0].seats[0].tasks = vec![TaskId("t99".into()), TaskId("t1".into())];
    let marks = work_marks(&data, &windows());
    let first = marks.by_pane[&PaneId("%0".into())]
        .first_task
        .as_ref()
        .unwrap();
    assert_eq!(first.id, TaskId("t99".into()));
    assert_eq!(first.title, None);
}
// Break caught: accounting/metadata events dirty org polling even through a wider union.
#[test]
fn work_reduce_usage_and_cost_do_not_dirty_org() {
    for kind in [
        "task.usage_observed",
        "task.cost_recorded",
        "task.attached",
        "task.reminded",
        "task.flagged",
    ] {
        let mut state = loaded();
        let _ = state.reduce(
            WorkInput::WatchEvent {
                seq: 43,
                kind: kind.into(),
            },
            at(1),
        );
        assert!(
            !state
                .reduce(WorkInput::Wake, at(5))
                .iter()
                .any(|e| matches!(e, WorkEffect::RequestOrg(_))),
            "{kind}"
        );
    }
}

// Break caught: the daemon still receives a broad task topic and forwards usage noise.
#[test]
fn work_reduce_watch_topics_exclude_accounting_and_metadata() {
    let mut state = WorkState::default();
    capabilities(&mut state, 0, &["session.windows", "org.tree"]);
    let shown = state.reduce(WorkInput::Shown, at(0));
    let reply = state.reduce(org_reply(requested(&shown, false), Some(42)), at(0));
    let topics = reply
        .iter()
        .find_map(|e| match e {
            WorkEffect::Watch { topics, .. } => Some(topics),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        topics,
        &[
            "task.created",
            "task.assigned",
            "task.state_changed",
            "task.submitted",
            "task.review_started",
            "task.verdict",
            "decision.",
            "agent.",
            "msg.delivery_failed"
        ]
    );
    // Every retained task kind still invalidates current marks; covered events do not.
    for kind in topics.iter().filter(|t| t.starts_with("task.")) {
        let mut state = loaded();
        let _ = state.reduce(
            WorkInput::WatchEvent {
                seq: 42,
                kind: kind.clone(),
            },
            at(1),
        );
        assert!(!state
            .reduce(WorkInput::Wake, at(2))
            .iter()
            .any(|e| matches!(e, WorkEffect::RequestOrg(_))));
        let _ = state.reduce(
            WorkInput::WatchEvent {
                seq: 43,
                kind: kind.clone(),
            },
            at(2),
        );
        assert!(
            state
                .reduce(WorkInput::Wake, at(5))
                .iter()
                .any(|e| matches!(e, WorkEffect::RequestOrg(_))),
            "{kind}"
        );
    }
}

// Break caught: a busy seat's accounting stream turns the minute fallback into 5 s reads.
#[test]
fn work_reduce_busy_usage_keeps_minute_fallback() {
    let mut state = loaded();
    let mut org_counts = [0; 2];
    let mut window_counts = [0; 2];
    for second in 1..=120 {
        let minute = if second <= 60 { 0 } else { 1 };
        for input in [
            WorkInput::WatchEvent {
                seq: 42 + second,
                kind: "task.usage_observed".into(),
            },
            WorkInput::Wake,
        ] {
            let effects = state.reduce(input, at(second));
            for effect in effects {
                match effect {
                    WorkEffect::RequestOrg(id) => {
                        org_counts[minute] += 1;
                        let _ = state.reduce(org_reply(id, Some(42 + second)), at(second));
                    }
                    WorkEffect::RequestWindows(id) => {
                        window_counts[minute] += 1;
                        let _ = state.reduce(window_reply(id, "ok"), at(second));
                    }
                    _ => {}
                }
            }
        }
    }
    assert_eq!(org_counts, [1, 1]);
    assert_eq!(window_counts, [20, 20]);
}

fn has_watch(effects: &[WorkEffect], cursor: u64) -> bool {
    effects
        .iter()
        .any(|e| matches!(e, WorkEffect::Watch { cursor: seq, .. } if *seq == cursor))
}

// Break caught: WatchFailed permanently latches Work off, or retry spins before its deadline.
#[test]
fn work_reduce_watch_failure_retries_after_ten_seconds() {
    let mut state = loaded();
    assert!(state
        .reduce(WorkInput::WatchFailed, at(1))
        .contains(&WorkEffect::Unwatch));
    let before = state.reduce(WorkInput::Wake, at(10));
    assert!(!has_watch(&before, 42));
    assert!(before.contains(&WorkEffect::WakeAt(at(11))));
    let retry = state.reduce(WorkInput::Wake, at(11));
    assert!(!has_watch(&retry, 42));
    assert!(has_watch(
        &state.reduce(org_reply(requested(&retry, false), Some(50)), at(11)),
        50
    ));
    assert!(!has_watch(&state.reduce(WorkInput::Wake, at(11)), 50));
    let _ = state.reduce(WorkInput::WatchFailed, at(11));
    assert!(!has_watch(&state.reduce(WorkInput::Wake, at(20)), 50));
    let retry = state.reduce(WorkInput::Wake, at(21));
    assert!(!has_watch(&retry, 50));
    assert!(has_watch(
        &state.reduce(org_reply(requested(&retry, false), Some(60)), at(21)),
        60
    ));
    // A pre-failure fallback completion cannot bypass the delay or supply the retry cursor.
    let mut state = loaded();
    let due = state.reduce(WorkInput::Wake, at(60));
    let _ = state.reduce(WorkInput::WatchFailed, at(61));
    assert!(!has_watch(
        &state.reduce(org_reply(requested(&due, false), Some(80)), at(62)),
        80
    ));
    let retry = state.reduce(WorkInput::Wake, at(71));
    assert!(!has_watch(&retry, 80));
    assert!(has_watch(
        &state.reduce(org_reply(requested(&retry, false), Some(90)), at(72)),
        90
    ));
}

// Break caught: a pre-failure flight or cached cursor installs Watch during slow reconnect.
#[test]
fn work_reduce_failed_watch_requires_a_post_failure_org_read() {
    let mut state = loaded();
    let before_failure = state.reduce(WorkInput::Refresh, at(1));
    let old_org = requested(&before_failure, false);
    let _ = state.reduce(WorkInput::WatchFailed, at(1));
    let old_reply = state.reduce(org_reply(old_org, Some(60)), at(5));
    assert!(!old_reply
        .iter()
        .any(|e| matches!(e, WorkEffect::Watch { .. })));
    let retry = state.reduce(WorkInput::Wake, at(11));
    assert!(
        !retry.iter().any(|e| matches!(e, WorkEffect::Watch { .. })),
        "retry must read before watching: {retry:?}"
    );
    let fresh_org = requested(&retry, false);
    assert!(has_watch(
        &state.reduce(org_reply(fresh_org, Some(90)), at(12)),
        90
    ));
    assert!(!has_watch(&state.reduce(WorkInput::Wake, at(12)), 90));
}

// Break caught: the first capabilities after a failed bootstrap watch installs its cached cursor.
#[test]
fn work_reduce_first_capabilities_after_failure_also_requires_fresh_org() {
    let mut state = WorkState::default();
    let shown = state.reduce(WorkInput::Shown, at(0));
    let _ = state.reduce(window_reply(requested(&shown, true), "ok"), at(0));
    let _ = state.reduce(org_reply(requested(&shown, false), Some(42)), at(0));
    let _ = state.reduce(WorkInput::WatchFailed, at(1));
    let caps = capabilities(&mut state, 2, &["session.windows", "org.tree"]);
    assert!(!has_watch(&caps, 42));
    assert!(has_watch(
        &state.reduce(org_reply(requested(&caps, false), Some(90)), at(3)),
        90
    ));
}

// Break caught: an occupied old read makes the retry deadline spin, or a failed retry loses recovery.
#[test]
fn work_reduce_watch_retry_waits_for_retained_flight_and_retries_read_errors() {
    let mut state = loaded();
    let initial = state.reduce(WorkInput::Refresh, at(1));
    let _ = state.reduce(WorkInput::WatchFailed, at(1));
    let blocked = state.reduce(WorkInput::Wake, at(11));
    assert!(!blocked
        .iter()
        .any(|e| matches!(e, WorkEffect::Watch { .. } | WorkEffect::RequestOrg(_))));
    assert!(!blocked
        .iter()
        .any(|e| matches!(e, WorkEffect::WakeAt(t) if *t <= at(11))));
    let released = state.reduce(org_reply(requested(&initial, false), Some(60)), at(12));
    assert_eq!(
        state.org().unwrap().seq,
        Some(42),
        "obsolete flight cannot refresh the watch floor"
    );
    assert!(!has_watch(&released, 60));
    let retry = requested(&released, false);
    let failure = state.reduce(
        WorkInput::OrgReply {
            generation: retry,
            result: Err(WorkFailure("offline".into())),
        },
        at(13),
    );
    assert!(!failure
        .iter()
        .any(|e| matches!(e, WorkEffect::Watch { .. } | WorkEffect::RequestOrg(_))));
    let early = state.reduce(WorkInput::Wake, at(22));
    assert!(!early.iter().any(|e| matches!(e, WorkEffect::RequestOrg(_))));
    assert!(early.contains(&WorkEffect::WakeAt(at(23))));
    let retry = state.reduce(WorkInput::Wake, at(23));
    assert!(!has_watch(&retry, 42));
    assert!(has_watch(
        &state.reduce(org_reply(requested(&retry, false), Some(90)), at(24)),
        90
    ));
}

// Break caught: an early post-failure event read supplies a cached cursor instead of the due retry read.
#[test]
fn work_reduce_early_event_read_cannot_replace_the_retry_read() {
    let mut state = loaded();
    let _ = state.reduce(WorkInput::WatchFailed, at(1));
    let _ = state.reduce(
        WorkInput::WatchEvent {
            seq: 43,
            kind: "task.assigned".into(),
        },
        at(2),
    );
    let early = state.reduce(WorkInput::Wake, at(5));
    let reply = state.reduce(org_reply(requested(&early, false), Some(60)), at(6));
    assert!(!has_watch(&reply, 60));
    let retry = state.reduce(WorkInput::Wake, at(11));
    assert!(!has_watch(&retry, 60));
    assert!(has_watch(
        &state.reduce(org_reply(requested(&retry, false), Some(90)), at(12)),
        90
    ));
}

// Break caught: ineligible retries spin an overdue wake, or reconnect watches pre-reconnect data.
#[test]
fn work_reduce_watch_retry_disarms_when_ineligible() {
    for mode in ["hidden", "windows", "org", "watch", "cursor"] {
        let mut state = loaded();
        let _ = state.reduce(WorkInput::WatchFailed, at(1));
        match mode {
            "hidden" => {
                let _ = state.reduce(WorkInput::Hidden, at(2));
            }
            "windows" => {
                capabilities(&mut state, 2, &["org.tree"]);
            }
            "org" => {
                capabilities(&mut state, 2, &["session.windows"]);
            }
            "watch" => {
                let _ = state.reduce(
                    WorkInput::Capabilities {
                        methods: vec!["session.windows".into(), "org.tree".into()],
                        watch: false,
                    },
                    at(2),
                );
            }
            "cursor" => {
                let due = state.reduce(WorkInput::Wake, at(60));
                let _ = state.reduce(org_reply(requested(&due, false), None), at(60));
                let _ = state.reduce(WorkInput::WatchFailed, at(61));
            }
            _ => unreachable!(),
        }
        for second in [100, 101] {
            let effects = state.reduce(WorkInput::Wake, at(second));
            assert!(
                !effects
                    .iter()
                    .any(|e| matches!(e, WorkEffect::Watch { .. })),
                "{mode}"
            );
            assert!(
                !effects
                    .iter()
                    .any(|e| matches!(e, WorkEffect::WakeAt(t) if *t <= at(second))),
                "{mode}: {effects:?}"
            );
        }
    }
    let mut state = loaded();
    let _ = state.reduce(WorkInput::WatchFailed, at(1));
    let reconnect = capabilities(&mut state, 2, &["session.windows", "org.tree"]);
    assert!(!has_watch(&reconnect, 42));
    assert!(!has_watch(&state.reduce(WorkInput::Wake, at(20)), 42));
    assert!(has_watch(
        &state.reduce(org_reply(requested(&reconnect, false), Some(90)), at(20)),
        90
    ));
    for input in [WorkInput::Refresh, WorkInput::Shown] {
        let mut state = loaded();
        let _ = state.reduce(WorkInput::WatchFailed, at(1));
        if matches!(input, WorkInput::Shown) {
            let _ = state.reduce(WorkInput::Hidden, at(2));
        }
        let effects = state.reduce(input, at(2));
        assert!(has_watch(
            &state.reduce(org_reply(requested(&effects, false), Some(91)), at(2)),
            91
        ));
    }
}

// Break caught: failed first reads have no availability signal, or retries hide it before success.
#[test]
fn work_reduce_source_failures_are_visible_until_success() {
    for non_ok in [false, true] {
        let mut state = WorkState::default();
        capabilities(&mut state, 0, &["session.windows", "org.tree"]);
        let initial = state.reduce(WorkInput::Shown, at(0));
        let w = requested(&initial, true);
        let _ = state.reduce(
            if non_ok {
                window_reply(w, "unavailable")
            } else {
                WorkInput::WindowsReply {
                    generation: w,
                    result: Err(WorkFailure("failed".into())),
                }
            },
            at(1),
        );
        let _ = state.reduce(
            WorkInput::OrgReply {
                generation: requested(&initial, false),
                result: Err(WorkFailure("failed".into())),
            },
            at(1),
        );
        assert!(state.windows_unavailable() && state.org_unavailable());
        assert!(state.windows().is_empty() && state.org().is_none());
        assert_eq!(
            state.stale_age(at(1000)),
            None,
            "failure does not invent freshness"
        );
        let retry = state.reduce(WorkInput::Refresh, at(2));
        assert!(state.windows_unavailable() && state.org_unavailable());
        let _ = state.reduce(
            WorkInput::WindowsReply {
                generation: requested(&retry, true),
                result: Ok(WorkWindows {
                    state: "ok".into(),
                    windows: vec![],
                }),
            },
            at(2),
        );
        assert!(!state.windows_unavailable() && state.org_unavailable());
        let _ = state.reduce(org_reply(requested(&retry, false), Some(80)), at(2));
        assert!(!state.windows_unavailable() && !state.org_unavailable());
    }
    let mut state = loaded();
    let refresh = state.reduce(WorkInput::Refresh, at(11));
    let _ = state.reduce(
        window_reply(requested(&refresh, true), "unknown_state"),
        at(12),
    );
    let _ = state.reduce(
        WorkInput::OrgReply {
            generation: requested(&refresh, false),
            result: Err(WorkFailure("failed".into())),
        },
        at(12),
    );
    assert!(state.windows_unavailable() && state.org_unavailable());
    assert_eq!(state.windows().len(), 2);
    assert_eq!(state.org().unwrap().seq, Some(42));
    assert_eq!(state.stale_age(at(12)), Some(at(12)));
}

// Break caught: obsolete replies set/clear status, or unsupported methods look like source failures.
#[test]
fn work_reduce_obsolete_failures_do_not_change_availability() {
    for hidden in [false, true] {
        let mut state = loaded();
        let old = state.reduce(WorkInput::Refresh, at(1));
        if hidden {
            let _ = state.reduce(WorkInput::Hidden, at(2));
        } else {
            capabilities(&mut state, 2, &["session.windows", "org.tree"]);
        }
        let _ = state.reduce(window_reply(requested(&old, true), "unavailable"), at(3));
        let _ = state.reduce(
            WorkInput::OrgReply {
                generation: requested(&old, false),
                result: Err(WorkFailure("obsolete".into())),
            },
            at(3),
        );
        assert!(!state.windows_unavailable() && !state.org_unavailable());
    }
    let mut state = loaded();
    let old = state.reduce(WorkInput::Refresh, at(1));
    let w = requested(&old, true);
    let o = requested(&old, false);
    let _ = state.reduce(window_reply(w, "unavailable"), at(1));
    let _ = state.reduce(
        WorkInput::OrgReply {
            generation: o,
            result: Err(WorkFailure("failed".into())),
        },
        at(1),
    );
    let _ = state.reduce(window_reply(w, "ok"), at(2));
    let _ = state.reduce(org_reply(o, Some(100)), at(2));
    assert!(
        state.windows_unavailable() && state.org_unavailable(),
        "duplicate successes cannot clear failures"
    );
    capabilities(&mut state, 3, &["session.windows"]);
    assert!(state.windows_unavailable() && !state.org_unavailable());
    assert!(state.no_org_data());
    capabilities(&mut state, 4, &[]);
    assert!(!state.windows_unavailable() && !state.org_unavailable());
    assert!(state.needs_newer_daemon());
}

// Break caught: projected badges do not recognize each of the four explicit attention causes.
#[test]
fn work_marks_attention_causes_are_explicit_and_missing_org_cursor_is_accepted() {
    for (kind, want) in [
        ("seat_stuck", true),
        ("seat_attention", true),
        ("delivery_failed", true),
        ("line_broken", true),
        ("decision_pending", false),
        ("future_kind", false),
    ] {
        let data:WorkOrg=serde_json::from_value(json!({"projects":[{"seats":[{"agent_id":"a/seat","pane":"%0","tasks":[]}],"tasks":[]}],"needs_owner":{"items":[{"kind":kind,"id":"one","agent_id":"a/seat"}]}})).unwrap();
        assert_eq!(data.seq, None);
        assert_eq!(
            work_marks(&data, &windows()).by_pane[&PaneId("%0".into())].attention,
            want,
            "{kind}"
        );
    }
}
