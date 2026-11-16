//! Pure, bounded ownership transitions. All clocks are supplied by the actor.
use std::{sync::Arc, time::Duration};
use tokio::time::Instant;

use super::{
    PaneId, Quarantine, ReservationError, ReservationKind, ReservationRequest, WaitPolicy,
};

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
    created: Option<PaneId>,
}

#[derive(Debug, Default)]
pub(super) struct State {
    actor: Arc<()>,
    issued: u64,
    entries: Vec<Entry>,
    quarantine_overflow: bool,
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
        && matches!(
            request.kind,
            ReservationKind::Clear | ReservationKind::Delivery | ReservationKind::Adopt
        )
    {
        return Err(ReservationError::InvalidRequest {
            reason: "this operation requires an initial pane".into(),
        });
    }
    Ok(())
}

fn conflicts(a: &Entry, b: &ReservationRequest) -> bool {
    a.request.agent == b.agent || b.pane.as_ref().is_some_and(|pane| owns(a, pane))
}
fn owns(entry: &Entry, pane: &PaneId) -> bool {
    entry.request.pane.as_ref() == Some(pane) || entry.created.as_ref() == Some(pane)
}
fn quarantined(entry: &Entry) -> ReservationError {
    ReservationError::Quarantined {
        agent: entry.request.agent.clone(),
        pane: entry.request.pane.clone().or_else(|| entry.created.clone()),
    }
}

pub(in crate::control) enum InputGate {
    Free,
    Held,
    Clearing(atm_org::AgentId),
    Quarantined(atm_org::AgentId),
    Unavailable,
}

impl State {
    pub(super) fn input_gate(&self, pane: &PaneId) -> InputGate {
        if self.quarantine_overflow {
            return InputGate::Unavailable;
        }
        let owners = || self.entries.iter().filter(|entry| owns(entry, pane));
        if let Some(entry) = owners().find(|entry| entry.phase == Phase::Quarantined) {
            return InputGate::Quarantined(entry.request.agent.clone());
        }
        if let Some(entry) = owners().find(|entry| {
            entry.request.kind == ReservationKind::Clear
                && matches!(entry.phase, Phase::Work | Phase::Cleanup)
        }) {
            return InputGate::Clearing(entry.request.agent.clone());
        }
        if owners().next().is_some() {
            InputGate::Held
        } else {
            InputGate::Free
        }
    }

    pub(super) fn admit(
        &mut self,
        request: ReservationRequest,
        now: Instant,
    ) -> Result<OperationId, ReservationError> {
        validate(&request, now)?;
        if let Some(entry) = self
            .entries
            .iter()
            .find(|e| e.phase == Phase::Quarantined && conflicts(e, &request))
        {
            return Err(quarantined(entry));
        }
        if self.quarantine_overflow {
            return Err(ReservationError::Unavailable);
        }
        // One parked job per overlapping kind prevents event storms on one pane
        // from consuming the shared pool. Consumers coalesce before admission.
        if self.entries.iter().any(|e| {
            conflicts(e, &request)
                && (request.wait == WaitPolicy::Refuse
                    || (e.phase == Phase::Waiting && e.request.kind == request.kind))
        }) {
            return Err(ReservationError::Busy {
                agent: request.agent,
                pane: request.pane,
            });
        }
        if self
            .entries
            .iter()
            .filter(|e| e.phase != Phase::Quarantined)
            .count()
            >= CAPACITY
        {
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
            created: None,
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
                e.id != id && conflicts(e, &request) && (e.phase != Phase::Waiting || e.id.0 < id.0)
            });
            let quarantine = self
                .entries
                .iter()
                .find(|e| e.phase == Phase::Quarantined && conflicts(e, &request));
            let error = if now >= request.deadline {
                Some(ReservationError::Timeout)
            } else if let Some(entry) = quarantine {
                Some(quarantined(entry))
            } else if self.quarantine_overflow {
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
        if self.quarantine_overflow {
            return Err(ReservationError::Unavailable);
        }
        if pane.is_some_and(|pane| !owns(entry, pane)) {
            return Err(ReservationError::StaleReservation);
        }
        if now >= entry.deadline {
            return Err(ReservationError::Timeout);
        }
        Ok(())
    }

