use super::agents::*;
use crate::server::methods::{router, CallContext, CallerId, MethodServices};
use serde_json::{json, Value};

#[tokio::test]
async fn agent_contract_disabled_has_no_effects() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = tokio_util::sync::CancellationToken::new();
    let org = super::OrgHandle::open(dir.path().into(), cancel.clone()).await;
    let registry = crate::registry::spawn_registry();
    let mock = atm_tmux::MockTmuxClient::new();
    let (control, control_task) = crate::control::service::spawn_control_with_org(
        Box::new(mock.clone()),
        registry.clone(),
        org.clone(),
        cancel.clone(),
    );
    let before = org.status().unwrap();
    let (agents, task) = spawn_agents(org.clone(), registry, control, cancel.clone());
    let mut services = MethodServices::new(crate::registry::spawn_registry());
    services.agents = Some(agents.clone());
    let router = router(services).unwrap();
    for (method, params) in requests() {
        let error = router.call(caller(), method, params).await.unwrap_err();
        assert_eq!(error.0.code, "unavailable");
        assert_eq!(
            error.0.message,
            format!("agent operation '{method}' is not implemented")
        );
    }
    assert!(matches!(
        agents.reconciled().await,
        Err(AgentError::NotImplemented { .. })
    ));
    assert!(
        matches!(AgentHandle::disabled("off").reconciled().await, Err(AgentError::Unavailable(reason)) if reason == "off")
    );
    task.await.unwrap();
    assert!(mock.calls().is_empty());
    assert_eq!(org.status().unwrap(), before);
    assert!(org.snapshot().await.unwrap().agents().next().is_none());
    cancel.cancel();
    control_task.await.unwrap();
}

#[test]
fn agent_contract_schemas_keep_defaults_and_optional_session() {
    let clear: ClearParams = serde_json::from_value(json!({"agent":"project/lead"})).unwrap();
    assert_eq!(clear.timeout_ms, 30_000);
    assert!(
        !serde_json::from_value::<DownParams>(json!({"agent":"project/lead"}))
            .unwrap()
            .kill
    );
    assert!(serde_json::from_value::<ListParams>(json!({}))
        .unwrap()
        .project_root
        .is_none());
    assert!(serde_json::from_value::<ClearParams>(
        json!({"agent":"project/lead","timeout_ms":null})
    )
    .is_err());
    let result = ClearResult {
        agent_id: clear.agent,
        session_id: None,
    };
    assert_eq!(
        serde_json::to_value(&result).unwrap(),
        json!({"agent_id":"project/lead","session_id":null})
    );
    let roundtrip: ClearResult =
        serde_json::from_value(json!({"agent_id":"project/lead","session_id":null,"future":true}))
            .unwrap();
    assert_eq!(roundtrip.agent_id, result.agent_id);
    assert!(roundtrip.session_id.is_none());
    for (method, params) in requests() {
        let value = match method {
            "agent.up" => {
                serde_json::to_value(serde_json::from_value::<UpParams>(params.clone()).unwrap())
                    .unwrap()
            }
            "agent.adopt" => {
                serde_json::to_value(serde_json::from_value::<AdoptParams>(params.clone()).unwrap())
                    .unwrap()
            }
            "agent.clear" => {
                serde_json::to_value(serde_json::from_value::<ClearParams>(params.clone()).unwrap())
                    .unwrap()
            }
            "agent.respawn" => serde_json::to_value(
                serde_json::from_value::<RespawnParams>(params.clone()).unwrap(),
            )
            .unwrap(),
            "agent.down" => {
                serde_json::to_value(serde_json::from_value::<DownParams>(params.clone()).unwrap())
                    .unwrap()
            }
            "agent.list" => {
                serde_json::to_value(serde_json::from_value::<ListParams>(params.clone()).unwrap())
                    .unwrap()
            }
            _ => unreachable!(),
        };
        for (key, expected) in params.as_object().unwrap() {
            assert_eq!(value.get(key), Some(expected), "{method}/{key}");
        }
    }
}

