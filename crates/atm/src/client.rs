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
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout, Instant};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::error::{Result, TuiError};
use crate::input::{ClientCommand, Event};
use atm_protocol::{ClientMessage, DaemonMessage, ProtocolVersion};

/// Default Unix socket path the daemon listens on.
pub const DEFAULT_SOCKET_PATH: &str = "/tmp/atm.sock";

/// Deadline for connecting to the daemon socket.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Deadline for the daemon's reply to `Connect`.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Deadline for writing one message to the daemon.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// The error every client deadline reports, so callers match one kind.
fn timed_out(step: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::TimedOut, format!("{step} timed out"))
}

/// Resolves the daemon socket path, honoring the `ATM_SOCKET` env var.
///
/// Mirrors `atmd`'s behavior so a daemon and its clients pointed at a
/// non-default socket (test sandboxes, multi-tenant setups) stay in sync.
///
/// `ATM_SOCKET` set but empty is treated as unset — connecting to an
/// empty path would just produce a confusing "No such file" error.
pub fn resolve_socket_path() -> PathBuf {
    std::env::var_os("ATM_SOCKET")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_SOCKET_PATH))
}

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

    /// Maximum delay between retry attempts. A connection that lasts this
    /// long resets the backoff; a shorter one (a daemon in a crash loop,
    /// or one that rejects the client) keeps backing off.
    pub retry_max_delay: Duration,

    /// Multiplier for exponential backoff (e.g., 2.0 doubles delay each retry).
    pub retry_multiplier: f64,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        // Honor `$ATM_SOCKET` so an isolated test daemon (and its
        // matching atm TUI) doesn't collide with the system-wide
        // daemon at `/tmp/atm.sock`. Mirrors `atmd` and `atm-hook`.
        Self {
            socket_path: resolve_socket_path(),
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
/// 6. On the default socket, each failed attempt also starts the daemon
///    if it is not running
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

    /// Starts the daemon after a failed connect, if it is not running.
    /// `ensure_daemon_running` on the default socket, which is the only
    /// socket it starts a daemon for; `None` on any other socket.
    respawn: Option<Respawn>,
}

