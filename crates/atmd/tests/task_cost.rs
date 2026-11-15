//! Accounting across real daemon boundaries; no model, tmux, sleeps or polling.
use atm_core::Money;
use atm_org::*;
use atmd::{
    org::OrgHandle,
    registry::spawn_registry,
    server::{
        methods::{router, MethodServices},
        ConnectionHandler,
    },
};
use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Value};
use std::{collections::HashMap, path::Path, sync::Arc, time::Duration as StdDuration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::RwLock,
};
use tokio_util::sync::CancellationToken;

fn now() -> DateTime<Utc> {
    "2026-01-01T00:00:00Z".parse().unwrap()
}
fn actor(who: &str) -> TaskActor {
    TaskActor {
        who: who.parse().unwrap(),
        lead: false,
    }
}
async fn seed(org: &OrgHandle) -> TaskId {
    let commands = [
        OrgCommand::RegisterProject { project_id: "p".parse().unwrap(), root: "/project".into(), move_root: false },
        OrgCommand::Adopt { agent_id: "p/build".parse().unwrap(), pane: "%1".into(), session_id: Some("s".into()) },
        OrgCommand::Task(TaskCommand::Create(serde_json::from_value(json!({"title":"work","kind":"work","creator":"owner","measure":{"kind":"owner"},"files":[]})).unwrap())),
        OrgCommand::Task(TaskCommand::Assign { task_id: "t3".parse().unwrap(), agent_id: "p/build".parse().unwrap(), by: actor("owner") }),
    ];
    for command in commands {
        org.execute(command, now()).await.unwrap();
    }
    "t3".parse().unwrap()
}
async fn sample(org: &OrgHandle, amount: i64, turn: bool) {
    org.execute(
        OrgCommand::ObserveTaskUsage(TaskUsageObservation {
            session_id: "s".into(),
            pane: Some("%1".into()),
            turn,
            cumulative_cost: Some(Money::from_microdollars(amount)),
            context_percent: Some(40),
        }),
        now(),
    )
    .await
    .unwrap();
}
async fn submit(org: &OrgHandle, id: &TaskId, what: TaskSubmission) {
    org.execute(
        OrgCommand::Task(TaskCommand::Submit {
            task_id: id.clone(),
            what,
            files: vec![],
            by: actor("p/build"),
        }),
        now() + Duration::seconds(4),
    )
    .await
    .unwrap();
}
async fn accept(org: &OrgHandle, id: &TaskId) -> Vec<atmd::eventlog::Envelope> {
    org.execute(
        OrgCommand::Task(TaskCommand::Verdict {
            task_id: id.clone(),
            verdict: TaskVerdict::Accepted,
            review_task: None,
            note: None,
            by: actor("owner"),
        }),
        now() + Duration::seconds(5),
    )
    .await
    .unwrap()
}
// Wait for the actual old writer's flock release, never for an arbitrary delay.
async fn reopen(path: &Path) -> (OrgHandle, CancellationToken) {
    use std::os::fd::AsRawFd;
    let lock = path.join("log/events/.lock");
    let (send, receive) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .spawn(move || {
            let f = std::fs::File::open(lock).unwrap();
            assert_eq!(unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) }, 0);
            drop(f);
            send.send(()).unwrap();
        })
        .unwrap();
    tokio::time::timeout(StdDuration::from_secs(60), receive)
        .await
        .unwrap()
        .unwrap();
    let cancel = CancellationToken::new();
    (
        OrgHandle::open(path.to_owned(), cancel.clone()).await,
        cancel,
    )
}

