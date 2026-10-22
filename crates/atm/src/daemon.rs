//! Daemon management for the ATM TUI.
//!
//! Provides utilities for checking if the daemon is running and
//! starting it automatically if needed.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

use socket2::{Domain, SockAddr, Socket, Type};
use tracing::{debug, info};

use crate::client::resolve_socket_path;

/// Returns the path to the daemon PID file.
fn pid_file_path() -> PathBuf {
    dirs::state_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("atm")
        .join("atmd.pid")
}

/// Reads the PID from the PID file, if it exists.
fn read_pid() -> Option<u32> {
    let path = pid_file_path();
    let mut file = File::open(&path).ok()?;
    let mut contents = String::new();
    file.read_to_string(&mut contents).ok()?;
    contents.trim().parse().ok()
}

/// Checks if a process with the given PID is running.
fn is_process_running(pid: u32) -> bool {
    PathBuf::from(format!("/proc/{pid}")).exists()
}

/// Checks if the daemon is currently running: our PID file names a live
/// process, or a daemon is listening on the configured socket. That
/// daemon may use another state dir, and so another PID file.
pub fn is_daemon_running() -> bool {
    read_pid().is_some_and(is_process_running) || is_socket_listening(&resolve_socket_path())
}

/// Whether something accepts connections on `socket`. The connect is
/// non-blocking: a blocking unix connect waits while a stalled daemon's
/// backlog is full, and a full backlog (`EAGAIN`) still means a listener.
fn is_socket_listening(socket: &Path) -> bool {
    let Ok(addr) = SockAddr::unix(socket) else {
        return false;
    };
    let Ok(probe) = Socket::new(Domain::UNIX, Type::STREAM, None) else {
        return false;
    };
    if probe.set_nonblocking(true).is_err() {
        return false;
    }
    match probe.connect(&addr) {
        Ok(()) => true,
        Err(e) => e.kind() == std::io::ErrorKind::WouldBlock,
    }
}

/// Starts the daemon in the background.
///
/// Spawns `atmd start -d` as a detached process.
/// Returns Ok(()) if the command was spawned successfully.
///
/// # Env propagation
///
/// `ATM_SOCKET` is forwarded explicitly so the spawned daemon binds to
/// the same socket the client will connect to via
/// `client::resolve_socket_path()`. `Command::spawn` would inherit the
/// var implicitly anyway, but stating it here keeps the contract
/// visible — a future refactor that adds `.env_clear()` "for
/// cleanliness" won't silently break the auto-start path.
///
/// Empty values are treated as unset (matching `resolve_socket_path`),
/// so atmd falls back to its own default rather than trying to bind an
/// empty path.
fn spawn_daemon() -> std::io::Result<()> {
    // Find atmd binary - try same directory as current binary first
    let atmd_path = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("atmd")))
        .filter(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from("atmd"));

    debug!(path = %atmd_path.display(), "Starting daemon");

    let mut cmd = Command::new(&atmd_path);
    cmd.args(["start", "-d"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if let Some(socket) = std::env::var_os("ATM_SOCKET").filter(|v| !v.is_empty()) {
        cmd.env("ATM_SOCKET", socket);
    }
    let mut child = cmd.spawn()?;
    // `atmd start -d` exits once it has forked the daemon. Reap it without
    // waiting here, so a TUI that respawns the daemon collects no zombies.
    if let Err(e) = thread::Builder::new().spawn(move || child.wait()) {
        debug!(error = %e, "Failed to start a thread to reap atmd start");
    }

    Ok(())
}

/// Ensures the daemon is running, starting it if necessary.
///
/// Returns Ok(()) if daemon is running (or was started successfully).
/// Returns Err with a message if daemon could not be started.
pub fn ensure_daemon_running() -> Result<(), String> {
    if is_daemon_running() {
        debug!("Daemon already running");
        return Ok(());
    }

    info!("Daemon not running, starting it...");

    // Start the daemon
    if let Err(e) = spawn_daemon() {
        return Err(format!("Failed to start daemon: {e}"));
    }

    // Wait for daemon to be ready (up to 3 seconds)
    for i in 0..30 {
        thread::sleep(Duration::from_millis(100));

        if is_daemon_running() {
            info!(attempts = i + 1, "Daemon started successfully");
            return Ok(());
        }
    }

    Err("Daemon failed to start within 3 seconds".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pid_file_path() {
        let path = pid_file_path();
        assert!(path.ends_with("atmd.pid"));
    }

    #[test]
    fn test_is_process_running_current() {
        // Current process should be running
        let pid = std::process::id();
        assert!(is_process_running(pid));
    }

    #[test]
    fn test_is_process_running_nonexistent() {
        // Very high PID should not exist
        assert!(!is_process_running(999_999_999));
    }

    #[test]
    fn test_is_socket_listening() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("atmd.sock");
        assert!(!is_socket_listening(&socket), "no socket file");

        let listener = std::os::unix::net::UnixListener::bind(&socket).expect("bind");
        assert!(is_socket_listening(&socket), "live listener");

        // A crashed daemon leaves the socket file behind.
        drop(listener);
        assert!(!is_socket_listening(&socket), "stale socket file");
    }

    /// A stalled daemon whose backlog is full still counts, and the probe
    /// must not wait for room in the backlog.
    #[test]
    fn test_is_socket_listening_with_a_full_backlog() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("atmd.sock");
        let addr = SockAddr::unix(&socket).expect("addr");
        let listener = Socket::new(Domain::UNIX, Type::STREAM, None).expect("socket");
        listener.bind(&addr).expect("bind");
        listener.listen(0).expect("listen");
        // Never accepted, so these fill the backlog. A blocking connect
        // here would itself wait forever.
        let connect = || {
            let client = Socket::new(Domain::UNIX, Type::STREAM, None).expect("socket");
            client.set_nonblocking(true).expect("nonblocking");
            (client.connect(&addr), client)
        };
        let _queued: Vec<_> = (0..4).map(|_| connect()).collect();
        let (full, _client) = connect();
        assert_eq!(
            full.map_err(|e| e.kind()),
            Err(std::io::ErrorKind::WouldBlock),
            "backlog not full"
        );

        let started = std::time::Instant::now();
        assert!(is_socket_listening(&socket));
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
