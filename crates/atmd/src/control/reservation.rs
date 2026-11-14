//! Exclusive, fenced control for an agent's lifecycle or one delivery.
//!
//! # Implementation checkpoint — ownership and supervision
//!
//! `run_reserved` now admits bounded daemon-owned jobs, serializes conflicting
//! ownership, rotates cleanup fences and quarantines failed cleanup. Pane methods
//! validate the live fence, then return [`ReservationError::NotImplemented`];
//! tmux effects, guarded input, org integration and legacy send/spawn isolation
//! are still pending. The additive guarded methods are typed stubs too.
//! The full effect/legacy-path guarantees below remain the implementation contract,
//! **not guarantees provided by this checkpoint**. Existing send/spawn is unchanged.
//!
//! # Ownership and ordering
//!
//! [`ControlHandle::run_reserved`] owns admission, acquisition, cleanup and
//! release; callers never release a guard by dropping it. After admission, a
//! disconnected caller only loses its reply: the bounded daemon job continues.
//! A reservation owns its agent and initial pane, plus any panes it creates.
//! This serializes Up before a pane exists and Respawn across old/new panes.
//! Short tmux operations use the existing actor. Readiness/confirmation and
//! organisation persistence waits occur outside it, so other panes can proceed.
//!
//! Every effect checks the live operation/phase epoch and owned pane, then the
//! expected **pane-local** `@atm-agent` inside the actor. A matching tag alone
//! never authorizes a stale capability. Release and cleanup invalidate older
//! capabilities; another actor cannot accept them. Arbitrary external tmux
//! clients are not serialized by this actor: this is not an external-client CAS.
//!
//! Work has an absolute deadline (at most 120 seconds from admission), with
//! each short action also bounded by the existing 30-second action budget.
//! Cleanup has a separate 10-second budget and a new phase capability, after
//! old work has stopped and admitted effects have been accounted for. Failed
//! cleanup quarantines the agent/panes: mutations fail unavailable until daemon
//! restart and lifecycle reconciliation. There is no unlock API here; quarantine
//! state must remain distinct from released state for a future resolve operation.
//! Active jobs, parked admissions and quarantines have bounded storage (32).
//!
//! # Lifecycle caller recipe
//!
//! All steps in each row after acquisition belong to **one** `run_reserved`
//! work closure. Submit durable commands through the caller's sole
//! [`crate::org::OrgHandle`], never by directly appending an event. On failure,
//! its cleanup closure records the appropriate durable failure before release.
//! A rejected precheck must not mark an otherwise unchanged ready agent stuck.
//!
//! | Operation | Calls, in order |
//! |---|---|
//! | Up | Validate role and use [`ControlHandle::prepare_spawn`] before admission. Request Up with no pane; execute `OrgCommand::Up`; [`Reservation::split`]; [`Reservation::tag`] with Absent; execute `PaneCreated`; [`Reservation::paste_guarded`] launch text then [`Reservation::keys_guarded`] Enter, each with the caller's pure expected-screen check; guard trust/update-dialog keys too; [`Reservation::capture`] and registry observation; execute `SessionBound` or `EditorReady`; return, cleanup, release. |
//! | Adopt | Request Adopt with its pane; [`Reservation::inspect`]; tag Absent only for explicitly untagged adoption, otherwise require this agent's tag; re-read eligible registry row; execute `Adopt`; return. Never send keys or restart. |
//! | Clear | Request Clear before prechecks; inspect/capture exact ownership and editor; subscribe and take baseline; execute `Clear`; paste_guarded adapter clear command; observe the displayed draft, then keys_guarded Enter with the adapter-derived draft/dialog check; guard any subsequent dialog keys too; await post-command evidence outside actor; capture again; execute `SessionBound`/`EditorReady` before returning. |
//! | Respawn | Validate role/placement first; request Respawn with old pane if any; inspect; [`Reservation::kill`] matching old pane (with audit); execute `Respawn`; split, tag Absent, `PaneCreated`, guarded launch paste/keys, observe, durable ready as Up. Old-pane ownership lasts until the whole job releases. |
//! | Down | Request Down with current pane; inspect; kill with audit or [`Reservation::untag`]; execute `Down`; return. Never act on a foreign replacement pane. |
//! | Binding observer | Request Observe; acquire before observing; re-read pane/registry and durable exclusion; observe then execute `SessionBound`/`EditorReady` in this same job. Never spawn a late report outside it. |
//!
//! Read-only list output needs no reservation, but applies the same
//! `excluded_session_id` filtering as every pane-to-row consumer. A snapshot is
//! not permission to submit a later unfenced readiness report. U8b's
//! `EditorReady` command has no pane/generation argument; keeping observation and
//! report inside this job is therefore mandatory even when tmux effects fence.
//!
//! # One organisation delivery
//!
//! U9b requests Delivery with [`WaitPolicy::Wait`] on the same agent/pane. After
//! acquisition, re-read organisation policy and eligible registry rows, inspect
//! ownership/copy mode, and capture the adapter-requested format. If lifecycle
//! moved the pane while waiting, return to policy for rescheduling; never deliver
//! against the pre-acquire snapshot. Record `DeliveryAttempted`, paste_guarded
//! the text using an adapter-derived editor check; observe the displayed draft,
//! then keys_guarded Enter using an adapter-derived draft/dialog check. Await
//! confirmation outside the tmux actor, then record `Delivered`/`DeliveryFailed`
//! before cleanup and release. The caller supplies vendor-specific screen and
//! confirmation policy; this module never infers it.
//!
//! **An unguarded Enter after a paste is never safe for U8/U9b.** A prior capture
//! is not authorization: a dialog or shell may replace the editor while a key
//! waits in the actor queue. Both guarded forms re-capture and invoke the caller's
//! pure, quick predicate in the same actor turn as the effect. Use predicates,
//! not whole-frame equality: harmless counters/spinners may change. This closes
//! the actor-queue gap, not races with external harness redraws or tmux clients.
//!
//! Do not call legacy `send` or `type_spawn_command` inside a reservation: those
//! paths wait behind its ownership. Use reserved guarded paste/keys instead. Raw send
//! during Clear (including durable Clearing after restart) refuses immediately;
//! other reservation kinds make legacy send/spawn_type wait within their original
//! action budget and re-resolve/recheck, without retry. An excluded old session
//! prefix becomes NoSession after clear, never input redirected to the new editor.
//! Quarantine and unavailable org state on tagged/unknown-ownership panes fail
//! unavailable; positively untagged legacy panes retain their existing behavior.

