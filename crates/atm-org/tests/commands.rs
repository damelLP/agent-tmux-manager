#![allow(clippy::unwrap_used, clippy::expect_used)] // test helpers may panic
mod support;
use atm_org::*;
use support::*;

fn run(state: &mut OrgState, command: OrgCommand, seq: &mut u64) -> Vec<OrgEvent> {
    let before = state.clone();
    let events = state.decide(&command, time()).unwrap();
    assert_eq!(*state, before);
    for event in &events {
        *seq += 1;
        state.apply(*seq, time(), event).unwrap();
    }
    events
}
fn setup() -> (OrgState, u64) {
    let mut state = OrgState::default();
    let mut seq = 0;
    run(
        &mut state,
        OrgCommand::RegisterProject {
            project_id: "p".parse().unwrap(),
            root: "/p".into(),
            move_root: false,
        },
        &mut seq,
    );
    (state, seq)
}
fn up() -> OrgCommand {
    OrgCommand::Up {
        agent_id: id(),
        role: role(),
    }
}
fn pane() -> OrgCommand {
    OrgCommand::PaneCreated {
        agent_id: id(),
        pane: "%1".into(),
    }
}
fn editor_ready() -> OrgCommand {
    OrgCommand::EditorReady { agent_id: id() }
}
fn clearing() -> (OrgState, u64) {
    let (mut state, mut seq) = setup();
    for command in [
        up(),
        pane(),
        bound("old"),
        OrgCommand::Clear { agent_id: id() },
    ] {
        run(&mut state, command, &mut seq);
    }
    (state, seq)
}
fn assert_noop(state: &mut OrgState, command: OrgCommand, seq: &mut u64) {
    let before = state.clone();
    assert!(run(state, command, seq).is_empty());
    assert_eq!(*state, before);
}
fn bound(session: &str) -> OrgCommand {
    OrgCommand::SessionBound {
        agent_id: id(),
        session_id: session.into(),
    }
}

#[test]
fn editor_ready_from_starting_with_pane() {
    let (mut state, mut seq) = setup();
    run(&mut state, up(), &mut seq);
    run(&mut state, pane(), &mut seq);
    assert_eq!(
        run(&mut state, editor_ready(), &mut seq),
        vec![OrgEvent::AgentEditorReady { agent_id: id() }]
    );
    let agent = state.agent(&id()).unwrap();
    assert_eq!(agent.status, AgentStatus::Ready);
    assert_eq!(agent.session_id, None);
    assert_eq!(agent.excluded_session_id, None);
    assert_eq!(agent.pane.as_deref(), Some("%1"));
}

#[test]
fn editor_ready_from_clearing_with_pane() {
    let (mut state, mut seq) = clearing();
    assert_eq!(
        state.agent(&id()).unwrap().excluded_session_id,
        Some("old".into())
    );
    assert_eq!(
        run(&mut state, editor_ready(), &mut seq),
        vec![OrgEvent::AgentEditorReady { agent_id: id() }]
    );
    let agent = state.agent(&id()).unwrap();
    assert_eq!(agent.status, AgentStatus::Ready);
    assert_eq!(agent.session_id, None);
    assert_eq!(agent.excluded_session_id, Some("old".into()));
}

#[test]
fn editor_ready_on_ready_agent_is_noop() {
    let (mut state, mut seq) = clearing();
    run(&mut state, editor_ready(), &mut seq);
    assert_noop(&mut state, editor_ready(), &mut seq);
    run(&mut state, bound("new"), &mut seq);
    assert_noop(&mut state, editor_ready(), &mut seq);
    assert_eq!(state.agent(&id()).unwrap().session_id, Some("new".into()));
}

#[test]
fn editor_ready_from_stuck_keeps_session() {
    for session in [None, Some("new")] {
        let (mut state, mut seq) = clearing();
        run(&mut state, editor_ready(), &mut seq);
        if let Some(session) = session {
            run(&mut state, bound(session), &mut seq);
        }
        let before = state.agent(&id()).unwrap().clone();
        run(
            &mut state,
            OrgCommand::Stuck {
                agent_id: id(),
                reason: "dialog".into(),
            },
            &mut seq,
        );
        let stuck = state.agent(&id()).unwrap();
        assert_eq!(stuck.status, AgentStatus::Stuck);
        assert_eq!(stuck.reason.as_deref(), Some("dialog"));
        assert_eq!(stuck.excluded_session_id, before.excluded_session_id);
        assert_eq!(run(&mut state, editor_ready(), &mut seq).len(), 1);
        let agent = state.agent(&id()).unwrap();
        assert_eq!(agent.status, AgentStatus::Ready);
        assert_eq!(agent.reason, None);
        assert_eq!(agent.session_id, before.session_id);
        assert_eq!(agent.pane, before.pane);
        assert_eq!(agent.excluded_session_id, before.excluded_session_id);
    }
}

