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
