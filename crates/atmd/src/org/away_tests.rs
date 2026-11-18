//! The away switch in the daemon: pure projections, then a real organisation
//! log on a temp dir with the decision bridge. Every wait is on a committed
//! event or on the bridge's first reconcile; the outer timeouts are hang guards.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::super::decisions::{spawn_decisions, RequestParams, ResolveParams};
use super::*;
use crate::server::methods::{router, CallerId, CallerPane, MethodServices};
use atm_org::{
    DecisionCommand, DecisionId, DecisionKind, DecisionRequest, DecisionResolution, OrgEvent,
    OwnerAway, ProvisionalChoice,
};
use serde_json::{json, Value};
use std::{future::Future, path::Path, sync::Arc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const TAKEN: &str = "Decision d3 taken provisionally: yes\nThe owner is away. Finish your task in progress on this option. Start nothing further on it: a task built on it names d3 in rests_on and waits until the owner has seen d3.";
const YOURS: &str = "Decision d3 is yours to decide: recorded as yes, your recommended option.\nThe owner is away and sees it when he is back.";
const WAITS_OUTSIDE: &str = "Decision d3 waits for the owner: he is away, and it cannot be undone or reaches outside.\nOnly work that depends on it waits. Carry on with other work.";
const WAITS_UNRECOMMENDED: &str = "Decision d3 waits for the owner: he is away, and it has no recommended option.\nTo have it handled while he is away, withdraw it and ask again with a recommended option.";

fn now() -> DateTime<Utc> {
    "2026-01-15T12:00:00Z".parse().unwrap()
}
fn worker() -> Address {
    "p/worker".parse().unwrap()
}
fn owner() -> CallContext {
    CallContext {
        client_id: CallerId("away-test".into()),
        from_pane: None,
    }
}
fn pane(pane: &str) -> CallContext {
    CallContext {
        client_id: CallerId("away-test".into()),
        from_pane: Some(CallerPane(pane.into())),
    }
}
fn request(
    may_be_undone: bool,
    changes_intent: bool,
    recommended: Option<&str>,
) -> DecisionRequest {
    DecisionRequest {
        requester: worker(),
        kind: DecisionKind::Plan,
        title: "Choose".into(),
        context: String::new(),
        files: vec![],
        options: vec!["yes".into(), "no".into()],
        recommended: recommended.map(Into::into),
        may_be_undone,
        changes_intent,
        deadline: None,
    }
}
fn params(request: DecisionRequest) -> RequestParams {
    RequestParams {
        kind: request.kind,
        title: request.title,
        context: request.context,
        files: request.files,
        options: request.options,
        recommended: request.recommended,
        may_be_undone: request.may_be_undone,
        changes_intent: request.changes_intent,
        deadline: request.deadline,
    }
}
fn message(kind: MessageKind, body: &str) -> MessageContent {
    MessageContent {
        delivery_notice: None,
        from: Address::System,
        to: worker(),
        kind,
        body: body.into(),
        attachments: vec![],
        task: None,
    }
}

/// Project `p` at `/project`, the agent `p/worker` in `%7`, then `events`
/// from sequence 3 on: a request applied first is `d3`.
fn folded(events: &[OrgEvent]) -> OrgState {
    let mut state = OrgState::default();
    let seed = [
        OrgEvent::ProjectRegistered {
            project_id: "p".parse().unwrap(),
            root: "/project".into(),
        },
        OrgEvent::AgentAdopted {
            agent_id: "p/worker".parse().unwrap(),
            pane: "%7".into(),
            session_id: None,
        },
    ];
    for (index, event) in seed.iter().chain(events).enumerate() {
        state.apply(index as u64 + 1, now(), event).unwrap();
    }
    state
}
fn away_fact() -> OrgEvent {
    OrgEvent::OwnerAway(OwnerAway {
        by: Address::Owner,
        stacking_caps: [].into(),
    })
}
fn taken(by: Address) -> OrgEvent {
    OrgEvent::DecisionTakenProvisionally(ProvisionalChoice {
        relayed_by: None,
        decision_id: DecisionId::new(3),
        option: "yes".into(),
        by,
    })
}

// Break caught: a notice chosen by class instead of by who took it, a waiting
// notice sent while the owner is present, or a changed word in a text that is
// the bridge's completion marker.
#[test]
fn notices_are_exact_and_follow_status_and_the_away_flag() {
    let requested = |undone, intent, recommended| {
        OrgEvent::DecisionRequested(request(undone, intent, recommended))
    };
    for (events, expected) in [
        (
            vec![
                requested(true, true, Some("yes")),
                away_fact(),
                taken(Address::System),
            ],
            Some(message(MessageKind::Answer, TAKEN)),
        ),
        (
            vec![
                requested(true, false, Some("yes")),
                away_fact(),
                taken(worker()),
            ],
            Some(message(MessageKind::Answer, YOURS)),
        ),
        (
            vec![requested(false, true, Some("yes")), away_fact()],
            Some(message(MessageKind::Note, WAITS_OUTSIDE)),
        ),
        (
            vec![requested(true, true, None), away_fact()],
            Some(message(MessageKind::Note, WAITS_UNRECOMMENDED)),
        ),
        // Cannot be undone and nothing recommended: the class is the reason.
        (
            vec![requested(false, false, None), away_fact()],
            Some(message(MessageKind::Note, WAITS_OUTSIDE)),
        ),
        // Present: a pending decision is the owner's, and nothing is said.
        (vec![requested(false, true, Some("yes"))], None),
        (vec![requested(true, true, None)], None),
        // Away, and about to be recorded: the notice follows the take.
        (vec![requested(true, true, Some("yes")), away_fact()], None),
        (vec![requested(true, false, Some("yes")), away_fact()], None),
    ] {
        let state = folded(&events);
        assert_eq!(
            missing_notices(&state),
            expected.into_iter().collect::<Vec<_>>(),
            "{events:?}"
        );
    }
    // A taken decision keeps its notice due after the owner is back, until it
    // is answered; the answer message then speaks for it.
    let mut state = folded(&[
        requested(true, true, Some("yes")),
        away_fact(),
        taken(Address::System),
    ]);
    state
        .apply(
            6,
            now(),
            &OrgEvent::OwnerBack(atm_org::OwnerBack { by: Address::Owner }),
        )
        .unwrap();
    assert_eq!(
        missing_notices(&state),
        [message(MessageKind::Answer, TAKEN)]
    );
    state
        .apply(
            7,
            now(),
            &OrgEvent::DecisionResolved(
                DecisionResolution {
                    decision_id: DecisionId::new(3),
                    option: "yes".into(),
                    by: Address::Owner,
                    note: None,
                    standing_rule: false,
                }
                .into(),
            ),
        )
        .unwrap();
    assert!(missing_notices(&state).is_empty());
    // A system requester has no inbox.
    let mut system = request(true, true, Some("yes"));
    system.requester = Address::System;
    let state = folded(&[
        OrgEvent::DecisionRequested(system),
        away_fact(),
        taken(Address::System),
    ]);
    assert!(missing_notices(&state).is_empty());
}

// Break caught: deduplicating on delivery or read state instead of the durable
// enqueue, which would send the notice again after every failed delivery.
#[test]
fn a_notice_is_complete_once_its_message_is_sent_whatever_became_of_it() {
    let mut state = folded(&[
        OrgEvent::DecisionRequested(request(true, true, Some("yes"))),
        away_fact(),
        taken(Address::System),
    ]);
    let content = message(MessageKind::Answer, TAKEN);
    assert_eq!(missing_notices(&state), std::slice::from_ref(&content));
    state
        .apply(6, now(), &OrgEvent::MessageSent(content))
        .unwrap();
    assert!(missing_notices(&state).is_empty());
    state
        .apply(
            7,
            now(),
            &OrgEvent::MessageDeliveryFailed {
                message_id: atm_org::MessageId::new(6),
                reason: "held".into(),
                final_failure: true,
            },
        )
        .unwrap();
    assert!(missing_notices(&state).is_empty());
}

// Break caught: reporting while present, reporting decisions that wait (one
// command per reconcile for ever), or reporting what is no longer pending.
#[test]
fn reports_name_only_pending_decisions_the_table_would_record_while_away() {
    let requests = [
        request(true, true, Some("yes")),  // d3: taken
        request(true, false, Some("yes")), // d4: the agent decides
        request(false, true, Some("yes")), // d5: waits
        request(true, true, None),         // d6: waits
        request(true, true, Some("yes")),  // d7: answered below
    ]
    .map(OrgEvent::DecisionRequested);
    let mut events = requests.to_vec();
    assert!(pending_reports(&folded(&events)).is_empty(), "present");
    events.push(OrgEvent::DecisionResolved(
        DecisionResolution {
            decision_id: DecisionId::new(7),
            option: "no".into(),
            by: Address::Owner,
            note: None,
            standing_rule: false,
        }
        .into(),
    ));
    events.push(away_fact());
    let report = |seq| {
        OrgCommand::Owner(OwnerCommand::ApplyAway {
            decision_id: DecisionId::new(seq),
        })
    };
    assert_eq!(pending_reports(&folded(&events)), [report(3), report(4)]);
    events.push(taken(Address::System));
    assert_eq!(pending_reports(&folded(&events)), [report(4)]);
}

async fn guarded<T>(work: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(60), work)
        .await
        .unwrap()
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

/// A real organisation log with project `p` (rooted in the temp dir, so its
/// settings file can be written) and the agent `p/worker` in `%7`.
struct Rig {
    dir: tempfile::TempDir,
    org: OrgHandle,
    handle: DecisionHandle,
    cancel: CancellationToken,
    bridge: JoinHandle<()>,
}
impl Rig {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("p/.atm")).unwrap();
        let cancel = CancellationToken::new();
        let org = OrgHandle::open(dir.path().join("state"), cancel.clone()).await;
        for command in [
            OrgCommand::RegisterProject {
                project_id: "p".parse().unwrap(),
                root: dir.path().join("p").to_str().unwrap().into(),
                move_root: false,
            },
            OrgCommand::Adopt {
                agent_id: "p/worker".parse().unwrap(),
                pane: "%7".into(),
                session_id: None,
            },
        ] {
            org.execute(command, now()).await.unwrap();
        }
        let (handle, bridge) = Self::bridge(&org, &cancel).await;
        Self {
            dir,
            org,
            handle,
            cancel,
            bridge,
        }
    }
    async fn bridge(
        org: &OrgHandle,
        cancel: &CancellationToken,
    ) -> (DecisionHandle, JoinHandle<()>) {
        let (handle, bridge) = spawn_decisions(
            org.clone(),
            MessageClock::new(tokio::time::Instant::now(), now()),
            cancel.child_token(),
        );
        guarded(handle.initialized()).await.unwrap();
        (handle, bridge)
    }
    fn settings(&self, text: &str) {
        std::fs::write(self.dir.path().join("p/.atm/away.toml"), text).unwrap();
    }
    fn watch(&self, topic: &str) -> crate::org::EventWatch {
        self.org
            .watch(vec![topic.into()], self.org.status().unwrap())
            .unwrap()
    }
    async fn ask(&self, request: DecisionRequest) -> DecisionId {
        guarded(self.handle.request(params(request), pane("%7")))
            .await
            .unwrap()
            .decision_id
    }
    async fn away(&self) -> AwayResult {
        guarded(self.handle.away(SwitchParams {}, owner()))
            .await
            .unwrap()
    }
    async fn state(&self) -> OrgState {
        self.org.snapshot().await.unwrap()
    }
    async fn messages(&self) -> Vec<MessageContent> {
        self.state()
            .await
            .messages()
            .map(|m| m.content.clone())
            .collect()
    }
    async fn stop(self) {
        self.cancel.cancel();
        guarded(self.bridge).await.unwrap();
    }
}
async fn finish_scenario(rig: Rig, mut scenario: JoinHandle<()>) {
    let outcome = tokio::time::timeout(Duration::from_secs(120), &mut scenario).await;
    if outcome.is_err() {
        scenario.abort();
        let _ = guarded(scenario).await;
    }
    rig.stop().await;
    outcome.unwrap().unwrap();
}
async fn next(watch: &mut crate::org::EventWatch) -> Arc<crate::eventlog::Envelope> {
    guarded(watch.next()).await.unwrap().unwrap()
}
fn body(seq: u64, option: &str, text: &str) -> String {
    text.replace("d3", &format!("d{seq}"))
        .replace("yes", option)
}

