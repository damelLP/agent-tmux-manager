//! The effects of the named-agent lifecycle flows.
//!
//! Each flow runs inside one reservation of its agent: it reads the committed
//! organisation state again, performs its pane effects through the
//! reservation, and reports through the organisation log. What a flow decides
//! from a screen lives in `agent_flow`, which is pure.

use super::agent_flow::{
    adopt_baseline, clear_noop, clear_refusal, cleared_session, decide_clear, decide_start,
    input_refusal, launch_enter_allowed, launch_line, launch_line_is_staged, launch_paste_allowed,
    placement, prompt_line_allowed, safe_editor, safe_to_paste, shell_baseline, shell_refusal,
    start_view, started_session, submit_allowed, submit_step, ClearProgress, KeyAction, ProcFacts,
    Ready, StartProgress, StartView, Step,
};
use super::agent_launch::{launch_command, LaunchCommand};
use super::agent_project::PreparedAgent;
use super::agent_screen::{agent_screen, AgentScreen, PaneBaseline};
use super::agents::{
    harness, matches_agent, AdoptResult, AgentError, AgentRuntime, ClearResult, DownResult,
    UpResult,
};
use crate::control::reservation::{
    ExpectedTag, PaneCapture, PaneId, PaneKey, Reservation, ReservationError,
};
use crate::control::{service::ServiceError, SpawnContext};
use crate::registry::PaneSession;
use crate::server::methods::CallContext;
use atm_core::harness_control::{ClearProof, ControlDialog, ControlScreen, SessionArrival};
use atm_core::{Harness, SessionId, SessionView};
use atm_org::{AgentId, AgentStatus, OrgCommand};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tokio::time::timeout;

/// Set once a flow has changed a pane, so its cleanup knows that the
/// organisation log must account for it.
pub(super) type Effect = Arc<AtomicBool>;

impl AgentRuntime {
    /// Take an agent down: kill its pane, or release it by removing the tag
    /// and leaving its program alone, then record Down.
    ///
    /// A pane that is positively gone already satisfies either. A pane that
    /// carries another tag is not this agent's any more: nothing is changed
    /// and the call refuses. No keys are sent and no end event is awaited.
    pub(super) async fn down(
        &self,
        r: &Reservation,
        id: &AgentId,
        reserved: Option<&PaneId>,
        kill: bool,
        effect: &Effect,
    ) -> Result<DownResult, AgentError> {
        let state = self.org.snapshot().await?;
        let agent = state
            .agent(id)
            .ok_or_else(|| AgentError::NotFound(format!("agent {id} was not found")))?;
        let done = DownResult {
            agent_id: id.clone(),
        };
        if agent.status == AgentStatus::Down {
            return Ok(done);
        }
        let pane = agent.pane.as_ref().map(PaneId::parse).transpose()?;
        if pane.as_ref() != reserved {
            return Err(AgentError::Conflict(format!(
                "agent {id} changed its pane while this call waited; try again"
            )));
        }
        let mut reason = String::from("taken down");
        if let Some(pane) = &pane {
            match r.inspect(pane).await? {
                None => reason = format!("taken down; {pane} was already gone"),
                Some(found) if found.local_agent_tag.as_deref() != Some(&id.to_string()) => {
                    return Err(AgentError::Refused(format!(
                        "{pane} is no longer tagged for agent {id} (found {}); nothing was changed",
                        found.local_agent_tag.as_deref().unwrap_or("no tag")
                    )));
                }
                Some(_) => {
                    // Set before the call: work that is stopped while the
                    // call is outstanding may have changed the pane.
                    effect.store(true, Ordering::SeqCst);
                    let done = if kill {
                        let action = serde_json::json!({
                            "method": "agent.down",
                            "agent": id.to_string(),
                        });
                        r.kill(pane, id.clone(), action, chrono::Utc::now()).await
                    } else {
                        r.untag(pane, id.clone()).await
                    };
                    match changed(effect, done) {
                        // It went away after the inspection: what a down wants.
                        Err(ReservationError::PaneGone { .. }) => {
                            reason = format!("taken down; {pane} was already gone");
                        }
                        Err(error) => return Err(error.into()),
                        Ok(()) if kill => {
                            if r.inspect(pane).await?.is_some() {
                                return Err(AgentError::Internal(format!(
                                    "{pane} is still there after it was killed"
                                )));
                            }
                            reason = format!("taken down; {pane} killed");
                        }
                        Ok(()) => {
                            if r.inspect(pane)
                                .await?
                                .is_some_and(|found| found.local_agent_tag.is_some())
                            {
                                return Err(AgentError::Internal(format!(
                                    "{pane} still carries a tag after it was released"
                                )));
                            }
                            reason =
                                format!("taken down; {pane} released with its program running");
                        }
                    }
                }
            }
        }
        self.org
            .execute(
                OrgCommand::Down {
                    agent_id: id.clone(),
                    reason,
                },
                chrono::Utc::now(),
            )
            .await?;
        self.forget_baseline(id);
        Ok(done)
    }

    /// The cleanup every flow with pane effects ends on. The snapshot is read
    /// after the work has stopped, so it accounts for a command whose reply
    /// was lost. If the flow changed its pane and the agent is still recorded
    /// with it, the record and the pane disagree and nothing here can say
    /// which is right: fail, so the reservation is quarantined, not released.
    pub(super) async fn settle_down(
        &self,
        id: &AgentId,
        effect: &Effect,
    ) -> Result<(), AgentError> {
        let state = self.org.snapshot().await?;
        if effect.load(Ordering::SeqCst)
            && state
                .agent(id)
                .is_some_and(|agent| agent.status != AgentStatus::Down)
        {
            return Err(AgentError::Internal(format!(
                "agent {id}'s pane was changed but its Down is not recorded"
            )));
        }
        Ok(())
    }
}

