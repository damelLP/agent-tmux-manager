//! Tests for the registry actor.

use std::path::PathBuf;

use super::*;
use crate::registry::commands::RegistryError;
use crate::server::parse_status_line;
use atm_core::ChildAgent;
use atm_core::{AgentType, Model, NeedsInputReason, Tool};
use atm_core::{BackgroundActivity, ChildAlias, LifecycleContext, LifecycleEvent};
use tokio::sync::oneshot;

fn create_test_session(id: &str) -> SessionDomain {
    SessionDomain::new(
        SessionId::new(id),
        AgentType::GeneralPurpose,
        Model::Sonnet4,
    )
}

fn create_actor() -> (
    mpsc::Sender<RegistryCommand>,
    RegistryActor,
    broadcast::Receiver<SessionEvent>,
) {
    let (cmd_tx, cmd_rx) = mpsc::channel(16);
    let (event_tx, event_rx) = broadcast::channel(16);
    let actor = RegistryActor::new(cmd_rx, event_tx);
    (cmd_tx, actor, event_rx)
}

#[tokio::test]
async fn test_register_session() {
    let (cmd_tx, mut actor, mut event_rx) = create_actor();

    let session = create_test_session("test-123");
    let (respond_tx, respond_rx) = oneshot::channel();

    cmd_tx
        .send(RegistryCommand::Register {
            session: Box::new(session),
            respond_to: respond_tx,
        })
        .await
        .unwrap();

    // Process the command manually (actor not running in background)
    if let Some(cmd) = actor.receiver.recv().await {
        actor.handle_command(cmd);
    }

    // Check response
    let result = respond_rx.await.unwrap();
    assert!(result.is_ok());
    assert_eq!(actor.session_count(), 1);

    // Check event was published
    let event = event_rx.try_recv().unwrap();
    assert!(matches!(event, SessionEvent::Registered { .. }));
}

#[tokio::test]
async fn test_register_duplicate_fails() {
    let (_, mut actor, _) = create_actor();

    let session1 = create_test_session("test-123");
    let session2 = create_test_session("test-123");

    // Register first session
    let (tx1, _) = oneshot::channel();
    let cmd1 = RegistryCommand::Register {
        session: Box::new(session1),
        respond_to: tx1,
    };
    actor.handle_command(cmd1);

    // Try to register duplicate
    let (tx2, rx2) = oneshot::channel();
    let cmd2 = RegistryCommand::Register {
        session: Box::new(session2),
        respond_to: tx2,
    };
    actor.handle_command(cmd2);

    let result = rx2.await.unwrap();
    assert!(matches!(
        result,
        Err(RegistryError::SessionAlreadyExists(_))
    ));
    assert_eq!(actor.session_count(), 1);
}

#[tokio::test]
async fn test_get_session() {
    let (_, mut actor, _) = create_actor();

    // Register a session
    let session = create_test_session("test-123");
    let (tx, _) = oneshot::channel();
    actor.handle_command(RegistryCommand::Register {
        session: Box::new(session),
        respond_to: tx,
    });

    // Get the session
    let (tx, rx) = oneshot::channel();
    actor.handle_command(RegistryCommand::GetSession {
        session_id: SessionId::new("test-123"),
        respond_to: tx,
    });

    let result = rx.await.unwrap();
    assert!(result.is_some());
    assert_eq!(result.unwrap().id.as_str(), "test-123");
}

#[tokio::test]
async fn test_get_nonexistent_session() {
    let (_, mut actor, _) = create_actor();

    let (tx, rx) = oneshot::channel();
    actor.handle_command(RegistryCommand::GetSession {
        session_id: SessionId::new("nonexistent"),
        respond_to: tx,
    });

    let result = rx.await.unwrap();
    assert!(result.is_none());
}

#[tokio::test]
async fn test_get_all_sessions() {
    let (_, mut actor, _) = create_actor();

    // Register multiple sessions
    for i in 0..3 {
        let session = create_test_session(&format!("test-{i}"));
        let (tx, _) = oneshot::channel();
        actor.handle_command(RegistryCommand::Register {
            session: Box::new(session),
            respond_to: tx,
        });
    }

    // Get all sessions
    let (tx, rx) = oneshot::channel();
    actor.handle_command(RegistryCommand::GetAllSessions { respond_to: tx });

    let result = rx.await.unwrap();
    assert_eq!(result.len(), 3);
}

#[tokio::test]
async fn test_remove_session() {
    let (_, mut actor, mut event_rx) = create_actor();

    // Register a session
    let session = create_test_session("test-123");
    let (tx, _) = oneshot::channel();
    actor.handle_command(RegistryCommand::Register {
        session: Box::new(session),
        respond_to: tx,
    });

    // Drain the registered event
    let _ = event_rx.try_recv();

    // Remove the session
    let (tx, rx) = oneshot::channel();
    actor.handle_command(RegistryCommand::Remove {
        session_id: SessionId::new("test-123"),
        respond_to: tx,
    });

    let result = rx.await.unwrap();
    assert!(result.is_ok());
    assert_eq!(actor.session_count(), 0);

    // Check removed event
    let event = event_rx.try_recv().unwrap();
    assert!(matches!(
        event,
        SessionEvent::Removed {
            reason: RemovalReason::Explicit,
            ..
        }
    ));
}

#[tokio::test]
async fn test_remove_nonexistent_fails() {
    let (_, mut actor, _) = create_actor();

    let (tx, rx) = oneshot::channel();
    actor.handle_command(RegistryCommand::Remove {
        session_id: SessionId::new("nonexistent"),
        respond_to: tx,
    });

    let result = rx.await.unwrap();
    assert!(matches!(result, Err(RegistryError::SessionNotFound(_))));
}

#[tokio::test]
async fn test_apply_hook_event() {
    let (_, mut actor, _) = create_actor();

    // Register a session
    let session = create_test_session("test-123");
    let (tx, _) = oneshot::channel();
    actor.handle_command(RegistryCommand::Register {
        session: Box::new(session),
        respond_to: tx,
    });

    // Apply lifecycle event
    let (tx, rx) = oneshot::channel();
    actor.handle_command(RegistryCommand::ApplyLifecycleEvent {
        session_id: SessionId::new("test-123"),
        event: LifecycleEvent::ToolCallStart {
            name: Tool::Bash,
            tool_use_id: None,
            input: None,
        },
        harness: atm_core::Harness::Unknown,
        pid: None,
        tmux_pane: None,
        context: LifecycleContext::default(),
        respond_to: tx,
    });

    let result = rx.await.unwrap();
    assert!(result.is_ok());

    // Verify session status changed
    let (tx, rx) = oneshot::channel();
    actor.handle_command(RegistryCommand::GetSession {
        session_id: SessionId::new("test-123"),
        respond_to: tx,
    });

    let view = rx.await.unwrap().unwrap();
    assert_eq!(view.status_label, "working");
    assert_eq!(view.activity_detail, Some("Bash".to_string()));
}

#[tokio::test]
async fn test_apply_hook_event_session_end() {
    let (_, mut actor, mut event_rx) = create_actor();

    // Register a session
    let session = create_test_session("test-session-end");
    let (tx, _) = oneshot::channel();
    actor.handle_command(RegistryCommand::Register {
        session: Box::new(session),
        respond_to: tx,
    });

    // Drain registered event
    let _ = event_rx.try_recv();

    assert_eq!(actor.session_count(), 1);

    // Apply SessionEnd hook - should remove the session
    let (tx, rx) = oneshot::channel();
    actor.handle_command(RegistryCommand::ApplyLifecycleEvent {
        session_id: SessionId::new("test-session-end"),
        event: LifecycleEvent::SessionEnd { reason: None },
        harness: atm_core::Harness::Unknown,
        pid: None,
        tmux_pane: None,
        context: LifecycleContext::default(),
        respond_to: tx,
    });

    let result = rx.await.unwrap();
    assert!(result.is_ok());

    // Session should be removed
    assert_eq!(actor.session_count(), 0);

    // Should have received Removed event with SessionEnded reason
    let event = event_rx.try_recv().unwrap();
    assert!(matches!(
        event,
        SessionEvent::Removed {
            reason: RemovalReason::SessionEnded,
            ..
        }
    ));
}

