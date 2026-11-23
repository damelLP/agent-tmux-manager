//! The scheduler's production observer on the executor's rig: the real
//! control actor over per-pane tmux scripts and the real agent service, so
//! the observer and the executor are tested reading the same screens.
//!
//! A child of the executor's test module, for its rig and nothing else.
//!
//! Time is paused and only in-memory I/O runs under it; tests synchronize on
//! returned calls and on the executor's own entry and return.

use super::*;
use crate::org::{
    agents::Recipient,
    message_history::PanePresence,
    message_scheduler::{observed, Effects, UNCONFIRMED},
};
use std::sync::OnceLock;

/// Real pi frames. Each fixture's `Captured:` line is in the adapter's README.
const RUNNING: &str = include_str!(
    "../../../atm-pi-adapter/tests/fixtures/screen_styled_p1_C2-steer-one-held-120x40_1.5.0.txt"
);
const UNREADABLE: &str = include_str!(
    "../../../atm-pi-adapter/tests/fixtures/screen_styled_p1_R-state-001-120x40_1.5.0.txt"
);

// Captured: Claude Code 2.1.328, 2026-11-19, tmux 3.8 styled viewport
// at 217 columns, idle, physically resized. Generic cwd; trailing padding trimmed.
pub(super) fn claude_resize_frame(height: u16) -> PaneCapture {
    let text = match height {
        4 => include_str!("../../tests/fixtures/message_notice/claude_217x4_2.1.328.styled.txt"),
        6 => include_str!("../../tests/fixtures/message_notice/claude_217x6_2.1.328.styled.txt"),
        40 => include_str!("../../tests/fixtures/message_notice/claude_217x40_2.1.328.styled.txt"),
        _ => panic!("no Claude resize capture at {height} rows"),
    };
    with(pi(IDLE), |capture| {
        capture.text = text.into();
        // The archived file omits trailing blank rows; tmux supplies them.
        capture
            .text
            .push_str(&"\n".repeat(usize::from(height) - text.lines().count()));
        capture.inspection.pane.width = 217;
        capture.inspection.pane.height = height;
        capture.inspection.foreground_command = Some("claude".into());
    })
}

// Captured: pi 1.5.2, 2026-11-19, tmux 3.8 styled viewport at 217 columns,
// idle, physically resized. Generic cwd; trailing padding trimmed.
fn pi_resize_frame(height: u16) -> PaneCapture {
    let text = match height {
        5 => include_str!("../../tests/fixtures/message_notice/pi_217x5_1.5.2.styled.txt"),
        6 => include_str!("../../tests/fixtures/message_notice/pi_217x6_1.5.2.styled.txt"),
        _ => panic!("no pi resize capture at {height} rows"),
    };
    with(pi(IDLE), |capture| {
        capture.text = text.into();
        capture.inspection.pane.width = 217;
        capture.inspection.pane.height = height;
    })
}

// Captured: Claude Code 2.1.327, 2026-11-22, tmux capture-pane -p -e -N.
fn survey_frame() -> PaneCapture {
    let stored = include_str!(
        "../../../atm-claude-adapter/tests/fixtures/claude_feedback_survey_2.1.327.styled.txt"
    );
    let frame = stored
        .lines()
        .skip(1)
        .map(|row| row.strip_suffix('|').unwrap())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    assert_eq!(frame.lines().count(), 120);
    with(claude_resize_frame(40), |capture| {
        capture.text = frame;
        capture.inspection.pane.height = 120;
    })
}

fn survey_without_banner() -> PaneCapture {
    with(survey_frame(), |capture| {
        // Synthetic: the first four rows have scrolled out of view.
        capture.text = capture.text.split_inclusive('\n').skip(4).collect();
        capture.inspection.pane.height -= 4;
    })
}

