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
            DecisionError::Conflict(_) => error_code::CONFLICT,
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
fn validate_get(p: &super::decisions::GetParams) -> Result<(), DecisionError> {
    if p.redo_limit
        .is_some_and(|limit| !(1..=1000).contains(&limit))
    {
        return Err(invalid("redo_limit must be between 1 and 1000"));
    }
    if let Some(after) = &p.redo_after {
        super::tasks::sequence(after)
            .map_err(|error| DecisionError::InvalidParams(error.to_string()))?;
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
fn parse_resolve(params: serde_json::Value) -> Result<ResolveParams, MethodError> {
    if params.get("relay").and_then(serde_json::Value::as_bool) == Some(true)
        && ["note", "standing_rule"]
            .iter()
            .any(|key| params.get(key).is_some())
    {
        return Err(invalid("relays cannot include note or standing_rule").into());
    }
    let p = parse_params(params)?;
    validate_resolve(&p)?;
    Ok(p)
}

/// Always advertised; a disabled service never accepts a valid mutation.
pub fn register_decision_methods(
    router: &mut MethodRouter,
    decisions: DecisionHandle,
) -> Result<(), MethodError> {
    macro_rules! register {
        ($name:literal, $method:ident, $parse:ident $(, $validate:expr)?) => {{
            let decisions = decisions.clone();
            router.register($name, Duration::from_secs(22), move |caller, params| {
                let decisions = decisions.clone();
                async move {
                    let p = $parse(params)?;
                    $( $validate(&p)?; )?
                    let result = decisions.$method(p, caller).await?;
                    serde_json::to_value(result).map_err(|error| MethodError::new(error_code::INTERNAL, error.to_string()))
                }
            })?;
        }};
    }
    register!("decision.request", request, parse_params, validate_request);
    register!("decision.list", list, parse_params, validate_list);
    register!("decision.get", get, parse_params, validate_get);
    register!("decision.resolve", resolve, parse_resolve);
    register!(
        "decision.withdraw",
        withdraw,
        parse_params,
        validate_withdraw
    );
    Ok(())
}

#[cfg(test)]
mod relay_tests {
    use super::*;
    use serde_json::{json, Value};

    // Break caught: serializing false breaks old daemons, or omitting the new
    // default breaks legacy callers. Plain note/rule bytes are historical.
    #[test]
    fn resolve_relay_is_additive_and_false_is_byte_compatible() {
        let plain = json!({"id":"d3","option":"yes","note":null,"standing_rule":false});
        let mut p: ResolveParams = parse_params(plain.clone()).unwrap();
        assert!(!p.relay);
        assert_eq!(serde_json::to_value(&p).unwrap(), plain);
        p.relay = true;
        // This typed encoding is not a valid relay RPC; send only id/option/relay.
        assert_eq!(serde_json::to_value(p).unwrap()["relay"], true);
        assert!(
            parse_resolve(json!({"id":"d3","option":"no","relay":true}))
                .unwrap()
                .relay
        );
    }

    // Break caught: Option/default deserialization erases forbidden null/false
    // key presence, or a caller can supply its own relayer identity.
    #[test]
    fn relay_params_reject_note_and_rule_key_presence() {
        for (key, value) in [
            ("note", Value::Null),
            ("note", json!("")),
            ("note", json!("go")),
            ("standing_rule", json!(false)),
            ("standing_rule", json!(true)),
            ("by", json!("owner")),
            ("relayed_by", json!("p/chief")),
        ] {
            let mut p = json!({"id":"d3","option":"no","relay":true});
            p[key] = value;
            assert_eq!(parse_resolve(p).unwrap_err().0.code, "invalid_params");
        }
        for p in [
            json!([]),
            json!({"id":"d3","option":"no","relay":"true"}),
            json!({"id":"d3","option":"","relay":true}),
        ] {
            assert_eq!(parse_resolve(p).unwrap_err().0.code, "invalid_params");
        }
        assert!(parse_resolve(
            json!({"id":"d3","option":"yes","note":"owner note","standing_rule":true,"relay":false})
        )
        .is_ok());
    }

    // Break caught: an internal typed caller bypasses the raw schema and enters
    // rule-file or final-answer work. Reject before even awaiting the service.
    #[tokio::test]
    async fn typed_relays_reject_note_and_rule_before_service_work() {
        for (note, standing_rule) in [(Some(String::new()), false), (None, true)] {
            let p = ResolveParams {
                id: atm_org::DecisionId::new(3),
                option: "yes".into(),
                note,
                standing_rule,
                relay: true,
            };
            let error = DecisionHandle::disabled("fixture")
                .resolve(
                    p,
                    crate::server::methods::CallContext {
                        client_id: crate::server::methods::CallerId("relay-test".into()),
                        from_pane: None,
                    },
                )
                .await
                .unwrap_err();
            assert!(
                matches!(error, DecisionError::InvalidParams(_)),
                "{error:?}"
            );
        }
    }
}
