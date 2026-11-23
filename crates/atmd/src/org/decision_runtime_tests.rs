//! Pure projections and restart cuts on committed files; no model or waits.
use super::*;
use crate::server::methods::{CallerPane, MethodRouter};
use atm_org::{channel_query::decision_post, MessageId, OrgEvent, OwnerCommand, ProvisionalChoice};
use serde_json::{json, Value};
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
        delivery_notice: None,
        from: Address::System,
        to: "p/worker".parse().unwrap(),
        kind: MessageKind::Answer,
        body: "Decision d3 resolved: yes\nNote: go\nStanding rule: yes".into(),
        attachments: vec![],
        task: None,
        post: None,
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
    let mut overruled = pending();
    overruled
        .apply(4, now(), &OrgEvent::DecisionOverruled(answer().into()))
        .unwrap();
    assert_eq!(
        missing_resolution_messages(&overruled),
        [MessageContent {
            body: "Decision d3 overruled: yes\nNote: go\nStanding rule: yes".into(),
            ..content()
        }]
    );
    for event in [
        OrgEvent::DecisionTakenProvisionally(ProvisionalChoice {
            relayed_by: None,
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

// Break caught: subscribing only to decision.resolved leaves an overrule
// unanswered until another resolution. The current-thread runtime's park hook
// is an idle barrier: all actor/bridge I/O here is in-memory, so no elapsed-time
// wait or subsequent event is needed to inspect the completed reaction.
#[test]
fn decision_overrule_wakes_bridge_without_another_decision_event() {
    use std::sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    };
    use tokio::sync::{broadcast, mpsc, oneshot, watch};

    let idle = Arc::new(Mutex::new(None::<oneshot::Sender<()>>));
    let parked = idle.clone();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .on_thread_park(move || {
            if let Some(reply) = parked.lock().unwrap().take() {
                let _ = reply.send(());
            }
        })
        .build()
        .unwrap();
    runtime.block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let (requests, receive) = mpsc::channel(64);
        let (health, status) = watch::channel(Ok(3));
        let (committed, _) = broadcast::channel(1024);
        let cancel = CancellationToken::new();
        let seq = AtomicU64::new(3);
        let actor = tokio::spawn(crate::org::actor::run(
            pending(),
            receive,
            health,
            committed.clone(),
            cancel.clone(),
            move |_| std::future::ready(Ok(seq.fetch_add(1, Ordering::SeqCst) + 1)),
        ));
        let org = OrgHandle {
            requests,
            health: status,
            committed,
            reads: Some(crate::org::watch::ReadAccess {
                reader: crate::org::storage::Reader::start(dir.path().into()).unwrap(),
            }),
        };
        let (handle, bridge) = spawn_decisions(
            org.clone(),
            MessageClock::new(tokio::time::Instant::now(), now()),
            cancel.child_token(),
        );
        guarded(handle.initialized()).await.unwrap();
        org.execute(
            OrgCommand::Decision(DecisionCommand::TakeProvisionally(ProvisionalChoice {
                relayed_by: None,
                decision_id: DecisionId::new(3),
                option: "yes".into(),
                by: Address::System,
            })),
            now(),
        )
        .await
        .unwrap();
        let mut overrule = answer();
        overrule.option = "no".into();
        org.execute(
            OrgCommand::Decision(DecisionCommand::Resolve(overrule)),
            now(),
        )
        .await
        .unwrap();
        let (reply, reached_idle) = oneshot::channel();
        *idle.lock().unwrap() = Some(reply);
        guarded(reached_idle).await.unwrap();
        let state = org.snapshot().await.unwrap();
        // The take has a notice of its own since the away switch. Whether the
        // bridge sent it depends on whether it ran between the two commands.
        // U8: the question's row to the owner is not an answer, and each
        // answer row is today's message plus its reply's `post`.
        let answers: Vec<_> = state
            .messages()
            .filter(|message| message.content.kind != MessageKind::Question)
            .map(|message| MessageContent {
                post: None,
                ..message.content.clone()
            })
            .filter(|content| !content.body.contains("taken provisionally"))
            .collect();
        assert!(
            state
                .messages()
                .filter(|m| m.content.kind != MessageKind::Question)
                .count()
                <= 2
        );
        assert_eq!(
            answers,
            [MessageContent {
                body: "Decision d3 overruled: no\nNote: go\nStanding rule: yes".into(),
                ..content()
            }],
            "overrule must wake the bridge"
        );
        cancel.cancel();
        guarded(bridge).await.unwrap();
        guarded(actor).await.unwrap();
    });
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
    let page = list(&state, &Address::Owner, &BTreeSet::new(), &params).unwrap();
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
        list(&state, &Address::Owner, &BTreeSet::new(), &params),
        Err(DecisionError::Refused(_))
    ));
    assert!(matches!(
        bounded(GetResult {
            decision: entry(&state, DecisionId::new(7)).unwrap(),
            redo: None,
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
                redo_after: None,
                redo_limit: None,
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

async fn reconciled_messages(org: &OrgHandle) -> Vec<MessageContent> {
    let cancel = CancellationToken::new();
    let (handle, task) = spawn_decisions(
        org.clone(),
        MessageClock::new(tokio::time::Instant::now(), now()),
        cancel.clone(),
    );
    guarded(handle.initialized()).await.unwrap();
    let messages = org
        .snapshot()
        .await
        .unwrap()
        .messages()
        .map(|message| message.content.clone())
        .collect();
    cancel.cancel();
    guarded(task).await.unwrap();
    messages
}

// Break caught: treating any earlier answer to a requester as the completion
// marker for all of that requester's decisions.
#[tokio::test]
async fn decision_bridge_sends_two_distinct_answers_to_the_same_requester() {
    let dir = tempfile::tempdir().unwrap();
    let (org, cancel) = seed(dir.path(), true).await;
    org.execute(
        OrgCommand::Decision(DecisionCommand::Request(request(
            "p/worker".parse().unwrap(),
        ))),
        now(),
    )
    .await
    .unwrap();
    org.execute(
        OrgCommand::Decision(DecisionCommand::Resolve(DecisionResolution {
            decision_id: DecisionId::new(6),
            option: "no".into(),
            by: Address::Owner,
            note: None,
            standing_rule: false,
        })),
        now(),
    )
    .await
    .unwrap();
    assert_eq!(
        reconciled_messages(&org).await,
        [
            content(),
            MessageContent {
                body: "Decision d6 resolved: no\nStanding rule: no".into(),
                ..content()
            }
        ]
    );
    // A second reconciliation must preserve both durable completion markers.
    assert_eq!(reconciled_messages(&org).await.len(), 2);
    cancel.cancel();
}

// Break caught: an agent's public answer-kind message suppressing the system
// answer because only recipient and kind, not exact content, are compared.
#[tokio::test]
async fn decision_bridge_ignores_another_agents_earlier_answer_as_a_marker() {
    let dir = tempfile::tempdir().unwrap();
    let (org, cancel) = seed(dir.path(), false).await;
    org.execute(
        OrgCommand::Adopt {
            agent_id: "p/other".parse().unwrap(),
            pane: "%8".into(),
            session_id: None,
        },
        now(),
    )
    .await
    .unwrap();
    let peer_answer = MessageContent {
        from: "p/other".parse().unwrap(),
        body: "An earlier peer answer".into(),
        ..content()
    };
    org.execute(OrgCommand::SendMessage(peer_answer.clone()), now())
        .await
        .unwrap();
    assert_eq!(reconciled_messages(&org).await, [peer_answer, content()]);
    assert_eq!(reconciled_messages(&org).await.len(), 2);
    cancel.cancel();
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
                crate::org::Request::Snapshot(_) | crate::org::Request::SnapshotAt(_) => {
                    !execute_timeout
                }
                crate::org::Request::ReportSnapshot(_) => false,
                crate::org::Request::Execute { command, .. } => {
                    execute_timeout && matches!(**command, OrgCommand::SendMessage(_))
                }
            };
            if target && entered.is_some() {
                entered.take().unwrap().send(()).unwrap();
                match &mut request {
                    crate::org::Request::Snapshot(reply) => reply.closed().await,
                    crate::org::Request::SnapshotAt(reply) => reply.closed().await,
                    crate::org::Request::ReportSnapshot(reply) => reply.closed().await,
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
    // Hang guards only (the paused clock reaches them only when nothing else
    // can run): a broken bridge that never sends the held request fails here.
    guarded(entry).await.unwrap();
    tokio::time::advance(Duration::from_secs(7)).await;
    guarded(timeout).await.unwrap();
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
                    relay: false,
                    confirm: false,
                },
                caller(),
            )
            .await
            .unwrap()
            .changed
    );
    // The question's row to the owner (U8) is the bridge's to write after the
    // request returned, so it may come first: pass over it.
    let sent = loop {
        let row = guarded(messages.next()).await.unwrap().unwrap();
        if row.body["kind"] != "question" {
            break row;
        }
    };
    assert_eq!(
        sent.body["body"],
        format!(
            "Decision {} resolved: no\nStanding rule: no",
            next.decision_id
        )
    );
    let state = org.snapshot().await.unwrap();
    // U8: each request also wrote its question's row to the owner.
    assert_eq!(
        state
            .messages()
            .filter(|m| m.content.kind != MessageKind::Question)
            .count(),
        2
    );
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
                    id: DecisionId::new(3),
                    redo_after: None,
                    redo_limit: None,
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
                    standing_rule: true,
                    relay: false,
                    confirm: false,
                },
                caller()
            )
            .await,
        Err(DecisionError::Unavailable(_))
    ));
    guarded(task).await.unwrap();
    cancel.cancel();
}

