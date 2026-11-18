use super::super::reporting_line::ReportingEdges;
use super::project;
use atm_core::{Harness, SessionId, SessionStatus, SessionView};
use atm_org::{
    Address, OrgEvent, OrgState, TaskAssigned, TaskCreated, TaskKind, TaskMeasure, TaskState,
    TaskStateChanged,
};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};

fn at(minute: u32) -> DateTime<Utc> {
    format!("2026-01-15T12:{minute:02}:00Z").parse().unwrap()
}
fn apply(state: &mut OrgState, seq: u64, minute: u32, event: OrgEvent) {
    state.apply(seq, at(minute), &event).unwrap();
}
fn task(title: &str, kind: TaskKind, creator: &str) -> OrgEvent {
    OrgEvent::TaskCreated(TaskCreated {
        project_id: None,
        title: title.into(),
        kind,
        creator: creator.parse().unwrap(),
        parent: None,
        judges: None,
        measure: TaskMeasure::Mechanical {
            command: "SECRET task command".into(),
        },
        budget: None,
        files: vec![atm_org::TaskFile {
            path: "SECRET task file".into(),
            kind: atm_org::TaskFileKind::Brief,
            note: Some("SECRET task note".into()),
        }],
        rests_on: None,
    })
}
fn assign(state: &mut OrgState, seq: u64, id: &str, agent: &str) {
    apply(
        state,
        seq,
        0,
        OrgEvent::TaskAssigned(TaskAssigned {
            task_id: id.parse().unwrap(),
            agent_id: agent.parse().unwrap(),
            by: Address::Owner,
        }),
    );
}
fn task_state(state: &mut OrgState, seq: u64, id: &str, status: TaskState) {
    apply(
        state,
        seq,
        0,
        OrgEvent::TaskStateChanged(TaskStateChanged {
            task_id: id.parse().unwrap(),
            state: status,
            reason: Some("SECRET task reason".into()),
            by: Address::Owner,
        }),
    );
}
fn decision(requester: &str) -> OrgEvent {
    OrgEvent::DecisionRequested(atm_org::DecisionRequest {
        requester: requester.parse().unwrap(),
        kind: atm_org::DecisionKind::Plan,
        title: "SECRET decision title".into(),
        context: "SECRET decision context".into(),
        files: vec!["SECRET decision file".into()],
        options: vec!["SECRET option".into(), "no".into()],
        recommended: Some("SECRET option".into()),
        may_be_undone: true,
        changes_intent: true,
        deadline: None,
    })
}
fn message(to: &str, body: &str) -> OrgEvent {
    OrgEvent::MessageSent(atm_org::MessageContent {
        delivery_notice: None,
        from: Address::System,
        to: to.parse().unwrap(),
        kind: atm_org::MessageKind::Note,
        body: body.into(),
        attachments: vec!["SECRET attachment".into()],
        task: Some("SECRET task reference".parse().unwrap()),
    })
}
fn seeded() -> (OrgState, Vec<SessionView>, ReportingEdges) {
    let mut state = OrgState::default();
    for (seq, id, root) in [
        (1, "gamma", "/fixtures/folders/gamma"),
        (2, "beta", "/fixtures/repos/beta"),
        (3, "alpha", "/fixtures/repos/alpha"),
    ] {
        apply(
            &mut state,
            seq,
            0,
            OrgEvent::ProjectRegistered {
                project_id: id.parse().unwrap(),
                root: root.into(),
            },
        );
    }
    for (seq, id, pane, session) in [
        (4, "alpha/lead", "%11", Some("lead-live")),
        (5, "alpha/watch", "%10", None),
        (6, "alpha/worker", "%12", Some("worker-live")),
        (7, "beta/broken", "%13", Some("broken-live")),
        (8, "beta/lead", "%14", Some("lead-missing")),
        (9, "beta/stalled", "%16", None),
        (10, "gamma/chief", "%15", Some("chief-live")),
    ] {
        apply(
            &mut state,
            seq,
            0,
            OrgEvent::AgentAdopted {
                agent_id: id.parse().unwrap(),
                pane: pane.into(),
                session_id: session.map(SessionId::new),
            },
        );
    }
    for (seq, title, kind, creator) in [
        (11, "Review plan", TaskKind::Work, "owner"),
        (12, "Check output", TaskKind::Review, "alpha/lead"),
        (13, "Gather notes", TaskKind::Errand, "alpha/lead"),
        (14, "Draft next step", TaskKind::Plan, "owner"),
        (15, "Triage queue", TaskKind::Work, "owner"),
    ] {
        apply(&mut state, seq, 0, task(title, kind, creator));
    }
    for (seq, id, agent) in [
        (16, "t11", "alpha/lead"),
        (17, "t12", "alpha/worker"),
        (18, "t13", "alpha/worker"),
        (19, "t14", "beta/lead"),
    ] {
        assign(&mut state, seq, id, agent);
    }
    for (seq, id, status) in [
        (20, "t11", TaskState::Working),
        (21, "t13", TaskState::Waiting),
        (22, "t14", TaskState::Blocked),
    ] {
        task_state(&mut state, seq, id, status);
    }
    apply(
        &mut state,
        23,
        0,
        OrgEvent::SessionSummary(atm_org::SessionSummary {
            session_id: "lead-saved".into(),
            pane: Some("%14".into()),
            agent_id: Some("beta/lead".parse().unwrap()),
            harness: Harness::Codex,
            model: Some("model-b".into()),
            cost: None,
            first_prompt: Some("SECRET saved prompt".into()),
            started: at(0),
            ended: None,
        }),
    );
    for (seq, id) in [(24, "alpha/watch"), (25, "beta/stalled")] {
        apply(
            &mut state,
            seq,
            0,
            OrgEvent::AgentDown {
                agent_id: id.parse().unwrap(),
                reason: "SECRET down reason".into(),
            },
        );
    }
    apply(
        &mut state,
        26,
        2,
        OrgEvent::AgentStuck {
            agent_id: "beta/stalled".parse().unwrap(),
            reason: "SECRET stuck reason".into(),
        },
    );
    apply(&mut state, 41, 0, decision("alpha/lead"));
    apply(&mut state, 42, 0, decision("beta/lead"));
    apply(&mut state, 43, 0, message("owner", "SECRET failed body"));
    apply(
        &mut state,
        44,
        1,
        OrgEvent::DecisionTakenProvisionally(atm_org::ProvisionalChoice {
            relayed_by: None,
            decision_id: "d42".parse().unwrap(),
            option: "SECRET option".into(),
            by: Address::System,
        }),
    );
    apply(
        &mut state,
        45,
        4,
        OrgEvent::MessageDeliveryFailed {
            message_id: "m43".parse().unwrap(),
            reason: "SECRET given up".into(),
            final_failure: true,
        },
    );
    apply(
        &mut state,
        46,
        4,
        message("owner", "SECRET system notice of m43 failure"),
    );
    let mut rows = Vec::new();
    for (id, pane, harness, model, context, status, minute) in [
        (
            "worker-live",
            "%12",
            "codex",
            "model-b",
            74.0,
            SessionStatus::AttentionNeeded,
            3,
        ),
        (
            "chief-live",
            "%15",
            "pi",
            "model-a",
            0.0,
            SessionStatus::Idle,
            0,
        ),
        (
            "lead-live",
            "%11",
            "pi",
            "model-a",
            42.5,
            SessionStatus::Idle,
            0,
        ),
        (
            "broken-live",
            "%13",
            "pi",
            "model-c",
            10.0,
            SessionStatus::Idle,
            0,
        ),
        (
            "unnamed",
            "%99",
            "pi",
            "model-other",
            95.0,
            SessionStatus::AttentionNeeded,
            0,
        ),
    ] {
        rows.push(SessionView {
            id: id.into(),
            tmux_pane: Some(pane.into()),
            harness: harness.into(),
            model: model.into(),
            context_percentage: context,
            status,
            last_activity: at(minute).to_rfc3339(),
            first_prompt: Some("SECRET live prompt".into()),
            activity_detail: Some("SECRET activity".into()),
            ..Default::default()
        });
    }
    let edges = ReportingEdges {
        reason: None,
        edges: [
            ("alpha/lead", Ok("owner")),
            ("alpha/watch", Ok("gamma/chief")),
            ("alpha/worker", Ok("alpha/lead")),
            (
                "beta/broken",
                Err("reporting line at beta/broken: invalid role configuration"),
            ),
            ("beta/lead", Ok("owner")),
            ("beta/stalled", Ok("beta/lead")),
            ("gamma/chief", Ok("owner")),
        ]
        .into_iter()
        .map(|(id, manager)| {
            (
                id.parse().unwrap(),
                manager
                    .map(|address| address.parse().unwrap())
                    .map_err(str::to_owned),
            )
        })
        .collect(),
    };
    (state, rows, edges)
}
fn value(state: &OrgState, rows: &[SessionView], edges: &ReportingEdges) -> Value {
    serde_json::to_value(project(state, 99, rows, edges, at(5))).unwrap()
}

