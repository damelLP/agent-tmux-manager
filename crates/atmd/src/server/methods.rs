//! Control-plane method registration and bounded dispatch.
//!
//! Register methods in [`router`], capturing only their required service handles.
//! Each handler owns its parameter schema; mutation handlers must tolerate being
//! dropped on timeout or daemon shutdown (already performed effects are not undone).
//! Client EOF/disconnect drains accepted calls instead of cancelling them.

use std::collections::{btree_map::Entry, BTreeMap};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use crate::registry::RegistryHandle;
use atm_protocol::{error_code, ProtocolVersion, ResponseError};
use serde::{de::DeserializeOwned, Deserialize};
use serde_json::Value;

/// Cloneable daemon dependencies shared by method registrations. Add a service
/// field here rather than changing the router or server constructor signatures.
#[derive(Clone)]
pub struct MethodServices {
    pub registry: RegistryHandle,
    /// Production resolves this before daemonization; tests can inject a path.
    /// daemon.info displays non-UTF-8 paths lossily; filesystem use stays exact.
    pub state_dir: PathBuf,
    /// Sole durable organisation writer; U7 submits control audit commands here.
    pub org: crate::org::OrgHandle,
    /// Nonblocking pre-translation capture; disabled for legacy library callers.
    pub raw: crate::org::RawHandle,
}

impl MethodServices {
    pub fn new(registry: RegistryHandle) -> Self {
        Self {
            registry,
            state_dir: crate::eventlog::state_dir(),
            org: crate::org::OrgHandle::disabled("organisation service not configured"),
            raw: crate::org::RawHandle::default(),
        }
    }
}

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

/// Object-only typed params, with null treated as an empty object. Parameter
/// structs must use `#[serde(deny_unknown_fields)]` to reject misspelled fields.
pub fn parse_params<T: DeserializeOwned>(params: Value) -> Result<T, MethodError> {
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
pub fn router(services: MethodServices) -> Result<MethodRouter, MethodError> {
    let mut router = MethodRouter::default();

    // Add service-backed registrations above this finalization. The factory,
    // not callers, owns the complete capability snapshot used by daemon.info.
    let mut names = router.names();
    names.push("daemon.info".into());
    names.sort();
    let info = serde_json::json!({
        "protocol": ProtocolVersion::CURRENT.to_string(),
        "version": env!("CARGO_PKG_VERSION"),
        "methods": names,
        "state_dir": services.state_dir.to_string_lossy(),
    });
    router.register("daemon.info", Duration::from_secs(5), move |_, params| {
        let info = info.clone();
        async move {
            parse_params::<InfoParams>(params)?;
            Ok(info)
        }
    })?;
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

    #[test]
    fn typed_params_share_object_null_and_unknown_field_rules() {
        #[derive(Debug, Deserialize, PartialEq)]
        #[serde(deny_unknown_fields)]
        struct Params {
            #[serde(default)]
            count: usize,
        }
        assert_eq!(
            parse_params::<Params>(Value::Null).unwrap(),
            Params { count: 0 }
        );
        assert_eq!(
            parse_params::<Params>(json!({"count":2})).unwrap(),
            Params { count: 2 }
        );
        for value in [
            json!([]),
            json!([2]),
            json!(true),
            json!({"typo":2}),
            json!({"count":"bad"}),
        ] {
            assert_eq!(
                parse_params::<Params>(value).unwrap_err().0.code,
                "invalid_params"
            );
        }
    }

    #[tokio::test]
    async fn info_reports_non_utf8_state_dir_lossily_without_panicking() {
        use std::os::unix::ffi::OsStringExt;
        let mut services = MethodServices::new(crate::registry::spawn_registry());
        services.state_dir = PathBuf::from(std::ffi::OsString::from_vec(b"/state/\xff".to_vec()));
        let router = router(services).unwrap();
        let info = router
            .call(context(), "daemon.info", Value::Null)
            .await
            .unwrap();
        assert_eq!(info["state_dir"], "/state/\u{fffd}");
    }

    #[tokio::test]
    async fn info_rejects_every_nonempty_or_nonobject_params_shape() {
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
                .call(context(), "daemon.info", params)
                .await
                .unwrap_err();
            assert_eq!(error.0.code, "invalid_params");
            assert!(!error.0.message.is_empty());
        }
        for params in [Value::Null, json!({})] {
            let info = router.call(context(), "daemon.info", params).await.unwrap();
            assert_eq!(info["state_dir"], json!(crate::eventlog::state_dir()));
        }
    }
}
