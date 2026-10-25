//! Background Beads reads for the compact preview.
//!
//! Each read runs on a detached thread, not `spawn_blocking`: tokio's runtime
//! drop waits forever for blocking-pool threads, so a read stuck on a hung
//! mount or a FIFO would keep `atm` from exiting after quit.

use atm_core::beads::BeadsTask;
use std::time::Duration;
use tokio::sync::oneshot;

/// Reads a directory's in-progress Beads tasks off the render path, with at
/// most one read in flight.
pub struct BeadsReader<F> {
    read: F,
    /// The directory being read and where its result will arrive.
    pending: Option<(String, oneshot::Receiver<Vec<BeadsTask>>)>,
}

impl<F> BeadsReader<F>
where
    F: Fn(&str) -> Vec<BeadsTask> + Clone + Send + 'static,
{
    /// Creates a reader that calls `read` with a project directory.
    pub fn new(read: F) -> Self {
        Self {
            read,
            pending: None,
        }
    }

    /// Starts a read of `dir` unless one is still pending, then waits up to
    /// `timeout` for the pending read.
    ///
    /// Returns the directory read and its newest in-progress task once a read
    /// completes. Returns `None` while the read is still running, so the caller
    /// keeps its last value, and when there is no directory to read.
    pub async fn poll(
        &mut self,
        dir: Option<&str>,
        timeout: Duration,
    ) -> Option<(String, Option<BeadsTask>)> {
        if self.pending.is_none() {
            let dir = dir?.to_string();
            let (tx, rx) = oneshot::channel();
            let read = self.read.clone();
            let thread_dir = dir.clone();
            std::thread::Builder::new()
                .name("atm-beads-read".to_string())
                .spawn(move || {
                    let _ = tx.send(read(&thread_dir));
                })
                .ok()?;
            self.pending = Some((dir, rx));
        }
        let (_, rx) = self.pending.as_mut()?;
        // Timed out: the read stays pending, so a hung read holds one thread
        // rather than one per poll.
        let result = tokio::time::timeout(timeout, rx).await.ok()?;
        let (dir, _) = self.pending.take()?;
        // A read thread that panicked sent nothing; the next poll reads afresh.
        Some((dir, result.ok()?.into_iter().next()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
    use std::sync::{mpsc, Arc, Mutex};
    use std::time::Instant;

    const SHORT: Duration = Duration::from_millis(20);
    const LONG: Duration = Duration::from_secs(5);

    fn task(id: &str) -> BeadsTask {
        BeadsTask {
            id: id.to_string(),
            title: format!("{id} title"),
            description: None,
        }
    }

    /// `(dir, task id)` of a poll result, for readable assertions.
    fn summary(polled: Option<(String, Option<BeadsTask>)>) -> Option<(String, Option<String>)> {
        polled.map(|(dir, task)| (dir, task.map(|t| t.id)))
    }

    fn read_of(dir: &str, id: Option<&str>) -> Option<(String, Option<String>)> {
        Some((dir.to_string(), id.map(str::to_string)))
    }

    #[tokio::test]
    async fn test_poll_returns_newest_task_for_dir() {
        let mut reader = BeadsReader::new(|_: &str| vec![task("newest"), task("older")]);

        let polled = reader.poll(Some("/p"), LONG).await;

        assert_eq!(summary(polled), read_of("/p", Some("newest")));
    }

    #[tokio::test]
    async fn test_poll_reports_no_task_when_read_finds_none() {
        // find_in_progress_tasks maps a missing file or read error to no tasks.
        let mut reader = BeadsReader::new(|_: &str| Vec::new());

        let polled = reader.poll(Some("/p"), LONG).await;

        assert_eq!(summary(polled), read_of("/p", None));
    }

    #[tokio::test]
    async fn test_poll_without_dir_reads_nothing() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let mut reader = BeadsReader::new(move |dir: &str| {
            counter.fetch_add(1, SeqCst);
            vec![task(dir)]
        });

        assert!(reader.poll(None, LONG).await.is_none());
        assert_eq!(calls.load(SeqCst), 0);
    }

    #[tokio::test]
    async fn test_hung_read_never_starts_a_second_read() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        // Never returns, like a read of a FIFO with no writer or a hung mount.
        let mut reader = BeadsReader::new(move |_: &str| -> Vec<BeadsTask> {
            counter.fetch_add(1, SeqCst);
            loop {
                std::thread::park();
            }
        });

        for dir in ["/a", "/b", "/a", "/b"] {
            assert!(reader.poll(Some(dir), SHORT).await.is_none(), "{dir}");
        }

        // The first read's thread may start late on a loaded host; a second
        // read, wrongly started by any of the polls, would have run by now.
        let deadline = Instant::now() + LONG;
        while calls.load(SeqCst) == 0 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        tokio::time::sleep(SHORT).await;
        assert_eq!(calls.load(SeqCst), 1);
    }

    #[tokio::test]
    async fn test_late_result_reports_its_own_dir_then_next_read_starts() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let (release, gate) = mpsc::channel::<()>();
        let gate = Arc::new(Mutex::new(gate));
        let mut reader = BeadsReader::new(move |dir: &str| {
            counter.fetch_add(1, SeqCst);
            let _ = gate.lock().unwrap().recv(); // returns once `release` drops
            vec![task(dir)]
        });

        assert!(reader.poll(Some("/a"), SHORT).await.is_none(), "slow read");
        drop(release);

        // The selection moved to /b meanwhile; the finished read is still /a's.
        let polled = reader.poll(Some("/b"), LONG).await;
        assert_eq!(summary(polled), read_of("/a", Some("/a")));
        let polled = reader.poll(Some("/b"), LONG).await;
        assert_eq!(summary(polled), read_of("/b", Some("/b")));
        assert_eq!(calls.load(SeqCst), 2);
    }

    #[tokio::test]
    async fn test_panicked_read_is_dropped_and_the_next_poll_reads_again() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let mut reader = BeadsReader::new(move |dir: &str| {
            if counter.fetch_add(1, SeqCst) == 0 {
                panic!("first read fails");
            }
            vec![task(dir)]
        });

        assert!(reader.poll(Some("/a"), LONG).await.is_none());
        let polled = reader.poll(Some("/a"), LONG).await;
        assert_eq!(summary(polled), read_of("/a", Some("/a")));
    }
}
