//! Serialized, deadline-bounded session control. Dropping a waiting caller does
//! not cancel an admitted action or retry its possibly partial effects.
//! Replies acknowledge control, not persistence. A bounded FIFO forwards audits
//! independently; failures warn per audit, and overflow counts drops without
//! delaying or changing any control result.

#[cfg(test)]
mod tests {
    use super::*;
    use atm_tmux::{mock::MockCall, MockTmuxClient};

    #[tokio::test]
    async fn actions_use_owned_client_and_spawn_stages_do_not_type_early() {
        let mock = MockTmuxClient::new();
        mock.set_next_raw_output("0 1");
        mock.set_next_pane_id("%42");
        mock.set_pane_cwd("%7", "/pane");
        let cancel = CancellationToken::new();
        let (control, task) = spawn_control(
            Box::new(mock.clone()),
            crate::registry::spawn_registry(),
            OrgHandle::disabled("test disabled"),
            cancel.clone(),
        );
        assert_eq!(
            control.send("%7".into(), "hello;".into()).await.unwrap(),
            "%7"
        );
        assert!(
            matches!(mock.calls().last(), Some(MockCall::SendKeys { keys, .. }) if keys == "Enter")
        );
        mock.clear_calls();
        let context = control
            .prepare_spawn(None, None, Some("%7".into()), Some("/fallback".into()))
            .await
            .unwrap();
        assert_eq!(context.cwd.as_deref(), Some("/pane"));
        assert!(context.caller_pane_missing());
        let missing = context.caller_pane_missing();
        let reconstructed = SpawnContext::new(context.target_pane, None, missing);
        assert_eq!(
            control
                .split_spawn(reconstructed, PaneDirection::Below, "50%".into())
                .await
                .unwrap(),
            "%42"
        );
        assert!(!mock
            .calls()
            .iter()
            .any(|call| matches!(call, MockCall::SendKeys { .. })));
        control
            .type_spawn_command("%42".into(), "printf probe;".into())
            .await
            .unwrap();
        assert!(
            matches!(mock.calls().last(), Some(MockCall::SendKeys { pane, keys }) if pane == "%42" && keys == "Enter")
        );
        cancel.cancel();
        task.await.unwrap();
        assert!(matches!(
            control.send("%7".into(), "hi".into()).await,
            Err(ServiceError::Unavailable)
        ));
    }

