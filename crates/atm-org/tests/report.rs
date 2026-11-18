//! Synthetic: question-specific histories, exact hand-calculated expected reports.
//! Breaks caught: a second usage ledger, lost replay metadata, scope/page leakage,
//! invented missing measurements and decisions incorrectly restricted to task links.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use atm_org::*;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeSet;

const CASES: [(&str, &str); 9] = [
    (
        include_str!("fixtures/report/q1-outcomes.jsonl"),
        include_str!("fixtures/report/q1-outcomes.json"),
    ),
    (
        include_str!("fixtures/report/q2-wall-time.jsonl"),
        include_str!("fixtures/report/q2-wall-time.json"),
    ),
    (
        include_str!("fixtures/report/q3-back-and-forth.jsonl"),
        include_str!("fixtures/report/q3-back-and-forth.json"),
    ),
    (
        include_str!("fixtures/report/q4-owner-step-ins.jsonl"),
        include_str!("fixtures/report/q4-owner-step-ins.json"),
    ),
    (
        include_str!("fixtures/report/q5-cost-source.jsonl"),
        include_str!("fixtures/report/q5-cost-source.json"),
    ),
    (
        include_str!("fixtures/report/q6-change-trend.jsonl"),
        include_str!("fixtures/report/q6-change-trend.json"),
    ),
    (
        include_str!("fixtures/report/q7-context-rework.jsonl"),
        include_str!("fixtures/report/q7-context-rework.json"),
    ),
    (
        include_str!("fixtures/report/q8-budget-overage.jsonl"),
        include_str!("fixtures/report/q8-budget-overage.json"),
    ),
    (
        include_str!("fixtures/report/q9-decisions.jsonl"),
        include_str!("fixtures/report/q9-decisions.json"),
    ),
];
#[derive(Deserialize)]
struct Event {
    seq: u64,
    ts: DateTime<Utc>,
    kind: String,
    body: Value,
}
fn events(text: &str) -> Vec<Event> {
    text.lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect()
}
fn fold(state: &mut OrgState, rows: &[Event]) {
    let mut stats = ReplayStats::default();
    for row in rows {
        stats
            .apply_parts(state, row.seq, row.ts, &row.kind, &row.body)
            .unwrap();
    }
}
fn history(index: usize) -> OrgState {
    let mut state = OrgState::default();
    fold(&mut state, &events(CASES[index].0));
    state
}
fn now() -> DateTime<Utc> {
    "2026-01-01T00:01:00Z".parse().unwrap()
}
fn params(value: Value) -> ReportParams {
    serde_json::from_value(value).unwrap()
}
fn rendered(state: &OrgState, p: Value, visible: &ReportVisibility) -> Value {
    serde_json::to_value(report(state, &params(p), visible, now()).unwrap()).unwrap()
}
fn owner(state: &OrgState, p: Value) -> Value {
    rendered(state, p, &ReportVisibility::All)
}
fn task<'a>(value: &'a Value, id: &str) -> &'a Value {
    value["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["task_id"] == id)
        .unwrap()
}
fn apply(state: &mut OrgState, seq: u64, time: &str, kind: &str, body: Value) {
    state
        .apply(
            seq,
            time.parse().unwrap(),
            &OrgEvent::from_parts(kind, &body).unwrap().unwrap(),
        )
        .unwrap();
}

// Break caught: cross-project workers move recorded task costs/trends to their own project.
#[test]
fn recorded_projects_own_costs_and_trends_across_worker_projects_and_replay() {
    let mut rows = events(CASES[5].0);
    for row in &mut rows {
        match row.kind.as_str() {
            "task.created" => row.body["project_id"] = json!("alpha"),
            "task.assigned" => row.body["agent_id"] = json!("beta/review"),
            "task.submitted" => row.body["by"] = json!("beta/review"),
            _ => (),
        }
    }
    let expected: Value = serde_json::from_str(CASES[5].1).unwrap();
    for split in 0..=rows.len() {
        let mut state = OrgState::default();
        fold(&mut state, &rows[..split]);
        fold(&mut state, &rows[split..]);
        let got = owner(&state, json!({"project_id":"alpha", "bucket":2, "limit":1}));
        assert_eq!(got["projects"], expected["projects"]);
        assert_eq!(got["tasks"][0]["project_id"], "alpha");
        assert_eq!(got["tasks"][0]["agent_id"], "beta/review");
        assert_eq!(got["next_after"], "t5");
        assert!(owner(&state, json!({"project_id":"beta"}))["tasks"]
            .as_array()
            .unwrap()
            .is_empty());
    }
}

