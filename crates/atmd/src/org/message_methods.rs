//! Strict message RPC registration, independent of runtime startup.

use super::messages::{Address, InboxParams, MessageError, MessageHandle, SendParams};
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
    register!("msg.send", 10, send, |p: &SendParams| recipient(Some(
        &p.to
    )));
    register!("msg.inbox", 10, inbox, |p: &InboxParams| recipient(
        p.recipient.as_ref()
    ));
    register!("msg.ack", 10, ack);
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
