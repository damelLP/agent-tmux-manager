#![allow(clippy::unwrap_used, clippy::expect_used)] // test generators/helpers only
mod support;
use atm_core::Money;
use atm_org::*;
use proptest::prelude::*;
use serde_json::{json, Value};
use support::*;

#[test]
fn unknown_kind_is_skipped_and_counted_without_touching_state() {
    let mut state = OrgState::default();
    let mut stats = ReplayStats::default();
    stats
        .apply_parts(
            &mut state,
            1,
            time(),
            "project.registered",
            &json!({"project_id":"p","root":"/p"}),
        )
        .unwrap();
    let before = state.clone();
    stats
        .apply_parts(&mut state, 2, time(), "future.kind", &Value::Null)
        .unwrap();
    assert_eq!(state, before);
    assert_eq!(stats.applied, 1);
    assert_eq!(stats.skipped_unknown, 1);
    stats
        .apply_parts(
            &mut state,
            3,
            time(),
            "control.sent",
            &json!({"pane":"%1","action":"hello"}),
        )
        .unwrap();
    assert_eq!(stats.applied, 2);
    let before_stats = stats.clone();
    assert!(stats
        .apply_parts(&mut state, 4, time(), "agent.down", &json!({}))
        .is_err());
    assert!(stats
        .apply_parts(
            &mut state,
            4,
            time(),
            "agent.down",
            &json!({"agent_id":"p/unknown","reason":"gone"})
        )
        .is_err());
    assert_eq!(stats, before_stats);
    assert_eq!(state, before);
}

#[test]
fn replay_counter_overflow_precedes_mutation() {
    let mut state = OrgState::default();
    let mut stats = ReplayStats {
        applied: u64::MAX,
        skipped_unknown: u64::MAX,
    };
    let before = state.clone();
    assert!(stats
        .apply_parts(
            &mut state,
            1,
            time(),
            "project.registered",
            &json!({"project_id":"p","root":"/p"})
        )
        .is_err());
    assert!(stats
        .apply_parts(&mut state, 1, time(), "future.kind", &json!({}))
        .is_err());
    assert_eq!(state, before);
    assert_eq!(stats.applied, u64::MAX);
    assert_eq!(stats.skipped_unknown, u64::MAX);
}

// Exhaustive matches deliberately fail compilation if a variant is added but
// these generators/coverage expectations have not been considered.
fn event_slot(event: &OrgEvent) -> usize {
    match event {
        OrgEvent::ProjectRegistered { .. } => 0,
        OrgEvent::AgentUpRequested { .. } => 1,
        OrgEvent::AgentPaneCreated { .. } => 2,
        OrgEvent::AgentAdopted { .. } => 3,
        OrgEvent::AgentSessionBound { .. } => 4,
        OrgEvent::AgentSessionLost { .. } => 5,
        OrgEvent::AgentClearRequested { .. } => 6,
        OrgEvent::AgentStuck { .. } => 7,
        OrgEvent::AgentDown { .. } => 8,
        OrgEvent::MessageSent(_) => 9,
        OrgEvent::MessageDeliveryAttempted { .. } => 10,
        OrgEvent::MessageDelivered { .. } => 11,
        OrgEvent::MessageDeliveryFailed { .. } => 12,
        OrgEvent::MessageRead { .. } => 13,
        OrgEvent::SessionSummary(_) => 14,
        OrgEvent::ControlSent(_) => 15,
        OrgEvent::ControlSpawned(_) => 16,
        OrgEvent::ControlKilled(_) => 17,
    }
}

fn event_case(slot: usize, text: String, n: u64, cost: i64, flag: bool) -> OrgEvent {
    let message_id = MessageId::new(n);
    let agent_id = id();
    let audit = ControlAudit {
        pane: text.clone(),
        action: json!({"nested":[null, flag, n, text]}),
    };
    match slot {
        0 => OrgEvent::ProjectRegistered {
            project_id: "p".parse().unwrap(),
            root: text,
        },
        1 => {
            let mut role = role();
            role.instructions = text;
            OrgEvent::AgentUpRequested { agent_id, role }
        }
        2 => OrgEvent::AgentPaneCreated {
            agent_id,
            pane: text,
        },
        3 => OrgEvent::AgentAdopted {
            agent_id,
            pane: text.clone(),
            session_id: flag.then(|| text.into()),
        },
        4 => OrgEvent::AgentSessionBound {
            agent_id,
            session_id: text.into(),
        },
        5 => OrgEvent::AgentSessionLost { agent_id },
        6 => OrgEvent::AgentClearRequested { agent_id },
        7 => OrgEvent::AgentStuck {
            agent_id,
            reason: text,
        },
        8 => OrgEvent::AgentDown {
            agent_id,
            reason: text,
        },
        9 => {
            let mut c = content(Address::Agent(agent_id));
            c.body = text;
            c.task = flag.then(|| format!("task{n}").parse().unwrap());
            c.kind = match n % 6 {
                0 => MessageKind::Note,
                1 => MessageKind::Brief,
                2 => MessageKind::Report,
                3 => MessageKind::Question,
                4 => MessageKind::Answer,
                _ => MessageKind::Review,
            };
            OrgEvent::MessageSent(c)
        }
        10 => OrgEvent::MessageDeliveryAttempted {
            message_id,
            attempt: n as u32,
            pane: text,
        },
        11 => OrgEvent::MessageDelivered {
            message_id,
            session_id: text.clone().into(),
            confirmation: text,
        },
        12 => OrgEvent::MessageDeliveryFailed {
            message_id,
            reason: text,
            final_failure: flag,
        },
        13 => OrgEvent::MessageRead {
            message_ids: vec![message_id],
        },
        14 => {
            let mut s = summary();
            s.cost = flag.then(|| Money::from_microdollars(cost));
            s.model = Some(text);
            OrgEvent::SessionSummary(s)
        }
        15 => OrgEvent::ControlSent(audit),
        16 => OrgEvent::ControlSpawned(audit),
        _ => OrgEvent::ControlKilled(audit),
    }
}

