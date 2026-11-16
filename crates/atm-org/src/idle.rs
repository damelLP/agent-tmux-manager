//! Pure handover policy: when a session should hand over to a fresh one.
//!
//! [`handover_step`] answers from one snapshot of an agent (its context share,
//! since when it has been idle, what it waits for) and the limits for its
//! harness and model. Limits are plain values: this module names no harness.
//! Time is an argument; nothing here reads a clock or does I/O.
//!
//! # Decision table
//!
//! The earlier row wins. `idle` is `now - idle_since`, zero when the clock
//! stepped back. The cache expires at `idle_since + lifetime`.
//!
//! | # | Condition | `automatic` off | `automatic` on |
//! |---|---|---|---|
//! | 1 | not idle (a turn runs, or a dialog is open) | Nothing | Nothing |
//! | 2 | cache limits known, `idle >= lifetime` (cold) | Flag(CacheCold) | Clear(CacheCold) |
//! | 3 | share known, `share >= context_line_percent` | Flag(ContextLine) | Save(ContextLine) |
//! | 4 | cache limits known, `idle >= idle_limit`, and what it waits for will not come before the cache expires | Flag(CacheExpiring) | Save(CacheExpiring) |
//! | 5 | otherwise | Nothing | Nothing |
//!
//! Whether what it waits for comes before the cache expires (row 4):
//!
//! | [`WaitsFor`] | Comes in time |
//! |---|---|
//! | `Nothing` | no |
//! | `Owner` | yes, unless the owner is away |
//! | `Agent` | yes: an unknown arrival gets the benefit of the doubt |
//! | `Until(t)` | yes if `t` is before the expiry (a time already due included); no at the expiry or later |
//!
//! Every limit is inclusive. A missing or NaN share never reaches the line.
//! The share is compared unrounded, so a display that rounds 59.6 to "60%"
//! is not yet at the line. A cold session is cleared without a save turn,
//! because that turn would pay for the whole context again; so row 2 comes
//! before the context line.
//!
//! # What the caller owns
//!
//! - `Save` means "run the handover": ask the agent to save, then clear it.
//!   `Clear` means clear with no save turn. Do not ask this policy again while
//!   a handover runs.
//! - The policy keeps no memory and answers the same for as long as the
//!   condition holds. Sending one note per session is the caller's.
//! - The policy never refuses. A handover on command, by the owner, a manager
//!   or the agent about itself, does not consult it, and the input carries no
//!   agent identity: a lead is judged like any member.
//! - `automatic` is the role's switch. With it off every answer is at most a
//!   flag for a person to act on.
//! - **Rows 2 and 4 must not be run on a timer alone** (reasoned, unverified:
//!   nothing calls them yet). A clear or a handover ends with a new session's
//!   first turn, which warms the cache again. A session that still waits for
//!   the same thing would then meet row 4 after every idle limit and row 2
//!   after every lifetime, one or two model turns each time. So carry out
//!   `Clear(CacheCold)` before the session's next work, not when it turns
//!   cold, and do not ask a session to save whose only turn was its start.
//!
//! # Limits
//!
//! [`HandoverLimits::default`] holds only the context line, 60 percent for
//! every harness and model: the owner's choice, not a measurement, until one
//! gives a line for each. It has
//! no cache numbers, so rows 2 and 4 are off until the caller passes
//! [`CacheLimits`] for the agent's harness and model. The idle limit belongs
//! below the lifetime; at or above it row 4 never answers and the session is
//! cleared cold. The reader of the settings validates that, this function
//! accepts any values.
//!
//! Assumed, unverified: a harness's prompt cache lives for `lifetime` from the
//! turn end (`idle_since`). No harness has been measured against this module.

use chrono::{DateTime, Utc};
use std::time::Duration;

/// What an idle agent waits for. The caller maps its tasks and decisions here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitsFor {
    /// No open work: nothing is coming that this session's context serves.
    Nothing,
    /// An answer only the owner gives.
    Owner,
    /// Another agent's work, with no known arrival time.
    Agent,
    /// Something due at a known time.
    Until(DateTime<Utc>),
}

/// One snapshot of an agent.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HandoverInput {
    /// Share of the context window in use, 0 to 100. `None` when not known.
    pub context_percent: Option<f64>,
    /// The end of the last turn. `None` while a turn runs or a dialog is open.
    pub idle_since: Option<DateTime<Utc>>,
    pub waits_for: WaitsFor,
    /// The owner has said he will not answer.
    pub away: bool,
    /// The role allows a handover without a command.
    pub automatic: bool,
}

/// Limits for one harness and model, passed in as data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HandoverLimits {
    pub context_line_percent: u8,
    /// `None` when the cache lifetime is not known: the idle rows are off.
    pub cache: Option<CacheLimits>,
}

impl Default for HandoverLimits {
    fn default() -> Self {
        Self {
            context_line_percent: 60,
            cache: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheLimits {
    /// How long the prompt cache lives after a turn end.
    pub lifetime: Duration,
    /// Idle time after which a session that waits in vain is asked to save.
    pub idle_limit: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandoverReason {
    /// The context share is at or above the line.
    ContextLine,
    /// The cache will expire before what the agent waits for arrives.
    CacheExpiring,
    /// The cache has expired.
    CacheCold,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandoverStep {
    Nothing,
    /// Tell a person; act only on a command.
    Flag(HandoverReason),
    /// Ask the agent to save its state, then clear it.
    Save(HandoverReason),
    /// Clear it with no save turn.
    Clear(HandoverReason),
}

/// The step due for this agent at `now`; see the module's decision table.
#[must_use]
pub fn handover_step(
    input: &HandoverInput,
    limits: &HandoverLimits,
    now: DateTime<Utc>,
) -> HandoverStep {
    let Some(idle_since) = input.idle_since else {
        return HandoverStep::Nothing;
    };
    let idle = elapsed(idle_since, now).unwrap_or_default();
    let reason = if limits.cache.is_some_and(|cache| idle >= cache.lifetime) {
        HandoverReason::CacheCold
    } else if input
        .context_percent
        .is_some_and(|share| share >= f64::from(limits.context_line_percent))
    {
        HandoverReason::ContextLine
    } else if limits.cache.is_some_and(|cache| {
        idle >= cache.idle_limit && !comes_in_time(input, idle_since, cache.lifetime)
    }) {
        HandoverReason::CacheExpiring
    } else {
        return HandoverStep::Nothing;
    };
    match (input.automatic, reason) {
        (false, _) => HandoverStep::Flag(reason),
        (true, HandoverReason::CacheCold) => HandoverStep::Clear(reason),
        (true, _) => HandoverStep::Save(reason),
    }
}

/// `None` when `to` is before `from`.
fn elapsed(from: DateTime<Utc>, to: DateTime<Utc>) -> Option<Duration> {
    to.signed_duration_since(from).to_std().ok()
}

fn comes_in_time(input: &HandoverInput, idle_since: DateTime<Utc>, lifetime: Duration) -> bool {
    match input.waits_for {
        WaitsFor::Nothing => false,
        WaitsFor::Owner => !input.away,
        WaitsFor::Agent => true,
        // A time before the turn end is already due.
        WaitsFor::Until(due) => elapsed(idle_since, due).is_none_or(|wait| wait < lifetime),
    }
}
