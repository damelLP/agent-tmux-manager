//! The effects of the named-agent lifecycle flows.
//!
//! Each flow runs inside one reservation of its agent: it reads the committed
//! organisation state again, performs its pane effects through the
//! reservation, and reports through the organisation log. What a flow decides
//! from a screen lives in `agent_flow`, which is pure.

use super::agent_flow::adopt_baseline;
use super::agent_screen::{agent_screen, AgentScreen};
use super::agents::{AdoptResult, AgentError, AgentRuntime, DownResult};
use crate::control::reservation::{ExpectedTag, PaneId, Reservation};
use crate::registry::PaneSession;
use atm_core::harness_control::ControlDialog;
use atm_core::{DialogState, Harness, InputState, SessionId, TurnState};
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
                Some(_) if kill => {
                    let action = serde_json::json!({
                        "method": "agent.down",
                        "agent": id.to_string(),
                    });
                    // An error may still have killed the pane: account for it.
                    effect.store(true, Ordering::SeqCst);
                    r.kill(pane, id.clone(), action, chrono::Utc::now()).await?;
                    if r.inspect(pane).await?.is_some() {
                        return Err(AgentError::Internal(format!(
                            "{pane} is still there after it was killed"
                        )));
                    }
                    reason = format!("taken down; {pane} killed");
                }
                Some(_) => {
                    effect.store(true, Ordering::SeqCst);
                    r.untag(pane, id.clone()).await?;
                    if r.inspect(pane)
                        .await?
                        .is_some_and(|found| found.local_agent_tag.is_some())
                    {
                        return Err(AgentError::Internal(format!(
                            "{pane} still carries a tag after it was released"
                        )));
                    }
                    reason = format!("taken down; {pane} released with its program running");
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
        candidates(&self.pane_sessions(pane).await?)?;
        if !tagged {
            effect.store(true, Ordering::SeqCst);
            r.tag(pane, ExpectedTag::Absent).await?;
        }
        // Read everything again now that the pane is ours.
        let rows = self.pane_sessions(pane).await?;
        let session = candidates(&rows)?;
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
        let safe = match harness.and_then(|harness| agent_screen(harness).zip(Some(harness))) {
            Some((screen, harness)) if live(harness) => {
                self.adopt_screen(r, id, pane, screen).await?
            }
            _ => false,
        };
        if let Some(recorded) = existing.filter(|agent| agent.pane.is_some() && !safe) {
            // Already this agent's pane, and nothing new was established: keep
            // what is recorded (a binding included) instead of unbinding it.
            return Ok(AdoptResult {
                agent_id: id.clone(),
                state: recorded.status,
            });
        }
        let session = session.filter(|_| safe);
        let bound = session.is_some();
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
        if safe && !bound {
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

    /// Establish what the adopted pane runs, and whether it shows a safe idle
    /// editor. The process facts are read between two captures and used only
    /// when both captures show the same root process and command.
    async fn adopt_screen(
        &self,
        r: &Reservation,
        id: &AgentId,
        pane: &PaneId,
        screen: &'static AgentScreen,
    ) -> Result<bool, AgentError> {
        let format = screen.capture_format();
        let first = r.capture(pane, format).await?;
        let seen = |capture: &crate::control::reservation::PaneCapture| {
            capture
                .inspection
                .pane_root_pid()
                .zip(capture.inspection.foreground_command.clone())
        };
        let Some((root, command)) = seen(&first) else {
            return Ok(false);
        };
        let Some(facts) = (self.proc)(root).await else {
            return Ok(false);
        };
        let second = r.capture(pane, format).await?;
        if seen(&second) != Some((root, command.clone())) {
            return Ok(false);
        }
        let Some(baseline) = adopt_baseline(&facts, root, &command) else {
            return Ok(false);
        };
        let Some((command, control)) = screen.start_candidate(&second, &baseline) else {
            return Ok(false);
        };
        if control.dialog == ControlDialog::Unknown {
            return Ok(false);
        }
        let Some(baseline) = baseline.with_harness(command) else {
            return Ok(false);
        };
        self.record_baseline(id.clone(), pane.as_str(), baseline);
        Ok(control.dialog == ControlDialog::None
            && control.facts.input == InputState::Empty
            && control.facts.turn == TurnState::NotRunning
            && control.facts.dialog == DialogState::None
            && !second.inspection.dead
            && !second.inspection.in_mode
            && second.inspection.local_agent_tag.as_deref() == Some(&id.to_string()))
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