struct RelayConfirmRig {
    _dir: tempfile::TempDir,
    _stop: tokio_util::sync::DropGuard,
    _ready: watch::Sender<Result<bool, String>>,
    org: OrgHandle,
    methods: MethodRouter,
}

fn from_pane(pane: &str) -> CallContext {
    CallContext {
        from_pane: Some(CallerPane(pane.into())),
        ..caller()
    }
}

impl RelayConfirmRig {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        let stop = cancel.clone().drop_guard();
        let org = OrgHandle::open(dir.path().into(), cancel).await;
        for command in commands().into_iter().take(2) {
            org.execute(command, now()).await.unwrap();
        }
        let mut question = request("p/worker".parse().unwrap());
        question.deadline = Some(now() + chrono::Duration::minutes(1));
        org.execute(
            OrgCommand::Decision(DecisionCommand::Request(question)),
            now(),
        )
        .await
        .unwrap();
        let chief: AgentId = "p/chief".parse().unwrap();
        org.execute(
            OrgCommand::Adopt {
                agent_id: chief.clone(),
                pane: "%8".into(),
                session_id: None,
            },
            now(),
        )
        .await
        .unwrap();
        org.execute(
            OrgCommand::Owner(OwnerCommand::SetRelay {
                by: Address::Owner,
                agent: Some(chief),
            }),
            now(),
        )
        .await
        .unwrap();
        // Keep the real decision service/actor/router, without the background
        // answer bridge: the whole log cursor is the no-append assertion.
        let (ready, health) = watch::channel(Ok(true));
        let handle = DecisionHandle {
            org: org.clone(),
            clock: MessageClock::new(tokio::time::Instant::now(), now()),
            health,
            requests: Arc::new(Semaphore::new(64)),
            projects: None,
            reporting: Default::default(),
        };
        let mut methods = MethodRouter::default();
        register_decision_methods(&mut methods, handle).unwrap();
        Self {
            _dir: dir,
            _stop: stop,
            _ready: ready,
            org,
            methods,
        }
    }

    async fn refuse(&self, who: CallContext, params: Value, code: &str, message: &str) {
        let before = self.org.status().unwrap();
        let error = self
            .methods
            .call(who, "decision.resolve", params)
            .await
            .unwrap_err();
        assert_eq!(
            self.org.status().unwrap(),
            before,
            "refusal appended a fact"
        );
        assert_eq!(
            (error.0.code.as_str(), error.0.message.as_str()),
            (code, message)
        );
    }

    async fn relay(&self) {
        let result = self
            .methods
            .call(
                from_pane("%8"),
                "decision.resolve",
                json!({"id":"d3","option":"yes","relay":true}),
            )
            .await
            .unwrap();
        assert_eq!(result["decision"]["status"], "taken_provisionally");
    }
}

