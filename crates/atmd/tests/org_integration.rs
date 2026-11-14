//! Durable organisation service: real files, isolated state, no harness or tmux.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use atm_core::{Harness, Money, SessionId};
use atm_org::{OrgCommand, OrgEvent, SessionSummary};
use atmd::eventlog::{read_from, EventLog, Stream};
use atmd::org::OrgHandle;
use chrono::{DateTime, Utc};
use serde_json::json;
use std::{path::Path, time::Duration};
use tokio_util::sync::CancellationToken;

fn now() -> DateTime<Utc> {
    DateTime::from_timestamp(1_700_000_000, 0).unwrap()
}

fn project() -> OrgCommand {
    OrgCommand::RegisterProject {
        project_id: "demo".parse().unwrap(),
        root: "/demo".into(),
        move_root: false,
    }
}

async fn reopen(path: &Path, cancel: CancellationToken) -> OrgHandle {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let org = OrgHandle::open(path.to_owned(), cancel.clone()).await;
            if org.status().is_ok() {
                return org;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("writer released the lock")
}

// Missing apply/replay or an incorrectly encoded cost loses these facts.
#[tokio::test]
async fn restart_replays_full_org_and_summaries() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let org = OrgHandle::open(dir.path().to_owned(), cancel.clone()).await;
    org.execute(project(), now()).await.unwrap();
    org.execute(
        OrgCommand::Adopt {
            agent_id: "demo/lead".parse().unwrap(),
            pane: "%1".into(),
            session_id: Some("s1".into()),
        },
        now(),
    )
    .await
    .unwrap();
    let summary = SessionSummary {
        session_id: SessionId::new("s1"),
        pane: Some("%1".into()),
        agent_id: Some("demo/lead".parse().unwrap()),
        harness: Harness::Pi,
        model: Some("example-model".into()),
        cost: Some(Money::from_microdollars(10_650)),
        first_prompt: Some("Read note.txt and reply.".into()),
        started: now(),
        ended: None,
    };
    org.execute(OrgCommand::RecordSummary(summary.clone()), now())
        .await
        .unwrap();
    assert!(org
        .execute(OrgCommand::RecordSummary(summary), now())
        .await
        .unwrap()
        .is_empty());
    let before = org.snapshot().await.unwrap();
    assert_eq!(org.status().unwrap(), 3);
    cancel.cancel();
    drop(org);
    let cancel = CancellationToken::new();
    let restarted = reopen(dir.path(), cancel.clone()).await;
    assert_eq!(restarted.snapshot().await.unwrap(), before);
    let rows: Vec<_> = read_from(dir.path(), Stream::Events, 0)
        .map(Result::unwrap)
        .collect();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[2].body["cost"], json!({"microdollars": 10650}));
    cancel.cancel();
}

// Deciding on two snapshots outside the actor would log conflicting pane owners.
#[tokio::test]
async fn concurrent_pane_claims_have_one_winner() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let org = OrgHandle::open(dir.path().to_owned(), cancel.clone()).await;
    org.execute(project(), now()).await.unwrap();
    let mut tasks = tokio::task::JoinSet::new();
    for name in ["one", "two"] {
        let org = org.clone();
        tasks.spawn(async move {
            org.execute(
                OrgCommand::Adopt {
                    agent_id: format!("demo/{name}").parse().unwrap(),
                    pane: "%1".into(),
                    session_id: None,
                },
                now(),
            )
            .await
        });
    }
    let mut successes = 0;
    while let Some(result) = tasks.join_next().await {
        successes += usize::from(result.unwrap().is_ok());
    }
    assert_eq!(successes, 1);
    assert_eq!(org.snapshot().await.unwrap().agents().count(), 1);
    assert_eq!(read_from(dir.path(), Stream::Events, 0).count(), 2);
    cancel.cancel();
}

fn segment(dir: &Path, first: u64, bytes: &[u8]) {
    let path = dir.join("log/events");
    std::fs::create_dir_all(&path).unwrap();
    std::fs::write(path.join(format!("{first:08}.jsonl")), bytes).unwrap();
}

fn record(seq: u64, kind: &str, body: serde_json::Value) -> Vec<u8> {
    format!(
        "{}\n",
        json!({"seq":seq,"ts":now(),"kind":kind,"body":body})
    )
    .into_bytes()
}

#[tokio::test]
async fn old_segment_and_known_body_corruption_disable_org() {
    for old in [
        b"not json\n".to_vec(),
        record(
            1,
            "agent.down",
            json!({"agent_id":"absent/a","reason":"gone"}),
        ),
        record(1, "session.summary", json!({"cost":"invalid"})),
    ] {
        let dir = tempfile::tempdir().unwrap();
        segment(dir.path(), 1, &old);
        segment(dir.path(), 2, &record(2, "future.kind", json!({})));
        let org = OrgHandle::open(dir.path().to_owned(), CancellationToken::new()).await;
        let error = org.snapshot().await.unwrap_err().to_string();
        assert!(!error.is_empty());
        assert!(org.execute(project(), now()).await.is_err());
        assert_eq!(
            std::fs::read(dir.path().join("log/events/00000001.jsonl")).unwrap(),
            old
        );
    }
}

#[tokio::test]
async fn newer_format_and_locked_log_are_clear_unavailable_errors() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("log")).unwrap();
    std::fs::write(dir.path().join("log/format"), "2\n").unwrap();
    let org = OrgHandle::open(dir.path().to_owned(), CancellationToken::new()).await;
    assert!(org
        .status()
        .unwrap_err()
        .to_string()
        .contains("unsupported log format"));
    let dir = tempfile::tempdir().unwrap();
    let _lock = EventLog::open(dir.path(), Stream::Events).unwrap();
    let org = OrgHandle::open(dir.path().to_owned(), CancellationToken::new()).await;
    assert!(org.status().unwrap_err().to_string().contains("locked"));
}

#[tokio::test]
async fn unknown_kind_advances_cursor_without_changing_org() {
    let dir = tempfile::tempdir().unwrap();
    let event = OrgEvent::ProjectRegistered {
        project_id: "demo".parse().unwrap(),
        root: "/demo".into(),
    };
    segment(
        dir.path(),
        1,
        &[
            record(1, event.kind(), event.to_body()),
            record(2, "future.kind", json!(null)),
        ]
        .concat(),
    );
    let cancel = CancellationToken::new();
    let org = OrgHandle::open(dir.path().to_owned(), cancel.clone()).await;
    assert_eq!(org.status().unwrap(), 2);
    assert_eq!(org.snapshot().await.unwrap().projects().count(), 1);
    assert!(org.execute(project(), now()).await.unwrap().is_empty());
    assert_eq!(org.status().unwrap(), 2);
    cancel.cancel();
}