// Break caught: final recording on plan approval, duplicate acceptance, or restart
// resetting the baseline and billing the whole resumed session again.
#[tokio::test]
async fn running_cost_and_accepted_record_survive_restart() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let org = OrgHandle::open(dir.path().into(), cancel.clone()).await;
    let id = seed(&org).await;
    sample(&org, 100, true).await;
    sample(&org, 150, false).await;
    submit(&org, &id, TaskSubmission::Plan).await;
    assert_eq!(
        accept(&org, &id)
            .await
            .iter()
            .map(|e| e.kind.as_str())
            .collect::<Vec<_>>(),
        ["task.verdict"]
    );
    let before = org.snapshot().await.unwrap();
    assert!(before.task(&id).unwrap().cost.is_none());
    cancel.cancel();
    drop(org);
    let (org, cancel) = reopen(dir.path()).await;
    assert_eq!(org.snapshot().await.unwrap(), before);
    sample(&org, 200, true).await;
    submit(&org, &id, TaskSubmission::Result).await;
    assert_eq!(
        accept(&org, &id)
            .await
            .iter()
            .map(|e| e.kind.as_str())
            .collect::<Vec<_>>(),
        ["task.verdict", "task.cost_recorded"]
    );
    let state = org.snapshot().await.unwrap();
    let record = state
        .task(&id)
        .unwrap()
        .cost
        .clone()
        .expect("accepted record");
    assert_eq!(record.cost, Some(Money::from_microdollars(100)));
    assert_eq!(record.turns, Some(2));
    assert_eq!(
        state.task_cost(&id, now() + Duration::days(1)),
        Some(record.clone())
    );
    assert!(accept(&org, &id).await.is_empty());
    cancel.cancel();
    drop(org);
    let (org, cancel) = reopen(dir.path()).await;
    assert_eq!(
        org.snapshot()
            .await
            .unwrap()
            .task(&id)
            .unwrap()
            .cost
            .as_ref(),
        Some(&record)
    );
    cancel.cancel();
}

// Break caught: crash between the verdict and cost append leaves a permanent gap,
// or repair uses restart time instead of the original acceptance instant.
#[tokio::test]
async fn accepted_prefix_is_repaired_once_before_serving_reads() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("log/events");
    std::fs::create_dir_all(&log).unwrap();
    let prefix = include_str!("../../atm-org/tests/fixtures/task_cost_log.jsonl")
        .lines()
        .take(8)
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(log.join("00000001.jsonl"), prefix).unwrap();
    let cancel = CancellationToken::new();
    let org = OrgHandle::open(dir.path().into(), cancel.clone()).await;
    let state = org.snapshot().await.unwrap();
    let id = "t3".parse().unwrap();
    let record = state
        .task(&id)
        .unwrap()
        .cost
        .as_ref()
        .expect("repaired cost");
    assert_eq!(record.cost, Some(Money::from_microdollars(25_000)));
    assert_eq!(record.seconds_in_state.values().sum::<u64>(), 5);
    assert_eq!(org.status().unwrap(), 9);
    cancel.cancel();
    drop(org);
    let (org, cancel) = reopen(dir.path()).await;
    assert_eq!(org.snapshot().await.unwrap(), state);
    assert_eq!(org.status().unwrap(), 9);
    cancel.cancel();
}

// Break caught: only the judged task gets a record, not the automatically
// accepted review; or review bookkeeping invents a review round on the review.
#[tokio::test]
async fn verdict_records_both_work_and_automatically_accepted_review() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let org = OrgHandle::open(dir.path().into(), cancel.clone()).await;
    let work = seed(&org).await;
    submit(&org, &work, TaskSubmission::Result).await;
    org.execute(
        OrgCommand::Adopt {
            agent_id: "p/review".parse().unwrap(),
            pane: "%2".into(),
            session_id: Some("review-session".into()),
        },
        now(),
    )
    .await
    .unwrap();
    let events = org.execute(OrgCommand::Task(TaskCommand::Create(serde_json::from_value(json!({"title":"review","kind":"review","creator":"owner","judges":work,"measure":{"kind":"owner"},"files":[]})).unwrap())), now()).await.unwrap();
    let review: TaskId = format!("t{}", events.first().unwrap().seq).parse().unwrap();
    for command in [
        TaskCommand::Assign {
            task_id: review.clone(),
            agent_id: "p/review".parse().unwrap(),
            by: actor("owner"),
        },
        TaskCommand::StartReview {
            task_id: work.clone(),
            review_task: review.clone(),
            by: actor("owner"),
        },
        TaskCommand::Submit {
            task_id: review.clone(),
            what: TaskSubmission::Result,
            files: vec![],
            by: actor("p/review"),
        },
        TaskCommand::Verdict {
            task_id: work.clone(),
            verdict: TaskVerdict::Accepted,
            review_task: Some(review.clone()),
            note: None,
            by: actor("owner"),
        },
    ] {
        org.execute(OrgCommand::Task(command), now()).await.unwrap();
    }
    let state = org.snapshot().await.unwrap();
    assert_eq!(
        state
            .task(&work)
            .unwrap()
            .cost
            .as_ref()
            .unwrap()
            .review_rounds,
        Some(1)
    );
    assert_eq!(
        state
            .task(&review)
            .unwrap()
            .cost
            .as_ref()
            .unwrap()
            .review_rounds,
        Some(0)
    );
    cancel.cancel();
}

