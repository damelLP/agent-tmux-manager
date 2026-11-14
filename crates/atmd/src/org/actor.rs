//! One decision and its complete append/apply sequence at a time.

#[cfg(test)]
#[path = "actor_tests.rs"]
mod tests;

use std::future::Future;

use atm_org::{OrgCommand, OrgState};
use chrono::{DateTime, Utc};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use super::{Health, OrgServiceError, Request};
use crate::eventlog::{Envelope, EventLogError};

pub(super) async fn run<F, Fut>(
    mut state: OrgState,
    mut requests: mpsc::Receiver<Request>,
    health: watch::Sender<Health>,
    cancel: CancellationToken,
    mut append: F,
) where
    F: FnMut(Envelope) -> Fut,
    Fut: Future<Output = Result<u64, EventLogError>>,
{
    loop {
        let request = tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            request = requests.recv() => match request {
                Some(request) => request,
                None => break,
            },
        };
        match request {
            Request::Snapshot(reply) => {
                let _ = reply.send(state.clone());
            }
            Request::Execute {
                command,
                now,
                reply,
            } => {
                let result = execute(&mut state, &command, now, &health, &mut append).await;
                let failed = matches!(result, Err(OrgServiceError::Unavailable(_)));
                let _ = reply.send(result);
                if failed {
                    // Dropping the queue resolves pending callers without ever
                    // deciding another command against an uncertain state.
                    return;
                }
            }
        }
    }
    let _ = health.send_replace(Err("organisation actor stopped".into()));
}

async fn execute<F, Fut>(
    state: &mut OrgState,
    command: &OrgCommand,
    now: DateTime<Utc>,
    health: &watch::Sender<Health>,
    append: &mut F,
) -> Result<Vec<Envelope>, OrgServiceError>
where
    F: FnMut(Envelope) -> Fut,
    Fut: Future<Output = Result<u64, EventLogError>>,
{
    let events = state.decide(command, now)?;
    let mut committed = Vec::with_capacity(events.len());
    for event in events {
        let mut envelope = Envelope {
            seq: 0, // Assigned by the log, never predicted by a command caller.
            ts: now,
            kind: event.kind().into(),
            body: event.to_body(),
        };
        envelope.seq = match append(envelope.clone()).await {
            Ok(seq) => seq,
            Err(EventLogError::TimeoutBeforeHandover) => return Err(OrgServiceError::Timeout),
            Err(error) => {
                // Io wrote nothing, but U2 stops its writer on any error.
                // Unknown outcomes additionally invalidate the in-memory fold.
                let reason = error.to_string();
                let _ = health.send_replace(Err(reason.clone()));
                tracing::warn!(%error, "Organisation writer disabled until restart");
                return Err(OrgServiceError::Unavailable(reason));
            }
        };
        if let Err(error) = state.apply(envelope.seq, now, &event) {
            let reason = format!("apply at sequence {}: {error}", envelope.seq);
            let _ = health.send_replace(Err(reason.clone()));
            return Err(OrgServiceError::Unavailable(reason));
        }
        let _ = health.send_replace(Ok(envelope.seq));
        committed.push(envelope);
    }
    Ok(committed)
}
