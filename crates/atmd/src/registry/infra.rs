use tracing::debug;

// ============================================================================
// Infrastructure Entity
// ============================================================================

/// Infrastructure-level data for a session.
///
/// Contains OS/system concerns that don't belong in the domain model.
/// Owned by RegistryActor alongside SessionDomain.
#[derive(Debug, Clone)]
pub(super) struct SessionInfrastructure {
    /// Process ID of the agent process (if known)
    pub(super) pid: Option<u32>,

    /// Process start time in clock ticks (from /proc/{pid}/stat field 22).
    /// Used to detect PID reuse - if the start time changes, it's a different process.
    process_start_time: Option<u64>,
}

impl SessionInfrastructure {
    /// Creates new SessionInfrastructure.
    pub(super) fn new() -> Self {
        Self {
            pid: None,
            process_start_time: None,
        }
    }

    /// Sets the process ID and captures the process start time for PID reuse detection.
    ///
    /// The start time is read from `/proc/{pid}/stat` field 22 (starttime in clock ticks).
    /// If the PID is already set with the same value, this is a no-op.
    ///
    /// # Validation
    ///
    /// The PID is only stored if:
    /// - It's non-zero (PID 0 is invalid)
    /// - We can successfully read its start time from `/proc/{pid}/stat`
    ///
    /// This prevents storing invalid PIDs that would cause incorrect liveness checks.
    pub(super) fn set_pid(&mut self, pid: u32) {
        // PID 0 is invalid
        if pid == 0 {
            return;
        }

        // Only update if PID changed or wasn't set
        if self.pid == Some(pid) {
            return;
        }

        // Only store PID if we can read and validate its start time
        // This ensures the PID is valid and gives us PID reuse protection
        if let Some(start_time) = read_process_start_time(pid) {
            self.pid = Some(pid);
            self.process_start_time = Some(start_time);
        } else {
            debug!(
                pid = pid,
                "PID validation failed - process may have exited or is inaccessible"
            );
        }
    }

    /// Checks if the tracked process is still alive.
    ///
    /// Returns `true` if:
    /// - No PID is tracked (can't determine liveness)
    /// - The process exists and has the same start time
    ///
    /// Returns `false` if:
    /// - The process no longer exists
    /// - The PID has been reused by a different process (start time mismatch)
    pub(super) fn is_process_alive(&self) -> bool {
        let Some(pid) = self.pid else {
            // No PID tracked - assume alive (can't determine)
            debug!(pid = ?self.pid, "is_process_alive: no PID tracked, assuming alive");
            return true;
        };

        let Some(expected_start_time) = self.process_start_time else {
            // No start time recorded - just check if process exists via procfs
            let exists = procfs::process::Process::new(pid as i32).is_ok();
            debug!(
                pid,
                exists, "is_process_alive: no start_time, checking procfs only"
            );
            return exists;
        };

        // Check if process exists and has same start time
        match read_process_start_time(pid) {
            Some(current_start_time) => {
                let alive = current_start_time == expected_start_time;
                if !alive {
                    debug!(
                        pid,
                        expected_start_time,
                        current_start_time,
                        "is_process_alive: start time MISMATCH - PID reused?"
                    );
                }
                alive
            }
            None => {
                debug!(
                    pid,
                    expected_start_time, "is_process_alive: process NOT FOUND in /proc"
                );
                false
            }
        }
    }
}

/// Reads the process start time using the procfs crate.
///
/// The start time (in clock ticks since boot) is stable for the lifetime
/// of a process and unique enough to detect PID reuse.
///
/// Returns `None` if the process doesn't exist or can't be read.
fn read_process_start_time(pid: u32) -> Option<u64> {
    let process = procfs::process::Process::new(pid as i32).ok()?;
    let stat = process.stat().ok()?;
    Some(stat.starttime)
}

impl Default for SessionInfrastructure {
    fn default() -> Self {
        Self::new()
    }
}
