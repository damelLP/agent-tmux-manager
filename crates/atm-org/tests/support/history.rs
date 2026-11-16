//! Synthetic multi-project histories: no harness, clock, daemon or tmux.
use super::decision_support;
use super::support::{
    assert_agent_invariants, assert_decided_bindings, assert_editor_ready_preserves, content,
    editor_ready_cycle, role, summary, time,
};
use atm_core::Money;
use atm_org::*;
use chrono::{DateTime, Duration, Utc};
use proptest::prelude::*;
use serde_json::{json, Value};

pub fn agents() -> [AgentId; 3] {
    ["p/lead", "p/worker", "q/lead"].map(|text| text.parse().unwrap())
}
fn agent(index: usize) -> AgentId {
    agents()[index % 3].clone()
}
fn spec(id: &AgentId) -> RoleSpec {
    let mut value = role();
    value.name.clone_from(&id.name);
    value
}
fn address(index: usize) -> Address {
    match index % 6 {
        0 => Address::Owner,
        1 => Address::System,
        2..=4 => Address::Agent(agent(index - 2)),
        _ => Address::Agent("p/missing".parse().unwrap()),
    }
}

#[derive(Debug, Clone)]
pub struct Seed {
    pub slot: usize,
    pub agent: usize,
    pub pane: usize,
    pub from: usize,
    pub to: usize,
    pub value: u32,
    pub flag: bool,
}
pub fn arb_seed() -> impl Strategy<Value = Seed> {
    (
        0usize..26,
        0usize..3,
        0usize..4,
        0usize..6,
        0usize..6,
        0u32..32,
        any::<bool>(),
    )
        .prop_map(|(slot, agent, pane, from, to, value, flag)| Seed {
            slot,
            agent,
            pane,
            from,
            to,
            value,
            flag,
        })
}
impl Seed {
    pub fn command(&self, state: &OrgState) -> OrgCommand {
        let agent_id = agent(self.agent);
        let mut pane = format!("%{}", self.pane);
        let session_id = format!("session-{}-{}", self.agent, self.value % 4).into();
        let messages: Vec<_> = state.messages().collect();
        let selected = if self.value == 0 {
            None
        } else {
            messages
                .get(self.value as usize % messages.len().max(1))
                .copied()
        };
        let message_id = selected.map(|m| m.id).unwrap_or(MessageId::new(999_999));
        let attempt = if self.flag {
            selected
                .and_then(|m| m.last_attempt.as_ref())
                .map(|a| a.attempt.saturating_add(1))
                .unwrap_or(1)
        } else {
            self.value
        };
        if self.flag {
            if let Some(recorded) = state.agent(&agent_id).and_then(|a| a.pane.as_ref()) {
                pane.clone_from(recorded);
            }
        }
        let audit = ControlAudit {
            pane: pane.clone(),
            action: json!({"value":self.value}),
        };
        match self.slot {
            0 => OrgCommand::RegisterProject {
                project_id: agent_id.project,
                root: format!("/root/{}", self.value % 2),
                move_root: self.flag,
            },
            1 => OrgCommand::Up {
                role: spec(&agent_id),
                agent_id,
            },
            2 => OrgCommand::Adopt {
                agent_id,
                pane,
                session_id: self.flag.then_some(session_id),
            },
            3 => OrgCommand::Clear { agent_id },
            4 => OrgCommand::Down {
                agent_id,
                reason: "stop".into(),
            },
            5 => OrgCommand::Respawn {
                role: spec(&agent_id),
                agent_id,
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
                reason: format!("timeout-{}", self.value % 2),
            },
            10 => {
                let mut message = content(address(self.to));
                message.from = address(self.from);
                message.kind = match self.value % 6 {
                    0 => MessageKind::Note,
                    1 => MessageKind::Brief,
                    2 => MessageKind::Report,
                    3 => MessageKind::Question,
                    4 => MessageKind::Answer,
                    _ => MessageKind::Review,
                };
                OrgCommand::SendMessage(message)
            }
            11 => OrgCommand::DeliveryAttempted {
                message_id,
                attempt,
                pane,
            },
            12 => OrgCommand::Delivered {
                message_id,
                session_id,
                confirmation: "late token".into(),
            },
            13 => OrgCommand::DeliveryFailed {
                message_id,
                reason: "timeout".into(),
                final_failure: self.flag,
            },
            14 => OrgCommand::ReadMessages {
                who: if self.flag {
                    selected
                        .map(|m| m.content.to.clone())
                        .unwrap_or_else(|| address(self.to))
                } else {
                    address(self.to)
                },
                message_ids: vec![message_id],
            },
            15 => {
                let mut snapshot = summary();
                snapshot.session_id = session_id;
                snapshot.agent_id = self.flag.then_some(agent_id);
                snapshot.pane = Some(pane);
                snapshot.cost = Some(Money::from_microdollars(i64::from(self.value)));
                snapshot.started = time() + Duration::seconds(i64::from(self.value));
                snapshot.ended = self.flag.then_some(time());
                OrgCommand::RecordSummary(snapshot)
            }
            16 => OrgCommand::ControlSent(audit),
            17 => OrgCommand::ControlSpawned(audit),
            18 => OrgCommand::ControlKilled(audit),
            19 => OrgCommand::EditorReady { agent_id },
            20 => OrgCommand::Revalidate { agent_id },
            21 => {
                let decisions: Vec<_> = state.decisions().collect();
                let target = decisions
                    .get(self.value as usize % decisions.len().max(1))
                    .map(|decision| decision.id)
                    .unwrap_or(DecisionId::new(999_999));
                // Some deadlines fall behind the history's clock (refused at the
                // request), some are reached later (expiry), some never.
                let deadline = time() + Duration::milliseconds(600 + i64::from(self.value) * 10);
                OrgCommand::Decision(decision_support::decision_command(
                    self.to as u64,
                    target,
                    address(self.from),
                    self.flag.then_some(deadline),
                    self.pane.is_multiple_of(2),
                ))
            }
            22 => {
                // A task this history made, or one that does not exist.
                let tasks: Vec<_> = state.tasks().collect();
                let task_id = tasks
                    .get(self.value as usize % tasks.len().max(1))
                    .map_or_else(|| "t999999".parse().unwrap(), |task| task.id.clone());
                let by = TaskActor {
                    who: address(self.from),
                    lead: self.flag,
                };
                OrgCommand::Task(match self.pane {
                    0 => TaskCommand::Create(task_body(by.who, TaskKind::Work, None)),
                    1 => TaskCommand::Assign {
                        task_id,
                        agent_id,
                        by,
                    },
                    2 => TaskCommand::Submit {
                        task_id,
                        what: TaskSubmission::Result,
                        files: vec![],
                        by: TaskActor {
                            who: Address::Agent(agent_id),
                            lead: false,
                        },
                    },
                    _ => TaskCommand::Verdict {
                        task_id,
                        verdict: if self.value.is_multiple_of(2) {
                            TaskVerdict::Accepted
                        } else {
                            TaskVerdict::ChangesRequested
                        },
                        review_task: None,
                        note: None,
                        by,
                    },
                })
            }
            23 => OrgCommand::SaveRequested {
                agent_id,
                message_id,
            },
            24 => OrgCommand::Saved {
                agent_id,
                message_id,
            },
            _ => {
                let decisions: Vec<_> = state.decisions().collect();
                let decision_id = decisions
                    .get(self.value as usize % decisions.len().max(1))
                    .map(|decision| decision.id)
                    .unwrap_or(DecisionId::new(999_999));
                OrgCommand::Owner(match self.to % 3 {
                    0 => OwnerCommand::Away {
                        by: address(self.from),
                        stacking_caps: [(agent_id.project, self.value % 3)].into(),
                    },
                    1 => OwnerCommand::Back {
                        by: address(self.from),
                    },
                    _ => OwnerCommand::ApplyAway { decision_id },
                })
            }
        }
    }
}

