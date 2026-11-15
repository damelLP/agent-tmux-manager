//! Running without the owner: the away table and the stacking cap.
//!
//! Pure: the class, the away flag, counts and the cap are arguments. Nothing
//! here reads a clock, a file or the daemon.
//!
//! # The away table
//!
//! [`away_step`] answers what happens to a *pending* decision. "Recommended"
//! is whether the request names a recommended option.
//!
//! | Class | Owner present | Away, recommended | Away, none recommended |
//! |---|---|---|---|
//! | [`DecisionClass::Detail`] | waits | the agent decides | waits |
//! | [`DecisionClass::ChangesIntent`] | waits | taken provisionally | waits |
//! | [`DecisionClass::Irreversible`] | waits | waits | waits |
//!
//! While the owner is present nothing is taken for him. Without a recommended
//! option nothing can be recorded, so the decision waits whatever its class.
//! Both outcomes that record something are written as
//! `decision.taken_provisionally` on the recommended option: by `system` when
//! it is taken provisionally, by the requester when the agent decides. The
//! owner can overrule either.
//!
//! # The stacking cap
//!
//! A provisional decision may carry the task in progress to its end; nothing
//! else may be built on it until the owner has answered it. The cap counts
//! tasks. The task in progress when the decision was taken is the first; it is
//! not linked to the decision, so it is counted without being named. Further
//! tasks are those whose `rests_on` names the decision and that have started.
//!
//! [`stacking_step`] lets a further task start only while
//! `1 + started < cap`, and only holds for a decision that is taken
//! provisionally and changes the intent:
//!
//! | Decision the task rests on | Step |
//! |---|---|
//! | taken provisionally, changes the intent, `1 + started >= cap` | hold |
//! | taken provisionally, changes the intent, `1 + started < cap` | start |
//! | taken provisionally, a detail (the agent decided it) | start |
//! | pending, resolved, overruled, expired, withdrawn | start |
//!
//! With the default cap of one no further task starts. With a cap of N, N - 1
//! may. Known limits: a task that does not declare `rests_on` is not seen, and
//! chains (a child of a resting task, a second decision raised inside one) are
//! not followed.
//!
//! # Settings
//!
//! [`parse_away_settings`] reads a project's away settings: one optional key,
//! `stacking_cap`, at least 1, default [`DEFAULT_STACKING_CAP`]. Unknown keys
//! are refused.

use crate::{DecisionClass, DecisionStatus};
use serde::Deserialize;
use thiserror::Error;

/// Why a pending decision waits for the owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AwayWait {
    /// The owner has not said he is away.
    OwnerPresent,
    /// It cannot be undone, or reaches outside the project.
    Irreversible,
    /// It names no recommended option, so nothing can be recorded.
    NoRecommendation,
}

/// What happens to a pending decision; see the module's away table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AwayStep {
    Wait(AwayWait),
    /// Record the recommended option, by the system.
    TakeProvisionally,
    /// Record the requester's own recommended option, by the requester.
    AgentDecides,
}

/// The step for a pending decision of this class. The owner's presence comes
/// first, then a class that must wait, then a missing recommendation.
#[must_use]
pub fn away_step(class: DecisionClass, recommended: bool, away: bool) -> AwayStep {
    if !away {
        return AwayStep::Wait(AwayWait::OwnerPresent);
    }
    match (class, recommended) {
        (DecisionClass::Irreversible, _) => AwayStep::Wait(AwayWait::Irreversible),
        (_, false) => AwayStep::Wait(AwayWait::NoRecommendation),
        (DecisionClass::ChangesIntent, true) => AwayStep::TakeProvisionally,
        (DecisionClass::Detail, true) => AwayStep::AgentDecides,
    }
}

/// The cap of a project that sets none: only the task in progress.
pub const DEFAULT_STACKING_CAP: u32 = 1;

/// Whether a task that rests on a decision may start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackingStep {
    Start,
    /// The cap is reached: the task waits until the owner has answered.
    Hold,
}

/// The step for a task not yet started that rests on a decision with this
/// status and class. `started` counts the other tasks resting on the same
/// decision that have started; the task in progress is added here.
#[must_use]
pub fn stacking_step(
    status: DecisionStatus,
    class: DecisionClass,
    started: usize,
    cap: u32,
) -> StackingStep {
    let capped =
        status == DecisionStatus::TakenProvisionally && class == DecisionClass::ChangesIntent;
    let in_line = u64::try_from(started).unwrap_or(u64::MAX).saturating_add(1);
    if capped && in_line >= u64::from(cap) {
        StackingStep::Hold
    } else {
        StackingStep::Start
    }
}

/// A project's away settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AwaySettings {
    #[serde(default = "default_stacking_cap")]
    pub stacking_cap: u32,
}
fn default_stacking_cap() -> u32 {
    DEFAULT_STACKING_CAP
}
impl Default for AwaySettings {
    fn default() -> Self {
        Self {
            stacking_cap: DEFAULT_STACKING_CAP,
        }
    }
}

#[derive(Debug, Error)]
pub enum AwaySettingsError {
    /// The diagnostic can quote the file's text.
    #[error("away settings: TOML: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("stacking_cap must be at least 1")]
    CapBelowOne,
}

/// Parse a project's away settings. An empty text is the default.
pub fn parse_away_settings(text: &str) -> Result<AwaySettings, AwaySettingsError> {
    let settings: AwaySettings = toml::from_str(text)?;
    if settings.stacking_cap < 1 {
        return Err(AwaySettingsError::CapBelowOne);
    }
    Ok(settings)
}
