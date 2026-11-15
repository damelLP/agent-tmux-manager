// `agent.respawn` over the real control actor and a scripted tmux client.
//
// Included after `agent_start_dialog_tests.rs`, into the same module:
// `Starting` and the helpers of the files before it are in scope, and the
// clock is handled as there (real until the flow has read its role, then
// paused). Each test first starts the agent on `%1` with `agent.up`; the
// respawn kills that pane and starts the agent again on `%2`.

impl Starting {
    /// `capture`, as taken from `pane`.
    fn moved(mut capture: PaneCapture, pane: &str) -> PaneCapture {
        capture.inspection.pane.pane_id = pane.into();
        capture
    }

    /// The agent started and Ready on `%1`, every observation of it done.
    /// (harness, its ready editor, the command tmux shows for it)
    async fn ready(harness: (Harness, &str, &str)) -> Self {
        Self::ready_on(Rig::new().await, harness).await
    }

    /// As [`Self::ready`], on `rig`.
    async fn ready_on(rig: Rig, (harness, ready, command): (Harness, &str, &str)) -> Self {
        let mut starting = Self::with_role(rig, harness, "").await;
        starting.script_launch();
        starting.frame(Self::screen(ready, command));
        if harness == Harness::Codex {
            starting.follow_up();
            starting.follow_up();
            starting.up().await.unwrap();
        } else {
            starting.frame(Self::screen(ready, command));
            starting.follow_up();
            let call = starting.start().await;
            progress(&mut starting.events, "up_awaiting_session", Some("q/lead")).await;
            starting.follow_up();
            start_session(&starting.rig, harness, "s1", Some("startup")).await;
            call.await.unwrap().unwrap();
        }
        starting.settled().await;
        assert_eq!(starting.unread(), 0);
        assert_eq!(starting.agent().await.unwrap().status, AgentStatus::Ready);
        starting.rig.client.mock().clear_calls();
        starting
    }

    fn inspect(&self, id: &str, tag: Option<&str>) {
        self.rig
            .client
            .script_inspect(&pane(id), Some(&Self::inspection(id, tag, "zsh")))
            .unwrap();
    }

    /// What a respawn reads up to the new pane's tag: the pane to split, the
    /// old pane, the kill's own ownership check, the old pane gone, the pane
    /// to split again, and the tag check of the new pane `%2`.
    fn script_replace(&self) {
        self.inspect("%0", None);
        self.inspect("%1", Some("q/lead"));
        self.rig
            .client
            .script_tag_check(&Self::inspection("%1", Some("q/lead"), "zsh"))
            .unwrap();
        self.rig.client.script_inspect(&pane("%1"), None).unwrap();
        self.inspect("%0", None);
        self.rig.client.mock().set_next_pane_id("%2");
        self.rig
            .client
            .script_tag_check(&Self::inspection("%2", None, "zsh"))
            .unwrap();
    }

    /// The new pane's frames from its idle shell through the launch's Enter.
    fn script_launch_on_new(&self) {
        for _ in 0..3 {
            self.frame(Self::moved(Self::shell(PROMPT), "%2"));
        }
        self.frame(Self::moved(self.staged(), "%2"));
        self.frame(Self::moved(self.staged(), "%2"));
    }

    /// Respawn the agent, and pause the clock once the flow has read its role.
    async fn respawning(
        &mut self,
        from_pane: Option<&str>,
    ) -> tokio::task::JoinHandle<Result<RespawnResult, AgentError>> {
        let agents = self.agents.clone();
        let mut caller = caller();
        caller.from_pane = from_pane.map(|pane| crate::server::methods::CallerPane(pane.into()));
        if PAUSED.replace(false) {
            tokio::time::resume();
        }
        self.events = self.agents.observation_events();
        let call = tokio::spawn(async move {
            let params = RespawnParams {
                agent: started(),
                caller_server: None,
            };
            agents.respawn(params, caller).await
        });
        progress(&mut self.events, "up_prepared", Some("q/lead")).await;
        tokio::time::pause();
        PAUSED.set(true);
        call
    }

    async fn respawn(&mut self) -> Result<RespawnResult, AgentError> {
        self.respawning(None).await.await.unwrap()
    }

    /// The effects of a respawn through its launch's Enter.
    fn respawned(&self) -> Vec<String> {
        vec![
            "kill %1".into(),
            "split %0 Below detached=true command=None".into(),
            "set-option -p -t %2 @atm-agent q/lead".into(),
            format!("paste %2 {}", self.text),
            "keys %2 Enter".into(),
        ]
    }