// Synthetic: a later writer used a task state this build cannot write back.
// Unsupported historical accounting must not disable unrelated org operations.
#[tokio::test]
async fn unsupported_historical_cost_does_not_poison_startup() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("log/events");
    std::fs::create_dir_all(&log).unwrap();
    let mut rows: Vec<Value> = include_str!("../../atm-org/tests/fixtures/task_cost_log.jsonl")
        .lines()
        .take(8)
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    rows.insert(6, json!({"seq":7,"ts":"2026-01-01T00:00:03Z","kind":"task.state_changed","body":{"task_id":"t3","state":"future_state","reason":null,"by":"owner"}}));
    for (i, row) in rows.iter_mut().enumerate() {
        row["seq"] = json!(i + 1);
    }
    std::fs::write(
        log.join("00000001.jsonl"),
        rows.iter().map(|r| format!("{r}\n")).collect::<String>(),
    )
    .unwrap();
    let cancel = CancellationToken::new();
    let org = OrgHandle::open(dir.path().into(), cancel.clone()).await;
    let state = org.snapshot().await.unwrap();
    assert!(state.task(&"t3".parse().unwrap()).unwrap().cost.is_none());
    assert_eq!(
        state
            .task_cost(&"t3".parse().unwrap(), now())
            .unwrap()
            .seconds_in_state
            .get(&TaskState::Unknown),
        Some(&1)
    );
    org.execute(
        OrgCommand::RegisterProject {
            project_id: "other".parse().unwrap(),
            root: "/other".into(),
            move_root: false,
        },
        now(),
    )
    .await
    .unwrap();
    cancel.cancel();
}

