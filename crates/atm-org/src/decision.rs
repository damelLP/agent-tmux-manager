//! Decisions: a question with listed options, put to the owner.
//!
//! A decision's ID is `d<seq>`, the sequence number of its
//! `decision.requested` event, so that body carries no ID. The class is never
//! stored: it follows from the request's two facts, so the two cannot disagree.
//!
//! | may be undone | changes the intent | class | while the owner is away |
//! |---|---|---|---|
//! | yes | no | [`DecisionClass::Detail`] | the agent decides and it is logged |
//! | yes | yes | [`DecisionClass::ChangesIntent`] | taken provisionally |
//! | no | either | [`DecisionClass::Irreversible`] | waits |
//!
//! An action that reaches outside the project counts as one that may not be
//! undone: a push or a deploy is published once it happens.
//!
//! # Events
//!
//! Six additive kinds. `decision.resolved` and `decision.overruled` share one
//! body: an overrule is the owner's answer against what was taken for him.
//! Optional facts (`recommended`, `deadline`, `note`, `reason`) emit null.
//! Applying a fact checks references only (the decision exists, the requester
//! and `by` are known, the ID is new) and never today's policy, so a late or
//! older fact still folds. Readers predating decisions skip all six kinds.
//! An unrecognised request `kind` folds as [`DecisionKind::Unknown`], losing
//! its original word; policy refuses new requests with that fallback.

use crate::{Address, OrgEvent, OrgState, Refusal};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, fmt, str::FromStr};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct DecisionId(u64);
impl DecisionId {
    #[must_use]
    pub fn new(seq: u64) -> Self {
        Self(seq)
    }
    #[must_use]
    pub fn seq(self) -> u64 {
        self.0
    }
}
impl FromStr for DecisionId {
    type Err = crate::IdError;
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let value = text
            .strip_prefix('d')
            .and_then(|s| s.parse::<u64>().ok())
            .map(Self);
        value
            .filter(|id| id.to_string() == text)
            .ok_or_else(|| crate::IdError {
                kind: "decision id",
                value: text.into(),
                rule: "expected d followed by canonical u64 decimal",
            })
    }
}
impl fmt::Display for DecisionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "d{}", self.0)
    }
}
impl TryFrom<String> for DecisionId {
    type Error = crate::IdError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}
impl From<DecisionId> for String {
    fn from(value: DecisionId) -> Self {
        value.to_string()
    }
}

/// What is asked for: an action (deploy, push), a hire, a budget, an answer
/// to a policy question, a plan's approval, or an escalated harness dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionKind {
    Action,
    Hire,
    Budget,
    Policy,
    Plan,
    Dialog,
    /// Read-only fallback for later kinds; the original word is not retained.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionClass {
    /// A detail inside the intent.
    Detail,
    /// Changes the intent and can be undone.
    ChangesIntent,
    /// Cannot be undone, or reaches outside the project.
    Irreversible,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionStatus {
    Pending,
    TakenProvisionally,
    Resolved,
    Overruled,
    Expired,
    Withdrawn,
}

/// Body of `decision.requested`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionRequest {
    pub requester: Address,
    pub kind: DecisionKind,
    pub title: String,
    pub context: String,
    pub files: Vec<String>,
    pub options: Vec<String>,
    pub recommended: Option<String>,
    pub may_be_undone: bool,
    pub changes_intent: bool,
    pub deadline: Option<DateTime<Utc>>,
}
impl DecisionRequest {
    #[must_use]
    pub fn class(&self) -> DecisionClass {
        match (self.may_be_undone, self.changes_intent) {
            (false, _) => DecisionClass::Irreversible,
            (true, true) => DecisionClass::ChangesIntent,
            (true, false) => DecisionClass::Detail,
        }
    }
}

/// Body of `decision.taken_provisionally`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvisionalChoice {
    pub decision_id: DecisionId,
    pub option: String,
    pub by: Address,
}

/// The owner's answer, independent of the pane that submitted it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionResolution {
    pub decision_id: DecisionId,
    pub option: String,
    pub by: Address,
    pub note: Option<String>,
    pub standing_rule: bool,
}

/// Body of `decision.resolved` and `decision.overruled`. Caller metadata is
/// audit only: equality compares the answer, and the fold does not retain it.
#[derive(Debug, Clone, Eq, Serialize, Deserialize)]
pub struct DecisionResolutionEvent {
    #[serde(flatten)]
    pub resolution: DecisionResolution,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller_pane: Option<String>,
}
impl PartialEq for DecisionResolutionEvent {
    fn eq(&self, other: &Self) -> bool {
        self.resolution == other.resolution
    }
}
impl From<DecisionResolution> for DecisionResolutionEvent {
    fn from(resolution: DecisionResolution) -> Self {
        Self {
            resolution,
            caller_pane: None,
        }
    }
}

