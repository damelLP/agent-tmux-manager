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
        message_evidence::MessageEvidence,
        message_history::MessageHistoryHandle,
        message_service_tests::{guarded, utc, worker},
        messages::{spawn_service_for_test, MessageClock, MessageHandle},
        OrgHandle, Request,
    },
};
use atm_core::{DialogState, InputState, TurnState};
use atm_org::{
    delivery::DeliveryPolicy, Address, AgentId, MessageContent, MessageKind, OrgCommand, OrgState,
};
use atm_tmux::{mock::MockCall, PaneInfo, TmuxError};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use tokio::sync::{broadcast, mpsc, watch, Semaphore};
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
    }
}
const VARIANTS: usize = 12;

fn control_errors() -> Vec<ControlError> {
    let pane = || "%1".to_string();
    vec![
        // Documented as before effect, and still Unknown here: the executor
        // never tells one `Control` from another.
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
    use Settled::{Held, Neither, Written};
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
        // The receipt stays retained until Delivered is folded: a stalled
        // organisation write is not a cleanup failure.
        (Deliver, Written, true),
        (Deliver, Held, true),
        (Deliver, Neither, true),
        // A zero-effect failure: written, or held in its place when the
        // organisation stalled. With neither, nothing vetoes a retry.
        (Fail, Written, true),
        (Fail, Held, true),
        (Fail, Neither, false),
        // No sequence to retain anything under.
        (Unaccountable, Written, false),
        (Unaccountable, Held, false),
        (Unaccountable, Neither, false),
    ];
    for hold in holds {
        // A retained hold is a successful cleanup, whatever could not be
        // persisted; an unretained one is the quarantine case. Nothing is
        // ever written for a hold, so "written" retains nothing.
        rows.push((Hold(hold), Held, true));
        rows.push((Hold(hold), Neither, false));
        rows.push((Hold(hold), Written, false));
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
// The rig
// ---------------------------------------------------------------------------

/// A real organisation writer whose appends stay in memory and pass a gate,
/// the service's fold loop as the one `Applied` writer, and the real control
/// actor over per-pane tmux scripts.
struct Rig {
    org: OrgHandle,
    rt: MessageRuntime,
    tmux: ScriptedTmuxClient,
    /// Keeps the service (the fold loop) alive; no scheduler runs in it.
    _messages: MessageHandle,
    /// Zero permits hold every append after it was entered.
    gate: Arc<Semaphore>,
    /// The sequence of each append, sent when it is entered.
    entered: mpsc::UnboundedReceiver<u64>,
    cancel: CancellationToken,
}

impl Rig {
    /// `enabled` false builds the runtime with disabled evidence and history.
    async fn build(enabled: bool) -> Self {
        let gate = Arc::new(Semaphore::new(1));
        let (enter, entered) = mpsc::unbounded_channel();
        let (requests, receive) = mpsc::channel::<Request>(64);
        let (health, status) = watch::channel(Ok(0));
        let (committed, _) = broadcast::channel(1024);
        let cancel = CancellationToken::new();
        let next = Arc::new(AtomicU64::new(0));
        let held = gate.clone();
        tokio::spawn(crate::org::actor::run(
            OrgState::default(),
            receive,
            health,
            committed.clone(),
            cancel.clone(),
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
        let registry = crate::registry::spawn_registry();
        let tmux = ScriptedTmuxClient::new();
        let (control, _task) = spawn_control(
            Box::new(tmux.clone()),
            registry.clone(),
            org.clone(),
            cancel.clone(),
        );
        let clock = MessageClock::new(Instant::now(), utc());
        let policy = DeliveryPolicy::default();
        let (evidence, history) = if enabled {
            (
                MessageEvidence::new(clock),
                MessageHistoryHandle::new(0, utc(), policy.clone()).unwrap(),
            )
        } else {
            (
                MessageEvidence::disabled(clock, "evidence is off"),
                MessageHistoryHandle::disabled("history is off"),
            )
        };
        let rt = MessageRuntime::new(
            org.clone(),
            registry,
            control,
            crate::org::agents::AgentHandle::disabled("no agent service in this rig yet"),
            evidence,
            history,
            policy,
            cancel.child_token(),
        );
        let (messages, _task) = spawn_service_for_test(rt.clone(), committed.subscribe(), 0, None);
        if enabled {
            guarded(messages.initialized()).await.unwrap();
        }
        Self {
            org,
            rt,
            tmux,
            _messages: messages,
            gate,
            entered,
            cancel,
        }
    }

    async fn new() -> Self {
        Self::build(true).await
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

    /// A project with `demo/worker` bound to session `s1` on %1; the last
    /// committed sequence, not yet folded.
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
    let rig = Rig::new().await;
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
    let mut rig = Rig::build(false).await;
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
    let mut rig = Rig::new().await;
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
