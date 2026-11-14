//! Durable organisation state, independent of the live session registry.
//!
//! [`OrgHandle`] is the only organisation writer: callers submit [`OrgCommand`]s,
//! never append the events log themselves. For example, U7 submits
//! `OrgCommand::ControlSent`, `ControlSpawned`, or `ControlKilled` with its audit
//! body and timestamp. A command is decided, appended and applied serially.
//! Dropping a caller does not cancel accepted work; a timeout is not a rollback.
//!
//! Open only after daemonization, with an absolute state directory. Open/replay
//! run on a detached thread with a deadline; unavailable persistence never owns
//! the registry's cancellation token. Do not retry a timed-out open in-process:
//! its filesystem worker may still hold the stream lock. Restart to recover.

mod actor;
mod storage;

use std::{path::PathBuf, time::Duration};

use atm_org::{OrgCommand, OrgState, Refusal};
use chrono::{DateTime, Utc};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

use crate::eventlog::Envelope;

const DEADLINE: Duration = Duration::from_secs(5);

/// Persistence and policy failures are distinct. Unavailable includes its cause.
#[derive(Debug, thiserror::Error)]
pub enum OrgServiceError {
    #[error("organisation service unavailable: {0}")]
    Unavailable(String),
    #[error(transparent)]
    Refused(#[from] Refusal),
    #[error("organisation request timed out; accepted work may still complete")]
    Timeout,
}

type Health = Result<u64, String>;

enum Request {
    Execute {
        command: Box<OrgCommand>,
        now: DateTime<Utc>,
        reply: oneshot::Sender<Result<Vec<Envelope>, OrgServiceError>>,
    },
    Snapshot(oneshot::Sender<OrgState>),
}

/// Cloneable actor handle. The high-water mark includes skipped unknown events
/// during replay, and only acknowledged AND applied events during this run.
#[derive(Clone, Debug)]
pub struct OrgHandle {
    requests: mpsc::Sender<Request>,
    health: watch::Receiver<Health>,
}

impl OrgHandle {
    /// An explicitly disabled service, useful before startup or for legacy tests.
    pub fn disabled(reason: impl Into<String>) -> Self {
        let (requests, _) = mpsc::channel(1);
        let (_, health) = watch::channel(Err(reason.into()));
        Self { requests, health }
    }

    /// Opens/replays once; an error becomes an unavailable service, not a daemon
    /// startup error. The caller must not retry a hung filesystem operation.
    pub async fn open(state_dir: PathBuf, cancel: CancellationToken) -> Self {
        let ready = match storage::open(state_dir).await {
            Ok(ready) => ready,
            Err(error) => {
                tracing::warn!(%error, "Organisation persistence disabled");
                return Self::disabled(error);
            }
        };
        let (requests, receive) = mpsc::channel(64);
        let (health, status) = watch::channel(Ok(ready.seq));
        tokio::spawn(actor::run(
            ready.state,
            receive,
            health,
            cancel,
            move |envelope| {
                let log = ready.log.clone();
                async move { log.append(envelope.ts, &envelope.kind, envelope.body).await }
            },
        ));
        Self {
            requests,
            health: status,
        }
    }

    /// Latest committed cursor, or the reason this service cannot be used.
    pub fn status(&self) -> Result<u64, OrgServiceError> {
        self.health
            .borrow()
            .clone()
            .map_err(OrgServiceError::Unavailable)
    }

    fn submit(&self, request: Request) -> Result<(), OrgServiceError> {
        self.status()?;
        self.requests
            .try_send(request)
            .map_err(|error| OrgServiceError::Unavailable(format!("organisation queue: {error}")))
    }

    /// Runs decide → append → apply on the sole writer. No-op decisions return
    /// an empty list. Multi-event commands can commit a prefix before failing;
    /// never blindly retry on timeout or an unavailable writer.
    pub async fn execute(
        &self,
        command: OrgCommand,
        now: DateTime<Utc>,
    ) -> Result<Vec<Envelope>, OrgServiceError> {
        let (reply, response) = oneshot::channel();
        self.submit(Request::Execute {
            command: Box::new(command),
            now,
            reply,
        })?;
        // The actor retains the command if this waiting future is dropped.
        tokio::time::timeout(DEADLINE + Duration::from_secs(1), response)
            .await
            .map_err(|_| OrgServiceError::Timeout)?
            .map_err(|_| self.closed_error())?
    }

    /// A read-only copy, not a safe place to decide later mutations. Submit the
    /// command itself to execute so its decision uses the actor's current state.
    pub async fn snapshot(&self) -> Result<OrgState, OrgServiceError> {
        let (reply, response) = oneshot::channel();
        self.submit(Request::Snapshot(reply))?;
        tokio::time::timeout(DEADLINE, response)
            .await
            .map_err(|_| OrgServiceError::Timeout)?
            .map_err(|_| self.closed_error())
    }

    fn closed_error(&self) -> OrgServiceError {
        self.status()
            .err()
            .unwrap_or_else(|| OrgServiceError::Unavailable("organisation actor stopped".into()))
    }
}