fn task_body(creator: Address, kind: TaskKind, judges: Option<TaskId>) -> TaskCreated {
    TaskCreated {
        title: "Work".into(),
        kind,
        creator,
        parent: None,
        judges,
        measure: TaskMeasure::Owner,
        budget: None,
        files: vec![],
        rests_on: None,
    }
}

#[derive(Debug, Clone)]
pub struct Record {
    pub seq: u64,
    pub ts: DateTime<Utc>,
    pub kind: String,
    pub body: Value,
}
#[derive(Debug, Default)]
pub struct History {
    pub state: OrgState,
    pub records: Vec<Record>,
    pub kinds: std::collections::BTreeSet<String>,
    pub unknown: u64,
    seq: u64,
}
impl History {
    /// Deliberately serialize every decision and application, never deciding two
    /// potentially conflicting commands from the same snapshot.
    pub fn step(&mut self, command: OrgCommand) -> Result<Vec<OrgEvent>, Refusal> {
        assert_agent_invariants(&self.state);
        let before = self.state.clone();
        let result = self
            .state
            .decide(&command, time() + Duration::milliseconds(self.seq as i64));
        assert_eq!(self.state, before, "decide mutated state for {command:?}");
        let events = result?;
        assert_decided_bindings(&self.state, &events);
        for event in &events {
            self.seq += 3; // sparse sequence numbers remain the actual message IDs
            let ts = time() + Duration::milliseconds(self.seq as i64);
            let result = self.state.apply(self.seq, ts, event);
            assert!(
                result.is_ok(),
                "decide returned an inapplicable event: {command:?}, {event:?}: {result:?}"
            );
            assert_agent_invariants(&self.state);
            assert_editor_ready_preserves(&before, &self.state, event);
            self.kinds.insert(event.kind().into());
            self.records.push(Record {
                seq: self.seq,
                ts,
                kind: event.kind().into(),
                body: event.to_body(),
            });
        }
        Ok(events)
    }
    pub fn unknown(&mut self) {
        self.seq += 3;
        self.unknown += 1;
        self.records.push(Record {
            seq: self.seq,
            ts: time() + Duration::milliseconds(self.seq as i64),
            kind: "future.kind".into(),
            body: json!([null, "opaque"]),
        });
    }
    fn send(&mut self, from: Address, to: Address) -> MessageId {
        let mut message = content(to);
        message.from = from;
        assert_eq!(
            self.step(OrgCommand::SendMessage(message)).unwrap().len(),
            1
        );
        MessageId::new(self.seq)
    }

