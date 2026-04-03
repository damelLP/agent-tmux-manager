//! Daemon connection client for the ATM TUI.
//!
//! This module provides the `DaemonClient` which handles:
//! - Connection to the daemon via Unix socket
//! - Automatic reconnection with exponential backoff
//! - Parsing and forwarding daemon messages to the TUI event loop
//!
//! **Panic-Free Policy:** This module follows the project's panic-free guidelines.
//! No `.unwrap()`, `.expect()`, `panic!()`, `unreachable!()`, or `todo!()`.

use std::path::PathBuf;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::error::{Result, TuiError};
use crate::input::{ClientCommand, Event};
use atm_protocol::{ClientMessage, DaemonMessage, ProtocolVersion};

// ============================================================================
// Configuration
// ============================================================================

/// Configuration for the daemon client.
///
/// Controls connection behavior including socket path, retry logic,
/// and timeouts for daemon communication.
///
/// # Example
///
/// ```rust
/// use atm_tui::client::DaemonConfig;
/// use std::time::Duration;
///
/// let config = DaemonConfig {
///     socket_path: std::path::PathBuf::from("/tmp/my-daemon.sock"),
///     retry_initial_delay: Duration::from_millis(500),
///     ..Default::default()
/// };
/// ```
#[derive(Debug, Clone)]
pub struct DaemonConfig {
    /// Path to the Unix socket where the daemon listens.
    pub socket_path: PathBuf,

    /// Initial delay before first retry after connection failure.
    pub retry_initial_delay: Duration,

    /// Maximum delay between retry attempts.
    pub retry_max_delay: Duration,

    /// Multiplier for exponential backoff (e.g., 2.0 doubles delay each retry).
    pub retry_multiplier: f64,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            socket_path: PathBuf::from("/tmp/atm.sock"),
            retry_initial_delay: Duration::from_secs(1),
            retry_max_delay: Duration::from_secs(30),
            retry_multiplier: 2.0,
        }
    }
}

// ============================================================================
// Daemon Client
// ============================================================================

/// Client for communicating with the ATM daemon.
///
/// The `DaemonClient` manages the connection to the daemon, handles
/// automatic reconnection with exponential backoff, and forwards
/// session updates to the TUI via the event channel.
///
/// # Connection Lifecycle
///
/// 1. Client attempts to connect to the Unix socket
/// 2. On success, sends a `Connect` message and waits for `Connected` response
/// 3. Sends a `Subscribe` message to receive session updates
/// 4. Reads messages in a loop, forwarding session updates to the TUI
/// 5. On disconnect, notifies TUI and retries with exponential backoff
///
/// # Example
///
/// ```rust,ignore
/// use atm_tui::client::{DaemonClient, DaemonConfig};
/// use tokio::sync::mpsc;
/// use tokio_util::sync::CancellationToken;
///
/// let (tx, rx) = mpsc::unbounded_channel();
/// let cancel_token = CancellationToken::new();
/// let client = DaemonClient::new(DaemonConfig::default(), tx, cancel_token);
///
/// // Run in a separate task
/// tokio::spawn(async move {
///     client.run().await;
/// });
/// ```
pub struct DaemonClient {
    /// Configuration for connection behavior.
    config: DaemonConfig,

    /// Channel to send events to the TUI.
    event_tx: mpsc::UnboundedSender<Event>,

    /// Channel to receive commands from the TUI.
    command_rx: tokio::sync::Mutex<mpsc::UnboundedReceiver<ClientCommand>>,

    /// Cancellation token for graceful shutdown.
    cancel_token: CancellationToken,
}

impl DaemonClient {
    /// Creates a new daemon client.
    ///
    /// # Arguments
    ///
    /// * `config` - Configuration for connection behavior
    /// * `event_tx` - Channel to send events to the TUI
    /// * `command_rx` - Channel to receive commands from the TUI
    /// * `cancel_token` - Token for signaling shutdown
    ///
    /// # Returns
    ///
    /// A new `DaemonClient` instance ready to connect.
    #[must_use]
    pub fn new(
        config: DaemonConfig,
        event_tx: mpsc::UnboundedSender<Event>,
        command_rx: mpsc::UnboundedReceiver<ClientCommand>,
        cancel_token: CancellationToken,
    ) -> Self {
        Self {
            config,
            event_tx,
            command_rx: tokio::sync::Mutex::new(command_rx),
            cancel_token,
        }
    }

