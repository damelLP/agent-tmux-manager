//! `process_confirmed` on plain rows: the restarted daemon's pending row and
//! the bound session it is, or is not (s3-d1).

use super::*;
use atm_core::{AgentType, Model, SessionDomain, SessionId};
use chrono::TimeZone;

const TPS: u64 = 100;

fn boot() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 21, 0, 0, 0).unwrap()
}

fn clock() -> Option<ProcessClock> {
    Some(ProcessClock {
        boot: boot(),
        ticks_per_second: TPS,
    })
}

/// Bound at boot + 1000 s.
fn bound_at() -> DateTime<Utc> {
    boot() + Duration::seconds(1000)
}

/// Ticks of a process that started `seconds` after boot.
fn ticks(seconds: u64) -> u64 {
    seconds * TPS
}

fn agent() -> Agent {
    Agent {
        id: "p/lead".parse().unwrap(),
        role: None,
        status: AgentStatus::Ready,
        pane: Some("%1".into()),
        session_id: Some("s1".into()),
        excluded_session_id: None,
        reason: None,
        updated_at: bound_at() + Duration::seconds(500),
        bound_at: Some(bound_at()),
    }
}

fn row(id: &str, pid: Option<u32>, start: Option<u64>) -> PaneSession {
    let mut session = SessionDomain::new(
        SessionId::new(id),
        AgentType::GeneralPurpose,
        Model::default(),
    );
    session.harness = Harness::ClaudeCode;
    session.tmux_pane = Some("%1".into());
    PaneSession {
        view: atm_core::SessionView::from_domain(&session),
        pid,
        process_start: start,
        start: None,
        harness_version: None,
    }
}

fn pending(pid: u32, start: u64) -> PaneSession {
    row(&format!("pending-{pid}"), Some(pid), Some(start))
}

fn record(pid: u32, start: u64) -> SessionProcess {
    SessionProcess {
        session_id: "s1".into(),
        pid,
        process_start: start,
    }
}

fn matched<'a>(
    agent: &Agent,
    recorded: Option<&SessionProcess>,
    rows: &'a [PaneSession],
    clock: Option<ProcessClock>,
) -> Option<&'a str> {
    process_confirmed(agent, recorded, Harness::ClaudeCode, rows, clock)
        .map(|row| row.view.id.as_str())
}

#[test]
fn pending_row_with_the_recorded_pid_and_start_is_the_bound_session() {
    let rows = [pending(41, ticks(900))];
    let recorded = record(41, ticks(900));
    assert_eq!(
        matched(&agent(), Some(&recorded), &rows, clock()),
        Some("pending-41")
    );
    // The record needs no clock, and holds for a process started after the
    // binding: a session resumed in a new process and recorded there.
    let late = [pending(77, ticks(4000))];
    assert_eq!(
        matched(&agent(), Some(&record(77, ticks(4000))), &late, None),
        Some("pending-77")
    );
}

#[test]
fn reused_pid_with_another_start_is_not() {
    let rows = [pending(41, ticks(900) + 1)];
    // Started before the binding, but a record exists and differs.
    assert_eq!(
        matched(&agent(), Some(&record(41, ticks(900))), &rows, clock()),
        None
    );
    // Two processes can start in the same tick: the pid must match too.
    let same_tick = [pending(42, ticks(900))];
    assert_eq!(
        matched(&agent(), Some(&record(41, ticks(900))), &same_tick, clock()),
        None
    );
}

#[test]
fn a_record_of_another_process_never_falls_back_to_time() {
    let rows = [pending(42, ticks(500))];
    assert_eq!(
        matched(&agent(), Some(&record(41, ticks(900))), &rows, clock()),
        None
    );
    // A record of another session is no record of this one's process.
    let other = SessionProcess {
        session_id: "s0".into(),
        ..record(42, ticks(500))
    };
    assert_eq!(matched(&agent(), Some(&other), &rows, clock()), None);
}

#[test]
fn without_a_record_a_process_started_before_the_binding_is_the_bound_session() {
    for start in [0, ticks(500), ticks(1000) - 1] {
        let rows = [pending(41, start)];
        assert_eq!(
            matched(&agent(), None, &rows, clock()),
            Some("pending-41"),
            "start tick {start}"
        );
    }
}

#[test]
fn without_a_record_a_process_started_at_or_after_the_binding_is_not() {
    for start in [ticks(1000), ticks(1000) + 1, ticks(5000)] {
        let rows = [pending(41, start)];
        assert_eq!(
            matched(&agent(), None, &rows, clock()),
            None,
            "start tick {start}"
        );
    }
    let rows = [pending(41, ticks(500))];
    // No clock, or no binding time: no rule without a record.
    assert_eq!(matched(&agent(), None, &rows, None), None);
    let unknown = Agent {
        bound_at: None,
        ..agent()
    };
    assert_eq!(matched(&unknown, None, &rows, clock()), None);
}