use std::{fmt, future::Future};

use atm_org::AgentId;
use atm_tmux::{PaneDirection, PaneInfo};
use chrono::{DateTime, Utc};
use tokio::time::Instant;

use super::{service::ControlHandle, ControlError, SpawnContext};

#[path = "reserved_jobs.rs"]
pub(super) mod jobs;
#[path = "reservation_state.rs"]
pub(super) mod state;

/// Consumer test scripting, available to integration tests without cfg(test).
#[doc(hidden)]
#[path = "reservation_testing.rs"]
pub mod testing;

/// Exact tmux pane target: `%` followed by one or more ASCII digits.
///
/// No leading zeroes (except `%0`), session/window targeting syntax, whitespace,
/// signs or command separators. tmux 3.8 resolves `%00` to `%0`; accepting both
/// spellings would let two reservation keys name the same physical pane.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PaneId(String);

impl PaneId {
    pub fn parse(value: impl Into<String>) -> Result<Self, ReservationError> {
        let value = value.into();
        if value.strip_prefix('%').is_some_and(|digits| {
            !digits.is_empty()
                && (digits == "0" || !digits.starts_with('0'))
                && digits.bytes().all(|c| c.is_ascii_digit())
        }) {
            Ok(Self(value))
        } else {
            Err(ReservationError::InvalidRequest {
                reason: "pane must be % followed by canonical ASCII digits".into(),
            })
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PaneId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why ownership is held; Clear is the special immediate raw-send refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReservationKind {
    Up,
    Adopt,
    Clear,
    Respawn,
    Down,
    Delivery,
    Observe,
}

/// Wait outside the tmux actor until available, or return Busy without effects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitPolicy {
    Wait,
    Refuse,
}

/// Admission, acquisition and work share this absolute deadline.
#[derive(Debug, Clone)]
pub struct ReservationRequest {
    pub agent: AgentId,
    /// None only for Up or Respawn creating a new pane without an old one.
    pub pane: Option<PaneId>,
    pub kind: ReservationKind,
    pub wait: WaitPolicy,
    pub deadline: Instant,
}

/// Outcome of work, supplied to cleanup while ownership is still exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Completion {
    Succeeded,
    Failed,
    TimedOut,
    WorkerStopped,
}

/// A pane-local option expectation, checked again at mutation time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExpectedTag {
    /// Only explicit Adopt or a pane just created by this Up/Respawn may use it.
    /// An empty-but-present tag and an inherited global tag are not a local tag.
    Absent,
    Agent(AgentId),
}

