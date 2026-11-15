// Lifecycle flows over the real control actor and a scripted tmux client.
//
// Included into the reconciliation tests (`agents_tests.rs`) for their rig, so
// this file has no module docs of its own. Every "nothing was changed" test
// names its refusal and has a twin that does change the pane.

use super::*;
use crate::control::reservation::ReservationKind;
use atm_tmux::mock::MockCall;

fn pane(id: &str) -> PaneId {
    PaneId::parse(id).unwrap()
}

fn lead() -> AgentId {
    "p/lead".parse().unwrap()
}

/// The pane mutations the mock saw: kills, and raw commands that set or unset
/// a pane option (the agent tag).
fn mutations(rig: &Rig) -> Vec<String> {
    rig.client
        .mock()
        .calls()
        .into_iter()
        .filter_map(|call| match call {
            MockCall::KillPane { pane } => Some(format!("kill {pane}")),
            MockCall::RawCommand { subcommand, args } if subcommand == "set-option" => {
                Some(format!("set-option {}", args.join(" ")))
            }
            MockCall::SendKeys { pane, keys } => Some(format!("keys {pane} {keys}")),
            MockCall::PasteText { pane, .. } => Some(format!("paste {pane}")),
            _ => None,
        })
        .collect()
}

fn committed_kinds(events: &mut broadcast::Receiver<std::sync::Arc<crate::eventlog::Envelope>>) -> Vec<String> {
    let mut kinds = Vec::new();
    while let Ok(event) = events.try_recv() {
        kinds.push(event.kind.clone());
    }
    kinds
}

fn tagged(rig_pane: &str, tag: Option<&str>) -> PaneInspection {
    let mut inspection = Rig::inspection(rig_pane, "p/lead", false);
    inspection.local_agent_tag = tag.map(Into::into);
    inspection
}

/// A Ready agent on %1 with the service started and its restart check done.
async fn ready_agent() -> (Rig, AgentHandle) {
    let rig = Rig::new().await;
    rig.agent("lead", Some("%1"), None).await;
    rig.inspect_only("%1", "p/lead");
    let (agents, _) = rig.start();
    agents.reconciled().await.unwrap();
    assert_eq!(rig.client.pending_reads(&pane("%1")).unwrap(), 0);
    (rig, agents)
}

async fn down(agents: &AgentHandle, kill: bool) -> Result<DownResult, AgentError> {
    agents
        .down(DownParams {
            agent: lead(),
            kill,
        })
        .await
}

#[tokio::test]
async fn down_kill_true_kills_once_and_records_down() {
    let (rig, agents) = ready_agent().await;
    agents.record_baseline(lead(), "%1", PaneBaseline::adopted(100));
    let mut committed = rig.org.committed.subscribe();
    // The flow's inspection, the kill's own ownership check, then the pane is gone.
    rig.inspect_only("%1", "p/lead");
    rig.client.script_tag_check(&tagged("%1", Some("p/lead"))).unwrap();
    rig.client.script_inspect(&pane("%1"), None).unwrap();
    let result = down(&agents, true).await.unwrap();
    assert_eq!(result.agent_id, lead());
    assert_eq!(mutations(&rig), ["kill %1"]);
    assert_eq!(rig.client.pending_reads(&pane("%1")).unwrap(), 0);
    let agent = rig.state("lead").await;
    assert_eq!(agent.status, AgentStatus::Down);
    assert_eq!(agent.pane, None);
    assert_eq!(agent.reason.as_deref(), Some("taken down; %1 killed"));
    let kinds = committed_kinds(&mut committed);
    assert_eq!(
        kinds.iter().filter(|kind| *kind == "control.killed").count(),
        1,
        "{kinds:?}"
    );
    assert_eq!(kinds.last().map(String::as_str), Some("agent.down"));
    // Its pane baseline went with the pane.
    assert_eq!(agents.baseline_of(&lead(), "%1"), PaneBaseline::unproven());
    // Down again is a no-op: no read, no event.
    let before = rig.org.status().unwrap();
    down(&agents, true).await.unwrap();
    assert_eq!(rig.org.status().unwrap(), before);
    assert_eq!(mutations(&rig), ["kill %1"]);
}

#[tokio::test]
async fn down_kill_false_untags_and_keeps_program() {
    let (rig, agents) = ready_agent().await;
    let mut committed = rig.org.committed.subscribe();
    rig.inspect_only("%1", "p/lead");
    rig.client.script_tag_check(&tagged("%1", Some("p/lead"))).unwrap();
    // After the untag the pane is still there, without a tag.
    rig.client
        .script_inspect(&pane("%1"), Some(&tagged("%1", None)))
        .unwrap();
    down(&agents, false).await.unwrap();
    let mutations = mutations(&rig);
    assert_eq!(mutations.len(), 1, "{mutations:?}");
    assert!(mutations[0].starts_with("set-option"), "{mutations:?}");
    assert!(mutations[0].contains("%1"), "{mutations:?}");
    assert_eq!(rig.client.pending_reads(&pane("%1")).unwrap(), 0);
    let agent = rig.state("lead").await;
    assert_eq!(agent.status, AgentStatus::Down);
    assert_eq!(
        agent.reason.as_deref(),
        Some("taken down; %1 released with its program running")
    );
    let kinds = committed_kinds(&mut committed);
    assert_eq!(kinds, ["agent.down"], "no kill audit: nothing was killed");
}

