//! Background git-context lookups for the registry actor.
//!
//! A lookup reads files under `.git`, and such a read can hang forever (a
//! FIFO, a stuck mount). Each one runs on a detached thread, not
//! `spawn_blocking`: tokio's runtime drop waits forever for blocking-pool
//! threads, so a hung read would keep `atmd` from exiting. The actor never
//! waits for a lookup; results reach it on one channel.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tracing::warn;

/// A directory's project root, worktree path and worktree branch.
pub(super) type GitContext = (Option<String>, Option<String>, Option<String>);

/// How long a lookup runs before it counts as stuck.
const STUCK_AFTER: Duration = Duration::from_secs(5);

/// A lookup that has not reported back yet.
struct InFlight {
    started_at: Instant,
    reported_stuck: bool,
}

/// Runs git-context lookups off the actor, with at most one in flight per
/// directory. A lookup that never returns keeps its slot, so a hung
/// filesystem costs one thread per directory.
pub(super) struct GitLookups {
    resolve: Arc<dyn Fn(&str) -> GitContext + Send + Sync>,
    results: mpsc::UnboundedSender<(String, GitContext)>,
    in_flight: HashMap<String, InFlight>,
}

impl GitLookups {
    /// Creates lookups that call `resolve` with a directory, and the receiver
    /// their results arrive on.
    pub(super) fn new(
        resolve: impl Fn(&str) -> GitContext + Send + Sync + 'static,
    ) -> (Self, mpsc::UnboundedReceiver<(String, GitContext)>) {
        let (results, receiver) = mpsc::unbounded_channel();
        let lookups = Self {
            resolve: Arc::new(resolve),
            results,
            in_flight: HashMap::new(),
        };
        (lookups, receiver)
    }

    /// Starts a lookup of `cwd` unless one is already in flight. A failed
    /// spawn records nothing, so the next request retries.
    pub(super) fn request(&mut self, cwd: &str, now: Instant) {
        if self.in_flight.contains_key(cwd) {
            return;
        }
        let resolve = Arc::clone(&self.resolve);
        let results = self.results.clone();
        let thread_cwd = cwd.to_string();
        let spawned = std::thread::Builder::new()
            .name("atmd-git-lookup".to_string())
            .spawn(move || {
                let context = resolve(&thread_cwd);
                // A send error means the actor is gone.
                let _ = results.send((thread_cwd, context));
            });
        if let Err(e) = spawned {
            warn!(error = %e, cwd, "Failed to start a git lookup");
            return;
        }
        self.in_flight.insert(
            cwd.to_string(),
            InFlight {
                started_at: now,
                reported_stuck: false,
            },
        );
    }

    /// Frees `cwd`'s slot once its result has arrived.
    pub(super) fn finished(&mut self, cwd: &str) {
        self.in_flight.remove(cwd);
    }

    /// Returns the directories whose lookups have just passed [`STUCK_AFTER`].
    /// Each is returned once per lookup, and keeps its slot.
    #[must_use = "the caller logs the stuck directories"]
    pub(super) fn newly_stuck(&mut self, now: Instant) -> Vec<String> {
        let mut stuck = Vec::new();
        for (cwd, lookup) in &mut self.in_flight {
            let age = now.saturating_duration_since(lookup.started_at);
            if !lookup.reported_stuck && age >= STUCK_AFTER {
                lookup.reported_stuck = true;
                stuck.push(cwd.clone());
            }
        }
        stuck
    }

