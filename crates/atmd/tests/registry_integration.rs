//! Integration tests for the Registry Actor.
//!
//! These tests verify the registry works correctly as a complete system,
//! testing the spawn_registry() function and RegistryHandle interface.
//!
//! Per CLAUDE.md: Tests CAN use `.unwrap()` and `.expect()` - this is allowed.
//! We test the panic-free behavior of production code through assertions.

use atm_core::{AgentType, HookEventType, Model, SessionDomain, SessionId};
use atmd::registry::{spawn_registry, RegistryError, RemovalReason, SessionEvent, MAX_SESSIONS};
use std::time::Duration;
use tokio::time::{sleep, timeout};

// ============================================================================
// Test Helpers
// ============================================================================

/// Helper to create a test session with default values.
fn create_test_session(id: &str) -> SessionDomain {
    SessionDomain::new(
        SessionId::new(id),
        AgentType::GeneralPurpose,
        Model::Sonnet4,
    )
}

/// Helper to create a test session with a specific agent type.
fn create_test_session_with_type(id: &str, agent_type: AgentType) -> SessionDomain {
    SessionDomain::new(SessionId::new(id), agent_type, Model::Sonnet4)
}

// ============================================================================
// Basic Lifecycle Tests
// ============================================================================

#[tokio::test]
async fn test_basic_lifecycle() {
    // Spawn registry
    let handle = spawn_registry();

    // Register
    let session = create_test_session("test-session-1");
    handle
        .register(session)
        .await
        .expect("registration should succeed");

    // Query
    let result = handle.get_session(SessionId::new("test-session-1")).await;
    assert!(result.is_some(), "session should be found");

    // Verify fields
    let view = result.unwrap();
    assert_eq!(view.id.as_str(), "test-session-1");
    assert_eq!(view.model, "Sonnet 4");
    assert_eq!(view.agent_type, "main"); // AgentType::GeneralPurpose.short_name() = "main"
    assert_eq!(view.status_label, "idle"); // New sessions start as Idle

    // Handle should still be connected
    assert!(handle.is_connected());
}

#[tokio::test]
async fn test_register_and_remove() {
    let handle = spawn_registry();

    // Register
    let session = create_test_session("remove-test");
    handle.register(session).await.expect("should register");

    // Verify exists
    assert!(handle
        .get_session(SessionId::new("remove-test"))
        .await
        .is_some());

    // Remove
    handle
        .remove(SessionId::new("remove-test"))
        .await
        .expect("should remove");

    // Verify gone
    assert!(handle
        .get_session(SessionId::new("remove-test"))
        .await
        .is_none());
}

#[tokio::test]
async fn test_duplicate_registration_fails() {
    let handle = spawn_registry();

    // First registration succeeds
    let session1 = create_test_session("duplicate-test");
    handle
        .register(session1)
        .await
        .expect("first should succeed");

    // Second registration with same ID fails
    let session2 = create_test_session("duplicate-test");
    let result = handle.register(session2).await;

    assert!(
        matches!(result, Err(RegistryError::SessionAlreadyExists(_))),
        "expected SessionAlreadyExists error, got: {result:?}"
    );
}

// ============================================================================
// Multiple Sessions Tests
// ============================================================================

#[tokio::test]
async fn test_multiple_sessions() {
    let handle = spawn_registry();

    // Register 5+ sessions
    for i in 0..7 {
        let session = create_test_session(&format!("multi-session-{i}"));
        handle
            .register(session)
            .await
            .unwrap_or_else(|_| panic!("session {i} should register"));
    }

    // Query all sessions
    let sessions = handle.get_all_sessions().await;

    // Verify all present
    assert_eq!(sessions.len(), 7, "should have 7 sessions");

    // Verify each session is findable
    for i in 0..7 {
        let id = SessionId::new(format!("multi-session-{i}"));
        let found = handle.get_session(id).await;
        assert!(found.is_some(), "session {i} should be found");
    }
}

#[tokio::test]
async fn test_sessions_with_different_types() {
    let handle = spawn_registry();

    // Register sessions with different agent types
    let types = [
        ("type-general", AgentType::GeneralPurpose),
        ("type-explore", AgentType::Explore),
        ("type-plan", AgentType::Plan),
        ("type-search", AgentType::FileSearch),
        ("type-review", AgentType::CodeReviewer),
    ];

    for (id, agent_type) in types.iter() {
        let session = create_test_session_with_type(id, agent_type.clone());
        handle.register(session).await.expect("should register");
    }

    let sessions = handle.get_all_sessions().await;
    assert_eq!(sessions.len(), 5);

    // Verify types are preserved
    let explore = handle
        .get_session(SessionId::new("type-explore"))
        .await
        .unwrap();
    assert_eq!(explore.agent_type, "explore");
}