// Break caught: a bridge that does not act on a request made while away, sends
// no notice, or sends the notice again on its next reconcile.
#[tokio::test]
async fn a_request_while_away_is_taken_and_its_requester_told_once() {
    let rig = Rig::new().await;
    let away = rig.away().await;
    assert!(away.changed);
    assert_eq!(away.pending_count, 0);
    let mut takes = rig.watch("decision.taken_provisionally");
    let mut sent = rig.watch("msg.sent");
    let change = rig.ask(request(true, true, Some("no"))).await;
    let take = next(&mut takes).await;
    assert_eq!(
        take.body,
        json!({"decision_id":change.to_string(),"option":"no","by":"system"})
    );
    let taken = message(MessageKind::Answer, &body(change.seq(), "no", TAKEN));
    assert_eq!(
        next(&mut sent).await.body,
        serde_json::to_value(&taken).unwrap()
    );
    // A detail is recorded for its requester. Its notice is the next message:
    // the reconciles it caused passed over the first decision without a second
    // notice for it.
    let detail = rig.ask(request(true, false, Some("yes"))).await;
    assert_eq!(
        next(&mut takes).await.body,
        json!({"decision_id":detail.to_string(),"option":"yes","by":"p/worker"})
    );
    let yours = message(MessageKind::Answer, &body(detail.seq(), "yes", YOURS));
    assert_eq!(
        next(&mut sent).await.body,
        serde_json::to_value(&yours).unwrap()
    );
    // What must wait is announced, and stays pending.
    let outside = rig.ask(request(false, true, Some("yes"))).await;
    let waits = message(
        MessageKind::Note,
        &body(outside.seq(), "yes", WAITS_OUTSIDE),
    );
    assert_eq!(
        next(&mut sent).await.body,
        serde_json::to_value(&waits).unwrap()
    );
    let state = rig.state().await;
    assert_eq!(
        state.decision(&outside).unwrap().status,
        DecisionStatus::Pending
    );
    assert_eq!(rig.messages().await, [taken, yours, waits]);
    rig.stop().await;
}

