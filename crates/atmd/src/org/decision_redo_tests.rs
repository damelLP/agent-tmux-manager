//! Synthetic writer/bridge crash boundaries, synchronized by actor/runtime events.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::tests::guarded;
use crate::eventlog::{Envelope, EventLogError};
use crate::org::{decisions::spawn_decisions, messages::MessageClock, OrgHandle};
use atm_org::*;
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

fn seed() -> OrgState {
    let mut state = OrgState::default();
    for line in include_str!("../../../atm-org/tests/fixtures/return_digest.jsonl").lines() {
        let value: serde_json::Value = serde_json::from_str(line).unwrap();
        let event = OrgEvent::from_parts(value["kind"].as_str().unwrap(), &value["body"])
            .unwrap()
            .unwrap();
        state
            .apply(
                value["seq"].as_u64().unwrap(),
                value["ts"].as_str().unwrap().parse().unwrap(),
                &event,
            )
            .unwrap();
    }
    state
}
fn start(
    state: OrgState,
    mut seq: u64,
    failure: u8,
    records: Arc<Mutex<Vec<Envelope>>>,
) -> (
    OrgHandle,
    CancellationToken,
    tokio::task::JoinHandle<()>,
    tempfile::TempDir,
) {
    let dir = tempfile::tempdir().unwrap();
    let (requests, receive) = mpsc::channel(64);
    let (health, status) = watch::channel(Ok(seq));
    let (committed, _) = broadcast::channel(1024);
    let cancel = CancellationToken::new();
    let mut overruled = false;
    let actor = tokio::spawn(crate::org::actor::run(
        state,
        receive,
        health,
        committed.clone(),
        cancel.clone(),
        move |mut record| {
            if record.kind == "decision.overruled" && failure == 1 {
                return std::future::ready(Err(EventLogError::TimeoutBeforeHandover));
            }
            if record.kind == "msg.sent" && overruled && failure == 2 {
                return std::future::ready(Err(EventLogError::Closed));
            }
            overruled |= record.kind == "decision.overruled";
            seq += 1;
            record.seq = seq;
            records.lock().unwrap().push(record);
            std::future::ready(Ok(seq))
        },
    ));
    let org = OrgHandle {
        requests,
        health: status,
        committed,
        reads: Some(crate::org::watch::ReadAccess {
            reader: crate::org::storage::Reader::start(dir.path().into()).unwrap(),
        }),
    };
    (org, cancel, actor, dir)
}
fn clock() -> MessageClock {
    MessageClock::new(
        tokio::time::Instant::now(),
        "2026-11-16T18:00:00Z".parse().unwrap(),
    )
}
fn resolve() -> OrgCommand {
    OrgCommand::Decision(DecisionCommand::Resolve(DecisionResolution {
        decision_id: DecisionId::new(11),
        option: "large".into(),
        by: Address::Owner,
        note: None,
        standing_rule: false,
    }))
}
fn notes(state: &OrgState) -> Vec<MessageContent> {
    state
        .messages()
        .filter(|m| m.content.body.starts_with("Task t14 needs redo"))
        .map(|m| m.content.clone())
        .collect()
}

// Break caught: overrule fails to wake the one bridge, recipients vanish, or replay duplicates notes.
#[test]
fn decision_redo_notice_reconcile_after_overrule_and_restart() {
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
        let records=Arc::new(Mutex::new(vec![]));let (org,cancel,actor,_dir)=start(seed(),31,0,records);
        let (handle,bridge)=spawn_decisions(org.clone(),clock(),cancel.clone());guarded(handle.initialized()).await.unwrap();
        org.execute_command(resolve(),clock().now().unwrap(),Some("%99".into())).await.unwrap();
        let (reply,reached_idle)=oneshot::channel();*idle.lock().unwrap()=Some(reply);guarded(reached_idle).await.unwrap();
        let state=org.snapshot().await.unwrap();let sent=notes(&state);assert_eq!(sent.len(),2,"overrule must enqueue redo notes without another event");
        assert_eq!(sent.iter().map(|m|m.to.to_string()).collect::<Vec<_>>(),["owner","alpha/builder"]);
        assert_eq!(sent.first().unwrap().body,"Task t14 needs redo after decision d11 was overruled: large\nIt was reopened and unassigned. Read the new answer before assigning or submitting it again. No files were reverted.");
        assert_eq!(state.decision_redo(DecisionId::new(11)).unwrap().caller_pane.as_deref(),Some("%99"));
        let seq=org.status().unwrap();cancel.cancel();guarded(bridge).await.unwrap();guarded(actor).await.unwrap();
        let (org,cancel,actor,_dir)=start(state,seq,0,Arc::new(Mutex::new(vec![])));let (handle,bridge)=spawn_decisions(org.clone(),clock(),cancel.clone());guarded(handle.initialized()).await.unwrap();
        assert_eq!(notes(&org.snapshot().await.unwrap()),sent);cancel.cancel();guarded(bridge).await.unwrap();guarded(actor).await.unwrap();
    });
}

// Break caught: the log admits a closed decision with an accepted dependent prefix.
#[tokio::test]
async fn decision_redo_crash_at_append_has_no_accepted_prefix() {
    for failure in [1, 2] {
        let records = Arc::new(Mutex::new(vec![]));
        let (org, cancel, actor, _dir) = start(seed(), 31, failure, records.clone());
        let (handle, bridge) = spawn_decisions(org.clone(), clock(), cancel.clone());
        guarded(handle.initialized()).await.unwrap();
        let result = org.execute(resolve(), clock().now().unwrap()).await;
        if failure == 1 {
            assert!(result.is_err());
            cancel.cancel();
        } else {
            assert_eq!(result.unwrap().len(), 1);
        }
        guarded(bridge).await.unwrap();
        cancel.cancel();
        guarded(actor).await.unwrap();
        let mut state = seed();
        let records = records.lock().unwrap().clone();
        for record in &records {
            let event = OrgEvent::from_parts(&record.kind, &record.body)
                .unwrap()
                .unwrap();
            state.apply(record.seq, record.ts, &event).unwrap();
        }
        let task = state.task(&"t14".parse().unwrap()).unwrap();
        if failure == 1 {
            assert_eq!(task.state, TaskState::Accepted);
            assert_eq!(
                state.decision(&DecisionId::new(11)).unwrap().status,
                DecisionStatus::TakenProvisionally
            );
        } else {
            assert_eq!(task.state, TaskState::Open);
            assert_eq!(task.agent_id, None);
            assert_eq!(
                state.decision(&DecisionId::new(11)).unwrap().status,
                DecisionStatus::Overruled
            );
            let seq = records.last().unwrap().seq;
            let (org, cancel, actor, _dir) = start(state, seq, 0, Arc::new(Mutex::new(vec![])));
            let (handle, bridge) = spawn_decisions(org.clone(), clock(), cancel.clone());
            guarded(handle.initialized()).await.unwrap();
            assert_eq!(notes(&org.snapshot().await.unwrap()).len(), 2);
            cancel.cancel();
            guarded(bridge).await.unwrap();
            guarded(actor).await.unwrap();
        }
    }
}
