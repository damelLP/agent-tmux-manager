//! Pure history contracts; synthetic events, with no filesystem or wall clock.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use atm_org::{message_query::*, *};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};

fn params(value: Value) -> HistoryParams {
    serde_json::from_value(value).unwrap()
}
fn at(day: u32) -> DateTime<Utc> {
    format!("2026-01-{day:02}T00:00:00Z").parse().unwrap()
}
fn conversation(reader: &str, agent: &str) -> HistoryScope {
    HistoryScope::Conversation {
        reader: reader.parse().unwrap(),
        agent: agent.parse().unwrap(),
    }
}
fn delivery() -> HistoryScope {
    HistoryScope::DeliveryInbox {
        reader: "gamma/chief".parse().unwrap(),
        agent: "alpha/worker".parse().unwrap(),
    }
}
fn ids(selection: HistorySelection<'_>) -> Vec<u64> {
    selection.rows.iter().map(|m| m.id.seq()).collect()
}
fn fixture() -> OrgState {
    let mut state = OrgState::default();
    for (seq, project) in [(1, "alpha"), (2, "beta"), (3, "gamma")] {
        state
            .apply(
                seq,
                at(1),
                &OrgEvent::ProjectRegistered {
                    project_id: project.parse().unwrap(),
                    root: format!("/{project}"),
                },
            )
            .unwrap();
    }
    for (seq, id, pane) in [
        (4, "alpha/worker", "%1"),
        (5, "beta/lead", "%2"),
        (6, "gamma/chief", "%3"),
    ] {
        state
            .apply(
                seq,
                at(1),
                &OrgEvent::AgentAdopted {
                    agent_id: id.parse().unwrap(),
                    pane: pane.into(),
                    session_id: None,
                },
            )
            .unwrap();
    }
    // Equal and backwards wall times deliberately disagree with numeric send order.
    for (seq, day, from, to, task, body) in [
        (10, 3, "owner", "alpha/worker", Some("t1"), "review café 🔎"),
        (
            12,
            3,
            "alpha/worker",
            "beta/lead",
            Some("t1"),
            "review café 🔎",
        ),
        (
            14,
            2,
            "beta/lead",
            "alpha/worker",
            Some("t2"),
            "REVIEW café",
        ),
        (
            16,
            4,
            "alpha/worker",
            "alpha/worker",
            Some("t1"),
            "review self",
        ),
        (18, 1, "gamma/chief", "owner", None, "unrelated review"),
        (20, 3, "system", "alpha/worker", None, "body only"),
    ] {
        send(&mut state, seq, day, from, to, task, body);
    }
    state
}
fn send(
    state: &mut OrgState,
    seq: u64,
    day: u32,
    from: &str,
    to: &str,
    task: Option<&str>,
    body: &str,
) {
    state
        .apply(
            seq,
            at(day),
            &OrgEvent::MessageSent(MessageContent {
                delivery_notice: None,
                from: from.parse().unwrap(),
                to: to.parse().unwrap(),
                kind: MessageKind::Note,
                body: body.into(),
                task: task.map(|t| t.parse().unwrap()),
                attachments: vec!["attachment-only".into()],
            }),
        )
        .unwrap();
}

// Break: incoming-only conversations, double-counted self-sends, or timestamp sorting.
#[test]
fn scope_direction_and_numeric_order() {
    let state = fixture();
    let p = params(json!({}));
    for (scope, expected) in [
        (HistoryScope::All, vec![20, 18, 16, 14, 12, 10]),
        (
            conversation("alpha/worker", "alpha/worker"),
            vec![20, 16, 14, 12, 10],
        ),
        (
            conversation("owner", "alpha/worker"),
            vec![20, 16, 14, 12, 10],
        ),
        (delivery(), vec![20, 16, 14, 10]),
    ] {
        assert_eq!(ids(history(&state, &p, &scope, 25).unwrap()), expected);
    }
}