/// A blocking step that starts the daemon unless it is already running.
type Respawn = Arc<dyn Fn() -> std::result::Result<(), String> + Send + Sync>;

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
    /// A new `DaemonClient` instance ready to connect. If
    /// `config.socket_path` is the default socket (`resolve_socket_path()`),
    /// the client starts the daemon (`atmd start -d`) whenever connecting
    /// fails while no daemon is running.
    #[must_use]
    pub fn new(
        config: DaemonConfig,
        event_tx: mpsc::UnboundedSender<Event>,
        command_rx: mpsc::UnboundedReceiver<ClientCommand>,
        cancel_token: CancellationToken,
    ) -> Self {
        let respawn = (config.socket_path == resolve_socket_path())
            .then(|| Arc::new(crate::daemon::ensure_daemon_running) as Respawn);
        Self {
            config,
            event_tx,
            command_rx: tokio::sync::Mutex::new(command_rx),
            cancel_token,
            respawn,
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

        let mut delay = self.config.retry_initial_delay;
        loop {
            // Check for cancellation before attempting connection
            if self.cancel_token.is_cancelled() {
                info!("Daemon client shutting down (cancelled)");
                return;
            }

            // Attempt to connect with retry logic
            match self.connect_with_retry(delay).await {
                Ok(stream) => {
                    info!("Connected to daemon");
                    let connected_at = Instant::now();

                    // Read messages until disconnect or error
                    let mut reason = None;
                    if let Err(e) = self.handle_connection(stream).await {
                        warn!(error = %e, "Connection ended with error");
                        // A rejection recurs on every retry, so show it
                        reason = matches!(e, TuiError::Rejected(_)).then(|| e.to_string());
                    }

                    // Notify TUI of disconnect (ignore send errors - TUI may be shutting down)
                    let _ = self.event_tx.send(Event::DaemonDisconnected(reason));

                    if connected_at.elapsed() >= self.config.retry_max_delay {
                        delay = self.config.retry_initial_delay;
                    } else {
                        // Err means cancelled, which the check below handles
                        let _ = self.wait_before_retry(&mut delay).await;
                    }
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
    /// Uses exponential backoff between attempts, starting at `delay` (which
    /// `run` carries across short-lived connections) and capping at
    /// `retry_max_delay`. Growth here isn't carried into later connections.
    ///
    /// # Returns
    ///
    /// * `Ok(UnixStream)` - Connected stream ready for communication
    /// * `Err(TuiError)` - If cancelled during retry (not typically an error path)
    async fn connect_with_retry(&self, mut delay: Duration) -> Result<UnixStream> {
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
                let connect = timeout(
                    CONNECT_TIMEOUT,
                    UnixStream::connect(&self.config.socket_path),
                );
                match connect.await.unwrap_or_else(|_| Err(timed_out("connect"))) {
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

            self.wait_before_retry(&mut delay).await?;
        }
    }

    /// Starts the daemon if it is not running, then waits `delay` and
    /// grows it for the next attempt (exponential backoff).
    ///
    /// # Returns
    ///
    /// * `Err(TuiError)` - If cancelled while waiting
    async fn wait_before_retry(&self, delay: &mut Duration) -> Result<()> {
        let respawn_and_wait = async {
            self.respawn_daemon().await;
            sleep(*delay).await;
        };
        tokio::select! {
            _ = respawn_and_wait => {
                let next_delay_ms = (delay.as_millis() as f64 * self.config.retry_multiplier) as u64;
                *delay = Duration::from_millis(next_delay_ms).min(self.config.retry_max_delay);
                Ok(())
            }
            _ = self.cancel_token.cancelled() => {
                info!("Connection retry cancelled");
                Err(TuiError::DaemonConnection("cancelled".to_string()))
            }
        }
    }

    /// Starts the daemon after `atmd stop` or a crash, so a running TUI
    /// recovers without a restart. The respawn step checks the PID file and
    /// the socket itself, so a daemon that is only slow is left alone. It
    /// sleeps while the daemon starts, so it runs off the runtime. A failure
    /// is only logged: the caller's backoff retries it.
    async fn respawn_daemon(&self) {
        let Some(respawn) = self.respawn.clone() else {
            return;
        };
        match tokio::task::spawn_blocking(move || respawn()).await {
            Ok(Ok(())) => debug!("Daemon running"),
            Ok(Err(e)) => warn!(error = %e, "Failed to start daemon, will retry"),
            Err(e) => warn!(error = %e, "Daemon start task failed, will retry"),
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
        timeout(HANDSHAKE_TIMEOUT, buf_reader.read_line(&mut line))
            .await
            .map_err(|_| timed_out("handshake"))??;

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
            DaemonMessage::Rejected { reason, .. } => {
                return Err(TuiError::Rejected(reason));
            }
            _ => {
                return Err(TuiError::ProtocolError(format!(
                    "Unexpected response to connect: {response:?}"
                )));
            }
        }

        // Subscribe to session updates. The daemon answers with the session
        // list, or with an Error when it refuses (e.g. at its subscriber
        // cap). A refused client would never get updates, so it's rejected.
        let subscribe_msg = ClientMessage::subscribe(None);
        self.send_message(&mut writer, &subscribe_msg).await?;
        line.clear();
        timeout(HANDSHAKE_TIMEOUT, buf_reader.read_line(&mut line))
            .await
            .map_err(|_| timed_out("subscribe"))??;
        if let DaemonMessage::Error { message, .. } = serde_json::from_str(line.trim())? {
            return Err(TuiError::Rejected(message));
        }
        self.handle_message(line.trim()).await?;

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
        timeout(WRITE_TIMEOUT, async {
            writer.write_all(json.as_bytes()).await?;
            writer.write_all(b"\n").await?;
            writer.flush().await
        })
        .await
        .map_err(|_| timed_out("write"))??;
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
        // Unlike `read_line`, `next_line` keeps a partly read line when a
        // command wins the select below.
        let mut lines = reader.lines();

        loop {
            // Check for cancellation
            if self.cancel_token.is_cancelled() {
                debug!("Message loop cancelled");
                return Ok(());
            }

            // Lock the command receiver for this iteration
            let mut command_rx = self.command_rx.lock().await;

            // Wait for either a daemon message or a TUI command
            tokio::select! {
                // Read from daemon
                read_result = lines.next_line() => {
                    drop(command_rx); // Release lock before processing
                    match read_result {
                        Ok(None) => {
                            // EOF - connection closed
                            info!("Daemon closed connection");
                            return Ok(());
                        }
                        Ok(Some(line)) => {
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
                            // A timed-out write may leave half a line on the
                            // socket, so end the connection and reconnect.
                            self.send_message(writer, &discover_msg).await?;
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
        // Branch on the env var so the assertion checks a concrete value
        // either way, instead of tautologically mirroring the impl. The
        // env-var override is also exercised end-to-end by the e2e
        // harness.
        let config = DaemonConfig::default();

        let expected_socket = match std::env::var("ATM_SOCKET") {
            Ok(s) if !s.is_empty() => PathBuf::from(s),
            _ => PathBuf::from(DEFAULT_SOCKET_PATH),
        };
        assert_eq!(config.socket_path, expected_socket);
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

        assert_eq!(client.config.socket_path, resolve_socket_path());
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

    // ------------------------------------------------------------------------
    // Deadline Tests
    // ------------------------------------------------------------------------

    /// A stalled daemon still completes connects through the kernel
    /// backlog. The client must give up on the handshake and reconnect
    /// rather than wait on the first stream forever. Time is paused, so
    /// the deadline elapses as soon as the client blocks on the read.
    #[tokio::test(start_paused = true)]
    async fn test_handshake_deadline_reconnects_to_a_daemon_that_never_answers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("stalled.sock");
        let listener = tokio::net::UnixListener::bind(&socket_path).expect("bind");

        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let cancel_token = CancellationToken::new();
        let config = DaemonConfig {
            socket_path,
            ..Default::default()
        };
        let client = DaemonClient::new(config, event_tx, cmd_rx, cancel_token.clone());
        let started = tokio::time::Instant::now();
        let run = tokio::spawn(async move { client.run().await });

        // Accept and hold the connection, never answering `Connect`.
        let (_held, _) = listener.accept().await.expect("accept");
        let event = tokio::time::timeout(Duration::from_secs(60), event_rx.recv()).await;
        assert!(
            matches!(event, Ok(Some(Event::DaemonDisconnected(None)))),
            "client still waiting on a silent daemon"
        );
        assert!(started.elapsed() >= HANDSHAKE_TIMEOUT);

        let reconnect = tokio::time::timeout(Duration::from_secs(60), listener.accept()).await;
        assert!(matches!(reconnect, Ok(Ok(_))), "client did not reconnect");

        cancel_token.cancel();
        run.await.expect("client task");
    }

    /// A daemon that rejects every handshake (an old atmd after an
    /// upgrade), or that crashes right after accepting, connects at once.
    /// The client must keep backing off across those short connections,
    /// and start over only after a connection that stays up. The backoff
    /// of failed connects before the daemon appeared doesn't carry over.
    /// Real time: the paused clock jumps to the next timer while socket
    /// reads are in flight, which would blur the gaps measured here.
    #[tokio::test]
    async fn test_reconnect_backoff_carries_across_short_connections() {
        async fn accept(listener: &tokio::net::UnixListener) -> UnixStream {
            let accepted = tokio::time::timeout(Duration::from_secs(10), listener.accept()).await;
            accepted
                .expect("client did not reconnect")
                .expect("accept")
                .0
        }
        let reply = |msg: DaemonMessage| {
            format!("{}\n", serde_json::to_string(&msg).expect("serialize")).into_bytes()
        };
        let rejected = reply(DaemonMessage::rejected("incompatible protocol"));

        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("rejecting.sock");
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let cancel_token = CancellationToken::new();
        let config = DaemonConfig {
            socket_path: socket_path.clone(),
            retry_initial_delay: Duration::from_millis(50),
            retry_max_delay: Duration::from_millis(800),
            ..Default::default()
        };
        let client = DaemonClient::new(config, event_tx, cmd_rx, cancel_token.clone());
        let run = tokio::spawn(async move { client.run().await });

        // No daemon yet: failed connects grow their backoff to 800ms.
        sleep(Duration::from_millis(1000)).await;
        let listener = tokio::net::UnixListener::bind(&socket_path).expect("bind");

        let mut accepted_at = Vec::new();
        for _ in 0..4 {
            let mut stream = accept(&listener).await;
            accepted_at.push(Instant::now());
            stream.write_all(&rejected).await.expect("reject");
        }
        // 50ms, 100ms, then 200ms apart, rather than redialled at once,
        // and not 800ms apart either.
        let first = accepted_at[1] - accepted_at[0];
        assert!(
            first < Duration::from_millis(400),
            "first redial after {first:?}"
        );
        let waited = accepted_at[3] - accepted_at[0];
        assert!(
            waited >= Duration::from_millis(350),
            "redialled after {waited:?}"
        );
        // The TUI learns why, so it can show it.
        let event = event_rx.recv().await;
        assert!(
            matches!(&event, Some(Event::DaemonDisconnected(Some(r))) if r.contains("incompatible protocol")),
            "rejection reason lost: {event:?}"
        );

        // A connection that outlives retry_max_delay resets the backoff...
        let mut stream = accept(&listener).await;
        let connected = reply(DaemonMessage::connected("client-1".to_string()));
        stream.write_all(&connected).await.expect("connect");
        sleep(Duration::from_millis(900)).await;
        drop(stream);

        // ...so the next rejection waits 50ms again, not 800ms.
        let mut stream = accept(&listener).await;
        let rejected_at = Instant::now();
        stream.write_all(&rejected).await.expect("reject");
        accept(&listener).await;
        let waited = rejected_at.elapsed();
        assert!(
            waited < Duration::from_millis(400),
            "backoff not reset: {waited:?}"
        );

        cancel_token.cancel();
        run.await.expect("client task");
    }

    /// A daemon at its subscriber cap accepts the handshake but answers
    /// `Subscribe` with an Error. The client would get no updates, so it
    /// must disconnect with that reason for the TUI to show, rather than
    /// sit on a stale list.
    #[tokio::test]
    async fn test_subscribe_refusal_reaches_the_disconnected_state() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("full.sock");
        let listener = tokio::net::UnixListener::bind(&socket_path).expect("bind");

        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let cancel_token = CancellationToken::new();
        let config = DaemonConfig {
            socket_path,
            ..Default::default()
        };
        let client = DaemonClient::new(config, event_tx, cmd_rx, cancel_token.clone());
        let run = tokio::spawn(async move { client.run().await });

        let (stream, _) = listener.accept().await.expect("accept");
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        reader.read_line(&mut line).await.expect("read connect");
        let connected = DaemonMessage::connected("client-1".to_string());
        let connected = format!("{}\n", serde_json::to_string(&connected).expect("json"));
        writer.write_all(connected.as_bytes()).await.expect("write");
        line.clear();
        reader.read_line(&mut line).await.expect("read subscribe");
        assert!(
            line.contains("subscribe"),
            "expected Subscribe, got {line:?}"
        );
        // The daemon's own refusal (atmd server/connection.rs)
        let refused = DaemonMessage::error("Too many subscribers (max: 64)");
        let refused = format!("{}\n", serde_json::to_string(&refused).expect("json"));
        writer.write_all(refused.as_bytes()).await.expect("write");

        let event = timeout(Duration::from_secs(5), event_rx.recv()).await;
        let Ok(Some(Event::DaemonDisconnected(reason))) = event else {
            panic!("subscribe refusal didn't disconnect: {event:?}");
        };
        // As the TUI event loop does
        let mut app = crate::app::App::new();
        app.mark_disconnected(reason);
        assert!(
            matches!(&app.state, crate::app::AppState::Disconnected { reason: Some(r), .. }
                if r.contains("Too many subscribers (max: 64)")),
            "reason lost: {:?}",
            app.state
        );

        cancel_token.cancel();
        run.await.expect("client task");
    }

    // ------------------------------------------------------------------------
    // Respawn Tests
    // ------------------------------------------------------------------------

    /// Runs a client on `socket_path` whose respawn step is `respawn`, so
    /// no test starts a real daemon.
    fn spawn_client_with_respawn(
        socket_path: PathBuf,
        respawn: impl Fn() -> std::result::Result<(), String> + Send + Sync + 'static,
    ) -> (CancellationToken, tokio::task::JoinHandle<()>) {
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let cancel_token = CancellationToken::new();
        let config = DaemonConfig {
            socket_path,
            retry_initial_delay: Duration::from_millis(10),
            ..Default::default()
        };
        let mut client = DaemonClient::new(config, event_tx, cmd_rx, cancel_token.clone());
        client.respawn = Some(Arc::new(respawn));
        let run = tokio::spawn(async move { client.run().await });
        (cancel_token, run)
    }

    /// Stands in for a daemon start: binds `socket_path`, replacing a
    /// stale socket file as atmd does, and hands the listener to the test.
    fn bind_daemon(
        socket_path: &std::path::Path,
        started: &mpsc::UnboundedSender<std::os::unix::net::UnixListener>,
    ) -> std::result::Result<(), String> {
        let _ = std::fs::remove_file(socket_path);
        let listener =
            std::os::unix::net::UnixListener::bind(socket_path).map_err(|e| e.to_string())?;
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        started.send(listener).map_err(|e| e.to_string())
    }

    /// `ensure_daemon_running` only manages the default socket, so a client
    /// on another socket must never run it, nor start an unrelated daemon.
    #[test]
    fn test_only_a_client_on_the_default_socket_respawns_the_daemon() {
        let dir = tempfile::tempdir().expect("tempdir");
        let client = |socket_path| {
            let (event_tx, _event_rx) = mpsc::unbounded_channel();
            let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
            let config = DaemonConfig {
                socket_path,
                ..Default::default()
            };
            DaemonClient::new(config, event_tx, cmd_rx, CancellationToken::new())
        };
        assert!(client(resolve_socket_path()).respawn.is_some());
        assert!(client(dir.path().join("other.sock")).respawn.is_none());
    }

    /// `atmd stop` removes the socket. The client starts the daemon, and
    /// a failed start is retried rather than ending the client.
    #[tokio::test]
    async fn test_reconnect_respawns_a_missing_daemon_and_retries_a_failed_start() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("atmd.sock");
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (started_tx, mut started_rx) = mpsc::unbounded_channel();

        let (path, count) = (socket_path.clone(), Arc::clone(&calls));
        let (cancel_token, run) = spawn_client_with_respawn(socket_path, move || {
            if count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                return Err("first start fails".to_string());
            }
            bind_daemon(&path, &started_tx)
        });

        let listener = tokio::time::timeout(Duration::from_secs(10), started_rx.recv())
            .await
            .expect("client never respawned the daemon")
            .expect("respawn channel closed");
        let listener = tokio::net::UnixListener::from_std(listener).expect("listener");
        let accepted = tokio::time::timeout(Duration::from_secs(10), listener.accept()).await;
        assert!(matches!(accepted, Ok(Ok(_))), "client did not reconnect");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);

        // Cancel, then close the stream so the handshake read ends now.
        cancel_token.cancel();
        drop(accepted);
        run.await.expect("client task");
    }

    /// `kill -9` leaves the socket file behind, so the connect is refused
    /// instead of finding no socket.
    #[tokio::test]
    async fn test_reconnect_respawns_a_daemon_behind_a_stale_socket() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("atmd.sock");
        drop(std::os::unix::net::UnixListener::bind(&socket_path).expect("bind"));
        assert!(socket_path.exists(), "no stale socket file");
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (started_tx, mut started_rx) = mpsc::unbounded_channel();

        let (path, count) = (socket_path.clone(), Arc::clone(&calls));
        let (cancel_token, run) = spawn_client_with_respawn(socket_path, move || {
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            bind_daemon(&path, &started_tx)
        });

        let listener = tokio::time::timeout(Duration::from_secs(10), started_rx.recv())
            .await
            .expect("client never respawned the daemon")
            .expect("respawn channel closed");
        let listener = tokio::net::UnixListener::from_std(listener).expect("listener");
        let accepted = tokio::time::timeout(Duration::from_secs(10), listener.accept()).await;
        assert!(matches!(accepted, Ok(Ok(_))), "client did not reconnect");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

        // Cancel, then close the stream so the handshake read ends now.
        cancel_token.cancel();
        drop(accepted);
        run.await.expect("client task");
    }

    /// A TUI command that arrives while a daemon line is half-read must not
    /// lose that line. Subscribed TUIs no longer reconnect every few
    /// minutes, so a lost update would stay lost.
    #[tokio::test(start_paused = true)]
    async fn test_message_loop_keeps_a_partial_line_across_a_command() {
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let client = DaemonClient::new(
            DaemonConfig::default(),
            event_tx,
            cmd_rx,
            CancellationToken::new(),
        );
        let (ours, daemon) = tokio::io::duplex(64 * 1024);
        let (reader, mut writer) = tokio::io::split(ours);
        let (daemon_reader, mut daemon_writer) = tokio::io::split(daemon);
        tokio::spawn(async move {
            client
                .message_loop(&mut BufReader::new(reader), &mut writer)
                .await
        });

        let json = serde_json::to_string(&DaemonMessage::session_removed(SessionId::new("s1")))
            .expect("serialize");
        let (head, tail) = json.split_at(json.len() / 2);
        daemon_writer
            .write_all(head.as_bytes())
            .await
            .expect("write head");
        // Let the client read the head and block waiting for the rest.
        tokio::time::sleep(Duration::from_millis(10)).await;

        // The command wins the select while the line is incomplete.
        cmd_tx.send(ClientCommand::Discover).expect("send command");
        let mut sent = String::new();
        BufReader::new(daemon_reader)
            .read_line(&mut sent)
            .await
            .expect("read discover");
        assert!(sent.contains("discover"), "unexpected client line: {sent}");

        daemon_writer
            .write_all(format!("{tail}\n").as_bytes())
            .await
            .expect("write tail");
        let event = tokio::time::timeout(Duration::from_secs(60), event_rx.recv()).await;
        assert!(
            matches!(&event, Ok(Some(Event::SessionRemoved(id))) if id == "s1"),
            "half-read line was lost: {event:?}"
        );
    }

    /// A daemon that stops reading must not block the client's writes
    /// forever. A 1-byte duplex stands in for a full socket buffer.
    #[tokio::test(start_paused = true)]
    async fn test_send_message_times_out_when_daemon_stops_reading() {
        let (client, _event_rx) = create_test_client();
        let (mut writer, _daemon) = tokio::io::duplex(1);

        let started = tokio::time::Instant::now();
        let msg = ClientMessage::discover();
        let send = client.send_message(&mut writer, &msg);
        let result = tokio::time::timeout(Duration::from_secs(60), send).await;

        assert!(
            matches!(&result, Ok(Err(TuiError::Io(e))) if e.kind() == std::io::ErrorKind::TimedOut),
            "expected a write timeout, got {result:?}"
        );
        assert!(started.elapsed() >= WRITE_TIMEOUT);
    }
}