    #[tokio::test]
    async fn refusal_and_tmux_failures_are_values_without_further_effects() {
        let mock = MockTmuxClient::new();
        mock.set_next_raw_output("1 0");
        let (control, task) = spawn_control(
            Box::new(mock.clone()),
            crate::registry::spawn_registry(),
            OrgHandle::disabled("test disabled"),
            CancellationToken::new(),
        );
        let error = control.send("%7".into(), "hi".into()).await.unwrap_err();
        assert_eq!(error.to_string(), "Refusing to send to %7: the pane is in copy mode (or another mode), so Enter would go to tmux");
        assert_eq!(mock.call_count(), 1);
        mock.clear_calls();
        mock.set_next_error(atm_tmux::TmuxError::NotFound);
        let error = control
            .type_spawn_command("%42".into(), "probe".into())
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "Failed to send command to new pane");
        assert_eq!(mock.call_count(), 1);
        drop(control);
        task.await.unwrap();
    }

    #[tokio::test]
    async fn expired_queued_actions_never_touch_tmux_even_if_caller_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        let org = OrgHandle::open(dir.path().into(), cancel.clone()).await;
        let mock = MockTmuxClient::new();
        let (reply, response) = oneshot::channel();
        drop(response);
        execute(
            &mock,
            &crate::registry::spawn_registry(),
            Pending {
                deadline: Instant::now(),
                caller: None,
                request: Request::Type {
                    pane: "%7".into(),
                    command: "late".into(),
                    reply,
                },
            },
        )
        .await;
        assert_eq!(mock.call_count(), 0);
        assert_eq!(
            org.status().unwrap(),
            0,
            "expired queued work is not an action"
        );
        cancel.cancel();
    }

    #[tokio::test]
    async fn admitted_action_is_audited_even_when_its_receiver_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let org_cancel = CancellationToken::new();
        let org = OrgHandle::open(dir.path().into(), org_cancel.clone()).await;
        let mock = MockTmuxClient::new();
        let cancel = CancellationToken::new();
        let (control, task) = spawn_control(
            Box::new(mock.clone()),
            crate::registry::spawn_registry(),
            org.clone(),
            cancel.clone(),
        );
        let (reply, response) = oneshot::channel();
        drop(response);
        control
            .sender
            .send(Pending {
                deadline: Instant::now() + ACTION_TIMEOUT,
                caller: None,
                request: Request::Type {
                    pane: "%42".into(),
                    command: "once".into(),
                    reply,
                },
            })
            .await
            .unwrap();
        control.flush_audits().await;
        let records = org.tail(crate::org::TailParams::default()).await.unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].kind, "control.sent");
        assert_eq!(records[0].body["pane"], "%42");
        assert_eq!(records[0].body["action"]["outcome"], "ok");
        assert_eq!(
            records[0].body["action"]["client_id"],
            serde_json::Value::Null
        );
        assert_eq!(
            mock.calls()
                .iter()
                .filter(|call| matches!(call, MockCall::SendKeys { .. }))
                .count(),
            2
        );
        cancel.cancel();
        task.await.unwrap();
        org_cancel.cancel();
    }

    #[tokio::test]
    async fn stalled_audit_never_holds_either_control_reply_and_preserves_order() {
        tokio::time::timeout(Duration::from_secs(30), async {
            let dir = tempfile::tempdir().unwrap();
            let org_cancel = CancellationToken::new();
            let org = OrgHandle::open(dir.path().into(), org_cancel.clone()).await;
            let gate = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
            let entered = std::sync::Arc::new(tokio::sync::Notify::new());
            let cancel = CancellationToken::new();
            let (control, task) = spawn_control_with_audit(
                Box::new(MockTmuxClient::new()),
                crate::registry::spawn_registry(),
                cancel.clone(),
                {
                    let gate = gate.clone();
                    let entered = entered.clone();
                    let org = org.clone();
                    move |audit| {
                        let gate = gate.clone();
                        let entered = entered.clone();
                        let org = org.clone();
                        async move {
                            entered.notify_one();
                            let _permit = gate.acquire().await.unwrap();
                            org.execute(audit, chrono::Utc::now()).await.unwrap();
                        }
                    }
                },
            );
            control
                .type_spawn_command("%1".into(), "one".into())
                .await
                .unwrap();
            entered.notified().await;
            // The first audit is blocked, yet a second action must reply.
            control
                .type_spawn_command("%2".into(), "two".into())
                .await
                .unwrap();
            // Fill and overflow the bounded FIFO while persistence is still held.
            for pane in 3..=AUDIT_CAPACITY + 3 {
                control
                    .type_spawn_command(format!("%{pane}"), "overflow".into())
                    .await
                    .unwrap();
            }
            gate.add_permits(1);
            drop(control);
            task.await.unwrap();
            let events = org.tail(crate::org::TailParams::default()).await.unwrap();
            assert_eq!(
                events
                    .iter()
                    .map(|e| e.body["pane"].as_str().unwrap().to_owned())
                    .collect::<Vec<_>>(),
                (1..=AUDIT_CAPACITY + 1)
                    .map(|pane| format!("%{pane}"))
                    .collect::<Vec<_>>()
            );
            org_cancel.cancel();
        })
        .await
        .expect("control replies must not wait for the held audit barrier");
    }

    #[tokio::test(start_paused = true)]
    async fn queue_admission_and_stalled_action_share_a_bounded_budget() {
        let (sender, _receiver) = mpsc::channel(CAPACITY);
        for _ in 0..CAPACITY {
            let (reply, _) = oneshot::channel();
            sender
                .try_send(Pending {
                    deadline: Instant::now() + ACTION_TIMEOUT,
                    caller: None,
                    request: Request::Type {
                        pane: "%7".into(),
                        command: "queued".into(),
                        reply,
                    },
                })
                .ok()
                .unwrap();
        }
        let handle = ControlHandle {
            sender,
            caller: None,
        };
        assert!(matches!(
            handle.send("%7".into(), "blocked".into()).await,
            Err(ServiceError::Timeout)
        ));
        assert!(matches!(
            outcome::<()>(Instant::now() + ACTION_TIMEOUT, std::future::pending()).await,
            Err(ServiceError::Timeout)
        ));
    }
}

use std::time::Duration;

use atm_org::{ControlAudit, OrgCommand};
use atm_tmux::{PaneDirection, TmuxClient};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{timeout_at, Instant};
use tokio_util::sync::CancellationToken;

use super::{ControlError, SpawnContext};
use crate::{org::OrgHandle, registry::RegistryHandle, server::methods::CallContext};

const CAPACITY: usize = 32;
const ACTION_TIMEOUT: Duration = Duration::from_secs(30);
// Absorb one full control-queue burst without unbounded log-backpressure memory.
const AUDIT_CAPACITY: usize = CAPACITY;
const AUDIT_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

enum Audit {
    Record(OrgCommand),
    #[cfg(test)]
    Barrier(Reply<()>),
}

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error(transparent)]
    Control(#[from] ControlError),
    #[error("control service is unavailable")]
    Unavailable,
    #[error("control action timed out")]
    Timeout,
}

