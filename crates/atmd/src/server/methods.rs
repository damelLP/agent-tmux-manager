//! Control-plane method registration and bounded dispatch.
//!
//! Register methods in [`router`], capturing only their required service handles.
//! Each handler owns its parameter schema; mutation handlers must tolerate being
//! dropped on timeout or disconnect (already performed effects are not undone).

use std::collections::{btree_map::Entry, BTreeMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use atm_protocol::{error_code, ResponseError};
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct CallerId(pub String);

#[derive(Debug, Clone)]
pub struct CallerPane(pub String);

/// Caller identity is attribution, not authentication; clients choose these values.
#[derive(Debug, Clone)]
pub struct CallContext {
    pub client_id: CallerId,
    pub from_pane: Option<CallerPane>,
}

#[derive(Debug, thiserror::Error)]
#[error("{message}", message = .0.message)]
pub struct MethodError(pub ResponseError);

impl MethodError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self(ResponseError {
            code: code.into(),
            message: message.into(),
        })
    }
}

pub type MethodResult = Result<Value, MethodError>;
type MethodFuture = Pin<Box<dyn Future<Output = MethodResult> + Send>>;
type Handler = Arc<dyn Fn(CallContext, Value) -> MethodFuture + Send + Sync>;

struct Method {
    deadline: Duration,
    handler: Handler,
}

/// Immutable after server startup; names are advertised in lexical order.
#[derive(Default)]
pub struct MethodRouter {
    methods: BTreeMap<&'static str, Method>,
}

impl MethodRouter {
    pub fn register<F, Fut>(
        &mut self,
        name: &'static str,
        deadline: Duration,
        handler: F,
    ) -> Result<(), MethodError>
    where
        F: Fn(CallContext, Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = MethodResult> + Send + 'static,
    {
        match self.methods.entry(name) {
            Entry::Vacant(entry) => {
                entry.insert(Method {
                    deadline,
                    handler: Arc::new(move |context, params| Box::pin(handler(context, params))),
                });
                Ok(())
            }
            Entry::Occupied(_) => Err(MethodError::new(
                error_code::INTERNAL,
                format!("duplicate method '{name}'"),
            )),
        }
    }

    pub fn names(&self) -> Vec<String> {
        self.methods.keys().map(|name| (*name).to_owned()).collect()
    }

    pub async fn call(&self, context: CallContext, name: &str, params: Value) -> MethodResult {
        let method = self.methods.get(name).ok_or_else(|| {
            MethodError::new(error_code::UNKNOWN_METHOD, format!("no method '{name}'"))
        })?;
        tokio::time::timeout(method.deadline, (method.handler)(context, params))
            .await
            .map_err(|_| {
                MethodError::new(error_code::TIMEOUT, format!("method '{name}' timed out"))
            })?
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InfoParams {}

fn info_params(params: Value) -> Result<InfoParams, MethodError> {
    let params = if params.is_null() {
        serde_json::json!({})
    } else {
        params
    };
    if !params.is_object() {
        return Err(MethodError::new(
            error_code::INVALID_PARAMS,
            "params must be an object or null",
        ));
    }
    serde_json::from_value(params)
        .map_err(|error| MethodError::new(error_code::INVALID_PARAMS, error.to_string()))
}

/// The single registration point. Later handlers capture registry/control handles
/// here; their addition never requires another connection dispatch arm.
pub(crate) fn router() -> Result<MethodRouter, MethodError> {
    let mut router = MethodRouter::default();
    router.register(
        "daemon.info",
        Duration::from_secs(5),
        |_, params| async move {
            info_params(params)?;
            // U2's state_dir resolver must land before the success result is wired.
            Err(MethodError::new(
                error_code::UNAVAILABLE,
                "daemon information is not available yet",
            ))
        },
    )?;
    Ok(router)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn context() -> CallContext {
        CallContext {
            client_id: CallerId("client".into()),
            from_pane: Some(CallerPane("%12".into())),
        }
    }

    #[tokio::test]
    async fn registration_preserves_context_state_and_errors() {
        let registry = crate::registry::spawn_registry();
        let mut router = MethodRouter::default();
        router
            .register("z", Duration::from_secs(1), move |ctx, params| {
                let registry = registry.clone();
                async move {
                    assert_eq!(ctx.client_id.0, "client");
                    assert_eq!(ctx.from_pane.unwrap().0, "%12");
                    assert!(registry.get_all_sessions().await.is_empty());
                    Ok(params)
                }
            })
            .unwrap();
        router
            .register("a", Duration::from_secs(1), |_, _| async {
                Err(MethodError::new(error_code::REFUSED, "no"))
            })
            .unwrap();
        assert_eq!(router.names(), ["a", "z"]);
        assert!(router
            .register("a", Duration::from_secs(1), |_, _| async {
                Ok(Value::Null)
            })
            .is_err());
        assert_eq!(
            router.call(context(), "z", json!({"v":1})).await.unwrap(),
            json!({"v":1})
        );
        let error = router.call(context(), "a", Value::Null).await.unwrap_err();
        assert_eq!(error.0.code, "refused");
        assert_eq!(error.0.message, "no");
        let error = router
            .call(context(), "x.y", Value::Null)
            .await
            .unwrap_err();
        assert_eq!(error.0.code, "unknown_method");
        assert_eq!(error.0.message, "no method 'x.y'");
    }

    #[tokio::test(start_paused = true)]
    async fn deadline_drops_the_handler_future() {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let tx = Arc::new(std::sync::Mutex::new(Some(tx)));
        let mut router = MethodRouter::default();
        router
            .register("slow", Duration::from_millis(5), move |_, _| {
                let guard = tx.lock().unwrap().take().unwrap();
                async move {
                    let _guard = guard;
                    std::future::pending::<MethodResult>().await
                }
            })
            .unwrap();
        assert_eq!(
            router
                .call(context(), "slow", Value::Null)
                .await
                .unwrap_err()
                .0
                .code,
            "timeout"
        );
        assert!(rx.await.is_err(), "handler future leaked after timeout");
    }

    #[tokio::test]
    async fn info_rejects_every_nonempty_or_nonobject_params_shape() {
        let router = router().unwrap();
        assert_eq!(router.names(), ["daemon.info"]);
        for params in [
            json!({"unexpected":true}),
            json!([]),
            json!([1]),
            json!("x"),
            json!(1),
            json!(true),
        ] {
            let error = router
                .call(context(), "daemon.info", params)
                .await
                .unwrap_err();
            assert_eq!(error.0.code, "invalid_params");
            assert!(!error.0.message.is_empty());
        }
        // U2 supplies state_dir later; valid params must already get past parsing.
        for params in [Value::Null, json!({})] {
            let error = router
                .call(context(), "daemon.info", params)
                .await
                .unwrap_err();
            assert_eq!(error.0.code, "unavailable");
        }
    }
}
