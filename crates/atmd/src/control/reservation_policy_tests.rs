//! Pure policy regressions: explicit time, no runtime waits.
use super::*;
fn request(
    agent: &str,
    pane: Option<&str>,
    kind: ReservationKind,
    now: Instant,
) -> ReservationRequest {
    ReservationRequest::new(
        agent.parse().unwrap(),
        pane.map(|p| PaneId::parse(p).unwrap()),
        kind,
        WaitPolicy::Wait,
        now + Duration::from_secs(60),
    )
}
#[test]
fn reservation_agent_only_down_observe_and_required_panes() {
    let now = Instant::now();
    for kind in [
        ReservationKind::Up,
        ReservationKind::Respawn,
        ReservationKind::Down,
        ReservationKind::Observe,
    ] {
        validate(&request("p/a", None, kind, now), now).unwrap();
    }
    for kind in [
        ReservationKind::Clear,
        ReservationKind::Delivery,
        ReservationKind::Adopt,
    ] {
        assert!(matches!(
            validate(&request("p/a", None, kind, now), now),
            Err(ReservationError::InvalidRequest { .. })
        ));
    }
}
#[test]
fn reservation_waiters_fifo_atomic_and_one_kind_cannot_fill_pool() {
    let now = Instant::now();
    let mut state = State::default();
    let active = state
        .admit(
            request("p/a", Some("%1"), ReservationKind::Delivery, now),
            now,
        )
        .unwrap();
    assert_eq!(state.ready(now).len(), 1);
    let first = state
        .admit(request("p/a", Some("%2"), ReservationKind::Clear, now), now)
        .unwrap();
    let second = state
        .admit(
            request("p/a", Some("%1"), ReservationKind::Delivery, now),
            now,
        )
        .unwrap();
    assert!(matches!(
        state.admit(
            request("p/a", Some("%1"), ReservationKind::Delivery, now),
            now
        ),
        Err(ReservationError::Busy { .. })
    ));
    // A waiter reserves neither key partially; independent ownership can proceed.
    let independent = state
        .admit(request("p/b", Some("%3"), ReservationKind::Down, now), now)
        .unwrap();
    let ready = state.ready(now);
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].0, independent);
    state.finish(active, true);
    let ready = state.ready(now);
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].0, first);
    assert!(state.ready(now).is_empty());
    state.finish(first, true);
    let ready = state.ready(now);
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].0, second);
}
#[test]
fn reservation_lifecycle_contention_holds_agent_and_pane_atomically() {
    let now = Instant::now();
    let mut state = State::default();
    let active = state
        .admit(request("p/a", Some("%1"), ReservationKind::Clear, now), now)
        .unwrap();
    state.ready(now);
    let waiting = state
        .admit(
            request("p/a", Some("%2"), ReservationKind::Respawn, now),
            now,
        )
        .unwrap();
    assert!(state.ready(now).is_empty());
    assert_eq!(
        state
            .entries
            .iter()
            .find(|entry| entry.id == waiting)
            .unwrap()
            .phase,
        Phase::Waiting
    );
    // The held agent blocks replacement even when the requested pane differs.
    state.finish(active, true);
    let ready = state.ready(now);
    assert_eq!(ready.len(), 1);
    let fence = ready[0].1.as_ref().unwrap();
    state
        .check(fence, Some(&PaneId::parse("%2").unwrap()), now)
        .unwrap();
    assert!(matches!(
        state.check(fence, Some(&PaneId::parse("%1").unwrap()), now),
        Err(ReservationError::StaleReservation)
    ));
}

#[test]
fn reservation_quarantines_lists_failed_cleanups_only() {
    // The read names each operation whose cleanup failed, with the pane it
    // reserved or else created, in admission order. A released operation
    // and a running one are not in it, and reading changes nothing.
    let now = Instant::now();
    let mut state = State::default();
    assert_eq!(state.quarantines(), Vec::new());
    let run = |state: &mut State, agent: &str, pane: Option<&str>, kind| {
        let id = state.admit(request(agent, pane, kind, now), now).unwrap();
        let fence = state.ready(now).pop().unwrap().1.unwrap();
        (id, fence)
    };
    let (bad, _) = run(&mut state, "p/bad", Some("%100"), ReservationKind::Clear);
    state.finish(bad, false);
    let (made, fence) = run(&mut state, "p/made", None, ReservationKind::Up);
    state.claim(&fence, PaneId::parse("%7").unwrap()).unwrap();
    state.finish(made, false);
    let (gone, _) = run(&mut state, "p/gone", None, ReservationKind::Down);
    state.finish(gone, false);
    let (ok, _) = run(&mut state, "p/ok", Some("%2"), ReservationKind::Observe);
    state.finish(ok, true);
    run(&mut state, "p/live", Some("%3"), ReservationKind::Delivery);
    let quarantine = |agent: &str, pane: Option<&str>, kind| Quarantine {
        agent: agent.parse().unwrap(),
        pane: pane.map(|pane| PaneId::parse(pane).unwrap()),
        kind,
    };
    let expected = vec![
        quarantine("p/bad", Some("%100"), ReservationKind::Clear),
        quarantine("p/made", Some("%7"), ReservationKind::Up),
        quarantine("p/gone", None, ReservationKind::Down),
    ];
    assert_eq!(state.quarantines(), expected);
    assert_eq!(state.quarantines(), expected);
    assert!(matches!(
        state.admit(request("p/bad", None, ReservationKind::Down, now), now),
        Err(ReservationError::Quarantined { .. })
    ));
}

#[test]
fn reservation_quarantine_does_not_take_live_pool_capacity() {
    let now = Instant::now();
    let mut state = State::default();
    let id = state
        .admit(
            request("p/bad", Some("%100"), ReservationKind::Clear, now),
            now,
        )
        .unwrap();
    state.ready(now);
    state.finish(id, false);
    for n in 0..32 {
        state
            .admit(
                request(
                    &format!("p/a{n}"),
                    Some(&format!("%{n}")),
                    ReservationKind::Observe,
                    now,
                ),
                now,
            )
            .unwrap();
    }
    assert!(matches!(
        state.admit(request("p/full", None, ReservationKind::Down, now), now),
        Err(ReservationError::Unavailable)
    ));
    assert!(matches!(
        state.admit(request("p/bad", None, ReservationKind::Down, now), now),
        Err(ReservationError::Quarantined { .. })
    ));
}