    /// Creates a new daemon client with default configuration.
    ///
    /// # Arguments
    ///
    /// * `event_tx` - Channel to send events to the TUI
    /// * `command_rx` - Channel to receive commands from the TUI
    /// * `cancel_token` - Token for signaling shutdown
    ///
    /// # Returns
    ///
    /// A new `DaemonClient` instance with default settings.
    #[must_use]
    pub fn with_defaults(
        event_tx: mpsc::UnboundedSender<Event>,
        command_rx: mpsc::UnboundedReceiver<ClientCommand>,
        cancel_token: CancellationToken,
    ) -> Self {
        Self::new(DaemonConfig::default(), event_tx, command_rx, cancel_token)
    }

    /// Main loop that maintains connection to the daemon.
    ///
    /// This method runs indefinitely until the cancellation token is triggered.
    /// It handles connection, reconnection, and message processing.
    ///
    /// The loop:
    /// 1. Attempts to connect with exponential backoff
    /// 2. Reads and processes messages
    /// 3. On disconnect, notifies the TUI and retries
    ///
    /// # Cancellation
    ///
    /// The loop checks the cancellation token between operations and will
    /// exit gracefully when cancelled.
    pub async fn run(&self) {
        info!(
            socket_path = %self.config.socket_path.display(),
            "Daemon client starting"
        );

        loop {
            // Check for cancellation before attempting connection
            if self.cancel_token.is_cancelled() {
                info!("Daemon client shutting down (cancelled)");
                return;
            }

            // Attempt to connect with retry logic
            match self.connect_with_retry().await {
                Ok(stream) => {
                    info!("Connected to daemon");

                    // Read messages until disconnect or error
                    if let Err(e) = self.handle_connection(stream).await {
                        warn!(error = %e, "Connection ended with error");
                    }

                    // Notify TUI of disconnect (ignore send errors - TUI may be shutting down)
                    let _ = self.event_tx.send(Event::DaemonDisconnected);
                }
                Err(e) => {
                    // Only log if not cancelled - connect_with_retry returns early on cancel
                    if !self.cancel_token.is_cancelled() {
                        error!(error = %e, "Failed to connect to daemon");
                    }
                }
            }

            // Check for cancellation before next retry
            if self.cancel_token.is_cancelled() {
                info!("Daemon client shutting down (cancelled)");
                return;
            }
        }
    }

    /// Attempts to connect to the daemon with exponential backoff.
    ///
    /// Retries connection indefinitely until successful or cancelled.
    /// Uses exponential backoff between attempts, starting at `retry_initial_delay`
    /// and capping at `retry_max_delay`.
    ///
    /// # Returns
    ///
    /// * `Ok(UnixStream)` - Connected stream ready for communication
    /// * `Err(TuiError)` - If cancelled during retry (not typically an error path)
    async fn connect_with_retry(&self) -> Result<UnixStream> {
        let mut delay = self.config.retry_initial_delay;
        let mut attempt = 0u32;

        loop {
            attempt = attempt.saturating_add(1);

            debug!(
                attempt,
                socket_path = %self.config.socket_path.display(),
                "Attempting to connect to daemon"
            );

            // Check if socket exists first (provides better error message)
            if !self.config.socket_path.exists() {
                if attempt == 1 {
                    warn!(
                        socket_path = %self.config.socket_path.display(),
                        "Daemon socket not found, will retry"
                    );
                }
            } else {
                // Attempt connection
                match UnixStream::connect(&self.config.socket_path).await {
                    Ok(stream) => {
                        debug!(attempt, "Connection successful");
                        return Ok(stream);
                    }
                    Err(e) => {
                        debug!(
                            attempt,
                            error = %e,
                            "Connection attempt failed"
                        );
                    }
                }
            }

            // Wait before retry, checking for cancellation
            tokio::select! {
                _ = sleep(delay) => {
                    // Increase delay for next attempt (exponential backoff)
                    let next_delay_ms = (delay.as_millis() as f64 * self.config.retry_multiplier) as u64;
                    delay = Duration::from_millis(next_delay_ms).min(self.config.retry_max_delay);
                }
                _ = self.cancel_token.cancelled() => {
                    info!("Connection retry cancelled");
                    return Err(TuiError::DaemonConnection("cancelled".to_string()));
                }
            }
        }
    }