async fn feed(org: OrgHandle, frames: Vec<Value>) {
    use atm_protocol::ClientMessage;
    let registry = spawn_registry();
    let cancel = CancellationToken::new();
    let (ours, peer) = tokio::net::UnixStream::pair().unwrap();
    let (read, write) = ours.into_split();
    let handler = ConnectionHandler::new(
        read,
        write,
        registry.clone(),
        Arc::new(RwLock::new(HashMap::new())),
        1,
        cancel.clone(),
        Arc::new(router(MethodServices::new(registry)).unwrap()),
    )
    .with_org(org);
    let task = tokio::spawn(handler.run());
    let (read, mut write) = peer.into_split();
    let mut read = BufReader::new(read);
    let run = async {
        write
            .write_all(
                format!(
                    "{}\n",
                    serde_json::to_string(&ClientMessage::connect(None)).unwrap()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut line = String::new();
        read.read_line(&mut line).await.unwrap();
        for mut frame in frames {
            frame["data"]["pid"] = json!(std::process::id());
            frame["data"]["session_id"] = json!("s");
            if frame["data"]["tmux_pane"].is_null() {
                frame["data"]["tmux_pane"] = json!("%1");
            }
            write
                .write_all(format!("{frame}\n").as_bytes())
                .await
                .unwrap();
        }
        write
            .write_all(
                format!(
                    "{}\n",
                    serde_json::to_string(&ClientMessage::ping(42)).unwrap()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        line.clear();
        read.read_line(&mut line).await.unwrap();
        assert_eq!(serde_json::from_str::<Value>(&line).unwrap()["seq"], 42);
    };
    tokio::time::timeout(StdDuration::from_secs(60), run)
        .await
        .unwrap();
    cancel.cancel();
    task.await.unwrap();
}

// Captured: pi 0.73.1, 2026-10-24, TUI/local fake provider + pi-atm socket
// tap (adapter fixture docs). Routing IDs/pid rewritten for this private test.
// Break caught: counting three provider replies as three user turns, summing
// snapshots, or requiring message evidence to be enabled for task accounting.
#[tokio::test]
async fn captured_pi_reports_two_prompts_three_replies_and_exact_total() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let org = OrgHandle::open(dir.path().into(), cancel.clone()).await;
    let id = seed(&org).await;
    let frames = include_str!("../../atm-pi-adapter/tests/fixtures/pi_session_cost_0.73.1.jsonl")
        .lines()
        .map(|s| serde_json::from_str::<Value>(s).unwrap())
        .filter(|f| f["data"]["event"] != "session_shutdown")
        .map(|mut f| {
            f["data"]["tmux_pane"] = json!("%1");
            f
        })
        .collect();
    feed(org.clone(), frames).await;
    let record = org.snapshot().await.unwrap().task_cost(&id, now()).unwrap();
    assert_eq!(record.turns, Some(2));
    assert_eq!(record.cost, Some(Money::from_microdollars(10_650)));
    assert_eq!(record.peak_context_percent, Some(4));
    cancel.cancel();
}

// Captured: Claude Code 2.1.310, 2026-10-24, TUI/local fake Messages API
// hook/status stdin captures. Routing IDs/pid rewritten for this private test.
#[tokio::test]
async fn captured_claude_status_cost_and_prompt_are_observed_without_child_credit() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let org = OrgHandle::open(dir.path().into(), cancel.clone()).await;
    let id = seed(&org).await;
    let prompt: Value =
        include_str!("../../atm-claude-adapter/tests/fixtures/claude_tui_session_2.1.310.jsonl")
            .lines()
            .map(|s| serde_json::from_str::<Value>(s).unwrap())
            .find(|v| v["hook_event_name"] == "UserPromptSubmit")
            .unwrap();
    let mut child = prompt.clone();
    child["agent_id"] = json!("child");
    child["agent_type"] = json!("general-purpose");
    let mut wrong_pane = prompt.clone();
    wrong_pane["tmux_pane"] = json!("%2");
    let mut frames = vec![
        json!({"protocol_version":{"major":1,"minor":0},"type":"hook_event","data":prompt}),
        json!({"protocol_version":{"major":1,"minor":0},"type":"hook_event","data":child}),
        json!({"protocol_version":{"major":1,"minor":0},"type":"hook_event","data":wrong_pane}),
    ];
    frames.extend(include_str!("../../atm-claude-adapter/tests/fixtures/claude_status_line_2.1.310.jsonl").lines().map(|s| json!({"protocol_version":{"major":1,"minor":0},"type":"status_update","data":serde_json::from_str::<Value>(s).unwrap()})));
    feed(org.clone(), frames).await;
    let record = org.snapshot().await.unwrap().task_cost(&id, now()).unwrap();
    assert_eq!(record.turns, Some(1));
    assert_eq!(record.cost, Some(Money::from_microdollars(93_184)));
    assert!(record.peak_context_percent.is_some_and(|v| v > 0));
    cancel.cancel();
}

// Captured: Codex 0.163.0, 2026-10-24, TUI/local fake provider + hook and
// rollout capture (adapter docs). The transcript path/routing IDs are private.
#[tokio::test]
async fn captured_codex_uses_active_tokens_with_reported_window_not_usd() {
    let dir = tempfile::tempdir().unwrap();
    let transcript = dir.path().join("rollout.jsonl");
    std::fs::write(
        &transcript,
        include_str!("../../atm-codex-adapter/tests/fixtures/codex_rollout_tokens_0.163.0.jsonl"),
    )
    .unwrap();
    let cancel = CancellationToken::new();
    let org = OrgHandle::open(dir.path().into(), cancel.clone()).await;
    let id = seed(&org).await;
    let frames =
        include_str!("../../atm-codex-adapter/tests/fixtures/codex_tui_session_0.163.0.jsonl")
            .lines()
            .map(|s| serde_json::from_str::<Value>(s).unwrap())
            .filter(|f| f["hook_event_name"] != "SessionEnd")
            .map(|mut f| {
                f["transcript_path"] = json!(transcript);
                json!({"protocol_version":{"major":1,"minor":0},"type":"codex_event","data":f})
            })
            .collect();
    feed(org.clone(), frames).await;
    let record = org.snapshot().await.unwrap().task_cost(&id, now()).unwrap();
    assert_eq!(record.turns, Some(1));
    assert_eq!(record.cost, None);
    assert_eq!(record.peak_context_percent, Some(3));
    cancel.cancel();
}
