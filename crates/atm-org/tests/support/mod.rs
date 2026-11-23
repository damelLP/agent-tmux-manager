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
        delivery_notice: None,
        from: Address::Owner,
        to,
        kind: MessageKind::Brief,
        body: "Work".into(),
        attachments: vec!["brief.md".into()],
        task: None,
        post: None,
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
// Invariants of histories produced by serialized decide/apply, not arbitrary
// historical facts. Starting may retain a binding after Revalidate; session-less
// adoption can retain an exclusion. Clearing must still be unbound.
pub fn assert_agent_invariants(state: &OrgState) {
    for agent in state.agents() {
        if agent.status == AgentStatus::Clearing {
            assert_eq!(agent.session_id, None, "{agent:?}");
        }
        if agent.excluded_session_id.is_some() {
            assert_eq!(agent.session_id, None, "{agent:?}");
            assert!(agent.pane.is_some(), "{agent:?}");
            assert!(
                matches!(
                    agent.status,
                    AgentStatus::Starting
                        | AgentStatus::Clearing
                        | AgentStatus::Ready
                        | AgentStatus::Stuck
                ),
                "{agent:?}"
            );
        }
    }
}

pub fn assert_decided_bindings(state: &OrgState, events: &[OrgEvent]) {
    for event in events {
        if let OrgEvent::AgentSessionBound {
            agent_id,
            session_id,
        } = event
        {
            assert_ne!(
                state.agent(agent_id).unwrap().excluded_session_id.as_ref(),
                Some(session_id)
            );
        }
    }
}

pub fn assert_editor_ready_preserves(before: &OrgState, after: &OrgState, event: &OrgEvent) {
    if let OrgEvent::AgentEditorReady { agent_id }
    | OrgEvent::AgentRevalidationRequested { agent_id } = event
    {
        let before = before.agent(agent_id).unwrap();
        let after = after.agent(agent_id).unwrap();
        assert_eq!(after.session_id, before.session_id);
        assert_eq!(after.pane, before.pane);
        assert_eq!(after.excluded_session_id, before.excluded_session_id);
    }
}

// Synthetic: required replay prefix includes stuck recovery both without and
// with a session, plus repeated observations of the excluded pre-clear ID.
pub fn editor_ready_cycle(agent_id: AgentId, old: &str, new: &str) -> Vec<OrgCommand> {
    vec![
        OrgCommand::Clear {
            agent_id: agent_id.clone(),
        },
        OrgCommand::SessionBound {
            agent_id: agent_id.clone(),
            session_id: old.into(),
        },
        OrgCommand::EditorReady {
            agent_id: agent_id.clone(),
        },
        OrgCommand::EditorReady {
            agent_id: agent_id.clone(),
        },
        OrgCommand::SessionLost {
            agent_id: agent_id.clone(),
        },
        OrgCommand::SessionBound {
            agent_id: agent_id.clone(),
            session_id: old.into(),
        },
        OrgCommand::Stuck {
            agent_id: agent_id.clone(),
            reason: "dialog without session".into(),
        },
        OrgCommand::EditorReady {
            agent_id: agent_id.clone(),
        },
        OrgCommand::SessionBound {
            agent_id: agent_id.clone(),
            session_id: new.into(),
        },
        OrgCommand::Stuck {
            agent_id: agent_id.clone(),
            reason: "dialog with session".into(),
        },
        OrgCommand::EditorReady { agent_id },
    ]
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
        ("agent.editor_ready", json!({"agent_id":"p/lead"})),
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
        (
            "agent.save_requested",
            json!({"agent_id":"p/lead","message_id":"m10"}),
        ),
        (
            "agent.saved",
            json!({"agent_id":"p/lead","message_id":"m10"}),
        ),
        ("owner.away", json!({"by":"owner","stacking_caps":{"p":1}})),
        ("owner.back", json!({"by":"owner"})),
        (
            "owner.digest_created",
            json!({"by":"owner","back_seq":null}),
        ),
    ]
}

