//! The effects of the named-agent lifecycle flows.
//!
//! Each flow runs inside one reservation of its agent: it reads the committed
//! organisation state again, performs its pane effects through the
//! reservation, and reports through the organisation log. What a flow decides
//! from a screen lives in `agent_flow`, which is pure.
//!
//! # Characterized limits
//!
//! A draft appearing after clear admission can exhaust the work deadline with
//! no input sent. Cleanup still records Stuck; an eligible session wake with a
//! safe editor can recover it without replaying the clear or deleting the draft.
//! The caller's Stuck reason is retained only after its write is acknowledged.
//! If that acknowledgement times out, a late commit can leave Stuck without a
//! note, a timeout answer and a quarantined reservation. Poisoned progress also
//! omits the caller's retained reason; this is not evidence that no effect ran.
//!
//! Shell launch proof samples focus, not selection history. A prefix already
//! present in both initial shell captures can be taken for the prompt, unlike
//! text introduced after that baseline. A private tmux 3.8 shell/client check
//! confirmed that a selected pane can retain input after selection moves away;
//! no real harness launch with such typeahead was checked.

use super::agent_flow::{
    adopt_baseline, clear_noop, clear_refusal, cleared_session, decide_clear, decide_start,
    input_blocker, input_refusal, launch_enter_allowed, launch_line, launch_line_is_staged,
    launch_paste_allowed, placement, safe_editor, safe_to_paste, shell_baseline, shell_refusal,
    start_view, started_session, submit_allowed, submit_step, ClearProgress, InputBlocker,
    KeyAction, ProcFacts, Ready, StartProgress, StartView, Step,
};
use super::agent_launch::{launch_command, LaunchCommand};
use super::agent_project::PreparedAgent;
use super::agent_screen::{agent_screen, AgentScreen, PaneBaseline};
use super::agent_unreadable::{foreground_session, ForegroundSession, ScreenSample};
use super::agents::{
    harness, matches_agent, AdoptResult, AgentError, AgentRuntime, ClearResult, DownResult,
    UpResult,
};
use super::start_line::StartLineDisposition;
use crate::control::reservation::{
    ExpectedTag, PaneCapture, PaneId, PaneKey, Reservation, ReservationError,
};
use crate::control::{service::ServiceError, SpawnContext};
use crate::registry::{PaneSession, SessionEvent};
use crate::server::methods::CallContext;
use atm_core::harness_control::{ClearProof, ControlDialog, ControlScreen, SessionArrival};
use atm_core::{Harness, SessionId, SessionStatus, SessionView};
use atm_org::{Agent, AgentId, AgentStatus, OrgCommand};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use tokio::{
    sync::broadcast,
    time::{timeout, timeout_at, Instant},
};

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
    /// `operation` is the method the kill is audited under.
    pub(super) async fn down(
        &self,
        r: &Reservation,
        id: &AgentId,
        reserved: Option<&PaneId>,
        kill: bool,
        effect: &Effect,
        operation: &str,
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
                            "method": operation,
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
    #[cfg(test)]
    if matches!(error, ReservationError::NotImplemented { .. }) {
        return true;
    }
    matches!(
        error,
        ReservationError::Busy { .. }
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
    pub(super) async fn pane_sessions(
        &self,
        pane: &PaneId,
    ) -> Result<Vec<PaneSession>, AgentError> {
        timeout(REGISTRY_BUDGET, self.registry.pane_sessions(pane.as_str()))
            .await
            .map_err(|_| AgentError::Timeout)?
            .map_err(|error| AgentError::Unavailable(format!("session registry: {error}")))
    }

    /// Take a pane that already runs a harness under this agent's name: tag
    /// it, and record the agent on it. No key is sent and nothing in the pane
    /// is changed but the tag. An agent already Clearing on this pane answers
    /// Clearing unchanged, after checking the pane's tag. A role is refused
    /// before tagging when the pane's non-excluded top-level rows name other
    /// harnesses and none names its own. Discovery rows count as evidence too.
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
        let (state, cursor) = self.org.snapshot_at().await?;
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
            if let Some(other) = other_harness(&rows, excluded.as_ref(), wanted) {
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
        let roots = |harness: Harness| {
            let excluded = excluded.as_ref();
            rows.iter().filter(move |row| {
                row.view.parent_session_id.is_none()
                    && excluded != Some(&row.view.id)
                    && row.view.harness == harness.short_tag()
            })
        };
        let seen = match harness.and_then(|harness| agent_screen(harness).zip(Some(harness))) {
            Some((screen, harness)) if roots(harness).next().is_some() => {
                self.adopt_screen(
                    r,
                    pane,
                    screen,
                    found.local_agent_tag.as_deref(),
                    existing.and_then(|agent| agent.session_id.as_ref()),
                    roots(harness),
                )
                .await?
            }
            _ => None,
        };
        let (baseline, safe, diagnosis) = match seen {
            Some(seen) => (seen.baseline, seen.safe, seen.diagnosis),
            None => (None, false, None),
        };
        let record_screen = |cursor| {
            if let Some((at, read, foreground)) = diagnosis {
                self.screens.record(id, read, at, cursor, foreground);
            }
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
            record_screen(cursor);
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
        let cursor = if kept.is_none() {
            self.org
                .execute(
                    OrgCommand::Adopt {
                        agent_id: id.clone(),
                        pane: pane.to_string(),
                        session_id: session,
                    },
                    chrono::Utc::now(),
                )
                .await?
                .last()
                .map_or(cursor, |event| event.seq)
        } else {
            cursor
        };
        // Only now is the agent recorded on this pane: a baseline is never
        // kept for a pane the organisation does not give the agent.
        if let Some(baseline) = baseline {
            self.record_baseline(id.clone(), pane.as_str(), baseline);
        }
        record_screen(cursor);
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
    async fn adopt_screen<'a>(
        &self,
        r: &Reservation,
        pane: &PaneId,
        screen: &'static AgentScreen,
        tag: Option<&str>,
        binding: Option<&SessionId>,
        rows: impl Iterator<Item = &'a PaneSession>,
    ) -> Result<Option<AdoptScreen>, AgentError> {
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
                let read =
                    ScreenSample::read(screen, &baseline, &second, pane, tag, binding, &control);
                let safe = safe_editor(&control)
                    && !second.inspection.dead
                    && !second.inspection.in_mode
                    && second.inspection.local_agent_tag.as_deref() == tag;
                AdoptScreen {
                    baseline: (control.dialog != ControlDialog::Unknown).then_some(baseline),
                    safe,
                    diagnosis: read.and_then(|sample| {
                        let foreground = if sample.unreadable {
                            Some(foreground_session(rows, &facts, binding)?)
                        } else {
                            None
                        };
                        Some((Instant::now(), sample, foreground))
                    }),
                }
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

/// A role refuses a pane only when its non-excluded top-level rows name
/// other harnesses and none names the wanted one. Discovery rows count too.
pub(super) fn other_harness<'a>(
    rows: &'a [PaneSession],
    excluded: Option<&SessionId>,
    wanted: &str,
) -> Option<&'a str> {
    let harnesses: Vec<&str> = rows
        .iter()
        .filter(|row| row.view.parent_session_id.is_none() && excluded != Some(&row.view.id))
        .map(|row| row.view.harness.as_str())
        .collect();
    harnesses
        .first()
        .copied()
        .filter(|_| !harnesses.contains(&wanted))
}

struct AdoptScreen {
    baseline: Option<PaneBaseline>,
    safe: bool,
    diagnosis: Option<(Instant, ScreenSample, Option<ForegroundSession>)>,
}

/// The adopt rule's candidate pin and the adapter's read of its screen.
/// Only a recognized control screen may install the candidate. `seen` is the root process and foreground
/// command of a look at the pane taken before `facts` were read, and `capture`
/// was taken after: the reads are not atomic, so the facts count only when
/// both looks show the same root process and command. `None` means the
/// looks differ, the process shape places no harness, or the command is the
/// shell's own.
pub(super) fn pin(
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
    Some((baseline.with_harness(command)?, control))
}

/// Why a clear has not completed. `None` until the flow commits to clearing;
/// from then on the flow keeps it current, so that the cleanup can say why
/// when the reservation's deadline, or an error, stops the flow.
pub(super) type Waiting = Arc<std::sync::Mutex<Unfinished>>;

#[derive(Debug, Default)]
pub(super) struct Unfinished {
    reason: Option<String>,
    /// The keys the flow has sent to its harness's dialogs. The cleanup adds
    /// it to the reason, whatever the flow waited for last.
    unanswered: Option<String>,
    /// The audit of an accept whose dialog has not been seen to go. The
    /// cleanup writes it when the flow ends before that was seen.
    unaudited: Option<atm_org::ControlAudit>,
    /// The reason the cleanup recorded the agent Stuck with, once it has.
    stuck: Option<String>,
}

/// The Stuck reason the cleanup of `waiting`'s flow recorded, if it did.
pub(super) fn stuck_record(waiting: &Waiting) -> Option<String> {
    waiting
        .lock()
        .ok()
        .and_then(|waiting| waiting.stuck.clone())
}

/// What the caller of a flow that decided to stop or timed out is told: the
/// Stuck reason that flow's own cleanup recorded (`stuck`), in the words of the
/// owner's note. A timeout without one stays a timeout: it ran out waiting
/// for the reservation, or before the flow committed to anything, and an
/// agent stuck since an earlier call is not this call's outcome. Every other
/// error is the caller's as it is.
pub(super) fn deadline_answer(
    error: AgentError,
    id: &AgentId,
    stuck: Option<String>,
) -> AgentError {
    match (error, stuck) {
        (
            AgentError::Refused(_) | AgentError::Reservation(ReservationError::Timeout),
            Some(reason),
        ) => AgentError::Refused(format!("agent {id}: {reason}")),
        (error, _) => error,
    }
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

fn unaudited(waiting: &Waiting, audit: Option<atm_org::ControlAudit>) {
    if let Ok(mut waiting) = waiting.lock() {
        waiting.unaudited = audit;
    }
}

/// How a reason names the dialog a start's key went to.
fn dialog_name(dialog: &ControlDialog) -> String {
    match dialog {
        ControlDialog::FolderTrust { folder, .. } => format!("the trust dialog for {folder}"),
        ControlDialog::LaunchSkip { .. } => "the update prompt".into(),
        _ => "the harness's start dialog".into(),
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

/// Identity of the editor whose session saved. A handover must not follow a
/// later pane or a newer binding when it takes the clear reservation.
#[derive(Debug, Clone)]
pub(super) struct SavedSeat {
    pub agent: AgentId,
    pub pane: PaneId,
    pub session: SessionId,
}

impl SavedSeat {
    pub(super) fn matches(&self, agent: &Agent) -> bool {
        agent.status == AgentStatus::Ready
            && agent.id == self.agent
            && agent.pane.as_deref() == Some(self.pane.as_str())
            && agent.session_id.as_ref() == Some(&self.session)
    }

    pub(super) fn changed(&self) -> AgentError {
        AgentError::Refused(format!(
            "agent {} no longer has its saved session {} on {}; nothing was sent",
            self.agent, self.session, self.pane
        ))
    }

    pub(super) fn refusal(&self, reason: &str) -> AgentError {
        clear_refused(&self.agent, &self.pane, reason)
    }

    pub(super) fn row_wake(&self, event: &SessionEvent) -> Result<bool, AgentError> {
        match event {
            SessionEvent::Removed { session_id, .. } if *session_id == self.session => {
                Err(self.changed())
            }
            SessionEvent::Updated { session } if session.id == self.session => {
                if session.tmux_pane.as_deref() == Some(self.pane.as_str()) {
                    Ok(true)
                } else {
                    Err(self.changed())
                }
            }
            _ => Ok(false),
        }
    }

    fn candidate(&self, row: &SessionView, harness: Harness) -> bool {
        row.tmux_pane.as_deref() == Some(self.pane.as_str())
            && row.parent_session_id.is_none()
            && !row.id.is_pending()
            && row.harness == harness.short_tag()
    }

    fn pinned_row<'a>(
        &self,
        rows: &'a [PaneSession],
        harness: Harness,
    ) -> Result<&'a SessionView, AgentError> {
        let mut candidates = rows
            .iter()
            .map(|row| &row.view)
            .filter(|row| self.candidate(row, harness));
        match (candidates.next(), candidates.next()) {
            (Some(row), None) if row.id == self.session => Ok(row),
            _ => Err(self.changed()),
        }
    }
}

/// Keep registry evidence through both reserved input guards. The control
/// actor already checks the pane and its agent tag on the fresh capture;
/// this nonblocking check covers session changes since the registry reads.
struct SavedClearFence {
    target: SavedSeat,
    // None latches an invalidation so a guard refusal cannot become a retry.
    rows: Mutex<Option<broadcast::Receiver<SessionEvent>>>,
}

impl SavedClearFence {
    fn permits(&self, harness: Harness) -> bool {
        let Ok(mut slot) = self.rows.try_lock() else {
            return false;
        };
        let valid = slot.as_mut().is_some_and(|rows| {
            for _ in 0..=rows.len() {
                match rows.try_recv() {
                    Ok(event) => {
                        if self.target.row_wake(&event).is_err()
                            || matches!(event, SessionEvent::Updated { session }
                                if session.id != self.target.session && self.target.candidate(&session, harness))
                        {
                            return false;
                        }
                    }
                    Err(broadcast::error::TryRecvError::Empty) => return true,
                    Err(_) => return false, // lost evidence cannot authorize input
                }
            }
            rows.is_empty()
        });
        if !valid {
            *slot = None;
        }
        valid
    }

    fn invalidated(&self) -> bool {
        self.rows.try_lock().map_or(true, |slot| slot.is_none())
    }
}

/// These two early results mean no Clear was committed and no input attempted.
/// Once the flow commits to the clear, only an error or Cleared can leave it.
#[derive(Debug)]
pub(super) enum ClearAttempt {
    Blocked(&'static str),
    Expired,
    Cleared(ClearOutcome),
}

#[derive(Debug)]
pub(super) struct ClearOutcome {
    pub result: ClearResult,
    /// This operation's committed binding (or unbound editor) fence.
    pub ready_seq: Option<u64>,
    pub start_line: StartLineDisposition,
}

struct ClearSetup {
    pane: PaneId,
    harness: Harness,
    screen: &'static AgentScreen,
    baseline: PaneBaseline,
    before: Vec<SessionId>,
    bind: Option<SessionId>,
}

enum ClearPreflight {
    Blocked(&'static str),
    Noop,
    Ready(ClearSetup),
}

fn clear_refused(id: &AgentId, pane: &PaneId, reason: &str) -> AgentError {
    AgentError::Refused(format!(
        "agent {id} was not cleared: {reason}; nothing was sent to {pane}"
    ))
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
        saved: Option<&SavedSeat>,
        retry_before: Option<Instant>,
    ) -> Result<ClearAttempt, AgentError> {
        // Subscribe before the preflight reads the rows. Retrying a read-only
        // preflight must not leave any live operation with input authority.
        let mut events = self.registry.subscribe();
        let fence = saved.map(|target| {
            Arc::new(SavedClearFence {
                target: target.clone(),
                rows: Mutex::new(Some(events.resubscribe())),
            })
        });
        let preflight = self.clear_preflight(r, id, reserved, saved);
        let plan = match retry_before {
            Some(at) => match timeout_at(at, preflight).await {
                Ok(result) => result?,
                Err(_) => return Ok(ClearAttempt::Expired),
            },
            None => preflight.await?,
        };
        let plan = match plan {
            ClearPreflight::Ready(plan) => plan,
            ClearPreflight::Blocked(reason) => return Ok(ClearAttempt::Blocked(reason)),
            ClearPreflight::Noop => {
                return Ok(ClearAttempt::Cleared(ClearOutcome {
                    result: ClearResult {
                        agent_id: id.clone(),
                        session_id: None,
                    },
                    ready_seq: None,
                    start_line: StartLineDisposition::Unidentified,
                }))
            }
        };
        if let Some(session_id) = plan.bind.clone() {
            self.org
                .execute(
                    OrgCommand::SessionBound {
                        agent_id: id.clone(),
                        session_id,
                    },
                    chrono::Utc::now(),
                )
                .await?;
        }
        if retry_before.is_some_and(|at| Instant::now() >= at) {
            return Ok(ClearAttempt::Expired);
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

        self.finish_clear(r, id, waiting, plan, fence, &mut events)
            .await
    }

    async fn clear_preflight(
        &self,
        r: &Reservation,
        id: &AgentId,
        reserved: Option<&PaneId>,
        saved: Option<&SavedSeat>,
    ) -> Result<ClearPreflight, AgentError> {
        let state = self.org.snapshot().await?;
        let agent = state
            .agent(id)
            .ok_or_else(|| AgentError::NotFound(format!("agent {id} was not found")))?;
        if let Some(saved) = saved.filter(|saved| !saved.matches(agent)) {
            return Err(saved.changed());
        }
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
        let capture = r.capture(&pane, format).await?;
        let mut registry_idle = true;
        if let Some(saved) = saved {
            // Capture is paced. Recheck the binding and registry under the
            // reservation *after* that await, not just when it was requested.
            let rows = self.pane_sessions(&pane).await?;
            let latest = self.org.snapshot().await?;
            if latest.agent(id).is_none_or(|agent| !saved.matches(agent)) {
                return Err(saved.changed());
            }
            registry_idle = saved.pinned_row(&rows, harness)?.status == SessionStatus::Idle;
        }
        let refused = |block: InputBlocker| match block {
            InputBlocker::Temporary(reason) if saved.is_some() => {
                Ok(ClearPreflight::Blocked(reason))
            }
            _ => Err(clear_refused(id, &pane, block.reason())),
        };
        if let Some(block) = input_blocker(screen, &capture, &baseline, &tag) {
            return refused(block);
        }
        let control = screen.control(&capture, &baseline);
        if clear_noop(policy, agent.session_id.is_some(), own.len(), &control) {
            return Ok(ClearPreflight::Noop);
        }
        if let Some(reason) = clear_refusal(policy, &control) {
            return refused(if safe_editor(&control) {
                InputBlocker::Terminal(reason)
            } else {
                InputBlocker::Temporary(reason)
            });
        }
        if !registry_idle {
            return Ok(ClearPreflight::Blocked("the session is not idle"));
        }
        // An unbound standalone clear excludes the sole earlier session too.
        let bind = own
            .first()
            .filter(|_| agent.session_id.is_none())
            .map(|row| row.id.clone());
        Ok(ClearPreflight::Ready(ClearSetup {
            pane,
            harness,
            screen,
            baseline,
            before,
            bind,
        }))
    }

    async fn finish_clear(
        &self,
        r: &Reservation,
        id: &AgentId,
        waiting: &Waiting,
        setup: ClearSetup,
        fence: Option<Arc<SavedClearFence>>,
        events: &mut broadcast::Receiver<SessionEvent>,
    ) -> Result<ClearAttempt, AgentError> {
        let ClearSetup {
            pane,
            harness,
            screen,
            baseline,
            mut before,
            ..
        } = setup;
        let (policy, format, tag) = (screen.policy(), screen.capture_format(), id.to_string());
        let command = policy.clear_command;
        if let Some(fence) = &fence {
            // Clear moves the old binding to the exclusion under our reservation.
            let target = &fence.target;
            let state = self.org.snapshot().await?;
            if state.agent(&target.agent).is_none_or(|agent| {
                agent.status != AgentStatus::Clearing
                    || agent.pane.as_deref() != Some(target.pane.as_str())
                    || agent.excluded_session_id.as_ref() != Some(&target.session)
            }) {
                return Err(target.changed());
            }
            target.pinned_row(&self.pane_sessions(&pane).await?, harness)?;
        }
        loop {
            let (baseline, tag, check) = (baseline.clone(), tag.clone(), fence.clone());
            let guard = move |capture: &_| {
                let safe = safe_to_paste(screen, capture, &baseline, &tag);
                check.is_none_or(|fence| fence.permits(harness)) && safe
            };
            let pasted = r
                .paste_guarded(&pane, id.clone(), command.to_owned(), format, guard)
                .await;
            if let Some(fence) = fence.as_ref().filter(|fence| fence.invalidated()) {
                return Err(fence.target.changed());
            }
            match pasted {
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
        waiting_for(waiting, "the clear command did not appear in the editor");
        let not_submitted = || {
            stopped(
                waiting,
                id,
                "clear",
                "the clear command was typed and not submitted".into(),
            )
        };
        loop {
            let capture = r.capture(&pane, format).await?;
            if fence.as_ref().is_some_and(|fence| !fence.permits(harness)) {
                return Err(not_submitted());
            }
            match submit_step(screen, &capture, &baseline, &tag, command) {
                Step::Key { key, .. } => {
                    let (baseline, tag, check) = (baseline.clone(), tag.clone(), fence.clone());
                    let guard = move |capture: &_| {
                        let safe = submit_allowed(screen, capture, &baseline, &tag, command);
                        check.is_none_or(|fence| fence.permits(harness)) && safe
                    };
                    // A session that is on the pane before the Enter is not
                    // its result (somebody cleared by hand meanwhile). Read
                    // before the key, not after: the real one can beat that.
                    let rows = self.pane_sessions(&pane).await?;
                    if let Some(fence) = &fence {
                        fence
                            .target
                            .pinned_row(&rows, harness)
                            .map_err(|_| not_submitted())?;
                    }
                    before.extend(rows.into_iter().map(|row| row.view.id));
                    // Said before the key: a deadline while the key is out
                    // must not report the step before it.
                    waiting_for(
                        waiting,
                        "the clear command was being submitted and may have reached the harness",
                    );
                    #[cfg(test)]
                    self.progress("clear_submitting", Some(id.clone()));
                    let entered = r.keys_guarded(&pane, id.clone(), key, format, guard).await;
                    if fence.as_ref().is_some_and(|fence| fence.invalidated()) {
                        return Err(not_submitted());
                    }
                    match entered {
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
                    // History must see the start line before it can let an
                    // older job act on this Ready, even across replay.
                    let start_line = self
                        .queue_start_line(id, session_id.as_ref(), fence.is_some())
                        .await;
                    let ready = self.org.execute(command, chrono::Utc::now()).await?;
                    return Ok(ClearAttempt::Cleared(ClearOutcome {
                        result: ClearResult {
                            agent_id: id.clone(),
                            session_id,
                        },
                        ready_seq: ready.first().map(|event| event.seq),
                        start_line,
                    }));
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
        saved_handover: bool,
    ) -> Result<(), AgentError> {
        // A saved handover is already bound. With a readable, unset marker,
        // its preflight never submitted Clear and has no writes to settle.
        if saved_handover
            && waiting
                .lock()
                .is_ok_and(|progress| progress.reason.is_none())
        {
            return Ok(());
        }
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

    /// The cleanup of a respawn: that of the Down it began with (a killed
    /// pane the agent is still recorded on can't be accounted for), then that
    /// of the start that followed.
    pub(super) async fn settle_respawn(
        &self,
        id: &AgentId,
        old: Option<&PaneId>,
        effect: &Effect,
        waiting: &Waiting,
    ) -> Result<(), AgentError> {
        let state = self.org.snapshot().await?;
        if effect.load(Ordering::SeqCst)
            && state.agent(id).is_some_and(|agent| {
                agent.status != AgentStatus::Down
                    && agent.pane.as_deref() == old.map(PaneId::as_str)
            })
        {
            return Err(AgentError::Internal(format!(
                "agent {id}'s pane was changed but its Down is not recorded"
            )));
        }
        self.settle_up(id, waiting).await
    }

    async fn settle(
        &self,
        id: &AgentId,
        waiting: &Waiting,
        unfinished: AgentStatus,
        what: &str,
    ) -> Result<(), AgentError> {
        let state = self.org.snapshot().await?;
        let (committed, unaudited) = waiting.lock().map_or_else(
            |_| (Some("its progress was lost".into()), None),
            |mut w| {
                let sent = w.unanswered.iter().map(|sent| format!("; {sent}"));
                let reason = w
                    .reason
                    .clone()
                    .map(|reason| reason + &sent.collect::<String>());
                (reason, w.unaudited.take())
            },
        );
        if let Some(audit) = unaudited {
            // An accept went out and the flow ended before its dialog was
            // seen to go. A failure to write this must not cost the Stuck
            // record below, which names the key too.
            let _ = self
                .org
                .execute(OrgCommand::ControlSent(audit), chrono::Utc::now())
                .await;
        }
        let (Some(why), Some(agent)) = (
            committed,
            state.agent(id).filter(|agent| agent.status == unfinished),
        ) else {
            return Ok(());
        };
        let reason = format!(
            "{what} did not complete: {why}; inspect {}",
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
        // For the caller's answer: the record is written, whatever follows.
        if let Ok(mut waiting) = waiting.lock() {
            waiting.stuck = Some(reason.clone());
        }
        self.notify_owner(format!("agent {id}: {reason}")).await
    }

    async fn notify_owner(&self, body: String) -> Result<(), AgentError> {
        let note = atm_org::MessageContent {
            delivery_notice: None,
            from: atm_org::Address::System,
            to: atm_org::Address::Owner,
            kind: atm_org::MessageKind::Note,
            body,
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

/// What a respawn replaces before it starts the agent again: the pane the
/// agent was reserved with, if it had one, and the flag its kill sets.
#[derive(Debug)]
pub(super) struct Replace {
    pub reserved: Option<PaneId>,
    pub effect: Effect,
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
    /// whose line can be read exactly, on a pane nobody has selected; that
    /// line is read once. The command is typed only after exactly that line,
    /// and run only when the line reads it followed by the command and
    /// nothing else. A line that reads otherwise before the paste ends the
    /// start. After the paste the text can't be taken back, so the start
    /// waits: the Enter goes out only once the line reads exactly that again,
    /// and at the deadline the command is left typed and not run. The
    /// command is pasted once and run once, each under a guard on the control
    /// actor's own capture; an input whose outcome is unknown is never
    /// repeated. A dialog the harness opens at its start is answered only
    /// where the harness's policy and the role allow it (a trust dialog for
    /// one of the role's trust directories, a prompt the launch may skip),
    /// each key under a guard on that same dialog, and audited.
    ///
    /// Once the flow knows its Up is recorded it never gives up quietly: every
    /// failure, and the deadline, leaves the agent Starting for
    /// [`Self::settle_up`] to mark Stuck. Two cases end otherwise. If the Up's
    /// reply is lost, the flow has set no reason, the cleanup marks nothing,
    /// and the next observation records the agent Down for having no pane.
    /// And if the cleanup can't write, the reservation is quarantined with
    /// the agent still Starting.
    ///
    /// With `replace` this is a respawn: once the pane to split is proven, and
    /// is not the agent's own, the agent is taken down as [`Self::down`] does
    /// with a kill, and then started as above, all in this one reservation.
    /// With a role's `pane` hint, `agent.respawn` called from the agent's own
    /// pane intentionally kills the caller's pane and loses its reply while
    /// the daemon finishes the job; use `atm agent list` to check the outcome.
    pub(super) async fn up(
        &self,
        r: &Reservation,
        launch: Launch,
        replace: Option<Replace>,
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
        let (verb, method) = if replace.is_some() {
            ("respawned", "agent.respawn")
        } else {
            ("started", "agent.up")
        };
        let refused = |reason: String| {
            AgentError::Refused(format!(
                "agent {id} was not {verb}: {reason}; nothing was changed"
            ))
        };
        // Why the pane to split can't be used; `None` when it can.
        let placement = || async {
            let found = r.inspect(&target).await?.filter(|found| !found.dead);
            Ok::<_, AgentError>(match (found, &beside) {
                (None, _) => Some(format!("the pane to split, {target}, is gone")),
                (Some(found), Some(beside))
                    if found.local_agent_tag.as_deref() != Some(&beside.to_string()) =>
                {
                    Some(format!("{target} is no longer agent {beside}'s pane"))
                }
                _ => None,
            })
        };
        if let Some(reason) = placement().await? {
            return Err(refused(reason));
        }
        let begin = match replace {
            // The organisation refuses an agent that is already up.
            None => OrgCommand::Up {
                agent_id: id.clone(),
                role: prepared.role.clone(),
            },
            Some(Replace { reserved, effect }) => {
                // Checked before the kill: afterwards there would be no pane
                // to split.
                if reserved.as_ref() == Some(&target) {
                    return Err(refused(format!(
                        "the pane to split, {target}, is the agent's own pane, which a respawn kills; run it from another pane"
                    )));
                }
                self.down(r, &id, reserved.as_ref(), true, &effect, "agent.respawn")
                    .await?;
                // Again, now that time has passed in the kill.
                if let Some(reason) = placement().await? {
                    self.notify_owner(format!(
                        "agent {id} is down: respawn placement failed after take-down: {reason}; fix the placement and run atm agent up {} from project {}",
                        id.name, id.project
                    ))
                    .await?;
                    return Err(AgentError::Refused(format!(
                        "agent {id} was taken down and not started again: {reason}"
                    )));
                }
                OrgCommand::Respawn {
                    agent_id: id.clone(),
                    role: prepared.role.clone(),
                    reason: "respawned".into(),
                }
            }
        };
        self.org.execute(begin, chrono::Utc::now()).await?;
        // Said for the call in flight: a split that is stopped, or whose
        // answer is lost, may have made a pane that no record names.
        waiting_for(
            waiting,
            "its pane may have been created and is not recorded",
        );
        let pane = match r.split(context, direction, "50%".into()).await {
            Ok(pane) => pane,
            Err(error) => {
                if before_effect(&error) {
                    waiting_for(waiting, "its pane was not created");
                }
                return Err(error.into());
            }
        };
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
        // The prompt is read once, here, and never again: a line that reads
        // otherwise later has something typed at it, and a launch typed
        // after it would run that too.
        let (mut baseline, before) = self.idle_shell(r, &id, &pane, screen, waiting).await?;
        loop {
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
                Ok(()) => break,
                Err(error) => input_error(
                    waiting,
                    "typing the launch command",
                    "the shell's line changed before the launch command was typed",
                    error,
                )?,
            }
            // Nothing was sent. A pane that takes no input for now (somebody
            // is focused on it) is waited on; a line that is not the prompt
            // any more ends the start.
            let capture = r.capture(&pane, format).await?;
            match shell_refusal(screen, &capture, &baseline, &tag) {
                Some(reason) => waiting_for(waiting, reason),
                None if launch_line(&capture).as_deref() == Some(before.as_str()) => {}
                None => {
                    return Err(stopped(
                        waiting,
                        &id,
                        "start",
                        "the shell's line changed after its prompt was read, so nothing was typed"
                            .into(),
                    ))
                }
            }
        }
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
                        "method": method,
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
        // key this start has sent to a dialog, as (the dialog, its keys).
        let mut accepted_with = None;
        let mut sent_keys: Vec<(String, Vec<String>)> = Vec::new();
        // `gone` is said only by the audit of an accept whose dialog was not
        // seen to go; the audit of one that was has no such field.
        let audit_of = |dialog: &str, key: PaneKey, folder: Option<&str>, gone: Option<bool>| {
            let mut action = serde_json::json!({
                "method": method,
                "agent": tag,
                "dialog": dialog,
                "key": format!("{key:?}"),
            });
            if let Some(action) = action.as_object_mut() {
                if let Some(folder) = folder {
                    action.insert("folder".into(), folder.into());
                }
                if let Some(gone) = gone {
                    action.insert("dialog_gone".into(), gone.into());
                }
            }
            atm_org::ControlAudit {
                pane: pane.to_string(),
                action,
            }
        };
        let audit = |audit: atm_org::ControlAudit| {
            self.org
                .execute(OrgCommand::ControlSent(audit), chrono::Utc::now())
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
                // Never two audits for one accept: this one replaces the one
                // the cleanup would write.
                unaudited(waiting, None);
                audit(audit_of("folder_trust", key, Some(&folder), None)).await?;
            }
            match decide_start(
                policy,
                &control,
                &progress,
                &prepared.trust_dirs,
                session.as_ref(),
            ) {
                Step::Ready(ready) => {
                    let session_id = match ready {
                        Ready::Session(session_id) => Some(session_id),
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
                    self.queue_start_line(&id, session_id.as_ref(), false).await;
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
                    let dialog = dialog_name(&expect);
                    waiting_for(
                        waiting,
                        format!(
                            "{key:?} was being sent to {dialog} and may have reached the harness"
                        ),
                    );
                    #[cfg(test)]
                    self.progress("up_answering", Some(id.clone()));
                    match r.keys_guarded(&pane, id.clone(), key, format, guard).await {
                        Ok(()) => {
                            progress.sent(action, &expect);
                            // Every reason from here on names the keys sent.
                            match sent_keys.last_mut() {
                                Some((to, keys)) if *to == dialog => keys.push(format!("{key:?}")),
                                _ => sent_keys.push((dialog, vec![format!("{key:?}")])),
                            }
                            let sent: Vec<String> = sent_keys
                                .iter()
                                .map(|(to, keys)| format!("{} to {to}", keys.join(", ")))
                                .collect();
                            unanswered(
                                waiting,
                                Some(format!("atm had sent {}", sent.join(" and "))),
                            );
                            match (action, &expect) {
                                (
                                    KeyAction::TrustAccept,
                                    ControlDialog::FolderTrust { folder, .. },
                                ) => {
                                    accepted_with = Some(key);
                                    // Written by the cleanup if the start
                                    // ends before the dialog is seen to go.
                                    unaudited(
                                        waiting,
                                        Some(audit_of(
                                            "folder_trust",
                                            key,
                                            Some(folder),
                                            Some(false),
                                        )),
                                    );
                                }
                                (KeyAction::SkipLaunchPrompt, _) => {
                                    audit(audit_of("launch_skip", key, None, None)).await?;
                                }
                                _ => {}
                            }
                        }
                        Err(error) => input_error(
                            waiting,
                            &format!("sending {key:?} to {dialog}"),
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
    /// show the same root process, shell and line, and that line is not blank.
    ///
    /// Text reaches that line only from a client that selected the pane, so a
    /// capture that shows the pane focused before the line is read, or whose
    /// focus could not be read, ends the start: the line could hold what was
    /// typed, and waiting for the focus to leave would not show it.
    async fn idle_shell(
        &self,
        r: &Reservation,
        id: &AgentId,
        pane: &PaneId,
        screen: &'static AgentScreen,
        waiting: &Waiting,
    ) -> Result<(PaneBaseline, String), AgentError> {
        let (format, tag) = (screen.capture_format(), id.to_string());
        let seen = |capture: &PaneCapture| {
            capture
                .inspection
                .pane_root_pid()
                .zip(capture.inspection.foreground_command.clone())
        };
        // Unknown focus is not permission: it ends the start as a shown
        // focus does.
        let selected = |capture: &PaneCapture| {
            let what = match capture.focused {
                Some(false) => return Ok(()),
                Some(true) => "the new pane was selected before its prompt could be read",
                None => {
                    "whether the new pane was selected could not be read before its prompt was read"
                }
            };
            Err(stopped(
                waiting,
                id,
                "start",
                format!(
                    "{what}, so nothing was typed; start the harness there by hand and adopt it, or take the agent down and up again"
                ),
            ))
        };
        loop {
            let first = r.capture(pane, format).await?;
            selected(&first)?;
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
            selected(&second)?;
            if let Some(reason) = shell_refusal(screen, &second, &baseline, &tag) {
                waiting_for(waiting, reason);
                continue;
            }
            match launch_line(&second) {
                // A blank line is a shell that has drawn no prompt yet (an idle
                // shell can still be reading its startup files).
                Some(line)
                    if !line.trim().is_empty() && launch_line(&first).as_ref() == Some(&line) =>
                {
                    self.record_baseline(id.clone(), pane.as_str(), baseline.clone());
                    return Ok((baseline, line));
                }
                _ => waiting_for(waiting, "the shell's prompt could not be read as settled"),
            }
        }
    }
}

#[cfg(test)]
mod progress_limit_tests {
    use super::*;

    #[test]
    fn op7_poisoned_progress_omits_even_a_previously_recorded_stuck_reason() {
        // Break caught: silently recovering a poisoned caller record publishes
        // its stale value as a proven outcome, or poisoning starts panicking.
        const REASON: &str = "clear did not complete: earlier progress; inspect %1";
        let waiting = Arc::new(std::sync::Mutex::new(Unfinished {
            stuck: Some(REASON.into()),
            ..Unfinished::default()
        }));
        assert_eq!(stuck_record(&waiting).as_deref(), Some(REASON));
        let poisoned = std::panic::catch_unwind(|| {
            let _guard = waiting.lock().unwrap();
            panic!("synthetic progress poison");
        });
        assert!(poisoned.is_err());
        assert!(waiting.is_poisoned());
        let answer = std::panic::catch_unwind(|| {
            deadline_answer(
                AgentError::Reservation(ReservationError::Timeout),
                &"p/lead".parse().unwrap(),
                stuck_record(&waiting),
            )
        });
        assert!(matches!(
            answer,
            Ok(AgentError::Reservation(ReservationError::Timeout))
        ));
        assert_eq!(
            waiting.lock().unwrap_err().into_inner().stuck.as_deref(),
            Some(REASON),
            "poisoning omitted the record, rather than erasing its contents"
        );
    }
}
