//! Pure delivery policy; observations and time belong to the executor.
//!
//! Consume each decision once, serialized per recipient pane: at most one
//! message may have an unconfirmed attempt there, including a queued retry.
//! Use [`delivery_in_flight`] for all other messages to compute the pane veto.
//! Decide/append/apply `DeliveryAttempted`
//! before paste + Enter; never paste if append fails or the command produces no
//! event. Recheck live guards immediately before I/O and update the pane's last
//! paste/observation continuity before releasing serialization. If a reserved
//! attempt cannot run, record `DeliveryFailed` with finality from
//! [`DeliveryPolicy::is_final_attempt`], never silently requeue. Resolution,
//! subagent targeting, bracketed-paste safety and token formatting stay outside
//! this module. A pure decision cannot make a paste and a log append atomic.
//!
//! `Confirm` maps to `Delivered`. `RetryAt` while delivering requires nonfinal
//! `DeliveryFailed`; once applied, recompute the deadline from logged failed_at.
//! `RetryAt` while queued only schedules. `Fail` requires final `DeliveryFailed`
//! (accepted from queued or delivering). `Blocked` emits no message event:
//! retain state, notify the owner once per reason, and reevaluate on changes.
//! A blocked attempt keeps its pane reservation until receipt or owner give-up.
//! `Wait(OtherDelivery)` is unbounded: the blocked predecessor's notice must
//! list the messages queued behind it, rather than send separate timeout notices.
//! `Wait(OwnerFocused)` also shows a deduplicated notice, with no timeout.
//! An owner can give up on any held message via final `DeliveryFailed`.
//! Continue listening for late receipts even after a final failure.
//! Wait/AwaitConfirmation/Complete emit no events. Deadlines are inclusive;
//! events can wake a timer earlier.
//!
//! # Restart and evidence
//!
//! The executor latches activity by receive order after applying the attempt,
//! not by comparing event/attempt timestamps. Working-to-working activity counts.
//! Evidence is not a receipt, but prevents automatic retries. On restart the
//! receipt and evidence latches are lost: **every prior unconfirmed attempt**,
//! queued or delivering, is held via `last_attempt_before_start`. Replay/process
//! lifetime establishes this input even if the wall clock stepped backwards.
//! Only a receipt or explicit owner resolution ends that hold (give up now;
//! an explicit retry command belongs to the executor's later API). A changed
//! budget or a newly empty screen must not release it. No attempt means no
//! restart hold. Terminal messages stay terminal unless a late receipt arrives.
//!
//! # Decision table
//!
//! Earlier applicable rows win. Metadata/arithmetic errors return a typed error,
//! never permission to paste; settings are validated before entering this policy.
//!
//! | State / condition | Step |
//! |---|---|
//! | Delivered, or addressed to owner | Complete |
//! | Token receipt, including queued/delivering/failed | Confirm |
//! | Failed without receipt | Complete |
//! | Queued/delivering, last attempt predates daemon start | Blocked(Restarted) |
//! | Attempts exceed new budget, or queued at exhausted budget | Fail(AttemptLimit) |
//! | Delivering with landing evidence | AwaitConfirmation until deadline, then Blocked(ProbablyDelivered) |
//! | Delivering, pane unknown | Bounded unknown hold |
//! | Delivering, pane positively gone | Wait grace, then RetryAt if budget remains, else Fail |
//! | Delivering without receipt/evidence, pane alive | AwaitConfirmation until deadline; then RetryAt or final Fail |
//! | Queued retry with landing evidence | Blocked(ProbablyDelivered) |
//! | Queued retry before logged failed_at + retry delay | RetryAt; no new failure event |
//! | Queued, owner focused (unknown focus is focused) | Wait(OwnerFocused), notice, no timeout |
//! | Queued, agent starting/clearing | Wait(AgentNotReady); Blocked(AgentNotReadyTimeout) after continuous non-ready + readiness_timeout |
//! | Queued, agent stuck/down | Blocked(AgentUnavailable); ready resumes |
//! | Queued, another message delivering or retry-pending on pane | Wait(OtherDelivery), unbounded; predecessor's notice lists the queue |
//! | Queued, pane unknown / gone | Bounded unknown hold / Wait grace then Fail |
//! | Queued, fresh non-idle row, no send_now OR input not EmptySince | Wait(SessionBusy) until stale or new observation |
//! | Stale non-idle row, draft/dialog | Wait; Blocked after three guard polls of this blocker, capped by the overall hold |
//! | Other copy mode, dialog, draft, unknown guards/input | Wait; Blocked after continuous hold + readiness_timeout |
//! | No row, positive empty input | Wait until max(empty_since,last_paste) + stable_empty_input |
//! | Idle row, no send_now | Wait until max(status_since,empty_since,last_paste) + stable_idle |
//! | Stale non-idle row, or send_now with any row | Wait until max(empty_since,last_paste) + stable_idle |
//! | All queued guards satisfied | PasteNow with increasing attempt |
//!
//! # Hold continuity
//!
//! The executor supplies the start of the uninterrupted stretch in which this
//! message could not paste, not the start of the current blocker. Changing guards
//! or briefly observing empty input does not reset it. Incomplete stabilization
//! and pane-gone grace also become Blocked(ReadinessTimeout) at that bound; a
//! fully stable, safe editor may still paste immediately. Blocked is a notice,
//! not a latch. A long preceding turn does not spend this budget.
//!
//! | Consumed step / reason | Overall hold clock |
//! |---|---|
//! | Wait CopyMode/DialogOpen/UnknownGuards/InputNotEmpty/InputUnknown | Start/continue |
//! | Wait IdleStabilizing/InputStabilizing/PaneGoneGrace | Start/continue |
//! | Wait OwnerFocused/SessionBusy/AgentNotReady/OtherDelivery | Reset |
//! | PasteNow | Reset on serialized consumption; reset pane readiness after paste |
//! | Blocked (any reason) | Retain; never restart because a notice was emitted |
//! | RetryAt/AwaitConfirmation | Preserve an existing hold; do not start one for scheduling |
//! | Confirm/Complete/Fail | Discard after consumption ends the message lifecycle |
//! | Error | Do not advance continuity based on an unsuccessful decision |
//!
//! Missing hold history starts at now, not sent_at. The executor must retain
//! subsequent continuity, or repeated missing history would extend the window.
//! After restart, never-attempted messages start at recovery; prior attempts keep
//! the restart veto. The independent agent-not-ready clock survives non-ready
//! status changes and focus waits; see [`RecipientLiveView::agent_not_ready_since`].
//! `EmptySince` means a positively recognized
//! empty editor **and no running turn**, never a blank screen or an empty box
//! during active work. `send_now` skips row status/stability only once the editor
//! is positively empty; it never spends the hold budget on a fresh running turn.
//!
//! A stale-row draft/dialog has a separate short confirmation: three guard polls
//! (750 ms by default). The executor supplies `stale_blocker_since`, resetting it
//! on a changed/cleared/unknown blocker, a fresh row, or a non-hold wait. Never
//! borrow an earlier blocker's short confirmation. The overall hold still caps
//! flapping, and unknown observations retain the full readiness timeout.
//!
//! # Defaults
//!
//! Stable idle/empty with row: 10 s; without row: 5 s; confirmation: 3 s;
//! retry delays: [1, 2, 4] s (four attempts); stale/readiness: 120 s each;
//! guard poll: 250 ms; positive pane-gone grace: 2 s. The idle default is not
//! a measured maximum automatic-retry gap: the executor's real integration
//! check must measure that gap and tune the setting.

use crate::{Address, AgentStatus, DeliveryReceipt, DeliveryStatus, Message};
use atm_core::SessionStatus;
use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use std::time::Duration;
use thiserror::Error;

/// Config-file settings. Durations serialize as `{secs, nanos}`; omitted fields
/// take defaults, unknown keys are errors. Validate via `DeliveryPolicy::new`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DeliverySettings {
    pub stable_idle: Duration,
    pub stable_empty_input: Duration,
    pub confirmation_timeout: Duration,
    /// Delays following attempts 1, 2, ...; maximum attempts = len + 1.
    /// Empty means one attempt. Delays must be positive and nondecreasing.
    pub retry_delays: Vec<Duration>,
    pub stale_after: Duration,
    pub readiness_timeout: Duration,
    pub guard_poll_interval: Duration,
    pub pane_gone_grace: Duration,
}

/// Immutable, validated settings; deserialization uses the same validation.
/// Durations must be positive and fit when added to `9999-12-31T23:59:59Z`,
/// including three guard polls for stale-editor confirmation. Later caller
/// timestamps can still return [`DeliveryPolicyError::TimeOverflow`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct DeliveryPolicy {
    settings: DeliverySettings,
}

impl DeliveryPolicy {
    pub fn new(settings: DeliverySettings) -> Result<Self, DeliveryPolicyError> {
        validate_settings(&settings)?;
        Ok(Self { settings })
    }

    #[must_use]
    pub fn settings(&self) -> &DeliverySettings {
        &self.settings
    }

    /// Use when a reserved attempt cannot execute: final iff no attempts remain.
    #[must_use]
    pub fn is_final_attempt(&self, attempt: u32) -> bool {
        self.retry_delay(attempt).is_none()
    }

    fn retry_delay(&self, attempt: u32) -> Option<Duration> {
        attempt
            .checked_sub(1)
            .and_then(|n| usize::try_from(n).ok())
            .and_then(|n| self.settings.retry_delays.get(n))
            .copied()
    }
}