    fn unread_all(&self) -> usize {
        self.unread() + self.rig.client.pending_reads(&pane("%2")).unwrap_or(0)
    }
}

const CODEX: (Harness, &str, &str) = (Harness::Codex, CODEX_FRESH, "codex");
const CLAUDE: (Harness, &str, &str) = (Harness::ClaudeCode, CLAUDE_READY, "claude");

#[tokio::test]
async fn respawn_kills_then_ups_in_one_reservation() {
    // The old pane is killed (audited under agent.respawn), the agent is
    // recorded down and requested up again, and the new pane is split,
    // tagged, launched into and waited on exactly as agent.up does. Hold
    // the launch's audit append: both the agent and its old pane must still
    // be reserved while the new start is in progress.
    let (gate, mut entered) = mpsc::channel(1);
    let rig = Rig::gated(Some(("control.spawned", gate))).await;
    let ready = tokio::spawn(Starting::ready_on(rig, CODEX));
    tokio::time::timeout(std::time::Duration::from_secs(30), entered.recv())
        .await.unwrap().unwrap().send(()).unwrap();
    let mut starting = ready.await.unwrap();
    let mut committed = starting.rig.org.committed.subscribe();
    starting.script_replace();
    starting.script_launch_on_new();
    starting.frame(Starting::moved(Starting::screen(CODEX_FRESH, "codex"), "%2"));
    for _ in 0..2 {
        starting.inspect("%2", Some("q/lead"));
    }
    let call = starting.respawning(None).await;
    let release = tokio::time::timeout(std::time::Duration::from_secs(30), entered.recv())
        .await.unwrap().unwrap();
    for (agent, held_pane) in [("q/lead", None), ("q/other", Some("%1"))] {
        let request = crate::control::reservation::ReservationRequest::new(
            agent.parse().unwrap(),
            held_pane.map(pane),
            ReservationKind::Observe,
            crate::control::reservation::WaitPolicy::Refuse,
            tokio::time::Instant::now() + std::time::Duration::from_secs(30),
        );
        let observed = starting.rig.control.run_reserved(
            request,
            |_| async { Ok::<_, AgentError>(()) },
            cleanup,
        ).await;
        assert!(matches!(observed, Err(AgentError::Reservation(
            crate::control::reservation::ReservationError::Busy { .. }
        ))), "{agent} {held_pane:?}: {observed:?}");
    }
    release.send(()).unwrap();
    let result = call.await.unwrap().unwrap();
    assert_eq!(result.agent_id, started());
    assert_eq!(result.pane, "%2");
    assert_eq!(starting.effects(), starting.respawned());
    let agent = starting.agent().await.unwrap();
    assert_eq!(agent.status, AgentStatus::Ready);
    assert_eq!(agent.pane.as_deref(), Some("%2"));
    let mut log = Vec::new();
    while let Ok(event) = committed.try_recv() {
        log.push(event);
    }
    assert_eq!(
        log.iter().map(|event| event.kind.as_str()).collect::<Vec<_>>(),
        [
            "control.killed",
            "agent.down",
            "agent.up_requested",
            "agent.pane_created",
            "control.spawned",
            "agent.editor_ready",
        ]
    );
    assert_eq!(
        log.first().map(|event| &event.body["action"]),
        Some(&serde_json::json!({"method": "agent.respawn", "agent": "q/lead"}))
    );
    assert_eq!(
        log.get(1).map(|event| &event.body["reason"]),
        Some(&serde_json::json!("taken down; %1 killed"))
    );
    // The old pane's baseline went with it; the new pane has its own.
    assert_eq!(
        starting.agents.baseline_of(&started(), "%1"),
        PaneBaseline::unproven()
    );
    assert_eq!(
        starting.agents.baseline_of(&started(), "%2"),
        PaneBaseline::shell(100, "zsh".into())
            .with_harness("codex".into())
            .unwrap()
    );
    starting.settled().await;
    assert_eq!(starting.unread_all(), 0);
}