// Break caught: redo erases recorded attribution or moves decision totals off their requester.
#[test]
fn redo_keeps_recorded_project_while_legacy_last_holder_and_decision_rules_stay_distinct() {
    let mut rows = events(include_str!("fixtures/report/q9-redo.jsonl"));
    for row in &mut rows {
        if row.kind == "task.created" {
            row.body["project_id"] = json!("alpha");
        }
    }
    let mut state = OrgState::default();
    fold(&mut state, &rows);
    let got = owner(&state, json!({}));
    assert_eq!(task(&got, "t12")["agent_id"], "beta/review");
    assert_eq!(task(&got, "t12")["project_id"], "alpha");
    assert_eq!(got["projects"][0]["tasks"]["open"], 1);
    assert_eq!(got["projects"][0]["tasks"]["accepted"], 1);
    assert_eq!(got["projects"][0]["decisions"]["tasks_redone"], 0);
    assert_eq!(got["projects"][1]["tasks"]["open"], 0);
    assert_eq!(got["projects"][1]["decisions"]["tasks_redone"], 2);
    assert_eq!(state.task(&"t12".parse().unwrap()).unwrap().agent_id, None);
    let mut legacy = OrgState::default();
    fold(
        &mut legacy,
        &events(include_str!("fixtures/report/q9-redo.jsonl")),
    );
    assert_eq!(
        task(&owner(&legacy, json!({})), "t12")["project_id"],
        "beta"
    );
}

#[test]
fn nine_question_histories_match_full_expected_reports_and_every_replay_split() {
    for (index, (input, expected)) in CASES.iter().enumerate() {
        let rows = events(input);
        let expected: Value = serde_json::from_str(expected).unwrap();
        let p = if index == 5 {
            json!({"bucket":2})
        } else {
            json!({})
        };
        let state = history(index);
        let before = state.clone();
        assert_eq!(owner(&state, p.clone()), expected, "question {}", index + 1);
        assert_eq!(state, before, "report changed the ledger");
        for split in 0..=rows.len() {
            let mut replay = OrgState::default();
            fold(&mut replay, &rows[..split]);
            fold(&mut replay, &rows[split..]);
            assert_eq!(replay, state, "question {}, split {split}", index + 1);
            assert_eq!(owner(&replay, p.clone()), expected);
        }
        for row in expected["tasks"].as_array().unwrap() {
            let usage = serde_json::to_value(
                state
                    .task_cost(&row["task_id"].as_str().unwrap().parse().unwrap(), now())
                    .unwrap(),
            )
            .unwrap();
            for (field, value) in usage.as_object().unwrap() {
                assert_eq!(&row[field], value, "ledger field {field}");
            }
        }
        let serialized = expected.to_string();
        assert!(
            !serialized.contains("PRIVATE_"),
            "private event payload leaked"
        );
    }
}

