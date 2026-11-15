//! Pure projections and restart cuts on committed files; no model or waits.
use super::*;
use atm_org::{MessageId, OrgEvent, ProvisionalChoice};
use std::path::Path;

fn now() -> DateTime<Utc> {
    "2026-01-15T12:00:00Z".parse().unwrap()
}
fn caller() -> CallContext {
    CallContext {
        client_id: crate::server::methods::CallerId("decision-test".into()),
        from_pane: None,
    }
}
fn request(requester: Address) -> DecisionRequest {
    DecisionRequest {
        requester,
        kind: DecisionKind::Plan,
        title: "Choose".into(),
        context: "context".into(),
        files: vec![],
        options: vec!["yes".into(), "no".into()],
        recommended: Some("yes".into()),
        may_be_undone: true,
        changes_intent: true,
        deadline: None,
    }
}
fn commands() -> [OrgCommand; 3] {
    [
        OrgCommand::RegisterProject {
            project_id: "p".parse().unwrap(),
            root: "/project".into(),
            move_root: false,
        },
        OrgCommand::Adopt {
            agent_id: "p/worker".parse().unwrap(),
            pane: "%7".into(),
            session_id: None,
        },
        OrgCommand::Decision(DecisionCommand::Request(request(
            "p/worker".parse().unwrap(),
        ))),
    ]
}
fn pending() -> OrgState {
    let mut state = OrgState::default();
    for (seq, command) in commands().into_iter().enumerate() {
        for event in state.decide(&command, now()).unwrap() {
            state.apply(seq as u64 + 1, now(), &event).unwrap();
        }
    }
    state
}
fn answer() -> DecisionResolution {
    DecisionResolution {
        decision_id: DecisionId::new(3),
        option: "yes".into(),
        by: Address::Owner,
        note: Some("go".into()),
        standing_rule: true,
    }
}
fn content() -> MessageContent {
    MessageContent {
        from: Address::System,
        to: "p/worker".parse().unwrap(),
        kind: MessageKind::Answer,
        body: "Decision d3 resolved: yes\nNote: go\nStanding rule: yes".into(),
        attachments: vec![],
        task: None,
    }
}

// Break caught: interpreting any retained resolution as final, or deduplicating
// only delivered/unread messages rather than the durable enqueue fact.
#[test]
fn decision_answer_mapping_uses_status_and_exact_durable_message() {
    let mut state = pending();
    assert!(missing_resolution_messages(&state).is_empty());
    state
        .apply(4, now(), &OrgEvent::DecisionResolved(answer().into()))
        .unwrap();
    assert_eq!(missing_resolution_messages(&state), [content()]);
    for event in [
        OrgEvent::DecisionTakenProvisionally(ProvisionalChoice {
            decision_id: DecisionId::new(3),
            option: "yes".into(),
            by: Address::System,
        }),
        OrgEvent::DecisionExpired {
            decision_id: DecisionId::new(3),
        },
        OrgEvent::DecisionWithdrawn(DecisionWithdrawal {
            decision_id: DecisionId::new(3),
            by: Address::Owner,
            reason: None,
        }),
    ] {
        let mut unusual = state.clone();
        unusual.apply(5, now(), &event).unwrap();
        assert!(unusual
            .decision(&DecisionId::new(3))
            .unwrap()
            .resolution
            .is_some());
        assert!(missing_resolution_messages(&unusual).is_empty());
    }
    state
        .apply(5, now(), &OrgEvent::MessageSent(content()))
        .unwrap();
    state
        .apply(
            6,
            now(),
            &OrgEvent::MessageDeliveryFailed {
                message_id: MessageId::new(5),
                reason: "held".into(),
                final_failure: true,
            },
        )
        .unwrap();
    state
        .apply(
            7,
            now(),
            &OrgEvent::MessageRead {
                message_ids: vec![MessageId::new(5)],
            },
        )
        .unwrap();
    assert!(missing_resolution_messages(&state).is_empty());
    let mut system = OrgState::default();
    system
        .apply(
            1,
            now(),
            &OrgEvent::DecisionRequested(request(Address::System)),
        )
        .unwrap();
    let mut resolution = answer();
    resolution.decision_id = DecisionId::new(1);
    system
        .apply(2, now(), &OrgEvent::DecisionResolved(resolution.into()))
        .unwrap();
    assert!(
        missing_resolution_messages(&system).is_empty(),
        "system has no inbox"
    );
}

