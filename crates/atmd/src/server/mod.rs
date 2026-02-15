//! Unix socket server for the ATM daemon.
//!
//! The server:
//! - Listens on a Unix socket for client connections
//! - Spawns a ConnectionHandler for each client
//! - Manages event subscriptions and broadcasts
//! - Supports graceful shutdown via CancellationToken
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────┐
//! │   DaemonServer  │
//! │                 │
//! │  UnixListener   │
//! └───────┬─────────┘
//!         │ accept()
//!         ▼
//! ┌─────────────────┐     ┌─────────────────┐
//! │ConnectionHandler│────▶│  RegistryHandle │
//! │   (per client)  │     │                 │
//! └─────────────────┘     └─────────────────┘
//!         │
//!         │ broadcast
//!         ▼
//! ┌─────────────────┐
//! │  TUI Clients    │
//! │  (subscribers)  │
//! └─────────────────┘
//! ```
//!
//! # Panic-Free Guarantees
//!
//! This module follows CLAUDE.md panic-free policy:
//! - No `.unwrap()`, `.expect()`, `panic!()`, `unreachable!()`, `todo!()`
//! - All fallible operations use `?`, pattern matching, or `unwrap_or`
//! - Server errors are logged and allow continued operation

mod connection;

pub use connection::{
    ConnectionError, ConnectionHandler, Subscriber, SubscriberWriter, SubscribersMap,
};

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::net::UnixListener;
use tokio::sync::{broadcast, RwLock};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use atm_core::SessionId;
use atm_protocol::DaemonMessage;

use crate::registry::{RegistryHandle, SessionEvent};

/// Default socket path
pub const DEFAULT_SOCKET_PATH: &str = "/tmp/atm.sock";

/// Maximum number of concurrent TUI clients
const MAX_TUI_CLIENTS: usize = 10;

/// Unix socket server for the ATM daemon.
///
/// Manages client connections and event broadcasting.
pub struct DaemonServer {
    /// Path to the Unix socket
    socket_path: PathBuf,

    /// Handle to the session registry
    registry: RegistryHandle,

    /// Cancellation token for graceful shutdown
    cancel_token: CancellationToken,

    /// Connection counter for generating client IDs
    connection_counter: AtomicU64,

    /// Active TUI subscribers (keyed by client_id)
    subscribers: SubscribersMap,
}

impl DaemonServer {
    /// Creates a new daemon server.
    ///
    /// # Arguments
    ///
    /// * `socket_path` - Path where the Unix socket will be created
    /// * `registry` - Handle to the session registry
    /// * `cancel_token` - Token for graceful shutdown
    pub fn new(
        socket_path: impl Into<PathBuf>,
        registry: RegistryHandle,
        cancel_token: CancellationToken,
    ) -> Self {
        Self {
            socket_path: socket_path.into(),
            registry,
            cancel_token,
            connection_counter: AtomicU64::new(0),
            subscribers: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Creates a server with the default socket path.
    pub fn with_default_path(registry: RegistryHandle, cancel_token: CancellationToken) -> Self {
        Self::new(DEFAULT_SOCKET_PATH, registry, cancel_token)
    }

    /// Returns the socket path.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Runs the server.
    ///
    /// Listens for connections until the cancellation token is triggered.
    /// This method does not return until shutdown.
    pub async fn run(&self) -> Result<(), ServerError> {
        // Remove existing socket file if present
        if self.socket_path.exists() {
            std::fs::remove_file(&self.socket_path).map_err(|e| ServerError::SocketSetup {
                path: self.socket_path.clone(),
                error: e.to_string(),
            })?;
        }

        // Create parent directory if needed
        if let Some(parent) = self.socket_path.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent).map_err(|e| ServerError::SocketSetup {
                    path: self.socket_path.clone(),
                    error: e.to_string(),
                })?;
            }
        }

        // Bind to the Unix socket
        let listener =
            UnixListener::bind(&self.socket_path).map_err(|e| ServerError::SocketSetup {
                path: self.socket_path.clone(),
                error: e.to_string(),
            })?;

        info!(
            socket = %self.socket_path.display(),
            "Daemon server listening"
        );

        // Spawn event broadcaster
        self.spawn_event_broadcaster();

        // Accept connections until cancelled
        loop {
            tokio::select! {
                // Check for cancellation
                _ = self.cancel_token.cancelled() => {
                    info!("Server shutdown requested");
                    break;
                }

                // Accept new connection
                result = listener.accept() => {
                    match result {
                        Ok((stream, _addr)) => {
                            let conn_num = self.connection_counter.fetch_add(1, Ordering::Relaxed);
                            self.handle_connection(stream, conn_num);
                        }
                        Err(e) => {
                            error!(error = %e, "Failed to accept connection");
                            // Continue accepting other connections
                        }
                    }
                }
            }
        }

