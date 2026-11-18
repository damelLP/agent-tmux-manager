//! Control-plane method registration and bounded dispatch.
//!
//! Register methods in [`router`], capturing only their required service handles.
//! Each handler owns its parameter schema; mutation handlers must tolerate being
//! dropped on timeout or daemon shutdown (already performed effects are not undone).
//! Client EOF/protocol Disconnect drains accepted calls. A full socket closure
//! drops send/reply/capture waits: the actor discards pre-effect input, but
//! finishes an admitted paste and bounds reply navigation by its total deadline.
//! It also drops handover's wait: the handover is its own task and carries on,
//! and tells the agent's manager if it then fails.

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
    /// Named-agent service; None advertises explicitly unavailable operations.
    pub agents: Option<crate::org::agents::AgentHandle>,
    /// The handover service the server shares with its watcher; None builds
    /// one for the method alone.
    pub handovers: Option<crate::org::agent_handover::Handovers>,
    /// Confirmed-message service; None advertises explicitly unavailable operations.
    pub messages: Option<crate::org::messages::MessageHandle>,
    /// Decision API and its sole restart-recoverable answer bridge.
    pub decisions: Option<crate::org::decisions::DecisionHandle>,
    /// None leaves library callers unconfigured; the binary supplies its single
    /// config parse result. Invalid settings disable messages with that cause.
    pub delivery_policy: Option<Result<atm_org::delivery::DeliveryPolicy, String>>,
    /// The reminder on a silent turn end; None leaves it off. Library callers
    /// start without it, the binary supplies its `[reminder]` settings.
    pub reminder: Option<atm_org::reminder::ReminderSettings>,
    /// The connection tap's two destinations, as one pair: the shared
    /// receipt/activity store and the history its Activity is forwarded to.
    /// The server builds both with the message runtime, before it accepts a
    /// connection, so the tap never has evidence without its history.
    pub message_tap: Option<(
        crate::org::message_evidence::MessageEvidence,
        crate::org::message_history::MessageHistoryHandle,
    )>,
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
            agents: None,
            handovers: None,
            messages: None,
            decisions: None,
            delivery_policy: None,
            reminder: None,
            message_tap: None,
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
    router_with_reporting(
        services,
        crate::org::reporting_line::ReportingFiles::default(),
    )
}