// Break caught: truncating a row, ignoring the byte cap, or revalidating a
// historical Unknown kind as though it were a new request.
#[test]
fn decision_pages_bound_complete_rows_and_keep_unknown_replay_readable() {
    let mut state = pending();
    for seq in 4..7 {
        let mut value = request("p/worker".parse().unwrap());
        value.kind = DecisionKind::Unknown;
        value.context = "x".repeat(200_000);
        state
            .apply(seq, now(), &OrgEvent::DecisionRequested(value))
            .unwrap();
    }
    let params = ListParams {
        statuses: None,
        after: Some(DecisionId::new(3)),
        limit: 100,
        count_only: false,
    };
    let page = list(&state, &Address::Owner, &params).unwrap();
    assert_eq!(page.decisions.len(), 2);
    assert_eq!(page.pending_count, 4);
    assert_eq!(page.next_after, Some(DecisionId::new(5)));
    assert!(serde_json::to_vec(&page).unwrap().len() <= PAGE_BYTES);
    assert_eq!(
        serde_json::to_value(&page.decisions[0]).unwrap()["request"]["kind"],
        "unknown"
    );
    assert_eq!(page.decisions[0].decision.request.context.len(), 200_000);
    let mut huge = request(Address::System);
    huge.context = "x".repeat(PAGE_BYTES);
    state
        .apply(7, now(), &OrgEvent::DecisionRequested(huge))
        .unwrap();
    let params = ListParams {
        after: Some(DecisionId::new(6)),
        ..params
    };
    assert!(matches!(
        list(&state, &Address::Owner, &params),
        Err(DecisionError::Refused(_))
    ));
    assert!(matches!(
        bounded(GetResult {
            decision: entry(&state, DecisionId::new(7)).unwrap()
        }),
        Err(DecisionError::Refused(_))
    ));
}

fn copy_log(from: &Path, to: &Path) {
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let dest = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            std::fs::create_dir_all(&dest).unwrap();
            copy_log(&entry.path(), &dest);
        } else {
            std::fs::copy(entry.path(), dest).unwrap();
        }
    }
}
async fn guarded<T>(work: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(60), work)
        .await
        .unwrap()
}
async fn seed(dir: &Path, enqueue: bool) -> (OrgHandle, CancellationToken) {
    let cancel = CancellationToken::new();
    let org = OrgHandle::open(dir.into(), cancel.clone()).await;
    for command in commands() {
        org.execute(command, now()).await.unwrap();
    }
    org.execute(
        OrgCommand::Decision(DecisionCommand::Resolve(answer())),
        now(),
    )
    .await
    .unwrap();
    if enqueue {
        org.execute(OrgCommand::SendMessage(content()), now())
            .await
            .unwrap();
    }
    (org, cancel)
}
async fn recover(dir: &Path) -> (OrgHandle, CancellationToken) {
    let cancel = CancellationToken::new();
    let org = OrgHandle::open(dir.into(), cancel.clone()).await;
    let bridge_cancel = cancel.child_token();
    let (handle, task) = spawn_decisions(
        org.clone(),
        MessageClock::new(tokio::time::Instant::now(), now()),
        bridge_cancel.clone(),
    );
    guarded(handle.initialized()).await.unwrap();
    assert_eq!(org.snapshot().await.unwrap().messages().count(), 1);
    let result = handle
        .get(
            GetParams {
                id: DecisionId::new(3),
            },
            caller(),
        )
        .await
        .unwrap();
    assert_eq!(result.decision.decision.resolution, Some(answer()));
    assert_eq!(result.decision.decision.closed_at, Some(now()));
    assert_eq!(
        org.snapshot()
            .await
            .unwrap()
            .messages()
            .next()
            .unwrap()
            .content,
        content()
    );
    // Cancel the bridge without cancelling the actor; a snapshot supplies a
    // committed source for the next recovery copy (same convention as messages).
    bridge_cancel.cancel();
    guarded(task).await.unwrap();
    (org, cancel)
}

