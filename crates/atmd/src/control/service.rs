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
                server: None,
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
                server: None,
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
                    move |audit, now| {
                        let gate = gate.clone();
                        let entered = entered.clone();
                        let org = org.clone();
                        async move {
                            entered.notify_one();
                            let _permit = gate.acquire().await.unwrap();
                            org.execute(audit, now).await
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

    #[tokio::test]
    async fn audit_acknowledgement_follows_persistence_and_keeps_supplied_time() {
        tokio::time::timeout(Duration::from_secs(30), async {
            let dir = tempfile::tempdir().unwrap();
            let org_cancel = CancellationToken::new();
            let org = OrgHandle::open(dir.path().into(), org_cancel.clone()).await;
            let (control, task) = spawn_control(
                Box::new(MockTmuxClient::new()),
                crate::registry::spawn_registry(),
                org.clone(),
                CancellationToken::new(),
            );
            let now: chrono::DateTime<chrono::Utc> = "2001-02-03T04:05:06Z".parse().unwrap();
            let (ack, done) = oneshot::channel();
            control.audit_queue().submit(
                OrgCommand::ControlKilled(ControlAudit {
                    pane: "%7".into(),
                    action: serde_json::json!({}),
                }),
                now,
                Some(ack),
            );
            let recorded = done.await.unwrap().unwrap();
            let events = org.tail(crate::org::TailParams::default()).await.unwrap();
            assert_eq!(events.len(), 1);
            assert_eq!(recorded[0].seq, events[0].seq);
            assert_eq!(events[0].ts, now);
            drop(control);
            task.await.unwrap();
            org_cancel.cancel();
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn audit_acknowledgement_waits_behind_a_held_persistence_barrier() {
        tokio::time::timeout(Duration::from_secs(30), async {
            let dir = tempfile::tempdir().unwrap();
            let org_cancel = CancellationToken::new();
            let org = OrgHandle::open(dir.path().into(), org_cancel.clone()).await;
            let gate = Arc::new(tokio::sync::Semaphore::new(0));
            let entered = Arc::new(tokio::sync::Notify::new());
            let (control, task) = spawn_control_with_audit(
                Box::new(MockTmuxClient::new()),
                crate::registry::spawn_registry(),
                CancellationToken::new(),
                {
                    let gate = gate.clone();
                    let entered = entered.clone();
                    let org = org.clone();
                    move |audit, now| {
                        let gate = gate.clone();
                        let entered = entered.clone();
                        let org = org.clone();
                        async move {
                            entered.notify_one();
                            let _held = gate.acquire().await.unwrap();
                            org.execute(audit, now).await
                        }
                    }
                },
            );
            let now: chrono::DateTime<chrono::Utc> = "2001-02-03T04:05:06Z".parse().unwrap();
            let (ack, mut done) = oneshot::channel();
            control.audit_queue().submit(
                OrgCommand::ControlKilled(ControlAudit {
                    pane: "%2".into(),
                    action: serde_json::json!({}),
                }),
                now,
                Some(ack),
            );
            entered.notified().await;
            assert!(matches!(
                done.try_recv(),
                Err(oneshot::error::TryRecvError::Empty)
            ));
            assert_eq!(org.status().unwrap(), 0);
            gate.add_permits(1);
            let recorded = done.await.unwrap().unwrap();
            let events = org.tail(crate::org::TailParams::default()).await.unwrap();
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].ts, now);
            assert_eq!(recorded[0].seq, events[0].seq);
            drop(control);
            task.await.unwrap();
            org_cancel.cancel();
        })
        .await
        .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn queue_admission_and_stalled_action_share_a_bounded_budget() {
        tokio::time::timeout(Duration::from_secs(90), async {
            let (sender, _receiver) = mpsc::channel(CAPACITY);
            for _ in 0..CAPACITY {
                let (reply, _) = oneshot::channel();
                sender
                    .try_send(Pending {
                        server: None,
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
                server: None,
                sender,
                audits: AuditQueue {
                    sender: mpsc::channel(1).0,
                    dropped: Arc::new(AtomicU64::new(0)),
                },
                caller: None,
            };
            let start = Instant::now();
            assert!(matches!(
                handle.send("%7".into(), "blocked".into()).await,
                Err(ServiceError::Timeout)
            ));
            assert_eq!(start.elapsed(), Duration::from_secs(30));
            let start = Instant::now();
            assert!(matches!(
                outcome::<()>(Instant::now() + ACTION_TIMEOUT, std::future::pending()).await,
                Err(ServiceError::Timeout)
            ));
            assert_eq!(start.elapsed(), Duration::from_secs(30));
        })
        .await
        .expect("admission and execution must have bounded budgets");
    }
}

use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

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

pub type AuditAcknowledgement =
    oneshot::Sender<Result<Vec<crate::eventlog::Envelope>, crate::org::OrgServiceError>>;

#[derive(Debug)]
enum Audit {
    Record {
        command: Box<OrgCommand>,
        now: chrono::DateTime<chrono::Utc>,
        acknowledgement: Option<AuditAcknowledgement>,
    },
    Finish,
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
    #[error("{0}")]
    ServerRefused(String),
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
    server: Option<String>,
    deadline: Instant,
    caller: Option<CallContext>,
    request: Request,
}

/// One ordered, non-blocking audit path, also usable by in-process clients.
#[derive(Debug, Clone)]
pub struct AuditQueue {
    sender: mpsc::Sender<Audit>,
    dropped: Arc<AtomicU64>,
}

impl AuditQueue {
    /// Capture time at the effect, not when persistence eventually catches up.
    /// Optional acknowledgement is awaited by callers outside the control actor.
    pub fn submit(
        &self,
        command: OrgCommand,
        now: chrono::DateTime<chrono::Utc>,
        acknowledgement: Option<AuditAcknowledgement>,
    ) {
        self.enqueue(Audit::Record {
            command: Box::new(command),
            now,
            acknowledgement,
        });
    }

    fn enqueue(&self, audit: Audit) {
        if let Err(error) = self.sender.try_send(audit) {
            let dropped = self
                .dropped
                .fetch_add(1, Ordering::Relaxed)
                .saturating_add(1);
            tracing::warn!(
                dropped,
                "control audit queue full or closed; dropping audit"
            );
            if let Audit::Record {
                acknowledgement: Some(reply),
                ..
            } = error.into_inner()
            {
                let _ = reply.send(Err(crate::org::OrgServiceError::Unavailable(
                    "control audit queue full or closed".into(),
                )));
            }
        }
    }
}

/// A bounded queue shared by RPC and in-process control clients.
#[derive(Debug, Clone)]
pub struct ControlHandle {
    server: Option<String>,
    sender: mpsc::Sender<Pending>,
    audits: AuditQueue,
    caller: Option<CallContext>,
}

impl ControlHandle {
    /// Optional server identity supplied by a caller; absent retains legacy behavior.
    #[must_use]
    pub fn for_server(mut self, server: Option<String>) -> Self {
        self.server = server;
        self
    }

    #[must_use]
    pub fn audit_queue(&self) -> AuditQueue {
        self.audits.clone()
    }

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
                    server: self.server.clone(),
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
    spawn_control_with_audit(client, registry, cancel, move |audit, now| {
        let org = org.clone();
        async move { org.execute(audit, now).await }
    })
}

fn spawn_control_with_audit<F, Fut>(
    client: Box<dyn TmuxClient>,
    registry: RegistryHandle,
    cancel: CancellationToken,
    mut persist: F,
) -> (ControlHandle, JoinHandle<()>)
where
    F: FnMut(OrgCommand, chrono::DateTime<chrono::Utc>) -> Fut + Send + 'static,
    Fut: std::future::Future<
            Output = Result<Vec<crate::eventlog::Envelope>, crate::org::OrgServiceError>,
        > + Send,
{
    let (sender, mut requests) = mpsc::channel::<Pending>(CAPACITY);
    let (audits, mut receive) = mpsc::channel(AUDIT_CAPACITY);
    let audits = AuditQueue {
        sender: audits,
        dropped: Arc::new(AtomicU64::new(0)),
    };
    let handle_audits = audits.clone();
    let task = tokio::spawn(async move {
        let mut forwarder = tokio::spawn(async move {
            while let Some(audit) = receive.recv().await {
                match audit {
                    Audit::Record {
                        command,
                        now,
                        acknowledgement,
                    } => {
                        let result = persist(*command, now).await;
                        if let Err(error) = &result {
                            tracing::warn!(%error, "control audit failed; action result unchanged, not retrying");
                        }
                        if let Some(reply) = acknowledgement {
                            let _ = reply.send(result);
                        }
                    }
                    Audit::Finish => receive.close(),
                    #[cfg(test)]
                    Audit::Barrier(reply) => {
                        let _ = reply.send(Ok(()));
                    }
                }
            }
        });
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
                        Some(Audit::Barrier(reply)) => { let _ = audits.sender.send(Audit::Barrier(reply)).await; }
                        Some(audit) => audits.enqueue(audit),
                        None => {}
                    }
                }
            }
        }
        if tokio::time::timeout(AUDIT_DRAIN_TIMEOUT, async {
            let _ = audits.sender.send(Audit::Finish).await;
            let _ = (&mut forwarder).await;
        })
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
            server: None,
            sender,
            audits: handle_audits,
            caller: None,
        },
        task,
    )
}

/// Organisation-aware constructor for reserved lifecycle/delivery clients.
///
/// Same startup/shutdown ownership as [`spawn_control`]. The supplied handle is
/// the sole writer for post-kill audits and source for durable clearing/exclusion.
/// Inject it from MethodServices at the daemon's I/O boundary; tests may use the
/// real control actor over MockTmuxClient, with a private or disabled OrgHandle.
///
/// **Contract checkpoint:** organisation-aware behavior is not wired yet. This
/// starts the unchanged legacy actor; the org argument is not used. Every new
/// reserved operation returns a typed NotImplemented without admitting work.
/// Production startup continues using spawn_control until implementation lands.
pub fn spawn_control_with_org(
    client: Box<dyn TmuxClient>,
    registry: RegistryHandle,
    _org: crate::org::OrgHandle,
    cancel: CancellationToken,
) -> (ControlHandle, JoinHandle<()>) {
    spawn_control(client, registry, _org, cancel)
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

async fn verify_server(
    client: &dyn TmuxClient,
    caller: String,
    operation: String,
) -> Result<(), ServiceError> {
    let own = client
        .raw_command("display-message", &["-p", "#{socket_path}"])
        .await
        .map_err(|error| {
            ServiceError::ServerRefused(format!(
                "Refusing to {operation}: cannot identify atmd's tmux server: {error}"
            ))
        })?;
    // Canonicalization can hang on a filesystem. One detached worker retains its
    // permit even if the action expires, so timed-out requests cannot pile up threads.
    static SLOT: std::sync::LazyLock<Arc<tokio::sync::Semaphore>> =
        std::sync::LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(1)));
    let permit = SLOT
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| ServiceError::Unavailable)?;
    let (send, receive) = oneshot::channel();
    std::thread::Builder::new()
        .name("control-server-identity".into())
        .spawn(move || {
            let _permit = permit;
            let result = std::fs::canonicalize(own.trim())
                .and_then(|own| std::fs::canonicalize(caller).map(|caller| (own, caller)));
            let _ = send.send(result);
        })
        .map_err(|_| ServiceError::Unavailable)?;
    let (own, caller) = receive
        .await
        .map_err(|_| ServiceError::Unavailable)?
        .map_err(|error| {
            ServiceError::ServerRefused(format!(
                "Refusing to {operation}: cannot verify tmux server identity: {error}"
            ))
        })?;
    if own != caller {
        return Err(ServiceError::ServerRefused(format!(
            "Refusing to {operation}: atmd controls tmux server {}, not this one ({})",
            own.display(),
            caller.display()
        )));
    }
    Ok(())
}