/// Account for the one pane operation of a flow, whose `effect` flag the flow
/// set before the call (so that work stopped during the call counts as a
/// change). The flag is cleared when the call failed in one of the ways
/// [`ReservationError`] documents as "before effect" for a pane operation:
/// then nothing was changed and nothing needs accounting for. Every other
/// outcome, success included, leaves it set: an error can follow the effect.
/// (`InvalidRequest` is left out: for a general operation it is not documented
/// as before effect.)
///
/// Only for a flow with a single pane effect: clearing the flag would forget
/// an earlier one.
pub(super) fn changed(
    effect: &Effect,
    result: Result<(), ReservationError>,
) -> Result<(), ReservationError> {
    if result.as_ref().is_err_and(before_effect) {
        effect.store(false, Ordering::SeqCst);
    }
    result
}

/// The errors [`ReservationError`] documents as "before effect" for a pane
/// operation: the call that answered with one sent nothing and changed nothing.
fn before_effect(error: &ReservationError) -> bool {
    matches!(
        error,
        ReservationError::NotImplemented { .. }
            | ReservationError::Busy { .. }
            | ReservationError::StaleReservation
            | ReservationError::ScreenChanged { .. }
            | ReservationError::PacingRefused { .. }
            | ReservationError::PaneInputRefused { .. }
            | ReservationError::UnsafePaste { .. }
            | ReservationError::TagMismatch { .. }
            | ReservationError::PaneGone { .. }
            | ReservationError::InvalidObservation { .. }
            | ReservationError::Quarantined { .. }
            | ReservationError::Control(crate::control::ControlError::SendRefused { .. })
    )
}

/// How long a flow waits for the registry to answer one read.
const REGISTRY_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);

impl AgentRuntime {
    /// The sessions on a pane, checked: a registry that can't answer is an
    /// error, never "no sessions".
    async fn pane_sessions(&self, pane: &PaneId) -> Result<Vec<PaneSession>, AgentError> {
        timeout(REGISTRY_BUDGET, self.registry.pane_sessions(pane.as_str()))
            .await
            .map_err(|_| AgentError::Timeout)?
            .map_err(|error| AgentError::Unavailable(format!("session registry: {error}")))
    }

    /// Take a pane that already runs a harness under this agent's name: tag
    /// it, and record the agent on it. No key is sent and nothing in the pane
    /// is changed but the tag.
    ///
    /// The agent is Ready only when this call's own capture shows a safe idle
    /// editor of a harness that is positively in the pane's foreground, with a
    /// live session row. Otherwise it is Starting until the harness's next
    /// event, as after a daemon restart.
    pub(super) async fn adopt(
        &self,
        r: &Reservation,
        id: &AgentId,
        pane: &PaneId,
        effect: &Effect,
    ) -> Result<AdoptResult, AgentError> {
        let state = self.org.snapshot().await?;
        let tag = id.to_string();
        let on_pane = |agent: &&atm_org::Agent| {
            agent.status != AgentStatus::Down && agent.pane.as_deref() == Some(pane.as_str())
        };
        if let Some(holder) = state.agents().filter(on_pane).find(|agent| agent.id != *id) {
            return Err(AgentError::Refused(format!(
                "{pane} already belongs to agent {}",
                holder.id
            )));
        }
        let existing = state
            .agent(id)
            .filter(|agent| agent.status != AgentStatus::Down);
        if let Some(elsewhere) = existing.and_then(|agent| agent.pane.as_ref()) {
            if elsewhere != pane.as_str() {
                return Err(AgentError::Refused(format!(
                    "agent {id} already runs in {elsewhere}; take it down first"
                )));
            }
        }
        let found = r
            .inspect(pane)
            .await?
            .ok_or_else(|| AgentError::Refused(format!("{pane} does not exist")))?;
        if found.dead {
            return Err(AgentError::Refused(format!(
                "the program in {pane} has exited"
            )));
        }
        let tagged = match found.local_agent_tag.as_deref() {
            None => false,
            Some(found) if found == tag => true,
            Some(other) => {
                return Err(AgentError::Refused(format!(
                    "{pane} is tagged for {}; nothing was changed",
                    if other.is_empty() {
                        "nobody (an empty tag)"
                    } else {
                        other
                    }
                )));
            }
        };
        // A clear that did not finish waits for its own proof: an adopt of
        // the agent's pane answers the unchanged state.
        if existing
            .is_some_and(|agent| agent.pane.is_some() && agent.status == AgentStatus::Clearing)
        {
            return Ok(AdoptResult {
                agent_id: id.clone(),
                state: AgentStatus::Clearing,
            });
        }
        let role = state.agent(id).and_then(|agent| agent.role.as_ref());
        let excluded = existing.and_then(|agent| agent.excluded_session_id.clone());
        let candidates = |rows: &[PaneSession]| -> Result<Option<SessionId>, AgentError> {
            let mut eligible = rows.iter().filter(|row| {
                row.view.parent_session_id.is_none()
                    && !row.view.id.is_pending()
                    && excluded.as_ref() != Some(&row.view.id)
                    && role.is_none_or(|role| row.view.harness == role.config.harness.short_tag())
            });
            let first = eligible.next().map(|row| row.view.id.clone());
            if eligible.next().is_some() {
                return Err(AgentError::Refused(format!(
                    "{pane} has more than one session; atm cannot tell which is the agent's"
                )));
            }
            Ok(first)
        };
        // Before the tag: an ambiguous pane is refused with nothing changed.
        let rows = self.pane_sessions(pane).await?;
        let session = candidates(&rows)?;
        // So is a pane whose sessions are all of another harness than the
        // role's: none of them could ever be bound to this agent.
        if let Some(wanted) = role.map(|role| role.config.harness.short_tag()) {
            let found: Vec<&str> = rows
                .iter()
                .filter(|row| {
                    row.view.parent_session_id.is_none() && excluded.as_ref() != Some(&row.view.id)
                })
                .map(|row| row.view.harness.as_str())
                .collect();
            if let Some(other) = found.first().filter(|_| !found.contains(&wanted)) {
                return Err(AgentError::Refused(format!(
                    "{pane} runs {other}, but agent {id}'s role names {wanted}; nothing was changed"
                )));
            }
        }
        // Every read and capture comes before the tag, so that once the pane
        // carries it only the organisation's writes can still fail.
        let harness = role.map(|role| role.config.harness).or_else(|| {
            let mut parents = rows
                .iter()
                .filter(|row| row.view.parent_session_id.is_none())
                .map(|row| row.view.harness.as_str());
            let first = parents.next()?;
            parents
                .all(|harness| harness == first)
                .then(|| atm_core::find_harness_definition(first))
                .flatten()
                .map(|definition| definition.harness)
        });
        // A pending row counts as a live harness process; a child or excluded row does not.
        let live = |harness: Harness| {
            rows.iter().any(|row| {
                row.view.parent_session_id.is_none()
                    && excluded.as_ref() != Some(&row.view.id)
                    && row.view.harness == harness.short_tag()
            })
        };
        let seen = match harness.and_then(|harness| agent_screen(harness).zip(Some(harness))) {
            Some((screen, harness)) if live(harness) => {
                self.adopt_screen(r, pane, screen, found.local_agent_tag.as_deref())
                    .await?
            }
            _ => None,
        };
        let (baseline, safe) = match seen {
            Some((baseline, safe)) => (Some(baseline), safe),
            None => (None, false),
        };
        if !tagged {
            effect.store(true, Ordering::SeqCst);
            changed(effect, r.tag(pane, ExpectedTag::Absent).await)?;
        }
        if let Some(recorded) = existing.filter(|agent| agent.pane.is_some() && !safe) {
            // Already this agent's pane, and nothing new was established: keep
            // what is recorded (a binding included) instead of unbinding it.
            if let Some(baseline) = baseline {
                self.record_baseline(id.clone(), pane.as_str(), baseline);
            }
            return Ok(AdoptResult {
                agent_id: id.clone(),
                state: recorded.status,
            });
        }
        let session = session.filter(|_| safe);
        let bound = session.is_some();
        // A Ready or Starting agent that is adopted again on its own pane, with
        // no session to bind, keeps its record: an Adopt without a session
        // would drop a binding whose row is not announced yet (after a daemon
        // restart every row is a discovery row until the harness's next event).
        let kept = existing
            .filter(|agent| agent.pane.is_some() && !bound)
            .map(|agent| agent.status)
            .filter(|status| matches!(status, AgentStatus::Ready | AgentStatus::Starting));
        if kept.is_none() {
            self.org
                .execute(
                    OrgCommand::Adopt {
                        agent_id: id.clone(),
                        pane: pane.to_string(),
                        session_id: session,
                    },
                    chrono::Utc::now(),
                )
                .await?;
        }
        // Only now is the agent recorded on this pane: a baseline is never
        // kept for a pane the organisation does not give the agent.
        if let Some(baseline) = baseline {
            self.record_baseline(id.clone(), pane.as_str(), baseline);
        }
        if safe && !bound && kept != Some(AgentStatus::Ready) {
            // A harness that announces its session with the first prompt.
            self.org
                .execute(
                    OrgCommand::EditorReady {
                        agent_id: id.clone(),
                    },
                    chrono::Utc::now(),
                )
                .await?;
        }
        Ok(AdoptResult {
            agent_id: id.clone(),
            state: if safe {
                AgentStatus::Ready
            } else {
                AgentStatus::Starting
            },
        })
    }