#[test]
fn editor_ready_refuses_unknown_and_invalid_agent() {
    let (state, _) = setup();
    assert_eq!(
        state.decide(&editor_ready(), time()),
        Err(Refusal::Invariant(ApplyError::UnknownAgent(id())))
    );
    let bad = AgentId {
        project: "p".parse().unwrap(),
        name: "BAD".into(),
    };
    assert!(matches!(
        state.decide(&OrgCommand::EditorReady { agent_id: bad }, time()),
        Err(Refusal::Invariant(ApplyError::InvalidId(_)))
    ));
}

#[test]
fn editor_ready_refuses_missing_pane_and_down() {
    for status in [
        AgentStatus::Starting,
        AgentStatus::Clearing,
        AgentStatus::Stuck,
        AgentStatus::Ready,
        AgentStatus::Down,
    ] {
        let (mut state, mut seq) = setup();
        run(&mut state, up(), &mut seq);
        // Effect-only facts let the fixture cover no-pane states policy cannot create.
        let event = match status {
            AgentStatus::Starting => None,
            AgentStatus::Clearing => Some(OrgEvent::AgentClearRequested { agent_id: id() }),
            AgentStatus::Stuck => Some(OrgEvent::AgentStuck {
                agent_id: id(),
                reason: "late".into(),
            }),
            AgentStatus::Ready => Some(OrgEvent::AgentSessionBound {
                agent_id: id(),
                session_id: "s".into(),
            }),
            AgentStatus::Down => Some(OrgEvent::AgentDown {
                agent_id: id(),
                reason: "gone".into(),
            }),
        };
        if let Some(event) = event {
            state.apply(seq + 1, time(), &event).unwrap();
        }
        let before = state.clone();
        assert_eq!(
            state.decide(&editor_ready(), time()),
            Err(Refusal::InvalidAgentState {
                agent_id: id(),
                state: status,
                operation: "record editor ready"
            })
        );
        assert_eq!(state, before);
    }
    let (mut state, mut seq) = clearing();
    run(
        &mut state,
        OrgCommand::Down {
            agent_id: id(),
            reason: "gone".into(),
        },
        &mut seq,
    );
    state
        .apply(
            seq + 1,
            time(),
            &OrgEvent::AgentPaneCreated {
                agent_id: id(),
                pane: "%1".into(),
            },
        )
        .unwrap();
    assert!(matches!(
        state.decide(&editor_ready(), time()),
        Err(Refusal::InvalidAgentState {
            state: AgentStatus::Down,
            ..
        })
    ));
}

#[test]
fn excluded_session_is_ignored_during_clear_and_after_editor_ready() {
    let (mut state, mut seq) = clearing();
    assert_noop(&mut state, bound("old"), &mut seq);
    run(&mut state, editor_ready(), &mut seq);
    assert_noop(&mut state, bound("old"), &mut seq);
    assert_noop(
        &mut state,
        OrgCommand::SessionLost { agent_id: id() },
        &mut seq,
    );
    assert_noop(&mut state, bound("old"), &mut seq);
    run(
        &mut state,
        OrgCommand::Stuck {
            agent_id: id(),
            reason: "late".into(),
        },
        &mut seq,
    );
    assert_noop(&mut state, bound("old"), &mut seq);
    // A committed session-lost fact still folds to Starting with the exclusion.
    seq += 1;
    state
        .apply(seq, time(), &OrgEvent::AgentSessionLost { agent_id: id() })
        .unwrap();
    assert_eq!(state.agent(&id()).unwrap().status, AgentStatus::Starting);
    assert_noop(&mut state, bound("old"), &mut seq);
    run(&mut state, editor_ready(), &mut seq);
    assert_eq!(run(&mut state, bound("new"), &mut seq).len(), 1);
    let agent = state.agent(&id()).unwrap();
    assert_eq!(agent.status, AgentStatus::Ready);
    assert_eq!(agent.session_id, Some("new".into()));
    assert_eq!(agent.excluded_session_id, None);
    assert!(state.decide(&bound(""), time()).is_err());
}

