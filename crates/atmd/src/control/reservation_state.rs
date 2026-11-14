//! Pure, bounded ownership transitions. All clocks are supplied by the actor.
use std::{sync::Arc, time::Duration};
use tokio::time::Instant;

use super::{PaneId, ReservationError, ReservationKind, ReservationRequest, WaitPolicy};

pub(super) const CAPACITY: usize = 32;
pub(super) const CLEANUP_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct OperationId(u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Waiting,
    Work,
    Cleanup,
    Quarantined,
}

#[derive(Debug, Clone)]
pub(in crate::control) struct Fence {
    actor: Arc<()>,
    pub(super) operation: OperationId,
    phase: Phase,
}

#[derive(Debug)]
struct Entry {
    id: OperationId,
    request: ReservationRequest,
    phase: Phase,
    deadline: Instant,
}

#[derive(Debug, Default)]
pub(super) struct State {
    actor: Arc<()>,
    issued: u64,
    entries: Vec<Entry>,
}

pub(super) fn validate(request: &ReservationRequest, now: Instant) -> Result<(), ReservationError> {
    if request.deadline <= now {
        return Err(ReservationError::Timeout);
    }
    if request.deadline.duration_since(now) > Duration::from_secs(120) {
        return Err(ReservationError::InvalidRequest {
            reason: "work budget exceeds 120 seconds".into(),
        });
    }
    if request.pane.is_none()
        && !matches!(request.kind, ReservationKind::Up | ReservationKind::Respawn)
    {
        return Err(ReservationError::InvalidRequest {
            reason: "this operation requires an initial pane".into(),
        });
    }
    Ok(())
}

fn conflicts(a: &ReservationRequest, b: &ReservationRequest) -> bool {
    a.agent == b.agent
        || a.pane
            .as_ref()
            .is_some_and(|pane| b.pane.as_ref() == Some(pane))
}

impl State {
    pub(super) fn admit(
        &mut self,
        request: ReservationRequest,
        now: Instant,
    ) -> Result<OperationId, ReservationError> {
        validate(&request, now)?;
        if self
            .entries
            .iter()
            .any(|e| e.phase == Phase::Quarantined && conflicts(&e.request, &request))
        {
            return Err(ReservationError::Unavailable);
        }
        if request.wait == WaitPolicy::Refuse
            && self.entries.iter().any(|e| conflicts(&e.request, &request))
        {
            return Err(ReservationError::Busy {
                agent: request.agent,
                pane: request.pane,
            });
        }
        if self.entries.len() >= CAPACITY {
            return Err(ReservationError::Unavailable);
        }
        self.issued = self
            .issued
            .checked_add(1)
            .ok_or(ReservationError::Unavailable)?;
        let id = OperationId(self.issued);
        let deadline = request.deadline;
        self.entries.push(Entry {
            id,
            request,
            phase: Phase::Waiting,
            deadline,
        });
        Ok(id)
    }

    /// FIFO among conflicting waiters; independent panes never wait behind them.
    pub(super) fn ready(
        &mut self,
        now: Instant,
    ) -> Vec<(OperationId, Result<Fence, ReservationError>)> {
        let waiting: Vec<_> = self
            .entries
            .iter()
            .filter(|e| e.phase == Phase::Waiting)
            .map(|e| (e.id, e.request.clone()))
            .collect();
        let mut ready = Vec::new();
        for (id, request) in waiting {
            let blocked = self.entries.iter().any(|e| {
                e.id != id
                    && conflicts(&e.request, &request)
                    && (e.phase != Phase::Waiting || e.id.0 < id.0)
            });
            let quarantined = self
                .entries
                .iter()
                .any(|e| e.phase == Phase::Quarantined && conflicts(&e.request, &request));
            let error = if now >= request.deadline {
                Some(ReservationError::Timeout)
            } else if quarantined {
                Some(ReservationError::Unavailable)
            } else {
                None
            };
            if let Some(error) = error {
                self.entries.retain(|e| e.id != id);
                ready.push((id, Err(error)));
            } else if !blocked {
                if let Some(entry) = self.entries.iter_mut().find(|e| e.id == id) {
                    entry.phase = Phase::Work;
                }
                ready.push((
                    id,
                    Ok(Fence {
                        actor: self.actor.clone(),
                        operation: id,
                        phase: Phase::Work,
                    }),
                ));
            }
        }
        ready
    }

    pub(super) fn next_deadline(&self) -> Option<Instant> {
        self.entries
            .iter()
            .filter(|e| e.phase == Phase::Waiting)
            .map(|e| e.deadline)
            .min()
    }

    pub(super) fn stop_waiting(&mut self) {
        self.entries.retain(|e| e.phase != Phase::Waiting);
    }

    fn entry(&self, fence: &Fence) -> Result<&Entry, ReservationError> {
        if !Arc::ptr_eq(&self.actor, &fence.actor) {
            return Err(ReservationError::StaleReservation);
        }
        self.entries
            .iter()
            .find(|e| {
                e.id == fence.operation
                    && e.phase == fence.phase
                    && matches!(e.phase, Phase::Work | Phase::Cleanup)
            })
            .ok_or(ReservationError::StaleReservation)
    }