// Break caught: landed redo lists remain zero, count only accepted-again work,
// double-count one owner event across answer+redo, lose last-holder placement,
// or leak requester aggregates through visible tasks.
#[test]
fn recorded_redo_counts_replay_and_follow_requester_visibility_before_paging() {
    let rows = events(include_str!("fixtures/report/q9-redo.jsonl"));
    let mut state = OrgState::default();
    fold(&mut state, &rows);
    let mut before = OrgState::default();
    fold(&mut before, &rows[..13]);
    for invalid in [json!(["t7", "t7"]), json!(["t7", "missing"])] {
        let mut body = rows[13].body.clone();
        body["redo_tasks"] = invalid;
        let fact = OrgEvent::from_parts("decision.overruled", &body)
            .unwrap()
            .unwrap();
        let unchanged = before.clone();
        assert!(before.apply(14, rows[13].ts, &fact).is_err());
        assert_eq!(
            before, unchanged,
            "failed redo must not advance report counters"
        );
    }
    let got = owner(&state, json!({}));
    assert_eq!(task(&got, "t7")["redone"], 1);
    assert_eq!(task(&got, "t7")["outcome"], "accepted");
    assert_eq!(
        task(&got, "t7")["owner_step_ins"],
        json!({"total":4,"verdict":2,"redirect":1,"answer":1,"message":0,"redo":1})
    );
    assert_eq!(task(&got, "t12")["redone"], 1);
    assert_eq!(task(&got, "t12")["outcome"], "open");
    assert_eq!(task(&got, "t12")["agent_id"], "beta/review");
    assert_eq!(task(&got, "t12")["ended_at"], Value::Null);
    assert_eq!(
        task(&got, "t12")["owner_step_ins"],
        json!({"total":2,"verdict":0,"redirect":1,"answer":1,"message":0,"redo":1})
    );
    assert_eq!(got["projects"][0]["decisions"]["tasks_redone"], 0);
    assert_eq!(got["projects"][1]["decisions"]["tasks_redone"], 2);
    assert_eq!(
        state
            .decision_redo("d5".parse().unwrap())
            .unwrap()
            .completed
            .len(),
        1
    );
    assert_eq!(
        owner(&state, json!({"after":"t7","limit":1}))["projects"],
        got["projects"]
    );
    let visible = |reader: &str| ReportVisibility::Reports {
        reader: reader.parse().unwrap(),
        agents: BTreeSet::new(),
    };
    let alpha = rendered(&state, json!({}), &visible("alpha/build"));
    assert_eq!(alpha["tasks"].as_array().unwrap().len(), 1);
    assert_eq!(alpha["projects"].as_array().unwrap().len(), 1);
    assert_eq!(alpha["projects"][0]["decisions"]["tasks_redone"], 0);
    let beta = rendered(&state, json!({}), &visible("beta/review"));
    assert_eq!(beta["tasks"].as_array().unwrap().len(), 1);
    assert_eq!(beta["projects"][0]["decisions"]["tasks_redone"], 2);
    assert!(!got.to_string().contains("PRIVATE_"));
    for split in 0..=rows.len() {
        let mut replay = OrgState::default();
        fold(&mut replay, &rows[..split]);
        fold(&mut replay, &rows[split..]);
        assert_eq!(replay, state);
        assert_eq!(owner(&replay, json!({})), got);
    }
}

