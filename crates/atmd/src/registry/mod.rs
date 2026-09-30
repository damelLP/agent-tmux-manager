//! Session registry using Actor pattern.
//!
//! The registry is the central state manager for all active Claude Code sessions.
//! It receives commands via a tokio mpsc channel and maintains the canonical
//! source of truth for session data.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────┐     ┌─────────────────┐     ┌─────────────────┐
//! │   Hook Script   │────▶│  RegistryActor  │────▶│ Broadcast Channel│
//! └─────────────────┘     └─────────────────┘     └─────────────────┘
//!         │                       │                       │
//!         │   RegistryCommand     │   SessionEvent        │
//!         │   (mpsc channel)      │   (broadcast)         │
//!         ▼                       ▼                       ▼
//!    Register/Update         HashMap<SessionId,      All TUI clients
//!    sessions               SessionDomain>          receive events
//! ```
//!
//! # Panic-Free Guarantees
//!
//! All operations in this module follow the panic-free policy:
//! - No `.unwrap()` or `.expect()` in production code
//! - All fallible operations return `Result` or `Option`
//! - Channel operations handle closure gracefully

use tokio::sync::{broadcast, mpsc};
use tokio::time::{interval, Duration};
use tracing::debug;

mod actor;
mod commands;
mod handle;

pub use actor::{RegistryActor, MAX_SESSIONS};
pub use commands::{RegistryCommand, RegistryError, RemovalReason, SessionEvent};
pub use handle::RegistryHandle;

/// Channel buffer sizes
const COMMAND_BUFFER: usize = 100;
const EVENT_BUFFER: usize = 100;

/// Cleanup interval in seconds
const CLEANUP_INTERVAL_SECS: u64 = 2;

/// Git info refresh interval in seconds
const GIT_REFRESH_INTERVAL_SECS: u64 = 5;

/// Spawn the registry actor and return a handle for interaction.
///
/// This function:
/// 1. Creates command and event channels
/// 2. Spawns the RegistryActor on a tokio task
/// 3. Spawns a background cleanup task
/// 4. Returns a RegistryHandle for client use
///
/// # Panics
///
/// This function does NOT panic. All operations are safe.
///
/// # Example
///
/// ```no_run
/// use atmd::registry::spawn_registry;
///
/// #[tokio::main]
/// async fn main() {
///     let handle = spawn_registry();
///
///     // Use handle to interact with registry
///     let sessions = handle.get_all_sessions().await;
/// }
/// ```
pub fn spawn_registry() -> RegistryHandle {
    // Create channels
    let (cmd_tx, cmd_rx) = mpsc::channel(COMMAND_BUFFER);
    let (event_tx, _) = broadcast::channel(EVENT_BUFFER);

    // Create and spawn actor
    let actor = RegistryActor::new(cmd_rx, event_tx.clone());
    tokio::spawn(actor.run());

    // Create handle
    let handle = RegistryHandle::new(cmd_tx.clone(), event_tx);

    // Spawn cleanup task
    spawn_cleanup_task(cmd_tx.clone());

    // Spawn git info refresh task (detects branch switches)
    spawn_git_refresh_task(cmd_tx);

    handle
}

/// Spawn a background task that triggers periodic stale session cleanup.
fn spawn_cleanup_task(sender: mpsc::Sender<RegistryCommand>) {
    tokio::spawn(async move {
        let mut ticker = interval(Duration::from_secs(CLEANUP_INTERVAL_SECS));

        loop {
            ticker.tick().await;

            // Fire-and-forget cleanup command
            if sender.send(RegistryCommand::CleanupStale).await.is_err() {
                // Channel closed, actor stopped - exit cleanup task
                debug!("Cleanup task stopping: registry channel closed");
                break;
            }

            debug!("Triggered stale session cleanup");
        }
    });
}

/// Spawn a background task that periodically refreshes git info for all sessions.
///
/// Detects branch switches that happen without a working directory change
/// (e.g., `git checkout other-branch`). The cost is minimal: one stat() + read()
/// of `.git/HEAD` per tracked session every 5 seconds.
fn spawn_git_refresh_task(sender: mpsc::Sender<RegistryCommand>) {
    tokio::spawn(async move {
        let mut ticker = interval(Duration::from_secs(GIT_REFRESH_INTERVAL_SECS));

        loop {
            ticker.tick().await;

            if sender.send(RegistryCommand::RefreshGitInfo).await.is_err() {
                debug!("Git refresh task stopping: registry channel closed");
                break;
            }
        }
    });
}
