// The daemon's stop over the real control actor and the real agent service.
//
// Included after `agent_clear_tests.rs`, into the same module, for its rig:
// no module docs. Every test runs on a paused clock with in-memory I/O only
// and stops the services with the server's own `stop_stages`, synchronised
// on the flow's progress events and on the stage's task.

use crate::control::reservation::ReservationError;
use crate::server::stop::{stop_stages, Overrun, Stage, Stages, AGENTS};
use tokio::time::Instant;

const AWAITING_SESSION: &str = "the harness has not announced a new session";

/// `rig`'s organisation, registry and tmux with another control actor and
/// another token for the agent service started on it.
fn sharing(
    rig: &Rig,
    control: crate::control::service::ControlHandle,
    cancel: &CancellationToken,
) -> Rig {
    Rig {
        org: rig.org.clone(),
        registry: rig.registry.clone(),
        events: rig.events.clone(),
        client: rig.client.clone(),
        control,
        cancel: cancel.clone(),
        proc: rig.proc.clone(),
    }
}

/// As `Clearing::on` with a bound agent, but the agent service's token and
/// task are kept as the server keeps them: its agents stage.
async fn clearing_with_stage() -> (Clearing, Stage) {
    let harness = Harness::ClaudeCode;
    let name = harness.short_tag();
    let mut rig = Rig::new().await;
    rig.agent_of("lead", name, Some("%1"), Some("old")).await;
    start_session(&rig, harness, "old", Some("startup")).await;
    rig.frame_of("%1", "p/lead", body(CLAUDE_CONVERSATION), name);
    // The rig's own token stops its organisation and control too, and is
    // never cancelled here: only the agent service gets `cancel`.
    let cancel = CancellationToken::new();
    rig.cancel = cancel.clone();
    let (agents, task) = rig.start();
    agents.reconciled().await.unwrap();
    assert_eq!(rig.state("lead").await.status, AgentStatus::Ready);
    agents.record_baseline(
        lead(),
        "%1",
        PaneBaseline::adopted(100)
            .with_harness(name.into())
            .unwrap(),
    );
    let events = agents.observation_events();
    let clearing = Clearing {
        rig,
        agents,
        events,
        harness,
    };
    // `start` gave the service a child of `cancel`.
    let stage = Stage {
        cancel,
        task: Some(task),
    };
    (clearing, stage)
}

struct Stopped {
    clearing: Clearing,
    overruns: Vec<Overrun>,
    took: std::time::Duration,
    call: tokio::task::JoinHandle<Result<ClearResult, AgentError>>,
}

/// A clear that has submitted its command and waits for the new session;
/// then the daemon stops, with or without stopping reserved work first.
/// Messages and control are not started: the organisation stays readable.
async fn stop_during_clear(stop_work: bool) -> Stopped {
    let (mut clearing, agents) = clearing_with_stage().await;
    // No frame after the Enter: the flow looks again only when a session
    // arrives, and none does.
    clearing.frames_through_enter(CLAUDE_CONVERSATION, CLAUDE_STAGED);
    let call = clearing.start();
    clearing.awaiting_session().await;
    assert_eq!(clearing.agent().await.status, AgentStatus::Clearing);
    let control = clearing.rig.control.clone();
    let stages = Stages {
        agents,
        ..Stages::default()
    };
    let started = Instant::now();
    let overruns = stop_stages(
        || {
            if stop_work {
                control.stop_reserved_work();
            }
        },
        stages,
    )
    .await;
    Stopped {
        clearing,
        overruns,
        took: started.elapsed(),
        call,
    }
}

#[tokio::test(start_paused = true)]
async fn stop_with_a_held_clear_ends_inside_the_budget() {
    let stopped = stop_during_clear(true).await;
    // The agents stage ended by itself, before its bound.
    assert_eq!(stopped.overruns, []);
    assert!(stopped.took < AGENTS, "{:?}", stopped.took);
    // The clear was stopped, not completed, and its caller is told so.
    let result = stopped.call.await.unwrap();
    assert!(
        matches!(
            result,
            Err(AgentError::Reservation(ReservationError::Unavailable))
        ),
        "{result:?}"
    );
    // Its cleanup recorded what a deadline would have: Stuck, with what the
    // flow last waited for, and one note to the owner.
    stopped.clearing.assert_stuck(AWAITING_SESSION).await;
    // Paste and Enter went out before the stop; nothing was sent or read
    // after it.
    assert_eq!(stopped.clearing.sent().len(), 2);
    assert_eq!(stopped.clearing.unread(), 0);
}

#[tokio::test(start_paused = true)]
async fn without_the_work_stop_a_held_clear_outlives_the_agents_stage() {
    // Twin of the test above: the stage waits for the clear, whose own
    // deadline is later than the stage's bound, and is reported.
    let stopped = stop_during_clear(false).await;
    let agents = Overrun {
        stage: "agents",
        bound: AGENTS,
    };
    assert_eq!(stopped.overruns, [agents]);
    assert_eq!(stopped.took, AGENTS);
    assert_eq!(
        stopped.clearing.agent().await.status,
        AgentStatus::Clearing
    );
}

#[tokio::test(start_paused = true)]
async fn restart_keeps_a_stopped_clears_record() {
    let stopped = stop_during_clear(true).await;
    stopped.clearing.assert_stuck(AWAITING_SESSION).await;
    let before = stopped.clearing.agent().await;
    // The next daemon: a new control actor and a new agent service over the
    // same organisation. Its restart check inspects the pane once.
    let old = &stopped.clearing.rig;
    let cancel = CancellationToken::new();
    let (control, _) = spawn_control_with_org(
        Box::new(old.client.clone()),
        old.registry.clone(),
        old.org.clone(),
        cancel.clone(),
    );
    let rig = sharing(old, control, &cancel);
    rig.inspect_only("%1", "p/lead");
    let (agents, _) = rig.start();
    agents.reconciled().await.unwrap();
    assert_eq!(rig.client.pending_reads(&pane("%1")).unwrap(), 0);
    // The restart pass wrote nothing: the same record, and still one note.
    assert_eq!(rig.state("lead").await, before);
    stopped.clearing.assert_stuck(AWAITING_SESSION).await;
    let state = rig.org.snapshot().await.unwrap();
    let to_owner = state
        .messages()
        .filter(|message| message.content.to == atm_org::Address::Owner)
        .count();
    assert_eq!(to_owner, 1);
}
