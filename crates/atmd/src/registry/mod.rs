//! Session registry using Actor pattern.
//!
//! The registry is the central state manager for all active coding-agent sessions.
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
use tokio::task::JoinHandle;
use tokio::time::{interval, Duration};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error};

mod actor;
mod commands;
mod handle;
mod infra;

pub use actor::{RegistryActor, MAX_SESSIONS};
pub use commands::{RegistryCommand, RegistryError, RemovalReason, SessionEvent};
pub use handle::RegistryHandle;

/// Channel buffer sizes
const COMMAND_BUFFER: usize = 100;
/// Also how far a subscriber may fall behind before it is disconnected
/// (see `server::connection`). Sized so a briefly busy client survives a
/// burst of 1500 updates.
const EVENT_BUFFER: usize = 2048;

/// Cleanup interval in seconds
const CLEANUP_INTERVAL_SECS: u64 = 2;

/// Git info refresh interval in seconds
const GIT_REFRESH_INTERVAL_SECS: u64 = 5;

/// Background tasks spawned by [`spawn_registry_with_cancel`].
#[derive(Debug)]
pub struct RegistryTasks {
    /// The registry actor.
    pub actor: JoinHandle<()>,
    /// Periodic stale-session cleanup and git refresh.
    pub maintenance: Vec<JoinHandle<()>>,
}

/// The registry actor stopped before shutdown (for example, it panicked).
#[derive(Debug, thiserror::Error)]
#[error("registry actor stopped: {0}")]
pub struct ActorStopped(pub String);

/// Spawn the registry actor and return a handle for interaction.
///
/// The tasks are never cancelled; they run until the runtime shuts down.
/// Use [`spawn_registry_with_cancel`] to stop and join them.
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
    spawn_registry_with_cancel(CancellationToken::new()).0
}

/// Spawn the registry actor and its maintenance tasks, all stopped by `cancel`.
///
/// The maintenance tasks hold command senders, so the actor never sees its
/// channel close on its own; `cancel` is what stops them. Join the returned
/// [`RegistryTasks`] to wait until they have.
#[must_use = "dropping RegistryTasks leaves the actor unsupervised"]
pub fn spawn_registry_with_cancel(cancel: CancellationToken) -> (RegistryHandle, RegistryTasks) {
    // Create channels
    let (cmd_tx, cmd_rx) = mpsc::channel(COMMAND_BUFFER);
    let (event_tx, _) = broadcast::channel(EVENT_BUFFER);

    // Create and spawn actor
    let actor = RegistryActor::new(cmd_rx, event_tx.clone());
    let actor_cancel = cancel.clone();
    let actor = tokio::spawn(async move {
        tokio::select! {
            biased;
            () = actor_cancel.cancelled() => {}
            () = actor.run() => {}
        }
    });

    // Create handle
    let handle = RegistryHandle::new(cmd_tx.clone(), event_tx);

    // Spawn cleanup task
    let cleanup = spawn_cleanup_task(cmd_tx.clone(), cancel.clone());

    // Spawn git info refresh task (detects branch switches)
    let git_refresh = spawn_git_refresh_task(cmd_tx, cancel);

    let tasks = RegistryTasks {
        actor,
        maintenance: vec![cleanup, git_refresh],
    };
    (handle, tasks)
}

/// Waits for the registry actor to end, given the `cancel` token it was
/// spawned with.
///
/// Ending is a failure if the actor panicked, or if it exited before `cancel`
/// fired (every sender was dropped). A failure cancels `cancel`, so anything
/// derived from it shuts down, and returns [`ActorStopped`].
pub async fn supervise_actor(
    actor: JoinHandle<()>,
    cancel: CancellationToken,
) -> Result<(), ActorStopped> {
    let reason = match actor.await {
        Ok(()) if cancel.is_cancelled() => return Ok(()),
        Ok(()) => "exited before shutdown".to_string(),
        Err(e) => e.to_string(),
    };
    error!(reason = %reason, "Registry actor stopped; shutting down");
    cancel.cancel();
    Err(ActorStopped(reason))
}

/// Spawn a background task that triggers periodic stale session cleanup.
fn spawn_cleanup_task(
    sender: mpsc::Sender<RegistryCommand>,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = interval(Duration::from_secs(CLEANUP_INTERVAL_SECS));

        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    debug!("Cleanup task stopping: cancelled");
                    break;
                }
                _ = ticker.tick() => {}
            }

            // Fire-and-forget cleanup command
            if sender.send(RegistryCommand::CleanupStale).await.is_err() {
                // Channel closed, actor stopped - exit cleanup task
                debug!("Cleanup task stopping: registry channel closed");
                break;
            }

            debug!("Triggered stale session cleanup");
        }
    })
}

/// Spawn a background task that periodically refreshes git info for all sessions.
///
/// Detects branch switches that happen without a working directory change
/// (e.g., `git checkout other-branch`). The cost is minimal: one stat() + read()
/// of `.git/HEAD` per tracked session every 5 seconds.
fn spawn_git_refresh_task(
    sender: mpsc::Sender<RegistryCommand>,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = interval(Duration::from_secs(GIT_REFRESH_INTERVAL_SECS));

        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    debug!("Git refresh task stopping: cancelled");
                    break;
                }
                _ = ticker.tick() => {}
            }

            if sender.send(RegistryCommand::RefreshGitInfo).await.is_err() {
                debug!("Git refresh task stopping: registry channel closed");
                break;
            }
        }
    })
}
