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
    use std::sync::{Arc, Condvar, Mutex};
    use tokio::sync::mpsc;

    const SHORT: Duration = Duration::from_millis(20);
    const LONG: Duration = Duration::from_secs(60);

    struct CountedRead {
        admissions: Arc<AtomicUsize>,
        calls: Arc<AtomicUsize>,
    }

    impl Clone for CountedRead {
        fn clone(&self) -> Self {
            // BeadsReader clones the read closure synchronously when it admits
            // a thread, before that thread can be delayed by the OS scheduler.
            self.admissions.fetch_add(1, SeqCst);
            Self {
                admissions: self.admissions.clone(),
                calls: self.calls.clone(),
            }
        }
    }

    impl CountedRead {
        fn entered(&self) {
            self.calls.fetch_add(1, SeqCst);
        }
    }

    struct HeldReads {
        gate: Arc<(Mutex<bool>, Condvar)>,
        entered: mpsc::UnboundedReceiver<String>,
        admissions: Arc<AtomicUsize>,
    }

    impl Drop for HeldReads {
        fn drop(&mut self) {
            // Also release every owned worker when an assertion unwinds.
            *self.gate.0.lock().unwrap() = true;
            self.gate.1.notify_all();
        }
    }

    fn held_reads(
        dirs: &'static [&'static str],
        calls: Arc<AtomicUsize>,
    ) -> (
        BeadsReader<impl Fn(&str) -> Vec<BeadsTask> + Clone + Send>,
        HeldReads,
    ) {
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let admissions = Arc::new(AtomicUsize::new(0));
        let counted = CountedRead {
            admissions: admissions.clone(),
            calls,
        };
        let (enter, entered) = mpsc::unbounded_channel();
        let held = gate.clone();
        let reader = BeadsReader::new(move |dir: &str| {
            counted.entered();
            let _ = enter.send(dir.to_string());
            if dirs.contains(&dir) {
                drop(
                    held.1
                        .wait_while(held.0.lock().unwrap(), |released| !*released)
                        .unwrap(),
                );
            }
            vec![task(dir)]
        });
        (
            reader,
            HeldReads {
                gate,
                entered,
                admissions,
            },
        )
    }

    async fn entered<F>(reader: &mut BeadsReader<F>, held: &mut HeldReads, dir: &str)
    where
        F: Fn(&str) -> Vec<BeadsTask> + Clone + Send + 'static,
    {
        {
            let read = reader.poll(Some(dir), LONG);
            tokio::pin!(read);
            let first = std::future::poll_fn(|cx| {
                std::task::Poll::Ready(std::future::Future::poll(read.as_mut(), cx))
            })
            .await;
            assert!(first.is_pending(), "held read completed");
        }
        assert_eq!(
            tokio::time::timeout(LONG, held.entered.recv())
                .await
                .unwrap()
                .as_deref(),
            Some(dir)
        );
    }

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
        let (mut reader, mut held) = held_reads(&["/a", "/b"], calls.clone());
        entered(&mut reader, &mut held, "/a").await;
        entered(&mut reader, &mut held, "/b").await;
        tokio::time::pause();
        for dir in ["/a", "/b", "/a", "/b", "/a"] {
            assert!(reader.poll(Some(dir), SHORT).await.is_none(), "{dir}");
        }
        let fenced_at = tokio::time::Instant::now();
        // Admission is synchronous: even a third thread that has not started
        // would already have cloned the read closure. Both workers stay held.
        assert_eq!(held.admissions.load(SeqCst), 2);
        assert_eq!(reader.pending.len(), 2);
        assert_eq!(calls.load(SeqCst), 2);
        assert_eq!(
            tokio::time::Instant::now(),
            fenced_at,
            "read admission needed a settling delay"
        );
        tokio::time::resume();
        drop(held);
        for dir in ["/a", "/b"] {
            assert_eq!(
                summary(reader.poll(Some(dir), LONG).await),
                read_of(dir, Some(dir))
            );
        }
        assert_eq!(calls.load(SeqCst), 2);
    }

    #[tokio::test]
    async fn test_hung_read_in_one_dir_does_not_block_another() {
        // /a stays held, like a project on a hung mount; /b is healthy.
        let (mut reader, mut held) = held_reads(&["/a"], Arc::new(AtomicUsize::new(0)));
        entered(&mut reader, &mut held, "/a").await;
        tokio::time::pause();
        assert!(reader.poll(Some("/a"), SHORT).await.is_none());
        tokio::time::resume();
        let polled = reader.poll(Some("/b"), LONG).await;
        assert_eq!(summary(polled), read_of("/b", Some("/b")));
        tokio::time::pause();
        assert!(
            reader.poll(Some("/a"), SHORT).await.is_none(),
            "/a still hung"
        );
        tokio::time::resume();
        drop(held);
        assert_eq!(
            summary(reader.poll(Some("/a"), LONG).await),
            read_of("/a", Some("/a"))
        );
    }

    #[tokio::test]
    async fn test_late_result_returns_when_its_dir_is_polled_again() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (mut reader, mut held) = held_reads(&["/a"], calls.clone());
        entered(&mut reader, &mut held, "/a").await;
        tokio::time::pause();
        assert!(reader.poll(Some("/a"), SHORT).await.is_none(), "slow read");
        tokio::time::resume();
        drop(held);

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