// Break caught: a bridge that only watches decision events, so a decision
// asked before the owner left is never handled.
#[tokio::test]
async fn decisions_pending_when_the_owner_leaves_are_handled_at_once() {
    let rig = Rig::new().await;
    let change = rig.ask(request(true, true, Some("yes"))).await;
    let unrecommended = rig.ask(request(true, true, None)).await;
    // Present, nothing is taken and nobody is told: the answer to a third
    // decision is the first message the bridge sends.
    let answered = rig.ask(request(true, true, Some("yes"))).await;
    let mut sent = rig.watch("msg.sent");
    guarded(rig.handle.resolve(
        ResolveParams {
            id: answered,
            option: "no".into(),
            note: None,
            standing_rule: false,
            relay: false,
        },
        owner(),
    ))
    .await
    .unwrap();
    assert!(next(&mut sent).await.body["body"]
        .as_str()
        .unwrap()
        .starts_with(&format!("Decision {answered} resolved: no")));
    assert_eq!(rig.messages().await.len(), 1);
    assert_eq!(
        rig.state().await.decision(&change).unwrap().status,
        DecisionStatus::Pending
    );
    let mut takes = rig.watch("decision.taken_provisionally");
    let away = rig.away().await;
    assert_eq!(away.pending_count, 2);
    assert_eq!(
        next(&mut takes).await.body["decision_id"],
        change.to_string()
    );
    let mut bodies = vec![
        next(&mut sent).await.body["body"]
            .as_str()
            .unwrap()
            .to_owned(),
        next(&mut sent).await.body["body"]
            .as_str()
            .unwrap()
            .to_owned(),
    ];
    bodies.sort();
    let mut expected = vec![
        body(change.seq(), "yes", TAKEN),
        body(unrecommended.seq(), "yes", WAITS_UNRECOMMENDED),
    ];
    expected.sort();
    assert_eq!(bodies, expected);
    rig.stop().await;
}

