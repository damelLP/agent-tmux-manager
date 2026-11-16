//! One decision and its complete append/apply sequence at a time.

#[cfg(test)]
#[path = "actor_tests.rs"]
mod tests;

use std::{collections::VecDeque, future::Future, sync::Arc};

use atm_org::{OrgCommand, OrgState};
use chrono::{DateTime, Utc};
use tokio::sync::{broadcast, mpsc, watch};
use tokio_util::sync::CancellationToken;

use super::{Health, OrgServiceError, Request, TaskCommit};
use crate::eventlog::{Envelope, EventLogError};

pub(super) async fn run<F, Fut>(
    mut state: OrgState,
    mut requests: mpsc::Receiver<Request>,
    health: watch::Sender<Health>,
    publication: broadcast::Sender<Arc<Envelope>>,
    cancel: CancellationToken,
    mut append: F,
) where
    F: FnMut(Envelope) -> Fut,
    Fut: Future<Output = Result<u64, EventLogError>>,
{
    // Recover a verdict/cost prefix before serving any snapshot or mutation.
    for (command, at) in super::cost::missing(&state) {
        if let Err(error) = execute(
            &mut state,
            &command,
            at,
            None,
            &health,
            &publication,
            &mut append,
        )
        .await
        {
            tracing::warn!(%error, "Accepted task cost repair incomplete");
            if matches!(error, OrgServiceError::Unavailable(_)) {
                return;
            }
        }
    }
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
                caller_pane,
                reply,
            } => {
                let before = match command.as_ref() {
                    OrgCommand::Task(command) => {
                        super::tasks::target(command).and_then(|id| state.task(&id).cloned())
                    }
                    _ => None,
                };
                let result = execute(
                    &mut state,
                    &command,
                    now,
                    caller_pane.as_deref(),
                    &health,
                    &publication,
                    &mut append,
                )
                .await;
                let failed = matches!(result, Err(OrgServiceError::Unavailable(_)));
                let _ = reply.send(result.map(|events| TaskCommit { events, before }));
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
    caller_pane: Option<&str>,
    health: &watch::Sender<Health>,
    publication: &broadcast::Sender<Arc<Envelope>>,
    append: &mut F,
) -> Result<Vec<Envelope>, OrgServiceError>
where
    F: FnMut(Envelope) -> Fut,
    Fut: Future<Output = Result<u64, EventLogError>>,
{
    // Bind against current actor state, not the observer's startup snapshot.
    let bound;
    let command = if let OrgCommand::RecordSummary(summary) = command {
        let mut summary = summary.clone();
        summary.agent_id = summary
            .pane
            .as_deref()
            .and_then(|pane| state.agent_for_pane(pane))
            .map(|agent| agent.id.clone())
            .or(summary.agent_id);
        bound = OrgCommand::RecordSummary(summary);
        &bound
    } else {
        command
    };
    let mut events: VecDeque<_> = state.decide(command, now)?.into();
    let mut committed = Vec::with_capacity(events.len());
    while let Some(mut event) = events.pop_front() {
        if let atm_org::OrgEvent::DecisionResolved(answer)
        | atm_org::OrgEvent::DecisionOverruled(answer) = &mut event
        {
            answer.caller_pane = caller_pane.map(str::to_owned);
        }
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
        if let Some(record) = super::cost::after_event(state, &event) {
            events.push_front(record);
        }
        // One owner advances the replay fence and publishes only applied records.
        // Subscribing before reading that fence makes replay/live overlap gapless.
        let _ = health.send_replace(Ok(envelope.seq));
        let _ = publication.send(Arc::new(envelope.clone()));
        committed.push(envelope);
    }
    Ok(committed)
}
