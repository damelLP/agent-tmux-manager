//! Serialized, deadline-bounded session control. Dropping a waiting caller does
//! not cancel an admitted action or retry its possibly partial effects.

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

    #[tokio::test(start_paused = true)]
    async fn expired_queued_actions_never_touch_tmux_even_if_caller_is_gone() {
        let mock = MockTmuxClient::new();
        let (reply, response) = oneshot::channel();
        drop(response);
        execute(
            &mock,
            &crate::registry::spawn_registry(),
            Pending {
                deadline: Instant::now(),
                request: Request::Type {
                    pane: "%7".into(),
                    command: "late".into(),
                    reply,
                },
            },
        )
        .await;
        assert_eq!(mock.call_count(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn queue_admission_and_stalled_action_share_a_bounded_budget() {
        let (sender, _receiver) = mpsc::channel(CAPACITY);
        for _ in 0..CAPACITY {
            let (reply, _) = oneshot::channel();
            sender
                .try_send(Pending {
                    deadline: Instant::now() + ACTION_TIMEOUT,
                    request: Request::Type {
                        pane: "%7".into(),
                        command: "queued".into(),
                        reply,
                    },
                })
                .ok()
                .unwrap();
        }
        let handle = ControlHandle { sender };
        assert!(matches!(
            handle.send("%7".into(), "blocked".into()).await,
            Err(ServiceError::Timeout)
        ));
        let (reply, response) = oneshot::channel::<Result<(), ServiceError>>();
        respond(
            Instant::now() + ACTION_TIMEOUT,
            reply,
            std::future::pending(),
        )
        .await;
        assert!(matches!(
            response.await.unwrap(),
            Err(ServiceError::Timeout)
        ));
    }
}

use std::time::Duration;

use atm_tmux::{PaneDirection, TmuxClient};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{timeout_at, Instant};
use tokio_util::sync::CancellationToken;

use super::{ControlError, SpawnContext};
use crate::registry::RegistryHandle;

const CAPACITY: usize = 32;
const ACTION_TIMEOUT: Duration = Duration::from_secs(30);

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
    request: Request,
}

/// A bounded queue shared by RPC and in-process control clients.
#[derive(Debug, Clone)]
pub struct ControlHandle {
    sender: mpsc::Sender<Pending>,
}

impl ControlHandle {
    async fn request<T>(&self, make: impl FnOnce(Reply<T>) -> Request) -> Result<T, ServiceError> {
        let deadline = Instant::now() + ACTION_TIMEOUT;
        let (reply, response) = oneshot::channel();
        timeout_at(deadline, async {
            self.sender
                .send(Pending {
                    deadline,
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
    cancel: CancellationToken,
) -> (ControlHandle, JoinHandle<()>) {
    let (sender, mut requests) = mpsc::channel::<Pending>(CAPACITY);
    let task = tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                pending = requests.recv() => {
                    let Some(pending) = pending else { break };
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => break,
                        () = execute(&*client, &registry, pending) => {}
                    }
                }
            }
        }
    });
    (ControlHandle { sender }, task)
}

async fn respond<T>(
    deadline: Instant,
    reply: Reply<T>,
    work: impl std::future::Future<Output = Result<T, ServiceError>>,
) {
    // timeout_at may poll an immediately-ready future at an expired deadline;
    // explicitly refuse queued work before its first external effect.
    let result = if Instant::now() >= deadline {
        Err(ServiceError::Timeout)
    } else {
        timeout_at(deadline, work)
            .await
            .unwrap_or(Err(ServiceError::Timeout))
    };
    let _ = reply.send(result);
}

async fn execute(client: &dyn TmuxClient, registry: &RegistryHandle, pending: Pending) {
    let deadline = pending.deadline;
    match pending.request {
        Request::Send {
            target,
            text,
            reply,
        } => {
            respond(deadline, reply, async {
                if !registry.is_connected() {
                    return Err(ServiceError::Unavailable);
                }
                let sessions = registry.get_all_sessions().await;
                if !registry.is_connected() {
                    return Err(ServiceError::Unavailable);
                }
                Ok(super::send(client, &sessions, &target, &text).await?)
            })
            .await
        }
        Request::Prepare {
            target,
            cwd,
            caller,
            fallback,
            reply,
        } => {
            respond(deadline, reply, async {
                Ok(super::prepare_spawn(
                    client,
                    cwd.as_deref(),
                    target.as_deref(),
                    caller.as_deref(),
                    fallback.as_deref(),
                )
                .await?)
            })
            .await
        }
        Request::Split {
            context,
            direction,
            size,
            reply,
        } => {
            respond(deadline, reply, async {
                Ok(super::split_spawn(client, &context, direction, &size).await?)
            })
            .await
        }
        Request::Type {
            pane,
            command,
            reply,
        } => {
            respond(deadline, reply, async {
                Ok(super::type_spawn_command(client, &pane, &command).await?)
            })
            .await
        }
    }
}
