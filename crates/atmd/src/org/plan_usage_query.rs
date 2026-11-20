//! Account-level readings follow the existing local owner admission convention.

use super::{agents::AgentError, message_inbox::caller_address, OrgHandle};
use crate::{
    plan_usage::PlanUsage,
    server::methods::{parse_params, MethodError, MethodRouter},
};
use atm_org::Address;
use atm_protocol::error_code;
use chrono::Utc;
use serde::Deserialize;
use std::time::Duration;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UsageListParams {}

pub(crate) fn register(
    router: &mut MethodRouter,
    org: OrgHandle,
    usage: Option<PlanUsage>,
) -> Result<(), MethodError> {
    router.register(
        "usage.list",
        Duration::from_secs(6),
        move |caller, params| {
            let org = org.clone();
            let usage = usage.clone();
            async move {
                let _: UsageListParams = parse_params(params)?;
                let state = org.snapshot().await.map_err(AgentError::Organisation)?;
                if caller_address(&state, &caller)? != Address::Owner {
                    return Err(MethodError::new(
                        error_code::REFUSED,
                        "only the owner may read plan usage",
                    ));
                }
                let samples = usage.map_or_else(Vec::new, |usage| usage.list(Utc::now()));
                serde_json::to_value(samples)
                    .map_err(|error| MethodError::new(error_code::INTERNAL, error.to_string()))
            }
        },
    )
}
