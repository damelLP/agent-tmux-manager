//! Spoken-answer relay policy. Synthetic organization facts, no harness or I/O.
#![allow(clippy::unwrap_used, clippy::expect_used)] // test builders only
#[path = "support/decision.rs"]
mod decision_support;
mod support;
use atm_org::*;
use support::time;

fn agent(text: &str) -> AgentId {
    text.parse().unwrap()
}
fn seeded() -> OrgState {
    let mut state = OrgState::default();
    for (seq, event) in [
        OrgEvent::ProjectRegistered {
            project_id: "p".parse().unwrap(),
            root: "/project".into(),
        },
        OrgEvent::AgentAdopted {
            agent_id: agent("p/lead"),
            pane: "%1".into(),
            session_id: None,
        },
        OrgEvent::AgentAdopted {
            agent_id: agent("p/builder"),
            pane: "%2".into(),
            session_id: None,
        },
    ]
    .into_iter()
    .enumerate()
    {
        state.apply(seq as u64 + 1, time(), &event).unwrap();
    }
    state
}
fn set_relay(by: Address, slot: Option<&str>) -> OrgCommand {
    OrgCommand::Owner(OwnerCommand::SetRelay {
        by,
        agent: slot.map(agent),
    })
}
fn decide(state: &OrgState, command: &OrgCommand) -> Result<Vec<OrgEvent>, Refusal> {
    let before = state.clone();
    let result = state.decide(command, time());
    assert_eq!(*state, before, "decide must not mutate: {command:?}");
    result
}

const KINDS: [DecisionKind; 7] = [
    DecisionKind::Action,
    DecisionKind::Hire,
    DecisionKind::Budget,
    DecisionKind::Policy,
    DecisionKind::Plan,
    DecisionKind::Dialog,
    DecisionKind::Unknown,
];
const STATUSES: [DecisionStatus; 6] = [
    DecisionStatus::Pending,
    DecisionStatus::TakenProvisionally,
    DecisionStatus::Resolved,
    DecisionStatus::Overruled,
    DecisionStatus::Expired,
    DecisionStatus::Withdrawn,
];
fn relay(option: &str, by: &str) -> OrgCommand {
    OrgCommand::Decision(DecisionCommand::Relay {
        decision_id: DecisionId::new(10),
        option: option.into(),
        by: agent(by),
    })
}
fn requested(request: DecisionRequest, status: DecisionStatus, slot: Option<&str>) -> OrgState {
    let mut state = seeded();
    state
        .apply(
            8,
            time(),
            &OrgEvent::OwnerRelaySet(OwnerRelaySet {
                by: Address::Owner,
                agent: slot.map(agent),
            }),
        )
        .unwrap();
    state
        .apply(10, time(), &OrgEvent::DecisionRequested(request))
        .unwrap();
    if matches!(
        status,
        DecisionStatus::TakenProvisionally | DecisionStatus::Overruled
    ) {
        state
            .apply(
                11,
                time(),
                &OrgEvent::DecisionTakenProvisionally(ProvisionalChoice {
                    decision_id: DecisionId::new(10),
                    option: "a".into(),
                    by: Address::System,
                    relayed_by: None,
                }),
            )
            .unwrap();
    }
    let answer = DecisionResolution {
        decision_id: DecisionId::new(10),
        option: "b".into(),
        by: Address::Owner,
        note: None,
        standing_rule: false,
    };
    let closed = match status {
        DecisionStatus::Resolved => Some(OrgEvent::DecisionResolved(answer.into())),
        DecisionStatus::Overruled => Some(OrgEvent::DecisionOverruled(answer.into())),
        DecisionStatus::Expired => Some(OrgEvent::DecisionExpired {
            decision_id: DecisionId::new(10),
        }),
        DecisionStatus::Withdrawn => Some(OrgEvent::DecisionWithdrawn(DecisionWithdrawal {
            decision_id: DecisionId::new(10),
            by: Address::Owner,
            reason: None,
        })),
        _ => None,
    };
    if let Some(event) = closed {
        state.apply(12, time(), &event).unwrap();
    }
    state
}

