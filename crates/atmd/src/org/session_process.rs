//! After a daemon restart every harness process is a `pending-<pid>`
//! discovery row until its harness's next event, so an idle seat's recorded
//! session is never confirmed and its mail waits or is refused (s3-d1). This
//! ties such a row to the bound session it is.
//!
//! The row is the recorded session when it is the only row of the agent's
//! harness in the agent's own pane, and its process is the session's:
//! - **recorded:** its pid and start ticks equal the session's latest
//!   `session.process`, so a reused pid never matches;
//! - **not recorded** (a binding made before atmd recorded processes, or one
//!   whose row was never confirmed with one): it started before the binding
//!   was recorded. While atmd runs, any new harness process in the pane
//!   announces a session and is bound, so an older process is the bound one.
//!   A record that does not match is never overruled by the time rule.
//!
//! The agents observer writes the record when a pass finds the bound
//! session's own row with a process other than the recorded one: when it
//! binds a session, and when the first harness event after a restart turns
//! the discovery row into the session's row (the row's removal wakes the
//! agent). A session resumed in a new process while atmd runs reports only
//! its own events, which start no pass for a Ready seat, so it is recorded at
//! the first event after the next restart; until then the old record leaves
//! its discovery row unmatched, as before.
//!
//! Known limits: a new session in the same process while atmd was down (a
//! hand `/clear`) matches; the delivery's paste then lands in the agent's own
//! pane and the real session's prompt-submit, which is not the attempted one,
//! holds the message instead of confirming it. Without a record, an older
//! harness process of the same harness in the same pane that is not the bound
//! one (a resumed job after the bound one exited) matches the same way.
//! Clock steps since the binding shift the time rule by the step.
//!
//! A matched harness that never reports to this daemon (atm's hooks removed
//! or broken while atmd was down, a daemon on another socket, or a wrong
//! match onto a process without hooks) gets attempt 1 pasted with nothing to
//! confirm it; its retries wait for the harness (`process_matched` on the
//! recipient), so it is never typed twice, and the message stays queued.
//!
//! Reboot: a record is pid and start ticks, with no boot identity. After a
//! reboot a match needs the same pid at the same tick (not seen; reasoned),
//! the time rule refuses a binding recorded before the boot, and the new
//! tmux server's panes carry no `@atm-agent` tag, so the observer records
//! the agent Down and delivery, which checks the tag, pastes nothing.

use super::agents::matches_agent;
use crate::registry::PaneSession;
use atm_core::Harness;
use atm_org::{Agent, AgentStatus, SessionProcess};
use chrono::{DateTime, Duration, Utc};

/// The wall-clock reading of a process start given in ticks since boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ProcessClock {
    pub boot: DateTime<Utc>,
    pub ticks_per_second: u64,
}

impl ProcessClock {
    /// Boot as the wall clock reads it now. `/proc/uptime` counts suspend, as
    /// process start ticks do. A procfs read resolves no user path and cannot
    /// hang, like the registry's inline `/proc/<pid>/stat` reads.
    pub(super) fn read() -> Option<Self> {
        use procfs::Current;
        let uptime = procfs::Uptime::current().ok()?.uptime_duration();
        Some(Self {
            boot: Utc::now().checked_sub_signed(Duration::from_std(uptime).ok()?)?,
            ticks_per_second: procfs::ticks_per_second(),
        })
    }

    /// When a process that started `ticks` after boot started.
    pub(super) fn started(&self, ticks: u64) -> Option<DateTime<Utc>> {
        let per = self.ticks_per_second;
        let seconds = i64::try_from(ticks.checked_div(per)?).ok()?;
        let nanos = i64::try_from((ticks % per).checked_mul(1_000_000_000)? / per).ok()?;
        self.boot
            .checked_add_signed(Duration::try_seconds(seconds)?)?
            .checked_add_signed(Duration::nanoseconds(nanos))
    }
}

/// The pane's one discovery row that is `agent`'s recorded session. `recorded`
/// is that session's latest `session.process`; `harness` the agent's; `clock`
/// reads start ticks for the rule without a record (`None`: no such rule).
pub(super) fn process_confirmed<'a>(
    agent: &Agent,
    recorded: Option<&SessionProcess>,
    harness: Harness,
    rows: &'a [PaneSession],
    clock: Option<ProcessClock>,
) -> Option<&'a PaneSession> {
    let session = agent.session_id.as_ref().filter(|id| !id.is_pending())?;
    if agent.status != AgentStatus::Ready {
        return None;
    }
    let mut own = rows
        .iter()
        .filter(|row| matches_agent(agent, &row.view) && row.view.harness == harness.short_tag());
    if own.clone().any(|row| !row.view.id.is_pending()) {
        return None;
    }
    let row = own.next()?;
    if own.next().is_some() {
        return None;
    }
    let (pid, start) = row.pid.zip(row.process_start)?;
    let confirmed = match recorded {
        Some(process) => {
            process.session_id == *session && process.pid == pid && process.process_start == start
        }
        None => agent
            .bound_at
            .zip(clock.and_then(|clock| clock.started(start)))
            .is_some_and(|(bound, started)| started < bound),
    };
    confirmed.then_some(row)
}

#[cfg(test)]
#[path = "session_process_tests.rs"]
mod tests;
