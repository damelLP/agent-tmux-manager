//! Tests for the checked per-pane read and the start source it carries.
//!
//! Payloads are hand-written lifecycle events; the source strings are the ones
//! the adapters map from their harness's SessionStart.

use super::*;
use crate::registry::{PaneSession, RegistryError, RegistryHandle};
use crate::server::parse_status_line;
use atm_core::{LifecycleContext, LifecycleEvent};
use tokio::sync::oneshot;

fn actor() -> RegistryActor {
    let (_, commands) = mpsc::channel(16);
    let (events, _) = broadcast::channel(64);
    RegistryActor::new(commands, events)
}

fn apply(actor: &mut RegistryActor, id: &str, pid: u32, pane: &str, event: LifecycleEvent) {
    let (respond_to, mut rx) = oneshot::channel();
    actor.handle_command(RegistryCommand::ApplyLifecycleEvent {
        session_id: SessionId::new(id),
        event,
        harness: atm_core::Harness::ClaudeCode,
        pid: Some(pid),
        tmux_pane: Some(pane.into()),
        context: LifecycleContext::default(),
        respond_to,
    });
    rx.try_recv().unwrap().unwrap();
}

fn start(source: Option<&str>) -> LifecycleEvent {
    LifecycleEvent::SessionStart {
        source: source.map(Into::into),
    }
}

fn read(actor: &mut RegistryActor, pane: &str) -> Vec<PaneSession> {
    let (respond_to, mut rx) = oneshot::channel();
    actor.handle_command(RegistryCommand::PaneSessions {
        pane: pane.into(),
        respond_to,
    });
    let mut rows = rx.try_recv().unwrap();
    rows.sort_by(|a, b| a.view.id.as_str().cmp(b.view.id.as_str()));
    rows
}

fn sources(rows: &[PaneSession]) -> Vec<(&str, Option<Option<&str>>)> {
    rows.iter()
        .map(|row| {
            (
                row.view.id.as_str(),
                row.start.as_ref().map(|start| start.source.as_deref()),
            )
        })
        .collect()
}

#[tokio::test]
async fn source_recorded_before_updated_event() {
    // A subscriber that reads on the row's event must already find the source.
    let (commands, receive) = mpsc::channel(16);
    let (events, mut subscription) = broadcast::channel(64);
    let registry = RegistryHandle::new(commands, events.clone());
    let actor = tokio::spawn(RegistryActor::new(receive, events).run());
    let pid = std::process::id();
    registry
        .apply_lifecycle_event(
            SessionId::new("after-clear"),
            start(Some("clear")),
            atm_core::Harness::ClaudeCode,
            Some(pid),
            Some("%7".into()),
        )
        .await
        .unwrap();
    loop {
        match subscription.recv().await.unwrap() {
            SessionEvent::Updated { session } if session.id.as_str() == "after-clear" => break,
            _ => {}
        }
    }
    let rows = registry.pane_sessions("%7").await.unwrap();
    assert_eq!(sources(&rows), [("after-clear", Some(Some("clear")))]);
    assert_eq!(rows[0].pid, Some(pid));
    assert_eq!(rows[0].view.tmux_pane.as_deref(), Some("%7"));
    drop(registry);
    actor.await.unwrap();
}

#[test]
fn row_created_by_status_has_no_source() {
    let mut actor = actor();
    let pid = std::process::id();
    let dir = tempfile::tempdir().unwrap();
    // Discovery finds the process, then its status line names the session:
    // a real row on the pane, and no SessionStart was ever accepted for it.
    let (respond_to, mut rx) = oneshot::channel();
    actor.handle_command(RegistryCommand::RegisterDiscovered {
        session_id: SessionId::pending_from_pid(pid),
        pid,
        cwd: dir.path().to_path_buf(),
        tmux_pane: Some("%7".into()),
        harness: atm_core::Harness::ClaudeCode,
        respond_to,
    });
    rx.try_recv().unwrap().unwrap();
    let pending = SessionId::pending_from_pid(pid);
    assert_eq!(sources(&read(&mut actor, "%7")), [(pending.as_str(), None)]);
    let (status_pid, data) = parse_status_line(serde_json::json!({
        "session_id": "status-only",
        "pid": pid,
        "model": {"id": "claude-sonnet-4-20250514"},
        "cost": {"total_cost_usd": 0.10, "total_duration_ms": 5000},
        "context_window": {"total_input_tokens": 1000, "context_window_size": 200000}
    }))
    .unwrap();
    let (respond_to, mut rx) = oneshot::channel();
    actor.handle_command(RegistryCommand::UpdateFromStatusLine {
        session_id: SessionId::new("status-only"),
        pid: status_pid,
        data,
        harness: atm_core::Harness::ClaudeCode,
        respond_to,
    });
    rx.try_recv().unwrap().unwrap();
    assert_eq!(sources(&read(&mut actor, "%7")), [("status-only", None)]);

    // A row created by an event that is not a start has none either.
    let mut actor = self::actor();
    apply(
        &mut actor,
        "working",
        pid,
        "%7",
        LifecycleEvent::WorkingStart,
    );
    assert_eq!(sources(&read(&mut actor, "%7")), [("working", None)]);
    // The start then arrives: a source of None is still an accepted start.
    apply(&mut actor, "working", pid, "%7", start(None));
    assert_eq!(sources(&read(&mut actor, "%7")), [("working", Some(None))]);
}