// Break caught: any missing authorization, kind, class, state or listed-option
// gate, or incorrectly reusing the away table's recommendation/presence gate.
#[test]
fn relay_table_covers_kinds_classes_states_options_and_slots() {
    let mut cases = 0;
    for kind in KINDS {
        // Five independent flags: undone, intent, recommendation, away, self-request.
        for flags in 0u8..32 {
            let mut request = decision_support::request();
            request.kind = kind;
            request.may_be_undone = flags & 1 != 0;
            request.changes_intent = flags & 2 != 0;
            request.recommended = (flags & 4 != 0).then(|| "a".into());
            if flags & 16 != 0 {
                request.requester = Address::Agent(agent("p/lead"));
            }
            let eligible = request.may_be_undone
                && matches!(
                    kind,
                    DecisionKind::Policy | DecisionKind::Plan | DecisionKind::Dialog
                );
            for status in STATUSES {
                for slot in [None, Some("p/lead"), Some("p/builder")] {
                    let mut state = requested(request.clone(), status, slot);
                    if flags & 8 != 0 {
                        state
                            .apply(
                                13,
                                time(),
                                &OrgEvent::OwnerAway(OwnerAway {
                                    by: Address::Owner,
                                    stacking_caps: [].into(),
                                }),
                            )
                            .unwrap();
                    }
                    for option in ["a", "b", "outside"] {
                        let decision_id = DecisionId::new(10);
                        let want = if slot != Some("p/lead") {
                            Err(DecisionRefusal::NotAllowed {
                                decision_id,
                                who: Address::Agent(agent("p/lead")),
                                operation: "relay",
                            }
                            .into())
                        } else {
                            match status {
                                DecisionStatus::Pending if !eligible => {
                                    Err(DecisionRefusal::MustWait { decision_id }.into())
                                }
                                DecisionStatus::Pending if option == "outside" => {
                                    Err(DecisionRefusal::UnknownOption {
                                        decision_id,
                                        option: option.into(),
                                    }
                                    .into())
                                }
                                DecisionStatus::Pending => {
                                    Ok(vec![OrgEvent::DecisionTakenProvisionally(
                                        ProvisionalChoice {
                                            decision_id,
                                            option: option.into(),
                                            by: Address::Owner,
                                            relayed_by: Some(agent("p/lead")),
                                        },
                                    )])
                                }
                                DecisionStatus::TakenProvisionally if option == "a" => Ok(vec![]),
                                DecisionStatus::TakenProvisionally => {
                                    Err(DecisionRefusal::AlreadyTaken {
                                        decision_id,
                                        option: "a".into(),
                                    }
                                    .into())
                                }
                                _ => Err(DecisionRefusal::InvalidState {
                                    decision_id,
                                    state: status,
                                    operation: "relay",
                                }
                                .into()),
                            }
                        };
                        assert_eq!(
                            decide(&state, &relay(option, "p/lead")),
                            want,
                            "{kind:?} flags={flags}, {status:?}, {slot:?}, {option}"
                        );
                        cases += 1;
                    }
                }
            }
        }
    }
    assert_eq!(cases, 12096);
}

// Break caught: adding a kind that always waits in one path but not the other.
#[test]
fn relay_safety_classification_cannot_drift_from_away_step() {
    for kind in KINDS {
        for (undone, intent) in [(true, true), (true, false), (false, true), (false, false)] {
            let mut request = decision_support::request();
            request.kind = kind;
            request.may_be_undone = undone;
            request.changes_intent = intent;
            let state = requested(request, DecisionStatus::Pending, Some("p/lead"));
            let waits = state
                .decision(&DecisionId::new(10))
                .unwrap()
                .away_step(true)
                == AwayStep::Wait(AwayWait::Irreversible);
            assert_eq!(
                matches!(
                    decide(&state, &relay("b", "p/lead")),
                    Err(Refusal::Decision(DecisionRefusal::MustWait { .. }))
                ),
                waits,
                "{kind:?} {undone} {intent}"
            );
        }
    }
}

// Break caught: a retry overwriting a different provisional option or its
// provenance, or accepting a former relay seat after revocation/replacement.
#[test]
fn relay_retry_preserves_all_existing_takers_and_rechecks_authority() {
    for by in [
        Address::System,
        Address::Agent(agent("p/builder")),
        Address::Owner,
    ] {
        for relayed_by in [None, Some(agent("p/builder"))] {
            let mut state = requested(
                decision_support::request(),
                DecisionStatus::Pending,
                Some("p/lead"),
            );
            state
                .apply(
                    11,
                    time(),
                    &OrgEvent::DecisionTakenProvisionally(ProvisionalChoice {
                        decision_id: DecisionId::new(10),
                        option: "a".into(),
                        by: by.clone(),
                        relayed_by,
                    }),
                )
                .unwrap();
            assert_eq!(decide(&state, &relay("a", "p/lead")), Ok(vec![]));
            let refusal = decide(&state, &relay("b", "p/lead")).unwrap_err();
            assert_eq!(
                refusal,
                Refusal::Decision(DecisionRefusal::AlreadyTaken {
                    decision_id: DecisionId::new(10),
                    option: "a".into()
                })
            );
            assert!(refusal.to_string().contains("\"a\""));
            for slot in [None, Some("p/builder")] {
                let events = decide(&state, &set_relay(Address::Owner, slot)).unwrap();
                for event in events {
                    state.apply(12, time(), &event).unwrap();
                }
                assert!(matches!(
                    decide(&state, &relay("a", "p/lead")),
                    Err(Refusal::Decision(DecisionRefusal::NotAllowed { .. }))
                ));
                if slot.is_some() {
                    assert_eq!(decide(&state, &relay("a", "p/builder")), Ok(vec![]));
                }
            }
        }
    }
    let state = requested(
        decision_support::request(),
        DecisionStatus::Pending,
        Some("p/lead"),
    );
    assert_eq!(
        decide(&state, &relay("a", "p/nobody")),
        Err(Refusal::Invariant(ApplyError::UnknownAgent(agent(
            "p/nobody"
        ))))
    );
    assert_eq!(
        decide(
            &state,
            &OrgCommand::Decision(DecisionCommand::Relay {
                decision_id: DecisionId::new(999),
                option: "a".into(),
                by: agent("p/lead"),
            })
        ),
        Err(Refusal::Invariant(ApplyError::UnknownDecision(
            DecisionId::new(999)
        )))
    );
}

