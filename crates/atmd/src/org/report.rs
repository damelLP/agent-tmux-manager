//! Read-only, ledger-backed reports over one committed organisation snapshot.
use super::{message_inbox::caller_address, reporting_line, OrgHandle};
use crate::server::methods::{parse_params, MethodError, MethodRouter};
use atm_org::{Address, ReportError, ReportParams, ReportVisibility};
use atm_protocol::error_code;
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::{collections::BTreeSet, time::Duration};

#[derive(Serialize)]
struct ResultEnvelope {
    seq: u64,
    #[serde(flatten)]
    report: atm_org::Report,
}

pub(crate) fn register(
    router: &mut MethodRouter,
    org: OrgHandle,
    files: reporting_line::ReportingFiles,
    now: fn() -> DateTime<Utc>,
) -> Result<(), MethodError> {
    // Match decision.list, which uses the same retained reporting reader.
    router.register("org.report", Duration::from_secs(22), move |caller, params| {
        let org = org.clone();
        let files = files.clone();
        async move {
            let params: ReportParams = parse_params(params)?;
            if let Some(id) = &params.after {
                super::tasks::sequence(id)?;
            }
            let (seq, state) = org.report_snapshot().await.map_err(super::agents::AgentError::Organisation)?;
            let visible = match caller_address(&state, &caller)? {
                Address::Agent(reader) => {
                    let mut agents = BTreeSet::new();
                    for agent in state.agents().filter(|agent| agent.id != reader) {
                        match files.ancestors(state.clone(), agent.id.clone()).await {
                            Ok(ancestors) if ancestors.contains(&reader) => { agents.insert(agent.id.clone()); }
                            Ok(_) => (),
                            Err(error) if error.0.code == error_code::REFUSED => {
                                tracing::warn!(agent_id = %agent.id, %error, "Reporting line unavailable for report visibility");
                            }
                            Err(error) => return Err(error),
                        }
                    }
                    ReportVisibility::Reports { reader, agents }
                }
                _ => ReportVisibility::All,
            };
            let report = atm_org::report(&state, &params, &visible, now()).map_err(|error| {
                let code = match &error {
                    ReportError::InvalidParams(_) => error_code::INVALID_PARAMS,
                    ReportError::NotFound(_) => error_code::NOT_FOUND,
                    ReportError::Internal(_) => error_code::INTERNAL,
                };
                MethodError::new(code, error.to_string())
            })?;
            let value = serde_json::to_value(ResultEnvelope { seq, report })
                .map_err(|e| MethodError::new(error_code::INTERNAL, e.to_string()))?;
            let bytes = serde_json::to_vec(&value)
                .map_err(|e| MethodError::new(error_code::INTERNAL, e.to_string()))?.len();
            if bytes > 512 * 1024 {
                return Err(MethodError::new(error_code::REFUSED, "report exceeds 512 KiB byte budget"));
            }
            Ok(value)
        }
    })
}

#[cfg(test)]
#[path = "report_tests.rs"]
mod tests;