// ============================================================================
// Event Subscription Tests
// ============================================================================

#[tokio::test]
async fn test_event_subscription_registered() {
    let handle = spawn_registry();
    let mut rx = handle.subscribe();

    // Register
    let session = create_test_session("event-test");
    handle.register(session).await.unwrap();

    // Should receive Registered event
    let event = timeout(Duration::from_secs(1), rx.recv())
        .await
        .expect("should receive event within timeout")
        .expect("event should be Some");

    match event {
        SessionEvent::Registered {
            session_id,
            agent_type,
        } => {
            assert_eq!(session_id.as_str(), "event-test");
            assert_eq!(agent_type, AgentType::GeneralPurpose);
        }
        _ => panic!("expected Registered event, got {event:?}"),
    }
}

#[tokio::test]
async fn test_event_subscription_removed() {
    let handle = spawn_registry();
    let mut rx = handle.subscribe();

    // Register
    let session = create_test_session("event-remove-test");
    handle.register(session).await.unwrap();

    // Drain registered event
    let _ = timeout(Duration::from_millis(100), rx.recv()).await;

    // Remove
    handle
        .remove(SessionId::new("event-remove-test"))
        .await
        .unwrap();

    // Should receive Removed event
    let event = timeout(Duration::from_secs(1), rx.recv())
        .await
        .expect("should receive event within timeout")
        .expect("event should be Some");

    match event {
        SessionEvent::Removed { session_id, reason } => {
            assert_eq!(session_id.as_str(), "event-remove-test");
            assert!(
                matches!(reason, atmd::registry::RemovalReason::Explicit),
                "expected Explicit removal reason"
            );
        }
        _ => panic!("expected Removed event, got {event:?}"),
    }
}

#[tokio::test]
async fn test_event_subscription_hook_event_update() {
    let handle = spawn_registry();
    let mut rx = handle.subscribe();

    // Register
    let session = create_test_session("hook-event-test");
    handle.register(session).await.unwrap();

    // Drain registered event
    let _ = timeout(Duration::from_millis(100), rx.recv()).await;

    // Apply hook event
    handle
        .apply_hook_event(
            SessionId::new("hook-event-test"),
            HookEventType::PreToolUse,
            Some("Bash".to_string()),
            None, // notification_type
            None, // pid
            None, // tmux_pane
            None, // agent_id
            None, // agent_type
        )
        .await
        .unwrap();

    // Should receive Updated event
    let event = timeout(Duration::from_secs(1), rx.recv())
        .await
        .expect("should receive event within timeout")
        .expect("event should be Some");

    match event {
        SessionEvent::Updated { session } => {
            assert_eq!(session.id.as_str(), "hook-event-test");
            assert_eq!(session.status_label, "working");
            assert_eq!(session.activity_detail, Some("Bash".to_string()));
        }
        _ => panic!("expected Updated event, got {event:?}"),
    }
}

// ============================================================================
// Capacity Limit Tests
// ============================================================================

#[tokio::test]
async fn test_capacity_limit() {
    let handle = spawn_registry();

    // Register MAX_SESSIONS
    for i in 0..MAX_SESSIONS {
        let session = create_test_session(&format!("capacity-session-{i}"));
        let result = handle.register(session).await;
        assert!(
            result.is_ok(),
            "session {i} should register, got {result:?}"
        );
    }

    // Verify count
    let sessions = handle.get_all_sessions().await;
    assert_eq!(sessions.len(), MAX_SESSIONS);

    // Try one more - should fail
    let overflow_session = create_test_session("session-overflow");
    let result = handle.register(overflow_session).await;

    assert!(
        matches!(
            result,
            Err(RegistryError::RegistryFull { max: MAX_SESSIONS })
        ),
        "expected RegistryFull error with max={MAX_SESSIONS}, got {result:?}"
    );
}