    /// Handles an established connection to the daemon.
    ///
    /// Performs the connection handshake, subscribes to updates,
    /// and reads messages until disconnect.
    ///
    /// # Arguments
    ///
    /// * `stream` - Connected Unix socket stream
    ///
    /// # Returns
    ///
    /// * `Ok(())` - Connection ended gracefully
    /// * `Err(TuiError)` - Connection ended due to error
    async fn handle_connection(&self, stream: UnixStream) -> Result<()> {
        let (reader, mut writer) = stream.into_split();
        let mut buf_reader = BufReader::new(reader);

        // Send connect message
        let connect_msg = ClientMessage::connect(None);
        self.send_message(&mut writer, &connect_msg).await?;

        // Wait for connected response
        let mut line = String::new();
        buf_reader.read_line(&mut line).await?;

        let response: DaemonMessage = serde_json::from_str(line.trim())?;
        match response {
            DaemonMessage::Connected {
                protocol_version,
                client_id,
            } => {
                // Verify protocol version compatibility
                if !ProtocolVersion::CURRENT.is_compatible_with(&protocol_version) {
                    return Err(TuiError::VersionMismatch {
                        client_version: ProtocolVersion::CURRENT.to_string(),
                        daemon_version: protocol_version.to_string(),
                    });
                }
                info!(
                    client_id,
                    protocol_version = %protocol_version,
                    "Handshake complete"
                );
            }
            DaemonMessage::Rejected {
                reason: _,
                protocol_version,
            } => {
                return Err(TuiError::VersionMismatch {
                    client_version: ProtocolVersion::CURRENT.to_string(),
                    daemon_version: protocol_version.to_string(),
                });
            }
            _ => {
                return Err(TuiError::ProtocolError(format!(
                    "Unexpected response to connect: {response:?}"
                )));
            }
        }

        // Subscribe to session updates
        let subscribe_msg = ClientMessage::subscribe(None);
        self.send_message(&mut writer, &subscribe_msg).await?;

        // Request initial session list
        let list_msg = ClientMessage::list_sessions();
        self.send_message(&mut writer, &list_msg).await?;

        // Read messages and handle commands in a loop
        self.message_loop(&mut buf_reader, &mut writer).await
    }