// Break caught: an away flag kept only in memory, or a restart that leaves a
// decision pending because no new event wakes the bridge.
#[tokio::test]
async fn a_restart_while_away_keeps_the_switch_and_handles_what_was_left_pending() {
    let before = tempfile::tempdir().unwrap();
    let after = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let old = OrgHandle::open(before.path().into(), cancel.clone()).await;
    let since = now() + chrono::Duration::seconds(5);
    for (command, at) in [
        (
            OrgCommand::RegisterProject {
                project_id: "p".parse().unwrap(),
                root: "/project".into(),
                move_root: false,
            },
            now(),
        ),
        (
            OrgCommand::Adopt {
                agent_id: "p/worker".parse().unwrap(),
                pane: "%7".into(),
                session_id: None,
            },
            now(),
        ),
        (
            OrgCommand::Decision(DecisionCommand::Request(request(true, true, Some("yes")))),
            now(),
        ),
        (
            OrgCommand::Owner(OwnerCommand::Away {
                by: Address::Owner,
                stacking_caps: [("p".parse().unwrap(), 2)].into(),
            }),
            since,
        ),
    ] {
        old.execute(command, at).await.unwrap();
    }
    // The daemon died here: away, one decision pending, no bridge had run.
    copy_log(before.path(), after.path());
    let restarted = CancellationToken::new();
    let org = OrgHandle::open(after.path().into(), restarted.clone()).await;
    let state = org.snapshot().await.unwrap();
    assert_eq!(state.owner().away_since, Some(since));
    assert_eq!(state.owner().stacking_cap(Some(&"p".parse().unwrap())), 2);
    let (handle, bridge) = Rig::bridge(&org, &restarted).await;
    // The first reconcile is over once the bridge reports itself initialized.
    let state = org.snapshot().await.unwrap();
    assert_eq!(
        state.decision(&DecisionId::new(3)).unwrap().status,
        DecisionStatus::TakenProvisionally
    );
    assert_eq!(
        state
            .messages()
            .map(|m| m.content.clone())
            .collect::<Vec<_>>(),
        [message(MessageKind::Answer, TAKEN)]
    );
    // A repeat reports the time the log holds, and changes nothing.
    let cursor = org.status().unwrap();
    let again = guarded(handle.away(SwitchParams {}, owner()))
        .await
        .unwrap();
    assert_eq!((again.changed, again.away_since), (false, since));
    assert_eq!(org.status().unwrap(), cursor);
    let back = guarded(handle.back(SwitchParams {}, owner()))
        .await
        .unwrap();
    assert_eq!(
        (
            back.changed,
            back.away_since,
            back.provisional_count,
            back.pending_count
        ),
        (true, Some(since), 1, 0)
    );
    let back = guarded(handle.back(SwitchParams {}, owner()))
        .await
        .unwrap();
    assert_eq!((back.changed, back.away_since), (false, None));
    cancel.cancel();
    restarted.cancel();
    guarded(bridge).await.unwrap();
}

