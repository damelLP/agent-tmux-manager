//! s3-5c: the retract executor on the real control actor and a scripted tmux,
//! for a Claude worker (the one harness with a retract key).
use super::*;
use crate::org::message_retract::{retract, Retracted};

// Captured: Claude Code 2.1.330, 2026-11-20, sealed rig capture-pane -p -e
// (provenance line in each fixture). Synthetic: the live message id replaces
// the captured `m99330`, on the same one row.
const STAGED_330: &str = include_str!(
    "../../../atm-claude-adapter/tests/fixtures/claude_reference_staged_82x60_2.1.330.styled.txt"
);
const AFTER_330: &str = include_str!(
    "../../../atm-claude-adapter/tests/fixtures/claude_reference_after_ctrl_u_82x60_2.1.330.styled.txt"
);

fn claude(fixture: &str, id: MessageId) -> PaneCapture {
    with(pi(IDLE), |capture| {
        capture.text = body(fixture).replace("m99330", &id.to_string());
        capture.inspection.pane.width = 82;
        capture.inspection.pane.height = 60;
        capture.inspection.foreground_command = Some("claude".into());
    })
}

async fn rig(harness: Harness) -> Rig {
    Rig::start_with_harness(
        0,
        Fold::Service,
        true,
        DeliveryPolicy::default(),
        None,
        harness,
        UnreadableFixture::Plain,
    )
    .await
}

/// A message attempted on %1 whose delivery then failed finally (or not).
async fn attempted(rig: &mut Rig, final_failure: Option<bool>) -> MessageId {
    let id = rig.queue_message().await;
    let mut seq = rig
        .commit(OrgCommand::DeliveryAttempted {
            message_id: id,
            attempt: 1,
            pane: "%1".into(),
        })
        .await;
    if let Some(final_failure) = final_failure {
        seq = rig
            .commit(OrgCommand::DeliveryFailed {
                message_id: id,
                reason: "recipient acknowledged; automatic delivery cancelled".into(),
                final_failure,
            })
            .await;
    }
    rig.folded(seq).await;
    id
}

async fn run(rig: &Rig, id: MessageId) -> Result<Retracted, MessageError> {
    let deadline = Instant::now() + ATTEMPT_WORK_BUDGET;
    guarded(spawn_polled(retract(rig.rt.clone(), id, deadline)))
        .await
        .unwrap()
}

// Break: the key is not one Ctrl-U to the recipient's pane, or a cleared
// editor is not reported as cleared, or the key goes unaudited.
#[tokio::test(start_paused = true)]
async fn retract_sends_one_ctrl_u_and_reads_the_editor_empty() {
    let mut rig = rig(Harness::ClaudeCode).await;
    let id = attempted(&mut rig, Some(true)).await;
    let staged = claude(STAGED_330, id);
    let reader = agent_screen(Harness::ClaudeCode).unwrap();
    let baseline = PaneBaseline::adopted(100)
        .with_harness("claude".into())
        .unwrap();
    assert_eq!(
        reader.staged(&staged, &baseline, &inbox_reference(id)),
        Some(true),
        "the substituted capture still stages exactly the reference"
    );
    let mut committed = rig.committed.resubscribe();
    // The actor's capture just before the key, then the look after it.
    rig.script(&[staged, claude(AFTER_330, id)]);
    assert_eq!(run(&rig, id).await.unwrap(), Retracted::Cleared);
    assert_eq!(
        rig.effects(),
        [MockCall::SendKeys {
            pane: "%1".into(),
            keys: "C-u".into(),
        }]
    );
    let audit = guarded(async {
        loop {
            let envelope = committed.recv().await.unwrap();
            if envelope.kind == "control.sent" {
                return envelope;
            }
        }
    })
    .await;
    assert_eq!(audit.body["pane"], "%1");
    assert_eq!(audit.body["action"]["method"], "msg.retract");
    assert_eq!(audit.body["action"]["op"], "retract");
    assert_eq!(audit.body["action"]["message"], id.to_string());
    assert_eq!(audit.body["action"]["key"], "CtrlU");
    assert_eq!(audit.body["action"]["outcome"], "ok");
}

// Break: a capture that is not exactly atm's reference on one row in an idle
// editor with nobody in the pane still gets the key.
#[tokio::test(start_paused = true)]
async fn retract_sends_nothing_unless_the_capture_allows_it() {
    for case in [
        "beside a draft",
        "other reference",
        "empty",
        "focused",
        "unknown focus",
        "copy mode",
        "dead",
        "synchronized",
        "input off",
        "foreground",
        "tag",
    ] {
        let mut rig = rig(Harness::ClaudeCode).await;
        let id = attempted(&mut rig, Some(true)).await;
        let staged = claude(STAGED_330, id);
        let reference = inbox_reference(id);
        let changed = match case {
            "beside a draft" => with(staged, |c| {
                c.text = c.text.replace(&reference, &format!("{reference} and mine"));
            }),
            "other reference" => with(staged, |c| {
                c.text = c
                    .text
                    .replace(&reference, "[m999] Read: atm inbox --id m999");
            }),
            "empty" => claude(AFTER_330, id),
            "focused" => with(staged, |c| c.focused = Some(true)),
            "unknown focus" => with(staged, |c| c.focused = None),
            "copy mode" => with(staged, |c| c.inspection.in_mode = true),
            "dead" => with(staged, |c| c.inspection.dead = true),
            "synchronized" => with(staged, |c| c.inspection.synchronized = true),
            "input off" => with(staged, |c| c.inspection.input_off = true),
            "foreground" => with(staged, |c| {
                c.inspection.foreground_command = Some("sh".into());
            }),
            "tag" => with(staged, |c| {
                c.inspection.local_agent_tag = Some("demo/other".into());
            }),
            _ => unreachable!(),
        };
        // The control actor's capture just before the key decides.
        rig.script(&[changed]);
        let done = run(&rig, id).await;
        assert!(
            matches!(done, Ok(Retracted::Refused) | Err(_)),
            "{case}: {done:?}"
        );
        assert_eq!(rig.keys(), 0, "{case}");
    }
}

