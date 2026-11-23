//! The away switch and relay designation: owner methods, settings, and what the decision
//! bridge does while the owner is away.
//!
//! Policy stays in atm-org. This module only supplies its inputs (the caller,
//! each project's cap as read from `<project>/.atm/away.toml`) and carries out
//! its answers:
//!
//! - `owner.away` reads the cap of every registered project, then submits the
//!   switch. A file that cannot be read or parsed refuses the call and records
//!   nothing: the owner is at the desk when it fails. A repeat reads no file.
//! - While away, [`apply_pending`] reports every pending decision the away
//!   table would record. The writer decides each report against the away flag
//!   as it is then, so a report that arrives after `owner.back` takes nothing.
//! - [`notice`] tells a requester what became of its decision, as a reply in
//!   the question's thread (`decision_steps`), or as a message for a decision
//!   without a question post. Like the answer message, a notice's exact
//!   content is its completion marker: the bridge fills a missing one after a
//!   restart and never sends two. The texts are therefore a persistence
//!   contract, not presentation only.
//!
//! Both run inside the decision bridge's reconcile, which also watches
//! `owner.` events, so a decision requested before `owner.away` is handled
//! when the switch is set, and one left pending by a restart when the daemon
//! comes back.

use super::{
    agents::AgentError,
    decisions::{DecisionError, DecisionHandle},
    message_inbox::caller_address,
    messages::MessageClock,
    OrgHandle, OrgServiceError,
};
use crate::server::methods::{parse_params, CallContext, MethodError, MethodRouter};
use atm_org::{
    Address, AgentId, AwayStep, AwayWait, Decision, DecisionStatus, MessageContent, MessageKind,
    OrgCommand, OrgState, OwnerCommand, ProjectId,
};
use atm_protocol::error_code;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf, time::Duration};

#[cfg(test)]
#[path = "away_tests.rs"]
mod tests;

/// Neither method takes a parameter; unknown fields are refused.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SwitchParams {}

