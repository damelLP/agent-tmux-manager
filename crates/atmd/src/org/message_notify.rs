//! Best-effort notifications from owner messages and pending decisions, never delivery work.
//! Startup captures a cursor before subscribing; watch replays only the gap and
//! recovers lag. At most 32 pending commands plus one running/delayed command;
//! overflow keeps the durable item. A crash may lose an external notice.

use super::{messages::is_focus_notice, EventWatch, OrgHandle, OrgServiceError};
use crate::{
    eventlog::Envelope,
    notify::{notify_argv, run_notify, NotifyConfig, NotifyError, NOTIFY_TIMEOUT},
    org::message_delivery::inbox_reference,
};
use atm_org::{channel_query, Address, DecisionId, DecisionStatus, MessageId, OrgEvent, OrgState};
use std::{fmt, future::Future, time::Duration};
use tokio::{sync::mpsc, task::JoinHandle, time::Instant};
use tokio_util::sync::CancellationToken;
use tracing::warn;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Subject {
    Message(MessageId),
    Decision(DecisionId),
}
impl fmt::Display for Subject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Message(id) => id.fmt(f),
            Self::Decision(id) => id.fmt(f),
        }
    }
}
fn still_open(state: &OrgState, subject: Subject) -> bool {
    match subject {
        // A wake of a post he has already been shown is not news (N6), and a
        // question's row is pinged as its decision (U8).
        Subject::Message(id) => state.message(&id).is_some_and(|m| {
            m.content.to == Address::Owner
                && m.read_at.is_none()
                && !m.content.post.is_some_and(|post| {
                    super::channel_post::owner_has_seen(state, post)
                        || channel_query::post(state, post).is_some_and(|p| {
                            p.content
                                .decision
                                .as_ref()
                                .is_some_and(|step| step.step == super::decision_steps::ASKED)
                        })
                })
        }),
        Subject::Decision(id) => state
            .decision(&id)
            .is_some_and(|d| d.status == DecisionStatus::Pending),
    }
}

#[derive(Debug)]
struct Pending {
    id: Subject,
    due: Instant,
    message: String,
}
#[derive(Debug, PartialEq, Eq)]
enum Admission {
    Ignored,
    Queued,
    Overflow,
}

fn enqueue(
    record: &Envelope,
    cursor: &mut u64,
    tx: &mpsc::Sender<Pending>,
    delay: Duration,
    now: Instant,
) -> Result<Admission, OrgServiceError> {
    if record.seq <= *cursor {
        return Ok(Admission::Ignored);
    }
    *cursor = record.seq;
    let event = OrgEvent::from_parts(&record.kind, &record.body)
        .map_err(|_| OrgServiceError::Unavailable("invalid committed owner notification".into()))?;
    let (id, message) = match event {
        Some(OrgEvent::MessageSent(content))
            if content.to == Address::Owner && !is_focus_notice(&content) =>
        {
            let id = MessageId::new(record.seq);
            (
                Subject::Message(id),
                format!(
                    "{id} from {}: {} --for owner",
                    content.from,
                    inbox_reference(id)
                ),
            )
        }
        Some(OrgEvent::DecisionRequested(request)) => {
            let id = DecisionId::new(record.seq);
            (
                Subject::Decision(id),
                format!("{id} from {}: atm decisions", request.requester),
            )
        }
        _ => return Ok(Admission::Ignored),
    };
    let due = now.checked_add(delay).ok_or_else(|| {
        OrgServiceError::Unavailable("notification delay exceeds clock range".into())
    })?;
    match tx.try_send(Pending { id, due, message }) {
        Ok(()) => Ok(Admission::Queued),
        Err(mpsc::error::TrySendError::Full(_)) => Ok(Admission::Overflow),
        Err(mpsc::error::TrySendError::Closed(_)) => Ok(Admission::Ignored),
    }
}

async fn consume(
    mut watch: EventWatch,
    mut cursor: u64,
    tx: mpsc::Sender<Pending>,
    delay: Duration,
    cancel: CancellationToken,
) {
    loop {
        let record = tokio::select! {
            biased;
            () = cancel.cancelled() => break,
            record = watch.next() => record,
        };
        match record {
            Ok(Some(record)) => match enqueue(&record, &mut cursor, &tx, delay, Instant::now()) {
                Ok(Admission::Overflow) => warn!(
                    seq = record.seq,
                    "Owner notification queue full; item remains in durable state"
                ),
                Err(error) => warn!(%error, seq = record.seq, "Owner notification skipped"),
                _ => {}
            },
            Ok(None) => break,
            Err(error) => {
                warn!(%error, "Owner notification watch stopped");
                break;
            }
        }
    }
}

// Inject only side effects; tests can advance time without filesystem/process I/O.
async fn run_queue<C, CF, R, RF>(
    mut pending: mpsc::Receiver<Pending>,
    cancel: CancellationToken,
    mut unread: C,
    mut run: R,
) where
    C: FnMut(Subject) -> CF,
    CF: Future<Output = Result<bool, OrgServiceError>>,
    R: FnMut(String) -> RF,
    RF: Future<Output = Result<(), NotifyError>>,
{
    loop {
        let next = tokio::select! {
            biased;
            () = cancel.cancelled() => break,
            next = pending.recv() => next,
        };
        let Some(next) = next else {
            break;
        };
        tokio::select! {
            biased;
            () = cancel.cancelled() => break,
            () = async {
                tokio::time::sleep_until(next.due).await;
                match unread(next.id).await {
                    Ok(true) => if let Err(error) = run(next.message).await { warn!(%error, id = %next.id, "Owner notify command failed"); },
                    Ok(false) => {},
                    Err(error) => warn!(%error, id = %next.id, "Owner notification state unavailable; skipping"),
                }
            } => {}
        }
    }
}

/// Subscribe synchronously before returning, so startup writes cannot fall into
/// a spawn race. Failures affect notifications only, never message admission.
pub fn spawn_message_notifier(
    org: OrgHandle,
    config: NotifyConfig,
    cancel: CancellationToken,
) -> Result<JoinHandle<()>, OrgServiceError> {
    let cursor = org.status()?;
    let watch = org.watch(vec!["msg.sent".into(), "decision.requested".into()], cursor)?;
    let (tx, rx) = mpsc::channel(32);
    Ok(tokio::spawn(async move {
        let reader = org.clone();
        let worker = run_queue(
            rx,
            cancel.clone(),
            move |id| {
                let org = reader.clone();
                async move { Ok(still_open(&org.snapshot().await?, id)) }
            },
            move |message| {
                let argv = notify_argv(&config.command, &message);
                async move { run_notify(&argv, NOTIFY_TIMEOUT).await }
            },
        );
        tokio::join!(
            consume(
                watch,
                cursor,
                tx,
                Duration::from_secs(config.delay_secs),
                cancel
            ),
            worker
        );
    }))
}

#[cfg(test)]
#[path = "message_notify_tests.rs"]
mod tests;