// Break caught: treating confirmation as an ordinary owner resolve or letting
// a relay promote a closed choice, a different option, or an empty quotation.
#[tokio::test]
async fn relay_confirm_refuses_without_writing() {
    let rig = RelayConfirmRig::start().await;
    let confirm =
        json!({"id":"d3","option":"yes","relay":true,"confirm":true,"note":"Yes sounds good"});
    let chief = || from_pane("%8");
    for (who, message) in [
        (
            from_pane("%7"),
            "p/worker may not confirm as relay for decision d3",
        ),
        (caller(), "owner may not confirm as relay for decision d3"),
        (
            from_pane("%90"),
            "owner may not confirm as relay for decision d3",
        ),
        (
            chief(),
            "cannot confirm before relaying decision d3 while Pending",
        ),
    ] {
        rig.refuse(who, confirm.clone(), "refused", message).await;
    }
    for who in [chief(), caller()] {
        let mut no_relay = confirm.clone();
        no_relay.as_object_mut().unwrap().remove("relay");
        rig.refuse(
            who,
            no_relay,
            "invalid_params",
            "invalid decision parameters: --confirm requires --relay",
        )
        .await;
    }
    let mut rule = confirm.clone();
    rule["standing_rule"] = json!(true);
    rig.refuse(
        chief(),
        rule,
        "invalid_params",
        "invalid decision parameters: --confirm cannot be used with --rule",
    )
    .await;
    for note in [
        None,
        Some(Value::Null),
        Some(json!("")),
        Some(json!(" \t\u{2003}")),
    ] {
        let mut bad = confirm.clone();
        bad.as_object_mut().unwrap().remove("note");
        if let Some(note) = note {
            bad["note"] = note;
        }
        rig.refuse(
            chief(),
            bad,
            "invalid_params",
            "invalid decision parameters: --confirm requires non-empty owner words",
        )
        .await;
    }
    rig.relay().await;
    let mut other = confirm.clone();
    other["option"] = json!("no");
    rig.refuse(
        chief(),
        other,
        "refused",
        r#"decision d3 is already taken provisionally on "yes"; only the owner may change it"#,
    )
    .await;
    let result = rig
        .methods
        .call(
            caller(),
            "decision.resolve",
            json!({"id":"d3","option":"no"}),
        )
        .await
        .unwrap();
    assert_eq!(result["decision"]["status"], "overruled");
    rig.refuse(
        chief(),
        confirm.clone(),
        "refused",
        "cannot confirm decision d3 while Overruled",
    )
    .await;

    let id = DecisionId::new(3);
    for (command, state) in [
        (
            DecisionCommand::Resolve(DecisionResolution {
                note: Some("confirmed in his words, relayed by p/chief: 'Yes sounds good'".into()),
                standing_rule: false,
                ..answer()
            }),
            "Resolved",
        ),
        (DecisionCommand::Expire { decision_id: id }, "Expired"),
        (
            DecisionCommand::Withdraw(DecisionWithdrawal {
                decision_id: id,
                by: Address::Owner,
                reason: None,
            }),
            "Withdrawn",
        ),
    ] {
        let closed = RelayConfirmRig::start().await;
        closed
            .org
            .execute(
                OrgCommand::Decision(command),
                now() + chrono::Duration::minutes(1),
            )
            .await
            .unwrap();
        closed
            .refuse(
                chief(),
                confirm.clone(),
                "refused",
                &format!("cannot confirm decision d3 while {state}"),
            )
            .await;
    }
}

