//! Strict decision RPC schemas and error classification; no policy duplication.
use super::decisions::{
    DecisionError, DecisionHandle, DecisionKind, ListParams, RequestParams, ResolveParams,
    WithdrawParams,
};
use crate::server::methods::{parse_params, MethodError, MethodRouter};
use atm_protocol::error_code;
use std::time::Duration;

impl From<DecisionError> for MethodError {
    fn from(error: DecisionError) -> Self {
        let message = error.to_string();
        let code = match error {
            DecisionError::Organisation(error) => {
                return super::agents::AgentError::Organisation(error).into()
            }
            DecisionError::Caller(error) => return error.into(),
            DecisionError::InvalidParams(_) => error_code::INVALID_PARAMS,
            DecisionError::Refused(_) => error_code::REFUSED,
            DecisionError::NotFound(_) => error_code::NOT_FOUND,
            DecisionError::Unavailable(_) => error_code::UNAVAILABLE,
            DecisionError::Timeout => error_code::TIMEOUT,
            DecisionError::Internal(_) => error_code::INTERNAL,
        };
        MethodError::new(code, message)
    }
}
fn invalid(reason: &str) -> DecisionError {
    DecisionError::InvalidParams(reason.into())
}
fn note(value: Option<&str>) -> Result<(), DecisionError> {
    if value.is_some_and(|v| v.len() > 8192) {
        return Err(invalid("note or reason exceeds 8192 UTF-8 bytes"));
    }
    Ok(())
}
fn option(value: &str) -> Result<(), DecisionError> {
    if value.is_empty() || value.len() > 1024 {
        return Err(invalid("option must contain 1 to 1024 UTF-8 bytes"));
    }
    Ok(())
}
fn validate_request(p: &RequestParams) -> Result<(), DecisionError> {
    if p.kind == DecisionKind::Unknown {
        return Err(invalid("kind must be known"));
    }
    if p.title.is_empty() || p.title.len() > 512 {
        return Err(invalid("title must contain 1 to 512 UTF-8 bytes"));
    }
    if p.context.len() > 16384 {
        return Err(invalid("context exceeds 16384 UTF-8 bytes"));
    }
    if !(2..=32).contains(&p.options.len()) {
        return Err(invalid("options must contain 2 to 32 entries"));
    }
    for value in &p.options {
        option(value)?;
    }
    if p.files.len() > 32 || p.files.iter().any(|f| f.is_empty() || f.len() > 4096) {
        return Err(invalid(
            "files must contain at most 32 nonempty paths of at most 4096 UTF-8 bytes",
        ));
    }
    Ok(())
}
fn validate_list(p: &ListParams) -> Result<(), DecisionError> {
    if !(1..=1000).contains(&p.limit) {
        return Err(invalid("limit must be between 1 and 1000"));
    }
    if p.statuses.as_ref().is_some_and(Vec::is_empty) {
        return Err(invalid("statuses must not be empty"));
    }
    if p.count_only && (p.after.is_some() || p.statuses.is_some()) {
        return Err(invalid("count_only excludes after and statuses"));
    }
    Ok(())
}
fn validate_resolve(p: &ResolveParams) -> Result<(), DecisionError> {
    option(&p.option)?;
    note(p.note.as_deref())
}
fn validate_withdraw(p: &WithdrawParams) -> Result<(), DecisionError> {
    note(p.reason.as_deref())
}

/// Always advertised; a disabled service never accepts a valid mutation.
pub fn register_decision_methods(
    router: &mut MethodRouter,
    decisions: DecisionHandle,
) -> Result<(), MethodError> {
    macro_rules! register {
        ($name:literal, $method:ident $(, $validate:expr)?) => {{
            let decisions = decisions.clone();
            router.register($name, Duration::from_secs(22), move |caller, params| {
                let decisions = decisions.clone();
                async move {
                    let p = parse_params(params)?;
                    $( $validate(&p)?; )?
                    let result = decisions.$method(p, caller).await?;
                    serde_json::to_value(result).map_err(|error| MethodError::new(error_code::INTERNAL, error.to_string()))
                }
            })?;
        }};
    }
    register!("decision.request", request, validate_request);
    register!("decision.list", list, validate_list);
    register!("decision.get", get);
    register!("decision.resolve", resolve, validate_resolve);
    register!("decision.withdraw", withdraw, validate_withdraw);
    Ok(())
}