/// The entire nonliteral-key surface. Text belongs in [`Reservation::paste`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneKey {
    Enter,
    Escape,
    Up,
    Down,
    CtrlC,
}

/// Selected by the consumer's adapter policy; never caller-supplied tmux flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureFormat {
    Plain,
    /// Preserve tmux's terminal style escapes for an attribute-aware observer.
    Ansi,
}

/// Raw tmux viewport metadata, not a harness's readiness or screen-domain type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneInspection {
    pub pane: PaneInfo,
    /// Preserve empty or malformed values; None means positively absent locally.
    pub local_agent_tag: Option<String>,
    pub dead: bool,
    pub in_mode: bool,
    /// None means unknown (for example, a tmux version without this format).
    pub bracketed_paste: Option<bool>,
    /// Zero-based terminal-cell coordinates, not UTF-8 offsets.
    pub cursor_column: u32,
    pub cursor_row: u32,
}

/// Full visible viewport and its metadata; never prompt extraction/scrollback.
///
/// Capture must sample text and cursor in one bounded synchronous tmux command
/// list, not separate subprocess calls. A tmux 3.8 private probe matched 150
/// changing printable-output samples with their cursor metadata. This does NOT
/// make an application's multi-write redraw atomic, freeze user input, or prove
/// a particular clear completed. Partial/unknown frames remain unsafe.
///
/// Captured: tmux 3.8, 2026-11-14, private shell/printable-output probes.
/// `control/fixtures/tmux_3.8_reserved_observations.json` records a metadata →
/// `capture-pane -p -N` (optionally `-e`) → end-marker command list, filtered
/// `list-panes`, and local `show-options -p -q -v` output. All 150 changing-output
/// samples agreed with the first metadata's cursor (59 distinct positions).
/// Local absent/inherited tags produce no bytes; an empty local tag produces
/// one newline. A missing-target display can succeed with empty fields, so it
/// is not positive liveness evidence. Versions 3.6a and 3.4 remain unverified.
///
/// Preserve meaningful whitespace and rows. A consumer may construct its
/// shared ScreenCursor from the inspection's coordinates when its adapter
/// supports them; otherwise pass None, and request Plain for text-only parsers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneCapture {
    pub text: String,
    pub inspection: PaneInspection,
    pub format: CaptureFormat,
}

/// An opaque capability minted only by the supervisor, never a drop-unlock guard.
///
/// Copies have the same lifetime/fence, not independent ownership. No public
/// constructor/release method: only `run_reserved` can acquire and release.
/// Private actor/operation/phase identity fences every submitted operation.
#[derive(Debug, Clone)]
#[must_use]
pub struct Reservation {
    agent: AgentId,
    control: ControlHandle,
    fence: state::Fence,
    deadline: Instant,
}

impl ControlHandle {
    /// Run one daemon-owned job with exclusive agent/pane ownership.
    ///
    /// Admission is bounded (32); no task is spawned per unadmitted waiter.
    /// After admission the supervisor owns work despite dropped client replies.
    /// Work waits for readiness outside the actor, but submits every short pane
    /// effect through its Reservation. Observe *after* the triggering effect and
    /// finish the durable readiness/delivery report before returning from work.
    ///
    /// Cleanup always runs after acquired work, even on success (normally a
    /// no-op then), with a fresh phase capability and separate 10-second budget.
    /// Old-phase work is stopped/fenced before cleanup. After its acknowledgement
    /// the supervisor releases; failed cleanup quarantines instead. Admission
    /// failure invokes neither callback. No external callback may outlive work.
    /// If both work and cleanup fail, preserve the work error; a cleanup failure
    /// after successful work is returned instead. Timeout/worker failure converts
    /// ReservationError through E. None of these failures rolls back effects.
    ///
    /// Pane effects remain typed stubs at this checkpoint; ownership is live.
    pub async fn run_reserved<T, E, W, WF, C, CF>(
        &self,
        request: ReservationRequest,
        work: W,
        cleanup: C,
    ) -> Result<T, E>
    where
        T: Send + 'static,
        E: From<ReservationError> + Send + 'static,
        W: FnOnce(Reservation) -> WF + Send + 'static,
        WF: Future<Output = Result<T, E>> + Send + 'static,
        C: FnOnce(Reservation, Completion) -> CF + Send + 'static,
        CF: Future<Output = Result<(), E>> + Send + 'static,
    {
        state::validate(&request, Instant::now())?;
        let deadline = request.deadline;
        let (job, response) = jobs::job(work, cleanup);
        self.enqueue_reserved(
            jobs::Command::Acquire(jobs::Admission {
                request,
                control: self.clone(),
                job,
            }),
            deadline,
        )
        .await?;
        // From enqueue onward the daemon owns the job, not this reply future.
        response.await.map_err(|_| ReservationError::Unavailable)?
    }
}