// Break caught: requiring relay provenance on the prior take, losing audit or
// treating the confirm as provisional again rather than an ordinary final answer.
#[tokio::test]
async fn relay_confirm_accepts_a_system_take() {
    let rig = RelayConfirmRig::start().await;
    for command in [
        OwnerCommand::Away {
            by: Address::Owner,
            stacking_caps: Default::default(),
        },
        OwnerCommand::ApplyAway {
            decision_id: DecisionId::new(3),
        },
    ] {
        rig.org
            .execute(OrgCommand::Owner(command), now())
            .await
            .unwrap();
    }
    let before = rig.org.status().unwrap();
    let mut watch = rig.org.watch(vec!["decision.".into()], before).unwrap();
    let words = "Yes, that's right.";
    let stored = "confirmed in his words, relayed by p/chief: 'Yes, that's right.'";
    let result = rig
        .methods
        .call(
            from_pane("%8"),
            "decision.resolve",
            json!({"id":"d3","option":"yes","relay":true,"confirm":true,"note":words}),
        )
        .await
        .unwrap();
    let record = guarded(watch.next()).await.unwrap().unwrap();
    assert_eq!(record.kind, "decision.resolved");
    assert_eq!(
        record.body,
        json!({"decision_id":"d3","option":"yes","by":"owner","note":stored,"standing_rule":false,"caller_pane":"%8"})
    );
    assert_eq!(rig.org.status().unwrap(), before + 1);
    assert_eq!(result["changed"], true);
    assert_eq!(result["decision"]["status"], "resolved");
    assert_eq!(
        result["decision"]["provisional"],
        json!({"decision_id":"d3","option":"yes","by":"system"})
    );
    assert_eq!(result["decision"]["resolution"]["note"], stored);
    assert!(result.get("redo").is_none());
    rig.refuse(
        caller(),
        json!({"id":"d3","option":"no"}),
        "refused",
        "cannot resolve decision d3 while Resolved",
    )
    .await;
    let again = rig
        .methods
        .call(
            caller(),
            "decision.resolve",
            json!({"id":"d3","option":"yes","note":stored}),
        )
        .await
        .unwrap();
    assert_eq!(again["changed"], false);
    assert_eq!(again["decision"], result["decision"]);
    assert_eq!(rig.org.status().unwrap(), before + 1);
}

/// A system post in project `p`, as a later daemon writes a decision step.
fn system_post() -> OrgEvent {
    OrgEvent::ChannelPosted(atm_org::PostContent {
        channel: atm_org::ChannelId::Project("p".parse().unwrap()),
        author: Address::System,
        session_id: None,
        kind: MessageKind::Answer,
        body: "step".into(),
        attachments: vec![],
        task: None,
        thread: None,
        mentions: vec![],
        wake: vec![],
        route: false,
        decision: None,
        client_key: None,
        also_in_chat: false,
    })
}

// Break caught: an answer a later daemon sent as a wake row (same content,
// linked to its post) counts as unsent, so a rollback would send it again.
#[test]
fn a_resolution_sent_as_a_wake_counts_as_sent() {
    let mut state = pending();
    state
        .apply(4, now(), &OrgEvent::DecisionResolved(answer().into()))
        .unwrap();
    state.apply(5, now(), &system_post()).unwrap();
    let wake = MessageContent {
        post: Some(MessageId::new(5)),
        ..content()
    };
    state.apply(6, now(), &OrgEvent::MessageSent(wake)).unwrap();
    assert!(missing_resolution_messages(&state).is_empty());
}

/// d3 asked through a bridge (its question post written), the bridge stopped,
/// then the owner's answer committed with no bridge running: a stop between
/// the decision fact and its reply. With `reply`, the reply post is written
/// too, but not its row: a stop between a reply and its wake.
async fn seed_native(dir: &Path, reply: bool) -> (OrgHandle, CancellationToken) {
    let cancel = CancellationToken::new();
    let org = OrgHandle::open(dir.into(), cancel.clone()).await;
    for command in commands() {
        org.execute(command, now()).await.unwrap();
    }
    let bridge_cancel = cancel.child_token();
    let (handle, task) = spawn_decisions(
        org.clone(),
        MessageClock::new(tokio::time::Instant::now(), now()),
        bridge_cancel.clone(),
    );
    guarded(handle.initialized()).await.unwrap();
    bridge_cancel.cancel();
    guarded(task).await.unwrap();
    let id = DecisionId::new(3);
    assert!(decision_post(&org.snapshot().await.unwrap(), id, "asked").is_some());
    org.execute(
        OrgCommand::Decision(DecisionCommand::Resolve(answer())),
        now(),
    )
    .await
    .unwrap();
    if reply {
        let said = super::decision_steps::replies(&org.snapshot().await.unwrap());
        let [super::decision_steps::Said::Post(post)] = said.as_slice() else {
            panic!("one reply: {said:?}")
        };
        org.execute(
            OrgCommand::Channel(ChannelCommand::Post(post.clone())),
            now(),
        )
        .await
        .unwrap();
    }
    (org, cancel)
}
/// Start a bridge on a copy and stop it once it has reconciled. Returns the
/// reply post and the rows naming it.
async fn restart_native(dir: &Path) -> (MessageId, Vec<MessageId>, CancellationToken) {
    let cancel = CancellationToken::new();
    let org = OrgHandle::open(dir.into(), cancel.clone()).await;
    let bridge_cancel = cancel.child_token();
    let (handle, task) = spawn_decisions(
        org.clone(),
        MessageClock::new(tokio::time::Instant::now(), now()),
        bridge_cancel.clone(),
    );
    guarded(handle.initialized()).await.unwrap();
    bridge_cancel.cancel();
    guarded(task).await.unwrap();
    let state = org.snapshot().await.unwrap();
    let reply = decision_post(&state, DecisionId::new(3), "resolved").unwrap();
    let rows: Vec<_> = state
        .messages()
        .filter(|m| m.content.to == "p/worker".parse::<Address>().unwrap())
        .map(|m| {
            assert_eq!(
                MessageContent {
                    post: None,
                    ..m.content.clone()
                },
                content(),
                "today's answer, plus post"
            );
            assert_eq!(m.content.post, Some(reply));
            m.id
        })
        .collect();
    (reply, rows, cancel)
}

