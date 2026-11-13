//! Shared stand-in for CLI commands that fetch the daemon's session list.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::Path;

use atm_core::SessionView;
use atm_protocol::DaemonMessage;

/// A stand-in daemon on `dir/atmd.sock`. It answers `connect` with
/// Connected, then answers ListSessions with `sessions`, or hangs up
/// first when `sessions` is None.
pub fn fake_daemon(dir: &Path, sessions: Option<Vec<SessionView>>) {
    let listener = UnixListener::bind(dir.join("atmd.sock")).expect("bind");
    let connected =
        serde_json::to_string(&DaemonMessage::connected("cli".to_string())).expect("serialize");
    let list = sessions.map(|sessions| {
        serde_json::to_string(&DaemonMessage::session_list(sessions)).expect("serialize")
    });
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let stream = stream.expect("accept");
            let mut reader = BufReader::new(&stream);
            let mut line = String::new();
            // `atm` first checks that something listens, then hangs up
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                continue;
            }
            (&stream)
                .write_all(format!("{connected}\n").as_bytes())
                .expect("reply Connected");
            // Read ListSessions
            line.clear();
            let _ = reader.read_line(&mut line);
            if let Some(list) = &list {
                let _ = (&stream).write_all(format!("{list}\n").as_bytes());
                // Read Disconnect
                line.clear();
                let _ = reader.read_line(&mut line);
            }
        }
    });
}
