//! The executor's pure safety decisions as tables, and the rig its attempt
//! tests run on: an in-memory organisation writer behind an append gate, the
//! message service's own fold loop as the one `Applied` writer, and the real
//! control actor over per-pane tmux scripts. No scheduler runs here: a test is
//! the recipient slot and calls the executor itself.
//!
//! Synchronization is by returned calls, the append gate and the fold cursor.
//! Time is paused and only in-memory I/O runs under it.
#![allow(clippy::unwrap_used, clippy::expect_used)]

/// The scheduler's production observer, tested on this file's rig.
#[path = "message_observer_tests.rs"]
mod observer;

use super::*;
use crate::{
    control::{
        reservation::{
            testing::ScriptedTmuxClient, CaptureFormat, Completion, ExpectedTag, PaneCapture,
            PaneId, PaneInspection, PaneKey, Reservation, ReservationKind, ReservationRequest,
            WaitPolicy,
        },
        service::spawn_control,
        ControlError,
    },
    eventlog::{Envelope, EventLogError},
    org::{
        agent_screen::{agent_screen, PaneBaseline},
        agent_screen_tests::body,
        agents::{spawn_agents_with, AgentHandle},
        message_delivery_gate::{Effect, GatedTmux},
        message_evidence::{AttemptScope, MessageEvidence},
        message_history::{
            DeliverySnapshot, HistoryUpdate, MessageHistoryHandle, RecipientObservation,
            SessionObservation,
        },
        message_scheduler::Effects,
        message_service_tests::{guarded, until, utc, worker},
        messages::{recipient_observation, spawn_service_for_test, MessageClock, MessageHandle},
        OrgHandle, Request,
    },
};
use atm_core::{
    harness_control::{ControlDialog, ControlKey},
    AgentType, DialogState, Harness, InputState, LifecycleEvent, Model, SessionDomain,
    SessionStatus, TurnState,
};
use atm_org::{
    delivery::{
        next_delivery, DeliveryPolicy, DeliverySettings, DeliveryStep, FailureReason, WaitReason,
    },
    Address, AgentId, AgentStatus, DeliveryStatus, MessageContent, MessageKind, OrgCommand,
    OrgEvent, OrgState,
};
use atm_tmux::{mock::MockCall, PaneInfo, TmuxError};
use std::{
    collections::HashSet,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    sync::{broadcast, mpsc, oneshot, watch, Semaphore},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

const NOT_STARTED: EffectCertainty = EffectCertainty::NotStarted;
const APPLIED: EffectCertainty = EffectCertainty::Applied;
const UNKNOWN: EffectCertainty = EffectCertainty::Unknown;

fn pane() -> PaneId {
    PaneId::parse("%1").unwrap()
}

// ---------------------------------------------------------------------------
// Error to certainty
// ---------------------------------------------------------------------------

/// One table row per variant. No `_` arm: a new variant needs an arm here,
/// and `VARIANTS` and the table then need its row.
fn variant(error: &ReservationError) -> usize {
    match error {
        ReservationError::NotImplemented { .. } => 0,
        ReservationError::InvalidRequest { .. } => 1,
        ReservationError::Busy { .. } => 2,
        ReservationError::StaleReservation => 3,
        ReservationError::ScreenChanged { .. } => 4,
        ReservationError::TagMismatch { .. } => 5,
        ReservationError::PaneGone { .. } => 6,
        ReservationError::InvalidObservation { .. } => 7,
        ReservationError::Quarantined { .. } => 8,
        ReservationError::Unavailable => 9,
        ReservationError::Timeout => 10,
        ReservationError::Control(_) => 11,
        ReservationError::PacingRefused { .. } => 12,
        ReservationError::PaneInputRefused { .. } => 13,
        ReservationError::UnsafePaste { .. } => 14,
    }
}
const VARIANTS: usize = 15;

fn control_errors() -> Vec<ControlError> {
    let pane = || "%1".to_string();
    vec![
        // Documented as before effect, and still Unknown here: the executor
        // never tells one `Control` from another. Reserved input reports a
        // pane that refuses input as `PaneInputRefused` instead.
        ControlError::SendRefused {
            pane: pane(),
            reason: "pane is in a mode",
        },
        ControlError::Tmux {
            context: "paste".into(),
            source: TmuxError::ParseError("lost".into()),
        },
        ControlError::NoSession { target: pane() },
        ControlError::NoPane {
            session: "s1".into(),
        },
        ControlError::AmbiguousTarget {
            target: pane(),
            count: 2,
            ids: "s1, s2".into(),
        },
        ControlError::AgentClearing {
            pane: pane(),
            agent: worker(),
        },
        ControlError::PaneQuarantined {
            pane: pane(),
            agent: worker(),
        },
        ControlError::OrganisationUnavailable { pane: pane() },
    ]
}

#[test]
fn reservation_error_certainty_table() {
    let mut table = vec![
        // "Before effect" for one guarded paste/key call in U8c's table.
        (
            ReservationError::NotImplemented { operation: "paste" },
            NOT_STARTED,
        ),
        (
            ReservationError::InvalidRequest {
                reason: "kind".into(),
            },
            NOT_STARTED,
        ),
        (
            ReservationError::Busy {
                agent: worker(),
                pane: Some(pane()),
            },
            NOT_STARTED,
        ),
        (ReservationError::StaleReservation, NOT_STARTED),
        (
            ReservationError::ScreenChanged { pane: pane() },
            NOT_STARTED,
        ),
        (
            ReservationError::PacingRefused { pane: pane() },
            NOT_STARTED,
        ),
        // A pane in a mode, dead, synchronized or with its input off.
        (
            ReservationError::PaneInputRefused {
                pane: pane(),
                reason: "pane is in a mode",
            },
            NOT_STARTED,
        ),
        (ReservationError::UnsafePaste { pane: pane() }, NOT_STARTED),
        (
            ReservationError::TagMismatch {
                pane: pane(),
                expected: ExpectedTag::Agent(worker()),
                actual: None,
            },
            NOT_STARTED,
        ),
        (ReservationError::PaneGone { pane: pane() }, NOT_STARTED),
        (
            ReservationError::InvalidObservation {
                pane: pane(),
                reason: "frame".into(),
            },
            NOT_STARTED,
        ),
        (
            ReservationError::Quarantined {
                agent: worker(),
                pane: None,
            },
            NOT_STARTED,
        ),
        // "May follow an effect".
        (ReservationError::Unavailable, UNKNOWN),
        (ReservationError::Timeout, UNKNOWN),
    ];
    table.extend(
        control_errors()
            .into_iter()
            .map(|error| (ReservationError::Control(error), UNKNOWN)),
    );

    let mut covered = [false; VARIANTS];
    for (error, expected) in &table {
        assert_eq!(certainty(error), *expected, "{error:?}");
        // Whatever the variant, an error never proves that input was typed.
        assert_ne!(certainty(error), APPLIED, "{error:?}");
        covered[variant(error)] = true;
    }
    assert_eq!(covered, [true; VARIANTS], "a variant has no table row");
    // The paste call's own result: only Ok says the text was typed.
    assert_eq!(paste_certainty(&Ok(())), APPLIED);
    let refused = Err(ReservationError::ScreenChanged { pane: pane() });
    assert_eq!(paste_certainty(&refused), NOT_STARTED);
    assert_eq!(paste_certainty(&Err(ReservationError::Timeout)), UNKNOWN);
}

// ---------------------------------------------------------------------------
// The guard decision over plain facts
// ---------------------------------------------------------------------------

fn screen(input: InputState, turn: TurnState, dialog: DialogState) -> ScreenFacts {
    let mut facts = ScreenFacts::default();
    facts.input = input;
    facts.turn = turn;
    facts.dialog = dialog;
    facts
}

/// Facts that allow `input`: the base every row changes one fact of.
fn allowing(input: GuardedInput) -> DeliveryGuardFacts {
    let (editor, staged) = match input {
        GuardedInput::Paste => (InputState::Empty, Some(false)),
        GuardedInput::Enter => (InputState::Draft, Some(true)),
    };
    DeliveryGuardFacts {
        input,
        screen: screen(editor, TurnState::NotRunning, DialogState::None),
        control_dialog: ControlDialog::None,
        staged,
        focused: Some(false),
        bracketed_paste: Some(true),
        in_mode: false,
        dead: false,
        synchronized: false,
        input_off: false,
    }
}

fn dialogs() -> Vec<DialogState> {
    vec![
        DialogState::Unknown,
        DialogState::FolderTrust {
            folder: "/demo".into(),
        },
        DialogState::Permission,
        DialogState::Other,
    ]
}

/// What the control read can show in place of "no dialog": unread, a popup
/// it cannot name, and dialogs whose own Enter would accept something.
fn control_dialogs() -> Vec<ControlDialog> {
    let key = ControlKey::Enter;
    vec![
        ControlDialog::Unknown,
        ControlDialog::Blocked,
        ControlDialog::LaunchSkip { key },
        ControlDialog::CurrentWorktree {
            selected: true,
            accept: key,
        },
        ControlDialog::FolderTrust {
            folder: "/demo".into(),
            selected_accept: true,
            select_accept: None,
            accept: key,
        },
    ]
}

#[test]
fn delivery_guard_plain_facts_table() {
    type Change = Box<dyn Fn(&mut DeliveryGuardFacts)>;
    for input in [GuardedInput::Paste, GuardedInput::Enter] {
        let base = allowing(input);
        assert!(guard_allows_input(&base), "{base:?}");

        // Each row changes one fact of the allowing base.
        let mut refusing: Vec<(&str, Change)> = vec![
            ("owner is focused", Box::new(|f| f.focused = Some(true))),
            ("focus is unknown", Box::new(|f| f.focused = None)),
            (
                "bracketed paste is off",
                Box::new(|f| f.bracketed_paste = Some(false)),
            ),
            ("pane is in a mode", Box::new(|f| f.in_mode = true)),
            ("pane is dead", Box::new(|f| f.dead = true)),
            (
                "the window's panes are synchronized",
                Box::new(|f| f.synchronized = true),
            ),
            ("the pane's input is off", Box::new(|f| f.input_off = true)),
            (
                "turn is running",
                Box::new(|f| f.screen.turn = TurnState::Running),
            ),
            (
                "turn is unknown",
                Box::new(|f| f.screen.turn = TurnState::Unknown),
            ),
            (
                "nothing is known",
                Box::new(|f| f.screen = ScreenFacts::default()),
            ),
        ];
        for dialog in dialogs() {
            refusing.push((
                "a dialog, or none proven",
                Box::new(move |f| f.screen.dialog = dialog.clone()),
            ));
        }
        // The control read's dialog refuses on its own, for either input,
        // with every other fact allowing.
        for dialog in control_dialogs() {
            refusing.push((
                "the control read shows a dialog, or none proven",
                Box::new(move |f| f.control_dialog = dialog.clone()),
            ));
        }
        for (why, change) in &refusing {
            let mut facts = base.clone();
            change(&mut facts);
            assert!(!guard_allows_input(&facts), "{why}: {facts:?}");
        }

        // Facts that leave the decision alone.
        let allowed: Vec<(&str, Change)> = vec![
            (
                "an unreported bracketed-paste flag (tmux before 3.7)",
                Box::new(|f| f.bracketed_paste = None),
            ),
            (
                "fresh-editor evidence is not a delivery fact",
                Box::new(|f| f.screen.fresh_editor = atm_core::FreshEditorEvidence::Present),
            ),
        ];
        for (why, change) in &allowed {
            let mut facts = base.clone();
            change(&mut facts);
            assert!(guard_allows_input(&facts), "{why}: {facts:?}");
        }
    }

    // Paste reads the editor, never the match: only a positively empty editor.
    for editor in [InputState::Unknown, InputState::Draft] {
        let mut facts = allowing(GuardedInput::Paste);
        facts.screen.input = editor;
        assert!(!guard_allows_input(&facts), "{facts:?}");
        // A match cannot stand in for the empty editor.
        facts.staged = Some(true);
        assert!(!guard_allows_input(&facts), "{facts:?}");
    }
    for staged in [Some(true), Some(false), None] {
        let mut facts = allowing(GuardedInput::Paste);
        facts.staged = staged;
        assert!(guard_allows_input(&facts), "{facts:?}");
    }

    // Enter reads the adapter's exact match, never the editor state alone.
    for staged in [Some(false), None] {
        for editor in [InputState::Draft, InputState::Empty, InputState::Unknown] {
            let mut facts = allowing(GuardedInput::Enter);
            facts.staged = staged;
            facts.screen.input = editor;
            assert!(!guard_allows_input(&facts), "{facts:?}");
        }
    }
    // Nor the match alone: a draft that matches in an editor read as empty
    // or unknown is a screen nobody understood, and gets no Enter.
    for editor in [InputState::Empty, InputState::Unknown] {
        let mut facts = allowing(GuardedInput::Enter);
        facts.screen.input = editor;
        assert_eq!(facts.staged, Some(true));
        assert!(!guard_allows_input(&facts), "{facts:?}");
    }

    // An exact match overrides no dialog, whatever else the screen shows
    // (the command-completion frames: a match under a popup).
    for dialog in dialogs() {
        for turn in [TurnState::NotRunning, TurnState::Unknown] {
            let mut facts = allowing(GuardedInput::Enter);
            facts.screen = screen(InputState::Draft, turn, dialog.clone());
            assert!(!guard_allows_input(&facts), "{facts:?}");
        }
    }
}

// ---------------------------------------------------------------------------
// Exit facts to disposition
// ---------------------------------------------------------------------------

const COMMITS: [AttemptCommit; 3] = [
    AttemptCommit::Absent,
    AttemptCommit::Unknown,
    AttemptCommit::Committed,
];
const CERTAINTIES: [EffectCertainty; 3] = [NOT_STARTED, APPLIED, UNKNOWN];
const TOKENS: [Option<bool>; 3] = [Some(true), Some(false), None];

fn exit(
    attempt: AttemptCommit,
    receipt: bool,
    paste: EffectCertainty,
    token_visible: Option<bool>,
) -> AttemptExitFacts {
    AttemptExitFacts {
        attempt,
        receipt,
        paste,
        token_visible,
        enter: NOT_STARTED,
    }
}

/// The same facts with an Enter call that returned Ok, or whose outcome is
/// unknown.
fn entered(enter: EffectCertainty, facts: AttemptExitFacts) -> AttemptExitFacts {
    AttemptExitFacts { enter, ..facts }
}

#[test]
fn attempt_exit_disposition_table() {
    use AttemptCommit::{Absent, Committed, Unknown as Lost};
    use AttemptDisposition::{Deliver, Fail, Hold, NoAttempt, Unaccountable};
    let visible = Hold(ExecutorHold::TokenVisible);
    let unknown = Hold(ExecutorHold::InputOutcomeUnknown);
    let staged = Hold(ExecutorHold::EnterNotSent);

    // The rows of the error disposition checklist, by what is known at exit.
    let rows = [
        // Before any attempt: a failed precheck, a moved pane, cancellation
        // before the commit, or an execute that returned no new envelope.
        (exit(Absent, false, NOT_STARTED, None), NoAttempt),
        // The attempt's reply was lost and the fence did not account for it.
        (exit(Lost, false, NOT_STARTED, None), Unaccountable),
        // Committed, and the paste was never dispatched or sent nothing: an
        // arm that failed, a refused guard, a deadline or cancellation
        // before the effect.
        (exit(Committed, false, NOT_STARTED, None), Fail),
        (exit(Committed, false, NOT_STARTED, Some(false)), Fail),
        // A token on the screen is not ours when we typed nothing.
        (exit(Committed, false, NOT_STARTED, Some(true)), Fail),
        // Applied paste and no Enter: the staging bound, a refused Enter on
        // an exact draft, a recipient that changed, cancellation after the
        // paste. With the token on screen the hold says so: the reference
        // is staged and nothing submitted it.
        (exit(Committed, false, APPLIED, Some(true)), staged),
        (exit(Committed, false, APPLIED, Some(false)), unknown),
        (exit(Committed, false, APPLIED, None), unknown),
        // The paste's own outcome is unknown: a timeout, a lost reply, work
        // aborted while it was dispatched.
        (exit(Committed, false, UNKNOWN, Some(true)), staged),
        (exit(Committed, false, UNKNOWN, Some(false)), unknown),
        (exit(Committed, false, UNKNOWN, None), unknown),
        // An Enter that returned Ok and no receipt at the confirmation
        // deadline (also one already past): unconfirmed, with the token.
        (
            entered(APPLIED, exit(Committed, false, APPLIED, Some(true))),
            visible,
        ),
        (
            entered(APPLIED, exit(Committed, false, APPLIED, None)),
            unknown,
        ),
        // An Enter whose outcome is unknown: the token says nothing more.
        (
            entered(UNKNOWN, exit(Committed, false, APPLIED, Some(true))),
            unknown,
        ),
        (
            entered(UNKNOWN, exit(Committed, false, APPLIED, None)),
            unknown,
        ),
        // An exact receipt wins: over a failure, over a hold, before the
        // paste, and for an older attempt's scope with no attempt of this run.
        (exit(Committed, true, NOT_STARTED, None), Deliver),
        (exit(Committed, true, APPLIED, Some(true)), Deliver),
        (exit(Committed, true, UNKNOWN, None), Deliver),
        (exit(Absent, true, NOT_STARTED, None), Deliver),
        (exit(Lost, true, NOT_STARTED, None), Deliver),
    ];
    for (facts, expected) in rows {
        assert_eq!(disposition(&facts), expected, "{facts:?}");
    }

    // The rules behind the rows, over every combination of facts.
    let all = COMMITS.into_iter().flat_map(|attempt| {
        CERTAINTIES.into_iter().flat_map(move |paste| {
            (CERTAINTIES.into_iter()).map(move |enter| (attempt, paste, enter))
        })
    });
    for (attempt, paste, enter) in all {
        {
            for token in TOKENS {
                let delivered = entered(enter, exit(attempt, true, paste, token));
                assert_eq!(disposition(&delivered), Deliver);
                let facts = entered(enter, exit(attempt, false, paste, token));
                let decided = disposition(&facts);
                assert_ne!(decided, Deliver, "{facts:?}");
                // Only a paste known to have sent nothing may be failed, and
                // only for a committed attempt.
                assert_eq!(
                    decided == Fail,
                    attempt == Committed && paste == NOT_STARTED,
                    "{facts:?}"
                );
                // "Nothing to settle" needs both: no attempt and no input.
                assert_eq!(
                    decided == NoAttempt,
                    attempt == Absent && paste == NOT_STARTED,
                    "{facts:?}"
                );
                if paste != NOT_STARTED {
                    // Something may have been typed: a hold, or an error
                    // when there is no attempt to hold it under.
                    // The reason: an unknown Enter first, then the token
                    // with what is known of the Enter.
                    let hold = match (enter, token) {
                        (UNKNOWN, _) => unknown,
                        (NOT_STARTED, Some(true)) => staged,
                        (APPLIED, Some(true)) => visible,
                        _ => unknown,
                    };
                    let expected = if attempt == Committed {
                        hold
                    } else {
                        Unaccountable
                    };
                    assert_eq!(decided, expected, "{facts:?}");
                }
                // The executor never reports that the draft's text differs.
                assert_ne!(decided, Hold(ExecutorHold::StagedMismatch), "{facts:?}");
            }
        }
    }
}

#[test]
fn attempt_exit_cleanup_table() {
    use AttemptDisposition::{Deliver, Fail, Hold, NoAttempt, Unaccountable};
    use Settled::{Held, LeftToPolicy, Neither, Written};
    let holds = [
        ExecutorHold::TokenVisible,
        ExecutorHold::InputOutcomeUnknown,
        ExecutorHold::StagedMismatch,
    ];
    let mut rows = vec![
        // Nothing is owed.
        (NoAttempt, Written, true),
        (NoAttempt, Held, true),
        (NoAttempt, Neither, true),
        (NoAttempt, LeftToPolicy, true),
        // The receipt stays retained until Delivered is folded: a stalled
        // organisation write is not a cleanup failure.
        (Deliver, Written, true),
        (Deliver, Held, true),
        (Deliver, Neither, true),
        (Deliver, LeftToPolicy, true),
        // A zero-effect failure: written, or held in its place when the
        // organisation stalled. With neither, nothing vetoes a retry.
        (Fail, Written, true),
        (Fail, Held, true),
        (Fail, Neither, false),
        // A final attempt recorded as having typed nothing: no retry can
        // follow it, so nothing is left to veto.
        (Fail, LeftToPolicy, true),
        // No sequence to retain anything under.
        (Unaccountable, Written, false),
        (Unaccountable, Held, false),
        (Unaccountable, Neither, false),
        (Unaccountable, LeftToPolicy, false),
    ];
    for hold in holds {
        // A retained hold is a successful cleanup, whatever could not be
        // persisted; an unretained one is the quarantine case. Nothing is
        // ever written for a hold, so "written" retains nothing.
        rows.push((Hold(hold), Held, true));
        rows.push((Hold(hold), Neither, false));
        rows.push((Hold(hold), Written, false));
        rows.push((Hold(hold), LeftToPolicy, false));
    }
    for (decided, settled, expected) in rows {
        assert_eq!(
            cleanup_succeeds(decided, settled),
            expected,
            "{decided:?} {settled:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// The reference
// ---------------------------------------------------------------------------

#[test]
fn inbox_reference_is_one_printable_ascii_line_for_any_id() {
    for seq in [1, 123, 123_456, u64::MAX] {
        let id = MessageId::new(seq);
        let reference = inbox_reference(id);
        assert_eq!(reference, format!("[m{seq}] Read: atm inbox --id m{seq}"));
        assert!(
            reference.bytes().all(|b| (b' '..=b'~').contains(&b)),
            "{reference:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// The token on the screen, and the session an attempt is confirmed against
// ---------------------------------------------------------------------------

fn shot(format: CaptureFormat, text: &str) -> PaneCapture {
    let mut capture = frame("demo/worker");
    capture.format = format;
    capture.text = text.into();
    capture
}

#[test]
fn token_split_by_style_or_wrap_still_visible() {
    let id = MessageId::new(123);
    let visible = |format, text: &str| token_visible(&shot(format, text), id);
    for format in [CaptureFormat::Ansi, CaptureFormat::Plain] {
        // One row, as the editor or the transcript shows the reference.
        assert_eq!(
            visible(format, "> [m123] Read: atm inbox --id m123   \n"),
            Some(true)
        );
        // A row break inside the token (a pane narrower than the token).
        assert_eq!(visible(format, "> [m1\n23] Read: atm\n"), Some(true));
        // Another message's token, and a longer id that starts the same.
        assert_eq!(visible(format, "> [m124] Read: atm\n"), Some(false));
        assert_eq!(visible(format, "> [m1234] Read: atm\n"), Some(false));
        // The id without its brackets (the command's own argument) is no token.
        assert_eq!(visible(format, "atm inbox --id m123\n"), Some(false));
        assert_eq!(visible(format, "\n\n"), Some(false));
    }
    // A style change inside the token: the raw bytes do not hold it, the
    // decoded cells do.
    let styled = "\x1b[1m> [m1\x1b[0m23] Read\x1b[7m \x1b[0m\n";
    assert!(!styled.contains("[m123]"));
    assert_eq!(visible(CaptureFormat::Ansi, styled), Some(true));
    // A style change and a row break together.
    assert_eq!(
        visible(CaptureFormat::Ansi, "\x1b[1m[m\x1b[0m1\n23]\n"),
        Some(true)
    );
    // A styled capture the decoder refuses is unknown, never "absent".
    assert_eq!(visible(CaptureFormat::Ansi, "\x1b[2J[m123] Read\n"), None);
}

#[test]
fn attempt_session_table() {
    use AgentStatus::{Clearing, Down, Ready, Starting, Stuck};
    let (s1, s2) = (SessionId::new("s1"), SessionId::new("s2"));
    let session = |status, recorded, eligible| attempt_session(status, recorded, eligible);
    // Ready and bound to the pane's one eligible row: confirm against it.
    assert_eq!(
        session(Ready, Some(&s1), Some(&s1)).unwrap(),
        Some(s1.clone())
    );
    // Ready, nothing bound and no eligible row: a lazy first prompt.
    assert_eq!(session(Ready, None, None).unwrap(), None);
    let refused = [
        // The binding's row is gone, or another session holds the pane.
        (Ready, Some(&s1), None),
        (Ready, Some(&s1), Some(&s2)),
        // A row the agent is not bound to is nothing to confirm against.
        (Ready, None, Some(&s1)),
    ];
    for (status, recorded, eligible) in refused {
        assert!(
            matches!(
                session(status, recorded, eligible),
                Err(MessageError::Refused(_))
            ),
            "{status:?} {recorded:?} {eligible:?}"
        );
    }
    // No other status takes a delivery, whatever is bound.
    for status in [Starting, Clearing, Stuck, Down] {
        for (recorded, eligible) in [(Some(&s1), Some(&s1)), (None, None)] {
            assert!(
                matches!(
                    session(status, recorded, eligible),
                    Err(MessageError::Refused(_))
                ),
                "{status:?} {recorded:?}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// The rig
// ---------------------------------------------------------------------------

/// How committed events reach history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fold {
    /// The message service's own fold loop, as in the daemon.
    Service,
    /// Nothing is folded until the test records `Applied` itself.
    Hand,
    /// Evidence and history are disabled.
    Off,
}

/// A real organisation writer whose appends stay in memory and pass a gate,
/// one `Applied` writer (the service's fold loop, or the test by hand), the
/// real control actor over per-pane tmux scripts and, for the attempt tests,
/// the real agent service with a pi or unbound Codex agent on %1.
struct Rig {
    org: OrgHandle,
    rt: MessageRuntime,
    tmux: ScriptedTmuxClient,
    /// The control actor's client: `tmux`, with a paste's or a key's return
    /// held or failed when a test arms it.
    tmux_gate: GatedTmux,
    /// Keeps the service (the fold loop) alive.
    messages: Option<MessageHandle>,
    /// Every committed envelope from the first one on.
    committed: broadcast::Receiver<Arc<Envelope>>,
    /// Zero permits hold every append after it was entered.
    gate: Arc<Semaphore>,
    /// The sequence of each append, sent when it is entered.
    entered: mpsc::UnboundedReceiver<u64>,
    /// Each stops one dependency alone.
    org_stop: CancellationToken,
    registry_stop: CancellationToken,
    registry_tasks: Option<crate::registry::RegistryTasks>,
    cancel: CancellationToken,
}

impl Rig {
    /// `agent` seeds `demo/worker` (pi, Ready, bound to `s1` on %1) and runs
    /// the agent service over it. `scheduler` runs the service's recipient
    /// jobs with these effects; without it a test is the recipient slot.
    async fn start(
        fold: Fold,
        agent: bool,
        policy: DeliveryPolicy,
        scheduler: Option<Effects>,
    ) -> Self {
        Self::start_at(0, fold, agent, policy, scheduler).await
    }

    /// As `start`, on a log whose last sequence is `base`: the first event
    /// is `base + 1`, so a message can have the id a real capture shows.
    /// History is given `base` events of no known kind first, as it folds
    /// every sequence. Only with the test folding: the message service
    /// keeps a projection of its own, which starts at zero.
    async fn start_at(
        base: u64,
        fold: Fold,
        agent: bool,
        policy: DeliveryPolicy,
        scheduler: Option<Effects>,
    ) -> Self {
        Self::start_with_harness(base, fold, agent, policy, scheduler, Harness::Pi).await
    }

    /// The Codex variant starts Ready after a clear: no binding or registry
    /// row, but the old session is excluded from the first-prompt scope.
    async fn start_with_harness(
        base: u64,
        fold: Fold,
        agent: bool,
        policy: DeliveryPolicy,
        scheduler: Option<Effects>,
        harness: Harness,
    ) -> Self {
        let gate = Arc::new(Semaphore::new(1));
        let (enter, entered) = mpsc::unbounded_channel();
        let (requests, receive) = mpsc::channel::<Request>(64);
        let (health, status) = watch::channel(Ok(base));
        let (committed, _) = broadcast::channel(1024);
        let cancel = CancellationToken::new();
        let org_stop = cancel.child_token();
        let next = Arc::new(AtomicU64::new(base));
        let held = gate.clone();
        tokio::spawn(crate::org::actor::run(
            OrgState::default(),
            receive,
            health,
            committed.clone(),
            org_stop.clone(),
            move |_envelope: Envelope| {
                let seq = next.fetch_add(1, Ordering::SeqCst) + 1;
                let held = held.clone();
                let enter = enter.clone();
                async move {
                    let _ = enter.send(seq);
                    let _open = held.acquire().await.unwrap();
                    Ok::<u64, EventLogError>(seq)
                }
            },
        ));
        let org = OrgHandle {
            requests,
            health: status,
            committed: committed.clone(),
            reads: None,
        };
        // Subscribed before the first commit, so both see every event.
        let (events, folding) = (committed.subscribe(), committed.subscribe());
        let registry_stop = cancel.child_token();
        let (registry, registry_tasks) =
            crate::registry::spawn_registry_with_cancel(registry_stop.clone());
        let tmux = ScriptedTmuxClient::new();
        let tmux_gate = GatedTmux::new(tmux.clone());
        let (control, _task) = spawn_control(
            Box::new(tmux_gate.clone()),
            registry.clone(),
            org.clone(),
            cancel.clone(),
        );
        let agents = if agent {
            let codex = harness == Harness::Codex;
            let role = if codex { "codex" } else { "pi" };
            let mut seed = vec![
                OrgCommand::RegisterProject {
                    project_id: "demo".parse().unwrap(),
                    root: "/demo".into(),
                    move_root: false,
                },
                OrgCommand::Up {
                    agent_id: worker(),
                    role: atm_org::parse_role("worker", &format!("+++\nharness='{role}'\n+++\n"))
                        .unwrap(),
                },
                OrgCommand::PaneCreated {
                    agent_id: worker(),
                    pane: pane().to_string(),
                },
                OrgCommand::SessionBound {
                    agent_id: worker(),
                    session_id: SessionId::new(if codex { "old" } else { "s1" }),
                },
            ];
            if codex {
                seed.extend([
                    OrgCommand::Clear { agent_id: worker() },
                    OrgCommand::EditorReady { agent_id: worker() },
                ]);
            }
            for command in seed {
                org.execute(command, utc()).await.unwrap();
            }
            if !codex {
                registry.register(row("s1")).await.unwrap();
            }
            // The agent service's check of the pane when it starts.
            let idle = if codex {
                codex_frame(CODEX_IDLE)
            } else {
                pi(IDLE)
            };
            tmux.script_inspect(&pane(), Some(&idle.inspection))
                .unwrap();
            // Ready unbound agents are inspected but need no restart
            // capture; only a recorded binding is revalidated by a read.
            if !codex {
                tmux.script_capture(&idle).unwrap();
            }
            let (agents, _task) = spawn_agents_with(
                org.clone(),
                registry.clone(),
                control.clone(),
                cancel.child_token(),
                Arc::new(|_| Box::pin(async { None })),
                Arc::new(|_| None),
            );
            guarded(agents.reconciled()).await.unwrap();
            assert_eq!(tmux.pending_reads(&pane()).unwrap(), 0);
            let baseline = if codex { codex_baseline() } else { baseline() };
            agents.record_baseline(worker(), "%1", baseline);
            agents
        } else {
            AgentHandle::disabled("no agent service in this rig")
        };
        let clock = MessageClock::new(Instant::now(), utc());
        let (evidence, history) = if fold == Fold::Off {
            (
                MessageEvidence::disabled(clock, "evidence is off"),
                MessageHistoryHandle::disabled("history is off"),
            )
        } else {
            (
                MessageEvidence::new(clock),
                MessageHistoryHandle::new(0, utc(), policy.clone()).unwrap(),
            )
        };
        let rt = MessageRuntime::new(
            org.clone(),
            registry,
            control,
            agents,
            evidence,
            history,
            policy,
            cancel.child_token(),
        );
        assert!(base == 0 || fold == Fold::Hand);
        for seq in 1..=base {
            let (at, event) = (utc(), None);
            let earlier = HistoryUpdate::Applied { seq, at, event };
            rt.history.record(earlier, at).unwrap();
        }
        let messages = if fold == Fold::Hand {
            None
        } else {
            let (messages, _task) = spawn_service_for_test(rt.clone(), events, 0, scheduler);
            if fold == Fold::Service {
                guarded(messages.initialized()).await.unwrap();
            }
            Some(messages)
        };
        Self {
            org,
            rt,
            tmux,
            tmux_gate,
            messages,
            committed: folding,
            gate,
            entered,
            org_stop,
            registry_stop,
            registry_tasks: Some(registry_tasks),
            cancel,
        }
    }

    /// No agent service: for the tests of the rig's own parts.
    async fn bare(fold: Fold) -> Self {
        Self::start(fold, false, DeliveryPolicy::default(), None).await
    }

    /// The worker, with the service folding.
    async fn attempt() -> Self {
        Self::start(Fold::Service, true, DeliveryPolicy::default(), None).await
    }

    /// The worker, with the test folding.
    async fn hand() -> Self {
        Self::start(Fold::Hand, true, DeliveryPolicy::default(), None).await
    }

    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        self.rt.clock.now().unwrap()
    }

    /// Execute on the writer; the committed sequence, not yet folded.
    async fn commit(&self, command: OrgCommand) -> u64 {
        let committed = guarded(self.org.execute(command, utc())).await.unwrap();
        committed.last().unwrap().seq
    }

    /// Wait until the service has folded `seq` into history.
    async fn folded(&self, seq: u64) {
        guarded(self.rt.history.folded(seq)).await.unwrap();
    }

    /// Record one committed envelope as applied, as the service's loop does.
    fn apply(&self, envelope: &Envelope) {
        let applied = HistoryUpdate::Applied {
            seq: envelope.seq,
            at: envelope.ts,
            event: OrgEvent::from_parts(&envelope.kind, &envelope.body).unwrap(),
        };
        self.rt.history.record(applied, envelope.ts).unwrap();
    }

    /// By hand: fold everything committed so far.
    fn fold(&mut self) {
        while let Ok(envelope) = self.committed.try_recv() {
            self.apply(&envelope);
        }
    }

    /// By hand: from here on, fold every event as it is committed.
    fn fold_rest(&mut self) {
        self.fold();
        let mut events = self.committed.resubscribe();
        let history = self.rt.history.clone();
        tokio::spawn(async move {
            while let Ok(envelope) = events.recv().await {
                let applied = HistoryUpdate::Applied {
                    seq: envelope.seq,
                    at: envelope.ts,
                    event: OrgEvent::from_parts(&envelope.kind, &envelope.body).unwrap(),
                };
                history.record(applied, envelope.ts).unwrap();
            }
        });
    }

    /// The next committed envelope, in order.
    async fn committed(&mut self) -> Arc<Envelope> {
        guarded(self.committed.recv()).await.unwrap()
    }

    /// The next committed event of a message, skipping every other kind.
    async fn message_event(&mut self) -> OrgEvent {
        loop {
            let envelope = self.committed().await;
            if envelope.kind.starts_with("msg.") {
                return OrgEvent::from_parts(&envelope.kind, &envelope.body)
                    .unwrap()
                    .unwrap();
            }
        }
    }

    /// A project with `demo/worker` bound to session `s1` on %1; the last
    /// committed sequence, not yet folded. For a rig without the agent.
    async fn seed(&self) -> u64 {
        self.commit(OrgCommand::RegisterProject {
            project_id: "demo".parse().unwrap(),
            root: "/demo".into(),
            move_root: false,
        })
        .await;
        self.commit(OrgCommand::Adopt {
            agent_id: worker(),
            pane: pane().to_string(),
            session_id: Some("s1".into()),
        })
        .await
    }

    fn message(to: AgentId) -> OrgCommand {
        OrgCommand::SendMessage(MessageContent {
            from: Address::Owner,
            to: Address::Agent(to),
            kind: MessageKind::Note,
            body: "the body never reaches a terminal".into(),
            attachments: vec![],
            task: None,
        })
    }

    /// A message for the worker, folded, and the worker's pane observed just
    /// now as the executor will see it: idle, empty, unfocused.
    async fn queue_message(&mut self) -> MessageId {
        self.queue_observed_message(idle_observation()).await
    }

    async fn queue_observed_message(&mut self, observation: RecipientObservation) -> MessageId {
        let seq = self.commit(Rig::message(worker())).await;
        if self.messages.is_some() {
            self.folded(seq).await;
        } else {
            self.fold();
        }
        let observed = HistoryUpdate::Observed {
            recipient: worker(),
            observation,
        };
        self.rt.history.record(observed, self.now()).unwrap();
        // The recipient's job consumes the wait it reads: the hold clock runs.
        let id = MessageId::new(seq);
        let waiting = HistoryUpdate::Consumed {
            id,
            step: self.step(id),
        };
        self.rt.history.record(waiting, self.now()).unwrap();
        assert!(self.snapshot(id).live.hold_since.is_some());
        id
    }

    /// As `queue_message`, then the stable-idle time passes: the policy's
    /// next step for the message is the first paste.
    async fn ready_message(&mut self) -> MessageId {
        let id = self.queue_message().await;
        tokio::time::advance(Duration::from_secs(10)).await;
        assert_eq!(self.step(id), DeliveryStep::PasteNow { attempt: 1 });
        id
    }

    /// Copy evidence's facts for the message into history, as the
    /// recipient's job does before each of its evaluations.
    fn copy_evidence(&self, id: MessageId) {
        let evidence = self.rt.evidence.snapshot(id).unwrap();
        let copy = HistoryUpdate::Evidence {
            id,
            receipt: evidence.receipt,
            probably_landed: evidence.probably_landed,
        };
        self.rt.history.record(copy, self.now()).unwrap();
    }

    fn snapshot(&self, id: MessageId) -> DeliverySnapshot {
        self.rt.history.snapshot(id, self.now()).unwrap()
    }

    /// The policy's answer for the message now, on history as it is.
    fn step(&self, id: MessageId) -> DeliveryStep {
        let snapshot = self.snapshot(id);
        next_delivery(
            &snapshot.message,
            &snapshot.live,
            self.now(),
            &self.rt.policy,
        )
        .unwrap()
    }

    /// One capture, or one guarded input's capture, per frame, in order.
    fn script(&self, frames: &[PaneCapture]) {
        for frame in frames {
            self.tmux.script_capture(frame).unwrap();
        }
    }

    /// Scripted reads of %1 nobody has consumed: three per frame.
    fn reads(&self) -> usize {
        self.tmux.pending_reads(&pane()).unwrap()
    }

    /// Run the executor once for the message, as the recipient's slot.
    fn spawn_deliver(&self, id: MessageId) -> JoinHandle<Result<(), MessageError>> {
        let deadline = Instant::now() + ATTEMPT_WORK_BUDGET;
        tokio::spawn(deliver(self.rt.clone(), id, deadline))
    }

    async fn deliver(&self, id: MessageId) -> Result<(), MessageError> {
        guarded(self.spawn_deliver(id)).await.unwrap()
    }

    /// Hold the worker and its pane under another reservation until the
    /// returned sender is used: an attempt started meanwhile parks.
    async fn park(&self) -> oneshot::Sender<()> {
        let (holding, held) = oneshot::channel();
        let (release, released) = oneshot::channel::<()>();
        let request = ReservationRequest::new(
            worker(),
            Some(pane()),
            ReservationKind::Observe,
            WaitPolicy::Wait,
            Instant::now() + ATTEMPT_WORK_BUDGET,
        );
        let control = self.rt.control.clone();
        tokio::spawn(async move {
            let work = move |_: Reservation| async move {
                let _ = holding.send(());
                let _ = released.await;
                Ok::<_, ReservationError>(())
            };
            control.run_reserved(request, work, no_cleanup).await
        });
        guarded(held).await.unwrap();
        release
    }

    /// The executor is past its first checks: it registers the reference
    /// right before it asks for its reservation.
    async fn admission_requested(&self, id: MessageId) {
        until("the executor registered the reference", || {
            self.rt.evidence.snapshot(id).is_ok()
        })
        .await;
    }

    /// Evidence's change counter: an arm moves it.
    fn wakes(&self) -> u64 {
        *self.rt.evidence.subscribe().borrow()
    }

    /// Hold every later append once it is entered; `entered` then yields the
    /// held append's sequence, not an earlier one's.
    fn hold(&mut self) {
        self.gate.forget_permits(usize::MAX);
        while self.entered.try_recv().is_ok() {}
    }

    fn release(&self) {
        self.gate.add_permits(1);
    }

    /// The sequence of the next append the writer enters.
    async fn entered(&mut self) -> u64 {
        guarded(self.entered.recv()).await.unwrap()
    }

    /// What tmux was asked to change: everything but the scripted reads.
    fn effects(&self) -> Vec<MockCall> {
        (self.tmux.mock().calls().into_iter())
            .filter(|call| !matches!(call, MockCall::RawCommand { .. }))
            .collect()
    }

    fn pastes(&self) -> usize {
        (self.effects().iter())
            .filter(|call| matches!(call, MockCall::PasteText { .. }))
            .count()
    }

    fn keys(&self) -> usize {
        (self.effects().iter())
            .filter(|call| matches!(call, MockCall::SendKeys { .. }))
            .count()
    }

    /// The paste and the Enter of one whole attempt, and nothing else.
    fn typed(&self, id: MessageId) -> bool {
        self.effects()
            == [
                MockCall::PasteText {
                    pane: "%1".into(),
                    text: inbox_reference(id),
                },
                MockCall::SendKeys {
                    pane: "%1".into(),
                    keys: "Enter".into(),
                },
            ]
    }

    /// The next audit of an input: the executor's Enter call has returned.
    /// Waiting for it lets the paused clock run on to that Enter, through
    /// the control actor's capture pacing, and no further.
    async fn audited(&mut self) -> Arc<Envelope> {
        loop {
            let envelope = self.committed().await;
            if envelope.kind == "control.sent" {
                return envelope;
            }
        }
    }

    /// Let the paused clock run, a guard poll at a time, until the reference
    /// is pasted. The executor is then parked on a timer before its next
    /// capture: what the test does next comes before that capture.
    async fn pasted(&self) {
        for _ in 0..100 {
            for _ in 0..100 {
                if self.pastes() == 1 {
                    // tmux has the paste; the executor has yet to hear of
                    // it. With the clock standing still it runs on to the
                    // pacing timer of its first look and no further.
                    for _ in 0..100 {
                        tokio::task::yield_now().await;
                    }
                    return;
                }
                tokio::task::yield_now().await;
            }
            tokio::time::advance(self.rt.policy.settings().guard_poll_interval).await;
        }
        panic!("the reference was never pasted");
    }

    /// The worker's session reports a prompt it was given.
    fn prompt(&self, session: &str, text: String) {
        let prompt = LifecycleEvent::PromptSubmit { prompt: Some(text) };
        (self.rt.evidence)
            .observe(&SessionId::new(session), Some("%1"), Harness::Pi, &prompt)
            .unwrap();
    }

    fn reservation(&self) -> ReservationRequest {
        ReservationRequest::new(
            worker(),
            Some(pane()),
            ReservationKind::Delivery,
            WaitPolicy::Wait,
            Instant::now() + ATTEMPT_WORK_BUDGET,
        )
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// A real idle pi editor, `-e -N` with tmux's padding. The adapter's fixture
/// README has its `Captured:` line, which `body` removes.
const IDLE: &str = include_str!(
    "../../../atm-pi-adapter/tests/fixtures/screen_styled_p1_A-ready-120x40_1.5.0.txt"
);

/// What is recorded about %1: its root process, and pi as its harness.
fn baseline() -> PaneBaseline {
    PaneBaseline::adopted(100)
        .with_harness("pi".into())
        .unwrap()
}

/// %1 as tmux reports it with pi in the foreground: a styled capture of an
/// adapter fixture, tagged for the worker, no client focused on the pane.
fn pi(fixture: &str) -> PaneCapture {
    let text = body(fixture);
    let inspection = PaneInspection::new(
        PaneInfo {
            pane_id: pane().to_string(),
            session_name: "private".into(),
            window_index: 0,
            pane_pid: 100,
            width: 120,
            height: text.lines().count() as u16,
            is_active: true,
        },
        Some(worker().to_string()),
        false,
        false,
        Some(true),
        0,
        0,
    )
    .with_foreground_process(None, Some("pi".into()));
    PaneCapture::new(text.into(), inspection, CaptureFormat::Ansi).with_focus(Some(false))
}

fn with(mut capture: PaneCapture, change: impl FnOnce(&mut PaneCapture)) -> PaneCapture {
    change(&mut capture);
    capture
}

/// The owner's client is on the pane: every delivery guard refuses.
fn focused() -> PaneCapture {
    with(pi(IDLE), |capture| capture.focused = Some(true))
}

/// Synthetic: the idle frame with its first row replaced by `row`, for the
/// neutral token check, which reads no editor.
fn showing(row: &str) -> PaneCapture {
    with(pi(IDLE), |capture| {
        let rest = capture.text.split_once('\n').unwrap().1;
        capture.text = format!("{row}\n{rest}");
    })
}

/// A real pi editor holding a draft that is not the reference: `/new`.
const DRAFT: &str =
    include_str!("../../../atm-pi-adapter/tests/fixtures/screen_p5_120x40-staged_1.5.0.styled.txt");

/// Synthetic: that real frame with its draft replaced by `draft`, for the
/// order of the executor's steps. What each harness really shows with the
/// reference staged is the adapters' own fixtures' to say.
fn drafted(draft: &str) -> PaneCapture {
    with(pi(DRAFT), |capture| {
        assert_eq!(capture.text.matches("/new").count(), 1);
        capture.text = capture.text.replace("/new", draft);
    })
}

/// The reference of `id` as the editor's draft, which the real pi matcher
/// and the real dispatch read as staged, with nothing else in the way.
fn staged(id: MessageId) -> PaneCapture {
    let (capture, reference) = (drafted(&inbox_reference(id)), inbox_reference(id));
    let screen = agent_screen(Harness::Pi).unwrap();
    assert_eq!(screen.staged(&capture, &baseline(), &reference), Some(true));
    let facts = screen.facts(&capture, &baseline());
    assert_eq!(
        (facts.input, facts.turn, facts.dialog),
        (InputState::Draft, TurnState::NotRunning, DialogState::None)
    );
    capture
}

/// Real Codex 0.167.0 fresh editor supplying the startup inspection's
/// metadata; a Ready unbound agent needs no restart capture. Provenance is
/// recorded with the adapter's fixture. No harness process is started.
const CODEX_IDLE: &str = include_str!(
    "../../../atm-codex-adapter/tests/fixtures/screen_styled_u5b_rich-fresh_0.167.0.txt"
);
const CODEX_EMPTY: &str = include_str!(
    "../../../atm-codex-adapter/tests/fixtures/screen_p3_120x40-r1-C-u-empty_0.167.0.styled.txt"
);
const CODEX_STAGED: &str = include_str!(
    "../../../atm-codex-adapter/tests/fixtures/screen_p3_120x40-r1-C-u-staged_0.167.0.styled.txt"
);

fn codex_baseline() -> PaneBaseline {
    PaneBaseline::adopted(100)
        .with_harness("codex".into())
        .unwrap()
}

fn codex_frame(fixture: &str) -> PaneCapture {
    with(pi(fixture), |capture| {
        capture.inspection.foreground_command = Some("codex".into());
    })
}

async fn ready_unbound_codex() -> (Rig, MessageId) {
    // Six seed events, then the message whose real staged frame is m123456.
    let mut rig = Rig::start_with_harness(
        123_449,
        Fold::Hand,
        true,
        DeliveryPolicy::default(),
        None,
        Harness::Codex,
    )
    .await;
    let recipient = rig.rt.agents.recipient(&worker()).await.unwrap();
    assert_eq!(recipient.status, AgentStatus::Ready);
    assert_eq!(recipient.harness, Harness::Codex);
    assert!(recipient.session_id.is_none() && recipient.session.is_none());
    assert_eq!(recipient.excluded, Some(SessionId::new("old")));
    let observation = observation(&recipient, &codex_frame(CODEX_EMPTY)).unwrap();
    let id = rig.queue_observed_message(observation).await;
    assert_eq!(id, MessageId::new(123_456));
    tokio::time::advance(Duration::from_secs(10)).await;
    assert_eq!(rig.step(id), DeliveryStep::PasteNow { attempt: 1 });
    (rig, id)
}

/// The worker's pane as the executor converts the idle frame, for history.
fn idle_observation() -> RecipientObservation {
    let capture = pi(IDLE);
    let facts = (agent_screen(Harness::Pi).unwrap()).facts(&capture, &baseline());
    assert_eq!(
        (facts.input, facts.turn, facts.dialog.clone()),
        (InputState::Empty, TurnState::NotRunning, DialogState::None)
    );
    let session = SessionObservation {
        id: SessionId::new("s1"),
        harness: Harness::Pi,
        status: SessionStatus::Idle,
        last_event_at: None,
    };
    recipient_observation(&capture, Some(session), facts).unwrap()
}

/// The scope the executor arms for the worker's attempt at `attempt_seq`.
fn scope(id: MessageId, attempt_seq: u64) -> AttemptScope {
    AttemptScope {
        message_id: id,
        attempt_seq,
        recipient: worker(),
        pane: pane(),
        harness: Harness::Pi,
        session_id: Some(SessionId::new("s1")),
        excluded_sessions: HashSet::new(),
    }
}

/// An eight-row viewport of %1 with `tag` as its pane-local agent tag.
fn frame(tag: &str) -> PaneCapture {
    let inspection = PaneInspection::new(
        PaneInfo {
            pane_id: pane().to_string(),
            session_name: "private".into(),
            window_index: 0,
            pane_pid: 100,
            width: 40,
            height: 8,
            is_active: true,
        },
        Some(tag.into()),
        false,
        false,
        Some(true),
        0,
        0,
    );
    PaneCapture::new("\n".repeat(8), inspection, CaptureFormat::Ansi).with_focus(Some(false))
}

async fn no_cleanup(_: Reservation, _: Completion) -> Result<(), ReservationError> {
    Ok(())
}

/// The real actor's refusals of one guarded call, read through `certainty`:
/// a refusing predicate and a foreign tag send nothing and are NotStarted.
/// The same calls on an allowing frame type exactly once each, so the zero
/// counts above are not an idle actor.
#[tokio::test(start_paused = true)]
async fn refused_guarded_input_sends_nothing_and_is_not_started() {
    let rig = Rig::bare(Fold::Service).await;
    let tmux = rig.tmux.clone();
    let reference = inbox_reference(MessageId::new(123));
    let text = reference.clone();
    let refused = guarded(rig.rt.control.run_reserved(
        rig.reservation(),
        move |r| async move {
            let agent = r.agent().clone();
            let mut refused = Vec::new();
            for (tag, allow) in [("demo/worker", false), ("demo/other", true)] {
                tmux.script_capture(&frame(tag)).unwrap();
                let paste = r
                    .paste_guarded(
                        &pane(),
                        agent.clone(),
                        text.clone(),
                        CaptureFormat::Ansi,
                        move |_| allow,
                    )
                    .await;
                tmux.script_capture(&frame(tag)).unwrap();
                let enter = r
                    .keys_guarded(
                        &pane(),
                        agent.clone(),
                        PaneKey::Enter,
                        CaptureFormat::Ansi,
                        move |_| allow,
                    )
                    .await;
                refused.extend([paste.unwrap_err(), enter.unwrap_err()]);
            }
            let sent = tmux.mock().calls().len();
            tmux.script_capture(&frame("demo/worker")).unwrap();
            r.paste_guarded(&pane(), agent.clone(), text, CaptureFormat::Ansi, |_| true)
                .await?;
            tmux.script_capture(&frame("demo/worker")).unwrap();
            r.keys_guarded(&pane(), agent, PaneKey::Enter, CaptureFormat::Ansi, |_| {
                true
            })
            .await?;
            Ok::<_, ReservationError>((refused, sent))
        },
        no_cleanup,
    ))
    .await
    .unwrap();

    let (refused, _) = &refused;
    assert!(
        matches!(
            refused.as_slice(),
            [
                ReservationError::ScreenChanged { .. },
                ReservationError::ScreenChanged { .. },
                ReservationError::TagMismatch { .. },
                ReservationError::TagMismatch { .. },
            ]
        ),
        "{refused:?}"
    );
    for error in refused {
        assert_eq!(certainty(error), NOT_STARTED, "{error:?}");
    }
    // Only the two allowed calls reached the pane: one paste, then one Enter.
    assert_eq!(
        rig.effects(),
        [
            MockCall::PasteText {
                pane: "%1".into(),
                text: reference,
            },
            MockCall::SendKeys {
                pane: "%1".into(),
                keys: "Enter".into(),
            },
        ]
    );
    assert_eq!(rig.tmux.pending_reads(&pane()).unwrap(), 0);
}

/// With evidence and history disabled the executor has nothing to arm, so it
/// must leave the organisation log and the pane alone, whatever it answers.
#[tokio::test(start_paused = true)]
async fn deliver_without_evidence_and_history_has_no_effect() {
    let mut rig = Rig::bare(Fold::Off).await;
    rig.seed().await;
    let seq = rig.commit(Rig::message(worker())).await;
    let before = rig.org.status().unwrap();
    assert_eq!(before, seq);
    while rig.entered.try_recv().is_ok() {}

    let delivered = guarded(deliver(
        rig.rt.clone(),
        MessageId::new(seq),
        Instant::now() + ATTEMPT_WORK_BUDGET,
    ))
    .await;

    assert!(
        matches!(delivered, Err(MessageError::Unavailable(_))),
        "{delivered:?}"
    );
    // A snapshot is answered after every command accepted before it.
    guarded(rig.org.snapshot()).await.unwrap();
    assert_eq!(rig.org.status().unwrap(), before);
    assert!(rig.entered.try_recv().is_err(), "an append was entered");
    assert!(rig.tmux.mock().calls().is_empty());
}

/// The gate the attempt tests stand on: a held append is entered and not
/// committed, and releasing it commits and folds exactly that one event.
#[tokio::test(start_paused = true)]
async fn held_append_commits_exactly_once_when_released() {
    let mut rig = Rig::bare(Fold::Service).await;
    let before = rig.seed().await;
    rig.folded(before).await;
    rig.hold();

    let org = rig.org.clone();
    let write = tokio::spawn(async move { org.execute(Rig::message(worker()), utc()).await });
    assert_eq!(rig.entered().await, before + 1);
    // Entered, and held: nothing is committed, so nothing can be folded.
    assert_eq!(rig.org.status().unwrap(), before);
    assert!(!write.is_finished());

    rig.release();
    let committed = guarded(write).await.unwrap().unwrap();
    assert_eq!(
        committed.iter().map(|e| e.seq).collect::<Vec<_>>(),
        [before + 1]
    );
    rig.folded(before + 1).await;
    assert_eq!(rig.org.status().unwrap(), before + 1);
    assert!(
        rig.entered.try_recv().is_err(),
        "a second append was entered"
    );
}

// ---------------------------------------------------------------------------
// One attempt, up to its single guarded paste
// ---------------------------------------------------------------------------
//
// Each test scripts the frames the executor may read, in order: its first
// capture, the paste guard's, and cleanup's token capture after a paste that
// may have typed. Unconsumed reads are counted, three per frame: they show
// which of those calls were made at all.

/// The attempt's event is appended and folded before anything is armed or
/// typed. Both gates are held in turn; released, exactly one paste follows.
#[tokio::test(start_paused = true)]
async fn attempt_append_precedes_paste() {
    let mut rig = Rig::hand().await;
    let id = rig.ready_message().await;
    let reference = inbox_reference(id);
    // The first capture, the paste guard's; then tmux fails the first look
    // at the editor, which ends the attempt, and cleanup's token capture.
    rig.script(&[pi(IDLE), pi(IDLE)]);
    let (before, armed) = (rig.org.status().unwrap(), rig.wakes());
    rig.hold();
    let attempt = rig.spawn_deliver(id);

    // The append is entered and held: nothing is committed, armed or typed.
    assert_eq!(rig.entered().await, before + 1);
    assert_eq!(rig.org.status().unwrap(), before);
    assert_eq!((rig.pastes(), rig.reads(), rig.wakes()), (0, 3, armed));

    // Committed, and not folded: still nothing.
    rig.release();
    let envelope = rig.committed().await;
    assert_eq!(
        (envelope.seq, envelope.kind.as_str()),
        (before + 1, "msg.delivery_attempted")
    );
    assert_eq!((rig.pastes(), rig.reads(), rig.wakes()), (0, 3, armed));

    // Folded: armed, and exactly one paste, of the reference alone.
    rig.apply(&envelope);
    let delivered = guarded(attempt).await.unwrap();
    assert!(
        matches!(
            delivered,
            Err(MessageError::Reservation(ReservationError::Control(_)))
        ),
        "{delivered:?}"
    );
    assert_eq!(
        rig.effects(),
        [MockCall::PasteText {
            pane: "%1".into(),
            text: reference,
        }]
    );
    assert_ne!(rig.wakes(), armed);
    assert_eq!(rig.reads(), 0);
    // The paste was applied and the editor could not be read after it:
    // held, and nothing written after the attempt.
    let snapshot = rig.snapshot(id);
    assert_eq!(snapshot.last_attempt_seq, Some(before + 1));
    assert_eq!(snapshot.hold, Some(ExecutorHold::InputOutcomeUnknown));
    // The executor consumed its PasteNow: the wait's hold clock is reset.
    assert_eq!(snapshot.live.hold_since, None);
    guarded(rig.org.snapshot()).await.unwrap();
    assert_eq!(rig.org.status().unwrap(), before + 1);
}

/// The log already holds this attempt and history has not folded it: the
/// policy still answers `PasteNow { 1 }`, the writer has nothing new to
/// commit, and an attempt without its own new event types nothing.
#[tokio::test(start_paused = true)]
async fn duplicate_noevent_attempt_never_pastes() {
    let mut rig = Rig::hand().await;
    let id = rig.ready_message().await;
    rig.script(&[pi(IDLE), pi(IDLE)]);
    let logged = rig
        .commit(OrgCommand::DeliveryAttempted {
            message_id: id,
            attempt: 1,
            pane: "%1".into(),
        })
        .await;
    let armed = rig.wakes();

    let delivered = rig.deliver(id).await;

    assert!(
        matches!(delivered, Err(MessageError::Conflict(_))),
        "{delivered:?}"
    );
    assert_eq!((rig.pastes(), rig.reads(), rig.wakes()), (0, 3, armed));
    guarded(rig.org.snapshot()).await.unwrap();
    assert_eq!(rig.org.status().unwrap(), logged);
    // The old attempt is not this run's to settle: no hold, no failure.
    rig.fold();
    let snapshot = rig.snapshot(id);
    assert_eq!(snapshot.message.status, DeliveryStatus::Delivering);
    assert_eq!(snapshot.hold, None);
}

/// Evidence refuses the arm (it already holds a later scope of the message):
/// history is not armed after it and nothing is typed. With the next test
/// this pins the order: evidence, then history, then input.
#[tokio::test(start_paused = true)]
async fn evidence_arm_precedes_history_arm_and_input() {
    let mut rig = Rig::attempt().await;
    let id = rig.ready_message().await;
    rig.script(&[pi(IDLE), pi(IDLE)]);
    rig.rt.evidence.register(id, inbox_reference(id)).unwrap();
    rig.rt.evidence.arm(scope(id, u64::MAX)).unwrap();
    let before = rig.org.status().unwrap();

    let delivered = rig.deliver(id).await;

    assert!(
        matches!(delivered, Err(MessageError::Refused(_))),
        "{delivered:?}"
    );
    assert_eq!((rig.pastes(), rig.reads()), (0, 3));
    // The attempt was committed, typed nothing and is failed for a retry.
    let snapshot = rig.snapshot(id);
    assert_eq!(snapshot.last_attempt_seq, Some(before + 1));
    assert_eq!(snapshot.message.status, DeliveryStatus::Queued);
    assert_eq!(snapshot.hold, None);
    assert!(
        matches!(rig.step(id), DeliveryStep::RetryAt { .. }),
        "{:?}",
        rig.step(id)
    );
    // History holds no scope for the attempt: it takes a first arm that
    // differs from the executor's. It would refuse a second, different one.
    let other = AttemptScope {
        session_id: None,
        ..scope(id, before + 1)
    };
    let armed = HistoryUpdate::Armed { scope: other };
    rig.rt.history.record(armed, rig.now()).unwrap();
}

/// History refuses the arm after evidence took it (the attempt is already
/// armed there with another scope): evidence holds the executor's scope,
/// and nothing is typed.
#[tokio::test(start_paused = true)]
async fn history_arm_failure_has_zero_input() {
    let mut rig = Rig::hand().await;
    let id = rig.ready_message().await;
    rig.script(&[pi(IDLE), pi(IDLE)]);
    let armed = rig.wakes();
    let attempt = rig.spawn_deliver(id);
    // Fold the attempt and arm it with another scope in one stretch, before
    // the executor runs again.
    let envelope = rig.committed().await;
    rig.apply(&envelope);
    let other = AttemptScope {
        session_id: None,
        ..scope(id, envelope.seq)
    };
    let foreign = HistoryUpdate::Armed {
        scope: other.clone(),
    };
    rig.rt.history.record(foreign, rig.now()).unwrap();
    rig.fold_rest();

    let delivered = guarded(attempt).await.unwrap();

    assert!(
        matches!(delivered, Err(MessageError::Conflict(_))),
        "{delivered:?}"
    );
    assert_eq!((rig.pastes(), rig.reads()), (0, 3));
    // Evidence was armed first, with exactly the executor's scope: it takes
    // that scope again and refuses another one for the same attempt.
    assert_ne!(rig.wakes(), armed);
    rig.rt.evidence.arm(scope(id, envelope.seq)).unwrap();
    assert!(matches!(
        rig.rt.evidence.arm(other),
        Err(MessageError::Refused(_))
    ));
    // Nothing was typed, so the attempt is failed for a retry.
    assert_eq!(rig.snapshot(id).message.status, DeliveryStatus::Queued);
}

/// A Ready Codex editor after clear has no session yet. Its first attempt
/// must arm both stores lazily and keep the excluded pre-clear session;
/// only a new session's exact prompt may confirm the one pasted reference.
#[tokio::test(start_paused = true)]
async fn unbound_codex_attempt_keeps_the_excluded_session_in_both_scopes() {
    let (mut rig, id) = ready_unbound_codex().await;
    rig.script(&[
        codex_frame(CODEX_EMPTY),
        codex_frame(CODEX_EMPTY),
        codex_frame(CODEX_STAGED),
        codex_frame(CODEX_STAGED),
    ]);
    let wakes = rig.wakes();
    let attempt = rig.spawn_deliver(id);
    let envelope = rig.committed().await;
    assert_eq!(envelope.kind, "msg.delivery_attempted");
    rig.apply(&envelope);
    rig.fold_rest();
    until("the lazy scope was armed", || rig.wakes() != wakes).await;
    let scope = AttemptScope {
        harness: Harness::Codex,
        session_id: None,
        excluded_sessions: HashSet::from([SessionId::new("old")]),
        ..scope(id, envelope.seq)
    };
    // Same scope is idempotent; missing the exclusion is a different scope.
    rig.rt.evidence.arm(scope.clone()).unwrap();
    rig.rt
        .history
        .record(
            HistoryUpdate::Armed {
                scope: scope.clone(),
            },
            rig.now(),
        )
        .unwrap();
    let other = AttemptScope {
        excluded_sessions: HashSet::new(),
        ..scope
    };
    assert!(matches!(
        rig.rt.evidence.arm(other.clone()),
        Err(MessageError::Refused(_))
    ));
    assert!(matches!(
        rig.rt
            .history
            .record(HistoryUpdate::Armed { scope: other }, rig.now()),
        Err(MessageError::Conflict(_))
    ));
    rig.audited().await;
    assert!(rig.typed(id), "{:?}", rig.effects());
    let prompt = LifecycleEvent::PromptSubmit {
        prompt: Some(inbox_reference(id)),
    };
    rig.rt
        .evidence
        .observe(&SessionId::new("old"), Some("%1"), Harness::Codex, &prompt)
        .unwrap();
    assert!(rig.rt.evidence.receipt(id).unwrap().is_none());
    rig.rt
        .evidence
        .observe(&SessionId::new("new"), Some("%1"), Harness::Codex, &prompt)
        .unwrap();

    guarded(attempt).await.unwrap().unwrap();

    assert!(rig.typed(id), "{:?}", rig.effects());
    assert_eq!(rig.reads(), 0);
    assert_eq!(rig.snapshot(id).message.status, DeliveryStatus::Delivered);
    assert_eq!(rig.snapshot(id).hold, None);
}

/// Lifecycle moves the agent to another pane while the attempt waits for
/// its reservation: no attempt is made against the pane it asked for.
#[tokio::test(start_paused = true)]
async fn pane_move_while_admission_parked_reschedules() {
    let mut rig = Rig::hand().await;
    let id = rig.ready_message().await;
    let parked = rig.park().await;
    let attempt = rig.spawn_deliver(id);
    rig.admission_requested(id).await;
    rig.commit(OrgCommand::Respawn {
        agent_id: worker(),
        role: atm_org::parse_role("worker", "+++\nharness='pi'\n+++\n").unwrap(),
        reason: "moved".into(),
    })
    .await;
    rig.commit(OrgCommand::PaneCreated {
        agent_id: worker(),
        pane: "%2".into(),
    })
    .await;
    parked.send(()).unwrap();

    let delivered = guarded(attempt).await.unwrap();

    assert!(
        matches!(delivered, Err(MessageError::Conflict(_))),
        "{delivered:?}"
    );
    // The executor read nothing of %1 and typed nothing; the message has no
    // attempt, so the policy decides again on the new pane.
    assert_eq!(rig.pastes(), 0);
    rig.fold();
    let snapshot = rig.snapshot(id);
    assert_eq!(snapshot.message.status, DeliveryStatus::Queued);
    assert_eq!(snapshot.message.last_attempt, None);
    assert_eq!(snapshot.recipient.pane.as_deref(), Some("%2"));
}

/// Under the reservation the policy answers a wait (the editor has not been
/// empty for long enough): the executor returns with nothing attempted and
/// nothing written.
#[tokio::test(start_paused = true)]
async fn short_poll_yields_no_manufactured_failure() {
    let mut rig = Rig::attempt().await;
    let id = rig.queue_message().await;
    assert!(
        matches!(rig.step(id), DeliveryStep::Wait { .. }),
        "{:?}",
        rig.step(id)
    );
    rig.script(&[pi(IDLE), pi(IDLE)]);
    let before = rig.org.status().unwrap();

    rig.deliver(id).await.unwrap();

    guarded(rig.org.snapshot()).await.unwrap();
    assert_eq!(rig.org.status().unwrap(), before);
    assert_eq!((rig.pastes(), rig.reads()), (0, 3));
    let snapshot = rig.snapshot(id);
    assert_eq!(snapshot.message.status, DeliveryStatus::Queued);
    assert_eq!((snapshot.message.last_attempt, snapshot.hold), (None, None));
}

/// The organisation writer is gone: the recipient cannot be read, so nothing
/// is captured or typed, and the agent is not quarantined for it.
#[tokio::test(start_paused = true)]
async fn unavailable_org_causes_no_paste() {
    let mut rig = Rig::attempt().await;
    let id = rig.ready_message().await;
    rig.script(&[pi(IDLE), pi(IDLE)]);
    rig.org_stop.cancel();
    until("the organisation writer stopped", || {
        rig.org.requests.is_closed()
    })
    .await;

    let delivered = rig.deliver(id).await;

    assert!(
        matches!(delivered, Err(MessageError::Organisation(_))),
        "{delivered:?}"
    );
    assert!(rig.effects().is_empty());
    assert_eq!(rig.reads(), 6);
    // A later reservation of the same agent and pane still acquires.
    let acquired = rig.rt.control.run_reserved(
        rig.reservation(),
        |_| async { Ok::<_, ReservationError>(()) },
        no_cleanup,
    );
    guarded(acquired).await.unwrap();
}

/// The session registry cannot answer. That is never "no session": no
/// attempt is committed, nothing is armed (so no lazy scope either), nothing
/// is typed.
#[tokio::test(start_paused = true)]
async fn registry_unavailable_never_arms() {
    let mut rig = Rig::attempt().await;
    let id = rig.ready_message().await;
    rig.script(&[pi(IDLE), pi(IDLE)]);
    let (before, armed) = (rig.org.status().unwrap(), rig.wakes());
    rig.registry_stop.cancel();
    guarded(rig.registry_tasks.take().unwrap().actor)
        .await
        .unwrap();

    let delivered = rig.deliver(id).await;

    assert!(
        matches!(delivered, Err(MessageError::Unavailable(_))),
        "{delivered:?}"
    );
    guarded(rig.org.snapshot()).await.unwrap();
    assert_eq!(rig.org.status().unwrap(), before);
    assert_eq!((rig.pastes(), rig.reads(), rig.wakes()), (0, 6, armed));
}

/// Attempt 1 was armed and typed nothing. While attempt 2's commit is on its
/// way, the session's prompt with the exact reference arrives for attempt
/// 1's scope: the receipt wins, attempt 2 pastes nothing, and the message is
/// delivered.
#[tokio::test(start_paused = true)]
async fn late_receipt_after_commit_skips_paste() {
    let mut rig = Rig::attempt().await;
    let id = rig.ready_message().await;
    // The owner's client is on the pane when the paste is guarded.
    rig.script(&[pi(IDLE), focused()]);
    rig.deliver(id).await.unwrap();
    assert_eq!((rig.pastes(), rig.reads()), (0, 0));
    // A non-final attempt that typed nothing is failed by the executor.
    let snapshot = rig.snapshot(id);
    assert_eq!(snapshot.message.status, DeliveryStatus::Queued);
    assert_eq!(snapshot.hold, None);
    tokio::time::advance(Duration::from_secs(1)).await;
    assert_eq!(rig.step(id), DeliveryStep::PasteNow { attempt: 2 });

    rig.script(&[pi(IDLE), pi(IDLE)]);
    rig.hold();
    let attempt = rig.spawn_deliver(id);
    rig.entered().await;
    let prompt = LifecycleEvent::PromptSubmit {
        prompt: Some(inbox_reference(id)),
    };
    (rig.rt.evidence)
        .observe(&SessionId::new("s1"), Some("%1"), Harness::Pi, &prompt)
        .unwrap();
    assert!(rig.rt.evidence.receipt(id).unwrap().is_some());
    rig.release();
    guarded(attempt).await.unwrap().unwrap();

    // The paste guard's frame was never read.
    assert_eq!((rig.pastes(), rig.reads()), (0, 3));
    assert_eq!(rig.snapshot(id).message.status, DeliveryStatus::Delivered);
    // Acknowledged only after Delivered was folded: evidence forgot it.
    assert!(rig.rt.evidence.snapshot(id).is_err());
}

/// Shutdown begins while the attempt waits for its reservation: it stops at
/// the check before its commit, with no attempt in the log.
#[tokio::test(start_paused = true)]
async fn cancel_before_attempt_commit_has_no_attempt() {
    let mut rig = Rig::hand().await;
    let id = rig.ready_message().await;
    rig.script(&[pi(IDLE), pi(IDLE)]);
    let before = rig.org.status().unwrap();
    let parked = rig.park().await;
    let attempt = rig.spawn_deliver(id);
    rig.admission_requested(id).await;
    rig.rt.cancel.cancel();
    parked.send(()).unwrap();

    let delivered = guarded(attempt).await.unwrap();

    assert!(
        matches!(delivered, Err(MessageError::Unavailable(_))),
        "{delivered:?}"
    );
    guarded(rig.org.snapshot()).await.unwrap();
    assert_eq!(rig.org.status().unwrap(), before);
    // It got as far as its first capture, and no further.
    assert_eq!((rig.pastes(), rig.reads()), (0, 3));
    assert_eq!(rig.snapshot(id).message.last_attempt, None);
}

/// The first capture shows another agent's tag on the pane: the observation
/// is lost, and there is no attempt.
#[tokio::test(start_paused = true)]
async fn foreign_tag_before_the_attempt_is_an_observation_loss() {
    let mut rig = Rig::attempt().await;
    let id = rig.ready_message().await;
    let foreign = with(pi(IDLE), |capture| {
        capture.inspection.local_agent_tag = Some("demo/other".into());
    });
    rig.script(&[foreign, pi(IDLE)]);
    let before = rig.org.status().unwrap();

    let delivered = rig.deliver(id).await;

    assert!(
        matches!(delivered, Err(MessageError::Conflict(_))),
        "{delivered:?}"
    );
    guarded(rig.org.snapshot()).await.unwrap();
    assert_eq!(rig.org.status().unwrap(), before);
    assert_eq!((rig.pastes(), rig.reads()), (0, 3));
    // What history knew about the pane is gone: the policy waits again.
    assert!(
        matches!(rig.step(id), DeliveryStep::Wait { .. }),
        "{:?}",
        rig.step(id)
    );
}

/// The first capture already refuses the paste for a reason the policy does
/// not see: no attempt is spent on it.
#[tokio::test(start_paused = true)]
async fn refused_paste_guard_before_the_commit_costs_no_attempt() {
    type Change = fn(&mut PaneCapture);
    let cases: [(&str, Change); 3] = [
        ("bracketed paste is off", |capture| {
            capture.inspection.bracketed_paste = Some(false);
        }),
        ("the window's panes are synchronized", |capture| {
            capture.inspection.synchronized = true;
        }),
        ("the pane's input is off", |capture| {
            capture.inspection.input_off = true;
        }),
    ];
    for (case, change) in cases {
        let mut rig = Rig::attempt().await;
        let id = rig.ready_message().await;
        rig.script(&[with(pi(IDLE), change), pi(IDLE)]);
        let before = rig.org.status().unwrap();

        let delivered = rig.deliver(id).await;

        assert!(
            matches!(delivered, Err(MessageError::Refused(_))),
            "{case}: {delivered:?}"
        );
        guarded(rig.org.snapshot()).await.unwrap();
        assert_eq!(rig.org.status().unwrap(), before, "{case}");
        assert_eq!((rig.pastes(), rig.reads()), (0, 3), "{case}");
        assert_eq!(rig.snapshot(id).message.last_attempt, None, "{case}");
    }
}

/// The attempt is committed and its fold never comes. Nothing was typed, but
/// nothing can be recorded or held under an attempt history does not have:
/// cleanup fails, and the control actor keeps the agent out of reach.
#[tokio::test(start_paused = true)]
async fn unfolded_attempt_fails_cleanup_and_quarantines() {
    let mut rig = Rig::hand().await;
    let id = rig.ready_message().await;
    rig.script(&[pi(IDLE), pi(IDLE)]);
    let attempt = rig.spawn_deliver(id);
    let envelope = rig.committed().await;
    assert_eq!(envelope.kind, "msg.delivery_attempted");

    // Work waits for the fold until its deadline, cleanup until its own.
    let delivered = guarded(attempt).await.unwrap();

    assert!(
        matches!(
            delivered,
            Err(MessageError::Reservation(ReservationError::Timeout))
        ),
        "{delivered:?}"
    );
    assert_eq!((rig.pastes(), rig.reads()), (0, 3));
    let later = rig.rt.control.run_reserved(
        rig.reservation(),
        |_| async { Ok::<_, ReservationError>(()) },
        no_cleanup,
    );
    let refused = guarded(later).await.unwrap_err();
    assert!(
        matches!(refused, ReservationError::Quarantined { .. }),
        "{refused:?}"
    );
}

/// Nothing is pinned about the pane's harness yet (a start, an adopt, the
/// pass after a daemon restart): the recipient cannot be observed. That is a
/// lost observation, not an observation of unknown facts: no attempt, and
/// the policy is back at its wait for a pane it knows nothing about.
#[tokio::test(start_paused = true)]
async fn unpinned_harness_is_a_lost_observation() {
    let mut rig = Rig::attempt().await;
    let id = rig.ready_message().await;
    (rig.rt.agents).record_baseline(worker(), "%1", PaneBaseline::adopted(100));
    rig.script(&[pi(IDLE), pi(IDLE)]);
    let before = rig.org.status().unwrap();

    let delivered = rig.deliver(id).await;

    assert!(
        matches!(delivered, Err(MessageError::Refused(_))),
        "{delivered:?}"
    );
    guarded(rig.org.snapshot()).await.unwrap();
    assert_eq!(rig.org.status().unwrap(), before);
    assert_eq!((rig.pastes(), rig.reads()), (0, 3));
    // Unknown facts would wait on the editor, with the readiness clock
    // running. A loss leaves the focus wait, which has no clock.
    let step = rig.step(id);
    assert!(
        matches!(
            step,
            DeliveryStep::Wait {
                reason: WaitReason::OwnerFocused,
                ..
            }
        ),
        "{step:?}"
    );
}

/// Whatever the one guarded paste answers, it is called once. Every tmux
/// call is counted: three reads per capture or guard, and one for a guard
/// or capture whose first read fails. A possible effect is followed by
/// cleanup's one token capture; a refusal by none. After the applied paste
/// tmux fails the first look at the editor, so the attempt ends there.
#[tokio::test(start_paused = true)]
async fn one_paste_guarded_call_per_deliver() {
    let foreign = with(pi(IDLE), |capture| {
        capture.inspection.local_agent_tag = Some("demo/other".into());
    });
    let cases = [
        // (the frames after the first capture, pastes, tmux calls, held)
        // The guard allows, the paste lands, cleanup looks for the token.
        ("applied", vec![pi(IDLE)], 1, 3 + 3 + 1 + 1 + 1, true),
        ("the guard refused", vec![focused()], 0, 3 + 3, false),
        ("a foreign tag", vec![foreign], 0, 3 + 3, false),
        // tmux fails the guard's own capture: the actor reports that like a
        // failed send, so the outcome is unknown. Cleanup's capture fails too.
        ("tmux failed", vec![], 0, 3 + 1 + 1, true),
    ];
    for (case, frames, pastes, calls, held) in cases {
        let mut rig = Rig::attempt().await;
        let id = rig.ready_message().await;
        rig.script(&[pi(IDLE)]);
        rig.script(&frames);
        let before = rig.org.status().unwrap();
        let reads = rig.tmux.mock().calls().len();

        let delivered = rig.deliver(id).await;

        assert_eq!(delivered.is_ok(), !held, "{case}: {delivered:?}");
        assert_eq!((rig.pastes(), rig.keys()), (pastes, 0), "{case}");
        assert_eq!(rig.tmux.mock().calls().len() - reads, calls, "{case}");
        assert_eq!(rig.reads(), 0, "{case}");
        let snapshot = rig.snapshot(id);
        assert_eq!(snapshot.last_attempt_seq, Some(before + 1), "{case}");
        // Only an attempt that typed nothing is failed; a paste that was or
        // may have been applied is held, and nothing is written for it.
        assert_eq!(
            snapshot.hold,
            held.then_some(ExecutorHold::InputOutcomeUnknown),
            "{case}"
        );
        let status = if held {
            DeliveryStatus::Delivering
        } else {
            DeliveryStatus::Queued
        };
        assert_eq!(snapshot.message.status, status, "{case}");
        guarded(rig.org.snapshot()).await.unwrap();
        let written = rig.org.status().unwrap() - before;
        assert_eq!(written, if held { 1 } else { 2 }, "{case}");
    }
}

/// The attempt's reply is lost while its append is still on its way. Cleanup
/// asks the writer again, behind the append: the attempt exists, typed
/// nothing, and is failed for a retry.
#[tokio::test(start_paused = true)]
async fn attempt_reply_lost_fifo_snapshot_accounts_commit() {
    let mut rig = Rig::attempt().await;
    let id = rig.ready_message().await;
    rig.script(&[pi(IDLE), pi(IDLE)]);
    let before = rig.org.status().unwrap();
    rig.hold();
    let attempt = rig.spawn_deliver(id);
    assert_eq!(rig.entered().await, before + 1);
    // The writer's reply deadline passes; cleanup's snapshot then queues
    // behind the held append.
    tokio::time::advance(Duration::from_secs(7)).await;
    until("cleanup asked the writer", || {
        rig.org.requests.capacity() < rig.org.requests.max_capacity()
    })
    .await;
    assert_eq!(rig.org.status().unwrap(), before);
    rig.release();

    let delivered = guarded(attempt).await.unwrap();

    assert!(
        matches!(delivered, Err(MessageError::Organisation(_))),
        "{delivered:?}"
    );
    assert_eq!((rig.pastes(), rig.reads()), (0, 3));
    let snapshot = rig.snapshot(id);
    assert_eq!(snapshot.last_attempt_seq, Some(before + 1));
    assert_eq!(snapshot.message.status, DeliveryStatus::Queued);
    assert_eq!(snapshot.hold, None);
}

fn one_attempt() -> DeliveryPolicy {
    DeliveryPolicy::new(DeliverySettings {
        retry_delays: vec![],
        ..DeliverySettings::default()
    })
    .unwrap()
}

/// The final attempt types nothing. The executor records that and writes no
/// failure: the policy fails the message after the confirmation window.
#[tokio::test(start_paused = true)]
async fn final_no_effect_attempt_is_left_to_the_policy() {
    let mut rig = Rig::start(Fold::Service, true, one_attempt(), None).await;
    let id = rig.ready_message().await;
    rig.script(&[pi(IDLE), focused()]);
    let before = rig.org.status().unwrap();

    rig.deliver(id).await.unwrap();

    // The attempt is the last event: no failure, and no hold in its place.
    guarded(rig.org.snapshot()).await.unwrap();
    assert_eq!(rig.org.status().unwrap(), before + 1);
    let snapshot = rig.snapshot(id);
    assert_eq!(snapshot.message.status, DeliveryStatus::Delivering);
    assert_eq!((snapshot.hold, rig.pastes()), (None, 0));
    assert!(
        matches!(rig.step(id), DeliveryStep::AwaitConfirmation { .. }),
        "{:?}",
        rig.step(id)
    );
    tokio::time::advance(Duration::from_secs(3)).await;
    let failed = DeliveryStep::Fail {
        reason: FailureReason::ConfirmationTimeout,
    };
    assert_eq!(rig.step(id), failed);
}

/// Even a final no-effect attempt needs a retained NotStarted record
/// before it can be left to policy. Exercise the failure-settlement seam
/// with the real unavailable-history backend: neither the record nor a
/// fallback hold can be retained, so cleanup must fail, not report success.
#[tokio::test(start_paused = true)]
async fn final_no_effect_attempt_is_not_left_to_policy_without_history_record() {
    let mut rig = Rig::start(Fold::Hand, true, one_attempt(), None).await;
    let id = rig.ready_message().await;
    let seq = rig
        .commit(OrgCommand::DeliveryAttempted {
            message_id: id,
            attempt: 1,
            pane: pane().to_string(),
        })
        .await;
    rig.fold();
    assert_eq!(rig.snapshot(id).last_attempt_seq, Some(seq));
    let progress = Progress {
        attempt: AttemptCommit::Committed,
        number: 1,
        before: seq - 1,
        seq: Some(seq),
        paste: NOT_STARTED,
        enter: NOT_STARTED,
        format: Some(CaptureFormat::Ansi),
        settled: None,
    };
    let mut rt = rig.rt.clone();
    rt.history = MessageHistoryHandle::disabled("history is unavailable");
    let attempt = Attempt {
        rt,
        id,
        pane: pane(),
        progress: Arc::new(Mutex::new(progress)),
    };

    let settled = attempt
        .fail(progress, Instant::now() + ATTEMPT_CLEANUP_BUDGET)
        .await;

    assert_eq!(settled, Settled::Neither);
    assert!(!cleanup_succeeds(AttemptDisposition::Fail, settled));
    assert!(rig.effects().is_empty());
    guarded(rig.org.snapshot()).await.unwrap();
    assert_eq!(
        rig.org.status().unwrap(),
        seq,
        "no failure may be written without the record"
    );
}

/// The same final attempt under the service's own recipient job: the policy
/// fails the message once, and the owner is told once.
#[tokio::test(start_paused = true)]
async fn final_no_effect_attempt_ends_failed_with_one_owner_notice() {
    let effects = Effects {
        deliver: Arc::new(|rt, id, deadline| Box::pin(deliver(rt, id, deadline))),
        observe: Arc::new(|_| Box::pin(async { Ok(idle_observation()) })),
    };
    let mut rig = Rig::start(Fold::Service, true, one_attempt(), Some(effects)).await;
    while rig.committed.try_recv().is_ok() {}
    rig.script(&[pi(IDLE), focused()]);
    let id = MessageId::new(rig.commit(Rig::message(worker())).await);

    let mut events = Vec::new();
    for _ in 0..4 {
        events.push(rig.message_event().await);
    }

    assert!(
        matches!(&events[0], OrgEvent::MessageSent(sent) if sent.to == Address::Agent(worker()))
    );
    assert!(
        matches!(&events[1], OrgEvent::MessageDeliveryAttempted { message_id, attempt: 1, .. } if *message_id == id),
        "{events:?}"
    );
    assert!(
        matches!(&events[2], OrgEvent::MessageDeliveryFailed { message_id, final_failure: true, .. } if *message_id == id),
        "{events:?}"
    );
    assert!(
        matches!(&events[3], OrgEvent::MessageSent(notice) if notice.to == Address::Owner && notice.body.contains("failed")),
        "{events:?}"
    );
    assert_eq!((rig.pastes(), rig.reads()), (0, 0));
    // Nothing else was written for the message: one failure, one notice.
    guarded(rig.org.snapshot()).await.unwrap();
    assert!(rig.committed.try_recv().is_err());
}

/// The same final attempt when its work ends with an error: the reply to
/// its commit is lost, so nothing is armed and nothing typed. Cleanup finds
/// the attempt, records that it typed nothing and leaves it to the policy,
/// and the executor reports no error for it. The scheduler would hold a
/// committed attempt that came back with an error as "input outcome is
/// unknown", for the owner to give up. Instead the message ends failed,
/// with one notice, as when work returned normally.
#[tokio::test(start_paused = true)]
async fn final_no_effect_attempt_with_a_work_error_ends_failed() {
    let effects = Effects {
        deliver: Arc::new(|rt, id, deadline| Box::pin(deliver(rt, id, deadline))),
        observe: Arc::new(|_| Box::pin(async { Ok(idle_observation()) })),
    };
    let mut rig = Rig::start(Fold::Service, true, one_attempt(), Some(effects)).await;
    while rig.committed.try_recv().is_ok() {}
    rig.script(&[pi(IDLE), focused()]);
    let id = MessageId::new(rig.commit(Rig::message(worker())).await);
    let before = rig.org.status().unwrap();
    rig.hold();
    assert_eq!(rig.entered().await, before + 1);
    // The writer's reply deadline passes; cleanup's snapshot then queues
    // behind the held append.
    tokio::time::advance(Duration::from_secs(7)).await;
    until("cleanup asked the writer", || {
        rig.org.requests.capacity() < rig.org.requests.max_capacity()
    })
    .await;
    rig.release();

    let mut events = Vec::new();
    for _ in 0..4 {
        events.push(rig.message_event().await);
    }

    assert!(
        matches!(&events[0], OrgEvent::MessageSent(sent) if sent.to == Address::Agent(worker()))
    );
    assert!(
        matches!(&events[1], OrgEvent::MessageDeliveryAttempted { message_id, attempt: 1, .. } if *message_id == id),
        "{events:?}"
    );
    assert!(
        matches!(&events[2], OrgEvent::MessageDeliveryFailed { message_id, final_failure: true, .. } if *message_id == id),
        "{events:?}"
    );
    assert!(
        matches!(&events[3], OrgEvent::MessageSent(notice) if notice.to == Address::Owner && notice.body.contains("failed")),
        "{events:?}"
    );
    // The paste guard's frame was never read, and nothing is held.
    assert_eq!((rig.pastes(), rig.reads()), (0, 3));
    assert_eq!(rig.snapshot(id).hold, None);
    guarded(rig.org.snapshot()).await.unwrap();
    assert!(rig.committed.try_recv().is_err());
}

// ---------------------------------------------------------------------------
// After the paste: the staged draft, the one Enter, the receipt
// ---------------------------------------------------------------------------
//
// The frames go on in order: each look at the editor between the paste and
// the Enter, the Enter guard's own capture, and cleanup's token capture when
// no receipt came. The control actor paces captures of one pane, so on the
// paused clock every one of them is a timer the test lets run by waiting for
// what follows it: an audit record, or the attempt's return.

/// The whole attempt: paste, the draft read as staged, one Enter, and the
/// session's own prompt event with the exact reference. Delivered is
/// written and folded before evidence forgets the receipt, and the Enter is
/// audited once, with nothing of the message's body.
#[tokio::test(start_paused = true)]
async fn exact_receipt_delivered_after_fold() {
    let mut rig = Rig::attempt().await;
    let id = rig.ready_message().await;
    rig.script(&[pi(IDLE), pi(IDLE), staged(id), staged(id)]);
    let before = rig.org.status().unwrap();
    let unpasted = rig.snapshot(id).live.last_paste_at;
    let attempt = rig.spawn_deliver(id);

    let audit = rig.audited().await;
    // Work recorded its applied paste: the pane's paste time moved.
    assert!(rig.snapshot(id).live.last_paste_at > unpasted);
    assert_eq!(
        audit.body,
        serde_json::json!({"pane": "%1", "action": {
            "method": "msg.deliver", "message": id.to_string(), "attempt": 1,
            "op": "enter", "outcome": "ok", "error": null,
        }})
    );
    // The Enter is out and nothing confirms it yet: the attempt waits.
    assert!(rig.typed(id), "{:?}", rig.effects());
    assert_eq!(rig.snapshot(id).message.status, DeliveryStatus::Delivering);
    rig.prompt("s1", inbox_reference(id));
    guarded(attempt).await.unwrap().unwrap();

    assert!(rig.typed(id), "{:?}", rig.effects());
    assert_eq!(rig.reads(), 0);
    let snapshot = rig.snapshot(id);
    assert_eq!(snapshot.message.status, DeliveryStatus::Delivered);
    assert_eq!(snapshot.hold, None);
    // Acknowledged only after Delivered was folded: evidence forgot it.
    assert!(rig.rt.evidence.snapshot(id).is_err());
    // The attempt, the audit and the delivery: nothing else was written.
    guarded(rig.org.snapshot()).await.unwrap();
    assert_eq!(rig.org.status().unwrap(), before + 3);
}

/// The harness draws the paste late: the first look still shows an empty
/// editor. That is not a mismatch. The second look shows the draft, and
/// exactly one Enter follows. With no receipt the attempt is then held at
/// the end of the confirmation window, never failed.
#[tokio::test(start_paused = true)]
async fn draft_appears_on_second_frame_then_one_enter() {
    let mut rig = Rig::attempt().await;
    let id = rig.ready_message().await;
    rig.script(&[pi(IDLE), pi(IDLE), pi(IDLE), staged(id), staged(id)]);
    // Cleanup's capture: the submitted prompt, as the transcript's first row.
    rig.script(&[showing(&inbox_reference(id))]);
    let before = rig.org.status().unwrap();

    rig.deliver(id).await.unwrap();

    assert!(rig.typed(id), "{:?}", rig.effects());
    assert_eq!(rig.reads(), 0);
    let snapshot = rig.snapshot(id);
    assert_eq!(snapshot.message.status, DeliveryStatus::Delivering);
    assert_eq!(snapshot.hold, Some(ExecutorHold::TokenVisible));
    // The attempt and the Enter's audit: no failure was written.
    rig.rt.control.flush_audits().await;
    guarded(rig.org.snapshot()).await.unwrap();
    assert_eq!(rig.org.status().unwrap(), before + 2);
}

/// The receipt wait ends at the event's deadline, not a fresh window from
/// Enter or from the fold. Cleanup's capture is already unpaced at that
/// instant, so the return itself pins the bound on the paused clock.
async fn unconfirmed_enter_returns_at_event_deadline(delay_fold: bool) {
    let mut rig = Rig::hand().await;
    let id = rig.ready_message().await;
    rig.script(&[pi(IDLE), pi(IDLE), staged(id), staged(id)]);
    rig.script(&[showing(&inbox_reference(id))]);
    let attempt = rig.spawn_deliver(id);
    let envelope = rig.committed().await;
    assert_eq!(envelope.kind, "msg.delivery_attempted");
    let event_at = rig.rt.clock.deadline(envelope.ts).unwrap();
    let deadline = event_at + rig.rt.policy.settings().confirmation_timeout;
    if delay_fold {
        // Work is parked on this event's fold, not on real I/O.
        tokio::time::advance(Duration::from_secs(1)).await;
    }
    rig.apply(&envelope);
    rig.fold_rest();

    guarded(attempt).await.unwrap().unwrap();

    assert_eq!(Instant::now(), deadline);
    assert!(rig.typed(id), "{:?}", rig.effects());
    assert_eq!(rig.reads(), 0);
    assert_eq!(rig.snapshot(id).hold, Some(ExecutorHold::TokenVisible));
}

#[tokio::test(start_paused = true)]
async fn unconfirmed_enter_has_no_new_confirmation_window() {
    unconfirmed_enter_returns_at_event_deadline(false).await;
}

#[tokio::test(start_paused = true)]
async fn delayed_attempt_fold_does_not_restart_confirmation_window() {
    unconfirmed_enter_returns_at_event_deadline(true).await;
}

/// The draft never shows. The executor keeps looking, sends no key, and at
/// the end of the confirmation window the attempt is held.
#[tokio::test(start_paused = true)]
async fn draft_never_appears_holds_without_enter() {
    let mut rig = Rig::attempt().await;
    let id = rig.ready_message().await;
    // More frames than the window has room for.
    let frames = 40;
    rig.script(&vec![pi(IDLE); frames]);
    let before = rig.org.status().unwrap();

    rig.deliver(id).await.unwrap();

    assert_eq!((rig.pastes(), rig.keys()), (1, 0));
    // It looked more than once after the paste, and stopped.
    let looks = frames - rig.reads() / 3 - 2;
    assert!(looks > 2 && rig.reads() > 0, "{looks} looks");
    let snapshot = rig.snapshot(id);
    assert_eq!(snapshot.message.status, DeliveryStatus::Delivering);
    assert_eq!(snapshot.hold, Some(ExecutorHold::InputOutcomeUnknown));
    rig.rt.control.flush_audits().await;
    guarded(rig.org.snapshot()).await.unwrap();
    assert_eq!(rig.org.status().unwrap(), before + 1);
}

/// The Enter guard refuses on its own capture (the owner's client came to
/// the pane): nothing was sent, so the executor looks again, and the one
/// Enter goes out on a later capture that allows it.
#[tokio::test(start_paused = true)]
async fn enter_screen_changed_reobserves_before_retry() {
    let mut rig = Rig::attempt().await;
    let id = rig.ready_message().await;
    let watched = with(staged(id), |capture| capture.focused = Some(true));
    rig.script(&[pi(IDLE), pi(IDLE), staged(id), watched]);
    rig.script(&[staged(id), staged(id)]);
    let attempt = rig.spawn_deliver(id);

    let audit = rig.audited().await;
    assert_eq!(audit.body["action"]["outcome"], "ok");
    rig.prompt("s1", inbox_reference(id));
    guarded(attempt).await.unwrap().unwrap();

    // Six captures, two of them Enter guards, and one Enter.
    assert!(rig.typed(id), "{:?}", rig.effects());
    assert_eq!(rig.reads(), 0);
    assert_eq!(rig.snapshot(id).message.status, DeliveryStatus::Delivered);
    // One audit: the refused guard sent nothing and is not an input.
    rig.rt.control.flush_audits().await;
    guarded(rig.org.snapshot()).await.unwrap();
    assert!(rig
        .committed
        .try_recv()
        .is_ok_and(|e| e.kind == "msg.delivered"));
    assert!(rig.committed.try_recv().is_err());
}

/// Any return of the Enter call but a refused screen check ends the keys of
/// the attempt, also one that says nothing was sent: more staged frames are
/// scripted and none of them gets a key.
#[tokio::test(start_paused = true)]
async fn enter_unknown_outcome_sends_no_second_key() {
    type Refusal = fn(&ReservationError) -> bool;
    let cases: [(&str, bool, Refusal); 2] = [
        // tmux fails the guard's own capture: the actor reports that like a
        // failed send. Nothing can be read after it either.
        ("tmux failed", false, |error| {
            matches!(error, ReservationError::Control(_))
        }),
        // Another agent's tag: a definite "nothing was sent".
        ("a foreign tag", true, |error| {
            matches!(error, ReservationError::TagMismatch { .. })
        }),
    ];
    for (case, readable, expected) in cases {
        let mut rig = Rig::attempt().await;
        let id = rig.ready_message().await;
        rig.script(&[pi(IDLE), pi(IDLE), staged(id)]);
        if readable {
            let foreign = with(staged(id), |capture| {
                capture.inspection.local_agent_tag = Some("demo/other".into());
            });
            // The guard's frame, cleanup's, and three nobody may read.
            rig.script(&[foreign, staged(id), staged(id), staged(id), staged(id)]);
        }
        let before = rig.org.status().unwrap();
        let calls = rig.tmux.mock().calls().len();

        let delivered = rig.deliver(id).await;

        assert!(
            matches!(&delivered, Err(MessageError::Reservation(error)) if expected(error)),
            "{case}: {delivered:?}"
        );
        assert_eq!((rig.pastes(), rig.keys()), (1, 0), "{case}");
        if readable {
            assert_eq!(rig.reads(), 3 * 3, "{case}");
        } else {
            // Three captures and the paste, then one failed read each for
            // the Enter guard and for cleanup's capture.
            let made = rig.tmux.mock().calls().len() - calls;
            assert_eq!(made, 3 * 3 + 1 + 1 + 1, "{case}");
        }
        let hold = if readable {
            ExecutorHold::EnterNotSent
        } else {
            ExecutorHold::InputOutcomeUnknown
        };
        assert_eq!(rig.snapshot(id).hold, Some(hold), "{case}");
        // The returned call is audited once, as an error; nothing is failed.
        let audit = rig.audited().await;
        assert_eq!(audit.body["action"]["outcome"], "error", "{case}");
        rig.rt.control.flush_audits().await;
        guarded(rig.org.snapshot()).await.unwrap();
        assert_eq!(rig.org.status().unwrap(), before + 2, "{case}");
    }
}

/// Copy mode is a no-effect error, but unlike ScreenChanged it must end
/// the attempt immediately. Only cleanup may read another frame; even an
/// exact staged draft after the refusal must not lead to another Enter.
#[tokio::test(start_paused = true)]
async fn enter_pane_input_refusal_ends_attempt_without_another_look() {
    let mut rig = Rig::attempt().await;
    let id = rig.ready_message().await;
    let mode = with(staged(id), |capture| capture.inspection.in_mode = true);
    rig.script(&[pi(IDLE), pi(IDLE), staged(id), mode]);
    // One cleanup frame, then three frames nobody may read.
    rig.script(&vec![staged(id); 4]);

    let delivered = rig.deliver(id).await;

    assert!(
        matches!(
            delivered,
            Err(MessageError::Reservation(
                ReservationError::PaneInputRefused { .. }
            ))
        ),
        "{delivered:?}"
    );
    assert_eq!((rig.pastes(), rig.keys(), rig.reads()), (1, 0, 3 * 3));
    assert_eq!(rig.snapshot(id).hold, Some(ExecutorHold::EnterNotSent));
}

/// The wait for the receipt wakes on evidence's own change counter and reads
/// what evidence retained. A change that is no receipt (the session is
/// active) keeps it waiting; the exact prompt then ends it.
#[tokio::test(start_paused = true)]
async fn confirmation_watch_wake_rechecks_retained_receipt() {
    let mut rig = Rig::attempt().await;
    let id = rig.ready_message().await;
    rig.script(&[pi(IDLE), pi(IDLE), staged(id), staged(id)]);
    let attempt = rig.spawn_deliver(id);
    rig.audited().await;

    let wakes = rig.wakes();
    rig.prompt("s1", "something else the session was told".into());
    assert_ne!(rig.wakes(), wakes, "activity did not change evidence");
    assert!(rig.rt.evidence.snapshot(id).unwrap().probably_landed);
    // The clock stands still: whatever the wake led to has happened now.
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    assert!(!attempt.is_finished());
    assert_eq!(rig.snapshot(id).message.status, DeliveryStatus::Delivering);

    rig.prompt("s1", inbox_reference(id));
    // Still no time passes: the wake ends the wait, not the window's end.
    until("the receipt ended the wait", || attempt.is_finished()).await;
    guarded(attempt).await.unwrap().unwrap();
    assert_eq!(rig.snapshot(id).message.status, DeliveryStatus::Delivered);
    assert!(rig.typed(id), "{:?}", rig.effects());
}

/// The session reports the exact reference after the paste and before the
/// Enter (the owner submitted the draft, or an earlier attempt's prompt came
/// late): the message is delivered, and a second prompt is not sent.
#[tokio::test(start_paused = true)]
async fn receipt_before_enter_sends_no_key() {
    let mut rig = Rig::attempt().await;
    let id = rig.ready_message().await;
    // The look that would allow the Enter; its guard has no frame.
    rig.script(&[pi(IDLE), pi(IDLE), staged(id)]);
    let attempt = rig.spawn_deliver(id);
    rig.pasted().await;
    rig.prompt("s1", inbox_reference(id));

    guarded(attempt).await.unwrap().unwrap();

    assert_eq!((rig.pastes(), rig.keys(), rig.reads()), (1, 0, 0));
    assert_eq!(rig.snapshot(id).message.status, DeliveryStatus::Delivered);
    assert!(rig.rt.evidence.snapshot(id).is_err());
}

/// The registry row of a new session on the worker's pane.
fn row(session: &str) -> SessionDomain {
    let mut row = SessionDomain::new(
        SessionId::new(session),
        AgentType::GeneralPurpose,
        Model::default(),
    );
    row.harness = Harness::Pi;
    row.tmux_pane = Some(pane().to_string());
    row
}

/// While the attempt's event is on its way, the session the agent is bound
/// to ends and another one starts on the pane. The attempt was committed
/// for the first one: nothing is pasted, and it is failed for a retry.
#[tokio::test(start_paused = true)]
async fn session_changed_before_paste_no_input() {
    let mut rig = Rig::attempt().await;
    let id = rig.ready_message().await;
    // The first capture alone. A paste guard would find no frame, which
    // leaves an unknown outcome and a hold. (No spare frame is scripted and
    // counted instead: the agent service looks at the pane after a registry
    // change, once the attempt has released it.)
    rig.script(&[pi(IDLE)]);
    let before = rig.org.status().unwrap();
    rig.hold();
    let attempt = rig.spawn_deliver(id);
    assert_eq!(rig.entered().await, before + 1);
    let registry = rig.rt.registry.clone();
    guarded(registry.remove(SessionId::new("s1")))
        .await
        .unwrap();
    guarded(registry.register(row("s2"))).await.unwrap();
    rig.release();

    let delivered = guarded(attempt).await.unwrap();

    assert!(
        matches!(delivered, Err(MessageError::Refused(_))),
        "{delivered:?}"
    );
    assert!(rig.effects().is_empty(), "{:?}", rig.effects());
    let snapshot = rig.snapshot(id);
    assert_eq!(snapshot.last_attempt_seq, Some(before + 1));
    assert_eq!(snapshot.message.status, DeliveryStatus::Queued);
    assert_eq!(snapshot.hold, None);
}

/// A row can arrive while delivery is waiting for its reservation. The
/// binding observer is behind that reservation, so a Ready unbound Codex
/// recipient with an eligible row is reachable and must spend no attempt.
#[tokio::test(start_paused = true)]
async fn unbound_codex_with_an_eligible_row_is_refused_before_commit() {
    let (rig, id) = ready_unbound_codex().await;
    let parked = rig.park().await;
    let attempt = rig.spawn_deliver(id);
    rig.admission_requested(id).await;
    let mut row = row("new");
    row.harness = Harness::Codex;
    rig.rt.registry.register(row).await.unwrap();
    let recipient = rig.rt.agents.recipient(&worker()).await.unwrap();
    assert_eq!(recipient.status, AgentStatus::Ready);
    assert!(recipient.session_id.is_none());
    assert_eq!(
        recipient.session.as_ref().map(|row| row.id.clone()),
        Some(SessionId::new("new"))
    );
    rig.script(&[codex_frame(CODEX_EMPTY)]);
    parked.send(()).unwrap();

    let delivered = guarded(attempt).await.unwrap();

    assert!(
        matches!(delivered, Err(MessageError::Refused(_))),
        "{delivered:?}"
    );
    assert!(rig.effects().is_empty(), "{:?}", rig.effects());
    assert!(rig.snapshot(id).message.last_attempt.is_none());
    assert_eq!(rig.snapshot(id).message.status, DeliveryStatus::Queued);
}

/// The same arrival race for a bound agent: another session's row does not
/// match its recorded binding even at the first capture, before any commit.
#[tokio::test(start_paused = true)]
async fn bound_recipient_with_another_row_is_refused_before_commit() {
    let mut rig = Rig::hand().await;
    let id = rig.ready_message().await;
    let parked = rig.park().await;
    let attempt = rig.spawn_deliver(id);
    rig.admission_requested(id).await;
    rig.rt.registry.remove(SessionId::new("s1")).await.unwrap();
    rig.rt.registry.register(row("s2")).await.unwrap();
    rig.script(&[pi(IDLE)]);
    parked.send(()).unwrap();

    let delivered = guarded(attempt).await.unwrap();

    assert!(
        matches!(delivered, Err(MessageError::Refused(_))),
        "{delivered:?}"
    );
    assert!(rig.effects().is_empty(), "{:?}", rig.effects());
    assert!(rig.snapshot(id).message.last_attempt.is_none());
    assert_eq!(rig.snapshot(id).message.status, DeliveryStatus::Queued);
}

/// A change after the paste, before the draft is seen: the reference is
/// staged in a pane whose session is no longer the armed one. No Enter; the
/// attempt is held, with the token as the reason. Two changes: the pane's
/// row is another session's while the agent is still bound to the first
/// (the gate refuses), and the agent is bound to the new session as well
/// (the gate agrees with itself, and its answer is not the armed scope's).
#[tokio::test(start_paused = true)]
async fn session_changed_before_enter_no_key() {
    for rebound in [false, true] {
        let mut rig = Rig::attempt().await;
        let id = rig.ready_message().await;
        // The look that would allow the Enter, and cleanup's capture. The
        // Enter guard's own frame is not scripted: reaching it would fail
        // the read count as well as the key count.
        rig.script(&[pi(IDLE), pi(IDLE), staged(id), staged(id)]);
        let before = rig.org.status().unwrap();
        let attempt = rig.spawn_deliver(id);
        rig.pasted().await;
        let registry = rig.rt.registry.clone();
        guarded(registry.remove(SessionId::new("s1")))
            .await
            .unwrap();
        guarded(registry.register(row("s2"))).await.unwrap();
        if rebound {
            rig.commit(OrgCommand::SessionBound {
                agent_id: worker(),
                session_id: SessionId::new("s2"),
            })
            .await;
        }

        let delivered = guarded(attempt).await.unwrap();

        assert_eq!(
            matches!(delivered, Err(MessageError::Conflict(_))),
            rebound,
            "{delivered:?}"
        );
        assert_eq!(
            matches!(delivered, Err(MessageError::Refused(_))),
            !rebound,
            "{delivered:?}"
        );
        assert_eq!((rig.pastes(), rig.keys(), rig.reads()), (1, 0, 0));
        let snapshot = rig.snapshot(id);
        assert_eq!(snapshot.message.status, DeliveryStatus::Delivering);
        assert_eq!(snapshot.hold, Some(ExecutorHold::EnterNotSent));
        // No failure, and no Enter to audit: the attempt, and the binding.
        rig.rt.control.flush_audits().await;
        guarded(rig.org.snapshot()).await.unwrap();
        let written = rig.org.status().unwrap() - before;
        assert_eq!(written, 1 + u64::from(rebound));
    }
}

// ---------------------------------------------------------------------------
// The guard on real frames
// ---------------------------------------------------------------------------

/// `guard_facts` reads one capture once: the control read's facts and its
/// dialog, the staged match and the pane's flags. With pi in the foreground
/// the real idle frame allows a paste; with the shell there every screen
/// fact is unknown, the control dialog included, and nothing is allowed.
#[test]
fn foreground_unknown_masks_editor_facts() {
    let screen = agent_screen(Harness::Pi).unwrap();
    let reference = inbox_reference(MessageId::new(123));
    let idle = pi(IDLE);
    let facts = guard_facts(GuardedInput::Paste, &idle, screen, &baseline(), &reference);
    assert_eq!(facts.control_dialog, ControlDialog::None);
    assert_eq!(facts.screen, screen.control(&idle, &baseline()).facts);
    assert!(guard_allows_input(&facts), "{facts:?}");

    let shell = with(pi(IDLE), |capture| {
        capture.inspection.foreground_command = Some("zsh".into());
    });
    for input in [GuardedInput::Paste, GuardedInput::Enter] {
        let facts = guard_facts(input, &shell, screen, &baseline(), &reference);
        assert_eq!(facts.screen, ScreenFacts::default());
        assert_eq!(facts.control_dialog, ControlDialog::Unknown);
        assert_eq!(facts.staged, None);
        assert!(!guard_allows_input(&facts), "{facts:?}");
    }
}

/// Real command-completion frames (Codex 0.167.0 with `/new` typed, Claude
/// 2.1.325 with `/clear` typed; each adapter's fixture README has the
/// `Captured:` line): the adapter matches the draft exactly, a popup is
/// open, and the guard gives no Enter. A match never picks a popup's row.
#[test]
fn command_completion_never_overrides_unknown_dialog() {
    use crate::org::agent_screen_tests::capture as shot_of;
    let frames = [
        (
            Harness::Codex,
            "codex",
            "/new",
            include_str!(
                "../../../atm-codex-adapter/tests/fixtures/screen_styled_p1_b09-valid-new-draft-100x32_0.167.0.txt"
            ),
        ),
        (
            Harness::ClaudeCode,
            "claude",
            "/clear",
            include_str!(
                "../../../atm-claude-adapter/tests/fixtures/screen_styled_p1_B-clear-001-120x40_2.1.325.txt"
            ),
        ),
    ];
    for (harness, command, draft, fixture) in frames {
        let screen = agent_screen(harness).unwrap();
        let baseline = (PaneBaseline::adopted(100).with_harness(command.into())).unwrap();
        let capture =
            shot_of(fixture, CaptureFormat::Ansi, Some(100), Some(command)).with_focus(Some(false));
        let facts = guard_facts(GuardedInput::Enter, &capture, screen, &baseline, draft);
        assert_eq!(facts.staged, Some(true), "{command}: {facts:?}");
        assert!(!guard_allows_input(&facts), "{command}: {facts:?}");

        // Synthetic: delivery guard adversarial mutation of a captured
        // command-completion frame: the reference in place of the command.
        let reference = inbox_reference(MessageId::new(123));
        let mutated = with(capture, |capture| {
            capture.text = capture.text.replace(draft, &reference);
        });
        let facts = guard_facts(GuardedInput::Enter, &mutated, screen, &baseline, &reference);
        assert!(!guard_allows_input(&facts), "{command}: {facts:?}");
    }
}

// ---------------------------------------------------------------------------
// Settlement: effects whose return is held or lost, cancellation, lost replies
// ---------------------------------------------------------------------------

/// The paste is in tmux and its call has not returned when the work deadline
/// passes. The attempt marked it unknown before the call, so cleanup holds
/// and writes no failure; it also records the paste, which work no longer
/// could: the pane's paste time is set.
#[tokio::test(start_paused = true)]
async fn timeout_after_uncertain_paste_holds() {
    let mut rig = Rig::attempt().await;
    let id = rig.ready_message().await;
    rig.script(&[pi(IDLE), pi(IDLE)]);
    let before = rig.org.status().unwrap();
    rig.tmux_gate.hold_next(Effect::PasteText).unwrap();
    // A work deadline that passes while the paste's return is held.
    let deadline = Instant::now() + Duration::from_secs(1);
    let attempt = tokio::spawn(deliver(rig.rt.clone(), id, deadline));
    guarded(rig.tmux_gate.entered()).await;
    assert_eq!(rig.pastes(), 1);
    // Work is inside the paste call and has recorded nothing about it.
    let recorded = rig.snapshot(id).live.last_paste_at;

    let delivered = guarded(attempt).await.unwrap();

    assert!(
        matches!(
            delivered,
            Err(MessageError::Reservation(ReservationError::Timeout))
        ),
        "{delivered:?}"
    );
    let snapshot = rig.snapshot(id);
    assert_eq!(snapshot.message.status, DeliveryStatus::Delivering);
    assert_eq!(snapshot.hold, Some(ExecutorHold::InputOutcomeUnknown));
    assert!(snapshot.live.last_paste_at > recorded);
    rig.tmux_gate.release();
    guarded(rig.org.snapshot()).await.unwrap();
    assert_eq!(rig.org.status().unwrap(), before + 1);
    assert_eq!((rig.pastes(), rig.keys()), (1, 0));
}

/// tmux took the paste and then reported a failure: the outcome is unknown.
/// Held, no failure, and no second paste.
#[tokio::test(start_paused = true)]
async fn paste_sent_then_failed_holds() {
    let mut rig = Rig::attempt().await;
    let id = rig.ready_message().await;
    rig.script(&[pi(IDLE), pi(IDLE), staged(id)]);
    let before = rig.org.status().unwrap();
    rig.tmux_gate
        .fail_next_after_effect(Effect::PasteText)
        .unwrap();

    let delivered = rig.deliver(id).await;

    assert!(
        matches!(
            delivered,
            Err(MessageError::Reservation(ReservationError::Control(_)))
        ),
        "{delivered:?}"
    );
    assert_eq!((rig.pastes(), rig.keys(), rig.reads()), (1, 0, 0));
    let snapshot = rig.snapshot(id);
    assert_eq!(snapshot.message.status, DeliveryStatus::Delivering);
    assert_eq!(snapshot.hold, Some(ExecutorHold::EnterNotSent));
    guarded(rig.org.snapshot()).await.unwrap();
    assert_eq!(rig.org.status().unwrap(), before + 1);
}

/// The Enter is in tmux and its call has not returned when the work deadline
/// passes: held, no failure, no second key.
#[tokio::test(start_paused = true)]
async fn timeout_after_enter_holds() {
    let mut rig = Rig::attempt().await;
    let id = rig.ready_message().await;
    rig.script(&[pi(IDLE), pi(IDLE), staged(id), staged(id)]);
    let before = rig.org.status().unwrap();
    rig.tmux_gate.hold_next(Effect::SendKeys).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    let attempt = tokio::spawn(deliver(rig.rt.clone(), id, deadline));
    guarded(rig.tmux_gate.entered()).await;
    assert!(rig.typed(id), "{:?}", rig.effects());

    let delivered = guarded(attempt).await.unwrap();

    assert!(
        matches!(
            delivered,
            Err(MessageError::Reservation(ReservationError::Timeout))
        ),
        "{delivered:?}"
    );
    let snapshot = rig.snapshot(id);
    assert_eq!(snapshot.message.status, DeliveryStatus::Delivering);
    assert_eq!(snapshot.hold, Some(ExecutorHold::InputOutcomeUnknown));
    rig.tmux_gate.release();
    rig.rt.control.flush_audits().await;
    guarded(rig.org.snapshot()).await.unwrap();
    assert_eq!(rig.org.status().unwrap(), before + 1);
    assert!(rig.typed(id), "{:?}", rig.effects());
}

/// tmux took the Enter and then reported a failure. More staged frames are
/// waiting, and no second key follows: held, audited once as an error.
#[tokio::test(start_paused = true)]
async fn enter_sent_then_failed_is_not_repeated() {
    let mut rig = Rig::attempt().await;
    let id = rig.ready_message().await;
    rig.script(&[pi(IDLE), pi(IDLE), staged(id), staged(id)]);
    // Cleanup's frame, and two nobody may read.
    rig.script(&[staged(id), staged(id), staged(id)]);
    rig.tmux_gate
        .fail_next_after_effect(Effect::SendKeys)
        .unwrap();

    let delivered = rig.deliver(id).await;

    assert!(
        matches!(
            delivered,
            Err(MessageError::Reservation(ReservationError::Control(_)))
        ),
        "{delivered:?}"
    );
    assert!(rig.typed(id), "{:?}", rig.effects());
    assert_eq!(rig.reads(), 2 * 3);
    assert_eq!(
        rig.snapshot(id).hold,
        Some(ExecutorHold::InputOutcomeUnknown)
    );
    let audit = rig.audited().await;
    assert_eq!(audit.body["action"]["outcome"], "error");
}

/// The Enter's call returns after the confirmation window has ended. That
/// is no failure and no reason for another input: the attempt is held.
#[tokio::test(start_paused = true)]
async fn deadline_already_past_at_enter_return_is_not_failure() {
    let mut rig = Rig::attempt().await;
    let id = rig.ready_message().await;
    rig.script(&[pi(IDLE), pi(IDLE), staged(id), staged(id)]);
    rig.script(&[showing(&inbox_reference(id))]);
    let before = rig.org.status().unwrap();
    rig.tmux_gate.hold_next(Effect::SendKeys).unwrap();
    let attempt = rig.spawn_deliver(id);
    guarded(rig.tmux_gate.entered()).await;
    let window = rig.rt.policy.settings().confirmation_timeout;
    tokio::time::advance(window).await;
    rig.tmux_gate.release();

    guarded(attempt).await.unwrap().unwrap();

    assert!(rig.typed(id), "{:?}", rig.effects());
    let snapshot = rig.snapshot(id);
    assert_eq!(snapshot.message.status, DeliveryStatus::Delivering);
    assert_eq!(snapshot.hold, Some(ExecutorHold::TokenVisible));
    // The attempt and the Enter's audit: no failure.
    rig.rt.control.flush_audits().await;
    guarded(rig.org.snapshot()).await.unwrap();
    assert_eq!(rig.org.status().unwrap(), before + 2);
}

/// The reply to `Delivered` is lost while its append is on its way. The
/// receipt is acknowledged only after `Delivered` is folded, so it is still
/// retained when cleanup gives up, and the message is delivered once the
/// append lands. Nothing is typed again.
#[tokio::test(start_paused = true)]
async fn delivered_reply_lost_is_folded_before_ack() {
    let mut rig = Rig::attempt().await;
    let id = rig.ready_message().await;
    rig.script(&[pi(IDLE), pi(IDLE), staged(id), staged(id)]);
    let attempt = rig.spawn_deliver(id);
    rig.audited().await;
    rig.hold();
    rig.prompt("s1", inbox_reference(id));

    // `Delivered` is entered and held: the receipt must not be gone yet.
    let seq = rig.entered().await;
    assert!(rig.rt.evidence.receipt(id).unwrap().is_some());
    assert_eq!(rig.snapshot(id).message.status, DeliveryStatus::Delivering);
    // The writer's reply deadline passes, and cleanup gives up on the write.
    tokio::time::advance(Duration::from_secs(7)).await;
    guarded(attempt).await.unwrap().unwrap();
    assert!(rig.rt.evidence.receipt(id).unwrap().is_some());
    assert_eq!(rig.snapshot(id).hold, None);

    rig.release();
    rig.folded(seq).await;
    assert_eq!(rig.snapshot(id).message.status, DeliveryStatus::Delivered);
    assert!(rig.typed(id), "{:?}", rig.effects());
}

/// Shutdown begins after the paste and before the Enter: no Enter, and the
/// attempt is held with the token as its reason.
#[tokio::test(start_paused = true)]
async fn cancel_between_paste_and_enter_holds_without_enter() {
    let mut rig = Rig::hand().await;
    let id = rig.ready_message().await;
    // The look that would allow the Enter, and cleanup's capture.
    rig.script(&[pi(IDLE), pi(IDLE), staged(id), staged(id)]);
    let before = rig.org.status().unwrap();
    rig.fold_rest();
    let attempt = rig.spawn_deliver(id);
    rig.pasted().await;
    rig.rt.cancel.cancel();

    let delivered = guarded(attempt).await.unwrap();

    assert!(
        matches!(delivered, Err(MessageError::Unavailable(_))),
        "{delivered:?}"
    );
    assert_eq!((rig.pastes(), rig.keys(), rig.reads()), (1, 0, 0));
    let snapshot = rig.snapshot(id);
    assert_eq!(snapshot.message.status, DeliveryStatus::Delivering);
    assert_eq!(snapshot.hold, Some(ExecutorHold::EnterNotSent));
    guarded(rig.org.snapshot()).await.unwrap();
    assert_eq!(rig.org.status().unwrap(), before + 1);
}

/// Shutdown begins while the attempt waits for its receipt: it is held, not
/// failed. The session's prompt event arrives afterwards and still wins:
/// the hold does not stand in the way of `Delivered`.
#[tokio::test(start_paused = true)]
async fn cancel_while_awaiting_receipt_holds_and_receipt_still_wins() {
    let mut rig = Rig::hand().await;
    let id = rig.ready_message().await;
    rig.script(&[pi(IDLE), pi(IDLE), staged(id), staged(id)]);
    rig.script(&[showing(&inbox_reference(id))]);
    rig.fold_rest();
    let attempt = rig.spawn_deliver(id);
    rig.audited().await;
    rig.rt.cancel.cancel();

    let delivered = guarded(attempt).await.unwrap();

    assert!(
        matches!(delivered, Err(MessageError::Unavailable(_))),
        "{delivered:?}"
    );
    assert!(rig.typed(id), "{:?}", rig.effects());
    let snapshot = rig.snapshot(id);
    assert_eq!(snapshot.message.status, DeliveryStatus::Delivering);
    assert_eq!(snapshot.hold, Some(ExecutorHold::TokenVisible));

    rig.prompt("s1", inbox_reference(id));
    let receipt = rig.rt.evidence.receipt(id).unwrap().unwrap();
    let seq = rig
        .commit(OrgCommand::Delivered {
            message_id: id,
            session_id: receipt.session_id.clone(),
            confirmation: receipt.confirmation.clone(),
        })
        .await;
    rig.folded(seq).await;
    assert_eq!(rig.snapshot(id).message.status, DeliveryStatus::Delivered);
    assert!(rig.typed(id), "{:?}", rig.effects());
}

// ---------------------------------------------------------------------------
// The reference in real editors
// ---------------------------------------------------------------------------
//
// Captured with the reference pasted and never submitted, in each harness's
// editor at three widths; each adapter's fixture README has the `Captured:`
// lines. `r0` frames hold `[m123] ...`, `r1` frames `[m123456] ...`.

macro_rules! p3 {
    ($vendor:literal, $name:literal, $version:literal) => {
        include_str!(concat!(
            "../../../atm-",
            $vendor,
            "-adapter/tests/fixtures/screen_p3_",
            $name,
            "_",
            $version,
            ".styled.txt"
        ))
    };
}

/// On every real frame of 80 and 120 columns the guard allows a paste into
/// the empty editor and an Enter on the staged reference, and neither the
/// other way round. At 28 columns the reference wraps: no adapter proves a
/// wrapped draft, so no frame there gets an Enter.
#[test]
fn real_staged_reference_frames_allow_enter_only_unwrapped() {
    use crate::org::agent_screen_tests::capture as shot_of;
    let (paste, enter) = (GuardedInput::Paste, GuardedInput::Enter);
    // (harness, its command, [(message id, the empty frame, the staged one)])
    let harnesses = [
        (
            Harness::ClaudeCode,
            "claude",
            [
                (
                    123,
                    p3!("claude", "80x40-r0-C-u-empty", "2.1.325"),
                    p3!("claude", "80x40-r0-C-u-staged", "2.1.325"),
                ),
                (
                    123_456,
                    p3!("claude", "120x40-r1-C-u-empty", "2.1.325"),
                    p3!("claude", "120x40-r1-C-u-staged", "2.1.325"),
                ),
                (
                    123_456,
                    p3!("claude", "28x40-r1-C-u-empty", "2.1.325"),
                    p3!("claude", "28x40-r1-C-u-staged", "2.1.325"),
                ),
            ],
        ),
        (
            Harness::Codex,
            "codex",
            [
                (
                    123,
                    p3!("codex", "80x40-r0-C-u-empty", "0.167.0"),
                    p3!("codex", "80x40-r0-C-u-staged", "0.167.0"),
                ),
                (
                    123_456,
                    p3!("codex", "120x40-r1-C-u-empty", "0.167.0"),
                    p3!("codex", "120x40-r1-C-u-staged", "0.167.0"),
                ),
                (
                    123_456,
                    p3!("codex", "28x40-r1-C-u-empty", "0.167.0"),
                    p3!("codex", "28x40-r1-C-u-staged", "0.167.0"),
                ),
            ],
        ),
        (
            Harness::Pi,
            "pi",
            [
                (
                    123,
                    p3!("pi", "80x40-r0-C-u-empty", "1.5.0"),
                    p3!("pi", "80x40-r0-C-u-staged", "1.5.0"),
                ),
                (
                    123_456,
                    p3!("pi", "120x40-r1-C-u-empty", "1.5.0"),
                    p3!("pi", "120x40-r1-C-u-staged", "1.5.0"),
                ),
                (
                    123_456,
                    p3!("pi", "28x40-r1-C-u-empty", "1.5.0"),
                    p3!("pi", "28x40-r1-C-u-staged", "1.5.0"),
                ),
            ],
        ),
    ];
    for (harness, command, frames) in harnesses {
        let screen = agent_screen(harness).unwrap();
        let baseline = (PaneBaseline::adopted(100).with_harness(command.into())).unwrap();
        let allows = |input, fixture: &str, reference: &str| {
            let capture = shot_of(fixture, CaptureFormat::Ansi, Some(100), Some(command))
                .with_focus(Some(false));
            guard_allows_input(&guard_facts(input, &capture, screen, &baseline, reference))
        };
        for (wrapped, (id, empty, staged)) in [false, false, true].into_iter().zip(frames) {
            let reference = inbox_reference(MessageId::new(id));
            let case = format!("{command}, m{id}, wrapped: {wrapped}");
            assert_eq!(allows(enter, staged, &reference), !wrapped, "{case}");
            assert!(!allows(paste, staged, &reference), "{case}");
            assert!(!allows(enter, empty, &reference), "{case}");
            // Only pi's editor is read as empty at 28 columns.
            let pastes = !wrapped || harness == Harness::Pi;
            assert_eq!(allows(paste, empty, &reference), pastes, "{case}");
            // Another message's reference is not this draft.
            let other = inbox_reference(MessageId::new(id + 1));
            assert!(!allows(enter, staged, &other), "{case}");
        }
    }
}

/// A real pi frame of S1's, as %1 reports it at `width` columns.
fn p3_pi(fixture: &str, width: u16) -> PaneCapture {
    with(pi(fixture), |capture| capture.inspection.pane.width = width)
}

/// The worker on a log that makes its first message `m123456`, the id in
/// the `r1` captures: the seed is four events, the message the fifth. The
/// test's own task folds: the message service's projection starts at zero.
async fn rig_for_m123456() -> (Rig, MessageId) {
    let policy = DeliveryPolicy::default();
    let mut rig = Rig::start_at(123_451, Fold::Hand, true, policy, None).await;
    let id = rig.ready_message().await;
    assert_eq!(id, MessageId::new(123_456));
    rig.fold_rest();
    (rig, id)
}

/// The whole attempt on real pi 1.5.0 frames at 120 columns: the empty
/// editor takes the paste, the staged reference gets its one Enter, and the
/// session's prompt event delivers the message.
#[tokio::test(start_paused = true)]
async fn real_pi_reference_is_staged_submitted_and_confirmed() {
    let (mut rig, id) = rig_for_m123456().await;
    let empty = p3_pi(p3!("pi", "120x40-r1-C-u-empty", "1.5.0"), 120);
    let staged = p3_pi(p3!("pi", "120x40-r1-C-u-staged", "1.5.0"), 120);
    rig.script(&[empty.clone(), empty, staged.clone(), staged]);
    let attempt = rig.spawn_deliver(id);

    let audit = rig.audited().await;
    assert_eq!(audit.body["action"]["outcome"], "ok");
    rig.prompt("s1", inbox_reference(id));
    guarded(attempt).await.unwrap().unwrap();

    assert!(rig.typed(id), "{:?}", rig.effects());
    assert_eq!(rig.reads(), 0);
    assert_eq!(rig.snapshot(id).message.status, DeliveryStatus::Delivered);
}

/// Real pi 1.5.0 frames at 28 columns, where the reference wraps. The empty
/// editor is read as empty, so the reference is pasted; the wrapped draft is
/// never proven, so no Enter is sent; at the end of the confirmation window
/// the attempt is held with the token as its reason, and nothing is failed.
/// The reference stays in the editor: the executor removes no text.
#[tokio::test(start_paused = true)]
async fn narrow_reference_and_trailing_padding_ambiguity_no_enter() {
    let (rig, id) = rig_for_m123456().await;
    let empty = p3_pi(p3!("pi", "28x40-r1-C-u-empty", "1.5.0"), 28);
    let staged = p3_pi(p3!("pi", "28x40-r1-C-u-staged", "1.5.0"), 28);
    rig.script(&[empty.clone(), empty]);
    // More looks than the window has room for, and cleanup's capture.
    rig.script(&vec![staged; 40]);
    let before = rig.org.status().unwrap();

    rig.deliver(id).await.unwrap();

    assert_eq!((rig.pastes(), rig.keys()), (1, 0));
    assert!(rig.reads() > 0 && rig.reads() < 38 * 3, "{}", rig.reads());
    let snapshot = rig.snapshot(id);
    assert_eq!(snapshot.message.status, DeliveryStatus::Delivering);
    assert_eq!(snapshot.hold, Some(ExecutorHold::EnterNotSent));
    // The attempt is the last event: no failure, so no retry.
    rig.rt.control.flush_audits().await;
    guarded(rig.org.snapshot()).await.unwrap();
    assert_eq!(rig.org.status().unwrap(), before + 1);
}

// ---------------------------------------------------------------------------
// Settlement against the policy: holds block, a proven no-effect retries
// ---------------------------------------------------------------------------

/// One whole attempt whose Enter is sent and never confirmed, optionally
/// with other activity in the session while it waits (a prompt event that
/// is not the reference: the hook for ours was dropped). Returns the rig
/// after the attempt ended at the confirmation window.
async fn unconfirmed_enter(activity: bool) -> (Rig, MessageId) {
    let mut rig = Rig::attempt().await;
    let id = rig.ready_message().await;
    rig.script(&[pi(IDLE), pi(IDLE), staged(id), staged(id)]);
    rig.script(&[showing(&inbox_reference(id))]);
    let attempt = rig.spawn_deliver(id);
    rig.audited().await;
    if activity {
        rig.prompt("s1", "something else the session was told".into());
    }
    guarded(attempt).await.unwrap().unwrap();
    assert!(rig.typed(id), "{:?}", rig.effects());
    assert_eq!(rig.reads(), 0);
    (rig, id)
}

/// After an unconfirmed Enter the policy blocks the message for as long as
/// nobody resolves it, and an executor run for it types nothing and writes
/// nothing: no failure, no retry, no second paste.
async fn stays_blocked(rig: &mut Rig, id: MessageId) {
    let blocked = DeliveryStep::Blocked {
        reason: FailureReason::ProbablyDelivered,
    };
    let before = rig.org.status().unwrap();
    for _ in 0..2 {
        assert_eq!(rig.step(id), blocked);
        // The executor's one capture; it must not ask for a paste guard.
        rig.script(&[pi(IDLE)]);
        rig.deliver(id).await.unwrap();
        assert!(rig.typed(id), "{:?}", rig.effects());
        assert_eq!(rig.reads(), 0);
        assert_eq!(rig.snapshot(id).message.status, DeliveryStatus::Delivering);
        tokio::time::advance(Duration::from_secs(600)).await;
    }
    rig.rt.control.flush_audits().await;
    guarded(rig.org.snapshot()).await.unwrap();
    assert_eq!(rig.org.status().unwrap(), before);
}

/// The Enter was applied; no receipt and no activity followed. Held with
/// the token as the reason, and blocked: never an automatic retry.
#[tokio::test(start_paused = true)]
async fn token_visible_after_enter_blocks_retry() {
    let (mut rig, id) = unconfirmed_enter(false).await;
    assert_eq!(rig.snapshot(id).hold, Some(ExecutorHold::TokenVisible));
    assert!(!rig.rt.evidence.snapshot(id).unwrap().probably_landed);
    stays_blocked(&mut rig, id).await;
}

/// The same with the session active while the attempt waited: the hook for
/// our prompt was dropped, the turn is visibly running. No second paste.
#[tokio::test(start_paused = true)]
async fn dropped_prompt_with_activity_blocks_second_paste() {
    let (mut rig, id) = unconfirmed_enter(true).await;
    assert!(rig.rt.evidence.snapshot(id).unwrap().probably_landed);
    stays_blocked(&mut rig, id).await;
}

/// The audit of the Enter cannot be written for a while (the organisation
/// writer is held). The attempt does not wait for it and types nothing
/// again; the record that lands later carries the time of the Enter, not
/// the time it was written.
#[tokio::test(start_paused = true)]
async fn audit_stall_or_overflow_never_repeats_input_and_preserves_time() {
    let mut rig = Rig::attempt().await;
    let id = rig.ready_message().await;
    rig.script(&[pi(IDLE), pi(IDLE), staged(id), staged(id)]);
    rig.script(&[showing(&inbox_reference(id))]);
    let attempt = rig.spawn_deliver(id);
    rig.pasted().await;
    rig.hold();
    // The next append is the audit: the Enter's call has just returned.
    let seq = rig.entered().await;
    let sent = rig.now();
    assert!(rig.typed(id), "{:?}", rig.effects());

    guarded(attempt).await.unwrap().unwrap();

    let ended = rig.now();
    assert!(rig.typed(id), "{:?}", rig.effects());
    assert_eq!(rig.snapshot(id).hold, Some(ExecutorHold::TokenVisible));
    rig.release();
    let audit = rig.audited().await;
    assert_eq!(audit.seq, seq);
    assert_eq!(audit.ts, sent);
    assert!(audit.ts < ended);
    assert_eq!(audit.body["action"]["outcome"], "ok");
}

/// Four attempts that each type nothing (the owner's client is on the pane
/// when the paste is guarded). The executor fails the first three for
/// their retries, which the policy times 1, 2 and 4 seconds later, and
/// leaves the fourth, the final one by the policy's count, to the policy.
/// Nothing is ever pasted.
#[tokio::test(start_paused = true)]
async fn proven_no_effect_retries_1_2_4_then_final() {
    let mut rig = Rig::attempt().await;
    let id = rig.ready_message().await;
    let before = rig.org.status().unwrap();
    for (attempt, delay) in [(1, Some(1)), (2, Some(2)), (3, Some(4)), (4, None)] {
        assert_eq!(rig.step(id), DeliveryStep::PasteNow { attempt });
        rig.script(&[pi(IDLE), focused()]);
        rig.deliver(id).await.unwrap();
        assert_eq!((rig.pastes(), rig.reads()), (0, 0), "attempt {attempt}");
        let snapshot = rig.snapshot(id);
        assert_eq!(snapshot.hold, None, "attempt {attempt}");
        let Some(delay) = delay else {
            // The final attempt: no failure from the executor.
            assert_eq!(snapshot.message.status, DeliveryStatus::Delivering);
            break;
        };
        assert_eq!(snapshot.message.status, DeliveryStatus::Queued);
        let step = rig.step(id);
        assert!(matches!(step, DeliveryStep::RetryAt { .. }), "{step:?}");
        // Not a moment before its delay has passed.
        tokio::time::advance(Duration::from_secs(delay) - Duration::from_millis(1)).await;
        let step = rig.step(id);
        assert!(matches!(step, DeliveryStep::RetryAt { .. }), "{step:?}");
        tokio::time::advance(Duration::from_millis(1)).await;
    }
    // Four attempts and three failures were written, and no fourth failure.
    guarded(rig.org.snapshot()).await.unwrap();
    assert_eq!(rig.org.status().unwrap(), before + 7);
    tokio::time::advance(Duration::from_secs(3)).await;
    let failed = DeliveryStep::Fail {
        reason: FailureReason::ConfirmationTimeout,
    };
    assert_eq!(rig.step(id), failed);
}

/// Attempt 1 was armed, typed nothing and was failed for its retry. Before
/// the retry the session reports the exact reference for attempt 1's scope
/// (the owner pasted and submitted it himself, say). The next run of the
/// executor finds the receipt: no attempt 2, no paste, delivered.
#[tokio::test(start_paused = true)]
async fn receipt_between_failure_and_retry_cancels() {
    let mut rig = Rig::attempt().await;
    let id = rig.ready_message().await;
    rig.script(&[pi(IDLE), focused()]);
    rig.deliver(id).await.unwrap();
    assert_eq!(rig.snapshot(id).message.status, DeliveryStatus::Queued);
    let before = rig.org.status().unwrap();
    rig.prompt("s1", inbox_reference(id));
    tokio::time::advance(Duration::from_secs(1)).await;

    // The executor's one capture: a paste guard would find no frame.
    rig.script(&[pi(IDLE)]);
    rig.deliver(id).await.unwrap();

    assert!(rig.effects().is_empty(), "{:?}", rig.effects());
    assert_eq!(rig.reads(), 0);
    let snapshot = rig.snapshot(id);
    assert_eq!(snapshot.message.status, DeliveryStatus::Delivered);
    assert_eq!(snapshot.message.last_attempt.map(|a| a.attempt), Some(1));
    // Only `Delivered` was written.
    guarded(rig.org.snapshot()).await.unwrap();
    assert_eq!(rig.org.status().unwrap(), before + 1);
}

// ---------------------------------------------------------------------------
// Activity after an attempt that typed nothing
// ---------------------------------------------------------------------------

/// The message's first attempt is armed and types nothing (the owner's
/// client is on the pane at the paste guard), and the session is active
/// around it: its own turn started, which is why the latch is set although
/// nothing of ours was typed. The executor records the paste as not
/// started, and history then discounts the latch: the message stays
/// retryable. In both orders: the activity before that record, and after.
#[tokio::test(start_paused = true)]
async fn activity_after_zero_effect_failure_stays_retryable() {
    for early in [true, false] {
        let mut rig = Rig::attempt().await;
        let id = rig.ready_message().await;
        rig.script(&[pi(IDLE), focused()]);
        let armed = rig.wakes();
        let attempt = rig.spawn_deliver(id);
        let other = || "something else the session was told".to_string();
        if early {
            // Armed, and parked before the paste guard's paced capture.
            until("the attempt was armed", || rig.wakes() != armed).await;
            rig.prompt("s1", other());
        }
        guarded(attempt).await.unwrap().unwrap();
        if !early {
            rig.prompt("s1", other());
        }

        assert!(rig.rt.evidence.snapshot(id).unwrap().probably_landed);
        rig.copy_evidence(id);
        assert_eq!((rig.pastes(), rig.reads()), (0, 0), "early: {early}");
        assert_eq!(rig.snapshot(id).message.status, DeliveryStatus::Queued);
        tokio::time::advance(Duration::from_secs(1)).await;
        let step = rig.step(id);
        assert_eq!(
            step,
            DeliveryStep::PasteNow { attempt: 2 },
            "early: {early}"
        );
    }
}

/// The same activity when an earlier attempt of the message was applied, or
/// has an unknown outcome: that paste may be what the session is working
/// on, so the latch stays in force and the message is blocked, although the
/// current attempt typed nothing. An exact receipt still wins.
#[tokio::test(start_paused = true)]
async fn prior_applied_or_unknown_attempt_keeps_activity_latch() {
    for prior in [APPLIED, UNKNOWN] {
        let mut rig = Rig::attempt().await;
        let id = rig.ready_message().await;
        // Attempt 1: the paste lands and tmux fails the first look
        // (applied), or tmux reports a failure for the paste itself.
        rig.script(&[pi(IDLE), pi(IDLE)]);
        if prior == UNKNOWN {
            rig.tmux_gate
                .fail_next_after_effect(Effect::PasteText)
                .unwrap();
        }
        rig.deliver(id).await.unwrap_err();
        let first = rig.snapshot(id);
        assert_eq!(first.hold, Some(ExecutorHold::InputOutcomeUnknown));
        assert_eq!(rig.pastes(), 1, "{prior:?}");

        // The owner retries, accepting the risk: the recipient's job
        // releases the latch and fails the attempt for its retry. The
        // editor is empty and quiet again by then.
        let held = first.last_attempt_seq.unwrap();
        rig.rt.evidence.release(id, held).unwrap();
        let retry = HistoryUpdate::OwnerRetry {
            id,
            attempt_seq: held,
        };
        rig.rt.history.record(retry, rig.now()).unwrap();
        let failed = rig
            .commit(OrgCommand::DeliveryFailed {
                message_id: id,
                reason: "retried by the owner".into(),
                final_failure: false,
            })
            .await;
        rig.folded(failed).await;
        let observed = HistoryUpdate::Observed {
            recipient: worker(),
            observation: idle_observation(),
        };
        rig.rt.history.record(observed, rig.now()).unwrap();
        tokio::time::advance(Duration::from_secs(10)).await;
        assert_eq!(rig.step(id), DeliveryStep::PasteNow { attempt: 2 });

        // Attempt 2 types nothing, and the session is active after it.
        rig.script(&[pi(IDLE), focused()]);
        rig.deliver(id).await.unwrap();
        assert_eq!((rig.pastes(), rig.reads()), (1, 0), "{prior:?}");
        assert_eq!(rig.snapshot(id).message.status, DeliveryStatus::Queued);
        rig.prompt("s1", "something else the session was told".into());
        rig.copy_evidence(id);
        tokio::time::advance(Duration::from_secs(2)).await;
        let blocked = DeliveryStep::Blocked {
            reason: FailureReason::ProbablyDelivered,
        };
        assert_eq!(rig.step(id), blocked, "{prior:?}");

        rig.prompt("s1", inbox_reference(id));
        rig.copy_evidence(id);
        let step = rig.step(id);
        assert!(matches!(step, DeliveryStep::Confirm { .. }), "{step:?}");
    }
}

// ---------------------------------------------------------------------------
// Cleanup under a stalled or lost organisation write, and zero-effect exits
// ---------------------------------------------------------------------------

/// A reservation of the worker and its pane that acquires and returns.
async fn acquires(rig: &Rig) -> Result<(), ReservationError> {
    let acquired = rig.rt.control.run_reserved(
        rig.reservation(),
        |_| async { Ok::<_, ReservationError>(()) },
        no_cleanup,
    );
    guarded(acquired).await
}

/// The attempt typed nothing and its failure cannot be written: the
/// organisation writer stalls for longer than cleanup may wait. Cleanup
/// retains a hold instead and succeeds, so the agent is not quarantined and
/// a later reservation acquires while the writer still stalls. The hold
/// stands when the failure lands after all: no retry is granted.
#[tokio::test(start_paused = true)]
async fn org_stall_in_cleanup_holds_without_quarantine() {
    let mut rig = Rig::attempt().await;
    let id = rig.ready_message().await;
    rig.script(&[pi(IDLE), focused()]);
    let (before, armed) = (rig.org.status().unwrap(), rig.wakes());
    let attempt = rig.spawn_deliver(id);
    // The attempt is committed and armed; every later append stalls.
    until("the attempt was armed", || rig.wakes() != armed).await;
    rig.hold();

    guarded(attempt).await.unwrap().unwrap();

    assert_eq!((rig.pastes(), rig.reads()), (0, 0));
    assert_eq!(rig.org.status().unwrap(), before + 1);
    let snapshot = rig.snapshot(id);
    assert_eq!(snapshot.message.status, DeliveryStatus::Delivering);
    assert_eq!(snapshot.hold, Some(ExecutorHold::InputOutcomeUnknown));
    acquires(&rig).await.unwrap();

    rig.release();
    rig.folded(before + 2).await;
    let snapshot = rig.snapshot(id);
    assert_eq!(snapshot.message.status, DeliveryStatus::Queued);
    assert_eq!(snapshot.hold, Some(ExecutorHold::InputOutcomeUnknown));
    tokio::time::advance(Duration::from_secs(60)).await;
    let step = rig.step(id);
    assert!(matches!(step, DeliveryStep::Blocked { .. }), "{step:?}");
}

/// One recipient's attempt waits on a stalled append. Control work for
/// another agent and pane is not behind it.
#[tokio::test(start_paused = true)]
async fn stalled_append_does_not_block_other_recipient_control() {
    let mut rig = Rig::attempt().await;
    let id = rig.ready_message().await;
    rig.script(&[pi(IDLE), focused()]);
    let before = rig.org.status().unwrap();
    rig.hold();
    let attempt = rig.spawn_deliver(id);
    assert_eq!(rig.entered().await, before + 1);

    let other = ReservationRequest::new(
        "demo/other".parse().unwrap(),
        Some(PaneId::parse("%2").unwrap()),
        ReservationKind::Observe,
        WaitPolicy::Wait,
        Instant::now() + ATTEMPT_WORK_BUDGET,
    );
    let ran = rig.rt.control.run_reserved(
        other,
        |_| async { Ok::<_, ReservationError>(()) },
        no_cleanup,
    );
    guarded(ran).await.unwrap();
    assert!(!attempt.is_finished());
    assert_eq!(rig.org.status().unwrap(), before);

    rig.release();
    guarded(attempt).await.unwrap().unwrap();
    assert_eq!(rig.snapshot(id).message.status, DeliveryStatus::Queued);
}

/// The second attempt of a message is committed and never folded. History
/// knows the message's first attempt only, so nothing can be recorded or
/// held under the second: cleanup fails, and the agent is quarantined.
#[tokio::test(start_paused = true)]
async fn cleanup_failure_quarantines() {
    let mut rig = Rig::hand().await;
    let id = rig.ready_message().await;
    // Attempt 1 types nothing and is failed for its retry, folded by hand.
    rig.script(&[pi(IDLE), focused()]);
    let first = rig.spawn_deliver(id);
    let attempted = rig.committed().await;
    rig.apply(&attempted);
    let failed = rig.committed().await;
    assert_eq!(failed.kind, "msg.delivery_failed");
    rig.apply(&failed);
    guarded(first).await.unwrap().unwrap();
    tokio::time::advance(Duration::from_secs(1)).await;
    assert_eq!(rig.step(id), DeliveryStep::PasteNow { attempt: 2 });

    rig.script(&[pi(IDLE), pi(IDLE)]);
    let second = rig.spawn_deliver(id);
    let attempted = rig.committed().await;
    assert_eq!(attempted.kind, "msg.delivery_attempted");

    // Work waits for the fold until its deadline, cleanup until its own.
    let delivered = guarded(second).await.unwrap();

    assert!(
        matches!(
            delivered,
            Err(MessageError::Reservation(ReservationError::Timeout))
        ),
        "{delivered:?}"
    );
    assert_eq!((rig.pastes(), rig.reads()), (0, 3));
    // Nothing was written or held for the attempt nobody could account for.
    guarded(rig.org.snapshot()).await.unwrap();
    assert_eq!(rig.org.status().unwrap(), attempted.seq);
    assert_eq!(rig.snapshot(id).hold, None);
    let refused = acquires(&rig).await.unwrap_err();
    assert!(
        matches!(refused, ReservationError::Quarantined { .. }),
        "{refused:?}"
    );
}

/// Whoever asked for the attempt goes away while its commit is on its way.
/// The control supervisor owns the attempt: it still runs and settles.
#[tokio::test(start_paused = true)]
async fn client_gone_still_settles() {
    let mut rig = Rig::attempt().await;
    let id = rig.ready_message().await;
    rig.script(&[pi(IDLE), focused()]);
    let before = rig.org.status().unwrap();
    rig.hold();
    let attempt = rig.spawn_deliver(id);
    assert_eq!(rig.entered().await, before + 1);
    attempt.abort();
    assert!(guarded(attempt).await.unwrap_err().is_cancelled());
    rig.release();

    // The attempt, then its failure for a retry: it typed nothing.
    rig.folded(before + 2).await;
    let snapshot = rig.snapshot(id);
    assert_eq!(snapshot.message.status, DeliveryStatus::Queued);
    assert_eq!((snapshot.hold, rig.pastes()), (None, 0));
    acquires(&rig).await.unwrap();
}

/// Shutdown begins after the attempt's commit and before its paste: no
/// input, and cleanup still accounts for the attempt by failing it.
#[tokio::test(start_paused = true)]
async fn cancel_before_effect_is_zero_effect() {
    let mut rig = Rig::hand().await;
    let id = rig.ready_message().await;
    rig.script(&[pi(IDLE), pi(IDLE)]);
    let before = rig.org.status().unwrap();
    rig.hold();
    let attempt = rig.spawn_deliver(id);
    assert_eq!(rig.entered().await, before + 1);
    rig.rt.cancel.cancel();
    rig.release();
    rig.fold_rest();

    let delivered = guarded(attempt).await.unwrap();

    assert!(
        matches!(delivered, Err(MessageError::Unavailable(_))),
        "{delivered:?}"
    );
    // The paste guard's frame was never read.
    assert_eq!((rig.pastes(), rig.reads()), (0, 3));
    let snapshot = rig.snapshot(id);
    assert_eq!(snapshot.last_attempt_seq, Some(before + 1));
    assert_eq!(snapshot.message.status, DeliveryStatus::Queued);
    assert_eq!(snapshot.hold, None);
}

/// The work deadline passes while the attempt waits for its own fold. The
/// fold arrives during cleanup: the attempt is known, typed nothing, and is
/// failed for a retry.
#[tokio::test(start_paused = true)]
async fn timeout_before_effect_accounts_attempt() {
    let mut rig = Rig::hand().await;
    let id = rig.ready_message().await;
    rig.script(&[pi(IDLE), pi(IDLE)]);
    let deadline = Instant::now() + Duration::from_secs(1);
    let attempt = tokio::spawn(deliver(rig.rt.clone(), id, deadline));
    let attempted = rig.committed().await;
    assert_eq!(attempted.kind, "msg.delivery_attempted");
    // Past the work deadline, inside cleanup's wait for the fold.
    tokio::time::advance(Duration::from_secs(2)).await;
    rig.apply(&attempted);
    rig.fold_rest();

    let delivered = guarded(attempt).await.unwrap();

    assert!(
        matches!(
            delivered,
            Err(MessageError::Reservation(ReservationError::Timeout))
        ),
        "{delivered:?}"
    );
    assert_eq!((rig.pastes(), rig.reads()), (0, 3));
    let snapshot = rig.snapshot(id);
    assert_eq!(snapshot.message.status, DeliveryStatus::Queued);
    assert_eq!(snapshot.hold, None);
    acquires(&rig).await.unwrap();
}

// ---------------------------------------------------------------------------
// The guard between the staged draft and the Enter, through the executor
// ---------------------------------------------------------------------------
//
// One driver: the first look after the paste shows the staged reference and
// allows the Enter; the Enter guard's own capture, every later look and
// cleanup's capture show the case's frame. Each case must end with one
// paste, no key, the message still Delivering, and a hold.

const NOT_SENT: ExecutorHold = ExecutorHold::EnterNotSent;
const NO_TOKEN: ExecutorHold = ExecutorHold::InputOutcomeUnknown;

async fn no_key(case: &str, frame: fn(MessageId) -> PaneCapture, hold: ExecutorHold) {
    let mut rig = Rig::attempt().await;
    let id = rig.ready_message().await;
    rig.script(&[pi(IDLE), pi(IDLE), staged(id)]);
    // More frames than the confirmation window has room for.
    let frames = 40;
    rig.script(&vec![frame(id); frames]);
    let before = rig.org.status().unwrap();

    let _ = rig.deliver(id).await;

    assert_eq!((rig.pastes(), rig.keys()), (1, 0), "{case}");
    // It read the case's frame at least twice (the guard or a look, and
    // cleanup), and stopped.
    let left = rig.reads() / 3;
    assert!(left > 0 && left <= frames - 2, "{case}: {left} frames left");
    let snapshot = rig.snapshot(id);
    assert_eq!(
        snapshot.message.status,
        DeliveryStatus::Delivering,
        "{case}"
    );
    assert_eq!(snapshot.hold, Some(hold), "{case}");
    // No failure: at most the audit of an Enter call that sent nothing.
    rig.rt.control.flush_audits().await;
    guarded(rig.org.snapshot()).await.unwrap();
    while let Ok(envelope) = rig.committed.try_recv() {
        let ours = envelope.seq > before && envelope.kind != "control.sent";
        assert!(!ours || envelope.kind == "msg.delivery_attempted", "{case}");
    }
}

macro_rules! no_key_tests {
    ($($name:ident: $hold:expr, $frame:expr;)*) => {$(
        #[tokio::test(start_paused = true)]
        async fn $name() {
            no_key(stringify!($name), $frame, $hold).await;
        }
    )*};
}

no_key_tests! {
    // The owner's client comes to the pane after the draft was seen. The
    // draft matches exactly, so the hold names the token, not a mismatch.
    focus_changes_before_guard_no_input: NOT_SENT,
        |id| with(staged(id), |capture| capture.focused = Some(true));
    exact_draft_guard_refusal_holds_token_visible: NOT_SENT,
        |id| with(staged(id), |capture| capture.inspection.synchronized = true);
    // tmux could not say whether a client is on the pane.
    unknown_focus_blocks: NOT_SENT,
        |id| with(staged(id), |capture| capture.focused = None);
    // A shell took the foreground: every screen fact is unknown.
    shell_before_enter_no_key: NOT_SENT,
        |id| with(staged(id), |capture| {
            capture.inspection.foreground_command = Some("zsh".into());
        });
    // Leading whitespace is part of the draft.
    leading_space_reference_no_enter: NOT_SENT,
        |id| drafted(&format!(" {}", inbox_reference(id)));
    // A draft that is not the reference at all.
    unknown_or_wrapped_draft_no_enter: NO_TOKEN,
        |_| drafted("something the owner is typing");
    mismatched_or_unknown_staged_frame_never_blind_enter: NOT_SENT,
        |id| drafted(&format!("{} and more", inbox_reference(id)));
    // The reference in the transcript above an empty editor.
    same_reference_in_history_is_not_staged_proof: NOT_SENT,
        |id| showing(&inbox_reference(id));
    // A real pi 1.5.0 frame with its slash-command popup open.
    modal_before_enter_no_key: NO_TOKEN, |_| pi(POPUP);
}

/// A real pi editor with the slash-command popup open (the adapter's
/// fixture README has its `Captured:` line).
const POPUP: &str = include_str!(
    "../../../atm-pi-adapter/tests/fixtures/screen_p5_120x40-typed-popup_1.5.0.styled.txt"
);

/// Any client that is on the pane blocks, also beside one that is not: the
/// capture's focus sample is "some client is focused here".
#[tokio::test(start_paused = true)]
async fn any_focused_client_blocks() {
    let frame = |id| with(staged(id), |capture| capture.focused = Some(true));
    no_key("the Enter guard", frame, NOT_SENT).await;
    // And when the client is there from the first capture on: no attempt.
    // History's last observation, from before the reservation, said
    // unfocused. The executor records its own capture and the policy
    // decides on that: a wait, answered Ok, and not a refused paste guard.
    let mut rig = Rig::attempt().await;
    let id = rig.ready_message().await;
    rig.script(&[focused(), focused()]);
    rig.deliver(id).await.unwrap();
    assert!(rig.effects().is_empty(), "{:?}", rig.effects());
    assert_eq!(
        (rig.reads(), rig.snapshot(id).message.last_attempt),
        (3, None)
    );
    let step = rig.step(id);
    let focused = WaitReason::OwnerFocused;
    assert!(
        matches!(step, DeliveryStep::Wait { reason, .. } if reason == focused),
        "{step:?}"
    );
}

/// Copy mode on the pane, and another agent's tag on it: no key for either.
/// The tag is refused by the control actor itself, which ends the keys.
#[tokio::test(start_paused = true)]
async fn copy_mode_and_foreign_tag_no_input() {
    let mode = |id| with(staged(id), |capture| capture.inspection.in_mode = true);
    no_key("copy mode", mode, NOT_SENT).await;
    let tag = |id| {
        with(staged(id), |capture| {
            capture.inspection.local_agent_tag = Some("demo/other".into());
        })
    };
    no_key("a foreign tag", tag, NOT_SENT).await;
}

/// The whole attempt with `change` applied to every frame it reads; whether
/// the paste and the Enter were sent.
async fn typed_with(change: fn(&mut PaneCapture)) -> bool {
    let mut rig = Rig::attempt().await;
    let id = rig.ready_message().await;
    let (idle, draft) = (with(pi(IDLE), change), with(staged(id), change));
    rig.script(&[idle.clone(), idle, draft.clone(), draft.clone(), draft]);
    let _ = rig.deliver(id).await;
    rig.typed(id)
}

/// A client that has the pane selected and is not focused on it does not
/// block: focus is `Some(false)`, and the attempt types.
#[tokio::test(start_paused = true)]
async fn unfocused_selected_client_allows() {
    assert!(typed_with(|capture| capture.focused = Some(false)).await);
}

/// tmux before 3.7 does not report the bracketed-paste flag: allowed, for
/// the one printable ASCII line the reference is. A reported "off" refuses
/// the Enter as it refuses the paste.
#[tokio::test(start_paused = true)]
async fn bracket_unknown_ascii_allowed_false_refused() {
    assert!(typed_with(|capture| capture.inspection.bracketed_paste = None).await);
    let off = |id| {
        with(staged(id), |capture| {
            capture.inspection.bracketed_paste = Some(false);
        })
    };
    no_key("bracketed paste is off", off, NOT_SENT).await;
}

/// A capture in another format than the harness's adapter reads proves
/// nothing: every screen fact is unknown and neither input is allowed.
#[test]
fn capture_format_mismatch_no_input() {
    let id = MessageId::new(5);
    let (screen, reference) = (agent_screen(Harness::Pi).unwrap(), inbox_reference(id));
    for (input, frame) in [
        (GuardedInput::Paste, pi(IDLE)),
        (GuardedInput::Enter, staged(id)),
    ] {
        let facts = guard_facts(input, &frame, screen, &baseline(), &reference);
        assert!(guard_allows_input(&facts), "{facts:?}");
        let plain = with(frame, |capture| capture.format = CaptureFormat::Plain);
        let facts = guard_facts(input, &plain, screen, &baseline(), &reference);
        assert_eq!(facts.screen, ScreenFacts::default());
        assert!(!guard_allows_input(&facts), "{facts:?}");
    }
}

/// Real Claude 2.1.325 folder-trust dialogs, with "No, exit" selected (the
/// default) and with "Yes" selected (the adapter's fixture README has the
/// `Captured:` lines). The control read names the dialog, and no delivery
/// input is allowed on it, whichever row is selected: the executor never
/// answers a dialog.
#[test]
fn trust_default_no_never_gets_a_key() {
    use crate::org::agent_screen_tests::capture as shot_of;
    let screen = agent_screen(Harness::ClaudeCode).unwrap();
    let baseline = (PaneBaseline::adopted(100).with_harness("claude".into())).unwrap();
    let reference = inbox_reference(MessageId::new(123));
    for (selected, fixture) in [
        (
            false,
            include_str!(
                "../../../atm-claude-adapter/tests/fixtures/screen_control_p1_A120-default-120x40_2.1.325.txt"
            ),
        ),
        (
            true,
            include_str!(
                "../../../atm-claude-adapter/tests/fixtures/screen_control_p1_A120-selected-120x40_2.1.325.txt"
            ),
        ),
    ] {
        let capture = shot_of(fixture, CaptureFormat::Ansi, Some(100), Some("claude"))
            .with_focus(Some(false));
        for input in [GuardedInput::Paste, GuardedInput::Enter] {
            let facts = guard_facts(input, &capture, screen, &baseline, &reference);
            assert!(
                matches!(&facts.control_dialog, ControlDialog::FolderTrust { selected_accept, .. } if *selected_accept == selected),
                "{facts:?}"
            );
            assert!(!guard_allows_input(&facts), "{facts:?}");
        }
    }
}

// ---------------------------------------------------------------------------
// The confirmation window and the inputs
// ---------------------------------------------------------------------------

/// The attempt's append takes longer than the confirmation window (and less
/// than the writer's reply deadline, so the commit succeeds). The window
/// started at the attempt's event and has ended before anything was typed:
/// no paste. An attempt that typed nothing is failed for its retry, where a
/// paste now could only be left staged in the editor.
#[tokio::test(start_paused = true)]
async fn no_paste_once_the_confirmation_window_has_ended() {
    let mut rig = Rig::attempt().await;
    let id = rig.ready_message().await;
    // The first capture; a paste guard and a look nobody may ask for.
    rig.script(&[pi(IDLE), pi(IDLE), staged(id)]);
    let before = rig.org.status().unwrap();
    rig.hold();
    let attempt = rig.spawn_deliver(id);
    assert_eq!(rig.entered().await, before + 1);
    let window = rig.rt.policy.settings().confirmation_timeout;
    tokio::time::advance(window).await;
    rig.release();

    guarded(attempt).await.unwrap().unwrap();

    assert!(rig.effects().is_empty(), "{:?}", rig.effects());
    assert_eq!(rig.reads(), 2 * 3);
    let snapshot = rig.snapshot(id);
    assert_eq!(snapshot.last_attempt_seq, Some(before + 1));
    assert_eq!(snapshot.message.status, DeliveryStatus::Queued);
    assert_eq!(snapshot.hold, None);
    // The attempt and its failure for the retry.
    guarded(rig.org.snapshot()).await.unwrap();
    assert_eq!(rig.org.status().unwrap(), before + 2);
}

/// The look the executor is waiting for comes back after the window has
/// ended, and shows the staged reference: the Enter still goes out, behind
/// its guard like any other. Nothing waits for its receipt any more, so
/// the attempt is held. The session's prompt event arrives late and still
/// delivers the message, as attempt 1 and with no second paste.
///
/// Refusing the key at the bound would leave the reference staged in the
/// editor and the queue behind it waiting for the owner.
#[tokio::test(start_paused = true)]
async fn enter_after_the_window_is_sent_and_a_late_receipt_delivers() {
    let mut rig = Rig::attempt().await;
    let id = rig.ready_message().await;
    rig.script(&[pi(IDLE), pi(IDLE), staged(id), staged(id)]);
    rig.script(&[showing(&inbox_reference(id))]);
    let before = rig.org.status().unwrap();
    let attempt = rig.spawn_deliver(id);
    rig.pasted().await;
    // The executor is parked before its first look; the window ends.
    let window = rig.rt.policy.settings().confirmation_timeout;
    tokio::time::advance(window).await;

    guarded(attempt).await.unwrap().unwrap();

    assert!(rig.typed(id), "{:?}", rig.effects());
    assert_eq!(rig.reads(), 0);
    let snapshot = rig.snapshot(id);
    assert_eq!(snapshot.message.status, DeliveryStatus::Delivering);
    assert_eq!(snapshot.hold, Some(ExecutorHold::TokenVisible));

    rig.prompt("s1", inbox_reference(id));
    // The executor's one capture: a paste guard would find no frame.
    rig.script(&[pi(IDLE)]);
    rig.deliver(id).await.unwrap();

    assert!(rig.typed(id), "{:?}", rig.effects());
    assert_eq!(rig.reads(), 0);
    let snapshot = rig.snapshot(id);
    assert_eq!(snapshot.message.status, DeliveryStatus::Delivered);
    assert_eq!(snapshot.message.last_attempt.map(|a| a.attempt), Some(1));
    // The attempt, the Enter's audit and the delivery.
    rig.rt.control.flush_audits().await;
    guarded(rig.org.snapshot()).await.unwrap();
    assert_eq!(rig.org.status().unwrap(), before + 3);
}