    /// Sends a message to the daemon.
    ///
    /// Serializes the message to JSON and writes it to the stream
    /// followed by a newline delimiter.
    ///
    /// # Arguments
    ///
    /// * `writer` - Writable half of the Unix socket
    /// * `message` - Message to send
    ///
    /// # Returns
    ///
    /// * `Ok(())` - Message sent successfully
    /// * `Err(TuiError)` - Failed to serialize or write
    async fn send_message<W: AsyncWriteExt + Unpin>(
        &self,
        writer: &mut W,
        message: &ClientMessage,
    ) -> Result<()> {
        let json = serde_json::to_string(message)?;
        writer.write_all(json.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
        debug!(message_type = ?message.message, "Sent message to daemon");
        Ok(())
    }

    /// Main message loop that handles both daemon messages and TUI commands.
    ///
    /// Reads newline-delimited JSON messages from the daemon and forwards
    /// session updates to the TUI. Also processes commands from the TUI
    /// (like discovery requests) and sends them to the daemon.
    ///
    /// # Arguments
    ///
    /// * `reader` - Buffered reader for the Unix socket
    /// * `writer` - Writer for sending messages to the daemon
    ///
    /// # Returns
    ///
    /// * `Ok(())` - Connection closed gracefully (EOF)
    /// * `Err(TuiError)` - Read or parse error
    async fn message_loop<R, W>(&self, reader: &mut R, writer: &mut W) -> Result<()>
    where
        R: AsyncBufReadExt + Unpin,
        W: AsyncWriteExt + Unpin,
    {
        let mut line = String::new();

        loop {
            // Check for cancellation
            if self.cancel_token.is_cancelled() {
                debug!("Message loop cancelled");
                return Ok(());
            }

            // Lock the command receiver for this iteration
            let mut command_rx = self.command_rx.lock().await;

            // Wait for either a daemon message or a TUI command
            line.clear();
            tokio::select! {
                // Read from daemon
                read_result = reader.read_line(&mut line) => {
                    drop(command_rx); // Release lock before processing
                    match read_result {
                        Ok(0) => {
                            // EOF - connection closed
                            info!("Daemon closed connection");
                            return Ok(());
                        }
                        Ok(_) => {
                            // Parse and handle message
                            if let Err(e) = self.handle_message(line.trim()).await {
                                warn!(error = %e, line = %line.trim(), "Failed to handle message");
                                // Continue reading - don't disconnect on single parse error
                            }
                        }
                        Err(e) => {
                            return Err(TuiError::Io(e));
                        }
                    }
                }

                // Receive command from TUI
                command = command_rx.recv() => {
                    drop(command_rx); // Release lock before processing
                    match command {
                        Some(ClientCommand::Discover) => {
                            debug!("Received discover command from TUI");
                            let discover_msg = ClientMessage::discover();
                            if let Err(e) = self.send_message(writer, &discover_msg).await {
                                warn!(error = %e, "Failed to send discover message");
                            }
                        }
                        None => {
                            // Command channel closed - TUI shutting down
                            debug!("Command channel closed");
                            return Ok(());
                        }
                    }
                }

                // Cancellation
                _ = self.cancel_token.cancelled() => {
                    drop(command_rx);
                    debug!("Message loop cancelled during select");
                    return Ok(());
                }
            }
        }
    }

    /// Handles a single message from the daemon.
    ///
    /// Parses the JSON message and forwards relevant events to the TUI.
    ///
    /// # Arguments
    ///
    /// * `line` - Raw JSON message string
    ///
    /// # Returns
    ///
    /// * `Ok(())` - Message handled successfully
    /// * `Err(TuiError)` - Failed to parse message
    async fn handle_message(&self, line: &str) -> Result<()> {
        let message: DaemonMessage = serde_json::from_str(line)?;

        match message {
            DaemonMessage::SessionList { sessions } => {
                debug!(count = sessions.len(), "Received session list");
                // Full list replaces all sessions (initial sync)
                let _ = self.event_tx.send(Event::SessionListReplace(sessions));
            }
            DaemonMessage::SessionUpdated { session } => {
                debug!(session_id = %session.id, "Received session update");
                // Individual update merges with existing sessions
                let _ = self.event_tx.send(Event::SessionUpdate(vec![*session]));
            }
            DaemonMessage::SessionRemoved { session_id } => {
                debug!(session_id = %session_id, "Session removed");
                let _ = self
                    .event_tx
                    .send(Event::SessionRemoved(session_id.to_string()));
            }
            DaemonMessage::DiscoveryComplete { discovered, failed } => {
                debug!(discovered, failed, "Discovery complete");
                let _ = self
                    .event_tx
                    .send(Event::DiscoveryComplete { discovered, failed });
            }
            DaemonMessage::Pong { seq } => {
                debug!(seq, "Received pong");
                // Pong responses are for connection health checks
                // Currently not used, but could track latency
            }
            DaemonMessage::Error { message, code } => {
                warn!(
                    error_message = %message,
                    error_code = ?code,
                    "Received error from daemon"
                );
            }
            DaemonMessage::Connected { .. } | DaemonMessage::Rejected { .. } => {
                // These should only appear during handshake
                warn!("Received unexpected handshake message after connection");
            }
        }

        Ok(())
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use atm_core::{SessionId, SessionView};
    use std::time::Duration;

    // ------------------------------------------------------------------------
    // DaemonConfig Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_daemon_config_default() {
        let config = DaemonConfig::default();

        assert_eq!(config.socket_path, PathBuf::from("/tmp/atm.sock"));
        assert_eq!(config.retry_initial_delay, Duration::from_secs(1));
        assert_eq!(config.retry_max_delay, Duration::from_secs(30));
        assert!((config.retry_multiplier - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_daemon_config_custom() {
        let config = DaemonConfig {
            socket_path: PathBuf::from("/custom/path.sock"),
            retry_initial_delay: Duration::from_millis(500),
            retry_max_delay: Duration::from_secs(60),
            retry_multiplier: 1.5,
        };

        assert_eq!(config.socket_path, PathBuf::from("/custom/path.sock"));
        assert_eq!(config.retry_initial_delay, Duration::from_millis(500));
        assert_eq!(config.retry_max_delay, Duration::from_secs(60));
        assert!((config.retry_multiplier - 1.5).abs() < f64::EPSILON);
    }

    #[test]
    fn test_daemon_config_debug() {
        let config = DaemonConfig::default();
        let debug_str = format!("{config:?}");
        assert!(debug_str.contains("DaemonConfig"));
        assert!(debug_str.contains("socket_path"));
    }

    #[test]
    fn test_daemon_config_clone() {
        let config = DaemonConfig::default();
        let cloned = config.clone();
        assert_eq!(config.socket_path, cloned.socket_path);
        assert_eq!(config.retry_initial_delay, cloned.retry_initial_delay);
    }

    // ------------------------------------------------------------------------
    // DaemonClient Tests
    // ------------------------------------------------------------------------

    /// Helper to create a test client with all channels
    fn create_test_client() -> (DaemonClient, mpsc::UnboundedReceiver<Event>) {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let cancel_token = CancellationToken::new();
        let client = DaemonClient::with_defaults(event_tx, cmd_rx, cancel_token);
        (client, event_rx)
    }

    #[test]
    fn test_daemon_client_new() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let cancel_token = CancellationToken::new();
        let config = DaemonConfig::default();

        let client = DaemonClient::new(config.clone(), tx, cmd_rx, cancel_token);

        assert_eq!(client.config.socket_path, config.socket_path);
    }

    #[test]
    fn test_daemon_client_with_defaults() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let cancel_token = CancellationToken::new();

        let client = DaemonClient::with_defaults(tx, cmd_rx, cancel_token);

        assert_eq!(client.config.socket_path, PathBuf::from("/tmp/atm.sock"));
    }

    // ------------------------------------------------------------------------
    // Message Handling Tests
    // ------------------------------------------------------------------------

    fn create_test_session(id: &str) -> SessionView {
        SessionView {
            id: SessionId::new(id),
            id_short: id.get(..8).unwrap_or(id).to_string(),
            agent_type: "general".to_string(),
            model: "Opus 4.5".to_string(),
            status: atm_core::SessionStatus::Working,
            status_label: "working".to_string(),
            activity_detail: None,
            should_blink: false,
            status_icon: ">".to_string(),
            context_percentage: 25.0,
            context_display: "25%".to_string(),
            context_warning: false,
            context_critical: false,
            cost_display: "$0.50".to_string(),
            cost_usd: 0.50,
            duration_display: "5m".to_string(),
            duration_seconds: 300.0,
            lines_display: "+100 -20".to_string(),
            working_directory: Some("/home/user/project".to_string()),

            needs_attention: false,
            last_activity_display: "10s ago".to_string(),
            age_display: "5m ago".to_string(),
            started_at: "2024-01-15T10:00:00Z".to_string(),
            last_activity: "2024-01-15T10:05:00Z".to_string(),
            tmux_pane: None,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn test_handle_message_session_list() {
        let (client, mut rx) = create_test_client();

        let sessions = vec![create_test_session("session-1")];
        let msg = DaemonMessage::session_list(sessions.clone());
        let json = serde_json::to_string(&msg).unwrap();

        client.handle_message(&json).await.unwrap();

        // Check that event was sent
        let event = rx.try_recv().unwrap();
        match event {
            Event::SessionListReplace(received_sessions) => {
                assert_eq!(received_sessions.len(), 1);
                assert_eq!(received_sessions[0].id.as_str(), "session-1");
            }
            _ => panic!("Expected SessionListReplace event"),
        }
    }

    #[tokio::test]
    async fn test_handle_message_session_updated() {
        let (client, mut rx) = create_test_client();

        let session = create_test_session("session-updated");
        let msg = DaemonMessage::session_updated(session);
        let json = serde_json::to_string(&msg).unwrap();

        client.handle_message(&json).await.unwrap();

        let event = rx.try_recv().unwrap();
        match event {
            Event::SessionUpdate(sessions) => {
                assert_eq!(sessions.len(), 1);
                assert_eq!(sessions[0].id.as_str(), "session-updated");
            }
            _ => panic!("Expected SessionUpdate event"),
        }
    }

    #[tokio::test]
    async fn test_handle_message_session_removed() {
        let (client, mut rx) = create_test_client();

        let msg = DaemonMessage::session_removed(SessionId::new("session-removed"));
        let json = serde_json::to_string(&msg).unwrap();

        // Should not error
        client.handle_message(&json).await.unwrap();

        // SessionRemoved event is now sent
        let event = rx.try_recv().unwrap();
        match event {
            Event::SessionRemoved(session_id) => {
                assert_eq!(session_id, "session-removed");
            }
            _ => panic!("Expected SessionRemoved event"),
        }
    }

    #[tokio::test]
    async fn test_handle_message_pong() {
        let (client, mut rx) = create_test_client();

        let msg = DaemonMessage::pong(42);
        let json = serde_json::to_string(&msg).unwrap();

        client.handle_message(&json).await.unwrap();

        // Pong doesn't generate an event
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_handle_message_error() {
        let (client, mut rx) = create_test_client();

        let msg = DaemonMessage::error("test error");
        let json = serde_json::to_string(&msg).unwrap();

        client.handle_message(&json).await.unwrap();

        // Error doesn't generate an event to the TUI
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_handle_message_invalid_json() {
        let (client, _rx) = create_test_client();

        let result = client.handle_message("not valid json").await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_handle_message_discovery_complete() {
        let (client, mut rx) = create_test_client();

        let msg = DaemonMessage::discovery_complete(5, 2);
        let json = serde_json::to_string(&msg).unwrap();

        client.handle_message(&json).await.unwrap();

        let event = rx.try_recv().unwrap();
        match event {
            Event::DiscoveryComplete { discovered, failed } => {
                assert_eq!(discovered, 5);
                assert_eq!(failed, 2);
            }
            _ => panic!("Expected DiscoveryComplete event"),
        }
    }

    // ------------------------------------------------------------------------
    // Exponential Backoff Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_exponential_backoff_calculation() {
        let config = DaemonConfig::default();

        // Initial delay is 1 second
        let delay1 = config.retry_initial_delay;
        assert_eq!(delay1, Duration::from_secs(1));

        // After multiplier: 1 * 2 = 2 seconds
        let delay2_ms = (delay1.as_millis() as f64 * config.retry_multiplier) as u64;
        let delay2 = Duration::from_millis(delay2_ms);
        assert_eq!(delay2, Duration::from_secs(2));

        // After multiplier again: 2 * 2 = 4 seconds
        let delay3_ms = (delay2.as_millis() as f64 * config.retry_multiplier) as u64;
        let delay3 = Duration::from_millis(delay3_ms);
        assert_eq!(delay3, Duration::from_secs(4));
    }

    #[test]
    fn test_exponential_backoff_max_cap() {
        let config = DaemonConfig {
            retry_max_delay: Duration::from_secs(10),
            retry_multiplier: 10.0,
            ..Default::default()
        };

        // Start at 1 second, multiply by 10 = 10 seconds
        let delay1 = config.retry_initial_delay;
        let delay2_ms = (delay1.as_millis() as f64 * config.retry_multiplier) as u64;
        let delay2 = Duration::from_millis(delay2_ms).min(config.retry_max_delay);
        assert_eq!(delay2, Duration::from_secs(10));

        // Multiply by 10 again would be 100 seconds, but cap at 10
        let delay3_ms = (delay2.as_millis() as f64 * config.retry_multiplier) as u64;
        let delay3 = Duration::from_millis(delay3_ms).min(config.retry_max_delay);
        assert_eq!(delay3, Duration::from_secs(10));
    }

    // ------------------------------------------------------------------------
    // Cancellation Tests
    // ------------------------------------------------------------------------

    #[tokio::test]
    async fn test_client_respects_cancellation() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let cancel_token = CancellationToken::new();
        let config = DaemonConfig {
            // Use non-existent socket to ensure we hit retry loop
            socket_path: PathBuf::from("/tmp/nonexistent-test.sock"),
            retry_initial_delay: Duration::from_millis(10),
            ..Default::default()
        };

        let client = DaemonClient::new(config, tx, cmd_rx, cancel_token.clone());

        // Cancel immediately
        cancel_token.cancel();

        // Run should return quickly due to cancellation
        let start = std::time::Instant::now();
        client.run().await;
        let elapsed = start.elapsed();

        // Should complete almost immediately (well under 1 second)
        assert!(elapsed < Duration::from_millis(500));
    }
}
