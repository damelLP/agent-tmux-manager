//! The start line's text, its recognition, when one is due, and the evidence
//! that confirms it. Events and prompts here are hand-written.

use super::message_delivery::inbox_reference;
use super::message_evidence::{AttemptScope, MessageEvidence};
use super::messages::{focus_notice, MessageClock};
use super::start_line::{
    delivery_line, is_start_line, start_line, start_line_due, start_reference,
};
use crate::control::reservation::PaneId;
use atm_core::{Harness, LifecycleEvent, SessionId};
use atm_org::{Address, AgentId, MessageContent, MessageId, MessageKind, OrgEvent, OrgState};
use chrono::{DateTime, Utc};
use std::collections::HashSet;

fn agent(name: &str) -> AgentId {
    format!("demo/{name}").parse().unwrap()
}

fn at() -> DateTime<Utc> {
    DateTime::from_timestamp(1_700_000_000, 0).unwrap()
}

#[test]
fn start_line_is_one_printable_line_never_longer_than_the_reference() {
    for seq in [1, 9, 10, 123, 123_456, u64::MAX] {
        let id = MessageId::new(seq);
        let line = start_reference(id);
        assert_eq!(line, format!("[m{seq}] New session: run atm me"));
        assert!(line.bytes().all(|b| (b' '..=b'~').contains(&b)), "{line:?}");
        assert!(!line.ends_with(' '));
        // The adapters refuse a wrapped draft: the line must not wrap where
        // the reference every other message is typed as would not.
        assert!(line.len() <= inbox_reference(id).len(), "{line:?}");
    }
}

#[test]
fn only_the_canonical_start_line_is_typed_as_itself() {
    let id = MessageId::new(42);
    let line = start_line(&agent("lead"));
    assert_eq!(
        line,
        MessageContent {
            from: Address::System,
            to: Address::Agent(agent("lead")),
            kind: MessageKind::Note,
            body: "New session: run atm me".into(),
            attachments: vec![],
            task: None,
        }
    );
    assert!(is_start_line(&line));
    assert_eq!(delivery_line(id, &line), "[m42] New session: run atm me");
    type Mutation = fn(&mut MessageContent);
    let mutations: Vec<(&str, Mutation)> = vec![
        ("from the owner", |c| c.from = Address::Owner),
        ("from an agent", |c| c.from = Address::Agent(agent("other"))),
        ("to the owner", |c| c.to = Address::Owner),
        ("a brief", |c| c.kind = MessageKind::Brief),
        ("a longer body", |c| c.body.push('!')),
        ("a shorter body", |c| {
            c.body.pop();
        }),
        ("an attachment", |c| {
            c.attachments.push("/tmp/brief.md".into())
        }),
        ("a task", |c| c.task = Some("t7".parse().unwrap())),
    ];
    for (case, mutate) in mutations {
        let mut content = start_line(&agent("lead"));
        mutate(&mut content);
        assert!(!is_start_line(&content), "{case}");
        assert_eq!(delivery_line(id, &content), inbox_reference(id), "{case}");
    }
    let notice = focus_notice(id, &PaneId::parse("%7").unwrap());
    assert!(!is_start_line(&notice));
}

/// An organisation with `demo/lead` and `demo/other` adopted, and the events
/// given folded in order from sequence 4 on.
fn state(events: &[OrgEvent]) -> OrgState {
    let mut state = OrgState::default();
    let mut seq = 0;
    let mut fold = |event: &OrgEvent| {
        seq += 1;
        state.apply(seq, at(), event).unwrap();
    };
    fold(&OrgEvent::ProjectRegistered {
        project_id: "demo".parse().unwrap(),
        root: "/demo".into(),
    });
    for (name, pane) in [("lead", "%1"), ("other", "%2")] {
        fold(&OrgEvent::AgentAdopted {
            agent_id: agent(name),
            pane: pane.into(),
            session_id: Some(SessionId::new(format!("{name}-old"))),
        });
    }
    for event in events {
        fold(event);
    }
    state
}

fn sent(content: MessageContent) -> OrgEvent {
    OrgEvent::MessageSent(content)
}
fn attempted(seq: u64) -> OrgEvent {
    OrgEvent::MessageDeliveryAttempted {
        message_id: MessageId::new(seq),
        attempt: 1,
        pane: "%1".into(),
    }
}
fn delivered(seq: u64, session: &str) -> OrgEvent {
    OrgEvent::MessageDelivered {
        message_id: MessageId::new(seq),
        session_id: SessionId::new(session),
        confirmation: "prompt_submit_exact_reference".into(),
    }
}
fn given_up(seq: u64) -> OrgEvent {
    OrgEvent::MessageDeliveryFailed {
        message_id: MessageId::new(seq),
        reason: "owner gave up on delivery".into(),
        final_failure: true,
    }
}

