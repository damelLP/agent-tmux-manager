//! Bounded pre-translation capture, never a disk wait on the hook path.
//! Captured before translation means enqueued in order, NOT durable beforehand.
//! Overflow drops incoming frames; a writer failure disables capture until restart.

use super::{storage, OrgServiceError, DEADLINE};
use crate::eventlog::{Envelope, EventLog, EventLogError, Stream};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::{
    future::Future,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};
use tokio::sync::{mpsc, oneshot};

enum Request {
    Capture(Envelope),
    Flush(oneshot::Sender<()>),
}

/// Cloneable, nonblocking raw tap. At most 64 frames plus one append in flight.
#[derive(Clone, Debug, Default)]
pub struct RawHandle {
    queue: Option<mpsc::Sender<Request>>,
    dropped: Arc<AtomicU64>,
}

impl RawHandle {
    pub async fn open(state_dir: PathBuf) -> Self {
        match storage::detached_open(DEADLINE, move || {
            EventLog::open(&state_dir, Stream::Raw).map_err(|e| e.to_string())
        })
        .await
        {
            Ok(log) => Self::with_append(move |envelope| {
                let log = log.clone();
                async move { log.append(envelope.ts, &envelope.kind, envelope.body).await }
            }),
            Err(error) => {
                tracing::warn!(%error, "Raw logging disabled");
                Self::default()
            }
        }
    }