// Break caught: page limits/cursors clip project totals or lexical t9/t10 ordering.
#[test]
fn paging_and_window_boundaries_do_not_clip_whole_task_numbers() {
    let state = history(5);
    let whole = owner(&state, json!({"bucket":2}));
    let first = owner(&state, json!({"bucket":2,"limit":1}));
    assert_eq!(first["tasks"].as_array().unwrap().len(), 1);
    assert_eq!(first["next_after"], "t5");
    assert_eq!(first["projects"], whole["projects"]);
    let second = owner(&state, json!({"bucket":2,"after":"t5","limit":1}));
    assert_eq!(second["tasks"][0]["task_id"], "t10");
    assert_eq!(second["projects"], whole["projects"]);
    let window = owner(
        &state,
        json!({"since":"2026-01-01T00:00:09Z","until":"2026-01-01T00:00:15Z"}),
    );
    assert_eq!(
        window["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["task_id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["t10", "t15"]
    );
    assert_eq!(window["projects"][0]["tasks"]["accepted"], 2);
    assert_eq!(window["tasks"][0]["turns"], 9);
    let empty = owner(&state, json!({"since":now(),"until":now()}));
    assert!(empty["tasks"].as_array().unwrap().is_empty());
}

// Break caught: nullable measurements become zeros or metadata replaces frozen metrics.
#[test]
fn frozen_metrics_and_latest_source_summary_remain_authoritative() {
    let mut state = history(4);
    apply(
        &mut state,
        100,
        "2026-01-01T00:00:08Z",
        "task.usage_observed",
        json!({"session_id":"s1","task_id":"t5","turn":true,"cumulative_cost":{"microdollars":900000},"context_percent":99}),
    );
    apply(
        &mut state,
        101,
        "2026-01-01T00:00:09Z",
        "session.summary",
        json!({"session_id":"s1","pane":"%1","agent_id":"alpha/build","harness":"codex","model":"later-model","cost":null,"first_prompt":"PRIVATE_NEW_PROMPT","started":"2026-01-01T00:00:00Z","ended":null}),
    );
    let got = owner(&state, json!({}));
    assert_eq!(task(&got, "t5")["cost"], json!({"microdollars":300000}));
    assert_eq!(task(&got, "t5")["turns"], 2);
    assert_eq!(task(&got, "t5")["peak_context_percent"], 60);
    assert_eq!(
        task(&got, "t5")["cost_source"],
        json!({"label":"harness_reported","harnesses":["codex"],"models":["later-model"]})
    );
    assert_eq!(task(&got, "t14")["cost"], Value::Null);
    assert_eq!(task(&got, "t14")["cost_source"]["label"], "unknown");
}

// Break caught: creator/last-holder/report rows are omitted or requester counts use task links.
#[test]
fn supplied_visibility_scopes_rows_and_unlinked_decisions_independently() {
    let visible = ReportVisibility::Reports {
        reader: "alpha/build".parse().unwrap(),
        agents: BTreeSet::from(["alpha/build".parse().unwrap()]),
    };
    let got = rendered(&history(8), json!({}), &visible);
    assert_eq!(got["scope"]["reader"], "alpha/build");
    assert_eq!(got["scope"]["sees"], "reports");
    assert!(got["tasks"].as_array().unwrap().is_empty());
    assert_eq!(got["projects"].as_array().unwrap().len(), 1);
    assert_eq!(got["projects"][0]["project_id"], "alpha");
    assert_eq!(got["projects"][0]["decisions"]["overruled"], 1);
    let until = rendered(
        &history(8),
        json!({"since":"2026-01-01T00:00:04Z","until":"2026-01-01T00:00:06Z"}),
        &ReportVisibility::All,
    );
    assert_eq!(until["projects"].as_array().unwrap().len(), 1);
    assert_eq!(until["projects"][0]["project_id"], "beta");
    assert_eq!(until["projects"][0]["decisions"]["agent_decided"], 1);
    let mut state = history(4);
    apply(
        &mut state,
        100,
        "2026-01-01T00:00:10Z",
        "task.state_changed",
        json!({"task_id":"t5","state":"open","reason":null,"by":"owner"}),
    );
    let got = rendered(&state, json!({}), &visible);
    let reopened = task(&got, "t5");
    assert_eq!(reopened["agent_id"], "alpha/build");
    assert_eq!(reopened["project_id"], "alpha");
    assert_eq!(reopened["outcome"], "open");
    assert_eq!(reopened["ended_at"], Value::Null);
    assert_eq!(
        got["tasks"].as_array().unwrap().len(),
        2,
        "created but unassigned task must be visible"
    );
}

// Break caught: partially mutating fold metadata before a failing apply.
#[test]
fn failed_events_leave_the_entire_reporting_fold_unchanged() {
    let mut state = history(0);
    let before = state.clone();
    let duplicate = OrgEvent::from_parts("task.created", &events(CASES[0].0)[4].body)
        .unwrap()
        .unwrap();
    assert!(state.apply(5, now(), &duplicate).is_err());
    assert_eq!(state, before);
    let bad = OrgEvent::from_parts(
        "task.assigned",
        &json!({"task_id":"t5","agent_id":"alpha/missing","by":"owner"}),
    )
    .unwrap()
    .unwrap();
    assert!(state.apply(200, now(), &bad).is_err());
    assert_eq!(state, before);
}

// Synthetic: four distinct takers and one actual owner confirmation, not harness captures.
const RELAYS: &str = include_str!("fixtures/report/q10-relays.jsonl");

// Break caught: owner-attributed relays disappear, self-relays become agent decisions, or relays count as owner answers.
#[test]
fn relay_report_fixture_counts_takes_without_misattributing_self_relay() {
    let rows = events(RELAYS);
    let expected: Value =
        serde_json::from_str(include_str!("fixtures/report/q10-relays.json")).unwrap();
    for split in 0..=rows.len() {
        let mut state = OrgState::default();
        fold(&mut state, &rows[..split]);
        fold(&mut state, &rows[split..]);
        let got = owner(&state, json!({}));
        assert_eq!(
            got["projects"][0]["decisions"],
            json!({"taken_for_owner":1,"agent_decided":1,"relayed":2,"overruled":0,"tasks_redone":0})
        );
        assert_eq!(got, expected);
        assert!(!got.to_string().contains("PRIVATE_"));
    }
    let mut before = OrgState::default();
    fold(&mut before, &rows[..14]);
    assert_eq!(
        owner(&before, json!({}))["projects"][0]["owner_step_ins"]["answer"],
        0
    );
}

// Break caught: recorder visibility replaces requester scope, page limits clip counts, or final answers erase relays.
#[test]
fn relay_counts_follow_requester_visibility_window_and_survive_final_answers() {
    let rows = events(RELAYS);
    let mut state = OrgState::default();
    fold(&mut state, &rows);
    for (reader, taken, agent, relayed) in [("alpha/chief", 0, 0, 1), ("alpha/build", 1, 1, 1)] {
        let visible = ReportVisibility::Reports {
            reader: reader.parse().unwrap(),
            agents: BTreeSet::new(),
        };
        let got = rendered(&state, json!({}), &visible);
        assert_eq!(
            got["projects"][0]["decisions"],
            json!({"taken_for_owner":taken,"agent_decided":agent,"relayed":relayed,"overruled":0,"tasks_redone":0})
        );
    }
    let window = owner(
        &state,
        json!({"project_id":"alpha","since":"2026-01-01T00:00:07Z","until":"2026-01-01T00:00:09Z"}),
    );
    assert_eq!(
        window["projects"][0]["decisions"],
        json!({"taken_for_owner":0,"agent_decided":0,"relayed":1,"overruled":0,"tasks_redone":0})
    );
    assert_eq!(
        owner(&state, json!({"after":"t5","limit":1}))["projects"],
        owner(&state, json!({}))["projects"]
    );
    apply(
        &mut state,
        100,
        "2026-01-01T00:00:12Z",
        "project.registered",
        json!({"project_id":"beta","root":"/fixtures/beta"}),
    );
    assert_eq!(
        owner(&state, json!({"project_id":"beta"}))["projects"],
        json!([])
    );
    for (option, overruled) in [("no", 0), ("yes", 1)] {
        let mut state = OrgState::default();
        fold(&mut state, &rows[..14]);
        let command = OrgCommand::Decision(DecisionCommand::Resolve(DecisionResolution {
            decision_id: "d11".parse().unwrap(),
            option: option.into(),
            by: Address::Owner,
            note: None,
            standing_rule: false,
        }));
        let facts = state.decide(&command, now()).unwrap();
        assert_eq!(facts.len(), 1);
        state.apply(15, now(), facts.first().unwrap()).unwrap();
        let got = owner(&state, json!({}));
        assert_eq!(
            got["projects"][0]["decisions"],
            json!({"taken_for_owner":1,"agent_decided":1,"relayed":2,"overruled":overruled,"tasks_redone":0})
        );
        assert_eq!(got["projects"][0]["owner_step_ins"]["answer"], 1);
    }
}

// Break caught: observations ignored by the usage ledger manufacture source/context facts.
#[test]
fn invalid_and_unattributed_usage_never_supplies_a_task_source() {
    let mut state = history(1);
    apply(
        &mut state,
        100,
        "2026-01-01T00:00:15Z",
        "task.usage_observed",
        json!({"session_id":"ignored","task_id":"t5","turn":false,"cumulative_cost":null,"context_percent":101}),
    );
    apply(
        &mut state,
        101,
        "2026-01-01T00:00:16Z",
        "task.usage_observed",
        json!({"session_id":"ignored","task_id":null,"turn":true,"cumulative_cost":{"microdollars":50},"context_percent":80}),
    );
    apply(
        &mut state,
        102,
        "2026-01-01T00:00:17Z",
        "session.summary",
        json!({"session_id":"ignored","pane":"%1","agent_id":"alpha/build","harness":"codex","model":"private-model","cost":null,"first_prompt":null,"started":"2026-01-01T00:00:00Z","ended":null}),
    );
    let got = owner(&state, json!({}));
    assert_eq!(
        task(&got, "t5")["cost_source"],
        json!({"label":"unknown","harnesses":[],"models":[]})
    );
    assert_eq!(task(&got, "t5")["turns"], 0);
    assert_eq!(task(&got, "t5")["peak_context_percent"], Value::Null);
}

// Break caught: crossing policy hides known spend merely because a future state is unknown.
#[test]
fn budget_overage_is_arithmetic_on_known_metrics_even_in_unknown_state() {
    let mut state = history(7);
    apply(
        &mut state,
        100,
        "2026-01-01T00:00:07Z",
        "task.state_changed",
        json!({"task_id":"t5","state":"future_state","reason":null,"by":"system"}),
    );
    let got = owner(&state, json!({}));
    assert_eq!(
        task(&got, "t5")["over_budget"],
        json!({"turns":1,"cost":{"microdollars":200000},"deadline_seconds":56})
    );
}

// Break caught: first-submit absence is later overwritten, or plan submissions freeze context.
#[test]
fn first_result_submission_freezes_even_absent_context_but_plans_do_not() {
    let mut rows = events(CASES[6].0);
    for row in &mut rows {
        if row.kind == "task.usage_observed" && row.seq < 10 {
            row.body["context_percent"] = Value::Null;
        }
    }
    let mut state = OrgState::default();
    fold(&mut state, &rows);
    assert_eq!(
        task(&owner(&state, json!({})), "t5")["context_percent_at_submit"],
        Value::Null
    );
    let mut rows = events(CASES[6].0);
    rows[9].body["what"] = json!("plan");
    let mut state = OrgState::default();
    fold(&mut state, &rows);
    assert_eq!(
        task(&owner(&state, json!({})), "t5")["context_percent_at_submit"],
        80
    );
}

// Break caught: an unrelated fact or second accounting record preserves a false closing-verdict exemption.
#[test]
fn unrelated_events_and_extra_accounting_break_review_closure_pairing() {
    for (kind, body) in [
        (
            "task.attached",
            json!({"task_id":"t5","file":{"path":"evidence","kind":"evidence","note":null},"by":"alpha/build"}),
        ),
        ("task.cost_recorded", events(CASES[2].0)[18].body.clone()),
    ] {
        let mut rows = events(CASES[2].0);
        rows.insert(
            19,
            Event {
                seq: 0,
                ts: rows[18].ts,
                kind: kind.into(),
                body,
            },
        );
        for (i, row) in rows.iter_mut().enumerate() {
            row.seq = i as u64 + 1;
        }
        let mut state = OrgState::default();
        fold(&mut state, &rows);
        assert_eq!(
            task(&owner(&state, json!({})), "t14")["owner_step_ins"]["verdict"],
            1
        );
    }
}

// Break caught: request-time and explicit decision links double-count one owner answer.
#[test]
fn explicit_and_inferred_answer_links_are_a_union_and_include_later_explicit_links() {
    let mut rows = events(CASES[3].0);
    rows[4].body["rests_on"] = json!("d8");
    let mut state = OrgState::default();
    fold(&mut state, &rows);
    assert_eq!(
        task(&owner(&state, json!({})), "t5")["owner_step_ins"]["answer"],
        1
    );
    let mut state = history(8);
    apply(
        &mut state,
        100,
        "2026-01-01T00:00:10Z",
        "task.created",
        json!({"title":"Linked","kind":"work","creator":"alpha/build","measure":{"kind":"owner"},"files":[],"rests_on":"d5"}),
    );
    let got = owner(&state, json!({"project_id":"alpha"}));
    assert_eq!(task(&got, "t100")["owner_step_ins"]["answer"], 1);
    assert_eq!(got["projects"][0]["decisions"]["overruled"], 1);
}

// Break caught: ambiguous seat ownership attributes messages/answers to an arbitrary task.
#[test]
fn ambiguous_holdings_do_not_attribute_owner_messages_or_inferred_answers() {
    let mut state = history(1);
    apply(
        &mut state,
        100,
        "2026-01-01T00:00:12Z",
        "task.created",
        json!({"title":"Other","kind":"work","creator":"owner","measure":{"kind":"owner"},"files":[]}),
    );
    apply(
        &mut state,
        101,
        "2026-01-01T00:00:13Z",
        "task.assigned",
        json!({"task_id":"t100","agent_id":"alpha/build","by":"owner"}),
    );
    apply(
        &mut state,
        102,
        "2026-01-01T00:00:14Z",
        "decision.requested",
        events(CASES[3].0)[7].body.clone(),
    );
    apply(
        &mut state,
        103,
        "2026-01-01T00:00:15Z",
        "decision.resolved",
        json!({"decision_id":"d102","option":"yes","by":"owner","note":null,"standing_rule":false}),
    );
    apply(
        &mut state,
        104,
        "2026-01-01T00:00:16Z",
        "msg.sent",
        json!({"from":"owner","to":"alpha/build","kind":"note","body":"PRIVATE_AMBIGUOUS","attachments":[],"task":null}),
    );
    let got = owner(&state, json!({}));
    for id in ["t5", "t100"] {
        assert_eq!(task(&got, id)["owner_step_ins"]["answer"], 0);
        assert_eq!(task(&got, id)["owner_step_ins"]["message"], 0);
    }
    assert_eq!(got["projects"][0]["tasks_with_step_ins"], 0);
}

// Break caught: lower medians always select the minimum, include null costs, or emit a spurious direction.
#[test]
fn odd_buckets_and_unknown_cost_comparisons_keep_their_defined_medians() {
    let state = history(5);
    let got = owner(&state, json!({"bucket":3}));
    let b = &got["projects"][0]["trend"]["buckets"][0];
    assert_eq!(b["turns"], 14);
    assert_eq!(b["peak_context_percent"], 62);
    assert_eq!(b["review_rounds"], 2);
    assert_eq!(b["changes_requested"], 1);
    assert_eq!(b["cost"], json!({"microdollars":1900000}));
    assert_eq!(b["cost_known"], 2);
    assert_eq!(got["projects"][0]["trend"]["direction"]["turns"], "none");
    let mut rows = events(CASES[5].0);
    for row in &mut rows {
        if row.kind == "task.cost_recorded" {
            row.body["record"]["cost"] = Value::Null;
        }
    }
    let mut state = OrgState::default();
    fold(&mut state, &rows);
    let got = owner(&state, json!({"bucket":2}));
    assert_eq!(got["projects"][0]["cost"], Value::Null);
    assert_eq!(got["projects"][0]["trend"]["buckets"][1]["cost_known"], 0);
    assert_eq!(got["projects"][0]["trend"]["direction"]["cost"], "none");
}

// Break caught: first assignment always gets exempted even when the owner did not create the task.
#[test]
fn only_owner_created_tasks_exempt_the_first_owner_assignment() {
    let mut state = history(4);
    apply(
        &mut state,
        100,
        "2026-01-01T00:00:10Z",
        "task.assigned",
        json!({"task_id":"t14","agent_id":"alpha/build","by":"owner"}),
    );
    let got = owner(&state, json!({}));
    assert_eq!(task(&got, "t14")["owner_step_ins"]["redirect"], 1);
    assert_eq!(task(&got, "t5")["owner_step_ins"]["redirect"], 0);
}

// Break caught: project overage silently clamps at the per-task u32 usage limit.
#[test]
fn project_turn_overage_can_exceed_a_single_tasks_counter_width() {
    let mut rows = events(CASES[5].0);
    for row in &mut rows {
        if row.kind == "task.created" {
            row.body["budget"] = json!({"turns":0,"cost":null,"deadline":null});
        }
        if row.kind == "task.cost_recorded" {
            row.body["record"]["turns"] = json!(4294967295_u64);
        }
    }
    let mut state = OrgState::default();
    fold(&mut state, &rows);
    let got = owner(&state, json!({}));
    assert_eq!(task(&got, "t5")["over_budget"]["turns"], 4294967295_u64);
    assert_eq!(got["projects"][0]["over_budget"]["turns"], 21474836475_u64);
}

// Break caught: unknown params, unbounded buckets, impossible windows or invalid cursors accepted.
#[test]
fn invalid_params_and_unknown_entities_are_refused_without_mutation() {
    let state = history(0);
    let before = state.clone();
    assert!(serde_json::from_value::<ReportParams>(json!({"bukcet":5})).is_err());
    assert!(serde_json::from_value::<ReportParams>(json!({"since":"1h"})).is_err());
    for p in [
        json!({"bucket":0}),
        json!({"bucket":1001}),
        json!({"limit":0}),
        json!({"limit":1001}),
        json!({"since":"2026-01-02T00:00:00Z"}),
        json!({"project_id":"absent"}),
        json!({"after":"garbage"}),
        json!({"after":"t999"}),
        json!({"after":"t05"}),
    ] {
        assert!(
            report(&state, &params(p.clone()), &ReportVisibility::All, now()).is_err(),
            "{p}"
        );
    }
    assert_eq!(state, before);
}
