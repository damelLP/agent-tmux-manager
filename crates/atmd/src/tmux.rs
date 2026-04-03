//! Tmux integration for session discovery.
//!
//! Provides utilities for finding tmux pane IDs for Claude processes.
//! All functions in this module perform blocking I/O and should be
//! called via `spawn_blocking`.

use std::collections::HashMap;
use std::fs;
use std::process::Command;

use tracing::{debug, trace};

/// Finds the tmux pane ID for a given process ID.
///
/// This function:
/// 1. Lists all tmux panes with their shell PIDs
/// 2. Walks the process tree from the target PID upward
/// 3. Returns the pane ID if any ancestor matches a pane's shell PID
///
/// # Arguments
/// * `pid` - The process ID to find the pane for (e.g., Claude's PID)
///
/// # Returns
/// * `Some(pane_id)` - The tmux pane ID (e.g., "%5") if found
/// * `None` - If not running in tmux or pane not found
///
/// # Note
/// This function performs blocking I/O and should be called via `spawn_blocking`.
pub fn find_pane_for_pid(pid: u32) -> Option<String> {
    // Get all tmux panes and their shell PIDs
    let pane_pids = list_tmux_panes()?;

    if pane_pids.is_empty() {
        debug!("No tmux panes found");
        return None;
    }

    trace!(pane_count = pane_pids.len(), "Found tmux panes");

    // Walk up the process tree from the target PID
    let mut current_pid = pid;
    let mut depth = 0;
    const MAX_DEPTH: u32 = 20; // Prevent infinite loops

    while depth < MAX_DEPTH {
        // Check if current PID matches any pane's shell PID
        if let Some(pane_id) = pane_pids.get(&current_pid) {
            debug!(pid, pane_id, depth, "Found tmux pane for process");
            return Some(pane_id.clone());
        }

        // Get parent PID
        match get_parent_pid(current_pid) {
            Some(ppid) if ppid > 1 => {
                current_pid = ppid;
                depth += 1;
            }
            _ => {
                // Reached init (PID 1) or couldn't read parent
                break;
            }
        }
    }

    debug!(pid, "No tmux pane found for process");
    None
}

/// Lists all tmux panes and their shell PIDs.
///
/// Runs `tmux list-panes -a -F "#{pane_id} #{pane_pid}"` and parses the output.
///
/// # Returns
/// * `Some(HashMap<u32, String>)` - Map of shell PID to pane ID
/// * `None` - If tmux is not running or command fails
fn list_tmux_panes() -> Option<HashMap<u32, String>> {
    let output = Command::new("tmux")
        .args(["list-panes", "-a", "-F", "#{pane_id} #{pane_pid}"])
        .output()
        .ok()?;

    if !output.status.success() {
        // tmux not running or no server
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut pane_pids = HashMap::new();

    for line in stdout.lines() {
        let mut parts = line.split_whitespace();
        if let (Some(pane_id), Some(pane_pid_str)) = (parts.next(), parts.next()) {
            if let Ok(pane_pid) = pane_pid_str.parse::<u32>() {
                pane_pids.insert(pane_pid, pane_id.to_string());
            }
        }
    }

    Some(pane_pids)
}

/// Gets the parent PID of a process.
///
/// Reads `/proc/{pid}/stat` and extracts the parent PID (field 4).
///
/// # Arguments
/// * `pid` - The process ID to get the parent of
///
/// # Returns
/// * `Some(ppid)` - The parent process ID
/// * `None` - If the process doesn't exist or can't be read
pub(crate) fn get_parent_pid(pid: u32) -> Option<u32> {
    let stat_path = format!("/proc/{pid}/stat");
    let stat_content = fs::read_to_string(&stat_path).ok()?;

    // Format: pid (comm) state ppid ...
    // The comm field can contain spaces and parentheses, so find the last ')'
    let close_paren = stat_content.rfind(')')?;
    let after_comm = stat_content.get(close_paren + 1..)?;

    // Fields after (comm): state ppid ...
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    // fields[0] = state, fields[1] = ppid
    fields.get(1)?.parse().ok()
}

/// Returns the start time (in clock ticks since boot) for a process.
///
/// Reads field 22 (`starttime`) from `/proc/{pid}/stat`.
/// Used to detect PID reuse: if a PID's start time changes,
/// the original process has exited and the PID was reassigned.
pub(crate) fn get_process_start_time(pid: u32) -> Option<u64> {
    let stat_path = format!("/proc/{pid}/stat");
    let stat_content = fs::read_to_string(&stat_path).ok()?;

    let close_paren = stat_content.rfind(')')?;
    let after_comm = stat_content.get(close_paren + 1..)?;

    // Fields after (comm): state(0) ppid(1) pgrp(2) session(3) tty_nr(4) tpgid(5)
    // flags(6) minflt(7) cminflt(8) majflt(9) cmajflt(10) utime(11) stime(12)
    // cutime(13) cstime(14) priority(15) nice(16) num_threads(17) itrealvalue(18)
    // starttime(19)
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    fields.get(19)?.parse().ok()
}

/// Checks if tmux is available on the system.
///
/// Simply checks if the `tmux` command exists and is executable.
pub fn is_tmux_available() -> bool {
    Command::new("tmux")
        .arg("-V")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_parent_pid_self() {
        // Current process should have a valid parent
        let pid = std::process::id();
        let ppid = get_parent_pid(pid);
        assert!(ppid.is_some());
        assert!(ppid.unwrap() > 0);
    }

    #[test]
    fn test_get_parent_pid_nonexistent() {
        // PID that almost certainly doesn't exist
        let ppid = get_parent_pid(999_999_999);
        assert!(ppid.is_none());
    }

    #[test]
    fn test_is_tmux_available() {
        // Just verify it doesn't panic - result depends on system
        let _ = is_tmux_available();
    }
}