    fn task(&mut self, command: TaskCommand) -> TaskId {
        assert_eq!(self.step(OrgCommand::Task(command)).unwrap().len(), 1);
        format!("t{}", self.seq).parse().unwrap()
    }

    /// One task from its creation to an accepted result with a cost record,
    /// through a reminder and a review: every task event kind.
    fn seed_tasks(&mut self) {
        let lead = TaskActor {
            who: Address::Agent(agent(0)),
            lead: false,
        };
        let worker = TaskActor {
            who: Address::Agent(agent(1)),
            lead: false,
        };
        let work = self.task(TaskCommand::Create(task_body(
            lead.who.clone(),
            TaskKind::Work,
            None,
        )));
        let review = self.task(TaskCommand::Create(task_body(
            lead.who.clone(),
            TaskKind::Review,
            Some(work.clone()),
        )));
        for command in [
            TaskCommand::Assign {
                task_id: work.clone(),
                agent_id: agent(1),
                by: lead.clone(),
            },
            TaskCommand::SetState {
                task_id: work.clone(),
                state: TaskState::Working,
                reason: None,
                by: worker.clone(),
            },
            TaskCommand::Reminded {
                task_id: work.clone(),
                agent_id: agent(1),
            },
            TaskCommand::Flagged {
                task_id: work.clone(),
                to: lead.who.clone(),
            },
            TaskCommand::Attach {
                task_id: work.clone(),
                file: TaskFile {
                    path: "run.log".into(),
                    kind: TaskFileKind::Evidence,
                    note: None,
                },
                by: worker.clone(),
            },
            TaskCommand::Submit {
                task_id: work.clone(),
                what: TaskSubmission::Result,
                files: vec![],
                by: worker,
            },
            TaskCommand::StartReview {
                task_id: work.clone(),
                review_task: review,
                by: lead,
            },
            TaskCommand::Verdict {
                task_id: work.clone(),
                verdict: TaskVerdict::Accepted,
                review_task: None,
                note: None,
                by: TaskActor {
                    who: Address::Owner,
                    lead: false,
                },
            },
            TaskCommand::RecordCost {
                task_id: work,
                record: TaskCost::default(),
            },
        ] {
            assert_eq!(self.step(OrgCommand::Task(command)).unwrap().len(), 1);
        }
    }

