//! Pure policy for a handover the daemon runs without a command.
//!
//! [`idle::handover_step`](crate::idle::handover_step) compares one snapshot
//! with one set of limits. This module is the guard around it that makes it
//! safe to act on alone: the role's switch ([`handover_settings`]), what an
//! agent waits for as far as the organisation's state shows ([`waits`]), and
//! [`auto_step`]. Time is an argument; nothing here reads a clock, does I/O
//! or names a harness.
//!
//! # The switch
//!
//! Four role keys. All are optional, and a role without `auto_handover` is
//! off: it gets a note at its context line and nothing else.
//!
//! | Key | Meaning | Rule |
//! |---|---|---|
//! | `auto_handover` | the reasons the daemon may act on: `"context"`, `"idle"` | no other word, none twice |
//! | `context_line` | the seat's context line in percent | 1 to 100; default 60 |
//! | `cache_minutes` | the prompt cache life the role's author assumes | at least 2; required for `"idle"` |
//! | `idle_minutes` | idle time after which a seat that waits in vain is asked to save | at least 1, below `cache_minutes`; default four fifths of it, rounded down |
//!
//! There is no built-in cache life, because none has been measured: without
//! `cache_minutes` a seat has no idle rule. The context line also moves the
//! seat's note.
//!
//! # What an agent waits for
//!
//! | Fact about the agent | Counted as |
//! |---|---|
//! | it holds a task that is `submitted` or `in review`, measured by the owner's judgement | [`WaitsFor::Owner`] |
//! | it holds any other task that is not `accepted` or `cancelled` (an unknown state too) | [`WaitsFor::Agent`] |
//! | it created a task another agent holds, not `accepted` or `cancelled` | [`WaitsFor::Agent`] |
//! | it requested a decision that is pending | [`WaitsFor::Owner`], and [`WaitsFor::Until`] its deadline when it has one |
//! | none of these | [`WaitsFor::Nothing`] |
//!
//! `Agent` always comes in time: the reminder, the agent's manager or its own
//! next turn. Known limit: work that travels by message only, with no task,
//! is not seen, so an agent that reported by message and waits for an answer
//! waits for nothing here.
//!
//! # Decision table
//!
//! The earlier row wins. "At the line" is the idle policy's answer for the
//! share against the seat's line. The idle answer is the idle policy's, asked
//! once for every wait: due when every wait fails to come before the cache
//! expires, cold when the cache has expired.
//!
//! | # | Condition | Step |
//! |---|---|---|
//! | 1 | the agent is not ready, or not bound to this session | `Nothing` |
//! | 2 | no turn end is on record (a turn runs, a dialog is open, or none was seen) | `Nothing` |
//! | 3 | the idle session shows background work | `Nothing` |
//! | 4 | this session was already attempted or excluded for starting at its line | `Nothing` |
//! | 5 | the cache is cold | at the line `Flag`, else `Nothing` |
//! | 6 | at the line, `"context"` off, idle rule not due | `Flag`, with a look-again at the idle limit if still in the future |
//! | 7 | neither rule is due | `LookAgainAt(idle_since + idle limit)` before that time with `"idle"` on, else `Nothing` |
//! | 8 | the session has not worked | at the line `Flag`, else `Nothing` |
//! | 9 | a message to the agent is queued or delivering | `LookAgainAt(now + LOOK_AGAIN)` |
//! | 10 | at the line, `"context"` on | `Run(ContextLine)` |
//! | 11 | the idle rule is due | `Run(CacheExpiring)`, given up when the cache expires |
//!
//! - Row 4 bounds the cost: at most one save turn per session that nobody
//!   asked for, whatever becomes of it.
//! - Row 5: a clear with no save turn cannot be undone and would rest on an
//!   assumed cache life, so this policy never answers one, and a save turn on
//!   a cold cache pays for the whole context. A cold session is left to a
//!   person.
//! - Row 8: a handover ends with a new session's first turn, which warms the
//!   cache again. Without this row a seat that waits for nothing would be
//!   handed over after every idle limit.
//! - Row 9: mail on its way starts a turn of its own, whose end is judged
//!   instead. The step asks to look again, because a message that fails for
//!   good starts no turn.
//! - A time the calendar cannot hold is never due.
//!
//! # What the caller owns
//!
//! - `idle_since` is the end of the last turn it saw in this session, and the
//!   session has been idle ever since. A harness's signal for that, and when
//!   it misleads, are the caller's.
//! - `Flag` is the note at the context line, once per session; its
//!   `look_again_at` keeps the idle rule scheduled even when already noted.
//!   If `worked` is false, the caller marks this session attempted instead
//!   and drops the wake: neither rule may run later in a session that
//!   started at or above its line.
//! - `Run` is one handover. The caller remembers that it started one for the
//!   session (row 4), tells the agent's manager how it ended, and for
//!   `give_up_at` stops waiting at that time.
//! - Whether the owner is away. Nothing tells the caller yet.