// Break: ORed filters, one-way correspondent, case-folding, or attachment/task search.
#[test]
fn filters_are_conjoined_literal_body_only_and_bidirectional() {
    let state = fixture();
    let scope = conversation("alpha/worker", "alpha/worker");
    for (filter, expected) in [
        (json!({"correspondent":"beta/lead"}), vec![14, 12]),
        (json!({"correspondent":"alpha/worker"}), vec![16]),
        (json!({"correspondent":"owner"}), vec![10]),
        (
            json!({"task":"t1", "text":"review", "since":"2026-01-03T00:00:00Z", "until":"2026-01-04T00:00:00Z"}),
            vec![12, 10],
        ),
        (json!({"text":"café 🔎"}), vec![12, 10]),
        (json!({"text":"REVIEW"}), vec![14]),
        (json!({"text":"review.*"}), vec![]),
        (json!({"text":"attachment-only"}), vec![]),
        (json!({"text":"t1"}), vec![]),
        (
            json!({"since":"2026-01-03T00:00:00Z", "until":"2026-01-03T00:00:00Z"}),
            vec![],
        ),
    ] {
        assert_eq!(
            ids(history(&state, &params(filter), &scope, 25).unwrap()),
            expected
        );
    }
    assert_eq!(
        ids(history(
            &state,
            &params(json!({"correspondent":"beta/lead"})),
            &HistoryScope::All,
            25
        )
        .unwrap()),
        [14, 12]
    );
    assert_eq!(
        ids(history(
            &state,
            &params(json!({"correspondent":"beta/lead"})),
            &delivery(),
            25
        )
        .unwrap()),
        [14]
    );
}

// Break: scanning a metadata-only inbox using a hidden-membership filter, even if empty.
#[test]
fn metadata_filters_refuse_before_any_scan() {
    for state in [OrgState::default(), fixture()] {
        for filter in [json!({"task":"private"}), json!({"text":"private"})] {
            assert!(matches!(
                history(&state, &params(filter), &delivery(), 25),
                Err(HistoryError::ForbiddenFilters)
            ));
        }
    }
}

// Break: permitting unbounded pages/cursors/text, empty search, or inverted endpoints.
#[test]
fn strict_bounds_and_shapes() {
    let state = fixture();
    for invalid in [
        json!({"limit":0}),
        json!({"limit":1001}),
        json!({"text":""}),
        json!({"text":"é".repeat(129)}),
        json!({"task":"é".repeat(129)}),
        json!({"cursor":"x".repeat(4097)}),
        json!({"since":"2026-01-04T00:00:00Z", "until":"2026-01-03T00:00:00Z"}),
    ] {
        assert!(matches!(
            history(&state, &params(invalid), &HistoryScope::All, 25),
            Err(HistoryError::InvalidParams(_))
        ));
    }
    for invalid in [
        json!({"limit":-1}),
        json!({"limit":65536}),
        json!({"extra":true}),
        json!({"since":"yesterday"}),
        json!({"agent":"worker"}),
        json!({"correspondent":"../lead"}),
    ] {
        assert!(serde_json::from_value::<HistoryParams>(invalid).is_err());
    }
    assert_eq!(params(json!({})).limit, 100);
    assert!(history(
        &state,
        &params(json!({"limit":1000,"text":"é".repeat(128)})),
        &HistoryScope::All,
        25
    )
    .is_ok());
}

// Break: new sends leaking into pages, last-lookahead cursor, or wall-time ordering.
#[test]
fn count_pages_are_fenced_exclusive_and_survive_reconstruction() {
    let mut state = fixture();
    let scope = conversation("alpha/worker", "alpha/worker");
    let mut p = params(json!({"limit":2}));
    let first = history(&state, &p, &scope, 25).unwrap();
    assert_eq!(ids(history(&state, &p, &scope, 25).unwrap()), [20, 16, 14]); // +1 lookahead
    p.cursor = Some(history_cursor(&p, &scope, first.snapshot_seq, MessageId::new(16)).unwrap());
    send(
        &mut state,
        26,
        1,
        "owner",
        "alpha/worker",
        None,
        "new visible",
    );
    send(
        &mut state,
        27,
        3,
        "owner",
        "beta/lead",
        None,
        "new invisible",
    );
    let rebuilt = state.clone();
    let second = history(&rebuilt, &p, &scope, 27).unwrap();
    assert_eq!(second.snapshot_seq, 25);
    assert_eq!(ids(second), [14, 12, 10]);
    p.cursor = Some(history_cursor(&p, &scope, 25, MessageId::new(12)).unwrap());
    assert_eq!(ids(history(&rebuilt, &p, &scope, 27).unwrap()), [10]);
    assert_eq!(state, rebuilt, "reads never fold or acknowledge");
}

