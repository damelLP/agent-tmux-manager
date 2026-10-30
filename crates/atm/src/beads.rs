//! Background Beads reads for the compact preview.
//!
//! Each read runs on a detached thread, not `spawn_blocking`: tokio's runtime
//! drop waits forever for blocking-pool threads, so a read stuck on a hung
//! mount or a FIFO would keep `atm` from exiting after quit.

mod task;
pub use task::{find_in_progress_tasks, BeadsTask};

use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::oneshot;
use tracing::warn;

/// Reads a directory's in-progress Beads tasks off the render path, with at
/// most one read in flight per directory.
pub struct BeadsReader<F> {
    read: F,
    /// Reads not yet returned, by directory: where each result will arrive.
    pending: HashMap<String, oneshot::Receiver<Vec<BeadsTask>>>,
}

impl<F> BeadsReader<F>
where
    F: Fn(&str) -> Vec<BeadsTask> + Clone + Send + 'static,
{
    /// Creates a reader that calls `read` with a project directory.
    pub fn new(read: F) -> Self {
        Self {
            read,
            pending: HashMap::new(),
        }
    }

    /// Starts a read of `dir` unless one is still pending, then waits up to
    /// `timeout` for it.
    ///
    /// Returns `dir` and its newest in-progress task once the read completes.
    /// Returns `None` while the read is still running, so the caller keeps its
    /// last value, and when there is no directory to read.
    #[must_use = "dropping the result loses a finished read"]
    pub async fn poll(
        &mut self,
        dir: Option<&str>,
        timeout: Duration,
    ) -> Option<(String, Option<BeadsTask>)> {
        let dir = dir?;
        if !self.pending.contains_key(dir) {
            let (tx, rx) = oneshot::channel();
            let read = self.read.clone();
            let thread_dir = dir.to_string();
            let spawned = std::thread::Builder::new()
                .name("atm-beads-read".to_string())
                .spawn(move || {
                    let _ = tx.send(read(&thread_dir));
                });
            if let Err(e) = spawned {
                warn!(error = %e, dir, "Failed to start a Beads read");
                return None;
            }
            self.pending.insert(dir.to_string(), rx);
        }
        let rx = self.pending.get_mut(dir)?;
        // Timed out: the read stays pending, so a hung directory holds one
        // thread rather than one per poll, and other directories still read.
        let result = tokio::time::timeout(timeout, rx).await.ok()?;
        self.pending.remove(dir);
        match result {
            Ok(tasks) => Some((dir.to_string(), tasks.into_iter().next())),
            Err(_) => {
                // The read thread panicked and sent nothing; the next poll reads afresh.
                warn!(dir, "Beads read ended without a result");
                None
            }
        }
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
    async fn test_hung_read_never_starts_a_second_read_of_its_dir() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        // Never returns, like a read of a FIFO with no writer or a hung mount.
        let mut reader = BeadsReader::new(move |_: &str| -> Vec<BeadsTask> {
            counter.fetch_add(1, SeqCst);
            loop {
                std::thread::park();
            }
        });

        for dir in ["/a", "/b", "/a", "/b", "/a"] {
            assert!(reader.poll(Some(dir), SHORT).await.is_none(), "{dir}");
        }

        // One read per directory. The reads' threads may start late on a
        // loaded host; a third read, wrongly started by any of the polls,
        // would have run by now.
        let deadline = Instant::now() + LONG;
        while calls.load(SeqCst) < 2 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        tokio::time::sleep(SHORT).await;
        assert_eq!(calls.load(SeqCst), 2);
    }

    #[tokio::test]
    async fn test_hung_read_in_one_dir_does_not_block_another() {
        // /a never returns, like a project on a hung mount; /b is healthy.
        let mut reader = BeadsReader::new(|dir: &str| -> Vec<BeadsTask> {
            if dir == "/a" {
                loop {
                    std::thread::park();
                }
            }
            vec![task(dir)]
        });

        assert!(reader.poll(Some("/a"), SHORT).await.is_none());
        let polled = reader.poll(Some("/b"), LONG).await;
        assert_eq!(summary(polled), read_of("/b", Some("/b")));
        assert!(
            reader.poll(Some("/a"), SHORT).await.is_none(),
            "/a still hung"
        );
    }

    #[tokio::test]
    async fn test_late_result_returns_when_its_dir_is_polled_again() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let (release, gate) = mpsc::channel::<()>();
        let gate = Arc::new(Mutex::new(gate));
        let mut reader = BeadsReader::new(move |dir: &str| {
            counter.fetch_add(1, SeqCst);
            if dir == "/a" {
                let _ = gate.lock().unwrap().recv(); // returns once `release` drops
            }
            vec![task(dir)]
        });

        assert!(reader.poll(Some("/a"), SHORT).await.is_none(), "slow read");
        drop(release);

        // The selection moved to /b meanwhile: /b reads on its own.
        let polled = reader.poll(Some("/b"), LONG).await;
        assert_eq!(summary(polled), read_of("/b", Some("/b")));
        // Back on /a, the finished read is returned without a new one.
        let polled = reader.poll(Some("/a"), LONG).await;
        assert_eq!(summary(polled), read_of("/a", Some("/a")));
        assert_eq!(calls.load(SeqCst), 2);
        // The next poll of /a reads afresh.
        let polled = reader.poll(Some("/a"), LONG).await;
        assert_eq!(summary(polled), read_of("/a", Some("/a")));
        assert_eq!(calls.load(SeqCst), 3);
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