#[tokio::test]
async fn down_without_pane_reserves_agent() {
    // An agent whose split never happened has no pane. While the observer
    // runs, that state only lasts as long as another job holds the agent (the
    // observer records such an agent Down itself), so that is where a down
    // meets it: it waits for the job, reads no pane, and ends Down.
    let rig = Rig::new().await;
    let (agents, _) = rig.start();
    agents.reconciled().await.unwrap();
    let (entered, entry) = tokio::sync::oneshot::channel();
    let (release, released) = tokio::sync::oneshot::channel();
    let control = rig.control.clone();
    let held = tokio::spawn(async move {
        control
            .run_reserved(
                request("p/lead", None, ReservationKind::Up),
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
    rig.command(OrgCommand::Up {
        agent_id: lead(),
        role: atm_org::parse_role("lead", "+++\nharness='pi'\n+++\n").unwrap(),
    })
    .await;
    assert_eq!(rig.state("lead").await.status, AgentStatus::Starting);
    let call = tokio::spawn({
        let agents = agents.clone();
        async move { down(&agents, true).await }
    });
    // The down is admitted and parked behind the held job: it has not run.
    let mut events = agents.observation_events();
    update_unrelated(&rig, &mut events).await;
    assert!(!call.is_finished());
    assert_eq!(rig.state("lead").await.status, AgentStatus::Starting);
    release.send(()).unwrap();
    held.await.unwrap();
    call.await.unwrap().unwrap();
    assert_eq!(rig.state("lead").await.status, AgentStatus::Down);
    assert!(mutations(&rig).is_empty());
    assert!(rig.client.mock().calls().is_empty(), "no pane, no tmux call");
    assert!(matches!(
        agents
            .down(DownParams {
                agent: "p/nobody".parse().unwrap(),
                kill: false
            })
            .await,
        Err(AgentError::NotFound(_))
    ));
}

// Let the service take a full turn: one unrelated registry event, handled.
async fn update_unrelated(
    rig: &Rig,
    events: &mut broadcast::Receiver<crate::org::agents::ObservationProgress>,
) {
    rig.events
        .send(crate::registry::SessionEvent::Removed {
            session_id: SessionId::new("unrelated"),
            reason: crate::registry::RemovalReason::Explicit,
        })
        .unwrap();
    progress(events, "scheduled", None).await;
}

#[tokio::test]
async fn down_foreign_tag_refuses_without_mutation() {
    // The recorded pane id now belongs to something else: another agent's tag,
    // an empty tag, or no tag at all. Its twins are the two tests above.
    for (kill, found) in [
        (true, Some("p/other")),
        (false, Some("p/other")),
        (true, Some("")),
        (false, None),
    ] {
        let (rig, agents) = ready_agent().await;
        rig.client
            .script_inspect(&pane("%1"), Some(&tagged("%1", found)))
            .unwrap();
        let before = rig.org.status().unwrap();
        match down(&agents, kill).await {
            Err(AgentError::Refused(reason)) => {
                assert!(reason.contains("no longer tagged for agent p/lead"), "{reason}");
                assert!(reason.contains("nothing was changed"), "{reason}");
            }
            other => panic!("{kill} {found:?}: {other:?}"),
        }
        assert!(mutations(&rig).is_empty(), "{kill} {found:?}");
        assert_eq!(rig.client.pending_reads(&pane("%1")).unwrap(), 0);
        assert_eq!(rig.org.status().unwrap(), before);
        assert_eq!(rig.state("lead").await.status, AgentStatus::Ready);
    }
}

#[tokio::test]
async fn down_positively_absent_pane_completes() {
    for kill in [true, false] {
        let (rig, agents) = ready_agent().await;
        rig.client.script_inspect(&pane("%1"), None).unwrap();
        down(&agents, kill).await.unwrap();
        assert!(mutations(&rig).is_empty());
        assert_eq!(rig.client.pending_reads(&pane("%1")).unwrap(), 0);
        let agent = rig.state("lead").await;
        assert_eq!(agent.status, AgentStatus::Down);
        assert_eq!(
            agent.reason.as_deref(),
            Some("taken down; %1 was already gone")
        );
    }
}

#[tokio::test]
async fn down_refuses_when_the_pane_cannot_be_read() {
    // Nothing scripted: tmux does not answer. That is not absence.
    let (rig, agents) = ready_agent().await;
    let before = rig.org.status().unwrap();
    assert!(matches!(
        down(&agents, true).await,
        Err(AgentError::Reservation(_))
    ));
    assert!(mutations(&rig).is_empty());
    assert_eq!(rig.org.status().unwrap(), before);
    assert_eq!(rig.state("lead").await.status, AgentStatus::Ready);
}

// After a down whose pane effect happened and whose record did not: the
// agent's reservation is quarantined, and another agent's is not.
async fn assert_quarantined(rig: &Rig) {
    use crate::control::reservation::ReservationError;
    assert_eq!(mutations(rig).len(), 1, "the untag happened");
    let again = rig
        .control
        .run_reserved(
            request("p/lead", Some("%1"), ReservationKind::Observe),
            |_| async { Ok::<_, AgentError>(()) },
            cleanup,
        )
        .await;
    assert!(
        matches!(
            again,
            Err(AgentError::Reservation(ReservationError::Quarantined { .. }))
        ),
        "{again:?}"
    );
    rig.control
        .run_reserved(
            request("p/other", None, ReservationKind::Observe),
            |_| async { Ok::<_, AgentError>(()) },
            cleanup,
        )
        .await
        .unwrap();
}

fn script_release(rig: &Rig) {
    rig.inspect_only("%1", "p/lead");
    rig.client
        .script_tag_check(&tagged("%1", Some("p/lead")))
        .unwrap();
    rig.client
        .script_inspect(&pane("%1"), Some(&tagged("%1", None)))
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn untag_ok_then_down_append_unknown_quarantines() {
    // The pane was released, and the Down that says so hangs in the
    // organisation's append: whether it is recorded can't be known.
    let (gate, mut entered) = mpsc::channel(1);
    let rig = Rig::gated(Some(("agent.down", gate))).await;
    rig.agent("lead", Some("%1"), None).await;
    rig.inspect_only("%1", "p/lead");
    let (agents, _) = rig.start();
    agents.reconciled().await.unwrap();
    script_release(&rig);
    let call = tokio::spawn({
        let agents = agents.clone();
        async move { down(&agents, false).await }
    });
    let release = entered.recv().await.unwrap();
    let result = call.await.unwrap();
    assert!(result.is_err(), "{result:?}");
    assert_quarantined(&rig).await;
    release.send(()).unwrap();
}

#[tokio::test]
async fn untag_ok_then_down_append_refused_quarantines() {
    // The same, with an append that fails outright. The organisation still
    // answers, and says the agent is Ready on a pane that no longer carries
    // its tag: the record and the pane disagree, so nothing is released.
    let rig = Rig::gated_with_failure(None, Some("agent.down")).await;
    rig.agent("lead", Some("%1"), None).await;
    rig.inspect_only("%1", "p/lead");
    let (agents, _) = rig.start();
    agents.reconciled().await.unwrap();
    script_release(&rig);
    let result = down(&agents, false).await;
    assert!(
        matches!(result, Err(AgentError::Organisation(_))),
        "{result:?}"
    );
    assert_eq!(rig.state("lead").await.status, AgentStatus::Ready);
    assert_quarantined(&rig).await;
}

// --- agent.adopt ---

use super::super::super::agent_flow::ProcFacts;

const PI_RUNNING: &str =
    include_str!("../../../atm-pi-adapter/tests/fixtures/screen_styled_p1_C2-steer-one-held-120x40_1.5.0.txt");
// Captured: codex-cli 0.167.0 (U8g): a finished turn of a Codex that was
// started with a positional prompt, drawn with a compact footer the adapter
// does not read.
const CODEX_COMPACT: &str = include_str!(
    "../../../atm-codex-adapter/tests/fixtures/screen_styled_u8g_finished-100x32_0.167.0.txt"
);

/// The pane's root is pid 100 in every scripted inspection.
fn job_in_front() -> ProcFacts {
    ProcFacts {
        process_group: 100,
        foreground_group: 150,
        system_shell: true,
        children: 1,
        name: "zsh".into(),
    }
}
fn shell_holding_child() -> ProcFacts {
    ProcFacts {
        foreground_group: 100,
        ..job_in_front()
    }
}
fn root_program(name: &str) -> ProcFacts {
    ProcFacts {
        process_group: 100,
        foreground_group: 100,
        system_shell: false,
        children: 0,
        name: name.into(),
    }
}

/// An adopt of %1 as `q/lead`, with its project on disk.
struct Adoption {
    rig: Rig,
    agents: AgentHandle,
    project: tempfile::TempDir,
    events: broadcast::Receiver<crate::org::agents::ObservationProgress>,
}

fn adoptee() -> AgentId {
    "q/lead".parse().unwrap()
}

impl Adoption {
    async fn new() -> Self {
        Self::on(Rig::new().await).await
    }
    async fn on(rig: Rig) -> Self {
        let (agents, _) = rig.start();
        agents.reconciled().await.unwrap();
        let events = agents.observation_events();
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir(project.path().join("q")).unwrap();
        Self {
            rig,
            agents,
            project,
            events,
        }
    }
    fn inspection(tag: Option<&str>, command: &str, text: &str) -> PaneInspection {
        let mut inspection = Rig::inspection("%1", "q/lead", false)
            .with_foreground_process(None, Some(command.into()));
        inspection.local_agent_tag = tag.map(Into::into);
        inspection.pane.height = text.lines().count().max(1) as u16;
        inspection
    }
    /// The first read of an adopt of an untagged pane: its inspection. An
    /// adopt reads and captures before it tags, so the captures
    /// (`script_untagged_capture`) come next and `script_tag` last.
    fn script_untagged(&self) {
        let untagged = Self::inspection(None, "pi", "");
        self.rig
            .client
            .script_inspect(&pane("%1"), Some(&untagged))
            .unwrap();
    }
    /// The tag's own check of the still untagged pane.
    fn script_tag(&self) {
        self.rig
            .client
            .script_tag_check(&Self::inspection(None, "pi", ""))
            .unwrap();
    }
    /// One capture of the tagged pane showing `text` with `command` in front.
    fn script_capture(&self, text: &str, command: &str) {
        self.script_capture_tagged(Some("q/lead"), text, command);
    }
    /// The same before the adopt's tag: the pane carries none yet.
    fn script_untagged_capture(&self, text: &str, command: &str) {
        self.script_capture_tagged(None, text, command);
    }
    fn script_capture_tagged(&self, tag: Option<&str>, text: &str, command: &str) {
        let text = body(text);
        self.rig
            .client
            .script_capture(&PaneCapture::new(
                text.into(),
                Self::inspection(tag, command, text),
                CaptureFormat::Ansi,
            ))
            .unwrap();
    }
    async fn adopt(&self) -> Result<AdoptResult, AgentError> {
        self.agents
            .adopt(AdoptParams {
                project_root: self.project.path().join("q"),
                name: "lead".into(),
                pane: "%1".into(),
                caller_server: None,
            })
            .await
    }
    /// Script the observation the adoption's commit starts; its reads follow
    /// the adopt's own. A Starting agent's reads its screen (and uses none of
    /// it); a Ready one's only inspects.
    fn script_follow_up(&self, starting: Option<(&str, &str)>) {
        let tagged = Self::inspection(Some("q/lead"), "pi", "");
        self.rig
            .client
            .script_inspect(&pane("%1"), Some(&tagged))
            .unwrap();
        if let Some((text, command)) = starting {
            self.script_capture(text, command);
        }
    }
    /// Wait for that observation: it succeeded and every scripted read was used.
    async fn followed(&mut self) {
        progress(&mut self.events, "finished", Some("q/lead")).await;
        assert_eq!(self.rig.client.pending_reads(&pane("%1")).unwrap(), 0);
        assert_eq!(
            self.agents.list(ListParams::default()).await.unwrap()[0].reason,
            None
        );
    }
    async fn agent(&self) -> Option<atm_org::Agent> {
        self.rig
            .org
            .snapshot()
            .await
            .unwrap()
            .agent(&adoptee())
            .cloned()
    }
    fn tags_set(&self) -> usize {
        mutations(&self.rig)
            .iter()
            .filter(|call| call.starts_with("set-option"))
            .count()
    }
    /// Nothing but the tag was ever done to a pane.
    fn only_tags(&self) -> bool {
        mutations(&self.rig)
            .iter()
            .all(|call| call.starts_with("set-option"))
    }
}

#[tokio::test]
async fn adopt_tags_absent_pane_and_binds_one_eligible_row() {
    // Also adopt_job_in_front_with_live_row_and_harness_screen_pins and
    // adopt_sends_no_keys: the whole happy path of a pi under a shell.
    let mut adoption = Adoption::new().await;
    adoption.rig.row("s1", "%1", false).await;
    *adoption.rig.proc.lock().unwrap() = Some(job_in_front());
    adoption.script_untagged();
    adoption.script_untagged_capture(PI_PADDED, "pi");
    adoption.script_untagged_capture(PI_PADDED, "pi");
    adoption.script_tag();
    adoption.script_follow_up(None);
    let result = adoption.adopt().await.unwrap();
    assert_eq!(result.agent_id, adoptee());
    assert_eq!(result.state, AgentStatus::Ready);
    assert_eq!(adoption.tags_set(), 1);
    assert!(adoption.only_tags(), "{:?}", mutations(&adoption.rig));
    let agent = adoption.agent().await.unwrap();
    assert_eq!(agent.status, AgentStatus::Ready);
    assert_eq!(agent.pane.as_deref(), Some("%1"));
    assert_eq!(agent.session_id, Some(SessionId::new("s1")));
    assert_eq!(
        adoption.agents.baseline_of(&adoptee(), "%1"),
        PaneBaseline::adopted(100).with_harness("pi".into()).unwrap()
    );
    adoption.followed().await;
}

#[tokio::test]
async fn adopt_same_tag_is_idempotent() {
    let mut adoption = Adoption::new().await;
    adoption.rig.row("s1", "%1", false).await;
    *adoption.rig.proc.lock().unwrap() = Some(job_in_front());
    let script_tagged = |adoption: &Adoption, text: &str| {
        adoption
            .rig
            .client
            .script_inspect(
                &pane("%1"),
                Some(&Adoption::inspection(Some("q/lead"), "pi", "")),
            )
            .unwrap();
        adoption.script_capture(text, "pi");
        adoption.script_capture(text, "pi");
    };
    // The pane already carries this agent's tag: no tag is set again.
    script_tagged(&adoption, PI_PADDED);
    adoption.script_follow_up(None);
    assert_eq!(adoption.adopt().await.unwrap().state, AgentStatus::Ready);
    assert_eq!(adoption.tags_set(), 0);
    adoption.followed().await;
    // Adopting the adopted agent again writes nothing, on an idle screen and
    // on a busy one: its recorded binding is kept, not dropped.
    let before = adoption.rig.org.status().unwrap();
    for text in [PI_PADDED, PI_RUNNING] {
        script_tagged(&adoption, text);
        assert_eq!(adoption.adopt().await.unwrap().state, AgentStatus::Ready);
        assert_eq!(adoption.rig.client.pending_reads(&pane("%1")).unwrap(), 0);
        assert_eq!(adoption.rig.org.status().unwrap(), before);
        assert_eq!(
            adoption.agent().await.unwrap().session_id,
            Some(SessionId::new("s1"))
        );
    }
    assert_eq!(adoption.tags_set(), 0);
}

#[tokio::test]
async fn adopt_foreign_or_empty_tag_refuses() {
    // Twin: adopt_tags_absent_pane_and_binds_one_eligible_row.
    for (found, named) in [("p/other", "p/other"), ("", "nobody (an empty tag)")] {
        let adoption = Adoption::new().await;
        adoption.rig.row("s1", "%1", false).await;
        adoption
            .rig
            .client
            .script_inspect(&pane("%1"), Some(&Adoption::inspection(Some(found), "pi", "")))
            .unwrap();
        match adoption.adopt().await {
            Err(AgentError::Refused(reason)) => {
                assert!(reason.contains(named), "{reason}");
                assert!(reason.contains("nothing was changed"), "{reason}");
            }
            other => panic!("{found:?}: {other:?}"),
        }
        assert!(mutations(&adoption.rig).is_empty());
        assert_eq!(adoption.rig.client.pending_reads(&pane("%1")).unwrap(), 0);
        assert!(adoption.agent().await.is_none());
    }
}

#[tokio::test]
async fn adopt_ambiguous_rows_refuse() {
    let adoption = Adoption::new().await;
    adoption.rig.row("s1", "%1", false).await;
    adoption.rig.row("s2", "%1", false).await;
    adoption
        .rig
        .client
        .script_inspect(&pane("%1"), Some(&Adoption::inspection(None, "pi", "")))
        .unwrap();
    match adoption.adopt().await {
        Err(AgentError::Refused(reason)) => {
            assert!(reason.contains("more than one session"), "{reason}");
        }
        other => panic!("{other:?}"),
    }
    assert!(mutations(&adoption.rig).is_empty(), "refused before the tag");
    assert_eq!(adoption.rig.client.pending_reads(&pane("%1")).unwrap(), 0);
    assert!(adoption.agent().await.is_none());
}

#[tokio::test]
async fn adopt_refuses_panes_it_cannot_take() {
    // A pane that does not exist, and one whose program has exited.
    let adoption = Adoption::new().await;
    adoption.rig.client.script_inspect(&pane("%1"), None).unwrap();
    match adoption.adopt().await {
        Err(AgentError::Refused(reason)) => assert!(reason.contains("does not exist"), "{reason}"),
        other => panic!("{other:?}"),
    }
    let mut dead = Adoption::inspection(None, "pi", "");
    dead.dead = true;
    adoption
        .rig
        .client
        .script_inspect(&pane("%1"), Some(&dead))
        .unwrap();
    match adoption.adopt().await {
        Err(AgentError::Refused(reason)) => assert!(reason.contains("has exited"), "{reason}"),
        other => panic!("{other:?}"),
    }
    assert!(mutations(&adoption.rig).is_empty());
    assert!(adoption.agent().await.is_none());
}

#[tokio::test]
async fn adopt_refuses_a_pane_another_agent_holds() {
    let rig = Rig::new().await;
    rig.agent("holder", Some("%1"), None).await;
    rig.inspect_only("%1", "p/holder");
    let (agents, _) = rig.start();
    agents.reconciled().await.unwrap();
    let project = tempfile::tempdir().unwrap();
    std::fs::create_dir(project.path().join("q")).unwrap();
    let result = agents
        .adopt(AdoptParams {
            project_root: project.path().join("q"),
            name: "lead".into(),
            pane: "%1".into(),
            caller_server: None,
        })
        .await;
    match result {
        Err(AgentError::Refused(reason)) => {
            assert!(
                reason.contains("already belongs to agent p/holder"),
                "{reason}"
            );
        }
        other => panic!("{other:?}"),
    }
    assert!(mutations(&rig).is_empty());
    assert!(rig.client.mock().calls().iter().all(|call| !matches!(
        call,
        MockCall::SendKeys { .. } | MockCall::PasteText { .. } | MockCall::KillPane { .. }
    )));
}

#[tokio::test]
async fn adopt_busy_or_unknown_screen_stays_starting() {
    // A running turn: the harness is recognised (and pinned), but not idle.
    let mut adoption = Adoption::new().await;
    adoption.rig.row("s1", "%1", false).await;
    *adoption.rig.proc.lock().unwrap() = Some(job_in_front());
    adoption.script_untagged();
    adoption.script_untagged_capture(PI_RUNNING, "pi");
    adoption.script_untagged_capture(PI_RUNNING, "pi");
    adoption.script_tag();
    adoption.script_follow_up(Some((PI_RUNNING, "pi")));
    assert_eq!(adoption.adopt().await.unwrap().state, AgentStatus::Starting);
    let agent = adoption.agent().await.unwrap();
    assert_eq!(agent.status, AgentStatus::Starting);
    assert_eq!(agent.session_id, None, "no session is bound on a busy screen");
    assert_eq!(
        adoption.agents.baseline_of(&adoptee(), "%1"),
        PaneBaseline::adopted(100).with_harness("pi".into()).unwrap()
    );
    assert!(adoption.only_tags());
    adoption.followed().await;
    // The harness's next event binds it, as after a daemon restart.
    adoption
        .rig
        .client
        .script_inspect(
            &pane("%1"),
            Some(&Adoption::inspection(Some("q/lead"), "pi", "")),
        )
        .unwrap();
    update(&adoption.rig, "s1").await;
    progress(&mut adoption.events, "finished", Some("q/lead")).await;
    let agent = adoption.agent().await.unwrap();
    assert_eq!(agent.status, AgentStatus::Ready);
    assert_eq!(agent.session_id, Some(SessionId::new("s1")));

    // A screen the adapter does not know: nothing is pinned either.
    let mut adoption = Adoption::new().await;
    adoption.rig.row("s1", "%1", false).await;
    *adoption.rig.proc.lock().unwrap() = Some(job_in_front());
    adoption.script_untagged();
    adoption.script_untagged_capture("unknown\n", "pi");
    adoption.script_untagged_capture("unknown\n", "pi");
    adoption.script_tag();
    adoption.script_follow_up(Some(("unknown\n", "pi")));
    assert_eq!(adoption.adopt().await.unwrap().state, AgentStatus::Starting);
    assert_eq!(
        adoption.agents.baseline_of(&adoptee(), "%1"),
        PaneBaseline::unproven()
    );
    adoption.followed().await;
}

#[tokio::test]
async fn adopt_records_a_baseline_only_for_a_harness_it_can_place() {
    // (the procfs answer, tmux's command, pinned?, why). Without process
    // facts the second capture is never taken.
    let cases: [(Option<ProcFacts>, &str, bool, &str); 6] = [
        (Some(job_in_front()), "pi", true, "adopt_job_in_front"),
        (Some(root_program("pi")), "pi", true, "adopt_root_program_pins"),
        (
            Some(shell_holding_child()),
            "zsh",
            false,
            "adopt_shell_holding_child_gets_no_baseline_and_stays_starting",
        ),
        (
            Some(job_in_front()),
            "zsh",
            false,
            "adopt_shell_named_job_gets_no_baseline",
        ),
        (None, "pi", false, "adopt_unreadable_procfs_gets_no_baseline"),
        (
            Some(ProcFacts {
                children: 0,
                ..shell_holding_child()
            }),
            "zsh",
            false,
            "an idle shell under a stale harness frame",
        ),
    ];
    for (facts, command, pinned, case) in cases {
        let mut adoption = Adoption::new().await;
        adoption.rig.row("s1", "%1", false).await;
        let readable = facts.is_some();
        *adoption.rig.proc.lock().unwrap() = facts;
        adoption.script_untagged();
        // The same safe idle frame every time: only the process facts differ.
        adoption.script_untagged_capture(PI_PADDED, command);
        if readable {
            adoption.script_untagged_capture(PI_PADDED, command);
        }
        adoption.script_tag();
        adoption.script_follow_up((!pinned).then_some((PI_PADDED, command)));
        let result = adoption.adopt().await.unwrap();
        assert_eq!(
            result.state,
            if pinned { AgentStatus::Ready } else { AgentStatus::Starting },
            "{case}"
        );
        assert_eq!(
            adoption.agents.baseline_of(&adoptee(), "%1"),
            if pinned {
                PaneBaseline::adopted(100).with_harness(command.into()).unwrap()
            } else {
                PaneBaseline::unproven()
            },
            "{case}"
        );
        assert!(adoption.only_tags(), "{case}");
        adoption.followed().await;
    }
}

#[tokio::test]
async fn adopt_command_changed_between_captures_gets_no_baseline() {
    let mut adoption = Adoption::new().await;
    adoption.rig.row("s1", "%1", false).await;
    *adoption.rig.proc.lock().unwrap() = Some(job_in_front());
    adoption.script_untagged();
    // The process facts were read between a `node` and a `pi`: they describe neither.
    adoption.script_untagged_capture(PI_PADDED, "node");
    adoption.script_untagged_capture(PI_PADDED, "pi");
    adoption.script_tag();
    adoption.script_follow_up(Some((PI_PADDED, "pi")));
    assert_eq!(adoption.adopt().await.unwrap().state, AgentStatus::Starting);
    assert_eq!(
        adoption.agents.baseline_of(&adoptee(), "%1"),
        PaneBaseline::unproven()
    );
    adoption.followed().await;
}

#[tokio::test]
async fn adopt_compact_codex_footer_stays_starting() {
    // A Codex started with a positional prompt: its real frame reads Unknown,
    // so nothing is guessed. Twin: the fresh Codex editor below.
    let mut adoption = Adoption::new().await;
    adoption
        .rig
        .row_of("pending-100", "%1", false, Harness::Codex)
        .await;
    *adoption.rig.proc.lock().unwrap() = Some(job_in_front());
    adoption.script_untagged();
    adoption.script_untagged_capture(CODEX_COMPACT, "codex");
    adoption.script_untagged_capture(CODEX_COMPACT, "codex");
    adoption.script_tag();
    adoption.script_follow_up(Some((CODEX_COMPACT, "codex")));
    assert_eq!(adoption.adopt().await.unwrap().state, AgentStatus::Starting);
    assert_eq!(
        adoption.agents.baseline_of(&adoptee(), "%1"),
        PaneBaseline::unproven()
    );
    adoption.followed().await;

    // A Codex that has had no prompt yet has only its discovery row. Its fresh
    // editor is Ready without a session, which arrives with the first prompt.
    let mut adoption = Adoption::new().await;
    adoption
        .rig
        .row_of("pending-100", "%1", false, Harness::Codex)
        .await;
    *adoption.rig.proc.lock().unwrap() = Some(job_in_front());
    adoption.script_untagged();
    adoption.script_untagged_capture(CODEX_PADDED, "codex");
    adoption.script_untagged_capture(CODEX_PADDED, "codex");
    adoption.script_tag();
    adoption.script_follow_up(None);
    assert_eq!(adoption.adopt().await.unwrap().state, AgentStatus::Ready);
    let agent = adoption.agent().await.unwrap();
    assert_eq!(agent.status, AgentStatus::Ready);
    assert_eq!(agent.session_id, None);
    adoption.followed().await;
}

#[tokio::test]
async fn adopt_without_any_session_row_tags_and_stays_starting() {
    // Nothing tells which harness this is: no screen is read at all.
    let mut adoption = Adoption::new().await;
    *adoption.rig.proc.lock().unwrap() = Some(job_in_front());
    adoption.script_untagged();
    adoption.script_tag();
    // The observer that follows knows no harness either, and asks for a plain capture.
    let tagged = Adoption::inspection(Some("q/lead"), "zsh", "unknown\n");
    adoption
        .rig
        .client
        .script_inspect(&pane("%1"), Some(&tagged))
        .unwrap();
    adoption
        .rig
        .client
        .script_capture(&PaneCapture::new(
            "unknown\n".into(),
            tagged,
            CaptureFormat::Plain,
        ))
        .unwrap();
    assert_eq!(adoption.adopt().await.unwrap().state, AgentStatus::Starting);
    assert_eq!(adoption.tags_set(), 1);
    let agent = adoption.agent().await.unwrap();
    assert_eq!(agent.pane.as_deref(), Some("%1"));
    assert_eq!(agent.session_id, None);
    adoption.followed().await;
}

// Captured: codex-cli 0.167.0 (P1): a turn running.
const CODEX_WORKING: &str = include_str!(
    "../../../atm-codex-adapter/tests/fixtures/screen_styled_p1_c07-working-held-100x24_0.167.0.txt"
);

/// Script the reads of an adopt of the pane the agent is already tagged on.
fn script_readopt(adoption: &Adoption, text: &str, command: &str) {
    adoption
        .rig
        .client
        .script_inspect(
            &pane("%1"),
            Some(&Adoption::inspection(Some("q/lead"), command, "")),
        )
        .unwrap();
    adoption.script_capture(text, command);
    adoption.script_capture(text, command);
}

#[tokio::test]
async fn readopt_keeps_a_binding_whose_row_is_only_a_discovery_row() {
    // Review 19, probe r02. A Ready agent bound to s1; its registry row is
    // only the discovery row, as every row is after a daemon restart until
    // the harness's next event. Adopting it again on its idle pane must not
    // drop the binding, and writes nothing.
    let mut adoption = Adoption::new().await;
    adoption.rig.row("s1", "%1", false).await;
    *adoption.rig.proc.lock().unwrap() = Some(job_in_front());
    adoption.script_untagged();
    adoption.script_untagged_capture(PI_PADDED, "pi");
    adoption.script_untagged_capture(PI_PADDED, "pi");
    adoption.script_tag();
    adoption.script_follow_up(None);
    assert_eq!(adoption.adopt().await.unwrap().state, AgentStatus::Ready);
    adoption.followed().await;
    let tagged = Adoption::inspection(Some("q/lead"), "pi", "");
    // The real row goes, the discovery row comes; each wakes one observation.
    adoption.rig.client.script_inspect(&pane("%1"), Some(&tagged)).unwrap();
    adoption.rig.registry.remove(SessionId::new("s1")).await.unwrap();
    progress(&mut adoption.events, "finished", Some("q/lead")).await;
    adoption.rig.client.script_inspect(&pane("%1"), Some(&tagged)).unwrap();
    adoption
        .rig
        .row_of("pending-100", "%1", false, Harness::Pi)
        .await;
    progress(&mut adoption.events, "finished", Some("q/lead")).await;
    let recorded = adoption.agent().await.unwrap();
    assert_eq!(recorded.status, AgentStatus::Ready);
    assert_eq!(recorded.session_id, Some(SessionId::new("s1")));

    let mut committed = adoption.rig.org.committed.subscribe();
    let before = adoption.rig.org.status().unwrap();
    script_readopt(&adoption, PI_PADDED, "pi");
    assert_eq!(adoption.adopt().await.unwrap().state, AgentStatus::Ready);
    assert_eq!(adoption.rig.client.pending_reads(&pane("%1")).unwrap(), 0);
    assert_eq!(committed_kinds(&mut committed), Vec::<String>::new());
    assert_eq!(adoption.rig.org.status().unwrap(), before);
    assert_eq!(adoption.agent().await.unwrap(), recorded);

    // The same agent suspended with its binding (Starting, as a restart check
    // leaves it on a busy screen): the adopt reports the editor and keeps s1.
    adoption
        .rig
        .command(OrgCommand::Revalidate {
            agent_id: adoptee(),
        })
        .await;
    assert_eq!(adoption.agent().await.unwrap().status, AgentStatus::Starting);
    committed_kinds(&mut committed);
    script_readopt(&adoption, PI_PADDED, "pi");
    assert_eq!(adoption.adopt().await.unwrap().state, AgentStatus::Ready);
    assert_eq!(committed_kinds(&mut committed), ["agent.editor_ready"]);
    let agent = adoption.agent().await.unwrap();
    assert_eq!(agent.status, AgentStatus::Ready);
    assert_eq!(agent.session_id, Some(SessionId::new("s1")));
}

#[tokio::test]
async fn readopt_of_a_ready_agent_without_a_session_writes_nothing() {
    // Review 19, probe r03. A Codex agent is Ready without a session until
    // its first prompt. Adopting it again on the same fresh editor is no
    // change at all.
    let mut adoption = Adoption::new().await;
    adoption
        .rig
        .row_of("pending-100", "%1", false, Harness::Codex)
        .await;
    *adoption.rig.proc.lock().unwrap() = Some(job_in_front());
    adoption.script_untagged();
    adoption.script_untagged_capture(CODEX_PADDED, "codex");
    adoption.script_untagged_capture(CODEX_PADDED, "codex");
    adoption.script_tag();
    adoption.script_follow_up(None);
    assert_eq!(adoption.adopt().await.unwrap().state, AgentStatus::Ready);
    adoption.followed().await;
    let recorded = adoption.agent().await.unwrap();
    assert_eq!(recorded.status, AgentStatus::Ready);
    assert_eq!(recorded.session_id, None);

    let mut committed = adoption.rig.org.committed.subscribe();
    let before = adoption.rig.org.status().unwrap();
    script_readopt(&adoption, CODEX_PADDED, "codex");
    assert_eq!(adoption.adopt().await.unwrap().state, AgentStatus::Ready);
    assert_eq!(adoption.rig.client.pending_reads(&pane("%1")).unwrap(), 0);
    assert_eq!(committed_kinds(&mut committed), Vec::<String>::new());
    assert_eq!(adoption.rig.org.status().unwrap(), before);
    assert_eq!(adoption.agent().await.unwrap(), recorded);
}

#[tokio::test]
async fn readopt_of_a_starting_agent_on_a_safe_screen_only_reports_the_editor() {
    // Adopted on a busy screen: Starting. Adopted again on its idle pane with
    // no session to bind: Ready through EditorReady alone, the observer's
    // revalidation rule. No second Adopt is recorded.
    let mut adoption = Adoption::new().await;
    adoption
        .rig
        .row_of("pending-100", "%1", false, Harness::Codex)
        .await;
    *adoption.rig.proc.lock().unwrap() = Some(job_in_front());
    adoption.script_untagged();
    adoption.script_untagged_capture(CODEX_WORKING, "codex");
    adoption.script_untagged_capture(CODEX_WORKING, "codex");
    adoption.script_tag();
    adoption.script_follow_up(Some((CODEX_WORKING, "codex")));
    assert_eq!(adoption.adopt().await.unwrap().state, AgentStatus::Starting);
    adoption.followed().await;
    assert_eq!(adoption.agent().await.unwrap().status, AgentStatus::Starting);

    let mut committed = adoption.rig.org.committed.subscribe();
    script_readopt(&adoption, CODEX_PADDED, "codex");
    assert_eq!(adoption.adopt().await.unwrap().state, AgentStatus::Ready);
    assert_eq!(adoption.rig.client.pending_reads(&pane("%1")).unwrap(), 0);
    assert_eq!(committed_kinds(&mut committed), ["agent.editor_ready"]);
    let agent = adoption.agent().await.unwrap();
    assert_eq!(agent.status, AgentStatus::Ready);
    assert_eq!(agent.session_id, None);
}

async fn quarantined(rig: &Rig, agent: &str) -> bool {
    use crate::control::reservation::ReservationError;
    matches!(
        rig.control
            .run_reserved(
                request(agent, Some("%1"), ReservationKind::Observe),
                |_| async { Ok::<_, AgentError>(()) },
                cleanup,
            )
            .await,
        Err(AgentError::Reservation(ReservationError::Quarantined { .. }))
    )
}

#[tokio::test]
async fn down_kill_of_a_pane_that_just_went_away_records_down() {
    // Review 19, probe r04. The pane is there at the down's inspection and
    // gone at the kill's own check: positively absent, which is what a down
    // wants. Twin (the pane is killed): down_kill_true_kills_once_and_records_down.
    // The same for a release: there is nothing left to untag.
    for kill in [true, false] {
        let (rig, agents) = ready_agent().await;
        rig.inspect_only("%1", "p/lead");
        rig.client.script_inspect(&pane("%1"), None).unwrap();
        down(&agents, kill).await.unwrap();
        assert!(mutations(&rig).is_empty(), "{kill}");
        assert_eq!(rig.client.pending_reads(&pane("%1")).unwrap(), 0);
        let agent = rig.state("lead").await;
        assert_eq!(agent.status, AgentStatus::Down, "{kill}");
        assert_eq!(agent.pane, None);
        assert_eq!(
            agent.reason.as_deref(),
            Some("taken down; %1 was already gone")
        );
        assert!(!quarantined(&rig, "p/lead").await);
    }
}

#[tokio::test]
async fn down_refused_before_any_effect_does_not_quarantine() {
    // Review 19, probe r04b. The tag changes between the down's inspection
    // and the untag's (or the kill's) own check: refused, nothing changed, so
    // nothing to account for. Twins (the effect happened and the record did
    // not): untag_ok_then_down_append_*_quarantines.
    for kill in [false, true] {
        let (rig, agents) = ready_agent().await;
        rig.inspect_only("%1", "p/lead");
        rig.client
            .script_tag_check(&tagged("%1", Some("p/other")))
            .unwrap();
        let result = down(&agents, kill).await;
        assert!(
            matches!(
                result,
                Err(AgentError::Reservation(
                    crate::control::reservation::ReservationError::TagMismatch { .. }
                ))
            ),
            "{kill}: {result:?}"
        );
        assert!(mutations(&rig).is_empty(), "{kill}");
        assert_eq!(rig.state("lead").await.status, AgentStatus::Ready);
        assert!(!quarantined(&rig, "p/lead").await, "{kill}");
    }
}

#[tokio::test]
async fn adopt_tag_refused_before_any_effect_does_not_quarantine() {
    // Review 19, probe r07b. Another tag appears between the adopt's
    // inspection and its own tag call.
    let adoption = Adoption::new().await;
    adoption.rig.row("s1", "%1", false).await;
    *adoption.rig.proc.lock().unwrap() = Some(job_in_front());
    adoption.script_untagged();
    adoption.script_untagged_capture(PI_PADDED, "pi");
    adoption.script_untagged_capture(PI_PADDED, "pi");
    adoption
        .rig
        .client
        .script_tag_check(&Adoption::inspection(Some("p/other"), "pi", ""))
        .unwrap();
    let result = adoption.adopt().await;
    assert!(
        matches!(
            result,
            Err(AgentError::Reservation(
                crate::control::reservation::ReservationError::TagMismatch { .. }
            ))
        ),
        "{result:?}"
    );
    assert!(mutations(&adoption.rig).is_empty());
    assert!(adoption.agent().await.is_none());
    assert!(!quarantined(&adoption.rig, "q/lead").await);
}

#[test]
fn only_a_before_effect_error_clears_the_effect_flag() {
    use crate::org::agent_lifecycle::{changed, Effect};
    use crate::control::reservation::{ExpectedTag, ReservationError as E};
    use crate::control::ControlError as C;
    use std::sync::atomic::Ordering;
    // What `ReservationError` documents for a general pane operation. No
    // wildcard arm on the outer match: a new variant needs a row here.
    fn before_effect(error: &E) -> bool {
        match error {
            E::NotImplemented { .. }
            | E::Busy { .. }
            | E::StaleReservation
            | E::ScreenChanged { .. }
            | E::PacingRefused { .. }
            | E::PaneInputRefused { .. }
            | E::UnsafePaste { .. }
            | E::TagMismatch { .. }
            | E::PaneGone { .. }
            | E::InvalidObservation { .. }
            | E::Quarantined { .. } => true,
            E::InvalidRequest { .. } | E::Unavailable | E::Timeout => false,
            E::Control(C::SendRefused { .. }) => true,
            E::Control(
                C::NoSession { .. }
                | C::NoPane { .. }
                | C::AmbiguousTarget { .. }
                | C::AgentClearing { .. }
                | C::PaneQuarantined { .. }
                | C::OrganisationUnavailable { .. }
                | C::Tmux { .. },
            ) => false,
        }
    }
    let p = || pane("%1");
    let s = || "%1".to_owned();
    let errors = || {
        vec![
            E::NotImplemented { operation: "x" },
            E::InvalidRequest { reason: "x".into() },
            E::Busy {
                agent: lead(),
                pane: Some(p()),
            },
            E::StaleReservation,
            E::ScreenChanged { pane: p() },
            E::PacingRefused { pane: p() },
            E::PaneInputRefused {
                pane: p(),
                reason: "x",
            },
            E::UnsafePaste { pane: p() },
            E::TagMismatch {
                pane: p(),
                expected: ExpectedTag::Absent,
                actual: None,
            },
            E::PaneGone { pane: p() },
            E::InvalidObservation {
                pane: p(),
                reason: "x".into(),
            },
            E::Quarantined {
                agent: lead(),
                pane: None,
            },
            E::Unavailable,
            E::Timeout,
            E::Control(C::SendRefused {
                pane: s(),
                reason: "x",
            }),
            E::Control(C::NoSession { target: s() }),
            E::Control(C::NoPane { session: s() }),
            E::Control(C::AmbiguousTarget {
                target: s(),
                count: 2,
                ids: s(),
            }),
            E::Control(C::AgentClearing {
                pane: s(),
                agent: lead(),
            }),
            E::Control(C::PaneQuarantined {
                pane: s(),
                agent: lead(),
            }),
            E::Control(C::OrganisationUnavailable { pane: s() }),
            E::Control(C::Tmux {
                context: "x".into(),
                source: atm_tmux::TmuxError::ParseError("x".into()),
            }),
        ]
    };
    assert_eq!(errors().len(), 22, "one of each variant");
    for error in errors() {
        let (label, expected) = (format!("{error:?}"), !before_effect(&error));
        // The flow sets the flag before its call.
        let effect = Effect::default();
        effect.store(true, Ordering::SeqCst);
        assert!(changed(&effect, Err(error)).is_err());
        assert_eq!(effect.load(Ordering::SeqCst), expected, "{label}");
    }
    // A call that succeeded changed the pane.
    let effect = Effect::default();
    effect.store(true, Ordering::SeqCst);
    assert!(changed(&effect, Ok(())).is_ok());
    assert!(effect.load(Ordering::SeqCst));
}

#[tokio::test(start_paused = true)]
async fn pane_effect_whose_call_outlives_the_deadline_quarantines() {
    // Review 22, probe v06. tmux performs the kill (or removes the tag) and
    // its call does not return before the down's deadline: the work is
    // stopped with the effect made and no Down recorded, so the reservation
    // must not be released. Twins (refused before any effect, released):
    // down_refused_before_any_effect_does_not_quarantine.
    use crate::control::reservation::ReservationError;
    for (kill, effect) in [
        (true, "kill %1"),
        (false, "set-option -p -u -t %1 @atm-agent"),
    ] {
        let (rig, agents) = ready_agent().await;
        rig.inspect_only("%1", "p/lead");
        rig.client
            .script_tag_check(&tagged("%1", Some("p/lead")))
            .unwrap();
        rig.client.stall_pane_effects(true);
        let result = down(&agents, kill).await;
        assert!(
            matches!(
                result,
                Err(AgentError::Reservation(ReservationError::Timeout))
            ),
            "{kill}: {result:?}"
        );
        assert_eq!(mutations(&rig), [effect]);
        assert_eq!(rig.state("lead").await.status, AgentStatus::Ready);
        assert!(quarantined(&rig, "p/lead").await, "{kill}");
    }
}

#[tokio::test]
async fn adopt_tag_whose_call_outlives_the_deadline_quarantines() {
    // The same for an adopt: tmux sets the tag and its call does not return.
    // The pane carries a tag with no adoption recorded. Twin:
    // adopt_tag_refused_before_any_effect_does_not_quarantine.
    //
    // An adopt reads its project from the filesystem, which a paused clock
    // would time out. So the clock is paused once tmux has the tag write:
    // from there only the deadline is left, and virtual time reaches it.
    use crate::control::reservation::ReservationError;
    let adoption = Adoption::new().await;
    adoption.rig.row("s1", "%1", false).await;
    *adoption.rig.proc.lock().unwrap() = Some(job_in_front());
    adoption.script_untagged();
    adoption.script_untagged_capture(PI_PADDED, "pi");
    adoption.script_untagged_capture(PI_PADDED, "pi");
    adoption.script_tag();
    adoption.rig.client.stall_pane_effects(true);
    let (agents, project_root) = (adoption.agents.clone(), adoption.project.path().join("q"));
    let call = tokio::spawn(async move {
        agents
            .adopt(AdoptParams {
                project_root,
                name: "lead".into(),
                pane: "%1".into(),
                caller_server: None,
            })
            .await
    });
    while mutations(&adoption.rig).is_empty() {
        // An adopt that ends before its tag never writes one: fail, don't spin.
        if call.is_finished() {
            panic!("the adopt ended before its tag: {:?}", call.await);
        }
        tokio::task::yield_now().await;
    }
    tokio::time::pause();
    let result = call.await.unwrap();
    assert!(
        matches!(
            result,
            Err(AgentError::Reservation(ReservationError::Timeout))
        ),
        "{result:?}"
    );
    assert_eq!(
        mutations(&adoption.rig),
        ["set-option -p -t %1 @atm-agent q/lead"]
    );
    assert!(adoption.agent().await.is_none());
    assert!(quarantined(&adoption.rig, "q/lead").await);
}

/// `q/lead` with a pi role, recorded on %1 before the service starts: Up and
/// PaneCreated, then `more`. Its project is registered at the directory an
/// adopt names, so the adopt finds this agent.
async fn recorded_with_role(more: Vec<OrgCommand>, rows: &[(&str, bool, Harness)]) -> Adoption {
    let rig = Rig::new().await;
    let project = tempfile::tempdir().unwrap();
    std::fs::create_dir(project.path().join("q")).unwrap();
    let root = project.path().join("q").canonicalize().unwrap();
    rig.command(OrgCommand::RegisterProject {
        project_id: "q".parse().unwrap(),
        root: root.to_str().unwrap().into(),
        move_root: false,
    })
    .await;
    rig.command(OrgCommand::Up {
        agent_id: adoptee(),
        role: atm_org::parse_role("lead", "+++\nharness='pi'\n+++\n").unwrap(),
    })
    .await;
    rig.command(OrgCommand::PaneCreated {
        agent_id: adoptee(),
        pane: "%1".into(),
    })
    .await;
    for command in more {
        rig.command(command).await;
    }
    for (id, child, harness) in rows {
        rig.row_of(id, "%1", *child, *harness).await;
    }
    let recorded = rig.org.snapshot().await.unwrap();
    let recorded = recorded.agent(&adoptee()).unwrap();
    // The restart check: a Starting agent's reads its screen, and uses none
    // of it without a session; a Ready agent's reads it to pin its harness
    // (the screen is unknown, so nothing is pinned); any other only inspects.
    let tagged = Adoption::inspection(Some("q/lead"), "pi", "unknown\n");
    rig.client.script_inspect(&pane("%1"), Some(&tagged)).unwrap();
    if matches!(recorded.status, AgentStatus::Starting | AgentStatus::Ready) {
        rig.client
            .script_capture(&PaneCapture::new("unknown\n".into(), tagged, CaptureFormat::Ansi))
            .unwrap();
    }
    let (agents, _) = rig.start();
    agents.reconciled().await.unwrap();
    assert_eq!(rig.client.pending_reads(&pane("%1")).unwrap(), 0);
    let events = agents.observation_events();
    Adoption {
        rig,
        agents,
        project,
        events,
    }
}

#[tokio::test]
async fn adopt_of_an_agent_with_a_role_binds_only_its_own_session() {
    // Review 19, finding 5. A Ready agent with a role and no session is
    // adopted again on its idle pane. A discovery row shows that its harness
    // lives there; the other row on the pane is not the agent's to bind: the
    // session its clear excluded, a subagent's row, a row of another harness.
    // Nothing is bound and nothing is written.
    let cleared = || {
        vec![
            OrgCommand::SessionBound {
                agent_id: adoptee(),
                session_id: SessionId::new("old"),
            },
            OrgCommand::Clear {
                agent_id: adoptee(),
            },
            OrgCommand::EditorReady {
                agent_id: adoptee(),
            },
        ]
    };
    let ready = || {
        vec![OrgCommand::EditorReady {
            agent_id: adoptee(),
        }]
    };
    type Case = (&'static str, Vec<OrgCommand>, (&'static str, bool, Harness));
    let cases: [Case; 3] = [
        ("the excluded session", cleared(), ("old", false, Harness::Pi)),
        ("a child row", ready(), ("child", true, Harness::Pi)),
        ("a row of another harness", ready(), ("other", false, Harness::Codex)),
    ];
    for (case, commands, row) in cases {
        let adoption =
            recorded_with_role(commands, &[row, ("pending-100", false, Harness::Pi)]).await;
        *adoption.rig.proc.lock().unwrap() = Some(job_in_front());
        let recorded = adoption.agent().await.unwrap();
        assert_eq!(recorded.status, AgentStatus::Ready, "{case}");
        let before = adoption.rig.org.status().unwrap();
        script_readopt(&adoption, PI_PADDED, "pi");
        assert_eq!(adoption.adopt().await.unwrap().state, AgentStatus::Ready);
        assert_eq!(adoption.rig.client.pending_reads(&pane("%1")).unwrap(), 0);
        assert_eq!(adoption.rig.org.status().unwrap(), before, "{case}");
        assert_eq!(adoption.agent().await.unwrap(), recorded, "{case}");
    }
    // The twin: a session that is the agent's. The pane is in copy mode when
    // the row arrives, so the observer leaves the binding alone, and the
    // adopt, with the pane out of copy mode again, makes it.
    let adoption = recorded_with_role(Vec::new(), &[]).await;
    *adoption.rig.proc.lock().unwrap() = Some(job_in_front());
    let mut in_mode = Adoption::inspection(Some("q/lead"), "pi", "unknown\n");
    in_mode.in_mode = true;
    adoption.rig.client.script_inspect(&pane("%1"), Some(&in_mode)).unwrap();
    adoption
        .rig
        .client
        .script_capture(&PaneCapture::new("unknown\n".into(), in_mode, CaptureFormat::Ansi))
        .unwrap();
    adoption.rig.row("s1", "%1", false).await;
    settle(&adoption).await;
    assert_eq!(adoption.rig.client.pending_reads(&pane("%1")).unwrap(), 0);
    assert_eq!(adoption.agent().await.unwrap().session_id, None);
    script_readopt(&adoption, PI_PADDED, "pi");
    adoption.script_follow_up(None);
    assert_eq!(adoption.adopt().await.unwrap().state, AgentStatus::Ready);
    assert_eq!(
        adoption.agent().await.unwrap().session_id,
        Some(SessionId::new("s1"))
    );
    settle(&adoption).await;
    assert_eq!(adoption.rig.client.pending_reads(&pane("%1")).unwrap(), 0);
}

/// Every observation the events so far started has ended, successfully: the
/// observer has nothing running and nothing queued.
async fn settle(adoption: &Adoption) {
    let mut events = adoption.agents.observation_events();
    update_unrelated(&adoption.rig, &mut events).await;
    progress(&mut events, "idle", None).await;
}

#[tokio::test]
async fn adopt_of_an_agent_with_a_role_reads_the_screen_only_of_its_live_harness() {
    // A Starting agent with a pi role. With no row of pi on the pane its
    // screen is not read at all: nothing is pinned and it stays Starting.
    // (With only a row of another harness the adopt is refused:
    // adopt_refuses_a_role_whose_harness_is_not_the_panes.)
    let adoption = recorded_with_role(Vec::new(), &[]).await;
    let before = adoption.rig.org.status().unwrap();
    script_readopt(&adoption, PI_PADDED, "pi");
    *adoption.rig.proc.lock().unwrap() = Some(job_in_front());
    assert_eq!(adoption.adopt().await.unwrap().state, AgentStatus::Starting);
    assert_eq!(
        adoption.rig.client.pending_reads(&pane("%1")).unwrap(),
        6,
        "neither capture was read"
    );
    assert_eq!(
        adoption.agents.baseline_of(&adoptee(), "%1"),
        PaneBaseline::unproven()
    );
    assert_eq!(adoption.rig.org.status().unwrap(), before);

    // With pi's discovery row, the idle screen counts unless the capture
    // itself says the pane is dead, in a mode, or not tagged for the agent.
    type Change = fn(&mut PaneInspection);
    let cases: [(&str, Change, bool); 4] = [
        ("the twin: a live tagged pane", |_| {}, true),
        ("a dead pane", |i| i.dead = true, false),
        ("copy mode", |i| i.in_mode = true, false),
        ("no tag", |i| i.local_agent_tag = None, false),
    ];
    for (case, change, safe) in cases {
        let adoption =
            recorded_with_role(Vec::new(), &[("pending-100", false, Harness::Pi)]).await;
        *adoption.rig.proc.lock().unwrap() = Some(job_in_front());
        let mut committed = adoption.rig.org.committed.subscribe();
        let tagged = Adoption::inspection(Some("q/lead"), "pi", "");
        adoption.rig.client.script_inspect(&pane("%1"), Some(&tagged)).unwrap();
        adoption.script_capture(PI_PADDED, "pi");
        let text = body(PI_PADDED);
        let mut second = Adoption::inspection(Some("q/lead"), "pi", text);
        change(&mut second);
        adoption
            .rig
            .client
            .script_capture(&PaneCapture::new(text.into(), second, CaptureFormat::Ansi))
            .unwrap();
        let result = adoption.adopt().await;
        let kinds = committed_kinds(&mut committed);
        if safe {
            assert_eq!(result.unwrap().state, AgentStatus::Ready, "{case}");
            assert_eq!(kinds, ["agent.editor_ready"], "{case}");
        } else {
            // Either the control service refuses the capture or the flow
            // does not count it: the agent is not made Ready.
            assert!(
                !matches!(&result, Ok(adopted) if adopted.state == AgentStatus::Ready),
                "{case}: {result:?}"
            );
            assert_eq!(kinds, Vec::<String>::new(), "{case}");
            assert_eq!(
                adoption.agent().await.unwrap().status,
                AgentStatus::Starting,
                "{case}"
            );
        }
    }
}

// --- cp-u8-21: a second adopt of a Clearing or Stuck agent ---

/// `q/lead` with a pi role, Ready without a session on %1, its harness's
/// discovery row on the pane, and the service started.
async fn ready_with_role() -> Adoption {
    let adoption = recorded_with_role(
        vec![OrgCommand::EditorReady {
            agent_id: adoptee(),
        }],
        &[("pending-100", false, Harness::Pi)],
    )
    .await;
    *adoption.rig.proc.lock().unwrap() = Some(job_in_front());
    adoption
}

/// A session row arrives on %1. It wakes one observation, which inspects.
async fn row_arrives(adoption: &Adoption, session: &str) {
    adoption
        .rig
        .client
        .script_inspect(
            &pane("%1"),
            Some(&Adoption::inspection(Some("q/lead"), "pi", "")),
        )
        .unwrap();
    adoption.rig.row(session, "%1", false).await;
    settle(adoption).await;
    assert_eq!(adoption.rig.client.pending_reads(&pane("%1")).unwrap(), 0);
}

#[tokio::test]
async fn second_adopt_of_a_clearing_agent_changes_nothing() {
    // Decision A, Clearing. A clear that did not finish keeps waiting for its
    // own proof: an adopt answers Clearing and reads, tags and writes nothing,
    // on a safe idle screen, with only a discovery row and with a new session
    // on the pane. Twin: second_adopt_of_a_stuck_agent_on_a_safe_screen_makes_it_ready.
    for session in [None, Some("s2")] {
        let adoption = ready_with_role().await;
        adoption
            .rig
            .command(OrgCommand::Clear {
                agent_id: adoptee(),
            })
            .await;
        if let Some(session) = session {
            row_arrives(&adoption, session).await;
        }
        let recorded = adoption.agent().await.unwrap();
        assert_eq!(recorded.status, AgentStatus::Clearing);
        let mut committed = adoption.rig.org.committed.subscribe();
        let before = adoption.rig.org.status().unwrap();
        script_readopt(&adoption, PI_PADDED, "pi");
        let result = adoption.adopt().await.unwrap();
        assert_eq!(result.state, AgentStatus::Clearing, "{session:?}");
        assert_eq!(
            adoption.rig.client.pending_reads(&pane("%1")).unwrap(),
            6,
            "nothing was read after the inspection"
        );
        assert!(mutations(&adoption.rig).is_empty(), "{session:?}");
        assert_eq!(committed_kinds(&mut committed), Vec::<String>::new());
        assert_eq!(adoption.rig.org.status().unwrap(), before);
        assert_eq!(adoption.agent().await.unwrap(), recorded, "{session:?}");

        // Named on another pane, it is refused before that pane is looked at:
        // the agent is not moved and the other pane gets no tag.
        let elsewhere = adoption
            .agents
            .adopt(AdoptParams {
                project_root: adoption.project.path().join("q"),
                name: "lead".into(),
                pane: "%2".into(),
                caller_server: None,
            })
            .await;
        match elsewhere {
            Err(AgentError::Refused(reason)) => {
                assert!(reason.contains("already runs in %1"), "{reason}");
            }
            other => panic!("{other:?}"),
        }
        assert!(mutations(&adoption.rig).is_empty());
        assert_eq!(adoption.agent().await.unwrap(), recorded);
    }
}

#[tokio::test]
async fn second_adopt_of_a_stuck_agent_on_a_safe_screen_makes_it_ready() {
    // Decision A, Stuck (the owner's choice, 2026-11-15): an explicit adopt is
    // the owner's way out of Stuck. On a safe idle screen it makes the agent
    // Ready and clears the reason; it binds the pane's one session, if any.
    for bound in [false, true] {
        let mut adoption = ready_with_role().await;
        if bound {
            row_arrives(&adoption, "s1").await;
            assert_eq!(
                adoption.agent().await.unwrap().session_id,
                Some(SessionId::new("s1"))
            );
        }
        adoption
            .rig
            .command(OrgCommand::Stuck {
                agent_id: adoptee(),
                reason: "the trust dialog reset again".into(),
            })
            .await;
        // The twin: a busy screen establishes nothing, and Stuck stays.
        let recorded = adoption.agent().await.unwrap();
        assert_eq!(recorded.status, AgentStatus::Stuck);
        let mut committed = adoption.rig.org.committed.subscribe();
        script_readopt(&adoption, PI_RUNNING, "pi");
        assert_eq!(adoption.adopt().await.unwrap().state, AgentStatus::Stuck);
        assert_eq!(adoption.rig.client.pending_reads(&pane("%1")).unwrap(), 0);
        assert_eq!(committed_kinds(&mut committed), Vec::<String>::new());
        assert_eq!(adoption.agent().await.unwrap(), recorded, "{bound}");

        script_readopt(&adoption, PI_PADDED, "pi");
        adoption.script_follow_up(None);
        assert_eq!(adoption.adopt().await.unwrap().state, AgentStatus::Ready);
        assert_eq!(
            committed_kinds(&mut committed),
            if bound {
                vec!["agent.adopted"]
            } else {
                vec!["agent.adopted", "agent.editor_ready"]
            }
        );
        let agent = adoption.agent().await.unwrap();
        assert_eq!(agent.status, AgentStatus::Ready, "{bound}");
        assert_eq!(agent.reason, None, "{bound}");
        assert_eq!(agent.session_id, bound.then(|| SessionId::new("s1")));
        assert!(mutations(&adoption.rig).is_empty(), "{bound}");
        adoption.followed().await;
    }
}

// --- cp-u8-21 D: a role whose harness is not the pane's ---

/// `q/lead` was started with a pi role and taken down: it has a role and no
/// pane. `rows` are the top-level session rows on %1.
async fn taken_down_with_role(rows: &[(&str, Harness)]) -> Adoption {
    let rig = Rig::new().await;
    let project = tempfile::tempdir().unwrap();
    std::fs::create_dir(project.path().join("q")).unwrap();
    let root = project.path().join("q").canonicalize().unwrap();
    rig.command(OrgCommand::RegisterProject {
        project_id: "q".parse().unwrap(),
        root: root.to_str().unwrap().into(),
        move_root: false,
    })
    .await;
    rig.command(OrgCommand::Up {
        agent_id: adoptee(),
        role: atm_org::parse_role("lead", "+++\nharness='pi'\n+++\n").unwrap(),
    })
    .await;
    rig.command(OrgCommand::Down {
        agent_id: adoptee(),
        reason: "taken down".into(),
    })
    .await;
    for (id, harness) in rows {
        rig.row_of(id, "%1", false, *harness).await;
    }
    *rig.proc.lock().unwrap() = Some(job_in_front());
    let (agents, _) = rig.start();
    agents.reconciled().await.unwrap();
    let events = agents.observation_events();
    Adoption {
        rig,
        agents,
        project,
        events,
    }
}

#[tokio::test]
async fn adopt_refuses_a_role_whose_harness_is_not_the_panes() {
    // Decision D. The agent's role names pi and the only session on the pane
    // is a Codex one: nothing could ever bind, and the agent would be
    // Starting for good. Refused before the tag, with nothing recorded.
    let refused = |result: Result<AdoptResult, AgentError>| match result {
        Err(AgentError::Refused(reason)) => {
            assert!(reason.contains("%1 runs codex"), "{reason}");
            assert!(reason.contains("role names pi"), "{reason}");
            assert!(reason.contains("nothing was changed"), "{reason}");
        }
        other => panic!("{other:?}"),
    };
    // A real session row, and a discovery row: both show which harness runs.
    for row in ["other", "pending-100"] {
        let adoption = taken_down_with_role(&[(row, Harness::Codex)]).await;
        let recorded = adoption.agent().await.unwrap();
        let before = adoption.rig.org.status().unwrap();
        adoption
            .rig
            .client
            .script_inspect(&pane("%1"), Some(&Adoption::inspection(None, "codex", "")))
            .unwrap();
        refused(adoption.adopt().await);
        assert!(mutations(&adoption.rig).is_empty(), "refused before the tag");
        assert_eq!(adoption.rig.client.pending_reads(&pane("%1")).unwrap(), 0);
        assert_eq!(adoption.rig.org.status().unwrap(), before, "{row}");
        assert_eq!(adoption.agent().await.unwrap(), recorded, "{row}");
        assert!(!quarantined(&adoption.rig, "q/lead").await);
    }

    // The same for an agent that is already recorded on the pane and tagged.
    let adoption = recorded_with_role(Vec::new(), &[("other", false, Harness::Codex)]).await;
    let recorded = adoption.agent().await.unwrap();
    let before = adoption.rig.org.status().unwrap();
    adoption
        .rig
        .client
        .script_inspect(
            &pane("%1"),
            Some(&Adoption::inspection(Some("q/lead"), "codex", "")),
        )
        .unwrap();
    refused(adoption.adopt().await);
    assert_eq!(adoption.rig.client.pending_reads(&pane("%1")).unwrap(), 0);
    assert_eq!(adoption.rig.org.status().unwrap(), before);
    assert_eq!(adoption.agent().await.unwrap(), recorded);

    // The twin: the pane's session is a pi one, and the agent is adopted.
    let mut adoption = taken_down_with_role(&[("s1", Harness::Pi)]).await;
    adoption.script_untagged();
    adoption.script_untagged_capture(PI_PADDED, "pi");
    adoption.script_untagged_capture(PI_PADDED, "pi");
    adoption.script_tag();
    adoption.script_follow_up(None);
    assert_eq!(adoption.adopt().await.unwrap().state, AgentStatus::Ready);
    assert_eq!(adoption.tags_set(), 1);
    let agent = adoption.agent().await.unwrap();
    assert_eq!(agent.pane.as_deref(), Some("%1"));
    assert_eq!(agent.session_id, Some(SessionId::new("s1")));
    adoption.followed().await;
}

#[tokio::test]
async fn adopt_of_an_agent_stuck_by_an_interrupted_clear_makes_it_ready() {
    // Decision A, as answered for the reason a daemon restart gives a clear
    // it interrupted: the agent is Stuck like any other, and the adopt of
    // its pane, on a safe idle screen, is the answer to "inspect %1".
    let mut adoption = recorded_with_role(
        vec![
            OrgCommand::EditorReady {
                agent_id: adoptee(),
            },
            OrgCommand::Clear {
                agent_id: adoptee(),
            },
        ],
        &[("pending-100", false, Harness::Pi)],
    )
    .await;
    *adoption.rig.proc.lock().unwrap() = Some(job_in_front());
    let recorded = adoption.agent().await.unwrap();
    assert_eq!(recorded.status, AgentStatus::Stuck);
    assert_eq!(
        recorded.reason.as_deref(),
        Some("clear interrupted by daemon restart; inspect %1")
    );
    let mut committed = adoption.rig.org.committed.subscribe();
    script_readopt(&adoption, PI_PADDED, "pi");
    adoption.script_follow_up(None);
    assert_eq!(adoption.adopt().await.unwrap().state, AgentStatus::Ready);
    assert_eq!(
        committed_kinds(&mut committed),
        ["agent.adopted", "agent.editor_ready"]
    );
    let agent = adoption.agent().await.unwrap();
    assert_eq!(agent.status, AgentStatus::Ready);
    assert_eq!(agent.reason, None);
    assert!(mutations(&adoption.rig).is_empty());
    adoption.followed().await;
}

// --- cp-u8-21 B: every read and capture of an adopt comes before its tag ---

#[tokio::test]
async fn adopt_that_fails_before_its_tag_leaves_no_tag_and_no_quarantine() {
    // Review 19, probe r07: a capture of the adopt fails. It used to come
    // after the tag and left a tagged pane with no adoption recorded, and a
    // quarantine. Now the pane is not tagged yet: nothing is changed, nothing
    // is recorded, and the same adopt succeeds when the pane can be read.
    let mut adoption = Adoption::new().await;
    adoption.rig.row("s1", "%1", false).await;
    *adoption.rig.proc.lock().unwrap() = Some(job_in_front());
    for captures in [0, 1] {
        adoption.script_untagged();
        for _ in 0..captures {
            adoption.script_untagged_capture(PI_PADDED, "pi");
        }
        let result = adoption.adopt().await;
        assert!(
            matches!(result, Err(AgentError::Reservation(_))),
            "{captures}: {result:?}"
        );
        assert!(mutations(&adoption.rig).is_empty(), "{captures}");
        assert_eq!(adoption.rig.client.pending_reads(&pane("%1")).unwrap(), 0);
        assert!(adoption.agent().await.is_none(), "{captures}");
        assert_eq!(
            adoption.agents.baseline_of(&adoptee(), "%1"),
            PaneBaseline::unproven()
        );
        assert!(!quarantined(&adoption.rig, "q/lead").await, "{captures}");
    }
    adoption.script_untagged();
    adoption.script_untagged_capture(PI_PADDED, "pi");
    adoption.script_untagged_capture(PI_PADDED, "pi");
    adoption.script_tag();
    adoption.script_follow_up(None);
    assert_eq!(adoption.adopt().await.unwrap().state, AgentStatus::Ready);
    assert_eq!(adoption.tags_set(), 1);
    adoption.followed().await;
}

#[tokio::test]
async fn adopt_tag_ok_then_adopt_append_fails_quarantines() {
    // Review 19, mutation m07. What can still fail after the tag is the
    // organisation's write. The pane then carries a tag with no adoption
    // behind it, and the cleanup must not release the reservation. No
    // baseline is kept for a pane the organisation does not give the agent.
    // Twin: adopt_that_fails_before_its_tag_leaves_no_tag_and_no_quarantine.
    let adoption = Adoption::on(Rig::gated_with_failure(None, Some("agent.adopted")).await).await;
    adoption.rig.row("s1", "%1", false).await;
    *adoption.rig.proc.lock().unwrap() = Some(job_in_front());
    adoption.script_untagged();
    adoption.script_untagged_capture(PI_PADDED, "pi");
    adoption.script_untagged_capture(PI_PADDED, "pi");
    adoption.script_tag();
    let result = adoption.adopt().await;
    assert!(
        matches!(result, Err(AgentError::Organisation(_))),
        "{result:?}"
    );
    assert_eq!(
        mutations(&adoption.rig),
        ["set-option -p -t %1 @atm-agent q/lead"]
    );
    assert_eq!(adoption.rig.client.pending_reads(&pane("%1")).unwrap(), 0);
    assert!(adoption.agent().await.is_none());
    assert_eq!(
        adoption.agents.baseline_of(&adoptee(), "%1"),
        PaneBaseline::unproven()
    );
    assert!(quarantined(&adoption.rig, "q/lead").await);
}

#[tokio::test]
async fn adopt_recorded_then_editor_ready_append_fails_is_starting_and_released() {
    // The second write fails after the first: the adoption is recorded, so
    // the tag has its record and nothing is quarantined. The agent is
    // Starting on its pane with its baseline, as after an adopt on a busy
    // screen, and the call reports the failure.
    let mut adoption =
        Adoption::on(Rig::gated_with_failure(None, Some("agent.editor_ready")).await).await;
    adoption
        .rig
        .row_of("pending-100", "%1", false, Harness::Codex)
        .await;
    *adoption.rig.proc.lock().unwrap() = Some(job_in_front());
    adoption.script_untagged();
    adoption.script_untagged_capture(CODEX_PADDED, "codex");
    adoption.script_untagged_capture(CODEX_PADDED, "codex");
    adoption.script_tag();
    adoption.script_follow_up(Some((CODEX_PADDED, "codex")));
    let result = adoption.adopt().await;
    assert!(
        matches!(result, Err(AgentError::Organisation(_))),
        "{result:?}"
    );
    assert_eq!(adoption.tags_set(), 1);
    let agent = adoption.agent().await.unwrap();
    assert_eq!(agent.status, AgentStatus::Starting);
    assert_eq!(agent.pane.as_deref(), Some("%1"));
    assert_eq!(
        adoption.agents.baseline_of(&adoptee(), "%1"),
        PaneBaseline::adopted(100).with_harness("codex".into()).unwrap()
    );
    adoption.followed().await;
    assert!(!quarantined(&adoption.rig, "q/lead").await);
}

#[tokio::test]
async fn row_that_arrives_during_an_adopt_is_bound_by_its_next_event() {
    // The adopt reads the pane's rows once, before its captures and its tag.
    // A session that registers after that read (here: while the Adopt is
    // being written) is not bound by the adopt, and not lost: the agent is
    // Ready without a session, as its screen showed, and the session's next
    // event binds it through the observer.
    let (gate, mut entered) = mpsc::channel(1);
    let mut adoption = Adoption::on(Rig::gated(Some(("agent.adopted", gate))).await).await;
    adoption
        .rig
        .row_of("pending-100", "%1", false, Harness::Pi)
        .await;
    *adoption.rig.proc.lock().unwrap() = Some(job_in_front());
    adoption.script_untagged();
    adoption.script_untagged_capture(PI_PADDED, "pi");
    adoption.script_untagged_capture(PI_PADDED, "pi");
    adoption.script_tag();
    adoption.script_follow_up(None);
    let (agents, project_root) = (adoption.agents.clone(), adoption.project.path().join("q"));
    let mut call = tokio::spawn(async move {
        agents
            .adopt(AdoptParams {
                project_root,
                name: "lead".into(),
                pane: "%1".into(),
                caller_server: None,
            })
            .await
    });
    // The adopt reaches its write, or the test fails: it never waits on a
    // write that an adopt which ended early will not make.
    let release = tokio::select! {
        release = entered.recv() => release.unwrap(),
        result = &mut call => panic!("the adopt ended before its write: {result:?}"),
    };
    // No agent is recorded on %1 yet, so the new row wakes nobody.
    adoption.rig.row("s1", "%1", false).await;
    settle(&adoption).await;
    release.send(()).unwrap();
    assert_eq!(call.await.unwrap().unwrap().state, AgentStatus::Ready);
    let agent = adoption.agent().await.unwrap();
    assert_eq!(agent.status, AgentStatus::Ready);
    assert_eq!(agent.session_id, None, "the adopt read the rows before s1");
    adoption.followed().await;
    adoption
        .rig
        .client
        .script_inspect(
            &pane("%1"),
            Some(&Adoption::inspection(Some("q/lead"), "pi", "")),
        )
        .unwrap();
    update(&adoption.rig, "s1").await;
    progress(&mut adoption.events, "finished", Some("q/lead")).await;
    let agent = adoption.agent().await.unwrap();
    assert_eq!(agent.status, AgentStatus::Ready);
    assert_eq!(agent.session_id, Some(SessionId::new("s1")));
}

// --- Two checks of an adopt that the mutation run of cp-u8n found unheld ---

#[tokio::test]
async fn adopt_does_not_count_the_excluded_session_as_the_roles_harness() {
    // Decision D. The agent's old pi session, which its clear excluded, still
    // has its row on the pane beside a Codex session. The excluded row is no
    // evidence that pi lives there (it is not for the screen read either):
    // the pane runs Codex, and the adopt is refused.
    let adoption = recorded_with_role(
        vec![
            OrgCommand::SessionBound {
                agent_id: adoptee(),
                session_id: SessionId::new("old"),
            },
            OrgCommand::Clear {
                agent_id: adoptee(),
            },
            OrgCommand::EditorReady {
                agent_id: adoptee(),
            },
        ],
        &[("old", false, Harness::Pi), ("other", false, Harness::Codex)],
    )
    .await;
    let recorded = adoption.agent().await.unwrap();
    assert_eq!(recorded.excluded_session_id, Some(SessionId::new("old")));
    let before = adoption.rig.org.status().unwrap();
    adoption
        .rig
        .client
        .script_inspect(
            &pane("%1"),
            Some(&Adoption::inspection(Some("q/lead"), "codex", "")),
        )
        .unwrap();
    match adoption.adopt().await {
        Err(AgentError::Refused(reason)) => {
            assert!(reason.contains("%1 runs codex"), "{reason}");
            assert!(reason.contains("role names pi"), "{reason}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(adoption.rig.client.pending_reads(&pane("%1")).unwrap(), 0);
    assert_eq!(adoption.rig.org.status().unwrap(), before);
    assert_eq!(adoption.agent().await.unwrap(), recorded);
}

#[tokio::test]
async fn readopt_on_a_busy_screen_keeps_the_record_and_records_the_baseline() {
    // An agent that is recorded on its pane with nothing known about the
    // pane's processes, as after a daemon restart. Adopting it again while a
    // turn runs establishes no readiness, so its record is kept as it is; but
    // the harness was recognised in the foreground, and that is recorded.
    let adoption = ready_with_role().await;
    assert_eq!(
        adoption.agents.baseline_of(&adoptee(), "%1"),
        PaneBaseline::unproven()
    );
    let recorded = adoption.agent().await.unwrap();
    let mut committed = adoption.rig.org.committed.subscribe();
    script_readopt(&adoption, PI_RUNNING, "pi");
    assert_eq!(adoption.adopt().await.unwrap().state, AgentStatus::Ready);
    assert_eq!(adoption.rig.client.pending_reads(&pane("%1")).unwrap(), 0);
    assert_eq!(committed_kinds(&mut committed), Vec::<String>::new());
    assert_eq!(adoption.agent().await.unwrap(), recorded);
    assert_eq!(
        adoption.agents.baseline_of(&adoptee(), "%1"),
        PaneBaseline::adopted(100).with_harness("pi".into()).unwrap()
    );
}