    /// Establish what a pane to adopt runs, and whether it shows a safe idle
    /// editor while it still carries `tag`, the tag the adopt's inspection
    /// found (none before a first adopt). `None` when no harness can be
    /// pinned. The process facts are read between two captures. Nothing is
    /// recorded here: the adopt records the baseline once the pane is the
    /// agent's.
    async fn adopt_screen(
        &self,
        r: &Reservation,
        pane: &PaneId,
        screen: &'static AgentScreen,
        tag: Option<&str>,
    ) -> Result<Option<(PaneBaseline, bool)>, AgentError> {
        let format = screen.capture_format();
        let first = r.capture(pane, format).await?.inspection;
        let Some((root, command)) = first.pane_root_pid().zip(first.foreground_command) else {
            return Ok(None);
        };
        let Some(facts) = (self.proc)(root).await else {
            return Ok(None);
        };
        let second = r.capture(pane, format).await?;
        Ok(
            pin(screen, (root, &command), &facts, &second).map(|(baseline, control)| {
                let safe = safe_editor(&control)
                    && !second.inspection.dead
                    && !second.inspection.in_mode
                    && second.inspection.local_agent_tag.as_deref() == tag;
                (baseline, safe)
            }),
        )
    }

    /// Cleanup of an adopt: if it tagged the pane and the agent is not
    /// recorded on it, the tag has no record behind it. Quarantine.
    pub(super) async fn settle_adopt(
        &self,
        id: &AgentId,
        pane: &PaneId,
        effect: &Effect,
    ) -> Result<(), AgentError> {
        let state = self.org.snapshot().await?;
        let recorded = state.agent(id).is_some_and(|agent| {
            agent.status != AgentStatus::Down && agent.pane.as_deref() == Some(pane.as_str())
        });
        if effect.load(Ordering::SeqCst) && !recorded {
            return Err(AgentError::Internal(format!(
                "{pane} was tagged for agent {id} but the adoption is not recorded"
            )));
        }
        Ok(())
    }
}

/// The adopt rule: what a pane that already runs something is pinned to, and
/// the adapter's read of its screen. `seen` is the root process and foreground
/// command of a look at the pane taken before `facts` were read, and `capture`
/// was taken after: the reads are not atomic, so the facts count only when
/// both looks show the same root process and command. `None` pins nothing: the
/// looks differ, the process shape places no harness, the command is the
/// shell's own, or the adapter does not recognise the screen.
fn pin(
    screen: &AgentScreen,
    seen: (u32, &str),
    facts: &ProcFacts,
    capture: &PaneCapture,
) -> Option<(PaneBaseline, ControlScreen)> {
    let (root, command) = seen;
    let after = &capture.inspection;
    if after.pane_root_pid() != Some(root) || after.foreground_command.as_deref() != Some(command) {
        return None;
    }
    let baseline = adopt_baseline(facts, root, command)?;
    let (command, control) = screen.start_candidate(capture, &baseline)?;
    if control.dialog == ControlDialog::Unknown {
        return None;
    }
    Some((baseline.with_harness(command)?, control))
}