// Break: after the actor's capture refuses the key, the executor looks again
// and sends it on a later frame (a retry inside one retract).
#[tokio::test(start_paused = true)]
async fn retract_takes_no_second_chance_after_the_actor_refuses() {
    let mut rig = rig(Harness::ClaudeCode).await;
    let id = attempted(&mut rig, Some(true)).await;
    let staged = claude(STAGED_330, id);
    let focused = with(staged.clone(), |c| c.focused = Some(true));
    rig.script(&[focused, staged]);
    let unread = rig.reads();
    assert_eq!(run(&rig, id).await.unwrap(), Retracted::Refused);
    assert_eq!(rig.keys(), 0);
    assert_eq!(unread - rig.reads(), 3, "one frame read, the next one left");
}

// Break: an editor that still holds text, or cannot be read, after the key
// is reported cleared (review M15: only an editor read Empty is Cleared).
#[tokio::test(start_paused = true)]
async fn retract_reports_text_left_after_the_key() {
    for case in ["still staged", "unreadable"] {
        let mut rig = rig(Harness::ClaudeCode).await;
        let id = attempted(&mut rig, Some(true)).await;
        let staged = claude(STAGED_330, id);
        let after = match case {
            "still staged" => staged.clone(),
            _ => with(staged.clone(), |c| {
                c.text = format!("an unrecognized screen\n{}", "\n".repeat(59))
            }),
        };
        rig.script(&[staged, after]);
        assert_eq!(run(&rig, id).await.unwrap(), Retracted::Left, "{case}");
        assert_eq!(rig.keys(), 1, "{case}");
    }
}

// Break: atm retracts the reference of a message that is still open (Enter
// recovery's) or of one that never failed finally, or on a harness with no
// proven key (pi), or reads the pane at all for them.
#[tokio::test(start_paused = true)]
async fn retract_refuses_open_messages_and_harnesses_without_a_key() {
    for (case, harness, failure) in [
        ("delivering", Harness::ClaudeCode, None),
        ("queued for its retry", Harness::ClaudeCode, Some(false)),
        ("pi", Harness::Pi, Some(true)),
    ] {
        let mut rig = rig(harness).await;
        let id = attempted(&mut rig, failure).await;
        // A frame that would allow the key, were the executor to look.
        rig.script(&[claude(STAGED_330, id)]);
        let unread = rig.reads();
        assert!(run(&rig, id).await.is_err(), "{case}");
        assert_eq!(rig.keys(), 0, "{case}");
        assert_eq!(rig.reads(), unread, "{case}: the pane was read");
    }
}

// s3-5c (review SHOULD 3). Break: a `send-keys` that fails after tmux may
// have delivered the key counts as nothing sent, so no note is written and a
// later look can press Ctrl-U again.
#[tokio::test(start_paused = true)]
async fn retract_key_of_unknown_outcome_is_never_refused() {
    use crate::org::message_delivery_gate::Effect;
    for (after, expected) in [
        (AFTER_330, Retracted::Cleared),
        (STAGED_330, Retracted::Left),
    ] {
        let mut rig = rig(Harness::ClaudeCode).await;
        let id = attempted(&mut rig, Some(true)).await;
        rig.tmux_gate
            .fail_next_after_effect(Effect::SendKeys)
            .unwrap();
        let mut committed = rig.committed.resubscribe();
        rig.script(&[claude(STAGED_330, id), claude(after, id)]);
        assert_eq!(run(&rig, id).await.unwrap(), expected);
        assert_eq!(rig.keys(), 1);
        let audit = guarded(async {
            loop {
                let envelope = committed.recv().await.unwrap();
                if envelope.kind == "control.sent" {
                    return envelope;
                }
            }
        })
        .await;
        assert_eq!(audit.body["action"]["outcome"], "error", "{expected:?}");
    }
}

// s3-5c (review NIT 7). Break: the executor clears a reference on a pane the
// message was never attempted on (the agent was re-adopted elsewhere between
// the job's look and the reservation).
#[tokio::test(start_paused = true)]
async fn retract_refuses_a_pane_the_message_was_not_attempted_on() {
    let mut rig = rig(Harness::ClaudeCode).await;
    let id = attempted(&mut rig, Some(true)).await;
    rig.commit(OrgCommand::Down {
        agent_id: worker(),
        reason: "fixture".into(),
    })
    .await;
    let seq = rig
        .commit(OrgCommand::Adopt {
            agent_id: worker(),
            pane: "%2".into(),
            session_id: Some(SessionId::new("s1")),
        })
        .await;
    rig.folded(seq).await;
    let moved = PaneId::parse("%2").unwrap();
    let frame = with(claude(STAGED_330, id), |c| {
        c.inspection.pane.pane_id = "%2".into()
    });
    rig.tmux.script_capture(&frame).unwrap();
    let unread = rig.tmux.pending_reads(&moved).unwrap();
    assert!(unread > 0);
    assert!(run(&rig, id).await.is_err());
    assert_eq!(rig.keys(), 0);
    assert_eq!(
        rig.tmux.pending_reads(&moved).unwrap(),
        unread,
        "%2 was read"
    );
}
