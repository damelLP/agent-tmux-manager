//! Agent RPC registration; schemas and service effects stay separate.

use super::agents::{
    AdoptParams, AgentError, AgentHandle, ClearParams, DownParams, ListParams, RespawnParams,
    UpParams,
};
use crate::control::reservation::PaneId;
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
                ReservationError::Unavailable
                | ReservationError::NotImplemented { .. }
                | ReservationError::Quarantined { .. } => error_code::UNAVAILABLE,
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

fn validate_root(root: &std::path::Path) -> Result<(), AgentError> {
    if !root.is_absolute() || root.to_str().is_none() {
        return Err(AgentError::InvalidParams(
            "project_root must be a nonempty absolute UTF-8 path".into(),
        ));
    }
    Ok(())
}

fn validate_name(name: &str) -> Result<(), AgentError> {
    // Only validate the name here; A1 resolves the actual project identity.
    "validation"
        .parse()
        .and_then(|project| atm_org::AgentId::new(project, name))
        .map(|_| ())
        .map_err(|error| AgentError::InvalidParams(error.to_string()))
}

/// Always advertised, including on daemons with explicitly disabled agents.
pub fn register_agent_methods(
    router: &mut MethodRouter,
    agents: AgentHandle,
) -> Result<(), MethodError> {
    // Each expansion still parses the method's own deny_unknown_fields schema.
    macro_rules! register {
        ($name:literal, $seconds:literal, $method:ident $(, $caller:ident)?;
         $params:ident: $ty:ty => $validate:block) => {{
            let agents = agents.clone();
            router.register($name, Duration::from_secs($seconds), move |_caller, params| {
                let agents = agents.clone();
                async move {
                    let $params: $ty = parse_params(params)?;
                    $validate
                    let result = agents.$method($params $(, { let $caller = _caller; $caller })?).await?;
                    serde_json::to_value(result)
                        .map_err(|error| MethodError::new(error_code::INTERNAL, error.to_string()))
                }
            })?;
        }};
    }
    register!("agent.up", 75, up, caller; p: UpParams => {
        validate_root(&p.project_root)?;
        validate_name(&p.name)?;
    });
    register!("agent.adopt", 45, adopt; p: AdoptParams => {
        validate_root(&p.project_root)?;
        validate_name(&p.name)?;
        PaneId::parse(p.pane.clone()).map_err(AgentError::from)?;
    });
    register!("agent.clear", 135, clear; p: ClearParams => {
        if !(1..=120_000).contains(&p.timeout_ms) {
            return Err(AgentError::InvalidParams("timeout_ms must be in 1..=120000".into()).into());
        }
    });
    register!("agent.respawn", 75, respawn, caller; p: RespawnParams => {});
    register!("agent.down", 45, down; p: DownParams => {});
    register!("agent.list", 10, list; p: ListParams => {
        if let Some(root) = &p.project_root {
            validate_root(root)?;
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::methods::{CallContext, CallerId};
    use serde_json::{json, Value};

    async fn code(method: &str, params: Value) -> String {
        let mut router = MethodRouter::default();
        register_agent_methods(&mut router, AgentHandle::disabled("off")).unwrap();
        router
            .call(
                CallContext {
                    client_id: CallerId("validation".into()),
                    from_pane: None,
                },
                method,
                params,
            )
            .await
            .unwrap_err()
            .0
            .code
    }

    #[test]
    fn reservation_errors_keep_codes_and_actionable_display_text() {
        use crate::control::{
            reservation::{ExpectedTag, ReservationError as R},
            ControlError,
        };
        let pane = || PaneId::parse("%7").unwrap();
        let agent = || "project/lead".parse().unwrap();
        for (error, code, message) in [
            (
                R::ScreenChanged { pane: pane() },
                "refused",
                "pane %7 no longer passes the approved screen check; nothing was sent",
            ),
            (
                R::Quarantined {
                    agent: agent(),
                    pane: Some(pane()),
                },
                "unavailable",
                "agent project/lead (pane %7) is quarantined; restart atmd and reconcile",
            ),
            (
                R::Quarantined {
                    agent: agent(),
                    pane: None,
                },
                "unavailable",
                "agent project/lead (pane no pane) is quarantined; restart atmd and reconcile",
            ),
            (
                R::Control(ControlError::PaneQuarantined {
                    pane: "%7".into(),
                    agent: agent(),
                }),
                "unavailable",
                "pane %7: agent project/lead is quarantined; restart atmd and reconcile",
            ),
            (
                R::Control(ControlError::OrganisationUnavailable { pane: "%7".into() }),
                "unavailable",
                "organisation state is unavailable for pane %7; refusing input",
            ),
            (
                R::Busy {
                    agent: agent(),
                    pane: Some(pane()),
                },
                "conflict",
                "agent project/lead or pane %7 is reserved",
            ),
            (
                R::TagMismatch {
                    pane: pane(),
                    expected: ExpectedTag::Agent(agent()),
                    actual: Some("project/other".into()),
                },
                "refused",
                "pane %7 tag mismatch: expected project/lead, found project/other",
            ),
            (R::StaleReservation, "refused", "stale pane reservation"),
            (R::PaneGone { pane: pane() }, "not_found", "pane %7 is gone"),
            (
                R::InvalidRequest {
                    reason: "bad pane".into(),
                },
                "invalid_params",
                "invalid reservation request: bad pane",
            ),
            (
                R::InvalidObservation {
                    pane: pane(),
                    reason: "ambiguous".into(),
                },
                "refused",
                "invalid observation of pane %7: ambiguous",
            ),
            (
                R::Unavailable,
                "unavailable",
                "reserved control service is unavailable",
            ),
            (
                R::NotImplemented {
                    operation: "capture",
                },
                "unavailable",
                "reserved control operation 'capture' is not implemented",
            ),
            (
                R::Timeout,
                "timeout",
                "reserved control operation timed out; admitted effects may have completed",
            ),
            (
                R::Control(ControlError::AgentClearing {
                    pane: "%7".into(),
                    agent: agent(),
                }),
                "refused",
                "Refusing to send to %7: agent project/lead is clearing; try again in a moment",
            ),
        ] {
            let mapped = MethodError::from(AgentError::from(error));
            assert_eq!(mapped.0.code, code, "{message}");
            assert_eq!(mapped.0.message, message);
            for wrapper in ["PaneId(", "AgentId {", "ProjectId(", "Some("] {
                assert!(!mapped.0.message.contains(wrapper), "{message}");
            }
        }
    }

    #[test]
    fn organisation_refusals_preserve_error_classification() {
        use super::super::OrgServiceError as O;
        use atm_org::{ApplyError as A, Refusal as R};
        let agent = || "project/lead".parse().unwrap();
        let project = || "project".parse().unwrap();
        for (error, code) in [
            (O::Unavailable("offline".into()), "unavailable"),
            (O::Timeout, "timeout"),
            (O::InvalidParams("bad cursor".into()), "invalid_params"),
            (O::Oversized(1), "refused"),
            (
                O::Refused(R::Invariant(A::UnknownProject(project()))),
                "not_found",
            ),
            (
                O::Refused(R::Invariant(A::UnknownAgent(agent()))),
                "not_found",
            ),
            (
                O::Refused(R::Invariant(A::UnknownMessage(atm_org::MessageId::new(1)))),
                "not_found",
            ),
            (
                O::Refused(R::Invariant(A::InvalidId(
                    "bad/id/extra".parse::<atm_org::AgentId>().unwrap_err(),
                ))),
                "invalid_params",
            ),
            (
                O::Refused(R::InvalidValue {
                    field: "root",
                    reason: "relative".into(),
                }),
                "invalid_params",
            ),
            (
                O::Refused(R::Invariant(A::PaneOccupied {
                    pane: "%7".into(),
                    holder: agent(),
                })),
                "conflict",
            ),
            (
                O::Refused(R::Invariant(A::DuplicateMessage(atm_org::MessageId::new(
                    1,
                )))),
                "conflict",
            ),
            (
                O::Refused(R::AgentAlreadyBound {
                    agent_id: agent(),
                    pane: "%7".into(),
                }),
                "conflict",
            ),
            (
                O::Refused(R::ProjectRootConflict {
                    project_id: project(),
                    registered: "/old".into(),
                    requested: "/new".into(),
                }),
                "conflict",
            ),
            (
                O::Refused(R::InvalidAgentState {
                    agent_id: agent(),
                    state: atm_org::AgentStatus::Down,
                    operation: "clear",
                }),
                "refused",
            ),
        ] {
            let message = error.to_string();
            let mapped = MethodError::from(AgentError::from(error));
            assert_eq!(mapped.0.code, code, "{message}");
            assert_eq!(mapped.0.message, message);
        }
    }

    #[cfg(unix)]
    #[test]
    fn typed_non_utf8_root_is_invalid() {
        use std::os::unix::ffi::OsStrExt;
        let root = std::path::Path::new(std::ffi::OsStr::from_bytes(b"/project/\xff"));
        assert!(matches!(
            validate_root(root),
            Err(AgentError::InvalidParams(_))
        ));
    }

    #[tokio::test]
    async fn roots_are_absolute_before_calling_disabled_service() {
        for method in ["agent.up", "agent.adopt", "agent.list"] {
            for root in ["", ".", "relative/path", "../project", "/project", "/"] {
                let mut params = json!({"project_root":root});
                if method != "agent.list" {
                    params["name"] = json!("lead");
                }
                if method == "agent.adopt" {
                    params["pane"] = json!("%7");
                }
                let expected = if root.starts_with('/') {
                    "unavailable"
                } else {
                    "invalid_params"
                };
                assert_eq!(code(method, params).await, expected, "{method}: {root:?}");
            }
        }
    }

    #[tokio::test]
    async fn names_use_org_rules_before_calling_disabled_service() {
        for method in ["agent.up", "agent.adopt"] {
            for (name, valid) in [
                ("", false),
                ("../lead", false),
                ("a/b", false),
                ("Lead", false),
                ("léad", false),
                ("-lead", false),
                (" lead", false),
                ("owner", false),
                ("system", false),
                ("lead-", true),
                ("0", true),
                ("01234567890123456789012345678901", true),
                ("012345678901234567890123456789012", false),
            ] {
                let mut params = json!({"project_root":"/project","name":name});
                if method == "agent.adopt" {
                    params["pane"] = json!("%7");
                }
                assert_eq!(
                    code(method, params).await,
                    if valid {
                        "unavailable"
                    } else {
                        "invalid_params"
                    },
                    "{method}: {name:?}"
                );
            }
        }
    }

    #[tokio::test]
    async fn adopt_requires_canonical_pane_before_calling_disabled_service() {
        for (pane, valid) in [
            ("%0", true),
            ("%7", true),
            ("%123456789012345678901234567890", true),
            ("%00", false),
            ("%07", false),
            ("7", false),
            ("%", false),
            ("%+7", false),
            ("%-7", false),
            ("%٧", false),
            (" %7", false),
            ("%7\n", false),
            ("%7; kill-pane", false),
            ("session:0.1", false),
        ] {
            assert_eq!(
                code(
                    "agent.adopt",
                    json!({"project_root":"/project","name":"lead","pane":pane})
                )
                .await,
                if valid {
                    "unavailable"
                } else {
                    "invalid_params"
                },
                "{pane:?}"
            );
        }
    }

    #[tokio::test]
    async fn clear_timeout_is_bounded_before_calling_disabled_service() {
        for (timeout, valid) in [
            (json!(1), true),
            (json!(30000), true),
            (json!(120000), true),
            (json!(0), false),
            (json!(120001), false),
            (json!(u64::MAX), false),
            (json!(-1), false),
            (json!(1.5), false),
            (json!(null), false),
            (json!("30000"), false),
            (serde_json::from_str("18446744073709551616").unwrap(), false),
        ] {
            assert_eq!(
                code(
                    "agent.clear",
                    json!({"agent":"project/lead","timeout_ms":timeout})
                )
                .await,
                if valid {
                    "unavailable"
                } else {
                    "invalid_params"
                },
                "{timeout}"
            );
        }
        assert_eq!(
            code("agent.clear", json!({"agent":"project/lead"})).await,
            "unavailable"
        );
    }

    #[tokio::test]
    async fn schemas_reject_bad_fields_and_allow_only_documented_defaults() {
        for (method, params) in [
            ("agent.up", json!({"project_root":"/project","name":"lead"})),
            (
                "agent.adopt",
                json!({"project_root":"/project","name":"lead","pane":"%7"}),
            ),
            ("agent.clear", json!({"agent":"project/lead"})),
            ("agent.respawn", json!({"agent":"project/lead"})),
            ("agent.down", json!({"agent":"project/lead"})),
            ("agent.list", json!({})),
        ] {
            assert_eq!(code(method, params.clone()).await, "unavailable");
            let mut unknown = params.clone();
            unknown["typo"] = json!(true);
            for bad in [unknown, json!([]), json!(true), json!("bad"), json!(1)] {
                assert_eq!(code(method, bad).await, "invalid_params", "{method}");
            }
            for field in params.as_object().unwrap().keys() {
                let mut missing = params.clone();
                missing.as_object_mut().unwrap().remove(field);
                assert_eq!(
                    code(method, missing).await,
                    "invalid_params",
                    "{method}/{field}"
                );
                for wrong in [Value::Null, json!(17), json!({}), json!([])] {
                    let mut bad = params.clone();
                    bad[field] = wrong;
                    assert_eq!(
                        code(method, bad).await,
                        "invalid_params",
                        "{method}/{field}"
                    );
                }
            }
            assert_eq!(
                code(method, Value::Null).await,
                if method == "agent.list" {
                    "unavailable"
                } else {
                    "invalid_params"
                }
            );
        }
        for method in ["agent.clear", "agent.respawn", "agent.down"] {
            for id in [
                "lead",
                "/lead",
                "project/",
                "project/owner",
                "project/system",
                "Project/lead",
                "project/lead/child",
            ] {
                assert_eq!(
                    code(method, json!({"agent":id})).await,
                    "invalid_params",
                    "{method}/{id}"
                );
            }
        }
        for root in [Value::Null, json!("/project")] {
            assert_eq!(
                code("agent.list", json!({"project_root":root})).await,
                "unavailable"
            );
        }
        for root in [json!(17), json!(true), json!([])] {
            assert_eq!(
                code("agent.list", json!({"project_root":root})).await,
                "invalid_params"
            );
        }
        for kill in [json!(false), json!(true)] {
            assert_eq!(
                code("agent.down", json!({"agent":"project/lead","kill":kill})).await,
                "unavailable"
            );
        }
        for kill in [Value::Null, json!(0), json!("false")] {
            assert_eq!(
                code("agent.down", json!({"agent":"project/lead","kill":kill})).await,
                "invalid_params"
            );
        }
    }
}