    /// Mandatory prefix covers every event kind, all senders, cross-project
    /// routing, both pane conflicts, duplicates, retries and late confirmations.
    pub fn seeded() -> Self {
        let mut history = Self::default();
        for project in ["p", "q"] {
            history
                .step(OrgCommand::RegisterProject {
                    project_id: project.parse().unwrap(),
                    root: format!("/{project}"),
                    move_root: false,
                })
                .unwrap();
        }
        history.unknown();
        for agent_id in agents() {
            history
                .step(OrgCommand::Up {
                    role: spec(&agent_id),
                    agent_id,
                })
                .unwrap();
        }
        let lead = agent(0);
        let worker = agent(1);
        let other = agent(2);
        let adopted = OrgCommand::Adopt {
            agent_id: lead.clone(),
            pane: "%1".into(),
            session_id: Some("lead-1".into()),
        };
        history.step(adopted.clone()).unwrap();
        assert!(history.step(adopted).unwrap().is_empty());
        let bound = history.state.agent(&lead).unwrap().clone();
        history
            .step(OrgCommand::Revalidate {
                agent_id: lead.clone(),
            })
            .unwrap();
        let starting = history.state.agent(&lead).unwrap();
        assert_eq!(starting.status, AgentStatus::Starting);
        assert_eq!(starting.session_id, bound.session_id);
        assert_eq!(starting.pane, bound.pane);
        assert_eq!(starting.excluded_session_id, bound.excluded_session_id);
        history
            .step(OrgCommand::EditorReady {
                agent_id: lead.clone(),
            })
            .unwrap();
        history
            .step(OrgCommand::PaneCreated {
                agent_id: worker.clone(),
                pane: "%2".into(),
            })
            .unwrap();
        for command in [
            OrgCommand::Adopt {
                agent_id: other.clone(),
                pane: "%2".into(),
                session_id: None,
            },
            OrgCommand::PaneCreated {
                agent_id: other.clone(),
                pane: "%1".into(),
            },
        ] {
            assert!(matches!(
                history.step(command),
                Err(Refusal::Invariant(ApplyError::PaneOccupied { .. }))
            ));
        }
        assert!(matches!(
            history.step(OrgCommand::PaneCreated {
                agent_id: worker.clone(),
                pane: "%3".into()
            }),
            Err(Refusal::AgentAlreadyBound { .. })
        ));
        history
            .step(OrgCommand::SessionBound {
                agent_id: worker.clone(),
                session_id: "worker-1".into(),
            })
            .unwrap();
        history
            .step(OrgCommand::Adopt {
                agent_id: other.clone(),
                pane: "%3".into(),
                session_id: None,
            })
            .unwrap();
        history
            .step(OrgCommand::SessionBound {
                agent_id: other.clone(),
                session_id: "other-1".into(),
            })
            .unwrap();
        history
            .step(OrgCommand::Stuck {
                agent_id: lead.clone(),
                reason: "late".into(),
            })
            .unwrap();
        history
            .step(OrgCommand::SessionLost {
                agent_id: lead.clone(),
            })
            .unwrap();
        history
            .step(OrgCommand::Stuck {
                agent_id: lead.clone(),
                reason: "still missing".into(),
            })
            .unwrap();
        assert!(history
            .step(OrgCommand::SessionLost {
                agent_id: lead.clone()
            })
            .unwrap()
            .is_empty());
        history
            .step(OrgCommand::SessionBound {
                agent_id: lead.clone(),
                session_id: "lead-2".into(),
            })
            .unwrap();
        history
            .step(OrgCommand::Clear {
                agent_id: lead.clone(),
            })
            .unwrap();
        assert!(history
            .step(OrgCommand::PaneCreated {
                agent_id: lead.clone(),
                pane: "%1".into()
            })
            .unwrap()
            .is_empty());
        history
            .step(OrgCommand::Adopt {
                agent_id: lead.clone(),
                pane: "%1".into(),
                session_id: None,
            })
            .unwrap();
        history
            .step(OrgCommand::SessionBound {
                agent_id: lead.clone(),
                session_id: "lead-3".into(),
            })
            .unwrap();
        history
            .step(OrgCommand::Down {
                agent_id: other.clone(),
                reason: "stop".into(),
            })
            .unwrap();
        history
            .step(OrgCommand::Respawn {
                agent_id: other.clone(),
                role: spec(&other),
                reason: "restart".into(),
            })
            .unwrap();
        history
            .step(OrgCommand::Adopt {
                agent_id: other.clone(),
                pane: "%3".into(),
                session_id: Some("other-2".into()),
            })
            .unwrap();

        // Cross-agent sender/recipient; retry once, fail finally, confirm late.
        let mid = history.send(Address::Agent(lead), Address::Agent(other.clone()));
        let attempt = OrgCommand::DeliveryAttempted {
            message_id: mid,
            attempt: 1,
            pane: "%3".into(),
        };
        history.step(attempt.clone()).unwrap();
        assert!(history.step(attempt).unwrap().is_empty());
        history
            .step(OrgCommand::DeliveryFailed {
                message_id: mid,
                reason: "timeout".into(),
                final_failure: false,
            })
            .unwrap();
        history
            .step(OrgCommand::DeliveryAttempted {
                message_id: mid,
                attempt: 2,
                pane: "%3".into(),
            })
            .unwrap();
        history
            .step(OrgCommand::DeliveryFailed {
                message_id: mid,
                reason: "timeout".into(),
                final_failure: true,
            })
            .unwrap();
        assert!(history
            .step(OrgCommand::DeliveryAttempted {
                message_id: mid,
                attempt: 2,
                pane: "%3".into()
            })
            .unwrap()
            .is_empty());
        let confirmation = OrgCommand::Delivered {
            message_id: mid,
            session_id: "other-2".into(),
            confirmation: "late token".into(),
        };
        history.step(confirmation.clone()).unwrap();
        assert!(history.step(confirmation).unwrap().is_empty());
        history
            .step(OrgCommand::ReadMessages {
                who: Address::Agent(other),
                message_ids: vec![mid],
            })
            .unwrap();

        // System sender; retryable failure followed by confirmation while queued.
        let mid = history.send(Address::System, Address::Agent(worker));
        history
            .step(OrgCommand::DeliveryAttempted {
                message_id: mid,
                attempt: 1,
                pane: "%2".into(),
            })
            .unwrap();
        history
            .step(OrgCommand::DeliveryFailed {
                message_id: mid,
                reason: "timeout".into(),
                final_failure: false,
            })
            .unwrap();
        history
            .step(OrgCommand::Delivered {
                message_id: mid,
                session_id: "worker-1".into(),
                confirmation: "late token".into(),
            })
            .unwrap();
        history.send(Address::Owner, Address::Agent(agent(0)));
        history.send(Address::Agent(agent(1)), Address::Owner);
        assert!(matches!(
            history.step(OrgCommand::SendMessage(content(Address::System))),
            Err(Refusal::InvalidRecipient(Address::System))
        ));
        for (index, id) in agents().iter().enumerate() {
            let mut s = summary();
            s.agent_id = Some(id.clone());
            s.session_id = format!("summary-{index}").into();
            s.started = time() + Duration::seconds(index as i64);
            s.ended = Some(time());
            history.step(OrgCommand::RecordSummary(s.clone())).unwrap();
            assert!(history
                .step(OrgCommand::RecordSummary(s))
                .unwrap()
                .is_empty());
        }
        let audit = ControlAudit {
            pane: "%1".into(),
            action: json!("audit"),
        };
        for command in [
            OrgCommand::ControlSent(audit.clone()),
            OrgCommand::ControlSpawned(audit.clone()),
            OrgCommand::ControlKilled(audit),
        ] {
            history.step(command).unwrap();
        }
        for (index, old) in ["lead-3", "worker-1", "other-2"].into_iter().enumerate() {
            for command in editor_ready_cycle(agent(index), old, &format!("session-{index}-1")) {
                history.step(command).unwrap();
            }
        }
        // Every decision kind: overruled after a provisional choice, confirmed,
        // withdrawn, and expired at its deadline (the clock is the sequence).
        let mut request = decision_support::request();
        request.requester = Address::Agent(agent(0));
        request.deadline = None;
        let resolution = |decision_id, option: &str| {
            DecisionCommand::Resolve(DecisionResolution {
                decision_id,
                option: option.into(),
                by: Address::Owner,
                note: None,
                standing_rule: false,
            })
        };
        let run = |history: &mut Self, command| {
            let events = history.step(OrgCommand::Decision(command)).unwrap();
            assert_eq!(events.len(), 1);
            DecisionId::new(history.seq)
        };
        let overruled = run(&mut history, DecisionCommand::Request(request.clone()));
        let taken = DecisionCommand::TakeProvisionally(ProvisionalChoice {
            decision_id: overruled,
            option: "a".into(),
            by: Address::System,
        });
        run(&mut history, taken);
        run(&mut history, resolution(overruled, "b"));
        let confirmed = run(&mut history, DecisionCommand::Request(request.clone()));
        run(&mut history, resolution(confirmed, "a"));
        let withdrawn = run(&mut history, DecisionCommand::Request(request.clone()));
        let withdrawal = DecisionCommand::Withdraw(DecisionWithdrawal {
            decision_id: withdrawn,
            by: Address::Agent(agent(0)),
            reason: None,
        });
        run(&mut history, withdrawal);
        request.deadline = Some(time() + Duration::milliseconds(history.seq as i64 + 1));
        let expired = run(&mut history, DecisionCommand::Request(request));
        let expiry = DecisionCommand::Expire {
            decision_id: expired,
        };
        run(&mut history, expiry);
        for (id, status) in [
            (overruled, DecisionStatus::Overruled),
            (confirmed, DecisionStatus::Resolved),
            (withdrawn, DecisionStatus::Withdrawn),
            (expired, DecisionStatus::Expired),
        ] {
            assert_eq!(history.state.decision(&id).unwrap().status, status);
        }
        history.seed_tasks();
        // The switch, on and off again: the random part starts with the owner present.
        for command in [
            OwnerCommand::Away {
                by: Address::Owner,
                stacking_caps: [(agent(0).project, 2)].into(),
            },
            OwnerCommand::Back { by: Address::Owner },
        ] {
            assert_eq!(history.step(OrgCommand::Owner(command)).unwrap().len(), 1);
        }
        assert_eq!(history.kinds.len(), 38);
        assert_eq!(history.state.agents().count(), 3);
        assert_eq!(history.state.projects().count(), 2);
        history
    }
}

pub fn fold(records: &[Record], state: &mut OrgState, stats: &mut ReplayStats) {
    for record in records {
        stats
            .apply_parts(state, record.seq, record.ts, &record.kind, &record.body)
            .unwrap();
        assert_agent_invariants(state);
    }
}