#[tokio::test]
async fn test_capacity_after_removal() {
    let handle = spawn_registry();

    // Fill to capacity
    for i in 0..MAX_SESSIONS {
        let session = create_test_session(&format!("cap-remove-{i}"));
        handle.register(session).await.expect("should register");
    }

    // Verify full
    let overflow = create_test_session("should-fail");
    assert!(matches!(
        handle.register(overflow).await,
        Err(RegistryError::RegistryFull { .. })
    ));

    // Remove one
    handle
        .remove(SessionId::new("cap-remove-0"))
        .await
        .expect("should remove");

    // Now can register again
    let new_session = create_test_session("new-after-remove");
    handle
        .register(new_session)
        .await
        .expect("should register after removal");

    // Still at capacity
    assert_eq!(handle.get_all_sessions().await.len(), MAX_SESSIONS);
}

// ============================================================================
// Stale Session Cleanup Tests
// ============================================================================

#[tokio::test]
async fn test_cleanup_fresh_sessions_not_removed() {
    let handle = spawn_registry();

    // Register fresh session
    let session = create_test_session("fresh-session");
    handle.register(session).await.unwrap();

    // Trigger cleanup
    handle.cleanup_stale().await;

    // Small delay to let actor process
    sleep(Duration::from_millis(50)).await;

    // Fresh session should still exist
    assert!(
        handle
            .get_session(SessionId::new("fresh-session"))
            .await
            .is_some(),
        "fresh session should not be removed"
    );
}

// Note: Testing dead-process cleanup requires sessions with real PIDs
// that have terminated. The unit tests in actor.rs provide coverage
// for the cleanup logic using synthetic PIDs.

// ============================================================================
// Concurrent Access Tests
// ============================================================================

#[tokio::test]
async fn test_concurrent_registration() {
    let handle = spawn_registry();

    // Spawn multiple tasks to register sessions concurrently
    let mut handles = Vec::new();
    for i in 0..10 {
        let h = handle.clone();
        let task = tokio::spawn(async move {
            let session = create_test_session(&format!("concurrent-{i}"));
            h.register(session).await
        });
        handles.push(task);
    }

    // Wait for all to complete
    let mut results = Vec::new();
    for task in handles {
        let result = task.await.expect("task should complete");
        results.push(result);
    }

    // All should succeed
    for (i, result) in results.iter().enumerate() {
        assert!(result.is_ok(), "registration {i} failed: {result:?}");
    }

    // Verify all sessions registered
    let sessions = handle.get_all_sessions().await;
    assert_eq!(sessions.len(), 10);
}

#[tokio::test]
async fn test_concurrent_queries() {
    let handle = spawn_registry();

    // Pre-register some sessions
    for i in 0..5 {
        let session = create_test_session(&format!("query-{i}"));
        handle.register(session).await.unwrap();
    }

    // Spawn multiple concurrent queries
    let mut query_handles = Vec::new();
    for _ in 0..20 {
        let h = handle.clone();
        let task = tokio::spawn(async move { h.get_all_sessions().await });
        query_handles.push(task);
    }

    // All queries should return consistent results
    let mut results = Vec::new();
    for task in query_handles {
        let result = task.await.expect("query should complete");
        results.push(result);
    }

    for result in results {
        assert_eq!(result.len(), 5, "each query should see 5 sessions");
    }
}

#[tokio::test]
async fn test_concurrent_mixed_operations() {
    let handle = spawn_registry();

    // Pre-register some sessions
    for i in 0..3 {
        let session = create_test_session(&format!("mixed-{i}"));
        handle.register(session).await.unwrap();
    }

    // Spawn mix of operations concurrently
    let mut tasks = Vec::new();

    // Query tasks
    for _ in 0..5 {
        let h = handle.clone();
        tasks.push(tokio::spawn(async move {
            let _ = h.get_all_sessions().await;
            Ok::<_, RegistryError>(())
        }));
    }

    // Register new sessions
    for i in 0..3 {
        let h = handle.clone();
        let id = format!("new-mixed-{i}");
        tasks.push(tokio::spawn(async move {
            let session = create_test_session(&id);
            h.register(session).await
        }));
    }

    // Get specific session tasks
    for i in 0..3 {
        let h = handle.clone();
        let id = SessionId::new(format!("mixed-{i}"));
        tasks.push(tokio::spawn(async move {
            let _ = h.get_session(id).await;
            Ok::<_, RegistryError>(())
        }));
    }

    // Wait for all
    let mut results = Vec::new();
    for task in tasks {
        let result = task.await.expect("task should complete");
        results.push(result);
    }

    // All should succeed
    for result in results {
        assert!(result.is_ok(), "operation failed: {result:?}");
    }

    // Final state should have 6 sessions (3 original + 3 new)
    let sessions = handle.get_all_sessions().await;
    assert_eq!(sessions.len(), 6);
}