pub(crate) fn router_with_reporting(
    services: MethodServices,
    reporting: crate::org::reporting_line::ReportingFiles,
) -> Result<MethodRouter, MethodError> {
    let mut router = MethodRouter::default();
    let agent_unavailable = services.org.status().err().map_or_else(
        || "named-agent operations are not configured".into(),
        |error| error.to_string(),
    );
    crate::org::agent_context::register_agent_context_method(&mut router, services.org.clone())?;
    crate::org::org_tree_methods::register_org_tree_method(
        &mut router,
        services.org.clone(),
        services.registry.clone(),
        reporting.clone(),
    )?;
    crate::org::report::register(
        &mut router,
        services.org.clone(),
        reporting.clone(),
        chrono::Utc::now,
    )?;
    let handover_org = services.org.clone();
    crate::org::tasks::register_task_methods_with_reporting(
        &mut router,
        services.org.clone(),
        reporting.clone(),
    )?;
    let decisions = services
        .decisions
        .unwrap_or_else(|| {
            crate::org::decisions::DecisionHandle::disabled(
                "decision operations are not configured",
            )
        })
        .with_reporting_files(reporting.clone());
    crate::org::away::register_away_methods(&mut router, decisions.clone())?;
    crate::org::decisions::register_decision_methods(&mut router, decisions)?;
    crate::org::message_query::register(&mut router, services.org.clone(), reporting.clone())?;
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

    crate::control::windows::register(&mut router, services.control.clone())?;
    register_control(&mut router, services.control.clone())?;
    let agents = services
        .agents
        .unwrap_or_else(|| crate::org::agents::AgentHandle::disabled(agent_unavailable));
    crate::org::agents::register_agent_methods(&mut router, agents.clone())?;
    let handovers = services.handovers.unwrap_or_else(|| {
        crate::org::agent_handover::Handovers::for_agents(
            agents.clone(),
            handover_org,
            services.registry.clone(),
        )
    });
    crate::org::agent_handover::register_agent_handover_method(&mut router, handovers)?;

    crate::org::messages::register_message_methods(
        &mut router,
        services
            .messages
            .unwrap_or_else(|| {
                crate::org::messages::MessageHandle::disabled(
                    "confirmed-message operations are not configured",
                )
            })
            .with_reporting_files(reporting),
    )?;

    super::health::register(&mut router, services.control, agents)?;

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
    caller_server: Option<String>,
    target: String,
    text: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PaneParams {
    caller_server: Option<String>,
    target: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplyParams {
    caller_server: Option<String>,
    target: String,
    option: Option<usize>,
    #[serde(default)]
    yes: bool,
    #[serde(default)]
    no: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CaptureParams {
    caller_server: Option<String>,
    target: String,
    tail: Option<usize>,
    #[serde(default)]
    prompt: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PanesParams {
    session_name: String,
    caller_server: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RolesParams {
    caller_server: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RoleSetParams {
    caller_server: String,
    pane: String,
    // Value (not Option) makes the field required while permitting explicit null.
    role: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PrepareLargestParams {
    caller_server: String,
    // Required like role_set's nullable role, not an omitted Option default.
    caller_pane: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PrepareParams {
    caller_server: Option<String>,
    target_pane: Option<String>,
    cwd: Option<String>,
    fallback_cwd: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SplitParams {
    caller_server: Option<String>,
    cwd: Option<String>,
    target_pane: String,
    caller_pane_missing: bool,
    direction: atm_tmux::PaneDirection,
    size: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TypeParams {
    caller_server: Option<String>,
    pane: String,
    command: String,
}

pub(crate) fn control_error(error: crate::control::service::ServiceError) -> MethodError {
    use crate::control::{service::ServiceError, ControlError};
    use std::error::Error;
    let code = match &error {
        ServiceError::Timeout => error_code::TIMEOUT,
        ServiceError::Unavailable => error_code::UNAVAILABLE,
        ServiceError::ServerRefused(_) => error_code::REFUSED,
        ServiceError::Control(error) => match error {
            ControlError::NoSession { .. } | ControlError::NoPane { .. } => error_code::NOT_FOUND,
            ControlError::AmbiguousTarget { .. } => error_code::CONFLICT,
            ControlError::SendRefused { .. } | ControlError::AgentClearing { .. } => {
                error_code::REFUSED
            }
            ControlError::PaneQuarantined { .. } | ControlError::OrganisationUnavailable { .. } => {
                error_code::UNAVAILABLE
            }
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
    server: Option<String>,
) -> Result<crate::control::service::ControlHandle, MethodError> {
    control
        .map(|control| control.with_caller(caller).for_server(server))
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
                let pane = require_control(control, caller, p.caller_server)?
                    .send(p.target, p.text)
                    .await
                    .map_err(control_error)?;
                Ok(serde_json::json!({"pane": pane}))
            }
        },
    )?;
    let kill = control.clone();
    router.register(
        "session.kill",
        Duration::from_secs(35),
        move |caller, params| {
            let control = kill.clone();
            async move {
                let p: PaneParams = parse_params(params)?;
                let pane = require_control(control, caller, p.caller_server)?
                    .kill(p.target)
                    .await
                    .map_err(control_error)?;
                Ok(serde_json::json!({"pane": pane}))
            }
        },
    )?;
    let interrupt = control.clone();
    router.register(
        "session.interrupt",
        Duration::from_secs(35),
        move |caller, params| {
            let control = interrupt.clone();
            async move {
                let p: PaneParams = parse_params(params)?;
                let pane = require_control(control, caller, p.caller_server)?
                    .interrupt(p.target)
                    .await
                    .map_err(control_error)?;
                Ok(serde_json::json!({"pane": pane}))
            }
        },
    )?;
    let reply = control.clone();
    router.register(
        "session.reply",
        Duration::from_secs(35),
        move |caller, params| {
            let control = reply.clone();
            async move {
                let p: ReplyParams = parse_params(params)?;
                if p.option.is_some_and(|option| option > 99) {
                    return Err(MethodError::new(
                        error_code::INVALID_PARAMS,
                        "option must be at most 99",
                    ));
                }
                if (p.yes && p.no) || (p.option.is_some() && (p.yes || p.no)) {
                    return Err(MethodError::new(
                        error_code::INVALID_PARAMS,
                        "choose at most one of option, yes or no",
                    ));
                }
                let result = require_control(control, caller, p.caller_server)?
                    .reply(p.target, p.option, p.yes, p.no)
                    .await
                    .map_err(control_error)?;
                serde_json::to_value(result)
                    .map_err(|e| MethodError::new(error_code::INTERNAL, e.to_string()))
            }
        },
    )?;
    let capture = control.clone();
    router.register(
        "session.capture",
        Duration::from_secs(35),
        move |caller, params| {
            let control = capture.clone();
            async move {
                let p: CaptureParams = parse_params(params)?;
                let lines = require_control(control, caller, p.caller_server)?
                    .capture(p.target, p.tail, p.prompt)
                    .await
                    .map_err(control_error)?;
                Ok(serde_json::json!({"lines": lines}))
            }
        },
    )?;
    let panes = control.clone();
    router.register(
        "session.panes",
        Duration::from_secs(35),
        move |caller, params| {
            let control = panes.clone();
            async move {
                let p: PanesParams = parse_params(params)?;
                if !std::path::Path::new(&p.caller_server).is_absolute() {
                    return Err(MethodError::new(
                        error_code::INVALID_PARAMS,
                        "caller_server must be an absolute path",
                    ));
                }
                let ids = require_control(control, caller, Some(p.caller_server))?
                    .session_panes(p.session_name)
                    .await
                    .map_err(control_error)?;
                let mut pane_ids: Vec<_> = ids.into_iter().collect();
                pane_ids.sort();
                Ok(serde_json::json!({"pane_ids":pane_ids}))
            }
        },
    )?;
    let roles = control.clone();
    router.register(
        "session.roles",
        Duration::from_secs(35),
        move |caller, params| {
            let control = roles.clone();
            async move {
                let p: RolesParams = parse_params(params)?;
                if !std::path::Path::new(&p.caller_server).is_absolute() {
                    return Err(MethodError::new(
                        error_code::INVALID_PARAMS,
                        "caller_server must be an absolute path",
                    ));
                }
                let panes = require_control(control, caller, Some(p.caller_server))?
                    .roles()
                    .await
                    .map_err(control_error)?;
                let mut panes: Vec<_> = panes.into_iter().collect();
                panes.sort();
                Ok(serde_json::json!({"coordinator_panes":panes}))
            }
        },
    )?;
    let role_set = control.clone();
    router.register(
        "session.role_set",
        Duration::from_secs(35),
        move |caller, params| {
            let control = role_set.clone();
            async move {
                let p: RoleSetParams = parse_params(params)?;
                if !std::path::Path::new(&p.caller_server).is_absolute() {
                    return Err(MethodError::new(
                        error_code::INVALID_PARAMS,
                        "caller_server must be an absolute path",
                    ));
                }
                crate::control::reservation::PaneId::parse(p.pane.clone()).map_err(|error| {
                    MethodError::new(error_code::INVALID_PARAMS, error.to_string())
                })?;
                let on = match &p.role {
                    Value::Null => false,
                    Value::String(role) if role == "coordinator" => true,
                    _ => {
                        return Err(MethodError::new(
                            error_code::INVALID_PARAMS,
                            "role must be coordinator or null",
                        ))
                    }
                };
                require_control(control, caller, Some(p.caller_server))?
                    .role_set(p.pane.clone(), on)
                    .await
                    .map_err(control_error)?;
                Ok(serde_json::json!({"pane":p.pane,"role":p.role}))
            }
        },
    )?;
    let largest = control.clone();
    router.register(
        "session.spawn_prepare_largest",
        Duration::from_secs(35),
        move |caller, params| {
            let control = largest.clone();
            async move {
                let p: PrepareLargestParams = parse_params(params)?;
                if !std::path::Path::new(&p.caller_server).is_absolute() {
                    return Err(MethodError::new(
                        error_code::INVALID_PARAMS,
                        "caller_server must be an absolute path",
                    ));
                }
                let caller_pane = match p.caller_pane {
                    Value::Null => None,
                    Value::String(pane) => Some(pane),
                    _ => {
                        return Err(MethodError::new(
                            error_code::INVALID_PARAMS,
                            "caller_pane must be a string or null",
                        ))
                    }
                };
                let context = require_control(control, caller, Some(p.caller_server))?
                    .prepare_spawn_largest(caller_pane)
                    .await
                    .map_err(control_error)?;
                Ok(serde_json::json!({"target_pane":context.target_pane,"cwd":context.cwd}))
            }
        },
    )?;
    let prepare = control.clone();
    router.register("session.spawn_prepare", Duration::from_secs(35), move |caller, params| {
        let control = prepare.clone();
        async move {
            let p: PrepareParams = parse_params(params)?;
            let context = require_control(control, caller.clone(), p.caller_server)?.prepare_spawn(p.target_pane, p.cwd, caller.from_pane.map(|p| p.0), p.fallback_cwd).await.map_err(control_error)?;
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
                    crate::control::SpawnContext::new(p.target_pane, p.cwd, p.caller_pane_missing);
                let pane = require_control(control, caller, p.caller_server)?
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
                require_control(control, caller, p.caller_server)?
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

    // Break caught: org.tree is registered with a generic/different budget.
    // Stall its I/O boundary, retaining the production registration's deadline.
    #[tokio::test(start_paused = true)]
    async fn org_tree_registered_budget_expires_at_twelve_seconds() {
        use std::{future::Future, task::Poll};
        let mut router = router(MethodServices::new(crate::registry::spawn_registry())).unwrap();
        router.methods.get_mut("org.tree").unwrap().handler =
            Arc::new(|_, _| Box::pin(std::future::pending()));
        let mut call = Box::pin(router.call(context(), "org.tree", json!({})));
        std::future::poll_fn(|cx| {
            assert!(call.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        tokio::time::advance(Duration::from_secs(11)).await;
        std::future::poll_fn(|cx| {
            assert!(call.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        tokio::time::advance(Duration::from_secs(1)).await;
        std::future::poll_fn(|cx| {
            assert!(matches!(call.as_mut().poll(cx), Poll::Ready(Err(error)) if error.0.code == error_code::TIMEOUT));
            Poll::Ready(())
        }).await;
    }

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
        crate::control::service::ControlHandle,
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
        services.control = Some(control.clone());
        services.org = org;
        (router(services).unwrap(), mock, cancel, task, control)
    }

    #[tokio::test]
    async fn spawn_prepare_largest_requires_both_fields_and_advertises_when_disabled() {
        let router = router(MethodServices::new(crate::registry::spawn_registry())).unwrap();
        assert!(router
            .names()
            .iter()
            .any(|name| name == "session.spawn_prepare_largest"));
        for params in [
            Value::Null,
            json!([]),
            json!({}),
            json!({"caller_pane":null}),
            json!({"caller_server":"/private/server"}),
            json!({"caller_server":null,"caller_pane":null}),
            json!({"caller_server":7,"caller_pane":null}),
            json!({"caller_server":"","caller_pane":null}),
            json!({"caller_server":"relative","caller_pane":null}),
            json!({"caller_server":"/private/server","caller_pane":7}),
            json!({"caller_server":"/private/server","caller_pane":false}),
            json!({"caller_server":"/private/server","caller_pane":[],"extra":true}),
            json!({"caller_server":"/private/server","caller_pane":null,"extra":true}),
        ] {
            assert_eq!(
                router
                    .call(context(), "session.spawn_prepare_largest", params.clone())
                    .await
                    .unwrap_err()
                    .0
                    .code,
                "invalid_params",
                "{params}"
            );
        }
        for caller in [Value::Null, json!(""), json!("%999")] {
            assert_eq!(
                router
                    .call(
                        context(),
                        "session.spawn_prepare_largest",
                        json!({"caller_server":"/private/server","caller_pane":caller})
                    )
                    .await
                    .unwrap_err()
                    .0
                    .code,
                "unavailable"
            );
        }
    }

    #[tokio::test]
    async fn spawn_prepare_largest_preserves_selection_cwd_fallbacks_and_identity_order() {
        use atm_tmux::{mock::MockCall, PaneInfo};
        let dir = tempfile::tempdir().unwrap();
        let own = dir.path().join("own.sock");
        let foreign = dir.path().join("foreign.sock");
        let alias = dir.path().join("alias.sock");
        std::fs::write(&own, "").unwrap();
        std::fs::write(&foreign, "").unwrap();
        std::os::unix::fs::symlink(&own, &alias).unwrap();
        let org_cancel = tokio_util::sync::CancellationToken::new();
        let org = crate::org::OrgHandle::open(dir.path().join("state"), org_cancel.clone()).await;
        let (router, mock, cancel, task, control) = control_router(org.clone()).await;
        let pane = |id: &str, session: &str, window, width, height| PaneInfo {
            pane_id: id.into(),
            session_name: session.into(),
            window_index: window,
            pane_pid: 1,
            width,
            height,
            is_active: false,
        };
        let panes = vec![
            pane("%1", "work", 0, 400, 100),
            pane("%9", "work", 0, 80, 30),
            pane("%2", "work", 1, 60, 40),
            pane("%8", "other", 0, 600, 300),
        ];
        let identity = MockCall::RawCommand {
            subcommand: "display-message".into(),
            args: vec!["-p".into(), "#{socket_path}".into()],
        };
        for (caller, candidates, target, cwd) in [
            (Some("%1"), panes.clone(), "%2", Some("/selected's cwd")),
            (None, panes.clone(), "%8", None),
            (Some("%999"), panes.clone(), "%8", None),
            (Some(""), panes.clone(), "%8", None),
            (
                Some("%1"),
                vec![panes[0].clone(), panes[3].clone()],
                "%1",
                None,
            ),
            (Some("%999"), vec![], "%999", None),
            (Some(""), vec![], "", None),
            (None, vec![], "%0", None),
        ] {
            mock.clear_calls();
            mock.set_panes(candidates);
            if let Some(cwd) = cwd {
                mock.set_pane_cwd(target, cwd);
            }
            mock.set_next_raw_output(own.to_str().unwrap());
            assert_eq!(
                router
                    .call(
                        context(),
                        "session.spawn_prepare_largest",
                        json!({"caller_server":own,"caller_pane":caller})
                    )
                    .await
                    .unwrap(),
                json!({"target_pane":target,"cwd":cwd})
            );
            assert_eq!(
                mock.calls(),
                vec![
                    identity.clone(),
                    MockCall::ListPanes,
                    MockCall::GetPaneCwd {
                        pane: target.into()
                    }
                ]
            );
        }
        for (server, allowed) in [
            (&alias, true),
            (&foreign, false),
            (&dir.path().join("gone.sock"), false),
        ] {
            mock.clear_calls();
            mock.set_next_raw_output(own.to_str().unwrap());
            let result = router
                .call(
                    context(),
                    "session.spawn_prepare_largest",
                    json!({"caller_server":server,"caller_pane":"%1"}),
                )
                .await;
            if allowed {
                assert!(result.is_ok());
            } else {
                assert_eq!(result.unwrap_err().0.code, "refused");
                assert_eq!(
                    mock.calls(),
                    vec![identity.clone()],
                    "foreign equal caller id must not read local panes or cwd"
                );
            }
        }
        control.flush_audits().await;
        assert!(
            org.tail(crate::org::TailParams::default())
                .await
                .unwrap()
                .is_empty(),
            "preparation must not audit"
        );
        cancel.cancel();
        task.await.unwrap();
        org_cancel.cancel();
    }

    // Breaks if missing identity/role defaults to an unguarded read or clear,
    // or malformed parameters reach the control service.
    #[tokio::test]
    async fn session_roles_validate_before_admission_and_advertise_when_disabled() {
        let router = router(MethodServices::new(crate::registry::spawn_registry())).unwrap();
        for method in ["session.roles", "session.role_set"] {
            assert!(router.names().iter().any(|name| name == method));
            let valid = if method == "session.roles" {
                json!({"caller_server":"/private/server"})
            } else {
                json!({"caller_server":"/private/server","pane":"%7","role":null})
            };
            let mut invalid = vec![Value::Null, json!([]), json!({})];
            for server in [Value::Null, json!(7), json!(""), json!("relative")] {
                let mut params = valid.clone();
                params["caller_server"] = server;
                invalid.push(params);
            }
            let mut missing = valid.clone();
            missing.as_object_mut().unwrap().remove("caller_server");
            invalid.push(missing);
            let mut extra = valid.clone();
            extra["extra"] = json!(true);
            invalid.push(extra);
            if method == "session.role_set" {
                let mut missing_role = valid.clone();
                missing_role.as_object_mut().unwrap().remove("role");
                invalid.push(missing_role);
                let mut missing_pane = valid.clone();
                missing_pane.as_object_mut().unwrap().remove("pane");
                invalid.push(missing_pane);
                for role in [
                    json!("implementer"),
                    json!("Coordinator"),
                    json!(false),
                    json!(1),
                    json!({}),
                ] {
                    let mut params = valid.clone();
                    params["role"] = role;
                    invalid.push(params);
                }
                for pane in [
                    Value::Null,
                    json!(7),
                    json!(""),
                    json!("%"),
                    json!("%00"),
                    json!("%01"),
                    json!("%١"),
                    json!("work:0"),
                    json!("%7;"),
                    json!(" %7"),
                ] {
                    let mut params = valid.clone();
                    params["pane"] = pane;
                    invalid.push(params);
                }
            }
            for params in invalid {
                assert_eq!(
                    router
                        .call(context(), method, params.clone())
                        .await
                        .unwrap_err()
                        .0
                        .code,
                    "invalid_params",
                    "{method} {params}"
                );
            }
            assert_eq!(
                router
                    .call(context(), method, valid)
                    .await
                    .unwrap_err()
                    .0
                    .code,
                "unavailable"
            );
        }
    }

    // Breaks if role reads classify registry rows, lose exact-tag parsing or
    // sorting, or either operation touches role data before the server guard.
    #[tokio::test]
    async fn session_roles_exact_tags_and_writes_follow_the_server_fence() {
        use atm_tmux::mock::MockCall;
        let dir = tempfile::tempdir().unwrap();
        let own = dir.path().join("own.sock");
        let other = dir.path().join("other.sock");
        let alias = dir.path().join("alias.sock");
        std::fs::write(&own, "").unwrap();
        std::fs::write(&other, "").unwrap();
        std::os::unix::fs::symlink(&own, &alias).unwrap();
        let org_cancel = tokio_util::sync::CancellationToken::new();
        let org = crate::org::OrgHandle::open(dir.path().join("state"), org_cancel.clone()).await;
        let (router, mock, cancel, task, control) = control_router(org.clone()).await;
        let identity = MockCall::RawCommand {
            subcommand: "display-message".into(),
            args: vec!["-p".into(), "#{socket_path}".into()],
        };
        for (output, expected) in [
            ("%7\tcoordinator\n%2\tcoordinator\n%7\tcoordinator\n%3\timplementer\n%4\tCoordinator\n%5\tcoordinator \n", json!(["%2","%7"])),
            ("%2\t\n%7\timplementer\n", json!([])),
            ("", json!([])),
        ] {
            mock.clear_calls();
            mock.set_next_raw_output(own.to_str().unwrap());
            mock.set_next_raw_output(output);
            assert_eq!(router.call(context(), "session.roles", json!({"caller_server":own})).await.unwrap(),
                json!({"coordinator_panes":expected}));
            assert_eq!(mock.calls(), vec![identity.clone(), MockCall::RawCommand {
                subcommand: "list-panes".into(), args: vec!["-a".into(), "-F".into(), "#{pane_id}\t#{@atm-role}".into()]
            }]);
        }
        for role in [json!("coordinator"), Value::Null, Value::Null] {
            mock.clear_calls();
            mock.set_next_raw_output(own.to_str().unwrap());
            assert_eq!(
                router
                    .call(
                        context(),
                        "session.role_set",
                        json!({"caller_server":own,"pane":"%7","role":role})
                    )
                    .await
                    .unwrap(),
                json!({"pane":"%7","role":role})
            );
            let args = if role.is_null() {
                vec!["-pu", "-t", "%7", "@atm-role"]
            } else {
                vec!["-p", "-t", "%7", "@atm-role", "coordinator"]
            };
            assert_eq!(
                mock.calls(),
                vec![
                    identity.clone(),
                    MockCall::RawCommand {
                        subcommand: "set-option".into(),
                        args: args.into_iter().map(str::to_owned).collect()
                    }
                ]
            );
        }
        for method in ["session.roles", "session.role_set"] {
            for (server, permitted) in [
                (&alias, true),
                (&other, false),
                (&dir.path().join("missing.sock"), false),
            ] {
                mock.clear_calls();
                mock.set_next_raw_output(own.to_str().unwrap());
                let params = if method == "session.roles" {
                    json!({"caller_server":server})
                } else {
                    json!({"caller_server":server,"pane":"%7","role":"coordinator"})
                };
                let result = router.call(context(), method, params).await;
                if permitted {
                    assert!(result.is_ok(), "{result:?}");
                    assert_eq!(mock.call_count(), 2);
                } else {
                    assert_eq!(result.unwrap_err().0.code, "refused");
                    assert_eq!(
                        mock.calls(),
                        vec![identity.clone()],
                        "foreign equal pane id must not read/write local roles"
                    );
                }
            }
        }
        control.flush_audits().await;
        assert!(org
            .tail(crate::org::TailParams::default())
            .await
            .unwrap()
            .is_empty());
        cancel.cancel();
        task.await.unwrap();
        org_cancel.cancel();
    }

    // Breaks if membership uses tmux prefix targets, leaks foreign-server ids,
    // loses ordering/dedup, or treats an unmatched session as a failed read.
    #[tokio::test]
    async fn session_panes_exact_membership_and_server_fence() {
        use atm_tmux::{mock::MockCall, PaneInfo};
        let dir = tempfile::tempdir().unwrap();
        let own = dir.path().join("own.sock");
        let other = dir.path().join("other.sock");
        let alias = dir.path().join("alias.sock");
        std::fs::write(&own, "").unwrap();
        std::fs::write(&other, "").unwrap();
        std::os::unix::fs::symlink(&own, &alias).unwrap();
        let org_cancel = tokio_util::sync::CancellationToken::new();
        let org = crate::org::OrgHandle::open(dir.path().join("state"), org_cancel.clone()).await;
        let (router, mock, cancel, task, control) = control_router(org.clone()).await;
        let pane = |id: &str, session: &str, window_index| PaneInfo {
            pane_id: id.into(),
            session_name: session.into(),
            window_index,
            pane_pid: 1,
            width: 80,
            height: 24,
            is_active: true,
        };
        mock.set_panes(vec![
            pane("%2", "work", 1),
            pane("%0", "work", 0),
            pane("%1", "work-extra", 0),
            pane("%3", "Work", 0),
            pane("%0", "work", 0),
        ]);
        for (name, ids) in [
            ("work", json!(["%0", "%2"])),
            ("work-extra", json!(["%1"])),
            ("Work", json!(["%3"])),
            ("wor", json!([])),
            ("missing", json!([])),
            ("", json!([])),
        ] {
            mock.clear_calls();
            mock.set_next_raw_output(own.to_str().unwrap());
            assert_eq!(
                router
                    .call(
                        context(),
                        "session.panes",
                        json!({"session_name":name,"caller_server":own})
                    )
                    .await
                    .unwrap(),
                json!({"pane_ids":ids})
            );
            assert_eq!(
                mock.calls(),
                vec![
                    MockCall::RawCommand {
                        subcommand: "display-message".into(),
                        args: vec!["-p".into(), "#{socket_path}".into()],
                    },
                    MockCall::ListPanes
                ]
            );
        }
        for (server, permitted) in [
            (&alias, true),
            (&other, false),
            (&dir.path().join("missing.sock"), false),
        ] {
            mock.clear_calls();
            mock.set_next_raw_output(own.to_str().unwrap());
            let result = router
                .call(
                    context(),
                    "session.panes",
                    json!({"session_name":"work","caller_server":server}),
                )
                .await;
            if permitted {
                assert_eq!(result.unwrap(), json!({"pane_ids":["%0","%2"]}));
            } else {
                assert_eq!(result.unwrap_err().0.code, "refused");
                assert_eq!(
                    mock.call_count(),
                    1,
                    "foreign identity must precede every membership read"
                );
                assert!(!mock.calls().contains(&MockCall::ListPanes));
            }
        }
        control.flush_audits().await;
        assert!(
            org.tail(crate::org::TailParams::default())
                .await
                .unwrap()
                .is_empty(),
            "read-only membership must not audit"
        );
        cancel.cancel();
        task.await.unwrap();
        org_cancel.cancel();
    }

    // Breaks if an omitted/relative server bypasses the guard or misspelled
    // fields silently select a different filter; validation precedes services.
    #[tokio::test]
    async fn session_panes_validates_required_server_before_service_admission() {
        let router = router(MethodServices::new(crate::registry::spawn_registry())).unwrap();
        assert!(router.names().iter().any(|name| name == "session.panes"));
        for params in [
            Value::Null,
            json!([]),
            json!({}),
            json!({"session_name":"work"}),
            json!({"caller_server":"/private/server"}),
            json!({"session_name":null,"caller_server":"/private/server"}),
            json!({"session_name":7,"caller_server":"/private/server"}),
            json!({"session_name":"work","caller_server":null}),
            json!({"session_name":"work","caller_server":true}),
            json!({"session_name":"work","caller_server":""}),
            json!({"session_name":"work","caller_server":"relative"}),
            json!({"session_name":"work","caller_server":"/private/server","extra":true}),
        ] {
            assert_eq!(
                router
                    .call(context(), "session.panes", params.clone())
                    .await
                    .unwrap_err()
                    .0
                    .code,
                "invalid_params",
                "{params}"
            );
        }
        assert_eq!(
            router
                .call(
                    context(),
                    "session.panes",
                    json!({"session_name":"work","caller_server":"/private/server"})
                )
                .await
                .unwrap_err()
                .0
                .code,
            "unavailable"
        );
    }

    #[tokio::test]
    async fn reply_accepts_option_99_but_refuses_100_before_control_work() {
        let dir = tempfile::tempdir().unwrap();
        let org_cancel = tokio_util::sync::CancellationToken::new();
        let org = crate::org::OrgHandle::open(dir.path().into(), org_cancel.clone()).await;
        let (router, mock, cancel, task, _) = control_router(org).await;
        mock.set_pane_content("%7", vec!["❯ 98. previous".into(), "  99. chosen".into()]);
        assert_eq!(
            router
                .call(
                    context(),
                    "session.reply",
                    json!({"target":"%7","option":99})
                )
                .await
                .unwrap(),
            json!({"action":"selected","pane":"%7","option":99})
        );
        assert!(mock.calls().ends_with(&[
            atm_tmux::mock::MockCall::SendKeys {
                pane: "%7".into(),
                keys: "Down".into()
            },
            atm_tmux::mock::MockCall::SendKeys {
                pane: "%7".into(),
                keys: "Enter".into()
            },
        ]));
        mock.clear_calls();
        let error = router
            .call(
                context(),
                "session.reply",
                json!({"target":"%7","option":100}),
            )
            .await
            .unwrap_err();
        assert_eq!(error.0.code, error_code::INVALID_PARAMS);
        assert_eq!(error.0.message, "option must be at most 99");
        assert!(
            mock.calls().is_empty(),
            "invalid options must not enter the actor"
        );
        cancel.cancel();
        task.await.unwrap();
        org_cancel.cancel();
    }

    #[tokio::test]
    async fn reply_and_capture_keep_legacy_results_and_only_reply_is_audited() {
        let dir = tempfile::tempdir().unwrap();
        let org_cancel = tokio_util::sync::CancellationToken::new();
        let org = crate::org::OrgHandle::open(dir.path().into(), org_cancel.clone()).await;
        let (router, mock, cancel, task, control) = control_router(org.clone()).await;
        mock.set_pane_content(
            "%7",
            vec![
                "earlier".into(),
                "Choose café?".into(),
                "❯ 1. Sí".into(),
                "  2. No".into(),
                "Enter to select".into(),
                "".into(),
            ],
        );
        for (params, expected) in [
            (
                json!({"target":"audit"}),
                json!({"action":"enter","pane":"%7"}),
            ),
            (
                json!({"target":"%7","yes":true}),
                json!({"action":"enter","pane":"%7"}),
            ),
            (
                json!({"target":"%7","no":true}),
                json!({"action":"escape","pane":"%7"}),
            ),
            (
                json!({"target":"%7","option":2}),
                json!({"action":"selected","pane":"%7","option":2}),
            ),
        ] {
            assert_eq!(
                router
                    .call(context(), "session.reply", params)
                    .await
                    .unwrap(),
                expected
            );
        }
        assert_eq!(
            router
                .call(
                    context(),
                    "session.capture",
                    json!({"target":"audit","tail":0})
                )
                .await
                .unwrap(),
            json!({"lines":[]})
        );
        assert_eq!(
            router
                .call(
                    context(),
                    "session.capture",
                    json!({"target":"%7","tail":0,"prompt":true})
                )
                .await
                .unwrap(),
            json!({"lines":["Choose café?","❯ 1. Sí","  2. No","Enter to select"]})
        );
        assert_eq!(
            router
                .call(context(), "session.capture", json!({"target":"%7"}))
                .await
                .unwrap(),
            json!({"lines":["earlier","Choose café?","❯ 1. Sí","  2. No","Enter to select",""]})
        );
        for (method, params) in [
            ("session.reply", json!({"target":"%7","yes":true,"no":true})),
            (
                "session.reply",
                json!({"target":"%7","yes":true,"option":2}),
            ),
            ("session.reply", json!({"target":"%7","option":-1})),
            ("session.reply", json!({"target":"%7","extra":true})),
            ("session.capture", json!({"target":"%7","tail":-1})),
            ("session.capture", json!({"target":"%7","prompt":"yes"})),
            ("session.capture", json!({"target":"%7","extra":true})),
        ] {
            assert_eq!(
                router
                    .call(context(), method, params)
                    .await
                    .unwrap_err()
                    .0
                    .code,
                error_code::INVALID_PARAMS
            );
        }
        for method in ["session.reply", "session.capture"] {
            let error = router
                .call(context(), method, json!({"target":"missing"}))
                .await
                .unwrap_err();
            assert_eq!(error.0.code, error_code::NOT_FOUND);
            assert_eq!(error.0.message, "No session matching 'missing'");
        }
        control.flush_audits().await;
        let records = org.tail(crate::org::TailParams::default()).await.unwrap();
        assert_eq!(records.len(), 4, "read-only capture must not flood the log");
        for record in records {
            assert_eq!(record.kind, "control.sent");
            assert_eq!(
                record.body,
                json!({"pane":"%7","action":{"method":"session.reply","outcome":"ok","error":null,"client_id":"client","from_pane":"%12"}})
            );
        }
        cancel.cancel();
        task.await.unwrap();
        org_cancel.cancel();
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
    }

    #[tokio::test]
    async fn control_actions_append_actual_panes_and_attribution_without_payloads() {
        let dir = tempfile::tempdir().unwrap();
        let org_cancel = tokio_util::sync::CancellationToken::new();
        let org = crate::org::OrgHandle::open(dir.path().into(), org_cancel.clone()).await;
        let (router, mock, cancel, task, control) = control_router(org).await;
        successful_control_actions(&router).await;
        control.flush_audits().await;
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
    async fn control_refuses_other_tmux_servers_but_accepts_aliases_and_legacy_requests() {
        let dir = tempfile::tempdir().unwrap();
        let own = dir.path().join("own.sock");
        let other = dir.path().join("other.sock");
        let alias = dir.path().join("alias.sock");
        std::fs::write(&own, "").unwrap();
        std::fs::write(&other, "").unwrap();
        std::os::unix::fs::symlink(&own, &alias).unwrap();
        let org_cancel = tokio_util::sync::CancellationToken::new();
        let org = crate::org::OrgHandle::open(dir.path().join("state"), org_cancel.clone()).await;
        let (router, mock, cancel, task, _) = control_router(org).await;
        for (method, params) in [
            ("session.send", json!({"target":"%7","text":"hello"})),
            ("session.spawn_prepare", json!({"target_pane":"%7"})),
            (
                "session.spawn",
                json!({"target_pane":"%7","caller_pane_missing":false,"direction":"below","size":"50%"}),
            ),
            (
                "session.spawn_type",
                json!({"pane":"%42","command":"hello"}),
            ),
        ] {
            for caller in [Some(&other), Some(&alias), None] {
                mock.clear_calls();
                let mut params = params.clone();
                if let Some(caller) = caller {
                    params["caller_server"] = json!(caller);
                    mock.set_next_raw_output(own.to_str().unwrap());
                }
                if method == "session.send" && caller != Some(&other) {
                    mock.set_next_raw_output("0 1");
                }
                let result = router.call(context(), method, params).await;
                if caller == Some(&other) {
                    let error = result.unwrap_err();
                    assert_eq!(error.0.code, "refused");
                    assert!(error.0.message.contains("atmd controls tmux server"));
                    assert_eq!(mock.call_count(), 1, "mismatch must not mutate");
                } else {
                    result.unwrap();
                }
            }
        }
        cancel.cancel();
        task.await.unwrap();
        org_cancel.cancel();
    }

    #[tokio::test]
    async fn spawn_passes_optional_shell_start_directory_to_tmux() {
        let (router, mock, cancel, task, _) =
            control_router(crate::org::OrgHandle::disabled("test")).await;
        for cwd in [Some("/caller"), None] {
            router.call(context(), "session.spawn", json!({"target_pane":"%7","caller_pane_missing":false,"direction":"below","size":"50%","cwd":cwd})).await.unwrap();
            assert!(
                matches!(mock.calls().last(), Some(atm_tmux::mock::MockCall::SplitWindow { cwd: got, .. }) if got.as_deref() == cwd)
            );
        }
        cancel.cancel();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn disabled_organisation_does_not_change_or_repeat_control_actions() {
        let (router, mock, cancel, task, control) =
            control_router(crate::org::OrgHandle::disabled("test disabled")).await;
        mock.set_next_raw_output("ATM-PANE-v1\t%7\tprivate\t0\t123\t80\t24\t1\t0\t0\t1\t0\t0\n");
        mock.set_next_raw_output(""); // untagged send pane
        mock.set_next_raw_output("0 1");
        mock.set_next_raw_output("ATM-PANE-v1\t%42\tprivate\t0\t123\t80\t24\t1\t0\t0\t1\t0\t0\n");
        mock.set_next_raw_output(""); // untagged spawn_type pane
        successful_control_actions(&router).await;
        control.flush_audits().await;
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
        let (router, mock, cancel, task, control) = control_router(org).await;
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
        control.flush_audits().await;
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
        control.flush_audits().await;
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
        control.flush_audits().await;
        mock.set_next_error(atm_tmux::TmuxError::NotFound);
        let error = router.call(context(), "session.spawn", json!({"target_pane":"%7","caller_pane_missing":false,"direction":"below","size":"50%"})).await.unwrap_err();
        assert_eq!(error.0.code, "internal");
        control.flush_audits().await;
        let error = router
            .call(
                context(),
                "session.send",
                json!({"target":"missing-session","text":"no"}),
            )
            .await
            .unwrap_err();
        assert_eq!(error.0.code, "not_found");
        control.flush_audits().await;
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

    #[test]
    fn contract_quarantine_and_org_failure_have_unavailable_code_and_human_messages() {
        use crate::control::ControlError;
        for (error, message) in [
            (
                ControlError::PaneQuarantined {
                    pane: "%7".into(),
                    agent: "project/lead".parse().unwrap(),
                },
                "pane %7: agent project/lead is quarantined; restart atmd and reconcile",
            ),
            (
                ControlError::OrganisationUnavailable { pane: "%7".into() },
                "organisation state is unavailable for pane %7; refusing input",
            ),
        ] {
            let error = control_error(error.into());
            assert_eq!(error.0.code, "unavailable");
            assert_eq!(error.0.message, message);
        }
    }

    #[tokio::test]
    async fn kill_and_interrupt_reach_the_actor_and_a_clear_refuses_only_the_interrupt() {
        use crate::control::reservation::{
            PaneId, ReservationError, ReservationKind, ReservationRequest, WaitPolicy,
        };
        use atm_tmux::mock::MockCall;
        let (router, mock, cancel, task, control) =
            control_router(crate::org::OrgHandle::disabled("pane request router test")).await;
        let call = |method: &'static str| router.call(context(), method, json!({"target":"audit"}));
        assert_eq!(
            call("session.interrupt").await.unwrap(),
            json!({"pane":"%7"})
        );
        let (entered, entry) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel::<()>();
        let clear = tokio::spawn(async move {
            control
                .run_reserved(
                    ReservationRequest::new(
                        "project/lead".parse().unwrap(),
                        Some(PaneId::parse("%7").unwrap()),
                        ReservationKind::Clear,
                        WaitPolicy::Wait,
                        tokio::time::Instant::now() + Duration::from_secs(60),
                    ),
                    move |_| async move {
                        entered.send(()).unwrap();
                        released.await.unwrap();
                        Ok::<_, ReservationError>(())
                    },
                    |_, _| async { Ok(()) },
                )
                .await
        });
        entry.await.unwrap();
        let error = call("session.interrupt").await.unwrap_err();
        assert_eq!(error.0.code, "refused");
        assert_eq!(
            error.0.message,
            "Refusing to interrupt %7: agent project/lead is clearing; try again in a moment"
        );
        assert_eq!(call("session.kill").await.unwrap(), json!({"pane":"%7"}));
        release.send(()).unwrap();
        clear.await.unwrap().unwrap();
        let effects: Vec<_> = mock
            .calls()
            .into_iter()
            .filter(|call| !matches!(call, MockCall::RawCommand { .. }))
            .collect();
        assert!(
            matches!(effects.as_slice(), [MockCall::SendKeys { pane, keys }, MockCall::KillPane { pane: killed }]
                if pane == "%7" && keys == "C-c" && killed == "%7"),
            "{effects:?}"
        );
        cancel.cancel();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn contract_clearing_error_reaches_method_response_unchanged() {
        let mut router = MethodRouter::default();
        router
            .register("test.clearing", Duration::from_secs(1), |_, _| async {
                Err(control_error(
                    crate::control::ControlError::AgentClearing {
                        pane: "%7".into(),
                        agent: "project/lead".parse().unwrap(),
                    }
                    .into(),
                ))
            })
            .unwrap();
        let error = router
            .call(context(), "test.clearing", Value::Null)
            .await
            .unwrap_err();
        assert_eq!(error.0.code, "refused");
        assert_eq!(
            error.0.message,
            "Refusing to send to %7: agent project/lead is clearing; try again in a moment"
        );
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
