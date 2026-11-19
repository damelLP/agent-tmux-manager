// `agent.clear` over the real control actor and a scripted tmux client.
//
// Included after `agent_lifecycle_tests.rs`, into the same module: no module
// docs, and that file's helpers are in scope. Clear timing uses a paused
// clock with in-memory I/O; the saved-handover cases pause after the save's
// file work. Tests synchronise on committed events, progress events and
// the order of scripted reads.
//
// Screens are the adapters' real captures (each adapter's fixture README
// records its `Captured:` line) unless marked synthetic.

use atm_core::LifecycleEvent;

macro_rules! fixture {
    ($adapter:literal, $name:literal) => {
        include_str!(concat!(
            "../../../atm-",
            $adapter,
            "-adapter/tests/fixtures/screen_",
            $name,
            ".txt"
        ))
    };
}

const CLAUDE_CONVERSATION: &str =
    fixture!("claude", "control_p1_B-baseline-conversation-120x40_2.1.325");
/// `/clear` pasted into that conversation's editor, its completion menu open.
const CLAUDE_STAGED: &str = fixture!("claude", "control_p1_B-clear-001-120x40_2.1.325");
const CLAUDE_CLEARED: &str = fixture!("claude", "control_p1_B-clear-007-120x40_2.1.325");
/// Another draft in the editor.
const CLAUDE_DRAFT: &str = fixture!("claude", "styled_p1_B-auto-pasted-120x40_2.1.325");
/// A turn running.
const CLAUDE_RUNNING: &str = fixture!("claude", "control_p1_B-auto-submitted-120x40_2.1.325");
/// A permission dialog the adapter does not read.
const CLAUDE_MODAL: &str = fixture!("claude", "control_p1_C3-dialog-120x40_2.1.325");
/// The folder trust dialog: a dialog the adapter does read.
const CLAUDE_TRUST: &str = fixture!("claude", "control_p1_A120-default-120x40_2.1.325");
const PI_CONVERSATION: &str = fixture!("pi", "control_p1_B-conversation-120x40_1.5.0");
/// `/new` pasted into pi's editor; a pasted command draws no completion list.
const PI_STAGED: &str = fixture!("pi", "p5_120x40-staged_1.5.0.styled");
const PI_RETRY: &str = fixture!("pi", "styled_p1_R-retry-held-120x40_1.5.0");
const CODEX_CONVERSATION: &str =
    fixture!("codex", "control_p1_b12-completed-before-new-100x32_0.167.0");
/// `/new` pasted, with Codex's one-row completion popup.
const CODEX_POPUP: &str = fixture!("codex", "p5_100x40-popup_0.167.0.styled");
/// After `/new`: Codex asks which worktree to use, the current one selected.
const CODEX_WORKTREE: &str =
    fixture!("codex", "control_p1_b31-nodaemon-clear-dialog-100x32_0.167.0");
const CODEX_FRESH: &str = fixture!("codex", "styled_p1_b14-fresh-baseline-32-100x32_0.167.0");
const CODEX_P6_CONVERSATION: &str =
    fixture!("codex", "p6_B-120x40-finished-P6_OLD_ANSWER_B-120x40_0.167.0.styled");
const CODEX_P6_POPUP: &str = fixture!("codex", "p6_B-120x40-before-enter_0.167.0.styled");
const CODEX_P6_80_CONVERSATION: &str =
    fixture!("codex", "p6_B-80x24-finished-P6_OLD_ANSWER_B-80x24_0.167.0.styled");
const CODEX_P6_80_POPUP: &str = fixture!("codex", "p6_B-80x24-before-enter_0.167.0.styled");
const CODEX_P6_80_AFTER_ENTER: [&str; 2] = [
    fixture!("codex", "p6_B-80x24-after-enter-poll-000_0.167.0.styled"),
    fixture!("codex", "p6_B-80x24-after-enter-poll-001_0.167.0.styled"),
];
const CODEX_P6_AFTER_ENTER: [&str; 5] = [
    fixture!("codex", "p6_B-120x40-after-enter-poll-000_0.167.0.styled"),
    fixture!("codex", "p6_B-120x40-after-enter-poll-001_0.167.0.styled"),
    fixture!("codex", "p6_B-120x40-after-enter-poll-002_0.167.0.styled"),
    fixture!("codex", "p6_B-120x40-after-enter-poll-003_0.167.0.styled"),
    fixture!("codex", "p6_B-120x40-after-enter-poll-004_0.167.0.styled"),
];

const NOTHING_SENT: &str = "nothing was sent to %1";
const NOT_SUBMITTED: &str = "the clear command was typed and not submitted";

/// What the registry knows of the agent's session before the clear.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Old {
    /// The agent is Ready without a session, and its pane has no row.
    Unbound,
    /// Bound to `old`, which the harness process announced. A new session of
    /// that process replaces the row, as a clear in the same process does.
    SameProcess,
    /// Bound to `old`, a row without a process: it stays until it is removed.
    Lingering,
}

/// `p/lead`, Ready on %1 with one harness in the pane's foreground and the
/// pane's baseline recorded, as after an adopt.
struct Clearing {
    rig: Rig,
    agents: AgentHandle,
    events: broadcast::Receiver<crate::org::agents::ObservationProgress>,
    harness: Harness,
}

impl Clearing {
    async fn new(harness: Harness, old: Old) -> Self {
        Self::on(Rig::new().await, harness, old).await
    }

    async fn on(rig: Rig, harness: Harness, old: Old) -> Self {
        let name = harness.short_tag();
        let bound = old != Old::Unbound;
        rig.agent_of("lead", name, Some("%1"), bound.then_some("old"))
            .await;
        match old {
            Old::Unbound => {}
            Old::SameProcess => start_session(&rig, harness, "old", Some("startup")).await,
            Old::Lingering => rig.row_of("old", "%1", false, harness).await,
        }
        if bound {
            // The restart check of a bound agent reads its screen once.
            let idle = match harness {
                Harness::ClaudeCode => CLAUDE_CONVERSATION,
                Harness::Codex => CODEX_CONVERSATION,
                _ => PI_CONVERSATION,
            };
            rig.frame_of("%1", "p/lead", body(idle), name);
        } else {
            rig.inspect_only("%1", "p/lead");
        }
        let (agents, _) = rig.start();
        agents.reconciled().await.unwrap();
        assert_eq!(rig.client.pending_reads(&pane("%1")).unwrap(), 0);
        assert_eq!(rig.state("lead").await.status, AgentStatus::Ready);
        agents.record_baseline(
            lead(),
            "%1",
            PaneBaseline::adopted(100)
                .with_harness(name.into())
                .unwrap(),
        );
        let events = agents.observation_events();
        Self {
            rig,
            agents,
            events,
            harness,
        }
    }

    /// One capture of the pane as a clear needs it: the agent's tag, the
    /// harness in front, no client focused on it.
    fn capture(&self, text: &str) -> PaneCapture {
        let text = body(text);
        let mut inspection = Rig::inspection("%1", "p/lead", false)
            .with_foreground_process(None, Some(self.harness.short_tag().into()));
        inspection.pane.height = text.lines().count() as u16;
        PaneCapture::new(text.into(), inspection, CaptureFormat::Ansi).with_focus(Some(false))
    }

    /// Script the next capture or guard of the pane.
    fn frame(&self, text: &str) {
        self.rig.client.script_capture(&self.capture(text)).unwrap();
    }

    fn frame_with(&self, text: &str, change: impl FnOnce(&mut PaneCapture)) {
        let mut capture = self.capture(text);
        change(&mut capture);
        self.rig.client.script_capture(&capture).unwrap();
    }

    /// Script the frames of a clear up to and including its Enter: the
    /// precheck, the paste guard, the staged capture and the Enter guard.
    fn frames_through_enter(&self, conversation: &str, staged: &str) {
        self.frame(conversation);
        self.frame(conversation);
        self.frame(staged);
        self.frame(staged);
    }

    /// Script the inspection of one observation. Registry events of the pane
    /// start observations; they run when the clear has released the agent.
    fn follow_up(&self) {
        self.rig.inspect_only("%1", "p/lead");
    }

    fn start(&self) -> tokio::task::JoinHandle<Result<ClearResult, AgentError>> {
        let agents = self.agents.clone();
        tokio::spawn(async move {
            agents
                .clear(ClearParams {
                    agent: lead(),
                    timeout_ms: 30_000,
                })
                .await
        })
    }

    async fn clear(&self) -> Result<ClearResult, AgentError> {
        self.start().await.unwrap()
    }

    /// The flow has submitted its command and waits for a session to arrive.
    async fn awaiting_session(&mut self) {
        progress(&mut self.events, "clear_awaiting_session", Some("p/lead")).await;
    }

    /// The harness process announces a session on the pane.
    async fn session_start(&self, id: &str, source: Option<&str>) {
        start_session(&self.rig, self.harness, id, source).await;
    }

    /// Every observation the events so far started has ended, successfully:
    /// the observer has nothing running and nothing queued.
    async fn settled(&mut self) {
        self.events = self.agents.observation_events();
        self.rig
            .events
            .send(crate::registry::SessionEvent::Removed {
                session_id: SessionId::new("unrelated"),
                reason: crate::registry::RemovalReason::Explicit,
            })
            .unwrap();
        progress(&mut self.events, "idle", None).await;
    }

    /// What was sent into the pane, in order.
    fn sent(&self) -> Vec<String> {
        self.rig
            .client
            .mock()
            .calls()
            .into_iter()
            .filter_map(|call| match call {
                MockCall::PasteText { pane, text } => Some(format!("paste {pane} {text}")),
                MockCall::SendKeys { pane, keys } => Some(format!("keys {pane} {keys}")),
                _ => None,
            })
            .collect()
    }

    fn unread(&self) -> usize {
        self.rig.client.pending_reads(&pane("%1")).unwrap()
    }

    async fn agent(&self) -> atm_org::Agent {
        self.rig.state("lead").await
    }

    /// The agent is Stuck for `reason`, and its owner was told so once.
    async fn assert_stuck(&self, reason: &str) {
        let agent = self.agent().await;
        assert_eq!(agent.status, AgentStatus::Stuck, "{agent:?}");
        let expected = format!("clear did not complete: {reason}; inspect %1");
        assert_eq!(agent.reason.as_deref(), Some(expected.as_str()));
        let state = self.rig.org.snapshot().await.unwrap();
        let notes = state
            .messages()
            .filter(|message| {
                message.content.to == atm_org::Address::Owner
                    && message.content.body == format!("agent p/lead: {expected}")
            })
            .count();
        assert_eq!(notes, 1);
        // Only an operation that ends Ready queues a start line.
        assert!(
            state
                .messages()
                .all(|message| !crate::org::start_line::is_start_line(&message.content)),
            "a start line after an operation that did not end Ready"
        );
    }

    /// The agent is Stuck for a reason that contains `part`.
    async fn assert_stuck_with(&self, part: &str) {
        let agent = self.agent().await;
        assert_eq!(agent.status, AgentStatus::Stuck, "{agent:?}");
        let reason = agent.reason.unwrap_or_default();
        assert!(reason.contains(part), "{reason}");
    }
}

/// A SessionStart of `id` from the harness process on %1. The registry keys
/// rows by process: a new id from the same process replaces its row.
///
/// The process id is one no process can have. For a real one the registry
/// reads that process's directory from `/proc` and looks its git repository
/// up on an OS thread, and these tests run on a paused clock, which goes only
/// with in-memory work: on a loaded host virtual time ran past a 5 s budget
/// before the real lookup answered.
async fn start_session(rig: &Rig, harness: Harness, id: &str, source: Option<&str>) {
    rig.registry
        .apply_lifecycle_event(
            SessionId::new(id),
            LifecycleEvent::SessionStart {
                source: source.map(Into::into),
            },
            harness,
            Some(u32::MAX - 1),
            Some("%1".into()),
        )
        .await
        .unwrap();
}

/// (harness, its command, a conversation, the command staged, the editor after)
const ANNOUNCING: [(Harness, &str, &str, &str, &str); 2] = [
    (
        Harness::ClaudeCode,
        "/clear",
        CLAUDE_CONVERSATION,
        CLAUDE_STAGED,
        CLAUDE_CLEARED,
    ),
    (Harness::Pi, "/new", PI_CONVERSATION, PI_STAGED, PI_PADDED),
];

