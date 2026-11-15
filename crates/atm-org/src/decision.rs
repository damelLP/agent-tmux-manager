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
//! older fact still folds. Older readers skip all six kinds.

use crate::Address;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};

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

/// Body of `decision.resolved` and `decision.overruled`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionResolution {
    pub decision_id: DecisionId,
    pub option: String,
    pub by: Address,
    pub note: Option<String>,
    pub standing_rule: bool,
}

/// Body of `decision.withdrawn`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionWithdrawal {
    pub decision_id: DecisionId,
    pub by: Address,
    pub reason: Option<String>,
}

/// One decision as folded from the log. `closed_at` is the time of the fact
/// that ended it: resolved, overruled, expired or withdrawn.
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
