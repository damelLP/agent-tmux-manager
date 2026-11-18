//! Private environment inputs. Live defaults stay inside atmd.

use std::path::PathBuf;
use std::time::Instant;

use chrono::{DateTime, Utc};

use atm_core::{Harness, SessionDomain, SessionId};

use super::infra::{read_process_start_time, SessionInfrastructure};

pub(super) trait Clock: Send + Sync {
    fn utc_now(&self) -> DateTime<Utc>;
    fn instant_now(&self) -> Instant;
}

/// Replace only the legacy constructor's clock sample, before insertion or
/// publication. Keep its domain initialization in core, not a second copy here.
pub(super) fn stamp_new_session(mut session: SessionDomain, clock: &dyn Clock) -> SessionDomain {
    let now = clock.utc_now();
    session.started_at = now;
    session.last_activity = now;
    session
}

pub(super) trait ProcessInfo: Send + Sync {
    fn start_time(&self, pid: u32) -> Option<u64>;
    fn alive(&self, pid: u32, expected_start_time: u64) -> bool;
    fn cwd(&self, pid: u32) -> Option<PathBuf>;
    fn parent(&self, pid: u32, harness: Harness) -> Option<SessionId>;
}

pub(super) struct SystemProcessInfo;

impl ProcessInfo for SystemProcessInfo {
    fn start_time(&self, pid: u32) -> Option<u64> {
        read_process_start_time(pid)
    }

    fn alive(&self, pid: u32, expected_start_time: u64) -> bool {
        SessionInfrastructure::process_alive(pid, expected_start_time)
    }

    fn cwd(&self, pid: u32) -> Option<PathBuf> {
        std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
    }

    fn parent(&self, pid: u32, harness: Harness) -> Option<SessionId> {
        crate::discovery::read_parent_session_id(pid, harness)
    }
}

pub(super) struct SystemClock;

impl Clock for SystemClock {
    fn utc_now(&self) -> DateTime<Utc> {
        Utc::now()
    }

    fn instant_now(&self) -> Instant {
        Instant::now()
    }
}
