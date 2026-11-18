#![allow(clippy::unwrap_used, clippy::expect_used)] // test generators/helpers only
#[path = "support/decision.rs"]
mod decision_support;
#[path = "support/history.rs"]
mod history;
mod support;
use atm_core::Money;
use atm_org::*;
use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed, TestRunner};
use serde_json::{json, Value};
use std::{cell::RefCell, collections::BTreeMap};
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

// Break caught: losing designation/provenance on restart or after a final answer,
// or emitting facts that cannot reconstruct the same state. Synthetic fixture.
#[test]
fn relay_fixture_matches_decided_history_and_every_replay_split() {
    let lines: Vec<decision_support::Line> = include_str!("fixtures/decision_relay.jsonl")
        .lines()
        .map(|line| {
            let row: Value = serde_json::from_str(line).unwrap();
            decision_support::Line {
                seq: row["seq"].as_u64().unwrap(),
                ts: row["ts"].as_str().unwrap().parse().unwrap(),
                kind: row["kind"].as_str().unwrap().into(),
                body: row["body"].clone(),
            }
        })
        .collect();
    let mut live = OrgState::default();
    for line in &lines {
        let fact = OrgEvent::from_parts(&line.kind, &line.body)
            .unwrap()
            .unwrap();
        let command = match &fact {
            OrgEvent::ProjectRegistered { project_id, root } => OrgCommand::RegisterProject {
                project_id: project_id.clone(),
                root: root.clone(),
                move_root: false,
            },
            OrgEvent::AgentAdopted {
                agent_id,
                pane,
                session_id,
            } => OrgCommand::Adopt {
                agent_id: agent_id.clone(),
                pane: pane.clone(),
                session_id: session_id.clone(),
            },
            OrgEvent::OwnerRelaySet(f) => OrgCommand::Owner(OwnerCommand::SetRelay {
                by: f.by.clone(),
                agent: f.agent.clone(),
            }),
            OrgEvent::OwnerAway(f) => OrgCommand::Owner(OwnerCommand::Away {
                by: f.by.clone(),
                stacking_caps: f.stacking_caps.clone(),
            }),
            OrgEvent::OwnerBack(f) => OrgCommand::Owner(OwnerCommand::Back { by: f.by.clone() }),
            OrgEvent::DecisionRequested(f) => {
                OrgCommand::Decision(DecisionCommand::Request(f.clone()))
            }
            OrgEvent::DecisionTakenProvisionally(f) => OrgCommand::Decision(match &f.relayed_by {
                Some(by) => DecisionCommand::Relay {
                    decision_id: f.decision_id,
                    option: f.option.clone(),
                    by: by.clone(),
                },
                None => DecisionCommand::TakeProvisionally(f.clone()),
            }),
            OrgEvent::DecisionResolved(f) | OrgEvent::DecisionOverruled(f) => {
                OrgCommand::Decision(DecisionCommand::Resolve(f.resolution.clone()))
            }
            other => panic!("unexpected fixture event: {other:?}"),
        };
        let events = live.decide(&command, line.ts).unwrap();
        assert_eq!(events, vec![fact.clone()], "fixture seq {}", line.seq);
        live.apply(line.seq, line.ts, &fact).unwrap();
    }
    for cut in 0..=lines.len() {
        let mut replayed = OrgState::default();
        let mut stats = ReplayStats::default();
        for part in [&lines[..cut], &lines[cut..]] {
            for line in part {
                stats
                    .apply_parts(&mut replayed, line.seq, line.ts, &line.kind, &line.body)
                    .unwrap();
            }
        }
        assert_eq!(replayed, live, "split {cut}");
        assert_eq!((stats.applied, stats.skipped_unknown), (16, 0));
    }
    assert_eq!(live.owner().relay, None);
    assert_eq!(live.owner().away_since, None);
    assert_eq!(live.owner().stacking_cap(Some(&"p".parse().unwrap())), 2);
    for (seq, status, relayer) in [
        (5, DecisionStatus::Resolved, Some("p/lead")),
        (9, DecisionStatus::Overruled, Some("p/builder")),
        (13, DecisionStatus::TakenProvisionally, None),
    ] {
        let decision = live.decision(&DecisionId::new(seq)).unwrap();
        assert_eq!(decision.status, status);
        assert_eq!(
            decision.provisional.as_ref().unwrap().relayed_by,
            relayer.map(|id| id.parse().unwrap())
        );
    }
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
        OrgEvent::AgentEditorReady { .. } => 18,
        OrgEvent::AgentRevalidationRequested { .. } => 19,
        OrgEvent::DecisionRequested(_) => 20,
        OrgEvent::DecisionTakenProvisionally(_) => 21,
        OrgEvent::DecisionResolved(_) => 22,
        OrgEvent::DecisionOverruled(_) => 23,
        OrgEvent::DecisionExpired { .. } => 24,
        OrgEvent::DecisionWithdrawn(_) => 25,
        OrgEvent::TaskCreated(_) => 26,
        OrgEvent::TaskAssigned(_) => 27,
        OrgEvent::TaskStateChanged(_) => 28,
        OrgEvent::TaskSubmitted(_) => 29,
        OrgEvent::TaskReviewStarted(_) => 30,
        OrgEvent::TaskVerdict(_) => 31,
        OrgEvent::TaskAttached(_) => 32,
        OrgEvent::TaskReminded(_) => 33,
        OrgEvent::TaskFlagged(_) => 34,
        OrgEvent::TaskCostRecorded(_) => 35,
        OrgEvent::AgentSaveRequested { .. } => 36,
        OrgEvent::AgentSaved { .. } => 37,
        OrgEvent::OwnerAway(_) => 38,
        OrgEvent::OwnerBack(_) => 39,
        OrgEvent::TaskUsageObserved(_) => 40,
        OrgEvent::OwnerDigestCreated(_) => 41,
        OrgEvent::OwnerRelaySet(_) => 42,
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
        17 => OrgEvent::ControlKilled(audit),
        18 => OrgEvent::AgentEditorReady { agent_id },
        19 => OrgEvent::AgentRevalidationRequested { agent_id },
        20..=25 => decision_support::decision_event_case(slot - 20, text, n, flag),
        26..=35 => task_event_case(slot, text, n, cost, flag),
        36 => OrgEvent::AgentSaveRequested {
            agent_id,
            message_id,
        },
        37 => OrgEvent::AgentSaved {
            agent_id,
            message_id,
        },
        38 => OrgEvent::OwnerAway(OwnerAway {
            by: owner_or(flag, agent_id),
            stacking_caps: [("p".parse().unwrap(), (n % 5) as u32)]
                .into_iter()
                .filter(|_| !n.is_multiple_of(7))
                .collect(),
        }),
        42 => OrgEvent::OwnerRelaySet(OwnerRelaySet {
            by: owner_or(flag, agent_id.clone()),
            agent: flag.then_some(agent_id),
        }),
        41 => OrgEvent::OwnerDigestCreated(OwnerDigestCreated {
            by: owner_or(flag, agent_id),
            back_seq: flag.then_some(n),
        }),
        40 => OrgEvent::TaskUsageObserved(TaskUsageObserved {
            session_id: text.into(),
            task_id: flag.then(|| format!("t{n}").parse().unwrap()),
            turn: flag,
            cumulative_cost: Some(Money::from_microdollars(cost)),
            context_percent: Some((n % 101) as u8),
        }),
        _ => OrgEvent::OwnerBack(OwnerBack {
            by: owner_or(flag, agent_id),
        }),
    }
}