// Break caught: a restart after the answer leaves it unsaid, says it twice,
// or says it outside the question's thread.
#[tokio::test]
async fn restart_between_the_decision_fact_and_its_reply_repairs_it_once() {
    let [before, after, again] = [(); 3].map(|()| tempfile::tempdir().unwrap());
    let (_old, old_cancel) = seed_native(before.path(), false).await;
    copy_log(before.path(), after.path());
    let (reply, rows, first) = restart_native(after.path()).await;
    assert_eq!(rows.len(), 1);
    copy_log(after.path(), again.path());
    let (same_reply, same_rows, second) = restart_native(again.path()).await;
    assert_eq!((same_reply, same_rows), (reply, rows));
    for cancel in [old_cancel, first, second] {
        cancel.cancel();
    }
}

// Break caught: a reply whose row was lost is posted again, or its row is
// never written after a restart.
#[tokio::test]
async fn restart_between_a_reply_and_its_wake_writes_the_wake_once() {
    let [before, after, again] = [(); 3].map(|()| tempfile::tempdir().unwrap());
    let (old, old_cancel) = seed_native(before.path(), true).await;
    let state = old.snapshot().await.unwrap();
    let posted = decision_post(&state, DecisionId::new(3), "resolved").unwrap();
    assert!(wake_for_test(&state, posted).is_none(), "no row yet");
    copy_log(before.path(), after.path());
    let (reply, rows, first) = restart_native(after.path()).await;
    assert_eq!((reply, rows.len()), (posted, 1));
    copy_log(after.path(), again.path());
    let (same_reply, same_rows, second) = restart_native(again.path()).await;
    assert_eq!((same_reply, same_rows), (reply, rows));
    for cancel in [old_cancel, first, second] {
        cancel.cancel();
    }
}
fn wake_for_test(state: &OrgState, post: MessageId) -> Option<MessageId> {
    atm_org::channel_query::wake_for(state, post, &"p/worker".parse().unwrap())
}

// The lead's ruling on item 6: a question is posted whole, even past the
// 16 KiB post limit that only RPC posts check. Break caught: a cut body, or a
// page limit that refuses the largest question a request can make.
#[tokio::test]
async fn a_maximum_size_question_round_trips_through_history_and_inbox() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let org = OrgHandle::open(dir.path().into(), cancel.clone()).await;
    for command in commands().into_iter().take(2) {
        org.execute(command, now()).await.unwrap();
    }
    let (handle, task) = spawn_decisions(
        org.clone(),
        MessageClock::new(tokio::time::Instant::now(), now()),
        cancel.child_token(),
    );
    guarded(handle.initialized()).await.unwrap();
    let mut methods = MethodRouter::default();
    register_decision_methods(&mut methods, handle).unwrap();
    super::super::channel_methods::register(&mut methods, org.clone(), Utc::now).unwrap();
    let (title, context) = ("t".repeat(512), "c".repeat(16384));
    let made = methods
        .call(
            from_pane("%7"),
            "decision.request",
            json!({"kind":"plan","title":title,"context":context,"files":["plan.md"],"options":["yes","no"],"recommended":"yes","may_be_undone":true,"changes_intent":true,"deadline":null}),
        )
        .await
        .unwrap();
    let body = format!("{title}\n\n{context}");
    assert_eq!(body.len(), 16_898);
    let row = methods
        .call(caller(), "decision.get", json!({"id":made["decision_id"]}))
        .await
        .unwrap();
    let page = methods
        .call(
            caller(),
            "channel.history",
            json!({"channel":row["decision"]["channel"],"thread":made["post_id"]}),
        )
        .await
        .unwrap();
    assert_eq!(page["posts"][0]["id"], made["post_id"]);
    assert_eq!(page["posts"][0]["body"], body);
    let state = org.snapshot().await.unwrap();
    let post: MessageId = serde_json::from_value(made["post_id"].clone()).unwrap();
    let id = atm_org::channel_query::wake_for(&state, post, &Address::Owner).unwrap();
    let params = serde_json::from_value(json!({"id":id})).unwrap();
    let inbox = super::super::message_inbox::inbox(&state, &Address::Owner, &params, None, |_| {
        Default::default()
    })
    .unwrap();
    assert_eq!(inbox.messages[0].content.body, body);
    assert_eq!(inbox.messages[0].content.attachments, ["plan.md"]);
    cancel.cancel();
    guarded(task).await.unwrap();
}

