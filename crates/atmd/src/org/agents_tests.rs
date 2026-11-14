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
    struct Rig {
        org: super::super::OrgHandle,
        registry: RegistryHandle,
        events: broadcast::Sender<crate::registry::SessionEvent>,
        client: ScriptedTmuxClient,
        control: crate::control::service::ControlHandle,
        cancel: CancellationToken,
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
            }
        }
        async fn command(&self, command: OrgCommand) {
            self.org.execute(command, chrono::Utc::now()).await.unwrap();
        }
        async fn agent(&self, name: &str, pane: Option<&str>, session: Option<&str>) {
            let id = format!("p/{name}").parse().unwrap();
            self.command(OrgCommand::Up {
                agent_id: id,
                role: atm_org::parse_role(name, "+++\nharness='pi'\n+++\n").unwrap(),
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
            let mut row = SessionDomain::new(
                SessionId::new(id),
                AgentType::GeneralPurpose,
                Model::default(),
            );
            row.harness = Harness::Pi;
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
            let mut inspection = Self::inspection(pane, tag, false);
            inspection.pane.height = text.lines().count() as u16;
            self.client
                .script_inspect(&PaneId::parse(pane).unwrap(), Some(&inspection))
                .unwrap();
            self.client
                .script_capture(&PaneCapture::new(
                    text.into(),
                    inspection,
                    CaptureFormat::Plain,
                ))
                .unwrap();
        }
        fn start(&self) -> (AgentHandle, tokio::task::JoinHandle<()>) {
            spawn_agents(
                self.org.clone(),
                self.registry.clone(),
                self.control.clone(),
                self.cancel.child_token(),
            )
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
        // No outcome of a capture can change a session-less Ready agent, so the
        // restart check must not read its screen, or fail when it can't.
        for capture in [true, false] {
            let rig = Rig::new().await;
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
            assert_eq!(
                agents.list(ListParams::default()).await.unwrap()[0].reason,
                None
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

    // Adapted from the A1 review probes: assertions describe the desired behavior.
    async fn progress(
        events: &mut broadcast::Receiver<crate::org::agents::ObservationProgress>,
        phase: &str,
        agent: Option<&str>,
    ) {
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
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
        let rig = Rig::new().await;
        rig.agent("lead", Some("%1"), Some("recorded")).await;
        rig.row("recorded", "%1", false).await;
        rig.frame("%1", "p/lead", IDLE);
        let (agents, _) = rig.start();
        agents.reconciled().await.unwrap();
        let mut events = agents.observation_events();
        for _ in 0..2 {
            let before = rig.org.status().unwrap();
            // No screen read: neither idle nor busy activity changes readiness.
            rig.inspect_only("%1", "p/lead");
            update(&rig, "recorded").await;
            progress(&mut events, "finished", Some("p/lead")).await;
            assert_eq!(rig.state("lead").await.status, AgentStatus::Ready);
            assert_eq!(rig.org.status().unwrap(), before);
        }
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
        rig.inspect_only("%1", "p/lead");
        update(&rig, "fresh").await;
        progress(&mut events, "finished", Some("p/lead")).await;
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
            let mut events = agents.observation_events();
            rig.client
                .script_inspect(
                    &PaneId::parse("%1").unwrap(),
                    Some(&Rig::inspection("%1", "p/lead", false)),
                )
                .unwrap();
            rig.row(session, "%1", false).await;
            progress(&mut events, "finished", Some("p/lead")).await;
            assert_eq!(rig.state("lead").await.status, AgentStatus::Ready);
            assert_eq!(
                rig.state("lead").await.session_id,
                Some(SessionId::new(session))
            );
        }
    }

    #[tokio::test]
    async fn review_updated_only_wake_binds_current_row() {
        let rig = Rig::new().await;
        rig.agent("lead", Some("%1"), None).await;
        // Registered before the service subscribes: no Registered wake exists.
        rig.row("fresh", "%1", false).await;
        rig.inspect_only("%1", "p/lead");
        let (agents, _) = rig.start();
        agents.reconciled().await.unwrap();
        assert_eq!(rig.state("lead").await.session_id, None);
        let mut events = agents.observation_events();
        rig.inspect_only("%1", "p/lead");
        update(&rig, "fresh").await;
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

    #[tokio::test]
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
        assert!(matches!(
            agents
                .down(DownParams {
                    agent: "p/good".parse().unwrap(),
                    kill: false,
                })
                .await,
            Err(AgentError::NotImplemented { .. })
        ));
        assert!(matches!(
            agents
                .down(DownParams {
                    agent: "p/bad".parse().unwrap(),
                    kill: false,
                })
                .await,
            Err(AgentError::Unavailable(_))
        ));
        let mut events = agents.observation_events();
        rig.frame("%2", "p/bad", IDLE);
        update(&rig, "bad").await;
        progress(&mut events, "finished", Some("p/bad")).await;
        assert!(agents
            .list(ListParams::default())
            .await
            .unwrap()
            .iter()
            .all(|a| a.reason.is_none()));
    }

    #[tokio::test(start_paused = true)]
    async fn review_transient_schedule_failure_does_not_stop_service() {
        let (gate, mut entered) = mpsc::channel(1);
        let rig = Rig::gated(Some(("agent.session_lost", gate))).await;
        rig.agent("lead", Some("%1"), Some("recorded")).await;
        rig.row("recorded", "%1", false).await;
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
        update(&rig, "recorded").await;
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
            Err(AgentError::NotImplemented {
                operation: "agent.up"
            })
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
        rig.frame("%1", "p/lead", IDLE);
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
        rig.frame("%1", "p/lead", IDLE);
        release.send(()).unwrap();
        held.await.unwrap();
        progress(&mut events, "finished", Some("p/lead")).await;
        assert_eq!(rig.state("lead").await.session_id, None);
        assert_eq!(
            rig.state("lead").await.excluded_session_id,
            Some(SessionId::new("s1"))
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
            loop {
                let event = events.recv().await.unwrap();
                assert_ne!(
                    event.phase, "schedule_failed",
                    "unrelated wake read stalled org"
                );
                if event.phase == "scheduled" {
                    break;
                }
            }
            for registered in [true, false] {
                if registered {
                    rig.row("unrelated", "%99", false).await;
                } else {
                    update(&rig, "unrelated").await;
                }
                loop {
                    let event = events.recv().await.unwrap();
                    assert_ne!(
                        event.phase, "schedule_failed",
                        "unrelated registration/update read stalled org"
                    );
                    if event.phase == "scheduled" {
                        break;
                    }
                }
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
            loop {
                let event = events.recv().await.unwrap();
                assert_ne!(
                    event.phase, "schedule_failed",
                    "ineligible {id} row read stalled org"
                );
                if event.phase == "scheduled" {
                    break;
                }
            }
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
        let mut attempts = 0;
        loop {
            let event = events.recv().await.unwrap();
            if event.phase == "started" {
                attempts += 1;
            }
            if event.phase == "dispatched" {
                break;
            }
        }
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
                let (agents, task) = spawn_agents(
                    rig.org.clone(),
                    rig.registry.clone(),
                    rig.control.clone(),
                    cancel.clone(),
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
                let (agents, task) = spawn_agents(
                    rig.org.clone(),
                    rig.registry.clone(),
                    rig.control.clone(),
                    cancel.clone(),
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

    #[tokio::test]
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
        for _ in 0..2 {
            rig.inspect_only("%1", "p/lead");
            update(&rig, "pending-100").await;
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
            atm_pi_adapter::screen::observe_screen(running, None).turn,
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
        let rows = agents.list(ListParams::default()).await.unwrap();
        assert!(rows[0]
            .reason
            .as_deref()
            .unwrap()
            .contains("observation unavailable"));
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
        rig.row("other-row", "%2", false).await;
        rig.frame("%1", "p/lead", IDLE);
        rig.frame("%2", "p/other", "unknown\n");
        let cancel = CancellationToken::new();
        let (agents, task) = spawn_agents(
            rig.org.clone(),
            rig.registry.clone(),
            rig.control.clone(),
            cancel.clone(),
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
            .get_session(SessionId::new("lead-row"))
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

    #[tokio::test]
    async fn lagged_events_reread_current_registry_instead_of_replaying_old_rows() {
        let rig = Rig::new().await;
        rig.agent("lead", Some("%1"), Some("recorded")).await;
        rig.row("current", "%1", false).await;
        rig.frame("%1", "p/lead", IDLE);
        let (agents, _) = rig.start();
        agents.reconciled().await.unwrap();
        assert_eq!(
            rig.state("lead").await.session_id,
            Some(SessionId::new("recorded"))
        );
        rig.inspect_only("%1", "p/lead");
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
        let (agents, task) = spawn_agents(
            rig.org.clone(),
            rig.registry.clone(),
            rig.control.clone(),
            cancel.clone(),
        );
        agents.reconciled().await.unwrap();
        cancel.cancel();
        task.await.unwrap();
        assert!(matches!(
            agents.reconciled().await,
            Err(AgentError::Unavailable(_))
        ));
    }

    #[tokio::test]
    async fn interrupted_start_without_binding_is_not_idle_revalidation() {
        let rig = Rig::new().await;
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
        rig.row("pending-100", "%1", false).await;
        rig.frame("%1", "p/lead", IDLE);
        let (agents, _) = rig.start();
        agents.reconciled().await.unwrap();
        assert_eq!(rig.state("lead").await.status, AgentStatus::Starting);
        assert!(rig.state("lead").await.session_id.is_none());
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
        let (agents, task) = spawn_agents(
            rig.org.clone(),
            dead,
            rig.control.clone(),
            rig.cancel.child_token(),
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

    #[tokio::test]
    async fn up_prepares_config_but_no_lifecycle_effects_until_flow_exists() {
        let rig = Rig::new().await;
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".atm/roles")).unwrap();
        std::fs::write(
            dir.path().join(".atm/roles/lead.md"),
            "+++\nharness='pi'\n+++\n",
        )
        .unwrap();
        let (agents, _) = rig.start();
        agents.reconciled().await.unwrap();
        let result = agents
            .up(
                UpParams {
                    project_root: dir.path().into(),
                    name: "lead".into(),
                    caller_server: None,
                },
                caller(),
            )
            .await;
        assert!(matches!(
            result,
            Err(AgentError::NotImplemented {
                operation: "agent.up"
            })
        ));
        assert!(dir.path().join(".atm/project.toml").is_file());
        assert!(rig.org.snapshot().await.unwrap().agents().next().is_none());
        assert!(rig.client.mock().calls().is_empty());
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