// Break caught: a cap that never reaches the log, a default other than one, a
// repeat that reads files, or a broken file that still sets the switch.
#[tokio::test]
async fn caps_come_from_each_projects_settings_file_when_the_switch_is_set() {
    let rig = Rig::new().await;
    let p: ProjectId = "p".parse().unwrap();
    let gone: ProjectId = "gone".parse().unwrap();
    // A project whose folder no longer exists has no file: the default.
    rig.org
        .execute(
            OrgCommand::RegisterProject {
                project_id: gone.clone(),
                root: rig.dir.path().join("gone").to_str().unwrap().into(),
                move_root: false,
            },
            now(),
        )
        .await
        .unwrap();
    let file = rig.dir.path().join("p/.atm/away.toml");
    let cursor = rig.org.status().unwrap();
    for (text, reason) in [
        ("stacking_cap = 0\n", "stacking_cap must be at least 1"),
        ("stacking_cap = \"secret words\"\n", "invalid away settings"),
        ("stacking_cpa = 2\n", "invalid away settings"),
    ] {
        rig.settings(text);
        let error = guarded(rig.handle.away(SwitchParams {}, owner()))
            .await
            .unwrap_err();
        assert!(matches!(error, DecisionError::Refused(_)), "{error:?}");
        assert_eq!(error.to_string(), format!("{}: {reason}", file.display()));
        assert_eq!(rig.org.status().unwrap(), cursor, "nothing is recorded");
    }
    // An agent is refused as an agent: no file of the owner's is read for it.
    let error = guarded(rig.handle.away(SwitchParams {}, pane("%7")))
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "only the owner may set away or back");
    rig.settings("# tuned after the first day away\nstacking_cap = 3\n");
    let mut events = rig.watch("owner.away");
    let away = rig.away().await;
    let caps = BTreeMap::from([(gone, 1), (p.clone(), 3)]);
    assert_eq!(away.stacking_caps, caps);
    assert_eq!(
        next(&mut events).await.body,
        json!({"by":"owner","stacking_caps":{"gone":1,"p":3}})
    );
    // Already away: the file is not read again, broken or changed.
    let cursor = rig.org.status().unwrap();
    for text in ["stacking_cap = 0\n", "stacking_cap = 9\n"] {
        rig.settings(text);
        let again = rig.away().await;
        assert_eq!((again.changed, &again.stacking_caps), (false, &caps));
    }
    assert_eq!(rig.org.status().unwrap(), cursor);
    // The next absence takes the file as it is then; no file is one.
    guarded(rig.handle.back(SwitchParams {}, owner()))
        .await
        .unwrap();
    std::fs::remove_file(&file).unwrap();
    assert_eq!(rig.away().await.stacking_caps[&p], 1);
    rig.stop().await;
}

// Break caught: unregistered methods, parameters passed over, an agent setting
// the switch, or a refusal reported under the wrong error code.
#[tokio::test]
async fn methods_are_the_owners_take_no_parameters_and_report_the_switch() {
    let rig = Rig::new().await;
    let mut services = MethodServices::new(crate::registry::spawn_registry());
    services.org = rig.org.clone();
    services.decisions = Some(rig.handle.clone());
    let methods = router(services).unwrap();
    let call = |context: CallContext, method: &'static str, params: Value| {
        let methods = &methods;
        async move { guarded(methods.call(context, method, params)).await }
    };
    for method in ["owner.away", "owner.back"] {
        let error = call(pane("%7"), method, json!({})).await.unwrap_err().0;
        assert_eq!(
            (error.code.as_str(), error.message.as_str()),
            ("refused", "only the owner may set away or back"),
            "{method}"
        );
        for params in [json!({"by":"owner"}), json!({"stacking_cap":2}), json!([])] {
            let error = call(owner(), method, params.clone()).await.unwrap_err().0;
            assert_eq!(error.code, "invalid_params", "{method} {params}");
        }
        let error = call(pane("not a pane"), method, json!({}))
            .await
            .unwrap_err()
            .0;
        assert_eq!(error.code, "invalid_params", "{method}");
    }
    assert_eq!(rig.state().await.owner().away_since, None);
    // The list result says nothing of the switch while the owner is present.
    let counts = json!({"decisions":[],"pending_count":0,"provisional_count":0,"next_after":null});
    let list = |context: CallContext| call(context, "decision.list", json!({"count_only":true}));
    assert_eq!(list(owner()).await.unwrap(), counts);
    let mut back = call(owner(), "owner.back", Value::Null).await.unwrap();
    let digest = back.as_object_mut().unwrap().remove("digest").unwrap();
    assert_eq!(digest["absence"], Value::Null);
    assert_eq!(digest["rows"], json!([]));
    assert_eq!(
        back,
        json!({"changed":false,"away_since":null,"provisional_count":0,"pending_count":0})
    );
    // A pane that is no agent's counts as the owner, as for `decision.resolve`.
    let away = call(pane("%99"), "owner.away", json!({})).await.unwrap();
    let since = rig.state().await.owner().away_since.unwrap();
    assert_eq!(
        away,
        json!({"changed":true,"away_since":since,"stacking_caps":{"p":1},"pending_count":0})
    );
    let again = call(owner(), "owner.away", Value::Null).await.unwrap();
    assert_eq!(again["changed"], false);
    assert_eq!(again["away_since"], away["away_since"]);
    // While away every caller's list says so: `atm status` reads it there.
    let mut marked = counts.clone();
    marked["away"] = json!(true);
    assert_eq!(list(pane("%7")).await.unwrap(), marked);
    assert_eq!(
        call(owner(), "decision.list", json!({})).await.unwrap(),
        marked
    );
    let mut back = call(owner(), "owner.back", json!({})).await.unwrap();
    let digest = back.as_object_mut().unwrap().remove("digest").unwrap();
    assert_eq!(digest["absence"]["since"], json!(since));
    assert_eq!(digest["rows"], json!([]));
    assert_eq!(
        back,
        json!({"changed":true,"away_since":since,"provisional_count":0,"pending_count":0})
    );
    assert_eq!(list(owner()).await.unwrap(), counts);
    // A daemon without the decision service advertises both and sets nothing.
    let disabled = router(MethodServices::new(crate::registry::spawn_registry())).unwrap();
    for method in ["owner.away", "owner.back"] {
        let error = guarded(disabled.call(owner(), method, json!({})))
            .await
            .unwrap_err()
            .0;
        assert_eq!(error.code, "unavailable", "{method}");
        let error = guarded(disabled.call(owner(), method, json!({"x":1})))
            .await
            .unwrap_err()
            .0;
        assert_eq!(error.code, "invalid_params", "{method}");
    }
    rig.stop().await;
}

