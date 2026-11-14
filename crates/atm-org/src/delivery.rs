//! Pure delivery policy; observations and time belong to the executor.
//!
//! Consume each decision once, serially per message: decide/append/apply
//! `DeliveryAttempted` before paste + Enter, never paste if append fails, and
//! never treat a duplicate/no-event attempt command as another authorization.
//! Recheck live guards immediately before I/O. If a reserved attempt cannot run,
//! report its failure; do not silently put it back in the queue. Resolution,
//! subagent targeting, bracketed-paste safety and token formatting stay outside
//! this module. A pure decision cannot make a paste and a log append atomic.
//!
//! `Confirm` maps to `Delivered`. `RetryAt` while delivering requires nonfinal
//! `DeliveryFailed`; once applied, recompute the deadline from logged failed_at.
//! `RetryAt` while queued only schedules. `Fail` requires final `DeliveryFailed`.
//! `Blocked` emits no message event: retain state, notify the owner once per
//! reason, and reevaluate when observations change. Continue listening for late
//! receipts even after a final failure. Wait/AwaitConfirmation/Complete emit no
//! events. Deadlines are inclusive; events can wake a timer earlier.

use crate::{DeliveryReceipt, DeliveryStatus, Message};
use atm_core::SessionStatus;
use chrono::{DateTime, TimeDelta, Utc};
use std::time::Duration;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryPolicy {
    pub stable_idle: Duration,
    pub stable_empty_input: Duration,
    pub confirmation_timeout: Duration,
    /// Delays following attempts 1, 2, ...; maximum attempts = len + 1.
    /// Empty means one attempt. Delays must be positive and nondecreasing.
    pub retry_delays: Vec<Duration>,
    pub stale_after: Duration,
    pub readiness_timeout: Duration,
    pub guard_poll_interval: Duration,
}

