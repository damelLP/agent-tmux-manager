//! Subscription meter readings shared by adapters, the daemon and its clients.

use crate::Harness;
use chrono::{DateTime, Datelike, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;

/// A local alias, not a provider account identifier.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct PlanProfileId(String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("expected a 1–32 character lowercase ASCII alias starting with a letter or digit; '_' and '-' are allowed after the first character")]
pub struct InvalidPlanProfileId;

impl PlanProfileId {
    pub fn new(alias: impl Into<String>) -> Result<Self, InvalidPlanProfileId> {
        Self::try_from(alias.into())
    }
}

impl TryFrom<String> for PlanProfileId {
    type Error = InvalidPlanProfileId;

    fn try_from(alias: String) -> Result<Self, Self::Error> {
        let alphanumeric = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
        let mut bytes = alias.bytes();
        if alias.len() <= 32
            && bytes.next().is_some_and(alphanumeric)
            && bytes.all(|b| alphanumeric(b) || matches!(b, b'_' | b'-'))
        {
            Ok(Self(alias))
        } else {
            Err(InvalidPlanProfileId)
        }
    }
}

impl fmt::Display for PlanProfileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanUsageWindow {
    /// None for the whole meter.
    pub scope: Option<String>,
    pub duration_minutes: u32,
    pub used_percent: f64,
    pub resets_at: Option<DateTime<Utc>>,
}

impl PlanUsageWindow {
    /// Build a whole-meter window from an adapter's optional epoch seconds.
    #[must_use]
    pub fn from_epoch_seconds(
        duration_minutes: u32,
        used_percent: f64,
        resets_at: Option<i64>,
    ) -> Option<Self> {
        let window = Self {
            scope: None,
            duration_minutes,
            used_percent,
            resets_at: match resets_at {
                Some(seconds) => Some(DateTime::from_timestamp(seconds, 0)?),
                None => None,
            },
        };
        window.is_valid().then_some(window)
    }

    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.duration_minutes > 0
            && (0.0..=100.0).contains(&self.used_percent)
            && self
                .resets_at
                .is_none_or(|at| (0..=9999).contains(&at.year()))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanUsageSnapshot {
    pub profile: PlanProfileId,
    pub meter: String,
    pub source: Harness,
    /// Daemon receipt, not provider sample time.
    pub observed_at: DateTime<Utc>,
    /// Computed on read.
    pub stale: bool,
    /// Sorted by `(scope, duration_minutes)`.
    pub windows: Vec<PlanUsageWindow>,
}
