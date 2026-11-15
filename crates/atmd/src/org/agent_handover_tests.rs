//! The handover on a real organisation actor and a real registry. The test
//! plays the message runtime (it records the request's delivery), the agent
//! (it writes the memory file and moves its session's status) and the clock
//! (it resolves the deadline by hand). The clear is a recorder: the real one
//! is exercised with the stand-in harness in `tests/agent_handover.rs`.
//! Every wait is on a committed event, a channel or the call's own answer.

use super::agent_handover::*;
use super::agents::{AgentError, ClearResult};
use super::OrgHandle;
use crate::eventlog::Envelope;
use crate::registry::RegistryHandle;
use crate::server::methods::{CallContext, CallerId, CallerPane, MethodError};
use atm_core::{
    AgentType, Harness, LifecycleEvent, Model, NeedsInputReason, SessionDomain, SessionId,
    SessionStatus, Tool,
};
use atm_org::idle::HandoverLimits;
use atm_org::{
    Address, AgentId, MessageContent, MessageId, MessageKind, OrgCommand, OrgEvent, OrgState,
};
use atm_protocol::error_code;
use chrono::Utc;
use std::{
    future::Future,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

/// A hang guard only; nothing below depends on how long a step takes.
const GUARD: Duration = Duration::from_secs(30);

struct Recorder {
    called: mpsc::UnboundedSender<AgentId>,
    refuse: Mutex<Option<String>>,
}

impl Clearer for Recorder {
    fn available(&self) -> std::pin::Pin<Box<dyn Future<Output = Result<(), AgentError>> + Send>> {
        Box::pin(async { Ok(()) })
    }
    fn clear(
        &self,
        agent: AgentId,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<ClearResult, AgentError>> + Send>> {
        let _ = self.called.send(agent.clone());
        let refuse = self.refuse.lock().unwrap().clone();
        Box::pin(async move {
            match refuse {
                Some(reason) => Err(AgentError::Refused(reason)),
                None => Ok(ClearResult {
                    agent_id: agent,
                    session_id: Some(SessionId::new("after-clear")),
                }),
            }
        })
    }
}

fn id(name: &str) -> AgentId {
    format!("p/{name}").parse().unwrap()
}

fn owner() -> CallContext {
    CallContext {
        client_id: CallerId("test".into()),
        from_pane: None,
    }
}

fn from_pane(pane: &str) -> CallContext {
    CallContext {
        client_id: CallerId("test".into()),
        from_pane: Some(CallerPane(pane.into())),
    }
}

type Call = tokio::task::JoinHandle<Result<HandoverResult, MethodError>>;

struct Rig {
    _dir: tempfile::TempDir,
    root: PathBuf,
    org: OrgHandle,
    registry: RegistryHandle,
    cancel: CancellationToken,
    recorder: Arc<Recorder>,
    cleared: mpsc::UnboundedReceiver<AgentId>,
    service: Handovers,
    committed: broadcast::Receiver<Arc<Envelope>>,
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl Rig {
    /// Project `p` with two ready agents, each idle in its session:
    /// `lead` (no manager, `%0`, `lead-1`) and `worker` (manager `lead`,
    /// `%1`, `s1`).
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        std::fs::create_dir_all(root.join(".atm/memory")).unwrap();
        let cancel = CancellationToken::new();
        let org = OrgHandle::open(dir.path().join("state"), cancel.clone()).await;
        let registry = crate::registry::spawn_registry();
        let (called, cleared) = mpsc::unbounded_channel();
        let recorder = Arc::new(Recorder {
            called,
            refuse: Mutex::new(None),
        });
        let service = Handovers::new(org.clone(), registry.clone(), recorder.clone());
        let committed = org.committed.subscribe();
        let mut rig = Self {
            _dir: dir,
            root,
            org,
            registry,
            cancel,
            recorder,
            cleared,
            service,
            committed,
        };
        rig.run(OrgCommand::RegisterProject {
            project_id: "p".parse().unwrap(),
            root: rig.root.to_str().unwrap().into(),
            move_root: false,
        })
        .await;
        rig.agent("lead", "+++\nharness='pi'\n+++\nLead", "%0", Some("lead-1"))
            .await;
        rig.agent(
            "worker",
            "+++\nharness='pi'\nmanager='lead'\n+++\nWork",
            "%1",
            Some("s1"),
        )
        .await;
        // What a test sees starts after this setup.
        rig.kinds();
        rig
    }

    async fn run(&self, command: OrgCommand) -> Vec<Envelope> {
        self.org.execute(command, Utc::now()).await.unwrap()
    }

    /// A started agent on `pane`; ready and idle in `session` when one is given.
    async fn agent(&self, name: &str, role: &str, pane: &str, session: Option<&str>) {
        let agent_id = id(name);
        self.run(OrgCommand::Up {
            agent_id: agent_id.clone(),
            role: atm_org::parse_role(name, role).unwrap(),
        })
        .await;
        self.run(OrgCommand::PaneCreated {
            agent_id: agent_id.clone(),
            pane: pane.into(),
        })
        .await;
        if let Some(session) = session {
            self.bind(name, session, pane).await;
        }
    }

    /// Register an idle row for `session` and bind the agent to it.
    async fn bind(&self, name: &str, session: &str, pane: &str) {
        let mut row = SessionDomain::new(
            SessionId::new(session),
            AgentType::GeneralPurpose,
            Model::Unknown,
        );
        row.harness = Harness::Pi;
        row.tmux_pane = Some(pane.into());
        self.registry.register(row).await.unwrap();
        self.run(OrgCommand::SessionBound {
            agent_id: id(name),
            session_id: session.into(),
        })
        .await;
    }

    fn memory(&self, name: &str) -> PathBuf {
        self.root.join(format!(".atm/memory/{name}.md"))
    }

    /// One lifecycle event of `session`, as its harness would report it.
    async fn report(&self, session: &str, event: LifecycleEvent) {
        self.registry
            .apply_lifecycle_event(SessionId::new(session), event, Harness::Pi, None, None)
            .await
            .unwrap();
    }

    async fn share(&self, session: &str, percent_tenths: u64) {
        self.report(
            session,
            LifecycleEvent::ContextUpdate {
                tokens: None,
                current_tokens: Some(percent_tenths * 1_000),
                context_window_size: Some(1_000_000),
                cost_usd: None,
            },
        )
        .await;
    }

    /// A whole turn of `session`: it starts working, then goes idle.
    async fn turn(&self, session: &str) {
        self.report(session, LifecycleEvent::WorkingStart).await;
        self.report(session, LifecycleEvent::Idle).await;
    }

    /// The next committed event of `kind`; other kinds are skipped.
    async fn next(&mut self, kind: &str) -> Arc<Envelope> {
        tokio::time::timeout(GUARD, async {
            loop {
                let event = self.committed.recv().await.unwrap();
                if event.kind == kind {
                    return event;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("no {kind} was committed"))
    }

    /// The kinds committed since the last look, in order.
    fn kinds(&mut self) -> Vec<String> {
        std::iter::from_fn(|| self.committed.try_recv().ok())
            .map(|event| event.kind.clone())
            .collect()
    }

    /// Start a handover; the returned sender resolves its deadline.
    fn start(&self, name: &str, caller: CallContext) -> (oneshot::Sender<()>, Call) {
        let (fire, fired) = oneshot::channel();
        let service = self.service.clone();
        let p = HandoverParams {
            agent: id(name),
            timeout_ms: 600_000,
        };
        let deadline = Box::pin(async move {
            if fired.await.is_err() {
                std::future::pending::<()>().await;
            }
        });
        let call = tokio::spawn(async move { service.request_until(p, caller, deadline).await });
        (fire, call)
    }

    /// The save request a handover has just recorded.
    async fn requested(&mut self) -> MessageId {
        let event = self.next("agent.save_requested").await;
        serde_json::from_value(event.body["message_id"].clone()).unwrap()
    }

    /// The message runtime's part: the request is typed and the harness
    /// confirms the prompt, which also starts the turn.
    async fn deliver(&self, message_id: MessageId, session: &str, pane: &str) {
        self.run(OrgCommand::DeliveryAttempted {
            message_id,
            attempt: 1,
            pane: pane.into(),
        })
        .await;
        self.report(session, LifecycleEvent::WorkingStart).await;
        self.run(OrgCommand::Delivered {
            message_id,
            session_id: session.into(),
            confirmation: "prompt".into(),
        })
        .await;
    }

    async fn state(&self) -> OrgState {
        self.org.snapshot().await.unwrap()
    }
}

async fn answer(call: Call) -> Result<HandoverResult, MethodError> {
    tokio::time::timeout(GUARD, call)
        .await
        .expect("the handover did not answer")
        .unwrap()
}

fn refusal(result: Result<HandoverResult, MethodError>) -> (String, String) {
    let error = result.expect_err("the handover must be refused").0;
    (error.code, error.message)
}

// Pure decisions.

#[test]
fn the_save_goes_on_only_when_delivered_idle_and_changed() {
    let facts = |delivered, idle, worked| SaveFacts {
        delivered,
        idle,
        worked,
    };
    for worked in [false, true] {
        for changed in [false, true] {
            // Not delivered, or a turn still running: wait, whatever the file says.
            assert_eq!(
                save_progress(facts(false, true, worked), changed),
                SaveProgress::Wait
            );
            assert_eq!(
                save_progress(facts(true, false, worked), changed),
                SaveProgress::Wait
            );
        }
        assert_eq!(
            save_progress(facts(true, true, worked), true),
            SaveProgress::Saved
        );
    }
    assert_eq!(
        save_progress(facts(true, true, true), false),
        SaveProgress::Unchanged
    );
    // Idle with an unchanged file, and no turn seen since the delivery: the
    // idle may be from before the save turn.
    assert_eq!(
        save_progress(facts(true, true, false), false),
        SaveProgress::Wait
    );
}

#[test]
fn a_turn_counts_as_the_save_turn_only_after_the_delivery() {
    let mut facts = SaveFacts::default();
    facts.status(SessionStatus::Working);
    facts.status(SessionStatus::Idle);
    assert!(
        !facts.worked,
        "a turn before the delivery is not the save turn"
    );
    facts.mark_delivered();
    assert!(!facts.worked, "delivered while idle: no turn seen yet");
    facts.status(SessionStatus::AttentionNeeded);
    assert!(facts.worked && !facts.idle);
    facts.status(SessionStatus::Idle);
    assert_eq!(save_progress(facts, false), SaveProgress::Unchanged);

    let mut busy = SaveFacts::default();
    busy.status(SessionStatus::Working);
    busy.mark_delivered();
    assert!(
        busy.worked,
        "delivered into a running turn: that is the save turn"
    );
}

#[test]
fn the_save_request_is_recognized_by_equality_only() {
    let memory = PathBuf::from("/project/.atm/memory/worker.md");
    let request = save_request(&id("worker"), &memory);
    assert_eq!(request.from, Address::System);
    assert_eq!(request.to, Address::Agent(id("worker")));
    assert_eq!(request.kind, MessageKind::Note);
    assert_eq!(
        request.body,
        "Handover: write what your next session needs to /project/.atm/memory/worker.md, \
         then end your turn. Your session is cleared after that."
    );
    let mut state = OrgState::default();
    let events = [
        OrgEvent::ProjectRegistered {
            project_id: "p".parse().unwrap(),
            root: "/project".into(),
        },
        OrgEvent::AgentUpRequested {
            agent_id: id("worker"),
            role: atm_org::parse_role("worker", "+++\nharness='pi'\n+++\n").unwrap(),
        },
        // m3: almost the request (another kind); m4: the request.
        OrgEvent::MessageSent(MessageContent {
            kind: MessageKind::Brief,
            ..request.clone()
        }),
        OrgEvent::MessageSent(request.clone()),
    ];
    for (seq, event) in events.iter().enumerate() {
        state.apply(seq as u64 + 1, Utc::now(), event).unwrap();
    }
    assert_eq!(open_save_request(&state, &request), Some(MessageId::new(4)));
    let other = save_request(&id("worker"), &PathBuf::from("/elsewhere/worker.md"));
    assert_eq!(open_save_request(&state, &other), None);
    // Delivered: no longer open, so a later handover asks again.
    let delivered = OrgEvent::MessageDelivered {
        message_id: MessageId::new(4),
        session_id: "s1".into(),
        confirmation: "prompt".into(),
    };
    state.apply(5, Utc::now(), &delivered).unwrap();
    assert_eq!(open_save_request(&state, &request), None);
}

#[test]
fn changed_means_a_regular_file_that_is_not_what_it_was() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("worker.md");
    let absent = fingerprint(&file).unwrap();
    assert_eq!(absent, None);
    assert!(!changed(None, None), "still absent is not saved");

    std::fs::write(&file, "one").unwrap();
    let first = fingerprint(&file).unwrap();
    assert!(changed(absent.as_ref(), first.as_ref()), "new file");
    assert!(!changed(
        first.as_ref(),
        fingerprint(&file).unwrap().as_ref()
    ));

    std::fs::write(&file, "longer").unwrap();
    let longer = fingerprint(&file).unwrap();
    assert!(changed(first.as_ref(), longer.as_ref()), "another length");

    // The same length at another time: set the time, do not wait for it.
    let later = std::time::SystemTime::now() + Duration::from_secs(60);
    let handle = std::fs::File::options().write(true).open(&file).unwrap();
    handle.set_modified(later).unwrap();
    let touched = fingerprint(&file).unwrap();
    assert!(changed(longer.as_ref(), touched.as_ref()), "another time");

    std::fs::remove_file(&file).unwrap();
    assert!(!changed(
        touched.as_ref(),
        fingerprint(&file).unwrap().as_ref()
    ));
    assert!(matches!(
        fingerprint(dir.path()),
        Err(AgentError::Refused(reason)) if reason.contains("not a regular file")
    ));
}

// The handover.

#[tokio::test]
async fn a_handover_asks_waits_for_the_save_and_clears() {
    let mut rig = Rig::new().await;
    let (_deadline, call) = rig.start("worker", owner());
    let request = rig.requested().await;
    rig.deliver(request, "s1", "%1").await;
    std::fs::write(rig.memory("worker"), "state").unwrap();
    rig.report("s1", LifecycleEvent::Idle).await;

    let result = answer(call).await.unwrap();
    assert_eq!(result.agent_id, id("worker"));
    assert_eq!(result.message_id, request);
    assert_eq!(result.state, HandoverState::Cleared);
    assert_eq!(result.session_id, Some(SessionId::new("after-clear")));
    assert_eq!(rig.cleared.try_recv().unwrap(), id("worker"));
    assert!(rig.cleared.try_recv().is_err(), "cleared once");
    assert_eq!(
        rig.kinds(),
        ["msg.delivery_attempted", "msg.delivered", "agent.saved"]
    );
    let state = rig.state().await;
    let message = state.message(&request).unwrap();
    assert_eq!(
        message.content,
        save_request(&id("worker"), &rig.memory("worker"))
    );
}

#[tokio::test]
async fn a_turn_that_ends_without_a_changed_file_is_not_cleared() {
    let mut rig = Rig::new().await;
    std::fs::write(rig.memory("worker"), "old state").unwrap();
    let (_deadline, call) = rig.start("worker", owner());
    let request = rig.requested().await;
    rig.deliver(request, "s1", "%1").await;
    rig.report("s1", LifecycleEvent::Idle).await;

    let (code, message) = refusal(answer(call).await);
    assert_eq!(code, error_code::REFUSED);
    assert_eq!(
        message,
        format!(
            "p/worker ended its save turn and {} did not change; nothing was cleared",
            rig.memory("worker").display()
        )
    );
    assert!(rig.cleared.try_recv().is_err(), "nothing was cleared");
    assert_eq!(rig.kinds(), ["msg.delivery_attempted", "msg.delivered"]);
}

#[tokio::test]
async fn a_save_turn_that_never_ends_is_refused_at_the_deadline() {
    let mut rig = Rig::new().await;
    let (deadline, call) = rig.start("worker", owner());
    let request = rig.requested().await;
    rig.deliver(request, "s1", "%1").await;
    // The file is there, the turn is not over: no clear.
    std::fs::write(rig.memory("worker"), "state").unwrap();
    deadline.send(()).unwrap();

    let (code, message) = refusal(answer(call).await);
    assert_eq!(code, error_code::TIMEOUT);
    assert_eq!(
        message,
        "p/worker's save turn did not end in 600 s; nothing was cleared"
    );
    assert!(rig.cleared.try_recv().is_err());
    assert!(!rig.kinds().contains(&"agent.saved".to_string()));
}

#[tokio::test]
async fn a_dialog_that_holds_the_save_turn_is_named() {
    let mut rig = Rig::new().await;
    let (deadline, call) = rig.start("worker", owner());
    let request = rig.requested().await;
    rig.deliver(request, "s1", "%1").await;
    let reason = NeedsInputReason::PermissionGate { tool: Tool::Write };
    rig.report("s1", LifecycleEvent::NeedsInput { reason })
        .await;
    deadline.send(()).unwrap();

    let (code, message) = refusal(answer(call).await);
    assert_eq!(code, error_code::TIMEOUT);
    assert_eq!(
        message,
        "p/worker's save turn did not end in 600 s; it waits at a dialog in %1; \
         nothing was cleared"
    );
    assert!(rig.cleared.try_recv().is_err());
}

#[tokio::test]
async fn an_undelivered_request_stays_queued_and_the_next_handover_reuses_it() {
    let mut rig = Rig::new().await;
    let (deadline, call) = rig.start("worker", owner());
    let request = rig.requested().await;
    deadline.send(()).unwrap();
    let (code, message) = refusal(answer(call).await);
    assert_eq!(code, error_code::TIMEOUT);
    assert_eq!(
        message,
        format!(
            "the save request {request} was not delivered to p/worker in 600 s; \
             `atm inbox --for p/worker --id {request}` shows what holds it; it stays queued and \
             a later handover reuses it; `atm msg give-up {request}` cancels it; \
             nothing was cleared"
        )
    );
    assert!(rig.kinds().is_empty(), "the request was not given up");

    let (_deadline, call) = rig.start("worker", owner());
    assert_eq!(rig.requested().await, request, "the open request is reused");
    rig.deliver(request, "s1", "%1").await;
    std::fs::write(rig.memory("worker"), "state").unwrap();
    rig.report("s1", LifecycleEvent::Idle).await;
    assert_eq!(answer(call).await.unwrap().message_id, request);
    assert!(
        !rig.kinds().contains(&"msg.sent".to_string()),
        "no second save request"
    );
}

#[tokio::test]
async fn an_idle_seen_before_the_save_turn_is_not_its_end() {
    let mut rig = Rig::new().await;
    std::fs::write(rig.memory("worker"), "old state").unwrap();
    let (_deadline, call) = rig.start("worker", owner());
    let request = rig.requested().await;
    // Delivered while the row still says idle (the prompt's status follows).
    rig.run(OrgCommand::DeliveryAttempted {
        message_id: request,
        attempt: 1,
        pane: "%1".into(),
    })
    .await;
    rig.run(OrgCommand::Delivered {
        message_id: request,
        session_id: "s1".into(),
        confirmation: "prompt".into(),
    })
    .await;
    // A second fact the handover must have looked at the delivery to give:
    // only then is the unchanged file below judged, and it must be waited for.
    rig.report("s1", LifecycleEvent::WorkingStart).await;
    std::fs::write(rig.memory("worker"), "new state, longer").unwrap();
    rig.report("s1", LifecycleEvent::Idle).await;
    assert_eq!(answer(call).await.unwrap().state, HandoverState::Cleared);
}

#[tokio::test]
async fn an_agent_that_asks_for_its_own_handover_is_answered_at_once() {
    let mut rig = Rig::new().await;
    let (_deadline, call) = rig.start("worker", from_pane("%1"));
    // Answered before anything is delivered: the caller is inside its turn.
    let result = answer(call).await.unwrap();
    assert_eq!(result.state, HandoverState::Started);
    assert_eq!(result.session_id, None);
    let request = rig.requested().await;
    assert_eq!(result.message_id, request);

    rig.deliver(request, "s1", "%1").await;
    std::fs::write(rig.memory("worker"), "state").unwrap();
    rig.report("s1", LifecycleEvent::Idle).await;
    rig.next("agent.saved").await;
    let cleared = tokio::time::timeout(GUARD, rig.cleared.recv()).await;
    assert_eq!(cleared.unwrap(), Some(id("worker")));
    assert!(
        !rig.kinds().contains(&"msg.sent".to_string()),
        "a handover that worked sends no note"
    );
}

#[tokio::test]
async fn a_failed_handover_nobody_waits_for_is_told_to_the_manager() {
    let mut rig = Rig::new().await;
    let (_deadline, call) = rig.start("worker", from_pane("%1"));
    assert_eq!(answer(call).await.unwrap().state, HandoverState::Started);
    let request = rig.requested().await;
    rig.deliver(request, "s1", "%1").await;
    rig.report("s1", LifecycleEvent::Idle).await;

    let note = rig.next("msg.sent").await;
    assert_eq!(note.body["from"], "system");
    assert_eq!(note.body["to"], "p/lead", "the worker's manager");
    assert_eq!(
        note.body["body"],
        format!(
            "handover of p/worker did not complete: p/worker ended its save turn and {} \
             did not change; nothing was cleared",
            rig.memory("worker").display()
        )
    );
    assert!(rig.cleared.try_recv().is_err());
}

#[tokio::test]
async fn the_leads_failed_handover_is_told_to_the_owner() {
    let mut rig = Rig::new().await;
    let (_deadline, call) = rig.start("lead", from_pane("%0"));
    assert_eq!(answer(call).await.unwrap().state, HandoverState::Started);
    let request = rig.requested().await;
    rig.deliver(request, "lead-1", "%0").await;
    rig.report("lead-1", LifecycleEvent::Idle).await;
    let note = rig.next("msg.sent").await;
    assert_eq!(note.body["to"], "owner");
    assert!(note.body["body"]
        .as_str()
        .unwrap()
        .starts_with("handover of p/lead did not complete: "));
}

#[tokio::test]
async fn a_caller_that_waits_gets_the_refusal_and_no_note() {
    let mut rig = Rig::new().await;
    // The lead asks for the worker's handover: not the agent itself.
    let (_deadline, call) = rig.start("worker", from_pane("%0"));
    let request = rig.requested().await;
    rig.deliver(request, "s1", "%1").await;
    rig.report("s1", LifecycleEvent::Idle).await;
    let (code, _) = refusal(answer(call).await);
    assert_eq!(code, error_code::REFUSED);
    assert_eq!(rig.kinds(), ["msg.delivery_attempted", "msg.delivered"]);
}

#[tokio::test]
async fn a_handover_is_refused_before_anything_is_sent() {
    let mut rig = Rig::new().await;
    rig.agent("fresh", "+++\nharness='pi'\n+++\n", "%2", None)
        .await;
    rig.agent("editor", "+++\nharness='pi'\n+++\n", "%3", None)
        .await;
    rig.run(OrgCommand::EditorReady {
        agent_id: id("editor"),
    })
    .await;
    rig.kinds();
    for (name, code, reason) in [
        (
            "nobody",
            error_code::NOT_FOUND,
            "agent p/nobody was not found",
        ),
        (
            "fresh",
            error_code::REFUSED,
            "agent p/fresh is starting; only a ready agent can hand over",
        ),
        (
            "editor",
            error_code::REFUSED,
            "agent p/editor has no session; atm cannot see its turn end",
        ),
    ] {
        let (_deadline, call) = rig.start(name, owner());
        assert_eq!(
            refusal(answer(call).await),
            (code.to_string(), reason.to_string())
        );
    }
    assert!(rig.kinds().is_empty(), "nothing was sent or recorded");
}

#[tokio::test]
async fn one_handover_per_agent_at_a_time() {
    let mut rig = Rig::new().await;
    let (_deadline, first) = rig.start("worker", owner());
    let request = rig.requested().await;
    let (_second_deadline, second) = rig.start("worker", owner());
    assert_eq!(
        refusal(answer(second).await),
        (
            error_code::CONFLICT.to_string(),
            "a handover of p/worker is already running".to_string()
        )
    );
    assert!(rig.kinds().is_empty(), "the second call sent nothing");
    // Another agent is not held up, and the first handover still ends.
    let (_lead_deadline, lead) = rig.start("lead", owner());
    let lead_request = rig.requested().await;
    assert_ne!(lead_request, request);
    rig.deliver(request, "s1", "%1").await;
    std::fs::write(rig.memory("worker"), "state").unwrap();
    rig.report("s1", LifecycleEvent::Idle).await;
    answer(first).await.unwrap();
    // Released: the worker can be handed over again.
    let (_third_deadline, third) = rig.start("worker", owner());
    assert_ne!(rig.requested().await, request);
    third.abort();
    lead.abort();
}

#[tokio::test]
async fn a_refused_clear_after_the_save_says_that_the_state_is_saved() {
    let mut rig = Rig::new().await;
    let reason = "agent p/worker was not cleared: the editor holds a draft; nothing was sent to %1";
    *rig.recorder.refuse.lock().unwrap() = Some(reason.into());
    let (_deadline, call) = rig.start("worker", owner());
    let request = rig.requested().await;
    rig.deliver(request, "s1", "%1").await;
    std::fs::write(rig.memory("worker"), "state").unwrap();
    rig.report("s1", LifecycleEvent::Idle).await;
    let (code, message) = refusal(answer(call).await);
    assert_eq!(code, error_code::REFUSED);
    assert_eq!(
        message,
        format!("p/worker saved its state ({request}); {reason}")
    );
    assert_eq!(
        rig.kinds(),
        ["msg.delivery_attempted", "msg.delivered", "agent.saved"]
    );
}

#[tokio::test]
async fn an_agent_that_leaves_its_session_is_not_cleared() {
    let mut rig = Rig::new().await;
    let (_deadline, call) = rig.start("worker", owner());
    let request = rig.requested().await;
    rig.deliver(request, "s1", "%1").await;
    // Somebody cleared it by hand meanwhile: it is bound to another session.
    rig.bind("worker", "s2", "%1").await;
    let (code, message) = refusal(answer(call).await);
    assert_eq!(code, error_code::REFUSED);
    assert_eq!(
        message,
        "p/worker is no longer ready in the session it was asked to save in; nothing was cleared"
    );
    assert!(rig.cleared.try_recv().is_err());
}

// The note at the context line.

#[tokio::test]
async fn context_note_names_the_agent_its_share_and_its_manager() {
    let rig = Rig::new().await;
    let state = rig.state().await;
    let now = Utc::now();
    let limits = HandoverLimits::default();
    let s1 = SessionId::new("s1");
    assert_eq!(context_note(&state, &s1, 39.99, &limits, now), None);
    let (agent, note) = context_note(&state, &s1, 40.0, &limits, now).unwrap();
    assert_eq!(agent, id("worker"));
    assert_eq!(
        note,
        MessageContent {
            from: Address::System,
            to: Address::Agent(id("lead")),
            kind: MessageKind::Note,
            body: "agent p/worker is at 40% of its context (line 40%). \
                   Hand it over: atm agent handover p/worker"
                .into(),
            attachments: Vec::new(),
            task: None,
        }
    );
    // The line is the policy's, passed in: nothing here knows 40.
    let seventy = HandoverLimits {
        context_line_percent: 70,
        ..limits
    };
    assert_eq!(context_note(&state, &s1, 69.99, &seventy, now), None);
    let (_, note) = context_note(&state, &s1, 70.4, &seventy, now).unwrap();
    assert!(note.body.contains("is at 70% of its context (line 70%)"));
    // The lead has no manager: its note goes to the owner.
    let lead = SessionId::new("lead-1");
    let (agent, note) = context_note(&state, &lead, 55.0, &limits, now).unwrap();
    assert_eq!((agent, note.to), (id("lead"), Address::Owner));
    // A session no ready agent is bound to gets no note.
    let unbound = SessionId::new("other");
    assert_eq!(context_note(&state, &unbound, 90.0, &limits, now), None);
    rig.run(OrgCommand::Clear {
        agent_id: id("worker"),
    })
    .await;
    let clearing = rig.state().await;
    assert_eq!(context_note(&clearing, &s1, 90.0, &limits, now), None);
}

#[tokio::test]
async fn one_note_per_session_when_a_turn_ends_at_the_line() {
    let mut rig = Rig::new().await;
    let _watcher = spawn_context_notes(rig.org.clone(), rig.registry.clone(), rig.cancel.clone());
    // Below the line: no note.
    rig.share("s1", 399).await;
    rig.turn("s1").await;
    // At the line: the first note.
    rig.share("s1", 400).await;
    rig.turn("s1").await;
    // The same session again, higher: no second note.
    rig.share("s1", 500).await;
    rig.turn("s1").await;
    // A session no agent is bound to: no note.
    let mut other = SessionDomain::new(
        SessionId::new("other"),
        AgentType::GeneralPurpose,
        Model::Unknown,
    );
    other.harness = Harness::Pi;
    rig.registry.register(other).await.unwrap();
    rig.share("other", 900).await;
    rig.turn("other").await;
    // The worker in a new session (after a clear): a new note.
    rig.bind("worker", "s2", "%1").await;
    rig.share("s2", 450).await;
    rig.turn("s2").await;
    // A share that is reported after the turn's end still counts, and the
    // lead's note goes to the owner.
    rig.turn("lead-1").await;
    rig.share("lead-1", 600).await;

    // The watcher reads these in order, so the three notes below, with no
    // other between them, are all that the sequence above produced.
    let mut notes = Vec::new();
    for _ in 0..3 {
        let note = rig.next("msg.sent").await;
        notes.push((
            note.body["to"].as_str().unwrap().to_owned(),
            note.body["body"].as_str().unwrap().to_owned(),
        ));
    }
    let text = |agent: &str, share: u32| {
        format!(
            "agent p/{agent} is at {share}% of its context (line 40%). \
             Hand it over: atm agent handover p/{agent}"
        )
    };
    assert_eq!(
        notes,
        [
            ("p/lead".to_owned(), text("worker", 40)),
            ("p/lead".to_owned(), text("worker", 45)),
            ("owner".to_owned(), text("lead", 60)),
        ]
    );
}
