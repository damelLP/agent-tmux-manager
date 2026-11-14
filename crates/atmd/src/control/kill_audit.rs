//! Shared acknowledgement: dropping an individual kill future never drops the
//! actor-owned receiver. Work/cleanup await it outside the tmux actor.
use crate::{control::service::AuditPermit, eventlog::Envelope, org::OrgServiceError};
use std::sync::Arc;
use tokio::sync::{oneshot, Mutex};

type Acknowledgement = oneshot::Receiver<Result<Vec<Envelope>, OrgServiceError>>;
#[derive(Debug)]
pub(in crate::control) struct KillAudit(Mutex<Option<Acknowledgement>>);

impl KillAudit {
    pub(super) fn submit(
        permit: AuditPermit,
        command: atm_org::OrgCommand,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Arc<Self> {
        let (ack, receive) = oneshot::channel();
        permit.submit(command, now, ack);
        Arc::new(Self(Mutex::new(Some(receive))))
    }

    /// Never block the tmux actor. Jobs retain at most two pending kills.
    pub(super) fn pending(&self) -> bool {
        self.0.try_lock().map_or(true, |pending| pending.is_some())
    }

    pub(super) async fn wait(&self) {
        let mut pending = self.0.lock().await;
        if let Some(receive) = pending.as_mut() {
            // Await by reference: cancelling this future leaves the receiver in
            // the shared slot for the supervisor's bounded cleanup drain.
            match receive.await {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => {
                    tracing::warn!(%error, "kill audit failed; successful kill is not retried")
                }
                Err(error) => {
                    tracing::warn!(%error, "kill audit acknowledgement lost; successful kill is not retried")
                }
            }
            *pending = None;
        }
    }
}