/// Synthetic organisation histories; roots are labels, never read by pure tests.
pub mod decision_world {
    use super::*;
    #[derive(Clone)]
    pub struct World {
        pub state: OrgState,
        pub records: Vec<(u64, DateTime<Utc>, OrgEvent)>,
        pub now: DateTime<Utc>,
    }
    pub fn owner() -> TaskActor {
        TaskActor {
            who: Address::Owner,
            manager_of: None,
        }
    }
    impl World {
        pub fn new() -> Self {
            let mut world = Self {
                state: OrgState::default(),
                records: vec![],
                now: time(),
            };
            for project in ["alpha", "beta", "gamma"] {
                world.run(OrgCommand::RegisterProject {
                    project_id: project.parse().unwrap(),
                    root: format!("/{project}"),
                    move_root: false,
                });
            }
            for (pane, name) in ["alpha/lead", "beta/lead", "gamma/chief"]
                .into_iter()
                .enumerate()
            {
                let id: AgentId = name.parse().unwrap();
                let manager = if name == "gamma/chief" {
                    "owner"
                } else {
                    "gamma/chief"
                };
                let role = parse_role(
                    &id.name,
                    &format!("+++\nharness='pi'\nmanager='{manager}'\n+++\nWork"),
                )
                .unwrap();
                world.run(OrgCommand::Up {
                    agent_id: id.clone(),
                    role,
                });
                world.run(OrgCommand::PaneCreated {
                    agent_id: id.clone(),
                    pane: format!("%{pane}"),
                });
                world.run(OrgCommand::SessionBound {
                    agent_id: id,
                    session_id: format!("s{pane}").into(),
                });
            }
            world
        }
        pub fn event(&mut self, event: OrgEvent) -> u64 {
            let seq = self.records.len() as u64 + 1;
            self.state.apply(seq, self.now, &event).unwrap();
            self.records.push((seq, self.now, event));
            seq
        }
        pub fn step(&mut self, command: OrgCommand) -> Result<Vec<OrgEvent>, Refusal> {
            let before = self.state.clone();
            let events = self.state.decide(&command, self.now);
            assert_eq!(self.state, before);
            let events = events?;
            for event in &events {
                self.event(event.clone());
            }
            Ok(events)
        }
        pub fn run(&mut self, command: OrgCommand) -> u64 {
            assert_eq!(self.step(command).unwrap().len(), 1);
            self.records.len() as u64
        }
        pub fn request(&mut self, requester: &str, changes_intent: bool) -> DecisionId {
            DecisionId::new(self.run(OrgCommand::Decision(DecisionCommand::Request(
                DecisionRequest {
                    requester: requester.parse().unwrap(),
                    kind: DecisionKind::Plan,
                    title: "Choose storage".into(),
                    context: String::new(),
                    files: vec![],
                    options: vec!["small".into(), "large".into()],
                    recommended: Some("small".into()),
                    may_be_undone: true,
                    changes_intent,
                    deadline: None,
                },
            ))))
        }
        pub fn away(&mut self) {
            self.run(OrgCommand::Owner(OwnerCommand::Away {
                by: Address::Owner,
                stacking_caps: [
                    ("alpha".parse().unwrap(), 1),
                    ("beta".parse().unwrap(), 2),
                    ("gamma".parse().unwrap(), 3),
                ]
                .into(),
            }));
        }
        pub fn take(&mut self, id: DecisionId) {
            self.run(OrgCommand::Owner(OwnerCommand::ApplyAway {
                decision_id: id,
            }));
        }
        pub fn create(
            &mut self,
            rests_on: Option<DecisionId>,
            parent: Option<TaskId>,
            judges: Option<TaskId>,
        ) -> TaskId {
            let seq = self.run(OrgCommand::Task(TaskCommand::Create(TaskCreated {
                project_id: None,
                title: "Build records".into(),
                kind: if judges.is_some() {
                    TaskKind::Review
                } else {
                    TaskKind::Work
                },
                creator: Address::Owner,
                parent,
                judges,
                measure: TaskMeasure::Owner,
                budget: None,
                files: vec![],
                rests_on,
            })));
            format!("t{seq}").parse().unwrap()
        }
        pub fn assign(&mut self, task: &TaskId, agent: &str) -> Result<Vec<OrgEvent>, Refusal> {
            self.step(OrgCommand::Task(TaskCommand::Assign {
                task_id: task.clone(),
                agent_id: agent.parse().unwrap(),
                by: owner(),
            }))
        }
        pub fn set(&mut self, task: &TaskId, state: TaskState) {
            self.run(OrgCommand::Task(TaskCommand::SetState {
                task_id: task.clone(),
                state,
                reason: None,
                by: owner(),
            }));
        }
        pub fn answer(&mut self, id: DecisionId, option: &str) -> Result<Vec<OrgEvent>, Refusal> {
            self.step(OrgCommand::Decision(DecisionCommand::Resolve(
                DecisionResolution {
                    decision_id: id,
                    option: option.into(),
                    by: Address::Owner,
                    note: None,
                    standing_rule: false,
                },
            )))
        }
        pub fn replay(&self) -> OrgState {
            let mut state = OrgState::default();
            for (seq, at, event) in &self.records {
                let decoded = OrgEvent::from_parts(event.kind(), &event.to_body())
                    .unwrap()
                    .unwrap();
                state.apply(*seq, *at, &decoded).unwrap();
            }
            state
        }
    }
}