/// Empty shows the slot; a registered agent sets it; clear revokes it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelayParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentId>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub clear: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayResult {
    pub agent: Option<AgentId>,
    pub changed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AwayResult {
    /// False when the owner was away already.
    pub changed: bool,
    pub away_since: DateTime<Utc>,
    /// The cap in force for every registered project.
    pub stacking_caps: BTreeMap<ProjectId, u32>,
    /// The decisions pending when the call arrived.
    pub pending_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackResult {
    /// False when the owner was not away.
    pub changed: bool,
    /// Since when he had been away; null when he was not.
    pub away_since: Option<DateTime<Utc>>,
    pub provisional_count: usize,
    pub pending_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<super::return_digest::DigestPage>,
}

fn count(state: &OrgState, status: DecisionStatus) -> usize {
    state.decisions().filter(|d| d.status == status).count()
}

fn lane_error(error: AgentError) -> DecisionError {
    match error {
        AgentError::Refused(reason) => DecisionError::Refused(reason),
        AgentError::Timeout => DecisionError::Timeout,
        AgentError::Unavailable(reason) => DecisionError::Unavailable(reason),
        other => DecisionError::Internal(other.to_string()),
    }
}

impl DecisionHandle {
    pub async fn relay(
        &self,
        params: RelayParams,
        caller: CallContext,
    ) -> Result<RelayResult, DecisionError> {
        if params.agent.is_some() && params.clear {
            return Err(DecisionError::InvalidParams(
                "agent and clear conflict".into(),
            ));
        }
        self.call(true, async {
            let state = self.org.snapshot().await?;
            let by = caller_address(&state, &caller)?;
            if by != Address::Owner {
                return Err(DecisionError::Refused(
                    "only the owner may set or show the relay seat".into(),
                ));
            }
            let changed = if params.agent.is_some() || params.clear {
                !self
                    .org
                    .execute(
                        OrgCommand::Owner(OwnerCommand::SetRelay {
                            by,
                            agent: params.agent,
                        }),
                        self.clock.now()?,
                    )
                    .await?
                    .is_empty()
            } else {
                false
            };
            Ok(RelayResult {
                agent: self.org.snapshot().await?.owner().relay.clone(),
                changed,
            })
        })
        .await
    }

    /// The cap of every registered project, each read on the configuration lane.
    async fn stacking_caps(
        &self,
        state: &OrgState,
    ) -> Result<BTreeMap<ProjectId, u32>, DecisionError> {
        let projects = self
            .projects
            .as_ref()
            .ok_or_else(|| DecisionError::Unavailable("away settings cannot be read".into()))?;
        let mut caps = BTreeMap::new();
        for project in state.projects() {
            let settings = projects
                .away_settings(PathBuf::from(&project.root))
                .await
                .map_err(lane_error)?;
            caps.insert(project.id.clone(), settings.stacking_cap);
        }
        Ok(caps)
    }

    pub async fn away(
        &self,
        _params: SwitchParams,
        caller: CallContext,
    ) -> Result<AwayResult, DecisionError> {
        self.call(true, async {
            let before = self.org.snapshot().await?;
            let by = caller_address(&before, &caller)?;
            // Only a call that can set the switch reads files; the policy
            // refuses any other caller and passes over a repeat.
            let stacking_caps = if by == Address::Owner && before.owner().away_since.is_none() {
                self.stacking_caps(&before).await?
            } else {
                BTreeMap::new()
            };
            let events = self
                .org
                .execute(
                    OrgCommand::Owner(OwnerCommand::Away { by, stacking_caps }),
                    self.clock.now()?,
                )
                .await?;
            let state = self.org.snapshot().await?;
            let away_since = state.owner().away_since.ok_or_else(|| {
                DecisionError::Refused("the owner was set back while this ran; try again".into())
            })?;
            Ok(AwayResult {
                changed: !events.is_empty(),
                away_since,
                stacking_caps: state
                    .projects()
                    .map(|p| (p.id.clone(), state.owner().stacking_cap(Some(&p.id))))
                    .collect(),
                // As the call found it: the bridge is already taking them.
                pending_count: count(&before, DecisionStatus::Pending),
            })
        })
        .await
    }

    pub async fn back(
        &self,
        _params: SwitchParams,
        caller: CallContext,
    ) -> Result<BackResult, DecisionError> {
        self.call(true, self.prepare_back(_params, caller)).await
    }
}

/// Always advertised; a disabled service never sets the switch.
pub fn register_away_methods(
    router: &mut MethodRouter,
    decisions: DecisionHandle,
) -> Result<(), MethodError> {
    macro_rules! register {
        ($name:literal, $method:ident) => {{
            let decisions = decisions.clone();
            router.register($name, Duration::from_secs(22), move |caller, params| {
                let decisions = decisions.clone();
                async move {
                    let result = decisions.$method(parse_params(params)?, caller).await?;
                    serde_json::to_value(result)
                        .map_err(|error| MethodError::new(error_code::INTERNAL, error.to_string()))
                }
            })?;
        }};
    }
    register!("owner.away", away);
    register!("owner.back", back);
    register!("owner.digest", digest);
    register!("owner.relay", relay);
    Ok(())
}

/// While the owner is away: one report for each pending decision the away
/// table would record. A decision that waits gets none.
fn pending_reports(state: &OrgState) -> Vec<OrgCommand> {
    if state.owner().away_since.is_none() {
        return Vec::new();
    }
    state
        .decisions()
        .filter(|d| d.status == DecisionStatus::Pending)
        .filter(|d| !matches!(d.away_step(true), AwayStep::Wait(_)))
        .map(|d| OrgCommand::Owner(OwnerCommand::ApplyAway { decision_id: d.id }))
        .collect()
}

/// Submit the reports. A refused report is logged and passed over: one
/// decision the policy will not take must not stop the answer bridge.
pub(super) async fn apply_pending(
    org: &OrgHandle,
    clock: &MessageClock,
) -> Result<(), DecisionError> {
    for report in pending_reports(&org.snapshot().await?) {
        match org.execute(report, clock.now()?).await {
            Err(OrgServiceError::Refused(refusal)) => {
                tracing::warn!(%refusal, "Away report refused; the decision stays pending");
            }
            result => {
                result?;
            }
        }
    }
    Ok(())
}

/// What a requester is told about its decision. Status decides for a decision
/// that was taken; a waiting one is only announced while the owner is away.
pub(super) fn notice(decision: &Decision, away: bool) -> Option<MessageContent> {
    let Address::Agent(_) = &decision.request.requester else {
        return None;
    };
    let id = decision.id;
    let (kind, body) = match decision.status {
        DecisionStatus::TakenProvisionally => {
            let taken = decision.provisional.as_ref()?;
            let body = if let Some(seat) = &taken.relayed_by {
                let next = if decision.class() == atm_org::DecisionClass::Detail {
                    " Carry on; he may still overrule it.".into()
                } else {
                    format!(" Finish your task in progress on this option. Start nothing further on it: a task built on it names {id} in rests_on and waits until the owner has confirmed {id}.")
                };
                format!("Decision {id} taken provisionally: {}\nRelayed by {seat} as the owner's spoken answer; he has not confirmed it himself.{next}", taken.option)
            } else if taken.by == decision.request.requester {
                format!(
                    "Decision {id} is yours to decide: recorded as {}, your recommended option.\nThe owner is away and sees it when he is back.",
                    taken.option
                )
            } else {
                format!(
                    "Decision {id} taken provisionally: {}\nThe owner is away. Finish your task in progress on this option. Start nothing further on it: a task built on it names {id} in rests_on and waits until the owner has seen {id}.",
                    taken.option
                )
            };
            (MessageKind::Answer, body)
        }
        DecisionStatus::Pending if away => {
            let body = match decision.away_step(true) {
                AwayStep::Wait(AwayWait::Irreversible) => format!(
                    "Decision {id} waits for the owner: he is away, and it cannot be undone or reaches outside.\nOnly work that depends on it waits. Carry on with other work."
                ),
                AwayStep::Wait(AwayWait::NoRecommendation) => format!(
                    "Decision {id} waits for the owner: he is away, and it has no recommended option.\nTo have it handled while he is away, withdraw it and ask again with a recommended option."
                ),
                // About to be recorded, or not away after all.
                AwayStep::Wait(AwayWait::OwnerPresent)
                | AwayStep::TakeProvisionally
                | AwayStep::AgentDecides => return None,
            };
            (MessageKind::Note, body)
        }
        _ => return None,
    };
    Some(MessageContent {
        delivery_notice: None,
        from: Address::System,
        to: decision.request.requester.clone(),
        kind,
        body,
        attachments: vec![],
        task: None,
        post: None,
    })
}

/// The notices no `msg.sent` carries yet: the previous bridge's list;
/// `decision_steps::replies` now decides. Kept as the rollback tests' oracle.
#[cfg(test)]
pub(super) fn missing_notices(state: &OrgState) -> Vec<MessageContent> {
    let away = state.owner().away_since.is_some();
    state
        .decisions()
        .filter_map(|decision| notice(decision, away))
        .filter(|content| {
            !state
                .messages()
                .any(|m| m.content.same_except_post(content))
        })
        .collect()
}