// JSON numbers have one numeric domain: the registry supplies f64, so 74.0
// and the hand-written golden's 74 are equivalent; keys/nulls/dates stay exact.
fn assert_golden(actual: &Value, expected: &Value) {
    match (actual, expected) {
        (Value::Number(a), Value::Number(b)) => assert_eq!(a.as_f64(), b.as_f64()),
        (Value::Array(a), Value::Array(b)) => {
            assert_eq!(a.len(), b.len());
            for (a, b) in a.iter().zip(b) {
                assert_golden(a, b);
            }
        }
        (Value::Object(a), Value::Object(b)) => {
            assert_eq!(a.keys().collect::<Vec<_>>(), b.keys().collect::<Vec<_>>());
            for (key, a) in a {
                assert_golden(a, b.get(key).unwrap());
            }
        }
        _ => assert_eq!(actual, expected),
    }
}

// Break caught: the daemon's allowlisted projection no longer matches the
// already-shipped client's contract (all dates and numeric nulls exact).
#[test]
fn org_tree_matches_cli_goldens() {
    let (state, rows, edges) = seeded();
    let mut actual = value(&state, &rows, &edges);
    assert_eq!(
        actual.as_object_mut().unwrap().remove("seq"),
        Some(json!(99))
    );
    assert_golden(
        &actual,
        &serde_json::from_str::<Value>(include_str!(
            "../../../../tests/fixtures/atm_org/tree.json"
        ))
        .unwrap(),
    );
    let unavailable = ReportingEdges {
        reason: Some("reader busy".into()),
        ..edges
    };
    let mut actual = value(&state, &rows, &unavailable);
    actual.as_object_mut().unwrap().remove("seq");
    let mut expected: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/atm_org/unavailable.json"
    ))
    .unwrap();
    expected["reporting"]["reason"] = json!("reader busy");
    assert_golden(&actual, &expected);
}