    fn with_append<F, Fut>(mut append: F) -> Self
    where
        F: FnMut(Envelope) -> Fut + Send + 'static,
        Fut: Future<Output = Result<u64, EventLogError>> + Send + 'static,
    {
        let (queue, mut receive) = mpsc::channel(64);
        tokio::spawn(async move {
            while let Some(request) = receive.recv().await {
                match request {
                    Request::Capture(envelope) => {
                        if let Err(error) = append(envelope).await {
                            tracing::warn!(%error, "Raw writer disabled until restart");
                            break;
                        }
                    }
                    Request::Flush(reply) => {
                        let _ = reply.send(());
                    }
                }
            }
        });
        Self {
            queue: Some(queue),
            dropped: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn capture(&self, ts: DateTime<Utc>, kind: &'static str, body: Value) {
        let Some(queue) = &self.queue else { return };
        if let Err(mpsc::error::TrySendError::Full(_)) =
            queue.try_send(Request::Capture(Envelope {
                seq: 0,
                ts,
                kind: kind.into(),
                body,
            }))
        {
            let count = self.dropped.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
            if count.is_power_of_two() {
                tracing::warn!(dropped = count, "Raw queue full; dropping incoming frames");
            }
        }
    }

    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Waits for earlier queued appends (raw itself is not fsynced per record).
    /// A deadline bounds shutdown even if the writer's filesystem is stuck.
    pub async fn flush(&self) -> Result<(), OrgServiceError> {
        let unavailable = || OrgServiceError::Unavailable("raw logging disabled".into());
        let queue = self.queue.as_ref().ok_or_else(unavailable)?;
        let (send, receive) = oneshot::channel();
        // Runs alongside summary flush after connection/control/audit drains;
        // both flushes share the final 2 s of the nominal 64 s shutdown budget.
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            queue
                .send(Request::Flush(send))
                .await
                .map_err(|_| unavailable())?;
            receive.await.map_err(|_| unavailable())
        })
        .await
        .map_err(|_| OrgServiceError::Timeout)?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eventlog::{read_from, Stream};
    use serde_json::json;

    #[tokio::test]
    async fn raw_captures_in_order_and_flush_is_a_barrier() {
        let dir = tempfile::tempdir().unwrap();
        let raw = RawHandle::open(dir.path().to_owned()).await;
        for n in 0..10 {
            raw.capture(chrono::Utc::now(), "hook_event", json!({"n":n}));
        }
        raw.flush().await.unwrap();
        let records: Vec<_> = read_from(dir.path(), Stream::Raw, 0)
            .map(Result::unwrap)
            .collect();
        assert_eq!(records.len(), 10);
        for (n, record) in records.iter().enumerate() {
            assert_eq!(record.body, json!({"n":n}));
        }
    }

    #[tokio::test]
    async fn hung_append_has_one_inflight_and_capture_drops_without_waiting() {
        let (started, mut receive) = tokio::sync::mpsc::unbounded_channel();
        let raw = RawHandle::with_append(move |_| {
            started.send(()).unwrap();
            std::future::pending::<Result<u64, crate::eventlog::EventLogError>>()
        });
        raw.capture(chrono::Utc::now(), "hook_event", json!(0));
        receive.recv().await.unwrap();
        for n in 0..1000 {
            raw.capture(chrono::Utc::now(), "hook_event", json!(n));
        }
        // Completion against the still-stalled append proves capture never waits.
        assert_eq!(raw.dropped(), 936);
        assert!(
            receive.try_recv().is_err(),
            "spawned another append while one was hung"
        );
        // Exercise the actual connection tap while the raw writer is hung and
        // its queue is full: translation and ping must still complete.
        use crate::server::{
            methods::{router, MethodServices},
            ConnectionHandler,
        };
        use atm_protocol::{ClientMessage, DaemonMessage};
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let registry = crate::registry::spawn_registry();
        let methods = Arc::new(router(MethodServices::new(registry.clone())).unwrap());
        let (ours, peer) = tokio::net::UnixStream::pair().unwrap();
        let (read, write) = ours.into_split();
        let cancel = tokio_util::sync::CancellationToken::new();
        let handler = ConnectionHandler::new(
            read,
            write,
            registry.clone(),
            Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
            1,
            cancel.clone(),
            methods,
        )
        .with_raw(raw);
        let task = tokio::spawn(handler.run());
        let (read, mut write) = peer.into_split();
        let mut read = BufReader::new(read);
        let probe = async {
            for message in [
                ClientMessage::connect(None),
                ClientMessage::pi_event(json!({
                    "event":"session_start", "session_id":"raw-stalled", "pid":std::process::id(),
                    "payload":{"type":"session_start","reason":"startup"}
                })),
                ClientMessage::ping(42),
            ] {
                write
                    .write_all(format!("{}\n", serde_json::to_string(&message).unwrap()).as_bytes())
                    .await
                    .unwrap();
            }
            let mut line = String::new();
            read.read_line(&mut line).await.unwrap();
            assert!(matches!(
                serde_json::from_str::<DaemonMessage>(&line).unwrap(),
                DaemonMessage::Connected { .. }
            ));
            line.clear();
            read.read_line(&mut line).await.unwrap();
            assert!(matches!(
                serde_json::from_str::<DaemonMessage>(&line).unwrap(),
                DaemonMessage::Pong { seq: 42 }
            ));
            assert!(registry
                .get_session(atm_core::SessionId::new("raw-stalled"))
                .await
                .is_some());
        };
        tokio::time::timeout(std::time::Duration::from_secs(60), probe)
            .await
            .unwrap();
        cancel.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(60), task)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_raw_flush_fits_stop_budget_after_connection_drain() {
        let raw = RawHandle::with_append(|_| std::future::pending());
        raw.capture(chrono::Utc::now(), "hook_event", json!({}));
        // Raw and summaries flush concurrently after the 2 s connection drain.
        let result = tokio::time::timeout(std::time::Duration::from_secs(3), raw.flush())
            .await
            .unwrap();
        assert!(matches!(result, Err(OrgServiceError::Timeout)));
    }

    #[tokio::test]
    async fn failed_writer_disables_raw_without_retry() {
        let raw = RawHandle::with_append(|_| async { Err(crate::eventlog::EventLogError::Closed) });
        raw.capture(chrono::Utc::now(), "pi_event", json!({}));
        assert!(raw.flush().await.is_err());
        raw.capture(chrono::Utc::now(), "pi_event", json!({}));
        assert!(raw.flush().await.is_err());
    }
}
