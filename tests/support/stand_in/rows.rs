//! Committed daemon rows from one subscription established before test actions.
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use atm_core::SessionView;
use atm_protocol::{ClientMessage, DaemonMessage};

const GUARD: Duration = Duration::from_secs(30);

struct State {
    rows: Vec<SessionView>,
    closed: Option<String>,
}

pub struct Rows {
    stream: UnixStream,
    state: Arc<(Mutex<State>, Condvar)>,
    reader: Option<JoinHandle<()>>,
}

fn read_message(reader: &mut impl BufRead) -> Result<DaemonMessage, String> {
    let mut line = String::new();
    reader.read_line(&mut line).map_err(|e| e.to_string())?;
    if line.is_empty() {
        return Err("daemon subscription closed".into());
    }
    serde_json::from_str(&line).map_err(|e| e.to_string())
}

impl Rows {
    pub fn connect(path: &Path) -> Self {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut stream = runtime.block_on(async {
            tokio::time::timeout(GUARD, tokio::net::UnixStream::connect(path))
                .await
                .unwrap()
                .unwrap()
                .into_std()
                .unwrap()
        });
        stream.set_nonblocking(false).unwrap();
        stream.set_read_timeout(Some(GUARD)).unwrap();
        stream.set_write_timeout(Some(GUARD)).unwrap();
        let mut read = BufReader::new(stream.try_clone().unwrap());
        writeln!(
            stream,
            "{}",
            serde_json::to_string(&ClientMessage::connect(Some("stand-in-rows".into()))).unwrap()
        )
        .unwrap();
        assert!(matches!(
            read_message(&mut read).unwrap(),
            DaemonMessage::Connected { .. }
        ));
        writeln!(
            stream,
            "{}",
            serde_json::to_string(&ClientMessage::subscribe(None)).unwrap()
        )
        .unwrap();
        let DaemonMessage::SessionList { sessions } = read_message(&mut read).unwrap() else {
            panic!("expected initial subscription snapshot");
        };
        // No idle read timeout: waiters have guards, and Drop shuts down the FD.
        stream.set_read_timeout(None).unwrap();
        let state = Arc::new((
            Mutex::new(State {
                rows: sessions,
                closed: None,
            }),
            Condvar::new(),
        ));
        let shared = state.clone();
        let reader = std::thread::spawn(move || loop {
            let message = read_message(&mut read);
            let mut state = shared.0.lock().unwrap();
            match message {
                Ok(DaemonMessage::SessionUpdated { session }) => {
                    state.rows.retain(|row| row.id != session.id);
                    state.rows.push(*session);
                }
                Ok(DaemonMessage::SessionRemoved { session_id }) => {
                    state.rows.retain(|row| row.id != session_id);
                }
                Ok(DaemonMessage::SessionList { sessions }) => state.rows = sessions,
                Ok(other) => {
                    state.closed = Some(format!("unexpected subscription message: {other:?}"))
                }
                Err(error) => state.closed = Some(error),
            }
            shared.1.notify_all();
            if state.closed.is_some() {
                break;
            }
        });
        Self {
            stream,
            state,
            reader: Some(reader),
        }
    }

    pub fn wait<T>(&self, predicate: impl Fn(&[SessionView]) -> Option<T>) -> T {
        let deadline = Instant::now() + GUARD;
        let mut state = self.state.0.lock().unwrap();
        loop {
            if let Some(result) = predicate(&state.rows) {
                return result;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() || state.closed.is_some() {
                let diagnostic = format!("closed={:?}; rows={:?}", state.closed, state.rows);
                drop(state);
                panic!("daemon subscription wait failed: {diagnostic}");
            }
            state = self.state.1.wait_timeout(state, remaining).unwrap().0;
        }
    }
}

impl Drop for Rows {
    fn drop(&mut self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atm_core::SessionId;
    use atm_protocol::{ClientMessage, DaemonMessage};
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;

    #[test]
    fn subscription_consumes_snapshot_before_returning_then_updates_and_removal() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("rows.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let (remove, release) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(30)))
                .unwrap();
            let mut read = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            read.read_line(&mut line).unwrap();
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&line).unwrap()["type"],
                "connect"
            );
            writeln!(
                stream,
                "{}",
                serde_json::to_string(&DaemonMessage::connected("rows-test".into())).unwrap()
            )
            .unwrap();
            line.clear();
            read.read_line(&mut line).unwrap();
            let want = serde_json::to_value(ClientMessage::subscribe(None)).unwrap();
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&line).unwrap(),
                want
            );
            let mut row = atm_core::SessionView::from_domain(&atm_core::SessionDomain::new(
                SessionId::new("test-row"),
                Default::default(),
                Default::default(),
            ));
            writeln!(
                stream,
                "{}",
                serde_json::to_string(&DaemonMessage::session_list(vec![row.clone()])).unwrap()
            )
            .unwrap();
            release
                .recv_timeout(std::time::Duration::from_secs(30))
                .unwrap();
            row.status = atm_core::SessionStatus::Working;
            writeln!(
                stream,
                "{}",
                serde_json::to_string(&DaemonMessage::session_updated(row)).unwrap()
            )
            .unwrap();
            release
                .recv_timeout(std::time::Duration::from_secs(30))
                .unwrap();
            writeln!(
                stream,
                "{}",
                serde_json::to_string(&DaemonMessage::session_removed(SessionId::new("test-row")))
                    .unwrap()
            )
            .unwrap();
        });
        let rows = Rows::connect(&socket);
        assert_eq!(
            rows.wait(|rows| rows.first().cloned()).id.as_str(),
            "test-row"
        );
        remove.send(()).unwrap();
        rows.wait(|rows| {
            rows.iter()
                .any(|r| r.status == atm_core::SessionStatus::Working)
                .then_some(())
        });
        remove.send(()).unwrap();
        rows.wait(|rows| rows.is_empty().then_some(()));
        server.join().unwrap();
    }
}
