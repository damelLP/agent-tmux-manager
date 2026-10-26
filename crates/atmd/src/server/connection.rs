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

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{broadcast, Mutex, RwLock};
use tokio::task::JoinSet;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use atm_claude_adapter::{RawHookEvent, RawStatusLine};
use atm_codex_adapter::RawCodexEvent;
use atm_core::{SessionId, StatusLineData};
use atm_pi_adapter::RawPiEvent;
use atm_protocol::{ClientMessage, DaemonMessage, MessageType, ProtocolVersion};

use super::MAX_TUI_CLIENTS;
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

/// Type alias for the subscribers map, keyed by [`subscriber_key`]
pub type SubscribersMap = Arc<RwLock<HashMap<String, Subscriber>>>;

/// Key of a connection's entry in the [`SubscribersMap`].
///
/// Per connection rather than per client id: clients choose their ids, so
/// ids can repeat, and every subscribed connection runs its own forwarder
/// and must count against the subscriber cap.
pub(super) fn subscriber_key(connection_number: u64) -> String {
    connection_number.to_string()
}

/// Maximum message size (1 MB)
const MAX_MESSAGE_SIZE: usize = 1_048_576;

/// Read timeout for idle, unsubscribed connections (5 minutes)
const READ_TIMEOUT: Duration = Duration::from_secs(300);

/// Write timeout (10 seconds)
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// Deadline for a new client's `Connect` message
const HANDSHAKE_TIMEOUT: Duration = WRITE_TIMEOUT;

