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
    /// Supplied at server startup, or injected by tests/in-process clients.
    pub control: Option<crate::control::service::ControlHandle>,
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
            control: None,
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
    let org = services.org;
    router.register("log.tail", Duration::from_secs(5), move |_, params| {
        let org = org.clone();
        async move {
            let params = parse_params::<crate::org::TailParams>(params)?;
            let records = org.tail(params).await.map_err(|error| {
                use crate::org::OrgServiceError;
                let code = match &error {
                    OrgServiceError::InvalidParams(_) => error_code::INVALID_PARAMS,
                    OrgServiceError::Oversized(_) | OrgServiceError::Refused(_) => {
                        error_code::REFUSED
                    }
                    OrgServiceError::Timeout => error_code::TIMEOUT,
                    OrgServiceError::Unavailable(_) => error_code::UNAVAILABLE,
                };
                MethodError::new(code, error.to_string())
            })?;
            serde_json::to_value(records)
                .map_err(|error| MethodError::new(error_code::INTERNAL, error.to_string()))
        }
    })?;

    register_control(&mut router, services.control)?;

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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SendParams {
    target: String,
    text: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PrepareParams {
    target_pane: Option<String>,
    cwd: Option<String>,
    fallback_cwd: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SplitParams {
    target_pane: String,
    caller_pane_missing: bool,
    direction: atm_tmux::PaneDirection,
    size: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TypeParams {
    pane: String,
    command: String,
}

fn control_error(error: crate::control::service::ServiceError) -> MethodError {
    use crate::control::{service::ServiceError, ControlError};
    use std::error::Error;
    let code = match &error {
        ServiceError::Timeout => error_code::TIMEOUT,
        ServiceError::Unavailable => error_code::UNAVAILABLE,
        ServiceError::Control(error) => match error {
            ControlError::NoSession { .. } | ControlError::NoPane { .. } => error_code::NOT_FOUND,
            ControlError::AmbiguousTarget { .. } => error_code::CONFLICT,
            ControlError::SendRefused { .. } => error_code::REFUSED,
            ControlError::Tmux {
                source: atm_tmux::TmuxError::Timeout { .. },
                ..
            } => error_code::TIMEOUT,
            ControlError::Tmux { .. } => error_code::INTERNAL,
        },
    };
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        message.push_str(&format!("\nCaused by: {cause}"));
        source = cause.source();
    }
    MethodError::new(code, message)
}

fn require_control(
    control: Option<crate::control::service::ControlHandle>,
    caller: CallContext,
) -> Result<crate::control::service::ControlHandle, MethodError> {
    control
        .map(|control| control.with_caller(caller))
        .ok_or_else(|| MethodError::new(error_code::UNAVAILABLE, "control service is unavailable"))
}

fn register_control(
    router: &mut MethodRouter,
    control: Option<crate::control::service::ControlHandle>,
) -> Result<(), MethodError> {
    let send = control.clone();
    router.register(
        "session.send",
        Duration::from_secs(35),
        move |caller, params| {
            let control = send.clone();
            async move {
                let p: SendParams = parse_params(params)?;
                let pane = require_control(control, caller)?
                    .send(p.target, p.text)
                    .await
                    .map_err(control_error)?;
                Ok(serde_json::json!({"pane": pane}))
            }
        },
    )?;
    let prepare = control.clone();
    router.register("session.spawn_prepare", Duration::from_secs(35), move |caller, params| {
        let control = prepare.clone();
        async move {
            let p: PrepareParams = parse_params(params)?;
            let context = require_control(control, caller.clone())?.prepare_spawn(p.target_pane, p.cwd, caller.from_pane.map(|p| p.0), p.fallback_cwd).await.map_err(control_error)?;
            Ok(serde_json::json!({"target_pane": context.target_pane, "cwd": context.cwd, "caller_pane_missing": context.caller_pane_missing()}))
        }
    })?;
    let split = control.clone();
    router.register(
        "session.spawn",
        Duration::from_secs(35),
        move |caller, params| {
            let control = split.clone();
            async move {
                let p: SplitParams = parse_params(params)?;
                let context =
                    crate::control::SpawnContext::new(p.target_pane, None, p.caller_pane_missing);
                let pane = require_control(control, caller)?
                    .split_spawn(context, p.direction, p.size)
                    .await
                    .map_err(control_error)?;
                Ok(serde_json::json!({"pane": pane}))
            }
        },
    )?;
    router.register(
        "session.spawn_type",
        Duration::from_secs(35),
        move |caller, params| {
            let control = control.clone();
            async move {
                let p: TypeParams = parse_params(params)?;
                require_control(control, caller)?
                    .type_spawn_command(p.pane, p.command)
                    .await
                    .map_err(control_error)?;
                Ok(Value::Null)
            }
        },
    )?;
    Ok(())
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

    async fn control_router(
        org: crate::org::OrgHandle,
    ) -> (
        MethodRouter,
        atm_tmux::MockTmuxClient,
        tokio_util::sync::CancellationToken,
        tokio::task::JoinHandle<()>,
    ) {
        let mut services = MethodServices::new(crate::registry::spawn_registry());
        services
            .registry
            .register(atm_core::SessionDomain {
                id: atm_core::SessionId::new("audit-session"),
                tmux_pane: Some("%7".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        let mock = atm_tmux::MockTmuxClient::new();
        mock.set_next_pane_id("%42");
        let cancel = tokio_util::sync::CancellationToken::new();
        let (control, task) = crate::control::service::spawn_control(
            Box::new(mock.clone()),
            services.registry.clone(),
            org.clone(),
            cancel.clone(),
        );
        services.control = Some(control);
        services.org = org;
        (router(services).unwrap(), mock, cancel, task)
    }

    // An actor request ordered after each mutation is a barrier for its audit.
    // No timer or polling is used to infer completion.
    async fn audit_barrier(router: &MethodRouter) {
        router
            .call(
                context(),
                "session.spawn_prepare",
                json!({"target_pane":"%7"}),
            )
            .await
            .unwrap();
    }

    async fn successful_control_actions(router: &MethodRouter) {
        assert_eq!(
            router
                .call(
                    context(),
                    "session.send",
                    json!({"target":"audit-session","text":"private text"})
                )
                .await
                .unwrap(),
            json!({"pane":"%7"})
        );
        assert_eq!(
            router.call(context(), "session.spawn", json!({"target_pane":"%7","caller_pane_missing":false,"direction":"below","size":"50%"})).await.unwrap(),
            json!({"pane":"%42"})
        );
        assert_eq!(
            router
                .call(
                    context(),
                    "session.spawn_type",
                    json!({"pane":"%42","command":"private command"})
                )
                .await
                .unwrap(),
            Value::Null
        );
        audit_barrier(router).await;
    }

    #[tokio::test]
    async fn control_actions_append_actual_panes_and_attribution_without_payloads() {
        let dir = tempfile::tempdir().unwrap();
        let org_cancel = tokio_util::sync::CancellationToken::new();
        let org = crate::org::OrgHandle::open(dir.path().into(), org_cancel.clone()).await;
        let (router, mock, cancel, task) = control_router(org).await;
        successful_control_actions(&router).await;
        let records = router
            .call(context(), "log.tail", json!({"topics":["control"]}))
            .await
            .unwrap();
        let records = records.as_array().unwrap();
        assert_eq!(records.len(), 3);
        for (record, kind, pane, method) in [
            (&records[0], "control.sent", "%7", "session.send"),
            (&records[1], "control.spawned", "%42", "session.spawn"),
            (&records[2], "control.sent", "%42", "session.spawn_type"),
        ] {
            assert_eq!(record["kind"], kind);
            assert_eq!(
                record["body"],
                json!({"pane":pane,"action":{
                    "method":method,"outcome":"ok","error":null,
                    "client_id":"client","from_pane":"%12"
                }})
            );
        }
        assert!(!serde_json::to_string(records).unwrap().contains("private"));
        assert_eq!(
            mock.calls()
                .iter()
                .filter(|call| matches!(call, atm_tmux::mock::MockCall::PasteText { .. }))
                .count(),
            1
        );
        cancel.cancel();
        task.await.unwrap();
        org_cancel.cancel();
    }

    #[tokio::test]
    async fn disabled_organisation_does_not_change_or_repeat_control_actions() {
        let (router, mock, cancel, task) =
            control_router(crate::org::OrgHandle::disabled("test disabled")).await;
        successful_control_actions(&router).await;
        assert_eq!(
            mock.calls()
                .iter()
                .filter(|call| matches!(call, atm_tmux::mock::MockCall::PasteText { .. }))
                .count(),
            1
        );
        assert_eq!(
            mock.calls()
                .iter()
                .filter(|call| matches!(call, atm_tmux::mock::MockCall::SplitWindow { .. }))
                .count(),
            1
        );
        assert_eq!(
            mock.calls()
                .iter()
                .filter(|call| matches!(call, atm_tmux::mock::MockCall::SendKeys { .. }))
                .count(),
            3
        );
        cancel.cancel();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn control_errors_are_audited_without_claiming_success_or_retrying() {
        let dir = tempfile::tempdir().unwrap();
        let org_cancel = tokio_util::sync::CancellationToken::new();
        let org = crate::org::OrgHandle::open(dir.path().into(), org_cancel.clone()).await;
        let (router, mock, cancel, task) = control_router(org).await;
        mock.set_next_raw_output("1 0");
        let error = router
            .call(
                context(),
                "session.send",
                json!({"target":"%7","text":"no"}),
            )
            .await
            .unwrap_err();
        assert_eq!(error.0.code, "refused");
        audit_barrier(&router).await;
        mock.set_next_error(atm_tmux::TmuxError::NotFound);
        let error = router
            .call(
                context(),
                "session.spawn_type",
                json!({"pane":"%42","command":"no"}),
            )
            .await
            .unwrap_err();
        assert_eq!(error.0.code, "internal");
        audit_barrier(&router).await;
        mock.set_next_error(atm_tmux::TmuxError::NotFound);
        let error = router
            .call(
                context(),
                "session.send",
                json!({"target":"audit-session","text":"no"}),
            )
            .await
            .unwrap_err();
        assert_eq!(error.0.code, "internal");
        audit_barrier(&router).await;
        mock.set_next_error(atm_tmux::TmuxError::NotFound);
        let error = router.call(context(), "session.spawn", json!({"target_pane":"%7","caller_pane_missing":false,"direction":"below","size":"50%"})).await.unwrap_err();
        assert_eq!(error.0.code, "internal");
        audit_barrier(&router).await;
        let error = router
            .call(
                context(),
                "session.send",
                json!({"target":"missing-session","text":"no"}),
            )
            .await
            .unwrap_err();
        assert_eq!(error.0.code, "not_found");
        audit_barrier(&router).await;
        let records = router
            .call(context(), "log.tail", json!({"topics":["control"]}))
            .await
            .unwrap();
        let records = records.as_array().unwrap();
        assert_eq!(records.len(), 4);
        assert_eq!(records[0]["body"]["pane"], "%7");
        assert_eq!(records[1]["body"]["pane"], "%42");
        assert_eq!(
            records[2]["body"]["pane"], "%7",
            "failed alias send still names the resolved pane"
        );
        assert_eq!(records[3]["kind"], "control.spawned");
        assert_eq!(
            records[3]["body"]["pane"], "%7",
            "failed split names its attempted target, not a fictitious new pane"
        );
        for record in records {
            assert_eq!(record["body"]["action"]["outcome"], "error");
            assert!(record["body"]["action"]["error"]
                .as_str()
                .is_some_and(|s| !s.is_empty()));
        }
        assert!(!mock
            .calls()
            .iter()
            .any(|call| matches!(call, atm_tmux::mock::MockCall::PasteText { .. })));
        assert_eq!(
            mock.calls()
                .iter()
                .filter(|call| matches!(call, atm_tmux::mock::MockCall::SendKeys { .. }))
                .count(),
            1
        );
        cancel.cancel();
        task.await.unwrap();
        org_cancel.cancel();
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
