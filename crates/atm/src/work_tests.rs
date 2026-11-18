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
            "task.".into(),
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
                kind: "task.changed".into(),
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
    let tick = state.reduce(WorkInput::Wake, at(120));
    let org_id = requested(&tick, false);
    assert!(!state
        .reduce(org_reply(org_id, Some(90)), at(120))
        .iter()
        .any(|e| matches!(e, WorkEffect::Watch { .. })));
    capabilities(&mut state, 121, &["session.windows"]);
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