/// Why a clear has not completed. `None` until the flow commits to clearing;
/// from then on the flow keeps it current, so that the cleanup can say why
/// when the reservation's deadline, or an error, stops the flow.
pub(super) type Waiting = Arc<std::sync::Mutex<Unfinished>>;

#[derive(Debug, Default)]
pub(super) struct Unfinished {
    reason: Option<String>,
    /// Keys the flow sent that nothing has shown the harness to have taken.
    /// The cleanup adds it to the reason, whatever the flow waited for last.
    unanswered: Option<String>,
}

fn waiting_for(waiting: &Waiting, reason: impl Into<String>) {
    if let Ok(mut waiting) = waiting.lock() {
        waiting.reason = Some(reason.into());
    }
}

fn unanswered(waiting: &Waiting, sent: Option<String>) {
    if let Ok(mut waiting) = waiting.lock() {
        waiting.unanswered = sent;
    }
}

/// A flow (`what`: a clear, a start) that stops by its own decision: the
/// reason goes to the caller and, through `waiting`, into the agent's Stuck
/// record.
fn stopped(waiting: &Waiting, id: &AgentId, what: &str, reason: String) -> AgentError {
    let error = AgentError::Refused(format!("agent {id}: the {what} did not complete: {reason}"));
    waiting_for(waiting, reason);
    error
}

/// A guarded paste or key (`what`) that answered with an error. `Ok` when the
/// flow may look at the pane again: its own guard refused the capture
/// (`changed` says so), or the pane refuses input for now (copy mode). Both
/// sent nothing. Any other error ends the flow: one documented as before
/// effect says that nothing was sent by this call; for the rest, whether the
/// input reached the pane is unknown, so it is never sent again.
fn input_error(
    waiting: &Waiting,
    what: &str,
    changed: &str,
    error: ReservationError,
) -> Result<(), AgentError> {
    let sent_nothing = format!("{what} was refused, and nothing was sent by it ({error})");
    match error {
        ReservationError::ScreenChanged { .. } => waiting_for(waiting, changed),
        ReservationError::PaneInputRefused { .. }
        | ReservationError::Control(crate::control::ControlError::SendRefused { .. }) => {
            waiting_for(waiting, sent_nothing);
        }
        error if before_effect(&error) => {
            waiting_for(waiting, sent_nothing);
            return Err(error.into());
        }
        error => {
            waiting_for(
                waiting,
                format!("{what} failed, and whether it reached the pane is unknown ({error})"),
            );
            return Err(error.into());
        }
    }
    Ok(())
}