// ============================================================================
// Hook Event Tests
// ============================================================================

#[tokio::test]
async fn test_hook_event_pre_tool_use() {
    let handle = spawn_registry();

    // Register
    let session = create_test_session("hook-pre");
    handle.register(session).await.unwrap();

    // Apply PreToolUse
    handle
        .apply_hook_event(
            SessionId::new("hook-pre"),
            HookEventType::PreToolUse,
            Some("Write".to_string()),
            None, // notification_type
            None, // pid
            None, // tmux_pane
            None, // agent_id
            None, // agent_type
        )
        .await
        .expect("should apply hook event");

    // Verify status changed to working
    let view = handle
        .get_session(SessionId::new("hook-pre"))
        .await
        .unwrap();
    assert_eq!(view.status_label, "working");
    assert_eq!(view.activity_detail, Some("Write".to_string()));
}

#[tokio::test]
async fn test_hook_event_post_tool_use() {
    let handle = spawn_registry();

    // Register
    let session = create_test_session("hook-post");
    handle.register(session).await.unwrap();

    // Apply PreToolUse first
    handle
        .apply_hook_event(
            SessionId::new("hook-post"),
            HookEventType::PreToolUse,
            Some("Bash".to_string()),
            None, // notification_type
            None, // pid
            None, // tmux_pane
            None, // agent_id
            None, // agent_type
        )
        .await
        .unwrap();

    // Then PostToolUse
    handle
        .apply_hook_event(
            SessionId::new("hook-post"),
            HookEventType::PostToolUse,
            Some("Bash".to_string()),
            None, // notification_type
            None, // pid
            None, // tmux_pane
            None, // agent_id
            None, // agent_type
        )
        .await
        .expect("should apply hook event");

    // Verify status changed to working (thinking is now Working in 3-state model)
    let view = handle
        .get_session(SessionId::new("hook-post"))
        .await
        .unwrap();
    assert_eq!(view.status_label, "working");
}

#[tokio::test]
async fn test_hook_event_nonexistent_session() {
    let handle = spawn_registry();

    // Apply hook event to non-existent session
    // This should succeed silently - hook events race with status line updates,
    // so we gracefully ignore hooks for sessions that don't exist yet.
    let result = handle
        .apply_hook_event(
            SessionId::new("nonexistent"),
            HookEventType::PreToolUse,
            Some("Bash".to_string()),
            None, // notification_type
            None, // pid
            None, // tmux_pane
            None, // agent_id
            None, // agent_type
        )
        .await;

    assert!(
        result.is_ok(),
        "hook events for non-existent sessions should be silently ignored"
    );
}

#[tokio::test]
async fn test_hook_event_session_end() {
    let handle = spawn_registry();
    let mut rx = handle.subscribe();

    // Register a session
    let session = create_test_session("session-end-test");
    handle.register(session).await.unwrap();

    // Drain registered event
    let _ = timeout(Duration::from_millis(100), rx.recv()).await;

    // Apply SessionEnd hook
    handle
        .apply_hook_event(
            SessionId::new("session-end-test"),
            HookEventType::SessionEnd,
            None, // tool_name
            None, // notification_type
            None, // pid
            None, // tmux_pane
            None, // agent_id
            None, // agent_type
        )
        .await
        .expect("should apply SessionEnd event");

    // Session should be removed
    assert!(
        handle
            .get_session(SessionId::new("session-end-test"))
            .await
            .is_none(),
        "session should be removed after SessionEnd"
    );

    // Should receive Removed event with SessionEnded reason
    let event = timeout(Duration::from_secs(1), rx.recv())
        .await
        .expect("should receive event within timeout")
        .expect("event should be Some");

    match event {
        SessionEvent::Removed { session_id, reason } => {
            assert_eq!(session_id.as_str(), "session-end-test");
            assert!(
                matches!(reason, RemovalReason::SessionEnded),
                "expected SessionEnded removal reason, got: {reason:?}"
            );
        }
        _ => panic!("expected Removed event, got {event:?}"),
    }
}

#[tokio::test]
async fn test_hook_event_session_end_nonexistent() {
    let handle = spawn_registry();

    // Apply SessionEnd to non-existent session (race condition scenario)
    let result = handle
        .apply_hook_event(
            SessionId::new("nonexistent-for-end"),
            HookEventType::SessionEnd,
            None, // tool_name
            None, // notification_type
            None, // pid
            None, // tmux_pane
            None, // agent_id
            None, // agent_type
        )
        .await;

    // Should succeed silently (not error)
    assert!(
        result.is_ok(),
        "SessionEnd for non-existent session should succeed silently"
    );
}