/// Body of `decision.withdrawn`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionWithdrawal {
    pub decision_id: DecisionId,
    pub by: Address,
    pub reason: Option<String>,
}

/// One decision as folded from the log. `status` decides; the other fields keep
/// the last fact of their kind, also from a log today's policy would not write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub id: DecisionId,
    pub request: DecisionRequest,
    pub requested_at: DateTime<Utc>,
    pub status: DecisionStatus,
    pub provisional: Option<ProvisionalChoice>,
    pub taken_at: Option<DateTime<Utc>>,
    pub resolution: Option<DecisionResolution>,
    pub withdrawal: Option<DecisionWithdrawal>,
    /// Time of the last resolved, overruled, expired or withdrawn fact, even if
    /// a later fact changed `status` back to taken provisionally.
    pub closed_at: Option<DateTime<Utc>>,
}
impl Decision {
    pub(crate) fn new(id: DecisionId, request: DecisionRequest, ts: DateTime<Utc>) -> Self {
        Self {
            id,
            request,
            requested_at: ts,
            status: DecisionStatus::Pending,
            provisional: None,
            taken_at: None,
            resolution: None,
            withdrawal: None,
            closed_at: None,
        }
    }
    #[must_use]
    pub fn class(&self) -> DecisionClass {
        self.request.class()
    }
    pub(crate) fn close(&mut self, status: DecisionStatus, ts: DateTime<Utc>) {
        self.status = status;
        self.closed_at = Some(ts);
    }
}

/// Requests about decisions, carried by [`crate::OrgCommand::Decision`]. Each holds
/// the body of the event it asks for; [`OrgState::decide`] applies the policy
/// table in the crate documentation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecisionCommand {
    Request(DecisionRequest),
    /// Only on the recommended option; the caller reads it from the request.
    TakeProvisionally(ProvisionalChoice),
    /// The owner's answer. It becomes `decision.overruled` when it differs
    /// from a provisional choice; the caller never picks between the two.
    Resolve(DecisionResolution),
    /// The deadline has passed, judged against `decide`'s `now`.
    Expire {
        decision_id: DecisionId,
    },
    Withdraw(DecisionWithdrawal),
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DecisionRefusal {
    #[error("cannot {operation} decision {decision_id} while {state:?}")]
    InvalidState {
        decision_id: DecisionId,
        state: DecisionStatus,
        operation: &'static str,
    },
    #[error("decision {decision_id} cannot be undone or reaches outside: it waits for the owner")]
    MustWait { decision_id: DecisionId },
    #[error("decision {decision_id} can only be taken provisionally on its recommended option, not {option:?}")]
    NotRecommended {
        decision_id: DecisionId,
        option: String,
    },
    #[error("{option:?} is not an option of decision {decision_id}")]
    UnknownOption {
        decision_id: DecisionId,
        option: String,
    },
    #[error("{who} may not {operation} decision {decision_id}")]
    NotAllowed {
        decision_id: DecisionId,
        who: Address,
        operation: &'static str,
    },
    #[error("decision {decision_id} has no deadline")]
    NoDeadline { decision_id: DecisionId },
    #[error("decision {decision_id} is open until {deadline}")]
    DeadlineNotReached {
        decision_id: DecisionId,
        deadline: DateTime<Utc>,
    },
}

impl DecisionCommand {
    /// The candidate event, built from the command alone like every other
    /// command's, so decide validates its references before policy.
    pub(crate) fn event(&self) -> OrgEvent {
        match self.clone() {
            Self::Request(request) => OrgEvent::DecisionRequested(request),
            Self::TakeProvisionally(choice) => OrgEvent::DecisionTakenProvisionally(choice),
            Self::Resolve(resolution) => OrgEvent::DecisionResolved(resolution.into()),
            Self::Expire { decision_id } => OrgEvent::DecisionExpired { decision_id },
            Self::Withdraw(withdrawal) => OrgEvent::DecisionWithdrawn(withdrawal),
        }
    }
}