fn owner_or(owner: bool, agent_id: AgentId) -> Address {
    if owner {
        Address::Owner
    } else {
        Address::Agent(agent_id)
    }
}

const TASK_STATES: [TaskState; 10] = [
    TaskState::Open,
    TaskState::Assigned,
    TaskState::Working,
    TaskState::Waiting,
    TaskState::Blocked,
    TaskState::Submitted,
    TaskState::InReview,
    TaskState::Accepted,
    TaskState::ChangesRequested,
    TaskState::Cancelled,
];

/// Slots 26 to 35: one task event of each kind, with arbitrary text and ids.
fn task_event_case(slot: usize, text: String, n: u64, cost: i64, flag: bool) -> OrgEvent {
    let pick = n as usize;
    let task_id: TaskId = format!("t{n}").parse().unwrap();
    let by = if flag {
        Address::Owner
    } else {
        Address::Agent(id())
    };
    let file = TaskFile {
        path: text.clone(),
        kind: [
            TaskFileKind::Brief,
            TaskFileKind::Report,
            TaskFileKind::Evidence,
            TaskFileKind::Plan,
            TaskFileKind::Contract,
        ][pick % 5],
        note: flag.then(|| text.clone()),
    };
    match slot {
        26 => OrgEvent::TaskCreated(TaskCreated {
            project_id: None,
            title: text.clone(),
            kind: [
                TaskKind::Work,
                TaskKind::Review,
                TaskKind::Plan,
                TaskKind::Errand,
                TaskKind::DriftCheck,
            ][pick % 5],
            creator: by,
            parent: flag.then(|| task_id.clone()),
            judges: (!flag).then(|| task_id.clone()),
            measure: match pick % 3 {
                0 => TaskMeasure::Mechanical {
                    command: text.clone(),
                },
                1 => TaskMeasure::Reviewed {
                    rubric: text.clone(),
                },
                _ => TaskMeasure::Owner,
            },
            budget: flag.then(|| TaskBudget {
                cost: pick
                    .is_multiple_of(2)
                    .then(|| Money::from_microdollars(cost)),
                turns: Some(n as u32),
                deadline: Some(time()),
            }),
            files: vec![file],
            rests_on: flag.then_some(DecisionId::new(n)),
        }),
        27 => OrgEvent::TaskAssigned(TaskAssigned {
            task_id,
            agent_id: id(),
            by,
        }),
        28 => OrgEvent::TaskStateChanged(TaskStateChanged {
            task_id,
            state: TASK_STATES[pick % 10],
            reason: flag.then_some(text),
            by,
        }),
        29 => OrgEvent::TaskSubmitted(TaskSubmitted {
            task_id,
            what: if flag {
                TaskSubmission::Plan
            } else {
                TaskSubmission::Result
            },
            files: vec![file],
            by,
        }),
        30 => OrgEvent::TaskReviewStarted(TaskReviewStarted {
            task_id: task_id.clone(),
            review_task: task_id,
            by,
        }),
        31 => OrgEvent::TaskVerdict(TaskVerdictGiven {
            task_id: task_id.clone(),
            verdict: if flag {
                TaskVerdict::Accepted
            } else {
                TaskVerdict::ChangesRequested
            },
            review_task: flag.then_some(task_id),
            note: (!flag).then_some(text),
            by,
        }),
        32 => OrgEvent::TaskAttached(TaskAttached { task_id, file, by }),
        33 => OrgEvent::TaskReminded(TaskReminded {
            task_id,
            agent_id: id(),
        }),
        34 => OrgEvent::TaskFlagged(TaskFlagged { task_id, to: by }),
        _ => OrgEvent::TaskCostRecorded(TaskCostRecorded {
            task_id,
            record: TaskCost {
                turns: Some(n as u32),
                review_rounds: flag.then_some(1),
                changes_requested: None,
                cost: flag.then(|| Money::from_microdollars(cost)),
                peak_context_percent: Some((n % 101) as u8),
                seconds_in_state: [(TASK_STATES[pick % 10], n)].into(),
            },
        }),
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
        OrgCommand::EditorReady { .. } => 19,
        OrgCommand::Revalidate { .. } => 20,
        OrgCommand::Decision(_) => 21,
        OrgCommand::Task(_) => 22,
        OrgCommand::SaveRequested { .. } => 23,
        OrgCommand::Saved { .. } => 24,
        OrgCommand::Owner(_) => 25,
        OrgCommand::ObserveTaskUsage(_) => 26,
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
            move_root: flag,
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
        18 => OrgCommand::ControlKilled(audit),
        19 => OrgCommand::EditorReady { agent_id },
        20 => OrgCommand::Revalidate { agent_id },
        21 => OrgCommand::Decision(decision_support::decision_command(
            n,
            DecisionId::new(n),
            Address::Agent(agent_id),
            flag.then(|| time() + chrono::Duration::milliseconds(n as i64)),
            flag,
        )),
        22 => OrgCommand::Task(task_command_case(n, flag)),
        23 => OrgCommand::SaveRequested {
            agent_id,
            message_id,
        },
        24 => OrgCommand::Saved {
            agent_id,
            message_id,
        },
        26 => OrgCommand::ObserveTaskUsage(TaskUsageObservation {
            session_id,
            pane: Some(pane),
            turn: flag,
            cumulative_cost: Some(Money::from_microdollars((n % 1000000) as i64)),
            context_percent: Some((n % 101) as u8),
        }),
        // Slot 25: the switch by the owner or an agent, caps from zero up, and
        // the away table applied to the decision ids a history reaches.
        _ => OrgCommand::Owner(match n % 4 {
            0 => OwnerCommand::Away {
                by: owner_or(flag, agent_id),
                stacking_caps: [("p".parse().unwrap(), (n % 4) as u32)].into(),
            },
            1 => OwnerCommand::Back {
                by: owner_or(flag, agent_id),
            },
            2 => OwnerCommand::ApplyAway {
                decision_id: DecisionId::new(n),
            },
            _ => OwnerCommand::CreateDigest {
                by: owner_or(flag, agent_id),
                back_seq: None,
            },
        }),
    }
}