use crate::idle::{
    handover_step, CacheLimits, HandoverInput, HandoverLimits, HandoverReason, HandoverStep,
    WaitsFor,
};
use crate::{
    Address, AgentId, AgentStatus, DecisionStatus, DeliveryStatus, OrgState, RoleConfig,
    TaskMeasure, TaskState,
};
use atm_core::SessionId;
use chrono::{DateTime, TimeDelta, Utc};
use std::time::Duration;
use thiserror::Error;

/// How long to wait before looking again while mail to the agent is on its way.
pub const LOOK_AGAIN: Duration = Duration::from_secs(60);

/// What a role says about handovers without a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HandoverSettings {
    /// Hand over at the context line.
    pub context: bool,
    /// Hand over an idle seat that waits in vain. Implies cache limits.
    pub idle: bool,
    pub limits: HandoverLimits,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SettingsError {
    #[error("auto_handover: unknown reason {0:?} (expected \"context\" or \"idle\")")]
    UnknownReason(String),
    #[error("auto_handover: {0:?} is listed twice")]
    Repeated(String),
    #[error("context_line must be 1 to 100, not {0}")]
    ContextLine(u8),
    #[error("cache_minutes must be at least 2, not {0}")]
    CacheMinutes(u32),
    #[error("idle_minutes must be at least 1 and below cache_minutes ({cache}), not {idle}")]
    IdleMinutes { idle: u32, cache: u32 },
    #[error("idle_minutes needs cache_minutes")]
    IdleWithoutCache,
    #[error("auto_handover \"idle\" needs cache_minutes: no cache life is built in")]
    IdleRuleWithoutCache,
}

/// The settings a role's keys state; see the module's table for the rules.
pub fn handover_settings(config: &RoleConfig) -> Result<HandoverSettings, SettingsError> {
    let mut settings = HandoverSettings::default();
    for reason in &config.auto_handover {
        let switch = match reason.as_str() {
            "context" => &mut settings.context,
            "idle" => &mut settings.idle,
            _ => return Err(SettingsError::UnknownReason(reason.clone())),
        };
        if *switch {
            return Err(SettingsError::Repeated(reason.clone()));
        }
        *switch = true;
    }
    if let Some(line) = config.context_line {
        if !(1..=100).contains(&line) {
            return Err(SettingsError::ContextLine(line));
        }
        settings.limits.context_line_percent = line;
    }
    match (config.cache_minutes, config.idle_minutes) {
        (None, Some(_)) => return Err(SettingsError::IdleWithoutCache),
        (None, None) if settings.idle => return Err(SettingsError::IdleRuleWithoutCache),
        (None, None) => {}
        (Some(cache), _) if cache < 2 => return Err(SettingsError::CacheMinutes(cache)),
        (Some(cache), idle) => {
            // Four fifths, in 64 bits so that no cache life overflows.
            let fifths = u32::try_from(u64::from(cache) * 4 / 5).unwrap_or(cache - 1);
            let idle = idle.unwrap_or(fifths);
            if idle == 0 || idle >= cache {
                return Err(SettingsError::IdleMinutes { idle, cache });
            }
            settings.limits.cache = Some(CacheLimits {
                lifetime: minutes(cache),
                idle_limit: minutes(idle),
            });
        }
    }
    Ok(settings)
}

fn minutes(count: u32) -> Duration {
    Duration::from_secs(u64::from(count) * 60)
}

/// Everything `agent` waits for that the state shows, never empty: a single
/// [`WaitsFor::Nothing`] when it shows no wait. See the module's table.
#[must_use]
pub fn waits(state: &OrgState, agent: &AgentId) -> Vec<WaitsFor> {
    let me = Address::Agent(agent.clone());
    let mut waits = Vec::new();
    for task in state.tasks() {
        if matches!(task.state, TaskState::Accepted | TaskState::Cancelled) {
            continue;
        }
        if task.agent_id.as_ref() == Some(agent) {
            let judged = matches!(task.state, TaskState::Submitted | TaskState::InReview);
            waits.push(if judged && task.measure == TaskMeasure::Owner {
                WaitsFor::Owner
            } else {
                WaitsFor::Agent
            });
        } else if task.creator == me && task.agent_id.is_some() {
            waits.push(WaitsFor::Agent);
        }
    }
    for decision in state.decisions() {
        if decision.status == DecisionStatus::Pending && decision.request.requester == me {
            waits.push(WaitsFor::Owner);
            waits.extend(decision.request.deadline.map(WaitsFor::Until));
        }
    }
    if waits.is_empty() {
        waits.push(WaitsFor::Nothing);
    }
    waits
}

