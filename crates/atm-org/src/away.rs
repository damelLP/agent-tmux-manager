//! Running without the owner: the away table and the stacking cap.
//!
//! Pure: the class, the away flag, counts and the cap are arguments. Nothing
//! here reads a clock, a file or the daemon.
//!
//! # The away table
//!
//! [`away_step`] answers what happens to a *pending* decision. "Recommended"
//! is whether the request names a recommended option.
//!
//! | Class | Owner present | Away, recommended | Away, none recommended |
//! |---|---|---|---|
//! | [`DecisionClass::Detail`] | waits | the agent decides | waits |
//! | [`DecisionClass::ChangesIntent`] | waits | taken provisionally | waits |
//! | [`DecisionClass::Irreversible`] | waits | waits | waits |
//!
//! While the owner is present nothing is taken for him. Without a recommended
//! option nothing can be recorded, so the decision waits whatever its class.
//! Both outcomes that record something are written as
//! `decision.taken_provisionally` on the recommended option: by `system` when
//! it is taken provisionally, by the requester when the agent decides. The
//! owner can overrule either.
//!
//! ## Kinds that always wait
//!
//! The class follows from two fields the requester fills in itself, so a
//! request can call a push something that can be undone. [`Decision::away_step`]
//! therefore reads the kind first: an `action`, a `hire` and a `budget` are
//! put through the table as [`DecisionClass::Irreversible`] whatever the two
//! fields say, and so is a kind this build does not know. `policy`, `plan`
//! and `dialog` keep the class their fields give them.
//!
//! # The stacking cap
//!
//! A provisional decision may carry the task in progress to its end; nothing
//! else may be built on it until the owner has answered it. The cap counts
//! tasks. The task in progress when the decision was taken is the first; it is
//! not linked to the decision, so it is counted without being named. Further
//! tasks are recorded dependencies ever assigned: direct `rests_on`, children
//! and judging reviews, including cancelled tasks. Reopening/cancelling never
//! refunds a place; resuming the same task ID never spends another.
//!
//! [`stacking_step`] lets a further task start only while
//! `1 + started < cap`, and only holds for a decision that is taken
//! provisionally and changes the intent:
//!
//! | Decision the task rests on | Step |
//! |---|---|
//! | taken provisionally, changes the intent, `1 + started >= cap` | hold |
//! | taken provisionally, changes the intent, `1 + started < cap` | start |
//! | taken provisionally, a detail (the agent decided it) | start |
//! | pending, resolved, overruled, expired, withdrawn | start |
//!
//! With the default cap of one no further task starts. With a cap of N, N - 1
//! may. Recorded parent/judges links inherit every governing decision, across
//! projects. Work that declares no link is invisible; no original task is guessed.
//!
//! # The switch
//!
//! Only the owner sets away and back ([`OwnerCommand`]). `owner.away` records
//! the time and each project's cap; `owner.back` clears the time and keeps the
//! caps, because what is still provisional stays capped until it is answered.
//! Repeating either writes nothing. [`OwnerCommand::ApplyAway`] is the
//! daemon's report "put this decision through the table": it is decided
//! against the away flag and the decision in one step, so nothing is taken
//! after the owner is back, and it writes nothing for a decision that is no
//! longer pending.
//!
//! # Settings
//!
//! [`parse_away_settings`] reads a project's away settings: one optional key,
//! `stacking_cap`, at least 1, default [`DEFAULT_STACKING_CAP`]. Unknown keys
//! are refused.

use crate::{
    Address, AgentId, Decision, DecisionClass, DecisionCommand, DecisionId, DecisionKind,
    DecisionStatus, OrgEvent, OrgState, ProjectId, ProvisionalChoice, Refusal, Task, TaskId,
    TaskState,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use thiserror::Error;

/// Why a pending decision waits for the owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AwayWait {
    /// The owner has not said he is away.
    OwnerPresent,
    /// It cannot be undone, or reaches outside the project.
    Irreversible,
    /// It names no recommended option, so nothing can be recorded.
    NoRecommendation,
}

/// What happens to a pending decision; see the module's away table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AwayStep {
    Wait(AwayWait),
    /// Record the recommended option, by the system.
    TakeProvisionally,
    /// Record the requester's own recommended option, by the requester.
    AgentDecides,
}

/// The step for a pending decision of this class. The owner's presence comes
/// first, then a class that must wait, then a missing recommendation.
#[must_use]
pub fn away_step(class: DecisionClass, recommended: bool, away: bool) -> AwayStep {
    if !away {
        return AwayStep::Wait(AwayWait::OwnerPresent);
    }
    match (class, recommended) {
        (DecisionClass::Irreversible, _) => AwayStep::Wait(AwayWait::Irreversible),
        (_, false) => AwayStep::Wait(AwayWait::NoRecommendation),
        (DecisionClass::ChangesIntent, true) => AwayStep::TakeProvisionally,
        (DecisionClass::Detail, true) => AwayStep::AgentDecides,
    }
}

