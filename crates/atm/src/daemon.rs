//! Daemon management for the ATM TUI.
//!
//! Provides utilities for checking if the daemon is running and
//! starting it automatically if needed.

use std::fs::File;
use std::io::Read;
use std::path::PathBuf;
use std::process::Command;
use std::thread;
use std::time::Duration;

use tracing::{debug, info};

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
    use sysinfo::{Pid, ProcessesToUpdate, System};
    let sys_pid = Pid::from_u32(pid);
    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::Some(&[sys_pid]), true);
    system.process(sys_pid).is_some()
}

/// Checks if the daemon is currently running.
pub fn is_daemon_running() -> bool {
    if let Some(pid) = read_pid() {
        is_process_running(pid)
    } else {
        false
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
    cmd.spawn()?;

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
}