impl AgentRuntime {
    /// Clear an agent's conversation with its harness's own command, and
    /// report the agent Ready again only on the proof the harness's policy
    /// names: a new session and then a safe editor, or a fresh safe editor
    /// where a conversation was.
    ///
    /// Before anything is sent the call refuses unless its own capture shows
    /// the agent's tagged pane with its harness in the foreground, nobody
    /// focused on it, and a safe editor. From the Clear it records onwards it
    /// never gives up quietly: every failure, and the deadline, leaves the
    /// agent Clearing for [`Self::settle_clear`] to mark Stuck.
    ///
    /// The command is pasted once and submitted once, each under a guard on
    /// the control actor's own capture. A refused guard sent nothing and is
    /// looked at again; a paste or Enter whose outcome is unknown is never
    /// repeated, and nothing is ever cleaned out of the editor.
    pub(super) async fn clear(
        &self,
        r: &Reservation,
        id: &AgentId,
        reserved: Option<&PaneId>,
        waiting: &Waiting,
    ) -> Result<ClearResult, AgentError> {
        let state = self.org.snapshot().await?;
        let agent = state
            .agent(id)
            .ok_or_else(|| AgentError::NotFound(format!("agent {id} was not found")))?;
        if agent.status != AgentStatus::Ready {
            return Err(AgentError::Refused(format!(
                "agent {id} is {}; only a ready agent can be cleared",
                format!("{:?}", agent.status).to_lowercase()
            )));
        }
        let pane = agent.pane.as_ref().map(PaneId::parse).transpose()?;
        if pane.as_ref() != reserved {
            return Err(AgentError::Conflict(format!(
                "agent {id} changed its pane while this call waited; try again"
            )));
        }
        let pane = pane.ok_or_else(|| AgentError::Refused(format!("agent {id} has no pane")))?;
        // Subscribed before the rows are read: a session that arrives from
        // here on is an event, and the rows below are what was there before.
        let mut events = self.registry.subscribe();
        let rows: Vec<SessionView> = self
            .pane_sessions(&pane)
            .await?
            .into_iter()
            .map(|row| row.view)
            .collect();
        let harness = harness(&state, agent, &rows);
        let screen = agent_screen(harness).ok_or_else(|| {
            AgentError::Refused(format!(
                "agent {id}: no screen reader for harness {}",
                harness.short_tag()
            ))
        })?;
        let (policy, format, tag) = (screen.policy(), screen.capture_format(), id.to_string());
        let baseline = self.baseline(id, pane.as_str());
        let own: Vec<&SessionView> = rows
            .iter()
            .filter(|row| {
                matches_agent(agent, row)
                    && !row.id.is_pending()
                    && row.harness == harness.short_tag()
            })
            .collect();
        if own.len() > 1 {
            return Err(AgentError::Refused(format!(
                "{pane} has more than one session; atm cannot tell which is the agent's"
            )));
        }
        // No session the pane or the agent has now can be the clear's result.
        let mut before: Vec<SessionId> = rows
            .iter()
            .map(|row| row.id.clone())
            .chain(agent.session_id.clone())
            .chain(agent.excluded_session_id.clone())
            .collect();
        let refused = |reason: &str| {
            AgentError::Refused(format!(
                "agent {id} was not cleared: {reason}; nothing was sent to {pane}"
            ))
        };
        let capture = r.capture(&pane, format).await?;
        if let Some(reason) = input_refusal(screen, &capture, &baseline, &tag) {
            return Err(refused(reason));
        }
        let control = screen.control(&capture, &baseline);
        if clear_noop(policy, agent.session_id.is_some(), own.len(), &control) {
            return Ok(ClearResult {
                agent_id: id.clone(),
                session_id: None,
            });
        }
        if let Some(reason) = clear_refusal(policy, &control) {
            return Err(refused(reason));
        }
        if let Some(row) = own.first().filter(|_| agent.session_id.is_none()) {
            // The one session on the pane is the conversation being cleared:
            // bound, the Clear excludes it and it is never taken for a new one.
            self.org
                .execute(
                    OrgCommand::SessionBound {
                        agent_id: id.clone(),
                        session_id: row.id.clone(),
                    },
                    chrono::Utc::now(),
                )
                .await?;
        }
        waiting_for(waiting, "the clear command was not typed");
        self.org
            .execute(
                OrgCommand::Clear {
                    agent_id: id.clone(),
                },
                chrono::Utc::now(),
            )
            .await?;

        let command = policy.clear_command;
        loop {
            let (baseline, tag) = (baseline.clone(), tag.clone());
            let guard = move |capture: &_| safe_to_paste(screen, capture, &baseline, &tag);
            match r
                .paste_guarded(&pane, id.clone(), command.to_owned(), format, guard)
                .await
            {
                Ok(()) => break,
                Err(error) => input_error(
                    waiting,
                    "typing the clear command",
                    "the editor stopped being safe before the clear command was typed",
                    error,
                )?,
            }
        }
        waiting_for(waiting, "the clear command did not appear in the editor");
        loop {
            let capture = r.capture(&pane, format).await?;
            match submit_step(screen, &capture, &baseline, &tag, command) {
                Step::Key { key, .. } => {
                    let (baseline, tag) = (baseline.clone(), tag.clone());
                    let guard = move |capture: &_| {
                        submit_allowed(screen, capture, &baseline, &tag, command)
                    };
                    // A session that is on the pane before the Enter is not
                    // its result (somebody cleared by hand meanwhile). Read
                    // before the key, not after: the real one can beat that.
                    before.extend(
                        self.pane_sessions(&pane)
                            .await?
                            .into_iter()
                            .map(|row| row.view.id),
                    );
                    // Said before the key: a deadline while the key is out
                    // must not report the step before it.
                    waiting_for(
                        waiting,
                        "the clear command was being submitted and may have reached the harness",
                    );
                    #[cfg(test)]
                    self.progress("clear_submitting", Some(id.clone()));
                    match r.keys_guarded(&pane, id.clone(), key, format, guard).await {
                        Ok(()) => break,
                        Err(error) => input_error(
                            waiting,
                            "submitting the clear command",
                            "the screen changed before the clear command was submitted",
                            error,
                        )?,
                    }
                }
                Step::Wait(reason) => waiting_for(
                    waiting,
                    format!("the clear command was typed and not submitted: {reason}"),
                ),
                Step::Stuck(reason) => return Err(stopped(waiting, id, "clear", reason)),
                Step::Ready(_) => {
                    return Err(AgentError::Internal("a submit step reported ready".into()))
                }
            }
        }

        waiting_for(waiting, "the harness has not confirmed the clear");
        let announces = matches!(policy.clear_proof, ClearProof::ChangedSession { .. });
        let mut progress = ClearProgress::default();
        loop {
            let mut session = None;
            if announces {
                let rows = self.pane_sessions(&pane).await?;
                session = cleared_session(policy, harness.short_tag(), &rows, &before)
                    .map_err(|reason| stopped(waiting, id, "clear", reason))?;
                if session.is_none() {
                    waiting_for(waiting, "the harness has not announced a new session");
                    #[cfg(test)]
                    self.progress("clear_awaiting_session", Some(id.clone()));
                    // The session arrives as a registry event, or not at all.
                    if let Err(tokio::sync::broadcast::error::RecvError::Closed) =
                        events.recv().await
                    {
                        return Err(AgentError::Unavailable("session registry stopped".into()));
                    }
                    continue;
                }
            }
            let capture = r.capture(&pane, format).await?;
            let control = screen.control(&capture, &baseline);
            match decide_clear(policy, &control, &progress, session.as_ref()) {
                Step::Ready(ready) => {
                    let session_id = match ready {
                        Ready::Session(session) => Some(session),
                        Ready::Editor => None,
                    };
                    let command = match session_id.clone() {
                        Some(session_id) => OrgCommand::SessionBound {
                            agent_id: id.clone(),
                            session_id,
                        },
                        None => OrgCommand::EditorReady {
                            agent_id: id.clone(),
                        },
                    };
                    self.org.execute(command, chrono::Utc::now()).await?;
                    return Ok(ClearResult {
                        agent_id: id.clone(),
                        session_id,
                    });
                }
                Step::Key { key, expect, .. } => {
                    let (baseline, tag) = (baseline.clone(), tag.clone());
                    let guard = move |capture: &_| {
                        input_refusal(screen, capture, &baseline, &tag).is_none()
                            && screen.control(capture, &baseline).dialog == expect
                    };
                    match r.keys_guarded(&pane, id.clone(), key, format, guard).await {
                        Ok(()) => progress.worktree_kept = true,
                        Err(error) => input_error(
                            waiting,
                            "answering the worktree choice",
                            "the worktree choice changed before it was answered",
                            error,
                        )?,
                    }
                }
                Step::Wait(reason) => waiting_for(waiting, reason),
                Step::Stuck(reason) => return Err(stopped(waiting, id, "clear", reason)),
            }
        }
    }

