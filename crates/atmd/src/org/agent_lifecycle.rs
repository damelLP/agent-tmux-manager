//! The effects of the named-agent lifecycle flows.
//!
//! Each flow runs inside one reservation of its agent: it reads the committed
//! organisation state again, performs its pane effects through the
//! reservation, and reports through the organisation log. What a flow decides
//! from a screen lives in `agent_flow`, which is pure.

use super::agents::{AgentError, AgentRuntime, DownResult};
use crate::control::reservation::{PaneId, Reservation};
use atm_org::{AgentId, AgentStatus, OrgCommand};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
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
