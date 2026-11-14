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

#[cfg(test)]
mod tests {
    use super::*;

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
        let stopped = finish_received.recv_timeout(Duration::from_secs(2));
        release.send(()).unwrap();
        thread.join().unwrap();
        assert!(
            stopped.is_ok(),
            "runtime waited on a hung filesystem worker"
        );
        drop_received.recv_timeout(Duration::from_secs(2)).unwrap();
    }
}
