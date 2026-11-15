use super::agents::*;
use crate::server::methods::{router, CallContext, CallerId, MethodServices};
use serde_json::{json, Value};

mod reconciliation {
    use super::*;
    use crate::{
        control::{
            reservation::{
                testing::ScriptedTmuxClient, CaptureFormat, PaneCapture, PaneId, PaneInspection,
            },
            service::spawn_control_with_org,
        },
        registry::{RegistryActor, RegistryHandle},
    };
    use atm_core::{AgentType, Harness, Model, SessionDomain, SessionId};
    use atm_org::OrgCommand;
    use tokio::sync::{broadcast, mpsc, watch};
    use tokio_util::sync::CancellationToken;

    const IDLE: &str =
        include_str!("../../../atm-pi-adapter/tests/fixtures/screen_pi-after-clear_1.5.0.txt");
    // Real styled captures; each adapter's fixture README has the `Captured:`
    // line, which `body` removes. The P1 ones are `-e -N` with tmux's padding.
    const CODEX_IDLE: &str = include_str!(
        "../../../atm-codex-adapter/tests/fixtures/screen_styled_u5b_rich-fresh_0.167.0.txt"
    );
    const CODEX_PADDED: &str = include_str!(
        "../../../atm-codex-adapter/tests/fixtures/screen_styled_p1_c11-working-released-100x24_0.167.0.txt"
    );
    const PI_PADDED: &str = include_str!(
        "../../../atm-pi-adapter/tests/fixtures/screen_styled_p1_A-ready-120x40_1.5.0.txt"
    );
    const CLAUDE_PADDED: [&str; 2] = [
        include_str!(
            "../../../atm-claude-adapter/tests/fixtures/screen_control_p1_A120-ready-120x40_2.1.325.txt"
        ),
        include_str!(
            "../../../atm-claude-adapter/tests/fixtures/screen_control_p1_B-baseline-conversation-120x40_2.1.325.txt"
        ),
    ];
    use super::super::agent_screen_tests::body;
    struct Rig {
        org: super::super::OrgHandle,
        registry: RegistryHandle,
        events: broadcast::Sender<crate::registry::SessionEvent>,
        client: ScriptedTmuxClient,
        control: crate::control::service::ControlHandle,
        cancel: CancellationToken,
        /// What the scripted procfs reader answers for any process. By default
        /// a shell with job control (`zsh`, the pane's root, pid 100) running
        /// one job in the foreground: the shape under which a harness named
        /// by a capture can be pinned.
        proc: std::sync::Arc<std::sync::Mutex<Option<super::super::agent_flow::ProcFacts>>>,
        /// What the reader answers, in place of `proc`, once an Enter has been
        /// sent to a pane; unset by default. A test of a start sets it: the
        /// Enter runs the launch, and from then on the pane's shell holds the
        /// harness as its foreground job.
        proc_after_enter:
            std::sync::Arc<std::sync::Mutex<Option<super::super::agent_flow::ProcFacts>>>,
        /// Answers the reader gives first, one per read, before `proc`.
        proc_script: std::sync::Arc<
            std::sync::Mutex<
                std::collections::VecDeque<Option<super::super::agent_flow::ProcFacts>>,
            >,
        >,
    }
    impl Rig {
        async fn new() -> Self {
            Self::gated(None).await
        }
        async fn gated(
            gate: Option<(&'static str, mpsc::Sender<tokio::sync::oneshot::Sender<()>>)>,
        ) -> Self {
            Self::gated_with_failure(gate, None).await
        }
        async fn gated_with_failure(
            gate: Option<(&'static str, mpsc::Sender<tokio::sync::oneshot::Sender<()>>)>,
            mut fail_once: Option<&'static str>,
        ) -> Self {
            let cancel = CancellationToken::new();
            let (requests, receive) = mpsc::channel(64);
            let (health, status) = watch::channel(Ok(0));
            let (committed, _) = broadcast::channel(1024);
            tokio::spawn(super::super::actor::run(
                Default::default(),
                receive,
                health,
                committed.clone(),
                cancel.clone(),
                {
                    let mut seq = 0;
                    move |envelope: crate::eventlog::Envelope| {
                        let fail = fail_once == Some(envelope.kind.as_str());
                        if fail {
                            fail_once = None;
                        } else {
                            seq += 1;
                        }
                        let gate = gate.clone();
                        async move {
                            if fail {
                                return Err(crate::eventlog::EventLogError::TimeoutBeforeHandover);
                            }
                            if let Some((kind, gate)) = gate {
                                if envelope.kind == kind {
                                    let (release, released) = tokio::sync::oneshot::channel();
                                    gate.send(release).await.unwrap();
                                    released.await.unwrap();
                                }
                            }
                            Ok(seq)
                        }
                    }
                },
            ));
            let org = super::super::OrgHandle {
                requests,
                health: status,
                committed,
                reads: None,
            };
            let (send, receive) = mpsc::channel(100);
            let (events, _) = broadcast::channel(2048);
            let actor = RegistryActor::new(receive, events.clone());
            let token = cancel.clone();
            tokio::spawn(async move {
                tokio::select! { _ = token.cancelled() => {}, _ = actor.run() => {} }
            });
            let registry = RegistryHandle::new(send, events.clone());
            let client = ScriptedTmuxClient::new();
            let (control, _) = spawn_control_with_org(
                Box::new(client.clone()),
                registry.clone(),
                org.clone(),
                cancel.clone(),
            );
            org.execute(
                OrgCommand::RegisterProject {
                    project_id: "p".parse().unwrap(),
                    root: "/private/project".into(),
                    move_root: false,
                },
                chrono::Utc::now(),
            )
            .await
            .unwrap();
            Self {
                org,
                registry,
                events,
                client,
                control,
                cancel,
                proc: std::sync::Arc::new(std::sync::Mutex::new(Some(
                    super::super::agent_flow::ProcFacts {
                        process_group: 100,
                        foreground_group: 150,
                        system_shell: true,
                        children: 1,
                        name: "zsh".into(),
                    },
                ))),
                proc_after_enter: Default::default(),
                proc_script: Default::default(),
            }
        }
        async fn command(&self, command: OrgCommand) {
            self.org.execute(command, chrono::Utc::now()).await.unwrap();
        }
        async fn agent(&self, name: &str, pane: Option<&str>, session: Option<&str>) {
            self.agent_of(name, "pi", pane, session).await;
        }
        async fn agent_of(
            &self,
            name: &str,
            harness: &str,
            pane: Option<&str>,
            session: Option<&str>,
        ) {
            let id = format!("p/{name}").parse().unwrap();
            self.command(OrgCommand::Up {
                agent_id: id,
                role: atm_org::parse_role(name, &format!("+++\nharness='{harness}'\n+++\n"))
                    .unwrap(),
            })
            .await;
            if let Some(pane) = pane {
                self.command(OrgCommand::PaneCreated {
                    agent_id: format!("p/{name}").parse().unwrap(),
                    pane: pane.into(),
                })
                .await;
                if let Some(session) = session {
                    self.command(OrgCommand::SessionBound {
                        agent_id: format!("p/{name}").parse().unwrap(),
                        session_id: SessionId::new(session),
                    })
                    .await;
                } else {
                    self.command(OrgCommand::EditorReady {
                        agent_id: format!("p/{name}").parse().unwrap(),
                    })
                    .await;
                }
            }
        }
        async fn row(&self, id: &str, pane: &str, child: bool) {
            self.row_of(id, pane, child, Harness::Pi).await;
        }
        async fn row_of(&self, id: &str, pane: &str, child: bool, harness: Harness) {
            let mut row = SessionDomain::new(
                SessionId::new(id),
                AgentType::GeneralPurpose,
                Model::default(),
            );
            row.harness = harness;
            row.tmux_pane = Some(pane.into());
            if child {
                row.parent_session_id = Some(SessionId::new("parent"));
            }
            self.registry.register(row).await.unwrap();
        }
        fn inspection(pane: &str, tag: &str, dead: bool) -> PaneInspection {
            PaneInspection::new(
                atm_tmux::PaneInfo {
                    pane_id: pane.into(),
                    session_name: "private".into(),
                    window_index: 0,
                    pane_pid: 100,
                    width: 120,
                    height: IDLE.lines().count() as u16,
                    is_active: true,
                },
                Some(tag.into()),
                dead,
                false,
                Some(true),
                0,
                0,
            )
        }
        fn inspect_only(&self, pane: &str, tag: &str) {
            self.client
                .script_inspect(
                    &PaneId::parse(pane).unwrap(),
                    Some(&Self::inspection(pane, tag, false)),
                )
                .unwrap();
        }
        fn frame(&self, pane: &str, tag: &str, text: &str) {
            self.frame_of(pane, tag, text, "pi");
        }
        // A capture as tmux reports it while `command` is the pane's foreground program.
        // Every adapter reads a styled capture, so that is what the observer asks for.
        fn frame_of(&self, pane: &str, tag: &str, text: &str, command: &str) {
            let mut inspection = Self::inspection(pane, tag, false)
                .with_foreground_process(None, Some(command.into()));
            inspection.pane.height = text.lines().count() as u16;
            self.client
                .script_inspect(&PaneId::parse(pane).unwrap(), Some(&inspection))
                .unwrap();
            self.client
                .script_capture(&PaneCapture::new(
                    text.into(),
                    inspection,
                    CaptureFormat::Ansi,
                ))
                .unwrap();
        }
        fn start(&self) -> (AgentHandle, tokio::task::JoinHandle<()>) {
            spawn_agents_with(
                self.org.clone(),
                self.registry.clone(),
                self.control.clone(),
                self.cancel.child_token(),
                self.reader(),
                std::sync::Arc::new(|_| None),
            )
        }
        /// The scripted procfs reader. Every test starts the service with it:
        /// the observer reads a pane's root process, and the scripted panes'
        /// pid 100 must never be looked up on the machine the tests run on.
        fn reader(&self) -> super::super::agent_proc::ProcReader {
            let (proc, script) = (self.proc.clone(), self.proc_script.clone());
            let (after_enter, mock) = (self.proc_after_enter.clone(), self.client.mock());
            std::sync::Arc::new(move |_| {
                let scripted = script.lock().unwrap().pop_front();
                let facts = scripted.unwrap_or_else(|| {
                    let entered = mock.calls().iter().any(|call| {
                        matches!(call, atm_tmux::mock::MockCall::SendKeys { keys, .. } if keys == "Enter")
                    });
                    let launched = after_enter.lock().unwrap().clone().filter(|_| entered);
                    launched.or_else(|| proc.lock().unwrap().clone())
                });
                Box::pin(async move { facts })
            })
        }
        /// Record that `command` is the harness in the foreground of `name`'s
        /// pane, as an adopt or an earlier observation would have. The
        /// observer itself pins only Starting and Ready agents, so a test
        /// whose agent is Stuck from the start gives it what it would have.
        fn pinned(agents: &AgentHandle, name: &str, pane: &str, command: &str) {
            agents.record_baseline(
                format!("p/{name}").parse().unwrap(),
                pane,
                PaneBaseline::adopted(100)
                    .with_harness(command.into())
                    .unwrap(),
            );
        }
        async fn state(&self, name: &str) -> atm_org::Agent {
            self.org
                .snapshot()
                .await
                .unwrap()
                .agent(&format!("p/{name}").parse().unwrap())
                .unwrap()
                .clone()
        }
    }
    impl Drop for Rig {
        fn drop(&mut self) {
            self.cancel.cancel();
        }
    }

    #[tokio::test]
    async fn review_restart_ready_unbound_is_not_a_lifecycle_change() {
        for text in [
            IDLE,
            "unknown\n",
            include_str!(
                "../../../atm-pi-adapter/tests/fixtures/screen_pi-before-escape_1.5.0.txt"
            ),
        ] {
            let rig = Rig::new().await;
            rig.agent("lead", Some("%1"), None).await;
            rig.row("pending-100", "%1", false).await;
            rig.frame("%1", "p/lead", text);
            let before = rig.org.status().unwrap();
            let (agents, _) = rig.start();
            agents.reconciled().await.unwrap();
            assert_eq!(rig.state("lead").await.status, AgentStatus::Ready);
            assert_eq!(rig.org.status().unwrap(), before);
        }
    }

    #[tokio::test]
    async fn review_restart_ready_unbound_reads_no_screen() {
        // No outcome of a capture can change a session-less Ready agent whose
        // harness can't be pinned (here: its pane's processes can't be read),
        // so the restart check must not read its screen, or fail when it can't.
        // The twin that pins: restart_rederives_baseline_with_live_row.
        for capture in [true, false] {
            let rig = Rig::new().await;
            *rig.proc.lock().unwrap() = None;
            rig.agent("lead", Some("%1"), None).await;
            rig.row("pending-100", "%1", false).await;
            if capture {
                rig.frame("%1", "p/lead", "unknown\n");
            } else {
                rig.inspect_only("%1", "p/lead");
            }
            let before = rig.org.status().unwrap();
            let (agents, _) = rig.start();
            agents.reconciled().await.unwrap();
            assert_eq!(rig.state("lead").await.status, AgentStatus::Ready);
            assert_eq!(rig.org.status().unwrap(), before);
            assert_eq!(
                rig.client
                    .pending_reads(&PaneId::parse("%1").unwrap())
                    .unwrap(),
                if capture { 3 } else { 0 }
            );
            // The agent keeps Ready, and the list says why it takes no input:
            // the frame's inspection names pi and the processes can't be read;
            // the bare inspection names no foreground command at all.
            assert_eq!(
                agents.list(ListParams::default()).await.unwrap()[0]
                    .reason
                    .as_deref(),
                Some(if capture { UNREADABLE } else { NO_COMMAND })
            );
        }
    }

    #[tokio::test]
    async fn review_safe_restart_ready_has_zero_writes() {
        let rig = Rig::new().await;
        rig.agent("lead", Some("%1"), Some("recorded")).await;
        rig.row("pending-100", "%1", false).await;
        rig.frame("%1", "p/lead", IDLE);
        let before = rig.org.status().unwrap();
        let (agents, _) = rig.start();
        agents.reconciled().await.unwrap();
        assert_eq!(rig.org.status().unwrap(), before);
        assert_eq!(
            rig.state("lead").await.session_id,
            Some(SessionId::new("recorded"))
        );
    }

    #[tokio::test]
    async fn restart_bound_codex_agent_stays_ready_on_styled_editor() {
        // A1 review G4: Codex's empty editor is only readable in a styled capture
        // (its placeholder is plain text otherwise), so a bound Codex agent used
        // to be suspended at every daemon restart.
        let rig = Rig::new().await;
        rig.agent_of("lead", "codex", Some("%1"), Some("recorded"))
            .await;
        rig.row_of("pending-100", "%1", false, Harness::Codex).await;
        rig.frame_of("%1", "p/lead", body(CODEX_IDLE), "codex");
        let before = rig.org.status().unwrap();
        let (agents, _) = rig.start();
        agents.reconciled().await.unwrap();
        // The capture was read (a refused or failed read would also write nothing).
        assert_eq!(
            rig.client
                .pending_reads(&PaneId::parse("%1").unwrap())
                .unwrap(),
            0
        );
        assert_eq!(
            agents.list(ListParams::default()).await.unwrap()[0].reason,
            None
        );
        let agent = rig.state("lead").await;
        assert_eq!(agent.status, AgentStatus::Ready);
        assert_eq!(agent.session_id, Some(SessionId::new("recorded")));
        assert_eq!(rig.org.status().unwrap(), before);
    }

    #[tokio::test]
    async fn restart_check_reads_real_padded_idle_editors() {
        // The control actor captures with `-N`, which pads rows to the pane's
        // width. A real padded capture of each harness's idle editor must read
        // as safe, or every restart suspends a Ready bound agent. For Claude
        // this takes the control observer: its legacy one does not know the
        // manual-mode footer of these two frames.
        for (harness, tag, command, text) in [
            ("pi", Harness::Pi, "pi", PI_PADDED),
            ("codex", Harness::Codex, "codex", CODEX_PADDED),
            ("claude", Harness::ClaudeCode, "claude", CLAUDE_PADDED[0]),
            ("claude", Harness::ClaudeCode, "claude", CLAUDE_PADDED[1]),
        ] {
            let text = body(text);
            assert!(
                text.lines().filter(|line| line.ends_with(' ')).count() > 5,
                "{harness}: not a padded capture"
            );
            let rig = Rig::new().await;
            rig.agent_of("lead", harness, Some("%1"), Some("recorded"))
                .await;
            rig.row_of("pending-100", "%1", false, tag).await;
            rig.frame_of("%1", "p/lead", text, command);
            let before = rig.org.status().unwrap();
            let (agents, _) = rig.start();
            agents.reconciled().await.unwrap();
            assert_eq!(
                rig.client
                    .pending_reads(&PaneId::parse("%1").unwrap())
                    .unwrap(),
                0,
                "{harness}"
            );
            assert_eq!(
                agents.list(ListParams::default()).await.unwrap()[0].reason,
                None,
                "{harness}"
            );
            assert_eq!(
                rig.state("lead").await.status,
                AgentStatus::Ready,
                "{harness}"
            );
            assert_eq!(rig.org.status().unwrap(), before, "{harness}");
        }
    }

    #[tokio::test]
    async fn stuck_codex_agent_recovers_on_styled_safe_editor() {
        // A1 review G2/G4: leaving Stuck takes this observation's own capture of
        // a safe editor, which the plain Codex read could never show.
        let rig = Rig::new().await;
        rig.agent_of("lead", "codex", Some("%1"), Some("s1")).await;
        rig.row_of("s1", "%1", false, Harness::Codex).await;
        let reason = "unexpected screen in %1; inspect the pane";
        rig.command(OrgCommand::Stuck {
            agent_id: "p/lead".parse().unwrap(),
            reason: reason.into(),
        })
        .await;
        rig.inspect_only("%1", "p/lead");
        let (agents, _) = rig.start();
        agents.reconciled().await.unwrap();
        // Stuck since before this daemon started: the observer pins nothing
        // for it, so it has the baseline of the adopt that would precede this.
        Rig::pinned(&agents, "lead", "%1", "codex");
        let mut events = agents.observation_events();
        let before = rig.org.status().unwrap();
        // An unknown screen keeps Stuck: the twin that differs only in the frame.
        rig.frame_of("%1", "p/lead", "unknown\n", "codex");
        update(&rig, "s1").await;
        progress(&mut events, "finished", Some("p/lead")).await;
        assert_eq!(rig.state("lead").await.status, AgentStatus::Stuck);
        assert_eq!(rig.org.status().unwrap(), before);
        rig.frame_of("%1", "p/lead", body(CODEX_IDLE), "codex");
        update(&rig, "s1").await;
        progress(&mut events, "finished", Some("p/lead")).await;
        let agent = rig.state("lead").await;
        assert_eq!(agent.status, AgentStatus::Ready);
        assert_eq!(agent.reason, None);
        assert_eq!(agent.session_id, Some(SessionId::new("s1")));
        assert_eq!(rig.org.status().unwrap(), before + 1);
        assert_eq!(
            rig.client
                .pending_reads(&PaneId::parse("%1").unwrap())
                .unwrap(),
            0
        );
    }

    async fn recipient_of(agents: &AgentHandle, name: &str) -> Result<Recipient, AgentError> {
        agents
            .recipient(&format!("p/{name}").parse().unwrap())
            .await
    }

    #[tokio::test]
    async fn recipient_registry_unavailable_is_an_error() {
        // A stopped registry.
        let rig = Rig::new().await;
        rig.agent("lead", Some("%1"), Some("recorded")).await;
        let (send, receive) = mpsc::channel(1);
        drop(receive);
        let (events, _) = broadcast::channel(1);
        let (agents, _) = spawn_agents_with(
            rig.org.clone(),
            RegistryHandle::new(send, events),
            rig.control.clone(),
            rig.cancel.child_token(),
            rig.reader(),
            std::sync::Arc::new(|_| None),
        );
        agents.reconciled().await.unwrap();
        assert!(matches!(
            recipient_of(&agents, "lead").await,
            Err(AgentError::Unavailable(_))
        ));

        // A registry that takes the request and drops it. Its other reads
        // answer, so the unchecked list sees the same failure as "no rows".
        let rig = Rig::new().await;
        rig.agent("lead", Some("%1"), None).await;
        let (send, mut receive) = mpsc::channel(16);
        let (events, _) = broadcast::channel(16);
        let (asked, mut asks) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(command) = receive.recv().await {
                match command {
                    crate::registry::RegistryCommand::GetAllSessions { respond_to } => {
                        let _ = respond_to.send(Vec::new());
                    }
                    crate::registry::RegistryCommand::PaneSessions { pane, respond_to } => {
                        drop(respond_to);
                        asked.send(pane).unwrap();
                    }
                    other => panic!("unexpected registry command: {other:?}"),
                }
            }
        });
        rig.inspect_only("%1", "p/lead");
        let (agents, _) = spawn_agents_with(
            rig.org.clone(),
            RegistryHandle::new(send, events),
            rig.control.clone(),
            rig.cancel.child_token(),
            rig.reader(),
            std::sync::Arc::new(|_| None),
        );
        agents.reconciled().await.unwrap();
        assert!(matches!(
            recipient_of(&agents, "lead").await,
            Err(AgentError::Unavailable(_))
        ));
        assert_eq!(asks.recv().await.as_deref(), Some("%1"));
        assert_eq!(agents.list(ListParams::default()).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn recipient_ambiguous_rows_is_an_error() {
        let rig = Rig::new().await;
        rig.agent("lead", Some("%1"), None).await;
        rig.row("one", "%1", false).await;
        rig.inspect_only("%1", "p/lead");
        let (agents, _) = rig.start();
        agents.reconciled().await.unwrap();
        // The twin: exactly one eligible row is the recipient's session.
        let recipient = recipient_of(&agents, "lead").await.unwrap();
        assert_eq!(
            recipient.session.map(|row| row.id),
            Some(SessionId::new("one"))
        );
        rig.row("two", "%1", false).await;
        match recipient_of(&agents, "lead").await {
            Err(AgentError::Conflict(reason)) => {
                assert!(
                    reason.contains("more than one eligible session"),
                    "{reason}"
                );
            }
            other => panic!("an ambiguous pane must not read as no session: {other:?}"),
        }
    }

    #[tokio::test]
    async fn recipient_excludes_old_child_and_pending_rows() {
        let rig = Rig::new().await;
        // A cleared agent that is Ready with no session yet: its old session
        // is excluded until a new one binds.
        rig.agent("lead", Some("%1"), Some("old")).await;
        rig.command(OrgCommand::Clear {
            agent_id: "p/lead".parse().unwrap(),
        })
        .await;
        rig.command(OrgCommand::EditorReady {
            agent_id: "p/lead".parse().unwrap(),
        })
        .await;
        for (id, pane, child, harness) in [
            ("old", "%1", false, Harness::Pi),
            ("child", "%1", true, Harness::Pi),
            ("pending-100", "%1", false, Harness::Pi),
            ("other-harness", "%1", false, Harness::Codex),
            ("other-pane", "%2", false, Harness::Pi),
        ] {
            rig.row_of(id, pane, child, harness).await;
        }
        rig.inspect_only("%1", "p/lead");
        let (agents, _) = rig.start();
        agents.reconciled().await.unwrap();
        let recipient = recipient_of(&agents, "lead").await.unwrap();
        assert_eq!(recipient.pane, PaneId::parse("%1").unwrap());
        assert_eq!(recipient.harness, Harness::Pi);
        assert_eq!(recipient.excluded, Some(SessionId::new("old")));
        assert!(recipient.session.is_none(), "{:?}", recipient.session);
        // The twin: a new parent session of the agent's harness on its pane.
        rig.row("new", "%1", false).await;
        let recipient = recipient_of(&agents, "lead").await.unwrap();
        assert_eq!(
            recipient.session.map(|row| row.id),
            Some(SessionId::new("new"))
        );
        assert_eq!(recipient.excluded, Some(SessionId::new("old")));
    }

    #[tokio::test]
    async fn recipient_without_baseline_refuses_every_read() {
        let rig = Rig::new().await;
        rig.agent("lead", Some("%1"), None).await;
        rig.row("s1", "%1", false).await;
        rig.inspect_only("%1", "p/lead");
        let (agents, _) = rig.start();
        agents.reconciled().await.unwrap();
        let id: AgentId = "p/lead".parse().unwrap();
        // A real idle pi editor with pi in the pane's foreground.
        let capture = PaneCapture::new(
            IDLE.into(),
            Rig::inspection("%1", "p/lead", false).with_foreground_process(None, Some("pi".into())),
            CaptureFormat::Ansi,
        );
        let idle =
            atm_pi_adapter::screen::observe_screen(atm_core::ScreenInput::Styled(IDLE), None);
        assert_eq!(idle.input, atm_core::InputState::Empty);

        let recipient = recipient_of(&agents, "lead").await.unwrap();
        assert_eq!(recipient.baseline, PaneBaseline::unproven());
        assert_eq!(
            recipient.screen.facts(&capture, &recipient.baseline),
            atm_core::ScreenFacts::default()
        );
        assert_eq!(
            recipient.screen.staged(&capture, &recipient.baseline, "x"),
            None
        );
        // A baseline recorded for another pane is not this pane's.
        let pinned = PaneBaseline::adopted(100)
            .with_harness("pi".into())
            .unwrap();
        agents.record_baseline(id.clone(), "%9", pinned.clone());
        let recipient = recipient_of(&agents, "lead").await.unwrap();
        assert_eq!(recipient.baseline, PaneBaseline::unproven());
        // The twin: with this pane's baseline the same capture is read.
        agents.record_baseline(id, "%1", pinned.clone());
        let recipient = recipient_of(&agents, "lead").await.unwrap();
        assert_eq!(recipient.baseline, pinned);
        assert_eq!(recipient.screen.facts(&capture, &recipient.baseline), idle);
    }

    #[tokio::test]
    async fn recipient_reports_status_and_recorded_binding() {
        // Review 19, probe r10: an agent that must get no input still has a
        // recipient. What tells them apart is in the same answer.
        for (state, status, bound, excluded) in [
            ("ready", AgentStatus::Ready, Some("s1"), None),
            ("stuck", AgentStatus::Stuck, Some("s1"), None),
            ("starting", AgentStatus::Starting, Some("s1"), None),
            // An interrupted clear: the restart check marks it Stuck.
            ("clearing", AgentStatus::Stuck, None, Some("s1")),
        ] {
            let rig = Rig::new().await;
            rig.agent("lead", Some("%1"), Some("s1")).await;
            rig.row("s1", "%1", false).await;
            let id: AgentId = "p/lead".parse().unwrap();
            match state {
                "stuck" => {
                    rig.command(OrgCommand::Stuck {
                        agent_id: id.clone(),
                        reason: "trust dialog reset".into(),
                    })
                    .await
                }
                "clearing" => {
                    rig.command(OrgCommand::Clear {
                        agent_id: id.clone(),
                    })
                    .await
                }
                "starting" => {
                    rig.command(OrgCommand::Revalidate {
                        agent_id: id.clone(),
                    })
                    .await
                }
                _ => {}
            }
            rig.frame("%1", "p/lead", "unknown\n");
            let (agents, _) = rig.start();
            agents.reconciled().await.unwrap();
            let recipient = agents.recipient(&id).await.unwrap();
            assert_eq!(recipient.status, rig.state("lead").await.status, "{state}");
            if state != "ready" {
                assert_eq!(recipient.status, status, "{state}");
            }
            assert_eq!(recipient.session_id, bound.map(SessionId::new), "{state}");
            assert_eq!(recipient.excluded, excluded.map(SessionId::new), "{state}");
        }
    }

    #[tokio::test]
    async fn recipient_reports_the_panes_row_and_the_binding_when_they_differ() {
        // The agent is recorded as bound to s1; the pane's one eligible row is
        // s2 (the pane is in copy mode, so the observer has not bound it).
        let rig = Rig::new().await;
        rig.agent("lead", Some("%1"), Some("s1")).await;
        rig.row("s2", "%1", false).await;
        rig.frame("%1", "p/lead", IDLE);
        let mut in_mode = Rig::inspection("%1", "p/lead", false);
        in_mode.in_mode = true;
        rig.client
            .script_inspect(&PaneId::parse("%1").unwrap(), Some(&in_mode))
            .unwrap();
        let (agents, _) = rig.start();
        agents.reconciled().await.unwrap();
        let mut events = agents.observation_events();
        rig.events
            .send(crate::registry::SessionEvent::Removed {
                session_id: SessionId::new("unrelated"),
                reason: crate::registry::RemovalReason::Explicit,
            })
            .unwrap();
        progress(&mut events, "idle", None).await;
        assert_eq!(
            rig.client
                .pending_reads(&PaneId::parse("%1").unwrap())
                .unwrap(),
            0
        );
        let recipient = recipient_of(&agents, "lead").await.unwrap();
        assert_eq!(recipient.status, AgentStatus::Ready);
        assert_eq!(recipient.session_id, Some(SessionId::new("s1")));
        assert_eq!(
            recipient.session.map(|row| row.id),
            Some(SessionId::new("s2"))
        );
    }

    #[tokio::test]
    async fn recipient_refuses_what_it_cannot_name() {
        let rig = Rig::new().await;
        rig.agent("lead", None, None).await;
        rig.agent_of("guest", "gemini", Some("%3"), None).await;
        rig.inspect_only("%3", "p/guest");
        let (agents, _) = rig.start();
        agents.reconciled().await.unwrap();
        assert!(matches!(
            recipient_of(&agents, "nobody").await,
            Err(AgentError::NotFound(_))
        ));
        match recipient_of(&agents, "lead").await {
            Err(AgentError::Refused(reason)) => assert!(reason.contains("no pane"), "{reason}"),
            other => panic!("{other:?}"),
        }
        // No adapter reads this harness's screen.
        match recipient_of(&agents, "guest").await {
            Err(AgentError::Refused(reason)) => {
                assert!(reason.contains("no screen reader"), "{reason}");
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            AgentHandle::disabled("off")
                .recipient(&"p/lead".parse().unwrap())
                .await,
            Err(AgentError::Unavailable(_))
        ));
    }

    // Adapted from the A1 review probes: assertions describe the desired behavior.
    async fn progress(
        events: &mut broadcast::Receiver<crate::org::agents::ObservationProgress>,
        phase: &str,
        agent: Option<&str>,
    ) {
        progress_within(events, phase, agent, 30).await;
    }

    // For paused-clock tests whose awaited step is itself a production timeout.
    async fn progress_within(
        events: &mut broadcast::Receiver<crate::org::agents::ObservationProgress>,
        phase: &str,
        agent: Option<&str>,
        seconds: u64,
    ) {
        tokio::time::timeout(std::time::Duration::from_secs(seconds), async {
            loop {
                let event = events.recv().await.unwrap();
                if event.phase == phase
                    && event.agent.as_ref().map(ToString::to_string).as_deref() == agent
                {
                    return;
                }
            }
        })
        .await
        .unwrap();
    }

    // Down is admitted for this agent: the admission alone, with work that does
    // nothing, so the probe does not depend on what `agent.down` does to a pane.
    async fn down_admitted(agents: &AgentHandle, agent: &str, pane: &str) {
        agents
            .lifecycle(
                request(
                    agent,
                    Some(pane),
                    crate::control::reservation::ReservationKind::Down,
                ),
                |_| async { Ok(()) },
                cleanup,
            )
            .await
            .unwrap();
    }

    // One registry event handled by the observer, without a failed state read.
    // The hang guard is the outer timeout; a stalled read fails after 5 s first.
    async fn scheduled_without_reading(
        events: &mut broadcast::Receiver<crate::org::agents::ObservationProgress>,
        context: &str,
    ) {
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                let event = events.recv().await.unwrap();
                assert_ne!(event.phase, "schedule_failed", "{context}");
                if event.phase == "scheduled" {
                    break;
                }
            }
        })
        .await
        .unwrap();
    }

    async fn update(rig: &Rig, id: &str) {
        let row = rig.registry.get_session(SessionId::new(id)).await.unwrap();
        rig.events
            .send(crate::registry::SessionEvent::Updated {
                session: Box::new(row),
            })
            .unwrap();
    }

    #[tokio::test]
    async fn review_runtime_updates_leave_ready_without_writes() {
        let running = include_str!(
            "../../../atm-pi-adapter/tests/fixtures/screen_pi-before-escape_1.5.0.txt"
        );
        for text in [IDLE, running, "unknown\n"] {
            let rig = Rig::new().await;
            rig.agent("lead", Some("%1"), Some("recorded")).await;
            rig.row("recorded", "%1", false).await;
            rig.frame("%1", "p/lead", IDLE);
            let (agents, _) = rig.start();
            agents.reconciled().await.unwrap();
            let mut events = agents.observation_events();
            let before = rig.org.status().unwrap();
            // A whole frame is scripted, so a screen read or a restart-style check
            // on this ordinary wake would consume it, and could write.
            rig.frame("%1", "p/lead", text);
            // Another row on the pane reports in: the agent is observed.
            rig.row("pending-200", "%1", false).await;
            progress(&mut events, "finished", Some("p/lead")).await;
            assert_eq!(rig.state("lead").await.status, AgentStatus::Ready);
            assert_eq!(rig.org.status().unwrap(), before);
            // The inspection (2 reads) is consumed, the capture (3) is not.
            assert_eq!(
                rig.client
                    .pending_reads(&PaneId::parse("%1").unwrap())
                    .unwrap(),
                3
            );
            assert_eq!(
                agents.list(ListParams::default()).await.unwrap()[0].reason,
                None
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn review_ready_agents_own_session_events_start_no_observation() {
        let rig = Rig::new().await;
        rig.agent("lead", Some("%1"), Some("recorded")).await;
        rig.row("recorded", "%1", false).await;
        rig.frame("%1", "p/lead", IDLE);
        let (agents, _) = rig.start();
        agents.reconciled().await.unwrap();
        let mut events = agents.observation_events();
        let before = rig.org.status().unwrap();
        let calls = rig.client.mock().calls().len();
        rig.inspect_only("%1", "p/lead");
        let row = rig
            .registry
            .get_session(SessionId::new("recorded"))
            .await
            .unwrap();
        // A hook or status line of the bound session, and its re-registration:
        // nothing to bind, and the agent is plainly alive.
        for event in [
            crate::registry::SessionEvent::Updated {
                session: Box::new(row.clone()),
            },
            crate::registry::SessionEvent::Registered {
                session_id: row.id.clone(),
                agent_type: AgentType::GeneralPurpose,
            },
        ] {
            rig.events.send(event).unwrap();
            progress(&mut events, "scheduled", None).await;
        }
        // Paused clock: this lets any queued or paced observation run.
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        while let Ok(event) = events.try_recv() {
            assert_ne!(event.phase, "started", "{event:?}");
        }
        assert_eq!(rig.client.mock().calls().len(), calls);
        assert_eq!(rig.org.status().unwrap(), before);
        // The same session leaving its pane, or ending, still wakes the agent.
        let mut moved = row.clone();
        moved.tmux_pane = Some("%9".into());
        for event in [
            crate::registry::SessionEvent::Updated {
                session: Box::new(moved),
            },
            crate::registry::SessionEvent::Removed {
                session_id: row.id.clone(),
                reason: crate::registry::RemovalReason::Explicit,
            },
        ] {
            rig.events.send(event).unwrap();
            progress(&mut events, "finished", Some("p/lead")).await;
            rig.inspect_only("%1", "p/lead");
        }
        assert_eq!(rig.state("lead").await.status, AgentStatus::Ready);
        assert_eq!(rig.org.status().unwrap(), before);
    }

    #[tokio::test]
    async fn review_other_agent_adopt_does_not_touch_ready_unbound() {
        let rig = Rig::new().await;
        rig.agent("lead", Some("%1"), Some("recorded")).await;
        rig.row("recorded", "%1", false).await;
        rig.frame("%1", "p/lead", IDLE);
        let (agents, _) = rig.start();
        agents.reconciled().await.unwrap();
        rig.command(OrgCommand::Clear {
            agent_id: "p/lead".parse().unwrap(),
        })
        .await;
        rig.command(OrgCommand::EditorReady {
            agent_id: "p/lead".parse().unwrap(),
        })
        .await;
        let before = rig.org.status().unwrap();
        let mut events = agents.observation_events();
        rig.frame("%1", "p/lead", IDLE);
        rig.frame("%2", "p/other", IDLE);
        rig.command(OrgCommand::Adopt {
            agent_id: "p/other".parse().unwrap(),
            pane: "%2".into(),
            session_id: Some(SessionId::new("other-row")),
        })
        .await;
        progress(&mut events, "committed_scheduled", None).await;
        progress(&mut events, "idle", None).await;
        assert_eq!(rig.state("lead").await.status, AgentStatus::Ready);
        assert_eq!(rig.state("lead").await.session_id, None);
        assert_eq!(rig.org.status().unwrap(), before + 1);
        // Only the adopted agent was observed: the other pane's scripted
        // inspection and capture are all still unread.
        assert_eq!(
            rig.client
                .pending_reads(&PaneId::parse("%1").unwrap())
                .unwrap(),
            5
        );
    }

    #[tokio::test]
    async fn review_busy_registration_binds_without_idle() {
        let rig = Rig::new().await;
        rig.agent("lead", Some("%1"), None).await;
        rig.row("pending-100", "%1", false).await;
        rig.inspect_only("%1", "p/lead");
        let (agents, _) = rig.start();
        agents.reconciled().await.unwrap();
        let mut events = agents.observation_events();
        // No screen read is scripted: even a busy/unreadable editor cannot lose
        // the identity event. Readiness at delivery remains a separate precheck.
        rig.inspect_only("%1", "p/lead");
        rig.row("fresh", "%1", false).await;
        progress(&mut events, "finished", Some("p/lead")).await;
        assert_eq!(
            rig.state("lead").await.session_id,
            Some(SessionId::new("fresh"))
        );
        assert_eq!(rig.state("lead").await.status, AgentStatus::Ready);
    }

    #[tokio::test]
    async fn review_stuck_binding_recovers_with_same_or_new_session() {
        for session in ["recorded", "fresh"] {
            let rig = Rig::new().await;
            rig.agent("lead", Some("%1"), Some("recorded")).await;
            rig.command(OrgCommand::Stuck {
                agent_id: "p/lead".parse().unwrap(),
                reason: "manual recovery".into(),
            })
            .await;
            rig.client
                .script_inspect(
                    &PaneId::parse("%1").unwrap(),
                    Some(&Rig::inspection("%1", "p/lead", false)),
                )
                .unwrap();
            let (agents, _) = rig.start();
            agents.reconciled().await.unwrap();
            // Stuck since before this daemon started: the observer pins nothing
            // for it, so it has the baseline of the adopt that would precede this.
            Rig::pinned(&agents, "lead", "%1", "pi");
            let mut events = agents.observation_events();
            // Stuck ends only when this observation's own capture shows a safe editor.
            rig.frame("%1", "p/lead", IDLE);
            rig.row(session, "%1", false).await;
            progress(&mut events, "finished", Some("p/lead")).await;
            assert_eq!(
                rig.client
                    .pending_reads(&PaneId::parse("%1").unwrap())
                    .unwrap(),
                0
            );
            assert_eq!(rig.state("lead").await.status, AgentStatus::Ready);
            assert_eq!(
                rig.state("lead").await.session_id,
                Some(SessionId::new(session))
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn review_stuck_needs_a_safe_screen_to_recover() {
        let dialog = include_str!(
            "../../../atm-pi-adapter/tests/fixtures/screen_pi-dialog-confirm_1.5.0.txt"
        )
        .split_once('\n')
        .unwrap()
        .1;
        let running = include_str!(
            "../../../atm-pi-adapter/tests/fixtures/screen_pi-before-escape_1.5.0.txt"
        );
        assert_ne!(
            atm_pi_adapter::screen::observe_screen(atm_core::ScreenInput::Plain(dialog), None)
                .dialog,
            atm_core::DialogState::None
        );
        for bound in [true, false] {
            for unsafe_screen in [dialog, running, "unknown\n"] {
                let rig = Rig::new().await;
                if bound {
                    rig.agent("lead", Some("%1"), Some("s1")).await;
                } else {
                    // An up that failed after the harness registered its session.
                    rig.command(OrgCommand::Up {
                        agent_id: "p/lead".parse().unwrap(),
                        role: atm_org::parse_role("lead", "+++\nharness='pi'\n+++\n").unwrap(),
                    })
                    .await;
                    rig.command(OrgCommand::PaneCreated {
                        agent_id: "p/lead".parse().unwrap(),
                        pane: "%1".into(),
                    })
                    .await;
                }
                rig.row("s1", "%1", false).await;
                let reason = "unexpected screen in %1; inspect the pane";
                rig.command(OrgCommand::Stuck {
                    agent_id: "p/lead".parse().unwrap(),
                    reason: reason.into(),
                })
                .await;
                rig.inspect_only("%1", "p/lead");
                let (agents, _) = rig.start();
                agents.reconciled().await.unwrap();
                // Stuck since before this daemon started: the observer pins nothing
                // for it, so it has the baseline of the adopt that would precede this.
                Rig::pinned(&agents, "lead", "%1", "pi");
                let mut events = agents.observation_events();
                let before = rig.org.status().unwrap();
                // The harness keeps sending hooks while the bad screen is up.
                for _ in 0..2 {
                    rig.frame("%1", "p/lead", unsafe_screen);
                    update(&rig, "s1").await;
                    progress(&mut events, "finished", Some("p/lead")).await;
                    let agent = rig.state("lead").await;
                    assert_eq!(agent.status, AgentStatus::Stuck);
                    assert_eq!(agent.reason.as_deref(), Some(reason));
                    assert_eq!(rig.org.status().unwrap(), before);
                }
                // The owner fixes the pane; the next hook shows a safe editor.
                rig.frame("%1", "p/lead", IDLE);
                update(&rig, "s1").await;
                progress(&mut events, "finished", Some("p/lead")).await;
                let agent = rig.state("lead").await;
                assert_eq!(agent.status, AgentStatus::Ready);
                assert_eq!(agent.reason, None);
                assert_eq!(agent.session_id, Some(SessionId::new("s1")));
                assert_eq!(rig.org.status().unwrap(), before + 1);
                assert_eq!(
                    rig.client
                        .pending_reads(&PaneId::parse("%1").unwrap())
                        .unwrap(),
                    0
                );
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn review_binding_wake_before_a_failed_restart_check_still_binds() {
        let rig = Rig::new().await;
        rig.agent("lead", Some("%1"), None).await;
        // Nothing scripted: the first check fails once.
        let (agents, _) = rig.start();
        agents.reconciled().await.unwrap();
        let mut events = agents.observation_events();
        // One read for the retried restart check, one for the binding after it.
        rig.inspect_only("%1", "p/lead");
        rig.inspect_only("%1", "p/lead");
        // The new session's only event arrives before the check has passed.
        rig.row("fresh", "%1", false).await;
        progress(&mut events, "finished", Some("p/lead")).await;
        progress(&mut events, "finished", Some("p/lead")).await;
        assert_eq!(
            rig.state("lead").await.session_id,
            Some(SessionId::new("fresh"))
        );
        // The pass succeeded: the only reason left is that the scripted
        // inspection names no foreground command.
        assert_eq!(
            agents.list(ListParams::default()).await.unwrap()[0]
                .reason
                .as_deref(),
            Some(NO_COMMAND)
        );
    }

    #[tokio::test]
    async fn review_binding_wake_during_a_suspending_restart_check_still_binds() {
        use crate::control::reservation::ReservationKind;
        let rig = Rig::new().await;
        rig.agent("lead", Some("%1"), Some("old")).await;
        let (entered, entry) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        let control = rig.control.clone();
        let held = tokio::spawn(async move {
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
        });
        entry.await.unwrap();
        let (agents, _) = rig.start();
        let mut events = agents.observation_events();
        progress(&mut events, "started", Some("p/lead")).await;
        // The new session's only event arrives while the restart check waits.
        rig.row("fresh", "%1", false).await;
        progress(&mut events, "scheduled", None).await;
        // The check reads an unknown screen and suspends the agent; that write
        // fences the waiting candidate, so the binding needs a fresh one.
        rig.frame("%1", "p/lead", "unknown\n");
        rig.inspect_only("%1", "p/lead");
        release.send(()).unwrap();
        held.await.unwrap().unwrap();
        progress(&mut events, "finished", Some("p/lead")).await;
        progress(&mut events, "finished", Some("p/lead")).await;
        let agent = rig.state("lead").await;
        assert_eq!(agent.status, AgentStatus::Ready);
        assert_eq!(agent.session_id, Some(SessionId::new("fresh")));
        assert_eq!(
            rig.client
                .pending_reads(&PaneId::parse("%1").unwrap())
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn review_updated_only_wake_binds_current_row() {
        let rig = Rig::new().await;
        rig.agent("lead", Some("%1"), None).await;
        // Known to the registry before the service starts, but on no pane yet:
        // nothing to bind at the restart check, and no Registered wake later.
        let mut row = SessionDomain::new(
            SessionId::new("fresh"),
            AgentType::GeneralPurpose,
            Model::default(),
        );
        row.harness = Harness::Pi;
        rig.registry.register(row).await.unwrap();
        rig.inspect_only("%1", "p/lead");
        let (agents, _) = rig.start();
        agents.reconciled().await.unwrap();
        assert_eq!(rig.state("lead").await.session_id, None);
        let mut events = agents.observation_events();
        rig.inspect_only("%1", "p/lead");
        // Its first hook carries the pane: one Updated and nothing else.
        rig.registry
            .apply_lifecycle_event(
                SessionId::new("fresh"),
                atm_core::LifecycleEvent::WorkingStart,
                Harness::Pi,
                None,
                Some("%1".into()),
            )
            .await
            .unwrap();
        progress(&mut events, "finished", Some("p/lead")).await;
        assert_eq!(
            rig.state("lead").await.session_id,
            Some(SessionId::new("fresh"))
        );
        assert_eq!(rig.state("lead").await.status, AgentStatus::Ready);
    }

    #[tokio::test]
    async fn review_removed_or_moved_unbound_rows_wake_their_former_agent() {
        for moved in [false, true] {
            let rig = Rig::new().await;
            rig.agent("lead", Some("%1"), None).await;
            rig.row("pending-100", "%1", false).await;
            rig.inspect_only("%1", "p/lead");
            let (agents, _) = rig.start();
            agents.reconciled().await.unwrap();
            let before = rig.org.status().unwrap();
            let mut events = agents.observation_events();
            rig.inspect_only("%1", "p/lead");
            let mut row = rig
                .registry
                .get_session(SessionId::new("pending-100"))
                .await
                .unwrap();
            // Neither payload names the agent's pane or a bound session: only the
            // routing index's memory of the row's former pane can wake the agent.
            let event = if moved {
                row.tmux_pane = Some("%9".into());
                crate::registry::SessionEvent::Updated {
                    session: Box::new(row),
                }
            } else {
                crate::registry::SessionEvent::Removed {
                    session_id: row.id,
                    reason: crate::registry::RemovalReason::Explicit,
                }
            };
            rig.events.send(event).unwrap();
            progress(&mut events, "finished", Some("p/lead")).await;
            assert_eq!(rig.state("lead").await.status, AgentStatus::Ready);
            assert_eq!(rig.org.status().unwrap(), before);
            assert_eq!(
                rig.client
                    .pending_reads(&PaneId::parse("%1").unwrap())
                    .unwrap(),
                0
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn review_initial_observation_failure_is_agent_local() {
        let rig = Rig::new().await;
        for (name, pane) in [("good", "%1"), ("bad", "%2")] {
            rig.agent(name, Some(pane), Some(name)).await;
            rig.row(name, pane, false).await;
        }
        rig.frame("%1", "p/good", IDLE);
        rig.client
            .script_inspect(
                &PaneId::parse("%2").unwrap(),
                Some(&Rig::inspection("%2", "p/bad", false)),
            )
            .unwrap();
        let (agents, _) = rig.start();
        agents.reconciled().await.unwrap();
        let rows = agents.list(ListParams::default()).await.unwrap();
        assert!(rows
            .iter()
            .find(|a| a.agent_id.to_string() == "p/bad")
            .unwrap()
            .reason
            .is_some());
        assert_eq!(rig.state("bad").await.status, AgentStatus::Ready);
        down_admitted(&agents, "p/good", "%1").await;
        // Down inspects the pane itself and is admitted; other work for the failed
        // agent waits for its restart check, and the healthy agent is untouched.
        down_admitted(&agents, "p/bad", "%2").await;
        for (agent, pane, admitted) in [("p/bad", "%2", false), ("p/good", "%1", true)] {
            let result = agents
                .lifecycle(
                    request(
                        agent,
                        Some(pane),
                        crate::control::reservation::ReservationKind::Clear,
                    ),
                    |_| async { Ok(()) },
                    cleanup,
                )
                .await;
            assert_eq!(result.is_ok(), admitted, "{agent}: {result:?}");
            assert!(admitted || matches!(result, Err(AgentError::Unavailable(_))));
        }
        // The failed check is retried by itself (paused clock), with no registry event.
        let mut events = agents.observation_events();
        rig.frame("%2", "p/bad", IDLE);
        progress(&mut events, "finished", Some("p/bad")).await;
        assert!(agents
            .list(ListParams::default())
            .await
            .unwrap()
            .iter()
            .all(|a| a.reason.is_none()));
    }

    #[tokio::test(start_paused = true)]
    async fn review_runtime_observe_timeout_is_retried_and_never_blocks_lifecycle() {
        use crate::control::reservation::ReservationKind;
        let rig = Rig::new().await;
        rig.agent("lead", Some("%1"), Some("recorded")).await;
        rig.row("recorded", "%1", false).await;
        rig.frame("%1", "p/lead", IDLE);
        let (agents, _) = rig.start();
        agents.reconciled().await.unwrap();
        let mut events = agents.observation_events();
        // A long lifecycle job holds the agent while another row reports in.
        let (entered, entry) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        let control = rig.control.clone();
        let held = tokio::spawn(async move {
            control
                .run_reserved(
                    request("p/lead", Some("%1"), ReservationKind::Clear),
                    move |_| async move {
                        entered.send(()).unwrap();
                        released.await.unwrap();
                        Ok::<_, AgentError>(())
                    },
                    cleanup,
                )
                .await
        });
        entry.await.unwrap();
        rig.row("pending-200", "%1", false).await;
        progress(&mut events, "started", Some("p/lead")).await;
        // The Observe gives up after its own 30 s budget, on the paused clock.
        progress_within(&mut events, "finished", Some("p/lead"), 600).await;
        release.send(()).unwrap();
        held.await.unwrap().unwrap();
        assert_eq!(rig.state("lead").await.status, AgentStatus::Ready);
        assert!(agents.list(ListParams::default()).await.unwrap()[0]
            .reason
            .as_deref()
            .unwrap()
            .contains("observation unavailable"));
        // A runtime failure of a checked agent refuses nothing.
        agents
            .lifecycle(
                request("p/lead", Some("%1"), ReservationKind::Clear),
                |_| async { Ok(()) },
                cleanup,
            )
            .await
            .unwrap();
        down_admitted(&agents, "p/lead", "%1").await;
        // The retry needs no registry event.
        rig.inspect_only("%1", "p/lead");
        progress(&mut events, "finished", Some("p/lead")).await;
        // The pass succeeded: the only reason left is that the scripted
        // inspection names no foreground command.
        assert_eq!(
            agents.list(ListParams::default()).await.unwrap()[0]
                .reason
                .as_deref(),
            Some(NO_COMMAND)
        );
        let calls = rig.client.mock().calls().len();
        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        assert_eq!(rig.client.mock().calls().len(), calls);
    }

    #[tokio::test(start_paused = true)]
    async fn review_failed_first_check_is_retried_and_admits_down_and_respawn() {
        use crate::control::reservation::ReservationKind;
        let rig = Rig::new().await;
        rig.agent("lead", Some("%1"), Some("recorded")).await;
        rig.row("recorded", "%1", false).await;
        // Nothing scripted: tmux does not answer the first check.
        let (agents, _) = rig.start();
        agents.reconciled().await.unwrap();
        let mut events = agents.observation_events();
        assert!(agents.list(ListParams::default()).await.unwrap()[0]
            .reason
            .as_deref()
            .unwrap()
            .contains("observation unavailable"));
        // Down and respawn inspect the pane themselves and are never refused.
        down_admitted(&agents, "p/lead", "%1").await;
        agents
            .lifecycle(
                request("p/lead", Some("%1"), ReservationKind::Respawn),
                |_| async { Ok(()) },
                cleanup,
            )
            .await
            .unwrap();
        // Other lifecycle work waits for the restart check.
        assert!(matches!(
            agents
                .lifecycle(
                    request("p/lead", Some("%1"), ReservationKind::Clear),
                    |_| async { Ok(()) },
                    cleanup,
                )
                .await,
            Err(AgentError::Unavailable(_))
        ));
        // Retried by itself, with a backoff that stops growing at one minute:
        // 1, 2, 4, 8, 16 and 32 s, then every 60 s.
        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        let mut retries = 0;
        while let Ok(event) = events.try_recv() {
            retries += usize::from(event.phase == "started");
        }
        assert!((55..=70).contains(&retries), "{retries} retries in an hour");
        // tmux answers again: the next retry completes the restart check.
        rig.frame("%1", "p/lead", IDLE);
        progress_within(&mut events, "finished", Some("p/lead"), 120).await;
        assert_eq!(
            agents.list(ListParams::default()).await.unwrap()[0].reason,
            None
        );
        agents
            .lifecycle(
                request("p/lead", Some("%1"), ReservationKind::Clear),
                |_| async { Ok(()) },
                cleanup,
            )
            .await
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn review_interrupted_clear_recovers_after_a_failed_first_check() {
        let rig = Rig::new().await;
        rig.agent("lead", Some("%1"), Some("old")).await;
        rig.command(OrgCommand::Clear {
            agent_id: "p/lead".parse().unwrap(),
        })
        .await;
        // The harness still runs the excluded old session: its events wake nobody.
        rig.row("old", "%1", false).await;
        let (agents, _) = rig.start();
        agents.reconciled().await.unwrap();
        assert_eq!(rig.state("lead").await.status, AgentStatus::Clearing);
        let mut events = agents.observation_events();
        rig.inspect_only("%1", "p/lead");
        progress(&mut events, "finished", Some("p/lead")).await;
        let agent = rig.state("lead").await;
        assert_eq!(agent.status, AgentStatus::Stuck);
        assert_eq!(
            agent.reason.as_deref(),
            Some("clear interrupted by daemon restart; inspect %1")
        );
        assert_eq!(rig.org.snapshot().await.unwrap().messages().count(), 1);
        down_admitted(&agents, "p/lead", "%1").await;
    }

    #[tokio::test(start_paused = true)]
    async fn review_transient_schedule_failure_does_not_stop_service() {
        let (gate, mut entered) = mpsc::channel(1);
        let rig = Rig::gated(Some(("agent.session_lost", gate))).await;
        rig.agent("lead", Some("%1"), Some("recorded")).await;
        rig.row("recorded", "%1", false).await;
        // A Ready agent's own session no longer wakes it; another row on its pane does.
        rig.row("pending-100", "%1", false).await;
        rig.frame("%1", "p/lead", IDLE);
        let (agents, _) = rig.start();
        agents.reconciled().await.unwrap();
        let mut events = agents.observation_events();
        let org = rig.org.clone();
        let held = tokio::spawn(async move {
            org.execute(
                OrgCommand::SessionLost {
                    agent_id: "p/lead".parse().unwrap(),
                },
                chrono::Utc::now(),
            )
            .await
        });
        let release = entered.recv().await.unwrap();
        update(&rig, "pending-100").await;
        progress(&mut events, "scheduling", None).await;
        tokio::time::advance(std::time::Duration::from_secs(6)).await;
        progress(&mut events, "schedule_failed", None).await;
        release.send(()).unwrap();
        let _ = held.await.unwrap();
        rig.org.snapshot().await.unwrap();
        assert!(agents.list(ListParams::default()).await.is_ok());
        agents
            .lifecycle(
                request(
                    "p/other",
                    None,
                    crate::control::reservation::ReservationKind::Down,
                ),
                |_| async { Ok(()) },
                cleanup,
            )
            .await
            .unwrap();
        // The filesystem lane must survive too; resume real time before I/O.
        tokio::time::resume();
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".atm/roles")).unwrap();
        std::fs::write(
            dir.path().join(".atm/roles/lead.md"),
            "+++\nharness='pi'\n+++\n",
        )
        .unwrap();
        // The role is read, and the start then finds no pane to split: refused
        // with nothing recorded.
        rig.client
            .script_inspect(&PaneId::parse("%0").unwrap(), None)
            .unwrap();
        assert!(matches!(
            agents
                .up(
                    UpParams {
                        project_root: dir.path().into(),
                        name: "lead".into(),
                        caller_server: None,
                    },
                    caller(),
                )
                .await,
            Err(AgentError::Refused(_))
        ));
        assert!(dir.path().join(".atm/project.toml").is_file());
        assert!(agents
            .list(ListParams {
                project_root: Some(dir.path().into())
            })
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn review_unbound_clear_invalidates_parked_binding() {
        use crate::control::reservation::ReservationKind;
        let rig = Rig::new().await;
        rig.agent("lead", Some("%1"), Some("s1")).await;
        rig.command(OrgCommand::Clear {
            agent_id: "p/lead".parse().unwrap(),
        })
        .await;
        rig.command(OrgCommand::EditorReady {
            agent_id: "p/lead".parse().unwrap(),
        })
        .await;
        rig.row("pending-100", "%1", false).await;
        rig.inspect_only("%1", "p/lead");
        let (agents, _) = rig.start();
        agents.reconciled().await.unwrap();
        let mut events = agents.observation_events();
        let (entered, entry) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        let control = rig.control.clone();
        let held = tokio::spawn(async move {
            control
                .run_reserved(
                    request("p/lead", Some("%1"), ReservationKind::Clear),
                    move |_| async move {
                        entered.send(()).unwrap();
                        released.await.unwrap();
                        Ok(())
                    },
                    cleanup,
                )
                .await
                .unwrap();
        });
        entry.await.unwrap();
        rig.row("r2", "%1", false).await;
        progress(&mut events, "started", Some("p/lead")).await;
        // Same pane/binding/exclusion before and after this clear.
        rig.command(OrgCommand::Clear {
            agent_id: "p/lead".parse().unwrap(),
        })
        .await;
        rig.command(OrgCommand::EditorReady {
            agent_id: "p/lead".parse().unwrap(),
        })
        .await;
        rig.inspect_only("%1", "p/lead");
        release.send(()).unwrap();
        held.await.unwrap();
        progress(&mut events, "finished", Some("p/lead")).await;
        assert_eq!(rig.state("lead").await.session_id, None);
        assert_eq!(
            rig.state("lead").await.excluded_session_id,
            Some(SessionId::new("s1"))
        );
        // The observation itself succeeded: the fence refused the binding, not a failed read.
        assert_eq!(
            rig.client
                .pending_reads(&PaneId::parse("%1").unwrap())
                .unwrap(),
            0
        );
        // The pass succeeded: the only reason left is that the scripted
        // inspection names no foreground command.
        assert_eq!(
            agents.list(ListParams::default()).await.unwrap()[0]
                .reason
                .as_deref(),
            Some(NO_COMMAND)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn review_irrelevant_wakes_do_not_read_busy_org() {
        for has_agent in [false, true] {
            let (gate, mut entered) = mpsc::channel(1);
            let rig = Rig::gated(Some(("control.sent", gate))).await;
            if has_agent {
                rig.agent("lead", Some("%1"), Some("recorded")).await;
                rig.row("recorded", "%1", false).await;
                rig.frame("%1", "p/lead", IDLE);
            }
            let (agents, _) = rig.start();
            agents.reconciled().await.unwrap();
            let mut events = agents.observation_events();
            let org = rig.org.clone();
            let held = tokio::spawn(async move {
                org.execute(
                    OrgCommand::ControlSent(atm_org::ControlAudit {
                        pane: "%99".into(),
                        action: json!({}),
                    }),
                    chrono::Utc::now(),
                )
                .await
            });
            let release = entered.recv().await.unwrap();
            rig.events
                .send(crate::registry::SessionEvent::Removed {
                    session_id: SessionId::new("unrelated"),
                    reason: crate::registry::RemovalReason::Explicit,
                })
                .unwrap();
            scheduled_without_reading(&mut events, "unrelated wake read stalled org").await;
            for registered in [true, false] {
                if registered {
                    rig.row("unrelated", "%99", false).await;
                } else {
                    update(&rig, "unrelated").await;
                }
                scheduled_without_reading(
                    &mut events,
                    "unrelated registration/update read stalled org",
                )
                .await;
            }
            release.send(()).unwrap();
            held.await.unwrap().unwrap();
        }
    }

    #[tokio::test(start_paused = true)]
    async fn review_ineligible_owned_pane_rows_do_not_read_busy_org() {
        for (id, child, harness) in [
            ("child", true, Harness::Pi),
            ("old", false, Harness::Pi),
            ("foreign", false, Harness::ClaudeCode),
        ] {
            let (gate, mut entered) = mpsc::channel(1);
            let rig = Rig::gated(Some(("control.sent", gate))).await;
            rig.agent("lead", Some("%1"), Some("old")).await;
            rig.command(OrgCommand::Clear {
                agent_id: "p/lead".parse().unwrap(),
            })
            .await;
            rig.command(OrgCommand::EditorReady {
                agent_id: "p/lead".parse().unwrap(),
            })
            .await;
            rig.frame("%1", "p/lead", IDLE);
            let (agents, _) = rig.start();
            agents.reconciled().await.unwrap();
            let mut events = agents.observation_events();
            let org = rig.org.clone();
            let held = tokio::spawn(async move {
                org.execute(
                    OrgCommand::ControlSent(atm_org::ControlAudit {
                        pane: "%99".into(),
                        action: json!({}),
                    }),
                    chrono::Utc::now(),
                )
                .await
            });
            let release = entered.recv().await.unwrap();
            let mut row = SessionDomain::new(
                SessionId::new(id),
                AgentType::GeneralPurpose,
                Model::default(),
            );
            row.harness = harness;
            row.tmux_pane = Some("%1".into());
            row.parent_session_id = child.then(|| SessionId::new("parent"));
            rig.registry.register(row).await.unwrap();
            scheduled_without_reading(
                &mut events,
                &format!("ineligible {id} row read stalled org"),
            )
            .await;
            release.send(()).unwrap();
            held.await.unwrap().unwrap();
        }
    }

    #[tokio::test(start_paused = true)]
    async fn review_captures_have_minimum_interval_without_idle_polling() {
        let rig = Rig::new().await;
        rig.agent("lead", Some("%1"), Some("recorded")).await;
        rig.row("recorded", "%1", false).await;
        rig.frame("%1", "p/lead", "unknown\n");
        let (agents, _) = rig.start();
        agents.reconciled().await.unwrap();
        let mut events = agents.observation_events();
        for _ in 0..2 {
            let before = tokio::time::Instant::now();
            rig.frame("%1", "p/lead", "unknown\n");
            update(&rig, "recorded").await;
            progress(&mut events, "capture", Some("p/lead")).await;
            assert!(tokio::time::Instant::now() >= before + std::time::Duration::from_millis(50));
            progress(&mut events, "finished", Some("p/lead")).await;
        }
        let calls = rig.client.mock().calls().len();
        tokio::time::advance(std::time::Duration::from_secs(3600)).await;
        agents.list(ListParams::default()).await.unwrap();
        assert_eq!(rig.client.mock().calls().len(), calls);
    }

    #[tokio::test]
    async fn review_observer_leaves_reservation_pool_headroom() {
        use crate::control::reservation::ReservationKind;
        let rig = Rig::new().await;
        let mut releases = Vec::new();
        let mut held = Vec::new();
        for n in 1..=10 {
            let name = format!("a{n}");
            let pane = format!("%{n}");
            rig.agent(&name, Some(&pane), None).await;
            rig.client
                .script_inspect(&PaneId::parse(&pane).unwrap(), None)
                .unwrap();
            let (entered, entry) = tokio::sync::oneshot::channel();
            let (release, released) = tokio::sync::oneshot::channel();
            releases.push(release);
            let control = rig.control.clone();
            held.push(tokio::spawn(async move {
                control
                    .run_reserved(
                        request(&format!("p/{name}"), Some(&pane), ReservationKind::Delivery),
                        move |_| async move {
                            entered.send(()).unwrap();
                            released.await.unwrap();
                            Ok(())
                        },
                        cleanup,
                    )
                    .await
                    .unwrap();
            }));
            entry.await.unwrap();
        }
        let (agents, _) = rig.start();
        let mut events = agents.observation_events();
        let attempts = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            let mut attempts = 0;
            loop {
                let event = events.recv().await.unwrap();
                if event.phase == "started" {
                    attempts += 1;
                }
                if event.phase == "dispatched" {
                    return attempts;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(attempts, 8);
        rig.control
            .run_reserved(
                request("p/other", None, ReservationKind::Down),
                |_| async { Ok::<_, AgentError>(()) },
                cleanup,
            )
            .await
            .unwrap();
        for release in releases {
            release.send(()).unwrap();
        }
        for job in held {
            job.await.unwrap();
        }
        agents.reconciled().await.unwrap();
        assert!(agents
            .list(ListParams::default())
            .await
            .unwrap()
            .iter()
            .all(|a| a.state == AgentStatus::Down));
    }

    #[tokio::test]
    async fn daemon_wires_agents_without_waiting_for_initial_reconciliation() {
        use std::{
            future::{poll_fn, Future},
            task::Poll,
        };
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let rig = Rig::new().await;
        rig.agent("lead", Some("%1"), None).await;
        rig.client
            .script_inspect(&PaneId::parse("%1").unwrap(), None)
            .unwrap();
        let (entered, entry) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        let control = rig.control.clone();
        let delivery = tokio::spawn(async move {
            control
                .run_reserved(
                    request(
                        "p/lead",
                        Some("%1"),
                        crate::control::reservation::ReservationKind::Delivery,
                    ),
                    move |_| async move {
                        entered.send(()).unwrap();
                        released.await.unwrap();
                        Ok(())
                    },
                    cleanup,
                )
                .await
                .unwrap();
        });
        entry.await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("agents.sock");
        let cancel = CancellationToken::new();
        let mut services = MethodServices::new(rig.registry.clone());
        services.org = rig.org.clone();
        services.control = Some(rig.control.clone());
        let server = crate::server::DaemonServer::with_services(&socket, services, cancel.clone());
        let mut run = Box::pin(async move { server.run().await });
        poll_fn(|cx| {
            assert!(run.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        let task = tokio::spawn(run);
        let stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
        let (read, mut write) = stream.into_split();
        let mut read = BufReader::new(read);
        write.write_all(b"{\"protocol_version\":{\"major\":1,\"minor\":0},\"type\":\"connect\",\"client_id\":\"agents-test\"}\n").await.unwrap();
        let mut line = String::new();
        read.read_line(&mut line).await.unwrap();
        write.write_all(b"{\"protocol_version\":{\"major\":1,\"minor\":1},\"type\":\"request\",\"id\":\"list\",\"method\":\"agent.list\",\"params\":{}}\n{\"protocol_version\":{\"major\":1,\"minor\":0},\"type\":\"ping\",\"seq\":1}\n").await.unwrap();
        line.clear();
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            read.read_line(&mut line),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&line).unwrap()["type"],
            "pong"
        );
        release.send(()).unwrap();
        delivery.await.unwrap();
        line.clear();
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            read.read_line(&mut line),
        )
        .await
        .unwrap()
        .unwrap();
        let result: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(result["result"][0]["state"], "down", "{result}");
        cancel.cancel();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn first_list_waits_for_initial_inspection_and_preserves_pending_binding() {
        let rig = Rig::new().await;
        rig.agent("lead", Some("%1"), Some("recorded")).await;
        rig.row("pending-100", "%1", false).await;
        rig.frame("%1", "p/lead", IDLE);
        let (agents, _) = rig.start();
        let rows = agents.list(ListParams::default()).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, AgentStatus::Ready);
        assert_eq!(rows[0].session_id, Some(SessionId::new("recorded")));
        assert!(rows[0].activity.is_none());
        assert_eq!(
            rig.client
                .pending_reads(&PaneId::parse("%1").unwrap())
                .unwrap(),
            0
        );
    }

    #[tokio::test(start_paused = true)]
    async fn unknown_or_missing_row_stays_starting_without_erasing_binding() {
        for (row, text) in [(true, "unknown\n"), (false, IDLE)] {
            let rig = Rig::new().await;
            rig.agent("lead", Some("%1"), Some("recorded")).await;
            if row {
                rig.row("recorded", "%1", false).await;
            }
            rig.frame("%1", "p/lead", text);
            let (agents, _) = rig.start();
            agents.reconciled().await.unwrap();
            let agent = rig.state("lead").await;
            assert_eq!(agent.status, AgentStatus::Starting);
            assert_eq!(agent.session_id, Some(SessionId::new("recorded")));
            let cursor = rig.org.status().unwrap();
            tokio::time::advance(std::time::Duration::from_secs(3600)).await;
            assert_eq!(
                agents.list(ListParams::default()).await.unwrap()[0].state,
                AgentStatus::Starting
            );
            assert_eq!(rig.org.status().unwrap(), cursor);
        }
    }

    #[tokio::test]
    async fn absent_dead_and_reused_panes_are_down_without_input() {
        let rig = Rig::new().await;
        for (name, pane) in [("absent", "%1"), ("dead", "%2"), ("reused", "%3")] {
            rig.agent(name, Some(pane), None).await;
        }
        rig.agent("nopane", None, None).await;
        rig.client
            .script_inspect(&PaneId::parse("%1").unwrap(), None)
            .unwrap();
        rig.client
            .script_inspect(
                &PaneId::parse("%2").unwrap(),
                Some(&Rig::inspection("%2", "p/dead", true)),
            )
            .unwrap();
        rig.client
            .script_inspect(
                &PaneId::parse("%3").unwrap(),
                Some(&Rig::inspection("%3", "foreign/agent", false)),
            )
            .unwrap();
        let (agents, _) = rig.start();
        let rows = agents.list(ListParams::default()).await.unwrap();
        assert_eq!(
            rows.iter()
                .map(|r| r.agent_id.to_string())
                .collect::<Vec<_>>(),
            ["p/absent", "p/dead", "p/nopane", "p/reused"]
        );
        assert!(rows.iter().all(|r| r.state == AgentStatus::Down));
        assert!(rig
            .client
            .mock()
            .calls()
            .iter()
            .all(|call| matches!(call, atm_tmux::mock::MockCall::RawCommand { .. })));
    }

    #[tokio::test]
    async fn clearing_excludes_old_child_and_pending_rows() {
        let rig = Rig::new().await;
        rig.agent("lead", Some("%1"), Some("old")).await;
        rig.command(OrgCommand::Clear {
            agent_id: "p/lead".parse().unwrap(),
        })
        .await;
        for (id, child) in [("old", false), ("child", true), ("pending-100", false)] {
            rig.row(id, "%1", child).await;
        }
        rig.client
            .script_inspect(
                &PaneId::parse("%1").unwrap(),
                Some(&Rig::inspection("%1", "p/lead", false)),
            )
            .unwrap();
        let (agents, _) = rig.start();
        let rows = agents.list(ListParams::default()).await.unwrap();
        assert_eq!(rows[0].state, AgentStatus::Stuck);
        assert_eq!(
            rows[0].reason.as_deref(),
            Some("clear interrupted by daemon restart; inspect %1")
        );
        let state = rig.org.snapshot().await.unwrap();
        let notes: Vec<_> = state.messages().collect();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].content.from, atm_org::Address::System);
        assert_eq!(notes[0].content.to, atm_org::Address::Owner);
        assert_eq!(notes[0].content.kind, atm_org::MessageKind::Note);
        assert!(notes[0].content.body.contains("p/lead"));
        assert!(rows[0].session_id.is_none());
        assert!(rows[0].activity.is_none());
        assert_eq!(
            rig.state("lead").await.excluded_session_id,
            Some(SessionId::new("old"))
        );
    }

    #[tokio::test]
    async fn review_restart_clear_note_repairs_partial_prefix_without_duplicates() {
        for partial in [false, true] {
            let rig = Rig::new().await;
            rig.agent("lead", Some("%1"), Some("old")).await;
            rig.command(OrgCommand::Clear {
                agent_id: "p/lead".parse().unwrap(),
            })
            .await;
            if partial {
                // Durable prefix left by a crash/failure after Stuck, before Note.
                rig.command(OrgCommand::Stuck {
                    agent_id: "p/lead".parse().unwrap(),
                    reason: "clear interrupted by daemon restart; inspect %1".into(),
                })
                .await;
            }
            for _ in 0..2 {
                rig.inspect_only("%1", "p/lead");
                let cancel = rig.cancel.child_token();
                let (agents, task) = spawn_agents_with(
                    rig.org.clone(),
                    rig.registry.clone(),
                    rig.control.clone(),
                    cancel.clone(),
                    rig.reader(),
                    std::sync::Arc::new(|_| None),
                );
                agents.reconciled().await.unwrap();
                let state = rig.org.snapshot().await.unwrap();
                assert_eq!(
                    state.agent(&"p/lead".parse().unwrap()).unwrap().status,
                    AgentStatus::Stuck
                );
                assert_eq!(state.messages().count(), 1);
                cancel.cancel();
                task.await.unwrap();
            }
        }
    }

    #[tokio::test]
    async fn review_second_interrupted_clear_sends_its_own_note() {
        let rig = Rig::new().await;
        rig.agent("lead", Some("%1"), Some("s1")).await;
        for round in 1..=2 {
            rig.command(OrgCommand::Clear {
                agent_id: "p/lead".parse().unwrap(),
            })
            .await;
            // Two restarts per incident: the second must not repeat the Note.
            for _ in 0..2 {
                rig.inspect_only("%1", "p/lead");
                let cancel = rig.cancel.child_token();
                let (agents, task) = spawn_agents_with(
                    rig.org.clone(),
                    rig.registry.clone(),
                    rig.control.clone(),
                    cancel.clone(),
                    rig.reader(),
                    std::sync::Arc::new(|_| None),
                );
                agents.reconciled().await.unwrap();
                assert_eq!(rig.state("lead").await.status, AgentStatus::Stuck);
                assert_eq!(rig.org.snapshot().await.unwrap().messages().count(), round);
                cancel.cancel();
                task.await.unwrap();
            }
            // The owner fixes the pane: a new session binds and the agent is Ready.
            rig.command(OrgCommand::SessionBound {
                agent_id: "p/lead".parse().unwrap(),
                session_id: SessionId::new(format!("s{}", round + 1)),
            })
            .await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn review_failed_restart_note_retries_on_next_wake() {
        let rig = Rig::gated_with_failure(None, Some("msg.sent")).await;
        rig.agent("lead", Some("%1"), Some("old")).await;
        rig.command(OrgCommand::Clear {
            agent_id: "p/lead".parse().unwrap(),
        })
        .await;
        rig.row("pending-100", "%1", false).await;
        rig.inspect_only("%1", "p/lead");
        let (agents, _) = rig.start();
        agents.reconciled().await.unwrap();
        assert_eq!(rig.state("lead").await.status, AgentStatus::Stuck);
        assert_eq!(rig.org.snapshot().await.unwrap().messages().count(), 0);
        assert!(agents.list(ListParams::default()).await.unwrap()[0]
            .reason
            .as_deref()
            .unwrap()
            .contains("observation unavailable"));
        let mut events = agents.observation_events();
        // The first recovery is the observer's own retry (paused clock); the
        // second pass is an ordinary wake and must not repeat the Note.
        for wake in [false, true] {
            rig.inspect_only("%1", "p/lead");
            if wake {
                update(&rig, "pending-100").await;
            }
            progress(&mut events, "finished", Some("p/lead")).await;
            assert_eq!(rig.org.snapshot().await.unwrap().messages().count(), 1);
            assert_eq!(
                agents.list(ListParams::default()).await.unwrap()[0]
                    .reason
                    .as_deref(),
                Some("clear interrupted by daemon restart; inspect %1")
            );
        }
        assert_eq!(rig.state("lead").await.session_id, None);
    }

    #[tokio::test]
    async fn review_runtime_clearing_is_not_restart_failure() {
        let rig = Rig::new().await;
        rig.agent("lead", Some("%1"), Some("old")).await;
        rig.row("old", "%1", false).await;
        rig.frame("%1", "p/lead", IDLE);
        let (agents, _) = rig.start();
        agents.reconciled().await.unwrap();
        let mut events = agents.observation_events();
        rig.command(OrgCommand::Clear {
            agent_id: "p/lead".parse().unwrap(),
        })
        .await;
        rig.inspect_only("%1", "p/lead");
        rig.row("fresh", "%1", false).await;
        progress(&mut events, "finished", Some("p/lead")).await;
        assert_eq!(rig.state("lead").await.status, AgentStatus::Clearing);
        assert_eq!(rig.org.snapshot().await.unwrap().messages().count(), 0);
    }

    #[tokio::test]
    async fn review_missing_stuck_pane_preserves_original_diagnostic() {
        let rig = Rig::new().await;
        rig.agent("lead", Some("%1"), Some("old")).await;
        rig.command(OrgCommand::Stuck {
            agent_id: "p/lead".parse().unwrap(),
            reason: "original clear failure".into(),
        })
        .await;
        rig.client
            .script_inspect(&PaneId::parse("%1").unwrap(), None)
            .unwrap();
        let (agents, _) = rig.start();
        agents.reconciled().await.unwrap();
        let agent = rig.state("lead").await;
        assert_eq!(agent.status, AgentStatus::Down);
        assert!(agent.reason.unwrap().contains("original clear failure"));
    }

    #[tokio::test]
    async fn excluded_and_child_rows_are_not_liveness() {
        for child in [false, true] {
            let rig = Rig::new().await;
            rig.agent("lead", Some("%1"), Some("recorded")).await;
            if !child {
                rig.command(OrgCommand::Clear {
                    agent_id: "p/lead".parse().unwrap(),
                })
                .await;
                rig.command(OrgCommand::EditorReady {
                    agent_id: "p/lead".parse().unwrap(),
                })
                .await;
            }
            rig.row("recorded", "%1", child).await;
            rig.frame("%1", "p/lead", IDLE);
            let (agents, _) = rig.start();
            let rows = agents.list(ListParams::default()).await.unwrap();
            assert_eq!(
                rows[0].state,
                if child {
                    AgentStatus::Starting
                } else {
                    AgentStatus::Ready
                }
            );
            assert!(rows[0].activity.is_none());
        }
    }

    #[tokio::test]
    async fn busy_restart_recovers_on_event_and_new_real_row_binds() {
        let rig = Rig::new().await;
        rig.agent("lead", Some("%1"), Some("recorded")).await;
        rig.row("pending-100", "%1", false).await;
        let running = include_str!(
            "../../../atm-pi-adapter/tests/fixtures/screen_pi-before-escape_1.5.0.txt"
        );
        assert_eq!(
            atm_pi_adapter::screen::observe_screen(atm_core::ScreenInput::Plain(running), None)
                .turn,
            atm_core::TurnState::Running
        );
        rig.frame("%1", "p/lead", running);
        let (agents, _) = rig.start();
        agents.reconciled().await.unwrap();
        assert_eq!(rig.state("lead").await.status, AgentStatus::Starting);
        let mut events = rig.org.committed.subscribe();
        rig.inspect_only("%1", "p/lead");
        rig.row("new-real", "%1", false).await;
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                if events.recv().await.unwrap().kind == "agent.session_bound" {
                    break;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(
            rig.state("lead").await.session_id,
            Some(SessionId::new("new-real"))
        );
        assert_eq!(rig.state("lead").await.status, AgentStatus::Ready);
    }

    #[tokio::test(start_paused = true)]
    async fn ready_append_unknown_quarantines_before_release() {
        use crate::control::reservation::{ReservationError, ReservationKind};
        let (gate, mut entered) = mpsc::channel(1);
        let rig = Rig::gated(Some(("agent.editor_ready", gate))).await;
        rig.agent("lead", Some("%1"), Some("recorded")).await;
        rig.command(OrgCommand::Revalidate {
            agent_id: "p/lead".parse().unwrap(),
        })
        .await;
        rig.row("pending-100", "%1", false).await;
        rig.frame("%1", "p/lead", IDLE);
        let (agents, _) = rig.start();
        let release = entered.recv().await.unwrap();
        tokio::time::advance(std::time::Duration::from_secs(7)).await;
        let result = rig
            .control
            .run_reserved(
                request("p/lead", Some("%1"), ReservationKind::Observe),
                |_| async { Ok::<_, AgentError>(()) },
                cleanup,
            )
            .await;
        assert!(matches!(
            result,
            Err(AgentError::Reservation(
                ReservationError::Quarantined { .. }
            ))
        ));
        agents.reconciled().await.unwrap();
        release.send(()).unwrap();
        rig.org.snapshot().await.unwrap();
        // The quarantined operation is the observation itself. The list
        // names it, in place of the failed observation it used to show.
        let rows = agents.list(ListParams::default()).await.unwrap();
        assert_eq!(
            rows[0].reason.as_deref(),
            Some(
                "quarantined: the agent's observation on %1 could not be settled; \
                 restart atmd and reconcile"
            )
        );
        agents
            .lifecycle(
                request("p/healthy", None, ReservationKind::Down),
                |_| async { Ok(()) },
                cleanup,
            )
            .await
            .unwrap();
        rig.control
            .run_reserved(
                request("p/other", None, ReservationKind::Observe),
                |_| async { Ok::<_, AgentError>(()) },
                cleanup,
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn clear_reply_lost_cleanup_sees_committed_clear() {
        use crate::control::reservation::ReservationKind;
        use std::{
            future::{poll_fn, Future},
            task::Poll,
        };
        let (gate, mut entered) = mpsc::channel(1);
        let rig = Rig::gated(Some(("agent.clear_requested", gate))).await;
        rig.agent("lead", Some("%1"), Some("recorded")).await;
        rig.row("recorded", "%1", false).await;
        rig.frame("%1", "p/lead", IDLE);
        let (agents, _) = rig.start();
        agents.reconciled().await.unwrap();
        let org = rig.org.clone();
        let cleanup_org = org.clone();
        let (read, reading) = tokio::sync::oneshot::channel();
        let job = tokio::spawn(async move {
            agents
                .lifecycle(
                    request("p/lead", Some("%1"), ReservationKind::Clear),
                    move |_| async move {
                        let mut execute = Box::pin(org.execute(
                            OrgCommand::Clear {
                                agent_id: "p/lead".parse().unwrap(),
                            },
                            chrono::Utc::now(),
                        ));
                        poll_fn(|cx| {
                            assert!(execute.as_mut().poll(cx).is_pending());
                            Poll::Ready(())
                        })
                        .await;
                        drop(execute);
                        Err::<(), _>(AgentError::Refused("lost reply".into()))
                    },
                    move |_, _| async move {
                        let mut snapshot = Box::pin(cleanup_org.snapshot());
                        poll_fn(|cx| {
                            assert!(snapshot.as_mut().poll(cx).is_pending());
                            Poll::Ready(())
                        })
                        .await;
                        read.send(()).unwrap();
                        let state = snapshot.await?;
                        assert_eq!(
                            state.agent(&"p/lead".parse().unwrap()).unwrap().status,
                            AgentStatus::Clearing
                        );
                        cleanup_org
                            .execute(
                                OrgCommand::Stuck {
                                    agent_id: "p/lead".parse().unwrap(),
                                    reason: "clear outcome was not acknowledged".into(),
                                },
                                chrono::Utc::now(),
                            )
                            .await?;
                        Ok(())
                    },
                )
                .await
        });
        let release = entered.recv().await.unwrap();
        reading.await.unwrap();
        release.send(()).unwrap();
        assert!(matches!(job.await.unwrap(), Err(AgentError::Refused(_))));
        assert_eq!(rig.state("lead").await.status, AgentStatus::Stuck);
        rig.control
            .run_reserved(
                request("p/lead", Some("%1"), ReservationKind::Observe),
                |_| async { Ok::<_, AgentError>(()) },
                cleanup,
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn observer_flood_has_one_waiter_and_other_agent_completes() {
        use crate::control::reservation::ReservationKind;
        let rig = Rig::new().await;
        rig.agent("lead", Some("%1"), Some("lead-row")).await;
        rig.agent("other", Some("%2"), Some("other-row")).await;
        rig.row("lead-row", "%1", false).await;
        // A Ready agent's own session no longer wakes it; another row on its pane does.
        rig.row("pending-300", "%1", false).await;
        rig.row("other-row", "%2", false).await;
        rig.frame("%1", "p/lead", IDLE);
        rig.frame("%2", "p/other", "unknown\n");
        let cancel = CancellationToken::new();
        let (agents, task) = spawn_agents_with(
            rig.org.clone(),
            rig.registry.clone(),
            rig.control.clone(),
            cancel.clone(),
            rig.reader(),
            std::sync::Arc::new(|_| None),
        );
        agents.reconciled().await.unwrap();
        let (entered, entry) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        let control = rig.control.clone();
        let delivery = tokio::spawn(async move {
            control
                .run_reserved(
                    request("p/lead", Some("%1"), ReservationKind::Delivery),
                    move |_| async move {
                        entered.send(()).unwrap();
                        released.await.unwrap();
                        Ok(())
                    },
                    cleanup,
                )
                .await
                .unwrap();
        });
        entry.await.unwrap();
        rig.inspect_only("%1", "p/lead");
        rig.frame("%2", "p/other", IDLE);
        let row = rig
            .registry
            .get_session(SessionId::new("pending-300"))
            .await
            .unwrap();
        let mut committed = rig.org.committed.subscribe();
        let mut observations = agents.observation_events();
        for _ in 0..80 {
            rig.events
                .send(crate::registry::SessionEvent::Updated {
                    session: Box::new(row.clone()),
                })
                .unwrap();
        }
        let row = rig
            .registry
            .get_session(SessionId::new("other-row"))
            .await
            .unwrap();
        rig.events
            .send(crate::registry::SessionEvent::Updated {
                session: Box::new(row),
            })
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                let event = committed.recv().await.unwrap();
                if event.kind == "agent.editor_ready" && event.body["agent_id"] == "p/other" {
                    break;
                }
            }
        })
        .await
        .unwrap();
        // The FIFO event receiver consumed the entire flood before this other
        // agent's ready report; count attempts too, so Busy cannot hide extras.
        let mut attempts = 0;
        let mut entries = 0;
        while let Ok(event) = observations.try_recv() {
            if event
                .agent
                .as_ref()
                .is_some_and(|id| id.to_string() == "p/lead")
            {
                attempts += usize::from(event.phase == "started");
                entries += usize::from(event.phase == "entered");
            }
        }
        assert_eq!(attempts, 1);
        assert_eq!(entries, 0);
        cancel.cancel();
        release.send(()).unwrap();
        delivery.await.unwrap();
        task.await.unwrap();
        while let Ok(event) = observations.try_recv() {
            if event
                .agent
                .as_ref()
                .is_some_and(|id| id.to_string() == "p/lead")
            {
                attempts += usize::from(event.phase == "started");
                entries += usize::from(event.phase == "entered");
            }
        }
        assert_eq!(attempts, 1);
        assert_eq!(entries, 1);
        assert_eq!(
            rig.client
                .pending_reads(&PaneId::parse("%1").unwrap())
                .unwrap(),
            0
        );
        assert_eq!(rig.state("other").await.status, AgentStatus::Ready);
    }

    #[tokio::test(start_paused = true)]
    async fn lagged_events_reread_current_registry_instead_of_replaying_old_rows() {
        let rig = Rig::new().await;
        rig.agent("lead", Some("%1"), Some("recorded")).await;
        // A Stuck agent binds only on a safe screen, so its current row can stay
        // unbound with no wake pending: the state a lag rescan has to find.
        rig.command(OrgCommand::Stuck {
            agent_id: "p/lead".parse().unwrap(),
            reason: "manual recovery".into(),
        })
        .await;
        rig.row("current", "%1", false).await;
        rig.inspect_only("%1", "p/lead");
        rig.frame("%1", "p/lead", "unknown\n");
        let (agents, _) = rig.start();
        agents.reconciled().await.unwrap();
        // Stuck since before this daemon started: the observer pins nothing
        // for it, so it has the baseline of the adopt that would precede this.
        Rig::pinned(&agents, "lead", "%1", "pi");
        // The look at the rows after the restart check reads the unsafe screen.
        let mut observations = agents.observation_events();
        progress(&mut observations, "finished", Some("p/lead")).await;
        let agent = rig.state("lead").await;
        assert_eq!(agent.status, AgentStatus::Stuck);
        assert_eq!(agent.session_id, Some(SessionId::new("recorded")));
        rig.frame("%1", "p/lead", IDLE);
        let mut committed = rig.org.committed.subscribe();
        // No await: overflow the subscription before it can consume any wake.
        for _ in 0..2050 {
            rig.events
                .send(crate::registry::SessionEvent::Removed {
                    session_id: SessionId::new("unrelated"),
                    reason: crate::registry::RemovalReason::Explicit,
                })
                .unwrap();
        }
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                if committed.recv().await.unwrap().kind == "agent.session_bound" {
                    break;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(
            rig.state("lead").await.session_id,
            Some(SessionId::new("current"))
        );
    }

    #[tokio::test]
    async fn coalesced_binding_candidate_cannot_cross_a_completed_clear() {
        use crate::control::reservation::ReservationKind;
        let rig = Rig::new().await;
        rig.agent("lead", Some("%1"), Some("recorded")).await;
        rig.agent("other", Some("%2"), Some("other-row")).await;
        rig.row("pending-100", "%1", false).await;
        rig.row("other-row", "%2", false).await;
        rig.frame("%1", "p/lead", IDLE);
        rig.frame("%2", "p/other", "unknown\n");
        let (agents, _) = rig.start();
        agents.reconciled().await.unwrap();
        let (entered, entry) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        let control = rig.control.clone();
        let held = tokio::spawn(async move {
            control
                .run_reserved(
                    request("p/lead", Some("%1"), ReservationKind::Clear),
                    move |_| async move {
                        entered.send(()).unwrap();
                        released.await.unwrap();
                        Ok(())
                    },
                    cleanup,
                )
                .await
                .unwrap();
        });
        entry.await.unwrap();
        // Wait for admission, not a delay: pacing may initially coalesce wakes.
        let mut observations = agents.observation_events();
        rig.row("early", "%1", false).await;
        progress(&mut observations, "started", Some("p/lead")).await;
        // One Observe parks, another wake becomes its coalesced dirty candidate.
        for _ in 0..5 {
            rig.events
                .send(crate::registry::SessionEvent::Registered {
                    session_id: SessionId::new("early"),
                    agent_type: AgentType::GeneralPurpose,
                })
                .unwrap();
        }
        rig.frame("%2", "p/other", IDLE);
        let mut committed = rig.org.committed.subscribe();
        let other = rig
            .registry
            .get_session(SessionId::new("other-row"))
            .await
            .unwrap();
        rig.events
            .send(crate::registry::SessionEvent::Updated {
                session: Box::new(other),
            })
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                let event = committed.recv().await.unwrap();
                if event.kind == "agent.editor_ready" && event.body["agent_id"] == "p/other" {
                    break;
                }
            }
        })
        .await
        .unwrap();
        rig.command(OrgCommand::Clear {
            agent_id: "p/lead".parse().unwrap(),
        })
        .await;
        rig.command(OrgCommand::EditorReady {
            agent_id: "p/lead".parse().unwrap(),
        })
        .await;
        let mut observations = agents.observation_events();
        rig.inspect_only("%1", "p/lead");
        rig.inspect_only("%1", "p/lead");
        release.send(()).unwrap();
        held.await.unwrap();
        progress(&mut observations, "finished", Some("p/lead")).await;
        progress(&mut observations, "finished", Some("p/lead")).await;
        assert_eq!(rig.state("lead").await.session_id, None);
        assert_eq!(
            rig.state("lead").await.excluded_session_id,
            Some(SessionId::new("recorded"))
        );
    }

    #[tokio::test]
    async fn stopped_service_reconciled_is_unavailable() {
        let rig = Rig::new().await;
        let cancel = CancellationToken::new();
        let (agents, task) = spawn_agents_with(
            rig.org.clone(),
            rig.registry.clone(),
            rig.control.clone(),
            cancel.clone(),
            rig.reader(),
            std::sync::Arc::new(|_| None),
        );
        agents.reconciled().await.unwrap();
        cancel.cancel();
        task.await.unwrap();
        assert!(matches!(
            agents.reconciled().await,
            Err(AgentError::Unavailable(_))
        ));
    }

    const STILL_STARTING: &str = "was still starting when the daemon restarted; inspect %1";

    /// `p/lead` with a role of `harness`, recorded on %1 and never bound:
    /// what a daemon that stops during `agent.up` leaves behind.
    async fn started(rig: &Rig, harness: &str) {
        rig.command(OrgCommand::Up {
            agent_id: "p/lead".parse().unwrap(),
            role: atm_org::parse_role("lead", &format!("+++\nharness='{harness}'\n+++\n")).unwrap(),
        })
        .await;
        rig.command(OrgCommand::PaneCreated {
            agent_id: "p/lead".parse().unwrap(),
            pane: "%1".into(),
        })
        .await;
    }
    /// The bodies of the notes sent so far.
    async fn notes(rig: &Rig) -> Vec<String> {
        let state = rig.org.snapshot().await.unwrap();
        state
            .messages()
            .filter(|message| message.content.kind == atm_org::MessageKind::Note)
            .map(|message| message.content.body.clone())
            .collect()
    }
    /// Keys and pastes the mock was asked to send.
    fn inputs(rig: &Rig) -> usize {
        rig.client
            .mock()
            .calls()
            .iter()
            .filter(|call| {
                matches!(
                    call,
                    atm_tmux::mock::MockCall::SendKeys { .. }
                        | atm_tmux::mock::MockCall::PasteText { .. }
                )
            })
            .count()
    }

    #[tokio::test]
    async fn interrupted_start_without_binding_is_stuck() {
        // The harness came up and shows its editor, with only its discovery
        // row: the start that would have made the agent Ready is gone with
        // its daemon, and a screen alone never makes a session-less Starting
        // agent Ready. Stuck, its owner told; the screen is not read.
        let rig = Rig::new().await;
        started(&rig, "pi").await;
        rig.row("pending-100", "%1", false).await;
        rig.frame("%1", "p/lead", IDLE);
        let scripted = unread(&rig);
        let (agents, _) = rig.start();
        agents.reconciled().await.unwrap();
        let lead = rig.state("lead").await;
        assert_eq!(lead.status, AgentStatus::Stuck);
        assert_eq!(lead.reason.as_deref(), Some(STILL_STARTING));
        assert!(lead.session_id.is_none());
        assert_eq!(
            notes(&rig).await,
            [format!("agent p/lead: {STILL_STARTING}")]
        );
        assert_eq!(unread(&rig), scripted - 2, "only the inspection was read");
        assert_eq!(told(&agents).await.2, PaneBaseline::unproven());
    }

    #[tokio::test]
    async fn interrupted_start_is_stuck_with_one_note() {
        // The other panes such an agent can be found on. Each time: Stuck
        // with the reason, one note to the owner, the list shows the reason,
        // nothing is typed, and no capture is read.
        const TRUST: &str = include_str!(
            "../../../atm-claude-adapter/tests/fixtures/screen_control_p1_A120-default-120x40_2.1.325.txt"
        );
        enum Pane {
            /// A scripted frame with `command` in front.
            Frame(&'static str, &'static str),
            /// An inspection only: a capture would fail.
            Unreadable,
        }
        for (case, harness, processes, row, pane) in [
            (
                "an idle shell, the launch never ran",
                "pi",
                idle_shell("zsh"),
                None,
                Pane::Frame("$ \n", "zsh"),
            ),
            (
                "a start dialog nobody answers",
                "claude",
                job(),
                Some(Harness::ClaudeCode),
                Pane::Frame(TRUST, "claude"),
            ),
            (
                "a pane that cannot be captured",
                "pi",
                job(),
                None,
                Pane::Unreadable,
            ),
        ] {
            let rig = Rig::new().await;
            started(&rig, harness).await;
            if let Some(harness) = row {
                rig.row_of("pending-100", "%1", false, harness).await;
            }
            *rig.proc.lock().unwrap() = Some(processes);
            match pane {
                Pane::Frame(text, command) => rig.frame_of("%1", "p/lead", text, command),
                Pane::Unreadable => inspect_with(&rig, "pi"),
            }
            let scripted = unread(&rig);
            let (agents, _) = rig.start();
            agents.reconciled().await.unwrap();
            let lead = rig.state("lead").await;
            assert_eq!(lead.status, AgentStatus::Stuck, "{case}");
            assert_eq!(lead.reason.as_deref(), Some(STILL_STARTING), "{case}");
            assert_eq!(
                notes(&rig).await,
                [format!("agent p/lead: {STILL_STARTING}")],
                "{case}"
            );
            assert_eq!(
                listed_reason(&agents).await.as_deref(),
                Some(STILL_STARTING),
                "{case}"
            );
            assert_eq!(inputs(&rig), 0, "{case}");
            assert_eq!(
                unread(&rig),
                scripted - 2,
                "{case}: only the inspection was read"
            );
        }

        // A pane that is gone or dead is not the agent's any more: Down, as
        // for every other agent, and no note.
        let rig = Rig::new().await;
        started(&rig, "pi").await;
        rig.client
            .script_inspect(
                &PaneId::parse("%1").unwrap(),
                Some(&Rig::inspection("%1", "p/lead", true)),
            )
            .unwrap();
        let (agents, _) = rig.start();
        agents.reconciled().await.unwrap();
        let lead = rig.state("lead").await;
        assert_eq!(lead.status, AgentStatus::Down);
        assert_eq!(
            lead.reason.as_deref(),
            Some("recorded pane is absent, dead or no longer owned")
        );
        assert_eq!(notes(&rig).await, Vec::<String>::new());
    }

    #[tokio::test]
    async fn interrupted_start_note_repairs_partial_prefix_without_duplicates() {
        // The daemon stops again between the Stuck and its note: the next
        // restart check sends the note, and the one after it sends no second.
        for partial in [false, true] {
            let rig = Rig::new().await;
            started(&rig, "pi").await;
            if partial {
                rig.command(OrgCommand::Stuck {
                    agent_id: "p/lead".parse().unwrap(),
                    reason: STILL_STARTING.into(),
                })
                .await;
            }
            for _ in 0..2 {
                inspect_with(&rig, "pi");
                let cancel = rig.cancel.child_token();
                let (agents, task) = spawn_agents_with(
                    rig.org.clone(),
                    rig.registry.clone(),
                    rig.control.clone(),
                    cancel.clone(),
                    rig.reader(),
                    std::sync::Arc::new(|_| None),
                );
                agents.reconciled().await.unwrap();
                assert_eq!(rig.state("lead").await.status, AgentStatus::Stuck);
                assert_eq!(
                    notes(&rig).await,
                    [format!("agent p/lead: {STILL_STARTING}")],
                    "{partial}"
                );
                cancel.cancel();
                task.await.unwrap();
            }
        }
    }

    #[tokio::test]
    async fn second_interrupted_start_sends_its_own_note() {
        let rig = Rig::new().await;
        for round in 1..=2 {
            started(&rig, "pi").await;
            // Two restarts per incident: the second must not repeat the note.
            for _ in 0..2 {
                inspect_with(&rig, "pi");
                let cancel = rig.cancel.child_token();
                let (agents, task) = spawn_agents_with(
                    rig.org.clone(),
                    rig.registry.clone(),
                    rig.control.clone(),
                    cancel.clone(),
                    rig.reader(),
                    std::sync::Arc::new(|_| None),
                );
                agents.reconciled().await.unwrap();
                assert_eq!(rig.state("lead").await.status, AgentStatus::Stuck);
                assert_eq!(notes(&rig).await.len(), round);
                cancel.cancel();
                task.await.unwrap();
            }
            // The owner takes it down; the next round starts it again.
            rig.command(OrgCommand::Down {
                agent_id: "p/lead".parse().unwrap(),
                reason: "taken down".into(),
            })
            .await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn failed_interrupted_start_note_retries_on_next_wake() {
        let rig = Rig::gated_with_failure(None, Some("msg.sent")).await;
        started(&rig, "pi").await;
        rig.row("pending-100", "%1", false).await;
        inspect_with(&rig, "pi");
        let (agents, _) = rig.start();
        agents.reconciled().await.unwrap();
        assert_eq!(rig.state("lead").await.status, AgentStatus::Stuck);
        assert_eq!(notes(&rig).await, Vec::<String>::new());
        assert!(listed_reason(&agents)
            .await
            .unwrap()
            .starts_with(&format!("{STILL_STARTING}; observation unavailable: ")));
        let mut events = agents.observation_events();
        // The first recovery is the observer's own retry (paused clock); the
        // second pass is an ordinary wake and must not repeat the note.
        for wake in [false, true] {
            inspect_with(&rig, "pi");
            if wake {
                update(&rig, "pending-100").await;
            }
            progress(&mut events, "finished", Some("p/lead")).await;
            assert_eq!(
                notes(&rig).await,
                [format!("agent p/lead: {STILL_STARTING}")]
            );
            assert_eq!(
                listed_reason(&agents).await.as_deref(),
                Some(STILL_STARTING)
            );
        }
    }

    #[tokio::test]
    async fn runtime_starting_is_not_a_restart_failure() {
        // An agent that is Starting without a session while its daemon runs
        // has a flow driving it (or is an adopt waiting for its session): an
        // observation of it is not the restart check, and changes nothing.
        let rig = Rig::new().await;
        let (agents, _) = rig.start();
        agents.reconciled().await.unwrap();
        rig.row("pending-100", "%1", false).await;
        let mut events = agents.observation_events();
        rig.frame("%1", "p/lead", IDLE);
        rig.command(OrgCommand::Adopt {
            agent_id: "p/lead".parse().unwrap(),
            pane: "%1".into(),
            session_id: None,
        })
        .await;
        progress(&mut events, "finished", Some("p/lead")).await;
        assert_eq!(unread(&rig), 0);
        let lead = rig.state("lead").await;
        assert_eq!((lead.status, lead.reason), (AgentStatus::Starting, None));
        assert_eq!(notes(&rig).await, Vec::<String>::new());
    }

    #[tokio::test]
    async fn starting_with_a_session_at_restart_is_not_stuck() {
        // A Ready agent the last restart check suspended is Starting with
        // its session: not an interrupted start. With its harness in front
        // and its row there it is pinned and Ready again, and nobody is told.
        let rig = Rig::new().await;
        rig.agent("lead", Some("%1"), Some("s1")).await;
        rig.command(OrgCommand::Revalidate {
            agent_id: "p/lead".parse().unwrap(),
        })
        .await;
        rig.row("s1", "%1", false).await;
        rig.frame("%1", "p/lead", IDLE);
        let (agents, _) = rig.start();
        agents.reconciled().await.unwrap();
        assert_eq!(unread(&rig), 0);
        let lead = rig.state("lead").await;
        assert_eq!((lead.status, lead.reason), (AgentStatus::Ready, None));
        assert_eq!(notes(&rig).await, Vec::<String>::new());
        assert_eq!(listed_reason(&agents).await, None);
    }

    #[tokio::test]
    async fn wrong_harness_is_not_liveness_or_list_activity() {
        let rig = Rig::new().await;
        rig.agent("lead", Some("%1"), Some("recorded")).await;
        let mut row = SessionDomain::new(
            SessionId::new("recorded"),
            AgentType::GeneralPurpose,
            Model::default(),
        );
        row.harness = Harness::ClaudeCode;
        row.tmux_pane = Some("%1".into());
        rig.registry.register(row).await.unwrap();
        rig.frame("%1", "p/lead", IDLE);
        let (agents, _) = rig.start();
        let rows = agents.list(ListParams::default()).await.unwrap();
        assert_eq!(rows[0].state, AgentStatus::Starting);
        assert!(rows[0].activity.is_none());
        assert_eq!(rows[0].harness.as_deref(), Some("pi"));
    }

    #[tokio::test]
    async fn registry_failure_is_unavailable_not_all_down() {
        let rig = Rig::new().await;
        rig.agent("lead", Some("%1"), Some("recorded")).await;
        let (send, receive) = mpsc::channel(1);
        drop(receive);
        let (events, _) = broadcast::channel(1);
        let dead = RegistryHandle::new(send, events);
        let (agents, task) = spawn_agents_with(
            rig.org.clone(),
            dead,
            rig.control.clone(),
            rig.cancel.child_token(),
            rig.reader(),
            std::sync::Arc::new(|_| None),
        );
        agents.reconciled().await.unwrap();
        assert!(matches!(
            agents.list(ListParams::default()).await,
            Err(AgentError::Unavailable(_))
        ));
        assert_eq!(rig.state("lead").await.status, AgentStatus::Ready);
        assert!(rig.client.mock().calls().is_empty());
        rig.cancel.cancel();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn list_is_sorted_read_only_uses_committed_cost_and_does_not_create_filter() {
        let rig = Rig::new().await;
        rig.agent("z", None, None).await;
        rig.agent("a", None, None).await;
        for (id, amount) in [("one", 4), ("one", 7), ("two", 2)] {
            rig.command(OrgCommand::RecordSummary(atm_org::SessionSummary {
                session_id: SessionId::new(id),
                pane: None,
                agent_id: Some("p/a".parse().unwrap()),
                harness: Harness::Pi,
                model: Some("summary-model".into()),
                cost: Some(atm_core::Money::from_microdollars(amount)),
                first_prompt: None,
                started: chrono::Utc::now(),
                ended: None,
            }))
            .await;
        }
        let (agents, _) = rig.start();
        agents.reconciled().await.unwrap();
        let cursor = rig.org.status().unwrap();
        let rows = agents.list(ListParams::default()).await.unwrap();
        assert_eq!(
            rows.iter()
                .map(|r| r.agent_id.to_string())
                .collect::<Vec<_>>(),
            ["p/a", "p/z"]
        );
        assert_eq!(rows[0].cost_usd, Some(0.000009));
        assert_eq!(rows[0].model.as_deref(), Some("summary-model"));
        assert_eq!(rig.org.status().unwrap(), cursor);
        let dir = tempfile::tempdir().unwrap();
        assert!(agents
            .list(ListParams {
                project_root: Some(dir.path().into())
            })
            .await
            .unwrap()
            .is_empty());
        assert!(!dir.path().join(".atm").exists());
        assert_eq!(rig.org.status().unwrap(), cursor);
    }

    fn request(
        id: &str,
        pane: Option<&str>,
        kind: crate::control::reservation::ReservationKind,
    ) -> crate::control::reservation::ReservationRequest {
        crate::control::reservation::ReservationRequest::new(
            id.parse().unwrap(),
            pane.map(|p| PaneId::parse(p).unwrap()),
            kind,
            crate::control::reservation::WaitPolicy::Wait,
            tokio::time::Instant::now() + std::time::Duration::from_secs(60),
        )
    }
    async fn cleanup(
        _: crate::control::reservation::Reservation,
        _: crate::control::reservation::Completion,
    ) -> Result<(), AgentError> {
        Ok(())
    }

    // Observations started for p/lead among the progress events received so far.
    fn drain_started(
        events: &mut broadcast::Receiver<crate::org::agents::ObservationProgress>,
    ) -> usize {
        let mut started = 0;
        while let Ok(event) = events.try_recv() {
            started += usize::from(event.phase == "started" && event.agent.is_some());
        }
        started
    }

    #[tokio::test(start_paused = true)]
    async fn down_drops_a_failed_restart_check_and_admits_up_at_once() {
        // Review 16 H2: the restart check of an agent keeps failing (tmux does
        // not answer), the owner takes it down and asks for it again. The
        // failure belonged to the pane that is gone; it must not hold `up`
        // until the backed-off retry fires.
        use crate::control::reservation::ReservationKind;
        let rig = Rig::new().await;
        rig.agent("lead", Some("%1"), Some("recorded")).await;
        let (agents, _) = rig.start();
        agents.reconciled().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_secs(130)).await;
        let up = || {
            agents.lifecycle(
                request("p/lead", None, ReservationKind::Up),
                |_| async { Ok(()) },
                cleanup,
            )
        };
        // The twin: while the check is still failing, `up` is refused.
        match up().await {
            Err(AgentError::Unavailable(reason)) => {
                assert!(
                    reason.contains("restart check is being retried"),
                    "{reason}"
                );
            }
            other => panic!("{other:?}"),
        }
        let at = tokio::time::Instant::now();
        rig.command(OrgCommand::Down {
            agent_id: "p/lead".parse().unwrap(),
            reason: "owner".into(),
        })
        .await;
        agents.failure_cleared(&"p/lead".parse().unwrap()).await;
        up().await.unwrap();
        // No timer was needed: the virtual clock has not moved.
        assert_eq!(at.elapsed(), std::time::Duration::ZERO);
        assert_eq!(
            agents.list(ListParams::default()).await.unwrap()[0]
                .reason
                .as_deref(),
            Some("owner")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn wake_during_backoff_retries_within_the_minimum_delay() {
        // Review 16 H2: after 40 s of failed checks the retry is some 20 s away.
        // A session registering then, with tmux answering again, is bound within
        // RETRY_MIN (plus the pacing between two observations), not when the old
        // timer fires.
        let rig = Rig::new().await;
        rig.agent("lead", Some("%1"), None).await;
        let (agents, _) = rig.start();
        agents.reconciled().await.unwrap();
        let mut events = agents.observation_events();
        tokio::time::sleep(std::time::Duration::from_secs(40)).await;
        drain_started(&mut events);
        rig.inspect_only("%1", "p/lead");
        rig.inspect_only("%1", "p/lead");
        let at = tokio::time::Instant::now();
        rig.row("fresh", "%1", false).await;
        // The restart check, then the binding it re-queues.
        progress(&mut events, "finished", Some("p/lead")).await;
        progress(&mut events, "finished", Some("p/lead")).await;
        assert_eq!(
            rig.state("lead").await.session_id,
            Some(SessionId::new("fresh"))
        );
        assert!(
            at.elapsed() >= std::time::Duration::from_secs(1),
            "the retry is never pulled in to the pacing interval: {:?}",
            at.elapsed()
        );
        assert!(
            at.elapsed() < std::time::Duration::from_secs(2),
            "{:?}",
            at.elapsed()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn wakes_during_backoff_retry_at_most_once_per_minimum_delay() {
        // The other side of H2: a busy agent whose pane can't be read must not
        // be retried on every hook (that was 20 warnings a second).
        let rig = Rig::new().await;
        rig.agent("lead", Some("%1"), None).await;
        rig.row("busy", "%1", false).await;
        let (agents, _) = rig.start();
        agents.reconciled().await.unwrap();
        let mut events = agents.observation_events();
        tokio::time::sleep(std::time::Duration::from_secs(40)).await;
        drain_started(&mut events);
        // 40 hooks in two seconds, tmux still not answering.
        for _ in 0..40 {
            update(&rig, "busy").await;
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let started = drain_started(&mut events);
        assert!((1..=3).contains(&started), "{started} retries in 2 s");
    }

    #[tokio::test]
    async fn own_hook_queued_with_the_commit_that_left_ready_still_wakes() {
        // Review 16 H1: the observer handles committed organisation events
        // before registry events. An agent's own hook that is queued together
        // with the commit that made it Stuck must be routed against the new
        // state, or the "Ready agent's own session" filter drops the one wake
        // that can recover it.
        let rig = Rig::new().await;
        rig.agent("lead", Some("%1"), Some("s1")).await;
        rig.row("s1", "%1", false).await;
        rig.frame("%1", "p/lead", IDLE);
        let (agents, _) = rig.start();
        agents.reconciled().await.unwrap();
        let mut events = agents.observation_events();
        // Park the observer in its select before the two events are queued.
        rig.events
            .send(crate::registry::SessionEvent::Removed {
                session_id: SessionId::new("unrelated"),
                reason: crate::registry::RemovalReason::Explicit,
            })
            .unwrap();
        progress(&mut events, "scheduled", None).await;
        progress(&mut events, "idle", None).await;
        let row = rig
            .registry
            .get_session(SessionId::new("s1"))
            .await
            .unwrap();
        rig.frame("%1", "p/lead", IDLE);
        let command = rig.command(OrgCommand::Stuck {
            agent_id: "p/lead".parse().unwrap(),
            reason: "unexpected screen in %1; inspect the pane".into(),
        });
        tokio::pin!(command);
        // One poll: the command is in the organisation actor's queue, not applied.
        tokio::select! {
            biased;
            _ = &mut command => panic!("the actor cannot have replied yet"),
            _ = std::future::ready(()) => {}
        }
        rig.events
            .send(crate::registry::SessionEvent::Updated {
                session: Box::new(row),
            })
            .unwrap();
        command.await;
        progress(&mut events, "finished", Some("p/lead")).await;
        let agent = rig.state("lead").await;
        assert_eq!(agent.status, AgentStatus::Ready);
        assert_eq!(agent.reason, None);
        assert_eq!(
            rig.client
                .pending_reads(&PaneId::parse("%1").unwrap())
                .unwrap(),
            0
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_successful_observation_resets_the_backoff() {
        // Four failed checks grow the delay to 8 s. After one success the next
        // failure must be retried after RETRY_MIN again, not after 16 s.
        let rig = Rig::new().await;
        rig.agent("lead", Some("%1"), None).await;
        let (agents, _) = rig.start();
        agents.reconciled().await.unwrap();
        let mut events = agents.observation_events();
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        // The first check failed before the subscription; its retries follow.
        assert_eq!(drain_started(&mut events), 3, "retries at 1, 3 and 7 s");
        // tmux answers the retry at 15 s.
        rig.inspect_only("%1", "p/lead");
        progress(&mut events, "finished", Some("p/lead")).await;
        agents.failure_cleared(&"p/lead".parse().unwrap()).await;
        assert_eq!(
            rig.client
                .pending_reads(&PaneId::parse("%1").unwrap())
                .unwrap(),
            0
        );
        // A new session wakes the agent and tmux fails again.
        rig.row("fresh", "%1", false).await;
        progress(&mut events, "finished", Some("p/lead")).await;
        let failed = tokio::time::Instant::now();
        progress(&mut events, "started", Some("p/lead")).await;
        assert_eq!(failed.elapsed(), std::time::Duration::from_secs(1));
    }

    #[tokio::test]
    async fn stuck_binding_honours_the_event_fence() {
        // A Stuck agent's binding candidate waits behind another reservation.
        // An agent event committed meanwhile (here: Stuck again, for a new
        // reason) must fence it, even though the pane, binding and exclusion
        // are unchanged and the screen is safe. `fenced = false` is the twin.
        use crate::control::reservation::ReservationKind;
        for fenced in [true, false] {
            let rig = Rig::new().await;
            rig.agent("lead", Some("%1"), Some("s1")).await;
            rig.command(OrgCommand::Stuck {
                agent_id: "p/lead".parse().unwrap(),
                reason: "first".into(),
            })
            .await;
            rig.inspect_only("%1", "p/lead");
            let (agents, _) = rig.start();
            agents.reconciled().await.unwrap();
            // Stuck since before this daemon started: the observer pins nothing
            // for it, so it has the baseline of the adopt that would precede this.
            Rig::pinned(&agents, "lead", "%1", "pi");
            let mut events = agents.observation_events();
            let (entered, entry) = tokio::sync::oneshot::channel();
            let (release, released) = tokio::sync::oneshot::channel();
            let control = rig.control.clone();
            let held = tokio::spawn(async move {
                control
                    .run_reserved(
                        request("p/lead", Some("%1"), ReservationKind::Clear),
                        move |_| async move {
                            entered.send(()).unwrap();
                            released.await.unwrap();
                            Ok(())
                        },
                        cleanup,
                    )
                    .await
                    .unwrap();
            });
            entry.await.unwrap();
            // The agent's session reports in: a candidate, parked behind the hold.
            rig.row("s1", "%1", false).await;
            progress(&mut events, "started", Some("p/lead")).await;
            if fenced {
                rig.command(OrgCommand::Stuck {
                    agent_id: "p/lead".parse().unwrap(),
                    reason: "second".into(),
                })
                .await;
            }
            rig.frame("%1", "p/lead", IDLE);
            release.send(()).unwrap();
            held.await.unwrap();
            progress(&mut events, "finished", Some("p/lead")).await;
            let agent = rig.state("lead").await;
            if fenced {
                assert_eq!(agent.status, AgentStatus::Stuck);
                assert_eq!(agent.reason.as_deref(), Some("second"));
            } else {
                assert_eq!(agent.status, AgentStatus::Ready);
                assert_eq!(agent.session_id, Some(SessionId::new("s1")));
            }
            // Both read the safe screen: the fence decided, not a failed read.
            assert_eq!(
                rig.client
                    .pending_reads(&PaneId::parse("%1").unwrap())
                    .unwrap(),
                0,
                "fenced={fenced}"
            );
            assert_eq!(
                agents.list(ListParams::default()).await.unwrap()[0]
                    .reason
                    .as_deref(),
                fenced.then_some("second")
            );
        }
    }

    // The lifecycle flows' tests use this module's rig; they live in their own file.
    // --- Task 10, the observer half: the recorded baseline, re-derived by the
    // adopt rule, and the process read when the harness is not in front ---

    use super::super::agent_flow::ProcFacts;

    /// A shell at its prompt: it holds the terminal and has no children.
    fn idle_shell(name: &str) -> ProcFacts {
        ProcFacts {
            process_group: 100,
            foreground_group: 100,
            system_shell: true,
            children: 0,
            name: name.into(),
        }
    }
    /// A shell that holds the terminal and has a child: a program run without
    /// job control, a stopped job, or a child left in the background.
    fn shell_with_child(name: &str) -> ProcFacts {
        ProcFacts {
            children: 1,
            ..idle_shell(name)
        }
    }
    /// A shell with job control running one job in the foreground: the rig's
    /// default answer.
    fn job() -> ProcFacts {
        ProcFacts {
            foreground_group: 150,
            ..shell_with_child("zsh")
        }
    }
    fn pinned_to(command: &str) -> PaneBaseline {
        PaneBaseline::adopted(100)
            .with_harness(command.into())
            .unwrap()
    }
    /// One inspection of %1, tagged for `p/lead`, with `command` in front.
    fn inspect_with(rig: &Rig, command: &str) {
        rig.client
            .script_inspect(
                &PaneId::parse("%1").unwrap(),
                Some(
                    &Rig::inspection("%1", "p/lead", false)
                        .with_foreground_process(None, Some(command.into())),
                ),
            )
            .unwrap();
    }
    fn unread(rig: &Rig) -> usize {
        rig.client
            .pending_reads(&PaneId::parse("%1").unwrap())
            .unwrap()
    }
    /// What a delivery to `p/lead` is told: status, binding, baseline, row.
    async fn told(
        agents: &AgentHandle,
    ) -> (
        AgentStatus,
        Option<SessionId>,
        PaneBaseline,
        Option<SessionId>,
    ) {
        let recipient = agents.recipient(&"p/lead".parse().unwrap()).await.unwrap();
        (
            recipient.status,
            recipient.session_id,
            recipient.baseline,
            recipient.session.map(|row| row.id),
        )
    }
    /// `p/lead`, Ready and bound to `s1` on %1 with pi in front, its baseline
    /// pinned by the restart pass.
    async fn pinned_ready() -> (Rig, AgentHandle) {
        let rig = Rig::new().await;
        rig.agent("lead", Some("%1"), Some("s1")).await;
        rig.row("s1", "%1", false).await;
        rig.frame("%1", "p/lead", IDLE);
        let (agents, _) = rig.start();
        agents.reconciled().await.unwrap();
        assert_eq!(unread(&rig), 0);
        assert_eq!(told(&agents).await.2, pinned_to("pi"));
        (rig, agents)
    }

    #[tokio::test]
    async fn restart_rederives_baseline_with_live_row() {
        // After a daemon restart nothing is recorded about a pane's processes.
        // The restart pass derives it by the adopt rule: the inspection, the
        // process read, the capture; a job in front that is not the shell, a
        // screen the adapter recognises, a live row of the harness. The agent
        // stays Ready with no event, and a delivery gets the pinned baseline.
        let (rig, agents) = pinned_ready().await;
        let lead = rig.state("lead").await;
        assert_eq!(lead.status, AgentStatus::Ready);
        assert_eq!(
            told(&agents).await,
            (
                AgentStatus::Ready,
                Some(SessionId::new("s1")),
                pinned_to("pi"),
                Some(SessionId::new("s1"))
            )
        );

        // A Ready agent without a session (a Codex before its first prompt,
        // with only its discovery row) is pinned too, with no lifecycle change.
        let rig = Rig::new().await;
        rig.agent_of("lead", "codex", Some("%1"), None).await;
        rig.row_of("pending-100", "%1", false, Harness::Codex).await;
        rig.frame_of("%1", "p/lead", body(CODEX_IDLE), "codex");
        let before = rig.org.status().unwrap();
        let (agents, _) = rig.start();
        agents.reconciled().await.unwrap();
        assert_eq!(unread(&rig), 0);
        assert_eq!(rig.org.status().unwrap(), before);
        assert_eq!(
            told(&agents).await,
            (AgentStatus::Ready, None, pinned_to("codex"), None)
        );
    }

    #[tokio::test]
    async fn restart_without_row_keeps_starting() {
        // The same pane with no row of the harness yet (the registry is empty
        // until discovery or the harness's next event): nothing is pinned, the
        // screen reads Unknown, and the bound agent waits as Starting. The
        // pass that the row's arrival starts pins it, and it is Ready.
        let rig = Rig::new().await;
        rig.agent("lead", Some("%1"), Some("s1")).await;
        rig.frame("%1", "p/lead", IDLE);
        let (agents, _) = rig.start();
        agents.reconciled().await.unwrap();
        assert_eq!(unread(&rig), 0);
        assert_eq!(
            told(&agents).await,
            (
                AgentStatus::Starting,
                Some(SessionId::new("s1")),
                PaneBaseline::unproven(),
                None
            )
        );
        let mut events = agents.observation_events();
        rig.frame("%1", "p/lead", IDLE);
        rig.row("s1", "%1", false).await;
        progress(&mut events, "finished", Some("p/lead")).await;
        assert_eq!(unread(&rig), 0);
        assert_eq!(
            told(&agents).await,
            (
                AgentStatus::Ready,
                Some(SessionId::new("s1")),
                pinned_to("pi"),
                Some(SessionId::new("s1"))
            )
        );
    }

    #[tokio::test]
    async fn row_before_or_after_the_first_pass_ends_the_same() {
        // Whether discovery or the observer is first after a daemon restart
        // must not decide whether an agent gets input again: the row before
        // the first pass, and the row after it, end pinned, Ready, and with
        // the same answer for a delivery.
        let mut ends = Vec::new();
        for row_first in [true, false] {
            let rig = Rig::new().await;
            rig.agent("lead", Some("%1"), Some("s1")).await;
            if row_first {
                rig.row("s1", "%1", false).await;
            }
            rig.frame("%1", "p/lead", IDLE);
            let (agents, _) = rig.start();
            agents.reconciled().await.unwrap();
            if !row_first {
                let mut events = agents.observation_events();
                rig.frame("%1", "p/lead", IDLE);
                rig.row("s1", "%1", false).await;
                progress(&mut events, "finished", Some("p/lead")).await;
            }
            assert_eq!(unread(&rig), 0, "{row_first}");
            let lead = rig.state("lead").await;
            ends.push((told(&agents).await, lead.reason, lead.excluded_session_id));
        }
        assert_eq!(ends[0], ends[1]);
        assert_eq!(
            ends[0].0,
            (
                AgentStatus::Ready,
                Some(SessionId::new("s1")),
                pinned_to("pi"),
                Some(SessionId::new("s1"))
            )
        );
    }

    #[tokio::test]
    async fn exit_to_shell_records_down_without_end() {
        // The harness exits to its shell and sends no end event: its row
        // stays. The next pass over the agent sees a command that is not the
        // pinned one and reads the pane's processes: an idle shell is the
        // evidence. Down, from the inspection alone (no capture is read).
        let (rig, agents) = pinned_ready().await;
        let mut events = agents.observation_events();
        // The twin: another row on the pane wakes a pass with pi in front.
        let before = rig.org.status().unwrap();
        inspect_with(&rig, "pi");
        *rig.proc.lock().unwrap() = Some(idle_shell("zsh"));
        rig.row("pending-300", "%1", false).await;
        progress(&mut events, "finished", Some("p/lead")).await;
        assert_eq!(unread(&rig), 0);
        assert_eq!(rig.org.status().unwrap(), before);
        assert_eq!(rig.state("lead").await.status, AgentStatus::Ready);

        inspect_with(&rig, "zsh");
        update(&rig, "pending-300").await;
        progress(&mut events, "finished", Some("p/lead")).await;
        assert_eq!(unread(&rig), 0);
        let lead = rig.state("lead").await;
        assert_eq!(lead.status, AgentStatus::Down);
        assert_eq!(
            lead.reason.as_deref(),
            Some("the harness exited to the shell in %1")
        );
        assert_eq!(
            agents.baseline_of(&"p/lead".parse().unwrap(), "%1"),
            PaneBaseline::unproven()
        );
    }

    #[tokio::test]
    async fn restart_idle_shell_records_down() {
        // The harness exited while the daemon was down. The restart pass finds
        // an idle shell under an agent that was Ready, or bound: Down, and the
        // stale frame is never read.
        for (case, session, revalidate) in [
            ("ready and bound", Some("s1"), false),
            ("ready without a session", None, false),
            ("starting and bound", Some("s1"), true),
        ] {
            let rig = Rig::new().await;
            rig.agent("lead", Some("%1"), session).await;
            if revalidate {
                rig.command(OrgCommand::Revalidate {
                    agent_id: "p/lead".parse().unwrap(),
                })
                .await;
            }
            *rig.proc.lock().unwrap() = Some(idle_shell("zsh"));
            inspect_with(&rig, "zsh");
            let (agents, _) = rig.start();
            agents.reconciled().await.unwrap();
            assert_eq!(unread(&rig), 0, "{case}");
            let lead = rig.state("lead").await;
            assert_eq!(lead.status, AgentStatus::Down, "{case}");
            assert_eq!(
                lead.reason.as_deref(),
                Some("the harness exited to the shell in %1"),
                "{case}"
            );
        }
        // The twin: an agent that was never Ready or bound. An idle shell
        // under it is a start that has not happened, not an exit, so it is
        // not Down: it is a start its daemon interrupted, Stuck with a note,
        // and the stale frame is not read.
        let rig = Rig::new().await;
        started(&rig, "pi").await;
        *rig.proc.lock().unwrap() = Some(idle_shell("zsh"));
        rig.frame_of("%1", "p/lead", IDLE, "zsh");
        let scripted = unread(&rig);
        let (agents, _) = rig.start();
        agents.reconciled().await.unwrap();
        assert_eq!(unread(&rig), scripted - 2);
        let lead = rig.state("lead").await;
        assert_eq!(lead.status, AgentStatus::Stuck);
        assert_eq!(lead.reason.as_deref(), Some(STILL_STARTING));
        assert_eq!(
            notes(&rig).await,
            [format!("agent p/lead: {STILL_STARTING}")]
        );
    }

    #[tokio::test]
    async fn restart_then_exit_to_shell_is_refused() {
        // A pane that pins nothing at restart: a shell without job control
        // that runs the harness as its child. Then the harness exits and
        // leaves a child behind. Through all of it every read is refused (the
        // baseline stays unproven) and the stale idle frame never makes the
        // agent Ready; the pass that finds the shell idle records Down.
        let rig = Rig::new().await;
        rig.agent("lead", Some("%1"), Some("s1")).await;
        rig.row("s1", "%1", false).await;
        *rig.proc.lock().unwrap() = Some(shell_with_child("sh"));
        rig.frame_of("%1", "p/lead", IDLE, "sh");
        let (agents, _) = rig.start();
        agents.reconciled().await.unwrap();
        let refused = (
            AgentStatus::Starting,
            Some(SessionId::new("s1")),
            PaneBaseline::unproven(),
            Some(SessionId::new("s1")),
        );
        assert_eq!(unread(&rig), 0);
        assert_eq!(told(&agents).await, refused);
        let mut events = agents.observation_events();
        let before = rig.org.status().unwrap();
        for _ in 0..2 {
            rig.frame_of("%1", "p/lead", IDLE, "sh");
            update(&rig, "s1").await;
            progress(&mut events, "finished", Some("p/lead")).await;
            assert_eq!(unread(&rig), 0);
            assert_eq!(rig.org.status().unwrap(), before);
            assert_eq!(told(&agents).await, refused);
        }
        *rig.proc.lock().unwrap() = Some(idle_shell("sh"));
        inspect_with(&rig, "sh");
        update(&rig, "s1").await;
        progress(&mut events, "finished", Some("p/lead")).await;
        assert_eq!(unread(&rig), 0);
        let lead = rig.state("lead").await;
        assert_eq!(lead.status, AgentStatus::Down);
        assert_eq!(
            lead.reason.as_deref(),
            Some("the harness exited to the shell in %1")
        );
    }

    #[tokio::test]
    async fn suspended_harness_is_refused_not_down() {
        // Ctrl-Z in the pane: the shell is in front and holds the stopped
        // harness as its child. That is not an exit: nothing is recorded and
        // the agent keeps its state and its pinned baseline. Input is refused
        // by the baseline itself: a capture with the shell in front reads
        // Unknown through it, and reads again once the harness is back.
        let (rig, agents) = pinned_ready().await;
        let mut events = agents.observation_events();
        let before = rig.org.status().unwrap();
        *rig.proc.lock().unwrap() = Some(shell_with_child("zsh"));
        inspect_with(&rig, "zsh");
        rig.row("pending-300", "%1", false).await;
        progress(&mut events, "finished", Some("p/lead")).await;
        assert_eq!(unread(&rig), 0);
        assert_eq!(rig.org.status().unwrap(), before);
        assert_eq!(rig.state("lead").await.status, AgentStatus::Ready);
        let recipient = agents.recipient(&"p/lead".parse().unwrap()).await.unwrap();
        assert_eq!(recipient.baseline, pinned_to("pi"));
        let shows = |command: &str| {
            recipient.screen.facts(
                &PaneCapture::new(
                    IDLE.into(),
                    Rig::inspection("%1", "p/lead", false)
                        .with_foreground_process(None, Some(command.into())),
                    CaptureFormat::Ansi,
                ),
                &recipient.baseline,
            )
        };
        assert_eq!(shows("zsh"), atm_core::ScreenFacts::default());
        assert_eq!(shows("pi").input, atm_core::InputState::Empty);
        // Resumed: the next pass finds pi in front again and changes nothing.
        inspect_with(&rig, "pi");
        update(&rig, "pending-300").await;
        progress(&mut events, "finished", Some("p/lead")).await;
        assert_eq!(unread(&rig), 0);
        assert_eq!(rig.org.status().unwrap(), before);
        assert_eq!(told(&agents).await.2, pinned_to("pi"));
    }

    #[tokio::test]
    async fn shell_at_restart_is_never_ready() {
        // At restart the pane shows the harness's idle editor, but a shell is
        // in front: the frame is stale. Nothing is pinned, the frame reads
        // Unknown, and a Ready agent is suspended (Starting), never confirmed.
        // The twin with pi in front: restart_rederives_baseline_with_live_row.
        let job_named_as_its_shell = ProcFacts {
            foreground_group: 150,
            ..shell_with_child("zsh")
        };
        for (case, processes) in [
            ("a shell holding a child", shell_with_child("zsh")),
            ("a job named as its shell", job_named_as_its_shell),
        ] {
            let rig = Rig::new().await;
            rig.agent("lead", Some("%1"), Some("s1")).await;
            rig.row("s1", "%1", false).await;
            *rig.proc.lock().unwrap() = Some(processes);
            rig.frame_of("%1", "p/lead", IDLE, "zsh");
            let (agents, _) = rig.start();
            agents.reconciled().await.unwrap();
            assert_eq!(unread(&rig), 0, "{case}");
            assert_eq!(
                told(&agents).await,
                (
                    AgentStatus::Starting,
                    Some(SessionId::new("s1")),
                    PaneBaseline::unproven(),
                    Some(SessionId::new("s1"))
                ),
                "{case}"
            );
            // The same stale frame on the harness's next event: still not Ready.
            let mut events = agents.observation_events();
            rig.frame_of("%1", "p/lead", IDLE, "zsh");
            update(&rig, "s1").await;
            progress(&mut events, "finished", Some("p/lead")).await;
            assert_eq!(unread(&rig), 0, "{case}");
            assert_eq!(rig.state("lead").await.status, AgentStatus::Starting);
        }
    }

    #[tokio::test]
    async fn observer_pins_nothing_for_a_stuck_agent() {
        // The observer pins only Starting and Ready agents. An agent that is
        // Stuck when the daemon starts has no baseline and gets none: its
        // session's event on a safe idle screen reads Unknown, and it stays
        // Stuck until its owner adopts it. Twin (the same with a baseline
        // recorded): review_stuck_binding_recovers_with_same_or_new_session.
        let rig = Rig::new().await;
        rig.agent("lead", Some("%1"), Some("s1")).await;
        rig.command(OrgCommand::Stuck {
            agent_id: "p/lead".parse().unwrap(),
            reason: "manual recovery".into(),
        })
        .await;
        rig.inspect_only("%1", "p/lead");
        let (agents, _) = rig.start();
        agents.reconciled().await.unwrap();
        let mut events = agents.observation_events();
        let before = rig.org.status().unwrap();
        rig.frame("%1", "p/lead", IDLE);
        rig.row("s1", "%1", false).await;
        progress(&mut events, "finished", Some("p/lead")).await;
        assert_eq!(unread(&rig), 0);
        assert_eq!(rig.org.status().unwrap(), before);
        assert_eq!(
            told(&agents).await,
            (
                AgentStatus::Stuck,
                Some(SessionId::new("s1")),
                PaneBaseline::unproven(),
                Some(SessionId::new("s1"))
            )
        );
    }

    // --- An agent that keeps its state while every input to it is refused
    // says why in `agent.list` ---

    const SHELL_HOLDS_CHILD: &str =
        "input refused: the shell in %1 has a child and atm cannot tell which is in front";
    const NOT_CONFIRMED: &str =
        "input refused: the program in front in %1 is not confirmed as the agent's harness";
    const NO_COMMAND: &str =
        "input refused: tmux reports no root process or foreground command for %1";
    const UNREADABLE: &str = "input refused: the processes of %1 could not be read";

    /// The reason `agent.list` shows for the only agent.
    async fn listed_reason(agents: &AgentHandle) -> Option<String> {
        agents.list(ListParams::default()).await.unwrap()[0]
            .reason
            .clone()
    }
    /// `pinned_ready`, then its harness is suspended (the shell is in front
    /// and holds it) and another row on the pane wakes a pass that sees it.
    async fn refused_ready() -> (
        Rig,
        AgentHandle,
        broadcast::Receiver<crate::org::agents::ObservationProgress>,
    ) {
        let (rig, agents) = pinned_ready().await;
        let mut events = agents.observation_events();
        *rig.proc.lock().unwrap() = Some(shell_with_child("zsh"));
        inspect_with(&rig, "zsh");
        rig.row("pending-300", "%1", false).await;
        progress(&mut events, "finished", Some("p/lead")).await;
        assert_eq!(unread(&rig), 0);
        assert_eq!(
            listed_reason(&agents).await.as_deref(),
            Some(SHELL_HOLDS_CHILD)
        );
        (rig, agents, events)
    }

    #[test]
    fn refusal_text_table() {
        use super::super::agent_flow::PaneShape::{
            IdleShell, JobInFront, RootProgram, ShellHoldingChild,
        };
        let (unproven, adopted, pi) = (
            PaneBaseline::unproven(),
            PaneBaseline::adopted(100),
            pinned_to("pi"),
        );
        // Rows are tried top to bottom: what the inspection shows, what the
        // process read made of the pane, then what was recorded before.
        for (pane, stored, command, shape, text) in [
            ("%1", &pi, None, Some(JobInFront), NO_COMMAND),
            ("%1", &unproven, None, None, NO_COMMAND),
            ("%1", &pi, Some("pi"), None, UNREADABLE),
            ("%1", &unproven, Some("zsh"), None, UNREADABLE),
            (
                "%1",
                &pi,
                Some("zsh"),
                Some(IdleShell),
                "input refused: no harness is running in %1",
            ),
            (
                "%1",
                &pi,
                Some("zsh"),
                Some(ShellHoldingChild),
                SHELL_HOLDS_CHILD,
            ),
            (
                "%1",
                &unproven,
                Some("sh"),
                Some(ShellHoldingChild),
                SHELL_HOLDS_CHILD,
            ),
            (
                "%1",
                &pi,
                Some("vim"),
                Some(JobInFront),
                "input refused: vim is in front in %1, not pi",
            ),
            (
                "%7",
                &pi,
                Some("less"),
                Some(RootProgram),
                "input refused: less is in front in %7, not pi",
            ),
            ("%1", &unproven, Some("pi"), Some(JobInFront), NOT_CONFIRMED),
            (
                "%1",
                &unproven,
                Some("pi"),
                Some(RootProgram),
                NOT_CONFIRMED,
            ),
            // A baseline with no harness pinned, and the pinned name in front
            // of a root process that is not the recorded one.
            ("%1", &adopted, Some("pi"), Some(JobInFront), NOT_CONFIRMED),
            ("%1", &pi, Some("pi"), Some(JobInFront), NOT_CONFIRMED),
            (
                "%7",
                &unproven,
                Some("pi"),
                Some(JobInFront),
                "input refused: the program in front in %7 is not confirmed as the agent's harness",
            ),
        ] {
            assert_eq!(
                refusal_text(pane, stored, command, shape),
                text,
                "{pane} {stored:?} {command:?} {shape:?}"
            );
        }
    }

    #[tokio::test]
    async fn restart_that_pins_nothing_shows_why() {
        // After a daemon restart nothing is recorded about a pane, and a pane
        // that pins nothing refuses every input. Its agent keeps its recorded
        // state (a bound one is suspended, as before) and `agent.list` says
        // why, read right after the restart check with no wake in between.
        // The pass that pins the harness drops the reason.
        enum Script {
            Frame(&'static str),
            NoCommand,
        }
        let cases = [
            (
                "a shell holding a child",
                Some(shell_with_child("sh")),
                Script::Frame("sh"),
                true,
                SHELL_HOLDS_CHILD,
            ),
            (
                "unreadable processes",
                None,
                Script::Frame("pi"),
                true,
                UNREADABLE,
            ),
            (
                "no foreground command",
                Some(idle_shell("zsh")),
                Script::NoCommand,
                true,
                NO_COMMAND,
            ),
            (
                "a job in front and no row of the harness",
                Some(job()),
                Script::Frame("pi"),
                false,
                NOT_CONFIRMED,
            ),
        ];
        for (case, processes, script, row, text) in cases {
            for bound in [true, false] {
                let rig = Rig::new().await;
                rig.agent("lead", Some("%1"), bound.then_some("s1")).await;
                let id = if bound { "s1" } else { "pending-100" };
                if row {
                    rig.row(id, "%1", false).await;
                }
                *rig.proc.lock().unwrap() = processes.clone();
                // A bound agent's restart check reads its screen; one without
                // a session only inspects when nothing can be pinned.
                match (&script, bound) {
                    (Script::Frame(command), true) => rig.frame_of("%1", "p/lead", IDLE, command),
                    (Script::Frame(command), false) => inspect_with(&rig, command),
                    (Script::NoCommand, true) => {
                        rig.inspect_only("%1", "p/lead");
                        rig.client
                            .script_capture(&PaneCapture::new(
                                IDLE.into(),
                                Rig::inspection("%1", "p/lead", false),
                                CaptureFormat::Ansi,
                            ))
                            .unwrap();
                    }
                    (Script::NoCommand, false) => rig.inspect_only("%1", "p/lead"),
                }
                let (agents, _) = rig.start();
                agents.reconciled().await.unwrap();
                assert_eq!(unread(&rig), 0, "{case} {bound}");
                assert_eq!(
                    listed_reason(&agents).await.as_deref(),
                    Some(text),
                    "{case} {bound}"
                );
                let lead = rig.state("lead").await;
                let status = if bound {
                    AgentStatus::Starting
                } else {
                    AgentStatus::Ready
                };
                assert_eq!((lead.status, lead.reason), (status, None), "{case} {bound}");
                assert_eq!(told(&agents).await.2, PaneBaseline::unproven());

                // The harness is in front with its row: the pass pins it.
                let mut events = agents.observation_events();
                *rig.proc.lock().unwrap() = Some(job());
                rig.frame("%1", "p/lead", IDLE);
                if row {
                    update(&rig, id).await;
                } else {
                    rig.row(id, "%1", false).await;
                }
                progress(&mut events, "finished", Some("p/lead")).await;
                assert_eq!(unread(&rig), 0, "{case} {bound}");
                assert_eq!(told(&agents).await.2, pinned_to("pi"), "{case} {bound}");
                assert_eq!(listed_reason(&agents).await, None, "{case} {bound}");
                assert_eq!(rig.state("lead").await.status, AgentStatus::Ready);
            }
        }
    }

    #[tokio::test]
    async fn suspended_harness_shows_its_refusal_until_it_is_back() {
        // Ctrl-Z in the pane of a Ready agent: nothing is recorded, the agent
        // stays Ready, and the list says why it takes no input. Once a pass
        // finds the harness in front again the reason is gone. The resumed
        // harness sends only its own session's events, which start no pass
        // for a Ready agent: while a refusal is stored, they do. (Without
        // one they don't: review_ready_agents_own_session_events_start_no_observation.)
        let (rig, agents) = pinned_ready().await;
        let before = rig.org.status().unwrap();
        assert_eq!(listed_reason(&agents).await, None);
        let mut events = agents.observation_events();
        *rig.proc.lock().unwrap() = Some(shell_with_child("zsh"));
        inspect_with(&rig, "zsh");
        rig.row("pending-300", "%1", false).await;
        progress(&mut events, "finished", Some("p/lead")).await;
        assert_eq!(unread(&rig), 0);
        assert_eq!(
            listed_reason(&agents).await.as_deref(),
            Some(SHELL_HOLDS_CHILD)
        );
        assert_eq!(rig.state("lead").await.status, AgentStatus::Ready);

        inspect_with(&rig, "pi");
        update(&rig, "s1").await;
        progress(&mut events, "finished", Some("p/lead")).await;
        assert_eq!(unread(&rig), 0);
        assert_eq!(listed_reason(&agents).await, None);
        assert_eq!(rig.org.status().unwrap(), before);
        assert_eq!(rig.state("lead").await.status, AgentStatus::Ready);
    }

    #[tokio::test]
    async fn another_program_in_front_is_named() {
        // The harness is pinned and the pane now runs something else in
        // front (its owner quit the harness and opened an editor): the list
        // names both. Nothing is recorded and no capture is read.
        let (rig, agents) = pinned_ready().await;
        let before = rig.org.status().unwrap();
        let mut events = agents.observation_events();
        inspect_with(&rig, "vim");
        rig.row("pending-300", "%1", false).await;
        assert_eq!(captures_of_pass(&mut events).await, 0);
        assert_eq!(unread(&rig), 0);
        assert_eq!(
            listed_reason(&agents).await.as_deref(),
            Some("input refused: vim is in front in %1, not pi")
        );
        assert_eq!(rig.org.status().unwrap(), before);
        assert_eq!(rig.state("lead").await.status, AgentStatus::Ready);
        assert_eq!(told(&agents).await.2, pinned_to("pi"));
    }

    #[tokio::test]
    async fn recorded_stuck_or_down_hides_the_refusal() {
        // A refusal is the reason of an agent that has no recorded one. Once
        // the agent is recorded Stuck its own reason is shown, at once and
        // with no pass; the next pass over it drops the stored refusal.
        let lead: AgentId = "p/lead".parse().unwrap();
        let (rig, agents, mut events) = refused_ready().await;
        rig.command(OrgCommand::Stuck {
            agent_id: lead.clone(),
            reason: "manual recovery".into(),
        })
        .await;
        assert_eq!(
            listed_reason(&agents).await.as_deref(),
            Some("manual recovery")
        );
        assert_eq!(
            agents.refusal_of(&lead, "%1").as_deref(),
            Some(SHELL_HOLDS_CHILD)
        );
        // A Stuck agent's pass reads the screen for its session's row; with
        // the shell in front it reads Unknown and the agent stays Stuck.
        rig.frame_of("%1", "p/lead", IDLE, "zsh");
        update(&rig, "pending-300").await;
        progress(&mut events, "finished", Some("p/lead")).await;
        assert_eq!(unread(&rig), 0);
        assert_eq!(agents.refusal_of(&lead, "%1"), None);
        assert_eq!(
            listed_reason(&agents).await.as_deref(),
            Some("manual recovery")
        );

        // The shell is found idle: Down, with Down's reason, and nothing kept.
        let (rig, agents, mut events) = refused_ready().await;
        *rig.proc.lock().unwrap() = Some(idle_shell("zsh"));
        inspect_with(&rig, "zsh");
        update(&rig, "pending-300").await;
        progress(&mut events, "finished", Some("p/lead")).await;
        assert_eq!(rig.state("lead").await.status, AgentStatus::Down);
        assert_eq!(
            listed_reason(&agents).await.as_deref(),
            Some("the harness exited to the shell in %1")
        );
        assert_eq!(agents.refusal_of(&lead, "%1"), None);
    }

    #[tokio::test]
    async fn refusal_belongs_to_the_pane_it_was_read_on() {
        let lead: AgentId = "p/lead".parse().unwrap();
        let (_rig, agents, _) = refused_ready().await;
        assert_eq!(
            agents.refusal_of(&lead, "%1").as_deref(),
            Some(SHELL_HOLDS_CHILD)
        );
        assert_eq!(agents.refusal_of(&lead, "%2"), None);
    }

    #[tokio::test]
    async fn recording_a_baseline_drops_the_refusal() {
        // What an adopt or a start does when it has placed the harness: the
        // reason read before it is gone with no pass.
        let (_rig, agents, _) = refused_ready().await;
        Rig::pinned(&agents, "lead", "%1", "pi");
        assert_eq!(listed_reason(&agents).await, None);
    }

    #[tokio::test]
    async fn a_clear_drops_the_refusal() {
        // A clear types only after its own capture showed the harness in
        // front, so a refusal stored before it is out of date. No pass need
        // follow a clear (a harness that announces no new session wakes
        // nobody): the recorded clear itself drops the reason.
        let lead: AgentId = "p/lead".parse().unwrap();
        let (rig, agents, mut events) = refused_ready().await;
        rig.command(OrgCommand::Clear {
            agent_id: lead.clone(),
        })
        .await;
        assert_eq!(listed_reason(&agents).await, None);
        progress(&mut events, "refusal_dropped", Some("p/lead")).await;
        rig.command(OrgCommand::SessionBound {
            agent_id: lead.clone(),
            session_id: SessionId::new("s2"),
        })
        .await;
        assert_eq!(rig.state("lead").await.status, AgentStatus::Ready);
        assert_eq!(agents.refusal_of(&lead, "%1"), None);
        assert_eq!(listed_reason(&agents).await, None);
    }

    #[tokio::test]
    async fn refusal_and_failed_observation_are_both_shown() {
        // The refusal is what the last completed pass found; a pass that
        // fails after it adds its error, as it does to a recorded reason.
        let (rig, agents, mut events) = refused_ready().await;
        // Nothing scripted: the next pass cannot inspect the pane.
        update(&rig, "pending-300").await;
        progress(&mut events, "finished", Some("p/lead")).await;
        let reason = listed_reason(&agents).await.unwrap();
        assert!(
            reason.starts_with(&format!("{SHELL_HOLDS_CHILD}; observation unavailable: ")),
            "{reason}"
        );
    }

    // --- A quarantined agent says so in `agent.list` ---

    const QUARANTINED_DOWN: &str =
        "quarantined: the agent's down on %1 could not be settled; restart atmd and reconcile";

    /// A down of `p/lead` on %1 that does nothing, and whose cleanup fails
    /// (the control service then quarantines the agent and the pane) or not.
    async fn down_whose_cleanup(agents: &AgentHandle, fails: bool) {
        let result = agents
            .lifecycle(
                request(
                    "p/lead",
                    Some("%1"),
                    crate::control::reservation::ReservationKind::Down,
                ),
                |_| async { Ok(()) },
                move |_, _| async move {
                    if fails {
                        Err(AgentError::Internal("the down is not recorded".into()))
                    } else {
                        Ok(())
                    }
                },
            )
            .await;
        assert_eq!(result.is_err(), fails);
    }

    #[test]
    fn quarantine_text_names_the_operation_and_its_pane() {
        use crate::control::reservation::{Quarantine, ReservationKind};
        let quarantine = |pane: Option<&str>, kind| Quarantine {
            agent: "p/lead".parse().unwrap(),
            pane: pane.map(|pane| PaneId::parse(pane).unwrap()),
            kind,
        };
        assert_eq!(
            quarantine_text(&quarantine(Some("%1"), ReservationKind::Down)),
            QUARANTINED_DOWN
        );
        assert_eq!(
            quarantine_text(&quarantine(Some("%12"), ReservationKind::Respawn)),
            "quarantined: the agent's respawn on %12 could not be settled; restart atmd and reconcile"
        );
        assert_eq!(
            quarantine_text(&quarantine(None, ReservationKind::Up)),
            "quarantined: the agent's up could not be settled; restart atmd and reconcile"
        );
        // The other kinds are named as their `agent.<kind>` methods are; the
        // observer's own reservation is an observation.
        for (kind, name) in [
            (ReservationKind::Adopt, "adopt"),
            (ReservationKind::Clear, "clear"),
            (ReservationKind::Observe, "observation"),
        ] {
            assert_eq!(
                quarantine_text(&quarantine(Some("%1"), kind)),
                format!(
                    "quarantined: the agent's {name} on %1 could not be settled; \
                     restart atmd and reconcile"
                )
            );
        }
    }

    #[tokio::test]
    async fn quarantined_agent_shows_it_in_the_list_without_a_pass() {
        // A down whose kill has an unknown outcome, or whose Down could not
        // be written, leaves the agent Ready on a pane that may be gone, and
        // every later call refused. The list says so at once: it reads the
        // control service's quarantines, and waits for no observation.
        let (rig, agents) = pinned_ready().await;
        let mut events = agents.observation_events();
        let before = rig.org.status().unwrap();
        // The twin: an operation that settles leaves nothing to show.
        down_whose_cleanup(&agents, false).await;
        assert_eq!(listed_reason(&agents).await, None);
        down_whose_cleanup(&agents, true).await;
        assert_eq!(
            listed_reason(&agents).await.as_deref(),
            Some(QUARANTINED_DOWN)
        );
        assert_eq!(drain_started(&mut events), 0);
        assert_eq!(rig.org.status().unwrap(), before);
        assert_eq!(rig.state("lead").await.status, AgentStatus::Ready);
    }

    #[tokio::test]
    async fn quarantine_wins_over_a_refusal_and_a_failed_observation() {
        // The refusal was read on a pane that may no longer exist, and a pass
        // over a quarantined agent fails on the quarantine itself: the list
        // shows the quarantine alone, once.
        let (rig, agents, mut events) = refused_ready().await;
        down_whose_cleanup(&agents, true).await;
        assert_eq!(
            listed_reason(&agents).await.as_deref(),
            Some(QUARANTINED_DOWN)
        );
        update(&rig, "pending-300").await;
        progress(&mut events, "finished", Some("p/lead")).await;
        assert_eq!(
            listed_reason(&agents).await.as_deref(),
            Some(QUARANTINED_DOWN)
        );
    }

    #[tokio::test]
    async fn quarantine_follows_a_recorded_reason() {
        let (rig, agents) = pinned_ready().await;
        rig.command(OrgCommand::Stuck {
            agent_id: "p/lead".parse().unwrap(),
            reason: "manual recovery".into(),
        })
        .await;
        down_whose_cleanup(&agents, true).await;
        assert_eq!(
            listed_reason(&agents).await,
            Some(format!("manual recovery; {QUARANTINED_DOWN}"))
        );
    }

    // --- A capture is read only when it can pin the harness ---

    /// The rig's process reader, counting its reads.
    fn counting(
        rig: &Rig,
    ) -> (
        super::super::agent_proc::ProcReader,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let reads = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (counter, inner) = (reads.clone(), rig.reader());
        let reader: super::super::agent_proc::ProcReader = std::sync::Arc::new(move |pid| {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            inner(pid)
        });
        (reader, reads)
    }
    /// The captures `p/lead`'s next observation takes, counted until it ends.
    async fn captures_of_pass(
        events: &mut broadcast::Receiver<crate::org::agents::ObservationProgress>,
    ) -> usize {
        let mut captures = 0;
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                let event = events.recv().await.unwrap();
                if event.agent.as_ref().map(ToString::to_string).as_deref() != Some("p/lead") {
                    continue;
                }
                match event.phase {
                    "capture" => captures += 1,
                    "finished" => return,
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
        captures
    }
    /// `p/lead`, Ready and bound to `s1` on %1, with nothing recorded about
    /// its pane and no row of its harness there: the restart pass found no
    /// row and suspended it, and it was made Ready again without a pin.
    async fn ready_unproven() -> (
        Rig,
        AgentHandle,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let rig = Rig::new().await;
        rig.agent("lead", Some("%1"), Some("s1")).await;
        rig.frame("%1", "p/lead", IDLE);
        let (reader, reads) = counting(&rig);
        let (agents, _) = spawn_agents_with(
            rig.org.clone(),
            rig.registry.clone(),
            rig.control.clone(),
            rig.cancel.child_token(),
            reader,
            std::sync::Arc::new(|_| None),
        );
        agents.reconciled().await.unwrap();
        assert_eq!(unread(&rig), 0);
        assert_eq!(rig.state("lead").await.status, AgentStatus::Starting);
        rig.command(OrgCommand::EditorReady {
            agent_id: "p/lead".parse().unwrap(),
        })
        .await;
        assert_eq!(told(&agents).await.2, PaneBaseline::unproven());
        reads.store(0, std::sync::atomic::Ordering::SeqCst);
        (rig, agents, reads)
    }

    #[tokio::test]
    async fn ready_agent_without_a_live_row_is_not_captured() {
        // A Ready agent with nothing recorded about its pane and no row of
        // its harness there cannot be pinned: the adopt rule needs such a row.
        // Each wake reads the pane's processes (an exit to the shell must be
        // seen) and takes no capture. Here its session reports from another
        // pane, which wakes it every time.
        let (rig, agents, reads) = ready_unproven().await;
        let mut events = agents.observation_events();
        let before = rig.org.status().unwrap();
        for wake in 0..5 {
            inspect_with(&rig, "pi");
            if wake == 0 {
                rig.row("s1", "%2", false).await;
            } else {
                update(&rig, "s1").await;
            }
            assert_eq!(captures_of_pass(&mut events).await, 0, "wake {wake}");
            assert_eq!(unread(&rig), 0, "wake {wake}");
        }
        assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 5);
        assert_eq!(rig.org.status().unwrap(), before);
        assert_eq!(rig.state("lead").await.status, AgentStatus::Ready);
        assert_eq!(told(&agents).await.2, PaneBaseline::unproven());

        // The restart check of a Ready agent without a session, a job in
        // front and no row: of the scripted frame only the inspection (two
        // reads) is used, and its capture is left unread.
        let rig = Rig::new().await;
        rig.agent("lead", Some("%1"), None).await;
        rig.frame("%1", "p/lead", IDLE);
        let before = rig.org.status().unwrap();
        let scripted = unread(&rig);
        let (agents, _) = rig.start();
        agents.reconciled().await.unwrap();
        assert_eq!(unread(&rig), scripted - 2);
        assert_eq!(rig.org.status().unwrap(), before);
        assert_eq!(rig.state("lead").await.status, AgentStatus::Ready);
    }

    #[tokio::test]
    async fn ready_agent_with_a_live_row_is_captured_once_and_pinned() {
        // The twin: a row of its harness arrives on the pane. That pass reads
        // the processes, takes the one capture the adopt rule needs, and pins.
        // From then on the harness is known to be in front: no process read
        // and no capture.
        let (rig, agents, reads) = ready_unproven().await;
        let mut events = agents.observation_events();
        let before = rig.org.status().unwrap();
        rig.frame("%1", "p/lead", IDLE);
        rig.row("pending-100", "%1", false).await;
        assert_eq!(captures_of_pass(&mut events).await, 1);
        assert_eq!(unread(&rig), 0);
        assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(told(&agents).await.2, pinned_to("pi"));
        for wake in 0..3 {
            inspect_with(&rig, "pi");
            update(&rig, "pending-100").await;
            assert_eq!(captures_of_pass(&mut events).await, 0, "wake {wake}");
            assert_eq!(unread(&rig), 0, "wake {wake}");
        }
        assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(rig.org.status().unwrap(), before);
        assert_eq!(rig.state("lead").await.status, AgentStatus::Ready);
    }

    mod lifecycle {
        include!("agent_lifecycle_tests.rs");
        include!("agent_clear_tests.rs");
        include!("agent_up_tests.rs");
        include!("agent_start_dialog_tests.rs");
        include!("agent_respawn_tests.rs");
        include!("stop_tests.rs");
    }

    #[tokio::test]
    async fn duplicate_clear_conflicts_and_slot_survives_disconnect_through_cleanup() {
        use crate::control::reservation::ReservationKind;
        use std::{
            future::{poll_fn, Future},
            task::Poll,
        };
        let rig = Rig::new().await;
        let (agents, _) = rig.start();
        agents.reconciled().await.unwrap();
        let (entered, entry) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        let control = rig.control.clone();
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
        let (worked, work) = tokio::sync::oneshot::channel();
        let (cleaning, in_cleanup) = tokio::sync::oneshot::channel();
        let (clean, cleaned) = tokio::sync::oneshot::channel();
        let mut caller = Box::pin(agents.lifecycle(
            request("p/lead", Some("%1"), ReservationKind::Clear),
            move |_| async move {
                worked.send(()).unwrap();
                Ok(())
            },
            move |_, _| async move {
                cleaning.send(()).unwrap();
                cleaned.await.unwrap();
                Ok(())
            },
        ));
        poll_fn(|cx| match caller.as_mut().poll(cx) {
            Poll::Pending => Poll::Ready(()),
            Poll::Ready(r) => panic!("must admit and park: {r:?}"),
        })
        .await;
        // A later unrelated job acknowledges the FIFO dispatcher has seen the first.
        agents
            .lifecycle(
                request("p/other", None, ReservationKind::Down),
                |_| async { Ok(()) },
                cleanup,
            )
            .await
            .unwrap();
        assert!(matches!(
            agents
                .lifecycle(
                    request("p/lead", Some("%1"), ReservationKind::Clear),
                    |_| async { Ok(()) },
                    cleanup
                )
                .await,
            Err(AgentError::Conflict(_))
        ));
        drop(caller);
        release.send(()).unwrap();
        delivery.await.unwrap();
        work.await.unwrap();
        in_cleanup.await.unwrap();
        assert!(matches!(
            agents
                .lifecycle(
                    request("p/lead", Some("%1"), ReservationKind::Clear),
                    |_| async { Ok(()) },
                    cleanup
                )
                .await,
            Err(AgentError::Conflict(_))
        ));
        clean.send(()).unwrap();
    }

    #[tokio::test]
    async fn down_without_pane_serializes_with_up() {
        use crate::control::reservation::ReservationKind;
        let rig = Rig::new().await;
        let (agents, _) = rig.start();
        agents.reconciled().await.unwrap();
        let (entered, entry) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        let handle = agents.clone();
        let down = tokio::spawn(async move {
            handle
                .lifecycle(
                    request("p/lead", None, ReservationKind::Down),
                    move |_| async move {
                        entered.send(()).unwrap();
                        released.await.unwrap();
                        Ok(())
                    },
                    cleanup,
                )
                .await
        });
        entry.await.unwrap();
        assert!(matches!(
            agents
                .lifecycle(
                    request("p/lead", None, ReservationKind::Up),
                    |_| async { Ok(()) },
                    cleanup
                )
                .await,
            Err(AgentError::Conflict(_))
        ));
        release.send(()).unwrap();
        down.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn ambiguous_parents_do_not_restore_ready() {
        let rig = Rig::new().await;
        rig.agent("lead", Some("%1"), Some("old")).await;
        rig.row("old", "%1", false).await;
        rig.row("other", "%1", false).await;
        rig.frame("%1", "p/lead", IDLE);
        let (agents, _) = rig.start();
        agents.reconciled().await.unwrap();
        assert_eq!(rig.state("lead").await.status, AgentStatus::Starting);
    }
}

mod agent_project {
    use super::super::{agent_project::AgentProjects, agents::AgentError, OrgHandle};
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::Arc,
    };
    use tokio::sync::Barrier;
    use tokio_util::sync::CancellationToken;

    struct Rig {
        dir: tempfile::TempDir,
        org: OrgHandle,
        projects: AgentProjects,
        cancel: CancellationToken,
    }
    impl Rig {
        async fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let cancel = CancellationToken::new();
            let org = OrgHandle::open(dir.path().join("state"), cancel.clone()).await;
            let (projects, _) = AgentProjects::spawn(org.clone(), cancel.clone());
            Self {
                dir,
                org,
                projects,
                cancel,
            }
        }
        fn root(&self, name: &str) -> PathBuf {
            let root = self.dir.path().join(name);
            fs::create_dir_all(&root).unwrap();
            root
        }
        fn role(root: &Path, text: &str) {
            fs::create_dir_all(root.join(".atm/roles")).unwrap();
            fs::write(root.join(".atm/roles/lead.md"), text).unwrap();
        }
    }
    impl Drop for Rig {
        fn drop(&mut self) {
            self.cancel.cancel();
        }
    }

    #[tokio::test]
    async fn configuration_validation_matches_rpc_boundary() {
        let rig = Rig::new().await;
        let root = rig.root("project");
        let mut services =
            crate::server::methods::MethodServices::new(crate::registry::spawn_registry());
        services.agents = Some(super::AgentHandle::disabled("validation only"));
        let router = crate::server::methods::router(services).unwrap();
        for (path, name) in [
            (PathBuf::new(), "lead"),
            (PathBuf::from("relative"), "lead"),
            (root.clone(), "../escape"),
            (root.clone(), "x/y"),
            (root.clone(), "two words"),
        ] {
            let rpc = router
                .call(
                    super::caller(),
                    "agent.up",
                    serde_json::json!({"project_root":path,"name":name}),
                )
                .await
                .unwrap_err();
            let project = rig.projects.prepare(path, name.into()).await.unwrap_err();
            assert_eq!(project.to_string(), rpc.0.message);
            assert!(matches!(project, AgentError::InvalidParams(_)));
        }
        assert_eq!(rig.org.status().unwrap(), 0);
        assert!(!root.join(".atm").exists());
    }

    #[tokio::test]
    async fn first_use_creates_once() {
        let rig = Rig::new().await;
        let root = rig.root("project");
        let mut events = rig.org.committed.subscribe();
        let barrier = Arc::new(Barrier::new(3));
        let mut jobs = Vec::new();
        for _ in 0..2 {
            let projects = rig.projects.clone();
            let root = root.clone();
            let barrier = barrier.clone();
            jobs.push(tokio::spawn(async move {
                barrier.wait().await;
                projects.identify(root, "lead".into()).await.unwrap()
            }));
        }
        barrier.wait().await;
        for job in jobs {
            assert_eq!(job.await.unwrap().to_string(), "project/lead");
        }
        assert_eq!(
            fs::read_to_string(root.join(".atm/project.toml")).unwrap(),
            "id = \"project\"\n"
        );
        assert_eq!(rig.org.status().unwrap(), 1);
        let event = events.recv().await.unwrap();
        assert_eq!(event.kind, "project.registered");
        assert_eq!(event.seq, 1);
        assert!(matches!(
            events.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
        let state = rig.org.snapshot().await.unwrap();
        assert_eq!(state.projects().count(), 1);
        assert_eq!(
            state.projects().next().unwrap().root,
            root.to_str().unwrap()
        );
        assert_eq!(fs::read_dir(root.join(".atm")).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn same_slug_different_roots() {
        let rig = Rig::new().await;
        let a = rig.root("a/project");
        let b = rig.root("b/project");
        let (a, b) = tokio::join!(
            rig.projects.identify(a, "lead".into()),
            rig.projects.identify(b, "lead".into())
        );
        let mut ids = vec![a.unwrap().to_string(), b.unwrap().to_string()];
        ids.sort();
        assert_eq!(ids, ["project-2/lead", "project/lead"]);
        assert_eq!(rig.org.status().unwrap(), 2);
    }

    #[tokio::test]
    async fn linked_root_uses_main_role() {
        let rig = Rig::new().await;
        let main = rig.root("main");
        let linked = rig.root("linked");
        let meta = main.join(".git/worktrees/linked");
        fs::create_dir_all(&meta).unwrap();
        fs::write(linked.join(".git"), format!("gitdir: {}\n", meta.display())).unwrap();
        fs::write(meta.join("commondir"), "../..\n").unwrap();
        fs::write(
            meta.join("gitdir"),
            format!("{}\n", linked.join(".git").display()),
        )
        .unwrap();
        fs::create_dir(main.join("work")).unwrap();
        Rig::role(
            &main,
            "+++\nharness='pi'\ncwd='work'\ntrust_dirs=['work']\n+++\nmain instructions",
        );
        Rig::role(&linked, "+++\nharness='codex'\n+++\nwrong");
        let prepared = rig
            .projects
            .prepare(linked.clone(), "lead".into())
            .await
            .unwrap();
        assert_eq!(prepared.agent_id.to_string(), "main/lead");
        assert_eq!(prepared.root, main);
        assert_eq!(prepared.role.instructions, "main instructions");
        assert_eq!(prepared.role.config.harness, atm_core::Harness::Pi);
        assert_eq!(prepared.cwd, main.join("work"));
        assert_eq!(prepared.trust_dirs, [main.join("work")]);
        assert!(!linked.join(".atm/project.toml").exists());
    }

    #[tokio::test]
    async fn unsupported_gitdir_refused() {
        let rig = Rig::new().await;
        for pointer in [
            "garbage",
            "gitdir: /not/a/main/.git/modules/module",
            "gitdir: missing",
        ] {
            let root = rig.root("bad");
            fs::write(root.join(".git"), pointer).unwrap();
            assert!(matches!(
                rig.projects.identify(root.clone(), "lead".into()).await,
                Err(AgentError::Refused(_))
            ));
            assert!(!root.join(".atm").exists());
        }
        assert_eq!(rig.org.status().unwrap(), 0);
    }

    async fn copied(rig: &Rig) -> (PathBuf, PathBuf) {
        let old = rig.root("old");
        rig.projects
            .identify(old.clone(), "lead".into())
            .await
            .unwrap();
        let new = rig.root("new");
        fs::create_dir(new.join(".atm")).unwrap();
        fs::copy(old.join(".atm/project.toml"), new.join(".atm/project.toml")).unwrap();
        (old, new)
    }

    #[tokio::test]
    async fn copied_id_refused_old_root_still_exists() {
        let rig = Rig::new().await;
        let (old, new) = copied(&rig).await;
        assert!(matches!(
            rig.projects.identify(new, "lead".into()).await,
            Err(AgentError::Conflict(_))
        ));
        assert_eq!(
            rig.org
                .snapshot()
                .await
                .unwrap()
                .projects()
                .next()
                .unwrap()
                .root,
            old.to_str().unwrap()
        );
        assert_eq!(rig.org.status().unwrap(), 1);
    }

    #[tokio::test]
    async fn missing_old_root_allows_move() {
        let rig = Rig::new().await;
        let (old, new) = copied(&rig).await;
        fs::remove_dir_all(old).unwrap();
        assert_eq!(
            rig.projects
                .identify(new.clone(), "lead".into())
                .await
                .unwrap()
                .to_string(),
            "old/lead"
        );
        assert_eq!(
            rig.org
                .snapshot()
                .await
                .unwrap()
                .projects()
                .next()
                .unwrap()
                .root,
            new.to_str().unwrap()
        );
        assert_eq!(rig.org.status().unwrap(), 2);
    }

    #[tokio::test]
    async fn invalid_or_unreadable_old_root_not_move_permission() {
        let rig = Rig::new().await;
        let (old, new) = copied(&rig).await;
        let file = old.join(".atm/project.toml");
        fs::write(&file, "invalid TOML").unwrap();
        assert!(matches!(
            rig.projects.identify(new.clone(), "lead".into()).await,
            Err(AgentError::Refused(_))
        ));
        fs::remove_file(&file).unwrap();
        fs::create_dir(&file).unwrap(); // EISDIR, deterministic even under root.
        assert!(rig.projects.identify(new, "lead".into()).await.is_err());
        assert_eq!(
            rig.org
                .snapshot()
                .await
                .unwrap()
                .projects()
                .next()
                .unwrap()
                .root,
            old.to_str().unwrap()
        );
        assert_eq!(rig.org.status().unwrap(), 1);
    }

    #[tokio::test]
    async fn dangling_old_root_is_not_move_permission() {
        let rig = Rig::new().await;
        let (old, new) = copied(&rig).await;
        fs::remove_dir_all(&old).unwrap();
        std::os::unix::fs::symlink(rig.dir.path().join("missing"), &old).unwrap();
        assert!(matches!(
            rig.projects.identify(new, "lead".into()).await,
            Err(AgentError::Refused(_))
        ));
        assert_eq!(rig.org.status().unwrap(), 1);
    }

    #[tokio::test]
    async fn malformed_recorded_root_is_not_move_permission() {
        let rig = Rig::new().await;
        let new = rig.root("new");
        fs::create_dir(new.join(".atm")).unwrap();
        fs::write(new.join(".atm/project.toml"), "id='old'\n").unwrap();
        rig.org
            .execute(
                atm_org::OrgCommand::RegisterProject {
                    project_id: "old".parse().unwrap(),
                    root: "relative-missing".into(),
                    move_root: false,
                },
                chrono::Utc::now(),
            )
            .await
            .unwrap();
        assert!(matches!(
            rig.projects.identify(new, "lead".into()).await,
            Err(AgentError::Refused(_))
        ));
        assert_eq!(rig.org.status().unwrap(), 1);
    }

    #[tokio::test]
    async fn list_does_not_create() {
        let rig = Rig::new().await;
        let root = rig.root("project");
        assert_eq!(rig.projects.filter(root.clone()).await.unwrap(), None);
        assert!(!root.join(".atm").exists());
        fs::create_dir(root.join(".atm")).unwrap();
        fs::write(root.join(".atm/project.toml"), "id='existing'\n").unwrap();
        assert_eq!(
            rig.projects
                .filter(root.clone())
                .await
                .unwrap()
                .unwrap()
                .to_string(),
            "existing"
        );
        fs::write(root.join(".atm/project.toml"), "invalid").unwrap();
        assert!(matches!(
            rig.projects.filter(root).await,
            Err(AgentError::Refused(_))
        ));
        assert_eq!(rig.org.status().unwrap(), 0);
    }

    #[tokio::test]
    async fn missing_role_and_nonabsolute_root_fail_before_registration() {
        let rig = Rig::new().await;
        assert!(matches!(
            rig.projects
                .identify("relative".into(), "lead".into())
                .await,
            Err(AgentError::InvalidParams(_))
        ));
        let root = rig.root("project");
        assert!(matches!(
            rig.projects.prepare(root.clone(), "lead".into()).await,
            Err(AgentError::NotFound(_))
        ));
        assert!(!root.join(".atm").exists());
        assert_eq!(rig.org.status().unwrap(), 0);
    }

    #[tokio::test]
    async fn git_environment_cannot_redirect_project_io() {
        // Run the real linked-root test in a subprocess to avoid changing this
        // test process's environment concurrently. No Git subprocess is needed
        // by either the implementation or its synthetic checkout fixtures.
        let decoy = tempfile::tempdir().unwrap();
        let git = decoy.path().join(".git");
        fs::create_dir_all(git.join("objects")).unwrap();
        fs::create_dir_all(git.join("refs/heads")).unwrap();
        fs::write(git.join("HEAD"), "ref: refs/heads/decoy\n").unwrap();
        fs::write(git.join("config"), "[core]\nrepositoryformatversion=0\n").unwrap();
        fs::write(git.join("index"), "decoy sentinel").unwrap();
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            tokio::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "org::agents_tests::agent_project::linked_root_uses_main_role",
                    "--nocapture",
                ])
                .env("GIT_DIR", &git)
                .env("GIT_INDEX_FILE", git.join("index"))
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        assert_eq!(
            fs::read_to_string(git.join("index")).unwrap(),
            "decoy sentinel"
        );
        assert_eq!(
            fs::read_to_string(git.join("HEAD")).unwrap(),
            "ref: refs/heads/decoy\n"
        );
        assert_eq!(
            fs::read_to_string(git.join("config")).unwrap(),
            "[core]\nrepositoryformatversion=0\n"
        );
        assert_eq!(fs::read_dir(git.join("objects")).unwrap().count(), 0);
        assert_eq!(fs::read_dir(git.join("refs/heads")).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn role_name_traversal_refused() {
        let rig = Rig::new().await;
        let root = rig.root("project");
        for name in ["../secret", "/absolute", "a/b", "owner"] {
            assert!(matches!(
                rig.projects.prepare(root.clone(), name.into()).await,
                Err(AgentError::InvalidParams(_))
            ));
            assert!(matches!(
                rig.projects.identify(root.clone(), name.into()).await,
                Err(AgentError::InvalidParams(_))
            ));
        }
        assert!(!root.join(".atm").exists());
    }

    #[tokio::test]
    async fn bad_respawn_role_precedes_kill() {
        let rig = Rig::new().await;
        let root = rig.root("project");
        // The same preparation seam is required before a respawn can acquire
        // its pane reservation. No lifecycle intent is recorded on failure.
        for role in [
            "+++\nharness='pi'\ntypo='secret'\n+++\nprivate",
            "+++\nharness='pi'\ncwd='missing'\n+++",
            "+++\nharness='pi'\ntrust_dirs=['missing']\n+++",
        ] {
            Rig::role(&root, role);
            let error = rig
                .projects
                .prepare(root.clone(), "lead".into())
                .await
                .unwrap_err();
            assert!(matches!(error, AgentError::Refused(_)));
            assert!(!error.to_string().contains("private"));
            assert!(!error.to_string().contains("secret"));
            assert_eq!(rig.org.status().unwrap(), 0);
            assert!(!root.join(".atm/project.toml").exists());
        }
    }
}

#[tokio::test]
async fn agent_unavailable_org_preserves_actual_reason() {
    let mut services = MethodServices::new(crate::registry::spawn_registry());
    services.org = super::OrgHandle::disabled("storage could not be opened");
    let router = router(services).unwrap();
    let error = router
        .call(caller(), "agent.list", json!({}))
        .await
        .unwrap_err();
    assert_eq!(error.0.code, "unavailable");
    assert!(error.0.message.contains("storage could not be opened"));
}

#[tokio::test]
async fn agent_contract_disabled_has_no_effects() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = tokio_util::sync::CancellationToken::new();
    let org = super::OrgHandle::open(dir.path().into(), cancel.clone()).await;
    let registry = crate::registry::spawn_registry();
    let mock = atm_tmux::MockTmuxClient::new();
    let (control, control_task) = crate::control::service::spawn_control_with_org(
        Box::new(mock.clone()),
        registry.clone(),
        org.clone(),
        cancel.clone(),
    );
    let root = dir.path().join("project");
    std::fs::create_dir_all(root.join(".atm/roles")).unwrap();
    let role = "+++\nharness='pi'\n+++\n";
    std::fs::write(root.join(".atm/roles/lead.md"), role).unwrap();
    for command in [
        atm_org::OrgCommand::RegisterProject {
            project_id: "project".parse().unwrap(),
            root: root.to_str().unwrap().into(),
            move_root: false,
        },
        atm_org::OrgCommand::Up {
            agent_id: "project/lead".parse().unwrap(),
            role: atm_org::parse_role("lead", role).unwrap(),
        },
        atm_org::OrgCommand::PaneCreated {
            agent_id: "project/lead".parse().unwrap(),
            pane: "%7".into(),
        },
    ] {
        org.execute(command, chrono::Utc::now()).await.unwrap();
    }
    let before = org.status().unwrap();
    let state = org.snapshot().await.unwrap();
    let agents = AgentHandle::disabled("off");
    let mut services = MethodServices::new(registry);
    services.org = org.clone();
    services.control = Some(control);
    services.agents = Some(agents.clone());
    let router = router(services).unwrap();
    for (method, mut params) in requests() {
        if params.get("project_root").is_some() {
            params["project_root"] = json!(root);
        }
        let error = router.call(caller(), method, params).await.unwrap_err();
        assert_eq!(error.0.code, "unavailable");
        assert_eq!(error.0.message, "agent service unavailable: off");
    }
    assert!(matches!(
        agents.reconciled().await,
        Err(AgentError::Unavailable(_))
    ));
    assert!(mock.calls().is_empty());
    assert_eq!(org.status().unwrap(), before);
    assert_eq!(org.snapshot().await.unwrap(), state);
    assert!(!root.join(".atm/project.toml").exists());
    assert_eq!(
        std::fs::read_to_string(root.join(".atm/roles/lead.md")).unwrap(),
        role
    );
    cancel.cancel();
    control_task.await.unwrap();
}

#[test]
fn agent_contract_schemas_keep_defaults_and_optional_session() {
    let clear: ClearParams = serde_json::from_value(json!({"agent":"project/lead"})).unwrap();
    assert_eq!(clear.timeout_ms, 30_000);
    assert!(
        !serde_json::from_value::<DownParams>(json!({"agent":"project/lead"}))
            .unwrap()
            .kill
    );
    assert!(serde_json::from_value::<ListParams>(json!({}))
        .unwrap()
        .project_root
        .is_none());
    assert!(serde_json::from_value::<ClearParams>(
        json!({"agent":"project/lead","timeout_ms":null})
    )
    .is_err());
    let result = ClearResult {
        agent_id: clear.agent,
        session_id: None,
    };
    assert_eq!(
        serde_json::to_value(&result).unwrap(),
        json!({"agent_id":"project/lead","session_id":null})
    );
    let roundtrip: ClearResult =
        serde_json::from_value(json!({"agent_id":"project/lead","session_id":null,"future":true}))
            .unwrap();
    assert_eq!(roundtrip.agent_id, result.agent_id);
    assert!(roundtrip.session_id.is_none());
    for (method, params) in requests() {
        let value = match method {
            "agent.up" => {
                serde_json::to_value(serde_json::from_value::<UpParams>(params.clone()).unwrap())
                    .unwrap()
            }
            "agent.adopt" => {
                serde_json::to_value(serde_json::from_value::<AdoptParams>(params.clone()).unwrap())
                    .unwrap()
            }
            "agent.clear" => {
                serde_json::to_value(serde_json::from_value::<ClearParams>(params.clone()).unwrap())
                    .unwrap()
            }
            "agent.respawn" => serde_json::to_value(
                serde_json::from_value::<RespawnParams>(params.clone()).unwrap(),
            )
            .unwrap(),
            "agent.down" => {
                serde_json::to_value(serde_json::from_value::<DownParams>(params.clone()).unwrap())
                    .unwrap()
            }
            "agent.list" => {
                serde_json::to_value(serde_json::from_value::<ListParams>(params.clone()).unwrap())
                    .unwrap()
            }
            _ => unreachable!(),
        };
        for (key, expected) in params.as_object().unwrap() {
            assert_eq!(value.get(key), Some(expected), "{method}/{key}");
        }
    }
}

#[test]
fn agent_contract_error_codes_preserve_actionable_messages() {
    use super::OrgServiceError;
    use crate::control::{
        reservation::{PaneId, ReservationError},
        ControlError,
    };
    use crate::server::methods::MethodError;
    let cases = [
        (
            AgentError::InvalidParams("bad root".into()),
            "invalid_params",
        ),
        (
            AgentError::NotFound("agent project/lead was not found".into()),
            "not_found",
        ),
        (AgentError::Conflict("busy".into()), "conflict"),
        (AgentError::Refused("unsafe editor".into()), "refused"),
        (AgentError::Timeout, "timeout"),
        (AgentError::Internal("storage".into()), "internal"),
        (ReservationError::Unavailable.into(), "unavailable"),
        (OrgServiceError::Timeout.into(), "timeout"),
        (
            OrgServiceError::Unavailable("disk".into()).into(),
            "unavailable",
        ),
        (
            OrgServiceError::Refused(atm_org::Refusal::Invariant(
                atm_org::ApplyError::UnknownAgent("project/lead".parse().unwrap()),
            ))
            .into(),
            "not_found",
        ),
    ];
    for (error, code) in cases {
        let message = error.to_string();
        let mapped = MethodError::from(error);
        assert_eq!(mapped.0.code, code);
        assert_eq!(mapped.0.message, message);
    }
    let error = MethodError::from(AgentError::from(ReservationError::ScreenChanged {
        pane: PaneId::parse("%7").unwrap(),
    }));
    assert_eq!(error.0.code, "refused");
    assert_eq!(
        error.0.message,
        "Refusing input to %7: screen changed or is no longer safe; nothing was sent by this action"
    );
    let error = MethodError::from(AgentError::from(ReservationError::Control(
        ControlError::AgentClearing {
            pane: "%7".into(),
            agent: "project/lead".parse().unwrap(),
        },
    )));
    assert_eq!(error.0.code, "refused");
    assert_eq!(
        error.0.message,
        "Refusing to send to %7: agent project/lead is clearing; try again in a moment"
    );
}

fn caller() -> CallContext {
    CallContext {
        client_id: CallerId("contract".into()),
        from_pane: None,
    }
}

fn requests() -> Vec<(&'static str, Value)> {
    vec![
        ("agent.up", json!({"project_root":"/project","name":"lead"})),
        (
            "agent.adopt",
            json!({"project_root":"/project","name":"lead","pane":"%7"}),
        ),
        ("agent.clear", json!({"agent":"project/lead"})),
        ("agent.respawn", json!({"agent":"project/lead"})),
        ("agent.down", json!({"agent":"project/lead"})),
        ("agent.list", json!({})),
    ]
}

// Missing registration must not masquerade as an old daemon; unavailable is the
// checkpoint contract, not unknown_method or a fabricated successful result.
#[tokio::test]
async fn agent_contract_advertises_six_disabled_methods() {
    let router = router(MethodServices::new(crate::registry::spawn_registry())).unwrap();
    let info = router
        .call(caller(), "daemon.info", Value::Null)
        .await
        .unwrap();
    for (method, params) in requests() {
        assert!(router.names().iter().any(|name| name == method), "{method}");
        assert!(info["methods"].as_array().unwrap().contains(&json!(method)));
        let error = router.call(caller(), method, params).await.unwrap_err();
        assert_eq!(error.0.code, "unavailable", "{method}");
        assert_eq!(
            error.0.message,
            "agent service unavailable: organisation service unavailable: organisation service not configured"
        );
    }
}

#[tokio::test]
async fn agent_contract_rejects_unknown_and_nonobject_parameters() {
    let router = router(MethodServices::new(crate::registry::spawn_registry())).unwrap();
    for (method, mut params) in requests() {
        params
            .as_object_mut()
            .unwrap()
            .insert("typo".into(), json!(true));
        for bad in [params, json!([]), json!(true), json!("bad")] {
            let error = router.call(caller(), method, bad).await.unwrap_err();
            assert_eq!(error.0.code, "invalid_params", "{method}");
        }
    }
}