fn scripted_survey_draft(frame: PaneCapture, id: MessageId, visible_survey: bool) -> PaneCapture {
    // Synthetic: put just the reference into the raw editor. A second frame
    // has the survey rows removed, as observed after a real bracketed paste.
    with(frame, |capture| {
        capture.text = capture
            .text
            .lines()
            .map(|row| {
                if !visible_survey
                    && (row.contains("How is Claude doing this session?")
                        || row.contains("0") && row.contains("Dismiss"))
                {
                    String::new()
                } else {
                    row.replace("❯\u{a0}", &format!("❯\u{a0}{}", inbox_reference(id)))
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
    })
}

#[tokio::test(start_paused = true)]
async fn inbox_pointers_deliver_through_survey_in_fifo_order() {
    assert_survey_fifo(survey_frame).await;
}

#[tokio::test(start_paused = true)]
async fn headerless_survey_delivers_in_fifo_order() {
    assert_survey_fifo(survey_without_banner).await;
}

async fn assert_survey_fifo(frame: fn() -> PaneCapture) {
    // Break: either the scheduler or the reserved executor refuses the exact
    // survey frame, or an Enter goes out before the ordinary editor returns.
    let production: Arc<OnceLock<(Effects, ScriptedTmuxClient)>> = Arc::default();
    let effects = Effects {
        deliver: Arc::new({
            let production = production.clone();
            move |rt, id, deadline| {
                let (real, tmux) = production.get().unwrap();
                let waiting = scripted_survey_draft(frame(), id, true);
                let ready = scripted_survey_draft(frame(), id, false);
                for shot in [
                    frame(),
                    frame(),
                    waiting.clone(),
                    waiting,
                    ready.clone(),
                    ready,
                ] {
                    tmux.script_capture(&shot).unwrap();
                }
                (real.deliver)(rt, id, deadline)
            }
        }),
        observe: Arc::new({
            let production = production.clone();
            move |agent, reference| {
                let (real, tmux) = production.get().unwrap();
                tmux.script_capture(&frame()).unwrap();
                (real.observe)(agent, reference)
            }
        }),
        retract: super::no_retract(),
    };
    let mut rig = Rig::start_with_harness(
        0,
        Fold::Service,
        true,
        DeliveryPolicy::new(DeliverySettings {
            readiness_timeout: Duration::from_secs(25),
            ..Default::default()
        })
        .unwrap(),
        Some(effects),
        Harness::ClaudeCode,
        UnreadableFixture::Plain,
    )
    .await;
    assert!(production
        .set((Effects::production(&rig.rt), rig.tmux.clone()))
        .is_ok());
    while rig.committed.try_recv().is_ok() {}
    let head = MessageId::new(rig.commit(Rig::message(worker())).await);
    let follower = MessageId::new(rig.commit(Rig::message(worker())).await);
    for (id, count) in [(head, 1), (follower, 2)] {
        let audit = guarded(async {
            loop {
                let event = rig.committed().await;
                if event.kind == "control.sent" {
                    break event;
                }
                if let Some(OrgEvent::MessageSent(content)) =
                    OrgEvent::from_parts(&event.kind, &event.body).unwrap()
                {
                    assert!(
                        content.delivery_notice.is_none(),
                        "inbox pointer was held: {}",
                        content.body
                    );
                }
            }
        })
        .await;
        assert_eq!(audit.kind, "control.sent");
        assert_eq!((rig.pastes(), rig.keys(), rig.reads()), (count, count, 0));
        let expected = inbox_reference(id);
        assert_eq!(
            &rig.effects()[count * 2 - 2..],
            &[
                MockCall::PasteText {
                    pane: "%1".into(),
                    text: expected.clone()
                },
                MockCall::SendKeys {
                    pane: "%1".into(),
                    keys: "Enter".into()
                },
            ]
        );
        assert_eq!(rig.snapshot(id).message.last_attempt.unwrap().attempt, 1);
        rig.rt
            .evidence
            .observe(
                &SessionId::new("s1"),
                Some("%1"),
                Harness::ClaudeCode,
                &LifecycleEvent::PromptSubmit {
                    prompt: Some(expected),
                },
            )
            .unwrap();
        let receipt = guarded(async {
            loop {
                let event = rig.committed().await;
                if event.kind == "msg.delivered" {
                    break event;
                }
                assert_ne!(
                    event.kind, "msg.delivery_attempted",
                    "the follower started before the receipt"
                );
            }
        })
        .await;
        assert_eq!(receipt.body["message_id"], id.to_string());
        rig.folded(receipt.seq).await;
        assert_eq!(rig.snapshot(id).message.status, DeliveryStatus::Delivered);
    }
}

#[test]
fn only_inbox_paste_with_positive_bracketed_mode_can_use_the_survey() {
    let screen = agent_screen(Harness::ClaudeCode).unwrap();
    let baseline = PaneBaseline::adopted(100)
        .with_harness("claude".into())
        .unwrap();
    let id = MessageId::new(42);
    let reference = inbox_reference(id);
    let frame = survey_frame();
    let allowed = |kind, capture: &PaneCapture| {
        guard_allows_input(&guard_facts(kind, capture, screen, &baseline, &reference))
    };
    assert!(allowed(GuardedInput::InboxPaste, &frame));
    assert!(!allowed(GuardedInput::Paste, &frame));
    assert!(!allowed(
        GuardedInput::Enter,
        &scripted_survey_draft(frame.clone(), id, true)
    ));
    for flag in [None, Some(false)] {
        let no_bracket = with(frame.clone(), |capture| {
            capture.inspection.bracketed_paste = flag
        });
        assert!(!allowed(GuardedInput::InboxPaste, &no_bracket));
    }
    let focused = frame.with_focus(Some(true));
    assert!(!allowed(GuardedInput::InboxPaste, &focused));
}

#[test]
fn headerless_survey_paste_requires_exact_choices() {
    let screen = agent_screen(Harness::ClaudeCode).unwrap();
    let baseline = PaneBaseline::adopted(100)
        .with_harness("claude".into())
        .unwrap();
    let id = MessageId::new(42);
    let reference = inbox_reference(id);
    let frame = survey_without_banner();
    let ordinary = screen.control(&frame, &baseline);
    assert_eq!(ordinary.dialog, ControlDialog::Unknown);
    assert_eq!(ordinary.facts.turn, TurnState::Unknown);

    let allowed = |kind, capture: &PaneCapture| {
        guard_allows_input(&guard_facts(kind, capture, screen, &baseline, &reference))
    };
    assert!(allowed(GuardedInput::InboxPaste, &frame));
    assert!(!allowed(GuardedInput::Paste, &frame));
    assert!(!allowed(
        GuardedInput::Enter,
        &scripted_survey_draft(frame.clone(), id, true)
    ));
    let partial = with(frame, |capture| {
        capture.text = capture.text.replace(": Dismiss", ": Dismis")
    });
    assert!(!allowed(GuardedInput::InboxPaste, &partial));
}

#[test]
fn busy_survey_footer_refuses_inbox_paste() {
    let screen = agent_screen(Harness::ClaudeCode).unwrap();
    let baseline = PaneBaseline::adopted(100)
        .with_harness("claude".into())
        .unwrap();
    let frame = survey_frame();
    let busy = with(frame.clone(), |capture| {
        capture.text = capture.text.replace(
            "(shift+tab to cycle)",
            "(shift+tab to cycle) · esc to interrupt",
        );
    });
    assert_ne!(
        busy.text, frame.text,
        "fixture must contain the idle footer"
    );
    assert!(!guard_allows_input(&guard_facts(
        GuardedInput::InboxPaste,
        &busy,
        screen,
        &baseline,
        &inbox_reference(MessageId::new(42)),
    )));
}

#[tokio::test(start_paused = true)]
async fn survey_readiness_does_not_admit_start_lines() {
    use atm_org::delivery::InputLine;
    let rig = Rig::start_with_harness(
        0,
        Fold::Service,
        true,
        DeliveryPolicy::default(),
        None,
        Harness::ClaudeCode,
        UnreadableFixture::Plain,
    )
    .await;
    let start = MessageId::new(
        rig.commit(OrgCommand::SendMessage(crate::org::start_line::start_line(
            &worker(),
        )))
        .await,
    );
    let pointer = MessageId::new(rig.commit(Rig::message(worker())).await);
    rig.folded(pointer.seq()).await;
    rig.script(&[survey_frame()]);
    let observation = observe(&rig).await.unwrap();
    rig.rt
        .history
        .record(
            HistoryUpdate::Observed {
                recipient: worker(),
                observation,
            },
            rig.now(),
        )
        .unwrap();
    assert!(matches!(
        rig.snapshot(pointer).live.input_line,
        InputLine::EmptySince(_)
    ));
    assert_eq!(rig.snapshot(start).live.input_line, InputLine::Unknown);
    assert_eq!((rig.pastes(), rig.keys()), (0, 0));
}

#[tokio::test(start_paused = true)]
async fn survey_is_rechecked_at_the_actor_paste() {
    let rig = Rig::start_with_harness(
        0,
        Fold::Service,
        true,
        DeliveryPolicy::default(),
        None,
        Harness::ClaudeCode,
        UnreadableFixture::Plain,
    )
    .await;
    let id = MessageId::new(rig.commit(Rig::message(worker())).await);
    rig.folded(id.seq()).await;
    let survey = survey_frame();
    rig.script(std::slice::from_ref(&survey));
    let observation = observe(&rig).await.unwrap();
    rig.rt
        .history
        .record(
            HistoryUpdate::Observed {
                recipient: worker(),
                observation,
            },
            rig.now(),
        )
        .unwrap();
    tokio::time::advance(rig.rt.policy.settings().stable_idle).await;
    assert_eq!(rig.step(id), DeliveryStep::PasteNow { attempt: 1 });

    // The executor's initial capture still reads the complete survey. The one
    // taken by the actor for the paste sees only incomplete choices.
    let partial = with(survey.clone(), |frame| {
        frame.text = frame.text.replace(": Dismiss", ": Dismis")
    });
    rig.script(&[survey, partial]);
    rig.deliver(id).await.unwrap();
    assert_eq!((rig.pastes(), rig.keys(), rig.reads()), (0, 0, 0));
    assert_eq!(rig.snapshot(id).message.last_attempt.unwrap().attempt, 1);
}

/// One call of the production observer for the worker.
async fn observe(rig: &Rig) -> Result<RecipientObservation, MessageError> {
    let state = guarded(rig.org.snapshot()).await.unwrap();
    let agent = state.agent(&worker()).cloned().unwrap();
    guarded((Effects::production(&rig.rt).observe)(agent, None))
        .await
        .map(|(seen, ..)| seen)
}

/// What the agent service answers for the worker: Ready, bound to `s1`,
/// which is the one eligible row on %1.
async fn recipient(rig: &Rig) -> Recipient {
    guarded(rig.rt.agents.recipient(&worker())).await.unwrap()
}

fn turn(observation: &RecipientObservation) -> TurnState {
    observation.facts.turn
}

#[tokio::test(start_paused = true)]
async fn each_call_reads_one_new_capture_and_keeps_nothing() {
    let rig = Rig::attempt().await;
    rig.script(&[pi(RUNNING), pi(IDLE), pi(UNREADABLE)]);
    let mut turns = Vec::new();
    for left in [6, 3, 0] {
        let seen = observe(&rig).await.unwrap();
        // The identity is the recipient's on every call.
        assert_eq!(seen.pane, Some(pane()));
        let row = seen.session.as_ref().unwrap();
        assert_eq!((row.id.as_str(), row.harness), ("s1", Harness::Pi));
        assert_eq!(seen.presence, PanePresence::Alive);
        assert_eq!(seen.focused, Some(false));
        assert_eq!(rig.reads(), left, "one capture per call");
        turns.push(turn(&seen));
    }
    // Running only on a running-turn read, NotRunning only on an idle one.
    assert_eq!(
        turns,
        [
            TurnState::Running,
            TurnState::NotRunning,
            TurnState::Unknown
        ]
    );
    // Nothing was kept: with no frame to read there is no observation.
    assert!(observe(&rig).await.is_err());
    assert_eq!(rig.pastes(), 0);
}

#[tokio::test(start_paused = true)]
async fn unreadable_screen_delivery_rearms_after_a_readable_busy_screen() {
    let mut rig = Rig::attempt().await;
    let diag = "atm cannot read demo/worker's screen (%1) for 120 s; pi may have changed its screen (version unknown). Delivery, readiness checks, clear, and handover need a readable screen.";
    for count in 1..=2 {
        rig.script(&[pi(UNREADABLE)]);
        assert_eq!(turn(&observe(&rig).await.unwrap()), TurnState::Unknown);
        let since = Instant::now();
        rig.script(&[pi(UNREADABLE)]); // the deadline's read, not the scheduler's
        tokio::time::advance(Duration::from_millis(119_999)).await;
        assert_eq!(screen_notes(&rig).await.len(), count - 1);
        assert!(Instant::now() < since + Duration::from_secs(120));
        assert_eq!(rig.reads(), 3);
        tokio::time::advance(Duration::from_millis(1)).await;
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let e = rig.committed.recv().await.unwrap();
                if e.kind == "msg.sent" {
                    break;
                }
            }
        })
        .await
        .expect("the diagnostic was not committed");
        assert_eq!(screen_notes(&rig).await, vec![diag; count]);
        rig.script(&[pi(UNREADABLE)]);
        assert_eq!(turn(&observe(&rig).await.unwrap()), TurnState::Unknown);
        tokio::time::advance(Duration::from_secs(3600)).await;
        assert_eq!(screen_notes(&rig).await.len(), count);
        assert_eq!(rig.reads(), 0);
        rig.script(&[pi(RUNNING)]);
        assert_eq!(turn(&observe(&rig).await.unwrap()), TurnState::Running);
        assert_eq!(screen_notes(&rig).await.len(), count, "no recovery note");
    }
    assert_eq!(rig.pastes(), 0);
}

