//! Read-only admission health; no supervisor verdicts or polling policy.

use super::methods::{parse_params, MethodError, MethodRouter};
use crate::{control::service::ControlHandle, org::agents::AgentHandle};
use atm_protocol::error_code;
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReservationState {
    Ok,
    Full,
    Failed,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReservationFailure {
    QuarantineOverflow,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ReservationHealth {
    pub state: ReservationState,
    pub reason: Option<ReservationFailure>,
    pub live: usize,
    pub quarantined: usize,
    pub capacity: usize,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LifecycleState {
    Ok,
    Full,
    Stopped,
    Absent,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct LifecycleHealth {
    pub state: LifecycleState,
    pub capacity: usize,
}

#[derive(Debug, Clone, Serialize)]
struct Health {
    reservations: ReservationHealth,
    lifecycle: LifecycleHealth,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Params {}

pub(super) fn register(
    router: &mut MethodRouter,
    control: Option<ControlHandle>,
    agents: AgentHandle,
) -> Result<(), MethodError> {
    const BUDGET: Duration = Duration::from_secs(5);
    router.register("daemon.health", BUDGET, move |_, params| {
        let control = control.clone();
        let agents = agents.clone();
        async move {
            parse_params::<Params>(params)?;
            let control = control.ok_or_else(|| {
                MethodError::new(error_code::UNAVAILABLE, "control service is unavailable")
            })?;
            let reservations = control
                .reservation_health(tokio::time::Instant::now() + BUDGET)
                .await
                .map_err(crate::org::agents::AgentError::from)?;
            serde_json::to_value(Health {
                reservations,
                lifecycle: agents.lifecycle_health(),
            })
            .map_err(|error| MethodError::new(error_code::INTERNAL, error.to_string()))
        }
    })
}

#[cfg(test)]
mod tests {
    use crate::control::service::spawn_control;
    use crate::server::methods::{router, CallContext, CallerId, MethodServices};
    use atm_tmux::MockTmuxClient;
    use serde_json::{json, Value};
    use tokio_util::sync::CancellationToken;

    fn caller() -> CallContext {
        CallContext {
            client_id: CallerId("health-test".into()),
            from_pane: None,
        }
    }

    #[tokio::test]
    async fn daemon_health_is_advertised_by_info() {
        let router = router(MethodServices::new(crate::registry::spawn_registry())).unwrap();
        let info = router
            .call(caller(), "daemon.info", Value::Null)
            .await
            .unwrap();
        assert!(info["methods"]
            .as_array()
            .unwrap()
            .contains(&json!("daemon.health")));
        assert_eq!(info["methods"], json!(router.names()));
    }

    #[tokio::test]
    async fn daemon_health_reads_live_actor_and_absent_lifecycle_without_effects() {
        let registry = crate::registry::spawn_registry();
        let mock = MockTmuxClient::new();
        let cancel = CancellationToken::new();
        let mut services = MethodServices::new(registry.clone());
        let (control, task) = spawn_control(
            Box::new(mock.clone()),
            registry,
            services.org.clone(),
            cancel.clone(),
        );
        services.control = Some(control);
        let router = router(services).unwrap();
        for params in [Value::Null, json!({}), Value::Null] {
            assert_eq!(
                router
                    .call(caller(), "daemon.health", params)
                    .await
                    .unwrap(),
                json!({
                    "reservations": {"state":"ok", "reason":null, "live":0, "quarantined":0, "capacity":32},
                    "lifecycle": {"state":"absent", "capacity":32}
                })
            );
        }
        assert!(mock.calls().is_empty());
        cancel.cancel();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn daemon_health_rejects_all_nonempty_or_nonobject_params_before_reading() {
        let router = router(MethodServices::new(crate::registry::spawn_registry())).unwrap();
        for params in [
            json!({"unexpected":true}),
            json!([]),
            json!([1]),
            json!("x"),
            json!(1),
            json!(true),
        ] {
            let error = router
                .call(caller(), "daemon.health", params)
                .await
                .unwrap_err();
            assert_eq!(error.0.code, "invalid_params");
        }
    }

    #[tokio::test]
    async fn daemon_health_never_fabricates_ok_for_missing_or_stopped_control() {
        let registry = crate::registry::spawn_registry();
        let mut services = MethodServices::new(registry.clone());
        let missing = router(services.clone()).unwrap();
        assert_eq!(
            missing
                .call(caller(), "daemon.health", Value::Null)
                .await
                .unwrap_err()
                .0
                .code,
            "unavailable"
        );
        let cancel = CancellationToken::new();
        let (control, task) = spawn_control(
            Box::new(MockTmuxClient::new()),
            registry,
            services.org.clone(),
            cancel.clone(),
        );
        services.control = Some(control);
        let stopped = router(services).unwrap();
        cancel.cancel();
        task.await.unwrap();
        assert_eq!(
            stopped
                .call(caller(), "daemon.health", Value::Null)
                .await
                .unwrap_err()
                .0
                .code,
            "unavailable"
        );
    }
}