#[test]
fn later_start_replaces_source() {
    let mut actor = actor();
    let pid = std::process::id();
    apply(&mut actor, "s1", pid, "%7", start(Some("startup")));
    assert_eq!(
        sources(&read(&mut actor, "%7")),
        [("s1", Some(Some("startup")))]
    );
    // An in-process resume of the same session.
    apply(&mut actor, "s1", pid, "%7", start(Some("resume")));
    assert_eq!(
        sources(&read(&mut actor, "%7")),
        [("s1", Some(Some("resume")))]
    );
    // Other events leave it alone.
    apply(&mut actor, "s1", pid, "%7", LifecycleEvent::WorkingStart);
    assert_eq!(
        sources(&read(&mut actor, "%7")),
        [("s1", Some(Some("resume")))]
    );
}

#[test]
fn source_removed_with_row() {
    let mut actor = actor();
    let pid = std::process::id();
    apply(&mut actor, "old", pid, "%7", start(Some("clear")));
    // A new session takes over the process: the old row and its source go.
    apply(&mut actor, "new", pid, "%7", start(Some("startup")));
    assert_eq!(
        sources(&read(&mut actor, "%7")),
        [("new", Some(Some("startup")))]
    );
    apply(
        &mut actor,
        "new",
        pid,
        "%7",
        LifecycleEvent::SessionEnd { reason: None },
    );
    assert_eq!(read(&mut actor, "%7").len(), 0);
    // A later row of the same process starts without a source.
    apply(&mut actor, "next", pid, "%7", LifecycleEvent::WorkingStart);
    assert_eq!(sources(&read(&mut actor, "%7")), [("next", None)]);
}

#[test]
fn pane_read_filters_by_pane() {
    let mut actor = actor();
    let pid = std::process::id();
    apply(&mut actor, "here", pid, "%7", start(Some("startup")));
    // A row without a process (registered directly) on another pane, and one
    // without any pane.
    for (id, pane) in [("there", Some("%8")), ("nowhere", None)] {
        let mut session = atm_core::SessionDomain::new(
            SessionId::new(id),
            atm_core::AgentType::GeneralPurpose,
            atm_core::Model::Sonnet4,
        );
        session.tmux_pane = pane.map(Into::into);
        let (respond_to, _) = oneshot::channel();
        actor.handle_command(RegistryCommand::Register {
            session: Box::new(session),
            respond_to,
        });
    }
    assert_eq!(
        sources(&read(&mut actor, "%7")),
        [("here", Some(Some("startup")))]
    );
    let there = read(&mut actor, "%8");
    assert_eq!(sources(&there), [("there", None)]);
    assert_eq!(there[0].pid, None);
    assert_eq!(read(&mut actor, "%9").len(), 0);
    assert_eq!(read(&mut actor, "").len(), 0);
}

#[tokio::test]
async fn registry_reply_dropped_is_an_error() {
    // The actor takes the command and goes away without answering: that is a
    // failure, never "no sessions on this pane".
    let (commands, mut receive) = mpsc::channel(16);
    let (events, _) = broadcast::channel(16);
    let registry = RegistryHandle::new(commands, events);
    let dropper = tokio::spawn(async move {
        match receive.recv().await {
            Some(RegistryCommand::PaneSessions { pane, respond_to }) => {
                drop(respond_to);
                pane
            }
            other => panic!("unexpected command: {other:?}"),
        }
    });
    assert!(matches!(
        registry.pane_sessions("%7").await,
        Err(RegistryError::ChannelClosed)
    ));
    assert_eq!(dropper.await.unwrap(), "%7");
    // The receiver is gone now: the send itself fails.
    assert!(matches!(
        registry.pane_sessions("%7").await,
        Err(RegistryError::ChannelClosed)
    ));
    // The legacy read answers the same failure with an empty list.
    assert!(registry.get_all_sessions().await.is_empty());
}