#[tokio::test(start_paused = true)]
async fn unreadable_screen_scheduler_keeps_two_messages_queued_without_duplicate_notes() {
    use crate::org::message_service_tests::{owner, page};
    use atm_comms::message_types::DeliveryPhase;

    let production: Arc<OnceLock<Effects>> = Arc::default();
    let (observed, mut observations) = mpsc::unbounded_channel();
    let effects = Effects {
        deliver: Arc::new({
            let production = production.clone();
            move |rt, id, deadline| (production.get().unwrap().deliver)(rt, id, deadline)
        }),
        observe: Arc::new({
            let production = production.clone();
            move |agent, reference| {
                let read = (production.get().unwrap().observe)(agent, reference);
                let observed = observed.clone();
                Box::pin(async move {
                    let result = read.await;
                    let _ = observed.send(result.is_ok());
                    result
                })
            }
        }),
        retract: super::no_retract(),
    };
    let policy = DeliveryPolicy::new(DeliverySettings {
        readiness_timeout: Duration::from_secs(3),
        ..Default::default()
    })
    .unwrap();
    let rig = Rig::start(Fold::Service, true, policy, Some(effects)).await;
    assert!(production.set(Effects::production(&rig.rt)).is_ok());
    // The short-viewport diagnostic is already committed before the mail's
    // hold deadline, so a blocked-readiness notice would repeat the same cause.
    let clipped = pi_resize_frame(5);
    rig.script(&[clipped.clone(), clipped.clone()]);
    assert_eq!(turn(&observe(&rig).await.unwrap()), TurnState::Unknown);
    let mut events = rig.org.committed.subscribe();
    let note = guarded(async {
        loop {
            let event = events.recv().await.unwrap();
            if event.kind == "msg.sent" {
                break event;
            }
        }
    })
    .await;
    rig.folded(note.seq).await;
    rig.script(&vec![clipped; 24]);
    let first = MessageId::new(rig.commit(Rig::message(worker())).await);
    let follower = MessageId::new(rig.commit(Rig::message(worker())).await);
    assert_eq!(guarded(observations.recv()).await, Some(true));
    let inbox = || async {
        let mut params = page();
        params.recipient = Some(Address::Agent(worker()));
        rig.messages
            .as_ref()
            .unwrap()
            .inbox(params, owner())
            .await
            .unwrap()
    };
    let snapshot = guarded(async {
        loop {
            let result = inbox().await;
            if result
                .messages
                .iter()
                .any(|m| m.message_id == first && m.delivery.phase == DeliveryPhase::Blocked)
            {
                break result;
            }
            assert!(observations.recv().await.is_some());
        }
    })
    .await;
    assert!(snapshot
        .messages
        .iter()
        .all(|m| m.status == DeliveryStatus::Queued && m.delivery.attempt == 0));
    assert_eq!(
        snapshot
            .messages
            .iter()
            .find(|m| m.message_id == follower)
            .unwrap()
            .delivery
            .blocked_by,
        Some(first)
    );
    // The next successful read proves the scheduler finished evaluating
    // the blocked round before we inspect its committed notices.
    assert_eq!(guarded(observations.recv()).await, Some(true));
    assert_eq!(screen_notes(&rig).await, [
        "atm cannot read demo/worker's screen for 3 s; pane %1 was 217x5 on the last read; try making it taller so atm can verify the editor and turn state. Delivery, readiness checks, clear, and handover need a readable screen."
    ]);
    assert_eq!(rig.pastes(), 0);
}

