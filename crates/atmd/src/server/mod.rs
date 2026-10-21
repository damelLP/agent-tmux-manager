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

use connection::subscriber_key;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::net::UnixListener;
use tokio::sync::RwLock;
use tokio::task::JoinSet;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use atm_core::SessionId;

use crate::registry::RegistryHandle;

/// Default socket path
pub const DEFAULT_SOCKET_PATH: &str = "/tmp/atm.sock";

/// Maximum number of concurrent TUI clients
const MAX_TUI_CLIENTS: usize = 10;

/// How long shutdown waits for connection handlers before aborting them
const CONNECTION_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

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

        // Connection handlers, each cancelled with the server
        let mut connections = JoinSet::new();

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
                            self.handle_connection(&mut connections, stream, conn_num);
                        }
                        Err(e) => {
                            error!(error = %e, "Failed to accept connection");
                            // Continue accepting other connections
                        }
                    }
                }

                // Reap finished handlers so the set doesn't grow
                Some(_) = connections.join_next(), if !connections.is_empty() => {}
            }
        }

        // Cleanup
        self.cleanup().await;
        drain_connections(connections).await;
        Ok(())
    }

    /// Handles a new client connection by spawning a handler task.
    fn handle_connection(
        &self,
        connections: &mut JoinSet<()>,
        stream: tokio::net::UnixStream,
        connection_number: u64,
    ) {
        let (reader, writer) = stream.into_split();
        let registry = self.registry.clone();
        let subscribers = Arc::clone(&self.subscribers);
        let cancel = self.cancel_token.child_token();

        connections.spawn(async move {
            let handler = ConnectionHandler::new(
                reader,
                writer,
                registry,
                Arc::clone(&subscribers),
                connection_number,
                cancel,
            );

            // Run the handler and get the client_id when done
            let client_id = handler.run().await;

            // Remove from subscribers if was subscribed
            let mut subs = subscribers.write().await;
            if subs.remove(&subscriber_key(connection_number)).is_some() {
                debug!(client_id = ?client_id, "Removed disconnected subscriber");
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

/// Waits up to [`CONNECTION_DRAIN_TIMEOUT`] for cancelled connection
/// handlers to finish, then aborts the rest.
async fn drain_connections(mut connections: JoinSet<()>) {
    let drained = timeout(CONNECTION_DRAIN_TIMEOUT, async {
        while connections.join_next().await.is_some() {}
    })
    .await;

    if drained.is_err() {
        warn!(
            remaining = connections.len(),
            "Aborting connection handlers that did not stop in time"
        );
        connections.shutdown().await;
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