/// Slot 22: a task command. A task made at event `s` of a history has the id
/// `t<s>`, so the target ids range over the positions a history reaches.
fn task_command_case(n: u64, flag: bool) -> TaskCommand {
    let task_id: TaskId = format!("t{}", 20 + n).parse().unwrap();
    let by = TaskActor {
        who: if flag {
            Address::Owner
        } else {
            Address::Agent(id())
        },
        manager_of: None,
    };
    match n % 10 {
        0 | 1 => TaskCommand::Create(TaskCreated {
            project_id: None,
            title: "Work".into(),
            kind: TaskKind::Work,
            creator: by.who,
            parent: None,
            judges: None,
            measure: TaskMeasure::Owner,
            budget: None,
            files: vec![],
            rests_on: None,
        }),
        2 => TaskCommand::Assign {
            task_id,
            agent_id: id(),
            by,
        },
        3 => TaskCommand::SetState {
            task_id,
            state: TaskState::Working,
            reason: None,
            by,
        },
        4 => TaskCommand::SetState {
            task_id,
            state: TaskState::Blocked,
            reason: Some("late".into()),
            by,
        },
        5 => TaskCommand::Submit {
            task_id,
            what: TaskSubmission::Result,
            files: vec![],
            by,
        },
        6 => TaskCommand::Verdict {
            task_id,
            verdict: TaskVerdict::Accepted,
            review_task: None,
            note: None,
            by,
        },
        7 => TaskCommand::Attach {
            task_id,
            file: TaskFile {
                path: "run.log".into(),
                kind: TaskFileKind::Evidence,
                note: None,
            },
            by,
        },
        8 => TaskCommand::Reminded {
            task_id,
            agent_id: id(),
        },
        _ => TaskCommand::SetState {
            task_id,
            state: TaskState::Cancelled,
            reason: None,
            by,
        },
    }
}