// Synthetic: replay mutations also use other valid `by` values to pin relay
// provenance as the first branch, not the old requester/system distinction.
// Break caught: relayed Details/self-relays receiving an away-table notice,
// or changed text causing a second durable message after replay.
#[test]
fn relayed_notices_are_exact_and_deduplicate_every_delivery_state() {
    for intent in [false, true] {
        for away in [false, true] {
            for relayer in ["p/worker", "p/chief"] {
                for by in [Address::Owner, Address::System, worker()] {
                    let mut events = vec![
                        OrgEvent::DecisionRequested(request(true, intent, Some("yes"))),
                        OrgEvent::AgentAdopted {
                            agent_id: "p/chief".parse().unwrap(),
                            pane: "%8".into(),
                            session_id: None,
                        },
                    ];
                    if away {
                        events.push(away_fact());
                    }
                    events.push(OrgEvent::DecisionTakenProvisionally(ProvisionalChoice {
                        decision_id: DecisionId::new(3),
                        option: "no".into(),
                        by: by.clone(),
                        relayed_by: Some(relayer.parse().unwrap()),
                    }));
                    let state = folded(&events);
                    let suffix = if intent {
                        " Finish your task in progress on this option. Start nothing further on it: a task built on it names d3 in rests_on and waits until the owner has confirmed d3."
                    } else {
                        " Carry on; he may still overrule it."
                    };
                    let content = message(MessageKind::Answer, &format!("Decision d3 taken provisionally: no\nRelayed by {relayer} as the owner's spoken answer; he has not confirmed it himself.{suffix}"));
                    assert_eq!(
                        missing_notices(&state).as_slice(),
                        std::slice::from_ref(&content),
                        "{intent} {away} {relayer} {by}"
                    );
                    let mut sent = state.clone();
                    sent.apply(100, now(), &OrgEvent::MessageSent(content))
                        .unwrap();
                    assert!(missing_notices(&sent).is_empty());
                    let id = atm_org::MessageId::new(100);
                    for event in [
                        OrgEvent::MessageDelivered {
                            message_id: id,
                            session_id: "fixture".into(),
                            confirmation: "fixture".into(),
                        },
                        OrgEvent::MessageDeliveryFailed {
                            message_id: id,
                            reason: "fixture".into(),
                            final_failure: true,
                        },
                        OrgEvent::MessageRead {
                            message_ids: vec![id],
                        },
                    ] {
                        let mut updated = sent.clone();
                        updated.apply(101, now(), &event).unwrap();
                        assert!(missing_notices(&updated).is_empty());
                    }
                    let mut decision = state.decision(&DecisionId::new(3)).unwrap().clone();
                    for status in [DecisionStatus::Resolved, DecisionStatus::Overruled] {
                        decision.status = status;
                        assert!(
                            notice(&decision, away).is_none(),
                            "stale provenance is not provisional"
                        );
                    }
                }
            }
        }
    }
}

