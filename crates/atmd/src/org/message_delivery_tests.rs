//! The executor's pure safety decisions as tables, and the rig its attempt
//! tests run on: an in-memory organisation writer behind an append gate, the
//! message service's own fold loop as the one `Applied` writer, and the real
//! control actor over per-pane tmux scripts. No scheduler runs here: a test is
//! the recipient slot and calls the executor itself.
//!
//! Synchronization is by returned calls, the append gate and the fold cursor.
//! Time is paused and only in-memory I/O runs under it.
#![allow(clippy::unwrap_used, clippy::expect_used)]

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
    }
}

#[test]
fn attempt_exit_disposition_table() {
    use AttemptCommit::{Absent, Committed, Unknown as Lost};
    use AttemptDisposition::{Deliver, Fail, Hold, NoAttempt, Unaccountable};
    let visible = Hold(ExecutorHold::TokenVisible);
    let unknown = Hold(ExecutorHold::InputOutcomeUnknown);

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
        // Applied paste: the staging bound without Enter, a refused Enter on
        // an exact draft, an applied Enter with no receipt at the
        // confirmation deadline (also one already past), a policy RetryAt or
        // final Fail, cancellation after the paste.
        (exit(Committed, false, APPLIED, Some(true)), visible),
        (exit(Committed, false, APPLIED, Some(false)), unknown),
        (exit(Committed, false, APPLIED, None), unknown),
        // The paste's own outcome is unknown: a timeout, a lost reply, work
        // aborted while it was dispatched.
        (exit(Committed, false, UNKNOWN, Some(true)), visible),
        (exit(Committed, false, UNKNOWN, Some(false)), unknown),
        (exit(Committed, false, UNKNOWN, None), unknown),
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
    for attempt in COMMITS {
        for paste in CERTAINTIES {
            for token in TOKENS {
                assert_eq!(disposition(&exit(attempt, true, paste, token)), Deliver);
                let facts = exit(attempt, false, paste, token);
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
                    let hold = if token == Some(true) {
                        visible
                    } else {
                        unknown
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
/// the real agent service with a pi agent on %1.
struct Rig {
    org: OrgHandle,
    rt: MessageRuntime,
    tmux: ScriptedTmuxClient,
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
        let gate = Arc::new(Semaphore::new(1));
        let (enter, entered) = mpsc::unbounded_channel();
        let (requests, receive) = mpsc::channel::<Request>(64);
        let (health, status) = watch::channel(Ok(0));
        let (committed, _) = broadcast::channel(1024);
        let cancel = CancellationToken::new();
        let org_stop = cancel.child_token();
        let next = Arc::new(AtomicU64::new(0));
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
        let (control, _task) = spawn_control(
            Box::new(tmux.clone()),
            registry.clone(),
            org.clone(),
            cancel.clone(),
        );
        let agents = if agent {
            let seed = [
                OrgCommand::RegisterProject {
                    project_id: "demo".parse().unwrap(),
                    root: "/demo".into(),
                    move_root: false,
                },
                OrgCommand::Up {
                    agent_id: worker(),
                    role: atm_org::parse_role("worker", "+++\nharness='pi'\n+++\n").unwrap(),
                },
                OrgCommand::PaneCreated {
                    agent_id: worker(),
                    pane: pane().to_string(),
                },
                OrgCommand::SessionBound {
                    agent_id: worker(),
                    session_id: SessionId::new("s1"),
                },
            ];
            for command in seed {
                org.execute(command, utc()).await.unwrap();
            }
            let mut row = SessionDomain::new(
                SessionId::new("s1"),
                AgentType::GeneralPurpose,
                Model::default(),
            );
            row.harness = Harness::Pi;
            row.tmux_pane = Some(pane().to_string());
            registry.register(row).await.unwrap();
            // The agent service's check of the pane when it starts.
            let idle = pi(IDLE);
            tmux.script_inspect(&pane(), Some(&idle.inspection))
                .unwrap();
            tmux.script_capture(&idle).unwrap();
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
            agents.record_baseline(worker(), "%1", baseline());
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
        let seq = self.commit(Rig::message(worker())).await;
        if self.messages.is_some() {
            self.folded(seq).await;
        } else {
            self.fold();
        }
        let observed = HistoryUpdate::Observed {
            recipient: worker(),
            observation: idle_observation(),
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
    rig.script(&[pi(IDLE), pi(IDLE), showing(&reference)]);
    let (before, armed) = (rig.org.status().unwrap(), rig.wakes());
    rig.hold();
    let attempt = rig.spawn_deliver(id);

    // The append is entered and held: nothing is committed, armed or typed.
    assert_eq!(rig.entered().await, before + 1);
    assert_eq!(rig.org.status().unwrap(), before);
    assert_eq!((rig.pastes(), rig.reads(), rig.wakes()), (0, 6, armed));

    // Committed, and not folded: still nothing.
    rig.release();
    let envelope = rig.committed().await;
    assert_eq!(
        (envelope.seq, envelope.kind.as_str()),
        (before + 1, "msg.delivery_attempted")
    );
    assert_eq!((rig.pastes(), rig.reads(), rig.wakes()), (0, 6, armed));

    // Folded: armed, and exactly one paste, of the reference alone.
    rig.apply(&envelope);
    guarded(attempt).await.unwrap().unwrap();
    assert_eq!(
        rig.effects(),
        [MockCall::PasteText {
            pane: "%1".into(),
            text: reference,
        }]
    );
    assert_ne!(rig.wakes(), armed);
    assert_eq!(rig.reads(), 0);
    // The attempt ends at its paste for now: held, the token as its reason,
    // and nothing written after the attempt.
    let snapshot = rig.snapshot(id);
    assert_eq!(snapshot.last_attempt_seq, Some(before + 1));
    assert_eq!(snapshot.hold, Some(ExecutorHold::TokenVisible));
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
/// whose first read fails. A possible effect is followed by cleanup's one
/// token capture; a refusal by none.
#[tokio::test(start_paused = true)]
async fn one_paste_guarded_call_per_deliver() {
    let foreign = with(pi(IDLE), |capture| {
        capture.inspection.local_agent_tag = Some("demo/other".into());
    });
    let cases = [
        // (the frames after the first capture, pastes, tmux calls, held)
        // The guard allows, the paste lands, cleanup looks for the token.
        ("applied", vec![pi(IDLE), pi(IDLE)], 1, 3 + 3 + 1 + 3, true),
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

        assert_eq!(
            delivered.is_ok(),
            case != "tmux failed",
            "{case}: {delivered:?}"
        );
        assert_eq!(rig.pastes(), pastes, "{case}");
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