// Break caught: short partial chrome produces a generic timeout instead of
// reporting the last trusted dimensions, or the hint changes paste policy.
#[tokio::test(start_paused = true)]
async fn short_viewport_explains_readiness_without_new_guards() {
    use crate::org::message_service_tests::{owner, page};
    use atm_comms::message_types::DeliveryPhase;

    for (height, width, copy_mode, hint, tag) in [
        (6, 217, false, true, "blocked.readiness_timeout"),
        (12, 217, false, true, "blocked.readiness_timeout"),
        (20, 217, false, false, "blocked.readiness_timeout"),
        (6, 0, false, false, "unobserved"), // rejected metadata is not a viewport
        (6, 217, true, false, "blocked.copy_mode"),
        (6, 217, false, true, "blocked.agent_not_ready_timeout"),
    ] {
        let production: Arc<OnceLock<Effects>> = Arc::default();
        let effects = Effects {
            deliver: Arc::new(|_, _, _| panic!("a partially read editor authorizes no input")),
            observe: Arc::new({
                let production = production.clone();
                move |agent, reference| (production.get().unwrap().observe)(agent, reference)
            }),
            retract: super::no_retract(),
        };
        let policy = DeliveryPolicy::new(DeliverySettings {
            readiness_timeout: Duration::from_secs(3),
            ..Default::default()
        })
        .unwrap();
        let rig = Rig::start_with_harness(
            0,
            Fold::Service,
            true,
            policy,
            Some(effects),
            Harness::ClaudeCode,
            UnreadableFixture::Plain,
        )
        .await;
        assert!(production.set(Effects::production(&rig.rt)).is_ok());
        let starting = tag == "blocked.agent_not_ready_timeout";
        if starting {
            // This case is about the readiness notice, not the revalidation
            // poll, whose background captures would land in the scripted
            // reads below in an unknown order (s3-stuckseat).
            rig.rt.agents.pause_revalidation();
            rig.commit(OrgCommand::Revalidate { agent_id: worker() })
                .await;
        }
        let frame = with(claude_resize_frame(6), |capture| {
            capture.inspection.pane.height = height;
            capture.inspection.pane.width = width;
            capture.inspection.in_mode = copy_mode;
            capture
                .text
                .push_str(&"\n".repeat(usize::from(height.saturating_sub(6))));
        });
        rig.script(&vec![frame; 24]);
        let mut events = rig.org.committed.subscribe();
        let id = MessageId::new(rig.commit(Rig::message(worker())).await);
        let content = guarded(async {
            loop {
                let event = events.recv().await.unwrap();
                if let Some(OrgEvent::MessageSent(content)) =
                    OrgEvent::from_parts(&event.kind, &event.body).unwrap()
                {
                    if content.from == Address::System {
                        break content;
                    }
                }
            }
        })
        .await;
        assert_eq!(content.to, Address::Owner);
        let key = content.delivery_notice.as_ref().unwrap();
        assert_eq!(
            (key.source_message_id, key.reason.as_str()),
            (id, tag),
            "{width}x{height}, copy mode={copy_mode}"
        );
        if hint {
            let reason = if starting {
                "recipient agent did not become ready before the readiness deadline"
            } else {
                "recipient readiness could not be established"
            };
            assert_eq!(content.body, format!("{id} blocked: {reason} for demo/worker; pane %1 was {width}x{height} on the last read; try making it taller so atm can verify the editor and turn state; waiting behind it: none. Delivery resumes when this clears; atm wake give-up {id} cancels it"));
        } else {
            assert!(
                !content.body.contains("making it taller"),
                "{height} rows, copy mode={copy_mode}: {}",
                content.body
            );
        }
        let mut query = page();
        query.recipient = Some(Address::Agent(worker()));
        let source = rig
            .messages
            .as_ref()
            .unwrap()
            .inbox(query, owner())
            .await
            .unwrap()
            .messages
            .remove(0);
        let phase = if tag == "unobserved" {
            DeliveryPhase::Waiting
        } else {
            DeliveryPhase::Blocked
        };
        assert_eq!(
            (
                source.status,
                source.delivery.phase,
                source.delivery.attempt
            ),
            (DeliveryStatus::Queued, phase, 0)
        );
        assert_eq!((rig.pastes(), rig.keys()), (0, 0));
    }
}

// Pin the known limit: readiness can diagnose a clipped frame before the
// unreadable confirmation for that episode. Both notes are informational.
#[tokio::test(start_paused = true)]
async fn readiness_first_sends_two_informational_notes_to_manager() {
    let production: Arc<OnceLock<Effects>> = Arc::default();
    let (seen, mut observations) = mpsc::unbounded_channel();
    let effects = Effects {
        deliver: Arc::new({
            let production = production.clone();
            move |rt, id, deadline| (production.get().unwrap().deliver)(rt, id, deadline)
        }),
        observe: Arc::new({
            let production = production.clone();
            move |agent, reference| {
                let read = (production.get().unwrap().observe)(agent, reference);
                let seen = seen.clone();
                Box::pin(async move {
                    let result = read.await;
                    let _ = seen.send(result.as_ref().ok().map(|(_, episode, _, _)| *episode));
                    result
                })
            }
        }),
        retract: super::no_retract(),
    };
    let policy = DeliveryPolicy::new(DeliverySettings {
        readiness_timeout: Duration::from_secs(3),
        guard_poll_interval: Duration::from_secs(1),
        ..Default::default()
    })
    .unwrap();
    let mut rig = Rig::start_with_harness(
        0,
        Fold::Service,
        true,
        policy,
        Some(effects),
        Harness::ClaudeCode,
        UnreadableFixture::RecordedManager,
    )
    .await;
    assert!(production.set(Effects::production(&rig.rt)).is_ok());

    // The partial read starts delivery's hold. The fully clipped read starts
    // the unreadable episode one guard poll later, without resetting the hold.
    let mut frames = vec![claude_resize_frame(6)];
    frames.extend(std::iter::repeat_n(claude_resize_frame(4), 12));
    rig.script(&frames);
    let mut events = rig.org.committed.subscribe();
    let id = MessageId::new(rig.commit(Rig::message(worker())).await);
    assert_eq!(guarded(observations.recv()).await, Some(Some(None)));
    let episode = guarded(observations.recv())
        .await
        .flatten()
        .flatten()
        .expect("clipped read starts the episode");
    let key = atm_org::DeliveryNotice {
        source_message_id: id,
        reason: "blocked.readiness_timeout".into(),
    };
    guarded(async {
        loop {
            let event = events.recv().await.unwrap();
            if matches!(OrgEvent::from_parts(&event.kind, &event.body).unwrap(),
                Some(OrgEvent::MessageSent(note)) if note.delivery_notice.as_ref() == Some(&key))
            {
                break;
            }
        }
    })
    .await;
    assert!(
        Instant::now() < episode + Duration::from_secs(3),
        "readiness published before the unreadable deadline"
    );
    let tracker = rig.rt.agents.screens().unwrap();
    guarded(tracker.wait_for_diagnosis_to_settle(&worker(), episode)).await;

    let manager: Address = "demo/lead".parse().unwrap();
    let state = guarded(rig.org.snapshot()).await.unwrap();
    let notes = state.inbox(&manager);
    let unreadable = MessageContent {
        delivery_notice: None, from: Address::System, to: manager.clone(),
        kind: MessageKind::Note, attachments: vec![], task: None,
        body: "atm cannot read demo/worker's screen for 3 s; pane %1 was 217x4 on the last read; try making it taller so atm can verify the editor and turn state. Delivery, readiness checks, clear, and handover need a readable screen.".into(),
        post: None,
    };
    let readiness = MessageContent {
        delivery_notice: Some(key),
        body: format!("{id} blocked: recipient readiness could not be established for demo/worker; pane %1 was 217x4 on the last read; try making it taller so atm can verify the editor and turn state; waiting behind it: none. Delivery resumes when this clears; the owner can cancel it with atm wake give-up {id}"),
        ..unreadable.clone()
    };
    assert_eq!(
        notes.iter().map(|note| &note.content).collect::<Vec<_>>(),
        [&readiness, &unreadable]
    );
    // The Down manager may generate its own blocked-mail breadcrumb, not
    // a second audience for either of the worker's notes.
    assert!(state.inbox(&Address::Owner).iter().all(|note| {
        note.content.delivery_notice != readiness.delivery_notice
            && note.content.body != unreadable.body
    }));
    assert!(notes
        .iter()
        .all(|m| m.status == DeliveryStatus::Queued && m.last_attempt.is_none()));
    let source = state.message(&id).unwrap();
    assert_eq!(source.status, DeliveryStatus::Queued);
    assert!(source.last_attempt.is_none());
    assert_eq!((rig.pastes(), rig.keys()), (0, 0));
    for event in std::iter::from_fn(|| rig.committed.try_recv().ok()) {
        assert!(!matches!(
            event.kind.as_str(),
            "control.sent" | "msg.delivery_attempted"
        ));
    }
}