/// One look at an agent's session. The caller's part is in the module docs.
#[derive(Debug, Clone, Copy)]
pub struct AutoInput<'a> {
    pub state: &'a OrgState,
    pub agent: &'a AgentId,
    /// The session the turn end was seen in.
    pub session: &'a SessionId,
    pub settings: &'a HandoverSettings,
    /// Share of the context window in use, 0 to 100. `None` when not known.
    pub context_percent: Option<f64>,
    /// The end of the last turn seen in this session, idle ever since.
    pub idle_since: Option<DateTime<Utc>>,
    /// The idle session shows background or scheduled work.
    pub background: bool,
    /// The session has ended a turn other than the one its start line began.
    pub worked: bool,
    /// This session was already attempted or excluded for starting at its line.
    pub attempted: bool,
    /// The owner has said he will not answer.
    pub away: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoStep {
    Nothing,
    /// Send the note at the context line; a person acts.
    Flag {
        /// Ask again at this time even if the session was already noted.
        look_again_at: Option<DateTime<Utc>>,
    },
    /// Run one handover for this reason.
    Run {
        reason: HandoverReason,
        /// Stop waiting for the save at this time: later it would be paid for
        /// on a cold cache.
        give_up_at: Option<DateTime<Utc>>,
    },
    /// Nothing now; ask again at this time if the session is still idle.
    LookAgainAt(DateTime<Utc>),
}

/// What the idle rule says for all of an agent's waits together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Idle {
    NotDue,
    Due,
    Cold,
}

/// The step due for this session at `now`; see the module's decision table.
#[must_use]
pub fn auto_step(input: &AutoInput<'_>, now: DateTime<Utc>) -> AutoStep {
    let bound = input.state.agent(input.agent).is_some_and(|agent| {
        agent.status == AgentStatus::Ready && agent.session_id.as_ref() == Some(input.session)
    });
    let Some(idle_since) = input.idle_since.filter(|_| bound) else {
        return AutoStep::Nothing;
    };
    if input.background || input.attempted {
        return AutoStep::Nothing;
    }
    let settings = input.settings;
    let at_line = at_the_line(input.context_percent, settings.limits, now);
    let cache = settings.limits.cache.filter(|_| settings.idle);
    let look_again_at = cache
        .and_then(|cache| later(idle_since, cache.idle_limit))
        .filter(|wake| now < *wake);
    let flag_or_nothing = if at_line {
        AutoStep::Flag { look_again_at }
    } else {
        AutoStep::Nothing
    };
    let idle = cache.map_or(Idle::NotDue, |cache| {
        idle_rule(input, cache, idle_since, now)
    });
    if idle == Idle::Cold {
        return flag_or_nothing;
    }
    let context_due = at_line && settings.context;
    if !context_due && idle == Idle::NotDue {
        if at_line {
            return flag_or_nothing;
        }
        return look_again_at.map_or(AutoStep::Nothing, AutoStep::LookAgainAt);
    }
    if !input.worked {
        return flag_or_nothing;
    }
    if mail_on_its_way(input.state, input.agent) {
        return later(now, LOOK_AGAIN).map_or(AutoStep::Nothing, AutoStep::LookAgainAt);
    }
    if context_due {
        return AutoStep::Run {
            reason: HandoverReason::ContextLine,
            give_up_at: None,
        };
    }
    match cache.and_then(|cache| later(idle_since, cache.lifetime)) {
        Some(expiry) => AutoStep::Run {
            reason: HandoverReason::CacheExpiring,
            give_up_at: Some(expiry),
        },
        None => AutoStep::Nothing,
    }
}

/// The idle policy flags `share` against the seat's line. A turn end has no
/// idle time, so what the agent waits for cannot change the answer.
fn at_the_line(share: Option<f64>, limits: HandoverLimits, now: DateTime<Utc>) -> bool {
    let input = HandoverInput {
        context_percent: share,
        idle_since: Some(now),
        waits_for: WaitsFor::Agent,
        away: false,
        automatic: false,
    };
    let line = HandoverLimits {
        cache: None,
        ..limits
    };
    handover_step(&input, &line, now) == HandoverStep::Flag(HandoverReason::ContextLine)
}