type Reply<T> = oneshot::Sender<Result<T, ServiceError>>;

enum Request {
    #[cfg(test)]
    FlushAudit(Reply<()>),
    Send {
        target: String,
        text: String,
        reply: Reply<String>,
    },
    Prepare {
        target: Option<String>,
        cwd: Option<String>,
        caller: Option<String>,
        fallback: Option<String>,
        reply: Reply<SpawnContext>,
    },
    Split {
        context: SpawnContext,
        direction: PaneDirection,
        size: String,
        reply: Reply<String>,
    },
    Type {
        pane: String,
        command: String,
        reply: Reply<()>,
    },
}

struct Pending {
    deadline: Instant,
    caller: Option<CallContext>,
    request: Request,
}

/// A bounded queue shared by RPC and in-process control clients.
#[derive(Debug, Clone)]
pub struct ControlHandle {
    sender: mpsc::Sender<Pending>,
    caller: Option<CallContext>,
}

impl ControlHandle {
    #[cfg(test)]
    pub(crate) async fn flush_audits(&self) {
        self.request(Request::FlushAudit).await.unwrap();
    }

    /// Attribute subsequent calls; this client-supplied identity is not authentication.
    #[must_use]
    pub fn with_caller(mut self, caller: CallContext) -> Self {
        self.caller = Some(caller);
        self
    }

    async fn request<T>(&self, make: impl FnOnce(Reply<T>) -> Request) -> Result<T, ServiceError> {
        let deadline = Instant::now() + ACTION_TIMEOUT;
        let (reply, response) = oneshot::channel();
        timeout_at(deadline, async {
            self.sender
                .send(Pending {
                    deadline,
                    caller: self.caller.clone(),
                    request: make(reply),
                })
                .await
                .map_err(|_| ServiceError::Unavailable)?;
            response.await.map_err(|_| ServiceError::Unavailable)?
        })
        .await
        .map_err(|_| ServiceError::Timeout)?
    }

    pub async fn send(&self, target: String, text: String) -> Result<String, ServiceError> {
        self.request(|reply| Request::Send {
            target,
            text,
            reply,
        })
        .await
    }

    pub async fn prepare_spawn(
        &self,
        target: Option<String>,
        cwd: Option<String>,
        caller: Option<String>,
        fallback: Option<String>,
    ) -> Result<SpawnContext, ServiceError> {
        self.request(|reply| Request::Prepare {
            target,
            cwd,
            caller,
            fallback,
            reply,
        })
        .await
    }

    pub async fn split_spawn(
        &self,
        context: SpawnContext,
        direction: PaneDirection,
        size: String,
    ) -> Result<String, ServiceError> {
        self.request(|reply| Request::Split {
            context,
            direction,
            size,
            reply,
        })
        .await
    }

    pub async fn type_spawn_command(
        &self,
        pane: String,
        command: String,
    ) -> Result<(), ServiceError> {
        self.request(|reply| Request::Type {
            pane,
            command,
            reply,
        })
        .await
    }
}

/// Start only at the daemon's I/O boundary, not when constructing services.
/// The owner cancels and joins this task before dropping its registry.
pub fn spawn_control(
    client: Box<dyn TmuxClient>,
    registry: RegistryHandle,
    org: OrgHandle,
    cancel: CancellationToken,
) -> (ControlHandle, JoinHandle<()>) {
    spawn_control_with_audit(client, registry, cancel, move |audit| {
        let org = org.clone();
        async move {
            if let Err(error) = org.execute(audit, chrono::Utc::now()).await {
                tracing::warn!(%error, "control audit failed; action result unchanged, not retrying");
            }
        }
    })
}

fn spawn_control_with_audit<F, Fut>(
    client: Box<dyn TmuxClient>,
    registry: RegistryHandle,
    cancel: CancellationToken,
    mut persist: F,
) -> (ControlHandle, JoinHandle<()>)
where
    F: FnMut(OrgCommand) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send,
{
    let (sender, mut requests) = mpsc::channel::<Pending>(CAPACITY);
    let task = tokio::spawn(async move {
        let (audits, mut receive) = mpsc::channel(AUDIT_CAPACITY);
        let mut forwarder = tokio::spawn(async move {
            while let Some(audit) = receive.recv().await {
                match audit {
                    Audit::Record(audit) => persist(audit).await,
                    #[cfg(test)]
                    Audit::Barrier(reply) => {
                        let _ = reply.send(Ok(()));
                    }
                }
            }
        });
        let mut dropped = 0_u64;
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled(), if !requests.is_closed() => requests.close(),
                pending = requests.recv() => {
                    let Some(pending) = pending else { break };
                    // Drain admitted work under its original absolute deadline;
                    // shutdown must not drop a paste before its separate Enter.
                    match execute(&*client, &registry, pending).await {
                        #[cfg(test)]
                        Some(Audit::Barrier(reply)) => { let _ = audits.send(Audit::Barrier(reply)).await; }
                        Some(audit) => {
                            if audits.try_send(audit).is_err() {
                                dropped = dropped.saturating_add(1);
                                tracing::warn!(dropped, "control audit queue full or closed; dropping audit");
                            }
                        }
                        None => {}
                    }
                }
            }
        }
        drop(audits);
        if tokio::time::timeout(AUDIT_DRAIN_TIMEOUT, &mut forwarder)
            .await
            .is_err()
        {
            tracing::warn!("control audit drain timed out; remaining audits dropped, not retried");
            forwarder.abort();
            let _ = forwarder.await;
        }
    });
    (
        ControlHandle {
            sender,
            caller: None,
        },
        task,
    )
}

