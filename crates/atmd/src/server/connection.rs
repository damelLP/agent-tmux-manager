//! Connection handler for individual client connections.
//!
//! Each client connection gets its own `ConnectionHandler` that:
//! - Performs protocol version negotiation
//! - Parses incoming messages
//! - Routes commands to the registry
//! - Sends responses and broadcasts events to subscribers
//!
//! # Panic-Free Guarantees
//!
//! This module follows CLAUDE.md panic-free policy:
//! - No `.unwrap()`, `.expect()`, `panic!()`, `unreachable!()`, `todo!()`
//! - All fallible operations use `?`, pattern matching, or `unwrap_or`
//! - Connection errors are logged and result in graceful disconnect

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{Mutex, RwLock};
use tokio::time::timeout;
use tracing::{debug, error, info, warn};

use atm_core::SessionId;
use atm_protocol::{ClientMessage, DaemonMessage, MessageType, ProtocolVersion, RawHookEvent};

use crate::discovery::{DiscoveryResult, DiscoveryService};
use crate::registry::{RegistryHandle, SessionEvent};

/// Type alias for subscriber writer handle
pub type SubscriberWriter = Arc<Mutex<BufWriter<OwnedWriteHalf>>>;

/// Information about a subscribed client
pub struct Subscriber {
    /// Writer for sending events
    pub writer: SubscriberWriter,

    /// Optional filter for session-specific subscriptions
    pub filter: Option<SessionId>,
}

/// Type alias for the subscribers map
pub type SubscribersMap = Arc<RwLock<HashMap<String, Subscriber>>>;

/// Maximum number of concurrent TUI clients
const MAX_TUI_CLIENTS: usize = 10;

/// Maximum message size (1 MB)
const MAX_MESSAGE_SIZE: usize = 1_048_576;

/// Read timeout for idle connections (5 minutes)
const READ_TIMEOUT: Duration = Duration::from_secs(300);

/// Write timeout (10 seconds)
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// Unique identifier for this connection
type ClientId = String;

/// Connection handler for a single client.
///
/// Manages the lifecycle of a client connection including:
/// - Protocol handshake
/// - Message processing loop
/// - Event subscription (for TUI clients)
/// - Graceful shutdown
pub struct ConnectionHandler {
    /// Buffered reader for incoming messages
    reader: BufReader<OwnedReadHalf>,

    /// Buffered writer for outgoing messages (shared for event broadcast)
    writer: SubscriberWriter,

    /// Handle to the session registry
    registry: RegistryHandle,

    /// Shared subscribers map for event broadcasting
    subscribers: SubscribersMap,

    /// Unique client identifier (assigned after handshake)
    client_id: Option<ClientId>,

    /// Whether this client is subscribed to events
    subscribed: bool,

    /// Session ID filter for subscriptions (None = all sessions)
    subscription_filter: Option<SessionId>,

    /// Counter for generating client IDs
    connection_number: u64,
}

impl ConnectionHandler {
    /// Creates a new connection handler.
    ///
    /// # Arguments
    ///
    /// * `reader` - Read half of the Unix stream
    /// * `writer` - Write half of the Unix stream
    /// * `registry` - Handle to the session registry
    /// * `subscribers` - Shared map of event subscribers
    /// * `connection_number` - Unique number for this connection
    pub fn new(
        reader: OwnedReadHalf,
        writer: OwnedWriteHalf,
        registry: RegistryHandle,
        subscribers: SubscribersMap,
        connection_number: u64,
    ) -> Self {
        Self {
            reader: BufReader::new(reader),
            writer: Arc::new(Mutex::new(BufWriter::new(writer))),
            registry,
            subscribers,
            client_id: None,
            subscribed: false,
            subscription_filter: None,
            connection_number,
        }
    }

    /// Returns a clone of the writer for event broadcasting.
    pub fn writer_handle(&self) -> SubscriberWriter {
        Arc::clone(&self.writer)
    }

