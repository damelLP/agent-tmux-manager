//! The handover on a real organisation actor and a real registry. The test
//! plays the message runtime (it records the request's delivery), the agent
//! (it writes the memory file and moves its session's status) and the clock
//! (it resolves the deadline by hand). The clear is a recorder: the real one
//! is exercised with the stand-in harness in `tests/agent_handover.rs`.
//! Every wait is on a committed event, a channel or the call's own answer.

use super::agent_auto_handover::spawn_handover_watch;
use super::agent_handover::*;
use super::agent_lifecycle::{ClearAttempt, ClearOutcome, SavedSeat};
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
    Address, AgentId, DeliveryStatus, MessageContent, MessageId, MessageKind, OrgCommand, OrgEvent,
    OrgState,
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
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// A hang guard only; nothing below depends on how long a step takes.
const GUARD: Duration = Duration::from_secs(30);

struct Recorder {
    called: mpsc::UnboundedSender<AgentId>,
    refuse: Mutex<Option<String>>,
    blocked: Mutex<Option<&'static str>>,
    clear_gate: Mutex<Option<oneshot::Receiver<()>>>,
    completion: Arc<Mutex<Option<ClearOutcome>>>,
    waits: Mutex<Vec<Option<String>>>,
    parked: Mutex<Option<oneshot::Sender<()>>>,
}

impl Clearer for Recorder {
    fn available(&self) -> std::pin::Pin<Box<dyn Future<Output = Result<(), AgentError>> + Send>> {
        Box::pin(async { Ok(()) })
    }
    fn clear(
        &self,
        target: SavedSeat,
        _retry_before: Option<Instant>,
        _finish_by: Instant,
    ) -> Answer<ClearAttempt> {
        let refuse = self.refuse.lock().unwrap().clone();
        let blocked = *self.blocked.lock().unwrap();
        let gate = self.clear_gate.lock().unwrap().take();
        let completion = self.completion.clone();
        let _ = self.called.send(target.agent.clone());
        Box::pin(async move {
            if let Some(gate) = gate {
                gate.await.unwrap();
            }
            match (refuse, blocked) {
                (Some(reason), _) => Err(AgentError::Refused(reason)),
                (_, Some(reason)) => Ok(ClearAttempt::Blocked(reason)),
                _ => Ok(ClearAttempt::Cleared(
                    completion.lock().unwrap().take().unwrap_or(ClearOutcome {
                        result: ClearResult {
                            agent_id: target.agent,
                            session_id: Some(SessionId::new("after-clear")),
                        },
                        ready_seq: None,
                        start_line: super::start_line::StartLineDisposition::Unidentified,
                    }),
                )),
            }
        })
    }
    fn clear_wait(&self, _target: &SavedSeat, reason: Option<String>) {
        if reason.is_some() {
            if let Some(parked) = self.parked.lock().unwrap().take() {
                let _ = parked.send(());
            }
        }
        self.waits.lock().unwrap().push(reason);
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
    service_stop: CancellationToken,
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
        Self::with_registry(crate::registry::spawn_registry()).await
    }