    /// The cleanup of a clear. The snapshot is read after the flow has
    /// stopped, so it accounts for a Clear whose reply was lost. An agent the
    /// flow left Clearing is marked Stuck with the last thing the flow waited
    /// for, and its owner is told. If that can't be recorded the reservation
    /// is quarantined: nothing may type into a pane whose clear is unsettled.
    pub(super) async fn settle_clear(
        &self,
        id: &AgentId,
        waiting: &Waiting,
    ) -> Result<(), AgentError> {
        self.settle(id, waiting, AgentStatus::Clearing, "clear")
            .await
    }

    /// The cleanup of a start, as [`Self::settle_clear`]: an agent the flow
    /// left Starting after it recorded its Up is marked Stuck, and its owner
    /// is told. A start that refused before its Up committed to nothing.
    pub(super) async fn settle_up(
        &self,
        id: &AgentId,
        waiting: &Waiting,
    ) -> Result<(), AgentError> {
        self.settle(id, waiting, AgentStatus::Starting, "start")
            .await
    }

    async fn settle(
        &self,
        id: &AgentId,
        waiting: &Waiting,
        unfinished: AgentStatus,
        what: &str,
    ) -> Result<(), AgentError> {
        let state = self.org.snapshot().await?;
        let committed = waiting.lock().map_or_else(
            |_| Some("its progress was lost".into()),
            |w| {
                let sent = w.unanswered.iter().map(|sent| format!("; {sent}"));
                w.reason
                    .clone()
                    .map(|reason| reason + &sent.collect::<String>())
            },
        );
        let (Some(waiting), Some(agent)) = (
            committed,
            state.agent(id).filter(|agent| agent.status == unfinished),
        ) else {
            return Ok(());
        };
        let reason = format!(
            "{what} did not complete: {waiting}; inspect {}",
            agent.pane.as_deref().unwrap_or("its pane")
        );
        self.org
            .execute(
                OrgCommand::Stuck {
                    agent_id: id.clone(),
                    reason: reason.clone(),
                },
                chrono::Utc::now(),
            )
            .await?;
        let note = atm_org::MessageContent {
            from: atm_org::Address::System,
            to: atm_org::Address::Owner,
            kind: atm_org::MessageKind::Note,
            body: format!("agent {id}: {reason}"),
            attachments: Vec::new(),
            task: None,
        };
        self.org
            .execute(OrgCommand::SendMessage(note), chrono::Utc::now())
            .await?;
        Ok(())
    }
}

/// Everything a start needs that is settled before its reservation: the role
/// as read from the project, the command to type, and where to split.
#[derive(Debug)]
pub(super) struct Launch {
    pub prepared: PreparedAgent,
    command: LaunchCommand,
    context: SpawnContext,
    direction: atm_tmux::PaneDirection,
    /// The agent whose pane the role's `pane` hint names, if it has one.
    beside: Option<AgentId>,
}

impl AgentRuntime {
    /// Resolve what `agent.up` types and where it splits, with no effect on
    /// any pane. A launch text that can't be typed safely, a `pane` hint that
    /// can't be read and a hint that names an agent without a pane refuse here.
    pub(super) async fn prepare_launch(
        &self,
        prepared: PreparedAgent,
        caller: CallContext,
        server: Option<String>,
    ) -> Result<Launch, AgentError> {
        let command = launch_command(&prepared, |key| (self.spawn_env)(key))?;
        let role = &prepared.role;
        let (direction, beside) = match role.config.pane.as_deref().map(placement) {
            None => (atm_tmux::PaneDirection::Below, None),
            Some(Some((direction, name))) => {
                let beside = AgentId::new(prepared.agent_id.project.clone(), name)
                    .map_err(|error| AgentError::Refused(format!("role {}: {error}", role.name)))?;
                (direction, Some(beside))
            }
            Some(None) => {
                return Err(AgentError::Refused(format!(
                    "role {}: its pane hint is not `<left|right|above|below>:<agent name>`",
                    role.name
                )))
            }
        };
        let target = match &beside {
            None => None,
            Some(beside) => Some(
                self.org
                    .snapshot()
                    .await?
                    .agent(beside)
                    .filter(|agent| agent.status != AgentStatus::Down)
                    .and_then(|agent| agent.pane.clone())
                    .ok_or_else(|| {
                        AgentError::Refused(format!(
                            "agent {beside}, which role {} is placed beside, has no pane",
                            role.name
                        ))
                    })?,
            ),
        };
        let context = self
            .control
            .clone()
            .for_server(server)
            .prepare_spawn(
                target,
                prepared.cwd.to_str().map(str::to_owned),
                caller.from_pane.map(|pane| pane.0),
                None,
            )
            .await
            .map_err(|error| match error {
                ServiceError::Timeout => AgentError::Timeout,
                ServiceError::ServerRefused(reason) => AgentError::Refused(reason),
                error => AgentError::Unavailable(format!("finding a pane to split: {error}")),
            })?
            .with_detached(true);
        Ok(Launch {
            prepared,
            command,
            context,
            direction,
            beside,
        })
    }