    /// Runs the connection handler.
    ///
    /// This is the main entry point - performs handshake then enters
    /// the message processing loop. Returns when the connection closes.
    pub async fn run(mut self) -> Option<ClientId> {
        debug!(connection = self.connection_number, "New client connected");

        // Perform protocol handshake
        match self.handle_handshake().await {
            Ok(()) => {
                info!(
                    client_id = ?self.client_id,
                    "Client handshake completed"
                );
            }
            Err(e) => {
                warn!(
                    connection = self.connection_number,
                    error = %e,
                    "Handshake failed"
                );
                return None;
            }
        }

        let client_id = self.client_id.clone();

        // Enter message processing loop
        if let Err(e) = self.process_messages().await {
            debug!(
                client_id = ?self.client_id,
                error = %e,
                "Connection closed"
            );
        }

        info!(client_id = ?self.client_id, "Client disconnected");
        client_id
    }

    /// Handles the initial protocol handshake.
    ///
    /// Expects a `Connect` message from the client, validates the protocol
    /// version, and responds with `Connected` or `Rejected`.
    async fn handle_handshake(&mut self) -> Result<(), ConnectionError> {
        // Read first message with timeout
        let msg = self.read_message().await?;

        // Check version compatibility using the top-level protocol_version
        let client_version = msg.protocol_version;
        if !client_version.is_compatible_with(&ProtocolVersion::CURRENT) {
            // Version mismatch - reject
            warn!(
                client_version = %client_version,
                server_version = %ProtocolVersion::CURRENT,
                "Protocol version mismatch"
            );

            self.send_message(DaemonMessage::rejected(&format!(
                "Protocol version {} not compatible with server version {}",
                client_version,
                ProtocolVersion::CURRENT
            )))
            .await?;

            return Err(ConnectionError::VersionMismatch {
                client: client_version,
                server: ProtocolVersion::CURRENT,
            });
        }

        match msg.message {
            MessageType::Connect { client_id } => {
                // Generate or use provided client ID
                let assigned_id =
                    client_id.unwrap_or_else(|| format!("client-{}", self.connection_number));

                self.client_id = Some(assigned_id.clone());

                // Send success response
                self.send_message(DaemonMessage::connected(assigned_id))
                    .await?;

                Ok(())
            }
            other => {
                // Wrong message type for handshake
                self.send_message(DaemonMessage::error(
                    "Expected Connect message for handshake",
                ))
                .await?;

                Err(ConnectionError::UnexpectedMessage(format!("{other:?}")))
            }
        }
    }

    /// Main message processing loop.
    ///
    /// Reads and processes messages until the connection closes or an
    /// unrecoverable error occurs.
    async fn process_messages(&mut self) -> Result<(), ConnectionError> {
        loop {
            // Read with timeout for idle connections
            let msg = match timeout(READ_TIMEOUT, self.read_message()).await {
                Ok(Ok(msg)) => msg,
                Ok(Err(ConnectionError::Eof)) => {
                    debug!(client_id = ?self.client_id, "Client sent EOF");
                    return Ok(());
                }
                Ok(Err(e)) => return Err(e),
                Err(_) => {
                    debug!(client_id = ?self.client_id, "Connection timed out");
                    return Err(ConnectionError::Timeout);
                }
            };

            // Process the message
            if let Err(e) = self.handle_message(msg).await {
                error!(
                    client_id = ?self.client_id,
                    error = %e,
                    "Error handling message"
                );

                // Send error response but continue processing
                let _ = self
                    .send_message(DaemonMessage::error(&e.to_string()))
                    .await;
            }
        }
    }