// Break caught: classifying a recoverable screen error after committing
// Clear, or following a changed target to a perfectly safe replacement.
#[tokio::test(start_paused = true)]
async fn handover_clear_rechecks_identity_and_blocks_without_input() {
    use crate::org::agent_lifecycle::{ClearAttempt, SavedSeat};
    use tokio::time::Instant;
    let c = Clearing::new(Harness::ClaudeCode, Old::Lingering).await;
    let mut committed = c.rig.org.committed.subscribe();
    let target = SavedSeat { agent: lead(), pane: pane("%1"), session: "old".into() };
    for text in [CLAUDE_DRAFT, CLAUDE_RUNNING, CLAUDE_MODAL] {
        c.frame(text);
        let result = c.agents.clear_handover(target.clone(), None, Instant::now() + std::time::Duration::from_secs(120)).await;
        assert!(matches!(result, Ok(ClearAttempt::Blocked("the editor is not empty and idle"))), "{result:?}");
        let row = c.agent().await;
        assert_eq!(row.status, AgentStatus::Ready);
        assert_eq!(row.session_id, Some("old".into()));
        assert!(committed_kinds(&mut committed).is_empty());
        assert!(c.sent().is_empty());
    }
    // An expired read-only preflight cannot admit Clear.
    let result = c.agents.clear_handover(target.clone(), Some(Instant::now()), Instant::now() + std::time::Duration::from_secs(120)).await;
    assert!(matches!(result, Ok(ClearAttempt::Expired)), "{result:?}");
    assert!(committed_kinds(&mut committed).is_empty());
    // The reservation cannot clear a rebind even with the same pane ID.
    c.rig.command(OrgCommand::SessionBound { agent_id: lead(), session_id: "replacement".into() }).await;
    let result = c.agents.clear_handover(target, None, Instant::now() + std::time::Duration::from_secs(120)).await;
    assert!(matches!(result, Err(AgentError::Refused(reason)) if reason.contains("no longer has its saved session old")));
    assert_eq!(committed_kinds(&mut committed), ["agent.session_bound"]);
    assert!(c.sent().is_empty());
}

// Break caught: the Clear write lets a retry lose the identity that passed
// preflight; a fresh editor alone cannot authorize pasting into a new session.
#[tokio::test(start_paused = true)]
async fn handover_clear_does_not_paste_into_replacement() {
    use crate::org::agent_lifecycle::{ClearAttempt, SavedSeat};
    use tokio::time::Instant;
    for replace in [true, false] {
        let (gate, mut entered) = mpsc::channel(1);
        let rig = Rig::gated(Some(("agent.clear_requested", gate))).await;
        let mut c = Clearing::on(rig, Harness::ClaudeCode, Old::SameProcess).await;
        let target = SavedSeat { agent: lead(), pane: pane("%1"), session: "old".into() };
        let finish_by = Instant::now() + std::time::Duration::from_secs(120);
        c.frame(CLAUDE_DRAFT);
        assert!(matches!(c.agents.clear_handover(target.clone(), None, finish_by).await, Ok(ClearAttempt::Blocked(_))));
        c.frames_through_enter(CLAUDE_CONVERSATION, CLAUDE_STAGED);
        c.frame(CLAUDE_CLEARED);
        c.follow_up();
        c.follow_up();
        let agents = c.agents.clone();
        let mut call = tokio::spawn(async move {
            agents.clear_handover(target, Some(Instant::now() + std::time::Duration::from_secs(20)), finish_by).await
        });
        let release = entered.recv().await.unwrap();
        assert!(c.sent().is_empty());
        if replace {
            c.session_start("replacement", Some("resume")).await;
            assert!(c.rig.registry.get_session("old".into()).await.is_none());
        }
        release.send(()).unwrap();
        let result = tokio::select! {
            result = &mut call => result.unwrap(),
            () = c.awaiting_session() => {
                c.session_start("after-daemon-clear", Some("clear")).await;
                call.await.unwrap()
            }
        };
        if replace {
            assert!(c.sent().is_empty(), "replacement received input: {:?}", c.sent());
            assert!(result.is_err(), "{result:?}");
        } else {
            assert_eq!(c.sent(), ["paste %1 /clear", "keys %1 Enter"]);
            assert!(matches!(result, Ok(ClearAttempt::Cleared(clear)) if clear.result.session_id == Some("after-daemon-clear".into())));
        }
    }
}

// Break caught: a retry pasted safely, but a replacement starts after the
// row read for Enter. A staged-looking frame must not authorize submission.
#[tokio::test(start_paused = true)]
async fn handover_clear_does_not_submit_in_replacement() {
    use crate::org::agent_lifecycle::SavedSeat;
    use std::time::Duration;
    use tokio::time::Instant;
    let mut c = Clearing::new(Harness::ClaudeCode, Old::SameProcess).await;
    c.frames_through_enter(CLAUDE_CONVERSATION, CLAUDE_STAGED);
    c.frame(CLAUDE_CLEARED);
    c.follow_up();
    c.follow_up();
    let agents = c.agents.clone();
    let mut call = tokio::spawn(async move {
        let target = SavedSeat { agent: lead(), pane: pane("%1"), session: "old".into() };
        agents.clear_handover(target, Some(Instant::now() + Duration::from_secs(20)), Instant::now() + Duration::from_secs(120)).await
    });
    progress(&mut c.events, "clear_submitting", Some("p/lead")).await;
    assert_eq!(c.sent(), ["paste %1 /clear"]);
    // The actor paces its capture after the staged frame. The registry
    // round trip is in-memory, so it finishes before virtual time advances.
    c.session_start("replacement", Some("resume")).await;
    assert!(c.rig.registry.get_session("old".into()).await.is_none());
    let result = tokio::select! {
        result = &mut call => result.unwrap(),
        () = c.awaiting_session() => {
            c.session_start("after-daemon-clear", Some("clear")).await;
            call.await.unwrap()
        }
    };
    assert_eq!(c.sent(), ["paste %1 /clear"], "replacement received Enter");
    assert!(result.is_err(), "{result:?}");
}

mod actor_closure {
    use super::*;
    use crate::control::reservation::Quarantine;
    use crate::eventlog::{Envelope, EventLogError};
    use crate::org::agent_handover::{Answer, Clearer, HandoverParams, Handovers, NoteOrigin};
    use crate::org::agent_lifecycle::{ClearAttempt, SavedSeat};
    use crate::org::{actor, OrgHandle};
    use atm_org::MessageId;
    use std::{
        path::Path,
        sync::{Arc, Mutex},
        time::Duration,
    };
    use tokio::sync::oneshot;
    use tokio::task::JoinHandle;

    const GUARD: Duration = Duration::from_secs(30);
    const REGISTRY_STOPPED: &str =
        "agent service unavailable: session registry: response channel closed";
    const WRITER_STOPPED: &str = "organisation service unavailable: organisation actor stopped";

    struct Actor {
        cancel: CancellationToken,
        task: JoinHandle<()>,
    }

    impl Actor {
        async fn stop(self) {
            self.cancel.cancel();
            self.task.await.unwrap();
        }
    }

    // The actual request actors, with separate stop tokens. The writer's
    // append sink and tmux are in-memory so shutdown and retry can be fenced
    // on paused time without depending on a filesystem or a tmux process.
    async fn rig(root: &Path, lose_append: Option<&'static str>) -> (Rig, Actor, Actor) {
        let cancel = CancellationToken::new();
        let writer_cancel = cancel.child_token();
        let (requests, receive) = mpsc::channel(64);
        let (health, status) = watch::channel(Ok(0));
        let (committed, _) = broadcast::channel(1024);
        let mut seq = 0;
        let writer = tokio::spawn(actor::run(
            Default::default(),
            receive,
            health,
            committed.clone(),
            writer_cancel.clone(),
            move |event: Envelope| {
                seq += 1;
                std::future::ready(if lose_append == Some(event.kind.as_str()) {
                    Err(EventLogError::TimeoutAfterHandover)
                } else {
                    Ok(seq)
                })
            },
        ));
        let org = OrgHandle {
            requests,
            health: status,
            committed,
            reads: None,
        };
        let registry_cancel = cancel.child_token();
        let (send, receive) = mpsc::channel(100);
        let (events, _) = broadcast::channel(2048);
        let registry_actor = RegistryActor::new(receive, events.clone());
        let registry_task = tokio::spawn({
            let stop = registry_cancel.clone();
            async move {
                tokio::select! { biased; () = stop.cancelled() => {}, () = registry_actor.run() => {} }
            }
        });
        let registry = RegistryHandle::new(send, events.clone());
        let client = ScriptedTmuxClient::new();
        let (control, _) = spawn_control_with_org(
            Box::new(client.clone()),
            registry.clone(),
            org.clone(),
            cancel.clone(),
        );
        let rig = Rig {
            org,
            registry,
            events,
            client,
            control,
            cancel,
            proc: Arc::new(Mutex::new(Some(job_in_front()))),
            proc_after_enter: Default::default(),
            proc_script: Default::default(),
            spawn_env: Default::default(),
        };
        rig.command(OrgCommand::RegisterProject {
            project_id: "p".parse().unwrap(),
            root: root.to_str().unwrap().into(),
            move_root: false,
        })
        .await;
        (
            rig,
            Actor {
                cancel: writer_cancel,
                task: writer,
            },
            Actor {
                cancel: registry_cancel,
                task: registry_task,
            },
        )
    }

    #[derive(Debug, PartialEq, Eq)]
    enum Step {
        Clear { retry: bool, blocked: bool },
        Parked,
    }

    struct ObserveClear {
        agents: AgentHandle,
        steps: mpsc::UnboundedSender<Step>,
        first: Mutex<Option<oneshot::Receiver<()>>>,
    }

    impl Clearer for ObserveClear {
        fn available(&self) -> Answer<()> {
            self.agents.available()
        }

        fn clear(
            &self,
            target: SavedSeat,
            retry_before: Option<Instant>,
            finish_by: Instant,
        ) -> Answer<ClearAttempt> {
            let work = Clearer::clear(&self.agents, target, retry_before, finish_by);
            let steps = self.steps.clone();
            let first = self.first.lock().unwrap().take();
            Box::pin(async move {
                let result = work.await;
                steps
                    .send(Step::Clear {
                        retry: retry_before.is_some(),
                        blocked: matches!(
                            result,
                            Ok(ClearAttempt::Blocked("the editor is not empty and idle"))
                        ),
                    })
                    .unwrap();
                if let Some(first) = first {
                    first.await.unwrap();
                }
                result
            })
        }

        fn clear_wait(&self, target: &SavedSeat, reason: Option<String>) {
            let parked = reason.is_some();
            self.agents.clear_wait(target, reason);
            if parked {
                self.steps.send(Step::Parked).unwrap();
            }
        }
    }

    async fn next_step(rx: &mut mpsc::UnboundedReceiver<Step>) -> Step {
        tokio::time::timeout(GUARD, rx.recv())
            .await
            .unwrap()
            .unwrap()
    }

    async fn quarantines(c: &Clearing) -> Vec<Quarantine> {
        c.rig
            .control
            .quarantines(Instant::now() + GUARD)
            .await
            .unwrap()
    }

    fn clear_quarantine() -> Vec<Quarantine> {
        vec![Quarantine {
            agent: lead(),
            pane: Some(pane("%1")),
            kind: ReservationKind::Clear,
        }]
    }

    #[derive(Clone, Copy)]
    enum Stop {
        Registry,
        Writer,
        Both,
    }