#[test]
fn agent_contract_error_codes_preserve_actionable_messages() {
    use super::OrgServiceError;
    use crate::control::{
        reservation::{PaneId, ReservationError},
        ControlError,
    };
    use crate::server::methods::MethodError;
    let cases = [
        (
            AgentError::InvalidParams("bad root".into()),
            "invalid_params",
        ),
        (
            AgentError::NotFound("agent project/lead was not found".into()),
            "not_found",
        ),
        (AgentError::Conflict("busy".into()), "conflict"),
        (AgentError::Refused("unsafe editor".into()), "refused"),
        (AgentError::Timeout, "timeout"),
        (AgentError::Internal("storage".into()), "internal"),
        (ReservationError::Unavailable.into(), "unavailable"),
        (OrgServiceError::Timeout.into(), "timeout"),
        (
            OrgServiceError::Unavailable("disk".into()).into(),
            "unavailable",
        ),
        (
            OrgServiceError::Refused(atm_org::Refusal::Invariant(
                atm_org::ApplyError::UnknownAgent("project/lead".parse().unwrap()),
            ))
            .into(),
            "not_found",
        ),
    ];
    for (error, code) in cases {
        let message = error.to_string();
        let mapped = MethodError::from(error);
        assert_eq!(mapped.0.code, code);
        assert_eq!(mapped.0.message, message);
    }
    let error = MethodError::from(AgentError::from(ReservationError::ScreenChanged {
        pane: PaneId::parse("%7").unwrap(),
    }));
    assert_eq!(error.0.code, "refused");
    assert_eq!(
        error.0.message,
        "Refusing input to %7: screen changed or is no longer safe; nothing was sent by this action"
    );
    let error = MethodError::from(AgentError::from(ReservationError::Control(
        ControlError::AgentClearing {
            pane: "%7".into(),
            agent: "project/lead".parse().unwrap(),
        },
    )));
    assert_eq!(error.0.code, "refused");
    assert_eq!(
        error.0.message,
        "Refusing to send to %7: agent project/lead is clearing; try again in a moment"
    );
}

fn caller() -> CallContext {
    CallContext {
        client_id: CallerId("contract".into()),
        from_pane: None,
    }
}

fn requests() -> Vec<(&'static str, Value)> {
    vec![
        ("agent.up", json!({"project_root":"/project","name":"lead"})),
        (
            "agent.adopt",
            json!({"project_root":"/project","name":"lead","pane":"%7"}),
        ),
        ("agent.clear", json!({"agent":"project/lead"})),
        ("agent.respawn", json!({"agent":"project/lead"})),
        ("agent.down", json!({"agent":"project/lead"})),
        ("agent.list", json!({})),
    ]
}

// Missing registration must not masquerade as an old daemon; unavailable is the
// checkpoint contract, not unknown_method or a fabricated successful result.
#[tokio::test]
async fn agent_contract_advertises_six_disabled_methods() {
    let router = router(MethodServices::new(crate::registry::spawn_registry())).unwrap();
    let info = router
        .call(caller(), "daemon.info", Value::Null)
        .await
        .unwrap();
    for (method, params) in requests() {
        assert!(router.names().iter().any(|name| name == method), "{method}");
        assert!(info["methods"].as_array().unwrap().contains(&json!(method)));
        let error = router.call(caller(), method, params).await.unwrap_err();
        assert_eq!(error.0.code, "unavailable", "{method}");
        assert_eq!(
            error.0.message,
            "agent service unavailable: named-agent operations are not configured"
        );
    }
}

#[tokio::test]
async fn agent_contract_rejects_unknown_and_nonobject_parameters() {
    let router = router(MethodServices::new(crate::registry::spawn_registry())).unwrap();
    for (method, mut params) in requests() {
        params
            .as_object_mut()
            .unwrap()
            .insert("typo".into(), json!(true));
        for bad in [params, json!([]), json!(true), json!("bad")] {
            let error = router.call(caller(), method, bad).await.unwrap_err();
            assert_eq!(error.0.code, "invalid_params", "{method}");
        }
    }
}