    /// Handles a single client message.
    async fn handle_message(&mut self, msg: ClientMessage) -> Result<(), ConnectionError> {
        match msg.message {
            MessageType::Connect { .. } => {
                // Already connected - send error
                self.send_message(DaemonMessage::error("Already connected"))
                    .await?;
            }

            MessageType::StatusUpdate { data } => {
                self.handle_status_update(data).await?;
            }

            MessageType::HookEvent { data } => {
                self.handle_hook_event(data).await?;
            }

            MessageType::ListSessions => {
                let sessions = self.registry.get_all_sessions().await;
                self.send_message(DaemonMessage::session_list(sessions))
                    .await?;
            }

            MessageType::Subscribe { session_id } => {
                // Get client_id - must be connected first
                let client_id = match &self.client_id {
                    Some(id) => id.clone(),
                    None => {
                        self.send_message(DaemonMessage::error("Must connect before subscribing"))
                            .await?;
                        return Ok(());
                    }
                };

                // Add to subscribers map
                {
                    let mut subs = self.subscribers.write().await;

                    // Check max clients limit
                    if subs.len() >= MAX_TUI_CLIENTS && !subs.contains_key(&client_id) {
                        self.send_message(DaemonMessage::error(&format!(
                            "Too many subscribers (max: {MAX_TUI_CLIENTS})"
                        )))
                        .await?;
                        return Ok(());
                    }

                    // Add or update subscription
                    subs.insert(
                        client_id.clone(),
                        Subscriber {
                            writer: Arc::clone(&self.writer),
                            filter: session_id.clone(),
                        },
                    );
                }

                self.subscribed = true;
                self.subscription_filter = session_id;

                debug!(
                    client_id = %client_id,
                    filter = ?self.subscription_filter,
                    "Client subscribed to updates"
                );

                // Send current session list as initial state
                let sessions = self.registry.get_all_sessions().await;
                self.send_message(DaemonMessage::session_list(sessions))
                    .await?;
            }

            MessageType::Unsubscribe => {
                // Remove from subscribers map
                if let Some(ref client_id) = self.client_id {
                    let mut subs = self.subscribers.write().await;
                    subs.remove(client_id);
                }

                self.subscribed = false;
                self.subscription_filter = None;

                debug!(
                    client_id = ?self.client_id,
                    "Client unsubscribed from updates"
                );
            }

            MessageType::Ping { seq } => {
                self.send_message(DaemonMessage::pong(seq)).await?;
            }

            MessageType::Discover => {
                debug!(client_id = ?self.client_id, "Client requested discovery");
                let result = self.handle_discover().await;
                self.send_message(DaemonMessage::discovery_complete(
                    result.discovered,
                    result.failed,
                ))
                .await?;
            }

            MessageType::Disconnect => {
                debug!(client_id = ?self.client_id, "Client requested disconnect");
                return Err(ConnectionError::Eof);
            }
        }

        Ok(())
    }

    /// Handles a status update from Claude Code.
    async fn handle_status_update(
        &mut self,
        data: serde_json::Value,
    ) -> Result<(), ConnectionError> {
        // Extract session_id from the data
        let session_id = data
            .get("session_id")
            .and_then(|v| v.as_str())
            .map(SessionId::new)
            .ok_or_else(|| ConnectionError::ParseError("Missing session_id".to_string()))?;

        // Send to registry for processing
        self.registry
            .update_from_status_line(session_id, data)
            .await
            .map_err(|e| ConnectionError::RegistryError(e.to_string()))?;

        Ok(())
    }

    /// Handles a hook event from Claude Code.
    async fn handle_hook_event(&mut self, data: serde_json::Value) -> Result<(), ConnectionError> {
        info!(client_id = ?self.client_id, "Received hook event data");

        // Parse the hook event
        let raw_event: RawHookEvent =
            serde_json::from_value(data).map_err(|e| ConnectionError::ParseError(e.to_string()))?;

        info!(
            session_id = %raw_event.session_id(),
            event_type = ?raw_event.event_type(),
            pid = ?raw_event.pid,
            tmux_pane = ?raw_event.tmux_pane,
            "Processing hook event"
        );

        // Get event type
        let event_type = raw_event.event_type().ok_or_else(|| {
            ConnectionError::ParseError(format!(
                "Unknown hook event type: '{}' (session_id={}, tool_name={:?})",
                raw_event.hook_event_name, raw_event.session_id, raw_event.tool_name
            ))
        })?;

        // Apply to registry (including PID and tmux_pane for process lifecycle tracking)
        self.registry
            .apply_hook_event(
                raw_event.session_id(),
                event_type,
                raw_event.tool_name,
                raw_event.notification_type,
                raw_event.pid,
                raw_event.tmux_pane,
            )
            .await
            .map_err(|e| ConnectionError::RegistryError(e.to_string()))?;

        Ok(())
    }

    /// Handles a discovery request from a TUI client.
    async fn handle_discover(&mut self) -> DiscoveryResult {
        info!(client_id = ?self.client_id, "Processing discovery request");

        let discovery = DiscoveryService::new(self.registry.clone());
        discovery.discover().await
    }

