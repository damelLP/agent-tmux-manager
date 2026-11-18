use tracing::debug;

use super::commands::AcceptedStart;
use super::inputs::ProcessInfo;
#[cfg(test)]
use super::inputs::SystemProcessInfo;

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

    /// The latest untagged `SessionStart` applied to this row. It lives and
    /// dies with the row, and is not part of the session's view.
    pub(super) start: Option<AcceptedStart>,
}

impl SessionInfrastructure {
    /// Creates new SessionInfrastructure.
    pub(super) fn new() -> Self {
        Self {
            pid: None,
            process_start_time: None,
            start: None,
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
    #[cfg(test)]
    pub(super) fn set_pid(&mut self, pid: u32) {
        self.set_pid_with(pid, &SystemProcessInfo);
    }

    pub(super) fn set_pid_with(&mut self, pid: u32, process: &dyn ProcessInfo) {
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
        if let Some(start_time) = process.start_time(pid) {
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
    /// - The process has exited and its parent has not waited for it yet
    /// - The PID has been reused by a different process (start time mismatch)
    #[cfg(test)]
    pub(super) fn is_process_alive(&self) -> bool {
        self.is_process_alive_with(&SystemProcessInfo)
    }

    pub(super) fn is_process_alive_with(&self, process: &dyn ProcessInfo) -> bool {
        let Some(pid) = self.pid else {
            // No PID tracked - assume alive (can't determine)
            debug!(pid = ?self.pid, "is_process_alive: no PID tracked, assuming alive");
            return true;
        };

        let Some(expected_start_time) = self.process_start_time else {
            // `set_pid` only stores a PID together with its start time.
            return false;
        };

        process.alive(pid, expected_start_time)
    }

    #[cfg(test)]
    pub(super) fn replay_state(&self) -> serde_json::Value {
        serde_json::json!({"pid": self.pid, "start_time": self.process_start_time,
            "accepted_start": self.start.as_ref().map(|start| serde_json::json!({"source": start.source}))})
    }

    pub(super) fn process_alive(pid: u32, expected_start_time: u64) -> bool {
        // Check if process exists, is still running and has same start time
        match read_process_stat(pid) {
            // An exited process keeps its /proc entry, start time included,
            // until its parent waits for it. Where nothing reaps an orphan (a
            // container whose PID 1 is not an init) that is for ever.
            Some(stat) if has_exited(&stat) => {
                debug!(
                    pid,
                    expected_start_time, "is_process_alive: process has EXITED, not yet reaped"
                );
                false
            }
            Some(stat) => {
                let current_start_time = stat.starttime;
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
pub(super) fn read_process_start_time(pid: u32) -> Option<u64> {
    read_process_stat(pid).map(|stat| stat.starttime)
}

/// Reads `/proc/{pid}/stat`. `None` if the process doesn't exist or can't be read.
fn read_process_stat(pid: u32) -> Option<procfs::process::Stat> {
    procfs::process::Process::new(pid as i32).ok()?.stat().ok()
}

/// True if the whole process has exited and only its `/proc` entry is left.
///
/// The leader is `Z` as soon as it exits, while the group's other threads may
/// still be exiting: only one thread left is all of it. The daemon's own
/// PID-file check reads `/proc/{pid}/stat` the same way.
fn has_exited(stat: &procfs::process::Stat) -> bool {
    matches!(stat.state, 'X' | 'x') || (stat.state == 'Z' && stat.num_threads == 1)
}

impl Default for SessionInfrastructure {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod observation_tests {
    use super::*;

    // Synthetic: state/thread mutations of the test process's own stat. Catches
    // treating a multi-thread exiting leader as a wholly exited process.
    #[test]
    fn exited_state_classification_is_preserved() {
        let mut stat = read_process_stat(std::process::id()).unwrap();
        for (state, threads, exited) in [
            ('R', 1, false),
            ('X', 1, true),
            ('x', 2, true),
            ('Z', 1, true),
            ('Z', 2, false),
        ] {
            stat.state = state;
            stat.num_threads = threads;
            assert_eq!(has_exited(&stat), exited, "{state}/{threads}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Child, Command, Stdio};

    /// A process that runs until the returned handle's stdin is closed.
    fn running() -> Child {
        Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap()
    }

    /// Ends `child` and returns once it has exited, leaving it unreaped: its
    /// `/proc` entry stands, as an orphan's does where nothing reaps it.
    fn exit_unreaped(child: &mut Child) {
        drop(child.stdin.take());
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let waited = unsafe {
            libc::waitid(
                libc::P_PID,
                child.id(),
                &mut info,
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        assert_eq!(waited, 0, "{}", std::io::Error::last_os_error());
        let stat = read_process_stat(child.id()).unwrap();
        assert_eq!((stat.state, stat.num_threads), ('Z', 1));
    }

    #[test]
    fn pid_and_start_time_stay_paired() {
        let mut infra = SessionInfrastructure::new();
        assert_eq!((infra.pid, infra.process_start_time), (None, None));
        assert!(infra.is_process_alive());
        for pid in [0, u32::MAX] {
            infra.set_pid(pid);
            assert_eq!((infra.pid, infra.process_start_time), (None, None));
            assert!(infra.is_process_alive());
        }

        let pid = std::process::id();
        let start_time = read_process_start_time(pid).unwrap();
        infra.set_pid(pid);
        assert_eq!(
            (infra.pid, infra.process_start_time),
            (Some(pid), Some(start_time))
        );
        for rejected_or_unchanged in [0, pid, u32::MAX] {
            infra.set_pid(rejected_or_unchanged);
            assert_eq!(
                (infra.pid, infra.process_start_time),
                (Some(pid), Some(start_time))
            );
            assert!(infra.is_process_alive());
        }

        let mut child = running();
        let child_pid = child.id();
        let child_start_time = read_process_start_time(child_pid).unwrap();
        infra.set_pid(child_pid);
        let pair = (infra.pid, infra.process_start_time);
        drop(child.stdin.take());
        child.wait().unwrap();
        assert_eq!(pair, (Some(child_pid), Some(child_start_time)));
    }

    #[test]
    fn a_reused_pid_is_not_alive() {
        let mut infra = SessionInfrastructure::new();
        infra.set_pid(std::process::id());
        assert!(infra.is_process_alive());
        // Model a different process at the same PID without waiting for OS reuse.
        let start_time = infra.process_start_time.unwrap();
        infra.process_start_time = Some(start_time.wrapping_add(1));
        assert!(!infra.is_process_alive());
    }

    #[test]
    fn a_running_process_is_alive() {
        let mut child = running();
        let mut infra = SessionInfrastructure::new();
        infra.set_pid(child.id());
        assert_eq!(infra.pid, Some(child.id()));
        assert!(infra.is_process_alive());
        drop(child.stdin.take());
        child.wait().unwrap();
        // Reaped: no /proc entry is left.
        assert!(!infra.is_process_alive());
    }

    #[test]
    fn an_exited_unreaped_process_is_not_alive() {
        let mut child = running();
        let mut infra = SessionInfrastructure::new();
        infra.set_pid(child.id());
        assert!(infra.is_process_alive());
        exit_unreaped(&mut child);
        assert!(
            !infra.is_process_alive(),
            "an exited, unreaped process is reported alive"
        );
        child.wait().unwrap();
    }

    /// A registration can name a process that has already exited. Its pid is
    /// stored all the same: without one the session would be assumed alive
    /// for ever. The liveness check then finds it exited.
    #[test]
    fn set_pid_stores_an_exited_unreaped_process_and_it_is_not_alive() {
        let mut child = running();
        exit_unreaped(&mut child);
        let mut infra = SessionInfrastructure::new();
        infra.set_pid(child.id());
        assert_eq!(infra.pid, Some(child.id()));
        assert!(!infra.is_process_alive());
        child.wait().unwrap();
    }
}