/// The cap of a project that sets none: only the task in progress.
pub const DEFAULT_STACKING_CAP: u32 = 1;

/// Whether a task that rests on a decision may start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackingStep {
    Start,
    /// The cap is reached: the task waits until the owner has answered.
    Hold,
}

/// The step for a task not yet started that rests on a decision with this
/// status and class. `started` counts the other tasks resting on the same
/// decision that have started; the task in progress is added here.
#[must_use]
pub fn stacking_step(
    status: DecisionStatus,
    class: DecisionClass,
    started: usize,
    cap: u32,
) -> StackingStep {
    let capped =
        status == DecisionStatus::TakenProvisionally && class == DecisionClass::ChangesIntent;
    let in_line = u64::try_from(started).unwrap_or(u64::MAX).saturating_add(1);
    if capped && in_line >= u64::from(cap) {
        StackingStep::Hold
    } else {
        StackingStep::Start
    }
}

/// A project's away settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AwaySettings {
    #[serde(default = "default_stacking_cap")]
    pub stacking_cap: u32,
}
fn default_stacking_cap() -> u32 {
    DEFAULT_STACKING_CAP
}
impl Default for AwaySettings {
    fn default() -> Self {
        Self {
            stacking_cap: DEFAULT_STACKING_CAP,
        }
    }
}

#[derive(Debug, Error)]
pub enum AwaySettingsError {
    /// The diagnostic can quote the file's text.
    #[error("away settings: TOML: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("stacking_cap must be at least 1")]
    CapBelowOne,
}

/// Parse a project's away settings. An empty text is the default.
pub fn parse_away_settings(text: &str) -> Result<AwaySettings, AwaySettingsError> {
    let settings: AwaySettings = toml::from_str(text)?;
    if settings.stacking_cap < 1 {
        return Err(AwaySettingsError::CapBelowOne);
    }
    Ok(settings)
}

/// Body of `owner.away`: who said so, and each project's stacking cap as read
/// at that moment. A project without an entry has [`DEFAULT_STACKING_CAP`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerAway {
    pub by: Address,
    pub stacking_caps: BTreeMap<ProjectId, u32>,
}
impl OwnerAway {
    pub(crate) fn fold(&self, owner: &mut OwnerState, ts: DateTime<Utc>) {
        owner.away_since = Some(ts);
        owner.stacking_caps.clone_from(&self.stacking_caps);
    }
}

/// Body of `owner.back`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerBack {
    pub by: Address,
}

/// Body of `owner.relay_set`: the designated recorder, or None to revoke it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerRelaySet {
    pub by: Address,
    pub agent: Option<AgentId>,
}

/// The owner's away switch and relay designation as folded from the log.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OwnerState {
    /// The time of the `owner.away` in force; `None` while the owner is present.
    pub away_since: Option<DateTime<Utc>>,
    /// The caps of the latest `owner.away`, kept after `owner.back`.
    pub stacking_caps: BTreeMap<ProjectId, u32>,
    /// One owner-designated full agent ID; independent of the away switch.
    pub relay: Option<AgentId>,
}
impl OwnerState {
    /// The cap for a project; the default for one not listed, and for a
    /// requester that belongs to no project.
    #[must_use]
    pub fn stacking_cap(&self, project: Option<&ProjectId>) -> u32 {
        project
            .and_then(|id| self.stacking_caps.get(id))
            .copied()
            .unwrap_or(DEFAULT_STACKING_CAP)
    }
}