    pub(super) fn deadline(&self, fence: &Fence) -> Result<Instant, ReservationError> {
        Ok(self.entry(fence)?.deadline)
    }
    pub(super) fn owner(
        &self,
        fence: &Fence,
    ) -> Result<(atm_org::AgentId, ReservationKind), ReservationError> {
        let entry = self.entry(fence)?;
        Ok((entry.request.agent.clone(), entry.request.kind))
    }
    pub(super) fn created(&self, fence: &Fence, pane: &PaneId) -> Result<bool, ReservationError> {
        Ok(self.entry(fence)?.created.as_ref() == Some(pane))
    }
    pub(super) fn can_split(&self, fence: &Fence) -> Result<(), ReservationError> {
        if self.entry(fence)?.created.is_some() {
            return Err(ReservationError::InvalidRequest {
                reason: "one new pane per reservation".into(),
            });
        }
        Ok(())
    }
    pub(super) fn claim(&mut self, fence: &Fence, pane: PaneId) -> Result<(), ReservationError> {
        self.can_split(fence)?;
        if self.entries.iter().any(|entry| owns(entry, &pane)) {
            // A server restart or faulty tmux result can reuse an ID. Do not
            // destroy a pane whose identity is uncertain; fail closed globally.
            self.quarantine_overflow = true;
            return Err(ReservationError::Unavailable);
        }
        if let Some(entry) = self.entries.iter_mut().find(|e| e.id == fence.operation) {
            entry.created = Some(pane);
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

    /// The quarantined operations, in the order they were admitted.
    pub(super) fn quarantines(&self) -> Vec<Quarantine> {
        self.entries
            .iter()
            .filter(|entry| entry.phase == Phase::Quarantined)
            .map(|entry| Quarantine {
                agent: entry.request.agent.clone(),
                pane: entry.created.clone().or_else(|| entry.request.pane.clone()),
                kind: entry.request.kind,
            })
            .collect()
    }

    /// Called only when the owned supervisor has joined work and cleanup.
    pub(super) fn finish(&mut self, id: OperationId, reusable: bool) {
        if reusable {
            self.entries.retain(|e| e.id != id);
        } else if self
            .entries
            .iter()
            .filter(|e| e.phase == Phase::Quarantined)
            .count()
            >= CAPACITY
        {
            // Keep tombstones bounded independently of the 32 live-job slots.
            // Overflow must never evict a fence and make uncertain input safe.
            self.quarantine_overflow = true;
            self.entries.retain(|e| e.id != id);
        } else if let Some(entry) = self.entries.iter_mut().find(|e| e.id == id) {
            entry.phase = Phase::Quarantined;
        }
    }
}

#[cfg(test)]
#[path = "reservation_policy_tests.rs"]
mod policy_tests;

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
    fn reservation_state_checks_fence_deadline_boundary() {
        let now = Instant::now();
        let mut state = State::default();
        let req = request(now);
        let deadline = req.deadline;
        let pane = req.pane.clone().unwrap();
        let fence = grant(&mut state, req, now);
        state
            .check(&fence, Some(&pane), deadline - Duration::from_nanos(1))
            .unwrap();
        for at in [deadline, deadline + Duration::from_nanos(1)] {
            assert!(matches!(
                state.check(&fence, Some(&pane), at),
                Err(ReservationError::Timeout)
            ));
        }
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
    fn reservation_state_epoch_exhaustion_and_quarantine_overflow_fail_closed() {
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
        let fence = grant(&mut state, req.clone(), now);
        state.finish(fence.operation, false);
        assert!(state.quarantine_overflow);
        assert_eq!(state.entries.len(), 32);
        assert!(matches!(
            state.admit(req, now),
            Err(ReservationError::Unavailable)
        ));
    }
}
