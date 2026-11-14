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