// The copies contain only acknowledged committed log data, and avoid racing
// detached stream-lock release inside one process. The real daemon restart is
// exercised separately in the shared private-daemon/stand-in rig.
#[tokio::test]
async fn decision_answer_bridge_restart_fills_missing_enqueue_once() {
    let before = tempfile::tempdir().unwrap();
    let after = tempfile::tempdir().unwrap();
    let again = tempfile::tempdir().unwrap();
    let (old, old_cancel) = seed(before.path(), false).await;
    assert_eq!(old.snapshot().await.unwrap().messages().count(), 0);
    copy_log(before.path(), after.path());
    let (new, new_cancel) = recover(after.path()).await;
    let message_id = new.snapshot().await.unwrap().messages().next().unwrap().id;
    copy_log(after.path(), again.path());
    let (last, last_cancel) = recover(again.path()).await;
    assert_eq!(
        last.snapshot().await.unwrap().messages().next().unwrap().id,
        message_id
    );
    old_cancel.cancel();
    new_cancel.cancel();
    last_cancel.cancel();
}

#[tokio::test]
async fn answer_bridge_restart_after_enqueue_does_not_duplicate() {
    let before = tempfile::tempdir().unwrap();
    let after = tempfile::tempdir().unwrap();
    let (old, old_cancel) = seed(before.path(), true).await;
    let cursor = old.status().unwrap();
    copy_log(before.path(), after.path());
    let (new, new_cancel) = recover(after.path()).await;
    assert_eq!(
        new.status().unwrap(),
        cursor,
        "a durable enqueue is already complete"
    );
    old_cancel.cancel();
    new_cancel.cancel();
}