impl Default for DeliveryPolicy {
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

/// Fresh, neutral observations for this message's resolved recipient pane.
/// Deliberately has no Default: missing observations must not grant permission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecipientLiveView {
    /// None is a missing row, not a dead pane.
    pub session: Option<LiveSession>,
    pub pane_alive: bool,
    /// Unknown guards block just as positive unsafe guards do.
    pub copy_mode: Option<bool>,
    pub dialog_open: Option<bool>,
    /// True iff any tmux client is BOTH focused (client_flags) and selecting
    /// this pane (pane_id). Unknown is true; selected but unfocused is false.
    /// A turn end only triggers a recheck, never overrides focus.
    pub owner_focused: bool,
    pub input_line: InputLine,
    /// Bypass idle status/stability only, never input or other safety guards.
    pub send_now: bool,
    /// Executor validated this message's token in a prompt-submitted event for
    /// its target pane/session. Latch until Delivered is applied. The receipt
    /// may name an earlier session/attempt; current live guards cannot veto it.
    pub confirmation: Option<DeliveryReceipt>,
    /// Latched evidence that ANY attempt of this message probably landed:
    /// busy transition after its last_attempt_at, or its token seen in the pane.
    /// Not a receipt. Retain across polls, timeout/retry scheduling and recovery;
    /// never clear merely because the row went idle or the token scrolled away.
    /// Recovery must reconstruct it or hold delivery for owner inspection.
    pub probably_landed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
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
pub enum WaitReason {
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
pub enum FailureReason {
    #[error("prompt token was not confirmed before the deadline")]
    ConfirmationTimeout,
    #[error("recipient pane is not alive")]
    PaneGone,
    #[error("stale session has a draft")]
    DraftPresent,
    #[error("stale session has a dialog")]
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
    #[error("delivery durations must be positive, representable, and retry delays nondecreasing")]
    InvalidConfiguration,
    #[error("delivery state lacks consistent attempt/failure metadata")]
    InvalidState,
    #[error("delivery deadline is outside the timestamp range")]
    TimeOverflow,
}

/// Decide without mutation. Caller must apply commands before another decision.
/// Terminal/receipt decisions take precedence even over invalid configuration.
/// Status and log ordering, not wall-clock timestamp ordering, identify attempts.
#[must_use = "consume the decision through the serialized delivery executor"]
pub fn next_delivery(
    message: &Message,
    live: &RecipientLiveView,
    now: DateTime<Utc>,
    policy: &DeliveryPolicy,
) -> Result<DeliveryStep, DeliveryPolicyError> {
    if message.status == DeliveryStatus::Delivered {
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
    let max_attempts = validate_policy(policy)?;
    let attempt = match (&message.last_attempt, message.last_attempt_at) {
        (Some(attempt), Some(_)) if attempt.attempt > 0 => attempt.attempt,
        (None, None) if message.status == DeliveryStatus::Queued => 0,
        _ => return Err(DeliveryPolicyError::InvalidState),
    };
    if live.probably_landed && attempt == 0 {
        return Err(DeliveryPolicyError::InvalidState);
    }
    let retry_delay = attempt
        .checked_sub(1)
        .and_then(|index| usize::try_from(index).ok())
        .and_then(|index| policy.retry_delays.get(index))
        .copied();
    if message.status == DeliveryStatus::Delivering {
        let started = message
            .last_attempt_at
            .ok_or(DeliveryPolicyError::InvalidState)?;
        let until = deadline(started, policy.confirmation_timeout)?;
        if now >= until && live.probably_landed {
            return Ok(DeliveryStep::Blocked {
                reason: FailureReason::ProbablyDelivered,
            });
        }
        if !live.pane_alive {
            return Ok(DeliveryStep::Fail {
                reason: FailureReason::PaneGone,
            });
        }
        return Ok(if now < until {
            DeliveryStep::AwaitConfirmation { until }
        } else if let Some(delay) = retry_delay {
            DeliveryStep::RetryAt {
                at: deadline(now, delay)?,
                reason: FailureReason::ConfirmationTimeout,
            }
        } else {
            DeliveryStep::Fail {
                reason: FailureReason::ConfirmationTimeout,
            }
        });
    }
    if attempt > 0 {
        let failed = message.failed_at.ok_or(DeliveryPolicyError::InvalidState)?;
        if live.probably_landed {
            return Ok(DeliveryStep::Blocked {
                reason: FailureReason::ProbablyDelivered,
            });
        }
        let Some(delay) = retry_delay else {
            return Ok(DeliveryStep::Blocked {
                reason: FailureReason::AttemptLimit,
            });
        };
        let at = deadline(failed, delay)?;
        if now < at {
            return Ok(DeliveryStep::RetryAt {
                at,
                reason: FailureReason::ConfirmationTimeout,
            });
        }
    }
    if !live.pane_alive {
        return Ok(DeliveryStep::Blocked {
            reason: FailureReason::PaneGone,
        });
    }
    let poll = |reason| {
        Ok(DeliveryStep::Wait {
            until: deadline(now, policy.guard_poll_interval)?,
            reason,
        })
    };
    if live.owner_focused {
        return poll(WaitReason::OwnerFocused);
    }
    let stale_at = live
        .session
        .as_ref()
        .filter(|row| row.status != SessionStatus::Idle)
        .map(|row| deadline(row.last_event_at, policy.stale_after))
        .transpose()?;
    let stale = stale_at.is_some_and(|at| now >= at);
    // Allow bounded pane inspection after a row becomes stale. A missing row
    // has no event anchor, so its unknown-readiness budget starts at msg.sent.
    let readiness_start = stale_at
        .filter(|_| stale)
        .unwrap_or(message.sent_at)
        .max(message.sent_at);
    let unknown = |reason| {
        let until = deadline(readiness_start, policy.readiness_timeout)?;
        Ok(if now >= until {
            DeliveryStep::Blocked {
                reason: FailureReason::ReadinessTimeout,
            }
        } else {
            DeliveryStep::Wait {
                until: deadline(now, policy.guard_poll_interval)?.min(until),
                reason,
            }
        })
    };
    match live.copy_mode {
        Some(true) => return poll(WaitReason::CopyMode),
        None => return unknown(WaitReason::UnknownGuards),
        Some(false) => {}
    }
    match live.dialog_open {
        Some(true) if stale => {
            return Ok(DeliveryStep::Blocked {
                reason: FailureReason::DialogOpen,
            })
        }
        Some(true) => return poll(WaitReason::DialogOpen),
        None => return unknown(WaitReason::UnknownGuards),
        Some(false) => {}
    }
    if !live.send_now && !stale {
        if let Some(at) = stale_at {
            return Ok(DeliveryStep::Wait {
                until: deadline(now, policy.guard_poll_interval)?.min(at),
                reason: WaitReason::SessionBusy,
            });
        }
    }
    let empty_since = match live.input_line {
        InputLine::NonEmpty if stale => {
            return Ok(DeliveryStep::Blocked {
                reason: FailureReason::DraftPresent,
            })
        }
        InputLine::NonEmpty if live.session.is_none() => return unknown(WaitReason::InputNotEmpty),
        InputLine::NonEmpty => return poll(WaitReason::InputNotEmpty),
        InputLine::Unknown => return unknown(WaitReason::InputUnknown),
        InputLine::EmptySince(since) => since,
    };
    if now < empty_since {
        return Ok(DeliveryStep::Wait {
            until: empty_since,
            reason: WaitReason::InputStabilizing,
        });
    }
    match &live.session {
        None => {
            let until = deadline(empty_since, policy.stable_empty_input)?;
            if now < until {
                return Ok(DeliveryStep::Wait {
                    until,
                    reason: WaitReason::InputStabilizing,
                });
            }
        }
        Some(row) if row.status == SessionStatus::Idle && !live.send_now => {
            let until = deadline(row.status_since, policy.stable_idle)?;
            if now < until {
                return Ok(DeliveryStep::Wait {
                    until,
                    reason: WaitReason::IdleStabilizing,
                });
            }
        }
        _ => {}
    }
    let attempt = attempt
        .checked_add(1)
        .filter(|n| *n <= max_attempts)
        .ok_or(DeliveryPolicyError::InvalidState)?;
    Ok(DeliveryStep::PasteNow { attempt })
}

fn deadline(at: DateTime<Utc>, duration: Duration) -> Result<DateTime<Utc>, DeliveryPolicyError> {
    let delta =
        TimeDelta::from_std(duration).map_err(|_| DeliveryPolicyError::InvalidConfiguration)?;
    at.checked_add_signed(delta)
        .ok_or(DeliveryPolicyError::TimeOverflow)
}

fn validate_policy(policy: &DeliveryPolicy) -> Result<u32, DeliveryPolicyError> {
    let durations = [
        policy.stable_idle,
        policy.stable_empty_input,
        policy.confirmation_timeout,
        policy.stale_after,
        policy.readiness_timeout,
        policy.guard_poll_interval,
    ];
    if durations
        .iter()
        .chain(&policy.retry_delays)
        .any(|d| d.is_zero() || TimeDelta::from_std(*d).is_err())
        || policy
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
            pane_alive: true,
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
                wait(h.step(&blocked, 0), reason);
            }
            live.pane_alive = false;
            assert_eq!(
                h.step(&live, 0),
                DeliveryStep::Blocked {
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
                live.input_line = InputLine::NonEmpty;
                assert_eq!(
                    h.step(&live, 120),
                    DeliveryStep::Blocked {
                        reason: FailureReason::DraftPresent
                    }
                );
                live.input_line = InputLine::EmptySince(time(0));
                live.dialog_open = Some(true);
                assert_eq!(
                    h.step(&live, 120),
                    DeliveryStep::Blocked {
                        reason: FailureReason::DialogOpen
                    }
                );
                live.dialog_open = Some(false);
                live.input_line = InputLine::Unknown;
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
                wait(h.step(&live, 124), WaitReason::InputStabilizing),
                time(125)
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
            live.pane_alive = false;
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
    fn lost_pane_fails_only_an_inflight_attempt_and_blocked_can_recover() {
        let mut h = History::new(false);
        let mut live = ready();
        live.pane_alive = false;
        assert_eq!(
            h.step(&live, 0),
            DeliveryStep::Blocked {
                reason: FailureReason::PaneGone
            }
        );
        assert!(h
            .state
            .decide(
                &OrgCommand::DeliveryFailed {
                    message_id: h.id,
                    reason: "gone".into(),
                    final_failure: true
                },
                time(0)
            )
            .is_err());
        assert_eq!(h.step(&ready(), 0), DeliveryStep::PasteNow { attempt: 1 });
        h.attempt(1, 0);
        assert_eq!(
            h.step(&live, 0),
            DeliveryStep::Fail {
                reason: FailureReason::PaneGone
            }
        );
        h.failure(true, 0);
    }

    #[test]
    fn configuration_controls_delays_and_budget_without_panics() {
        let mut h = History::new(false);
        let mut policy = DeliveryPolicy {
            stable_idle: Duration::from_secs(20),
            retry_delays: vec![],
            ..DeliveryPolicy::default()
        };
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
        policy.retry_delays = vec![Duration::from_secs(2), Duration::from_secs(1)];
        assert!(matches!(
            next_delivery(h.message(), &ready(), time(13), &policy),
            Err(DeliveryPolicyError::InvalidConfiguration)
        ));
        policy = DeliveryPolicy::default();
        policy.stable_idle = Duration::ZERO;
        assert!(next_delivery(h.message(), &ready(), time(13), &policy).is_err());
        policy = DeliveryPolicy::default();
        policy.confirmation_timeout = Duration::MAX;
        assert!(next_delivery(h.message(), &ready(), time(13), &policy).is_err());
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
            DeliveryStep::Blocked {
                reason: FailureReason::AttemptLimit
            }
        );
    }

    use proptest::prelude::*;

    proptest! {
        #[test]
        fn no_two_pastes_without_recorded_failure_and_none_after_confirmation(
            ticks in prop::collection::vec((-10i64..151, 0u8..4, 0u8..3, any::<[bool; 7]>(), 0u8..6), 0..200)
        ) {
            let mut h = History::new(false);
            let mut live = ready();
            // Every generated history reaches a real authorized attempt.
            prop_assert_eq!(h.step(&live, 0), DeliveryStep::PasteNow { attempt: 1 });
            h.attempt(1, 0);
            let mut now = 0;
            let mut failure_since_paste = false;
            let mut confirmed = false;
            let mut previous_attempt = 1;
            for (delta, status, input, flags, action) in ticks {
                now += delta;
                let [focus, copy, dialog, alive, event, send_now, landed] = flags;
                live.owner_focused = focus;
                live.copy_mode = if action == 5 { None } else { Some(copy) };
                live.dialog_open = if action == 4 { None } else { Some(dialog) };
                live.pane_alive = alive;
                live.send_now = send_now;
                live.probably_landed |= landed;
                let next_status = match status {
                    0 => None,
                    1 => Some(SessionStatus::Idle),
                    2 => Some(SessionStatus::Working),
                    _ => Some(SessionStatus::AttentionNeeded),
                };
                let old_status = live.session.as_ref().map(|s| s.status);
                if old_status != next_status {
                    live.session = next_status.map(|status| LiveSession {
                        status, status_since: time(now), last_event_at: time(now),
                    });
                    live.input_line = InputLine::Unknown;
                } else if event {
                    if let Some(row) = &mut live.session { row.last_event_at = time(now); }
                }
                live.input_line = match input {
                    0 => InputLine::Unknown,
                    1 => InputLine::NonEmpty,
                    _ => match live.input_line {
                        InputLine::EmptySince(at) => InputLine::EmptySince(at),
                        _ => InputLine::EmptySince(time(now)),
                    },
                };
                if action == 1 && h.message().status == DeliveryStatus::Delivering
                    && !live.probably_landed && !confirmed {
                    h.failure(false, now); // explicit executor I/O failure
                    failure_since_paste = true;
                }
                if action == 2 { live.confirmation = Some(receipt()); }
                if action == 3 {
                    let mut replayed = OrgState::default();
                    for (index, (ts, event)) in h.log.iter().enumerate() {
                        replayed.apply(index as u64 + 1, *ts, event).unwrap();
                    }
                    prop_assert_eq!(&replayed, &h.state);
                    h.state = replayed;
                    // Restart loses continuity, never the latched landing evidence.
                    if let Some(row) = &mut live.session { row.status_since = time(now); }
                    live.input_line = InputLine::Unknown;
                }
                match h.step(&live, now) {
                    DeliveryStep::PasteNow { attempt } => {
                        prop_assert!(failure_since_paste);
                        prop_assert!(!confirmed && live.confirmation.is_none() && !live.probably_landed);
                        prop_assert!(alive && !focus && live.copy_mode == Some(false) && live.dialog_open == Some(false));
                        prop_assert!(matches!(live.input_line, InputLine::EmptySince(_)));
                        prop_assert!(attempt > previous_attempt);
                        h.attempt(attempt, now);
                        previous_attempt = attempt;
                        failure_since_paste = false;
                    }
                    DeliveryStep::RetryAt { .. } if h.message().status == DeliveryStatus::Delivering => {
                        prop_assert!(!live.probably_landed);
                        h.failure(false, now);
                        failure_since_paste = true;
                    }
                    DeliveryStep::Fail { .. } => {
                        prop_assert_eq!(h.message().status, DeliveryStatus::Delivering);
                        h.failure(true, now);
                        failure_since_paste = true;
                    }
                    DeliveryStep::Confirm { receipt } => {
                        h.command(OrgCommand::Delivered {
                            message_id: h.id, session_id: receipt.session_id, confirmation: receipt.confirmation,
                        }, time(now));
                        confirmed = true;
                    }
                    _ => {}
                }
                if confirmed { prop_assert_eq!(h.message().status, DeliveryStatus::Delivered); }
            }
        }

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
