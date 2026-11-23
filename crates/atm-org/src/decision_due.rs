//! When a decision's deadline expires it (design 12, owner Q2): the queries the
//! decision bridge's timer runs on. The test is the `Expire` policy's: pending,
//! with a deadline at or before `now`.

use crate::{DecisionId, DecisionStatus, OrgState};
use chrono::{DateTime, Utc};

/// Pending decisions whose deadline is at or before `now`, in id order.
#[must_use]
pub fn expiring(state: &OrgState, now: DateTime<Utc>) -> Vec<DecisionId> {
    state
        .decisions()
        .filter(|d| d.status == DecisionStatus::Pending)
        .filter(|d| d.request.deadline.is_some_and(|deadline| deadline <= now))
        .map(|d| d.id)
        .collect()
}

/// The earliest deadline of a pending decision after `now`. One at or before
/// `now` was `expiring` in the same pass: if it is still pending its expiry
/// was refused, and arming the timer on it again would spin.
#[must_use]
pub fn next_deadline(state: &OrgState, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    state
        .decisions()
        .filter(|d| d.status == DecisionStatus::Pending)
        .filter_map(|d| d.request.deadline)
        .filter(|deadline| *deadline > now)
        .min()
}