impl Reservation {
    async fn check(&self, pane: Option<&PaneId>) -> Result<(), ReservationError> {
        let deadline = Instant::now() + super::service::ACTION_TIMEOUT;
        self.control
            .reservation_request(deadline, |reply| jobs::Command::Check {
                fence: self.fence.clone(),
                pane: pane.cloned(),
                reply,
            })
            .await
    }

    async fn begin_cleanup(&self, deadline: Instant) -> Result<Self, ReservationError> {
        let fence = self
            .control
            .reservation_request(deadline, |reply| jobs::Command::Cleanup {
                fence: self.fence.clone(),
                deadline,
                reply,
            })
            .await?;
        Ok(Self {
            fence,
            deadline,
            ..self.clone()
        })
    }

    #[must_use]
    pub fn agent(&self) -> &AgentId {
        &self.agent
    }

    /// Inspect an owned pane; None requires positively established absence.
    /// Transport failure or malformed output is an error, never absence.
    pub async fn inspect(
        &self,
        _pane: &PaneId,
    ) -> Result<Option<PaneInspection>, ReservationError> {
        self.check(Some(_pane)).await?;
        Err(ReservationError::NotImplemented {
            operation: "inspect",
        })
    }

    /// Set this agent's local tag after checking expected at the actor boundary.
    /// Absent is allowed only for explicit adoption or this job's newly split pane;
    /// an Agent expectation must equal self.agent. Never steal a foreign tag.
    pub async fn tag(
        &self,
        _pane: &PaneId,
        _expected: ExpectedTag,
    ) -> Result<(), ReservationError> {
        self.check(Some(_pane)).await?;
        Err(ReservationError::NotImplemented { operation: "tag" })
    }

    /// Remove only this agent's matching local tag, without touching the process.
    pub async fn untag(&self, _pane: &PaneId, _expected: AgentId) -> Result<(), ReservationError> {
        self.check(Some(_pane)).await?;
        Err(ReservationError::NotImplemented { operation: "untag" })
    }

    /// One allowed key, after checking the matching agent tag inside the actor.
    /// This unguarded form is retained for legacy callers, not U8/U9b input:
    /// an unguarded Enter after paste is never safe. Use [`Self::keys_guarded`].
    pub async fn keys(
        &self,
        _pane: &PaneId,
        _expected: AgentId,
        _key: PaneKey,
    ) -> Result<(), ReservationError> {
        self.check(Some(_pane)).await?;
        Err(ReservationError::NotImplemented { operation: "keys" })
    }

    /// Literal text with no implicit Enter, after matching-tag/fence checks.
    /// Reuses TmuxClient::paste_text; adapters/callers own editor safety policy.
    /// U8/U9b use [`Self::paste_guarded`], not a prior capture plus this call.
    pub async fn paste(
        &self,
        _pane: &PaneId,
        _expected: AgentId,
        _text: String,
    ) -> Result<(), ReservationError> {
        self.check(Some(_pane)).await?;
        Err(ReservationError::NotImplemented { operation: "paste" })
    }

    /// Re-capture, run a caller-supplied pure screen check, then send one key.
    ///
    /// The implementation contract is one actor turn: validate fence/ownership
    /// and expected agent, capture in `format`, check the fresh local tag, invoke
    /// `check` on the fresh capture, and send only if it returns true. False
    /// returns [`ReservationError::ScreenChanged`] without sending anything.
    /// `check` must be quick and pure: no I/O, awaiting, or blocking. Build it
    /// from the adapter's observation policy, allowing irrelevant chrome changes;
    /// no vendor knowledge or whole-frame equality is built into this actor.
    ///
    /// U8 uses this for launch Enter and every trust/update/clear-dialog key;
    /// U9b uses it for delivery Enter after observing the displayed draft. An
    /// unguarded Enter after a paste is never safe for either consumer. This
    /// removes actor queue delay between approval and effect, not external races.
    ///
    /// **Contract checkpoint:** typed NotImplemented; `check` is not invoked.
    pub async fn keys_guarded<P>(
        &self,
        pane: &PaneId,
        _expected: AgentId,
        _key: PaneKey,
        _format: CaptureFormat,
        _check: P,
    ) -> Result<(), ReservationError>
    where
        P: FnOnce(&PaneCapture) -> bool + Send + 'static,
    {
        self.check(Some(pane)).await?;
        Err(ReservationError::NotImplemented {
            operation: "keys_guarded",
        })
    }

