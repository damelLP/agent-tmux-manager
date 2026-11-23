//! Blocking startup is detached: a hung filesystem cannot hold Tokio shutdown.

use std::{path::PathBuf, time::Duration};

use atm_org::{OrgState, ReplayStats};
use tokio::sync::oneshot;

use crate::eventlog::{read_from, EventLog, Stream};

pub(super) struct Ready {
    pub log: EventLog,
    pub state: OrgState,
    pub seq: u64,
}

pub(super) async fn open(state_dir: PathBuf) -> Result<Ready, String> {
    detached_open(super::DEADLINE, move || {
        let log = EventLog::open(&state_dir, Stream::Events).map_err(|e| e.to_string())?;
        let mut state = OrgState::default();
        let mut stats = ReplayStats::default();
        let mut seq = 0;
        for record in read_from(&state_dir, Stream::Events, 0) {
            let record = record.map_err(|e| e.to_string())?;
            stats
                .apply_parts(
                    &mut state,
                    record.seq,
                    record.ts,
                    &record.kind,
                    &record.body,
                )
                .map_err(|e| format!("events replay at sequence {}: {e}", record.seq))?;
            seq = record.seq;
        }
        tracing::info!(
            applied = stats.applied,
            skipped_unknown = stats.skipped_unknown,
            seq,
            "Organisation replay complete"
        );
        Ok(Ready { log, state, seq })
    })
    .await
}

pub(super) async fn detached_open<T: Send + 'static>(
    deadline: Duration,
    open: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let (send, receive) = oneshot::channel();
    std::thread::Builder::new()
        .name("atmd-log-open".into())
        .spawn(move || {
            // If the deadline expired, dropping the late result releases all
            // its handles. No late transition from disabled to enabled.
            let _ = send.send(open());
        })
        .map_err(|e| format!("cannot start log open: {e}"))?;
    tokio::time::timeout(deadline, receive)
        .await
        .map_err(|_| "log open/replay timed out; restart to recover".to_string())?
        .map_err(|_| "log open/replay worker stopped".to_string())?
}

/// One detached reader per service, not per call. A timed-out caller drops its
/// reply, never the in-flight worker; even a hung read cannot multiply threads.
#[derive(Clone, Debug)]
pub(super) struct Reader {
    requests: tokio::sync::mpsc::Sender<ReadRequest>,
}

struct ReadRequest {
    after: u64,
    fence: u64,
    topics: Vec<String>,
    reply: oneshot::Sender<Result<Page, super::OrgServiceError>>,
}

#[derive(Debug)]
pub(super) struct Page {
    pub records: Vec<crate::eventlog::Envelope>,
    pub scanned: u64,
}

impl Reader {
    pub fn start(state_dir: PathBuf) -> Result<Self, String> {
        Self::spawn(move |after, fence, topics| read_page(&state_dir, after, fence, topics))
    }

    fn spawn(
        mut read: impl FnMut(u64, u64, &[String]) -> Result<Page, super::OrgServiceError>
            + Send
            + 'static,
    ) -> Result<Self, String> {
        let (requests, mut receive) = tokio::sync::mpsc::channel::<ReadRequest>(8);
        std::thread::Builder::new()
            .name("atmd-log-read".into())
            .spawn(move || {
                while let Some(job) = receive.blocking_recv() {
                    if !job.reply.is_closed() {
                        let _ = job.reply.send(read(job.after, job.fence, &job.topics));
                    }
                }
            })
            .map_err(|e| format!("cannot start log reader: {e}"))?;
        Ok(Self { requests })
    }

    // Use the same request/response path without a filesystem thread that
    // would race a paused Tokio clock's auto-advance.
    #[cfg(test)]
    pub(super) fn in_memory_for_test(
        mut read: impl FnMut(u64, u64, &[String]) -> Result<Page, super::OrgServiceError>
            + Send
            + 'static,
    ) -> Self {
        let (requests, mut receive) = tokio::sync::mpsc::channel::<ReadRequest>(8);
        tokio::spawn(async move {
            while let Some(job) = receive.recv().await {
                let _ = job.reply.send(read(job.after, job.fence, &job.topics));
            }
        });
        Self { requests }
    }

    pub async fn page(
        &self,
        after: u64,
        fence: u64,
        topics: &[String],
    ) -> Result<Page, super::OrgServiceError> {
        let (reply, response) = oneshot::channel();
        self.requests
            .try_send(ReadRequest {
                after,
                fence,
                topics: topics.to_vec(),
                reply,
            })
            .map_err(|e| super::OrgServiceError::Unavailable(format!("log reader queue: {e}")))?;
        tokio::time::timeout(super::DEADLINE, response)
            .await
            .map_err(|_| super::OrgServiceError::Timeout)?
            .map_err(|_| super::OrgServiceError::Unavailable("log reader stopped".into()))?
    }
}

