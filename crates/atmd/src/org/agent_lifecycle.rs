//! The effects of the named-agent lifecycle flows.
//!
//! Each flow runs inside one reservation of its agent: it reads the committed
//! organisation state again, performs its pane effects through the
//! reservation, and reports through the organisation log. What a flow decides
//! from a screen lives in `agent_flow`, which is pure.

use super::agent_flow::{
    adopt_baseline, clear_noop, clear_refusal, cleared_session, decide_clear, input_refusal,
    safe_to_paste, submit_allowed, submit_step, ClearProgress, Ready, Step,
};
use super::agent_screen::{agent_screen, AgentScreen};
use super::agents::{
    harness, matches_agent, AdoptResult, AgentError, AgentRuntime, ClearResult, DownResult,
};
use crate::control::reservation::{ExpectedTag, PaneId, Reservation, ReservationError};
use crate::registry::PaneSession;
use atm_core::harness_control::{ClearProof, ControlDialog};
use atm_core::{DialogState, Harness, InputState, SessionId, SessionView, TurnState};
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

/// Why a clear has not completed. `None` until the flow commits to clearing;
/// from then on the flow keeps it current, so that the cleanup can say why
/// when the reservation's deadline, or an error, stops the flow.
pub(super) type Waiting = Arc<std::sync::Mutex<Option<String>>>;

fn waiting_for(waiting: &Waiting, reason: impl Into<String>) {
    if let Ok(mut waiting) = waiting.lock() {
        *waiting = Some(reason.into());
    }
}

/// A clear that stops by its own decision: the reason goes to the caller and,
/// through `waiting`, into the agent's Stuck record.
fn incomplete(waiting: &Waiting, id: &AgentId, reason: String) -> AgentError {
    let error = AgentError::Refused(format!("agent {id}: the clear did not complete: {reason}"));
    waiting_for(waiting, reason);
    error
}

/// An input whose reply was an error other than a refused guard: whether it
/// reached the pane is unknown, so it is never sent again.
fn unknown_outcome(waiting: &Waiting, what: &str, error: ReservationError) -> AgentError {
    waiting_for(
        waiting,
        format!("{what} failed, and whether it reached the pane is unknown ({error})"),
    );
    error.into()
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
        let before: Vec<SessionId> = rows
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
                // Nothing was sent: look again.
                Err(ReservationError::ScreenChanged { .. }) => waiting_for(
                    waiting,
                    "the editor stopped being safe before the clear command was typed",
                ),
                Err(error) => {
                    return Err(unknown_outcome(waiting, "typing the clear command", error))
                }
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
                    #[cfg(test)]
                    self.progress("clear_submitting", Some(id.clone()));
                    match r.keys_guarded(&pane, id.clone(), key, format, guard).await {
                        Ok(()) => break,
                        Err(ReservationError::ScreenChanged { .. }) => waiting_for(
                            waiting,
                            "the screen changed before the clear command was submitted",
                        ),
                        Err(error) => {
                            return Err(unknown_outcome(
                                waiting,
                                "submitting the clear command",
                                error,
                            ))
                        }
                    }
                }
                Step::Wait(reason) => waiting_for(
                    waiting,
                    format!("the clear command was typed and not submitted: {reason}"),
                ),
                Step::Stuck(reason) => return Err(incomplete(waiting, id, reason)),
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
                    .map_err(|reason| incomplete(waiting, id, reason))?;
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
                        Err(ReservationError::ScreenChanged { .. }) => waiting_for(
                            waiting,
                            "the worktree choice changed before it was answered",
                        ),
                        Err(error) => {
                            return Err(unknown_outcome(
                                waiting,
                                "answering the worktree choice",
                                error,
                            ))
                        }
                    }
                }
                Step::Wait(reason) => waiting_for(waiting, reason),
                Step::Stuck(reason) => return Err(incomplete(waiting, id, reason)),
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
        let state = self.org.snapshot().await?;
        let committed = waiting
            .lock()
            .map_or_else(|_| Some("its progress was lost".into()), |w| w.clone());
        let (Some(waiting), Some(agent)) = (
            committed,
            state
                .agent(id)
                .filter(|agent| agent.status == AgentStatus::Clearing),
        ) else {
            return Ok(());
        };
        let reason = format!(
            "clear did not complete: {waiting}; inspect {}",
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