fn command_slot(command: &OrgCommand) -> usize {
    match command {
        OrgCommand::RegisterProject { .. } => 0,
        OrgCommand::Up { .. } => 1,
        OrgCommand::Adopt { .. } => 2,
        OrgCommand::Clear { .. } => 3,
        OrgCommand::Down { .. } => 4,
        OrgCommand::Respawn { .. } => 5,
        OrgCommand::PaneCreated { .. } => 6,
        OrgCommand::SessionBound { .. } => 7,
        OrgCommand::SessionLost { .. } => 8,
        OrgCommand::Stuck { .. } => 9,
        OrgCommand::SendMessage(_) => 10,
        OrgCommand::DeliveryAttempted { .. } => 11,
        OrgCommand::Delivered { .. } => 12,
        OrgCommand::DeliveryFailed { .. } => 13,
        OrgCommand::ReadMessages { .. } => 14,
        OrgCommand::RecordSummary(_) => 15,
        OrgCommand::ControlSent(_) => 16,
        OrgCommand::ControlSpawned(_) => 17,
        OrgCommand::ControlKilled(_) => 18,
    }
}

fn command_case(slot: usize, n: u64, flag: bool) -> OrgCommand {
    let agent_id = id();
    let pane = format!("%{}", n % 3);
    let session_id = format!("s{}", n % 3).into();
    let message_id = MessageId::new(n);
    let audit = ControlAudit {
        pane: pane.clone(),
        action: json!({"n":n}),
    };
    match slot {
        0 => OrgCommand::RegisterProject {
            project_id: "p".parse().unwrap(),
            root: format!("/p{n}"),
        },
        1 => OrgCommand::Up {
            agent_id,
            role: role(),
        },
        2 => OrgCommand::Adopt {
            agent_id,
            pane,
            session_id: flag.then_some(session_id),
        },
        3 => OrgCommand::Clear { agent_id },
        4 => OrgCommand::Down {
            agent_id,
            reason: "stop".into(),
        },
        5 => OrgCommand::Respawn {
            agent_id,
            role: role(),
            reason: "restart".into(),
        },
        6 => OrgCommand::PaneCreated { agent_id, pane },
        7 => OrgCommand::SessionBound {
            agent_id,
            session_id,
        },
        8 => OrgCommand::SessionLost { agent_id },
        9 => OrgCommand::Stuck {
            agent_id,
            reason: "late".into(),
        },
        10 => OrgCommand::SendMessage(content(if flag {
            Address::Owner
        } else {
            Address::Agent(agent_id)
        })),
        11 => OrgCommand::DeliveryAttempted {
            message_id,
            attempt: n as u32,
            pane,
        },
        12 => OrgCommand::Delivered {
            message_id,
            session_id,
            confirmation: "token".into(),
        },
        13 => OrgCommand::DeliveryFailed {
            message_id,
            reason: "timeout".into(),
            final_failure: flag,
        },
        14 => OrgCommand::ReadMessages {
            who: if flag {
                Address::Owner
            } else {
                Address::Agent(agent_id)
            },
            message_ids: vec![message_id],
        },
        15 => {
            let mut s = summary();
            s.cost = Some(Money::from_microdollars(n as i64));
            OrgCommand::RecordSummary(s)
        }
        16 => OrgCommand::ControlSent(audit),
        17 => OrgCommand::ControlSpawned(audit),
        _ => OrgCommand::ControlKilled(audit),
    }
}

#[test]
fn case_generators_cover_every_event_and_command_variant() {
    let events: Vec<_> = (0..18)
        .map(|slot| event_slot(&event_case(slot, "text".into(), 1, 1, true)))
        .collect();
    assert_eq!(events, (0..18).collect::<Vec<_>>());
    let commands: Vec<_> = (0..19)
        .map(|slot| command_slot(&command_case(slot, 1, true)))
        .collect();
    assert_eq!(commands, (0..19).collect::<Vec<_>>());
}

