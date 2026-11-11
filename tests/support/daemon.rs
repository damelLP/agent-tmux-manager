//! A bounded one-exchange daemon fixture; readiness probes are ignored.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use atm_core::SessionView;
use atm_protocol::{ClientMessage, DaemonMessage, MessageType};

pub fn serve_sessions(socket: &Path, sessions: Vec<SessionView>) -> JoinHandle<()> {
    let listener = UnixListener::bind(socket).expect("bind fixture daemon");
    listener.set_nonblocking(true).expect("nonblocking");
    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            assert!(
                Instant::now() < deadline,
                "no session exchange before deadline"
            );
            let (mut stream, _) = match listener.accept() {
                Ok(pair) => pair,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(e) => panic!("accept: {e}"),
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap() == 0 {
                continue;
            }
            let connect: ClientMessage = serde_json::from_str(&line).unwrap();
            assert!(matches!(connect.message, MessageType::Connect { .. }));
            writeln!(
                stream,
                "{}",
                serde_json::to_string(&DaemonMessage::connected("test".into())).unwrap()
            )
            .unwrap();
            line.clear();
            reader.read_line(&mut line).unwrap();
            let list: ClientMessage = serde_json::from_str(&line).unwrap();
            assert!(matches!(list.message, MessageType::ListSessions));
            writeln!(
                stream,
                "{}",
                serde_json::to_string(&DaemonMessage::session_list(sessions)).unwrap()
            )
            .unwrap();
            return;
        }
    })
}
