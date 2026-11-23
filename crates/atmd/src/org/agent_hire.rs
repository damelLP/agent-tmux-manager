//! Department admission for named `agent.up`; the org writer owns decisions.

use super::{
    agent_context::manager_address, agent_project::PreparedAgent, agents::AgentError,
    department_directory::DepartmentFiles, message_inbox::caller_address,
    reporting_line::ReportingFiles, OrgHandle, OrgServiceError,
};
use crate::server::methods::CallContext;
use atm_org::{
    department::{hire_check, role_model, HireCheck},
    Address, DecisionCommand, DecisionId, DecisionKind, DecisionRequest, DecisionStatus,
    OrgCommand,
};
use chrono::Utc;

#[derive(Debug, Clone)]
pub(super) struct Hiring {
    pub(super) departments: DepartmentFiles,
    pub(super) reporting: ReportingFiles,
}

impl Hiring {
    pub(super) async fn check(
        &self,
        org: &OrgHandle,
        prepared: &PreparedAgent,
        context: &CallContext,
        approval: Option<DecisionId>,
    ) -> Result<Option<String>, AgentError> {
        if context.from_pane.is_none() {
            return Ok(None);
        }
        let directory = self.departments.read().await;
        let id = &prepared.agent_id;
        let covered = directory
            .departments
            .iter()
            .any(|dept| dept.enforces(&id.project));
        let incomplete = !directory.problems.is_empty();
        if !covered && !incomplete {
            return Ok(None);
        }
        let state = org.snapshot().await?;
        let Address::Agent(caller) = caller_address(&state, context)
            .map_err(|error| AgentError::InvalidParams(error.to_string()))?
        else {
            return Ok(None);
        };
        let warning = incomplete.then(|| {
            let check = if covered {
                "checked loaded departments"
            } else {
                "hiring was not checked"
            };
            let warning = format!("department policy incomplete; {check} for {}", id.project);
            tracing::warn!(%warning);
            warning
        });
        if !covered {
            return Ok(warning);
        }

        // Do not request a hire for a seat Up would refuse anyway. This is
        // read-only; the actual Up still validates against the live writer.
        let now = Utc::now();
        state
            .decide(
                &OrgCommand::Up {
                    agent_id: id.clone(),
                    role: prepared.role.clone(),
                },
                now,
            )
            .map_err(OrgServiceError::from)?;
        let refuse =
            |reason: &str| AgentError::Refused(format!("agent {id} was not started: {reason}"));
        let Some(Address::Agent(manager)) = manager_address(id, Some(&prepared.role)) else {
            return Err(refuse(
                "the role must name a valid seat as manager; ask the owner",
            ));
        };
        let cycle = || {
            refuse("the role would make a cycle in the manager line; fix the role managers first")
        };
        if &manager == id {
            return Err(cycle());
        }
        let ancestors = self
            .reporting
            .ancestors(state.clone(), manager.clone())
            .await
            .map_err(|error| refuse(&format!("cannot verify the role's manager line: {error}")))?;
        if ancestors.contains(id) {
            return Err(cycle());
        }
        if caller != manager && !ancestors.contains(&caller) {
            return Err(refuse(
                "caller is not in the role's manager line; ask its manager",
            ));
        }

        let title = format!("Hire {id}");
        let path = prepared.role_path();
        let path = path
            .to_str()
            .ok_or_else(|| refuse("the role path is not UTF-8"))?;
        let hire_for_seat = |request: &DecisionRequest| {
            request.kind == DecisionKind::Hire && request.title == title
        };
        let attaches_role =
            |request: &DecisionRequest| request.files.iter().any(|file| file == path);
        if let Some(approval) = approval {
            if state.decision(&approval).is_some_and(|decision| {
                hire_for_seat(&decision.request)
                    && attaches_role(&decision.request)
                    && decision.status == DecisionStatus::Resolved
                    && decision.provisional.is_none()
                    && decision.resolution.as_ref().is_some_and(|answer| {
                        answer.by == Address::Owner && answer.option == "approve"
                    })
            }) {
                return Ok(warning);
            }
            return Err(refuse(&format!(
                "{approval} is not a direct owner hire approval for this seat and role file"
            )));
        }
        let HireCheck::NeedsApproval { reason: refusal } = hire_check(
            &directory.departments,
            Some(&caller),
            id,
            &prepared.role,
            &state,
        ) else {
            return Ok(warning);
        };
        if let Some(pending) = state.decisions().find(|decision| {
            decision.status == DecisionStatus::Pending && hire_for_seat(&decision.request)
        }) {
            let mut note = format!("{refusal}; hire decision {} is pending", pending.id);
            if !attaches_role(&pending.request) {
                note.push_str(" but its role attachment no longer matches; ask to withdraw it before retrying");
            }
            return Err(refuse(&note));
        }

        // The internal writer bypasses the decision RPC's attachment-size
        // validation. Keep the exact path or refuse; a truncated path cannot approve Up.
        if path.len() > 4096 {
            return Err(refuse(&format!(
                "{refusal}; role path exceeds the 4096-byte decision attachment limit"
            )));
        }
        let model: String = role_model(&prepared.role)
            .chars()
            .take(128)
            .flat_map(char::escape_default)
            .collect();
        let events = org.execute(OrgCommand::Decision(DecisionCommand::Request(DecisionRequest {
            requester: Address::Agent(caller),
            kind: DecisionKind::Hire,
            title,
            context: format!("{refusal}; manager: {manager}; model: {model}"),
            files: vec![path.to_owned()],
            options: vec!["approve".into(), "decline".into()],
            recommended: None,
            may_be_undone: true,
            changes_intent: true,
            deadline: None,
        })), now).await.map_err(|error| match error {
            OrgServiceError::Refused(_) => refuse(&format!("{refusal}; hire request refused: {error}")),
            _ => AgentError::Unavailable(format!("agent {id} was not started; hire request unconfirmed: {error}; check atm decisions list before retrying")),
        })?;
        let Some(record) = events.first() else {
            return Err(AgentError::Internal(format!("agent {id} was not started; hire request unconfirmed; check atm decisions list before retrying")));
        };
        Err(refuse(&format!(
            "{refusal}; hire decision {} is pending",
            DecisionId::new(record.seq)
        )))
    }
}