    /// Whether any lookup has not reported back.
    #[cfg(test)]
    pub(super) fn has_in_flight(&self) -> bool {
        !self.in_flight.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc as std_mpsc;
    use std::sync::Mutex;

    const HUNG: &str = "/hung";
    const WAIT: Duration = Duration::from_secs(5);

    fn context_of(cwd: &str) -> GitContext {
        (Some(cwd.to_string()), None, Some("main".to_string()))
    }

    /// A resolver that reports each call, and blocks on [`HUNG`] until the
    /// returned release sender sends or drops.
    #[allow(clippy::type_complexity)]
    fn lookups() -> (
        GitLookups,
        mpsc::UnboundedReceiver<(String, GitContext)>,
        std_mpsc::Receiver<String>,
        std_mpsc::Sender<()>,
    ) {
        let (calls_tx, calls) = std_mpsc::channel();
        let (release, gate) = std_mpsc::channel::<()>();
        let gate = Mutex::new(gate);
        let (lookups, results) = GitLookups::new(move |cwd: &str| {
            calls_tx.send(cwd.to_string()).unwrap();
            if cwd == HUNG {
                let _ = gate.lock().unwrap().recv();
            }
            context_of(cwd)
        });
        (lookups, results, calls, release)
    }

    async fn next_result(
        results: &mut mpsc::UnboundedReceiver<(String, GitContext)>,
    ) -> (String, GitContext) {
        tokio::time::timeout(WAIT, results.recv())
            .await
            .expect("a result within the timeout")
            .expect("the channel stays open")
    }

    /// Fails unless no other call shows up within a short wait.
    fn assert_no_more_calls(calls: &std_mpsc::Receiver<String>) {
        let extra = calls.recv_timeout(Duration::from_millis(100));
        assert!(extra.is_err(), "unexpected resolver call: {extra:?}");
    }

    #[tokio::test]
    async fn result_arrives_on_the_channel() {
        let (mut lookups, mut results, _calls, _release) = lookups();

        lookups.request("/repo", Instant::now());

        let (cwd, context) = next_result(&mut results).await;
        assert_eq!(cwd, "/repo");
        assert_eq!(context, context_of("/repo"));
    }

    #[tokio::test]
    async fn repeated_requests_for_a_cwd_call_the_resolver_once() {
        let (mut lookups, mut results, calls, release) = lookups();
        let now = Instant::now();

        for _ in 0..5 {
            lookups.request(HUNG, now);
        }

        assert_eq!(calls.recv_timeout(WAIT).unwrap(), HUNG);
        assert_no_more_calls(&calls);
        release.send(()).unwrap();
        assert_eq!(next_result(&mut results).await.0, HUNG);
        assert!(lookups.has_in_flight(), "only `finished` frees the slot");
        lookups.request(HUNG, now);
        assert_no_more_calls(&calls);
    }

    #[tokio::test]
    async fn finished_frees_the_slot() {
        let (mut lookups, mut results, calls, _release) = lookups();
        let now = Instant::now();

        lookups.request("/repo", now);
        let (cwd, _) = next_result(&mut results).await;
        lookups.finished(&cwd);
        assert!(!lookups.has_in_flight());
        lookups.request("/repo", now);

        assert_eq!(calls.recv_timeout(WAIT).unwrap(), "/repo");
        assert_eq!(calls.recv_timeout(WAIT).unwrap(), "/repo");
    }

    #[tokio::test]
    async fn a_second_cwd_resolves_while_the_first_hangs() {
        let (mut lookups, mut results, calls, _release) = lookups();
        let now = Instant::now();

        lookups.request(HUNG, now);
        assert_eq!(calls.recv_timeout(WAIT).unwrap(), HUNG);
        lookups.request("/repo", now);

        assert_eq!(next_result(&mut results).await.0, "/repo");
    }

    #[tokio::test]
    async fn a_stuck_lookup_is_reported_once_after_five_seconds_and_keeps_its_slot() {
        let (mut lookups, _results, calls, _release) = lookups();
        let start = Instant::now();

        lookups.request(HUNG, start);
        assert_eq!(calls.recv_timeout(WAIT).unwrap(), HUNG);

        let almost = start + Duration::from_millis(4_999);
        assert!(lookups.newly_stuck(almost).is_empty());
        let stuck = start + Duration::from_secs(5);
        assert_eq!(lookups.newly_stuck(stuck), [HUNG]);
        assert!(lookups.newly_stuck(stuck).is_empty());
        assert!(lookups.newly_stuck(stuck + WAIT).is_empty());

        lookups.request(HUNG, stuck + WAIT);
        assert_no_more_calls(&calls);
    }

    #[test]
    fn a_clock_running_backwards_reports_nothing() {
        let (mut lookups, _results, _calls, _release) = lookups();
        let start = Instant::now() + Duration::from_secs(60);

        lookups.request(HUNG, start);

        assert!(lookups.newly_stuck(Instant::now()).is_empty());
    }

    #[tokio::test]
    async fn a_lookup_that_returns_after_being_stuck_can_be_reported_again() {
        let (mut lookups, mut results, calls, release) = lookups();
        let start = Instant::now();

        lookups.request(HUNG, start);
        assert_eq!(calls.recv_timeout(WAIT).unwrap(), HUNG);
        let stuck = start + STUCK_AFTER;
        assert_eq!(lookups.newly_stuck(stuck), [HUNG]);
        release.send(()).unwrap();
        lookups.finished(&next_result(&mut results).await.0);

        lookups.request(HUNG, stuck);
        assert_eq!(calls.recv_timeout(WAIT).unwrap(), HUNG);
        assert_eq!(lookups.newly_stuck(stuck + STUCK_AFTER), [HUNG]);
    }
}