/// Project `p`, seat `p/worker` and d3 by it, due one minute after `now()`.
async fn timed_decision(org: &OrgHandle) {
    for command in commands().into_iter().take(2) {
        org.execute(command, now()).await.unwrap();
    }
    let mut timed = request("p/worker".parse().unwrap());
    timed.deadline = Some(now() + chrono::Duration::minutes(1));
    org.execute(OrgCommand::Decision(DecisionCommand::Request(timed)), now())
        .await
        .unwrap();
}
fn expired_text() -> &'static str {
    "Decision d3 expired: no answer by its deadline, 2026-01-15T12:01:00Z.\nIt is closed. Ask again if it still matters."
}

// Owner Q2 = yes. Paused clock, in-memory actor (no disk or socket I/O, so
// the paused clock cannot jump during real I/O); the deadline is reached by
// `advance` alone. Break caught: no timer, a timer that does not expire the
// decision, or an expiry that does not wake the asker in the thread.
#[test]
fn expiry_timer_expires_at_the_deadline_and_wakes_the_asker() {
    use std::sync::atomic::{AtomicU64, Ordering};
    use tokio::sync::{broadcast, mpsc, watch};
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .unwrap();
    runtime.block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let mut state = pending();
        let mut timed = request("p/worker".parse().unwrap());
        timed.deadline = Some(now() + chrono::Duration::minutes(1));
        state
            .apply(4, now(), &OrgEvent::DecisionRequested(timed))
            .unwrap();
        let (requests, receive) = mpsc::channel(64);
        let (health, status) = watch::channel(Ok(4));
        let (committed, _) = broadcast::channel(1024);
        let cancel = CancellationToken::new();
        let seq = AtomicU64::new(4);
        let actor = tokio::spawn(crate::org::actor::run(
            state,
            receive,
            health,
            committed.clone(),
            cancel.clone(),
            move |_| std::future::ready(Ok(seq.fetch_add(1, Ordering::SeqCst) + 1)),
        ));
        let org = OrgHandle {
            requests,
            health: status,
            committed,
            reads: Some(crate::org::watch::ReadAccess {
                reader: crate::org::storage::Reader::start(dir.path().into()).unwrap(),
            }),
        };
        let mut expired = org
            .watch(vec!["decision.expired".into()], org.status().unwrap())
            .unwrap();
        let mut rows = org
            .watch(vec!["msg.sent".into()], org.status().unwrap())
            .unwrap();
        let (handle, bridge) = spawn_decisions(
            org.clone(),
            MessageClock::new(tokio::time::Instant::now(), now()),
            cancel.child_token(),
        );
        guarded(handle.initialized()).await.unwrap();
        let d4 = DecisionId::new(4);
        assert_eq!(
            org.snapshot().await.unwrap().decision(&d4).unwrap().status,
            DecisionStatus::Pending
        );
        tokio::time::advance(Duration::from_secs(60)).await;
        let fact = guarded(expired.next()).await.unwrap().unwrap();
        assert_eq!(fact.body["decision_id"], "d4");
        assert!(fact.ts >= now() + chrono::Duration::minutes(1));
        let row = loop {
            let row = guarded(rows.next()).await.unwrap().unwrap();
            if row.body["to"] == "p/worker" {
                break row;
            }
        };
        assert_eq!(row.body["body"], expired_text().replace("d3", "d4"));
        let state = org.snapshot().await.unwrap();
        assert_eq!(state.decision(&d4).unwrap().status, DecisionStatus::Expired);
        let reply = decision_post(&state, d4, "expired").unwrap();
        assert_eq!(row.body["post"], reply.to_string());
        assert_eq!(
            atm_org::channel_query::post(&state, reply)
                .unwrap()
                .content
                .thread,
            decision_post(&state, d4, "asked")
        );
        // d3 has no deadline and stays open.
        assert_eq!(
            state.decision(&DecisionId::new(3)).unwrap().status,
            DecisionStatus::Pending
        );
        cancel.cancel();
        guarded(bridge).await.unwrap();
        guarded(actor).await.unwrap();
    });
}

// Restart: the timer is not stored; the deadline is. Break caught: a decision
// whose deadline passed while the daemon was down stays pending, or expires
// without its question and reply.
#[tokio::test]
async fn deadline_passed_while_down_expires_on_the_first_reconcile() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let org = OrgHandle::open(dir.path().into(), cancel.clone()).await;
    timed_decision(&org).await;
    let (handle, task) = spawn_decisions(
        org.clone(),
        MessageClock::new(
            tokio::time::Instant::now(),
            now() + chrono::Duration::minutes(2),
        ),
        cancel.child_token(),
    );
    guarded(handle.initialized()).await.unwrap();
    let state = org.snapshot().await.unwrap();
    let d3 = DecisionId::new(3);
    assert_eq!(state.decision(&d3).unwrap().status, DecisionStatus::Expired);
    let asked = decision_post(&state, d3, "asked").unwrap();
    // Asked before this daemon started: its question wakes nobody (Q3).
    let question = atm_org::channel_query::post(&state, asked).unwrap();
    assert_eq!(question.content.wake, Vec::<Address>::new());
    let reply = decision_post(&state, d3, "expired").unwrap();
    let post = atm_org::channel_query::post(&state, reply).unwrap();
    assert_eq!(post.content.thread, Some(asked));
    assert_eq!(post.content.body, expired_text());
    assert_eq!(post.content.wake, ["p/worker".parse::<Address>().unwrap()]);
    cancel.cancel();
    guarded(task).await.unwrap();
}