// ============================================================================
// Error Handling Tests
// ============================================================================

#[tokio::test]
async fn test_remove_nonexistent_session() {
    let handle = spawn_registry();

    let result = handle.remove(SessionId::new("does-not-exist")).await;

    assert!(
        matches!(result, Err(RegistryError::SessionNotFound(_))),
        "expected SessionNotFound error, got {result:?}"
    );
}

#[tokio::test]
async fn test_get_nonexistent_session_returns_none() {
    let handle = spawn_registry();

    let result = handle.get_session(SessionId::new("nonexistent")).await;

    assert!(
        result.is_none(),
        "should return None for nonexistent session"
    );
}

// ============================================================================
// Handle Clone Tests
// ============================================================================

#[tokio::test]
async fn test_handle_cloning() {
    let handle1 = spawn_registry();
    let handle2 = handle1.clone();

    // Register via handle1
    let session = create_test_session("clone-test");
    handle1.register(session).await.unwrap();

    // Query via handle2
    let result = handle2.get_session(SessionId::new("clone-test")).await;
    assert!(
        result.is_some(),
        "cloned handle should see registered session"
    );

    // Both handles should be connected
    assert!(handle1.is_connected());
    assert!(handle2.is_connected());
}

#[tokio::test]
async fn test_multiple_subscribers() {
    let handle = spawn_registry();

    // Create multiple subscribers
    let mut rx1 = handle.subscribe();
    let mut rx2 = handle.subscribe();
    let mut rx3 = handle.subscribe();

    // Register a session
    let session = create_test_session("multi-sub-test");
    handle.register(session).await.unwrap();

    // All subscribers should receive the event
    let event1 = timeout(Duration::from_secs(1), rx1.recv())
        .await
        .expect("rx1 should receive")
        .expect("event1 should be Some");
    let event2 = timeout(Duration::from_secs(1), rx2.recv())
        .await
        .expect("rx2 should receive")
        .expect("event2 should be Some");
    let event3 = timeout(Duration::from_secs(1), rx3.recv())
        .await
        .expect("rx3 should receive")
        .expect("event3 should be Some");

    // All should be Registered events
    assert!(matches!(event1, SessionEvent::Registered { .. }));
    assert!(matches!(event2, SessionEvent::Registered { .. }));
    assert!(matches!(event3, SessionEvent::Registered { .. }));
}

// ============================================================================
// Session View Field Tests
// ============================================================================

#[tokio::test]
async fn test_session_view_fields() {
    let handle = spawn_registry();

    let session = SessionDomain::new(
        SessionId::new("8e11bfb5-7dc2-432b-9206-928fa5c35731"),
        AgentType::Explore,
        Model::Opus45,
    );
    handle.register(session).await.unwrap();

    let view = handle
        .get_session(SessionId::new("8e11bfb5-7dc2-432b-9206-928fa5c35731"))
        .await
        .unwrap();

    // Verify view fields are correctly populated
    assert_eq!(view.id_short, "8e11bfb5");
    assert_eq!(view.agent_type, "explore");
    assert_eq!(view.model, "Opus 4.5");
    assert_eq!(view.status_label, "idle"); // New sessions start as Idle
    assert!(!view.needs_attention);
}

// ============================================================================
// Cleanup Fire-and-Forget Test
// ============================================================================

#[tokio::test]
async fn test_cleanup_dead_processes_is_fire_and_forget() {
    let handle = spawn_registry();

    // cleanup_stale should complete quickly without waiting for result
    let start = std::time::Instant::now();
    handle.cleanup_stale().await;
    let elapsed = start.elapsed();

    // Should be nearly instant (fire-and-forget)
    assert!(
        elapsed < Duration::from_millis(100),
        "cleanup_stale took too long: {elapsed:?}"
    );
}

// ============================================================================
// Status Line Update Tests (via auto-registration)
// ============================================================================