/// The away switch and the daemon's report, carried by
/// [`crate::OrgCommand::Owner`]; see the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnerCommand {
    Away {
        by: Address,
        stacking_caps: BTreeMap<ProjectId, u32>,
    },
    Back {
        by: Address,
    },
    SetRelay {
        by: Address,
        agent: Option<AgentId>,
    },
    /// Freeze the committed return view after the switch has been set back.
    CreateDigest {
        by: Address,
        back_seq: Option<u64>,
    },
    /// Put this decision through the away table.
    ApplyAway {
        decision_id: DecisionId,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AwayRefusal {
    #[error("only the owner may set away or back")]
    NotOwner { who: Address },
    #[error("task {task_id} rests on decision {decision_id}, which was taken provisionally for the owner: it waits until he has seen {decision_id} (stacking cap {cap})")]
    Held {
        task_id: TaskId,
        decision_id: DecisionId,
        cap: u32,
    },
}

impl OwnerCommand {
    /// The candidate event, from the command alone. `ApplyAway` cannot know
    /// its option or taker without the state: its candidate only carries the
    /// decision reference for validation, and decide builds the fact.
    pub(crate) fn event(&self) -> OrgEvent {
        match self.clone() {
            Self::Away { by, stacking_caps } => {
                OrgEvent::OwnerAway(OwnerAway { by, stacking_caps })
            }
            Self::Back { by } => OrgEvent::OwnerBack(OwnerBack { by }),
            Self::SetRelay { by, agent } => OrgEvent::OwnerRelaySet(OwnerRelaySet { by, agent }),
            Self::CreateDigest { by, back_seq } => {
                OrgEvent::OwnerDigestCreated(crate::OwnerDigestCreated { by, back_seq })
            }
            Self::ApplyAway { decision_id } => {
                OrgEvent::DecisionTakenProvisionally(ProvisionalChoice {
                    decision_id,
                    option: String::new(),
                    by: Address::System,
                    relayed_by: None,
                })
            }
        }
    }
}

impl Decision {
    /// The away table's answer for this decision's class and recommendation.
    /// Only meaningful while the decision is pending.
    #[must_use]
    pub fn away_step(&self, away: bool) -> AwayStep {
        // Kinds that reach outside wait, whatever class the requester claims.
        let class = match self.request.kind {
            DecisionKind::Action
            | DecisionKind::Hire
            | DecisionKind::Budget
            | DecisionKind::Unknown => DecisionClass::Irreversible,
            DecisionKind::Policy | DecisionKind::Plan | DecisionKind::Dialog => self.class(),
        };
        away_step(class, self.request.recommended.is_some(), away)
    }
}

fn owner_only(by: &Address) -> Result<(), AwayRefusal> {
    if *by == Address::Owner {
        Ok(())
    } else {
        Err(AwayRefusal::NotOwner { who: by.clone() })
    }
}

impl OrgState {
    #[must_use]
    pub fn owner(&self) -> &OwnerState {
        &self.owner
    }

    /// The tasks whose `rests_on` names this decision, in creation order.
    pub fn resting_tasks(&self, id: DecisionId) -> impl Iterator<Item = &Task> {
        self.tasks().filter(move |task| task.rests_on == Some(id))
    }

    /// The switch and the away table. `event` is the command's candidate,
    /// whose references decide has already validated.
    pub(crate) fn decide_owner(
        &self,
        command: &OwnerCommand,
        event: OrgEvent,
        now: DateTime<Utc>,
    ) -> Result<Vec<OrgEvent>, Refusal> {
        let away = self.owner.away_since.is_some();
        let changed = match command {
            OwnerCommand::Away { by, stacking_caps } => {
                owner_only(by)?;
                if stacking_caps.values().any(|cap| *cap < 1) {
                    return Err(Refusal::InvalidValue {
                        field: "stacking_cap",
                        reason: "must be at least 1".into(),
                    });
                }
                !away
            }
            OwnerCommand::Back { by } => {
                owner_only(by)?;
                away
            }
            OwnerCommand::SetRelay { by, agent } => {
                owner_only(by)?;
                self.owner.relay != *agent
            }
            OwnerCommand::CreateDigest { by, back_seq } => {
                if *by != Address::Owner {
                    return Err(Refusal::InvalidValue {
                        field: "return digest",
                        reason: "only the owner may create a return digest".into(),
                    });
                }
                crate::return_digest::check_digest(self, *back_seq)?;
                true
            }
            OwnerCommand::ApplyAway { decision_id } => {
                let decision = self.require_decision(decision_id)?;
                if decision.status != DecisionStatus::Pending {
                    return Ok(vec![]);
                }
                let by = match decision.away_step(away) {
                    AwayStep::Wait(_) => return Ok(vec![]),
                    AwayStep::TakeProvisionally => Address::System,
                    AwayStep::AgentDecides => decision.request.requester.clone(),
                };
                let Some(option) = decision.request.recommended.clone() else {
                    return Ok(vec![]);
                };
                let choice = ProvisionalChoice {
                    decision_id: decision.id,
                    option,
                    by,
                    relayed_by: None,
                };
                // The decision policy stays the one place that lets a take through.
                return self.decide_decision(
                    &DecisionCommand::TakeProvisionally(choice.clone()),
                    OrgEvent::DecisionTakenProvisionally(choice),
                    now,
                );
            }
        };
        Ok(if changed { vec![event] } else { vec![] })
    }

    /// Every inherited cap at a first assignment. Previously assigned IDs may
    /// resume; current state changes never refund their exposure.
    pub(crate) fn stacking_hold(&self, task: &Task) -> Result<(), AwayRefusal> {
        if task.state != TaskState::Open || self.task_was_assigned(&task.id) {
            return Ok(());
        }
        for decision in self.decisions().filter(|d| {
            d.status == DecisionStatus::TakenProvisionally
                && d.class() == DecisionClass::ChangesIntent
        }) {
            let impact = crate::decision_impact(self, decision.id);
            if !impact.contains(&task.id) {
                continue;
            }
            let started = impact.started_count()
                + impact
                    .cancelled
                    .iter()
                    .filter(|id| self.task_was_assigned(id))
                    .count();
            let project = match &decision.request.requester {
                Address::Agent(agent) => Some(&agent.project),
                Address::Owner | Address::System => None,
            };
            let cap = self.owner.stacking_cap(project);
            if stacking_step(decision.status, decision.class(), started, cap) == StackingStep::Hold
            {
                return Err(AwayRefusal::Held {
                    task_id: task.id.clone(),
                    decision_id: decision.id,
                    cap,
                });
            }
        }
        Ok(())
    }
}