#[tokio::test]
async fn test_apply_hook_event_session_end_nonexistent() {
    let (_, mut actor, _) = create_actor();

    // Apply SessionEnd to non-existent session (race condition scenario)
    let (tx, rx) = oneshot::channel();
    actor.handle_command(RegistryCommand::ApplyLifecycleEvent {
        session_id: SessionId::new("nonexistent"),
        event: LifecycleEvent::SessionEnd { reason: None },
        harness: atm_core::Harness::Unknown,
        pid: None,
        tmux_pane: None,
        context: LifecycleContext::default(),
        respond_to: tx,
    });

    // Should succeed silently (not error)
    let result = rx.await.unwrap();
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_max_sessions_limit() {
    let (_, mut actor, _) = create_actor();

    // Register MAX_SESSIONS sessions
    for i in 0..MAX_SESSIONS {
        let session = create_test_session(&format!("test-{i}"));
        let (tx, _) = oneshot::channel();
        actor.handle_command(RegistryCommand::Register {
            session: Box::new(session),
            respond_to: tx,
        });
    }

    assert_eq!(actor.session_count(), MAX_SESSIONS);

    // Try to register one more
    let session = create_test_session("one-too-many");
    let (tx, rx) = oneshot::channel();
    actor.handle_command(RegistryCommand::Register {
        session: Box::new(session),
        respond_to: tx,
    });

    let result = rx.await.unwrap();
    assert!(matches!(
        result,
        Err(RegistryError::RegistryFull { max: MAX_SESSIONS })
    ));
    assert_eq!(actor.session_count(), MAX_SESSIONS);
}

#[test]
fn test_lifecycle_event_for_new_pid_respects_max_sessions() {
    let (_, mut actor, _) = create_actor();
    fill_registry(&mut actor, MAX_SESSIONS);

    let result = apply_event(
        &mut actor,
        "one-too-many",
        Some(UNUSED_PID),
        LifecycleEvent::SessionStart { source: None },
    );

    assert!(matches!(
        result,
        Err(RegistryError::RegistryFull { max: MAX_SESSIONS })
    ));
    assert_eq!(actor.session_count(), MAX_SESSIONS);
    assert!(session_view(&actor, "one-too-many").is_none());
}

#[tokio::test]
async fn test_register_duplicate_when_full_reports_already_exists() {
    let (_, mut actor, _) = create_actor();
    fill_registry(&mut actor, MAX_SESSIONS);

    // A duplicate is rejected as a duplicate before capacity is checked.
    let (tx, rx) = oneshot::channel();
    actor.handle_command(RegistryCommand::Register {
        session: Box::new(create_test_session("test-0")),
        respond_to: tx,
    });

    let result = rx.await.unwrap();
    assert!(matches!(
        result,
        Err(RegistryError::SessionAlreadyExists(id)) if id == SessionId::new("test-0")
    ));
    assert_eq!(actor.session_count(), MAX_SESSIONS);
}

/// Above Linux's PID_MAX_LIMIT (2^22), so never a live process.
const UNUSED_PID: u32 = 4_194_305;

fn fill_registry(actor: &mut RegistryActor, count: usize) {
    for i in 0..count {
        register_test_session(actor, &format!("test-{i}"));
    }
}

fn apply_event(
    actor: &mut RegistryActor,
    id: &str,
    pid: Option<u32>,
    event: LifecycleEvent,
) -> Result<(), RegistryError> {
    let (respond_to, mut rx) = oneshot::channel();
    actor.handle_command(RegistryCommand::ApplyLifecycleEvent {
        session_id: SessionId::new(id),
        event,
        harness: atm_core::Harness::ClaudeCode,
        pid,
        tmux_pane: None,
        context: LifecycleContext::default(),
        respond_to,
    });
    rx.try_recv().unwrap()
}

#[test]
fn test_lifecycle_events_for_held_sessions_apply_when_full() {
    let (_, mut actor, _) = create_actor();
    apply_event(
        &mut actor,
        "by-pid",
        Some(UNUSED_PID),
        LifecycleEvent::SessionStart { source: None },
    )
    .unwrap();
    fill_registry(&mut actor, MAX_SESSIONS - 1);
    assert_eq!(actor.session_count(), MAX_SESSIONS);

    // Found by pid, then by session id: neither creates, so both apply.
    for (id, pid) in [("by-pid", Some(UNUSED_PID)), ("test-0", None)] {
        apply_event(&mut actor, id, pid, LifecycleEvent::WorkingStart).unwrap();
        let status = session_view(&actor, id).unwrap().status;
        assert_eq!(status, atm_core::SessionStatus::Working, "{id}");
    }
    assert_eq!(actor.session_count(), MAX_SESSIONS);
}

#[tokio::test]
async fn test_register_discovered_respects_max_sessions() {
    let (_, mut actor, _) = create_actor();
    fill_registry(&mut actor, MAX_SESSIONS);

    let (tx, rx) = oneshot::channel();
    actor.handle_command(RegistryCommand::RegisterDiscovered {
        session_id: SessionId::pending_from_pid(UNUSED_PID),
        pid: UNUSED_PID,
        cwd: PathBuf::from("/tmp"),
        tmux_pane: None,
        harness: atm_core::Harness::ClaudeCode,
        respond_to: tx,
    });

    assert!(matches!(
        rx.await.unwrap(),
        Err(RegistryError::RegistryFull { max: MAX_SESSIONS })
    ));
    assert_eq!(actor.session_count(), MAX_SESSIONS);
}

#[tokio::test]
async fn test_status_line_create_respects_max_sessions() {
    let (_, mut actor, _) = create_actor();
    fill_registry(&mut actor, MAX_SESSIONS);

    let (tx, rx) = oneshot::channel();
    let (pid, data) = parse_status_line(serde_json::json!({
        "session_id": "one-too-many",
        "pid": UNUSED_PID,
        "model": {"id": "claude-sonnet-4-20250514"},
        "cost": {"total_cost_usd": 0.10, "total_duration_ms": 5000},
        "context_window": {"total_input_tokens": 1000, "context_window_size": 200000}
    }))
    .unwrap();
    actor.handle_command(RegistryCommand::UpdateFromStatusLine {
        session_id: SessionId::new("one-too-many"),
        pid,
        data,
        harness: atm_core::Harness::ClaudeCode,
        respond_to: tx,
    });

    assert!(matches!(
        rx.await.unwrap(),
        Err(RegistryError::RegistryFull { max: MAX_SESSIONS })
    ));
    assert_eq!(actor.session_count(), MAX_SESSIONS);
    assert!(session_view(&actor, "one-too-many").is_none());
}

#[tokio::test]
async fn test_update_from_status_line_existing_session() {
    let (_, mut actor, _) = create_actor();

    // Register a session
    let session = create_test_session("test-123");
    let (tx, _) = oneshot::channel();
    actor.handle_command(RegistryCommand::Register {
        session: Box::new(session),
        respond_to: tx,
    });

    // Update via status line
    let status_json = serde_json::json!({
        "session_id": "test-123",
        "model": {"id": "claude-sonnet-4-20250514"},
        "cost": {"total_cost_usd": 0.25, "total_duration_ms": 15000},
        "context_window": {"total_input_tokens": 5000, "context_window_size": 200000}
    });

    let (tx, rx) = oneshot::channel();
    let (pid, data) = parse_status_line(status_json).unwrap();
    actor.handle_command(RegistryCommand::UpdateFromStatusLine {
        session_id: SessionId::new("test-123"),
        pid,
        data,
        harness: atm_core::Harness::ClaudeCode,
        respond_to: tx,
    });

    let result = rx.await.unwrap();
    assert!(result.is_ok());

    // Verify update was applied
    let (tx, rx) = oneshot::channel();
    actor.handle_command(RegistryCommand::GetSession {
        session_id: SessionId::new("test-123"),
        respond_to: tx,
    });

    let view = rx.await.unwrap().unwrap();
    assert!(view.cost_display.contains("0.25") || view.cost_usd > 0.24);
}

#[tokio::test]
async fn test_update_from_status_line_auto_register() {
    let (_, mut actor, mut event_rx) = create_actor();

    // Use the current process PID (a real PID that set_pid can validate)
    let current_pid = std::process::id();

    // Update for non-existent session (should auto-register)
    // Note: PID is required for auto-registration with PID-as-primary-key design
    let status_json = serde_json::json!({
        "session_id": "new-session",
        "pid": current_pid,
        "model": {"id": "claude-sonnet-4-20250514"},
        "cost": {"total_cost_usd": 0.10, "total_duration_ms": 5000},
        "context_window": {"total_input_tokens": 1000, "context_window_size": 200000}
    });

    let (tx, rx) = oneshot::channel();
    let (pid, data) = parse_status_line(status_json).unwrap();
    actor.handle_command(RegistryCommand::UpdateFromStatusLine {
        session_id: SessionId::new("new-session"),
        pid,
        data,
        harness: atm_core::Harness::ClaudeCode,
        respond_to: tx,
    });

    let result = rx.await.unwrap();
    assert!(result.is_ok());
    assert_eq!(actor.session_count(), 1);

    // Check registered event was published
    let event = event_rx.try_recv().unwrap();
    assert!(matches!(event, SessionEvent::Registered { .. }));
}

#[tokio::test]
async fn test_cleanup_stale_no_stale_sessions() {
    let (_, mut actor, _) = create_actor();

    // Register a session (it will be fresh)
    let session = create_test_session("test-123");
    let (tx, _) = oneshot::channel();
    actor.handle_command(RegistryCommand::Register {
        session: Box::new(session),
        respond_to: tx,
    });

    // Run cleanup
    actor.handle_command(RegistryCommand::CleanupStale);

    // Session should still exist (not stale)
    assert_eq!(actor.session_count(), 1);
}

#[tokio::test]
async fn test_pending_session_upgrade_on_status_line() {
    let (_, mut actor, mut event_rx) = create_actor();

    // Use the current process PID (a real PID that set_pid can validate)
    let current_pid = std::process::id();

    // Register a pending session (simulating discovery without transcript)
    let pending_id = SessionId::pending_from_pid(current_pid);
    let (tx, rx) = oneshot::channel();
    let cmd = RegistryCommand::RegisterDiscovered {
        session_id: pending_id.clone(),
        pid: current_pid,
        cwd: std::path::PathBuf::from("/home/user/project"),
        tmux_pane: None,
        harness: atm_core::Harness::Unknown,
        respond_to: tx,
    };
    actor.handle_command(cmd);
    let result = rx.await.unwrap();
    assert!(result.is_ok());
    assert_eq!(actor.session_count(), 1);

    // Drain the registered event
    let _ = event_rx.try_recv();
    let _ = event_rx.try_recv(); // Updated event

    // Now receive a status line with the real session ID and same PID
    let status_json = serde_json::json!({
        "session_id": "real-session-uuid",
        "pid": current_pid,
        "model": {"id": "claude-sonnet-4-20250514"},
        "cost": {"total_cost_usd": 0.10, "total_duration_ms": 5000},
        "context_window": {"total_input_tokens": 1000, "context_window_size": 200000}
    });

    let (tx, rx) = oneshot::channel();
    let (pid, data) = parse_status_line(status_json).unwrap();
    let cmd = RegistryCommand::UpdateFromStatusLine {
        session_id: SessionId::new("real-session-uuid"),
        pid,
        data,
        harness: atm_core::Harness::ClaudeCode,
        respond_to: tx,
    };
    actor.handle_command(cmd);
    let result = rx.await.unwrap();
    assert!(result.is_ok());

    // Should still have 1 session (pending was upgraded, not a new one added)
    assert_eq!(actor.session_count(), 1);

    // The session should now have the real ID, not the pending ID
    let (tx, rx) = oneshot::channel();
    let cmd = RegistryCommand::GetSession {
        session_id: SessionId::new("real-session-uuid"),
        respond_to: tx,
    };
    actor.handle_command(cmd);
    let session = rx.await.unwrap();
    assert!(session.is_some());
    assert_eq!(session.unwrap().id.as_str(), "real-session-uuid");

    // The pending session should no longer exist
    let (tx, rx) = oneshot::channel();
    let cmd = RegistryCommand::GetSession {
        session_id: pending_id,
        respond_to: tx,
    };
    actor.handle_command(cmd);
    let pending_session = rx.await.unwrap();
    assert!(pending_session.is_none());

    // Should have received Removed event for pending and Registered for real
    let mut found_removed = false;
    let mut found_registered = false;
    while let Ok(event) = event_rx.try_recv() {
        match event {
            SessionEvent::Removed {
                reason: RemovalReason::Upgraded,
                ..
            } => {
                found_removed = true;
            }
            SessionEvent::Registered { session_id, .. }
                if session_id.as_str() == "real-session-uuid" =>
            {
                found_registered = true;
            }
            _ => {}
        }
    }
    assert!(
        found_removed,
        "Should have received Removed event with Upgraded reason"
    );
    assert!(
        found_registered,
        "Should have received Registered event for real session"
    );
}

#[tokio::test]
async fn test_pending_session_upgrade_on_lifecycle_event() {
    // Mirrors test_pending_session_upgrade_on_status_line but
    // exercises the vendor-adapter lifecycle path. Without the
    // reconcile call in handle_apply_lifecycle_event, a pi
    // session discovered via /proc would stay as `pending-{pid}`
    // forever, even after the first event with a real session_id.
    let (_, mut actor, mut event_rx) = create_actor();

    let current_pid = std::process::id();
    let pending_id = SessionId::pending_from_pid(current_pid);

    // Step 1: discovery registers a pending session.
    let (tx, rx) = oneshot::channel();
    actor.handle_command(RegistryCommand::RegisterDiscovered {
        session_id: pending_id.clone(),
        pid: current_pid,
        cwd: std::path::PathBuf::from("/home/user/project"),
        tmux_pane: None,
        harness: atm_core::Harness::Pi,
        respond_to: tx,
    });
    rx.await.unwrap().unwrap();
    assert_eq!(actor.session_count(), 1);

    // Drain register/update events from discovery.
    while event_rx.try_recv().is_ok() {}

    // Step 2: a vendor adapter (pi) fires a lifecycle event with
    // the real session id and the same PID. The session should
    // get its id upgraded.
    let (tx, rx) = oneshot::channel();
    actor.handle_command(RegistryCommand::ApplyLifecycleEvent {
        session_id: SessionId::new("real-pi-session"),
        event: atm_core::LifecycleEvent::WorkingStart,
        harness: atm_core::Harness::Pi,
        pid: Some(current_pid),
        tmux_pane: None,
        context: LifecycleContext::default(),
        respond_to: tx,
    });
    rx.await.unwrap().unwrap();

    // Still one session (upgraded, not added).
    assert_eq!(actor.session_count(), 1);

    // Lookup by real id succeeds.
    let (tx, rx) = oneshot::channel();
    actor.handle_command(RegistryCommand::GetSession {
        session_id: SessionId::new("real-pi-session"),
        respond_to: tx,
    });
    let session = rx.await.unwrap();
    assert!(session.is_some(), "real id should resolve");
    assert_eq!(session.unwrap().id.as_str(), "real-pi-session");

    // Lookup by old pending id fails.
    let (tx, rx) = oneshot::channel();
    actor.handle_command(RegistryCommand::GetSession {
        session_id: pending_id,
        respond_to: tx,
    });
    assert!(
        rx.await.unwrap().is_none(),
        "pending id should no longer resolve"
    );

    // The Removed{Upgraded} + Registered event pair should fire.
    let mut found_removed = false;
    let mut found_registered = false;
    while let Ok(event) = event_rx.try_recv() {
        match event {
            SessionEvent::Removed {
                reason: RemovalReason::Upgraded,
                ..
            } => found_removed = true,
            SessionEvent::Registered { session_id, .. }
                if session_id.as_str() == "real-pi-session" =>
            {
                found_registered = true;
            }
            _ => {}
        }
    }
    assert!(found_removed, "expected Removed{{Upgraded}} for pending id");
    assert!(found_registered, "expected Registered for real id");
}

#[tokio::test]
async fn test_lifecycle_event_does_not_rename_real_to_real() {
    // Defensive: only pending → real triggers reconcile. Two
    // adapter events with different real session_ids on the same
    // PID must NOT thrash the index — that would mean some other
    // bug (PID reuse, stale state) and the daemon shouldn't paper
    // over it by silently renaming. (Only an untagged SessionStart
    // acts on a new real id: it ends the row's session.)
    let (_, mut actor, _) = create_actor();
    let current_pid = std::process::id();

    // Seed a session with a real (non-pending) id via discovery.
    let (tx, rx) = oneshot::channel();
    actor.handle_command(RegistryCommand::RegisterDiscovered {
        session_id: SessionId::new("first-real-id"),
        pid: current_pid,
        cwd: std::path::PathBuf::from("/tmp"),
        tmux_pane: None,
        harness: atm_core::Harness::Pi,
        respond_to: tx,
    });
    rx.await.unwrap().unwrap();

    // Apply a lifecycle event with a *different* real id.
    let (tx, rx) = oneshot::channel();
    actor.handle_command(RegistryCommand::ApplyLifecycleEvent {
        session_id: SessionId::new("different-real-id"),
        event: atm_core::LifecycleEvent::WorkingStart,
        harness: atm_core::Harness::Pi,
        pid: Some(current_pid),
        tmux_pane: None,
        context: LifecycleContext::default(),
        respond_to: tx,
    });
    let _ = rx.await.unwrap();

    // Original id still resolves; rename did not occur.
    let (tx, rx) = oneshot::channel();
    actor.handle_command(RegistryCommand::GetSession {
        session_id: SessionId::new("first-real-id"),
        respond_to: tx,
    });
    assert!(
        rx.await.unwrap().is_some(),
        "real id must not be renamed by another real id"
    );
}

#[tokio::test]
async fn test_no_duplicate_sessions_for_same_pid() {
    // This is the key test for the fix: with PID as primary key,
    // we should never have duplicate sessions for the same Claude process.
    let (_, mut actor, _) = create_actor();

    // Use the current process PID (a real PID that set_pid can validate)
    let current_pid = std::process::id();

    // Simulate discovery finding a transcript with one session ID
    let discovered_id = SessionId::new("discovered-uuid");
    let (tx, rx) = oneshot::channel();
    let cmd = RegistryCommand::RegisterDiscovered {
        session_id: discovered_id.clone(),
        pid: current_pid,
        cwd: std::path::PathBuf::from("/home/user/project"),
        tmux_pane: None,
        harness: atm_core::Harness::Unknown,
        respond_to: tx,
    };
    actor.handle_command(cmd);
    let result = rx.await.unwrap();
    assert!(result.is_ok());
    assert_eq!(actor.session_count(), 1);

    // Now simulate status line arriving with a DIFFERENT session ID but SAME PID
    // (This was the bug scenario - before the fix, this would create a duplicate)
    let real_id = SessionId::new("real-uuid-from-status-line");
    let status_json = serde_json::json!({
        "session_id": "real-uuid-from-status-line",
        "pid": current_pid,
        "model": {"id": "claude-sonnet-4-20250514"},
        "cost": {"total_cost_usd": 0.10, "total_duration_ms": 5000},
        "context_window": {"total_input_tokens": 1000, "context_window_size": 200000}
    });
    let (tx, rx) = oneshot::channel();
    let (pid, data) = parse_status_line(status_json).unwrap();
    let cmd = RegistryCommand::UpdateFromStatusLine {
        session_id: real_id.clone(),
        pid,
        data,
        harness: atm_core::Harness::ClaudeCode,
        respond_to: tx,
    };
    actor.handle_command(cmd);
    let result = rx.await.unwrap();
    assert!(result.is_ok());

    // CRITICAL: Should still have only 1 session, not 2!
    assert_eq!(
        actor.session_count(),
        1,
        "Should have 1 session, not duplicates"
    );

    // The session should now have the real ID from the status line
    let (tx, rx) = oneshot::channel();
    let cmd = RegistryCommand::GetSession {
        session_id: real_id.clone(),
        respond_to: tx,
    };
    actor.handle_command(cmd);
    let session = rx.await.unwrap();
    assert!(session.is_some(), "Session should exist with real ID");
    assert_eq!(session.unwrap().id.as_str(), "real-uuid-from-status-line");

    // The old discovered ID should no longer exist
    let (tx, rx) = oneshot::channel();
    let cmd = RegistryCommand::GetSession {
        session_id: discovered_id,
        respond_to: tx,
    };
    actor.handle_command(cmd);
    let old_session = rx.await.unwrap();
    assert!(
        old_session.is_none(),
        "Old session ID should not exist anymore"
    );
}

// ========================================================================
// CWD Change Detection Tests
// ========================================================================

#[tokio::test]
async fn test_refresh_git_info_detects_branch_change() {
    let (_cmd_tx, mut actor, mut event_rx) = create_actor();

    // Create a temp repo
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("refresh-repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();

    // Register a discovered session
    let current_pid = std::process::id();
    let (tx, rx) = oneshot::channel();
    actor.handle_command(RegistryCommand::RegisterDiscovered {
        session_id: SessionId::new("refresh-test"),
        pid: current_pid,
        cwd: repo.clone(),
        tmux_pane: None,
        harness: atm_core::Harness::Unknown,
        respond_to: tx,
    });
    rx.await.unwrap().unwrap();
    actor.finish_git_lookups();

    // Drain events
    while event_rx.try_recv().is_ok() {}

    // Verify initial branch
    let (tx, rx) = oneshot::channel();
    actor.handle_command(RegistryCommand::GetSession {
        session_id: SessionId::new("refresh-test"),
        respond_to: tx,
    });
    let view = rx.await.unwrap().unwrap();
    assert_eq!(view.worktree_branch.as_deref(), Some("main"));

    // Change branch on disk
    std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/develop\n").unwrap();

    // Trigger git info refresh
    actor.handle_command(RegistryCommand::RefreshGitInfo);
    actor.finish_git_lookups();

    // Verify branch was updated
    let (tx, rx) = oneshot::channel();
    actor.handle_command(RegistryCommand::GetSession {
        session_id: SessionId::new("refresh-test"),
        respond_to: tx,
    });
    let view = rx.await.unwrap().unwrap();
    assert_eq!(
        view.worktree_branch.as_deref(),
        Some("develop"),
        "branch should be updated after RefreshGitInfo"
    );

    // Should have published an Updated event
    let event = event_rx.try_recv();
    assert!(
        matches!(event, Ok(SessionEvent::Updated { .. })),
        "should publish Updated event on branch change"
    );
}

#[tokio::test]
async fn test_refresh_git_info_no_change_no_event() {
    let (_cmd_tx, mut actor, mut event_rx) = create_actor();

    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("no-change-repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();

    let current_pid = std::process::id();
    let (tx, rx) = oneshot::channel();
    actor.handle_command(RegistryCommand::RegisterDiscovered {
        session_id: SessionId::new("no-change-test"),
        pid: current_pid,
        cwd: repo.clone(),
        tmux_pane: None,
        harness: atm_core::Harness::Unknown,
        respond_to: tx,
    });
    rx.await.unwrap().unwrap();

    // Drain events from registration
    while event_rx.try_recv().is_ok() {}

    // Trigger refresh without changing anything
    actor.handle_command(RegistryCommand::RefreshGitInfo);

    // Should NOT publish any event
    let event = event_rx.try_recv();
    assert!(
        event.is_err(),
        "should NOT publish event when nothing changed"
    );
}

#[tokio::test]
async fn test_rediscovery_preserves_domain_metadata() {
    let (_cmd_tx, mut actor, _event_rx) = create_actor();

    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("preserve-repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();

    let current_pid = std::process::id();

    // Register initial discovery
    let (tx, rx) = oneshot::channel();
    actor.handle_command(RegistryCommand::RegisterDiscovered {
        session_id: SessionId::new("pending-1"),
        pid: current_pid,
        cwd: repo.clone(),
        tmux_pane: None,
        harness: atm_core::Harness::Unknown,
        respond_to: tx,
    });
    rx.await.unwrap().unwrap();

    // Upgrade via status line (accumulate cost)
    let status_json = serde_json::json!({
        "session_id": "real-id",
        "pid": current_pid,
        "cwd": repo.to_str().unwrap(),
        "model": {"id": "claude-sonnet-4-20250514"},
        "cost": {"total_cost_usd": 2.50, "total_duration_ms": 120000},
        "context_window": {
            "total_input_tokens": 80000,
            "total_output_tokens": 20000,
            "context_window_size": 200000
        }
    });
    let (tx, rx) = oneshot::channel();
    let (pid, data) = parse_status_line(status_json).unwrap();
    actor.handle_command(RegistryCommand::UpdateFromStatusLine {
        session_id: SessionId::new("real-id"),
        pid,
        data,
        harness: atm_core::Harness::ClaudeCode,
        respond_to: tx,
    });
    rx.await.unwrap().unwrap();

    // Verify cost accumulated
    let (tx, rx) = oneshot::channel();
    actor.handle_command(RegistryCommand::GetSession {
        session_id: SessionId::new("real-id"),
        respond_to: tx,
    });
    let view = rx.await.unwrap().unwrap();
    assert!(view.cost_usd > 2.0, "cost should be ~2.50");

    // Re-discover (simulating rescan)
    let (tx, rx) = oneshot::channel();
    actor.handle_command(RegistryCommand::RegisterDiscovered {
        session_id: SessionId::new("pending-rescan"),
        pid: current_pid,
        cwd: repo.clone(),
        tmux_pane: None,
        harness: atm_core::Harness::Unknown,
        respond_to: tx,
    });
    rx.await.unwrap().unwrap();

    // Verify metadata preserved under the real session_id
    let (tx, rx) = oneshot::channel();
    actor.handle_command(RegistryCommand::GetSession {
        session_id: SessionId::new("real-id"),
        respond_to: tx,
    });
    let view = rx.await.unwrap().unwrap();
    assert!(
        view.cost_usd > 2.0,
        "cost should be preserved after rescan, got {}",
        view.cost_usd
    );

    // The rescan's pending session_id should not enter the index
    let (tx, rx) = oneshot::channel();
    actor.handle_command(RegistryCommand::GetSession {
        session_id: SessionId::new("pending-rescan"),
        respond_to: tx,
    });
    let pending = rx.await.unwrap();
    assert!(
        pending.is_none(),
        "real session_id should not be downgraded"
    );
}

#[tokio::test]
async fn test_update_from_status_line_cwd_change_re_resolves_git() {
    let (_cmd_tx, mut actor, _event_rx) = create_actor();

    let dir = tempfile::tempdir().unwrap();
    let repo_a = dir.path().join("repo-a");
    let repo_b = dir.path().join("repo-b");
    std::fs::create_dir_all(repo_a.join(".git")).unwrap();
    std::fs::create_dir_all(repo_b.join(".git")).unwrap();
    std::fs::write(repo_a.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    std::fs::write(repo_b.join(".git/HEAD"), "ref: refs/heads/feature\n").unwrap();

    let current_pid = std::process::id();

    // Register in repo_a
    let (tx, rx) = oneshot::channel();
    actor.handle_command(RegistryCommand::RegisterDiscovered {
        session_id: SessionId::new("cwd-test"),
        pid: current_pid,
        cwd: repo_a.clone(),
        tmux_pane: None,
        harness: atm_core::Harness::Unknown,
        respond_to: tx,
    });
    rx.await.unwrap().unwrap();
    actor.finish_git_lookups();

    // Verify initial state
    let (tx, rx) = oneshot::channel();
    actor.handle_command(RegistryCommand::GetSession {
        session_id: SessionId::new("cwd-test"),
        respond_to: tx,
    });
    let view = rx.await.unwrap().unwrap();
    assert_eq!(view.worktree_branch.as_deref(), Some("main"));

    // Send status line with cwd changed to repo_b
    let status_json = serde_json::json!({
        "session_id": "cwd-test",
        "pid": current_pid,
        "cwd": repo_b.to_str().unwrap(),
        "model": {"id": "claude-sonnet-4-20250514"},
        "cost": {"total_cost_usd": 0.50, "total_duration_ms": 5000},
        "context_window": {"total_input_tokens": 1000, "context_window_size": 200000}
    });
    let (tx, rx) = oneshot::channel();
    let (pid, data) = parse_status_line(status_json).unwrap();
    actor.handle_command(RegistryCommand::UpdateFromStatusLine {
        session_id: SessionId::new("cwd-test"),
        pid,
        data,
        harness: atm_core::Harness::ClaudeCode,
        respond_to: tx,
    });
    rx.await.unwrap().unwrap();
    actor.finish_git_lookups();

    // Verify git info re-resolved
    let (tx, rx) = oneshot::channel();
    actor.handle_command(RegistryCommand::GetSession {
        session_id: SessionId::new("cwd-test"),
        respond_to: tx,
    });
    let view = rx.await.unwrap().unwrap();
    assert_eq!(
        view.worktree_branch.as_deref(),
        Some("feature"),
        "branch should be re-resolved after cwd change"
    );
    assert_eq!(
        view.project_root.as_deref(),
        Some(repo_b.to_str().unwrap()),
        "project_root should point to repo_b"
    );
}

/// Creates a git repo at `path` whose HEAD names `branch`.
fn init_repo(path: &std::path::Path, branch: &str) {
    std::fs::create_dir_all(path.join(".git")).unwrap();
    std::fs::write(
        path.join(".git/HEAD"),
        format!("ref: refs/heads/{branch}\n"),
    )
    .unwrap();
}

fn register_session_in(actor: &mut RegistryActor, session: SessionDomain) {
    let (tx, mut rx) = oneshot::channel();
    actor.handle_command(RegistryCommand::Register {
        session: Box::new(session),
        respond_to: tx,
    });
    rx.try_recv().unwrap().unwrap();
}

#[test]
fn test_register_resolves_git_unless_project_root_is_set() {
    let (_, mut actor, _) = create_actor();
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("register-repo");
    init_repo(&repo, "main");
    let repo = repo.to_str().unwrap();
    // A lookup applies to every session in its directory, so the preset
    // session needs its own.
    let other = dir.path().join("preset-repo");
    init_repo(&other, "main");

    let mut fresh = create_test_session("register-fresh");
    fresh.working_directory = Some(repo.to_string());
    register_session_in(&mut actor, fresh);
    let mut preset = create_test_session("register-preset");
    preset.working_directory = Some(other.to_str().unwrap().to_string());
    preset.project_root = Some("/preset".to_string());
    register_session_in(&mut actor, preset);
    let mut no_cwd = create_test_session("register-no-cwd");
    no_cwd.worktree_branch = Some("kept".to_string());
    register_session_in(&mut actor, no_cwd);
    actor.finish_git_lookups();

    let fresh = session_view(&actor, "register-fresh").unwrap();
    assert_eq!(fresh.project_root.as_deref(), Some(repo));
    assert_eq!(fresh.worktree_path.as_deref(), Some(repo));
    assert_eq!(fresh.worktree_branch.as_deref(), Some("main"));
    let preset = session_view(&actor, "register-preset").unwrap();
    assert_eq!(preset.project_root.as_deref(), Some("/preset"));
    assert_eq!(
        preset.worktree_branch, None,
        "a preset project root skips git resolution"
    );
    let no_cwd = session_view(&actor, "register-no-cwd").unwrap();
    assert_eq!(
        no_cwd.worktree_branch.as_deref(),
        Some("kept"),
        "without a working directory the git fields are left alone"
    );
}

#[test]
fn test_status_line_without_pid_re_resolves_git_on_cwd_change() {
    let (_, mut actor, _) = create_actor();
    let dir = tempfile::tempdir().unwrap();
    let repo_a = dir.path().join("repo-a");
    let repo_b = dir.path().join("repo-b");
    init_repo(&repo_a, "main");
    init_repo(&repo_b, "feature");
    let mut session = create_test_session("no-pid-cwd");
    session.working_directory = Some(repo_a.to_str().unwrap().to_string());
    register_session_in(&mut actor, session);

    // No "pid" field: the registry finds the session by its id.
    let (pid, data) = parse_status_line(serde_json::json!({
        "session_id": "no-pid-cwd",
        "cwd": repo_b.to_str().unwrap(),
        "model": {"id": "claude-sonnet-4-20250514"}
    }))
    .unwrap();
    assert_eq!(pid, None);
    let (tx, mut rx) = oneshot::channel();
    actor.handle_command(RegistryCommand::UpdateFromStatusLine {
        session_id: SessionId::new("no-pid-cwd"),
        pid,
        data,
        harness: atm_core::Harness::ClaudeCode,
        respond_to: tx,
    });
    rx.try_recv().unwrap().unwrap();
    actor.finish_git_lookups();

    let view = session_view(&actor, "no-pid-cwd").unwrap();
    assert_eq!(view.project_root.as_deref(), repo_b.to_str());
    assert_eq!(view.worktree_branch.as_deref(), Some("feature"));
}

#[test]
fn test_status_line_auto_register_resolves_linked_worktree() {
    let (_, mut actor, _) = create_actor();
    let dir = tempfile::tempdir().unwrap();
    let main_repo = dir.path().join("main-repo");
    let gitdir = main_repo.join(".git/worktrees/wt");
    let worktree = dir.path().join("wt");
    std::fs::create_dir_all(&gitdir).unwrap();
    std::fs::write(gitdir.join("HEAD"), "ref: refs/heads/wt-branch\n").unwrap();
    std::fs::create_dir_all(&worktree).unwrap();
    std::fs::write(
        worktree.join(".git"),
        format!("gitdir: {}\n", gitdir.display()),
    )
    .unwrap();

    let (pid, data) = parse_status_line(serde_json::json!({
        "session_id": "wt-session",
        "pid": std::process::id(),
        "cwd": worktree.to_str().unwrap(),
        "model": {"id": "claude-sonnet-4-20250514"}
    }))
    .unwrap();
    let (tx, mut rx) = oneshot::channel();
    actor.handle_command(RegistryCommand::UpdateFromStatusLine {
        session_id: SessionId::new("wt-session"),
        pid,
        data,
        harness: atm_core::Harness::ClaudeCode,
        respond_to: tx,
    });
    rx.try_recv().unwrap().unwrap();
    actor.finish_git_lookups();

    let view = session_view(&actor, "wt-session").unwrap();
    assert_eq!(view.project_root.as_deref(), main_repo.to_str());
    assert_eq!(view.worktree_path.as_deref(), worktree.to_str());
    assert_eq!(view.worktree_branch.as_deref(), Some("wt-branch"));
}

#[test]
fn test_lifecycle_event_for_new_pid_resolves_git_from_proc_cwd() {
    let (_, mut actor, _) = create_actor();
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("lifecycle-repo");
    init_repo(&repo, "main");
    // /proc/{pid}/cwd is canonical, so compare against the canonical path.
    let repo = repo.canonicalize().unwrap();
    // `read` blocks sh on stdin, so the process lives until `kill`.
    let mut process = std::process::Command::new("sh")
        .args(["-c", "read _"])
        .current_dir(&repo)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .expect("spawn stand-in agent");

    let (respond_to, _) = oneshot::channel();
    actor.handle_command(RegistryCommand::ApplyLifecycleEvent {
        session_id: SessionId::new("lifecycle-git"),
        event: LifecycleEvent::WorkingStart,
        harness: atm_core::Harness::ClaudeCode,
        pid: Some(process.id()),
        tmux_pane: None,
        context: LifecycleContext::default(),
        respond_to,
    });
    actor.finish_git_lookups();
    let view = session_view(&actor, "lifecycle-git");
    let _ = process.kill();
    let _ = process.wait();

    let view = view.unwrap();
    assert_eq!(view.project_root.as_deref(), repo.to_str());
    assert_eq!(view.worktree_branch.as_deref(), Some("main"));
}

fn register_test_session(actor: &mut RegistryActor, id: &str) -> SessionId {
    let (respond_to, _) = oneshot::channel();
    actor.handle_command(RegistryCommand::Register {
        session: Box::new(create_test_session(id)),
        respond_to,
    });
    SessionId::new(id)
}

fn apply_with_context(
    actor: &mut RegistryActor,
    parent: &SessionId,
    event: LifecycleEvent,
    context: LifecycleContext,
) {
    let (respond_to, _) = oneshot::channel();
    actor.handle_command(RegistryCommand::ApplyLifecycleEvent {
        session_id: parent.clone(),
        event,
        harness: atm_core::Harness::ClaudeCode,
        pid: None,
        tmux_pane: None,
        context,
        respond_to,
    });
}

fn session_view(actor: &RegistryActor, id: &str) -> Option<SessionView> {
    actor.handle_get_session(&SessionId::new(id))
}

fn start_child(actor: &mut RegistryActor, parent: &SessionId, id: &str) {
    apply_with_context(
        actor,
        parent,
        LifecycleEvent::ChildSessionStart {
            id: Some(id.into()),
            role: Some("general-purpose".into()),
        },
        LifecycleContext::default(),
    );
}

fn needs_input_from(actor: &mut RegistryActor, parent: &SessionId, context: LifecycleContext) {
    apply_with_context(
        actor,
        parent,
        LifecycleEvent::NeedsInput {
            reason: NeedsInputReason::PermissionGate { tool: Tool::Bash },
        },
        context,
    );
}

#[test]
fn in_process_child_lifecycle_and_event_routing() {
    let (_, mut actor, _) = create_actor();
    let parent = register_test_session(&mut actor, "lead");
    if let Some((session, _)) = actor.sessions_by_pid.values_mut().next() {
        session.tmux_pane = Some("%7".into());
        session.project_root = Some("/repo".into());
    }

    start_child(&mut actor, &parent, "agent-1");
    let child = session_view(&actor, "agent-1").expect("child created");
    assert_eq!(child.parent_session_id, Some(parent.clone()));
    assert_eq!(child.tmux_pane.as_deref(), Some("%7"));
    assert_eq!(child.project_root.as_deref(), Some("/repo"));

    let child_context = LifecycleContext {
        child: Some(ChildAgent {
            reference: ChildRef::Id("agent-1".into()),
            agent_type: AgentType::Subagent,
        }),
        ..LifecycleContext::default()
    };
    let lead_status = session_view(&actor, "lead").map(|view| view.status);
    needs_input_from(&mut actor, &parent, child_context.clone());
    assert_eq!(
        session_view(&actor, "agent-1").map(|view| view.status),
        Some(atm_core::SessionStatus::AttentionNeeded)
    );
    // The child's needs-input stays on the child's row.
    assert_eq!(
        session_view(&actor, "lead").map(|view| view.status),
        lead_status
    );

    apply_with_context(
        &mut actor,
        &parent,
        LifecycleEvent::SessionEnd { reason: None },
        child_context,
    );
    assert!(session_view(&actor, "lead").is_some());

    apply_with_context(
        &mut actor,
        &parent,
        LifecycleEvent::ChildSessionEnd {
            id: Some("agent-1".into()),
        },
        LifecycleContext::default(),
    );
    assert!(session_view(&actor, "agent-1").is_none());
    assert!(session_view(&actor, "lead").is_some_and(|view| view.child_session_ids.is_empty()));
}

#[test]
fn teammate_alias_handles_both_arrival_orders_and_is_parent_scoped() {
    let (_, mut actor, _) = create_actor();
    let first = register_test_session(&mut actor, "lead-a");
    let second = register_test_session(&mut actor, "lead-b");
    let alias = |agent_id: &str| LifecycleContext {
        child_alias: Some(ChildAlias {
            name: "reviewer".into(),
            id: agent_id.into(),
        }),
        ..LifecycleContext::default()
    };
    let by_name = || LifecycleContext {
        child: Some(ChildAgent {
            reference: ChildRef::Name("reviewer".into()),
            agent_type: AgentType::Teammate,
        }),
        ..LifecycleContext::default()
    };

    // Alias before the child starts.
    apply_with_context(
        &mut actor,
        &first,
        LifecycleEvent::WorkingStart,
        alias("agent-a"),
    );
    start_child(&mut actor, &first, "agent-a");

    // Name-only event before the alias: placeholder is upgraded away.
    apply_with_context(&mut actor, &second, LifecycleEvent::Idle, by_name());
    assert!(session_view(&actor, "reviewer@lead-b").is_some());
    apply_with_context(
        &mut actor,
        &second,
        LifecycleEvent::WorkingStart,
        alias("agent-b"),
    );
    start_child(&mut actor, &second, "agent-b");
    assert!(session_view(&actor, "reviewer@lead-b").is_none());

    for (parent, expected) in [(&first, "agent-a"), (&second, "agent-b")] {
        needs_input_from(&mut actor, parent, by_name());
        assert_eq!(
            session_view(&actor, expected).map(|view| view.status),
            Some(atm_core::SessionStatus::AttentionNeeded)
        );
    }
    assert_eq!(actor.session_count(), 4);
}

#[test]
fn parent_cleanup_cascades_to_children() {
    let (_, mut actor, _) = create_actor();
    let parent = register_test_session(&mut actor, "lead");
    start_child(&mut actor, &parent, "synthetic");

    let (respond_to, _) = oneshot::channel();
    actor.handle_command(RegistryCommand::Remove {
        session_id: parent,
        respond_to,
    });
    assert!(session_view(&actor, "synthetic").is_none());
}

/// A tmux-mode teammate is its own process whose argv names the lead
/// via `--parent-session-id`. Uses a real process so the
/// `/proc/{pid}/cmdline` read is exercised.
#[test]
fn process_teammate_links_to_lead_from_cmdline_and_outlives_it() {
    let (_, mut actor, _) = create_actor();
    let lead = register_test_session(&mut actor, "lead-1");
    // The `read` builtin blocks sh itself on stdin: no child process
    // that could outlive `kill`, and no exec to replace the argv.
    let mut process = std::process::Command::new("sh")
        .args(["-c", "read _", "claude", "--parent-session-id"])
        .arg(lead.as_str())
        .stdin(std::process::Stdio::piped())
        .spawn()
        .expect("spawn fake teammate");
    // Until exec completes, the child's cmdline is still the test's.
    let cmdline = format!("/proc/{}/cmdline", process.id());
    let flag = b"--parent-session-id";
    for _ in 0..200 {
        let exec_done =
            std::fs::read(&cmdline).is_ok_and(|argv| argv.windows(flag.len()).any(|w| w == flag));
        if exec_done {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    let (respond_to, _) = oneshot::channel();
    actor.handle_command(RegistryCommand::ApplyLifecycleEvent {
        session_id: SessionId::new("teammate-1"),
        event: LifecycleEvent::WorkingStart,
        harness: atm_core::Harness::ClaudeCode,
        pid: Some(process.id()),
        tmux_pane: None,
        context: LifecycleContext::default(),
        respond_to,
    });
    let teammate = session_view(&actor, "teammate-1");
    let lead_view = session_view(&actor, "lead-1");

    let (respond_to, _) = oneshot::channel();
    actor.handle_command(RegistryCommand::Remove {
        session_id: lead.clone(),
        respond_to,
    });
    let survived = session_view(&actor, "teammate-1").is_some();
    let _ = process.kill();
    let _ = process.wait();

    assert_eq!(teammate.and_then(|view| view.parent_session_id), Some(lead));
    assert!(lead_view.is_some_and(|view| view.child_session_ids.is_empty()));
    assert!(
        survived,
        "removing the lead must not remove a process teammate"
    );
}

#[test]
fn quiet_stop_sweeps_children_but_busy_stop_keeps_them() {
    let (_, mut actor, _) = create_actor();
    let parent = register_test_session(&mut actor, "lead");
    start_child(&mut actor, &parent, "agent-1");
    let stop = |running, scheduled| LifecycleContext {
        background_activity: Some(BackgroundActivity { running, scheduled }),
        ..LifecycleContext::default()
    };

    apply_with_context(&mut actor, &parent, LifecycleEvent::WorkingEnd, stop(2, 1));
    assert!(session_view(&actor, "agent-1").is_some());
    assert_eq!(
        session_view(&actor, "lead").and_then(|view| view.activity_detail),
        Some("2 bg tasks, 1 scheduled".into())
    );

    apply_with_context(&mut actor, &parent, LifecycleEvent::WorkingEnd, stop(0, 0));
    assert!(session_view(&actor, "agent-1").is_none());
}

// ========================================================================
// Status-line and Updated-event pins
// ========================================================================

/// Drains the queued events. Returns a `"<kind> <id>"` summary of each,
/// in order, and the views the `Updated` events carried.
fn drain_events(rx: &mut broadcast::Receiver<SessionEvent>) -> (Vec<String>, Vec<SessionView>) {
    let mut summaries = Vec::new();
    let mut views = Vec::new();
    while let Ok(event) = rx.try_recv() {
        summaries.push(match event {
            SessionEvent::Registered { session_id, .. } => format!("Registered {session_id}"),
            SessionEvent::Updated { session } => {
                let summary = format!("Updated {}", session.id);
                views.push(*session);
                summary
            }
            SessionEvent::Removed { session_id, reason } => {
                format!("Removed {session_id} {reason:?}")
            }
        });
    }
    (summaries, views)
}

/// `view` as JSON without its clock-derived age fields.
fn view_json(view: &SessionView) -> serde_json::Value {
    let mut json = serde_json::to_value(view).unwrap();
    let fields = json.as_object_mut().unwrap();
    fields.remove("last_activity_display").unwrap();
    fields.remove("age_display").unwrap();
    json
}

/// Asserts that a published view matches a fresh view of the same session.
fn assert_view_is_current(actor: &RegistryActor, view: &SessionView) {
    let current = session_view(actor, view.id.as_str()).unwrap();
    assert_eq!(view_json(view), view_json(&current));
}

fn send_status_line(actor: &mut RegistryActor, json: serde_json::Value) {
    let session_id = SessionId::new(json["session_id"].as_str().unwrap());
    let (pid, data) = parse_status_line(json).unwrap();
    let (tx, mut rx) = oneshot::channel();
    actor.handle_command(RegistryCommand::UpdateFromStatusLine {
        session_id,
        pid,
        data,
        harness: atm_core::Harness::ClaudeCode,
        respond_to: tx,
    });
    rx.try_recv().unwrap().unwrap();
}

fn discover(
    actor: &mut RegistryActor,
    id: SessionId,
    pid: u32,
    cwd: &std::path::Path,
    tmux_pane: Option<&str>,
) {
    let (tx, mut rx) = oneshot::channel();
    actor.handle_command(RegistryCommand::RegisterDiscovered {
        session_id: id,
        pid,
        cwd: cwd.to_path_buf(),
        tmux_pane: tmux_pane.map(String::from),
        harness: atm_core::Harness::ClaudeCode,
        respond_to: tx,
    });
    rx.try_recv().unwrap().unwrap();
}

fn pid_of(actor: &RegistryActor, id: &str) -> u32 {
    actor.session_id_to_pid[&SessionId::new(id)]
}

fn path_str(path: &std::path::Path) -> &str {
    path.to_str().unwrap()
}

/// Runs `f` and returns what it logged, up to DEBUG, one line per event.
fn capture_logs(f: impl FnOnce()) -> String {
    // tracing caches each callsite's interest. While only one
    // dispatcher is registered it asks just the thread that hits the
    // callsite first, so a parallel test could switch a line off for
    // this one. An idle second dispatcher makes it ask every live one.
    static IDLE: std::sync::OnceLock<tracing::Dispatch> = std::sync::OnceLock::new();
    IDLE.get_or_init(|| tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default()));
    #[derive(Clone, Default)]
    struct Buffer(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Buffer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let buffer = Buffer::default();
    let writer = buffer.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_ansi(false)
        .without_time()
        .with_target(false)
        .with_writer(move || writer.clone())
        .finish();
    tracing::subscriber::with_default(subscriber, f);
    let bytes = buffer.0.lock().unwrap().clone();
    String::from_utf8(bytes).unwrap()
}

#[test]
fn rediscovery_of_an_upgraded_session_publishes_one_updated_view() {
    let (_, mut actor, mut event_rx) = create_actor();
    let dir = tempfile::tempdir().unwrap();
    let (repo_a, repo_b) = (dir.path().join("repo-a"), dir.path().join("repo-b"));
    init_repo(&repo_a, "main");
    init_repo(&repo_b, "feature");
    let pid = std::process::id();
    let pending = SessionId::pending_from_pid(pid);
    discover(&mut actor, pending.clone(), pid, &repo_a, None);
    send_status_line(
        &mut actor,
        serde_json::json!({
            "session_id": "real-a",
            "pid": pid,
            "cwd": path_str(&repo_a),
            "model": {"id": "claude-sonnet-4-20250514"},
            "cost": {"total_cost_usd": 0.30, "total_duration_ms": 5000}
        }),
    );
    actor.finish_git_lookups();
    drain_events(&mut event_rx);

    // Discovery only knows the pending id; the session keeps its real one.
    discover(&mut actor, pending, pid, &repo_b, Some("%7"));

    let (events, views) = drain_events(&mut event_rx);
    assert_eq!(events, ["Updated real-a"]);
    let view = &views[0];
    assert_view_is_current(&actor, view);
    // The view shortens long paths, so check the stored session's cwd.
    let stored = &actor.sessions_by_pid[&pid].0;
    assert_eq!(stored.working_directory.as_deref(), Some(path_str(&repo_b)));
    assert_eq!(view.tmux_pane.as_deref(), Some("%7"));
    assert_eq!(
        view.cost_usd, 0.30,
        "rediscovery keeps the session's metadata"
    );

    actor.finish_git_lookups();

    let (events, views) = drain_events(&mut event_rx);
    assert_eq!(events, ["Updated real-a"]);
    assert_eq!(views[0].project_root.as_deref(), Some(path_str(&repo_b)));
    assert_eq!(views[0].worktree_branch.as_deref(), Some("feature"));
}

#[test]
fn new_discovery_publishes_registered_then_updated() {
    let (_, mut actor, mut event_rx) = create_actor();
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    init_repo(&repo, "main");
    let pid = std::process::id();
    let id = SessionId::pending_from_pid(pid);

    discover(&mut actor, id.clone(), pid, &repo, None);

    let (events, views) = drain_events(&mut event_rx);
    assert_eq!(
        events,
        [format!("Registered {id}"), format!("Updated {id}")]
    );
    assert_view_is_current(&actor, &views[0]);

    actor.finish_git_lookups();

    let (events, views) = drain_events(&mut event_rx);
    assert_eq!(events, [format!("Updated {id}")]);
    assert_eq!(views[0].project_root.as_deref(), Some(path_str(&repo)));
    assert_eq!(views[0].worktree_branch.as_deref(), Some("main"));
}

#[test]
fn status_line_by_session_id_updates_and_publishes() {
    let (_, mut actor, mut event_rx) = create_actor();
    let dir = tempfile::tempdir().unwrap();
    let (repo_a, repo_b) = (dir.path().join("repo-a"), dir.path().join("repo-b"));
    init_repo(&repo_a, "main");
    init_repo(&repo_b, "feature");
    let mut session = create_test_session("by-id");
    session.working_directory = Some(path_str(&repo_a).to_string());
    register_session_in(&mut actor, session);
    drain_events(&mut event_rx);

    // No "pid" field: the registry finds the session by its id.
    send_status_line(
        &mut actor,
        serde_json::json!({
            "session_id": "by-id",
            "cwd": path_str(&repo_b),
            "model": {"id": "claude-sonnet-4-20250514"},
            "cost": {"total_cost_usd": 0.50, "total_duration_ms": 5000}
        }),
    );

    let (events, views) = drain_events(&mut event_rx);
    assert_eq!(events, ["Updated by-id"]);
    assert_view_is_current(&actor, &views[0]);
    assert_eq!(views[0].cost_usd, 0.50);

    actor.finish_git_lookups();

    let (events, views) = drain_events(&mut event_rx);
    assert_eq!(events, ["Updated by-id"]);
    assert_eq!(views[0].project_root.as_deref(), Some(path_str(&repo_b)));
    assert_eq!(views[0].worktree_branch.as_deref(), Some("feature"));

    send_status_line(
        &mut actor,
        serde_json::json!({
            "session_id": "by-id",
            "model": {"id": "claude-sonnet-4-20250514"},
            "cost": {"total_cost_usd": 0.75, "total_duration_ms": 6000}
        }),
    );

    let (events, views) = drain_events(&mut event_rx);
    assert_eq!(events, ["Updated by-id"]);
    assert_view_is_current(&actor, &views[0]);
    assert_eq!(views[0].cost_usd, 0.75);
    assert_eq!(views[0].project_root.as_deref(), Some(path_str(&repo_b)));
}

#[test]
fn status_line_by_pid_upgrade_publishes_removed_registered_updated() {
    let (_, mut actor, mut event_rx) = create_actor();
    let dir = tempfile::tempdir().unwrap();
    let (repo_a, repo_b) = (dir.path().join("repo-a"), dir.path().join("repo-b"));
    init_repo(&repo_a, "main");
    init_repo(&repo_b, "feature");
    let pid = std::process::id();
    let pending = SessionId::pending_from_pid(pid);
    discover(&mut actor, pending.clone(), pid, &repo_a, None);
    drain_events(&mut event_rx);

    send_status_line(
        &mut actor,
        serde_json::json!({
            "session_id": "real-d",
            "pid": pid,
            "cwd": path_str(&repo_b),
            "model": {"id": "claude-sonnet-4-20250514"},
            "cost": {"total_cost_usd": 0.40, "total_duration_ms": 5000}
        }),
    );

    let (events, views) = drain_events(&mut event_rx);
    assert_eq!(
        events,
        [
            format!("Removed {pending} Upgraded"),
            "Registered real-d".to_string(),
            "Updated real-d".to_string(),
        ]
    );
    assert_view_is_current(&actor, &views[0]);
    assert_eq!(views[0].cost_usd, 0.40);

    actor.finish_git_lookups();

    let (events, views) = drain_events(&mut event_rx);
    assert_eq!(events, ["Updated real-d"]);
    assert_eq!(views[0].project_root.as_deref(), Some(path_str(&repo_b)));
    assert_eq!(views[0].worktree_branch.as_deref(), Some("feature"));

    send_status_line(
        &mut actor,
        serde_json::json!({
            "session_id": "real-d",
            "pid": pid,
            "model": {"id": "claude-sonnet-4-20250514"},
            "cost": {"total_cost_usd": 0.60, "total_duration_ms": 6000}
        }),
    );

    let (events, views) = drain_events(&mut event_rx);
    assert_eq!(events, ["Updated real-d"]);
    assert_view_is_current(&actor, &views[0]);
    assert_eq!(views[0].cost_usd, 0.60);
}

#[test]
fn status_line_auto_register_publishes_registered_then_updated() {
    let (_, mut actor, mut event_rx) = create_actor();
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    init_repo(&repo, "main");
    let status_line = |cost: f64| {
        serde_json::json!({
            "session_id": "auto-e",
            "pid": std::process::id(),
            "cwd": path_str(&repo),
            "model": {"id": "claude-sonnet-4-20250514"},
            "cost": {"total_cost_usd": cost, "total_duration_ms": 5000}
        })
    };

    send_status_line(&mut actor, status_line(0.10));

    let (events, views) = drain_events(&mut event_rx);
    assert_eq!(events, ["Registered auto-e", "Updated auto-e"]);
    assert_view_is_current(&actor, &views[0]);
    assert_eq!(views[0].cost_usd, 0.10);

    actor.finish_git_lookups();

    let (events, views) = drain_events(&mut event_rx);
    assert_eq!(events, ["Updated auto-e"]);
    assert_eq!(views[0].project_root.as_deref(), Some(path_str(&repo)));
    assert_eq!(views[0].worktree_branch.as_deref(), Some("main"));

    send_status_line(&mut actor, status_line(0.20));

    let (events, views) = drain_events(&mut event_rx);
    assert_eq!(events, ["Updated auto-e"]);
    assert_view_is_current(&actor, &views[0]);
    assert_eq!(views[0].cost_usd, 0.20);
}

#[test]
fn status_line_resolves_git_when_project_root_is_unset() {
    let (_, mut actor, _) = create_actor();
    let dir = tempfile::tempdir().unwrap();
    let (repo_a, repo_b) = (dir.path().join("repo-a"), dir.path().join("repo-b"));
    init_repo(&repo_a, "main");
    init_repo(&repo_b, "main");
    let mut by_id = create_test_session("unset-by-id");
    by_id.working_directory = Some(path_str(&repo_a).to_string());
    register_session_in(&mut actor, by_id);
    let pid = std::process::id();
    discover(
        &mut actor,
        SessionId::new("unset-by-pid"),
        pid,
        &repo_b,
        None,
    );
    actor.finish_git_lookups();
    // As if the directories were not repos when the sessions were
    // resolved. Clearing the fields directly keeps the test independent
    // of whether TMPDIR sits inside some other checkout.
    for id in ["unset-by-id", "unset-by-pid"] {
        let session = &mut actor
            .sessions_by_pid
            .get_mut(&pid_of(&actor, id))
            .unwrap()
            .0;
        session.project_root = None;
        session.worktree_path = None;
        session.worktree_branch = None;
    }

    // The status lines report no cwd change.
    let model = serde_json::json!({"id": "claude-sonnet-4-20250514"});
    send_status_line(
        &mut actor,
        serde_json::json!({"session_id": "unset-by-id", "model": model}),
    );
    send_status_line(
        &mut actor,
        serde_json::json!({"session_id": "unset-by-pid", "pid": pid, "model": model}),
    );
    actor.finish_git_lookups();

    let by_id = session_view(&actor, "unset-by-id").unwrap();
    assert_eq!(by_id.project_root.as_deref(), Some(path_str(&repo_a)));
    assert_eq!(by_id.worktree_branch.as_deref(), Some("main"));
    let by_pid = session_view(&actor, "unset-by-pid").unwrap();
    assert_eq!(by_pid.project_root.as_deref(), Some(path_str(&repo_b)));
    assert_eq!(by_pid.worktree_branch.as_deref(), Some("main"));
}

#[test]
fn status_line_logs_keep_each_paths_session_id() {
    let (_, mut actor, _) = create_actor();
    let dir = tempfile::tempdir().unwrap();
    let (repo_a, repo_b) = (dir.path().join("repo-a"), dir.path().join("repo-b"));
    init_repo(&repo_a, "main");
    init_repo(&repo_b, "feature");

    // A stale index entry: "stale-alias" still points at the session
    // now called "renamed". The by-session_id path logs the id it
    // looked up, not the session's own.
    let mut renamed = create_test_session("renamed");
    renamed.working_directory = Some(path_str(&repo_a).to_string());
    register_session_in(&mut actor, renamed);
    let renamed_pid = pid_of(&actor, "renamed");
    actor
        .session_id_to_pid
        .insert(SessionId::new("stale-alias"), renamed_pid);
    let logs = capture_logs(|| {
        send_status_line(
            &mut actor,
            serde_json::json!({
                "session_id": "stale-alias",
                "cwd": path_str(&repo_b),
                "model": {"id": "claude-sonnet-4-20250514"},
                "cost": {"total_cost_usd": 0.50, "total_duration_ms": 5000}
            }),
        );
    });
    assert_eq!(
        logs,
        format!(
            " INFO Working directory changed, re-resolving git info \
             session_id=stale-alias pid={renamed_pid} new_cwd={}\n\
             DEBUG Session updated from status line (by session_id) \
             session_id=stale-alias pid={renamed_pid} cost=$0.50\n",
            path_str(&repo_b)
        )
    );

    // By pid: the session is renamed first, so it logs the new id.
    let pid = std::process::id();
    let pending = SessionId::pending_from_pid(pid);
    discover(&mut actor, pending.clone(), pid, &repo_a, None);
    let logs = capture_logs(|| {
        send_status_line(
            &mut actor,
            serde_json::json!({
                "session_id": "real-p",
                "pid": pid,
                "cwd": path_str(&repo_b),
                "model": {"id": "claude-sonnet-4-20250514"},
                "cost": {"total_cost_usd": 0.25, "total_duration_ms": 5000}
            }),
        );
    });
    assert_eq!(
        logs,
        format!(
            " INFO Session ID upgraded old_id={pending} new_id=real-p pid={pid}\n \
             INFO Working directory changed, re-resolving git info \
             session_id=real-p pid={pid} new_cwd={}\n\
             DEBUG Session updated from status line session_id=real-p pid={pid} \
             cost=$0.25\n",
            path_str(&repo_b)
        )
    );
}

#[test]
fn refresh_git_info_publishes_each_changed_session() {
    let (_, mut actor, mut event_rx) = create_actor();
    let dir = tempfile::tempdir().unwrap();
    let ids = ["git-0", "git-1", "git-2", "git-3"];
    for id in ids {
        let repo = dir.path().join(id);
        init_repo(&repo, "main");
        let mut session = create_test_session(id);
        session.working_directory = Some(path_str(&repo).to_string());
        register_session_in(&mut actor, session);
    }
    actor.finish_git_lookups();
    drain_events(&mut event_rx);
    let changed = ["git-0", "git-1", "git-3"];
    for id in changed {
        init_repo(&dir.path().join(id), "develop");
    }

    actor.handle_command(RegistryCommand::RefreshGitInfo);
    actor.finish_git_lookups();

    let expected: Vec<String> = changed.iter().map(|id| format!("Updated {id}")).collect();
    let (mut events, views) = drain_events(&mut event_rx);
    // Each directory's result lands on its own; order within one is pinned
    // by `a_result_is_published_in_map_order_within_a_cwd`.
    events.sort();
    assert_eq!(events, expected);
    for view in &views {
        assert_view_is_current(&actor, view);
        assert_eq!(view.worktree_branch.as_deref(), Some("develop"));
    }
}

// ========================================================================
// Removal pins
// ========================================================================

/// The sorted ids in the session_id index, then in the rows.
fn index_and_row_ids(actor: &RegistryActor) -> (Vec<String>, Vec<String>) {
    let mut index: Vec<String> = actor
        .session_id_to_pid
        .keys()
        .map(ToString::to_string)
        .collect();
    let mut rows: Vec<String> = actor
        .sessions_by_pid
        .values()
        .map(|(session, _)| session.id.to_string())
        .collect();
    index.sort();
    rows.sort();
    (index, rows)
}

/// The sorted `parent/reference->child` entries of `child_refs`.
fn child_refs(actor: &RegistryActor) -> Vec<String> {
    let mut refs: Vec<String> = actor
        .child_refs
        .iter()
        .map(|((parent, reference), child)| format!("{parent}/{reference:?}->{child}"))
        .collect();
    refs.sort();
    refs
}

fn alias(name: &str, id: &str) -> LifecycleContext {
    LifecycleContext {
        child_alias: Some(ChildAlias {
            name: name.into(),
            id: id.into(),
        }),
        ..LifecycleContext::default()
    }
}

fn ids(ids: &[&str]) -> Vec<String> {
    ids.iter().map(ToString::to_string).collect()
}

/// The line `handle_remove` and `handle_remove_by_pid` log.
fn removed_log(id: &str, pid: u32, reason: &str, remaining: usize) -> String {
    format!(
        " INFO Session removed session_id={id} pid={pid} reason={reason} \
         remaining_sessions={remaining}\n"
    )
}

/// Drains setup events, failing loudly if setup overflowed the channel.
fn drain_setup(rx: &mut broadcast::Receiver<SessionEvent>) {
    loop {
        match rx.try_recv() {
            Ok(_) => {}
            Err(broadcast::error::TryRecvError::Empty) => return,
            Err(error) => panic!("setup overflowed the event channel: {error:?}"),
        }
    }
}

/// A `sleep` process, killed and reaped when dropped.
struct Sleep(std::process::Child);

impl Drop for Sleep {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Starts `sleep` and registers it as `id` through a lifecycle event,
/// so the row tracks a real pid that cleanup can find dead.
fn register_sleep(actor: &mut RegistryActor, id: &str, context: LifecycleContext) -> Sleep {
    let process = Sleep(
        std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep"),
    );
    let pid = process.0.id();
    let (respond_to, _) = oneshot::channel();
    actor.handle_command(RegistryCommand::ApplyLifecycleEvent {
        session_id: SessionId::new(id),
        event: LifecycleEvent::WorkingStart,
        harness: atm_core::Harness::ClaudeCode,
        pid: Some(pid),
        tmux_pane: None,
        context,
        respond_to,
    });
    assert_eq!(actor.sessions_by_pid[&pid].1.pid, Some(pid));
    process
}

/// `capture_logs` of a cleanup, without the liveness checks' DEBUG
/// lines, which come in map order.
fn capture_cleanup_logs(actor: &mut RegistryActor) -> String {
    capture_logs(|| actor.handle_command(RegistryCommand::CleanupStale))
        .lines()
        .filter(|line| !line.contains("is_process_alive"))
        .map(|line| format!("{line}\n"))
        .collect()
}

/// SessionEnd → `handle_remove_by_pid`: the parent's in-process
/// children are removed, logged and published first, then the parent
/// logs and publishes. Its index entry and child refs go with it.
#[test]
fn session_end_removes_children_before_the_parent() {
    let (_, mut actor, mut event_rx) = create_actor();
    let lead = register_test_session(&mut actor, "lead");
    let other = register_test_session(&mut actor, "other");
    let working = LifecycleEvent::WorkingStart;
    apply_with_context(&mut actor, &lead, working.clone(), alias("rev", "agent-1"));
    apply_with_context(&mut actor, &other, working, alias("tester", "agent-3"));
    drain_setup(&mut event_rx);
    start_child(&mut actor, &lead, "agent-1");
    start_child(&mut actor, &lead, "agent-2");
    start_child(&mut actor, &other, "agent-3");
    let [lead_pid, agent_1, agent_2] = ["lead", "agent-1", "agent-2"].map(|id| pid_of(&actor, id));
    drain_setup(&mut event_rx);

    let logs = capture_logs(|| {
        apply_with_context(
            &mut actor,
            &lead,
            LifecycleEvent::SessionEnd { reason: None },
            LifecycleContext::default(),
        );
    });

    assert_eq!(
        logs,
        format!(
            " INFO SessionEnd received, removing session session_id=lead pid={lead_pid}\n{}{}{}",
            removed_log("agent-1", agent_1, "session ended", 3),
            removed_log("agent-2", agent_2, "session ended", 2),
            removed_log("lead", lead_pid, "session ended", 2),
        )
    );
    assert_eq!(
        drain_events(&mut event_rx).0,
        [
            "Removed agent-1 SessionEnded",
            "Removed agent-2 SessionEnded",
            "Removed lead SessionEnded"
        ]
    );
    let remaining = ids(&["agent-3", "other"]);
    assert_eq!(index_and_row_ids(&actor), (remaining.clone(), remaining));
    assert_eq!(
        child_refs(&actor),
        [
            "other/Id(\"agent-3\")->agent-3",
            "other/Name(\"tester\")->agent-3"
        ]
    );

    // A pid with no row reports a synthetic `pid-{pid}` id.
    let result = actor.handle_remove_by_pid(lead_pid, RemovalReason::SessionEnded);
    assert!(
        matches!(&result, Err(RegistryError::SessionNotFound(id)) if *id == SessionId::new(format!("pid-{lead_pid}"))),
        "{result:?}"
    );
    assert!(drain_events(&mut event_rx).0.is_empty());
}

/// ChildSessionEnd → `handle_remove`: a nested child goes first, then
/// the parent drops the child from its list and publishes, then the
/// child logs and publishes `Removed`. Refs to and under the child are
/// pruned; a sibling's stay.
#[test]
fn child_session_end_updates_the_parent_before_the_childs_removed() {
    let (_, mut actor, mut event_rx) = create_actor();
    let lead = register_test_session(&mut actor, "lead");
    let working = LifecycleEvent::WorkingStart;
    apply_with_context(&mut actor, &lead, working.clone(), alias("rev", "mid"));
    apply_with_context(&mut actor, &lead, working, alias("helper", "sibling"));
    drain_setup(&mut event_rx);
    start_child(&mut actor, &lead, "mid");
    start_child(&mut actor, &lead, "sibling");
    let mid = SessionId::new("mid");
    actor.ensure_child_session(
        pid_of(&actor, "mid"),
        SessionId::new("leaf"),
        AgentType::Subagent,
        atm_core::Harness::ClaudeCode,
    );
    actor.register_child_alias(
        &mid,
        &ChildAlias {
            name: "nested".into(),
            id: "leaf".into(),
        },
    );
    let [lead_pid, mid_pid, leaf_pid] = ["lead", "mid", "leaf"].map(|id| pid_of(&actor, id));
    drain_setup(&mut event_rx);

    let logs = capture_logs(|| {
        apply_with_context(
            &mut actor,
            &lead,
            LifecycleEvent::ChildSessionEnd {
                id: Some("mid".into()),
            },
            LifecycleContext::default(),
        );
    });

    let applied = logs.lines().nth(2).unwrap_or_default();
    assert!(applied.starts_with("DEBUG Lifecycle event applied session_id=lead"));
    assert_eq!(
        logs,
        format!(
            "{}{}{applied}\n",
            removed_log("leaf", leaf_pid, "session ended", 2),
            removed_log("mid", mid_pid, "session ended", 2),
        )
    );
    let (events, views) = drain_events(&mut event_rx);
    assert_eq!(
        events,
        [
            "Removed leaf SessionEnded",
            "Updated lead",
            "Removed mid SessionEnded",
            "Updated lead"
        ]
    );
    assert_eq!(views[0].child_session_ids, [SessionId::new("sibling")]);
    let remaining = ids(&["lead", "sibling"]);
    assert_eq!(index_and_row_ids(&actor), (remaining.clone(), remaining));
    assert_eq!(
        child_refs(&actor),
        [
            "lead/Id(\"sibling\")->sibling",
            "lead/Name(\"helper\")->sibling"
        ]
    );
    assert_eq!(
        actor.sessions_by_pid[&lead_pid].0.child_session_ids,
        [SessionId::new("sibling")]
    );
}

/// `handle_remove` trusts the index: it removes the row the id maps to
/// and reports the id it was given, even when that row has another id
/// or is gone. An id not in the index is an error naming that id.
#[test]
fn remove_reports_the_id_it_was_given() {
    let (_, mut actor, mut event_rx) = create_actor();
    register_test_session(&mut actor, "renamed");
    let renamed_pid = pid_of(&actor, "renamed");
    actor
        .session_id_to_pid
        .insert(SessionId::new("stale-alias"), renamed_pid);
    actor
        .session_id_to_pid
        .insert(SessionId::new("ghost"), 4242);
    drain_events(&mut event_rx);

    let mut results = Vec::new();
    let logs = capture_logs(|| {
        for id in ["stale-alias", "ghost", "missing"] {
            let (respond_to, mut rx) = oneshot::channel();
            actor.handle_command(RegistryCommand::Remove {
                session_id: SessionId::new(id),
                respond_to,
            });
            results.push(rx.try_recv().unwrap());
        }
    });

    assert_eq!(
        logs,
        format!(
            "{}{}",
            removed_log("stale-alias", renamed_pid, "explicitly removed", 0),
            removed_log("ghost", 4242, "explicitly removed", 0),
        )
    );
    assert!(results[0].is_ok() && results[1].is_ok());
    assert!(
        matches!(&results[2], Err(RegistryError::SessionNotFound(id)) if id.as_str() == "missing"),
        "{results:?}"
    );
    assert_eq!(
        drain_events(&mut event_rx).0,
        ["Removed stale-alias Explicit", "Removed ghost Explicit"]
    );
    assert_eq!(index_and_row_ids(&actor), (ids(&["renamed"]), ids(&[])));
}

/// Cleanup of a dead process: its in-process children go first (an
/// INFO line and `Removed` each), then its WARN line with the last
/// activity, then its `Removed{ProcessDied}`.
#[test]
fn cleanup_removes_a_dead_sessions_children_before_it() {
    let (_, mut actor, mut event_rx) = create_actor();
    let process = register_sleep(&mut actor, "dead-lead", alias("rev", "agent-1"));
    let pid = process.0.id();
    start_child(&mut actor, &SessionId::new("dead-lead"), "agent-1");
    drain_setup(&mut event_rx);
    let live = register_test_session(&mut actor, "live");
    apply_with_context(
        &mut actor,
        &live,
        LifecycleEvent::WorkingStart,
        alias("helper", "agent-2"),
    );
    start_child(&mut actor, &live, "agent-2");
    let child_pid = pid_of(&actor, "agent-1");
    drop(process);
    actor.sessions_by_pid.get_mut(&pid).unwrap().0.last_activity =
        Utc::now() - chrono::Duration::seconds(300);
    drain_setup(&mut event_rx);

    let logs = capture_cleanup_logs(&mut actor);

    // Cleanup reads the clock itself, so allow for a stalled runner.
    let secs = logs
        .split("last_activity=")
        .nth(1)
        .and_then(|rest| rest.split('s').next())
        .unwrap_or_default();
    assert!((300..310).contains(&secs.parse::<i64>().unwrap()), "{logs}");
    let logs = logs.replace(&format!("last_activity={secs}s"), "last_activity=300s");
    assert_eq!(
        logs,
        format!(
            " INFO Cleaning up dead-process sessions count=1\n{} WARN Session removed by \
             cleanup session_id=dead-lead reason=process died without a session-end signal \
             details=last_activity=300s ago, pid={pid}\n",
            removed_log(
                "agent-1",
                child_pid,
                "process died without a session-end signal",
                2
            ),
        )
    );
    assert_eq!(
        drain_events(&mut event_rx).0,
        [
            "Removed agent-1 ProcessDied",
            "Removed dead-lead ProcessDied"
        ]
    );
    let remaining = ids(&["agent-2", "live"]);
    assert_eq!(index_and_row_ids(&actor), (remaining.clone(), remaining));
    assert_eq!(
        child_refs(&actor),
        [
            "live/Id(\"agent-2\")->agent-2",
            "live/Name(\"helper\")->agent-2"
        ]
    );
}

/// Both by-pid paths keep the removed row's id in the index when it
/// points at another row with the same id, so that row stays
/// reachable by id (#53 survey gap G2a).
#[test]
fn by_pid_removal_keeps_a_shared_ids_index_entry() {
    let own_pid = std::process::id();
    let working_at = |actor: &mut RegistryActor, pid| {
        let (respond_to, _) = oneshot::channel();
        actor.handle_command(RegistryCommand::ApplyLifecycleEvent {
            session_id: SessionId::new("dup"),
            event: LifecycleEvent::WorkingStart,
            harness: atm_core::Harness::ClaudeCode,
            pid: Some(pid),
            tmux_pane: None,
            context: LifecycleContext::default(),
            respond_to,
        });
    };

    // SessionEnd for the row the index no longer points at.
    let (_, mut actor, _) = create_actor();
    let dup = register_test_session(&mut actor, "dup");
    let synthetic_pid = pid_of(&actor, "dup");
    working_at(&mut actor, own_pid);
    assert_eq!(pid_of(&actor, "dup"), own_pid);
    let (respond_to, _) = oneshot::channel();
    actor.handle_command(RegistryCommand::ApplyLifecycleEvent {
        session_id: dup,
        event: LifecycleEvent::SessionEnd { reason: None },
        harness: atm_core::Harness::ClaudeCode,
        pid: Some(synthetic_pid),
        tmux_pane: None,
        context: LifecycleContext::default(),
        respond_to,
    });
    assert_eq!(index_and_row_ids(&actor), (ids(&["dup"]), ids(&["dup"])));
    assert!(actor.sessions_by_pid.contains_key(&own_pid));
    assert_eq!(pid_of(&actor, "dup"), own_pid);
    let cost = send_pidless_dup_status(&mut actor);
    assert_eq!(actor.sessions_by_pid[&own_pid].0.cost, cost);

    // Cleanup of the dead row the index no longer points at.
    let (_, mut actor, _) = create_actor();
    let process = register_sleep(&mut actor, "dup", LifecycleContext::default());
    let dead_pid = process.0.id();
    working_at(&mut actor, own_pid);
    assert_eq!(pid_of(&actor, "dup"), own_pid);
    drop(process);
    actor.handle_command(RegistryCommand::CleanupStale);
    assert!(!actor.sessions_by_pid.contains_key(&dead_pid));
    assert_eq!(index_and_row_ids(&actor), (ids(&["dup"]), ids(&["dup"])));
    assert_eq!(pid_of(&actor, "dup"), own_pid);
    let cost = send_pidless_dup_status(&mut actor);
    assert_eq!(actor.sessions_by_pid[&own_pid].0.cost, cost);
}

/// Two live processes reporting one id: two rows, and the index points
/// at the newer pid. Returns (older, newer).
fn register_shared_id(actor: &mut RegistryActor) -> (Sleep, Sleep) {
    let older = register_sleep(actor, "dup", LifecycleContext::default());
    let newer = register_sleep(actor, "dup", LifecycleContext::default());
    assert_eq!(pid_of(actor, "dup"), newer.0.id());
    assert_eq!(
        index_and_row_ids(actor),
        (ids(&["dup"]), ids(&["dup", "dup"]))
    );
    (older, newer)
}

/// Sends a pid-less status line for `dup` and returns the cost it
/// carries. Without a pid it reaches a row only through the id index.
fn send_pidless_dup_status(actor: &mut RegistryActor) -> atm_core::Money {
    send_status_line(
        actor,
        serde_json::json!({
            "session_id": "dup",
            "model": {"id": "claude-sonnet-4-20250514"},
            "cost": {"total_cost_usd": 0.42, "total_duration_ms": 6000}
        }),
    );
    atm_core::Money::from_usd(0.42)
}

/// Known limit, left for the insert-side follow-up (#53 survey G2):
/// removing the row the index points at drops the shared id's entry,
/// so the other row with that id is unreachable by id.
#[test]
fn removing_the_indexed_row_orphans_a_shared_ids_other_row() {
    let (_, mut actor, _) = create_actor();
    let (older, newer) = register_shared_id(&mut actor);
    let (respond_to, _) = oneshot::channel();
    actor.handle_command(RegistryCommand::ApplyLifecycleEvent {
        session_id: SessionId::new("dup"),
        event: LifecycleEvent::SessionEnd { reason: None },
        harness: atm_core::Harness::ClaudeCode,
        pid: Some(newer.0.id()),
        tmux_pane: None,
        context: LifecycleContext::default(),
        respond_to,
    });
    assert_eq!(index_and_row_ids(&actor), (ids(&[]), ids(&["dup"])));

    send_pidless_dup_status(&mut actor);

    assert_eq!(
        actor.sessions_by_pid[&older.0.id()].0.cost,
        atm_core::Money::zero()
    );
}

/// Rediscovery that renames the row the index does not point at keeps
/// the shared id's entry on the other row (#53 survey gap G2a).
#[test]
fn rediscovery_rename_keeps_a_shared_ids_index_entry() {
    let (_, mut actor, _) = create_actor();
    let (older, newer) = register_shared_id(&mut actor);
    let dir = tempfile::tempdir().unwrap();

    discover(
        &mut actor,
        SessionId::new("fresh"),
        older.0.id(),
        dir.path(),
        None,
    );

    let both = ids(&["dup", "fresh"]);
    assert_eq!(index_and_row_ids(&actor), (both.clone(), both));
    assert_eq!(pid_of(&actor, "dup"), newer.0.id());
    assert_eq!(pid_of(&actor, "fresh"), older.0.id());
    let cost = send_pidless_dup_status(&mut actor);
    assert_eq!(actor.sessions_by_pid[&newer.0.id()].0.cost, cost);
}

/// A status line that renames the row the index does not point at
/// keeps the shared id's entry on the other row (#53 survey gap G2a).
#[test]
fn status_line_rename_keeps_a_shared_ids_index_entry() {
    let (_, mut actor, mut event_rx) = create_actor();
    let (older, newer) = register_shared_id(&mut actor);
    drain_setup(&mut event_rx);

    send_status_line(
        &mut actor,
        serde_json::json!({
            "session_id": "fresh",
            "pid": older.0.id(),
            "model": {"id": "claude-sonnet-4-20250514"},
            "cost": {"total_cost_usd": 0.30, "total_duration_ms": 5000}
        }),
    );

    assert_eq!(
        drain_events(&mut event_rx).0,
        ["Removed dup Upgraded", "Registered fresh", "Updated fresh"]
    );
    let both = ids(&["dup", "fresh"]);
    assert_eq!(index_and_row_ids(&actor), (both.clone(), both));
    assert_eq!(pid_of(&actor, "dup"), newer.0.id());
    assert_eq!(pid_of(&actor, "fresh"), older.0.id());
    let cost = send_pidless_dup_status(&mut actor);
    assert_eq!(actor.sessions_by_pid[&newer.0.id()].0.cost, cost);
}

// ========================================================================
// Rename link pins (#53 survey gap G3)
// ========================================================================

/// Sends a status line for `pid` reporting `id`. The status-line path
/// renames the session at `pid` on any id mismatch.
fn status_line_as(actor: &mut RegistryActor, pid: u32, id: &str) {
    send_status_line(
        actor,
        serde_json::json!({
            "session_id": id,
            "pid": pid,
            "model": {"id": "claude-sonnet-4-20250514"}
        }),
    );
}

/// Sends a `WorkingStart` for `pid` reporting `id`. The lifecycle path
/// creates the session if `pid` has none, and renames only a pending id.
fn working_as(actor: &mut RegistryActor, pid: u32, id: &str) {
    apply_event(actor, id, Some(pid), LifecycleEvent::WorkingStart).unwrap();
}

/// Starts in-process children `agent-1` (alias `rev`) and `agent-2`
/// (alias `helper`) under `parent`.
fn start_aliased_children(actor: &mut RegistryActor, parent: &SessionId) {
    for (name, id) in [("rev", "agent-1"), ("helper", "agent-2")] {
        apply_with_context(actor, parent, LifecycleEvent::WorkingStart, alias(name, id));
        start_child(actor, parent, id);
    }
}

/// `Updated <id>` for each of `ids`, in the registry's map order.
fn updated_in_map_order(actor: &RegistryActor, ids: &[&str]) -> Vec<String> {
    actor
        .sessions_by_pid
        .values()
        .map(|(session, _)| session.id.as_str())
        .filter(|id| ids.contains(id))
        .map(|id| format!("Updated {id}"))
        .collect()
}

/// A child event that names `name` under `parent`, as a teammate's
/// events do.
fn by_name(name: &str) -> LifecycleContext {
    LifecycleContext {
        child: Some(ChildAgent {
            reference: ChildRef::Name(name.into()),
            agent_type: AgentType::Teammate,
        }),
        ..LifecycleContext::default()
    }
}

/// Asserts that `parent` and its children `agent-1` and `agent-2` link
/// to each other in all three stores, and that a by-name child event
/// sent to `parent` still reaches `agent-1`.
fn assert_children_follow(actor: &mut RegistryActor, parent: &str) {
    let parent_id = SessionId::new(parent);
    for child in ["agent-1", "agent-2"] {
        let stored = &actor.sessions_by_pid[&pid_of(actor, child)].0;
        assert_eq!(
            stored.parent_session_id.as_ref(),
            Some(&parent_id),
            "{child}"
        );
    }
    assert_eq!(
        actor.sessions_by_pid[&pid_of(actor, parent)]
            .0
            .child_session_ids,
        [SessionId::new("agent-1"), SessionId::new("agent-2")]
    );
    assert_eq!(
        child_refs(actor),
        [
            format!("{parent}/Id(\"agent-1\")->agent-1"),
            format!("{parent}/Id(\"agent-2\")->agent-2"),
            format!("{parent}/Name(\"helper\")->agent-2"),
            format!("{parent}/Name(\"rev\")->agent-1"),
        ]
    );

    let count = actor.session_count();
    needs_input_from(actor, &parent_id, by_name("rev"));
    assert_eq!(
        session_view(actor, "agent-1").map(|view| view.status),
        Some(atm_core::SessionStatus::AttentionNeeded)
    );
    assert_eq!(actor.session_count(), count, "no placeholder child");
}

/// A status line with a new id for a parent's pid (real → real, e.g.
/// after `/clear`) moves the parent's links: its children point at
/// the new id, and its aliases resolve under it. Each child's view is
/// published after the rename's Removed/Registered pair, before the
/// parent's own Updated.
#[test]
fn status_line_rename_of_a_parent_carries_its_children() {
    let (_, mut actor, mut event_rx) = create_actor();
    let pid = std::process::id();
    working_as(&mut actor, pid, "lead");
    drain_setup(&mut event_rx);
    start_aliased_children(&mut actor, &SessionId::new("lead"));
    drain_setup(&mut event_rx);

    status_line_as(&mut actor, pid, "lead-2");

    let mut expected = vec!["Removed lead Upgraded".into(), "Registered lead-2".into()];
    expected.extend(updated_in_map_order(&actor, &["agent-1", "agent-2"]));
    expected.push("Updated lead-2".into());
    let (events, views) = drain_events(&mut event_rx);
    assert_eq!(events, expected);
    for view in &views {
        assert_view_is_current(&actor, view);
    }
    let all = ids(&["agent-1", "agent-2", "lead-2"]);
    assert_eq!(index_and_row_ids(&actor), (all.clone(), all));
    assert_children_follow(&mut actor, "lead-2");
}

/// A lifecycle event with the real id for a pending parent (pending →
/// real) moves the parent's links, as the status-line path does.
#[test]
fn lifecycle_rename_of_a_pending_parent_carries_its_children() {
    let (_, mut actor, mut event_rx) = create_actor();
    let dir = tempfile::tempdir().unwrap();
    let pid = std::process::id();
    let pending = SessionId::pending_from_pid(pid);
    discover(&mut actor, pending.clone(), pid, dir.path(), None);
    drain_setup(&mut event_rx);
    start_aliased_children(&mut actor, &pending);
    drain_setup(&mut event_rx);

    working_as(&mut actor, pid, "lead-real");

    let mut expected = vec![
        format!("Removed {pending} Upgraded"),
        "Registered lead-real".into(),
    ];
    expected.extend(updated_in_map_order(&actor, &["agent-1", "agent-2"]));
    expected.push("Updated lead-real".into());
    let (events, views) = drain_events(&mut event_rx);
    assert_eq!(events, expected);
    for view in &views {
        assert_view_is_current(&actor, view);
    }
    let all = ids(&["agent-1", "agent-2", "lead-real"]);
    assert_eq!(index_and_row_ids(&actor), (all.clone(), all));
    assert_children_follow(&mut actor, "lead-real");
}

/// Rediscovery that renames a parent (real → real; production
/// discovery sends only pending ids today) moves the parent's links.
/// The children's views follow the parent's own Updated. Rediscovery
/// with a pending id keeps the real one, so nothing is relinked.
#[test]
fn rediscovery_rename_of_a_parent_carries_its_children() {
    let (_, mut actor, mut event_rx) = create_actor();
    let dir = tempfile::tempdir().unwrap();
    let pid = std::process::id();
    working_as(&mut actor, pid, "lead");
    drain_setup(&mut event_rx);
    start_aliased_children(&mut actor, &SessionId::new("lead"));
    drain_setup(&mut event_rx);

    let pending = SessionId::pending_from_pid(pid);
    discover(&mut actor, pending, pid, dir.path(), None);
    assert_eq!(drain_events(&mut event_rx).0, ["Updated lead"]);

    discover(&mut actor, SessionId::new("lead-b"), pid, dir.path(), None);

    let mut expected = vec!["Updated lead-b".to_string()];
    expected.extend(updated_in_map_order(&actor, &["agent-1", "agent-2"]));
    let (events, views) = drain_events(&mut event_rx);
    assert_eq!(events, expected);
    for view in &views {
        assert_view_is_current(&actor, view);
    }
    let all = ids(&["agent-1", "agent-2", "lead-b"]);
    assert_eq!(index_and_row_ids(&actor), (all.clone(), all));
    assert_children_follow(&mut actor, "lead-b");
}

/// Gives the newer of two `dup` rows a child `agent-1` (alias `rev`),
/// runs `rename` on the older row's pid, and asserts the events and
/// that every link still names `dup`.
fn assert_shared_id_rename_keeps_links(
    rename: impl FnOnce(&mut RegistryActor, u32),
    expected: &[&str],
) {
    let (_, mut actor, mut event_rx) = create_actor();
    let (older, newer) = register_shared_id(&mut actor);
    let dup = SessionId::new("dup");
    let working = LifecycleEvent::WorkingStart;
    apply_with_context(&mut actor, &dup, working, alias("rev", "agent-1"));
    start_child(&mut actor, &dup, "agent-1");
    drain_setup(&mut event_rx);

    rename(&mut actor, older.0.id());

    assert_eq!(drain_events(&mut event_rx).0, expected);
    let child = &actor.sessions_by_pid[&pid_of(&actor, "agent-1")].0;
    assert_eq!(child.parent_session_id.as_ref(), Some(&dup));
    assert_eq!(
        actor.sessions_by_pid[&newer.0.id()].0.child_session_ids,
        [SessionId::new("agent-1")]
    );
    assert_eq!(
        child_refs(&actor),
        ["dup/Id(\"agent-1\")->agent-1", "dup/Name(\"rev\")->agent-1"]
    );
}

/// Renaming one of two rows that share an id leaves every link to that
/// id alone, as on main: the links may belong to the other row, which
/// still holds it (shared ids are open, #97). Covers both rename sites.
#[test]
fn rename_of_a_shared_id_leaves_its_links() {
    assert_shared_id_rename_keeps_links(
        |actor, pid| status_line_as(actor, pid, "fresh"),
        &["Removed dup Upgraded", "Registered fresh", "Updated fresh"],
    );
    assert_shared_id_rename_keeps_links(
        |actor, pid| {
            let dir = tempfile::tempdir().unwrap();
            discover(actor, SessionId::new("fresh"), pid, dir.path(), None);
        },
        &["Updated fresh"],
    );
}

/// Discovers this process as a pending child of a new `lead` session.
/// The parent's links to the child (its child list and a `mate` alias)
/// are set by hand: no production path links a real-pid child from
/// its parent today. Returns the pending id.
fn pending_child_of_lead(actor: &mut RegistryActor) -> SessionId {
    let dir = tempfile::tempdir().unwrap();
    let lead = register_test_session(actor, "lead");
    let pid = std::process::id();
    let pending = SessionId::pending_from_pid(pid);
    discover(actor, pending.clone(), pid, dir.path(), None);
    actor
        .sessions_by_pid
        .get_mut(&pid)
        .unwrap()
        .0
        .parent_session_id = Some(lead.clone());
    let lead_pid = pid_of(actor, "lead");
    let lead_session = &mut actor.sessions_by_pid.get_mut(&lead_pid).unwrap().0;
    lead_session.child_session_ids.push(pending.clone());
    actor
        .child_refs
        .insert((lead, ChildRef::Name("mate".into())), pending.clone());
    pending
}

/// Asserts that `lead` and its renamed child `mate` link to each other
/// in all three stores, and that a by-name `mate` event sent to `lead`
/// reaches the renamed row instead of creating a new child.
fn assert_parent_follows(actor: &mut RegistryActor) {
    let lead = SessionId::new("lead");
    let pid = std::process::id();
    assert_eq!(
        actor.sessions_by_pid[&pid].0.parent_session_id.as_ref(),
        Some(&lead)
    );
    assert_eq!(
        actor.sessions_by_pid[&pid_of(actor, "lead")]
            .0
            .child_session_ids,
        [SessionId::new("mate")]
    );
    assert_eq!(child_refs(actor), ["lead/Name(\"mate\")->mate"]);

    let count = actor.session_count();
    needs_input_from(actor, &lead, by_name("mate"));
    assert_eq!(
        actor.sessions_by_pid[&pid].0.status,
        atm_core::SessionStatus::AttentionNeeded
    );
    assert_eq!(actor.session_count(), count, "no duplicate child");
}

/// A status line with the real id for a pending child renames it in
/// its parent's child list and aliases. The parent's view is published
/// after the rename's Removed/Registered pair.
#[test]
fn status_line_rename_of_a_child_updates_its_parent() {
    let (_, mut actor, mut event_rx) = create_actor();
    let pending = pending_child_of_lead(&mut actor);
    drain_setup(&mut event_rx);

    status_line_as(&mut actor, std::process::id(), "mate");

    let (events, views) = drain_events(&mut event_rx);
    assert_eq!(
        events,
        [
            format!("Removed {pending} Upgraded"),
            "Registered mate".into(),
            "Updated lead".into(),
            "Updated mate".into(),
        ]
    );
    for view in &views {
        assert_view_is_current(&actor, view);
    }
    let all = ids(&["lead", "mate"]);
    assert_eq!(index_and_row_ids(&actor), (all.clone(), all));
    assert_parent_follows(&mut actor);
}

/// The lifecycle path's pending → real rename of a child updates its
/// parent, as the status-line path does.
#[test]
fn lifecycle_rename_of_a_pending_child_updates_its_parent() {
    let (_, mut actor, mut event_rx) = create_actor();
    let pending = pending_child_of_lead(&mut actor);
    drain_setup(&mut event_rx);

    working_as(&mut actor, std::process::id(), "mate");

    let (events, views) = drain_events(&mut event_rx);
    assert_eq!(
        events,
        [
            format!("Removed {pending} Upgraded"),
            "Registered mate".into(),
            "Updated lead".into(),
            "Updated mate".into(),
        ]
    );
    for view in &views {
        assert_view_is_current(&actor, view);
    }
    let all = ids(&["lead", "mate"]);
    assert_eq!(index_and_row_ids(&actor), (all.clone(), all));
    assert_parent_follows(&mut actor);
}

// ========================================================================
// Late status lines after SessionEnd (#53 survey gap G5)
// ========================================================================

const OLD: &str = "old-session";
const NEW: &str = "new-session";
const OLD_COST: f64 = 0.09;

/// Sends a status line from `pid` reporting `id` and `cost`, so a test
/// can tell whose data a row holds.
fn status_line_costing(actor: &mut RegistryActor, pid: u32, id: &str, cost: f64) {
    send_status_line(
        actor,
        serde_json::json!({
            "session_id": id,
            "pid": pid,
            "model": {"id": "claude-sonnet-4-20250514"},
            "cost": {"total_cost_usd": cost, "total_duration_ms": 1000}
        }),
    );
}

fn session_end(actor: &mut RegistryActor, id: &str, pid: u32, reason: &str) {
    let event = LifecycleEvent::SessionEnd {
        reason: Some(reason.into()),
    };
    apply_event(actor, id, Some(pid), event).unwrap();
}

fn session_start(actor: &mut RegistryActor, id: &str, pid: u32, source: &str) {
    let event = LifecycleEvent::SessionStart {
        source: Some(source.into()),
    };
    apply_event(actor, id, Some(pid), event).unwrap();
}

/// A Claude session `OLD` at `pid` that has had a status line, as
/// before `/clear` or `/exit`, with the setup events drained.
fn old_session_at(actor: &mut RegistryActor, rx: &mut broadcast::Receiver<SessionEvent>) {
    status_line_costing(actor, std::process::id(), OLD, OLD_COST);
    drain_setup(rx);
}

/// Claude Code 2.1.310's `/clear` order: `SessionEnd(old)`, a status
/// line still carrying the old id ~30 ms later, then
/// `SessionStart(new)`. The late line must not bring the old session
/// back, so `SessionStart(new)` creates the new row.
#[test]
fn clear_drops_a_late_old_status_line_that_arrives_before_session_start() {
    let (_, mut actor, mut event_rx) = create_actor();
    let pid = std::process::id();
    old_session_at(&mut actor, &mut event_rx);
    session_end(&mut actor, OLD, pid, "clear");
    drain_setup(&mut event_rx);

    status_line_costing(&mut actor, pid, OLD, OLD_COST);

    assert_eq!(drain_events(&mut event_rx).0, Vec::<String>::new());
    assert_eq!(actor.session_count(), 0);

    session_start(&mut actor, NEW, pid, "clear");
    assert_eq!(index_and_row_ids(&actor), (ids(&[NEW]), ids(&[NEW])));
    assert_eq!(session_view(&actor, NEW).unwrap().cost_usd, 0.0);

    status_line_costing(&mut actor, pid, NEW, 0.01);
    assert_eq!(session_view(&actor, NEW).unwrap().cost_usd, 0.01);
}

/// The other arrival order: `atm-hook` sends each frame from its own
/// process, so `SessionStart(new)` can reach atmd before the late
/// line. The late line must not rename the new row back.
#[test]
fn clear_drops_a_late_old_status_line_that_arrives_after_session_start() {
    let (_, mut actor, mut event_rx) = create_actor();
    let pid = std::process::id();
    old_session_at(&mut actor, &mut event_rx);
    session_end(&mut actor, OLD, pid, "clear");
    session_start(&mut actor, NEW, pid, "clear");
    drain_setup(&mut event_rx);

    status_line_costing(&mut actor, pid, OLD, OLD_COST);

    assert_eq!(drain_events(&mut event_rx).0, Vec::<String>::new());
    assert_eq!(index_and_row_ids(&actor), (ids(&[NEW]), ids(&[NEW])));
    assert_eq!(session_view(&actor, NEW).unwrap().cost_usd, 0.0);
}

/// `/exit`: a status line after `SessionEnd` must not leave a ghost
/// row that lasts until cleanup finds the process dead. The drop logs
/// at DEBUG.
#[test]
fn exit_drops_a_late_status_line() {
    let (_, mut actor, mut event_rx) = create_actor();
    let pid = std::process::id();
    old_session_at(&mut actor, &mut event_rx);
    session_end(&mut actor, OLD, pid, "prompt_input_exit");
    drain_setup(&mut event_rx);

    let logs = capture_logs(|| status_line_costing(&mut actor, pid, OLD, OLD_COST));

    assert_eq!(drain_events(&mut event_rx).0, Vec::<String>::new());
    assert_eq!(actor.session_count(), 0);
    assert_eq!(
        logs,
        format!(
            "DEBUG Status line for a session that ended in this process, ignoring \
             session_id={OLD} pid={pid}\n"
        )
    );
}

/// `claude --resume <id>` brings an ended id back in a new process.
/// Its status lines count at once: only the ended process's are late.
#[test]
fn a_resumed_id_from_a_new_pid_is_accepted_right_after_session_end() {
    let (_, mut actor, mut event_rx) = create_actor();
    old_session_at(&mut actor, &mut event_rx);
    session_end(&mut actor, OLD, std::process::id(), "prompt_input_exit");
    drain_setup(&mut event_rx);

    status_line_costing(&mut actor, UNUSED_PID, OLD, OLD_COST);

    let (events, _) = drain_events(&mut event_rx);
    assert_eq!(
        events,
        [format!("Registered {OLD}"), format!("Updated {OLD}")]
    );
    assert_eq!(pid_of(&actor, OLD), UNUSED_PID);
    assert_eq!(session_view(&actor, OLD).unwrap().cost_usd, OLD_COST);
}

/// Once the marker expires, a status line for the ended pair is
/// applied as on main. The marker is aged instead of sleeping.
#[test]
fn a_status_line_after_the_marker_expires_is_applied() {
    let (_, mut actor, mut event_rx) = create_actor();
    let pid = std::process::id();
    old_session_at(&mut actor, &mut event_rx);
    session_end(&mut actor, OLD, pid, "clear");
    drain_setup(&mut event_rx);
    *actor
        .ended_sessions
        .get_mut(&(SessionId::new(OLD), pid))
        .unwrap() -= ENDED_SESSION_TTL;

    status_line_costing(&mut actor, pid, OLD, OLD_COST);

    let (events, _) = drain_events(&mut event_rx);
    assert_eq!(
        events,
        [format!("Registered {OLD}"), format!("Updated {OLD}")]
    );
    assert_eq!(index_and_row_ids(&actor), (ids(&[OLD]), ids(&[OLD])));
}

/// Each new marker first drops the expired ones, so the map holds only
/// the SessionEnds of the last `ENDED_SESSION_TTL`.
#[test]
fn recording_a_session_end_prunes_expired_markers() {
    let (_, mut actor, _) = create_actor();
    let now = Utc::now();
    let just_live = now - ENDED_SESSION_TTL + chrono::Duration::milliseconds(1);
    actor.record_session_end(SessionId::new("expired"), 1, now - ENDED_SESSION_TTL);
    actor.record_session_end(SessionId::new("just-live"), 2, just_live);

    actor.record_session_end(SessionId::new(OLD), 3, now);

    let mut markers: Vec<String> = (actor.ended_sessions.keys())
        .map(|(id, pid)| format!("{id}@{pid}"))
        .collect();
    markers.sort();
    assert_eq!(markers, ["just-live@2", "old-session@3"]);
}

#[test]
fn a_marker_lives_for_the_ttl_and_a_backwards_clock_expires_it() {
    let ended_at = Utc::now();
    let ms = chrono::Duration::milliseconds;
    assert!(marker_live(ended_at, ended_at));
    assert!(marker_live(ended_at, ended_at + ENDED_SESSION_TTL - ms(1)));
    assert!(!marker_live(ended_at, ended_at + ENDED_SESSION_TTL));
    assert!(!marker_live(ended_at, ended_at - ms(1)));
}

/// An in-process `/resume` back to a session that ended in this pid
/// moments ago starts it again, so its redraw status line counts. The
/// marker of the session it left stays.
#[test]
fn session_start_in_the_ended_process_clears_that_pairs_marker() {
    let (_, mut actor, mut event_rx) = create_actor();
    let pid = std::process::id();
    old_session_at(&mut actor, &mut event_rx);
    session_end(&mut actor, OLD, pid, "clear");
    session_start(&mut actor, NEW, pid, "clear");
    session_end(&mut actor, NEW, pid, "resume");
    session_start(&mut actor, OLD, pid, "resume");
    drain_setup(&mut event_rx);

    status_line_costing(&mut actor, pid, OLD, OLD_COST);

    assert_eq!(session_view(&actor, OLD).unwrap().cost_usd, OLD_COST);
    let markers: Vec<_> = actor.ended_sessions.keys().cloned().collect();
    assert_eq!(markers, [(SessionId::new(NEW), pid)]);
}

/// Ending a session records a marker (its `SessionEnd`, or a new
/// session's `SessionStart` in its pid); cleanup's removal of a dead
/// process doesn't.
#[test]
fn cleanup_of_a_dead_process_records_no_marker() {
    let (_, mut actor, _) = create_actor();
    let sleep = register_sleep(&mut actor, OLD, LifecycleContext::default());
    drop(sleep);

    actor.handle_command(RegistryCommand::CleanupStale);

    assert_eq!(actor.session_count(), 0);
    assert!(actor.ended_sessions.is_empty());
}

// ========================================================================
// SessionStart ends the session its process's row held (#53 goal 4, R1)
// ========================================================================

/// The cost of a late status line, so a test can tell whether it was
/// applied.
const LATE_COST: f64 = 0.5;

/// One of `/clear`'s three frames from one Claude Code 2.1.310 process.
#[derive(Clone, Copy, Debug)]
enum Clear {
    /// `SessionEnd(OLD, reason clear)`.
    End,
    /// The status line still carrying `OLD`, costing [`LATE_COST`].
    Late,
    /// `SessionStart(NEW, source clear)`.
    Start,
}

/// Sends `/clear`'s frames in `order` to an `OLD` row, and returns the
/// index and row ids, then `NEW`'s cost if it has a row.
fn clear_in_order(order: [Clear; 3]) -> ((Vec<String>, Vec<String>), Option<f64>) {
    let (_, mut actor, mut event_rx) = create_actor();
    let pid = std::process::id();
    old_session_at(&mut actor, &mut event_rx);
    for frame in order {
        match frame {
            Clear::End => session_end(&mut actor, OLD, pid, "clear"),
            Clear::Late => status_line_costing(&mut actor, pid, OLD, LATE_COST),
            Clear::Start => session_start(&mut actor, NEW, pid, "clear"),
        }
    }
    let cost = session_view(&actor, NEW).map(|view| view.cost_usd);
    (index_and_row_ids(&actor), cost)
}

/// `atm-hook` sends each frame from its own process, so atmd can get
/// `/clear`'s three frames in any order. Every order leaves what the
/// end-first order does: one fresh row under `NEW`, which no `OLD`
/// status line reaches once `SessionStart` or `SessionEnd` is in. A
/// status line before both hooks is `OLD`'s last, and applies to `OLD`'s
/// row before it ends.
#[test]
fn clear_leaves_one_row_under_the_new_id_in_every_arrival_order() {
    use Clear::{End, Late, Start};
    let orders = [
        [End, Late, Start],
        [End, Start, Late],
        [Start, End, Late],
        [Start, Late, End],
        [Late, End, Start],
        [Late, Start, End],
    ];
    let fresh_new_row = ((ids(&[NEW]), ids(&[NEW])), Some(0.0));

    let outcomes: Vec<_> = (orders.iter())
        .map(|order| (format!("{order:?}"), clear_in_order(*order)))
        .collect();

    let expected: Vec<_> = (orders.iter())
        .map(|order| (format!("{order:?}"), fresh_new_row.clone()))
        .collect();
    assert_eq!(outcomes, expected);
}

/// `/clear` with `start_first` or end first, after `OLD` took a prompt
/// and started an in-process child. Returns `NEW`'s view without its
/// clock fields, the index and row ids, and the child references.
fn cleared_row(start_first: bool) -> (serde_json::Value, (Vec<String>, Vec<String>), Vec<String>) {
    let (_, mut actor, mut event_rx) = create_actor();
    let pid = std::process::id();
    old_session_at(&mut actor, &mut event_rx);
    let prompt = LifecycleEvent::PromptSubmit {
        prompt: Some("fix the bug".into()),
    };
    apply_event(&mut actor, OLD, Some(pid), prompt).unwrap();
    apply_with_context(
        &mut actor,
        &SessionId::new(OLD),
        LifecycleEvent::WorkingStart,
        alias("rev", "agent-1"),
    );
    start_child(&mut actor, &SessionId::new(OLD), "agent-1");
    assert_eq!(actor.session_count(), 2);

    if start_first {
        session_start(&mut actor, NEW, pid, "clear");
        session_end(&mut actor, OLD, pid, "clear");
    } else {
        session_end(&mut actor, OLD, pid, "clear");
        session_start(&mut actor, NEW, pid, "clear");
    }

    let mut view = view_json(&session_view(&actor, NEW).unwrap());
    let fields = view.as_object_mut().unwrap();
    fields.remove("started_at").unwrap();
    fields.remove("last_activity").unwrap();
    (view, index_and_row_ids(&actor), child_refs(&actor))
}

/// Whichever hook comes first, `NEW` gets the same fresh row: `OLD`'s
/// prompt, cost and in-process child end with `OLD`.
#[test]
fn clear_leaves_the_same_row_in_either_order() {
    let end_first = cleared_row(false);
    assert_eq!(end_first.1, (ids(&[NEW]), ids(&[NEW])));
    assert_eq!(end_first.0["first_prompt"], serde_json::Value::Null);

    assert_eq!(cleared_row(true), end_first);
}

/// `SessionStart(NEW)` before `SessionEnd(OLD)` ends `OLD` as its
/// `SessionEnd` would, logging why, then starts `NEW`'s row. The
/// `SessionEnd(OLD)` after it publishes nothing and logs at INFO that
/// the process moved on.
#[test]
fn a_session_start_ends_the_rows_session_and_its_late_end_keeps_the_new_row() {
    let (_, mut actor, mut event_rx) = create_actor();
    let pid = std::process::id();
    old_session_at(&mut actor, &mut event_rx);

    let logs = capture_logs(|| session_start(&mut actor, NEW, pid, "clear"));
    assert_eq!(
        drain_events(&mut event_rx).0,
        [
            format!("Removed {OLD} SessionEnded"),
            format!("Registered {NEW}"),
            format!("Updated {NEW}"),
        ]
    );
    let info: String = (logs.lines())
        .filter(|line| line.starts_with(" INFO "))
        .map(|line| format!("{line}\n"))
        .collect();
    assert_eq!(
        info,
        format!(
            " INFO SessionStart of a new session, ending the session this process's row held \
             session_id={OLD} pid={pid} new_id={NEW}\n{}",
            removed_log(OLD, pid, "session ended", 0)
        )
    );

    let logs = capture_logs(|| session_end(&mut actor, OLD, pid, "clear"));

    assert_eq!(drain_events(&mut event_rx).0, Vec::<String>::new());
    assert_eq!(index_and_row_ids(&actor), (ids(&[NEW]), ids(&[NEW])));
    assert_eq!(
        logs,
        format!(
            " INFO SessionEnd for a session this process moved on from, keeping the row \
             session_id={OLD} pid={pid} current_id={NEW}\n"
        )
    );
}

/// What [`resume_old`] leaves behind.
#[derive(Clone, Debug, PartialEq)]
struct Resumed {
    /// The index and row ids once both hooks are in.
    rows: (Vec<String>, Vec<String>),
    /// `OLD`'s cost once both hooks are in, if it has a row.
    hooks_cost: Option<f64>,
    /// `OLD`'s cost after the redraw, if it has a row.
    cost: Option<f64>,
    /// The ended-session markers left.
    markers: Vec<(SessionId, u32)>,
}

/// Claude Code 2.1.310's in-process `/resume` from `NEW` back to `OLD`
/// (#101): `SessionEnd(NEW, resume)` and `SessionStart(OLD, resume)`,
/// which can reach atmd in either order, then `OLD`'s redraw status
/// line.
fn resume_old(end_first: bool) -> Resumed {
    let (_, mut actor, mut event_rx) = create_actor();
    let pid = std::process::id();
    old_session_at(&mut actor, &mut event_rx);
    session_end(&mut actor, OLD, pid, "clear");
    session_start(&mut actor, NEW, pid, "clear");
    status_line_costing(&mut actor, pid, NEW, 0.01);

    if end_first {
        session_end(&mut actor, NEW, pid, "resume");
        session_start(&mut actor, OLD, pid, "resume");
    } else {
        session_start(&mut actor, OLD, pid, "resume");
        session_end(&mut actor, NEW, pid, "resume");
    }
    let rows = index_and_row_ids(&actor);
    let hooks_cost = session_view(&actor, OLD).map(|view| view.cost_usd);
    status_line_costing(&mut actor, pid, OLD, OLD_COST);

    Resumed {
        rows,
        hooks_cost,
        cost: session_view(&actor, OLD).map(|view| view.cost_usd),
        markers: actor.ended_sessions.keys().cloned().collect(),
    }
}

/// Both orders of `/resume`'s hooks leave one fresh `OLD` row as soon
/// as both are in, the redraw applies to it, and `NEW` stays ended in
/// this process.
#[test]
fn in_process_resume_leaves_one_row_under_the_resumed_id_in_both_orders() {
    let expected = Resumed {
        rows: (ids(&[OLD]), ids(&[OLD])),
        hooks_cost: Some(0.0),
        cost: Some(OLD_COST),
        markers: vec![(SessionId::new(NEW), std::process::id())],
    };
    assert_eq!(
        [
            ("end first", resume_old(true)),
            ("start first", resume_old(false))
        ],
        [("end first", expected.clone()), ("start first", expected)]
    );
}

/// A row discovery registered as `pending-<pid>`, which no event
/// renamed, is still removed by its process's `SessionEnd`.
#[test]
fn session_end_with_a_real_id_removes_a_pending_row() {
    let (_, mut actor, _) = create_actor();
    let dir = tempfile::tempdir().unwrap();
    let pid = std::process::id();
    discover(
        &mut actor,
        SessionId::pending_from_pid(pid),
        pid,
        dir.path(),
        None,
    );

    session_end(&mut actor, OLD, pid, "prompt_input_exit");

    assert_eq!(actor.session_count(), 0);
}

/// pi's in-memory fork: the old pi-atm instance's `session_shutdown`
/// carries the fork's new id (#74, pi 0.73.1), and pi-atm relies on
/// atmd ending the session by pid. No `SessionStart` moved the row
/// off that id, so the row is removed, and the fork's `session_start`
/// gets a fresh row. In the other order the start ends the row's
/// session and starts the fork's row, and the end, which names that
/// row's id, removes it, as on main.
#[test]
fn a_session_end_naming_another_id_still_removes_the_row() {
    let pid = std::process::id();
    let (_, mut actor, mut event_rx) = create_actor();
    old_session_at(&mut actor, &mut event_rx);

    session_end(&mut actor, NEW, pid, "fork");
    assert_eq!(actor.session_count(), 0);
    session_start(&mut actor, NEW, pid, "fork");
    assert_eq!(index_and_row_ids(&actor), (ids(&[NEW]), ids(&[NEW])));
    assert_eq!(session_view(&actor, NEW).unwrap().cost_usd, 0.0);

    let (_, mut actor, mut event_rx) = create_actor();
    old_session_at(&mut actor, &mut event_rx);
    session_start(&mut actor, NEW, pid, "fork");
    session_end(&mut actor, NEW, pid, "fork");
    assert_eq!(actor.session_count(), 0);
}

/// `claude --resume OLD` starts `OLD` in a new process. Its
/// `SessionStart` creates that process's row and leaves the ended
/// process alone. While the first process still holds `OLD`, the id
/// has two rows, as on main.
#[test]
fn session_start_from_a_new_pid_creates_its_own_row() {
    let pid = std::process::id();
    let new_pid = UNUSED_PID;
    let (_, mut actor, mut event_rx) = create_actor();
    old_session_at(&mut actor, &mut event_rx);
    session_end(&mut actor, OLD, pid, "prompt_input_exit");

    session_start(&mut actor, OLD, new_pid, "resume");
    status_line_costing(&mut actor, new_pid, OLD, OLD_COST);

    assert_eq!(index_and_row_ids(&actor), (ids(&[OLD]), ids(&[OLD])));
    assert_eq!(pid_of(&actor, OLD), new_pid);
    assert_eq!(session_view(&actor, OLD).unwrap().cost_usd, OLD_COST);

    let (_, mut actor, mut event_rx) = create_actor();
    old_session_at(&mut actor, &mut event_rx);
    session_start(&mut actor, OLD, new_pid, "resume");
    assert_eq!(index_and_row_ids(&actor), (ids(&[OLD]), ids(&[OLD, OLD])));
}

/// Of two rows sharing the id `dup`, the newer has a child `agent-1`
/// (alias `rev`). Ends the older row with a `SessionEnd(dup)` or, with
/// `start`, a `SessionStart(fresh)`, and returns the index and row
/// ids, the child's parent, the newer row's children and the child
/// references.
fn end_older_shared_row(start: bool) -> (Vec<String>, Vec<String>, String, String, Vec<String>) {
    let (_, mut actor, _) = create_actor();
    let (older, newer) = register_shared_id(&mut actor);
    let dup = SessionId::new("dup");
    let working = LifecycleEvent::WorkingStart;
    apply_with_context(&mut actor, &dup, working, alias("rev", "agent-1"));
    start_child(&mut actor, &dup, "agent-1");

    if start {
        session_start(&mut actor, "fresh", older.0.id(), "clear");
    } else {
        session_end(&mut actor, "dup", older.0.id(), "clear");
    }

    let (index, rows) = index_and_row_ids(&actor);
    let child = &actor.sessions_by_pid[&pid_of(&actor, "agent-1")].0;
    let children = &actor.sessions_by_pid[&newer.0.id()].0.child_session_ids;
    (
        index,
        rows,
        format!("{:?}", child.parent_session_id),
        format!("{children:?}"),
        child_refs(&actor),
    )
}

/// A `SessionStart` on one of two rows that share an id ends that row
/// as its `SessionEnd` does on main, links included, then adds the new
/// session's row.
#[test]
fn session_start_on_a_shared_id_row_ends_it_as_session_end_does() {
    let (mut index, mut rows, parent, children, refs) = end_older_shared_row(false);
    index.push("fresh".into());
    rows.push("fresh".into());

    assert_eq!(
        end_older_shared_row(true),
        (index, rows, parent, children, refs)
    );
}

/// A `SessionStart` with a pending id (older pi-atm sends
/// `pending-<pid>`) never ends a real row. The same rule as
/// rediscovery, which keeps a real id over a pending one.
#[test]
fn session_start_with_a_pending_id_keeps_a_real_row() {
    let (_, mut actor, mut event_rx) = create_actor();
    let pid = std::process::id();
    old_session_at(&mut actor, &mut event_rx);

    let pending = SessionId::pending_from_pid(pid);
    session_start(&mut actor, pending.as_str(), pid, "startup");

    assert_eq!(index_and_row_ids(&actor), (ids(&[OLD]), ids(&[OLD])));
    assert!(actor.ended_sessions.is_empty());
}

/// One process serving several sessions, as codex's shared app-server
/// daemon does: its row follows each new session's `SessionStart`,
/// while other events keep today's pending → real-only rename. A
/// `SessionEnd` from a session the row moved on from keeps the row
/// while that session's marker lives, and removes it, as on main,
/// once the marker expires.
#[test]
fn a_shared_process_row_follows_each_new_session_start() {
    let (_, mut actor, _) = create_actor();
    let pid = std::process::id();
    session_start(&mut actor, "a", pid, "startup");

    session_start(&mut actor, "b", pid, "startup");
    assert_eq!(index_and_row_ids(&actor), (ids(&["b"]), ids(&["b"])));
    apply_event(&mut actor, "a", Some(pid), LifecycleEvent::WorkingStart).unwrap();
    session_start(&mut actor, "b", pid, "compact");
    assert_eq!(index_and_row_ids(&actor), (ids(&["b"]), ids(&["b"])));

    session_end(&mut actor, "a", pid, "other");
    assert_eq!(index_and_row_ids(&actor), (ids(&["b"]), ids(&["b"])));

    *actor
        .ended_sessions
        .get_mut(&(SessionId::new("a"), pid))
        .unwrap() -= ENDED_SESSION_TTL;
    session_end(&mut actor, "a", pid, "other");
    assert_eq!(actor.session_count(), 0);
}

// ========================================================================
// Git lookups off the actor
// ========================================================================

const GIT_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

impl RegistryActor {
    /// Applies results until no git lookup is in flight. Polls with real
    /// sleeps so it works in sync tests and under a paused tokio clock.
    fn finish_git_lookups(&mut self) {
        let deadline = std::time::Instant::now() + GIT_WAIT;
        while self.git_lookups.has_in_flight() {
            match self.git_results.try_recv() {
                Ok((cwd, context)) => self.apply_git_result(cwd, context),
                Err(_) => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "git lookups did not land"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            }
        }
    }
}

/// The context the gated resolver answers for `cwd`.
fn gated_context(cwd: &str) -> GitContext {
    (
        Some(format!("{cwd}-root")),
        Some(cwd.to_string()),
        Some("main".to_string()),
    )
}

/// A test git resolver: each lookup reports its cwd, then blocks until
/// it takes a token from `release` (or `release` is dropped).
struct GitGate {
    calls: std::sync::mpsc::Receiver<String>,
    release: std::sync::mpsc::Sender<()>,
}

impl GitGate {
    /// Lets one blocked lookup land, and applies it.
    fn land_one(&self, actor: &mut RegistryActor, cwd: &str) {
        self.release.send(()).unwrap();
        let deadline = std::time::Instant::now() + GIT_WAIT;
        loop {
            match actor.git_results.try_recv() {
                Ok((got, context)) => {
                    assert_eq!(got, cwd);
                    actor.apply_git_result(got, context);
                    return;
                }
                Err(_) => {
                    assert!(std::time::Instant::now() < deadline, "lookup did not land");
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            }
        }
    }

    fn expect_call(&self, cwd: &str) {
        assert_eq!(self.calls.recv_timeout(GIT_WAIT).unwrap(), cwd);
    }

    /// Lookups started together report in either order.
    fn expect_calls_in_any_order(&self, cwds: &[&str]) {
        let mut calls: Vec<String> = cwds
            .iter()
            .map(|_| self.calls.recv_timeout(GIT_WAIT).unwrap())
            .collect();
        calls.sort();
        assert_eq!(calls, cwds);
    }

    fn assert_no_more_calls(&self) {
        let extra = self
            .calls
            .recv_timeout(std::time::Duration::from_millis(100));
        assert!(extra.is_err(), "unexpected git lookup: {extra:?}");
    }
}

fn create_gated_actor() -> (
    mpsc::Sender<RegistryCommand>,
    RegistryActor,
    broadcast::Receiver<SessionEvent>,
    GitGate,
) {
    let (calls_tx, calls) = std::sync::mpsc::channel();
    let (release, tokens) = std::sync::mpsc::channel::<()>();
    let tokens = std::sync::Mutex::new(tokens);
    let (cmd_tx, cmd_rx) = mpsc::channel(16);
    let (event_tx, event_rx) = broadcast::channel(64);
    let actor = RegistryActor::with_git_resolver(cmd_rx, event_tx, move |cwd: &str| {
        calls_tx.send(cwd.to_string()).unwrap();
        let _ = tokens.lock().unwrap().recv();
        gated_context(cwd)
    });
    (cmd_tx, actor, event_rx, GitGate { calls, release })
}

fn session_in(id: &str, cwd: &str) -> SessionDomain {
    let mut session = create_test_session(id);
    session.working_directory = Some(cwd.to_string());
    session
}

fn assert_no_git_fields(view: &SessionView) {
    assert_eq!(view.project_root, None);
    assert_eq!(view.worktree_path, None);
    assert_eq!(view.worktree_branch, None);
}

fn assert_gated_git_fields(view: &SessionView, cwd: &str) {
    let (project_root, worktree_path, worktree_branch) = gated_context(cwd);
    assert_eq!(view.project_root, project_root);
    assert_eq!(view.worktree_path, worktree_path);
    assert_eq!(view.worktree_branch, worktree_branch);
}

#[test]
fn register_publishes_at_once_and_one_update_when_the_lookup_lands() {
    let (_tx, mut actor, mut event_rx, gate) = create_gated_actor();

    register_session_in(&mut actor, session_in("fresh", "/a"));

    gate.expect_call("/a");
    let (events, _) = drain_events(&mut event_rx);
    assert_eq!(events, ["Registered fresh"]);
    assert_no_git_fields(&session_view(&actor, "fresh").unwrap());

    gate.land_one(&mut actor, "/a");

    let (events, views) = drain_events(&mut event_rx);
    assert_eq!(events, ["Updated fresh"]);
    assert_gated_git_fields(&views[0], "/a");
    assert_view_is_current(&actor, &views[0]);
}

#[test]
fn a_preset_project_root_requests_no_lookup() {
    let (_tx, mut actor, _events, gate) = create_gated_actor();

    let mut preset = session_in("preset", "/a");
    preset.project_root = Some("/preset".to_string());
    register_session_in(&mut actor, preset);

    gate.assert_no_more_calls();
    assert!(!actor.git_lookups.has_in_flight());
}

#[test]
fn a_session_without_a_cwd_requests_no_lookup() {
    let (_tx, mut actor, _events, gate) = create_gated_actor();

    register_session_in(&mut actor, create_test_session("no-cwd"));

    gate.assert_no_more_calls();
    assert!(!actor.git_lookups.has_in_flight());
}

#[test]
fn a_status_line_cwd_change_keeps_the_old_fields_until_the_result_lands() {
    let (_tx, mut actor, mut event_rx, gate) = create_gated_actor();
    register_session_in(&mut actor, session_in("mover", "/a"));
    gate.expect_call("/a");
    gate.land_one(&mut actor, "/a");
    drain_events(&mut event_rx);

    send_status_line(
        &mut actor,
        serde_json::json!({
            "session_id": "mover",
            "cwd": "/b",
            "model": {"id": "claude-sonnet-4-20250514"}
        }),
    );

    gate.expect_call("/b");
    let (events, views) = drain_events(&mut event_rx);
    assert_eq!(events, ["Updated mover"]);
    assert_eq!(views[0].working_directory.as_deref(), Some("/b"));
    assert_gated_git_fields(&views[0], "/a");

    gate.land_one(&mut actor, "/b");

    let (events, views) = drain_events(&mut event_rx);
    assert_eq!(events, ["Updated mover"]);
    assert_gated_git_fields(&views[0], "/b");
}

#[test]
fn a_status_line_keeping_the_cwd_and_root_requests_no_lookup() {
    let (_tx, mut actor, _events, gate) = create_gated_actor();
    register_session_in(&mut actor, session_in("still", "/a"));
    gate.expect_call("/a");
    gate.land_one(&mut actor, "/a");

    send_status_line(
        &mut actor,
        serde_json::json!({
            "session_id": "still",
            "cwd": "/a",
            "model": {"id": "claude-sonnet-4-20250514"}
        }),
    );

    gate.assert_no_more_calls();
}

#[test]
fn a_refresh_with_no_change_publishes_nothing() {
    let (_tx, mut actor, mut event_rx, gate) = create_gated_actor();
    register_session_in(&mut actor, session_in("same", "/a"));
    gate.expect_call("/a");
    gate.land_one(&mut actor, "/a");
    drain_events(&mut event_rx);

    actor.handle_command(RegistryCommand::RefreshGitInfo);
    gate.expect_call("/a");
    gate.land_one(&mut actor, "/a");

    let (events, _) = drain_events(&mut event_rx);
    assert!(events.is_empty(), "unexpected events: {events:?}");
}

#[test]
fn a_refresh_requests_one_lookup_per_distinct_cwd() {
    let (_tx, mut actor, _events, gate) = create_gated_actor();
    register_session_in(&mut actor, session_in("one", "/a"));
    register_session_in(&mut actor, session_in("two", "/a"));
    gate.expect_call("/a");
    gate.assert_no_more_calls();
    gate.land_one(&mut actor, "/a");

    actor.handle_command(RegistryCommand::RefreshGitInfo);

    gate.expect_call("/a");
    gate.assert_no_more_calls();
}

#[test]
fn a_result_updates_every_session_in_its_cwd() {
    let (_tx, mut actor, mut event_rx, gate) = create_gated_actor();
    for id in ["s0", "s1", "s2"] {
        register_session_in(&mut actor, session_in(id, "/a"));
    }
    register_session_in(&mut actor, session_in("elsewhere", "/b"));
    gate.expect_calls_in_any_order(&["/a", "/b"]);
    drain_events(&mut event_rx);

    // Both lookups are blocked on one gate, so they may land in either
    // order; apply each by the cwd it reports.
    gate.release.send(()).unwrap();
    gate.release.send(()).unwrap();
    actor.finish_git_lookups();

    for id in ["s0", "s1", "s2"] {
        assert_gated_git_fields(&session_view(&actor, id).unwrap(), "/a");
    }
    assert_gated_git_fields(&session_view(&actor, "elsewhere").unwrap(), "/b");
    let mut expected: Vec<String> = actor
        .sessions_by_pid
        .values()
        .map(|(session, _)| format!("Updated {}", session.id))
        .collect();
    expected.sort();
    let (mut events, _) = drain_events(&mut event_rx);
    events.sort();
    assert_eq!(events, expected);
}

#[test]
fn a_result_is_published_in_map_order_within_a_cwd() {
    let (_tx, mut actor, mut event_rx, gate) = create_gated_actor();
    for id in ["s0", "s1", "s2", "s3"] {
        register_session_in(&mut actor, session_in(id, "/a"));
    }
    gate.expect_call("/a");
    drain_events(&mut event_rx);

    gate.land_one(&mut actor, "/a");

    let expected: Vec<String> = actor
        .sessions_by_pid
        .values()
        .map(|(session, _)| format!("Updated {}", session.id))
        .collect();
    let (events, _) = drain_events(&mut event_rx);
    assert_eq!(events, expected);
}

#[test]
fn a_result_reaches_a_child_that_copied_a_parent_still_waiting() {
    let (_tx, mut actor, mut event_rx, gate) = create_gated_actor();
    let (parent_tx, mut parent_rx) = oneshot::channel();
    actor.handle_command(RegistryCommand::Register {
        session: Box::new(session_in("lead", "/a")),
        respond_to: parent_tx,
    });
    parent_rx.try_recv().unwrap().unwrap();
    gate.expect_call("/a");

    start_child(&mut actor, &SessionId::new("lead"), "agent-1");
    assert_no_git_fields(&session_view(&actor, "agent-1").unwrap());
    gate.assert_no_more_calls();
    drain_events(&mut event_rx);

    gate.land_one(&mut actor, "/a");

    assert_gated_git_fields(&session_view(&actor, "lead").unwrap(), "/a");
    assert_gated_git_fields(&session_view(&actor, "agent-1").unwrap(), "/a");
    let (mut events, _) = drain_events(&mut event_rx);
    events.sort();
    assert_eq!(events, ["Updated agent-1", "Updated lead"]);
}

#[test]
fn a_stale_result_is_ignored_and_frees_its_slot() {
    let (_tx, mut actor, mut event_rx, gate) = create_gated_actor();
    register_session_in(&mut actor, session_in("mover", "/a"));
    gate.expect_call("/a");
    send_status_line(
        &mut actor,
        serde_json::json!({
            "session_id": "mover",
            "cwd": "/b",
            "model": {"id": "claude-sonnet-4-20250514"}
        }),
    );
    gate.expect_call("/b");
    drain_events(&mut event_rx);

    actor.apply_git_result("/a".to_string(), Some(gated_context("/a")));

    let (events, _) = drain_events(&mut event_rx);
    assert!(events.is_empty(), "unexpected events: {events:?}");
    assert_no_git_fields(&session_view(&actor, "mover").unwrap());
    // "/a"'s slot is free again, so a later session there gets a lookup.
    register_session_in(&mut actor, session_in("back", "/a"));
    gate.expect_call("/a");
}

#[test]
fn rediscovery_requests_a_lookup_for_the_new_cwd() {
    let (_tx, mut actor, mut event_rx, gate) = create_gated_actor();
    let pid = std::process::id();
    let id = SessionId::pending_from_pid(pid);
    discover(
        &mut actor,
        id.clone(),
        pid,
        std::path::Path::new("/a"),
        None,
    );
    gate.expect_call("/a");
    gate.land_one(&mut actor, "/a");
    drain_events(&mut event_rx);

    discover(
        &mut actor,
        SessionId::new("real"),
        pid,
        std::path::Path::new("/b"),
        None,
    );

    gate.expect_call("/b");
    let view = session_view(&actor, "real").unwrap();
    assert_eq!(view.working_directory.as_deref(), Some("/b"));
    assert_gated_git_fields(&view, "/a");
    drain_events(&mut event_rx);

    gate.land_one(&mut actor, "/b");

    let (events, views) = drain_events(&mut event_rx);
    assert_eq!(events, ["Updated real"]);
    assert_gated_git_fields(&views[0], "/b");
}

#[test]
fn a_full_registry_requests_no_lookup_for_a_rejected_session() {
    let (_tx, mut actor, _events, gate) = create_gated_actor();
    for i in 0..MAX_SESSIONS {
        register_session_in(&mut actor, create_test_session(&format!("s{i}")));
    }

    let (tx, mut rx) = oneshot::channel();
    actor.handle_command(RegistryCommand::Register {
        session: Box::new(session_in("over", "/a")),
        respond_to: tx,
    });

    assert!(matches!(
        rx.try_recv().unwrap(),
        Err(RegistryError::RegistryFull { .. })
    ));
    gate.assert_no_more_calls();
}

#[tokio::test]
async fn the_running_actor_serves_commands_while_a_lookup_hangs_then_applies_it() {
    let (cmd_tx, actor, mut event_rx, gate) = create_gated_actor();
    let running = tokio::spawn(actor.run());
    let (tx, rx) = oneshot::channel();
    cmd_tx
        .send(RegistryCommand::Register {
            session: Box::new(session_in("hung", "/a")),
            respond_to: tx,
        })
        .await
        .unwrap();
    rx.await.unwrap().unwrap();

    let (tx, rx) = oneshot::channel();
    cmd_tx
        .send(RegistryCommand::GetSession {
            session_id: SessionId::new("hung"),
            respond_to: tx,
        })
        .await
        .unwrap();
    let view = tokio::time::timeout(GIT_WAIT, rx)
        .await
        .expect("the actor answers while the lookup hangs")
        .unwrap()
        .unwrap();
    assert_no_git_fields(&view);

    gate.release.send(()).unwrap();
    let updated = tokio::time::timeout(GIT_WAIT, async {
        loop {
            if let SessionEvent::Updated { session } = event_rx.recv().await.unwrap() {
                return session;
            }
        }
    })
    .await
    .expect("the actor applies the result");
    assert_gated_git_fields(&updated, "/a");

    drop(cmd_tx);
    tokio::time::timeout(GIT_WAIT, running)
        .await
        .expect("the actor stops")
        .unwrap();
}

#[tokio::test]
async fn run_returns_when_the_senders_drop_while_a_lookup_hangs() {
    let (cmd_tx, actor, _events, gate) = create_gated_actor();
    let running = tokio::spawn(actor.run());
    let (tx, rx) = oneshot::channel();
    cmd_tx
        .send(RegistryCommand::Register {
            session: Box::new(session_in("hung", "/a")),
            respond_to: tx,
        })
        .await
        .unwrap();
    rx.await.unwrap().unwrap();
    gate.expect_call("/a");

    drop(cmd_tx);

    tokio::time::timeout(GIT_WAIT, running)
        .await
        .expect("run() must not wait for the hung lookup")
        .unwrap();
    // `gate` still holds the lookup blocked here.
    drop(gate);
}

/// An actor whose git lookups panic on the given calls, counted from 0, and
/// otherwise answer [`gated_context`] at once.
fn create_actor_with_panicking_lookups(
    panic_on: &'static [usize],
) -> (RegistryActor, broadcast::Receiver<SessionEvent>) {
    let calls = std::sync::atomic::AtomicUsize::new(0);
    let (_, cmd_rx) = mpsc::channel(16);
    let (event_tx, event_rx) = broadcast::channel(16);
    let actor = RegistryActor::with_git_resolver(cmd_rx, event_tx, move |cwd: &str| {
        let call = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        assert!(!panic_on.contains(&call), "lookup {call} fails");
        gated_context(cwd)
    });
    (actor, event_rx)
}

#[test]
fn a_panicking_lookup_frees_its_slot_and_the_next_refresh_fills_the_fields() {
    let (mut actor, mut event_rx) = create_actor_with_panicking_lookups(&[0]);
    register_session_in(&mut actor, session_in("unlucky", "/a"));
    actor.finish_git_lookups();
    assert_no_git_fields(&session_view(&actor, "unlucky").unwrap());
    let (events, _) = drain_events(&mut event_rx);
    assert_eq!(events, ["Registered unlucky"]);

    actor.handle_command(RegistryCommand::RefreshGitInfo);
    actor.finish_git_lookups();

    let (events, views) = drain_events(&mut event_rx);
    assert_eq!(events, ["Updated unlucky"]);
    assert_gated_git_fields(&views[0], "/a");
}

#[test]
fn a_panicking_lookup_leaves_good_fields_alone() {
    let (mut actor, mut event_rx) = create_actor_with_panicking_lookups(&[1]);
    register_session_in(&mut actor, session_in("lucky", "/a"));
    actor.finish_git_lookups();
    drain_events(&mut event_rx);

    actor.handle_command(RegistryCommand::RefreshGitInfo);
    actor.finish_git_lookups();

    let (events, _) = drain_events(&mut event_rx);
    assert!(events.is_empty(), "unexpected events: {events:?}");
    assert_gated_git_fields(&session_view(&actor, "lucky").unwrap(), "/a");
}