// A readable editor with an unknown turn is not an all-Unknown screen
// episode, nor does it admit input even when the bound session is idle.
#[tokio::test(start_paused = true)]
async fn background_agent_list_ends_unreadable_episode_without_admitting_input() {
    // Captured: Claude Code not recorded, not recorded, styled tmux viewport (sanitized).
    let fixture = include_str!(
        "../../../atm-claude-adapter/tests/fixtures/claude_background_agent_list_version-not-recorded.styled.txt"
    );
    let frame = with(claude_resize_frame(40), |capture| {
        capture.text = fixture.split_once('\n').unwrap().1.into();
        capture.inspection.pane.width = 189;
    });
    // Synthetic: keep the viewport geometry but alter the footer anchor.
    let malformed = with(frame.clone(), |capture| {
        capture.text = capture.text.replace("↓ to manage", "↓ to manage extra");
    });
    let rig = Rig::start_with_harness(
        0,
        Fold::Service,
        true,
        DeliveryPolicy::default(),
        None,
        Harness::ClaudeCode,
        UnreadableFixture::Plain,
    )
    .await;
    let bound = recipient(&rig).await;
    assert_eq!(bound.status, AgentStatus::Ready);
    assert_eq!(bound.session.as_ref().unwrap().status, SessionStatus::Idle);
    let org = guarded(rig.org.snapshot()).await.unwrap();
    let agent = org.agent(&worker()).unwrap();
    let tracker = rig.rt.agents.screens().unwrap();
    for (shot, unreadable) in [(&frame, false), (&malformed, true), (&frame, false)] {
        rig.script(std::slice::from_ref(shot));
        let observed = observe(&rig).await.unwrap();
        assert_eq!(tracker.active(agent), unreadable, "{observed:?}");
        assert_eq!(turn(&observed), TurnState::Unknown);
        assert_eq!(
            observed.facts.input,
            if unreadable {
                InputState::Unknown
            } else {
                InputState::Empty
            }
        );
        assert!(!observed.inbox_paste);
        assert!(!guard_allows_input(&guard_facts(
            GuardedInput::InboxPaste,
            shot,
            bound.screen,
            &bound.baseline,
            &inbox_reference(MessageId::new(1)),
        )));
    }
    assert!(screen_notes(&rig).await.is_empty());
    assert_eq!((rig.pastes(), rig.keys()), (0, 0));
}

// Break caught: retain short geometry from an unproven read, or make a known
// usable editor a short-viewport diagnosis just because the pane is short.
#[tokio::test(start_paused = true)]
async fn short_viewport_requires_an_attributable_incomplete_read() {
    let rig = Rig::start_with_harness(
        0,
        Fold::Service,
        true,
        DeliveryPolicy::default(),
        None,
        Harness::ClaudeCode,
        UnreadableFixture::Plain,
    )
    .await;
    let state = rig.org.snapshot().await.unwrap();
    let agent = state.agent(&worker()).unwrap().clone();
    let read = Effects::production(&rig.rt).observe;
    for (frame, want_hint) in [
        (claude_resize_frame(6), true),
        (claude_resize_frame(40), false),
        (
            with(claude_resize_frame(6), |c| c.inspection.in_mode = true),
            false,
        ),
        (
            with(claude_resize_frame(6), |c| {
                c.inspection.foreground_command = Some("sh".into())
            }),
            false,
        ),
    ] {
        rig.script(&[frame]);
        assert_eq!(
            guarded(read(agent.clone(), None))
                .await
                .unwrap()
                .2
                .is_some(),
            want_hint
        );
    }
    rig.script(&[with(claude_resize_frame(6), |c| {
        c.inspection.local_agent_tag = Some("demo/other".into())
    })]);
    assert!(guarded(read(agent.clone(), None)).await.is_err());
    assert!(guarded(read(agent, None)).await.is_err(), "capture missing");

    let ready = Rig::attempt().await;
    let state = ready.org.snapshot().await.unwrap();
    let agent = state.agent(&worker()).unwrap().clone();
    ready.script(&[pi_resize_frame(6)]);
    let (seen, _, hint, _) = guarded((Effects::production(&ready.rt).observe)(agent, None))
        .await
        .unwrap();
    assert_eq!(
        (seen.facts.input, seen.facts.turn, seen.facts.dialog),
        (InputState::Empty, TurnState::NotRunning, DialogState::None)
    );
    assert!(hint.is_none());
}

// Break caught: guess a screen change at the confirming short read, or keep
// a small hint from an earlier read even after a resize with no new episode.
#[tokio::test(start_paused = true)]
async fn short_viewport_unreadable_uses_confirming_size() {
    for height in [5, 40] {
        let rig = Rig::attempt().await;
        rig.script(&[pi_resize_frame(5)]);
        assert_eq!(observe(&rig).await.unwrap().facts, ScreenFacts::default());
        rig.script(&[with(pi_resize_frame(5), |capture| {
            capture.inspection.pane.height = height;
            capture.text.push_str(&"\n".repeat(usize::from(height - 5)));
        })]);
        let mut events = rig.org.committed.subscribe();
        tokio::time::advance(Duration::from_secs(120)).await;
        let note = guarded(events.recv()).await.unwrap();
        assert_eq!(note.kind, "msg.sent");
        let text = if height == 5 {
            "atm cannot read demo/worker's screen for 120 s; pane %1 was 217x5 on the last read; try making it taller so atm can verify the editor and turn state. Delivery, readiness checks, clear, and handover need a readable screen."
        } else {
            "atm cannot read demo/worker's screen (%1) for 120 s; pi may have changed its screen (version unknown). Delivery, readiness checks, clear, and handover need a readable screen."
        };
        assert_eq!(screen_notes(&rig).await, vec![text]);
        // A healthy editor also fits at 6. The same tracker must end the
        // episode on that successful read, irrespective of the new height.
        rig.script(&[pi_resize_frame(6)]);
        let ready = observe(&rig).await.unwrap();
        assert_eq!(
            (ready.facts.input, turn(&ready), ready.facts.dialog),
            (InputState::Empty, TurnState::NotRunning, DialogState::None)
        );
        let state = rig.org.snapshot().await.unwrap();
        assert!(!rig
            .rt
            .agents
            .screens()
            .unwrap()
            .active(state.agent(&worker()).unwrap()));
        assert_eq!((rig.pastes(), rig.keys()), (0, 0));
    }
}

#[tokio::test(start_paused = true)]
async fn unreadable_screen_needs_a_successful_attributable_read() {
    let rig = Rig::attempt().await;
    let tracker = rig.rt.agents.screens().unwrap();
    let agent = rig
        .org
        .snapshot()
        .await
        .unwrap()
        .agent(&worker())
        .unwrap()
        .clone();
    for frame in [
        pi(RUNNING),
        pi(DRAFT),
        with(pi(UNREADABLE), |c| c.inspection.in_mode = true),
        with(pi(UNREADABLE), |c| {
            c.inspection.foreground_command = Some("zsh".into())
        }),
        with(pi(UNREADABLE), |c| {
            c.inspection.local_agent_tag = Some("demo/other".into())
        }),
    ] {
        rig.script(&[frame]);
        let _ = observe(&rig).await;
        assert!(!tracker.active(&agent));
        assert_eq!(rig.reads(), 0);
    }
    assert!(observe(&rig).await.is_err());
    assert!(!tracker.active(&agent));
    rig.script(&[pi(UNREADABLE)]);
    assert_eq!(turn(&observe(&rig).await.unwrap()), TurnState::Unknown);
    assert!(tracker.active(&agent));
    assert_eq!(rig.pastes(), 0);
}

