//! Ordered, single-writer JSONL streams.
//!
//! # Disk format
//! `<state>/log/events/NNNNNNNN.jsonl` and `<state>/log/raw/NNNNNNNN.jsonl`
//! contain UTF-8 JSON envelopes, one per newline-terminated record, for example:
//! `{"seq":1,"ts":"2023-11-14T22:13:20Z","kind":"started","body":{}}`.
//! Segment names are their first sequence number, padded to at least eight digits
//! and sorted numerically. Sequence numbers start at 1 independently per stream.
//! An empty newest segment reserves its filename's sequence for the next append.
//! The persistent `.lock` file is advisory-locked exclusively and never unlinked.
//!
//! Open lists segments and validates only the newest. Its unterminated final
//! record is truncated and reported with a tracing warning; malformed complete
//! lines are errors. Replay validates the older segments it reads, including
//! increasing sequence order across files; it never repairs files.
//!
//! Events are synced before acknowledgement and retained forever. Raw records
//! are not individually synced; their segment is synced when closed. Subscribers
//! receive committed envelopes in sequence order (not historical records).
//! A bounded live channel can return `Lagged`; use replay to recover a gap.
//!
//! Open and replay perform blocking work with no internal timeout. Async callers
//! must run them on detached threads, never `spawn_blocking` (a hung filesystem
//! would otherwise prevent runtime shutdown). They remain safe to call on any
//! thread, including a Tokio worker, but can block that caller.

mod lock;
mod reader;
mod state;
mod writer;

pub use reader::read_from;
pub use state::state_dir;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, oneshot, OwnedSemaphorePermit, Semaphore};

/// A persisted event. Sequence numbers are scoped to one stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    pub seq: u64,
    pub ts: DateTime<Utc>,
    pub kind: String,
    pub body: serde_json::Value,
}

/// Independent streams with different durability and retention policies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Events,
    Raw,
}

impl Stream {
    fn directory(self, state: &Path) -> PathBuf {
        state.join("log").join(match self {
            Self::Events => "events",
            Self::Raw => "raw",
        })
    }

    const fn sync_each_record(self) -> bool {
        matches!(self, Self::Events)
    }
}

/// Persistence errors are explicit, including whether a timeout may have written.
#[derive(Debug, thiserror::Error)]
pub enum EventLogError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path}: line {line}: {reason}")]
    Corrupt {
        path: PathBuf,
        line: u64,
        reason: String,
    },
    #[error("stream already locked: {0}")]
    Locked(PathBuf),
    #[error("event log writer is closed")]
    Closed,
    #[error("append timed out before hand-over; nothing was written")]
    TimeoutBeforeHandover,
    #[error("append timed out after hand-over; outcome unknown")]
    TimeoutAfterHandover,
    #[error("event log sequence numbers exhausted")]
    SequenceExhausted,
}

fn io_error(path: &Path, source: std::io::Error) -> EventLogError {
    EventLogError::Io {
        path: path.to_owned(),
        source,
    }
}

fn corrupt(path: &Path, line: u64, reason: impl ToString) -> EventLogError {
    EventLogError::Corrupt {
        path: path.to_owned(),
        line,
        reason: reason.to_string(),
    }
}

struct Append {
    ts: DateTime<Utc>,
    kind: String,
    body: serde_json::Value,
    reply: oneshot::Sender<Result<u64, EventLogError>>,
    // The worker, not the waiting caller, owns the in-flight slot.
    _permit: OwnedSemaphorePermit,
}

/// Cheaply cloned handle to one detached writer thread. Drop never joins it.
#[derive(Debug, Clone)]
pub struct EventLog {
    requests: mpsc::Sender<Append>,
    slot: Arc<Semaphore>,
    events: broadcast::Sender<Arc<Envelope>>,
}

impl EventLog {
    /// Opens a stream exclusively. Blocking, without an internal timeout.
    /// Async callers must use a detached thread, never `spawn_blocking`.
    pub fn open(state_dir: &Path, stream: Stream) -> Result<Self, EventLogError> {
        let directory = stream.directory(state_dir);
        let (requests, mut receiver) = mpsc::channel::<Append>(1);
        let (events, _) = broadcast::channel(1024);
        let publish = events.clone();
        let (started, ready) = std::sync::mpsc::sync_channel(1);
        let thread_path = directory.clone();
        let dispatch = tracing::dispatcher::get_default(Clone::clone);
        std::thread::Builder::new()
            .name(format!("atmd-log-{stream:?}"))
            .spawn(move || {
                tracing::dispatcher::with_default(&dispatch, || {
                    let mut writer = match writer::Writer::open(&thread_path, stream) {
                        Ok(writer) => writer,
                        Err(error) => {
                            let _ = started.send(Err(error));
                            return;
                        }
                    };
                    if started.send(Ok(())).is_err() {
                        return;
                    }
                    while let Some(request) = receiver.blocking_recv() {
                        let result = writer.append(request.ts, request.kind, request.body);
                        let failed = result.is_err();
                        let result = result.map(|envelope| {
                            let seq = envelope.seq;
                            let _ = publish.send(Arc::new(envelope));
                            seq
                        });
                        let _ = request.reply.send(result);
                        if failed {
                            break;
                        }
                    }
                    if let Err(error) = writer.close() {
                        tracing::warn!(%error, "Cannot sync closed event log");
                    }
                })
            })
            .map_err(|e| io_error(&directory, e))?;
        // std receive is safe even if open was mistakenly called inside Tokio.
        ready.recv().map_err(|_| EventLogError::Closed)??;
        Ok(Self {
            requests,
            slot: Arc::new(Semaphore::new(1)),
            events,
        })
    }

    /// Persists an envelope, bounded by five seconds including admission.
    /// After-hand-over timeout or cancellation can still commit; do not auto-retry.
    pub async fn append(
        &self,
        ts: DateTime<Utc>,
        kind: &str,
        body: serde_json::Value,
    ) -> Result<u64, EventLogError> {
        self.append_with_timeout(ts, kind, body, Duration::from_secs(5))
            .await
    }

    async fn append_with_timeout(
        &self,
        ts: DateTime<Utc>,
        kind: &str,
        body: serde_json::Value,
        timeout: Duration,
    ) -> Result<u64, EventLogError> {
        let deadline = tokio::time::Instant::now() + timeout;
        let permit = tokio::time::timeout_at(deadline, Arc::clone(&self.slot).acquire_owned())
            .await
            .map_err(|_| EventLogError::TimeoutBeforeHandover)?
            .map_err(|_| EventLogError::Closed)?;
        if tokio::time::Instant::now() >= deadline {
            return Err(EventLogError::TimeoutBeforeHandover);
        }
        let (reply, response) = oneshot::channel();
        self.requests
            .try_send(Append {
                ts,
                kind: kind.to_owned(),
                body,
                reply,
                _permit: permit,
            })
            .map_err(|_| EventLogError::Closed)?;
        tokio::time::timeout_at(deadline, response)
            .await
            .map_err(|_| EventLogError::TimeoutAfterHandover)?
            .map_err(|_| EventLogError::Closed)?
    }

    /// Subscribes to future commits. A slow receiver gets `RecvError::Lagged`.
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<Envelope>> {
        self.events.subscribe()
    }
}

#[cfg(test)]
mod tests;