// Break caught: project placement follows the creator instead of the current
// holder, closed work remains open, or global decisions leak into project counts.
#[test]
fn org_tree_task_and_decision_placement() {
    let (mut state, rows, edges) = seeded();
    assign(&mut state, 50, "t12", "gamma/chief");
    apply(
        &mut state,
        51,
        0,
        task("Unassigned member work", TaskKind::Work, "alpha/worker"),
    );
    apply(
        &mut state,
        52,
        0,
        task("System work", TaskKind::Work, "system"),
    );
    task_state(&mut state, 53, "t11", TaskState::Accepted);
    task_state(&mut state, 54, "t13", TaskState::Cancelled);
    task_state(&mut state, 55, "t14", TaskState::Unknown);
    apply(&mut state, 56, 0, decision("owner"));
    apply(&mut state, 57, 0, decision("system"));
    // Status wins over any retained provisional fact.
    apply(
        &mut state,
        58,
        0,
        OrgEvent::DecisionResolved(
            atm_org::DecisionResolution {
                decision_id: "d42".parse().unwrap(),
                option: "no".into(),
                by: Address::Owner,
                note: Some("SECRET resolution".into()),
                standing_rule: false,
            }
            .into(),
        ),
    );
    let result = value(&state, &rows, &edges);
    assert_eq!(result["projects"][0]["tasks_open"], 1);
    assert_eq!(result["projects"][0]["tasks"][0]["task_id"], "t51");
    assert_eq!(result["projects"][1]["tasks"][0]["state"], "unknown");
    assert_eq!(
        result["projects"][1]["decisions"],
        json!({"pending":0,"provisional":0})
    );
    assert_eq!(result["projects"][2]["tasks"][0]["task_id"], "t12");
    assert_eq!(result["projects"][2]["seats"][0]["tasks"], json!(["t12"]));
    assert_eq!(
        result["unplaced_tasks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["task_id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["t15", "t52"]
    );
    for id in ["d56", "d57"] {
        let item = result["needs_owner"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["id"] == id)
            .unwrap();
        assert!(item["agent_id"].is_null());
        assert!(item["project"].is_null());
    }
}

// Break caught: recorded projects follow workers, or the legacy tree fallback starts using the last holder.
#[test]
fn recorded_task_project_is_separate_from_seat_assignment_and_legacy_reopen() {
    let (mut state, rows, edges) = seeded();
    let mut event = task("Targeted", TaskKind::Work, "owner").to_body();
    event["project_id"] = json!("alpha");
    apply(
        &mut state,
        50,
        0,
        OrgEvent::from_parts("task.created", &event)
            .unwrap()
            .unwrap(),
    );
    assign(&mut state, 51, "t50", "beta/lead");
    let assigned = value(&state, &rows, &edges);
    assert_eq!(assigned["projects"][0]["tasks_open"], 4);
    assert_eq!(assigned["projects"][1]["tasks_open"], 1);
    assert_eq!(
        assigned["projects"][1]["seats"][1]["tasks"],
        json!(["t14", "t50"])
    );
    task_state(&mut state, 52, "t50", TaskState::Open);
    task_state(&mut state, 53, "t14", TaskState::Open);
    let reopened = value(&state, &rows, &edges);
    assert_eq!(reopened["projects"][0]["tasks_open"], 4);
    assert_eq!(reopened["projects"][1]["tasks_open"], 0);
    assert_eq!(
        reopened["unplaced_tasks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["task_id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["t14", "t15"]
    );
}

// Break caught: notification text doubles authoritative problems, read/failed
// state is ignored, or named/ineligible/child rows inflate unnamed attention.
#[test]
fn org_tree_needs_owner_once() {
    let (mut state, mut rows, edges) = seeded();
    apply(
        &mut state,
        50,
        0,
        message("owner", "SECRET beta/stalled is stuck"),
    );
    apply(
        &mut state,
        51,
        0,
        message("owner", "SECRET duplicate m43 failure notice"),
    );
    apply(
        &mut state,
        52,
        0,
        message("alpha/worker", "SECRET named failure"),
    );
    apply(
        &mut state,
        53,
        4,
        OrgEvent::MessageDeliveryFailed {
            message_id: "m52".parse().unwrap(),
            reason: "SECRET given-up".into(),
            final_failure: true,
        },
    );
    rows.extend([
        SessionView {
            id: "child".into(),
            parent_session_id: Some("unnamed".into()),
            status: SessionStatus::AttentionNeeded,
            ..Default::default()
        },
        SessionView {
            id: "wrong".into(),
            tmux_pane: Some("%14".into()),
            harness: "pi".into(),
            status: SessionStatus::AttentionNeeded,
            ..Default::default()
        },
    ]);
    let result = value(&state, &rows, &edges);
    assert_eq!(result["needs_owner"]["items_total"], 7);
    assert_eq!(result["needs_owner"]["owner_unread"], 4);
    assert_eq!(result["needs_owner"]["unnamed_attention"], 1);
    let failed = result["needs_owner"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["id"] == "m52")
        .unwrap();
    assert_eq!(failed["agent_id"], "alpha/worker");
    assert_eq!(failed["project"], "alpha");
    assert_eq!(failed["since"], "2026-01-15T12:04:00Z");
    apply(
        &mut state,
        54,
        0,
        OrgEvent::OwnerAway(atm_org::OwnerAway {
            by: Address::Owner,
            stacking_caps: Default::default(),
        }),
    );
    assert_eq!(
        value(&state, &rows, &edges)["needs_owner"]["away_since"],
        "2026-01-15T12:00:00Z"
    );
}

// Break caught: serializing a source record exposes task text, decision facts,
// message bodies/attachments or private role/session content.
#[test]
fn org_tree_visibility_allowlist() {
    let (mut state, rows, edges) = seeded();
    apply(
        &mut state,
        50,
        0,
        OrgEvent::AgentUpRequested {
            agent_id: "alpha/watch".parse().unwrap(),
            role: atm_org::parse_role(
                "watch",
                "+++\nharness='pi'\n+++\nSECRET saved role instructions",
            )
            .unwrap(),
        },
    );
    let result = value(&state, &rows, &edges);
    assert!(!result.to_string().contains("SECRET"));
    for project in result["projects"].as_array().unwrap() {
        for seat in project["seats"].as_array().unwrap() {
            assert_eq!(
                seat.as_object()
                    .unwrap()
                    .keys()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
                [
                    "activity",
                    "agent_id",
                    "context_percentage",
                    "harness",
                    "line_error",
                    "manager",
                    "model",
                    "pane",
                    "state",
                    "tasks"
                ]
            );
        }
        for task in project["tasks"].as_array().unwrap() {
            assert_eq!(
                task.as_object()
                    .unwrap()
                    .keys()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
                ["agent_id", "creator", "kind", "state", "task_id", "title"]
            );
        }
    }
}

// Break caught: an invalid observation time becomes an invented timestamp or
// sorts before known problems instead of remaining explicitly unknown.
#[test]
fn org_tree_unknown_attention_time_stays_null() {
    let (state, mut rows, edges) = seeded();
    rows.iter_mut()
        .find(|row| row.id.as_str() == "worker-live")
        .unwrap()
        .last_activity = "not-a-date".into();
    let result = value(&state, &rows, &edges);
    assert_eq!(
        result["needs_owner"]["items"][4],
        json!({
            "kind":"seat_attention", "id":"alpha/worker", "agent_id":"alpha/worker", "project":"alpha", "since":null,
        })
    );
    assert_eq!(result["needs_owner"]["items"][5]["kind"], "line_broken");
    let result = value(&OrgState::default(), &[], &ReportingEdges::default());
    assert_eq!(result["projects"], json!([]));
    assert_eq!(result["needs_owner"]["items_total"], 0);
}

// Break caught: caps replace totals, lexicographic task IDs replace creation
// order, missing dates precede old dates, or empty/non-repository projects vanish.
#[test]
fn org_tree_caps_and_order() {
    let (mut state, mut rows, mut edges) = seeded();
    for (seq, id, root) in [
        (60, "epsilon", "/fixtures/home/epsilon"),
        (61, "delta", "/fixtures/repos/delta"),
    ] {
        apply(
            &mut state,
            seq,
            0,
            OrgEvent::ProjectRegistered {
                project_id: id.parse().unwrap(),
                root: root.into(),
            },
        );
    }
    for seq in 100..205 {
        apply(
            &mut state,
            seq,
            0,
            task(&format!("Work {seq}"), TaskKind::Work, "alpha/lead"),
        );
        apply(&mut state, seq + 200, 0, decision("alpha/lead"));
    }
    rows.reverse();
    let result = value(&state, &rows, &edges);
    assert_eq!(
        result["projects"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["alpha", "beta", "delta", "epsilon", "gamma"]
    );
    assert_eq!(result["projects"][0]["tasks_open"], 108);
    let tasks = result["projects"][0]["tasks"].as_array().unwrap();
    assert_eq!(tasks.len(), 50);
    assert_eq!(tasks[0]["task_id"], "t11");
    assert_eq!(tasks[3]["task_id"], "t100");
    assert_eq!(tasks[49]["task_id"], "t146");
    assert_eq!(result["projects"][2]["seats"], json!([]));
    assert_eq!(result["projects"][3]["root"], "/fixtures/home/epsilon");
    assert_eq!(result["needs_owner"]["items_total"], 111);
    assert_eq!(
        result["needs_owner"]["items"].as_array().unwrap().len(),
        100
    );
    assert_eq!(result["needs_owner"]["items"][0]["id"], "d300");
    assert_eq!(result["needs_owner"]["items"][99]["id"], "d399");
    // A missing observation timestamp stays null; no clock-based guess.
    rows.iter_mut()
        .find(|r| r.id.as_str() == "worker-live")
        .unwrap()
        .last_activity = "not-a-date".into();
    edges.reason = Some("busy".into());
    let result = value(&state, &rows, &edges);
    assert_eq!(result["reporting"]["state"], "unavailable");
    assert!(result["projects"][0]["seats"][0]["manager"].is_null());
    assert!(result["projects"][1]["seats"][0]["line_error"].is_null());
}
