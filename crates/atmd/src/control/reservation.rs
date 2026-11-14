//! Exclusive, fenced control for an agent's lifecycle or one delivery.
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
//! Local `atm reply`, `interrupt`, `kill` and the TUI also bypass this actor.
//! Pane IDs can be reused after a tmux server restart; restart/reconcile atmd too.
//!
//! Exactly one control actor serves a daemon. U8/U9b must clone the same handle
//! installed in `MethodServices.control` for RPC, never start a second actor.
//! Organisation reads for legacy gating must use a snapshot/watch mirror outside
//! the tmux actor; never await an organisation snapshot during a tmux turn.
//!
//! Work has an absolute deadline (at most 120 seconds from admission), with
//! each short action also bounded by the existing 30-second action budget.
//! Cleanup has a separate 10-second budget and a new phase capability, after
//! old work has stopped and admitted effects have been accounted for. Failed
//! cleanup quarantines the agent/panes: mutations fail unavailable until daemon
//! restart and lifecycle reconciliation. There is no unlock API here; quarantine
//! state must remain distinct from released state for a future resolve operation.
//! Active jobs plus parked admissions have 32 slots; a full pool immediately
//! returns Unavailable. Quarantines use a separate bounded 32-entry store, not
//! live-job slots. Tombstone overflow fails all later control closed until restart
//! rather than evicting uncertain ownership. A split returning an already-owned
//! pane ID likewise fails closed: do not destroy an ambiguously identified pane.
//!
//! Acquisition is all-or-nothing for agent and pane, FIFO among overlapping
//! requests. Independent keys proceed. At most one waiter of each kind may
//! overlap an agent or pane (further requests return Busy). Consumers must coalesce
//! triggers to one admitted-or-parked job per (consumer, agent). Lifecycle callers
//! use Wait; U8 itself suppresses duplicate lifecycle jobs before admission.
//! The maximum return budget is 120 s work + 10 s cleanup: U8's router ceiling
//! must exceed 130 s (use 135 s), with a larger client deadline.
//!
//! # Lifecycle caller recipe
//!
//! All steps in each row after acquisition belong to **one** `run_reserved`
//! work closure. Submit durable commands through the caller's sole
//! [`crate::org::OrgHandle`], never by directly appending an event. On failure,
//! its cleanup closure records the appropriate durable failure before release.
//! Cleanup decides from a fresh organisation snapshot ordered after the aborted
//! work's accepted commands: a missing acknowledgement does not mean no commit.
//! A rejected precheck must not mark an otherwise unchanged ready agent stuck.
//!
//! | Operation | Calls, in order |
//! |---|---|
//! | Up | Validate role and use [`ControlHandle::prepare_spawn`] before admission. Request Up with no pane; execute `OrgCommand::Up`; [`Reservation::split`]; [`Reservation::tag`] with Absent; execute `PaneCreated`; [`Reservation::paste_guarded`] launch text then [`Reservation::keys_guarded`] Enter, each with the caller's pure expected-screen check; guard trust/update-dialog keys too; [`Reservation::capture`] and registry observation; execute `SessionBound` or `EditorReady`; return, cleanup, release. |
//! | Adopt | Request Adopt with its pane; [`Reservation::inspect`]; tag Absent only for explicitly untagged adoption, otherwise require this agent's tag; re-read eligible registry row; execute `Adopt`; return. Never send keys or restart. |
//! | Clear | Request Clear before prechecks; inspect/capture exact ownership and editor; subscribe and take baseline; execute `Clear`; paste_guarded adapter clear command; observe the displayed draft, then keys_guarded Enter with the adapter-derived draft/dialog check; guard any subsequent dialog keys too; await post-command evidence outside actor; capture again; execute `SessionBound`/`EditorReady` before returning. |
//! | Respawn | Validate role/placement first; request Respawn with old pane if any. U8 chooses kill/split order so it never kills the placement target or last pane before splitting. Use matching-tag [`Reservation::kill`], durable `Respawn`, split/tag/`PaneCreated` and guarded launch/readiness as Up. Old/new pane ownership lasts until release. |
//! | Down | Request Down with current pane, or no pane for a pane-less agent; inspect if present; kill with audit or [`Reservation::untag`]; execute `Down`; return. Never act on a foreign replacement pane. |
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
//! Before either predicate or input, fresh metadata refuses dead panes, any mode,
//! and multiline paste when bracketed paste is explicitly off (U7's rule).
//! tmux before 3.7 reports no flag: unknown remains allowed, not a guarantee
//! against line submission. Reserved paste rejects ASCII control bytes except
//! tab, LF and CR, including injected bracketed-paste terminators on older tmux.
//!
//! Do not call legacy `send` or `type_spawn_command` inside a reservation: those
//! paths wait behind its ownership. Use reserved guarded paste/keys instead.
//! Both raw send and spawn_type refuse during Clear (including durable Clearing
//! after restart). Other reservation kinds make them wait within their original
//! action budget and re-resolve/recheck, without retry. Noncanonical tmux targets
//! (including `%05` aliases and spawn_type's named targets) resolve to `#{pane_id}`
//! before every gate; an empty/invalid result never authorizes input. The parked
//! legacy queue has 32 slots independent of reservation jobs; overflow returns
//! unavailable. Org projection catch-up also parks outside tmux turns, within the
//! same deadline. Parked legacy input yields to reservation waiters on its pane,
//! even later arrivals; a steady stream of them can starve it until Timeout.
//! A dropped caller does not cancel admitted parked input: it can still start
//! within its original deadline. After the first effect, Enter is completed under
//! the tmux client's timeout, even if the caller's response budget has expired.
//! An excluded old session
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
#[path = "kill_audit.rs"]
pub(super) mod kill_audit;
#[path = "pane_ops.rs"]
pub(super) mod pane_ops;
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
    /// None for agent-only Up, Respawn, Down or Observe; other kinds need a pane.
    pub pane: Option<PaneId>,
    pub kind: ReservationKind,
    pub wait: WaitPolicy,
    pub deadline: Instant,
}

