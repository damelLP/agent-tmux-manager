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
        let rig = Rig::new().await;
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
    /// The reads of an adopt of an untagged pane up to and including the tag.
    fn script_tagging(&self) {
        let untagged = Self::inspection(None, "pi", "");
        self.rig
            .client
            .script_inspect(&pane("%1"), Some(&untagged))
            .unwrap();
        self.rig.client.script_tag_check(&untagged).unwrap();
    }
    /// One capture of the tagged pane showing `text` with `command` in front.
    fn script_capture(&self, text: &str, command: &str) {
        let text = body(text);
        self.rig
            .client
            .script_capture(&PaneCapture::new(
                text.into(),
                Self::inspection(Some("q/lead"), command, text),
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
    adoption.script_tagging();
    adoption.script_capture(PI_PADDED, "pi");
    adoption.script_capture(PI_PADDED, "pi");
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
    adoption.script_tagging();
    adoption.script_capture(PI_RUNNING, "pi");
    adoption.script_capture(PI_RUNNING, "pi");
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
    adoption.script_tagging();
    adoption.script_capture("unknown\n", "pi");
    adoption.script_capture("unknown\n", "pi");
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
        adoption.script_tagging();
        // The same safe idle frame every time: only the process facts differ.
        adoption.script_capture(PI_PADDED, command);
        if readable {
            adoption.script_capture(PI_PADDED, command);
        }
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
    adoption.script_tagging();
    // The process facts were read between a `node` and a `pi`: they describe neither.
    adoption.script_capture(PI_PADDED, "node");
    adoption.script_capture(PI_PADDED, "pi");
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
    adoption.script_tagging();
    adoption.script_capture(CODEX_COMPACT, "codex");
    adoption.script_capture(CODEX_COMPACT, "codex");
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
    adoption.script_tagging();
    adoption.script_capture(CODEX_PADDED, "codex");
    adoption.script_capture(CODEX_PADDED, "codex");
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
    adoption.script_tagging();
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
    adoption.script_tagging();
    adoption.script_capture(PI_PADDED, "pi");
    adoption.script_capture(PI_PADDED, "pi");
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
    adoption.script_tagging();
    adoption.script_capture(CODEX_PADDED, "codex");
    adoption.script_capture(CODEX_PADDED, "codex");
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
    adoption.script_tagging();
    adoption.script_capture(CODEX_WORKING, "codex");
    adoption.script_capture(CODEX_WORKING, "codex");
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