#[tokio::test(start_paused = true)]
async fn unreadable_screen_deadline_does_not_report_an_old_binding() {
    let rig = Rig::attempt().await;
    rig.script(&[pi(UNREADABLE)]);
    assert_eq!(turn(&observe(&rig).await.unwrap()), TurnState::Unknown);
    rig.commit(OrgCommand::SessionBound {
        agent_id: worker(),
        session_id: SessionId::new("s2"),
    })
    .await;
    rig.script(&[pi(UNREADABLE)]);
    let mut events = rig.org.committed.subscribe();
    tokio::time::advance(Duration::from_secs(120)).await;
    let diagnostic = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if events.recv().await.unwrap().kind == "msg.sent" {
                return;
            }
        }
    })
    .await;
    assert!(
        diagnostic.is_err(),
        "a previous binding's clock produced a stale note"
    );
    assert_eq!(rig.reads(), 3, "the obsolete deadline must not capture");
    assert_eq!(rig.pastes(), 0);
}

#[tokio::test(start_paused = true)]
async fn unreadable_screen_transient_retry_ends_on_loss_rebind_or_readable_frame() {
    #[derive(Clone, Copy, Debug)]
    enum End {
        PaneGone,
        Readable,
    }
    for end in [End::PaneGone, End::Readable] {
        let rig = Rig::attempt().await;
        rig.script(&[pi(UNREADABLE)]);
        assert_eq!(turn(&observe(&rig).await.unwrap()), TurnState::Unknown);
        let mut reads = rig.read_events.clone();
        let before = reads.borrow().0;
        assert_eq!(rig.reads(), 0);
        tokio::time::advance(Duration::from_secs(120)).await;
        // Empty script: the compound capture and the metadata fallback both fail.
        let first_retry = before + 2;
        guarded(reads.wait_for(|event| event.0 >= first_retry))
            .await
            .unwrap();
        assert_eq!(reads.borrow().0, first_retry);
        let failed_at = Instant::now();
        assert!(screen_notes(&rig).await.is_empty());

        let raw_calls = match end {
            End::PaneGone => {
                rig.tmux.script_inspect(&pane(), None).unwrap();
                2 // failed compound followed by positive pane-gone metadata
            }
            End::Readable => {
                rig.script(&[pi(DRAFT)]); // readable does not mean safe for input
                1
            }
        };
        let last = first_retry + raw_calls;
        guarded(reads.wait_for(|event| event.0 >= last))
            .await
            .unwrap();
        assert!(
            Instant::now() >= failed_at + Duration::from_secs(5),
            "{end:?}"
        );
        assert_eq!(rig.reads(), 0);
        // Wait across several retry deadlines, ending on any unexpected read.
        assert!(
            tokio::time::timeout(
                Duration::from_secs(30),
                reads.wait_for(|event| event.0 > last)
            )
            .await
            .is_err(),
            "retry outlived {end:?}"
        );
        let state = rig.org.snapshot().await.unwrap();
        assert!(!rig
            .rt
            .agents
            .screens()
            .unwrap()
            .active(state.agent(&worker()).unwrap()));
        assert!(screen_notes(&rig).await.is_empty());
        assert_eq!(rig.pastes(), 0);
    }
}

// Break caught: permanent capture/metadata failure re-enters the worker
// past the budget, or a matching unknown sample resets that budget.
#[tokio::test(start_paused = true)]
async fn unreadable_screen_confirmation_stops_after_twelve_errors() {
    let rig = Rig::attempt().await;
    let worker = worker();
    let state = guarded(rig.org.snapshot()).await.unwrap();
    let agent = state.agent(&worker).cloned().unwrap();
    rig.script(&[pi(UNREADABLE)]);
    let (seen, since, _, _) = guarded((Effects::production(&rig.rt).observe)(agent, None))
        .await
        .unwrap();
    assert_eq!(turn(&seen), TurnState::Unknown);
    let since = since.unwrap();
    assert_eq!(rig.reads(), 0);
    let screens = rig.rt.agents.screens().unwrap();
    let mut reads = rig.read_events.clone();
    let before = reads.borrow().0;
    // With no script, each check fails both compound capture and fallback
    // metadata. The thirteenth signals an assertion failure on the base.
    let settled = tokio::time::timeout(Duration::from_secs(240), async {
        tokio::select! {
            () = screens.wait_for_diagnosis_to_settle(&worker, since) => true,
            _ = reads.wait_for(|event| event.0 >= before + 26) => false,
        }
    })
    .await
    .expect("neither confirmation exhaustion nor a thirteenth check");
    assert!(settled, "a thirteenth confirmation ran in the same episode");
    assert_eq!(
        reads.borrow().0 - before,
        24,
        "exactly twelve failed checks"
    );
    // s3-5c S1: a spent budget leaves the episode dormant, so the
    // revalidation poll may look at the seat again a minute later.
    let now = tokio::time::Instant::now();
    assert!(!screens.take_relook(&worker, now));
    assert!(screens.take_relook(&worker, now + Duration::from_secs(60)));
    assert!(
        screen_notes(&rig).await.is_empty(),
        "no note without confirmation"
    );

    rig.script(&[pi(UNREADABLE)]);
    assert_eq!(turn(&observe(&rig).await.unwrap()), TurnState::Unknown);
    let unchanged = reads.borrow().0;
    // Negative assertion on virtual time: across the readiness window, no
    // more calls are made just because we saw that same screen again.
    assert!(
        tokio::time::timeout(
            Duration::from_secs(180),
            reads.wait_for(|event| event.0 > unchanged)
        )
        .await
        .is_err(),
        "the same sample rearmed a spent episode"
    );

    rig.script(&[pi(IDLE)]);
    assert_eq!(turn(&observe(&rig).await.unwrap()), TurnState::NotRunning);
    let mut notes = rig.org.committed.subscribe();
    rig.script(&[pi(UNREADABLE), pi(UNREADABLE)]);
    assert_eq!(turn(&observe(&rig).await.unwrap()), TurnState::Unknown);
    let note = tokio::time::timeout(Duration::from_secs(180), notes.recv())
        .await
        .expect("new unreadable episode did not confirm")
        .unwrap();
    assert_eq!(note.kind, "msg.sent", "a new episode can confirm");
    let bodies = screen_notes(&rig).await;
    assert_eq!(bodies.len(), 1);
    assert!(bodies[0].starts_with("atm cannot read demo/worker's screen"));
    assert_eq!(rig.pastes(), 0);
}