#[test]
fn clearing_an_unbound_ready_editor_preserves_exclusion() {
    let (mut state, mut seq) = clearing();
    for _ in 0..2 {
        run(&mut state, editor_ready(), &mut seq);
        assert_eq!(
            run(&mut state, OrgCommand::Clear { agent_id: id() }, &mut seq).len(),
            1
        );
        assert_eq!(state.agent(&id()).unwrap().status, AgentStatus::Clearing);
        assert_eq!(
            state.agent(&id()).unwrap().excluded_session_id,
            Some("old".into())
        );
        assert_noop(&mut state, bound("old"), &mut seq);
    }
    run(&mut state, bound("new"), &mut seq);
    run(&mut state, OrgCommand::Clear { agent_id: id() }, &mut seq);
    assert_eq!(
        state.agent(&id()).unwrap().excluded_session_id,
        Some("new".into())
    );
    assert_noop(&mut state, bound("new"), &mut seq);
}

#[test]
fn session_lost_preserves_ready_without_session() {
    let (mut state, mut seq) = clearing();
    run(&mut state, editor_ready(), &mut seq);
    assert_noop(
        &mut state,
        OrgCommand::SessionLost { agent_id: id() },
        &mut seq,
    );
    assert_eq!(state.agent(&id()).unwrap().status, AgentStatus::Ready);
}

#[test]
fn explicit_binding_and_lifecycle_reset_exclusion() {
    for command in [
        OrgCommand::Adopt {
            agent_id: id(),
            pane: "%1".into(),
            session_id: Some("old".into()),
        },
        OrgCommand::Down {
            agent_id: id(),
            reason: "stop".into(),
        },
        OrgCommand::Respawn {
            agent_id: id(),
            role: role(),
            reason: "restart".into(),
        },
    ] {
        let (mut state, mut seq) = clearing();
        run(&mut state, editor_ready(), &mut seq);
        assert!(!run(&mut state, command, &mut seq).is_empty());
        let agent = state.agent(&id()).unwrap();
        assert_eq!(agent.excluded_session_id, None);
        if agent.status == AgentStatus::Down {
            assert_eq!(agent.session_id, None);
            assert_eq!(agent.pane, None);
            run(&mut state, up(), &mut seq);
            assert_eq!(state.agent(&id()).unwrap().excluded_session_id, None);
        }
    }
}

#[test]
fn sessionless_adopt_repairs_status_without_losing_exclusion() {
    // Confirmed review sequence, with and without editor readiness before repair.
    for ready in [false, true] {
        let (mut state, mut seq) = clearing();
        if ready {
            run(&mut state, editor_ready(), &mut seq);
        }
        let adopt = OrgCommand::Adopt {
            agent_id: id(),
            pane: "%1".into(),
            session_id: None,
        };
        assert_eq!(run(&mut state, adopt.clone(), &mut seq).len(), 1);
        let agent = state.agent(&id()).unwrap();
        assert_eq!(agent.status, AgentStatus::Starting);
        assert_eq!(agent.session_id, None);
        assert_eq!(agent.excluded_session_id, Some("old".into()));
        assert_noop(&mut state, bound("old"), &mut seq);
        assert_noop(&mut state, adopt, &mut seq);
        run(&mut state, editor_ready(), &mut seq);
        assert_noop(&mut state, bound("old"), &mut seq);
        let explicit = OrgCommand::Adopt {
            agent_id: id(),
            pane: "%1".into(),
            session_id: Some("old".into()),
        };
        assert_eq!(run(&mut state, explicit.clone(), &mut seq).len(), 1);
        assert_eq!(state.agent(&id()).unwrap().session_id, Some("old".into()));
        assert_eq!(state.agent(&id()).unwrap().excluded_session_id, None);
        assert_noop(&mut state, explicit, &mut seq);
    }
}