impl ReservationRequest {
    /// Prefer this constructor over a struct literal so new metadata can be added.
    #[must_use]
    pub fn new(
        agent: AgentId,
        pane: Option<PaneId>,
        kind: ReservationKind,
        wait: WaitPolicy,
        deadline: Instant,
    ) -> Self {
        Self {
            agent,
            pane,
            kind,
            wait,
            deadline,
        }
    }
}

/// Outcome of work, supplied to cleanup while ownership is still exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Completion {
    /// Work returned Ok, not a guarantee that cleanup will succeed.
    Succeeded,
    /// Work returned its error value.
    Failed,
    /// The absolute work deadline expired; accepted effects are not rolled back.
    TimedOut,
    /// Work panicked/was cancelled, or daemon shutdown stopped it.
    WorkerStopped,
}

/// A pane-local option expectation, checked again at mutation time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExpectedTag {
    /// Only explicit Adopt or a pane just created by this Up/Respawn may use it.
    /// Matches only an unset pane-local option. Empty is Some("") and mismatches;
    /// inherited window/global values are ignored.
    Absent,
    Agent(AgentId),
}

impl fmt::Display for ExpectedTag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Absent => f.write_str("absent"),
            Self::Agent(agent) => agent.fmt(f),
        }
    }
}
fn optional_pane(pane: &Option<PaneId>) -> &str {
    pane.as_ref().map_or("no pane", PaneId::as_str)
}

/// The entire nonliteral-key surface. Text belongs in [`Reservation::paste`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneKey {
    Enter,
    Escape,
    Up,
    Down,
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
    /// Preserve empty or malformed values. None means the local tag query
    /// returned no bytes; the pane may have vanished after the metadata read.
    pub local_agent_tag: Option<String>,
    pub dead: bool,
    pub in_mode: bool,
    /// None means unknown (for example, a tmux version without this format).
    pub bracketed_paste: Option<bool>,
    /// Zero-based terminal-cell coordinates, not UTF-8 offsets.
    pub cursor_column: u32,
    pub cursor_row: u32,
    /// Foreground PID, not a process group. tmux 3.6/3.8 do not expose it:
    /// None must not be replaced by pane_pid or inferred from a command name.
    pub foreground_pid: Option<u32>,
    /// Neutral pane_current_command, never proof that the shell is back. A
    /// same-named wrapper can report the shell while its child runs, even with
    /// job control enabled. A dead pane can report its start command; immediately
    /// after creation the name can transiently be tmux or a wrapper shell.
    /// U8 reads procfs once for its baseline, outside any guard; these facts alone
    /// cannot establish positive shell/editor identity.
    pub foreground_command: Option<String>,
}