#[tokio::test(start_paused = true)]
async fn unreadable_screen_real_observation_loss_still_tells_owner() {
    for previous_note in [false, true] {
        let production: Arc<OnceLock<Effects>> = Arc::default();
        let effects = Effects {
            deliver: Arc::new(|_, _, _| panic!("no usable observation authorizes input")),
            observe: Arc::new({
                let production = production.clone();
                move |agent, reference| (production.get().unwrap().observe)(agent, reference)
            }),
            retract: super::no_retract(),
        };
        let policy = DeliveryPolicy::new(DeliverySettings {
            readiness_timeout: Duration::from_secs(3),
            ..Default::default()
        })
        .unwrap();
        let rig = Rig::start(Fold::Service, true, policy, Some(effects)).await;
        assert!(production.set(Effects::production(&rig.rt)).is_ok());
        let mut events = rig.org.committed.subscribe();
        rig.script(&[pi(UNREADABLE)]);
        assert_eq!(turn(&observe(&rig).await.unwrap()), TurnState::Unknown);
        if previous_note {
            rig.script(&[pi(UNREADABLE)]);
            tokio::time::advance(Duration::from_secs(3)).await;
            assert_eq!(guarded(events.recv()).await.unwrap().kind, "msg.sent");
        }
        // Both the capture and its metadata fallback now fail. That is a
        // separate problem from a successful but unreadable viewport.
        let source = MessageId::new(rig.commit(Rig::message(worker())).await);
        let owner_notice = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let event = events.recv().await.unwrap();
                if let Some(OrgEvent::MessageSent(content)) =
                    OrgEvent::from_parts(&event.kind, &event.body).unwrap()
                {
                    if content.delivery_notice.as_ref().is_some_and(|key| {
                        key.source_message_id == source && key.reason == "unobserved"
                    }) {
                        break content;
                    }
                }
            }
        })
        .await
        .expect("real observation loss must tell the owner even during an unreadable episode");
        assert_eq!(owner_notice.to, Address::Owner);
        assert_eq!(rig.snapshot(source).message.status, DeliveryStatus::Queued);
        assert_eq!(rig.pastes(), 0);
    }
}

#[tokio::test(start_paused = true)]
async fn unreadable_screen_failed_capture_cannot_retry_after_rebind() {
    let rig = Rig::attempt().await;
    rig.script(&[pi(UNREADABLE)]);
    assert_eq!(turn(&observe(&rig).await.unwrap()), TurnState::Unknown);
    let mut reads = rig.read_events.clone();
    let before = reads.borrow().0;
    // The compound capture and the metadata fallback fail.
    tokio::time::advance(Duration::from_secs(120)).await;
    guarded(reads.wait_for(|event| event.0 >= before + 2))
        .await
        .unwrap();
    rig.commit(OrgCommand::SessionBound {
        agent_id: worker(),
        session_id: SessionId::new("new-session"),
    })
    .await;
    // No working frame or metadata is available to learn about the rebind.
    assert!(
        tokio::time::timeout(
            Duration::from_secs(20),
            reads.wait_for(|event| event.0 > before + 2),
        )
        .await
        .is_err(),
        "retried a failed confirmation for an obsolete binding"
    );
    assert!(screen_notes(&rig).await.is_empty());
}

#[tokio::test(start_paused = true)]
async fn unreadable_screen_rebind_at_the_writer_does_not_change_delivery() {
    let mut rig = Rig::start_with_harness(
        0,
        Fold::Service,
        true,
        DeliveryPolicy::default(),
        None,
        Harness::Pi,
        UnreadableFixture::RebindBeforeNote,
    )
    .await;
    let mail = MessageId::new(rig.commit(Rig::message(worker())).await);
    rig.script(&[pi(UNREADABLE)]);
    assert_eq!(turn(&observe(&rig).await.unwrap()), TurnState::Unknown);
    rig.script(&[pi(UNREADABLE)]);
    let mut events = rig.org.committed.subscribe();
    tokio::time::advance(Duration::from_secs(120)).await;
    let binding = guarded(async {
        loop {
            let event = events.recv().await.unwrap();
            if event.kind == "agent.session_bound" {
                break event.seq;
            }
        }
    })
    .await;
    let state = rig.org.snapshot().await.unwrap();
    assert_eq!(
        state.agent(&worker()).unwrap().session_id,
        Some(SessionId::new("new-session"))
    );
    assert_eq!(state.message(&mail).unwrap().status, DeliveryStatus::Queued);
    assert_eq!(rig.pastes(), 0);
    // Replay the journal from before the race. The description may no longer
    // be true when the event arrives, so replay must not depend on it.
    let mut replayed = OrgState::default();
    let mut stats = atm_org::ReplayStats::default();
    while let Ok(event) = rig.committed.try_recv() {
        stats
            .apply_parts(&mut replayed, event.seq, event.ts, &event.kind, &event.body)
            .unwrap();
    }
    assert_eq!(stats.skipped_unknown, 0);
    assert_eq!(
        replayed.agent(&worker()).unwrap().session_id,
        Some(SessionId::new("new-session"))
    );
    assert_eq!(
        replayed.message(&mail).unwrap().status,
        DeliveryStatus::Queued
    );
    let mut late = replayed.messages().filter(|m| m.id.seq() > binding);
    // Until a writer-side precondition exists the only allowed late effect
    // is one ordinary, replayable informational Note. No other side effect.
    if let Some(note) = late.next() {
        assert_eq!(
            (note.content.from.clone(), note.content.kind),
            (Address::System, MessageKind::Note)
        );
        assert!(note.content.delivery_notice.is_none());
        assert!(note
            .content
            .body
            .starts_with("atm cannot read demo/worker's screen"));
        assert!(late.next().is_none());
    }
}

async fn screen_notes(rig: &Rig) -> Vec<String> {
    rig.org
        .snapshot()
        .await
        .unwrap()
        .inbox(&Address::Owner)
        .iter()
        .filter(|msg| msg.content.from == Address::System && msg.content.kind == MessageKind::Note)
        .map(|msg| msg.content.body.clone())
        .collect()
}

#[tokio::test(start_paused = true)]
async fn focus_is_the_captures_sample_and_never_a_guess() {
    let rig = Rig::attempt().await;
    for focus in [None, Some(true), Some(false)] {
        rig.script(&[with(pi(IDLE), |capture| capture.focused = focus)]);
        assert_eq!(observe(&rig).await.unwrap().focused, focus);
    }
}

#[tokio::test(start_paused = true)]
async fn every_failure_is_an_error() {
    // A pane tagged for someone else.
    let mut rig = Rig::attempt().await;
    rig.script(&[with(pi(IDLE), |capture| {
        capture.inspection.local_agent_tag = Some("demo/other".into());
    })]);
    let foreign = observe(&rig).await.unwrap_err();
    assert!(matches!(foreign, MessageError::Conflict(_)), "{foreign:?}");
    // A harness the agent service has not established (a start, an adopt,
    // the pass after a daemon restart).
    (rig.rt.agents).record_baseline(worker(), "%1", PaneBaseline::adopted(100));
    rig.script(&[pi(IDLE)]);
    let unpinned = observe(&rig).await.unwrap_err();
    assert!(matches!(unpinned, MessageError::Refused(_)), "{unpinned:?}");
    // A registry that cannot answer: never "no session".
    rig.registry_stop.cancel();
    guarded(rig.registry_tasks.take().unwrap().actor)
        .await
        .unwrap();
    rig.script(&[pi(IDLE)]);
    let registry = observe(&rig).await.unwrap_err();
    assert!(
        matches!(registry, MessageError::Unavailable(_)),
        "{registry:?}"
    );
    // The frame scripted for it was not read: no capture without a recipient.
    assert_eq!(rig.reads(), 3);
}

// s3-stuckseat (lead's ask a). Break: a Ready seat whose pane shows only a
// discovery row since a restart reads "cannot be observed", or a row merely
// missing for one poll is reported as unconfirmed.
#[tokio::test(start_paused = true)]
async fn ready_seat_with_only_a_discovery_row_says_its_session_is_unconfirmed() {
    let rig = Rig::attempt().await;
    let mut recipient = recipient(&rig).await;
    recipient.status = AgentStatus::Ready;
    recipient.session = None;
    let frame = pi(IDLE);
    let facts = recipient.screen.facts(&frame, &recipient.baseline);
    recipient.unconfirmed = true;
    let error = observed(&worker(), &recipient, &frame, facts.clone()).unwrap_err();
    assert!(
        matches!(&error, MessageError::Refused(text) if text == UNCONFIRMED),
        "{error:?}"
    );
    recipient.unconfirmed = false;
    let error = observed(&worker(), &recipient, &frame, facts).unwrap_err();
    assert!(
        matches!(&error, MessageError::Refused(text) if text != UNCONFIRMED),
        "{error:?}"
    );
}