// Break caught: relay falling into owner Resolve. With no hook observers,
// each refusal must preserve the entire committed cursor, not just decisions.
#[tokio::test]
async fn relay_rpc_refusals_append_nothing_without_hook_telemetry() {
    let rig = Rig::new().await;
    let org = rig.org.clone();
    let handle = rig.handle.clone();
    // Catch assertion unwinds in the scenario task so stop still joins the
    // bridge before propagating a failure. No harness/usage observers exist.
    let scenario = tokio::spawn(async move {
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
        let id = handle
            .request(params(request(true, true, Some("yes"))), pane("%7"))
            .await
            .unwrap()
            .decision_id;
        let mut services = MethodServices::new(crate::registry::spawn_registry());
        services.org = org.clone();
        services.decisions = Some(handle);
        let methods = router(services).unwrap();
        let refuse = |caller: CallContext, params: Value, code: &'static str| {
            let methods = &methods;
            let org = &org;
            async move {
                let before = org.status().unwrap();
                let error = guarded(methods.call(caller, "decision.resolve", params))
                    .await
                    .unwrap_err();
                assert_eq!(error.0.code, code, "{error}");
                assert_eq!(org.status().unwrap(), before, "refusal appended a fact");
            }
        };
        let answer = json!({"id":id,"option":"no","relay":true});
        refuse(pane("%7"), answer.clone(), "refused").await;
        guarded(methods.call(owner(), "owner.relay", json!({"agent":"p/worker"})))
            .await
            .unwrap();
        for caller in [owner(), pane("%99"), pane("%8")] {
            refuse(caller, answer.clone(), "refused").await;
        }
        for caller in [pane("%00"), pane("not a pane")] {
            refuse(caller, answer.clone(), "invalid_params").await;
        }
        for (key, value) in [
            ("note", Value::Null),
            ("note", json!("")),
            ("note", json!("spoken")),
            ("standing_rule", json!(false)),
            ("standing_rule", json!(true)),
        ] {
            let mut params = answer.clone();
            params[key] = value;
            refuse(pane("%7"), params, "invalid_params").await;
        }
        org.execute(
            OrgCommand::Down {
                agent_id: "p/worker".parse().unwrap(),
                reason: "fixture".into(),
            },
            now(),
        )
        .await
        .unwrap();
        refuse(pane("%7"), answer, "refused").await;
        let state = org.snapshot().await.unwrap();
        let decision = state.decision(&id).unwrap();
        assert_eq!(decision.status, DecisionStatus::Pending);
        assert!(decision.provisional.is_none() && decision.resolution.is_none());
    });
    let outcome = guarded(scenario).await;
    rig.stop().await;
    outcome.unwrap();
}

// Break caught: an oversized relay result disguises its committed take as a
// refusal, or an identical retry appends another take/notice.
#[tokio::test]
async fn relay_oversized_committed_result_reports_commit_and_retries_without_appending() {
    let rig = Rig::new().await;
    let org = rig.org.clone();
    let handle = rig.handle.clone();
    let scenario = tokio::spawn(async move {
        let mut services = MethodServices::new(crate::registry::spawn_registry());
        services.org = org.clone();
        services.decisions = Some(handle);
        let methods = router(services).unwrap();
        let mut requested = params(request(true, true, Some("yes")));
        // Valid path byte lengths expand sixfold when their controls are escaped.
        requested.files = vec!["\u{1}".repeat(4096); 32];
        let asked = guarded(methods.call(
            pane("%7"),
            "decision.request",
            serde_json::to_value(requested).unwrap(),
        ))
        .await
        .unwrap();
        let id: DecisionId = serde_json::from_value(asked["decision_id"].clone()).unwrap();
        guarded(methods.call(owner(), "owner.relay", json!({"agent":"p/worker"})))
            .await
            .unwrap();
        let mut takes = org
            .watch(
                vec!["decision.taken_provisionally".into()],
                org.status().unwrap(),
            )
            .unwrap();
        let mut notices = org
            .watch(vec!["msg.sent".into()], org.status().unwrap())
            .unwrap();
        let answer = json!({"id":id,"option":"no","relay":true});
        let first = guarded(methods.call(pane("%7"), "decision.resolve", answer.clone()))
            .await
            .unwrap_err();
        let take = next(&mut takes).await;
        assert_eq!(take.kind, "decision.taken_provisionally");
        assert_eq!(
            take.body,
            json!({"decision_id":id,"option":"no","by":"owner","relayed_by":"p/worker"})
        );
        let notice = next(&mut notices).await;
        assert_eq!(notice.body["to"], "p/worker");
        assert!(notice.body["body"].as_str().unwrap().starts_with(&format!(
            "Decision {id} taken provisionally: no\nRelayed by p/worker"
        )));
        let state = org.snapshot().await.unwrap();
        let decision = state.decision(&id).unwrap();
        let bytes = serde_json::to_vec(&super::super::decisions::DecisionEntry::from(decision))
            .unwrap()
            .len();
        assert!(
            bytes > 524288,
            "entry must exceed the result bound: {bytes}"
        );
        assert_eq!(decision.status, DecisionStatus::TakenProvisionally);
        assert!(decision.resolution.is_none());
        assert_eq!(state.messages().count(), 1);
        let before = org.status().unwrap();
        let again = guarded(methods.call(pane("%7"), "decision.resolve", answer))
            .await
            .unwrap_err();
        assert_eq!(org.status().unwrap(), before, "retry appended a fact");
        eprintln!(
            "serialized entry bytes: {bytes}; one take and one notice, retry appended nothing"
        );
        eprintln!(
            "first caller response: {}: {}",
            first.0.code, first.0.message
        );
        eprintln!(
            "repeat caller response: {}: {}",
            again.0.code, again.0.message
        );
        for error in [first, again] {
            assert_eq!(error.0.code, "refused");
            assert_eq!(
                error.0.message,
                format!("decision {id} committed; result was not confirmed: decision exceeds 524288 bytes")
            );
        }
    });
    finish_scenario(rig, scenario).await;
}