    /// Re-capture, run a caller-supplied pure screen check, then paste literal
    /// text without Enter. Same actor-turn, predicate and failure contract as
    /// [`Self::keys_guarded`]. U8 uses it for launch/clear text; U9b for delivery
    /// text. A true predicate must authorize this text in this editor/shell,
    /// not merely recognize the harness's name or a still-matching pane tag.
    /// Observe the resulting draft and use keys_guarded to submit it.
    ///
    /// **Contract checkpoint:** typed NotImplemented; `check` is not invoked.
    pub async fn paste_guarded<P>(
        &self,
        pane: &PaneId,
        _expected: AgentId,
        _text: String,
        _format: CaptureFormat,
        _check: P,
    ) -> Result<(), ReservationError>
    where
        P: FnOnce(&PaneCapture) -> bool + Send + 'static,
    {
        self.check(Some(pane)).await?;
        Err(ReservationError::NotImplemented {
            operation: "paste_guarded",
        })
    }

    /// Capture only a pane owned by this reservation, with no vendor flags.
    /// Missing pane is PaneGone, invalid metadata is InvalidObservation.
    pub async fn capture(
        &self,
        _pane: &PaneId,
        _format: CaptureFormat,
    ) -> Result<PaneCapture, ReservationError> {
        self.check(Some(_pane)).await?;
        Err(ReservationError::NotImplemented {
            operation: "capture",
        })
    }

    /// Up/Respawn: split and reserve the returned pane in the same actor turn.
    /// The placement pane may be unmanaged; this grants no input permission on it.
    /// Before launch input, tag Absent then durably record PaneCreated.
    pub async fn split(
        &self,
        _context: SpawnContext,
        _direction: PaneDirection,
        _size: String,
    ) -> Result<PaneId, ReservationError> {
        self.check(None).await?;
        Err(ReservationError::NotImplemented { operation: "split" })
    }

    /// Kill once after fence/tag checks, then submit ControlKilled through the
    /// configured OrgHandle using the supplied action/time. Audit failure never
    /// fails/repeats a successful kill. The owned continuation attempts audit even
    /// if the reply future disappears; persistence does not block the tmux actor.
    /// No success audit is invented for a failed or outcome-unknown kill.
    pub async fn kill(
        &self,
        _pane: &PaneId,
        _expected: AgentId,
        _action: serde_json::Value,
        _now: DateTime<Utc>,
    ) -> Result<(), ReservationError> {
        self.check(Some(_pane)).await?;
        Err(ReservationError::NotImplemented { operation: "kill" })
    }
}

/// Expected failures are values; no effect may be retried merely on timeout.
#[derive(Debug, thiserror::Error)]
pub enum ReservationError {
    #[error("reserved control operation '{operation}' is not implemented")]
    NotImplemented { operation: &'static str },
    #[error("invalid reservation request: {reason}")]
    InvalidRequest { reason: String },
    #[error("agent {agent} or pane {pane:?} is reserved")]
    Busy {
        agent: AgentId,
        pane: Option<PaneId>,
    },
    #[error("stale pane reservation")]
    StaleReservation,
    #[error("pane {pane} no longer passes the approved screen check; nothing was sent")]
    ScreenChanged { pane: PaneId },
    #[error("pane {pane} tag mismatch: expected {expected:?}, found {actual:?}")]
    TagMismatch {
        pane: PaneId,
        expected: ExpectedTag,
        actual: Option<String>,
    },
    #[error("pane {pane} is gone")]
    PaneGone { pane: PaneId },
    #[error("invalid observation of pane {pane}: {reason}")]
    InvalidObservation { pane: PaneId, reason: String },
    #[error("reserved control service is unavailable")]
    Unavailable,
    #[error("reserved control operation timed out; admitted effects may have completed")]
    Timeout,
    #[error(transparent)]
    Control(#[from] ControlError),
}

#[cfg(test)]
#[path = "reservation_contract_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "reservation_tests.rs"]
mod ownership_tests;