#[tokio::test]
async fn respawn_waiter_after_kill_sees_ready_on_new_pane() {
    // Hold the Down append after the kill and admit a waiting reservation of
    // the agent. It must wait for the entire respawn, not enter between the
    // kill and the new start and see an agent that is temporarily Down.
    let (gate, mut entered) = mpsc::channel(1);
    let rig = Rig::gated(Some(("agent.down", gate))).await;
    let mut starting = Starting::ready_with(rig, CODEX).await;
    starting.script_replace();
    starting.script_launch_on_new();
    starting.frame(Starting::moved(Starting::screen(CODEX_FRESH, "codex"), "%2"));
    for _ in 0..2 {
        starting.inspect("%2", Some("q/lead"));
    }
    let call = starting.respawning(None).await;
    let release = tokio::time::timeout(std::time::Duration::from_secs(30), entered.recv())
        .await.unwrap().unwrap();
    let org = starting.rig.org.clone();
    let request = crate::control::reservation::ReservationRequest::new(
        started(),
        None,
        ReservationKind::Up,
        crate::control::reservation::WaitPolicy::Wait,
        tokio::time::Instant::now() + std::time::Duration::from_secs(50),
    );
    let waiter = starting.rig.control.run_reserved(
        request,
        move |_| async move {
            let state = org.snapshot().await.unwrap();
            Ok::<_, AgentError>(state.agent(&started()).map(|agent| (agent.status, agent.pane.clone())))
        },
        cleanup,
    );
    tokio::pin!(waiter);
    tokio::select! {
        biased;
        result = &mut waiter => panic!("the waiter cannot have run yet: {result:?}"),
        _ = std::future::ready(()) => {}
    }
    // FIFO on the control actor: this later independent request answers
    // only after the waiter's admission has been handled.
    let barrier = crate::control::reservation::ReservationRequest::new(
        "q/unrelated".parse().unwrap(),
        None,
        ReservationKind::Observe,
        crate::control::reservation::WaitPolicy::Refuse,
        tokio::time::Instant::now() + std::time::Duration::from_secs(30),
    );
    starting.rig.control.run_reserved(
        barrier,
        |_| async { Ok::<_, AgentError>(()) },
        cleanup,
    ).await.unwrap();
    release.send(()).unwrap();
    let result = call.await.unwrap().unwrap();
    assert_eq!(result.pane, "%2");
    assert_eq!(waiter.await.unwrap(), Some((AgentStatus::Ready, Some("%2".into()))));
}

#[tokio::test]
async fn respawn_launch_and_dialog_audits_name_respawn() {
    // A launch that skips Codex's update offer audits both the launch and
    // its dialog key under the respawn, just as it audits the old pane's kill.
    let mut starting = Starting::ready(CODEX).await;
    let mut committed = starting.rig.org.committed.subscribe();
    starting.script_replace();
    starting.script_launch_on_new();
    starting.answered(Starting::moved(Starting::screen(CODEX_UPDATE, "codex"), "%2"));
    starting.frame(Starting::moved(Starting::screen(CODEX_FRESH, "codex"), "%2"));
    for _ in 0..2 {
        starting.inspect("%2", Some("q/lead"));
    }
    starting.respawn().await.unwrap();
    let mut audits = Vec::new();
    while let Ok(event) = committed.try_recv() {
        if event.kind.starts_with("control.") {
            audits.push((event.kind.clone(), event.body["action"]["method"].clone()));
        }
    }
    assert_eq!(audits, [
        ("control.killed".into(), serde_json::json!("agent.respawn")),
        ("control.spawned".into(), serde_json::json!("agent.respawn")),
        ("control.sent".into(), serde_json::json!("agent.respawn")),
    ]);
    let mut effects = starting.respawned();
    effects.push("keys %2 Escape".into());
    assert_eq!(starting.effects(), effects);
    assert_eq!(starting.agent().await.unwrap().status, AgentStatus::Ready);
    starting.settled().await;
    assert_eq!(starting.unread_all(), 0);
}