#[tokio::test]
async fn test_update_from_status_line_auto_registers() {
    let handle = spawn_registry();

    // Use the current process PID (a real PID that set_pid can validate)
    let current_pid = std::process::id();

    // Update for non-existent session should auto-register
    // Note: PID is required for auto-registration with PID-as-primary-key design
    let status_json = serde_json::json!({
        "session_id": "auto-register-test",
        "pid": current_pid,
        "model": {"id": "claude-sonnet-4-20250514"},
        "cost": {"total_cost_usd": 0.15, "total_duration_ms": 10000},
        "context_window": {"total_input_tokens": 2500, "context_window_size": 200000}
    });

    let result = handle
        .update_from_status_line(SessionId::new("auto-register-test"), status_json)
        .await;

    assert!(result.is_ok(), "auto-registration should succeed");

    // Session should now exist
    let view = handle
        .get_session(SessionId::new("auto-register-test"))
        .await;
    assert!(view.is_some(), "auto-registered session should be findable");
}

#[tokio::test]
async fn test_update_from_status_line_updates_existing() {
    let handle = spawn_registry();

    // First register normally
    let session = create_test_session("update-test");
    handle.register(session).await.unwrap();

    // Then update via status line
    let status_json = serde_json::json!({
        "session_id": "update-test",
        "model": {"id": "claude-sonnet-4-20250514"},
        "cost": {"total_cost_usd": 0.50, "total_duration_ms": 30000},
        "context_window": {"total_input_tokens": 10000, "context_window_size": 200000}
    });

    let result = handle
        .update_from_status_line(SessionId::new("update-test"), status_json)
        .await;

    assert!(result.is_ok(), "update should succeed");

    // Verify cost was updated
    let view = handle
        .get_session(SessionId::new("update-test"))
        .await
        .unwrap();
    assert!(
        view.cost_usd > 0.4,
        "cost should be updated to ~0.50, got {}",
        view.cost_usd
    );
}

// ============================================================================
// Phase 1: Subagent & Project Field Tests
// ============================================================================

#[tokio::test]
async fn test_hook_event_forwards_agent_fields() {
    let handle = spawn_registry();
    let mut rx = handle.subscribe();

    // Register a parent session
    let session = create_test_session("parent-for-subagent");
    handle.register(session).await.unwrap();

    // Drain registered event
    let _ = timeout(Duration::from_millis(100), rx.recv()).await;

    // Send SubagentStart hook with agent_id and agent_type
    handle
        .apply_hook_event(
            SessionId::new("parent-for-subagent"),
            HookEventType::SubagentStart,
            None,
            None,
            None,
            None,
            Some("subagent-001".to_string()),
            Some("explore".to_string()),
        )
        .await
        .expect("SubagentStart hook should succeed");

    // The parent session should still be accessible and working
    let view = handle
        .get_session(SessionId::new("parent-for-subagent"))
        .await
        .expect("parent session should exist");
    // SubagentStart is a pre-event, so session stays working
    assert_eq!(view.status_label, "working");
}

#[tokio::test]
async fn test_project_resolution_on_discovery() {
    // This test verifies that project resolution functions work correctly
    // when given a real git repo path. The functions are in atm-core and
    // will be wired into the actor in a later phase.
    use atm_core::{resolve_project_root, resolve_worktree_info};
    use std::fs;

    let handle = spawn_registry();

    // Create a temp dir with a .git directory to simulate a git repo
    let dir = tempfile::tempdir().unwrap();
    let repo_path = dir.path().join("myproject");
    fs::create_dir_all(repo_path.join(".git")).unwrap();
    fs::create_dir_all(repo_path.join("src")).unwrap();
    fs::write(repo_path.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();

    let working_dir = repo_path.join("src");
    let cwd_str = working_dir.to_str().unwrap();

    // Verify project resolution works
    let project_root = resolve_project_root(cwd_str);
    assert_eq!(
        project_root,
        Some(repo_path.to_string_lossy().to_string()),
        "should resolve project root from subdirectory"
    );

    // Verify worktree info resolution
    let (wt_path, branch) = resolve_worktree_info(repo_path.to_str().unwrap());
    assert_eq!(wt_path, Some(repo_path.to_str().unwrap().to_string()));
    assert_eq!(branch, Some("main".to_string()));

    // Register a discovered session with this working directory
    let current_pid = std::process::id();
    handle
        .register_discovered(
            SessionId::new("project-test-session"),
            current_pid,
            working_dir.clone(),
            None,
        )
        .await
        .expect("discovery registration should succeed");

    // Verify session was registered with working_directory populated
    let view = handle
        .get_session(SessionId::new("project-test-session"))
        .await
        .expect("session should exist");

    // working_directory should be set from the discovery path
    assert!(
        view.working_directory.is_some(),
        "working_directory should be populated from discovery"
    );
}
