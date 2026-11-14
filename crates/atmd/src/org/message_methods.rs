//! Strict message RPC registration, independent of runtime startup.

use super::messages::{AckParams, Address, InboxParams, MessageError, MessageHandle, SendParams};
use crate::server::methods::{parse_params, MethodError, MethodRouter};
use atm_protocol::error_code;
use std::time::Duration;

impl From<MessageError> for MethodError {
    fn from(error: MessageError) -> Self {
        let message = error.to_string();
        let code = match error {
            MessageError::InvalidParams(_) => error_code::INVALID_PARAMS,
            MessageError::NotFound(_) => error_code::NOT_FOUND,
            MessageError::Conflict(_) => error_code::CONFLICT,
            MessageError::Refused(_) => error_code::REFUSED,
            MessageError::Timeout => error_code::TIMEOUT,
            MessageError::Unavailable(_) => error_code::UNAVAILABLE,
            MessageError::Internal(_) => error_code::INTERNAL,
            MessageError::Organisation(super::OrgServiceError::Refused(
                atm_org::Refusal::InvalidDeliveryState { .. },
            )) => error_code::CONFLICT,
            // Reuse the existing transparent org/reservation mappings rather than
            // maintaining a second error-policy table for the same dependencies.
            MessageError::Organisation(error) => {
                return super::agents::AgentError::Organisation(error).into();
            }
            MessageError::Reservation(error) => {
                return super::agents::AgentError::Reservation(error).into();
            }
        };
        MethodError::new(code, message)
    }
}

fn recipient(address: Option<&Address>) -> Result<(), MessageError> {
    if matches!(address, Some(Address::System)) {
        return Err(MessageError::Refused(
            "system is not a message recipient or inbox".into(),
        ));
    }
    Ok(())
}

fn validate_send(p: &SendParams) -> Result<(), MessageError> {
    let invalid = if p.body.is_empty() && p.attachments.is_empty() {
        Some("empty body requires an attachment")
    } else if p.body.len() > 16 * 1024 {
        Some("body exceeds 16384 UTF-8 bytes")
    } else if p.attachments.len() > 32 {
        Some("at most 32 attachments are allowed")
    } else if p.attachments.iter().any(String::is_empty) {
        Some("attachments must not be empty")
    } else if p.attachments.iter().any(|path| path.len() > 4096) {
        Some("attachment exceeds 4096 UTF-8 bytes")
    } else if p
        .task
        .as_ref()
        .is_some_and(|task| task.to_string().len() > 256)
    {
        Some("task exceeds 256 UTF-8 bytes")
    } else {
        None
    };
    if let Some(reason) = invalid {
        return Err(MessageError::InvalidParams(reason.into()));
    }
    recipient(Some(&p.to))
}

fn validate_inbox(p: &InboxParams) -> Result<(), MessageError> {
    let invalid = if !(1..=1000).contains(&p.limit) {
        Some("limit must be between 1 and 1000")
    } else if p.id.is_some() && p.after.is_some() {
        Some("id and after are mutually exclusive")
    } else if p.count_only && (p.id.is_some() || p.after.is_some()) {
        Some("count_only excludes id and after")
    } else {
        None
    };
    if let Some(reason) = invalid {
        return Err(MessageError::InvalidParams(reason.into()));
    }
    recipient(p.recipient.as_ref())
}

fn validate_ack(p: &AckParams) -> Result<(), MessageError> {
    if !(1..=1000).contains(&p.ids.len()) {
        return Err(MessageError::InvalidParams(
            "ids must contain 1 to 1000 messages".into(),
        ));
    }
    let mut distinct = std::collections::HashSet::with_capacity(p.ids.len());
    if p.ids.iter().any(|id| !distinct.insert(id)) {
        return Err(MessageError::InvalidParams("ids must be distinct".into()));
    }
    Ok(())
}

