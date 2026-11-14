//! Pure replay accounting, without copying the daemon's log envelope type.

use crate::{OrgError, OrgEvent, OrgState};
use chrono::{DateTime, Utc};
use serde_json::Value;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReplayStats {
    pub applied: u64,
    pub skipped_unknown: u64,
}
impl ReplayStats {
    /// Known corrupt bodies or inapplicable facts fail without changing either
    /// state or counters. Unknown kinds are skipped regardless of body shape.
    pub fn apply_parts(
        &mut self,
        state: &mut OrgState,
        seq: u64,
        ts: DateTime<Utc>,
        kind: &str,
        body: &Value,
    ) -> Result<(), OrgError> {
        if let Some(event) = OrgEvent::from_parts(kind, body)? {
            let count = self
                .applied
                .checked_add(1)
                .ok_or(OrgError::CounterOverflow)?;
            state.apply(seq, ts, &event)?;
            self.applied = count;
        } else {
            self.skipped_unknown = self
                .skipped_unknown
                .checked_add(1)
                .ok_or(OrgError::CounterOverflow)?;
        }
        Ok(())
    }
}
