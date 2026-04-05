//! Integration tests for atm-tmux against a real tmux server.
//!
//! These tests are `#[ignore]`d by default because they require tmux to be installed.
//! Run with: `cargo test -p atm-tmux -- --ignored`
//!
//! Each test spins up an isolated tmux server using a unique socket name
//! and cleans it up on completion.

use atm_tmux::{PaneDirection, RealTmuxClient, TmuxClient};
use std::process::Command;

/// Unique socket name for test isolation.
fn test_socket() -> String {
    format!(
        "atm-test-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    )
}

/// Starts an isolated tmux server and returns the socket name.
fn start_tmux_server(socket: &str) -> bool {
    Command::new("tmux")
        .args([
            "-L",
            socket,
            "new-session",
            "-d",
            "-s",
            "test",
            "-x",
            "200",
            "-y",
            "50",
        ])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Kills the test tmux server.
fn kill_tmux_server(socket: &str) {
    let _ = Command::new("tmux")
        .args(["-L", socket, "kill-server"])
        .status();
}

#[tokio::test]
#[ignore]
async fn test_list_panes_real() {
    let socket = test_socket();
    if !start_tmux_server(&socket) {
        eprintln!("tmux not available, skipping");
        return;
    }

    let client = RealTmuxClient::with_socket(&socket);
    let panes = client.list_panes().await;
    kill_tmux_server(&socket);

    let panes = panes.expect("list_panes should succeed");
    assert!(!panes.is_empty(), "should have at least one pane");
    assert_eq!(panes[0].session_name, "test");
}

#[tokio::test]
#[ignore]
async fn test_split_and_kill_real() {
    let socket = test_socket();
    if !start_tmux_server(&socket) {
        eprintln!("tmux not available, skipping");
        return;
    }

    let client = RealTmuxClient::with_socket(&socket);

    // Get the initial pane
    let panes = client.list_panes().await.expect("list_panes");
    let initial_pane = &panes[0].pane_id;

    // Split the pane
    let new_pane = client
        .split_window(initial_pane, "50%", PaneDirection::Below, None)
        .await
        .expect("split_window");
    assert!(new_pane.starts_with('%'), "pane ID should start with %");

    // Verify we now have 2 panes
    let panes = client.list_panes().await.expect("list_panes after split");
    assert_eq!(panes.len(), 2, "should have 2 panes after split");

    // Kill the new pane
    client.kill_pane(&new_pane).await.expect("kill_pane");

    // Verify back to 1 pane
    let panes = client.list_panes().await.expect("list_panes after kill");
    assert_eq!(panes.len(), 1, "should have 1 pane after kill");

    kill_tmux_server(&socket);
}

#[tokio::test]
#[ignore]
async fn test_send_keys_real() {
    let socket = test_socket();
    if !start_tmux_server(&socket) {
        eprintln!("tmux not available, skipping");
        return;
    }

    let client = RealTmuxClient::with_socket(&socket);
    let panes = client.list_panes().await.expect("list_panes");
    let pane = &panes[0].pane_id;

    // send_keys should not error
    let result = client.send_keys(pane, "echo hello").await;
    kill_tmux_server(&socket);

    result.expect("send_keys should succeed");
}

#[tokio::test]
#[ignore]
async fn test_select_pane_real() {
    let socket = test_socket();
    if !start_tmux_server(&socket) {
        eprintln!("tmux not available, skipping");
        return;
    }

    let client = RealTmuxClient::with_socket(&socket);
    let panes = client.list_panes().await.expect("list_panes");
    let pane = &panes[0].pane_id;

    let result = client.select_pane(pane).await;
    kill_tmux_server(&socket);

    result.expect("select_pane should succeed");
}

#[tokio::test]
#[ignore]
async fn test_resize_pane_real() {
    let socket = test_socket();
    if !start_tmux_server(&socket) {
        eprintln!("tmux not available, skipping");
        return;
    }

    let client = RealTmuxClient::with_socket(&socket);

    // Split so we have a resizable pane
    let panes = client.list_panes().await.expect("list_panes");
    let initial = &panes[0].pane_id;
    let new_pane = client
        .split_window(initial, "50%", PaneDirection::Right, None)
        .await
        .expect("split_window");

    let result = client.resize_pane(&new_pane, Some(40), None).await;
    kill_tmux_server(&socket);

    result.expect("resize_pane should succeed");
}

#[tokio::test]
#[ignore]
async fn test_new_window_real() {
    let socket = test_socket();
    if !start_tmux_server(&socket) {
        eprintln!("tmux not available, skipping");
        return;
    }

    let client = RealTmuxClient::with_socket(&socket);

    let new_pane = client.new_window("test", None).await.expect("new_window");
    assert!(new_pane.starts_with('%'));

    // Should have panes in 2 windows now
    let panes = client.list_panes().await.expect("list_panes");
    let windows: std::collections::HashSet<u32> = panes.iter().map(|p| p.window_index).collect();
    assert!(windows.len() >= 2, "should have at least 2 windows");

    kill_tmux_server(&socket);
}

#[tokio::test]
#[ignore]
async fn test_kill_nonexistent_pane() {
    let socket = test_socket();
    if !start_tmux_server(&socket) {
        eprintln!("tmux not available, skipping");
        return;
    }

    let client = RealTmuxClient::with_socket(&socket);
    let result = client.kill_pane("%99999").await;
    kill_tmux_server(&socket);

    assert!(result.is_err(), "killing nonexistent pane should fail");
}