/// Always advertised; valid requests to a disabled service never claim success.
pub fn register_message_methods(
    router: &mut MethodRouter,
    messages: MessageHandle,
) -> Result<(), MethodError> {
    macro_rules! register {
        ($name:literal, $seconds:literal, $method:ident $(, $validate:expr)?) => {{
            let messages = messages.clone();
            router.register($name, Duration::from_secs($seconds), move |caller, params| {
                let messages = messages.clone();
                async move {
                    let p = parse_params(params)?;
                    $( $validate(&p)?; )?
                    let result = messages.$method(p, caller).await?;
                    serde_json::to_value(result)
                        .map_err(|error| MethodError::new(error_code::INTERNAL, error.to_string()))
                }
            })?;
        }};
    }
    // Each ordinary call: 8 s service budget + 2 s router margin = 10 s.
    register!("msg.send", 10, send, validate_send);
    register!("msg.inbox", 10, inbox, validate_inbox);
    register!("msg.ack", 10, ack, validate_ack);
    // Retry: message_delivery::{ATTEMPT_WORK_BUDGET, ATTEMPT_CLEANUP_BUDGET}
    // are 30 + 10 s; + 5 s snapshot + 6 s execute + 4 s overhead = 55 s
    // MESSAGE_RESOLUTION_BUDGET, leaving 5 s before this 60 s router ceiling.
    register!("msg.retry", 60, retry);
    // Give-up uses the same ATTEMPT_WORK_BUDGET + ATTEMPT_CLEANUP_BUDGET:
    // 30 + 10 + 5 snapshot + 6 execute + 4 overhead = MESSAGE_RESOLUTION_BUDGET
    // (55 s), plus 5 s router margin. See message_delivery's constants.
    register!("msg.give_up", 60, give_up);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::methods::{CallContext, CallerId};
    use serde_json::{json, Value};

    async fn check(method: &str, invalid: Value, valid: Value) {
        let mut router = MethodRouter::default();
        register_message_methods(&mut router, MessageHandle::disabled("test")).unwrap();
        for (params, code) in [(invalid, "invalid_params"), (valid, "unavailable")] {
            let error = router
                .call(
                    CallContext {
                        client_id: CallerId("bounds".into()),
                        from_pane: None,
                    },
                    method,
                    params,
                )
                .await
                .unwrap_err();
            assert_eq!(error.0.code, code, "{method}: {}", error.0.message);
        }
    }

    macro_rules! bound {
        ($name:ident, $method:literal, $invalid:expr, $valid:expr) => {
            #[tokio::test]
            async fn $name() {
                check($method, $invalid, $valid).await;
            }
        };
    }
    bound!(
        send_empty_body_requires_attachment,
        "msg.send",
        json!({"to":"owner","body":""}),
        json!({"to":"owner","body":"","attachments":["literal/path"]})
    );
    bound!(
        send_whitespace_only_body_needs_no_attachment,
        "msg.send",
        json!({"to":"owner","body":""}),
        json!({"to":"owner","body":" \n\t "})
    );
    bound!(
        send_body_has_16_kib_utf8_byte_limit,
        "msg.send",
        json!({"to":"owner","body":format!("{}x", "é".repeat(8192))}),
        json!({"to":"owner","body":"é".repeat(8192)})
    );
    bound!(
        send_has_at_most_32_attachments,
        "msg.send",
        json!({"to":"owner","body":"x","attachments":vec!["p";33]}),
        json!({"to":"owner","body":"x","attachments":vec!["p";32]})
    );
    bound!(
        send_attachment_must_not_be_empty,
        "msg.send",
        json!({"to":"owner","body":"x","attachments":["p",""]}),
        json!({"to":"owner","body":"x","attachments":["p"," "]})
    );
    bound!(
        send_attachment_has_4096_byte_limit,
        "msg.send",
        json!({"to":"owner","body":"x","attachments":[format!("{}x", "é".repeat(2048))]}),
        json!({"to":"owner","body":"x","attachments":["é".repeat(2048)]})
    );
    bound!(
        send_task_has_256_byte_limit,
        "msg.send",
        json!({"to":"owner","body":"x","task":format!("{}x", "é".repeat(128))}),
        json!({"to":"owner","body":"x","task":"é".repeat(128)})
    );
    bound!(
        inbox_limit_must_be_positive,
        "msg.inbox",
        json!({"limit":0,"id":"m1"}),
        json!({"limit":1,"id":"m1"})
    );
    bound!(
        inbox_limit_is_at_most_1000,
        "msg.inbox",
        json!({"limit":1001,"id":"m1"}),
        json!({"limit":1000,"id":"m1"})
    );
    bound!(
        inbox_id_excludes_after,
        "msg.inbox",
        json!({"id":"m1","after":"m2"}),
        json!({"id":"m1"})
    );
    bound!(
        inbox_count_only_excludes_id,
        "msg.inbox",
        json!({"count_only":true,"id":"m1"}),
        json!({"count_only":true})
    );
    bound!(
        inbox_count_only_excludes_after,
        "msg.inbox",
        json!({"count_only":true,"after":"m1"}),
        json!({"count_only":true})
    );
    bound!(
        ack_requires_at_least_one_id,
        "msg.ack",
        json!({"ids":[]}),
        json!({"ids":["m1"]})
    );
    bound!(
        ack_accepts_at_most_1000_ids,
        "msg.ack",
        json!({"ids":(1..=1001).map(|n|format!("m{n}")).collect::<Vec<_>>()}),
        json!({"ids":(1..=1000).map(|n|format!("m{n}")).collect::<Vec<_>>()})
    );
    bound!(
        ack_rejects_duplicate_ids,
        "msg.ack",
        json!({"ids":["m1","m2","m1"]}),
        json!({"ids":["m1","m2"]})
    );
}