/// The idle policy's cache rows for every wait of the agent. The share is
/// left out: the context line is judged on its own.
fn idle_rule(
    input: &AutoInput<'_>,
    cache: CacheLimits,
    idle_since: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Idle {
    let limits = HandoverLimits {
        cache: Some(cache),
        ..input.settings.limits
    };
    let mut answer = Idle::Due;
    for waits_for in waits(input.state, input.agent) {
        let one = HandoverInput {
            context_percent: None,
            idle_since: Some(idle_since),
            waits_for,
            away: input.away,
            automatic: true,
        };
        match handover_step(&one, &limits, now) {
            HandoverStep::Clear(_) => return Idle::Cold,
            HandoverStep::Save(HandoverReason::CacheExpiring) => {}
            _ => answer = Idle::NotDue,
        }
    }
    answer
}

pub(crate) fn mail_on_its_way(state: &OrgState, agent: &AgentId) -> bool {
    state
        .inbox(&Address::Agent(agent.clone()))
        .into_iter()
        .any(|message| {
            matches!(
                message.status,
                DeliveryStatus::Queued | DeliveryStatus::Delivering
            )
        })
}

/// `None` when the calendar cannot hold the time.
pub(crate) fn later(from: DateTime<Utc>, by: Duration) -> Option<DateTime<Utc>> {
    from.checked_add_signed(TimeDelta::from_std(by).ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MessageContent, MessageId, MessageKind, OrgEvent};

    #[test]
    fn mail_on_its_way_ignores_owner_system_other_agents_and_empty_inboxes() {
        let now = DateTime::from_timestamp(0, 0).unwrap();
        let agent: AgentId = "p/worker".parse().unwrap();
        let other: AgentId = "p/other".parse().unwrap();
        let mut state = OrgState::default();
        state
            .apply(
                1,
                now,
                &OrgEvent::ProjectRegistered {
                    project_id: "p".parse().unwrap(),
                    root: "/p".into(),
                },
            )
            .unwrap();
        for (seq, id) in [(2, &agent), (3, &other)] {
            state
                .apply(
                    seq,
                    now,
                    &OrgEvent::AgentAdopted {
                        agent_id: id.clone(),
                        pane: format!("%{seq}"),
                        session_id: None,
                    },
                )
                .unwrap();
        }
        assert!(!mail_on_its_way(&state, &agent));

        for (to, addressed) in [
            (Address::Owner, false),
            (Address::System, false),
            (Address::Agent(other), false),
            (Address::Agent(agent.clone()), true),
        ] {
            let message_id = MessageId::new(4);
            let mut inbox = state.clone();
            inbox
                .apply(
                    4,
                    now,
                    &OrgEvent::MessageSent(MessageContent {
                        delivery_notice: None,
                        from: Address::Owner,
                        to: to.clone(),
                        kind: MessageKind::Note,
                        body: "Read this".into(),
                        attachments: Vec::new(),
                        task: None,
                        post: None,
                    }),
                )
                .unwrap();
            // Apply committed facts to cover every address/status combination,
            // including ones today's send policy would not create.
            for (event, pending) in [
                (
                    OrgEvent::MessageDeliveryFailed {
                        message_id,
                        reason: "retry".into(),
                        final_failure: false,
                    },
                    true,
                ),
                (
                    OrgEvent::MessageDeliveryAttempted {
                        message_id,
                        attempt: 1,
                        pane: "%2".into(),
                    },
                    true,
                ),
                (
                    OrgEvent::MessageDelivered {
                        message_id,
                        session_id: SessionId::new("s1"),
                        confirmation: "prompt".into(),
                    },
                    false,
                ),
                (
                    OrgEvent::MessageDeliveryFailed {
                        message_id,
                        reason: "gone".into(),
                        final_failure: true,
                    },
                    false,
                ),
            ] {
                let mut one = inbox.clone();
                one.apply(5, now, &event).unwrap();
                assert_eq!(
                    mail_on_its_way(&one, &agent),
                    addressed && pending,
                    "unread: {to}, pending={pending}"
                );
                one.apply(
                    6,
                    now,
                    &OrgEvent::MessageRead {
                        message_ids: vec![message_id],
                    },
                )
                .unwrap();
                assert_eq!(
                    mail_on_its_way(&one, &agent),
                    addressed && pending,
                    "read: {to}, pending={pending}"
                );
            }
        }
    }

    #[test]
    fn checked_time_addition_preserves_precision_and_rejects_both_overflows() {
        let epoch = DateTime::from_timestamp(0, 0).unwrap();
        for (from, by, expected) in [
            (epoch, Duration::ZERO, Some(epoch)),
            (epoch, Duration::new(60, 7), DateTime::from_timestamp(60, 7)),
            (
                DateTime::<Utc>::MAX_UTC,
                Duration::ZERO,
                Some(DateTime::<Utc>::MAX_UTC),
            ),
            (DateTime::<Utc>::MAX_UTC, Duration::from_nanos(1), None),
            (epoch, Duration::MAX, None),
        ] {
            assert_eq!(later(from, by), expected, "{from} + {by:?}");
        }
    }
}
