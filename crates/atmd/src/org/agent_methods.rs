//! Agent RPC registration; schemas and service effects stay separate.

use super::agents::{AgentError, AgentHandle};
use crate::server::methods::{control_error, parse_params, MethodError, MethodRouter};
use atm_protocol::error_code;
use std::time::Duration;

impl From<AgentError> for MethodError {
    fn from(error: AgentError) -> Self {
        use super::OrgServiceError;
        use crate::control::reservation::ReservationError;
        use atm_org::{ApplyError, Refusal};
        let message = error.to_string();
        let code = match error {
            AgentError::NotImplemented { .. } | AgentError::Unavailable(_) => {
                error_code::UNAVAILABLE
            }
            AgentError::InvalidParams(_) => error_code::INVALID_PARAMS,
            AgentError::NotFound(_) => error_code::NOT_FOUND,
            AgentError::Conflict(_) => error_code::CONFLICT,
            AgentError::Refused(_) => error_code::REFUSED,
            AgentError::Timeout => error_code::TIMEOUT,
            AgentError::Internal(_) => error_code::INTERNAL,
            AgentError::Reservation(reservation) => match reservation {
                ReservationError::Busy { .. } => error_code::CONFLICT,
                ReservationError::TagMismatch { .. }
                | ReservationError::StaleReservation
                | ReservationError::ScreenChanged { .. }
                | ReservationError::InvalidObservation { .. } => error_code::REFUSED,
                ReservationError::PaneGone { .. } => error_code::NOT_FOUND,
                ReservationError::InvalidRequest { .. } => error_code::INVALID_PARAMS,
                ReservationError::Unavailable | ReservationError::NotImplemented { .. } => {
                    error_code::UNAVAILABLE
                }
                ReservationError::Timeout => error_code::TIMEOUT,
                ReservationError::Control(control) => return control_error(control.into()),
            },
            AgentError::Organisation(org) => match org {
                OrgServiceError::Unavailable(_) => error_code::UNAVAILABLE,
                OrgServiceError::Timeout => error_code::TIMEOUT,
                OrgServiceError::InvalidParams(_) => error_code::INVALID_PARAMS,
                OrgServiceError::Oversized(_) => error_code::REFUSED,
                OrgServiceError::Refused(refusal) => match refusal {
                    Refusal::Invariant(
                        ApplyError::UnknownProject(_)
                        | ApplyError::UnknownAgent(_)
                        | ApplyError::UnknownMessage(_),
                    ) => error_code::NOT_FOUND,
                    Refusal::Invariant(ApplyError::InvalidId(_)) | Refusal::InvalidValue { .. } => {
                        error_code::INVALID_PARAMS
                    }
                    Refusal::Invariant(
                        ApplyError::PaneOccupied { .. } | ApplyError::DuplicateMessage(_),
                    )
                    | Refusal::AgentAlreadyBound { .. }
                    | Refusal::ProjectRootConflict { .. } => error_code::CONFLICT,
                    _ => error_code::REFUSED,
                },
            },
        };
        MethodError::new(code, message)
    }
}

/// Always advertised, including on daemons with explicitly disabled agents.
pub fn register_agent_methods(
    router: &mut MethodRouter,
    agents: AgentHandle,
) -> Result<(), MethodError> {
    // Each expansion still parses the method's own deny_unknown_fields schema.
    macro_rules! register {
        ($name:literal, $seconds:literal, $method:ident $(, $caller:ident)?) => {{
            let agents = agents.clone();
            router.register($name, Duration::from_secs($seconds), move |_caller, params| {
                let agents = agents.clone();
                async move {
                    let result = agents.$method(parse_params(params)? $(, { let $caller = _caller; $caller })?).await?;
                    serde_json::to_value(result)
                        .map_err(|error| MethodError::new(error_code::INTERNAL, error.to_string()))
                }
            })?;
        }};
    }
    register!("agent.up", 75, up, caller);
    register!("agent.adopt", 45, adopt);
    register!("agent.clear", 135, clear);
    register!("agent.respawn", 75, respawn, caller);
    register!("agent.down", 45, down);
    register!("agent.list", 10, list);
    Ok(())
}