impl PaneInspection {
    /// Prefer this constructor over a struct literal so new metadata can be added.
    #[must_use]
    pub fn new(
        pane: PaneInfo,
        local_agent_tag: Option<String>,
        dead: bool,
        in_mode: bool,
        bracketed_paste: Option<bool>,
        cursor_column: u32,
        cursor_row: u32,
    ) -> Self {
        Self {
            pane,
            local_agent_tag,
            dead,
            in_mode,
            bracketed_paste,
            cursor_column,
            cursor_row,
            foreground_pid: None,
            foreground_command: None,
        }
    }

    /// The tmux pane's root process, not necessarily its interactive shell.
    #[must_use]
    pub fn pane_root_pid(&self) -> Option<u32> {
        (self.pane.pane_pid != 0).then_some(self.pane.pane_pid)
    }

    /// Builder for neutral observations/consumer fixtures; no classification.
    #[must_use]
    pub fn with_foreground_process(mut self, pid: Option<u32>, command: Option<String>) -> Self {
        self.foreground_pid = pid;
        self.foreground_command = command;
        self
    }
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
/// is not positive liveness evidence. The separate local tag read also returns
/// no bytes if the pane disappears after metadata; no external-client CAS is
/// promised. Mutations of that missing canonical pane fail, never retarget.
/// Grouped/linked metadata accepts agreeing pane fields, retaining the first
/// line's session/window. Versions 3.6a and 3.4 remain unverified.
///
/// Preserve meaningful whitespace and rows. A consumer may construct its
/// shared ScreenCursor from the inspection's coordinates when its adapter
/// supports them; otherwise pass None, and request Plain for text-only parsers.
///
/// Current v2 frames include `inspection.pane_root_pid()`, `foreground_pid` and
/// `foreground_command` from the same command list as the screen, including
/// captures supplied to both guard predicates. These are neutral observations,
/// never shell/harness classifications. Verified tmux 3.6/3.8 expose the root PID
/// and command but no foreground PID (None). U8 owns the spawn/adopt shell
/// baseline; a command-name match cannot substitute for unknown PID identity.
/// `focused` is sampled by list-clients in that command list. Malformed/unknown
/// client data gives None; a command/transport failure fails the whole capture
/// rather than inventing a frame or unfocused state. Clients/processes may move
/// between the read and effect, even within one actor turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneCapture {
    pub text: String,
    pub inspection: PaneInspection,
    pub format: CaptureFormat,
    /// Any client with the exact focused flag showing this canonical pane.
    /// Some(false) is observed absence; None is unknown, never permission.
    /// Sampled in the frame's command list. A client may move before an effect.
    pub focused: Option<bool>,
}

impl PaneCapture {
    /// Prefer this constructor over a struct literal so new metadata can be added.
    #[must_use]
    pub fn new(text: String, inspection: PaneInspection, format: CaptureFormat) -> Self {
        Self {
            text,
            inspection,
            format,
            focused: None,
        }
    }

    #[must_use]
    pub fn with_focus(mut self, focused: Option<bool>) -> Self {
        self.focused = focused;
        self
    }
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
    /// Read through the sole actor without acquiring ownership or changing a pane.
    /// Includes metadata, local tag, viewport (-N, optionally styled), focus and
    /// neutral process facts. Captures of one pane are paced at least 250ms apart,
    /// including guarded input, without blocking other panes. No cached frame is
    /// returned. The scheduler bounds pending captures and recent pane keys to
    /// 32 each; overflow is Unavailable. The original 30-second deadline includes
    /// pacing/queue time.
    /// U8 owns any spawn/adopt shell identity baseline and its interpretation;
    /// a command-name match alone is not positive process identity evidence.
    pub async fn observe(
        &self,
        pane: &PaneId,
        format: CaptureFormat,
    ) -> Result<PaneCapture, ReservationError> {
        let deadline = Instant::now() + super::service::ACTION_TIMEOUT;
        self.reservation_request(deadline, |reply| jobs::Command::Observe {
            pane: pane.clone(),
            format,
            deadline,
            reply,
        })
        .await
    }

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
    /// Cleanup must take a fresh organisation snapshot before deciding what failed;
    /// work commands may have committed even when their reply was lost. E: Display
    /// lets the supervisor log a cleanup error even when preserving the work error.
    pub async fn run_reserved<T, E, W, WF, C, CF>(
        &self,
        request: ReservationRequest,
        work: W,
        cleanup: C,
    ) -> Result<T, E>
    where
        T: Send + 'static,
        E: From<ReservationError> + fmt::Display + Send + 'static,
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
    async fn operation(
        &self,
        action: pane_ops::Action,
    ) -> Result<pane_ops::Output, ReservationError> {
        // The actor checks the phase fence before its work deadline, so old
        // capabilities report StaleReservation even after their deadline.
        let deadline = Instant::now() + super::service::ACTION_TIMEOUT;
        self.control
            .reservation_request(deadline, |reply| jobs::Command::Pane {
                fence: self.fence.clone(),
                action,
                deadline,
                reply,
            })
            .await
    }