#[test]
fn two_pending_rows_a_real_row_or_another_harness_never_match() {
    let recorded = record(41, ticks(500));
    let ours = pending(41, ticks(500));
    let mut real = row("s1", Some(43), Some(ticks(600)));
    for rows in [
        vec![ours.clone(), pending(42, ticks(600))],
        vec![ours.clone(), real.clone()],
        vec![ours.clone(), row("s9", Some(43), Some(ticks(600)))],
    ] {
        assert_eq!(matched(&agent(), Some(&recorded), &rows, clock()), None);
        assert_eq!(matched(&agent(), None, &rows, clock()), None);
    }
    // A real row alone is not a discovery row: the harness has spoken, and
    // that row is whatever session it names, even in the recorded process.
    for id in ["s1", "s9"] {
        let rows = [row(id, Some(41), Some(ticks(500)))];
        assert_eq!(matched(&agent(), Some(&recorded), &rows, clock()), None);
        assert_eq!(matched(&agent(), None, &rows, clock()), None);
    }
    // Another harness's row, a child row, or a row on another pane is not
    // the agent's: neither alone nor as a second row.
    let mut pi = ours.clone();
    pi.view.harness = Harness::Pi.short_tag().into();
    let mut child = ours.clone();
    child.view.parent_session_id = Some(SessionId::new("s1"));
    let mut elsewhere = ours.clone();
    elsewhere.view.tmux_pane = Some("%2".into());
    for other in [pi, child, elsewhere] {
        assert_eq!(
            matched(
                &agent(),
                Some(&recorded),
                std::slice::from_ref(&other),
                clock()
            ),
            None
        );
        let mut beside = other.clone();
        beside.pid = Some(42);
        beside.view.id = SessionId::new("pending-42");
        assert_eq!(
            matched(&agent(), Some(&recorded), &[ours.clone(), beside], clock()),
            Some("pending-41")
        );
    }
    // A real row of another harness does not hide the agent's pending row.
    real.view.harness = Harness::Pi.short_tag().into();
    assert_eq!(
        matched(&agent(), Some(&recorded), &[ours, real], clock()),
        Some("pending-41")
    );
}

#[test]
fn unready_unbound_paneless_or_pidless_never_match() {
    let recorded = record(41, ticks(500));
    let rows = [pending(41, ticks(500))];
    for status in [
        AgentStatus::Starting,
        AgentStatus::Clearing,
        AgentStatus::Stuck,
        AgentStatus::Down,
    ] {
        let agent = Agent { status, ..agent() };
        assert_eq!(matched(&agent, Some(&recorded), &rows, clock()), None);
    }
    for session in [None, Some(SessionId::new("pending-41"))] {
        let agent = Agent {
            session_id: session,
            ..agent()
        };
        assert_eq!(matched(&agent, Some(&recorded), &rows, clock()), None);
    }
    let paneless = Agent {
        pane: None,
        ..agent()
    };
    assert_eq!(matched(&paneless, Some(&recorded), &rows, clock()), None);
    for (pid, start) in [(None, Some(ticks(500))), (Some(41), None)] {
        let rows = [row("pending-41", pid, start)];
        assert_eq!(matched(&agent(), Some(&recorded), &rows, clock()), None);
        assert_eq!(matched(&agent(), None, &rows, clock()), None);
    }
}

#[test]
fn process_clock_reads_ticks_exactly_and_refuses_what_it_cannot_read() {
    let clock = clock().unwrap();
    assert_eq!(clock.started(0), Some(boot()));
    assert_eq!(
        clock.started(ticks(1000) + 37),
        Some(boot() + Duration::seconds(1000) + Duration::milliseconds(370))
    );
    let zero = ProcessClock {
        ticks_per_second: 0,
        ..clock
    };
    assert_eq!(zero.started(5), None);
    assert_eq!(clock.started(u64::MAX), None);
}

/// I/O: the live clock places this test process's own start after boot and
/// before now.
#[test]
fn process_clock_reads_this_process_start_between_boot_and_now() {
    let before = Utc::now();
    let clock = ProcessClock::read().unwrap();
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
    let after_comm = stat.rsplit_once(')').unwrap().1;
    let ticks: u64 = after_comm
        .split_whitespace()
        .nth(19)
        .unwrap()
        .parse()
        .unwrap();
    let started = clock.started(ticks).unwrap();
    assert!(clock.boot < started, "{clock:?} {started}");
    assert!(
        started <= before + Duration::seconds(1),
        "{started} {before}"
    );
}