async fn execute(
    client: &dyn TmuxClient,
    registry: &RegistryHandle,
    pending: Pending,
) -> Option<Audit> {
    let deadline = pending.deadline;
    let expired = Instant::now() >= deadline;
    let caller = pending.caller.as_ref();
    if let Some(server) = pending.server {
        let operation = match &pending.request {
            Request::Send { target, .. } => format!("send to {target}"),
            Request::Prepare { target, caller, .. } => format!(
                "spawn from {}",
                target
                    .as_deref()
                    .or(caller.as_deref())
                    .unwrap_or("the active pane")
            ),
            Request::Split { context, .. } => format!("spawn from {}", context.target_pane),
            Request::Type { pane, .. } => format!("spawn in {pane}"),
            #[cfg(test)]
            Request::FlushAudit(_) => "flush audits".into(),
        };
        if let Err(error) = outcome(deadline, verify_server(client, server, operation)).await {
            match pending.request {
                Request::Send { reply, .. } | Request::Split { reply, .. } => {
                    let _ = reply.send(Err(error));
                }
                Request::Prepare { reply, .. } => {
                    let _ = reply.send(Err(error));
                }
                Request::Type { reply, .. } => {
                    let _ = reply.send(Err(error));
                }
                #[cfg(test)]
                Request::FlushAudit(reply) => {
                    let _ = reply.send(Err(error));
                }
            }
            return None;
        }
    }
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
    audit.filter(|_| !expired).map(|command| Audit::Record {
        command: Box::new(command),
        now: chrono::Utc::now(),
        acknowledgement: None,
    })
}