    pub(super) fn check(
        &self,
        fence: &Fence,
        pane: Option<&PaneId>,
        now: Instant,
    ) -> Result<(), ReservationError> {
        let entry = self.entry(fence)?;
        if pane.is_some_and(|pane| entry.request.pane.as_ref() != Some(pane)) {
            return Err(ReservationError::StaleReservation);
        }
        if now >= entry.deadline {
            return Err(ReservationError::Timeout);
        }
        Ok(())
    }

    /// Rotate even after the work deadline; expiry is not a reusable release.
    pub(super) fn begin_cleanup(
        &mut self,
        fence: &Fence,
        deadline: Instant,
    ) -> Result<Fence, ReservationError> {
        self.entry(fence)?;
        if fence.phase != Phase::Work {
            return Err(ReservationError::StaleReservation);
        }
        if let Some(entry) = self.entries.iter_mut().find(|e| e.id == fence.operation) {
            entry.phase = Phase::Cleanup;
            entry.deadline = deadline;
        }
        Ok(Fence {
            actor: self.actor.clone(),
            operation: fence.operation,
            phase: Phase::Cleanup,
        })
    }

    /// Called only when the owned supervisor has joined work and cleanup.
    pub(super) fn finish(&mut self, id: OperationId, reusable: bool) {
        if reusable {
            self.entries.retain(|e| e.id != id);
        } else if let Some(entry) = self.entries.iter_mut().find(|e| e.id == id) {
            entry.phase = Phase::Quarantined;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request(now: Instant) -> ReservationRequest {
        ReservationRequest {
            agent: "p/a".parse().unwrap(),
            pane: Some(PaneId::parse("%1").unwrap()),
            kind: ReservationKind::Clear,
            wait: WaitPolicy::Wait,
            deadline: now + Duration::from_secs(60),
        }
    }
    fn grant(state: &mut State, request: ReservationRequest, now: Instant) -> Fence {
        state.admit(request, now).unwrap();
        state.ready(now).pop().unwrap().1.unwrap()
    }

    #[test]
    fn reservation_state_rejects_foreign_actor_released_phase_and_unowned_pane() {
        let now = Instant::now();
        let mut a = State::default();
        let mut b = State::default();
        let old = grant(&mut a, request(now), now);
        let foreign = grant(&mut b, request(now), now);
        assert!(matches!(
            a.check(&foreign, None, now),
            Err(ReservationError::StaleReservation)
        ));
        assert!(matches!(
            a.check(&old, Some(&PaneId::parse("%2").unwrap()), now),
            Err(ReservationError::StaleReservation)
        ));
        let cleanup = a.begin_cleanup(&old, now + CLEANUP_TIMEOUT).unwrap();
        assert!(matches!(
            a.check(&old, None, now),
            Err(ReservationError::StaleReservation)
        ));
        a.check(&cleanup, None, now).unwrap();
        a.finish(old.operation, true);
        let _new = grant(&mut a, request(now), now);
        assert!(matches!(
            a.check(&cleanup, None, now),
            Err(ReservationError::StaleReservation)
        ));
    }

    #[test]
    fn reservation_state_new_pane_jobs_fence_the_previous_agent_generation() {
        let now = Instant::now();
        let mut state = State::default();
        let mut req = request(now);
        req.pane = None;
        req.kind = ReservationKind::Up;
        let old = grant(&mut state, req.clone(), now);
        let cleanup = state.begin_cleanup(&old, now + CLEANUP_TIMEOUT).unwrap();
        state.finish(old.operation, true);
        req.kind = ReservationKind::Respawn;
        let current = grant(&mut state, req, now);
        state.check(&current, None, now).unwrap();
        for stale in [old, cleanup] {
            assert!(matches!(
                state.check(&stale, None, now),
                Err(ReservationError::StaleReservation)
            ));
        }
    }

    #[test]
    fn reservation_state_epoch_exhaustion_never_wraps_and_quarantines_consume_slots() {
        let now = Instant::now();
        let mut state = State {
            issued: u64::MAX,
            ..State::default()
        };
        assert!(matches!(
            state.admit(request(now), now),
            Err(ReservationError::Unavailable)
        ));
        assert_eq!(state.issued, u64::MAX);
        let mut state = State::default();
        for n in 0..32 {
            let mut req = request(now);
            req.agent = format!("p/a{n}").parse().unwrap();
            req.pane = Some(PaneId::parse(format!("%{n}")).unwrap());
            let fence = grant(&mut state, req, now);
            state.finish(fence.operation, false);
        }
        let mut req = request(now);
        req.agent = "p/extra".parse().unwrap();
        req.pane = Some(PaneId::parse("%99").unwrap());
        assert!(matches!(
            state.admit(req, now),
            Err(ReservationError::Unavailable)
        ));
    }
}