    async fn with_registry(registry: RegistryHandle) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        std::fs::create_dir_all(root.join(".atm/memory")).unwrap();
        let cancel = CancellationToken::new();
        let org = OrgHandle::open(dir.path().join("state"), cancel.clone()).await;
        let (called, cleared) = mpsc::unbounded_channel();
        let recorder = Arc::new(Recorder {
            called,
            refuse: Mutex::new(None),
            blocked: Mutex::new(None),
            clear_gate: Mutex::new(None),
            completion: Arc::default(),
            waits: Mutex::new(Vec::new()),
            parked: Mutex::new(None),
        });
        let service_stop = cancel.child_token();
        let service = Handovers::new(
            org.clone(),
            registry.clone(),
            recorder.clone(),
            service_stop.clone(),
        );
        let committed = org.committed.subscribe();
        let mut rig = Self {
            _dir: dir,
            root,
            org,
            registry,
            cancel,
            service_stop,
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

    async fn register_chief(&self) {
        self.run(OrgCommand::RegisterProject {
            project_id: "org".parse().unwrap(),
            root: self._dir.path().join("org").to_str().unwrap().into(),
            move_root: false,
        })
        .await;
        self.run(OrgCommand::Up {
            agent_id: "org/chief".parse().unwrap(),
            role: atm_org::parse_role("chief", "+++\nharness='pi'\n+++\nChief").unwrap(),
        })
        .await;
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

    /// Finish the save and hold the first clear's result so the test can put
    /// events in the retry's inbox before the refusal opens the wait.
    async fn hold_first_clear(&mut self) -> (oneshot::Sender<()>, Call, MessageId) {
        let (release, gate) = oneshot::channel();
        *self.recorder.clear_gate.lock().unwrap() = Some(gate);
        let (_deadline, call) = self.start("worker", owner());
        let save = self.requested().await;
        self.deliver(save, "s1", "%1").await;
        std::fs::write(self.memory("worker"), "state").unwrap();
        self.report("s1", LifecycleEvent::Idle).await;
        assert_eq!(
            tokio::time::timeout(GUARD, self.cleared.recv())
                .await
                .unwrap(),
            Some(id("worker"))
        );
        (release, call, save)
    }
}

#[derive(Clone, Copy)]
enum AdmissionWrite {
    Queue,
    Audit,
}

/// Pause one service-side write, leaving the real actor and its other readers live.
/// Tests may commit it and hold its reply, or return a failure without committing.
struct AdmissionGate {
    writes: mpsc::UnboundedReceiver<super::Request>,
    forwarding: tokio::task::JoinHandle<()>,
}

impl Drop for AdmissionGate {
    fn drop(&mut self) {
        self.forwarding.abort();
    }
}

impl Rig {
    fn gate(&mut self, boundary: impl Into<Option<AdmissionWrite>>) -> AdmissionGate {
        let boundary = boundary.into();
        let real = self.org.clone();
        let (requests, mut receive) = mpsc::channel(64);
        self.service.org = OrgHandle {
            requests,
            ..real.clone()
        };
        let (held, writes) = mpsc::unbounded_channel();
        let forwarding = tokio::spawn(async move {
            let mut armed = true;
            while let Some(request) = receive.recv().await {
                let matches = match &request {
                    super::Request::Execute { command, .. } => match (boundary, command.as_ref()) {
                        (Some(AdmissionWrite::Queue), OrgCommand::SendMessage(content)) => {
                            content.body.starts_with("Handover:")
                        }
                        (Some(AdmissionWrite::Audit), OrgCommand::SaveRequested { .. }) => true,
                        _ => false,
                    },
                    _ => false,
                };
                if armed && matches {
                    armed = false;
                    held.send(request).unwrap();
                } else {
                    real.submit(request).unwrap();
                }
            }
        });
        AdmissionGate { writes, forwarding }
    }
}

type WriteResult = Result<super::TaskCommit, super::OrgServiceError>;

impl AdmissionGate {
    async fn take(&mut self) -> super::Request {
        tokio::time::timeout(GUARD, self.writes.recv())
            .await
            .expect("admission did not reach the write boundary")
            .unwrap()
    }

    /// Drop the service's sender and join after every retained sender is gone.
    /// A manual worker cannot enqueue a later duplicate note beyond this fence.
    async fn finish(mut self, rig: &mut Rig) {
        rig.service.org = rig.org.clone();
        tokio::time::timeout(GUARD, &mut self.forwarding)
            .await
            .expect("retained handover did not release its sender")
            .unwrap();
    }
}

/// Commit through the real actor, but leave its original waiter behind a gate.
async fn commit_held(
    org: &OrgHandle,
    request: super::Request,
) -> (oneshot::Sender<WriteResult>, WriteResult) {
    let super::Request::Execute {
        command,
        now,
        caller_pane,
        reply,
    } = request
    else {
        panic!("gate held a read instead of a write");
    };
    let (forward, response) = oneshot::channel();
    org.submit(super::Request::Execute {
        command,
        now,
        caller_pane,
        reply: forward,
    })
    .unwrap();
    (reply, response.await.unwrap())
}

fn audit_failure(request: MessageId) -> String {
    format!(
        "the save request {request} for p/worker exists, but its handover record was not \
         confirmed: organisation request timed out; accepted work may still complete; \
         the request may be queued or already delivered; nothing was cleared; inspect \
         `atm inbox --for p/worker --id {request}`; a later handover reuses it while queued \
         or delivering; `atm msg give-up {request}` cancels an undelivered request"
    )
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

/// The ordering a busy daemon can produce: the agent wrote its memory file
/// before the request reached it (it asked for its own handover and wrote in
/// the same turn), and the job reads the delivery from committed state while
/// the save turn's first status is still unread in its registry channel. The
/// file has changed and the last status the job knows is idle, yet the save
/// turn has only begun. The deadline is already due, so a job that waits for
/// the turn answers "did not end", and one that takes the stale idle for the
/// turn's end answers `cleared`.
#[tokio::test]
async fn a_delivery_read_before_the_save_turns_status_does_not_end_the_save() {
    let mut rig = Rig::new().await;
    std::fs::write(rig.memory("worker"), "old state").unwrap();
    let p = HandoverParams {
        agent: id("worker"),
        timeout_ms: 600_000,
    };
    let due = Box::pin(std::future::ready(()));
    let admitted = rig.service.admit(p, owner(), due, None).await.ok().unwrap();
    let job = admitted.job.await.unwrap();
    let request = rig.requested().await;
    std::fs::write(rig.memory("worker"), "written before the request arrived").unwrap();
    // The prompt: the registry shows the save turn, then the delivery is
    // committed. The job has not run yet and has read neither.
    rig.deliver(request, "s1", "%1").await;

    let (code, message) = refusal(
        answer(tokio::spawn(async move {
            let mut job = job;
            job.run().await
        }))
        .await,
    );
    assert_eq!(code, error_code::TIMEOUT);
    assert_eq!(
        message,
        "p/worker's save turn did not end in 600 s; nothing was cleared"
    );
    assert!(
        rig.cleared.try_recv().is_err(),
        "cleared before the save turn"
    );
    assert!(!rig.kinds().contains(&"agent.saved".to_string()));
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
    let note = rig.next("msg.sent").await;
    assert_eq!(note.body["to"], "p/lead");
    assert_eq!(note.body["body"], format!("handover of p/worker did not complete: saved ({request}); clear confirmed; resume not confirmed: the start line could not be identified."));
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
async fn a_failure_after_the_callers_call_has_ended_is_told_to_the_manager() {
    let mut rig = Rig::new().await;
    // The lead asks for the worker's handover and waits for the answer.
    let (_deadline, call) = rig.start("worker", from_pane("%0"));
    let request = rig.requested().await;
    // The call ends before the handover does (the daemon drops a call at
    // its deadline). Awaited, so the waiting side is gone before the end.
    call.abort();
    assert!(call.await.unwrap_err().is_cancelled());
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
async fn a_clear_after_its_caller_left_reports_unconfirmed_resume() {
    let mut rig = Rig::new().await;
    let (_deadline, call) = rig.start("worker", from_pane("%0"));
    let request = rig.requested().await;
    call.abort();
    assert!(call.await.unwrap_err().is_cancelled());
    rig.deliver(request, "s1", "%1").await;
    std::fs::write(rig.memory("worker"), "state").unwrap();
    rig.report("s1", LifecycleEvent::Idle).await;
    let cleared = tokio::time::timeout(GUARD, rig.cleared.recv()).await;
    assert_eq!(cleared.unwrap(), Some(id("worker")));
    let note = rig.next("msg.sent").await;
    assert_eq!(note.body["to"], "p/lead");
    assert!(note.body["body"]
        .as_str()
        .unwrap()
        .contains("clear confirmed; resume not confirmed"));
    assert!(rig.kinds().is_empty());
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
    let note = rig.next("msg.sent").await;
    assert_eq!(
        note.body["body"],
        format!("handover of p/worker did not complete: saved ({request}); {reason}.")
    );
    assert_eq!(note.body["to"], "p/lead");
}

// Break caught: reporting success from clear/queue alone, accepting another
// message's receipt, or freezing the manager before the resume is confirmed.
#[tokio::test]
async fn handover_requires_exact_replayed_resume_receipt_and_current_manager() {
    use crate::org::start_line::{start_line, StartLineDisposition};
    let mut rig = Rig::new().await;
    rig.register_chief().await;
    rig.kinds();
    let (release, gate) = oneshot::channel();
    *rig.recorder.clear_gate.lock().unwrap() = Some(gate);
    rig.share("s1", 510).await;
    let (_deadline, call) = rig.start("worker", owner());
    let save = rig.requested().await;
    rig.deliver(save, "s1", "%1").await;
    std::fs::write(rig.memory("worker"), "state").unwrap();
    rig.report("s1", LifecycleEvent::Idle).await;
    assert_eq!(
        tokio::time::timeout(GUARD, rig.cleared.recv())
            .await
            .unwrap(),
        Some(id("worker"))
    );
    let saved = rig.next("agent.saved").await;
    let clear = rig
        .run(OrgCommand::Clear {
            agent_id: id("worker"),
        })
        .await[0]
        .seq;
    // Use an actual Ready commit and a pending *existing* canonical start line.
    let resume = MessageId::new(
        rig.run(OrgCommand::SendMessage(start_line(&id("worker"))))
            .await[0]
            .seq,
    );
    let ready = rig
        .run(OrgCommand::SessionBound {
            agent_id: id("worker"),
            session_id: "s2".into(),
        })
        .await[0]
        .seq;
    *rig.recorder.completion.lock().unwrap() = Some(ClearOutcome {
        result: ClearResult {
            agent_id: id("worker"),
            session_id: Some("s2".into()),
        },
        ready_seq: Some(ready),
        start_line: StartLineDisposition::Track(resume),
    });
    release.send(()).unwrap();
    let done = answer(call).await.unwrap();
    assert_eq!(done.state, HandoverState::Cleared);
    assert_eq!(done.session_id, Some("s2".into()));
    // Drain the existing start-line send before looking for the outcome.
    assert_eq!(rig.next("msg.sent").await.seq, resume.seq());
    let roles = rig.root.join(".atm/roles");
    std::fs::create_dir_all(&roles).unwrap();
    std::fs::write(
        roles.join("worker.md"),
        "+++\nharness='pi'\nmanager='org/chief'\n+++\nWork",
    )
    .unwrap();
    let receipt = rig
        .run(OrgCommand::Delivered {
            message_id: resume,
            session_id: "s2".into(),
            confirmation: "prompt_submit_exact_reference".into(),
        })
        .await;
    let note = rig.next("msg.sent").await;
    assert!(
        saved.seq < clear && clear < ready && ready < receipt[0].seq && receipt[0].seq < note.seq
    );
    assert_eq!(note.body["from"], "system");
    assert_eq!(note.body["to"], "org/chief");
    assert_eq!(note.body["body"], format!("handover of p/worker (context 51%) complete: saved ({save}); cleared and bound s1 -> s2; resume prompt {resume} delivered."));
    assert_eq!(rig.state().await.messages().count(), 3);
}

#[test]
fn handover_resume_evidence_rejects_wrong_id_session_and_lifecycle() {
    use serde_json::json;
    let (seat, old, resume) = (id("worker"), SessionId::new("s1"), MessageId::new(8));
    let event = |kind: &str, body| Envelope {
        seq: 10,
        ts: Utc::now(),
        kind: kind.into(),
        body,
    };
    let bind = event(
        "agent.session_bound",
        json!({"agent_id":"p/worker", "session_id":"s2"}),
    );
    let receipt = |id: &str, session: &str| {
        event(
            "msg.delivered",
            json!({"message_id":id, "session_id":session, "confirmation":"prompt_submit_exact_reference"}),
        )
    };
    let delivered = receipt("m8", "s2");
    for [first, second] in [[&bind, &delivered], [&delivered, &bind]] {
        let mut facts = ResumeFacts::default();
        assert_eq!(facts.apply(first, &seat, &old, resume), Ok(None));
        assert_eq!(
            facts.apply(second, &seat, &old, resume),
            Ok(Some("s2".into()))
        );
    }
    let mut facts = ResumeFacts {
        bound: Some("s2".into()),
        receipt: None,
    };
    assert_eq!(
        facts.apply(&receipt("m7", "s2"), &seat, &old, resume),
        Ok(None)
    );
    for session in ["s1", "s3", "pending-42"] {
        assert_eq!(
            facts.apply(&receipt("m8", session), &seat, &old, resume),
            Err("start line m8 was not confirmed in the replacement session".into())
        );
    }
    let down = event("agent.down", json!({"agent_id":"p/worker"}));
    assert_eq!(
        facts.apply(&down, &seat, &old, resume),
        Err("the agent left the cleared lifecycle".into())
    );
    let failed = event(
        "msg.delivery_failed",
        json!({"message_id":"m8", "reason":"owner gave up", "final_failure":true}),
    );
    assert_eq!(
        facts.apply(&failed, &seat, &old, resume),
        Err("start line m8 failed: owner gave up".into())
    );
}

// Break caught: a registry update is ignored; the 250 ms backoff must not be
// able to satisfy this test, even if the scheduler auto-advances paused time.
#[tokio::test]
async fn handover_clear_retries_after_a_same_session_update() {
    let mut rig = Rig::new().await;
    let (parked, wait_entered) = oneshot::channel();
    *rig.recorder.parked.lock().unwrap() = Some(parked);
    *rig.recorder.blocked.lock().unwrap() = Some("the editor is not empty and idle");
    let (release, call, request) = rig.hold_first_clear().await;
    assert_eq!(
        rig.kinds(),
        ["msg.delivery_attempted", "msg.delivered", "agent.saved"]
    );
    *rig.recorder.blocked.lock().unwrap() = None;
    tokio::time::pause(); // no more filesystem work until after the retry
    let before = Instant::now();
    release.send(()).unwrap();
    tokio::time::timeout(GUARD, wait_entered)
        .await
        .unwrap()
        .unwrap();
    rig.share("s1", 510).await;
    assert_eq!(
        tokio::time::timeout(GUARD, rig.cleared.recv())
            .await
            .unwrap(),
        Some(id("worker"))
    );
    assert_eq!(
        Instant::now() - before,
        Duration::ZERO,
        "the timer, not the registry event, woke the retry"
    );
    tokio::time::resume();
    let result = answer(call).await.unwrap();
    assert_eq!(result.message_id, request);
    assert_eq!(result.session_id, Some(SessionId::new("after-clear")));
    assert!(rig.cleared.try_recv().is_err());
    let note = rig.next("msg.sent").await;
    assert_eq!(note.body["to"], "p/lead", "not a new save request");
    assert!(rig.kinds().is_empty(), "did not queue another save");
}

// Break caught: a harmless Updated coalesces away the following Removed, so
// the restored tuple hides that this seat disappeared while the job waited.
#[tokio::test]
async fn handover_clear_keeps_queued_removal_before_resume() {
    let mut rig = Rig::new().await;
    *rig.recorder.blocked.lock().unwrap() = Some("the editor is not empty and idle");
    let (release, call, _) = rig.hold_first_clear().await;
    tokio::time::pause();
    rig.share("s1", 510).await;
    rig.registry.remove("s1".into()).await.unwrap();
    let mut row = SessionDomain::new("s1".into(), AgentType::GeneralPurpose, Model::Unknown);
    row.harness = Harness::Pi;
    row.tmux_pane = Some("%1".into());
    rig.registry.register(row).await.unwrap();
    rig.share("s1", 520).await;
    *rig.recorder.blocked.lock().unwrap() = None;
    release.send(()).unwrap();
    let result = answer(call).await;
    assert_eq!(refusal(result).0, error_code::REFUSED);
    assert!(
        rig.cleared.try_recv().is_err(),
        "removal allowed another preflight"
    );
}

// Break caught: skipping over a pane move after the first same-session wake.
#[tokio::test]
async fn handover_clear_keeps_queued_pane_move_before_return() {
    use crate::registry::{RegistryActor, SessionEvent};
    let (requests, receive) = mpsc::channel(64);
    let (events, _) = broadcast::channel(64);
    let actor = tokio::spawn(RegistryActor::new(receive, events.clone()).run());
    let mut rig = Rig::with_registry(RegistryHandle::new(requests, events.clone())).await;
    *rig.recorder.blocked.lock().unwrap() = Some("the editor is not empty and idle");
    let (release, call, _) = rig.hold_first_clear().await;
    tokio::time::pause();
    rig.share("s1", 510).await;
    // The snapshot matches again; only the notifications witness the move.
    let mut row = rig.registry.get_session("s1".into()).await.unwrap();
    for pane in ["%2", "%1"] {
        row.tmux_pane = Some(pane.into());
        events
            .send(SessionEvent::Updated {
                session: Box::new(row.clone()),
            })
            .unwrap();
    }
    *rig.recorder.blocked.lock().unwrap() = None;
    release.send(()).unwrap();
    let result = answer(call).await;
    assert_eq!(refusal(result).0, error_code::REFUSED);
    assert!(
        rig.cleared.try_recv().is_err(),
        "pane move allowed another preflight"
    );
    actor.abort();
    let _ = actor.await;
}

// Break caught: a missed registry invalidation becomes permission to retry
// against a now-matching tuple. A lagged receiver must fail closed.
#[tokio::test]
async fn handover_clear_does_not_retry_with_lagged_registry_evidence() {
    let mut rig = Rig::new().await;
    *rig.recorder.blocked.lock().unwrap() = Some("the editor is not empty and idle");
    let (release, call, _) = rig.hold_first_clear().await;
    tokio::time::pause();
    let mut witness = rig.registry.subscribe();
    for share in 0..4096 {
        rig.share("s1", share % 1000).await;
    }
    assert!(matches!(
        witness.try_recv(),
        Err(broadcast::error::TryRecvError::Lagged(_))
    ));
    *rig.recorder.blocked.lock().unwrap() = None;
    release.send(()).unwrap();
    let result = answer(call).await;
    assert_eq!(refusal(result).0, error_code::UNAVAILABLE);
    assert!(
        rig.cleared.try_recv().is_err(),
        "registry lag allowed another preflight"
    );
}

// Break caught: a parked wait-source error loses the last safe-editor refusal
// in the RPC or the manager's terminal note.
#[tokio::test]
async fn handover_clear_reports_last_refusal_when_wait_source_lags() {
    let mut rig = Rig::new().await;
    *rig.recorder.blocked.lock().unwrap() = Some("the session is not idle");
    let (release, call, save) = rig.hold_first_clear().await;
    let saved = rig.next("agent.saved").await;
    let mut witness = rig.org.committed.subscribe();
    for _ in 0..2048 {
        rig.org.committed.send(saved.clone()).unwrap();
    }
    assert!(matches!(
        witness.try_recv(),
        Err(broadcast::error::TryRecvError::Lagged(_))
    ));
    rig.committed = rig.org.committed.subscribe();
    tokio::time::pause();
    release.send(()).unwrap();
    let (code, reply) = refusal(answer(call).await);
    tokio::time::resume();
    assert_eq!(code, error_code::UNAVAILABLE);
    let reason = reply
        .strip_prefix(&format!("p/worker saved its state ({save}); "))
        .unwrap();
    assert!(
        reason.starts_with("could not wait for another screen observation ("),
        "{reason}"
    );
    assert!(reason.ends_with("; last clear refusal: agent p/worker was not cleared: the session is not idle; nothing was sent to %1"), "{reason}");
    let note = rig.next("msg.sent").await;
    assert_eq!(
        note.body["body"],
        format!("handover of p/worker did not complete: saved ({save}); {reason}.")
    );
    assert_eq!(note.body["to"], "p/lead");
    assert!(rig.cleared.try_recv().is_err());
}

// Break caught: resolver shutdown leaves the detached job waiting on
// broadcast senders it owns; the timer wakes it for another preflight.
#[tokio::test]
async fn handover_clear_stops_parked_retry_on_agent_service_shutdown() {
    let mut rig = Rig::new().await;
    let fence = rig.gate(None);
    spawn_handover_watch(rig.service.clone(), rig.service_stop.clone());
    *rig.recorder.blocked.lock().unwrap() = Some("the editor is not empty and idle");
    let (parked, wait_entered) = oneshot::channel();
    *rig.recorder.parked.lock().unwrap() = Some(parked);
    let (release, call, _) = rig.hold_first_clear().await;
    tokio::time::pause();
    release.send(()).unwrap();
    tokio::time::timeout(GUARD, wait_entered)
        .await
        .unwrap()
        .unwrap();
    *rig.recorder.blocked.lock().unwrap() = None;
    let stopped_at = Instant::now();
    rig.service_stop.cancel();
    let result = answer(call).await;
    assert_eq!(refusal(result).0, error_code::UNAVAILABLE);
    fence.finish(&mut rig).await;
    assert_eq!(
        stopped_at,
        Instant::now(),
        "the parked job waited past shutdown"
    );
    assert!(
        rig.cleared.try_recv().is_err(),
        "service shutdown allowed another preflight"
    );
    assert_eq!(
        rig.recorder.waits.lock().unwrap().last(),
        Some(&None),
        "parked overlay leaked"
    );
}

// Break caught: after physical clear has been acknowledged to the command,
// its retained job still owns the receipt watch past agent-service shutdown.
#[tokio::test]
async fn handover_clear_stops_confirmation_on_agent_service_shutdown() {
    use super::start_line::{start_line, StartLineDisposition};
    let mut rig = Rig::new().await;
    let fence = rig.gate(None);
    spawn_handover_watch(rig.service.clone(), rig.service_stop.clone());
    let (release, call, _) = rig.hold_first_clear().await;
    rig.run(OrgCommand::Clear {
        agent_id: id("worker"),
    })
    .await;
    rig.bind("worker", "s2", "%1").await;
    let resume = MessageId::new(
        rig.run(OrgCommand::SendMessage(start_line(&id("worker"))))
            .await[0]
            .seq,
    );
    *rig.recorder.completion.lock().unwrap() = Some(ClearOutcome {
        result: ClearResult {
            agent_id: id("worker"),
            session_id: Some("s2".into()),
        },
        ready_seq: Some(resume.seq()),
        start_line: StartLineDisposition::Track(resume),
    });
    tokio::time::pause();
    let watchers = rig.org.committed.receiver_count();
    release.send(()).unwrap();
    assert_eq!(answer(call).await.unwrap().state, HandoverState::Cleared);
    assert_eq!(
        rig.org.committed.receiver_count(),
        watchers + 1,
        "waiting on live receipt evidence"
    );
    let stopped_at = Instant::now();
    rig.service_stop.cancel();
    // Fence on the retained job dropping its request sender. The org and
    // registry actors deliberately remain available during this shutdown.
    fence.finish(&mut rig).await;
    assert_eq!(
        stopped_at,
        Instant::now(),
        "confirmation survived shutdown until its deadline"
    );
    assert!(!rig
        .state()
        .await
        .messages()
        .any(|m| m.content.body.starts_with("handover of ")));
}

// Break caught: a draft removed without any new turn is never checked again;
// or an event flood extends the first refusal's fixed deadline.
#[tokio::test]
async fn handover_clear_backoff_and_cutoff_need_no_new_turn() {
    for expires in [false, true] {
        let mut rig = Rig::new().await;
        let (release, gate) = oneshot::channel();
        *rig.recorder.clear_gate.lock().unwrap() = Some(gate);
        *rig.recorder.blocked.lock().unwrap() = Some("the editor is not empty and idle");
        let (_deadline, call) = rig.start("worker", owner());
        let request = rig.requested().await;
        rig.deliver(request, "s1", "%1").await;
        std::fs::write(rig.memory("worker"), "state").unwrap();
        rig.report("s1", LifecycleEvent::Idle).await;
        assert_eq!(
            tokio::time::timeout(GUARD, rig.cleared.recv())
                .await
                .unwrap(),
            Some(id("worker"))
        );
        tokio::time::pause(); // all filesystem work is already done
        let start = Instant::now();
        release.send(()).unwrap();
        // Return of the first refused preflight and the second attempt are
        // in-memory fences; Tokio can advance only its virtual clock here.
        assert_eq!(
            tokio::time::timeout(GUARD, rig.cleared.recv())
                .await
                .unwrap(),
            Some(id("worker"))
        );
        // Tokio's virtual timer rounds a deadline up to its next 1 ms tick.
        assert!((250..=251).contains(&(Instant::now() - start).as_millis()));
        if expires {
            // The last refusal is distinct from the first; keep the window
            // anchored even though new registry frames are continually ready.
            *rig.recorder.blocked.lock().unwrap() = Some("the session is not idle");
            for share in 510..525 {
                rig.share("s1", share).await;
            }
            tokio::time::advance(Duration::from_secs(20)).await;
            let (_code, message) = refusal(call.await.unwrap());
            assert!(
                message.contains("clear retry window expired; last clear refusal:"),
                "{message}"
            );
            assert!(message.contains("the session is not idle"), "{message}");
        } else {
            // The real capture test covers how the screen refusal is made;
            // this scripted clear service only substitutes the next frame.
            *rig.recorder.blocked.lock().unwrap() = None;
            let result = call.await.unwrap().unwrap();
            assert_eq!(result.message_id, request);
            assert_eq!(result.session_id, Some(SessionId::new("after-clear")));
            assert!((750..=752).contains(&(Instant::now() - start).as_millis()));
            assert_eq!(rig.cleared.try_recv().unwrap(), id("worker"));
            assert!(rig.cleared.try_recv().is_err());
        }
        tokio::time::resume();
        let waits = rig.recorder.waits.lock().unwrap();
        assert_eq!(waits.first().unwrap().as_deref(), Some(format!("handover saved ({request}); waiting for a safe editor to clear: the editor is not empty and idle").as_str()));
        assert_eq!(waits.last(), Some(&None));
    }
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
    assert_eq!(context_note(&state, &s1, 59.99, &limits, now), None);
    let (agent, note) = context_note(&state, &s1, 60.0, &limits, now).unwrap();
    assert_eq!(agent, id("worker"));
    assert_eq!(
        note,
        MessageContent {
            delivery_notice: None,
            from: Address::System,
            to: Address::Agent(id("lead")),
            kind: MessageKind::Note,
            body: "agent p/worker is at 60% of its context (line 60%). \
                   Hand it over: atm agent handover p/worker"
                .into(),
            attachments: Vec::new(),
            task: None,
        }
    );
    // The line is the policy's, passed in: nothing here knows 60.
    let seventy = HandoverLimits {
        context_line_percent: 70,
        ..limits
    };
    assert_eq!(context_note(&state, &s1, 69.99, &seventy, now), None);
    let (_, note) = context_note(&state, &s1, 70.4, &seventy, now).unwrap();
    assert!(note.body.contains("is at 70% of its context (line 70%)"));
    // The lead has no manager: its note goes to the owner.
    let lead = SessionId::new("lead-1");
    let (agent, note) = context_note(&state, &lead, 65.0, &limits, now).unwrap();
    assert_eq!((agent, note.to), (id("lead"), Address::Owner));
    // A session no ready agent is bound to gets no note.
    let unbound = SessionId::new("other");
    assert_eq!(context_note(&state, &unbound, 90.0, &limits, now), None);
    // An agent that is not ready gets none, though it keeps its session.
    rig.run(OrgCommand::Stuck {
        agent_id: id("worker"),
        reason: "a dialog nobody answered".into(),
    })
    .await;
    let stuck = rig.state().await;
    let worker = stuck.agent(&id("worker")).unwrap();
    assert_eq!(worker.session_id.as_ref(), Some(&s1));
    assert_eq!(context_note(&stuck, &s1, 90.0, &limits, now), None);
}

#[tokio::test]
async fn one_note_per_session_when_a_turn_ends_at_the_line() {
    let mut rig = Rig::new().await;
    spawn_handover_watch(rig.service.clone(), rig.cancel.clone());
    // Below the line: no note.
    rig.share("s1", 599).await;
    rig.turn("s1").await;
    // At the line: the first note.
    rig.share("s1", 600).await;
    rig.turn("s1").await;
    // The same session again, higher: no second note.
    rig.share("s1", 700).await;
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
    // The note is for the agent bound to the session when the watcher looks,
    // so the worker's first note is awaited before the worker moves on.
    let first = rig.next("msg.sent").await;
    // The worker in a new session (after a clear): a new note.
    rig.bind("worker", "s2", "%1").await;
    rig.share("s2", 650).await;
    rig.turn("s2").await;
    // A share that is reported after the turn's end still counts, and the
    // lead's note goes to the owner.
    rig.turn("lead-1").await;
    rig.share("lead-1", 600).await;

    // The watcher reads these in order, so the three notes below, with no
    // other between them, are all that the sequence above produced.
    let mut notes = Vec::new();
    for note in [
        first,
        rig.next("msg.sent").await,
        rig.next("msg.sent").await,
    ] {
        notes.push((
            note.body["to"].as_str().unwrap().to_owned(),
            note.body["body"].as_str().unwrap().to_owned(),
        ));
    }
    let text = |agent: &str, share: u32| {
        format!(
            "agent p/{agent} is at {share}% of its context (line 60%). \
             Hand it over: atm agent handover p/{agent}"
        )
    };
    assert_eq!(
        notes,
        [
            ("p/lead".to_owned(), text("worker", 60)),
            ("p/lead".to_owned(), text("worker", 65)),
            ("owner".to_owned(), text("lead", 60)),
        ]
    );
}

fn line_note(agent: &str, share: u32) -> String {
    format!(
        "agent p/{agent} is at {share}% of its context (line 60%). \
         Hand it over: atm agent handover p/{agent}"
    )
}

/// The next `count` notes, as (to, body). The watcher reads registry events
/// in order, so these are the first notes the steps before them produced.
async fn notes(rig: &mut Rig, count: usize) -> Vec<(String, String)> {
    let mut notes = Vec::new();
    for _ in 0..count {
        let note = rig.next("msg.sent").await;
        notes.push((
            note.body["to"].as_str().unwrap().to_owned(),
            note.body["body"].as_str().unwrap().to_owned(),
        ));
    }
    notes
}

#[tokio::test]
async fn a_share_at_the_line_is_not_noted_while_the_turn_runs() {
    let mut rig = Rig::new().await;
    spawn_handover_watch(rig.service.clone(), rig.cancel.clone());
    // The worker has ended a turn, then works again and crosses the line
    // inside that turn.
    rig.turn("s1").await;
    rig.report("s1", LifecycleEvent::WorkingStart).await;
    rig.share("s1", 700).await;
    // Meanwhile the lead's turn ends above the line: its note comes first,
    // which shows that the worker's share was seen and not noted.
    rig.turn("lead-1").await;
    rig.share("lead-1", 600).await;
    // The worker's turn ends: now it is noted.
    rig.report("s1", LifecycleEvent::Idle).await;
    assert_eq!(
        notes(&mut rig, 2).await,
        [
            ("owner".to_owned(), line_note("lead", 60)),
            ("p/lead".to_owned(), line_note("worker", 70)),
        ]
    );
}

#[tokio::test]
async fn an_idle_session_is_not_noted_before_a_turn_has_ended_in_it() {
    let mut rig = Rig::new().await;
    spawn_handover_watch(rig.service.clone(), rig.cancel.clone());
    // The worker's session is first seen idle and above the line, as after
    // a daemon restart: updates arrive, and no turn has ended.
    rig.share("s1", 700).await;
    rig.share("s1", 710).await;
    // The lead's note comes first, which shows that those updates were seen
    // and not noted.
    rig.turn("lead-1").await;
    rig.share("lead-1", 600).await;
    // A turn ends in the worker's session: now it is noted.
    rig.turn("s1").await;
    assert_eq!(
        notes(&mut rig, 2).await,
        [
            ("owner".to_owned(), line_note("lead", 60)),
            ("p/lead".to_owned(), line_note("worker", 71)),
        ]
    );
}

// Break caught: preflight itself sends, before a retained owner exists.
#[tokio::test]
async fn admission_preflight_leaves_both_writes_to_the_retained_owner() {
    let mut rig = Rig::new().await;
    let admitted = rig
        .service
        .admit(
            HandoverParams {
                agent: id("worker"),
                timeout_ms: 600_000,
            },
            owner(),
            Box::pin(std::future::pending()),
            None,
        )
        .await
        .ok()
        .unwrap();
    assert!(rig.kinds().is_empty(), "preflight queued a save request");
    drop(admitted);
    assert!(rig.state().await.messages().next().is_none());
}

// Break caught: caller cancellation drops either admission write or releases Running.
#[tokio::test]
async fn admission_writes_survive_caller_loss_between_queue_and_audit() {
    for caller in [owner(), from_pane("%1")] {
        let mut rig = Rig::new().await;
        let mut gate = rig.gate(AdmissionWrite::Queue);
        let (_deadline, call) = rig.start("worker", caller);
        let (reply, committed) = commit_held(&rig.org, gate.take().await).await;
        let request = MessageId::new(committed.as_ref().unwrap().events[0].seq);
        call.abort();
        assert!(call.await.unwrap_err().is_cancelled());
        assert!(
            !reply.is_closed(),
            "caller loss dropped the admission write's waiter"
        );
        let (_deadline, competing) = rig.start("worker", owner());
        assert_eq!(refusal(answer(competing).await).0, error_code::CONFLICT);
        assert!(rig.cleared.try_recv().is_err());
        assert!(
            reply.send(committed).is_ok(),
            "retained queue reply was lost"
        );
        assert_eq!(rig.requested().await, request);
        rig.deliver(request, "s1", "%1").await;
        std::fs::write(rig.memory("worker"), "saved after disconnect").unwrap();
        rig.report("s1", LifecycleEvent::Idle).await;
        assert_eq!(
            tokio::time::timeout(GUARD, rig.cleared.recv())
                .await
                .unwrap(),
            Some(id("worker"))
        );
        gate.finish(&mut rig).await;
        assert!(rig.cleared.try_recv().is_err(), "cleared more than once");
        let note = rig.next("msg.sent").await;
        assert_eq!(note.body["to"], "p/lead");
        assert!(note.body["body"]
            .as_str()
            .unwrap()
            .contains("clear confirmed; resume not confirmed"));
    }
}

// Break caught: an audit error hides an existing request, clears anyway, or is reported twice.
#[tokio::test]
async fn admission_failed_audit_reports_the_existing_request_without_clearing() {
    for own in [false, true] {
        for lost in [false, true] {
            let mut rig = Rig::new().await;
            let mut gate = rig.gate(AdmissionWrite::Audit);
            let caller = if own { from_pane("%1") } else { owner() };
            let (_deadline, call) = rig.start("worker", caller);
            let super::Request::Execute { command, reply, .. } = gate.take().await else {
                panic!("not a write")
            };
            let OrgCommand::SaveRequested {
                message_id: request,
                ..
            } = command.as_ref()
            else {
                panic!("not a save audit")
            };
            let request = *request;
            assert!(!call.is_finished(), "Started was returned before the audit");
            // Even a completed save cannot clear without confirmed admission.
            rig.deliver(request, "s1", "%1").await;
            std::fs::write(rig.memory("worker"), "saved while audit held").unwrap();
            rig.report("s1", LifecycleEvent::Idle).await;
            rig.kinds();
            assert!(rig.cleared.try_recv().is_err());
            let waiting = if lost {
                call.abort();
                assert!(call.await.unwrap_err().is_cancelled());
                None
            } else {
                Some(call)
            };
            assert!(!reply.is_closed(), "caller loss dropped the audit waiter");
            assert!(reply.send(Err(super::OrgServiceError::Timeout)).is_ok());
            if let Some(waiting) = waiting {
                assert_eq!(
                    refusal(answer(waiting).await),
                    (error_code::TIMEOUT.into(), audit_failure(request))
                );
            } else {
                let note = rig.next("msg.sent").await;
                assert_eq!(note.body["to"], "p/lead");
                assert_eq!(
                    note.body["body"],
                    format!(
                        "handover of p/worker did not complete: {}",
                        audit_failure(request)
                    )
                );
            }
            gate.finish(&mut rig).await;
            let state = rig.state().await;
            assert_eq!(
                state.message(&request).unwrap().status,
                DeliveryStatus::Delivered
            );
            assert_eq!(
                state
                    .messages()
                    .filter(|m| m.content.body.starts_with("handover of "))
                    .count(),
                usize::from(lost)
            );
            assert!(
                rig.cleared.try_recv().is_err(),
                "failed admission cleared the saved session"
            );
            assert!(!rig
                .kinds()
                .iter()
                .any(|kind| kind == "agent.saved" || kind == "agent.clear_requested"));
        }
    }
}

// Break caught: failed SaveRequested discards or duplicates an undelivered request on retry.
#[tokio::test]
async fn admission_failed_audit_keeps_an_open_request_for_retry() {
    let mut rig = Rig::new().await;
    let mut open = None;
    // Fail once on a new request and once on the reused request.
    for _ in 0..2 {
        let mut gate = rig.gate(AdmissionWrite::Audit);
        let (_deadline, call) = rig.start("worker", owner());
        let super::Request::Execute { command, reply, .. } = gate.take().await else {
            panic!("not a write")
        };
        let OrgCommand::SaveRequested {
            message_id: request,
            ..
        } = command.as_ref()
        else {
            panic!("not a save audit")
        };
        let request = *request;
        assert_eq!(
            *open.get_or_insert(request),
            request,
            "retry sent another request"
        );
        reply.send(Err(super::OrgServiceError::Timeout)).unwrap();
        assert_eq!(refusal(answer(call).await).1, audit_failure(request));
        assert_eq!(
            rig.state().await.message(&request).unwrap().status,
            DeliveryStatus::Queued
        );
        assert!(rig.cleared.try_recv().is_err());
        gate.finish(&mut rig).await;
    }
    let request = open.unwrap();
    let (_deadline, retry) = rig.start("worker", owner());
    assert_eq!(rig.requested().await, request);
    rig.deliver(request, "s1", "%1").await;
    std::fs::write(rig.memory("worker"), "retry saved").unwrap();
    rig.report("s1", LifecycleEvent::Idle).await;
    assert_eq!(answer(retry).await.unwrap().message_id, request);
    assert_eq!(
        rig.state().await.messages().count(),
        1,
        "retry duplicated the save request"
    );
}

// Break caught: a lost/empty queue receipt claims no message exists or permits a clear.
#[tokio::test]
async fn admission_unconfirmed_queue_is_reported_without_claiming_nothing_was_sent() {
    for empty in [false, true] {
        let mut rig = Rig::new().await;
        let mut gate = rig.gate(AdmissionWrite::Queue);
        let (_deadline, call) = rig.start("worker", owner());
        let (reply, committed) = commit_held(&rig.org, gate.take().await).await;
        let request = MessageId::new(committed.as_ref().unwrap().events[0].seq);
        let (result, cause, code) = if empty {
            (
                Ok(super::TaskCommit {
                    events: Vec::new(),
                    before: None,
                }),
                "agent operation failed: the save request committed no event",
                error_code::INTERNAL,
            )
        } else {
            (
                Err(super::OrgServiceError::Timeout),
                "organisation request timed out; accepted work may still complete",
                error_code::TIMEOUT,
            )
        };
        reply.send(result).unwrap();
        assert_eq!(refusal(answer(call).await), (code.into(), format!(
            "could not confirm the save request for p/worker: {cause}; a request may be queued \
             or already delivered; nothing was cleared; inspect `atm inbox --for p/worker` before retrying"
        )));
        assert_eq!(
            rig.state().await.message(&request).unwrap().status,
            DeliveryStatus::Queued
        );
        assert!(rig.cleared.try_recv().is_err());
        assert!(!rig.kinds().contains(&"agent.save_requested".into()));
    }
}

// The handover watcher's tests use this file's rig.
#[path = "agent_auto_handover_tests.rs"]
mod auto;