// s3-d1. Break: a restarted seat whose discovery row is its recorded
// session's process still waits as unconfirmed in the scheduler's look, or
// a row with no record (the binding predates this process) stops waiting.
#[tokio::test(start_paused = true)]
async fn restarted_seat_matched_by_process_is_observed_as_its_session() {
    let rig = Rig::attempt().await;
    // The agent service's own pass after the registry change waits here.
    let _parked = rig.park().await;
    let registry = rig.rt.registry.clone();
    guarded(registry.remove(SessionId::new("s1")))
        .await
        .unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let pid = std::process::id();
    guarded(registry.register_discovered(
        SessionId::pending_from_pid(pid),
        pid,
        cwd.path().to_path_buf(),
        Some("%1".into()),
        Harness::Pi,
    ))
    .await
    .unwrap();
    let frame = pi(IDLE);
    let unmatched = recipient(&rig).await;
    assert!(unmatched.unconfirmed && unmatched.session.is_none());
    let facts = unmatched.screen.facts(&frame, &unmatched.baseline);
    let error = observed(&worker(), &unmatched, &frame, facts).unwrap_err();
    assert!(
        matches!(&error, MessageError::Refused(text) if text == UNCONFIRMED),
        "{error:?}"
    );

    let start = crate::org::message_service_tests::own_process_start();
    let seq = rig
        .commit(crate::org::message_service_tests::own_process("s1", start))
        .await;
    rig.folded(seq).await;
    let matched = recipient(&rig).await;
    assert!(!matched.unconfirmed);
    let row = matched.session.as_ref().unwrap();
    assert_eq!((row.id.as_str(), row.status), ("s1", SessionStatus::Idle));
    let facts = matched.screen.facts(&frame, &matched.baseline);
    let seen = observed(&worker(), &matched, &frame, facts).unwrap();
    assert_eq!(seen.session.map(|row| row.id), Some(SessionId::new("s1")));
}

#[tokio::test(start_paused = true)]
async fn only_a_ready_agent_passes_the_executors_gate() {
    let rig = Rig::attempt().await;
    let bound = recipient(&rig).await;
    let row = bound.session.clone();
    let frame = pi(RUNNING);
    let observe_as = |id: &AgentId, recipient: &Recipient| {
        let facts = recipient.screen.facts(&frame, &recipient.baseline);
        observed(id, recipient, &frame, facts)
    };
    let see = |recipient: &Recipient| observe_as(&worker(), recipient);
    let binding = Some(SessionId::new("s1"));
    for (recorded, eligible, attempted) in [
        (binding.clone(), row.clone(), true),
        // The row is missing for one poll, or the binding is not made yet.
        (binding.clone(), None, false),
        (None, row.clone(), false),
        // A lazy first prompt: no binding, and no row to contradict it.
        (None, None, true),
    ] {
        let mut recipient = bound.clone();
        (recipient.session_id, recipient.session) = (recorded, eligible.clone());
        // Ready: the executor's own answer, so the scheduler never asks for
        // an attempt the gate would refuse under the reservation.
        recipient.status = AgentStatus::Ready;
        let gate = attempt_session(
            recipient.status,
            recipient.session_id.as_ref(),
            recipient.session.as_ref().map(|row| &row.id),
        );
        assert_eq!(gate.is_ok(), attempted);
        assert_eq!(see(&recipient).is_ok(), attempted);
        // Starting or clearing: no attempt is possible, and the turn is
        // observed whatever the binding says.
        for status in [AgentStatus::Starting, AgentStatus::Clearing] {
            recipient.status = status;
            let seen = see(&recipient).unwrap();
            assert_eq!(turn(&seen), TurnState::Running);
            assert_eq!(
                seen.session.map(|row| row.id),
                eligible.as_ref().map(|row| row.id.clone())
            );
        }
    }
    // The gate is the only thing a status changes: these refuse for any.
    for status in [AgentStatus::Ready, AgentStatus::Starting] {
        let mut recipient = bound.clone();
        recipient.status = status;
        assert!(observe_as(&"demo/other".parse().unwrap(), &recipient).is_err());
        recipient.baseline = PaneBaseline::unproven();
        assert!(see(&recipient).is_err());
    }
}

/// The scheduler's own job, on the production observer and the production
/// executor: it observes the worker on %1 and asks for an attempt there; the
/// agent moves to %2 before the executor gets its reservation.
#[tokio::test(start_paused = true)]
async fn pane_changed_between_the_observation_and_the_reservation_gets_no_input() {
    // The production effects need the rig's runtime, which exists only once
    // the rig does.
    let production: Arc<OnceLock<Effects>> = Arc::default();
    let (enter, mut entered) = mpsc::unbounded_channel();
    let (leave, mut returned) = mpsc::unbounded_channel();
    let effects = Effects {
        deliver: Arc::new({
            let production = production.clone();
            move |rt, id, deadline| {
                let deliver = production.get().unwrap().deliver.clone();
                let (enter, leave) = (enter.clone(), leave.clone());
                Box::pin(async move {
                    let _ = enter.send(id);
                    let result = deliver(rt, id, deadline).await;
                    let _ = leave.send(result.as_ref().map(|_| ()).map_err(ToString::to_string));
                    result
                })
            }
        }),
        observe: Arc::new({
            let production = production.clone();
            move |agent, reference| (production.get().unwrap().observe)(agent, reference)
        }),
        retract: super::no_retract(),
    };
    let mut rig = Rig::start(
        Fold::Service,
        true,
        DeliveryPolicy::default(),
        Some(effects),
    )
    .await;
    assert!(production.set(Effects::production(&rig.rt)).is_ok());
    while rig.committed.try_recv().is_ok() {}
    // Another reservation holds the worker and %1; observing needs none.
    let parked = rig.park().await;
    rig.script(&vec![pi(IDLE); 600]);
    let id = MessageId::new(rig.commit(Rig::message(worker())).await);
    let started = Instant::now();
    let hang = Duration::from_secs(600);
    // The job saw a ready pane for the stable time and asked for an attempt.
    assert_eq!(
        timeout_at(started + hang, entered.recv()).await,
        Ok(Some(id))
    );
    assert!(rig.reads() < 600 * 3, "the observer read %1");

    for command in [
        OrgCommand::Respawn {
            agent_id: worker(),
            role: atm_org::parse_role("worker", "+++\nharness='pi'\n+++\n").unwrap(),
            reason: "moved".into(),
        },
        OrgCommand::PaneCreated {
            agent_id: worker(),
            pane: "%2".into(),
        },
    ] {
        rig.commit(command).await;
    }
    parked.send(()).unwrap();

    let result = timeout_at(started + hang, returned.recv()).await.unwrap();
    let refused = result.unwrap().unwrap_err();
    assert!(refused.contains("no longer on %1"), "{refused}");
    // Nothing was typed anywhere, and no attempt was spent.
    assert_eq!(rig.pastes(), 0);
    guarded(rig.org.snapshot()).await.unwrap();
    while let Ok(envelope) = rig.committed.try_recv() {
        assert_ne!(envelope.kind, "msg.delivery_attempted");
    }
    let snapshot = rig.snapshot(id);
    assert_eq!(snapshot.message.status, DeliveryStatus::Queued);
    assert_eq!(snapshot.message.last_attempt, None);
}