async fn outcome<T>(
    deadline: Instant,
    work: impl std::future::Future<Output = Result<T, ServiceError>>,
) -> Result<T, ServiceError> {
    // timeout_at may poll an immediately-ready future at an expired deadline;
    // explicitly refuse queued work before its first external effect.
    if Instant::now() >= deadline {
        Err(ServiceError::Timeout)
    } else {
        timeout_at(deadline, work)
            .await
            .unwrap_or(Err(ServiceError::Timeout))
    }
}

fn respond<T>(
    reply: Reply<T>,
    result: Result<T, ServiceError>,
    audit: impl FnOnce(&Result<T, ServiceError>) -> Option<OrgCommand>,
) -> Option<OrgCommand> {
    let audit = audit(&result);
    let _ = reply.send(result);
    audit
}

fn control_audit<T>(
    method: &str,
    pane: &str,
    caller: Option<&CallContext>,
    result: &Result<T, ServiceError>,
) -> ControlAudit {
    ControlAudit {
        pane: pane.into(),
        action: serde_json::json!({
            "method": method,
            "outcome": if result.is_ok() { "ok" } else { "error" },
            "error": result.as_ref().err().map(ToString::to_string),
            "client_id": caller.map(|c| c.client_id.0.as_str()),
            "from_pane": caller.and_then(|c| c.from_pane.as_ref()).map(|p| p.0.as_str()),
        }),
    }
}

async fn execute(
    client: &dyn TmuxClient,
    registry: &RegistryHandle,
    pending: Pending,
) -> Option<Audit> {
    let deadline = pending.deadline;
    let expired = Instant::now() >= deadline;
    let caller = pending.caller.as_ref();
    let audit = match pending.request {
        #[cfg(test)]
        Request::FlushAudit(reply) => return Some(Audit::Barrier(reply)),
        Request::Send {
            target,
            text,
            reply,
        } => {
            let mut pane = target.starts_with('%').then(|| target.clone());
            let result = outcome(deadline, async {
                if !registry.is_connected() {
                    return Err(ServiceError::Unavailable);
                }
                let sessions = registry.get_all_sessions().await;
                if !registry.is_connected() {
                    return Err(ServiceError::Unavailable);
                }
                pane = super::resolve_pane_id(&sessions, &target).ok();
                Ok(super::send(client, &sessions, &target, &text).await?)
            })
            .await;
            respond(reply, result, |result| {
                pane.as_deref().map(|pane| {
                    OrgCommand::ControlSent(control_audit("session.send", pane, caller, result))
                })
            })
        }
        Request::Prepare {
            target,
            cwd,
            caller,
            fallback,
            reply,
        } => {
            let result = outcome(deadline, async {
                Ok(super::prepare_spawn(
                    client,
                    cwd.as_deref(),
                    target.as_deref(),
                    caller.as_deref(),
                    fallback.as_deref(),
                )
                .await?)
            })
            .await;
            respond(reply, result, |_| None)
        }
        Request::Split {
            context,
            direction,
            size,
            reply,
        } => {
            let result = outcome(deadline, async {
                Ok(super::split_spawn(client, &context, direction, &size).await?)
            })
            .await;
            respond(reply, result, |result| {
                let pane = result.as_deref().unwrap_or(&context.target_pane);
                Some(OrgCommand::ControlSpawned(control_audit(
                    "session.spawn",
                    pane,
                    caller,
                    result,
                )))
            })
        }
        Request::Type {
            pane,
            command,
            reply,
        } => {
            let result = outcome(deadline, async {
                Ok(super::type_spawn_command(client, &pane, &command).await?)
            })
            .await;
            respond(reply, result, |result| {
                Some(OrgCommand::ControlSent(control_audit(
                    "session.spawn_type",
                    &pane,
                    caller,
                    result,
                )))
            })
        }
    };
    audit.filter(|_| !expired).map(Audit::Record)
}
