//! Private runtime event collector. Bound before launch, replayed under the same
//! lock as live notification, and keyed by PID + procfs start time (not pane).
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::Value;

const GUARD: Duration = Duration::from_secs(30);

#[derive(Default)]
struct State {
    clients: Vec<UnixStream>,
    events: Vec<(usize, Value)>,
    errors: Vec<String>,
}

type Shared = Arc<(Mutex<State>, Condvar)>;

pub struct Events {
    path: PathBuf,
    shared: Shared,
    stop: Arc<AtomicBool>,
    accept: Option<JoinHandle<Vec<JoinHandle<()>>>>,
    command: AtomicU64,
}

impl Events {
    pub fn bind(path: PathBuf) -> Self {
        let listener = UnixListener::bind(&path).expect("bind private events");
        let shared = Shared::default();
        let stop = Arc::new(AtomicBool::new(false));
        let state = shared.clone();
        let stopping = stop.clone();
        let accept = std::thread::spawn(move || {
            let mut readers = Vec::new();
            for stream in listener.incoming() {
                if stopping.load(Ordering::SeqCst) {
                    break;
                }
                let stream = stream.expect("private event connection");
                stream.set_write_timeout(Some(GUARD)).unwrap();
                let mut guard = state.0.lock().unwrap();
                let client = guard.clients.len();
                guard.clients.push(stream.try_clone().unwrap());
                drop(guard);
                let state = state.clone();
                readers.push(std::thread::spawn(move || {
                    for line in BufReader::new(stream).lines() {
                        let event = line.map_err(|error| error.to_string()).and_then(|line| {
                            serde_json::from_str(&line).map_err(|e| e.to_string())
                        });
                        let mut guard = state.0.lock().unwrap();
                        match event {
                            Ok(event) => guard.events.push((client, event)),
                            Err(error) => {
                                guard.errors.push(error);
                                state.1.notify_all();
                                break;
                            }
                        }
                        state.1.notify_all();
                    }
                }));
            }
            readers
        });
        Self {
            path,
            shared,
            stop,
            accept: Some(accept),
            command: AtomicU64::new(0),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn wait(&self, predicate: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + GUARD;
        let mut guard = self.shared.0.lock().unwrap();
        loop {
            if let Some((_, event)) = guard.events.iter().rev().find(|(_, e)| predicate(e)) {
                return event.clone();
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let diagnostics = format!("events={:?}\nerrors={:?}", guard.events, guard.errors);
                drop(guard); // Do not poison cleanup on a failed test.
                panic!("timed out waiting for runtime event: {diagnostics}");
            }
            guard = self.shared.1.wait_timeout(guard, remaining).unwrap().0;
        }
    }

    pub fn cursor(&self, pid: u32, start_time: &str) -> u64 {
        self.shared
            .0
            .lock()
            .unwrap()
            .events
            .iter()
            .rev()
            .find(|(_, e)| matches_process(e, pid, start_time))
            .and_then(|(_, e)| e["event_sequence"].as_u64())
            .unwrap_or(0)
    }

    pub fn control(&self, pid: u32, start_time: &str, mut command: Value) -> Value {
        let id = self.command.fetch_add(1, Ordering::SeqCst) + 1;
        command["id"] = id.into();
        let mut stream = {
            let guard = self.shared.0.lock().unwrap();
            let (client, _) = guard
                .events
                .iter()
                .rev()
                .find(|(_, e)| matches_process(e, pid, start_time))
                .expect("observed runtime connection");
            guard.clients[*client].try_clone().unwrap()
        };
        writeln!(stream, "{command}").expect("write private control");
        let reply = self.wait(|e| {
            matches_process(e, pid, start_time) && e["kind"] == "control" && e["id"] == id
        });
        assert!(reply.get("error").is_none(), "control refused: {reply}");
        reply
    }
}

pub fn matches_process(event: &Value, pid: u32, start_time: &str) -> bool {
    event["pid"] == pid && event["start_time"] == start_time
}

impl Drop for Events {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = UnixStream::connect(&self.path); // Wake the blocking accept.
        if let Some(accept) = self.accept.take() {
            if let Ok(readers) = accept.join() {
                for stream in &self.shared.0.lock().unwrap().clients {
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                }
                for reader in readers {
                    let _ = reader.join();
                }
            }
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use serde_json::json;

    pub fn stale_start_identity_cannot_match_reused_pid() {
        let stale = json!({"kind": "ready", "pid": 123, "start_time": "old"});
        assert!(!matches_process(&stale, 123, "new"));
        assert!(!matches_process(&stale, 124, "old"));
        assert!(matches_process(&stale, 123, "old"));
    }

    pub fn events_replay_without_lost_wake_and_close_with_open_peer() {
        let dir = tempfile::tempdir().unwrap();
        let events = Events::bind(dir.path().join("events.sock"));
        let mut peer = UnixStream::connect(events.path()).unwrap();
        writeln!(
            peer,
            "{}",
            json!({"kind": "identity", "pid": 123, "start_time": "new", "event_sequence": 1})
        )
        .unwrap();
        let first = events.wait(|e| e["kind"] == "identity");
        // Replay after arrival is the same subscription, not a second snapshot.
        assert_eq!(first, events.wait(|e| e["kind"] == "identity"));
        assert_eq!(events.cursor(123, "new"), 1);
        assert_eq!(events.cursor(123, "old"), 0);
        drop(events); // Must cancel the reader even though peer is still open.
        peer.set_read_timeout(Some(GUARD)).unwrap();
        assert!(BufReader::new(peer).lines().next().is_none());
    }
}
