//! Best-effort notifications from committed owner messages, never delivery work.
//! Startup captures a cursor before subscribing; watch replays only the gap and
//! recovers lag. At most 32 pending commands plus one running/delayed command;
//! overflow keeps the durable inbox entry. A crash may lose an external notice.

use super::{messages::is_focus_notice, EventWatch, OrgHandle, OrgServiceError};
use crate::{
    eventlog::Envelope,
    notify::{notify_argv, run_notify, NotifyConfig, NotifyError, NOTIFY_TIMEOUT},
    org::message_delivery::inbox_reference,
};
use atm_org::{Address, MessageId, OrgEvent};
use std::{future::Future, time::Duration};
use tokio::{sync::mpsc, task::JoinHandle, time::Instant};
use tokio_util::sync::CancellationToken;
use tracing::warn;

#[derive(Debug)]
struct Pending {
    id: MessageId,
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
    let Some(OrgEvent::MessageSent(content)) = OrgEvent::from_parts(&record.kind, &record.body)
        .map_err(|_| {
            OrgServiceError::Unavailable("invalid committed message notification".into())
        })?
    else {
        return Ok(Admission::Ignored);
    };
    if content.to != Address::Owner || is_focus_notice(&content) {
        return Ok(Admission::Ignored);
    }
    let id = MessageId::new(record.seq);
    let due = now.checked_add(delay).ok_or_else(|| {
        OrgServiceError::Unavailable("notification delay exceeds clock range".into())
    })?;
    let message = format!("{id} from {}: {}", content.from, inbox_reference(id));
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
                    "Owner notification queue full; message remains in inbox"
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
    C: FnMut(MessageId) -> CF,
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
    let watch = org.watch(vec!["msg.sent".into()], cursor)?;
    let (tx, rx) = mpsc::channel(32);
    Ok(tokio::spawn(async move {
        let reader = org.clone();
        let worker = run_queue(
            rx,
            cancel.clone(),
            move |id| {
                let org = reader.clone();
                async move {
                    Ok(org
                        .snapshot()
                        .await?
                        .message(&id)
                        .is_some_and(|m| m.content.to == Address::Owner && m.read_at.is_none()))
                }
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