/// Token usage is optional metadata and must never delay hook handling.
const CODEX_TRANSCRIPT_READ_TIMEOUT: Duration = Duration::from_millis(100);

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

    /// Cancelled on server shutdown, or by the forwarder when this client
    /// can't keep up with events
    cancel: CancellationToken,

    /// Forwards registry events while subscribed
    forwarder: Option<Forwarder>,
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
    /// * `cancel` - Closes the connection when cancelled
    pub fn new(
        reader: OwnedReadHalf,
        writer: OwnedWriteHalf,
        registry: RegistryHandle,
        subscribers: SubscribersMap,
        connection_number: u64,
        cancel: CancellationToken,
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
            cancel,
            forwarder: None,
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
                debug!(
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

        debug!(client_id = ?self.client_id, "Client disconnected");
        client_id
    }

    /// Handles the initial protocol handshake.
    ///
    /// Expects a `Connect` message from the client, validates the protocol
    /// version, and responds with `Connected` or `Rejected`.
    async fn handle_handshake(&mut self) -> Result<(), ConnectionError> {
        // Read first message with timeout
        let msg = timeout(HANDSHAKE_TIMEOUT, self.read_message())
            .await
            .map_err(|_| ConnectionError::Timeout)??;

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
        let cancel = self.cancel.clone();
        loop {
            // Read with timeout for idle connections, until cancelled.
            // read_message isn't cancel-safe (its partial line is a local),
            // but cancelling closes the connection, so a lost partial line
            // doesn't matter.
            // A subscribed client may stay silent, so it gets no idle
            // timeout (Duration::MAX never fires): a dead peer still gives
            // EOF, and the forwarder's WRITE_TIMEOUT cancels a stopped one
            // once writes to it back up.
            let idle = if self.subscribed {
                Duration::MAX
            } else {
                READ_TIMEOUT
            };
            let read = tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    debug!(client_id = ?self.client_id, "Connection cancelled");
                    return Ok(());
                }
                read = timeout(idle, self.read_message()) => read,
            };
            let msg = match read {
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
                // Disconnect: the client is done, which is no error
                if matches!(e, ConnectionError::Eof) {
                    return Ok(());
                }
                error!(
                    client_id = ?self.client_id,
                    error = %e,
                    "Error handling message"
                );

                // A failed reply may have left half a line on the socket
                if matches!(e, ConnectionError::WriteTimeout | ConnectionError::Io(_)) {
                    return Err(e);
                }

                // Send error response but continue processing, unless the
                // reply itself failed
                self.send_message(DaemonMessage::error(&e.to_string()))
                    .await?;
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

            MessageType::PiEvent { data } => {
                self.handle_pi_event(data).await?;
            }

            MessageType::CodexEvent { data } => {
                self.handle_codex_event(data).await?;
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
                    let key = subscriber_key(self.connection_number);
                    let mut subs = self.subscribers.write().await;

                    // Check max clients limit
                    if subs.len() >= MAX_TUI_CLIENTS && !subs.contains_key(&key) {
                        // Don't hold the lock while writing to this client
                        drop(subs);
                        self.send_message(DaemonMessage::error(&format!(
                            "Too many subscribers (max: {MAX_TUI_CLIENTS})"
                        )))
                        .await?;
                        return Ok(());
                    }

                    // Add or update subscription
                    subs.insert(
                        key,
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

                // Replace any previous forwarder. Subscribe to events before
                // taking the snapshot so none fall between the two.
                self.stop_forwarder().await;
                let events = self.registry.subscribe();

                // Send current session list as initial state
                let sessions = self.registry.get_all_sessions().await;
                self.send_message(DaemonMessage::session_list(sessions))
                    .await?;

                self.forwarder = Some(Forwarder::spawn(
                    client_id,
                    events,
                    Arc::clone(&self.writer),
                    self.subscription_filter.clone(),
                    &self.cancel,
                ));
            }

            MessageType::Unsubscribe => {
                self.stop_forwarder().await;

                // Remove from subscribers map
                self.subscribers
                    .write()
                    .await
                    .remove(&subscriber_key(self.connection_number));

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
    ///
    /// Parses the Claude status line at this boundary, as
    /// [`Self::handle_hook_event`] does for hook events, so the registry
    /// below sees only `StatusLineData`.
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
        let (pid, status) = parse_status_line(data)?;

        // Send to registry for processing
        self.registry
            .update_from_status_line(session_id, pid, status, atm_core::Harness::ClaudeCode)
            .await
            .map_err(|e| ConnectionError::RegistryError(e.to_string()))?;

        Ok(())
    }

    /// Handles a hook event from Claude Code.
    ///
    /// Translates the Claude raw event into a vendor-neutral
    /// `LifecycleEvent` at this boundary, so the registry below sees
    /// only the abstract event vocabulary.
    async fn handle_hook_event(&mut self, data: serde_json::Value) -> Result<(), ConnectionError> {
        debug!(client_id = ?self.client_id, "Received hook event data");

        let raw_event: RawHookEvent =
            serde_json::from_value(data).map_err(|e| ConnectionError::ParseError(e.to_string()))?;

        debug!(
            session_id = %raw_event.session_id(),
            event_type = ?raw_event.event_type(),
            pid = ?raw_event.pid,
            tmux_pane = ?raw_event.tmux_pane,
            "Processing hook event"
        );

        // Symmetric with `handle_pi_event`'s suppression-skip below:
        // `to_lifecycle_event` returns `None` for two reasons that
        // shouldn't crash the connection layer — unknown
        // `hook_event_name` (a future Claude event we don't translate
        // yet) and known-but-malformed payloads (e.g. `PreToolUse`
        // without a `tool_name`, dropped by the empty-tool_name
        // guard). Log either case at debug and move on.
        let lifecycle = match raw_event.to_lifecycle_event() {
            Some(le) => le,
            None => {
                debug!(
                    hook_event_name = %raw_event.hook_event_name,
                    event_type = ?raw_event.event_type(),
                    tool_name = ?raw_event.tool_name,
                    "hook event suppressed by adapter"
                );
                return Ok(());
            }
        };

        let session_id = raw_event.session_id();
        let pid = raw_event.pid;
        let tmux_pane = raw_event.tmux_pane.clone();

        self.registry
            .apply_lifecycle_event_with_context(
                session_id,
                lifecycle,
                atm_core::Harness::ClaudeCode,
                pid,
                tmux_pane,
                raw_event.lifecycle_context(),
            )
            .await
            .map_err(|e| ConnectionError::RegistryError(e.to_string()))?;

        Ok(())
    }

    /// Handles a pi event from the pi extension.
    ///
    /// Symmetric with [`Self::handle_hook_event`] — parses raw pi-shaped
    /// JSON via `atm-pi-adapter`, translates into a vendor-neutral
    /// `LifecycleEvent`, and forwards to the registry.
    async fn handle_pi_event(&mut self, data: serde_json::Value) -> Result<(), ConnectionError> {
        debug!(client_id = ?self.client_id, "Received pi event data");

        let raw_event: RawPiEvent =
            serde_json::from_value(data).map_err(|e| ConnectionError::ParseError(e.to_string()))?;

        debug!(
            session_id = ?raw_event.session_id,
            event = %raw_event.event,
            pid = ?raw_event.pid,
            tmux_pane = ?raw_event.tmux_pane,
            "Processing pi event"
        );

        // Pi adapter intentionally returns None for events it suppresses
        // (e.g. tool_execution_start/tool_result paired duplicates) and
        // for high-frequency internal events. Skip those silently.
        let lifecycle = match raw_event.to_lifecycle_event() {
            Some(le) => le,
            None => {
                debug!(event = %raw_event.event, "pi event suppressed by adapter");
                return Ok(());
            }
        };

        // Pi events identify their session via the extension-injected
        // `session_id` field. If absent (older pi-atm read a property pi
        // doesn't have, so it never sent one), derive a pending id from
        // the pid so two pi processes don't collide on a shared sentinel id —
        // matches Claude's `pending_from_pid` discovery pattern.
        // Reject events that have neither: nothing to attribute them to.
        let session_id = match raw_event.session_id.as_deref() {
            Some(s) => atm_core::SessionId::new(s),
            None => raw_event
                .pid
                .map(atm_core::SessionId::pending_from_pid)
                .ok_or_else(|| {
                    ConnectionError::ParseError(
                        "pi event missing both session_id and pid; cannot attribute".to_string(),
                    )
                })?,
        };

        self.registry
            .apply_lifecycle_event(
                session_id,
                lifecycle,
                atm_core::Harness::Pi,
                raw_event.pid,
                raw_event.tmux_pane,
            )
            .await
            .map_err(|e| ConnectionError::RegistryError(e.to_string()))?;

        Ok(())
    }

    /// Handles a hook event from the Codex CLI.
    ///
    /// Symmetric with [`Self::handle_hook_event`] (Claude) and
    /// [`Self::handle_pi_event`] — parses raw codex-shaped JSON via
    /// `atm-codex-adapter`, translates into a vendor-neutral
    /// `LifecycleEvent`, and forwards to the registry. Codex always
    /// sends a `session_id` (like Claude), so no
    /// pending-from-pid fallback is needed here.
    async fn handle_codex_event(&mut self, data: serde_json::Value) -> Result<(), ConnectionError> {
        debug!(client_id = ?self.client_id, "Received codex event data");

        let raw_event: RawCodexEvent =
            serde_json::from_value(data).map_err(|e| ConnectionError::ParseError(e.to_string()))?;

        debug!(
            session_id = %raw_event.session_id(),
            event_type = ?raw_event.event_type(),
            pid = ?raw_event.pid,
            tmux_pane = ?raw_event.tmux_pane,
            "Processing codex event"
        );

        // Same suppression contract as the Claude/pi handlers:
        // `to_lifecycle_event` returns `None` for unknown event names
        // (a future Codex event we don't translate yet) and for
        // known-but-malformed payloads (PreToolUse/PostToolUse without
        // a tool_name). Log at debug and move on.
        let lifecycle = match raw_event.to_lifecycle_event() {
            Some(le) => le,
            None => {
                debug!(
                    hook_event_name = %raw_event.hook_event_name,
                    event_type = ?raw_event.event_type(),
                    tool_name = ?raw_event.tool_name,
                    "codex event suppressed by adapter"
                );
                return Ok(());
            }
        };

        let session_id = raw_event.session_id();
        let pid = raw_event.pid;
        let tmux_pane = raw_event.tmux_pane.clone();
        let model = raw_event.model.clone();
        let transcript_path = raw_event.transcript_path.clone();
        let ends_session = matches!(lifecycle, atm_core::LifecycleEvent::SessionEnd { .. });

        self.registry
            .apply_lifecycle_event(
                session_id.clone(),
                lifecycle,
                atm_core::Harness::Codex,
                pid,
                tmux_pane.clone(),
            )
            .await
            .map_err(|e| ConnectionError::RegistryError(e.to_string()))?;

        if !ends_session && model.is_some() {
            self.registry
                .apply_lifecycle_event(
                    session_id.clone(),
                    atm_core::LifecycleEvent::ProviderModelChange {
                        provider: None,
                        model,
                    },
                    atm_core::Harness::Codex,
                    pid,
                    tmux_pane.clone(),
                )
                .await
                .map_err(|e| ConnectionError::RegistryError(e.to_string()))?;
        }

        // Codex has no status-line hook, but its rollout transcript
        // currently persists token_count records. Read only a bounded
        // tail on the blocking pool and treat every parsing/I/O failure
        // as unavailable metadata: lifecycle handling must still win.
        if !ends_session {
            let usage = match transcript_path.filter(|path| !path.is_empty()) {
                Some(path) => {
                    let task = tokio::task::spawn_blocking(move || {
                        atm_codex_adapter::read_token_usage(path)
                    });
                    match timeout(CODEX_TRANSCRIPT_READ_TIMEOUT, task).await {
                        Ok(Ok(Ok(usage))) => usage,
                        Ok(Ok(Err(error))) => {
                            debug!(%error, "Codex transcript token usage unavailable");
                            None
                        }
                        Ok(Err(error)) => {
                            debug!(%error, "Codex transcript reader task failed");
                            None
                        }
                        Err(_) => {
                            warn!("Codex transcript token usage read timed out");
                            None
                        }
                    }
                }
                None => None,
            };

            if let Some(usage) = usage {
                self.registry
                    .apply_lifecycle_event(
                        session_id,
                        usage.to_lifecycle_event(),
                        atm_core::Harness::Codex,
                        pid,
                        tmux_pane,
                    )
                    .await
                    .map_err(|e| ConnectionError::RegistryError(e.to_string()))?;
            }
        }

        Ok(())
    }

    /// Stops the current subscription's forwarder, if any.
    async fn stop_forwarder(&mut self) {
        if let Some(forwarder) = self.forwarder.take() {
            forwarder.stop().await;
        }
    }

    /// Handles a discovery request from a TUI client.
    async fn handle_discover(&mut self) -> DiscoveryResult {
        info!(client_id = ?self.client_id, "Processing discovery request");

        let discovery = DiscoveryService::new(self.registry.clone());
        discovery.discover().await
    }

    /// Reads a single message from the client.
    async fn read_message(&mut self) -> Result<ClientMessage, ConnectionError> {
        let mut line = Vec::new();

        // One byte past the limit is enough to tell a line is too large,
        // so a client that never sends a newline can't grow `line` forever.
        let bytes_read = (&mut self.reader)
            .take(MAX_MESSAGE_SIZE as u64 + 1)
            .read_until(b'\n', &mut line)
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

        let msg: ClientMessage = serde_json::from_slice(&line)
            .map_err(|e| ConnectionError::ParseError(e.to_string()))?;

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

/// Parses a Claude status line from the wire into the pid `atm-hook`
/// added and the status line's vendor-neutral data.
///
/// # Errors
///
/// `ConnectionError::ParseError` if the line doesn't parse as a
/// `RawStatusLine`.
pub fn parse_status_line(
    data: serde_json::Value,
) -> Result<(Option<u32>, StatusLineData), ConnectionError> {
    let raw: RawStatusLine =
        serde_json::from_value(data).map_err(|e| ConnectionError::ParseError(e.to_string()))?;
    // Always Some today; the Option predates the empty-model rules
    let status = raw
        .to_status_line_data()
        .ok_or_else(|| ConnectionError::ParseError("status line has no data".to_string()))?;
    Ok((raw.pid, status))
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

/// The event forwarder of a connection's current subscription.
#[must_use = "dropping a Forwarder aborts its task"]
struct Forwarder {
    /// Stops the forwarder without closing the connection
    stop: CancellationToken,

    /// Owns the forwarder task. Dropping it aborts the task, so an aborted
    /// connection handler never leaves its forwarder running.
    task: JoinSet<()>,
}

impl Forwarder {
    /// Spawns [`forward_events`] for one subscription on `connection`.
    fn spawn(
        client_id: ClientId,
        events: broadcast::Receiver<SessionEvent>,
        writer: SubscriberWriter,
        filter: Option<SessionId>,
        connection: &CancellationToken,
    ) -> Self {
        let stop = connection.child_token();
        let mut task = JoinSet::new();
        task.spawn(forward_events(
            client_id,
            events,
            writer,
            filter,
            connection.clone(),
            stop.clone(),
        ));
        Self { stop, task }
    }

    /// Stops forwarding and waits for the task. The task only checks
    /// `stop` between events, so it never leaves half a line behind.
    async fn stop(mut self) {
        self.stop.cancel();
        while self.task.join_next().await.is_some() {}
    }
}

/// Forwards registry events to one subscriber until `stop` is cancelled.
///
/// Each subscription has its own broadcast receiver, so a slow subscriber
/// only delays itself. One that can't keep up has its whole `connection`
/// cancelled: a write that exceeded [`WRITE_TIMEOUT`] may have left half a
/// line on the socket, and a receiver that lagged past the channel's
/// capacity has missed events. The client recovers by reconnecting, which
/// re-subscribes and fetches a full session list.
async fn forward_events(
    client_id: ClientId,
    mut events: broadcast::Receiver<SessionEvent>,
    writer: SubscriberWriter,
    filter: Option<SessionId>,
    connection: CancellationToken,
    stop: CancellationToken,
) {
    loop {
        let event = tokio::select! {
            biased;
            _ = stop.cancelled() => return,
            event = events.recv() => event,
        };
        let event = match event {
            Ok(event) => event,
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                warn!(
                    client_id = %client_id,
                    skipped,
                    "Subscriber lagged behind events, disconnecting"
                );
                break;
            }
            Err(broadcast::error::RecvError::Closed) => return,
        };

        // Check filter
        let session_id = match &event {
            SessionEvent::Registered { session_id, .. }
            | SessionEvent::Removed { session_id, .. } => session_id,
            SessionEvent::Updated { session } => &session.id,
        };
        if filter.as_ref().is_some_and(|filter| filter != session_id) {
            continue;
        }

        match send_event(&writer, &event).await {
            Ok(()) => {}
            // Nothing was written, so skip just this event
            Err(ConnectionError::ParseError(e)) => {
                error!(error = %e, "Failed to serialize event");
            }
            Err(e) => {
                warn!(
                    client_id = %client_id,
                    error = %e,
                    "Failed to send event to subscriber, disconnecting"
                );
                break;
            }
        }
    }
    connection.cancel();
}

/// Sends an event to a subscribed client.
///
/// This is used by each subscription's forwarder.
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

    use atm_core::{AgentType, Model, SessionDomain, SessionView};
    use tokio::net::UnixStream;

    use crate::registry::RemovalReason;

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

    // Synthetic: the status lines below are hand-written, trimmed to the
    // fields each test needs. Captured lines are tested in
    // atm-claude-adapter's status_line.rs.

    #[test]
    fn test_parse_status_line_returns_pid_and_data() {
        let (pid, data) = parse_status_line(serde_json::json!({
            "session_id": "sl-test",
            "pid": 4242,
            "model": {"id": "claude-sonnet-4-20250514"},
            "cost": {"total_cost_usd": 0.25, "total_duration_ms": 15000}
        }))
        .unwrap();

        assert_eq!(pid, Some(4242));
        assert_eq!(data.session_id, "sl-test");
        assert_eq!(data.model_id, "claude-sonnet-4-20250514");
        assert_eq!(data.cost_usd, 0.25);
        assert_eq!(data.total_duration_ms, 15000);
    }

    /// A malformed line fails here as a ParseError, the variant a line
    /// without a session_id already gets, so `process_messages` replies
    /// with the error and keeps the connection open for both.
    #[test]
    fn test_parse_status_line_malformed_is_parse_error() {
        let err = parse_status_line(serde_json::json!({
            "session_id": "sl-test",
            "cost": {"total_cost_usd": "x", "total_duration_ms": 0}
        }))
        .unwrap_err();

        assert!(matches!(err, ConnectionError::ParseError(_)));
        assert_eq!(
            err.to_string(),
            "Parse error: invalid type: string \"x\", expected f64"
        );
    }

    /// A subscriber writer on one end of a socket pair, plus the peer end.
    fn subscriber_pair() -> (SubscriberWriter, UnixStream) {
        let (ours, peer) = UnixStream::pair().expect("socket pair");
        let (_, write_half) = ours.into_split();
        (Arc::new(Mutex::new(BufWriter::new(write_half))), peer)
    }

    fn updated(id: &str) -> SessionEvent {
        let domain = SessionDomain::new(
            SessionId::new(id),
            AgentType::GeneralPurpose,
            Model::Sonnet4,
        );
        SessionEvent::Updated {
            session: Box::new(SessionView::from_domain(&domain)),
        }
    }

    /// A subscriber that never reads fills its socket; the next write hits
    /// WRITE_TIMEOUT and the forwarder cancels the whole connection.
    /// Time is paused, so the timeout elapses as soon as the write blocks.
    #[tokio::test(start_paused = true)]
    async fn test_forwarder_disconnects_subscriber_that_never_reads() {
        let (writer, _peer) = subscriber_pair();
        let (tx, rx) = broadcast::channel(4096);
        for _ in 0..4000 {
            tx.send(updated("flood")).unwrap();
        }
        let connection = CancellationToken::new();

        let _forwarder = Forwarder::spawn("stuck".to_string(), rx, writer, None, &connection);

        timeout(Duration::from_secs(60), connection.cancelled())
            .await
            .expect("forwarder should cancel the connection after a write timeout");
    }

    /// A subscriber that falls more than the channel capacity behind has
    /// missed events, so the forwarder cancels the connection.
    #[tokio::test]
    async fn test_forwarder_disconnects_subscriber_that_lags() {
        let (writer, _peer) = subscriber_pair();
        let (tx, rx) = broadcast::channel(4);
        for _ in 0..10 {
            tx.send(updated("burst")).unwrap();
        }
        let connection = CancellationToken::new();

        let _forwarder = Forwarder::spawn("lagging".to_string(), rx, writer, None, &connection);

        timeout(Duration::from_secs(5), connection.cancelled())
            .await
            .expect("forwarder should cancel the connection on lag");
    }

    /// Only events for the filtered session are written, and `Registered`
    /// is never written (subscribers wait for the first update).
    #[tokio::test]
    async fn test_forwarder_applies_session_filter() {
        let (writer, peer) = subscriber_pair();
        let (tx, rx) = broadcast::channel(16);
        tx.send(SessionEvent::Registered {
            session_id: SessionId::new("wanted"),
            agent_type: AgentType::GeneralPurpose,
        })
        .unwrap();
        tx.send(updated("other")).unwrap();
        tx.send(SessionEvent::Removed {
            session_id: SessionId::new("other"),
            reason: RemovalReason::SessionEnded,
        })
        .unwrap();
        tx.send(updated("wanted")).unwrap();
        let connection = CancellationToken::new();

        let _forwarder = Forwarder::spawn(
            "filtered".to_string(),
            rx,
            writer,
            Some(SessionId::new("wanted")),
            &connection,
        );

        let mut line = String::new();
        timeout(
            Duration::from_secs(5),
            BufReader::new(peer).read_line(&mut line),
        )
        .await
        .unwrap()
        .unwrap();
        match serde_json::from_str::<DaemonMessage>(&line).unwrap() {
            DaemonMessage::SessionUpdated { session } => {
                assert_eq!(session.id, SessionId::new("wanted"));
            }
            other => panic!("Expected SessionUpdated for 'wanted', got {other:?}"),
        }
    }

    /// `stop` takes priority over queued events, so a forwarder stopped
    /// before it runs writes nothing. Repeated because an unbiased
    /// `select!` would pick the stop branch half the time.
    #[tokio::test]
    async fn test_forwarder_stop_takes_priority_over_queued_events() {
        for _ in 0..20 {
            let (writer, peer) = subscriber_pair();
            let (tx, rx) = broadcast::channel(16);
            for _ in 0..8 {
                tx.send(updated("queued")).unwrap();
            }
            let connection = CancellationToken::new();
            let forwarder = Forwarder::spawn("stopping".to_string(), rx, writer, None, &connection);

            forwarder.stop().await;

            let mut line = String::new();
            let read = BufReader::new(peer).read_line(&mut line).await.unwrap();
            assert_eq!(read, 0, "forwarder wrote {line:?} after stop");
        }
    }

    /// Stopping a forwarder (unsubscribe) ends it without closing the
    /// connection.
    #[tokio::test]
    async fn test_forwarder_stop_keeps_connection_open() {
        let (writer, _peer) = subscriber_pair();
        let (_tx, rx) = broadcast::channel(16);
        let connection = CancellationToken::new();
        let forwarder =
            Forwarder::spawn("unsubscribing".to_string(), rx, writer, None, &connection);

        timeout(Duration::from_secs(5), forwarder.stop())
            .await
            .expect("forwarder should stop");

        assert!(!connection.is_cancelled());
    }
}
