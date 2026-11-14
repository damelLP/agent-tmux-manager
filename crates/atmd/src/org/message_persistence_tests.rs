//! The message service on a real organisation log: the production watch, its
//! startup replay and a restart. Real time and real files, so no paused clock;
//! every step is synchronized on a returned call, and the 60 s bounds are hang
//! guards only.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::{message_service_tests::runtime, messages::*, OrgHandle};
use crate::server::methods::{CallContext, CallerId, CallerPane};
use atm_org::{
    delivery::{next_delivery, DeliveryStep, FailureReason},
    OrgCommand,
};
use chrono::{DateTime, Utc};
use std::{future::Future, path::Path, time::Duration};
use tokio_util::sync::CancellationToken;

fn utc() -> DateTime<Utc> {
    DateTime::from_timestamp(1_700_000_000, 0).unwrap()
}

fn caller(pane: Option<&str>) -> CallContext {
    CallContext {
        client_id: CallerId("test".into()),
        from_pane: pane.map(|pane| CallerPane(pane.into())),
    }
}

async fn guarded<T>(work: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(60), work)
        .await
        .expect("hang guard")
}

/// One daemon lifetime over a state directory.
struct Process {
    org: OrgHandle,
    rt: MessageRuntime,
    messages: MessageHandle,
    cancel: CancellationToken,
}

impl Process {
    async fn start(dir: &Path) -> Self {
        let cancel = CancellationToken::new();
        let org = OrgHandle::open(dir.into(), cancel.clone()).await;
        // The committed cursor is read once: history's replay gate and the
        // service's startup fence are the same number.
        let high_water = org.status().unwrap();
        let rt = runtime(org.clone(), high_water, &cancel);
        let (messages, _task) = spawn_messages(rt.clone(), high_water);
        guarded(messages.initialized()).await.unwrap();
        Self {
            org,
            rt,
            messages,
            cancel,
        }
    }

    async fn run(&self, command: OrgCommand) -> u64 {
        let committed = guarded(self.org.execute(command, utc())).await.unwrap();
        let seq = committed.last().unwrap().seq;
        guarded(self.rt.history.folded(seq)).await.unwrap();
        seq
    }

    async fn send(&self, from: Option<&str>, to: Address, body: &str) -> MessageId {
        let params = SendParams {
            to,
            kind: MessageKind::Note,
            body: body.into(),
            attachments: vec![],
            task: None,
        };
        guarded(self.messages.send(params, caller(from)))
            .await
            .unwrap()
            .message_id
    }

    async fn inbox(&self, from: Option<&str>) -> InboxResult {
        let params = InboxParams {
            recipient: None,
            unread_only: false,
            count_only: false,
            id: None,
            after: None,
            limit: 100,
        };
        guarded(self.messages.inbox(params, caller(from)))
            .await
            .unwrap()
    }