// Break: cursor authority, changed filters/readers, noncanonical IDs, or invalid positions.
#[test]
fn cursor_binding_and_validation() {
    let state = fixture();
    let p = params(json!({"limit":1, "text":"review"}));
    let scope = conversation("alpha/worker", "alpha/worker");
    let token = history_cursor(&p, &scope, 25, MessageId::new(16)).unwrap();
    let with_cursor = HistoryParams {
        cursor: Some(token.clone()),
        ..p.clone()
    };
    assert_eq!(
        ids(history(&state, &with_cursor, &scope, 25).unwrap()),
        [12, 10]
    );
    for changed in [
        params(json!({"cursor":token})),
        params(json!({"cursor":token,"text":"REVIEW"})),
    ] {
        assert!(matches!(
            history(&state, &changed, &scope, 25),
            Err(HistoryError::InvalidParams(_))
        ));
    }
    for changed_scope in [
        HistoryScope::All,
        conversation("owner", "alpha/worker"),
        delivery(),
        conversation("beta/lead", "beta/lead"),
    ] {
        assert!(history(&state, &with_cursor, &changed_scope, 25).is_err());
    }
    let valid: Value = serde_json::from_str(&token).unwrap();
    for (key, bad) in [
        ("version", json!(2)),
        ("fence", json!(26)),
        ("last", json!("m016")),
        ("last", json!("m0")),
        ("last", json!("m26")),
        ("last", json!("m11")),
        ("last", json!("m14")),
        ("unknown", json!(true)),
    ] {
        let mut forged = valid.clone();
        forged[key] = bad;
        let bad_params = HistoryParams {
            cursor: Some(forged.to_string()),
            ..p.clone()
        };
        assert!(
            matches!(
                history(&state, &bad_params, &scope, 25),
                Err(HistoryError::InvalidParams(_))
            ),
            "{forged}"
        );
    }
    for malformed in ["", "[]", "null", "not json"] {
        let p = HistoryParams {
            cursor: Some(malformed.into()),
            ..p.clone()
        };
        assert!(history(&state, &p, &scope, 25).is_err());
    }
}

// Break: freezing mutable delivery/read facts rather than just membership.
#[test]
fn current_facts_refresh_inside_the_fence() {
    let mut state = fixture();
    let scope = conversation("alpha/worker", "alpha/worker");
    let mut p = params(json!({"limit":1}));
    p.cursor = Some(history_cursor(&p, &scope, 25, MessageId::new(20)).unwrap());
    for (seq, event) in [
        (
            26,
            OrgEvent::MessageDeliveryAttempted {
                message_id: MessageId::new(16),
                attempt: 1,
                pane: "%1".into(),
            },
        ),
        (
            27,
            OrgEvent::MessageDelivered {
                message_id: MessageId::new(16),
                session_id: "session".into(),
                confirmation: "receipt".into(),
            },
        ),
        (
            28,
            OrgEvent::MessageRead {
                message_ids: vec![MessageId::new(16)],
            },
        ),
    ] {
        state.apply(seq, at(5), &event).unwrap();
    }
    let page = history(&state, &p, &scope, 28).unwrap();
    assert_eq!(page.snapshot_seq, 25);
    let row = page.rows[0];
    assert_eq!(row.id.seq(), 16);
    assert_eq!(row.status, DeliveryStatus::Delivered);
    assert_eq!(row.read_at, Some(at(5)));
    assert_eq!(row.content.body, "review self");
}