        // Cleanup
        self.cleanup().await;
        Ok(())
    }

    /// Handles a new client connection by spawning a handler task.
    fn handle_connection(&self, stream: tokio::net::UnixStream, connection_number: u64) {
        let (reader, writer) = stream.into_split();
        let registry = self.registry.clone();
        let subscribers = Arc::clone(&self.subscribers);

        tokio::spawn(async move {
            let handler = ConnectionHandler::new(
                reader,
                writer,
                registry,
                Arc::clone(&subscribers),
                connection_number,
            );

            // Run the handler and get the client_id when done
            let client_id = handler.run().await;

            // Remove from subscribers if was subscribed
            if let Some(id) = client_id {
                let mut subs = subscribers.write().await;
                if subs.remove(&id).is_some() {
                    debug!(client_id = %id, "Removed disconnected subscriber");
                }
            }
        });
    }

    /// Spawns the event broadcaster task.
    ///
    /// This task receives events from the registry and broadcasts them
    /// to all subscribed TUI clients.
    fn spawn_event_broadcaster(&self) {
        let mut event_rx = self.registry.subscribe();
        let subscribers = Arc::clone(&self.subscribers);
        let cancel_token = self.cancel_token.clone();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel_token.cancelled() => {
                        debug!("Event broadcaster shutting down");
                        break;
                    }

                    result = event_rx.recv() => {
                        match result {
                            Ok(event) => {
                                broadcast_event(&subscribers, &event).await;
                            }
                            Err(broadcast::error::RecvError::Lagged(n)) => {
                                warn!(skipped = n, "Event broadcaster lagged, skipped events");
                            }
                            Err(broadcast::error::RecvError::Closed) => {
                                debug!("Event channel closed");
                                break;
                            }
                        }
                    }
                }
            }
        });
    }

    /// Adds a subscriber for event broadcasts.
    pub async fn add_subscriber(
        &self,
        client_id: String,
        writer: SubscriberWriter,
        filter: Option<SessionId>,
    ) -> Result<(), ServerError> {
        let mut subs = self.subscribers.write().await;

        if subs.len() >= MAX_TUI_CLIENTS {
            return Err(ServerError::TooManyClients {
                max: MAX_TUI_CLIENTS,
            });
        }

        subs.insert(client_id.clone(), Subscriber { writer, filter });
        debug!(client_id = %client_id, "Added subscriber");

        Ok(())
    }

    /// Removes a subscriber.
    pub async fn remove_subscriber(&self, client_id: &str) {
        let mut subs = self.subscribers.write().await;
        if subs.remove(client_id).is_some() {
            debug!(client_id = %client_id, "Removed subscriber");
        }
    }

    /// Returns the number of active subscribers.
    pub async fn subscriber_count(&self) -> usize {
        self.subscribers.read().await.len()
    }

    /// Performs cleanup on shutdown.
    async fn cleanup(&self) {
        // Clear subscribers
        {
            let mut subs = self.subscribers.write().await;
            subs.clear();
        }

        // Remove socket file
        if self.socket_path.exists() {
            if let Err(e) = std::fs::remove_file(&self.socket_path) {
                warn!(
                    socket = %self.socket_path.display(),
                    error = %e,
                    "Failed to remove socket file"
                );
            }
        }

        info!("Server cleanup complete");
    }
}

/// Broadcasts an event to all subscribed clients.
async fn broadcast_event(subscribers: &SubscribersMap, event: &SessionEvent) {
    // Get session_id from event for filtering
    let session_id = match event {
        SessionEvent::Registered { session_id, .. } => session_id,
        SessionEvent::Updated { session } => &session.id,
        SessionEvent::Removed { session_id, .. } => session_id,
    };

    // Build the message once
    let msg = match event {
        SessionEvent::Registered { .. } => {
            // Don't broadcast registration - wait for first update with data
            return;
        }
        SessionEvent::Updated { session } => DaemonMessage::session_updated((**session).clone()),
        SessionEvent::Removed { session_id, .. } => {
            DaemonMessage::session_removed(session_id.clone())
        }
    };

    let json = match serde_json::to_string(&msg) {
        Ok(j) => j,
        Err(e) => {
            error!(error = %e, "Failed to serialize event");
            return;
        }
    };

    // Send to all matching subscribers
    let subs = subscribers.read().await;
    let mut failed_clients = Vec::new();

    for (client_id, sub) in subs.iter() {
        // Check filter
        if let Some(ref filter) = sub.filter {
            if filter != session_id {
                continue;
            }
        }

        // Try to send
        let mut writer = sub.writer.lock().await;
        let send_result = async {
            use tokio::io::AsyncWriteExt;
            writer.write_all(json.as_bytes()).await?;
            writer.write_all(b"\n").await?;
            writer.flush().await?;
            Ok::<(), std::io::Error>(())
        }
        .await;

        if let Err(e) = send_result {
            debug!(
                client_id = %client_id,
                error = %e,
                "Failed to send event to subscriber"
            );
            failed_clients.push(client_id.clone());
        }
    }

    // Remove failed clients (need to drop read lock first)
    drop(subs);

    if !failed_clients.is_empty() {
        let mut subs = subscribers.write().await;
        for client_id in failed_clients {
            subs.remove(&client_id);
            debug!(client_id = %client_id, "Removed failed subscriber");
        }
    }
}

/// Errors that can occur in server operations.
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("Failed to setup socket at {path}: {error}")]
    SocketSetup { path: PathBuf, error: String },

    #[error("Too many TUI clients (max: {max})")]
    TooManyClients { max: usize },

    #[error("Connection error: {0}")]
    Connection(#[from] ConnectionError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_socket_path() {
        assert_eq!(DEFAULT_SOCKET_PATH, "/tmp/atm.sock");
    }

    #[test]
    fn test_server_error_display() {
        let err = ServerError::SocketSetup {
            path: PathBuf::from("/tmp/test.sock"),
            error: "permission denied".to_string(),
        };
        assert!(err.to_string().contains("/tmp/test.sock"));
        assert!(err.to_string().contains("permission denied"));
    }

    #[test]
    fn test_max_clients_error() {
        let err = ServerError::TooManyClients {
            max: MAX_TUI_CLIENTS,
        };
        assert!(err.to_string().contains(&MAX_TUI_CLIENTS.to_string()));
    }
}