#[test]
fn case_generators_cover_every_event_and_command_variant() {
    let events: Vec<_> = (0..43)
        .map(|slot| event_slot(&event_case(slot, "text".into(), 1, 1, true)))
        .collect();
    assert_eq!(events, (0..43).collect::<Vec<_>>());
    let commands: Vec<_> = (0..27)
        .map(|slot| command_slot(&command_case(slot, 1, true)))
        .collect();
    assert_eq!(commands, (0..27).collect::<Vec<_>>());
    let picks: Vec<_> = (0..6)
        .map(|pick| match command_case(21, pick, true) {
            OrgCommand::Decision(command) => decision_support::decision_command_pick(&command),
            other => panic!("not a decision command: {other:?}"),
        })
        .collect();
    assert_eq!(picks, (0..6).collect::<Vec<_>>());
    let history = history::History::seeded();
    let wide_commands: Vec<_> = (0..26)
        .map(|slot| {
            let seed = history::Seed {
                slot,
                agent: slot % 3,
                pane: slot % 4,
                from: slot % 6,
                to: slot % 6,
                value: slot as u32,
                flag: true,
            };
            command_slot(&seed.command(&history.state))
        })
        .collect();
    assert_eq!(wide_commands, (0..26).collect::<Vec<_>>());
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
    commands.extend(editor_ready_cycle(id(), "s2", "s1"));
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

#[test]
fn editor_ready_replay_retains_unbound_readiness_and_exclusion() {
    let mut reachable = OrgState::default();
    let mut log = vec![];
    let mut commands = vec![
        command_case(0, 1, false),
        command_case(1, 1, false),
        command_case(6, 1, false),
        command_case(7, 1, false),
    ];
    commands.extend(editor_ready_cycle(id(), "s1", "s2").into_iter().take(3));
    for command in commands {
        for event in reachable.decide(&command, time()).unwrap() {
            reachable
                .apply(log.len() as u64 + 1, time(), &event)
                .unwrap();
            log.push((event.kind().to_owned(), event.to_body()));
        }
    }
    assert_eq!(log.len(), 6); // obsolete session report emitted no fact
    assert_eq!(log.last().unwrap().0, "agent.editor_ready");
    let agent = reachable.agent(&id()).unwrap();
    assert_eq!(agent.status, AgentStatus::Ready);
    assert_eq!(agent.session_id, None);
    assert_eq!(agent.excluded_session_id, Some("s1".into()));
    for cut in 0..=log.len() {
        let mut replayed = OrgState::default();
        let mut stats = ReplayStats::default();
        fold(&log[..cut], &mut replayed, &mut stats, 0);
        fold(&log[cut..], &mut replayed, &mut stats, cut);
        assert_eq!(replayed, reachable);
        assert_eq!(stats.applied, 6);
        assert!(replayed
            .decide(&command_case(7, 1, false), time())
            .unwrap()
            .is_empty());
        let next = replayed.decide(&command_case(7, 2, false), time()).unwrap();
        assert_eq!(
            next,
            vec![OrgEvent::AgentSessionBound {
                agent_id: id(),
                session_id: "s2".into()
            }]
        );
        replayed.apply(7, time(), &next[0]).unwrap();
        assert_eq!(replayed.agent(&id()).unwrap().session_id, Some("s2".into()));
        assert_eq!(replayed.agent(&id()).unwrap().excluded_session_id, None);
    }
}

// Count only the generated suffix, not the mandatory prefix. Separate advancing
// steps from probes/retries so the coverage report cannot hide rare transitions.
#[derive(Default)]
struct ExcludedReach(RefCell<BTreeMap<(&'static str, &'static str), usize>>);
impl ExcludedReach {
    fn record(
        &self,
        phase: &'static str,
        state: &OrgState,
        command: &OrgCommand,
        result: &Result<Vec<OrgEvent>, Refusal>,
    ) {
        if let OrgCommand::SessionBound {
            agent_id,
            session_id,
        } = command
        {
            if let Some(agent) = state.agent(agent_id).filter(|agent| {
                agent.pane.is_some()
                    && agent.status != AgentStatus::Down
                    && agent.excluded_session_id.as_ref() == Some(session_id)
            }) {
                assert!(result.as_ref().unwrap().is_empty());
                let status = match agent.status {
                    AgentStatus::Starting => "starting",
                    AgentStatus::Clearing => "clearing",
                    AgentStatus::Ready => "ready",
                    AgentStatus::Stuck => "stuck",
                    AgentStatus::Down => unreachable!(), // filtered above, test-only
                };
                *self.0.borrow_mut().entry((phase, status)).or_default() += 1;
            }
        }
    }
    fn assert_coverage(&self, name: &str) {
        let counts = self.0.borrow();
        println!("{name} excluded-id reach (256 cases): {counts:?}");
        for status in ["starting", "clearing", "ready", "stuck"] {
            assert!(
                counts
                    .iter()
                    .any(|((_, observed), count)| *observed == status && *count > 0),
                "{name}: no excluded-id report in {status}"
            );
        }
    }
}

proptest! {
    #[test]
    fn event_parts_roundtrip(slot in 0usize..43, text in any::<String>(), n in any::<u64>(), cost in any::<i64>(), flag in any::<bool>()) {
        let event = event_case(slot, text, n, cost, flag);
        prop_assert!(event.to_body().is_object());
        prop_assert_eq!(OrgEvent::from_parts(event.kind(), &event.to_body()).unwrap(), Some(event));
    }

}

#[test]
fn applying_a_log_is_deterministic_and_prefix_composes() {
    let reach = ExcludedReach::default();
    let mut runner = TestRunner::new(Config {
        cases: 256,
        rng_seed: RngSeed::Fixed(0x8b02),
        failure_persistence: None, // the fixed seed reproduces the complete coverage run
        ..Config::default()
    });
    let inputs = (
        prop::collection::vec((0usize..27, 0u64..50, any::<bool>()), 0..80),
        any::<usize>(),
    );
    runner
        .run(&inputs, |(actions, split)| {
            let mut events = seeded_log();
            let mut reachable = OrgState::default();
            for (index, event) in events.iter().enumerate() {
                reachable.apply(index as u64 + 1, time(), event).unwrap();
                assert_agent_invariants(&reachable);
            }
            for (slot, n, flag) in actions {
                let command = command_case(slot, n, flag);
                let before = reachable.clone();
                let result = reachable.decide(&command, time());
                prop_assert_eq!(&reachable, &before);
                reach.record("step", &before, &command, &result);
                if let Ok(next) = result {
                    assert_decided_bindings(&before, &next);
                    for event in next {
                        prop_assert!(reachable
                            .apply(events.len() as u64 + 1, time(), &event)
                            .is_ok());
                        assert_agent_invariants(&reachable);
                        assert_editor_ready_preserves(&before, &reachable, &event);
                        events.push(event);
                    }
                }
                // Every command is tried at every generated reachable state, not
                // only the randomly selected command that advanced the history.
                for candidate in 0..27 {
                    let mut probe = reachable.clone();
                    let command = command_case(candidate, n, flag);
                    let result = probe.decide(&command, time());
                    prop_assert_eq!(&probe, &reachable);
                    reach.record("probe", &probe, &command, &result);
                    if let Ok(next) = result {
                        assert_decided_bindings(&probe, &next);
                        for (index, event) in next.iter().enumerate() {
                            prop_assert!(probe
                                .apply((events.len() + index + 1) as u64, time(), event)
                                .is_ok());
                            assert_agent_invariants(&probe);
                            assert_editor_ready_preserves(&reachable, &probe, event);
                        }
                    }
                }
            }
            let mut log: Vec<_> = events
                .iter()
                .map(|e| (e.kind().to_owned(), e.to_body()))
                .collect();
            // Unknown events inserted only at the end preserve the literal message
            // IDs in the generated history; seq gaps are valid, not renumbered IDs.
            log.extend([
                ("future.kind".into(), json!(null)),
                ("future.kind".into(), json!([1, 2])),
            ]);
            let mut whole = OrgState::default();
            let mut whole_stats = ReplayStats::default();
            fold(&log, &mut whole, &mut whole_stats, 0);
            prop_assert_eq!(&whole, &reachable);
            let mut repeat = OrgState::default();
            let mut repeat_stats = ReplayStats::default();
            fold(&log, &mut repeat, &mut repeat_stats, 0);
            prop_assert_eq!(&whole, &repeat);
            prop_assert_eq!(&whole_stats, &repeat_stats);
            let split = split % (log.len() + 1);
            let mut partial = OrgState::default();
            let mut partial_stats = ReplayStats::default();
            fold(&log[..split], &mut partial, &mut partial_stats, 0);
            fold(&log[split..], &mut partial, &mut partial_stats, split);
            prop_assert_eq!(whole, partial);
            prop_assert_eq!(whole_stats, partial_stats);
            prop_assert_eq!(repeat_stats.skipped_unknown, 2);
            Ok(())
        })
        .unwrap();
    reach.assert_coverage("single-agent");
}

#[test]
fn multi_project_history_is_serial_deterministic_and_prefix_composes() {
    let reach = ExcludedReach::default();
    let mut runner = TestRunner::new(Config {
        cases: 256,
        rng_seed: RngSeed::Fixed(0x8b03),
        failure_persistence: None, // the fixed seed reproduces the complete coverage run
        ..Config::default()
    });
    let inputs = (
        prop::collection::vec(history::arb_seed(), 0..100),
        any::<usize>(),
    );
    runner
        .run(&inputs, |(actions, split)| {
            let mut history = history::History::seeded();
            for input in actions {
                let command = input.command(&history.state);
                // step checks nonmutation, then applies every returned event before
                // the next decision. Retrying a report happens after that apply.
                let before = history.state.clone();
                let result = history.step(command.clone());
                reach.record("step", &before, &command, &result);
                if input.flag {
                    let before = history.state.clone();
                    let result = history.step(command.clone());
                    reach.record("retry", &before, &command, &result);
                }
                if input.value == 0 {
                    history.unknown();
                }
                // Probe the generator's session-ID alphabet at each randomly reached
                // state, including short-lived Starting after a session-less repair.
                // These probes do not advance or bias the random history.
                for agent in 0..3 {
                    for value in 0..4 {
                        let command = history::Seed {
                            slot: 7,
                            agent,
                            value,
                            ..input.clone()
                        }
                        .command(&history.state);
                        let mut probe = history.state.clone();
                        let result = probe.decide(&command, time());
                        prop_assert_eq!(&probe, &history.state);
                        reach.record("probe", &probe, &command, &result);
                        if let Ok(events) = result {
                            assert_decided_bindings(&probe, &events);
                            for event in events {
                                prop_assert!(probe.apply(1, time(), &event).is_ok());
                                assert_agent_invariants(&probe);
                                assert_editor_ready_preserves(&history.state, &probe, &event);
                            }
                        }
                    }
                }
            }
            history.unknown();
            prop_assert!(history
                .records
                .windows(2)
                .all(|pair| pair[0].seq < pair[1].seq && pair[0].ts < pair[1].ts));
            let mut whole = OrgState::default();
            let mut stats = ReplayStats::default();
            history::fold(&history.records, &mut whole, &mut stats);
            prop_assert_eq!(&whole, &history.state);
            prop_assert_eq!(stats.skipped_unknown, history.unknown);
            let mut repeated = OrgState::default();
            let mut repeated_stats = ReplayStats::default();
            history::fold(&history.records, &mut repeated, &mut repeated_stats);
            prop_assert_eq!(&whole, &repeated);
            prop_assert_eq!(&stats, &repeated_stats);
            for cut in [
                0,
                history.records.len(),
                split % (history.records.len() + 1),
            ] {
                let mut partial = OrgState::default();
                let mut partial_stats = ReplayStats::default();
                history::fold(&history.records[..cut], &mut partial, &mut partial_stats);
                history::fold(&history.records[cut..], &mut partial, &mut partial_stats);
                prop_assert_eq!(&whole, &partial);
                prop_assert_eq!(&stats, &partial_stats);
            }
            Ok(())
        })
        .unwrap();
    reach.assert_coverage("multi-project");
}

fn delivery_notice_body(source: &str, reason: &str) -> Value {
    json!({"from":"system", "to":"owner", "kind":"note", "body":"Delivery needs attention",
        "attachments":[], "task":null, "delivery_notice":{"source_message_id":source,"reason":reason}})
}

#[test]
fn notice_survives_replay_and_lost_reply() {
    let mut live = OrgState::default();
    let source = OrgEvent::MessageSent(content(Address::Owner));
    live.apply(1, time(), &source).unwrap();
    let OrgEvent::MessageSent(note) =
        OrgEvent::from_parts("msg.sent", &delivery_notice_body("m1", "focus"))
            .unwrap()
            .unwrap()
    else {
        panic!("expected a message");
    };
    let events = live.decide(&OrgCommand::SendMessage(note), time()).unwrap();
    assert_eq!(events.len(), 1, "the note and key must be a single record");
    let log = [
        serde_json::to_string(&source).unwrap(),
        serde_json::to_string(&events[0]).unwrap(),
    ];
    // Crash after the single note record was appended, before applying it or
    // replying. Reconstruct only from serialized facts, not process-local keys.
    let mut recovered = OrgState::default();
    let mut stats = ReplayStats::default();
    for (seq, line) in (1..).zip(&log) {
        let record: Value = serde_json::from_str(line).unwrap();
        stats
            .apply_parts(
                &mut recovered,
                seq,
                time(),
                record["kind"].as_str().unwrap(),
                &record["body"],
            )
            .unwrap();
    }
    live.apply(2, time(), &events[0]).unwrap();
    assert_eq!(recovered, live);
    assert_eq!(stats.applied, 2);
    let note = recovered.message(&MessageId::new(2)).unwrap();
    assert_eq!(note.status, DeliveryStatus::Delivered);
    assert_eq!(note.read_at, None);
    let changed = MessageContent {
        body: "Changed after the lost reply".into(),
        ..note.content.clone()
    };
    let before = recovered.clone();
    assert!(recovered
        .decide(&OrgCommand::SendMessage(changed), time())
        .unwrap()
        .is_empty());
    assert_eq!(recovered, before);
    assert_eq!(
        recovered
            .messages()
            .filter(|m| m.content.delivery_notice.is_some())
            .count(),
        1
    );
}

#[test]
fn notice_key_survives_source_and_note_changes() {
    let note = OrgEvent::from_parts("msg.sent", &delivery_notice_body("m3", "unobserved"))
        .unwrap()
        .unwrap();
    let records = [
        OrgEvent::ProjectRegistered {
            project_id: "p".parse().unwrap(),
            root: "/p".into(),
        },
        OrgEvent::AgentUpRequested {
            agent_id: id(),
            role: role(),
        },
        OrgEvent::MessageSent(content(Address::Agent(id()))),
        note,
        OrgEvent::MessageDeliveryAttempted {
            message_id: MessageId::new(3),
            attempt: 1,
            pane: "%1".into(),
        },
        OrgEvent::MessageDeliveryFailed {
            message_id: MessageId::new(3),
            reason: "retry".into(),
            final_failure: false,
        },
        OrgEvent::MessageDeliveryFailed {
            message_id: MessageId::new(3),
            reason: "give up".into(),
            final_failure: true,
        },
        OrgEvent::MessageDelivered {
            message_id: MessageId::new(3),
            session_id: "late".into(),
            confirmation: "receipt".into(),
        },
        OrgEvent::MessageRead {
            message_ids: vec![MessageId::new(3)],
        },
        OrgEvent::MessageRead {
            message_ids: vec![MessageId::new(4)],
        },
    ];
    let mut direct = OrgState::default();
    let mut recovered = OrgState::default();
    let mut stats = ReplayStats::default();
    let mut repeated_counts = Vec::new();
    for (seq, event) in (1..).zip(&records) {
        direct.apply(seq, time(), event).unwrap();
        stats
            .apply_parts(&mut recovered, seq, time(), event.kind(), &event.to_body())
            .unwrap();
        assert_eq!(recovered, direct);
        if seq >= 4 {
            let repeated = recovered
                .message(&MessageId::new(4))
                .unwrap()
                .content
                .clone();
            repeated_counts.push((
                seq,
                recovered
                    .decide(&OrgCommand::SendMessage(repeated), time())
                    .unwrap()
                    .len(),
            ));
        }
    }
    assert!(
        repeated_counts.iter().all(|(_, count)| *count == 0),
        "{repeated_counts:?}"
    );
    assert_eq!(
        recovered.message(&MessageId::new(3)).unwrap().status,
        DeliveryStatus::Delivered
    );
    assert_eq!(
        recovered.message(&MessageId::new(4)).unwrap().read_at,
        Some(time())
    );
    let OrgEvent::MessageSent(other) =
        OrgEvent::from_parts("msg.sent", &delivery_notice_body("m3", "future_reason"))
            .unwrap()
            .unwrap()
    else {
        panic!("expected a message");
    };
    assert_eq!(
        recovered
            .decide(&OrgCommand::SendMessage(other), time())
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn legacy_unkeyed_notice_does_not_claim_key() {
    let source = json!({"from":"owner","to":"owner","kind":"brief","body":"Work","attachments":[],"task":null});
    let legacy_note = json!({"from":"system","to":"owner","kind":"note","body":"Delivery needs attention","attachments":[],"task":null});
    let mut state = OrgState::default();
    let mut stats = ReplayStats::default();
    for (seq, body) in [(1, &source), (2, &legacy_note)] {
        stats
            .apply_parts(&mut state, seq, time(), "msg.sent", body)
            .unwrap();
    }
    let mut direct = OrgState::default();
    for (seq, body) in [(1, &source), (2, &legacy_note)] {
        direct
            .apply(
                seq,
                time(),
                &OrgEvent::from_parts("msg.sent", body).unwrap().unwrap(),
            )
            .unwrap();
    }
    assert_eq!(state, direct);
    assert!(state
        .messages()
        .all(|m| m.content.delivery_notice.is_none()));
    let OrgEvent::MessageSent(keyed) =
        OrgEvent::from_parts("msg.sent", &delivery_notice_body("m1", "focus"))
            .unwrap()
            .unwrap()
    else {
        panic!("expected a message");
    };
    let command = OrgCommand::SendMessage(keyed);
    let events = state.decide(&command, time()).unwrap();
    assert_eq!(
        events.len(),
        1,
        "an old unkeyed note cannot reserve a new key"
    );
    direct.apply(3, time(), &events[0]).unwrap();
    stats
        .apply_parts(
            &mut state,
            3,
            time(),
            events[0].kind(),
            &events[0].to_body(),
        )
        .unwrap();
    assert_eq!(state, direct);
    assert_eq!(state.inbox(&Address::Owner).len(), 3);
    assert!(state.decide(&command, time()).unwrap().is_empty());
    assert_eq!(stats.applied, 3);
}

#[test]
fn malformed_notice_replay_is_atomic() {
    let mut state = OrgState::default();
    let mut stats = ReplayStats::default();
    stats.apply_parts(&mut state, 1, time(), "msg.sent", &json!({"from":"owner","to":"owner","kind":"note","body":"Work","attachments":[],"task":null})).unwrap();
    let valid = delivery_notice_body("m1", "focus");
    stats
        .apply_parts(&mut state, 2, time(), "msg.sent", &valid)
        .unwrap();
    let before = state.clone();
    let before_stats = stats.clone();
    let mut malformed = valid.clone();
    malformed["delivery_notice"] = json!({"source_message_id":"m1"});
    let outcomes: Vec<_> = [
        valid,
        delivery_notice_body("m999", "focus"),
        delivery_notice_body("m1", ""),
        malformed,
    ]
    .into_iter()
    .map(|body| {
        let mut candidate = state.clone();
        let mut candidate_stats = stats.clone();
        let refused = candidate_stats
            .apply_parts(&mut candidate, 3, time(), "msg.sent", &body)
            .is_err();
        (
            body,
            refused,
            candidate == before,
            candidate_stats == before_stats,
        )
    })
    .collect();
    assert!(
        outcomes
            .iter()
            .all(|(_, refused, state_same, stats_same)| *refused && *state_same && *stats_same),
        "{outcomes:?}"
    );
}

proptest! {
    #[test]
    fn arbitrary_config_and_event_text_never_panics(text in any::<String>()) {
        let _ = parse_project(&text);
        let _ = parse_role("lead", &text);
        let _ = text.parse::<AgentId>();
        for (kind, _) in bodies() { let _ = OrgEvent::from_parts(kind, &json!({"agent_id":text})); }
        prop_assert_eq!(OrgEvent::from_parts("future", &json!(text)).unwrap(), None);
    }
}