    // Break caught: a read-only retry that genuinely reached reservation
    // cleanup quarantines a pane because the org writer is unavailable.
    async fn parked_retry(stop: Stop) {
        let dir = tempfile::tempdir().unwrap();
        let memory = dir.path().join(".atm/memory");
        std::fs::create_dir_all(&memory).unwrap();
        let (rig, writer, registry) = rig(dir.path(), None).await;
        let c = Clearing::on(rig, Harness::ClaudeCode, Old::SameProcess).await;
        let mut committed = c.rig.org.committed.subscribe();
        c.frame(CLAUDE_DRAFT);
        let (steps, mut observations) = mpsc::unbounded_channel();
        let (release, first) = oneshot::channel();
        let agent_stop = c.agents.stop_token();
        let service = Handovers::new(
            c.rig.org.clone(),
            c.rig.registry.clone(),
            Arc::new(ObserveClear {
                agents: c.agents.clone(),
                steps,
                first: Mutex::new(Some(first)),
            }),
            agent_stop.clone(),
        );
        let (reply, answer) = oneshot::channel();
        let call = tokio::spawn(async move {
            let admitted = service
                .admit(
                    HandoverParams {
                        agent: lead(),
                        timeout_ms: 600_000,
                    },
                    CallContext {
                        client_id: CallerId("owner".into()),
                        from_pane: None,
                    },
                    Box::pin(std::future::pending()),
                    None,
                )
                .await
                .unwrap();
            service
                .until_stopped(service.finish_admitted(
                    &lead(),
                    admitted,
                    NoteOrigin::Command,
                    Some(reply),
                ))
                .await
                .unwrap();
        });
        let save: MessageId = tokio::time::timeout(GUARD, async {
            loop {
                let event = committed.recv().await.unwrap();
                if event.kind == "agent.save_requested" {
                    break serde_json::from_value(event.body["message_id"].clone()).unwrap();
                }
            }
        })
        .await
        .unwrap();
        c.rig
            .command(OrgCommand::DeliveryAttempted {
                message_id: save,
                attempt: 1,
                pane: "%1".into(),
            })
            .await;
        c.rig
            .registry
            .apply_lifecycle_event(
                "old".into(),
                LifecycleEvent::WorkingStart,
                Harness::ClaudeCode,
                None,
                None,
            )
            .await
            .unwrap();
        c.rig
            .command(OrgCommand::Delivered {
                message_id: save,
                session_id: "old".into(),
                confirmation: "prompt".into(),
            })
            .await;
        std::fs::write(memory.join("lead.md"), "saved state").unwrap();
        c.rig
            .registry
            .apply_lifecycle_event(
                "old".into(),
                LifecycleEvent::Idle,
                Harness::ClaudeCode,
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            next_step(&mut observations).await,
            Step::Clear {
                retry: false,
                blocked: true
            }
        );
        tokio::time::pause(); // no filesystem work between here and the retry
        let parked_at = Instant::now();
        release.send(()).unwrap();
        assert_eq!(next_step(&mut observations).await, Step::Parked);
        let writer = if matches!(stop, Stop::Writer | Stop::Both) {
            writer.stop().await;
            None
        } else {
            Some(writer)
        };
        let registry = if matches!(stop, Stop::Registry | Stop::Both) {
            registry.stop().await;
            None
        } else {
            Some(registry)
        };
        assert_eq!(
            Instant::now(),
            parked_at,
            "actors stopped before backoff elapsed"
        );
        assert_eq!(
            next_step(&mut observations).await,
            Step::Clear {
                retry: true,
                blocked: false
            }
        );
        assert!(
            Instant::now() > parked_at,
            "retry woke from backoff, not a row change"
        );
        tokio::time::resume(); // the registry-only outcome reads the role file
        let error = tokio::time::timeout(GUARD, answer)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err()
            .0;
        let reason = if writer.is_some() {
            REGISTRY_STOPPED
        } else {
            WRITER_STOPPED
        };
        assert_eq!(error.code, "unavailable");
        assert_eq!(
            error.message,
            format!("p/lead saved its state ({save}); {reason}")
        );
        tokio::time::timeout(GUARD, call).await.unwrap().unwrap();
        assert!(
            observations.try_recv().is_err(),
            "only one retry after a save"
        );
        assert!(!agent_stop.is_cancelled());
        assert!(
            c.sent().is_empty(),
            "the saved handover sent input: {:?}",
            c.sent()
        );
        assert_eq!(c.unread(), 0, "the first clear consumed the draft");
        let kinds = committed_kinds(&mut committed);
        if writer.is_some() {
            let state = c.rig.org.snapshot().await.unwrap();
            let notes: Vec<_> = state
                .messages()
                .filter(|message| message.id != save)
                .collect();
            assert_eq!(notes.len(), 1);
            assert_eq!(notes[0].content.to, atm_org::Address::Owner);
            assert_eq!(
                notes[0].content.body,
                format!("handover of p/lead did not complete: saved ({save}); {reason}.")
            );
            assert_eq!(
                kinds,
                [
                    "msg.delivery_attempted",
                    "msg.delivered",
                    "agent.saved",
                    "msg.sent"
                ]
            );
        } else {
            assert_eq!(
                kinds,
                ["msg.delivery_attempted", "msg.delivered", "agent.saved"]
            );
        }
        let quarantines = quarantines(&c).await;
        if matches!(stop, Stop::Writer) {
            let interrupt = c.rig.control.interrupt("%1".into()).await;
            assert!(
                quarantines.is_empty()
                    && matches!(&interrupt, Ok(pane) if pane == "%1")
                    && c.sent() == ["keys %1 C-c"],
                "quarantines: {quarantines:?}; owner interrupt: {interrupt:?}; sent: {:?}",
                c.sent()
            );
        } else {
            assert!(quarantines.is_empty(), "quarantines: {quarantines:?}");
        }
        for actor in [writer, registry].into_iter().flatten() {
            actor.stop().await;
        }
    }

    #[tokio::test]
    async fn handover_clear_does_not_quarantine_on_parked_registry_stop() {
        parked_retry(Stop::Registry).await;
    }

    #[tokio::test]
    async fn handover_clear_does_not_quarantine_on_parked_writer_stop() {
        parked_retry(Stop::Writer).await;
    }

    #[tokio::test]
    async fn handover_clear_does_not_quarantine_on_parked_registry_and_writer_stop() {
        parked_retry(Stop::Both).await;
    }

    #[tokio::test(start_paused = true)]
    async fn standalone_clear_quarantines_on_unknown_pre_clear_binding() {
        let dir = tempfile::tempdir().unwrap();
        let (rig, writer, registry) = rig(dir.path(), Some("agent.session_bound")).await;
        rig.row_of("old", "%1", false, Harness::ClaudeCode).await;
        let c = Clearing::on(rig, Harness::ClaudeCode, Old::Unbound).await;
        c.frame(CLAUDE_CONVERSATION);
        let error = c.clear().await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("append timed out after hand-over; outcome unknown"),
            "{error:?}"
        );
        assert_eq!(quarantines(&c).await, clear_quarantine());
        assert!(c.sent().is_empty());
        writer.stop().await;
        registry.stop().await;
    }

    #[tokio::test(start_paused = true)]
    async fn handover_clear_quarantines_on_unknown_clear_submission() {
        let dir = tempfile::tempdir().unwrap();
        let (rig, writer, registry) = rig(dir.path(), Some("agent.clear_requested")).await;
        let c = Clearing::on(rig, Harness::ClaudeCode, Old::SameProcess).await;
        c.frame(CLAUDE_CONVERSATION);
        let error = c
            .agents
            .clear_handover(
                SavedSeat {
                    agent: lead(),
                    pane: pane("%1"),
                    session: "old".into(),
                },
                Some(Instant::now() + Duration::from_secs(20)),
                Instant::now() + GUARD,
            )
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("append timed out after hand-over; outcome unknown"),
            "{error:?}"
        );
        assert_eq!(quarantines(&c).await, clear_quarantine());
        assert!(c.sent().is_empty());
        writer.stop().await;
        registry.stop().await;
    }
}

#[tokio::test(start_paused = true)]
async fn clear_types_submits_and_binds_the_new_session() {
    // Also missing_end_with_valid_start_succeeds (with a lingering old row no
    // end is ever reported) and changed_id_clear_after_adopt_without_pid (an
    // adopted pane's baseline; the lingering old row carries no process, and
    // nothing compares the new session's).
    for (harness, command, conversation, staged, cleared) in ANNOUNCING {
        for old in [Old::SameProcess, Old::Lingering] {
            let mut clearing = Clearing::new(harness, old).await;
            let mut committed = clearing.rig.org.committed.subscribe();
            clearing.frames_through_enter(conversation, staged);
            clearing.frame(cleared);
            clearing.follow_up();
            clearing.follow_up();
            let call = clearing.start();
            clearing.awaiting_session().await;
            // Submitted, and nothing is known yet: Clearing, unbound.
            assert_eq!(
                clearing.sent(),
                [format!("paste %1 {command}"), "keys %1 Enter".to_owned()],
            );
            let agent = clearing.agent().await;
            assert_eq!(agent.status, AgentStatus::Clearing);
            assert_eq!(agent.session_id, None);
            assert_eq!(agent.excluded_session_id, Some(SessionId::new("old")));
            clearing.session_start("new", Some("clear")).await;
            let result = call.await.unwrap().unwrap();
            assert_eq!(result.agent_id, lead());
            assert_eq!(result.session_id, Some(SessionId::new("new")));
            let agent = clearing.agent().await;
            assert_eq!(agent.status, AgentStatus::Ready);
            assert_eq!(agent.session_id, Some(SessionId::new("new")));
            // Make the start line visible before the new session is Ready.
            assert_eq!(
                committed_kinds(&mut committed),
                ["agent.clear_requested", "msg.sent", "agent.session_bound"]
            );
            // One paste, one Enter, nothing else, ever.
            assert_eq!(clearing.sent().len(), 2, "{harness:?} {old:?}");
            clearing.settled().await;
            assert_eq!(clearing.unread(), 0, "{harness:?} {old:?}");
            // The pane's baseline is the same harness process's: kept.
            assert_ne!(
                clearing.agents.baseline_of(&lead(), "%1"),
                PaneBaseline::unproven()
            );
        }
    }
}

#[tokio::test(start_paused = true)]
async fn event_before_editor_stays_clearing() {
    // The new session is announced while the editor still shows the command:
    // not done. Twin: the same with a safe editor on the next capture.
    for complete in [false, true] {
        let mut clearing = Clearing::new(Harness::ClaudeCode, Old::SameProcess).await;
        clearing.frames_through_enter(CLAUDE_CONVERSATION, CLAUDE_STAGED);
        clearing.frame(CLAUDE_STAGED);
        if complete {
            clearing.frame(CLAUDE_CLEARED);
        }
        let call = clearing.start();
        clearing.awaiting_session().await;
        clearing.session_start("new", Some("clear")).await;
        let result = call.await.unwrap();
        if complete {
            assert_eq!(result.unwrap().session_id, Some(SessionId::new("new")));
            assert_eq!(clearing.agent().await.status, AgentStatus::Ready);
        } else {
            // No further capture is scripted: the flow could not look again.
            assert!(result.is_err(), "{result:?}");
            assert_eq!(clearing.agent().await.session_id, None);
            clearing
                .assert_stuck("the editor is not empty and idle")
                .await;
        }
        assert_eq!(clearing.sent().len(), 2);
    }
}

#[tokio::test(start_paused = true)]
async fn editor_before_event_stays_clearing() {
    // Also readiness_timeout_without_any_prompt_becomes_stuck and
    // pi_repeated_notice_not_ack: the command was submitted and no session is
    // announced. Whatever the editor shows meanwhile (pi's shows the same
    // "New session started" notice as before the command) is never looked at,
    // and proves nothing, however long it lasts.
    for (harness, _, conversation, staged, cleared) in ANNOUNCING {
        let mut clearing = Clearing::new(harness, Old::SameProcess).await;
        clearing.frames_through_enter(conversation, staged);
        clearing.frame(cleared);
        let call = clearing.start();
        clearing.awaiting_session().await;
        assert_eq!(clearing.agent().await.status, AgentStatus::Clearing);
        // Nothing more happens: the call ends at its deadline (virtual time).
        let result = call.await.unwrap();
        assert_told_stuck(&result, &clearing.agent().await);
        clearing
            .assert_stuck("the harness has not announced a new session")
            .await;
        assert_eq!(clearing.sent().len(), 2, "the command is never sent again");
        assert_eq!(clearing.unread(), 3, "the empty editor was never read");
    }
}

#[tokio::test(start_paused = true)]
async fn status_only_new_row_does_not_complete() {
    // Also wrong_claude_source_rejected. A new row on the pane completes the
    // clear only once the registry accepted its start with the source "clear".
    let mut clearing = Clearing::new(Harness::ClaudeCode, Old::Lingering).await;
    clearing.frames_through_enter(CLAUDE_CONVERSATION, CLAUDE_STAGED);
    clearing.frame(CLAUDE_CLEARED);
    let call = clearing.start();
    clearing.awaiting_session().await;
    // A row no SessionStart was accepted for, as a status line creates it.
    clearing
        .rig
        .row_of("new", "%1", false, Harness::ClaudeCode)
        .await;
    clearing.awaiting_session().await;
    assert_eq!(clearing.agent().await.status, AgentStatus::Clearing);
    // A start of that session with another source. (No process is named: the
    // registry finds the row by its id.)
    let registry = clearing.rig.registry.clone();
    let start = |source: &'static str| {
        registry.apply_lifecycle_event(
            SessionId::new("new"),
            LifecycleEvent::SessionStart {
                source: Some(source.into()),
            },
            Harness::ClaudeCode,
            None,
            Some("%1".into()),
        )
    };
    start("startup").await.unwrap();
    clearing.awaiting_session().await;
    assert_eq!(clearing.agent().await.status, AgentStatus::Clearing);
    assert!(!call.is_finished());
    // The twin: the same session, started by the clear.
    start("clear").await.unwrap();
    let result = call.await.unwrap().unwrap();
    assert_eq!(result.session_id, Some(SessionId::new("new")));
    assert_eq!(
        clearing.agent().await.session_id,
        Some(SessionId::new("new"))
    );
    assert_eq!(clearing.sent().len(), 2);
}