// Break caught: the owner's answer landing between the due query and the
// Expire stops the bridge, or the decision is said to have expired.
#[tokio::test]
async fn a_concurrent_answer_before_expiry_is_passed_over() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let org = OrgHandle::open(dir.path().into(), cancel.clone()).await;
    timed_decision(&org).await;
    let later = now() + chrono::Duration::minutes(2);
    let real = org.requests.clone();
    let (requests, mut receive) = tokio::sync::mpsc::channel(64);
    let resolver = org.clone();
    let proxy = tokio::spawn(async move {
        let mut first = true;
        while let Some(request) = receive.recv().await {
            let expire = matches!(&request, crate::org::Request::Execute { command, .. }
                if matches!(**command, OrgCommand::Decision(DecisionCommand::Expire { .. })));
            if expire && std::mem::take(&mut first) {
                resolver
                    .execute(
                        OrgCommand::Decision(DecisionCommand::Resolve(answer())),
                        later,
                    )
                    .await
                    .unwrap();
            }
            real.send(request).await.unwrap();
        }
    });
    let mut bridged = org.clone();
    bridged.requests = requests;
    let (handle, task) = spawn_decisions(
        bridged,
        MessageClock::new(tokio::time::Instant::now(), later),
        cancel.child_token(),
    );
    guarded(handle.initialized()).await.unwrap();
    let state = org.snapshot().await.unwrap();
    let d3 = DecisionId::new(3);
    assert_eq!(
        state.decision(&d3).unwrap().status,
        DecisionStatus::Resolved
    );
    assert!(decision_post(&state, d3, "resolved").is_some());
    assert_eq!(decision_post(&state, d3, "expired"), None);
    cancel.cancel();
    guarded(task).await.unwrap();
    drop(handle);
    guarded(proxy).await.unwrap();
}

// Review S1. Break caught: a request whose decision committed reports a
// timeout because its inline question was slow. Paused clock, in-memory actor
// (no real I/O, so the clock cannot jump past one); once the bridge is up a
// proxy makes every org request take 4 s, under the actor's own deadlines.
#[test]
fn a_committed_request_returns_its_id_however_slow_its_question_is() {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use tokio::sync::{broadcast, mpsc, watch};
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .unwrap();
    runtime.block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let (real, receive) = mpsc::channel(64);
        let (health, status) = watch::channel(Ok(3));
        let (committed, _) = broadcast::channel(1024);
        let cancel = CancellationToken::new();
        let seq = AtomicU64::new(3);
        let actor = tokio::spawn(crate::org::actor::run(
            pending(),
            receive,
            health,
            committed.clone(),
            cancel.clone(),
            move |_| std::future::ready(Ok(seq.fetch_add(1, Ordering::SeqCst) + 1)),
        ));
        let (requests, mut proxied) = mpsc::channel::<crate::org::Request>(64);
        let slow = Arc::new(AtomicBool::new(false));
        let slowed = slow.clone();
        let proxy = tokio::spawn(async move {
            while let Some(request) = proxied.recv().await {
                let (real, slow) = (real.clone(), slowed.load(Ordering::SeqCst));
                tokio::spawn(async move {
                    if slow {
                        tokio::time::sleep(Duration::from_secs(4)).await;
                    }
                    let _ = real.send(request).await;
                });
            }
        });
        let org = OrgHandle {
            requests,
            health: status,
            committed,
            reads: Some(crate::org::watch::ReadAccess {
                reader: crate::org::storage::Reader::start(dir.path().into()).unwrap(),
            }),
        };
        let (handle, bridge) = spawn_decisions(
            org.clone(),
            MessageClock::new(tokio::time::Instant::now(), now()),
            cancel.child_token(),
        );
        guarded(handle.initialized()).await.unwrap();
        // d3's question and owner row are written; the request is next.
        let next = DecisionId::new(org.status().unwrap() + 1);
        slow.store(true, Ordering::SeqCst);
        let params = RequestParams {
            kind: DecisionKind::Plan,
            title: "Slow question".into(),
            context: String::new(),
            files: vec![],
            options: vec!["yes".into(), "no".into()],
            recommended: None,
            may_be_undone: true,
            changes_intent: true,
            deadline: None,
        };
        let made = guarded(handle.request(params, from_pane("%7")))
            .await
            .unwrap();
        assert_eq!(made.decision_id, next);
        assert_eq!(made.post_id, None, "the bridge writes it");
        cancel.cancel();
        guarded(bridge).await.unwrap();
        guarded(actor).await.unwrap();
        drop(handle);
        drop(org);
        guarded(proxy).await.unwrap();
    });
}