#[tokio::test]
async fn respawn_failed_new_start_is_stuck_on_new_pane_with_owner_note() {
    // The old pane was killed, but somebody selected the new pane before
    // its prompt was read: nothing is typed and cleanup accounts for the
    // unfinished start on that new pane, with one note to the owner.
    let mut starting = Starting::ready(CODEX).await;
    starting.script_replace();
    starting.frame(Starting::moved(
        Starting::shell(PROMPT).with_focus(Some(true)),
        "%2",
    ));
    let result = starting.respawn().await;
    assert!(matches!(result, Err(AgentError::Refused(_))), "{result:?}");
    let agent = starting.agent().await.unwrap();
    assert_eq!(agent.status, AgentStatus::Stuck);
    assert_eq!(agent.pane.as_deref(), Some("%2"));
    assert_eq!(agent.session_id, None);
    let reason = agent.reason.as_deref().unwrap();
    assert!(reason.starts_with("start did not complete: the new pane was selected"));
    assert!(reason.ends_with("; inspect %2"));
    let state = starting.rig.org.snapshot().await.unwrap();
    let notes: Vec<_> = state.messages()
        .filter(|message| message.content.to == atm_org::Address::Owner)
        .collect();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].content.body, format!("agent q/lead: {reason}"));
    assert_eq!(starting.effects(), [
        "kill %1",
        "split %0 Below detached=true command=None",
        "set-option -p -t %2 @atm-agent q/lead",
    ]);
    assert_eq!(starting.unread_all(), 0);
}

#[tokio::test]
async fn respawn_only_old_pane_refuses_before_kill() {
    // Run from the agent's own pane, the pane to split is the pane the
    // respawn kills: refused with nothing changed. Twin: the same call from
    // outside tmux, respawn_kills_then_ups_in_one_reservation.
    let mut starting = Starting::ready(CODEX).await;
    let before = starting.rig.org.status().unwrap();
    starting.inspect("%1", Some("q/lead"));
    let result = starting.respawning(Some("%1")).await.await.unwrap();
    assert!(
        matches!(&result, Err(AgentError::Refused(reason))
            if reason.contains("is the agent's own pane") && reason.ends_with("nothing was changed")),
        "{result:?}"
    );
    assert_eq!(starting.effects(), Vec::<String>::new());
    assert_eq!(starting.rig.org.status().unwrap(), before);
    let agent = starting.agent().await.unwrap();
    assert_eq!(agent.status, AgentStatus::Ready);
    assert_eq!(agent.pane.as_deref(), Some("%1"));
    assert_eq!(starting.unread_all(), 0);
}

/// As [`Starting::ready`] for Codex, with the role placed to the right of
/// agent `q/boss`, which is recorded Ready on `%7` before the service starts
/// (the setup of `up_refuses_a_placement_it_cannot_prove`).
async fn ready_beside_boss() -> Starting {
    let rig = Rig::new().await;
    let project = tempfile::tempdir().unwrap();
    std::fs::create_dir(project.path().join("q")).unwrap();
    let root = project.path().join("q").canonicalize().unwrap();
    let boss: AgentId = "q/boss".parse().unwrap();
    for command in [
        OrgCommand::RegisterProject {
            project_id: "q".parse().unwrap(),
            root: root.to_str().unwrap().into(),
            move_root: false,
        },
        OrgCommand::Up {
            agent_id: boss.clone(),
            role: atm_org::parse_role("boss", "+++\nharness='codex'\n+++\n").unwrap(),
        },
        OrgCommand::PaneCreated {
            agent_id: boss.clone(),
            pane: "%7".into(),
        },
        OrgCommand::EditorReady {
            agent_id: boss.clone(),
        },
    ] {
        rig.command(command).await;
    }
    rig.inspect_only("%7", "q/boss");
    let mut starting = Starting::on(rig, project, Harness::Codex, "pane='right:boss'\n").await;
    starting.inspect("%7", Some("q/boss"));
    starting.rig.client.mock().set_next_pane_id("%1");
    starting
        .rig
        .client
        .script_tag_check(&Starting::inspection("%1", None, "zsh"))
        .unwrap();
    starting.script_paste();
    starting.frame(starting.staged());
    starting.frame(starting.staged());
    starting.frame(Starting::screen(CODEX_FRESH, "codex"));
    starting.follow_up();
    starting.follow_up();
    starting.up().await.unwrap();
    starting.settled().await;
    assert_eq!(starting.unread(), 0);
    assert_eq!(starting.rig.client.pending_reads(&pane("%7")).unwrap(), 0);
    starting.rig.client.mock().clear_calls();
    starting
}