#[tokio::test(start_paused = true)]
async fn old_removed_after_new_does_not_unbind() {
    let mut clearing = Clearing::new(Harness::ClaudeCode, Old::Lingering).await;
    clearing.frames_through_enter(CLAUDE_CONVERSATION, CLAUDE_STAGED);
    clearing.frame(CLAUDE_CLEARED);
    clearing.follow_up();
    clearing.follow_up();
    let call = clearing.start();
    clearing.awaiting_session().await;
    clearing.session_start("new", Some("clear")).await;
    call.await.unwrap().unwrap();
    clearing.settled().await;
    assert_eq!(clearing.unread(), 0);
    // The old session's row leaves only now. Nothing about it concerns the
    // agent any more: no observation, no read, no change.
    clearing
        .rig
        .registry
        .remove(SessionId::new("old"))
        .await
        .unwrap();
    clearing.settled().await;
    assert_eq!(clearing.unread(), 0);
    let agent = clearing.agent().await;
    assert_eq!(agent.status, AgentStatus::Ready);
    assert_eq!(agent.session_id, Some(SessionId::new("new")));
}

#[tokio::test(start_paused = true)]
async fn pi_retry_empty_idle_not_ready() {
    // The new session is there and pi shows an empty editor under its retry
    // notice: a turn is running. Ready only on the idle editor after it.
    for complete in [false, true] {
        let mut clearing = Clearing::new(Harness::Pi, Old::SameProcess).await;
        clearing.frames_through_enter(PI_CONVERSATION, PI_STAGED);
        clearing.frame(PI_RETRY);
        if complete {
            clearing.frame(PI_PADDED);
        }
        let call = clearing.start();
        clearing.awaiting_session().await;
        clearing.session_start("new", Some("new")).await;
        let result = call.await.unwrap();
        if complete {
            assert_eq!(result.unwrap().session_id, Some(SessionId::new("new")));
        } else {
            assert!(result.is_err(), "{result:?}");
            clearing
                .assert_stuck("the editor is not empty and idle")
                .await;
        }
    }
}

#[tokio::test(start_paused = true)]
async fn clear_refuses_focused_pane() {
    // Every refusal before the command: its reason, nothing sent, nothing
    // recorded. The twin is clear_types_submits_and_binds_the_new_session,
    // which differs only in this first frame.
    type Change = fn(&mut PaneCapture);
    let cases: [(&str, Change, &str); 8] = [
        (
            CLAUDE_CONVERSATION,
            |c| c.focused = Some(true),
            "the pane is focused",
        ),
        (
            CLAUDE_CONVERSATION,
            |c| c.focused = None,
            "whether the pane is focused could not be read",
        ),
        // The harness exited to its shell; its last frame is still on screen.
        (
            CLAUDE_CONVERSATION,
            |c| c.inspection.foreground_command = Some("zsh".into()),
            "the harness is not known to be the pane's foreground program",
        ),
        (
            CLAUDE_CONVERSATION,
            |c| c.inspection.pane.pane_pid = 101,
            "the harness is not known to be the pane's foreground program",
        ),
        (
            CLAUDE_CONVERSATION,
            |c| c.inspection.in_mode = true,
            "the pane is in copy mode",
        ),
        (CLAUDE_DRAFT, |_| {}, "the editor is not empty and idle"),
        (CLAUDE_RUNNING, |_| {}, "the editor is not empty and idle"),
        (CLAUDE_MODAL, |_| {}, "the editor is not empty and idle"),
    ];
    for (text, change, reason) in cases {
        let clearing = Clearing::new(Harness::ClaudeCode, Old::SameProcess).await;
        let before = clearing.rig.org.status().unwrap();
        clearing.frame_with(text, change);
        match clearing.clear().await {
            Err(AgentError::Refused(refusal)) => {
                assert!(refusal.contains(reason), "{refusal}");
                assert!(refusal.contains(NOTHING_SENT), "{refusal}");
            }
            other => panic!("{reason}: {other:?}"),
        }
        assert!(clearing.sent().is_empty(), "{reason}");
        assert_eq!(clearing.unread(), 0, "{reason}");
        assert_eq!(clearing.rig.org.status().unwrap(), before, "{reason}");
        let agent = clearing.agent().await;
        assert_eq!(agent.status, AgentStatus::Ready);
        assert_eq!(agent.session_id, Some(SessionId::new("old")));
    }
}