// Review N2. The owner answers between a request's commit and its inline
// question: the proxy queues his answer right behind the commit, before the
// request's own snapshot. Break caught: a question card and an owner row for
// a decision that is already closed.
#[tokio::test]
async fn an_answer_before_the_inline_question_leaves_no_card() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let org = OrgHandle::open(dir.path().into(), cancel.clone()).await;
    for command in commands().into_iter().take(2) {
        org.execute(command, now()).await.unwrap();
    }
    let real = org.requests.clone();
    let (requests, mut receive) = tokio::sync::mpsc::channel(64);
    let resolver = org.clone();
    let proxy = tokio::spawn(async move {
        while let Some(request) = receive.recv().await {
            let asked = matches!(&request, crate::org::Request::Execute { command, .. }
                if matches!(**command, OrgCommand::Decision(DecisionCommand::Request(_))));
            let next = DecisionId::new(resolver.status().unwrap() + 1);
            real.send(request).await.unwrap();
            if asked {
                let mut answer = answer();
                answer.decision_id = next;
                resolver
                    .execute(
                        OrgCommand::Decision(DecisionCommand::Resolve(answer)),
                        now(),
                    )
                    .await
                    .unwrap();
            }
        }
    });
    let mut bridged = org.clone();
    bridged.requests = requests;
    let (handle, task) = spawn_decisions(
        bridged,
        MessageClock::new(tokio::time::Instant::now(), now()),
        cancel.child_token(),
    );
    guarded(handle.initialized()).await.unwrap();
    let params = RequestParams {
        kind: DecisionKind::Plan,
        title: "Answered at once".into(),
        context: String::new(),
        files: vec![],
        options: vec!["yes".into(), "no".into()],
        recommended: None,
        may_be_undone: true,
        changes_intent: true,
        deadline: None,
    };
    let made = guarded(handle.request(params, from_pane("%7")))
        .await
        .unwrap();
    assert_eq!(made.post_id, None);
    let state = org.snapshot().await.unwrap();
    let id = made.decision_id;
    assert_eq!(
        state.decision(&id).unwrap().status,
        DecisionStatus::Resolved
    );
    assert_eq!(decision_post(&state, id, "asked"), None, "no card");
    assert!(
        !state
            .messages()
            .any(|m| m.content.kind == MessageKind::Question),
        "no owner row"
    );
    cancel.cancel();
    guarded(task).await.unwrap();
    drop(handle);
    guarded(proxy).await.unwrap();
}

// Review N3. The wall clock was stepped back between two runs: a decision
// recorded an hour "later" than the restarted daemon's clock was still asked
// before it started. Break caught: its question wakes the owner at the switch.
#[tokio::test]
async fn a_decision_asked_before_the_start_wakes_nobody_whatever_the_clock() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let org = OrgHandle::open(dir.path().into(), cancel.clone()).await;
    let later = now() + chrono::Duration::hours(1);
    for command in commands() {
        org.execute(command, later).await.unwrap();
    }
    let (handle, task) = spawn_decisions(
        org.clone(),
        MessageClock::new(tokio::time::Instant::now(), now()),
        cancel.child_token(),
    );
    guarded(handle.initialized()).await.unwrap();
    let state = org.snapshot().await.unwrap();
    let asked = decision_post(&state, DecisionId::new(3), "asked").unwrap();
    let question = atm_org::channel_query::post(&state, asked).unwrap();
    assert_eq!(question.content.wake, Vec::<Address>::new());
    assert!(!state
        .messages()
        .any(|m| m.content.kind == MessageKind::Question));
    cancel.cancel();
    guarded(task).await.unwrap();
}

// Review S1, the error branch. Break caught: a failed inline question
// (here every post is dropped before the actor sees it, the bridge's too, so
// whichever runs first the inline one fails) fails a request whose decision
// committed. The request passed the bridge's health check before its commit.
#[tokio::test]
async fn a_committed_request_returns_its_id_when_its_question_fails() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let org = OrgHandle::open(dir.path().into(), cancel.clone()).await;
    for command in commands().into_iter().take(2) {
        org.execute(command, now()).await.unwrap();
    }
    let real = org.requests.clone();
    let (requests, mut receive) = tokio::sync::mpsc::channel(64);
    let proxy = tokio::spawn(async move {
        while let Some(request) = receive.recv().await {
            let post = matches!(&request, crate::org::Request::Execute { command, .. }
                if matches!(**command, OrgCommand::Channel(ChannelCommand::Post(_))));
            if !post {
                real.send(request).await.unwrap();
            }
        }
    });
    let mut bridged = org.clone();
    bridged.requests = requests;
    let (handle, task) = spawn_decisions(
        bridged,
        MessageClock::new(tokio::time::Instant::now(), now()),
        cancel.child_token(),
    );
    guarded(handle.initialized()).await.unwrap();
    let next = DecisionId::new(org.status().unwrap() + 1);
    let params = RequestParams {
        kind: DecisionKind::Plan,
        title: "Lost question".into(),
        context: String::new(),
        files: vec![],
        options: vec!["yes".into(), "no".into()],
        recommended: None,
        may_be_undone: true,
        changes_intent: true,
        deadline: None,
    };
    let made = guarded(handle.request(params, from_pane("%7")))
        .await
        .unwrap();
    assert_eq!(made.decision_id, next);
    assert_eq!(made.post_id, None);
    cancel.cancel();
    guarded(task).await.unwrap();
    drop(handle);
    guarded(proxy).await.unwrap();
}