impl<'de> Deserialize<'de> for DeliveryPolicy {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::new(DeliverySettings::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

impl Default for DeliverySettings {
    fn default() -> Self {
        Self {
            // Observed automatic retries can follow a transient idle row. This
            // is a tuning default, not a verified upper bound: the executor's
            // real-harness check must measure that gap and tune this setting.
            stable_idle: Duration::from_secs(10),
            stable_empty_input: Duration::from_secs(5),
            confirmation_timeout: Duration::from_secs(3),
            retry_delays: vec![
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4),
            ],
            stale_after: Duration::from_secs(120),
            readiness_timeout: Duration::from_secs(120),
            guard_poll_interval: Duration::from_millis(250),
            pane_gone_grace: Duration::from_secs(2),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveSession {
    pub status: SessionStatus,
    /// Continuous status observation; reset on status or session identity change.
    pub status_since: DateTime<Utc>,
    /// Latest session event, not the time of a registry poll.
    pub last_event_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputLine {
    Unknown,
    NonEmpty,
    /// Positively recognized empty harness input AND no running turn on screen
    /// (no spinner or interrupt hint), continuously since this time. An empty
    /// box during a turn, blank terminal, shell prompt or old scrollback is NOT
    /// this evidence. Reset on identity change or any loss of that observation.
    EmptySince(DateTime<Utc>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneLiveness {
    Alive,
    /// Positive evidence of absence, continuously since this time. Reset after
    /// alive OR unknown; a failed query is Unknown, never GoneSince.
    GoneSince(DateTime<Utc>),
    Unknown,
}

/// Fresh, neutral observations for this message's resolved recipient pane.
/// Deliberately has no Default: missing observations must not grant permission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecipientLiveView {
    /// None is a missing row, not a dead pane.
    pub session: Option<LiveSession>,
    pub pane: PaneLiveness,
    pub agent_status: AgentStatus,
    /// Start of the continuous non-Ready interval, independent of message holds.
    /// Reconstruct from applied transitions in log order: set when leaving Ready
    /// or first creating a non-ready agent; clear on Ready. Preserve across all
    /// non-ready status changes and metadata events. Agent.updated_at is NOT this
    /// clock (even recording a pane changes it). Without history, start at the
    /// first observation; None starts now and must be retained on later polls.
    pub agent_not_ready_since: Option<DateTime<Utc>>,
    /// Another message has an unconfirmed attempt on this pane, including queued
    /// retries and blocked attempts. Compute with delivery_in_flight over other
    /// messages, excluding this message by ID. Serialize reservations per pane.
    pub other_delivery_in_flight: bool,
    /// Last paste to this pane (any message). Reset readiness after every paste.
    /// On restart use recovery time if the last paste cannot be reconstructed.
    pub last_paste_at: Option<DateTime<Utc>>,
    /// Start of the continuous inability to paste, including stabilization and
    /// pane-gone grace. Preserve across blocker/source changes and brief empty
    /// samples. Reset on paste or a non-hold wait (focus, busy row, agent state,
    /// other delivery); see the module's table. None starts now, never sent_at.
    pub hold_since: Option<DateTime<Utc>>,
    /// Start of continuous observation of this effective draft/dialog on a stale
    /// non-idle row. Reset on blocker change/clear/unknown, row freshness, or a
    /// non-hold wait; do NOT reset hold_since when just the blocker changes.
    /// None starts now and must be retained on later polls. The short bound is
    /// three guard polls; the overall readiness bound still caps flapping.
    pub stale_blocker_since: Option<DateTime<Utc>>,
    /// The last attempt predates this daemon process, including queued retries.
    /// Compute from replay/process lifetime, not wall-clock timestamp order.
    /// Lost receipt/landing latches require a hold until receipt or explicit
    /// owner resolution; new screen observations must not clear this input.
    pub last_attempt_before_start: bool,
    /// Unknown guards block just as positive unsafe guards do.
    pub copy_mode: Option<bool>,
    pub dialog_open: Option<bool>,
    /// True iff any tmux client is BOTH focused (client_flags) and selecting
    /// this pane (pane_id). Unknown is true; selected but unfocused is false.
    /// A turn end only triggers a recheck, never overrides focus.
    pub owner_focused: bool,
    pub input_line: InputLine,
    /// Bypass row status/stability only with positively empty input; never skip
    /// empty-input/post-paste stability or spend readiness budget on a fresh turn.
    pub send_now: bool,
    /// Executor validated this message's token in a prompt-submitted event for
    /// its target pane/session. Latch until Delivered is applied. The receipt
    /// may name an earlier session/attempt; current live guards cannot veto it.
    pub confirmation: Option<DeliveryReceipt>,
    /// A session event or non-idle status RECEIVED after DeliveryAttempted was
    /// applied (receive order, NOT timestamp comparison), or token seen in pane.
    /// Includes working -> working activity. Not a receipt. Latch across polls
    /// and failure/retry scheduling; reset only on a new reserved attempt.
    /// A restart loses the latch: set last_attempt_before_start for ALL replayed
    /// messages with prior attempts, including queued ones with late evidence.
    pub probably_landed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DeliveryStep {
    Complete,
    Wait {
        until: DateTime<Utc>,
        reason: WaitReason,
    },
    PasteNow {
        attempt: u32,
    },
    AwaitConfirmation {
        until: DateTime<Utc>,
    },
    Confirm {
        receipt: DeliveryReceipt,
    },
    RetryAt {
        at: DateTime<Utc>,
        reason: FailureReason,
    },
    Fail {
        reason: FailureReason,
    },
    Blocked {
        reason: FailureReason,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum WaitReason {
    #[error("another delivery is in flight on this pane")]
    OtherDelivery,
    #[error("recipient agent is starting or clearing")]
    AgentNotReady,
    #[error("waiting to establish that the pane is gone")]
    PaneGoneGrace,
    #[error("owner is focused on this pane")]
    OwnerFocused,
    #[error("pane is in copy mode")]
    CopyMode,
    #[error("dialog is open")]
    DialogOpen,
    #[error("pane guards are unknown")]
    UnknownGuards,
    #[error("input contains a draft")]
    InputNotEmpty,
    #[error("empty input with no running turn is not established")]
    InputUnknown,
    #[error("waiting for stable idle")]
    IdleStabilizing,
    #[error("waiting for stable empty input")]
    InputStabilizing,
    #[error("session is busy")]
    SessionBusy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum FailureReason {
    #[error("recipient agent is stuck or down")]
    AgentUnavailable,
    #[error("recipient agent did not become ready before the readiness deadline")]
    AgentNotReadyTimeout,
    #[error("daemon restarted after an unconfirmed attempt; owner resolution required")]
    Restarted,
    #[error("pane remained in copy mode")]
    CopyMode,
    #[error("prompt token was not confirmed before the deadline")]
    ConfirmationTimeout,
    #[error("recipient pane is not alive")]
    PaneGone,
    #[error("input draft persisted past the delivery hold deadline")]
    DraftPresent,
    #[error("dialog persisted past the delivery hold deadline")]
    DialogOpen,
    #[error("recipient readiness could not be established")]
    ReadinessTimeout,
    #[error("delivery attempt limit reached")]
    AttemptLimit,
    #[error("unconfirmed, probably delivered; inspect before resending")]
    ProbablyDelivered,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum DeliveryPolicyError {
    #[error("delivery retry delays must be nondecreasing and attempt count representable")]
    InvalidConfiguration,
    #[error(
        "delivery duration {setting} must be positive and fit a deadline from 9999-12-31T23:59:59Z"
    )]
    InvalidDuration { setting: &'static str },
    #[error("delivery state lacks consistent attempt/failure metadata")]
    InvalidState,
    #[error("delivery deadline is outside the timestamp range")]
    TimeOverflow,
}

/// Whether an unconfirmed attempt reserves this pane, including a queued retry.
/// The caller excludes the current message by ID before computing its live-view
/// veto over other messages. Blocked emits no event and retains the reservation;
/// a receipt or final failure releases it. Inputs are applied message state.
#[must_use]
pub fn delivery_in_flight(message: &Message, pane: &str) -> bool {
    matches!(
        message.status,
        DeliveryStatus::Queued | DeliveryStatus::Delivering
    ) && message
        .last_attempt
        .as_ref()
        .is_some_and(|attempt| attempt.pane == pane)
}

/// Decide without mutation. Caller must apply commands before another decision.
/// Policy is already valid; terminal/receipt decisions precede live guards.
/// Status and log ordering, not wall-clock timestamp ordering, identify attempts.
#[must_use = "consume the decision through the serialized delivery executor"]
pub fn next_delivery(
    message: &Message,
    live: &RecipientLiveView,
    now: DateTime<Utc>,
    policy: &DeliveryPolicy,
) -> Result<DeliveryStep, DeliveryPolicyError> {
    if message.status == DeliveryStatus::Delivered || message.content.to == Address::Owner {
        return Ok(DeliveryStep::Complete);
    }
    if let Some(receipt) = &live.confirmation {
        return Ok(DeliveryStep::Confirm {
            receipt: receipt.clone(),
        });
    }
    if message.status == DeliveryStatus::Failed {
        return Ok(DeliveryStep::Complete);
    }
    let settings = policy.settings();
    let attempt = match (&message.last_attempt, message.last_attempt_at) {
        (Some(attempt), Some(_)) if attempt.attempt > 0 => attempt.attempt,
        (None, None) if message.status == DeliveryStatus::Queued => 0,
        _ => return Err(DeliveryPolicyError::InvalidState),
    };
    if live.probably_landed && attempt == 0 {
        return Err(DeliveryPolicyError::InvalidState);
    }
    if live.last_attempt_before_start && attempt > 0 {
        return Ok(DeliveryStep::Blocked {
            reason: FailureReason::Restarted,
        });
    }
    let retry_delay = policy.retry_delay(attempt);
    let max_attempts = settings.retry_delays.len().saturating_add(1);
    if usize::try_from(attempt).map_or(true, |n| n > max_attempts)
        || (message.status == DeliveryStatus::Queued && attempt > 0 && retry_delay.is_none())
    {
        return Ok(DeliveryStep::Fail {
            reason: FailureReason::AttemptLimit,
        });
    }
    let poll = |reason| {
        Ok(DeliveryStep::Wait {
            until: deadline(now, settings.guard_poll_interval)?,
            reason,
        })
    };
    let bounded_wait = |reason,
                        blocked_reason,
                        since: Option<DateTime<Utc>>,
                        wake_at,
                        limit: Option<DateTime<Utc>>| {
        let until = deadline(since.unwrap_or(now), settings.readiness_timeout)?;
        let until = limit.map_or(until, |limit| until.min(limit));
        Ok(if now >= until {
            DeliveryStep::Blocked {
                reason: blocked_reason,
            }
        } else {
            DeliveryStep::Wait {
                until: match wake_at {
                    Some(at) => at,
                    None => deadline(now, settings.guard_poll_interval)?,
                }
                .min(until),
                reason,
            }
        })
    };
    let hold =
        |reason, blocked_reason| bounded_wait(reason, blocked_reason, live.hold_since, None, None);
    let stabilize = |reason, until| {
        bounded_wait(
            reason,
            FailureReason::ReadinessTimeout,
            live.hold_since,
            Some(until),
            None,
        )
    };
    let pane_guard = || match live.pane {
        PaneLiveness::Alive => Ok(None),
        PaneLiveness::Unknown => {
            hold(WaitReason::UnknownGuards, FailureReason::ReadinessTimeout).map(Some)
        }
        PaneLiveness::GoneSince(since) => {
            let until = deadline(since, settings.pane_gone_grace)?;
            Ok(Some(if now < until {
                stabilize(WaitReason::PaneGoneGrace, until)?
            } else if message.status == DeliveryStatus::Delivering {
                failed_attempt(now, retry_delay, FailureReason::PaneGone)?
            } else {
                DeliveryStep::Fail {
                    reason: FailureReason::PaneGone,
                }
            }))
        }
    };
    if message.status == DeliveryStatus::Delivering {
        let started = message
            .last_attempt_at
            .ok_or(DeliveryPolicyError::InvalidState)?;
        let until = deadline(started, settings.confirmation_timeout)?;
        if live.probably_landed {
            return Ok(if now < until {
                DeliveryStep::AwaitConfirmation { until }
            } else {
                DeliveryStep::Blocked {
                    reason: FailureReason::ProbablyDelivered,
                }
            });
        }
        if let Some(step) = pane_guard()? {
            return Ok(step);
        }
        return if now < until {
            Ok(DeliveryStep::AwaitConfirmation { until })
        } else {
            failed_attempt(now, retry_delay, FailureReason::ConfirmationTimeout)
        };
    }
    if attempt > 0 {
        let failed = message.failed_at.ok_or(DeliveryPolicyError::InvalidState)?;
        if live.probably_landed {
            return Ok(DeliveryStep::Blocked {
                reason: FailureReason::ProbablyDelivered,
            });
        }
        let delay = retry_delay.ok_or(DeliveryPolicyError::InvalidState)?;
        let at = deadline(failed, delay)?;
        if now < at {
            return Ok(DeliveryStep::RetryAt {
                at,
                reason: FailureReason::ConfirmationTimeout,
            });
        }
    }
    if live.owner_focused {
        return poll(WaitReason::OwnerFocused);
    }
    match live.agent_status {
        AgentStatus::Starting | AgentStatus::Clearing => {
            return bounded_wait(
                WaitReason::AgentNotReady,
                FailureReason::AgentNotReadyTimeout,
                live.agent_not_ready_since,
                None,
                None,
            );
        }
        AgentStatus::Stuck | AgentStatus::Down => {
            return Ok(DeliveryStep::Blocked {
                reason: FailureReason::AgentUnavailable,
            })
        }
        AgentStatus::Ready => {}
    }
    if live.other_delivery_in_flight {
        return poll(WaitReason::OtherDelivery);
    }
    if let Some(step) = pane_guard()? {
        return Ok(step);
    }
    let stale_at = live
        .session
        .as_ref()
        .filter(|row| row.status != SessionStatus::Idle)
        .map(|row| deadline(row.last_event_at, settings.stale_after))
        .transpose()?;
    let stale = stale_at.is_some_and(|at| now >= at);

    if !stale && (!live.send_now || !matches!(live.input_line, InputLine::EmptySince(_))) {
        if let Some(at) = stale_at {
            return Ok(DeliveryStep::Wait {
                until: deadline(now, settings.guard_poll_interval)?.min(at),
                reason: WaitReason::SessionBusy,
            });
        }
    }
    match live.copy_mode {
        Some(true) => return hold(WaitReason::CopyMode, FailureReason::CopyMode),
        None => return hold(WaitReason::UnknownGuards, FailureReason::ReadinessTimeout),
        Some(false) => {}
    }
    let editor_hold = |reason, blocked_reason| {
        let limit = if stale {
            let confirmation = settings
                .guard_poll_interval
                .checked_mul(3)
                .ok_or(DeliveryPolicyError::InvalidConfiguration)?;
            Some(deadline(
                live.stale_blocker_since.unwrap_or(now),
                confirmation,
            )?)
        } else {
            None
        };
        bounded_wait(reason, blocked_reason, live.hold_since, None, limit)
    };
    match live.dialog_open {
        Some(true) => return editor_hold(WaitReason::DialogOpen, FailureReason::DialogOpen),
        None => return hold(WaitReason::UnknownGuards, FailureReason::ReadinessTimeout),
        Some(false) => {}
    }
    let empty_since = match live.input_line {
        InputLine::NonEmpty => {
            return editor_hold(WaitReason::InputNotEmpty, FailureReason::DraftPresent)
        }
        InputLine::Unknown => {
            return hold(WaitReason::InputUnknown, FailureReason::ReadinessTimeout)
        }
        InputLine::EmptySince(since) => since.max(live.last_paste_at.unwrap_or(since)),
    };
    if now < empty_since {
        return stabilize(WaitReason::InputStabilizing, empty_since);
    }
    match &live.session {
        None => {
            let until = deadline(empty_since, settings.stable_empty_input)?;
            if now < until {
                return stabilize(WaitReason::InputStabilizing, until);
            }
        }
        Some(row) => {
            let since = if row.status == SessionStatus::Idle && !live.send_now {
                row.status_since.max(empty_since)
            } else {
                empty_since
            };
            let until = deadline(since, settings.stable_idle)?;
            if now < until {
                return stabilize(WaitReason::IdleStabilizing, until);
            }
        }
    }
    let attempt = attempt
        .checked_add(1)
        .ok_or(DeliveryPolicyError::InvalidState)?;
    Ok(DeliveryStep::PasteNow { attempt })
}

fn failed_attempt(
    now: DateTime<Utc>,
    retry_delay: Option<Duration>,
    reason: FailureReason,
) -> Result<DeliveryStep, DeliveryPolicyError> {
    Ok(if let Some(delay) = retry_delay {
        DeliveryStep::RetryAt {
            at: deadline(now, delay)?,
            reason,
        }
    } else {
        DeliveryStep::Fail { reason }
    })
}

fn deadline(at: DateTime<Utc>, duration: Duration) -> Result<DateTime<Utc>, DeliveryPolicyError> {
    let delta = TimeDelta::from_std(duration).map_err(|_| DeliveryPolicyError::TimeOverflow)?;
    at.checked_add_signed(delta)
        .ok_or(DeliveryPolicyError::TimeOverflow)
}

fn validate_settings(policy: &DeliverySettings) -> Result<u32, DeliveryPolicyError> {
    // 9999-12-31T23:59:59Z: the supported reference for configuration checks.
    let reference =
        DateTime::from_timestamp(253_402_300_799, 0).ok_or(DeliveryPolicyError::TimeOverflow)?;
    let validate = |setting, duration: Duration| {
        if duration.is_zero() || deadline(reference, duration).is_err() {
            Err(DeliveryPolicyError::InvalidDuration { setting })
        } else {
            Ok(())
        }
    };
    for (setting, duration) in [
        ("stable_idle", policy.stable_idle),
        ("stable_empty_input", policy.stable_empty_input),
        ("confirmation_timeout", policy.confirmation_timeout),
        ("stale_after", policy.stale_after),
        ("readiness_timeout", policy.readiness_timeout),
        ("guard_poll_interval", policy.guard_poll_interval),
        ("pane_gone_grace", policy.pane_gone_grace),
    ]
    .into_iter()
    .chain(policy.retry_delays.iter().map(|d| ("retry_delays", *d)))
    {
        validate(setting, duration)?;
    }
    validate(
        "guard_poll_interval",
        policy
            .guard_poll_interval
            .checked_mul(3)
            .ok_or(DeliveryPolicyError::InvalidDuration {
                setting: "guard_poll_interval",
            })?,
    )?;
    if policy
        .retry_delays
        .iter()
        .zip(policy.retry_delays.iter().skip(1))
        .any(|(a, b)| a > b)
    {
        return Err(DeliveryPolicyError::InvalidConfiguration);
    }
    u32::try_from(policy.retry_delays.len())
        .ok()
        .and_then(|n| n.checked_add(1))
        .ok_or(DeliveryPolicyError::InvalidConfiguration)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Address, DeliveryReceipt, DeliveryStatus, Message, MessageContent, MessageId, MessageKind,
        OrgCommand, OrgEvent, OrgState,
    };
    use atm_core::SessionStatus;
    use chrono::{DateTime, TimeDelta, Utc};
    use std::time::Duration;

    fn time(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + seconds, 0).unwrap()
    }

    // Synthetic: serial organisation history, not a captured harness payload.
    struct History {
        state: OrgState,
        id: MessageId,
        log: Vec<(DateTime<Utc>, OrgEvent)>,
    }

    impl History {
        fn new(to_owner: bool) -> Self {
            let mut h = Self {
                state: OrgState::default(),
                id: MessageId::new(3),
                log: vec![],
            };
            h.command(
                OrgCommand::RegisterProject {
                    project_id: "p".parse().unwrap(),
                    root: "/project".into(),
                    move_root: false,
                },
                time(0),
            );
            h.command(
                OrgCommand::Adopt {
                    agent_id: "p/worker".parse().unwrap(),
                    pane: "%1".into(),
                    session_id: Some("s1".into()),
                },
                time(0),
            );
            h.command(
                OrgCommand::SendMessage(MessageContent {
                    from: Address::Owner,
                    to: if to_owner {
                        Address::Owner
                    } else {
                        Address::Agent("p/worker".parse().unwrap())
                    },
                    kind: MessageKind::Note,
                    body: "hello".into(),
                    attachments: vec![],
                    task: None,
                }),
                time(0),
            );
            h
        }
        fn message(&self) -> &Message {
            self.state.message(&self.id).unwrap()
        }
        fn command(&mut self, command: OrgCommand, now: DateTime<Utc>) -> usize {
            let events = self.state.decide(&command, now).unwrap();
            for event in &events {
                self.state
                    .apply(self.log.len() as u64 + 1, now, event)
                    .unwrap();
                self.log.push((now, event.clone()));
            }
            events.len()
        }
        fn attempt(&mut self, attempt: u32, seconds: i64) {
            assert_eq!(
                self.command(
                    OrgCommand::DeliveryAttempted {
                        message_id: self.id,
                        attempt,
                        pane: "%1".into(),
                    },
                    time(seconds)
                ),
                1
            );
        }
        fn failure(&mut self, final_failure: bool, seconds: i64) {
            assert_eq!(
                self.command(
                    OrgCommand::DeliveryFailed {
                        message_id: self.id,
                        reason: "timeout".into(),
                        final_failure,
                    },
                    time(seconds)
                ),
                1
            );
        }
        fn step(&self, live: &RecipientLiveView, seconds: i64) -> DeliveryStep {
            next_delivery(
                self.message(),
                live,
                time(seconds),
                &DeliveryPolicy::default(),
            )
            .unwrap()
        }
    }

    fn ready() -> RecipientLiveView {
        RecipientLiveView {
            session: Some(LiveSession {
                status: SessionStatus::Idle,
                status_since: time(-10),
                last_event_at: time(0),
            }),
            pane: PaneLiveness::Alive,
            agent_status: AgentStatus::Ready,
            agent_not_ready_since: None,
            other_delivery_in_flight: false,
            last_paste_at: None,
            hold_since: Some(time(0)),
            stale_blocker_since: None,
            last_attempt_before_start: false,
            copy_mode: Some(false),
            dialog_open: Some(false),
            owner_focused: false,
            input_line: InputLine::EmptySince(time(-10)),
            send_now: false,
            confirmation: None,
            probably_landed: false,
        }
    }
    fn receipt() -> DeliveryReceipt {
        DeliveryReceipt {
            session_id: "s1".into(),
            confirmation: "prompt token".into(),
        }
    }
    fn wait(step: DeliveryStep, reason: WaitReason) -> DateTime<Utc> {
        match step {
            DeliveryStep::Wait {
                until,
                reason: actual,
            } => {
                assert_eq!(actual, reason);
                until
            }
            other => panic!("expected wait {reason:?}, got {other:?}"),
        }
    }

    // Synthetic executor model: update continuity by the consumed decision,
    // never by a changing guard tuple. Scheduling alone does not start a hold.
    fn track_hold(live: &mut RecipientLiveView, step: &DeliveryStep, now: DateTime<Utc>) {
        match step {
            DeliveryStep::PasteNow { .. }
            | DeliveryStep::Confirm { .. }
            | DeliveryStep::Complete
            | DeliveryStep::Fail { .. }
            | DeliveryStep::Wait {
                reason:
                    WaitReason::OwnerFocused
                    | WaitReason::SessionBusy
                    | WaitReason::AgentNotReady
                    | WaitReason::OtherDelivery,
                ..
            } => live.hold_since = None,
            DeliveryStep::Wait { .. } | DeliveryStep::Blocked { .. } => {
                live.hold_since.get_or_insert(now);
            }
            _ => {}
        }
    }

    #[test]
    fn changing_blockers_do_not_restart_the_hold() {
        let h = History::new(false);
        let mut live = ready();
        live.hold_since = None;
        live.input_line = InputLine::NonEmpty;
        for now in 0..3600 {
            live.dialog_open = if now % 100 == 99 { None } else { Some(false) };
            let step = h.step(&live, now);
            if now < 120 {
                assert!(matches!(step, DeliveryStep::Wait { .. }));
            } else {
                assert!(matches!(step, DeliveryStep::Blocked { .. }));
            }
            track_hold(&mut live, &step, time(now));
        }
        live.dialog_open = Some(false);
        assert_eq!(
            h.step(&live, 3600),
            DeliveryStep::Blocked {
                reason: FailureReason::DraftPresent
            }
        );
    }

    #[test]
    fn flapping_empty_input_has_a_bounded_hold() {
        let h = History::new(false);
        for row in [true, false] {
            let mut live = ready();
            if !row {
                live.session = None;
            }
            live.hold_since = None;
            // Four-second empty stretches defeat both five- and ten-second
            // stability. The review's nine-second cycle defeats idle stability.
            let cycle = if row { 9 } else { 5 };
            for now in 0..3600 {
                live.input_line = if now % cycle == 0 {
                    InputLine::Unknown
                } else {
                    InputLine::EmptySince(time(now - now % cycle + 1))
                };
                let step = h.step(&live, now);
                if now < 120 {
                    assert!(matches!(step, DeliveryStep::Wait { .. }));
                } else {
                    assert_eq!(
                        step,
                        DeliveryStep::Blocked {
                            reason: FailureReason::ReadinessTimeout
                        }
                    );
                }
                track_hold(&mut live, &step, time(now));
            }
        }
    }

    #[test]
    fn agent_not_ready_uses_the_transition_deadline() {
        let h = History::new(false);
        for status in [AgentStatus::Starting, AgentStatus::Clearing] {
            let mut live = ready();
            live.agent_status = status;
            live.agent_not_ready_since = Some(time(0));
            for now in [120, 86400] {
                assert_eq!(
                    h.step(&live, now),
                    DeliveryStep::Blocked {
                        reason: FailureReason::AgentNotReadyTimeout
                    }
                );
            }
            let before = time(120) - TimeDelta::milliseconds(1);
            assert_eq!(
                wait(
                    next_delivery(h.message(), &live, before, &DeliveryPolicy::default()).unwrap(),
                    WaitReason::AgentNotReady
                ),
                time(120)
            );
            live.owner_focused = true;
            wait(h.step(&live, 86400), WaitReason::OwnerFocused);
            live.owner_focused = false;
            live.agent_status = AgentStatus::Ready;
            assert_eq!(h.step(&live, 86400), DeliveryStep::PasteNow { attempt: 1 });
            live.agent_status = status;
            live.agent_not_ready_since = Some(time(86400));
            wait(h.step(&live, 86519), WaitReason::AgentNotReady);
            assert_eq!(
                h.step(&live, 86520),
                DeliveryStep::Blocked {
                    reason: FailureReason::AgentNotReadyTimeout
                }
            );
            live.agent_not_ready_since = None;
            wait(h.step(&live, 100000), WaitReason::AgentNotReady);
            live.agent_not_ready_since = Some(time(100100));
            wait(h.step(&live, 100000), WaitReason::AgentNotReady);
        }
    }

    #[test]
    fn agent_non_ready_time_survives_status_and_metadata_changes() {
        let mut h = History::new(false);
        let id: crate::AgentId = "p/worker".parse().unwrap();
        h.command(
            OrgCommand::Clear {
                agent_id: id.clone(),
            },
            time(0),
        );
        h.command(
            OrgCommand::Adopt {
                agent_id: id.clone(),
                pane: "%1".into(),
                session_id: None,
            },
            time(10),
        );
        h.command(
            OrgCommand::Stuck {
                agent_id: id.clone(),
                reason: "waiting".into(),
            },
            time(20),
        );
        h.command(
            OrgCommand::Adopt {
                agent_id: id.clone(),
                pane: "%1".into(),
                session_id: None,
            },
            time(30),
        );
        // Synthetic replayed metadata report: apply accepts late facts even if
        // today's decide would deduplicate the already recorded pane.
        h.log.push((
            time(90),
            OrgEvent::AgentPaneCreated {
                agent_id: id.clone(),
                pane: "%1".into(),
            },
        ));
        let mut state = OrgState::default();
        let mut since = None;
        for (index, (ts, event)) in h.log.iter().enumerate() {
            let before = state.agent(&id).map(|agent| agent.status);
            state.apply(index as u64 + 1, *ts, event).unwrap();
            if let Some(agent) = state.agent(&id) {
                if agent.status == AgentStatus::Ready {
                    since = None;
                } else if before.is_none_or(|status| status == AgentStatus::Ready) {
                    since = Some(*ts);
                }
            }
        }
        let agent = state.agent(&id).unwrap();
        assert_eq!(agent.updated_at, time(90));
        assert_eq!(since, Some(time(0)));
        let mut live = ready();
        live.agent_status = agent.status;
        live.agent_not_ready_since = since;
        // Recent message holds and metadata do not buy a new agent window.
        live.hold_since = Some(time(119));
        assert_eq!(
            h.step(&live, 120),
            DeliveryStep::Blocked {
                reason: FailureReason::AgentNotReadyTimeout
            }
        );
        live.agent_status = AgentStatus::Clearing;
        assert_eq!(
            h.step(&live, 121),
            DeliveryStep::Blocked {
                reason: FailureReason::AgentNotReadyTimeout
            }
        );
    }

    #[test]
    fn expired_hold_allows_only_fully_stable_recovery() {
        let h = History::new(false);
        let mut live = ready();
        live.input_line = InputLine::EmptySince(time(119));
        assert_eq!(
            h.step(&live, 120),
            DeliveryStep::Blocked {
                reason: FailureReason::ReadinessTimeout
            }
        );
        assert_eq!(h.step(&live, 129), DeliveryStep::PasteNow { attempt: 1 });
        live.input_line = InputLine::EmptySince(time(500));
        assert_eq!(
            h.step(&live, 120),
            DeliveryStep::Blocked {
                reason: FailureReason::ReadinessTimeout
            }
        );
        live.hold_since = Some(time(600));
        assert_eq!(
            wait(h.step(&live, 120), WaitReason::InputStabilizing),
            time(500)
        );
        live.hold_since = None;
        assert_eq!(
            wait(h.step(&live, 120), WaitReason::InputStabilizing),
            time(240)
        );
    }

    #[test]
    fn pane_gone_grace_does_not_restart_an_expired_hold() {
        let h = History::new(false);
        let mut live = ready();
        live.pane = PaneLiveness::GoneSince(time(119));
        assert_eq!(
            wait(h.step(&live, 119), WaitReason::PaneGoneGrace),
            time(120)
        );
        assert_eq!(
            h.step(&live, 120),
            DeliveryStep::Blocked {
                reason: FailureReason::ReadinessTimeout
            }
        );
        assert_eq!(
            h.step(&live, 121),
            DeliveryStep::Fail {
                reason: FailureReason::PaneGone
            }
        );
    }

    #[test]
    fn hold_resets_only_on_paste_or_non_hold_wait() {
        let h = History::new(false);
        for reason in [
            WaitReason::OwnerFocused,
            WaitReason::SessionBusy,
            WaitReason::AgentNotReady,
            WaitReason::OtherDelivery,
        ] {
            let mut live = ready();
            live.hold_since = Some(time(0));
            match reason {
                WaitReason::OwnerFocused => live.owner_focused = true,
                WaitReason::SessionBusy => {
                    let row = live.session.as_mut().unwrap();
                    row.status = SessionStatus::Working;
                    row.last_event_at = time(119);
                }
                WaitReason::AgentNotReady => live.agent_status = AgentStatus::Starting,
                WaitReason::OtherDelivery => live.other_delivery_in_flight = true,
                _ => unreachable!(),
            }
            let step = h.step(&live, 119);
            wait(step.clone(), reason);
            track_hold(&mut live, &step, time(119));
            let reset = live.hold_since;
            live = ready();
            live.hold_since = reset;
            live.input_line = InputLine::NonEmpty;
            let step = h.step(&live, 120);
            wait(step.clone(), WaitReason::InputNotEmpty);
            track_hold(&mut live, &step, time(120));
            wait(h.step(&live, 239), WaitReason::InputNotEmpty);
            assert_eq!(
                h.step(&live, 240),
                DeliveryStep::Blocked {
                    reason: FailureReason::DraftPresent
                }
            );
            live = ready();
            let step = h.step(&live, 241);
            assert_eq!(step, DeliveryStep::PasteNow { attempt: 1 });
            track_hold(&mut live, &step, time(241));
            assert_eq!(live.hold_since, None);
        }
    }

    #[test]
    fn queued_final_failure_ends_a_hold_and_accepts_late_receipt() {
        let mut h = History::new(false);
        assert!(h
            .state
            .decide(
                &OrgCommand::DeliveryFailed {
                    message_id: h.id,
                    reason: "give up".into(),
                    final_failure: false,
                },
                time(0)
            )
            .is_err());
        h.failure(true, 0);
        assert_eq!(h.message().status, DeliveryStatus::Failed);
        assert_eq!(
            h.command(
                OrgCommand::DeliveryFailed {
                    message_id: h.id,
                    reason: "timeout".into(),
                    final_failure: true,
                },
                time(1)
            ),
            0
        );
        let mut live = ready();
        live.confirmation = Some(receipt());
        assert_eq!(
            h.step(&live, 2),
            DeliveryStep::Confirm { receipt: receipt() }
        );
    }

    #[test]
    fn row_readiness_requires_stable_empty_input_even_when_status_is_old() {
        let h = History::new(false);
        for status in [
            SessionStatus::Idle,
            SessionStatus::Working,
            SessionStatus::AttentionNeeded,
        ] {
            let mut live = ready();
            live.session.as_mut().unwrap().status = status;
            live.session.as_mut().unwrap().last_event_at = time(-500);
            live.input_line = InputLine::EmptySince(time(31));
            assert!(matches!(h.step(&live, 31), DeliveryStep::Wait { .. }));
            assert!(matches!(h.step(&live, 40), DeliveryStep::Wait { .. }));
            assert_eq!(h.step(&live, 41), DeliveryStep::PasteNow { attempt: 1 });
        }
    }

    #[test]
    fn pane_serialization_and_last_paste_reset_readiness() {
        let h = History::new(false);
        let mut live = ready();
        live.other_delivery_in_flight = true;
        wait(h.step(&live, 0), WaitReason::OtherDelivery);
        live.other_delivery_in_flight = false;
        live.last_paste_at = Some(time(0));
        for send_now in [false, true] {
            live.send_now = send_now;
            assert!(matches!(h.step(&live, 9), DeliveryStep::Wait { .. }));
            assert_eq!(h.step(&live, 10), DeliveryStep::PasteNow { attempt: 1 });
        }
        live.session = None;
        wait(h.step(&live, 4), WaitReason::InputStabilizing);
        assert_eq!(h.step(&live, 5), DeliveryStep::PasteNow { attempt: 1 });
    }

    #[test]
    fn agent_state_gates_and_recovers_without_message_events() {
        let h = History::new(false);
        let mut live = ready();
        for status in [AgentStatus::Starting, AgentStatus::Clearing] {
            live.agent_status = status;
            live.agent_not_ready_since = Some(time(0));
            wait(h.step(&live, 119), WaitReason::AgentNotReady);
            assert_eq!(
                h.step(&live, 1000),
                DeliveryStep::Blocked {
                    reason: FailureReason::AgentNotReadyTimeout
                }
            );
        }
        for status in [AgentStatus::Stuck, AgentStatus::Down] {
            live.agent_status = status;
            assert_eq!(
                h.step(&live, 1000),
                DeliveryStep::Blocked {
                    reason: FailureReason::AgentUnavailable
                }
            );
        }
        live.agent_status = AgentStatus::Ready;
        assert_eq!(h.step(&live, 1000), DeliveryStep::PasteNow { attempt: 1 });
        assert_eq!(History::new(true).step(&live, 1000), DeliveryStep::Complete);
    }

    #[test]
    fn holds_are_bounded_from_current_observation_not_message_age() {
        let h = History::new(false);
        for which in 0..7 {
            let mut live = ready();
            live.hold_since = Some(time(600));
            let (wait_reason, blocked_reason) = match which {
                0 => {
                    live.copy_mode = Some(true);
                    (WaitReason::CopyMode, FailureReason::CopyMode)
                }
                1 => {
                    live.dialog_open = Some(true);
                    (WaitReason::DialogOpen, FailureReason::DialogOpen)
                }
                2 => {
                    live.input_line = InputLine::NonEmpty;
                    (WaitReason::InputNotEmpty, FailureReason::DraftPresent)
                }
                3 => {
                    live.input_line = InputLine::Unknown;
                    (WaitReason::InputUnknown, FailureReason::ReadinessTimeout)
                }
                4 => {
                    live.copy_mode = None;
                    (WaitReason::UnknownGuards, FailureReason::ReadinessTimeout)
                }
                5 => {
                    live.dialog_open = None;
                    (WaitReason::UnknownGuards, FailureReason::ReadinessTimeout)
                }
                _ => {
                    live.pane = PaneLiveness::Unknown;
                    (WaitReason::UnknownGuards, FailureReason::ReadinessTimeout)
                }
            };
            wait(h.step(&live, 600), wait_reason);
            wait(h.step(&live, 719), wait_reason);
            assert_eq!(
                h.step(&live, 720),
                DeliveryStep::Blocked {
                    reason: blocked_reason
                }
            );
            // A paste-ready recovery ends the hold, not just one empty sample.
            assert_eq!(h.step(&ready(), 721), DeliveryStep::PasteNow { attempt: 1 });
            live.hold_since = Some(time(722));
            wait(h.step(&live, 722), wait_reason);
            live.owner_focused = true;
            wait(h.step(&live, 100000), WaitReason::OwnerFocused);
        }
    }

    #[test]
    fn gone_pane_requires_grace_unknown_never_fails_an_attempt() {
        let mut h = History::new(false);
        let mut live = ready();
        live.pane = PaneLiveness::GoneSince(time(0));
        wait(h.step(&live, 1), WaitReason::PaneGoneGrace);
        assert_eq!(
            h.step(&live, 2),
            DeliveryStep::Fail {
                reason: FailureReason::PaneGone
            }
        );
        h.attempt(1, 0);
        wait(h.step(&live, 1), WaitReason::PaneGoneGrace);
        assert_eq!(
            h.step(&live, 2),
            DeliveryStep::RetryAt {
                at: time(3),
                reason: FailureReason::PaneGone
            }
        );
        live.pane = PaneLiveness::Unknown;
        live.hold_since = Some(time(2));
        wait(h.step(&live, 3), WaitReason::UnknownGuards);
        assert_eq!(
            h.step(&live, 122),
            DeliveryStep::Blocked {
                reason: FailureReason::ReadinessTimeout
            }
        );
        live.pane = PaneLiveness::Alive;
        assert!(matches!(h.step(&live, 123), DeliveryStep::RetryAt { .. }));
    }

    #[test]
    fn restart_after_late_queued_evidence_must_not_authorize_another_paste() {
        let mut h = History::new(false);
        h.attempt(1, 0);
        h.failure(false, 3);
        let mut live = ready();
        live.probably_landed = true; // late activity after the timeout was logged
        assert_eq!(
            h.step(&live, 4),
            DeliveryStep::Blocked {
                reason: FailureReason::ProbablyDelivered
            }
        );
        let mut replayed = OrgState::default();
        for (index, (ts, event)) in h.log.iter().enumerate() {
            replayed.apply(index as u64 + 1, *ts, event).unwrap();
        }
        h.state = replayed;
        live.probably_landed = false;
        live.confirmation = None;
        live.last_attempt_before_start = h.message().last_attempt.is_some();
        live.last_paste_at = Some(time(5));
        live.input_line = InputLine::EmptySince(time(5));
        live.session.as_mut().unwrap().status_since = time(5);
        assert_eq!(
            h.step(&live, 15),
            DeliveryStep::Blocked {
                reason: FailureReason::Restarted
            }
        );
        live.confirmation = Some(receipt());
        assert_eq!(
            h.step(&live, 15),
            DeliveryStep::Confirm { receipt: receipt() }
        );
    }

    #[test]
    fn restart_hold_survives_budget_changes_until_owner_gives_up() {
        for queued in [false, true] {
            let mut h = History::new(false);
            h.attempt(2, 0);
            if queued {
                h.failure(false, 3);
            }
            let mut live = ready();
            live.last_attempt_before_start = true;
            let policy = DeliveryPolicy::new(DeliverySettings {
                retry_delays: vec![],
                ..DeliverySettings::default()
            })
            .unwrap();
            for now in [4, 10000] {
                assert_eq!(
                    next_delivery(h.message(), &live, time(now), &policy).unwrap(),
                    DeliveryStep::Blocked {
                        reason: FailureReason::Restarted
                    }
                );
            }
            h.failure(true, 10000); // explicit owner give-up, not a policy timeout
            assert_eq!(h.step(&live, 10001), DeliveryStep::Complete);
            live.confirmation = Some(receipt());
            assert_eq!(
                h.step(&live, 10002),
                DeliveryStep::Confirm { receipt: receipt() }
            );
        }
        // Never attempted messages are still eligible after recovery.
        assert_eq!(
            History::new(false).step(&ready(), 10000),
            DeliveryStep::PasteNow { attempt: 1 }
        );
    }

    #[test]
    fn restart_loses_latches_and_blocks_until_receipt() {
        let mut h = History::new(false);
        h.attempt(1, 0);
        let mut live = ready();
        live.last_attempt_before_start = true;
        for now in [1, 3, 10000] {
            assert_eq!(
                h.step(&live, now),
                DeliveryStep::Blocked {
                    reason: FailureReason::Restarted
                }
            );
        }
        live.confirmation = Some(receipt());
        assert_eq!(
            h.step(&live, 10000),
            DeliveryStep::Confirm { receipt: receipt() }
        );
    }

    #[test]
    fn stale_working_activity_in_receive_order_prevents_retry_despite_backward_clock() {
        let mut h = History::new(false);
        let mut live = ready();
        live.session.as_mut().unwrap().status = SessionStatus::Working;
        live.session.as_mut().unwrap().last_event_at = time(-500);
        assert_eq!(h.step(&live, 0), DeliveryStep::PasteNow { attempt: 1 });
        h.attempt(1, 0);
        // Executor received another working event after applying the attempt,
        // despite its timestamp preceding it: arrival order sets the latch.
        live.session.as_mut().unwrap().last_event_at = time(-1);
        live.probably_landed = true;
        live.pane = PaneLiveness::GoneSince(time(0));
        assert_eq!(
            h.step(&live, 2),
            DeliveryStep::AwaitConfirmation { until: time(3) }
        );
        assert_eq!(
            h.step(&live, 3),
            DeliveryStep::Blocked {
                reason: FailureReason::ProbablyDelivered
            }
        );
        live.session.as_mut().unwrap().status = SessionStatus::Idle;
        assert_eq!(
            h.step(&live, 70),
            DeliveryStep::Blocked {
                reason: FailureReason::ProbablyDelivered
            }
        );
    }

    #[test]
    fn shrinking_budget_finalizes_queued_or_over_budget_delivering() {
        for queued in [false, true] {
            let mut h = History::new(false);
            h.attempt(2, 0);
            if queued {
                h.failure(false, 1);
            }
            let policy = DeliveryPolicy::new(DeliverySettings {
                retry_delays: vec![],
                ..DeliverySettings::default()
            })
            .unwrap();
            assert_eq!(
                next_delivery(h.message(), &ready(), time(2), &policy).unwrap(),
                DeliveryStep::Fail {
                    reason: FailureReason::AttemptLimit
                }
            );
            h.failure(true, 2);
            assert_eq!(h.step(&ready(), 3), DeliveryStep::Complete);
        }
    }

    #[test]
    fn settings_validate_at_construction_and_deserialization() {
        let settings = DeliverySettings::default();
        let policy = DeliveryPolicy::new(settings.clone()).unwrap();
        assert_eq!(
            serde_json::from_str::<DeliveryPolicy>(&serde_json::to_string(&policy).unwrap())
                .unwrap(),
            policy
        );
        for (bad, error) in [
            (
                DeliverySettings {
                    stable_idle: Duration::ZERO,
                    ..settings.clone()
                },
                DeliveryPolicyError::InvalidDuration {
                    setting: "stable_idle",
                },
            ),
            (
                DeliverySettings {
                    confirmation_timeout: Duration::MAX,
                    ..settings.clone()
                },
                DeliveryPolicyError::InvalidDuration {
                    setting: "confirmation_timeout",
                },
            ),
            (
                DeliverySettings {
                    retry_delays: vec![Duration::from_secs(2), Duration::from_secs(1)],
                    ..settings.clone()
                },
                DeliveryPolicyError::InvalidConfiguration,
            ),
            (
                DeliverySettings {
                    pane_gone_grace: Duration::ZERO,
                    ..settings.clone()
                },
                DeliveryPolicyError::InvalidDuration {
                    setting: "pane_gone_grace",
                },
            ),
        ] {
            assert_eq!(DeliveryPolicy::new(bad.clone()), Err(error));
            assert!(
                serde_json::from_str::<DeliveryPolicy>(&serde_json::to_string(&bad).unwrap())
                    .is_err()
            );
        }
        assert!(serde_json::from_str::<DeliveryPolicy>(r#"{"stable_idel": 3}"#).is_err());
        let minimal: DeliveryPolicy = serde_json::from_str("{}").unwrap();
        assert_eq!(minimal, policy);
    }

    fn settings_with_duration(duration: Duration) -> [(&'static str, DeliverySettings); 8] {
        [
            (
                "stable_idle",
                DeliverySettings {
                    stable_idle: duration,
                    ..DeliverySettings::default()
                },
            ),
            (
                "stable_empty_input",
                DeliverySettings {
                    stable_empty_input: duration,
                    ..DeliverySettings::default()
                },
            ),
            (
                "confirmation_timeout",
                DeliverySettings {
                    confirmation_timeout: duration,
                    ..DeliverySettings::default()
                },
            ),
            (
                "retry_delays",
                DeliverySettings {
                    retry_delays: vec![duration],
                    ..DeliverySettings::default()
                },
            ),
            (
                "stale_after",
                DeliverySettings {
                    stale_after: duration,
                    ..DeliverySettings::default()
                },
            ),
            (
                "readiness_timeout",
                DeliverySettings {
                    readiness_timeout: duration,
                    ..DeliverySettings::default()
                },
            ),
            (
                "guard_poll_interval",
                DeliverySettings {
                    guard_poll_interval: duration,
                    ..DeliverySettings::default()
                },
            ),
            (
                "pane_gone_grace",
                DeliverySettings {
                    pane_gone_grace: duration,
                    ..DeliverySettings::default()
                },
            ),
        ]
    }

    fn assert_invalid_duration(setting: &'static str, settings: DeliverySettings) {
        let error = DeliveryPolicy::new(settings.clone()).unwrap_err();
        assert_eq!(error, DeliveryPolicyError::InvalidDuration { setting });
        assert!(error.to_string().contains(setting), "{setting}: {error}");
        let error =
            serde_json::from_str::<DeliveryPolicy>(&serde_json::to_string(&settings).unwrap())
                .unwrap_err();
        assert!(error.to_string().contains(setting), "{setting}: {error}");
    }

    #[test]
    fn overflowing_settings_are_rejected_with_their_names() {
        // A TimeDelta can represent 9e12 seconds, but a delivery deadline cannot.
        for duration in [
            Duration::from_secs(9_000_000_000_000),
            Duration::ZERO,
            Duration::MAX,
        ] {
            for (setting, settings) in settings_with_duration(duration) {
                assert_invalid_duration(setting, settings);
            }
        }
    }

    #[test]
    fn duration_validation_checks_the_reference_boundary() {
        let reference = "9999-12-31T23:59:59Z".parse::<DateTime<Utc>>().unwrap();
        let maximum = DateTime::<Utc>::MAX_UTC
            .signed_duration_since(reference)
            .to_std()
            .unwrap();
        for (setting, settings) in settings_with_duration(maximum) {
            if setting == "guard_poll_interval" {
                continue; // The derived three-poll boundary is checked separately.
            }
            let policy = DeliveryPolicy::new(settings).unwrap();
            assert_eq!(
                serde_json::from_str::<DeliveryPolicy>(&serde_json::to_string(&policy).unwrap())
                    .unwrap(),
                policy,
            );
        }
        for (setting, settings) in settings_with_duration(maximum + Duration::from_nanos(1)) {
            assert_invalid_duration(setting, settings);
        }
    }

    #[test]
    fn three_poll_confirmation_must_also_fit() {
        let reference = "9999-12-31T23:59:59Z".parse::<DateTime<Utc>>().unwrap();
        let maximum = DateTime::<Utc>::MAX_UTC
            .signed_duration_since(reference)
            .to_std()
            .unwrap();
        let poll = maximum / 3;
        let settings = DeliverySettings {
            guard_poll_interval: poll,
            ..DeliverySettings::default()
        };
        let policy = DeliveryPolicy::new(settings.clone()).unwrap();
        assert_eq!(
            serde_json::from_str::<DeliveryPolicy>(&serde_json::to_string(&policy).unwrap())
                .unwrap(),
            policy,
        );
        let too_large = poll + Duration::from_nanos(1);
        assert!(reference
            .checked_add_signed(TimeDelta::from_std(too_large).unwrap())
            .is_some());
        assert_invalid_duration(
            "guard_poll_interval",
            DeliverySettings {
                guard_poll_interval: too_large,
                ..settings
            },
        );
    }

    #[test]
    fn stable_idle_boundary_and_retry_window() {
        let h = History::new(false);
        let mut live = ready();
        live.session.as_mut().unwrap().status_since = time(0);
        assert_eq!(
            wait(h.step(&live, 9), WaitReason::IdleStabilizing),
            time(10)
        );
        assert_eq!(h.step(&live, 10), DeliveryStep::PasteNow { attempt: 1 });
        live.session.as_mut().unwrap().status = SessionStatus::Working;
        wait(h.step(&live, 11), WaitReason::SessionBusy);
        live.session.as_mut().unwrap().status = SessionStatus::Idle;
        live.session.as_mut().unwrap().status_since = time(12);
        assert_eq!(
            wait(h.step(&live, 21), WaitReason::IdleStabilizing),
            time(22)
        );
        assert_eq!(h.step(&live, 22), DeliveryStep::PasteNow { attempt: 1 });
    }

    #[test]
    fn focused_pane_never_pastes_even_after_turn_end() {
        let h = History::new(false);
        let mut live = ready();
        live.owner_focused = true;
        for send_now in [false, true] {
            live.send_now = send_now;
            wait(h.step(&live, 0), WaitReason::OwnerFocused);
            live.session.as_mut().unwrap().status_since = time(1);
            wait(h.step(&live, 20), WaitReason::OwnerFocused);
        }
        // Selected in an unfocused client is normalized to false by the executor.
        live.owner_focused = false;
        assert_eq!(h.step(&live, 20), DeliveryStep::PasteNow { attempt: 1 });
        live.input_line = InputLine::NonEmpty;
        wait(h.step(&live, 20), WaitReason::InputNotEmpty);
    }

    #[test]
    fn send_now_waits_without_notice_during_a_fresh_turn() {
        let h = History::new(false);
        for status in [SessionStatus::Working, SessionStatus::AttentionNeeded] {
            for send_now in [false, true] {
                for input in [InputLine::Unknown, InputLine::NonEmpty] {
                    let mut live = ready();
                    live.send_now = send_now;
                    live.input_line = input;
                    live.session.as_mut().unwrap().status = status;
                    for now in 0..=300 {
                        live.session.as_mut().unwrap().last_event_at = time(now);
                        let step = h.step(&live, now);
                        wait(step.clone(), WaitReason::SessionBusy);
                        track_hold(&mut live, &step, time(now));
                    }
                    live.input_line = InputLine::EmptySince(time(290));
                    if send_now {
                        assert_eq!(h.step(&live, 300), DeliveryStep::PasteNow { attempt: 1 });
                        live.copy_mode = Some(true);
                        wait(h.step(&live, 300), WaitReason::CopyMode);
                        live.copy_mode = Some(false);
                        live.dialog_open = Some(true);
                        wait(h.step(&live, 300), WaitReason::DialogOpen);
                    } else {
                        wait(h.step(&live, 300), WaitReason::SessionBusy);
                    }
                }
            }
        }
    }

    #[test]
    fn stale_draft_and_dialog_notify_after_three_polls() {
        let h = History::new(false);
        for (poll_ms, confirmation_ms) in [(250, 750), (400, 1200)] {
            let policy = DeliveryPolicy::new(DeliverySettings {
                guard_poll_interval: Duration::from_millis(poll_ms),
                ..DeliverySettings::default()
            })
            .unwrap();
            for status in [SessionStatus::Working, SessionStatus::AttentionNeeded] {
                for send_now in [false, true] {
                    for dialog in [false, true] {
                        let mut live = ready();
                        live.session.as_mut().unwrap().status = status;
                        live.send_now = send_now;
                        live.hold_since = Some(time(120));
                        live.stale_blocker_since = Some(time(120));
                        live.dialog_open = Some(dialog);
                        live.input_line = if dialog {
                            InputLine::EmptySince(time(0))
                        } else {
                            InputLine::NonEmpty
                        };
                        let reason = if dialog {
                            WaitReason::DialogOpen
                        } else {
                            WaitReason::InputNotEmpty
                        };
                        let failure = if dialog {
                            FailureReason::DialogOpen
                        } else {
                            FailureReason::DraftPresent
                        };
                        wait(
                            next_delivery(h.message(), &live, time(120), &policy).unwrap(),
                            reason,
                        );
                        let deadline = time(120) + TimeDelta::milliseconds(confirmation_ms);
                        assert_eq!(
                            wait(
                                next_delivery(
                                    h.message(),
                                    &live,
                                    deadline - TimeDelta::nanoseconds(1),
                                    &policy
                                )
                                .unwrap(),
                                reason
                            ),
                            deadline
                        );
                        assert_eq!(
                            next_delivery(h.message(), &live, deadline, &policy).unwrap(),
                            DeliveryStep::Blocked { reason: failure }
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn stale_unknown_keeps_the_full_readiness_bound() {
        let h = History::new(false);
        for which in 0..4 {
            let mut live = ready();
            live.session.as_mut().unwrap().status = SessionStatus::Working;
            live.hold_since = Some(time(120));
            live.stale_blocker_since = Some(time(0)); // unrelated old draft clock
            let reason = match which {
                0 => {
                    live.input_line = InputLine::Unknown;
                    WaitReason::InputUnknown
                }
                1 => {
                    live.dialog_open = None;
                    WaitReason::UnknownGuards
                }
                2 => {
                    live.copy_mode = None;
                    WaitReason::UnknownGuards
                }
                _ => {
                    live.pane = PaneLiveness::Unknown;
                    WaitReason::UnknownGuards
                }
            };
            for now in [
                time(120),
                time(120) + TimeDelta::milliseconds(750),
                time(240) - TimeDelta::nanoseconds(1),
            ] {
                wait(
                    next_delivery(h.message(), &live, now, &DeliveryPolicy::default()).unwrap(),
                    reason,
                );
            }
            assert_eq!(
                h.step(&live, 240),
                DeliveryStep::Blocked {
                    reason: FailureReason::ReadinessTimeout
                }
            );
        }
    }

    #[test]
    fn stale_confirmation_does_not_borrow_other_blocker_history() {
        let h = History::new(false);
        let policy = DeliveryPolicy::default();
        let mut live = ready();
        live.session.as_mut().unwrap().status = SessionStatus::Working;
        live.hold_since = Some(time(120));
        live.dialog_open = Some(true);
        live.stale_blocker_since = Some(time(120));
        wait(h.step(&live, 120), WaitReason::DialogOpen);
        // Draft replaces the dialog halfway through its short confirmation.
        live.dialog_open = Some(false);
        live.input_line = InputLine::NonEmpty;
        live.stale_blocker_since = Some(time(120) + TimeDelta::milliseconds(500));
        wait(
            next_delivery(
                h.message(),
                &live,
                time(120) + TimeDelta::milliseconds(750),
                &policy,
            )
            .unwrap(),
            WaitReason::InputNotEmpty,
        );
        assert_eq!(
            next_delivery(
                h.message(),
                &live,
                time(121) + TimeDelta::milliseconds(250),
                &policy
            )
            .unwrap(),
            DeliveryStep::Blocked {
                reason: FailureReason::DraftPresent
            }
        );
        // An unknown sample interrupts the short confirmation, not the hold.
        live.input_line = InputLine::Unknown;
        live.stale_blocker_since = None;
        wait(h.step(&live, 122), WaitReason::InputUnknown);
        live.input_line = InputLine::NonEmpty;
        wait(h.step(&live, 123), WaitReason::InputNotEmpty);
        live.stale_blocker_since = Some(time(123));
        assert_eq!(
            next_delivery(
                h.message(),
                &live,
                time(123) + TimeDelta::milliseconds(750),
                &policy
            )
            .unwrap(),
            DeliveryStep::Blocked {
                reason: FailureReason::DraftPresent
            }
        );
        live.stale_blocker_since = Some(time(125));
        // A future clock never shortens confirmation.
        wait(h.step(&live, 124), WaitReason::InputNotEmpty);
        // A fresh event is a normal busy wait; a later stale observation starts
        // both clocks anew, rather than inheriting this old draft's timestamp.
        live.session.as_mut().unwrap().last_event_at = time(124);
        let step = h.step(&live, 124);
        wait(step.clone(), WaitReason::SessionBusy);
        track_hold(&mut live, &step, time(124));
        live.stale_blocker_since = None;
        wait(h.step(&live, 244), WaitReason::InputNotEmpty);
        live.hold_since = Some(time(244));
        live.stale_blocker_since = Some(time(244));
        assert_eq!(
            next_delivery(
                h.message(),
                &live,
                time(244) + TimeDelta::milliseconds(750),
                &policy
            )
            .unwrap(),
            DeliveryStep::Blocked {
                reason: FailureReason::DraftPresent
            }
        );
    }

    #[test]
    fn stale_flapping_blockers_keep_the_overall_bound() {
        let h = History::new(false);
        let mut live = ready();
        live.session.as_mut().unwrap().status = SessionStatus::Working;
        live.hold_since = Some(time(120));
        for poll in 0..=480 {
            let now = time(120) + TimeDelta::milliseconds(poll * 250);
            live.dialog_open = Some(poll % 3 == 0);
            live.input_line = if poll % 3 == 1 {
                InputLine::Unknown
            } else {
                InputLine::NonEmpty
            };
            live.stale_blocker_since = Some(now); // no blocker lasts even two polls
            let step = next_delivery(h.message(), &live, now, &DeliveryPolicy::default()).unwrap();
            if poll < 480 {
                assert!(matches!(step, DeliveryStep::Wait { .. }));
            } else {
                assert_eq!(
                    step,
                    DeliveryStep::Blocked {
                        reason: FailureReason::DialogOpen
                    }
                );
            }
        }
    }

    #[test]
    fn non_stale_editors_ignore_the_short_confirmation_clock() {
        let h = History::new(false);
        for row in [false, true] {
            for dialog in [false, true] {
                let mut live = ready();
                if !row {
                    live.session = None;
                }
                live.hold_since = Some(time(120));
                live.stale_blocker_since = Some(time(0));
                live.dialog_open = Some(dialog);
                live.input_line = InputLine::NonEmpty;
                let reason = if dialog {
                    WaitReason::DialogOpen
                } else {
                    WaitReason::InputNotEmpty
                };
                wait(h.step(&live, 121), reason);
                wait(h.step(&live, 239), reason);
                assert_eq!(
                    h.step(&live, 240),
                    DeliveryStep::Blocked {
                        reason: if dialog {
                            FailureReason::DialogOpen
                        } else {
                            FailureReason::DraftPresent
                        },
                    }
                );
            }
        }
    }

    #[test]
    fn send_now_only_bypasses_idle_and_each_guard_still_vetoes() {
        let h = History::new(false);
        for status in [
            SessionStatus::Idle,
            SessionStatus::Working,
            SessionStatus::AttentionNeeded,
        ] {
            let mut live = ready();
            live.session.as_mut().unwrap().status = status;
            live.session.as_mut().unwrap().status_since = time(0);
            live.send_now = true;
            assert_eq!(h.step(&live, 0), DeliveryStep::PasteNow { attempt: 1 });
            for (copy, dialog, input, reason) in [
                (
                    Some(true),
                    Some(false),
                    InputLine::EmptySince(time(0)),
                    WaitReason::CopyMode,
                ),
                (
                    None,
                    Some(false),
                    InputLine::EmptySince(time(0)),
                    WaitReason::UnknownGuards,
                ),
                (
                    Some(false),
                    Some(true),
                    InputLine::EmptySince(time(0)),
                    WaitReason::DialogOpen,
                ),
                (
                    Some(false),
                    None,
                    InputLine::EmptySince(time(0)),
                    WaitReason::UnknownGuards,
                ),
                (
                    Some(false),
                    Some(false),
                    InputLine::NonEmpty,
                    WaitReason::InputNotEmpty,
                ),
                (
                    Some(false),
                    Some(false),
                    InputLine::Unknown,
                    WaitReason::InputUnknown,
                ),
            ] {
                let mut blocked = live.clone();
                blocked.copy_mode = copy;
                blocked.dialog_open = dialog;
                blocked.input_line = input;
                if status != SessionStatus::Idle && !matches!(input, InputLine::EmptySince(_)) {
                    wait(h.step(&blocked, 0), WaitReason::SessionBusy);
                } else {
                    wait(h.step(&blocked, 0), reason);
                }
            }
            live.pane = PaneLiveness::GoneSince(time(-2));
            assert_eq!(
                h.step(&live, 0),
                DeliveryStep::Fail {
                    reason: FailureReason::PaneGone
                }
            );
        }
    }

    #[test]
    fn stale_rows_use_the_pane_not_the_status() {
        let h = History::new(false);
        for status in [SessionStatus::Working, SessionStatus::AttentionNeeded] {
            let mut live = ready();
            live.session.as_mut().unwrap().status = status;
            wait(h.step(&live, 119), WaitReason::SessionBusy);
            assert_eq!(h.step(&live, 120), DeliveryStep::PasteNow { attempt: 1 });
            for send_now in [false, true] {
                live.send_now = send_now;
                live.hold_since = Some(time(120));
                live.stale_blocker_since = Some(time(120));
                live.dialog_open = Some(false);
                live.input_line = InputLine::NonEmpty;
                wait(h.step(&live, 120), WaitReason::InputNotEmpty);
                assert_eq!(
                    next_delivery(
                        h.message(),
                        &live,
                        time(120) + TimeDelta::milliseconds(750),
                        &DeliveryPolicy::default()
                    )
                    .unwrap(),
                    DeliveryStep::Blocked {
                        reason: FailureReason::DraftPresent
                    }
                );
                live.input_line = InputLine::EmptySince(time(0));
                live.dialog_open = Some(true);
                live.stale_blocker_since = Some(time(121));
                wait(h.step(&live, 121), WaitReason::DialogOpen);
                assert_eq!(
                    next_delivery(
                        h.message(),
                        &live,
                        time(121) + TimeDelta::milliseconds(750),
                        &DeliveryPolicy::default()
                    )
                    .unwrap(),
                    DeliveryStep::Blocked {
                        reason: FailureReason::DialogOpen
                    }
                );
                live.dialog_open = Some(false);
                live.input_line = InputLine::Unknown;
                live.stale_blocker_since = None;
                wait(h.step(&live, 239), WaitReason::InputUnknown);
                assert_eq!(
                    h.step(&live, 240),
                    DeliveryStep::Blocked {
                        reason: FailureReason::ReadinessTimeout
                    }
                );
            }
            live.send_now = false;
            live.input_line = InputLine::EmptySince(time(0));
            live.session.as_mut().unwrap().last_event_at = time(100);
            wait(h.step(&live, 120), WaitReason::SessionBusy);
            assert_eq!(h.step(&live, 220), DeliveryStep::PasteNow { attempt: 1 });
        }
    }

    #[test]
    fn missing_row_bootstraps_only_from_stable_empty_editor() {
        let h = History::new(false);
        let mut live = ready();
        live.session = None;
        for send_now in [false, true] {
            live.send_now = send_now;
            live.input_line = InputLine::Unknown;
            wait(h.step(&live, 119), WaitReason::InputUnknown);
            assert_eq!(
                h.step(&live, 120),
                DeliveryStep::Blocked {
                    reason: FailureReason::ReadinessTimeout
                }
            );
            live.input_line = InputLine::EmptySince(time(120));
            assert_eq!(
                h.step(&live, 124),
                DeliveryStep::Blocked {
                    reason: FailureReason::ReadinessTimeout
                }
            );
            assert_eq!(h.step(&live, 125), DeliveryStep::PasteNow { attempt: 1 });
        }
        live.send_now = false;
        live.session = Some(LiveSession {
            status: SessionStatus::Working,
            status_since: time(125),
            last_event_at: time(125),
        });
        wait(h.step(&live, 125), WaitReason::SessionBusy);
    }

    #[test]
    fn active_unknown_input_does_not_use_up_the_stale_inspection_budget() {
        let h = History::new(false);
        let mut live = ready();
        live.session = Some(LiveSession {
            status: SessionStatus::Working,
            status_since: time(0),
            last_event_at: time(119),
        });
        live.input_line = InputLine::Unknown;
        wait(h.step(&live, 120), WaitReason::SessionBusy);
        wait(h.step(&live, 238), WaitReason::SessionBusy);
        live.hold_since = Some(time(239));
        wait(h.step(&live, 239), WaitReason::InputUnknown);
        wait(h.step(&live, 358), WaitReason::InputUnknown);
        assert_eq!(
            h.step(&live, 359),
            DeliveryStep::Blocked {
                reason: FailureReason::ReadinessTimeout
            }
        );
    }

    #[test]
    fn first_paste_reserves_attempt_before_any_more_decisions() {
        let mut h = History::new(false);
        assert_eq!(h.step(&ready(), 0), DeliveryStep::PasteNow { attempt: 1 });
        h.attempt(1, 0);
        assert_eq!(
            h.step(&ready(), 0),
            DeliveryStep::AwaitConfirmation { until: time(3) }
        );
        assert_eq!(
            h.command(
                OrgCommand::DeliveryAttempted {
                    message_id: h.id,
                    attempt: 1,
                    pane: "%1".into()
                },
                time(1)
            ),
            0
        );
        assert_eq!(
            h.step(&ready(), 2),
            DeliveryStep::AwaitConfirmation { until: time(3) }
        );
    }

    #[test]
    fn delivering_uses_fixed_deadline_not_current_status_or_old_failure() {
        let mut h = History::new(false);
        h.attempt(1, 0);
        h.failure(false, 3);
        h.attempt(2, 4);
        for status in [
            SessionStatus::Idle,
            SessionStatus::Working,
            SessionStatus::AttentionNeeded,
        ] {
            let mut live = ready();
            live.session.as_mut().unwrap().status = status;
            live.owner_focused = true;
            live.dialog_open = Some(true);
            assert_eq!(
                h.step(&live, 6),
                DeliveryStep::AwaitConfirmation { until: time(7) }
            );
            assert_eq!(
                h.step(&live, 7),
                DeliveryStep::RetryAt {
                    at: time(9),
                    reason: FailureReason::ConfirmationTimeout
                }
            );
        }
    }

    #[test]
    fn retry_schedule_exhaustion_and_unconsumed_timeouts() {
        let mut h = History::new(false);
        for (attempt, start, end, retry) in [(1, 0, 3, 4), (2, 4, 7, 9), (3, 9, 12, 16)] {
            assert_eq!(h.step(&ready(), start), DeliveryStep::PasteNow { attempt });
            h.attempt(attempt, start);
            assert_eq!(
                h.step(&ready(), end),
                DeliveryStep::RetryAt {
                    at: time(retry),
                    reason: FailureReason::ConfirmationTimeout
                }
            );
            assert!(matches!(
                h.step(&ready(), retry + 30),
                DeliveryStep::RetryAt { .. }
            ));
            h.failure(false, end);
            assert_eq!(
                h.step(&ready(), retry - 1),
                DeliveryStep::RetryAt {
                    at: time(retry),
                    reason: FailureReason::ConfirmationTimeout
                }
            );
        }
        h.attempt(4, 16);
        assert_eq!(
            h.step(&ready(), 19),
            DeliveryStep::Fail {
                reason: FailureReason::ConfirmationTimeout
            }
        );
        h.failure(true, 19);
        assert_eq!(h.step(&ready(), 100), DeliveryStep::Complete);
    }

    #[test]
    fn retry_rechecks_guards_and_uses_actual_failure_timestamp() {
        let mut h = History::new(false);
        h.attempt(1, 0);
        h.failure(false, 10);
        assert_eq!(
            h.step(&ready(), 10),
            DeliveryStep::RetryAt {
                at: time(11),
                reason: FailureReason::ConfirmationTimeout
            }
        );
        let mut live = ready();
        live.copy_mode = Some(true);
        wait(h.step(&live, 11), WaitReason::CopyMode);
        live.copy_mode = Some(false);
        live.owner_focused = true;
        wait(h.step(&live, 11), WaitReason::OwnerFocused);
        live.owner_focused = false;
        live.dialog_open = Some(true);
        wait(h.step(&live, 11), WaitReason::DialogOpen);
        live.dialog_open = Some(false);
        assert_eq!(h.step(&live, 11), DeliveryStep::PasteNow { attempt: 2 });
    }

    #[test]
    fn probably_landed_blocks_retry_but_never_confirms() {
        let mut h = History::new(false);
        h.attempt(1, 0);
        let mut live = ready();
        live.probably_landed = true;
        assert_eq!(
            h.step(&live, 2),
            DeliveryStep::AwaitConfirmation { until: time(3) }
        );
        for now in [3, 10, 1000] {
            assert_eq!(
                h.step(&live, now),
                DeliveryStep::Blocked {
                    reason: FailureReason::ProbablyDelivered
                }
            );
        }
        // Evidence can arrive after a failure was persisted and a retry scheduled.
        h.failure(false, 3);
        assert_eq!(
            h.step(&live, 4),
            DeliveryStep::Blocked {
                reason: FailureReason::ProbablyDelivered
            }
        );
        live.confirmation = Some(receipt());
        assert_eq!(
            h.step(&live, 4),
            DeliveryStep::Confirm { receipt: receipt() }
        );
    }

    #[test]
    fn late_confirmation_wins_in_all_non_delivered_states() {
        for state in 0..4 {
            let mut h = History::new(false);
            if state > 0 {
                h.attempt(1, 0);
            }
            if state > 1 {
                h.failure(state == 3, 3);
            }
            let mut live = ready();
            live.confirmation = Some(receipt());
            live.session = None;
            live.pane = PaneLiveness::GoneSince(time(0));
            live.owner_focused = true;
            live.probably_landed = true;
            assert_eq!(
                h.step(&live, 3),
                DeliveryStep::Confirm { receipt: receipt() }
            );
            assert_eq!(
                h.command(
                    OrgCommand::Delivered {
                        message_id: h.id,
                        session_id: receipt().session_id,
                        confirmation: receipt().confirmation,
                    },
                    time(3)
                ),
                1
            );
            assert_eq!(h.step(&live, 3), DeliveryStep::Complete);
        }
        assert_eq!(History::new(true).step(&ready(), 0), DeliveryStep::Complete);
    }

    #[test]
    fn lost_pane_finalizes_queued_and_last_attempt_after_grace() {
        let mut h = History::new(false);
        let mut live = ready();
        live.pane = PaneLiveness::GoneSince(time(-2));
        assert_eq!(
            h.step(&live, 0),
            DeliveryStep::Fail {
                reason: FailureReason::PaneGone
            }
        );
        assert_eq!(
            h.state
                .decide(
                    &OrgCommand::DeliveryFailed {
                        message_id: h.id,
                        reason: "gone".into(),
                        final_failure: true
                    },
                    time(0)
                )
                .unwrap()
                .len(),
            1
        );
        assert_eq!(h.step(&ready(), 0), DeliveryStep::PasteNow { attempt: 1 });
        h.attempt(4, 0);
        assert_eq!(
            h.step(&live, 0),
            DeliveryStep::Fail {
                reason: FailureReason::PaneGone
            }
        );
        h.failure(true, 0);
    }

    #[test]
    fn reserved_attempts_that_cannot_run_fail_final_only_at_budget() {
        let mut h = History::new(false);
        let policy = DeliveryPolicy::default();
        for attempt in 1..=4 {
            let now = i64::from(attempt) * 20;
            assert_eq!(h.step(&ready(), now), DeliveryStep::PasteNow { attempt });
            h.attempt(attempt, now);
            h.failure(policy.is_final_attempt(attempt), now);
            assert_eq!(
                h.message().status,
                if attempt < 4 {
                    DeliveryStatus::Queued
                } else {
                    DeliveryStatus::Failed
                }
            );
        }
        assert_eq!(h.step(&ready(), 1000), DeliveryStep::Complete);
    }

    #[test]
    fn configuration_controls_delays_and_budget_without_panics() {
        let mut h = History::new(false);
        let policy = DeliveryPolicy::new(DeliverySettings {
            stable_idle: Duration::from_secs(20),
            retry_delays: vec![],
            ..DeliverySettings::default()
        })
        .unwrap();
        assert_eq!(
            next_delivery(h.message(), &ready(), time(0), &policy).unwrap(),
            DeliveryStep::Wait {
                until: time(10),
                reason: WaitReason::IdleStabilizing
            }
        );
        h.attempt(1, 10);
        assert_eq!(
            next_delivery(h.message(), &ready(), time(13), &policy).unwrap(),
            DeliveryStep::Fail {
                reason: FailureReason::ConfirmationTimeout
            }
        );
        assert!(policy.is_final_attempt(1));
        assert!(!DeliveryPolicy::default().is_final_attempt(3));
        assert!(DeliveryPolicy::default().is_final_attempt(4));
    }

    #[test]
    fn invalid_metadata_and_extreme_times_are_errors_not_pastes() {
        let h = History::new(false);
        let mut message = h.message().clone();
        message.status = DeliveryStatus::Delivering;
        assert!(matches!(
            next_delivery(&message, &ready(), time(0), &DeliveryPolicy::default()),
            Err(DeliveryPolicyError::InvalidState)
        ));
        message.last_attempt = Some(crate::DeliveryAttempt {
            attempt: 0,
            pane: "%1".into(),
        });
        message.last_attempt_at = Some(time(0));
        assert!(next_delivery(&message, &ready(), time(0), &DeliveryPolicy::default()).is_err());
        message.last_attempt.as_mut().unwrap().attempt = 1;
        message.last_attempt_at = Some(DateTime::<Utc>::MAX_UTC);
        assert!(matches!(
            next_delivery(&message, &ready(), time(0), &DeliveryPolicy::default()),
            Err(DeliveryPolicyError::TimeOverflow)
        ));
        message.status = DeliveryStatus::Queued;
        message.last_attempt_at = Some(time(0));
        assert!(next_delivery(&message, &ready(), time(0), &DeliveryPolicy::default()).is_err());
        message.failed_at = Some(time(1));
        message.last_attempt.as_mut().unwrap().attempt = u32::MAX;
        assert_eq!(
            next_delivery(&message, &ready(), time(10), &DeliveryPolicy::default()).unwrap(),
            DeliveryStep::Fail {
                reason: FailureReason::AttemptLimit
            }
        );
    }

    use proptest::prelude::*;

    #[test]
    fn no_two_pastes_without_recorded_failure_and_none_after_confirmation() {
        use proptest::test_runner::{Config, RngSeed, TestRunner};
        use std::cell::Cell;
        let cases = Cell::new(0usize);
        let multiple = Cell::new(0usize);
        let four = Cell::new(0usize);
        let restarts = Cell::new(0usize);
        let evidence = Cell::new(0usize);
        let receipts = Cell::new(0usize);
        let failures = Cell::new(0usize);
        let evidence_vetoes = Cell::new(0usize);
        let restart_vetoes = Cell::new(0usize);
        let last_paste_vetoes = Cell::new(0usize);
        let mut runner = TestRunner::new(Config {
            cases: 512,
            rng_seed: RngSeed::Fixed(0x9a02),
            failure_persistence: None, // fixed seed reproduces this entire run
            ..Config::default()
        });
        let ticks =
            prop::collection::vec((-3i64..26, prop::array::uniform12(0u16..1000)), 100..200);
        runner.run(&ticks, |ticks| {
            let policy = DeliveryPolicy::default();
            let mut h = History::new(false);
            let mut live = ready();
            prop_assert_eq!(h.step(&live, 0), DeliveryStep::PasteNow { attempt: 1 });
            h.attempt(1, 0);
            live.last_paste_at = Some(time(0));
            let mut now = 0;
            let mut failure_since_paste = false;
            let mut confirmed = false;
            let mut landed_any = false; // oracle survives restart, executor latch does not
            let mut previous_attempt = 1;
            for (delta, flags) in ticks {
                now += delta;
                let [focus, copy, dialog, pane, status, input, send, agent, other, activity, action, io] = flags;
                live.owner_focused = focus < 50;
                live.copy_mode = if copy < 25 { None } else { Some(copy < 50) };
                live.dialog_open = if dialog < 25 { None } else { Some(dialog < 50) };
                live.pane = match pane {
                    0..5 => match live.pane {
                        PaneLiveness::GoneSince(at) => PaneLiveness::GoneSince(at),
                        _ => PaneLiveness::GoneSince(time(now)),
                    },
                    5..25 => PaneLiveness::Unknown,
                    _ => PaneLiveness::Alive,
                };
                let was_ready = live.agent_status == AgentStatus::Ready;
                live.agent_status = match agent {
                    0..10 => AgentStatus::Starting,
                    10..20 => AgentStatus::Clearing,
                    20..30 => AgentStatus::Stuck,
                    30..40 => AgentStatus::Down,
                    _ => AgentStatus::Ready,
                };
                if live.agent_status == AgentStatus::Ready {
                    live.agent_not_ready_since = None;
                } else if was_ready {
                    live.agent_not_ready_since = Some(time(now));
                }
                live.other_delivery_in_flight = other < 30;
                if other < 10 { live.last_paste_at = Some(time(now)); }
                live.send_now = send < 100;
                let next_status = match status {
                    0..30 => None,
                    30..60 => Some(SessionStatus::Working),
                    60..90 => Some(SessionStatus::AttentionNeeded),
                    _ => Some(SessionStatus::Idle),
                };
                if live.session.as_ref().map(|s| s.status) != next_status {
                    live.session = next_status.map(|status| LiveSession {
                        status, status_since: time(now), last_event_at: time(now),
                    });
                    live.input_line = InputLine::Unknown;
                }
                live.input_line = match input {
                    0..40 => InputLine::Unknown,
                    40..80 => InputLine::NonEmpty,
                    _ => match live.input_line {
                        InputLine::EmptySince(at) => InputLine::EmptySince(at),
                        _ => InputLine::EmptySince(time(now)),
                    },
                };
                // Rare receive-order evidence, including after a timeout was logged.
                if activity < 5 && matches!(h.message().status, DeliveryStatus::Delivering | DeliveryStatus::Queued) {
                    live.probably_landed = true;
                    landed_any = true;
                    evidence.set(evidence.get() + 1);
                    if let Some(row) = &mut live.session { row.last_event_at = time(now); }
                }
                if io < 30 && h.message().status == DeliveryStatus::Delivering
                    && !live.probably_landed && !live.last_attempt_before_start {
                    h.failure(policy.is_final_attempt(previous_attempt), now);
                    failures.set(failures.get() + 1);
                    failure_since_paste = true;
                }
                if action < 5 {
                    let mut replayed = OrgState::default();
                    for (index, (ts, event)) in h.log.iter().enumerate() {
                        replayed.apply(index as u64 + 1, *ts, event).unwrap();
                    }
                    prop_assert_eq!(&replayed, &h.state);
                    h.state = replayed;
                    live.confirmation = None;
                    live.probably_landed = false;
                    live.last_attempt_before_start = h.message().last_attempt.is_some();
                    if matches!(h.message().status, DeliveryStatus::Delivering | DeliveryStatus::Queued) {
                        restarts.set(restarts.get() + 1);
                    }
                    if let Some(row) = &mut live.session { row.status_since = time(now); }
                    live.input_line = InputLine::Unknown;
                    live.last_paste_at = Some(time(now));
                    live.hold_since = None;
                } else if action < 10 {
                    live.confirmation = Some(receipt());
                }
                let step = h.step(&live, now);
                // Count only counterfactuals where removing this one veto
                // would actually paste; merely generating the flag is not coverage.
                if !matches!(step, DeliveryStep::PasteNow { .. }) {
                    for (counter, without_veto) in [
                        (&evidence_vetoes, RecipientLiveView { probably_landed: false, ..live.clone() }),
                        (&restart_vetoes, RecipientLiveView { last_attempt_before_start: false, ..live.clone() }),
                        (&last_paste_vetoes, RecipientLiveView { last_paste_at: None, ..live.clone() }),
                    ] {
                        if matches!(h.step(&without_veto, now), DeliveryStep::PasteNow { .. }) {
                            counter.set(counter.get() + 1);
                        }
                    }
                }
                track_hold(&mut live, &step, time(now));
                match step {
                    DeliveryStep::PasteNow { attempt } => {
                        prop_assert!(failure_since_paste);
                        prop_assert!(!confirmed && !landed_any && live.confirmation.is_none() && !live.probably_landed);
                        prop_assert_eq!(live.pane, PaneLiveness::Alive);
                        prop_assert!(!live.owner_focused && !live.other_delivery_in_flight);
                        prop_assert_eq!(live.agent_status, AgentStatus::Ready);
                        prop_assert!(live.copy_mode == Some(false) && live.dialog_open == Some(false));
                        prop_assert!(matches!(live.input_line, InputLine::EmptySince(_)));
                        prop_assert!(attempt > previous_attempt);
                        h.attempt(attempt, now);
                        live.probably_landed = false; // reset per reserved attempt, not per tick
                        live.last_attempt_before_start = false;
                        live.last_paste_at = Some(time(now));
                        previous_attempt = attempt;
                        failure_since_paste = false;
                    }
                    DeliveryStep::RetryAt { .. } if h.message().status == DeliveryStatus::Delivering => {
                        prop_assert!(!live.probably_landed && !live.last_attempt_before_start);
                        prop_assert!(!policy.is_final_attempt(previous_attempt));
                        h.failure(false, now);
                        failures.set(failures.get() + 1);
                        failure_since_paste = true;
                    }
                    DeliveryStep::Fail { .. } => {
                        h.failure(true, now);
                        failures.set(failures.get() + 1);
                        failure_since_paste = true;
                    }
                    DeliveryStep::Confirm { receipt } => {
                        h.command(OrgCommand::Delivered {
                            message_id: h.id, session_id: receipt.session_id, confirmation: receipt.confirmation,
                        }, time(now));
                        receipts.set(receipts.get() + 1);
                        confirmed = true;
                    }
                    _ => {}
                }
                if confirmed { prop_assert_eq!(h.message().status, DeliveryStatus::Delivered); }
            }
            cases.set(cases.get() + 1);
            if previous_attempt >= 2 { multiple.set(multiple.get() + 1); }
            if previous_attempt == 4 { four.set(four.get() + 1); }
            Ok(())
        }).unwrap();
        println!("delivery coverage: cases={}, >=2 pastes={}, 4 pastes={}, unconfirmed restarts={}, evidence={}, receipts={}, failures={}",
            cases.get(), multiple.get(), four.get(), restarts.get(), evidence.get(), receipts.get(), failures.get());
        assert!(
            multiple.get() * 4 >= cases.get(),
            "retry assertions must run in at least 25% of histories"
        );
        assert!(four.get() > 0 && restarts.get() > 0 && evidence.get() > 0 && receipts.get() > 0);
        println!(
            "counterfactual veto coverage: evidence={}, restart={}, last_paste={}",
            evidence_vetoes.get(),
            restart_vetoes.get(),
            last_paste_vetoes.get()
        );
        assert!(
            evidence_vetoes.get() > 0 && restart_vetoes.get() > 0 && last_paste_vetoes.get() > 0
        );
    }

    proptest! {
        #[test]
        fn landed_paste_with_lost_prompt_event_never_retries(
            times in prop::collection::vec(-30i64..10000, 1..200),
            queued in any::<bool>(),
        ) {
            let mut h = History::new(false);
            h.attempt(1, 0);
            if queued { h.failure(false, 3); }
            let mut live = ready(); live.probably_landed = true;
            for now in times {
                let step = h.step(&live, now);
                if !queued && now < 3 {
                    prop_assert_eq!(step, DeliveryStep::AwaitConfirmation { until: time(3) });
                } else {
                    prop_assert_eq!(step, DeliveryStep::Blocked { reason: FailureReason::ProbablyDelivered });
                }
                prop_assert_eq!(h.message().last_attempt.as_ref().unwrap().attempt, 1);
            }
            live.confirmation = Some(receipt());
            prop_assert_eq!(h.step(&live, 10000), DeliveryStep::Confirm { receipt: receipt() });
        }
    }

    #[test]
    fn backward_clock_and_future_observations_never_grant_early_paste() {
        let mut h = History::new(false);
        h.attempt(1, 10);
        assert_eq!(
            h.step(&ready(), 5),
            DeliveryStep::AwaitConfirmation { until: time(13) }
        );
        // Wall-clock failure time preceding attempt is legal in the log.
        h.failure(false, 4);
        assert_eq!(
            h.step(&ready(), 3),
            DeliveryStep::RetryAt {
                at: time(5),
                reason: FailureReason::ConfirmationTimeout
            }
        );
        let mut live = ready();
        live.input_line = InputLine::EmptySince(time(10));
        assert_eq!(
            wait(h.step(&live, 5), WaitReason::InputStabilizing),
            time(10)
        );
        let mut live = ready();
        live.session.as_mut().unwrap().status_since = time(20);
        assert_eq!(
            wait(h.step(&live, 5), WaitReason::IdleStabilizing),
            time(30)
        );
        let now = time(30) - TimeDelta::milliseconds(1);
        assert!(matches!(
            next_delivery(h.message(), &live, now, &DeliveryPolicy::default()).unwrap(),
            DeliveryStep::Wait { .. }
        ));
    }
}