#[tokio::test(start_paused = true)]
async fn clear_refuses_what_it_cannot_identify() {
    // A pane nothing was recorded about: its screen is never trusted.
    let clearing = Clearing::new(Harness::ClaudeCode, Old::SameProcess).await;
    clearing
        .agents
        .record_baseline(lead(), "%9", PaneBaseline::adopted(100));
    let before = clearing.rig.org.status().unwrap();
    clearing.frame(CLAUDE_CONVERSATION);
    match clearing.clear().await {
        Err(AgentError::Refused(refusal)) => {
            assert!(refusal.contains("not known to be the pane's foreground program"));
            assert!(refusal.contains(NOTHING_SENT), "{refusal}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(clearing.rig.org.status().unwrap(), before);

    // Two sessions that could be the agent's: refused before the pane is read.
    let mut clearing = Clearing::new(Harness::ClaudeCode, Old::Lingering).await;
    clearing.follow_up();
    clearing
        .rig
        .row_of("other", "%1", false, Harness::ClaudeCode)
        .await;
    clearing.settled().await;
    let before = clearing.rig.org.status().unwrap();
    match clearing.clear().await {
        Err(AgentError::Refused(refusal)) => {
            assert!(refusal.contains("more than one session"), "{refusal}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(clearing.unread(), 0);
    assert_eq!(clearing.rig.org.status().unwrap(), before);

    // An agent that is not Ready, and one that does not exist.
    clearing
        .rig
        .command(OrgCommand::Stuck {
            agent_id: lead(),
            reason: "for the test".into(),
        })
        .await;
    match clearing.clear().await {
        Err(AgentError::Refused(refusal)) => {
            assert!(
                refusal.contains("is stuck; only a ready agent"),
                "{refusal}"
            );
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        clearing
            .agents
            .clear(ClearParams {
                agent: "p/nobody".parse().unwrap(),
                timeout_ms: 30_000
            })
            .await,
        Err(AgentError::NotFound(_))
    ));
    assert!(clearing.sent().is_empty());

    // An agent that is Clearing without this call having made it so (another
    // clear's record): refused like any agent that is not Ready, and this
    // call's cleanup leaves that clear's state alone.
    let clearing = Clearing::new(Harness::ClaudeCode, Old::SameProcess).await;
    clearing
        .rig
        .command(OrgCommand::Clear { agent_id: lead() })
        .await;
    let before = clearing.rig.org.status().unwrap();
    match clearing.clear().await {
        Err(AgentError::Refused(refusal)) => {
            assert!(
                refusal.contains("is clearing; only a ready agent"),
                "{refusal}"
            );
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(clearing.agent().await.status, AgentStatus::Clearing);
    assert_eq!(clearing.rig.org.status().unwrap(), before);
}

#[tokio::test(start_paused = true)]
async fn draft_appears_before_guarded_paste_sends_nothing() {
    // The precheck saw a safe editor; on the paste guard's own capture there
    // is a draft, then an unread dialog, then a focused pane. Nothing is
    // pasted into any of them. On a safe editor the command is pasted, once.
    let mut clearing = Clearing::new(Harness::ClaudeCode, Old::SameProcess).await;
    clearing.frame(CLAUDE_CONVERSATION);
    clearing.frame(CLAUDE_DRAFT);
    clearing.frame(CLAUDE_MODAL);
    clearing.frame_with(CLAUDE_CONVERSATION, |c| c.focused = Some(true));
    // The twin of the three refused guards: the same guard on a safe frame.
    clearing.frame(CLAUDE_CONVERSATION);
    clearing.frame(CLAUDE_STAGED);
    clearing.frame(CLAUDE_STAGED);
    clearing.frame(CLAUDE_CLEARED);
    let call = clearing.start();
    clearing.awaiting_session().await;
    assert_eq!(clearing.sent(), ["paste %1 /clear", "keys %1 Enter"]);
    assert_eq!(clearing.unread(), 3, "only the last capture is left");
    clearing.session_start("new", Some("clear")).await;
    call.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn op7_draft_through_clear_deadline_sends_nothing_and_safe_wake_recovers() {
    // Break caught: a refused paste guard sends input, or a no-effect clear
    // deadline prevents recovery on the next eligible, safe session wake.
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        let mut clearing = Clearing::new(Harness::ClaudeCode, Old::SameProcess).await;
        clearing.frame(CLAUDE_CONVERSATION);
        // More draft captures than the five-second work budget can consume.
        // Unread captures below prove the flow ended at its deadline, not
        // because its observation script ran out.
        for _ in 0..32 {
            clearing.frame(CLAUDE_DRAFT);
        }
        let result = clearing
            .agents
            .clear(ClearParams {
                agent: lead(),
                timeout_ms: 5_000,
            })
            .await;
        assert_told_stuck(&result, &clearing.agent().await);
        assert!(clearing.sent().is_empty(), "the draft was never changed");
        clearing
            .assert_stuck("the editor stopped being safe before the clear command was typed")
            .await;
        assert!(!quarantined(&clearing.rig, "p/lead").await);
        let remaining = clearing.unread();
        assert!(remaining > 0 && remaining < 32 * 3, "{remaining}");
        assert_eq!(remaining % 3, 0);
        // Discard only the unused fake observations, through the real read-only
        // actor API; no production input or state transition clears a draft.
        for _ in 0..remaining / 3 {
            let capture = clearing
                .rig
                .control
                .observe(&pane("%1"), CaptureFormat::Ansi)
                .await
                .unwrap();
            assert_eq!(capture.text, body(CLAUDE_DRAFT));
        }
        clearing
            .rig
            .frame_of("%1", "p/lead", body(CLAUDE_CLEARED), "claude");
        clearing.follow_up();
        clearing.follow_up();
        clearing.session_start("new", Some("clear")).await;
        clearing.settled().await;
        let agent = clearing.agent().await;
        assert_eq!(agent.status, AgentStatus::Ready);
        assert_eq!(agent.session_id, Some(SessionId::new("new")));
        assert_eq!(agent.reason, None);
        assert!(clearing.sent().is_empty(), "recovery does not send the clear");
    })
    .await
    .expect("draft deadline and recovery exceeded their hang guard");
}

#[tokio::test(start_paused = true)]
async fn refused_paste_guard_until_the_pane_is_unreadable_is_stuck_with_nothing_sent() {
    let clearing = Clearing::new(Harness::ClaudeCode, Old::SameProcess).await;
    clearing.frame(CLAUDE_CONVERSATION);
    clearing.frame(CLAUDE_DRAFT);
    // Nothing more is scripted: the next guard cannot read the pane.
    let result = clearing.clear().await;
    assert!(result.is_err(), "{result:?}");
    assert!(clearing.sent().is_empty());
    clearing
        .assert_stuck_with("typing the clear command failed")
        .await;
}

#[tokio::test(start_paused = true)]
async fn modal_between_capture_and_enter_sends_no_key() {
    // The command is staged on the flow's capture; on the Enter guard's own
    // capture a dialog is open. No Enter. The next capture shows a dialog the
    // adapter recognises: the clear stops at once.
    let clearing = Clearing::new(Harness::ClaudeCode, Old::SameProcess).await;
    clearing.frame(CLAUDE_CONVERSATION);
    clearing.frame(CLAUDE_CONVERSATION);
    clearing.frame(CLAUDE_STAGED);
    clearing.frame(CLAUDE_MODAL);
    clearing.frame(CLAUDE_TRUST);
    // Never read: the flow has stopped.
    clearing.frame(CLAUDE_STAGED);
    let result = clearing.clear().await;
    assert_told_stuck(&result, &clearing.agent().await);
    match result {
        Err(AgentError::Refused(refusal)) => {
            assert!(
                refusal.contains("a dialog opened before the command was submitted"),
                "{refusal}"
            );
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(clearing.sent(), ["paste %1 /clear"], "no Enter");
    assert_eq!(clearing.unread(), 3, "one capture was left unread");
    clearing
        .assert_stuck("a dialog opened before the command was submitted")
        .await;
}

#[tokio::test(start_paused = true)]
async fn focus_between_paste_and_enter_sends_no_enter() {
    // Someone looks at the pane after the paste. The flow watches without a
    // key; the Enter goes out once, on a guard capture nobody is focused on.
    let mut clearing = Clearing::new(Harness::ClaudeCode, Old::SameProcess).await;
    clearing.frame(CLAUDE_CONVERSATION);
    clearing.frame(CLAUDE_CONVERSATION);
    // Focused on the flow's capture: no guard is even tried.
    clearing.frame_with(CLAUDE_STAGED, |c| c.focused = Some(true));
    // Unfocused on the flow's capture, focused on the guard's.
    clearing.frame(CLAUDE_STAGED);
    clearing.frame_with(CLAUDE_STAGED, |c| c.focused = Some(true));
    // Unfocused on the flow's capture, focus unknown on the guard's.
    clearing.frame(CLAUDE_STAGED);
    clearing.frame_with(CLAUDE_STAGED, |c| c.focused = None);
    // The twin: both unfocused.
    clearing.frame(CLAUDE_STAGED);
    clearing.frame(CLAUDE_STAGED);
    clearing.frame(CLAUDE_CLEARED);
    let call = clearing.start();
    clearing.awaiting_session().await;
    assert_eq!(clearing.sent(), ["paste %1 /clear", "keys %1 Enter"]);
    assert_eq!(clearing.unread(), 3, "only the last capture is left");
    clearing.session_start("new", Some("clear")).await;
    call.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn whitespace_draft_plus_clear_never_gets_enter() {
    // After the paste the editor holds something that is not exactly the
    // command (here: another draft), on every capture until the pane can't be
    // read any more. Never an Enter, and the draft is left alone.
    let clearing = Clearing::new(Harness::ClaudeCode, Old::SameProcess).await;
    clearing.frame(CLAUDE_CONVERSATION);
    clearing.frame(CLAUDE_CONVERSATION);
    for _ in 0..3 {
        clearing.frame(CLAUDE_DRAFT);
    }
    let result = clearing.clear().await;
    assert!(result.is_err(), "{result:?}");
    assert_eq!(clearing.sent(), ["paste %1 /clear"]);
    assert_eq!(clearing.unread(), 0);
    clearing
        .assert_stuck(&format!(
            "{NOT_SUBMITTED}: the command is not staged in the editor"
        ))
        .await;
}

fn fail_next_tmux_call(clearing: &Clearing) {
    clearing
        .rig
        .client
        .mock()
        .set_next_error(atm_tmux::TmuxError::ParseError("scripted failure".into()));
}

#[tokio::test(start_paused = true)]
async fn clear_enter_outcome_unknown_never_repeats_enter() {
    // The guarded Enter fails as an operation (the scripted client can only
    // fail the next tmux call, which is the operation's own read; the flow
    // gets the same kind of error when the key itself fails). The staged
    // command is still on the following captures: the flow must not look, or
    // send, again. Twin without the fault:
    // clear_types_submits_and_binds_the_new_session.
    let mut clearing = Clearing::new(Harness::ClaudeCode, Old::SameProcess).await;
    clearing.frames_through_enter(CLAUDE_CONVERSATION, CLAUDE_STAGED);
    clearing.frame(CLAUDE_STAGED);
    clearing.frame(CLAUDE_STAGED);
    let call = clearing.start();
    // The flow is about to send its Enter. Captures of a pane are paced, and
    // on the paused clock that pacing cannot end before this task is idle
    // again: the fault is in place for the Enter operation.
    progress(&mut clearing.events, "clear_submitting", Some("p/lead")).await;
    assert_eq!(clearing.sent(), ["paste %1 /clear"]);
    fail_next_tmux_call(&clearing);
    let result = call.await.unwrap();
    assert!(
        matches!(result, Err(AgentError::Reservation(_))),
        "{result:?}"
    );
    assert_eq!(clearing.sent(), ["paste %1 /clear"], "no second attempt");
    assert!(clearing.unread() >= 6, "the flow did not look again");
    clearing
        .assert_stuck_with(
            "submitting the clear command failed, and whether it reached the pane is unknown",
        )
        .await;
}

#[tokio::test(start_paused = true)]
async fn a_guard_refusal_that_sent_nothing_is_not_an_unknown_outcome() {
    // Review 23, probes r2 and r2b. The flow's own capture was clean, and the
    // control actor refuses on the capture of the paste's guard (`at_paste`)
    // or of the Enter's. Twins with an uncertain error: the two
    // *_outcome_unknown_* tests.
    type Change = fn(&mut PaneCapture);
    let passing: [(&str, Change); 2] = [
        ("copy mode", |capture| capture.inspection.in_mode = true),
        ("a dead pane", |capture| capture.inspection.dead = true),
    ];
    for at_paste in [true, false] {
        // The pane refuses input for now: nothing was sent, and the flow
        // looks again. The pane allows it later, and the clear completes.
        for (label, change) in passing {
            let mut clearing = Clearing::new(Harness::ClaudeCode, Old::SameProcess).await;
            clearing.frame(CLAUDE_CONVERSATION);
            if at_paste {
                clearing.frame_with(CLAUDE_CONVERSATION, change);
            }
            clearing.frame(CLAUDE_CONVERSATION);
            clearing.frame(CLAUDE_STAGED);
            if !at_paste {
                clearing.frame_with(CLAUDE_STAGED, change);
                clearing.frame(CLAUDE_STAGED);
            }
            clearing.frame(CLAUDE_STAGED);
            clearing.frame(CLAUDE_CLEARED);
            clearing.follow_up();
            clearing.follow_up();
            let call = clearing.start();
            clearing.awaiting_session().await;
            clearing.session_start("new", Some("clear")).await;
            let result = call.await.unwrap();
            assert!(result.is_ok(), "{label}, at the paste {at_paste}: {result:?}");
            assert_eq!(clearing.sent(), ["paste %1 /clear", "keys %1 Enter"]);
            clearing.settled().await;
            assert_eq!(clearing.unread(), 0, "{label}, at the paste {at_paste}");
        }
        // The pane is not the agent's any more, or is gone: nothing was sent,
        // and the clear stops saying so.
        for gone in [false, true] {
            let clearing = Clearing::new(Harness::ClaudeCode, Old::SameProcess).await;
            clearing.frame(CLAUDE_CONVERSATION);
            if !at_paste {
                clearing.frame(CLAUDE_CONVERSATION);
                clearing.frame(CLAUDE_STAGED);
            }
            if gone {
                clearing.rig.client.script_inspect(&pane("%1"), None).unwrap();
            } else {
                clearing.frame_with(CLAUDE_STAGED, |capture| {
                    capture.inspection.local_agent_tag = Some("p/other".into());
                });
            }
            let result = clearing.clear().await;
            assert!(
                matches!(result, Err(AgentError::Reservation(_))),
                "{result:?}"
            );
            let sent: &[&str] = if at_paste { &[] } else { &["paste %1 /clear"] };
            assert_eq!(clearing.sent(), sent, "gone {gone}");
            let what = if at_paste { "typing" } else { "submitting" };
            clearing
                .assert_stuck_with(&format!(
                    "{what} the clear command was refused, and nothing was sent by it (pane %1"
                ))
                .await;
            assert_eq!(clearing.unread(), 0, "gone {gone}, at the paste {at_paste}");
        }
    }
}

#[tokio::test(start_paused = true)]
async fn deadline_during_the_enter_says_the_command_may_have_been_submitted() {
    // Review 23, follow-up 1. tmux takes the Enter and its call does not
    // return before the clear's deadline: the agent is Stuck, and the reason
    // must not say that the command never appeared in the editor.
    let mut clearing = Clearing::new(Harness::ClaudeCode, Old::SameProcess).await;
    clearing.frames_through_enter(CLAUDE_CONVERSATION, CLAUDE_STAGED);
    let call = clearing.start();
    progress(&mut clearing.events, "clear_submitting", Some("p/lead")).await;
    clearing.rig.client.stall_pane_effects(true);
    let result = call.await.unwrap();
    assert_told_stuck(&result, &clearing.agent().await);
    assert_eq!(clearing.sent(), ["paste %1 /clear", "keys %1 Enter"]);
    clearing
        .assert_stuck("the clear command was being submitted and may have reached the harness")
        .await;
}

/// A clear that its deadline ends while tmux holds its Enter, as
/// deadline_during_the_enter_says_the_command_may_have_been_submitted, on an
/// organisation whose first write of an event of `kind` fails. Returns the
/// rig and the caller's answer.
async fn clear_ended_at_its_deadline_with_one_failed_write(
    kind: &'static str,
) -> (Clearing, Result<ClearResult, AgentError>) {
    let rig = Rig::gated_with_failure(None, Some(kind)).await;
    let mut clearing = Clearing::on(rig, Harness::ClaudeCode, Old::SameProcess).await;
    clearing.frames_through_enter(CLAUDE_CONVERSATION, CLAUDE_STAGED);
    let call = clearing.start();
    progress(&mut clearing.events, "clear_submitting", Some("p/lead")).await;
    clearing.rig.client.stall_pane_effects(true);
    let result = call.await.unwrap();
    (clearing, result)
}

#[tokio::test(start_paused = true)]
async fn op7_late_stuck_commit_keeps_timeout_without_a_note_and_quarantines() {
    // Break caught: retaining a Stuck reason before its append is acknowledged
    // falsely turns this call's timeout into a refusal naming an unproven write.
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        let (gate, mut entered) = mpsc::channel(1);
        let rig = Rig::gated(Some(("agent.stuck", gate))).await;
        let mut clearing = Clearing::on(rig, Harness::ClaudeCode, Old::SameProcess).await;
        clearing.frames_through_enter(CLAUDE_CONVERSATION, CLAUDE_STAGED);
        let call = clearing.start();
        progress(&mut clearing.events, "clear_submitting", Some("p/lead")).await;
        clearing.rig.client.stall_pane_effects(true);
        let release = entered.recv().await.unwrap();
        // The append gate witnesses cleanup entry. Advance past the org reply
        // cap while keeping the append itself held: no real-I/O timing is used.
        tokio::time::advance(std::time::Duration::from_secs(6)).await;
        let result = call.await.unwrap();
        assert!(
            matches!(result, Err(AgentError::Reservation(
                crate::control::reservation::ReservationError::Timeout
            ))),
            "{result:?}"
        );
        assert!(quarantined(&clearing.rig, "p/lead").await);
        // A FIFO snapshot after release sees the late durable write, but the
        // abandoned cleanup cannot send its note or revise the caller's answer.
        release.send(()).unwrap();
        let state = clearing.rig.org.snapshot().await.unwrap();
        let agent = state.agent(&lead()).unwrap();
        assert_eq!(agent.status, AgentStatus::Stuck);
        assert_eq!(agent.reason.as_deref(), Some(
            "clear did not complete: the clear command was being submitted and may have reached the harness; inspect %1"
        ));
        assert_eq!(state.messages().count(), 0);
        assert_eq!(clearing.sent(), ["paste %1 /clear", "keys %1 Enter"]);
    })
    .await
    .expect("late Stuck append exceeded its hang guard");
}

#[tokio::test(start_paused = true)]
async fn failed_stuck_write_at_the_deadline_keeps_the_timeout() {
    // The cleanup's Stuck write fails: nothing says why the clear ended, so
    // the caller is not told a reason either. The answer stays the timeout,
    // the agent is still Clearing with no reason and no note, and the
    // reservation is quarantined. Twin:
    // failed_note_at_the_deadline_still_answers_with_the_recorded_reason
    // (the Stuck write succeeds).
    let (clearing, result) =
        clear_ended_at_its_deadline_with_one_failed_write("agent.stuck").await;
    assert!(
        matches!(
            result,
            Err(AgentError::Reservation(
                crate::control::reservation::ReservationError::Timeout
            ))
        ),
        "{result:?}"
    );
    let agent = clearing.agent().await;
    assert_eq!(agent.status, AgentStatus::Clearing, "{agent:?}");
    assert_eq!(agent.reason, None);
    let state = clearing.rig.org.snapshot().await.unwrap();
    assert_eq!(state.messages().count(), 0, "nobody is told by a note");
    assert!(quarantined(&clearing.rig, "p/lead").await);
}

#[tokio::test(start_paused = true)]
async fn failed_note_at_the_deadline_still_answers_with_the_recorded_reason() {
    // The cleanup's Stuck write succeeds and the owner's note cannot be
    // written. The choice: the record is written, so the caller is told its
    // reason, whatever follows. The agent is Stuck with that reason, there is
    // no note, and the reservation is quarantined. Twin:
    // failed_stuck_write_at_the_deadline_keeps_the_timeout (no record).
    const REASON: &str = "clear did not complete: the clear command was being submitted \
        and may have reached the harness; inspect %1";
    let (clearing, result) = clear_ended_at_its_deadline_with_one_failed_write("msg.sent").await;
    let agent = clearing.agent().await;
    assert_eq!(agent.reason.as_deref(), Some(REASON));
    assert_told_stuck(&result, &agent);
    let state = clearing.rig.org.snapshot().await.unwrap();
    assert_eq!(state.messages().count(), 0, "the note was not written");
    assert!(quarantined(&clearing.rig, "p/lead").await);
}

#[tokio::test(start_paused = true)]
async fn timeout_waiting_for_the_reservation_is_not_answered_with_an_earlier_stuck_reason() {
    // The agent is Stuck since an earlier call, and another job holds its
    // reservation. A clear waits to be admitted and runs out of its time
    // there: its flow never ran, and its cleanup recorded nothing. The
    // caller keeps the timeout. The earlier reason is not this call's
    // outcome, and the record keeps it. Twin:
    // deadline_during_the_enter_says_the_command_may_have_been_submitted
    // (a timeout this call's own cleanup recorded Stuck for).
    const EARLIER: &str = "start did not complete: an earlier call; inspect %1";
    let (rig, agents) = ready_agent().await;
    rig.command(OrgCommand::Stuck {
        agent_id: lead(),
        reason: EARLIER.into(),
    })
    .await;
    let (release, held) = hold_agent_reservation(&rig, "p/lead", "%1").await;
    let result = agents
        .clear(ClearParams {
            agent: lead(),
            timeout_ms: 5_000,
        })
        .await;
    assert!(
        matches!(
            result,
            Err(AgentError::Reservation(
                crate::control::reservation::ReservationError::Timeout
            ))
        ),
        "{result:?}"
    );
    let agent = rig.state("lead").await;
    assert_eq!(agent.status, AgentStatus::Stuck);
    assert_eq!(agent.reason.as_deref(), Some(EARLIER));
    release.send(()).unwrap();
    held.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn session_that_starts_before_the_enter_is_not_the_clears_proof() {
    // Review 23, probe r3. Somebody clears by hand in the agent's pane after
    // the flow's first read and before its Enter: that session is not what
    // the flow's Enter produced. The flow waits for one that starts after it.
    // Twin: clear_types_submits_and_binds_the_new_session.
    for (harness, command, conversation, staged, cleared) in ANNOUNCING {
        let mut clearing = Clearing::new(harness, Old::SameProcess).await;
        let mut committed = clearing.rig.org.committed.subscribe();
        clearing.frames_through_enter(conversation, staged);
        clearing.frame(cleared);
        let call = clearing.start();
        // The Clear is committed after the precheck's capture, and the
        // paste's guard is paced after that capture. Bounded: the rig keeps
        // the sender alive, so a Clear that is never recorded would
        // otherwise be waited for without end.
        tokio::time::timeout(std::time::Duration::from_secs(60), async {
            loop {
                if committed.recv().await.unwrap().kind == "agent.clear_requested" {
                    break;
                }
            }
        })
        .await
        .expect("the clear was never recorded");
        clearing.session_start("early", Some("clear")).await;
        clearing.awaiting_session().await;
        assert_eq!(
            clearing.sent(),
            [format!("paste %1 {command}"), "keys %1 Enter".to_owned()],
        );
        let agent = clearing.agent().await;
        assert_eq!(
            (agent.status, agent.session_id.clone()),
            (AgentStatus::Clearing, None),
            "{harness:?}"
        );
        clearing.session_start("new", Some("clear")).await;
        let result = call.await.unwrap().unwrap();
        assert_eq!(
            result.session_id,
            Some(SessionId::new("new")),
            "{harness:?}"
        );
        assert_eq!(
            clearing.agent().await.session_id,
            Some(SessionId::new("new"))
        );
    }
}

#[tokio::test(start_paused = true)]
async fn clear_paste_outcome_unknown_never_repeats_paste() {
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        let clearing = Clearing::new(Harness::ClaudeCode, Old::SameProcess).await;
        let mut committed = clearing.rig.org.committed.subscribe();
        clearing.frame(CLAUDE_CONVERSATION);
        clearing.frame(CLAUDE_CONVERSATION);
        // A safe editor again: a second paste would pass its guard.
        clearing.frame(CLAUDE_CONVERSATION);
        let call = clearing.start();
        // The Clear is committed after the precheck's capture; the paste's guard
        // is paced after that capture (see the test above).
        loop {
            if committed.recv().await.unwrap().kind == "agent.clear_requested" {
                break;
            }
        }
        fail_next_tmux_call(&clearing);
        let result = call.await.unwrap();
        assert!(
            matches!(result, Err(AgentError::Reservation(_))),
            "{result:?}"
        );
        assert!(clearing.sent().is_empty(), "no second attempt");
        assert!(clearing.unread() >= 3, "the flow did not look again");
        clearing
            .assert_stuck_with(
                "typing the clear command failed, and whether it reached the pane is unknown",
            )
            .await;
    })
    .await
    .expect("clear must finish even if its intent is never committed");
}

#[tokio::test(start_paused = true)]
async fn clear_reply_lost_goes_stuck_from_fresh_snapshot() {
    tokio::time::timeout(std::time::Duration::from_secs(90), async {
        // The Clear hangs in the organisation's append, and the flow gives up on
        // its reply. `released`: the append then completes while the cleanup's
        // snapshot waits behind it; the snapshot sees the agent Clearing and the
        // cleanup marks it Stuck. Otherwise the cleanup can't learn the outcome
        // and the agent's reservation is quarantined. Nothing was typed either way.
        for released in [true, false] {
            let (gate, mut entered) = mpsc::channel(1);
            let rig = Rig::gated(Some(("agent.clear_requested", gate))).await;
            let clearing = Clearing::on(rig, Harness::ClaudeCode, Old::SameProcess).await;
            clearing.frame(CLAUDE_CONVERSATION);
            let call = clearing.start();
            let release = entered.recv().await.unwrap();
            // Virtual time: past the wait for the command's reply (6 s), and
            // either inside or past the cleanup's wait for its snapshot (5 s more).
            tokio::time::sleep(std::time::Duration::from_secs(if released { 8 } else { 20 })).await;
            assert_eq!(call.is_finished(), !released);
            release.send(()).unwrap();
            let result = call.await.unwrap();
            assert!(result.is_err(), "{result:?}");
            assert!(clearing.sent().is_empty());
            let again = clearing
                .rig
                .control
                .run_reserved(
                    request("p/lead", Some("%1"), ReservationKind::Observe),
                    |_| async { Ok::<_, AgentError>(()) },
                    cleanup,
                )
                .await;
            if released {
                clearing
                    .assert_stuck("the clear command was not typed")
                    .await;
                again.unwrap();
            } else {
                assert!(
                    matches!(
                        again,
                        Err(AgentError::Reservation(
                            crate::control::reservation::ReservationError::Quarantined { .. }
                        ))
                    ),
                    "{again:?}"
                );
            }
        }
    })
    .await
    .expect("clear must finish even if its append gate is never entered");
}

#[tokio::test(start_paused = true)]
async fn duplicate_clear_conflicts_while_first_waits_for_delivery() {
    // A delivery holds the agent. The first clear is admitted and waits behind
    // it; a second is refused as a conflict. While the first waits, the
    // agent's first session arrives: the clear binds it before clearing, so
    // that it is excluded and can never be taken for the new one.
    let mut clearing = Clearing::new(Harness::ClaudeCode, Old::Unbound).await;
    let mut committed = clearing.rig.org.committed.subscribe();
    let (entered, entry) = tokio::sync::oneshot::channel();
    let (release, released) = tokio::sync::oneshot::channel();
    let control = clearing.rig.control.clone();
    let delivery = tokio::spawn(async move {
        control
            .run_reserved(
                request("p/lead", Some("%1"), ReservationKind::Delivery),
                move |_| async move {
                    entered.send(()).unwrap();
                    released.await.unwrap();
                    Ok::<_, AgentError>(())
                },
                cleanup,
            )
            .await
            .unwrap();
    });
    entry.await.unwrap();
    clearing.frames_through_enter(CLAUDE_CONVERSATION, CLAUDE_STAGED);
    clearing.frame(CLAUDE_CLEARED);
    let first = clearing.start();
    // A later job of another agent, done: the dispatcher has seen the first.
    clearing
        .agents
        .lifecycle(
            request("p/other", None, ReservationKind::Down),
            |_| async { Ok(()) },
            cleanup,
        )
        .await
        .unwrap();
    match clearing.clear().await {
        Err(AgentError::Conflict(reason)) => assert!(reason.contains("is busy"), "{reason}"),
        other => panic!("{other:?}"),
    }
    assert!(!first.is_finished());
    clearing.session_start("old", Some("startup")).await;
    update_unrelated(&clearing.rig, &mut clearing.events).await;
    assert!(
        clearing.sent().is_empty(),
        "nothing while the delivery holds"
    );
    assert_eq!(clearing.agent().await.session_id, None);
    release.send(()).unwrap();
    delivery.await.unwrap();
    clearing.awaiting_session().await;
    let agent = clearing.agent().await;
    assert_eq!(agent.status, AgentStatus::Clearing);
    assert_eq!(agent.excluded_session_id, Some(SessionId::new("old")));
    // The old session reporting again is not the clear's result.
    clearing.session_start("old", Some("clear")).await;
    clearing.awaiting_session().await;
    assert_eq!(clearing.agent().await.status, AgentStatus::Clearing);
    clearing.session_start("new", Some("clear")).await;
    let result = first.await.unwrap().unwrap();
    assert_eq!(result.session_id, Some(SessionId::new("new")));
    assert_eq!(
        committed_kinds(&mut committed),
        [
            "agent.session_bound",
            "agent.clear_requested",
            "msg.sent",
            "agent.session_bound"
        ]
    );
    assert_eq!(clearing.sent(), ["paste %1 /clear", "keys %1 Enter"]);
}

#[tokio::test(start_paused = true)]
async fn clear_never_binds_a_pending_row_before_clearing() {
    let rig = Rig::new().await;
    rig.row_of("pending-4242", "%1", false, Harness::ClaudeCode)
        .await;
    let mut clearing = Clearing::on(rig, Harness::ClaudeCode, Old::Unbound).await;
    let mut committed = clearing.rig.org.committed.subscribe();
    clearing.frames_through_enter(CLAUDE_CONVERSATION, CLAUDE_STAGED);
    clearing.frame(CLAUDE_CLEARED);
    let call = clearing.start();
    clearing.awaiting_session().await;
    let agent = clearing.agent().await;
    assert_eq!(agent.status, AgentStatus::Clearing);
    assert_eq!(agent.session_id, None);
    assert_eq!(
        agent.excluded_session_id, None,
        "a pending id was never bound first"
    );
    clearing.session_start("new", Some("clear")).await;
    assert_eq!(
        call.await.unwrap().unwrap().session_id,
        Some(SessionId::new("new"))
    );
    assert_eq!(
        committed_kinds(&mut committed),
        ["agent.clear_requested", "msg.sent", "agent.session_bound"]
    );
    assert_eq!(clearing.sent(), ["paste %1 /clear", "keys %1 Enter"]);
}

#[tokio::test(start_paused = true)]
async fn codex_clear_goes_from_conversation_to_fresh_editor() {
    // Codex announces no session when it clears: the proof is the screen.
    // Replay one real P6 session in order, including its transitional Unknown
    // frame. The first fresh poll completes the clear; later polls stay unread.
    // A neighboring test checks exclusion until the next prompt binds.
    let clearing = Clearing::new(Harness::Codex, Old::SameProcess).await;
    let mut committed = clearing.rig.org.committed.subscribe();
    clearing.frames_through_enter(CODEX_P6_CONVERSATION, CODEX_P6_POPUP);
    for frame in CODEX_P6_AFTER_ENTER {
        clearing.frame(frame);
    }
    let result = clearing.clear().await.unwrap();
    assert_eq!(result.session_id, None, "the next prompt announces it");
    assert_eq!(
        clearing.sent(),
        ["paste %1 /new", "keys %1 Enter"],
        "one submit, no worktree choice in this real sequence"
    );
    assert_eq!(clearing.unread(), 9, "polls 002 through 004 were not needed");
    assert_eq!(
        committed_kinds(&mut committed),
        ["agent.clear_requested", "msg.sent", "agent.editor_ready"]
    );
    let agent = clearing.agent().await;
    assert_eq!(agent.status, AgentStatus::Ready);
    assert_eq!(agent.session_id, None);
    assert_eq!(agent.excluded_session_id, Some(SessionId::new("old")));
}

#[tokio::test(start_paused = true)]
async fn codex_p6_80x24_finished_popup_gets_one_enter() {
    let clearing = Clearing::new(Harness::Codex, Old::SameProcess).await;
    let reader = crate::org::agent_screen::agent_screen(Harness::Codex).unwrap();
    let baseline = clearing.agents.baseline_of(&lead(), "%1");
    let mut popup = clearing.capture(CODEX_P6_80_POPUP);
    popup.inspection.pane.width = 80;
    assert_eq!(
        reader.control(&popup, &baseline).dialog,
        atm_core::harness_control::ControlDialog::None
    );
    // One real P6 session: finished conversation, selected popup, transitional
    // Unknown, then fresh editor. The finished-turn boundary permits Enter.
    for frame in [
        CODEX_P6_80_CONVERSATION,
        CODEX_P6_80_CONVERSATION,
        CODEX_P6_80_POPUP,
        CODEX_P6_80_POPUP,
    ]
    .into_iter()
    .chain(CODEX_P6_80_AFTER_ENTER)
    {
        clearing.frame_with(frame, |capture| capture.inspection.pane.width = 80);
    }
    let result = clearing.clear().await;
    assert_eq!(clearing.sent(), ["paste %1 /new", "keys %1 Enter"]);
    assert_eq!(result.unwrap().session_id, None);
    assert_eq!(clearing.unread(), 0);
    let agent = clearing.agent().await;
    assert_eq!(agent.status, AgentStatus::Ready);
    assert_eq!(agent.session_id, None);
    assert_eq!(agent.excluded_session_id, Some(SessionId::new("old")));
}

#[tokio::test(start_paused = true)]
async fn codex_p6_80x24_popup_without_inverse_gets_no_enter() {
    let popup = body(CODEX_P6_80_POPUP);
    let selected = popup
        .lines()
        .find(|line| line.contains("start a new chat during a conversation"))
        .unwrap();
    // Synthetic, hand-changed: remove only inverse selection from the real
    // popup row. Without positive selection evidence the guard must refuse
    // Enter, even though the composer still holds exactly /new.
    let unselected = selected
        .replace("\x1b[0;1;7m", "\x1b[0;1m")
        .replace("\x1b[0;7m", "\x1b[0m");
    assert_ne!(unselected, selected);
    let changed = popup.replacen(selected, &unselected, 1);
    let clearing = Clearing::new(Harness::Codex, Old::SameProcess).await;
    let reader = crate::org::agent_screen::agent_screen(Harness::Codex).unwrap();
    let baseline = clearing.agents.baseline_of(&lead(), "%1");
    let mut capture = clearing.capture(&changed);
    capture.inspection.pane.width = 80;
    assert_eq!(
        reader.control(&capture, &baseline).dialog,
        atm_core::harness_control::ControlDialog::Unknown
    );
    for frame in [
        CODEX_P6_80_CONVERSATION,
        CODEX_P6_80_CONVERSATION,
        CODEX_P6_80_POPUP,
        &changed,
        &changed,
    ] {
        clearing.frame_with(frame, |capture| capture.inspection.pane.width = 80);
    }
    // The staged read is recognized, but selection disappears on the guard's
    // own capture and stays absent until the scripted frames run out.
    let result = clearing.clear().await;
    assert_eq!(clearing.sent(), ["paste %1 /new"]);
    assert!(result.is_err(), "{result:?}");
    assert_eq!(clearing.unread(), 0);
    clearing
        .assert_stuck(&format!("{NOT_SUBMITTED}: the screen was not recognised"))
        .await;
}

#[tokio::test(start_paused = true)]
async fn codex_old_row_stays_excluded_until_the_next_prompt_binds() {
    let mut clearing = Clearing::new(Harness::Codex, Old::SameProcess).await;
    clearing.frames_through_enter(CODEX_P6_CONVERSATION, CODEX_P6_POPUP);
    for frame in CODEX_P6_AFTER_ENTER.into_iter().take(2) {
        clearing.frame(frame);
    }
    clearing.clear().await.unwrap();
    assert_eq!(clearing.unread(), 0);
    // The old conversation's row is still there and reports again: never bound.
    clearing.session_start("old", Some("startup")).await;
    clearing.settled().await;
    assert_eq!(clearing.agent().await.session_id, None);
    // The first prompt after the clear announces the new session: bound by
    // the observer, as after any start.
    clearing.follow_up();
    clearing.follow_up();
    clearing.session_start("fresh", Some("startup")).await;
    clearing.settled().await;
    let agent = clearing.agent().await;
    assert_eq!(agent.status, AgentStatus::Ready);
    assert_eq!(agent.session_id, Some(SessionId::new("fresh")));
}

#[tokio::test(start_paused = true)]
async fn worktree_default_is_answered_only_once() {
    let clearing = Clearing::new(Harness::Codex, Old::SameProcess).await;
    clearing.frames_through_enter(CODEX_CONVERSATION, CODEX_POPUP);
    clearing.frame(CODEX_WORKTREE);
    clearing.frame(CODEX_WORKTREE);
    clearing.frame(CODEX_WORKTREE);
    clearing.frame(CODEX_FRESH);
    assert_eq!(clearing.clear().await.unwrap().session_id, None);
    assert_eq!(
        clearing.sent(),
        ["paste %1 /new", "keys %1 Enter", "keys %1 Enter"]
    );
    assert_eq!(clearing.unread(), 0);
    assert_eq!(clearing.agent().await.status, AgentStatus::Ready);
}

#[tokio::test(start_paused = true)]
async fn worktree_default_changes_before_guard_refused() {
    // The worktree choice is on the flow's capture; on the guard's own
    // capture the screen is another one. No key. Twin (the key is sent):
    // worktree_default_is_answered_only_once.
    let clearing = Clearing::new(Harness::Codex, Old::SameProcess).await;
    clearing.frames_through_enter(CODEX_CONVERSATION, CODEX_POPUP);
    clearing.frame(CODEX_WORKTREE);
    clearing.frame(CODEX_FRESH);
    clearing.frame(CODEX_FRESH);
    let result = clearing.clear().await.unwrap();
    assert_eq!(result.session_id, None);
    assert_eq!(
        clearing.sent(),
        ["paste %1 /new", "keys %1 Enter"],
        "no Enter for a choice that was gone"
    );
    assert_eq!(clearing.unread(), 0);
    assert_eq!(clearing.agent().await.status, AgentStatus::Ready);
}

#[tokio::test(start_paused = true)]
async fn worktree_focused_at_guard_sends_no_key() {
    for focused in [Some(true), None] {
        let clearing = Clearing::new(Harness::Codex, Old::SameProcess).await;
        clearing.frames_through_enter(CODEX_CONVERSATION, CODEX_POPUP);
        clearing.frame(CODEX_WORKTREE);
        clearing.frame_with(CODEX_WORKTREE, |capture| capture.focused = focused);
        // The choice disappears before the next pass. A mutant that ignores
        // focus would send a key into the focused dialog, then also finish.
        clearing.frame(CODEX_FRESH);
        assert_eq!(clearing.clear().await.unwrap().session_id, None);
        assert_eq!(
            clearing.sent(),
            ["paste %1 /new", "keys %1 Enter"],
            "{focused:?}"
        );
        assert_eq!(clearing.unread(), 0);
        assert_eq!(clearing.agent().await.status, AgentStatus::Ready);
    }
}

#[tokio::test(start_paused = true)]
async fn worktree_uncertain_key_is_not_retried() {
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        let mut clearing = Clearing::new(Harness::Codex, Old::SameProcess).await;
        clearing.frames_through_enter(CODEX_CONVERSATION, CODEX_POPUP);
        clearing.frame(CODEX_WORKTREE);
        clearing.frame(CODEX_WORKTREE);
        clearing.frame(CODEX_WORKTREE);
        clearing.frame(CODEX_WORKTREE);
        clearing.frame(CODEX_FRESH);
        let call = clearing.start();
        progress(&mut clearing.events, "clear_submitting", Some("p/lead")).await;
        // Advance only virtual capture pacing until the flow has read the
        // worktree dialog. The next operation is its paced guarded key.
        while clearing.unread() > 12 && !call.is_finished() {
            tokio::time::advance(std::time::Duration::from_millis(250)).await;
        }
        assert_eq!(clearing.unread(), 12, "worktree guard is next");
        assert_eq!(clearing.sent(), ["paste %1 /new", "keys %1 Enter"]);
        fail_next_tmux_call(&clearing);
        let result = call.await.unwrap();
        assert_eq!(
            clearing.sent(),
            ["paste %1 /new", "keys %1 Enter"],
            "no second worktree key"
        );
        assert!(matches!(result, Err(AgentError::Reservation(_))), "{result:?}");
        clearing
            .assert_stuck_with(
                "answering the worktree choice failed, and whether it reached the pane is unknown",
            )
            .await;
        // Only the failed capture's diagnostic metadata read was consumed.
        assert_eq!(clearing.unread(), 11, "no further capture after uncertainty");
    })
    .await
    .expect("clear must finish even if the worktree key fails");
}

#[tokio::test(start_paused = true)]
async fn codex_popup_other_selection_sends_no_enter() {
    // Also codex_popup_candidate_above_selected_sends_no_enter and
    // codex_popup_selected_row_without_inverse_sends_no_enter. Synthetic: the
    // real popup frame with its one row changed. Codex's adapter reads none
    // of them as "no dialog", so the staged command gets no Enter. Twins
    // (the unchanged frame gets its Enter): worktree_default_is_answered_only_once,
    // codex_second_clear_ignores_the_excluded_old_row.
    let popup = body(CODEX_POPUP);
    let row = popup
        .lines()
        .find(|line| line.contains("start a new chat during a conversation"))
        .unwrap();
    let plain = row.replace("\x1b[1;7m", "\x1b[1m").replace("\x1b[0;7m", "\x1b[0m");
    let other = plain.replace("/new", "/next");
    assert_ne!(plain, row);
    for (case, changed) in [
        ("another command selected", row.replace("/new", "/next")),
        ("a candidate above the selected row", format!("{other}\n{row}")),
        ("a second candidate below", format!("{row}\n{other}")),
        ("the selected row without its inverse style", plain.clone()),
    ] {
        let frame = popup.replacen(row, &changed, 1);
        let clearing = Clearing::new(Harness::Codex, Old::SameProcess).await;
        clearing.frame(CODEX_CONVERSATION);
        clearing.frame(CODEX_CONVERSATION);
        clearing.frame(&frame);
        clearing.frame(&frame);
        let result = clearing.clear().await;
        assert!(result.is_err(), "{case}: {result:?}");
        assert_eq!(clearing.sent(), ["paste %1 /new"], "{case}");
        assert_eq!(clearing.unread(), 0, "{case}");
        clearing
            .assert_stuck(&format!("{NOT_SUBMITTED}: the screen was not recognised"))
            .await;
    }
}

#[tokio::test(start_paused = true)]
async fn codex_ready_unbound_noop_sends_zero_keys() {
    // Ready without a session, no session on the pane, a fresh safe editor:
    // there is nothing to clear, and nothing is sent or recorded.
    let clearing = Clearing::new(Harness::Codex, Old::Unbound).await;
    let before = clearing.rig.org.status().unwrap();
    clearing.frame(CODEX_FRESH);
    let result = clearing.clear().await.unwrap();
    assert_eq!(result.agent_id, lead());
    assert_eq!(result.session_id, None);
    assert!(clearing.sent().is_empty());
    assert_eq!(clearing.unread(), 0);
    assert_eq!(clearing.rig.org.status().unwrap(), before);
    assert_eq!(clearing.agent().await.status, AgentStatus::Ready);
}

#[tokio::test(start_paused = true)]
async fn codex_second_clear_ignores_the_excluded_old_row() {
    let clearing = Clearing::new(Harness::Codex, Old::SameProcess).await;
    clearing.frames_through_enter(CODEX_CONVERSATION, CODEX_POPUP);
    clearing.frame(CODEX_FRESH);
    clearing.clear().await.unwrap();
    assert!(clearing
        .rig
        .registry
        .get_session(SessionId::new("old"))
        .await
        .is_some());
    let agent = clearing.agent().await;
    assert_eq!((agent.status, agent.session_id), (AgentStatus::Ready, None));
    assert_eq!(agent.excluded_session_id, Some(SessionId::new("old")));
    let before = clearing.rig.org.status().unwrap();
    let sent = clearing.sent();
    clearing.frame(CODEX_FRESH);
    assert_eq!(clearing.clear().await.unwrap().session_id, None);
    assert_eq!(clearing.sent(), sent, "a second clear sends nothing");
    assert_eq!(
        clearing.rig.org.status().unwrap(), before,
        "no rebind or clear intent"
    );
    assert_eq!(clearing.unread(), 0);
    assert_eq!(
        clearing.agent().await.excluded_session_id,
        Some(SessionId::new("old"))
    );
}

#[tokio::test(start_paused = true)]
async fn codex_bound_or_new_row_requires_conversation_to_fresh() {
    // A bound agent on a fresh editor: no conversation is on screen, so a
    // fresh editor afterwards would prove nothing. Refused, nothing sent.
    // Twin: codex_clear_goes_from_conversation_to_fresh_editor.
    let clearing = Clearing::new(Harness::Codex, Old::SameProcess).await;
    let before = clearing.rig.org.status().unwrap();
    clearing.frame(CODEX_FRESH);
    match clearing.clear().await {
        Err(AgentError::Refused(refusal)) => {
            assert!(
                refusal.contains("no conversation is on screen"),
                "{refusal}"
            );
            assert!(refusal.contains(NOTHING_SENT), "{refusal}");
        }
        other => panic!("{other:?}"),
    }
    assert!(clearing.sent().is_empty());
    assert_eq!(clearing.rig.org.status().unwrap(), before);
    let agent = clearing.agent().await;
    assert_eq!(agent.status, AgentStatus::Ready);
    assert_eq!(agent.session_id, Some(SessionId::new("old")));
}

#[tokio::test(start_paused = true)]
async fn codex_clear_refuses_when_prompt_scrolled_off() {
    // Synthetic: the real conversation frame without its prompt row, as when a
    // long answer has pushed the prompt out of the viewport. The adapter then
    // reports no conversation, and the clear refuses before any effect.
    let prompt = body(CODEX_CONVERSATION)
        .lines()
        .find(|line| line.contains("B_VALID_FIRST_AFTER_NEW"))
        .unwrap();
    let scrolled = body(CODEX_CONVERSATION).replacen(prompt, "", 1);
    let clearing = Clearing::new(Harness::Codex, Old::SameProcess).await;
    let before = clearing.rig.org.status().unwrap();
    clearing.frame(&scrolled);
    match clearing.clear().await {
        Err(AgentError::Refused(refusal)) => {
            assert!(
                refusal.contains("no conversation is on screen"),
                "{refusal}"
            );
            assert!(refusal.contains(NOTHING_SENT), "{refusal}");
        }
        other => panic!("{other:?}"),
    }
    assert!(clearing.sent().is_empty());
    assert_eq!(clearing.rig.org.status().unwrap(), before);
    assert_eq!(clearing.agent().await.status, AgentStatus::Ready);
}

#[tokio::test(start_paused = true)]
async fn missed_hook_conversation_not_noop() {
    // Ready without a session and without a row, but a conversation is on
    // screen (its hook never arrived): that is something to clear. The
    // command is sent, and the clear is proven by the screen as usual.
    let clearing = Clearing::new(Harness::Codex, Old::Unbound).await;
    let mut committed = clearing.rig.org.committed.subscribe();
    clearing.frames_through_enter(CODEX_CONVERSATION, CODEX_POPUP);
    clearing.frame(CODEX_FRESH);
    let result = clearing.clear().await.unwrap();
    assert_eq!(result.session_id, None);
    assert_eq!(clearing.sent(), ["paste %1 /new", "keys %1 Enter"]);
    assert_eq!(clearing.unread(), 0);
    assert_eq!(
        committed_kinds(&mut committed),
        ["agent.clear_requested", "msg.sent", "agent.editor_ready"]
    );
    assert_eq!(clearing.agent().await.status, AgentStatus::Ready);
}

#[tokio::test]
async fn clear_timeout_outside_its_range_is_refused_with_the_agent_unchanged() {
    // Through the method, with a Ready agent behind it: a timeout under the
    // floor (or over the ceiling) is refused before the clear is asked for,
    // so nothing is read, sent or recorded. Twin: the floor itself reaches
    // the clear's own precheck, which refuses for another reason.
    const RANGE: &str = "invalid agent parameters: timeout_ms must be in 5000..=120000";
    let clearing = Clearing::new(Harness::ClaudeCode, Old::SameProcess).await;
    let mut router = crate::server::methods::MethodRouter::default();
    crate::org::agent_methods::register_agent_methods(&mut router, clearing.agents.clone())
        .unwrap();
    let before = clearing.rig.org.snapshot().await.unwrap();
    let mut committed = clearing.rig.org.committed.subscribe();
    let clear = |timeout: u64| {
        router.call(
            caller(),
            "agent.clear",
            serde_json::json!({"agent": "p/lead", "timeout_ms": timeout}),
        )
    };
    for timeout in [0, 1, 4_999, 120_001] {
        let error = clear(timeout).await.unwrap_err().0;
        assert_eq!(error.code, "invalid_params", "{timeout}");
        assert_eq!(error.message, RANGE, "{timeout}");
    }
    assert_eq!(clearing.rig.org.snapshot().await.unwrap(), before);
    assert!(committed_kinds(&mut committed).is_empty());
    assert!(mutations(&clearing.rig).is_empty());

    clearing.frame_with(CLAUDE_CONVERSATION, |c| c.focused = Some(true));
    let error = clear(5_000).await.unwrap_err().0;
    assert_eq!(error.code, "refused");
    assert!(
        error.message.contains("the pane is focused"),
        "{}",
        error.message
    );
    assert_eq!(clearing.rig.client.pending_reads(&pane("%1")).unwrap(), 0);
}

#[tokio::test(start_paused = true)]
async fn clear_does_not_publish_ready_while_its_start_line_append_is_held() {
    let (gate, mut entered) = mpsc::channel(1);
    let rig = Rig::gated(Some(("msg.sent", gate))).await;
    let clearing = Clearing::on(rig, Harness::Codex, Old::Unbound).await;
    let mut committed = clearing.rig.org.committed.subscribe();
    clearing.frames_through_enter(CODEX_CONVERSATION, CODEX_POPUP);
    clearing.frame(CODEX_FRESH);
    let call = clearing.start();
    let release = tokio::time::timeout(std::time::Duration::from_secs(60), entered.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(committed_kinds(&mut committed), ["agent.clear_requested"]);
    release.send(()).unwrap();
    call.await.unwrap().unwrap();
    assert_eq!(committed_kinds(&mut committed), ["msg.sent", "agent.editor_ready"]);
}

#[tokio::test(start_paused = true)]
async fn a_refused_clear_start_append_still_publishes_ready() {
    let rig = Rig::gated_with_failure(None, Some("msg.sent")).await;
    let clearing = Clearing::on(rig, Harness::Codex, Old::Unbound).await;
    let mut committed = clearing.rig.org.committed.subscribe();
    clearing.frames_through_enter(CODEX_CONVERSATION, CODEX_POPUP);
    clearing.frame(CODEX_FRESH);
    clearing.clear().await.unwrap();
    assert_eq!(committed_kinds(&mut committed), ["agent.clear_requested", "agent.editor_ready"]);
    assert_eq!(clearing.agent().await.status, AgentStatus::Ready);
    assert_eq!(clearing.rig.org.snapshot().await.unwrap().messages().count(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_clear_queues_one_start_line_and_a_second_clear_reuses_it() {
    // The clear's start line is the canonical note to the agent, queued. A
    // second clear while that line is still open queues no second one: the
    // open line is typed to the newest session.
    let clearing = Clearing::new(Harness::Codex, Old::Unbound).await;
    let lines = || async {
        let state = clearing.rig.org.snapshot().await.unwrap();
        state
            .messages()
            .map(|message| (message.content.clone(), message.status))
            .collect::<Vec<_>>()
    };
    clearing.frames_through_enter(CODEX_CONVERSATION, CODEX_POPUP);
    clearing.frame(CODEX_FRESH);
    clearing.clear().await.unwrap();
    let once = vec![(
        crate::org::start_line::start_line(&lead()),
        atm_org::DeliveryStatus::Queued,
    )];
    assert_eq!(lines().await, once);

    let mut committed = clearing.rig.org.committed.subscribe();
    clearing.frames_through_enter(CODEX_CONVERSATION, CODEX_POPUP);
    clearing.frame(CODEX_FRESH);
    clearing.clear().await.unwrap();
    assert_eq!(
        clearing.sent(),
        ["paste %1 /new", "keys %1 Enter", "paste %1 /new", "keys %1 Enter"]
    );
    assert_eq!(
        committed_kinds(&mut committed),
        ["agent.clear_requested", "agent.editor_ready"]
    );
    assert_eq!(lines().await, once);
}

#[tokio::test(start_paused = true)]
async fn handover_clear_does_not_claim_a_carried_plain_start_is_a_resume() {
    use crate::org::agent_lifecycle::{ClearAttempt, SavedSeat};
    use crate::org::start_line::{StartLineDisposition, start_line};
    for copy in ["New session: run the shell command atm me", "New session: run atm me"] {
        let mut c = Clearing::new(Harness::ClaudeCode, Old::SameProcess).await;
        let mut note = start_line(&lead());
        note.body = copy.into();
        c.rig.command(OrgCommand::SendMessage(note.clone())).await;
        let mut committed = c.rig.org.committed.subscribe();
        c.frames_through_enter(CLAUDE_CONVERSATION, CLAUDE_STAGED);
        c.frame(CLAUDE_CLEARED);
        c.follow_up();
        c.follow_up();
        let agents = c.agents.clone();
        let call = tokio::spawn(async move {
            agents.clear_handover(
                SavedSeat { agent: lead(), pane: pane("%1"), session: "old".into() },
                None,
                tokio::time::Instant::now() + std::time::Duration::from_secs(120),
            ).await
        });
        c.awaiting_session().await;
        c.session_start("new", Some("clear")).await;
        let result = call.await.unwrap().unwrap();
        assert!(matches!(&result, ClearAttempt::Cleared(clear)
            if clear.result.session_id == Some("new".into())
                && matches!(clear.start_line, StartLineDisposition::Unidentified)), "{result:?}");
        assert_eq!(committed_kinds(&mut committed), ["agent.clear_requested", "agent.session_bound"]);
        let state = c.rig.org.snapshot().await.unwrap();
        assert_eq!(state.messages().map(|m| (&m.content, m.status)).collect::<Vec<_>>(),
            [(&note, atm_org::DeliveryStatus::Queued)]);
    }
}

#[tokio::test(start_paused = true)]
async fn a_clear_with_start_lines_off_queues_none() {
    let mut rig = Rig::new().await;
    rig.spawn_env
        .insert("ATM_SPAWN_START_LINE".into(), "off".into());
    let clearing = Clearing::on(rig, Harness::Codex, Old::Unbound).await;
    let mut committed = clearing.rig.org.committed.subscribe();
    clearing.frames_through_enter(CODEX_CONVERSATION, CODEX_POPUP);
    clearing.frame(CODEX_FRESH);
    clearing.clear().await.unwrap();
    assert_eq!(
        committed_kinds(&mut committed),
        ["agent.clear_requested", "agent.editor_ready"]
    );
    assert_eq!(clearing.agent().await.status, AgentStatus::Ready);
}