#[tokio::test]
async fn respawn_placement_tag_reused_refuses() {
    // The role places the agent beside agent q/boss. By the respawn's own
    // inspection the pane recorded for q/boss carries another tag (its pane
    // id was reused): refused before the kill, nothing changed. And the same
    // seen only at the second inspection, after the kill: the agent is down,
    // and nothing is split or typed.
    for after_kill in [false, true] {
        let mut starting = ready_beside_boss().await;
        let before = starting.rig.org.status().unwrap();
        if after_kill {
            starting.inspect("%7", Some("q/boss"));
            starting.inspect("%1", Some("q/lead"));
            starting
                .rig
                .client
                .script_tag_check(&Starting::inspection("%1", Some("q/lead"), "zsh"))
                .unwrap();
            starting.rig.client.script_inspect(&pane("%1"), None).unwrap();
        }
        starting.inspect("%7", Some("q/other"));
        let result = starting.respawn().await;
        let agent = starting.agent().await.unwrap();
        if after_kill {
            assert!(
                matches!(&result, Err(AgentError::Refused(reason))
                    if reason.contains("was taken down and not started again: %7 is no longer agent q/boss's pane")),
                "{result:?}"
            );
            assert_eq!(starting.effects(), ["kill %1"]);
            assert_eq!(agent.status, AgentStatus::Down);
        } else {
            assert!(
                matches!(&result, Err(AgentError::Refused(reason))
                    if reason.contains("%7 is no longer agent q/boss's pane")
                        && reason.ends_with("nothing was changed")),
                "{result:?}"
            );
            assert_eq!(starting.effects(), Vec::<String>::new());
            assert_eq!(starting.rig.org.status().unwrap(), before);
            assert_eq!(agent.status, AgentStatus::Ready);
            assert_eq!(agent.pane.as_deref(), Some("%1"));
        }
        assert_eq!(
            starting.unread_all() + starting.rig.client.pending_reads(&pane("%7")).unwrap(),
            0
        );
    }
}

#[tokio::test]
async fn respawn_of_a_pane_that_is_not_the_agents_any_more_refuses() {
    // The agent's recorded pane carries another tag: it is not killed, and
    // nothing is changed. Twin: respawn_kills_then_ups_in_one_reservation.
    let mut starting = Starting::ready(CODEX).await;
    let before = starting.rig.org.status().unwrap();
    starting.inspect("%0", None);
    starting.inspect("%1", Some("q/other"));
    let result = starting.respawn().await;
    assert!(
        matches!(&result, Err(AgentError::Refused(reason))
            if reason.contains("%1 is no longer tagged for agent q/lead")),
        "{result:?}"
    );
    assert_eq!(starting.effects(), Vec::<String>::new());
    assert_eq!(starting.rig.org.status().unwrap(), before);
    assert_eq!(starting.unread_all(), 0);
    assert!(!quarantined(&starting.rig, "q/lead").await);
}

#[tokio::test]
async fn respawn_kill_done_and_down_not_recorded_quarantines() {
    // The old pane is killed, and the Down that says so can't be written.
    // The agent is still recorded Ready on a pane that is gone: the record
    // and the pane disagree, so the cleanup fails and the reservation is
    // quarantined, not released. Nothing is split or typed, and the agent is
    // not marked Stuck: the start's own cleanup is not reached, and its
    // owner gets no note. Twins: respawn_kills_then_ups_in_one_reservation
    // (the Down is recorded), and
    // respawn_of_a_pane_that_is_not_the_agents_any_more_refuses (the same
    // reads with another tag: nothing killed, nothing quarantined).
    let rig = Rig::gated_with_failure(None, Some("agent.down")).await;
    let mut starting = Starting::ready_on(rig, CODEX).await;
    let mut committed = starting.rig.org.committed.subscribe();
    starting.inspect("%0", None);
    starting.inspect("%1", Some("q/lead"));
    starting
        .rig
        .client
        .script_tag_check(&Starting::inspection("%1", Some("q/lead"), "zsh"))
        .unwrap();
    starting.rig.client.script_inspect(&pane("%1"), None).unwrap();
    let result = starting.respawn().await;
    assert!(
        matches!(result, Err(AgentError::Organisation(_))),
        "{result:?}"
    );
    assert_eq!(starting.effects(), ["kill %1"]);
    assert_eq!(committed_kinds(&mut committed), ["control.killed"]);
    let agent = starting.agent().await.unwrap();
    assert_eq!(agent.status, AgentStatus::Ready);
    assert_eq!(agent.pane.as_deref(), Some("%1"));
    assert_eq!(agent.reason, None);
    let state = starting.rig.org.snapshot().await.unwrap();
    assert_eq!(state.messages().count(), 0, "nobody is told by a note");
    assert!(quarantined(&starting.rig, "q/lead").await);
    assert_eq!(starting.unread_all(), 0);
}