#[test]
fn agent_matrix_and_repeated_facts() {
    let (mut state, mut seq) = setup();
    assert_eq!(run(&mut state, up(), &mut seq).len(), 1);
    assert!(state.decide(&up(), time()).is_err());
    assert!(state
        .decide(&OrgCommand::Clear { agent_id: id() }, time())
        .is_err());
    assert!(state.decide(&bound("s1"), time()).is_err());
    assert_eq!(run(&mut state, pane(), &mut seq).len(), 1);
    assert!(run(&mut state, pane(), &mut seq).is_empty());
    assert_eq!(run(&mut state, bound("s1"), &mut seq).len(), 1);
    assert!(run(&mut state, bound("s1"), &mut seq).is_empty());
    assert!(state.decide(&up(), time()).is_err());
    // Manual clear/new session can bind while already ready.
    assert_eq!(run(&mut state, bound("s2"), &mut seq).len(), 1);
    assert_eq!(
        run(&mut state, OrgCommand::Clear { agent_id: id() }, &mut seq).len(),
        1
    );
    assert!(state.decide(&up(), time()).is_err());
    assert!(run(
        &mut state,
        OrgCommand::SessionLost { agent_id: id() },
        &mut seq
    )
    .is_empty());
    run(&mut state, bound("s3"), &mut seq);
    assert_eq!(
        run(
            &mut state,
            OrgCommand::SessionLost { agent_id: id() },
            &mut seq
        )
        .len(),
        1
    );
    assert!(run(
        &mut state,
        OrgCommand::SessionLost { agent_id: id() },
        &mut seq
    )
    .is_empty());
    let stuck = OrgCommand::Stuck {
        agent_id: id(),
        reason: "late".into(),
    };
    assert_eq!(run(&mut state, stuck.clone(), &mut seq).len(), 1);
    assert!(run(&mut state, stuck, &mut seq).is_empty());
    assert!(state.decide(&up(), time()).is_err());
    assert_eq!(run(&mut state, bound("late-session"), &mut seq).len(), 1);
    let down = OrgCommand::Down {
        agent_id: id(),
        reason: "stop".into(),
    };
    assert_eq!(run(&mut state, down.clone(), &mut seq).len(), 1);
    assert!(run(&mut state, down, &mut seq).is_empty());
    assert!(state.decide(&bound("no"), time()).is_err());
    assert!(state
        .decide(
            &OrgCommand::Stuck {
                agent_id: id(),
                reason: "no".into()
            },
            time()
        )
        .is_err());
    assert!(run(
        &mut state,
        OrgCommand::SessionLost { agent_id: id() },
        &mut seq
    )
    .is_empty());
    assert_eq!(run(&mut state, up(), &mut seq).len(), 1);
}

#[test]
fn adopt_pane_ownership_and_respawn() {
    let (mut state, mut seq) = setup();
    let adopt = OrgCommand::Adopt {
        agent_id: id(),
        pane: "%1".into(),
        session_id: Some("s1".into()),
    };
    assert_eq!(run(&mut state, adopt.clone(), &mut seq).len(), 1);
    assert!(run(&mut state, adopt, &mut seq).is_empty());
    for command in [
        OrgCommand::Adopt {
            agent_id: "p/other".parse().unwrap(),
            pane: "%1".into(),
            session_id: None,
        },
        OrgCommand::Adopt {
            agent_id: id(),
            pane: "%2".into(),
            session_id: None,
        },
    ] {
        assert!(state.decide(&command, time()).is_err());
    }
    assert!(run(&mut state, pane(), &mut seq).is_empty());
    let events = run(
        &mut state,
        OrgCommand::Respawn {
            agent_id: id(),
            role: role(),
            reason: "again".into(),
        },
        &mut seq,
    );
    assert_eq!(
        events.iter().map(OrgEvent::kind).collect::<Vec<_>>(),
        ["agent.down", "agent.up_requested"]
    );
    assert_eq!(state.agent(&id()).unwrap().status, AgentStatus::Starting);
}