    /// Reads a single message from the client.
    async fn read_message(&mut self) -> Result<ClientMessage, ConnectionError> {
        let mut line = String::new();

        let bytes_read = self
            .reader
            .read_line(&mut line)
            .await
            .map_err(|e| ConnectionError::Io(e.to_string()))?;

        if bytes_read == 0 {
            return Err(ConnectionError::Eof);
        }

        if line.len() > MAX_MESSAGE_SIZE {
            return Err(ConnectionError::MessageTooLarge {
                size: line.len(),
                max: MAX_MESSAGE_SIZE,
            });
        }

        let msg: ClientMessage =
            serde_json::from_str(&line).map_err(|e| ConnectionError::ParseError(e.to_string()))?;

        debug!(
            client_id = ?self.client_id,
            message_type = ?std::mem::discriminant(&msg.message),
            "Received message"
        );

        Ok(msg)
    }

    /// Sends a message to the client.
    async fn send_message(&self, msg: DaemonMessage) -> Result<(), ConnectionError> {
        let json =
            serde_json::to_string(&msg).map_err(|e| ConnectionError::ParseError(e.to_string()))?;

        let mut writer = self.writer.lock().await;

        match timeout(WRITE_TIMEOUT, async {
            writer.write_all(json.as_bytes()).await?;
            writer.write_all(b"\n").await?;
            writer.flush().await?;
            Ok::<(), std::io::Error>(())
        })
        .await
        {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(ConnectionError::Io(e.to_string())),
            Err(_) => Err(ConnectionError::WriteTimeout),
        }
    }

    /// Checks if this client is subscribed to events.
    pub fn is_subscribed(&self) -> bool {
        self.subscribed
    }

    /// Checks if an event should be sent to this client based on filter.
    pub fn should_receive_event(&self, session_id: &SessionId) -> bool {
        if !self.subscribed {
            return false;
        }

        match &self.subscription_filter {
            Some(filter) => filter == session_id,
            None => true, // No filter = receive all
        }
    }

    /// Returns the client ID (if connected).
    pub fn client_id(&self) -> Option<&str> {
        self.client_id.as_deref()
    }
}

/// Errors that can occur during connection handling.
#[derive(Debug, thiserror::Error)]
pub enum ConnectionError {
    #[error("Protocol version mismatch: client {client}, server {server}")]
    VersionMismatch {
        client: ProtocolVersion,
        server: ProtocolVersion,
    },

    #[error("Unexpected message: {0}")]
    UnexpectedMessage(String),

    #[error("Parse error: {0}")]
    ParseError(String),

    #[error("I/O error: {0}")]
    Io(String),

    #[error("Connection closed")]
    Eof,

    #[error("Read timeout")]
    Timeout,

    #[error("Write timeout")]
    WriteTimeout,

    #[error("Message too large: {size} bytes (max: {max})")]
    MessageTooLarge { size: usize, max: usize },

    #[error("Registry error: {0}")]
    RegistryError(String),
}

/// Sends an event to a subscribed client.
///
/// This is used by the server to broadcast events to all subscribers.
#[allow(dead_code)]
pub async fn send_event(
    writer: &Arc<Mutex<BufWriter<OwnedWriteHalf>>>,
    event: &SessionEvent,
) -> Result<(), ConnectionError> {
    let msg = match event {
        SessionEvent::Registered { .. } => {
            // For registered events, we'll let the next status update populate the full data
            // Just acknowledge registration happened
            return Ok(());
        }
        SessionEvent::Updated { session } => DaemonMessage::session_updated((**session).clone()),
        SessionEvent::Removed { session_id, .. } => {
            DaemonMessage::session_removed(session_id.clone())
        }
    };

    let json =
        serde_json::to_string(&msg).map_err(|e| ConnectionError::ParseError(e.to_string()))?;

    let mut writer = writer.lock().await;

    match timeout(WRITE_TIMEOUT, async {
        writer.write_all(json.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
        Ok::<(), std::io::Error>(())
    })
    .await
    {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(ConnectionError::Io(e.to_string())),
        Err(_) => Err(ConnectionError::WriteTimeout),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_connection_error_display() {
        let err = ConnectionError::VersionMismatch {
            client: ProtocolVersion::new(2, 0),
            server: ProtocolVersion::new(1, 0),
        };
        assert!(err.to_string().contains("2.0"));
        assert!(err.to_string().contains("1.0"));
    }

    #[test]
    fn test_message_size_error() {
        let err = ConnectionError::MessageTooLarge {
            size: 2_000_000,
            max: MAX_MESSAGE_SIZE,
        };
        assert!(err.to_string().contains("2000000"));
    }
}