    fn step(&self, id: MessageId) -> DeliveryStep {
        let snapshot = self.rt.history.snapshot(id, utc()).unwrap();
        next_delivery(&snapshot.message, &snapshot.live, utc(), &self.rt.policy).unwrap()
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// A restart reads the log the previous process left. Copying it gives the
/// next process its own stream lock: a reopen in one process can briefly find
/// the old writer's lock still held.
fn copy_log(from: &Path, to: &Path) {
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            std::fs::create_dir_all(&target).unwrap();
            copy_log(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

#[tokio::test]
async fn restart_replays_messages_read_state_and_attempts() {
    let before = tempfile::tempdir().unwrap();
    let first = Process::start(before.path()).await;
    first
        .run(OrgCommand::RegisterProject {
            project_id: "demo".parse().unwrap(),
            root: "/demo".into(),
            move_root: false,
        })
        .await;
    let worker: AgentId = "demo/worker".parse().unwrap();
    first
        .run(OrgCommand::Adopt {
            agent_id: worker.clone(),
            pane: "%1".into(),
            session_id: Some("s1".into()),
        })
        .await;
    let report = first.send(Some("%1"), Address::Owner, "report").await;
    let unread = first.send(Some("%1"), Address::Owner, "unread").await;
    let read = guarded(
        first
            .messages
            .ack(AckParams { ids: vec![report] }, caller(None)),
    )
    .await
    .unwrap();
    assert_eq!(read.count, 1);
    let queued = first
        .send(None, Address::Agent(worker.clone()), "queued")
        .await;
    let delivered = first
        .send(None, Address::Agent(worker.clone()), "delivered")
        .await;
    let attempted = first
        .send(None, Address::Agent(worker.clone()), "attempted")
        .await;
    for id in [delivered, attempted] {
        first
            .run(OrgCommand::DeliveryAttempted {
                message_id: id,
                attempt: 1,
                pane: "%1".into(),
            })
            .await;
    }
    first
        .run(OrgCommand::Delivered {
            message_id: delivered,
            session_id: "s1".into(),
            confirmation: "prompt_submit_exact_reference".into(),
        })
        .await;
    // In the process that made it, the attempt is live, not a restart hold.
    assert!(!matches!(
        first.step(attempted),
        DeliveryStep::Blocked {
            reason: FailureReason::Restarted
        }
    ));

    let after = tempfile::tempdir().unwrap();
    copy_log(before.path(), after.path());
    let second = Process::start(after.path()).await;

    let owner = second.inbox(None).await;
    assert_eq!(
        owner
            .messages
            .iter()
            .map(|m| m.message_id)
            .collect::<Vec<_>>(),
        [report, unread]
    );
    assert!(owner.messages[0].read_at.is_some());
    assert!(owner.messages[1].read_at.is_none());
    assert_eq!(owner.unread_count, 1);
    assert_eq!(
        owner.messages[0].content.from,
        Address::Agent(worker.clone())
    );

    let agent = second.inbox(Some("%1")).await;
    let status: Vec<_> = agent
        .messages
        .iter()
        .map(|m| (m.message_id, m.status))
        .collect();
    assert_eq!(
        status,
        [
            (queued, DeliveryStatus::Queued),
            (delivered, DeliveryStatus::Delivered),
            (attempted, DeliveryStatus::Delivering),
        ]
    );
    assert_eq!(
        agent.messages[1]
            .delivery
            .session_id
            .as_ref()
            .unwrap()
            .as_str(),
        "s1"
    );

    // Evidence expects what can still be delivered, and nothing that was.
    assert!(second.rt.evidence.snapshot(queued).is_ok());
    assert!(second.rt.evidence.snapshot(attempted).is_ok());
    assert!(second.rt.evidence.snapshot(delivered).is_err());
    // An attempt of the previous process is held for the owner; a message
    // that was never attempted is not.
    assert_eq!(
        second.step(attempted),
        DeliveryStep::Blocked {
            reason: FailureReason::Restarted
        }
    );
    assert!(!matches!(second.step(queued), DeliveryStep::Blocked { .. }));

    // After the replay the same watch delivers live events.
    let live = second
        .send(Some("%1"), Address::Owner, "after restart")
        .await;
    assert!(live > attempted);
    assert_eq!(second.inbox(None).await.messages.len(), 3);
    let repeated = guarded(
        second
            .messages
            .ack(AckParams { ids: vec![report] }, caller(None)),
    )
    .await
    .unwrap();
    assert_eq!(repeated.count, 0);
}

#[tokio::test]
async fn empty_log_starts_ready() {
    let dir = tempfile::tempdir().unwrap();
    let process = Process::start(dir.path()).await;
    assert_eq!(process.org.status().unwrap(), 0);
    let id = process
        .send(None, Address::Owner, "first event of the log")
        .await;
    assert_eq!(id, MessageId::new(1));
    assert_eq!(process.inbox(None).await.unread_count, 1);
}

#[tokio::test]
async fn unavailable_organisation_gives_a_disabled_service_with_its_cause() {
    let cancel = CancellationToken::new();
    let rt = runtime(
        OrgHandle::disabled("state directory is read-only"),
        0,
        &cancel,
    );
    let (messages, task) = spawn_messages(rt, 0);
    guarded(task).await.unwrap();
    for error in [
        messages.initialized().await.unwrap_err(),
        messages
            .inbox(
                InboxParams {
                    recipient: None,
                    unread_only: false,
                    count_only: true,
                    id: None,
                    after: None,
                    limit: 1,
                },
                caller(None),
            )
            .await
            .unwrap_err(),
    ] {
        assert!(matches!(error, MessageError::Unavailable(_)));
        assert!(
            error.to_string().contains("state directory is read-only"),
            "{error}"
        );
    }
    cancel.cancel();
}