    async fn effect(&self, action: pane_ops::Action) -> Result<(), ReservationError> {
        match self.operation(action).await? {
            pane_ops::Output::Done => Ok(()),
            _ => Err(ReservationError::Unavailable),
        }
    }

    async fn drain_audits(&self, deadline: Instant) -> Result<(), ReservationError> {
        let pending = self
            .control
            .reservation_request(deadline, |reply| jobs::Command::Drain {
                fence: self.fence.clone(),
                reply,
            })
            .await?;
        for audit in pending {
            tokio::time::timeout_at(deadline, audit.wait())
                .await
                .map_err(|_| ReservationError::Timeout)?;
        }
        Ok(())
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

    /// Inspect any canonical pane, including an unowned placement pane. This
    /// grants no mutation/capture permission. None requires positive absence.
    /// Transport failure or malformed output is an error, never absence.
    pub async fn inspect(&self, pane: &PaneId) -> Result<Option<PaneInspection>, ReservationError> {
        match self
            .operation(pane_ops::Action::Inspect(pane.clone()))
            .await?
        {
            pane_ops::Output::Inspection(inspection) => Ok(inspection),
            _ => Err(ReservationError::Unavailable),
        }
    }

    /// Set this agent's local tag after checking expected at the actor boundary.
    /// Absent is allowed only for explicit adoption or this job's newly split pane;
    /// an Agent expectation must equal self.agent. Never steal a foreign tag.
    pub async fn tag(&self, pane: &PaneId, expected: ExpectedTag) -> Result<(), ReservationError> {
        self.effect(pane_ops::Action::Tag(pane.clone(), expected))
            .await
    }

    /// Down/Respawn: remove only this agent's matching local tag. Every mutating
    /// method requires expected == self.agent(), otherwise InvalidRequest.
    pub async fn untag(&self, pane: &PaneId, expected: AgentId) -> Result<(), ReservationError> {
        self.effect(pane_ops::Action::Untag(pane.clone(), expected))
            .await
    }

    /// Up/Respawn/Clear/Delivery: one allowed key. expected must equal self.agent();
    /// the matching agent tag and live/non-mode state are checked in the actor.
    /// This unguarded form is retained for legacy callers, not U8/U9b input:
    /// an unguarded Enter after paste is never safe. Use [`Self::keys_guarded`].
    pub async fn keys(
        &self,
        pane: &PaneId,
        expected: AgentId,
        key: PaneKey,
    ) -> Result<(), ReservationError> {
        self.effect(pane_ops::Action::Input {
            pane: pane.clone(),
            expected,
            input: pane_ops::Input::Key(key),
            guard: None,
        })
        .await
    }

    /// Up/Respawn/Clear/Delivery: paste text without a separate Enter key, after
    /// tag/fence and input-safety checks. Multiline text may still submit lines
    /// on tmux before 3.7, whose bracketed-paste state is unknown (U7 policy).
    /// expected must equal self.agent(), otherwise InvalidRequest.
    /// Reuses TmuxClient::paste_text; adapters/callers own editor safety policy.
    /// U8/U9b use [`Self::paste_guarded`], not a prior capture plus this call.
    pub async fn paste(
        &self,
        pane: &PaneId,
        expected: AgentId,
        text: String,
    ) -> Result<(), ReservationError> {
        self.effect(pane_ops::Action::Input {
            pane: pane.clone(),
            expected,
            input: pane_ops::Input::Paste(text),
            guard: None,
        })
        .await
    }

    /// Re-capture, run a caller-supplied pure screen check, then send one key.
    ///
    /// The implementation contract is one actor turn: validate fence/ownership
    /// and expected agent, capture in `format`, check the fresh local tag and
    /// input safety, invoke `check`, and send only if it returns true. False
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
    /// expected must equal self.agent(). A panicking predicate is rejected as
    /// ScreenChanged; callers must still never block or perform I/O in it.
    pub async fn keys_guarded<P>(
        &self,
        pane: &PaneId,
        expected: AgentId,
        key: PaneKey,
        format: CaptureFormat,
        check: P,
    ) -> Result<(), ReservationError>
    where
        P: FnOnce(&PaneCapture) -> bool + Send + 'static,
    {
        self.effect(pane_ops::Action::Input {
            pane: pane.clone(),
            expected,
            input: pane_ops::Input::Key(key),
            guard: Some((format, Box::new(check))),
        })
        .await
    }

    /// Re-capture, run a caller-supplied pure screen check, then paste literal
    /// text without a separate Enter key (see [`Self::paste`] for the pre-3.7
    /// multiline limit). Same actor-turn, predicate and failure contract as
    /// [`Self::keys_guarded`]. U8 uses it for launch/clear text; U9b for delivery
    /// text. A true predicate must authorize this text in this editor/shell,
    /// not merely recognize the harness's name or a still-matching pane tag.
    /// Observe the resulting draft and use keys_guarded to submit it.
    ///
    /// expected must equal self.agent(); same kind restrictions as paste.
    pub async fn paste_guarded<P>(
        &self,
        pane: &PaneId,
        expected: AgentId,
        text: String,
        format: CaptureFormat,
        check: P,
    ) -> Result<(), ReservationError>
    where
        P: FnOnce(&PaneCapture) -> bool + Send + 'static,
    {
        self.effect(pane_ops::Action::Input {
            pane: pane.clone(),
            expected,
            input: pane_ops::Input::Paste(text),
            guard: Some((format, Box::new(check))),
        })
        .await
    }

    /// Capture only a pane owned by this reservation, with no vendor flags.
    /// Missing pane is PaneGone, invalid metadata is InvalidObservation.
    pub async fn capture(
        &self,
        pane: &PaneId,
        format: CaptureFormat,
    ) -> Result<PaneCapture, ReservationError> {
        match self
            .operation(pane_ops::Action::Capture(pane.clone(), format))
            .await?
        {
            pane_ops::Output::Capture(capture) => Ok(capture),
            _ => Err(ReservationError::Unavailable),
        }
    }

    /// Up/Respawn: split and reserve the returned pane in the same actor turn.
    /// The placement pane may be unmanaged; this grants no input permission on it.
    /// Before launch input, tag Absent then durably record PaneCreated. At most
    /// one new pane per job bounds ownership storage; old/new panes remain held.
    pub async fn split(
        &self,
        context: SpawnContext,
        direction: PaneDirection,
        size: String,
    ) -> Result<PaneId, ReservationError> {
        match self
            .operation(pane_ops::Action::Split(context, direction, size))
            .await?
        {
            pane_ops::Output::Pane(pane) => Ok(pane),
            _ => Err(ReservationError::Unavailable),
        }
    }

    /// Down/Respawn: kill once after fence/tag checks; expected must equal
    /// self.agent(). Reserve a slot in the actor's sole audit queue before the
    /// effect; a full/closed queue refuses the kill with Unavailable. On success,
    /// enqueue ControlKilled through that slot using the supplied action/time,
    /// then await its acknowledgement outside the actor. At most two pending
    /// kills per job allow cleanup of a replacement after a dropped first wait.
    /// A disabled organisation still permits the kill but cannot persist an audit.
    /// The actor/supervisor retain the acknowledgement even if this future drops. Audit failure never
    /// fails/repeats a successful kill. The owned continuation attempts audit even
    /// if the reply future disappears; persistence does not block the tmux actor.
    /// No success audit is invented for a failed or outcome-unknown kill.
    pub async fn kill(
        &self,
        pane: &PaneId,
        expected: AgentId,
        action: serde_json::Value,
        now: DateTime<Utc>,
    ) -> Result<(), ReservationError> {
        match self
            .operation(pane_ops::Action::Kill(pane.clone(), expected, action, now))
            .await?
        {
            pane_ops::Output::Audit(audit) => {
                if tokio::time::timeout_at(self.deadline, audit.wait())
                    .await
                    .is_err()
                {
                    tracing::warn!(%pane, "kill audit wait timed out; supervisor retains acknowledgement, kill is not retried");
                }
                Ok(())
            }
            _ => Err(ReservationError::Unavailable),
        }
    }
}

/// Expected failures are values; never retry merely because a reply failed.
///
/// # Effect certainty for consumers (U8 / U9b)
///
/// The variant docs describe **one pane-operation call**, especially paste/keys
/// and their guarded forms. "Before effect" permits read-only tmux queries, but
/// guarantees this call sent no input and made no pane mutation. It says nothing
/// about earlier calls. An error returned by `run_reserved` can follow successful
/// earlier work or cleanup effects, even when that error's variant is normally
/// pre-effect. Caller-created errors likewise carry no actor guarantee.
///
/// | Variant | One guarded paste/key call | General pane operation |
/// |---|---|---|
/// | NotImplemented | Before effect | Before effect |
/// | InvalidRequest | Before effect | May follow a split effect |
/// | Busy | Before effect (admission) | Before effect (admission) |
/// | StaleReservation | Before effect | Before effect |
/// | ScreenChanged | Before effect | Before effect |
/// | TagMismatch | Before effect | Before effect |
/// | PaneGone | Before effect | Before effect |
/// | InvalidObservation | Before effect | Before effect |
/// | Quarantined | Before effect (admission) | Before effect (admission) |
/// | Unavailable | May follow an effect | May follow an effect |
/// | Timeout | May follow an effect | May follow an effect |
/// | Control(SendRefused) | Before effect | Before effect |
/// | Control(other) | May follow an effect | May follow an effect |
///
/// A generic match on `Control` must therefore treat it as uncertain unless it
/// distinguishes `ControlError::SendRefused`. In particular, a tmux error can be
/// returned after load/paste or key execution; a timeout/closed reply does not
/// cancel an admitted action. None of these errors proves that a previous paste
/// was undone or that retrying cannot duplicate input.
#[derive(Debug, thiserror::Error)]
pub enum ReservationError {
    /// Before effect: the unsupported operation was not executed.
    #[error("reserved control operation '{operation}' is not implemented")]
    NotImplemented { operation: &'static str },
    /// Before effect for paste/keys (including guarded forms): invalid kind,
    /// expected agent, or request. For a general operation, may follow a split
    /// whose returned pane ID could not be validated. Do not classify an
    /// arbitrary run_reserved/split error as no-effect from this variant alone.
    #[error("invalid reservation request: {reason}")]
    InvalidRequest { reason: String },
    /// Before effect: admission refused ownership; callbacks did not run.
    #[error("agent {agent} or pane {} is reserved", optional_pane(.pane))]
    Busy {
        agent: AgentId,
        pane: Option<PaneId>,
    },
    /// Before effect: actor/operation/phase or owned-pane fence did not match.
    #[error("stale pane reservation")]
    StaleReservation,
    /// Before effect: the fresh predicate rejected (or panicked); no input sent.
    #[error("pane {pane} no longer passes the approved screen check; nothing was sent")]
    ScreenChanged { pane: PaneId },
    /// Before effect: mutation-time local tag did not match the expectation.
    #[error("pane {pane} tag mismatch: expected {expected}, found {}", actual.as_deref().unwrap_or("absent"))]
    TagMismatch {
        pane: PaneId,
        expected: ExpectedTag,
        actual: Option<String>,
    },
    /// Before effect: the read-only precheck positively found no such pane.
    #[error("pane {pane} is gone")]
    PaneGone { pane: PaneId },
    /// Before effect: pre-effect metadata/frame decoding failed. No input sent.
    #[error("invalid observation of pane {pane}: {reason}")]
    InvalidObservation { pane: PaneId, reason: String },
    /// Before effect: admission refused uncertain ownership; callbacks did not run.
    #[error("agent {agent} (pane {}) is quarantined; restart atmd and reconcile", optional_pane(.pane))]
    Quarantined {
        agent: AgentId,
        pane: Option<PaneId>,
    },
    /// Uncertain: may be pre-effect queue refusal (including no kill-audit slot),
    /// but may follow execution with a lost reply or split-claim failure.
    #[error("reserved control service is unavailable")]
    Unavailable,
    /// Uncertain: admission, pacing, action or reply may have timed out. Already
    /// admitted input may have executed, or may still execute; never infer rollback.
    #[error("reserved control operation timed out; admitted effects may have completed")]
    Timeout,
    /// Uncertain in general: tmux failure may follow a partial or complete effect.
    /// Exception: SendRefused is before effect. Mode/dead-pane, explicit-off
    /// multiline and unsafe-control-byte refusals all run before the predicate
    /// and before any input; they send nothing. Other Control variants must not
    /// be assumed pre-effect without a more specific caller contract.
    #[error(transparent)]
    Control(#[from] ControlError),
}

#[cfg(test)]
#[path = "reservation_contract_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "reservation_tests.rs"]
mod ownership_tests;