// Break caught: broadening designation authority or treating an unauthorized
// repeat/clear as a successful no-op before checking who asked.
#[test]
fn only_owner_sets_relay_and_unknown_targets_are_atomic_refusals() {
    for slot in [None, Some("p/lead")] {
        let mut state = seeded();
        for by in [Address::System, Address::Agent(agent("p/builder"))] {
            assert!(decide(&state, &set_relay(by, slot)).is_err());
        }
        assert_eq!(state.owner().relay, None);
        let nobody = agent("p/nobody");
        assert_eq!(
            decide(&state, &set_relay(Address::Owner, Some("p/nobody"))),
            Err(Refusal::Invariant(ApplyError::UnknownAgent(nobody.clone())))
        );
        let before = state.clone();
        assert_eq!(
            state.apply(
                10,
                time(),
                &OrgEvent::OwnerRelaySet(OwnerRelaySet {
                    by: Address::Owner,
                    agent: Some(nobody),
                })
            ),
            Err(ApplyError::UnknownAgent(agent("p/nobody")))
        );
        assert_eq!(state, before);
    }
}

// Break caught: failing to persist replacement/revocation, writing duplicate
// facts, or accidentally erasing the relay slot with the away switch.
#[test]
fn designation_replace_clear_and_retries_keep_away_state_independent() {
    let mut state = seeded();
    for (seq, slot) in [Some("p/lead"), Some("p/builder"), None]
        .into_iter()
        .enumerate()
    {
        let command = set_relay(Address::Owner, slot);
        let event = OrgEvent::OwnerRelaySet(OwnerRelaySet {
            by: Address::Owner,
            agent: slot.map(agent),
        });
        assert_eq!(decide(&state, &command), Ok(vec![event.clone()]));
        state.apply(seq as u64 + 10, time(), &event).unwrap();
        assert_eq!(state.owner().relay, slot.map(agent));
        assert_eq!(decide(&state, &command), Ok(vec![]));
        for by in [Address::System, Address::Agent(agent("p/lead"))] {
            assert!(decide(&state, &set_relay(by, slot)).is_err());
        }
        state
            .apply(
                20,
                time(),
                &OrgEvent::OwnerAway(OwnerAway {
                    by: Address::Owner,
                    stacking_caps: [("p".parse().unwrap(), 3)].into(),
                }),
            )
            .unwrap();
        state
            .apply(
                21,
                time(),
                &OrgEvent::OwnerBack(OwnerBack { by: Address::Owner }),
            )
            .unwrap();
        assert_eq!(state.owner().relay, slot.map(agent));
        assert_eq!(state.owner().stacking_cap(Some(&"p".parse().unwrap())), 3);
    }
    assert_eq!(decide(&state, &set_relay(Address::Owner, None)), Ok(vec![]));
}

// Break caught: enforcing today's slot/authority during replay, or accepting an
// unknown relay identity and partially mutating a committed aggregate.
#[test]
fn folding_relay_facts_checks_references_not_current_policy() {
    let mut state = seeded();
    state
        .apply(
            9,
            time(),
            &OrgEvent::OwnerRelaySet(OwnerRelaySet {
                by: Address::System,
                agent: Some(agent("p/lead")),
            }),
        )
        .unwrap();
    state
        .apply(
            10,
            time(),
            &OrgEvent::DecisionRequested(decision_support::request()),
        )
        .unwrap();
    let choice = |relayed_by| {
        OrgEvent::DecisionTakenProvisionally(ProvisionalChoice {
            decision_id: DecisionId::new(10),
            option: "b".into(),
            by: Address::Owner,
            relayed_by,
        })
    };
    let before = state.clone();
    assert_eq!(
        state.apply(11, time(), &choice(Some(agent("p/nobody")))),
        Err(ApplyError::UnknownAgent(agent("p/nobody")))
    );
    assert_eq!(state, before);
    // The current slot is lead; a historical builder relay still folds.
    state
        .apply(12, time(), &choice(Some(agent("p/builder"))))
        .unwrap();
    assert_eq!(
        state
            .decision(&DecisionId::new(10))
            .unwrap()
            .provisional
            .as_ref()
            .unwrap()
            .relayed_by,
        Some(agent("p/builder"))
    );
    let legacy = OrgEvent::from_parts(
        "decision.taken_provisionally",
        &serde_json::json!({
            "decision_id":"d10", "option":"a", "by":"system"
        }),
    )
    .unwrap()
    .unwrap();
    state.apply(13, time(), &legacy).unwrap();
    assert_eq!(
        state
            .decision(&DecisionId::new(10))
            .unwrap()
            .provisional
            .as_ref()
            .unwrap()
            .relayed_by,
        None
    );
}