#[test]
fn project_move_unknown_references_invalid_ids_and_role_mismatch() {
    let (mut state, mut seq) = setup();
    assert!(run(
        &mut state,
        OrgCommand::RegisterProject {
            project_id: "p".parse().unwrap(),
            root: "/p".into(),
            move_root: false,
        },
        &mut seq
    )
    .is_empty());
    assert_eq!(
        run(
            &mut state,
            OrgCommand::RegisterProject {
                project_id: "p".parse().unwrap(),
                root: "/renamed".into(),
                move_root: true,
            },
            &mut seq
        )
        .len(),
        1
    );
    for command in [
        OrgCommand::Up {
            agent_id: "unknown/lead".parse().unwrap(),
            role: role(),
        },
        OrgCommand::Up {
            agent_id: "p/different".parse().unwrap(),
            role: role(),
        },
        OrgCommand::Clear { agent_id: id() },
        OrgCommand::SendMessage(content(Address::Agent(id()))),
        OrgCommand::SendMessage(content(Address::System)),
        OrgCommand::Up {
            agent_id: AgentId {
                project: "p".parse().unwrap(),
                name: "BAD".into(),
            },
            role: role(),
        },
    ] {
        let before = state.clone();
        assert!(state.decide(&command, time()).is_err());
        assert_eq!(state, before);
    }
}

#[test]
fn delivery_policy_and_read_ownership() {
    let (mut state, mut seq) = setup();
    run(&mut state, up(), &mut seq);
    run(
        &mut state,
        OrgCommand::SendMessage(content(Address::Agent(id()))),
        &mut seq,
    );
    let mid = MessageId::new(seq);
    let delivered = OrgCommand::Delivered {
        message_id: mid,
        session_id: "s1".into(),
        confirmation: "token".into(),
    };
    assert_eq!(state.decide(&delivered, time()).unwrap().len(), 1);
    let read = OrgCommand::ReadMessages {
        who: Address::Agent(id()),
        message_ids: vec![mid],
    };
    assert_eq!(
        state.decide(&read, time()).unwrap(),
        vec![
            OrgEvent::MessageDeliveryFailed {
                message_id: mid,
                reason: "recipient acknowledged; automatic delivery cancelled".into(),
                final_failure: true,
            },
            OrgEvent::MessageRead {
                message_ids: vec![mid]
            },
        ]
    );
    let attempt = OrgCommand::DeliveryAttempted {
        message_id: mid,
        attempt: 1,
        pane: "%1".into(),
    };
    assert_eq!(run(&mut state, attempt.clone(), &mut seq).len(), 1);
    assert!(run(&mut state, attempt.clone(), &mut seq).is_empty());
    let failure = OrgCommand::DeliveryFailed {
        message_id: mid,
        reason: "retry".into(),
        final_failure: false,
    };
    assert_eq!(run(&mut state, failure.clone(), &mut seq).len(), 1);
    assert!(run(&mut state, failure, &mut seq).is_empty());
    assert!(state.decide(&attempt, time()).is_err()); // previous attempt cannot be reused
    run(
        &mut state,
        OrgCommand::DeliveryAttempted {
            message_id: mid,
            attempt: 2,
            pane: "%1".into(),
        },
        &mut seq,
    );
    assert_eq!(run(&mut state, delivered.clone(), &mut seq).len(), 1);
    assert!(run(&mut state, delivered, &mut seq).is_empty());
    assert!(state
        .decide(
            &OrgCommand::ReadMessages {
                who: Address::Owner,
                message_ids: vec![mid]
            },
            time()
        )
        .is_err());
    assert_eq!(run(&mut state, read.clone(), &mut seq).len(), 1);
    assert!(run(&mut state, read, &mut seq).is_empty());
}