// Break caught: a snapshot or accepted enqueue timing out permanently disables
// the bridge. Hold only the response boundary; the real actor still processes
// the accepted request before any retry, including a late SendMessage commit.
async fn retry_timeout(execute_timeout: bool) {
    let dir = tempfile::tempdir().unwrap();
    let (org, cancel) = seed(dir.path(), false).await;
    let (requests, mut receive) = tokio::sync::mpsc::channel(64);
    let (entered, entry) = tokio::sync::oneshot::channel();
    let (timed_out, timeout) = tokio::sync::oneshot::channel();
    let (release, released) = tokio::sync::oneshot::channel();
    let real_requests = org.requests.clone();
    let proxy = tokio::spawn(async move {
        let mut entered = Some(entered);
        let mut timed_out = Some(timed_out);
        let mut released = Some(released);
        while let Some(mut request) = receive.recv().await {
            let target = match &request {
                crate::org::Request::Snapshot(_) => !execute_timeout,
                crate::org::Request::Execute { command, .. } => {
                    execute_timeout && matches!(**command, OrgCommand::SendMessage(_))
                }
            };
            if target && entered.is_some() {
                entered.take().unwrap().send(()).unwrap();
                match &mut request {
                    crate::org::Request::Snapshot(reply) => reply.closed().await,
                    crate::org::Request::Execute { reply, .. } => reply.closed().await,
                }
                timed_out.take().unwrap().send(()).unwrap();
                released.take().unwrap().await.unwrap();
            }
            real_requests.send(request).await.unwrap();
        }
    });
    let mut bridged_org = org.clone();
    bridged_org.requests = requests;
    let bridge_cancel = cancel.child_token();
    // No disk/socket I/O occurs while time is paused: only the held
    // in-memory response and OrgHandle's actual 5/6-second deadline.
    tokio::time::pause();
    let (handle, task) = spawn_decisions(
        bridged_org,
        MessageClock::new(tokio::time::Instant::now(), now()),
        bridge_cancel.clone(),
    );
    entry.await.unwrap();
    tokio::time::advance(Duration::from_secs(7)).await;
    timeout.await.unwrap();
    tokio::time::resume();
    release.send(()).unwrap();
    guarded(handle.initialized()).await.unwrap();
    let state = org.snapshot().await.unwrap();
    assert_eq!(
        state.messages().count(),
        1,
        "execute timeout: {execute_timeout}"
    );
    assert_eq!(state.messages().next().unwrap().content, content());

    let agent = CallContext {
        from_pane: Some(crate::server::methods::CallerPane("%7".into())),
        ..caller()
    };
    let params = RequestParams {
        kind: DecisionKind::Plan,
        title: "Choose again".into(),
        context: String::new(),
        files: vec![],
        options: vec!["yes".into(), "no".into()],
        recommended: None,
        may_be_undone: true,
        changes_intent: true,
        deadline: None,
    };
    let withdrawn = handle.request(params.clone(), agent.clone()).await.unwrap();
    assert!(
        handle
            .withdraw(
                WithdrawParams {
                    id: withdrawn.decision_id,
                    reason: None
                },
                agent.clone(),
            )
            .await
            .unwrap()
            .changed
    );
    let next = handle.request(params, agent).await.unwrap();
    let mut messages = org
        .watch(vec!["msg.sent".into()], org.status().unwrap())
        .unwrap();
    assert!(
        handle
            .resolve(
                ResolveParams {
                    id: next.decision_id,
                    option: "no".into(),
                    note: None,
                    standing_rule: false,
                },
                caller(),
            )
            .await
            .unwrap()
            .changed
    );
    let sent = guarded(messages.next()).await.unwrap().unwrap();
    assert_eq!(
        sent.body["body"],
        format!(
            "Decision {} resolved: no\nStanding rule: no",
            next.decision_id
        )
    );
    let state = org.snapshot().await.unwrap();
    assert_eq!(state.messages().count(), 2);
    assert_eq!(
        state.messages().filter(|m| m.content == content()).count(),
        1
    );
    bridge_cancel.cancel();
    guarded(task).await.unwrap();
    drop(handle);
    guarded(proxy).await.unwrap();
    cancel.cancel();
}

#[tokio::test]
async fn decision_bridge_retries_snapshot_timeout_without_duplicates() {
    retry_timeout(false).await;
}

#[tokio::test]
async fn decision_bridge_retries_execute_timeout_without_duplicates() {
    retry_timeout(true).await;
}

#[tokio::test]
async fn decision_bridge_failure_refuses_mutations_but_retains_reads() {
    let dir = tempfile::tempdir().unwrap();
    let (org, cancel) = seed(dir.path(), false).await;
    let clock = MessageClock::new(
        tokio::time::Instant::now() + Duration::from_secs(3600),
        now(),
    );
    let (handle, task) = spawn_decisions(org.clone(), clock, cancel.child_token());
    assert!(matches!(
        guarded(handle.initialized()).await,
        Err(DecisionError::Unavailable(_))
    ));
    assert_eq!(
        handle
            .get(
                GetParams {
                    id: DecisionId::new(3)
                },
                caller()
            )
            .await
            .unwrap()
            .decision
            .decision
            .status,
        DecisionStatus::Resolved
    );
    assert!(matches!(
        handle
            .resolve(
                ResolveParams {
                    id: DecisionId::new(3),
                    option: "yes".into(),
                    note: Some("go".into()),
                    standing_rule: true
                },
                caller()
            )
            .await,
        Err(DecisionError::Unavailable(_))
    ));
    guarded(task).await.unwrap();
    cancel.cancel();
}