    /// Start an agent: split a new pane off without selecting it, tag it,
    /// type the launch command into its shell and run it, then wait for the
    /// harness to be ready: its new session and then a safe editor, or a fresh
    /// safe editor for a harness that announces its session later.
    ///
    /// Nothing is typed until the new pane's root is proven an idle shell
    /// whose line can be read exactly, and the command is run only when the
    /// line reads that prompt followed by the command and nothing else. The
    /// command is pasted once and run once, each under a guard on the control
    /// actor's own capture; an input whose outcome is unknown is never
    /// repeated. A dialog the harness opens at its start is answered only
    /// where the harness's policy and the role allow it (a trust dialog for
    /// one of the role's trust directories, a prompt the launch may skip),
    /// each key under a guard on that same dialog, and audited. From the Up it
    /// records onwards the flow never gives up quietly: every failure, and the deadline, leaves the agent Starting for
    /// [`Self::settle_up`] to mark Stuck.
    pub(super) async fn up(
        &self,
        r: &Reservation,
        launch: Launch,
        waiting: &Waiting,
    ) -> Result<UpResult, AgentError> {
        let Launch {
            prepared,
            command,
            context,
            direction,
            beside,
        } = launch;
        let id = prepared.agent_id.clone();
        let harness = prepared.role.config.harness;
        let screen = agent_screen(harness).ok_or_else(|| {
            AgentError::Refused(format!(
                "agent {id}: no screen reader for harness {}",
                harness.short_tag()
            ))
        })?;
        let (policy, format, tag) = (screen.policy(), screen.capture_format(), id.to_string());
        // The pane to split: read only, it grants no input.
        let target = PaneId::parse(context.target_pane.clone())?;
        let refused = |reason: String| {
            AgentError::Refused(format!(
                "agent {id} was not started: {reason}; nothing was changed"
            ))
        };
        let found = r
            .inspect(&target)
            .await?
            .filter(|found| !found.dead)
            .ok_or_else(|| refused(format!("the pane to split, {target}, is gone")))?;
        if let Some(beside) = &beside {
            if found.local_agent_tag.as_deref() != Some(&beside.to_string()) {
                return Err(refused(format!(
                    "{target} is no longer agent {beside}'s pane"
                )));
            }
        }
        // The organisation refuses an agent that is already up.
        self.org
            .execute(
                OrgCommand::Up {
                    agent_id: id.clone(),
                    role: prepared.role.clone(),
                },
                chrono::Utc::now(),
            )
            .await?;
        waiting_for(waiting, "its pane was not created");
        let pane = r.split(context, direction, "50%".into()).await?;
        waiting_for(waiting, format!("{pane} was created and not recorded"));
        r.tag(&pane, ExpectedTag::Absent).await?;
        self.org
            .execute(
                OrgCommand::PaneCreated {
                    agent_id: id.clone(),
                    pane: pane.to_string(),
                },
                chrono::Utc::now(),
            )
            .await?;

        waiting_for(waiting, "the shell in the new pane is not at its prompt");
        let text = command.text;
        let mut refused_line = None;
        let (mut baseline, before) = loop {
            let (baseline, before) = self
                .idle_shell(r, &id, &pane, screen, refused_line.as_deref(), waiting)
                .await?;
            let guard = {
                let (baseline, tag, before) = (baseline.clone(), tag.clone(), before.clone());
                move |capture: &_| launch_paste_allowed(screen, capture, &baseline, &tag, &before)
            };
            #[cfg(test)]
            self.progress("up_pasting", Some(id.clone()));
            match r
                .paste_guarded(&pane, id.clone(), text.clone(), format, guard)
                .await
            {
                Ok(()) => break (baseline, before),
                // Nothing was sent: read the shell's line again.
                Err(error) => {
                    input_error(
                        waiting,
                        "typing the launch command",
                        "the shell's line changed before the launch command was typed",
                        error,
                    )?;
                    refused_line = Some(before);
                }
            }
        };
        // Subscribed before the command runs: a session that arrives from
        // here on is an event.
        let mut events = self.registry.subscribe();
        waiting_for(
            waiting,
            "the launch command did not appear at the shell's prompt",
        );
        loop {
            let capture = r.capture(&pane, format).await?;
            let reason = match shell_refusal(screen, &capture, &baseline, &tag) {
                Some(reason) => reason,
                None if launch_line_is_staged(&capture, &before, &text) => {
                    let guard = {
                        let (baseline, tag) = (baseline.clone(), tag.clone());
                        let (before, text) = (before.clone(), text.clone());
                        move |capture: &_| {
                            launch_enter_allowed(screen, capture, &baseline, &tag, (&before, &text))
                        }
                    };
                    waiting_for(
                        waiting,
                        "the launch command was being run and may have started the harness",
                    );
                    #[cfg(test)]
                    self.progress("up_running", Some(id.clone()));
                    match r
                        .keys_guarded(&pane, id.clone(), PaneKey::Enter, format, guard)
                        .await
                    {
                        Ok(()) => break,
                        Err(error) => {
                            input_error(
                                waiting,
                                "running the launch command",
                                "the launch command was typed and not run: the shell's line changed before the command was run",
                                error,
                            )?;
                            continue;
                        }
                    }
                }
                None => "the shell's line is not its prompt followed by the command",
            };
            waiting_for(
                waiting,
                format!("the launch command was typed and not run: {reason}"),
            );
        }
        waiting_for(waiting, "the launch has started no harness");
        self.org
            .execute(
                OrgCommand::ControlSpawned(atm_org::ControlAudit {
                    pane: pane.to_string(),
                    action: serde_json::json!({
                        "method": "agent.up",
                        "agent": tag,
                        "binary": command.binary,
                    }),
                }),
                chrono::Utc::now(),
            )
            .await?;

        let announces = policy.session_arrival == SessionArrival::AtStart;
        let mut progress = StartProgress::default();
        // The key that last accepted a trust dialog, for its audit, and every
        // key sent to that dialog since it was last seen accepted.
        let mut accepted_with = None;
        let mut trust_keys: Vec<String> = Vec::new();
        let audit = |dialog: &str, key: PaneKey, folder: Option<String>| {
            let mut action = serde_json::json!({
                "method": "agent.up",
                "agent": tag,
                "dialog": dialog,
                "key": format!("{key:?}"),
            });
            if let Some((action, folder)) = action.as_object_mut().zip(folder) {
                action.insert("folder".into(), folder.into());
            }
            self.org.execute(
                OrgCommand::ControlSent(atm_org::ControlAudit {
                    pane: pane.to_string(),
                    action,
                }),
                chrono::Utc::now(),
            )
        };
        loop {
            let mut session = None;
            if announces {
                let rows = self.pane_sessions(&pane).await?;
                session = started_session(harness.short_tag(), &rows)
                    .map_err(|reason| stopped(waiting, &id, "start", reason))?;
            }
            let capture = r.capture(&pane, format).await?;
            let control = match start_view(screen, &capture, &baseline) {
                StartView::Wait(reason) => {
                    waiting_for(waiting, reason);
                    continue;
                }
                StartView::Stuck(reason) => return Err(stopped(waiting, &id, "start", reason)),
                StartView::Pin {
                    baseline: pinned,
                    control,
                } => {
                    self.record_baseline(id.clone(), pane.as_str(), pinned.clone());
                    baseline = pinned;
                    control
                }
                StartView::Harness(control) => control,
            };
            // The first screen after an accept that is not the trust dialog:
            // the harness took the answer.
            if let Some((folder, key)) = progress.trust_accepted(&control).zip(accepted_with) {
                audit("folder_trust", key, Some(folder)).await?;
                trust_keys.clear();
                unanswered(waiting, None);
            }
            match decide_start(
                policy,
                &control,
                &progress,
                &prepared.trust_dirs,
                session.as_ref(),
            ) {
                Step::Ready(ready) => {
                    let command = match ready {
                        Ready::Session(session_id) => OrgCommand::SessionBound {
                            agent_id: id.clone(),
                            session_id,
                        },
                        Ready::Editor => OrgCommand::EditorReady {
                            agent_id: id.clone(),
                        },
                    };
                    self.org.execute(command, chrono::Utc::now()).await?;
                    return Ok(UpResult {
                        agent_id: id,
                        pane: pane.to_string(),
                        state: AgentStatus::Ready,
                    });
                }
                Step::Wait(reason) => {
                    waiting_for(waiting, reason);
                    if announces && session.is_none() && safe_editor(&control) {
                        #[cfg(test)]
                        self.progress("up_awaiting_session", Some(id.clone()));
                        // The session arrives as a registry event, or not at all.
                        if let Err(tokio::sync::broadcast::error::RecvError::Closed) =
                            events.recv().await
                        {
                            return Err(AgentError::Unavailable("session registry stopped".into()));
                        }
                    }
                }
                Step::Key {
                    key,
                    expect,
                    action,
                } => {
                    // A dialog's key waits while the pane takes no input from
                    // atm (somebody is focused on it): the guard is not asked.
                    if let Some(reason) = input_refusal(screen, &capture, &baseline, &tag) {
                        waiting_for(waiting, reason);
                        continue;
                    }
                    let guard = {
                        let (baseline, tag, expect) =
                            (baseline.clone(), tag.clone(), expect.clone());
                        move |capture: &_| {
                            input_refusal(screen, capture, &baseline, &tag).is_none()
                                && screen.control(capture, &baseline).dialog == expect
                        }
                    };
                    // Said before the key, as for the launch's Enter.
                    waiting_for(
                        waiting,
                        "the harness's start dialog was being answered and the key may have reached it",
                    );
                    #[cfg(test)]
                    self.progress("up_answering", Some(id.clone()));
                    match r.keys_guarded(&pane, id.clone(), key, format, guard).await {
                        Ok(()) => {
                            progress.sent(action, &expect);
                            if let ControlDialog::FolderTrust { folder, .. } = &expect {
                                trust_keys.push(format!("{key:?}"));
                                // A round that is never accepted is not
                                // audited: its Stuck reason says what was sent.
                                unanswered(
                                    waiting,
                                    Some(format!(
                                        "atm sent {} to the trust dialog for {folder} and has not seen it accepted",
                                        trust_keys.join(", ")
                                    )),
                                );
                            }
                            match action {
                                KeyAction::TrustAccept => accepted_with = Some(key),
                                KeyAction::SkipLaunchPrompt => {
                                    audit("launch_skip", key, None).await?;
                                }
                                _ => {}
                            }
                        }
                        Err(error) => input_error(
                            waiting,
                            "answering the harness's start dialog",
                            "the harness's start dialog changed before it was answered",
                            error,
                        )?,
                    }
                }
                Step::Stuck(reason) => return Err(stopped(waiting, &id, "start", reason)),
            }
        }
    }