// A stale retry must not reopen a cancelled delivery, even before the read fold.
#[test]
fn read_messages_cancels_open_deliveries_before_read() {
    let (mut state, mut seq) = setup();
    run(&mut state, up(), &mut seq);
    let [queued, retry, active, received, failed] = std::array::from_fn(|_| {
        run(
            &mut state,
            OrgCommand::SendMessage(content(Address::Agent(id()))),
            &mut seq,
        );
        MessageId::new(seq)
    });
    for message_id in [retry, active] {
        run(
            &mut state,
            OrgCommand::DeliveryAttempted {
                message_id,
                attempt: 1,
                pane: "%1".into(),
            },
            &mut seq,
        );
    }
    for (message_id, final_failure) in [(retry, false), (failed, true)] {
        run(
            &mut state,
            OrgCommand::DeliveryFailed {
                message_id,
                reason: "original problem".into(),
                final_failure,
            },
            &mut seq,
        );
    }
    let receipt = |message_id| OrgCommand::Delivered {
        message_id,
        session_id: "s1".into(),
        confirmation: "prompt_submit_exact_reference".into(),
    };
    run(&mut state, receipt(received), &mut seq);
    let message_ids = vec![queued, retry, active, queued, received, failed];
    let read = OrgCommand::ReadMessages {
        who: Address::Agent(id()),
        message_ids: message_ids.clone(),
    };
    let cancel = |message_id, reason: &str| OrgEvent::MessageDeliveryFailed {
        message_id,
        reason: reason.into(),
        final_failure: true,
    };
    let possible = "recipient acknowledged; automatic delivery cancelled; a prior attempt may still finish or have left a draft";
    let expected = vec![
        cancel(
            queued,
            "recipient acknowledged; automatic delivery cancelled",
        ),
        cancel(retry, possible),
        cancel(active, possible),
        OrgEvent::MessageRead { message_ids },
    ];
    assert_eq!(state.decide(&read, time()).unwrap(), expected);
    for event in &expected {
        seq += 1;
        state.apply(seq, time(), event).unwrap();
        if let OrgEvent::MessageDeliveryFailed { message_id, .. } = event {
            let m = state.message(message_id).unwrap();
            assert_eq!(m.status, DeliveryStatus::Failed);
            assert!(m.read_at.is_none());
            assert_noop(
                &mut state,
                OrgCommand::DeliveryAttempted {
                    message_id: *message_id,
                    attempt: 2,
                    pane: "%1".into(),
                },
                &mut seq,
            );
            assert_noop(
                &mut state,
                OrgCommand::DeliveryFailed {
                    message_id: *message_id,
                    reason: "late retry".into(),
                    final_failure: false,
                },
                &mut seq,
            );
        }
    }
    for (mid, attempt) in [(queued, None), (retry, Some(1)), (active, Some(1))] {
        let row = state.message(&mid).unwrap();
        assert_eq!(row.read_at, Some(time()));
        assert_eq!(row.last_attempt.as_ref().map(|a| a.attempt), attempt);
        assert!(row.receipt.is_none());
    }
    assert_eq!(
        state.message(&failed).unwrap().failure.as_deref(),
        Some("original problem")
    );
    assert_eq!(
        state.message(&received).unwrap().status,
        DeliveryStatus::Delivered
    );
    assert_noop(&mut state, read, &mut seq);
    run(&mut state, receipt(active), &mut seq);
    let late = state.message(&active).unwrap();
    assert_eq!(late.status, DeliveryStatus::Delivered);
    assert_eq!(late.read_at, Some(time()));
    assert!(late.receipt.is_some());
}

fn notice(source_message_id: MessageId, reason: &str) -> MessageContent {
    MessageContent {
        from: Address::System,
        kind: MessageKind::Note,
        delivery_notice: Some(DeliveryNotice {
            source_message_id,
            reason: reason.into(),
        }),
        ..content(Address::Owner)
    }
}

#[test]
fn notice_key_is_source_and_reason() {
    let (mut state, mut seq) = setup();
    let mut sources = Vec::new();
    for _ in 0..2 {
        run(
            &mut state,
            OrgCommand::SendMessage(content(Address::Owner)),
            &mut seq,
        );
        sources.push(MessageId::new(seq));
    }
    let first = notice(sources[0], "focus");
    assert_eq!(
        run(&mut state, OrgCommand::SendMessage(first.clone()), &mut seq).len(),
        1
    );
    let first_note = MessageId::new(seq);
    let changed = MessageContent {
        body: "Different diagnostics and followers: m99".into(),
        attachments: vec!["different.md".into()],
        task: Some("t99".parse().unwrap()),
        ..first.clone()
    };
    assert_noop(&mut state, OrgCommand::SendMessage(changed), &mut seq);
    assert_eq!(state.message(&first_note).unwrap().content, first);
    for note in [
        notice(sources[0], "blocked_draft"),
        notice(sources[1], "focus"),
    ] {
        assert_eq!(
            run(&mut state, OrgCommand::SendMessage(note), &mut seq).len(),
            1
        );
    }
    assert_eq!(
        state
            .messages()
            .filter(|m| m.content.delivery_notice.is_some())
            .count(),
        3
    );
    let unkeyed = MessageContent {
        delivery_notice: None,
        ..first
    };
    for _ in 0..2 {
        assert_eq!(
            run(
                &mut state,
                OrgCommand::SendMessage(unkeyed.clone()),
                &mut seq
            )
            .len(),
            1
        );
    }
}