// Synthetic: a restart-style history exercising clear, retry, owner delivery
// and summaries, not a captured harness payload.
fn seeded_log() -> Vec<OrgEvent> {
    let mut log = Vec::new();
    let mut state = OrgState::default();
    let mut commands = vec![
        command_case(0, 1, false),
        command_case(1, 1, false),
        command_case(6, 1, false),
        command_case(7, 1, false),
        command_case(3, 1, false),
        command_case(7, 2, false),
        command_case(10, 1, false), // msg.sent sequence 7
        command_case(11, 7, false),
        command_case(13, 7, false),
        OrgCommand::DeliveryAttempted {
            message_id: MessageId::new(7),
            attempt: 8,
            pane: "%1".into(),
        },
        command_case(12, 7, false),
        command_case(14, 7, false),
        command_case(10, 1, true),
        command_case(15, 5, false),
    ];
    commands.extend((16..19).map(|slot| command_case(slot, 1, true)));
    for command in commands {
        for event in state.decide(&command, time()).unwrap() {
            state.apply(log.len() as u64 + 1, time(), &event).unwrap();
            log.push(event);
        }
    }
    assert_eq!(state.agent(&id()).unwrap().status, AgentStatus::Ready);
    assert_eq!(
        state.message(&MessageId::new(7)).unwrap().status,
        DeliveryStatus::Delivered
    );
    assert!(state.message(&MessageId::new(7)).unwrap().read_at.is_some());
    assert_eq!(state.inbox(&Address::Owner).len(), 1);
    assert_eq!(state.agent_cost(&id()), Some(Money::from_microdollars(5)));
    log
}

fn fold(log: &[(String, Value)], state: &mut OrgState, stats: &mut ReplayStats, offset: usize) {
    for (index, (kind, body)) in log.iter().enumerate() {
        stats
            .apply_parts(state, (offset + index + 1) as u64, time(), kind, body)
            .unwrap();
    }
}

proptest! {
    #[test]
    fn event_parts_roundtrip(slot in 0usize..18, text in any::<String>(), n in any::<u64>(), cost in any::<i64>(), flag in any::<bool>()) {
        let event = event_case(slot, text, n, cost, flag);
        prop_assert_eq!(OrgEvent::from_parts(event.kind(), &event.to_body()).unwrap(), Some(event));
    }

    #[test]
    fn applying_a_log_is_deterministic_and_prefix_composes(actions in prop::collection::vec((0usize..19, 0u64..50, any::<bool>()), 0..80), split in any::<usize>()) {
        let mut events = seeded_log();
        let mut reachable = OrgState::default();
        for (index, event) in events.iter().enumerate() { reachable.apply(index as u64 + 1, time(), event).unwrap(); }
        for (slot, n, flag) in actions {
            let command = command_case(slot, n, flag);
            let before = reachable.clone();
            let result = reachable.decide(&command, time());
            prop_assert_eq!(&reachable, &before);
            if let Ok(next) = result {
                for event in next {
                    prop_assert!(reachable.apply(events.len() as u64 + 1, time(), &event).is_ok());
                    events.push(event);
                }
            }
            // Every command is tried at every generated reachable state, not
            // only the randomly selected command that advanced the history.
            for candidate in 0..19 {
                let mut probe = reachable.clone();
                if let Ok(next) = probe.decide(&command_case(candidate, n, flag), time()) {
                    for (index, event) in next.iter().enumerate() {
                        prop_assert!(probe.apply((events.len() + index + 1) as u64, time(), event).is_ok());
                    }
                }
            }
        }
        let mut log: Vec<_> = events.iter().map(|e| (e.kind().to_owned(), e.to_body())).collect();
        // Unknown events inserted only at the end preserve the literal message
        // IDs in the generated history; seq gaps are valid, not renumbered IDs.
        log.extend([("future.kind".into(), json!(null)), ("future.kind".into(), json!([1,2]))]);
        let mut whole = OrgState::default(); let mut whole_stats = ReplayStats::default();
        fold(&log, &mut whole, &mut whole_stats, 0);
        prop_assert_eq!(&whole, &reachable);
        let mut repeat = OrgState::default(); let mut repeat_stats = ReplayStats::default();
        fold(&log, &mut repeat, &mut repeat_stats, 0);
        prop_assert_eq!(&whole, &repeat);
        prop_assert_eq!(&whole_stats, &repeat_stats);
        let split = split % (log.len() + 1);
        let mut partial = OrgState::default(); let mut partial_stats = ReplayStats::default();
        fold(&log[..split], &mut partial, &mut partial_stats, 0);
        fold(&log[split..], &mut partial, &mut partial_stats, split);
        prop_assert_eq!(whole, partial);
        prop_assert_eq!(whole_stats, partial_stats);
        prop_assert_eq!(repeat_stats.skipped_unknown, 2);
    }

    #[test]
    fn arbitrary_config_and_event_text_never_panics(text in any::<String>()) {
        let _ = parse_project(&text);
        let _ = parse_role("lead", &text);
        let _ = text.parse::<AgentId>();
        for (kind, _) in bodies() { let _ = OrgEvent::from_parts(kind, &json!({"agent_id":text})); }
        prop_assert_eq!(OrgEvent::from_parts("future", &json!(text)).unwrap(), None);
    }
}
