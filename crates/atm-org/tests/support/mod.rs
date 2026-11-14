#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)] // test builders only
use atm_core::{Harness, Money, SessionId};
use atm_org::*;
use chrono::{DateTime, Utc};
use serde_json::{json, Value};

pub fn time() -> DateTime<Utc> {
    "2026-01-15T12:00:00Z".parse().unwrap()
}
pub fn id() -> AgentId {
    "p/lead".parse().unwrap()
}
pub fn role() -> RoleSpec {
    parse_role("lead", "+++\nharness='pi'\n+++\nInstructions").unwrap()
}
pub fn content(to: Address) -> MessageContent {
    MessageContent {
        from: Address::Owner,
        to,
        kind: MessageKind::Brief,
        body: "Work".into(),
        attachments: vec!["brief.md".into()],
        task: None,
    }
}
pub fn summary() -> SessionSummary {
    SessionSummary {
        session_id: SessionId::new("s1"),
        pane: Some("%1".into()),
        agent_id: Some(id()),
        harness: Harness::Pi,
        model: Some("raw-model".into()),
        cost: Some(Money::from_microdollars(123)),
        first_prompt: Some("Work".into()),
        started: time(),
        ended: None,
    }
}
pub fn bodies() -> Vec<(&'static str, Value)> {
    vec![
        (
            "project.registered",
            json!({"project_id":"p", "root":"/project"}),
        ),
        (
            "agent.up_requested",
            json!({"agent_id":"p/lead", "role": {
                "name":"lead", "harness":"pi", "model":null, "cwd":".", "manager":null, "profile":null,
                "skills":[], "spawn_args":[], "command_prefix":[], "pane":null,"clear":null,"trust_dirs":[],"instructions":"Instructions"
            }}),
        ),
        (
            "agent.pane_created",
            json!({"agent_id":"p/lead","pane":"%1"}),
        ),
        (
            "agent.adopted",
            json!({"agent_id":"p/lead","pane":"%1","session_id":null}),
        ),
        (
            "agent.session_bound",
            json!({"agent_id":"p/lead","session_id":"s1"}),
        ),
        ("agent.session_lost", json!({"agent_id":"p/lead"})),
        ("agent.clear_requested", json!({"agent_id":"p/lead"})),
        (
            "agent.stuck",
            json!({"agent_id":"p/lead","reason":"timeout"}),
        ),
        ("agent.down", json!({"agent_id":"p/lead","reason":"done"})),
        (
            "msg.sent",
            json!({"from":"owner","to":"p/lead","kind":"brief","body":"Work","attachments":["brief.md"],"task":null}),
        ),
        (
            "msg.delivery_attempted",
            json!({"message_id":"m10","attempt":1,"pane":"%1"}),
        ),
        (
            "msg.delivered",
            json!({"message_id":"m10","session_id":"s1","confirmation":"prompt token"}),
        ),
        (
            "msg.delivery_failed",
            json!({"message_id":"m10","reason":"timeout","final":false}),
        ),
        ("msg.read", json!({"message_ids":["m10"]})),
        (
            "session.summary",
            json!({"session_id":"s1","pane":"%1","agent_id":"p/lead","harness":"pi","model":"raw-model","cost":{"microdollars":123},"first_prompt":"Work","started":"2026-01-15T12:00:00Z","ended":null}),
        ),
        (
            "control.sent",
            json!({"pane":"%1","action":{"text":"hello"}}),
        ),
        (
            "control.spawned",
            json!({"pane":"%1","action":{"harness":"pi"}}),
        ),
        ("control.killed", json!({"pane":"%1","action":"kill"})),
    ]
}