#[test]
fn notice_validation_is_atomic() {
    let (mut state, mut seq) = setup();
    run(&mut state, up(), &mut seq);
    run(
        &mut state,
        OrgCommand::SendMessage(content(Address::Owner)),
        &mut seq,
    );
    let source = MessageId::new(seq);
    let valid = notice(source, "focus");
    run(&mut state, OrgCommand::SendMessage(valid.clone()), &mut seq);
    // Invalid requests must refuse even when their key was already committed.
    let mut invalid = vec![
        notice(MessageId::new(999), "focus"),
        notice(source, ""),
        MessageContent {
            from: Address::Owner,
            ..valid.clone()
        },
        MessageContent {
            from: Address::Agent(id()),
            ..valid.clone()
        },
        MessageContent {
            to: Address::Agent(id()),
            ..valid.clone()
        },
        MessageContent {
            to: Address::System,
            ..valid.clone()
        },
    ];
    for kind in [
        MessageKind::Brief,
        MessageKind::Report,
        MessageKind::Question,
        MessageKind::Answer,
        MessageKind::Review,
    ] {
        invalid.push(MessageContent {
            kind,
            ..valid.clone()
        });
    }
    let before = state.clone();
    let outcomes: Vec<_> = invalid
        .into_iter()
        .map(|note| {
            let refused = state
                .decide(&OrgCommand::SendMessage(note.clone()), time())
                .is_err();
            (note, refused)
        })
        .collect();
    assert!(outcomes.iter().all(|(_, refused)| *refused), "{outcomes:?}");
    assert_eq!(state, before);
}

#[test]
fn serialized_notice_requests_write_once() {
    // Both commands are pending before the writer starts. Exercise each serial
    // order, with the first append applied before the second command decides.
    for reverse in [false, true] {
        let (mut state, mut seq) = setup();
        run(
            &mut state,
            OrgCommand::SendMessage(content(Address::Owner)),
            &mut seq,
        );
        let source = MessageId::new(seq);
        let mut pending = [
            notice(source, "focus"),
            MessageContent {
                body: "Changed diagnostics".into(),
                ..notice(source, "focus")
            },
        ];
        if reverse {
            pending.reverse();
        }
        let expected = pending[0].clone();
        let mut appended = Vec::new();
        for note in pending {
            appended.extend(run(&mut state, OrgCommand::SendMessage(note), &mut seq));
        }
        assert_eq!(appended, [OrgEvent::MessageSent(expected)]);
        assert_eq!(
            state
                .messages()
                .filter(|m| m.content.delivery_notice.is_some())
                .count(),
            1
        );
    }
}

#[test]
fn failed_notice_append_does_not_claim_key() {
    let (mut state, mut seq) = setup();
    run(
        &mut state,
        OrgCommand::SendMessage(content(Address::Owner)),
        &mut seq,
    );
    let command = OrgCommand::SendMessage(notice(MessageId::new(seq), "unobserved"));
    let before = state.clone();
    let refused_append = state.decide(&command, time()).unwrap();
    assert_eq!(refused_append.len(), 1);
    drop(refused_append); // The writer did not append: there is no fact to apply.
    assert_eq!(state, before);
    assert_eq!(run(&mut state, command.clone(), &mut seq).len(), 1);
    assert_noop(&mut state, command, &mut seq);
}

#[test]
fn summary_deduplicates_but_control_audits_always_emit() {
    let (mut state, mut seq) = setup();
    run(&mut state, up(), &mut seq);
    let command = OrgCommand::RecordSummary(summary());
    assert_eq!(run(&mut state, command.clone(), &mut seq).len(), 1);
    assert!(run(&mut state, command, &mut seq).is_empty());
    let audit = ControlAudit {
        pane: "%unmanaged".into(),
        action: serde_json::json!("audit"),
    };
    for command in [
        OrgCommand::ControlSent(audit.clone()),
        OrgCommand::ControlSpawned(audit.clone()),
        OrgCommand::ControlKilled(audit),
    ] {
        assert_eq!(run(&mut state, command.clone(), &mut seq).len(), 1);
        assert_eq!(run(&mut state, command, &mut seq).len(), 1);
    }
}