fn read_page(
    state_dir: &std::path::Path,
    after: u64,
    fence: u64,
    topics: &[String],
) -> Result<Page, super::OrgServiceError> {
    let mut page = Page {
        records: Vec::new(),
        scanned: after,
    };
    if after >= fence {
        return Ok(page);
    }
    let mut bytes = 0;
    let mut records = read_from(state_dir, Stream::Events, after);
    for _ in 0..256 {
        let record = records
            .next()
            .ok_or_else(|| {
                super::OrgServiceError::Unavailable(format!(
                    "events log ended before committed sequence {fence}"
                ))
            })?
            .map_err(|e| super::OrgServiceError::Unavailable(e.to_string()))?;
        if record.seq > fence {
            return Err(super::OrgServiceError::Unavailable(format!(
                "missing committed sequence {fence}"
            )));
        }
        if super::watch::matches(topics, &record.kind) {
            let size = serde_json::to_vec(&record)
                .map_err(|e| super::OrgServiceError::Unavailable(e.to_string()))?
                .len();
            if !page.records.is_empty() && bytes + size > super::watch::TAIL_BYTES {
                break;
            }
            bytes += size;
            page.scanned = record.seq;
            page.records.push(record);
        } else {
            page.scanned = record.seq;
        }
        // Never read beyond the ACK/apply fence, including an incomplete or bad
        // pending append. One oversized record is returned alone so tail can
        // refuse it explicitly; watch need not silently skip a large event.
        if page.scanned == fence || bytes >= super::watch::TAIL_BYTES {
            break;
        }
    }
    Ok(page)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn read_hang_never_spawns_replacement_threads_and_recovers_after_timeout() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let (entered, entry) = oneshot::channel();
        let mut entered = Some(entered);
        let (release, gate) = std::sync::mpsc::channel();
        let reader = Reader::spawn(move |after, _, _| {
            count.fetch_add(1, Ordering::SeqCst);
            if let Some(entered) = entered.take() {
                let _ = entered.send(());
                gate.recv().unwrap();
            }
            Ok(Page {
                records: vec![],
                scanned: after,
            })
        })
        .unwrap();
        let first = reader.clone();
        let first = tokio::spawn(async move { first.page(0, 1, &[]).await });
        entry.await.unwrap();
        // Only the fake read is stalled. Paused time exercises production
        // deadlines without risking real file I/O races.
        tokio::time::pause();
        assert!(matches!(
            first.await.unwrap(),
            Err(super::super::OrgServiceError::Timeout)
        ));
        for _ in 0..20 {
            assert!(reader.page(0, 1, &[]).await.is_err());
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        tokio::time::resume();
        release.send(()).unwrap();
        // Timed-out queued jobs are skipped. Once drained, the same reader
        // remains usable; no abandoned operation needs a replacement thread.
        let page = tokio::time::timeout(Duration::from_secs(60), async {
            // Queue capacity is the synchronization point; no other callers
            // can take this slot after we release the reservation.
            drop(reader.requests.reserve().await.unwrap());
            reader.page(7, 8, &[]).await.unwrap()
        })
        .await
        .unwrap();
        assert_eq!(page.scanned, 7);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn hung_reader_does_not_hold_runtime_shutdown() {
        let (release, gate) = std::sync::mpsc::channel();
        let (finished, finish_received) = std::sync::mpsc::channel();
        let (exited, exit_received) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let (entered, entry) = oneshot::channel();
            let mut entered = Some(entered);
            let reader = Reader::spawn(move |_, _, _| {
                if let Some(entered) = entered.take() {
                    let _ = entered.send(());
                }
                gate.recv().unwrap();
                exited.send(()).unwrap();
                Ok(Page {
                    records: vec![],
                    scanned: 1,
                })
            })
            .unwrap();
            runtime.block_on(async {
                let caller = tokio::spawn(async move { reader.page(0, 1, &[]).await });
                entry.await.unwrap();
                caller.abort();
                assert!(caller.await.unwrap_err().is_cancelled());
            });
            drop(runtime);
            finished.send(()).unwrap();
        });
        let stopped = finish_received.recv_timeout(Duration::from_secs(60));
        release.send(()).unwrap();
        thread.join().unwrap();
        assert!(stopped.is_ok(), "runtime waited for a hung reader");
        exit_received.recv_timeout(Duration::from_secs(60)).unwrap();
    }

    #[test]
    fn read_page_does_not_inspect_uncommitted_corrupt_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let events = dir.path().join("log/events");
        std::fs::create_dir_all(&events).unwrap();
        std::fs::write(
            events.join("00000001.jsonl"),
            concat!(
            "{\"seq\":1,\"ts\":\"2023-11-14T22:13:20Z\",\"kind\":\"control.sent\",\"body\":{}}\n",
            "incomplete pending write\n",
        ),
        )
        .unwrap();
        assert_eq!(read_page(dir.path(), 0, 1, &[]).unwrap().records.len(), 1);
        assert!(read_page(dir.path(), 0, 2, &[]).is_err());
    }

    // If this used spawn_blocking, dropping the runtime would wait for release.
    #[test]
    fn hung_open_does_not_block_runtime_shutdown_and_late_result_is_dropped() {
        struct LateResult(std::sync::mpsc::Sender<()>);
        impl Drop for LateResult {
            fn drop(&mut self) {
                let _ = self.0.send(());
            }
        }
        let (release, gate) = std::sync::mpsc::channel();
        let (dropped, drop_received) = std::sync::mpsc::channel();
        let (finished, finish_received) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let result = runtime.block_on(detached_open(Duration::from_millis(20), move || {
                gate.recv().unwrap();
                Ok(LateResult(dropped))
            }));
            assert!(matches!(result, Err(ref error) if error.contains("timed out")));
            drop(runtime);
            finished.send(()).unwrap();
        });
        let stopped = finish_received.recv_timeout(Duration::from_secs(60));
        release.send(()).unwrap();
        thread.join().unwrap();
        assert!(
            stopped.is_ok(),
            "runtime waited on a hung filesystem worker"
        );
        drop_received.recv_timeout(Duration::from_secs(60)).unwrap();
    }
}