// Break caught: owner.relay treats canonical unbound or Down panes as agents,
// unlike the shared away attribution, or writes a fact merely to show the slot.
#[tokio::test]
async fn owner_relay_unbound_and_down_panes_are_owner_for_all_actions() {
    let rig = Rig::new().await;
    let org = rig.org.clone();
    let handle = rig.handle.clone();
    let scenario = tokio::spawn(async move {
        let mut services = MethodServices::new(crate::registry::spawn_registry());
        services.org = org.clone();
        services.decisions = Some(handle);
        let methods = router(services).unwrap();
        for down in [false, true] {
            if down {
                org.execute(
                    OrgCommand::Down {
                        agent_id: "p/worker".parse().unwrap(),
                        reason: "fixture".into(),
                    },
                    now(),
                )
                .await
                .unwrap();
            }
            let caller = pane(if down { "%7" } else { "%99" });
            assert!(org
                .snapshot()
                .await
                .unwrap()
                .agent_for_pane(&caller.from_pane.as_ref().unwrap().0)
                .is_none());
            for (params, expected, appends) in [
                (json!({}), json!({"agent":null,"changed":false}), 0),
                (
                    json!({"agent":"p/worker"}),
                    json!({"agent":"p/worker","changed":true}),
                    1,
                ),
                (json!({}), json!({"agent":"p/worker","changed":false}), 0),
                (
                    json!({"clear":true}),
                    json!({"agent":null,"changed":true}),
                    1,
                ),
            ] {
                let before = org.status().unwrap();
                assert_eq!(
                    guarded(methods.call(caller.clone(), "owner.relay", params.clone()))
                        .await
                        .unwrap(),
                    expected,
                    "Down={down}: {params}"
                );
                assert_eq!(
                    org.status().unwrap(),
                    before + appends,
                    "Down={down}: {params}"
                );
            }
        }
    });
    finish_scenario(rig, scenario).await;
}

// Break caught: a malformed pane falls back to Owner and can show/set/clear
// the slot instead of returning invalid_params without committing anything.
#[tokio::test]
async fn owner_relay_malformed_panes_refuse_all_actions_without_appends() {
    let rig = Rig::new().await;
    let org = rig.org.clone();
    let handle = rig.handle.clone();
    let scenario = tokio::spawn(async move {
        let mut services = MethodServices::new(crate::registry::spawn_registry());
        services.org = org.clone();
        services.decisions = Some(handle);
        let methods = router(services).unwrap();
        for caller in ["%00", "%", "session:1.2", " %7", "%７"] {
            for params in [
                json!({}),
                json!({"agent":"p/worker"}),
                json!({"clear":true}),
            ] {
                let before = org.status().unwrap();
                let error = guarded(methods.call(pane(caller), "owner.relay", params.clone()))
                    .await
                    .unwrap_err();
                assert_eq!(error.0.code, "invalid_params", "{caller:?}: {params}");
                assert_eq!(org.status().unwrap(), before, "{caller:?}: {params}");
            }
        }
        assert!(org.snapshot().await.unwrap().owner().relay.is_none());
    });
    finish_scenario(rig, scenario).await;
}

// Break caught: a disabled daemon omitting the capability or accepting an
// invalid designation shape before checking availability.
#[tokio::test]
async fn owner_relay_schema_and_disabled_service_fail_closed() {
    let methods = router(MethodServices::new(crate::registry::spawn_registry())).unwrap();
    assert!(methods.names().iter().any(|m| m == "owner.relay"));
    for params in [
        json!({}),
        Value::Null,
        json!({"agent":"p/chief"}),
        json!({"clear":true}),
    ] {
        assert_eq!(
            guarded(methods.call(owner(), "owner.relay", params))
                .await
                .unwrap_err()
                .0
                .code,
            "unavailable"
        );
    }
    for params in [
        json!([]),
        json!({"by":"owner"}),
        json!({"agent":"chief"}),
        json!({"agent":"p/chief","clear":true}),
    ] {
        assert_eq!(
            guarded(methods.call(owner(), "owner.relay", params))
                .await
                .unwrap_err()
                .0
                .code,
            "invalid_params"
        );
    }
}