    /// Wait until the new pane's root is proven a shell at its prompt, and
    /// record that as the pane's baseline. Returns it with the shell's line
    /// before the cursor, which the paste and its Enter are checked against.
    ///
    /// The process facts are read between two captures and used only when both
    /// show the same root process, shell and line, and that line is not blank. A line that is
    /// `refused_line` with more typed after it is never taken.
    async fn idle_shell(
        &self,
        r: &Reservation,
        id: &AgentId,
        pane: &PaneId,
        screen: &'static AgentScreen,
        refused_line: Option<&str>,
        waiting: &Waiting,
    ) -> Result<(PaneBaseline, String), AgentError> {
        let (format, tag) = (screen.capture_format(), id.to_string());
        let seen = |capture: &PaneCapture| {
            capture
                .inspection
                .pane_root_pid()
                .zip(capture.inspection.foreground_command.clone())
        };
        loop {
            let first = r.capture(pane, format).await?;
            let Some((root, command)) = seen(&first) else {
                continue;
            };
            let Some(baseline) = (self.proc)(root)
                .await
                .and_then(|facts| shell_baseline(&facts, root, &command))
            else {
                continue;
            };
            // The second capture must show that root and shell again.
            let second = r.capture(pane, format).await?;
            if let Some(reason) = shell_refusal(screen, &second, &baseline, &tag) {
                waiting_for(waiting, reason);
                continue;
            }
            match launch_line(&second) {
                // A blank line is a shell that has drawn no prompt yet (an idle
                // shell can still be reading its startup files).
                Some(line)
                    if !line.trim().is_empty()
                        && launch_line(&first).as_ref() == Some(&line)
                        && prompt_line_allowed(refused_line, &line) =>
                {
                    self.record_baseline(id.clone(), pane.as_str(), baseline.clone());
                    return Ok((baseline, line));
                }
                _ => waiting_for(waiting, "the shell's prompt could not be read as settled"),
            }
        }
    }
}