impl OrgState {
    /// Decision policy. `event` is the command's candidate, whose references
    /// decide has already validated.
    pub(crate) fn decide_decision(
        &self,
        command: &DecisionCommand,
        event: OrgEvent,
        now: DateTime<Utc>,
    ) -> Result<Vec<OrgEvent>, Refusal> {
        let changed = match command {
            DecisionCommand::Request(request) => {
                check_request(request, now)?;
                true
            }
            DecisionCommand::TakeProvisionally(choice) => {
                let decision = self.require_decision(&choice.decision_id)?;
                let decision_id = decision.id;
                match decision.status {
                    DecisionStatus::Pending => {
                        if decision.class() == DecisionClass::Irreversible {
                            return Err(DecisionRefusal::MustWait { decision_id }.into());
                        }
                        if decision.request.recommended.as_ref() != Some(&choice.option) {
                            return Err(DecisionRefusal::NotRecommended {
                                decision_id,
                                option: choice.option.clone(),
                            }
                            .into());
                        }
                        true
                    }
                    DecisionStatus::TakenProvisionally
                        if decision
                            .provisional
                            .as_ref()
                            .is_some_and(|taken| taken.option == choice.option) =>
                    {
                        false
                    }
                    _ => return Err(wrong_state(decision, "take provisionally")),
                }
            }
            DecisionCommand::Resolve(resolution) => {
                let decision = self.require_decision(&resolution.decision_id)?;
                if resolution.by != Address::Owner {
                    return Err(not_allowed(decision, &resolution.by, "resolve"));
                }
                match decision.status {
                    DecisionStatus::Pending | DecisionStatus::TakenProvisionally => {
                        if !decision.request.options.contains(&resolution.option) {
                            return Err(DecisionRefusal::UnknownOption {
                                decision_id: decision.id,
                                option: resolution.option.clone(),
                            }
                            .into());
                        }
                        let overrules = decision.status == DecisionStatus::TakenProvisionally
                            && decision
                                .provisional
                                .as_ref()
                                .is_some_and(|taken| taken.option != resolution.option);
                        if overrules {
                            return Ok(vec![OrgEvent::DecisionOverruled(
                                resolution.clone().into(),
                            )]);
                        }
                        true
                    }
                    DecisionStatus::Resolved | DecisionStatus::Overruled
                        if decision.resolution.as_ref() == Some(resolution) =>
                    {
                        false
                    }
                    _ => return Err(wrong_state(decision, "resolve")),
                }
            }
            DecisionCommand::Expire { decision_id } => {
                let decision = self.require_decision(decision_id)?;
                let decision_id = decision.id;
                match (decision.status, decision.request.deadline) {
                    (DecisionStatus::Expired, _) => false,
                    (DecisionStatus::Pending, None) => {
                        return Err(DecisionRefusal::NoDeadline { decision_id }.into())
                    }
                    (DecisionStatus::Pending, Some(deadline)) if now < deadline => {
                        return Err(DecisionRefusal::DeadlineNotReached {
                            decision_id,
                            deadline,
                        }
                        .into())
                    }
                    (DecisionStatus::Pending, Some(_)) => true,
                    _ => return Err(wrong_state(decision, "expire")),
                }
            }
            DecisionCommand::Withdraw(withdrawal) => {
                let decision = self.require_decision(&withdrawal.decision_id)?;
                if withdrawal.by != Address::Owner && withdrawal.by != decision.request.requester {
                    return Err(not_allowed(decision, &withdrawal.by, "withdraw"));
                }
                match decision.status {
                    DecisionStatus::Withdrawn => false,
                    DecisionStatus::Pending => true,
                    _ => return Err(wrong_state(decision, "withdraw")),
                }
            }
        };
        Ok(if changed { vec![event] } else { vec![] })
    }
}

fn check_request(request: &DecisionRequest, now: DateTime<Utc>) -> Result<(), Refusal> {
    let invalid = |field, reason: &str| {
        Err(Refusal::InvalidValue {
            field,
            reason: reason.into(),
        })
    };
    if request.requester == Address::Owner {
        return invalid("requester", "must be an agent or system");
    }
    if request.kind == DecisionKind::Unknown {
        return invalid("kind", "must be known");
    }
    let options = &request.options;
    if request.title.is_empty() {
        return invalid("title", "must not be empty");
    }
    if options.len() < 2 {
        return invalid("options", "need at least two");
    }
    if options.iter().any(String::is_empty) {
        return invalid("options", "must not be empty");
    }
    if options.iter().collect::<BTreeSet<_>>().len() != options.len() {
        return invalid("options", "must be distinct");
    }
    if request
        .recommended
        .as_ref()
        .is_some_and(|option| !options.contains(option))
    {
        return invalid("recommended", "must be one of the options");
    }
    if request.deadline.is_some_and(|deadline| deadline <= now) {
        return invalid("deadline", "must be in the future");
    }
    Ok(())
}
fn wrong_state(decision: &Decision, operation: &'static str) -> Refusal {
    DecisionRefusal::InvalidState {
        decision_id: decision.id,
        state: decision.status,
        operation,
    }
    .into()
}
fn not_allowed(decision: &Decision, who: &Address, operation: &'static str) -> Refusal {
    DecisionRefusal::NotAllowed {
        decision_id: decision.id,
        who: who.clone(),
        operation,
    }
    .into()
}
