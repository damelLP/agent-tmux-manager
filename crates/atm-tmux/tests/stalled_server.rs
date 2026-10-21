//! Real tmux check: a stalled tmux server must not hang a `RealTmuxClient`
//! call, and the timed-out `tmux` client process must not be leaked.
//!
//! Freezes an isolated `tmux -L <label>` server with SIGSTOP. Against
//! tmux 3.8, `list-panes` on a stopped server blocks forever. The test
//! runs on paused tokio time, so the 5s command deadline elapses instantly.
//!
//! Skips (prints a message, passes) when tmux can't start a server.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)] // test code may panic

use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use atm_tmux::{RealTmuxClient, TmuxClient, TmuxError};

/// Isolated tmux server, resumed and killed on drop — including when the
/// test panics while the server is stopped.
struct IsolatedServer {
    label: String,
    pid: Option<String>,
}

impl IsolatedServer {
    fn start() -> Option<Self> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let label = format!("atm-stall-{}-{nanos}", std::process::id());
        // `-f /dev/null`: don't load the developer's tmux.conf.
        let started = Command::new("tmux")
            .args(["-L", &label, "-f", "/dev/null"])
            .args(["new-session", "-d", "-s", "stall"])
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        started.then_some(Self { label, pid: None })
    }

    fn stop(&mut self) {
        let output = Command::new("tmux")
            .args(["-L", &self.label, "display-message", "-p", "#{pid}"])
            .output()
            .expect("query tmux server pid");
        let pid = String::from_utf8(output.stdout).unwrap().trim().to_string();
        assert!(!pid.is_empty(), "tmux server pid should be reported");
        // Record the pid before stopping, so Drop always resumes it.
        self.pid = Some(pid.clone());
        let stopped = Command::new("kill").args(["-STOP", &pid]).status();
        assert!(stopped.is_ok_and(|s| s.success()), "SIGSTOP tmux server");
    }
}

impl Drop for IsolatedServer {
    fn drop(&mut self) {
        if let Some(pid) = &self.pid {
            let _ = Command::new("kill").args(["-CONT", pid]).status();
        }
        let _ = Command::new("tmux")
            .args(["-L", &self.label, "kill-server"])
            .stderr(Stdio::null())
            .status();
    }
}

/// Counts live `tmux -L <label> list-panes` processes. Reads Linux `/proc`;
/// elsewhere it finds none. A killed client's `cmdline` reads empty, so a
/// zombie awaiting reaping is not counted.
fn live_list_panes_clients(label: &str) -> usize {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| std::fs::read(entry.path().join("cmdline")).ok())
        .filter(|cmdline| {
            let args: Vec<&[u8]> = cmdline.split(|b| *b == 0).collect();
            args.contains(&label.as_bytes()) && args.contains(&b"list-panes".as_slice())
        })
        .count()
}

#[tokio::test(start_paused = true)]
async fn stalled_server_call_times_out_without_leaking_client() {
    let Some(mut server) = IsolatedServer::start() else {
        eprintln!("tmux not available, skipping");
        return;
    };
    server.stop();

    let client = RealTmuxClient::with_socket(&server.label);
    // Without a deadline in the client this outer guard is the only timer,
    // so a regression fails here (instantly, on paused time) instead of hanging.
    let result = tokio::time::timeout(Duration::from_secs(60), client.list_panes())
        .await
        .expect("list_panes must give up on a stalled server, not hang");
    assert!(
        matches!(&result, Err(TmuxError::Timeout { command, timeout })
            if command == "list-panes" && *timeout == Duration::from_secs(5)),
        "expected a 5s Timeout for list-panes, got {result:?}"
    );

    // new-session may cold-start a server, which runs the user's tmux.conf
    // first, so it gets a longer deadline. Workspace setup sends it through
    // raw_command.
    let started = tokio::time::Instant::now();
    let result = client
        .raw_command("new-session", &["-d", "-s", "late"])
        .await;
    assert!(
        matches!(&result, Err(TmuxError::Timeout { command, timeout })
            if command == "new-session" && *timeout == Duration::from_secs(30)),
        "expected a 30s Timeout for new-session, got {result:?}"
    );
    assert!(started.elapsed() >= Duration::from_secs(30));

    // kill_on_drop: the blocked client is killed, not left waiting on the
    // server. Real time, since the kill is delivered outside tokio.
    let deadline = Instant::now() + Duration::from_secs(2);
    while live_list_panes_clients(&server.label) > 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        live_list_panes_clients(&server.label),
        0,
        "timed-out tmux client should be killed"
    );
}