#[test]
fn a_start_line_is_due_unless_one_is_open_or_this_session_had_one() {
    let lead = agent("lead");
    let (s1, s2) = (SessionId::new("s1"), SessionId::new("s2"));
    let line = || sent(start_line(&lead));
    let note = |to: Address| {
        sent(MessageContent {
            from: Address::System,
            to,
            kind: MessageKind::Note,
            body: "agent demo/lead: start did not complete".into(),
            attachments: vec![],
            task: None,
        })
    };
    // The first event after the setup has sequence 4.
    let cases: Vec<(&str, Vec<OrgEvent>, Option<&SessionId>, bool)> = vec![
        ("none sent", vec![], Some(&s1), true),
        ("queued", vec![line()], Some(&s2), false),
        ("queued, no session", vec![line()], None, false),
        ("delivering", vec![line(), attempted(4)], Some(&s2), false),
        (
            "delivered to this session",
            vec![line(), attempted(4), delivered(4, "s1")],
            Some(&s1),
            false,
        ),
        (
            "delivered to another session",
            vec![line(), attempted(4), delivered(4, "s1")],
            Some(&s2),
            true,
        ),
        (
            "delivered, a ready editor without a session",
            vec![line(), attempted(4), delivered(4, "s1")],
            None,
            true,
        ),
        ("given up", vec![line(), given_up(4)], Some(&s1), true),
        (
            "another agent's line is open",
            vec![sent(start_line(&agent("other")))],
            Some(&s1),
            true,
        ),
        (
            "a system note to the agent, open",
            vec![note(Address::Agent(lead.clone()))],
            Some(&s1),
            true,
        ),
        (
            "a stuck note to the owner",
            vec![note(Address::Owner)],
            Some(&s1),
            true,
        ),
        (
            "an older line delivered, a newer one open",
            vec![line(), attempted(4), delivered(4, "s1"), line()],
            Some(&s2),
            false,
        ),
    ];
    for (case, events, session, due) in cases {
        assert_eq!(
            start_line_due(&state(&events), &lead, session),
            due,
            "{case}"
        );
    }
}

fn evidence() -> MessageEvidence {
    MessageEvidence::new(MessageClock::new(tokio::time::Instant::now(), at()))
}

#[test]
fn evidence_expects_either_line_of_its_own_id_and_nothing_else() {
    let id = MessageId::new(123);
    for line in [inbox_reference(id), start_reference(id)] {
        evidence().register(id, line.clone()).unwrap();
    }
    for line in [
        start_reference(MessageId::new(124)),
        "[m123] New session: run atm me ".into(),
        "[m123] New session".into(),
        "New session: run atm me".into(),
    ] {
        assert!(evidence().register(id, line.clone()).is_err(), "{line:?}");
    }
}

#[tokio::test]
async fn only_the_start_text_confirms_a_start_line() {
    let id = MessageId::new(123);
    let armed = || {
        let evidence = evidence();
        evidence.register(id, start_reference(id)).unwrap();
        evidence
            .arm(AttemptScope {
                message_id: id,
                attempt_seq: 124,
                recipient: agent("lead"),
                pane: PaneId::parse("%1").unwrap(),
                harness: Harness::Pi,
                session_id: Some(SessionId::new("s1")),
                excluded_sessions: HashSet::new(),
            })
            .unwrap();
        evidence
    };
    let prompt = |evidence: &MessageEvidence, text: String| {
        let event = LifecycleEvent::PromptSubmit { prompt: Some(text) };
        evidence
            .observe(&SessionId::new("s1"), Some("%1"), Harness::Pi, &event)
            .unwrap();
    };
    let other = armed();
    prompt(&other, inbox_reference(id));
    assert_eq!(other.receipt(id).unwrap(), None);
    let confirmed = armed();
    prompt(&confirmed, start_reference(id));
    let receipt = confirmed.receipt(id).unwrap().unwrap();
    assert_eq!(receipt.session_id.as_str(), "s1");
    assert_eq!(receipt.confirmation, "prompt_submit_exact_reference");
}