#[tokio::test]
async fn respawn_refuses_when_recorded_pane_changed_before_acquisition() {
    // The respawn prepared against %1 while a preceding job held the agent;
    // that job leaves it Ready on %2. The stale respawn must not take down
    // or replace the newly recorded pane.
    let mut starting = Starting::ready(CODEX).await;
    let (release, held) = hold_agent_reservation(&starting.rig, "q/lead", "%1").await;
    starting.inspect("%0", None);
    let call = starting.respawning(None).await;
    move_recorded_pane(&starting.rig, started(), "codex").await;
    let before = starting.rig.org.status().unwrap();
    release.send(()).unwrap();
    held.await.unwrap();
    let result = call.await.unwrap();
    assert!(matches!(&result, Err(AgentError::Conflict(reason))
        if reason.contains("changed its pane")), "{result:?}");
    assert!(starting.effects().is_empty());
    assert_eq!(starting.rig.org.status().unwrap(), before);
    let agent = starting.agent().await.unwrap();
    assert_eq!(agent.status, AgentStatus::Ready);
    assert_eq!(agent.pane.as_deref(), Some("%2"));
    assert_eq!(starting.unread_all(), 0);
}

#[tokio::test]
async fn bad_respawn_role_refuses_before_any_read() {
    // The role is read again before anything is reserved: a role file that
    // no longer parses refuses the respawn with no pane read and no record.
    let starting = Starting::ready(CODEX).await;
    let before = starting.rig.org.status().unwrap();
    let root = starting.project.path().join("q");
    std::fs::write(root.join(".atm/roles/lead.md"), "+++\nharness='codex'\ntypo=1\n+++\n").unwrap();
    let agents = starting.agents.clone();
    let params = RespawnParams {
        agent: started(),
        caller_server: None,
    };
    if PAUSED.replace(false) {
        tokio::time::resume();
    }
    let result = agents.respawn(params, caller()).await;
    assert!(matches!(result, Err(AgentError::Refused(_))), "{result:?}");
    assert_eq!(starting.effects(), Vec::<String>::new());
    assert_eq!(starting.rig.org.status().unwrap(), before);
    assert_eq!(starting.agent().await.unwrap().status, AgentStatus::Ready);
}

#[tokio::test]
async fn stale_completion_after_respawn_cannot_bind() {
    // The old process's session is still in the registry on the old pane
    // when the agent is started again, and an event of it arrives during the
    // new start: it is not the new start's session. The agent is Ready only
    // on the session the new pane announces.
    let mut starting = Starting::ready(CLAUDE).await;
    assert_eq!(
        starting.agent().await.unwrap().session_id,
        Some(SessionId::new("s1"))
    );
    starting.script_replace();
    starting.script_launch_on_new();
    let editor = || Starting::moved(Starting::screen(CLAUDE_READY, "claude"), "%2");
    starting.frame(editor());
    let call = starting.respawning(None).await;
    progress(&mut starting.events, "up_awaiting_session", Some("q/lead")).await;
    assert_eq!(starting.agent().await.unwrap().session_id, None);
    // The old session speaks again, on the old pane: the start looks again
    // and still has no session.
    starting.frame(editor());
    start_session(&starting.rig, Harness::ClaudeCode, "s1", Some("resume")).await;
    progress(&mut starting.events, "up_awaiting_session", Some("q/lead")).await;
    assert_eq!(starting.agent().await.unwrap().status, AgentStatus::Starting);
    // The new process announces its own.
    starting.frame(editor());
    for _ in 0..2 {
        starting.inspect("%2", Some("q/lead"));
    }
    starting
        .rig
        .registry
        .apply_lifecycle_event(
            SessionId::new("s2"),
            LifecycleEvent::SessionStart {
                source: Some("startup".into()),
            },
            Harness::ClaudeCode,
            Some(u32::MAX - 2),
            Some("%2".into()),
        )
        .await
        .unwrap();
    let result = call.await.unwrap().unwrap();
    assert_eq!(result.pane, "%2");
    assert_eq!(starting.effects(), starting.respawned());
    starting.settled().await;
    let agent = starting.agent().await.unwrap();
    assert_eq!(agent.status, AgentStatus::Ready);
    assert_eq!(agent.session_id, Some(SessionId::new("s2")));
    assert_eq!(starting.unread_all(), 0);
}
